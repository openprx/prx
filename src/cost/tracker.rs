use super::types::{BudgetCheck, CostRecord, CostSettlement, CostSummary, ModelStats, TokenUsage, UsagePeriod};
use crate::config::schema::CostConfig;
use anyhow::{Context, Result, anyhow};
use chrono::{Datelike, NaiveDate, Utc};
use parking_lot::{Mutex, MutexGuard};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

const COST_LEDGER_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const COST_LEDGER_LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(20);

struct CostLedgerLockGuard {
    _file: File,
}

fn acquire_cost_ledger_lock(path: &Path) -> Result<CostLedgerLockGuard> {
    let lock_path = path.with_extension("jsonl.lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("Failed to open cost ledger lock {}", lock_path.display()))?;
    let started = std::time::Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(CostLedgerLockGuard { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                if started.elapsed() > COST_LEDGER_LOCK_TIMEOUT {
                    anyhow::bail!("Timed out waiting for cost ledger lock {}", lock_path.display());
                }
                std::thread::sleep(COST_LEDGER_LOCK_POLL);
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error)
                    .with_context(|| format!("Failed to acquire cost ledger lock {}", lock_path.display()));
            }
        }
    }
}

/// Cost tracker for API usage monitoring and budget enforcement.
pub struct CostTracker {
    config: Arc<Mutex<CostConfig>>,
    storage: Arc<Mutex<CostStorage>>,
    session_id: String,
    session_costs: Arc<Mutex<Vec<CostRecord>>>,
}

impl CostTracker {
    /// Return the sole process-level cost authority for a canonical workspace.
    pub fn for_workspace(config: CostConfig, workspace_dir: &Path) -> Result<Arc<Self>> {
        static TRACKERS: OnceLock<Mutex<HashMap<PathBuf, Arc<CostTracker>>>> = OnceLock::new();
        let workspace_dir = workspace_dir
            .canonicalize()
            .unwrap_or_else(|_| workspace_dir.to_path_buf());
        let trackers = TRACKERS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut trackers = trackers.lock();
        if let Some(tracker) = trackers.get(&workspace_dir) {
            *tracker.config.lock() = config;
            return Ok(Arc::clone(tracker));
        }
        let tracker = Arc::new(Self::new(config, &workspace_dir)?);
        trackers.insert(workspace_dir, Arc::clone(&tracker));
        Ok(tracker)
    }

    /// Create a new cost tracker.
    pub fn new(config: CostConfig, workspace_dir: &Path) -> Result<Self> {
        let storage_path = resolve_storage_path(workspace_dir)?;

        let storage = CostStorage::new(&storage_path)
            .with_context(|| format!("Failed to open cost storage at {}", storage_path.display()))?;

        Ok(Self {
            config: Arc::new(Mutex::new(config)),
            storage: Arc::new(Mutex::new(storage)),
            session_id: uuid::Uuid::new_v4().to_string(),
            session_costs: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Get the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    fn lock_storage(&self) -> MutexGuard<'_, CostStorage> {
        self.storage.lock()
    }

    fn lock_session_costs(&self) -> MutexGuard<'_, Vec<CostRecord>> {
        self.session_costs.lock()
    }

    /// Check if a request is within budget.
    pub fn check_budget(&self, estimated_cost_usd: f64) -> Result<BudgetCheck> {
        let config = self.config.lock().clone();
        if !config.enabled {
            return Ok(BudgetCheck::Allowed);
        }

        if !estimated_cost_usd.is_finite() || estimated_cost_usd < 0.0 {
            return Err(anyhow!("Estimated cost must be a finite, non-negative value"));
        }

        let mut storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        storage.refresh_from_disk()?;
        let (daily_cost, monthly_cost) = storage.get_aggregated_costs()?;

        Ok(budget_check(&config, daily_cost, monthly_cost, estimated_cost_usd))
    }

    /// Record a usage event.
    pub fn record_usage(&self, usage: TokenUsage) -> Result<()> {
        if !self.config.lock().enabled {
            return Ok(());
        }

        if !usage.cost_usd.is_finite() || usage.cost_usd < 0.0 {
            return Err(anyhow!("Token usage cost must be a finite, non-negative value"));
        }

        let record = CostRecord::new(&self.session_id, usage);

        // Persist first for durability guarantees.
        {
            let mut storage = self.lock_storage();
            let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
            storage.refresh_from_disk()?;
            storage.add_record(record.clone())?;
        }

        // Then update in-memory session snapshot.
        let mut session_costs = self.lock_session_costs();
        session_costs.push(record);

        Ok(())
    }

    /// Idempotently project a canonical terminal usage settlement into the
    /// persistent cost ledger. Unknown price remains explicit, never zero.
    pub fn settle_metered(
        &self,
        record: &crate::llm::route_decision::MeteredTokenUsageRecord,
    ) -> Result<CostSettlement> {
        let config = self.config.lock().clone();
        if !config.enabled {
            return Ok(CostSettlement::Disabled);
        }
        let settlement_id = record
            .settlement_id
            .as_deref()
            .ok_or_else(|| anyhow!("Metered usage settlement is missing settlement_id"))?;
        let Some(cost_usd) = record.cost_usd else {
            return Ok(CostSettlement::UnknownPricing);
        };
        if !cost_usd.is_finite() || cost_usd < 0.0 {
            return Err(anyhow!("Metered usage cost must be a finite, non-negative value"));
        }

        let mut storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        storage.refresh_from_disk()?;
        if storage.has_settlement(settlement_id) {
            return Ok(CostSettlement::Replayed);
        }
        let (daily_cost, monthly_cost) = storage.get_aggregated_costs()?;
        let budget = budget_check(&config, daily_cost, monthly_cost, cost_usd);
        let usage = TokenUsage {
            model: format!("{}/{}", record.provider, record.model),
            input_tokens: record.prompt_tokens,
            output_tokens: record.completion_tokens,
            total_tokens: record.total_tokens,
            cost_usd,
            timestamp: Utc::now(),
        };
        let cost_record = CostRecord::from_settlement(&self.session_id, settlement_id, usage);
        storage.add_record(cost_record.clone())?;
        drop(storage);
        self.lock_session_costs().push(cost_record);
        Ok(CostSettlement::Recorded { budget })
    }

    /// Get the current cost summary.
    pub fn get_summary(&self) -> Result<CostSummary> {
        let (daily_cost, monthly_cost) = {
            let mut storage = self.lock_storage();
            let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
            storage.refresh_from_disk()?;
            storage.get_aggregated_costs()?
        };

        let session_costs = self.lock_session_costs();
        let session_cost: f64 = session_costs.iter().map(|record| record.usage.cost_usd).sum();
        let total_tokens: u64 = session_costs.iter().map(|record| record.usage.total_tokens).sum();
        let request_count = session_costs.len();
        let by_model = build_session_model_stats(&session_costs);

        Ok(CostSummary {
            session_cost_usd: session_cost,
            daily_cost_usd: daily_cost,
            monthly_cost_usd: monthly_cost,
            total_tokens,
            request_count,
            by_model,
        })
    }

    /// Get the daily cost for a specific date.
    pub fn get_daily_cost(&self, date: NaiveDate) -> Result<f64> {
        let mut storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        storage.refresh_from_disk()?;
        storage.get_cost_for_date(date)
    }

    /// Get the monthly cost for a specific month.
    pub fn get_monthly_cost(&self, year: i32, month: u32) -> Result<f64> {
        let mut storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        storage.refresh_from_disk()?;
        storage.get_cost_for_month(year, month)
    }
}

fn budget_check(config: &CostConfig, daily_cost: f64, monthly_cost: f64, estimated_cost_usd: f64) -> BudgetCheck {
    let projected_daily = daily_cost + estimated_cost_usd;
    if projected_daily > config.daily_limit_usd {
        return BudgetCheck::Exceeded {
            current_usd: daily_cost,
            limit_usd: config.daily_limit_usd,
            period: UsagePeriod::Day,
        };
    }
    let projected_monthly = monthly_cost + estimated_cost_usd;
    if projected_monthly > config.monthly_limit_usd {
        return BudgetCheck::Exceeded {
            current_usd: monthly_cost,
            limit_usd: config.monthly_limit_usd,
            period: UsagePeriod::Month,
        };
    }
    let warn_threshold = f64::from(config.warn_at_percent.min(100)) / 100.0;
    if projected_daily >= config.daily_limit_usd * warn_threshold {
        return BudgetCheck::Warning {
            current_usd: daily_cost,
            limit_usd: config.daily_limit_usd,
            period: UsagePeriod::Day,
        };
    }
    if projected_monthly >= config.monthly_limit_usd * warn_threshold {
        return BudgetCheck::Warning {
            current_usd: monthly_cost,
            limit_usd: config.monthly_limit_usd,
            period: UsagePeriod::Month,
        };
    }
    BudgetCheck::Allowed
}

fn resolve_storage_path(workspace_dir: &Path) -> Result<PathBuf> {
    let storage_path = workspace_dir.join("state").join("costs.jsonl");
    let legacy_path = workspace_dir.join(".openprx").join("costs.db");

    if !storage_path.exists() && legacy_path.exists() {
        if let Some(parent) = storage_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }

        if let Err(error) = fs::rename(&legacy_path, &storage_path) {
            tracing::warn!(
                "Failed to move legacy cost storage from {} to {}: {error}; falling back to copy",
                legacy_path.display(),
                storage_path.display()
            );
            fs::copy(&legacy_path, &storage_path).with_context(|| {
                format!(
                    "Failed to copy legacy cost storage from {} to {}",
                    legacy_path.display(),
                    storage_path.display()
                )
            })?;
        }
    }

    Ok(storage_path)
}

fn build_session_model_stats(session_costs: &[CostRecord]) -> HashMap<String, ModelStats> {
    let mut by_model: HashMap<String, ModelStats> = HashMap::new();

    for record in session_costs {
        let entry = by_model
            .entry(record.usage.model.clone())
            .or_insert_with(|| ModelStats {
                model: record.usage.model.clone(),
                cost_usd: 0.0,
                total_tokens: 0,
                request_count: 0,
            });

        entry.cost_usd += record.usage.cost_usd;
        entry.total_tokens += record.usage.total_tokens;
        entry.request_count += 1;
    }

    by_model
}

/// Persistent storage for cost records.
struct CostStorage {
    path: PathBuf,
    daily_cost_usd: f64,
    monthly_cost_usd: f64,
    cached_day: NaiveDate,
    cached_year: i32,
    cached_month: u32,
    settlement_ids: HashSet<String>,
}

impl CostStorage {
    /// Create or open cost storage.
    fn new(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }

        let now = Utc::now();
        let mut storage = Self {
            path: path.to_path_buf(),
            daily_cost_usd: 0.0,
            monthly_cost_usd: 0.0,
            cached_day: now.date_naive(),
            cached_year: now.year(),
            cached_month: now.month(),
            settlement_ids: HashSet::new(),
        };

        storage.rebuild_aggregates(storage.cached_day, storage.cached_year, storage.cached_month)?;
        storage.load_settlement_ids()?;

        Ok(storage)
    }

    fn refresh_from_disk(&mut self) -> Result<()> {
        let now = Utc::now();
        self.rebuild_aggregates(now.date_naive(), now.year(), now.month())?;
        self.cached_day = now.date_naive();
        self.cached_year = now.year();
        self.cached_month = now.month();
        self.load_settlement_ids()
    }

    fn load_settlement_ids(&mut self) -> Result<()> {
        let mut ids = HashSet::new();
        self.for_each_record(|record| {
            if let Some(settlement_id) = record.settlement_id {
                ids.insert(settlement_id);
            }
        })?;
        self.settlement_ids = ids;
        Ok(())
    }

    fn has_settlement(&self, settlement_id: &str) -> bool {
        self.settlement_ids.contains(settlement_id)
    }

    fn for_each_record<F>(&self, mut on_record: F) -> Result<()>
    where
        F: FnMut(CostRecord),
    {
        if !self.path.exists() {
            return Ok(());
        }

        let file = File::open(&self.path)
            .with_context(|| format!("Failed to read cost storage from {}", self.path.display()))?;
        let reader = BufReader::new(file);

        for (line_number, line) in reader.lines().enumerate() {
            let raw_line = line.with_context(|| {
                format!(
                    "Failed to read line {} from cost storage {}",
                    line_number + 1,
                    self.path.display()
                )
            })?;

            let trimmed = raw_line.trim();
            if trimmed.is_empty() {
                continue;
            }

            match serde_json::from_str::<CostRecord>(trimmed) {
                Ok(record) => on_record(record),
                Err(error) => {
                    tracing::warn!(
                        "Skipping malformed cost record at {}:{}: {error}",
                        self.path.display(),
                        line_number + 1
                    );
                }
            }
        }

        Ok(())
    }

    fn rebuild_aggregates(&mut self, day: NaiveDate, year: i32, month: u32) -> Result<()> {
        let mut daily_cost = 0.0;
        let mut monthly_cost = 0.0;

        self.for_each_record(|record| {
            let timestamp = record.usage.timestamp.naive_utc();

            if timestamp.date() == day {
                daily_cost += record.usage.cost_usd;
            }

            if timestamp.year() == year && timestamp.month() == month {
                monthly_cost += record.usage.cost_usd;
            }
        })?;

        self.daily_cost_usd = daily_cost;
        self.monthly_cost_usd = monthly_cost;
        self.cached_day = day;
        self.cached_year = year;
        self.cached_month = month;

        Ok(())
    }

    fn ensure_period_cache_current(&mut self) -> Result<()> {
        let now = Utc::now();
        let day = now.date_naive();
        let year = now.year();
        let month = now.month();

        if day != self.cached_day || year != self.cached_year || month != self.cached_month {
            self.rebuild_aggregates(day, year, month)?;
        }

        Ok(())
    }

    /// Add a new record.
    fn add_record(&mut self, record: CostRecord) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("Failed to open cost storage at {}", self.path.display()))?;

        writeln!(file, "{}", serde_json::to_string(&record)?)
            .with_context(|| format!("Failed to write cost record to {}", self.path.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to sync cost storage at {}", self.path.display()))?;

        self.ensure_period_cache_current()?;
        if let Some(settlement_id) = record.settlement_id.as_ref() {
            self.settlement_ids.insert(settlement_id.clone());
        }

        let timestamp = record.usage.timestamp.naive_utc();
        if timestamp.date() == self.cached_day {
            self.daily_cost_usd += record.usage.cost_usd;
        }
        if timestamp.year() == self.cached_year && timestamp.month() == self.cached_month {
            self.monthly_cost_usd += record.usage.cost_usd;
        }

        Ok(())
    }

    /// Get aggregated costs for current day and month.
    fn get_aggregated_costs(&mut self) -> Result<(f64, f64)> {
        self.ensure_period_cache_current()?;
        Ok((self.daily_cost_usd, self.monthly_cost_usd))
    }

    /// Get cost for a specific date.
    fn get_cost_for_date(&self, date: NaiveDate) -> Result<f64> {
        let mut cost = 0.0;

        self.for_each_record(|record| {
            if record.usage.timestamp.naive_utc().date() == date {
                cost += record.usage.cost_usd;
            }
        })?;

        Ok(cost)
    }

    /// Get cost for a specific month.
    fn get_cost_for_month(&self, year: i32, month: u32) -> Result<f64> {
        let mut cost = 0.0;

        self.for_each_record(|record| {
            let timestamp = record.usage.timestamp.naive_utc();
            if timestamp.year() == year && timestamp.month() == month {
                cost += record.usage.cost_usd;
            }
        })?;

        Ok(cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn enabled_config() -> CostConfig {
        CostConfig {
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn cost_tracker_initialization() {
        let tmp = TempDir::new().unwrap();
        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        assert!(!tracker.session_id().is_empty());
    }

    #[test]
    fn budget_check_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            enabled: false,
            ..Default::default()
        };

        let tracker = CostTracker::new(config, tmp.path()).unwrap();
        let check = tracker.check_budget(1000.0).unwrap();
        assert!(matches!(check, BudgetCheck::Allowed));
    }

    #[test]
    fn record_usage_and_get_summary() {
        let tmp = TempDir::new().unwrap();
        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();

        let usage = TokenUsage::new("test/model", 1000, 500, 1.0, 2.0);
        tracker.record_usage(usage).unwrap();

        let summary = tracker.get_summary().unwrap();
        assert_eq!(summary.request_count, 1);
        assert!(summary.session_cost_usd > 0.0);
        assert_eq!(summary.by_model.len(), 1);
    }

    #[test]
    fn budget_exceeded_daily_limit() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            enabled: true,
            daily_limit_usd: 0.01, // Very low limit
            ..Default::default()
        };

        let tracker = CostTracker::new(config, tmp.path()).unwrap();

        // Record a usage that exceeds the limit
        let usage = TokenUsage::new("test/model", 10000, 5000, 1.0, 2.0); // ~0.02 USD
        tracker.record_usage(usage).unwrap();

        let check = tracker.check_budget(0.01).unwrap();
        assert!(matches!(check, BudgetCheck::Exceeded { .. }));
    }

    #[test]
    fn summary_by_model_is_session_scoped() {
        let tmp = TempDir::new().unwrap();
        let storage_path = resolve_storage_path(tmp.path()).unwrap();
        if let Some(parent) = storage_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        let old_record = CostRecord::new("old-session", TokenUsage::new("legacy/model", 500, 500, 1.0, 1.0));
        let mut file = OpenOptions::new().create(true).append(true).open(storage_path).unwrap();
        writeln!(file, "{}", serde_json::to_string(&old_record).unwrap()).unwrap();
        file.sync_all().unwrap();

        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        tracker
            .record_usage(TokenUsage::new("session/model", 1000, 1000, 1.0, 1.0))
            .unwrap();

        let summary = tracker.get_summary().unwrap();
        assert_eq!(summary.by_model.len(), 1);
        assert!(summary.by_model.contains_key("session/model"));
        assert!(!summary.by_model.contains_key("legacy/model"));
    }

    #[test]
    fn malformed_lines_are_ignored_while_loading() {
        let tmp = TempDir::new().unwrap();
        let storage_path = resolve_storage_path(tmp.path()).unwrap();
        if let Some(parent) = storage_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        let valid_usage = TokenUsage::new("test/model", 1000, 0, 1.0, 1.0);
        let valid_record = CostRecord::new("session-a", valid_usage.clone());

        let mut file = OpenOptions::new().create(true).append(true).open(storage_path).unwrap();
        writeln!(file, "{}", serde_json::to_string(&valid_record).unwrap()).unwrap();
        writeln!(file, "not-a-json-line").unwrap();
        writeln!(file).unwrap();
        file.sync_all().unwrap();

        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        let today_cost = tracker.get_daily_cost(Utc::now().date_naive()).unwrap();
        assert!((today_cost - valid_usage.cost_usd).abs() < f64::EPSILON);
    }

    #[test]
    fn invalid_budget_estimate_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();

        let err = tracker.check_budget(f64::NAN).unwrap_err();
        assert!(
            err.to_string()
                .contains("Estimated cost must be a finite, non-negative value")
        );
    }

    fn metered_record(
        settlement_id: &str,
        cost_usd: Option<f64>,
    ) -> crate::llm::route_decision::MeteredTokenUsageRecord {
        crate::llm::route_decision::MeteredTokenUsageRecord {
            settlement_id: Some(settlement_id.to_string()),
            provider: "provider-a".to_string(),
            model: "model-a".to_string(),
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            source: crate::llm::route_decision::TokenUsageSource::Reported,
            cost_usd,
        }
    }

    #[test]
    fn canonical_settlement_is_process_owned_idempotent_and_budgeted() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            enabled: true,
            daily_limit_usd: 0.001,
            monthly_limit_usd: 1.0,
            ..Default::default()
        };
        let first = CostTracker::for_workspace(config.clone(), tmp.path()).unwrap();
        let second = CostTracker::for_workspace(config, tmp.path()).unwrap();
        assert!(Arc::ptr_eq(&first, &second));

        let record = metered_record("turn-1", Some(0.002));
        let settled = first.settle_metered(&record).unwrap();
        assert!(matches!(
            settled,
            CostSettlement::Recorded {
                budget: BudgetCheck::Exceeded { .. }
            }
        ));
        assert_eq!(second.settle_metered(&record).unwrap(), CostSettlement::Replayed);
        assert_eq!(first.get_summary().unwrap().request_count, 1);
    }

    #[test]
    fn separate_tracker_instances_serialize_the_same_settlement() {
        let tmp = TempDir::new().unwrap();
        let first = Arc::new(CostTracker::new(enabled_config(), tmp.path()).unwrap());
        let second = Arc::new(CostTracker::new(enabled_config(), tmp.path()).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let first_result = {
            let tracker = Arc::clone(&first);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                tracker.settle_metered(&metered_record("shared-settlement", Some(0.002)))
            })
        };
        let second_result = {
            let tracker = Arc::clone(&second);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                tracker.settle_metered(&metered_record("shared-settlement", Some(0.002)))
            })
        };

        let results = [
            first_result.join().unwrap().unwrap(),
            second_result.join().unwrap().unwrap(),
        ];
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, CostSettlement::Recorded { .. }))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, CostSettlement::Replayed))
                .count(),
            1
        );
        let persisted = std::fs::read_to_string(tmp.path().join("state").join("costs.jsonl")).unwrap();
        assert_eq!(persisted.lines().filter(|line| !line.trim().is_empty()).count(), 1);
    }

    #[test]
    fn separate_tracker_instances_refresh_budget_and_summary_from_disk() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            enabled: true,
            daily_limit_usd: 1.0,
            monthly_limit_usd: 10.0,
            ..Default::default()
        };
        let writer = CostTracker::new(config.clone(), tmp.path()).unwrap();
        let reader = CostTracker::new(config, tmp.path()).unwrap();

        assert!(matches!(
            writer
                .settle_metered(&metered_record("external-settlement", Some(0.9)))
                .unwrap(),
            CostSettlement::Recorded { .. }
        ));
        assert!(matches!(
            reader.check_budget(0.2).unwrap(),
            BudgetCheck::Exceeded {
                period: UsagePeriod::Day,
                ..
            }
        ));
        let summary = reader.get_summary().unwrap();
        assert!((summary.daily_cost_usd - 0.9).abs() < f64::EPSILON);
        assert!((summary.monthly_cost_usd - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn unknown_price_is_not_recorded_as_zero_cost() {
        let tmp = TempDir::new().unwrap();
        let tracker = CostTracker::for_workspace(enabled_config(), tmp.path()).unwrap();
        let settlement = tracker.settle_metered(&metered_record("turn-unknown", None)).unwrap();
        assert_eq!(settlement, CostSettlement::UnknownPricing);
        assert_eq!(tracker.get_summary().unwrap().request_count, 0);
    }
}

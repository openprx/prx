#[derive(Debug, thiserror::Error)]
pub(crate) enum LedgerBlocked {
    #[error("cost ledger ends with an incomplete row")]
    TornTail,
    #[error("Metered usage settlement conflicts with the cost ledger")]
    Conflict,
}

#[cfg(test)]
pub(crate) use settlement::TestCrash;

use super::types::{BudgetCheck, CostRecord, CostSettlement, CostSummary, ModelStats, TokenUsage, UsagePeriod};
use crate::config::schema::CostConfig;
use anyhow::{Context, Result, anyhow};
use chrono::{Datelike, NaiveDate, Utc};
use parking_lot::{Mutex, MutexGuard};
use std::collections::{HashMap, HashSet};
#[cfg(any(test, not(unix)))]
use std::fs;
use std::fs::File;
#[cfg(any(test, not(unix)))]
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

mod report;
mod settlement;
pub use report::{CostTotals, ModelCostTotals, WorkspaceCostReport, workspace_cost_report};
#[cfg(test)]
pub(crate) use settlement::{crash_after_append, crash_before_append, force_io_error, take_recovery_projection_faults};

const COST_LEDGER_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const COST_LEDGER_LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(20);
const MAX_READ_ONLY_BUDGET_BYTES: u64 = 8 * 1024 * 1024;

#[cfg(test)]
fn settlement_call_counts() -> &'static Mutex<HashMap<PathBuf, usize>> {
    static COUNTS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn settle_metered_call_count(workspace: &Path) -> usize {
    settlement_call_counts()
        .lock()
        .get(&workspace.join("state/costs.jsonl"))
        .copied()
        .unwrap_or_default()
}

#[cfg(unix)]
pub(super) fn read_state_ledger_bounded(workspace: &Path, filename: &str) -> Result<Option<String>> {
    use rustix::fs::{FileType, Mode, OFlags};

    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let root = rustix::fs::open(workspace, directory_flags, Mode::empty())
        .context("cost workspace cannot be opened safely")?;
    let state = match rustix::fs::openat(&root, "state", directory_flags, Mode::empty()) {
        Ok(state) => state,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error).context("cost state directory cannot be opened safely"),
    };
    let state_stat = rustix::fs::fstat(&state).context("cost state directory cannot be inspected")?;
    super::state_dir::validate(&workspace.join("state"), &state_stat)?;
    let fd = match rustix::fs::openat(
        &state,
        filename,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error).context("cost ledger cannot be opened safely"),
    };
    let stat = rustix::fs::fstat(&fd).context("cost ledger cannot be inspected")?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink != 1 || stat.st_mode & 0o022 != 0
    {
        return Err(anyhow!("cost ledger is not a private single-link regular file"));
    }
    if stat.st_size < 0 || stat.st_size as u64 > MAX_READ_ONLY_BUDGET_BYTES {
        return Err(anyhow!("cost ledger exceeds the read-only snapshot bound"));
    }
    let mut bytes = Vec::new();
    File::from(fd)
        .take(MAX_READ_ONLY_BUDGET_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("cost ledger snapshot cannot be read")?;
    if bytes.len() as u64 > MAX_READ_ONLY_BUDGET_BYTES {
        return Err(anyhow!("cost ledger changed beyond the read-only snapshot bound"));
    }
    let ledger = String::from_utf8(bytes).context("cost ledger is not UTF-8")?;
    if !ledger.is_empty() && !ledger.ends_with('\n') {
        return Err(LedgerBlocked::TornTail.into());
    }
    Ok(Some(ledger))
}

#[cfg(not(unix))]
pub(super) fn read_state_ledger_bounded(_workspace: &Path, _filename: &str) -> Result<Option<String>> {
    Err(anyhow!("safe read-only cost snapshots require Unix file handles"))
}

#[cfg(unix)]
fn legacy_cost_ledger_present(workspace: &Path) -> Result<bool> {
    use rustix::fs::{Mode, OFlags};

    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let root = rustix::fs::open(workspace, directory_flags, Mode::empty())
        .context("cost workspace cannot be opened safely")?;
    let legacy_dir = match rustix::fs::openat(&root, ".openprx", directory_flags, Mode::empty()) {
        Ok(dir) => dir,
        Err(rustix::io::Errno::NOENT) => return Ok(false),
        Err(error) => return Err(error).context("legacy cost state cannot be opened safely"),
    };
    match rustix::fs::openat(
        &legacy_dir,
        "costs.db",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(_) => Ok(true),
        Err(rustix::io::Errno::NOENT) => Ok(false),
        Err(error) => Err(error).context("legacy cost ledger cannot be inspected safely"),
    }
}

#[cfg(not(unix))]
fn legacy_cost_ledger_present(_workspace: &Path) -> Result<bool> {
    Err(anyhow!("safe legacy cost inspection requires Unix file handles"))
}

/// Remaining daily/monthly spend, in micro-USD, from a bounded, read-only
/// ledger snapshot. The caller must recheck before a priced execution because
/// another process may append after this snapshot. An unsafe, malformed or
/// oversized ledger is an error, never treated as an empty one.
#[cfg(test)]
pub(crate) fn remaining_micro_usd_read_only(config: &CostConfig, workspace_dir: &Path) -> Result<u64> {
    remaining_micro_usd_read_only_inner(config, workspace_dir, None)
}

/// The same bounded ledger snapshot also applies the security policy's daily
/// ceiling to today's already charged amount. A separate read could observe
/// a different ledger version and overstate one of the remaining limits.
pub(crate) fn remaining_micro_usd_read_only_with_security_ceiling(
    config: &CostConfig,
    workspace_dir: &Path,
    max_cost_per_day_cents: u32,
) -> Result<u64> {
    let security_daily_micro = u128::from(max_cost_per_day_cents) * 10_000;
    remaining_micro_usd_read_only_inner(config, workspace_dir, Some(security_daily_micro))
}

fn remaining_micro_usd_read_only_inner(
    config: &CostConfig,
    workspace_dir: &Path,
    security_daily_micro: Option<u128>,
) -> Result<u64> {
    let spend = read_only_spend(config, workspace_dir)?;
    let remaining = spend
        .day_limit
        .saturating_sub(spend.daily)
        .min(spend.month_limit.saturating_sub(spend.monthly))
        .min(security_daily_micro.map_or(u128::MAX, |limit| limit.saturating_sub(spend.daily)));
    // Only reachable without any limit (the production caller always carries
    // the security ceiling): unlimited, not an error.
    Ok(u64::try_from(remaining).unwrap_or(u64::MAX))
}

/// A daily limit that today's charged amount has used up, in micro-USD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExhaustedDailyBudget {
    /// Today's (UTC) settled spend plus active reservations.
    pub(crate) spent_micro_usd: u128,
    /// The exhausted limit: the lower of `[cost] daily_limit_usd` and the
    /// security policy's `max_cost_per_day_cents`.
    pub(crate) limit_micro_usd: u128,
}

/// The daily limit with the lower remaining when today's spend has used it
/// up, read from the same bounded ledger snapshot as the remaining budget;
/// `None` while both daily limits have budget left.
pub(crate) fn exhausted_daily_budget_read_only(
    config: &CostConfig,
    workspace_dir: &Path,
    max_cost_per_day_cents: u32,
) -> Result<Option<ExhaustedDailyBudget>> {
    // No daily limit at all: nothing can be exhausted, so skip the ledger read.
    if config.daily_limit_usd.is_none() && max_cost_per_day_cents == u32::MAX {
        return Ok(None);
    }
    let spend = read_only_spend(config, workspace_dir)?;
    // Both limits charge the same daily figure, so the lower limit is the one
    // with the lower remaining.
    let limit = spend.day_limit.min(u128::from(max_cost_per_day_cents) * 10_000);
    Ok((spend.daily >= limit).then_some(ExhaustedDailyBudget {
        spent_micro_usd: spend.daily,
        limit_micro_usd: limit,
    }))
}

/// Charged amounts and configured limits of one read-only ledger snapshot.
struct ReadOnlySpend {
    daily: u128,
    monthly: u128,
    day_limit: u128,
    month_limit: u128,
}

fn read_only_spend(config: &CostConfig, workspace_dir: &Path) -> Result<ReadOnlySpend> {
    for limit in [config.daily_limit_usd, config.monthly_limit_usd].into_iter().flatten() {
        if !limit.is_finite() || limit < 0.0 {
            return Err(anyhow!("cost limits are not finite nonnegative amounts"));
        }
    }
    let workspace = workspace_dir
        .canonicalize()
        .with_context(|| "cost workspace cannot be resolved")?;
    let ledger = read_state_ledger_bounded(&workspace, "costs.jsonl")?;
    if ledger.is_none() && legacy_cost_ledger_present(&workspace)? {
        return Err(anyhow!(
            "legacy cost ledger needs migration before a read-only budget snapshot"
        ));
    }
    let now = Utc::now();
    let mut daily = 0_u128;
    let mut monthly = 0_u128;
    if let Some(ledger) = ledger {
        for (index, line) in ledger.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let record: CostRecord =
                serde_json::from_str(line).with_context(|| format!("cost ledger row {} is malformed", index + 1))?;
            let cost = record.usage.cost_usd;
            if !cost.is_finite() || cost < 0.0 {
                return Err(anyhow!("cost ledger contains an invalid amount"));
            }
            let micro = ceil_micro_usd(cost)?;
            let date = record.usage.timestamp.date_naive();
            if date == now.date_naive() {
                daily = daily
                    .checked_add(micro)
                    .ok_or_else(|| anyhow!("daily cost ledger aggregate overflows"))?;
            }
            if date.year() == now.year() && date.month() == now.month() {
                monthly = monthly
                    .checked_add(micro)
                    .ok_or_else(|| anyhow!("monthly cost ledger aggregate overflows"))?;
            }
        }
    }
    // Unknown sent liability remains charged against every current period,
    // including after a day/month rollover, until authoritative settlement.
    let pending = super::reservation::Reservations::load(&workspace)?.active_micro_usd()?;
    daily = daily.checked_add(pending).context("daily reserved cost overflows")?;
    monthly = monthly
        .checked_add(pending)
        .context("monthly reserved cost overflows")?;
    Ok(ReadOnlySpend {
        daily,
        monthly,
        day_limit: day_limit_micro_usd(config)?,
        month_limit: month_limit_micro_usd(config)?,
    })
}

/// The daily limit in micro-USD; `u128::MAX` when none is configured.
fn day_limit_micro_usd(config: &CostConfig) -> Result<u128> {
    config.daily_limit_usd.map_or(Ok(u128::MAX), floor_micro_usd)
}

/// The monthly limit in micro-USD; `u128::MAX` when none is configured.
fn month_limit_micro_usd(config: &CostConfig) -> Result<u128> {
    config.monthly_limit_usd.map_or(Ok(u128::MAX), floor_micro_usd)
}

pub(crate) fn ceil_micro_usd(usd: f64) -> Result<u128> {
    exact_micro_usd(usd, true)
}

fn floor_micro_usd(usd: f64) -> Result<u128> {
    exact_micro_usd(usd, false)
}

/// Convert the exact binary64 value, not a rounded floating-point product.
/// Charges round upward and limits downward at the micro-USD boundary.
fn exact_micro_usd(usd: f64, round_up: bool) -> Result<u128> {
    if !usd.is_finite() || usd < 0.0 {
        return Err(anyhow!("cost amount is not finite and nonnegative"));
    }
    if usd == 0.0 {
        return Ok(0);
    }
    let bits = usd.to_bits();
    let exponent_bits = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1_u64 << 52) - 1);
    let (significand, exponent) = if exponent_bits == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1_u64 << 52), exponent_bits - 1023 - 52)
    };
    let numerator = u128::from(significand) * 1_000_000;
    if exponent >= 0 {
        let multiplier = 1_u128
            .checked_shl(exponent as u32)
            .ok_or_else(|| anyhow!("cost amount is out of range"))?;
        return numerator
            .checked_mul(multiplier)
            .ok_or_else(|| anyhow!("cost amount is out of range"));
    }
    let shift = exponent.unsigned_abs();
    if shift >= 128 {
        return Ok(u128::from(round_up));
    }
    let divisor = 1_u128 << shift;
    let whole = numerator / divisor;
    Ok(whole + u128::from(round_up && numerator % divisor != 0))
}

/// Open authority-bearing files relative to the verified state directory.
/// Creation does not grant permission to follow a link or use a shared inode.
#[derive(Clone, Copy)]
enum CostFileAccess {
    Read,
    Append,
    Migration,
    Lock,
}

#[cfg(unix)]
fn open_cost_file(path: &Path, access: CostFileAccess) -> Result<Option<(File, Option<File>)>> {
    use rustix::fs::{FileType, Mode, OFlags};
    let parent = path.parent().context("cost storage has no directory")?;
    let workspace = parent.parent().context("cost storage has no workspace")?;
    let directory = parent.file_name().context("cost storage directory has no name")?;
    let filename = path.file_name().context("cost storage has no filename")?;
    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let root = rustix::fs::open(workspace, directory_flags, Mode::empty())?;
    let state = match rustix::fs::openat(&root, directory, directory_flags, Mode::empty()) {
        Ok(state) => state,
        Err(rustix::io::Errno::NOENT) if matches!(access, CostFileAccess::Read) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    super::state_dir::validate(parent, &rustix::fs::fstat(&state)?)?;
    let flags = match access {
        CostFileAccess::Read => OFlags::RDONLY,
        CostFileAccess::Append => OFlags::WRONLY | OFlags::APPEND,
        CostFileAccess::Migration => OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
        CostFileAccess::Lock => OFlags::RDWR | OFlags::CREATE,
    };
    let creates = matches!(access, CostFileAccess::Append | CostFileAccess::Migration);
    let creation_flags = if creates {
        OFlags::CREATE | OFlags::EXCL
    } else {
        OFlags::empty()
    };
    let secure_flags = flags | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let (fd, created) =
        match rustix::fs::openat(&state, filename, secure_flags | creation_flags, Mode::RUSR | Mode::WUSR) {
            Ok(fd) => (fd, creates),
            Err(rustix::io::Errno::EXIST) if matches!(access, CostFileAccess::Append) => (
                rustix::fs::openat(&state, filename, secure_flags, Mode::RUSR | Mode::WUSR)?,
                false,
            ),
            Err(rustix::io::Errno::NOENT) if matches!(access, CostFileAccess::Read) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
    let file = File::from(fd);
    let state = File::from(state);
    let validate = || -> Result<()> {
        let stat = rustix::fs::fstat(&file)?;
        anyhow::ensure!(
            FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
                && stat.st_nlink == 1
                && stat.st_mode & 0o022 == 0,
            "cost file is not a private single-link regular file"
        );
        Ok(())
    };
    if created {
        super::creation::with_created_entry(&file, &state, filename, || {
            validate()?;
            // CostStorage may have just created state; persist that entry too.
            File::from(root).sync_all()?;
            Ok(())
        })?;
    } else {
        validate()?;
    }
    Ok(Some((file, created.then_some(state))))
}

// Keep ordinary cost recording available on non-Unix platforms. This path
// retains native file locking and does not claim Unix openat/ACL guarantees.
#[cfg(not(unix))]
fn open_cost_file(path: &Path, access: CostFileAccess) -> Result<Option<(File, Option<File>)>> {
    let parent = path.parent().context("cost storage has no directory")?;
    let directory = match fs::symlink_metadata(parent) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && matches!(access, CostFileAccess::Read) => {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        directory.is_dir() && !directory.file_type().is_symlink(),
        "unsafe cost directory"
    );
    match fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "unsafe cost file"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if matches!(access, CostFileAccess::Read) {
                return Ok(None);
            }
        }
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    match access {
        CostFileAccess::Read => {
            options.read(true);
        }
        CostFileAccess::Append => {
            options.append(true).create(true);
        }
        CostFileAccess::Migration => {
            options.write(true).create_new(true);
        }
        CostFileAccess::Lock => {
            options.read(true).write(true).create(true).truncate(false);
        }
    }
    let file = options.open(path)?;
    anyhow::ensure!(file.metadata()?.is_file(), "cost file is not regular");
    // Portable std has no directory sync facility on Windows. Preserve its
    // prior file-sync contract rather than disabling basic usage settlement.
    Ok(Some((file, None)))
}

struct CostLedgerLockGuard {
    _file: File,
}

fn acquire_cost_ledger_lock(path: &Path) -> Result<CostLedgerLockGuard> {
    let lock_path = path.with_extension("jsonl.lock");
    let (file, _) = open_cost_file(&lock_path, CostFileAccess::Lock)
        .with_context(|| format!("Failed to open cost ledger lock {}", lock_path.display()))?
        .context("cost ledger lock was not created")?;
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

/// One coherent, read-only view of existing physical owners. Missing old
/// evidence is Unknown (None), never a fresh empty tracker or a zero balance.
/// No mkdir, migration, reservation, settlement, projection refresh or write.
pub(crate) fn physical_scopes_read_only(
    workspace: &Path,
    scopes: &[String],
) -> Result<Option<std::collections::BTreeMap<String, super::reservation::ScopeUsage>>> {
    anyhow::ensure!(
        scopes.len() <= 64
            && scopes
                .iter()
                .all(|scope| scope.len() == 64 && scope.bytes().all(|byte| byte.is_ascii_hexdigit()))
            && scopes.iter().collect::<std::collections::BTreeSet<_>>().len() == scopes.len(),
        "invalid or repeated physical scope snapshot"
    );
    let path = workspace.join("state").join("costs.jsonl.lock");
    let Some((file, _)) = open_cost_file(&path, CostFileAccess::Read)? else {
        return Ok(None);
    };
    let started = std::time::Instant::now();
    loop {
        match file.try_lock_shared() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                if started.elapsed() > COST_LEDGER_LOCK_TIMEOUT {
                    return Err(anyhow!("timed out reading the existing physical ledger"));
                }
                std::thread::sleep(COST_LEDGER_LOCK_POLL);
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
    let _lock = CostLedgerLockGuard { _file: file };
    // Existing bounded no-follow readers and reservation replay validate every
    // terminal cost projection while the same original writer lock is shared.
    if read_state_ledger_bounded(workspace, super::reservation::JOURNAL)?.is_none()
        || read_state_ledger_bounded(workspace, "costs.jsonl")?.is_none()
    {
        return Ok(None);
    }
    let reservations = super::reservation::Reservations::load(workspace)?;
    let mut result = std::collections::BTreeMap::new();
    for scope in scopes {
        let mut usage = reservations.scope_usage(scope)?;
        usage.models = reservations.scope_models(workspace, scope, &std::collections::BTreeSet::new())?;
        anyhow::ensure!(
            !usage.unresolved && usage.calls == usage.settled_calls,
            "physical owner still has unresolved work"
        );
        result.insert(scope.clone(), usage);
    }
    Ok(Some(result))
}

/// Cost tracker for API usage monitoring and budget enforcement.
pub struct CostTracker {
    config: CostConfig,
    storage: Arc<Mutex<CostStorage>>,
    session_id: String,
    session_costs: Arc<Mutex<Vec<CostRecord>>>,
}

impl CostTracker {
    /// Reuse canonical storage with an immutable configuration view for each caller.
    pub fn for_workspace(config: CostConfig, workspace_dir: &Path) -> Result<Arc<Self>> {
        static TRACKERS: OnceLock<Mutex<HashMap<PathBuf, Arc<CostTracker>>>> = OnceLock::new();
        let workspace_dir = workspace_dir
            .canonicalize()
            .unwrap_or_else(|_| workspace_dir.to_path_buf());
        let trackers = TRACKERS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut trackers = trackers.lock();
        if let Some(tracker) = trackers.get(&workspace_dir) {
            if tracker.config == config {
                return Ok(Arc::clone(tracker));
            }
            let view = Arc::new(Self {
                config,
                storage: Arc::clone(&tracker.storage),
                session_id: tracker.session_id.clone(),
                session_costs: Arc::clone(&tracker.session_costs),
            });
            trackers.insert(workspace_dir, Arc::clone(&view));
            return Ok(view);
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
            config,
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

    /// Today's settled USD spend for this root agent, including its children.
    /// The canonical cross-process ledger lock protects the bounded snapshot.
    pub fn root_agent_spend_today(&self, agent: &super::types::RootAgent) -> Result<u128> {
        let storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        let text = read_state_ledger_bounded(storage.workspace()?, "costs.jsonl")?.unwrap_or_default();
        let today = Utc::now().date_naive();
        let mut total = 0u128;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let row: CostRecord = serde_json::from_str(line)?;
            if row.root_agent.as_ref() == Some(agent) && row.usage.timestamp.date_naive() == today {
                total = total
                    .checked_add(ceil_micro_usd(row.usage.cost_usd)?)
                    .context("agent daily spend overflows")?;
            }
        }
        Ok(total)
    }

    /// Check if a request is within budget.
    pub fn check_budget(&self, estimated_cost_usd: f64) -> Result<BudgetCheck> {
        let config = self.config.clone();
        if !estimated_cost_usd.is_finite() || estimated_cost_usd < 0.0 {
            return Err(anyhow!("Estimated cost must be a finite, non-negative value"));
        }

        let mut storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        storage.refresh_from_disk()?;
        let (daily_cost, monthly_cost) = storage.get_aggregated_costs()?;

        let pending = super::reservation::Reservations::load(storage.workspace()?)?.active_micro_usd()?;
        budget_check(
            &config,
            daily_cost,
            monthly_cost,
            storage
                .daily_micro_usd
                .checked_add(pending)
                .context("daily reserved cost overflows")?,
            storage
                .monthly_micro_usd
                .checked_add(pending)
                .context("monthly reserved cost overflows")?,
            estimated_cost_usd,
        )
    }

    /// Reserve a previously proved transport bound. This does not establish
    /// the bound itself or authorize a provider; callers must retain admission.
    #[cfg(test)]
    pub(crate) fn reserve_attempt(
        &self,
        request: super::reservation::ReservationRequest,
    ) -> Result<super::reservation::ReservationReceipt> {
        let config = self.config.clone();
        let mut storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        storage.refresh_from_disk()?;
        let workspace = storage.workspace()?;
        let mut reservations = super::reservation::Reservations::load(workspace)?;
        let active = reservations.active_micro_usd()?;
        let daily = storage
            .daily_micro_usd
            .checked_add(active)
            .context("daily reserved cost overflows")?;
        let monthly = storage
            .monthly_micro_usd
            .checked_add(active)
            .context("monthly reserved cost overflows")?;
        let remaining = day_limit_micro_usd(&config)?
            .saturating_sub(daily)
            .min(month_limit_micro_usd(&config)?.saturating_sub(monthly))
            .min(u128::from(request.security_daily_micro_usd).saturating_sub(daily));
        reservations.reserve(workspace, request, remaining)
    }

    /// Reserve the versioned local estimate under the actual ledger lock.
    #[cfg(test)]
    pub(crate) fn reserve_estimated_attempt(
        &self,
        request: super::reservation::EstimatedReservationRequest,
    ) -> Result<super::reservation::ReservationReceipt> {
        let config = self.config.clone();
        let mut storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        storage.refresh_from_disk()?;
        let workspace = storage.workspace()?;
        let mut reservations = super::reservation::Reservations::load(workspace)?;
        let active = reservations.active_micro_usd()?;
        let daily = storage
            .daily_micro_usd
            .checked_add(active)
            .context("daily reserved cost overflows")?;
        let monthly = storage
            .monthly_micro_usd
            .checked_add(active)
            .context("monthly reserved cost overflows")?;
        let remaining = day_limit_micro_usd(&config)?
            .saturating_sub(daily)
            .min(month_limit_micro_usd(&config)?.saturating_sub(monthly))
            .min(u128::from(request.reservation.security_daily_micro_usd).saturating_sub(daily));
        reservations.reserve_estimated(workspace, request, remaining)
    }

    /// Append an estimated reservation and its dispatched state in one synced
    /// batch under the actual ledger lock before a latency-bounded physical send.
    #[cfg(test)]
    pub(crate) fn reserve_dispatched_estimated_attempt(
        &self,
        request: super::reservation::EstimatedReservationRequest,
        may_dispatch: impl FnOnce() -> bool,
    ) -> Result<super::reservation::ReservationReceipt> {
        self.reserve_dispatched_estimated_attempt_with_limit(request, None, may_dispatch)
    }

    pub(crate) fn reserve_dispatched_estimated_attempt_with_limit(
        &self,
        request: super::reservation::EstimatedReservationRequest,
        call_limit: Option<u64>,
        may_dispatch: impl FnOnce() -> bool,
    ) -> Result<super::reservation::ReservationReceipt> {
        let config = self.config.clone();
        let mut storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        storage.refresh_from_disk()?;
        let workspace = storage.workspace()?;
        let mut reservations = super::reservation::Reservations::load(workspace)?;
        if let Some(limit) = call_limit {
            anyhow::ensure!(
                reservations.scope_usage(&request.reservation.turn_id)?.calls < limit,
                "experiment physical-call allowance exhausted"
            );
        }
        let active = reservations.active_micro_usd()?;
        let daily = storage
            .daily_micro_usd
            .checked_add(active)
            .context("daily reserved cost overflows")?;
        let monthly = storage
            .monthly_micro_usd
            .checked_add(active)
            .context("monthly reserved cost overflows")?;
        let remaining = day_limit_micro_usd(&config)?
            .saturating_sub(daily)
            .min(month_limit_micro_usd(&config)?.saturating_sub(monthly))
            .min(u128::from(request.reservation.security_daily_micro_usd).saturating_sub(daily));
        anyhow::ensure!(may_dispatch(), "attempt deadline elapsed before durable dispatch");
        reservations.reserve_dispatched_estimated(workspace, request, remaining)
    }

    /// Bounded by the existing ledger loader; no accounting mutations.
    pub(crate) fn physical_scope_usage(&self, scope: &str) -> Result<super::reservation::ScopeUsage> {
        self.physical_scope_usage_since(scope, &std::collections::BTreeSet::new())
    }
    /// A case observes only attempts created after its owned snapshot. Parent
    /// admission always reads the complete ledger and cannot use this filter.
    pub(crate) fn physical_scope_usage_since(
        &self,
        scope: &str,
        previous: &std::collections::BTreeSet<String>,
    ) -> Result<super::reservation::ScopeUsage> {
        let mut storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        storage.refresh_from_disk()?;
        let workspace = storage.workspace()?;
        let reservations = super::reservation::Reservations::load(workspace)?;
        let mut usage = reservations.scope_usage_since(scope, previous)?;
        usage.models = reservations.scope_models(workspace, scope, previous)?;
        Ok(usage)
    }

    /// Release a completed dispatch batch whose unique receipt has never been
    /// handed to a transport. Dispatched/unknown transport receipts cannot use
    /// this path; the runtime keeps them outside its unclaimed wrapper.
    pub(crate) fn release_unclaimed_estimated_attempt(
        &self,
        receipt: &super::reservation::ReservationReceipt,
    ) -> Result<()> {
        let mut storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        storage.refresh_from_disk()?;
        let workspace = storage.workspace()?;
        super::reservation::Reservations::load(workspace)?.release_unclaimed_estimated(workspace, receipt)
    }

    /// Persist before initiating a physical send. This transition is one-use;
    /// a repeated call cannot authorize another physical attempt.
    #[cfg(test)]
    pub(crate) fn mark_attempt_dispatched(&self, receipt: &super::reservation::ReservationReceipt) -> Result<()> {
        self.transition_reservation(receipt, true)
    }

    /// Explicitly release only a reservation that has never been dispatched.
    /// Dropping a receipt never releases potentially chargeable liability.
    #[cfg(test)]
    pub(crate) fn release_unsent_attempt(&self, receipt: &super::reservation::ReservationReceipt) -> Result<()> {
        self.transition_reservation(receipt, false)
    }

    #[cfg(test)]
    fn transition_reservation(&self, receipt: &super::reservation::ReservationReceipt, dispatch: bool) -> Result<()> {
        let config = self.config.clone();
        let mut storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        storage.refresh_from_disk()?;
        let workspace = storage.workspace()?;
        let mut reservations = super::reservation::Reservations::load(workspace)?;
        if dispatch {
            let pending = reservations.active_micro_usd()?;
            let daily = storage
                .daily_micro_usd
                .checked_add(pending)
                .context("daily reserved cost overflows")?;
            let monthly = storage
                .monthly_micro_usd
                .checked_add(pending)
                .context("monthly reserved cost overflows")?;
            anyhow::ensure!(
                daily <= u128::from(receipt.security_daily_micro_usd()),
                "reserved dispatch exceeds security ceiling"
            );
            let budget = budget_check(
                &config,
                storage.daily_cost_usd,
                storage.monthly_cost_usd,
                daily,
                monthly,
                0.0,
            )?;
            anyhow::ensure!(
                !matches!(budget, BudgetCheck::Exceeded { .. }),
                "reserved dispatch exceeds current budget"
            );
        }
        reservations.transition(workspace, receipt, dispatch)
    }

    /// Restore accounting ownership for reconciliation, never permission to
    /// resend. The recovered receipt still fails the one-use dispatch check.
    #[cfg(test)]
    pub(crate) fn recover_dispatched_attempt(
        &self,
        request: super::reservation::ReservationRequest,
    ) -> Result<super::reservation::ReservationReceipt> {
        let storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        let workspace = storage.workspace()?;
        super::reservation::Reservations::load(workspace)?.recover(workspace, request)
    }

    /// Reconcile only an already persisted canonical attempt settlement. The
    /// cost write precedes this transition, so interruption retains both the
    /// charge and conservative hold rather than releasing unpaid liability.
    #[cfg(test)]
    pub(crate) fn reconcile_reserved_attempt(
        &self,
        receipt: &super::reservation::ReservationReceipt,
        record: &crate::llm::route_decision::MeteredTokenUsageRecord,
    ) -> Result<()> {
        anyhow::ensure!(
            record.settlement_id.as_deref() == Some(receipt.attempt_id()),
            "reserved settlement does not identify this physical attempt"
        );
        let cost = record.cost_usd.context("reserved settlement price is unknown")?;
        let actual = ceil_micro_usd(cost)?;
        let mut storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        storage.refresh_from_disk()?;
        storage.verify_replayed_settlement(record, cost)?;
        let digest = settlement_fingerprint(record, cost)?;
        let mut projection = None;
        storage.for_each_record(|stored| {
            if stored.settlement_id == record.settlement_id {
                projection = Some(stored);
            }
        })?;
        let projection = projection.context("reservation settlement cost projection is missing")?;
        anyhow::ensure!(
            projection.settlement_sha256.as_deref() == Some(digest.as_str()),
            "reservation settlement requires a canonical cost projection"
        );
        let row_digest = super::reservation::cost_row_sha256(&projection)?;
        let workspace = storage.workspace()?;
        let mut reservations = super::reservation::Reservations::load(workspace)?;
        reservations.settle(workspace, receipt, actual, digest, row_digest)
    }

    /// Reconcile only a known, already durably metered estimated attempt.
    /// Partial/missing usage must retain the dispatched reservation instead.
    #[cfg(test)]
    pub(crate) fn reconcile_estimated_attempt(
        &self,
        receipt: &super::reservation::ReservationReceipt,
        record: &crate::llm::route_decision::MeteredTokenUsageRecord,
    ) -> Result<()> {
        anyhow::ensure!(
            record.settlement_id.as_deref() == Some(receipt.attempt_id()),
            "reserved settlement does not identify this physical attempt"
        );
        let cost = record.cost_usd.context("reserved settlement price is unknown")?;
        let actual = ceil_micro_usd(cost)?;
        let mut storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        storage.refresh_from_disk()?;
        storage.verify_replayed_settlement(record, cost)?;
        settlement::reconcile_durable_projection(&storage, receipt, record, actual, None)
    }

    /// Record a usage event.
    pub fn record_usage(&self, usage: TokenUsage) -> Result<()> {
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
    /// A subscription-billed row with no dollar amount returns
    /// [`CostSettlement::Subscription`] naming the provider, model, and token
    /// counts. It writes no priced cost row.
    pub fn settle_metered(
        &self,
        record: &crate::llm::route_decision::MeteredTokenUsageRecord,
    ) -> Result<CostSettlement> {
        #[cfg(test)]
        {
            let path = self.lock_storage().path.clone();
            *settlement_call_counts().lock().entry(path).or_default() += 1;
        }
        self.settle_metered_inner(record, None)
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

fn budget_check(
    config: &CostConfig,
    daily_cost: f64,
    monthly_cost: f64,
    daily_micro: u128,
    monthly_micro: u128,
    estimated_cost_usd: f64,
) -> Result<BudgetCheck> {
    // Match the read-only admission snapshot: each charge rounds up and
    // configured ceilings round down. Floating-point addition can lose a
    // small new charge when the current total is near its limit.
    let estimate = ceil_micro_usd(estimated_cost_usd)?;
    let day_limit = day_limit_micro_usd(config)?;
    let month_limit = month_limit_micro_usd(config)?;
    let projected_daily = daily_micro
        .checked_add(estimate)
        .context("daily projected cost overflows")?;
    let projected_monthly = monthly_micro
        .checked_add(estimate)
        .context("monthly projected cost overflows")?;
    // An unset limit is `u128::MAX` micro-USD and never exceeded.
    if let Some(limit_usd) = config.daily_limit_usd
        && projected_daily > day_limit
    {
        return Ok(BudgetCheck::Exceeded {
            current_usd: daily_cost,
            limit_usd,
            period: UsagePeriod::Day,
        });
    }
    if let Some(limit_usd) = config.monthly_limit_usd
        && projected_monthly > month_limit
    {
        return Ok(BudgetCheck::Exceeded {
            current_usd: monthly_cost,
            limit_usd,
            period: UsagePeriod::Month,
        });
    }
    // Divide before multiplying to keep the percentage calculation bounded.
    let percent = u128::from(config.warn_at_percent.min(100));
    let warning_at = |limit: u128| (limit / 100) * percent + ((limit % 100) * percent).div_ceil(100);
    // With no limit there is nothing to warn against.
    if let Some(limit_usd) = config.daily_limit_usd
        && projected_daily >= warning_at(day_limit)
    {
        return Ok(BudgetCheck::Warning {
            current_usd: daily_cost,
            limit_usd,
            period: UsagePeriod::Day,
        });
    }
    if let Some(limit_usd) = config.monthly_limit_usd
        && projected_monthly >= warning_at(month_limit)
    {
        return Ok(BudgetCheck::Warning {
            current_usd: monthly_cost,
            limit_usd,
            period: UsagePeriod::Month,
        });
    }
    Ok(BudgetCheck::Allowed)
}

fn resolve_storage_path(workspace_dir: &Path) -> Result<PathBuf> {
    let storage_path = workspace_dir.join("state").join("costs.jsonl");
    let legacy_path = workspace_dir.join(".openprx").join("costs.db");
    crate::util::create_private_dir_all(&workspace_dir.join("state"))
        .context("failed to create cost state directory")?;

    // Migration shares the same lock as settlement. Open each inode before
    // copying; neither a legacy symlink nor a target link can redirect writes.
    let _ledger_lock = acquire_cost_ledger_lock(&storage_path)?;
    if open_cost_file(&storage_path, CostFileAccess::Read)?.is_none()
        && let Some((legacy, _)) = open_cost_file(&legacy_path, CostFileAccess::Read)?
    {
        migrate_legacy_cost_file(legacy, &storage_path)?;
    }
    Ok(storage_path)
}

#[cfg(unix)]
fn migrate_legacy_cost_file(mut legacy: File, storage_path: &Path) -> Result<()> {
    let temporary_name = format!("costs.migration.{}.jsonl", uuid::Uuid::new_v4());
    let temporary = storage_path.with_file_name(&temporary_name);
    let (mut destination, state) =
        open_cost_file(&temporary, CostFileAccess::Migration)?.context("cost migration file was not created")?;
    std::io::copy(&mut legacy, &mut destination).context("failed to copy legacy cost ledger")?;
    destination.sync_all().context("failed to sync migrated cost ledger")?;
    // The ledger is published only after the entire copy is durable. A crash
    // leaves an unreferenced temporary file, never an accepted prefix balance.
    let state = state.context("cost migration directory handle is missing")?;
    rustix::fs::renameat(&state, temporary_name.as_str(), &state, "costs.jsonl")
        .context("failed to publish migrated cost ledger")?;
    super::creation::sync_created_entry(&destination, &state, std::ffi::OsStr::new("costs.jsonl"), &state)?;
    // Preserve the legacy source; no migration cleanup is performed here.
    Ok(())
}

#[cfg(not(unix))]
fn migrate_legacy_cost_file(mut legacy: File, storage_path: &Path) -> Result<()> {
    let temporary = storage_path.with_file_name(format!("costs.migration.{}.jsonl", uuid::Uuid::new_v4()));
    let (mut destination, _) =
        open_cost_file(&temporary, CostFileAccess::Migration)?.context("cost migration file was not created")?;
    std::io::copy(&mut legacy, &mut destination).context("failed to copy legacy cost ledger")?;
    destination.sync_all().context("failed to sync migrated cost ledger")?;
    drop(destination);
    fs::rename(&temporary, storage_path).context("failed to publish migrated cost ledger")?;
    Ok(())
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

/// Versioned settlement identity excludes wall time and tracker session. The
/// explicit tuple stays stable if the metered record gains unrelated fields.
fn settlement_fingerprint(
    record: &crate::llm::route_decision::MeteredTokenUsageRecord,
    cost_usd: f64,
) -> Result<String> {
    let mut bytes = serde_json::to_vec(&(
        "prx-cost-settlement-v1",
        &record.provider,
        &record.model,
        record.prompt_tokens,
        record.completion_tokens,
        record.total_tokens,
        record.cache_creation_input_tokens,
        record.cache_read_input_tokens,
        &record.source,
        cost_usd.to_bits(),
    ))?;
    if let Some(root) = &record.root_agent {
        bytes.extend(serde_json::to_vec(root)?);
    }
    Ok(hex::encode(<sha2::Sha256 as sha2::Digest>::digest(bytes)))
}

/// Whether a stored settlement row carries the fingerprint the ledger derives
/// for it. A row written with its settlement inputs (provider, source and cache
/// counts) is checked exactly. An older row lacks them, so every split of the
/// model string and both sources are tried with no cache tokens; a legacy row
/// that settled cache usage does not verify (fail closed).
pub(crate) fn settlement_row_is_authentic(row: &CostRecord) -> bool {
    use crate::llm::route_decision::{MeteredTokenUsageRecord, TokenUsageSource};
    let (Some(settlement_id), Some(digest)) = (row.settlement_id.as_deref(), row.settlement_sha256.as_deref()) else {
        return false;
    };
    if settlement_id.trim().is_empty() {
        return false;
    }
    let usage = &row.usage;
    let matches = |provider: &str, model: &str, source: TokenUsageSource| {
        let record = MeteredTokenUsageRecord {
            root_agent: row.root_agent.clone(),
            settlement_id: Some(settlement_id.to_string()),
            provider: provider.to_string(),
            model: model.to_string(),
            prompt_tokens: usage.input_tokens,
            completion_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            cache_creation_input_tokens: row.cache_creation_input_tokens.unwrap_or(0),
            cache_read_input_tokens: row.cache_read_input_tokens.unwrap_or(0),
            source,
            cost_usd: Some(usage.cost_usd),
            settlement_class: None,
        };
        settlement_fingerprint(&record, usage.cost_usd).is_ok_and(|expected| expected == digest)
    };
    if let (Some(provider), Some(source)) = (row.settlement_provider.as_deref(), row.settlement_source) {
        return usage
            .model
            .strip_prefix(provider)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|model| matches(provider, model, source));
    }
    usage.model.match_indices('/').any(|(at, _)| {
        let (provider, model) = usage.model.split_at(at);
        [TokenUsageSource::Reported, TokenUsageSource::Estimated]
            .into_iter()
            .any(|source| matches(provider, model.trim_start_matches('/'), source))
    })
}

/// Persistent storage for cost records.
struct CostStorage {
    path: PathBuf,
    daily_cost_usd: f64,
    monthly_cost_usd: f64,
    daily_micro_usd: u128,
    monthly_micro_usd: u128,
    cached_day: NaiveDate,
    cached_year: i32,
    cached_month: u32,
    settlement_ids: HashSet<String>,
}

impl CostStorage {
    fn workspace(&self) -> Result<&Path> {
        self.path
            .parent()
            .and_then(Path::parent)
            .context("cost storage has no workspace")
    }

    /// Create or open cost storage.
    fn new(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            crate::util::create_private_dir_all(parent)
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }

        let now = Utc::now();
        let mut storage = Self {
            path: path.to_path_buf(),
            daily_cost_usd: 0.0,
            monthly_cost_usd: 0.0,
            daily_micro_usd: 0,
            monthly_micro_usd: 0,
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

    /// The `billing` class of the stored row for `settlement_id`. The caller
    /// holds the cross-process ledger lock and has verified the replay.
    fn settlement_billing(&self, settlement_id: &str) -> Result<Option<String>> {
        let mut billing = None;
        self.for_each_record(|stored| {
            if stored.settlement_id.as_deref() == Some(settlement_id) {
                billing = stored.billing;
            }
        })?;
        Ok(billing)
    }

    /// The caller holds the cross-process ledger lock. Legacy rows prove
    /// only their persisted projection; new rows also bind cache and source.
    fn verify_replayed_settlement(
        &self,
        incoming: &crate::llm::route_decision::MeteredTokenUsageRecord,
        cost_usd: f64,
    ) -> Result<()> {
        let fingerprint = settlement_fingerprint(incoming, cost_usd)?;
        let model = format!("{}/{}", incoming.provider, incoming.model);
        let mut matches = 0usize;
        let mut conflict = false;
        self.for_each_record(|stored| {
            if stored.settlement_id != incoming.settlement_id {
                return;
            }
            matches += 1;
            let projection_matches = stored.usage.model == model
                && stored.usage.input_tokens == incoming.prompt_tokens
                && stored.usage.output_tokens == incoming.completion_tokens
                && stored.usage.total_tokens == incoming.total_tokens
                && stored.usage.cost_usd.to_bits() == cost_usd.to_bits();
            conflict |= !projection_matches
                || stored
                    .settlement_sha256
                    .as_deref()
                    .is_some_and(|prior| prior != fingerprint);
        })?;
        anyhow::ensure!(matches == 1 && !conflict, LedgerBlocked::Conflict);
        Ok(())
    }

    fn for_each_record<F>(&self, mut on_record: F) -> Result<()>
    where
        F: FnMut(CostRecord),
    {
        let Some((file, _)) = open_cost_file(&self.path, CostFileAccess::Read)
            .with_context(|| format!("Failed to read cost storage from {}", self.path.display()))?
        else {
            return Ok(());
        };
        let mut reader = BufReader::new(file);
        let mut raw_line = String::new();
        let mut line_number = 0usize;
        loop {
            raw_line.clear();
            let bytes = reader.read_line(&mut raw_line).with_context(|| {
                format!(
                    "Failed to read line {} from cost storage {}",
                    line_number + 1,
                    self.path.display()
                )
            })?;
            if bytes == 0 {
                break;
            }
            line_number += 1;
            // A synced append always includes its newline. Even a valid JSON
            // value without that delimiter may be an interrupted append; do
            // not accept it and concatenate the next settlement onto its tail.
            anyhow::ensure!(raw_line.ends_with('\n'), LedgerBlocked::TornTail);

            let trimmed = raw_line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let record: CostRecord =
                serde_json::from_str(trimmed).with_context(|| format!("cost ledger row {line_number} is malformed"))?;
            anyhow::ensure!(
                record.usage.cost_usd.is_finite() && record.usage.cost_usd >= 0.0,
                "cost ledger contains an invalid amount"
            );
            on_record(record);
        }

        Ok(())
    }

    fn rebuild_aggregates(&mut self, day: NaiveDate, year: i32, month: u32) -> Result<()> {
        let mut daily_cost = 0.0;
        let mut monthly_cost = 0.0;
        let mut daily_micro = Some(0_u128);
        let mut monthly_micro = Some(0_u128);

        self.for_each_record(|record| {
            let timestamp = record.usage.timestamp.naive_utc();
            let micro = ceil_micro_usd(record.usage.cost_usd).ok();

            if timestamp.date() == day {
                daily_cost += record.usage.cost_usd;
                daily_micro = daily_micro.zip(micro).and_then(|(sum, cost)| sum.checked_add(cost));
            }

            if timestamp.year() == year && timestamp.month() == month {
                monthly_cost += record.usage.cost_usd;
                monthly_micro = monthly_micro.zip(micro).and_then(|(sum, cost)| sum.checked_add(cost));
            }
        })?;

        anyhow::ensure!(
            daily_cost.is_finite() && monthly_cost.is_finite(),
            "cost ledger aggregate overflows"
        );
        let daily_micro = daily_micro.context("daily cost ledger micro-USD aggregate overflows")?;
        let monthly_micro = monthly_micro.context("monthly cost ledger micro-USD aggregate overflows")?;
        self.daily_micro_usd = daily_micro;
        self.monthly_micro_usd = monthly_micro;
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
        // Refresh before appending: a rollover rebuild must not already include
        // the row that the incremental update below is about to account for.
        self.ensure_period_cache_current()?;
        let micro = ceil_micro_usd(record.usage.cost_usd)?;
        let timestamp = record.usage.timestamp.naive_utc();
        let daily_micro = self
            .daily_micro_usd
            .checked_add(if timestamp.date() == self.cached_day { micro } else { 0 })
            .context("daily cost ledger micro-USD aggregate overflows")?;
        let monthly_micro = self
            .monthly_micro_usd
            .checked_add(
                if timestamp.year() == self.cached_year && timestamp.month() == self.cached_month {
                    micro
                } else {
                    0
                },
            )
            .context("monthly cost ledger micro-USD aggregate overflows")?;
        let mut encoded = serde_json::to_vec(&record)?;
        encoded.push(b'\n');
        let (mut file, state) = open_cost_file(&self.path, CostFileAccess::Append)
            .with_context(|| format!("Failed to open cost storage at {}", self.path.display()))?
            .context("cost ledger was not created")?;
        let result = (|| -> Result<()> {
            file.write_all(&encoded)
                .with_context(|| format!("Failed to write cost record to {}", self.path.display()))?;
            file.sync_all()
                .with_context(|| format!("Failed to sync cost storage at {}", self.path.display()))?;
            Ok(())
        })();
        #[cfg(unix)]
        if let Some(parent) = state.as_ref() {
            super::creation::with_created_entry(
                &file,
                parent,
                self.path.file_name().context("cost ledger has no filename")?,
                || {
                    result?;
                    parent.sync_all()?;
                    Ok(())
                },
            )?;
        } else {
            result?;
        }
        #[cfg(not(unix))]
        {
            let _ = state;
            result?;
        }

        self.daily_micro_usd = daily_micro;
        self.monthly_micro_usd = monthly_micro;
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
mod tests;

#[cfg(test)]
pub(crate) fn probe_ledger_lock_contention() -> Result<String> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("costs.jsonl");
    let _held = acquire_cost_ledger_lock(&path)?;
    let error = match acquire_cost_ledger_lock(&path) {
        Ok(_) => anyhow::bail!("test: second ledger owner unexpectedly acquired held lock"),
        Err(error) => error,
    };
    Ok(format!("{error:#}"))
}

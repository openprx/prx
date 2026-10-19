use super::types::{BudgetCheck, CostRecord, CostSettlement, CostSummary, ModelStats, TokenUsage, UsagePeriod};
use crate::config::schema::CostConfig;
use anyhow::{Context, Result, anyhow};
use chrono::{Datelike, NaiveDate, Utc};
use parking_lot::{Mutex, MutexGuard};
use std::collections::{HashMap, HashSet};
#[cfg(any(test, not(unix)))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

mod settlement;

const COST_LEDGER_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const COST_LEDGER_LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(20);
const MAX_READ_ONLY_BUDGET_BYTES: u64 = 8 * 1024 * 1024;

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
    if FileType::from_raw_mode(state_stat.st_mode) != FileType::Directory || state_stat.st_mode & 0o022 != 0 {
        return Err(anyhow!("cost state directory is writable by another account"));
    }
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
        return Err(anyhow!("cost ledger ends with an incomplete row"));
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
    if !config.daily_limit_usd.is_finite()
        || !config.monthly_limit_usd.is_finite()
        || config.daily_limit_usd < 0.0
        || config.monthly_limit_usd < 0.0
    {
        return Err(anyhow!("cost limits are not finite nonnegative amounts"));
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
    let day_limit = floor_micro_usd(config.daily_limit_usd)?;
    let month_limit = floor_micro_usd(config.monthly_limit_usd)?;
    let remaining = day_limit
        .saturating_sub(daily)
        .min(month_limit.saturating_sub(monthly))
        .min(security_daily_micro.map_or(u128::MAX, |limit| limit.saturating_sub(daily)));
    u64::try_from(remaining).context("remaining cost budget is out of range")
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
    anyhow::ensure!(
        rustix::fs::fstat(&state)?.st_mode & 0o022 == 0,
        "cost state directory is writable by another account"
    );
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
    let stat = rustix::fs::fstat(&fd)?;
    anyhow::ensure!(
        FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
            && stat.st_nlink == 1
            && stat.st_mode & 0o022 == 0,
        "cost file is not a private single-link regular file"
    );
    if created {
        // CostStorage may have just created state; persist that entry too.
        File::from(root)
            .sync_all()
            .context("failed to sync cost workspace directory")?;
    }
    Ok(Some((File::from(fd), created.then(|| File::from(state)))))
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

fn sync_cost_directory(state: Option<File>) -> Result<()> {
    if let Some(state) = state {
        state.sync_all().context("failed to sync cost state directory")?;
    }
    Ok(())
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
        let config = self.config.lock().clone();
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
        let remaining = floor_micro_usd(config.daily_limit_usd)?
            .saturating_sub(daily)
            .min(floor_micro_usd(config.monthly_limit_usd)?.saturating_sub(monthly))
            .min(u128::from(request.security_daily_micro_usd).saturating_sub(daily));
        reservations.reserve(workspace, request, remaining)
    }

    /// Reserve the versioned local estimate under the actual ledger lock.
    #[cfg(test)]
    pub(crate) fn reserve_estimated_attempt(
        &self,
        request: super::reservation::EstimatedReservationRequest,
    ) -> Result<super::reservation::ReservationReceipt> {
        let config = self.config.lock().clone();
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
        let remaining = floor_micro_usd(config.daily_limit_usd)?
            .saturating_sub(daily)
            .min(floor_micro_usd(config.monthly_limit_usd)?.saturating_sub(monthly))
            .min(u128::from(request.reservation.security_daily_micro_usd).saturating_sub(daily));
        reservations.reserve_estimated(workspace, request, remaining)
    }

    /// Append an estimated reservation and its dispatched state in one synced
    /// batch under the actual ledger lock before a latency-bounded physical send.
    pub(crate) fn reserve_dispatched_estimated_attempt(
        &self,
        request: super::reservation::EstimatedReservationRequest,
        may_dispatch: impl FnOnce() -> bool,
    ) -> Result<super::reservation::ReservationReceipt> {
        let config = self.config.lock().clone();
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
        let remaining = floor_micro_usd(config.daily_limit_usd)?
            .saturating_sub(daily)
            .min(floor_micro_usd(config.monthly_limit_usd)?.saturating_sub(monthly))
            .min(u128::from(request.reservation.security_daily_micro_usd).saturating_sub(daily));
        anyhow::ensure!(may_dispatch(), "attempt deadline elapsed before durable dispatch");
        reservations.reserve_dispatched_estimated(workspace, request, remaining)
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
        let config = self.config.lock().clone();
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
    pub fn settle_metered(
        &self,
        record: &crate::llm::route_decision::MeteredTokenUsageRecord,
    ) -> Result<CostSettlement> {
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
    let day_limit = floor_micro_usd(config.daily_limit_usd)?;
    let month_limit = floor_micro_usd(config.monthly_limit_usd)?;
    let projected_daily = daily_micro
        .checked_add(estimate)
        .context("daily projected cost overflows")?;
    let projected_monthly = monthly_micro
        .checked_add(estimate)
        .context("monthly projected cost overflows")?;
    if projected_daily > day_limit {
        return Ok(BudgetCheck::Exceeded {
            current_usd: daily_cost,
            limit_usd: config.daily_limit_usd,
            period: UsagePeriod::Day,
        });
    }
    if projected_monthly > month_limit {
        return Ok(BudgetCheck::Exceeded {
            current_usd: monthly_cost,
            limit_usd: config.monthly_limit_usd,
            period: UsagePeriod::Month,
        });
    }
    // Divide before multiplying to keep the percentage calculation bounded.
    let percent = u128::from(config.warn_at_percent.min(100));
    let warning_at = |limit: u128| (limit / 100) * percent + ((limit % 100) * percent).div_ceil(100);
    if projected_daily >= warning_at(day_limit) {
        return Ok(BudgetCheck::Warning {
            current_usd: daily_cost,
            limit_usd: config.daily_limit_usd,
            period: UsagePeriod::Day,
        });
    }
    if projected_monthly >= warning_at(month_limit) {
        return Ok(BudgetCheck::Warning {
            current_usd: monthly_cost,
            limit_usd: config.monthly_limit_usd,
            period: UsagePeriod::Month,
        });
    }
    Ok(BudgetCheck::Allowed)
}

fn resolve_storage_path(workspace_dir: &Path) -> Result<PathBuf> {
    let storage_path = workspace_dir.join("state").join("costs.jsonl");
    let legacy_path = workspace_dir.join(".openprx").join("costs.db");
    fs::create_dir_all(workspace_dir.join("state")).context("failed to create cost state directory")?;

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
    state
        .sync_all()
        .context("failed to sync migrated cost state directory")?;
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
    let bytes = serde_json::to_vec(&(
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
            fs::create_dir_all(parent).with_context(|| format!("Failed to create directory {}", parent.display()))?;
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
        anyhow::ensure!(
            matches == 1 && !conflict,
            "Metered usage settlement conflicts with the cost ledger"
        );
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
            anyhow::ensure!(raw_line.ends_with('\n'), "cost ledger ends with an incomplete row");

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
        file.write_all(&encoded)
            .with_context(|| format!("Failed to write cost record to {}", self.path.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to sync cost storage at {}", self.path.display()))?;
        // A durable file body is insufficient when this append created the
        // ledger. Persist its directory entry before acknowledging settlement.
        sync_cost_directory(state)?;

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
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn enabled_config() -> CostConfig {
        CostConfig::default()
    }

    fn reservation(attempt: &str, maximum: u64, turn_limit: u64) -> super::super::reservation::ReservationRequest {
        super::super::reservation::ReservationRequest {
            turn_id: "prx-turn".into(),
            attempt_id: attempt.into(),
            binding_sha256: "a".repeat(64),
            maximum_micro_usd: maximum,
            turn_limit_micro_usd: turn_limit,
            security_daily_micro_usd: 1_000_000,
        }
    }

    fn estimated(
        attempt: &str,
        money: u64,
        tokens: u64,
        token_limit: u64,
    ) -> super::super::reservation::EstimatedReservationRequest {
        super::super::reservation::EstimatedReservationRequest {
            reservation: reservation(attempt, money, 1_000_000),
            reserved_tokens: tokens,
            turn_token_limit: token_limit,
        }
    }

    #[test]
    fn estimated_attempt_overrun_records_actual_without_legacy_poison() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            daily_limit_usd: 1.0,
            monthly_limit_usd: 1.0,
            ..enabled_config()
        };
        let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
        let receipt = tracker
            .reserve_estimated_attempt(estimated("prx-estimated", 1, 10, 30))
            .unwrap();
        tracker.mark_attempt_dispatched(&receipt).unwrap();
        assert!(tracker.mark_attempt_dispatched(&receipt).is_err());
        let record = metered_record("prx-estimated", Some(0.25));
        assert!(tracker.reconcile_estimated_attempt(&receipt, &record).is_err());
        tracker.settle_metered(&record).unwrap();
        tracker.reconcile_estimated_attempt(&receipt, &record).unwrap();
        tracker.reconcile_estimated_attempt(&receipt, &record).unwrap();
        assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 750_000);
        // Actual 15 tokens replaces estimate 10, rather than clamping it.
        assert!(
            tracker
                .reserve_estimated_attempt(estimated("prx-over-tokens", 1, 16, 30))
                .is_err()
        );
        let next = tracker
            .reserve_estimated_attempt(estimated("prx-next-estimate", 1, 15, 30))
            .unwrap();
        tracker.release_unsent_attempt(&next).unwrap();
        let reopened = CostTracker::new(config, tmp.path()).unwrap();
        assert!(
            reopened
                .reserve_estimated_attempt(estimated("prx-after-restart", 1, 15, 30))
                .is_ok()
        );
    }

    #[test]
    fn estimated_unknown_drop_retains_tokens_and_money_across_reopen() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            daily_limit_usd: 1.0,
            monthly_limit_usd: 1.0,
            ..enabled_config()
        };
        let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
        let receipt = tracker
            .reserve_estimated_attempt(estimated("prx-unknown", 250_000, 30, 30))
            .unwrap();
        tracker.mark_attempt_dispatched(&receipt).unwrap();
        assert!(tracker.release_unsent_attempt(&receipt).is_err());
        drop(receipt);
        drop(tracker);
        let reopened = CostTracker::new(config.clone(), tmp.path()).unwrap();
        assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 750_000);
        assert!(
            reopened
                .reserve_estimated_attempt(estimated("prx-no-token-reset", 1, 1, 30))
                .is_err()
        );
    }

    #[test]
    fn estimated_dispatch_batches_existing_journal_rows_before_authority_returns() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            daily_limit_usd: 1.0,
            monthly_limit_usd: 1.0,
            ..enabled_config()
        };
        let request = estimated("prx-batched-dispatch", 250_000, 30, 30);
        let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
        let receipt = tracker
            .reserve_dispatched_estimated_attempt(request.clone(), || true)
            .unwrap();
        assert!(tracker.mark_attempt_dispatched(&receipt).is_err());
        assert!(tracker.release_unsent_attempt(&receipt).is_err());
        let rows: Vec<serde_json::Value> =
            std::fs::read_to_string(tmp.path().join("state").join(super::super::reservation::JOURNAL))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.first()
                .and_then(|row| row.get("event"))
                .and_then(serde_json::Value::as_str),
            Some("estimated_reserved_v1")
        );
        assert_eq!(
            rows.get(1)
                .and_then(|row| row.get("event"))
                .and_then(serde_json::Value::as_str),
            Some("dispatched")
        );
        let reserved_attempt_id = rows
            .first()
            .and_then(|row| row.get("request"))
            .and_then(|request| request.get("reservation"))
            .and_then(|reservation| reservation.get("attempt_id"));
        let dispatched_attempt_id = rows.get(1).and_then(|row| row.get("attempt_id"));
        assert!(reserved_attempt_id.is_some());
        assert_eq!(reserved_attempt_id, dispatched_attempt_id);

        drop(receipt);
        drop(tracker);
        let reopened = CostTracker::new(config, tmp.path()).unwrap();
        assert!(reopened.reserve_dispatched_estimated_attempt(request, || true).is_err());
    }

    #[test]
    fn expired_reservation_wait_never_marks_attempt_dispatched() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            daily_limit_usd: 1.0,
            monthly_limit_usd: 1.0,
            ..enabled_config()
        };
        let tracker = Arc::new(CostTracker::new(config, tmp.path()).unwrap());
        let lock_path = tmp.path().join("state/costs.jsonl.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .unwrap();
        lock.lock().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(40);
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
        let blocked = {
            let tracker = Arc::clone(&tracker);
            std::thread::spawn(move || {
                entered_tx.send(()).unwrap();
                tracker.reserve_dispatched_estimated_attempt(
                    estimated("prx-expired-before-dispatch", 250_000, 30, 30),
                    || std::time::Instant::now() < deadline,
                )
            })
        };
        entered_rx.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(80));
        lock.unlock().unwrap();

        let error = blocked.join().unwrap().expect_err("expired attempt must not dispatch");
        assert!(error.to_string().contains("deadline elapsed"), "{error:#}");
        let journal = tmp.path().join("state").join(super::super::reservation::JOURNAL);
        assert!(
            !journal.exists() || std::fs::read_to_string(journal).unwrap().is_empty(),
            "an expired wait must not leave a reservation or dispatched hold"
        );
    }

    #[test]
    fn reservation_reconciliation_requires_cost_first_and_preserves_turn_spend() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            daily_limit_usd: 1.0,
            monthly_limit_usd: 1.0,
            ..enabled_config()
        };
        let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
        let request = reservation("prx-attempt-settle", 750_000, 1_000_000);
        let receipt = tracker.reserve_attempt(request.clone()).unwrap();
        tracker.mark_attempt_dispatched(&receipt).unwrap();
        let record = metered_record("prx-attempt-settle", Some(0.25));
        assert!(tracker.reconcile_reserved_attempt(&receipt, &record).is_err());
        tracker.settle_metered(&record).unwrap();
        // Simulate interruption after durable projection but before hold release.
        drop(tracker);
        drop(receipt);
        let reopened = CostTracker::new(config.clone(), tmp.path()).unwrap();
        assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 0);
        let recovered = reopened.recover_dispatched_attempt(request).unwrap();
        assert!(reopened.mark_attempt_dispatched(&recovered).is_err());
        reopened.reconcile_reserved_attempt(&recovered, &record).unwrap();
        assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 750_000);
        let journal = tmp.path().join("state/costs.reservations.jsonl");
        let before = fs::read(&journal).unwrap();
        reopened.reconcile_reserved_attempt(&recovered, &record).unwrap();
        assert_eq!(fs::read(&journal).unwrap(), before);
        assert!(
            reopened
                .reserve_attempt(reservation("prx-too-large", 800_000, 1_000_000))
                .is_err()
        );
        assert!(
            reopened
                .reserve_attempt(reservation("prx-remaining", 750_000, 1_000_000))
                .is_ok()
        );
    }

    #[test]
    fn over_bound_settlement_is_durable_and_blocks_further_admission() {
        let tmp = TempDir::new().unwrap();
        let config = enabled_config();
        let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
        let request = reservation("prx-over-bound", 250_000, 1_000_000);
        let receipt = tracker.reserve_attempt(request.clone()).unwrap();
        tracker.mark_attempt_dispatched(&receipt).unwrap();
        let record = metered_record("prx-over-bound", Some(0.5));
        tracker.settle_metered(&record).unwrap();
        // Cost projection alone must close the crash window before the
        // explicit terminal reservation transition can be appended.
        assert!(tracker.check_budget(0.0).is_err());
        assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
        let before_reconciliation = CostTracker::new(config.clone(), tmp.path()).unwrap();
        assert!(
            before_reconciliation
                .reserve_attempt(reservation("prx-crash-window", 1, 1_000_000))
                .is_err()
        );
        assert!(tracker.reconcile_reserved_attempt(&receipt, &record).is_err());
        drop(tracker);
        let reopened = CostTracker::new(config.clone(), tmp.path()).unwrap();
        assert_eq!(reopened.get_summary().unwrap().daily_cost_usd, 0.5);
        assert!(reopened.check_budget(0.0).is_err());
        assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
        assert!(reopened.reserve_attempt(reservation("prx-next", 1, 1_000_000)).is_err());
        let recovered = reopened.recover_dispatched_attempt(request).unwrap();
        assert!(reopened.reconcile_reserved_attempt(&recovered, &record).is_err());
        assert!(reopened.release_unsent_attempt(&recovered).is_err());
    }

    #[test]
    fn over_bound_settlement_above_u64_micro_usd_reopens_without_losing_the_charge() {
        let tmp = TempDir::new().unwrap();
        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        let request = reservation("prx-large-over-bound", 1, 1);
        let receipt = tracker.reserve_attempt(request.clone()).unwrap();
        tracker.mark_attempt_dispatched(&receipt).unwrap();
        let actual = 2_f64.powi(45);
        assert!(ceil_micro_usd(actual).unwrap() > u128::from(u64::MAX));
        let record = metered_record("prx-large-over-bound", Some(actual));
        tracker.settle_metered(&record).unwrap();
        assert!(tracker.reconcile_reserved_attempt(&receipt, &record).is_err());
        let reopened = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        let recovered = reopened.recover_dispatched_attempt(request).unwrap();
        assert!(reopened.reconcile_reserved_attempt(&recovered, &record).is_err());
        assert_eq!(reopened.get_summary().unwrap().daily_cost_usd, actual);
        assert!(reopened.check_budget(0.0).is_err());
    }

    #[test]
    fn reconciled_hold_rejects_missing_or_conflicting_cost_projection() {
        let tmp = TempDir::new().unwrap();
        let config = enabled_config();
        let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
        let request = reservation("prx-projection", 250_000, 1_000_000);
        let receipt = tracker.reserve_attempt(request).unwrap();
        tracker.mark_attempt_dispatched(&receipt).unwrap();
        let record = metered_record("prx-projection", Some(0.125));
        tracker.settle_metered(&record).unwrap();
        tracker.reconcile_reserved_attempt(&receipt, &record).unwrap();
        let mut conflict = record;
        conflict.prompt_tokens += 1;
        assert!(tracker.reconcile_reserved_attempt(&receipt, &conflict).is_err());
        let path = tmp.path().join("state/costs.jsonl");
        // A damaged/restored ledger must not turn terminal journal state into free capacity.
        fs::write(&path, b"").unwrap();
        assert!(tracker.check_budget(0.0).is_err());
        assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
    }

    #[test]
    fn reservation_recovery_binds_full_cost_row_despite_unchanged_settlement_digest() {
        let tmp = TempDir::new().unwrap();
        let config = enabled_config();
        let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
        let request = reservation("prx-full-projection", 250_000, 1_000_000);
        let receipt = tracker.reserve_attempt(request.clone()).unwrap();
        tracker.mark_attempt_dispatched(&receipt).unwrap();
        let record = metered_record("prx-full-projection", Some(0.125));
        tracker.settle_metered(&record).unwrap();
        tracker.reconcile_reserved_attempt(&receipt, &record).unwrap();
        let path = tmp.path().join("state/costs.jsonl");
        let journal = tmp.path().join("state/costs.reservations.jsonl");
        let original = fs::read(&path).unwrap();
        let journal_before = fs::read(&journal).unwrap();
        let stored: CostRecord = serde_json::from_slice(&original).unwrap();
        let mut altered = Vec::new();
        let mut row = stored.clone();
        row.usage.model = "prx/other-model".into();
        altered.push(row);
        let mut row = stored.clone();
        row.usage.input_tokens += 1;
        altered.push(row);
        let mut row = stored.clone();
        row.usage.timestamp -= chrono::Duration::days(40);
        altered.push(row);
        let mut row = stored.clone();
        row.session_id = "prx-other-session".into();
        altered.push(row);
        let mut row = stored.clone();
        row.id = "prx-other-record".into();
        altered.push(row);
        for row in altered {
            assert_eq!(row.settlement_sha256, stored.settlement_sha256);
            assert_eq!(row.usage.cost_usd, stored.usage.cost_usd);
            fs::write(&path, format!("{}\n", serde_json::to_string(&row).unwrap())).unwrap();
            assert!(tracker.check_budget(0.0).is_err());
            assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
            assert!(tracker.recover_dispatched_attempt(request.clone()).is_err());
            assert_eq!(fs::read(&journal).unwrap(), journal_before);
        }
        fs::write(path, original).unwrap();
        assert!(tracker.recover_dispatched_attempt(request).is_ok());
        assert!(tracker.check_budget(0.0).is_ok());
    }

    #[test]
    fn unpriced_and_unsent_attempts_cannot_be_reconciled() {
        let tmp = TempDir::new().unwrap();
        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        let request = reservation("prx-unsent", 250_000, 1_000_000);
        let receipt = tracker.reserve_attempt(request.clone()).unwrap();
        assert!(tracker.recover_dispatched_attempt(request).is_err());
        let unknown = metered_record("prx-unsent", None);
        assert!(tracker.reconcile_reserved_attempt(&receipt, &unknown).is_err());
        let record = metered_record("prx-unsent", Some(0.125));
        tracker.settle_metered(&record).unwrap();
        assert!(tracker.reconcile_reserved_attempt(&receipt, &record).is_err());
        tracker.release_unsent_attempt(&receipt).unwrap();
    }

    #[test]
    fn reserved_budget_survives_reopen_and_sent_liability_cannot_be_released() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            daily_limit_usd: 1.0,
            monthly_limit_usd: 1.0,
            ..enabled_config()
        };
        let first = CostTracker::new(config.clone(), tmp.path()).unwrap();
        let receipt = first
            .reserve_attempt(reservation("prx-attempt-a", 750_000, 1_000_000))
            .unwrap();
        let reopened = CostTracker::new(config.clone(), tmp.path()).unwrap();
        assert!(
            reopened
                .reserve_attempt(reservation("prx-attempt-b", 500_000, 1_000_000))
                .is_err()
        );
        assert!(matches!(
            reopened.check_budget(0.5).unwrap(),
            BudgetCheck::Exceeded { .. }
        ));
        assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 250_000);
        reopened.mark_attempt_dispatched(&receipt).unwrap();
        assert!(first.mark_attempt_dispatched(&receipt).is_err());
        assert!(first.release_unsent_attempt(&receipt).is_err());
        drop(receipt);
        assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 250_000);
    }

    #[test]
    fn only_unsent_reservations_release_and_receipts_do_not_cross_workspaces() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let first = CostTracker::new(enabled_config(), a.path()).unwrap();
        let other = CostTracker::new(enabled_config(), b.path()).unwrap();
        let request = reservation("prx-attempt", 100, 200);
        let receipt = first.reserve_attempt(request.clone()).unwrap();
        let other_receipt = other.reserve_attempt(request.clone()).unwrap();
        assert!(other.mark_attempt_dispatched(&receipt).is_err());
        first.release_unsent_attempt(&receipt).unwrap();
        assert!(first.mark_attempt_dispatched(&receipt).is_err());
        assert!(first.reserve_attempt(request).is_err());
        other.release_unsent_attempt(&other_receipt).unwrap();
        assert!(first.reserve_attempt(reservation("prx-new-attempt", 200, 200)).is_ok());
        assert!(first.reserve_attempt(reservation("prx-expanded-turn", 1, 300)).is_err());
    }

    #[test]
    fn competing_trackers_cannot_reserve_the_same_remaining_balance() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            daily_limit_usd: 1.0,
            monthly_limit_usd: 1.0,
            ..enabled_config()
        };
        let first = CostTracker::new(config.clone(), tmp.path()).unwrap();
        let second = CostTracker::new(config, tmp.path()).unwrap();
        let barrier = std::sync::Barrier::new(2);
        let (a, b) = std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                barrier.wait();
                first.reserve_attempt(reservation("prx-a", 750_000, 1_000_000))
            });
            let b = scope.spawn(|| {
                barrier.wait();
                second.reserve_attempt(reservation("prx-b", 750_000, 1_000_000))
            });
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_ne!(a.is_ok(), b.is_ok());
    }

    #[test]
    fn incomplete_reservation_tail_blocks_admission_without_repair() {
        let tmp = TempDir::new().unwrap();
        let config = enabled_config();
        let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
        tracker.reserve_attempt(reservation("prx-a", 10, 20)).unwrap();
        let path = tmp.path().join("state/costs.reservations.jsonl");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"event\":").unwrap();
        file.sync_all().unwrap();
        let original = fs::read(&path).unwrap();
        assert!(tracker.check_budget(0.0).is_err());
        assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
        assert!(tracker.reserve_attempt(reservation("prx-b", 10, 20)).is_err());
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn budget_rounds_each_charge_up_and_survives_reopen() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            daily_limit_usd: 2_f64.powi(-19),
            monthly_limit_usd: 1.0,
            ..enabled_config()
        };
        let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
        for _ in 0..2 {
            let mut usage = TokenUsage::new("prx/model", 1, 0, 0.0, 0.0);
            usage.cost_usd = 2_f64.powi(-22);
            tracker.record_usage(usage).unwrap();
        }
        // Each sub-micro charge consumes one micro-USD. The binary-exact
        // daily limit allows only one, though the floating sum is below it.
        assert!(matches!(
            tracker.check_budget(0.0).unwrap(),
            BudgetCheck::Exceeded {
                period: UsagePeriod::Day,
                ..
            }
        ));
        let reopened = CostTracker::new(config.clone(), tmp.path()).unwrap();
        assert!(matches!(
            reopened.check_budget(0.0).unwrap(),
            BudgetCheck::Exceeded {
                period: UsagePeriod::Day,
                ..
            }
        ));
        assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 0);
    }

    #[test]
    fn budget_does_not_lose_small_charge_at_large_limit() {
        let limit = 2_f64.powi(53);
        let config = CostConfig {
            daily_limit_usd: limit,
            monthly_limit_usd: limit,
            ..enabled_config()
        };
        let total = ceil_micro_usd(limit).unwrap();
        assert_eq!(limit + 2_f64.powi(-22), limit);
        assert!(matches!(
            budget_check(&config, limit, limit, total, total, 2_f64.powi(-22)).unwrap(),
            BudgetCheck::Exceeded {
                period: UsagePeriod::Day,
                ..
            }
        ));
    }

    #[test]
    fn invalid_limits_refuse_admission_but_preserve_incurred_settlement() {
        for invalid in [f64::NAN, f64::INFINITY, -1.0] {
            for daily in [true, false] {
                let tmp = TempDir::new().unwrap();
                let mut config = enabled_config();
                if daily {
                    config.daily_limit_usd = invalid;
                } else {
                    config.monthly_limit_usd = invalid;
                }
                let tracker = CostTracker::new(config, tmp.path()).unwrap();
                assert!(tracker.check_budget(0.0).is_err());
                let record = metered_record("prx-invalid-limit-settlement", Some(0.125));
                assert!(tracker.settle_metered(&record).is_err());
                let reopened = CostTracker::new(enabled_config(), tmp.path()).unwrap();
                assert_eq!(reopened.get_summary().unwrap().daily_cost_usd, 0.125);
                assert!(matches!(
                    reopened.settle_metered(&record).unwrap(),
                    CostSettlement::Replayed
                ));
            }
        }
    }

    #[test]
    fn interrupted_cost_ledger_tail_blocks_reopen_and_append_without_repairing_evidence() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig::default();
        let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
        let usage = TokenUsage::new("test/model", 1, 1, 1.0, 1.0);
        tracker.record_usage(usage.clone()).unwrap();
        let path = tmp.path().join("state/costs.jsonl");
        let committed = fs::read(&path).unwrap();
        let encoded = serde_json::to_vec(&CostRecord::new("prx_session", usage.clone())).unwrap();
        for tail in [encoded.as_slice(), b"{\"usage\":".as_slice(), b" ".as_slice()] {
            let mut interrupted = committed.clone();
            interrupted.extend_from_slice(tail);
            fs::write(&path, &interrupted).unwrap();
            assert!(CostTracker::new(config.clone(), tmp.path()).is_err());
            assert!(tracker.check_budget(0.0).is_err());
            assert!(tracker.record_usage(usage.clone()).is_err());
            assert_eq!(
                fs::read(&path).unwrap(),
                interrupted,
                "never truncate or repair a possibly charged tail"
            );
        }
        // A newline is the commit delimiter, including for a blank legacy row.
        let mut complete = committed;
        complete.extend_from_slice(&encoded);
        complete.extend_from_slice(b"\n \n");
        fs::write(&path, complete).unwrap();
        let reopened = CostTracker::new(config, tmp.path()).unwrap();
        reopened.record_usage(usage).unwrap();
        assert_eq!(
            fs::read_to_string(path)
                .unwrap()
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count(),
            3
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_only_budget_snapshot_bounds_spend_without_creating_files() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            daily_limit_usd: 2.0,
            monthly_limit_usd: 3.0,
            ..Default::default()
        };
        assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 2_000_000);
        assert!(!tmp.path().join("state").exists());
        let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
        tracker
            .record_usage(TokenUsage::new("test/model", 1_000_000, 0, 1.0, 0.0))
            .unwrap();
        assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 1_000_000);
    }

    #[cfg(unix)]
    #[test]
    fn read_only_security_daily_ceiling_subtracts_prior_spend_and_rejects_bad_ledger() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            daily_limit_usd: 100.0,
            monthly_limit_usd: 100.0,
            ..Default::default()
        };
        assert_eq!(
            remaining_micro_usd_read_only_with_security_ceiling(&config, tmp.path(), 100).unwrap(),
            1_000_000
        );
        let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
        tracker
            .record_usage(TokenUsage::new("test/model", 1_000_000, 0, 0.875, 0.0))
            .unwrap();
        assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 99_125_000);
        assert_eq!(
            remaining_micro_usd_read_only_with_security_ceiling(&config, tmp.path(), 100).unwrap(),
            125_000,
            "the security daily limit must account for today's prior spend"
        );
        assert_eq!(
            remaining_micro_usd_read_only_with_security_ceiling(&config, tmp.path(), 50).unwrap(),
            0
        );
        fs::write(tmp.path().join("state/costs.jsonl"), b"malformed\n").unwrap();
        assert!(remaining_micro_usd_read_only_with_security_ceiling(&config, tmp.path(), 100).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn read_only_budget_snapshot_rejects_unsafe_file_types_and_links() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let config = CostConfig::default();
        let state = tmp.path().join("state");
        symlink(outside.path(), &state).unwrap();
        assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
        fs::remove_file(&state).unwrap();
        fs::create_dir(&state).unwrap();
        let ledger = state.join("costs.jsonl");
        let outside_file = outside.path().join("outside.jsonl");
        fs::write(&outside_file, "").unwrap();
        symlink(&outside_file, &ledger).unwrap();
        assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
        fs::remove_file(&ledger).unwrap();
        fs::hard_link(&outside_file, &ledger).unwrap();
        assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
        fs::remove_file(&ledger).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&ledger)
                .status()
                .unwrap()
                .success()
        );
        assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn read_only_budget_snapshot_rejects_unknown_or_incomplete_spend() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig::default();
        let state = tmp.path().join("state");
        fs::create_dir(&state).unwrap();
        let ledger = state.join("costs.jsonl");
        let oversized = "x".repeat(MAX_READ_ONLY_BUDGET_BYTES as usize + 1);
        for content in ["not json\n", "{}", oversized.as_str()] {
            fs::write(&ledger, content).unwrap();
            assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
        }
        fs::remove_file(&ledger).unwrap();
        fs::create_dir(tmp.path().join(".openprx")).unwrap();
        assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_ok());
        fs::write(tmp.path().join(".openprx/costs.db"), "legacy").unwrap();
        assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn read_only_budget_snapshot_rounds_each_charge_up_and_limits_down() {
        let tmp = TempDir::new().unwrap();
        let state = tmp.path().join("state");
        fs::create_dir(&state).unwrap();
        let mut ledger = File::create(state.join("costs.jsonl")).unwrap();
        for cost in [10_000_000_000.0, 0.000_000_1, 0.000_000_1] {
            let mut usage = TokenUsage::new("test/model", 0, 0, 0.0, 0.0);
            usage.cost_usd = cost;
            writeln!(
                ledger,
                "{}",
                serde_json::to_string(&CostRecord::new("prx_session", usage)).unwrap()
            )
            .unwrap();
        }
        ledger.sync_all().unwrap();
        let config = CostConfig {
            daily_limit_usd: 10_000_000_001.0,
            monthly_limit_usd: 10_000_000_000.5,
            ..Default::default()
        };
        assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 499_998);
    }

    #[test]
    fn read_only_budget_micro_conversions_use_exact_binary64_value() {
        assert_eq!(ceil_micro_usd(3e-6).unwrap(), 4);
        assert_eq!(floor_micro_usd(1e-6).unwrap(), 0);
        assert_eq!(ceil_micro_usd(f64::from_bits(1)).unwrap(), 1);
        assert_eq!(floor_micro_usd(f64::from_bits(1)).unwrap(), 0);
        assert_eq!(ceil_micro_usd(0.5).unwrap(), 500_000);
        assert_eq!(floor_micro_usd(0.5).unwrap(), 500_000);
        assert!(ceil_micro_usd(f64::MAX).is_err());
    }

    #[test]
    fn cost_tracker_initialization() {
        let tmp = TempDir::new().unwrap();
        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        assert!(!tracker.session_id().is_empty(), "expected a non-empty value");
    }

    #[test]
    fn budget_check_is_always_enforced() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig::default();

        let tracker = CostTracker::new(config, tmp.path()).unwrap();
        let check = tracker.check_budget(1000.0).unwrap();
        assert!(matches!(check, BudgetCheck::Exceeded { .. }));
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
    fn append_after_period_rollover_counts_new_record_once() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("costs.jsonl");
        let mut storage = CostStorage::new(&path).unwrap();
        let mut usage = TokenUsage::new("test/model", 1000, 0, 1.0, 1.0);
        usage.cost_usd = 0.25;
        storage
            .add_record(CostRecord::new("prx-session", usage.clone()))
            .unwrap();
        // Force a stale period without waiting for a wall-clock boundary.
        storage.cached_day = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
        storage.cached_year = 2000;
        storage.cached_month = 1;
        storage.add_record(CostRecord::new("prx-session", usage)).unwrap();
        let cached = (storage.daily_cost_usd.to_bits(), storage.monthly_cost_usd.to_bits());
        storage
            .rebuild_aggregates(storage.cached_day, storage.cached_year, storage.cached_month)
            .unwrap();
        assert_eq!(
            cached,
            (storage.daily_cost_usd.to_bits(), storage.monthly_cost_usd.to_bits())
        );
        assert_eq!(fs::read_to_string(path).unwrap().lines().count(), 2);
    }

    #[test]
    fn malformed_ledger_rejects_reopen_and_existing_tracker_without_writing() {
        let tmp = TempDir::new().unwrap();
        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        let usage = TokenUsage::new("test/model", 1000, 0, 1.0, 1.0);
        tracker.record_usage(usage.clone()).unwrap();
        let storage_path = resolve_storage_path(tmp.path()).unwrap();
        let mut file = OpenOptions::new().append(true).open(&storage_path).unwrap();
        writeln!(file, "not-a-json-line").unwrap();
        file.sync_all().unwrap();
        let before = fs::read(&storage_path).unwrap();

        assert!(CostTracker::new(enabled_config(), tmp.path()).is_err());
        assert!(tracker.check_budget(0.0).is_err());
        assert!(tracker.record_usage(usage).is_err());
        assert!(
            tracker
                .settle_metered(&metered_record("prx-settlement", Some(0.001)))
                .is_err()
        );
        assert!(tracker.get_summary().is_err());
        assert_eq!(fs::read(storage_path).unwrap(), before);
    }

    #[test]
    fn invalid_persisted_amount_and_aggregate_overflow_reject_budget_refresh() {
        for amounts in [vec![-1.0], vec![f64::MAX, f64::MAX]] {
            let tmp = TempDir::new().unwrap();
            let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
            let storage_path = resolve_storage_path(tmp.path()).unwrap();
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&storage_path)
                .unwrap();
            for amount in amounts {
                let mut usage = TokenUsage::new("test/model", 1, 0, 1.0, 1.0);
                usage.cost_usd = amount;
                let record = CostRecord::new("prx-session", usage);
                writeln!(file, "{}", serde_json::to_string(&record).unwrap()).unwrap();
            }
            file.sync_all().unwrap();
            let before = fs::read(&storage_path).unwrap();
            assert!(tracker.check_budget(0.0).is_err());
            assert!(CostTracker::new(enabled_config(), tmp.path()).is_err());
            assert_eq!(fs::read(storage_path).unwrap(), before);
        }
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
    fn reopened_settlement_rejects_changed_charge_or_usage_without_writing() {
        let tmp = TempDir::new().unwrap();
        let first = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        let record = metered_record("prx-settlement", Some(0.002));
        first.settle_metered(&record).unwrap();
        let path = first.lock_storage().path.clone();
        let before = fs::read(&path).unwrap();
        drop(first);

        let reopened = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        assert_eq!(reopened.settle_metered(&record).unwrap(), CostSettlement::Replayed);
        let mut changed_cost = record.clone();
        changed_cost.cost_usd = Some(0.004);
        let mut changed_model = record.clone();
        changed_model.model = "prx-other-model".to_owned();
        let mut changed_cache = record;
        changed_cache.cache_read_input_tokens = 1;
        for changed in [changed_cost, changed_model, changed_cache] {
            assert!(
                reopened
                    .settle_metered(&changed)
                    .unwrap_err()
                    .to_string()
                    .contains("conflicts")
            );
        }
        assert_eq!(fs::read(path).unwrap(), before);
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

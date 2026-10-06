//! Configuration hot-reload infrastructure.
//!
//! Provides the shared process-level [`ConfigGenerationManager`] handle and a
//! file watcher that routes every reload through that sole owner.
//!
//! # Design
//!
//! - Readers call `.load_full()` for an immutable `Arc<Config>` or `.pin()` for
//!   the complete generation.
//! - The watcher runs a `notify` debouncer and calls the generation manager's
//!   single reload entry point.
//! - On parse failure the old config is kept and a warning is logged.
//! - A monotonic `reload_version` counter is bumped only when file content
//!   changes and reload succeeds.

use super::{
    files,
    generation::{ConfigGenerationManager, ConfigReloadTrigger},
    schema::Config,
};
use crate::config::files::compute_config_fingerprint_gated;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

/// Shared read handle and sole runtime configuration publisher.
pub type SharedConfig = Arc<ConfigGenerationManager>;

/// Create a new [`SharedConfig`] pre-loaded with `initial`.
pub fn new_shared(initial: Config) -> SharedConfig {
    Arc::new(ConfigGenerationManager::new(initial))
}

/// Watches `config.toml` and submits stable candidates to [`SharedConfig`].
pub struct HotReloadManager {
    /// Dedicated watcher thread. `None` only if the OS refused to create it,
    /// in which case hot-reload is inactive and the failure was logged.
    _handle: Option<std::thread::JoinHandle<()>>,
    reload_version: Arc<AtomicU64>,
    ready: Arc<AtomicBool>,
}

impl HotReloadManager {
    /// Spawn the file watcher task.
    ///
    /// `config_path` must be the path to `config.toml`.
    /// `shared` is the sole process generation owner.
    pub fn spawn(config_path: PathBuf, shared: SharedConfig) -> Self {
        let reload_version = Arc::new(AtomicU64::new(0));
        let rv_clone = Arc::clone(&reload_version);
        let ready = Arc::new(AtomicBool::new(false));
        let ready_clone = Arc::clone(&ready);
        // The watcher thread settles the CLI's deferred changes on this
        // runtime (history, compensation), blocking only itself.
        let runtime = tokio::runtime::Handle::try_current().ok();

        // Dedicated OS thread, not the tokio blocking pool: the watcher blocks
        // on `notify` events until process exit, so a pool slot handed to it is
        // never returned. With no ceiling on concurrent agent work, permanently
        // retired slots are exactly what drives the pool into deadlock.
        let handle = match crate::runtime::blocking::spawn_detached_thread("prx-config-watch", move || {
            if let Err(e) = run_watcher(config_path, shared, rv_clone, ready_clone, runtime) {
                tracing::error!("Config hot-reload watcher exited: {e}");
            }
        }) {
            Ok(handle) => Some(handle),
            Err(error) => {
                tracing::error!("failed to start config hot-reload watcher thread: {error}");
                None
            }
        };

        Self {
            _handle: handle,
            reload_version,
            ready,
        }
    }

    /// How many successful reloads have occurred since startup.
    pub fn reload_version(&self) -> u64 {
        self.reload_version.load(Ordering::Relaxed)
    }

    /// Whether the OS watcher is installed and its initial content fingerprint
    /// has been captured. Callers may use this as a startup/readiness signal.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

// ── watcher implementation ────────────────────────────────────────────────────

/// How often, and how many times, an outcome whose deferred record is not
/// recorded yet is settled again (the CLI records its commit right after
/// writing, so a few seconds cover it).
const SETTLE_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
const SETTLE_RETRIES: u32 = 10;

/// An outcome the watcher still has to settle, and how often it tried.
struct PendingSettlement {
    seen: crate::config::manage::WatcherActivation,
    attempts: u32,
}

/// Settle the CLI's `activation_deferred` record for the content this
/// process evaluated (D-085 F-4, D-101 F-1). Returns the outcome again when
/// no record names that content yet, so the caller retries. Without a
/// runtime the record stays deferred and a warning says so.
fn settle_deferred(
    shared: &SharedConfig,
    runtime: Option<&tokio::runtime::Handle>,
    seen: crate::config::manage::WatcherActivation,
) -> Option<crate::config::manage::WatcherActivation> {
    use crate::config::manage::Settlement;
    let Some(runtime) = runtime else {
        tracing::warn!("No async runtime for the configuration watcher; a CLI change stays activation_deferred");
        return None;
    };
    let transactions = crate::config::manage::ConfigTransactions::new(Arc::clone(shared));
    match runtime.block_on(transactions.settle_deferred_activation(seen.clone())) {
        Settlement::Settled(status) => {
            tracing::info!(
                status = status.as_str(),
                "Settled a configuration change the CLI deferred"
            );
            None
        }
        Settlement::NotYet => Some(seen),
        Settlement::Skipped => None,
    }
}

/// The outcome of a reload of the files, bound to the revision the reload
/// evaluated: the loaded revision when it succeeded, the refused candidate's
/// when it failed, and, when the files did not even load (no candidate
/// revision was recorded), the fingerprint the watcher read before the
/// reload, so such a refusal settles too (D-104 C-3).
fn watcher_outcome(
    shared: &SharedConfig,
    reloaded: &anyhow::Result<super::generation::ConfigApplyReport>,
    content_hash: &[u8],
) -> Option<crate::config::manage::WatcherActivation> {
    use crate::config::manage::{WatcherActivation, WatcherOutcome};
    match reloaded {
        Ok(report) => Some(WatcherActivation {
            revision: report.desired_source_revision.fingerprint_sha256.clone(),
            outcome: WatcherOutcome::Active {
                restart_required: report.restart_required.clone(),
            },
        }),
        Err(error) => Some(WatcherActivation {
            revision: shared
                .status()
                .last_failure
                .and_then(|failure| failure.desired_source_revision)
                .map_or_else(
                    || files::revision_of_fingerprint(content_hash),
                    |revision| revision.fingerprint_sha256,
                ),
            outcome: WatcherOutcome::Refused {
                reason: format!("{error:#}"),
            },
        }),
    }
}

/// An unchanged disk image is already active. A CLI write may still have
/// created a deferred history record for that image during the debounce.
fn unchanged_active_outcome(
    shared: &SharedConfig,
    content_hash: &[u8],
) -> Option<crate::config::manage::WatcherActivation> {
    let active = shared.pin();
    (active.source_revision.fingerprint_sha256 == files::revision_of_fingerprint(content_hash)).then(|| {
        crate::config::manage::WatcherActivation {
            revision: active.source_revision.fingerprint_sha256.clone(),
            outcome: crate::config::manage::WatcherOutcome::Active {
                restart_required: Vec::new(),
            },
        }
    })
}

/// Settle a CLI write that returned to the already active bytes. Keep a
/// pending older outcome when the disk does not match the active generation.
fn settle_unchanged_event(
    shared: &SharedConfig,
    runtime: Option<&tokio::runtime::Handle>,
    content_hash: &[u8],
    pending: &mut Option<PendingSettlement>,
) {
    if let Some(seen) = unchanged_active_outcome(shared, content_hash) {
        *pending = settle_deferred(shared, runtime, seen).map(|seen| PendingSettlement { seen, attempts: 0 });
    }
}

fn run_watcher(
    config_path: PathBuf,
    shared: SharedConfig,
    reload_version: Arc<AtomicU64>,
    ready: Arc<AtomicBool>,
    runtime: Option<tokio::runtime::Handle>,
) -> anyhow::Result<()> {
    use notify::RecursiveMode;
    use notify_debouncer_mini::{DebounceEventResult, new_debouncer};

    let (tx, rx) = std::sync::mpsc::channel::<DebounceEventResult>();
    let debounce_ms = std::time::Duration::from_secs(1);
    let watch_root = config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut debouncer = new_debouncer(debounce_ms, tx)?;
    debouncer.watcher().watch(&watch_root, RecursiveMode::Recursive)?;

    let mut last_content_hash = compute_config_fingerprint_gated(&config_path).ok();
    // A change the CLI wrote while no PRX ran was activated by this
    // process's start, when the active generation is those files.
    let started = crate::config::manage::WatcherActivation {
        revision: shared.pin().source_revision.fingerprint_sha256.clone(),
        outcome: crate::config::manage::WatcherOutcome::Active {
            restart_required: Vec::new(),
        },
    };
    let mut pending =
        settle_deferred(&shared, runtime.as_ref(), started).map(|seen| PendingSettlement { seen, attempts: 0 });
    ready.store(true, Ordering::Release);

    tracing::info!(
        path = %config_path.display(),
        watch_root = %watch_root.display(),
        "Config hot-reload watcher started (1 s debounce)"
    );

    loop {
        let result = match rx.recv_timeout(SETTLE_RETRY_INTERVAL) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Retry an outcome whose record was not recorded yet.
                if let Some(mut waiting) = pending.take() {
                    waiting.attempts += 1;
                    if let Some(seen) = settle_deferred(&shared, runtime.as_ref(), waiting.seen) {
                        if waiting.attempts < SETTLE_RETRIES {
                            pending = Some(PendingSettlement {
                                seen,
                                attempts: waiting.attempts,
                            });
                        } else {
                            tracing::debug!("No deferred configuration record names the evaluated revision");
                        }
                    }
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match result {
            Ok(events) => {
                let relevant = events
                    .iter()
                    .any(|ev| files::is_relevant_config_path(&config_path, &ev.path));

                if !relevant {
                    continue;
                }

                let content_hash = match compute_config_fingerprint_gated(&config_path) {
                    Ok(hash) => hash,
                    Err(e) => {
                        tracing::warn!(
                            path = %config_path.display(),
                            error = %e,
                            "⚠️  Failed to read config for hot-reload"
                        );
                        continue;
                    }
                };

                if last_content_hash.as_ref().is_some_and(|h| h == &content_hash) {
                    // Two CLI writes can coalesce inside the debounce window,
                    // with the last write restoring the active bytes. No
                    // reload is needed, but its deferred record still needs
                    // an active outcome and older records need superseding.
                    settle_unchanged_event(&shared, runtime.as_ref(), &content_hash, &mut pending);
                    tracing::debug!(
                        path = %config_path.display(),
                        "Config reload skipped (content unchanged)"
                    );
                    continue;
                }

                let previous = shared.load_full();
                let reloaded = shared.reload_from_disk(ConfigReloadTrigger::FileWatcher);
                // A new outcome replaces one still waiting: it is about newer
                // content, and the older record is superseded by it.
                pending = watcher_outcome(&shared, &reloaded, &content_hash)
                    .and_then(|seen| settle_deferred(&shared, runtime.as_ref(), seen))
                    .map(|seen| PendingSettlement { seen, attempts: 0 });
                match reloaded {
                    Ok(report) => {
                        log_diff(&previous, &shared.load_full());
                        last_content_hash = Some(content_hash);
                        let version = reload_version.fetch_add(1, Ordering::Relaxed) + 1;
                        tracing::info!(
                            path = %config_path.display(),
                            version,
                            active_generation = report.active_generation.0,
                            status = report.status(),
                            restart_required = ?report.restart_required,
                            "Config hot-reloaded (version {version})"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %config_path.display(),
                            error = %e,
                            "⚠️  Config reload failed — keeping previous config"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Config watcher error: {e}");
            }
        }
    }

    ready.store(false, Ordering::Release);
    Ok(())
}

fn log_diff(old: &Config, fresh: &Config) {
    let mut changes: Vec<String> = Vec::new();

    if (old.default_temperature - fresh.default_temperature).abs() > 1e-9 {
        changes.push(format!(
            "temperature: {:.2} → {:.2}",
            old.default_temperature, fresh.default_temperature
        ));
    }
    if old.agent.read_only_tool_concurrency_window != fresh.agent.read_only_tool_concurrency_window {
        changes.push(format!(
            "agent.read_only_tool_concurrency_window: {} → {}",
            old.agent.read_only_tool_concurrency_window, fresh.agent.read_only_tool_concurrency_window
        ));
    }
    if old.agent.priority_scheduling_enabled != fresh.agent.priority_scheduling_enabled {
        changes.push(format!(
            "agent.priority_scheduling_enabled: {} → {}",
            old.agent.priority_scheduling_enabled, fresh.agent.priority_scheduling_enabled
        ));
    }
    if old.agent.low_priority_tools != fresh.agent.low_priority_tools {
        changes.push(format!(
            "agent.low_priority_tools: {:?} → {:?}",
            old.agent.low_priority_tools, fresh.agent.low_priority_tools
        ));
    }

    if changes.is_empty() {
        tracing::debug!("Config file changed but no key tracked fields differ");
    } else {
        tracing::info!("Config diff:");
        for change in &changes {
            tracing::info!("  • {change}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::manage::{
        ApplyOptions, ConfigActor, ConfigChange, ConfigHistory, ConfigTransactions, RecordStatus,
    };
    use crate::config::schema::Config;

    #[tokio::test]
    async fn returning_to_active_bytes_settles_both_cli_records_without_reload() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_path = dir.path().join("config.toml");
        let original = "default_temperature = 0.7\n";
        std::fs::write(&config_path, original).expect("test: write");
        let workspace = dir.path().join("workspace");
        let initial = Config::load_from_path(&config_path, workspace.clone()).expect("test: load");
        let shared = new_shared(initial.clone());
        let active_generation = shared.active_generation_id();

        let cli1 = ConfigTransactions::new(new_shared(initial)).deferring_activation();
        let first = cli1
            .apply(
                cli1.plan(ConfigChange::RawFile {
                    filename: "config.toml".to_string(),
                    contents: "default_temperature = 0.4\n".to_string(),
                })
                .await
                .expect("test: first plan"),
                ConfigActor::Cli,
                ApplyOptions::default(),
            )
            .await
            .expect("test: first write")
            .record_id
            .expect("test: first record");

        let second_loaded = Config::load_from_path(&config_path, workspace).expect("test: load first write");
        let cli2 = ConfigTransactions::new(new_shared(second_loaded)).deferring_activation();
        let second = cli2
            .apply(
                cli2.plan(ConfigChange::RawFile {
                    filename: "config.toml".to_string(),
                    contents: original.to_string(),
                })
                .await
                .expect("test: second plan"),
                ConfigActor::Cli,
                ApplyOptions::default(),
            )
            .await
            .expect("test: second write")
            .record_id
            .expect("test: second record");

        assert_eq!(std::fs::read_to_string(&config_path).expect("test: read"), original);
        let history = ConfigHistory::for_config(&config_path).expect("test: history");
        assert_eq!(
            history.get(&first).expect("test: first record").status,
            RecordStatus::ActivationDeferred
        );
        assert_eq!(
            history.get(&second).expect("test: second record").status,
            RecordStatus::ActivationDeferred
        );
        let hash = compute_config_fingerprint_gated(&config_path).expect("test: fingerprint");
        let runtime = tokio::runtime::Handle::current();
        let watcher_shared = Arc::clone(&shared);
        let retry = tokio::task::spawn_blocking(move || {
            let mut pending = None;
            settle_unchanged_event(&watcher_shared, Some(&runtime), &hash, &mut pending);
            pending.is_some()
        })
        .await
        .expect("test: watcher thread");

        assert!(!retry, "test: both records settle at once");
        assert_eq!(
            history.get(&first).expect("test: first record").status,
            RecordStatus::Superseded
        );
        assert_eq!(
            history.get(&second).expect("test: second record").status,
            RecordStatus::Activated
        );
        assert_eq!(shared.active_generation_id(), active_generation, "test: no reload");
        assert_eq!(std::fs::read_to_string(&config_path).expect("test: read"), original);
    }

    #[test]
    fn unchanged_bytes_can_confirm_only_the_active_revision() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "default_temperature = 0.7\n").expect("test: write");
        let shared =
            new_shared(Config::load_from_path(&config_path, dir.path().join("workspace")).expect("test: load"));
        let active_hash = compute_config_fingerprint_gated(&config_path).expect("test: fingerprint");
        assert!(unchanged_active_outcome(&shared, &active_hash).is_some());

        std::fs::write(&config_path, "default_temperature = 0.4\n").expect("test: change disk");
        let changed_hash = compute_config_fingerprint_gated(&config_path).expect("test: fingerprint");
        assert!(unchanged_active_outcome(&shared, &changed_hash).is_none());
    }

    /// RED before PMMH-g (D-104 C-3): when the daemon could not even load
    /// the files a CLI wrote, no candidate revision was recorded, so the
    /// refusal was never bound to content and the record never settled. It
    /// is bound to the fingerprint the watcher read now.
    #[test]
    fn a_reload_that_cannot_load_the_files_is_bound_to_the_watchers_fingerprint() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "default_temperature = 0.7\n").expect("test: write");
        let shared = new_shared(Config {
            config_path: config_path.clone(),
            workspace_dir: dir.path().join("workspace"),
            ..Config::default()
        });
        // Valid TOML the loader refuses (the watcher fingerprints only TOML
        // that parses): a type error, as a newer or older release could write.
        std::fs::write(&config_path, "default_temperature = \"hot\"\n").expect("test: write a file that does not load");
        let content_hash = compute_config_fingerprint_gated(&config_path).expect("test: fingerprint");

        let reloaded = shared.reload_from_disk(ConfigReloadTrigger::Test);
        assert!(reloaded.is_err(), "test precondition: the files do not load");
        let seen = watcher_outcome(&shared, &reloaded, &content_hash).expect("the refusal is bound to content");

        assert_eq!(
            seen.revision,
            files::current_config_revision(&config_path).expect("test: revision")
        );
        assert!(matches!(
            seen.outcome,
            crate::config::manage::WatcherOutcome::Refused { .. }
        ));
    }

    #[test]
    fn new_shared_creates_generation_manager() {
        let cfg = Config::default();
        let shared = new_shared(cfg);
        let loaded = shared.load_full();
        assert!(loaded.default_temperature > 0.0);
    }

    #[test]
    fn manager_replaces_runtime_config() {
        let shared = new_shared(Config::default());
        let mut new_cfg = Config::default();
        new_cfg.default_temperature = 0.42;
        shared
            .apply_runtime_config(new_cfg, ConfigReloadTrigger::Test)
            .expect("apply runtime config");
        assert!((shared.load_full().default_temperature - 0.42).abs() < 1e-9);
    }

    #[test]
    fn load_full_returns_arc_config() {
        let shared = new_shared(Config::default());
        let cfg: Arc<Config> = shared.load_full();
        // Arc<Config> derefs to Config
        assert!(cfg.default_temperature > 0.0);
    }
}

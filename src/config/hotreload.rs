//! Configuration hot-reload infrastructure.
//!
//! Provides [`SharedConfig`] — a lock-free, atomically-swappable config handle —
//! and [`HotReloadManager`] which watches the config file for changes and
//! atomically replaces the stored [`Config`] on success.
//!
//! # Design
//!
//! - `SharedConfig = Arc<ArcSwap<Config>>` — readers call `.load_full()` for a
//!   snapshot `Arc<Config>` with no locks, no contention.
//! - The manager spawns a Tokio task that runs a `notify` debouncer (1 s
//!   window). On each confirmed write it parses the file and, if valid, calls
//!   `.store()` to atomically publish the new config.
//! - On parse failure the old config is kept and a warning is logged.
//! - A monotonic `reload_version` counter is bumped only when file content
//!   changes and reload succeeds.

use super::{files, schema::Config};
use arc_swap::ArcSwap;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

/// Lock-free, hot-swappable configuration handle.
///
/// Clone cheaply — all clones point to the same `ArcSwap` cell.
pub type SharedConfig = Arc<ArcSwap<Config>>;

/// Create a new [`SharedConfig`] pre-loaded with `initial`.
pub fn new_shared(initial: Config) -> SharedConfig {
    Arc::new(ArcSwap::from_pointee(initial))
}

/// Watches `config.toml` and atomically swaps [`SharedConfig`] on change.
pub struct HotReloadManager {
    _handle: tokio::task::JoinHandle<()>,
    reload_version: Arc<AtomicU64>,
}

impl HotReloadManager {
    /// Spawn the file watcher task.
    ///
    /// `config_path` must be the path to `config.toml`.
    /// `shared` is the live config handle — the manager will call `.store()` on
    /// every successful reload.
    pub fn spawn(config_path: PathBuf, shared: SharedConfig) -> Self {
        let reload_version = Arc::new(AtomicU64::new(0));
        let rv_clone = Arc::clone(&reload_version);

        let handle = tokio::task::spawn_blocking(move || {
            if let Err(e) = run_watcher(config_path, shared, rv_clone) {
                tracing::error!("Config hot-reload watcher exited: {e}");
            }
        });

        Self {
            _handle: handle,
            reload_version,
        }
    }

    /// How many successful reloads have occurred since startup.
    pub fn reload_version(&self) -> u64 {
        self.reload_version.load(Ordering::Relaxed)
    }
}

// ── watcher implementation ────────────────────────────────────────────────────

fn run_watcher(
    config_path: PathBuf,
    shared: SharedConfig,
    reload_version: Arc<AtomicU64>,
) -> anyhow::Result<()> {
    use notify::RecursiveMode;
    use notify_debouncer_mini::{new_debouncer, DebounceEventResult};

    let (tx, rx) = std::sync::mpsc::channel::<DebounceEventResult>();
    let debounce_ms = std::time::Duration::from_secs(1);
    let watch_root = config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut debouncer = new_debouncer(debounce_ms, tx)?;
    debouncer
        .watcher()
        .watch(&watch_root, RecursiveMode::Recursive)?;

    let mut last_content_hash = files::compute_config_fingerprint(&config_path).ok();

    tracing::info!(
        path = %config_path.display(),
        watch_root = %watch_root.display(),
        "Config hot-reload watcher started (1 s debounce)"
    );

    for result in rx {
        match result {
            Ok(events) => {
                let relevant = events
                    .iter()
                    .any(|ev| files::is_relevant_config_path(&config_path, &ev.path));

                if !relevant {
                    continue;
                }

                let content_hash = match files::compute_config_fingerprint(&config_path) {
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

                if last_content_hash
                    .as_ref()
                    .is_some_and(|h| h == &content_hash)
                {
                    tracing::debug!(
                        path = %config_path.display(),
                        "Config watcher event ignored (content unchanged)"
                    );
                    continue;
                }

                match try_reload(&config_path, &shared) {
                    Ok(()) => {
                        last_content_hash = Some(content_hash);
                        let version = reload_version.fetch_add(1, Ordering::Relaxed) + 1;
                        tracing::info!(
                            path = %config_path.display(),
                            version,
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

    Ok(())
}

/// Parse config file and atomically store it.
fn try_reload(config_path: &std::path::Path, shared: &SharedConfig) -> anyhow::Result<()> {
    let old = shared.load_full();
    let mut fresh = load_stable_config_snapshot(config_path, old.workspace_dir.clone())?;
    // ACL mode is not hot-reloadable for already-initialized tool instances.
    // Keep runtime behavior deterministic until restart.
    if fresh.memory.acl_enabled != old.memory.acl_enabled {
        tracing::warn!(
            old = old.memory.acl_enabled,
            new = fresh.memory.acl_enabled,
            "memory.acl_enabled change detected but requires restart; keeping current runtime value"
        );
        fresh.memory.acl_enabled = old.memory.acl_enabled;
    }

    // Log diff of key hot-reloadable fields
    log_diff(&old, &fresh);

    // Atomically publish new config
    shared.store(Arc::new(fresh));
    Ok(())
}

fn load_stable_config_snapshot(
    config_path: &std::path::Path,
    workspace_dir: PathBuf,
) -> anyhow::Result<Config> {
    const MAX_ATTEMPTS: usize = 3;

    for attempt in 1..=MAX_ATTEMPTS {
        let before = files::compute_config_fingerprint(config_path)?;
        let fresh = Config::load_from_path(config_path, workspace_dir.clone())?;
        let after = files::compute_config_fingerprint(config_path)?;
        if before == after {
            return Ok(fresh);
        }

        tracing::debug!(
            path = %config_path.display(),
            attempt,
            "Config changed during hot-reload snapshot; retrying"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    anyhow::bail!(
        "Config changed repeatedly while reloading: {}",
        config_path.display()
    );
}

fn log_diff(old: &Config, fresh: &Config) {
    let mut changes: Vec<String> = Vec::new();

    if (old.default_temperature - fresh.default_temperature).abs() > 1e-9 {
        changes.push(format!(
            "temperature: {:.2} → {:.2}",
            old.default_temperature, fresh.default_temperature
        ));
    }
    if old.agent.max_tool_iterations != fresh.agent.max_tool_iterations {
        changes.push(format!(
            "agent.max_tool_iterations: {} → {}",
            old.agent.max_tool_iterations, fresh.agent.max_tool_iterations
        ));
    }
    if old.agent.max_history_messages != fresh.agent.max_history_messages {
        changes.push(format!(
            "agent.max_history_messages: {} → {}",
            old.agent.max_history_messages, fresh.agent.max_history_messages
        ));
    }
    if old.heartbeat.enabled != fresh.heartbeat.enabled {
        changes.push(format!(
            "heartbeat.enabled: {} → {}",
            old.heartbeat.enabled, fresh.heartbeat.enabled
        ));
    }
    if old.cron.enabled != fresh.cron.enabled {
        changes.push(format!(
            "cron.enabled: {} → {}",
            old.cron.enabled, fresh.cron.enabled
        ));
    }
    if old.web_search.enabled != fresh.web_search.enabled {
        changes.push(format!(
            "web_search.enabled: {} → {}",
            old.web_search.enabled, fresh.web_search.enabled
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

    // Warn about fields that require a restart to take effect
    if old.default_provider != fresh.default_provider || old.default_model != fresh.default_model {
        tracing::warn!(
            "default_provider/default_model changed in config — \
             restart the daemon to apply (TODO P3: live provider rebuild)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::Config;

    #[test]
    fn new_shared_creates_arc_arcswap() {
        let cfg = Config::default();
        let shared = new_shared(cfg);
        let loaded = shared.load_full();
        assert!(loaded.default_temperature > 0.0);
    }

    #[test]
    fn store_replaces_config() {
        let shared = new_shared(Config::default());
        let mut new_cfg = Config::default();
        new_cfg.default_temperature = 0.42;
        shared.store(Arc::new(new_cfg));
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

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
        atomic::{AtomicU64, Ordering},
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
    _handle: tokio::task::JoinHandle<()>,
    reload_version: Arc<AtomicU64>,
}

impl HotReloadManager {
    /// Spawn the file watcher task.
    ///
    /// `config_path` must be the path to `config.toml`.
    /// `shared` is the sole process generation owner.
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

fn run_watcher(config_path: PathBuf, shared: SharedConfig, reload_version: Arc<AtomicU64>) -> anyhow::Result<()> {
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
                    tracing::debug!(
                        path = %config_path.display(),
                        "Config watcher event ignored (content unchanged)"
                    );
                    continue;
                }

                let previous = shared.load_full();
                match shared.reload_from_disk(ConfigReloadTrigger::FileWatcher) {
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
    if old.agent.read_only_tool_concurrency_window != fresh.agent.read_only_tool_concurrency_window {
        changes.push(format!(
            "agent.read_only_tool_concurrency_window: {} → {}",
            old.agent.read_only_tool_concurrency_window, fresh.agent.read_only_tool_concurrency_window
        ));
    }
    if old.agent.read_only_tool_timeout_secs != fresh.agent.read_only_tool_timeout_secs {
        changes.push(format!(
            "agent.read_only_tool_timeout_secs: {} → {}",
            old.agent.read_only_tool_timeout_secs, fresh.agent.read_only_tool_timeout_secs
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
    if old.agent.concurrency_kill_switch_force_serial != fresh.agent.concurrency_kill_switch_force_serial {
        changes.push(format!(
            "agent.concurrency_kill_switch_force_serial: {} → {}",
            old.agent.concurrency_kill_switch_force_serial, fresh.agent.concurrency_kill_switch_force_serial
        ));
    }
    if old.agent.concurrency_rollout_stage != fresh.agent.concurrency_rollout_stage {
        changes.push(format!(
            "agent.concurrency_rollout_stage: {} → {}",
            old.agent.concurrency_rollout_stage, fresh.agent.concurrency_rollout_stage
        ));
    }
    if old.agent.concurrency_rollout_sample_percent != fresh.agent.concurrency_rollout_sample_percent {
        changes.push(format!(
            "agent.concurrency_rollout_sample_percent: {} → {}",
            old.agent.concurrency_rollout_sample_percent, fresh.agent.concurrency_rollout_sample_percent
        ));
    }
    if old.agent.concurrency_rollout_channels != fresh.agent.concurrency_rollout_channels {
        changes.push(format!(
            "agent.concurrency_rollout_channels: {:?} → {:?}",
            old.agent.concurrency_rollout_channels, fresh.agent.concurrency_rollout_channels
        ));
    }
    if old.agent.concurrency_auto_rollback_enabled != fresh.agent.concurrency_auto_rollback_enabled {
        changes.push(format!(
            "agent.concurrency_auto_rollback_enabled: {} → {}",
            old.agent.concurrency_auto_rollback_enabled, fresh.agent.concurrency_auto_rollback_enabled
        ));
    }
    if (old.agent.concurrency_rollback_timeout_rate_threshold - fresh.agent.concurrency_rollback_timeout_rate_threshold)
        .abs()
        > f64::EPSILON
    {
        changes.push(format!(
            "agent.concurrency_rollback_timeout_rate_threshold: {:.3} → {:.3}",
            old.agent.concurrency_rollback_timeout_rate_threshold,
            fresh.agent.concurrency_rollback_timeout_rate_threshold
        ));
    }
    if (old.agent.concurrency_rollback_cancel_rate_threshold - fresh.agent.concurrency_rollback_cancel_rate_threshold)
        .abs()
        > f64::EPSILON
    {
        changes.push(format!(
            "agent.concurrency_rollback_cancel_rate_threshold: {:.3} → {:.3}",
            old.agent.concurrency_rollback_cancel_rate_threshold,
            fresh.agent.concurrency_rollback_cancel_rate_threshold
        ));
    }
    if (old.agent.concurrency_rollback_error_rate_threshold - fresh.agent.concurrency_rollback_error_rate_threshold)
        .abs()
        > f64::EPSILON
    {
        changes.push(format!(
            "agent.concurrency_rollback_error_rate_threshold: {:.3} → {:.3}",
            old.agent.concurrency_rollback_error_rate_threshold, fresh.agent.concurrency_rollback_error_rate_threshold
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
    use crate::config::schema::Config;

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

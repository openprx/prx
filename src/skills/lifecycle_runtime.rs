//! Wires the skill lifecycle service into the process (SL09 part B): building
//! it from `[skills_lifecycle]`, and the recovery sweep at daemon start.
//!
//! A disabled lifecycle builds nothing and opens no store. Startup recovery is
//! the only lifecycle work that runs by itself; it repairs and never starts a
//! proposal, an evaluation or a publication.

use crate::config::Config;
use crate::config::skills_lifecycle::{SkillsLifecycleConfig, SkillsLifecycleConfigError};
use crate::skills::lifecycle::{
    LifecycleError, ServiceError, SkillLifecycleStore, SkillsLifecycleService, StartupReport,
};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

/// Why the lifecycle service could not be built.
#[derive(Debug, Error)]
pub enum LifecycleSetupError {
    #[error(transparent)]
    Config(#[from] SkillsLifecycleConfigError),
    #[error("the skill lifecycle store could not be opened: {0}")]
    Store(#[from] LifecycleError),
    #[error(transparent)]
    Service(#[from] ServiceError),
}

/// Bounded retry of a call that can find the store locked by another process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BusyRetry {
    /// Total calls, the first included.
    pub attempts: u32,
    /// Wait before the second call; it doubles for each one after.
    pub base_delay: Duration,
}

/// Startup recovery: 5 calls, waiting 250 ms, 500 ms, 1 s and 2 s between them.
pub const STARTUP_BUSY_RETRY: BusyRetry = BusyRetry {
    attempts: 5,
    base_delay: Duration::from_millis(250),
};

/// The services built so far, one per workspace and configuration, so every
/// tool-registry build and the daemon hook of a process share one service.
/// The key is the workspace plus the debug form of the section, so a different
/// configuration (tests, a restart-free reload that was refused) never gets
/// another one's service. Bounded by the handful of distinct keys a process has.
type ServiceKey = (std::path::PathBuf, String);
static SERVICES: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<ServiceKey, Arc<SkillsLifecycleService>>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// Builds, or returns the process's shared, lifecycle service for an
/// installation. `Ok(None)` when the lifecycle is disabled: nothing is built
/// and no store is opened. The store is opened (and created) only when it is
/// enabled.
pub fn open_service(
    workspace_dir: &Path,
    config: &SkillsLifecycleConfig,
) -> Result<Option<Arc<SkillsLifecycleService>>, LifecycleSetupError> {
    let settings = config.to_settings()?;
    if !settings.enabled {
        return Ok(None);
    }
    let key: ServiceKey = (workspace_dir.to_path_buf(), format!("{config:?}"));
    if let Some(shared) = SERVICES.lock().get(&key) {
        return Ok(Some(Arc::clone(shared)));
    }
    let owner = config.owner()?.ok_or(SkillsLifecycleConfigError::OwnerScopeMissing)?;
    let store = SkillLifecycleStore::open(workspace_dir, owner)?;
    let service = Arc::new(SkillsLifecycleService::new(Arc::new(store), settings)?);
    // A concurrent builder may have won; keep the first one.
    let shared = Arc::clone(SERVICES.lock().entry(key).or_insert(service));
    Ok(Some(shared))
}

/// Whether a lifecycle service of an enabled `[skills_lifecycle]` section was
/// built in this process for `workspace_dir`. Nothing is recorded for a
/// workspace without one: a disabled lifecycle (or one never configured) must
/// not leave experiences behind, even when an old store directory exists.
#[must_use]
pub fn lifecycle_enabled_for(workspace_dir: &Path) -> bool {
    SERVICES.lock().keys().any(|(workspace, _)| workspace == workspace_dir)
}

/// Runs `call` again while it reports a store locked by another process
/// ([`ServiceError::Busy`]), up to `retry.attempts` calls with a doubling wait.
/// Any other result, and the last `Busy`, is returned.
pub async fn retry_busy<T, F, Fut>(retry: BusyRetry, mut call: F) -> Result<T, ServiceError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ServiceError>>,
{
    let attempts = retry.attempts.max(1);
    let mut delay = retry.base_delay;
    let mut attempt = 1_u32;
    loop {
        match call().await {
            Err(ServiceError::Busy { path }) if attempt < attempts => {
                tracing::warn!(
                    %path,
                    attempt,
                    attempts,
                    retry_in_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    "skill lifecycle store is locked by another process; retrying"
                );
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// Runs `startup_recovery` on the blocking pool, retrying while the store is
/// busy.
pub async fn run_startup_recovery(
    service: Arc<SkillsLifecycleService>,
    retry: BusyRetry,
) -> Result<StartupReport, ServiceError> {
    retry_busy(retry, || {
        let service = Arc::clone(&service);
        async move {
            crate::runtime::blocking::spawn_blocking(move || service.startup_recovery())
                .await
                .map_err(|error| ServiceError::Worker(error.to_string()))?
        }
    })
    .await
}

/// Daemon start hook: when the lifecycle is enabled, runs the recovery sweep in
/// the background and logs what it did. A disabled lifecycle does nothing. The
/// daemon does not wait for it and its failure does not stop the daemon.
pub fn spawn_startup_recovery(config: &Config) {
    let service = match open_service(&config.workspace_dir, &config.skills_lifecycle) {
        Ok(Some(service)) => service,
        Ok(None) => return,
        Err(error) => {
            tracing::error!(%error, "skill lifecycle is enabled but could not start; recovery did not run");
            return;
        }
    };
    tokio::spawn(async move {
        match run_startup_recovery(service, STARTUP_BUSY_RETRY).await {
            Ok(report) => tracing::info!(
                committed = report.recovery.committed.len(),
                aborted = report.recovery.aborted.len(),
                finalized = report.finalized.len(),
                interrupted = report.interrupted.len(),
                skipped = report.skipped.len(),
                "skill lifecycle startup recovery finished"
            ),
            Err(error) => tracing::error!(%error, "skill lifecycle startup recovery failed"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn enabled_config() -> SkillsLifecycleConfig {
        SkillsLifecycleConfig {
            enabled: true,
            owner_scope: Some("owner:ws:cli:alice".to_string()),
            ..SkillsLifecycleConfig::default()
        }
    }

    fn store_exists(workspace: &Path) -> bool {
        workspace.join("self").exists()
    }

    #[test]
    fn a_disabled_lifecycle_opens_no_store() {
        let workspace = tempfile::TempDir::new().expect("test: temp dir");
        let service =
            open_service(workspace.path(), &SkillsLifecycleConfig::default()).expect("test: disabled is fine");
        assert!(service.is_none());
        assert!(
            !store_exists(workspace.path()),
            "a disabled lifecycle must not touch the workspace"
        );

        // Even a disabled config with an owner and suites opens nothing.
        let mut config = enabled_config();
        config.enabled = false;
        assert!(
            open_service(workspace.path(), &config)
                .expect("test: disabled")
                .is_none()
        );
        assert!(!store_exists(workspace.path()));
    }

    #[test]
    fn an_enabled_lifecycle_opens_its_store_and_a_bad_config_does_not() {
        let workspace = tempfile::TempDir::new().expect("test: temp dir");
        let mut weak = enabled_config();
        weak.require_holdout = false;
        assert!(matches!(
            open_service(workspace.path(), &weak),
            Err(LifecycleSetupError::Config(SkillsLifecycleConfigError::HoldoutRequired))
        ));
        assert!(!store_exists(workspace.path()), "a refused config opens no store");

        let service = open_service(workspace.path(), &enabled_config())
            .expect("test: enabled opens")
            .expect("test: a service");
        assert_eq!(service.owner_scope().as_str(), "owner:ws:cli:alice");
        assert!(store_exists(workspace.path()));

        // The same installation gets the same service, not a reopened store.
        let again = open_service(workspace.path(), &enabled_config())
            .expect("test: enabled opens")
            .expect("test: a service");
        assert!(Arc::ptr_eq(&service, &again));
    }

    #[tokio::test]
    async fn startup_recovery_runs_through_the_blocking_pool_and_is_reported_in_status() {
        let workspace = tempfile::TempDir::new().expect("test: temp dir");
        let service = open_service(workspace.path(), &enabled_config())
            .expect("test: enabled opens")
            .expect("test: a service");
        assert!(service.status().expect("test: status").last_startup.is_none());
        let report = run_startup_recovery(Arc::clone(&service), STARTUP_BUSY_RETRY)
            .await
            .expect("test: recovery");
        assert_eq!(report.skipped, Vec::<(String, String)>::new());
        let status = service.status().expect("test: status");
        let startup = status.last_startup.expect("test: the startup summary is surfaced");
        assert_eq!(startup.skipped_total, 0);

        // It is in the store, owner-only, so another process reads it too.
        let store = SkillLifecycleStore::open(
            workspace.path(),
            crate::skills::lifecycle::OwnerScope::new("owner:ws:cli:alice").expect("test: owner"),
        )
        .expect("test: second handle");
        let other =
            SkillsLifecycleService::new(Arc::new(store), enabled_config().to_settings().expect("test: settings"))
                .expect("test: second service");
        assert_eq!(other.status().expect("test: status").last_startup, Some(startup));
        let file = workspace.path().join("self/skill-lifecycle/status/last_startup.json");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file)
            .expect("test: summary file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode & 0o077, 0, "the summary is owner-only: {mode:o}");
    }

    #[tokio::test]
    async fn a_busy_store_is_retried_with_a_bound_and_other_errors_are_not() {
        let retry = BusyRetry {
            attempts: 4,
            base_delay: Duration::from_millis(1),
        };
        let calls = AtomicU32::new(0);
        let result = retry_busy(retry, || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    Err(ServiceError::Busy {
                        path: "lock".to_string(),
                    })
                } else {
                    Ok(n)
                }
            }
        })
        .await;
        assert_eq!(result.expect("test: third call succeeds"), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let calls = AtomicU32::new(0);
        let result: Result<(), ServiceError> = retry_busy(retry, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Err(ServiceError::Busy {
                    path: "lock".to_string(),
                })
            }
        })
        .await;
        assert!(
            matches!(result, Err(ServiceError::Busy { .. })),
            "the last Busy is returned"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 4, "the retry is bounded by attempts");

        let calls = AtomicU32::new(0);
        let result: Result<(), ServiceError> = retry_busy(retry, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(ServiceError::Disabled) }
        })
        .await;
        assert!(matches!(result, Err(ServiceError::Disabled)));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "only Busy is retried");
    }
}

//! Process-level configuration generation ownership.
//!
//! `ConfigGenerationManager` is the only runtime publisher of effective
//! configuration. Readers pin an immutable [`ConfigGeneration`] (or its
//! `Arc<Config>` payload) for the lifetime of one operation.

use super::{files::compute_config_revision_gated, schema::Config};
use anyhow::Context;
use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

/// Resolve the existing part of a process-bound path and retain any missing
/// tail. This gives aliases one filesystem identity while keeping a pinned
/// binding verifiable if its config file is removed after launch.
fn process_path_identity(path: &Path) -> anyhow::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Ok(PathBuf::new());
    }
    let mut cursor = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match cursor.canonicalize() {
            Ok(mut resolved) => {
                for segment in missing.iter().rev() {
                    resolved.push(segment);
                }
                return Ok(resolved);
            }
            Err(error) => {
                let Some(segment) = cursor.file_name().map(ToOwned::to_owned) else {
                    return Err(error)
                        .with_context(|| format!("failed to resolve process-bound path {}", path.display()));
                };
                missing.push(segment);
                if !cursor.pop() {
                    return Err(error)
                        .with_context(|| format!("failed to resolve process-bound path {}", path.display()));
                }
            }
        }
    }
}

pub(crate) fn process_paths_match(left: &Path, right: &Path) -> anyhow::Result<bool> {
    Ok(process_path_identity(left)? == process_path_identity(right)?)
}

/// Stable revision of the desired configuration source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigSourceRevision {
    pub fingerprint_sha256: String,
    pub disk_generation: Option<u64>,
}

impl ConfigSourceRevision {
    fn from_config(config: &Config) -> anyhow::Result<Self> {
        if !config.config_path.as_os_str().is_empty() && config.config_path.exists() {
            let (fingerprint, disk_generation) = compute_config_revision_gated(&config.config_path)?;
            return Ok(Self {
                fingerprint_sha256: fingerprint.iter().map(|byte| format!("{byte:02x}")).collect(),
                disk_generation: Some(disk_generation),
            });
        }

        let encoded = serde_json::to_vec(config)?;
        Ok(Self {
            fingerprint_sha256: format!("{:x}", Sha256::digest(encoded)),
            disk_generation: None,
        })
    }
}

/// Monotonic, process-local identifier of an actually published generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConfigGenerationId(pub u64);

/// Origin of a reload attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigReloadTrigger {
    Startup,
    FileWatcher,
    Api,
    Tool,
    ConfigFileApi,
    PairingPersistence,
    /// A configuration management transaction: an operator rollback, or the
    /// restore of a change the runtime refused.
    Management,
    Test,
}

/// A desired field that is valid on disk but cannot be activated in-process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredConfigChange {
    pub field: String,
    pub reason: String,
}

/// Immutable runtime configuration generation.
#[derive(Debug, Clone)]
pub struct ConfigGeneration {
    pub id: ConfigGenerationId,
    pub source_revision: ConfigSourceRevision,
    pub effective: Arc<Config>,
    pub applied_at: DateTime<Utc>,
    pub trigger: ConfigReloadTrigger,
    pub deferred_changes: Arc<[DeferredConfigChange]>,
}

/// Metadata and keyed effective-snapshot digest carried to a child process.
/// Configuration and credential values are never included in this transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessGenerationBinding {
    pub id: ConfigGenerationId,
    pub source_revision: ConfigSourceRevision,
    pub applied_at: DateTime<Utc>,
    pub trigger: ConfigReloadTrigger,
    pub deferred_changes: Vec<DeferredConfigChange>,
    pub seal: [u8; 32],
}

impl ProcessGenerationBinding {
    fn signing_payload(&self, config: &Config) -> anyhow::Result<Vec<u8>> {
        // Config excludes both paths from serde; bind them explicitly. Sort
        // nested maps so process-specific HashMap iteration cannot change a tag.
        let workspace = process_path_identity(&config.workspace_dir)?;
        let config_path = process_path_identity(&config.config_path)?;
        let mut value = serde_json::to_value((
            self.id,
            &self.source_revision,
            self.applied_at,
            self.trigger,
            &self.deferred_changes,
            workspace,
            config_path,
            config,
        ))?;
        value.sort_all_objects();
        Ok(serde_json::to_vec(&value)?)
    }
}

impl ConfigGeneration {
    /// Pin an ordinary disabled child without startup globals or disk reloads.
    pub(crate) fn for_disabled_child(config: &Config) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            !config.causal_tree.enabled,
            "ordinary child snapshot requires disabled decision plane"
        );
        let mut value = serde_json::to_value((&config.workspace_dir, &config.config_path, config))?;
        value.sort_all_objects();
        let encoded = serde_json::to_vec(&value)?;
        Ok(Arc::new(Self {
            id: ConfigGenerationId(0),
            source_revision: ConfigSourceRevision {
                fingerprint_sha256: format!("{:x}", Sha256::digest(encoded)),
                disk_generation: None,
            },
            effective: Arc::new(config.clone()),
            applied_at: Utc::now(),
            trigger: ConfigReloadTrigger::Startup,
            deferred_changes: Arc::from([]),
        }))
    }

    pub(crate) fn bind_process(
        &self,
        key: &crate::decision::key::DecisionKey,
    ) -> anyhow::Result<ProcessGenerationBinding> {
        anyhow::ensure!(
            self.source_revision.fingerprint_sha256.len() == 64
                && self
                    .source_revision
                    .fingerprint_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "process configuration source revision is unavailable"
        );
        let mut binding = ProcessGenerationBinding {
            id: self.id,
            source_revision: self.source_revision.clone(),
            applied_at: self.applied_at,
            trigger: self.trigger,
            deferred_changes: self.deferred_changes.to_vec(),
            seal: [0; 32],
        };
        binding.seal = key.hmac(
            crate::decision::key::KeyDomain::StateFingerprint,
            &[
                b"process-effective-config-v1",
                &binding.signing_payload(&self.effective)?,
            ],
        );
        Ok(binding)
    }

    /// Authenticate a parent-owned pinned generation without reopening mutable disk state.
    pub(crate) fn verify_pinned_process_binding(
        &self,
        binding: &ProcessGenerationBinding,
        key: &crate::decision::key::DecisionKey,
    ) -> anyhow::Result<()> {
        let deferred_changes_match = self.deferred_changes.as_ref() == binding.deferred_changes.as_slice();
        anyhow::ensure!(
            self.id == binding.id
                && self.source_revision == binding.source_revision
                && self.applied_at == binding.applied_at
                && self.trigger == binding.trigger
                && deferred_changes_match,
            "pinned process generation differs from launch"
        );
        anyhow::ensure!(
            key.verify_hmac(
                crate::decision::key::KeyDomain::StateFingerprint,
                &[
                    b"process-effective-config-v1",
                    &binding.signing_payload(&self.effective)?
                ],
                &binding.seal,
            ),
            "pinned process effective configuration differs from launch"
        );
        Ok(())
    }

    /// Restore only an exact snapshot. A newer disk revision or a configuration
    /// with different effective values cannot borrow the parent's generation id.
    pub(crate) fn restore_process(
        config: Config,
        binding: &ProcessGenerationBinding,
        key: &crate::decision::key::DecisionKey,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            ConfigSourceRevision::from_config(&config)? == binding.source_revision,
            "process configuration source differs from parent generation"
        );
        anyhow::ensure!(
            key.verify_hmac(
                crate::decision::key::KeyDomain::StateFingerprint,
                &[b"process-effective-config-v1", &binding.signing_payload(&config)?],
                &binding.seal
            ),
            "process effective configuration differs from parent generation"
        );
        Ok(Arc::new(Self {
            id: binding.id,
            source_revision: binding.source_revision.clone(),
            effective: Arc::new(config),
            applied_at: binding.applied_at,
            trigger: binding.trigger,
            deferred_changes: binding.deferred_changes.clone().into(),
        }))
    }
}

/// Latest valid configuration found on disk, including fields awaiting restart.
#[derive(Debug, Clone)]
pub struct DesiredConfigState {
    pub source_revision: ConfigSourceRevision,
    pub config: Arc<Config>,
    pub observed_at: DateTime<Utc>,
}

/// Structured result shared by watcher, API and tool reload entry points.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigApplyReport {
    pub active_generation: ConfigGenerationId,
    pub active_source_revision: ConfigSourceRevision,
    pub desired_source_revision: ConfigSourceRevision,
    pub changed: Vec<String>,
    pub applied: Vec<String>,
    pub rebuilt: Vec<String>,
    pub restarted: Vec<String>,
    pub restart_required: Vec<String>,
    pub participant_acks: Vec<String>,
}

impl ConfigApplyReport {
    #[must_use]
    pub const fn has_runtime_change(&self) -> bool {
        !(self.applied.is_empty() && self.rebuilt.is_empty() && self.restarted.is_empty())
    }

    #[must_use]
    pub const fn status(&self) -> &'static str {
        if self.changed.is_empty() {
            "unchanged"
        } else if self.restart_required.is_empty() {
            "applied"
        } else if self.has_runtime_change() {
            "applied_with_restart_required"
        } else {
            "restart_required"
        }
    }
}

/// Most recent reload failure retained for doctor/health/config status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigReloadFailure {
    pub occurred_at: DateTime<Utc>,
    pub trigger: ConfigReloadTrigger,
    pub desired_source_revision: Option<ConfigSourceRevision>,
    pub error: String,
}

/// Read-only process status for the generation coordinator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigGenerationStatus {
    pub active_generation: ConfigGenerationId,
    pub active_source_revision: ConfigSourceRevision,
    pub desired_source_revision: ConfigSourceRevision,
    pub reload_in_progress: bool,
    pub last_report: Option<ConfigApplyReport>,
    pub last_failure: Option<ConfigReloadFailure>,
    pub registered_participants: Vec<String>,
}

/// Reload/supervisor event emitted into the existing MessageEvent ledger.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigGenerationAuditEvent {
    pub event_type: String,
    pub generation_id: Option<ConfigGenerationId>,
    pub source_revision: Option<ConfigSourceRevision>,
    pub trigger: ConfigReloadTrigger,
    pub payload: serde_json::Value,
}

/// Adapter used to route config lifecycle events to the process event fabric.
pub trait ConfigGenerationAuditSink: Send + Sync {
    fn record(&self, event: ConfigGenerationAuditEvent);
}

/// Sole process-level owner of active and desired configuration generations.
pub struct ConfigGenerationManager {
    active: ArcSwap<ConfigGeneration>,
    /// Published generations remain addressable while an admitted operation
    /// still owns its exact snapshot. Weak entries retain no configuration or
    /// credentials after the last operation releases that generation.
    published: Mutex<BTreeMap<ConfigGenerationId, Weak<ConfigGeneration>>>,
    desired: ArcSwap<DesiredConfigState>,
    reload_lock: Mutex<()>,
    next_generation: AtomicU64,
    participants: RwLock<Vec<Weak<dyn ConfigGenerationParticipant>>>,
    reload_in_progress: AtomicBool,
    last_report: RwLock<Option<ConfigApplyReport>>,
    last_failure: RwLock<Option<ConfigReloadFailure>>,
    audit_sinks: RwLock<Vec<Arc<dyn ConfigGenerationAuditSink>>>,
    /// The configured-model allowlist refreshed with every generation.
    model_allowlist: Arc<crate::observability::labels::ModelAllowlist>,
}

/// Runtime subsystem that must prepare a complete candidate before publication.
pub trait ConfigGenerationParticipant: Send + Sync {
    fn name(&self) -> &'static str;

    /// Whether this participant can atomically rebuild a rebuild-and-swap field.
    fn supports_rebuild_field(&self, _field: &str) -> bool {
        false
    }

    /// Whether this participant can perform a controlled restart for a field.
    fn supports_controlled_restart_field(&self, _field: &str) -> bool {
        false
    }

    /// Whether the participant must prepare when this effective field changes.
    fn prepares_for_field(&self, field: &str) -> bool {
        self.supports_rebuild_field(field) || self.supports_controlled_restart_field(field)
    }

    fn prepare(
        &self,
        generation: Arc<ConfigGeneration>,
        changed_fields: &[String],
    ) -> anyhow::Result<Box<dyn PreparedConfigGeneration>>;
}

/// Reversible commit produced by a successful participant preparation.
pub trait PreparedConfigGeneration: Send {
    fn commit(&mut self) -> anyhow::Result<()>;
    fn rollback(&mut self);

    /// Release rollback state after the manager has published active.
    fn finalize(&mut self) {}
}

impl std::fmt::Debug for ConfigGenerationManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let active = self.active.load();
        formatter
            .debug_struct("ConfigGenerationManager")
            .field("active_generation", &active.id)
            .field("active_source_revision", &active.source_revision)
            .finish_non_exhaustive()
    }
}

impl ConfigGenerationManager {
    /// Build the process owner around the startup-effective configuration.
    #[must_use]
    pub fn new(initial: Config) -> Self {
        Self::with_model_allowlist(initial, crate::observability::labels::ModelAllowlist::global())
    }

    /// Establish an owner for a standalone caller that already holds an
    /// effective generation. Unlike startup loading, this never relabels the
    /// snapshot using a later on-disk revision. A generation with deferred
    /// fields needs its existing manager, which owns the distinct desired
    /// configuration; an effective snapshot cannot reconstruct that state.
    pub(crate) fn from_pinned_generation(generation: Arc<ConfigGeneration>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            generation.deferred_changes.is_empty(),
            "a generation with deferred configuration requires its existing manager"
        );
        anyhow::ensure!(
            !generation.source_revision.fingerprint_sha256.is_empty()
                && generation.source_revision.fingerprint_sha256 != "startup-unavailable",
            "pinned configuration source revision is unavailable"
        );
        let next_generation = generation
            .id
            .0
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("configuration generation counter is exhausted"))?;
        let model_allowlist = crate::observability::labels::ModelAllowlist::global();
        // Preserve standalone startup setup, but take every value from the
        // supplied effective snapshot rather than loading a new revision.
        crate::config::set_runtime_proxy_config(generation.effective.proxy.clone());
        if let Some(warning) = crate::config::manage::validate::decision_startup_check(&generation.effective) {
            tracing::warn!("{warning}");
        }
        if generation.effective.causal_tree.enabled
            && let Some(warning) = crate::config::manage::validate::decision_orphans_warning(&generation.effective)
        {
            tracing::warn!("{warning}");
        }
        model_allowlist.refresh(&generation.effective);
        let desired = Arc::new(DesiredConfigState {
            source_revision: generation.source_revision.clone(),
            config: Arc::clone(&generation.effective),
            observed_at: generation.applied_at,
        });
        let published = BTreeMap::from([(generation.id, Arc::downgrade(&generation))]);
        Ok(Self {
            active: ArcSwap::from(generation),
            published: Mutex::new(published),
            desired: ArcSwap::from(desired),
            reload_lock: Mutex::new(()),
            next_generation: AtomicU64::new(next_generation),
            participants: RwLock::new(Vec::new()),
            reload_in_progress: AtomicBool::new(false),
            last_report: RwLock::new(None),
            last_failure: RwLock::new(None),
            audit_sinks: RwLock::new(Vec::new()),
            model_allowlist,
        })
    }

    /// [`Self::new`] refreshing `model_allowlist` instead of the process
    /// allowlist the metrics read.
    fn with_model_allowlist(
        initial: Config,
        model_allowlist: Arc<crate::observability::labels::ModelAllowlist>,
    ) -> Self {
        crate::config::set_runtime_proxy_config(initial.proxy.clone());
        // Startup never fails for the decision plane: a refused backend
        // runs builtin-only, and says so (D-094 C-1).
        if let Some(warning) = crate::config::manage::validate::decision_startup_check(&initial) {
            tracing::warn!("{warning}");
        }
        // Decision credentials left unreferenced are reported (D-094 C-5);
        // a disabled plane does not read the credential store.
        if initial.causal_tree.enabled
            && let Some(warning) = crate::config::manage::validate::decision_orphans_warning(&initial)
        {
            tracing::warn!("{warning}");
        }
        // Metric model labels follow the configured models (D-059).
        model_allowlist.refresh(&initial);
        let source_revision = ConfigSourceRevision::from_config(&initial).unwrap_or_else(|_| ConfigSourceRevision {
            fingerprint_sha256: "startup-unavailable".to_string(),
            disk_generation: None,
        });
        let config = Arc::new(initial);
        let generation = Arc::new(ConfigGeneration {
            id: ConfigGenerationId(0),
            source_revision: source_revision.clone(),
            effective: Arc::clone(&config),
            applied_at: Utc::now(),
            trigger: ConfigReloadTrigger::Startup,
            deferred_changes: Arc::from([]),
        });
        let desired = Arc::new(DesiredConfigState {
            source_revision,
            config,
            observed_at: Utc::now(),
        });
        let published = BTreeMap::from([(generation.id, Arc::downgrade(&generation))]);
        Self {
            active: ArcSwap::from(generation),
            published: Mutex::new(published),
            desired: ArcSwap::from(desired),
            reload_lock: Mutex::new(()),
            next_generation: AtomicU64::new(1),
            participants: RwLock::new(Vec::new()),
            reload_in_progress: AtomicBool::new(false),
            last_report: RwLock::new(None),
            last_failure: RwLock::new(None),
            audit_sinks: RwLock::new(Vec::new()),
            model_allowlist,
        }
    }

    /// Register a weakly-held runtime participant.
    ///
    /// The caller retains the strong `Arc`; stale registrations disappear when
    /// their owning component is dropped or restarted.
    pub fn register_participant(&self, participant: &Arc<dyn ConfigGenerationParticipant>) {
        let _reload_guard = self.reload_lock.lock();
        self.participants.write().push(Arc::downgrade(participant));
    }

    /// Register an event-fabric adapter for reload and supervisor lifecycle audit.
    pub fn register_audit_sink(&self, sink: Arc<dyn ConfigGenerationAuditSink>) {
        self.audit_sinks.write().push(sink);
    }

    /// Pin the complete immutable active generation for one operation.
    #[must_use]
    pub fn pin(&self) -> Arc<ConfigGeneration> {
        self.active.load_full()
    }

    /// Recover one published generation only while an admitted operation still
    /// pins it. This lets long-lived tool registries resolve the provider owned
    /// by an in-flight turn without retaining superseded credentials forever.
    #[must_use]
    pub(crate) fn pin_generation(&self, id: ConfigGenerationId) -> Option<Arc<ConfigGeneration>> {
        let active = self.active.load_full();
        if active.id == id {
            return Some(active);
        }
        let mut published = self.published.lock();
        published.retain(|_, generation| generation.strong_count() > 0);
        published.get(&id).and_then(Weak::upgrade)
    }

    /// Compatibility accessor for readers that only need the effective config.
    #[must_use]
    pub fn load_full(&self) -> Arc<Config> {
        Arc::clone(&self.active.load_full().effective)
    }

    /// Compatibility accessor matching the former ArcSwap reader surface.
    #[must_use]
    pub fn load(&self) -> Arc<Config> {
        self.load_full()
    }

    /// Return the latest desired configuration, including deferred fields.
    #[must_use]
    pub fn desired(&self) -> Arc<DesiredConfigState> {
        self.desired.load_full()
    }

    #[must_use]
    pub fn active_generation_id(&self) -> ConfigGenerationId {
        self.active.load().id
    }

    /// Snapshot coordinator status without exposing mutation primitives.
    #[must_use]
    pub fn status(&self) -> ConfigGenerationStatus {
        let active = self.active.load_full();
        let desired = self.desired.load_full();
        let mut registered_participants = self
            .participants
            .read()
            .iter()
            .filter_map(Weak::upgrade)
            .map(|participant| participant.name().to_string())
            .collect::<Vec<_>>();
        registered_participants.sort();
        registered_participants.dedup();
        ConfigGenerationStatus {
            active_generation: active.id,
            active_source_revision: active.source_revision.clone(),
            desired_source_revision: desired.source_revision.clone(),
            reload_in_progress: self.reload_in_progress.load(Ordering::Acquire),
            last_report: self.last_report.read().clone(),
            last_failure: self.last_failure.read().clone(),
            registered_participants,
        }
    }

    /// Load a stable merged snapshot from the configured source and apply it.
    pub fn reload_from_disk(&self, trigger: ConfigReloadTrigger) -> anyhow::Result<ConfigApplyReport> {
        let current = self.load_full();
        let config_path = current.config_path.clone();
        if config_path.as_os_str().is_empty() {
            let error = anyhow::anyhow!("Config path is not set; cannot reload");
            self.record_failure(trigger, None, &error);
            return Err(error);
        }
        // The revision is the one the loaded files had, not one read again
        // afterwards: a write landing in between must not be published as the
        // revision of content that does not contain it.
        let (fresh, revision) = match load_stable_config_snapshot(&config_path, current.workspace_dir.clone()) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.record_failure(trigger, None, &error);
                return Err(error);
            }
        };
        #[cfg(test)]
        run_after_snapshot_hook(&config_path);
        // An operator's file edit is trusted and applied without
        // confirmation; one that sends decisions off the machine is said so,
        // once the reload is applied (D-079 L-4, D-082 N-4). It is compared
        // with the desired configuration, so an edit already published and
        // waiting for a restart is not reported again.
        let remote_paths = if matches!(trigger, ConfigReloadTrigger::FileWatcher | ConfigReloadTrigger::Tool) {
            hand_edited_remote_decisions(&self.desired().config, &fresh)
        } else {
            Vec::new()
        };
        let report = self.apply_config(fresh, revision, trigger)?;
        for path in remote_paths {
            tracing::warn!(
                path = %path,
                "a configuration file edit sends decision state off this machine; it was applied without \
                 confirmation because the operator edited the file"
            );
        }
        Ok(report)
    }

    /// Publish a validated desired configuration through the sole owner.
    ///
    /// Process-restart-only fields are retained from the previous active
    /// generation while the complete desired value remains queryable via
    /// [`desired`](Self::desired).
    pub fn apply_config(
        &self,
        desired: Config,
        source_revision: ConfigSourceRevision,
        trigger: ConfigReloadTrigger,
    ) -> anyhow::Result<ConfigApplyReport> {
        let _reload_guard = self.reload_lock.lock();
        self.reload_in_progress.store(true, Ordering::Release);
        let result = self.apply_config_locked(desired, source_revision.clone(), trigger);
        self.reload_in_progress.store(false, Ordering::Release);
        match &result {
            Ok(report) => {
                *self.last_report.write() = Some(report.clone());
                *self.last_failure.write() = None;
                if report.has_runtime_change() {
                    self.emit_audit(ConfigGenerationAuditEvent {
                        event_type: "config.reload.applied".to_string(),
                        generation_id: Some(report.active_generation),
                        source_revision: Some(report.active_source_revision.clone()),
                        trigger,
                        payload: serde_json::to_value(report).unwrap_or_else(|_| serde_json::json!({})),
                    });
                }
                if !report.restart_required.is_empty() {
                    self.emit_audit(ConfigGenerationAuditEvent {
                        event_type: "config.reload.deferred".to_string(),
                        generation_id: Some(report.active_generation),
                        source_revision: Some(report.desired_source_revision.clone()),
                        trigger,
                        payload: serde_json::json!({
                            "restart_required": report.restart_required,
                        }),
                    });
                }
            }
            Err(error) => self.record_failure(trigger, Some(source_revision), error),
        }
        result
    }

    fn apply_config_locked(
        &self,
        desired: Config,
        source_revision: ConfigSourceRevision,
        trigger: ConfigReloadTrigger,
    ) -> anyhow::Result<ConfigApplyReport> {
        desired.validate()?;

        let old_generation = self.active.load_full();
        // The decision backend check every configuration plan runs: a hand
        // edit to an invalid backend leaves this generation active
        // (CDP-12, R-CXR P0-B t2).
        // A reload that does not own a refusal is not held back by it
        // (D-094 C-1).
        if let Some(warning) =
            crate::config::manage::validate::check_decision_plane(Some(old_generation.effective.as_ref()), &desired)
                .map_err(|refusal| anyhow::anyhow!(refusal))?
        {
            tracing::warn!("{warning}");
        }
        let active = old_generation.effective.as_ref();
        let adopted = adopt_live_pairing_tokens(active, &desired, trigger);
        let old = adopted.as_ref().unwrap_or(active);
        let changed = changed_top_level_fields(old, &desired)?;
        let participants = {
            let mut registrations = self.participants.write();
            let active = registrations.iter().filter_map(Weak::upgrade).collect::<Vec<_>>();
            registrations.retain(|participant| participant.strong_count() > 0);
            active
        };
        let restart_required = changed
            .iter()
            .filter(|field| {
                is_process_restart_only(field)
                    || (is_rebuild_and_swap(field)
                        && !participants
                            .iter()
                            .any(|participant| participant.supports_rebuild_field(field)))
                    || (is_controlled_restart(field)
                        && !participants
                            .iter()
                            .any(|participant| participant.supports_controlled_restart_field(field)))
            })
            .cloned()
            .collect::<Vec<_>>();
        let rebuilt = changed
            .iter()
            .filter(|field| is_rebuild_and_swap(field) && !restart_required.contains(field))
            .cloned()
            .collect::<Vec<_>>();
        let restarted = changed
            .iter()
            .filter(|field| is_controlled_restart(field) && !restart_required.contains(field))
            .cloned()
            .collect::<Vec<_>>();
        let mut applied = changed
            .iter()
            .filter(|field| {
                !is_process_restart_only(field) && !is_rebuild_and_swap(field) && !is_controlled_restart(field)
            })
            .cloned()
            .collect::<Vec<_>>();
        // Adopted pairing tokens are applied in-process (the pairing guard
        // holds them); report them so, unless another `gateway` change is
        // what is being deferred.
        let mut changed = changed;
        if adopted.is_some() && !changed.iter().any(|field| field == "gateway") {
            changed.push("gateway".to_string());
            applied.push("gateway".to_string());
        }

        self.desired.store(Arc::new(DesiredConfigState {
            source_revision: source_revision.clone(),
            config: Arc::new(desired.clone()),
            observed_at: Utc::now(),
        }));

        let effective = preserve_deferred_fields(old, desired, &restart_required)?;
        // Against the generation that is really active: adopted pairing
        // tokens are a change to publish even when nothing else changed.
        let effective_changed = changed_top_level_fields(active, &effective)?;
        let deferred_changes: Arc<[DeferredConfigChange]> = restart_required
            .iter()
            .map(|field| DeferredConfigChange {
                field: field.clone(),
                reason: "process restart required".to_string(),
            })
            .collect::<Vec<_>>()
            .into();

        let mut participant_acks = Vec::new();
        let (active_generation, active_source_revision) = if effective_changed.is_empty() {
            (old_generation.id, old_generation.source_revision.clone())
        } else {
            let supervisor_switch = !restarted.is_empty()
                || participants.iter().any(|participant| {
                    participant.name() == "daemon_component_supervisors"
                        && effective_changed
                            .iter()
                            .any(|field| participant.prepares_for_field(field))
                });
            let id = ConfigGenerationId(
                self.next_generation
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |next| next.checked_add(1))
                    .map_err(|_| anyhow::anyhow!("configuration generation counter is exhausted"))?,
            );
            let generation = Arc::new(ConfigGeneration {
                id,
                source_revision: source_revision.clone(),
                effective: Arc::new(effective),
                applied_at: Utc::now(),
                trigger,
                deferred_changes,
            });
            self.emit_audit(ConfigGenerationAuditEvent {
                event_type: "config.reload.prepared".to_string(),
                generation_id: Some(generation.id),
                source_revision: Some(generation.source_revision.clone()),
                trigger,
                payload: serde_json::json!({
                    "changed": effective_changed,
                    "rebuilt": rebuilt,
                    "restarted": restarted,
                }),
            });
            let previous_runtime_proxy = effective_changed
                .iter()
                .any(|field| field == "proxy")
                .then(crate::config::runtime_proxy_config);
            if previous_runtime_proxy.is_some() {
                crate::config::set_runtime_proxy_config(generation.effective.proxy.clone());
            }
            if supervisor_switch {
                self.emit_audit(ConfigGenerationAuditEvent {
                    event_type: "config.supervisor.restart_started".to_string(),
                    generation_id: Some(generation.id),
                    source_revision: Some(generation.source_revision.clone()),
                    trigger,
                    payload: serde_json::json!({ "fields": restarted }),
                });
            }
            let mut prepared = Vec::with_capacity(participants.len());
            for participant in participants.into_iter().filter(|participant| {
                effective_changed
                    .iter()
                    .any(|field| participant.prepares_for_field(field))
            }) {
                match participant.prepare(Arc::clone(&generation), &effective_changed) {
                    Ok(update) => {
                        participant_acks.push(participant.name().to_string());
                        prepared.push(update);
                    }
                    Err(error) => {
                        for update in prepared.iter_mut().rev() {
                            update.rollback();
                        }
                        if let Some(previous_proxy) = previous_runtime_proxy.as_ref() {
                            crate::config::set_runtime_proxy_config(previous_proxy.clone());
                        }
                        if supervisor_switch {
                            self.emit_audit(ConfigGenerationAuditEvent {
                                event_type: "config.supervisor.rollback".to_string(),
                                generation_id: Some(generation.id),
                                source_revision: Some(generation.source_revision.clone()),
                                trigger,
                                payload: serde_json::json!({
                                    "participant": participant.name(),
                                    "reason": error.to_string(),
                                }),
                            });
                        }
                        return Err(anyhow::anyhow!(
                            "config generation participant '{}' rejected generation {}: {error}",
                            participant.name(),
                            generation.id.0
                        ));
                    }
                }
            }
            if let Err(error) = prepared.iter_mut().try_for_each(|update| update.commit()) {
                for update in prepared.iter_mut().rev() {
                    update.rollback();
                }
                if let Some(previous_proxy) = previous_runtime_proxy.as_ref() {
                    crate::config::set_runtime_proxy_config(previous_proxy.clone());
                }
                if supervisor_switch {
                    self.emit_audit(ConfigGenerationAuditEvent {
                        event_type: "config.supervisor.rollback".to_string(),
                        generation_id: Some(generation.id),
                        source_revision: Some(generation.source_revision.clone()),
                        trigger,
                        payload: serde_json::json!({ "reason": error.to_string() }),
                    });
                }
                anyhow::bail!(
                    "config generation participant commit failed for generation {}: {error}",
                    generation.id.0
                );
            }
            self.active.store(Arc::clone(&generation));
            {
                let mut published = self.published.lock();
                published.retain(|_, pinned| pinned.strong_count() > 0);
                published.insert(generation.id, Arc::downgrade(&generation));
            }
            self.model_allowlist.refresh(&generation.effective);
            for update in &mut prepared {
                update.finalize();
            }
            if supervisor_switch {
                self.emit_audit(ConfigGenerationAuditEvent {
                    event_type: "config.supervisor.restart_completed".to_string(),
                    generation_id: Some(generation.id),
                    source_revision: Some(generation.source_revision.clone()),
                    trigger,
                    payload: serde_json::json!({ "fields": restarted }),
                });
            }
            (generation.id, generation.source_revision.clone())
        };

        Ok(ConfigApplyReport {
            active_generation,
            active_source_revision,
            desired_source_revision: source_revision,
            changed,
            applied,
            rebuilt,
            restarted,
            restart_required,
            participant_acks,
        })
    }

    /// Convenience for in-memory callers and tests that do not have a disk revision.
    pub fn apply_runtime_config(
        &self,
        desired: Config,
        trigger: ConfigReloadTrigger,
    ) -> anyhow::Result<ConfigApplyReport> {
        let revision = ConfigSourceRevision::from_config(&desired)?;
        self.apply_config(desired, revision, trigger)
    }

    fn record_failure(
        &self,
        trigger: ConfigReloadTrigger,
        desired_source_revision: Option<ConfigSourceRevision>,
        error: &anyhow::Error,
    ) {
        *self.last_failure.write() = Some(ConfigReloadFailure {
            occurred_at: Utc::now(),
            trigger,
            desired_source_revision: desired_source_revision.clone(),
            error: error.to_string(),
        });
        self.emit_audit(ConfigGenerationAuditEvent {
            event_type: "config.reload.failed".to_string(),
            generation_id: Some(self.active_generation_id()),
            source_revision: desired_source_revision,
            trigger,
            payload: serde_json::json!({ "error": error.to_string() }),
        });
    }

    fn emit_audit(&self, event: ConfigGenerationAuditEvent) {
        for sink in self.audit_sinks.read().iter() {
            sink.record(event.clone());
        }
    }
}

/// Test seam: run once, for one config path, right after the stable
/// snapshot is loaded (a write landing in the reload's window).
#[cfg(test)]
#[allow(clippy::type_complexity)]
pub(crate) static AFTER_SNAPSHOT_HOOK: parking_lot::Mutex<Option<(std::path::PathBuf, Box<dyn FnOnce() + Send>)>> =
    parking_lot::Mutex::new(None);

#[cfg(test)]
fn run_after_snapshot_hook(config_path: &Path) {
    let hook = {
        let mut slot = AFTER_SNAPSHOT_HOOK.lock();
        if slot.as_ref().is_some_and(|(path, _)| path == config_path) {
            slot.take()
        } else {
            None
        }
    };
    if let Some((_, hook)) = hook {
        hook();
    }
}

fn load_stable_config_snapshot(
    config_path: &Path,
    workspace_dir: std::path::PathBuf,
) -> anyhow::Result<(Config, ConfigSourceRevision)> {
    const MAX_ATTEMPTS: usize = 3;

    for attempt in 1..=MAX_ATTEMPTS {
        let before = compute_config_revision_gated(config_path)?;
        let fresh = Config::load_from_path(config_path, workspace_dir.clone())?;
        let after = compute_config_revision_gated(config_path)?;
        if before == after {
            let (fingerprint, disk_generation) = before;
            let revision = ConfigSourceRevision {
                fingerprint_sha256: fingerprint.iter().map(|byte| format!("{byte:02x}")).collect(),
                disk_generation: Some(disk_generation),
            };
            return Ok((fresh, revision));
        }

        tracing::debug!(
            path = %config_path.display(),
            attempt,
            "Config changed during generation snapshot; retrying"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    anyhow::bail!("Config changed repeatedly while reloading: {}", config_path.display())
}

fn changed_top_level_fields(old: &Config, desired: &Config) -> anyhow::Result<Vec<String>> {
    let old = serde_json::to_value(old)?;
    let desired = serde_json::to_value(desired)?;
    let old = old
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("serialized config root is not an object"))?;
    let desired = desired
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("serialized config root is not an object"))?;
    let keys = old.keys().chain(desired.keys()).cloned().collect::<BTreeSet<_>>();
    Ok(keys
        .into_iter()
        .filter(|key| old.get(key) != desired.get(key))
        .collect())
}

/// The active configuration with the persisted `gateway.paired_tokens`
/// adopted, when `trigger` is the pairing persistence and the list differs.
///
/// `gateway` is a process-restart-only field, and its token list is too for
/// every other writer: a token added by hand is not accepted until restart.
/// The one exception is `/pair`: the running pairing guard already holds the
/// token it issued, and the reload that persists it is triggered with
/// [`ConfigReloadTrigger::PairingPersistence`]. Keeping the old list active
/// after that reload made every later reload find `gateway` changed again, so
/// even a save of `agent.compact_context` reported
/// `restart_required: ["gateway"]` (CONSOLE-OBS-3), and a supervised gateway
/// restart would have been built from the list without the token. Only the
/// token list is adopted; any other `gateway` change stays deferred.
fn adopt_live_pairing_tokens(active: &Config, desired: &Config, trigger: ConfigReloadTrigger) -> Option<Config> {
    if trigger != ConfigReloadTrigger::PairingPersistence
        || active.gateway.paired_tokens == desired.gateway.paired_tokens
    {
        return None;
    }
    let mut adopted = active.clone();
    adopted.gateway.paired_tokens.clone_from(&desired.gateway.paired_tokens);
    Some(adopted)
}

pub(crate) fn is_process_restart_only(field: &str) -> bool {
    matches!(
        field,
        "runtime"
            | "memory"
            | "storage"
            | "tunnel"
            | "gateway"
            | "modules"
            | "observability"
            | "mcp_server"
            | "a2a"
            | "skills_lifecycle"
    )
}

pub(crate) fn is_controlled_restart(field: &str) -> bool {
    matches!(
        field,
        "channels_config" | "cron" | "scheduler" | "heartbeat" | "xin" | "webhook" | "self_system"
    )
}

pub(crate) fn is_rebuild_and_swap(field: &str) -> bool {
    matches!(
        field,
        "api_key"
            | "api_url"
            | "auth"
            | "default_provider"
            | "default_model"
            | "providers"
            | "reliability"
            | "security"
            | "autonomy"
            | "browser"
            | "http_request"
            | "multimodal"
            | "mcp"
            | "composio"
            | "agents"
            | "tool_tiering"
            | "nodes"
            | "skills"
            | "skill_rag"
            | "model_routes"
            | "embedding_routes"
            | "query_classification"
            | "task_routing"
            | "router"
            | "smart_group"
            | "identity"
            | "identity_bindings"
            | "user_policies"
            | "secrets"
            | "media"
            | "causal_tree"
            | "proxy"
    )
}

/// The paths of `remote_decision_enabled` reasons between `old` and `new`:
/// what a hand edit applied by a reload turned towards remote decisions.
fn hand_edited_remote_decisions(old: &Config, new: &Config) -> Vec<String> {
    crate::config::manage::ConfigChangeSummary::between(Some(old), new)
        .map(|summary| {
            summary
                .risk_reasons
                .into_iter()
                .filter_map(|reason| match reason {
                    crate::config::manage::RiskReason::RemoteDecisionEnabled { path } => Some(path),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_else(|error| {
            tracing::warn!("could not compare the reloaded configuration: {error:#}");
            Vec::new()
        })
}

fn preserve_deferred_fields(old: &Config, desired: Config, fields: &[String]) -> anyhow::Result<Config> {
    let old_value = serde_json::to_value(old)?;
    let mut effective_value = serde_json::to_value(desired)?;
    let old_object = old_value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("serialized active config root is not an object"))?;
    let effective_object = effective_value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("serialized desired config root is not an object"))?;
    for field in fields {
        if let Some(value) = old_object.get(field) {
            effective_object.insert(field.clone(), value.clone());
        } else {
            effective_object.remove(field);
        }
    }
    let mut effective: Config = serde_json::from_value(effective_value)?;
    effective.workspace_dir = old.workspace_dir.clone();
    effective.config_path = old.config_path.clone();
    Ok(effective)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    struct RecordingParticipant {
        name: &'static str,
        events: Arc<Mutex<Vec<String>>>,
        fail_prepare: bool,
        fail_commit: bool,
    }

    struct RecordingPrepared {
        name: &'static str,
        events: Arc<Mutex<Vec<String>>>,
        fail_commit: bool,
    }

    struct RecordingAuditSink {
        event_types: Arc<Mutex<Vec<String>>>,
    }

    impl ConfigGenerationAuditSink for RecordingAuditSink {
        fn record(&self, event: ConfigGenerationAuditEvent) {
            self.event_types.lock().push(event.event_type);
        }
    }

    impl ConfigGenerationParticipant for RecordingParticipant {
        fn name(&self) -> &'static str {
            self.name
        }

        fn supports_rebuild_field(&self, _field: &str) -> bool {
            true
        }

        fn supports_controlled_restart_field(&self, _field: &str) -> bool {
            true
        }

        fn prepares_for_field(&self, _field: &str) -> bool {
            true
        }

        fn prepare(
            &self,
            _generation: Arc<ConfigGeneration>,
            _changed_fields: &[String],
        ) -> anyhow::Result<Box<dyn PreparedConfigGeneration>> {
            self.events.lock().push(format!("{}.prepare", self.name));
            if self.fail_prepare {
                anyhow::bail!("injected prepare failure");
            }
            Ok(Box::new(RecordingPrepared {
                name: self.name,
                events: Arc::clone(&self.events),
                fail_commit: self.fail_commit,
            }))
        }
    }

    impl PreparedConfigGeneration for RecordingPrepared {
        fn commit(&mut self) -> anyhow::Result<()> {
            self.events.lock().push(format!("{}.commit", self.name));
            if self.fail_commit {
                anyhow::bail!("injected commit failure");
            }
            Ok(())
        }

        fn rollback(&mut self) {
            self.events.lock().push(format!("{}.rollback", self.name));
        }

        fn finalize(&mut self) {
            self.events.lock().push(format!("{}.finalize", self.name));
        }
    }

    /// The proxy the publication tests publish: a real `all_proxy`, so the
    /// publication stays observable, but with loopback exempt, so a test
    /// running at the same time against its own `127.0.0.1` server is never
    /// sent to a proxy that is not there (R-INT2A F3). A services scope with a
    /// dummy key would be refused by validation, which only knows real keys.
    fn test_publication_proxy(url: &str) -> crate::config::ProxyConfig {
        crate::config::ProxyConfig {
            all_proxy: Some(url.to_string()),
            no_proxy: vec!["127.0.0.1".to_string(), "localhost".to_string(), "::1".to_string()],
            ..Config::default().proxy
        }
    }

    /// Restores the process-wide runtime proxy when dropped, so a failing
    /// assertion cannot leave a test proxy published.
    struct RuntimeProxyRestore(crate::config::ProxyConfig);

    impl RuntimeProxyRestore {
        fn capture() -> Self {
            Self(crate::config::runtime_proxy_config())
        }

        fn publish(proxy: crate::config::ProxyConfig) -> Self {
            let restore = Self::capture();
            crate::config::set_runtime_proxy_config(proxy);
            restore
        }
    }

    impl Drop for RuntimeProxyRestore {
        fn drop(&mut self) {
            crate::config::set_runtime_proxy_config(self.0.clone());
        }
    }

    fn participant(
        name: &'static str,
        events: Arc<Mutex<Vec<String>>>,
        fail_prepare: bool,
        fail_commit: bool,
    ) -> Arc<dyn ConfigGenerationParticipant> {
        Arc::new(RecordingParticipant {
            name,
            events,
            fail_prepare,
            fail_commit,
        })
    }

    #[test]
    #[serial]
    fn one_operation_keeps_pinned_generation_after_reload() {
        let manager = ConfigGenerationManager::new(Config::default());
        let pinned = manager.pin();
        let mut desired = (*pinned.effective).clone();
        desired.default_temperature = 0.31;

        let report = manager
            .apply_runtime_config(desired, ConfigReloadTrigger::Test)
            .expect("apply config");

        assert_eq!(pinned.id, ConfigGenerationId(0));
        assert_eq!(manager.pin().id, report.active_generation);
        assert_ne!(pinned.id, manager.pin().id);
        assert_ne!(
            pinned.effective.default_temperature,
            manager.pin().effective.default_temperature
        );
    }

    #[test]
    #[serial]
    fn restart_only_fields_update_desired_but_not_active() {
        let manager = ConfigGenerationManager::new(Config::default());
        let old_active = manager.load_full();
        let mut desired = (*old_active).clone();
        desired.runtime.kind = "docker".to_string();

        let report = manager
            .apply_runtime_config(desired.clone(), ConfigReloadTrigger::Test)
            .expect("apply config");

        assert_eq!(manager.load_full().runtime.kind, old_active.runtime.kind);
        assert_eq!(manager.desired().config.runtime.kind, desired.runtime.kind);
        assert_eq!(report.restart_required, vec!["runtime"]);
        assert_eq!(report.status(), "restart_required");
    }

    /// `[skills_lifecycle]` is read once at process start (the service, its
    /// store and the registered tools are built from it), so a reload that
    /// changes it reports `restart_required` and the effective configuration
    /// keeps the old value, while the desired one records the edit.
    #[test]
    #[serial]
    fn a_changed_skills_lifecycle_needs_a_restart_and_keeps_the_old_value() {
        let manager = ConfigGenerationManager::new(Config::default());
        let old_active = manager.load_full();
        assert!(!old_active.skills_lifecycle.enabled);
        let mut desired = (*old_active).clone();
        desired.skills_lifecycle.enabled = true;
        desired.skills_lifecycle.owner_scope = Some("owner:ws:cli:alice".to_string());
        desired.skills_lifecycle.mode = crate::config::skills_lifecycle::SkillsLifecycleModeConfig::Auto;

        let report = manager
            .apply_runtime_config(desired.clone(), ConfigReloadTrigger::Test)
            .expect("apply config");

        assert_eq!(report.restart_required, vec!["skills_lifecycle"]);
        assert_eq!(report.status(), "restart_required");
        assert_eq!(manager.load_full().skills_lifecycle, old_active.skills_lifecycle);
        assert!(!manager.load_full().skills_lifecycle.enabled);
        assert_eq!(manager.desired().config.skills_lifecycle, desired.skills_lifecycle);
        assert!(is_process_restart_only("skills_lifecycle"));
    }

    #[test]
    #[serial]
    fn reload_report_classifies_runtime_changes() {
        let manager = ConfigGenerationManager::new(Config::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let runtime = participant("runtime", events, false, false);
        manager.register_participant(&runtime);
        let mut desired = (*manager.load_full()).clone();
        desired.default_temperature = 0.22;
        desired.default_model = Some("example/model".to_string());
        desired.cron.max_run_history += 1;

        let report = manager
            .apply_runtime_config(desired, ConfigReloadTrigger::Test)
            .expect("apply config");

        assert_eq!(report.applied, vec!["default_temperature"]);
        assert_eq!(report.rebuilt, vec!["default_model"]);
        assert_eq!(report.restarted, vec!["cron"]);
        assert!(report.restart_required.is_empty(), "expected an empty value");
    }

    /// CONSOLE-OBS-3 (CONSOLE-VERIFY): once the gateway paired, every later
    /// save reported `restart_required: ["gateway"]`, even one that changed
    /// only `agent.compact_context`. `/pair` persists the new token hash and
    /// reloads; `gateway` is restart-only, so the active generation kept the
    /// old (empty) token list while disk held the new one, and every reload
    /// after it found `gateway` different again. The pairing guard already
    /// holds that token, so the pairing reload adopts the persisted list into
    /// the active generation; nothing else about `gateway` is adopted.
    #[test]
    #[serial]
    fn a_save_after_pairing_does_not_report_a_gateway_restart() {
        let manager = ConfigGenerationManager::new(Config::default());
        let mut paired = (*manager.load_full()).clone();
        paired.gateway.paired_tokens = vec![crate::security::pairing::hash_token("zc_paired_in_test")];

        let pairing = manager
            .apply_runtime_config(paired.clone(), ConfigReloadTrigger::PairingPersistence)
            .expect("test: pairing reload");
        assert!(
            pairing.restart_required.is_empty(),
            "the token pairing just issued is live already: {:?}",
            pairing.restart_required
        );
        assert_eq!(
            manager.load_full().gateway.paired_tokens,
            paired.gateway.paired_tokens,
            "the active generation carries the persisted token list"
        );
        assert_eq!(pairing.applied, vec!["gateway"], "the adoption is reported as applied");

        let mut save = paired;
        save.agent.compact_context = !save.agent.compact_context;
        let report = manager
            .apply_runtime_config(save, ConfigReloadTrigger::Api)
            .expect("test: low-risk save");
        assert!(
            report.restart_required.is_empty(),
            "a save of agent.compact_context must not report a restart: {:?}",
            report.restart_required
        );
        assert_eq!(report.applied, vec!["agent"]);
    }

    /// The pairing adoption is exactly the token list: another `gateway`
    /// change in the same reload stays deferred, and a token list changed by
    /// any other writer (a hand edit picked up by the watcher) still needs a
    /// restart, because the running pairing guard never loaded it.
    #[test]
    #[serial]
    fn only_pairing_adopts_only_the_token_list() {
        let manager = ConfigGenerationManager::new(Config::default());
        let original_port = manager.load_full().gateway.port;
        let mut desired = (*manager.load_full()).clone();
        desired.gateway.paired_tokens = vec![crate::security::pairing::hash_token("zc_first")];
        desired.gateway.port = original_port.wrapping_add(1);

        let report = manager
            .apply_runtime_config(desired.clone(), ConfigReloadTrigger::PairingPersistence)
            .expect("test: pairing reload with a port change");
        assert_eq!(report.restart_required, vec!["gateway"]);
        let active = manager.load_full();
        assert_eq!(
            active.gateway.port, original_port,
            "the port change waits for a restart"
        );
        assert_eq!(active.gateway.paired_tokens, desired.gateway.paired_tokens);

        let mut hand_edit = desired;
        hand_edit.gateway.port = original_port;
        hand_edit.gateway.paired_tokens.push("operator-added-token".to_string());
        let report = manager
            .apply_runtime_config(hand_edit, ConfigReloadTrigger::FileWatcher)
            .expect("test: watcher reload");
        assert_eq!(
            report.restart_required,
            vec!["gateway"],
            "a token the pairing guard never loaded is not live until restart"
        );
        assert_eq!(manager.load_full().gateway.paired_tokens.len(), 1);
    }

    #[test]
    fn auth_and_proxy_are_rebuild_and_swap_fields() {
        assert!(is_rebuild_and_swap("auth"));
        assert!(is_rebuild_and_swap("proxy"));
        assert!(!is_process_restart_only("auth"));
        assert!(!is_process_restart_only("proxy"));
    }

    /// Serve one HTTP response on a loopback port and return its URL.
    fn one_shot_loopback_server() -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("test: bind");
        let addr = listener.local_addr().expect("test: addr");
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let mut buffer = [0_u8; 1024];
                let _ = socket.read(&mut buffer);
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
            }
        });
        format!("http://{addr}/")
    }

    /// R-INT2A F3: while a publication test's proxy is live, a concurrently
    /// running test's runtime-proxy-built client can still reach its own
    /// loopback server. Before, the published `all_proxy` applied to every
    /// service with no loopback exemption, so such a client was sent to the
    /// absent proxy and refused (this is how the A12 test hung).
    #[tokio::test]
    #[serial]
    async fn a_published_test_proxy_leaves_loopback_clients_alone() {
        let url = one_shot_loopback_server();
        let _restore = RuntimeProxyRestore::publish(test_publication_proxy("http://127.0.0.1:8080"));
        let client = crate::config::build_runtime_proxy_client("provider.compatible").expect("test: client");
        let response = tokio::time::timeout(std::time::Duration::from_secs(20), client.get(&url).send())
            .await
            .expect("test: bounded")
            .expect("test: the loopback request bypasses the test proxy");
        assert!(response.status().is_success());
    }

    #[test]
    #[serial]
    fn proxy_runtime_state_tracks_successful_generation_publication() {
        let _restore = RuntimeProxyRestore::capture();
        let initial = Config::default();
        let manager = ConfigGenerationManager::new(initial.clone());
        let events = Arc::new(Mutex::new(Vec::new()));
        let runtime = participant("runtime", events, false, false);
        manager.register_participant(&runtime);
        let mut desired = initial;
        desired.proxy = test_publication_proxy("http://127.0.0.1:8080");

        let report = manager
            .apply_runtime_config(desired.clone(), ConfigReloadTrigger::Test)
            .expect("proxy generation must publish");
        let runtime_proxy = crate::config::runtime_proxy_config();

        assert_eq!(report.rebuilt, vec!["proxy"]);
        assert!(runtime_proxy.has_any_proxy_url());
        assert_eq!(runtime_proxy.all_proxy, desired.proxy.all_proxy);
    }

    #[test]
    #[serial]
    fn proxy_runtime_state_rolls_back_when_participant_rejects_candidate() {
        let _restore = RuntimeProxyRestore::capture();
        let initial = Config::default();
        let manager = ConfigGenerationManager::new(initial.clone());
        let events = Arc::new(Mutex::new(Vec::new()));
        let rejecting = participant("rejecting", events, true, false);
        manager.register_participant(&rejecting);
        let mut desired = initial.clone();
        desired.proxy = test_publication_proxy("http://127.0.0.1:8081");

        manager
            .apply_runtime_config(desired, ConfigReloadTrigger::Test)
            .expect_err("rejected proxy candidate must roll back");
        let runtime_proxy = crate::config::runtime_proxy_config();

        assert_eq!(manager.active_generation_id(), ConfigGenerationId(0));
        assert_eq!(runtime_proxy.has_any_proxy_url(), initial.proxy.has_any_proxy_url());
        assert_eq!(runtime_proxy.all_proxy, initial.proxy.all_proxy);
    }

    #[test]
    #[serial]
    fn participant_prepare_failure_rolls_back_and_keeps_active_generation() {
        let manager = ConfigGenerationManager::new(Config::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let first = participant("first", Arc::clone(&events), false, false);
        let second = participant("second", Arc::clone(&events), true, false);
        manager.register_participant(&first);
        manager.register_participant(&second);
        let mut desired = (*manager.load_full()).clone();
        desired.default_temperature = 0.19;

        let error = manager
            .apply_runtime_config(desired.clone(), ConfigReloadTrigger::Test)
            .expect_err("second participant must reject candidate");

        assert!(error.to_string().contains("second"));
        assert_eq!(manager.active_generation_id(), ConfigGenerationId(0));
        assert_eq!(
            manager.desired().config.default_temperature,
            desired.default_temperature
        );
        assert_eq!(
            *events.lock(),
            vec!["first.prepare", "second.prepare", "first.rollback"]
        );
        assert!(manager.status().last_failure.is_some());
    }

    #[test]
    #[serial]
    fn participant_commit_failure_rolls_every_candidate_back_before_publish() {
        let manager = ConfigGenerationManager::new(Config::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let first = participant("first", Arc::clone(&events), false, false);
        let second = participant("second", Arc::clone(&events), false, true);
        manager.register_participant(&first);
        manager.register_participant(&second);
        let mut desired = (*manager.load_full()).clone();
        desired.default_temperature = 0.18;

        manager
            .apply_runtime_config(desired, ConfigReloadTrigger::Test)
            .expect_err("second participant commit must fail");

        assert_eq!(manager.active_generation_id(), ConfigGenerationId(0));
        assert_eq!(
            *events.lock(),
            vec![
                "first.prepare",
                "second.prepare",
                "first.commit",
                "second.commit",
                "second.rollback",
                "first.rollback",
            ]
        );
    }

    #[test]
    #[serial]
    fn successful_participants_finalize_only_after_publication() {
        let manager = ConfigGenerationManager::new(Config::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let participant = participant("runtime", Arc::clone(&events), false, false);
        manager.register_participant(&participant);
        let mut desired = (*manager.load_full()).clone();
        desired.default_temperature = 0.17;

        let report = manager
            .apply_runtime_config(desired, ConfigReloadTrigger::Test)
            .expect("candidate must publish");

        assert_eq!(manager.active_generation_id(), report.active_generation);
        assert_eq!(report.participant_acks, vec!["runtime"]);
        assert_eq!(
            *events.lock(),
            vec!["runtime.prepare", "runtime.commit", "runtime.finalize"]
        );
    }

    #[test]
    #[serial]
    fn concurrent_reload_attempts_are_serialized_into_distinct_generations() {
        let manager = Arc::new(ConfigGenerationManager::new(Config::default()));
        let mut handles = Vec::new();
        for temperature in [0.12, 0.13] {
            let manager = Arc::clone(&manager);
            handles.push(std::thread::spawn(move || {
                let mut desired = (*manager.load_full()).clone();
                desired.default_temperature = temperature;
                manager
                    .apply_runtime_config(desired, ConfigReloadTrigger::Test)
                    .expect("serialized reload")
                    .active_generation
            }));
        }
        let mut generations = handles
            .into_iter()
            .map(|handle| handle.join().expect("reload thread"))
            .collect::<Vec<_>>();
        generations.sort();

        assert_eq!(generations, vec![ConfigGenerationId(1), ConfigGenerationId(2)]);
        assert_eq!(manager.active_generation_id(), ConfigGenerationId(2));
    }

    #[test]
    #[serial]
    fn reload_and_supervisor_lifecycle_use_the_shared_audit_sink() {
        let manager = ConfigGenerationManager::new(Config::default());
        let participant_events = Arc::new(Mutex::new(Vec::new()));
        let runtime = participant("runtime", participant_events, false, false);
        manager.register_participant(&runtime);
        let audit_events = Arc::new(Mutex::new(Vec::new()));
        manager.register_audit_sink(Arc::new(RecordingAuditSink {
            event_types: Arc::clone(&audit_events),
        }));
        let mut desired = (*manager.load_full()).clone();
        desired.default_temperature = 0.11;
        desired.cron.max_run_history += 1;

        manager
            .apply_runtime_config(desired, ConfigReloadTrigger::Test)
            .expect("reload with controlled restart");

        assert_eq!(
            *audit_events.lock(),
            vec![
                "config.reload.prepared",
                "config.supervisor.restart_started",
                "config.supervisor.restart_completed",
                "config.reload.applied",
            ]
        );
    }

    /// PMMD-g (N-8): the revision `reload_from_disk` publishes with the
    /// desired configuration is the one of the files it loaded, not of a
    /// write that landed after the load.
    /// D-074 P1-1: the reload rebuilds the active generation through a JSON
    /// round trip (`preserve_deferred_fields`); configured values and the
    /// schema version survive the reload.
    #[test]
    fn a_reload_keeps_the_causal_settings() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, CAUSAL_FIXTURE).expect("test: causal config");
        let initial = Config::load_from_path(&config_path, dir.path().join("workspace")).expect("test: load");
        let manager = ConfigGenerationManager::new(initial);
        // `causal_tree` is deferred to restart, so the new active generation
        // carries the running section through `preserve_deferred_fields`'s
        // JSON round trip.
        std::fs::write(
            &config_path,
            CAUSAL_FIXTURE.replace("default_temperature = 0.7", "default_temperature = 0.4"),
        )
        .expect("test: edited config");

        manager
            .reload_from_disk(ConfigReloadTrigger::Test)
            .expect("test: reload");
        assert!((manager.load_full().default_temperature - 0.4).abs() < f64::EPSILON);

        for config in [manager.load_full(), Arc::clone(&manager.desired().config)] {
            let causal = &config.causal_tree;
            assert_eq!(causal.schema_version, 2);
            assert_eq!(causal.policy.max_candidates, 5);
            assert_eq!(causal.rehearsal.timeout_ms, 80);
            assert_eq!(causal.scoring.weights.w_cost, Some(0.4));
        }
    }

    /// D-079 L-4: a hand edit that turns remote decisions on is found, so
    /// the reload can warn; a local edit is not.
    #[test]
    fn a_hand_edited_remote_enablement_is_reported() {
        use crate::causal_tree::config::BackendSelection;
        let old = Config::default();
        let mut new = old.clone();
        new.causal_tree.enabled = true;
        new.causal_tree.backend.kind = BackendSelection::Fixed(crate::decision::BackendKind::CompatibleHttp);
        new.causal_tree.backend.endpoint = "https://decisions.example.com/v1".to_string();
        new.causal_tree.backend.remote_enabled = true;
        assert_eq!(
            hand_edited_remote_decisions(&old, &new),
            vec!["causal_tree.backend.remote_enabled".to_string()]
        );
        let mut local = old.clone();
        local.causal_tree.enabled = true;
        assert!(
            hand_edited_remote_decisions(&old, &local).is_empty(),
            "expected an empty value"
        );
    }

    /// D-082 N-4: the hand-edit warning is logged only when the reload is
    /// applied; a refused reload logs nothing of it.
    /// D-059: the configured-model allowlist follows the applied
    /// generation: a model named only by the new configuration becomes a
    /// label after the reload, and one no longer named becomes `unlisted`.
    /// The manager refreshes its own allowlist here, so no other test can
    /// race it.
    #[test]
    fn the_model_allowlist_follows_the_applied_generation() {
        use crate::observability::labels::{ModelAllowlist, UNLISTED_MODEL};
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "default_temperature = 0.7\n\n[causal_tree.backend]\nmodel = \"vendor/before-reload\"\n",
        )
        .expect("test: config");
        let initial = Config::load_from_path(&config_path, dir.path().join("workspace")).expect("test: load");
        let allowlist = Arc::new(ModelAllowlist::default());
        let manager = ConfigGenerationManager::with_model_allowlist(initial, Arc::clone(&allowlist));
        // A participant that rebuilds `[causal_tree]`, so the edit is applied
        // rather than deferred to a restart.
        let events = Arc::new(Mutex::new(Vec::new()));
        let rebuilding = participant("decision", events, false, false);
        manager.register_participant(&rebuilding);
        assert_eq!(allowlist.label("vendor/before-reload"), "vendor/before-reload");
        assert_eq!(allowlist.label("vendor/after-reload"), UNLISTED_MODEL);
        std::fs::write(
            &config_path,
            "default_temperature = 0.7\n\n[causal_tree.backend]\nmodel = \"vendor/after-reload\"\n",
        )
        .expect("test: edit");
        manager
            .reload_from_disk(ConfigReloadTrigger::FileWatcher)
            .expect("test: reload");
        assert_eq!(allowlist.label("vendor/after-reload"), "vendor/after-reload");
        assert_eq!(allowlist.label("vendor/before-reload"), UNLISTED_MODEL);
    }

    #[test]
    #[serial]
    fn a_hand_edited_remote_enablement_is_logged_only_when_applied() {
        #[derive(Clone, Default)]
        struct Sink(Arc<parking_lot::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }
        const REMOTE: &str = "default_temperature = 0.4\n\n[causal_tree]\nenabled = true\n\n[causal_tree.backend]\n\
                              kind = \"compatible_http\"\nendpoint = \"https://decisions.example.com/v1\"\n\
                              model = \"decider-1\"\nremote_enabled = true\n\n[[providers.models]]\n\
                              provider = \"custom:https://decisions.example.com/v1\"\nmodel = \"decider-1\"\n\
                              operations = [\"decision_endpoint\"]\n";
        let run = |fail_prepare: bool| {
            let dir = tempfile::TempDir::new().expect("test: temp dir");
            let config_path = dir.path().join("config.toml");
            std::fs::write(&config_path, "default_temperature = 0.7\n").expect("test: config");
            let initial = Config::load_from_path(&config_path, dir.path().join("workspace")).expect("test: load");
            let manager = ConfigGenerationManager::new(initial);
            let events = Arc::new(Mutex::new(Vec::new()));
            let refusing = participant("refusing", Arc::clone(&events), fail_prepare, false);
            manager.register_participant(&refusing);
            std::fs::write(&config_path, REMOTE).expect("test: edit");
            let sink = Sink::default();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(sink.clone())
                .with_ansi(false)
                .finish();
            let result = tracing::subscriber::with_default(subscriber, || {
                // Another test may have cached this callsite as disabled.
                tracing::callsite::rebuild_interest_cache();
                let result = manager.reload_from_disk(ConfigReloadTrigger::FileWatcher);
                // A control line: the capture worked for this run, so a
                // missing warning is a real absence (D-086 C-3).
                tracing::warn!("capture control line");
                result
            });
            let logs = String::from_utf8_lossy(&sink.0.lock()).to_string();
            (result.is_ok(), logs)
        };
        let (applied, logs) = run(false);
        assert!(applied);
        assert!(
            logs.contains("sends decision state off this machine")
                && logs.contains("causal_tree.backend.remote_enabled"),
            "{logs}"
        );
        assert!(
            !logs.contains("decisions.example.com"),
            "the warning names the path, never the endpoint: {logs}"
        );
        let (applied, logs) = run(true);
        assert!(!applied, "the participant refused the reload");
        assert!(logs.contains("capture control line"), "the capture must work: {logs}");
        assert!(!logs.contains("sends decision state off this machine"), "{logs}");
    }

    /// CDP-04 (V6.07 hot-reload half): a hand edit that points the decision
    /// backend at a remote host without `remote_enabled` does not activate;
    /// the running generation is kept.
    #[test]
    fn a_remote_decision_endpoint_without_the_remote_switch_keeps_the_generation() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "default_temperature = 0.7\n").expect("test: config");
        let initial = Config::load_from_path(&config_path, dir.path().join("workspace")).expect("test: load");
        let manager = ConfigGenerationManager::new(initial);
        let active = manager.active_generation_id();
        std::fs::write(
            &config_path,
            "default_temperature = 0.4\n\n[causal_tree.backend]\nkind = \"compatible_http\"\n\
             endpoint = \"https://decisions.example.com/v1\"\n",
        )
        .expect("test: edit");
        let error = manager
            .reload_from_disk(ConfigReloadTrigger::Test)
            .expect_err("test: refused");
        let message = format!("{error:#}");
        assert!(message.contains("remote_enabled"), "{message}");
        assert!(!message.contains("decisions.example.com"), "{message}");
        assert_eq!(manager.active_generation_id(), active);
        assert!((manager.load_full().default_temperature - 0.7).abs() < f64::EPSILON);
        assert!(!manager.load_full().causal_tree.backend.remote_enabled);
    }

    /// CDP-12 (R-CXR P0-B t2): a hand edit to a decision backend the strict
    /// selection refuses (here a model nobody declared for
    /// `decision_endpoint`) leaves the running generation active.
    #[test]
    fn an_invalid_causal_backend_leaves_the_prior_generation_active() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "default_temperature = 0.7\n").expect("test: config");
        let initial = Config::load_from_path(&config_path, dir.path().join("workspace")).expect("test: load");
        let manager = ConfigGenerationManager::new(initial);
        let events = Arc::new(Mutex::new(Vec::new()));
        let rebuilding = participant("decision", events, false, false);
        manager.register_participant(&rebuilding);
        let active = manager.active_generation_id();
        std::fs::write(
            &config_path,
            "default_temperature = 0.4\n\n[causal_tree.backend]\nkind = \"compatible_http\"\n\
             endpoint = \"http://127.0.0.1:8900/v1\"\nmodel = \"undeclared-model\"\n",
        )
        .expect("test: edit");
        let error = manager
            .reload_from_disk(ConfigReloadTrigger::Test)
            .expect_err("test: refused");
        assert!(format!("{error:#}").contains("causal_tree.backend"), "{error:#}");
        assert_eq!(manager.active_generation_id(), active);
        assert!((manager.load_full().default_temperature - 0.7).abs() < f64::EPSILON);

        // Declared for decision_endpoint, the same backend activates.
        std::fs::write(
            &config_path,
            "default_temperature = 0.4\n\n[causal_tree.backend]\nkind = \"compatible_http\"\n\
             endpoint = \"http://127.0.0.1:8900/v1\"\nmodel = \"undeclared-model\"\n\n\
             [[providers.models]]\nprovider = \"custom:http://127.0.0.1:8900/v1\"\nmodel = \"undeclared-model\"\n\
             operations = [\"decision_endpoint\"]\n",
        )
        .expect("test: edit");
        manager
            .reload_from_disk(ConfigReloadTrigger::Test)
            .expect("test: applied");
        assert_ne!(manager.active_generation_id(), active);
    }

    /// D-094 C-1: a daemon started with a decision section the check
    /// refuses starts, and a later reload that does not touch
    /// `[causal_tree]` is applied; one that does is still refused.
    #[test]
    fn an_invalid_decision_section_never_blocks_startup_or_unrelated_reloads() {
        const INVALID: &str = "\n[causal_tree]\nenabled = true\n\n[causal_tree.backend]\nkind = \"compatible_http\"\n\
                               endpoint = \"http://127.0.0.1:8900/v1\"\nmodel = \"undeclared-model\"\n";
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, format!("default_temperature = 0.7\n{INVALID}")).expect("test: config");
        let initial = Config::load_from_path(&config_path, dir.path().join("workspace")).expect("test: load");
        let manager = ConfigGenerationManager::new(initial);
        let active = manager.active_generation_id();
        // A declaration for another provider: the check runs, finds the
        // section already refused, and does not hold the change back.
        const UNRELATED: &str = "\n[[providers.models]]\nprovider = \"openrouter\"\nmodel = \"vendor/chat-model\"\n\
                                 operations = [\"chat\"]\n";
        std::fs::write(&config_path, format!("default_temperature = 0.4\n{INVALID}{UNRELATED}")).expect("test: edit");
        manager
            .reload_from_disk(ConfigReloadTrigger::Test)
            .expect("test: an unrelated change is applied");
        assert_ne!(manager.active_generation_id(), active);
        assert!((manager.load_full().default_temperature - 0.4).abs() < f64::EPSILON);
        let active = manager.active_generation_id();
        std::fs::write(
            &config_path,
            format!(
                "default_temperature = 0.4\n{}{UNRELATED}",
                INVALID.replace("undeclared-model", "other-model")
            ),
        )
        .expect("test: edit");
        let error = manager
            .reload_from_disk(ConfigReloadTrigger::Test)
            .expect_err("test: a change to the section is refused");
        assert!(format!("{error:#}").contains("causal_tree.backend"), "{error:#}");
        assert_eq!(manager.active_generation_id(), active);
    }

    const CAUSAL_FIXTURE: &str = r#"default_temperature = 0.7

[causal_tree]
schema_version = 1
enabled = false

[causal_tree.policy]
max_candidates = 5

[causal_tree.rehearsal]
timeout_ms = 80

[causal_tree.scoring.weights]
w_cost = 0.4
"#;

    #[test]
    fn a_reload_publishes_the_revision_of_the_files_it_loaded() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "default_temperature = 0.7\n").expect("test: config");
        let mut initial = Config::default();
        initial.config_path = config_path.clone();
        initial.workspace_dir = dir.path().join("workspace");
        let manager = ConfigGenerationManager::new(initial);
        let loaded_revision = crate::config::files::current_config_revision(&config_path).expect("test: revision");
        let writer_path = config_path.clone();
        *AFTER_SNAPSHOT_HOOK.lock() = Some((
            config_path,
            Box::new(move || {
                std::fs::write(&writer_path, "default_temperature = 0.4\n").expect("test: late write");
            }),
        ));

        manager
            .reload_from_disk(ConfigReloadTrigger::Test)
            .expect("test: reload");

        let desired = manager.desired();
        assert!((desired.config.default_temperature - 0.7).abs() < f64::EPSILON);
        assert_eq!(
            desired.source_revision.fingerprint_sha256, loaded_revision,
            "the desired configuration must carry the revision it was loaded from"
        );
    }
}

#[cfg(test)]
mod pinned_parent_process_binding_tests {
    use super::*;

    fn fixture() -> anyhow::Result<(tempfile::TempDir, ConfigGeneration, Arc<crate::decision::DecisionKey>)> {
        let home = tempfile::tempdir()?;
        let config = Config {
            config_path: home.path().join("config.toml"),
            workspace_dir: home.path().join("workspace"),
            ..Config::default()
        };
        std::fs::write(&config.config_path, toml::to_string(&config)?)?;
        let generation = ConfigGeneration {
            id: ConfigGenerationId(7),
            source_revision: ConfigSourceRevision::from_config(&config)?,
            effective: Arc::new(config),
            applied_at: Utc::now(),
            trigger: ConfigReloadTrigger::Test,
            deferred_changes: Arc::from([]),
        };
        let transport = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"prx-parent-binding-fixture");
        let key =
            crate::decision::DecisionKey::from_worker_channel(&transport, "prx-workspace", "prx-run", "prx-config")?;
        Ok((home, generation, key))
    }

    #[test]
    fn pinned_parent_binding_survives_removed_source_but_child_restore_rejects() -> anyhow::Result<()> {
        let (_home, generation, key) = fixture()?;
        let binding = generation.bind_process(&key)?;
        generation.verify_pinned_process_binding(&binding, &key)?;
        ConfigGeneration::restore_process(generation.effective.as_ref().clone(), &binding, &key)?;
        std::fs::remove_file(&generation.effective.config_path)?;
        generation.verify_pinned_process_binding(&binding, &key)?;
        assert!(ConfigGeneration::restore_process(generation.effective.as_ref().clone(), &binding, &key).is_err());
        Ok(())
    }

    #[test]
    fn pinned_parent_binding_survives_changed_source_but_child_restore_rejects() -> anyhow::Result<()> {
        let (_home, generation, key) = fixture()?;
        let binding = generation.bind_process(&key)?;
        std::fs::write(&generation.effective.config_path, "default_temperature = 0.25\n")?;
        generation.verify_pinned_process_binding(&binding, &key)?;
        assert!(ConfigGeneration::restore_process(generation.effective.as_ref().clone(), &binding, &key).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn process_binding_accepts_a_filesystem_alias_for_the_same_source() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;

        let (home, generation, key) = fixture()?;
        std::fs::create_dir_all(&generation.effective.workspace_dir)?;
        let binding = generation.bind_process(&key)?;
        let alias = home.path().join("source-alias");
        symlink(home.path(), &alias)?;
        let mut child = generation.effective.as_ref().clone();
        child.config_path = alias.join("config.toml");
        child.workspace_dir = alias.join("workspace");

        let restored = ConfigGeneration::restore_process(child, &binding, &key)?;
        assert_eq!(restored.id, generation.id);
        assert_eq!(restored.source_revision, generation.source_revision);
        Ok(())
    }

    #[test]
    fn pinned_parent_binding_rejects_metadata_snapshot_paths_and_key_mutation() -> anyhow::Result<()> {
        let (_home, generation, key) = fixture()?;
        let binding = generation.bind_process(&key)?;
        let mut changed = generation.clone();
        changed.id = ConfigGenerationId(8);
        assert!(changed.verify_pinned_process_binding(&binding, &key).is_err());
        let mut changed = generation.clone();
        changed.source_revision.disk_generation = None;
        assert!(changed.verify_pinned_process_binding(&binding, &key).is_err());
        let mut changed = generation.clone();
        changed.source_revision.fingerprint_sha256 = "0".repeat(64);
        assert!(changed.verify_pinned_process_binding(&binding, &key).is_err());
        let mut changed = generation.clone();
        changed.applied_at += chrono::Duration::seconds(1);
        assert!(changed.verify_pinned_process_binding(&binding, &key).is_err());
        let mut changed = generation.clone();
        changed.trigger = ConfigReloadTrigger::Startup;
        assert!(changed.verify_pinned_process_binding(&binding, &key).is_err());
        let mut changed = generation.clone();
        changed.deferred_changes = vec![DeferredConfigChange {
            field: "prx-field".to_owned(),
            reason: "prx-reason".to_owned(),
        }]
        .into();
        assert!(changed.verify_pinned_process_binding(&binding, &key).is_err());
        for field in ["workspace", "config", "effective"] {
            let mut changed = generation.clone();
            let config = Arc::make_mut(&mut changed.effective);
            match field {
                "workspace" => config.workspace_dir = std::path::PathBuf::from("/prx-other-workspace"),
                "config" => config.config_path = std::path::PathBuf::from("/prx-other-config"),
                _ => config.default_temperature += 0.1,
            }
            assert!(changed.verify_pinned_process_binding(&binding, &key).is_err());
        }
        let transport = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"prx-wrong-binding-key");
        let wrong =
            crate::decision::DecisionKey::from_worker_channel(&transport, "prx-workspace", "prx-run", "prx-config")?;
        assert!(generation.verify_pinned_process_binding(&binding, &wrong).is_err());
        let transport = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"prx-parent-binding-fixture");
        for (workspace, run, config) in [
            ("prx-other-workspace", "prx-run", "prx-config"),
            ("prx-workspace", "prx-other-run", "prx-config"),
            ("prx-workspace", "prx-run", "prx-other-config"),
        ] {
            let different_scope =
                crate::decision::DecisionKey::from_worker_channel(&transport, workspace, run, config)?;
            assert!(
                generation
                    .verify_pinned_process_binding(&binding, &different_scope)
                    .is_err()
            );
        }
        let mut forged = binding;
        forged.seal = [0; 32];
        assert!(generation.verify_pinned_process_binding(&forged, &key).is_err());
        Ok(())
    }
}

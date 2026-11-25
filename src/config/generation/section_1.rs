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

        let encoded = serde_json::to_vec(&normalized_process_config(config)?)?;
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

/// A worker cannot replace the working root selected by its parent.
#[derive(Debug, thiserror::Error)]
#[error("worker working root differs from signed parent configuration")]
pub(crate) struct ProcessWorkingRootMismatch;

/// The configuration a worker loaded from disk is not the effective
/// configuration its parent turn runs on and sealed (a change that has not been
/// activated in the parent yet). The worker never runs under a seal its own
/// configuration does not reproduce.
#[derive(Debug, thiserror::Error)]
#[error("config_changed: the configuration on disk differs from the effective configuration this parent turn runs on; restart the parent chat and retry the process child")]
pub(crate) struct ProcessConfigurationChanged;

/// Metadata, a working root override and a keyed effective snapshot digest.
/// Credential values and other configuration overrides are not transported.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessGenerationBinding {
    /// Parent in-memory working root, authenticated by the effective-config seal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_root: Option<PathBuf>,
    pub id: ConfigGenerationId,
    pub source_revision: ConfigSourceRevision,
    pub applied_at: DateTime<Utc>,
    pub trigger: ConfigReloadTrigger,
    pub deferred_changes: Vec<DeferredConfigChange>,
    pub seal: [u8; 32],
}

// Disk migration materializes the inherited backend. Fingerprints and seals
// must bind the same representation while retaining explicit overrides.
fn normalized_process_config(config: &Config) -> anyhow::Result<serde_json::Value> {
    let mut normalized = serde_json::to_value(config)?;
    let backend = crate::config::schema::decision_engine::effective_engine_backend(config);
    let mut engine = config.decision_engine.clone();
    engine.backend = Some(backend.clone());
    if let Some(root) = normalized.as_object_mut() {
        root.insert("decision_engine".into(), serde_json::to_value(engine)?);
    }
    normalized.sort_all_objects();
    Ok(normalized)
}

impl ProcessGenerationBinding {
    pub(crate) fn signing_payload(&self, config: &Config) -> anyhow::Result<Vec<u8>> {
        // Config excludes both paths from serde; bind them explicitly. Sort
        // nested maps so process-specific HashMap iteration cannot change a tag.
        let workspace = process_path_identity(&config.workspace_dir)?;
        let config_path = process_path_identity(&config.config_path)?;
        let normalized = normalized_process_config(config)?;
        let mut value = serde_json::to_value((
            self.id,
            &self.source_revision,
            self.applied_at,
            self.trigger,
            &self.deferred_changes,
            workspace,
            config_path,
        ))?;
        value.sort_all_objects();
        value
            .as_array_mut()
            .context("process signing metadata must serialize as an array")?
            .push(normalized);
        Ok(serde_json::to_vec(&value)?)
    }
}

impl ConfigGeneration {
    /// Bind a freshly loaded mailbox-child configuration to the disk revision
    /// that a relaunched process worker must verify. Its effective snapshot is
    /// still sealed independently by `bind_process`.
    pub(crate) fn for_mailbox_child(config: &Config) -> anyhow::Result<Arc<Self>> {
        let mut generation = Self::for_child(config)?.as_ref().clone();
        generation.source_revision = ConfigSourceRevision::from_config(config)?;
        Ok(Arc::new(generation))
    }

    /// Pin an ordinary child without startup globals or disk reloads.
    pub(crate) fn for_child(config: &Config) -> anyhow::Result<Arc<Self>> {
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
        key: &crate::security::runtime_key::DecisionKey,
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
            working_root: self.effective.runtime.workdir.clone(),
            id: self.id,
            source_revision: self.source_revision.clone(),
            applied_at: self.applied_at,
            trigger: self.trigger,
            deferred_changes: self.deferred_changes.to_vec(),
            seal: [0; 32],
        };
        binding.seal = key.hmac(
            crate::security::runtime_key::KeyDomain::StateFingerprint,
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
        key: &crate::security::runtime_key::DecisionKey,
    ) -> anyhow::Result<()> {
        let deferred_changes_match = self.deferred_changes.as_ref() == binding.deferred_changes.as_slice();
        anyhow::ensure!(
            self.effective.runtime.workdir == binding.working_root
                && self.id == binding.id
                && self.source_revision == binding.source_revision
                && self.applied_at == binding.applied_at
                && self.trigger == binding.trigger
                && deferred_changes_match,
            "pinned process generation differs from launch"
        );
        anyhow::ensure!(
            key.verify_hmac(
                crate::security::runtime_key::KeyDomain::StateFingerprint,
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

    /// Restore only an exact snapshot. A configuration with different effective
    /// values, a forged seal or another working root cannot borrow the parent's
    /// generation id; a disk revision that moved on without changing the
    /// effective values can.
    pub(crate) fn restore_process(
        config: Config,
        binding: &ProcessGenerationBinding,
        key: &crate::security::runtime_key::DecisionKey,
    ) -> anyhow::Result<Arc<Self>> {
        if config.runtime.workdir != binding.working_root {
            return Err(ProcessWorkingRootMismatch.into());
        }
        // The keyed seal binds the parent's complete effective configuration
        // together with its source revision, so it is the comparison that
        // carries the integrity. The revision on disk is deliberately not
        // compared: it moves on every configuration write (its generation
        // counter advances even when the bytes are restored identically),
        // while the parent's active generation keeps the revision it was
        // activated from whenever the effective configuration did not change.
        anyhow::ensure!(
            key.verify_hmac(
                crate::security::runtime_key::KeyDomain::StateFingerprint,
                &[b"process-effective-config-v1", &binding.signing_payload(&config)?],
                &binding.seal
            ),
            ProcessConfigurationChanged
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
    runtime_writes_frozen: AtomicBool,
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
        crate::context::sources::set_share_host_identity(generation.effective.agent.prompt.share_host_identity);
        if let Some(warning) = crate::config::manage::validate::decision_startup_check(&generation.effective) {
            tracing::warn!("{warning}");
        }
        if generation.effective.decision_engine.enabled
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
            runtime_writes_frozen: AtomicBool::new(false),
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
        crate::context::sources::set_share_host_identity(initial.agent.prompt.share_host_identity);
        // Startup never fails for the decision plane: a refused backend
        // runs builtin-only, and says so (D-094 C-1).
        if let Some(warning) = crate::config::manage::validate::decision_startup_check(&initial) {
            tracing::warn!("{warning}");
        }
        // Decision credentials left unreferenced are reported (D-094 C-5);
        // a disabled engine does not read the credential store.
        if initial.decision_engine.enabled
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
            runtime_writes_frozen: AtomicBool::new(false),
            reload_in_progress: AtomicBool::new(false),
            last_report: RwLock::new(None),
            last_failure: RwLock::new(None),
            audit_sinks: RwLock::new(Vec::new()),
            model_allowlist,
        }
    }

    /// A runtime may only narrow this policy; no Config field clears it.
    pub(crate) fn freeze_runtime_writes(&self) {
        self.runtime_writes_frozen.store(true, Ordering::Release);
    }
    pub(crate) fn ensure_runtime_writable(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.runtime_writes_frozen.load(Ordering::Acquire),
            "configuration is frozen for controlled experiments"
        );
        Ok(())
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
        self.ensure_runtime_writable()?;
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
        self.ensure_runtime_writable()?;
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
            crate::context::sources::set_share_host_identity(generation.effective.agent.prompt.share_host_identity);
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
            | "plugins"
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
            tracing::warn!(error = %crate::security::redaction::redact_provider_text(&format!("{error:#}")), "could not compare the reloaded configuration");
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

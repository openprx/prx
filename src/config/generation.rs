//! Process-level configuration generation ownership.
//!
//! `ConfigGenerationManager` is the only runtime publisher of effective
//! configuration. Readers pin an immutable [`ConfigGeneration`] (or its
//! `Arc<Config>` payload) for the lifetime of one operation.

use super::{
    files::{compute_config_fingerprint_gated, compute_config_revision_gated},
    schema::Config,
};
use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

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
    desired: ArcSwap<DesiredConfigState>,
    reload_lock: Mutex<()>,
    next_generation: AtomicU64,
    participants: RwLock<Vec<Weak<dyn ConfigGenerationParticipant>>>,
    reload_in_progress: AtomicBool,
    last_report: RwLock<Option<ConfigApplyReport>>,
    last_failure: RwLock<Option<ConfigReloadFailure>>,
    audit_sinks: RwLock<Vec<Arc<dyn ConfigGenerationAuditSink>>>,
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
        crate::config::set_runtime_proxy_config(initial.proxy.clone());
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
        Self {
            active: ArcSwap::from(generation),
            desired: ArcSwap::from(desired),
            reload_lock: Mutex::new(()),
            next_generation: AtomicU64::new(1),
            participants: RwLock::new(Vec::new()),
            reload_in_progress: AtomicBool::new(false),
            last_report: RwLock::new(None),
            last_failure: RwLock::new(None),
            audit_sinks: RwLock::new(Vec::new()),
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
        let fresh = match load_stable_config_snapshot(&config_path, current.workspace_dir.clone()) {
            Ok(fresh) => fresh,
            Err(error) => {
                self.record_failure(trigger, None, &error);
                return Err(error);
            }
        };
        let revision = match ConfigSourceRevision::from_config(&fresh) {
            Ok(revision) => revision,
            Err(error) => {
                self.record_failure(trigger, None, &error);
                return Err(error);
            }
        };
        self.apply_config(fresh, revision, trigger)
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
        let old = old_generation.effective.as_ref();
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
        let applied = changed
            .iter()
            .filter(|field| {
                !is_process_restart_only(field) && !is_rebuild_and_swap(field) && !is_controlled_restart(field)
            })
            .cloned()
            .collect::<Vec<_>>();

        self.desired.store(Arc::new(DesiredConfigState {
            source_revision: source_revision.clone(),
            config: Arc::new(desired.clone()),
            observed_at: Utc::now(),
        }));

        let effective = preserve_deferred_fields(old, desired, &restart_required)?;
        let effective_changed = changed_top_level_fields(old, &effective)?;
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
            let id = ConfigGenerationId(self.next_generation.fetch_add(1, Ordering::SeqCst));
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

fn load_stable_config_snapshot(config_path: &Path, workspace_dir: std::path::PathBuf) -> anyhow::Result<Config> {
    const MAX_ATTEMPTS: usize = 3;

    for attempt in 1..=MAX_ATTEMPTS {
        let before = compute_config_fingerprint_gated(config_path)?;
        let fresh = Config::load_from_path(config_path, workspace_dir.clone())?;
        let after = compute_config_fingerprint_gated(config_path)?;
        if before == after {
            return Ok(fresh);
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

pub(crate) fn is_process_restart_only(field: &str) -> bool {
    matches!(
        field,
        "runtime" | "memory" | "storage" | "tunnel" | "gateway" | "modules" | "observability" | "mcp_server" | "a2a"
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

    #[test]
    fn reload_report_classifies_runtime_changes() {
        let manager = ConfigGenerationManager::new(Config::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let runtime = participant("runtime", events, false, false);
        manager.register_participant(&runtime);
        let mut desired = (*manager.load_full()).clone();
        desired.default_temperature = 0.22;
        desired.default_model = Some("example/model".to_string());
        desired.cron.enabled = !desired.cron.enabled;

        let report = manager
            .apply_runtime_config(desired, ConfigReloadTrigger::Test)
            .expect("apply config");

        assert_eq!(report.applied, vec!["default_temperature"]);
        assert_eq!(report.rebuilt, vec!["default_model"]);
        assert_eq!(report.restarted, vec!["cron"]);
        assert!(report.restart_required.is_empty());
    }

    #[test]
    fn auth_and_proxy_are_rebuild_and_swap_fields() {
        assert!(is_rebuild_and_swap("auth"));
        assert!(is_rebuild_and_swap("proxy"));
        assert!(!is_process_restart_only("auth"));
        assert!(!is_process_restart_only("proxy"));
    }

    #[test]
    #[serial]
    fn proxy_runtime_state_tracks_successful_generation_publication() {
        let initial = Config::default();
        let manager = ConfigGenerationManager::new(initial.clone());
        let events = Arc::new(Mutex::new(Vec::new()));
        let runtime = participant("runtime", events, false, false);
        manager.register_participant(&runtime);
        let mut desired = initial.clone();
        desired.proxy.enabled = true;
        desired.proxy.all_proxy = Some("http://127.0.0.1:8080".to_string());

        let report = manager
            .apply_runtime_config(desired.clone(), ConfigReloadTrigger::Test)
            .expect("proxy generation must publish");
        let runtime_proxy = crate::config::runtime_proxy_config();

        assert_eq!(report.rebuilt, vec!["proxy"]);
        assert!(runtime_proxy.enabled);
        assert_eq!(runtime_proxy.all_proxy, desired.proxy.all_proxy);
        crate::config::set_runtime_proxy_config(initial.proxy);
    }

    #[test]
    #[serial]
    fn proxy_runtime_state_rolls_back_when_participant_rejects_candidate() {
        let initial = Config::default();
        let manager = ConfigGenerationManager::new(initial.clone());
        let events = Arc::new(Mutex::new(Vec::new()));
        let rejecting = participant("rejecting", events, true, false);
        manager.register_participant(&rejecting);
        let mut desired = initial.clone();
        desired.proxy.enabled = true;
        desired.proxy.all_proxy = Some("http://127.0.0.1:8081".to_string());

        manager
            .apply_runtime_config(desired, ConfigReloadTrigger::Test)
            .expect_err("rejected proxy candidate must roll back");
        let runtime_proxy = crate::config::runtime_proxy_config();

        assert_eq!(manager.active_generation_id(), ConfigGenerationId(0));
        assert_eq!(runtime_proxy.enabled, initial.proxy.enabled);
        assert_eq!(runtime_proxy.all_proxy, initial.proxy.all_proxy);
        crate::config::set_runtime_proxy_config(initial.proxy);
    }

    #[test]
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
        desired.cron.enabled = !desired.cron.enabled;

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
}

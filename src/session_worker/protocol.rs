mod operator_completion;
pub use operator_completion::OperatorWorkerCompletionEvidence;
mod ordinary_completion;
pub use ordinary_completion::OrdinaryWorkerCompletionEvidence;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub use crate::tools::capability::child::wire::{
    WorkerChildCapabilityManifest, WorkerChildCapabilityRef, WorkerDelegationAuthority,
};

/// Original PMM selection, covered by the complete worker transport signature.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerProviderSelection {
    pub provider: String,
    pub model: String,
    pub credential: crate::providers::credentials::CredentialRef,
}
impl std::fmt::Debug for WorkerProviderSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerProviderSelection").finish_non_exhaustive()
    }
}

/// Source identifiers covered by the outer worker signature. These are lookup
/// bindings, not authority: admission must re-read and verify the parent decision
/// and durable child input before constructing a source proof.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerChildSource {
    pub input_event_id: String,
    pub provider_selection: WorkerProviderSelection,
    pub input_event_mac: [u8; 32],
    pub parent_source: crate::runtime::child_source::parent::ParentSourceSnapshot,
    pub provider_catalog: crate::providers::catalog::process_snapshot::ProcessProviderCatalog,
    pub parent_store_workspace: String,
    pub parent_decision_digest: String,
    pub parent_constraints: crate::tools::capability::child::wire::WorkerParentConstraints,
    pub config_generation: crate::config::generation::ProcessGenerationBinding,
    pub catalog: crate::tools::capability::catalog::process_snapshot::ProcessCatalogSnapshot,
    /// Authenticated journal version. Absent means a pre-upgrade (legacy) child,
    /// and the field is omitted from JSON so historical manifests stay byte-identical.
    /// Version 3 and later require the authority journal row to be present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_version: Option<u8>,
}

/// Optional durable input evidence for an ordinary worker. Absence means
/// recording was disabled; it must never be replaced by a synthetic event ID.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerRecordedInput {
    pub event_id: String,
    pub event_mac: [u8; 32],
}

/// Ordinary process provenance, sealed in the worker-channel key domain.
/// This deliberately carries no durable parent decision/store assertion.
/// Deserialization is transport parsing only; outer worker authentication,
/// scope subset checks and each snapshot seal must precede source issuance.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerOrdinarySource {
    pub attempt_id: Option<String>,
    pub lease_epoch: Option<i64>,
    pub input: Option<WorkerRecordedInput>,
    pub provider_selection: WorkerProviderSelection,
    pub parent_source: crate::runtime::child_source::parent::ParentSourceSnapshot,
    pub provider_catalog: crate::providers::catalog::process_snapshot::ProcessProviderCatalog,
    pub parent_constraints: crate::tools::capability::child::wire::WorkerParentConstraints,
    pub capability_manifest: WorkerChildCapabilityManifest,
    pub config_generation: crate::config::generation::ProcessGenerationBinding,
    pub catalog: crate::tools::capability::catalog::process_snapshot::ProcessCatalogSnapshot,
}

impl std::fmt::Debug for WorkerOrdinarySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerOrdinarySource").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for WorkerRecordedInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerRecordedInput").finish_non_exhaustive()
    }
}

/// Root capability ceiling captured from an explicit local operator source.
/// The publication ID is a transport identity, not a parent model decision.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerOperatorPublication {
    pub publication_id: String,
    pub owner: String,
    pub workspace: String,
    pub level: crate::security::AutonomyLevel,
    pub max_cost_cents: u32,
    pub allowed: Vec<crate::tools::capability::child::wire::WorkerChildCapabilityRef>,
    pub catalog_generation: [u8; 32],
    pub seal: [u8; 32],
}

/// Transport parsing alone grants no execution authority. The receiver must
/// authenticate the channel, all snapshots, actual input and executor parity.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerOperatorSource {
    pub attempt_id: Option<String>,
    pub lease_epoch: Option<i64>,
    pub input: Option<WorkerRecordedInput>,
    pub provider_selection: WorkerProviderSelection,
    pub operator_source: crate::runtime::child_source::parent::ParentSourceSnapshot,
    pub publication: WorkerOperatorPublication,
    pub provider_catalog: crate::providers::catalog::process_snapshot::ProcessProviderCatalog,
    pub config_generation: crate::config::generation::ProcessGenerationBinding,
    pub catalog: crate::tools::capability::catalog::process_snapshot::ProcessCatalogSnapshot,
}

impl std::fmt::Debug for WorkerOperatorSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerOperatorSource").finish_non_exhaustive()
    }
}

/// Manifest passed from parent `sessions_spawn` to `session-worker`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerManifest {
    #[serde(default)]
    pub parent_capability: Option<String>,
    /// Included in the outer worker HMAC payload whenever present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_manifest: Option<WorkerChildCapabilityManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_source: Option<WorkerChildSource>,
    /// Mutually exclusive with durable child authority; covered by the outer HMAC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinary_source: Option<WorkerOrdinarySource>,
    /// Explicit operator root provenance, mutually exclusive with delegated sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_source: Option<WorkerOperatorSource>,
    pub run_id: String,
    pub task: String,
    pub provider_name: String,
    pub model: String,
    pub api_key: Option<String>,
    /// Selected model route, when the parent resolved a `hint:` model.
    /// The worker verifies its destination against the sealed config.
    #[serde(default)]
    pub route_hint: Option<String>,
    /// Original provider selection and final credential reference, sealed
    /// separately from `agent_id` (which controls execution identity).
    #[serde(default)]
    pub route_source_provider: Option<String>,
    #[serde(default)]
    pub route_credential: Option<crate::providers::credentials::CredentialRef>,
    /// True only when that route selected the root connection as a unit.
    /// A byte-identical route key cannot acquire the root URL by accident.
    #[serde(default)]
    pub route_uses_root_connection: bool,
    /// A direct-model child may use the root URL and rotation keys only when
    /// its parent selected the root connection, independent of key equality.
    #[serde(default)]
    pub direct_uses_root_connection: bool,
    pub temperature: f64,
    /// Explicit caller temperature, distinct from the global default; covered
    /// by the existing manifest HMAC and required to match `temperature`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_override: Option<f64>,
    /// Canonical configuration directory selected by the parent process.
    #[serde(default)]
    pub config_dir: PathBuf,
    /// SHA-256 generation of config.toml plus every config.d TOML fragment.
    #[serde(default)]
    pub config_generation: String,
    /// Runtime configuration generation pinned by the parent turn.
    #[serde(default)]
    pub runtime_config_generation_id: Option<u64>,
    /// Stable source revision associated with the pinned runtime generation.
    #[serde(default)]
    pub runtime_config_source_revision: Option<String>,
    pub workspace_dir: PathBuf,
    pub memory_db_path: PathBuf,
    #[serde(default)]
    pub memory_workspace_id: Option<String>,
    #[serde(default)]
    pub memory_strategy: Option<String>,
    /// Effective parent memory backend selected by the parent process. The
    /// worker verifies this against its sealed config generation before opening
    /// shared memory, so shared mode cannot silently fall back to SQLite.
    #[serde(default)]
    pub memory_backend: String,
    #[serde(default)]
    pub shared_memory_db_path: Option<PathBuf>,
    #[serde(default)]
    pub worker_memory_db_path: Option<PathBuf>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub persona_id: Option<String>,
    #[serde(default)]
    pub memory_event_recording: crate::memory::MemoryEventRecording,
    pub allowed_tools: Vec<String>,
    /// Tool names the spawning channel surface hides, carried across the
    /// process boundary.
    ///
    /// `allowed_tools` is the *agent's* contract and is usually empty, meaning
    /// "inherit everything". The channel surface's `channel_exclude` list is a
    /// different thing: it is the boundary between a conversation partner on an
    /// IM channel and the operations surface, and until 0.8.131 it stopped at
    /// the OS process boundary — a `mode: "process"` spawn, or a worker resumed
    /// from this manifest, got `gateway`, `config_reload`, `proxy_config`,
    /// `nodes` and `git_operations` back. The list is covered by the manifest
    /// capability seal, so a worker cannot be started with it stripped.
    #[serde(default)]
    pub channel_excluded_tools: Vec<String>,
    pub system_prompt: Option<String>,
    pub identity_dir: Option<String>,
    pub scope_sender: Option<String>,
    pub scope_channel: Option<String>,
    pub scope_chat_type: Option<String>,
    pub scope_chat_id: Option<String>,
    #[serde(default)]
    pub owner_id: Option<String>,
    #[serde(default)]
    pub owner_isolated: bool,
    #[serde(default)]
    pub topic_id: Option<String>,
    #[serde(default)]
    pub parent_task_id: Option<String>,
    #[serde(default)]
    pub source_message_event_id: Option<String>,
    #[serde(default)]
    pub spawn_depth: usize,
    #[serde(default)]
    pub session_scope_key: String,
    #[serde(default)]
    pub parent_run_id: Option<String>,
    /// Sealed remote provenance for a chained turn. Absent on local operator work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited_provenance: Option<crate::messaging::InheritedProvenance>,
    #[serde(default)]
    pub compaction_config: Option<crate::config::AgentCompactionConfig>,
}

impl WorkerManifest {
    /// Seal remote provenance into this manifest. FOUNDATION API; spawn sites
    /// still pass `None` until MSG-CORE / CHILD-CAP wire a chained turn.
    pub fn seal_inherited_provenance(&mut self, provenance: crate::messaging::InheritedProvenance) {
        self.inherited_provenance = Some(provenance);
    }
}

/// Compute a stable generation for the complete on-disk configuration source.
///
/// Both the parent and worker call this exact implementation. File names and
/// lengths are framed before their bytes so distinct source layouts cannot hash
/// as the same concatenated byte stream.
pub fn config_source_generation(config_dir: &Path) -> anyhow::Result<String> {
    let config_path = config_dir.join("config.toml");
    crate::config::files::with_consistent_config_snapshot(&config_path, || config_source_generation_once(config_dir))
}

fn config_source_generation_once(config_dir: &Path) -> anyhow::Result<String> {
    let config_path = config_dir.join("config.toml");
    if !config_path.is_file() {
        anyhow::bail!("session-worker config source is missing: {}", config_path.display());
    }

    let mut source_paths = vec![config_path.clone()];
    source_paths.extend(crate::config::files::list_config_fragment_paths(&config_path)?);
    let mut hasher = Sha256::new();
    for path in source_paths {
        let relative = path.strip_prefix(config_dir).map_err(|error| {
            anyhow::anyhow!(
                "config source {} is outside {}: {error}",
                path.display(),
                config_dir.display()
            )
        })?;
        let relative = relative.to_string_lossy();
        let bytes = std::fs::read(&path)
            .map_err(|error| anyhow::anyhow!("failed to read config source {}: {error}", path.display()))?;
        hasher.update((relative.len() as u64).to_be_bytes());
        hasher.update(relative.as_bytes());
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Control frame streamed from the parent to a **running** `session-worker` on
/// stdin, one JSON object per line.
///
/// Line 1 of the worker's stdin is always the [`WorkerManifest`]; every later
/// line is one of these frames. The manifest is the only authenticated part of
/// the stream (sealed HMAC + absolute expiry, see
/// `session_worker::runner::validate_worker_capability_with_env`); control
/// frames inherit their trust from the pipe itself, which only the spawning
/// parent holds a write end of. They therefore carry no capability material and
/// can never widen what the sealed manifest already granted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerControlFrame {
    /// Inject an operator message into the worker's running conversation.
    Steer { message: String },
    /// Parent-to-worker Data envelope (MSG-5). Types only in FOUNDATION.
    Message { envelope: crate::messaging::PrxMessage },
    /// Answer to a worker broker call (CAP-6a). Types only in FOUNDATION.
    BrokerResult { call_id: u64, outcome: BrokerOutcome },
    /// Acknowledgement of `BrokerCancel`.
    BrokerCancelAck { call_id: u64 },
}

/// Worker-to-parent stdout frame. Line-delimited; the terminal `WorkerResult`
/// still follows. Reader wiring is MSG-CORE.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[allow(clippy::large_enum_variant, clippy::derive_partial_eq_without_eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerOutputFrame {
    /// Worker-originated Data.
    Message { envelope: crate::messaging::PrxMessage },
    /// Delivery ack for a parent-injected message.
    Ack {
        message_id: uuid::Uuid,
        state: crate::messaging::DeliveryState,
    },
    /// Capability broker call.
    BrokerCall {
        call_id: u64,
        capability_kind: CapabilityKind,
        name: String,
        args: serde_json::Value,
        deadline_ms: u64,
        mac: String,
    },
    /// Cancel a pending broker call.
    BrokerCancel { call_id: u64 },
}

/// Issued capability kind on a broker call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CapabilityKind {
    McpAlias,
    Wasm,
    Skill,
    Browser,
    OtherLive { name: String },
}

/// Result of a broker call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrokerOutcome {
    Ok { output: serde_json::Value },
    Err { error: BrokerError },
}

/// Broker failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrokerError {
    NotIssued,
    Denied,
    BackendGone,
    Timeout,
    Cancelled,
    Failed { message: String },
}

/// Durable segment lookup references; consumers must verify the stored events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerSegmentTerminal {
    pub terminal_id: String,
    pub event_id: String,
    pub event_digest: String,
}

/// Authenticated inventory of a logical worker completion and its segments.
/// This is transport evidence, not a verified usage receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCompletionEvidence {
    pub run_id: String,
    pub input_event_id: String,
    pub terminal_event_id: String,
    pub terminal_event_digest: String,
    pub recorded_segments: u64,
    pub segments: Vec<WorkerSegmentTerminal>,
    pub seal: [u8; 32],
}

impl WorkerCompletionEvidence {
    pub(crate) fn validate_inventory(&self, success: bool) -> anyhow::Result<()> {
        let confirmed = u64::try_from(self.segments.len())?;
        anyhow::ensure!(
            confirmed <= self.recorded_segments,
            "worker inventory exceeds recorded segments"
        );
        anyhow::ensure!(
            !success || (self.recorded_segments > 0 && confirmed == self.recorded_segments),
            "successful worker inventory has unconfirmed segments"
        );
        let prefix = format!("child-segment:{}:", self.input_event_id);
        let mut indices = std::collections::BTreeSet::new();
        let mut events = std::collections::BTreeSet::new();
        events.insert(self.terminal_event_id.as_str());
        for segment in &self.segments {
            let index = segment
                .terminal_id
                .strip_prefix(&prefix)
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| anyhow::anyhow!("worker segment identity differs from original input"))?;
            anyhow::ensure!(
                index < self.recorded_segments
                    && segment.terminal_id == format!("{prefix}{index}")
                    && indices.insert(index)
                    && !segment.event_id.is_empty()
                    && events.insert(segment.event_id.as_str()),
                "worker inventory contains duplicate or invalid segment references"
            );
        }
        Ok(())
    }

    pub(crate) fn signing_bytes(&self, result: &WorkerResult) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(&(
            &self.run_id,
            &self.input_event_id,
            &self.terminal_event_id,
            &self.terminal_event_digest,
            self.recorded_segments,
            &self.segments,
            result.success,
            &result.output,
            &result.error,
            &result.tokens_used,
        ))?)
    }
}

/// Result returned by `session-worker` on stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_evidence: Option<WorkerCompletionEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinary_completion: Option<OrdinaryWorkerCompletionEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_completion: Option<OperatorWorkerCompletionEvidence>,
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_used: Option<crate::llm::route_decision::TokenUsage>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_manifest_roundtrip_json_preserves_owner_isolation() {
        let manifest = WorkerManifest {
            parent_capability: Some("capability".into()),
            capability_manifest: None,
            child_source: None,
            ordinary_source: None,
            operator_source: None,
            run_id: "run-1".into(),
            task: "analyze".into(),
            provider_name: "openrouter".into(),
            model: "anthropic/claude-sonnet-4.6".into(),
            api_key: None,
            route_hint: None,
            route_source_provider: None,
            route_credential: None,
            route_uses_root_connection: false,
            direct_uses_root_connection: false,
            temperature: 0.7,
            temperature_override: None,
            config_dir: PathBuf::from("/tmp/openprx"),
            config_generation: "a".repeat(64),
            runtime_config_generation_id: Some(42),
            runtime_config_source_revision: Some("revision-42".into()),
            workspace_dir: PathBuf::from("/tmp/ws"),
            memory_db_path: PathBuf::from("/tmp/ws/brain.db"),
            memory_workspace_id: Some("/tmp/parent-ws".into()),
            memory_strategy: Some("shared_fabric".into()),
            memory_backend: "sqlite".into(),
            shared_memory_db_path: Some(PathBuf::from("/tmp/parent-ws/memory/brain.db")),
            worker_memory_db_path: Some(PathBuf::from("/tmp/ws/brain.db")),
            agent_id: Some("worker-agent".into()),
            persona_id: Some("persona-a".into()),
            memory_event_recording: crate::memory::MemoryEventRecording {
                enabled: true,
                record_user_messages: true,
                record_assistant_messages: true,
                record_tool_events: true,
            },
            allowed_tools: vec!["shell".into(), "file_read".into()],
            channel_excluded_tools: Vec::new(),
            system_prompt: None,
            identity_dir: Some("identity/worker".into()),
            scope_sender: Some("openprx_user".into()),
            scope_channel: Some("telegram".into()),
            scope_chat_type: Some("direct".into()),
            scope_chat_id: Some("chat-1".into()),
            owner_id: Some("owner:/tmp/parent-ws:telegram:openprx_user".into()),
            owner_isolated: true,
            topic_id: Some("topic-1".into()),
            parent_task_id: Some("run-0".into()),
            source_message_event_id: Some("msg-1".into()),
            spawn_depth: 1,
            session_scope_key: "telegram:chat-1:openprx_user".into(),
            parent_run_id: Some("run-0".into()),
            inherited_provenance: None,
            compaction_config: Some(crate::config::AgentCompactionConfig::default()),
        };

        let json = serde_json::to_string(&manifest).expect("serialize manifest");
        let parsed: WorkerManifest = serde_json::from_str(&json).expect("deserialize manifest");

        assert_eq!(parsed.run_id, "run-1");
        assert_eq!(parsed.config_dir, PathBuf::from("/tmp/openprx"));
        assert_eq!(parsed.config_generation, "a".repeat(64));
        assert_eq!(parsed.runtime_config_generation_id, Some(42));
        assert_eq!(parsed.runtime_config_source_revision.as_deref(), Some("revision-42"));
        assert_eq!(parsed.parent_capability.as_deref(), Some("capability"));
        assert_eq!(parsed.allowed_tools.len(), 2);
        assert_eq!(parsed.memory_workspace_id.as_deref(), Some("/tmp/parent-ws"));
        assert_eq!(parsed.memory_strategy.as_deref(), Some("shared_fabric"));
        assert_eq!(parsed.memory_backend, "sqlite");
        assert_eq!(
            parsed.shared_memory_db_path.as_deref(),
            Some(PathBuf::from("/tmp/parent-ws/memory/brain.db").as_path())
        );
        assert_eq!(parsed.agent_id.as_deref(), Some("worker-agent"));
        assert_eq!(parsed.persona_id.as_deref(), Some("persona-a"));
        assert!(parsed.memory_event_recording.record_tool_events);
        assert_eq!(parsed.identity_dir.as_deref(), Some("identity/worker"));
        assert_eq!(parsed.scope_sender.as_deref(), Some("openprx_user"));
        assert_eq!(
            parsed.owner_id.as_deref(),
            Some("owner:/tmp/parent-ws:telegram:openprx_user")
        );
        assert!(parsed.owner_isolated);
        assert_eq!(parsed.topic_id.as_deref(), Some("topic-1"));
        assert_eq!(parsed.parent_task_id.as_deref(), Some("run-0"));
        assert_eq!(parsed.source_message_event_id.as_deref(), Some("msg-1"));
        assert_eq!(parsed.spawn_depth, 1);
        assert_eq!(parsed.session_scope_key, "telegram:chat-1:openprx_user".to_string());
        assert_eq!(parsed.parent_run_id.as_deref(), Some("run-0"));
        assert!(parsed.compaction_config.is_some());
    }

    #[test]
    fn worker_control_frame_is_tagged_line_json() {
        let frame = WorkerControlFrame::Steer {
            message: "pivot to X".into(),
        };
        let json = serde_json::to_string(&frame).expect("serialize control frame");
        assert!(!json.contains('\n'), "control frames must fit on a single line");
        assert_eq!(json, r#"{"kind":"steer","message":"pivot to X"}"#);
        let parsed: WorkerControlFrame = serde_json::from_str(&json).expect("deserialize control frame");
        assert_eq!(parsed, frame);
    }

    #[test]
    fn worker_control_frame_rejects_unknown_kind() {
        let error = serde_json::from_str::<WorkerControlFrame>(r#"{"kind":"exec","command":"rm"}"#).unwrap_err();
        assert!(error.to_string().contains("unknown variant"), "{error}");
    }

    fn sample_envelope() -> crate::messaging::PrxMessage {
        crate::messaging::PrxMessage {
            id: uuid::Uuid::nil(),
            idempotency: crate::messaging::IdempotencyKey::parse("0123456789abcdef").expect("test: key"),
            stream_id: Some(uuid::Uuid::nil()),
            seq: 1,
            ts: 0,
            from: crate::messaging::SessionAddr::new("i", "o", "from", None),
            to: crate::messaging::SessionAddr::new("i", "o", "to", None),
            to_pin: crate::messaging::RecipientPin {
                incarnation: uuid::Uuid::nil(),
                generation: 1,
                token: "t".into(),
            },
            in_reply_to: None,
            hop_count: 0,
            ancestry: Vec::new(),
            kind: crate::messaging::MessageKind::Data,
            body: crate::messaging::MessageBody::default(),
            origin: None,
            auth: None,
            exec: None,
            canonical_hash: None,
        }
    }

    #[test]
    fn worker_c3_frames_round_trip() {
        let control = WorkerControlFrame::Message {
            envelope: sample_envelope(),
        };
        let json = serde_json::to_string(&control).expect("test: control json");
        assert!(json.contains("\"kind\":\"message\""));
        let parsed: WorkerControlFrame = serde_json::from_str(&json).expect("test: control parse");
        assert_eq!(parsed, control);

        let result = WorkerControlFrame::BrokerResult {
            call_id: 1,
            outcome: BrokerOutcome::Ok {
                output: serde_json::json!({"ok": true}),
            },
        };
        let parsed: WorkerControlFrame =
            serde_json::from_str(&serde_json::to_string(&result).expect("test: result")).expect("test: result parse");
        assert_eq!(parsed, result);

        let ack = WorkerControlFrame::BrokerCancelAck { call_id: 2 };
        let parsed: WorkerControlFrame =
            serde_json::from_str(&serde_json::to_string(&ack).expect("test: ack")).expect("test: ack parse");
        assert_eq!(parsed, ack);

        let output = WorkerOutputFrame::BrokerCall {
            call_id: 3,
            capability_kind: CapabilityKind::McpAlias,
            name: "search".into(),
            args: serde_json::json!({}),
            deadline_ms: 1000,
            mac: "ab".into(),
        };
        let json = serde_json::to_string(&output).expect("test: output json");
        assert!(json.contains("\"kind\":\"broker_call\""));
        assert!(json.contains("\"type\":\"mcp_alias\""));
        let parsed: WorkerOutputFrame = serde_json::from_str(&json).expect("test: output parse");
        assert_eq!(parsed, output);

        let nack = WorkerOutputFrame::Ack {
            message_id: uuid::Uuid::nil(),
            state: crate::messaging::DeliveryState::Accepted,
        };
        let parsed: WorkerOutputFrame =
            serde_json::from_str(&serde_json::to_string(&nack).expect("test: ack out")).expect("test: ack out parse");
        assert_eq!(parsed, nack);
    }

    #[test]
    fn inherited_provenance_is_sealed_into_worker_manifest() {
        let mut manifest = WorkerManifest {
            parent_capability: None,
            capability_manifest: None,
            child_source: None,
            ordinary_source: None,
            operator_source: None,
            run_id: "run-1".into(),
            task: "t".into(),
            provider_name: "mock".into(),
            model: "mock".into(),
            api_key: None,
            route_hint: None,
            route_source_provider: None,
            route_credential: None,
            route_uses_root_connection: false,
            direct_uses_root_connection: false,
            temperature: 0.7,
            temperature_override: None,
            config_dir: PathBuf::from("/tmp"),
            config_generation: "0".repeat(64),
            runtime_config_generation_id: None,
            runtime_config_source_revision: None,
            workspace_dir: PathBuf::from("/tmp"),
            memory_db_path: PathBuf::from("/tmp/brain.db"),
            memory_workspace_id: None,
            memory_strategy: None,
            memory_backend: "sqlite".into(),
            shared_memory_db_path: None,
            worker_memory_db_path: None,
            agent_id: None,
            persona_id: None,
            memory_event_recording: crate::memory::MemoryEventRecording::default(),
            allowed_tools: Vec::new(),
            channel_excluded_tools: Vec::new(),
            system_prompt: None,
            identity_dir: None,
            scope_sender: None,
            scope_channel: None,
            scope_chat_type: None,
            scope_chat_id: None,
            owner_id: None,
            owner_isolated: false,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            spawn_depth: 0,
            session_scope_key: "s".into(),
            parent_run_id: None,
            inherited_provenance: None,
            compaction_config: None,
        };
        let trigger = uuid::Uuid::from_u128(42);
        let provenance = crate::messaging::InheritedProvenance {
            remote: crate::messaging::RemotePeerAttr {
                issuer: "spiffe://prx".into(),
                subject: "spiffe://prx/peer".into(),
                message_id: trigger,
            },
            chain: crate::messaging::CausalChain {
                trigger,
                root: trigger,
                depth: 1,
                ancestry: Vec::new(),
            },
        };
        manifest.seal_inherited_provenance(provenance.clone());
        assert_eq!(manifest.inherited_provenance.as_ref(), Some(&provenance));
        let json = serde_json::to_value(&manifest).expect("test: json");
        assert!(json.get("inherited_provenance").is_some());
    }

    #[test]
    fn worker_result_roundtrip_json() {
        let result = WorkerResult {
            completion_evidence: None,
            ordinary_completion: None,
            operator_completion: None,
            success: false,
            output: String::new(),
            error: Some("timeout".into()),
            tokens_used: None,
        };

        let json = serde_json::to_string(&result).expect("serialize result");
        let parsed: WorkerResult = serde_json::from_str(&json).expect("deserialize result");

        assert!(!parsed.success);
        assert_eq!(parsed.error.as_deref(), Some("timeout"));
    }

    #[test]
    fn config_source_generation_tracks_main_and_fragments() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let config_dir = tmp.path();
        std::fs::write(config_dir.join("config.toml"), "default_temperature = 0.7\n").expect("write config.toml");
        let fragments = config_dir.join("config.d");
        std::fs::create_dir(&fragments).expect("create config.d");
        std::fs::write(fragments.join("memory.toml"), "[memory]\nbackend = \"sqlite\"\n").expect("write fragment");

        let first = config_source_generation(config_dir).expect("first generation");
        let stable = config_source_generation(config_dir).expect("stable generation");
        assert_eq!(first, stable);

        std::fs::write(fragments.join("memory.toml"), "[memory]\nbackend = \"markdown\"\n").expect("rewrite fragment");
        let changed = config_source_generation(config_dir).expect("changed generation");
        assert_ne!(first, changed);
    }
}

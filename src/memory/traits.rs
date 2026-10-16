use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::memory::principal::MemoryWriteContext;

/// A single memory entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub key: String,
    pub content: String,
    pub category: MemoryCategory,
    pub timestamp: String,
    pub session_id: Option<String>,
    pub score: Option<f64>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub access_count: Option<u32>,
    #[serde(default)]
    pub useful_count: Option<u32>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub source_confidence: Option<f64>,
    #[serde(default)]
    pub verification_status: Option<VerificationStatus>,
    #[serde(default)]
    pub lifecycle_state: Option<LifecycleState>,
    #[serde(default)]
    pub compressed_from: Option<Vec<String>>,
}

/// Bounded content returned by a selected causal Memory retrieval.
/// The runtime owns the query and principal; these fields are never a tool
/// argument or a substitute for the backend ACL check.
#[derive(Clone, PartialEq, Eq)]
pub struct DecisionMemoryHit {
    pub id: String,
    pub key: String,
    pub category: MemoryCategory,
    pub session_id: Option<String>,
    pub content: String,
    pub created_at: String,
    /// Stored row metadata, not a verified runtime event receipt.
    pub source_event_id: Option<String>,
}

impl std::fmt::Debug for DecisionMemoryHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionMemoryHit")
            .field("content_len", &self.content.len())
            .finish_non_exhaustive()
    }
}

/// Bounded document evidence from a selected, read-only decision query.
/// Row identities are metadata, not execution authority or event receipts.
#[derive(Clone, PartialEq, Eq)]
pub struct DecisionDocumentHit {
    pub chunk_id: String,
    pub document_id: String,
    pub content: String,
    pub created_at: String,
}

impl std::fmt::Debug for DecisionDocumentHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionDocumentHit")
            .field("content_len", &self.content.len())
            .finish_non_exhaustive()
    }
}

/// Bounded historical session text, never a runtime authority receipt.
#[derive(Clone, PartialEq, Eq)]
pub struct DecisionSessionHit {
    pub event_id: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
}

impl std::fmt::Debug for DecisionSessionHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionSessionHit")
            .field("content_len", &self.content.len())
            .finish_non_exhaustive()
    }
}

/// ACL behavior requested by a backend-neutral memory reader.
///
/// `Enforce` returns only entries visible to the runtime principal. `Observe`
/// returns the backend's unrestricted compatibility result while still letting
/// ACL-aware backends evaluate and audit what enforcement would have removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryReadMode {
    Enforce,
    Observe,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatProfile {
    pub id: String,
    pub channel: String,
    pub chat_id: String,
    pub chat_kind: String,
    pub title: Option<String>,
    pub purpose: Option<String>,
    pub notes: Option<String>,
    pub tags: Vec<String>,
    pub updated_by: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Verification state of a memory entry's factual quality.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Unverified,
    Verified,
    Conflicted,
    Deprecated,
}

/// Lifecycle state used by evolution-aware memory retrieval and maintenance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Active,
    Archived,
    Tombstoned,
}

/// Memory categories for organization
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCategory {
    /// Long-term facts, preferences, decisions
    Core,
    /// Daily session logs
    Daily,
    /// Conversation context
    Conversation,
    /// User-defined custom category
    Custom(String),
}

pub fn validate_memory_write_target(key: &str, session_id: Option<&str>) -> anyhow::Result<()> {
    const RESERVED_PREFIXES: &[&str] = &["self/", "router/"];

    anyhow::ensure!(
        !key.starts_with("self/fitness/"),
        "refusing to write fitness telemetry into conversational memory; use FitnessStore"
    );

    if RESERVED_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
        && session_id != Some(crate::self_system::SELF_SYSTEM_SESSION_ID)
    {
        anyhow::bail!("refusing to write reserved memory namespace without self_system session");
    }

    Ok(())
}

impl std::fmt::Display for MemoryCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Core => write!(f, "core"),
            Self::Daily => write!(f, "daily"),
            Self::Conversation => write!(f, "conversation"),
            Self::Custom(name) => write!(f, "{name}"),
        }
    }
}

/// Session metadata for persisted channel conversations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSessionSummary {
    pub session_key: String,
    pub channel: String,
    pub sender: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: u64,
    pub last_message_preview: String,
}

/// A persisted conversation turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationTurn {
    pub id: i64,
    pub session_key: String,
    pub role: String,
    pub content: String,
    pub timestamp: String,
    pub message_id: Option<String>,
}

/// Visibility scope for message and memory fabric events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryVisibility {
    Global,
    Workspace,
    Agent,
    Session,
    Private,
    System,
}

impl MemoryVisibility {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Workspace => "workspace",
            Self::Agent => "agent",
            Self::Session => "session",
            Self::Private => "private",
            Self::System => "system",
        }
    }
}

impl std::fmt::Display for MemoryVisibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for MemoryVisibility {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "global" => Ok(Self::Global),
            "workspace" => Ok(Self::Workspace),
            "agent" => Ok(Self::Agent),
            "session" => Ok(Self::Session),
            "private" => Ok(Self::Private),
            "system" => Ok(Self::System),
            other => anyhow::bail!("unknown memory visibility '{other}'"),
        }
    }
}

/// Principal used to filter shared message events for a concrete agent turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryPrincipal {
    pub workspace_id: String,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub session_key: Option<String>,
    pub channel: Option<String>,
    pub sender: Option<String>,
    #[serde(default)]
    pub owner_id: Option<String>,
    /// Require `owner_id` equality before admitting `session`- and
    /// `private`-visibility rows (C03 / V02-R4).
    ///
    /// `false` (the default) is the historical contract: `session` rows are
    /// admitted by `session_key` and `private` rows by `sender` / `agent_id` /
    /// `persona_id`. That is correct for channel, chat, cron, xin and sub-agent
    /// principals, whose `session_key` and `sender` already encode one trust
    /// domain.
    ///
    /// `true` is set ONLY by trusted internal context for an externally
    /// authenticated origin — today the MCP execution context, whose session key
    /// (`mcp:{subject}`) and sender are the *external subject only*, so two
    /// identities presenting the same subject under different issuers would
    /// otherwise share both. With this set, those two arms additionally require
    /// `owner_id = principal.owner_id`; a principal that requires the match but
    /// carries no `owner_id` admits no `session`/`private` row at all (fail
    /// closed), and a stored row with a NULL `owner_id` is never admitted.
    /// `workspace` and `global` visibility keep their documented shared
    /// contract.
    ///
    /// This is never read from caller-supplied tool arguments: the execution
    /// service re-authors `_zc_scope` from the authenticated envelope.
    #[serde(default)]
    pub require_owner_match: bool,
    /// Optional pre-cutover (legacy) `session_key` candidate for read-merge.
    ///
    /// When a durable `session_key` is migrated to a canonical format (D4),
    /// the legacy value is carried here so `session`-visibility recall can read
    /// both the new canonical history and the old legacy history as a union
    /// (read-merge, never move). `None` (the default) preserves the historical
    /// single-key behaviour exactly. The legacy key is always read-only.
    #[serde(default)]
    pub legacy_session_key: Option<String>,
}

impl Default for MemoryPrincipal {
    fn default() -> Self {
        Self {
            workspace_id: String::new(),
            agent_id: None,
            persona_id: None,
            session_key: None,
            channel: None,
            sender: None,
            owner_id: None,
            require_owner_match: false,
            legacy_session_key: None,
        }
    }
}

impl MemoryPrincipal {
    /// Deduplicated `session_key` candidates for `session`-visibility recall.
    ///
    /// Returns the canonical `session_key` first, followed by the optional
    /// `legacy_session_key` when it is present, non-empty, and distinct from the
    /// canonical key. When no `session_key` is set the list is empty (the
    /// predicate must then match nothing, mirroring the historical
    /// `session_key IS NOT NULL AND session_key = ?` behaviour). When the legacy
    /// key is `None` (or equal to canonical) the list degrades to a single key,
    /// producing byte-identical SQL to the pre-D4 single-key predicate.
    #[must_use]
    pub fn session_key_candidates(&self) -> Vec<String> {
        let mut candidates: Vec<String> = Vec::with_capacity(2);
        if let Some(canonical) = self.session_key.as_deref().map(str::trim).filter(|key| !key.is_empty()) {
            candidates.push(canonical.to_string());
        }
        if let Some(legacy) = self
            .legacy_session_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
        {
            if !candidates.iter().any(|existing| existing == legacy) {
                candidates.push(legacy.to_string());
            }
        }
        candidates
    }

    #[must_use]
    pub fn effective_owner_id(&self) -> Option<String> {
        if self.owner_id.as_deref().is_some_and(|owner| !owner.trim().is_empty()) {
            return self.owner_id.clone();
        }

        let channel = self.channel.as_deref()?.trim();
        let sender = self.sender.as_deref()?.trim();
        if channel.is_empty() || sender.is_empty() {
            return None;
        }

        Some(
            crate::memory::principal::OwnerPrincipal::new(
                self.workspace_id.clone(),
                channel,
                sender,
                self.session_key.clone().unwrap_or_default(),
                vec![crate::memory::principal::Role::Anonymous],
            )
            .owner_id,
        )
    }
}

/// Typed origin for an event in the shared message fabric.
///
/// The string representation intentionally matches the legacy `source` column
/// so existing rows and external JSON remain compatible while Rust callers no
/// longer pass an unstructured source string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageEventSource {
    Chat,
    Agent,
    Gateway,
    Channel,
    Console,
    SessionWorker,
    SessionsSpawn,
    Delegate,
    Cron,
    Xin,
    Heartbeat,
    Webhook,
    Other(String),
}

impl MessageEventSource {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Chat => "chat",
            Self::Agent => "agent",
            Self::Gateway => "gateway",
            Self::Channel => "channel",
            Self::Console => "console",
            Self::SessionWorker => "session_worker",
            Self::SessionsSpawn => "sessions_spawn",
            Self::Delegate => "delegate",
            Self::Cron => "cron",
            Self::Xin => "xin",
            Self::Heartbeat => "heartbeat",
            Self::Webhook => "webhook",
            Self::Other(source) => source,
        }
    }
}

impl std::fmt::Display for MessageEventSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl PartialEq<str> for MessageEventSource {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for MessageEventSource {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl From<&str> for MessageEventSource {
    fn from(source: &str) -> Self {
        match source.trim() {
            "chat" => Self::Chat,
            "agent" => Self::Agent,
            "gateway" => Self::Gateway,
            "channel" => Self::Channel,
            "console" => Self::Console,
            "session_worker" => Self::SessionWorker,
            "sessions_spawn" => Self::SessionsSpawn,
            "delegate" => Self::Delegate,
            "cron" => Self::Cron,
            "xin" => Self::Xin,
            "heartbeat" => Self::Heartbeat,
            "webhook" => Self::Webhook,
            other => Self::Other(other.to_string()),
        }
    }
}

impl From<String> for MessageEventSource {
    fn from(source: String) -> Self {
        Self::from(source.as_str())
    }
}

impl Serialize for MessageEventSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MessageEventSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from)
    }
}

/// Typed subject associated with a message event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum MessageEventSubject {
    Project(String),
    Task(String),
    Incident(String),
    Customer(String),
    Document(String),
    Goal(String),
    Topic(String),
    Conversation(String),
    Other { subject_type: String, id: String },
}

fn legacy_message_event_type() -> String {
    "message.legacy".to_string()
}

/// Input used to append an event into the shared message fabric.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEventInput {
    pub event_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub source: MessageEventSource,
    pub channel: Option<String>,
    pub session_key: Option<String>,
    pub parent_session_key: Option<String>,
    pub run_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub sender: Option<String>,
    pub recipient: Option<String>,
    pub role: String,
    #[serde(default)]
    pub event_type: String,
    #[serde(default)]
    pub subject: Option<MessageEventSubject>,
    #[serde(default)]
    pub goal_id: Option<String>,
    #[serde(default)]
    pub causation_event_id: Option<String>,
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub attempt_id: Option<String>,
    #[serde(default)]
    pub lease_epoch: Option<i64>,
    #[serde(default)]
    pub config_generation_id: Option<u64>,
    #[serde(default)]
    pub config_source_revision: Option<String>,
    pub content: String,
    pub raw_payload_json: Option<String>,
    pub visibility: MemoryVisibility,
}

impl MessageEventInput {
    /// Validate the explicit typed metadata required by every backend.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.source.as_str().trim().is_empty(),
            "message event source must not be empty"
        );
        anyhow::ensure!(
            !self.event_type.trim().is_empty(),
            "message event type must not be empty"
        );
        Ok(())
    }
}

/// Persisted shared message event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEvent {
    pub id: i64,
    pub event_id: String,
    pub idempotency_key: Option<String>,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub source: MessageEventSource,
    pub channel: Option<String>,
    pub session_key: Option<String>,
    pub parent_session_key: Option<String>,
    pub run_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub sender: Option<String>,
    pub recipient: Option<String>,
    pub role: String,
    #[serde(default = "legacy_message_event_type")]
    pub event_type: String,
    #[serde(default)]
    pub subject: Option<MessageEventSubject>,
    #[serde(default)]
    pub goal_id: Option<String>,
    #[serde(default)]
    pub causation_event_id: Option<String>,
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub attempt_id: Option<String>,
    #[serde(default)]
    pub lease_epoch: Option<i64>,
    #[serde(default)]
    pub config_generation_id: Option<u64>,
    #[serde(default)]
    pub config_source_revision: Option<String>,
    pub content: String,
    pub content_hash: Option<String>,
    pub raw_payload_json: Option<String>,
    pub visibility: MemoryVisibility,
    pub created_at: String,
    pub updated_at: String,
}

impl MessageEvent {
    pub(crate) fn validate_console_projection(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.id > 0
                && !self.event_id.is_empty()
                && !self.workspace_id.is_empty()
                && self.owner_id.as_deref().is_some_and(|value| !value.is_empty())
                && self.session_key.as_deref().is_some_and(|value| !value.is_empty())
                && self.channel.as_deref().is_some_and(|value| !value.is_empty())
                && self.sender.as_deref().is_some_and(|value| !value.is_empty())
                && self.source == MessageEventSource::Console
                && self.visibility == MemoryVisibility::Session
                && self.event_type == "message.created"
                && matches!(self.role.as_str(), "user" | "assistant"),
            "console projection requires a durable scoped message event"
        );
        Ok(())
    }
}

#[cfg(test)]
mod message_event_contract_tests {
    use super::*;

    /// The SQLite backend's production source: the root `sqlite/mod.rs` followed
    /// by every other source file directly under `src/memory/sqlite/`. Its
    /// `tests/` directory is excluded so a test fixture cannot satisfy a
    /// production guard.
    fn sqlite_backend_source() -> String {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/memory/sqlite");
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&directory)
            .expect("test: read src/memory/sqlite")
            .map(|entry| entry.expect("test: read directory entry").path())
            .filter(|path| {
                path.extension().is_some_and(|extension| extension == "rs")
                    && path
                        .file_name()
                        .is_some_and(|name| name != "mod.rs" && name != "fixture_readback.rs")
            })
            .collect();
        files.sort();
        files.insert(0, directory.join("mod.rs"));
        assert!(files.len() > 1, "test: the sqlite backend source files were not found");
        let source = files
            .iter()
            .map(|path| std::fs::read_to_string(path).expect("test: read sqlite backend source"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!source.is_empty(), "test: the sqlite backend source is empty");
        source
    }

    #[test]
    fn typed_source_and_subject_keep_stable_json_contracts() {
        let known = MessageEventSource::Chat;
        assert_eq!(serde_json::to_string(&known).unwrap(), "\"chat\"");
        assert_eq!(serde_json::from_str::<MessageEventSource>("\"chat\"").unwrap(), known);

        let custom = MessageEventSource::Other("custom-adapter".to_string());
        assert_eq!(serde_json::to_string(&custom).unwrap(), "\"custom-adapter\"");
        assert_eq!(
            serde_json::from_str::<MessageEventSource>("\"custom-adapter\"").unwrap(),
            custom
        );

        let subject = MessageEventSubject::Task("task-1".to_string());
        assert_eq!(
            serde_json::from_str::<MessageEventSubject>(&serde_json::to_string(&subject).unwrap()).unwrap(),
            subject
        );
    }

    /// The PostgreSQL backend's production text: the root file plus the sibling
    /// files that were split out of it. Listed with `include_str!`, so a file
    /// that moves or disappears breaks the build instead of blinding the scan.
    fn postgres_backend_source() -> String {
        [
            include_str!("postgres.rs"),
            include_str!("postgres/pool.rs"),
            include_str!("postgres/schema.rs"),
            include_str!("postgres/access.rs"),
            include_str!("postgres/embedding.rs"),
            include_str!("postgres/rows.rs"),
            include_str!("postgres/memory_impl.rs"),
            include_str!("postgres/events.rs"),
        ]
        .join("\n// ---- next source file ----\n")
    }

    /// A new file under `postgres/` must join the list above or be named here
    /// as outside the old single-file text.
    #[test]
    fn postgres_backend_source_lists_every_backend_file() {
        let source = postgres_backend_source();
        let scanned: Vec<&str> = source.split("\n// ---- next source file ----\n").collect();
        assert_eq!(
            scanned.len(),
            8,
            "the scanned text must hold the root and seven sibling files"
        );
        let outside = [
            "decision_reads.rs",
            "ingress_recovery.rs",
            "router_history.rs",
            "router_learning.rs",
        ];
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/memory/postgres");
        for entry in std::fs::read_dir(directory).expect("test: read the backend directory") {
            let path = entry.expect("test: read a directory entry").path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("test: utf-8 file name");
            if path.is_dir() {
                assert_eq!(
                    name, "tests",
                    "postgres/{name}/ is a subdirectory the scan does not read"
                );
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("test: read a backend file");
            assert!(
                scanned.iter().any(|part| *part == text) || outside.contains(&name),
                "postgres/{name} is neither scanned nor listed as outside the scan"
            );
        }
    }

    #[test]
    fn sqlite_and_postgres_append_paths_share_lineage_and_transaction_boundaries() {
        let sqlite = sqlite_backend_source();
        let postgres = postgres_backend_source();
        // The SQLite append lives in `append_event_with_recovery`; the trait
        // method `append_message_event` only delegates to it.
        let sqlite_append = sqlite
            .split_once("async fn append_event_with_recovery")
            .map_or("", |(_, body)| body.split("\n    fn ").next().unwrap_or(body));
        let postgres_append = postgres
            .split_once("async fn append_message_event")
            .map_or("", |(_, body)| body);
        for field in [
            "event_type",
            "source_ref_json",
            "subject_ref_json",
            "goal_id",
            "causation_event_id",
            "correlation_id",
            "attempt_id",
            "lease_epoch",
        ] {
            assert!(sqlite_append.contains(field), "SQLite append must carry {field}");
            assert!(postgres_append.contains(field), "Postgres append must carry {field}");
        }
        assert!(sqlite_append.contains("let tx = conn.unchecked_transaction()?"));
        assert!(postgres_append.contains("let mut tx = client.transaction()?"));
        assert!(sqlite_append.contains("tx.commit()?"));
        assert!(postgres_append.contains("tx.commit()?"));
        assert!(sqlite_append.contains("input.event_type"));
        assert!(postgres_append.contains("input.event_type"));
    }

    fn compaction_input(
        source_event_ids_json: Option<String>,
        source_event_range_json: Option<String>,
    ) -> CompactionRunInput {
        CompactionRunInput {
            run_id: Some("compact-contract".to_string()),
            workspace_id: "workspace".to_string(),
            owner_id: None,
            session_key: Some("chat:session".to_string()),
            agent_id: None,
            persona_id: None,
            trigger: "test".to_string(),
            mode: "safeguard".to_string(),
            source_message_count: 1,
            source_token_estimate: 1,
            summary: "summary".to_string(),
            summary_memory_key: None,
            source_event_ids_json,
            source_event_range_json,
            source_document_refs_json: None,
            fidelity_status: "accepted".to_string(),
            payload_json: None,
        }
    }

    #[test]
    fn compaction_provenance_rejects_hash_objects_masquerading_as_event_ids() {
        let input = compaction_input(
            Some(r#"[{"index":0,"role":"user","content_hash":"fake"}]"#.to_string()),
            Some(
                serde_json::to_string(&CompactionSourceEventRange {
                    first_event_id: "fake".to_string(),
                    last_event_id: "fake".to_string(),
                    first_row_id: 1,
                    last_row_id: 1,
                    source_event_count: 1,
                })
                .unwrap(),
            ),
        );
        let error = input.validate_source_event_provenance().unwrap_err();
        assert!(error.to_string().contains("string array"));
    }

    #[test]
    fn compaction_provenance_requires_matching_ids_and_covered_range() {
        let range = CompactionSourceEventRange {
            first_event_id: "event-1".to_string(),
            last_event_id: "event-2".to_string(),
            first_row_id: 11,
            last_row_id: 12,
            source_event_count: 2,
        };
        let valid = compaction_input(
            Some(r#"["event-1","event-2"]"#.to_string()),
            Some(serde_json::to_string(&range).unwrap()),
        );
        valid.validate_source_event_provenance().unwrap();

        let missing_range = compaction_input(Some(r#"["event-1"]"#.to_string()), None);
        assert!(missing_range.validate_source_event_provenance().is_err());
    }

    #[test]
    fn sqlite_and_postgres_compaction_paths_persist_ids_and_covered_range() {
        let sqlite = sqlite_backend_source();
        let postgres = postgres_backend_source();
        let sqlite_append = sqlite
            .split_once("async fn append_compaction_run")
            .map_or("", |(_, body)| body);
        let postgres_append = postgres
            .split_once("async fn append_compaction_run")
            .map_or("", |(_, body)| body);
        for field in ["source_event_ids_json", "source_event_range_json"] {
            assert!(
                sqlite_append.contains(field),
                "SQLite compaction append must carry {field}"
            );
            assert!(
                postgres_append.contains(field),
                "Postgres compaction append must carry {field}"
            );
        }
        assert!(sqlite_append.contains("validate_source_event_provenance"));
        assert!(postgres_append.contains("validate_source_event_provenance"));
    }
}

/// Query for recent shared events visible to an agent turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedContextQuery {
    pub principal: MemoryPrincipal,
    pub since_event_id: Option<i64>,
    pub limit: usize,
    pub include_roles: Vec<String>,
}

/// Query for recent current-session events visible to an agent turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionContextQuery {
    pub principal: MemoryPrincipal,
    pub since_event_id: Option<i64>,
    pub limit: usize,
    pub include_roles: Vec<String>,
}

/// Input used to append a lightweight outbox event for memory fabric changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEventInput {
    pub event_id: Option<String>,
    pub workspace_id: String,
    /// Owning principal of this event, mirroring `message_events.owner_id`.
    ///
    /// `None` means unattributed: the row is preserved and stays readable by
    /// every non-owner-isolated principal, but an owner-isolated principal
    /// (see [`MemoryPrincipal::require_owner_match`]) never admits it.
    #[serde(default)]
    pub owner_id: Option<String>,
    pub event_type: String,
    pub subject_table: String,
    pub subject_id: String,
    pub session_key: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub parent_run_id: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub visibility: MemoryVisibility,
    pub payload_json: Option<String>,
}

/// Persisted memory fabric outbox event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEvent {
    pub id: i64,
    pub event_id: String,
    pub workspace_id: String,
    /// Owning principal of this event. `None` for rows written before the
    /// column existed, and for writers that have no owner in scope.
    #[serde(default)]
    pub owner_id: Option<String>,
    pub event_type: String,
    pub subject_table: String,
    pub subject_id: String,
    pub session_key: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub parent_run_id: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub visibility: MemoryVisibility,
    pub payload_json: Option<String>,
    pub created_at: String,
}

/// Optional metadata persisted with semantic memory entries.
///
/// This is intentionally backend-neutral so SQLite can persist it now and
/// Postgres can implement the same contract without changing entrypoint code.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryStoreMetadata {
    pub workspace_id: Option<String>,
    pub owner_id: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub source_event_id: Option<String>,
    pub source: Option<String>,
    /// Optional topic scope. When absent, backends may fall back to
    /// `source_event_id` so Project-visibility memories remain scope-resolvable
    /// (FIX-P0-23).
    pub topic_id: Option<String>,
    /// Optional originating channel (e.g. discord/slack/cli). Persisted so that
    /// anonymous principals can still resolve channel scope on later recall
    /// (FIX-P1-08).
    pub channel: Option<String>,
}

/// A private worker memory draft waiting for parent merge/reject.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDraftInput {
    pub draft_id: Option<String>,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub worker_run_id: String,
    pub parent_run_id: Option<String>,
    pub session_key: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub key: String,
    pub content: String,
    pub category: MemoryCategory,
    pub source_event_id: Option<String>,
    pub visibility: MemoryVisibility,
    pub payload_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDraft {
    pub id: i64,
    pub draft_id: String,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub worker_run_id: String,
    pub parent_run_id: Option<String>,
    pub session_key: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub key: String,
    pub content: String,
    pub category: MemoryCategory,
    pub source_event_id: Option<String>,
    pub visibility: MemoryVisibility,
    pub status: String,
    pub payload_json: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentIngestInput {
    pub document_id: Option<String>,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    pub task_id: Option<String>,
    pub source_message_event_id: Option<String>,
    pub source_kind: String,
    pub source_uri: Option<String>,
    pub title: Option<String>,
    pub content: String,
    pub mime_type: Option<String>,
    pub visibility: MemoryVisibility,
    pub metadata_json: Option<String>,
    /// V02-R6: scoping keys copied onto the `document.ingested` memory event so
    /// it is readable through the same predicate arms as every other memory
    /// event. Without them the event row carries `NULL` in the columns the
    /// `session`/`agent`/`private` arms compare against, which no principal can
    /// ever match. They scope the event row only; the document's own visibility
    /// and owner columns are unchanged.
    pub session_key: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentRecord {
    pub id: i64,
    pub document_id: String,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    pub task_id: Option<String>,
    pub source_message_event_id: Option<String>,
    pub source_kind: String,
    pub source_uri: Option<String>,
    pub title: Option<String>,
    pub content_sha256: String,
    pub mime_type: Option<String>,
    pub visibility: MemoryVisibility,
    pub metadata_json: Option<String>,
    pub chunk_count: usize,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChunkRecord {
    pub id: i64,
    pub chunk_id: String,
    pub document_id: String,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    pub task_id: Option<String>,
    pub chunk_index: usize,
    pub heading: Option<String>,
    pub content: String,
    pub content_sha256: String,
    pub source_anchor: String,
    pub token_estimate: usize,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSearchResult {
    pub chunk: DocumentChunkRecord,
    pub score: f32,
    /// `source_kind` of the owning document (e.g. `"tool_output"`,
    /// `"untrusted_external"`). FIX-P2-02: carried up from the `documents` row so
    /// the recall→context path can hard-filter untrusted external copies before
    /// they are injected as trusted agent context. `None` when the backend cannot
    /// resolve it (e.g. backends without a documents table).
    pub source_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievedContextItem {
    pub source: String,
    pub document_id: Option<String>,
    pub chunk_id: Option<String>,
    pub source_anchor: Option<String>,
    pub score: Option<f32>,
    pub token_estimate: Option<usize>,
    pub payload_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryLinkInput {
    pub link_id: Option<String>,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub memory_key: Option<String>,
    pub memory_event_id: Option<String>,
    pub message_event_id: Option<String>,
    pub document_id: String,
    pub chunk_id: Option<String>,
    pub link_type: String,
    pub payload_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryLink {
    pub id: i64,
    pub link_id: String,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub memory_key: Option<String>,
    pub memory_event_id: Option<String>,
    pub message_event_id: Option<String>,
    pub document_id: String,
    pub chunk_id: Option<String>,
    pub link_type: String,
    pub payload_json: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalTraceInput {
    pub trace_id: Option<String>,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub session_key: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub source: String,
    pub query: String,
    pub candidate_count: usize,
    pub selected_count: usize,
    pub dropped_count: usize,
    pub budget_tokens: Option<usize>,
    pub selected_json: Option<String>,
    pub dropped_json: Option<String>,
    pub payload_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalTrace {
    pub id: i64,
    pub trace_id: String,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub session_key: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub source: String,
    pub query: String,
    pub candidate_count: usize,
    pub selected_count: usize,
    pub dropped_count: usize,
    pub budget_tokens: Option<usize>,
    pub selected_json: Option<String>,
    pub dropped_json: Option<String>,
    pub payload_json: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionSourceEventRange {
    pub first_event_id: String,
    pub last_event_id: String,
    pub first_row_id: i64,
    pub last_row_id: i64,
    pub source_event_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionRunInput {
    pub run_id: Option<String>,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub session_key: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub trigger: String,
    pub mode: String,
    pub source_message_count: usize,
    pub source_token_estimate: usize,
    pub summary: String,
    pub summary_memory_key: Option<String>,
    pub source_event_ids_json: Option<String>,
    #[serde(default)]
    pub source_event_range_json: Option<String>,
    pub source_document_refs_json: Option<String>,
    pub fidelity_status: String,
    pub payload_json: Option<String>,
}

impl CompactionRunInput {
    pub fn validate_source_event_provenance(&self) -> anyhow::Result<()> {
        let (Some(ids_json), Some(range_json)) = (
            self.source_event_ids_json.as_deref(),
            self.source_event_range_json.as_deref(),
        ) else {
            anyhow::ensure!(
                self.source_event_ids_json.is_none() && self.source_event_range_json.is_none(),
                "compaction source event IDs and covered range must be supplied together"
            );
            return Ok(());
        };

        let ids: Vec<String> = serde_json::from_str(ids_json)
            .map_err(|error| anyhow::anyhow!("compaction source_event_ids_json must be a string array: {error}"))?;
        anyhow::ensure!(!ids.is_empty(), "compaction source event IDs must not be empty");
        anyhow::ensure!(
            ids.iter().all(|id| !id.trim().is_empty()),
            "compaction source event IDs must not contain blank values"
        );
        let unique = ids.iter().collect::<std::collections::HashSet<_>>();
        anyhow::ensure!(
            unique.len() == ids.len(),
            "compaction source event IDs must not contain duplicates"
        );

        let range: CompactionSourceEventRange = serde_json::from_str(range_json)
            .map_err(|error| anyhow::anyhow!("invalid compaction source event range: {error}"))?;
        anyhow::ensure!(
            range.first_row_id > 0,
            "compaction source event range must start after row zero"
        );
        anyhow::ensure!(
            range.last_row_id >= range.first_row_id,
            "compaction source event range rows are reversed"
        );
        anyhow::ensure!(
            ids.first() == Some(&range.first_event_id) && ids.last() == Some(&range.last_event_id),
            "compaction covered range endpoints must match the source event ID list"
        );
        anyhow::ensure!(
            range.source_event_count == ids.len(),
            "compaction covered range count must match the source event ID list"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionRun {
    pub id: i64,
    pub run_id: String,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub session_key: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub trigger: String,
    pub mode: String,
    pub source_message_count: usize,
    pub source_token_estimate: usize,
    pub summary: String,
    pub summary_memory_key: Option<String>,
    pub source_event_ids_json: Option<String>,
    #[serde(default)]
    pub source_event_range_json: Option<String>,
    pub source_document_refs_json: Option<String>,
    pub fidelity_status: String,
    pub payload_json: Option<String>,
    pub created_at: String,
}

/// Whether a backend's *default*, unscoped read must answer nothing
/// (C03 / V02-R5, follow-up F2).
///
/// The context-carrying read defaults on [`Memory`] fall back to the unscoped
/// `recall` / `get`, which cannot express an owner boundary: those backends
/// store no `owner_id` and have no visibility predicate to attach one to. An
/// owner-isolated caller would therefore read every row in the store — the
/// same defect the SQL adapters were fixed for, reachable on every backend
/// that does not override those methods (`markdown`, `none`, `lucid`, the wasm
/// plugin wrappers, and an unknown backend name, which falls back to
/// `markdown`).
///
/// Fail closed instead: no attribution, no rows. `SqliteMemory` and
/// `PostgresMemory` override every one of those methods with a real
/// owner-scoped predicate, so they never reach this and keep serving an
/// isolated caller its own rows. A caller that is not owner-isolated —
/// including one with no context at all — is untouched.
///
/// The log line carries the backend name and the operation only: no key, no
/// query, no owner id, no content.
fn unscoped_read_is_withheld(backend: &str, context: Option<&MemoryWriteContext>, operation: &str) -> bool {
    if !context.is_some_and(|context| context.require_owner_match) {
        return false;
    }
    tracing::warn!(
        backend = backend,
        operation = operation,
        "memory backend cannot scope a read to an owner; the owner-isolated caller is served nothing"
    );
    true
}

/// An owner-isolated context is enforced whatever mode was requested, so an
/// `Observe` request cannot route around the fail-closed defaults above. This
/// mirrors the override both SQL adapters already apply in their own
/// `*_with_context_mode`.
const fn enforced_read_mode(context: Option<&MemoryWriteContext>, requested: MemoryReadMode) -> MemoryReadMode {
    match context {
        Some(context) if context.require_owner_match => MemoryReadMode::Enforce,
        _ => requested,
    }
}

/// Core memory trait — implement for any persistence backend
#[async_trait]
pub trait Memory: Send + Sync {
    /// Backend name
    fn name(&self) -> &str;

    /// Store a memory entry, optionally scoped to a session
    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()>;

    /// Store a memory entry with optional channel/tool context.
    ///
    /// Backends that do not support context-aware metadata can ignore `context`
    /// by relying on this default implementation.
    async fn store_with_context(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<()> {
        // V02-R9: this default stores no owner, so it cannot keep an
        // owner-isolated caller inside its own keys. Refuse the write rather
        // than let it overwrite whatever the key holds. `SqliteMemory` and
        // `PostgresMemory` override this with an owner-guarded upsert.
        if context.is_some_and(|context| context.require_owner_match) {
            return Err(super::owner_predicate::OwnerScopedStoreRefusal::UnscopedBackend {
                backend: self.name().to_string(),
            }
            .into());
        }
        self.store(key, content, category, session_id).await
    }

    /// Store a memory entry with portable fabric metadata.
    ///
    /// Backends that have not implemented metadata persistence can safely fall
    /// back to `store`, preserving compatibility while SQLite/Postgres converge
    /// on the same schema.
    async fn store_with_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        metadata: MemoryStoreMetadata,
    ) -> anyhow::Result<()> {
        let _ = metadata;
        self.store(key, content, category, session_id).await
    }

    /// Store a memory entry with both channel/tool context and fabric metadata.
    async fn store_with_context_and_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        metadata: MemoryStoreMetadata,
    ) -> anyhow::Result<()> {
        let _ = metadata;
        self.store_with_context(key, content, category, session_id, context)
            .await
    }

    /// Recall memories matching a query (keyword search), optionally scoped to a session
    async fn recall(&self, query: &str, limit: usize, session_id: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>>;

    /// Runtime statistics read; verifies a durable terminal receipt and source.
    /// One actual selected Router-history purpose, consumed without lexical recall fallback.
    async fn read_router_history(
        &self,
        evidence: super::RouterHistoryReadEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<super::RouterHistoryRead> {
        let _ = (evidence, deadline);
        Err(crate::memory::fabric::fail_fast(self.name(), "read_router_history"))
    }

    /// Persist one actual terminal outcome and its retained local embedding.
    async fn commit_router_history(
        &self,
        evidence: &super::RouterHistoryWriteEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<super::RouterHistoryCommitReceipt> {
        let _ = (evidence, deadline);
        Err(crate::memory::fabric::fail_fast(self.name(), "commit_router_history"))
    }

    async fn read_router_learning(
        &self,
        evidence: &RouterLearningEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<RouterLearningRead> {
        let _ = (evidence, deadline);
        Err(crate::memory::fabric::fail_fast(self.name(), "read_router_learning"))
    }

    /// Atomically commit a bounded statistics snapshot and permanent terminal deduplication.
    async fn commit_router_learning(
        &self,
        evidence: &RouterLearningEvidence,
        expected_version: i64,
        next: &RouterLearningSnapshot,
        deadline: std::time::Instant,
    ) -> anyhow::Result<RouterLearningCommit> {
        let _ = (evidence, expected_version, next, deadline);
        Err(crate::memory::fabric::fail_fast(self.name(), "commit_router_learning"))
    }

    /// Count at most `limit` keyword hits visible to a trusted source scope.
    /// Rehearsal must not compute embeddings, cache, log access, or return
    /// content. Backends without a strictly scoped read-only path fail closed.
    async fn count_rehearsal_hits(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        deadline: std::time::Instant,
    ) -> anyhow::Result<u32> {
        let _ = (query, limit, context, deadline);
        Err(crate::memory::fabric::fail_fast(self.name(), "count_rehearsal_hits"))
    }

    /// Read selected Memory evidence with source-owned ACL scope. An optional
    /// session filter is applied in the backend before ranking/limit; callers
    /// must authorize its value from the source rather than model arguments.
    /// Returns stored key/category/session metadata, but no semantic score or
    /// ancillary metadata. Unsupported backends fail closed instead of using
    /// semantic recall with side effects.
    async fn retrieve_decision_memory(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        session_id: Option<&str>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<DecisionMemoryHit>> {
        let _ = (query, limit, context, session_id, deadline);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "retrieve_decision_memory",
        ))
    }

    /// Read selected document evidence locally without embedding generation,
    /// cache writes or unscoped fallback. The caller owns the principal and deadline.
    async fn retrieve_decision_documents(
        &self,
        query: &str,
        limit: u8,
        principal: &MemoryPrincipal,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<DecisionDocumentHit>> {
        let _ = (query, limit, principal, deadline);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "retrieve_decision_documents",
        ))
    }

    /// Read bounded session history strictly before the runtime-owned source
    /// event. No embeddings, unscoped fallback, or current-input echo is allowed.
    async fn retrieve_decision_session(
        &self,
        principal: &MemoryPrincipal,
        before_event_id: &str,
        limit: u8,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<DecisionSessionHit>> {
        let _ = (principal, before_event_id, limit, deadline);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "retrieve_decision_session",
        ))
    }

    /// Recall memories matching a query with optional runtime ACL/write context.
    ///
    /// Backends without ACL-aware recall support fall back to legacy recall —
    /// except for an owner-isolated caller, which this default cannot serve at
    /// all; see [`unscoped_read_is_withheld`].
    async fn recall_with_context(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        if unscoped_read_is_withheld(self.name(), context, "recall") {
            return Ok(Vec::new());
        }
        self.recall(query, limit, session_id).await
    }

    /// Backend-neutral recall for user-facing readers with ACL
    /// enforcement/observation modes.
    async fn recall_with_context_mode(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        match enforced_read_mode(context, mode) {
            MemoryReadMode::Enforce => self.recall_with_context(query, limit, session_id, context).await,
            MemoryReadMode::Observe => self.recall(query, limit, session_id).await,
        }
    }

    /// Get a specific memory by key
    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>>;

    /// Get a specific memory by key under an optional runtime ACL context.
    async fn get_with_context(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        if unscoped_read_is_withheld(self.name(), context, "keyed read") {
            return Ok(None);
        }
        self.get(key).await
    }

    /// Backend-neutral keyed read for ACL enforcement/observation modes.
    async fn get_with_context_mode(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        match enforced_read_mode(context, mode) {
            MemoryReadMode::Enforce => self.get_with_context(key, context).await,
            MemoryReadMode::Observe => self.get(key).await,
        }
    }

    /// List all memory keys, optionally filtered by category and/or session
    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>>;

    /// The owner of every row in `category`, by row id, for maintenance that
    /// must never act across owners (deduplication, for one).
    ///
    /// `None` means this backend keeps no per-row owner, so every row belongs
    /// to the one workspace. A backend that stores owners (`SqliteMemory`,
    /// `PostgresMemory`) must override this: `None` from it would let such
    /// maintenance treat two owners' rows as one owner's.
    async fn list_owner_ids(
        &self,
        category: &MemoryCategory,
    ) -> anyhow::Result<Option<std::collections::HashMap<String, Option<String>>>> {
        let _ = category;
        Ok(None)
    }

    /// Remove a memory by key
    async fn forget(&self, key: &str) -> anyhow::Result<bool>;

    /// Mirror a write the **local operator** made into the backend's workspace
    /// Markdown files (`MEMORY.md` for `core`, `memory/<date>.md` otherwise),
    /// when the backend keeps such a mirror (D-026).
    ///
    /// `MEMORY.md` is inlined into the system prompt of every surface, so text
    /// placed there steers every user's turns. The caller — `memory_store` —
    /// invokes this only for a write that came from the terminal UI or the
    /// `prx agent` CLI; every other origin (webhook, channel, gateway,
    /// Console, MCP, sub-agent) stays in the owner-scoped store. Backends with
    /// no separate mirror do nothing.
    async fn mirror_operator_write(&self, key: &str, content: &str, category: &MemoryCategory) -> anyhow::Result<()> {
        let _ = (key, content, category);
        Ok(())
    }

    /// Whether this backend's store *is* the shared prompt material: the
    /// `markdown` backend keeps memory in `MEMORY.md` and the daily logs
    /// themselves, so a store on it cannot be kept out of every surface's
    /// system prompt (D-026). `memory_store` refuses such a backend to every
    /// origin but the local operator.
    fn stores_into_shared_prompt_files(&self) -> bool {
        false
    }

    /// Remove a memory by key after applying a runtime write/read scope.
    ///
    /// Backends that do not support ACL metadata fall back to legacy key
    /// deletion for compatibility; SQLite and PostgreSQL override this with
    /// scoped deletion.
    ///
    /// The owner boundary applies to the delete as it does to the reads above.
    /// A backend that cannot prove who owns a row cannot prove the row belongs
    /// to an owner-isolated caller either, so it deletes nothing and reports
    /// that nothing was deleted. `memory_forget` is not on the default MCP
    /// surface, but an operator may expose it; without this, the caller that
    /// cannot *read* another owner's row could still delete it by key.
    /// A caller that is not owner-isolated — including one with no context at
    /// all — keeps the legacy unscoped delete unchanged.
    async fn forget_with_context(&self, key: &str, context: Option<&MemoryWriteContext>) -> anyhow::Result<bool> {
        if unscoped_read_is_withheld(self.name(), context, "forget") {
            return Ok(false);
        }
        self.forget(key).await
    }

    /// Increment the usefulness feedback counter for a recalled memory entry.
    ///
    /// Backends that do not persist `useful_count` can safely no-op.
    async fn increment_useful_count(&self, id: &str) -> anyhow::Result<()> {
        let _ = id;
        Ok(())
    }

    /// Count total memories
    async fn count(&self) -> anyhow::Result<usize>;

    /// Health check
    async fn health_check(&self) -> bool;

    /// Rebuild backend search indexes and backfill stale vectors when supported.
    async fn reindex(&self) -> anyhow::Result<usize> {
        Ok(0)
    }

    /// Whether this backend can durably ingest source documents and chunks
    /// (FIX-P0-28). Backends with full document support (SQLite/Postgres) keep the
    /// default `true`. Backends without it (`NoneMemory`, `MarkdownMemory`) return
    /// `false` so callers can skip ingestion and gracefully degrade to plain
    /// history compaction instead of emitting a fail-fast error on every large
    /// tool output.
    fn supports_document_ingest(&self) -> bool {
        true
    }

    /// Upsert channel-provided metadata for the exact conversation without
    /// touching self-maintained purpose/notes/tags.
    async fn upsert_chat_profile_metadata(
        &self,
        channel: &str,
        chat_id: &str,
        chat_kind: &str,
        title: Option<&str>,
    ) -> anyhow::Result<()> {
        let _ = (channel, chat_id, chat_kind, title);
        Ok(())
    }

    /// Update the current conversation profile. The caller is responsible for
    /// deriving channel/chat_id from trusted runtime scope.
    async fn update_chat_profile(
        &self,
        channel: &str,
        chat_id: &str,
        chat_kind: &str,
        purpose: Option<&str>,
        notes: Option<&str>,
        tags: Option<&[String]>,
        updated_by: &str,
    ) -> anyhow::Result<ChatProfile> {
        let _ = (channel, chat_id, chat_kind, purpose, notes, tags, updated_by);
        Err(crate::memory::fabric::fail_fast(self.name(), "update_chat_profile"))
    }

    /// Load the exact conversation profile by `(channel, chat_id)`.
    async fn get_chat_profile(&self, channel: &str, chat_id: &str) -> anyhow::Result<Option<ChatProfile>> {
        let _ = (channel, chat_id);
        Ok(None)
    }

    /// Append a persisted channel conversation turn.
    ///
    /// Backends without conversation persistence can no-op.
    #[allow(clippy::too_many_arguments)]
    async fn append_conversation_turn(
        &self,
        session_key: &str,
        channel: &str,
        sender: &str,
        role: &str,
        content: &str,
        timestamp: Option<&str>,
        message_id: Option<&str>,
        owner_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let _ = (
            session_key,
            channel,
            sender,
            role,
            content,
            timestamp,
            message_id,
            owner_id,
        );
        Ok(())
    }

    /// Project an exact durable Console message once, including the session
    /// counter update. Repeating the same event is a no-op; conflicts fail.
    /// Implementations must verify the stored event inside the transaction.
    async fn project_console_message(&self, event: &MessageEvent) -> anyhow::Result<()> {
        let _ = event;
        Err(crate::memory::fabric::fail_fast(self.name(), "project_console_message"))
    }

    /// Enumerate committed Console terminal markers for explicit recovery.
    /// Returned rows are candidates only; the caller must authenticate replay.
    async fn list_console_terminal_events(
        &self,
        workspace_id: &str,
        session_key: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = (workspace_id, session_key, after_id, limit);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_console_terminal_events",
        ))
    }

    /// List persisted conversation sessions ordered by most recently updated.
    async fn list_conversation_sessions(
        &self,
        limit: usize,
        offset: usize,
        channel: Option<&str>,
    ) -> anyhow::Result<Vec<ConversationSessionSummary>> {
        let _ = (limit, offset, channel);
        Ok(Vec::new())
    }

    /// Get one persisted conversation session by key.
    async fn get_conversation_session(&self, session_key: &str) -> anyhow::Result<Option<ConversationSessionSummary>> {
        let _ = session_key;
        Ok(None)
    }

    /// List persisted turns for a conversation session (oldest first).
    async fn list_conversation_turns(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<ConversationTurn>> {
        let _ = (principal, session_key, limit, offset);
        Ok(Vec::new())
    }

    /// Read the newest Console history window in event-time order, reversing
    /// the selected page for chronological presentation. Recovery insertion
    /// order must not move an old event into the newest page.
    async fn list_console_conversation_turns(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<ConversationTurn>> {
        let _ = (principal, session_key, limit, offset);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_console_conversation_turns",
        ))
    }

    /// Load recent turns per session for runtime history hydration.
    async fn load_recent_conversation_histories(
        &self,
        principal: &MemoryPrincipal,
        max_turns_per_session: usize,
        max_sessions: usize,
    ) -> anyhow::Result<HashMap<String, Vec<ConversationTurn>>> {
        let _ = (principal, max_turns_per_session, max_sessions);
        Ok(HashMap::new())
    }

    /// Append a normalized message event into the shared memory fabric.
    ///
    /// Backends without event-log support must fail fast instead of returning
    /// synthetic success, so runtime timeline loss is visible to callers.
    async fn append_message_event(&self, input: MessageEventInput) -> anyhow::Result<MessageEvent> {
        let _ = input;
        Err(crate::memory::fabric::fail_fast(self.name(), "append_message_event"))
    }

    /// Create a backend lifetime owner. Unsupported backends fail explicitly.
    async fn acquire_ingress_lifetime(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<std::sync::Arc<dyn super::ingress_recovery::IngressLifetime>> {
        let _ = (workspace_id, token, deadline);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "acquire_ingress_lifetime",
        ))
    }

    /// Input, capsule and indexed projection must commit in one transaction.
    async fn append_ingress_input(
        &self,
        input: MessageEventInput,
        capsule: super::ingress_recovery::IngressCapsule,
        lifetime: std::sync::Arc<dyn super::ingress_recovery::IngressLifetime>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<super::ingress_recovery::IngressAppendOutcome> {
        let _ = (input, capsule, lifetime, deadline);
        Err(crate::memory::fabric::fail_fast(self.name(), "append_ingress_input"))
    }

    async fn list_ingress_recovery_candidates(
        &self,
        scope: &super::ingress_recovery::IngressRecoveryScope,
        current: std::sync::Arc<dyn super::ingress_recovery::IngressLifetime>,
        phase: super::ingress_recovery::IngressRecoveryScan,
        after_id: i64,
        limit: usize,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<super::ingress_recovery::IngressRecoveryCandidate>> {
        let _ = (scope, current, phase, after_id, limit, deadline);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_ingress_recovery_candidates",
        ))
    }

    async fn read_ingress_event(
        &self,
        scope: &super::ingress_recovery::IngressRecoveryScope,
        lookup: &super::ingress_recovery::IngressEventLookup,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Option<MessageEvent>> {
        let _ = (scope, lookup, deadline);
        Err(crate::memory::fabric::fail_fast(self.name(), "read_ingress_event"))
    }

    /// None means another lifetime still owns the lock; no death inference.
    async fn claim_ingress_recovery(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Option<super::ingress_recovery::IngressRecoveryClaim>> {
        let _ = (workspace_id, token, deadline);
        Err(crate::memory::fabric::fail_fast(self.name(), "claim_ingress_recovery"))
    }

    /// Evidence event, outbox and projection transition must commit atomically.
    async fn commit_ingress_recovery(
        &self,
        commit: super::ingress_recovery::IngressRecoveryCommit,
    ) -> anyhow::Result<MessageEvent> {
        let _ = commit;
        Err(crate::memory::fabric::fail_fast(self.name(), "commit_ingress_recovery"))
    }

    /// Bounded evidence repair; this does not grant provider or tool authority.
    async fn append_message_event_until(
        &self,
        input: MessageEventInput,
        deadline: std::time::Instant,
    ) -> anyhow::Result<MessageEvent> {
        let _ = (input, deadline);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "append_message_event_until",
        ))
    }
    async fn project_console_message_until(
        &self,
        event: &MessageEvent,
        deadline: std::time::Instant,
    ) -> anyhow::Result<()> {
        let _ = (event, deadline);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "project_console_message_until",
        ))
    }

    /// Find one MessageEvent by its workspace-scoped idempotency key.
    ///
    /// The default returns no match for backends without event-ledger lookup
    /// support. Persistent event backends override this so execution recovery can
    /// distinguish a committed result from an unowned retry.
    async fn find_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        let _ = (workspace_id, idempotency_key);
        Ok(None)
    }

    /// Bounded runtime recovery discovery, ordered by durable event ID. This
    /// returns launch candidates only; callers must authenticate their signed
    /// provenance before using any owner, run, process or result fields.
    async fn list_worker_launch_events(
        &self,
        workspace_id: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = (workspace_id, after_id, limit);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_worker_launch_events",
        ))
    }

    /// Indexed recovery lookup with owner isolation. Unsupported backends must
    /// not turn an unavailable ledger into evidence that no segment existed.
    async fn find_owned_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        owner_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        let _ = (workspace_id, owner_id, idempotency_key);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "find_owned_message_event_by_idempotency_key",
        ))
    }

    /// Read one durable event for an exact workspace and owner. Runtime replay
    /// uses this for its original source receipt; it grants no caller authority.
    async fn find_owned_message_event(
        &self,
        workspace_id: &str,
        owner_id: &str,
        event_id: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        let _ = (workspace_id, owner_id, event_id);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "find_owned_message_event",
        ))
    }

    /// List message events visible to `principal` after a cursor.
    async fn list_message_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = (principal, after_id, limit);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_message_events_since",
        ))
    }

    /// List the most recent message events visible to `principal`.
    async fn list_message_events_recent(
        &self,
        principal: &MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = (principal, limit);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_message_events_recent",
        ))
    }

    /// List message events in an RFC 3339 half-open time range.
    ///
    /// The default preserves compatibility for external backends by paging the
    /// existing cursor API. Indexed backends should override this to avoid a
    /// lifetime scan for daily analytics. Supplying
    /// `descendant_workspace_root` requests a privileged system-only query over
    /// every visibility in the principal workspace and that descendant tree;
    /// backends that cannot enforce that boundary must reject it.
    async fn list_message_events_time_range(
        &self,
        principal: &MemoryPrincipal,
        descendant_workspace_root: Option<&str>,
        start: &str,
        end: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        anyhow::ensure!(
            descendant_workspace_root.is_none(),
            "this memory backend does not support system-scoped descendant workspace event queries"
        );
        let start = chrono::DateTime::parse_from_rfc3339(start)?.with_timezone(&chrono::Utc);
        let end = chrono::DateTime::parse_from_rfc3339(end)?.with_timezone(&chrono::Utc);
        let mut cursor = 0_i64;
        let mut output = Vec::new();
        loop {
            let page = self.list_message_events_since(principal, cursor, 500).await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |event| event.id);
            let page_len = page.len();
            output.extend(page.into_iter().filter(|event| {
                chrono::DateTime::parse_from_rfc3339(&event.created_at).is_ok_and(|time| time >= start && time < end)
            }));
            if output.len() >= limit || page_len < 500 {
                break;
            }
        }
        output.truncate(limit);
        Ok(output)
    }

    /// Load recent shared context events visible to an agent turn.
    async fn load_recent_shared_context(&self, query: SharedContextQuery) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = query;
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "load_recent_shared_context",
        ))
    }

    /// Load recent current-session context events visible to an agent turn.
    async fn load_recent_session_context(&self, query: SessionContextQuery) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = query;
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "load_recent_session_context",
        ))
    }

    /// Append a memory outbox event.
    async fn append_memory_event(&self, input: MemoryEventInput) -> anyhow::Result<MemoryEvent> {
        let _ = input;
        Err(crate::memory::fabric::fail_fast(self.name(), "append_memory_event"))
    }

    /// List memory outbox events visible to `principal` after a cursor.
    async fn list_memory_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEvent>> {
        let _ = (principal, after_id, limit);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_memory_events_since",
        ))
    }

    /// List the most recent memory outbox events visible to `principal`.
    async fn list_memory_events_recent(
        &self,
        principal: &MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEvent>> {
        let _ = (principal, limit);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_memory_events_recent",
        ))
    }

    /// Create a private worker memory draft.
    async fn create_memory_draft(&self, input: MemoryDraftInput) -> anyhow::Result<MemoryDraft> {
        let _ = input;
        Err(crate::memory::fabric::fail_fast(self.name(), "create_memory_draft"))
    }

    /// List memory drafts produced by a worker run, scoped to `principal`.
    ///
    /// Only drafts owned by the principal (or with no owner) are returned, so a
    /// caller that merely knows a `worker_run_id` cannot read another owner's
    /// drafts.
    async fn list_memory_drafts_for_run(
        &self,
        principal: &MemoryPrincipal,
        worker_run_id: &str,
    ) -> anyhow::Result<Vec<MemoryDraft>> {
        let _ = (principal, worker_run_id);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_memory_drafts_for_run",
        ))
    }

    /// Mark a draft as merged into semantic memory, enforcing owner ACL.
    ///
    /// Returns `Err` if the draft is owned by a different principal.
    async fn merge_memory_draft(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
    ) -> anyhow::Result<Option<MemoryDraft>> {
        let _ = (principal, draft_id);
        Err(crate::memory::fabric::fail_fast(self.name(), "merge_memory_draft"))
    }

    /// Reject a draft without merging it into semantic memory, enforcing owner ACL.
    ///
    /// Returns `Err` if the draft is owned by a different principal.
    async fn reject_memory_draft(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        reason: Option<&str>,
    ) -> anyhow::Result<Option<MemoryDraft>> {
        let _ = (principal, draft_id, reason);
        Err(crate::memory::fabric::fail_fast(self.name(), "reject_memory_draft"))
    }

    /// Persist a new evolution proposal draft (FIX-P0-03).
    ///
    /// Writes one `evolution_proposals` row plus a `proposal.drafted` event and
    /// returns the stored `draft_id`. Backends without evolution support keep the
    /// default `Err` so callers see backend gaps instead of silent loss.
    async fn create_evolution_proposal(
        &self,
        draft: crate::self_system::evolution::EvolutionProposalDraft,
    ) -> anyhow::Result<String> {
        let _ = draft;
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "create_evolution_proposal",
        ))
    }

    /// List evolution proposals visible to `principal`, filtered by `filter`.
    ///
    /// Non-system principals are constrained to their own `owner_id`; system
    /// principals (`self_system`/`router`/`internal`/`system`) may query globally.
    async fn list_evolution_proposals(
        &self,
        principal: &MemoryPrincipal,
        filter: crate::self_system::evolution::ProposalFilter,
    ) -> anyhow::Result<Vec<crate::self_system::evolution::EvolutionProposalDraft>> {
        let _ = (principal, filter);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_evolution_proposals",
        ))
    }

    /// Fetch one proposal by id, enforcing owner ACL.
    ///
    /// Returns `Ok(None)` for a missing OR cross-owner draft (NotFound rather
    /// than Forbidden) to avoid a cross-owner existence side channel.
    async fn get_evolution_proposal(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
    ) -> anyhow::Result<Option<crate::self_system::evolution::EvolutionProposalDraft>> {
        let _ = (principal, draft_id);
        Err(crate::memory::fabric::fail_fast(self.name(), "get_evolution_proposal"))
    }

    /// Apply a status transition (judge / apply / rollback) to a proposal.
    ///
    /// Each transition appends the matching `evolution_proposal_events` row and
    /// mutates the proposal columns. Re-judging an already-judged proposal is
    /// rejected to prevent verdict laundering.
    async fn update_evolution_proposal_status(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        update: crate::self_system::evolution::ProposalStatusUpdate,
    ) -> anyhow::Result<()> {
        let _ = (principal, draft_id, update);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "update_evolution_proposal_status",
        ))
    }

    /// Append a free-form lifecycle event to an existing proposal.
    ///
    /// Used for non-state-changing markers such as `proposal.evidence_mismatch`.
    async fn append_evolution_proposal_event(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        event_type: &str,
        actor: &str,
        payload_json: Option<&str>,
    ) -> anyhow::Result<()> {
        let _ = (principal, draft_id, event_type, actor, payload_json);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "append_evolution_proposal_event",
        ))
    }

    /// Count proposals visible to `principal` matching `filter` (cheap status probe).
    async fn count_evolution_proposals(
        &self,
        principal: &MemoryPrincipal,
        filter: crate::self_system::evolution::ProposalFilter,
    ) -> anyhow::Result<usize> {
        let proposals = self.list_evolution_proposals(principal, filter).await?;
        Ok(proposals.len())
    }

    /// Soft-delete a memory key into a trash holding area with a grace expiry
    /// (FIX-P1-11). Unlike `forget`, this MUST NOT physically remove the row;
    /// it snapshots the value and marks it recoverable until `grace_until`.
    ///
    /// Returns the trash entry id on success, or `Ok(None)` if the key is absent.
    /// Backends without trash support keep the default `Err` so callers do not
    /// silently fall back to a hard delete.
    async fn move_to_trash(&self, key: &str, reason: &str, grace_days: u32) -> anyhow::Result<Option<String>> {
        let _ = (key, reason, grace_days);
        Err(crate::memory::fabric::fail_fast(self.name(), "move_to_trash"))
    }

    /// Ingest a durable source document and create chunk records.
    async fn ingest_document(&self, input: DocumentIngestInput) -> anyhow::Result<DocumentRecord> {
        let _ = input;
        Err(crate::memory::document::fail_fast(self.name(), "ingest_document"))
    }

    /// Search document chunks visible to a principal.
    async fn search_document_chunks(
        &self,
        principal: &MemoryPrincipal,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<DocumentSearchResult>> {
        let _ = (principal, query, limit);
        Err(crate::memory::document::fail_fast(
            self.name(),
            "search_document_chunks",
        ))
    }

    /// Retrieve one document chunk by stable chunk id.
    async fn get_document_chunk(&self, chunk_id: &str) -> anyhow::Result<Option<DocumentChunkRecord>> {
        let _ = chunk_id;
        Err(crate::memory::document::fail_fast(self.name(), "get_document_chunk"))
    }

    /// Link a memory/event/summary to a source document chunk.
    async fn link_memory_source(&self, input: MemoryLinkInput) -> anyhow::Result<MemoryLink> {
        let _ = input;
        Err(crate::memory::document::fail_fast(self.name(), "link_memory_source"))
    }

    /// Append a retrieval trace for context packing and evidence audits.
    async fn append_retrieval_trace(&self, input: RetrievalTraceInput) -> anyhow::Result<RetrievalTrace> {
        let _ = input;
        Err(crate::memory::retrieval::fail_fast(
            self.name(),
            "append_retrieval_trace",
        ))
    }

    /// Append a compaction run audit record.
    async fn append_compaction_run(&self, input: CompactionRunInput) -> anyhow::Result<CompactionRun> {
        let _ = input;
        Err(crate::memory::compaction::fail_fast(
            self.name(),
            "append_compaction_run",
        ))
    }
}

#[cfg(test)]
pub(crate) mod conformance {
    use super::{Memory, MemoryCategory, MemoryReadMode};
    use crate::memory::principal::MemoryWriteContext;

    pub(crate) async fn assert_scoped_memory_acl_conformance(mem: &dyn Memory, key_prefix: &str) {
        let alice_ctx = MemoryWriteContext {
            workspace_id: Some("workspace-a".into()),
            channel: Some("telegram".into()),
            chat_type: Some("private".into()),
            chat_id: Some(format!("{key_prefix}:dm-alice")),
            sender_id: None,
            raw_sender: Some("alice".into()),
            owner_id: None,
            require_owner_match: false,
        };
        let bob_ctx = MemoryWriteContext {
            workspace_id: Some("workspace-a".into()),
            channel: Some("telegram".into()),
            chat_type: Some("private".into()),
            chat_id: Some(format!("{key_prefix}:dm-bob")),
            sender_id: None,
            raw_sender: Some("bob".into()),
            owner_id: None,
            require_owner_match: false,
        };
        let alice_key = format!("{key_prefix}:alice-private");
        let bob_key = format!("{key_prefix}:bob-private");

        mem.store_with_context(
            &alice_key,
            "shared conformance keyword alice private",
            MemoryCategory::Conversation,
            None,
            Some(&alice_ctx),
        )
        .await
        .expect("store alice scoped memory");
        mem.store_with_context(
            &bob_key,
            "shared conformance keyword bob private",
            MemoryCategory::Conversation,
            None,
            Some(&bob_ctx),
        )
        .await
        .expect("store bob scoped memory");

        let alice_results = mem
            .recall_with_context("shared conformance keyword", 10, None, Some(&alice_ctx))
            .await
            .expect("recall alice scoped memory");
        let alice_keys = alice_results.iter().map(|entry| entry.key.as_str()).collect::<Vec<_>>();
        assert!(alice_keys.contains(&alice_key.as_str()), "{alice_keys:?}");
        assert!(!alice_keys.contains(&bob_key.as_str()), "{alice_keys:?}");

        assert!(
            mem.get_with_context_mode(&bob_key, Some(&alice_ctx), MemoryReadMode::Enforce)
                .await
                .expect("enforced keyed read")
                .is_none(),
            "enforce mode must hide another sender's private memory"
        );
        assert!(
            mem.get_with_context_mode(&bob_key, Some(&alice_ctx), MemoryReadMode::Observe)
                .await
                .expect("observed keyed read")
                .is_some(),
            "observe mode must preserve legacy visibility while recording the would-deny decision"
        );
        let enforced_search = mem
            .recall_with_context_mode(
                "shared conformance keyword bob private",
                10,
                None,
                Some(&alice_ctx),
                MemoryReadMode::Enforce,
            )
            .await
            .expect("enforced scoped search");
        assert!(!enforced_search.iter().any(|entry| entry.key == bob_key));
        let observed_search = mem
            .recall_with_context_mode(
                "shared conformance keyword bob private",
                10,
                None,
                Some(&alice_ctx),
                MemoryReadMode::Observe,
            )
            .await
            .expect("observed scoped search");
        assert!(observed_search.iter().any(|entry| entry.key == bob_key));

        assert!(
            !mem.forget_with_context(&bob_key, Some(&alice_ctx))
                .await
                .expect("alice cannot delete bob memory"),
            "alice context must not delete bob scoped memory"
        );
        assert!(
            mem.forget_with_context(&alice_key, Some(&alice_ctx))
                .await
                .expect("alice deletes alice memory"),
            "alice context should delete alice scoped memory"
        );
        assert!(
            mem.get(&bob_key).await.expect("bob memory remains").is_some(),
            "bob memory should remain after denied alice delete"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------
    // V02-R5 follow-up F2: the trait-level defaults must fail closed.
    //
    // `SqliteMemory` and `PostgresMemory` override every `*_with_context*`
    // read with a real owner-scoped predicate. Every other backend — markdown,
    // none, lucid and the wasm plugin wrappers — inherits the defaults in this
    // file, which used to discard the context and answer from the unscoped
    // read. An owner-isolated MCP caller therefore read the whole store on any
    // of them, and an unknown backend name falls back to markdown.
    //
    // `MarkdownMemory` is the fixture because it inherits every default here
    // unchanged and really returns rows, so "the caller reads what it must not"
    // is observable rather than argued.
    // ---------------------------------------------------------------------

    const UNSCOPED_KEY: &str = "first-owner-note";
    const UNSCOPED_CONTENT: &str = "alpha rollout note authored by the first owner";
    const UNSCOPED_QUERY: &str = "rollout";

    /// A read context from a trusted, owner-isolated origin whose owner is not
    /// the one that authored the row.
    fn isolated_reader_context(owner_id: Option<&str>) -> MemoryWriteContext {
        MemoryWriteContext {
            workspace_id: Some("ws-isolation".to_string()),
            channel: Some("mcp".to_string()),
            chat_type: Some("gateway".to_string()),
            chat_id: Some("mcp:shared-subject".to_string()),
            sender_id: owner_id.map(str::to_string),
            raw_sender: Some("shared-subject".to_string()),
            owner_id: owner_id.map(str::to_string),
            require_owner_match: true,
        }
    }

    async fn unscoped_backend_with_one_row() -> (tempfile::TempDir, crate::memory::markdown::MarkdownMemory) {
        let workspace = tempfile::TempDir::new().expect("test: temp workspace");
        let memory = crate::memory::markdown::MarkdownMemory::new(workspace.path());
        memory
            .store(UNSCOPED_KEY, UNSCOPED_CONTENT, MemoryCategory::Core, None)
            .await
            .expect("test: store a row on the unscoped backend");
        assert!(
            !memory
                .recall(UNSCOPED_QUERY, 10, None)
                .await
                .expect("test: unscoped recall")
                .is_empty(),
            "sanity: the fixture backend really returns the row to an unscoped read"
        );
        (workspace, memory)
    }

    /// MUTATION GUARD: drop the `require_owner_match` short-circuit from
    /// `recall_with_context` / `get_with_context` in this file and this goes
    /// red — the isolated reader gets the other owner's row back.
    #[tokio::test]
    async fn an_owner_isolated_reader_gets_nothing_from_a_backend_without_owner_scoping() {
        let (_workspace, memory) = unscoped_backend_with_one_row().await;
        let reader = isolated_reader_context(Some("owner:ws-isolation:agent:issuer-b.example:shared-subject"));

        let recalled = memory
            .recall_with_context(UNSCOPED_QUERY, 10, None, Some(&reader))
            .await
            .expect("test: isolated recall");
        assert!(
            recalled.is_empty(),
            "an unscoped backend cannot prove ownership, so it must answer nothing: {:?}",
            recalled.iter().map(|entry| entry.key.as_str()).collect::<Vec<_>>()
        );

        assert!(
            memory
                .get_with_context(UNSCOPED_KEY, Some(&reader))
                .await
                .expect("test: isolated keyed read")
                .is_none(),
            "an exact key must not cross the boundary on an unscoped backend either"
        );
    }

    /// The isolation bit outranks the requested mode here exactly as it does in
    /// both SQL adapters, so an `Observe` request cannot reopen the boundary.
    #[tokio::test]
    async fn an_observe_request_does_not_reopen_the_boundary_on_an_unscoped_backend() {
        let (_workspace, memory) = unscoped_backend_with_one_row().await;
        let reader = isolated_reader_context(Some("owner:ws-isolation:agent:issuer-b.example:shared-subject"));

        assert!(
            memory
                .recall_with_context_mode(UNSCOPED_QUERY, 10, None, Some(&reader), MemoryReadMode::Observe)
                .await
                .expect("test: observe-mode recall under isolation")
                .is_empty(),
            "Observe must not reopen the owner boundary on an unscoped backend"
        );
        assert!(
            memory
                .get_with_context_mode(UNSCOPED_KEY, Some(&reader), MemoryReadMode::Observe)
                .await
                .expect("test: observe-mode keyed read under isolation")
                .is_none(),
            "Observe must not reopen the owner boundary for the keyed read either"
        );
    }

    /// Fail closed, not fail open: an isolated caller with no owner id at all
    /// reaches nothing rather than falling back to the unscoped read.
    #[tokio::test]
    async fn an_isolated_reader_without_an_owner_id_reads_nothing_on_an_unscoped_backend() {
        let (_workspace, memory) = unscoped_backend_with_one_row().await;

        for owner in [None, Some("   ")] {
            let reader = isolated_reader_context(owner);
            assert!(
                memory
                    .recall_with_context(UNSCOPED_QUERY, 10, None, Some(&reader))
                    .await
                    .expect("test: isolated recall without an owner id")
                    .is_empty(),
                "an isolated caller with owner {owner:?} must reach nothing"
            );
        }
    }

    /// The byte-for-byte lock: without the isolation bit the defaults still
    /// delegate to the unscoped read, so every in-process caller is unchanged.
    #[tokio::test]
    async fn a_non_isolated_reader_still_gets_the_legacy_unscoped_answer() {
        let (_workspace, memory) = unscoped_backend_with_one_row().await;
        let mut reader = isolated_reader_context(Some("owner:ws-isolation:agent:issuer-b.example:shared-subject"));
        reader.require_owner_match = false;

        assert!(
            memory
                .recall_with_context(UNSCOPED_QUERY, 10, None, Some(&reader))
                .await
                .expect("test: non-isolated recall")
                .iter()
                .any(|entry| entry.content.contains(UNSCOPED_CONTENT)),
            "a non-isolated caller keeps the historical unscoped answer"
        );
        assert!(
            memory
                .get_with_context(UNSCOPED_KEY, Some(&reader))
                .await
                .expect("test: non-isolated keyed read")
                .is_some(),
            "a non-isolated keyed read keeps the historical unscoped answer"
        );
        assert!(
            memory
                .recall_with_context(UNSCOPED_QUERY, 10, None, None)
                .await
                .expect("test: recall with no context at all")
                .iter()
                .any(|entry| entry.content.contains(UNSCOPED_CONTENT)),
            "a caller with no context at all keeps the historical unscoped answer"
        );
    }

    /// A minimal backend that inherits every default on this trait and whose
    /// `forget` really deletes.
    ///
    /// `MarkdownMemory` is append-only — its `forget` always answers `false` —
    /// so it cannot tell a refusal apart from "nothing was deleted". This
    /// double can, which is the only way to make the delete assertions below
    /// non-vacuous. It stores no owner and has no visibility predicate, exactly
    /// like every backend the defaults serve.
    #[derive(Default)]
    struct DeletableUnscopedBackend {
        rows: parking_lot::Mutex<Vec<(String, String)>>,
    }

    impl DeletableUnscopedBackend {
        fn entry(key: &str, content: &str) -> MemoryEntry {
            MemoryEntry {
                id: key.to_string(),
                key: key.to_string(),
                content: content.to_string(),
                category: MemoryCategory::Core,
                timestamp: "2026-09-21T00:00:00Z".to_string(),
                session_id: None,
                score: None,
                tags: None,
                access_count: None,
                useful_count: None,
                source: None,
                source_confidence: None,
                verification_status: None,
                lifecycle_state: None,
                compressed_from: None,
            }
        }
    }

    #[async_trait]
    impl Memory for DeletableUnscopedBackend {
        fn name(&self) -> &str {
            "deletable-unscoped"
        }

        async fn store(
            &self,
            key: &str,
            content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            self.rows.lock().push((key.to_string(), content.to_string()));
            Ok(())
        }

        async fn recall(
            &self,
            query: &str,
            limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(self
                .rows
                .lock()
                .iter()
                .filter(|(_, content)| content.contains(query))
                .take(limit)
                .map(|(key, content)| Self::entry(key, content))
                .collect())
        }

        async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(self
                .rows
                .lock()
                .iter()
                .find(|(stored, _)| stored == key)
                .map(|(stored, content)| Self::entry(stored, content)))
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(self
                .rows
                .lock()
                .iter()
                .map(|(key, content)| Self::entry(key, content))
                .collect())
        }

        async fn forget(&self, key: &str) -> anyhow::Result<bool> {
            let mut rows = self.rows.lock();
            let before = rows.len();
            rows.retain(|(stored, _)| stored != key);
            Ok(rows.len() != before)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(self.rows.lock().len())
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    /// The delete half of the same boundary: `forget_with_context` is the one
    /// context-carrying method on this trait that used to discard its context,
    /// so delete-by-key was unscoped on every backend that inherits the
    /// default. `memory_forget` is not on the default MCP surface, but an
    /// operator may expose it, and an unscoped delete from an owner-isolated
    /// caller is a cross-owner delete.
    ///
    /// MUTATION GUARD: restore `let _ = context;` in the default
    /// `forget_with_context` and the first two assertions go red.
    #[tokio::test]
    async fn an_owner_isolated_caller_forgets_nothing_on_a_backend_without_owner_scoping() {
        let memory = DeletableUnscopedBackend::default();
        memory
            .store(UNSCOPED_KEY, UNSCOPED_CONTENT, MemoryCategory::Core, None)
            .await
            .expect("test: store a row on the unscoped backend");
        let isolated = isolated_reader_context(Some("owner:ws-isolation:agent:issuer-b.example:shared-subject"));

        assert!(
            !memory
                .forget_with_context(UNSCOPED_KEY, Some(&isolated))
                .await
                .expect("test: isolated delete by key"),
            "an unscoped backend cannot prove ownership, so an isolated caller deletes nothing"
        );
        assert!(
            memory.get(UNSCOPED_KEY).await.expect("test: keyed read").is_some(),
            "the row must still be there: the refusal has to be a refusal, not a silent delete"
        );

        let mut in_process = isolated;
        in_process.require_owner_match = false;
        assert!(
            memory
                .forget_with_context(UNSCOPED_KEY, Some(&in_process))
                .await
                .expect("test: non-isolated delete by key"),
            "a non-isolated caller keeps the historical unscoped delete"
        );
        assert!(
            memory.get(UNSCOPED_KEY).await.expect("test: keyed read").is_none(),
            "and the row really is gone, so the assertions above are not vacuous"
        );
    }

    /// V02-R9: the write half. A backend that stores no owner (`lucid`,
    /// `markdown`, `none`, wasm wrappers) cannot keep an owner-isolated caller
    /// inside its own keys, so the default `store_with_context` refuses the
    /// write instead of overwriting whatever the key holds.
    ///
    /// MUTATION GUARD: restore `let _ = context;` in the default
    /// `store_with_context` and the first two assertions go red.
    #[tokio::test]
    async fn an_owner_isolated_caller_stores_nothing_on_a_backend_without_owner_scoping() {
        let memory = DeletableUnscopedBackend::default();
        memory
            .store(UNSCOPED_KEY, UNSCOPED_CONTENT, MemoryCategory::Core, None)
            .await
            .expect("test: store a row on the unscoped backend");
        let isolated = isolated_reader_context(Some("owner:ws-isolation:agent:issuer-b.example:shared-subject"));

        let refused = memory
            .store_with_context(UNSCOPED_KEY, "overwrite", MemoryCategory::Core, None, Some(&isolated))
            .await
            .expect_err("test: an isolated write on an unscoped backend must fail");
        assert!(
            refused.to_string().contains("cannot confine a write to an owner"),
            "{refused}"
        );
        assert_eq!(
            memory
                .get(UNSCOPED_KEY)
                .await
                .expect("test: keyed read")
                .map(|entry| entry.content),
            Some(UNSCOPED_CONTENT.to_string()),
            "the refusal has to be a refusal, not a silent overwrite"
        );

        let mut in_process = isolated;
        in_process.require_owner_match = false;
        memory
            .store_with_context(UNSCOPED_KEY, "overwrite", MemoryCategory::Core, None, Some(&in_process))
            .await
            .expect("test: a non-isolated caller keeps the historical write");
        assert!(
            memory
                .list(None, None)
                .await
                .expect("test: list rows")
                .iter()
                .any(|entry| entry.content == "overwrite"),
            "and the write really happened, so the refusal above is not vacuous"
        );
    }

    #[test]
    fn memory_category_display_outputs_expected_values() {
        assert_eq!(MemoryCategory::Core.to_string(), "core");
        assert_eq!(MemoryCategory::Daily.to_string(), "daily");
        assert_eq!(MemoryCategory::Conversation.to_string(), "conversation");
        assert_eq!(
            MemoryCategory::Custom("project_notes".into()).to_string(),
            "project_notes"
        );
    }

    #[test]
    fn memory_category_serde_uses_snake_case() {
        let core = serde_json::to_string(&MemoryCategory::Core).unwrap();
        let daily = serde_json::to_string(&MemoryCategory::Daily).unwrap();
        let conversation = serde_json::to_string(&MemoryCategory::Conversation).unwrap();

        assert_eq!(core, "\"core\"");
        assert_eq!(daily, "\"daily\"");
        assert_eq!(conversation, "\"conversation\"");
    }

    #[test]
    fn memory_write_target_rejects_fitness_even_for_self_system() {
        let error = validate_memory_write_target(
            "self/fitness/daily/2026-09-11",
            Some(crate::self_system::SELF_SYSTEM_SESSION_ID),
        )
        .unwrap_err();

        assert!(error.to_string().contains("use FitnessStore"));
        assert!(
            validate_memory_write_target("self/decisions/item", Some(crate::self_system::SELF_SYSTEM_SESSION_ID))
                .is_ok()
        );
    }

    #[test]
    fn memory_entry_roundtrip_preserves_optional_fields() {
        let entry = MemoryEntry {
            id: "id-1".into(),
            key: "favorite_language".into(),
            content: "Rust".into(),
            category: MemoryCategory::Core,
            timestamp: "2026-02-16T00:00:00Z".into(),
            session_id: Some("session-abc".into()),
            score: Some(0.98),
            tags: Some(vec!["language".into(), "preference".into()]),
            access_count: Some(4),
            useful_count: Some(3),
            source: Some("task-2026-02-16".into()),
            source_confidence: Some(0.92),
            verification_status: Some(VerificationStatus::Verified),
            lifecycle_state: Some(LifecycleState::Active),
            compressed_from: Some(vec!["old-1".into(), "old-2".into()]),
        };

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: MemoryEntry = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id, "id-1");
        assert_eq!(parsed.key, "favorite_language");
        assert_eq!(parsed.content, "Rust");
        assert_eq!(parsed.category, MemoryCategory::Core);
        assert_eq!(parsed.session_id.as_deref(), Some("session-abc"));
        assert_eq!(parsed.score, Some(0.98));
        assert_eq!(parsed.tags.as_ref().map(Vec::len), Some(2));
        assert_eq!(parsed.access_count, Some(4));
        assert_eq!(parsed.useful_count, Some(3));
        assert_eq!(parsed.source.as_deref(), Some("task-2026-02-16"));
        assert_eq!(parsed.source_confidence, Some(0.92));
        assert_eq!(parsed.verification_status, Some(VerificationStatus::Verified));
        assert_eq!(parsed.lifecycle_state, Some(LifecycleState::Active));
        assert_eq!(parsed.compressed_from.as_ref().map(Vec::len), Some(2));
    }

    #[test]
    fn memory_entry_deserialize_legacy_payload_defaults_new_fields() {
        let raw = r#"{
            "id":"id-legacy",
            "key":"legacy_key",
            "content":"legacy content",
            "category":"core",
            "timestamp":"2026-02-01T00:00:00Z",
            "session_id":null,
            "score":0.7
        }"#;
        let parsed: MemoryEntry = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.id, "id-legacy");
        assert!(parsed.tags.is_none());
        assert!(parsed.access_count.is_none());
        assert!(parsed.useful_count.is_none());
        assert!(parsed.source.is_none());
        assert!(parsed.source_confidence.is_none());
        assert!(parsed.verification_status.is_none());
        assert!(parsed.lifecycle_state.is_none());
        assert!(parsed.compressed_from.is_none());
    }
}

/// Source-owned runtime statistics authority. Fields are not caller-constructible.
/// This is not a Memory Retrieve grant and carries no user-memory read permission.
#[derive(Clone)]
pub struct RouterLearningEvidence {
    pub(super) terminal: RouterLearningTerminal,
    pub(super) source_event_id: String,
    pub(super) source_input: String,
    pub(super) source_raw_payload: Option<String>,
    pub(super) generation: i64,
    pub(super) revision: String,
    pub(super) policy: String,
    pub(super) provider: String,
    pub(super) model: String,
    pub(super) success: bool,
    pub(super) latency_ms: u64,
}

#[derive(Clone)]
pub(super) struct RouterLearningTerminal {
    pub id: i64,
    pub content_hash: Option<String>,
    pub event_id: String,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub session_key: Option<String>,
    pub run_id: Option<String>,
    pub channel: Option<String>,
    pub sender: Option<String>,
    pub source: MessageEventSource,
    pub content: String,
    pub raw_payload_json: Option<String>,
    pub idempotency_key: Option<String>,
    pub role: String,
    pub event_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterLearningMetrics {
    pub provider: String,
    pub model: String,
    pub elo: f32,
    pub success_rate: f32,
    pub latency_ms: u32,
    pub successes: std::collections::VecDeque<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterLearningSnapshot {
    pub version: i64,
    pub models: Vec<RouterLearningMetrics>,
}

pub struct RouterLearningRead {
    pub snapshot: Option<RouterLearningSnapshot>,
    pub replayed: bool,
}

pub enum RouterLearningCommit {
    Committed(RouterLearningSnapshot),
    Conflict,
}

impl RouterLearningSnapshot {
    pub(crate) const MAX_BYTES: usize = 262_144;
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version >= 0 && !self.models.is_empty() && self.models.len() <= 128,
            "invalid Router statistics bounds"
        );
        let mut ids = std::collections::HashSet::new();
        for model in &self.models {
            anyhow::ensure!(
                !model.provider.is_empty()
                    && model.provider.len() <= 512
                    && !model.model.is_empty()
                    && model.model.len() <= 512
                    && model.elo.is_finite()
                    && model.success_rate.is_finite()
                    && (0.0..=1.0).contains(&model.success_rate)
                    && model.successes.len() <= 100
                    && ids.insert((&model.provider, &model.model)),
                "invalid Router statistics model"
            );
        }
        Ok(())
    }
    pub(crate) fn encode(&self) -> anyhow::Result<String> {
        self.validate()?;
        let value = serde_json::to_string(self)?;
        anyhow::ensure!(value.len() <= Self::MAX_BYTES, "Router statistics exceed byte bound");
        Ok(value)
    }
    pub(crate) fn decode(value: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(value.len() <= Self::MAX_BYTES, "Router statistics exceed byte bound");
        let snapshot: Self = serde_json::from_str(value)?;
        snapshot.validate()?;
        Ok(snapshot)
    }
}

impl RouterLearningEvidence {
    /// Only the source-bound terminal consumer constructs this. A public receipt
    /// is not a seal: each backend still verifies its durable event and source.
    #[cfg(feature = "llm-router")]
    pub(crate) fn from_terminal(
        receipt: &MessageEvent,
        source_event_id: String,
        source_input: &str,
        source_raw_payload: Option<&str>,
        generation: u64,
        revision: &str,
        policy: String,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            source_raw_payload.is_none_or(|raw| raw.len() <= 32 * 1024),
            "Router source metadata exceeds bound"
        );
        anyhow::ensure!(
            source_input.len() <= 65_536
                && revision.len() <= 512
                && receipt.content_hash.as_ref().is_none_or(|value| value.len() <= 128),
            "Router source/receipt exceeds byte bound"
        );
        // This producer is the native embedded source, not a generic imported terminal.
        // Refuse any additional authority instead of erasing it while projecting the DTO.
        anyhow::ensure!(
            receipt.id > 0
                && receipt.visibility == MemoryVisibility::Session
                && receipt.parent_session_key.is_none()
                && receipt.parent_run_id.is_none()
                && receipt.agent_id.is_none()
                && receipt.persona_id.is_none()
                && receipt.subject.is_none()
                && receipt.goal_id.is_none()
                && receipt.correlation_id.is_none()
                && receipt.attempt_id.is_none()
                && receipt.lease_epoch.is_none()
                && receipt.recipient == receipt.session_key
                && receipt.causation_event_id.as_deref() == Some(source_event_id.as_str())
                && receipt.config_generation_id == Some(generation)
                && receipt.config_source_revision.as_deref() == Some(revision),
            "Router native terminal authority differs"
        );
        let payload = receipt
            .raw_payload_json
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("terminal payload missing"))?;
        anyhow::ensure!(
            payload.len() <= 65_536 && receipt.content.len() <= 8192,
            "Router terminal exceeds byte bound"
        );
        for value in [
            Some(receipt.event_id.as_str()),
            Some(receipt.workspace_id.as_str()),
            receipt.owner_id.as_deref(),
            receipt.session_key.as_deref(),
            receipt.run_id.as_deref(),
            receipt.channel.as_deref(),
            receipt.sender.as_deref(),
            receipt.idempotency_key.as_deref(),
            Some(receipt.source.as_str()),
            Some(receipt.role.as_str()),
            Some(receipt.event_type.as_str()),
        ] {
            anyhow::ensure!(
                value.is_none_or(|value| value.len() <= 512),
                "Router receipt identity exceeds byte bound"
            );
        }
        let terminal = RouterLearningTerminal {
            id: receipt.id,
            content_hash: receipt.content_hash.clone(),
            event_id: receipt.event_id.clone(),
            workspace_id: receipt.workspace_id.clone(),
            owner_id: receipt.owner_id.clone(),
            session_key: receipt.session_key.clone(),
            run_id: receipt.run_id.clone(),
            channel: receipt.channel.clone(),
            sender: receipt.sender.clone(),
            source: receipt.source.clone(),
            content: receipt.content.clone(),
            raw_payload_json: receipt.raw_payload_json.clone(),
            idempotency_key: receipt.idempotency_key.clone(),
            role: receipt.role.clone(),
            event_type: receipt.event_type.clone(),
        };
        let value: serde_json::Value = serde_json::from_str(payload)?;
        let (provider, model, success) = Self::actual_terminal_outcome(&value)?;
        let started = chrono::DateTime::parse_from_rfc3339(
            value
                .get("telemetry")
                .and_then(|telemetry| telemetry.get("started_at"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("terminal start missing"))?,
        )?;
        let finished = chrono::DateTime::parse_from_rfc3339(
            value
                .get("telemetry")
                .and_then(|telemetry| telemetry.get("finished_at"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("terminal finish missing"))?,
        )?;
        let latency_ms = u64::try_from(finished.signed_duration_since(started).num_milliseconds()).unwrap_or(0);
        let evidence = Self {
            terminal,
            source_event_id,
            source_input: source_input.to_owned(),
            source_raw_payload: source_raw_payload.map(str::to_owned),
            generation: i64::try_from(generation)?,
            revision: revision.to_owned(),
            policy,
            provider,
            model,
            success,
            latency_ms,
        };
        evidence.validate()?;
        Ok(evidence)
    }
    #[cfg(feature = "llm-router")]
    pub(crate) fn served(&self) -> (&str, &str) {
        (&self.provider, &self.model)
    }
    #[cfg(feature = "llm-router")]
    pub(crate) const fn outcome(&self) -> (bool, u64) {
        (self.success, self.latency_ms)
    }
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        let event = &self.terminal;
        anyhow::ensure!(
            self.source_input.len() <= 65_536
                && self
                    .source_raw_payload
                    .as_ref()
                    .is_none_or(|raw| raw.len() <= 32 * 1024)
                && self.generation >= 0
                && self.revision.len() <= 512,
            "Router learning source exceeds bounds"
        );
        for id in [
            event.workspace_id.as_str(),
            event.owner_id.as_deref().unwrap_or_default(),
            event.session_key.as_deref().unwrap_or_default(),
            event.run_id.as_deref().unwrap_or_default(),
            event.event_id.as_str(),
            self.source_event_id.as_str(),
            self.provider.as_str(),
            self.model.as_str(),
        ] {
            anyhow::ensure!(!id.is_empty() && id.len() <= 512, "invalid Router learning identity");
        }
        anyhow::ensure!(
            self.policy.len() == 64 && self.policy.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid Router learning policy"
        );
        anyhow::ensure!(
            event.role == "event"
                && event.event_type == "turn.finalized"
                && event.source == MessageEventSource::Agent
                && event.channel.as_deref() == Some("agent_api")
                && event.sender.as_deref() == Some("embedded-caller")
                && event.idempotency_key.as_deref() == Some(format!("turn:{}:final", self.terminal_id()).as_str()),
            "Router learning requires a committed terminal"
        );
        let payload = event
            .raw_payload_json
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("terminal payload missing"))?;
        anyhow::ensure!(
            payload.len() <= 65_536 && event.content.len() <= 8192,
            "Router terminal receipt exceeds byte bound"
        );
        let value: serde_json::Value = serde_json::from_str(payload)?;
        let (provider, model, success) = Self::actual_terminal_outcome(&value)?;
        anyhow::ensure!(
            value.get("terminal_id").and_then(serde_json::Value::as_str) == Some(self.terminal_id())
                && provider == self.provider
                && model == self.model
                && success == self.success,
            "Router learning actual outcome differs from terminal"
        );
        Ok(())
    }
    fn actual_terminal_outcome(value: &serde_json::Value) -> anyhow::Result<(String, String, bool)> {
        use crate::llm::route_decision::{AttemptStatus, ExecutionStatus, ProviderExecutionOutcome};
        // The terminal finalizer authenticates an actual auxiliary event and
        // projects its serving outcome separately from main usage settlement.
        let outcome_value = if let Some(served) = value.get("served_auxiliary") {
            served
                .get("provider_outcome")
                .ok_or_else(|| anyhow::anyhow!("served auxiliary outcome missing"))?
        } else {
            &value["provider_outcome"]
        };
        let outcome: ProviderExecutionOutcome = serde_json::from_value(outcome_value.clone())?;
        let terminal_status: crate::agent::terminal::TurnTerminalStatus =
            serde_json::from_value(value["status"].clone())?;
        anyhow::ensure!(
            matches!(
                outcome.status,
                ExecutionStatus::Success | ExecutionStatus::FallbackSuccess
            ),
            "Router learning requires an actual serving outcome, not all-failed labels"
        );
        let served = outcome
            .attempts
            .iter()
            .rev()
            .find(|attempt| attempt.status != AttemptStatus::Skipped)
            .ok_or_else(|| anyhow::anyhow!("Router learning has no physical attempt"))?;
        anyhow::ensure!(
            served.status == AttemptStatus::Success
                && served.provider == outcome.final_provider
                && served.model == outcome.final_model
                && served.finished_at >= served.started_at
                && outcome.finished_at >= outcome.started_at,
            "Router actual serving attempt differs from terminal outcome"
        );
        let success = matches!(
            terminal_status,
            crate::agent::terminal::TurnTerminalStatus::Completed | crate::agent::terminal::TurnTerminalStatus::Silent
        );
        Ok((outcome.final_provider, outcome.final_model, success))
    }

    pub(crate) fn terminal_id(&self) -> &str {
        self.terminal.run_id.as_deref().unwrap_or_default()
    }
    pub(crate) fn domain(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        for value in [
            &self.terminal.workspace_id,
            self.terminal.owner_id.as_ref().unwrap_or(&self.policy),
            self.terminal.session_key.as_ref().unwrap_or(&self.policy),
            &self.policy,
        ] {
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value.as_bytes());
        }
        hex::encode(hash.finalize())
    }
    pub(crate) fn fingerprint(&self) -> anyhow::Result<String> {
        use sha2::{Digest, Sha256};
        self.validate()?;
        Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
            &self.source_event_id,
            self.domain(),
            &self.terminal.event_id,
            &self.terminal.raw_payload_json,
            &self.source_input,
            &self.source_raw_payload,
            self.generation,
            &self.revision,
            &self.provider,
            &self.model,
            self.success,
            self.latency_ms,
        ))?)))
    }
}

#[cfg(test)]
mod router_served_auxiliary_tests {
    use super::RouterLearningEvidence;
    use crate::llm::route_decision::{
        AttemptStatus, ExecutionStatus, ProviderAttempt, ProviderExecutionOutcome, TokenUsage,
    };

    fn outcome(model: &str) -> ProviderExecutionOutcome {
        let now = chrono::Utc::now();
        ProviderExecutionOutcome {
            decision_id: "prx-routing-observation".to_owned(),
            started_at: now,
            finished_at: now,
            attempts: vec![ProviderAttempt {
                seq: 1,
                provider: "prx-provider".to_owned(),
                model: model.to_owned(),
                started_at: now,
                finished_at: now,
                status: AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "prx-provider".to_owned(),
            final_model: model.to_owned(),
            status: ExecutionStatus::Success,
            fallback_reason: None,
            tokens_used: TokenUsage::default(),
        }
    }

    #[test]
    fn served_auxiliary_selects_real_serving_tuple_without_replacing_main_usage() {
        let main = outcome("cheap");
        let mut marker = serde_json::json!({ "status": "completed", "provider_outcome": main });
        assert_eq!(
            RouterLearningEvidence::actual_terminal_outcome(&marker).unwrap().1,
            "cheap"
        );
        let original = marker.get("provider_outcome").expect("main outcome").clone();
        marker.as_object_mut().expect("terminal object").insert(
            "served_auxiliary".to_owned(),
            serde_json::json!({ "provider_outcome": outcome("premium") }),
        );
        assert_eq!(
            RouterLearningEvidence::actual_terminal_outcome(&marker).unwrap().1,
            "premium"
        );
        assert_eq!(marker.get("provider_outcome").expect("main outcome"), &original);
        *marker
            .pointer_mut("/served_auxiliary/provider_outcome/final_model")
            .expect("premium final model") = serde_json::json!("foreign");
        assert!(RouterLearningEvidence::actual_terminal_outcome(&marker).is_err());
        *marker
            .get_mut("served_auxiliary")
            .expect("present auxiliary projection") = serde_json::json!({});
        assert!(
            RouterLearningEvidence::actual_terminal_outcome(&marker).is_err(),
            "malformed present projection must not fall back to cheap"
        );
    }
}

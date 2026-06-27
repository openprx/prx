//! Chat session persistence — schema-versioned conversation storage.
//!
//! Each `ChatSession` captures a full conversation (turns, metadata, timestamps)
//! and can be serialized/deserialized for persistence via the Memory backend.

use crate::agent::loop_::ChatMode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Current schema version — bump on breaking changes to the session format.
pub const SCHEMA_VERSION: u32 = 1;

/// Memory category key prefix for stored sessions.
pub const SESSION_MEMORY_PREFIX: &str = "chat_session";

/// A single turn in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatTurn {
    /// "user", "assistant", or "system"
    pub role: String,
    /// The message content
    pub content: String,
    /// When this turn was recorded
    pub timestamp: DateTime<Utc>,
    /// Optional summary of tool calls made during this turn
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallSummary>,
}

/// Condensed record of a tool call for session replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallSummary {
    pub name: String,
    pub args_preview: String,
    pub success: bool,
}

/// A complete chat session with versioned schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSession {
    /// Unique session identifier
    pub id: String,
    /// Schema version for migration support
    pub schema_version: u32,
    /// Human-readable title (auto-generated or user-set)
    pub title: String,
    /// Provider used for this session
    pub provider: String,
    /// Model used for this session
    pub model: String,
    /// When the session was created
    pub created_at: DateTime<Utc>,
    /// When the session was last updated
    pub updated_at: DateTime<Utc>,
    /// Ordered conversation turns
    pub turns: Vec<ChatTurn>,
    /// Summaries of child sessions (agent / shell / pty) that ran during
    /// this chat session (v4). Persisted so a reloaded chat session can show
    /// what its background tasks produced. Reload restores **summaries only** —
    /// it never revives a process, sub-agent, or PTY. `#[serde(default)]` keeps
    /// older session blobs (written before v4) loadable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background_sessions: Vec<crate::chat::sessions::PersistedSessionSummary>,
    /// Active interaction mode (plan/edit/auto). Not persisted to JSON so
    /// resumed sessions always start back in the default mode — the user
    /// re-issues `/plan` if they want it.
    #[serde(skip)]
    pub mode: ChatMode,
}

impl ChatSession {
    /// Create a new empty session.
    pub fn new(provider: &str, model: &str) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            schema_version: SCHEMA_VERSION,
            title: String::new(),
            provider: provider.to_string(),
            model: model.to_string(),
            created_at: now,
            updated_at: now,
            turns: Vec::new(),
            background_sessions: Vec::new(),
            mode: ChatMode::default(),
        }
    }

    /// Update the interactive mode for this session.
    pub const fn set_mode(&mut self, mode: ChatMode) {
        self.mode = mode;
    }

    /// Add a user turn.
    pub fn add_user_turn(&mut self, content: &str) {
        self.turns.push(ChatTurn {
            role: "user".to_string(),
            content: content.to_string(),
            timestamp: Utc::now(),
            tool_calls: Vec::new(),
        });
        self.updated_at = Utc::now();

        // Auto-title from first user message if title is empty
        if self.title.is_empty() {
            self.title = truncate_title(content);
        }
    }

    /// Add an assistant turn with optional tool call summaries.
    pub fn add_assistant_turn(&mut self, content: &str, tool_calls: Vec<ToolCallSummary>) {
        self.turns.push(ChatTurn {
            role: "assistant".to_string(),
            content: content.to_string(),
            timestamp: Utc::now(),
            tool_calls,
        });
        self.updated_at = Utc::now();
    }

    /// Number of turns in this session.
    pub const fn turn_count(&self) -> usize {
        self.turns.len()
    }

    /// Upsert a background-session summary (v4), dedup by id. A later record for
    /// the same id replaces the earlier one. Records **summary only** — never
    /// revives a process / sub-agent / PTY. Mirrors the Redux reducer's
    /// `reduce_background_session_recorded` so the legacy (non-TUI) persistence
    /// path stores the same data.
    pub fn record_background_session(&mut self, summary: crate::chat::sessions::PersistedSessionSummary) {
        if let Some(existing) = self.background_sessions.iter_mut().find(|s| s.id == summary.id) {
            *existing = summary;
        } else {
            self.background_sessions.push(summary);
        }
    }

    /// Memory key for storing this session.
    ///
    /// Chat is the one session subsystem whose durable key is **not** derived
    /// through [`crate::runtime::RuntimeEnvelope::canonical_session_key`]: the
    /// whole session is persisted as a single JSON blob under
    /// `chat_session:{id}` (a session-id basis, no channel/sender/recipient
    /// component). This `session-id` form is kept deliberately — the recipient-
    /// aware durable-key migration is deferred to a dedicated wave (see the D7
    /// session-contract notes on `canonical_session_key`). Chat still derives a
    /// consistent canonical identity at the envelope layer for cross-mode recall;
    /// only this blob storage key stays on the legacy format.
    pub fn memory_key(&self) -> String {
        format!("{SESSION_MEMORY_PREFIX}:{}", self.id)
    }

    /// Serialize to JSON for persistence.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Deserialize from JSON.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

/// Truncate content to a title (max 50 chars, break at word boundary).
pub(crate) fn truncate_title(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.chars().count() <= 50 {
        return trimmed.to_string();
    }
    // Find a word boundary near 50 chars
    let mut end = 0;
    for (i, _) in trimmed.char_indices() {
        if i > 50 {
            break;
        }
        end = i;
    }
    // Try to break at a space
    trimmed[..end].rfind(' ').map_or_else(
        || format!("{}...", &trimmed[..end]),
        |space_pos| format!("{}...", &trimmed[..space_pos]),
    )
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_has_correct_defaults() {
        let session = ChatSession::new("openrouter", "anthropic/claude-sonnet-4");
        assert_eq!(session.schema_version, SCHEMA_VERSION);
        assert_eq!(session.provider, "openrouter");
        assert_eq!(session.model, "anthropic/claude-sonnet-4");
        assert!(session.title.is_empty());
        assert!(session.turns.is_empty());
        assert!(!session.id.is_empty());
    }

    #[test]
    fn add_user_turn_auto_titles() {
        let mut session = ChatSession::new("test", "test-model");
        session.add_user_turn("Hello, help me with Rust programming");
        assert_eq!(session.turn_count(), 1);
        assert_eq!(session.title, "Hello, help me with Rust programming");
    }

    #[test]
    fn title_truncation() {
        let long = "This is a very long message that exceeds fifty characters and should be truncated at a word boundary somewhere around here";
        let title = truncate_title(long);
        assert!(title.len() <= 55); // 50 + "..."
        assert!(title.ends_with("..."));
    }

    #[test]
    fn serialization_roundtrip() {
        let mut session = ChatSession::new("provider", "model");
        session.add_user_turn("test input");
        session.add_assistant_turn(
            "test response",
            vec![ToolCallSummary {
                name: "shell".to_string(),
                args_preview: "ls -la".to_string(),
                success: true,
            }],
        );

        let json = session.to_json().unwrap();
        let restored = ChatSession::from_json(&json).unwrap();
        assert_eq!(restored.id, session.id);
        assert_eq!(restored.turn_count(), 2);
        assert_eq!(restored.turns[1].tool_calls.len(), 1);
        assert_eq!(restored.turns[1].tool_calls[0].name, "shell");
    }

    #[test]
    fn memory_key_format() {
        let session = ChatSession::new("p", "m");
        let key = session.memory_key();
        assert!(key.starts_with("chat_session:"));
        assert!(key.len() > "chat_session:".len());
    }

    fn sample_summary(id: &str, status: &str) -> crate::chat::sessions::PersistedSessionSummary {
        crate::chat::sessions::PersistedSessionSummary {
            id: id.to_string(),
            seq: 1,
            kind: "agent".to_string(),
            origin: "user".to_string(),
            status: status.to_string(),
            title: "do the thing".to_string(),
            summary: "all done".to_string(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn background_sessions_round_trip() {
        let mut session = ChatSession::new("p", "m");
        session.record_background_session(sample_summary("run-1", "completed"));
        session.record_background_session(sample_summary("run-2", "failed"));

        let json = session.to_json().expect("test: serialize");
        let restored = ChatSession::from_json(&json).expect("test: deserialize");
        assert_eq!(restored.background_sessions.len(), 2);
        assert_eq!(restored.background_sessions[0].id, "run-1");
        assert_eq!(restored.background_sessions[0].status, "completed");
        assert_eq!(restored.background_sessions[1].status, "failed");
    }

    #[test]
    fn old_format_without_background_sessions_still_loads() {
        // A pre-v4 blob has no `background_sessions` key at all. `#[serde(default)]`
        // must let it deserialize with an empty vec rather than failing.
        let legacy = r#"{
            "id": "abc",
            "schema_version": 1,
            "title": "old",
            "provider": "p",
            "model": "m",
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z",
            "turns": []
        }"#;
        let restored = ChatSession::from_json(legacy).expect("test: legacy blob must load");
        assert_eq!(restored.id, "abc");
        assert!(restored.background_sessions.is_empty());
    }

    #[test]
    fn empty_background_sessions_are_not_serialized() {
        // skip_serializing_if keeps the wire format identical to pre-v4 when no
        // child sessions ran, so nothing changes for the common case.
        let session = ChatSession::new("p", "m");
        let json = session.to_json().expect("test: serialize");
        assert!(!json.contains("background_sessions"));
    }

    #[test]
    fn record_background_session_upserts_by_id() {
        let mut session = ChatSession::new("p", "m");
        session.record_background_session(sample_summary("run-1", "running"));
        // A later record for the same id replaces the earlier one (e.g. a
        // terminal status superseding an interim one). No duplicate is added.
        let mut updated = sample_summary("run-1", "completed");
        updated.summary = "finished cleanly".to_string();
        session.record_background_session(updated);

        assert_eq!(session.background_sessions.len(), 1);
        assert_eq!(session.background_sessions[0].status, "completed");
        assert_eq!(session.background_sessions[0].summary, "finished cleanly");
    }
}

//! Chat session persistence — schema-versioned conversation storage.
//!
//! Each `ChatSession` captures a full conversation (turns, metadata, timestamps)
//! and can be serialized/deserialized for persistence via the Memory backend.

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
        }
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
    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    /// Memory key for storing this session.
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
fn truncate_title(content: &str) -> String {
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
    if let Some(space_pos) = trimmed[..end].rfind(' ') {
        format!("{}...", &trimmed[..space_pos])
    } else {
        format!("{}...", &trimmed[..end])
    }
}

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
}

//! Search of the owner's earlier conversations with the current agent, over the
//! transcript index, and the process-start warm-up of that index.

use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use super::transcript_history_lookup::TranscriptHistoryLookupTool;
use crate::agent::diagnostics::error_class;
use crate::memory::Memory;
use crate::memory::traits::{TRANSCRIPT_MATCH_CLOSE, TRANSCRIPT_MATCH_OPEN, TranscriptHit};
use crate::security::redaction::redact_secrets;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub const TRANSCRIPT_SEARCH_TOOL_NAME: &str = "transcript_search";
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 50;
const NO_MATCH: &str = "No earlier conversation matched.";

/// Set once the warm-up has been started in this process.
static WARM_STARTED: AtomicBool = AtomicBool::new(false);

/// Search the owner's earlier sessions with the current agent.
pub struct TranscriptSearchTool {
    memory: Arc<dyn Memory>,
}

impl TranscriptSearchTool {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self { memory }
    }
}

/// Build the transcript index in the background, once per process, so the first
/// search does not build it. Without a Tokio runtime nothing is started and the
/// first search builds the index as before.
pub(crate) fn warm_index_in_background(memory: Arc<dyn Memory>) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if WARM_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    handle.spawn(async move {
        if let Err(error) = memory.warm_transcript_index().await {
            tracing::warn!(
                error = %error_class(&error),
                "transcript index warm-up failed; the first search builds it"
            );
        }
    });
}

/// The excerpt as shown: the FTS match marks removed, then secrets redacted on
/// the clean text, so no mark can split a secret, then line breaks flattened.
/// The marks are not replaced by brackets, so a highlighted hit shows no
/// highlight here.
fn clean_excerpt(excerpt: &str) -> String {
    let stripped: String = excerpt
        .chars()
        .filter(|ch| *ch != TRANSCRIPT_MATCH_OPEN && *ch != TRANSCRIPT_MATCH_CLOSE)
        .collect();
    redact_secrets(&stripped).replace(['\r', '\n'], " ")
}

/// One line per hit: the time, the session, the role and the clean excerpt.
fn render_hits(hits: &[TranscriptHit]) -> String {
    hits.iter()
        .map(|hit| {
            let excerpt = clean_excerpt(&hit.excerpt);
            format!(
                "{} {} {}: {excerpt}",
                hit.created_at,
                hit.session_key.as_deref().unwrap_or("-"),
                hit.role
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl Tool for TranscriptSearchTool {
    fn name(&self) -> &str {
        TRANSCRIPT_SEARCH_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Search the earlier conversations of this owner with the current agent, in every session. Every word of the query must occur in a message; words of 3 or more characters are matched through the index, shorter words as substrings. Returns the matching messages with their time, session, role and an excerpt."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Words to find in earlier conversations; every word must occur."
                },
                "session_key": {
                    "type": "string",
                    "description": "Optional: search only this session."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_LIMIT,
                    "default": DEFAULT_LIMIT
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let principal = TranscriptHistoryLookupTool::trusted_principal(&args)?;
        let query = args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("query must be a string"))?;
        let session_key = args
            .get("session_key")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|key| !key.is_empty());
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(DEFAULT_LIMIT, |limit| usize::try_from(limit).unwrap_or(MAX_LIMIT))
            .clamp(1, MAX_LIMIT);
        let memory = crate::memory::active_view::select(&self.memory);
        match memory.search_transcripts(&principal, query, session_key, limit).await {
            Ok(hits) if hits.is_empty() => Ok(ToolResult {
                success: true,
                output: NO_MATCH.to_owned(),
                error: None,
            }),
            Ok(hits) => Ok(ToolResult {
                success: true,
                output: render_hits(&hits),
                error: None,
            }),
            Err(error) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("transcript search failed: {}", error_class(&error))),
            }),
        }
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Memory]
    }
}

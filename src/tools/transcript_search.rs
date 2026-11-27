//! Search of the owner's earlier conversations with the current agent, over the
//! transcript index, and the process-start warm-up of that index.

use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use super::transcript_history_lookup::TranscriptHistoryLookupTool;
use crate::agent::diagnostics::error_class;
use crate::memory::Memory;
use crate::memory::traits::TranscriptHit;
use crate::security::redaction::redact_secrets;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub const TRANSCRIPT_SEARCH_TOOL_NAME: &str = "transcript_search";
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 50;
const NO_MATCH: &str = "No earlier conversation matched.";
/// Characters of an excerpt.
const EXCERPT_CHARS: usize = 200;
/// Characters of an excerpt before the anchor's first match.
const EXCERPT_LEAD_CHARS: usize = 80;

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

/// Build the transcript index in the background, once per process: create it
/// if missing and fill it in short batches (resuming an interrupted fill).
/// Searches match on the rows themselves until the index is complete. Without a
/// Tokio runtime nothing is started.
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
                "transcript index warm-up failed; searches match without the index until a later start completes it"
            );
        }
    });
}

/// The excerpt as shown. Secrets are redacted on the whole row text the store
/// returned, before anything is cut, so the cut can never remove the prefix or
/// label a redaction pattern needs. The excerpt is then [`EXCERPT_CHARS`]
/// characters of the redacted text, starting [`EXCERPT_LEAD_CHARS`] before the
/// first case-insensitive occurrence of `anchor`, or at the start when the
/// redacted text does not contain it (for example when the match was inside a
/// redacted secret). Line breaks are flattened to spaces.
fn excerpt(content: &str, anchor: &str) -> String {
    let redacted = redact_secrets(content);
    let start = find_ignoring_case(&redacted, anchor).map_or(0, |at| at.saturating_sub(EXCERPT_LEAD_CHARS));
    redacted
        .chars()
        .skip(start)
        .take(EXCERPT_CHARS)
        .map(|ch| if matches!(ch, '\r' | '\n') { ' ' } else { ch })
        .collect()
}

/// Character index in `haystack` of the first occurrence of `needle`, both
/// compared in lowercase. Linear: the haystack is folded once, with the
/// character index each folded character came from.
fn find_ignoring_case(haystack: &str, needle: &str) -> Option<usize> {
    let needle: String = needle.chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return None;
    }
    let mut folded = String::with_capacity(haystack.len());
    let mut origins = Vec::with_capacity(haystack.len());
    for (index, ch) in haystack.chars().enumerate() {
        origins.push((folded.len(), index));
        folded.extend(ch.to_lowercase());
    }
    let byte = folded.find(&needle)?;
    let after = origins.partition_point(|(offset, _)| *offset <= byte);
    origins.get(after.checked_sub(1)?).map(|(_, index)| *index)
}

/// One line per hit: the time, the session, the role and the excerpt around
/// `anchor`, the first word of the query.
fn render_hits(hits: &[TranscriptHit], anchor: &str) -> String {
    hits.iter()
        .map(|hit| {
            format!(
                "{} {} {}: {}",
                hit.created_at,
                hit.session_key.as_deref().unwrap_or("-"),
                hit.role,
                excerpt(&hit.excerpt, anchor)
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
                output: render_hits(&hits, query.split_whitespace().next().unwrap_or_default()),
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

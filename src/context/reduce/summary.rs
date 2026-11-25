//! Compaction summary support: the bounded transcript the summarizer reads, the
//! patch guard, the fidelity status and the audit source of a compaction.

use crate::agent::context_budget::estimate_history_tokens;
use crate::agent::turn_contracts::CompactionPatch;
use crate::agent::turn_contracts::CompactionPatchError;
use crate::agent::turn_contracts::CompactionPatchGuard;
use crate::memory::CompactionSourceEventRange;
use crate::providers::ChatMessage;
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use sha2::Digest;
use std::fmt::Write;

/// Keep this many most-recent non-system messages after compaction.
pub(crate) const COMPACTION_KEEP_RECENT_MESSAGES: usize = 20;

/// Safety cap for compaction source transcript passed to the summarizer.
pub(crate) const COMPACTION_MAX_SOURCE_CHARS: usize = 12_000;

/// Max characters retained in stored compaction summary.
/// 🟡 Behavior-limits Phase 1: raised 2000 -> 8000 (4x).
pub(crate) const COMPACTION_MAX_SUMMARY_CHARS: usize = 8_000;

/// 🟡 Behavior-limits Phase 1: raised 800 -> 3200 (4x).
pub(crate) const MEMORY_FLUSH_MAX_CHARS: usize = 3_200;

pub(crate) fn build_compaction_transcript(messages: &[ChatMessage]) -> String {
    let mut transcript = String::new();
    for msg in messages {
        let role = msg.role.to_uppercase();
        let _ = writeln!(transcript, "{role}: {}", msg.content.trim());
    }

    if transcript.chars().count() > COMPACTION_MAX_SOURCE_CHARS {
        truncate_with_ellipsis(&transcript, COMPACTION_MAX_SOURCE_CHARS)
    } else {
        transcript
    }
}

pub(crate) fn bounded_compaction_projection(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut projected = Vec::new();
    let mut retained_chars = 0usize;
    for msg in messages {
        if retained_chars >= COMPACTION_MAX_SOURCE_CHARS {
            break;
        }
        let remaining = COMPACTION_MAX_SOURCE_CHARS.saturating_sub(retained_chars);
        let role_chars = msg.role.chars().count();
        let content_budget = remaining.saturating_sub(role_chars.saturating_add(4));
        if content_budget == 0 {
            break;
        }
        let content = if msg.content.chars().count() > content_budget {
            truncate_with_ellipsis(&msg.content, content_budget)
        } else {
            msg.content.clone()
        };
        retained_chars =
            retained_chars.saturating_add(role_chars.saturating_add(content.chars().count()).saturating_add(4));
        projected.push(ChatMessage {
            role: msg.role.clone(),
            content,
        });
    }
    projected
}

#[cfg(test)]
pub(crate) fn apply_compaction_summary(
    history: &mut Vec<ChatMessage>,
    start: usize,
    compact_end: usize,
    summary: &str,
) {
    let summary_msg = ChatMessage::assistant(format!("[Compaction summary]\n{}", summary.trim()));
    history.splice(start..compact_end, std::iter::once(summary_msg));
}

pub(crate) fn hash_compaction_messages(messages: &[ChatMessage]) -> String {
    let mut hasher = sha2::Sha256::new();
    for message in messages {
        hasher.update(message.role.as_bytes());
        hasher.update(b"\0");
        hasher.update(message.content.as_bytes());
        hasher.update(b"\0");
    }
    hex::encode(hasher.finalize())
}

#[must_use]
pub fn compaction_patch_guard_for(
    history: &[ChatMessage],
    range_start: usize,
    range_end: usize,
) -> Option<CompactionPatchGuard> {
    let replaced = history.get(range_start..range_end)?;
    Some(CompactionPatchGuard {
        history_len: history.len(),
        range_start,
        range_end,
        replaced_hash: hash_compaction_messages(replaced),
    })
}

#[must_use]
pub fn compaction_patch_guard_matches(history: &[ChatMessage], guard: &CompactionPatchGuard) -> bool {
    let Some(current) = compaction_patch_guard_for(history, guard.range_start, guard.range_end) else {
        return false;
    };
    current == *guard
}

/// Applies `patch` to `history` only when the patch's guard describes it.
/// The replaced range is the guard's range. Both the bounds and the guard are
/// checked before any mutation, so on error `history` is unchanged.
pub fn apply_compaction_patch_exact(
    history: &mut Vec<ChatMessage>,
    patch: &CompactionPatch,
) -> Result<(), CompactionPatchError> {
    let start = patch.range_start();
    let end = patch.range_end();
    let len = history.len();
    if start > end || end > len {
        return Err(CompactionPatchError::RangeOutOfBounds { start, end, len });
    }
    if !compaction_patch_guard_matches(history, &patch.guard) {
        return Err(CompactionPatchError::GuardMismatch {
            expected_len: patch.guard.history_len,
            actual_len: len,
        });
    }
    let append_index = start.saturating_add(patch.replacement.len());
    history.splice(start..end, patch.replacement.iter().cloned());
    history.splice(append_index..append_index, patch.append_after.iter().cloned());
    Ok(())
}

pub(crate) fn compaction_summary_fidelity_status(
    summary: &str,
    mode: crate::config::AgentCompactionMode,
    source_messages: &[ChatMessage],
) -> &'static str {
    if summary.trim().is_empty() {
        return "rejected_empty";
    }
    if matches!(mode, crate::config::AgentCompactionMode::Safeguard) {
        let required_headers = [
            "## Decisions",
            "## Open TODOs",
            "## Constraints",
            "## Pending",
            "## Exact",
            "## Progress",
            "## Critical Context",
        ];
        let found_count = required_headers
            .iter()
            .filter(|header| summary.contains(*header))
            .count();
        if found_count < 3 {
            return "accepted_retry_missing_headers";
        }
    }
    let identifiers = source_messages
        .iter()
        .flat_map(|message| message.content.split_whitespace())
        .filter(|word| {
            word.contains('/')
                || word.contains("://")
                || word.contains(':')
                || word.chars().filter(|ch| *ch == '-').count() >= 2
        })
        .take(8);
    let missing_identifiers = identifiers
        .filter(|identifier| !summary.contains(identifier.trim_matches(|ch: char| ch.is_ascii_punctuation())))
        .count();
    if missing_identifiers > 0 {
        "accepted_identifier_loss"
    } else {
        "accepted"
    }
}

pub(crate) fn extract_document_ingest_refs(source_messages: &[ChatMessage]) -> Vec<serde_json::Value> {
    let mut refs = Vec::new();
    for (index, message) in source_messages.iter().enumerate() {
        let mut remaining = message.content.as_str();
        while let Some(start) = remaining.find("[document_ingest_ref]") {
            let after_start = &remaining[start + "[document_ingest_ref]".len()..];
            let Some(end) = after_start.find("[/document_ingest_ref]") else {
                break;
            };
            let block = &after_start[..end];
            let mut item = serde_json::Map::new();
            item.insert("index".to_string(), serde_json::json!(index));
            item.insert("role".to_string(), serde_json::json!(message.role));
            for line in block.lines() {
                let Some((key, value)) = line.split_once(':') else {
                    continue;
                };
                let key = key.trim();
                let value = value.trim();
                if key.is_empty() || value.is_empty() {
                    continue;
                }
                let parsed_value = match key {
                    "bytes" | "inline_preview_bytes" | "chunks" => value
                        .parse::<u64>()
                        .map_or_else(|_| serde_json::json!(value), serde_json::Value::from),
                    _ => serde_json::json!(value),
                };
                item.insert(key.to_string(), parsed_value);
            }
            if item.contains_key("document_id") {
                refs.push(serde_json::Value::Object(item));
            }
            remaining = &after_start[end + "[/document_ingest_ref]".len()..];
        }
    }
    refs
}

#[derive(Debug)]
pub(crate) struct CompactionAuditSource {
    pub(crate) message_count: usize,
    pub(crate) token_estimate: usize,
    pub(crate) source_event_ids: Vec<String>,
    pub(crate) source_event_range: Option<CompactionSourceEventRange>,
    pub(crate) source_event_provenance_status: String,
    /// Cold-window messages covered by an assistant turn rather than by an
    /// event id of their own (tool round-trip envelopes and results).
    pub(crate) unpersisted_companion_count: usize,
    pub(crate) document_refs: Vec<serde_json::Value>,
}

impl Default for CompactionAuditSource {
    fn default() -> Self {
        Self {
            message_count: 0,
            token_estimate: 0,
            source_event_ids: Vec::new(),
            source_event_range: None,
            source_event_provenance_status: "unavailable".to_string(),
            unpersisted_companion_count: 0,
            document_refs: Vec::new(),
        }
    }
}

pub(crate) fn build_compaction_audit_source(source_messages: &[ChatMessage]) -> CompactionAuditSource {
    CompactionAuditSource {
        message_count: source_messages.len(),
        token_estimate: estimate_history_tokens(source_messages),
        source_event_ids: Vec::new(),
        source_event_range: None,
        source_event_provenance_status: "unavailable".to_string(),
        unpersisted_companion_count: 0,
        document_refs: extract_document_ingest_refs(source_messages),
    }
}

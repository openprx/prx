//! Compaction summary support: the patch guard, the patch application and the
//! audit source of a compaction.

use crate::agent::context_budget::estimate_history_tokens;
use crate::agent::turn_contracts::CompactionPatch;
use crate::agent::turn_contracts::CompactionPatchError;
use crate::agent::turn_contracts::CompactionPatchGuard;
use crate::memory::CompactionSourceEventRange;
use crate::providers::ChatMessage;
use anyhow::Result;
use sha2::Digest;

/// Keep this many most-recent non-system messages after compaction.
pub(crate) const COMPACTION_KEEP_RECENT_MESSAGES: usize = 20;

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
    }
}

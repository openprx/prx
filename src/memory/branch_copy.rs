//! Transactional copy of a chat session's transcript rows into a new branch.
//!
//! A branch starts a new session key, and transcript lookup authorizes a read
//! by `session_key` equality. A branch that keeps a context handoff therefore
//! needs its own copy of every row the handoff chain references, with each
//! handoff note rewritten to point at the copies. The copy is planned by the
//! caller (oldest handoff first) and applied by the backend in one transaction.

use std::collections::HashMap;

use crate::memory::traits::{MessageEvent, MessageEventInput, MessageEventSubject};
use crate::tools::transcript_history_lookup::{MAX_REFERENCED_EVENTS, MAX_ROW_SPAN};

/// Upper bound on the rows one branch copy may insert. The copy is a
/// user-initiated foreground operation; this bounds its single transaction.
pub(crate) const BRANCH_COPY_MAX_ROWS: usize = 4096;

/// Event type of a persisted context handoff row.
pub(crate) const CONTEXT_SWITCH_EVENT_TYPE: &str = "context.switch.created";

/// Idempotency-key prefix of every row a branch copy inserted.
pub(crate) const BRANCH_COPY_IDEMPOTENCY_PREFIX: &str = "branch-copy:";

/// Whether `event` is a row a branch copy inserted (a copy of a row of the
/// session the branch was made from).
#[must_use]
pub(crate) fn is_branch_copy(event: &MessageEvent) -> bool {
    event
        .idempotency_key
        .as_deref()
        .is_some_and(|key| key.starts_with(BRANCH_COPY_IDEMPOTENCY_PREFIX))
}

/// The canonical and legacy session keys of one chat session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCopySessionKeys {
    pub session_key: String,
    pub legacy_session_key: String,
}

impl BranchCopySessionKeys {
    fn contains(&self, key: &str) -> bool {
        key == self.session_key || key == self.legacy_session_key
    }
}

/// One ordered batch of original rows. A handoff batch holds the rows its
/// handoff references; the final batch holds the newest handoff and the
/// retained turns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCopyBatch {
    pub original_event_ids: Vec<String>,
}

/// What to copy, in insertion order: one batch per handoff (oldest first), then
/// the final batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCopyPlan {
    pub branch_id: String,
    pub workspace_id: String,
    pub source: BranchCopySessionKeys,
    pub target: BranchCopySessionKeys,
    pub handoff_batches: Vec<BranchCopyBatch>,
    pub final_batch: BranchCopyBatch,
}

impl BranchCopyPlan {
    /// Total number of rows the copy inserts.
    #[must_use]
    pub fn total_rows(&self) -> usize {
        self.handoff_batches
            .iter()
            .chain(std::iter::once(&self.final_batch))
            .map(|batch| batch.original_event_ids.len())
            .sum()
    }

    /// Bounds checked before any transaction starts.
    pub fn check_bounds(&self) -> Result<(), BranchCopyError> {
        let rows = self.total_rows();
        if rows > BRANCH_COPY_MAX_ROWS {
            return Err(BranchCopyError::TooLarge {
                rows,
                limit: BRANCH_COPY_MAX_ROWS,
            });
        }
        for batch in &self.handoff_batches {
            let rows = batch.original_event_ids.len();
            if rows == 0 {
                return Err(BranchCopyError::InvalidPlan("empty handoff batch"));
            }
            if rows > MAX_REFERENCED_EVENTS {
                return Err(BranchCopyError::BatchTooLarge {
                    rows,
                    limit: MAX_REFERENCED_EVENTS,
                });
            }
        }
        let mut seen = std::collections::HashSet::with_capacity(rows);
        let all_unique = self
            .handoff_batches
            .iter()
            .chain(std::iter::once(&self.final_batch))
            .flat_map(|batch| batch.original_event_ids.iter())
            .all(|id| seen.insert(id.as_str()));
        if !all_unique {
            return Err(BranchCopyError::InvalidPlan("an original row appears twice"));
        }
        Ok(())
    }

    /// The idempotency key of the copy of `original_event_id`.
    #[must_use]
    pub fn idempotency_key(&self, original_event_id: &str) -> String {
        format!("{BRANCH_COPY_IDEMPOTENCY_PREFIX}{}:{original_event_id}", self.branch_id)
    }
}

/// The copy of the newest handoff, whose content replaces the branch's
/// handoff turn so the branch's runtime note points at the copies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCopiedHandoff {
    pub original_event_id: String,
    pub event_id: String,
    pub content: String,
}

/// Result of a committed branch copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCopyResult {
    pub copied_rows: usize,
    pub newest_handoff: Option<BranchCopiedHandoff>,
}

/// Typed failures of a branch copy. Every failure leaves the store unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BranchCopyError {
    #[error("branch transcript copy is unsupported by this memory backend")]
    Unsupported,
    #[error("branch transcript copy needs {rows} rows, more than the limit of {limit}")]
    TooLarge { rows: usize, limit: usize },
    #[error("a handoff batch of {rows} rows exceeds the lookup limit of {limit}")]
    BatchTooLarge { rows: usize, limit: usize },
    #[error("invalid branch copy plan: {0}")]
    InvalidPlan(&'static str),
    #[error("transcript row {event_id} is missing from the source session")]
    SourceRowMissing { event_id: String },
    #[error("context handoff note cannot be rewritten: {0}")]
    HandoffNote(&'static str),
    #[error("copied rows are not contiguous")]
    NotContiguous,
    #[error("transcript row {event_id} was already copied into this branch")]
    AlreadyCopied { event_id: String },
}

/// A copied row: its new event id and row id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CopiedRow {
    pub(crate) event_id: String,
    pub(crate) row_id: i64,
}

/// Map a source-session key onto the branch; other keys stay as they are.
fn retarget_key(plan: &BranchCopyPlan, key: &str) -> String {
    if key == plan.source.session_key {
        plan.target.session_key.clone()
    } else if key == plan.source.legacy_session_key {
        plan.target.legacy_session_key.clone()
    } else {
        key.to_string()
    }
}

/// Whether `event` belongs to the plan's source session.
pub(crate) fn is_source_row(plan: &BranchCopyPlan, event: &MessageEvent) -> bool {
    event.workspace_id == plan.workspace_id
        && event
            .session_key
            .as_deref()
            .is_some_and(|key| plan.source.contains(key))
}

/// The insert input for the copy of `original`. A handoff row is rewritten to
/// reference the copies already recorded in `copies`.
pub(crate) fn branch_copy_input(
    plan: &BranchCopyPlan,
    original: &MessageEvent,
    copies: &HashMap<String, CopiedRow>,
    new_event_id: &str,
) -> Result<MessageEventInput, BranchCopyError> {
    let (content, raw_payload_json) = if original.event_type == CONTEXT_SWITCH_EVENT_TYPE {
        rewrite_handoff_row(original, copies, new_event_id)?
    } else {
        (original.content.clone(), original.raw_payload_json.clone())
    };
    let mapped_id = |id: &Option<String>| {
        id.as_ref()
            .map(|id| copies.get(id).map_or_else(|| id.clone(), |copy| copy.event_id.clone()))
    };
    let subject = original.subject.as_ref().map(|subject| match subject {
        MessageEventSubject::Conversation(key) => MessageEventSubject::Conversation(retarget_key(plan, key)),
        other => other.clone(),
    });
    Ok(MessageEventInput {
        event_id: Some(new_event_id.to_string()),
        idempotency_key: Some(plan.idempotency_key(&original.event_id)),
        workspace_id: original.workspace_id.clone(),
        owner_id: original.owner_id.clone(),
        source: original.source.clone(),
        channel: original.channel.clone(),
        session_key: original.session_key.as_deref().map(|key| retarget_key(plan, key)),
        parent_session_key: original
            .parent_session_key
            .as_deref()
            .map(|key| retarget_key(plan, key)),
        run_id: original.run_id.clone(),
        parent_run_id: original.parent_run_id.clone(),
        agent_id: original.agent_id.clone(),
        persona_id: original.persona_id.clone(),
        sender: original.sender.clone(),
        recipient: original.recipient.clone(),
        role: original.role.clone(),
        event_type: original.event_type.clone(),
        subject,
        goal_id: original.goal_id.clone(),
        causation_event_id: mapped_id(&original.causation_event_id),
        correlation_id: mapped_id(&original.correlation_id),
        attempt_id: original.attempt_id.clone(),
        lease_epoch: original.lease_epoch,
        config_generation_id: original.config_generation_id,
        config_source_revision: original.config_source_revision.clone(),
        content,
        raw_payload_json,
        visibility: original.visibility.clone(),
        agent_partition: None,
    })
}

/// Rewrite a handoff row's note (content and `raw_payload_json.handoff_note`)
/// to reference the copies of its rows, its own copy id and its parent's copy.
fn rewrite_handoff_row(
    original: &MessageEvent,
    copies: &HashMap<String, CopiedRow>,
    new_event_id: &str,
) -> Result<(String, Option<String>), BranchCopyError> {
    let mut note = crate::agent::compaction::parse_context_handoff_note(&original.content)
        .ok_or(BranchCopyError::HandoffNote("the row carries no handoff note"))?;
    rewrite_handoff_note(&mut note, copies, new_event_id)?;
    let note_json =
        serde_json::to_string(&note).map_err(|_| BranchCopyError::HandoffNote("the rewritten note does not encode"))?;
    let content = crate::agent::compaction::format_context_handoff_message(&note_json);
    let raw_payload_json = match original.raw_payload_json.as_deref() {
        Some(raw) => {
            let mut payload = serde_json::from_str::<serde_json::Value>(raw)
                .map_err(|_| BranchCopyError::HandoffNote("the row payload is not JSON"))?;
            let object = payload
                .as_object_mut()
                .ok_or(BranchCopyError::HandoffNote("the row payload is not an object"))?;
            object.insert("handoff_note".to_string(), note);
            Some(
                serde_json::to_string(&payload)
                    .map_err(|_| BranchCopyError::HandoffNote("the rewritten payload does not encode"))?,
            )
        }
        None => None,
    };
    Ok((content, raw_payload_json))
}

/// Rewrite the identity and provenance fields of one handoff note. Every
/// referenced row and the parent handoff must already be copied.
pub(crate) fn rewrite_handoff_note(
    note: &mut serde_json::Value,
    copies: &HashMap<String, CopiedRow>,
    new_event_id: &str,
) -> Result<(), BranchCopyError> {
    let referenced = note
        .pointer("/lookup/arguments/event_ids")
        .and_then(serde_json::Value::as_array)
        .ok_or(BranchCopyError::HandoffNote("the note has no lookup event ids"))?
        .iter()
        .map(|id| {
            id.as_str()
                .and_then(|id| copies.get(id))
                .ok_or(BranchCopyError::HandoffNote("a referenced row was not copied first"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let (Some(first), Some(last)) = (
        referenced.iter().min_by_key(|copy| copy.row_id),
        referenced.iter().max_by_key(|copy| copy.row_id),
    ) else {
        return Err(BranchCopyError::HandoffNote("the note references no rows"));
    };
    if last.row_id.saturating_sub(first.row_id) > MAX_ROW_SPAN {
        return Err(BranchCopyError::HandoffNote(
            "the copied rows exceed the lookup row span",
        ));
    }
    let parent = match note.get("parent_handoff_event_id") {
        Some(serde_json::Value::String(parent)) => Some(
            copies
                .get(parent)
                .map(|copy| copy.event_id.clone())
                .ok_or(BranchCopyError::HandoffNote("the parent handoff was not copied first"))?,
        ),
        Some(serde_json::Value::Null) | None => None,
        Some(_) => return Err(BranchCopyError::HandoffNote("the parent handoff id is not a string")),
    };
    let event_ids = referenced
        .iter()
        .map(|copy| serde_json::Value::String(copy.event_id.clone()))
        .collect::<Vec<_>>();
    let (first_event_id, last_event_id, first_row_id, last_row_id) =
        (first.event_id.clone(), last.event_id.clone(), first.row_id, last.row_id);
    let object = note
        .as_object_mut()
        .ok_or(BranchCopyError::HandoffNote("the note is not an object"))?;
    object.insert(
        "handoff_event_id".to_string(),
        serde_json::Value::String(new_event_id.to_string()),
    );
    object.insert(
        "parent_handoff_event_id".to_string(),
        parent.map_or(serde_json::Value::Null, serde_json::Value::String),
    );
    let range = object
        .get_mut("source_event_range")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or(BranchCopyError::HandoffNote("the note has no source event range"))?;
    range.insert("first_event_id".to_string(), first_event_id.into());
    range.insert("last_event_id".to_string(), last_event_id.into());
    range.insert("first_row_id".to_string(), first_row_id.into());
    range.insert("last_row_id".to_string(), last_row_id.into());
    let arguments = object
        .get_mut("lookup")
        .and_then(|lookup| lookup.get_mut("arguments"))
        .and_then(serde_json::Value::as_object_mut)
        .ok_or(BranchCopyError::HandoffNote("the note has no lookup arguments"))?;
    arguments.insert("event_ids".to_string(), serde_json::Value::Array(event_ids));
    arguments.insert("first_row_id".to_string(), first_row_id.into());
    arguments.insert("last_row_id".to_string(), last_row_id.into());
    Ok(())
}

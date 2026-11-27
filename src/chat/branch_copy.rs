//! Branch transcript provenance (R3d).
//!
//! A branch gets a new session key, and transcript lookup authorizes reads by
//! session key equality. Before the branch session is saved, its handoff chain
//! and retained turns are copied into the branch in one transaction, and the
//! branch's handoff turn is replaced by the copy that references the copies.

use std::collections::HashSet;

use anyhow::Result;

use super::chat_events::chat_runtime_envelope;
use super::session;
use crate::agent::compaction::{parse_context_handoff_note, resolve_compaction_event_provenance};
use crate::agent::turn_contracts::stored_turn_message;
use crate::context::reduce::provenance::resolve_recorded_rows;
use crate::memory::Memory;
use crate::memory::branch_copy::{
    BRANCH_COPY_MAX_ROWS, BranchCopyBatch, BranchCopyError, BranchCopyPlan, BranchCopySessionKeys,
    CONTEXT_SWITCH_EVENT_TYPE,
};

/// Copy `branch`'s transcript provenance from the session `source_session_id`
/// and point its handoff turn at the copies.
///
/// A branch that keeps a context handoff copies the whole chain or fails, and
/// the caller aborts the branch. A branch without a handoff references no
/// transcript rows; its retained-turn copy only gives later compaction exact
/// provenance, so a backend without branch copy or an unresolvable turn leaves
/// it without copies instead of refusing `/branch`.
pub(super) async fn copy_branch_transcript(
    mem: &dyn Memory,
    workspace_id: &str,
    source_session_id: &str,
    branch: &mut session::ChatSession,
) -> Result<()> {
    // The copied turns are new rows of the branch: the source's event ids do
    // not name them, so none is kept after the source rows are resolved.
    let recorded_ids = branch
        .turns
        .iter()
        .map(|turn| turn.event_id.iter().cloned().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    for turn in &mut branch.turns {
        turn.event_id = None;
    }
    if branch.turns.is_empty() {
        return Ok(());
    }
    let handoff_turns = branch
        .turns
        .iter()
        .enumerate()
        .filter(|(_, turn)| is_handoff_turn(turn))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let handoff_index = match handoff_turns.as_slice() {
        [] => None,
        [index] => Some(*index),
        _ => return Err(BranchCopyError::InvalidPlan("the branch keeps more than one handoff turn").into()),
    };
    let source = chat_runtime_envelope(workspace_id, &format!("chat:{source_session_id}"));
    let target = chat_runtime_envelope(workspace_id, &format!("chat:{}", branch.id));
    let mut plan = BranchCopyPlan {
        branch_id: branch.id.clone(),
        workspace_id: workspace_id.to_string(),
        source: BranchCopySessionKeys {
            session_key: source.session_key.clone(),
            legacy_session_key: format!("chat:{source_session_id}"),
        },
        target: BranchCopySessionKeys {
            session_key: target.session_key.clone(),
            legacy_session_key: format!("chat:{}", branch.id),
        },
        handoff_batches: Vec::new(),
        final_batch: BranchCopyBatch {
            original_event_ids: Vec::new(),
        },
    };

    let Some(handoff_index) = handoff_index else {
        match retained_turn_event_ids(mem, &source, branch, None, &recorded_ids).await {
            Ok(ids) => plan.final_batch.original_event_ids = ids,
            Err(error) => {
                let (error_len, error_sha256) = crate::agent::diagnostics::tool_parse_log_digest(&error.to_string());
                tracing::warn!(
                    turns = branch.turns.len(),
                    error_len,
                    error_sha256,
                    "branch keeps no handoff and its turns have no exact transcript rows; later compaction in the branch starts without provenance"
                );
                return Ok(());
            }
        }
        return match mem.copy_events_for_branch(plan).await {
            Ok(_) => Ok(()),
            Err(error) if error.downcast_ref::<BranchCopyError>() == Some(&BranchCopyError::Unsupported) => {
                // Nothing in a branch without a handoff references transcript
                // rows; a backend without an event log simply has none to copy.
                tracing::debug!("memory backend has no branch copy; branch keeps no transcript rows");
                Ok(())
            }
            Err(error) => Err(error),
        };
    };

    let handoff_turn = branch
        .turns
        .get(handoff_index)
        .ok_or(BranchCopyError::InvalidPlan("the handoff turn is out of range"))?;
    let newest_note = parse_context_handoff_note(&handoff_turn.content)
        .ok_or(BranchCopyError::HandoffNote("the branch handoff turn carries no note"))?;
    let newest_handoff_id = note_str(&newest_note, "handoff_event_id")
        .ok_or(BranchCopyError::HandoffNote("the branch handoff has no event id"))?
        .to_string();
    plan.handoff_batches = handoff_chain_batches(mem, &source, &plan, newest_note).await?;
    plan.final_batch.original_event_ids.push(newest_handoff_id.clone());
    match retained_turn_event_ids(mem, &source, branch, Some(handoff_index), &recorded_ids).await {
        Ok(retained) => plan.final_batch.original_event_ids.extend(retained),
        Err(error) => {
            let (error_len, error_sha256) = crate::agent::diagnostics::tool_parse_log_digest(&error.to_string());
            tracing::warn!(
                turns = branch.turns.len(),
                error_len,
                error_sha256,
                "branch retained turns have no exact transcript rows; the branch keeps its handoff chain and later compaction lacks provenance for them"
            );
        }
    }
    plan.check_bounds()?;

    let result = mem.copy_events_for_branch(plan).await?;
    let copied = result
        .newest_handoff
        .filter(|copied| copied.original_event_id == newest_handoff_id)
        .ok_or(BranchCopyError::HandoffNote("the newest handoff was not copied"))?;
    let turn = branch
        .turns
        .get_mut(handoff_index)
        .ok_or(BranchCopyError::InvalidPlan("the handoff turn is out of range"))?;
    turn.content = copied.content;
    Ok(())
}

fn is_handoff_turn(turn: &session::ChatTurn) -> bool {
    crate::agent::compaction::is_context_handoff_turn(&turn.role, &turn.content)
}

fn note_str<'a>(note: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    note.get(field).and_then(serde_json::Value::as_str)
}

fn note_event_ids(note: &serde_json::Value) -> Result<Vec<String>, BranchCopyError> {
    note.pointer("/lookup/arguments/event_ids")
        .and_then(serde_json::Value::as_array)
        .ok_or(BranchCopyError::HandoffNote("the note has no lookup event ids"))?
        .iter()
        .map(|id| {
            id.as_str()
                .map(str::to_string)
                .ok_or(BranchCopyError::HandoffNote("a lookup event id is not a string"))
        })
        .collect()
}

/// One batch per handoff, oldest first: the rows each handoff references,
/// which include its parent handoff row.
async fn handoff_chain_batches(
    mem: &dyn Memory,
    source: &crate::runtime::envelope::RuntimeEnvelope,
    plan: &BranchCopyPlan,
    newest_note: serde_json::Value,
) -> Result<Vec<BranchCopyBatch>> {
    let owner_id = source.resolved_owner_id();
    let mut batches = Vec::new();
    let mut visited = HashSet::new();
    let mut rows = 0usize;
    let mut note = newest_note;
    loop {
        let ids = note_event_ids(&note)?;
        rows = rows.saturating_add(ids.len());
        if rows > BRANCH_COPY_MAX_ROWS {
            return Err(BranchCopyError::TooLarge {
                rows,
                limit: BRANCH_COPY_MAX_ROWS,
            }
            .into());
        }
        let parent = note_str(&note, "parent_handoff_event_id").map(str::to_string);
        batches.push(BranchCopyBatch {
            original_event_ids: ids,
        });
        let Some(parent) = parent else {
            break;
        };
        let referenced = batches
            .last()
            .is_some_and(|batch| batch.original_event_ids.contains(&parent));
        if !referenced || !visited.insert(parent.clone()) {
            return Err(BranchCopyError::HandoffNote("a handoff does not reference its parent exactly once").into());
        }
        let parent_row = mem
            .find_owned_message_event(&plan.workspace_id, &owner_id, &parent)
            .await?
            .filter(|event| {
                event.event_type == CONTEXT_SWITCH_EVENT_TYPE
                    && event
                        .session_key
                        .as_deref()
                        .is_some_and(|key| key == plan.source.session_key || key == plan.source.legacy_session_key)
            })
            .ok_or_else(|| BranchCopyError::SourceRowMissing {
                event_id: parent.clone(),
            })?;
        note = parse_context_handoff_note(&parent_row.content)
            .ok_or(BranchCopyError::HandoffNote("the parent handoff row carries no note"))?;
    }
    batches.reverse();
    Ok(batches)
}

/// The original `message.created` rows of every retained turn (all turns but
/// the handoff), in turn order.
///
/// Turns that recorded one event id each are verified against those rows, with
/// the handoff first when there is one; otherwise the rows are found by text
/// over the same sequence.
async fn retained_turn_event_ids(
    mem: &dyn Memory,
    source: &crate::runtime::envelope::RuntimeEnvelope,
    branch: &session::ChatSession,
    handoff_index: Option<usize>,
    recorded_ids: &[Vec<String>],
) -> Result<Vec<String>> {
    let mut order = Vec::with_capacity(branch.turns.len());
    order.extend(handoff_index);
    order.extend((0..branch.turns.len()).filter(|index| Some(*index) != handoff_index));
    let mut joint = Vec::with_capacity(order.len());
    let mut recorded = Vec::with_capacity(order.len());
    for index in order {
        let turn = branch
            .turns
            .get(index)
            .ok_or(BranchCopyError::InvalidPlan("a retained turn is out of range"))?;
        joint.push(stored_turn_message(turn.role.clone(), turn.content.clone()));
        recorded.push(recorded_ids.get(index).cloned().unwrap_or_default());
    }
    let skip = usize::from(handoff_index.is_some());
    if joint.len() == skip {
        return Ok(Vec::new());
    }
    let pinned = resolve_recorded_rows(mem, source.memory_principal(), &joint, &recorded).await?;
    let rows = match pinned.filter(|rows| rows.len() == joint.len()) {
        Some(rows) => rows.into_iter().map(|(_, event_id)| event_id).collect::<Vec<_>>(),
        None => {
            resolve_compaction_event_provenance(
                mem,
                source.memory_principal(),
                &joint,
                crate::agent::compaction::RoundCompanions::Acknowledged,
            )
            .await?
            .filter(|provenance| {
                provenance.companion_message_count == 0 && provenance.source_event_ids.len() == joint.len()
            })
            .ok_or(BranchCopyError::InvalidPlan(
                "the retained turns have no exact transcript rows",
            ))?
            .source_event_ids
        }
    };
    Ok(rows.into_iter().skip(skip).collect())
}

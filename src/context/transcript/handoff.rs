//! The newest valid context handoff of one session, read from its log rows. A
//! model history starts at that handoff, so the turns it moved out of the
//! window are not loaded again.

use crate::agent::compaction::{ValidatedContextHandoff, validated_context_handoff};
use crate::memory::branch_copy::CONTEXT_SWITCH_EVENT_TYPE;
use crate::memory::erasure::Unsupported;
use crate::memory::{ConversationTurn, Memory, MemoryPrincipal, MessageEvent};
use std::collections::HashSet;

/// Handoff rows read per lookup, newest first. A handoff that does not validate
/// is passed over; the first valid one of these rows is the session's newest.
const HANDOFF_CANDIDATE_ROWS: usize = 8;

/// The handoff rows of `session_key` that pass the handoff validator, newest
/// first, among the newest [`HANDOFF_CANDIDATE_ROWS`] switch rows, each with its
/// typed values. The first is the session's newest handoff; the rest let a
/// resume follow `parent_handoff_event_id`. Empty when none is valid, and also
/// when the backend has no ordered event log (its fail-fast read is the normal
/// no-handoff case). Any other read error is returned, not read as no handoff.
pub(super) async fn valid_handoffs(
    memory: &dyn Memory,
    principal: &MemoryPrincipal,
    session_key: &str,
) -> anyhow::Result<Vec<(MessageEvent, ValidatedContextHandoff)>> {
    let rows = match memory
        .newest_session_events_of_type(
            principal,
            session_key,
            CONTEXT_SWITCH_EVENT_TYPE,
            HANDOFF_CANDIDATE_ROWS,
        )
        .await
    {
        Ok(rows) => rows,
        Err(error) if error.is::<Unsupported>() => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    Ok(rows
        .into_iter()
        .filter(|event| event.session_key.as_deref() == Some(session_key))
        .filter_map(|event| validated_context_handoff(&event.content).map(|handoff| (event, handoff)))
        .collect())
}

/// The highest row id a resume treats as covered by the newest handoff of
/// `chain` (as [`valid_handoffs`] returns it). A chained switch's `last_row_id`
/// is the parent switch row, which lies above messages the parent kept, so the
/// boundary is the newest loaded `message.created` turn the newest note lists.
/// When it lists none that is loaded (it folded only its parent handoff), the
/// walk goes up through `parent_handoff_event_id` among the rows read and takes
/// the first ancestor that lists a loaded turn. Only when the chain ends
/// without one does the newest note's `last_row_id` stand.
pub(super) fn covered_through(turns: &[ConversationTurn], chain: &[(MessageEvent, ValidatedContextHandoff)]) -> i64 {
    let Some((_, newest)) = chain.first() else {
        return 0;
    };
    let mut current = chain.first();
    for _ in 0..chain.len() {
        let Some((_, handoff)) = current else {
            break;
        };
        let listed: HashSet<&str> = handoff.event_ids.iter().map(String::as_str).collect();
        let found = turns
            .iter()
            .filter(|turn| turn.message_event_id.as_deref().is_some_and(|id| listed.contains(id)))
            .map(|turn| turn.id)
            .max();
        if let Some(id) = found {
            return id;
        }
        current = handoff
            .parent_handoff_event_id
            .as_deref()
            .and_then(|parent| chain.iter().find(|(event, _)| event.event_id == parent));
    }
    newest.last_row_id
}

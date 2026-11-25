//! The newest valid context handoff of one session, read from its log rows. A
//! model history starts at that handoff, so the turns it moved out of the
//! window are not loaded again.

use crate::agent::compaction::{ValidatedContextHandoff, validated_context_handoff};
use crate::memory::branch_copy::CONTEXT_SWITCH_EVENT_TYPE;
use crate::memory::erasure::Unsupported;
use crate::memory::{Memory, MemoryPrincipal, MessageEvent};

/// Handoff rows read per lookup, newest first. A handoff that does not validate
/// is passed over; the first valid one of these rows is the session's newest.
const HANDOFF_CANDIDATE_ROWS: usize = 8;

/// The handoff row of `session_key` with the highest row id whose note passes
/// the handoff validator, and its typed values. `Ok(None)` when none of the
/// newest [`HANDOFF_CANDIDATE_ROWS`] rows is one, and also when the backend has
/// no ordered event log (its fail-fast read is the normal no-handoff case). Any
/// other read error is returned, not read as no handoff.
pub(super) async fn newest_valid_handoff(
    memory: &dyn Memory,
    principal: &MemoryPrincipal,
    session_key: &str,
) -> anyhow::Result<Option<(MessageEvent, ValidatedContextHandoff)>> {
    let rows = match memory
        .newest_session_events_of_type(principal, session_key, CONTEXT_SWITCH_EVENT_TYPE, HANDOFF_CANDIDATE_ROWS)
        .await
    {
        Ok(rows) => rows,
        Err(error) if error.is::<Unsupported>() => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(rows
        .into_iter()
        .filter(|event| event.session_key.as_deref() == Some(session_key))
        .find_map(|event| validated_context_handoff(&event.content).map(|handoff| (event, handoff))))
}

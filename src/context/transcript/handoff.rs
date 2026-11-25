//! The newest valid context handoff of one session, read from its log rows. A
//! model history starts at that handoff, so the turns it moved out of the
//! window are not loaded again.

use crate::agent::compaction::{POOL_MAX_ROWS, POOL_PAGE_ROWS, ValidatedContextHandoff, validated_context_handoff};
use crate::memory::{Memory, MemoryPrincipal, MessageEvent, SessionContextQuery};

/// Event type of the row a context switch stores its handoff note in.
const CONTEXT_SWITCH_EVENT_TYPE: &str = "context.switch.created";

/// The handoff row of `session_key` with the highest row id whose note passes
/// the handoff validator, and its typed values. Rows are read newest first in
/// pages of [`POOL_PAGE_ROWS`], at most [`POOL_MAX_ROWS`] rows; `None` when no
/// valid handoff lies within that bound. A read error is returned, not read as
/// no handoff, so the caller decides what it means.
pub(super) async fn newest_valid_handoff(
    memory: &dyn Memory,
    principal: &MemoryPrincipal,
    session_key: &str,
) -> anyhow::Result<Option<(MessageEvent, ValidatedContextHandoff)>> {
    // The session read is hard-filtered by the principal's own session key.
    let mut principal = principal.clone();
    principal.session_key = Some(session_key.to_string());
    let mut before_id = i64::MAX;
    let mut rows_read = 0usize;
    while rows_read < POOL_MAX_ROWS {
        let query = SessionContextQuery {
            principal: principal.clone(),
            since_event_id: None,
            limit: POOL_PAGE_ROWS,
            include_roles: vec!["assistant".to_string()],
        };
        let page = memory.load_session_context_before(query, before_id).await?;
        let page_len = page.len();
        rows_read = rows_read.saturating_add(page_len);
        let oldest = page.iter().map(|event| event.id).min();
        // Pages run newest first, so the first page holding a valid handoff
        // holds the newest one; within the page the highest row id wins.
        let newest = page
            .into_iter()
            .filter(|event| {
                event.event_type == CONTEXT_SWITCH_EVENT_TYPE && event.session_key.as_deref() == Some(session_key)
            })
            .filter_map(|event| validated_context_handoff(&event.content).map(|handoff| (event, handoff)))
            .max_by_key(|(event, _)| event.id);
        if newest.is_some() {
            return Ok(newest);
        }
        match oldest {
            Some(oldest) if page_len >= POOL_PAGE_ROWS => before_id = oldest,
            _ => return Ok(None),
        }
    }
    Ok(None)
}

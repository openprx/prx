//! Root-owned approval surfaces, without retaining the ended parent's authority.
use crate::{
    gateway::AppState,
    memory::child_sessions::ChildSessionRecord,
    messaging::runtime::{MessageInput, MessagingRuntime},
};
use anyhow::{Context, Result};
use std::sync::Arc;
pub(super) async fn live(
    state: &AppState,
    record: &ChildSessionRecord,
    input: &MessageInput,
    console: bool,
    wait_secs: u64,
) -> Result<crate::agent::live_frames::LiveTurnRef> {
    let state_owned = state.clone();
    let record = record.clone();
    let input = input.clone();
    let scope = crate::runtime::blocking::spawn_blocking(move || {
        let event = input.receipt.adopted_message_event_id.context("adopted child event missing")?.to_string();
        if let Some(store) = state_owned.mem.sync_store()
            && let Some(scope) = store.live_scope(&record.owner, &event)?
        {
            // rf requires a connected surface. A generic gateway root has none.
            return Ok(if state_owned.rf_streams.connected(&record.owner) { Some((scope, false)) } else { None });
        }
        if !console { return Ok(None); }
        // A registered Console root has its authenticated durable approval inbox
        // and decision endpoints, independently of an rf WebSocket connection.
        let config = state_owned.config.load_full();
        let runtime = MessagingRuntime::for_config(&config)?;
        let agent = runtime.store.logical_agent(&record.workspace, &record.owner, &record.parent_session)?.unwrap_or_else(|| "main".into());
        let address = runtime.address(&record.owner, &record.parent_session, &agent);
        let root = runtime.store.session_scope(&record.workspace, &address, false)?;
        Ok::<_, anyhow::Error>(Some((serde_json::json!({"session":record.parent_session,"agent":agent,"incarnation":root.incarnation,"run_id":record.parent_run}), true)))
    }).await??;
    Ok(
        scope.map_or_else(crate::runtime::mailbox_child::no_surface, |(scope, console)| {
            crate::agent::live_frames::LiveTurnRef(Arc::new(crate::gateway::run_approvals::RunApprovalResolver {
                scope,
                streams: Arc::clone(&state.rf_streams),
                approvals: Arc::clone(&state.rf_approvals),
                cancellation: tokio_util::sync::CancellationToken::new(),
                wait_secs: if console { 240 } else { wait_secs },
            }))
        }),
    )
}

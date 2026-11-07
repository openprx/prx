//! Journal-backed repair of retained child terminals. It reads only the
//! authority journal and never consults the decision store.
use super::authority_journal;
use super::completion::{ChildOutcomeHook, JournalChildOutcomeHook};
use crate::agent::terminal::{
    VerifiedTerminalReplay, read_journal_replay_record_on, verify_terminal_replay_journal_until,
};
use crate::causal_tree::store::ChildRecoveryLineage;
use crate::decision::DecisionKey;
use crate::memory::ingress_recovery::{IngressEventLookup, IngressRecoveryScope, remaining};
use crate::memory::{Memory, MessageEvent};
use crate::runtime::turn_cleanup::CleanupTracker;
use anyhow::Context;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

/// Replays the discovered child's terminal from journal evidence alone. A
/// missing terminal is `None`; discovery lineage must match the authenticated
/// terminal and the enumerated decision row.
pub(crate) async fn verify_candidate(
    memory: &dyn Memory,
    workspace: &Path,
    key: &Arc<DecisionKey>,
    store_workspace: &str,
    scope: &IngressRecoveryScope,
    lineage: &ChildRecoveryLineage,
    owner: &Arc<CleanupTracker>,
    deadline: Instant,
) -> anyhow::Result<Option<(MessageEvent, VerifiedTerminalReplay)>> {
    remaining(deadline)?;
    let Some(event) = memory
        .read_ingress_event(
            scope,
            &IngressEventLookup::OwnedIdempotencyKey(format!("turn:{}:final", lineage.run_id)),
            deadline,
        )
        .await?
    else {
        return Ok(None);
    };
    let replay = verify_terminal_replay_journal_until(
        memory,
        workspace,
        key,
        store_workspace,
        &scope.workspace_id,
        &scope.owner_id,
        &lineage.run_id,
        &event,
        deadline,
        owner,
    )
    .await?;
    anyhow::ensure!(
        replay.source().event_id == lineage.source_event_id
            && replay.source().run_id.as_deref() == Some(lineage.run_id.as_str())
            && replay.source().session_key.as_deref() == Some(lineage.session_key.as_str()),
        "child discovery lineage differs from authenticated terminal"
    );
    let row = read_journal_replay_record_on(
        Some(Arc::clone(owner)),
        workspace,
        key,
        store_workspace,
        &scope.owner_id,
        &lineage.decision_id,
    )
    .await?
    .context("child discovery decision disappeared")?;
    anyhow::ensure!(
        row.source_event_id.as_deref() == Some(lineage.source_event_id.as_str())
            && row.decision.run_id.as_deref() == Some(lineage.run_id.as_str())
            && row.decision.session_key.as_deref() == Some(lineage.session_key.as_str()),
        "child discovery decision lineage changed"
    );
    Ok(Some((event, replay)))
}

/// Settles the verified child in the journal. `true` means this call wrote the
/// row; `false` means an identical row was already there. A conflicting or
/// tampered row is an error, never overwritten.
pub(crate) async fn settle(
    workspace: &Path,
    key: &Arc<DecisionKey>,
    store_workspace: &Arc<str>,
    replay: &VerifiedTerminalReplay,
    event: &MessageEvent,
    owner: &Arc<CleanupTracker>,
    deadline: Instant,
) -> anyhow::Result<bool> {
    let hook = JournalChildOutcomeHook::new(workspace.to_path_buf(), Arc::clone(key), Arc::clone(store_workspace));
    let expected = hook.expected_row(replay, event)?;
    let (read_workspace, read_key) = (workspace.to_path_buf(), Arc::clone(key));
    let present = crate::runtime::ingress_recovery::owned_store_read(owner, move || {
        Ok(authority_journal::verify_child_outcome(
            &read_workspace,
            &read_key,
            &expected,
            None,
        )?)
    })
    .await?;
    if present {
        return Ok(false);
    }
    hook.persist(replay, event, Some(owner), Some(deadline)).await?;
    Ok(true)
}

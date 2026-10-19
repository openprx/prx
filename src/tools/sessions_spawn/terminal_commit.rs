use super::{
    KILLED_BY_USER_REASON, ProcessFinalization, ProcessRunControl, SubAgentRun, SubAgentStatus, process_terminal,
};
use anyhow::Context as _;
use chrono::Utc;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
pub(super) struct ProcessTerminalCommitHook {
    pub(super) entered: tokio::sync::oneshot::Sender<()>,
    pub(super) release: tokio::sync::oneshot::Receiver<()>,
}

pub(super) async fn commit_process_terminal_state(
    active_runs: &Arc<RwLock<Vec<SubAgentRun>>>,
    run_id: &str,
    status: SubAgentStatus,
    token_usage_records: Vec<crate::llm::route_decision::MeteredTokenUsageRecord>,
    process_control: &ProcessRunControl,
    finalization: ProcessFinalization,
    repair: Option<&process_terminal::ConfirmedProcessRepair>,
    #[cfg(test)] commit_hook: Option<ProcessTerminalCommitHook>,
) -> anyhow::Result<()> {
    let mut runs = active_runs.write().await;
    let run = runs
        .iter_mut()
        .find(|run| run.id == run_id)
        .context("process terminal registry row missing")?;
    let prior_finalization = process_control.finalization();
    if let Some(repair) = repair {
        anyhow::ensure!(
            finalization == ProcessFinalization::Natural
                && repair.matches(run_id, run.owner_id.as_deref(), &status, &token_usage_records),
            "process repair proposal differs from verified completion"
        );
    }
    let repairing = repair.is_some()
        && matches!(
            prior_finalization,
            Some(ProcessFinalization::Unconfirmed | ProcessFinalization::TerminationFailed)
        );
    if let Some(previous) = prior_finalization.filter(|_| !repairing) {
        let same_status = match (&run.status, &status) {
            (SubAgentStatus::Completed(left), SubAgentStatus::Completed(right))
            | (SubAgentStatus::Failed(left), SubAgentStatus::Failed(right)) => left == right,
            _ => false,
        };
        anyhow::ensure!(
            previous == finalization && same_status,
            "process terminal conflicts with confirmed finalization"
        );
    }
    // Validate the entire merge before changing status or releasing the slot.
    let mut known = std::collections::BTreeMap::new();
    for usage in &run.token_usage_records {
        anyhow::ensure!(
            repair.is_none() || usage.settlement_id.is_some(),
            "verified process repair cannot reconcile unidentified prior usage"
        );
        if let Some(id) = usage.settlement_id.as_deref() {
            anyhow::ensure!(!id.is_empty(), "process settlement identity is empty");
            if let Some(previous) = known.insert(id, usage) {
                anyhow::ensure!(previous == usage, "process registry contains conflicting settlements");
            }
        }
    }
    let mut additions = Vec::new();
    for usage in &token_usage_records {
        if let Some(id) = usage.settlement_id.as_deref() {
            anyhow::ensure!(!id.is_empty(), "process settlement identity is empty");
            if let Some(previous) = known.get(id) {
                anyhow::ensure!(*previous == usage, "process settlement conflicts with prior usage");
                continue;
            }
            known.insert(id, usage);
        } else {
            anyhow::ensure!(
                prior_finalization.is_none(),
                "legacy process usage cannot be replayed without settlement identity"
            );
        }
        additions.push(usage.clone());
    }
    run.finished_at.get_or_insert_with(Utc::now);
    run.status = status;
    run.token_usage_records.extend(additions);
    run.steer_tx = None;

    #[cfg(test)]
    if let Some(hook) = commit_hook {
        let _ = hook.entered.send(());
        let _ = hook.release.await;
    }

    // Registry readers remain excluded by the write lock until both terminal
    // status and finalization are committed, so no caller can observe an early
    // slot release or terminal status with an unfinalized process control.
    if repairing {
        // The external observer has joined the sole monitor and still holds
        // the registry lock; the verified proposal and usage merge agree.
        process_control.finalized_tx.send_replace(Some(finalization));
    } else {
        process_control.finalize(finalization);
    }
    Ok(())
}

pub(super) async fn commit_process_owner_failure_if_unfinalized(
    active_runs: &Arc<RwLock<Vec<SubAgentRun>>>,
    run_id: &str,
    process_control: &ProcessRunControl,
    error: String,
) {
    if process_control.finalization().is_some() {
        return;
    }
    if let Err(commit_error) = commit_process_terminal_state(
        active_runs,
        run_id,
        SubAgentStatus::Failed(error),
        Vec::new(),
        process_control,
        ProcessFinalization::TerminationFailed,
        None,
        #[cfg(test)]
        None,
    )
    .await
    {
        tracing::error!(run_id, error = %commit_error, "process failure terminal remains unconfirmed");
    }
}

/// Make a process-mode run answer a kill aimed at its **own** registry row.
///
/// A task-mode run publishes an abort handle on its row, so
/// `registry::kill(row, cascade = false)` ends it. A process-mode run
/// deliberately publishes none: this monitor is the sole owner allowed to
/// signal and reap the OS child, and aborting it would strand the very process
/// the kill is meant to end. The consequence was that the row had *no*
/// termination mechanism at all — `prx tasks kill <batch-id> --no-cascade`
/// answered `not_killable` for every process-mode member, and only the default
/// cascade reached them, by signalling the worker's process group through the
/// child row [`run_sub_agent_process`] registers underneath.
///
/// The row now carries a cancellation token, and this bridge turns cancelling
/// it into exactly the termination the tool-plane `kill` action asks for: the
/// owner is told to stop, terminates and reaps its child, and commits
/// [`KILLED_BY_USER_REASON`]. No second owner, no new signalling path, no
/// clock — the registry simply reaches the mechanism that was already there.
///
/// The bridge ends with the run whichever way the run ends: it waits on the
/// token and on the control's finalization together, so a run that finishes on
/// its own does not leave a task behind.
///
/// MUTATION GUARD: drop the `token.cancelled()` arm and
/// `a_process_mode_member_answers_a_kill_of_its_own_row` reports `Requested`
/// instead of `Killed` and never sees the owner asked to stop.
fn bridge_registry_kill_to_process_control(control: Arc<ProcessRunControl>, token: CancellationToken) {
    tokio::spawn(async move {
        tokio::select! {
            () = token.cancelled() => {
                let outcome = control.request_termination(KILLED_BY_USER_REASON).await;
                tracing::info!(
                    ?outcome,
                    "work registry kill reached a process-mode sub-agent run; its owner was asked to terminate"
                );
            }
            () = control.finalized() => {}
        }
    });
}

/// Publish the registry row for a process-mode run, already wired to the only
/// mechanism that can end it.
///
/// One entry point rather than two statements at the call site, because the two
/// halves are not independently correct: a row published without the token is a
/// run the work registry cannot end at all (that was the bug), and a token
/// bridged onto a control whose row nobody registered is a kill nothing can
/// address. Keeping them together is also what lets a test exercise the wiring
/// production uses instead of a hand-built copy of it.
///
/// MUTATION GUARD: pass `None` for the token here and
/// `a_process_mode_member_answers_a_kill_of_its_own_row` reports
/// `NotKillable`.
pub(super) fn register_killable_process_run(
    label: &str,
    run_id: &str,
    parent: Option<crate::runtime::registry::WorkId>,
    batch_id: Option<&str>,
    control: &Arc<ProcessRunControl>,
) -> crate::runtime::registry::WorkGuard {
    let token = CancellationToken::new();
    let guard = crate::runtime::registry::register_sub_agent(label, run_id, parent, batch_id, Some(token.clone()));
    bridge_registry_kill_to_process_control(Arc::clone(control), token);
    guard
}

/// Publish a terminal status for a run whose owning task vanished without
/// publishing one itself.
///
/// This is the discoverability half of the runtime's only automatic
/// termination. The idle (no-progress) detector ends a wedged turn by killing
/// everything the turn owns (`crate::agent::idle::kill_turn_subtree` ->
/// `crate::runtime::registry::kill`), and for a sub-agent that means *aborting
/// the monitor task* through the abort handle published on its registry row.
/// An aborted task runs no more code, so the row it was going to update is
/// left as it was: the registry's hang ledger records the termination while
/// `sessions_list` keeps reporting `running`, forever. Nothing polls it back
/// into line, so a join on that run would never return.
///
/// Only a non-terminal status is overwritten, so a terminal state committed by
/// the monitor microseconds earlier always wins.
pub(super) async fn commit_run_termination_if_unfinished(
    active_runs: &Arc<RwLock<Vec<SubAgentRun>>>,
    run_id: &str,
    reason: &str,
) -> bool {
    let mut runs = active_runs.write().await;
    let Some(run) = runs.iter_mut().find(|run| run.id == run_id) else {
        return false;
    };
    if matches!(run.status, SubAgentStatus::Completed(_) | SubAgentStatus::Failed(_)) {
        return false;
    }
    run.finished_at = Some(Utc::now());
    run.status = SubAgentStatus::Failed(reason.to_string());
    run.steer_tx = None;
    true
}

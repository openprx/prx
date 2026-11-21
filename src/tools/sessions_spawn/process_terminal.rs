//! Verification and recovery of worker completion against logical source storage.
pub(super) mod operator;
pub(super) mod ordinary;
mod process_identity;
pub(super) mod retained;
use crate::agent::terminal::TurnTerminalStatus;
use crate::memory::{MemoryFabric, MessageEvent, MessageEventSource, MessageEventSubject};
use crate::session_worker::protocol::WorkerResult;
use anyhow::Context;
use sha2::{Digest, Sha256};

/// Bound on the launches remembered as already warned about in this sweep.
const MAX_WARNED_LAUNCHES: usize = 4096;
/// Set once the process has reported that the suppression set is saturated.
static SUPPRESSION_SATURATION_REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What the sweep does about one unconfirmed launch.
#[derive(Debug, PartialEq, Eq)]
enum UnconfirmedWarning {
    /// First report of this launch: emit its warning.
    Warn,
    /// Already reported and remembered: stay silent.
    Suppressed,
    /// The set is full, so this launch cannot be remembered and gets no
    /// per-launch warning. `aggregate` is true only for the first such launch
    /// in the process, which emits the saturation warning.
    Unremembered { aggregate: bool },
}

/// Decide the warning for `launch_event_id`. Remembered launches are never
/// forgotten: a full set stays full rather than being cleared.
fn admit_unconfirmed_warning(
    warned: &mut std::collections::HashSet<i64>,
    launch_event_id: i64,
    saturation_reported: &std::sync::atomic::AtomicBool,
) -> UnconfirmedWarning {
    if warned.contains(&launch_event_id) {
        return UnconfirmedWarning::Suppressed;
    }
    if warned.len() < MAX_WARNED_LAUNCHES {
        warned.insert(launch_event_id);
        return UnconfirmedWarning::Warn;
    }
    UnconfirmedWarning::Unremembered {
        aggregate: !saturation_reported.swap(true, std::sync::atomic::Ordering::Relaxed),
    }
}

pub(super) async fn run_restart_recovery(
    fabric: MemoryFabric,
    workspace: std::path::PathBuf,
    worker_root: std::path::PathBuf,
    runs: std::sync::Arc<tokio::sync::RwLock<Vec<super::SubAgentRun>>>,
) {
    // Once per process start: settle snapshot folders whose owner is gone.
    retained::sweep_retained_worker_folders(&fabric, &workspace, &worker_root, &runs).await;
    // An unconfirmed reconciliation is reported once per launch per process,
    // not again on every sweep; the set is bounded and never cleared.
    let mut warned = std::collections::HashSet::new();
    let mut cursor = 0;
    loop {
        let delay = match recover_restart_page(&fabric, &workspace, cursor, 128).await {
            Ok(page) => {
                let exhausted = page.outcomes.len() < 128;
                for outcome in page.outcomes {
                    match outcome.result {
                        Ok(Some(recovered)) => {
                            if recovered.restore(&runs).await.is_err() {
                                match admit_unconfirmed_warning(
                                    &mut warned,
                                    outcome.launch_event_id,
                                    &SUPPRESSION_SATURATION_REPORTED,
                                ) {
                                    UnconfirmedWarning::Warn => tracing::warn!(
                                        launch_event_id = outcome.launch_event_id,
                                        "worker restart registry reconciliation remains unconfirmed"
                                    ),
                                    UnconfirmedWarning::Suppressed
                                    | UnconfirmedWarning::Unremembered { aggregate: false } => {}
                                    UnconfirmedWarning::Unremembered { aggregate: true } => tracing::warn!(
                                        limit = MAX_WARNED_LAUNCHES,
                                        "worker restart warning suppression set is saturated; further unconfirmed launches are not reported individually"
                                    ),
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(_) => tracing::warn!(
                            launch_event_id = outcome.launch_event_id,
                            "worker restart evidence remains unconfirmed"
                        ),
                    }
                }
                // A fresh sweep revisits live/unknown/invalid entries without
                // accumulating an unbounded in-memory retry queue.
                cursor = if exhausted { 0 } else { page.next_cursor };
                if exhausted { 30 } else { 1 }
            }
            Err(_) => {
                tracing::warn!("worker restart discovery unavailable; retaining recovery cursor");
                30
            }
        };
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
    }
}

pub(super) struct RecoveredProcessRun {
    pub(super) source: MessageEvent,
    pub(super) batch_id: Option<String>,
    pub(super) outcome: RecoveredProcessOutcome,
    /// The root partition sealed in the run's launch record (MA-01b5).
    pub(super) agent_partition: Option<crate::memory::AgentPartition>,
}

pub(super) enum RecoveredProcessOutcome {
    OperatorComplete(operator::VerifiedOperatorWorkerCompletion),
    OperatorUnconfirmed(RecoveredWorkerUsage),
    OrdinaryComplete(ordinary::VerifiedOrdinaryWorkerCompletion),
    OrdinaryUnconfirmed(RecoveredWorkerUsage),
}

impl RecoveredProcessRun {
    /// Only rows created by this recovery path may later be upgraded. Live
    /// monitor rows keep their sole owner, including on unconfirmed outcomes.
    pub(super) async fn restore(
        self,
        active_runs: &std::sync::Arc<tokio::sync::RwLock<Vec<super::SubAgentRun>>>,
    ) -> anyhow::Result<bool> {
        use super::{HistoryEntry, ProcessRunControl, SubAgentRun, SubAgentStatus};
        let source = self.source;
        let delegation = super::SpawnDelegationStatus::from_stored_payload(source.raw_payload_json.as_deref());
        let run_id = source.run_id.clone().context("recovered worker run missing")?;
        let started_at = chrono::DateTime::parse_from_rfc3339(&source.created_at)?.with_timezone(&chrono::Utc);
        let session = source.session_key.context("recovered worker session missing")?;
        let (status, finished_at, usage, history, process_control) = match self.outcome {
            RecoveredProcessOutcome::OrdinaryComplete(completion) => {
                let finished_at = completion
                    .finished_at
                    .context("ordinary recovered terminal timestamp missing")?;
                let status = if completion.result.success {
                    SubAgentStatus::Completed(completion.result.output.clone())
                } else {
                    SubAgentStatus::Failed(super::sub_agent_reported_failure(
                        completion
                            .result
                            .error
                            .as_deref()
                            .context("ordinary recovered failure missing")?,
                    ))
                };
                let history = if completion.result.success {
                    vec![HistoryEntry {
                        role: "assistant".to_owned(),
                        content: completion.result.output,
                        timestamp: finished_at,
                    }]
                } else {
                    Vec::new()
                };
                (status, Some(finished_at), completion.usage, history, None)
            }
            RecoveredProcessOutcome::OrdinaryUnconfirmed(recovered) => (
                SubAgentStatus::Failed(format!(
                    "{}: ordinary completion journal unavailable; cleanup and provider usage remain unconfirmed; {} visible segments lack a terminal",
                    super::EXITED_WITHOUT_RESULT_PREFIX,
                    recovered.unfinished_segments
                )),
                None,
                recovered.records,
                Vec::new(),
                Some(ProcessRunControl::recovered_unconfirmed()),
            ),
            RecoveredProcessOutcome::OperatorComplete(completion) => {
                let finished_at = completion
                    .finished_at
                    .context("operator recovered terminal timestamp missing")?;
                let status = if completion.result.success {
                    SubAgentStatus::Completed(completion.result.output.clone())
                } else {
                    SubAgentStatus::Failed(super::sub_agent_reported_failure(
                        completion
                            .result
                            .error
                            .as_deref()
                            .context("operator recovered failure missing")?,
                    ))
                };
                let history = if completion.result.success {
                    vec![HistoryEntry {
                        role: "assistant".to_owned(),
                        content: completion.result.output,
                        timestamp: finished_at,
                    }]
                } else {
                    Vec::new()
                };
                (status, Some(finished_at), completion.usage, history, None)
            }
            RecoveredProcessOutcome::OperatorUnconfirmed(recovered) => (
                SubAgentStatus::Failed(format!(
                    "{}: operator completion journal unavailable; cleanup and provider usage remain unconfirmed; {} visible segments lack a terminal",
                    super::EXITED_WITHOUT_RESULT_PREFIX,
                    recovered.unfinished_segments
                )),
                None,
                recovered.records,
                Vec::new(),
                Some(ProcessRunControl::recovered_unconfirmed()),
            ),
        };
        let mut runs = active_runs.write().await;
        let existing_index = runs.iter().position(|run| run.id == run_id);
        if let Some(index) = existing_index {
            let existing = runs.get(index).context("recovered worker registry index disappeared")?;
            let recovery_only = existing
                .process_control
                .as_ref()
                .is_some_and(|control| control.recovery_only);
            // A live row of the same owner is the sole owner of the run: its
            // source is the parent turn's event while recovery reads the
            // child input event, so the identities legitimately differ.
            if !recovery_only && existing.owner_id == source.owner_id {
                return Ok(false);
            }
            // Only an upgrade of a recovery-only projection must match the
            // recovered source and ownership exactly.
            anyhow::ensure!(
                recovery_only
                    && existing.owner_id == source.owner_id
                    && existing.source_message_event_id.as_deref() == Some(source.event_id.as_str()),
                "recovered worker conflicts with registry ownership"
            );
            // A later sweep cannot discard or change an already authenticated
            // settlement, including while upgrading to a completed journal.
            anyhow::ensure!(
                existing.token_usage_records.iter().all(|known| usage.contains(known)),
                "worker recovery would lose or change a confirmed settlement"
            );
        }
        let run = SubAgentRun {
            agent_partition: self.agent_partition,
            id: run_id,
            task: if source.role == "user" {
                source.content
            } else {
                "[task input was not recorded]".to_owned()
            },
            owner_id: source.owner_id,
            topic_id: match source.subject {
                Some(MessageEventSubject::Topic(topic)) => Some(topic),
                _ => None,
            },
            source_message_event_id: Some(source.event_id),
            started_at,
            finished_at,
            status,
            recipient: None,
            channel_name: None,
            abort_handle: None,
            process_control,
            task_control: None,
            history: std::sync::Arc::new(tokio::sync::RwLock::new(history)),
            steer_tx: None,
            parent_run_id: source.parent_run_id,
            session_scope_key: session,
            // Read-only projections cannot spawn descendants or resume.
            spawn_depth: 0,
            delegation,
            token_usage_records: usage,
            progress: crate::agent::idle::child_beat(),
            batch_id: self.batch_id,
        };
        if let Some(index) = existing_index {
            *runs
                .get_mut(index)
                .context("recovered worker registry index disappeared")? = run;
        } else {
            runs.push(run);
        }
        Ok(true)
    }
}

/// A cursor advances over discovery records even if an individual record is
/// invalid. Each outcome remains separate so one corrupt launch cannot hide
/// later work. Callers must retain unresolved outcomes for the next sweep.
pub(super) struct RestartRecoveryPage {
    pub(super) next_cursor: i64,
    pub(super) outcomes: Vec<RestartRecoveryOutcome>,
}

pub(super) struct RestartRecoveryOutcome {
    pub(super) launch_event_id: i64,
    pub(super) result: anyhow::Result<Option<RecoveredProcessRun>>,
}

pub(super) async fn recover_restart_page(
    fabric: &MemoryFabric,
    workspace: &std::path::Path,
    after_id: i64,
    limit: usize,
) -> anyhow::Result<RestartRecoveryPage> {
    anyhow::ensure!(
        fabric.workspace_id() == workspace.to_string_lossy() && after_id >= 0 && (1..=128).contains(&limit),
        "restart recovery requires exact workspace, nonnegative cursor and limit 1..=128"
    );
    let events = fabric
        .memory()
        .list_worker_launch_events(fabric.workspace_id(), after_id, limit)
        .await?;
    anyhow::ensure!(events.len() <= limit, "worker discovery exceeded requested page");
    let mut page = RestartRecoveryPage {
        next_cursor: after_id,
        outcomes: Vec::with_capacity(events.len()),
    };
    for event in events {
        anyhow::ensure!(event.id > page.next_cursor, "worker discovery cursor did not advance");
        page.next_cursor = event.id;
        page.outcomes.push(RestartRecoveryOutcome {
            launch_event_id: event.id,
            result: recover_restart_event(fabric, workspace, event).await,
        });
        tokio::task::yield_now().await;
    }
    Ok(page)
}

async fn recover_restart_event(
    fabric: &MemoryFabric,
    workspace: &std::path::Path,
    event: MessageEvent,
) -> anyhow::Result<Option<RecoveredProcessRun>> {
    anyhow::ensure!(
        event.workspace_id == fabric.workspace_id(),
        "worker discovery crossed workspace"
    );
    if event.event_type == "worker.operator.recovery.registered" {
        return operator::recover_restart_event(fabric, workspace, event).await;
    }
    if event.event_type == "worker.ordinary.recovery.registered" {
        return ordinary::recover_restart_event(fabric, workspace, event).await;
    }
    // Delegated (decision-issued) launch records are no longer recovered
    // (P2-G4); only ordinary and operator launches carry recovery authority.
    Ok(None)
}

async fn read_terminal(
    fabric: &MemoryFabric,
    owner: &str,
    event_id: &str,
    digest: &str,
    run_id: &str,
) -> anyhow::Result<MessageEvent> {
    read_terminal_with_source(
        fabric,
        owner,
        event_id,
        digest,
        run_id,
        &MessageEventSource::SessionWorker,
    )
    .await
}

async fn read_terminal_with_source(
    fabric: &MemoryFabric,
    owner: &str,
    event_id: &str,
    digest: &str,
    run_id: &str,
    expected_source: &MessageEventSource,
) -> anyhow::Result<MessageEvent> {
    let event = fabric
        .memory()
        .find_owned_message_event(fabric.workspace_id(), owner, event_id)
        .await?
        .context("worker terminal is not durable")?;
    anyhow::ensure!(
        event.id > 0
            && event.event_id == event_id
            && event.workspace_id == fabric.workspace_id()
            && event.owner_id.as_deref() == Some(owner)
            && event.run_id.as_deref() == Some(run_id)
            && &event.source == expected_source
            && event.role == "event"
            && event.event_type == "turn.finalized"
            && event.idempotency_key.as_deref() == Some(format!("turn:{run_id}:final").as_str())
            && format!("{:x}", Sha256::digest(serde_json::to_vec(&event)?)) == digest,
        "worker terminal differs from signed completion"
    );
    Ok(event)
}

/// Confirmed usage only. Recovery cannot prove that interrupted provider calls
/// were unbilled, even when every visible segment has a terminal.
pub(super) struct RecoveredWorkerUsage {
    pub(super) records: Vec<crate::llm::route_decision::MeteredTokenUsageRecord>,
    pub(super) unfinished_segments: u64,
}

/// Repair authority joins a verified result with owned exit acknowledgement or
/// signed launch absence and a cleanup journal. It cannot be built from a PID.
pub(super) struct ConfirmedProcessRepair {
    owner: String,
    run_id: String,
    result: WorkerResult,
    usage: Vec<crate::llm::route_decision::MeteredTokenUsageRecord>,
}

impl ConfirmedProcessRepair {
    pub(super) fn matches(
        &self,
        run_id: &str,
        owner: Option<&str>,
        status: &super::SubAgentStatus,
        usage: &[crate::llm::route_decision::MeteredTokenUsageRecord],
    ) -> bool {
        if self.run_id != run_id || owner != Some(self.owner.as_str()) || self.usage != usage {
            return false;
        }
        match (self.result.success, status) {
            (true, super::SubAgentStatus::Completed(output)) => output == &self.result.output,
            (false, super::SubAgentStatus::Failed(error)) => self
                .result
                .error
                .as_deref()
                .is_some_and(|original| *error == super::sub_agent_reported_failure(original)),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_WARNED_LAUNCHES, UnconfirmedWarning, admit_unconfirmed_warning};
    use std::collections::HashSet;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn saturated_suppression_set_keeps_known_launches_and_reports_overflow_once() {
        let saturation_reported = AtomicBool::new(false);
        let mut warned = HashSet::new();
        let limit = i64::try_from(MAX_WARNED_LAUNCHES).expect("test: limit fits i64");
        for id in 1..=limit {
            assert_eq!(
                admit_unconfirmed_warning(&mut warned, id, &saturation_reported),
                UnconfirmedWarning::Warn,
                "launch {id} within the bound must be remembered and warned once"
            );
        }
        // A remembered launch stays silent after saturation, and the set is not cleared.
        assert_eq!(
            admit_unconfirmed_warning(&mut warned, 1, &saturation_reported),
            UnconfirmedWarning::Suppressed,
            "saturated set must keep already-remembered launch ids silent"
        );
        assert_eq!(
            warned.len(),
            MAX_WARNED_LAUNCHES,
            "saturated set must stay full, not be cleared"
        );

        // The first launch that cannot be remembered raises the aggregate warning; later ones do not.
        let overflow = limit + 1;
        assert_eq!(
            admit_unconfirmed_warning(&mut warned, overflow, &saturation_reported),
            UnconfirmedWarning::Unremembered { aggregate: true },
            "first launch that cannot be remembered must raise the saturation warning"
        );
        assert_eq!(
            admit_unconfirmed_warning(&mut warned, overflow + 1, &saturation_reported),
            UnconfirmedWarning::Unremembered { aggregate: false },
            "later unremembered launches must not repeat the saturation warning"
        );
        assert_eq!(
            admit_unconfirmed_warning(&mut warned, 1, &saturation_reported),
            UnconfirmedWarning::Suppressed,
            "remembered launch must stay silent after overflow"
        );
        assert_eq!(
            warned.len(),
            MAX_WARNED_LAUNCHES,
            "overflow launches must not be inserted into a full set"
        );
        assert!(
            !warned.contains(&overflow),
            "launch {overflow} that did not fit must not be remembered"
        );
    }
}

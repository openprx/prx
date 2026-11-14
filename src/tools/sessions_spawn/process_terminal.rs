//! Verification and recovery of worker completion against logical source storage.
pub(super) mod operator;
pub(super) mod ordinary;
mod process_identity;
use crate::agent::terminal::TurnTerminalStatus;
use crate::memory::{MemoryFabric, MessageEvent, MessageEventSource, MessageEventSubject};
use crate::session_worker::protocol::WorkerResult;
use anyhow::Context;
use sha2::{Digest, Sha256};

pub(super) async fn run_restart_recovery(
    fabric: MemoryFabric,
    workspace: std::path::PathBuf,
    runs: std::sync::Arc<tokio::sync::RwLock<Vec<super::SubAgentRun>>>,
) {
    let mut cursor = 0;
    loop {
        let delay = match recover_restart_page(&fabric, &workspace, cursor, 128).await {
            Ok(page) => {
                let exhausted = page.outcomes.len() < 128;
                for outcome in page.outcomes {
                    match outcome.result {
                        Ok(Some(recovered)) => {
                            if recovered.restore(&runs).await.is_err() {
                                tracing::warn!(
                                    launch_event_id = outcome.launch_event_id,
                                    "worker restart registry reconciliation remains unconfirmed"
                                );
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
    source: MessageEvent,
    batch_id: Option<String>,
    outcome: RecoveredProcessOutcome,
}

enum RecoveredProcessOutcome {
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
            anyhow::ensure!(
                existing.owner_id == source.owner_id
                    && existing.source_message_event_id.as_deref() == Some(source.event_id.as_str()),
                "recovered worker conflicts with registry ownership"
            );
            if !existing
                .process_control
                .as_ref()
                .is_some_and(|control| control.recovery_only)
            {
                return Ok(false);
            }
            // A later sweep cannot discard or change an already authenticated
            // settlement, including while upgrading to a completed journal.
            anyhow::ensure!(
                existing.token_usage_records.iter().all(|known| usage.contains(known)),
                "worker recovery would lose or change a confirmed settlement"
            );
        }
        let run = SubAgentRun {
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

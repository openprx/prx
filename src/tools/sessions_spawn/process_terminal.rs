//! Verification and recovery of worker completion against logical source storage.
mod launch;
pub(super) mod operator;
pub(super) mod ordinary;
use crate::agent::terminal::{TurnTerminalStatus, verify_terminal_replay};
use crate::causal_tree::store::CausalDecisionStore;
use crate::memory::{MemoryFabric, MessageEvent, MessageEventSource, MessageEventSubject};
use crate::runtime::child_source::{ChildInputReceipt, IssuedChildGrant};
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
    Complete(VerifiedWorkerCompletion),
    Incomplete(RecoveredWorkerUsage),
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
            RecoveredProcessOutcome::Complete(completion) => {
                anyhow::ensure!(
                    completion.run_id == run_id,
                    "recovered worker result belongs to another run"
                );
                let status = if completion.result.success {
                    SubAgentStatus::Completed(completion.result.output.clone())
                } else {
                    SubAgentStatus::Failed(super::sub_agent_reported_failure(
                        completion
                            .result
                            .error
                            .as_deref()
                            .context("recovered worker failure missing")?,
                    ))
                };
                let history = if completion.result.success {
                    vec![HistoryEntry {
                        role: "assistant".to_owned(),
                        content: completion.result.output,
                        timestamp: completion.finished_at,
                    }]
                } else {
                    Vec::new()
                };
                (status, Some(completion.finished_at), completion.usage, history, None)
            }
            RecoveredProcessOutcome::Incomplete(recovered) => (
                SubAgentStatus::Failed(format!(
                    "{}: completion journal unavailable; cleanup and provider usage remain unconfirmed; {} visible segments lack a terminal",
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
    let owner = event.owner_id.as_deref().context("worker discovery owner missing")?;
    let run = event.run_id.as_deref().context("worker discovery run missing")?;
    let launch = launch::read(fabric, workspace, owner, run)
        .await?
        .context("worker discovery launch missing")?;
    anyhow::ensure!(
        event.id == launch.event_id()
            && event.idempotency_key.as_deref() == Some(format!("worker:{run}:launch").as_str())
            && event.event_type == "worker.recovery.registered",
        "worker discovery record is not a canonical launch"
    );
    if !launch.process_absent()? {
        return Ok(None);
    }
    let outcome = match recover_from_launch(fabric, &launch).await? {
        Some(completion) => RecoveredProcessOutcome::Complete(completion),
        None => RecoveredProcessOutcome::Incomplete(recover_usage_bound(fabric, &launch.provenance()).await?),
    };
    Ok(Some(RecoveredProcessRun {
        source: launch.provenance().input.clone(),
        batch_id: launch.batch_id().map(str::to_owned),
        outcome,
    }))
}

#[derive(Clone)]
pub(super) struct VerifiedWorkerCompletion {
    run_id: String,
    result: WorkerResult,
    usage: Vec<crate::llm::route_decision::MeteredTokenUsageRecord>,
    finished_at: chrono::DateTime<chrono::Utc>,
}

impl VerifiedWorkerCompletion {
    pub(super) fn run_id(&self) -> &str {
        &self.run_id
    }

    pub(super) fn into_parts(self) -> (WorkerResult, Vec<crate::llm::route_decision::MeteredTokenUsageRecord>) {
        (self.result, self.usage)
    }
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

/// Borrowed replay facts only; this type cannot admit execution.
struct RecoveryProvenance<'a> {
    workspace: &'a std::path::Path,
    store_workspace: &'a str,
    owner: &'a str,
    parent_decision: crate::causal_tree::ids::DecisionId,
    parent_digest: &'a str,
    input: &'a MessageEvent,
    key: &'a crate::decision::DecisionKey,
}

pub(super) async fn verify(
    fabric: &MemoryFabric,
    grant: &IssuedChildGrant,
    input: &ChildInputReceipt,
    result: WorkerResult,
) -> anyhow::Result<VerifiedWorkerCompletion> {
    grant.verify_receipt(input)?;
    let (child, constraints) = grant.scope().gate_inputs();
    let (store_workspace, parent_digest) = grant.parent_record_binding();
    let provenance = RecoveryProvenance {
        workspace: &grant.generation().effective.workspace_dir,
        store_workspace,
        owner: &child.authority.owner,
        parent_decision: child.parent_decision_id,
        parent_digest,
        input: input.event(),
        key: &constraints.key,
    };
    verify_bound(fabric, &provenance, result, b"worker-completion-v1", None).await
}

async fn verify_bound(
    fabric: &MemoryFabric,
    provenance: &RecoveryProvenance<'_>,
    result: WorkerResult,
    signature_domain: &[u8],
    repair_store: Option<&CausalDecisionStore>,
) -> anyhow::Result<VerifiedWorkerCompletion> {
    anyhow::ensure!(
        result.ordinary_completion.is_none() && result.operator_completion.is_none(),
        "durable worker returned mixed completion authority"
    );
    let evidence = result
        .completion_evidence
        .as_ref()
        .context("worker completion evidence missing")?;
    anyhow::ensure!(
        provenance.key.verify_hmac(
            crate::decision::key::KeyDomain::StateFingerprint,
            &[
                signature_domain,
                provenance.parent_decision.to_string().as_bytes(),
                &evidence.signing_bytes(&result)?,
            ],
            &evidence.seal,
        ),
        "worker completion authentication failed"
    );
    evidence.validate_inventory(result.success)?;
    let source = provenance.input;
    anyhow::ensure!(
        fabric.workspace_id() == provenance.workspace.to_string_lossy()
            && source.run_id.as_deref() == Some(evidence.run_id.as_str())
            && source.event_id == evidence.input_event_id,
        "worker completion lookup differs from issued source"
    );
    let logical = read_terminal_with_source(
        fabric,
        provenance.owner,
        &evidence.terminal_event_id,
        &evidence.terminal_event_digest,
        &evidence.run_id,
        &source.source,
    )
    .await?;
    anyhow::ensure!(
        logical.session_key == source.session_key
            && logical.parent_run_id == source.parent_run_id
            && logical.config_generation_id == source.config_generation_id
            && logical.config_source_revision == source.config_source_revision,
        "worker logical terminal lineage differs from input"
    );
    let payload: serde_json::Value = serde_json::from_str(
        logical
            .raw_payload_json
            .as_deref()
            .context("worker logical terminal payload missing")?,
    )?;
    let status: TurnTerminalStatus = serde_json::from_value(
        payload
            .get("status")
            .cloned()
            .context("worker logical terminal status missing")?,
    )?;
    anyhow::ensure!(
        payload.get("terminal_id").and_then(serde_json::Value::as_str) == Some(evidence.run_id.as_str())
            && payload.get("decision_id").is_some_and(serde_json::Value::is_null)
            && payload.get("provider_outcome").is_some_and(serde_json::Value::is_null)
            && payload.get("usage_settlement").is_some_and(serde_json::Value::is_null)
            && payload.get("cost_settlement").is_some_and(serde_json::Value::is_null)
            && status
                == if result.success {
                    TurnTerminalStatus::Completed
                } else {
                    TurnTerminalStatus::Failed
                },
        "worker logical terminal result or billing differs from completion"
    );
    if result.success {
        let expected = crate::agent::terminal::redact_persisted_text(&result.output);
        anyhow::ensure!(
            payload
                .get("history")
                .and_then(|history| history.get("assistant_content"))
                .and_then(serde_json::Value::as_str)
                == Some(expected.as_str()),
            "worker output differs from durable history projection"
        );
    }
    if !result.success {
        let expected = crate::agent::terminal::redact_persisted_text(
            result.error.as_deref().context("failed worker result has no error")?,
        );
        anyhow::ensure!(
            payload.get("history").is_some_and(serde_json::Value::is_null)
                && payload
                    .get("telemetry")
                    .and_then(|value| value.get("summary"))
                    .and_then(serde_json::Value::as_str)
                    == Some(expected.as_str()),
            "worker failure differs from durable terminal"
        );
    }
    let mut usage_records = Vec::new();
    let store = CausalDecisionStore::open_unshared_read_only(provenance.workspace);
    let actual_store = repair_store.unwrap_or(&store);
    anyhow::ensure!(
        actual_store.workspace_id().as_deref() == Some(provenance.store_workspace),
        "worker completion decision store differs from original namespace"
    );
    let reader = actual_store.reader();
    let memory = fabric.memory();
    let scope = crate::causal_tree::store::DecisionScope::Owner {
        workspace_id: std::sync::Arc::from(provenance.store_workspace),
        owner_id: std::sync::Arc::from(provenance.owner),
    };
    for reference in &evidence.segments {
        let event = read_terminal(
            fabric,
            provenance.owner,
            &reference.event_id,
            &reference.event_digest,
            &reference.terminal_id,
        )
        .await?;
        anyhow::ensure!(
            event.subject == Some(MessageEventSubject::Task(evidence.run_id.clone()))
                && event.session_key == source.session_key
                && event.parent_run_id == source.parent_run_id
                && event.config_generation_id == source.config_generation_id
                && event.config_source_revision == source.config_source_revision,
            "worker segment terminal lineage differs from task"
        );
        let replay = verify_terminal_replay(
            memory.as_ref(),
            &reader,
            provenance.key,
            provenance.store_workspace,
            fabric.workspace_id(),
            provenance.owner,
            &reference.terminal_id,
            &event,
        )
        .await?;
        anyhow::ensure!(
            replay.parent_decision_id() == Some(provenance.parent_decision)
                && replay.parent_digest() == Some(provenance.parent_digest)
                && replay.source().subject == Some(MessageEventSubject::Task(evidence.run_id.clone())),
            "worker segment replay differs from issued parent"
        );
        // A worker completion must preserve the parent's durable view of each
        // segment, not just the transport result and its billing record.
        let facts = crate::causal_tree::outcome::TerminalFacts::from_persisted_marker(
            &event,
            replay.decision(),
            provenance.owner,
            fabric.workspace_id(),
        )?
        .with_verified_resume(&replay);
        let outcome = crate::causal_tree::outcome::OutcomeCorrelator::persisted_record(replay.decision(), &facts)?;
        let link = crate::causal_tree::store::ChildOutcomeLink::from_replay(&replay, &outcome)
            .context("worker segment outcome lineage is inconsistent")?;
        if !reader.child_outcome_matches(&scope, &link)? {
            let writer = repair_store
                .and_then(CausalDecisionStore::writer)
                .context("worker parent outcome repair requires an existing owned writer")?;
            anyhow::ensure!(
                writer.record_outcome(outcome.clone()) == crate::causal_tree::store::Enqueue::Queued,
                "recovered worker outcome could not be queued"
            );
            anyhow::ensure!(
                writer
                    .drain(std::time::Instant::now() + std::time::Duration::from_secs(5))
                    .await
                    && reader.outcome_matches(&scope, &outcome)?,
                "recovered worker outcome is missing or conflicts"
            );
            let repair = crate::causal_tree::store::ChildOutcomeLink::from_replay(&replay, &outcome)
                .context("recovered worker outcome lineage changed")?;
            anyhow::ensure!(
                writer.record_child_outcome(repair) == crate::causal_tree::store::Enqueue::Queued,
                "recovered worker parent link could not be queued"
            );
            anyhow::ensure!(
                writer
                    .drain(std::time::Instant::now() + std::time::Duration::from_secs(5))
                    .await,
                "recovered worker parent link persistence is unconfirmed"
            );
        }
        anyhow::ensure!(
            reader.child_outcome_matches(&scope, &link)?,
            "worker segment parent outcome link is missing or inconsistent"
        );
        let payload: serde_json::Value = serde_json::from_str(
            event
                .raw_payload_json
                .as_deref()
                .context("worker segment payload missing")?,
        )?;
        let usage: Option<crate::llm::route_decision::MeteredTokenUsageRecord> = serde_json::from_value(
            payload
                .get("usage_settlement")
                .cloned()
                .context("worker segment settlement field missing")?,
        )?;
        if let Some(usage) = usage {
            anyhow::ensure!(
                usage.settlement_id.as_deref() == Some(reference.terminal_id.as_str()),
                "worker usage settlement differs from verified segment"
            );
            usage_records.push(usage);
        }
    }
    Ok(VerifiedWorkerCompletion {
        run_id: evidence.run_id.clone(),
        result,
        usage: usage_records,
        finished_at: chrono::DateTime::parse_from_rfc3339(&logical.created_at)?.with_timezone(&chrono::Utc),
    })
}

/// Confirmed usage only. Recovery cannot prove that interrupted provider calls
/// were unbilled, even when every visible segment has a terminal.
pub(super) struct RecoveredWorkerUsage {
    pub(super) records: Vec<crate::llm::route_decision::MeteredTokenUsageRecord>,
    pub(super) unfinished_segments: u64,
}

pub(super) async fn recover_usage(
    fabric: &MemoryFabric,
    grant: &IssuedChildGrant,
    input: &ChildInputReceipt,
) -> anyhow::Result<RecoveredWorkerUsage> {
    grant.verify_receipt(input)?;
    let (child, constraints) = grant.scope().gate_inputs();
    let (store_workspace, parent_digest) = grant.parent_record_binding();
    let provenance = RecoveryProvenance {
        workspace: &grant.generation().effective.workspace_dir,
        store_workspace,
        owner: &child.authority.owner,
        parent_decision: child.parent_decision_id,
        parent_digest,
        input: input.event(),
        key: &constraints.key,
    };
    recover_usage_bound(fabric, &provenance).await
}

async fn recover_usage_bound(
    fabric: &MemoryFabric,
    provenance: &RecoveryProvenance<'_>,
) -> anyhow::Result<RecoveredWorkerUsage> {
    let source = provenance.input;
    anyhow::ensure!(
        fabric.workspace_id() == provenance.workspace.to_string_lossy(),
        "worker recovery workspace differs from provenance"
    );
    let task = source.run_id.as_deref().context("worker recovery task missing")?;
    let memory = fabric.memory();
    let store = CausalDecisionStore::open_unshared_read_only(provenance.workspace);
    anyhow::ensure!(
        store.workspace_id().as_deref() == Some(provenance.store_workspace),
        "worker usage recovery decision store differs from original namespace"
    );
    let reader = store.reader();
    let mut recovered = RecoveredWorkerUsage {
        records: Vec::new(),
        unfinished_segments: 0,
    };
    let mut index = 0_u64;
    loop {
        let run = format!("child-segment:{}:{index}", source.event_id);
        let Some(segment_input) = memory
            .find_owned_message_event_by_idempotency_key(
                fabric.workspace_id(),
                provenance.owner,
                &format!("{run}:input"),
            )
            .await?
        else {
            break;
        };
        anyhow::ensure!(
            segment_input.id > 0
                && segment_input.workspace_id == fabric.workspace_id()
                && segment_input.owner_id.as_deref() == Some(provenance.owner)
                && segment_input.run_id.as_deref() == Some(run.as_str())
                && segment_input.source == MessageEventSource::SessionWorker
                && segment_input.subject == Some(MessageEventSubject::Task(task.to_owned()))
                && segment_input.session_key == source.session_key
                && segment_input.parent_run_id == source.parent_run_id
                && segment_input.config_generation_id == source.config_generation_id
                && segment_input.config_source_revision == source.config_source_revision,
            "worker recovery input differs from task lineage"
        );
        if let Some(event) = memory
            .find_owned_message_event_by_idempotency_key(
                fabric.workspace_id(),
                provenance.owner,
                &format!("turn:{run}:final"),
            )
            .await?
        {
            let replay = verify_terminal_replay(
                memory.as_ref(),
                &reader,
                provenance.key,
                provenance.store_workspace,
                fabric.workspace_id(),
                provenance.owner,
                &run,
                &event,
            )
            .await?;
            anyhow::ensure!(
                replay.parent_decision_id() == Some(provenance.parent_decision)
                    && replay.parent_digest() == Some(provenance.parent_digest)
                    && replay.source().event_id == segment_input.event_id,
                "recovered worker terminal belongs to another input or parent"
            );
            let payload: serde_json::Value = serde_json::from_str(
                event
                    .raw_payload_json
                    .as_deref()
                    .context("recovered terminal payload missing")?,
            )?;
            let usage: Option<crate::llm::route_decision::MeteredTokenUsageRecord> = serde_json::from_value(
                payload
                    .get("usage_settlement")
                    .cloned()
                    .context("recovered usage field missing")?,
            )?;
            if let Some(usage) = usage {
                anyhow::ensure!(
                    usage.settlement_id.as_deref() == Some(run.as_str()),
                    "recovered usage belongs to another settlement"
                );
                recovered.records.push(usage);
            }
        } else {
            recovered.unfinished_segments = recovered
                .unfinished_segments
                .checked_add(1)
                .context("worker unfinished segment count overflow")?;
        }
        index = index.checked_add(1).context("worker recovery segment count overflow")?;
        if index.is_multiple_of(64) {
            tokio::task::yield_now().await;
        }
    }
    Ok(recovered)
}

/// Recover the worker's cleanup-ordered durable result only after its process
/// owner has confirmed exit. A record does not prove OS process liveness.
pub(super) async fn recover_completion(
    fabric: &MemoryFabric,
    grant: &IssuedChildGrant,
    input: &ChildInputReceipt,
) -> anyhow::Result<Option<VerifiedWorkerCompletion>> {
    let Some(launch) = read_bound_launch(fabric, grant, input).await? else {
        return Ok(None);
    };
    recover_from_launch(fabric, &launch).await
}

async fn read_bound_launch(
    fabric: &MemoryFabric,
    grant: &IssuedChildGrant,
    input: &ChildInputReceipt,
) -> anyhow::Result<Option<launch::VerifiedLaunch>> {
    grant.verify_receipt(input)?;
    let (child, _) = grant.scope().gate_inputs();
    let run = input.event().run_id.as_deref().context("worker recovery run missing")?;
    let Some(launch) = launch::read(
        fabric,
        &grant.generation().effective.workspace_dir,
        &child.authority.owner,
        run,
    )
    .await?
    else {
        return Ok(None);
    };
    let provenance = launch.provenance();
    anyhow::ensure!(
        provenance.parent_decision == child.parent_decision_id
            && provenance.store_workspace == grant.parent_record_binding().0
            && provenance.parent_digest == grant.parent_record_binding().1
            && provenance.input.event_id == input.event().event_id,
        "worker recovery launch differs from retained grant"
    );
    Ok(Some(launch))
}

async fn recover_from_launch(
    fabric: &MemoryFabric,
    launch: &launch::VerifiedLaunch,
) -> anyhow::Result<Option<VerifiedWorkerCompletion>> {
    let provenance = launch.provenance();
    let source = provenance.input;
    anyhow::ensure!(
        fabric.workspace_id() == provenance.workspace.to_string_lossy(),
        "worker recovery workspace differs from verified launch"
    );
    let run = source.run_id.as_deref().context("worker recovery run missing")?;
    let key = format!("worker:{run}:completion");
    let Some(event) = fabric
        .memory()
        .find_owned_message_event_by_idempotency_key(fabric.workspace_id(), provenance.owner, &key)
        .await?
    else {
        return Ok(None);
    };
    anyhow::ensure!(
        event.id > 0
            && !event.event_id.is_empty()
            && event.workspace_id == fabric.workspace_id()
            && event.owner_id.as_deref() == Some(provenance.owner)
            && event.source == MessageEventSource::SessionWorker
            && event.role == "event"
            && event.event_type == "worker.completion.confirmed"
            && event.idempotency_key.as_deref() == Some(key.as_str())
            && event.run_id == source.run_id
            && event.session_key == source.session_key
            && event.parent_run_id == source.parent_run_id
            && event.config_generation_id == source.config_generation_id
            && event.config_source_revision == source.config_source_revision,
        "worker completion journal differs from recovery scope"
    );
    let result: WorkerResult = serde_json::from_str(
        event
            .raw_payload_json
            .as_deref()
            .context("worker completion journal payload missing")?,
    )?;
    // All transport, inventory, logical terminal and segment replay checks are
    // shared with normal completion. Journal presence alone grants nothing.
    let repair_store = CausalDecisionStore::existing_shared_writer(provenance.workspace);
    verify_bound(
        fabric,
        &provenance,
        result,
        b"worker-completion-journal-v1",
        repair_store.as_deref(),
    )
    .await
    .map(Some)
}

/// Retained outside the monitor. The live owner's exit acknowledgement remains
/// separate from independently verified absence plus a cleanup-ordered journal.
pub(super) struct ProcessRecovery {
    fabric: Option<MemoryFabric>,
    grant: std::sync::Arc<IssuedChildGrant>,
    input: Option<std::sync::Arc<ChildInputReceipt>>,
    batch_id: Option<String>,
    exited: std::sync::atomic::AtomicBool,
    recovered_absence: std::sync::atomic::AtomicBool,
}

impl ProcessRecovery {
    pub(super) const fn new(
        fabric: Option<MemoryFabric>,
        grant: std::sync::Arc<IssuedChildGrant>,
        input: Option<std::sync::Arc<ChildInputReceipt>>,
        batch_id: Option<String>,
    ) -> Self {
        Self {
            fabric,
            grant,
            input,
            batch_id,
            exited: std::sync::atomic::AtomicBool::new(false),
            recovered_absence: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(super) async fn persist_launch(&self, pid: u32) -> anyhow::Result<()> {
        launch::persist(
            self.fabric.as_ref().context("worker launch storage missing")?,
            &self.grant,
            self.input.as_deref().context("worker launch input missing")?,
            pid,
            self.batch_id.as_deref(),
        )
        .await
    }

    pub(super) fn confirm_exit(&self, run_id: &str) -> anyhow::Result<()> {
        let input = self.input.as_ref().context("process exit source missing")?;
        anyhow::ensure!(
            input.event().run_id.as_deref() == Some(run_id),
            "process exit belongs to another worker"
        );
        self.exited.store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    pub(super) async fn recover(&self) -> anyhow::Result<Option<VerifiedWorkerCompletion>> {
        let fabric = self.fabric.as_ref().context("process recovery fabric missing")?;
        let input = self.input.as_deref().context("process recovery source missing")?;
        if !self.exited.load(std::sync::atomic::Ordering::Acquire) {
            let Some(launch) = read_bound_launch(fabric, &self.grant, input).await? else {
                return Ok(None);
            };
            if !launch.process_absent()? {
                return Ok(None);
            }
            let completion = recover_from_launch(fabric, &launch).await?;
            if completion.is_some() {
                // Absence alone says nothing about descendant cleanup or
                // billing. The verified journal was written after cleanup.
                self.recovered_absence.store(true, std::sync::atomic::Ordering::Release);
            }
            return Ok(completion);
        }
        recover_completion(fabric, &self.grant, input).await
    }
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

impl ProcessRecovery {
    pub(super) fn repair(&self, completion: VerifiedWorkerCompletion) -> anyhow::Result<ConfirmedProcessRepair> {
        anyhow::ensure!(
            self.exited.load(std::sync::atomic::Ordering::Acquire)
                || self.recovered_absence.load(std::sync::atomic::Ordering::Acquire),
            "process repair requires confirmed exit and completion"
        );
        let input = self.input.as_deref().context("process repair input missing")?;
        self.grant.verify_receipt(input)?;
        anyhow::ensure!(
            input.event().run_id.as_deref() == Some(completion.run_id()),
            "process repair completion belongs to another run"
        );
        Ok(ConfirmedProcessRepair {
            owner: self.grant.scope().gate_inputs().0.authority.owner.clone(),
            run_id: completion.run_id,
            result: completion.result,
            usage: completion.usage,
        })
    }
}

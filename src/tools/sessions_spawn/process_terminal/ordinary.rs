//! Parent verification for an ordinary process, without a durable decision grant.
pub(in crate::tools::sessions_spawn) mod launch;
mod segments;
use super::{
    ConfirmedProcessRepair, Context, Digest, MemoryFabric, MessageEvent, MessageEventSource, MessageEventSubject,
    RecoveredProcessOutcome, RecoveredProcessRun, RecoveredWorkerUsage, Sha256, TurnTerminalStatus, WorkerResult,
    read_terminal,
};
use crate::session_worker::protocol::WorkerManifest;
use crate::tools::capability::child::{CapabilityConstraints, ChildCapabilityManifest, VerifiedChildScope};

/// Read-only completion provenance. Deliberately excludes task text, credentials,
/// provider catalogs and executable capability snapshots.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CompletionLineage {
    run_id: String,
    session_key: String,
    parent_run_id: Option<String>,
    parent_session: String,
    owner_id: Option<String>,
    config_generation_id: Option<u64>,
    config_source_revision: Option<String>,
    channel: Option<String>,
    sender: Option<String>,
    recipient: Option<String>,
    agent_id: Option<String>,
    persona_id: Option<String>,
    attempt_id: Option<String>,
    lease_epoch: Option<i64>,
    parent_scope_id: String,
    input_event_id: Option<String>,
    recording_enabled: bool,
}

impl CompletionLineage {
    fn from_worker(worker: &WorkerManifest, parent_scope_id: String, parent_session: String) -> anyhow::Result<Self> {
        let source = worker.ordinary_source.as_ref().context("ordinary source missing")?;
        Ok(Self {
            run_id: worker.run_id.clone(),
            session_key: worker.session_scope_key.clone(),
            parent_run_id: worker.parent_run_id.clone(),
            parent_session,
            owner_id: worker.owner_id.clone(),
            config_generation_id: worker.runtime_config_generation_id,
            config_source_revision: worker.runtime_config_source_revision.clone(),
            channel: worker.scope_channel.clone(),
            sender: worker.scope_sender.clone(),
            recipient: worker.scope_chat_id.clone(),
            agent_id: worker.agent_id.clone(),
            persona_id: worker.persona_id.clone(),
            attempt_id: source.attempt_id.clone(),
            lease_epoch: source.lease_epoch,
            parent_scope_id,
            input_event_id: source.input.as_ref().map(|input| input.event_id.clone()),
            recording_enabled: worker.memory_event_recording.enabled,
        })
    }
}

#[derive(Clone)]
pub(crate) struct VerifiedOrdinaryWorkerCompletion {
    run_id: String,
    owner: String,
    exit_confirmed: bool,
    pub(crate) result: WorkerResult,
    pub(crate) usage: Vec<crate::llm::route_decision::MeteredTokenUsageRecord>,
    /// Absent when recording is off; local completion is not durable recovery.
    pub(crate) finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl VerifiedOrdinaryWorkerCompletion {
    /// Called only by the process owner after the owned process group is reaped.
    pub(in crate::tools::sessions_spawn) fn confirm_reaped(&mut self, run_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(self.run_id == run_id, "ordinary exit belongs to another worker");
        self.exit_confirmed = true;
        Ok(())
    }

    pub(in crate::tools::sessions_spawn) fn repair(&self) -> anyhow::Result<ConfirmedProcessRepair> {
        anyhow::ensure!(self.exit_confirmed, "ordinary repair requires confirmed process exit");
        Ok(ConfirmedProcessRepair {
            owner: self.owner.clone(),
            run_id: self.run_id.clone(),
            result: self.result.clone(),
            usage: self.usage.clone(),
        })
    }
}

pub(crate) async fn verify(
    fabric: &MemoryFabric,
    worker: &WorkerManifest,
    generation: &crate::config::ConfigGeneration,
    transport_key: &ring::hmac::Key,
    result: WorkerResult,
) -> anyhow::Result<VerifiedOrdinaryWorkerCompletion> {
    verify_bound(
        fabric,
        worker,
        generation,
        transport_key,
        result,
        b"ordinary-worker-completion-v1",
    )
    .await
}

async fn verify_bound(
    fabric: &MemoryFabric,
    worker: &WorkerManifest,
    generation: &crate::config::ConfigGeneration,
    transport_key: &ring::hmac::Key,
    result: WorkerResult,
    domain: &[u8],
) -> anyhow::Result<VerifiedOrdinaryWorkerCompletion> {
    anyhow::ensure!(
        result.completion_evidence.is_none() && result.operator_completion.is_none(),
        "ordinary completion rejects granted evidence"
    );
    let (key, lineage) = authenticate_launch(fabric, worker, generation, transport_key).await?;
    verify_completion(fabric, &lineage, &key, result, domain).await
}

pub(super) async fn authenticate_launch(
    fabric: &MemoryFabric,
    worker: &WorkerManifest,
    generation: &crate::config::ConfigGeneration,
    transport_key: &ring::hmac::Key,
) -> anyhow::Result<(std::sync::Arc<crate::decision::DecisionKey>, CompletionLineage)> {
    let source = worker
        .ordinary_source
        .as_ref()
        .context("ordinary launch source missing")?;
    anyhow::ensure!(
        worker.child_source.is_none()
            && worker.capability_manifest.is_none()
            && !generation.effective.causal_tree.enabled
            && fabric.workspace_id() == generation.effective.workspace_dir.to_string_lossy(),
        "ordinary completion requires its exact launch source"
    );
    let recording = generation.effective.memory.event_recording_config();
    anyhow::ensure!(
        worker.memory_event_recording.enabled == recording.enabled
            && worker.memory_event_recording.record_user_messages == recording.record_user_messages
            && worker.memory_event_recording.record_assistant_messages == recording.record_assistant_messages
            && worker.memory_event_recording.record_tool_events == recording.record_tool_events,
        "ordinary launch recording policy differs from generation"
    );
    let key = crate::decision::key::DecisionKey::from_worker_channel(
        transport_key,
        fabric.workspace_id(),
        &worker.run_id,
        &worker.config_generation,
    )?;
    let parent =
        CapabilityConstraints::from_worker_constraints(&source.parent_constraints, std::sync::Arc::clone(&key))?;
    let child = ChildCapabilityManifest::from_worker_manifest(&source.capability_manifest, &parent)?;
    let scope = VerifiedChildScope::new(child, &parent)?;
    let restored = crate::config::ConfigGeneration::restore_process(
        generation.effective.as_ref().clone(),
        &source.config_generation,
        &key,
    )?;
    anyhow::ensure!(
        restored.id == generation.id && restored.source_revision == generation.source_revision,
        "ordinary completion configuration differs from launch"
    );
    let provenance =
        crate::runtime::child_source::parent::ParentSource::restore_ordinary_process(worker, &scope, &restored)?;
    crate::runtime::child_source::ordinary_process::restore_input(fabric, worker, &scope, &provenance).await?;
    let lineage = CompletionLineage::from_worker(
        worker,
        scope.gate_inputs().0.parent_decision_id.to_string(),
        provenance.session_key().to_owned(),
    )?;
    Ok((key, lineage))
}

async fn verify_completion(
    fabric: &MemoryFabric,
    lineage: &CompletionLineage,
    key: &crate::decision::DecisionKey,
    result: WorkerResult,
    domain: &[u8],
) -> anyhow::Result<VerifiedOrdinaryWorkerCompletion> {
    let owner = lineage
        .owner_id
        .as_deref()
        .context("ordinary completion owner missing")?;
    anyhow::ensure!(
        result.completion_evidence.is_none() && result.operator_completion.is_none(),
        "ordinary completion rejects granted evidence"
    );
    let evidence = result
        .ordinary_completion
        .as_ref()
        .context("ordinary worker completion evidence missing")?;
    anyhow::ensure!(
        key.verify_hmac(
            crate::decision::key::KeyDomain::StateFingerprint,
            &[
                domain,
                lineage.parent_scope_id.as_bytes(),
                &evidence.signing_bytes(&result)?
            ],
            &evidence.seal,
        ),
        "ordinary worker completion authentication failed"
    );
    evidence.validate_inventory(result.success)?;
    anyhow::ensure!(
        result.success || result.error.as_deref().is_some_and(|error| !error.is_empty()),
        "ordinary failed completion requires failure details"
    );
    anyhow::ensure!(
        evidence.run_id == lineage.run_id
            && evidence.input_event_id.as_deref() == lineage.input_event_id.as_deref()
            && evidence.recording_enabled == lineage.recording_enabled,
        "ordinary completion inventory differs from launch"
    );
    let mut verified_usage = Vec::new();
    let finished_at = if let Some(reference) = &evidence.terminal {
        let event = read_terminal(
            fabric,
            owner,
            &reference.event_id,
            &reference.event_digest,
            &lineage.run_id,
        )
        .await?;
        verify_lineage(&event, lineage)?;
        let payload = terminal_payload(&event)?;
        let expected_status = if result.success {
            TurnTerminalStatus::Completed
        } else {
            TurnTerminalStatus::Failed
        };
        let status: TurnTerminalStatus = serde_json::from_value(
            payload
                .get("status")
                .cloned()
                .context("ordinary terminal status missing")?,
        )?;
        anyhow::ensure!(
            status == expected_status
                && payload.get("terminal_id").and_then(serde_json::Value::as_str) == Some(lineage.run_id.as_str())
                && ["decision_id", "provider_outcome", "usage_settlement", "cost_settlement"]
                    .iter()
                    .all(|key| payload.get(key).is_some_and(serde_json::Value::is_null)),
            "ordinary logical terminal result or billing differs"
        );
        if result.success {
            let expected = crate::agent::terminal::redact_persisted_text(&result.output);
            anyhow::ensure!(
                payload
                    .get("history")
                    .and_then(|history| history.get("assistant_content"))
                    .and_then(serde_json::Value::as_str)
                    == Some(expected.as_str()),
                "ordinary output differs from terminal history"
            );
        } else {
            let expected = crate::agent::terminal::redact_persisted_text(
                result.error.as_deref().context("ordinary failure detail missing")?,
            );
            anyhow::ensure!(
                payload.get("history").is_some_and(serde_json::Value::is_null)
                    && payload
                        .get("telemetry")
                        .and_then(|telemetry| telemetry.get("summary"))
                        .and_then(serde_json::Value::as_str)
                        == Some(expected.as_str()),
                "ordinary error differs from terminal history"
            );
        }
        for reference in &evidence.segments {
            let event = read_terminal(
                fabric,
                owner,
                &reference.event_id,
                &reference.event_digest,
                &reference.terminal_id,
            )
            .await?;
            verify_lineage(&event, lineage)?;
            anyhow::ensure!(
                event.subject == Some(MessageEventSubject::Task(lineage.run_id.clone())),
                "ordinary segment belongs to another task"
            );
            let payload = terminal_payload(&event)?;
            anyhow::ensure!(
                payload.get("terminal_id").and_then(serde_json::Value::as_str) == Some(reference.terminal_id.as_str())
                    && payload.get("decision_id").is_some_and(serde_json::Value::is_null),
                "ordinary segment asserts a different decision or terminal"
            );
            let usage: Option<crate::llm::route_decision::MeteredTokenUsageRecord> = serde_json::from_value(
                payload
                    .get("usage_settlement")
                    .cloned()
                    .context("ordinary segment usage field missing")?,
            )?;
            if let Some(usage) = usage {
                anyhow::ensure!(
                    usage.settlement_id.as_deref() == Some(reference.terminal_id.as_str()),
                    "ordinary usage belongs to another segment"
                );
                verified_usage.push(usage);
            }
        }
        anyhow::ensure!(
            verified_usage == evidence.usage,
            "ordinary completion usage differs from stored terminals"
        );
        Some(chrono::DateTime::parse_from_rfc3339(&event.created_at)?.with_timezone(&chrono::Utc))
    } else {
        // Only the authenticated process attests these local receipts. No
        // database event, timestamp or restart guarantee is synthesized.
        verified_usage.clone_from(&evidence.usage);
        None
    };
    Ok(VerifiedOrdinaryWorkerCompletion {
        run_id: lineage.run_id.clone(),
        owner: owner.to_owned(),
        exit_confirmed: false,
        result,
        usage: verified_usage,
        finished_at,
    })
}

fn terminal_payload(event: &MessageEvent) -> anyhow::Result<serde_json::Value> {
    Ok(serde_json::from_str(
        event
            .raw_payload_json
            .as_deref()
            .context("ordinary terminal payload missing")?,
    )?)
}

fn verify_lineage(event: &MessageEvent, lineage: &CompletionLineage) -> anyhow::Result<()> {
    anyhow::ensure!(
        event.session_key.as_deref() == Some(lineage.session_key.as_str())
            && event.parent_run_id == lineage.parent_run_id
            && event.parent_session_key.as_deref() == Some(lineage.parent_session.as_str())
            && event.owner_id == lineage.owner_id
            && event.config_generation_id == lineage.config_generation_id
            && event.config_source_revision == lineage.config_source_revision
            && event.channel == lineage.channel
            && event.sender == lineage.sender
            && event.recipient == lineage.recipient
            && event.agent_id == lineage.agent_id
            && event.persona_id == lineage.persona_id
            && event.attempt_id == lineage.attempt_id
            && event.lease_epoch == lineage.lease_epoch,
        "ordinary terminal lineage differs from launch"
    );
    Ok(())
}

/// Verify a cleanup-ordered journal against a separately authenticated launch.
/// This does not attest process absence; callers must establish that separately.
pub(crate) async fn recover_completion(
    fabric: &MemoryFabric,
    worker: &WorkerManifest,
    generation: &crate::config::ConfigGeneration,
    transport_key: &ring::hmac::Key,
) -> anyhow::Result<Option<VerifiedOrdinaryWorkerCompletion>> {
    anyhow::ensure!(
        worker.ordinary_source.is_some() && worker.child_source.is_none(),
        "ordinary recovery requires an ordinary launch"
    );
    if !generation.effective.memory.event_recording_config().enabled {
        return Ok(None);
    }
    let (derived_key, lineage) = authenticate_launch(fabric, worker, generation, transport_key).await?;
    recover_journal(fabric, &lineage, &derived_key).await
}

async fn recover_journal(
    fabric: &MemoryFabric,
    lineage: &CompletionLineage,
    derived_key: &crate::decision::DecisionKey,
) -> anyhow::Result<Option<VerifiedOrdinaryWorkerCompletion>> {
    anyhow::ensure!(lineage.recording_enabled, "ordinary recovery requires recording");
    let owner = lineage.owner_id.as_deref().context("ordinary recovery owner missing")?;

    let key = format!("worker:{}:ordinary-completion", lineage.run_id);
    let Some(event) = fabric
        .memory()
        .find_owned_message_event_by_idempotency_key(fabric.workspace_id(), owner, &key)
        .await?
    else {
        return Ok(None);
    };
    anyhow::ensure!(
        event.id > 0
            && !event.event_id.is_empty()
            && event.workspace_id == fabric.workspace_id()
            && event.owner_id.as_deref() == Some(owner)
            && event.source == MessageEventSource::SessionWorker
            && event.role == "event"
            && event.event_type == "worker.ordinary.completion.confirmed"
            && event.run_id.as_deref() == Some(lineage.run_id.as_str())
            && event.idempotency_key.as_deref() == Some(key.as_str()),
        "ordinary completion journal differs from launch"
    );
    verify_lineage(&event, lineage)?;
    let result = serde_json::from_str(
        event
            .raw_payload_json
            .as_deref()
            .context("ordinary completion journal payload missing")?,
    )?;
    verify_completion(
        fabric,
        lineage,
        derived_key,
        result,
        b"ordinary-worker-completion-journal-v1",
    )
    .await
    .map(Some)
}

pub(super) async fn recover_restart_event(
    fabric: &MemoryFabric,
    workspace: &std::path::Path,
    event: MessageEvent,
) -> anyhow::Result<Option<RecoveredProcessRun>> {
    let owner = event.owner_id.as_deref().context("ordinary recovery owner missing")?;
    let run = event.run_id.as_deref().context("ordinary recovery run missing")?;
    let launch = launch::read(fabric, workspace, owner, run)
        .await?
        .context("ordinary recovery launch missing")?;
    anyhow::ensure!(
        serde_json::to_value(&event)? == serde_json::to_value(&launch.event)?,
        "ordinary discovery differs from verified launch"
    );
    if !launch.process_absent()? {
        return Ok(None);
    }
    let outcome = match launch.completion(fabric).await? {
        Some(completion) => RecoveredProcessOutcome::OrdinaryComplete(completion),
        None => RecoveredProcessOutcome::OrdinaryUnconfirmed(launch.partial_usage(fabric).await?),
    };
    let batch_id = launch.batch_id().map(str::to_owned);
    Ok(Some(RecoveredProcessRun {
        source: launch.input.unwrap_or(launch.event),
        batch_id,
        outcome,
    }))
}

/// Retained outside the sole monitor, so panic cannot discard journal lookup
/// identity. It carries no permission to signal, resume or launch a process.
pub(in crate::tools::sessions_spawn) struct OrdinaryProcessRecovery {
    fabric: MemoryFabric,
    workspace: std::path::PathBuf,
    owner: String,
    run: String,
}

impl OrdinaryProcessRecovery {
    pub(in crate::tools::sessions_spawn) fn new(
        fabric: MemoryFabric,
        workspace: std::path::PathBuf,
        owner: String,
        run: String,
    ) -> anyhow::Result<Self> {
        let workspace_matches = workspace.to_string_lossy() == fabric.workspace_id();
        anyhow::ensure!(
            !owner.is_empty() && !run.is_empty() && workspace_matches,
            "ordinary monitor recovery requires exact source identity"
        );
        Ok(Self {
            fabric,
            workspace,
            owner,
            run,
        })
    }

    /// Merge authenticated partial settlements without releasing a process
    /// slot or asserting completed cleanup. Validate the whole merge first.
    pub(in crate::tools::sessions_spawn) async fn retain_partial(
        &self,
        active_runs: &std::sync::Arc<tokio::sync::RwLock<Vec<crate::tools::sessions_spawn::SubAgentRun>>>,
    ) -> anyhow::Result<()> {
        let Some(launch) = launch::read(&self.fabric, &self.workspace, &self.owner, &self.run).await? else {
            return Ok(());
        };
        if !launch.process_absent()? {
            return Ok(());
        }
        let recovered = launch.partial_usage(&self.fabric).await?;
        let mut runs = active_runs.write().await;
        let run = runs
            .iter_mut()
            .find(|run| run.id == self.run)
            .context("ordinary recovery registry row missing")?;
        anyhow::ensure!(
            run.owner_id.as_deref() == Some(self.owner.as_str()),
            "ordinary partial recovery registry owner differs"
        );
        if run
            .process_control
            .as_ref()
            .and_then(|control| control.finalization())
            .is_some_and(|state| {
                matches!(
                    state,
                    crate::tools::sessions_spawn::ProcessFinalization::Natural
                        | crate::tools::sessions_spawn::ProcessFinalization::Terminated
                )
            })
        {
            return Ok(());
        }
        let mut merged = run.token_usage_records.clone();
        for usage in recovered.records {
            let id = usage
                .settlement_id
                .as_deref()
                .context("ordinary recovered settlement identity missing")?;
            if let Some(previous) = merged.iter().find(|known| known.settlement_id.as_deref() == Some(id)) {
                anyhow::ensure!(
                    *previous == usage,
                    "ordinary recovered settlement conflicts with registry"
                );
            } else {
                merged.push(usage);
            }
        }
        run.token_usage_records = merged;
        Ok(())
    }

    pub(in crate::tools::sessions_spawn) async fn recover(
        &self,
    ) -> anyhow::Result<Option<VerifiedOrdinaryWorkerCompletion>> {
        let Some(launch) = launch::read(&self.fabric, &self.workspace, &self.owner, &self.run).await? else {
            return Ok(None);
        };
        launch.completion(&self.fabric).await
    }
}

pub(in crate::tools::sessions_spawn) async fn recover_partial_usage(
    fabric: &MemoryFabric,
    worker: &WorkerManifest,
    generation: &crate::config::ConfigGeneration,
    transport_key: &ring::hmac::Key,
) -> anyhow::Result<RecoveredWorkerUsage> {
    let (key, lineage) = authenticate_launch(fabric, worker, generation, transport_key).await?;
    if !lineage.recording_enabled {
        return Ok(RecoveredWorkerUsage {
            records: Vec::new(),
            unfinished_segments: 0,
        });
    }
    segments::recover_usage(fabric, &lineage, &key).await
}

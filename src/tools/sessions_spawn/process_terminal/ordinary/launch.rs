//! Minimal ordinary launch provenance; contains no executable worker manifest.
use super::super::launch::process_identity;
use super::{
    CompletionLineage, Context, MemoryFabric, MessageEvent, MessageEventSource, RecoveredWorkerUsage,
    VerifiedOrdinaryWorkerCompletion, WorkerManifest, authenticate_launch, recover_journal, verify_lineage,
};
use crate::decision::key::KeyDomain;
use crate::memory::{MemoryVisibility, MessageEventScope};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchFields {
    version: u8,
    workspace: String,
    config_source: String,
    lineage: CompletionLineage,
    input_mac: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    batch_id: Option<String>,
    worker_pid: u32,
    process_context_mac: Option<[u8; 32]>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchRecord {
    fields: LaunchFields,
    seal: [u8; 32],
}

pub(in crate::tools::sessions_spawn) async fn persist(
    fabric: &MemoryFabric,
    worker: &WorkerManifest,
    generation: &crate::config::ConfigGeneration,
    transport_key: &ring::hmac::Key,
    pid: u32,
    batch_id: Option<&str>,
) -> anyhow::Result<()> {
    let (key, lineage) = authenticate_launch(fabric, worker, generation, transport_key).await?;
    if !lineage.recording_enabled {
        return Ok(());
    }
    anyhow::ensure!(pid > 0, "ordinary launch requires a positive process ID");
    let owner = lineage.owner_id.as_deref().context("ordinary launch owner missing")?;
    if let Some(batch) = batch_id {
        crate::tools::sessions_spawn::batch_journal::verify_ordinary_member(
            fabric,
            batch,
            owner,
            lineage
                .parent_run_id
                .as_deref()
                .context("ordinary batch parent run missing")?,
            &lineage.parent_session,
            &lineage.run_id,
        )
        .await?;
    }
    // Preserve the exact input MAC checked by authenticate_launch. Re-reading
    // and re-signing here could endorse a row changed since that verification.
    let input_mac = worker
        .ordinary_source
        .as_ref()
        .and_then(|source| source.input.as_ref())
        .map(|input| input.event_mac);
    let mut scope = MessageEventScope::new("session_worker", MemoryVisibility::Workspace)
        .with_owner_id(owner)
        .with_run_id(&lineage.run_id)
        .with_session_key(&lineage.session_key);
    scope.parent_run_id = lineage.parent_run_id.clone();
    scope.parent_session_key = Some(lineage.parent_session.clone());
    scope.config_generation_id = lineage.config_generation_id;
    scope.config_source_revision = lineage.config_source_revision.clone();
    scope.channel = lineage.channel.clone();
    scope.sender = lineage.sender.clone();
    scope.recipient = lineage.recipient.clone();
    scope.agent_id = lineage.agent_id.clone();
    scope.persona_id = lineage.persona_id.clone();
    scope.attempt_id = lineage.attempt_id.clone();
    scope.lease_epoch = lineage.lease_epoch;
    let lookup = format!("worker:{}:ordinary-launch", lineage.run_id);
    let fields = LaunchFields {
        version: 1,
        workspace: fabric.workspace_id().to_owned(),
        config_source: worker.config_generation.clone(),
        lineage,
        input_mac,
        worker_pid: pid,
        batch_id: batch_id.map(str::to_owned),
        process_context_mac: process_identity::context_mac(&key)?,
    };
    let seal = key.hmac(
        KeyDomain::StateFingerprint,
        &[b"ordinary-worker-recovery-launch-v1", &serde_json::to_vec(&fields)?],
    );
    let payload = serde_json::to_string(&LaunchRecord { fields, seal })?;
    let event = fabric
        .record_runtime_event_idempotent(
            scope.clone(),
            "worker.ordinary.recovery.registered",
            "ordinary worker recovery registered",
            Some(payload.clone()),
            lookup.clone(),
        )
        .await?;
    anyhow::ensure!(
        event.id > 0
            && !event.event_id.is_empty()
            && event.workspace_id == fabric.workspace_id()
            && event.owner_id == scope.owner_id
            && event.run_id == scope.run_id
            && event.session_key == scope.session_key
            && event.source == MessageEventSource::SessionWorker
            && event.role == "event"
            && event.visibility == MemoryVisibility::Workspace
            && event.event_type == "worker.ordinary.recovery.registered"
            && event.idempotency_key.as_deref() == Some(lookup.as_str())
            && event.raw_payload_json.as_deref() == Some(payload.as_str()),
        "ordinary launch receipt differs from provenance"
    );
    let stored = fabric
        .memory()
        .find_owned_message_event(
            fabric.workspace_id(),
            scope.owner_id.as_deref().context("ordinary launch owner missing")?,
            &event.event_id,
        )
        .await?
        .context("ordinary launch record is not durable")?;
    anyhow::ensure!(
        serde_json::to_value(&stored)? == serde_json::to_value(&event)?,
        "ordinary launch record differs from durable storage"
    );
    Ok(())
}

/// Authenticated read-only launch evidence; never usable to resume execution.
pub(super) struct VerifiedLaunch {
    pub(super) event: MessageEvent,
    pub(super) input: Option<MessageEvent>,
    fields: LaunchFields,
    key: std::sync::Arc<crate::decision::DecisionKey>,
}

impl VerifiedLaunch {
    pub(super) fn batch_id(&self) -> Option<&str> {
        self.fields.batch_id.as_deref()
    }
    pub(super) fn process_absent(&self) -> anyhow::Result<bool> {
        let Some(expected) = self.fields.process_context_mac else {
            return Ok(false);
        };
        if process_identity::context_mac(&self.key)? != Some(expected) {
            return Ok(false);
        }
        process_identity::absent(self.fields.worker_pid)
    }

    pub(super) async fn partial_usage(&self, fabric: &MemoryFabric) -> anyhow::Result<RecoveredWorkerUsage> {
        anyhow::ensure!(
            fabric.workspace_id() == self.fields.workspace && self.process_absent()?,
            "ordinary partial recovery requires exact workspace and absent process"
        );
        super::segments::recover_usage(fabric, &self.fields.lineage, &self.key).await
    }

    pub(super) async fn completion(
        &self,
        fabric: &MemoryFabric,
    ) -> anyhow::Result<Option<VerifiedOrdinaryWorkerCompletion>> {
        anyhow::ensure!(
            fabric.workspace_id() == self.fields.workspace,
            "ordinary recovery crossed workspace"
        );
        if !self.process_absent()? {
            return Ok(None);
        }
        // Absence alone is insufficient: the journal attests completed cleanup.
        let mut completion = recover_journal(fabric, &self.fields.lineage, &self.key).await?;
        if let Some(completion) = completion.as_mut() {
            // Verified same-context absence plus the cleanup-ordered journal
            // supplies exit evidence without pretending this owner reaped it.
            completion.exit_confirmed = true;
        }
        Ok(completion)
    }
}

pub(super) async fn read(
    fabric: &MemoryFabric,
    workspace: &std::path::Path,
    owner: &str,
    run: &str,
) -> anyhow::Result<Option<VerifiedLaunch>> {
    let workspace_matches = workspace.to_string_lossy() == fabric.workspace_id();
    anyhow::ensure!(
        !owner.is_empty() && !run.is_empty() && workspace_matches,
        "ordinary recovery requires exact workspace, owner and run"
    );
    let lookup = format!("worker:{run}:ordinary-launch");
    let Some(event) = fabric
        .memory()
        .find_owned_message_event_by_idempotency_key(fabric.workspace_id(), owner, &lookup)
        .await?
    else {
        return Ok(None);
    };
    let record: LaunchRecord = serde_json::from_str(
        event
            .raw_payload_json
            .as_deref()
            .context("ordinary launch payload missing")?,
    )?;
    let fields = record.fields;
    let transport = crate::tools::sessions_spawn::existing_session_worker_signing_key()?;
    let key = crate::decision::DecisionKey::from_worker_channel(
        &transport,
        fabric.workspace_id(),
        run,
        &fields.config_source,
    )?;
    anyhow::ensure!(
        key.verify_hmac(
            KeyDomain::StateFingerprint,
            &[b"ordinary-worker-recovery-launch-v1", &serde_json::to_vec(&fields)?],
            &record.seal
        ),
        "ordinary launch signature is invalid"
    );
    let lineage = &fields.lineage;
    anyhow::ensure!(
        fields.version == 1
            && fields.worker_pid > 0
            && fields.workspace == fabric.workspace_id()
            && !fields.config_source.is_empty()
            && lineage.run_id == run
            && lineage.owner_id.as_deref() == Some(owner)
            && !lineage.session_key.is_empty()
            && lineage.recording_enabled
            && lineage.input_event_id.is_some() == fields.input_mac.is_some()
            && event.id > 0
            && !event.event_id.is_empty()
            && event.workspace_id == fields.workspace
            && event.owner_id.as_deref() == Some(owner)
            && event.run_id.as_deref() == Some(run)
            && event.source == MessageEventSource::SessionWorker
            && event.role == "event"
            && event.visibility == MemoryVisibility::Workspace
            && event.event_type == "worker.ordinary.recovery.registered"
            && event.idempotency_key.as_deref() == Some(lookup.as_str()),
        "ordinary launch differs from signed provenance"
    );
    verify_lineage(&event, lineage)?;
    if let Some(batch) = &fields.batch_id {
        crate::tools::sessions_spawn::batch_journal::verify_ordinary_member(
            fabric,
            batch,
            owner,
            lineage
                .parent_run_id
                .as_deref()
                .context("ordinary batch parent run missing")?,
            &lineage.parent_session,
            run,
        )
        .await?;
    }
    let input = if let (Some(id), Some(mac)) = (&lineage.input_event_id, fields.input_mac) {
        let input = fabric
            .memory()
            .find_owned_message_event(fabric.workspace_id(), owner, id)
            .await?
            .context("ordinary recovery input missing")?;
        anyhow::ensure!(
            key.verify_hmac(
                KeyDomain::StateFingerprint,
                &[
                    b"worker-child-input-v1",
                    lineage.parent_scope_id.as_bytes(),
                    &serde_json::to_vec(&input)?
                ],
                &mac
            ) && input.id > 0
                && input.event_id == *id
                && input.workspace_id == fields.workspace
                && input.owner_id.as_deref() == Some(owner)
                && input.run_id.as_deref() == Some(run)
                && input.source == MessageEventSource::SessionsSpawn
                && input.role == "user"
                && input.visibility == MemoryVisibility::Workspace
                && input.event_type == "message.created",
            "ordinary recovery input differs from original signature"
        );
        verify_lineage(&input, lineage)?;
        Some(input)
    } else {
        None
    };
    Ok(Some(VerifiedLaunch {
        event,
        input,
        fields,
        key,
    }))
}

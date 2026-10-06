//! Privacy-safe worker completion journal, written only after runtime cleanup.
use super::protocol::WorkerResult;
use crate::memory::{MemoryFabric, MessageEventScope};
use crate::runtime::child_source::IssuedChildGrant;
use anyhow::Context;

pub(crate) async fn persist(
    fabric: &MemoryFabric,
    scope: MessageEventScope,
    grant: &IssuedChildGrant,
    result: &WorkerResult,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        result.ordinary_completion.is_none() && result.operator_completion.is_none(),
        "durable completion journal rejects ordinary evidence"
    );
    let mut projection = result.clone();
    projection.output = crate::agent::terminal::redact_persisted_text(&projection.output);
    projection.error = projection
        .error
        .as_deref()
        .map(crate::agent::terminal::redact_persisted_text);
    let mut evidence = projection
        .completion_evidence
        .take()
        .context("worker completion evidence missing")?;
    let (child, constraints) = grant.scope().gate_inputs();
    anyhow::ensure!(
        fabric.workspace_id() == child.authority.workspace
            && scope.owner_id.as_deref() == Some(child.authority.owner.as_str())
            && scope.run_id.as_deref() == Some(evidence.run_id.as_str()),
        "worker completion journal differs from issued scope"
    );
    evidence.validate_inventory(projection.success)?;
    // The durable representation carries redacted result text and its own MAC.
    // The stdout result retains the independently signed original representation.
    evidence.seal = constraints.key.hmac(
        crate::decision::key::KeyDomain::StateFingerprint,
        &[
            b"worker-completion-journal-v1",
            child.parent_decision_id.to_string().as_bytes(),
            &evidence.signing_bytes(&projection)?,
        ],
    );
    let key = format!("worker:{}:completion", evidence.run_id);
    projection.completion_evidence = Some(evidence);
    let payload = serde_json::to_string(&projection)?;
    let event = fabric
        .record_runtime_event_idempotent(
            scope.clone(),
            "worker.completion.confirmed",
            "worker completion confirmed",
            Some(payload.clone()),
            key.clone(),
        )
        .await?;
    anyhow::ensure!(
        event.id > 0
            && event.workspace_id == fabric.workspace_id()
            && event.owner_id == scope.owner_id
            && event.run_id == scope.run_id
            && event.session_key == scope.session_key
            && event.source == scope.source
            && event.event_type == "worker.completion.confirmed"
            && event.idempotency_key.as_deref() == Some(key.as_str())
            && event.raw_payload_json.as_deref() == Some(payload.as_str()),
        "worker completion journal receipt differs from projection"
    );
    let stored = fabric
        .memory()
        .find_owned_message_event(fabric.workspace_id(), &child.authority.owner, &event.event_id)
        .await?
        .context("worker completion journal is not durable")?;
    anyhow::ensure!(
        serde_json::to_value(&stored)? == serde_json::to_value(&event)?,
        "worker completion journal differs from durable record"
    );
    Ok(())
}

/// Ordinary completion uses its authenticated process source, never a fabricated
/// durable decision grant. The caller must finish runtime cleanup first.
pub(crate) async fn persist_ordinary(
    authority: &crate::runtime::child_source::ordinary_process::OrdinaryProcessAuthority,
    result: &WorkerResult,
) -> anyhow::Result<()> {
    authority.verify()?;
    let fabric = authority.source_fabric();
    let scope = authority.logical_scope();
    let recording = authority
        .snapshots()
        .generation
        .effective
        .memory
        .event_recording_config();
    anyhow::ensure!(
        result.completion_evidence.is_none() && result.operator_completion.is_none(),
        "ordinary journal rejects granted evidence"
    );
    let mut projection = result.clone();
    projection.output = crate::agent::terminal::redact_persisted_text(&projection.output);
    projection.error = projection
        .error
        .as_deref()
        .map(crate::agent::terminal::redact_persisted_text);
    let mut evidence = projection
        .ordinary_completion
        .take()
        .context("ordinary completion evidence missing")?;
    let (child, constraints) = authority.snapshots().scope.gate_inputs();
    anyhow::ensure!(
        evidence.run_id == authority.run_id()
            && evidence.process_id == authority.id().to_string()
            && evidence.input_event_id.as_deref() == authority.input().map(|input| input.event().event_id.as_str())
            && evidence.recording_enabled == recording.enabled
            && fabric.workspace_id() == child.authority.workspace
            && scope.owner_id.as_deref() == Some(child.authority.owner.as_str()),
        "ordinary journal differs from process authority"
    );
    evidence.validate_inventory(projection.success)?;
    if !recording.enabled {
        // Recording-off evidence remains in authenticated stdout only.
        return Ok(());
    }
    evidence.seal = constraints.key.hmac(
        crate::decision::key::KeyDomain::StateFingerprint,
        &[
            b"ordinary-worker-completion-journal-v1",
            child.parent_decision_id.to_string().as_bytes(),
            &evidence.signing_bytes(&projection)?,
        ],
    );
    let idempotency_key = format!("worker:{}:ordinary-completion", authority.run_id());
    projection.ordinary_completion = Some(evidence);
    let payload = serde_json::to_string(&projection)?;
    authority.verify()?;
    let event = fabric
        .record_runtime_event_idempotent(
            scope.clone(),
            "worker.ordinary.completion.confirmed",
            "ordinary worker completion confirmed",
            Some(payload.clone()),
            idempotency_key.clone(),
        )
        .await?;
    anyhow::ensure!(
        event.id > 0
            && !event.event_id.is_empty()
            && event.workspace_id == fabric.workspace_id()
            && event.owner_id == scope.owner_id
            && event.run_id == scope.run_id
            && event.session_key == scope.session_key
            && event.parent_run_id == scope.parent_run_id
            && event.source == scope.source
            && event.role == "event"
            && event.event_type == "worker.ordinary.completion.confirmed"
            && event.config_generation_id == scope.config_generation_id
            && event.config_source_revision == scope.config_source_revision
            && event.idempotency_key.as_deref() == Some(idempotency_key.as_str())
            && event.raw_payload_json.as_deref() == Some(payload.as_str()),
        "ordinary completion journal receipt differs from projection"
    );
    let stored = fabric
        .memory()
        .find_owned_message_event(fabric.workspace_id(), &child.authority.owner, &event.event_id)
        .await?
        .context("ordinary completion journal is not durable")?;
    anyhow::ensure!(
        serde_json::to_value(&stored)? == serde_json::to_value(&event)?,
        "ordinary completion journal differs from durable record"
    );
    authority.verify()?;
    Ok(())
}

/// Operator completion is bound to its signed publication and process lifetime.
pub(crate) async fn persist_operator(
    authority: &crate::runtime::child_source::operator_process::OperatorProcessAuthority,
    result: &WorkerResult,
) -> anyhow::Result<()> {
    authority.verify()?;
    let fabric = authority.source_fabric();
    let scope = authority.logical_scope();
    let recording = authority
        .snapshots()
        .generation
        .effective
        .memory
        .event_recording_config();
    anyhow::ensure!(
        result.completion_evidence.is_none() && result.ordinary_completion.is_none(),
        "operator journal rejects granted evidence"
    );
    let mut projection = result.clone();
    projection.output = crate::agent::terminal::redact_persisted_text(&projection.output);
    projection.error = projection
        .error
        .as_deref()
        .map(crate::agent::terminal::redact_persisted_text);
    let mut evidence = projection
        .operator_completion
        .take()
        .context("operator completion evidence missing")?;
    let publication = &authority.snapshots().publication;
    anyhow::ensure!(
        evidence.publication_id == publication.id().to_string()
            && evidence.run_id == authority.run_id()
            && evidence.process_id == authority.id().to_string()
            && evidence.input_event_id.as_deref() == authority.input().map(|input| input.event().event_id.as_str())
            && evidence.recording_enabled == recording.enabled
            && fabric.workspace_id() == publication.workspace()
            && scope.owner_id.as_deref() == Some(publication.owner()),
        "operator journal differs from process authority"
    );
    evidence.validate_inventory(projection.success)?;
    if !recording.enabled {
        // Recording-off evidence remains in authenticated stdout only.
        return Ok(());
    }
    evidence.seal = authority.snapshots().key.hmac(
        crate::decision::key::KeyDomain::StateFingerprint,
        &[
            b"operator-worker-completion-journal-v1",
            publication.id().to_string().as_bytes(),
            &evidence.signing_bytes(&projection)?,
        ],
    );
    let idempotency_key = format!("worker:{}:operator-completion", authority.run_id());
    projection.operator_completion = Some(evidence);
    let payload = serde_json::to_string(&projection)?;
    authority.verify()?;
    let event = fabric
        .record_runtime_event_idempotent(
            scope.clone(),
            "worker.operator.completion.confirmed",
            "operator worker completion confirmed",
            Some(payload.clone()),
            idempotency_key.clone(),
        )
        .await?;
    anyhow::ensure!(
        event.id > 0
            && !event.event_id.is_empty()
            && event.workspace_id == fabric.workspace_id()
            && event.owner_id == scope.owner_id
            && event.run_id == scope.run_id
            && event.session_key == scope.session_key
            && event.parent_run_id == scope.parent_run_id
            && event.source == scope.source
            && event.role == "event"
            && event.event_type == "worker.operator.completion.confirmed"
            && event.config_generation_id == scope.config_generation_id
            && event.config_source_revision == scope.config_source_revision
            && event.idempotency_key.as_deref() == Some(idempotency_key.as_str())
            && event.raw_payload_json.as_deref() == Some(payload.as_str()),
        "operator completion journal receipt differs from projection"
    );
    let stored = fabric
        .memory()
        .find_owned_message_event(fabric.workspace_id(), publication.owner(), &event.event_id)
        .await?
        .context("operator completion journal is not durable")?;
    anyhow::ensure!(
        serde_json::to_value(&stored)? == serde_json::to_value(&event)?,
        "operator completion journal differs from durable record"
    );
    authority.verify()?;
    Ok(())
}

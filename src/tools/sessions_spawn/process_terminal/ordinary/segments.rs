//! Verify partial settlements without claiming that interrupted calls were free.
use super::{
    CompletionLineage, Context, Digest, MemoryFabric, MessageEventSource, MessageEventSubject, RecoveredWorkerUsage,
    Sha256, read_terminal, terminal_payload, verify_lineage,
};
use crate::runtime::child_source::ordinary_process::journal::SegmentEvidence;

async fn read_evidence(
    fabric: &MemoryFabric,
    lineage: &CompletionLineage,
    key: &crate::decision::DecisionKey,
    index: u64,
    kind: &str,
) -> anyhow::Result<Option<SegmentEvidence>> {
    let owner = lineage.owner_id.as_deref().context("ordinary segment owner missing")?;
    let lookup = format!("worker:{}:segment:{index}:{kind}", lineage.run_id);
    let Some(event) = fabric
        .memory()
        .find_owned_message_event_by_idempotency_key(fabric.workspace_id(), owner, &lookup)
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
            && event.visibility == crate::memory::MemoryVisibility::Workspace
            && event.run_id.as_deref() == Some(lineage.run_id.as_str())
            && event.subject == Some(MessageEventSubject::Task(lineage.run_id.clone()))
            && event.event_type == format!("worker.ordinary.segment.{kind}")
            && event.idempotency_key.as_deref() == Some(lookup.as_str()),
        "ordinary segment journal identity differs"
    );
    verify_lineage(&event, lineage)?;
    let evidence: SegmentEvidence = serde_json::from_str(
        event
            .raw_payload_json
            .as_deref()
            .context("ordinary segment journal payload missing")?,
    )?;
    let process = uuid::Uuid::parse_str(&evidence.process_id)?;
    anyhow::ensure!(
        evidence.version == 1
            && evidence.task_run_id == lineage.run_id
            && evidence.index == index
            && !process.is_nil()
            && process.to_string() == evidence.process_id
            && key.verify_hmac(
                crate::decision::key::KeyDomain::StateFingerprint,
                &[
                    b"ordinary-worker-segment-v1",
                    kind.as_bytes(),
                    lineage.parent_scope_id.as_bytes(),
                    &evidence.signing_bytes()?
                ],
                &evidence.seal
            ),
        "ordinary segment journal signature or inventory differs"
    );
    anyhow::ensure!(
        match kind {
            "admission" => evidence.terminal.is_none(),
            "terminal" => evidence.input.is_none() && evidence.terminal.is_some(),
            _ => false,
        },
        "ordinary segment journal kind differs"
    );
    Ok(Some(evidence))
}

pub(super) async fn recover_usage(
    fabric: &MemoryFabric,
    lineage: &CompletionLineage,
    key: &crate::decision::DecisionKey,
) -> anyhow::Result<RecoveredWorkerUsage> {
    anyhow::ensure!(
        lineage.recording_enabled,
        "ordinary partial recovery requires recording"
    );
    let owner = lineage.owner_id.as_deref().context("ordinary segment owner missing")?;
    let mut recovered = RecoveredWorkerUsage {
        records: Vec::new(),
        unfinished_segments: 0,
    };
    let mut process_id: Option<String> = None;
    let mut index = 0_u64;
    loop {
        let Some(admission) = read_evidence(fabric, lineage, key, index, "admission").await? else {
            break;
        };
        if let Some(expected) = &process_id {
            anyhow::ensure!(
                *expected == admission.process_id,
                "ordinary recovery mixes process lifetimes"
            );
        } else {
            process_id = Some(admission.process_id.clone());
        }
        let run = format!("child-segment:{}:{index}", admission.process_id);
        if let Some(reference) = admission.input {
            let event = fabric
                .memory()
                .find_owned_message_event(fabric.workspace_id(), owner, &reference.event_id)
                .await?
                .context("ordinary admitted segment input missing")?;
            anyhow::ensure!(
                reference.terminal_id == run
                    && event.id > 0
                    && event.workspace_id == fabric.workspace_id()
                    && event.owner_id.as_deref() == Some(owner)
                    && event.event_id == reference.event_id
                    && event.run_id.as_deref() == Some(run.as_str())
                    && event.source == MessageEventSource::SessionWorker
                    && event.role == "user"
                    && event.event_type == "message.created"
                    && event.subject == Some(MessageEventSubject::Task(lineage.run_id.clone()))
                    && format!("{:x}", Sha256::digest(serde_json::to_vec(&event)?)) == reference.event_digest,
                "ordinary segment input differs from signed admission"
            );
            verify_lineage(&event, lineage)?;
        }
        if let Some(terminal) = read_evidence(fabric, lineage, key, index, "terminal").await? {
            anyhow::ensure!(
                terminal.process_id == admission.process_id,
                "ordinary terminal belongs to another process lifetime"
            );
            let reference = terminal.terminal.context("ordinary terminal reference missing")?;
            anyhow::ensure!(reference.terminal_id == run, "ordinary terminal index differs");
            let event = read_terminal(fabric, owner, &reference.event_id, &reference.event_digest, &run).await?;
            verify_lineage(&event, lineage)?;
            anyhow::ensure!(
                event.subject == Some(MessageEventSubject::Task(lineage.run_id.clone())),
                "ordinary terminal belongs to another task"
            );
            let payload = terminal_payload(&event)?;
            anyhow::ensure!(
                payload.get("terminal_id").and_then(serde_json::Value::as_str) == Some(run.as_str())
                    && payload.get("decision_id").is_some_and(serde_json::Value::is_null),
                "ordinary terminal asserts a different decision or segment"
            );
            let usage: Option<crate::llm::route_decision::MeteredTokenUsageRecord> = serde_json::from_value(
                payload
                    .get("usage_settlement")
                    .cloned()
                    .context("ordinary settlement field missing")?,
            )?;
            if let Some(usage) = usage {
                anyhow::ensure!(
                    usage.settlement_id.as_deref() == Some(run.as_str()),
                    "ordinary partial settlement identity differs"
                );
                recovered.records.push(usage);
            }
        } else {
            recovered.unfinished_segments += 1;
        }
        index = index
            .checked_add(1)
            .context("ordinary recovery segment index exhausted")?;
        tokio::task::yield_now().await;
    }
    // A missing later admission or zero unfinished visible segments never
    // establishes complete billing; only the final cleanup journal can do so.
    Ok(recovered)
}

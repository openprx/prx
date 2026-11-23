//! Restore actual operator command and task input from the owner-scoped backend.
use super::{Arc, ChildInputReceipt, Context, MemoryFabric, MemoryVisibility, MessageEventSource};

pub(crate) async fn restore_input(
    fabric: &MemoryFabric,
    worker: &crate::session_worker::protocol::WorkerManifest,
    snapshots: &crate::runtime::operator_spawn::process::OperatorProcessSnapshots,
) -> anyhow::Result<Option<Arc<ChildInputReceipt>>> {
    let source = worker
        .operator_source
        .as_ref()
        .context("operator worker source missing")?;
    let parent_source = &snapshots.parent;
    let recording = snapshots.generation.effective.memory.event_recording_config();
    anyhow::ensure!(
        worker.ordinary_source.is_none()
            && fabric.workspace_id() == snapshots.publication.workspace()
            && parent_source.workspace_id() == fabric.workspace_id()
            && parent_source.owner_id() == snapshots.publication.owner()
            && worker.owner_id.as_deref() == Some(parent_source.owner_id())
            && worker.parent_run_id.as_deref() == Some(parent_source.run_id())
            && worker.runtime_config_generation_id == Some(snapshots.generation.id.0)
            && worker.runtime_config_source_revision.as_deref()
                == Some(snapshots.generation.source_revision.fingerprint_sha256.as_str()),
        "operator worker input lookup differs from verified source"
    );
    anyhow::ensure!(
        parent_source.source_event_id().is_some() || !recording.enabled || !recording.record_user_messages,
        "operator worker original command receipt is missing"
    );
    if let Some(id) = parent_source.source_event_id() {
        let event = fabric
            .memory()
            .find_owned_message_event(fabric.workspace_id(), parent_source.owner_id(), id)
            .await?
            .context("operator worker parent event is unavailable")?;
        let mailbox = parent_source.mailbox_child_source_id();
        if let Some(message) = mailbox {
            anyhow::ensure!(
                event.source.as_str() == "prx_mailbox"
                    && event.session_key.as_deref() == Some(worker.session_scope_key.as_str())
                    && event.content == parent_source.routing_input(),
                "mailbox worker adopted input differs from signed source"
            );
            let config = std::sync::Arc::clone(&snapshots.generation.effective);
            let parent = parent_source.clone();
            let session = worker.session_scope_key.clone();
            let agent = worker.agent_id.clone().unwrap_or_else(|| "main".to_owned());
            let run = worker.run_id.clone();
            let adopted = uuid::Uuid::parse_str(id)?;
            let memory = fabric.memory();
            crate::runtime::blocking::spawn_blocking(move || {
                let runtime = crate::messaging::runtime::MessagingRuntime::for_config(&config)?;
                let target = runtime.address(parent.owner_id(), &session, &agent);
                runtime.store.validate_adopted_input(
                    parent.workspace_id(),
                    &target,
                    adopted,
                    parent.routing_input(),
                    message,
                )?;
                let child = memory
                    .child_session_store()
                    .context("mailbox child storage unavailable")?
                    .sessions(parent.workspace_id(), parent.owner_id())?
                    .into_iter()
                    .find(|child| child.session_key == session)
                    .context("mailbox child unavailable")?;
                anyhow::ensure!(
                    child.parent_session == parent.session_key()
                        && child.parent_run == parent.run_id()
                        && child.runs.last() == Some(&run)
                        && child.mode == "process",
                    "mailbox worker differs from persisted child lineage"
                );
                Ok::<_, anyhow::Error>(())
            })
            .await??;
        }
        anyhow::ensure!(
            event.id > 0
                && event.event_id == id
                && event.workspace_id == parent_source.workspace_id()
                && event.owner_id.as_deref() == Some(parent_source.owner_id())
                && (mailbox.is_some()
                    || (event.run_id.as_deref() == Some(parent_source.run_id())
                        && event.session_key.as_deref() == Some(parent_source.session_key())))
                && event.role == "user"
                && event.event_type == "message.created"
                && (mailbox.is_some()
                    || crate::runtime::turn_source::recorded_input_matches(
                        parent_source.surface(),
                        &event,
                        parent_source.routing_input(),
                    )),
            "operator worker parent event differs from signed provenance"
        );
    }
    let Some(input) = &source.input else {
        anyhow::ensure!(
            !recording.enabled || !recording.record_user_messages,
            "operator worker recorded input is missing"
        );
        return Ok(None);
    };
    anyhow::ensure!(!input.event_id.is_empty(), "operator worker input identity is empty");
    let event = fabric
        .memory()
        .find_owned_message_event(fabric.workspace_id(), parent_source.owner_id(), &input.event_id)
        .await?
        .context("operator worker input is unavailable")?;
    anyhow::ensure!(
        snapshots.key.verify_hmac(
            crate::security::runtime_key::KeyDomain::StateFingerprint,
            &[
                b"operator-worker-input-v1",
                snapshots.publication.id().to_string().as_bytes(),
                &serde_json::to_vec(&event)?
            ],
            &input.event_mac,
        ) && event.id > 0
            && event.event_id == input.event_id
            && event.workspace_id == fabric.workspace_id()
            && event.owner_id.as_deref() == Some(parent_source.owner_id())
            && event.source == MessageEventSource::SessionsSpawn
            && event.visibility == MemoryVisibility::Workspace
            && event.role == "user"
            && event.event_type == "message.created"
            && event.content == worker.task
            && event.run_id.as_deref() == Some(worker.run_id.as_str())
            && event.session_key.as_deref() == Some(worker.session_scope_key.as_str())
            && event.parent_run_id.as_deref() == Some(parent_source.run_id())
            && event.parent_session_key.as_deref() == Some(parent_source.session_key())
            && event.causation_event_id.as_deref() == parent_source.source_event_id()
            && event.run_id != event.parent_run_id
            && event.config_generation_id == worker.runtime_config_generation_id
            && event.config_source_revision == worker.runtime_config_source_revision
            && event.channel == worker.scope_channel
            && event.sender == worker.scope_sender
            && event.attempt_id == source.attempt_id
            && event.lease_epoch == source.lease_epoch
            && event.recipient == worker.scope_chat_id
            && event.agent_id == worker.agent_id
            && event.persona_id == worker.persona_id
            && event.subject == Some(crate::memory::MessageEventSubject::Task(worker.run_id.clone())),
        "operator worker input differs from authenticated lineage"
    );
    Ok(Some(Arc::new(ChildInputReceipt {
        previous_segment_input: None,
        segment_issued: false,
        memory: fabric.memory(),
        input: Arc::from(worker.task.as_str()),
        event,
        authority_guard: None,
    })))
}

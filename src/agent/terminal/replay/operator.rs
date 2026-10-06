//! Replay-only binding to an explicit local operator command.
use super::{
    Context, DecisionKey, DecisionSettlementRef, Deserialize, EventBinding, Memory, MessageEvent, SOURCE_DOMAIN,
    Serialize, event_binding, verify_event_mac,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReplayOperator {
    source: EventBinding,
    task_id: String,
}

fn verify_lineage(parent: &MessageEvent, child: &MessageEvent, task: &str) -> anyhow::Result<()> {
    let parent_session_matches = child.parent_session_key == parent.session_key;
    anyhow::ensure!(
        !task.is_empty()
            && parent.id > 0
            && parent.source == crate::memory::MessageEventSource::Chat
            && parent.channel.as_deref() == Some("terminal")
            && parent.sender.as_deref() == Some("local-user")
            && parent.visibility == crate::memory::MemoryVisibility::Workspace
            && parent.role == "user"
            && parent.event_type == "message.created"
            && parent.workspace_id == child.workspace_id
            && parent.owner_id == child.owner_id
            && parent.run_id.is_some()
            && parent.run_id == child.parent_run_id
            && parent.session_key.is_some()
            && parent_session_matches
            && parent.session_key == child.session_key
            && parent.run_id != child.run_id
            && child.causation_event_id.as_deref() == Some(parent.event_id.as_str())
            && child.channel == parent.channel
            && child.sender == parent.sender
            && child.config_generation_id == parent.config_generation_id
            && child.config_source_revision == parent.config_source_revision
            && child.subject == Some(crate::memory::MessageEventSubject::Task(task.to_owned()))
            && matches!(
                child.source,
                crate::memory::MessageEventSource::SessionsSpawn | crate::memory::MessageEventSource::SessionWorker
            ),
        "operator replay lineage differs from original command"
    );
    Ok(())
}

pub(super) async fn prepare(
    link: &DecisionSettlementRef,
    source: &MessageEvent,
    task: Option<&str>,
) -> anyhow::Result<Option<ReplayOperator>> {
    let Some(parent) = &link.operator_parent else {
        return Ok(None);
    };
    anyhow::ensure!(
        link.parent_grant.is_none(),
        "operator replay cannot also claim model delegation"
    );
    let event = link
        .memory
        .find_owned_message_event(
            parent.workspace_id(),
            parent.owner_id(),
            parent.source_event_id().context("operator original event missing")?,
        )
        .await?
        .context("operator original command is not durable")?;
    anyhow::ensure!(
        event.run_id.as_deref() == Some(parent.run_id())
            && event.session_key.as_deref() == Some(parent.session_key())
            && crate::runtime::turn_source::recorded_input_matches(parent.surface(), &event, parent.routing_input()),
        "operator replay command differs from issued source"
    );
    let task = task.context("operator replay task missing")?;
    verify_lineage(&event, source, task)?;
    Ok(Some(ReplayOperator {
        source: event_binding(
            &link
                .recorded
                .as_ref()
                .context("operator replay requires a recorded decision")?
                .key,
            SOURCE_DOMAIN,
            &event,
        )?,
        task_id: task.to_owned(),
    }))
}

pub(super) async fn verify(
    binding: &ReplayOperator,
    memory: &dyn Memory,
    key: &DecisionKey,
    workspace: &str,
    owner: &str,
    source: &MessageEvent,
) -> anyhow::Result<()> {
    let parent = memory
        .find_owned_message_event(workspace, owner, &binding.source.event_id)
        .await?
        .context("operator replay original command missing")?;
    verify_event_mac(key, SOURCE_DOMAIN, &parent, &binding.source.mac)?;
    verify_lineage(&parent, source, &binding.task_id)
}

//! Mailbox turns use the registered receiver's Console policy and provider.
use super::{
    AgentField, AppState, ConsolePendingInput, ConsoleTurnSource, MAX_PAGE_LIMIT, RuntimeEnvelope, TurnTarget,
    prepare_target, resolve_target, run_console_message_job_work,
};
use crate::messaging::runtime::{MessageInput, MessagingRuntime};
use std::sync::Arc;

pub(super) fn register(
    state: &AppState,
    mut envelope: RuntimeEnvelope,
    principal: super::super::ApiPrincipal,
) -> anyhow::Result<()> {
    let config = state.config.load_full();
    if config.memory.backend != "sqlite" {
        return Ok(());
    }
    if let Some(crate::memory::AgentPartition {
        key: crate::memory::PartitionKey::Named { name, .. },
        ..
    }) = &envelope.agent_partition
    {
        // Registration precedes target prompt binding. Use the already
        // admitted partition rather than defaulting the named root to Main.
        envelope.agent_id = Some(name.as_str().to_owned());
    }
    let runtime = MessagingRuntime::for_config(&config)?;
    crate::gateway::child_mailbox::register(state, &envelope)?;
    let address = runtime.address(
        &envelope.resolved_owner_id(),
        &envelope.session_key,
        envelope.agent_id.as_deref().unwrap_or("main"),
    );
    let state = state.clone();
    crate::messaging::receiver::register(
        runtime,
        address,
        Arc::new(move |input| {
            let state = state.clone();
            let envelope = envelope.clone();
            let principal = principal.clone();
            Box::pin(run(state, envelope, principal, input))
        }),
    )
}
/// Registers the receivers of owner sessions that hold adopted but unwoken
/// messages, so a restart wakes them without further input. Registration is
/// the existing path: its first poll queues the unwoken messages, and a later
/// submission to the same session finds the receiver registered and changes
/// nothing, so a message is never woken twice. A child session is reached
/// through its root, whose registration drives the children.
pub(in crate::gateway) async fn recover_startup(state: &AppState) {
    let config = state.config.load_full();
    if config.memory.backend != "sqlite" {
        return;
    }
    let Ok(runtime) = MessagingRuntime::for_config(&config) else {
        return;
    };
    let Ok(Ok(roots)) = crate::runtime::blocking::spawn_blocking(move || pending_roots(&runtime)).await else {
        tracing::warn!("session mailbox startup wake scan failed");
        return;
    };
    let workspace = config.workspace_dir.to_string_lossy().into_owned();
    for (owner, session, partition) in roots {
        let Ok(Some(summary)) = state.mem.get_conversation_session(&session).await else {
            continue;
        };
        let envelope = super::console_runtime_envelope_for_workspace(
            workspace.clone(),
            &session,
            &summary.channel,
            &summary.sender,
        )
        .with_owner_id(owner.clone())
        .with_owner_isolation()
        .with_agent_partition(partition);
        let principal = super::super::ApiPrincipal { owner_id: owner };
        match register(state, envelope, principal) {
            Ok(()) => tracing::info!("session mailbox startup wake registered a receiver"),
            Err(error) => {
                tracing::warn!(error=%crate::agent::diagnostics::error_class(&error),"session mailbox startup wake refused");
            }
        }
    }
}

/// Owner (`api:` ids only: the Console ingress) root sessions to register.
fn pending_roots(
    runtime: &MessagingRuntime,
) -> crate::messaging::mailbox::MailboxResult<Vec<(String, String, crate::memory::AgentPartition)>> {
    let workspace = &runtime.workspace;
    let mut roots: Vec<(String, String, crate::memory::AgentPartition)> = Vec::new();
    for (owner, session, _) in runtime.store.unwoken_sessions(workspace)? {
        if !owner.starts_with("api:") {
            continue;
        }
        let mut root = session;
        for _ in 0..8 {
            match runtime.store.parent_session(workspace, &owner, &root)? {
                Some(parent) => root = parent,
                None => break,
            }
        }
        if roots.iter().any(|(o, s, _)| *o == owner && *s == root) {
            continue;
        }
        let agent = runtime
            .store
            .logical_agent(workspace, &owner, &root)?
            .unwrap_or_else(|| "main".to_owned());
        let key = if agent == "main" {
            crate::memory::PartitionKey::Main
        } else {
            let address = runtime.address(&owner, &root, &agent);
            let scope = runtime.store.session_scope(workspace, &address, false)?;
            let Ok(name) = crate::config::agent_name::AgentName::parse(&agent) else {
                continue;
            };
            crate::memory::PartitionKey::Named {
                name,
                incarnation: scope.incarnation,
            }
        };
        let partition = crate::memory::AgentPartition {
            workspace_id: workspace.clone(),
            owner_id: owner.clone(),
            key,
        };
        roots.push((owner, root, partition));
    }
    Ok(roots)
}

async fn run(
    state: AppState,
    mut envelope: RuntimeEnvelope,
    principal: super::super::ApiPrincipal,
    input: MessageInput,
) -> anyhow::Result<()> {
    let parent = state
        .admit_turn_runtime()
        .map_err(|_| anyhow::anyhow!("receiver runtime unavailable"))?;
    let runtime = MessagingRuntime::for_config(&parent.config_generation.effective)?;
    input.verify(&runtime)?;
    anyhow::ensure!(
        input.message.to.owner == principal.owner_id && input.message.to.session == envelope.session_key,
        "mailbox receiver differs from registered Console owner"
    );
    let target = TurnTarget::from_field(AgentField::Text(input.message.to.agent.clone()))
        .map_err(|_| anyhow::anyhow!("receiver agent unavailable"))?;
    let plan = resolve_target(&parent.config_generation.effective, &target)
        .map_err(|_| anyhow::anyhow!("receiver agent unavailable"))?;
    let (turn_runtime, target) = if let Some(plan) = plan {
        let (runtime, target) =
            prepare_target(&parent, &plan).map_err(|_| anyhow::anyhow!("receiver provider unavailable"))?;
        (runtime, Some(target))
    } else {
        (parent, None)
    };
    envelope.run_id = Some(uuid::Uuid::new_v4().to_string());
    envelope.source_message_event_id = input.receipt.adopted_message_event_id.map(|id| id.to_string());
    envelope = envelope.with_config_generation(&turn_runtime.config_generation);
    // Only the verified adopted event supplies causation. Content never enters
    // the HTTP approval parser or creates an owner-review adapter.
    let _permit = crate::runtime::agent_admission::wait(
        &turn_runtime.config_generation.effective,
        &principal.owner_id,
        &input.message.to.agent,
    )
    .await?;
    // The adoption row is the message's only history row. The woken turn's own
    // input is not projected, and the request below carries the text once, so
    // the adoption turn leaves the loaded history. A missing adoption row
    // refuses the turn, as the child continuation does.
    let adopted = envelope
        .source_message_event_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("mailbox adoption row is missing"))?;
    let principal_memory = envelope.memory_principal();
    let mut turns = state
        .mem
        .list_console_conversation_turns(&principal_memory, &input.message.to.session, MAX_PAGE_LIMIT, 0)
        .await?;
    let before = turns.len();
    turns.retain(|turn| turn.message_event_id.as_deref() != Some(adopted.as_str()));
    anyhow::ensure!(turns.len() < before, "mailbox adoption row is missing from history");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let stop = cancellation.clone();
    let _message_turn = runtime.hold_turn(&input, Arc::new(move || stop.cancel()))?;
    let ticket = turn_runtime.publication_slot.cleanup.register(cancellation.clone())?;
    let pending = ConsolePendingInput {
        state: state.clone(),
        runtime: turn_runtime.clone(),
        principal,
        envelope,
        ticket,
        continuation: true,
    };
    let (proof, guard) = pending.record(input.text.clone(), None).await?;
    let source = ConsoleTurnSource::Admitted {
        proof,
        guard,
        owner_authenticated: false,
    };
    crate::gateway::jobs::scope_cancellation(
        cancellation,
        run_console_message_job_work(turn_runtime, state, input.text.clone(), turns, source, target),
    )
    .await
    .map_err(anyhow::Error::msg)?;
    drop(input);
    Ok(())
}

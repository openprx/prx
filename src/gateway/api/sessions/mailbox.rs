//! Mailbox turns use the registered receiver's Console policy and provider.
use super::{
    AgentField, AppState, ConsolePendingInput, ConsoleTurnSource, MAX_PAGE_LIMIT, RuntimeEnvelope, TurnTarget,
    console_owner_isolated, prepare_target, resolve_target, run_console_message_job_work,
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
        .with_owner_id(owner.clone());
        let isolated = startup_owner_isolated(state, &owner, &partition);
        tracing::debug!(isolated, "session mailbox startup wake isolation decided");
        let envelope = if isolated {
            envelope.with_owner_isolation()
        } else {
            envelope
        }
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

/// The isolation the owner's next submission to this session would get,
/// from the current pairing and config. It evaluates the submission's own
/// rule ([`console_owner_isolated`]) with the same inputs: an
/// owner-authenticated flag that is true only when pairing is required, a
/// paired token remains and the owner is the install owner, the target's
/// level and the controlled flag. An unauthenticated owner is isolated unless
/// the target runs at `Full`, exactly as on submission. Any failure (target
/// unresolved, runtime not admitted) yields isolation.
fn startup_owner_isolated(state: &AppState, owner: &str, partition: &crate::memory::AgentPartition) -> bool {
    let owner_authenticated = state.pairing.require_pairing()
        && state.pairing.is_paired()
        && state
            .pairing
            .install_owner()
            .is_some_and(|install| install.as_str() == owner);
    let Ok(runtime) = state.admit_turn_runtime() else {
        return true;
    };
    let target_level = match &partition.key {
        crate::memory::PartitionKey::Main => None,
        crate::memory::PartitionKey::Named { name, .. } => {
            match resolve_target(&runtime.config_generation.effective, &TurnTarget::Named(name.clone())) {
                Ok(Some(plan)) => Some(plan.autonomy),
                Ok(None) | Err(_) => return true,
            }
        }
    };
    console_owner_isolated(
        owner_authenticated,
        target_level,
        runtime.publication_slot.controlled.is_some(),
    )
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

tokio::task_local! {
    /// The woken run's own approval root, `{session, agent, incarnation, run_id}`.
    static WOKEN_TURN: serde_json::Value;
}

/// The approval root of the turn this module drives for an adopted mailbox
/// message, `None` outside such a turn. The woken run is its own root, scoped
/// to the receiving session: built here from the receiver address and the new
/// run id, never from the adopted event (which inherits an earlier run id).
pub(super) fn woken_scope() -> Option<serde_json::Value> {
    WOKEN_TURN.try_with(Clone::clone).ok()
}

/// The explicit approval root of a woken run owned by the receiver session.
async fn receiver_root(
    runtime: &Arc<MessagingRuntime>,
    input: &MessageInput,
    run_id: &str,
) -> anyhow::Result<serde_json::Value> {
    let address = input.message.to.clone();
    let store = Arc::clone(runtime);
    let scope = crate::runtime::blocking::spawn_blocking(move || {
        store.store.session_scope(&store.workspace, &address, false)
    })
    .await??;
    Ok(serde_json::json!({
        "session": input.message.to.session,
        "agent": input.message.to.agent,
        "incarnation": scope.incarnation.to_string(),
        "run_id": run_id,
    }))
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
    let run_id = uuid::Uuid::new_v4().to_string();
    let root = receiver_root(&runtime, &input, &run_id).await?;
    envelope.run_id = Some(run_id);
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
    let transcript = crate::context::transcript::Transcript::new(state.mem.as_ref(), envelope.memory_principal());
    let mut loaded = transcript
        .load_session_turns(&input.message.to.session, MAX_PAGE_LIMIT)
        .await?;
    // The check runs on every loaded row, before a handoff may cover the adoption row.
    let before = loaded.turns.len();
    loaded
        .turns
        .retain(|turn| turn.message_event_id.as_deref() != Some(adopted.as_str()));
    anyhow::ensure!(loaded.turns.len() < before, "mailbox adoption row is missing from history");
    let turns = transcript.model_turns_from(&input.message.to.session, loaded).await;
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
    WOKEN_TURN
        .scope(
            root,
            crate::gateway::jobs::scope_cancellation(
                cancellation,
                run_console_message_job_work(turn_runtime, state, input.text.clone(), turns, source, target),
            ),
        )
        .await
    .map_err(anyhow::Error::msg)?;
    drop(input);
    Ok(())
}

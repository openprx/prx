//! Mailbox turns use the registered receiver's Console policy and provider.
use super::{
    AgentField, AppState, ConsolePendingInput, ConsoleTurnSource, MAX_PAGE_LIMIT, RuntimeEnvelope, TurnTarget,
    prepare_target, resolve_target, run_console_message_job_work,
};
use crate::messaging::runtime::{MessageInput, MessagingRuntime};
use std::sync::Arc;

pub(super) fn register(
    state: &AppState,
    envelope: RuntimeEnvelope,
    principal: super::super::ApiPrincipal,
) -> anyhow::Result<()> {
    let config = state.config.load_full();
    if config.memory.backend != "sqlite" {
        return Ok(());
    }
    let runtime = MessagingRuntime::for_config(&config)?;
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
    let cancellation = tokio_util::sync::CancellationToken::new();
    let ticket = turn_runtime.publication_slot.cleanup.register(cancellation)?;
    let pending = ConsolePendingInput {
        state: state.clone(),
        runtime: turn_runtime.clone(),
        principal,
        envelope,
        ticket,
    };
    let (proof, guard) = pending.record(input.text.clone(), None).await?;
    let principal = proof.envelope().memory_principal();
    let turns = state
        .mem
        .list_console_conversation_turns(&principal, &input.message.to.session, MAX_PAGE_LIMIT, 0)
        .await?;
    let source = ConsoleTurnSource::Admitted {
        proof,
        guard,
        owner_authenticated: false,
    };
    run_console_message_job_work(turn_runtime, state, input.text.clone(), turns, source, target)
        .await
        .map_err(anyhow::Error::msg)?;
    drop(input);
    Ok(())
}

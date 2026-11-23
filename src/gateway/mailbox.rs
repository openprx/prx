//! Mailbox wakeups retain the actual gateway receiver's ingress identity.
use super::{AppState, GatewayFabricContext, TurnFailureClass, run_gateway_chat_with_multimodal};
use crate::messaging::runtime::{MessageInput, MessagingRuntime};
use std::sync::Arc;

tokio::task_local! { static INPUT: MessageInput; }

pub(super) fn causation(
    runtime: &MessagingRuntime,
    envelope: &crate::runtime::envelope::RuntimeEnvelope,
) -> anyhow::Result<Option<String>> {
    INPUT
        .try_with(|input| {
            input.verify(runtime)?;
            anyhow::ensure!(
                input.auto_start()
                    && input.message.to.owner == envelope.resolved_owner_id()
                    && input.message.to.session == envelope.session_key
                    && input.message.to.agent == envelope.agent_id.as_deref().unwrap_or("main"),
                "mailbox input differs from gateway receiver"
            );
            Ok(input.receipt.adopted_message_event_id.map(|id| id.to_string()))
        })
        .unwrap_or(Ok(None))
}

pub(super) fn register(
    state: &AppState,
    context: &GatewayFabricContext,
    envelope: &crate::runtime::envelope::RuntimeEnvelope,
) -> anyhow::Result<()> {
    let config = state.config.load_full();
    if config.memory.backend != "sqlite" {
        return Ok(());
    }
    let runtime = MessagingRuntime::for_config(&config)?;
    super::child_mailbox::register(state, envelope)?;
    let address = runtime.address(
        &envelope.resolved_owner_id(),
        &envelope.session_key,
        envelope.agent_id.as_deref().unwrap_or("main"),
    );
    let state = state.clone();
    let context = context.clone();
    crate::messaging::receiver::register(
        runtime,
        address,
        Arc::new(move |input| {
            let state = state.clone();
            let mut context = context.clone();
            // A message starts a fresh ingress; it never replays an HTTP request key.
            context.idempotency_key = None;
            Box::pin(async move {
                let config = state.config.load_full();
                let _permit =
                    crate::runtime::agent_admission::wait(&config, &input.message.to.owner, &input.message.to.agent)
                        .await?;
                let text = input.text.clone();
                let runtime = MessagingRuntime::for_config(&config)?;
                let cancellation = tokio_util::sync::CancellationToken::new();
                let stop = cancellation.clone();
                let _message_turn = runtime.hold_turn(&input, Arc::new(move || stop.cancel()))?;
                INPUT
                    .scope(
                        input,
                        super::jobs::scope_cancellation(cancellation, async {
                            let mut stage = TurnFailureClass::TurnNotReady;
                            // Normal gateway admission owns history, policy, costs and
                            // cleanup. Message data never enters the HTTP approval parser.
                            run_gateway_chat_with_multimodal(
                                &state,
                                crate::providers::selection::default_provider(&config),
                                &text,
                                &context,
                                None,
                                &mut stage,
                            )
                            .await
                            .map(|_| ())
                        }),
                    )
                    .await
            })
        }),
    )
}

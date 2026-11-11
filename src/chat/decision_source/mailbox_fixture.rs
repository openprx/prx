//! Test-mock ingress using the genuine recorded Chat/model source. It is
//! source issuance, readback and normal decision admission, never a fake grant.
use super::{ChatAdmissionMode, ChatProviderBinding, ChatSourceReceipt};
use crate::memory::MemoryFabric;
use crate::runtime::turn_publication::TurnDecisionPublication;
use crate::runtime::turn_source::TurnSourceProof as _;
use anyhow::{Context, Result};
use std::sync::Arc;

pub(crate) async fn spawn(
    publication: TurnDecisionPublication,
    provider: Arc<dyn crate::providers::Provider>,
    arguments: serde_json::Value,
) -> Result<crate::tools::ToolResult> {
    let generation = Arc::clone(&publication.generation);
    let memory = Arc::clone(&publication.memory);
    let config = &generation.effective;
    let selected = ChatProviderBinding::capture(&generation, &provider, "mock", "prx-mailbox-model")?
        .context("mailbox parent provider missing")?;
    let service = publication
        .publish(crate::causal_tree::state::TurnSurfaceKind::Interactive)
        .await?;
    crate::runtime::turn_cleanup::current_tracker()
        .context("mailbox parent cleanup owner missing")?
        .retain_service(Arc::clone(&service));
    let input = "Use sessions_spawn to run the mailbox worker";
    let session = "chat:prx-mailbox-parent";
    let run = uuid::Uuid::new_v4().to_string();
    let envelope = crate::chat::chat_runtime_envelope(&config.workspace_dir.to_string_lossy(), session)
        .with_config_generation(&generation)
        .with_run_id(run.clone());
    let fabric = MemoryFabric::new(Arc::clone(&memory), config.workspace_dir.to_string_lossy())
        .with_event_recording(config.memory.event_recording_config());
    let event = fabric
        .record_inbound_user_message(envelope.message_scope(), input, None, None)
        .await?;
    let receipt = ChatSourceReceipt::issue(memory.as_ref(), &generation, session, &run, input, &event).await?;
    let binding = receipt.bind_provider(
        &selected,
        ChatAdmissionMode::Model(crate::agent::turn_contracts::ChatMode::Edit),
    )?;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let context = binding.decide(&service, &cancellation).await?;
    let specs = service.agent_execution_specs(binding.as_ref(), context)?;
    anyhow::ensure!(
        specs.specs.iter().any(|spec| spec.name == "sessions_spawn"),
        "genuine mailbox parent admission did not select sessions_spawn"
    );
    let execution_context =
        crate::tools::ToolExecutionContext::new(binding.envelope().clone(), binding.policy_chat_type());
    let (execution, execution_context) =
        service.bind_agent_execution(binding.as_ref(), context, &execution_context, &specs.specs)?;
    let outcome = execution
        .execute(
            crate::tools::ToolExecutionCommand::new("sessions_spawn", arguments),
            execution_context,
            Some(cancellation),
        )
        .await;
    match (outcome.result, outcome.error) {
        (Some(result), _) => Ok(result),
        (None, Some(error)) => Err(anyhow::anyhow!(error)),
        (None, None) => anyhow::bail!("mailbox parent dispatch closed without a result"),
    }
}

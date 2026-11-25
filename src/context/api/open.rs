//! The opening of a turn's model request: the recall of the session's memory,
//! the stored attachment row of the user message, the replay of the earlier
//! turns with their tool rounds, and the fit of that history to the budget.

use std::sync::Arc;

use crate::agent::context_recall::{RECALL_CONTEXT_BUDGET, RecallScope, scoped_recall};
use crate::config::{AgentCompactionConfig, MemoryConfig};
use crate::context::request_provenance::{LiveAttachment, note_live_attachment};
use crate::context::transcript::{Attachment, Transcript, append_attachment};
use crate::memory::{ConversationTurn, Memory, MemoryFabric};
use crate::providers::ChatMessage;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::runtime::turn_context::OpenedTurn;
use crate::runtime::turn_publication::TurnRuntimeService;

/// What a turn opening reads, borrowed from the caller's turn state.
pub(crate) struct OpenInput<'a> {
    /// The memory the recall reads and the attachment row is written to.
    pub(crate) memory: &'a Arc<dyn Memory>,
    pub(crate) envelope: &'a RuntimeEnvelope,
    pub(crate) memory_config: &'a MemoryConfig,
    /// The chat type the recall scope is written with.
    pub(crate) chat_type: &'a str,
    /// The user's text, as the caller received it.
    pub(crate) visible_message: &'a str,
    /// Text appended after the redacted recall preamble in the attachment
    /// prefix. `None` leaves the prefix as the recall preamble alone.
    pub(crate) card: Option<&'a str>,
    /// The stored turns of the session, oldest first.
    pub(crate) previous_turns: Vec<ConversationTurn>,
    /// The system prompt of the turn, passed through untouched.
    pub(crate) system_prompt: String,
    /// The compaction of the turn is resolved from these, for the provider
    /// and model the turn serves.
    pub(crate) service: &'a TurnRuntimeService,
    pub(crate) context: &'a OpenedTurn,
    pub(crate) provider_label: &'a str,
    pub(crate) model: &'a str,
}

/// The opened turn: the history the model is sent, the stored event of each
/// history message, and the compaction config the history was fitted with.
pub(crate) struct TurnOpening {
    /// System prompt, replayed turns, then the request message.
    pub(crate) history: Vec<ChatMessage>,
    /// The stored event of each history message, index for index.
    pub(crate) history_event_ids: Vec<Option<String>>,
    /// The compaction config the turn runs with.
    pub(crate) compaction: AgentCompactionConfig,
}

/// Open a turn: recall, store the attachment row, replay the earlier turns,
/// fit the history to the budget and assemble it.
pub(crate) async fn open_turn(input: OpenInput<'_>) -> TurnOpening {
    let OpenInput {
        memory,
        envelope,
        memory_config,
        chat_type,
        visible_message,
        card,
        previous_turns,
        system_prompt,
        service,
        context,
        provider_label,
        model,
    } = input;

    let mem_context = scoped_recall(
        memory.as_ref(),
        &RecallScope::from_envelope(envelope, memory_config, chat_type),
        visible_message,
        RECALL_CONTEXT_BUDGET,
    )
    .await;
    // What is put around the message is stored beside it and replayed on later
    // turns, so the message keeps its bytes. It is stored (and sent) masked.
    let mut prefix = crate::security::redaction::redact_secrets(&mem_context.preamble);
    if let Some(card) = card {
        prefix.push_str(card);
    }
    let attachment = Attachment {
        prefix,
        tail: crate::agent::turn_contracts::current_time_tail(),
    };
    let enriched_message = attachment.compose(visible_message);
    let replay_fabric = MemoryFabric::new(Arc::clone(memory), envelope.workspace_id.clone())
        .with_event_recording(memory_config.event_recording_config());
    if let Some(user_event_id) = envelope.source_message_event_id.as_deref() {
        append_attachment(
            &replay_fabric,
            envelope.message_scope(),
            user_event_id,
            &attachment,
            &mem_context.sources,
        )
        .await;
    }
    let transcript = Transcript::new(memory.as_ref(), envelope.memory_principal());

    // OQ-23 / R5: budget on the *effective* window of the model this turn
    // serves (the operator/discovery/catalog model limits and
    // `reserved_output_tokens`), with the same arguments the CLI, TUI and
    // webhook resolve with, so one request is trimmed the same way on every
    // surface of this process.
    let mut effective_compaction = service.effective_compaction(provider_label, model);
    effective_compaction.config = context.main_compaction().clone();
    crate::providers::context_window::trace_effective_compaction_resolution(&effective_compaction);
    let system_message = ChatMessage::system(system_prompt);
    let request_message = crate::agent::turn_contracts::inbound_user_message(&enriched_message);
    // Earlier turns with their tool rounds; the oldest results become stubs
    // when the request would be over budget. The stored event of each history
    // message, index for index: none for the system prompt or a tool message,
    // each stored turn's event, then the request's own event.
    let mut replayed = transcript.expand_turns(previous_turns).await;
    note_live_attachment(LiveAttachment {
        run_id: envelope.run_id.clone(),
        prefix_chars: Some(attachment.prefix.chars().count()),
        tail_chars: Some(attachment.tail.chars().count()),
        attachments_replayed: Some(replayed.attachments),
        ..LiveAttachment::new(request_message.content.chars().count(), &mem_context.sources)
    });
    let stubbed = replayed.fit(&system_message, &request_message, &effective_compaction.config);
    if stubbed > 0 {
        tracing::info!(stubbed, "oldest replayed tool results were replaced by stubs to fit the request");
    }
    let history_event_ids: Vec<Option<String>> = std::iter::once(None)
        .chain(replayed.event_ids)
        .chain(std::iter::once(envelope.source_message_event_id.clone()))
        .collect();
    let mut history = Vec::with_capacity(replayed.messages.len() + 2);
    history.push(system_message);
    history.extend(replayed.messages);
    history.push(request_message);

    TurnOpening {
        history,
        history_event_ids,
        compaction: effective_compaction.config,
    }
}

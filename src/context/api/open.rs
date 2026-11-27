//! The opening of a turn's model request: the recall of the session's memory,
//! the stored attachment row of the user message, the replay of the earlier
//! turns with their tool rounds, and the fit of that history to the budget.

use std::sync::Arc;

use super::hook_points;
use super::prefix::{SessionPrefixInput, session_prefix};
use super::tail::{TurnTail, TurnTailInput, turn_tail};
use crate::agent::context_recall::{RECALL_CONTEXT_BUDGET, RecallScope, replayed_session_recall, scoped_recall};
use crate::config::{AgentCompactionConfig, MemoryConfig};
use crate::context::request_provenance::{LiveAttachment, note_live_attachment};
use crate::context::transcript::{Attachment, History, ReplayLog, Transcript, append_attachment};
use crate::hooks::HookManager;
use crate::memory::{ConversationTurn, Memory, MemoryFabric};
use crate::providers::ChatMessage;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::runtime::turn_context::OpenedTurn;
use crate::runtime::turn_publication::TurnRuntimeService;

/// Smallest recall budget of a replaying turn, in bytes. A window of 0 maps
/// here, because a budget of 0 switches the recall off.
const REPLAYED_RECALL_BUDGET_FLOOR: usize = 1_024;

/// The recall budget, in bytes, of every surface that replays the session's
/// own rows: half the window of the model, clamped to the floor and to the
/// context ceiling. The floor keeps a window of 0 from switching recall off.
pub(crate) fn replayed_recall_budget(max_context_tokens: usize) -> usize {
    (max_context_tokens / 2).clamp(REPLAYED_RECALL_BUDGET_FLOOR, RECALL_CONTEXT_BUDGET)
}

/// How the replayed tool results of the earlier turns are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplayMode {
    /// Keep the results in full; the oldest become stubs only when the request
    /// would be over its budget.
    FitToBudget,
    /// Replace every replayed result by its stub, then fit as usual.
    ClearAll,
}

/// Which recall of the session's memory a turn opening reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenRecall {
    /// The session's own rows are replayed as history, so the recall leaves
    /// its current-session block out.
    ReplayedSession,
    /// The turn is single shot and keeps no replayed history, so the recall
    /// carries the session's current-session block as its only session memory.
    WithSessionCopy,
}

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
    /// How the replayed tool results of `previous_turns` are shown.
    pub(crate) replay: ReplayMode,
    /// Which recall of the session's memory the opening reads.
    pub(crate) recall: OpenRecall,
    /// The memory key of the current input, which the recall leaves out. `None`
    /// keeps the key the recall scope was built with.
    pub(crate) current_input_key: Option<&'a str>,
    /// The session whose rows the recall's shared-events block leaves out: the
    /// sender of the message a woken turn adopted. `None` for every other turn.
    pub(crate) excluded_session: Option<&'a str>,
    /// The system prompt of the turn, passed through untouched.
    pub(crate) system_prompt: String,
    /// The surface's hook manager, for the session-open and turn-open context
    /// hooks. `None` runs no context hook (child turns).
    pub(crate) hooks: Option<&'a HookManager>,
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
    /// The memory ids the recall of the turn read, for the usefulness counts.
    pub(crate) recalled_ids: Vec<String>,
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
        replay,
        recall,
        current_input_key,
        excluded_session,
        system_prompt,
        hooks,
        service,
        context,
        provider_label,
        model,
    } = input;

    // OQ-23 / R5: budget on the *effective* window of the model this turn
    // serves (the operator/discovery/catalog model limits and
    // `reserved_output_tokens`), with the same arguments the CLI, TUI and
    // webhook resolve with, so one request is trimmed the same way on every
    // surface of this process.
    let mut effective_compaction = service.effective_compaction(provider_label, model);
    effective_compaction.config = context.main_compaction().clone();
    crate::providers::context_window::trace_effective_compaction_resolution(&effective_compaction);
    // The recall's shared part is at most half the window of this model; a
    // replaying turn leaves its own session rows out (they are replayed below).
    let recall_budget = replayed_recall_budget(effective_compaction.config.max_context_tokens);
    let scope = RecallScope::from_envelope(envelope, memory_config, chat_type)
        .with_excluded_session(excluded_session)
        .with_current_input_key(current_input_key.map(str::to_owned));
    let mem_context = match recall {
        OpenRecall::ReplayedSession => {
            replayed_session_recall(memory.as_ref(), &scope, visible_message, recall_budget).await
        }
        OpenRecall::WithSessionCopy => scoped_recall(memory.as_ref(), &scope, visible_message, recall_budget).await,
    };
    // What is put around the message is stored beside it. Only the tail is
    // replayed on later turns; the prefix is sent with this live message alone.
    // It is stored (and sent) masked, the task card included.
    let prefix = match card {
        Some(card) => crate::security::redaction::redact_secrets(&format!("{}{card}", mem_context.preamble)),
        None => crate::security::redaction::redact_secrets(&mem_context.preamble),
    };
    // K-H3: a child's first input takes the child-open contributions and runs
    // no turn-open or session-open hook; every other surface is unchanged.
    let child = hook_points::is_child_open(envelope.source);
    // The turn-open hooks and the session-open hooks are independent (the
    // first turn of a session runs both), so they run concurrently: the first
    // turn waits for the slower of the two, not for their sum. Each part has
    // its own place in the request (the attachment tail, the system prompt
    // prefix), so the output order does not depend on which finishes first.
    // The session's frozen memory index goes at the end of the system prompt
    // (the end of segment 4). Empty when the session has no index.
    let compact = service.publication_scope().generation.effective.agent.compact_context;
    let tail_future = async {
        if child {
            TurnTail {
                text: hook_points::child_open_tail(TurnTailInput {
                    memory,
                    envelope,
                    memory_config,
                    visible_message,
                    hooks,
                })
                .await,
                ..TurnTail::default()
            }
        } else {
            turn_tail(TurnTailInput {
                memory,
                envelope,
                memory_config,
                visible_message,
                hooks,
            })
            .await
        }
    };
    let prefix_future = session_prefix(SessionPrefixInput {
        memory,
        envelope,
        memory_config,
        compact,
        hooks: if child { None } else { hooks },
    });
    let (hook_tail, prefix_text) = futures::future::join(tail_future, prefix_future).await;
    let attachment = Attachment {
        prefix,
        tail: format!(
            "{}{}",
            hook_tail.text,
            crate::agent::turn_contracts::current_time_tail()
        ),
    };
    let enriched_message = attachment.compose(visible_message);
    let replay_fabric = MemoryFabric::new(Arc::clone(memory), envelope.workspace_id.clone())
        .with_event_recording(memory_config.event_recording_config());
    if let Some(user_event_id) = envelope.source_message_event_id.as_deref() {
        let mut sources = mem_context.sources.clone();
        sources.results.clone_from(&hook_tail.results);
        append_attachment(
            &replay_fabric,
            envelope.message_scope(),
            user_event_id,
            &attachment,
            &sources,
            &hook_tail.note,
        )
        .await;
    }
    let system_prompt = if prefix_text.is_empty() {
        system_prompt
    } else {
        format!("{system_prompt}\n\n{prefix_text}")
    };
    let system_message = ChatMessage::system(system_prompt);
    let request_message = crate::agent::turn_contracts::inbound_user_message(&enriched_message);
    // Earlier turns with their tool rounds; the oldest results become stubs
    // when the request would be over budget. The stored event of each history
    // message, index for index: none for the system prompt or a tool message,
    // each stored turn's event, then the request's own event.
    // A turn with no earlier turns replays nothing. `expand_turns` would still
    // read the session's replay rows first, so the read is skipped and the
    // empty history is the one `expand_turns` returns for zero turns.
    let mut replayed = if previous_turns.is_empty() {
        History::default()
    } else if previous_turns.iter().all(|turn| turn.message_event_id.is_none()) {
        // Cached turns (a channel's in-memory history) have no stored rows to
        // read; expanding them against an empty log gives the same history.
        ReplayLog::default().expand_turns(previous_turns)
    } else {
        Transcript::new(memory.as_ref(), envelope.memory_principal())
            .expand_turns(previous_turns)
            .await
    };
    if replay == ReplayMode::ClearAll {
        replayed.stub_all_results();
    }
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
        recalled_ids: mem_context.ids,
    }
}

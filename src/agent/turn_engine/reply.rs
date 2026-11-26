//! The terminal text branch of a turn round: retries, guards and the accepted reply.

use crate::hooks::{HookEvent, HookManager, payload_error};
use crate::llm::route_decision::TokenUsage;
use crate::multimodal;
use crate::observability::{Observer, ObserverEvent};
use crate::providers;
use crate::providers::{ChatMessage, ChatRequest, ProviderCapabilityError};
use crate::tools::Tool;
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use std::borrow::Cow;
use std::fmt::Write;
use std::io::Write as _;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use crate::agent::compaction::is_tool_roundtrip_companion;
use crate::agent::compaction::{
    COMPACTION_KEEP_RECENT_MESSAGES, CompactionBuild, build_configurable_compaction_patch_with_source_history,
    persist_context_switch_degradation,
};
#[cfg(test)]
use crate::agent::context_budget::budgeted_request_tool_tokens;
#[cfg(test)]
use crate::agent::context_budget::{APPENDED_SECTION_FRAMING_TOKENS, TOKEN_HEURISTIC_BUDGET_MULTIPLIER};
use crate::agent::context_budget::{
    PRE_TURN_FLUSH_THRESHOLD, RequestToolSurfaceCounts, budgeted_tokens, estimate_history_tokens,
    estimate_history_tokens_with_source, estimate_request_tokens, plan_context_budget, reserve_request_tool_surface,
    token_source_label, trim_history_to_context_budget,
    trim_history_to_context_budget_preserving_compaction_replacement_with_floor,
};
use crate::agent::context_paging::{
    context_mode_uses_os_paging, inject_recalled_context, plan_os_paging, recall_context_pages,
};
use crate::agent::context_preflight::{
    RestoredRequestPosition, capture_current_turn_request, restore_current_turn_request,
};
use crate::agent::diagnostics::{redact_sensitive_json_keys, scrub_credentials, tool_parse_log_digest};
#[cfg(test)]
use crate::agent::document_ingest::drop_untrusted_external_results;
use crate::agent::document_ingest::{persist_tool_output_document, persist_untrusted_external_copy};
use crate::agent::execution_policy::{AgentToolRuntimeInputs, build_agent_tool_runtime_adapter};
use crate::agent::history::{
    apply_aggressive_trim, build_native_assistant_history, coalesce_system_messages_at_front, latest_user_query,
};
use crate::agent::provider_round::{is_stream_failure_after_model_output, run_streaming_provider_turn};
#[cfg(test)]
use crate::agent::response_guard::{
    CORRECTIVE_ASIDE_PREFIX, contains_fabricated_tool_marker, strip_fabricated_tool_markers,
};
use crate::agent::response_guard::{
    EMPTY_ASSISTANT_RESPONSE_RETRY_MESSAGE, TOOL_SUMMARY_ONLY_RESPONSE_MESSAGE, UNEXECUTED_TOOL_CLAIM_NOTICE,
    append_corrective_aside, contains_assistant_fabricated_tool_marker, is_empty_assistant_response,
    is_tool_summary_only_response, strip_assistant_fabricated_tool_markers,
};
use crate::agent::sanitize::{known_tool_names, sanitize_stream_chunk};
use crate::agent::stream_buffer::StreamBoundaryBuffer;
use crate::agent::tool_call_parser::{
    looks_like_unparsed_tool_call_syntax, parse_structured_tool_calls, parse_tool_calls,
};
use crate::agent::tool_evidence::{claims_tool_evidence, strip_model_written_tool_evidence};
#[cfg(test)]
use crate::agent::tool_result_recording::INTERRUPTED_TOOL_RESULT;
use crate::agent::tool_result_recording::{
    ExecutedToolRecords, ToolResultRecorder, compact_tool_result_for_history_with_status,
    tool_result_inline_budget_for_history,
};
use crate::agent::tool_scheduler::{
    ReadOnlyToolScheduleConfig, execute_tools_with_service, shared_unrecoverable_tool_error,
};
#[cfg(test)]
use crate::agent::turn_contracts::ChatMode;
use crate::agent::turn_contracts::{
    ToolCallNotification, ToolLoopCancelled, ToolLoopEvent, ToolLoopOutcome, ToolLoopTrace, emit_tool_loop_event,
};
use crate::agent::turn_history::{TurnHistory, same_messages};
use crate::agent::turn_request::{ToolLoopMemory, TurnDecision, TurnDelivery, TurnRequest, TurnSchedule, TurnServices};
use crate::context::budget::anchor::{self, RequestBudget};
use crate::context::budget::record::{plan_round, settle_round};
use crate::context::reduce::clear::ClearOldResults;
use crate::context::reduce::compact::{PreTurnCompact, RoundCompact};
use crate::context::reduce::engine::{
    AfterTools, PreTurn, Round, Services, history_before_change, report_durable_compaction,
};
use crate::context::reduce::ladder;
use crate::context::reduce::normalize::repair_view;
use crate::context::reduce::preflight::PreflightCompaction;
use crate::context::reduce::trim::{MidTurnTrim, RoundTrim};
use crate::context::transcript::{RoundLog, call_id_for};
#[cfg(test)]
use crate::memory::{DocumentSearchResult, MemoryPrincipal};
#[cfg(test)]
use crate::providers::Provider;
#[cfg(test)]
use crate::providers::ToolCall;
#[cfg(all(test, feature = "wasm-plugins"))]
use std::path::Path;

use super::state::{TextReply, TurnInputs, TurnState};

/// The terminal text branch of one round: the corrective retries and the
/// guards that may send the round back, then the accepted reply, which is
/// stored in the history. The outbound middleware pass runs on it once.
pub(super) async fn finish_text_reply(
    turn: &TurnInputs<'_>,
    state: &mut TurnState,
    history: &mut Vec<ChatMessage>,
    response_text: &str,
    mut display_text: String,
    guarded_delivery: bool,
) -> Result<TextReply> {
    let TurnInputs {
        channel_name,
        provider_name,
        model,
        hooks,
        ref tools_registry,
        ref runtime_adapter,
        ref on_delta,
        ref cancellation_token,
        ..
    } = *turn;
    // No tool calls — this is the final response.
    if is_tool_summary_only_response(&response_text) {
        if state.tool_summary_retry_attempted {
            anyhow::bail!("model returned a tool-summary-only response after one corrective retry");
        }
        tracing::warn!(
            provider = %crate::security::redaction::redact_provider_text(provider_name),
            model = %crate::security::redaction::redact_provider_text(model),
            "model returned a tool-summary-only response; retrying once with a corrective instruction"
        );
        state.tool_summary_retry_attempted = true;
        state.inject_tool_summary_retry_instruction = true;
        return Ok(TextReply::Retry);
    }
    if is_empty_assistant_response(&response_text, false) {
        if state.empty_response_retry_attempted {
            anyhow::bail!("model returned an empty response after one corrective retry");
        }
        tracing::warn!(
            provider = %crate::security::redaction::redact_provider_text(provider_name),
            model = %crate::security::redaction::redact_provider_text(model),
            "model returned empty assistant response; retrying once with a corrective instruction"
        );
        state.empty_response_retry_attempted = true;
        state.inject_empty_response_retry_instruction = true;
        return Ok(TextReply::Retry);
    }
    // Fabricated tool-use marker guard. A `[Used tools: ...]` line the
    // model wrote while this turn never dispatched a tool is a claim
    // about work that did not happen; spend the one corrective retry on
    // it (the same budget the summary-only path uses, so a response that
    // is nothing but a marker still costs at most one retry).
    // A model-written tool-evidence line that names tools is the same
    // kind of claim.
    let fabricated_marker = contains_assistant_fabricated_tool_marker(&response_text)
        || contains_assistant_fabricated_tool_marker(&display_text)
        || claims_tool_evidence(&response_text)
        || claims_tool_evidence(&display_text);
    if fabricated_marker && !state.executed_any_tool_call && !state.tool_summary_retry_attempted {
        tracing::warn!(
            provider = %crate::security::redaction::redact_provider_text(provider_name),
            model = %crate::security::redaction::redact_provider_text(model),
            "model claimed tool use without executing any tool; retrying once with a corrective instruction"
        );
        state.tool_summary_retry_attempted = true;
        state.inject_tool_summary_retry_instruction = true;
        return Ok(TextReply::Retry);
    }
    if fabricated_marker {
        if state.executed_any_tool_call {
            // A tool really ran: keep whatever the model actually
            // answered and drop only the marker lines, so the
            // fabrication never reaches the user, the history, or the
            // next turn's prompt where it would be imitated again.
            if let Cow::Owned(cleaned) = strip_assistant_fabricated_tool_markers(&display_text) {
                display_text = cleaned;
            }
            if display_text.trim().is_empty() {
                anyhow::bail!("model returned a tool-summary-only response after one corrective retry");
            }
        } else {
            // Retry spent and no tool ran: the rest of the reply is
            // built on a claim that did not happen, so it is replaced
            // whole by a fixed notice. `display_text` is what is
            // streamed, pushed to history and returned, so all three
            // carry the notice and none the fabricated text.
            display_text = UNEXECUTED_TOOL_CLAIM_NOTICE.to_owned();
        }
    }
    // Tool evidence is written by the runtime only: every evidence
    // line the model wrote is dropped before delivery and history,
    // a `none` line included.
    if let Cow::Owned(cleaned) = strip_model_written_tool_evidence(&display_text) {
        display_text = cleaned;
        if display_text.trim().is_empty() {
            anyhow::bail!("model returned a tool-summary-only response after one corrective retry");
        }
    }
    // One outbound middleware pass per accepted, delivered turn. Every
    // retry, repair and marker guard above has accepted this response,
    // and nothing below can replace it: the next statements stream it,
    // store it and return it. Running the stage earlier — once per loop
    // iteration, as it used to — executed the plugin again for every
    // tool round and every corrective retry, so a plugin holding a
    // counter, an audit log, a rate budget or any other side effect saw
    // one delivered turn as several. A tool-call iteration keeps the
    // pre-outbound `assistant_history_content` it always had.
    //
    // `has_tool_calls` is necessarily `false` here; it is kept in the
    // envelope so existing plugins that read the field still parse it.
    #[cfg(feature = "wasm-plugins")]
    super::apply_outbound_middleware(hooks, channel_name, &mut display_text).await;
    // If a streaming sender is provided, relay the text in small chunks
    // so the channel can progressively update the draft message.
    if runtime_adapter.stream_provider && guarded_delivery {
        emit_tool_loop_event(&runtime_adapter, ToolLoopEvent::TextDelta(display_text.clone())).await?;
    }
    if let Some(tx) = on_delta {
        // Semantic-boundary chunking: only flush after closed XML tags
        // (`<...>`) or closed JSON objects/arrays. See
        // `stream_buffer::StreamBoundaryBuffer` for the state machine.
        let mut sbuf = StreamBoundaryBuffer::new();
        let mut receiver_alive = true;
        // Precompute the known-tool-name set once. The scrubber needs
        // it to decide whether an isolated `{"name":...}` JSON object
        // really is a tool call. Reused for every flushed chunk on
        // the hot path; rebuilding it per word would be wasteful.
        let tool_names = known_tool_names(tools_registry.as_ref());
        // Feed deltas roughly word-sized so progressive flushes still happen
        // inside long plain-text runs.
        for word in display_text.split_inclusive(char::is_whitespace) {
            if cancellation_token.as_ref().is_some_and(CancellationToken::is_cancelled) {
                return Err(ToolLoopCancelled.into());
            }
            if let Some(chunk) = sbuf.push(word) {
                // Sanitize the chunk *before* it enters the channel
                // mpsc::Sender so any tool-call XML/JSON artifact is
                // removed at the source instead of being painted on
                // screen and replaced after the response completes
                // (which would flicker — see P0-3 in the chat-mode
                // fix plan, 2026-05-12).
                let cleaned = sanitize_stream_chunk(&chunk, &tool_names);
                if cleaned.is_empty() {
                    continue;
                }
                if tx.send(cleaned.into_owned()).await.is_err() {
                    receiver_alive = false;
                    break; // receiver dropped
                }
            }
        }
        if receiver_alive && let Some(tail) = sbuf.flush_all() {
            let cleaned = sanitize_stream_chunk(&tail, &tool_names);
            if !cleaned.is_empty() {
                let _ = tx.send(cleaned.into_owned()).await;
            }
        }
    }
    history.push(ChatMessage::assistant(display_text.clone()));
    Ok(TextReply::Deliver(display_text))
}

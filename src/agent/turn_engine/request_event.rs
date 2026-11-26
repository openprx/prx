//! The request-context event of one round: the sizes and provenance of the
//! prepared request, recorded before the provider call.

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

use super::state::{PreparedRound, RoundBudget, RoundSurface, TurnInputs, TurnState};

/// Records the request context event of one round when something reads it:
/// the durable event, or an enabled prompt subscriber.
pub(super) async fn record_request_context(
    turn: &TurnInputs<'_>,
    state: &TurnState,
    surface: &RoundSurface<'_>,
    budget: &RoundBudget,
    round: &PreparedRound,
) {
    let TurnInputs {
        compaction_config,
        ref event_fabric,
        ref request_event_scope,
        provider_name,
        model,
        ..
    } = *turn;
    let tool_surface_counts = &surface.tool_surface_counts;
    let tool_snapshot = round.compiled_tool_context.fingerprint();
    // How much of this request is prompt and how much is native tool
    // schema. Sizes, counts and estimates only: this is emitted into a
    // durable event, so no schema body, prompt body or tool argument may
    // appear here.
    //
    // One estimator, two inputs. The preflight measured `history`; this
    // measures `prepared_messages` after the multimodal pass, the
    // corrective asides, the middleware, the appended tool instructions
    // and the system coalescing, so the two totals are systematically
    // different numbers for the same iteration. Both are reported —
    // `budgeted_*` is what the window decision was made on, the rest is
    // what actually leaves — so a disagreement is an observable delta
    // instead of an invariant nobody can check.
    //
    // Measured only when something will read it: the durable event, or an
    // enabled subscriber on this target. Otherwise the turn would pay a
    // second full tokenizer pass over the prompt, on every iteration, for
    // a number nobody consumes.
    if event_fabric.is_some() || tracing::enabled!(target: "openprx::prompt", tracing::Level::DEBUG) {
        let budgeted_messages_tokens: Option<usize> = budget.budgeted_totals.map(|(messages, _)| messages);
        let budgeted_request_tokens: Option<usize> = budget.budgeted_totals.map(|(_, request)| request);
        let messages_bytes: usize = round.prepared_messages.messages.iter().fold(0usize, |total, message| {
            total
                .saturating_add(message.role.len())
                .saturating_add(message.content.len())
        });
        let messages_chars: usize = round.prepared_messages.messages.iter().fold(0usize, |total, message| {
            total
                .saturating_add(message.role.chars().count())
                .saturating_add(message.content.chars().count())
        });
        let messages_measurement = estimate_history_tokens_with_source(&round.prepared_messages.messages);
        let messages_tokens = budgeted_tokens(messages_measurement);
        let native_tool_tokens = budgeted_tokens(round.tool_surface.native_payload.measurement);
        let instruction_tokens = budgeted_tokens(round.tool_surface.instructions);
        let request_tokens =
            estimate_request_tokens(messages_measurement, round.tool_surface.native_payload.measurement);
        let token_source: &'static str = token_source_label(messages_measurement.source);
        tracing::debug!(
            target: "openprx::prompt",
            system_prompt_sha256 = round.system_prompt_sha256.as_deref().unwrap_or("none"),
            round.system_prompt_chars,
            tool_snapshot_sha256 = %tool_snapshot.sha256,
            tool_count = tool_snapshot.count,
            tool_snapshot_render_hits = state.tool_snapshot_render_cache.hits(),
            tool_snapshot_render_misses = state.tool_snapshot_render_cache.misses(),
            messages_bytes,
            messages_chars,
            messages_tokens,
            native_tool_bytes = round.tool_surface.native_payload.bytes,
            native_tool_tokens,
            instruction_tokens,
            budgeted_messages_tokens,
            budgeted_request_tokens,
            request_tokens,
            token_source,
            "final request system prompt compiled"
        );
        // Compilation is completed work, including a cold tokenizer or a
        // changed tool surface. Record it before the durable event yields;
        // an iteration starting is not a completed provider response.
        crate::agent::idle::beat(crate::agent::idle::ProgressKind::RequestPrepared);

        if let Some(fabric) = event_fabric.as_ref() {
            let request_context_payload = serde_json::json!({
                "schema_version": 1,
                "iteration": state.iteration,
                // Durable: a `custom:` URL may carry `user:password@`.
                "provider": providers::redact_endpoint_userinfo(provider_name),
                "model": model,
                "messages_count": round.prepared_messages.messages.len(),
                "system_prompt": {
                    "sha256": round.system_prompt_sha256.as_deref(),
                    "chars": round.system_prompt_chars,
                },
                "tool_snapshot": &tool_snapshot,
                "tool_surface": tool_surface_counts.to_json(),
                "tool_snapshot_render": {
                    "hits": state.tool_snapshot_render_cache.hits(),
                    "misses": state.tool_snapshot_render_cache.misses(),
                },
                "budget": budget.request_budget
                    .as_ref()
                    .zip(compaction_config)
                    .map(|(planned, config)| planned.to_json(config, budgeted_request_tokens)),
                "request_estimate": {
                    "messages_bytes": messages_bytes,
                    "messages_chars": messages_chars,
                    "messages_tokens": messages_tokens,
                    "native_tool_bytes": round.tool_surface.native_payload.bytes,
                    "native_tool_tokens": native_tool_tokens,
                    "instruction_tokens": instruction_tokens,
                    "budgeted_messages_tokens": budgeted_messages_tokens,
                    "budgeted_total_tokens": budgeted_request_tokens,
                    "total_tokens": request_tokens,
                    "token_source": token_source_label(messages_measurement.source),
                },
                "provenance": crate::context::request_provenance::request_provenance(
                    &crate::context::request_provenance::RequestFacts {
                        messages: &round.prepared_messages.messages,
                        run_id: request_event_scope
                            .as_ref()
                            .and_then(|scope| scope.run_id.as_deref())
                            .or(round.execution_adapter.tool_execution_context.envelope.run_id.as_deref()),
                        system_sha256: round.system_prompt_sha256.as_deref(),
                        system_chars: round.system_prompt_chars,
                        tool_surface_sha256: &tool_snapshot.sha256,
                        tools_published: tool_snapshot.count,
                        tools_deferred: tool_surface_counts.deferred_roots(),
                    },
                ),
            });
            let scope = request_event_scope
                .clone()
                .unwrap_or_else(|| round.execution_adapter.tool_execution_context.envelope.message_scope());
            let scope = scope.with_recipient(format!("{provider_name}/{model}"));
            let content = format!(
                "system_prompt_sha256={} tool_snapshot_sha256={} iteration={iteration}",
                round.system_prompt_sha256.as_deref().unwrap_or("none"),
                tool_snapshot.sha256,
                iteration = state.iteration
            );
            if let Err(error) = fabric
                .record_runtime_event(
                    scope,
                    "llm.request.context.compiled",
                    content,
                    Some(request_context_payload.to_string()),
                )
                .await
            {
                let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
                tracing::warn!(
                    error_len,
                    error_sha256,
                    "Failed to persist compiled LLM request context"
                );
            }
        }
    } else {
        crate::agent::idle::beat(crate::agent::idle::ProgressKind::RequestPrepared);
    }
}

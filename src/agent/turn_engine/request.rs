//! Request preparation of one round: the multimodal and corrective passes,
//! the request middleware, the compiled tool context and the request
//! provenance the provider call sends.

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

use super::state::{PreparedRound, RoundSurface, TurnInputs, TurnState};

/// Prepares the messages and tool surface one round sends to the provider.
pub(super) async fn prepare_request(
    turn: &TurnInputs<'_>,
    state: &mut TurnState,
    surface: &mut RoundSurface<'_>,
    history: &[ChatMessage],
    context_lookup_required: bool,
) -> Result<PreparedRound> {
    let TurnInputs {
        ref advice,
        chat_mode,
        expose_stay_silent,
        multimodal_config,
        service,
        context,
        proof,
        provider,
        provider_name,
        model,
        temperature,
        hooks,
        ref runtime_adapter,
        ref mode_capabilities,
        ref safety_surface,
        ..
    } = *turn;
    let mut tool_specs = std::mem::take(&mut surface.tool_specs);
    let appended_names = &surface.appended_names;
    let decision_fingerprints = &surface.decision_fingerprints;
    let mut tool_surface = surface.tool_surface;
    let image_marker_count = multimodal::count_image_markers(history);
    if image_marker_count > 0 && !mode_capabilities.vision {
        return Err(ProviderCapabilityError {
            provider: providers::redact_endpoint_userinfo(provider_name).into_owned(),
            capability: "vision".to_string(),
            message: format!(
                "received {image_marker_count} image marker(s), but this provider does not support vision input"
            ),
        }
        .into());
    }

    let media_artifacts = hooks.media_artifacts();
    // Media sources load at the turn's effective level (a child turn's
    // sealed level, else the service's): at Full they are not confined
    // to the workspace.
    let media_level = crate::runtime::turn_source::TurnSourceProof::child_scope(proof).map_or_else(
        || service.security().autonomy,
        |scope| scope.gate_inputs().0.authority.level,
    );
    let mut prepared_messages = crate::tools::effective_autonomy::scope(
        media_level,
        crate::tools::effective_autonomy::scope_evaluation_paths(
            service.security(),
            multimodal::prepare_messages_for_provider(history, multimodal_config, media_artifacts.as_ref()),
        ),
    )
    .await?;
    // Corrective instructions are appended at the tail, not as request-local
    // `system` context. A system message here is folded into the leading
    // one by `coalesce_system_messages_at_front` below, which appends bytes
    // to the system prefix and misses the provider-side prefix cache for
    // the whole conversation — on the one turn that is already paying for a
    // wasted provider call. This keeps the leading system policy unchanged
    // and the "exactly one system message, at index 0" shape that strict
    // chat templates require.
    //
    // Guarded delivery: an interactive chat turn with tools buffers the
    // provider stream so response guards and outbound middleware run before
    // any text renders; the rendered text then equals the persisted reply.
    let guarded_delivery = runtime_adapter.guarded_delivery && !chat_mode.intercepts_writes() && !tool_specs.is_empty();
    if state.inject_tool_summary_retry_instruction {
        append_corrective_aside(&mut prepared_messages.messages, TOOL_SUMMARY_ONLY_RESPONSE_MESSAGE);
        state.inject_tool_summary_retry_instruction = false;
    }
    if state.inject_empty_response_retry_instruction {
        append_corrective_aside(&mut prepared_messages.messages, EMPTY_ASSISTANT_RESPONSE_RETRY_MESSAGE);
        state.inject_empty_response_retry_instruction = false;
    }
    let required_lookup_spec = context_lookup_required
        .then(|| {
            tool_specs
                .iter()
                .find(|spec| spec.name == crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME)
                .cloned()
        })
        .flatten();

    #[cfg(feature = "wasm-plugins")]
    let mut tool_specs_rewritten = false;
    #[cfg(feature = "wasm-plugins")]
    {
        let transformed = hooks
            .process_wasm_middleware(
                crate::plugins::capabilities::middleware::MiddlewareStage::LlmRequest,
                serde_json::json!({
                    "provider": provider_name,
                    "model": model,
                    "temperature": temperature,
                    "messages": prepared_messages.messages,
                    "tools": tool_specs,
                }),
            )
            .await;
        if let Some(messages) = transformed.get("messages")
            && let Ok(candidate) = serde_json::from_value::<Vec<ChatMessage>>(messages.clone())
            && !candidate.is_empty()
            && candidate.iter().all(|message| !message.role.trim().is_empty())
        {
            prepared_messages.messages = candidate;
        }
        if let Some(tools) = transformed.get("tools")
            && let Ok(candidate) = serde_json::from_value::<Vec<crate::tools::ToolSpec>>(tools.clone())
        {
            tool_specs = candidate;
            tool_specs_rewritten = true;
        }
    }
    // Middleware may narrow or reshape schemas, but it must not bypass the
    // entrypoint exposure boundary. Reapply the gate to the final snapshot
    // that drives both prompt/native advertisement and execution.
    crate::tools::filter_tool_specs_for_exposure(&mut tool_specs, expose_stay_silent);
    super::pin_required_lookup(&mut tool_specs, required_lookup_spec);
    if !tool_specs.iter().any(|spec| spec.name == "skill_read")
        && prepared_messages
            .messages
            .iter()
            .any(|message| message.role == "system" && message.content.contains("<available_skills>"))
    {
        crate::agent::runtime_prompt::suppress_unreadable_skill_metadata(&mut prepared_messages.messages);
        tracing::warn!("skill metadata suppressed because the final tool surface has no skill_read");
    }
    {
        let fingerprints = &decision_fingerprints;
        anyhow::ensure!(
            tool_specs
                .iter()
                .all(|spec| fingerprints.get(&spec.name).is_some_and(|expected| *expected
                    == crate::tools::capability::fingerprint::tool_spec_fingerprint(
                        &spec.name,
                        &spec.description,
                        &spec.parameters
                    ))),
            "request middleware changed the decision-constrained tool surface"
        );
    }
    // Middleware rewrote the published set after it was budgeted, so this
    // iteration's request accounting is re-measured against what will
    // actually be sent. Only the rewrite pays for it.
    #[cfg(feature = "wasm-plugins")]
    if tool_specs_rewritten {
        tool_surface = state
            .request_tool_surface_cache
            .measure(mode_capabilities.native_tool_calling, &tool_specs);
    }
    tool_specs.retain(|spec| safety_surface.contains(&spec.name));
    let (core_specs, appended_specs) = if appended_names.is_empty() {
        (tool_specs, Vec::new())
    } else {
        tool_specs
            .into_iter()
            .partition(|spec| !appended_names.contains(&spec.name))
    };
    let compiled_tool_context = crate::agent::turn_context::CompiledToolContext::compile(
        mode_capabilities.native_tool_calling,
        core_specs,
        appended_specs,
        &mut state.tool_snapshot_render_cache,
    );
    compiled_tool_context.apply_to_messages(&mut prepared_messages.messages);
    let mut execution_adapter = runtime_adapter.clone();
    execution_adapter.allowed_tool_names = Some(compiled_tool_context.allowed_tool_names());
    {
        let (execution, bound_context) = service.bind_agent_execution_with_runtime(
            proof,
            context,
            &execution_adapter.tool_execution_context,
            compiled_tool_context.tool_specs(),
            execution_adapter.tool_execution_service.clone(),
        )?;
        execution_adapter.tool_execution_service = Some(execution);
        execution_adapter.tool_execution_context = bound_context;
        execution_adapter.tool_execution_context.call_budget = Some(crate::agent::send_admission::call_token_budget(
            service, context, proof,
        )?);
    }
    execution_adapter.tool_execution_context.dispatch_origin = crate::tools::DispatchOrigin::ModelTurn;
    execution_adapter.tool_execution_context.turn_surface = Some(Arc::new(
        safety_surface.for_publication(compiled_tool_context.tool_specs()),
    ));
    // Argument validation enforces the contract the model was shown, and
    // what it was shown depends on which provider and model serve the
    // turn: Gemini strips `additionalProperties` / `minLength` / `minimum`
    // and a dozen more keywords before the request leaves, and an endpoint
    // may declare a dialect per model. Seeded from the provider object for
    // this turn's model and re-stamped below from the member that actually
    // answered, so a fallback across provider families validates against
    // the fallback's dialect rather than the one that was routed to first.
    execution_adapter.tool_execution_context.schema_dialect = provider.tool_schema_dialect_for(model);

    // Some provider templates (including the deployed Qwen template)
    // reject any system message that is not the first transcript item.
    // Corrective retries, recalled pages, and middleware are all allowed to
    // contribute request-local system context, so canonicalize only the
    // prepared request after every contributor has run.
    // P1-03: the brief never makes a send inadmissible. A brief whose
    // prompt cost would fail this send's token or money admission is
    // dropped for the rest of the turn; the send is then admitted (or
    // refused) exactly as with the engine off.
    if let Some(brief) = state.decision_brief.as_deref() {
        let mut with_brief = prepared_messages.messages.clone();
        crate::decision_engine::turn::append_brief(&mut with_brief, Some(brief));
        coalesce_system_messages_at_front(&mut with_brief);
        if crate::agent::send_admission::admit_provider_send(
            service,
            context,
            proof,
            &with_brief,
            compiled_tool_context.native_tool_specs(),
            runtime_adapter.tool_execution_context.schema_dialect,
        )
        .is_err()
        {
            tracing::debug!("decision brief dropped: the send would not be admitted with it");
            state.decision_brief = None;
            state.decision_brief_sha256 = None;
        }
    }
    if let (Some(analysis), Some(brief)) = (advice.as_ref(), state.decision_brief.as_deref())
        && !state.analysis_recorded
    {
        service.advisory().record_analysis(analysis, brief);
        state.analysis_recorded = true;
    }
    crate::decision_engine::turn::append_brief(&mut prepared_messages.messages, state.decision_brief.as_deref());
    coalesce_system_messages_at_front(&mut prepared_messages.messages);
    let final_system_prompt = prepared_messages
        .messages
        .iter()
        .find(|message| message.role == "system");
    let system_prompt_sha256 = final_system_prompt.map(|message| crate::agent::prompt::prompt_sha256(&message.content));
    let system_prompt_chars: usize = final_system_prompt.map_or(0, |message| message.content.chars().count());
    if let Some(brief_hash) = state.decision_brief_sha256.as_deref() {
        tracing::debug!(
            system_prompt_sha256 = system_prompt_sha256.as_deref(),
            state.decision_brief_sha256 = brief_hash,
            "advisory request prepared"
        );
    }
    Ok(PreparedRound {
        prepared_messages,
        compiled_tool_context,
        execution_adapter,
        guarded_delivery,
        system_prompt_sha256,
        system_prompt_chars,
        tool_surface,
    })
}

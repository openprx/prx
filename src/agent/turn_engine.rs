#![allow(clippy::print_stdout, clippy::print_stderr)]

use crate::hooks::{HookEvent, HookManager, payload_error};
use crate::llm::route_decision::{ProviderUsageAccumulator, TokenUsage};
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
    COMPACTION_KEEP_RECENT_MESSAGES, apply_compaction_patch_exact, apply_configurable_compaction,
    build_configurable_compaction_patch, is_context_handoff_message, persist_context_switch_degradation,
};
#[cfg(test)]
use crate::agent::context_budget::budgeted_request_tool_tokens;
#[cfg(test)]
use crate::agent::context_budget::{APPENDED_SECTION_FRAMING_TOKENS, TOKEN_HEURISTIC_BUDGET_MULTIPLIER};
use crate::agent::context_budget::{
    PRE_TURN_FLUSH_THRESHOLD, RequestToolSurfaceCounts, budgeted_tokens, estimate_budgeted_request_tokens,
    estimate_history_tokens, estimate_history_tokens_with_source, estimate_request_tokens, plan_context_budget,
    reserve_request_tool_surface, token_source_label, trim_history_to_context_budget,
    trim_history_to_context_budget_preserving_compaction_replacement_with_floor, trim_history_token_aware,
};
use crate::agent::context_paging::{
    apply_os_paging, context_mode_uses_os_paging, inject_recalled_context, recall_context_pages,
};
use crate::agent::context_preflight::{
    RestoredRequestPosition, capture_current_turn_request, preflight_context_budget_before_provider_call,
    restore_current_turn_request,
};
use crate::agent::diagnostics::{redact_sensitive_json_keys, sanitize_tool_parse_log_preview, scrub_credentials};
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
    EMPTY_ASSISTANT_RESPONSE_RETRY_MESSAGE, TOOL_SUMMARY_ONLY_RESPONSE_MESSAGE, append_corrective_aside,
    contains_assistant_fabricated_tool_marker, is_empty_assistant_response, is_tool_summary_only_response,
    strip_assistant_fabricated_tool_markers,
};
use crate::agent::sanitize::{known_tool_names, sanitize_stream_chunk};
use crate::agent::stream_buffer::StreamBoundaryBuffer;
use crate::agent::tool_call_parser::{
    looks_like_unparsed_tool_call_syntax, parse_structured_tool_calls, parse_tool_calls,
};
#[cfg(test)]
use crate::agent::tool_result_recording::INTERRUPTED_TOOL_RESULT;
use crate::agent::tool_result_recording::{
    ExecutedToolRecords, MAX_TOOL_RESULT_INLINE_CHARS, ToolResultRecorder, compact_tool_result_for_history_with_status,
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
use crate::agent::turn_request::{ToolLoopMemory, TurnDecision, TurnDelivery, TurnRequest, TurnSchedule, TurnServices};
#[cfg(test)]
use crate::memory::{DocumentSearchResult, MemoryPrincipal};
#[cfg(test)]
use crate::providers::Provider;
#[cfg(test)]
use crate::providers::ToolCall;
#[cfg(all(test, feature = "wasm-plugins"))]
use std::path::Path;

/// Refresh one selected tool's dynamic capabilities, reporting a failure
/// without aborting the turn: a backend that cannot refresh keeps whatever
/// schema it last published rather than taking the whole request down.
async fn refresh_selected_tool(tool: &dyn Tool, observer: &dyn Observer, hooks: &HookManager) {
    if let Err(err) = tool.refresh().await {
        let message = format!("refresh failed for tool {}: {err}", tool.name());
        observer.record_event(&ObserverEvent::Error {
            component: "tool-refresh".to_string(),
            message: message.clone(),
        });
        hooks
            .emit(HookEvent::Error, payload_error("tool-refresh", &message))
            .await;
    }
}

/// Run one turn of the agent loop: send messages, parse tool calls, execute
/// tools, and loop until the model produces a final answer (or decides to stay
/// silent). Returns the [`ToolLoopOutcome`] and the [`ToolLoopTrace`] that
/// attributes the final returning turn to the provider/model that served it
/// (FIX-P0-30/31). This is the only engine entry point.
///
/// # Hang detection
///
/// This entrypoint wraps the loop in [`crate::agent::idle::run_guarded`], which
/// terminates the turn if it produces **no observable progress at all** for the
/// configured window (`[runtime] idle_hang_secs`, default 1800s). That is a
/// stall detector, not a turn timeout: every provider chunk, tool call, and
/// compaction resets the window, so a turn that keeps working is never bounded
/// by it however long it runs. Wall-clock turn budgets were removed from this
/// runtime deliberately and this is not a way back to one — see the module
/// documentation of [`crate::agent::idle`] before changing anything here.
///
/// The guard lives at this level rather than inside the loop so that returning
/// from it *drops* the loop future, which drops every future nested in it and
/// so every RAII guard owning a child process. That cleanup works even against
/// a tool that ignores cancellation, which is the population a hang detector
/// exists for.
pub(crate) async fn run_turn(
    history: &mut Vec<ChatMessage>,
    request: TurnRequest<'_>,
    services: TurnServices<'_>,
    delivery: TurnDelivery,
) -> Result<(ToolLoopOutcome, ToolLoopTrace)> {
    let guard_token = delivery.cancellation.clone();
    let channel_name = request.channel_name;
    // Boxed at the boundary, not inside the guard. The loop future is one of the
    // largest in the process, and letting the guard hold it inline would place a
    // second copy of it in this frame — enough to overflow a tokio worker's
    // stack on the chat path. A pointer costs nothing here and keeps the guard's
    // own future tiny.
    let inner = Box::pin(run_turn_unguarded(history, request, services, delivery));
    crate::agent::idle::run_guarded(
        crate::agent::idle::configured(),
        channel_name,
        guard_token.as_ref(),
        inner,
    )
    .await
}

/// The history an inbound middleware plugin handed back, with every `user`
/// message the plugin wrote itself neutralised: a plugin is not trusted to
/// write the loop's own notes, so a user message it added or changed that
/// opens with a reserved marker is defused like stored user text.
///
/// A user message the plugin passed through unchanged (byte-equal to one the
/// history already held, such as an earlier round's `[Tool results]` block)
/// keeps its text: neutralising that would turn the loop's own note into a
/// user question.
#[cfg(feature = "wasm-plugins")]
fn neutralize_middleware_user_messages(before: &[ChatMessage], candidate: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let passed_through: std::collections::HashSet<&str> = before
        .iter()
        .filter(|message| message.role == "user")
        .map(|message| message.content.as_str())
        .collect();
    candidate
        .into_iter()
        .map(|message| {
            if message.role == "user" && !passed_through.contains(message.content.as_str()) {
                crate::agent::turn_contracts::stored_turn_message(message.role, message.content)
            } else {
                message
            }
        })
        .collect()
}

/// Apply a valid middleware replacement to its actual prior message buffer.
#[cfg(feature = "wasm-plugins")]
fn apply_middleware_messages(history: &mut Vec<ChatMessage>, transformed: &serde_json::Value) {
    if let Some(messages) = transformed.get("messages")
        && let Ok(candidate) = serde_json::from_value::<Vec<ChatMessage>>(messages.clone())
        && !candidate.is_empty()
        && candidate.iter().all(|message| !message.role.trim().is_empty())
    {
        *history = neutralize_middleware_user_messages(history, candidate);
    }
}

/// The bytes an accepted turn delivers once outbound middleware has had its say.
///
/// `accepted` is the text the terminal branch accepted; `transformed` is what
/// the outbound stage returned for it.
///
/// The fabricated tool-marker guard runs above the stage, so it inspects the
/// model's text only. Re-running the strip here is what keeps a middleware
/// plugin from putting a `[Used tools: ...]` line into the delivered reply and
/// into the in-memory history, where nothing else in this turn would take it
/// out again. It only strips: the response is already accepted, so a marker the
/// plugin introduced never spends a corrective retry and never fails the turn —
/// that is the whole difference from the guard above.
///
/// Borrowed whenever there is nothing to do — middleware that changed nothing
/// returned text that already passed the guard above, and
/// [`strip_assistant_fabricated_tool_markers`] borrows in turn whenever it finds
/// no marker — so the delivered path pays for no allocation of its own.
#[cfg(feature = "wasm-plugins")]
fn finalize_outbound_display_text<'a>(accepted: &str, transformed: &'a str) -> Cow<'a, str> {
    if transformed == accepted {
        return Cow::Borrowed(transformed);
    }
    strip_assistant_fabricated_tool_markers(transformed)
}

// Per-thread count of `MiddlewareStage::Outbound` invocations made by the tool
// loop, for the exactly-once regressions only.
//
// The `Outbound` contract is a *call-count* contract, and the middleware chain
// (`crate::plugins::capabilities::middleware::MiddlewareChain`) accepts compiled
// WASM components only — a test can install a scripted provider or a recording
// tool, but it cannot install a counting middleware double the way it can for
// those. This cell is that counter, and nothing else reads it.
//
// It is `#[cfg(test)]`: a production build carries no instrumentation at the
// `Outbound` call site. It is thread-local: `#[tokio::test]` drives the loop on
// the test's own thread, so tests running in parallel cannot observe each
// other's turns and no lock is needed.
#[cfg(all(test, feature = "wasm-plugins"))]
thread_local! {
    static OUTBOUND_MIDDLEWARE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Start a fresh `Outbound` invocation count for the current test thread.
#[cfg(all(test, feature = "wasm-plugins"))]
fn reset_outbound_middleware_calls() {
    OUTBOUND_MIDDLEWARE_CALLS.with(|calls| calls.set(0));
}

/// `Outbound` invocations recorded on this thread since the last reset.
#[cfg(all(test, feature = "wasm-plugins"))]
fn outbound_middleware_calls() -> usize {
    OUTBOUND_MIDDLEWARE_CALLS.with(std::cell::Cell::get)
}

/// The loop itself, without the hang guard.
///
/// Split out only so [`run_turn`] can wrap it; every caller must go through
/// the guarded entrypoint.
async fn run_turn_unguarded(
    history: &mut Vec<ChatMessage>,
    request: TurnRequest<'_>,
    services: TurnServices<'_>,
    delivery: TurnDelivery,
) -> Result<(ToolLoopOutcome, ToolLoopTrace)> {
    let TurnRequest {
        channel_name,
        scope: scope_ctx,
        mode: chat_mode,
        silent,
        expose_stay_silent,
        multimodal: multimodal_config,
        schedule:
            TurnSchedule {
                read_only_tool_concurrency_window,
                priority_scheduling_enabled,
                low_priority_tool_names,
            },
        compaction: compaction_config,
        decision: TurnDecision {
            service,
            context,
            proof,
        },
    } = request;
    let TurnServices {
        provider,
        provider_name,
        model,
        temperature,
        tools_registry,
        observer,
        hooks,
        approval,
        approval_resolver,
        runtime_adapter,
        memory: memory_runtime,
    } = services;
    let TurnDelivery {
        cancellation: cancellation_token,
        on_delta,
        on_tool_call,
    } = delivery;
    service.validate_agent_execution_model(proof, provider_name, model)?;
    if matches!(
        context.disposition()?,
        crate::runtime::turn_decision::TurnDisposition::CollectApprovalArguments
    ) {
        let retained = runtime_adapter
            .as_ref()
            .and_then(|adapter| adapter.provider_trace.as_ref())
            .ok_or_else(|| anyhow::anyhow!("collection main terminal owner missing"))?;
        let cancellation = cancellation_token
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("collection cancellation owner missing"))?;
        let lease = service.begin_approval_collection(proof, context)?;
        let fabric = memory_runtime
            .event_fabric
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("collection request event owner missing"))?;
        let scope = memory_runtime
            .request_event_scope
            .clone()
            .ok_or_else(|| anyhow::anyhow!("collection request event scope missing"))?;
        #[cfg(feature = "wasm-plugins")]
        {
            let transformed = hooks
                .process_wasm_middleware(
                    crate::plugins::capabilities::middleware::MiddlewareStage::Inbound,
                    serde_json::json!({"channel": channel_name, "messages": history}),
                )
                .await;
            apply_middleware_messages(history, &transformed);
        }
        let spec = lease.proposal_spec();
        let mut proposal_messages = history.clone();
        proposal_messages.insert(0, ChatMessage::system(
            "Collect exactly one complete argument proposal for the offered operation (a file change, an MCP write or a skills_manage lifecycle change). This request cannot execute it. Return the requested tool call; do not claim that the operation has run.",
        ));
        #[cfg(feature = "wasm-plugins")]
        {
            let transformed = hooks
                .process_wasm_middleware(
                    crate::plugins::capabilities::middleware::MiddlewareStage::LlmRequest,
                    serde_json::json!({
                        "provider": provider_name, "model": model, "temperature": temperature,
                        "messages": proposal_messages, "tools": [&spec],
                    }),
                )
                .await;
            apply_middleware_messages(&mut proposal_messages, &transformed);
            if let Some(tools) = transformed.get("tools")
                && let Ok(candidate) = serde_json::from_value::<Vec<crate::tools::ToolSpec>>(tools.clone())
            {
                anyhow::ensure!(
                    candidate.len() == 1
                        && candidate.first().is_some_and(|offered| {
                            offered.name == spec.name
                                && crate::tools::capability::fingerprint::tool_spec_fingerprint(
                                    &offered.name,
                                    &offered.description,
                                    &offered.parameters,
                                ) == crate::tools::capability::fingerprint::tool_spec_fingerprint(
                                    &spec.name,
                                    &spec.description,
                                    &spec.parameters,
                                )
                        }),
                    "request middleware changed the approval proposal schema"
                );
            }
        }
        coalesce_system_messages_at_front(&mut proposal_messages);
        if let Some(config) = compaction_config {
            let surface = crate::agent::turn_context::RequestToolMeasurement {
                native_payload: crate::agent::turn_context::measure_native_tool_payload_for(
                    std::slice::from_ref(&spec),
                    provider.tool_schema_dialect_for(model),
                ),
                instructions: crate::agent::context_budget::TokenMeasurement::NONE,
            };
            let reserved = reserve_request_tool_surface(config, surface);
            anyhow::ensure!(
                !plan_context_budget(&proposal_messages, &reserved, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit,
                "approval collection request exceeds the configured context budget"
            );
        }
        let proposal_schema = crate::tools::capability::fingerprint::tool_spec_fingerprint(
            &spec.name,
            &spec.description,
            &spec.parameters,
        );
        fabric
            .record_runtime_event(
                scope.with_recipient(format!("{provider_name}/{model}")),
                "llm.request.context.compiled",
                "approval argument collection; no executable capabilities",
                Some(
                    serde_json::json!({
                        "schema_version": 1,
                        "provider": providers::redact_endpoint_userinfo(provider_name),
                        "model": model,
                        "messages_count": proposal_messages.len(),
                        "decision": {
                            "decision_id": context.decision_id().to_string(),
                            "catalog_generation": context.catalog_generation().to_hex(),
                            "directive_kind": "collect_approval_arguments",
                            "constraint_count": 0,
                        },
                        "proposal_schema": {"name": spec.name, "fingerprint": hex::encode(proposal_schema.0)},
                        "executable_capabilities": [],
                    })
                    .to_string(),
                ),
            )
            .await?;
        let system_prompt = proposal_messages.iter().find(|message| message.role == "system");
        let system_prompt_sha256 = system_prompt.map(|message| crate::agent::prompt::prompt_sha256(&message.content));
        hooks
            .emit(
                HookEvent::LlmRequest,
                serde_json::json!({
                    "provider": provider_name, "model": model, "messages_count": proposal_messages.len(),
                    "system_prompt_sha256": system_prompt_sha256,
                    "system_prompt_chars": system_prompt.map_or(0, |message| message.content.chars().count()),
                    "proposal_schema_sha256": hex::encode(proposal_schema.0),
                    "tool_count": 1, "executable_capability_count": 0,
                }),
            )
            .await;
        observer.record_event(&ObserverEvent::LlmRequest {
            provider: provider_name.to_owned(),
            model: model.to_owned(),
            messages_count: proposal_messages.len(),
        });
        let outcome = super::approval_collection::collect_approval_arguments(
            super::approval_collection::CollectionProviderRequest {
                provider,
                messages: &proposal_messages,
                selected_provider: provider_name,
                model,
                temperature,
                retained,
                cancellation,
            },
            lease,
        )
        .await?;
        outcome.pending?;
        let mut trace = outcome.trace;
        trace.execution_trace = Some(context.terminal_execution_trace());
        return Ok((
            ToolLoopOutcome::AwaitingApproval {
                reply: format!(
                    "Approval is required for decision {}. Inspect and review this decision before resuming it. The operation has not run.",
                    context.decision_id(),
                ),
            },
            trace,
        ));
    }
    let ToolLoopMemory {
        ledger,
        ingest: document_ingest,
        event_fabric,
        request_event_scope,
    } = memory_runtime;
    let parameter_capture = context.capture_main_parameters(event_fabric.is_some());
    let runtime_adapter = runtime_adapter.unwrap_or_else(|| {
        build_agent_tool_runtime_adapter(AgentToolRuntimeInputs {
            scope: scope_ctx,
            channel_name,
            tools_registry: Arc::clone(&tools_registry),
            approval: approval.clone(),
            approval_resolver: approval_resolver.clone(),
            cancellation: cancellation_token.clone(),
            ledger: ledger.clone(),
            unscoped_policy: None,
        })
    });

    // FIX-P0-30/31: attribution of the final returning turn. Each provider call
    // overwrites this with that turn's real serving provider/model + attempts;
    // the value at the returning turn is handed back to the caller.
    let mut last_turn_trace = ToolLoopTrace::default();
    let mut usage_accumulator = ProviderUsageAccumulator::new();
    // FIX #2: sticky cross-turn fallback flag. `last_turn_trace` is overwritten
    // every turn, so a fallback on an intermediate (tool-call) turn would be lost
    // if a later clean turn produced the answer. Accumulate it separately and
    // fold it into the returned trace so the recorded status reflects the whole
    // user turn, not just its final iteration.
    let mut any_turn_had_fallback = false;

    let read_only_schedule = ReadOnlyToolScheduleConfig {
        concurrency_window: read_only_tool_concurrency_window.max(1),
        priority_enabled: priority_scheduling_enabled,
        low_priority_tool_names: low_priority_tool_names.into_iter().collect(),
    };
    // The Chat streaming contract historically advertises the registry on
    // every `stream_chat_with_history` request. Some streaming-compatible test
    // and custom providers do not override `supports_native_tools()`, so using
    // that buffered-path capability gate here silently drops real Chat tools.
    // Buffered Agent callers keep the provider capability gate unchanged.
    let provider_mode = if runtime_adapter.stream_provider {
        crate::providers::traits::ProviderRequestMode::Streaming
    } else {
        crate::providers::traits::ProviderRequestMode::NonStreaming
    };
    let mode_capabilities =
        service.effective_provider_capabilities(provider_name, model, provider.capabilities_for(model, provider_mode));

    // FIX-P3-05: Letta/MemGPT-style OS-paging for legacy compaction modes.
    // Exact context switching bypasses this block so the durable handoff owns
    // every cold message before the provider projection changes. When active:
    //   1. Recall — semantically retrieve relevant evicted pages and inject them
    //      (proactive_retrieval) so the model can see paged-out history again.
    //   2. Evict — page the oldest cold context out to the durable document
    //      store (non-destructively), shrinking the hot window before the turn.
    // Legacy compaction below still runs as a backstop if paging left a legacy
    // mode above the 0.85 threshold.
    if let Some(config) = compaction_config {
        let switch_needs_lookup = matches!(config.mode, crate::config::AgentCompactionMode::Switch)
            && (history.iter().any(is_context_handoff_message)
                || plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD).over_warning);
        if switch_needs_lookup
            && !tools_registry
                .iter()
                .any(|tool| tool.supports_name(crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME))
        {
            anyhow::bail!(
                "context switch requires the '{}' recovery tool, but it is not registered",
                crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME
            );
        }
    }

    if let Some(config) = compaction_config
        && context_mode_uses_os_paging(config)
    {
        if let Some(query) = latest_user_query(history).map(ToString::to_string) {
            if let Some(recalled) = recall_context_pages(&query, config, document_ingest.as_ref()).await {
                inject_recalled_context(history, recalled);
            }
        }
        match apply_os_paging(history, config, document_ingest.as_ref()).await {
            Ok(true) => {
                crate::agent::idle::beat(crate::agent::idle::ProgressKind::Compaction);
                tracing::debug!("os-paging: pre-turn eviction applied");
            }
            Ok(false) => {}
            Err(e) => tracing::warn!("os-paging: pre-turn eviction error (soft): {e}"),
        }
    }

    // P1-3: Proactive pre-turn memory flush — compact before starting the loop
    // if the context is already above 85% of the token budget.
    if let Some(config) = compaction_config {
        let budget = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
        if budget.over_warning {
            tracing::info!(
                token_estimate = budget.used_tokens,
                threshold = budget.warning_threshold_tokens,
                "Pre-turn memory flush: context near limit, running pre-emptive compaction"
            );
            if let Err(e) =
                apply_configurable_compaction(history, provider, model, config, document_ingest.as_ref(), "pre_turn")
                    .await
            {
                tracing::warn!("Pre-turn compaction failed: {e}");
            }
        }
    }

    // One inbound middleware pass per user turn. Every TUI, CLI, gateway, and
    // IM channel reaches this shared loop, so the transformation cannot drift
    // between entrypoints. A malformed or structurally invalid replacement is
    // ignored and the original history remains authoritative.
    #[cfg(feature = "wasm-plugins")]
    {
        let transformed = hooks
            .process_wasm_middleware(
                crate::plugins::capabilities::middleware::MiddlewareStage::Inbound,
                serde_json::json!({
                    "channel": channel_name,
                    "messages": history,
                }),
            )
            .await;
        apply_middleware_messages(history, &transformed);
    }

    let mut consecutive_failures: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut unrecoverable_failures: std::collections::HashMap<String, u8> = std::collections::HashMap::new();
    let mut tool_summary_retry_attempted = false;
    let mut inject_tool_summary_retry_instruction = false;
    // Whether this turn ever really dispatched a tool batch. A response that
    // claims tool use (`[Used tools: ...]`) while this is still false is a
    // fabrication, and the only case worth spending a corrective retry on.
    let mut executed_any_tool_call = false;
    let assessment_source = runtime_adapter.verify_chat_completion.then(|| history.clone());
    let mut observed_tool_results = false;
    let mut completion_evidence = super::completion_review::CompletionEvidence::default();
    let mut completion_correction: Option<String> = None;
    let mut execution_requirement = None;
    let mut inject_execution_retry = false;
    let mut empty_response_retry_attempted = false;
    let mut inject_empty_response_retry_instruction = false;

    // Rebuilt every iteration so dynamic backends cannot go stale, but the
    // snapshot is normally identical across a turn's iterations: memoize its
    // rendered prompt sections and fingerprint instead of recomputing one JSON
    // serialization plus two SHA-256 digests per tool each time.
    let mut tool_snapshot_render_cache = crate::agent::turn_context::ToolSnapshotRenderCache::default();
    // Same reasoning for the request-size accounting: measuring the published
    // tool surface means serializing and tokenizing every schema and rendering
    // and tokenizing the prompt sections, so the result is reused for as long
    // as the exposed set is unchanged.
    // The payload is measured as the provider's dialect puts it on the wire
    // (C2b-3): a cleaning dialect inlines `$ref` and can be larger than the
    // published document.
    let mut request_tool_surface_cache =
        crate::agent::turn_context::RequestToolSurfaceCache::for_dialect(provider.tool_schema_dialect_for(model));
    // A sub-agent inherits the hidden surface of the turn that spawned it.
    // Sub-agent loops deliberately run without tool tiering, so without this a
    // conversation partner on an IM channel could reach the whole operations
    // surface simply by asking for a spawn or a delegate. Read once: the spawn
    // context is fixed for the lifetime of this turn.
    let inherited_exclusions = crate::tools::sessions_spawn::ambient_channel_excluded_tools();
    let mut iteration = 0usize;
    loop {
        iteration = iteration.saturating_add(1);
        // Notify progress for multi-iteration tool loops (skip the first iteration).
        if iteration > 1 {
            if let Some(ref tx) = on_tool_call {
                let _ = tx.try_send(ToolCallNotification::Progress { iteration });
            }
        }

        if cancellation_token.as_ref().is_some_and(CancellationToken::is_cancelled) {
            return Err(ToolLoopCancelled.into());
        }

        // Hits of an earlier `capability_search` become declared tools on this
        // request, through the same admission as a call by name. The request
        // that carried the search is already sent and stays as it was.
        service.admit_offered_discoveries(proof, context, |name, root| {
            inherited_exclusions
                .iter()
                .any(|excluded| excluded == name || excluded == root)
        });

        // Route the turn before refreshing dynamic tools. Refreshing the whole
        // registry here made an unrelated turn wait for every configured MCP
        // server (for example, a PDF-only WhatsApp request paid Playwright's
        // cold `npx` startup cost). The selected slice is also the exact slice
        // whose schemas are exposed below, so runtime discovery and the model's
        // catalog stay on one capability boundary.
        //
        // This whole block runs ahead of the context-budget preflight because
        // the schemas it produces travel to the provider in the same request as
        // the messages: a budget that cannot see them cannot keep the request
        // inside the window. Routing still precedes every refresh, and only the
        // routed slice is refreshed.
        let constrained = service.agent_execution_specs(proof, context)?;
        let search_spec = service.agent_capability_search_spec(proof, context)?;
        let appended = service.agent_appended_execution_specs(proof, context)?;
        let mut selected_tools: Vec<&dyn Tool> = tools_registry
            .iter()
            .filter(|tool| {
                constrained.roots.contains(tool.name())
                    || appended.roots.contains(tool.name())
                    || search_spec.as_ref().is_some_and(|spec| spec.name == tool.name())
            })
            .map(Box::as_ref)
            .collect();
        if !inherited_exclusions.is_empty() {
            // Matched through `supports_name`, not the root name: `channel_exclude`
            // names a *published* tool, and a backend that publishes several
            // (an MCP router, a WASM alias) answers to a name its root spelling
            // never equals. Comparing root names left those aliases exposed.
            selected_tools.retain(|tool| {
                !inherited_exclusions
                    .iter()
                    .any(|hidden| tool.name() == hidden || tool.supports_name(hidden))
            });
        }
        for tool in &selected_tools {
            refresh_selected_tool(*tool, observer, hooks).await;
        }
        // Refresh dynamic backends before snapshotting their schemas. Rebuild on
        // every iteration so MCP discovery and WASM generation swaps cannot leave
        // the provider with stale ToolSpecs for the remainder of the turn.
        tracing::debug!(
            total = tools_registry.len(),
            filtered = selected_tools.len(),
            "tool tiering applied"
        );
        let tool_catalog = crate::tools::ToolCatalog::from_tools(selected_tools.iter().copied());
        let mut tool_specs = service.agent_execution_specs(proof, context)?.specs;
        let appended = service.agent_appended_execution_specs(proof, context)?;
        let appended_names: std::collections::BTreeSet<_> =
            appended.specs.iter().map(|spec| spec.name.clone()).collect();
        tool_specs.extend(appended.specs);
        if let Some(spec) = search_spec {
            tool_specs.push(spec);
        }
        if expose_stay_silent {
            {
                tool_specs.push(service.agent_runtime_dependency_spec(proof, context, "stay_silent")?);
            }
        }
        let mut decision_fingerprints = {
            tool_specs
                .iter()
                .map(|spec| {
                    (
                        spec.name.clone(),
                        crate::tools::capability::fingerprint::tool_spec_fingerprint(
                            &spec.name,
                            &spec.description,
                            &spec.parameters,
                        ),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        crate::tools::filter_tool_specs_for_exposure(&mut tool_specs, expose_stay_silent);
        let mut tool_surface = request_tool_surface_cache.measure(mode_capabilities.native_tool_calling, &tool_specs);
        let mut tool_surface_counts =
            RequestToolSurfaceCounts::measure(tools_registry.len(), selected_tools.len(), &tool_catalog, &tool_specs);

        // P5: hard budget preflight before every provider call. Summary
        // compaction is preferred when enabled; token-aware trim is the last
        // local guard. Overflow retry below remains the provider-side fallback.
        //
        // The message total and the request total it planned against are
        // carried into the request event, so a difference between what was
        // budgeted and what was finally prepared is a number an operator can
        // read rather than an invariant nobody can check. `None` on the
        // entrypoints that pass no compaction config: there is no budget to
        // report there.
        let budgeted_totals: Option<(usize, usize)> = if let Some(config) = compaction_config {
            let budget = preflight_context_budget_before_provider_call(
                history,
                provider,
                model,
                config,
                tool_surface,
                document_ingest.as_ref(),
                "pre_provider_call",
            )
            .await?;
            Some((
                budget.used_tokens,
                estimate_budgeted_request_tokens(budget.measurement(), tool_surface),
            ))
        } else {
            None
        };

        // Evaluated after the preflight: a `switch` rollover performed there is
        // what puts the handoff into history, and the iteration that receives
        // the handoff is the one that has to publish the recovery tool the
        // handoff tells the model to call.
        let context_lookup_required = compaction_config
            .is_some_and(|config| matches!(config.mode, crate::config::AgentCompactionMode::Switch))
            && history.iter().any(is_context_handoff_message);
        if context_lookup_required
            && !selected_tools
                .iter()
                .any(|tool| tool.supports_name(crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME))
        {
            let lookup = tools_registry
                .iter()
                .find(|tool| tool.supports_name(crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "context switch recovery tool '{}' disappeared from the registry",
                        crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME
                    )
                })?;
            refresh_selected_tool(lookup.as_ref(), observer, hooks).await;
            selected_tools.push(lookup.as_ref());
            let tool_catalog = crate::tools::ToolCatalog::from_tools(selected_tools.iter().copied());
            {
                let dependency = service.agent_runtime_dependency_spec(
                    proof,
                    context,
                    crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME,
                )?;
                {
                    let fingerprints = &mut decision_fingerprints;
                    fingerprints.insert(
                        dependency.name.clone(),
                        crate::tools::capability::fingerprint::tool_spec_fingerprint(
                            &dependency.name,
                            &dependency.description,
                            &dependency.parameters,
                        ),
                    );
                }
                tool_specs.push(dependency);
            }
            crate::tools::filter_tool_specs_for_exposure(&mut tool_specs, expose_stay_silent);
            // The recovery tool is published after the budget was planned, so
            // this iteration's window is short by exactly what that one spec
            // adds — its schema and its line in the capability snapshot. The
            // next iteration budgets the full set.
            tool_surface = request_tool_surface_cache.measure(mode_capabilities.native_tool_calling, &tool_specs);
            tool_surface_counts = RequestToolSurfaceCounts::measure(
                tools_registry.len(),
                selected_tools.len(),
                &tool_catalog,
                &tool_specs,
            );
        }

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
        let mut prepared_messages =
            multimodal::prepare_messages_for_provider(history, multimodal_config, media_artifacts.as_ref()).await?;
        // Corrective retries are appended at the tail, not as request-local
        // `system` context. A system message here is folded into the leading
        // one by `coalesce_system_messages_at_front` below, which appends bytes
        // to the system prefix and misses the provider-side prefix cache for
        // the whole conversation — on the one turn that is already paying for a
        // wasted provider call. Appending at the tail leaves every cached byte
        // ahead of it untouched, and keeps the "exactly one system message, at
        // index 0" shape that strict chat templates require.
        let verify_chat_completion =
            runtime_adapter.verify_chat_completion && !chat_mode.intercepts_writes() && !tool_specs.is_empty();
        if verify_chat_completion {
            append_corrective_aside(
                &mut prepared_messages.messages,
                super::completion_review::COMPLETION_INSTRUCTIONS,
            );
            if let Some(correction) = &completion_correction {
                append_corrective_aside(&mut prepared_messages.messages, correction);
            }
        }
        if inject_execution_retry {
            append_corrective_aside(
                &mut prepared_messages.messages,
                super::execution_requirement::RETRY_MESSAGE,
            );
            if let Some(config) = compaction_config {
                anyhow::ensure!(
                    estimate_request_tokens(
                        estimate_history_tokens_with_source(&prepared_messages.messages),
                        tool_surface.native_payload.measurement
                    ) <= config.max_context_tokens.saturating_sub(config.reserve_tokens),
                    "execution correction exceeds the request budget; execution is unverified"
                );
            }
            inject_execution_retry = false;
        }
        if inject_tool_summary_retry_instruction {
            append_corrective_aside(&mut prepared_messages.messages, TOOL_SUMMARY_ONLY_RESPONSE_MESSAGE);
            inject_tool_summary_retry_instruction = false;
        }
        if inject_empty_response_retry_instruction {
            append_corrective_aside(&mut prepared_messages.messages, EMPTY_ASSISTANT_RESPONSE_RETRY_MESSAGE);
            inject_empty_response_retry_instruction = false;
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
        if let Some(required) = required_lookup_spec {
            tool_specs.retain(|spec| spec.name != crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME);
            tool_specs.push(required);
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
            tool_surface = request_tool_surface_cache.measure(mode_capabilities.native_tool_calling, &tool_specs);
        }
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
            &mut tool_snapshot_render_cache,
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
        }
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
        coalesce_system_messages_at_front(&mut prepared_messages.messages);
        let final_system_prompt = prepared_messages
            .messages
            .iter()
            .find(|message| message.role == "system");
        let system_prompt_sha256 =
            final_system_prompt.map(|message| crate::agent::prompt::prompt_sha256(&message.content));
        let system_prompt_chars = final_system_prompt.map_or(0, |message| message.content.chars().count());
        let tool_snapshot = compiled_tool_context.fingerprint();
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
            let budgeted_messages_tokens = budgeted_totals.map(|(messages, _)| messages);
            let budgeted_request_tokens = budgeted_totals.map(|(_, request)| request);
            let messages_bytes = prepared_messages.messages.iter().fold(0usize, |total, message| {
                total
                    .saturating_add(message.role.len())
                    .saturating_add(message.content.len())
            });
            let messages_chars = prepared_messages.messages.iter().fold(0usize, |total, message| {
                total
                    .saturating_add(message.role.chars().count())
                    .saturating_add(message.content.chars().count())
            });
            let messages_measurement = estimate_history_tokens_with_source(&prepared_messages.messages);
            let messages_tokens = budgeted_tokens(messages_measurement);
            let native_tool_tokens = budgeted_tokens(tool_surface.native_payload.measurement);
            let instruction_tokens = budgeted_tokens(tool_surface.instructions);
            let request_tokens = estimate_request_tokens(messages_measurement, tool_surface.native_payload.measurement);
            tracing::debug!(
                target: "openprx::prompt",
                system_prompt_sha256 = system_prompt_sha256.as_deref().unwrap_or("none"),
                system_prompt_chars,
                tool_snapshot_sha256 = %tool_snapshot.sha256,
                tool_count = tool_snapshot.count,
                tool_snapshot_render_hits = tool_snapshot_render_cache.hits(),
                tool_snapshot_render_misses = tool_snapshot_render_cache.misses(),
                messages_bytes,
                messages_chars,
                messages_tokens,
                native_tool_bytes = tool_surface.native_payload.bytes,
                native_tool_tokens,
                instruction_tokens,
                budgeted_messages_tokens,
                budgeted_request_tokens,
                request_tokens,
                token_source = token_source_label(messages_measurement.source),
                "final request system prompt compiled"
            );
            // Compilation is completed work, including a cold tokenizer or a
            // changed tool surface. Record it before the durable event yields;
            // an iteration starting is not a completed provider response.
            crate::agent::idle::beat(crate::agent::idle::ProgressKind::RequestPrepared);

            if let Some(fabric) = event_fabric.as_ref() {
                let compiled_decision = {
                    let directive_kind = match context.directive() {
                        crate::causal_tree::directive::TurnExecutionDirective::CollectApprovalArguments(_) => {
                            anyhow::bail!("collection entered executable request preparation")
                        }
                        crate::causal_tree::directive::TurnExecutionDirective::AnswerDirectly => "answer_directly",
                        crate::causal_tree::directive::TurnExecutionDirective::Execute { .. } => "execute",
                        crate::causal_tree::directive::TurnExecutionDirective::Delegate { .. } => "delegate",
                        crate::causal_tree::directive::TurnExecutionDirective::Retrieve { .. } => "retrieve",
                        crate::causal_tree::directive::TurnExecutionDirective::Discover { .. } => "discover",
                        crate::causal_tree::directive::TurnExecutionDirective::Clarify { .. } => "clarify",
                        crate::causal_tree::directive::TurnExecutionDirective::RequestApproval { .. } => {
                            "request_approval"
                        }
                        crate::causal_tree::directive::TurnExecutionDirective::Refuse { .. } => "refuse",
                    };
                    serde_json::json!({
                        "decision_id": context.decision_id().to_string(),
                        "snapshot_id": tool_snapshot.sha256,
                        "catalog_generation": context.catalog_generation().to_hex(),
                        "directive_kind": directive_kind,
                        "constraint_count": context.execution_refs().len(),
                        // This reader currently has no mid-turn discovery producer.
                        "discovered_count": 0,
                    })
                };
                let request_context_payload = serde_json::json!({
                    "schema_version": 1,
                    "iteration": iteration,
                    // Durable: a `custom:` URL may carry `user:password@`.
                    "provider": providers::redact_endpoint_userinfo(provider_name),
                    "model": model,
                    "messages_count": prepared_messages.messages.len(),
                    "system_prompt": {
                        "sha256": system_prompt_sha256.as_deref(),
                        "chars": system_prompt_chars,
                    },
                    "decision": compiled_decision,
                    "tool_snapshot": &tool_snapshot,
                    "tool_surface": tool_surface_counts.to_json(),
                    "tool_snapshot_render": {
                        "hits": tool_snapshot_render_cache.hits(),
                        "misses": tool_snapshot_render_cache.misses(),
                    },
                    "request_estimate": {
                        "messages_bytes": messages_bytes,
                        "messages_chars": messages_chars,
                        "messages_tokens": messages_tokens,
                        "native_tool_bytes": tool_surface.native_payload.bytes,
                        "native_tool_tokens": native_tool_tokens,
                        "instruction_tokens": instruction_tokens,
                        "budgeted_messages_tokens": budgeted_messages_tokens,
                        "budgeted_total_tokens": budgeted_request_tokens,
                        "total_tokens": request_tokens,
                        "token_source": token_source_label(messages_measurement.source),
                    },
                });
                let scope = request_event_scope
                    .clone()
                    .unwrap_or_else(|| execution_adapter.tool_execution_context.envelope.message_scope());
                let scope = scope.with_recipient(format!("{provider_name}/{model}"));
                let content = format!(
                    "system_prompt_sha256={} tool_snapshot_sha256={} iteration={iteration}",
                    system_prompt_sha256.as_deref().unwrap_or("none"),
                    tool_snapshot.sha256
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
                    tracing::warn!(error = %error, "Failed to persist compiled LLM request context");
                }
            }
        } else {
            crate::agent::idle::beat(crate::agent::idle::ProgressKind::RequestPrepared);
        }
        hooks
            .emit(
                HookEvent::LlmRequest,
                serde_json::json!({
                    "provider": provider_name,
                    "model": model,
                    "messages_count": history.len(),
                    "system_prompt_sha256": system_prompt_sha256.as_deref(),
                    "system_prompt_chars": system_prompt_chars,
                    "tool_snapshot_sha256": tool_snapshot.sha256,
                    "tool_count": tool_snapshot.count,
                }),
            )
            .await;
        observer.record_event(&ObserverEvent::LlmRequest {
            provider: provider_name.to_string(),
            model: model.to_string(),
            messages_count: history.len(),
        });

        let llm_started_at = Instant::now();

        // Unified path via Provider::chat so provider-specific native tool logic
        // (OpenAI/Anthropic/OpenRouter/compatible adapters) is honored.
        let request_tools = compiled_tool_context.native_tool_specs();

        // FIX-P0-30/31: use the trace-returning provider entrypoint so this
        // turn's real serving provider/model and failover attempts flow back
        // to the caller. The `ReliableProvider` overrides `chat_traced`; other
        // providers get a synthetic single-attempt trace via the trait default.
        let provider_observation = execution_adapter
            .tool_execution_context
            .execution_trace
            .as_ref()
            .map(|trace| trace.begin_provider())
            .transpose()?;
        if let Some(retained) = &runtime_adapter.provider_trace {
            retained.begin();
        }
        let chat_result = if runtime_adapter.stream_provider {
            parameter_capture
                .scope(run_streaming_provider_turn(
                    provider,
                    &prepared_messages.messages,
                    request_tools,
                    provider_name,
                    model,
                    temperature,
                    cancellation_token.as_ref(),
                    &runtime_adapter,
                    !verify_chat_completion,
                ))
                .await
        } else {
            let chat_future = parameter_capture.scope(provider.chat_traced(
                ChatRequest {
                    messages: &prepared_messages.messages,
                    tools: request_tools,
                },
                model,
                temperature,
            ));
            if let Some(token) = cancellation_token.as_ref() {
                tokio::select! {
                    () = token.cancelled() => {
                        if let Some(observation) = provider_observation {
                            observation.cancelled();
                        }
                        if let Some(retained) = &runtime_adapter.provider_trace {
                            retained.failed();
                        }
                        return Err(ToolLoopCancelled.into());
                    },
                    result = chat_future => result,
                }
            } else {
                chat_future.await
            }
        };
        let prepared = parameter_capture.snapshot_from(last_turn_trace.prepared_parameters.len());
        last_turn_trace.prepared_parameters.extend(prepared);

        if let Some(observation) = provider_observation {
            observation.finish(chat_result.as_ref().ok().map(|trace| trace.attempts.as_slice()));
        }

        // A completed provider round-trip is progress even on the buffered
        // path, where no intermediate signal exists to observe.
        crate::agent::idle::beat(crate::agent::idle::ProgressKind::ProviderResponse);

        // P0-1: chat_processed is Result so we can detect context overflow below.
        if chat_result.is_err()
            && let Some(retained) = &runtime_adapter.provider_trace
        {
            retained.failed();
        }
        let chat_processed = match chat_result {
            Ok(trace) => {
                // FIX #2: a turn recovered via fallback when its trace recorded
                // more than one attempt (each failed attempt before the terminal
                // success adds an entry) OR when the serving provider/model
                // deviated from the routed selection for this turn — a single
                // attempt can still land on a non-routed provider/model (e.g. a
                // sticky/affinity reroute). This mirrors `from_trace`'s fallback
                // predicate (`attempts.len() > 1 || final_provider != selected ||
                // final_model != selected`), where `provider_name`/`model` here
                // are this turn's routed selection. Sticky across turns.
                let turn_had_fallback =
                    trace.attempts.len() > 1 || trace.final_provider != provider_name || trace.final_model != model;
                if turn_had_fallback {
                    any_turn_had_fallback = true;
                }
                usage_accumulator.record(trace.tokens_used.clone());
                // Record this turn's real attribution. Overwritten each turn;
                // the value at the returning turn is what the caller receives.
                // The served member is named by its unique key when the chain
                // reports one: display names of members that differ only in
                // their password are equal (D-068 (2)).
                execution_adapter.tool_execution_context.schema_dialect = provider
                    .tool_schema_dialect_of_member(
                        trace.served_member.as_deref().unwrap_or(&trace.final_provider),
                        &trace.final_model,
                    )
                    .unwrap_or_else(|| provider.tool_schema_dialect_for(&trace.final_model));
                last_turn_trace = ToolLoopTrace {
                    prepared_parameters: std::mem::take(&mut last_turn_trace.prepared_parameters),
                    final_provider: Some(trace.final_provider),
                    final_model: Some(trace.final_model),
                    attempts: trace.attempts,
                    // Folded in just before returning so it reflects all turns.
                    any_turn_had_fallback: false,
                    tokens_used: TokenUsage::default(),
                    execution_trace: None,
                };
                if let Some(retained) = &runtime_adapter.provider_trace {
                    retained.completed(
                        &last_turn_trace,
                        usage_accumulator.finish(),
                        any_turn_had_fallback,
                        trace.tokens_used.is_reported() && trace.tokens_used.has_any_tokens(),
                    );
                }
                #[allow(unused_mut)]
                let mut resp = trace.response;

                #[cfg(feature = "wasm-plugins")]
                {
                    let transformed = hooks
                        .process_wasm_middleware(
                            crate::plugins::capabilities::middleware::MiddlewareStage::LlmResponse,
                            serde_json::json!({
                                "provider": provider_name,
                                "model": model,
                                "text": resp.text,
                                "tool_calls": resp.tool_calls,
                                "reasoning_content": resp.reasoning_content,
                            }),
                        )
                        .await;
                    if let Some(text) = transformed.get("text") {
                        resp.text = if text.is_null() {
                            None
                        } else {
                            text.as_str().map(ToString::to_string).or(resp.text)
                        };
                    }
                    if let Some(tool_calls) = transformed.get("tool_calls")
                        && let Ok(candidate) = serde_json::from_value(tool_calls.clone())
                    {
                        resp.tool_calls = candidate;
                    }
                    if let Some(reasoning) = transformed.get("reasoning_content") {
                        resp.reasoning_content = if reasoning.is_null() {
                            None
                        } else {
                            reasoning.as_str().map(ToString::to_string).or(resp.reasoning_content)
                        };
                    }
                }
                let duration = llm_started_at.elapsed();
                hooks
                    .emit(
                        HookEvent::LlmResponse,
                        serde_json::json!({
                            "provider": provider_name,
                            "model": model,
                            "duration_ms": duration.as_millis(),
                            "success": true,
                            "error_message": serde_json::Value::Null,
                        }),
                    )
                    .await;
                observer.record_event(&ObserverEvent::LlmResponse {
                    provider: provider_name.to_string(),
                    model: model.to_string(),
                    duration,
                    success: true,
                    error_message: None,
                });

                let response_text = resp.text_or_empty().to_string();
                // First try native structured tool calls (OpenAI-format).
                // Fall back to text-based parsing (XML tags, markdown blocks,
                // GLM format) only if the provider returned no native calls —
                // this ensures we support both native and prompt-guided models.
                let mut calls = parse_structured_tool_calls(&resp.tool_calls);
                let mut parsed_text = String::new();

                if calls.is_empty() {
                    let (fallback_text, fallback_calls) = parse_tool_calls(&response_text);
                    if !fallback_text.is_empty() {
                        parsed_text = fallback_text;
                    }
                    calls = fallback_calls;
                    if calls.is_empty() && looks_like_unparsed_tool_call_syntax(&response_text) {
                        let sanitized_preview = sanitize_tool_parse_log_preview(&response_text);
                        tracing::error!(
                            raw_response = sanitized_preview.as_str(),
                            "Failed to parse model tool-call syntax; suppressing raw tool-call text"
                        );
                        parsed_text = "tool execution failed".to_string();
                    }
                }

                // Preserve native tool call IDs in assistant history so role=tool
                // follow-up messages can reference the exact call id.
                // A model can write `[Used tools: ...]` in the very same
                // response that carries real tool calls; that text goes to
                // history untouched by the final-reply guard below (which only
                // sees turns without tool calls), so strip it here before it can
                // be replayed, compacted or summarized. Inline-aware: a marker
                // this branch leaves in a sentence is replayed on every later
                // turn, which is exactly how the shape gets imitated.
                let history_text = strip_assistant_fabricated_tool_markers(&response_text);
                let assistant_history_content = if resp.tool_calls.is_empty() {
                    history_text.into_owned()
                } else {
                    build_native_assistant_history(&history_text, &resp.tool_calls, resp.reasoning_content.as_deref())
                };

                let native_calls = resp.tool_calls;
                Ok((
                    response_text,
                    parsed_text,
                    calls,
                    assistant_history_content,
                    native_calls,
                ))
            }
            Err(e) => {
                let duration = llm_started_at.elapsed();
                let error_message = crate::providers::sanitize_api_error(&e.to_string());
                hooks
                    .emit(
                        HookEvent::LlmResponse,
                        serde_json::json!({
                            "provider": provider_name,
                            "model": model,
                            "duration_ms": duration.as_millis(),
                            "success": false,
                            "error_message": error_message,
                        }),
                    )
                    .await;
                observer.record_event(&ObserverEvent::LlmResponse {
                    provider: provider_name.to_string(),
                    model: model.to_string(),
                    duration,
                    success: false,
                    error_message: Some(error_message.clone()),
                });
                hooks.emit(HookEvent::Error, payload_error("llm", &error_message)).await;
                Err(e)
            }
        };

        // On context overflow, compact and retry for as long as each pass makes
        // measurable progress. This has no arbitrary retry ceiling and cannot
        // spin forever once history reaches its minimal representation.
        let (response_text, parsed_text, mut tool_calls, assistant_history_content, native_tool_calls) =
            match chat_processed {
                // A stream that failed after model output is never compacted
                // and replayed: the unsafe-replay refusal it carries stands.
                Err(ref e)
                    if crate::recovery::overflow::is_context_overflow_error(e)
                        && !is_stream_failure_after_model_output(e) =>
                {
                    let messages_before_compaction = history.len();
                    let tokens_before_compaction = estimate_history_tokens(history);
                    tracing::warn!("Context overflow detected; running compaction and retrying LLM call");
                    let mut emitted_exact_patch = false;
                    if let Some(config) = compaction_config {
                        if runtime_adapter.stream_provider {
                            let turns_before = history.len().saturating_sub(usize::from(
                                history.first().is_some_and(|message| message.role == "system"),
                            ));
                            let tokens_before = estimate_history_tokens(history);
                            match build_configurable_compaction_patch(
                                history,
                                provider,
                                model,
                                config,
                                document_ingest.as_ref(),
                                "overflow_retry",
                            )
                            .await
                            {
                                Ok(Some(patch)) => {
                                    let replacement_len = patch.replacement.len();
                                    apply_compaction_patch_exact(history, &patch);
                                    let switch_mode = matches!(config.mode, crate::config::AgentCompactionMode::Switch);
                                    let before_trim = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
                                    if !switch_mode || before_trim.over_hard_limit {
                                        trim_history_to_context_budget_preserving_compaction_replacement_with_floor(
                                            history,
                                            config,
                                            replacement_len,
                                        );
                                    }
                                    if switch_mode && before_trim.over_hard_limit {
                                        let after_trim = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
                                        tracing::warn!(
                                            before_used_tokens = before_trim.used_tokens,
                                            after_used_tokens = after_trim.used_tokens,
                                            "context switch retry stayed above the hard limit; degraded to a lossy trim"
                                        );
                                        persist_context_switch_degradation(
                                            document_ingest.as_ref(),
                                            "overflow_retry",
                                            "over_hard_limit_after_handoff",
                                            before_trim.used_tokens,
                                            after_trim.used_tokens,
                                            after_trim.available_input_tokens,
                                        )
                                        .await;
                                    }
                                    let turns_after = history.len().saturating_sub(usize::from(
                                        history.first().is_some_and(|message| message.role == "system"),
                                    ));
                                    let tokens_after = estimate_history_tokens(history);
                                    let used_context_tokens =
                                        plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD).used_tokens;
                                    emit_tool_loop_event(
                                        &runtime_adapter,
                                        ToolLoopEvent::ContextCompactionPatch {
                                            patch,
                                            config: config.clone(),
                                            turns_before,
                                            tokens_before,
                                            turns_after,
                                            tokens_after,
                                            used_context_tokens,
                                        },
                                    )
                                    .await?;
                                    emitted_exact_patch = true;
                                }
                                Ok(None) => {
                                    let before_trim = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
                                    trim_history_to_context_budget(history, config);
                                    if history.len() == messages_before_compaction
                                        && estimate_history_tokens(history) >= tokens_before_compaction
                                    {
                                        apply_aggressive_trim(history, config.keep_recent_messages);
                                    }
                                    if matches!(config.mode, crate::config::AgentCompactionMode::Switch) {
                                        let after_trim = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
                                        tracing::warn!(
                                            "context switch retry could not create an exact handoff; degraded to a lossy trim"
                                        );
                                        persist_context_switch_degradation(
                                            document_ingest.as_ref(),
                                            "overflow_retry",
                                            "handoff_unavailable",
                                            before_trim.used_tokens,
                                            after_trim.used_tokens,
                                            after_trim.available_input_tokens,
                                        )
                                        .await;
                                    }
                                }
                                Err(error) => {
                                    tracing::warn!("Overflow retry compaction failed: {error}");
                                    let before_trim = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
                                    apply_aggressive_trim(history, config.keep_recent_messages);
                                    if matches!(config.mode, crate::config::AgentCompactionMode::Switch) {
                                        let after_trim = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
                                        persist_context_switch_degradation(
                                            document_ingest.as_ref(),
                                            "overflow_retry",
                                            "handoff_failed",
                                            before_trim.used_tokens,
                                            after_trim.used_tokens,
                                            after_trim.available_input_tokens,
                                        )
                                        .await;
                                    }
                                }
                            }
                        } else {
                            match apply_configurable_compaction(
                                history,
                                provider,
                                model,
                                config,
                                document_ingest.as_ref(),
                                "overflow_retry",
                            )
                            .await
                            {
                                Ok(true) => {}
                                Ok(false) => {
                                    let before_trim = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
                                    apply_aggressive_trim(history, config.keep_recent_messages);
                                    if matches!(config.mode, crate::config::AgentCompactionMode::Switch) {
                                        let after_trim = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
                                        persist_context_switch_degradation(
                                            document_ingest.as_ref(),
                                            "overflow_retry",
                                            "handoff_unavailable",
                                            before_trim.used_tokens,
                                            after_trim.used_tokens,
                                            after_trim.available_input_tokens,
                                        )
                                        .await;
                                    }
                                }
                                Err(error) => {
                                    tracing::warn!("Overflow retry compaction failed: {error}");
                                    let before_trim = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
                                    apply_aggressive_trim(history, config.keep_recent_messages);
                                    if matches!(config.mode, crate::config::AgentCompactionMode::Switch) {
                                        let after_trim = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
                                        persist_context_switch_degradation(
                                            document_ingest.as_ref(),
                                            "overflow_retry",
                                            "handoff_failed",
                                            before_trim.used_tokens,
                                            after_trim.used_tokens,
                                            after_trim.available_input_tokens,
                                        )
                                        .await;
                                    }
                                }
                            }
                        }
                    } else {
                        apply_aggressive_trim(history, COMPACTION_KEEP_RECENT_MESSAGES);
                    }
                    if !(emitted_exact_patch || runtime_adapter.stream_provider && compaction_config.is_some()) {
                        emit_tool_loop_event(&runtime_adapter, ToolLoopEvent::ContextCompacted).await?;
                    }
                    let made_progress = history.len() < messages_before_compaction
                        || estimate_history_tokens(history) < tokens_before_compaction;
                    if !made_progress {
                        anyhow::bail!("context overflow persisted and compaction made no progress: {e}");
                    }
                    continue;
                }
                Err(e) => return Err(e),
                Ok(values) => values,
            };

        #[allow(unused_mut)]
        let mut display_text = if parsed_text.is_empty() {
            response_text.clone()
        } else {
            parsed_text
        };

        if tool_calls.is_empty() {
            // No tool calls — this is the final response.
            if is_tool_summary_only_response(&response_text) {
                if tool_summary_retry_attempted {
                    anyhow::bail!("model returned a tool-summary-only response after one corrective retry");
                }
                tracing::warn!(
                    provider = %crate::providers::redact_endpoint_userinfo(provider_name),
                    model,
                    "model returned a tool-summary-only response; retrying once with a corrective instruction"
                );
                tool_summary_retry_attempted = true;
                inject_tool_summary_retry_instruction = true;
                continue;
            }
            if is_empty_assistant_response(&response_text, false) {
                if empty_response_retry_attempted {
                    anyhow::bail!("model returned an empty response after one corrective retry");
                }
                tracing::warn!(
                    provider = %crate::providers::redact_endpoint_userinfo(provider_name),
                    model,
                    "model returned empty assistant response; retrying once with a corrective instruction"
                );
                empty_response_retry_attempted = true;
                inject_empty_response_retry_instruction = true;
                continue;
            }
            // Fabricated tool-use marker guard. A `[Used tools: ...]` line the
            // model wrote while this turn never dispatched a tool is a claim
            // about work that did not happen; spend the one corrective retry on
            // it (the same budget the summary-only path uses, so a response that
            // is nothing but a marker still costs at most one retry).
            let fabricated_marker = contains_assistant_fabricated_tool_marker(&response_text)
                || contains_assistant_fabricated_tool_marker(&display_text);
            if fabricated_marker && !executed_any_tool_call && !tool_summary_retry_attempted {
                tracing::warn!(
                    provider = %crate::providers::redact_endpoint_userinfo(provider_name),
                    model,
                    "model claimed tool use without executing any tool; retrying once with a corrective instruction"
                );
                tool_summary_retry_attempted = true;
                inject_tool_summary_retry_instruction = true;
                continue;
            }
            if fabricated_marker {
                // Retry spent (or tools really ran): keep whatever the model
                // actually answered and drop only the marker lines, so the
                // fabrication never reaches the user, the history, or the next
                // turn's prompt where it would be imitated again.
                if let Cow::Owned(cleaned) = strip_assistant_fabricated_tool_markers(&display_text) {
                    display_text = cleaned;
                }
                if display_text.trim().is_empty() {
                    anyhow::bail!("model returned a tool-summary-only response after one corrective retry");
                }
            }
            if verify_chat_completion {
                use super::completion_review::{CompletionDecision, ReplyAssessment};
                use super::execution_requirement::{ExecutionRequirement, assessment_messages, parse_assessment};
                let assessment = if !observed_tool_results && let Some(requirement) = execution_requirement {
                    ReplyAssessment::Execution(requirement)
                } else {
                    // Keep the assessment and persistence future off the turn's
                    // already large stack frame, including debug/PTY builds.
                    Box::pin(async {
                    let budget = compaction_config.map_or(8192, |config| {
                        config.max_context_tokens.saturating_sub(config.reserve_tokens)
                    });
                    let source = assessment_source.as_deref().unwrap_or(history);
                    let assessment = if observed_tool_results {
                        completion_evidence.messages(source, &display_text, budget)?
                    } else {
                        assessment_messages(source, budget)?
                    };
                    let purpose = if observed_tool_results { "completion_review" } else { "execution_requirement" };
                    if let (Some(fabric), Some(scope)) = (event_fabric.as_ref(), request_event_scope.as_ref()) {
                        record_assessment_request(
                            fabric, scope.clone(), provider_name, model, purpose, &assessment,
                        ).await?;
                    }
                    let observation = execution_adapter
                        .tool_execution_context
                        .execution_trace
                        .as_ref()
                        .map(|trace| trace.begin_provider())
                        .transpose()?;
                    if let Some(retained) = &runtime_adapter.provider_trace {
                        retained.begin();
                    }
                    let assessment_started = Instant::now();
                    observer.record_event(&ObserverEvent::LlmRequest {
                        provider: provider_name.to_string(),
                        model: model.to_string(),
                        messages_count: assessment.len(),
                    });
                    hooks
                        .emit(
                            HookEvent::LlmRequest,
                            serde_json::json!({
                                "provider":provider_name, "model":model, "purpose":purpose,
                                "messages_count":assessment.len(), "tool_count":0,
                            }),
                        )
                        .await;
                    let assessment_future = parameter_capture.scope(provider.chat_traced(
                        ChatRequest {
                            messages: &assessment,
                            tools: None,
                        },
                        model,
                        0.0,
                    ));
                    let assessment_result = if let Some(token) = cancellation_token.as_ref() {
                        tokio::select! {
                            () = token.cancelled() => {
                                if let Some(observation) = observation { observation.cancelled(); }
                                if let Some(retained) = &runtime_adapter.provider_trace { retained.failed(); }
                                return Err(ToolLoopCancelled.into());
                            }
                            result = assessment_future => result,
                        }
                    } else {
                        assessment_future.await
                    };
                    if let Some(observation) = observation {
                        observation.finish(assessment_result.as_ref().ok().map(|trace| trace.attempts.as_slice()));
                    }
                    crate::agent::idle::beat(crate::agent::idle::ProgressKind::ProviderResponse);
                    let duration = assessment_started.elapsed();
                    let error_message = assessment_result.as_ref().err().map(ToString::to_string);
                    observer.record_event(&ObserverEvent::LlmResponse {
                        provider: provider_name.to_string(),
                        model: model.to_string(),
                        duration,
                        success: assessment_result.is_ok(),
                        error_message: error_message.clone(),
                    });
                    hooks.emit(HookEvent::LlmResponse, serde_json::json!({
                        "provider":provider_name, "model":model, "purpose":purpose,
                        "duration_ms":duration.as_millis(), "success":assessment_result.is_ok(), "error_message":error_message,
                    })).await;
                    if assessment_result.is_err()
                        && let Some(retained) = &runtime_adapter.provider_trace
                    {
                        retained.failed();
                    }
                    let trace = assessment_result?;
                    usage_accumulator.record(trace.tokens_used.clone());
                    any_turn_had_fallback |=
                        trace.attempts.len() > 1 || trace.final_provider != provider_name || trace.final_model != model;
                    last_turn_trace
                        .prepared_parameters
                        .extend(parameter_capture.snapshot_from(last_turn_trace.prepared_parameters.len()));
                    if let Some(retained) = &runtime_adapter.provider_trace {
                        retained.completed(
                            &last_turn_trace,
                            usage_accumulator.finish(),
                            any_turn_had_fallback,
                            trace.tokens_used.is_reported()
                                && trace.tokens_used.has_any_tokens()
                                && !trace
                                    .attempts
                                    .iter()
                                    .any(|attempt| attempt.status == crate::llm::route_decision::AttemptStatus::Failed),
                        );
                    }
                    let assessment = if observed_tool_results {
                        ReplyAssessment::Completion(super::completion_review::parse_review(&trace.response)?)
                    } else {
                        ReplyAssessment::Execution(parse_assessment(&trace.response)?)
                    };
                    if let (Some(fabric), Some(scope)) = (event_fabric.as_ref(), request_event_scope.as_ref()) {
                        let payload = serde_json::json!({
                            "assessment": format!("{assessment:?}"),
                            "requirement": match &assessment {
                                ReplyAssessment::Execution(requirement) => Some(format!("{requirement:?}")),
                                ReplyAssessment::Completion(_) => None,
                            },
                            "provider": crate::providers::redact_endpoint_userinfo(&trace.final_provider),
                            "model": trace.final_model,
                            "tool_results_observed": observed_tool_results,
                        });
                        if let Err(error) = fabric
                            .record_runtime_event(
                                scope.clone(),
                                if observed_tool_results { "turn.completion_review.assessed" } else { "turn.execution_requirement.assessed" },
                                "Chat reply assessed".to_string(),
                                Some(payload.to_string()),
                            )
                            .await
                        {
                            tracing::warn!(%error, "Failed to record execution requirement assessment");
                        }
                    }
                        Ok::<_, anyhow::Error>(assessment)
                    }).await?
                };
                match assessment {
                    ReplyAssessment::Execution(ExecutionRequirement::ExecutionRequired) => {
                        anyhow::ensure!(
                            execution_requirement.is_none(),
                            "request requires execution but no tool result was produced after one corrective retry; execution is unverified"
                        );
                        execution_requirement = Some(ExecutionRequirement::ExecutionRequired);
                        inject_execution_retry = true;
                        continue;
                    }
                    ReplyAssessment::Execution(ExecutionRequirement::Uncertain) => {
                        anyhow::bail!("execution requirement is uncertain; execution is unverified")
                    }
                    ReplyAssessment::Execution(ExecutionRequirement::Informational) => {}
                    ReplyAssessment::Completion(review) => match review.decision {
                        CompletionDecision::Accept => {}
                        CompletionDecision::Revise => {
                            anyhow::ensure!(
                                completion_correction.is_none(),
                                "completion remains unverified after one corrective attempt: {}",
                                review.reason
                            );
                            completion_correction = Some(super::completion_review::corrective_message(&review.reason));
                            continue;
                        }
                        CompletionDecision::Uncertain => anyhow::bail!(
                            "completion evidence is uncertain; completion is unverified: {}",
                            review.reason
                        ),
                    },
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
            {
                // Braced so that a `#[cfg(test)]`-item scanner (the source guard
                // `outbound_middleware_has_one_production_owner`) can tell this
                // test-only counter from the production call below it.
                #[cfg(test)]
                {
                    OUTBOUND_MIDDLEWARE_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
                }
                let transformed = hooks
                    .process_wasm_middleware(
                        crate::plugins::capabilities::middleware::MiddlewareStage::Outbound,
                        serde_json::json!({
                            "channel": channel_name,
                            "text": display_text,
                            "has_tool_calls": false,
                        }),
                    )
                    .await;
                if let Some(text) = transformed.get("text").and_then(serde_json::Value::as_str) {
                    display_text = finalize_outbound_display_text(&display_text, text).into_owned();
                }
            }
            // If a streaming sender is provided, relay the text in small chunks
            // so the channel can progressively update the draft message.
            if runtime_adapter.stream_provider && verify_chat_completion {
                emit_tool_loop_event(&runtime_adapter, ToolLoopEvent::TextDelta(display_text.clone())).await?;
            }
            if let Some(ref tx) = on_delta {
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
            // FIX-P0-30/31: returning turn — hand back this turn's real
            // provider attribution alongside the final answer.
            // FIX #2: fold the cross-turn fallback flag into the returned trace so
            // a fallback on any earlier turn is reflected in the recorded status.
            last_turn_trace.any_turn_had_fallback = any_turn_had_fallback;
            last_turn_trace.tokens_used = usage_accumulator.finish();
            last_turn_trace.execution_trace = Some(context.terminal_execution_trace());
            return Ok((ToolLoopOutcome::Text(display_text), last_turn_trace));
        }

        // A streaming ingress cannot execute a structured provider tool call
        // unless it supplied a registry. Fail at the ownership boundary rather
        // than manufacturing repeated unknown-tool results until the iteration
        // cap is exhausted. This preserves the Redux driver's permanent,
        // immediate no-registry rejection contract.
        if runtime_adapter.stream_provider && tools_registry.is_empty() {
            return Err(anyhow::anyhow!(
                "provider returned tool_calls but no tools_registry is configured"
            ));
        }

        // Smart group-reply: a `stay_silent` call is a *terminal* decision for the
        // turn. Detect it BEFORE executing any tool and BEFORE pushing the
        // assistant/tool turn into history, so a silent turn leaves zero trace in
        // the conversation history and produces no channel output. Only honored
        // when the caller advertised the tool (`expose_stay_silent`), which is
        // restricted to smart group turns; this keeps DMs / non-smart turns from
        // ever short-circuiting to Silent even if a model hallucinates the name.
        if expose_stay_silent && compiled_tool_context.allows(crate::tools::STAY_SILENT_TOOL_NAME) {
            if let Some(silent_call) = tool_calls
                .iter()
                .find(|call| call.name == crate::tools::STAY_SILENT_TOOL_NAME)
            {
                let reason = crate::tools::stay_silent::extract_reason(&silent_call.arguments);
                tracing::info!(reason = reason.as_str(), "tool loop: model chose to stay silent");
                if cancellation_token.as_ref().is_some_and(CancellationToken::is_cancelled) {
                    return Err(ToolLoopCancelled.into());
                }
                service.record_agent_silence(proof, context, &compiled_tool_context)?;
                last_turn_trace.any_turn_had_fallback = any_turn_had_fallback;
                last_turn_trace.tokens_used = usage_accumulator.finish();
                last_turn_trace.execution_trace = Some(context.terminal_execution_trace());
                return Ok((ToolLoopOutcome::Silent { reason }, last_turn_trace));
            }
        }

        // A model may call a previously returned hit by public name or exact
        // capability ID. Admit that specific offer before widening this batch's
        // dispatch snapshot; the provider snapshot above remains immutable.
        let mut discovery_dispatch_specs: Option<Vec<crate::tools::ToolSpec>> = None;
        for call in &mut tool_calls {
            if compiled_tool_context.allows(&call.name) {
                continue;
            }
            let Some(proposal) = service.prepare_discovered_tool(proof, context, &call.name)? else {
                continue;
            };
            if inherited_exclusions
                .iter()
                .any(|excluded| excluded == &proposal.spec.name || excluded == &proposal.root)
            {
                continue;
            }
            let Some(proposal) = service.admit_discovered_tool(proof, context, &call.name)? else {
                continue;
            };
            if matches!(
                proposal.admission,
                crate::causal_tree::gates::GateOutcome::Blocked { .. }
            ) {
                continue;
            }
            call.name.clone_from(&proposal.spec.name);
            let specs = discovery_dispatch_specs.get_or_insert_with(|| compiled_tool_context.tool_specs().to_vec());
            if !specs.iter().any(|spec| spec.name == proposal.spec.name) {
                specs.push(proposal.spec);
            }
        }
        if let Some(specs) = discovery_dispatch_specs {
            let (execution, bound_context) = service.bind_agent_execution_with_runtime(
                proof,
                context,
                &execution_adapter.tool_execution_context,
                &specs,
                execution_adapter.tool_execution_service.clone(),
            )?;
            execution_adapter.allowed_tool_names = Some(std::sync::Arc::new(
                specs.iter().map(|spec| spec.name.clone()).collect(),
            ));
            execution_adapter.tool_execution_service = Some(execution);
            execution_adapter.tool_execution_context = bound_context;
        }

        // Print any text the LLM produced alongside tool calls (unless silent)
        if !silent && !display_text.is_empty() {
            print!("{display_text}");
            let _ = std::io::stdout().flush();
        }

        // Execute tool calls and build results. `individual_results` tracks per-call output so
        // native-mode history can emit one role=tool message per tool call with the correct ID.
        //
        // Conservative scheduler:
        // - run read-only tools in small bounded batches;
        // - keep all stateful tools strictly serial.
        let mut tool_results = String::new();
        for call in &tool_calls {
            hooks
                .emit(
                    HookEvent::ToolCallStart,
                    serde_json::json!({
                        "tool": call.name,
                        "arguments": redact_sensitive_json_keys(&call.arguments),
                    }),
                )
                .await;
            if let Some(ref tx) = on_tool_call {
                let args_str = scrub_credentials(&call.arguments.to_string());
                let args_summary = truncate_with_ellipsis(&args_str, 80);
                let _ = tx
                    .send(ToolCallNotification::Started {
                        name: call.name.clone(),
                        args_summary,
                    })
                    .await;
            }
        }

        let tools_started_at = Instant::now();
        let recorder = ToolResultRecorder::new(tool_calls.len());
        // The assistant message that *requested* these calls goes into history
        // before any of them runs. Written after the batch, as it used to be, an
        // interruption partway through the batch left the calls that had already
        // run — and already had side effects — with nothing in history to attach
        // to, so a replay repeated every one of them.
        history.push(ChatMessage::assistant(assistant_history_content));
        let executed = ExecutedToolRecords::new(history, &recorder, &tool_calls, &native_tool_calls);

        let individual_results = match execute_tools_with_service(
            &tool_calls,
            &native_tool_calls,
            &execution_adapter,
            read_only_schedule.clone(),
            cancellation_token.as_ref(),
            chat_mode,
            observer,
            &recorder,
        )
        .await
        {
            Ok(results) => results,
            Err(error) => {
                if let Some(pending) = error.downcast_ref::<super::turn_contracts::ToolLoopAwaitingApproval>() {
                    // Dropping the recorder guard retains completed calls and
                    // explicit cancellation placeholders for unexecuted siblings.
                    drop(executed);
                    last_turn_trace.any_turn_had_fallback = any_turn_had_fallback;
                    last_turn_trace.tokens_used = usage_accumulator.finish();
                    last_turn_trace.execution_trace = Some(context.terminal_execution_trace());
                    return Ok((
                        ToolLoopOutcome::AwaitingApproval {
                            reply: pending.reply.clone(),
                        },
                        last_turn_trace,
                    ));
                }
                return Err(error);
            }
        };
        observed_tool_results |= !individual_results.is_empty();
        if runtime_adapter.verify_chat_completion {
            for (call, result) in tool_calls.iter().zip(&individual_results) {
                completion_evidence.record(call, result);
            }
        }
        let tools_elapsed_ms = tools_started_at.elapsed().as_millis() as u64;

        // Only a batch that actually produced a result counts as real tool use.
        // Dispatching is not enough: a batch whose every call was denied by the
        // permission layer or the idempotency ledger did no work, so a later
        // `[Used tools: ...]` line is still provably fabricated and still worth
        // the one corrective retry.
        if individual_results.iter().any(|result| result.executed) {
            executed_any_tool_call = true;
        }

        // FIX-P2-02: persistence (untrusted external copy + oversized tool_output
        // document) happens exactly once per tool result, in this loop, for every
        // mode. The native-history branch below reuses the document status captured
        // here instead of persisting a second time.
        let mut document_statuses: Vec<&'static str> = Vec::with_capacity(individual_results.len());
        let mut repeated_unrecoverable: Option<String> = None;
        // Everything below that plans against the window plans against the
        // reserved one, the same window the preflight used: this turn's schemas
        // and the prompt sections compiled from them occupy it before a single
        // tool result does. Built once per tool round and shared by the inline
        // caps and the mid-turn trim.
        let reserved_config = compaction_config.map(|config| reserve_request_tool_surface(config, tool_surface));
        for (call, outcome) in tool_calls.iter().zip(individual_results.iter()) {
            let success = outcome.succeeded();
            let result = &outcome.content;
            hooks
                .emit(
                    HookEvent::ToolCall,
                    serde_json::json!({
                        "tool": call.name,
                        "success": success,
                        "output": result,
                    }),
                )
                .await;
            if let Some(ref tx) = on_tool_call {
                let _ = tx
                    .send(ToolCallNotification::Finished {
                        name: call.name.clone(),
                        success,
                        duration_ms: tools_elapsed_ms,
                    })
                    .await;
            }
            if !success {
                hooks.emit(HookEvent::Error, payload_error("tool", result)).await;
                if shared_unrecoverable_tool_error(result) {
                    let signature = format!("{}:{}", call.name, result.to_ascii_lowercase());
                    let count = unrecoverable_failures.entry(signature).or_insert(0);
                    *count = count.saturating_add(1);
                    if *count >= 2 {
                        repeated_unrecoverable = Some(call.name.clone());
                    }
                }
            }

            // Track consecutive failures per tool for repeated-failure hints.
            let repeated_hint = if success {
                consecutive_failures.remove(&call.name);
                String::new()
            } else {
                let count = consecutive_failures.entry(call.name.clone()).or_insert(0);
                *count += 1;
                if *count >= 2 {
                    format!(
                        "\n[System: tool '{}' has failed {} times consecutively. Consider a different approach or tool.]",
                        call.name, count
                    )
                } else {
                    String::new()
                }
            };

            // FIX-P2-02: keep a verbatim, read-only, untrusted copy of successful
            // external content fetches (web_fetch / http_request / file_read).
            if success {
                persist_untrusted_external_copy(document_ingest.as_ref(), &call.name, result).await;
            }

            // P0-8: Keep history bounded and persist oversized outputs in the
            // document layer when a runtime document sink is available.
            let document_status = persist_tool_output_document(
                document_ingest.as_ref(),
                &call.name,
                result,
                MAX_TOOL_RESULT_INLINE_CHARS,
            )
            .await
            .unwrap_or("pending_document_backend");
            // FIX-P2-02: record the status so the native-history branch can reuse
            // it without persisting the same document a second time.
            document_statuses.push(document_status);
            let max_inline_chars =
                tool_result_inline_budget_for_history(&call.name, executed.history(), reserved_config.as_ref());
            let truncated_result =
                compact_tool_result_for_history_with_status(&call.name, result, max_inline_chars, document_status);
            let _ = writeln!(
                tool_results,
                "<tool_result name=\"{}\">\n{}{}\n</tool_result>",
                call.name, truncated_result, repeated_hint
            );
        }

        // Tool results follow the assistant message already in history.
        // Native mode: use JSON-structured messages so convert_messages() can
        // reconstruct proper OpenAI-format tool_calls and tool result messages.
        // Prompt mode: use XML-based text format as before.
        let mut result_messages: Vec<ChatMessage> = Vec::with_capacity(native_tool_calls.len().max(1));
        if native_tool_calls.is_empty() {
            // Prompt/text mode marker. Shares the prefix constant with the CTE
            // snapshot parser (`build_causal_state_from_chat`) so a wording change
            // stays consistent across producer and parser.
            result_messages.push(ChatMessage::user(format!(
                "{}\n{tool_results}",
                crate::agent::turn_contracts::TOOL_RESULTS_PREFIX
            )));
        } else {
            for (index, (native_call, result)) in native_tool_calls.iter().zip(individual_results.iter()).enumerate() {
                // FIX-P2-02: persistence (untrusted external copy + oversized
                // tool_output document) already ran exactly once in the loop above,
                // which iterates the same `individual_results` in order. Reuse the
                // captured document status here instead of persisting a second time
                // (the previous duplicate write doubled I/O and storage for native
                // mode). Fall back defensively if the status vector is shorter than
                // expected (positional mismatch should not happen in practice).
                let document_status = document_statuses
                    .get(index)
                    .copied()
                    .unwrap_or("pending_document_backend");
                let max_inline_chars = tool_result_inline_budget_for_history(
                    &native_call.name,
                    executed.history(),
                    reserved_config.as_ref(),
                );
                let truncated_result = compact_tool_result_for_history_with_status(
                    &native_call.name,
                    &result.content,
                    max_inline_chars,
                    document_status,
                );
                let tool_msg = serde_json::json!({
                    "tool_call_id": native_call.id,
                    "content": truncated_result,
                });
                result_messages.push(ChatMessage::tool(tool_msg.to_string()));
            }
        }
        executed.commit(result_messages);

        // P1-4: Token-aware mid-turn trim (primary) + count-based safety net (secondary).
        //
        // Against the same reserved window the preflight uses: the schemas and
        // the appended prompt sections of this turn occupy it too, so trimming
        // to the unreserved limit here would hand the next iteration's
        // preflight history it has to compact when a trim would have done.
        //
        // Under the same request floor the preflight has. This trim removes the
        // oldest non-system message until the history fits, and the turn's own
        // request is the oldest thing in it: on a saturated window it goes
        // first, *before* the next iteration's preflight can capture it, after
        // which that preflight has nothing to put back and the turn runs on
        // without the question it was asked. Captured inside the branch, so a
        // round that needs no trim still clones nothing.
        if let Some(reserved_config) = reserved_config.as_ref() {
            let mid_turn_tokens = estimate_history_tokens(history);
            let mid_turn_limit = reserved_config
                .max_context_tokens
                .saturating_sub(reserved_config.reserve_tokens);
            if mid_turn_tokens > mid_turn_limit {
                tracing::warn!(mid_turn_tokens, mid_turn_limit, "Mid-turn token trim triggered");
                let current_request = capture_current_turn_request(history);
                trim_history_token_aware(history, mid_turn_limit);
                restore_current_turn_request(
                    history,
                    current_request,
                    "mid_turn_trim",
                    RestoredRequestPosition::AboveEverythingLeftStanding,
                );
            }
        }
        if let Some(tool_name) = repeated_unrecoverable {
            anyhow::bail!(
                "stopped after a repeated unrecoverable tool failure ('{tool_name}' is blocked by policy/permissions); not retrying further"
            );
        }
    }
}

/// Account for the separate, tool-free assessment request as well as task
/// requests. Persist only measurements and purpose, never the supplied evidence.
async fn record_assessment_request(
    fabric: &crate::memory::MemoryFabric,
    scope: crate::memory::MessageEventScope,
    provider: &str,
    model: &str,
    purpose: &str,
    messages: &[ChatMessage],
) -> anyhow::Result<()> {
    use super::context_budget::{budgeted_tokens, estimate_history_tokens_with_source, token_source_label};
    let measurement = estimate_history_tokens_with_source(messages);
    let tokens = budgeted_tokens(measurement);
    let bytes = messages.iter().fold(0usize, |total, message| {
        total
            .saturating_add(message.role.len())
            .saturating_add(message.content.len())
    });
    fabric
        .record_runtime_event(
            scope.with_recipient(format!("{provider}/{model}")),
            "llm.request.context.compiled",
            format!("{purpose}; no executable capabilities"),
            Some(
                serde_json::json!({
                    "schema_version": 1,
                    "purpose": purpose,
                    "provider": crate::providers::redact_endpoint_userinfo(provider),
                    "model": model,
                    "messages_count": messages.len(),
                    "tool_snapshot": {"count": 0, "native_tools": false, "tools": []},
                    "request_estimate": {
                        "messages_bytes": bytes,
                        "messages_tokens": tokens,
                        "native_tool_bytes": 0,
                        "native_tool_tokens": 0,
                        "total_tokens": tokens,
                        "token_source": token_source_label(measurement.source),
                    },
                })
                .to_string(),
            ),
        )
        .await?;
    Ok(())
}

#[path = "turn_tests/turn_engine.rs"]
mod tests;

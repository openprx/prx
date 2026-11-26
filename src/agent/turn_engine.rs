#![allow(clippy::print_stdout, clippy::print_stderr)]

use crate::hooks::{HookEvent, HookManager, payload_error};
use crate::observability::{Observer, ObserverEvent};
use crate::providers::ChatMessage;
use crate::tools::Tool;
use anyhow::Result;
use std::borrow::Cow;
use std::io::Write as _;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

mod overflow;
mod provider_call;
mod reply;
mod request;
mod request_event;
mod state;
mod surface;
mod tool_round;
use provider_call::{ParsedResponse, ProviderReply};
use state::{TextReply, TurnInputs, TurnState};

#[cfg(test)]
use crate::agent::compaction::is_tool_roundtrip_companion;
#[cfg(test)]
use crate::agent::context_budget::budgeted_request_tool_tokens;
#[cfg(test)]
use crate::agent::context_budget::{APPENDED_SECTION_FRAMING_TOKENS, TOKEN_HEURISTIC_BUDGET_MULTIPLIER};
use crate::agent::context_budget::{PRE_TURN_FLUSH_THRESHOLD, plan_context_budget};
use crate::agent::context_paging::{
    context_mode_uses_os_paging, inject_recalled_context, plan_os_paging, recall_context_pages,
};
#[cfg(test)]
use crate::agent::document_ingest::drop_untrusted_external_results;
use crate::agent::execution_policy::{AgentToolRuntimeInputs, build_agent_tool_runtime_adapter};
use crate::agent::history::latest_user_query;
use crate::agent::provider_round::is_stream_failure_after_model_output;
use crate::agent::response_guard::strip_assistant_fabricated_tool_markers;
#[cfg(test)]
use crate::agent::response_guard::{
    CORRECTIVE_ASIDE_PREFIX, contains_fabricated_tool_marker, strip_fabricated_tool_markers,
};
#[cfg(test)]
use crate::agent::tool_result_recording::INTERRUPTED_TOOL_RESULT;
use crate::agent::tool_scheduler::ReadOnlyToolScheduleConfig;
#[cfg(test)]
use crate::agent::turn_contracts::ChatMode;
use crate::agent::turn_contracts::{ToolCallNotification, ToolLoopCancelled, ToolLoopOutcome, ToolLoopTrace};
use crate::agent::turn_history::{TurnHistory, same_messages};
use crate::agent::turn_request::{ToolLoopMemory, TurnDecision, TurnDelivery, TurnRequest, TurnSchedule, TurnServices};
use crate::context::budget::anchor;
use crate::context::reduce::clear::ClearOldResults;
use crate::context::reduce::compact::PreTurnCompact;
use crate::context::reduce::engine::{PreTurn, Services, history_before_change, report_durable_compaction};
use crate::context::reduce::ladder;
#[cfg(test)]
use crate::memory::{DocumentSearchResult, MemoryPrincipal};
#[cfg(test)]
use crate::providers::Provider;
#[cfg(test)]
use crate::providers::ToolCall;
#[cfg(all(test, feature = "wasm-plugins"))]
use std::path::Path;

#[cfg(test)]
use crate::agent::context_budget::{
    budgeted_tokens, estimate_history_tokens, estimate_history_tokens_with_source, estimate_request_tokens,
    reserve_request_tool_surface, trim_history_to_context_budget,
    trim_history_to_context_budget_preserving_compaction_replacement_with_floor,
};
#[cfg(test)]
use crate::agent::history::apply_aggressive_trim;
#[cfg(test)]
use crate::agent::response_guard::{
    EMPTY_ASSISTANT_RESPONSE_RETRY_MESSAGE, TOOL_SUMMARY_ONLY_RESPONSE_MESSAGE, is_tool_summary_only_response,
};
#[cfg(test)]
use crate::agent::tool_result_recording::tool_result_inline_budget_for_history;
#[cfg(test)]
use crate::agent::turn_contracts::ToolLoopEvent;
#[cfg(test)]
use crate::llm::route_decision::TokenUsage;
#[cfg(test)]
use crate::providers::ChatRequest;

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
    let advisory_service = request.decision.service;
    let advisory_context = request.decision.context;
    let advisory_id = request.advice.as_ref().map(|analysis| analysis.decision_id.clone());
    let guard_token = delivery.cancellation.clone();
    let channel_name = request.channel_name;
    // Boxed at the boundary, not inside the guard. The loop future is one of the
    // largest in the process, and letting the guard hold it inline would place a
    // second copy of it in this frame — enough to overflow a tokio worker's
    // stack on the chat path. A pointer costs nothing here and keeps the guard's
    // own future tiny.
    // The terminal settles a failed, cancelled or refused turn from this same
    // retained provider evidence; the outcome cost reads it too (P2-R10).
    let retained = services
        .runtime_adapter
        .as_ref()
        .and_then(|adapter| adapter.provider_trace.clone());
    let started = std::time::Instant::now();
    let provenance = turn_write_provenance(&services);
    // Every hook event of this turn carries the same identity, whichever
    // surface runs it; the start and the completion are emitted here, once,
    // for all of them.
    let hooks = services.hooks;
    let turn_id = uuid::Uuid::new_v4().to_string();
    let (identity, mode) = match services.runtime_adapter.as_ref() {
        Some(adapter) => {
            let envelope = &adapter.tool_execution_context.envelope;
            let mode = match envelope.source {
                crate::runtime::envelope::RuntimeSource::Chat => "chat",
                crate::runtime::envelope::RuntimeSource::Agent => "single",
                source => crate::hooks::HookIdentity::origin_of(source),
            };
            (crate::hooks::HookIdentity::from_envelope(envelope, turn_id), mode)
        }
        None => (crate::hooks::HookIdentity::bare_turn(turn_id), channel_name),
    };
    let input_chars = history
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map_or(0, |message| message.content.chars().count());
    hooks
        .emit_for(
            &identity,
            HookEvent::TurnOpened,
            serde_json::json!({"input_chars": input_chars, "surface": channel_name}),
        )
        .await;
    // The loop future is boxed before the scope takes it, so only a pointer
    // moves through the scope's frames.
    // Type-erased so the auto-trait proof of every future that awaits a turn
    // stops here instead of walking the whole loop future again.
    let inner: std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(ToolLoopOutcome, ToolLoopTrace)>> + Send + '_>,
    > = Box::pin(crate::hooks::identity_scope(
        identity.clone().with_manager(hooks.clone()),
        crate::memory::write_provenance::scope_turn(
            provenance,
            Box::pin(run_turn_unguarded(history, request, services, delivery)),
        ),
    ));
    let result = crate::agent::idle::run_guarded(
        crate::agent::idle::configured(),
        channel_name,
        guard_token.as_ref(),
        inner,
    )
    .await;
    // A completed turn settles its returned trace; any other turn settles the
    // metered usage retained before it ended, as the terminal does.
    let settled_trace = match &result {
        Ok(_) => None,
        Err(_) => retained.as_ref().and_then(|retained| retained.snapshot().0),
    };
    let usage = match &result {
        Ok((_, trace)) => Some(trace),
        Err(_) => settled_trace.as_ref(),
    }
    .and_then(|trace| {
        Some(crate::runtime::decision_advisory::TurnUsage {
            provider: trace.final_provider.as_deref()?,
            model: trace.final_model.as_deref()?,
            tokens: &trace.tokens_used,
        })
    });
    let ended = crate::runtime::decision_advisory::TurnEnd {
        result: turn_result_class(&result),
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        usage,
    };
    let response_chars = match &result {
        Ok((ToolLoopOutcome::Text(text), _)) => text.chars().count(),
        Ok((ToolLoopOutcome::AwaitingApproval { reply }, _)) => reply.chars().count(),
        _ => 0,
    };
    let token_source = match &result {
        Ok((_, trace)) => Some(trace),
        Err(_) => settled_trace.as_ref(),
    };
    let tokens = |pick: fn(&crate::llm::route_decision::TokenUsage) -> Option<u32>| {
        token_source.and_then(|trace| pick(&trace.tokens_used))
    };
    hooks
        .emit_for(
            &identity,
            HookEvent::TurnComplete,
            serde_json::json!({
                "mode": mode,
                "response_chars": response_chars,
                "status": ended.result.as_str(),
                "awaiting_approval": matches!(&result, Ok((ToolLoopOutcome::AwaitingApproval { .. }, _))),
                "rounds": identity.round().unwrap_or(0),
                "duration_ms": ended.duration_ms,
                "input_tokens": tokens(|usage| usage.prompt_tokens),
                "output_tokens": tokens(|usage| usage.completion_tokens),
            }),
        )
        .await;
    advisory_service
        .advisory()
        .observe(|| advisory_context.terminal_execution_trace(), advisory_id, ended);
    result
}

/// The telemetry class of a finished turn: cancellation and a refused send
/// are told apart from other failures (P2-R10).
fn turn_result_class(result: &Result<(ToolLoopOutcome, ToolLoopTrace)>) -> crate::decision_engine::TurnResultClass {
    use crate::decision_engine::TurnResultClass;
    match result {
        Ok(_) => TurnResultClass::Completed,
        Err(error)
            if error
                .chain()
                .any(|cause| cause.is::<crate::agent::turn_contracts::ToolLoopCancelled>()) =>
        {
            TurnResultClass::Cancelled
        }
        Err(error)
            if error
                .chain()
                .any(|cause| cause.is::<crate::agent::send_admission::ProviderSendRefused>()) =>
        {
            TurnResultClass::Refused
        }
        Err(_) => TurnResultClass::Failed,
    }
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

/// The valid middleware replacement of `history`, when it changes anything.
#[cfg(feature = "wasm-plugins")]
fn middleware_messages(history: &[ChatMessage], transformed: &serde_json::Value) -> Option<Vec<ChatMessage>> {
    let candidate = serde_json::from_value::<Vec<ChatMessage>>(transformed.get("messages")?.clone()).ok()?;
    if candidate.is_empty() || candidate.iter().any(|message| message.role.trim().is_empty()) {
        return None;
    }
    let rewritten = neutralize_middleware_user_messages(history, candidate);
    (!same_messages(history, &rewritten)).then_some(rewritten)
}

/// Re-derive the provider view from the turn's canonical history after a
/// durable compaction changed it: the turn's projection (recalled page,
/// skill-metadata suppression, inbound middleware) over the canonical
/// history. The per-provider preflight trims it again before the next request.
async fn rederive_provider_view(
    turn_history: &mut TurnHistory,
    history: &mut Vec<ChatMessage>,
    hooks: &HookManager,
    channel_name: &str,
) {
    let projected = turn_history.project(history);
    #[cfg(feature = "wasm-plugins")]
    let projected = if turn_history.inbound_middleware_applied() {
        let transformed = hooks
            .process_wasm_middleware(
                crate::plugins::capabilities::middleware::MiddlewareStage::Inbound,
                serde_json::json!({
                    "channel": channel_name,
                    "messages": &projected,
                }),
            )
            .await;
        middleware_messages(&projected, &transformed).unwrap_or(projected)
    } else {
        projected
    };
    #[cfg(not(feature = "wasm-plugins"))]
    let _ = (hooks, channel_name);
    turn_history.reshape(history, |view| *view = projected);
}

/// [`rederive_provider_view`] as the function pointer the reduction steps hold.
fn rederive_boxed<'x>(
    turn_history: &'x mut TurnHistory,
    history: &'x mut Vec<ChatMessage>,
    hooks: &'x HookManager,
    channel_name: &'x str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'x>> {
    Box::pin(rederive_provider_view(turn_history, history, hooks, channel_name))
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
/// The run this turn's writes belong to: the execution adapter's envelope
/// (every ingress and child turn), else the envelope the turn's ingest
/// runtime was built from (single-shot, cron and xin turns, whose adapter is
/// synthetic).
fn turn_write_provenance(services: &TurnServices<'_>) -> Option<crate::memory::write_provenance::WriteProvenance> {
    services
        .runtime_adapter
        .as_ref()
        .and_then(|adapter| adapter.tool_execution_context.envelope.write_provenance())
        .or_else(|| {
            services
                .memory
                .ingest
                .as_ref()
                .and_then(|ingest| ingest.write_provenance.clone())
        })
}

/// The outbound middleware stage, run once on the accepted reply of a
/// delivered turn. Only the terminal text branch calls it.
#[cfg(feature = "wasm-plugins")]
async fn apply_outbound_middleware(hooks: &HookManager, channel_name: &str, display_text: &mut String) {
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
                "text": display_text.as_str(),
                "has_tool_calls": false,
            }),
        )
        .await;
    if let Some(text) = transformed.get("text").and_then(serde_json::Value::as_str) {
        *display_text = finalize_outbound_display_text(display_text.as_str(), text).into_owned();
    }
}

/// Keeps the one required lookup tool in the published set, once, at the end.
fn pin_required_lookup(
    tool_specs: &mut Vec<crate::tools::ToolSpec>,
    required_lookup_spec: Option<crate::tools::ToolSpec>,
) {
    if let Some(required) = required_lookup_spec {
        tool_specs.retain(|spec| spec.name != crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME);
        tool_specs.push(required);
    }
}

async fn run_turn_unguarded(
    history: &mut Vec<ChatMessage>,
    request: TurnRequest<'_>,
    services: TurnServices<'_>,
    delivery: TurnDelivery,
) -> Result<(ToolLoopOutcome, ToolLoopTrace)> {
    let TurnRequest {
        advice,
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
        decision:
            TurnDecision {
                service,
                context,
                runtime: turn_runtime,
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
    let decision_brief = advice
        .as_ref()
        .map(|analysis| {
            let config = service.advisory().config();
            crate::decision_engine::render::render_decision_brief(
                analysis,
                config.prompt_budget_tokens,
                config.candidates,
            )
        })
        // V-12: a brief that does not fit the first send's token admission is
        // dropped; the send itself is never refused because of the engine.
        .filter(|brief| {
            let fits = crate::cost::estimate::candidate_tokens(brief.len())
                <= crate::agent::send_admission::live_g6_tokens(context);
            if !fits {
                tracing::debug!("decision brief dropped: it does not fit the first send's token budget");
            }
            fits
        });
    let cache_series = super::prompt_cache_metrics::Series::for_turn(scope_ctx, &memory_runtime);
    let TurnDelivery {
        cancellation: cancellation_token,
        on_delta,
        on_tool_call,
    } = delivery;
    crate::runtime::turn_model::validate_execution(proof, provider_name, model)?;
    let ToolLoopMemory {
        ledger,
        ingest: document_ingest,
        event_fabric,
        request_event_scope,
        tool_surface_reserve,
        canonical_history,
        caller_rollover,
        history_event_ids,
        rendered_stored_handoffs: rendered,
    } = memory_runtime;
    // The provider's count of each request anchors the estimate of the next
    // one; a chat session keeps its anchors across turns, any other turn its own.
    let usage_anchors = tool_surface_reserve.as_ref().map_or_else(
        anchor::UsageAnchors::default,
        crate::agent::context_budget::ToolSurfaceReserve::usage_anchors,
    );
    // A session authority adopts this turn's durable compactions as patches
    // over its acknowledged history, so the turn must build them from that
    // history, never from the provider view (I-CANON).
    anyhow::ensure!(
        runtime_adapter.as_ref().is_none_or(|adapter| adapter.events.is_none()) || canonical_history.is_some(),
        "a turn that reports to a session authority requires its canonical history"
    );
    // The canonical history every durable compaction of this turn is built
    // from; `history` is the provider view projected from it.
    let mut turn_history = TurnHistory::open(history, canonical_history, caller_rollover, history_event_ids, rendered);
    anyhow::ensure!(
        Arc::ptr_eq(turn_runtime, context.runtime()),
        "turn runtime context differs from the opened decision context"
    );
    let parameter_capture = turn_runtime.capture_main_parameters(event_fabric.is_some());
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

    // The request provenance of this turn starts from empty notes.
    crate::context::request_provenance::open_turn(
        request_event_scope
            .as_ref()
            .and_then(|scope| scope.run_id.as_deref())
            .or(runtime_adapter.tool_execution_context.envelope.run_id.as_deref()),
    );

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
            && (turn_history.holds_context_handoff(history)
                || plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD).over_warning);
        if switch_needs_lookup
            && !tools_registry
                .iter()
                .any(|tool| tool.supports_name(crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME))
        {
            tracing::warn!(
                tool = crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME,
                "context switch recovery tool is not registered; continuing without it"
            );
        }
    }

    if let Some(config) = compaction_config
        && context_mode_uses_os_paging(config)
    {
        if let Some(query) = latest_user_query(history).map(ToString::to_string) {
            if let Some(recalled) = recall_context_pages(&query, config, document_ingest.as_ref()).await {
                // A projection: the canonical history never holds the page.
                turn_history.reshape(history, |view| inject_recalled_context(view, recalled.clone()));
                turn_history.record_recalled_page(recalled);
            }
        }
        // Gated on the provider view, evicted from the canonical history.
        if plan_context_budget(history, config, config.os_paging.eviction_threshold).over_warning {
            let canonical = turn_history.canonical_mut(history);
            let before = history_before_change(&runtime_adapter, canonical);
            match plan_os_paging(
                canonical.messages(),
                canonical.sources(),
                config,
                document_ingest.as_ref(),
            )
            .await
            {
                Ok(Some(eviction)) if canonical.apply_page_eviction(&eviction) => {
                    crate::agent::idle::beat(crate::agent::idle::ProgressKind::Compaction);
                    tracing::debug!("os-paging: pre-turn eviction applied");
                    report_durable_compaction(
                        &runtime_adapter,
                        before,
                        turn_history.finish_durable_compaction(history),
                        config,
                    )
                    .await?;
                    rederive_provider_view(&mut turn_history, history, hooks, channel_name).await;
                }
                Ok(_) => {}
                Err(error) => {
                    let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
                    tracing::warn!(error_len, error_sha256, "os-paging: pre-turn eviction error (soft)");
                }
            }
        }
    }

    // P1-3: Proactive pre-turn memory flush — compact before starting the loop
    // if the context is already above 85% of the token budget, unless the
    // caller's own pre-turn rollover is current (`caller_rollover_current`).
    // The step is `PreTurnCompact` of the reduction ladder.
    if let Some(config) = compaction_config {
        let mut pre_turn = PreTurn {
            history: &mut *history,
            turn_history: &mut turn_history,
            config,
            services: Services {
                provider,
                model,
                audit: document_ingest.as_ref(),
                runtime_adapter: &runtime_adapter,
                hooks,
                channel_name,
                rederive: rederive_boxed,
            },
        };
        Box::pin(ladder(&[&ClearOldResults, &PreTurnCompact], &mut pre_turn)).await?;
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
        // A projection of every request this turn sends: a view re-derived
        // after a durable compaction passes through the middleware again.
        if let Some(rewritten) = middleware_messages(history, &transformed) {
            turn_history.reshape(history, |view| *view = rewritten);
        }
        turn_history.mark_inbound_middleware_applied();
    }

    // The request this turn answers, as the view holds it now: the anchor
    // that tells the reducers which tool results belong to the turn.
    turn_history.anchor_live_request(history);

    // Same reasoning for the request-size accounting: measuring the published
    // tool surface means serializing and tokenizing every schema and rendering
    // and tokenizing the prompt sections, so the result is reused for as long
    // as the exposed set is unchanged.
    // The payload is measured as the provider's dialect puts it on the wire
    // (C2b-3): a cleaning dialect inlines `$ref` and can be larger than the
    // published document.
    let request_tool_surface_cache =
        crate::agent::turn_context::RequestToolSurfaceCache::for_dialect(provider.tool_schema_dialect_for(model));
    // A sub-agent inherits the hidden surface of the turn that spawned it.
    // Sub-agent loops deliberately run without tool tiering, so without this a
    // conversation partner on an IM channel could reach the whole operations
    // surface simply by asking for a spawn or a delegate. Read once: the spawn
    // context is fixed for the lifetime of this turn.
    let inherited_exclusions = crate::tools::sessions_spawn::ambient_channel_excluded_tools();
    let safety_surface = Arc::new(crate::tools::surface_owner::build(
        crate::tools::surface_owner::SurfaceParts {
            execution: service.execution(),
            catalog: service.catalog(),
            publication_policy: service.publication_policy(),
            tiering: service.tool_tiering(),
            security: service.security(),
            channel: service.channel_surface(),
            inherited_exclusions: &inherited_exclusions,
            sender: proof.policy_sender(),
            channel_name: proof.policy_channel(),
            chat_type: proof.policy_chat_type(),
            tool_calls_allowed: service.model_can_call_tools(proof),
        },
    ));
    let turn = TurnInputs {
        advice,
        channel_name,
        chat_mode,
        silent,
        expose_stay_silent,
        multimodal_config,
        compaction_config,
        service,
        context,
        proof,
        provider,
        provider_name,
        model,
        temperature,
        tools_registry,
        observer,
        hooks,
        runtime_adapter,
        document_ingest,
        event_fabric,
        request_event_scope,
        tool_surface_reserve,
        cancellation_token,
        on_delta,
        on_tool_call,
        usage_anchors,
        parameter_capture,
        cache_series,
        read_only_schedule,
        mode_capabilities,
        inherited_exclusions,
        safety_surface,
    };
    let mut state = TurnState::new(decision_brief, request_tool_surface_cache);
    let TurnInputs {
        silent,
        service,
        context,
        proof,
        ref cancellation_token,
        ref on_tool_call,
        ref inherited_exclusions,
        ..
    } = turn;
    loop {
        state.iteration = state.iteration.saturating_add(1);
        crate::hooks::set_round(state.iteration);
        // Notify progress for multi-iteration tool loops (skip the first iteration).
        if state.iteration > 1 {
            if let Some(tx) = on_tool_call {
                let _ = tx.try_send(ToolCallNotification::Progress {
                    iteration: state.iteration,
                });
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

        let mut surface = Box::pin(surface::select_round_surface(
            &turn,
            &mut state,
            &mut turn_history,
            history,
        ))
        .await?;
        let budget = Box::pin(surface::plan_round_budget(&turn, &surface, &mut turn_history, history)).await?;
        let context_lookup_required = Box::pin(surface::publish_context_lookup(
            &turn,
            &mut state,
            &mut surface,
            &turn_history,
            history,
        ))
        .await?;

        let mut round = Box::pin(request::prepare_request(
            &turn,
            &mut state,
            &mut surface,
            history,
            context_lookup_required,
        ))
        .await?;
        Box::pin(request_event::record_request_context(
            &turn, &state, &surface, &budget, &round,
        ))
        .await;
        let parsed = match Box::pin(provider_call::call_provider(
            &turn, &mut state, &mut round, &budget, history,
        ))
        .await?
        {
            ProviderReply::Parsed(parsed) => parsed,
            ProviderReply::Failed(error)
                if crate::recovery::overflow::is_context_overflow_error(&error)
                    && !is_stream_failure_after_model_output(&error) =>
            {
                Box::pin(overflow::recover_from_overflow(
                    &turn,
                    &mut state,
                    &mut turn_history,
                    history,
                    &error,
                ))
                .await?;
                continue;
            }
            ProviderReply::Failed(error) => return Err(error),
        };
        let ParsedResponse {
            response_text,
            parsed_text,
            mut tool_calls,
            assistant_history_content,
            native_tool_calls,
        } = parsed;

        let display_text = if parsed_text.is_empty() {
            response_text.clone()
        } else {
            parsed_text
        };

        if tool_calls.is_empty() {
            match Box::pin(reply::finish_text_reply(
                &turn,
                &mut state,
                history,
                &response_text,
                display_text,
                round.guarded_delivery,
            ))
            .await?
            {
                TextReply::Retry => continue,
                TextReply::Deliver(display_text) => {
                    // FIX #2: fold the cross-turn fallback flag into the returned trace so
                    // a fallback on any earlier turn is reflected in the recorded status.
                    let last_turn_trace = state.returned_trace(context);
                    return Ok((ToolLoopOutcome::Text(display_text), last_turn_trace));
                }
            }
        }

        if let Some(outcome) = Box::pin(tool_round::screen_calls(&turn, &mut round, &mut tool_calls)).await? {
            return Ok((outcome, state.returned_trace(context)));
        }
        // Print any text the LLM produced alongside tool calls (unless silent)
        if !silent && !display_text.is_empty() {
            print!("{display_text}");
            let _ = std::io::stdout().flush();
        }

        let calls = tool_round::ToolCalls {
            tool_calls,
            assistant_history_content,
            native_tool_calls,
        };
        if let Some(outcome) = Box::pin(tool_round::run_tools(
            &turn,
            &mut state,
            &mut turn_history,
            history,
            &round,
            calls,
        ))
        .await?
        {
            return Ok((outcome, state.returned_trace(context)));
        }
    }
}

#[path = "turn_tests/turn_engine.rs"]
mod tests;

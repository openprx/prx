//! The provider call of one round: the request hook, the admitted send, the
//! streamed or buffered call, and the parsing of its answer.

use crate::hooks::{HookEvent, payload_error};
use crate::llm::route_decision::TokenUsage;
use crate::observability::ObserverEvent;
use crate::providers::{ChatMessage, ChatRequest};
use anyhow::Result;
use std::borrow::Cow;
use std::time::Instant;

use crate::agent::diagnostics::tool_parse_log_digest;
use crate::agent::history::build_native_assistant_history;
use crate::agent::provider_round::run_streaming_provider_turn;
use crate::agent::response_guard::strip_assistant_fabricated_tool_markers;
use crate::agent::tool_call_parser::{
    looks_like_unparsed_tool_call_syntax, parse_structured_tool_calls, parse_tool_calls,
};
use crate::agent::tool_evidence::strip_model_written_tool_evidence;
use crate::agent::turn_contracts::{ToolLoopCancelled, ToolLoopTrace};
use crate::context::budget::record::settle_round;

use crate::agent::tool_call_parser::ParsedToolCall;
use crate::providers::ToolCall;

use super::state::{PreparedRound, RoundBudget, TurnInputs, TurnState};

/// The parts of a provider answer the round acts on.
pub(super) struct ParsedResponse {
    pub(super) response_text: String,
    pub(super) parsed_text: String,
    pub(super) tool_calls: Vec<ParsedToolCall>,
    pub(super) assistant_history_content: String,
    pub(super) native_tool_calls: Vec<ToolCall>,
}

/// What one provider call came to: a parsed answer, or the error the round
/// either recovers from (context overflow) or returns.
pub(super) enum ProviderReply {
    Parsed(ParsedResponse),
    Failed(anyhow::Error),
}

/// Makes the provider call of one round and parses its answer.
pub(super) async fn call_provider(
    turn: &TurnInputs<'_>,
    state: &mut TurnState,
    round: &mut PreparedRound,
    budget: &RoundBudget,
    history: &[ChatMessage],
) -> Result<ProviderReply> {
    let TurnInputs {
        service,
        context,
        proof,
        provider,
        provider_name,
        model,
        temperature,
        observer,
        hooks,
        ref runtime_adapter,
        ref event_fabric,
        ref request_event_scope,
        ref cancellation_token,
        ref usage_anchors,
        ref parameter_capture,
        cache_series,
        ..
    } = *turn;
    let tool_snapshot = round.compiled_tool_context.fingerprint();
    hooks
        .emit(
            HookEvent::LlmRequest,
            crate::decision_engine::turn::request_trace(
                serde_json::json!({
                    "provider": provider_name,
                    "model": model,
                    "messages_count": history.len(),
                    "system_prompt_sha256": round.system_prompt_sha256.as_deref(),
                    "system_prompt_chars": round.system_prompt_chars,
                    "tool_snapshot_sha256": tool_snapshot.sha256,
                    "tool_count": tool_snapshot.count,
                }),
                state.decision_brief_sha256.as_deref(),
            ),
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
    let request_tools = round.compiled_tool_context.native_tool_specs();
    // One budget decision per provider round, before either transport.
    crate::agent::send_admission::admit_provider_send(
        service,
        context,
        proof,
        &round.prepared_messages.messages,
        request_tools,
        runtime_adapter.tool_execution_context.schema_dialect,
    )?;

    // FIX-P0-30/31: use the trace-returning provider entrypoint so this
    // turn's real serving provider/model and failover attempts flow back
    // to the caller. The `ReliableProvider` overrides `chat_traced`; other
    // providers get a synthetic single-attempt trace via the trait default.
    let provider_observation = round
        .execution_adapter
        .tool_execution_context
        .execution_trace
        .as_ref()
        .map(|trace| trace.begin_provider())
        .transpose()?;
    if let Some(retained) = &runtime_adapter.provider_trace {
        retained.begin();
    }
    anyhow::ensure!(
        !runtime_adapter.stream_provider || crate::runtime::experiment_stage::current().is_none(),
        "experiment solver streaming has no admitted physical accounting adapter"
    );
    if let Some(live) = &runtime_adapter.live_frames {
        live.thinking();
    }
    let chat_result = if runtime_adapter.stream_provider {
        parameter_capture
            .scope(run_streaming_provider_turn(
                provider,
                &round.prepared_messages.messages,
                request_tools,
                provider_name,
                model,
                temperature,
                cancellation_token.as_ref(),
                &runtime_adapter,
                !round.guarded_delivery,
            ))
            .await
    } else {
        let chat_future = parameter_capture.scope(crate::runtime::experiment_stage::chat_traced(
            provider,
            provider_name,
            runtime_adapter
                .tool_execution_context
                .envelope
                .run_id
                .as_deref()
                .unwrap_or(""),
            ChatRequest {
                messages: &round.prepared_messages.messages,
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
    let prepared = parameter_capture.snapshot_from(state.last_turn_trace.prepared_parameters.len());
    state.last_turn_trace.prepared_parameters.extend(prepared);

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
    let reply = match chat_result {
        Ok(trace) => {
            crate::agent::prompt_cache_metrics::record_round_trip(crate::agent::prompt_cache_metrics::RoundTrip {
                series: cache_series,
                provider: &trace.final_provider,
                model: &trace.final_model,
                system_prompt_sha256: round.system_prompt_sha256.as_deref(),
                tools_sha256: &tool_snapshot.sha256,
                usage: &trace.tokens_used,
            });
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
                state.any_turn_had_fallback = true;
            }
            state.usage_accumulator.record(trace.tokens_used.clone());
            if let Some(planned) = budget.request_budget.as_ref() {
                let sink = event_fabric.as_ref().map(|fabric| {
                    let scope = request_event_scope
                        .clone()
                        .unwrap_or_else(|| round.execution_adapter.tool_execution_context.envelope.message_scope())
                        .with_recipient(format!("{provider_name}/{model}"));
                    (fabric, scope)
                });
                settle_round(
                    &usage_anchors,
                    planned,
                    &trace,
                    (state.iteration, provider_name, model),
                    sink,
                )
                .await;
            }
            // Record this turn's real attribution. Overwritten each turn;
            // the value at the returning turn is what the caller receives.
            // The served member is named by its unique key when the chain
            // reports one: display names of members that differ only in
            // their password are equal (D-068 (2)).
            round.execution_adapter.tool_execution_context.schema_dialect = provider
                .tool_schema_dialect_of_member(
                    trace.served_member.as_deref().unwrap_or(&trace.final_provider),
                    &trace.final_model,
                )
                .unwrap_or_else(|| provider.tool_schema_dialect_for(&trace.final_model));
            state.last_turn_trace = ToolLoopTrace {
                prepared_parameters: std::mem::take(&mut state.last_turn_trace.prepared_parameters),
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
                    &state.last_turn_trace,
                    state.usage_accumulator.finish(),
                    state.any_turn_had_fallback,
                    trace.tokens_used.is_reported() && trace.tokens_used.has_any_tokens(),
                );
            }
            let resp = trace.response;

            #[cfg(feature = "wasm-plugins")]
            let resp = {
                let mut resp = resp;
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
                resp
            };
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
                let (fallback_text, fallback_calls) = parse_tool_calls(&response_text)?;
                if !fallback_text.is_empty() {
                    parsed_text = fallback_text;
                }
                calls = fallback_calls;
                if calls.is_empty() && looks_like_unparsed_tool_call_syntax(&response_text)? {
                    let (response_len, response_sha256) = tool_parse_log_digest(&response_text);
                    tracing::error!(
                        response_len,
                        response_sha256 = response_sha256.as_str(),
                        parse_error = "unparsed tool-call syntax",
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
            // An evidence line the model wrote is never replayed either:
            // only the runtime writes tool evidence.
            let history_text = match strip_assistant_fabricated_tool_markers(&response_text) {
                Cow::Borrowed(text) => strip_model_written_tool_evidence(text),
                Cow::Owned(text) => Cow::Owned(strip_model_written_tool_evidence(&text).into_owned()),
            };
            let assistant_history_content = if resp.tool_calls.is_empty() {
                history_text.into_owned()
            } else {
                build_native_assistant_history(&history_text, &resp.tool_calls, resp.reasoning_content.as_deref())
            };

            let native_calls = resp.tool_calls;
            ProviderReply::Parsed(ParsedResponse {
                response_text,
                parsed_text,
                tool_calls: calls,
                assistant_history_content,
                native_tool_calls: native_calls,
            })
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
            ProviderReply::Failed(e)
        }
    };
    Ok(reply)
}

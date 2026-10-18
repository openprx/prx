use crate::agent::turn_contracts::{ToolLoopCancelled, ToolLoopEvent, ToolLoopRuntimeAdapter, emit_tool_loop_event};
use crate::llm::route_decision::ProviderUsageAccumulator;
use crate::providers::{ChatMessage, Provider, ToolCall};
use anyhow::Result;
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct StreamingProviderFailure {
    message: String,
    retryable: bool,
    /// The stream failed after the model had already produced text,
    /// reasoning or a tool call. Such a failure is never a request-size
    /// problem to compact and replay, whatever its message says.
    after_model_output: bool,
}

impl std::fmt::Display for StreamingProviderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StreamingProviderFailure {}

/// Whether `err` is a streaming failure that happened after model output.
pub(super) fn is_stream_failure_after_model_output(err: &anyhow::Error) -> bool {
    err.chain()
        .find_map(|source| source.downcast_ref::<StreamingProviderFailure>())
        .is_some_and(|failure| failure.after_model_output)
}

pub(crate) fn tool_loop_error_is_retryable(err: &anyhow::Error) -> bool {
    err.chain()
        .find_map(|source| source.downcast_ref::<StreamingProviderFailure>())
        .is_some_and(|failure| failure.retryable)
}

fn streaming_error_is_context_overflow(err: &crate::providers::traits::StreamError) -> bool {
    use crate::providers::traits::StreamError;
    match err {
        StreamError::Provider(message) => {
            crate::recovery::overflow::is_context_overflow_error(&anyhow::anyhow!(message.clone()))
        }
        StreamError::Http(http_error) => {
            matches!(http_error.status(), Some(status) if status.as_u16() == 413)
        }
        StreamError::Json(_) | StreamError::InvalidSse(_) | StreamError::Io(_) | StreamError::RateLimited { .. } => {
            false
        }
    }
}

fn streaming_error_is_network_timeout(err: &crate::providers::traits::StreamError) -> bool {
    use crate::providers::traits::StreamError;
    match err {
        StreamError::Io(_) => true,
        StreamError::Http(http_error) => http_error.is_timeout() || http_error.is_connect(),
        StreamError::Json(_)
        | StreamError::InvalidSse(_)
        | StreamError::Provider(_)
        | StreamError::RateLimited { .. } => false,
    }
}

const SHARED_STREAM_NETWORK_RETRIES: u8 = 3;

const SHARED_STREAM_BACKOFF_BASE_MS: u64 = 500;

#[derive(Default)]
struct SharedToolCallAggregator {
    slots: HashMap<usize, SharedToolCallSlot>,
    completed: std::collections::HashSet<usize>,
}

#[derive(Default)]
struct SharedToolCallSlot {
    id: String,
    name: String,
    args: String,
}

impl SharedToolCallAggregator {
    fn ingest(&mut self, chunk: crate::providers::traits::ToolCallChunk) -> Option<ToolCall> {
        use crate::providers::traits::ToolCallChunkStatus;
        match chunk.status {
            ToolCallChunkStatus::Streaming => {
                let slot = self.slots.entry(chunk.index).or_default();
                if slot.id.is_empty() && !chunk.id.is_empty() {
                    slot.id = chunk.id;
                }
                if slot.name.is_empty() && !chunk.name.is_empty() {
                    slot.name = chunk.name;
                }
                if let Some(delta) = chunk.arguments_delta {
                    slot.args.push_str(&delta);
                }
                None
            }
            ToolCallChunkStatus::Completed => {
                if !self.completed.insert(chunk.index) {
                    return None;
                }
                let slot = self.slots.entry(chunk.index).or_default();
                let id = if chunk.id.is_empty() { slot.id.clone() } else { chunk.id };
                let name = if chunk.name.is_empty() {
                    slot.name.clone()
                } else {
                    chunk.name
                };
                let arguments = if chunk.args.is_empty() {
                    slot.args.clone()
                } else {
                    chunk.args
                };
                Some(ToolCall { id, name, arguments })
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_streaming_provider_turn(
    provider: &dyn Provider,
    messages: &[ChatMessage],
    tool_specs: Option<&[crate::tools::ToolSpec]>,
    provider_name: &str,
    model: &str,
    temperature: f64,
    cancellation: Option<&CancellationToken>,
    adapter: &ToolLoopRuntimeAdapter,
) -> Result<crate::providers::traits::ChatTrace> {
    use crate::providers::traits::{ChatResponse, StreamChunk, StreamOptions};
    use futures::StreamExt;

    let mut network_attempt = 0_u8;
    loop {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(ToolLoopCancelled.into());
        }

        let started_at = chrono::Utc::now();
        let options = StreamOptions::new(true).with_tools(tool_specs.unwrap_or_default().to_vec());
        let stream = provider.stream_chat_with_history(messages, model, temperature, options);
        tokio::pin!(stream);
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut calls = Vec::new();
        let mut call_aggregator = SharedToolCallAggregator::default();
        let mut usage = ProviderUsageAccumulator::new();
        let mut route_attempts = Vec::new();
        let mut served_member: Option<String> = None;
        let mut emitted_model_output = false;

        let stream_result: Result<()> = loop {
            let next = if let Some(token) = cancellation {
                tokio::select! {
                    biased;
                    () = token.cancelled() => return Err(ToolLoopCancelled.into()),
                    next = stream.next() => next,
                }
            } else {
                stream.next().await
            };
            match next {
                Some(Ok(StreamChunk {
                    delta,
                    reasoning: reasoning_delta,
                    is_final,
                    usage: chunk_usage,
                    tool_calls,
                    route_attempt,
                    served_member: chunk_served_member,
                    ..
                })) => {
                    // Any chunk at all — text, reasoning, a tool-call
                    // fragment, or a bare keepalive frame — is evidence the
                    // provider connection is alive, which is the quantity a
                    // stream should be judged on. Resets the idle window.
                    crate::agent::idle::beat(crate::agent::idle::ProgressKind::ProviderStream);
                    if let Some(route_attempt) = route_attempt {
                        route_attempts.push(route_attempt);
                    }
                    if chunk_served_member.is_some() {
                        served_member = chunk_served_member;
                    }
                    if let Some(chunk_usage) = chunk_usage {
                        usage.record(chunk_usage);
                    }
                    emitted_model_output |= !delta.is_empty()
                        || reasoning_delta.as_ref().is_some_and(|value| !value.is_empty())
                        || !tool_calls.is_empty();
                    for chunk in tool_calls {
                        if let Some(call) = call_aggregator.ingest(chunk) {
                            calls.push(call);
                        }
                    }
                    if !delta.is_empty() {
                        text.push_str(&delta);
                        emit_tool_loop_event(adapter, ToolLoopEvent::TextDelta(delta)).await?;
                    }
                    if let Some(reasoning_delta) = reasoning_delta
                        && !reasoning_delta.is_empty()
                    {
                        reasoning.push_str(&reasoning_delta);
                        emit_tool_loop_event(adapter, ToolLoopEvent::ReasoningDelta(reasoning_delta)).await?;
                    }
                    if is_final {
                        break Ok(());
                    }
                }
                Some(Err(error)) => break Err(anyhow::Error::new(error)),
                None => break Ok(()),
            }
        };

        match stream_result {
            Ok(()) => {
                let finished_at = chrono::Utc::now();
                let tokens_used = usage.finish_or_estimate_completion_chars(
                    text.chars().count().saturating_add(reasoning.chars().count()),
                );
                let successful_attempt = route_attempts
                    .iter()
                    .rev()
                    .find(|attempt| attempt.status == crate::llm::route_decision::AttemptStatus::Success);
                let final_provider =
                    successful_attempt.map_or_else(|| provider_name.to_string(), |attempt| attempt.provider.clone());
                let final_model = successful_attempt.map_or_else(|| model.to_string(), |attempt| attempt.model.clone());
                if route_attempts.is_empty() {
                    route_attempts.push(crate::llm::route_decision::ProviderAttempt {
                        seq: 1,
                        provider: final_provider.clone(),
                        model: final_model.clone(),
                        started_at,
                        finished_at,
                        status: crate::llm::route_decision::AttemptStatus::Success,
                        error_class: None,
                        error_message: None,
                    });
                }
                return Ok(crate::providers::traits::ChatTrace {
                    response: ChatResponse {
                        text: Some(text),
                        tool_calls: calls,
                        reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
                    },
                    attempts: route_attempts,
                    final_provider,
                    final_model,
                    served_member,
                    tokens_used,
                });
            }
            Err(error) => {
                let Some(stream_error) = error.downcast_ref::<crate::providers::traits::StreamError>() else {
                    return Err(error);
                };
                if emitted_model_output {
                    return Err(StreamingProviderFailure {
                        message: format!(
                            "stream interrupted after model output; refusing unsafe request replay: {stream_error}"
                        ),
                        retryable: false,
                        after_model_output: true,
                    }
                    .into());
                }
                if streaming_error_is_context_overflow(stream_error) {
                    return Err(anyhow::anyhow!(stream_error.to_string()));
                }
                if !streaming_error_is_network_timeout(stream_error) {
                    return Err(StreamingProviderFailure {
                        message: stream_error.to_string(),
                        retryable: matches!(
                            stream_error,
                            crate::providers::traits::StreamError::Http(_)
                                | crate::providers::traits::StreamError::Io(_)
                                | crate::providers::traits::StreamError::RateLimited { .. }
                        ),
                        after_model_output: false,
                    }
                    .into());
                }
                network_attempt = network_attempt.saturating_add(1);
                if network_attempt > SHARED_STREAM_NETWORK_RETRIES {
                    return Err(StreamingProviderFailure {
                        message: format!("network retries exhausted ({SHARED_STREAM_NETWORK_RETRIES}): {stream_error}"),
                        retryable: false,
                        after_model_output: false,
                    }
                    .into());
                }
                crate::agent::idle::beat(crate::agent::idle::ProgressKind::ProviderRetry);
                emit_tool_loop_event(
                    adapter,
                    ToolLoopEvent::RetryAttempt {
                        attempt: network_attempt,
                        reason: stream_error.to_string(),
                    },
                )
                .await?;
                let backoff_ms =
                    SHARED_STREAM_BACKOFF_BASE_MS.saturating_mul(1_u64 << network_attempt.saturating_sub(1).min(31));
                let sleep = tokio::time::sleep(std::time::Duration::from_millis(backoff_ms));
                tokio::pin!(sleep);
                if let Some(token) = cancellation {
                    tokio::select! {
                        biased;
                        () = token.cancelled() => return Err(ToolLoopCancelled.into()),
                        () = &mut sleep => {}
                    }
                } else {
                    sleep.await;
                }
            }
        }
    }
}

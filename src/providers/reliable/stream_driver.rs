//! Streaming failover driver for the reliable provider: runs a streaming
//! request across the candidate list, retries a failed candidate within its
//! budget before falling back, and forwards the chosen stream to the caller.

use super::super::Provider;
use super::super::circuit_breaker::{self, BreakerOutcome, BreakerParams, ChainLease, CircuitBreakerRegistry};
use super::super::rate_limit::{self, MAX_HONORED_RETRY_AFTER_MS, RateLimitGate};
use super::super::traits::{StreamChunk, StreamOptions, StreamResult};
use super::classify::{
    StreamFailureClass, classify_stream_error, skips_in_place_retry, stream_error_is_tool_schema_refusal,
    stream_retry_kind,
};
use super::retry_policy::{RetryCounts, RetryKind, rate_limit_wait_ms, retry_budget};
use super::wait::{RetryWait, overload_wait_ms, parse_stream_retry_after_ms, plan_wait_from_hint};
use super::{RefusedCandidates, circuit_reason, complete_circuit, notice_overload_retry};
use crate::llm::route_decision::{AttemptStatus, ProviderAttempt};
use crate::security::redaction::redact_provider_text;
use futures_util::{StreamExt, stream};
use std::sync::Arc;
use std::time::Duration;

/// One candidate (provider, model) pair for a streaming request.
pub(super) struct StreamAttempt {
    pub(super) circuit_breaker: Option<(Arc<CircuitBreakerRegistry>, BreakerParams)>,
    pub(super) _lease: Option<Arc<ChainLease>>,
    pub(super) provider_name: Arc<str>,
    /// Refusals and the rate-limit cool-down are keyed by this
    /// ([`member_key`]), never by the display name.
    pub(super) member_key: Arc<str>,
    pub(super) provider: Arc<dyn Provider>,
    pub(super) model: String,
}

/// Drive a streaming request across the candidate list with failover.
///
/// `build_stream` creates the per-attempt provider stream from owned request
/// data. The driver:
/// 1. Tries candidates in order, buffering the first chunk of each. Before each
///    attempt it observes the shared [`RateLimitGate`], so a 429 seen by any
///    other in-flight request defers this one too.
/// 2. If the first chunk is a recoverable error (rate-limited or retryable),
///    backs off and retries the **same** provider/model up to the budget of its
///    [`RetryKind`] (rate limits get more) before considering fallback. An
///    upstream `Retry-After` decides how long that wait is; its absence no
///    longer means "do not retry". A hint above
///    [`MAX_HONORED_RETRY_AFTER_MS`] is not truncated: the candidate is
///    abandoned and the cool-down published to the gate. The sleep is
///    interruptible via `tx.closed()` so a dropped receiver never leaves the
///    driver sleeping pointlessly.
/// 3. Otherwise, if the first chunk is an error that
///    [allows fallback](StreamFailureClass::allows_fallback) and another viable
///    candidate remains, moves on (context-overflow only falls back to a
///    *different* model). A tool-schema refusal also moves on, and its
///    member and model are skipped from then on, as on the buffered path.
///    Otherwise the error is surfaced.
/// 4. Once content has started, forwards the remaining chunks verbatim — a
///    mid-content error is never swallowed (switching providers would corrupt
///    the already-emitted response). FIX-P0-33: mid-stream **resume** (retrying
///    a different provider after partial content) is intentionally NOT done
///    here — see the `emitted_content` branch below.
pub(super) fn drive_streaming_fallback<F>(
    attempts: Vec<StreamAttempt>,
    temperature: f64,
    options: StreamOptions,
    max_retries: u32,
    base_backoff_ms: u64,
    gate: Arc<RateLimitGate>,
    build_stream: F,
) -> stream::BoxStream<'static, StreamResult<StreamChunk>>
where
    F: Fn(&Arc<dyn Provider>, &str, f64, StreamOptions) -> stream::BoxStream<'static, StreamResult<StreamChunk>>
        + Send
        + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(100);

    crate::providers::compatible::spawn_until_closed(tx.clone(), async move {
        let total = attempts.len();
        // The last failure that was not a tool-schema refusal, and the last
        // refusal. A refusal is surfaced only when no tried candidate failed
        // for another reason, as on the buffered path (D-069 L-1).
        let mut last_error: Option<super::super::traits::StreamError> = None;
        let mut last_refusal: Option<super::super::traits::StreamError> = None;
        let mut attempt_seq = 0_u32;
        let mut refused = RefusedCandidates::default();
        let mut circuit_refusals = Vec::new();

        for (index, attempt) in attempts.iter().enumerate() {
            if refused.contains(&attempt.member_key, &attempt.model) {
                continue;
            }
            let is_last = index + 1 == total;
            // For context overflow, only a *different* model is worth trying.
            let next_model_differs = attempts.get(index + 1).is_some_and(|next| next.model != attempt.model);

            // Pre-content retry budget for the *same* candidate. FIX-P0-33: a
            // rate-limit error with a Retry-After hint earns an in-place retry
            // (after sleeping) before we fall back to the next candidate.
            let mut backoff_ms = base_backoff_ms;
            let mut retries = RetryCounts::default();
            // Whether this candidate was throttled at least once, so recovery /
            // give-up land in the rate-limit counters rather than being lost.
            let mut rate_limited_seen = false;

            'candidate: loop {
                let mut permit = match &attempt.circuit_breaker {
                    Some((registry, params)) => match registry.admit(&attempt.member_key, *params) {
                        Ok(permit) => permit,
                        Err(ms) => {
                            circuit_refusals.push(format!(
                                "{}: {}",
                                crate::security::redaction::redact_provider_text(&attempt.provider_name),
                                circuit_reason(ms)
                            ));
                            break 'candidate;
                        }
                    },
                    None => None,
                };
                if let Some(deferred_ms) = gate.wait_until_clear(&attempt.member_key, &attempt.provider_name).await {
                    tracing::warn!(
                        provider = %redact_provider_text(&attempt.provider_name),
                        model = %attempt.model,
                        deferred_ms,
                        "Deferred behind the shared provider rate-limit cool-down"
                    );
                }
                if permit.as_ref().is_some_and(|permit| !permit.revalidate()) {
                    circuit_refusals.push(format!(
                        "{}: {}",
                        crate::security::redaction::redact_provider_text(&attempt.provider_name),
                        circuit_reason(0)
                    ));
                    break 'candidate;
                }
                attempt_seq = attempt_seq.saturating_add(1);
                let attempt_started_at = chrono::Utc::now();
                let mut stream = build_stream(&attempt.provider, &attempt.model, temperature, options.clone());
                let mut emitted_content = false;
                let mut success_reported = false;

                loop {
                    // Stop as soon as nobody consumes the stream (the turn
                    // ended, for example by the idle guard): dropping the inner
                    // stream lets the provider task release its connection.
                    // Waiting for the next chunk would hold both for as long
                    // as a silent server stays silent.
                    let next = tokio::select! {
                        () = tx.closed() => return,
                        next = stream.next() => next,
                    };
                    let Some(chunk) = next else {
                        break;
                    };
                    match chunk {
                        Ok(mut content) => {
                            complete_circuit(permit.take(), BreakerOutcome::Answered);
                            if !emitted_content && rate_limited_seen {
                                rate_limit::record_recovered(&attempt.provider_name);
                            }
                            emitted_content = true;
                            if content.is_final {
                                content.route_attempt = Some(ProviderAttempt {
                                    seq: attempt_seq,
                                    provider: attempt.provider_name.to_string(),
                                    model: attempt.model.clone(),
                                    started_at: attempt_started_at,
                                    finished_at: chrono::Utc::now(),
                                    status: AttemptStatus::Success,
                                    error_class: None,
                                    error_message: None,
                                });
                                content.served_member = Some(attempt.member_key.to_string());
                                success_reported = true;
                            }
                            if tx.send(Ok(content)).await.is_err() {
                                return; // Receiver dropped.
                            }
                        }
                        Err(err) => {
                            // Mid-content failure: surface as-is, never switch
                            // providers. FIX-P0-33: post-content mid-stream
                            // resume is a *deliberate* non-goal. Resuming would
                            // require provider-side checkpointing (a resumable
                            // cursor / token offset); blindly re-issuing the
                            // request against another provider or the same one
                            // would duplicate or truncate the already-emitted
                            // text and corrupt the response. We surface the error
                            // to the caller instead of silently degrading.
                            //
                            // Note: `emitted_content` flips to true for ANY
                            // forwarded chunk — including reasoning/thinking and
                            // tool-call chunks, not just visible text. That is a
                            // conservative choice: once anything has been emitted
                            // downstream, a transparent retry is unsafe.
                            if emitted_content {
                                tracing::warn!(
                                    provider = %redact_provider_text(&attempt.provider_name),
                                    model = %attempt.model,
                                    "Streaming error after content emitted; surfacing without failover (mid-stream resume not supported): {err}"
                                );
                                let _ = tx.send(Err(err)).await;
                                return;
                            }

                            // Pre-content failure: classify and decide.
                            let class = classify_stream_error(&err);
                            // A tool-schema refusal is decided before transport,
                            // so nothing was emitted: like the buffered chain, it
                            // is never retried against this member and model, and
                            // it fails over to the next candidate, which lowers
                            // the schema in its own dialect (D-020 Q2, D-068 (3)).
                            let tool_schema_refusal = stream_error_is_tool_schema_refusal(&err);
                            complete_circuit(
                                permit.take(),
                                circuit_breaker::classify(
                                    skips_in_place_retry(&err),
                                    tool_schema_refusal || class != StreamFailureClass::Retryable,
                                ),
                            );
                            if tool_schema_refusal {
                                refused.insert(&attempt.member_key, &attempt.model);
                            }
                            let failed_attempt = ProviderAttempt {
                                seq: attempt_seq,
                                provider: attempt.provider_name.to_string(),
                                model: attempt.model.clone(),
                                started_at: attempt_started_at,
                                finished_at: chrono::Utc::now(),
                                status: AttemptStatus::Failed,
                                error_class: Some(format!("{class:?}").to_ascii_lowercase()),
                                error_message: Some(err.to_string()),
                            };
                            if tx.send(Ok(StreamChunk::route_attempt(failed_attempt))).await.is_err() {
                                return;
                            }

                            // Retry the SAME candidate before rotating, exactly
                            // like the non-streaming loops do.
                            //
                            // This used to be gated on the presence of a
                            // structured `Retry-After` hint, which made the
                            // streaming path strictly weaker than the
                            // non-streaming one: a 429 or 503 from an upstream
                            // that sends no `Retry-After` header (most of them)
                            // got zero retries and, with a single candidate,
                            // killed the turn outright. The hint now only
                            // decides *how long* to wait, never *whether* to
                            // retry. Classes that cannot self-heal
                            // (`NonRetryable`) and context overflow (which only
                            // benefits from a different model) are excluded.
                            let hint_ms = parse_stream_retry_after_ms(&err);
                            let rate_limited = class == StreamFailureClass::RateLimited;
                            if rate_limited {
                                rate_limit::record_rate_limited(
                                    &attempt.provider_name,
                                    &attempt.model,
                                    true,
                                    hint_ms.is_some(),
                                );
                                rate_limited_seen = true;
                            }
                            // Retry budget and schedule depend on the failure kind.
                            let kind = stream_retry_kind(&err, class);
                            let retry_in_place =
                                matches!(class, StreamFailureClass::RateLimited | StreamFailureClass::Retryable)
                                    && !skips_in_place_retry(&err)
                                    && retries.may_retry(kind, max_retries, is_last);
                            let retry = retries.taken(kind) + 1;

                            if retry_in_place && kind == RetryKind::Overload {
                                let wait = overload_wait_ms(retry, hint_ms);
                                tracing::warn!(
                                    provider = %redact_provider_text(&attempt.provider_name),
                                    model = %attempt.model,
                                    attempt = retry,
                                    wait_ms = wait,
                                    retry_after_ms = hint_ms,
                                    "Provider overloaded before content; retrying same provider/model"
                                );
                                notice_overload_retry(retry, retry_budget(max_retries, RetryKind::Overload, is_last));
                                gate.note_rate_limited(&attempt.member_key, wait);
                                // Cancelling the turn drops the receiver and ends the wait.
                                tokio::select! {
                                    () = tx.closed() => return,
                                    () = tokio::time::sleep(Duration::from_millis(wait)) => {}
                                }
                                retries.record(kind);
                                last_error = Some(err);
                                continue 'candidate;
                            }
                            if retry_in_place {
                                let plan = match (kind, hint_ms) {
                                    (RetryKind::RateLimited, None) if is_last => {
                                        RetryWait::Sleep(rate_limit_wait_ms(retry))
                                    }
                                    (RetryKind::RateLimited, None) => RetryWait::Sleep(overload_wait_ms(retry, None)),
                                    _ => plan_wait_from_hint(backoff_ms, hint_ms),
                                };
                                match plan {
                                    RetryWait::Sleep(wait) => {
                                        tracing::warn!(
                                            provider = %redact_provider_text(&attempt.provider_name),
                                            model = %attempt.model,
                                            ?class,
                                            retry_after_ms = hint_ms,
                                            wait_ms = wait,
                                            attempt = retry,
                                            "Streaming failure before content; backing off and retrying same provider/model: {err}"
                                        );
                                        if kind == RetryKind::RateLimited {
                                            notice_overload_retry(
                                                retry,
                                                retry_budget(max_retries, RetryKind::RateLimited, is_last),
                                            );
                                        }
                                        if rate_limited {
                                            gate.note_rate_limited(&attempt.member_key, wait);
                                        }
                                        // Interruptible sleep: bail out early if the
                                        // receiver has gone away rather than waiting
                                        // out the full backoff for nobody.
                                        tokio::select! {
                                            () = tx.closed() => return,
                                            () = tokio::time::sleep(Duration::from_millis(wait)) => {}
                                        }
                                        retries.record(kind);
                                        backoff_ms = (backoff_ms.saturating_mul(2)).min(10_000);
                                        last_error = Some(err);
                                        continue 'candidate; // Retry the same candidate.
                                    }
                                    RetryWait::HintExceedsCap(requested_ms) => {
                                        tracing::warn!(
                                            provider = %redact_provider_text(&attempt.provider_name),
                                            model = %attempt.model,
                                            requested_retry_after_ms = requested_ms,
                                            max_honored_ms = MAX_HONORED_RETRY_AFTER_MS,
                                            "Upstream Retry-After exceeds the honored ceiling; not retrying this candidate in place"
                                        );
                                        gate.note_rate_limited(&attempt.member_key, requested_ms);
                                    }
                                }
                            }

                            if rate_limited {
                                rate_limit::record_exhausted(&attempt.provider_name);
                                // Publish the cool-down even though this request
                                // stops here, so concurrent streams defer too.
                                gate.note_rate_limited(
                                    &attempt.member_key,
                                    hint_ms.unwrap_or(backoff_ms).min(MAX_HONORED_RETRY_AFTER_MS),
                                );
                                tracing::error!(
                                    provider = %redact_provider_text(&attempt.provider_name),
                                    model = %attempt.model,
                                    attempts = retries.total() + 1,
                                    "Streaming rate limit outlasted the retry budget; abandoning this candidate"
                                );
                            } else if kind == RetryKind::Overload && !skips_in_place_retry(&err) {
                                tracing::warn!(
                                    provider = %redact_provider_text(&attempt.provider_name),
                                    model = %attempt.model,
                                    attempts = retries.total() + 1,
                                    "Streaming provider overload outlasted the retry budget; abandoning this candidate"
                                );
                            }

                            let can_fallback = !is_last
                                && (class.allows_fallback() || tool_schema_refusal)
                                && (class != StreamFailureClass::ContextOverflow || next_model_differs);

                            if can_fallback {
                                tracing::warn!(
                                    provider = %redact_provider_text(&attempt.provider_name),
                                    model = %attempt.model,
                                    ?class,
                                    "Streaming attempt failed before content; falling back to next candidate: {err}"
                                );
                                if tool_schema_refusal {
                                    last_refusal = Some(err);
                                } else {
                                    last_error = Some(err);
                                }
                                break 'candidate; // Try next candidate.
                            }

                            tracing::warn!(
                                provider = %redact_provider_text(&attempt.provider_name),
                                model = %attempt.model,
                                ?class,
                                "Streaming attempt failed with no viable failover; surfacing: {err}"
                            );
                            // A refusal after another candidate's transient
                            // failure is not what ended the request for good:
                            // the earlier failure is surfaced instead.
                            let surfaced = if tool_schema_refusal {
                                last_error.take().unwrap_or(err)
                            } else {
                                err
                            };
                            let _ = tx.send(Err(surfaced)).await;
                            return;
                        }
                    }
                }

                // Stream ended. If it produced content, we are done.
                if emitted_content {
                    if !success_reported {
                        let success = ProviderAttempt {
                            seq: attempt_seq,
                            provider: attempt.provider_name.to_string(),
                            model: attempt.model.clone(),
                            started_at: attempt_started_at,
                            finished_at: chrono::Utc::now(),
                            status: AttemptStatus::Success,
                            error_class: None,
                            error_message: None,
                        };
                        let mut chunk = StreamChunk::route_attempt(success);
                        chunk.served_member = Some(attempt.member_key.to_string());
                        let _ = tx.send(Ok(chunk)).await;
                    }
                    return;
                }
                // Empty, error-free stream: treat as a transient failure and try
                // the next candidate (parity with non-streaming "exhausted, try
                // next"). Break the per-candidate retry loop either way.
                complete_circuit(permit.take(), BreakerOutcome::Failure);
                let empty_attempt = ProviderAttempt {
                    seq: attempt_seq,
                    provider: attempt.provider_name.to_string(),
                    model: attempt.model.clone(),
                    started_at: attempt_started_at,
                    finished_at: chrono::Utc::now(),
                    status: AttemptStatus::Failed,
                    error_class: Some("empty_stream".to_string()),
                    error_message: Some("provider stream ended before emitting content".to_string()),
                };
                if tx.send(Ok(StreamChunk::route_attempt(empty_attempt))).await.is_err() {
                    return;
                }
                break 'candidate;
            }
        }

        // All candidates exhausted without content. Surface the last failure
        // that was not a refusal if there was one, else the last refusal,
        // otherwise a generic aggregate failure.
        if !circuit_refusals.is_empty() {
            let _ = tx
                .send(Err(super::super::traits::StreamError::Provider(format!(
                    "All streaming providers/models failed; {}",
                    circuit_refusals.join("; ")
                ))))
                .await;
            return;
        }
        let final_err = last_error.or(last_refusal).unwrap_or_else(|| {
            super::super::traits::StreamError::Provider("All streaming providers/models failed".to_string())
        });
        let _ = tx.send(Err(final_err)).await;
    });

    stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|chunk| (chunk, rx)) }).boxed()
}

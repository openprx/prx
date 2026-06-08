use super::Provider;
use super::traits::{ChatMessage, ChatRequest, ChatResponse, ChatTrace, StreamChunk, StreamOptions, StreamResult};
use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, ProviderExecutionOutcome, RouteDecision};
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

// ── Error Classification ─────────────────────────────────────────────────
// Errors are split into retryable (transient server/network failures) and
// non-retryable (permanent client errors). This distinction drives whether
// the retry loop continues, falls back to the next provider, or aborts
// immediately — avoiding wasted latency on errors that cannot self-heal.

/// Check if an error is non-retryable (client errors that won't resolve with retries).
fn is_non_retryable(err: &anyhow::Error) -> bool {
    if is_context_window_exceeded(err) {
        return true;
    }

    // 4xx errors are generally non-retryable (bad request, auth failure, etc.),
    // except 429 (rate-limit — transient) and 408 (timeout — worth retrying).
    if let Some(reqwest_err) = err.downcast_ref::<reqwest::Error>() {
        if let Some(status) = reqwest_err.status() {
            let code = status.as_u16();
            return status.is_client_error() && code != 429 && code != 408;
        }
    }
    // Fallback: parse status codes from stringified errors (some providers
    // embed codes in error messages rather than returning typed HTTP errors).
    let msg = err.to_string();
    for word in msg.split(|c: char| !c.is_ascii_digit()) {
        if let Ok(code) = word.parse::<u16>() {
            if (400..500).contains(&code) {
                return code != 429 && code != 408;
            }
        }
    }

    // Heuristic: detect auth/model failures by keyword when no HTTP status
    // is available (e.g. gRPC or custom transport errors).
    let msg_lower = msg.to_lowercase();
    let auth_failure_hints = [
        "invalid api key",
        "incorrect api key",
        "missing api key",
        "api key not set",
        "authentication failed",
        "auth failed",
        "unauthorized",
        "forbidden",
        "permission denied",
        "access denied",
        "invalid token",
    ];

    if auth_failure_hints.iter().any(|hint| msg_lower.contains(hint)) {
        return true;
    }
    if msg_lower.contains("provider_response_parse_error") {
        return is_non_retryable_provider_response_parse_error(&msg_lower);
    }

    msg_lower.contains("model")
        && (msg_lower.contains("not found")
            || msg_lower.contains("unknown")
            || msg_lower.contains("unsupported")
            || msg_lower.contains("does not exist")
            || msg_lower.contains("invalid"))
}

fn is_non_retryable_provider_response_parse_error(msg_lower: &str) -> bool {
    const NON_RETRYABLE_PARSE_ERROR_KINDS: [&str; 3] = [
        "kind=malformed_json",
        "kind=empty_or_unsupported_payload",
        "kind=payload_too_large",
    ];
    const RETRYABLE_PARSE_ERROR_KINDS: [&str; 1] = ["kind=body_read_failed"];
    const TRANSIENT_BODY_READ_HINTS: [&str; 9] = [
        "connection reset",
        "connection aborted",
        "connection closed",
        "connection refused",
        "broken pipe",
        "network unreachable",
        "temporary failure",
        "timed out",
        "timeout",
    ];

    if NON_RETRYABLE_PARSE_ERROR_KINDS
        .iter()
        .any(|kind| msg_lower.contains(kind))
    {
        return true;
    }

    if RETRYABLE_PARSE_ERROR_KINDS.iter().any(|kind| msg_lower.contains(kind)) {
        return false;
    }

    // Keep transient network read failures retryable even when kind tagging
    // differs by provider implementation details.
    if TRANSIENT_BODY_READ_HINTS.iter().any(|hint| msg_lower.contains(hint)) {
        return false;
    }

    // Unknown parse kinds remain non-retryable by default to preserve
    // fail-fast behavior on deterministic payload/protocol mismatches.
    true
}

fn is_context_window_exceeded(err: &anyhow::Error) -> bool {
    let lower = err.to_string().to_lowercase();
    let hints = [
        "exceeds the context window",
        "context window of this model",
        "maximum context length",
        "context length exceeded",
        "too many tokens",
        "token limit exceeded",
        "prompt is too long",
        "input is too long",
    ];

    hints.iter().any(|hint| lower.contains(hint))
}

/// Check if an error is a rate-limit (429) error.
fn is_rate_limited(err: &anyhow::Error) -> bool {
    if let Some(reqwest_err) = err.downcast_ref::<reqwest::Error>() {
        if let Some(status) = reqwest_err.status() {
            return status.as_u16() == 429;
        }
    }
    let msg = err.to_string();
    msg.contains("429") && (msg.contains("Too Many") || msg.contains("rate") || msg.contains("limit"))
}

/// Check if a 429 is a business/quota-plan error that retries cannot fix.
///
/// Examples:
/// - plan does not include requested model
/// - insufficient balance / package not active
/// - known provider business codes (e.g. Z.AI: 1311, 1113)
fn is_non_retryable_rate_limit(err: &anyhow::Error) -> bool {
    if !is_rate_limited(err) {
        return false;
    }

    let msg = err.to_string();
    let lower = msg.to_lowercase();

    let business_hints = [
        "plan does not include",
        "doesn't include",
        "not include",
        "insufficient balance",
        "insufficient_balance",
        "insufficient quota",
        "insufficient_quota",
        "quota exhausted",
        "out of credits",
        "no available package",
        "package not active",
        "purchase package",
        "model not available for your plan",
    ];

    if business_hints.iter().any(|hint| lower.contains(hint)) {
        return true;
    }

    // Known provider business codes observed for 429 where retry is futile.
    for token in lower.split(|c: char| !c.is_ascii_digit()) {
        if let Ok(code) = token.parse::<u16>() {
            if matches!(code, 1113 | 1311) {
                return true;
            }
        }
    }

    false
}

/// Try to extract a Retry-After value (in milliseconds) from an error message.
/// Looks for patterns like `Retry-After: 5` or `retry_after: 2.5` in the error string.
fn parse_retry_after_ms(err: &anyhow::Error) -> Option<u64> {
    let msg = err.to_string();
    let lower = msg.to_lowercase();

    // Look for "retry-after: <number>" or "retry_after: <number>"
    for prefix in &["retry-after:", "retry_after:", "retry-after ", "retry_after "] {
        if let Some(pos) = lower.find(prefix) {
            let after = &msg[pos + prefix.len()..];
            let num_str: String = after
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Ok(secs) = num_str.parse::<f64>() {
                if secs.is_finite() && secs >= 0.0 {
                    let millis = Duration::from_secs_f64(secs).as_millis();
                    if let Ok(value) = u64::try_from(millis) {
                        return Some(value);
                    }
                }
            }
        }
    }
    None
}

const fn failure_reason(rate_limited: bool, non_retryable: bool) -> &'static str {
    if rate_limited && non_retryable {
        "rate_limited_non_retryable"
    } else if rate_limited {
        "rate_limited"
    } else if non_retryable {
        "non_retryable"
    } else {
        "retryable"
    }
}

fn compact_error_detail(err: &anyhow::Error) -> String {
    super::sanitize_api_error(&err.to_string())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn push_failure(
    failures: &mut Vec<String>,
    provider_name: &str,
    model: &str,
    attempt: u32,
    max_attempts: u32,
    reason: &str,
    error_detail: &str,
) {
    failures.push(format!(
        "provider={provider_name} model={model} attempt {attempt}/{max_attempts}: {reason}; error={error_detail}"
    ));
}

/// Build a `ProviderAttempt` record for a single (provider, model) call.
///
/// `error` is `Some` for a failed attempt (the error is classified via
/// [`classify_provider_error`] and its sanitized message truncated to 500
/// chars, mirroring `failed_for_decision`), `None` for a terminal success.
///
/// `seq` uses `saturating_add(u8)` at the call site: the attempt counter is a
/// `u8` because the bounded failover space (model_chain × providers × retries)
/// realistically stays well under 255; if it ever saturated, later attempts
/// would all carry `seq = 255` rather than wrapping to a misleading low value.
fn build_attempt(
    seq: u8,
    provider_name: &str,
    model: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    finished_at: chrono::DateTime<chrono::Utc>,
    error: Option<&anyhow::Error>,
) -> ProviderAttempt {
    let (status, error_class, error_message) = error.map_or((AttemptStatus::Success, None, None), |err| {
        (
            AttemptStatus::Failed,
            Some(crate::llm::route_decision::classify_provider_error(err)),
            Some(super::sanitize_api_error(&err.to_string()).chars().take(500).collect()),
        )
    });
    ProviderAttempt {
        seq,
        provider: provider_name.to_string(),
        model: model.to_string(),
        started_at,
        finished_at,
        status,
        error_class,
        error_message,
    }
}

// ── Resilient Provider Wrapper ────────────────────────────────────────────
// Three-level failover strategy: model chain → provider chain → retry loop.
//   Outer loop:  iterate model fallback chain (original model first, then
//                configured alternatives).
//   Middle loop: iterate registered providers in priority order.
//   Inner loop:  retry the same (provider, model) pair with exponential
//                backoff, rotating API keys on rate-limit errors.
// Loop invariant: `failures` accumulates every failed attempt so the final
// error message gives operators a complete diagnostic trail.

/// Provider wrapper with retry, fallback, auth rotation, and model failover.
pub struct ReliableProvider {
    // Providers are stored as `Arc` so the streaming fallback driver can clone a
    // handle into a `'static` background task (the trait's streaming methods
    // return `'static` streams, so the driver cannot borrow `self`). Non-stream
    // call sites continue to use the providers via `Deref` to `dyn Provider`,
    // so behavior is unchanged for them.
    providers: Vec<(Arc<str>, Arc<dyn Provider>)>,
    max_retries: u32,
    base_backoff_ms: u64,
    /// Per-model fallback chains: model_name → [fallback_model_1, fallback_model_2, ...]
    model_fallbacks: HashMap<String, Vec<String>>,
    /// Providers filtered out at startup with reasons (invalid/missing credential/init failure).
    unavailable_providers: Vec<(String, String)>,
}

impl ReliableProvider {
    pub fn new(providers: Vec<(String, Box<dyn Provider>)>, max_retries: u32, base_backoff_ms: u64) -> Self {
        let providers = providers
            .into_iter()
            .map(|(name, provider)| {
                let name: Arc<str> = Arc::from(name);
                let provider: Arc<dyn Provider> = Arc::from(provider);
                (name, provider)
            })
            .collect();
        Self {
            providers,
            max_retries,
            base_backoff_ms: base_backoff_ms.max(50),
            model_fallbacks: HashMap::new(),
            unavailable_providers: Vec::new(),
        }
    }

    /// Set per-model fallback chains.
    pub fn with_model_fallbacks(mut self, fallbacks: HashMap<String, Vec<String>>) -> Self {
        self.model_fallbacks = fallbacks;
        self
    }

    /// Attach provider availability failures captured during startup.
    pub fn with_unavailable_providers(mut self, unavailable: Vec<(String, String)>) -> Self {
        self.unavailable_providers = unavailable;
        self
    }

    fn provider_model_compatible(&self, provider_name: &str, model: &str) -> bool {
        super::provider_matches_model_prefix(provider_name, model)
    }

    fn all_failed_message(&self, failures: &[String], runtime_unavailable: &[(String, String)]) -> String {
        let available = self
            .providers
            .iter()
            .map(|(name, _)| name.to_string())
            .collect::<Vec<_>>()
            .join(", ");

        let mut unavailable = self.unavailable_providers.clone();
        unavailable.extend(runtime_unavailable.iter().cloned());
        let unavailable_text = if unavailable.is_empty() {
            "none".to_string()
        } else {
            unavailable
                .into_iter()
                .map(|(name, reason)| format!("{name}: {reason}"))
                .collect::<Vec<_>>()
                .join("; ")
        };

        format!(
            "All providers/models failed. Available providers: [{}]. Unavailable providers: {}. Attempts:\n{}",
            available,
            unavailable_text,
            failures.join("\n")
        )
    }

    /// Build the list of models to try: [original, fallback1, fallback2, ...]
    fn model_chain<'a>(&'a self, model: &'a str) -> Vec<&'a str> {
        let mut chain = vec![model];
        if let Some(fallbacks) = self.model_fallbacks.get(model) {
            chain.extend(fallbacks.iter().map(|s| s.as_str()));
        }
        chain
    }

    /// Compute backoff duration, respecting Retry-After if present.
    fn compute_backoff(&self, base: u64, err: &anyhow::Error) -> u64 {
        parse_retry_after_ms(err).map_or(base, |retry_after| retry_after.min(30_000).max(base))
    }
}

#[async_trait]
impl Provider for ReliableProvider {
    async fn warmup(&self) -> anyhow::Result<()> {
        for (name, provider) in &self.providers {
            tracing::info!(provider = %name, "Warming up provider connection pool");
            if provider.warmup().await.is_err() {
                tracing::warn!(provider = %name, "Warmup failed (non-fatal)");
            }
        }
        Ok(())
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let models = self.model_chain(model);
        let mut failures = Vec::new();
        let mut runtime_unavailable = Vec::new();

        // Outer: model fallback chain. Middle: provider priority. Inner: retries.
        // Each iteration: attempt one (provider, model) call. On success, return
        // immediately. On non-retryable error, break to next provider. On
        // retryable error, sleep with exponential backoff and retry.
        for current_model in &models {
            for (provider_name, provider) in &self.providers {
                if !self.provider_model_compatible(provider_name, current_model) {
                    runtime_unavailable.push((
                        provider_name.to_string(),
                        format!("model '{current_model}' not compatible with provider"),
                    ));
                    continue;
                }

                let mut backoff_ms = self.base_backoff_ms;

                for attempt in 0..=self.max_retries {
                    match provider
                        .chat_with_system(system_prompt, message, current_model, temperature)
                        .await
                    {
                        Ok(resp) => {
                            if attempt > 0 || *current_model != model {
                                tracing::info!(
                                    provider = %provider_name,
                                    model = *current_model,
                                    attempt,
                                    original_model = model,
                                    "Provider recovered (failover/retry)"
                                );
                            }
                            return Ok(resp);
                        }
                        Err(e) => {
                            let non_retryable_rate_limit = is_non_retryable_rate_limit(&e);
                            let non_retryable = is_non_retryable(&e) || non_retryable_rate_limit;
                            let rate_limited = is_rate_limited(&e);
                            let failure_reason = failure_reason(rate_limited, non_retryable);
                            let error_detail = compact_error_detail(&e);

                            push_failure(
                                &mut failures,
                                provider_name,
                                current_model,
                                attempt + 1,
                                self.max_retries + 1,
                                failure_reason,
                                &error_detail,
                            );

                            if non_retryable {
                                tracing::warn!(
                                    provider = %provider_name,
                                    model = *current_model,
                                    error = %error_detail,
                                    "Non-retryable error, moving on"
                                );

                                if is_context_window_exceeded(&e) {
                                    anyhow::bail!(
                                        "Request exceeds model context window; retries and fallbacks were skipped. Attempts:\n{}",
                                        failures.join("\n")
                                    );
                                }

                                break;
                            }

                            if attempt < self.max_retries {
                                let wait = self.compute_backoff(backoff_ms, &e);
                                tracing::warn!(
                                    provider = %provider_name,
                                    model = *current_model,
                                    attempt = attempt + 1,
                                    backoff_ms = wait,
                                    reason = failure_reason,
                                    error = %error_detail,
                                    "Provider call failed, retrying"
                                );
                                tokio::time::sleep(Duration::from_millis(wait)).await;
                                backoff_ms = (backoff_ms.saturating_mul(2)).min(10_000);
                            }
                        }
                    }
                }

                tracing::warn!(
                    provider = %provider_name,
                    model = *current_model,
                    "Exhausted retries, trying next provider/model"
                );
            }

            if *current_model != model {
                tracing::warn!(
                    original_model = model,
                    fallback_model = *current_model,
                    "Model fallback exhausted all providers, trying next fallback model"
                );
            }
        }

        anyhow::bail!(self.all_failed_message(&failures, &runtime_unavailable))
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let models = self.model_chain(model);
        let mut failures = Vec::new();
        let mut runtime_unavailable = Vec::new();

        for current_model in &models {
            for (provider_name, provider) in &self.providers {
                if !self.provider_model_compatible(provider_name, current_model) {
                    runtime_unavailable.push((
                        provider_name.to_string(),
                        format!("model '{current_model}' not compatible with provider"),
                    ));
                    continue;
                }

                let mut backoff_ms = self.base_backoff_ms;

                for attempt in 0..=self.max_retries {
                    match provider.chat_with_history(messages, current_model, temperature).await {
                        Ok(resp) => {
                            if attempt > 0 || *current_model != model {
                                tracing::info!(
                                    provider = %provider_name,
                                    model = *current_model,
                                    attempt,
                                    original_model = model,
                                    "Provider recovered (failover/retry)"
                                );
                            }
                            return Ok(resp);
                        }
                        Err(e) => {
                            let non_retryable_rate_limit = is_non_retryable_rate_limit(&e);
                            let non_retryable = is_non_retryable(&e) || non_retryable_rate_limit;
                            let rate_limited = is_rate_limited(&e);
                            let failure_reason = failure_reason(rate_limited, non_retryable);
                            let error_detail = compact_error_detail(&e);

                            push_failure(
                                &mut failures,
                                provider_name,
                                current_model,
                                attempt + 1,
                                self.max_retries + 1,
                                failure_reason,
                                &error_detail,
                            );

                            if non_retryable {
                                tracing::warn!(
                                    provider = %provider_name,
                                    model = *current_model,
                                    error = %error_detail,
                                    "Non-retryable error, moving on"
                                );

                                if is_context_window_exceeded(&e) {
                                    anyhow::bail!(
                                        "Request exceeds model context window; retries and fallbacks were skipped. Attempts:\n{}",
                                        failures.join("\n")
                                    );
                                }

                                break;
                            }

                            if attempt < self.max_retries {
                                let wait = self.compute_backoff(backoff_ms, &e);
                                tracing::warn!(
                                    provider = %provider_name,
                                    model = *current_model,
                                    attempt = attempt + 1,
                                    backoff_ms = wait,
                                    reason = failure_reason,
                                    error = %error_detail,
                                    "Provider call failed, retrying"
                                );
                                tokio::time::sleep(Duration::from_millis(wait)).await;
                                backoff_ms = (backoff_ms.saturating_mul(2)).min(10_000);
                            }
                        }
                    }
                }

                tracing::warn!(
                    provider = %provider_name,
                    model = *current_model,
                    "Exhausted retries, trying next provider/model"
                );
            }
        }

        anyhow::bail!(self.all_failed_message(&failures, &runtime_unavailable))
    }

    async fn chat(&self, request: ChatRequest<'_>, model: &str, temperature: f64) -> anyhow::Result<ChatResponse> {
        // Thin shell over `chat_traced`: the structured chat path runs the full
        // failover loop in `chat_traced` and discards the trace here. The
        // `Provider` trait signature is preserved so every existing caller is
        // unaffected (FIX-P0-30/31).
        Ok(self.chat_traced(request, model, temperature).await?.response)
    }

    /// Structured chat that returns the real failover trace.
    ///
    /// FIX-P0-30 / FIX-P0-31: this is the single source of the non-streaming
    /// three-level failover loop (model chain × provider × retry). It records a
    /// `ProviderAttempt` for **every** failed attempt and a terminal `Success`
    /// attempt, and reports the provider/model that *actually* served the
    /// request (rather than the routed `decision.selected.model`).
    async fn chat_traced(&self, request: ChatRequest<'_>, model: &str, temperature: f64) -> anyhow::Result<ChatTrace> {
        let models = self.model_chain(model);
        let mut failures = Vec::new();
        let mut runtime_unavailable = Vec::new();
        let mut attempts: Vec<ProviderAttempt> = Vec::new();
        // `seq` counts every recorded attempt. `u8` + `saturating_add` keeps the
        // counter monotonic even in the (unreachable in practice) event that the
        // bounded failover space exceeds 255 attempts.
        let mut seq: u8 = 0;

        for current_model in &models {
            for (provider_name, provider) in &self.providers {
                if !self.provider_model_compatible(provider_name, current_model) {
                    runtime_unavailable.push((
                        provider_name.to_string(),
                        format!("model '{current_model}' not compatible with provider"),
                    ));
                    continue;
                }

                let mut backoff_ms = self.base_backoff_ms;

                for attempt in 0..=self.max_retries {
                    let attempt_started_at = chrono::Utc::now();
                    match provider.chat(request, current_model, temperature).await {
                        Ok(resp) => {
                            if attempt > 0 || *current_model != model {
                                tracing::info!(
                                    provider = %provider_name,
                                    model = *current_model,
                                    attempt,
                                    original_model = model,
                                    "Provider recovered (failover/retry)"
                                );
                            }
                            seq = seq.saturating_add(1);
                            attempts.push(build_attempt(
                                seq,
                                provider_name,
                                current_model,
                                attempt_started_at,
                                chrono::Utc::now(),
                                None,
                            ));
                            return Ok(ChatTrace {
                                response: resp,
                                attempts,
                                final_provider: provider_name.to_string(),
                                final_model: (*current_model).to_string(),
                            });
                        }
                        Err(e) => {
                            let non_retryable_rate_limit = is_non_retryable_rate_limit(&e);
                            let non_retryable = is_non_retryable(&e) || non_retryable_rate_limit;
                            let rate_limited = is_rate_limited(&e);
                            let failure_reason = failure_reason(rate_limited, non_retryable);
                            let error_detail = compact_error_detail(&e);

                            push_failure(
                                &mut failures,
                                provider_name,
                                current_model,
                                attempt + 1,
                                self.max_retries + 1,
                                failure_reason,
                                &error_detail,
                            );
                            seq = seq.saturating_add(1);
                            attempts.push(build_attempt(
                                seq,
                                provider_name,
                                current_model,
                                attempt_started_at,
                                chrono::Utc::now(),
                                Some(&e),
                            ));

                            if non_retryable {
                                tracing::warn!(
                                    provider = %provider_name,
                                    model = *current_model,
                                    error = %error_detail,
                                    "Non-retryable error, moving on"
                                );

                                if is_context_window_exceeded(&e) {
                                    anyhow::bail!(
                                        "Request exceeds model context window; retries and fallbacks were skipped. Attempts:\n{}",
                                        failures.join("\n")
                                    );
                                }

                                break;
                            }

                            if attempt < self.max_retries {
                                let wait = self.compute_backoff(backoff_ms, &e);
                                tracing::warn!(
                                    provider = %provider_name,
                                    model = *current_model,
                                    attempt = attempt + 1,
                                    backoff_ms = wait,
                                    reason = failure_reason,
                                    error = %error_detail,
                                    "Provider call failed, retrying"
                                );
                                tokio::time::sleep(Duration::from_millis(wait)).await;
                                backoff_ms = (backoff_ms.saturating_mul(2)).min(10_000);
                            }
                        }
                    }
                }

                tracing::warn!(
                    provider = %provider_name,
                    model = *current_model,
                    "Exhausted retries, trying next provider/model"
                );
            }
        }

        anyhow::bail!(self.all_failed_message(&failures, &runtime_unavailable))
    }

    async fn chat_with_decision(
        &self,
        decision: &RouteDecision,
        request: ChatRequest<'_>,
        temperature: f64,
    ) -> anyhow::Result<(ChatResponse, ProviderExecutionOutcome)> {
        let started_at = chrono::Utc::now();
        // FIX-P0-30/31: build the outcome from the real trace so the recorded
        // final_provider/final_model and attempt sequence reflect what actually
        // executed, and the status distinguishes a clean Success from a
        // retry/provider/model FallbackSuccess.
        let trace = self
            .chat_traced(request, decision.effective_model(), temperature)
            .await?;
        let finished_at = chrono::Utc::now();
        let outcome = ProviderExecutionOutcome::from_trace(
            decision,
            trace.attempts,
            trace.final_provider,
            trace.final_model,
            started_at,
            finished_at,
        );
        Ok((trace.response, outcome))
    }

    fn supports_native_tools(&self) -> bool {
        self.providers
            .iter()
            .any(|(_, provider)| provider.supports_native_tools())
    }

    fn supports_vision(&self) -> bool {
        self.providers.iter().any(|(_, provider)| provider.supports_vision())
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let models = self.model_chain(model);
        let mut failures = Vec::new();
        let mut runtime_unavailable = Vec::new();

        for current_model in &models {
            for (provider_name, provider) in &self.providers {
                if !self.provider_model_compatible(provider_name, current_model) {
                    runtime_unavailable.push((
                        provider_name.to_string(),
                        format!("model '{current_model}' not compatible with provider"),
                    ));
                    continue;
                }

                let mut backoff_ms = self.base_backoff_ms;

                for attempt in 0..=self.max_retries {
                    match provider
                        .chat_with_tools(messages, tools, current_model, temperature)
                        .await
                    {
                        Ok(resp) => {
                            if attempt > 0 || *current_model != model {
                                tracing::info!(
                                    provider = %provider_name,
                                    model = *current_model,
                                    attempt,
                                    original_model = model,
                                    "Provider recovered (failover/retry)"
                                );
                            }
                            return Ok(resp);
                        }
                        Err(e) => {
                            let non_retryable_rate_limit = is_non_retryable_rate_limit(&e);
                            let non_retryable = is_non_retryable(&e) || non_retryable_rate_limit;
                            let rate_limited = is_rate_limited(&e);
                            let failure_reason = failure_reason(rate_limited, non_retryable);
                            let error_detail = compact_error_detail(&e);

                            push_failure(
                                &mut failures,
                                provider_name,
                                current_model,
                                attempt + 1,
                                self.max_retries + 1,
                                failure_reason,
                                &error_detail,
                            );

                            if non_retryable {
                                tracing::warn!(
                                    provider = %provider_name,
                                    model = *current_model,
                                    error = %error_detail,
                                    "Non-retryable error, moving on"
                                );

                                if is_context_window_exceeded(&e) {
                                    anyhow::bail!(
                                        "Request exceeds model context window; retries and fallbacks were skipped. Attempts:\n{}",
                                        failures.join("\n")
                                    );
                                }

                                break;
                            }

                            if attempt < self.max_retries {
                                let wait = self.compute_backoff(backoff_ms, &e);
                                tracing::warn!(
                                    provider = %provider_name,
                                    model = *current_model,
                                    attempt = attempt + 1,
                                    backoff_ms = wait,
                                    reason = failure_reason,
                                    error = %error_detail,
                                    "Provider call failed, retrying"
                                );
                                tokio::time::sleep(Duration::from_millis(wait)).await;
                                backoff_ms = (backoff_ms.saturating_mul(2)).min(10_000);
                            }
                        }
                    }
                }

                tracing::warn!(
                    provider = %provider_name,
                    model = *current_model,
                    "Exhausted retries, trying next provider/model"
                );
            }
        }

        anyhow::bail!(self.all_failed_message(&failures, &runtime_unavailable))
    }

    fn supports_streaming(&self) -> bool {
        self.providers.iter().any(|(_, p)| p.supports_streaming())
    }

    /// Stream with full failover parity to the non-streaming path.
    ///
    /// Mirrors the non-streaming model_chain × provider failover: each candidate
    /// (model, streaming-capable provider) is attempted in order. If a candidate
    /// fails *before emitting any content*, the failure is classified
    /// ([`classify_stream_error`]) and — when it is recoverable (retryable /
    /// rate-limited / context-overflow on a non-last model) — the driver falls
    /// back to the next candidate. Once a candidate has emitted content, its
    /// output is forwarded verbatim and a later error is surfaced as-is (we never
    /// silently switch providers mid-content, which would corrupt the response).
    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let attempts = self.streaming_attempts(model, &options);
        if attempts.is_empty() {
            return no_streaming_provider_stream();
        }
        let messages: Vec<ChatMessage> = messages.to_vec();
        drive_streaming_fallback(
            attempts,
            temperature,
            options,
            move |provider, model, temperature, options| {
                provider.stream_chat_with_history(&messages, model, temperature, options)
            },
        )
    }

    fn stream_chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let attempts = self.streaming_attempts(model, &options);
        if attempts.is_empty() {
            return no_streaming_provider_stream();
        }
        let system_prompt: Option<String> = system_prompt.map(ToString::to_string);
        let message: String = message.to_string();
        drive_streaming_fallback(
            attempts,
            temperature,
            options,
            move |provider, model, temperature, options| {
                provider.stream_chat_with_system(system_prompt.as_deref(), &message, model, temperature, options)
            },
        )
    }
}

impl ReliableProvider {
    /// Build the ordered list of streaming attempts: model_chain × streaming-capable,
    /// model-compatible providers. Empty when streaming is disabled or unsupported.
    fn streaming_attempts(&self, model: &str, options: &StreamOptions) -> Vec<StreamAttempt> {
        if !options.enabled {
            return Vec::new();
        }
        let mut attempts = Vec::new();
        for current_model in self.model_chain(model) {
            for (provider_name, provider) in &self.providers {
                if !provider.supports_streaming() {
                    continue;
                }
                if !self.provider_model_compatible(provider_name, current_model) {
                    continue;
                }
                attempts.push(StreamAttempt {
                    provider_name: Arc::clone(provider_name),
                    provider: Arc::clone(provider),
                    model: current_model.to_string(),
                });
            }
        }
        attempts
    }
}

/// One candidate (provider, model) pair for a streaming request.
struct StreamAttempt {
    provider_name: Arc<str>,
    provider: Arc<dyn Provider>,
    model: String,
}

/// Classification of a streaming error, mirroring the non-streaming
/// `is_*` classifiers so the streaming fallback driver can make the same
/// retry/fallback/abort decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamFailureClass {
    /// Rate-limited (429) and recoverable by trying another provider/model.
    RateLimited,
    /// Input exceeds the model context window — fall back to a larger model if
    /// one remains in the chain, otherwise abort (retrying the same model is futile).
    ContextOverflow,
    /// Permanent client error (auth/model-not-found/business 429) — do not retry,
    /// but a different compatible provider/model may still succeed.
    NonRetryable,
    /// Transient server/network failure — worth trying the next candidate.
    Retryable,
}

impl StreamFailureClass {
    /// Whether falling back to the next candidate is worthwhile for this class.
    /// Context overflow only benefits from fallback to a *different* (larger)
    /// model; that gating is applied by the driver using the candidate list.
    const fn allows_fallback(self) -> bool {
        matches!(self, Self::RateLimited | Self::Retryable | Self::ContextOverflow)
    }
}

/// Classify a [`StreamError`] for failover decisions. Reuses the same
/// string/`reqwest` heuristics as the non-streaming path by routing through the
/// shared `anyhow` classifiers where possible.
fn classify_stream_error(err: &super::traits::StreamError) -> StreamFailureClass {
    use super::traits::StreamError;

    // HTTP transport errors carry a status we can map directly.
    if let StreamError::Http(http_err) = err {
        if let Some(status) = http_err.status() {
            let code = status.as_u16();
            if code == 429 {
                return StreamFailureClass::RateLimited;
            }
            if code == 413 {
                return StreamFailureClass::ContextOverflow;
            }
            if status.is_client_error() && code != 408 {
                return StreamFailureClass::NonRetryable;
            }
        }
        // Timeouts / connects / unknown HTTP failures are transient.
        return StreamFailureClass::Retryable;
    }

    // For message-bearing variants, reuse the anyhow-based classifiers so the
    // streaming path stays in lock-step with the non-streaming path.
    let message = match err {
        StreamError::Provider(msg) | StreamError::InvalidSse(msg) => msg.clone(),
        StreamError::Json(e) => e.to_string(),
        StreamError::Io(_) => return StreamFailureClass::Retryable,
        // `StreamError::Http` is fully classified by the status-code block above,
        // which always returns before reaching this match. Should that invariant
        // ever change, treat an unclassified HTTP failure as transient rather
        // than panicking.
        StreamError::Http(_) => return StreamFailureClass::Retryable,
    };
    let anyhow_err = anyhow::anyhow!("{message}");

    if is_context_window_exceeded(&anyhow_err) {
        return StreamFailureClass::ContextOverflow;
    }
    if is_non_retryable_rate_limit(&anyhow_err) {
        return StreamFailureClass::NonRetryable;
    }
    if is_rate_limited(&anyhow_err) {
        return StreamFailureClass::RateLimited;
    }
    if is_non_retryable(&anyhow_err) {
        return StreamFailureClass::NonRetryable;
    }
    StreamFailureClass::Retryable
}

/// Error stream returned when no streaming-capable provider is available.
fn no_streaming_provider_stream() -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
    stream::once(async move {
        Err(super::traits::StreamError::Provider(
            "No provider supports streaming".to_string(),
        ))
    })
    .boxed()
}

/// Drive a streaming request across the candidate list with failover.
///
/// `build_stream` creates the per-attempt provider stream from owned request
/// data. The driver:
/// 1. Tries candidates in order, buffering the first chunk of each.
/// 2. If the first chunk is an error that [allows fallback](StreamFailureClass::allows_fallback)
///    and another viable candidate remains, moves on (context-overflow only
///    falls back to a *different* model). Otherwise the error is surfaced.
/// 3. Once content has started, forwards the remaining chunks verbatim — a
///    mid-content error is never swallowed (switching providers would corrupt
///    the already-emitted response).
fn drive_streaming_fallback<F>(
    attempts: Vec<StreamAttempt>,
    temperature: f64,
    options: StreamOptions,
    build_stream: F,
) -> stream::BoxStream<'static, StreamResult<StreamChunk>>
where
    F: Fn(&Arc<dyn Provider>, &str, f64, StreamOptions) -> stream::BoxStream<'static, StreamResult<StreamChunk>>
        + Send
        + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(100);

    tokio::spawn(async move {
        let total = attempts.len();
        let mut last_error: Option<super::traits::StreamError> = None;

        for (index, attempt) in attempts.iter().enumerate() {
            let is_last = index + 1 == total;
            // For context overflow, only a *different* model is worth trying.
            let next_model_differs = attempts.get(index + 1).is_some_and(|next| next.model != attempt.model);

            let mut stream = build_stream(&attempt.provider, &attempt.model, temperature, options.clone());
            let mut emitted_content = false;

            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(content) => {
                        emitted_content = true;
                        if tx.send(Ok(content)).await.is_err() {
                            return; // Receiver dropped.
                        }
                    }
                    Err(err) => {
                        // Mid-content failure: surface as-is, never switch providers.
                        if emitted_content {
                            tracing::warn!(
                                provider = %attempt.provider_name,
                                model = %attempt.model,
                                "Streaming error after content emitted; surfacing without failover: {err}"
                            );
                            let _ = tx.send(Err(err)).await;
                            return;
                        }

                        // Pre-content failure: classify and decide on failover.
                        let class = classify_stream_error(&err);
                        let can_fallback = !is_last
                            && class.allows_fallback()
                            && (class != StreamFailureClass::ContextOverflow || next_model_differs);

                        if can_fallback {
                            tracing::warn!(
                                provider = %attempt.provider_name,
                                model = %attempt.model,
                                ?class,
                                "Streaming attempt failed before content; falling back to next candidate: {err}"
                            );
                            last_error = Some(err);
                            break; // Try next candidate.
                        }

                        tracing::warn!(
                            provider = %attempt.provider_name,
                            model = %attempt.model,
                            ?class,
                            "Streaming attempt failed with no viable failover; surfacing: {err}"
                        );
                        let _ = tx.send(Err(err)).await;
                        return;
                    }
                }
            }

            // Stream ended. If it produced content, we are done.
            if emitted_content {
                return;
            }
            // Empty, error-free stream: treat as a transient failure and try the
            // next candidate (parity with non-streaming "exhausted, try next").
            if !is_last {
                continue;
            }
        }

        // All candidates exhausted without content. Surface the last error if we
        // captured one, otherwise a generic aggregate failure.
        let final_err = last_error.unwrap_or_else(|| {
            super::traits::StreamError::Provider("All streaming providers/models failed".to_string())
        });
        let _ = tx.send(Err(final_err)).await;
    });

    stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|chunk| (chunk, rx)) }).boxed()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::needless_collect,
        clippy::unreadable_literal
    )]
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockProvider {
        calls: Arc<AtomicUsize>,
        fail_until_attempt: usize,
        response: &'static str,
        error: &'static str,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt <= self.fail_until_attempt {
                anyhow::bail!(self.error);
            }
            Ok(self.response.to_string())
        }

        async fn chat_with_history(
            &self,
            _messages: &[ChatMessage],
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt <= self.fail_until_attempt {
                anyhow::bail!(self.error);
            }
            Ok(self.response.to_string())
        }
    }

    /// Mock that records which model was used for each call.
    struct ModelAwareMock {
        calls: Arc<AtomicUsize>,
        models_seen: parking_lot::Mutex<Vec<String>>,
        fail_models: Vec<&'static str>,
        response: &'static str,
    }

    #[async_trait]
    impl Provider for ModelAwareMock {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.models_seen.lock().push(model.to_string());
            if self.fail_models.contains(&model) {
                anyhow::bail!("500 model {} unavailable", model);
            }
            Ok(self.response.to_string())
        }
    }

    struct NativeCapabilityMock {
        native_tools: bool,
    }

    #[async_trait]
    impl Provider for NativeCapabilityMock {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("ok".to_string())
        }

        fn supports_native_tools(&self) -> bool {
            self.native_tools
        }
    }

    // ── Existing tests (preserved) ──

    #[tokio::test]
    async fn succeeds_without_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![(
                "primary".into(),
                Box::new(MockProvider {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: 0,
                    response: "ok",
                    error: "boom",
                }),
            )],
            2,
            1,
        );

        let result = provider.simple_chat("hello", "test", 0.0).await.unwrap();
        assert_eq!(result, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_then_recovers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![(
                "primary".into(),
                Box::new(MockProvider {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: 1,
                    response: "recovered",
                    error: "temporary",
                }),
            )],
            2,
            1,
        );

        let result = provider.simple_chat("hello", "test", 0.0).await.unwrap();
        assert_eq!(result, "recovered");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn falls_back_after_retries_exhausted() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));

        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(MockProvider {
                        calls: Arc::clone(&primary_calls),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "primary down",
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(MockProvider {
                        calls: Arc::clone(&fallback_calls),
                        fail_until_attempt: 0,
                        response: "from fallback",
                        error: "fallback down",
                    }),
                ),
            ],
            1,
            1,
        );

        let result = provider.simple_chat("hello", "test", 0.0).await.unwrap();
        assert_eq!(result, "from fallback");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 2);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    // ── FIX-P0-30/31: chat_traced attempt accumulation + fallback attribution ──

    #[tokio::test]
    async fn chat_traced_accumulates_attempts_and_reports_real_final_model() {
        use crate::llm::route_decision::AttemptStatus;
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));

        // Primary always fails (retryable) → exhausts retries; fallback succeeds.
        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(MockProvider {
                        calls: Arc::clone(&primary_calls),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "500 primary down",
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(MockProvider {
                        calls: Arc::clone(&fallback_calls),
                        fail_until_attempt: 0,
                        response: "from fallback",
                        error: "fallback down",
                    }),
                ),
            ],
            1, // max_retries = 1 → 2 attempts per provider before moving on.
            1,
        );

        let messages = vec![ChatMessage::user("hi")];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
        };
        let trace = provider.chat_traced(request, "test", 0.0).await.unwrap();

        assert_eq!(trace.response.text_or_empty(), "from fallback");
        // Primary: 2 failed attempts (attempt 0 + retry). Fallback: 1 success.
        assert_eq!(trace.attempts.len(), 3, "two primary failures + one fallback success");

        // seq is strictly increasing starting at 1.
        for (idx, attempt) in trace.attempts.iter().enumerate() {
            assert_eq!(attempt.seq as usize, idx + 1, "seq must be monotonic from 1");
        }

        // The two primary attempts are Failed with a classified error; the final
        // fallback attempt is Success with no error.
        assert_eq!(trace.attempts[0].status, AttemptStatus::Failed);
        assert_eq!(trace.attempts[0].provider, "primary");
        assert!(trace.attempts[0].error_class.is_some());
        assert_eq!(trace.attempts[1].status, AttemptStatus::Failed);
        assert_eq!(trace.attempts[1].provider, "primary");
        assert_eq!(trace.attempts[2].status, AttemptStatus::Success);
        assert_eq!(trace.attempts[2].provider, "fallback");
        assert!(trace.attempts[2].error_class.is_none());

        // final_provider/final_model reflect what actually executed.
        assert_eq!(trace.final_provider, "fallback");
        assert_eq!(trace.final_model, "test");
    }

    #[tokio::test]
    async fn chat_with_decision_marks_fallback_success_with_real_final_provider() {
        use crate::llm::route_decision::{ExecutionStatus, RouteDecision};

        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(MockProvider {
                        calls: Arc::new(AtomicUsize::new(0)),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "500 primary down",
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(MockProvider {
                        calls: Arc::new(AtomicUsize::new(0)),
                        fail_until_attempt: 0,
                        response: "served by fallback",
                        error: "unused",
                    }),
                ),
            ],
            0, // no retries → straight provider fallback.
            1,
        );

        // Router selected "primary" but it fails; "fallback" actually serves.
        let decision = RouteDecision::single_candidate("primary", "test");
        let messages = vec![ChatMessage::user("hi")];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
        };

        let (response, outcome) = provider.chat_with_decision(&decision, request, 0.0).await.unwrap();
        assert_eq!(response.text_or_empty(), "served by fallback");

        // A provider switch must be recorded as FallbackSuccess (not Success).
        assert_eq!(outcome.status, ExecutionStatus::FallbackSuccess);
        assert_eq!(outcome.fallback_reason.as_deref(), Some("provider_fallback"));
        // final_provider differs from the routed selection.
        assert_eq!(outcome.final_provider, "fallback");
        assert_ne!(outcome.final_provider, decision.selected.provider);
        // Two attempts: one failed (primary) + one success (fallback).
        assert_eq!(outcome.attempts.len(), 2);
    }

    #[tokio::test]
    async fn chat_with_decision_clean_success_is_not_fallback() {
        use crate::llm::route_decision::{ExecutionStatus, RouteDecision};

        let provider = ReliableProvider::new(
            vec![(
                "primary".into(),
                Box::new(MockProvider {
                    calls: Arc::new(AtomicUsize::new(0)),
                    fail_until_attempt: 0,
                    response: "first try",
                    error: "unused",
                }),
            )],
            2,
            1,
        );

        let decision = RouteDecision::single_candidate("primary", "test");
        let messages = vec![ChatMessage::user("hi")];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
        };

        let (_resp, outcome) = provider.chat_with_decision(&decision, request, 0.0).await.unwrap();
        assert_eq!(outcome.status, ExecutionStatus::Success);
        assert!(outcome.fallback_reason.is_none());
        assert_eq!(outcome.final_provider, "primary");
        assert_eq!(outcome.attempts.len(), 1);
    }

    #[tokio::test]
    async fn returns_aggregated_error_when_all_providers_fail() {
        let provider = ReliableProvider::new(
            vec![
                (
                    "p1".into(),
                    Box::new(MockProvider {
                        calls: Arc::new(AtomicUsize::new(0)),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "p1 error",
                    }),
                ),
                (
                    "p2".into(),
                    Box::new(MockProvider {
                        calls: Arc::new(AtomicUsize::new(0)),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "p2 error",
                    }),
                ),
            ],
            0,
            1,
        );

        let err = provider
            .simple_chat("hello", "test", 0.0)
            .await
            .expect_err("all providers should fail");
        let msg = err.to_string();
        assert!(msg.contains("All providers/models failed"));
        assert!(msg.contains("provider=p1 model=test"));
        assert!(msg.contains("provider=p2 model=test"));
        assert!(msg.contains("error=p1 error"));
        assert!(msg.contains("error=p2 error"));
        assert!(msg.contains("retryable"));
    }

    #[test]
    fn non_retryable_detects_common_patterns() {
        assert!(is_non_retryable(&anyhow::anyhow!("400 Bad Request")));
        assert!(is_non_retryable(&anyhow::anyhow!("401 Unauthorized")));
        assert!(is_non_retryable(&anyhow::anyhow!("403 Forbidden")));
        assert!(is_non_retryable(&anyhow::anyhow!("404 Not Found")));
        assert!(is_non_retryable(&anyhow::anyhow!("invalid api key provided")));
        assert!(is_non_retryable(&anyhow::anyhow!("authentication failed")));
        assert!(is_non_retryable(&anyhow::anyhow!("model glm-4.7 not found")));
        assert!(is_non_retryable(&anyhow::anyhow!("unsupported model: glm-4.7")));
        assert!(!is_non_retryable(&anyhow::anyhow!("429 Too Many Requests")));
        assert!(!is_non_retryable(&anyhow::anyhow!("408 Request Timeout")));
        assert!(!is_non_retryable(&anyhow::anyhow!("500 Internal Server Error")));
        assert!(!is_non_retryable(&anyhow::anyhow!("502 Bad Gateway")));
        assert!(!is_non_retryable(&anyhow::anyhow!("timeout")));
        assert!(!is_non_retryable(&anyhow::anyhow!("connection reset")));
        assert!(!is_non_retryable(&anyhow::anyhow!("model overloaded, try again later")));
        assert!(is_non_retryable(&anyhow::anyhow!(
            "OpenAI Codex stream error: Your input exceeds the context window of this model."
        )));
    }

    #[tokio::test]
    async fn context_window_error_aborts_retries_and_model_fallbacks() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut model_fallbacks = std::collections::HashMap::new();
        model_fallbacks.insert("gpt-5.3-codex".to_string(), vec!["gpt-5.2-codex".to_string()]);

        let provider = ReliableProvider::new(
            vec![(
                "openai-codex".into(),
                Box::new(MockProvider {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: usize::MAX,
                    response: "never",
                    error: "OpenAI Codex stream error: Your input exceeds the context window of this model. Please adjust your input and try again.",
                }),
            )],
            4,
            1,
        )
        .with_model_fallbacks(model_fallbacks);

        let err = provider
            .simple_chat("hello", "gpt-5.3-codex", 0.0)
            .await
            .expect_err("context window overflow should fail fast");
        let msg = err.to_string();

        assert!(msg.contains("context window"));
        assert!(msg.contains("skipped"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn aggregated_error_marks_non_retryable_model_mismatch_with_details() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![(
                "custom".into(),
                Box::new(MockProvider {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: usize::MAX,
                    response: "never",
                    error: "unsupported model: glm-4.7",
                }),
            )],
            3,
            1,
        );

        let err = provider
            .simple_chat("hello", "glm-4.7", 0.0)
            .await
            .expect_err("provider should fail");
        let msg = err.to_string();

        assert!(msg.contains("non_retryable"));
        assert!(msg.contains("error=unsupported model: glm-4.7"));
        // Non-retryable errors should not consume retry budget.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn skips_retries_on_non_retryable_error() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));

        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(MockProvider {
                        calls: Arc::clone(&primary_calls),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "401 Unauthorized",
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(MockProvider {
                        calls: Arc::clone(&fallback_calls),
                        fail_until_attempt: 0,
                        response: "from fallback",
                        error: "fallback err",
                    }),
                ),
            ],
            3,
            1,
        );

        let result = provider.simple_chat("hello", "test", 0.0).await.unwrap();
        assert_eq!(result, "from fallback");
        // Primary should have been called only once (no retries)
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn chat_with_history_retries_then_recovers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![(
                "primary".into(),
                Box::new(MockProvider {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: 1,
                    response: "history ok",
                    error: "temporary",
                }),
            )],
            2,
            1,
        );

        let messages = vec![ChatMessage::system("system"), ChatMessage::user("hello")];
        let result = provider.chat_with_history(&messages, "test", 0.0).await.unwrap();
        assert_eq!(result, "history ok");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn chat_with_history_falls_back() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));

        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(MockProvider {
                        calls: Arc::clone(&primary_calls),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "primary down",
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(MockProvider {
                        calls: Arc::clone(&fallback_calls),
                        fail_until_attempt: 0,
                        response: "fallback ok",
                        error: "fallback err",
                    }),
                ),
            ],
            1,
            1,
        );

        let messages = vec![ChatMessage::user("hello")];
        let result = provider.chat_with_history(&messages, "test", 0.0).await.unwrap();
        assert_eq!(result, "fallback ok");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 2);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    // ── New tests: model failover ──

    #[tokio::test]
    async fn model_failover_tries_fallback_model() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mock = Arc::new(ModelAwareMock {
            calls: Arc::clone(&calls),
            models_seen: parking_lot::Mutex::new(Vec::new()),
            fail_models: vec!["claude-opus"],
            response: "ok from sonnet",
        });

        let mut fallbacks = HashMap::new();
        fallbacks.insert("claude-opus".to_string(), vec!["claude-sonnet".to_string()]);

        let provider = ReliableProvider::new(
            vec![("anthropic".into(), Box::new(mock.clone()) as Box<dyn Provider>)],
            0, // no retries — force immediate model failover
            1,
        )
        .with_model_fallbacks(fallbacks);

        let result = provider.simple_chat("hello", "claude-opus", 0.0).await.unwrap();
        assert_eq!(result, "ok from sonnet");

        let seen = mock.models_seen.lock();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], "claude-opus");
        assert_eq!(seen[1], "claude-sonnet");
    }

    #[tokio::test]
    async fn model_failover_all_models_fail() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mock = Arc::new(ModelAwareMock {
            calls: Arc::clone(&calls),
            models_seen: parking_lot::Mutex::new(Vec::new()),
            fail_models: vec!["model-a", "model-b", "model-c"],
            response: "never",
        });

        let mut fallbacks = HashMap::new();
        fallbacks.insert(
            "model-a".to_string(),
            vec!["model-b".to_string(), "model-c".to_string()],
        );

        let provider = ReliableProvider::new(vec![("p1".into(), Box::new(mock.clone()) as Box<dyn Provider>)], 0, 1)
            .with_model_fallbacks(fallbacks);

        let err = provider
            .simple_chat("hello", "model-a", 0.0)
            .await
            .expect_err("all models should fail");
        assert!(err.to_string().contains("All providers/models failed"));

        let seen = mock.models_seen.lock();
        assert_eq!(seen.len(), 3);
    }

    #[tokio::test]
    async fn no_model_fallbacks_behaves_like_before() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![(
                "primary".into(),
                Box::new(MockProvider {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: 0,
                    response: "ok",
                    error: "boom",
                }),
            )],
            2,
            1,
        );
        // No model_fallbacks set — should work exactly as before
        let result = provider.simple_chat("hello", "test", 0.0).await.unwrap();
        assert_eq!(result, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // ── New tests: Retry-After parsing ──

    #[test]
    fn parse_retry_after_integer() {
        let err = anyhow::anyhow!("429 Too Many Requests, Retry-After: 5");
        assert_eq!(parse_retry_after_ms(&err), Some(5000));
    }

    #[test]
    fn parse_retry_after_float() {
        let err = anyhow::anyhow!("Rate limited. retry_after: 2.5 seconds");
        assert_eq!(parse_retry_after_ms(&err), Some(2500));
    }

    #[test]
    fn parse_retry_after_missing() {
        let err = anyhow::anyhow!("500 Internal Server Error");
        assert_eq!(parse_retry_after_ms(&err), None);
    }

    #[test]
    fn rate_limited_detection() {
        assert!(is_rate_limited(&anyhow::anyhow!("429 Too Many Requests")));
        assert!(is_rate_limited(&anyhow::anyhow!("HTTP 429 rate limit exceeded")));
        assert!(!is_rate_limited(&anyhow::anyhow!("401 Unauthorized")));
        assert!(!is_rate_limited(&anyhow::anyhow!("500 Internal Server Error")));
    }

    #[test]
    fn non_retryable_rate_limit_detects_plan_restricted_model() {
        let err = anyhow::anyhow!(
            "{}",
            "API error (429 Too Many Requests): {\"code\":1311,\"message\":\"the current account plan does not include glm-5\"}"
        );
        assert!(
            is_non_retryable_rate_limit(&err),
            "plan-restricted 429 should skip retries"
        );
    }

    #[test]
    fn non_retryable_rate_limit_detects_insufficient_balance() {
        let err = anyhow::anyhow!(
            "{}",
            "API error (429 Too Many Requests): {\"code\":1113,\"message\":\"insufficient balance\"}"
        );
        assert!(
            is_non_retryable_rate_limit(&err),
            "insufficient-balance 429 should skip retries"
        );
    }

    #[test]
    fn non_retryable_rate_limit_does_not_flag_generic_429() {
        let err = anyhow::anyhow!("429 Too Many Requests: rate limit exceeded");
        assert!(
            !is_non_retryable_rate_limit(&err),
            "generic rate-limit 429 should remain retryable"
        );
    }

    #[test]
    fn compute_backoff_uses_retry_after() {
        let provider = ReliableProvider::new(vec![], 0, 500);
        let err = anyhow::anyhow!("429 Retry-After: 3");
        assert_eq!(provider.compute_backoff(500, &err), 3000);
    }

    #[test]
    fn compute_backoff_caps_at_30s() {
        let provider = ReliableProvider::new(vec![], 0, 500);
        let err = anyhow::anyhow!("429 Retry-After: 120");
        assert_eq!(provider.compute_backoff(500, &err), 30_000);
    }

    #[test]
    fn compute_backoff_falls_back_to_base() {
        let provider = ReliableProvider::new(vec![], 0, 500);
        let err = anyhow::anyhow!("500 Server Error");
        assert_eq!(provider.compute_backoff(500, &err), 500);
    }

    // ── §2.1 API auth error (401/403) tests ──────────────────

    #[test]
    fn non_retryable_detects_401() {
        let err = anyhow::anyhow!("API error (401 Unauthorized): invalid api key");
        assert!(is_non_retryable(&err), "401 errors must be detected as non-retryable");
    }

    #[test]
    fn non_retryable_detects_403() {
        let err = anyhow::anyhow!("API error (403 Forbidden): access denied");
        assert!(is_non_retryable(&err), "403 errors must be detected as non-retryable");
    }

    #[test]
    fn non_retryable_detects_404() {
        let err = anyhow::anyhow!("API error (404 Not Found): model not found");
        assert!(is_non_retryable(&err), "404 errors must be detected as non-retryable");
    }

    #[test]
    fn non_retryable_does_not_flag_429() {
        let err = anyhow::anyhow!("429 Too Many Requests");
        assert!(
            !is_non_retryable(&err),
            "429 must NOT be treated as non-retryable (it is retryable with backoff)"
        );
    }

    #[test]
    fn non_retryable_does_not_flag_408() {
        let err = anyhow::anyhow!("408 Request Timeout");
        assert!(
            !is_non_retryable(&err),
            "408 must NOT be treated as non-retryable (it is retryable)"
        );
    }

    #[test]
    fn non_retryable_does_not_flag_500() {
        let err = anyhow::anyhow!("500 Internal Server Error");
        assert!(
            !is_non_retryable(&err),
            "500 must NOT be treated as non-retryable (server errors are retryable)"
        );
    }

    #[test]
    fn non_retryable_does_not_flag_502() {
        let err = anyhow::anyhow!("502 Bad Gateway");
        assert!(!is_non_retryable(&err), "502 must NOT be treated as non-retryable");
    }

    #[test]
    fn parse_error_malformed_json_is_non_retryable() {
        let err = anyhow::anyhow!(
            "OpenAI Codex provider_response_parse_error kind=malformed_json content_type=application/json detail=EOF while parsing"
        );
        assert!(
            is_non_retryable(&err),
            "malformed_json parse errors must be non-retryable"
        );
    }

    #[test]
    fn parse_error_empty_or_unsupported_payload_is_non_retryable() {
        let err = anyhow::anyhow!(
            "OpenAI Codex provider_response_parse_error kind=empty_or_unsupported_payload content_type=text/plain body_len=0"
        );
        assert!(
            is_non_retryable(&err),
            "empty_or_unsupported_payload parse errors must be non-retryable"
        );
    }

    #[test]
    fn parse_error_payload_too_large_is_non_retryable() {
        let err = anyhow::anyhow!(
            "OpenAI Codex provider_response_parse_error kind=payload_too_large content_type=application/json body_len=16777217"
        );
        assert!(
            is_non_retryable(&err),
            "payload_too_large parse errors must be non-retryable"
        );
    }

    #[test]
    fn parse_error_body_read_failed_is_retryable() {
        let err = anyhow::anyhow!(
            "OpenAI Codex provider_response_parse_error kind=body_read_failed content_type=application/json detail=error reading response body"
        );
        assert!(
            !is_non_retryable(&err),
            "body_read_failed parse errors must remain retryable"
        );
    }

    #[test]
    fn parse_error_with_transient_network_read_hint_is_retryable() {
        let err = anyhow::anyhow!(
            "OpenAI Codex provider_response_parse_error kind=unknown content_type=application/json detail=connection reset by peer while reading response body"
        );
        assert!(
            !is_non_retryable(&err),
            "transient network read parse errors must remain retryable"
        );
    }

    // ── §2.2 Rate limit Retry-After edge cases ───────────────

    #[test]
    fn parse_retry_after_zero() {
        let err = anyhow::anyhow!("429 Too Many Requests, Retry-After: 0");
        assert_eq!(
            parse_retry_after_ms(&err),
            Some(0),
            "Retry-After: 0 should parse as 0ms"
        );
    }

    #[test]
    fn parse_retry_after_with_underscore_separator() {
        let err = anyhow::anyhow!("rate limited, retry_after: 10");
        assert_eq!(
            parse_retry_after_ms(&err),
            Some(10_000),
            "retry_after with underscore must be parsed"
        );
    }

    #[test]
    fn parse_retry_after_space_separator() {
        let err = anyhow::anyhow!("Retry-After 7");
        assert_eq!(
            parse_retry_after_ms(&err),
            Some(7000),
            "Retry-After with space separator must be parsed"
        );
    }

    #[test]
    fn rate_limited_false_for_generic_error() {
        let err = anyhow::anyhow!("Connection refused");
        assert!(
            !is_rate_limited(&err),
            "generic errors must not be flagged as rate-limited"
        );
    }

    // ── §2.3 Malformed API response error classification ─────

    #[tokio::test]
    async fn non_retryable_skips_retries_for_401() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![(
                "primary".into(),
                Box::new(MockProvider {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: usize::MAX,
                    response: "never",
                    error: "API error (401 Unauthorized): invalid key",
                }),
            )],
            5,
            1,
        );

        let result = provider.simple_chat("hello", "test", 0.0).await;
        assert!(result.is_err(), "401 should fail without retries");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "must not retry on 401 — should be exactly 1 call"
        );
    }

    #[tokio::test]
    async fn non_retryable_rate_limit_skips_retries_for_plan_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![(
                "primary".into(),
                Box::new(MockProvider {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: usize::MAX,
                    response: "never",
                    error: "API error (429 Too Many Requests): {\"code\":1311,\"message\":\"plan does not include glm-5\"}",
                }),
            )],
            5,
            1,
        );

        let result = provider.simple_chat("hello", "test", 0.0).await;
        assert!(
            result.is_err(),
            "plan-restricted 429 should fail quickly without retrying"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "must not retry non-retryable 429 business errors"
        );
    }

    #[tokio::test]
    async fn rejects_cross_provider_model_mapping_during_fallback() {
        let calls_openai = Arc::new(AtomicUsize::new(0));
        let calls_anthropic = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![
                (
                    "openai".into(),
                    Box::new(MockProvider {
                        calls: Arc::clone(&calls_openai),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "500 openai down",
                    }),
                ),
                (
                    "anthropic".into(),
                    Box::new(MockProvider {
                        calls: Arc::clone(&calls_anthropic),
                        fail_until_attempt: 0,
                        response: "should-not-be-used",
                        error: "",
                    }),
                ),
            ],
            0,
            1,
        );

        let err = provider
            .simple_chat("hello", "openai/gpt-4o", 0.0)
            .await
            .expect_err("cross-provider fallback should be blocked");
        let msg = err.to_string();
        assert!(msg.contains("not compatible with provider"));
        assert_eq!(calls_openai.load(Ordering::SeqCst), 1);
        assert_eq!(calls_anthropic.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn all_failed_error_includes_available_and_unavailable_provider_summary() {
        let provider = ReliableProvider::new(
            vec![(
                "openai".into(),
                Box::new(MockProvider {
                    calls: Arc::new(AtomicUsize::new(0)),
                    fail_until_attempt: usize::MAX,
                    response: "never",
                    error: "p1 error",
                }),
            )],
            0,
            1,
        )
        .with_unavailable_providers(vec![("anthropic".into(), "missing credential/api key".into())]);

        let err = provider
            .simple_chat("hello", "openai/gpt-4o", 0.0)
            .await
            .expect_err("all providers should fail");
        let msg = err.to_string();
        assert!(msg.contains("Available providers"));
        assert!(msg.contains("openai"));
        assert!(msg.contains("anthropic: missing credential/api key"));
    }

    #[test]
    fn supports_native_tools_is_true_if_any_provider_supports_it() {
        let provider = ReliableProvider::new(
            vec![
                ("primary".into(), Box::new(NativeCapabilityMock { native_tools: false })),
                ("fallback".into(), Box::new(NativeCapabilityMock { native_tools: true })),
            ],
            0,
            1,
        );

        assert!(provider.supports_native_tools());
    }

    // ── Arc<ModelAwareMock> Provider impl for test ──

    #[async_trait]
    impl Provider for Arc<ModelAwareMock> {
        async fn chat_with_system(
            &self,
            system_prompt: Option<&str>,
            message: &str,
            model: &str,
            temperature: f64,
        ) -> anyhow::Result<String> {
            self.as_ref()
                .chat_with_system(system_prompt, message, model, temperature)
                .await
        }
    }

    // ── §P0-33 streaming fallback parity tests ──

    use super::super::traits::{StreamChunk, StreamError, StreamOptions, StreamResult};
    use futures_util::stream;

    fn text_chunk(delta: &str, _last: bool) -> StreamChunk {
        // The fallback driver keys off content presence and Ok/Err, not the
        // finality flag, so a plain visible-text delta is sufficient here.
        StreamChunk::delta(delta)
    }

    /// Streaming mock. `outcome` decides what the stream yields:
    /// - `Ok(text)`     → a single content chunk then end.
    /// - `Err(message)` → an immediate pre-content error chunk.
    struct StreamMock {
        calls: Arc<AtomicUsize>,
        outcome: Result<&'static str, &'static str>,
    }

    #[async_trait]
    impl Provider for StreamMock {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("non-stream".to_string())
        }

        fn supports_streaming(&self) -> bool {
            true
        }

        fn stream_chat_with_history(
            &self,
            _messages: &[ChatMessage],
            _model: &str,
            _temperature: f64,
            _options: StreamOptions,
        ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.outcome {
                Ok(text) => {
                    let chunk = text_chunk(text, true);
                    stream::iter(vec![Ok(chunk)]).boxed()
                }
                Err(msg) => stream::iter(vec![Err(StreamError::Provider(msg.to_string()))]).boxed(),
            }
        }
    }

    async fn collect_stream(
        mut s: stream::BoxStream<'static, StreamResult<StreamChunk>>,
    ) -> (String, Option<StreamError>) {
        let mut text = String::new();
        let mut err = None;
        while let Some(item) = s.next().await {
            match item {
                Ok(chunk) => text.push_str(&chunk.delta),
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        (text, err)
    }

    fn enabled_options() -> StreamOptions {
        StreamOptions {
            enabled: true,
            ..StreamOptions::default()
        }
    }

    #[test]
    fn classify_stream_rate_limit() {
        let err = StreamError::Provider("429 Too Many Requests".to_string());
        assert_eq!(classify_stream_error(&err), StreamFailureClass::RateLimited);
    }

    #[test]
    fn classify_stream_context_overflow() {
        let err = StreamError::Provider("Your input exceeds the context window of this model".to_string());
        assert_eq!(classify_stream_error(&err), StreamFailureClass::ContextOverflow);
    }

    #[test]
    fn classify_stream_non_retryable_auth() {
        let err = StreamError::Provider("401 Unauthorized: invalid api key".to_string());
        assert_eq!(classify_stream_error(&err), StreamFailureClass::NonRetryable);
    }

    #[test]
    fn classify_stream_non_retryable_business_rate_limit() {
        let err = StreamError::Provider(
            "429 Too Many Requests: {\"code\":1311,\"message\":\"plan does not include glm-5\"}".to_string(),
        );
        assert_eq!(classify_stream_error(&err), StreamFailureClass::NonRetryable);
    }

    #[test]
    fn classify_stream_retryable_default() {
        let err = StreamError::Provider("500 Internal Server Error".to_string());
        assert_eq!(classify_stream_error(&err), StreamFailureClass::Retryable);
        let io = StreamError::Io(std::io::Error::other("reset"));
        assert_eq!(classify_stream_error(&io), StreamFailureClass::Retryable);
    }

    #[tokio::test]
    async fn streaming_falls_back_to_next_provider_on_pre_content_error() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(StreamMock {
                        calls: Arc::clone(&primary_calls),
                        outcome: Err("500 transient server error"),
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(StreamMock {
                        calls: Arc::clone(&fallback_calls),
                        outcome: Ok("hello from fallback"),
                    }),
                ),
            ],
            2,
            1,
        );

        let messages = vec![ChatMessage::user("hi")];
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, enabled_options());
        let (text, err) = collect_stream(s).await;
        assert!(err.is_none(), "fallback should have succeeded, got {err:?}");
        assert_eq!(text, "hello from fallback");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn streaming_does_not_fall_back_on_non_retryable_error() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(StreamMock {
                        calls: Arc::clone(&primary_calls),
                        outcome: Err("401 Unauthorized: invalid api key"),
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(StreamMock {
                        calls: Arc::clone(&fallback_calls),
                        outcome: Ok("should not be reached"),
                    }),
                ),
            ],
            2,
            1,
        );

        let messages = vec![ChatMessage::user("hi")];
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, enabled_options());
        let (text, err) = collect_stream(s).await;
        assert!(text.is_empty(), "non-retryable error must not yield content");
        assert!(matches!(err, Some(StreamError::Provider(_))), "auth error must surface");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fallback_calls.load(Ordering::SeqCst),
            0,
            "non-retryable error must not trigger provider failover"
        );
    }

    #[tokio::test]
    async fn streaming_surfaces_last_error_when_all_candidates_fail() {
        let provider = ReliableProvider::new(
            vec![
                (
                    "p1".into(),
                    Box::new(StreamMock {
                        calls: Arc::new(AtomicUsize::new(0)),
                        outcome: Err("500 down"),
                    }),
                ),
                (
                    "p2".into(),
                    Box::new(StreamMock {
                        calls: Arc::new(AtomicUsize::new(0)),
                        outcome: Err("503 also down"),
                    }),
                ),
            ],
            0,
            1,
        );

        let messages = vec![ChatMessage::user("hi")];
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, enabled_options());
        let (text, err) = collect_stream(s).await;
        assert!(text.is_empty());
        assert!(err.is_some(), "all-failed streaming must surface an error");
    }

    #[tokio::test]
    async fn streaming_disabled_returns_no_streaming_error() {
        let provider = ReliableProvider::new(
            vec![(
                "p1".into(),
                Box::new(StreamMock {
                    calls: Arc::new(AtomicUsize::new(0)),
                    outcome: Ok("never"),
                }),
            )],
            0,
            1,
        );
        let messages = vec![ChatMessage::user("hi")];
        let opts = StreamOptions {
            enabled: false,
            ..StreamOptions::default()
        };
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, opts);
        let (text, err) = collect_stream(s).await;
        assert!(text.is_empty());
        match err {
            Some(StreamError::Provider(msg)) => assert!(msg.contains("No provider supports streaming")),
            other => panic!("expected no-streaming provider error, got {other:?}"),
        }
    }
}

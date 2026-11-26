use super::Provider;
use super::circuit_breaker::{self, AttemptPermit, BreakerOutcome, BreakerParams, ChainLease, CircuitBreakerRegistry};
use super::rate_limit::{
    self, MAX_HONORED_RETRY_AFTER_MS, RateLimitGate, jitter_after_hint_ms, jitter_backoff_ms, retry_after_hint_ms,
};
use super::traits::{
    ChatMessage, ChatRequest, ChatResponse, ChatTrace, ProviderCapabilities, ProviderRequestMode, StreamChunk,
    StreamOptions, StreamResult,
};
use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, ProviderExecutionOutcome, RouteDecision};
use crate::security::redaction::redact_provider_text;
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

mod retry_policy;
use retry_policy::{RetryKind, may_retry, rate_limit_wait_ms, retry_budget};

mod classify;
use classify::{
    CONTENT_FILTER_STOP_PREFIX, StreamFailureClass, TOOL_CALL_INDEX_LIMIT_PREFIX, classify_stream_error, is_non_retryable,
    is_non_retryable_rate_limit, is_rate_limited, is_tool_schema_refusal, retry_kind, skips_in_place_retry,
    stream_error_is_tool_schema_refusal, stream_retry_kind,
};
#[cfg(test)]
use classify::has_non_retryable_business_code;

/// Wait before each overload retry when the upstream sent no `Retry-After`.
const OVERLOAD_RETRY_WAITS_MS: [u64; 2] = [2_000, 6_000];

/// Longest wait an overload retry honors from a `Retry-After` hint.
const OVERLOAD_MAX_WAIT_MS: u64 = 30_000;

/// The wait before overload retry `retry` (1-based): the upstream's
/// `Retry-After` capped at [`OVERLOAD_MAX_WAIT_MS`], else the fixed schedule.
fn overload_wait_ms(retry: u32, hint_ms: Option<u64>) -> u64 {
    hint_ms.map_or_else(
        || {
            usize::try_from(retry.saturating_sub(1))
                .ok()
                .and_then(|index| OVERLOAD_RETRY_WAITS_MS.get(index).copied())
                .unwrap_or(OVERLOAD_MAX_WAIT_MS)
        },
        |hint| hint.min(OVERLOAD_MAX_WAIT_MS),
    )
}

/// The operator notice for overload retry `retry` (1-based) out of `total`.
fn notice_overload_retry(retry: u32, total: u32) {
    crate::runtime::operator_notice::emit(&format!("Provider overloaded; retrying ({retry}/{total})."));
}

/// Extract a `Retry-After` value (in milliseconds) from a non-streaming error.
///
/// Prefers the **structured** hint attached by
/// [`api_error`](super::api_error), which reads the real HTTP `Retry-After`
/// response header. Before that hint existed this function could only inspect
/// the error's text, and no provider ever put the header into the message — so
/// the whole honor path was unreachable on the non-streaming side and every
/// 429 fell back to a locally guessed exponential backoff. The textual scan is
/// kept as a fallback for upstreams that spell the delay into the body.
fn parse_retry_after_ms(err: &anyhow::Error) -> Option<u64> {
    if let Some(millis) = retry_after_hint_ms(err) {
        return Some(millis);
    }
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
    // A tool-schema refusal carries no upstream body, only tool names,
    // keywords, the model and the (redacted) provider label, and every part of
    // it is needed to act on it: the 200-character clip used to cut the tool
    // list and always cut the configuration to add. It is scrubbed but not
    // clipped.
    // Likewise the declaration hint on an upstream root-combinator
    // rejection: the upstream body is clipped as usual, the hint is kept.
    let detail = if is_tool_schema_refusal(err) {
        crate::security::redaction::redact_secrets(&err.to_string())
    } else if let Some(rejection) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<super::compatible::RootCombinatorRejection>())
    {
        format!(
            "{}{}",
            super::sanitize_api_error(&rejection.upstream),
            crate::security::redaction::redact_secrets(&rejection.hint)
        )
    } else {
        super::sanitize_api_error(&err.to_string())
    };
    detail.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A failover chain that ran out of candidates.
///
/// Its rendering is the all-failed summary every caller already printed. It
/// also carries, typed, the tool-schema refusal, when every attempt the chain
/// made was refused: that is a configuration problem an operator has to fix,
/// not a transient failure, and the surfaces above the provider (channels,
/// the gateway) report it differently without matching text. A chain in
/// which any tried candidate failed for another reason (a 503 before or
/// after the refusal) carries no refusal, so that failure is what is
/// reported (SP-INT2 a52, D-069 L-1); the summary still lists every attempt.
#[derive(Debug, thiserror::Error)]
#[error("{summary}")]
pub struct ProviderChainFailure {
    summary: String,
    tool_schema_refusal: Option<super::ToolSchemaRefusalError>,
}

impl ProviderChainFailure {
    /// The tool-schema refusal that ended this chain: every attempt it made
    /// was refused.
    #[must_use]
    pub const fn tool_schema_refusal(&self) -> Option<&super::ToolSchemaRefusalError> {
        self.tool_schema_refusal.as_ref()
    }
}

/// One member of a failover chain: its display name, its unique key and its
/// provider, kept together so they can never fall out of step.
struct ChainMember {
    /// The label as it may be printed: URL userinfo redacted. Every log line,
    /// record, summary and telemetry entry names the member by this.
    name: Arc<str>,
    /// The key its refusals and rate-limit cool-down are kept under
    /// ([`member_key`]). The display name is not enough: two labels that
    /// differ only in URL userinfo print the same, but are two providers.
    /// In-process only: never printed, recorded or persisted.
    key: Arc<str>,
    /// Canonical provider identity used only for admission comparisons. Its
    /// raw form may contain URL userinfo, so it is never printed.
    id: super::ProviderId,
    provider: Arc<dyn Provider>,
}

/// The gate facts for one fallback provider/model pair.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FallbackEligibility {
    pub(crate) cost_admitted: bool,
    pub(crate) native_tools: bool,
}

/// Admission facts pinned when a resolver-built provider chain is created.
///
/// The primary pair already passed G6/G7. Every other pair must be present in
/// this map before the reliability loop may send it a request.
#[derive(Debug, Clone)]
pub(crate) struct FallbackAdmission {
    primary_provider: super::ProviderId,
    primary_model: String,
    candidates: HashMap<(super::ProviderId, String), FallbackEligibility>,
}

impl FallbackAdmission {
    pub(crate) fn new(primary_provider: super::ProviderId, primary_model: impl Into<String>) -> Self {
        Self {
            primary_provider,
            primary_model: primary_model.into(),
            candidates: HashMap::new(),
        }
    }

    pub(crate) fn insert(
        &mut self,
        provider: super::ProviderId,
        model: impl Into<String>,
        eligibility: FallbackEligibility,
    ) {
        self.candidates.insert((provider, model.into()), eligibility);
    }

    pub(super) fn eligibility(&self, provider: &super::ProviderId, model: &str) -> Option<FallbackEligibility> {
        if provider == &self.primary_provider && model == self.primary_model {
            return Some(FallbackEligibility {
                cost_admitted: true,
                native_tools: true,
            });
        }
        self.candidates.get(&(provider.clone(), model.to_string())).copied()
    }

    fn refusal(
        &self,
        provider: &super::ProviderId,
        display_provider: &str,
        model: &str,
        requires_native_tools: bool,
    ) -> Option<FallbackAdmissionError> {
        let Some(eligibility) = self.eligibility(provider, model) else {
            return Some(FallbackAdmissionError::new(
                display_provider,
                model,
                "pair was not admitted with the resolved chain",
            ));
        };
        if !eligibility.cost_admitted {
            return Some(FallbackAdmissionError::new(
                display_provider,
                model,
                "no valid price is configured",
            ));
        }
        if requires_native_tools && !eligibility.native_tools {
            return Some(FallbackAdmissionError::new(
                display_provider,
                model,
                "NativeTools is not verified",
            ));
        }
        None
    }
}

/// A fallback rejected locally before any transport starts.
#[derive(Debug, Clone, thiserror::Error)]
#[error("fallback provider '{provider}' model '{model}' refused before transport: {reason}")]
pub(crate) struct FallbackAdmissionError {
    provider: String,
    model: String,
    reason: &'static str,
}

impl FallbackAdmissionError {
    fn new(provider: &str, model: &str, reason: &'static str) -> Self {
        Self {
            provider: provider.to_string(),
            model: model.to_string(),
            reason,
        }
    }
}

/// The key a chain member's refusals and rate-limit cool-down are kept under.
///
/// A label without URL userinfo is its own display name, and that is its key,
/// as before. A label with userinfo is displayed redacted, and two labels that
/// differ only in their userinfo (two accounts on one host) redact to the same
/// display name while being two providers (SP-INT2 a50). Such a label is keyed
/// by its display name plus a fingerprint of the full label: a hash salted
/// per process, so it is stable across the chains of one process (the
/// rate-limit gate is shared) and says nothing about the credential. The
/// `reliability.api_keys` rotation instances carry the same label, so they
/// share their primary's key, which is what D-020 Q2 wants.
fn member_key(label: &str, display: &Arc<str>) -> Arc<str> {
    use std::hash::BuildHasher as _;
    static SALT: std::sync::LazyLock<std::collections::hash_map::RandomState> =
        std::sync::LazyLock::new(std::collections::hash_map::RandomState::new);
    if label == display.as_ref() {
        return Arc::clone(display);
    }
    Arc::from(format!("{display}#{:016x}", SALT.hash_one(label)))
}

/// The candidates a tool-schema refusal already ruled out, by member key
/// ([`member_key`]) and model.
///
/// The refusal is decided before transport from the provider's declaration for
/// the model, so every chain entry with the same label and model — the
/// `reliability.api_keys` rotation instances are such entries — would refuse
/// the same request again (D-020 Q2). A different provider (including one
/// that differs only in its URL userinfo), or a different model of the same
/// provider, may accept it and is still tried.
#[derive(Debug, Default)]
struct RefusedCandidates(Vec<(Arc<str>, String)>);

impl RefusedCandidates {
    fn contains(&self, member_key: &str, model: &str) -> bool {
        self.0
            .iter()
            .any(|(refused, refused_model)| refused.as_ref() == member_key && refused_model == model)
    }

    fn insert(&mut self, member_key: &Arc<str>, model: &str) {
        if !self.contains(member_key, model) {
            self.0.push((Arc::clone(member_key), model.to_string()));
        }
    }
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
/// `seq` is a 1-based `u32` (FIX #4): `provider_retries` is an uncapped `u32`
/// in config, so the bounded failover space (model_chain × providers × retries)
/// can in principle exceed 255. A `u32` keeps every attempt index orderable and
/// the sequence complete, satisfying the P0-31 contract; `saturating_add` at the
/// call site is then effectively non-saturating in any realistic configuration.
fn build_attempt(
    seq: u32,
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
    providers: Vec<ChainMember>,
    max_retries: u32,
    base_backoff_ms: u64,
    /// Per-model fallback chains: model_name → [fallback_model_1, fallback_model_2, ...]
    model_fallbacks: HashMap<String, Vec<String>>,
    /// Per-fallback G6/G7 facts for resolver-built chains. Direct unit-test
    /// construction leaves this absent.
    fallback_admission: Option<FallbackAdmission>,
    /// Providers filtered out at startup with reasons (invalid/missing credential/init failure).
    unavailable_providers: Vec<(String, String)>,
    /// Per-provider cool-down learned from upstream 429/503 responses.
    ///
    /// Chains built by [`create_provider_for`](super::create_provider_for)
    /// share the process-wide gate, so a 429 on one request defers every other
    /// in-flight request to the same provider. Chains constructed directly get
    /// a private gate, keeping unit tests isolated from one another.
    rate_limit_gate: Arc<RateLimitGate>,
    circuit_breaker: Option<(Arc<CircuitBreakerRegistry>, BreakerParams)>,
    circuit_lease: Option<Arc<ChainLease>>,
}

impl ReliableProvider {
    /// Build a chain over `providers`, tried in order.
    ///
    /// Each name is the label the provider was configured under, and is
    /// stored with any URL userinfo redacted (`custom:http://[redacted]@host`).
    /// The chain only ever prints, logs, records or keys its cool-down by that
    /// name; the real URL, credential included, stays inside the provider's
    /// transport. Redacting here, once, is what keeps every failed-attempt
    /// line, the all-failed summary, every `tracing` field, the warmup log and
    /// every `ProviderAttempt` free of it.
    pub fn new(providers: Vec<(String, Box<dyn Provider>)>, max_retries: u32, base_backoff_ms: u64) -> Self {
        let providers = providers
            .into_iter()
            .map(|(label, provider)| {
                let name: Arc<str> = Arc::from(super::redact_endpoint_userinfo(&label));
                let key = member_key(&label, &name);
                ChainMember {
                    name,
                    key,
                    id: super::ProviderId::of(&label),
                    provider: Arc::from(provider),
                }
            })
            .collect();
        Self {
            providers,
            max_retries,
            base_backoff_ms: base_backoff_ms.max(50),
            model_fallbacks: HashMap::new(),
            fallback_admission: None,
            unavailable_providers: Vec::new(),
            rate_limit_gate: Arc::new(RateLimitGate::new()),
            circuit_breaker: None,
            circuit_lease: None,
        }
    }

    /// Share a rate-limit gate with other provider chains in this process.
    #[must_use]
    pub fn with_rate_limit_gate(mut self, gate: Arc<RateLimitGate>) -> Self {
        self.rate_limit_gate = gate;
        self
    }

    /// Attach process-local provider health protection and lease this chain's keys.
    #[must_use]
    pub fn with_circuit_breaker(mut self, registry: Arc<CircuitBreakerRegistry>, params: BreakerParams) -> Self {
        self.circuit_lease = Some(Arc::new(
            registry.register(
                self.providers
                    .iter()
                    .map(|member| (Arc::clone(&member.key), Arc::clone(&member.name))),
            ),
        ));
        self.circuit_breaker = Some((registry, params));
        self
    }

    fn admit_circuit(&self, key: &Arc<str>) -> Result<Option<AttemptPermit>, u64> {
        match &self.circuit_breaker {
            Some((registry, params)) => registry.admit(key, *params),
            None => Ok(None),
        }
    }

    /// Set per-model fallback chains.
    pub fn with_model_fallbacks(mut self, fallbacks: HashMap<String, Vec<String>>) -> Self {
        self.model_fallbacks = fallbacks;
        self
    }

    /// Attach the G6/G7 facts pinned by the resolver-built factory.
    #[must_use]
    pub(crate) fn with_fallback_admission(mut self, admission: FallbackAdmission) -> Self {
        self.fallback_admission = Some(admission);
        self
    }

    /// Attach provider availability failures captured during startup.
    ///
    /// Both the label and the reason are printed in the all-failed summary,
    /// and a reason may quote the label, so both are redacted like the chain's
    /// own names.
    pub fn with_unavailable_providers(mut self, unavailable: Vec<(String, String)>) -> Self {
        self.unavailable_providers = unavailable
            .into_iter()
            .map(|(name, reason)| {
                (
                    super::redact_endpoint_userinfo(&name).into_owned(),
                    super::redact_endpoint_userinfo(&reason).into_owned(),
                )
            })
            .collect();
        self
    }

    /// Whether chain member `provider_name` may serve `model`.
    ///
    /// The route belongs to the chain's primary member. A self-hosted model
    /// server (llama.cpp) serves whatever repository it is configured for:
    /// on its own route any model (D-075); as a fallback member only a
    /// `model_fallbacks` entry that explicitly names a model repository, never
    /// the route's requested model and never a bare id or a `<provider>/…`
    /// model routed to another provider (D-076 N-1). Every other member keeps
    /// the model-prefix check; the primary serving the requested model uses
    /// the configured-model rule.
    fn provider_model_compatible(&self, provider_name: &str, model: &str, requested: &str) -> bool {
        let primary = self
            .providers
            .first()
            .is_some_and(|member| member.name.as_ref() == provider_name);
        if super::is_self_hosted_model_server(provider_name) {
            return if primary {
                super::provider_serves_configured_model(provider_name, model)
            } else {
                model != requested && super::names_model_repository(model)
            };
        }
        if primary && model == requested {
            super::provider_serves_configured_model(provider_name, model)
        } else {
            super::provider_matches_model_prefix(provider_name, model)
        }
    }

    fn all_failed_message(&self, failures: &[String], runtime_unavailable: &[(String, String)]) -> String {
        // Redact labels before adding punctuation and diagnostic reasons: a
        // credential-shaped query value followed by ':' can otherwise consume
        // the reason as another credential pair in the assembled message.
        let display = |text: &str| {
            if self.circuit_breaker.is_some() {
                crate::security::redaction::redact_provider_text(text)
            } else {
                text.to_owned()
            }
        };
        let available = self
            .providers
            .iter()
            .map(|member| display(&member.name))
            .collect::<Vec<_>>()
            .join(", ");

        let mut unavailable = self.unavailable_providers.clone();
        unavailable.extend(runtime_unavailable.iter().cloned());
        let unavailable_text = if unavailable.is_empty() {
            "none".to_string()
        } else {
            unavailable
                .into_iter()
                .map(|(name, reason)| format!("{}: {}", display(&name), display(&reason)))
                .collect::<Vec<_>>()
                .join("; ")
        };

        let message = format!(
            "All providers/models failed for this model request (earlier requests in the same turn are unaffected). Available providers: [{}]. Unavailable providers: {}. Attempts:\n{}",
            available,
            unavailable_text,
            failures.join("\n")
        );
        if self.circuit_breaker.is_some() {
            crate::security::redaction::redact_provider_text(&message)
        } else {
            message
        }
    }

    /// Build the list of models to try: [original, fallback1, fallback2, ...]
    fn model_chain<'a>(&'a self, model: &'a str) -> Vec<&'a str> {
        let mut chain = vec![model];
        if let Some(fallbacks) = self.model_fallbacks.get(model) {
            chain.extend(fallbacks.iter().map(|s| s.as_str()));
        }
        chain
    }

    fn fallback_refusal(
        &self,
        provider: &super::ProviderId,
        display_provider: &str,
        model: &str,
        requires_native_tools: bool,
    ) -> Option<FallbackAdmissionError> {
        self.fallback_admission
            .as_ref()
            .and_then(|admission| admission.refusal(provider, display_provider, model, requires_native_tools))
    }

    /// The most in-place retries one candidate can take on the rate-limit
    /// budget; `is_last` tells whether it is the last candidate of the chain.
    fn max_candidate_retries(&self, is_last: bool) -> u32 {
        retry_budget(self.max_retries, RetryKind::RateLimited, is_last)
    }

    /// Emit rate-limit telemetry, publish the cool-down to the shared gate and
    /// sleep before the next attempt on the same candidate.
    ///
    /// Returns `false` when the upstream asked for longer than
    /// [`MAX_HONORED_RETRY_AFTER_MS`], meaning this candidate must be abandoned
    /// rather than retried after a truncated (and therefore useless) wait.
    async fn wait_before_retry(&self, ctx: &RetryContext<'_>, backoff_ms: u64, err: &anyhow::Error) -> bool {
        let hint_ms = parse_retry_after_ms(err);
        if ctx.rate_limited {
            rate_limit::record_rate_limited(ctx.provider_name, ctx.model, false, hint_ms.is_some());
        }
        if ctx.kind == RetryKind::Overload {
            let wait = overload_wait_ms(ctx.attempt, hint_ms);
            tracing::warn!(
                provider = %redact_provider_text(ctx.provider_name),
                model = ctx.model,
                attempt = ctx.attempt,
                wait_ms = wait,
                retry_after_ms = hint_ms,
                reason = ctx.failure_reason,
                "Provider overloaded, retrying"
            );
            notice_overload_retry(
                ctx.attempt,
                retry_budget(self.max_retries, RetryKind::Overload, ctx.is_last),
            );
            self.rate_limit_gate.note_rate_limited(ctx.gate_key, wait);
            tokio::time::sleep(Duration::from_millis(wait)).await;
            return true;
        }
        // A 429 without `Retry-After` follows the rate-limit schedule; with a
        // hint the hint decides, up to the honored ceiling.
        let plan = match (ctx.kind, hint_ms) {
            (RetryKind::RateLimited, None) if ctx.is_last => RetryWait::Sleep(rate_limit_wait_ms(ctx.attempt)),
            (RetryKind::RateLimited, None) => RetryWait::Sleep(overload_wait_ms(ctx.attempt, None)),
            _ => plan_retry_wait(backoff_ms, err),
        };
        match plan {
            RetryWait::HintExceedsCap(requested_ms) => {
                tracing::warn!(
                    provider = %redact_provider_text(ctx.provider_name),
                    model = ctx.model,
                    requested_retry_after_ms = requested_ms,
                    max_honored_ms = MAX_HONORED_RETRY_AFTER_MS,
                    reason = ctx.failure_reason,
                    "Upstream Retry-After exceeds the honored ceiling; abandoning this candidate instead of waiting a truncated interval"
                );
                // Still publish the real cool-down: other in-flight requests
                // must not keep hammering a provider that just asked for a very
                // long pause, even though this request gives up on it.
                self.rate_limit_gate.note_rate_limited(ctx.gate_key, requested_ms);
                false
            }
            RetryWait::Sleep(wait) => {
                tracing::warn!(
                    provider = %redact_provider_text(ctx.provider_name),
                    model = ctx.model,
                    attempt = ctx.attempt,
                    backoff_ms = wait,
                    retry_after_ms = hint_ms,
                    reason = ctx.failure_reason,
                    error = ctx.error_detail,
                    "Provider call failed, retrying"
                );
                if ctx.kind == RetryKind::RateLimited {
                    notice_overload_retry(
                        ctx.attempt,
                        retry_budget(self.max_retries, RetryKind::RateLimited, ctx.is_last),
                    );
                }
                if ctx.rate_limited {
                    self.rate_limit_gate.note_rate_limited(ctx.gate_key, wait);
                }
                tokio::time::sleep(Duration::from_millis(wait)).await;
                true
            }
        }
    }

    /// Park until the shared cool-down of the member keyed `gate_key` (named
    /// `provider_name` in logs) has elapsed.
    async fn await_rate_limit_gate(&self, provider_name: &str, gate_key: &str, model: &str) {
        if let Some(deferred_ms) = self.rate_limit_gate.wait_until_clear(gate_key, provider_name).await {
            tracing::warn!(
                provider = %redact_provider_text(provider_name),
                model,
                deferred_ms,
                "Deferred behind the shared provider rate-limit cool-down"
            );
        }
    }
}

/// How long to wait before the next attempt on the same candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryWait {
    /// Sleep this many milliseconds (jitter already applied), then retry.
    Sleep(u64),
    /// The upstream asked for a longer pause than we honor in place; the
    /// payload is the delay it actually requested, for diagnostics.
    HintExceedsCap(u64),
}

/// Shared inputs for one retry decision, kept in a struct so the four
/// non-streaming failover loops call an identical code path.
struct RetryContext<'a> {
    provider_name: &'a str,
    /// The member's rate-limit gate key ([`member_key`]).
    gate_key: &'a str,
    model: &'a str,
    /// 1-based attempt number that just failed.
    attempt: u32,
    failure_reason: &'static str,
    error_detail: &'a str,
    rate_limited: bool,
    /// How the failure is retried; see [`RetryKind`].
    kind: RetryKind,
    /// Whether this candidate is the last one of the chain.
    is_last: bool,
}

/// Compute the (jittered) wait before retrying the same provider/model.
///
/// * With a `Retry-After` hint: honor it, floored at the local backoff so a
///   `0` hint still pauses, and add a small positive spread. The wait is never
///   shortened below what the server asked for — truncating a server-declared
///   cool-down guarantees an immediate second 429.
/// * Without a hint: equal-jitter the local exponential backoff so concurrent
///   callers throttled in the same instant stop retrying in lock-step.
fn plan_retry_wait(base: u64, err: &anyhow::Error) -> RetryWait {
    plan_wait_from_hint(base, parse_retry_after_ms(err))
}

/// [`plan_retry_wait`] for callers that already extracted the hint (the
/// streaming driver reads it off the structured `StreamError` variant).
fn plan_wait_from_hint(base: u64, hint_ms: Option<u64>) -> RetryWait {
    match hint_ms {
        Some(hint) if hint > MAX_HONORED_RETRY_AFTER_MS => RetryWait::HintExceedsCap(hint),
        Some(hint) => RetryWait::Sleep(jitter_after_hint_ms(hint.max(base))),
        None => RetryWait::Sleep(jitter_backoff_ms(base)),
    }
}

#[async_trait]
impl Provider for ReliableProvider {
    fn accounting_targets(&self, model: &str) -> Option<super::AccountingTargets> {
        let mut targets = super::AccountingTargets::empty();
        for current_model in self.model_chain(model) {
            for member in &self.providers {
                if self.provider_model_compatible(&member.name, current_model, model)
                    && self
                        .fallback_refusal(&member.id, &member.name, current_model, false)
                        .is_none()
                    && let Some(descriptor) = member.provider.accounting_targets(current_model)
                {
                    targets.append_member(descriptor, &member.name);
                }
            }
        }
        targets.nonempty()
    }

    fn supports_attempt_accounting(&self) -> bool {
        true
    }

    fn capabilities_for(&self, model: &str, mode: ProviderRequestMode) -> ProviderCapabilities {
        if self.fallback_admission.is_some() {
            let mut viable = Vec::new();
            let mut native_viable = Vec::new();
            for current_model in self.model_chain(model) {
                for member in &self.providers {
                    if !self.provider_model_compatible(&member.name, current_model, model)
                        || (mode == ProviderRequestMode::Streaming && !member.provider.supports_streaming())
                        || self
                            .fallback_refusal(&member.id, &member.name, current_model, false)
                            .is_some()
                    {
                        continue;
                    }
                    let capabilities = member.provider.capabilities_for(current_model, mode);
                    viable.push(capabilities.clone());
                    if self
                        .fallback_refusal(&member.id, &member.name, current_model, true)
                        .is_none()
                    {
                        native_viable.push(capabilities);
                    }
                }
            }
            let vision = viable
                .first()
                .is_some_and(|first| first.vision && viable.iter().all(|next| next.vision));
            let native_tool_calling = native_viable.first().is_some_and(|first| {
                first.native_tool_calling && native_viable.iter().all(|next| next.native_tool_calling)
            });
            return ProviderCapabilities {
                native_tool_calling,
                vision,
            };
        }
        let mut matched = self
            .providers
            .iter()
            .filter(|member| {
                self.provider_model_compatible(&member.name, model, model)
                    && (mode == ProviderRequestMode::NonStreaming || member.provider.supports_streaming())
            })
            .map(|member| member.provider.capabilities_for(model, mode));
        let Some(first) = matched.next() else {
            return ProviderCapabilities::default();
        };
        // A failover chain may land on any viable provider. Advertise only the
        // intersection so a later fallback cannot silently lose a requirement.
        matched.fold(first, |current, next| ProviderCapabilities {
            native_tool_calling: current.native_tool_calling && next.native_tool_calling,
            vision: current.vision && next.vision,
        })
    }

    /// Forward the dialect of the providers in the chain.
    ///
    /// Without this override the chain inherits the trait default `Raw`, and a
    /// Gemini turn behind a failover chain is validated against the twenty
    /// keywords `clean_for_gemini` removed on the way out — rejecting calls the
    /// model was never shown the constraints for.
    ///
    /// A chain whose members all declare one dialect resolves to it exactly.
    /// A mixed chain resolves to `Raw` (see
    /// [`unanimous_dialect`](super::traits::unanimous_dialect)).
    fn tool_schema_dialect(&self) -> crate::tools::ToolSchemaDialect {
        super::traits::unanimous_dialect(
            self.providers
                .iter()
                .map(|member| member.provider.tool_schema_dialect()),
        )
    }

    /// The dialect of every (member, model) pair that may serve `model`:
    /// each member for `model` and for each of its model fallbacks. Exact
    /// when they agree, `Raw` when they do not.
    fn tool_schema_dialect_for(&self, model: &str) -> crate::tools::ToolSchemaDialect {
        let models = self.model_chain(model);
        if self.fallback_admission.is_some() {
            return super::traits::unanimous_dialect(self.providers.iter().flat_map(|member| {
                models
                    .iter()
                    .filter(move |candidate| {
                        self.fallback_refusal(&member.id, &member.name, candidate, true)
                            .is_none()
                    })
                    .map(move |candidate| member.provider.tool_schema_dialect_for(candidate))
            }));
        }
        super::traits::unanimous_dialect(self.providers.iter().flat_map(|member| {
            models
                .iter()
                .map(move |candidate| member.provider.tool_schema_dialect_for(candidate))
        }))
    }

    /// The member that served: its own dialect for `model`.
    ///
    /// `member` is matched against the unique member keys first, so two
    /// members whose labels differ only in their password answer for
    /// themselves (D-068 (2)); a display name is matched as before, for a
    /// trace that carries no member key.
    fn tool_schema_dialect_of_member(&self, member: &str, model: &str) -> Option<crate::tools::ToolSchemaDialect> {
        self.providers
            .iter()
            .find(|candidate| &*candidate.key == member)
            .or_else(|| self.providers.iter().find(|candidate| &*candidate.name == member))
            .map(|candidate| candidate.provider.tool_schema_dialect_for(model))
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        for ChainMember { name, provider, .. } in &self.providers {
            tracing::info!(provider = %redact_provider_text(name), "Warming up provider connection pool");
            if provider.warmup().await.is_err() {
                tracing::warn!(provider = %redact_provider_text(name), "Warmup failed (non-fatal)");
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
        for (model_index, current_model) in models.iter().enumerate() {
            for (
                provider_index,
                ChainMember {
                    name: provider_name,
                    key: member_key,
                    id: provider_id,
                    provider,
                },
            ) in self.providers.iter().enumerate()
            {
                if !self.provider_model_compatible(provider_name, current_model, model) {
                    runtime_unavailable.push((
                        provider_name.to_string(),
                        format!("model '{current_model}' not compatible with provider"),
                    ));
                    continue;
                }
                if let Some(refusal) = self.fallback_refusal(provider_id, provider_name, current_model, false) {
                    runtime_unavailable.push((provider_name.to_string(), refusal.to_string()));
                    continue;
                }

                let mut backoff_ms = self.base_backoff_ms;
                // Tracks whether this candidate was ever throttled, so recovery
                // and give-up can be reported separately from ordinary errors.
                let mut rate_limited_seen = false;

                let is_last = model_index + 1 == models.len() && provider_index + 1 == self.providers.len();
                for attempt in 0..=self.max_candidate_retries(is_last) {
                    let permit = match self.admit_circuit(member_key) {
                        Ok(permit) => permit,
                        Err(ms) => {
                            runtime_unavailable.push((provider_name.to_string(), circuit_reason(ms)));
                            break;
                        }
                    };
                    self.await_rate_limit_gate(provider_name, member_key, current_model)
                        .await;
                    if permit.as_ref().is_some_and(|permit| !permit.revalidate()) {
                        runtime_unavailable.push((provider_name.to_string(), circuit_reason(0)));
                        break;
                    }
                    match provider
                        .chat_with_system(system_prompt, message, current_model, temperature)
                        .await
                    {
                        Ok(resp) => {
                            complete_circuit(permit, BreakerOutcome::Answered);
                            if attempt > 0 || *current_model != model {
                                tracing::info!(
                                    provider = %redact_provider_text(provider_name),
                                    model = *current_model,
                                    attempt,
                                    original_model = model,
                                    "Provider recovered (failover/retry)"
                                );
                            }
                            if rate_limited_seen {
                                rate_limit::record_recovered(provider_name);
                            }
                            return Ok(resp);
                        }
                        Err(e) => {
                            complete_circuit(permit, buffered_breaker_outcome(&e));
                            let non_retryable_rate_limit = is_non_retryable_rate_limit(&e);
                            let non_retryable = is_non_retryable(&e) || non_retryable_rate_limit;
                            let rate_limited = is_rate_limited(&e);
                            let failure_reason = failure_reason(rate_limited, non_retryable);
                            let error_detail = compact_error_detail(&e);
                            rate_limited_seen = rate_limited_seen || (rate_limited && !non_retryable);

                            let kind = retry_kind(&e);
                            push_failure(
                                &mut failures,
                                provider_name,
                                current_model,
                                attempt + 1,
                                retry_budget(self.max_retries, kind, is_last) + 1,
                                failure_reason,
                                &error_detail,
                            );

                            if non_retryable {
                                tracing::warn!(
                                    provider = %redact_provider_text(provider_name),
                                    model = *current_model,
                                    error = %error_detail,
                                    "Non-retryable error, moving on"
                                );

                                if crate::recovery::overflow::is_context_overflow_error(&e) {
                                    anyhow::bail!(
                                        "Request exceeds model context window; retries and fallbacks were skipped. Attempts:\n{}",
                                        failures.join("\n")
                                    );
                                }

                                break;
                            }

                            if may_retry(attempt, self.max_retries, kind, is_last) {
                                let ctx = RetryContext {
                                    provider_name,
                                    gate_key: member_key,
                                    model: current_model,
                                    attempt: attempt + 1,
                                    failure_reason,
                                    error_detail: &error_detail,
                                    rate_limited,
                                    kind,
                                    is_last,
                                };
                                if !self.wait_before_retry(&ctx, backoff_ms, &e).await {
                                    break;
                                }
                                backoff_ms = (backoff_ms.saturating_mul(2)).min(10_000);
                            } else if rate_limited {
                                // Final attempt: no sleep follows, but the
                                // throttle still has to reach the counters — and
                                // the cool-down still has to reach every other
                                // in-flight request, even though this one gives up.
                                let hint = parse_retry_after_ms(&e);
                                rate_limit::record_rate_limited(provider_name, current_model, false, hint.is_some());
                                self.rate_limit_gate.note_rate_limited(
                                    member_key,
                                    hint.unwrap_or(backoff_ms).min(MAX_HONORED_RETRY_AFTER_MS),
                                );
                                break;
                            } else {
                                break;
                            }
                        }
                    }
                }

                if rate_limited_seen {
                    rate_limit::record_exhausted(provider_name);
                    tracing::error!(
                        provider = %redact_provider_text(provider_name),
                        model = *current_model,
                        attempts = self.max_candidate_retries(is_last) + 1,
                        "Provider rate limit outlasted the retry budget; abandoning this candidate"
                    );
                }
                tracing::warn!(
                    provider = %redact_provider_text(provider_name),
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

        for (model_index, current_model) in models.iter().enumerate() {
            for (
                provider_index,
                ChainMember {
                    name: provider_name,
                    key: member_key,
                    id: provider_id,
                    provider,
                },
            ) in self.providers.iter().enumerate()
            {
                if !self.provider_model_compatible(provider_name, current_model, model) {
                    runtime_unavailable.push((
                        provider_name.to_string(),
                        format!("model '{current_model}' not compatible with provider"),
                    ));
                    continue;
                }
                if let Some(refusal) = self.fallback_refusal(provider_id, provider_name, current_model, false) {
                    runtime_unavailable.push((provider_name.to_string(), refusal.to_string()));
                    continue;
                }

                let mut backoff_ms = self.base_backoff_ms;
                // Tracks whether this candidate was ever throttled, so recovery
                // and give-up can be reported separately from ordinary errors.
                let mut rate_limited_seen = false;

                let is_last = model_index + 1 == models.len() && provider_index + 1 == self.providers.len();
                for attempt in 0..=self.max_candidate_retries(is_last) {
                    let permit = match self.admit_circuit(member_key) {
                        Ok(permit) => permit,
                        Err(ms) => {
                            runtime_unavailable.push((provider_name.to_string(), circuit_reason(ms)));
                            break;
                        }
                    };
                    self.await_rate_limit_gate(provider_name, member_key, current_model)
                        .await;
                    if permit.as_ref().is_some_and(|permit| !permit.revalidate()) {
                        runtime_unavailable.push((provider_name.to_string(), circuit_reason(0)));
                        break;
                    }
                    match provider.chat_with_history(messages, current_model, temperature).await {
                        Ok(resp) => {
                            complete_circuit(permit, BreakerOutcome::Answered);
                            if attempt > 0 || *current_model != model {
                                tracing::info!(
                                    provider = %redact_provider_text(provider_name),
                                    model = *current_model,
                                    attempt,
                                    original_model = model,
                                    "Provider recovered (failover/retry)"
                                );
                            }
                            if rate_limited_seen {
                                rate_limit::record_recovered(provider_name);
                            }
                            return Ok(resp);
                        }
                        Err(e) => {
                            complete_circuit(permit, buffered_breaker_outcome(&e));
                            let non_retryable_rate_limit = is_non_retryable_rate_limit(&e);
                            let non_retryable = is_non_retryable(&e) || non_retryable_rate_limit;
                            let rate_limited = is_rate_limited(&e);
                            let failure_reason = failure_reason(rate_limited, non_retryable);
                            let error_detail = compact_error_detail(&e);
                            rate_limited_seen = rate_limited_seen || (rate_limited && !non_retryable);

                            let kind = retry_kind(&e);
                            push_failure(
                                &mut failures,
                                provider_name,
                                current_model,
                                attempt + 1,
                                retry_budget(self.max_retries, kind, is_last) + 1,
                                failure_reason,
                                &error_detail,
                            );

                            if non_retryable {
                                tracing::warn!(
                                    provider = %redact_provider_text(provider_name),
                                    model = *current_model,
                                    error = %error_detail,
                                    "Non-retryable error, moving on"
                                );

                                if crate::recovery::overflow::is_context_overflow_error(&e) {
                                    anyhow::bail!(
                                        "Request exceeds model context window; retries and fallbacks were skipped. Attempts:\n{}",
                                        failures.join("\n")
                                    );
                                }

                                break;
                            }

                            if may_retry(attempt, self.max_retries, kind, is_last) {
                                let ctx = RetryContext {
                                    provider_name,
                                    gate_key: member_key,
                                    model: current_model,
                                    attempt: attempt + 1,
                                    failure_reason,
                                    error_detail: &error_detail,
                                    rate_limited,
                                    kind,
                                    is_last,
                                };
                                if !self.wait_before_retry(&ctx, backoff_ms, &e).await {
                                    break;
                                }
                                backoff_ms = (backoff_ms.saturating_mul(2)).min(10_000);
                            } else if rate_limited {
                                // Final attempt: no sleep follows, but the
                                // throttle still has to reach the counters — and
                                // the cool-down still has to reach every other
                                // in-flight request, even though this one gives up.
                                let hint = parse_retry_after_ms(&e);
                                rate_limit::record_rate_limited(provider_name, current_model, false, hint.is_some());
                                self.rate_limit_gate.note_rate_limited(
                                    member_key,
                                    hint.unwrap_or(backoff_ms).min(MAX_HONORED_RETRY_AFTER_MS),
                                );
                                break;
                            } else {
                                break;
                            }
                        }
                    }
                }

                if rate_limited_seen {
                    rate_limit::record_exhausted(provider_name);
                    tracing::error!(
                        provider = %redact_provider_text(provider_name),
                        model = *current_model,
                        attempts = self.max_candidate_retries(is_last) + 1,
                        "Provider rate limit outlasted the retry budget; abandoning this candidate"
                    );
                }
                tracing::warn!(
                    provider = %redact_provider_text(provider_name),
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
        // `seq` counts every recorded attempt (1-based after the first
        // increment). FIX #4: `u32` so the sequence stays complete and orderable
        // even when an uncapped `provider_retries` pushes the bounded failover
        // space past 255; `saturating_add` is then effectively non-saturating.
        let mut seq: u32 = 0;
        let mut refused = RefusedCandidates::default();
        // The chain is reported as refused only when every attempt it made
        // was refused (SP-INT2 a52, D-069 L-1): if any tried candidate failed
        // for another reason (a 503, say), that failure class is what the
        // caller reports, since sending again may then succeed.
        let mut chain_refusal: Option<super::ToolSchemaRefusalError> = None;
        let mut every_failure_refused = true;

        for (model_index, current_model) in models.iter().enumerate() {
            for (
                provider_index,
                ChainMember {
                    name: provider_name,
                    key: member_key,
                    id: provider_id,
                    provider,
                },
            ) in self.providers.iter().enumerate()
            {
                if !self.provider_model_compatible(provider_name, current_model, model) {
                    runtime_unavailable.push((
                        provider_name.to_string(),
                        format!("model '{current_model}' not compatible with provider"),
                    ));
                    continue;
                }
                if let Some(refusal) = self.fallback_refusal(
                    provider_id,
                    provider_name,
                    current_model,
                    request.tools.is_some_and(|tools| !tools.is_empty()),
                ) {
                    runtime_unavailable.push((provider_name.to_string(), refusal.to_string()));
                    continue;
                }
                if refused.contains(member_key, current_model) {
                    continue;
                }

                let mut backoff_ms = self.base_backoff_ms;
                // Tracks whether this candidate was ever throttled, so recovery
                // and give-up can be reported separately from ordinary errors.
                let mut rate_limited_seen = false;

                let is_last = model_index + 1 == models.len() && provider_index + 1 == self.providers.len();
                for attempt in 0..=self.max_candidate_retries(is_last) {
                    super::attempt_accounting::ensure_supported(provider.as_ref())?;
                    let permit = match self.admit_circuit(member_key) {
                        Ok(permit) => permit,
                        Err(ms) => {
                            runtime_unavailable.push((provider_name.to_string(), circuit_reason(ms)));
                            break;
                        }
                    };
                    self.await_rate_limit_gate(provider_name, member_key, current_model)
                        .await;
                    if permit.as_ref().is_some_and(|permit| !permit.revalidate()) {
                        runtime_unavailable.push((provider_name.to_string(), circuit_reason(0)));
                        break;
                    }
                    let attempt_started_at = chrono::Utc::now();
                    match provider.chat_traced(request, current_model, temperature).await {
                        Ok(trace) => {
                            complete_circuit(permit, BreakerOutcome::Answered);
                            if attempt > 0 || *current_model != model {
                                tracing::info!(
                                    provider = %redact_provider_text(provider_name),
                                    model = *current_model,
                                    attempt,
                                    original_model = model,
                                    "Provider recovered (failover/retry)"
                                );
                            }
                            if rate_limited_seen {
                                rate_limit::record_recovered(provider_name);
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
                                response: trace.response,
                                attempts,
                                final_provider: provider_name.to_string(),
                                final_model: (*current_model).to_string(),
                                served_member: Some(member_key.to_string()),
                                tokens_used: trace.tokens_used,
                            });
                        }
                        Err(e) => {
                            complete_circuit(permit, buffered_breaker_outcome(&e));
                            if super::attempt_accounting::is_local_refusal(&e) {
                                return Err(e);
                            }
                            let non_retryable_rate_limit = is_non_retryable_rate_limit(&e);
                            let non_retryable = is_non_retryable(&e) || non_retryable_rate_limit;
                            let rate_limited = is_rate_limited(&e);
                            let failure_reason = failure_reason(rate_limited, non_retryable);
                            let error_detail = compact_error_detail(&e);
                            rate_limited_seen = rate_limited_seen || (rate_limited && !non_retryable);
                            let failure_refusal = e
                                .chain()
                                .find_map(|cause| cause.downcast_ref::<super::ToolSchemaRefusalError>());
                            match failure_refusal {
                                Some(refusal) => {
                                    chain_refusal.get_or_insert_with(|| refusal.clone());
                                }
                                None => every_failure_refused = false,
                            }

                            let kind = retry_kind(&e);
                            push_failure(
                                &mut failures,
                                provider_name,
                                current_model,
                                attempt + 1,
                                retry_budget(self.max_retries, kind, is_last) + 1,
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
                                    provider = %redact_provider_text(provider_name),
                                    model = *current_model,
                                    error = %error_detail,
                                    "Non-retryable error, moving on"
                                );

                                if crate::recovery::overflow::is_context_overflow_error(&e) {
                                    anyhow::bail!(
                                        "Request exceeds model context window; retries and fallbacks were skipped. Attempts:\n{}",
                                        failures.join("\n")
                                    );
                                }

                                if failure_refusal.is_some() {
                                    refused.insert(member_key, current_model);
                                }

                                break;
                            }

                            if may_retry(attempt, self.max_retries, kind, is_last) {
                                let ctx = RetryContext {
                                    provider_name,
                                    gate_key: member_key,
                                    model: current_model,
                                    attempt: attempt + 1,
                                    failure_reason,
                                    error_detail: &error_detail,
                                    rate_limited,
                                    kind,
                                    is_last,
                                };
                                if !self.wait_before_retry(&ctx, backoff_ms, &e).await {
                                    break;
                                }
                                backoff_ms = (backoff_ms.saturating_mul(2)).min(10_000);
                            } else if rate_limited {
                                // Final attempt: no sleep follows, but the
                                // throttle still has to reach the counters — and
                                // the cool-down still has to reach every other
                                // in-flight request, even though this one gives up.
                                let hint = parse_retry_after_ms(&e);
                                rate_limit::record_rate_limited(provider_name, current_model, false, hint.is_some());
                                self.rate_limit_gate.note_rate_limited(
                                    member_key,
                                    hint.unwrap_or(backoff_ms).min(MAX_HONORED_RETRY_AFTER_MS),
                                );
                                break;
                            } else {
                                break;
                            }
                        }
                    }
                }

                if rate_limited_seen {
                    rate_limit::record_exhausted(provider_name);
                    tracing::error!(
                        provider = %redact_provider_text(provider_name),
                        model = *current_model,
                        attempts = self.max_candidate_retries(is_last) + 1,
                        "Provider rate limit outlasted the retry budget; abandoning this candidate"
                    );
                }
                tracing::warn!(
                    provider = %redact_provider_text(provider_name),
                    model = *current_model,
                    "Exhausted retries, trying next provider/model"
                );
            }
        }

        Err(ProviderChainFailure {
            summary: self.all_failed_message(&failures, &runtime_unavailable),
            tool_schema_refusal: chain_refusal.filter(|_| every_failure_refused),
        }
        .into())
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
        let outcome = ProviderExecutionOutcome::from_trace_with_usage(
            decision,
            trace.attempts,
            trace.final_provider,
            trace.final_model,
            started_at,
            finished_at,
            // Single chat call: no earlier-turn fallback to fold in.
            false,
            trace.tokens_used,
        );
        Ok((trace.response, outcome))
    }

    fn supports_native_tools(&self) -> bool {
        self.providers
            .iter()
            .any(|member| member.provider.supports_native_tools())
    }

    fn supports_vision(&self) -> bool {
        self.providers.iter().any(|member| member.provider.supports_vision())
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

        for (model_index, current_model) in models.iter().enumerate() {
            for (
                provider_index,
                ChainMember {
                    name: provider_name,
                    key: member_key,
                    id: provider_id,
                    provider,
                },
            ) in self.providers.iter().enumerate()
            {
                if !self.provider_model_compatible(provider_name, current_model, model) {
                    runtime_unavailable.push((
                        provider_name.to_string(),
                        format!("model '{current_model}' not compatible with provider"),
                    ));
                    continue;
                }
                if let Some(refusal) =
                    self.fallback_refusal(provider_id, provider_name, current_model, !tools.is_empty())
                {
                    runtime_unavailable.push((provider_name.to_string(), refusal.to_string()));
                    continue;
                }

                let mut backoff_ms = self.base_backoff_ms;
                // Tracks whether this candidate was ever throttled, so recovery
                // and give-up can be reported separately from ordinary errors.
                let mut rate_limited_seen = false;

                let is_last = model_index + 1 == models.len() && provider_index + 1 == self.providers.len();
                for attempt in 0..=self.max_candidate_retries(is_last) {
                    let permit = match self.admit_circuit(member_key) {
                        Ok(permit) => permit,
                        Err(ms) => {
                            runtime_unavailable.push((provider_name.to_string(), circuit_reason(ms)));
                            break;
                        }
                    };
                    self.await_rate_limit_gate(provider_name, member_key, current_model)
                        .await;
                    if permit.as_ref().is_some_and(|permit| !permit.revalidate()) {
                        runtime_unavailable.push((provider_name.to_string(), circuit_reason(0)));
                        break;
                    }
                    match provider
                        .chat_with_tools(messages, tools, current_model, temperature)
                        .await
                    {
                        Ok(resp) => {
                            complete_circuit(permit, BreakerOutcome::Answered);
                            if attempt > 0 || *current_model != model {
                                tracing::info!(
                                    provider = %redact_provider_text(provider_name),
                                    model = *current_model,
                                    attempt,
                                    original_model = model,
                                    "Provider recovered (failover/retry)"
                                );
                            }
                            if rate_limited_seen {
                                rate_limit::record_recovered(provider_name);
                            }
                            return Ok(resp);
                        }
                        Err(e) => {
                            complete_circuit(permit, buffered_breaker_outcome(&e));
                            let non_retryable_rate_limit = is_non_retryable_rate_limit(&e);
                            let non_retryable = is_non_retryable(&e) || non_retryable_rate_limit;
                            let rate_limited = is_rate_limited(&e);
                            let failure_reason = failure_reason(rate_limited, non_retryable);
                            let error_detail = compact_error_detail(&e);
                            rate_limited_seen = rate_limited_seen || (rate_limited && !non_retryable);

                            let kind = retry_kind(&e);
                            push_failure(
                                &mut failures,
                                provider_name,
                                current_model,
                                attempt + 1,
                                retry_budget(self.max_retries, kind, is_last) + 1,
                                failure_reason,
                                &error_detail,
                            );

                            if non_retryable {
                                tracing::warn!(
                                    provider = %redact_provider_text(provider_name),
                                    model = *current_model,
                                    error = %error_detail,
                                    "Non-retryable error, moving on"
                                );

                                if crate::recovery::overflow::is_context_overflow_error(&e) {
                                    anyhow::bail!(
                                        "Request exceeds model context window; retries and fallbacks were skipped. Attempts:\n{}",
                                        failures.join("\n")
                                    );
                                }

                                break;
                            }

                            if may_retry(attempt, self.max_retries, kind, is_last) {
                                let ctx = RetryContext {
                                    provider_name,
                                    gate_key: member_key,
                                    model: current_model,
                                    attempt: attempt + 1,
                                    failure_reason,
                                    error_detail: &error_detail,
                                    rate_limited,
                                    kind,
                                    is_last,
                                };
                                if !self.wait_before_retry(&ctx, backoff_ms, &e).await {
                                    break;
                                }
                                backoff_ms = (backoff_ms.saturating_mul(2)).min(10_000);
                            } else if rate_limited {
                                // Final attempt: no sleep follows, but the
                                // throttle still has to reach the counters — and
                                // the cool-down still has to reach every other
                                // in-flight request, even though this one gives up.
                                let hint = parse_retry_after_ms(&e);
                                rate_limit::record_rate_limited(provider_name, current_model, false, hint.is_some());
                                self.rate_limit_gate.note_rate_limited(
                                    member_key,
                                    hint.unwrap_or(backoff_ms).min(MAX_HONORED_RETRY_AFTER_MS),
                                );
                                break;
                            } else {
                                break;
                            }
                        }
                    }
                }

                if rate_limited_seen {
                    rate_limit::record_exhausted(provider_name);
                    tracing::error!(
                        provider = %redact_provider_text(provider_name),
                        model = *current_model,
                        attempts = self.max_candidate_retries(is_last) + 1,
                        "Provider rate limit outlasted the retry budget; abandoning this candidate"
                    );
                }
                tracing::warn!(
                    provider = %redact_provider_text(provider_name),
                    model = *current_model,
                    "Exhausted retries, trying next provider/model"
                );
            }
        }

        anyhow::bail!(self.all_failed_message(&failures, &runtime_unavailable))
    }

    fn supports_streaming(&self) -> bool {
        self.providers.iter().any(|member| member.provider.supports_streaming())
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
            self.max_retries,
            self.base_backoff_ms,
            Arc::clone(&self.rate_limit_gate),
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
            self.max_retries,
            self.base_backoff_ms,
            Arc::clone(&self.rate_limit_gate),
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
            for ChainMember {
                name: provider_name,
                key: member_key,
                id: provider_id,
                provider,
            } in &self.providers
            {
                if !provider.supports_streaming() {
                    continue;
                }
                if !self.provider_model_compatible(provider_name, current_model, model) {
                    continue;
                }
                if self
                    .fallback_refusal(
                        provider_id,
                        provider_name,
                        current_model,
                        options.tools.as_ref().is_some_and(|tools| !tools.is_empty()),
                    )
                    .is_some()
                {
                    continue;
                }
                attempts.push(StreamAttempt {
                    provider_name: Arc::clone(provider_name),
                    member_key: Arc::clone(member_key),
                    provider: Arc::clone(provider),
                    circuit_breaker: self.circuit_breaker.clone(),
                    _lease: self.circuit_lease.clone(),
                    model: current_model.to_string(),
                });
            }
        }
        attempts
    }
}

fn circuit_reason(ms: u64) -> String {
    format!("circuit open; next probe in {}s", ms.div_ceil(1000))
}

fn complete_circuit(permit: Option<AttemptPermit>, outcome: BreakerOutcome) {
    if let Some(permit) = permit {
        permit.complete(outcome);
    }
}

fn buffered_breaker_outcome(err: &anyhow::Error) -> BreakerOutcome {
    circuit_breaker::classify(
        is_tool_schema_refusal(err) || super::attempt_accounting::is_local_refusal(err),
        is_rate_limited(err) || is_non_retryable_rate_limit(err) || is_non_retryable(err),
    )
}

/// One candidate (provider, model) pair for a streaming request.
struct StreamAttempt {
    circuit_breaker: Option<(Arc<CircuitBreakerRegistry>, BreakerParams)>,
    _lease: Option<Arc<ChainLease>>,
    provider_name: Arc<str>,
    /// Refusals and the rate-limit cool-down are keyed by this
    /// ([`member_key`]), never by the display name.
    member_key: Arc<str>,
    provider: Arc<dyn Provider>,
    model: String,
}

/// The error for a content-filter stop reported by `provider`.
pub(crate) fn content_filter_stop(provider: &str, detail: &str) -> super::traits::StreamError {
    super::traits::StreamError::Provider(format!("{CONTENT_FILTER_STOP_PREFIX} ({provider}): {detail}"))
}

/// The error for a streamed tool-call index above `max`.
pub(crate) fn tool_call_index_over_limit(provider: &str, index: usize, max: usize) -> super::traits::StreamError {
    super::traits::StreamError::InvalidSse(format!(
        "{TOOL_CALL_INDEX_LIMIT_PREFIX} ({provider}): index {index} exceeds the supported maximum {max}"
    ))
}

/// The error for an in-stream `{"error": {...}}` object reported by `provider`.
///
/// The label comes from `type`, else `code` (a string code without its JSON
/// quotes), and is limited to 64 identifier characters; the message is capped
/// at 300 characters. A `code`/`type` of `content_filter` or
/// `content_policy_violation` is a content-filter stop.
pub(crate) fn in_stream_error_object(
    provider: &str,
    kind: Option<String>,
    code: Option<serde_json::Value>,
    message: Option<String>,
) -> super::traits::StreamError {
    let code = code.map(|code| match code {
        serde_json::Value::String(code) => code,
        other => other.to_string(),
    });
    let is_filter = [kind.as_deref(), code.as_deref()]
        .into_iter()
        .flatten()
        .any(|value| matches!(value, "content_filter" | "content_policy_violation"));
    let label: String = kind
        .or(code)
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
        .take(64)
        .collect();
    let label = if label.is_empty() {
        "unknown_error".to_string()
    } else {
        label
    };
    let message: String = message.unwrap_or_default().chars().take(300).collect();
    if is_filter {
        content_filter_stop(provider, &format!("error {label}: {message}"))
    } else {
        super::traits::StreamError::Provider(format!("{provider} stream error ({label}): {message}"))
    }
}

/// Extract a Retry-After value (in milliseconds) from a streaming error.
///
/// Prefers the **structured** `retry_after_ms` field carried by
/// [`StreamError::RateLimited`] — read directly off the real HTTP `Retry-After`
/// header by the provider — rather than errors whose textual form happens to
/// embed the value. Falls back to parsing the error's `Display` form (mirroring
/// [`parse_retry_after_ms`]) for any other error shape.
///
/// The non-streaming side reaches the same header through
/// [`RetryAfterHint`](super::rate_limit::RetryAfterHint); neither path depends
/// on an upstream spelling the delay into the response body any more.
fn parse_stream_retry_after_ms(err: &super::traits::StreamError) -> Option<u64> {
    if let super::traits::StreamError::RateLimited {
        retry_after_ms: Some(ms),
        ..
    } = err
    {
        return Some(*ms);
    }
    let anyhow_err = anyhow::anyhow!("{err}");
    parse_retry_after_ms(&anyhow_err)
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
fn drive_streaming_fallback<F>(
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
        let mut last_error: Option<super::traits::StreamError> = None;
        let mut last_refusal: Option<super::traits::StreamError> = None;
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
            let mut pre_content_retries = 0u32;
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
                                    && may_retry(pre_content_retries, max_retries, kind, is_last);
                            let retry = pre_content_retries + 1;

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
                                pre_content_retries = retry;
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
                                            attempt = pre_content_retries + 1,
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
                                        pre_content_retries += 1;
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

                            if rate_limited_seen {
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
                                    attempts = pre_content_retries + 1,
                                    "Streaming rate limit outlasted the retry budget; abandoning this candidate"
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
                .send(Err(super::traits::StreamError::Provider(format!(
                    "All streaming providers/models failed; {}",
                    circuit_refusals.join("; ")
                ))))
                .await;
            return;
        }
        let final_err = last_error.or(last_refusal).unwrap_or_else(|| {
            super::traits::StreamError::Provider("All streaming providers/models failed".to_string())
        });
        let _ = tx.send(Err(final_err)).await;
    });

    stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|chunk| (chunk, rx)) }).boxed()
}

#[cfg(test)]
mod tests;

/// Test support for the "release the connection when nobody reads the
/// stream" probes: a loopback SSE stub that sends one record and then nothing,
/// and reports when the server lets go of that response.
#[cfg(test)]
pub(crate) mod stream_release_probe {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// Serve `first` at `path` as `text/event-stream`, then stay silent
    /// forever. The returned flag turns true when the response body is dropped,
    /// i.e. when the client closed the connection.
    pub(crate) async fn spawn_silent_after_first_record_stub(
        path: &'static str,
        first: &'static str,
    ) -> (String, Arc<AtomicBool>, tokio::task::JoinHandle<()>) {
        use axum::{Router, body::Body, http::header, routing::post};
        use futures_util::StreamExt;

        let released = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&released);
        let app = Router::new().route(
            path,
            post(move || {
                let flag = Arc::clone(&flag);
                async move {
                    let guard = DropFlag(flag);
                    let body = futures_util::stream::once(async move { Ok::<_, std::convert::Infallible>(first) })
                        .chain(futures_util::stream::pending())
                        .map(move |item| {
                            let _keep = &guard;
                            item
                        });
                    ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind loopback");
        let addr = listener.local_addr().expect("test: loopback addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), released, server)
    }

    /// A raw TCP server that accepts connections, reads whatever arrives and
    /// never answers: no status line, no headers. Returns the base URL and
    /// the counts of accepted and client-closed connections.
    pub(crate) async fn spawn_never_answering_server() -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use std::sync::atomic::AtomicUsize;
        use tokio::io::AsyncReadExt;

        let accepted = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind loopback");
        let addr = listener.local_addr().expect("test: loopback addr");
        let (accepted_in, closed_in) = (Arc::clone(&accepted), Arc::clone(&closed));
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                accepted_in.fetch_add(1, Ordering::SeqCst);
                let closed = Arc::clone(&closed_in);
                tokio::spawn(async move {
                    let mut buf = [0_u8; 4096];
                    loop {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                    closed.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        (format!("http://{addr}"), accepted, closed, server)
    }

    /// Wait up to five seconds for the server to see its response released.
    pub(crate) async fn wait_released(released: &AtomicBool) -> bool {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !released.load(Ordering::SeqCst) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        released.load(Ordering::SeqCst)
    }
}

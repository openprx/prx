use super::Provider;
use super::rate_limit::{
    self, MAX_HONORED_RETRY_AFTER_MS, RateLimitGate, jitter_after_hint_ms, jitter_backoff_ms, retry_after_hint_ms,
};
use super::traits::{
    ChatMessage, ChatRequest, ChatResponse, ChatTrace, ProviderCapabilities, ProviderRequestMode, StreamChunk,
    StreamOptions, StreamResult,
};
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

/// Whether `err` is a tool-schema refusal: the endpoint's declared dialect
/// cannot carry a tool the request offered, so nothing was sent.
///
/// Typed, never matched on text: the refusal is deterministic, and a text
/// heuristic both missed it (it was retried with backoff) and could
/// misclassify an unrelated error whose tool or model name happened to
/// contain a matching word.
fn is_tool_schema_refusal(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.downcast_ref::<super::ToolSchemaRefusalError>().is_some())
}

/// Check if an error is non-retryable (client errors that won't resolve with retries).
fn is_non_retryable(err: &anyhow::Error) -> bool {
    if is_context_window_exceeded(err) || is_tool_schema_refusal(err) {
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

/// Business error codes observed on 429 responses where retrying is futile
/// (Z.AI / GLM: plan does not cover the model, insufficient balance).
const NON_RETRYABLE_RATE_LIMIT_CODES: [u16; 2] = [1113, 1311];

/// Match a provider business code only where it appears as the value of a
/// `code`-like key (`"code":1311`, `error_code = 1113`, ...).
///
/// A bare digit scan over the whole message is not safe: an ordinary 429 body
/// carries a `request_id`, token counters and timestamps, and any digit run
/// that happened to read `1113` or `1311` used to classify that response as
/// permanently failed — silently skipping the entire retry budget. Requiring
/// the key context keeps the real business codes matched while making an
/// accidental hit impossible. Default is "retryable".
fn has_non_retryable_business_code(lower: &str) -> bool {
    lower.match_indices("code").any(|(idx, keyword)| {
        let Some(rest) = lower.get(idx + keyword.len()..) else {
            return false;
        };
        let value = rest.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ':' | '='));
        let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
        digits
            .parse::<u16>()
            .is_ok_and(|code| NON_RETRYABLE_RATE_LIMIT_CODES.contains(&code))
    })
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
    is_business_rate_limit_message(&err.to_string().to_lowercase())
}

/// Whether an already-confirmed 429 body describes a billing/entitlement
/// condition rather than transient throttling.
///
/// Split out from [`is_non_retryable_rate_limit`] so the streaming path — which
/// learns the `429` from the structured status code, not from the text — can
/// apply the same verdict without the message having to spell out "429".
fn is_business_rate_limit_message(lower: &str) -> bool {
    // Every hint below names a *billing or entitlement* condition. The bare
    // `"not include"` fragment used to live here too and was far too broad —
    // any upstream sentence containing those two words (for example "this
    // error does not include a request id") turned a transient 429 into a
    // permanent failure.
    let business_hints = [
        "plan does not include",
        "does not include",
        "doesn't include",
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

    has_non_retryable_business_code(lower)
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

    fn eligibility(&self, provider: &super::ProviderId, model: &str) -> Option<FallbackEligibility> {
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
        }
    }

    /// Share a rate-limit gate with other provider chains in this process.
    #[must_use]
    pub fn with_rate_limit_gate(mut self, gate: Arc<RateLimitGate>) -> Self {
        self.rate_limit_gate = gate;
        self
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
        let available = self
            .providers
            .iter()
            .map(|member| member.name.to_string())
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
        match plan_retry_wait(backoff_ms, err) {
            RetryWait::HintExceedsCap(requested_ms) => {
                tracing::warn!(
                    provider = ctx.provider_name,
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
                    provider = ctx.provider_name,
                    model = ctx.model,
                    attempt = ctx.attempt,
                    backoff_ms = wait,
                    retry_after_ms = hint_ms,
                    reason = ctx.failure_reason,
                    error = ctx.error_detail,
                    "Provider call failed, retrying"
                );
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
                provider = provider_name,
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
            for ChainMember {
                name: provider_name,
                key: member_key,
                id: provider_id,
                provider,
            } in &self.providers
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

                for attempt in 0..=self.max_retries {
                    self.await_rate_limit_gate(provider_name, member_key, current_model)
                        .await;
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
                            if rate_limited_seen {
                                rate_limit::record_recovered(provider_name);
                            }
                            return Ok(resp);
                        }
                        Err(e) => {
                            let non_retryable_rate_limit = is_non_retryable_rate_limit(&e);
                            let non_retryable = is_non_retryable(&e) || non_retryable_rate_limit;
                            let rate_limited = is_rate_limited(&e);
                            let failure_reason = failure_reason(rate_limited, non_retryable);
                            let error_detail = compact_error_detail(&e);
                            rate_limited_seen = rate_limited_seen || (rate_limited && !non_retryable);

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
                                let ctx = RetryContext {
                                    provider_name,
                                    gate_key: member_key,
                                    model: current_model,
                                    attempt: attempt + 1,
                                    failure_reason,
                                    error_detail: &error_detail,
                                    rate_limited,
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
                            }
                        }
                    }
                }

                if rate_limited_seen {
                    rate_limit::record_exhausted(provider_name);
                    tracing::error!(
                        provider = %provider_name,
                        model = *current_model,
                        attempts = self.max_retries + 1,
                        "Provider rate limit outlasted the retry budget; abandoning this candidate"
                    );
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
            for ChainMember {
                name: provider_name,
                key: member_key,
                id: provider_id,
                provider,
            } in &self.providers
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

                for attempt in 0..=self.max_retries {
                    self.await_rate_limit_gate(provider_name, member_key, current_model)
                        .await;
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
                            if rate_limited_seen {
                                rate_limit::record_recovered(provider_name);
                            }
                            return Ok(resp);
                        }
                        Err(e) => {
                            let non_retryable_rate_limit = is_non_retryable_rate_limit(&e);
                            let non_retryable = is_non_retryable(&e) || non_retryable_rate_limit;
                            let rate_limited = is_rate_limited(&e);
                            let failure_reason = failure_reason(rate_limited, non_retryable);
                            let error_detail = compact_error_detail(&e);
                            rate_limited_seen = rate_limited_seen || (rate_limited && !non_retryable);

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
                                let ctx = RetryContext {
                                    provider_name,
                                    gate_key: member_key,
                                    model: current_model,
                                    attempt: attempt + 1,
                                    failure_reason,
                                    error_detail: &error_detail,
                                    rate_limited,
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
                            }
                        }
                    }
                }

                if rate_limited_seen {
                    rate_limit::record_exhausted(provider_name);
                    tracing::error!(
                        provider = %provider_name,
                        model = *current_model,
                        attempts = self.max_retries + 1,
                        "Provider rate limit outlasted the retry budget; abandoning this candidate"
                    );
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

        for current_model in &models {
            for ChainMember {
                name: provider_name,
                key: member_key,
                id: provider_id,
                provider,
            } in &self.providers
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

                for attempt in 0..=self.max_retries {
                    super::attempt_accounting::ensure_supported(provider.as_ref())?;
                    self.await_rate_limit_gate(provider_name, member_key, current_model)
                        .await;
                    let attempt_started_at = chrono::Utc::now();
                    match provider.chat_traced(request, current_model, temperature).await {
                        Ok(trace) => {
                            if attempt > 0 || *current_model != model {
                                tracing::info!(
                                    provider = %provider_name,
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

                                if failure_refusal.is_some() {
                                    refused.insert(member_key, current_model);
                                }

                                break;
                            }

                            if attempt < self.max_retries {
                                let ctx = RetryContext {
                                    provider_name,
                                    gate_key: member_key,
                                    model: current_model,
                                    attempt: attempt + 1,
                                    failure_reason,
                                    error_detail: &error_detail,
                                    rate_limited,
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
                            }
                        }
                    }
                }

                if rate_limited_seen {
                    rate_limit::record_exhausted(provider_name);
                    tracing::error!(
                        provider = %provider_name,
                        model = *current_model,
                        attempts = self.max_retries + 1,
                        "Provider rate limit outlasted the retry budget; abandoning this candidate"
                    );
                }
                tracing::warn!(
                    provider = %provider_name,
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

        for current_model in &models {
            for ChainMember {
                name: provider_name,
                key: member_key,
                id: provider_id,
                provider,
            } in &self.providers
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

                for attempt in 0..=self.max_retries {
                    self.await_rate_limit_gate(provider_name, member_key, current_model)
                        .await;
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
                            if rate_limited_seen {
                                rate_limit::record_recovered(provider_name);
                            }
                            return Ok(resp);
                        }
                        Err(e) => {
                            let non_retryable_rate_limit = is_non_retryable_rate_limit(&e);
                            let non_retryable = is_non_retryable(&e) || non_retryable_rate_limit;
                            let rate_limited = is_rate_limited(&e);
                            let failure_reason = failure_reason(rate_limited, non_retryable);
                            let error_detail = compact_error_detail(&e);
                            rate_limited_seen = rate_limited_seen || (rate_limited && !non_retryable);

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
                                let ctx = RetryContext {
                                    provider_name,
                                    gate_key: member_key,
                                    model: current_model,
                                    attempt: attempt + 1,
                                    failure_reason,
                                    error_detail: &error_detail,
                                    rate_limited,
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
                            }
                        }
                    }
                }

                if rate_limited_seen {
                    rate_limit::record_exhausted(provider_name);
                    tracing::error!(
                        provider = %provider_name,
                        model = *current_model,
                        attempts = self.max_retries + 1,
                        "Provider rate limit outlasted the retry budget; abandoning this candidate"
                    );
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
    /// Refusals and the rate-limit cool-down are keyed by this
    /// ([`member_key`]), never by the display name.
    member_key: Arc<str>,
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

/// Start of the error PRX builds when a stream reports a content-filter stop.
/// Providers build it only from structured fields (`finish_reason:
/// content_filter`, or an in-stream error whose `code`/`type` names the
/// filter), never from upstream prose.
pub(crate) const CONTENT_FILTER_STOP_PREFIX: &str = "content filter stop";

/// Start of the error PRX builds when a streamed tool-call index exceeds the
/// supported maximum.
pub(crate) const TOOL_CALL_INDEX_LIMIT_PREFIX: &str = "streamed tool-call index over the limit";

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

/// Deterministic pre-content failures that PRX itself detected from
/// structured stream fields. Retrying the same provider in place cannot change
/// the outcome, so these skip only the in-place retry; another candidate may
/// still succeed (filters and parsers differ by provider), so fallback stays.
///
/// Matched by the PRX-built prefix at the start of the message, never by a
/// phrase anywhere in upstream text.
fn skips_in_place_retry(err: &super::traits::StreamError) -> bool {
    use super::traits::StreamError;
    match err {
        StreamError::Provider(message) => message.starts_with(CONTENT_FILTER_STOP_PREFIX),
        StreamError::InvalidSse(message) => message.starts_with(TOOL_CALL_INDEX_LIMIT_PREFIX),
        _ => false,
    }
}

/// Whether a stream error is PRX's own tool-schema refusal: a provider
/// message that is a rendered refusal ([`super::is_tool_schema_refusal_text`]).
fn stream_error_is_tool_schema_refusal(err: &super::traits::StreamError) -> bool {
    matches!(err, super::traits::StreamError::Provider(message) if super::is_tool_schema_refusal_text(message))
}

/// Classify a [`StreamError`] for failover decisions. Reuses the same
/// string/`reqwest` heuristics as the non-streaming path by routing through the
/// shared `anyhow` classifiers where possible.
fn classify_stream_error(err: &super::traits::StreamError) -> StreamFailureClass {
    use super::traits::StreamError;

    // A failure PRX detected from structured stream fields (a content-filter
    // stop, a tool-call index over the limit) is classified by what it is:
    // deterministic for this candidate, so no in-place retry, and worth a
    // fallback. Its message quotes upstream text (the filter's own words, the
    // index), which the prose heuristics below would misread: `(403)` or
    // "invalid model" in a filter message ended the whole chain, and a
    // tool-call index of 404 or 429 read as an HTTP status (COMPATf F-1).
    if skips_in_place_retry(err) {
        return StreamFailureClass::Retryable;
    }

    // Structured rate-limit (FIX-P0-33): a 429 is rate-limited; a 503 is a
    // transient server failure worth retrying/falling back. Both already carry
    // the parsed Retry-After hint, so the transport classification needs no
    // textual heuristics.
    //
    // The message still has to be inspected for *business* 429s (plan does not
    // cover the model, insufficient balance, ...): those never clear on their
    // own, and since the driver now retries a `RateLimited` class even without
    // a Retry-After hint, misclassifying one would burn the whole retry budget
    // waiting for a quota that is not coming back. This keeps the streaming and
    // non-streaming verdicts identical for the same upstream body.
    if let StreamError::RateLimited { status, message, .. } = err {
        if *status == 429 && is_business_rate_limit_message(&message.to_lowercase()) {
            return StreamFailureClass::NonRetryable;
        }
        return if *status == 429 {
            StreamFailureClass::RateLimited
        } else {
            StreamFailureClass::Retryable
        };
    }

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
        // `StreamError::Http` and `StreamError::RateLimited` are fully classified
        // by the blocks above, which always return before reaching this match.
        // Should that invariant ever change, treat the failure as transient
        // rather than panicking.
        StreamError::Http(_) | StreamError::RateLimited { .. } => return StreamFailureClass::Retryable,
    };
    // The stream boundary carries errors as text, so a tool-schema refusal is
    // recognised by the refusal's own rendering at the start of a provider
    // message, never by the marker anywhere in upstream text (SP-INT2 a51).
    // It is deterministic: retrying the same provider cannot help.
    if stream_error_is_tool_schema_refusal(err) {
        return StreamFailureClass::NonRetryable;
    }
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
///    backs off and retries the **same** provider/model up to `max_retries`
///    before considering fallback — mirroring the non-streaming loops. An
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
                if let Some(deferred_ms) = gate.wait_until_clear(&attempt.member_key, &attempt.provider_name).await {
                    tracing::warn!(
                        provider = %attempt.provider_name,
                        model = %attempt.model,
                        deferred_ms,
                        "Deferred behind the shared provider rate-limit cool-down"
                    );
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
                                    provider = %attempt.provider_name,
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
                            let retry_in_place =
                                matches!(class, StreamFailureClass::RateLimited | StreamFailureClass::Retryable)
                                    && !skips_in_place_retry(&err)
                                    && pre_content_retries < max_retries;

                            if retry_in_place {
                                match plan_wait_from_hint(backoff_ms, hint_ms) {
                                    RetryWait::Sleep(wait) => {
                                        tracing::warn!(
                                            provider = %attempt.provider_name,
                                            model = %attempt.model,
                                            ?class,
                                            retry_after_ms = hint_ms,
                                            wait_ms = wait,
                                            attempt = pre_content_retries + 1,
                                            "Streaming failure before content; backing off and retrying same provider/model: {err}"
                                        );
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
                                            provider = %attempt.provider_name,
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
                                    provider = %attempt.provider_name,
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
                                    provider = %attempt.provider_name,
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
                                provider = %attempt.provider_name,
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
        let final_err = last_error.or(last_refusal).unwrap_or_else(|| {
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

    struct MeteredMockProvider {
        calls: Arc<AtomicUsize>,
        fail_until_attempt: usize,
        response: &'static str,
        error: &'static str,
        usage: crate::llm::route_decision::TokenUsage,
    }

    #[async_trait]
    impl Provider for MeteredMockProvider {
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

        async fn chat_traced(
            &self,
            _request: ChatRequest<'_>,
            model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatTrace> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt <= self.fail_until_attempt {
                anyhow::bail!(self.error);
            }
            let started_at = chrono::Utc::now();
            let finished_at = chrono::Utc::now();
            Ok(ChatTrace {
                response: ChatResponse {
                    text: Some(self.response.to_string()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                },
                attempts: vec![ProviderAttempt {
                    seq: 1,
                    provider: "metered-mock".to_string(),
                    model: model.to_string(),
                    started_at,
                    finished_at,
                    status: AttemptStatus::Success,
                    error_class: None,
                    error_message: None,
                }],
                final_provider: "metered-mock".to_string(),
                final_model: model.to_string(),
                served_member: None,
                tokens_used: self.usage.clone(),
            })
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
    async fn chat_traced_counts_only_successful_attempt_usage_after_fallback() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));

        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(MeteredMockProvider {
                        calls: Arc::clone(&primary_calls),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "500 primary down",
                        usage: crate::llm::route_decision::TokenUsage::reported(
                            Some(10_000),
                            Some(10_000),
                            Some(20_000),
                        ),
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(MeteredMockProvider {
                        calls: Arc::clone(&fallback_calls),
                        fail_until_attempt: 0,
                        response: "from fallback",
                        error: "fallback down",
                        usage: crate::llm::route_decision::TokenUsage::reported(Some(11), Some(7), Some(18)),
                    }),
                ),
            ],
            1,
            1,
        );

        let messages = vec![ChatMessage::user("hi")];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
        };
        let trace = provider.chat_traced(request, "test", 0.0).await.unwrap();

        assert_eq!(primary_calls.load(Ordering::SeqCst), 2);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
        assert_eq!(trace.response.text_or_empty(), "from fallback");
        assert_eq!(
            trace.tokens_used.source,
            crate::llm::route_decision::TokenUsageSource::Reported
        );
        assert_eq!(
            trace.tokens_used.total_tokens,
            Some(18),
            "failed primary attempts must not be added to successful fallback usage"
        );
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

    /// Mock that fails for a specific set of `(provider_model)` combinations and
    /// succeeds otherwise, recording the `(model)` of every call. The provider
    /// identity is fixed at construction via `provider_tag`, so two instances
    /// registered under different names can model a cross-provider failover.
    struct ProviderModelAwareMock {
        provider_tag: &'static str,
        calls: Arc<AtomicUsize>,
        models_seen: parking_lot::Mutex<Vec<String>>,
        /// `(provider_tag, model)` pairs that must fail.
        fail_on: Vec<(&'static str, &'static str)>,
        response: &'static str,
    }

    #[async_trait]
    impl Provider for ProviderModelAwareMock {
        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.models_seen.lock().push(model.to_string());
            if self.fail_on.iter().any(|(p, m)| *p == self.provider_tag && *m == model) {
                anyhow::bail!("500 {} cannot serve {}", self.provider_tag, model);
            }
            Ok(ChatResponse {
                text: Some(self.response.to_string()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }

        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(format!("{}:{model}", self.provider_tag))
        }
    }

    #[tokio::test]
    async fn chat_traced_real_provider_model_failover_lands_on_different_provider_and_model() {
        // #2: a REAL `ReliableProvider` failover (not a `from_trace` helper). The
        // model chain is m1 -> m2. Provider A can serve neither m1 nor m2;
        // provider B can serve m2 only. So the sequence is:
        //   A/m1 (fail) → B/m1 (fail) → A/m2 (fail) → B/m2 (success)
        // The terminal success lands on a DIFFERENT provider AND a DIFFERENT model
        // than routed, so the outcome must be `provider_model_fallback`.
        use crate::llm::route_decision::{AttemptStatus, ExecutionStatus, RouteDecision};

        let a_calls = Arc::new(AtomicUsize::new(0));
        let b_calls = Arc::new(AtomicUsize::new(0));
        let mut model_fallbacks = std::collections::HashMap::new();
        model_fallbacks.insert("m1".to_string(), vec!["m2".to_string()]);

        let provider = ReliableProvider::new(
            vec![
                (
                    "provider-a".into(),
                    Box::new(ProviderModelAwareMock {
                        provider_tag: "provider-a",
                        calls: Arc::clone(&a_calls),
                        models_seen: parking_lot::Mutex::new(Vec::new()),
                        // A fails for every model it is asked to serve.
                        fail_on: vec![("provider-a", "m1"), ("provider-a", "m2")],
                        response: "unused",
                    }),
                ),
                (
                    "provider-b".into(),
                    Box::new(ProviderModelAwareMock {
                        provider_tag: "provider-b",
                        calls: Arc::clone(&b_calls),
                        models_seen: parking_lot::Mutex::new(Vec::new()),
                        // B fails on m1 but serves m2.
                        fail_on: vec![("provider-b", "m1")],
                        response: "served by B on m2",
                    }),
                ),
            ],
            0, // no in-place retries → straight provider/model failover.
            1,
        )
        .with_model_fallbacks(model_fallbacks);

        // Router selected provider-a / m1.
        let decision = RouteDecision::single_candidate("provider-a", "m1");
        let messages = vec![ChatMessage::user("hi")];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
        };

        let (response, outcome) = provider.chat_with_decision(&decision, request, 0.0).await.unwrap();
        assert_eq!(response.text_or_empty(), "served by B on m2");

        // Real failover crossed both provider AND model boundaries.
        assert_eq!(outcome.final_provider, "provider-b");
        assert_eq!(outcome.final_model, "m2");
        assert_eq!(outcome.status, ExecutionStatus::FallbackSuccess);
        assert_eq!(
            outcome.fallback_reason.as_deref(),
            Some("provider_model_fallback"),
            "crossing both provider and model must be classified provider_model_fallback"
        );

        // Attempt sequence: 3 failures (A/m1, B/m1, A/m2) + 1 success (B/m2).
        assert_eq!(outcome.attempts.len(), 4, "three failures then one success");
        let failed: Vec<_> = outcome
            .attempts
            .iter()
            .filter(|a| a.status == AttemptStatus::Failed)
            .collect();
        assert_eq!(failed.len(), 3, "exactly three failed attempts before success");
        let last = outcome.attempts.last().unwrap();
        assert_eq!(last.status, AttemptStatus::Success);
        assert_eq!(last.provider, "provider-b");
        assert_eq!(last.model, "m2");
        // seq is monotonic from 1.
        for (idx, attempt) in outcome.attempts.iter().enumerate() {
            assert_eq!(attempt.seq as usize, idx + 1, "seq must be monotonic from 1");
        }
    }

    #[tokio::test]
    async fn chat_with_decision_all_candidates_failing_yields_error_not_silent_success() {
        // #7 (chat-outcome level): when every provider fails, `chat_with_decision`
        // must propagate an error (it cannot fabricate a clean Success). The chat
        // orchestration layer turns this into an `ExecutionStatus::AllFailed`
        // outcome via `failed_for_decision`; here we lock the contract that the
        // provider surfaces a failure rather than a degraded success.
        use crate::llm::route_decision::{ExecutionStatus, RouteDecision};

        let provider = ReliableProvider::new(
            vec![
                (
                    "p1".into(),
                    Box::new(MockProvider {
                        calls: Arc::new(AtomicUsize::new(0)),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "500 p1 down",
                    }),
                ),
                (
                    "p2".into(),
                    Box::new(MockProvider {
                        calls: Arc::new(AtomicUsize::new(0)),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "503 p2 down",
                    }),
                ),
            ],
            0,
            1,
        );

        let decision = RouteDecision::single_candidate("p1", "test");
        let messages = vec![ChatMessage::user("hi")];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
        };

        let started = chrono::Utc::now();
        let err = provider
            .chat_with_decision(&decision, request, 0.0)
            .await
            .expect_err("all providers failing must surface an error, not a degraded success");

        // The chat layer maps this failure to an AllFailed outcome; assert that
        // mapping records the correct terminal status and a classified error.
        let outcome = ProviderExecutionOutcome::failed_for_decision(&decision, started, &err);
        assert!(
            matches!(outcome.status, ExecutionStatus::AllFailed { .. }),
            "all-candidates-failed must record AllFailed, got {:?}",
            outcome.status
        );
        if let ExecutionStatus::AllFailed { last_error_class } = &outcome.status {
            assert!(
                !last_error_class.is_empty(),
                "AllFailed must carry a non-empty classified error class"
            );
        }
        assert_eq!(
            outcome.final_provider, "p1",
            "failed outcome attributes to the routed selection"
        );
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

    // ── Rate-limit handling: mocks and evidence ─────────────────

    /// Non-streaming mock answering with a 429 in the exact shape
    /// [`api_error`](super::super::api_error) builds: sanitized body text, plus
    /// the structured `Retry-After` hint when the upstream sent that header.
    struct RateLimit429Mock {
        calls: Arc<AtomicUsize>,
        /// Number of leading calls that fail; later calls succeed.
        fail_until_attempt: usize,
        retry_after_ms: Option<u64>,
        body: &'static str,
    }

    impl RateLimit429Mock {
        fn error(&self) -> anyhow::Error {
            let message = format!("Mock API error (429 Too Many Requests): {}", self.body);
            match self.retry_after_ms {
                Some(millis) => {
                    anyhow::Error::new(super::super::rate_limit::RetryAfterHint { millis }).context(message)
                }
                None => anyhow::anyhow!("{message}"),
            }
        }
    }

    #[async_trait]
    impl Provider for RateLimit429Mock {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt <= self.fail_until_attempt {
                return Err(self.error());
            }
            Ok("recovered".to_string())
        }
    }

    /// Mock that records the wall-clock offset of every call, so a test can see
    /// whether concurrent retries land on the same instant.
    struct CallTimeRecorderMock {
        origin: std::time::Instant,
        offsets: Arc<parking_lot::Mutex<Vec<u128>>>,
        calls: Arc<AtomicUsize>,
        fail_until_attempt: usize,
    }

    #[async_trait]
    impl Provider for CallTimeRecorderMock {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == 2 {
                self.offsets.lock().push(self.origin.elapsed().as_millis());
            }
            if attempt <= self.fail_until_attempt {
                anyhow::bail!("Mock API error (429 Too Many Requests): rate limit exceeded");
            }
            Ok("recovered".to_string())
        }
    }

    /// Evidence (non-streaming, no `Retry-After`): a 429 is retried on the local
    /// exponential schedule, and the schedule is really slept through.
    #[tokio::test]
    async fn non_streaming_429_without_retry_after_backs_off_exponentially() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![(
                "np-no-hint".into(),
                Box::new(RateLimit429Mock {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: 2,
                    retry_after_ms: None,
                    body: "rate limit exceeded",
                }),
            )],
            2,
            200,
        );

        let started = std::time::Instant::now();
        let out = provider.chat_with_system(None, "hi", "m", 0.0).await;
        let elapsed = started.elapsed();

        assert_eq!(out.ok().as_deref(), Some("recovered"));
        assert_eq!(calls.load(Ordering::SeqCst), 3, "initial attempt plus two retries");
        // Jittered equal-jitter band: [100,200] + [200,400] = [300,600].
        assert!(
            elapsed >= Duration::from_millis(290),
            "backoff was not actually slept: {elapsed:?}"
        );
        assert!(elapsed < Duration::from_millis(1_500), "backoff overshot: {elapsed:?}");
    }

    /// Evidence (non-streaming, with `Retry-After`): the upstream delay wins over
    /// the (much smaller) local backoff. Before the structured hint existed this
    /// header was dropped on the floor for every non-streaming provider.
    #[tokio::test]
    async fn non_streaming_429_with_retry_after_honors_the_upstream_delay() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![(
                "np-hint".into(),
                Box::new(RateLimit429Mock {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: 1,
                    retry_after_ms: Some(400),
                    body: "rate_limit_error",
                }),
            )],
            2,
            50,
        );

        let started = std::time::Instant::now();
        let out = provider.chat_with_system(None, "hi", "m", 0.0).await;
        let elapsed = started.elapsed();

        assert_eq!(out.ok().as_deref(), Some("recovered"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            elapsed >= Duration::from_millis(390),
            "upstream Retry-After was ignored (local backoff is only 50ms): {elapsed:?}"
        );
    }

    /// Evidence: a `Retry-After` beyond the honored ceiling is not truncated into
    /// a wait that is known to be too short — the candidate is abandoned and the
    /// caller gets the full attempt trail instead.
    #[tokio::test]
    async fn non_streaming_oversized_retry_after_abandons_candidate_without_waiting() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![(
                "np-oversized".into(),
                Box::new(RateLimit429Mock {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: 99,
                    retry_after_ms: Some(MAX_HONORED_RETRY_AFTER_MS + 1_000),
                    body: "rate_limit_error",
                }),
            )],
            3,
            50,
        );

        let started = std::time::Instant::now();
        let out = provider.chat_with_system(None, "hi", "m", 0.0).await;

        assert!(out.is_err(), "an unrelievable throttle must surface");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no truncated wait, no blind retry");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "must not sleep out an over-ceiling hint"
        );
    }

    /// Evidence: concurrent callers throttled at the same instant no longer retry
    /// in lock-step. Remove the jitter and every offset collapses onto the same
    /// millisecond, which is exactly the thundering herd this guards.
    #[tokio::test]
    async fn concurrent_retries_are_not_phase_locked() {
        const CALLERS: usize = 12;
        let origin = std::time::Instant::now();
        let offsets = Arc::new(parking_lot::Mutex::new(Vec::new()));

        let mut handles = Vec::new();
        for _ in 0..CALLERS {
            let offsets = Arc::clone(&offsets);
            handles.push(tokio::spawn(async move {
                // A private gate per caller: this test isolates jitter, and the
                // shared-gate behaviour is covered separately.
                let provider = ReliableProvider::new(
                    vec![(
                        "jitter-probe".into(),
                        Box::new(CallTimeRecorderMock {
                            origin,
                            offsets,
                            calls: Arc::new(AtomicUsize::new(0)),
                            fail_until_attempt: 1,
                        }),
                    )],
                    2,
                    400,
                );
                let _ = provider.chat_with_system(None, "hi", "m", 0.0).await;
            }));
        }
        for handle in handles {
            let _ = handle.await;
        }

        let samples = offsets.lock().clone();
        assert_eq!(samples.len(), CALLERS, "every caller must have retried once");
        let min = samples.iter().copied().min().unwrap_or(0);
        let max = samples.iter().copied().max().unwrap_or(0);
        assert!(
            max - min >= 40,
            "retry instants are phase-locked (spread {}ms over {samples:?}); jitter is missing",
            max - min
        );
    }

    /// Evidence: the shared gate turns one request's 429 into a deferral for the
    /// others, without capping how many may run at once.
    #[tokio::test]
    async fn a_429_defers_other_requests_sharing_the_gate() {
        let gate = Arc::new(RateLimitGate::new());
        let throttled = ReliableProvider::new(
            vec![(
                "gate-probe".into(),
                Box::new(RateLimit429Mock {
                    calls: Arc::new(AtomicUsize::new(0)),
                    fail_until_attempt: 99,
                    retry_after_ms: Some(400),
                    body: "rate_limit_error",
                }),
            )],
            0,
            50,
        )
        .with_rate_limit_gate(Arc::clone(&gate));
        let healthy = ReliableProvider::new(
            vec![(
                "gate-probe".into(),
                Box::new(RateLimit429Mock {
                    calls: Arc::new(AtomicUsize::new(0)),
                    fail_until_attempt: 0,
                    retry_after_ms: None,
                    body: "unused",
                }),
            )],
            0,
            50,
        )
        .with_rate_limit_gate(Arc::clone(&gate));

        assert!(throttled.chat_with_system(None, "hi", "m", 0.0).await.is_err());

        let started = std::time::Instant::now();
        let out = healthy.chat_with_system(None, "hi", "m", 0.0).await;
        let elapsed = started.elapsed();

        assert_eq!(out.ok().as_deref(), Some("recovered"));
        assert!(
            elapsed >= Duration::from_millis(300),
            "a second request must observe the shared cool-down: {elapsed:?}"
        );
    }

    /// Evidence: an ordinary 429 whose `request_id` happens to contain `1113` or
    /// `1311` must keep its retries. The old bare digit scan classified these as
    /// permanent business failures and skipped the whole budget.
    #[tokio::test]
    async fn generic_429_with_business_code_digits_in_request_id_is_still_retried() {
        for request_id in ["req_011CbBM1113uwxpsbDo2", "req_01311FQE3zdVWaa"] {
            let calls = Arc::new(AtomicUsize::new(0));
            let body: &'static str = Box::leak(
                format!(
                    "{{\"type\":\"error\",\"error\":{{\"type\":\"rate_limit_error\"}},\"request_id\":\"{request_id}\"}}"
                )
                .into_boxed_str(),
            );
            let provider = ReliableProvider::new(
                vec![(
                    "digits-probe".into(),
                    Box::new(RateLimit429Mock {
                        calls: Arc::clone(&calls),
                        fail_until_attempt: 1,
                        retry_after_ms: None,
                        body,
                    }),
                )],
                2,
                1,
            );

            let out = provider.chat_with_system(None, "hi", "m", 0.0).await;
            assert_eq!(
                out.ok().as_deref(),
                Some("recovered"),
                "request_id containing {request_id} must not be read as a business code"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 2, "the retry must actually happen");
        }
    }

    #[test]
    fn business_code_matching_requires_a_code_key() {
        assert!(has_non_retryable_business_code("{\"code\":1311,\"message\":\"nope\"}"));
        assert!(has_non_retryable_business_code("error_code = 1113"));
        assert!(has_non_retryable_business_code("{\"error_code\": 1113}"));
        assert!(
            !has_non_retryable_business_code("\"request_id\":\"req_1311abc\""),
            "a bare digit run must not be read as a business code"
        );
        assert!(
            !has_non_retryable_business_code("input_tokens: 1113"),
            "token counters must not be read as business codes"
        );
        assert!(!has_non_retryable_business_code("{\"code\":429}"));
    }

    #[test]
    fn non_retryable_rate_limit_ignores_incidental_business_digits() {
        let err = anyhow::anyhow!(
            "{}",
            "Anthropic API error (429 Too Many Requests): {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"Number of request tokens has exceeded your per-minute rate limit\"},\"request_id\":\"req_011CbBM1113uwx\"}"
        );
        assert!(
            !is_non_retryable_rate_limit(&err),
            "an ordinary 429 must stay retryable regardless of digits in its request id"
        );
    }

    #[test]
    fn non_retryable_rate_limit_ignores_a_bare_not_include_phrase() {
        let err = anyhow::anyhow!(
            "{}",
            "API error (429 Too Many Requests): {\"message\":\"this response does not include a retry hint\"}"
        );
        // "does not include" is still an entitlement hint, but the previously
        // matched bare "not include" fragment is gone; guard the narrower rule.
        assert!(is_non_retryable_rate_limit(&err));
        let unrelated = anyhow::anyhow!(
            "{}",
            "API error (429 Too Many Requests): {\"message\":\"retry later; results will not include cached rows\"}"
        );
        assert!(
            !is_non_retryable_rate_limit(&unrelated),
            "a bare 'not include' must no longer condemn a transient 429"
        );
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
    fn retry_plan_uses_retry_after() {
        let err = anyhow::anyhow!("429 Retry-After: 3");
        // Honored hints are only ever jittered upward, never below the ask.
        let RetryWait::Sleep(wait) = plan_retry_wait(500, &err) else {
            panic!("a 3s hint is well under the honored ceiling");
        };
        assert!((3_000..=3_750).contains(&wait), "unexpected wait {wait}");
    }

    #[test]
    fn retry_plan_honors_a_60s_retry_after_instead_of_truncating_it() {
        // Regression: the old 30s ceiling halved Anthropic/OpenAI's routine
        // `Retry-After: 60`, which guarantees an immediate second 429.
        let err = anyhow::anyhow!("429 Too Many Requests Retry-After: 60");
        let RetryWait::Sleep(wait) = plan_retry_wait(1_000, &err) else {
            panic!("60s is within the honored ceiling");
        };
        assert!(wait >= 60_000, "a 60s Retry-After must not be truncated: {wait}");
        assert!(wait <= 65_000, "jitter must stay bounded: {wait}");
    }

    #[test]
    fn retry_plan_abandons_candidate_when_hint_exceeds_the_ceiling() {
        let err = anyhow::anyhow!("429 Retry-After: 600");
        assert_eq!(plan_retry_wait(500, &err), RetryWait::HintExceedsCap(600_000));
    }

    #[test]
    fn retry_plan_falls_back_to_jittered_base() {
        let err = anyhow::anyhow!("500 Server Error");
        for _ in 0..200 {
            let RetryWait::Sleep(wait) = plan_retry_wait(500, &err) else {
                panic!("no hint means a plain backoff");
            };
            assert!((250..=500).contains(&wait), "unexpected wait {wait}");
        }
    }

    #[test]
    fn retry_plan_reads_the_structured_retry_after_hint() {
        // Evidence for the non-streaming Retry-After path: providers attach the
        // real header as a structured source, not as message text.
        let err = anyhow::Error::new(super::super::rate_limit::RetryAfterHint { millis: 45_000 })
            .context("Anthropic API error (429 Too Many Requests): rate_limit_error");
        assert_eq!(parse_retry_after_ms(&err), Some(45_000));
        let RetryWait::Sleep(wait) = plan_retry_wait(1_000, &err) else {
            panic!("45s is within the honored ceiling");
        };
        assert!((45_000..=50_000).contains(&wait), "unexpected wait {wait}");
    }

    #[test]
    fn retry_plan_floors_a_zero_hint_at_the_local_backoff() {
        let err = anyhow::Error::new(super::super::rate_limit::RetryAfterHint { millis: 0 })
            .context("API error (429 Too Many Requests): slow down");
        let RetryWait::Sleep(wait) = plan_retry_wait(800, &err) else {
            panic!("0ms hint must still sleep");
        };
        assert!(wait >= 800, "a zero hint must not defeat the local backoff: {wait}");
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

    fn counting_mock(calls: &Arc<AtomicUsize>, fail: bool, response: &'static str) -> Box<dyn Provider> {
        Box::new(MockProvider {
            calls: Arc::clone(calls),
            fail_until_attempt: if fail { usize::MAX } else { 0 },
            response,
            error: "500 upstream down",
        })
    }

    /// D-072 L-9 / D-075, RED before (B5 made llama.cpp accept any model
    /// everywhere): a llama.cpp *fallback* answered an Anthropic route after
    /// Anthropic failed, with whatever model the server had loaded. After: a
    /// llama.cpp member answers only its own route.
    #[tokio::test]
    async fn a_llamacpp_fallback_never_answers_for_another_familys_model() {
        let calls_anthropic = Arc::new(AtomicUsize::new(0));
        let calls_llamacpp = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![
                ("anthropic".into(), counting_mock(&calls_anthropic, true, "never")),
                ("llamacpp".into(), counting_mock(&calls_llamacpp, false, "local answer")),
            ],
            0,
            1,
        );
        let err = provider
            .simple_chat("hello", "anthropic/claude-sonnet-5", 0.0)
            .await
            .expect_err("the local fallback must not answer for Anthropic's model");
        assert!(err.to_string().contains("not compatible with provider"), "{err}");
        assert_eq!(calls_anthropic.load(Ordering::SeqCst), 1);
        assert_eq!(calls_llamacpp.load(Ordering::SeqCst), 0);
    }

    fn openai_then_llamacpp(
        calls_openai: &Arc<AtomicUsize>,
        calls_llamacpp: &Arc<AtomicUsize>,
        fallbacks: &[&str],
    ) -> ReliableProvider {
        ReliableProvider::new(
            vec![
                ("openai".into(), counting_mock(calls_openai, true, "never")),
                ("llamacpp".into(), counting_mock(calls_llamacpp, false, "local answer")),
            ],
            0,
            1,
        )
        .with_model_fallbacks(HashMap::from([(
            "gpt-5".to_string(),
            fallbacks.iter().map(ToString::to_string).collect(),
        )]))
    }

    /// D-076 N-1, RED before (D-075 made a llama.cpp fallback serve nothing):
    /// a `model_fallbacks` entry that names a model repository is served by
    /// the llama.cpp fallback when the route's provider fails. Reviewer probe
    /// `[openai(fail), llamacpp]`, `{"gpt-5": ["ggml-org/gpt-oss-20b-GGUF"]}`.
    #[tokio::test]
    async fn a_llamacpp_fallback_serves_a_repository_named_in_model_fallbacks() {
        let calls_openai = Arc::new(AtomicUsize::new(0));
        let calls_llamacpp = Arc::new(AtomicUsize::new(0));
        let provider = openai_then_llamacpp(&calls_openai, &calls_llamacpp, &["ggml-org/gpt-oss-20b-GGUF"]);
        let answer = provider
            .simple_chat("hello", "gpt-5", 0.0)
            .await
            .expect("the named repository is served by the local fallback");
        assert_eq!(answer, "local answer");
        assert_eq!(calls_openai.load(Ordering::SeqCst), 1);
        assert_eq!(calls_llamacpp.load(Ordering::SeqCst), 1);
    }

    /// D-076 N-1: never the route's own model, a bare id or a
    /// `<provider>/…` model routed to another provider. Reviewer probe
    /// `[openai(fail), llamacpp]` with bare `gpt-5`.
    #[tokio::test]
    async fn a_llamacpp_fallback_never_serves_models_routed_to_another_provider() {
        for fallbacks in [
            &[][..],
            &["gpt-4o"][..],
            &["anthropic/claude-sonnet-5", "openai/gpt-4o"][..],
            // D-076 N-10, reviewer probe: the organisation in any case.
            &["OpenAI/gpt-4o", "Anthropic/claude-sonnet-5", "openai /gpt-4o"][..],
        ] {
            let calls_openai = Arc::new(AtomicUsize::new(0));
            let calls_llamacpp = Arc::new(AtomicUsize::new(0));
            let provider = openai_then_llamacpp(&calls_openai, &calls_llamacpp, fallbacks);
            let err = provider
                .simple_chat("hello", "gpt-5", 0.0)
                .await
                .expect_err("nothing here is the local server's");
            assert!(
                err.to_string().contains("not compatible with provider"),
                "{fallbacks:?}: {err}"
            );
            assert_eq!(calls_llamacpp.load(Ordering::SeqCst), 0, "{fallbacks:?}");
        }
    }

    /// D-075: the route's provider identity decides, not the model's
    /// organisation. An Anthropic route whose model is written with a
    /// neutral organisation is still never answered by a llama.cpp fallback.
    #[tokio::test]
    async fn a_llamacpp_fallback_never_answers_an_anthropic_route_whatever_the_model() {
        let calls_anthropic = Arc::new(AtomicUsize::new(0));
        let calls_llamacpp = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![
                ("anthropic".into(), counting_mock(&calls_anthropic, true, "never")),
                ("llamacpp".into(), counting_mock(&calls_llamacpp, false, "local answer")),
            ],
            0,
            1,
        );
        let err = provider
            .simple_chat("hello", "claude-sonnet-5", 0.0)
            .await
            .expect_err("the local fallback must not answer the Anthropic route");
        assert!(err.to_string().contains("not compatible with provider"), "{err}");
        assert_eq!(calls_anthropic.load(Ordering::SeqCst), 1);
        assert_eq!(calls_llamacpp.load(Ordering::SeqCst), 0);
    }

    /// D-075: llama.cpp as the route's provider serves any repository it is
    /// configured for, including one whose organisation names a provider
    /// family.
    #[tokio::test]
    async fn a_llamacpp_route_serves_a_gemma_repository() {
        let calls_llamacpp = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![("llamacpp".into(), counting_mock(&calls_llamacpp, false, "gemma answer"))],
            0,
            1,
        );
        let answer = provider
            .simple_chat("hello", "google/gemma-3-27b-it-GGUF", 0.0)
            .await
            .expect("the configured repository is served");
        assert_eq!(answer, "gemma answer");
        assert_eq!(calls_llamacpp.load(Ordering::SeqCst), 1);
    }

    /// D-072 L-9: llama.cpp as the primary serves the Hugging Face repository
    /// it is configured for.
    #[tokio::test]
    async fn a_llamacpp_primary_serves_the_repository_it_is_configured_for() {
        let calls_llamacpp = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![("llamacpp".into(), counting_mock(&calls_llamacpp, false, "local answer"))],
            0,
            1,
        );
        let answer = provider
            .simple_chat("hello", "ggml-org/gpt-oss-20b-GGUF", 0.0)
            .await
            .expect("the configured repository is served");
        assert_eq!(answer, "local answer");
        assert_eq!(calls_llamacpp.load(Ordering::SeqCst), 1);
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

    #[test]
    fn failover_mode_capabilities_are_the_safe_intersection() {
        let provider = ReliableProvider::new(
            vec![
                ("primary".into(), Box::new(NativeCapabilityMock { native_tools: true })),
                (
                    "fallback".into(),
                    Box::new(NativeCapabilityMock { native_tools: false }),
                ),
            ],
            0,
            1,
        );

        assert!(
            !provider
                .capabilities_for("model", ProviderRequestMode::NonStreaming)
                .native_tool_calling
        );
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
    async fn streaming_retries_same_candidate_then_falls_back_on_pre_content_error() {
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
        assert_eq!(
            primary_calls.load(Ordering::SeqCst),
            3,
            "a retryable pre-content failure must spend the same-candidate retry budget \
             (initial + max_retries) before rotating, exactly like the non-streaming loops"
        );
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn streaming_fallback_emits_complete_attempt_trace() {
        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(StreamMock {
                        calls: Arc::new(AtomicUsize::new(0)),
                        outcome: Err("500 transient server error"),
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(StreamMock {
                        calls: Arc::new(AtomicUsize::new(0)),
                        outcome: Ok("served by fallback"),
                    }),
                ),
            ],
            0,
            1,
        );
        let messages = vec![ChatMessage::user("hi")];
        let mut stream = provider.stream_chat_with_history(&messages, "model", 0.0, enabled_options());
        let mut attempts = Vec::new();
        while let Some(chunk) = stream.next().await {
            if let Some(attempt) = chunk.unwrap().route_attempt {
                attempts.push(attempt);
            }
        }

        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].provider, "primary");
        assert_eq!(attempts[0].status, AttemptStatus::Failed);
        assert_eq!(attempts[1].provider, "fallback");
        assert_eq!(attempts[1].status, AttemptStatus::Success);
        assert_eq!(attempts[0].seq, 1);
        assert_eq!(attempts[1].seq, 2);
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
        assert!(text.is_empty(), "expected an empty value");
        assert!(err.is_some(), "all-failed streaming must surface an error");
    }

    /// Streaming mock that fails the first `fail_first` calls with a 429
    /// carrying a `Retry-After`, then succeeds. Records every call so a test
    /// can assert the *same* provider was retried (not failed over).
    struct RetryAfterStreamMock {
        calls: Arc<AtomicUsize>,
        fail_first: usize,
        retry_after_error: &'static str,
        success_text: &'static str,
    }

    #[async_trait]
    impl Provider for RetryAfterStreamMock {
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
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= self.fail_first {
                let msg = self.retry_after_error.to_string();
                stream::iter(vec![Err(StreamError::Provider(msg))]).boxed()
            } else {
                stream::iter(vec![Ok(text_chunk(self.success_text, true))]).boxed()
            }
        }
    }

    #[tokio::test]
    async fn streaming_honors_retry_after_and_retries_same_provider() {
        // FIX-P0-33: a pre-content 429 with Retry-After must retry the SAME
        // provider/model (after sleeping) rather than immediately falling back.
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(RetryAfterStreamMock {
                        calls: Arc::clone(&primary_calls),
                        fail_first: 1,
                        // Small Retry-After so the test stays fast.
                        retry_after_error: "429 Too Many Requests, Retry-After: 0",
                        success_text: "recovered on retry",
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(StreamMock {
                        calls: Arc::clone(&fallback_calls),
                        outcome: Ok("from fallback"),
                    }),
                ),
            ],
            2, // max_retries = 2 → room for the in-place retry.
            1,
        );

        let messages = vec![ChatMessage::user("hi")];
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, enabled_options());
        let (text, err) = collect_stream(s).await;

        assert!(err.is_none(), "retry-after retry should recover, got {err:?}");
        assert_eq!(text, "recovered on retry", "must retry primary, not fall back");
        // Primary called twice: initial 429 + the honored retry.
        assert_eq!(primary_calls.load(Ordering::SeqCst), 2);
        // Fallback must never be reached.
        assert_eq!(
            fallback_calls.load(Ordering::SeqCst),
            0,
            "retry-after on the same provider must not trigger provider failover"
        );
    }

    /// Streaming mock that fails the first `fail_first` calls with a *structured*
    /// [`StreamError::RateLimited`] carrying `retry_after_ms` read off a real HTTP
    /// header (FIX-P0-33), then succeeds. This is the production shape — the
    /// reliability layer must honor the structured field, not text parsing.
    struct StructuredRateLimitStreamMock {
        calls: Arc<AtomicUsize>,
        fail_first: usize,
        status: u16,
        retry_after_ms: Option<u64>,
        success_text: &'static str,
    }

    #[async_trait]
    impl Provider for StructuredRateLimitStreamMock {
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
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= self.fail_first {
                let retry_after_ms = self.retry_after_ms;
                stream::iter(vec![Err(StreamError::RateLimited {
                    status: self.status,
                    retry_after_ms,
                    // Intentionally NO "retry-after" substring in the message:
                    // proves the honor path uses the structured field, not text.
                    message: "upstream overloaded".to_string(),
                })])
                .boxed()
            } else {
                stream::iter(vec![Ok(text_chunk(self.success_text, true))]).boxed()
            }
        }
    }

    #[tokio::test]
    async fn streaming_honors_structured_retry_after_header_and_retries_same_provider() {
        // FIX-P0-33 (#3): a real HTTP Retry-After header arrives as a structured
        // `StreamError::RateLimited { retry_after_ms }`. The driver must read the
        // structured field, sleep, and retry the SAME provider/model — not fall
        // back. The error text carries no "retry-after" substring, so a recovery
        // here proves the structured path (not Display parsing) is in effect.
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(StructuredRateLimitStreamMock {
                        calls: Arc::clone(&primary_calls),
                        fail_first: 1,
                        status: 429,
                        retry_after_ms: Some(0), // Honor the header; stay fast.
                        success_text: "recovered via structured retry-after",
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(StreamMock {
                        calls: Arc::clone(&fallback_calls),
                        outcome: Ok("from fallback"),
                    }),
                ),
            ],
            2,
            1,
        );

        let messages = vec![ChatMessage::user("hi")];
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, enabled_options());
        let (text, err) = collect_stream(s).await;

        assert!(err.is_none(), "structured retry-after should recover, got {err:?}");
        assert_eq!(text, "recovered via structured retry-after");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 2, "same provider retried once");
        assert_eq!(
            fallback_calls.load(Ordering::SeqCst),
            0,
            "structured retry-after must not trigger provider failover"
        );
    }

    #[tokio::test]
    async fn streaming_honors_retry_after_on_503_overload_and_retries_same_provider() {
        // FIX-P0-33 (#3, completeness): a 503/529 overload response carries the
        // same structured `retry_after_ms` as a 429, but is classified
        // `Retryable` rather than `RateLimited`. The honor path is keyed on the
        // presence of a structured Retry-After, NOT on the failure class, so a
        // 503 with a hint must back off and retry the SAME provider/model rather
        // than immediately rotating to the fallback candidate.
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(StructuredRateLimitStreamMock {
                        calls: Arc::clone(&primary_calls),
                        fail_first: 1,
                        status: 503,             // Overload, classified Retryable (not RateLimited).
                        retry_after_ms: Some(0), // Honor the header; stay fast.
                        success_text: "recovered after 503 retry-after",
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(StreamMock {
                        calls: Arc::clone(&fallback_calls),
                        outcome: Ok("from fallback"),
                    }),
                ),
            ],
            2,
            1,
        );

        let messages = vec![ChatMessage::user("hi")];
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, enabled_options());
        let (text, err) = collect_stream(s).await;

        assert!(err.is_none(), "503 retry-after should recover, got {err:?}");
        assert_eq!(
            text, "recovered after 503 retry-after",
            "must retry primary, not fall back"
        );
        assert_eq!(
            primary_calls.load(Ordering::SeqCst),
            2,
            "503 with Retry-After must retry the same provider once"
        );
        assert_eq!(
            fallback_calls.load(Ordering::SeqCst),
            0,
            "503 Retry-After must be honored before any provider failover"
        );
    }

    #[tokio::test]
    async fn streaming_503_without_retry_after_still_retries_the_same_candidate() {
        // The absence of a `Retry-After` header used to mean "zero retries" on
        // the streaming path — strictly weaker than the non-streaming loops, and
        // fatal with a single candidate. The header now only decides how long to
        // wait, never whether to retry at all.
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(StructuredRateLimitStreamMock {
                        calls: Arc::clone(&primary_calls),
                        fail_first: 1,
                        status: 503,
                        retry_after_ms: None, // No hint: local backoff decides the wait.
                        success_text: "recovered in place",
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(StreamMock {
                        calls: Arc::clone(&fallback_calls),
                        outcome: Ok("from fallback"),
                    }),
                ),
            ],
            2,
            1,
        );

        let messages = vec![ChatMessage::user("hi")];
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, enabled_options());
        let (text, err) = collect_stream(s).await;

        assert!(err.is_none(), "should recover in place, got {err:?}");
        assert_eq!(text, "recovered in place");
        assert_eq!(
            primary_calls.load(Ordering::SeqCst),
            2,
            "hintless 503 must still earn a same-candidate retry"
        );
        assert_eq!(
            fallback_calls.load(Ordering::SeqCst),
            0,
            "recovery in place must not consume the fallback candidate"
        );
    }

    #[tokio::test]
    async fn streaming_429_without_retry_after_retries_instead_of_dying() {
        // The exact production shape of the two logged streaming outages: one
        // candidate, a 429 with no `Retry-After`, and previously zero retries —
        // the turn died on the spot.
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![(
                "only".into(),
                Box::new(StructuredRateLimitStreamMock {
                    calls: Arc::clone(&calls),
                    fail_first: 2,
                    status: 429,
                    retry_after_ms: None,
                    success_text: "recovered after backoff",
                }),
            )],
            3,
            1,
        );

        let messages = vec![ChatMessage::user("hi")];
        let started = std::time::Instant::now();
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, enabled_options());
        let (text, err) = collect_stream(s).await;

        assert!(err.is_none(), "single-candidate 429 must recover, got {err:?}");
        assert_eq!(text, "recovered after backoff");
        assert_eq!(calls.load(Ordering::SeqCst), 3, "two retries then success");
        assert!(
            started.elapsed() >= Duration::from_millis(35),
            "retries must actually back off, not spin: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn streaming_business_429_still_fails_fast() {
        // Counterweight to the previous test: quota/plan 429s never clear, so
        // they must not consume the (now unconditional) retry budget.
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![(
                "only".into(),
                Box::new(BusinessRateLimitStreamMock {
                    calls: Arc::clone(&calls),
                }),
            )],
            3,
            50,
        );

        let messages = vec![ChatMessage::user("hi")];
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, enabled_options());
        let (text, err) = collect_stream(s).await;

        assert!(text.is_empty(), "expected an empty value");
        assert!(err.is_some(), "business 429 must surface");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "an insufficient-balance 429 must not be retried"
        );
    }

    /// Streaming mock that always answers with a structured business 429
    /// (quota exhausted), the shape that retries can never fix.
    struct BusinessRateLimitStreamMock {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for BusinessRateLimitStreamMock {
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
            stream::iter(vec![Err(StreamError::RateLimited {
                status: 429,
                retry_after_ms: Some(10),
                message: "{\"code\":1113,\"message\":\"insufficient balance\"}".to_string(),
            })])
            .boxed()
        }
    }

    /// Streaming mock that first emits a content chunk (a visible-text delta, a
    /// reasoning-only delta, or a tool-call chunk — selectable) and *then* yields
    /// an error. Records every call so a test can prove the next candidate was
    /// never reached. FIX-P0-33: once any chunk is forwarded downstream the
    /// driver must surface a mid-stream error as-is (no resume / no failover).
    struct PostContentErrorStreamMock {
        calls: Arc<AtomicUsize>,
        kind: PostContentKind,
        error: StreamError,
    }

    #[derive(Clone, Copy)]
    enum PostContentKind {
        Text,
        Reasoning,
        ToolCall,
    }

    #[async_trait]
    impl Provider for PostContentErrorStreamMock {
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
            let first = match self.kind {
                PostContentKind::Text => StreamChunk::delta("partial answer"),
                PostContentKind::Reasoning => StreamChunk::reasoning_delta("thinking..."),
                PostContentKind::ToolCall => StreamChunk::tool_call_chunk(Vec::new()),
            };
            let err = clone_stream_error(&self.error);
            stream::iter(vec![Ok(first), Err(err)]).boxed()
        }
    }

    /// `StreamError` is not `Clone` (it wraps non-clonable reqwest/io/serde
    /// errors). The variants the mocks build are simple data variants, so a
    /// shallow rebuild is sufficient for tests.
    fn clone_stream_error(err: &StreamError) -> StreamError {
        match err {
            StreamError::Provider(m) => StreamError::Provider(m.clone()),
            StreamError::InvalidSse(m) => StreamError::InvalidSse(m.clone()),
            StreamError::RateLimited {
                status,
                retry_after_ms,
                message,
            } => StreamError::RateLimited {
                status: *status,
                retry_after_ms: *retry_after_ms,
                message: message.clone(),
            },
            other => StreamError::Provider(format!("{other}")),
        }
    }

    #[tokio::test]
    async fn streaming_does_not_resume_after_text_content_emitted() {
        // FIX-P0-33 (design lock): once a visible-text chunk has been forwarded,
        // a subsequent pre-terminal error must be surfaced verbatim — the driver
        // must NOT switch to the next candidate (resuming would duplicate or
        // truncate the already-emitted text).
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(PostContentErrorStreamMock {
                        calls: Arc::clone(&primary_calls),
                        kind: PostContentKind::Text,
                        // A normally fallback-eligible error (rate limit) — proves
                        // the *content-emitted* guard, not the error class, blocks
                        // failover here.
                        error: StreamError::RateLimited {
                            status: 429,
                            retry_after_ms: Some(0),
                            message: "rate limited mid-stream".to_string(),
                        },
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(StreamMock {
                        calls: Arc::clone(&fallback_calls),
                        outcome: Ok("from fallback"),
                    }),
                ),
            ],
            2,
            1,
        );

        let messages = vec![ChatMessage::user("hi")];
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, enabled_options());
        let (text, err) = collect_stream(s).await;

        // The already-emitted content is delivered, then the error is surfaced.
        assert_eq!(text, "partial answer", "emitted content must reach the receiver");
        assert!(
            matches!(err, Some(StreamError::RateLimited { .. })),
            "mid-stream error must be surfaced verbatim, got {err:?}"
        );
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1, "primary called exactly once");
        assert_eq!(
            fallback_calls.load(Ordering::SeqCst),
            0,
            "fallback must never be reached after content was emitted (no resume)"
        );
    }

    #[tokio::test]
    async fn streaming_does_not_resume_after_reasoning_content_emitted() {
        // FIX-P0-33: `emitted_content` flips to true for ANY forwarded chunk,
        // including reasoning/thinking deltas. A failure after a reasoning chunk
        // must therefore also surface without failover.
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(PostContentErrorStreamMock {
                        calls: Arc::clone(&primary_calls),
                        kind: PostContentKind::Reasoning,
                        error: StreamError::Provider("500 transient after reasoning".to_string()),
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(StreamMock {
                        calls: Arc::clone(&fallback_calls),
                        outcome: Ok("from fallback"),
                    }),
                ),
            ],
            2,
            1,
        );

        let messages = vec![ChatMessage::user("hi")];
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, enabled_options());
        let (text, err) = collect_stream(s).await;

        // Reasoning carries no visible delta, so `text` stays empty, but the
        // failover is still suppressed because content (reasoning) was emitted.
        assert!(text.is_empty(), "reasoning chunk carries no visible delta");
        assert!(
            matches!(err, Some(StreamError::Provider(_))),
            "error after reasoning chunk must be surfaced, got {err:?}"
        );
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fallback_calls.load(Ordering::SeqCst),
            0,
            "a reasoning chunk counts as emitted content → no failover"
        );
    }

    #[tokio::test]
    async fn streaming_does_not_resume_after_tool_call_chunk_emitted() {
        // FIX-P0-33: a tool-call chunk also counts as emitted content. A failure
        // afterward must surface without rotating providers.
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(PostContentErrorStreamMock {
                        calls: Arc::clone(&primary_calls),
                        kind: PostContentKind::ToolCall,
                        error: StreamError::Provider("503 transient after tool_call".to_string()),
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(StreamMock {
                        calls: Arc::clone(&fallback_calls),
                        outcome: Ok("from fallback"),
                    }),
                ),
            ],
            2,
            1,
        );

        let messages = vec![ChatMessage::user("hi")];
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, enabled_options());
        let (_text, err) = collect_stream(s).await;

        assert!(
            matches!(err, Some(StreamError::Provider(_))),
            "error after tool-call chunk must be surfaced, got {err:?}"
        );
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fallback_calls.load(Ordering::SeqCst),
            0,
            "a tool-call chunk counts as emitted content → no failover"
        );
    }

    #[tokio::test]
    async fn streaming_exhausts_retry_after_budget_before_falling_back() {
        // FIX-P0-33 (#4): a primary that keeps returning a structured Retry-After
        // beyond the retry budget must be tried `max_retries + 1` times (initial +
        // each honored retry) before the driver rotates to the next candidate.
        let max_retries = 2u32;
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(StructuredRateLimitStreamMock {
                        calls: Arc::clone(&primary_calls),
                        // Fail more times than the budget so retries are exhausted.
                        fail_first: (max_retries as usize) + 5,
                        status: 429,
                        retry_after_ms: Some(0), // Honor, but stay fast.
                        success_text: "never reached",
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(StreamMock {
                        calls: Arc::clone(&fallback_calls),
                        outcome: Ok("served by fallback"),
                    }),
                ),
            ],
            max_retries,
            1,
        );

        let messages = vec![ChatMessage::user("hi")];
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, enabled_options());
        let (text, err) = collect_stream(s).await;

        assert!(
            err.is_none(),
            "fallback should serve after retries exhausted, got {err:?}"
        );
        assert_eq!(text, "served by fallback");
        assert_eq!(
            primary_calls.load(Ordering::SeqCst),
            (max_retries as usize) + 1,
            "primary must be tried initial + max_retries times before fallback"
        );
        assert_eq!(
            fallback_calls.load(Ordering::SeqCst),
            1,
            "fallback runs exactly once after the primary's retry budget is spent"
        );
    }

    #[tokio::test]
    async fn streaming_interrupts_retry_after_sleep_when_receiver_dropped() {
        // FIX-P0-33 (#5): the Retry-After backoff sleep is interruptible — if the
        // stream receiver is dropped while the driver is waiting out the
        // Retry-After, the task must bail out promptly via the `tx.closed()` arm
        // of its `tokio::select!` rather than waiting out the full delay for
        // nobody.
        //
        // Discriminator without faking the clock: `fail_first = 1` means a
        // *completed* sleep would trigger exactly one same-provider RETRY (a
        // second call). We pick a Retry-After (700ms) long enough to observe, drop
        // the receiver while the driver is parked in the sleep, then wait WELL
        // PAST the Retry-After. If the sleep were not interruptible the retry
        // would have fired by then (calls == 2); with interruption the task exited
        // and `calls` stays at 1. The wait is bounded and small to keep the test
        // fast and non-flaky.
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let retry_after = Duration::from_millis(700);
        let provider = ReliableProvider::new(
            vec![(
                "primary".into(),
                Box::new(StructuredRateLimitStreamMock {
                    calls: Arc::clone(&primary_calls),
                    fail_first: 1,
                    status: 429,
                    retry_after_ms: Some(u64::try_from(retry_after.as_millis()).unwrap()),
                    success_text: "never reached",
                }),
            )],
            2,
            1,
        );

        let messages = vec![ChatMessage::user("hi")];
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, enabled_options());

        // Let the spawned driver run: it makes the first call, receives the 429,
        // and enters the interruptible sleep. A short real-time yield window well
        // under the 700ms Retry-After guarantees the initial call happened but the
        // retry has NOT yet fired.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            primary_calls.load(Ordering::SeqCst),
            1,
            "driver must have made the initial call and be parked in the Retry-After sleep"
        );

        // Drop the receiver mid-sleep → closes the channel → the `tx.closed()`
        // arm must win and the task must exit WITHOUT performing the retry.
        let dropped_at = std::time::Instant::now();
        drop(s);

        // Wait past the full Retry-After. An interruptible sleep means the retry
        // never fires; a blocking sleep would have resolved and bumped calls to 2.
        tokio::time::sleep(retry_after + Duration::from_millis(300)).await;
        assert_eq!(
            primary_calls.load(Ordering::SeqCst),
            1,
            "dropping the receiver must interrupt the Retry-After sleep before the same-provider retry fires"
        );
        // Sanity: we genuinely waited past the Retry-After window, so a
        // non-interrupted sleep would have had ample time to fire the retry.
        assert!(
            dropped_at.elapsed() > retry_after,
            "test must observe a window longer than the Retry-After to be conclusive"
        );
    }

    #[tokio::test]
    async fn streaming_floors_a_zero_retry_after_at_the_layer_backoff() {
        // The honored wait is `max(retry_after_ms, backoff)` plus jitter. With
        // retry_after_ms = 0 the floor is the layer backoff, so a 0ms hint must
        // still recover quickly via the same provider rather than hot-spinning.
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![
                (
                    "primary".into(),
                    Box::new(StructuredRateLimitStreamMock {
                        calls: Arc::clone(&primary_calls),
                        fail_first: 1,
                        status: 429,
                        retry_after_ms: Some(0), // Below the backoff floor.
                        success_text: "recovered after floored backoff",
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(StreamMock {
                        calls: Arc::clone(&fallback_calls),
                        outcome: Ok("from fallback"),
                    }),
                ),
            ],
            2,
            1, // base backoff = 50ms floor (clamped in `new`); used as the wait floor.
        );

        let messages = vec![ChatMessage::user("hi")];
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, enabled_options());
        let (text, err) = collect_stream(s).await;

        assert!(err.is_none(), "floored backoff must still recover, got {err:?}");
        assert_eq!(text, "recovered after floored backoff");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 2, "same provider retried once");
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn streaming_all_failed_surfaces_last_error_class_and_status() {
        // #7 (strengthened): when every candidate fails, the driver must surface
        // the LAST candidate's error (not the first), preserving its class/status
        // for the caller's diagnostics.
        let p1_calls = Arc::new(AtomicUsize::new(0));
        let p2_calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![
                (
                    "p1".into(),
                    Box::new(StreamMock {
                        calls: Arc::clone(&p1_calls),
                        outcome: Err("500 first candidate down"),
                    }),
                ),
                (
                    "p2".into(),
                    Box::new(StructuredRateLimitStreamMock {
                        calls: Arc::clone(&p2_calls),
                        // No retry-after hint → falls back/surfaces immediately as
                        // the terminal candidate.
                        fail_first: usize::MAX,
                        status: 503,
                        retry_after_ms: None,
                        success_text: "never",
                    }),
                ),
            ],
            0,
            1,
        );

        let messages = vec![ChatMessage::user("hi")];
        let s = provider.stream_chat_with_history(&messages, "test", 0.0, enabled_options());
        let (text, err) = collect_stream(s).await;

        assert!(text.is_empty(), "expected an empty value");
        match err {
            Some(StreamError::RateLimited { status, message, .. }) => {
                assert_eq!(status, 503, "must surface the LAST candidate's status, not the first");
                assert_eq!(message, "upstream overloaded");
            }
            other => panic!("expected the last candidate's RateLimited error, got {other:?}"),
        }
        assert_eq!(p1_calls.load(Ordering::SeqCst), 1, "first candidate tried once");
        assert_eq!(p2_calls.load(Ordering::SeqCst), 1, "last candidate tried once");
    }

    #[test]
    fn retry_after_ms_from_headers_parses_delta_seconds() {
        use crate::providers::traits::retry_after_ms_from_headers;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("12"),
        );
        assert_eq!(retry_after_ms_from_headers(&headers), Some(12_000));

        // Missing header → None.
        let empty = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_ms_from_headers(&empty), None);

        // HTTP-date form is not parsed here → None (falls back to layer backoff).
        let mut date_headers = reqwest::header::HeaderMap::new();
        date_headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("Wed, 21 Oct 2099 07:28:00 GMT"),
        );
        assert_eq!(retry_after_ms_from_headers(&date_headers), None);
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
        assert!(text.is_empty(), "expected an empty value");
        match err {
            Some(StreamError::Provider(msg)) => assert!(msg.contains("No provider supports streaming")),
            other => panic!("expected no-streaming provider error, got {other:?}"),
        }
    }

    /// C2b-1 / D-020 Q2: on the streaming path a tool-schema refusal arrives
    /// as the provider's own stream error, before any connection. It is
    /// deterministic, so the chain must not retry it against the same
    /// provider. Before: it classified as retryable and was retried with
    /// backoff.
    #[tokio::test]
    async fn a_streamed_tool_schema_refusal_is_not_retryable() {
        use crate::providers::compatible::{AuthStyle, OpenAiCompatibleProvider};
        let provider = OpenAiCompatibleProvider::new(
            "Fixture",
            "http://127.0.0.1:1/v1",
            Some("sk-fixture-not-real"),
            AuthStyle::Bearer,
        )
        .with_tool_schema_dialects(vec![(None, crate::tools::ToolSchemaDialect::NoRootCombinators)]);
        let tools = vec![crate::tools::ToolSpec {
            name: "fixture_alternatives".to_string(),
            description: "Needs a path or a key.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "key": {"type": "string"}},
                "anyOf": [{"required": ["path"]}, {"required": ["key"]}]
            }),
        }];
        let messages = vec![ChatMessage::user("hi")];
        let stream = provider.stream_chat_with_history(
            &messages,
            "fixture-model",
            0.0,
            StreamOptions::new(true).with_tools(tools),
        );
        let (_, error) = collect_stream(stream).await;
        let Some(error) = error else {
            panic!("test: the refused tool must end the stream with an error");
        };
        assert!(error.to_string().contains("fixture_alternatives"), "{error}");
        assert_eq!(classify_stream_error(&error), StreamFailureClass::NonRetryable);
    }

    /// COMPAT-STREAM (R-4): in production every provider sits behind
    /// `ReliableProvider`. When the consumer drops the stream (the idle guard
    /// ended the turn), the forwarding task must stop reading and drop the
    /// inner stream, so the provider's own release reaches the connection.
    async fn assert_reliable_releases(
        path: &'static str,
        first: &'static str,
        make: impl FnOnce(&str) -> Box<dyn Provider>,
    ) {
        use super::stream_release_probe::{spawn_silent_after_first_record_stub, wait_released};
        let (base, released, server) = spawn_silent_after_first_record_stub(path, first).await;
        let reliable = ReliableProvider::new(vec![("primary".into(), make(&base))], 0, 1);
        let messages = vec![ChatMessage::user("hi")];
        let mut stream = reliable.stream_chat_with_history(&messages, "test-model", 0.0, StreamOptions::new(true));
        let first_chunk = stream.next().await.expect("test: first chunk");
        assert!(first_chunk.is_ok(), "first chunk: {first_chunk:?}");
        drop(stream);
        let was_released = wait_released(&released).await;
        server.abort();
        assert!(
            was_released,
            "{path}: the connection must be released through ReliableProvider"
        );
    }

    #[tokio::test]
    async fn reliable_stream_releases_an_anthropic_connection_when_the_consumer_goes_away() {
        assert_reliable_releases(
            "/v1/messages",
            concat!(
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hel\"}}\n\n",
            ),
            |base| Box::new(super::super::anthropic::AnthropicProvider::with_base_url(Some("test-key"), Some(base))),
        )
        .await;
    }

    #[tokio::test]
    async fn reliable_stream_releases_an_openai_connection_when_the_consumer_goes_away() {
        assert_reliable_releases(
            "/chat/completions",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hel\"}}]}\n\n",
            |base| {
                Box::new(super::super::openai::OpenAiProvider::with_base_url(
                    Some(base),
                    Some("test-key"),
                ))
            },
        )
        .await;
    }

    #[tokio::test]
    async fn reliable_stream_releases_a_compatible_connection_when_the_consumer_goes_away() {
        assert_reliable_releases(
            "/chat/completions",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
            |base| {
                Box::new(super::super::compatible::OpenAiCompatibleProvider::new(
                    "custom",
                    base,
                    Some("test-key"),
                    super::super::compatible::AuthStyle::Bearer,
                ))
            },
        )
        .await;
    }

    /// COMPATf (N-3/N-4): a PRX-built content-filter stop skips only the
    /// in-place retry; its class still allows fallback. Upstream prose that
    /// merely mentions the phrase is not a filter stop.
    #[test]
    fn content_filter_stop_skips_only_the_in_place_retry() {
        let stop = content_filter_stop("OpenAI", "finish_reason content_filter");
        assert!(skips_in_place_retry(&stop));
        assert!(classify_stream_error(&stop).allows_fallback(), "fallback stays open");
        let structured = in_stream_error_object(
            "OpenAI-compatible",
            None,
            Some(serde_json::json!("content_filter")),
            Some("blocked".to_string()),
        );
        assert!(skips_in_place_retry(&structured));
        for mention in [
            StreamError::Provider("OpenAI stream error (server_error): last finish_reason content_filter".to_string()),
            StreamError::Provider("upstream said: content filter stop".to_string()),
        ] {
            assert!(!skips_in_place_retry(&mention), "{mention}");
        }
    }

    /// Minor: the in-stream error label is bounded and stripped to identifier
    /// characters; a string code loses its JSON quotes.
    #[test]
    fn in_stream_error_label_is_bounded() {
        let long = in_stream_error_object("OpenAI", Some("x".repeat(500) + "\n\u{1b}[31m"), None, None);
        let text = long.to_string();
        assert!(text.contains(&"x".repeat(64)), "{text}");
        assert!(!text.contains(&"x".repeat(65)), "{text}");
        assert!(!text.contains('\u{1b}'), "{text}");
        let coded = in_stream_error_object("OpenAI", None, Some(serde_json::json!("rate_limit")), None);
        assert!(coded.to_string().contains("(rate_limit)"), "{coded}");
    }

    #[tokio::test]
    async fn reliable_does_not_retry_a_content_filter_stop_before_content() {
        use axum::{Router, http::header, routing::post};
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let app = Router::new().route(
            "/chat/completions",
            post(move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        concat!(
                            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\n",
                            "data: [DONE]\n\n"
                        ),
                    )
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
        let reliable = ReliableProvider::new(
            vec![(
                "primary".into(),
                Box::new(super::super::openai::OpenAiProvider::with_base_url(
                    Some(&format!("http://{addr}")),
                    Some("test-key"),
                )),
            )],
            2,
            1,
        );
        let messages = vec![ChatMessage::user("hi")];
        let results: Vec<StreamResult<StreamChunk>> = reliable
            .stream_chat_with_history(&messages, "test-model", 0.0, StreamOptions::new(true))
            .collect()
            .await;
        server.abort();
        let error = results
            .iter()
            .find_map(|r| r.as_ref().err())
            .expect("test: the filter stop surfaces");
        assert!(error.to_string().contains("content_filter"), "got {error}");
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "no in-place retry of a deterministic filter stop"
        );
    }

    /// Loopback stub serving `body` as SSE on both OpenAI paths, counting hits.
    async fn spawn_counting_sse(body: &'static str) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        use axum::{Router, http::header, routing::post};
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let handler = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                ([(header::CONTENT_TYPE, "text/event-stream")], body)
            }
        };
        let app = Router::new().route("/chat/completions", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind loopback");
        let addr = listener.local_addr().expect("test: loopback addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), hits, server)
    }

    const FALLBACK_OK: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"fallback-ok\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );

    enum FirstWire {
        OpenAi,
        Compatible,
    }

    /// Stream once through `ReliableProvider(max_retries 2)` with `first`
    /// serving `body`, optionally with a compatible second candidate that
    /// answers "fallback-ok". Returns (first hits, second hits, text, error).
    async fn reliable_run(
        wire: FirstWire,
        body: &'static str,
        with_fallback: bool,
    ) -> (usize, usize, String, Option<String>) {
        let (b1, h1, s1) = spawn_counting_sse(body).await;
        let (b2, h2, s2) = spawn_counting_sse(FALLBACK_OK).await;
        let first: Box<dyn Provider> = match wire {
            FirstWire::OpenAi => Box::new(super::super::openai::OpenAiProvider::with_base_url(
                Some(&b1),
                Some("test-key"),
            )),
            FirstWire::Compatible => Box::new(super::super::compatible::OpenAiCompatibleProvider::new(
                "first",
                &b1,
                Some("test-key"),
                super::super::compatible::AuthStyle::Bearer,
            )),
        };
        let mut chain: Vec<(String, Box<dyn Provider>)> = vec![("first".into(), first)];
        if with_fallback {
            chain.push((
                "second".into(),
                Box::new(super::super::compatible::OpenAiCompatibleProvider::new(
                    "second",
                    &b2,
                    Some("test-key"),
                    super::super::compatible::AuthStyle::Bearer,
                )),
            ));
        }
        let reliable = ReliableProvider::new(chain, 2, 1);
        let messages = vec![ChatMessage::user("hi")];
        let items: Vec<StreamResult<StreamChunk>> = reliable
            .stream_chat_with_history(&messages, "test-model", 0.0, StreamOptions::new(true))
            .collect()
            .await;
        s1.abort();
        s2.abort();
        let text: String = items
            .iter()
            .filter_map(|i| i.as_ref().ok())
            .map(|c| c.delta.as_str())
            .collect();
        let error = items.iter().find_map(|i| i.as_ref().err()).map(ToString::to_string);
        (h1.load(Ordering::SeqCst), h2.load(Ordering::SeqCst), text, error)
    }

    /// COMPATf (N-3): a content-filter stop before any output skips only the
    /// in-place retry. Filters differ by provider, so the next candidate is
    /// still tried.
    #[tokio::test]
    async fn reliable_falls_back_after_a_content_filter_stop_before_content() {
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (first, second, text, error) = reliable_run(FirstWire::OpenAi, body, true).await;
        assert_eq!(first, 1, "no in-place retry of the filter stop");
        assert_eq!(second, 1, "the next candidate is tried");
        assert_eq!(text, "fallback-ok", "error: {error:?}");
        let (first, _, _, error) = reliable_run(FirstWire::OpenAi, body, false).await;
        assert_eq!(first, 1);
        assert!(error.is_some_and(|e| e.contains("content_filter")));
    }

    /// N-3/N-4: a structured in-stream error code `content_filter` is a filter
    /// stop too: one attempt, then fallback.
    #[tokio::test]
    async fn reliable_treats_an_in_stream_content_filter_error_code_as_a_filter_stop() {
        let body = "data: {\"error\":{\"code\":\"content_filter\",\"message\":\"blocked by policy\"}}\n\n";
        let (first, second, text, _) = reliable_run(FirstWire::Compatible, body, true).await;
        assert_eq!(first, 1, "no in-place retry");
        assert_eq!(second, 1);
        assert_eq!(text, "fallback-ok");
    }

    /// N-4: an upstream server error that merely mentions the phrase keeps its
    /// in-place retries.
    #[tokio::test]
    async fn reliable_keeps_retries_for_an_error_that_only_mentions_content_filter() {
        let body = "data: {\"error\":{\"message\":\"upstream timeout (last finish_reason content_filter)\",\"type\":\"server_error\"}}\n\n";
        let (first, _, _, error) = reliable_run(FirstWire::Compatible, body, false).await;
        assert_eq!(first, 3, "1 attempt + 2 in-place retries (error: {error:?})");
    }

    /// Minor: a tool-call index over the limit before any output is
    /// deterministic; skip the in-place retry, keep the fallback.
    #[tokio::test]
    async fn reliable_does_not_retry_a_tool_call_index_over_the_limit_in_place() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1000,\"id\":\"call_far\",",
            "\"function\":{\"name\":\"fixture_write\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (first, _, _, error) = reliable_run(FirstWire::Compatible, body, false).await;
        assert_eq!(first, 1, "no in-place retry (error: {error:?})");
        let (first, second, text, _) = reliable_run(FirstWire::Compatible, body, true).await;
        assert_eq!((first, second), (1, 1));
        assert_eq!(text, "fallback-ok");
    }

    /// COMPATf (N-2): streams abandoned before the server sent any headers
    /// must not leak. Behind `ReliableProvider`, the consumer drops each
    /// stream while the request is still waiting for a response; every
    /// connection must be closed.
    async fn assert_abandoned_before_headers_are_released(make: impl Fn(&str) -> Box<dyn Provider>) {
        use super::stream_release_probe::spawn_never_answering_server;
        const N: usize = 10;
        let (base, accepted, closed, server) = spawn_never_answering_server().await;
        let reliable = ReliableProvider::new(vec![("primary".into(), make(&base))], 0, 1);
        let messages = vec![ChatMessage::user("hi")];
        for _ in 0..N {
            let mut stream = reliable.stream_chat_with_history(&messages, "test-model", 0.0, StreamOptions::new(true));
            let pending = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
            assert!(pending.is_err(), "no headers were ever sent, so nothing can arrive");
            drop(stream);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while closed.load(Ordering::SeqCst) < N && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        server.abort();
        assert_eq!(accepted.load(Ordering::SeqCst), N, "every request reached the server");
        assert_eq!(closed.load(Ordering::SeqCst), N, "every abandoned connection is closed");
    }

    #[tokio::test]
    async fn reliable_releases_anthropic_streams_abandoned_before_headers() {
        assert_abandoned_before_headers_are_released(|base| {
            Box::new(super::super::anthropic::AnthropicProvider::with_base_url(
                Some("test-key"),
                Some(base),
            ))
        })
        .await;
    }

    #[tokio::test]
    async fn reliable_releases_openai_streams_abandoned_before_headers() {
        assert_abandoned_before_headers_are_released(|base| {
            Box::new(super::super::openai::OpenAiProvider::with_base_url(
                Some(base),
                Some("test-key"),
            ))
        })
        .await;
    }

    #[tokio::test]
    async fn reliable_releases_compatible_streams_abandoned_before_headers() {
        assert_abandoned_before_headers_are_released(|base| {
            Box::new(super::super::compatible::OpenAiCompatibleProvider::new(
                "custom",
                base,
                Some("test-key"),
                super::super::compatible::AuthStyle::Bearer,
            ))
        })
        .await;
    }

    // ── INT-2B a51 / COMPATf F-1: stream classification by PRX's own markers ──

    /// SP-INT2 a51: an upstream body that merely quotes the refusal marker is
    /// not a refusal. Before, any stream error containing
    /// `capability=tool_schema_dialect` was non-retryable, so a 503 whose
    /// body quoted it never failed over.
    #[test]
    fn upstream_text_quoting_the_refusal_marker_is_classified_as_upstream_text() {
        let quoted = StreamError::Provider(
            "OpenAI-compatible API error (503 Service Unavailable): proxy says \
             provider_capability_error provider=x capability=tool_schema_dialect message=nope"
                .to_string(),
        );
        assert_eq!(
            classify_stream_error(&quoted),
            StreamFailureClass::Retryable,
            "a transient upstream status must decide, not a quoted marker"
        );
        let real = StreamError::Provider(
            "provider_capability_error provider=Custom capability=tool_schema_dialect message=tool_schema_dialect \
             refused 1 tool(s)"
                .to_string(),
        );
        assert_eq!(
            classify_stream_error(&real),
            StreamFailureClass::NonRetryable,
            "PRX's own refusal is still deterministic"
        );
    }

    /// A stream mock that fails before content with the error `make` builds,
    /// counting its calls.
    struct PreContentErrorStreamMock {
        calls: Arc<AtomicUsize>,
        make: fn() -> StreamError,
    }

    #[async_trait]
    impl Provider for PreContentErrorStreamMock {
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
            stream::iter(vec![Err((self.make)())]).boxed()
        }
    }

    /// Run a two-candidate stream whose primary fails with `make`, and return
    /// (text, primary calls, fallback calls).
    async fn stream_with_failing_primary(primary: &str, make: fn() -> StreamError) -> (String, usize, usize) {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let provider = ReliableProvider::new(
            vec![
                (
                    primary.to_string(),
                    Box::new(PreContentErrorStreamMock {
                        calls: Arc::clone(&primary_calls),
                        make,
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(StreamMock {
                        calls: Arc::clone(&fallback_calls),
                        outcome: Ok("served by fallback"),
                    }),
                ),
            ],
            2,
            1,
        );
        let messages = vec![ChatMessage::user("hi")];
        let (text, _error) =
            collect_stream(provider.stream_chat_with_history(&messages, "test-model", 0.0, enabled_options())).await;
        (
            text,
            primary_calls.load(Ordering::SeqCst),
            fallback_calls.load(Ordering::SeqCst),
        )
    }

    /// COMPATf F-1 (RC11): a structured content-filter stop whose upstream
    /// message quotes `(403)` still falls back. Before, the prose heuristics
    /// read the quoted status and gave up on the whole chain.
    #[tokio::test]
    async fn a_content_filter_stop_quoting_a_status_still_falls_back() {
        fn make() -> StreamError {
            in_stream_error_object(
                "OpenAI-compatible",
                None,
                Some(serde_json::json!("content_filter")),
                Some("request blocked by policy (403) invalid model output".to_string()),
            )
        }
        let (text, primary, fallback) = stream_with_failing_primary("rc11-primary", make).await;
        assert_eq!(text, "served by fallback");
        assert_eq!(primary, 1, "a filter stop is not retried in place");
        assert_eq!(fallback, 1);
    }

    /// COMPATf F-1 (RC16): a tool-call index of 404 is not an HTTP status.
    #[tokio::test]
    async fn a_tool_call_index_of_404_still_falls_back() {
        fn make() -> StreamError {
            tool_call_index_over_limit("OpenAI-compatible", 404, 64)
        }
        let (text, primary, fallback) = stream_with_failing_primary("rc16-primary", make).await;
        assert_eq!(text, "served by fallback");
        assert_eq!(primary, 1, "an index over the limit is not retried in place");
        assert_eq!(fallback, 1);
    }

    /// COMPATf F-1 (RC17): a tool-call index of 429 is not a rate limit, so
    /// it records no rate-limit telemetry for the provider.
    #[tokio::test]
    async fn a_tool_call_index_of_429_is_not_a_rate_limit() {
        fn make() -> StreamError {
            tool_call_index_over_limit("OpenAI-compatible", 429, 64)
        }
        let (text, primary, fallback) = stream_with_failing_primary("rc17-primary", make).await;
        assert_eq!(text, "served by fallback");
        assert_eq!(primary, 1);
        assert_eq!(fallback, 1);
        let stats = rate_limit::snapshot();
        assert!(
            stats
                .providers
                .get("rc17-primary")
                .is_none_or(|entry| entry.rate_limited_total == 0),
            "an index over the limit was counted as a rate limit: {:?}",
            stats.providers.get("rc17-primary")
        );
    }

    // ── INT-2B B3f / D-068 (3): a streamed refusal fails over ──

    /// A tool whose root `anyOf` a `no_root_combinators` endpoint cannot take.
    fn root_any_of_tool() -> crate::tools::ToolSpec {
        crate::tools::ToolSpec {
            name: "fixture_alternatives".to_string(),
            description: "Needs a path or a key.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "key": {"type": "string"}},
                "anyOf": [{"required": ["path"]}, {"required": ["key"]}]
            }),
        }
    }

    /// D-068 (3): a streamed tool-schema refusal fails over exactly like the
    /// buffered one (D-020 Q2). The refusing endpoint declares
    /// `no_root_combinators` and refuses before transport; the fallback, a
    /// loopback endpoint that accepts the schema raw, serves the turn. Before,
    /// the refusal was `NonRetryable`, which never falls back, so the turn
    /// ended on it.
    #[tokio::test]
    async fn a_streamed_tool_schema_refusal_fails_over_before_any_output() {
        use crate::providers::compatible::{AuthStyle, OpenAiCompatibleProvider};
        let (refusing_base, refusing_hits, refusing_server) = spawn_counting_sse(FALLBACK_OK).await;
        let (fallback_base, fallback_hits, fallback_server) = spawn_counting_sse(FALLBACK_OK).await;
        let refusing = OpenAiCompatibleProvider::new("Refusing", &refusing_base, Some("test-key"), AuthStyle::Bearer)
            .with_tool_schema_dialects(vec![(None, crate::tools::ToolSchemaDialect::NoRootCombinators)]);
        let fallback = OpenAiCompatibleProvider::new("Fallback", &fallback_base, Some("test-key"), AuthStyle::Bearer);
        let chain = ReliableProvider::new(
            vec![
                ("refusing".into(), Box::new(refusing)),
                ("fallback".into(), Box::new(fallback)),
            ],
            2,
            1,
        );
        let messages = vec![ChatMessage::user("hi")];
        let (text, error) = collect_stream(chain.stream_chat_with_history(
            &messages,
            "fixture-model",
            0.0,
            StreamOptions::new(true).with_tools(vec![root_any_of_tool()]),
        ))
        .await;
        refusing_server.abort();
        fallback_server.abort();
        assert!(error.is_none(), "the fallback must serve the turn: {error:?}");
        assert_eq!(text, "fallback-ok");
        assert_eq!(refusing_hits.load(Ordering::SeqCst), 0, "a refusal sends nothing");
        assert_eq!(fallback_hits.load(Ordering::SeqCst), 1);
    }

    /// A member that emits output and then fails with a tool-schema refusal.
    struct RefusalAfterOutputMock;

    #[async_trait]
    impl Provider for RefusalAfterOutputMock {
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
            stream::iter(vec![
                Ok(StreamChunk::delta("partial")),
                Err(StreamError::Provider(
                    "provider_capability_error provider=Fixture capability=tool_schema_dialect \
                     message=tool_schema_dialect refused 1 tool(s)"
                        .to_string(),
                )),
            ])
            .boxed()
        }
    }

    /// D-068 (3): once output has been emitted, a refusal is surfaced as the
    /// error: the loopback fallback is never contacted, because switching
    /// providers mid-output would corrupt the response.
    #[tokio::test]
    async fn a_streamed_tool_schema_refusal_after_output_is_surfaced() {
        use crate::providers::compatible::{AuthStyle, OpenAiCompatibleProvider};
        let (fallback_base, fallback_hits, fallback_server) = spawn_counting_sse(FALLBACK_OK).await;
        let fallback = OpenAiCompatibleProvider::new("Fallback", &fallback_base, Some("test-key"), AuthStyle::Bearer);
        let chain = ReliableProvider::new(
            vec![
                ("partial".into(), Box::new(RefusalAfterOutputMock)),
                ("fallback".into(), Box::new(fallback)),
            ],
            2,
            1,
        );
        let messages = vec![ChatMessage::user("hi")];
        let (text, error) = collect_stream(chain.stream_chat_with_history(
            &messages,
            "fixture-model",
            0.0,
            StreamOptions::new(true).with_tools(vec![root_any_of_tool()]),
        ))
        .await;
        fallback_server.abort();
        assert_eq!(text, "partial");
        let error = error.expect("test: the refusal is surfaced");
        assert!(stream_error_is_tool_schema_refusal(&error), "{error}");
        assert_eq!(fallback_hits.load(Ordering::SeqCst), 0, "no failover after output");
    }

    // ── INT-2B B3f / D-069 L-1: a refusal is the chain's answer only when
    //    every tried candidate refused ──

    /// Stream through a two-member chain of a refusing endpoint and a
    /// loopback endpoint answering 503, in the given order, and return the
    /// surfaced error.
    async fn stream_refusal_and_503(refusal_first: bool) -> StreamError {
        use crate::providers::compatible::{AuthStyle, OpenAiCompatibleProvider};
        let unavailable = crate::providers::label_redaction_tests::spawn_fixed_status_stub(
            "503 Service Unavailable",
            r#"{"error":{"message":"overloaded","type":"server_error"}}"#,
        )
        .await;
        let refusing =
            OpenAiCompatibleProvider::new("Refusing", "http://127.0.0.1:1", Some("test-key"), AuthStyle::Bearer)
                .with_tool_schema_dialects(vec![(None, crate::tools::ToolSchemaDialect::NoRootCombinators)]);
        let overloaded = OpenAiCompatibleProvider::new(
            "Overloaded",
            &format!("http://{}", unavailable.addr),
            Some("test-key"),
            AuthStyle::Bearer,
        );
        let refusing: (String, Box<dyn Provider>) = ("refusing".into(), Box::new(refusing));
        let overloaded: (String, Box<dyn Provider>) = ("overloaded".into(), Box::new(overloaded));
        let members = if refusal_first {
            vec![refusing, overloaded]
        } else {
            vec![overloaded, refusing]
        };
        let chain = ReliableProvider::new(members, 0, 1);
        let messages = vec![ChatMessage::user("hi")];
        let (_text, error) = collect_stream(chain.stream_chat_with_history(
            &messages,
            "fixture-model",
            0.0,
            StreamOptions::new(true).with_tools(vec![root_any_of_tool()]),
        ))
        .await;
        assert!(unavailable.hits.load(Ordering::SeqCst) >= 1, "the 503 member was tried");
        error.expect("test: neither member serves the turn")
    }

    /// D-069 L-1, streaming, refusal then 503: the 503 is surfaced.
    #[tokio::test]
    async fn a_streamed_refusal_then_503_surfaces_the_503() {
        let error = stream_refusal_and_503(true).await;
        assert!(!stream_error_is_tool_schema_refusal(&error), "{error}");
        assert_eq!(classify_stream_error(&error), StreamFailureClass::Retryable, "{error}");
    }

    /// D-069 L-1, streaming, 503 then refusal: the 503 is still surfaced,
    /// because sending again may succeed once the first member recovers.
    /// Before, the last failure (the refusal) was surfaced.
    #[tokio::test]
    async fn a_streamed_503_then_refusal_surfaces_the_503() {
        let error = stream_refusal_and_503(false).await;
        assert!(!stream_error_is_tool_schema_refusal(&error), "{error}");
        assert_eq!(classify_stream_error(&error), StreamFailureClass::Retryable, "{error}");
    }

    // ── INT-2B B3f / D-069 L-3: the member key stays in-process ──

    /// D-069 L-3: a gate deferral of a member whose label carries a password
    /// is counted under the member's display name. It was counted under the
    /// salted member key, which reached `rate_limit::snapshot()`, the health
    /// snapshot and `daemon_state.json`, and changed on every restart.
    #[tokio::test]
    async fn a_gate_deferral_is_counted_under_the_display_name_not_the_member_key() {
        let label = "custom:http://alice:fixture-pw-l3@127.0.0.1:47/v1";
        let display: Arc<str> = Arc::from(super::super::redact_endpoint_userinfo(label));
        let key = member_key(label, &display);
        assert_ne!(key, display, "test: a label with a password has its own key");
        let gate = Arc::new(RateLimitGate::new());
        gate.note_rate_limited(&key, 60);
        let chain = ReliableProvider::new(
            vec![(
                label.to_string(),
                Box::new(MockProvider {
                    calls: Arc::new(AtomicUsize::new(0)),
                    fail_until_attempt: 0,
                    response: "ok",
                    error: "unused",
                }),
            )],
            0,
            1,
        )
        .with_rate_limit_gate(Arc::clone(&gate));

        let reply = chain.chat_with_system(None, "hi", "fixture-model", 0.0).await;
        assert_eq!(reply.ok().as_deref(), Some("ok"));

        let stats = rate_limit::snapshot();
        assert!(
            stats
                .providers
                .get(&*display)
                .is_some_and(|entry| entry.gate_deferrals_total >= 1),
            "the deferral must be counted under the display name: {:?}",
            stats.providers.keys().collect::<Vec<_>>()
        );
        assert!(
            !stats.providers.contains_key(&*key),
            "the member key reached the telemetry"
        );
        let health = crate::health::snapshot_json().to_string();
        assert!(!health.contains(&*key), "the member key reached the health snapshot");
        assert!(!health.contains("fixture-pw-l3"));
    }

    // ── INT-2B B3f / D-068 (2): the served member, by its unique key ──

    /// A chain member with its own tool-schema dialect that fails (503) or
    /// serves, on both the buffered and the streaming path.
    struct DialectMemberMock {
        dialect: crate::tools::ToolSchemaDialect,
        fails: bool,
    }

    #[async_trait]
    impl Provider for DialectMemberMock {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            if self.fails {
                anyhow::bail!("fixture API error (503 Service Unavailable): try later");
            }
            Ok("served".to_string())
        }

        async fn chat_with_history(
            &self,
            _messages: &[ChatMessage],
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            if self.fails {
                anyhow::bail!("fixture API error (503 Service Unavailable): try later");
            }
            Ok("served".to_string())
        }

        fn tool_schema_dialect_for(&self, _model: &str) -> crate::tools::ToolSchemaDialect {
            self.dialect
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
            if self.fails {
                stream::iter(vec![Err(StreamError::Provider(
                    "fixture API error (503 Service Unavailable): try later".to_string(),
                ))])
                .boxed()
            } else {
                stream::iter(vec![Ok(StreamChunk::delta("served")), Ok(StreamChunk::final_chunk())]).boxed()
            }
        }
    }

    /// Two members whose labels differ only in their password: the first
    /// (raw) fails, the second (Gemini) serves.
    fn members_differing_only_in_password() -> ReliableProvider {
        ReliableProvider::new(
            vec![
                (
                    "custom:http://a:fixture-pw-a@127.0.0.1:9/v1".to_string(),
                    Box::new(DialectMemberMock {
                        dialect: crate::tools::ToolSchemaDialect::Raw,
                        fails: true,
                    }),
                ),
                (
                    "custom:http://b:fixture-pw-b@127.0.0.1:9/v1".to_string(),
                    Box::new(DialectMemberMock {
                        dialect: crate::tools::ToolSchemaDialect::Gemini,
                        fails: false,
                    }),
                ),
            ],
            0,
            1,
        )
    }

    /// D-068 (2): the dialect reported for the served member is the served
    /// member's, even when its display name equals another member's. Before,
    /// the member was looked up by its redacted display name, which both
    /// members share, so the first member's dialect (`Raw`) was reported.
    #[tokio::test]
    async fn the_served_member_is_identified_by_its_key_not_its_redacted_label() {
        let chain = members_differing_only_in_password();
        let messages = [ChatMessage::user("hi")];
        let trace = chain
            .chat_traced(
                ChatRequest {
                    messages: &messages,
                    tools: None,
                },
                "fixture-model",
                0.0,
            )
            .await
            .expect("test: the second member serves");
        assert_eq!(trace.response.text.as_deref(), Some("served"));
        assert!(!trace.final_provider.contains("fixture-pw"), "{}", trace.final_provider);
        let member = trace
            .served_member
            .as_deref()
            .expect("test: the chain names its member");
        assert!(
            !member.contains("fixture-pw"),
            "the member key carries no password: {member}"
        );
        assert_eq!(
            chain.tool_schema_dialect_of_member(member, &trace.final_model),
            Some(crate::tools::ToolSchemaDialect::Gemini)
        );
    }

    /// D-068 (2), streaming: the chunk that reports the successful attempt
    /// names the served member by its key too.
    #[tokio::test]
    async fn a_streamed_turn_names_its_served_member_by_key() {
        let chain = members_differing_only_in_password();
        let messages = vec![ChatMessage::user("hi")];
        let chunks: Vec<StreamResult<StreamChunk>> = chain
            .stream_chat_with_history(&messages, "fixture-model", 0.0, enabled_options())
            .collect()
            .await;
        let member = chunks
            .iter()
            .filter_map(|chunk| chunk.as_ref().ok())
            .find_map(|chunk| chunk.served_member.clone())
            .expect("test: the success chunk names the served member");
        assert_eq!(
            chain.tool_schema_dialect_of_member(&member, "fixture-model"),
            Some(crate::tools::ToolSchemaDialect::Gemini)
        );
    }
}

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

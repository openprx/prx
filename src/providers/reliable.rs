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
    if crate::recovery::overflow::is_context_overflow_error(err) || is_tool_schema_refusal(err) {
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

                                if crate::recovery::overflow::is_context_overflow_error(&e) {
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

                                if crate::recovery::overflow::is_context_overflow_error(&e) {
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

                                if crate::recovery::overflow::is_context_overflow_error(&e) {
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

    if crate::recovery::overflow::is_context_overflow_error(&anyhow_err) {
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

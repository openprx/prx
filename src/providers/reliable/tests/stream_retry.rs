#![cfg(test)]
use super::*;
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
    let err = anyhow::Error::new(crate::providers::rate_limit::RetryAfterHint { millis: 45_000 })
        .context("Anthropic API error (429 Too Many Requests): rate_limit_error");
    assert_eq!(parse_retry_after_ms(&err), Some(45_000));
    let RetryWait::Sleep(wait) = plan_retry_wait(1_000, &err) else {
        panic!("45s is within the honored ceiling");
    };
    assert!((45_000..=50_000).contains(&wait), "unexpected wait {wait}");
}

#[test]
fn retry_plan_floors_a_zero_hint_at_the_local_backoff() {
    let err = anyhow::Error::new(crate::providers::rate_limit::RetryAfterHint { millis: 0 })
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

pub(super) fn counting_mock(calls: &Arc<AtomicUsize>, fail: bool, response: &'static str) -> Box<dyn Provider> {
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

pub(super) fn openai_then_llamacpp(
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

pub(super) use crate::providers::traits::{StreamChunk, StreamError, StreamOptions, StreamResult};
pub(super) use futures_util::stream;

pub(super) fn text_chunk(delta: &str, _last: bool) -> StreamChunk {
    // The fallback driver keys off content presence and Ok/Err, not the
    // finality flag, so a plain visible-text delta is sufficient here.
    StreamChunk::delta(delta)
}

/// Streaming mock. `outcome` decides what the stream yields:
/// - `Ok(text)`     → a single content chunk then end.
/// - `Err(message)` → an immediate pre-content error chunk.
pub(super) struct StreamMock {
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) outcome: Result<&'static str, &'static str>,
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

pub(super) async fn collect_stream(
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

pub(super) fn enabled_options() -> StreamOptions {
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
pub(super) struct RetryAfterStreamMock {
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) fail_first: usize,
    pub(super) retry_after_error: &'static str,
    pub(super) success_text: &'static str,
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
pub(super) struct StructuredRateLimitStreamMock {
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) fail_first: usize,
    pub(super) status: u16,
    pub(super) retry_after_ms: Option<u64>,
    pub(super) success_text: &'static str,
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
pub(super) struct BusinessRateLimitStreamMock {
    pub(super) calls: Arc<AtomicUsize>,
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
pub(super) struct PostContentErrorStreamMock {
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) kind: PostContentKind,
    pub(super) error: StreamError,
}

#[derive(Clone, Copy)]
pub(super) enum PostContentKind {
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
pub(super) fn clone_stream_error(err: &StreamError) -> StreamError {
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

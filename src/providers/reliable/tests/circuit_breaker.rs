//! Real reliability-loop admission and endpoint-health classification.
use super::*;
use crate::providers::circuit_breaker::{BreakerParams, CircuitBreakerRegistry};
use tokio::time::{Instant, advance};

const fn params(threshold: u32) -> BreakerParams {
    BreakerParams {
        failure_threshold: threshold,
        open_for: Duration::from_secs(30),
        max_open_for: Duration::from_secs(300),
    }
}
fn request(messages: &[ChatMessage]) -> ChatRequest<'_> {
    ChatRequest { messages, tools: None }
}
fn mock(calls: &Arc<AtomicUsize>, failures: usize) -> Box<dyn Provider> {
    Box::new(MockProvider {
        calls: Arc::clone(calls),
        fail_until_attempt: failures,
        response: "ok",
        error: "503 unavailable",
    })
}
fn chain(
    registry: &Arc<CircuitBreakerRegistry>,
    calls: &Arc<AtomicUsize>,
    failures: usize,
    retries: u32,
    threshold: u32,
) -> ReliableProvider {
    ReliableProvider::new(vec![("prx_primary".into(), mock(calls, failures))], retries, 50)
        .with_circuit_breaker(Arc::clone(registry), params(threshold))
}
async fn call(chain: &ReliableProvider, mode: &str) -> anyhow::Result<String> {
    let messages = [ChatMessage::user("ping")];
    match mode {
        "system" => chain.chat_with_system(None, "ping", "model", 0.0).await,
        "history" => chain.chat_with_history(&messages, "model", 0.0).await,
        "tools" => chain
            .chat_with_tools(&messages, &[], "model", 0.0)
            .await
            .map(|r| r.text.unwrap_or_default()),
        _ => chain.chat_traced(request(&messages), "model", 0.0).await.map(|t| {
            assert_eq!(t.final_provider, "prx_fallback");
            assert_eq!(
                t.attempts.len(),
                if t.attempts.first().is_some_and(|a| a.provider == "prx_primary") {
                    4
                } else {
                    1
                }
            );
            t.response.text.unwrap_or_default()
        }),
    }
}
async fn fallback_case(mode: &str) {
    let registry = Arc::new(CircuitBreakerRegistry::default());
    let primary = Arc::new(AtomicUsize::new(0));
    let fallback = Arc::new(AtomicUsize::new(0));
    let provider = ReliableProvider::new(
        vec![
            ("prx_primary".into(), mock(&primary, usize::MAX)),
            ("prx_fallback".into(), mock(&fallback, 0)),
        ],
        2,
        50,
    )
    .with_circuit_breaker(registry, params(3));
    assert_eq!(call(&provider, mode).await.unwrap(), "ok");
    assert_eq!(primary.load(Ordering::SeqCst), 3);
    assert_eq!(call(&provider, mode).await.unwrap(), "ok");
    assert_eq!(
        primary.load(Ordering::SeqCst),
        3,
        "open primary must receive zero new calls"
    );
    assert_eq!(fallback.load(Ordering::SeqCst), 2);
}
#[tokio::test(start_paused = true)]
async fn r3a_traced_fallback_skips_open_primary() {
    fallback_case("traced").await;
}
#[tokio::test(start_paused = true)]
async fn r3b_system_fallback_skips_open_primary() {
    fallback_case("system").await;
}
#[tokio::test(start_paused = true)]
async fn r3c_history_fallback_skips_open_primary() {
    fallback_case("history").await;
}
#[tokio::test(start_paused = true)]
async fn r3d_tools_fallback_skips_open_primary() {
    fallback_case("tools").await;
}
#[tokio::test(start_paused = true)]
async fn r3e_stream_fallback_skips_open_primary() {
    let registry = Arc::new(CircuitBreakerRegistry::default());
    let primary = Arc::new(AtomicUsize::new(0));
    let fallback = Arc::new(AtomicUsize::new(0));
    let provider = ReliableProvider::new(
        vec![
            (
                "prx_primary".into(),
                Box::new(StreamMock {
                    calls: Arc::clone(&primary),
                    outcome: Err("503 unavailable"),
                }),
            ),
            (
                "prx_fallback".into(),
                Box::new(StreamMock {
                    calls: Arc::clone(&fallback),
                    outcome: Ok("ok"),
                }),
            ),
        ],
        2,
        50,
    )
    .with_circuit_breaker(registry, params(3));
    for _ in 0..2 {
        let (text, err) = collect_stream(provider.stream_chat_with_history(
            &[ChatMessage::user("ping")],
            "model",
            0.0,
            enabled_options(),
        ))
        .await;
        assert!(err.is_none());
        assert_eq!(text, "ok");
    }
    assert_eq!(primary.load(Ordering::SeqCst), 3);
    assert_eq!(fallback.load(Ordering::SeqCst), 2);
}
#[tokio::test(start_paused = true)]
async fn r3fg_all_open_fails_fast_and_successful_probe_recovers() {
    let registry = Arc::new(CircuitBreakerRegistry::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = chain(&registry, &calls, 1, 0, 1);
    assert!(provider.chat_with_system(None, "ping", "model", 0.0).await.is_err());
    let started = Instant::now();
    let error = provider.chat_with_system(None, "ping", "model", 0.0).await.unwrap_err();
    assert!(error.to_string().contains("circuit open"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(started.elapsed() < Duration::from_millis(50));
    advance(Duration::from_secs(30)).await;
    assert_eq!(
        provider.chat_with_system(None, "ping", "model", 0.0).await.unwrap(),
        "ok"
    );
    assert_eq!(
        provider.chat_with_system(None, "ping", "model", 0.0).await.unwrap(),
        "ok"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}
#[tokio::test(start_paused = true)]
async fn r3i_each_retry_needs_fresh_admission() {
    let registry = Arc::new(CircuitBreakerRegistry::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = chain(&registry, &calls, usize::MAX, 4, 2);
    let messages = [ChatMessage::user("ping")];
    let error = provider
        .chat_traced(request(&messages), "model", 0.0)
        .await
        .unwrap_err();
    assert_eq!(
        registry.snapshot().get("prx_primary").unwrap().state,
        "open",
        "the failure outcomes must open the circuit"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "retries must stop at breaker threshold"
    );
    assert!(error.to_string().contains("circuit open"));
}
fn deferrals(name: &str) -> u64 {
    rate_limit::snapshot()
        .providers
        .get(name)
        .map_or(0, |r| r.gate_deferrals_total)
}
#[tokio::test(start_paused = true)]
async fn r3j_open_circuit_refuses_before_gate_wait() {
    let registry = Arc::new(CircuitBreakerRegistry::default());
    let gate = Arc::new(RateLimitGate::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let other_calls = Arc::new(AtomicUsize::new(0));
    let build = |calls: &Arc<AtomicUsize>| {
        ReliableProvider::new(vec![("prx_before_gate_r3j".into(), mock(calls, usize::MAX))], 0, 50)
            .with_circuit_breaker(Arc::clone(&registry), params(1))
    };
    let provider = build(&calls).with_rate_limit_gate(Arc::clone(&gate));
    let opener = build(&other_calls);
    assert!(opener.chat_with_system(None, "ping", "model", 0.0).await.is_err());
    gate.note_rate_limited("prx_before_gate_r3j", 60_000);
    let before = deferrals("prx_before_gate_r3j");
    let started = Instant::now();
    let messages = [ChatMessage::user("ping")];
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        provider.chat_traced(request(&messages), "model", 0.0),
    )
    .await;
    assert!(
        result.is_ok(),
        "open circuit must complete before the one-second timeout"
    );
    assert!(result.unwrap().unwrap_err().to_string().contains("circuit open"));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(deferrals("prx_before_gate_r3j"), before);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
#[tokio::test(start_paused = true)]
async fn r1k_gate_wait_revalidates_permit_before_transport() {
    let registry = Arc::new(CircuitBreakerRegistry::default());
    let gate = Arc::new(RateLimitGate::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = chain(&registry, &calls, 0, 0, 1).with_rate_limit_gate(Arc::clone(&gate));
    let key = Arc::<str>::from("prx_primary");
    gate.note_rate_limited(&key, 60_000);
    let task = tokio::spawn(async move {
        let messages = [ChatMessage::user("ping")];
        provider.chat_traced(request(&messages), "model", 0.0).await
    });
    // Polling the task reaches admission and the gate's first yielding sleep.
    tokio::task::yield_now().await;
    registry
        .admit(&key, params(1))
        .unwrap()
        .unwrap()
        .complete(BreakerOutcome::Failure);
    // The production gate uses std::time::Instant. Clear it via a test-only
    // hook, so this fixture never waits on wall time or changes gate semantics.
    gate.clear_for_circuit_test(&key);
    advance(Duration::from_secs(61)).await;
    let result = task.await.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "stale gate-waiting permit must not reach upstream"
    );
    assert!(result.unwrap_err().to_string().contains("circuit open"));
}

struct ErrorMock {
    error: parking_lot::Mutex<Option<anyhow::Error>>,
}
#[async_trait]
impl Provider for ErrorMock {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        Err(self.error.lock().take().unwrap())
    }
}
#[tokio::test(start_paused = true)]
async fn r2_buffered_failure_classes_count_only_endpoint_failures() {
    let mut cases: Vec<(&str, anyhow::Error, bool)> = [
        (500, "500"),
        (502, "502"),
        (503, "503"),
        (504, "504"),
        (408, "408"),
        (401, "401"),
        (400, "400"),
        (403, "403"),
        (404, "404"),
    ]
    .into_iter()
    .map(|(code, name)| (name, anyhow::anyhow!("HTTP {code} error"), code >= 500 || code == 408))
    .collect();
    cases.extend([
        (
            "io",
            std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset").into(),
            true,
        ),
        ("context", anyhow::anyhow!("maximum context length exceeded"), false),
        ("429", anyhow::anyhow!("429 Too Many Requests"), false),
        (
            "429 hint",
            anyhow::Error::new(crate::providers::rate_limit::RetryAfterHint { millis: 1 })
                .context("429 Too Many Requests"),
            false,
        ),
        ("business", anyhow::anyhow!("429 rate limit code=1311"), false),
    ]);
    cases.push((
        "schema refusal",
        crate::providers::ToolSchemaRefusalError {
            provider: "prx_classification".into(),
            refusals: crate::tools::ToolSchemaRefusals {
                dialect: crate::tools::ToolSchemaDialect::Raw,
                refusals: Vec::new(),
                provider: "prx_classification".into(),
                model: "model".into(),
                descriptor_source: "test".into(),
            },
        }
        .into(),
        false,
    ));
    // A real reqwest timeout from a loopback endpoint that never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let error = reqwest::Client::builder()
        .timeout(Duration::from_millis(10))
        .build()
        .unwrap()
        .get(format!("http://{address}"))
        .send()
        .await
        .unwrap_err();
    assert!(error.is_timeout());
    cases.push(("timeout", error.into(), true));
    server.abort();
    for (name, error, opens) in cases {
        let registry = Arc::new(CircuitBreakerRegistry::default());
        let provider = ReliableProvider::new(
            vec![(
                "prx_classification".into(),
                Box::new(ErrorMock {
                    error: parking_lot::Mutex::new(Some(error)),
                }),
            )],
            0,
            50,
        )
        .with_circuit_breaker(Arc::clone(&registry), params(1));
        assert!(provider.chat_with_system(None, "ping", "model", 0.0).await.is_err());
        assert_eq!(
            registry.snapshot().get("prx_classification").unwrap().state,
            if opens { "open" } else { "closed" },
            "classification row {name}"
        );
    }
}
#[tokio::test(start_paused = true)]
async fn r2_stream_content_filter_is_not_endpoint_failure() {
    let registry = Arc::new(CircuitBreakerRegistry::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = ReliableProvider::new(
        vec![(
            "prx_filter".into(),
            Box::new(StreamMock {
                calls,
                outcome: Err("content filter stop (prx): filtered"),
            }),
        )],
        0,
        50,
    )
    .with_circuit_breaker(Arc::clone(&registry), params(1));
    let (_, error) = collect_stream(provider.stream_chat_with_history(&[], "model", 0.0, enabled_options())).await;
    assert!(error.is_some());
    assert_eq!(registry.snapshot().get("prx_filter").unwrap().state, "closed");
}

#[tokio::test(start_paused = true)]
async fn circuit_open_error_redacts_configured_endpoint_credentials() {
    let credential = ["sk", "-live-SECRET"].concat();
    let label = format!("custom:https://user:{credential}@example.test/v1?key=QUERYSECRET");
    let registry = Arc::new(CircuitBreakerRegistry::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let provider =
        ReliableProvider::new(vec![(label, mock(&calls, usize::MAX))], 0, 50).with_circuit_breaker(registry, params(1));
    assert!(provider.chat_with_system(None, "ping", "model", 0.0).await.is_err());
    let error = provider
        .chat_with_system(None, "ping", "model", 0.0)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("circuit open"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    for secret in [credential.as_str(), "QUERYSECRET", "user:"] {
        assert!(!error.contains(secret), "circuit error leaked {secret}");
    }
}

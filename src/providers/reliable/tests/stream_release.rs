#![cfg(test)]
use super::*;
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
pub(super) async fn assert_reliable_releases(
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
        |base| Box::new(crate::providers::anthropic::AnthropicProvider::with_base_url(Some("test-key"), Some(base))),
    )
    .await;
}

#[tokio::test]
async fn reliable_stream_releases_an_openai_connection_when_the_consumer_goes_away() {
    assert_reliable_releases(
        "/chat/completions",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hel\"}}]}\n\n",
        |base| {
            Box::new(crate::providers::openai::OpenAiProvider::with_base_url(
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
            Box::new(crate::providers::compatible::OpenAiCompatibleProvider::new(
                "custom",
                base,
                Some("test-key"),
                crate::providers::compatible::AuthStyle::Bearer,
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
            Box::new(crate::providers::openai::OpenAiProvider::with_base_url(
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
pub(super) async fn spawn_counting_sse(body: &'static str) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
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

pub(super) const FALLBACK_OK: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"fallback-ok\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n"
);

pub(super) enum FirstWire {
    OpenAi,
    Compatible,
}

/// Stream once through `ReliableProvider(max_retries 2)` with `first`
/// serving `body`, optionally with a compatible second candidate that
/// answers "fallback-ok". Returns (first hits, second hits, text, error).
pub(super) async fn reliable_run(
    wire: FirstWire,
    body: &'static str,
    with_fallback: bool,
) -> (usize, usize, String, Option<String>) {
    let (b1, h1, s1) = spawn_counting_sse(body).await;
    let (b2, h2, s2) = spawn_counting_sse(FALLBACK_OK).await;
    let first: Box<dyn Provider> = match wire {
        FirstWire::OpenAi => Box::new(crate::providers::openai::OpenAiProvider::with_base_url(
            Some(&b1),
            Some("test-key"),
        )),
        FirstWire::Compatible => Box::new(crate::providers::compatible::OpenAiCompatibleProvider::new(
            "first",
            &b1,
            Some("test-key"),
            crate::providers::compatible::AuthStyle::Bearer,
        )),
    };
    let mut chain: Vec<(String, Box<dyn Provider>)> = vec![("first".into(), first)];
    if with_fallback {
        chain.push((
            "second".into(),
            Box::new(crate::providers::compatible::OpenAiCompatibleProvider::new(
                "second",
                &b2,
                Some("test-key"),
                crate::providers::compatible::AuthStyle::Bearer,
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
pub(super) async fn assert_abandoned_before_headers_are_released(make: impl Fn(&str) -> Box<dyn Provider>) {
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
        Box::new(crate::providers::anthropic::AnthropicProvider::with_base_url(
            Some("test-key"),
            Some(base),
        ))
    })
    .await;
}

#[tokio::test]
async fn reliable_releases_openai_streams_abandoned_before_headers() {
    assert_abandoned_before_headers_are_released(|base| {
        Box::new(crate::providers::openai::OpenAiProvider::with_base_url(
            Some(base),
            Some("test-key"),
        ))
    })
    .await;
}

#[tokio::test]
async fn reliable_releases_compatible_streams_abandoned_before_headers() {
    assert_abandoned_before_headers_are_released(|base| {
        Box::new(crate::providers::compatible::OpenAiCompatibleProvider::new(
            "custom",
            base,
            Some("test-key"),
            crate::providers::compatible::AuthStyle::Bearer,
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
pub(super) struct PreContentErrorStreamMock {
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) make: fn() -> StreamError,
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
pub(super) async fn stream_with_failing_primary(primary: &str, make: fn() -> StreamError) -> (String, usize, usize) {
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
pub(super) fn root_any_of_tool() -> crate::tools::ToolSpec {
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
pub(super) struct RefusalAfterOutputMock;

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
pub(super) async fn stream_refusal_and_503(refusal_first: bool) -> StreamError {
    use crate::providers::compatible::{AuthStyle, OpenAiCompatibleProvider};
    let unavailable = crate::providers::label_redaction_tests::spawn_fixed_status_stub(
        "503 Service Unavailable",
        r#"{"error":{"message":"overloaded","type":"server_error"}}"#,
    )
    .await;
    let refusing = OpenAiCompatibleProvider::new("Refusing", "http://127.0.0.1:1", Some("test-key"), AuthStyle::Bearer)
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
    let display: Arc<str> = Arc::from(crate::providers::redact_endpoint_userinfo(label));
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
pub(super) struct DialectMemberMock {
    pub(super) dialect: crate::tools::ToolSchemaDialect,
    pub(super) fails: bool,
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
pub(super) fn members_differing_only_in_password() -> ReliableProvider {
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

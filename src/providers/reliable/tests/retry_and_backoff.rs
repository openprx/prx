#![cfg(test)]
use super::*;

pub(super) struct MockProvider {
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) fail_until_attempt: usize,
    pub(super) response: &'static str,
    pub(super) error: &'static str,
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

pub(super) struct MeteredMockProvider {
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) fail_until_attempt: usize,
    pub(super) response: &'static str,
    pub(super) error: &'static str,
    pub(super) usage: crate::llm::route_decision::TokenUsage,
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
pub(super) struct ModelAwareMock {
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) models_seen: parking_lot::Mutex<Vec<String>>,
    pub(super) fail_models: Vec<&'static str>,
    pub(super) response: &'static str,
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

pub(super) struct NativeCapabilityMock {
    pub(super) native_tools: bool,
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
                    usage: crate::llm::route_decision::TokenUsage::reported(Some(10_000), Some(10_000), Some(20_000)),
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
pub(super) struct ProviderModelAwareMock {
    pub(super) provider_tag: &'static str,
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) models_seen: parking_lot::Mutex<Vec<String>>,
    /// `(provider_tag, model)` pairs that must fail.
    pub(super) fail_on: Vec<(&'static str, &'static str)>,
    pub(super) response: &'static str,
}

#[async_trait]
impl Provider for ProviderModelAwareMock {
    async fn chat(&self, _request: ChatRequest<'_>, model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
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
/// [`api_error`](crate::providers::api_error) builds: sanitized body text, plus
/// the structured `Retry-After` hint when the upstream sent that header.
pub(super) struct RateLimit429Mock {
    pub(super) calls: Arc<AtomicUsize>,
    /// Number of leading calls that fail; later calls succeed.
    pub(super) fail_until_attempt: usize,
    pub(super) retry_after_ms: Option<u64>,
    pub(super) body: &'static str,
}

impl RateLimit429Mock {
    pub(super) fn error(&self) -> anyhow::Error {
        let message = format!("Mock API error (429 Too Many Requests): {}", self.body);
        match self.retry_after_ms {
            Some(millis) => {
                anyhow::Error::new(crate::providers::rate_limit::RetryAfterHint { millis }).context(message)
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
pub(super) struct CallTimeRecorderMock {
    pub(super) origin: std::time::Instant,
    pub(super) offsets: Arc<parking_lot::Mutex<Vec<u128>>>,
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) fail_until_attempt: usize,
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

//! A tool-schema refusal is deterministic: one diagnostic that names every
//! refused tool and the configuration to add, no retry against the same
//! provider, failover to a provider that accepts the schema in its own
//! dialect, and — for an endpoint nobody declared — a hint naming the
//! declaration when the endpoint itself rejects a root combinator.
//!
//! Every test loads a `config.toml` from a temporary directory through
//! `Config::load_from_path`, builds the chain through the production factory
//! and talks to loopback stubs only.

#![allow(clippy::indexing_slicing)]

use super::create_resilient_provider_with_options;
use super::label_redaction_tests::spawn_fixed_status_stub;
use super::provider_runtime_options_from_config;
use super::tool_dialect_config_tests::{
    FIXTURE_MODEL, ROOT_COMBINATOR_REJECTION, alternatives_tool, load_config, mergeable_tool,
    no_root_combinators_declaration, root_combinators_in, spawn_dialect_stub,
};
use super::traits::{ChatMessage, ChatRequest};
use crate::tools::ToolSpec;
use serde_json::json;
use std::sync::atomic::Ordering;
use tempfile::TempDir;

/// A published contract whose root `allOf` carries a conditional
/// requirement, which no single object can express.
fn conditional_tool() -> ToolSpec {
    ToolSpec {
        name: "fixture_conditional".to_string(),
        description: "Needs a target when acting.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {"action": {"type": "string"}, "target": {"type": "string"}},
            "required": ["action"],
            "allOf": [{"if": {"properties": {"action": {"const": "act"}}}, "then": {"required": ["target"]}}]
        }),
    }
}

/// A config whose default provider is `provider`, with the given reliability
/// table body and extra top-level TOML.
fn config_with(provider: &str, reliability: &str, extra: &str) -> String {
    format!(
        "default_temperature = 0.0\ndefault_provider = \"{provider}\"\ndefault_model = \"{FIXTURE_MODEL}\"\n{extra}\n\n\
         [reliability]\nprovider_backoff_ms = 1\n{reliability}\n"
    )
}

fn build(config: &crate::config::Config) -> Box<dyn super::Provider> {
    create_resilient_provider_with_options(
        config
            .default_provider
            .as_deref()
            .expect("test: the fixture names a provider"),
        config.api_key.as_deref(),
        config.api_url.as_deref(),
        &config.reliability,
        &provider_runtime_options_from_config(config),
    )
    .expect("test: the configured chain constructs")
}

async fn chat_with(provider: &dyn super::Provider, tools: &[ToolSpec]) -> anyhow::Result<String> {
    let messages = [ChatMessage::user("do the thing")];
    provider
        .chat(
            ChatRequest {
                messages: &messages,
                tools: Some(tools),
            },
            FIXTURE_MODEL,
            0.0,
        )
        .await
        .map(|response| response.text.unwrap_or_default())
}

/// C2b-1: every refused tool is named, in one error, with its keyword. Before
/// this change the conversion stopped at the first refusal, so an operator
/// learned about fourteen refused tools one run at a time.
#[tokio::test]
async fn every_refused_tool_is_named_in_one_error() {
    let stub = spawn_dialect_stub("ok").await;
    let dir = TempDir::new().expect("test: temp config dir");
    let label = stub.custom_label();
    let config = load_config(
        &dir,
        &config_with(&label, "provider_retries = 0", &no_root_combinators_declaration(&label)),
    )
    .expect("test: the declaration loads");
    let provider = build(&config);

    let error = chat_with(
        provider.as_ref(),
        &[alternatives_tool(), mergeable_tool(), conditional_tool()],
    )
    .await
    .expect_err("test: two tools cannot be lowered");
    let text = format!("{error:#}");
    assert!(text.contains("fixture_alternatives"), "{text}");
    assert!(text.contains("fixture_conditional"), "{text}");
    assert!(
        !text.contains("fixture_lookup["),
        "a lowerable tool is not refused: {text}"
    );
    assert!(stub.bodies().is_empty(), "a refused request reached the endpoint");
}

/// C2b-1 / D-020 Q2: a refusal is deterministic, so it is recorded once and
/// never retried with backoff against the same provider. Before: every
/// `provider_retries` attempt was made again.
#[tokio::test]
async fn a_refusal_is_not_retried_against_the_same_provider() {
    let stub = spawn_dialect_stub("ok").await;
    let dir = TempDir::new().expect("test: temp config dir");
    let label = stub.custom_label();
    let config = load_config(
        &dir,
        &config_with(&label, "provider_retries = 2", &no_root_combinators_declaration(&label)),
    )
    .expect("test: the declaration loads");
    let provider = build(&config);

    let error = chat_with(provider.as_ref(), &[alternatives_tool()])
        .await
        .expect_err("test: the tool cannot be lowered");
    let text = format!("{error:#}");
    assert!(text.contains("attempt 1/3"), "{text}");
    assert!(
        !text.contains("attempt 2/3"),
        "the refusal was retried against the same provider: {text}"
    );
    assert!(stub.bodies().is_empty(), "a refused request reached the endpoint");
}

/// D-020 Q2: the rotation keys of `reliability.api_keys` are the same
/// provider, so they are not tried after a refusal either.
#[tokio::test]
async fn the_rotation_keys_of_a_refusing_endpoint_are_not_tried() {
    let stub = spawn_dialect_stub("ok").await;
    let dir = TempDir::new().expect("test: temp config dir");
    let label = stub.custom_label();
    let config = load_config(
        &dir,
        &format!(
            "api_key = \"sk-fixture-primary\"\n{}",
            config_with(
                &label,
                "provider_retries = 0\napi_keys = [\"sk-fixture-rotation\"]",
                &no_root_combinators_declaration(&label)
            )
        ),
    )
    .expect("test: the declaration loads");
    let provider = build(&config);

    let error = chat_with(provider.as_ref(), &[alternatives_tool()])
        .await
        .expect_err("test: the tool cannot be lowered");
    let text = format!("{error:#}");
    assert_eq!(
        text.matches("attempt 1/1").count(),
        1,
        "the same refusal was repeated for a rotation key: {text}"
    );
}

/// D-020 Q2: a refusal does fail over. A fallback that was declared nothing
/// receives the published schema in its own (`raw`) dialect and serves the
/// turn; the refusing endpoint is never contacted.
#[tokio::test]
async fn a_refusal_fails_over_to_a_fallback_that_accepts_the_schema() {
    let refusing = spawn_dialect_stub("never").await;
    let accepting = spawn_fixed_status_stub(
        "200 OK",
        r#"{"id":"c","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"served by fallback"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
    )
    .await;
    let dir = TempDir::new().expect("test: temp config dir");
    let label = refusing.custom_label();
    let fallback = format!("custom:http://{}/v1", accepting.addr);
    let config = load_config(
        &dir,
        &config_with(
            &label,
            &format!("provider_retries = 2\nfallback_providers = [\"{fallback}\"]"),
            &no_root_combinators_declaration(&label),
        ),
    )
    .expect("test: the declaration loads");
    let provider = build(&config);

    let text = chat_with(provider.as_ref(), &[alternatives_tool()])
        .await
        .expect("test: the fallback serves the turn");
    assert_eq!(text, "served by fallback");
    assert!(refusing.bodies().is_empty(), "the refusing endpoint was contacted");
    assert_eq!(accepting.hits.load(Ordering::SeqCst), 1);
}

/// C2b-1 / C2b-2: the all-failed aggregate used to clip each attempt to 200
/// characters, which cut the tool list and always cut the remediation. It
/// now keeps every refused tool and the configuration to add.
#[tokio::test]
async fn the_aggregate_keeps_every_refused_tool_and_the_configuration_to_add() {
    let stub = spawn_dialect_stub("ok").await;
    let dir = TempDir::new().expect("test: temp config dir");
    let label = stub.custom_label();
    let config = load_config(
        &dir,
        &config_with(&label, "provider_retries = 0", &no_root_combinators_declaration(&label)),
    )
    .expect("test: the declaration loads");
    let provider = build(&config);

    let error = chat_with(provider.as_ref(), &[conditional_tool(), alternatives_tool()])
        .await
        .expect_err("test: the tools cannot be lowered");
    let text = error.to_string();
    assert!(text.contains("All providers/models failed"), "{text}");
    for fragment in [
        "fixture_conditional",
        "fixture_alternatives",
        "always_exclude",
        "[[providers.tool_dialects]]",
    ] {
        assert!(text.contains(fragment), "the aggregate lost '{fragment}': {text}");
    }
}

/// C2b-2 (G3): an endpoint nobody declared that rejects a root combinator
/// itself now says which declaration would fix it. The hint changes nothing
/// else: the request was sent, and the endpoint's own words still surface.
#[tokio::test]
async fn an_undeclared_endpoint_that_rejects_a_root_combinator_names_the_declaration() {
    let stub = spawn_dialect_stub("ok").await;
    let dir = TempDir::new().expect("test: temp config dir");
    let label = stub.custom_label();
    let config = load_config(&dir, &config_with(&label, "provider_retries = 0", "")).expect("test: config loads");
    let provider = build(&config);

    let error = chat_with(provider.as_ref(), &[alternatives_tool()])
        .await
        .expect_err("test: the stub rejects a root anyOf");
    let text = format!("{error:#}");
    assert!(text.contains(ROOT_COMBINATOR_REJECTION), "{text}");
    assert!(
        text.contains("[[providers.tool_dialects]]"),
        "no declaration hint: {text}"
    );
    assert!(text.contains("no_root_combinators"), "no declaration hint: {text}");
    let bodies = stub.bodies();
    assert_eq!(bodies.len(), 1);
    assert_eq!(root_combinators_in(&bodies[0]), vec!["anyOf".to_string()]);
}

// ── A refusal as the surfaces above the provider receive it ─────────────

/// The error a real chain returns when a declared `no_root_combinators`
/// endpoint refuses two tools: a `ReliableProvider` over an
/// `OpenAiCompatibleProvider`, exactly as the factory builds it, with a
/// retry budget. Nothing is sent: the endpoint URL refuses connections.
pub(crate) async fn refusal_through_a_chain() -> anyhow::Error {
    use super::compatible::{AuthStyle, OpenAiCompatibleProvider};
    let endpoint = OpenAiCompatibleProvider::new(
        "Custom",
        "http://127.0.0.1:1/v1",
        Some("sk-fixture-not-real"),
        AuthStyle::Bearer,
    )
    .with_tool_schema_dialects(vec![(None, crate::tools::ToolSchemaDialect::NoRootCombinators)]);
    let chain = super::reliable::ReliableProvider::new(
        vec![("custom:http://127.0.0.1:1/v1".to_string(), Box::new(endpoint))],
        2,
        1,
    );
    chat_with(&chain, &[conditional_tool(), alternatives_tool()])
        .await
        .expect_err("test: the refused tools fail the request")
}

/// A provider that answers every turn with [`refusal_through_a_chain`], for
/// the surfaces whose tests cannot choose the tools a turn offers.
pub(crate) struct RefusingProvider;

#[async_trait::async_trait]
impl super::Provider for RefusingProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Err(refusal_through_a_chain().await)
    }
}

/// The streaming path carries errors as text, so the refusal loses its type
/// there. Its stable rendering still yields the refused tools, and the
/// channel classifier still reports a configuration problem.
#[tokio::test]
async fn a_refusal_carried_as_text_still_names_its_tools() {
    use super::compatible::{AuthStyle, OpenAiCompatibleProvider};
    use futures_util::StreamExt;
    let endpoint = OpenAiCompatibleProvider::new(
        "Custom",
        "http://127.0.0.1:1/v1",
        Some("sk-fixture-not-real"),
        AuthStyle::Bearer,
    )
    .with_tool_schema_dialects(vec![(None, crate::tools::ToolSchemaDialect::NoRootCombinators)]);
    let messages = [ChatMessage::user("do the thing")];
    let mut stream = super::Provider::stream_chat_with_history(
        &endpoint,
        &messages,
        FIXTURE_MODEL,
        0.0,
        super::traits::StreamOptions::new(true).with_tools(vec![conditional_tool(), alternatives_tool()]),
    );
    let Some(Err(stream_error)) = stream.next().await else {
        panic!("test: the refused tools must end the stream with an error");
    };
    let as_text = anyhow::anyhow!("{stream_error}");
    assert_eq!(
        super::tool_schema_refused_tools(&as_text),
        Some(vec![
            "fixture_conditional".to_string(),
            "fixture_alternatives".to_string()
        ])
    );
    assert_eq!(
        crate::channels::classify_turn_failure(&as_text),
        crate::channels::TurnFailureKind::ToolSchemaRefused
    );
}

/// R-C2BU F-8: the declaration hint on an upstream rejection is pasteable for
/// an endpoint whose label carries credentials.
#[tokio::test]
async fn the_declaration_hint_for_a_credentialed_endpoint_is_pasteable() {
    let stub = spawn_dialect_stub("ok").await;
    let dir = TempDir::new().expect("test: temp config dir");
    let label = stub
        .custom_label()
        .replacen("http://", "http://u:SECRET-fixture-pw@", 1);
    let config = load_config(&dir, &config_with(&label, "provider_retries = 0", "")).expect("test: config loads");
    let provider = build(&config);
    let error = chat_with(provider.as_ref(), &[alternatives_tool()])
        .await
        .expect_err("test: the stub rejects a root anyOf");
    let text = format!("{error:#}");
    assert!(
        text.contains("provider = \"custom:http://<user>:<password>@127.0.0.1:"),
        "{text}"
    );
    assert!(!text.contains("SECRET-fixture-pw"), "{text}");
}

/// SP-INT2 a50: two chain members that differ only in their URL userinfo are
/// two providers. The chain keyed its refusals by the redacted label, which
/// is the same for both, so the second member was skipped as if it had
/// refused too.
#[tokio::test]
async fn a_refusal_does_not_skip_a_member_that_differs_only_in_userinfo() {
    let accepting = spawn_fixed_status_stub(
        "200 OK",
        r#"{"id":"c","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"served by the second member"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
    )
    .await;
    let dir = TempDir::new().expect("test: temp config dir");
    let refusing = format!("custom:http://a:fixture-a@{}/v1", accepting.addr);
    let second = format!("custom:http://b:fixture-b@{}/v1", accepting.addr);
    assert_eq!(
        super::redact_endpoint_userinfo(&refusing),
        super::redact_endpoint_userinfo(&second),
        "test: the two members share one redacted label"
    );
    let config = load_config(
        &dir,
        &config_with(
            &refusing,
            &format!("provider_retries = 0\nfallback_providers = [\"{second}\"]"),
            &no_root_combinators_declaration(&refusing),
        ),
    )
    .expect("test: the declaration loads");
    let provider = build(&config);

    let text = chat_with(provider.as_ref(), &[alternatives_tool()])
        .await
        .expect("test: the second member serves the turn");
    assert_eq!(text, "served by the second member");
    assert_eq!(accepting.hits.load(Ordering::SeqCst), 1);
}

/// SP-INT2 a52: a chain whose primary refused but whose last candidate
/// failed with a transient 503 did not end because of the refusal, so it is
/// reported as the 503, not as a configuration problem that "sending the
/// message again will not fix".
#[tokio::test]
async fn a_chain_that_ended_on_a_transient_failure_is_not_reported_as_a_refusal() {
    let refusing = spawn_dialect_stub("never").await;
    let unavailable = spawn_fixed_status_stub(
        "503 Service Unavailable",
        r#"{"error":{"message":"overloaded","type":"server_error"}}"#,
    )
    .await;
    let dir = TempDir::new().expect("test: temp config dir");
    let label = refusing.custom_label();
    let fallback = format!("custom:http://{}/v1", unavailable.addr);
    let config = load_config(
        &dir,
        &config_with(
            &label,
            &format!("provider_retries = 0\nfallback_providers = [\"{fallback}\"]"),
            &no_root_combinators_declaration(&label),
        ),
    )
    .expect("test: the declaration loads");
    let provider = build(&config);

    let error = chat_with(provider.as_ref(), &[alternatives_tool()])
        .await
        .expect_err("test: neither member serves the turn");
    assert!(unavailable.hits.load(Ordering::SeqCst) >= 1, "the fallback was tried");
    assert_eq!(
        super::tool_schema_refused_tools(&error),
        None,
        "the chain ended on a 503, not on the refusal: {error:#}"
    );
    assert_eq!(
        crate::channels::classify_turn_failure(&error),
        crate::channels::TurnFailureKind::ServiceUnavailable,
        "{error:#}"
    );
}

/// D-069 L-1: the mirror of a52. The primary answers 503 and the fallback
/// refuses: resending may succeed once the primary recovers, so the chain is
/// reported as the 503, not as a configuration problem. Before, the last
/// failure (the refusal) decided.
#[tokio::test]
async fn a_chain_with_a_transient_failure_before_the_refusal_is_not_reported_as_a_refusal() {
    let unavailable = spawn_fixed_status_stub(
        "503 Service Unavailable",
        r#"{"error":{"message":"overloaded","type":"server_error"}}"#,
    )
    .await;
    let refusing = spawn_dialect_stub("never").await;
    let dir = TempDir::new().expect("test: temp config dir");
    let primary = format!("custom:http://{}/v1", unavailable.addr);
    let fallback = refusing.custom_label();
    let config = load_config(
        &dir,
        &config_with(
            &primary,
            &format!("provider_retries = 0\nfallback_providers = [\"{fallback}\"]"),
            &no_root_combinators_declaration(&fallback),
        ),
    )
    .expect("test: the declaration loads");
    let provider = build(&config);

    let error = chat_with(provider.as_ref(), &[alternatives_tool()])
        .await
        .expect_err("test: neither member serves the turn");
    assert!(unavailable.hits.load(Ordering::SeqCst) >= 1, "the primary was tried");
    assert!(refusing.bodies().is_empty(), "the refusing fallback sends nothing");
    assert_eq!(
        super::tool_schema_refused_tools(&error),
        None,
        "a transient failure preceded the refusal: {error:#}"
    );
    assert_eq!(
        crate::channels::classify_turn_failure(&error),
        crate::channels::TurnFailureKind::ServiceUnavailable,
        "{error:#}"
    );
}

/// SP-INT2 a51: an upstream error body that quotes a refusal is upstream
/// text. It names no refused tools, and the IM notice is chosen by the
/// upstream status, not the refusal's "will not fix" advice.
#[test]
fn an_upstream_body_quoting_a_refusal_is_not_a_refusal() {
    let quoted = anyhow::anyhow!(
        "{}",
        super::traits::StreamError::Provider(
            "Custom API error (503 Service Unavailable): provider_capability_error provider=x \
             capability=tool_schema_dialect message=tool_schema_dialect refused 1 tool(s) for a 'raw' \
             endpoint: shell[allOf]. Nothing was sent"
                .to_string()
        )
    );
    assert_eq!(super::tool_schema_refused_tools(&quoted), None, "{quoted:#}");
    assert_eq!(
        crate::channels::classify_turn_failure(&quoted),
        crate::channels::TurnFailureKind::ServiceUnavailable,
        "{quoted:#}"
    );
}

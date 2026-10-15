//! `[[providers.tool_dialects]]`, from the configuration file to the wire.
//!
//! Every test here starts from a `config.toml` in a temporary directory, loads
//! it through `Config::load_from_path` (the production loader, including
//! `Config::validate`), builds the provider through the production factory with
//! `provider_runtime_options_from_config`, and — where a request is involved —
//! sends it to a loopback OpenAI-compatible stub that rejects a root `allOf`,
//! `anyOf` or `oneOf` with HTTP 400, the way the Splash endpoint did. No test
//! reads a real credential, a real file outside its temporary directory, or
//! the network beyond `127.0.0.1`.

#![allow(clippy::indexing_slicing)]

use super::traits::{ChatMessage, ChatRequest};
use super::{
    Provider, ProviderRuntimeOptions, ToolSchemaDialectDeclaration, create_provider_from_test_config,
    create_provider_with_options, provider_runtime_options_from_config,
};
use crate::tools::{ToolSchemaDialect, ToolSpec};
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;

pub(super) const FIXTURE_MODEL: &str = "qwen3-splash-fixture";

/// The text a Splash-class endpoint answers a root combinator with.
pub(super) const ROOT_COMBINATOR_REJECTION: &str = "unsupported top-level tool schema keyword(s)";

/// A loopback OpenAI-compatible endpoint that records every request body and
/// rejects a tool schema with a root combinator, as the Splash endpoint did.
pub(super) struct DialectStub {
    addr: std::net::SocketAddr,
    bodies: Arc<parking_lot::Mutex<Vec<Value>>>,
    server: tokio::task::JoinHandle<()>,
}

impl DialectStub {
    pub(super) fn custom_label(&self) -> String {
        format!("custom:http://{}/v1", self.addr)
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    pub(super) fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().clone()
    }
}

impl Drop for DialectStub {
    fn drop(&mut self) {
        self.server.abort();
    }
}

pub(super) fn root_combinators_in(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.pointer("/function/parameters").and_then(Value::as_object))
        .flat_map(|parameters| {
            ["allOf", "anyOf", "oneOf"]
                .into_iter()
                .filter(|keyword| parameters.contains_key(*keyword))
                .map(str::to_string)
        })
        .collect()
}

pub(super) async fn spawn_dialect_stub(reply_content: &str) -> DialectStub {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: bind loopback");
    let addr = listener.local_addr().expect("test: loopback address");
    let bodies: Arc<parking_lot::Mutex<Vec<Value>>> = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = Arc::clone(&bodies);
    let success = json!({
        "id": "chatcmpl-dialect",
        "object": "chat.completion",
        "created": 1,
        "model": FIXTURE_MODEL,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": reply_content},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
    .to_string();

    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _peer)) = listener.accept().await else {
                return;
            };
            let sink = Arc::clone(&sink);
            let success = success.clone();
            tokio::spawn(async move {
                let mut buffer: Vec<u8> = Vec::with_capacity(8192);
                let mut chunk = [0_u8; 4096];
                let head_end = loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                    }
                    if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                        break position + 4;
                    }
                    if buffer.len() > 1_048_576 {
                        return;
                    }
                };
                let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
                let content_length = head
                    .split("\r\n")
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                while buffer.len() < head_end + content_length {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                    }
                }
                let body: Value = serde_json::from_slice(&buffer[head_end..]).unwrap_or(Value::Null);
                let rejected = root_combinators_in(&body);
                sink.lock().push(body);

                let (status, payload) = if rejected.is_empty() {
                    ("200 OK", success)
                } else {
                    (
                        "400 Bad Request",
                        json!({"error": {
                            "message": format!("{ROOT_COMBINATOR_REJECTION}: {}", rejected.join(", ")),
                            "type": "invalid_request_error"
                        }})
                        .to_string(),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });

    DialectStub { addr, bodies, server }
}

/// Write `body` as `config.toml` in `dir` and load it the way the daemon does.
pub(super) fn load_config(dir: &TempDir, body: &str) -> anyhow::Result<crate::config::Config> {
    let path = dir.path().join("config.toml");
    std::fs::write(&path, body).expect("test: write config.toml");
    crate::config::Config::load_from_path(&path, dir.path().join("workspace"))
}

/// The provider chain the gateway and the channels build for the configured
/// default provider, from nothing but the loaded configuration.
fn build_default_provider(config: &crate::config::Config) -> Box<dyn Provider> {
    create_provider_from_test_config(config, &provider_runtime_options_from_config(config))
        .expect("test: the configured provider constructs")
}

fn config_for(provider: &str, extra: &str) -> String {
    format!(
        "default_temperature = 0.0\ndefault_provider = \"{provider}\"\ndefault_model = \"{FIXTURE_MODEL}\"\n{extra}\n\n\
         [reliability]\nprovider_retries = 0\nprovider_backoff_ms = 1\n"
    )
}

/// A published contract whose root `allOf` is plain object fragments, so
/// `no_root_combinators` merges it without changing what it accepts.
pub(super) fn mergeable_tool() -> ToolSpec {
    ToolSpec {
        name: "fixture_lookup".to_string(),
        description: "Look something up.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"],
            "allOf": [{
                "properties": {"limit": {"type": "integer", "minimum": 1}},
                "required": ["limit"]
            }]
        }),
    }
}

/// A published contract whose root `anyOf` is a choice between required sets,
/// which no single object can express: `no_root_combinators` must refuse it.
pub(super) fn alternatives_tool() -> ToolSpec {
    ToolSpec {
        name: "fixture_alternatives".to_string(),
        description: "Needs a path or a key.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}, "key": {"type": "string"}},
            "anyOf": [{"required": ["path"]}, {"required": ["key"]}]
        }),
    }
}

pub(super) fn no_root_combinators_declaration(provider: &str) -> String {
    format!("[[providers.tool_dialects]]\nprovider = \"{provider}\"\ntool_schema = \"no_root_combinators\"\n")
}

// ── Red-before: the declaration reaches the wire ──────────────────────

/// Before this key existed the load itself failed ("Unknown configuration
/// path(s): providers.tool_dialects"), so an operator had no way to stop the
/// endpoint from receiving the root `allOf` it rejects.
#[tokio::test]
async fn a_configured_no_root_combinators_endpoint_receives_the_merged_document() {
    let stub = spawn_dialect_stub("ok").await;
    let dir = TempDir::new().expect("test: temp config dir");
    let label = stub.custom_label();
    let config = load_config(&dir, &config_for(&label, &no_root_combinators_declaration(&label)))
        .expect("test: a no_root_combinators declaration for a custom endpoint must load");
    let provider = build_default_provider(&config);

    let tools = [mergeable_tool()];
    let messages = [ChatMessage::user("look it up")];
    let response = provider
        .chat(
            ChatRequest {
                messages: &messages,
                tools: Some(&tools),
            },
            FIXTURE_MODEL,
            0.0,
        )
        .await
        .expect("test: the declared endpoint accepts the lowered document");
    assert_eq!(response.text.as_deref(), Some("ok"));

    let bodies = stub.bodies();
    assert_eq!(bodies.len(), 1, "exactly one request reaches the endpoint");
    assert!(
        root_combinators_in(&bodies[0]).is_empty(),
        "a root combinator reached a no_root_combinators endpoint: {}",
        bodies[0]
    );
    let wire = bodies[0]
        .pointer("/tools/0/function/parameters")
        .expect("test: the tool went out");
    assert_eq!(
        wire.get("required"),
        Some(&json!(["query", "limit"])),
        "the merge must keep every requirement: {wire}"
    );
    assert_eq!(wire.pointer("/properties/limit/minimum"), Some(&json!(1)));
}

/// The same endpoint with no declaration keeps the published document, and
/// the stub rejects it exactly as the Splash endpoint did. This is the
/// reproduction the declaration exists to fix; it must stay reproducible,
/// because an undeclared endpoint is never lowered on a guess.
#[tokio::test]
async fn an_undeclared_endpoint_is_sent_the_published_root_combinator() {
    let stub = spawn_dialect_stub("ok").await;
    let dir = TempDir::new().expect("test: temp config dir");
    let config = load_config(&dir, &config_for(&stub.custom_label(), "")).expect("test: plain config loads");
    let provider = build_default_provider(&config);

    let tools = [mergeable_tool()];
    let messages = [ChatMessage::user("look it up")];
    let error = provider
        .chat(
            ChatRequest {
                messages: &messages,
                tools: Some(&tools),
            },
            FIXTURE_MODEL,
            0.0,
        )
        .await
        .expect_err("test: the stub rejects a root allOf");
    assert!(
        format!("{error:#}").contains(ROOT_COMBINATOR_REJECTION),
        "the endpoint's own rejection must surface: {error:#}"
    );
    let bodies = stub.bodies();
    assert!(!bodies.is_empty(), "expected a non-empty value");
    assert_eq!(root_combinators_in(&bodies[0]), vec!["allOf".to_string()]);
}

/// A contract the dialect cannot express fails before transport: no request
/// at all, and the error names the tool, the keyword and the configuration
/// line the operator would edit.
#[tokio::test]
async fn a_refused_tool_fails_before_transport_and_names_the_declaration() {
    let stub = spawn_dialect_stub("ok").await;
    let dir = TempDir::new().expect("test: temp config dir");
    let label = stub.custom_label();
    let config = load_config(&dir, &config_for(&label, &no_root_combinators_declaration(&label)))
        .expect("test: the declaration loads");
    let provider = build_default_provider(&config);

    let tools = [mergeable_tool(), alternatives_tool()];
    let messages = [ChatMessage::user("look it up")];
    let error = provider
        .chat(
            ChatRequest {
                messages: &messages,
                tools: Some(&tools),
            },
            FIXTURE_MODEL,
            0.0,
        )
        .await
        .expect_err("test: an unmergeable root anyOf is refused");
    let text = format!("{error:#}");
    assert!(stub.bodies().is_empty(), "a refused request reached the wire");
    assert!(
        text.contains("fixture_alternatives"),
        "the refused tool is named: {text}"
    );
    assert!(text.contains("anyOf"), "the keyword is named: {text}");

    // The failover aggregate above clips each attempt to 200 characters, which
    // cuts the "Declared by" clause (C2b gap G2). The provider's own error,
    // which the TUI and the session worker show in full, points at the entry.
    let leaf = create_provider_with_options(&label, None, &provider_runtime_options_from_config(&config))
        .expect("test: the endpoint constructs");
    let error = leaf
        .chat(
            ChatRequest {
                messages: &messages,
                tools: Some(&tools),
            },
            FIXTURE_MODEL,
            0.0,
        )
        .await
        .expect_err("test: the leaf refuses too");
    let text = format!("{error:#}");
    assert!(stub.bodies().is_empty(), "a refused request reached the wire");
    assert!(
        text.contains(&format!(
            "Declared by: `[[providers.tool_dialects]]` (provider '{label}', every model)"
        )),
        "the error must point at the configuration entry: {text}"
    );
}

/// A model-scoped `raw` entry pins that one model back to the published
/// document under a provider-wide `no_root_combinators`: the most specific
/// declaration wins, per model, on the wire.
#[tokio::test]
async fn a_model_scoped_raw_entry_overrides_a_provider_wide_declaration() {
    let stub = spawn_dialect_stub("ok").await;
    let dir = TempDir::new().expect("test: temp config dir");
    let label = stub.custom_label();
    let declarations = format!(
        "{}\n[[providers.tool_dialects]]\nprovider = \"{label}\"\nmodel = \"published-model\"\ntool_schema = \"raw\"\n",
        no_root_combinators_declaration(&label)
    );
    let config = load_config(&dir, &config_for(&label, &declarations)).expect("test: both entries load");
    let lowered_provider = build_default_provider(&config);
    let mut published_config = config.clone();
    published_config.default_model = Some("PUBLISHED-MODEL".to_string());
    let published_provider = build_default_provider(&published_config);

    let tools = [mergeable_tool()];
    let messages = [ChatMessage::user("look it up")];
    let lowered = lowered_provider
        .chat(
            ChatRequest {
                messages: &messages,
                tools: Some(&tools),
            },
            FIXTURE_MODEL,
            0.0,
        )
        .await;
    assert!(lowered.is_ok(), "the provider-wide entry lowers: {lowered:?}");
    let published = published_provider
        .chat(
            ChatRequest {
                messages: &messages,
                tools: Some(&tools),
            },
            "PUBLISHED-MODEL",
            0.0,
        )
        .await;
    assert!(published.is_err(), "the raw pin must send the published document");

    let bodies = stub.bodies();
    assert_eq!(bodies.len(), 2);
    assert!(root_combinators_in(&bodies[0]).is_empty(), "expected an empty value");
    assert_eq!(root_combinators_in(&bodies[1]), vec!["allOf".to_string()]);
}

/// D-020 Q3: an OpenAI-compatible server run by the operator and named by its
/// catalog entry (`vllm`, `llamacpp`, `lmstudio`, `litellm`) takes the same
/// declaration a `custom:<url>` endpoint does, keyed by its catalog identity.
#[tokio::test]
async fn a_catalog_local_server_honours_its_declaration_on_the_wire() {
    let stub = spawn_dialect_stub("ok").await;
    let dir = TempDir::new().expect("test: temp config dir");
    let extra = format!(
        "api_url = \"{}\"\napi_key = \"fixture-not-a-credential\"\n\n{}",
        stub.base_url(),
        no_root_combinators_declaration("v-llm")
    );
    let config = load_config(&dir, &config_for("vllm", &extra)).expect("test: a vllm declaration loads");
    let provider = build_default_provider(&config);

    let tools = [mergeable_tool()];
    let messages = [ChatMessage::user("look it up")];
    let response = provider
        .chat(
            ChatRequest {
                messages: &messages,
                tools: Some(&tools),
            },
            FIXTURE_MODEL,
            0.0,
        )
        .await;
    assert!(
        response.is_ok(),
        "the declared local server was sent a root allOf: {response:?}"
    );
    let bodies = stub.bodies();
    assert_eq!(bodies.len(), 1);
    assert!(root_combinators_in(&bodies[0]).is_empty(), "expected an empty value");
}

// ── Red-before: one declaration per endpoint identity (R-INT1 I-2) ────

/// A declaration is keyed by C1's canonical provider identity, so every
/// spelling that identity treats as one endpoint shares it, and nothing else
/// does. Before, the lookup was an ASCII case-fold of the raw label: a padded
/// or trailing-slash spelling missed, and a different URL path matched.
#[test]
fn a_declaration_matches_every_spelling_of_the_same_endpoint_and_no_other() {
    let options = ProviderRuntimeOptions {
        tool_schema_dialects: vec![ToolSchemaDialectDeclaration {
            provider: "custom:http://h.invalid:8080/v1".to_string(),
            model: None,
            dialect: ToolSchemaDialect::NoRootCombinators,
        }],
        ..ProviderRuntimeOptions::default()
    };
    let dialect_of = |label: &str| {
        create_provider_with_options(label, Some("fixture-not-a-credential"), &options)
            .expect("test: the label constructs")
            .tool_schema_dialect()
    };
    for same in [
        "custom:http://h.invalid:8080/v1",
        "CUSTOM:http://h.invalid:8080/v1/",
        "  custom:http://H.INVALID:8080/v1  ",
        "custom:HTTP://h.invalid:8080/v1/",
    ] {
        assert_eq!(
            dialect_of(same),
            ToolSchemaDialect::NoRootCombinators,
            "'{same}' is the declared endpoint"
        );
    }
    for other in [
        "custom:http://h.invalid:9090/v1",
        "custom:http://h.invalid:8080/V1",
        "custom:http://h.invalid:8080/splash/v1",
        "custom:https://h.invalid:8080/v1",
    ] {
        assert_eq!(
            dialect_of(other),
            ToolSchemaDialect::Raw,
            "'{other}' is another endpoint"
        );
    }
}

/// Undeclared means `Raw` for every endpoint, whatever its URL looks like.
#[test]
fn an_undeclared_custom_endpoint_stays_raw_whatever_its_url_says() {
    let dir = TempDir::new().expect("test: temp config dir");
    let config =
        load_config(&dir, &config_for("custom:http://127.0.0.1:1/splash/v1", "")).expect("test: plain config loads");
    let options = provider_runtime_options_from_config(&config);
    for label in [
        "custom:http://127.0.0.1:1/splash/v1",
        "custom:http://127.0.0.1:1/no_root_combinators/v1",
    ] {
        let provider = create_provider_with_options(label, None, &options).expect("test: constructs");
        assert_eq!(provider.tool_schema_dialect(), ToolSchemaDialect::Raw, "{label}");
    }
}

// ── Red-before: textual grammar through config and factory (R-INT1 I-3) ──

const TEXTUAL_CALL: &str = r#"<function=fixture_lookup>{"query": "x", "limit": 1}</function>"#;

async fn textual_calls_through_config(declare: bool, api_key: Option<&str>) -> Vec<String> {
    let stub = spawn_dialect_stub(TEXTUAL_CALL).await;
    let dir = TempDir::new().expect("test: temp config dir");
    let label = stub.custom_label();
    let mut extra = String::new();
    if let Some(key) = api_key {
        extra.push_str(&format!("api_key = \"{key}\"\n"));
    }
    if declare {
        extra.push_str(&format!(
            "\n[[providers.tool_dialects]]\nprovider = \"{label}\"\ntool_call = \"xml_function_tags\"\n"
        ));
    }
    let config = load_config(&dir, &config_for(&label, &extra)).expect("test: the config loads");
    let provider = build_default_provider(&config);
    let tools = [ToolSpec {
        name: "fixture_lookup".to_string(),
        description: "Look something up.".to_string(),
        parameters: json!({"type": "object", "properties": {"query": {"type": "string"}}}),
    }];
    let messages = [ChatMessage::user("look it up")];
    provider
        .chat(
            ChatRequest {
                messages: &messages,
                tools: Some(&tools),
            },
            FIXTURE_MODEL,
            0.0,
        )
        .await
        .expect("test: the stub answers")
        .tool_calls
        .into_iter()
        .map(|call| call.name)
        .collect()
}

#[tokio::test]
async fn a_configured_textual_grammar_is_read_on_a_keyless_endpoint() {
    assert_eq!(
        textual_calls_through_config(true, None).await,
        vec!["fixture_lookup".to_string()]
    );
}

#[tokio::test]
async fn a_configured_textual_grammar_is_read_on_a_keyed_endpoint() {
    assert_eq!(
        textual_calls_through_config(true, Some("fixture-not-a-credential")).await,
        vec!["fixture_lookup".to_string()]
    );
}

#[tokio::test]
async fn an_undeclared_endpoint_never_reads_the_textual_grammar() {
    assert!(
        textual_calls_through_config(false, None).await.is_empty(),
        "expected an empty value"
    );
    assert!(
        textual_calls_through_config(false, Some("fixture-not-a-credential"))
            .await
            .is_empty(),
        "expected an empty value"
    );
}

// ── Red-before: typed load errors ─────────────────────────────────────

fn load_error(entry: &str) -> String {
    let dir = TempDir::new().expect("test: temp config dir");
    let body = format!("{}\n{entry}", config_for("custom:http://127.0.0.1:1/v1", ""));
    format!(
        "{:#}",
        load_config(&dir, &body).expect_err("test: the entry must be refused at load")
    )
}

#[test]
fn a_cleaning_dialect_or_a_typo_is_not_declarable() {
    for value in ["gemini", "anthropic", "no_root"] {
        let error = load_error(&format!(
            "[[providers.tool_dialects]]\nprovider = \"custom:http://127.0.0.1:1/v1\"\ntool_schema = \"{value}\"\n"
        ));
        assert!(
            error.contains(value) && error.contains("no_root_combinators") && error.contains("raw"),
            "'{value}' must be refused naming the accepted values: {error}"
        );
    }
    let error = load_error(
        "[[providers.tool_dialects]]\nprovider = \"custom:http://127.0.0.1:1/v1\"\ntool_call = \"function_tags\"\n",
    );
    assert!(
        error.contains("xml_function_tags") && error.contains("native"),
        "an unknown grammar must be refused naming the accepted values: {error}"
    );
}

#[test]
fn a_provider_that_ignores_declarations_is_refused_at_load() {
    for provider in [
        "openai",
        "gemini",
        "anthropic",
        "groq",
        "anthropic-custom:https://x.invalid",
        "custom:not a url",
        "no-such-provider",
        "   ",
    ] {
        let error = load_error(&format!(
            "[[providers.tool_dialects]]\nprovider = \"{provider}\"\ntool_schema = \"no_root_combinators\"\n"
        ));
        assert!(
            error.contains("providers.tool_dialects[0]"),
            "'{provider}' must be refused naming the entry: {error}"
        );
    }
}

#[test]
fn a_model_scoped_textual_grammar_is_refused_at_load() {
    let error = load_error(
        "[[providers.tool_dialects]]\nprovider = \"custom:http://127.0.0.1:1/v1\"\nmodel = \"m\"\ntool_call = \"xml_function_tags\"\n",
    );
    assert!(
        error.contains("providers.tool_dialects[0]") && error.contains("tool_call"),
        "{error}"
    );
}

#[test]
fn a_duplicate_or_an_empty_entry_is_refused_at_load() {
    let duplicate = load_error(
        "[[providers.tool_dialects]]\nprovider = \"custom:http://127.0.0.1:1/v1\"\ntool_schema = \"raw\"\n\n\
         [[providers.tool_dialects]]\nprovider = \"CUSTOM:http://127.0.0.1:1/v1/\"\ntool_schema = \"no_root_combinators\"\n",
    );
    assert!(
        duplicate.contains("providers.tool_dialects[1]") && duplicate.contains("providers.tool_dialects[0]"),
        "a duplicate names both entries: {duplicate}"
    );
    let empty = load_error("[[providers.tool_dialects]]\nprovider = \"custom:http://127.0.0.1:1/v1\"\n");
    assert!(
        empty.contains("providers.tool_dialects[0]") && empty.contains("tool_schema"),
        "{empty}"
    );
    let blank_model = load_error(
        "[[providers.tool_dialects]]\nprovider = \"custom:http://127.0.0.1:1/v1\"\nmodel = \" \"\ntool_schema = \"raw\"\n",
    );
    assert!(
        blank_model.contains("providers.tool_dialects[0]") && blank_model.contains("model"),
        "{blank_model}"
    );
}

// ── Enforcement agrees with the wire (INT-1 / R-C2a C2a-03(c)) ────────

/// The agent loop stamps the validation dialect from the provider object for
/// the model it serves (PMM-C-L; it used to read the serving provider's
/// label, which could not see a declaration). This pins that the dialect the
/// loop enforces, the constructed provider's dialect and the document the
/// stub actually received are the same contract: every argument the model
/// could send is accepted or rejected, and normalized, identically.
#[tokio::test]
async fn loop_enforcement_accepts_exactly_what_the_declared_wire_document_accepts() {
    let stub = spawn_dialect_stub("ok").await;
    let dir = TempDir::new().expect("test: temp config dir");
    let label = stub.custom_label();
    let config = load_config(&dir, &config_for(&label, &no_root_combinators_declaration(&label)))
        .expect("test: the declaration loads");
    let provider = build_default_provider(&config);
    let tool = mergeable_tool();
    let messages = [ChatMessage::user("look it up")];
    provider
        .chat(
            ChatRequest {
                messages: &messages,
                tools: Some(std::slice::from_ref(&tool)),
            },
            FIXTURE_MODEL,
            0.0,
        )
        .await
        .expect("test: the declared endpoint accepts the lowered document");
    let shown = stub.bodies()[0]
        .pointer("/tools/0/function/parameters")
        .cloned()
        .expect("test: the tool went out");

    // The agent loop asks the provider object for the model it serves.
    let loop_dialect = provider.tool_schema_dialect_for(FIXTURE_MODEL);
    let enforced_by_loop =
        crate::tools::schema::compiled_tool_schema_for_dialect(&tool.name, loop_dialect, &tool.parameters);
    let enforced_by_provider = crate::tools::schema::compiled_tool_schema_for_dialect(
        &tool.name,
        provider.tool_schema_dialect(),
        &tool.parameters,
    );
    let as_shown = crate::tools::schema::CompiledToolSchema::compile(&shown);

    for probe in [
        json!({"query": "x", "limit": 3}),
        json!({"query": "x"}),
        json!({"limit": 3}),
        json!({"query": "x", "limit": "3"}),
        json!({"query": "x", "limit": 0}),
        json!({"query": 1, "limit": 3}),
        json!({"query": "x", "limit": 3, "extra": true}),
    ] {
        let expected = as_shown.validate_tool_arguments(&probe).is_empty();
        assert_eq!(
            enforced_by_loop.validate_tool_arguments(&probe).is_empty(),
            expected,
            "the loop's dialect disagrees with the wire on {probe}"
        );
        assert_eq!(
            enforced_by_provider.validate_tool_arguments(&probe).is_empty(),
            expected,
            "the constructed provider's dialect disagrees with the wire on {probe}"
        );
    }

    let mut shown_value = json!({"query": "x", "limit": 3.0});
    let mut loop_value = shown_value.clone();
    as_shown.normalize_integral_numbers(&mut shown_value);
    enforced_by_loop.normalize_integral_numbers(&mut loop_value);
    assert_eq!(shown_value, json!({"query": "x", "limit": 3}));
    assert_eq!(loop_value, shown_value, "integral normalization must agree");
}

// ── Red-before: the catalog local-server constructors (D-020 Q3) ──────

/// Every spelling of the four catalog entries that build an
/// `OpenAiCompatibleProvider` against a server the operator runs applies a
/// declaration made under any other spelling of the same entry. Before, only
/// the `custom:` arm read declarations at all.
#[test]
fn every_catalog_local_server_spelling_applies_its_declaration() {
    for (declared, constructed) in [
        ("vllm", "vllm"),
        ("vllm", "v-llm"),
        ("llama.cpp", "llamacpp"),
        ("llamacpp", "llama.cpp"),
        ("lm-studio", "lmstudio"),
        ("lmstudio", "lm-studio"),
        ("litellm", "lite-llm"),
        ("lite-llm", "litellm"),
    ] {
        let options = ProviderRuntimeOptions {
            tool_schema_dialects: vec![ToolSchemaDialectDeclaration {
                provider: declared.to_string(),
                model: None,
                dialect: ToolSchemaDialect::NoRootCombinators,
            }],
            ..ProviderRuntimeOptions::default()
        };
        let provider = create_provider_with_options(constructed, Some("fixture-not-a-credential"), &options)
            .expect("test: the catalog entry constructs");
        assert_eq!(
            provider.tool_schema_dialect(),
            ToolSchemaDialect::NoRootCombinators,
            "a declaration for '{declared}' must reach '{constructed}'"
        );
    }
}

/// R-INT1 I-3: a textual-grammar declaration handed to the factory reaches the
/// constructed provider's response parser, with and without a credential.
#[tokio::test]
async fn a_textual_grammar_declaration_reaches_the_parser_through_the_factory() {
    for key in [None, Some("fixture-not-a-credential")] {
        let stub = spawn_dialect_stub(TEXTUAL_CALL).await;
        let label = stub.custom_label();
        let options = ProviderRuntimeOptions {
            tool_call_dialects: vec![super::ToolCallDialectDeclaration {
                provider: label.clone(),
                model: None,
                dialect: super::ToolCallDialect::XmlFunctionTags,
            }],
            ..ProviderRuntimeOptions::default()
        };
        let provider = create_provider_with_options(&label, key, &options).expect("test: constructs");
        let messages = [ChatMessage::user("look it up")];
        let flat = [ToolSpec {
            name: "fixture_lookup".to_string(),
            description: "Look something up.".to_string(),
            parameters: json!({"type": "object"}),
        }];
        let calls = provider
            .chat(
                ChatRequest {
                    messages: &messages,
                    tools: Some(&flat),
                },
                FIXTURE_MODEL,
                0.0,
            )
            .await
            .expect("test: the stub answers")
            .tool_calls;
        assert_eq!(calls.len(), 1, "key {key:?}: the declared grammar was not read");
        assert_eq!(calls[0].name, "fixture_lookup");
    }
}

/// The table survives a save round trip unchanged (the web console and PMM's
/// migration both rewrite configuration), and an empty table is not written.
#[test]
fn the_table_round_trips_and_an_empty_one_is_not_written() {
    let dir = TempDir::new().expect("test: temp config dir");
    let declarations = "[[providers.tool_dialects]]\nprovider = \"custom:http://127.0.0.1:1/v1\"\n\
                        tool_schema = \"no_root_combinators\"\ntool_call = \"xml_function_tags\"\n\n\
                        [[providers.tool_dialects]]\nprovider = \"custom:http://127.0.0.1:1/v1\"\n\
                        model = \"published-model\"\ntool_schema = \"raw\"\n";
    let config = load_config(&dir, &config_for("custom:http://127.0.0.1:1/v1", declarations))
        .expect("test: the declarations load");
    let written = toml::to_string(&config.providers).expect("test: providers serialize");
    let reread: crate::config::ProvidersConfig = toml::from_str(&written).expect("test: providers parse back");
    assert_eq!(reread.tool_dialects, config.providers.tool_dialects);
    assert_eq!(reread.tool_dialects.len(), 2);

    let empty = toml::to_string(&crate::config::ProvidersConfig::default()).expect("test: default serializes");
    assert!(!empty.contains("tool_dialects"), "an empty table was written: {empty}");
}

// ── C2K-f F-1: URL userinfo never reaches an error ────────────────────

const USERINFO_SECRET: &str = "s3cr3t-fixture-pw";

fn userinfo_label(host_port: &str) -> String {
    format!("custom:http://alice:{USERINFO_SECRET}@{host_port}/v1")
}

#[test]
fn a_duplicate_entry_error_redacts_url_userinfo() {
    let label = userinfo_label("127.0.0.1:1");
    let error = load_error(&format!(
        "[[providers.tool_dialects]]\nprovider = \"{label}\"\ntool_schema = \"raw\"\n\n\
         [[providers.tool_dialects]]\nprovider = \"{label}/\"\ntool_schema = \"no_root_combinators\"\n"
    ));
    assert!(error.contains("providers.tool_dialects[1]"), "{error}");
    assert!(
        !error.contains(USERINFO_SECRET),
        "the duplicate error leaked the password: {error}"
    );
    assert!(error.contains("[redacted]@"), "{error}");
}

#[test]
fn an_unsupported_provider_error_redacts_url_userinfo() {
    let error = load_error(&format!(
        "[[providers.tool_dialects]]\nprovider = \"anthropic-custom:https://alice:{USERINFO_SECRET}@x.invalid/v1\"\n\
         tool_schema = \"no_root_combinators\"\n"
    ));
    assert!(error.contains("providers.tool_dialects[0]"), "{error}");
    assert!(
        !error.contains(USERINFO_SECRET),
        "the unsupported-provider error leaked the password: {error}"
    );
    assert!(error.contains("[redacted]@"), "{error}");
}

#[tokio::test]
async fn a_refusal_names_the_entry_without_url_userinfo() {
    let stub = spawn_dialect_stub("ok").await;
    let dir = TempDir::new().expect("test: temp config dir");
    let label = userinfo_label(&stub.addr.to_string());
    let config = load_config(&dir, &config_for(&label, &no_root_combinators_declaration(&label)))
        .expect("test: a userinfo endpoint declaration loads");
    let tools = [alternatives_tool()];
    let messages = [ChatMessage::user("look it up")];
    let request = || ChatRequest {
        messages: &messages,
        tools: Some(&tools),
    };

    let leaf = create_provider_with_options(&label, None, &provider_runtime_options_from_config(&config))
        .expect("test: the endpoint constructs");
    let leaf_error = format!(
        "{:#}",
        leaf.chat(request(), FIXTURE_MODEL, 0.0)
            .await
            .expect_err("test: the leaf refuses")
    );
    assert!(stub.bodies().is_empty(), "a refused request reached the wire");
    assert!(
        leaf_error.contains("Declared by: `[[providers.tool_dialects]]` (provider 'custom:http://[redacted]@"),
        "the refusal must still name the entry: {leaf_error}"
    );
    assert!(
        !leaf_error.contains(USERINFO_SECRET),
        "the refusal leaked the password: {leaf_error}"
    );
    // Not asserted: `ReliableProvider`'s all-failed aggregate prints its member
    // labels verbatim ("Available providers: [custom:http://alice:…@…]"). That
    // predates this key and applies to every failure of such an endpoint; it is
    // recorded for C2b/PMM in the C2K-f handoff rather than fixed here.
}

// ── C2K-f N-5: one file owns the table ────────────────────────────────

#[test]
fn a_config_d_fragment_may_not_silently_replace_the_main_files_table() {
    let dir = TempDir::new().expect("test: temp config dir");
    let fragment_dir = dir.path().join("config.d");
    std::fs::create_dir_all(&fragment_dir).expect("test: config.d");
    std::fs::write(
        fragment_dir.join("routing.toml"),
        no_root_combinators_declaration("custom:http://127.0.0.1:2/v1"),
    )
    .expect("test: write fragment");
    let error = format!(
        "{:#}",
        load_config(
            &dir,
            &config_for(
                "custom:http://127.0.0.1:1/v1",
                &no_root_combinators_declaration("custom:http://127.0.0.1:1/v1")
            )
        )
        .expect_err("test: two files declaring the table must be refused")
    );
    assert!(
        error.contains("providers.tool_dialects") && error.contains("routing.toml") && error.contains("config.toml"),
        "the error must name the table and both files: {error}"
    );
}

#[test]
fn a_config_d_fragment_alone_may_own_the_table() {
    let dir = TempDir::new().expect("test: temp config dir");
    let fragment_dir = dir.path().join("config.d");
    std::fs::create_dir_all(&fragment_dir).expect("test: config.d");
    std::fs::write(
        fragment_dir.join("routing.toml"),
        no_root_combinators_declaration("custom:http://127.0.0.1:2/v1"),
    )
    .expect("test: write fragment");
    let config = load_config(&dir, &config_for("custom:http://127.0.0.1:1/v1", "")).expect("test: one owner loads");
    assert_eq!(config.providers.tool_dialects.len(), 1);
}

// ── C2K-f N-6: an entry nothing uses is reported, not refused ─────────

#[test]
fn an_entry_for_an_unused_provider_loads_and_is_reported() {
    let dir = TempDir::new().expect("test: temp config dir");
    let declarations = format!(
        "{}\n{}\n{}",
        no_root_combinators_declaration("CUSTOM:http://127.0.0.1:1/v1/"),
        no_root_combinators_declaration("custom:http://127.0.0.1:9/v1"),
        no_root_combinators_declaration("vllm"),
    );
    let config = load_config(&dir, &config_for("custom:http://127.0.0.1:1/v1", &declarations))
        .expect("test: an unused entry is a warning, not a load error");
    assert_eq!(super::unused_tool_dialect_entries(&config), vec![1, 2]);

    let with_fallback = format!(
        "{}\n\n[reliability]\nprovider_retries = 0\nfallback_providers = [\"v-llm\"]\n",
        declarations
    );
    let dir = TempDir::new().expect("test: temp config dir");
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        format!(
            "default_temperature = 0.0\ndefault_provider = \"custom:http://127.0.0.1:1/v1\"\n\
             default_model = \"{FIXTURE_MODEL}\"\n{with_fallback}"
        ),
    )
    .expect("test: write config");
    let config = crate::config::Config::load_from_path(&path, dir.path().join("workspace"))
        .expect("test: a fallback-using config loads");
    assert_eq!(super::unused_tool_dialect_entries(&config), vec![1]);
}

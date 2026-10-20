//! Regression proof for adapter-owned fields and actual multi-turn sends.
#![cfg(test)]
use super::parameter_wire::{Format, request, retry_request};
use super::{ChatMessage, ChatRequest, Provider};
use serde_json::{Value, json};
use std::sync::Arc;

#[test]
fn ollama_boolean_thinking_survives_every_declared_model() {
    let catalog = super::catalog::ReleaseCatalog::embedded_ref().unwrap();
    let mut exercised = 0;
    for row in &catalog.models {
        if row.provider != "ollama" {
            continue;
        }
        for think in [true, false] {
            let body = json!({"model":row.api_model,"think":think,"options":{"temperature":0.0}});
            let encoded = serde_json::to_value(request("ollama", Format::Ollama, &row.api_model, &body)).unwrap();
            assert_eq!(*encoded.get("think").unwrap(), think, "{}", row.id);
            assert_eq!(*encoded.pointer("/options/temperature").unwrap(), 0.0);
        }
        exercised += 1;
    }
    assert!(exercised > 0);
}

#[test]
fn unverified_chat_declarations_preserve_classifier_temperature() {
    let catalog = super::catalog::ReleaseCatalog::embedded_ref().unwrap();
    let mut exercised = 0;
    for row in &catalog.models {
        if !row.parameters.is_empty() || !row.supports(super::catalog::ModelOperation::Chat) {
            continue;
        }
        let body = json!({"model":row.api_model,"temperature":0.0,"messages":[]});
        let encoded = serde_json::to_value(request(&row.provider, Format::Compatible, &row.api_model, &body)).unwrap();
        assert_eq!(encoded, body, "{}", row.id);
        exercised += 1;
    }
    assert!(exercised > 50);
}

#[tokio::test]
async fn streaming_usage_retry_captures_logical_parameters_once() {
    let capture = super::parameter_capture::Capture::new(true);
    capture.scope(async {
        let body = json!({"model":"kimi-for-coding","temperature":0.3,"stream":true,"stream_options":{"include_usage":true}});
        let first = serde_json::to_value(request("kimi-code", Format::Compatible, "kimi-for-coding", &body)).unwrap();
        let mut retry = body;
        retry.as_object_mut().unwrap().remove("stream_options");
        let second = serde_json::to_value(retry_request("kimi-code", Format::Compatible, "kimi-for-coding", &retry)).unwrap();
        assert_eq!(*first.get("temperature").unwrap(), 1.0);
        assert_eq!(*second.get("temperature").unwrap(), *first.get("temperature").unwrap());
        assert!(second.get("stream_options").is_none());
    }).await;
    assert_eq!(capture.take().len(), 1);
}

#[test]
fn selection_and_request_use_the_same_immutable_release_instance() {
    let catalog = super::catalog::CatalogView::release_only();
    assert!(std::ptr::eq(
        catalog.release_catalog().unwrap(),
        super::catalog::ReleaseCatalog::embedded_ref().unwrap()
    ));
}

#[tokio::test]
async fn anthropic_effort_only_preserves_a_two_request_tool_history() {
    use axum::{Json, Router, routing::post};
    let bodies = Arc::new(parking_lot::Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&bodies);
    let app = Router::new().route("/v1/messages", post(move |Json(body): Json<Value>| {
        let captured = Arc::clone(&captured);
        async move {
            let mut bodies = captured.lock();
            bodies.push(body);
            Json(if bodies.len() == 1 {
                json!({"content":[{"type":"tool_use","id":"prx_call","name":"prx_lookup","input":{}}],"stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":1}})
            } else {
                json!({"content":[{"type":"text","text":"complete"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})
            })
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider =
        super::anthropic::AnthropicProvider::with_base_url(Some("prx-fixture-key"), Some(&format!("http://{address}")));
    let tools = vec![crate::tools::ToolSpec {
        name: "prx_lookup".into(),
        description: "Read fixture".into(),
        parameters: json!({"type":"object","properties":{}}),
    }];
    let mut messages = vec![ChatMessage::user("read the fixture")];
    let first = provider
        .chat(
            ChatRequest {
                messages: &messages,
                tools: Some(&tools),
            },
            "claude-sonnet-5",
            0.7,
        )
        .await
        .unwrap();
    assert_eq!(first.tool_calls.len(), 1);
    messages.push(ChatMessage::assistant(
        json!({"tool_calls":first.tool_calls}).to_string(),
    ));
    messages.push(ChatMessage::tool(
        serde_json::to_string(&super::ToolResultMessage {
            tool_call_id: "prx_call".into(),
            content: "fixture value".into(),
        })
        .unwrap(),
    ));
    let second = provider
        .chat(
            ChatRequest {
                messages: &messages,
                tools: Some(&tools),
            },
            "claude-sonnet-5",
            0.7,
        )
        .await
        .unwrap();
    assert_eq!(second.text.as_deref(), Some("complete"));
    let bodies = bodies.lock();
    assert_eq!(bodies.len(), 2);
    for body in bodies.iter() {
        assert_eq!(*body.pointer("/output_config/effort").unwrap(), "medium");
        assert!(body.get("thinking").is_none());
    }
    let history = bodies.get(1).unwrap().get("messages").unwrap().to_string();
    assert!(history.contains("tool_result"));
    assert!(history.contains("tool_use"));
    assert!(history.contains("prx_call"));
    server.abort();
}

#[test]
fn typed_anthropic_identity_maps_effort_without_borrowing_it_for_custom_endpoints() {
    let body = json!({"model":"claude-sonnet-5","temperature":0.7,"thinking":{"type":"adaptive"}});
    let canonical = super::ProviderId::of("anthropic");
    let encoded = serde_json::to_value(super::parameter_wire::request_for(
        &canonical,
        Format::Anthropic,
        "claude-sonnet-5",
        &body,
    ))
    .unwrap();
    assert_eq!(*encoded.pointer("/output_config/effort").unwrap(), "medium");
    assert!(encoded.get("thinking").is_none());
    assert!(encoded.get("temperature").is_none());
    let custom = super::ProviderId::of("anthropic-custom:https://example.com");
    assert_eq!(
        serde_json::to_value(super::parameter_wire::request_for(
            &custom,
            Format::Anthropic,
            "claude-sonnet-5",
            &body
        ))
        .unwrap(),
        body
    );
}

#[tokio::test]
async fn anthropic_authentication_retry_keeps_one_preparation_and_identical_parameters() {
    let capture = super::parameter_capture::Capture::new(true);
    let provider = super::ProviderId::of("anthropic");
    capture
        .scope(async {
            let body = json!({"model":"claude-sonnet-5","max_tokens":4096,"temperature":0.7});
            let original = serde_json::to_value(
                super::parameter_wire::request_for(&provider, Format::Anthropic, "claude-sonnet-5", &body)
                    .with_default_output_limit(),
            )
            .unwrap();
            let retry = serde_json::to_value(
                super::parameter_wire::request_for(&provider, Format::Anthropic, "claude-sonnet-5", &body)
                    .with_default_output_limit()
                    .without_recording(),
            )
            .unwrap();
            assert_eq!(retry, original);
        })
        .await;
    assert_eq!(capture.take().len(), 1);
}

#[tokio::test]
async fn bound_openai_transport_does_not_borrow_another_providers_parameters() {
    use axum::{Json, Router, routing::post};
    let bodies = Arc::new(parking_lot::Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&bodies);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            captured.lock().push(body);
            async { Json(json!({"choices":[{"message":{"content":"prx reply"}}]})) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for identity in ["ovhcloud", "ovh", "custom:https://example.com/v1", "openai"] {
        let provider = super::openai::OpenAiProvider::with_base_url(Some(&base), Some("prx-fixture"))
            .with_parameter_provider(identity);
        provider
            .chat_with_system(None, "prx request", "gpt-5.6", 0.3)
            .await
            .unwrap();
    }
    server.abort();
    let bodies = bodies.lock();
    assert_eq!(bodies.len(), 4);
    for body in bodies.iter().take(3) {
        assert_eq!(body.get("temperature"), Some(&json!(0.3)));
        assert!(body.get("reasoning_effort").is_none());
    }
    let known = bodies.last().unwrap();
    assert!(known.get("temperature").is_none());
    assert!(known.get("reasoning_effort").is_some());
}

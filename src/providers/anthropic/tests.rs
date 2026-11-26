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

/// **UTF-8 streaming regression** — the byte-level decoder used by this
/// provider's stream loop must carry a character that the transport split
/// across two chunks, while still rejecting genuinely malformed bytes.
#[test]
fn stream_decoder_survives_split_multibyte_characters() {
    let mut decoder = SseTextDecoder::new(STREAM_DECODER_LABEL);
    let mut buf = String::new();
    let bytes = "\u{20ac}".as_bytes();
    assert_eq!(bytes.len(), 3);

    decoder
        .push(&bytes[..2], &mut buf)
        .expect("test: an incomplete character is not a stream error");
    assert!(buf.is_empty(), "test: nothing decodable yet");
    assert_eq!(decoder.pending_len(), 2, "test: the split tail is carried, not dropped");

    decoder.push(&bytes[2..], &mut buf).expect("test: character completes");
    assert_eq!(buf, "\u{20ac}");
    decoder.finish().expect("test: stream ends on a character boundary");

    // Genuinely invalid input must still fail, tagged with this provider.
    let mut decoder = SseTextDecoder::new(STREAM_DECODER_LABEL);
    let err = decoder
        .push(&[0x80], &mut buf)
        .expect_err("test: a stray continuation byte must be reported");
    assert!(err.to_string().contains(STREAM_DECODER_LABEL), "got {err}");

    // A body that stops mid-character is truncation, not success.
    let mut decoder = SseTextDecoder::new(STREAM_DECODER_LABEL);
    let mut sink = String::new();
    decoder
        .push(&bytes[..1], &mut sink)
        .expect("test: partial tail buffered");
    assert!(decoder.finish().is_err(), "test: truncated tail must surface");
}
use crate::auth::anthropic_token::{AnthropicAuthKind, detect_auth_kind};

#[test]
fn creates_with_key() {
    let p = AnthropicProvider::new(Some("anthropic-test-credential"));
    assert!(p.credential().is_some());
    assert_eq!(p.credential().as_deref(), Some("anthropic-test-credential"));
    assert_eq!(p.base_url, "https://api.anthropic.com");
}

#[test]
fn creates_without_key() {
    let p = AnthropicProvider::new(None);
    assert!(p.credential().is_none());
    assert_eq!(p.base_url, "https://api.anthropic.com");
}

#[test]
fn creates_with_empty_key() {
    let p = AnthropicProvider::new(Some(""));
    assert!(p.credential().is_none());
}

#[test]
fn creates_with_whitespace_key() {
    let p = AnthropicProvider::new(Some("  anthropic-test-credential  "));
    assert!(p.credential().is_some());
    assert_eq!(p.credential().as_deref(), Some("anthropic-test-credential"));
}

#[test]
fn creates_with_custom_base_url() {
    let p = AnthropicProvider::with_base_url(Some("anthropic-credential"), Some("https://api.example.com"));
    assert_eq!(p.base_url, "https://api.example.com");
    assert_eq!(p.credential().as_deref(), Some("anthropic-credential"));
}

#[test]
fn custom_base_url_trims_trailing_slash() {
    let p = AnthropicProvider::with_base_url(None, Some("https://api.example.com/"));
    assert_eq!(p.base_url, "https://api.example.com");
}

#[test]
fn default_base_url_when_none_provided() {
    let p = AnthropicProvider::with_base_url(None, None);
    assert_eq!(p.base_url, "https://api.anthropic.com");
}

#[tokio::test]
async fn chat_fails_without_key() {
    let p = AnthropicProvider::new(None);
    let result = p.chat_with_system(None, "hello", "claude-3-opus", 0.7).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("credentials not set"), "Expected key error, got: {err}");
}

#[test]
fn setup_token_detection_works() {
    assert!(AnthropicProvider::is_setup_token("sk-ant-oat01-abcdef"));
    assert!(!AnthropicProvider::is_setup_token("sk-ant-api-key"));
}

#[test]
fn apply_auth_does_not_send_setup_tokens() {
    let provider = AnthropicProvider::new(None);
    let request = provider
        .apply_auth(
            provider.http_client().get("https://api.anthropic.com/v1/models"),
            "sk-ant-oat01-test-token",
        )
        .build()
        .expect("request should build");

    assert!(request.headers().get("authorization").is_none());
    assert!(request.headers().get("anthropic-beta").is_none());
    assert!(request.headers().get("x-api-key").is_none());
    let refused = AnthropicProvider::new(Some("sk-ant-oat01-test-token"));
    let error = refused.ensure_fresh_credential().expect_err("setup tokens are refused");
    assert!(error.to_string().contains("17154008"), "{error}");
}

#[test]
fn apply_auth_uses_x_api_key_for_regular_tokens() {
    let provider = AnthropicProvider::new(None);
    let request = provider
        .apply_auth(
            provider.http_client().get("https://api.anthropic.com/v1/models"),
            "sk-ant-api-key",
        )
        .build()
        .expect("request should build");

    assert_eq!(
        request.headers().get("x-api-key").and_then(|v| v.to_str().ok()),
        Some("sk-ant-api-key")
    );
    assert!(request.headers().get("authorization").is_none());
    assert!(request.headers().get("anthropic-beta").is_none());
}

#[tokio::test]
async fn chat_with_system_fails_without_key() {
    let p = AnthropicProvider::new(None);
    let result = p
        .chat_with_system(Some("You are OpenPRX"), "hello", "claude-3-opus", 0.7)
        .await;
    assert!(result.is_err());
}

#[test]
fn chat_request_serializes_without_system() {
    let req = ChatRequest {
        model: "claude-3-opus".to_string(),
        max_tokens: 4096,
        system: None,
        messages: vec![Message {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        temperature: 0.7,
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(!json.contains("system"), "system field should be skipped when None");
    assert!(json.contains("claude-3-opus"));
    assert!(json.contains("hello"));
}

#[test]
fn chat_request_serializes_with_system() {
    let req = ChatRequest {
        model: "claude-3-opus".to_string(),
        max_tokens: 4096,
        system: Some("You are OpenPRX".to_string()),
        messages: vec![Message {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        temperature: 0.7,
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("\"system\":\"You are OpenPRX\""));
}

#[test]
fn chat_response_deserializes() {
    let json = r#"{"content":[{"type":"text","text":"Hello there!"}]}"#;
    let resp: ChatResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.content.len(), 1);
    assert_eq!(resp.content[0].kind, "text");
    assert_eq!(resp.content[0].text.as_deref(), Some("Hello there!"));
}

#[test]
fn chat_response_empty_content() {
    let json = r#"{"content":[]}"#;
    let resp: ChatResponse = serde_json::from_str(json).unwrap();
    assert!(resp.content.is_empty());
}

#[test]
fn chat_response_multiple_blocks() {
    let json = r#"{"content":[{"type":"text","text":"First"},{"type":"text","text":"Second"}]}"#;
    let resp: ChatResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.content.len(), 2);
    assert_eq!(resp.content[0].text.as_deref(), Some("First"));
    assert_eq!(resp.content[1].text.as_deref(), Some("Second"));
}

#[test]
fn temperature_range_serializes() {
    for temp in [0.0, 0.5, 1.0, 2.0] {
        let req = ChatRequest {
            model: "claude-3-opus".to_string(),
            max_tokens: 4096,
            system: None,
            messages: vec![],
            temperature: temp,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(&format!("{temp}")));
    }
}

#[test]
fn detects_auth_from_jwt_shape() {
    let kind = detect_auth_kind("a.b.c", None);
    assert_eq!(kind, AnthropicAuthKind::Authorization);
}

#[test]
fn cache_control_serializes_correctly() {
    let cache = CacheControl::ephemeral();
    let json = serde_json::to_string(&cache).unwrap();
    assert_eq!(json, r#"{"type":"ephemeral"}"#);
}

#[test]
fn system_prompt_string_variant_serializes() {
    let prompt = SystemPrompt::String("You are a helpful assistant".to_string());
    let json = serde_json::to_string(&prompt).unwrap();
    assert_eq!(json, r#""You are a helpful assistant""#);
}

#[test]
fn system_prompt_blocks_variant_serializes() {
    let prompt = SystemPrompt::Blocks(vec![SystemBlock {
        block_type: "text".to_string(),
        text: "You are a helpful assistant".to_string(),
        cache_control: Some(CacheControl::ephemeral()),
    }]);
    let json = serde_json::to_string(&prompt).unwrap();
    assert!(json.contains(r#""type":"text""#));
    assert!(json.contains("You are a helpful assistant"));
    assert!(json.contains(r#""type":"ephemeral""#));
}

#[test]
fn system_prompt_blocks_without_cache_control() {
    let prompt = SystemPrompt::Blocks(vec![SystemBlock {
        block_type: "text".to_string(),
        text: "Short prompt".to_string(),
        cache_control: None,
    }]);
    let json = serde_json::to_string(&prompt).unwrap();
    assert!(json.contains("Short prompt"));
    assert!(!json.contains("cache_control"));
}

#[test]
fn native_content_text_without_cache_control() {
    let content = NativeContentOut::Text {
        text: "Hello".to_string(),
        cache_control: None,
    };
    let json = serde_json::to_string(&content).unwrap();
    assert!(json.contains(r#""type":"text""#));
    assert!(json.contains("Hello"));
    assert!(!json.contains("cache_control"));
}

#[test]
fn native_content_text_with_cache_control() {
    let content = NativeContentOut::Text {
        text: "Hello".to_string(),
        cache_control: Some(CacheControl::ephemeral()),
    };
    let json = serde_json::to_string(&content).unwrap();
    assert!(json.contains(r#""type":"text""#));
    assert!(json.contains("Hello"));
    assert!(json.contains(r#""cache_control":{"type":"ephemeral"}"#));
}

#[test]
fn native_content_tool_use_without_cache_control() {
    let content = NativeContentOut::ToolUse {
        id: "tool_123".to_string(),
        name: "get_weather".to_string(),
        input: serde_json::json!({"location": "San Francisco"}),
        cache_control: None,
    };
    let json = serde_json::to_string(&content).unwrap();
    assert!(json.contains(r#""type":"tool_use""#));
    assert!(json.contains("tool_123"));
    assert!(json.contains("get_weather"));
    assert!(!json.contains("cache_control"));
}

#[test]
fn native_content_tool_result_with_cache_control() {
    let content = NativeContentOut::ToolResult {
        tool_use_id: "tool_123".to_string(),
        content: "Result data".to_string(),
        cache_control: Some(CacheControl::ephemeral()),
    };
    let json = serde_json::to_string(&content).unwrap();
    assert!(json.contains(r#""type":"tool_result""#));
    assert!(json.contains("tool_123"));
    assert!(json.contains("Result data"));
    assert!(json.contains(r#""cache_control":{"type":"ephemeral"}"#));
}

#[test]
fn native_tool_spec_without_cache_control() {
    let schema = serde_json::json!({"type": "object"});
    let tool = NativeToolSpec {
        name: "get_weather",
        description: "Get weather info",
        input_schema: schema,
        cache_control: None,
    };
    let json = serde_json::to_string(&tool).unwrap();
    assert!(json.contains("get_weather"));
    assert!(!json.contains("cache_control"));
}

#[test]
fn native_tool_spec_with_cache_control() {
    let schema = serde_json::json!({"type": "object"});
    let tool = NativeToolSpec {
        name: "get_weather",
        description: "Get weather info",
        input_schema: schema,
        cache_control: Some(CacheControl::ephemeral()),
    };
    let json = serde_json::to_string(&tool).unwrap();
    assert!(json.contains("get_weather"));
    assert!(json.contains(r#""cache_control":{"type":"ephemeral"}"#));
}

#[test]
fn should_cache_system_small_prompt() {
    let small_prompt = "You are a helpful assistant.";
    assert!(!AnthropicProvider::should_cache_system(small_prompt));
}

#[test]
fn should_cache_system_large_prompt() {
    let large_prompt = "a".repeat(3073); // Just over 3072 bytes
    assert!(AnthropicProvider::should_cache_system(&large_prompt));
}

#[test]
fn should_cache_system_boundary() {
    let boundary_prompt = "a".repeat(3072); // Exactly 3072 bytes
    assert!(!AnthropicProvider::should_cache_system(&boundary_prompt));

    let over_boundary = "a".repeat(3073);
    assert!(AnthropicProvider::should_cache_system(&over_boundary));
}

#[test]
fn should_cache_conversation_short() {
    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: "System prompt".to_string(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: "Hello".to_string(),
        },
        ChatMessage {
            role: "assistant".to_string(),
            content: "Hi".to_string(),
        },
    ];
    // Only 2 non-system messages
    assert!(!AnthropicProvider::should_cache_conversation(&messages));
}

#[test]
fn should_cache_conversation_long() {
    let mut messages = vec![ChatMessage {
        role: "system".to_string(),
        content: "System prompt".to_string(),
    }];
    // Add 5 non-system messages
    for i in 0..5 {
        messages.push(ChatMessage {
            role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
            content: format!("Message {i}"),
        });
    }
    assert!(AnthropicProvider::should_cache_conversation(&messages));
}

#[test]
fn should_cache_conversation_boundary() {
    let mut messages = vec![];
    // Add exactly 4 non-system messages
    for i in 0..4 {
        messages.push(ChatMessage {
            role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
            content: format!("Message {i}"),
        });
    }
    assert!(!AnthropicProvider::should_cache_conversation(&messages));

    // Add one more to cross boundary
    messages.push(ChatMessage {
        role: "user".to_string(),
        content: "One more".to_string(),
    });
    assert!(AnthropicProvider::should_cache_conversation(&messages));
}

#[test]
fn apply_cache_to_last_message_text() {
    let mut messages = vec![NativeMessage {
        role: "user".to_string(),
        content: vec![NativeContentOut::Text {
            text: "Hello".to_string(),
            cache_control: None,
        }],
    }];

    AnthropicProvider::apply_cache_to_last_message(&mut messages);

    match &messages[0].content[0] {
        NativeContentOut::Text { cache_control, .. } => {
            assert!(cache_control.is_some());
        }
        _ => panic!("Expected Text variant"),
    }
}

#[test]
fn apply_cache_to_last_message_tool_result() {
    let mut messages = vec![NativeMessage {
        role: "user".to_string(),
        content: vec![NativeContentOut::ToolResult {
            tool_use_id: "tool_123".to_string(),
            content: "Result".to_string(),
            cache_control: None,
        }],
    }];

    AnthropicProvider::apply_cache_to_last_message(&mut messages);

    match &messages[0].content[0] {
        NativeContentOut::ToolResult { cache_control, .. } => {
            assert!(cache_control.is_some());
        }
        _ => panic!("Expected ToolResult variant"),
    }
}

#[test]
fn apply_cache_to_last_message_does_not_affect_tool_use() {
    let mut messages = vec![NativeMessage {
        role: "assistant".to_string(),
        content: vec![NativeContentOut::ToolUse {
            id: "tool_123".to_string(),
            name: "get_weather".to_string(),
            input: serde_json::json!({}),
            cache_control: None,
        }],
    }];

    AnthropicProvider::apply_cache_to_last_message(&mut messages);

    // ToolUse should not be affected
    match &messages[0].content[0] {
        NativeContentOut::ToolUse { cache_control, .. } => {
            assert!(cache_control.is_none());
        }
        _ => panic!("Expected ToolUse variant"),
    }
}

#[test]
fn apply_cache_empty_messages() {
    let mut messages = vec![];
    AnthropicProvider::apply_cache_to_last_message(&mut messages);
    // Should not panic
    assert!(messages.is_empty());
}

#[tokio::test]
async fn root_combinator_tools_are_refused_before_native_transport() {
    let probe = crate::providers::credential_probe::CredentialProbe::start().await;
    let provider = AnthropicProvider::with_base_url(Some("prx-test-credential"), Some(probe.base_url().as_str()));
    let tools = vec![ToolSpec {
        name: "mcp_fixture".into(),
        description: "fixture".into(),
        parameters: serde_json::json!({"type":"object","oneOf":[{"required":["a"]},{"required":["b"]}]}),
    }];
    let Err(error) = provider.convert_tools(Some(&tools)) else {
        panic!("test: schema must be refused");
    };
    assert!(error.downcast_ref::<super::super::ProviderCapabilityError>().is_some());
    for expected in ["mcp_fixture", "oneOf", "no_root_combinators", "tool_tiering"] {
        assert!(error.to_string().contains(expected), "{error}");
    }
    let messages = vec![ChatMessage::user("fixture")];
    let error = provider
        .chat(
            ProviderChatRequest {
                messages: &messages,
                tools: Some(&tools),
            },
            "fixture",
            0.0,
        )
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<super::super::ProviderCapabilityError>().is_some());
    let legacy = tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function", "function": {
                    "name": tool.name, "description": tool.description, "parameters": tool.parameters,
                }
            })
        })
        .collect::<Vec<_>>();
    let error = provider
        .chat_with_tools(&messages, &legacy, "fixture", 0.0)
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<super::super::ProviderCapabilityError>().is_some());
    assert!(error.to_string().contains("mcp_fixture"));
    let streamed = provider
        .stream_chat_with_history(
            &messages,
            "fixture",
            0.0,
            StreamOptions {
                tools: Some(tools.clone()),
                ..StreamOptions::default()
            },
        )
        .collect::<Vec<_>>()
        .await;
    assert_eq!(streamed.len(), 1);
    assert!(
        streamed[0]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("no_root_combinators")
    );
    assert!(probe.requests().is_empty());
    assert_eq!(
        provider.tool_schema_dialect(),
        crate::tools::ToolSchemaDialect::NoRootCombinators
    );
}

#[test]
fn convert_tools_adds_cache_to_last_tool() {
    let tools = vec![
        ToolSpec {
            name: "tool1".to_string(),
            description: "First tool".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        },
        ToolSpec {
            name: "tool2".to_string(),
            description: "Second tool".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        },
    ];

    let provider = AnthropicProvider::new(Some("test-key"));
    let native_tools = provider
        .convert_tools(Some(&tools))
        .expect("test: tools convert")
        .expect("test: tools present");

    assert_eq!(native_tools.len(), 2);
    assert!(native_tools[0].cache_control.is_none());
    assert!(native_tools[1].cache_control.is_some());
}

#[test]
fn convert_tools_single_tool_gets_cache() {
    let tools = vec![ToolSpec {
        name: "tool1".to_string(),
        description: "Only tool".to_string(),
        parameters: serde_json::json!({"type": "object"}),
    }];

    let provider = AnthropicProvider::new(Some("test-key"));
    let native_tools = provider
        .convert_tools(Some(&tools))
        .expect("test: tools convert")
        .expect("test: tools present");

    assert_eq!(native_tools.len(), 1);
    assert!(native_tools[0].cache_control.is_some());
}

#[test]
fn convert_messages_small_system_prompt() {
    let messages = vec![ChatMessage {
        role: "system".to_string(),
        content: "Short system prompt".to_string(),
    }];

    let (system_prompt, _) = AnthropicProvider::convert_messages(&messages);

    match system_prompt.unwrap() {
        SystemPrompt::String(s) => {
            assert_eq!(s, "Short system prompt");
        }
        SystemPrompt::Blocks(_) => panic!("Expected String variant for small prompt"),
    }
}

#[test]
fn convert_messages_large_system_prompt() {
    let large_content = "a".repeat(3073);
    let messages = vec![ChatMessage {
        role: "system".to_string(),
        content: large_content.clone(),
    }];

    let (system_prompt, _) = AnthropicProvider::convert_messages(&messages);

    match system_prompt.unwrap() {
        SystemPrompt::Blocks(blocks) => {
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].text, large_content);
            assert!(blocks[0].cache_control.is_some());
        }
        SystemPrompt::String(_) => panic!("Expected Blocks variant for large prompt"),
    }
}

#[test]
fn backward_compatibility_native_chat_request() {
    // Test that requests without cache_control serialize identically to old format
    let req = NativeChatRequest {
        model: "claude-3-opus".to_string(),
        max_tokens: 4096,
        system: Some(SystemPrompt::String("System".to_string())),
        messages: vec![NativeMessage {
            role: "user".to_string(),
            content: vec![NativeContentOut::Text {
                text: "Hello".to_string(),
                cache_control: None,
            }],
        }],
        temperature: 0.7,
        tools: None,
    };

    let json = serde_json::to_string(&req).unwrap();
    assert!(!json.contains("cache_control"));
    assert!(json.contains(r#""system":"System""#));
}

#[tokio::test]
async fn warmup_without_key_is_noop() {
    let provider = AnthropicProvider::new(None);
    let result = provider.warmup().await;
    assert!(result.is_ok());
}

#[test]
fn convert_messages_preserves_multi_turn_history() {
    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: "You are helpful.".to_string(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: "gen a 2 sum in golang".to_string(),
        },
        ChatMessage {
            role: "assistant".to_string(),
            content: "```go\nfunc twoSum(nums []int) {}\n```".to_string(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: "what's meaning of make here?".to_string(),
        },
    ];

    let (system, native_msgs) = AnthropicProvider::convert_messages(&messages);

    // System prompt extracted
    assert!(system.is_some());
    // All 3 non-system messages preserved in order
    assert_eq!(native_msgs.len(), 3);
    assert_eq!(native_msgs[0].role, "user");
    assert_eq!(native_msgs[1].role, "assistant");
    assert_eq!(native_msgs[2].role, "user");
}

#[test]
fn convert_messages_splits_user_text_and_image_blocks() {
    let messages = vec![ChatMessage::user("Look [IMAGE:data:image/png;base64,abcd==] now")];

    let (_, native_msgs) = AnthropicProvider::convert_messages(&messages);

    assert_eq!(native_msgs.len(), 1);
    assert_eq!(native_msgs[0].role, "user");
    assert_eq!(native_msgs[0].content.len(), 3);
    match &native_msgs[0].content[0] {
        NativeContentOut::Text { text, .. } => assert_eq!(text, "Look "),
        other => panic!("expected text block, got {other:?}"),
    }
    match &native_msgs[0].content[1] {
        NativeContentOut::Image { source } => {
            assert_eq!(source.source_type, "base64");
            assert_eq!(source.media_type, "image/png");
            assert_eq!(source.data, "abcd==");
        }
        other => panic!("expected image block, got {other:?}"),
    }
    match &native_msgs[0].content[2] {
        NativeContentOut::Text { text, .. } => assert_eq!(text, " now"),
        other => panic!("expected text block, got {other:?}"),
    }
}

#[test]
fn parse_anthropic_image_source_strips_whitespace_from_base64_payload() {
    let source = AnthropicProvider::parse_anthropic_image_source("data:image/png;base64,Zm9v\n YmFy\t")
        .expect("expected valid image source");

    assert_eq!(source.media_type, "image/png");
    assert_eq!(source.data, "Zm9vYmFy");
}

/// Integration test: spin up a mock Anthropic API server, call chat_with_tools
/// with a multi-turn conversation + tools, and verify the request body contains
/// ALL conversation turns and native tool definitions.
#[tokio::test]
async fn chat_with_tools_sends_full_history_and_native_tools() {
    use axum::{Json, Router, routing::post};
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    // Captured request body for assertion
    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();

    let app = Router::new().route(
        "/v1/messages",
        post(move |Json(body): Json<serde_json::Value>| {
            let cap = captured_clone.clone();
            async move {
                *cap.lock().unwrap() = Some(body);
                // Return a minimal valid Anthropic response
                Json(serde_json::json!({
                    "id": "msg_test",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "The make function creates a map."}],
                    "model": "claude-opus-4-6",
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 100, "output_tokens": 20}
                }))
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Create provider pointing at mock server
    let provider = AnthropicProvider::with_base_url(Some("test-key"), Some(&format!("http://{addr}")));

    // Multi-turn conversation: system → user (Go code) → assistant (code response) → user (follow-up)
    let messages = vec![
        ChatMessage::system("You are a helpful assistant."),
        ChatMessage::user("gen a 2 sum in golang"),
        ChatMessage::assistant(
            "```go\nfunc twoSum(nums []int, target int) []int {\n    m := make(map[int]int)\n    for i, n := range nums {\n        if j, ok := m[target-n]; ok {\n            return []int{j, i}\n        }\n        m[n] = i\n    }\n    return nil\n}\n```",
        ),
        ChatMessage::user("what's meaning of make here?"),
    ];

    let tools = vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "shell",
            "description": "Run a shell command",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string"}
                },
                "required": ["command"]
            }
        }
    })];

    let result = provider
        .chat_with_tools(&messages, &tools, "claude-opus-4-6", 0.7)
        .await;
    assert!(result.is_ok(), "chat_with_tools failed: {:?}", result.err());

    let body = captured.lock().unwrap().take().expect("No request captured");

    // Verify system prompt extracted to top-level field
    let system = &body["system"];
    assert!(
        system.to_string().contains("helpful assistant"),
        "System prompt missing: {system}"
    );

    // Verify ALL conversation turns present in messages array
    let msgs = body["messages"].as_array().expect("messages not an array");
    assert_eq!(
        msgs.len(),
        3,
        "Expected 3 messages (2 user + 1 assistant), got {}",
        msgs.len()
    );

    // Turn 1: user with Go request
    assert_eq!(msgs[0]["role"], "user");
    let turn1_text = msgs[0]["content"].to_string();
    assert!(turn1_text.contains("2 sum"), "Turn 1 missing Go request: {turn1_text}");

    // Turn 2: assistant with Go code
    assert_eq!(msgs[1]["role"], "assistant");
    let turn2_text = msgs[1]["content"].to_string();
    assert!(
        turn2_text.contains("make(map[int]int)"),
        "Turn 2 missing Go code: {turn2_text}"
    );

    // Turn 3: user follow-up
    assert_eq!(msgs[2]["role"], "user");
    let turn3_text = msgs[2]["content"].to_string();
    assert!(
        turn3_text.contains("meaning of make"),
        "Turn 3 missing follow-up: {turn3_text}"
    );

    // Verify native tools are present
    let api_tools = body["tools"].as_array().expect("tools not an array");
    assert_eq!(api_tools.len(), 1);
    assert_eq!(api_tools[0]["name"], "shell");
    assert!(api_tools[0]["input_schema"].is_object(), "Missing input_schema");

    server_handle.abort();
}

// -----------------------------------------------------------------
// Reasoning/thinking block separation tests
// -----------------------------------------------------------------

#[test]
fn parse_native_response_routes_thinking_block_to_reasoning_field() {
    // Anthropic extended-thinking returns a content block of type "thinking"
    // with the chain-of-thought in the `thinking` field. It must NOT be
    // mixed into the visible `text` payload.
    let json = r#"{
            "content": [
                {"type": "thinking", "thinking": "Let me reason about this..."},
                {"type": "text", "text": "Final answer."}
            ]
        }"#;
    let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
    let parsed = AnthropicProvider::parse_native_response(resp);

    // Visible text only — no thinking content leaks in.
    assert_eq!(parsed.text.as_deref(), Some("Final answer."));
    assert_eq!(parsed.reasoning_content.as_deref(), Some("Let me reason about this..."));
    assert!(parsed.tool_calls.is_empty());
}

#[test]
fn parse_native_response_handles_thinking_only_without_text() {
    let json = r#"{
            "content": [
                {"type": "thinking", "thinking": "Internal monologue only."}
            ]
        }"#;
    let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
    let parsed = AnthropicProvider::parse_native_response(resp);

    // No visible text at all.
    assert_eq!(parsed.text, None);
    assert_eq!(parsed.reasoning_content.as_deref(), Some("Internal monologue only."));
}

#[test]
fn parse_native_response_with_thinking_and_tool_use() {
    let json = r#"{
            "content": [
                {"type": "thinking", "thinking": "I should call shell."},
                {"type": "tool_use", "id": "tu_1", "name": "shell", "input": {"command": "ls"}}
            ]
        }"#;
    let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
    let parsed = AnthropicProvider::parse_native_response(resp);

    assert_eq!(parsed.text, None);
    assert_eq!(parsed.reasoning_content.as_deref(), Some("I should call shell."));
    assert_eq!(parsed.tool_calls.len(), 1);
    assert_eq!(parsed.tool_calls[0].name, "shell");
}

#[test]
fn parse_native_response_no_thinking_keeps_reasoning_none() {
    let json = r#"{
            "content": [
                {"type": "text", "text": "Just an answer."}
            ]
        }"#;
    let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
    let parsed = AnthropicProvider::parse_native_response(resp);

    assert_eq!(parsed.text.as_deref(), Some("Just an answer."));
    assert_eq!(parsed.reasoning_content, None);
}

#[test]
fn anthropic_non_streaming_response_usage_maps_reported() {
    let json = r#"{
            "content": [{"type": "text", "text": "Just an answer."}],
            "usage": {
                "input_tokens": 64,
                "output_tokens": 11,
                "cache_creation_input_tokens": 20,
                "cache_read_input_tokens": 300
            }
        }"#;
    let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
    let usage = resp.usage.expect("usage expected").into_reported();

    assert_eq!(usage.source, crate::llm::route_decision::TokenUsageSource::Reported);
    assert_eq!(usage.prompt_tokens, Some(384));
    assert_eq!(usage.completion_tokens, Some(11));
    assert_eq!(usage.total_tokens, Some(395));
    assert_eq!(usage.cache_creation_input_tokens, Some(20));
    assert_eq!(usage.cache_read_input_tokens, Some(300));
}

#[test]
fn anthropic_sse_usage_events_parse_reported_tokens() {
    let start = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":1,\"cache_creation_input_tokens\":25,\"cache_read_input_tokens\":400}}}\n\n"
    );
    let delta = concat!(
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":23}}\n\n"
    );

    let start_usage = match parse_anthropic_sse_record(start).unwrap().expect("message_start event") {
        AnthropicEvent::MessageStart(usage) => usage,
        other => panic!("expected message_start usage, got {other:?}"),
    };
    let delta_usage = match parse_anthropic_sse_record(delta).unwrap().expect("message_delta event") {
        AnthropicEvent::MessageDelta(usage, _) => usage,
        other => panic!("expected message_delta usage, got {other:?}"),
    };

    assert_eq!(start_usage.input_tokens, Some(100));
    assert_eq!(start_usage.output_tokens, Some(1));
    assert_eq!(start_usage.cache_creation_input_tokens, Some(25));
    assert_eq!(start_usage.cache_read_input_tokens, Some(400));
    assert_eq!(delta_usage.input_tokens, None);
    assert_eq!(delta_usage.output_tokens, Some(23));

    let reported = AnthropicUsage {
        input_tokens: start_usage.input_tokens,
        output_tokens: delta_usage.output_tokens,
        cache_creation_input_tokens: start_usage.cache_creation_input_tokens,
        cache_read_input_tokens: start_usage.cache_read_input_tokens,
    }
    .into_reported();
    assert_eq!(reported.source, crate::llm::route_decision::TokenUsageSource::Reported);
    assert_eq!(reported.prompt_tokens, Some(525));
    assert_eq!(reported.completion_tokens, Some(23));
    assert_eq!(reported.total_tokens, Some(548));
    assert_eq!(reported.cache_creation_input_tokens, Some(25));
    assert_eq!(reported.cache_read_input_tokens, Some(400));
}

// ─── S3 T3-2-A: SSE tool_use incremental streaming ──────────────────────

/// Build a `tool_use` `content_block_start` payload.
fn tool_use_start_block(id: &str, name: &str) -> AnthropicSseContentBlock {
    AnthropicSseContentBlock {
        kind: "tool_use".into(),
        id: Some(id.into()),
        name: Some(name.into()),
        text: None,
    }
}

/// Build an `input_json_delta` SSE delta.
fn input_json_delta(partial: &str) -> AnthropicSseDelta {
    AnthropicSseDelta {
        kind: "input_json_delta".into(),
        text: None,
        thinking: None,
        partial_json: Some(partial.into()),
    }
}

/// Mock the full SSE sequence for a single tool_use block (start +
/// multiple input_json_delta + stop) and verify provider emits:
/// 1) an initial `Streaming { arguments_delta: Some("") }` chunk,
/// 2) one `Streaming` chunk per partial_json with the verbatim fragment,
/// 3) a final `Completed` chunk whose `args` equals Σ fragments.
#[test]
fn test_tool_use_streaming_emits_incremental_chunks() {
    let mut state = AnthropicStreamState::default();

    // BlockStart → initial Streaming chunk
    let initial = state
        .on_content_block_start(0, tool_use_start_block("tu_42", "shell"))
        .expect("BlockStart for tool_use must emit an initial Streaming chunk");
    assert_eq!(initial.id, "tu_42");
    assert_eq!(initial.name, "shell");
    assert_eq!(initial.index, 0);
    assert_eq!(initial.status, ToolCallChunkStatus::Streaming);
    assert_eq!(initial.arguments_delta.as_deref(), Some(""));
    assert!(initial.args.is_empty(), "Streaming.args MUST be empty");

    // Three input_json_delta events → three Streaming chunks
    let fragments = [r#"{"command":"#, r#" "ls -"#, r#"la"}"#];
    let mut streamed_chunks: Vec<ToolCallChunk> = Vec::new();
    for frag in &fragments {
        let outcome = state
            .on_content_block_delta(0, input_json_delta(frag))
            .expect("input_json_delta must produce a ToolDelta outcome");
        match outcome {
            DeltaOutcome::ToolDelta(chunk) => {
                assert_eq!(chunk.id, "tu_42");
                assert_eq!(chunk.name, "shell");
                assert_eq!(chunk.index, 0);
                assert_eq!(chunk.status, ToolCallChunkStatus::Streaming);
                assert!(chunk.args.is_empty(), "Streaming.args MUST be empty");
                assert_eq!(chunk.arguments_delta.as_deref(), Some(*frag));
                streamed_chunks.push(chunk);
            }
            other => panic!("expected ToolDelta, got {other:?}"),
        }
    }
    assert_eq!(streamed_chunks.len(), 3);

    // BlockStop → terminal Completed chunk with full JSON
    let completed = state
        .on_content_block_stop(0)
        .expect("BlockStop for tool_use must emit a Completed chunk");
    assert_eq!(completed.id, "tu_42");
    assert_eq!(completed.name, "shell");
    assert_eq!(completed.index, 0);
    assert_eq!(completed.status, ToolCallChunkStatus::Completed);
    assert!(
        completed.arguments_delta.is_none(),
        "Completed.arguments_delta MUST be None"
    );

    // T3-0 invariant: Σ Streaming.arguments_delta == Completed.args.
    let aggregated: String = streamed_chunks
        .iter()
        .filter_map(|c| c.arguments_delta.as_deref())
        .collect();
    assert_eq!(aggregated, completed.args);
    assert_eq!(completed.args, r#"{"command": "ls -la"}"#);

    // The block-level entry must be drained at BlockStop.
    assert!(state.blocks.is_empty(), "blocks map must drain at BlockStop");
}

/// Stream interruption: BlockStop never arrives. Provider must not panic;
/// the per-block entry stays in the map, no terminal Completed is emitted,
/// and a follow-up second tool_use still gets a fresh stable `index`.
#[test]
fn test_tool_use_streaming_interrupted_handles_partial_json() {
    let mut state = AnthropicStreamState::default();

    // Tool 1: start + one partial fragment, then "stream ends" — no stop.
    let initial1 = state
        .on_content_block_start(0, tool_use_start_block("tu_a", "search"))
        .expect("initial Streaming chunk");
    assert_eq!(initial1.index, 0);

    let frag_out = state
        .on_content_block_delta(0, input_json_delta(r#"{"query":"#))
        .expect("partial fragment must surface");
    match frag_out {
        DeltaOutcome::ToolDelta(chunk) => {
            assert_eq!(chunk.index, 0);
            assert_eq!(chunk.arguments_delta.as_deref(), Some(r#"{"query":"#));
        }
        other => panic!("expected ToolDelta, got {other:?}"),
    }

    // Buffer is still alive (no Completed flushed yet).
    assert_eq!(state.blocks.len(), 1, "interrupted tool buffer must persist");

    // A second tool_use must still allocate a *fresh* index (1) — the
    // ordinal counter is independent of orphan buffers.
    let initial2 = state
        .on_content_block_start(1, tool_use_start_block("tu_b", "edit"))
        .expect("second initial Streaming chunk");
    assert_eq!(initial2.index, 1);
    assert_eq!(initial2.id, "tu_b");

    // Calling stop on a content_block that was never started must not panic
    // and must return None.
    assert!(state.on_content_block_stop(99).is_none());

    // Calling stop on the second tool flushes it cleanly with empty-args
    // fallback (`{}`) even though no input_json_delta arrived for it.
    let completed2 = state.on_content_block_stop(1).expect("Completed for tool 2");
    assert_eq!(completed2.index, 1);
    assert_eq!(completed2.args, "{}");
    assert_eq!(completed2.status, ToolCallChunkStatus::Completed);
}

// -----------------------------------------------------------------
// STREAM-TOOLS: native tools on the streaming request
// -----------------------------------------------------------------

/// One Anthropic SSE record (`event:` + `data:` + blank line).
fn anthropic_sse_record(event: &str, data: &serde_json::Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// The streamed `tool_use` reply the real Messages API sends for a
/// `fixture_write({"value":"x"})` call, split across two JSON fragments.
fn anthropic_streamed_tool_use_body() -> String {
    [
        anthropic_sse_record(
            "message_start",
            &serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0}}}),
        ),
        anthropic_sse_record(
            "content_block_start",
            &serde_json::json!({"type":"content_block_start","index":0,
                    "content_block":{"type":"tool_use","id":"toolu_s1","name":"fixture_write","input":{}}}),
        ),
        anthropic_sse_record(
            "content_block_delta",
            &serde_json::json!({"type":"content_block_delta","index":0,
                    "delta":{"type":"input_json_delta","partial_json":"{\"value\":"}}),
        ),
        anthropic_sse_record(
            "content_block_delta",
            &serde_json::json!({"type":"content_block_delta","index":0,
                    "delta":{"type":"input_json_delta","partial_json":"\"x\"}"}}),
        ),
        anthropic_sse_record(
            "content_block_stop",
            &serde_json::json!({"type":"content_block_stop","index":0}),
        ),
        anthropic_sse_record(
            "message_delta",
            &serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}),
        ),
        anthropic_sse_record("message_stop", &serde_json::json!({"type":"message_stop"})),
    ]
    .concat()
}

/// Loopback `/v1/messages` stub that records every request body and answers
/// with `body` as `text/event-stream`.
async fn spawn_anthropic_sse_stub(
    body: String,
) -> (
    String,
    std::sync::Arc<Mutex<Vec<serde_json::Value>>>,
    tokio::task::JoinHandle<()>,
) {
    use axum::{Json, Router, http::header, routing::post};
    use tokio::net::TcpListener;

    let captured: std::sync::Arc<Mutex<Vec<serde_json::Value>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    let app = Router::new().route(
        "/v1/messages",
        post(move |Json(request): Json<serde_json::Value>| {
            let sink = sink.clone();
            let body = body.clone();
            async move {
                sink.lock().push(request);
                ([(header::CONTENT_TYPE, "text/event-stream")], body)
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test: bind loopback");
    let addr = listener.local_addr().expect("test: loopback addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), captured, handle)
}

/// A tool whose schema carries `$defs` + `$ref`, so the Anthropic dialect's
/// lowering is observable on the wire.
fn fixture_write_spec() -> ToolSpec {
    ToolSpec {
        name: "fixture_write".to_string(),
        description: "Write a fixture value".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "$defs": {"Value": {"type": "string", "description": "the value"}},
            "properties": {"value": {"$ref": "#/$defs/Value"}},
            "required": ["value"]
        }),
    }
}

#[tokio::test]
async fn anthropic_streaming_request_carries_lowered_native_tools_and_surfaces_the_call() {
    let (base, captured, server) = spawn_anthropic_sse_stub(anthropic_streamed_tool_use_body()).await;
    let provider = AnthropicProvider::with_base_url(Some("test-key"), Some(&base));
    let messages = vec![ChatMessage::user("write x")];

    let chunks: Vec<StreamChunk> = provider
        .stream_chat_with_history(
            &messages,
            "claude-test",
            0.0,
            StreamOptions::new(true).with_tools(vec![fixture_write_spec()]),
        )
        .map(|chunk| chunk.expect("test: stream chunk"))
        .collect()
        .await;
    server.abort();

    let bodies = captured.lock().clone();
    assert_eq!(bodies.len(), 1, "exactly one streaming request");
    let body = &bodies[0];
    assert_eq!(body["stream"], true);
    let tools = body["tools"]
        .as_array()
        .expect("test: streaming request must carry tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "fixture_write");
    assert_eq!(tools[0]["description"], "Write a fixture value");
    let schema = tools[0]["input_schema"].to_string();
    assert!(
        !schema.contains("$ref"),
        "schema must be lowered through the Anthropic dialect, got {schema}"
    );
    assert_eq!(tools[0]["input_schema"]["properties"]["value"]["type"], "string");
    assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");

    let completed: Vec<&ToolCallChunk> = chunks
        .iter()
        .flat_map(|c| c.tool_calls.iter())
        .filter(|c| c.status == ToolCallChunkStatus::Completed)
        .collect();
    assert_eq!(completed.len(), 1, "exactly one completed tool call");
    assert_eq!(completed[0].id, "toolu_s1");
    assert_eq!(completed[0].name, "fixture_write");
    let args: serde_json::Value = serde_json::from_str(&completed[0].args).expect("test: args are JSON");
    assert_eq!(args, serde_json::json!({"value": "x"}));
    assert!(
        chunks.last().is_some_and(|c| c.is_final),
        "stream ends with a final chunk"
    );
}

#[tokio::test]
async fn anthropic_streaming_request_without_tools_omits_the_field() {
    let text_body = [
        anthropic_sse_record(
            "content_block_start",
            &serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        ),
        anthropic_sse_record(
            "content_block_delta",
            &serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
        ),
        anthropic_sse_record("message_stop", &serde_json::json!({"type":"message_stop"})),
    ]
    .concat();
    let (base, captured, server) = spawn_anthropic_sse_stub(text_body).await;
    let provider = AnthropicProvider::with_base_url(Some("test-key"), Some(&base));
    let messages = vec![ChatMessage::user("hi")];

    let text: String = provider
        .stream_chat_with_history(&messages, "claude-test", 0.0, StreamOptions::new(true))
        .map(|chunk| chunk.expect("test: stream chunk").delta)
        .collect::<Vec<_>>()
        .await
        .concat();
    server.abort();

    assert_eq!(text, "hi");
    let bodies = captured.lock().clone();
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0].get("tools").is_none(),
        "no tools offered => no tools key, got {}",
        bodies[0]
    );
}

/// STREAM-S3 (DC-7): a streamed multi-step conversation carries the same
/// cache breakpoint on its last message as the buffered path, so a tool
/// loop does not re-send its whole history uncached on every step.
#[tokio::test]
async fn anthropic_streaming_request_caches_a_long_conversation_like_the_buffered_one() {
    let text_body = [
        anthropic_sse_record(
            "content_block_start",
            &serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        ),
        anthropic_sse_record(
            "content_block_delta",
            &serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
        ),
        anthropic_sse_record("message_stop", &serde_json::json!({"type":"message_stop"})),
    ]
    .concat();
    let (base, captured, server) = spawn_anthropic_sse_stub(text_body).await;
    let provider = AnthropicProvider::with_base_url(Some("test-key"), Some(&base));
    let messages = vec![
        ChatMessage::user("step 1"),
        ChatMessage::assistant("answer 1"),
        ChatMessage::user("step 2"),
        ChatMessage::assistant("answer 2"),
        ChatMessage::user("step 3"),
    ];
    assert!(
        AnthropicProvider::should_cache_conversation(&messages),
        "test: the buffered path caches this conversation"
    );

    let text: String = provider
        .stream_chat_with_history(&messages, "claude-test", 0.0, StreamOptions::new(true))
        .map(|chunk| chunk.expect("test: stream chunk").delta)
        .collect::<Vec<_>>()
        .await
        .concat();
    server.abort();

    assert_eq!(text, "ok");
    let bodies = captured.lock().clone();
    assert_eq!(bodies.len(), 1);
    let last_block = bodies[0]["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["content"].as_array())
        .and_then(|content| content.last())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        last_block["cache_control"],
        serde_json::json!({"type": "ephemeral"}),
        "the last message must carry the cache breakpoint: {}",
        bodies[0]
    );
    let short = vec![ChatMessage::user("hi")];
    assert!(!AnthropicProvider::should_cache_conversation(&short));
}

// -----------------------------------------------------------------
// STREAM-f: a half-received tool call must fail the stream, never
// surface as a completed call
// -----------------------------------------------------------------

/// `message_start`, a `tool_use` block for `fixture_write` and the first
/// half of its arguments: everything a stream has sent when it is cut in
/// the middle of a call.
fn anthropic_tool_use_prefix() -> String {
    [
        anthropic_sse_record(
            "message_start",
            &serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0}}}),
        ),
        anthropic_sse_record(
            "content_block_start",
            &serde_json::json!({"type":"content_block_start","index":0,
                    "content_block":{"type":"tool_use","id":"toolu_s1","name":"fixture_write","input":{}}}),
        ),
        anthropic_sse_record(
            "content_block_delta",
            &serde_json::json!({"type":"content_block_delta","index":0,
                    "delta":{"type":"input_json_delta","partial_json":"{\"value\":"}}),
        ),
    ]
    .concat()
}

/// Drive one streaming request against `body` with `fixture_write` offered.
async fn anthropic_stream_results(body: String) -> Vec<StreamResult<StreamChunk>> {
    let (base, _captured, server) = spawn_anthropic_sse_stub(body).await;
    let provider = AnthropicProvider::with_base_url(Some("test-key"), Some(&base));
    let messages = vec![ChatMessage::user("write x")];
    let results = provider
        .stream_chat_with_history(
            &messages,
            "claude-test",
            0.0,
            StreamOptions::new(true).with_tools(vec![fixture_write_spec()]),
        )
        .collect()
        .await;
    server.abort();
    results
}

/// The stream must end in an error, and nothing may follow that error.
fn assert_stream_ends_in_error(results: &[StreamResult<StreamChunk>]) -> &StreamError {
    let last = results.last().expect("test: the stream yielded something");
    let error = last.as_ref().err().unwrap_or_else(|| {
        panic!(
            "the stream must end in an error, got {:?}",
            results
                .iter()
                .map(|r| r.as_ref().map(|c| (&c.delta, &c.tool_calls, c.is_final)))
                .collect::<Vec<_>>()
        )
    });
    assert!(
        results.iter().filter(|r| r.is_err()).count() == 1,
        "exactly one error ends the stream"
    );
    error
}

fn completed_calls(results: &[StreamResult<StreamChunk>]) -> Vec<&ToolCallChunk> {
    results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .flat_map(|c| c.tool_calls.iter())
        .filter(|c| c.status == ToolCallChunkStatus::Completed)
        .collect()
}

#[tokio::test]
async fn anthropic_stream_cut_mid_tool_arguments_ends_in_an_error() {
    let results = anthropic_stream_results(anthropic_tool_use_prefix()).await;

    let error = assert_stream_ends_in_error(&results);
    assert!(matches!(error, StreamError::InvalidSse(_)), "got {error:?}");
    assert!(error.to_string().contains("message_stop"), "got {error}");
    assert!(
        completed_calls(&results).is_empty(),
        "a call whose block never closed must not surface as completed"
    );
    assert!(
        !results.iter().filter_map(|r| r.as_ref().ok()).any(|c| c.is_final),
        "a truncated stream must not report a final chunk"
    );
}

#[tokio::test]
async fn anthropic_stream_text_cut_before_message_stop_ends_in_an_error() {
    let body = [
            anthropic_sse_record(
                "content_block_start",
                &serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            anthropic_sse_record(
                "content_block_delta",
                &serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half an ans"}}),
            ),
        ]
        .concat();
    let results = anthropic_stream_results(body).await;
    let error = assert_stream_ends_in_error(&results);
    assert!(error.to_string().contains("message_stop"), "got {error}");
}

#[tokio::test]
async fn anthropic_stream_error_event_ends_in_a_provider_error() {
    let body = [
        anthropic_tool_use_prefix(),
        anthropic_sse_record(
            "error",
            &serde_json::json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
        ),
    ]
    .concat();
    let results = anthropic_stream_results(body).await;

    let error = assert_stream_ends_in_error(&results);
    assert!(matches!(error, StreamError::Provider(_)), "got {error:?}");
    let message = error.to_string();
    assert!(
        message.contains("overloaded_error"),
        "the upstream error type is kept: {message}"
    );
    assert!(
        message.contains("Overloaded"),
        "the upstream message is kept: {message}"
    );
    assert!(completed_calls(&results).is_empty(), "expected an empty value");
}

#[tokio::test]
async fn anthropic_stream_max_tokens_inside_a_tool_call_ends_in_an_error() {
    let body = [
            anthropic_tool_use_prefix(),
            anthropic_sse_record(
                "content_block_stop",
                &serde_json::json!({"type":"content_block_stop","index":0}),
            ),
            anthropic_sse_record(
                "message_delta",
                &serde_json::json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":4096}}),
            ),
            anthropic_sse_record("message_stop", &serde_json::json!({"type":"message_stop"})),
        ]
        .concat();
    let results = anthropic_stream_results(body).await;

    let error = assert_stream_ends_in_error(&results);
    let message = error.to_string();
    assert!(message.contains("output cap"), "got {message}");
    assert!(
        !results.iter().filter_map(|r| r.as_ref().ok()).any(|c| c.is_final),
        "a truncated tool call must not end in a final chunk"
    );
}

/// `max_tokens` after a complete tool call and a truncated *text* block is
/// an ordinary truncated answer, not a truncated call.
#[tokio::test]
async fn anthropic_stream_max_tokens_inside_text_stays_a_normal_end() {
    let body = [
            anthropic_sse_record(
                "content_block_start",
                &serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            anthropic_sse_record(
                "content_block_delta",
                &serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"long answ"}}),
            ),
            anthropic_sse_record(
                "content_block_stop",
                &serde_json::json!({"type":"content_block_stop","index":0}),
            ),
            anthropic_sse_record(
                "message_delta",
                &serde_json::json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":4096}}),
            ),
            anthropic_sse_record("message_stop", &serde_json::json!({"type":"message_stop"})),
        ]
        .concat();
    let results = anthropic_stream_results(body).await;
    assert!(results.iter().all(Result::is_ok), "no error for truncated text");
    assert!(results.last().is_some_and(|r| r.as_ref().is_ok_and(|c| c.is_final)));
}

/// A healthy stream that keeps sending for longer than the old two-minute
/// client deadline must complete.
///
/// The clock is paused once the first chunk has arrived, so the two
/// 90-second gaps that follow are virtual. Pausing only after the
/// connection is up keeps auto-advance away from the connect timeout.
#[tokio::test]
async fn anthropic_stream_survives_past_the_old_two_minute_deadline() {
    use axum::{Router, body::Body, http::header, routing::post};
    use tokio::net::TcpListener;

    let parts: Vec<(u64, String)> = vec![
            (0, [
                    anthropic_sse_record(
                        "content_block_start",
                        &serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    ),
                    anthropic_sse_record(
                        "content_block_delta",
                        &serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"slow "}}),
                    ),
                ]
                .concat()),
            (90, anthropic_sse_record(
                    "content_block_delta",
                    &serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}),
                )),
            (90, anthropic_sse_record("message_stop", &serde_json::json!({"type":"message_stop"}))),
        ];
    let app = Router::new().route(
        "/v1/messages",
        post(move || {
            let parts = parts.clone();
            async move {
                let body = futures::stream::iter(parts).then(|(delay, part)| async move {
                    tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                    Ok::<_, std::convert::Infallible>(part)
                });
                ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(body))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test: bind loopback");
    let addr = listener.local_addr().expect("test: loopback addr");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let started = tokio::time::Instant::now();
    let provider = AnthropicProvider::with_base_url(Some("test-key"), Some(&format!("http://{addr}")));
    let messages = vec![ChatMessage::user("take your time")];
    let mut stream = provider.stream_chat_with_history(&messages, "claude-test", 0.0, StreamOptions::new(true));
    let first = stream.next().await.expect("test: the first chunk arrives at once");
    // Connected and streaming: from here on the gaps are virtual time.
    tokio::time::pause();
    let mut results = vec![first];
    while let Some(item) = stream.next().await {
        results.push(item);
    }
    server.abort();

    let errors: Vec<String> = results
        .iter()
        .filter_map(|r| r.as_ref().err().map(ToString::to_string))
        .collect();
    assert!(
        errors.is_empty(),
        "a healthy slow stream must not be cut (after {:?} of virtual time): {errors:?}",
        started.elapsed()
    );
    assert!(
        started.elapsed() >= std::time::Duration::from_mins(3),
        "the stub really ran past two minutes"
    );
    let text: String = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .map(|c| c.delta.as_str())
        .collect();
    assert_eq!(text, "slow answer");
}

// -----------------------------------------------------------------
// STREAM-g
// -----------------------------------------------------------------

/// A `tool_use` block that got its name but no argument bytes, closed, then
/// the message ends with `stop_reason`.
fn anthropic_name_only_call_ended_by(stop_reason: &str) -> String {
    [
            anthropic_sse_record(
                "content_block_start",
                &serde_json::json!({"type":"content_block_start","index":0,
                    "content_block":{"type":"tool_use","id":"toolu_s1","name":"fixture_write","input":{}}}),
            ),
            anthropic_sse_record(
                "content_block_stop",
                &serde_json::json!({"type":"content_block_stop","index":0}),
            ),
            anthropic_sse_record(
                "message_delta",
                &serde_json::json!({"type":"message_delta","delta":{"stop_reason":stop_reason},"usage":{"output_tokens":3}}),
            ),
            anthropic_sse_record("message_stop", &serde_json::json!({"type":"message_stop"})),
        ]
        .concat()
}

/// F-2: any stop other than `tool_use`, `end_turn` or `stop_sequence` while
/// the last block is a `tool_use` means the call is unfinished, even when
/// no argument byte arrived (which would otherwise become `{}`).
#[tokio::test]
async fn anthropic_stream_unfinished_stop_inside_a_tool_call_ends_in_an_error() {
    for reason in ["max_tokens", "model_context_window_exceeded", "refusal", "pause_turn"] {
        let results = anthropic_stream_results(anthropic_name_only_call_ended_by(reason)).await;
        let last = results.last().expect("test: the stream yielded something");
        assert!(
            last.is_err(),
            "stop_reason {reason} inside a tool call must fail the stream"
        );
        assert!(
            !results.iter().filter_map(|r| r.as_ref().ok()).any(|c| c.is_final),
            "stop_reason {reason}: no final chunk"
        );
    }
}

/// F-2 guard: the stops that end a message normally keep a no-argument
/// call as `{}`.
#[tokio::test]
async fn anthropic_stream_normal_stops_keep_a_no_argument_call() {
    for reason in ["tool_use", "end_turn", "stop_sequence"] {
        let results = anthropic_stream_results(anthropic_name_only_call_ended_by(reason)).await;
        assert!(
            results.iter().all(Result::is_ok),
            "stop_reason {reason} is a normal end"
        );
        let completed = completed_calls(&results);
        assert_eq!(completed.len(), 1, "stop_reason {reason}");
        assert_eq!(completed[0].args, "{}");
    }
}

/// F-1: no truncation error may read as a context overflow; the loop would
/// compact the history and replay the request.
#[tokio::test]
async fn anthropic_truncation_errors_never_read_as_context_overflow() {
    for reason in ["max_tokens", "model_context_window_exceeded", "refusal", "pause_turn"] {
        let results = anthropic_stream_results(anthropic_name_only_call_ended_by(reason)).await;
        let error = results
            .last()
            .and_then(|r| r.as_ref().err())
            .unwrap_or_else(|| panic!("stop_reason {reason}: the stream must fail"));
        let message = error.to_string();
        assert!(
            !crate::recovery::overflow::is_context_overflow_error(&anyhow::anyhow!(message.clone())),
            "stop_reason {reason}: '{message}' reads as a context overflow"
        );
    }
}

/// F-5: once the consumer drops the stream (the idle guard ended the turn),
/// the background task lets go of the connection instead of waiting on a
/// server that never sends again.
#[tokio::test]
async fn anthropic_stream_releases_the_connection_when_the_consumer_goes_away() {
    use axum::{Router, body::Body, http::header, routing::post};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::net::TcpListener;

    struct DropFlag(std::sync::Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let released = std::sync::Arc::new(AtomicBool::new(false));
    let flag = released.clone();
    let first = [
        anthropic_sse_record(
            "content_block_start",
            &serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        ),
        anthropic_sse_record(
            "content_block_delta",
            &serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hel"}}),
        ),
    ]
    .concat();
    let app = Router::new().route(
        "/v1/messages",
        post(move || {
            let flag = flag.clone();
            let first = first.clone();
            async move {
                // One record, then silence forever. The guard drops with
                // the body, i.e. when the server lets go of the response.
                let guard = DropFlag(flag);
                let body = futures::stream::once(async move { Ok::<_, std::convert::Infallible>(first) })
                    .chain(futures::stream::pending())
                    .map(move |item| {
                        let _keep = &guard;
                        item
                    });
                ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(body))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test: bind loopback");
    let addr = listener.local_addr().expect("test: loopback addr");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let provider = AnthropicProvider::with_base_url(Some("test-key"), Some(&format!("http://{addr}")));
    let messages = vec![ChatMessage::user("hi")];
    let mut stream = provider.stream_chat_with_history(&messages, "claude-test", 0.0, StreamOptions::new(true));
    let first_chunk = stream.next().await.expect("test: first chunk");
    assert_eq!(first_chunk.expect("test: first chunk is ok").delta, "hel");
    drop(stream);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !released.load(Ordering::SeqCst) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    server.abort();
    assert!(
        released.load(Ordering::SeqCst),
        "the provider task must close the connection once nobody reads the stream"
    );
}

/// COMPAT-STREAM (R-1): a message that ends without any `stop_reason`
/// while its last block is a `tool_use` leaves that call unconfirmed; a
/// name-only one would run as `{}`.
#[tokio::test]
async fn anthropic_stream_without_a_stop_reason_after_a_tool_call_ends_in_an_error() {
    let start = anthropic_sse_record(
        "content_block_start",
        &serde_json::json!({"type":"content_block_start","index":0,
                "content_block":{"type":"tool_use","id":"toolu_s1","name":"fixture_write","input":{}}}),
    );
    let stop = anthropic_sse_record(
        "content_block_stop",
        &serde_json::json!({"type":"content_block_stop","index":0}),
    );
    let message_stop = anthropic_sse_record("message_stop", &serde_json::json!({"type":"message_stop"}));
    for body in [
        // No `message_delta` at all.
        [start.clone(), stop.clone(), message_stop.clone()].concat(),
        // A `message_delta` without `stop_reason`.
        [
            start.clone(),
            stop.clone(),
            anthropic_sse_record(
                "message_delta",
                &serde_json::json!({"type":"message_delta","delta":{},"usage":{"output_tokens":3}}),
            ),
            message_stop.clone(),
        ]
        .concat(),
    ] {
        let results = anthropic_stream_results(body).await;
        let last = results.last().expect("test: the stream yielded something");
        let error = last
            .as_ref()
            .expect_err("test: no stop reason after a tool call must fail");
        assert!(error.to_string().contains("inside a tool call"), "got {error}");
        assert!(
            !crate::recovery::overflow::is_context_overflow_error(&anyhow::anyhow!(error.to_string())),
            "must not read as an overflow: {error}"
        );
    }
}

/// R-1 guard: a text-only message without `message_delta` stays a normal end.
#[tokio::test]
async fn anthropic_stream_text_without_a_stop_reason_stays_a_normal_end() {
    let body = [
        anthropic_sse_record(
            "content_block_start",
            &serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        ),
        anthropic_sse_record(
            "content_block_delta",
            &serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
        ),
        anthropic_sse_record("message_stop", &serde_json::json!({"type":"message_stop"})),
    ]
    .concat();
    let results = anthropic_stream_results(body).await;
    assert!(results.iter().all(Result::is_ok));
    assert!(results.last().is_some_and(|r| r.as_ref().is_ok_and(|c| c.is_final)));
}

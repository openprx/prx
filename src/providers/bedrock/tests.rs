use super::*;
use crate::providers::traits::ChatMessage;

// ── SigV4 signing tests ─────────────────────────────────────

#[test]
fn sha256_hex_empty_string() {
    // Known SHA-256 of empty input
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn sha256_hex_known_input() {
    // SHA-256 of "hello"
    assert_eq!(
        sha256_hex(b"hello"),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

/// AWS documentation example key for SigV4 test vectors (not a real credential).
const TEST_VECTOR_SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

#[test]
fn hmac_sha256_known_input() {
    let test_key: &[u8] = b"key";
    let result = hmac_sha256(test_key, b"message").unwrap();
    assert_eq!(
        hex::encode(&result),
        "6e9ef29b75fffc5b7abae527d58fdadb2fe42e7219011976917343065f58ed4a"
    );
}

#[test]
fn derive_signing_key_structure() {
    // Verify the key derivation produces a 32-byte key (SHA-256 output).
    let key = derive_signing_key(TEST_VECTOR_SECRET, "20150830", "us-east-1", "iam").unwrap();
    assert_eq!(key.len(), 32);
}

#[test]
fn derive_signing_key_known_test_vector() {
    // AWS SigV4 test vector from documentation.
    let key = derive_signing_key(TEST_VECTOR_SECRET, "20150830", "us-east-1", "iam").unwrap();
    assert_eq!(
        hex::encode(&key),
        "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
    );
}

#[test]
fn build_authorization_header_format() {
    let credentials = AwsCredentials {
        access_key_id: "AKIAIOSFODNN7EXAMPLE".to_string(),
        secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
        session_token: None,
        region: "us-east-1".to_string(),
    };

    let timestamp = chrono::DateTime::parse_from_rfc3339("2024-01-15T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        (
            "host".to_string(),
            "bedrock-runtime.us-east-1.amazonaws.com".to_string(),
        ),
        ("x-amz-date".to_string(), "20240115T120000Z".to_string()),
    ];

    let auth = build_authorization_header(
        &credentials,
        "POST",
        "/model/anthropic.claude-3-sonnet/converse",
        "",
        &headers,
        b"{}",
        &timestamp,
    )
    .unwrap();

    // Verify structure
    assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/"));
    assert!(auth.contains("SignedHeaders=content-type;host;x-amz-date"));
    assert!(auth.contains("Signature="));
    assert!(auth.contains("/us-east-1/bedrock/aws4_request"));
}

#[test]
fn build_authorization_header_includes_security_token_in_signed_headers() {
    let credentials = AwsCredentials {
        access_key_id: "AKIAIOSFODNN7EXAMPLE".to_string(),
        secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
        session_token: Some("session-token-value".to_string()),
        region: "us-east-1".to_string(),
    };

    let timestamp = chrono::DateTime::parse_from_rfc3339("2024-01-15T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        (
            "host".to_string(),
            "bedrock-runtime.us-east-1.amazonaws.com".to_string(),
        ),
        ("x-amz-date".to_string(), "20240115T120000Z".to_string()),
        ("x-amz-security-token".to_string(), "session-token-value".to_string()),
    ];

    let auth = build_authorization_header(
        &credentials,
        "POST",
        "/model/test-model/converse",
        "",
        &headers,
        b"{}",
        &timestamp,
    )
    .unwrap();

    assert!(auth.contains("x-amz-security-token"));
}

// ── Credential tests ────────────────────────────────────────

#[test]
fn credentials_host_formats_correctly() {
    let creds = AwsCredentials {
        access_key_id: "AKID".to_string(),
        secret_access_key: "secret".to_string(),
        session_token: None,
        region: "us-west-2".to_string(),
    };
    assert_eq!(creds.host(), "bedrock-runtime.us-west-2.amazonaws.com");
}

// ── AWS config file parsing tests ─────────────────────────

#[test]
fn parse_aws_credentials_file_default_profile() {
    let content = "[default]\naws_access_key_id = AKIA_TEST\naws_secret_access_key = secret123\n";
    let (key_id, secret, token) = AwsCredentials::parse_aws_credentials_file(content).unwrap();
    assert_eq!(key_id, "AKIA_TEST");
    assert_eq!(secret, "secret123");
    assert!(token.is_none());
}

#[test]
fn parse_aws_credentials_file_with_session_token() {
    let content =
        "[default]\naws_access_key_id = AKIA_TEST\naws_secret_access_key = secret123\naws_session_token = sess_tok\n";
    let (key_id, secret, token) = AwsCredentials::parse_aws_credentials_file(content).unwrap();
    assert_eq!(key_id, "AKIA_TEST");
    assert_eq!(secret, "secret123");
    assert_eq!(token.as_deref(), Some("sess_tok"));
}

#[test]
fn parse_aws_credentials_file_ignores_other_profiles() {
    let content = "[other]\naws_access_key_id = OTHER\naws_secret_access_key = OTHER_SECRET\n";
    assert!(AwsCredentials::parse_aws_credentials_file(content).is_none());
}

#[test]
fn parse_aws_config_region_default_profile() {
    let content = "[default]\nregion = eu-west-1\n";
    assert_eq!(
        AwsCredentials::parse_aws_config_region(content).as_deref(),
        Some("eu-west-1")
    );
}

#[test]
fn parse_aws_config_region_profile_default() {
    let content = "[profile default]\nregion = ap-southeast-1\n";
    assert_eq!(
        AwsCredentials::parse_aws_config_region(content).as_deref(),
        Some("ap-southeast-1")
    );
}

#[test]
fn parse_aws_config_region_not_found() {
    let content = "[profile other]\nregion = ap-northeast-1\n";
    assert!(AwsCredentials::parse_aws_config_region(content).is_none());
}

// ── Provider construction tests ─────────────────────────────

#[test]
fn creates_without_credentials() {
    // Provider should construct even without AWS config files.
    let _provider = BedrockProvider::new();
}

#[tokio::test]
async fn chat_fails_without_credentials() {
    let provider = BedrockProvider { credentials: None };
    let result = provider
        .chat_with_system(None, "hello", "anthropic.claude-sonnet-4-6", 0.7)
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("credentials not found"),
        "Expected credentials error, got: {err}"
    );
}

// ── Endpoint URL tests ──────────────────────────────────────

#[test]
fn endpoint_url_formats_correctly() {
    let url = BedrockProvider::endpoint_url("us-east-1", "anthropic.claude-sonnet-4-6");
    assert_eq!(
        url,
        "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-sonnet-4-6/converse"
    );
}

#[test]
fn endpoint_url_keeps_raw_colon() {
    // Endpoint URL uses raw colon so reqwest sends `:` on the wire.
    let url = BedrockProvider::endpoint_url("us-west-2", "anthropic.claude-3-5-haiku-20241022-v1:0");
    assert!(url.contains("/model/anthropic.claude-3-5-haiku-20241022-v1:0/converse"));
}

#[test]
fn stream_endpoint_url_formats_correctly() {
    let url = BedrockProvider::stream_endpoint_url("us-east-1", "anthropic.claude-sonnet-4-6");
    assert_eq!(
        url,
        "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-sonnet-4-6/converse-stream"
    );
}

#[test]
fn canonical_uri_encodes_colon() {
    // Canonical URI must encode `:` as `%3A` for SigV4 signing.
    let uri = BedrockProvider::canonical_uri("anthropic.claude-3-5-haiku-20241022-v1:0");
    assert_eq!(uri, "/model/anthropic.claude-3-5-haiku-20241022-v1%3A0/converse");
}

#[test]
fn canonical_uri_no_colon_unchanged() {
    let uri = BedrockProvider::canonical_uri("anthropic.claude-sonnet-4-6");
    assert_eq!(uri, "/model/anthropic.claude-sonnet-4-6/converse");
}

#[test]
fn stream_canonical_uri_encodes_colon() {
    let uri = BedrockProvider::stream_canonical_uri("anthropic.claude-3-5-haiku-20241022-v1:0");
    assert_eq!(uri, "/model/anthropic.claude-3-5-haiku-20241022-v1%3A0/converse-stream");
}

// ── Message conversion tests ────────────────────────────────

#[test]
fn convert_messages_system_extracted() {
    let messages = vec![ChatMessage::system("You are helpful"), ChatMessage::user("Hello")];
    let (system, msgs) = BedrockProvider::convert_messages(&messages);
    assert!(system.is_some());
    let system_blocks = system.unwrap();
    assert_eq!(system_blocks.len(), 1);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, "user");
}

#[test]
fn convert_messages_user_and_assistant() {
    let messages = vec![ChatMessage::user("Hello"), ChatMessage::assistant("Hi there")];
    let (system, msgs) = BedrockProvider::convert_messages(&messages);
    assert!(system.is_none());
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, "user");
    assert_eq!(msgs[1].role, "assistant");
}

#[test]
fn convert_messages_tool_role_to_tool_result() {
    let tool_json = r#"{"tool_call_id": "call_123", "content": "Result data"}"#;
    let messages = vec![ChatMessage::tool(tool_json)];
    let (_, msgs) = BedrockProvider::convert_messages(&messages);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, "user");
    assert!(matches!(msgs[0].content[0], ContentBlock::ToolResult(_)));
}

#[test]
fn convert_messages_assistant_tool_calls_parsed() {
    let tool_call_json = r#"{"content": "Let me check", "tool_calls": [{"id": "call_1", "name": "shell", "arguments": "{\"command\":\"ls\"}"}]}"#;
    let messages = vec![ChatMessage::assistant(tool_call_json)];
    let (_, msgs) = BedrockProvider::convert_messages(&messages);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, "assistant");
    assert_eq!(msgs[0].content.len(), 2);
    assert!(matches!(msgs[0].content[0], ContentBlock::Text(_)));
    assert!(matches!(msgs[0].content[1], ContentBlock::ToolUse(_)));
}

#[test]
fn convert_messages_plain_assistant_text() {
    let messages = vec![ChatMessage::assistant("Just text")];
    let (_, msgs) = BedrockProvider::convert_messages(&messages);
    assert_eq!(msgs.len(), 1);
    assert!(matches!(msgs[0].content[0], ContentBlock::Text(_)));
}

// ── Cache tests ─────────────────────────────────────────────

#[test]
fn should_cache_system_small_prompt() {
    assert!(!BedrockProvider::should_cache_system("Short prompt"));
}

#[test]
fn should_cache_system_large_prompt() {
    let large = "a".repeat(3073);
    assert!(BedrockProvider::should_cache_system(&large));
}

#[test]
fn should_cache_system_boundary() {
    assert!(!BedrockProvider::should_cache_system(&"a".repeat(3072)));
    assert!(BedrockProvider::should_cache_system(&"a".repeat(3073)));
}

#[test]
fn should_cache_conversation_short() {
    let messages = vec![
        ChatMessage::system("System"),
        ChatMessage::user("Hello"),
        ChatMessage::assistant("Hi"),
    ];
    assert!(!BedrockProvider::should_cache_conversation(&messages));
}

#[test]
fn should_cache_conversation_long() {
    let mut messages = vec![ChatMessage::system("System")];
    for i in 0..5 {
        messages.push(ChatMessage {
            role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
            content: format!("Message {i}"),
        });
    }
    assert!(BedrockProvider::should_cache_conversation(&messages));
}

// ── Tool conversion tests ───────────────────────────────────

#[test]
fn convert_tools_to_converse_formats_correctly() {
    let tools = vec![ToolSpec {
        name: "shell".to_string(),
        description: "Run commands".to_string(),
        parameters: serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}}),
    }];
    let provider = BedrockProvider::new();
    let config = provider
        .convert_tools_to_converse(Some(&tools))
        .expect("test: tools convert");
    assert!(config.is_some());
    let config = config.unwrap();
    assert_eq!(config.tools.len(), 1);
    assert_eq!(config.tools[0].tool_spec.name, "shell");
}

#[test]
fn convert_tools_to_converse_empty_returns_none() {
    let provider = BedrockProvider::new();
    assert!(
        provider
            .convert_tools_to_converse(Some(&[]))
            .expect("test: empty tools convert")
            .is_none()
    );
    assert!(
        provider
            .convert_tools_to_converse(None)
            .expect("test: absent tools convert")
            .is_none()
    );
}

fn test_eventstream_header(name: &str, value: &str) -> Vec<u8> {
    let mut header = Vec::new();
    header.push(name.len() as u8);
    header.extend_from_slice(name.as_bytes());
    header.push(7);
    header.extend_from_slice(&(value.len() as u16).to_be_bytes());
    header.extend_from_slice(value.as_bytes());
    header
}

fn test_eventstream_message(event_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut headers = Vec::new();
    headers.extend(test_eventstream_header(":message-type", "event"));
    headers.extend(test_eventstream_header(":event-type", event_type));
    headers.extend(test_eventstream_header(":content-type", "application/json"));

    let total_len = 16 + headers.len() + payload.len();
    let headers_len = headers.len();
    let mut message = Vec::new();
    message.extend_from_slice(&(total_len as u32).to_be_bytes());
    message.extend_from_slice(&(headers_len as u32).to_be_bytes());
    let prelude_crc = eventstream_crc(&message);
    message.extend_from_slice(&prelude_crc.to_be_bytes());
    message.extend_from_slice(&headers);
    message.extend_from_slice(payload);
    let message_crc = eventstream_crc(&message);
    message.extend_from_slice(&message_crc.to_be_bytes());
    message
}

#[test]
fn parses_eventstream_message_headers_and_payload() {
    let payload = br#"{"contentBlockDelta":{"delta":{"text":"hi"}}}"#;
    let message = test_eventstream_message("contentBlockDelta", payload);

    let parsed = try_parse_eventstream_message(&message)
        .unwrap()
        .expect("complete message");

    assert_eq!(parsed.consumed, message.len());
    assert_eq!(parsed.message.event_type.as_deref(), Some("contentBlockDelta"));
    assert_eq!(parsed.message.message_type.as_deref(), Some("event"));
    assert_eq!(parsed.message.payload, payload);
}

#[test]
fn eventstream_message_waits_for_complete_frame() {
    let payload = br#"{"messageStop":{"stopReason":"end_turn"}}"#;
    let message = test_eventstream_message("messageStop", payload);
    let partial = &message[..message.len() - 2];

    assert!(try_parse_eventstream_message(partial).unwrap().is_none());
}

#[test]
fn eventstream_message_rejects_bad_crc() {
    let payload = br#"{"messageStop":{"stopReason":"end_turn"}}"#;
    let mut message = test_eventstream_message("messageStop", payload);
    let last = message.len() - 1;
    message[last] ^= 0xff;

    let err = try_parse_eventstream_message(&message).unwrap_err().to_string();
    assert!(err.contains("crc"));
}

#[tokio::test]
async fn handles_bedrock_text_reasoning_tool_and_final_events() {
    let mut state = BedrockStreamState::default();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(10);

    let text_event = EventStreamMessage {
        event_type: Some("contentBlockDelta".to_string()),
        message_type: Some("event".to_string()),
        payload: br#"{"contentBlockIndex":0,"delta":{"text":"hello"}}"#.to_vec(),
    };
    assert!(!handle_bedrock_stream_message(text_event, &mut state, &tx, false).await);

    let reasoning_event = EventStreamMessage {
        event_type: Some("contentBlockDelta".to_string()),
        message_type: Some("event".to_string()),
        payload: br#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"thinking"}}}"#.to_vec(),
    };
    assert!(!handle_bedrock_stream_message(reasoning_event, &mut state, &tx, false).await);

    let tool_start = EventStreamMessage {
        event_type: Some("contentBlockStart".to_string()),
        message_type: Some("event".to_string()),
        payload: br#"{"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"tool_1","name":"shell"}}}"#.to_vec(),
    };
    assert!(!handle_bedrock_stream_message(tool_start, &mut state, &tx, false).await);

    let tool_delta = EventStreamMessage {
        event_type: Some("contentBlockDelta".to_string()),
        message_type: Some("event".to_string()),
        payload: br#"{"contentBlockIndex":1,"delta":{"toolUse":{"input":"{\"command\":\"ls\"}"}}}"#.to_vec(),
    };
    assert!(!handle_bedrock_stream_message(tool_delta, &mut state, &tx, false).await);

    let tool_stop = EventStreamMessage {
        event_type: Some("contentBlockStop".to_string()),
        message_type: Some("event".to_string()),
        payload: br#"{"contentBlockIndex":1}"#.to_vec(),
    };
    assert!(!handle_bedrock_stream_message(tool_stop, &mut state, &tx, false).await);

    let stop = EventStreamMessage {
        event_type: Some("messageStop".to_string()),
        message_type: Some("event".to_string()),
        payload: br#"{"stopReason":"tool_use"}"#.to_vec(),
    };
    assert!(handle_bedrock_stream_message(stop, &mut state, &tx, false).await);
    drop(tx);

    let text = rx.recv().await.unwrap().unwrap();
    assert_eq!(text.delta, "hello");

    let reasoning = rx.recv().await.unwrap().unwrap();
    assert_eq!(reasoning.reasoning.as_deref(), Some("thinking"));

    let tool_initial = rx.recv().await.unwrap().unwrap();
    assert_eq!(tool_initial.tool_calls[0].id, "tool_1");
    assert_eq!(tool_initial.tool_calls[0].name, "shell");

    let tool_delta = rx.recv().await.unwrap().unwrap();
    assert_eq!(
        tool_delta.tool_calls[0].arguments_delta.as_deref(),
        Some("{\"command\":\"ls\"}")
    );

    let tool_completed = rx.recv().await.unwrap().unwrap();
    assert_eq!(tool_completed.tool_calls[0].args, "{\"command\":\"ls\"}");

    let final_chunk = rx.recv().await.unwrap().unwrap();
    assert!(final_chunk.is_final);
    assert!(rx.recv().await.is_none());
}

#[test]
fn supports_streaming_returns_true() {
    let provider = BedrockProvider { credentials: None };
    assert!(provider.supports_streaming());
}

// ── Serde tests ─────────────────────────────────────────────

#[test]
fn converse_request_serializes_without_system() {
    let req = ConverseRequest {
        system: None,
        messages: vec![ConverseMessage {
            role: "user".to_string(),
            content: vec![ContentBlock::Text(TextBlock {
                text: "Hello".to_string(),
            })],
        }],
        inference_config: Some(InferenceConfig {
            max_tokens: 4096,
            temperature: 0.7,
        }),
        tool_config: None,
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(!json.contains("system"));
    assert!(json.contains("Hello"));
    assert!(json.contains("maxTokens"));
}

#[test]
fn converse_response_deserializes_text() {
    let json = r#"{
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{"text": "Hello from Bedrock"}]
                }
            },
            "stopReason": "end_turn"
        }"#;
    let resp: ConverseResponse = serde_json::from_str(json).unwrap();
    let parsed = BedrockProvider::parse_converse_response(resp);
    assert_eq!(parsed.text.as_deref(), Some("Hello from Bedrock"));
    assert!(parsed.tool_calls.is_empty());
}

#[test]
fn converse_response_deserializes_tool_use() {
    let json = r#"{
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"toolUse": {"toolUseId": "call_1", "name": "shell", "input": {"command": "ls"}}}
                    ]
                }
            },
            "stopReason": "tool_use"
        }"#;
    let resp: ConverseResponse = serde_json::from_str(json).unwrap();
    let parsed = BedrockProvider::parse_converse_response(resp);
    assert!(parsed.text.is_none());
    assert_eq!(parsed.tool_calls.len(), 1);
    assert_eq!(parsed.tool_calls[0].name, "shell");
    assert_eq!(parsed.tool_calls[0].id, "call_1");
}

#[test]
fn converse_response_empty_output() {
    let json = r#"{"output": null, "stopReason": null}"#;
    let resp: ConverseResponse = serde_json::from_str(json).unwrap();
    let parsed = BedrockProvider::parse_converse_response(resp);
    assert!(parsed.text.is_none());
    assert!(parsed.tool_calls.is_empty());
}

#[test]
fn content_block_text_serializes_as_flat_string() {
    let block = ContentBlock::Text(TextBlock {
        text: "Hello".to_string(),
    });
    let json = serde_json::to_string(&block).unwrap();
    // Must be {"text":"Hello"}, NOT {"text":{"text":"Hello"}}
    assert_eq!(json, r#"{"text":"Hello"}"#);
}

#[test]
fn content_block_tool_use_serializes_with_nested_object() {
    let block = ContentBlock::ToolUse(ToolUseWrapper {
        tool_use: ToolUseBlock {
            tool_use_id: "call_1".to_string(),
            name: "shell".to_string(),
            input: serde_json::json!({"command": "ls"}),
        },
    });
    let json = serde_json::to_string(&block).unwrap();
    assert!(json.contains(r#""toolUse""#));
    assert!(json.contains(r#""toolUseId":"call_1""#));
}

#[test]
fn content_block_cache_point_serializes() {
    let block = ContentBlock::CachePointBlock(CachePointWrapper {
        cache_point: CachePoint::default_cache(),
    });
    let json = serde_json::to_string(&block).unwrap();
    assert_eq!(json, r#"{"cachePoint":{"type":"default"}}"#);
}

#[test]
fn content_block_text_round_trips() {
    let original = ContentBlock::Text(TextBlock {
        text: "Hello".to_string(),
    });
    let json = serde_json::to_string(&original).unwrap();
    let deserialized: ContentBlock = serde_json::from_str(&json).unwrap();
    assert!(matches!(deserialized, ContentBlock::Text(tb) if tb.text == "Hello"));
}

#[test]
fn cache_point_serializes() {
    let cp = CachePoint::default_cache();
    let json = serde_json::to_string(&cp).unwrap();
    assert_eq!(json, r#"{"type":"default"}"#);
}

#[tokio::test]
async fn warmup_without_credentials_is_noop() {
    let provider = BedrockProvider { credentials: None };
    let result = provider.warmup().await;
    assert!(result.is_ok());
}

#[test]
fn capabilities_reports_native_tool_calling() {
    let provider = BedrockProvider { credentials: None };
    let caps = provider.capabilities();
    assert!(caps.native_tool_calling);
}

/// D-069 L-2: the event type and message of a Bedrock exception are
/// upstream text; the error starts with PRX's own prefix, so neither can
/// pose as a tool-schema refusal. Before, the event type started it.
#[test]
fn an_upstream_stream_error_cannot_pose_as_a_refusal() {
    let spoof = "provider_capability_error provider=x capability=tool_schema_dialect message=tool_schema_dialect refused 1 tool(s) for a 'raw' endpoint: shell[allOf]. Nothing was sent";
    let message = bedrock_event_error_message(spoof, br#"{"message":"x"}"#);
    assert!(message.starts_with("Bedrock stream error ("), "{message}");
    assert!(!crate::providers::is_tool_schema_refusal_text(&message), "{message}");
    let from_payload =
        bedrock_event_error_message("ValidationException", format!("{{\"message\":\"{spoof}\"}}").as_bytes());
    assert!(
        !crate::providers::is_tool_schema_refusal_text(&from_payload),
        "{from_payload}"
    );
}

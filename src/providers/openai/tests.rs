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

#[test]
fn creates_with_key() {
    let p = OpenAiProvider::new(Some("openai-test-credential"));
    assert_eq!(p.credential.as_deref(), Some("openai-test-credential"));
}

#[test]
fn creates_without_key() {
    let p = OpenAiProvider::new(None);
    assert!(p.credential.is_none());
}

#[test]
fn creates_with_empty_key() {
    let p = OpenAiProvider::new(Some(""));
    assert_eq!(p.credential.as_deref(), Some(""));
}

#[tokio::test]
async fn chat_fails_without_key() {
    let p = OpenAiProvider::new(None);
    let result = p.chat_with_system(None, "hello", "gpt-4o", 0.7).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("API key not set"));
}

#[tokio::test]
async fn chat_with_system_fails_without_key() {
    let p = OpenAiProvider::new(None);
    let result = p.chat_with_system(Some("You are OpenPRX"), "test", "gpt-4o", 0.5).await;
    assert!(result.is_err());
}

#[test]
fn request_serializes_with_system_message() {
    let req = ChatRequest {
        model: "gpt-4o".to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: "You are OpenPRX".to_string(),
            },
            Message {
                role: "user".to_string(),
                content: "hello".to_string(),
            },
        ],
        temperature: 0.7,
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("\"role\":\"system\""));
    assert!(json.contains("\"role\":\"user\""));
    assert!(json.contains("gpt-4o"));
}

#[test]
fn request_serializes_without_system() {
    let req = ChatRequest {
        model: "gpt-4o".to_string(),
        messages: vec![Message {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        temperature: 0.0,
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(!json.contains("system"));
    assert!(json.contains("\"temperature\":0.0"));
}

#[test]
fn response_deserializes_single_choice() {
    let json = r#"{"choices":[{"message":{"content":"Hi!"}}]}"#;
    let resp: ChatResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.choices.len(), 1);
    assert_eq!(resp.choices[0].message.visible_content(), "Hi!");
}

#[test]
fn response_deserializes_empty_choices() {
    let json = r#"{"choices":[]}"#;
    let resp: ChatResponse = serde_json::from_str(json).unwrap();
    assert!(resp.choices.is_empty());
}

#[test]
fn response_deserializes_multiple_choices() {
    let json = r#"{"choices":[{"message":{"content":"A"}},{"message":{"content":"B"}}]}"#;
    let resp: ChatResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.choices.len(), 2);
    assert_eq!(resp.choices[0].message.visible_content(), "A");
}

#[test]
fn response_with_unicode() {
    let json = r#"{"choices":[{"message":{"content":"Hello \u03A9"}}]}"#;
    let resp: ChatResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.choices[0].message.visible_content(), "Hello \u{03A9}");
}

#[test]
fn response_with_long_content() {
    let long = "x".repeat(100_000);
    let json = format!(r#"{{"choices":[{{"message":{{"content":"{long}"}}}}]}}"#);
    let resp: ChatResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(resp.choices[0].message.content.as_ref().unwrap().len(), 100_000);
}

#[tokio::test]
async fn warmup_without_key_is_noop() {
    let provider = OpenAiProvider::new(None);
    let result = provider.warmup().await;
    assert!(result.is_ok());
}

// ----------------------------------------------------------
// Reasoning model fallback tests (reasoning_content)
// ----------------------------------------------------------

#[test]
fn reasoning_content_not_merged_into_visible_when_content_empty() {
    let json = r#"{"choices":[{"message":{"content":"","reasoning_content":"Thinking..."}}]}"#;
    let resp: ChatResponse = serde_json::from_str(json).unwrap();
    let msg = &resp.choices[0].message;
    // Reasoning must stay in its own field — visible content remains empty.
    assert_eq!(msg.visible_content(), "");
    assert_eq!(msg.reasoning().as_deref(), Some("Thinking..."));
}

#[test]
fn reasoning_content_not_merged_when_content_null() {
    let json = r#"{"choices":[{"message":{"content":null,"reasoning_content":"Thinking..."}}]}"#;
    let resp: ChatResponse = serde_json::from_str(json).unwrap();
    let msg = &resp.choices[0].message;
    assert_eq!(msg.visible_content(), "");
    assert_eq!(msg.reasoning().as_deref(), Some("Thinking..."));
}

#[test]
fn visible_content_preferred_when_present() {
    let json = r#"{"choices":[{"message":{"content":"Hello","reasoning_content":"Sidebar"}}]}"#;
    let resp: ChatResponse = serde_json::from_str(json).unwrap();
    let msg = &resp.choices[0].message;
    // Both fields are separate; visible_content stays just the visible bit.
    assert_eq!(msg.visible_content(), "Hello");
    assert_eq!(msg.reasoning().as_deref(), Some("Sidebar"));
}

#[test]
fn native_response_surfaces_reasoning_separately_when_content_empty() {
    let json = r#"{"choices":[{"message":{"content":"","reasoning_content":"Native thinking"}}]}"#;
    let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
    let msg = &resp.choices[0].message;
    // Visible content stays empty; reasoning lives on its own.
    assert_eq!(msg.visible_content(), None);
    assert_eq!(msg.reasoning().as_deref(), Some("Native thinking"));
}

#[test]
fn native_response_keeps_visible_and_reasoning_apart() {
    let json = r#"{"choices":[{"message":{"content":"Real answer","reasoning_content":"Sidebar"}}]}"#;
    let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
    let msg = &resp.choices[0].message;
    assert_eq!(msg.visible_content(), Some("Real answer".to_string()));
    assert_eq!(msg.reasoning().as_deref(), Some("Sidebar"));
}

#[test]
fn parse_native_response_populates_reasoning_content_field() {
    let json = r#"{"choices":[{"message":{"content":"Visible","reasoning_content":"Internal"}}]}"#;
    let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
    let msg = resp.choices.into_iter().next().expect("test: choice").message;
    let chat_resp = OpenAiProvider::parse_native_response(msg);
    // Visible text -> ProviderChatResponse.text. Reasoning -> reasoning_content.
    assert_eq!(chat_resp.text.as_deref(), Some("Visible"));
    assert_eq!(chat_resp.reasoning_content.as_deref(), Some("Internal"));
}

#[test]
fn parse_native_response_no_reasoning_content_when_absent() {
    let json = r#"{"choices":[{"message":{"content":"Visible only"}}]}"#;
    let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
    let msg = resp.choices.into_iter().next().expect("test: choice").message;
    let chat_resp = OpenAiProvider::parse_native_response(msg);
    assert_eq!(chat_resp.text.as_deref(), Some("Visible only"));
    assert_eq!(chat_resp.reasoning_content, None);
}

#[test]
fn openai_non_streaming_response_usage_maps_reported() {
    let json = r#"{
            "choices":[{"message":{"content":"Real answer"}}],
            "usage":{"prompt_tokens":30,"completion_tokens":7,"total_tokens":37}
        }"#;
    let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
    let usage = resp
        .usage
        .expect("usage expected")
        .into_reported()
        .expect("complete usage should report");

    assert_eq!(usage.source, crate::llm::route_decision::TokenUsageSource::Reported);
    assert_eq!(usage.prompt_tokens, Some(30));
    assert_eq!(usage.completion_tokens, Some(7));
    assert_eq!(usage.total_tokens, Some(37));
}

#[test]
fn openai_malformed_usage_is_ignored_without_breaking_content() {
    let line = r#"data: {"choices":[{"delta":{"content":"still streams"}}],"usage":{"prompt_tokens":"bad","completion_tokens":4}}"#;
    let event = parse_openai_sse_line(line)
        .expect("malformed usage fields should not fail SSE parsing")
        .expect("content event expected");

    assert_eq!(event.content.as_deref(), Some("still streams"));
    assert!(
        event.usage.is_none(),
        "partial/malformed usage must fall back to estimate outside the parser"
    );
}

#[tokio::test]
async fn chat_with_tools_fails_without_key() {
    let p = OpenAiProvider::new(None);
    let messages = vec![ChatMessage::user("hello".to_string())];
    let tools = vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "shell",
            "description": "Run a shell command",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                },
                "required": ["command"]
            }
        }
    })];
    let result = p.chat_with_tools(&messages, &tools, "gpt-4o", 0.7).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("API key not set"));
}

#[tokio::test]
async fn chat_with_tools_rejects_invalid_tool_shape() {
    let p = OpenAiProvider::new(Some("openai-test-credential"));
    let messages = vec![ChatMessage::user("hello".to_string())];
    let tools = vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "shell",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                },
                "required": ["command"]
            }
        }
    })];

    let result = p.chat_with_tools(&messages, &tools, "gpt-4o", 0.7).await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Invalid OpenAI tool specification")
    );
}

#[test]
fn native_tool_spec_deserializes_from_openai_format() {
    let json = serde_json::json!({
        "type": "function",
        "function": {
            "name": "shell",
            "description": "Run a shell command",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                },
                "required": ["command"]
            }
        }
    });
    let spec = parse_native_tool_spec(json).unwrap();
    assert_eq!(spec.kind, "function");
    assert_eq!(spec.function.name, "shell");
}

// ─── 5a-7a Streaming SSE tests ──────────────────────────────────────

#[test]
fn openai_sse_line_done_sentinel() {
    let ev = parse_openai_sse_line("data: [DONE]").unwrap().expect("event");
    assert!(ev.done_sentinel);
    assert!(ev.content.is_none());
}

#[test]
fn openai_sse_line_blank_and_comment_skipped() {
    assert!(parse_openai_sse_line("").unwrap().is_none());
    assert!(parse_openai_sse_line(":heartbeat").unwrap().is_none());
    assert!(parse_openai_sse_line("\n").unwrap().is_none());
}

#[test]
fn openai_sse_line_parses_content_delta() {
    let line = r#"data: {"choices":[{"delta":{"content":"hello"}}]}"#;
    let ev = parse_openai_sse_line(line).unwrap().expect("event");
    assert_eq!(ev.content.as_deref(), Some("hello"));
    assert!(ev.tool_call_deltas.is_empty());
}

#[test]
fn openai_sse_line_parses_final_usage_chunk() {
    let line = r#"data: {"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":5,"total_tokens":17}}"#;
    let ev = parse_openai_sse_line(line).unwrap().expect("usage event");
    let usage = ev.usage.expect("usage must be surfaced");

    assert_eq!(usage.source, crate::llm::route_decision::TokenUsageSource::Reported);
    assert_eq!(usage.prompt_tokens, Some(12));
    assert_eq!(usage.completion_tokens, Some(5));
    assert_eq!(usage.total_tokens, Some(17));
    assert!(ev.content.is_none());
    assert!(ev.tool_call_deltas.is_empty());
}

#[test]
fn openai_sse_line_parses_reasoning_content_separately() {
    let line = r#"data: {"choices":[{"delta":{"reasoning_content":"thinking"}}]}"#;
    let ev = parse_openai_sse_line(line).unwrap().expect("event");
    assert!(ev.content.is_none());
    assert_eq!(ev.reasoning.as_deref(), Some("thinking"));
}

#[test]
fn openai_sse_line_parses_tool_call_delta() {
    let line = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_42","function":{"name":"shell","arguments":"{\"cmd\":"}}]}}]}"#;
    let ev = parse_openai_sse_line(line).unwrap().expect("event");
    assert_eq!(ev.tool_call_deltas.len(), 1);
    let tc = &ev.tool_call_deltas[0];
    assert_eq!(tc.index, 0);
    assert_eq!(tc.id.as_deref(), Some("call_42"));
    let f = tc.function.as_ref().unwrap();
    assert_eq!(f.name.as_deref(), Some("shell"));
    assert_eq!(f.arguments.as_deref(), Some("{\"cmd\":"));
}

#[test]
fn openai_sse_finish_reason_propagates() {
    let line = r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
    let ev = parse_openai_sse_line(line).unwrap().expect("event");
    assert_eq!(ev.finish_reason.as_deref(), Some("tool_calls"));
}

#[test]
fn tool_call_buffer_accumulates_partial_arguments() {
    let mut buf: Vec<ToolCallBuffer> = Vec::new();
    let s1 = apply_tool_call_deltas(
        &mut buf,
        vec![StreamSseToolCall {
            index: 0,
            id: Some("call_1".into()),
            function: Some(StreamSseFunction {
                name: Some("shell".into()),
                arguments: Some(r#"{"a""#.into()),
            }),
        }],
    )
    .expect("test: in-range tool-call index");
    // First delta carries id+name+partial args → one Streaming chunk.
    assert_eq!(s1.len(), 1);
    assert_eq!(s1[0].status, crate::providers::traits::ToolCallChunkStatus::Streaming);
    assert_eq!(s1[0].arguments_delta.as_deref(), Some(r#"{"a""#));

    let s2 = apply_tool_call_deltas(
        &mut buf,
        vec![StreamSseToolCall {
            index: 0,
            id: None,
            function: Some(StreamSseFunction {
                name: None,
                arguments: Some(r#": 1}"#.into()),
            }),
        }],
    )
    .expect("test: in-range tool-call index");
    assert_eq!(s2.len(), 1);
    assert_eq!(s2[0].arguments_delta.as_deref(), Some(r#": 1}"#));

    let flushed = flush_tool_call_buffer(&buf);
    assert_eq!(flushed.len(), 1);
    assert_eq!(flushed[0].id, "call_1");
    assert_eq!(flushed[0].name, "shell");
    assert_eq!(flushed[0].args, r#"{"a": 1}"#);
    assert_eq!(flushed[0].index, 0);
    assert_eq!(
        flushed[0].status,
        crate::providers::traits::ToolCallChunkStatus::Completed
    );
    assert!(flushed[0].arguments_delta.is_none());
}

#[test]
fn tool_call_buffer_supports_parallel_calls_by_index() {
    let mut buf: Vec<ToolCallBuffer> = Vec::new();
    let streaming = apply_tool_call_deltas(
        &mut buf,
        vec![
            StreamSseToolCall {
                index: 0,
                id: Some("call_a".into()),
                function: Some(StreamSseFunction {
                    name: Some("ls".into()),
                    arguments: Some("{}".into()),
                }),
            },
            StreamSseToolCall {
                index: 1,
                id: Some("call_b".into()),
                function: Some(StreamSseFunction {
                    name: Some("pwd".into()),
                    arguments: Some("{}".into()),
                }),
            },
        ],
    )
    .expect("test: in-range tool-call index");
    assert_eq!(streaming.len(), 2);
    assert_eq!(streaming[0].index, 0);
    assert_eq!(streaming[1].index, 1);

    let flushed = flush_tool_call_buffer(&buf);
    assert_eq!(flushed.len(), 2);
    assert_eq!(flushed[0].name, "ls");
    assert_eq!(flushed[1].name, "pwd");
    assert_eq!(flushed[1].index, 1);
}

#[test]
fn tool_call_buffer_skips_unnamed_entries() {
    // Some servers send empty `arguments` chunks before name appears; we
    // must not emit unnamed/empty placeholder calls.
    let mut buf: Vec<ToolCallBuffer> = Vec::new();
    let streaming = apply_tool_call_deltas(
        &mut buf,
        vec![StreamSseToolCall {
            index: 0,
            id: Some("call_x".into()),
            function: Some(StreamSseFunction {
                name: None,
                arguments: Some(String::new()),
            }),
        }],
    )
    .expect("test: in-range tool-call index");
    // No name yet → no Streaming chunk emitted to the driver.
    assert!(streaming.is_empty(), "expected an empty value");
    assert!(flush_tool_call_buffer(&buf).is_empty(), "expected an empty value");
}

/// Test helper: replay a sequence of SSE lines through the parser to
/// assert the driver-facing chunk shape (delta / tool_call / final).
///
/// Returns `(text, all_emitted_calls, saw_final, had_finish_reason)` where
/// `all_emitted_calls` interleaves Streaming chunks (in arrival order) and
/// the terminal Completed chunks emitted at `finish_reason == "tool_calls"`.
fn replay_openai_sse(lines: &[&str]) -> (String, Vec<ToolCallChunk>, bool, bool) {
    let mut tool_buf: Vec<ToolCallBuffer> = Vec::new();
    let mut text = String::new();
    let mut emitted_calls: Vec<ToolCallChunk> = Vec::new();
    let mut saw_final = false;
    let mut had_finish_reason = false;
    for line in lines {
        let event = match parse_openai_sse_line(line).expect("test: line parses") {
            Some(e) => e,
            None => continue,
        };
        if event.done_sentinel {
            saw_final = true;
            break;
        }
        if !event.tool_call_deltas.is_empty() {
            let streaming =
                apply_tool_call_deltas(&mut tool_buf, event.tool_call_deltas).expect("test: in-range tool-call index");
            emitted_calls.extend(streaming);
        }
        if let Some(c) = event.content {
            text.push_str(&c);
        }
        if let Some(finish) = event.finish_reason.as_deref() {
            had_finish_reason = true;
            if finish == "tool_calls" {
                emitted_calls.extend(flush_tool_call_buffer(&tool_buf));
                tool_buf.clear();
            }
            saw_final = true;
            break;
        }
    }
    (text, emitted_calls, saw_final, had_finish_reason)
}

#[test]
fn openai_streaming_native_tool_call_via_fixture() {
    use crate::providers::traits::ToolCallChunkStatus;
    // Replay a realistic SSE stream: content delta, then two `tool_calls`
    // arguments fragments, then `finish_reason=tool_calls`, then [DONE].
    let lines = [
        r#"data: {"choices":[{"delta":{"content":"checking"}}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"shell","arguments":"{\"cmd\":"}}]}}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]}}]}"#,
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        "data: [DONE]",
    ];
    let (text, calls, final_seen, had_finish) = replay_openai_sse(&lines);
    assert_eq!(text, "checking");
    assert!(had_finish, "stream must include finish_reason");
    assert!(final_seen, "must reach final / done sentinel");
    // 2 Streaming + 1 Completed.
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].status, ToolCallChunkStatus::Streaming);
    assert_eq!(calls[1].status, ToolCallChunkStatus::Streaming);
    assert_eq!(calls[2].status, ToolCallChunkStatus::Completed);
    assert_eq!(calls[2].name, "shell");
    assert_eq!(calls[2].id, "call_1");
    assert_eq!(calls[2].args, r#"{"cmd":"ls"}"#);
    assert_eq!(calls[2].index, 0);
}

#[test]
fn openai_streaming_delta_then_final_stop() {
    let lines = [
        r#"data: {"choices":[{"delta":{"content":"hello"}}]}"#,
        r#"data: {"choices":[{"delta":{"content":" world"}}]}"#,
        r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        "data: [DONE]",
    ];
    let (text, calls, final_seen, had_finish) = replay_openai_sse(&lines);
    assert_eq!(text, "hello world");
    assert!(had_finish);
    assert!(final_seen);
    assert!(calls.is_empty(), "expected an empty value");
}

#[test]
fn openai_streaming_handles_partial_tool_args_across_three_chunks() {
    use crate::providers::traits::ToolCallChunkStatus;
    let lines = [
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_z","function":{"name":"edit"}}]}}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"p\""}}]}}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"a.rs\","}}]}}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"l\":1}"}}]}}]}"#,
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    ];
    let (_text, calls, _final, _finish) = replay_openai_sse(&lines);
    // 4 Streaming (id/name + 3 args fragments) + 1 Completed = 5.
    assert_eq!(calls.len(), 5);
    let completed = calls.last().expect("test: completed chunk");
    assert_eq!(completed.status, ToolCallChunkStatus::Completed);
    assert_eq!(completed.name, "edit");
    assert_eq!(completed.args, r#"{"p":"a.rs","l":1}"#);
}

/// **S3 T3-2-B**: incremental Streaming protocol — provider must emit a
/// `Streaming` `ToolCallChunk` for each SSE delta carrying a tool-call
/// fragment, then a single final `Completed` chunk whose `args` equals the
/// concatenation of all preceding `arguments_delta` values.
#[test]
fn test_tool_calls_streaming_emits_incremental_chunks() {
    use crate::providers::traits::ToolCallChunkStatus;
    let lines = [
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"search","arguments":""}}]}}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q"}}]}}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"uery\":"}}]}}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"hello\"}"}}]}}]}"#,
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    ];
    let (_text, calls, final_seen, had_finish) = replay_openai_sse(&lines);
    assert!(had_finish);
    assert!(final_seen);

    // 4 Streaming (1 opener with empty args + 3 fragments) + 1 Completed.
    assert_eq!(calls.len(), 5, "expected 4 Streaming + 1 Completed, got {calls:?}");
    let (streaming_chunks, completed_chunks): (Vec<_>, Vec<_>) =
        calls.iter().partition(|c| c.status == ToolCallChunkStatus::Streaming);
    assert_eq!(streaming_chunks.len(), 4);
    assert_eq!(completed_chunks.len(), 1);

    // Every Streaming chunk must carry an `arguments_delta` (possibly empty
    // on the opener) and an empty `args` field.
    for chunk in &streaming_chunks {
        assert_eq!(chunk.id, "call_abc");
        assert_eq!(chunk.name, "search");
        assert_eq!(chunk.index, 0);
        assert_eq!(chunk.args, "");
        assert!(chunk.arguments_delta.is_some());
    }

    // Aggregated streaming deltas must equal Completed.args.
    let aggregated: String = streaming_chunks
        .iter()
        .filter_map(|c| c.arguments_delta.as_deref())
        .collect();
    let completed = completed_chunks[0];
    assert_eq!(completed.status, ToolCallChunkStatus::Completed);
    assert_eq!(completed.id, "call_abc");
    assert_eq!(completed.name, "search");
    assert_eq!(completed.index, 0);
    assert!(completed.arguments_delta.is_none());
    assert_eq!(completed.args, r#"{"query":"hello"}"#);
    assert_eq!(
        aggregated, completed.args,
        "streaming deltas aggregated must equal Completed.args"
    );
}

/// **S3 T3-2-B**: concurrent tool calls — buffer must be isolated by
/// `index` and emit independent `Completed` chunks for each call.
#[test]
fn test_tool_calls_concurrent_indices() {
    use crate::providers::traits::ToolCallChunkStatus;
    // Interleaved deltas for two concurrent tool calls (index 0 + 1).
    let lines = [
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"search","arguments":""}},{"index":1,"id":"call_b","function":{"name":"fetch","arguments":""}}]}}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q\":\"rust\"}"}}]}}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"url\":\"x\"}"}}]}}]}"#,
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    ];
    let (_text, calls, _final, _finish) = replay_openai_sse(&lines);

    let streaming_chunks: Vec<&ToolCallChunk> = calls
        .iter()
        .filter(|c| c.status == ToolCallChunkStatus::Streaming)
        .collect();
    let completed_chunks: Vec<&ToolCallChunk> = calls
        .iter()
        .filter(|c| c.status == ToolCallChunkStatus::Completed)
        .collect();

    // 2 openers + 1 fragment for index 0 + 1 fragment for index 1 = 4 Streaming.
    assert_eq!(streaming_chunks.len(), 4);
    assert_eq!(completed_chunks.len(), 2, "expected 2 independent Completed chunks");

    // Find Completed by index.
    let c0 = completed_chunks
        .iter()
        .find(|c| c.index == 0)
        .expect("test: completed index 0");
    let c1 = completed_chunks
        .iter()
        .find(|c| c.index == 1)
        .expect("test: completed index 1");

    assert_eq!(c0.id, "call_a");
    assert_eq!(c0.name, "search");
    assert_eq!(c0.args, r#"{"q":"rust"}"#);
    assert!(c0.arguments_delta.is_none());

    assert_eq!(c1.id, "call_b");
    assert_eq!(c1.name, "fetch");
    assert_eq!(c1.args, r#"{"url":"x"}"#);
    assert!(c1.arguments_delta.is_none());

    // Per-index aggregation invariant.
    let agg0: String = streaming_chunks
        .iter()
        .filter(|c| c.index == 0)
        .filter_map(|c| c.arguments_delta.as_deref())
        .collect();
    let agg1: String = streaming_chunks
        .iter()
        .filter(|c| c.index == 1)
        .filter_map(|c| c.arguments_delta.as_deref())
        .collect();
    assert_eq!(agg0, c0.args);
    assert_eq!(agg1, c1.args);
}

#[tokio::test]
async fn openai_streaming_fails_without_key() {
    use crate::providers::traits::StreamOptions;
    let provider = OpenAiProvider::new(None);
    let messages = vec![ChatMessage::user("hi".to_string())];
    let mut stream = provider.stream_chat_with_history(&messages, "gpt-4o", 0.0, StreamOptions::new(false));
    let first = stream.next().await.expect("chunk");
    let err = first.expect_err("no key → error");
    assert!(err.to_string().contains("API key not set"));
}

#[tokio::test]
async fn openai_streaming_propagates_http_error() {
    use crate::providers::traits::StreamOptions;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::post;
    use tokio::net::TcpListener;

    async fn unauthorized() -> (StatusCode, &'static str) {
        (StatusCode::UNAUTHORIZED, "Unauthorized")
    }
    let app = Router::new().route("/chat/completions", post(unauthorized));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
    let addr = listener.local_addr().expect("test: addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let provider = OpenAiProvider::with_base_url(Some(&format!("http://{addr}")), Some("bad-key"));
    let messages = vec![ChatMessage::user("hi".to_string())];
    let mut stream = provider.stream_chat_with_history(&messages, "gpt-4o", 0.0, StreamOptions::new(false));
    let first = stream.next().await.expect("chunk");
    let err = first.expect_err("must surface 401 as StreamError");
    let msg = err.to_string();
    assert!(msg.contains("401"), "got: {msg}");
}

#[tokio::test]
async fn openai_streaming_rejects_malformed_non_sse_success_body() {
    use crate::providers::traits::StreamOptions;
    use axum::Router;
    use axum::routing::post;
    use tokio::net::TcpListener;

    async fn malformed() -> &'static str {
        "{not valid json"
    }
    let app = Router::new().route("/chat/completions", post(malformed));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
    let addr = listener.local_addr().expect("test: addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let provider = OpenAiProvider::with_base_url(Some(&format!("http://{addr}")), Some("test-key"));
    let messages = vec![ChatMessage::user("hi".to_string())];
    let mut stream = provider.stream_chat_with_history(&messages, "gpt-4o", 0.0, StreamOptions::new(false));
    let first = stream.next().await.expect("chunk");
    let err = first.expect_err("malformed 200 response must not become empty success");
    let msg = err.to_string();
    assert!(msg.contains("Invalid SSE format"), "got: {msg}");
    assert!(msg.contains("incomplete trailing data"), "got: {msg}");
}

// -----------------------------------------------------------------
// STREAM-TOOLS: native tools on the streaming request
// -----------------------------------------------------------------

/// One OpenAI SSE `data:` line followed by the blank separator.
fn openai_sse_data(data: &serde_json::Value) -> String {
    format!("data: {data}\n\n")
}

/// The streamed reply the real Chat Completions API sends for a
/// `fixture_write({"value":"x"})` call, split across two argument fragments.
fn openai_streamed_tool_call_body() -> String {
    [
        openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{"role":"assistant",
                "tool_calls":[{"index":0,"id":"call_s1","type":"function",
                    "function":{"name":"fixture_write","arguments":"{\"value\":"}}]}}]})),
        openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{
                "tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]}}]})),
        openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})),
        openai_sse_data(&serde_json::json!({"choices":[],
                "usage":{"prompt_tokens":10,"completion_tokens":7,"total_tokens":17}})),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat()
}

/// Loopback `/chat/completions` stub that records every request body and
/// answers with `body` as `text/event-stream`.
async fn spawn_openai_sse_stub(
    body: String,
) -> (
    String,
    std::sync::Arc<parking_lot::Mutex<Vec<serde_json::Value>>>,
    tokio::task::JoinHandle<()>,
) {
    use axum::{Json, Router, http::header, routing::post};
    use tokio::net::TcpListener;

    let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = captured.clone();
    let app = Router::new().route(
        "/chat/completions",
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

fn fixture_write_spec() -> ToolSpec {
    ToolSpec {
        name: "fixture_write".to_string(),
        description: "Write a fixture value".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"]
        }),
    }
}

#[tokio::test]
async fn openai_streaming_request_carries_native_tools_and_tool_choice() {
    use crate::providers::traits::ToolCallChunkStatus;

    let (base, captured, server) = spawn_openai_sse_stub(openai_streamed_tool_call_body()).await;
    let provider = OpenAiProvider::with_base_url(Some(&base), Some("test-key"));
    let messages = vec![ChatMessage::user("write x")];

    let chunks: Vec<StreamChunk> = provider
        .stream_chat_with_history(
            &messages,
            "gpt-test",
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
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "fixture_write");
    assert_eq!(tools[0]["function"]["description"], "Write a fixture value");
    assert_eq!(tools[0]["function"]["parameters"], fixture_write_spec().parameters);
    assert_eq!(body["tool_choice"], "auto");

    let completed: Vec<&ToolCallChunk> = chunks
        .iter()
        .flat_map(|c| c.tool_calls.iter())
        .filter(|c| c.status == ToolCallChunkStatus::Completed)
        .collect();
    assert_eq!(completed.len(), 1, "exactly one completed tool call");
    assert_eq!(completed[0].id, "call_s1");
    assert_eq!(completed[0].name, "fixture_write");
    let args: serde_json::Value = serde_json::from_str(&completed[0].args).expect("test: args are JSON");
    assert_eq!(args, serde_json::json!({"value": "x"}));
    assert!(
        chunks.last().is_some_and(|c| c.is_final),
        "stream ends with a final chunk"
    );
}

#[tokio::test]
async fn openai_streaming_request_without_tools_omits_tools_and_tool_choice() {
    // `with_tools` normalises an empty list away, but the field is public:
    // a raw empty list must not reach the wire either.
    for options in [
        StreamOptions::new(true),
        StreamOptions {
            tools: Some(Vec::new()),
            ..StreamOptions::new(true)
        },
    ] {
        assert_streaming_request_omits_tools(options).await;
    }
}

async fn assert_streaming_request_omits_tools(options: StreamOptions) {
    let text_body = [
        openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{"content":"hi"}}]})),
        openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let (base, captured, server) = spawn_openai_sse_stub(text_body).await;
    let provider = OpenAiProvider::with_base_url(Some(&base), Some("test-key"));
    let messages = vec![ChatMessage::user("hi")];

    let text: String = provider
        .stream_chat_with_history(&messages, "gpt-test", 0.0, options)
        .map(|chunk| chunk.expect("test: stream chunk").delta)
        .collect::<Vec<_>>()
        .await
        .concat();
    server.abort();

    assert_eq!(text, "hi");
    let bodies = captured.lock().clone();
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].get("tools").is_none(), "got {}", bodies[0]);
    assert!(bodies[0].get("tool_choice").is_none(), "got {}", bodies[0]);
}

/// The tool-call `index` is server-controlled; the buffer must not grow to
/// whatever it names. An index past the cap ends the stream with an error.
#[tokio::test]
async fn openai_stream_rejects_an_out_of_range_tool_call_index() {
    let body = [
        openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{
                "tool_calls":[{"index":1000,"id":"call_far","type":"function",
                    "function":{"name":"fixture_write","arguments":"{}"}}]}}]})),
        openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let (base, _captured, server) = spawn_openai_sse_stub(body).await;
    let provider = OpenAiProvider::with_base_url(Some(&base), Some("test-key"));
    let messages = vec![ChatMessage::user("hi")];

    let results: Vec<StreamResult<StreamChunk>> = provider
        .stream_chat_with_history(&messages, "gpt-test", 0.0, StreamOptions::new(true))
        .collect()
        .await;
    server.abort();

    let error = results
        .iter()
        .find_map(|r| r.as_ref().err())
        .expect("test: an out-of-range tool-call index must end the stream with an error");
    assert!(matches!(error, StreamError::InvalidSse(_)), "got {error:?}");
    assert!(error.to_string().contains("1000"), "got {error}");
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().ok())
            .all(|c| c.tool_calls.is_empty()),
        "no tool call may surface from the rejected delta"
    );
}

/// The largest in-range index is still accepted.
#[test]
fn apply_tool_call_deltas_accepts_the_last_in_range_index() {
    let mut buf = Vec::new();
    let chunks = apply_tool_call_deltas(
        &mut buf,
        vec![StreamSseToolCall {
            index: MAX_STREAMED_TOOL_CALL_INDEX,
            id: Some("call_last".into()),
            function: Some(StreamSseFunction {
                name: Some("shell".into()),
                arguments: Some("{}".into()),
            }),
        }],
    )
    .expect("test: the cap itself is in range");
    assert_eq!(chunks.len(), 1);
    assert_eq!(buf.len(), MAX_STREAMED_TOOL_CALL_INDEX + 1);
}

// -----------------------------------------------------------------
// STREAM-f: a half-received tool call must fail the stream, never
// surface as a completed call
// -----------------------------------------------------------------

/// The first half of a `fixture_write` call: what a stream has sent when it
/// is cut in the middle of the arguments.
fn openai_tool_call_prefix() -> String {
    openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{"role":"assistant",
            "tool_calls":[{"index":0,"id":"call_s1","type":"function",
                "function":{"name":"fixture_write","arguments":"{\"value\":"}}]}}]}))
}

async fn openai_stream_results(body: String) -> Vec<StreamResult<StreamChunk>> {
    let (base, _captured, server) = spawn_openai_sse_stub(body).await;
    let provider = OpenAiProvider::with_base_url(Some(&base), Some("test-key"));
    let messages = vec![ChatMessage::user("write x")];
    let results = provider
        .stream_chat_with_history(
            &messages,
            "gpt-test",
            0.0,
            StreamOptions::new(true).with_tools(vec![fixture_write_spec()]),
        )
        .collect()
        .await;
    server.abort();
    results
}

fn openai_stream_error(results: &[StreamResult<StreamChunk>]) -> &StreamError {
    let error = results
        .last()
        .and_then(|r| r.as_ref().err())
        .unwrap_or_else(|| panic!("the stream must end in an error, got {} items", results.len()));
    assert_eq!(results.iter().filter(|r| r.is_err()).count(), 1, "exactly one error");
    assert!(
        !results.iter().filter_map(|r| r.as_ref().ok()).any(|c| c.is_final),
        "a failed stream must not report a final chunk"
    );
    error
}

fn openai_completed_calls(results: &[StreamResult<StreamChunk>]) -> usize {
    results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .flat_map(|c| c.tool_calls.iter())
        .filter(|c| c.status == crate::providers::traits::ToolCallChunkStatus::Completed)
        .count()
}

#[tokio::test]
async fn openai_stream_cut_mid_tool_arguments_ends_in_an_error() {
    let results = openai_stream_results(openai_tool_call_prefix()).await;
    let error = openai_stream_error(&results);
    assert!(matches!(error, StreamError::InvalidSse(_)), "got {error:?}");
    assert!(error.to_string().contains("[DONE]"), "got {error}");
    assert_eq!(
        openai_completed_calls(&results),
        0,
        "a cut call must not be flushed as completed"
    );
}

#[tokio::test]
async fn openai_stream_text_cut_before_done_ends_in_an_error() {
    let body = openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{"content":"half an ans"}}]}));
    let results = openai_stream_results(body).await;
    let error = openai_stream_error(&results);
    assert!(error.to_string().contains("[DONE]"), "got {error}");
}

#[tokio::test]
async fn openai_stream_error_object_ends_in_a_provider_error() {
    let body = [
        openai_tool_call_prefix(),
        openai_sse_data(&serde_json::json!({"error":{"message":"The server is overloaded","type":"server_error"}})),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let results = openai_stream_results(body).await;
    let error = openai_stream_error(&results);
    assert!(matches!(error, StreamError::Provider(_)), "got {error:?}");
    let message = error.to_string();
    assert!(message.contains("server_error"), "got {message}");
    assert!(message.contains("The server is overloaded"), "got {message}");
    assert_eq!(openai_completed_calls(&results), 0);
}

#[tokio::test]
async fn openai_stream_length_inside_a_tool_call_ends_in_an_error() {
    let body = [
        openai_tool_call_prefix(),
        openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}]})),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let results = openai_stream_results(body).await;
    let error = openai_stream_error(&results);
    assert!(error.to_string().contains("length"), "got {error}");
    assert_eq!(openai_completed_calls(&results), 0);
}

/// A healthy stream that keeps sending for longer than the old two-minute
/// client deadline must complete.
///
/// The clock is paused once the first chunk has arrived, so the two
/// 90-second gaps that follow are virtual. Pausing only after the
/// connection is up keeps auto-advance away from the connect timeout.
#[tokio::test]
async fn openai_stream_survives_past_the_old_two_minute_deadline() {
    use axum::{Router, body::Body, http::header, routing::post};
    use tokio::net::TcpListener;

    let parts: Vec<(u64, String)> = vec![
        (
            0,
            openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{"content":"slow "}}]})),
        ),
        (
            90,
            openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{"content":"answer"}}]})),
        ),
        (
            90,
            [
                openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})),
                "data: [DONE]\n\n".to_string(),
            ]
            .concat(),
        ),
    ];
    let app = Router::new().route(
        "/chat/completions",
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
    let provider = OpenAiProvider::with_base_url(Some(&format!("http://{addr}")), Some("test-key"));
    let messages = vec![ChatMessage::user("take your time")];
    let mut stream = provider.stream_chat_with_history(&messages, "gpt-test", 0.0, StreamOptions::new(true));
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

/// The buffered path treats an empty tool list as no offer too, like the
/// streaming path: no `tools: []` and no `tool_choice`.
#[tokio::test]
async fn openai_buffered_request_with_an_empty_tool_list_omits_tools_and_tool_choice() {
    use axum::{Json, Router, routing::post};
    use tokio::net::TcpListener;

    let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<serde_json::Value>::new()));
    let sink = captured.clone();
    let app = Router::new().route(
        "/chat/completions",
        post(move |Json(request): Json<serde_json::Value>| {
            let sink = sink.clone();
            async move {
                sink.lock().push(request);
                Json(serde_json::json!({
                    "choices": [{"message": {"role": "assistant", "content": "hi"}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                }))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test: bind loopback");
    let addr = listener.local_addr().expect("test: loopback addr");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let provider = OpenAiProvider::with_base_url(Some(&format!("http://{addr}")), Some("test-key"));
    let messages = vec![ChatMessage::user("hi")];
    let response = provider
        .chat(
            ProviderChatRequest {
                messages: &messages,
                tools: Some(&[]),
            },
            "gpt-test",
            0.0,
        )
        .await
        .expect("test: buffered chat succeeds");
    server.abort();

    assert_eq!(response.text.as_deref(), Some("hi"));
    let bodies = captured.lock().clone();
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].get("tools").is_none(), "got {}", bodies[0]);
    assert!(bodies[0].get("tool_choice").is_none(), "got {}", bodies[0]);
}

// -----------------------------------------------------------------
// STREAM-g
// -----------------------------------------------------------------

/// F-3: a complete text answer (`finish_reason: stop`) whose body ends
/// without `[DONE]` is accepted.
#[tokio::test]
async fn openai_stream_complete_text_answer_without_done_is_accepted() {
    let body = [
        openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{"content":"hello"}}]})),
        openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})),
    ]
    .concat();
    let results = openai_stream_results(body).await;
    assert!(results.iter().all(Result::is_ok), "a finished answer is not an error");
    let text: String = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .map(|c| c.delta.as_str())
        .collect();
    assert_eq!(text, "hello");
    assert!(results.last().is_some_and(|r| r.as_ref().is_ok_and(|c| c.is_final)));
}

/// F-3: calls already flushed on `finish_reason: tool_calls` are complete;
/// a missing `[DONE]` after them is not an error.
#[tokio::test]
async fn openai_stream_flushed_tool_calls_without_done_are_accepted() {
    let body = [
        openai_tool_call_prefix(),
        openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{
                "tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]}}]})),
        openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})),
    ]
    .concat();
    let results = openai_stream_results(body).await;
    assert!(results.iter().all(Result::is_ok), "complete calls are not an error");
    assert_eq!(openai_completed_calls(&results), 1);
}

/// F-4: `stop` with buffered calls hands over the complete ones as
/// completed calls and the incomplete ones with their raw arguments, which
/// the loop refuses visibly on the same call id.
#[tokio::test]
async fn openai_stream_stop_hands_over_buffered_calls_for_the_loop_to_check() {
    let body = [
            openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[
                {"index":0,"id":"call_ok","type":"function","function":{"name":"fixture_write","arguments":"{\"value\":\"x\"}"}},
                {"index":1,"id":"call_cut","type":"function","function":{"name":"fixture_write","arguments":"{\"value\":"}}
            ]}}]})),
            openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();
    let results = openai_stream_results(body).await;
    assert!(results.iter().all(Result::is_ok));
    let completed: Vec<(String, String)> = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .flat_map(|c| c.tool_calls.iter())
        .filter(|c| c.status == crate::providers::traits::ToolCallChunkStatus::Completed)
        .map(|c| (c.id.clone(), c.args.clone()))
        .collect();
    assert_eq!(
        completed,
        vec![
            ("call_ok".to_string(), r#"{"value":"x"}"#.to_string()),
            ("call_cut".to_string(), r#"{"value":"#.to_string()),
        ]
    );
}

/// F-4: `stop` with a call that never got argument bytes cannot be told
/// apart from a no-argument call downstream, so the stream fails.
#[tokio::test]
async fn openai_stream_stop_with_a_name_only_call_ends_in_an_error() {
    let body = [
        openai_sse_data(
            &serde_json::json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[
                {"index":0,"id":"call_s1","type":"function","function":{"name":"fixture_write","arguments":""}}
            ]}}]}),
        ),
        openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let results = openai_stream_results(body).await;
    let error = openai_stream_error(&results);
    assert!(error.to_string().contains("incomplete"), "got {error}");
    assert_eq!(openai_completed_calls(&results), 0);
}

/// F-4: `content_filter` fails the stream visibly, with or without a call.
#[tokio::test]
async fn openai_stream_content_filter_ends_in_an_error() {
    for prefix in [
        openai_tool_call_prefix(),
        openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{"content":"partial"}}]})),
    ] {
        let body = [
            prefix,
            openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]})),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();
        let results = openai_stream_results(body).await;
        let error = openai_stream_error(&results);
        assert!(error.to_string().contains("content_filter"), "got {error}");
        assert_eq!(openai_completed_calls(&results), 0);
    }
}

/// F-5: once the consumer drops the stream, the background task lets go of
/// the connection instead of waiting on a server that never sends again.
#[tokio::test]
async fn openai_stream_releases_the_connection_when_the_consumer_goes_away() {
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
    let first = openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{"content":"hel"}}]}));
    let app = Router::new().route(
        "/chat/completions",
        post(move || {
            let flag = flag.clone();
            let first = first.clone();
            async move {
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

    let provider = OpenAiProvider::with_base_url(Some(&format!("http://{addr}")), Some("test-key"));
    let messages = vec![ChatMessage::user("hi")];
    let mut stream = provider.stream_chat_with_history(&messages, "gpt-test", 0.0, StreamOptions::new(true));
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

/// COMPAT-STREAM (R-3): a server that never sends `finish_reason` but ends
/// with `[DONE]` still delivers its complete calls, as compatible does.
#[tokio::test]
async fn openai_stream_done_without_finish_reason_flushes_complete_calls() {
    let body = [
        openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{"role":"assistant",
                "tool_calls":[{"index":0,"id":"call_s1","type":"function",
                    "function":{"name":"fixture_write","arguments":"{\"value\":\"x\"}"}}]}}]})),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let results = openai_stream_results(body).await;
    assert!(results.iter().all(Result::is_ok), "a complete call is not an error");
    assert_eq!(
        openai_completed_calls(&results),
        1,
        "the call is delivered, not dropped"
    );
}

/// R-3: the same end with a call that never got argument bytes fails, as
/// with `stop`: downstream it would run as `{}`.
#[tokio::test]
async fn openai_stream_done_without_finish_reason_refuses_a_name_only_call() {
    let body = [
        openai_sse_data(&serde_json::json!({"choices":[{"index":0,"delta":{"role":"assistant",
                "tool_calls":[{"index":0,"id":"call_s1","type":"function",
                    "function":{"name":"fixture_write","arguments":""}}]}}]})),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let results = openai_stream_results(body).await;
    let error = openai_stream_error(&results);
    assert!(error.to_string().contains("incomplete"), "got {error}");
    assert_eq!(openai_completed_calls(&results), 0);
}

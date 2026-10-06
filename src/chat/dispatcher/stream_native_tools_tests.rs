//! STREAM-TOOLS: the chat driver streams from the direct Anthropic and OpenAI
//! providers, and a native tool offered on that stream must actually reach the
//! model and run.
//!
//! The provider is the real `AnthropicProvider` / `OpenAiProvider` talking to a
//! loopback SSE stub that behaves like the real API: it answers with a tool
//! call only when the request offered the tool. A stub that answered with a
//! tool call regardless would pass even when the request carried no tools,
//! because the stream parser and the executor already work.
//!
//! The tests sit in a `#[cfg(test)]` module of their own, like
//! `src/skills/symlink_tests.rs`, so the source-level architecture guards see
//! them as test code: they build the real providers directly and read the
//! request-accounting event back, both of which production code must not do.

#![cfg(test)]
#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing)]

    use super::super::*;
    use crate::tools::{Tool, ToolResult, ToolSpec};
    use async_trait::async_trait;
    use axum::{Json, Router, http::header, routing::post};
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    const FIXTURE_MODEL: &str = "fixture-model";
    const FIXTURE_TOOL: &str = "fixture_write";
    const FIXTURE_DRAFT: &str = "stream-native-tools-draft";
    const FIXTURE_SYSTEM: &str = "fixture system";

    /// Which wire dialect the loopback stub speaks.
    #[derive(Clone, Copy)]
    enum Wire {
        Anthropic,
        OpenAi,
    }

    impl Wire {
        const fn path(self) -> &'static str {
            match self {
                Self::Anthropic => "/v1/messages",
                Self::OpenAi => "/chat/completions",
            }
        }

        /// Whether this request body offered `fixture_write` as a native tool.
        fn offers_fixture_tool(self, body: &serde_json::Value) -> bool {
            body["tools"].as_array().is_some_and(|tools| {
                tools.iter().any(|tool| {
                    let name = match self {
                        Self::Anthropic => &tool["name"],
                        Self::OpenAi => &tool["function"]["name"],
                    };
                    name == FIXTURE_TOOL
                })
            })
        }

        /// Whether this request replays the model's `fixture_write` call, with its
        /// id and complete arguments, on the assistant side of the history.
        fn replays_assistant_call(self, body: &serde_json::Value) -> bool {
            body["messages"].as_array().is_some_and(|messages| {
                messages
                    .iter()
                    .filter(|message| message["role"] == "assistant")
                    .any(|message| match self {
                        Self::Anthropic => message["content"].as_array().is_some_and(|blocks| {
                            blocks.iter().any(|block| {
                                block["type"] == "tool_use"
                                    && block["id"] == "toolu_s1"
                                    && block["name"] == FIXTURE_TOOL
                                    && block["input"] == serde_json::json!({"value": "x"})
                            })
                        }),
                        Self::OpenAi => message["tool_calls"].as_array().is_some_and(|calls| {
                            calls.iter().any(|call| {
                                call["id"] == "call_s1"
                                    && call["function"]["name"] == FIXTURE_TOOL
                                    && call["function"]["arguments"]
                                        .as_str()
                                        .and_then(|arguments| serde_json::from_str::<serde_json::Value>(arguments).ok())
                                        == Some(serde_json::json!({"value": "x"}))
                            })
                        }),
                    })
            })
        }

        /// The system text the request carried.
        fn system_text(self, body: &serde_json::Value) -> String {
            match self {
                Self::Anthropic => body["system"].to_string(),
                Self::OpenAi => body["messages"]
                    .as_array()
                    .and_then(|messages| messages.iter().find(|message| message["role"] == "system"))
                    .map(|message| message["content"].to_string())
                    .unwrap_or_default(),
            }
        }

        fn sse_body(self, reply: StubReply) -> String {
            match (self, reply) {
                (Self::Anthropic, StubReply::ToolCall) => [
                    anthropic_record(
                        "message_start",
                        &serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0}}}),
                    ),
                    anthropic_record(
                        "content_block_start",
                        &serde_json::json!({"type":"content_block_start","index":0,
                            "content_block":{"type":"tool_use","id":"toolu_s1","name":FIXTURE_TOOL,"input":{}}}),
                    ),
                    anthropic_record(
                        "content_block_delta",
                        &serde_json::json!({"type":"content_block_delta","index":0,
                            "delta":{"type":"input_json_delta","partial_json":"{\"value\":"}}),
                    ),
                    anthropic_record(
                        "content_block_delta",
                        &serde_json::json!({"type":"content_block_delta","index":0,
                            "delta":{"type":"input_json_delta","partial_json":"\"x\"}"}}),
                    ),
                    anthropic_record(
                        "content_block_stop",
                        &serde_json::json!({"type":"content_block_stop","index":0}),
                    ),
                    anthropic_record(
                        "message_delta",
                        &serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}),
                    ),
                    anthropic_record("message_stop", &serde_json::json!({"type":"message_stop"})),
                ]
                .concat(),
                (Self::Anthropic, StubReply::ToolCallCut) => anthropic_tool_use_prefix(),
                (Self::Anthropic, StubReply::ToolCallThenError) => [
                    anthropic_tool_use_prefix(),
                    anthropic_record(
                        "error",
                        &serde_json::json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
                    ),
                ]
                .concat(),
                (Self::Anthropic, StubReply::ToolCallOutOfTokens) => [
                    anthropic_tool_use_prefix(),
                    anthropic_record(
                        "content_block_stop",
                        &serde_json::json!({"type":"content_block_stop","index":0}),
                    ),
                    anthropic_record(
                        "message_delta",
                        &serde_json::json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":4096}}),
                    ),
                    anthropic_record("message_stop", &serde_json::json!({"type":"message_stop"})),
                ]
                .concat(),
                (Self::Anthropic, StubReply::NameOnlyThenStop(reason)) => [
                    anthropic_record(
                        "content_block_start",
                        &serde_json::json!({"type":"content_block_start","index":0,
                            "content_block":{"type":"tool_use","id":"toolu_s1","name":FIXTURE_TOOL,"input":{}}}),
                    ),
                    anthropic_record(
                        "content_block_stop",
                        &serde_json::json!({"type":"content_block_stop","index":0}),
                    ),
                    anthropic_record(
                        "message_delta",
                        &serde_json::json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":3}}),
                    ),
                    anthropic_record("message_stop", &serde_json::json!({"type":"message_stop"})),
                ]
                .concat(),
                (Self::Anthropic, StubReply::ToolCallThenErrorSaying(message)) => [
                    anthropic_tool_use_prefix(),
                    anthropic_record(
                        "error",
                        &serde_json::json!({"type":"error","error":{"type":"api_error","message":message}}),
                    ),
                ]
                .concat(),
                (Self::Anthropic, StubReply::PartialCallThenStop) => [
                    anthropic_tool_use_prefix(),
                    anthropic_record(
                        "content_block_stop",
                        &serde_json::json!({"type":"content_block_stop","index":0}),
                    ),
                    anthropic_record(
                        "message_delta",
                        &serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
                    ),
                    anthropic_record("message_stop", &serde_json::json!({"type":"message_stop"})),
                ]
                .concat(),
                (Self::Anthropic, StubReply::CallWithArguments(arguments)) => [
                    anthropic_record(
                        "content_block_start",
                        &serde_json::json!({"type":"content_block_start","index":0,
                            "content_block":{"type":"tool_use","id":"toolu_s1","name":FIXTURE_TOOL,"input":{}}}),
                    ),
                    anthropic_record(
                        "content_block_delta",
                        &serde_json::json!({"type":"content_block_delta","index":0,
                            "delta":{"type":"input_json_delta","partial_json":arguments}}),
                    ),
                    anthropic_record(
                        "content_block_stop",
                        &serde_json::json!({"type":"content_block_stop","index":0}),
                    ),
                    anthropic_record(
                        "message_delta",
                        &serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}),
                    ),
                    anthropic_record("message_stop", &serde_json::json!({"type":"message_stop"})),
                ]
                .concat(),
                (Self::Anthropic, StubReply::CompleteCallThenStop) => Self::Anthropic.sse_body(StubReply::ToolCall),
                (Self::Anthropic, StubReply::Text(text)) => [
                    anthropic_record(
                        "content_block_start",
                        &serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    ),
                    anthropic_record(
                        "content_block_delta",
                        &serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
                    ),
                    anthropic_record(
                        "content_block_stop",
                        &serde_json::json!({"type":"content_block_stop","index":0}),
                    ),
                    anthropic_record(
                        "message_delta",
                        &serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
                    ),
                    anthropic_record("message_stop", &serde_json::json!({"type":"message_stop"})),
                ]
                .concat(),
                (Self::OpenAi, StubReply::ToolCall) => [
                    openai_data(&serde_json::json!({"choices":[{"index":0,"delta":{"role":"assistant",
                        "tool_calls":[{"index":0,"id":"call_s1","type":"function",
                            "function":{"name":FIXTURE_TOOL,"arguments":"{\"value\":"}}]}}]})),
                    openai_data(&serde_json::json!({"choices":[{"index":0,"delta":{
                        "tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]}}]})),
                    openai_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})),
                    openai_data(&serde_json::json!({"choices":[],
                        "usage":{"prompt_tokens":10,"completion_tokens":7,"total_tokens":17}})),
                    "data: [DONE]\n\n".to_string(),
                ]
                .concat(),
                (Self::OpenAi, StubReply::ToolCallCut) => openai_tool_call_prefix(),
                (Self::OpenAi, StubReply::ToolCallThenError) => [
                    openai_tool_call_prefix(),
                    openai_data(&serde_json::json!({"error":{"message":"The server is overloaded","type":"server_error"}})),
                    "data: [DONE]\n\n".to_string(),
                ]
                .concat(),
                (Self::OpenAi, StubReply::ToolCallOutOfTokens) => [
                    openai_tool_call_prefix(),
                    openai_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}]})),
                    "data: [DONE]\n\n".to_string(),
                ]
                .concat(),
                (Self::OpenAi, StubReply::NameOnlyThenStop(reason)) => [
                    openai_data(&serde_json::json!({"choices":[{"index":0,"delta":{"role":"assistant",
                        "tool_calls":[{"index":0,"id":"call_s1","type":"function",
                            "function":{"name":FIXTURE_TOOL,"arguments":""}}]}}]})),
                    openai_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]})),
                    "data: [DONE]\n\n".to_string(),
                ]
                .concat(),
                (Self::OpenAi, StubReply::ToolCallThenErrorSaying(message)) => [
                    openai_tool_call_prefix(),
                    openai_data(&serde_json::json!({"error":{"message":message,"type":"server_error"}})),
                    "data: [DONE]\n\n".to_string(),
                ]
                .concat(),
                (Self::OpenAi, StubReply::PartialCallThenStop) => [
                    openai_tool_call_prefix(),
                    openai_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})),
                    "data: [DONE]\n\n".to_string(),
                ]
                .concat(),
                (Self::OpenAi, StubReply::CallWithArguments(arguments)) => [
                    openai_data(&serde_json::json!({"choices":[{"index":0,"delta":{"role":"assistant",
                        "tool_calls":[{"index":0,"id":"call_s1","type":"function",
                            "function":{"name":FIXTURE_TOOL,"arguments":arguments}}]}}]})),
                    openai_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})),
                    "data: [DONE]\n\n".to_string(),
                ]
                .concat(),
                (Self::OpenAi, StubReply::CompleteCallThenStop) => [
                    openai_data(&serde_json::json!({"choices":[{"index":0,"delta":{"role":"assistant",
                        "tool_calls":[{"index":0,"id":"call_s1","type":"function",
                            "function":{"name":FIXTURE_TOOL,"arguments":"{\"value\":\"x\"}"}}]}}]})),
                    openai_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})),
                    "data: [DONE]\n\n".to_string(),
                ]
                .concat(),
                (Self::OpenAi, StubReply::Text(text)) => [
                    openai_data(&serde_json::json!({"choices":[{"index":0,"delta":{"content":text}}]})),
                    openai_data(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})),
                    "data: [DONE]\n\n".to_string(),
                ]
                .concat(),
            }
        }
    }

    #[derive(Clone, Copy)]
    enum StubReply {
        ToolCall,
        /// The tool call's stream is cut after the first argument fragment.
        ToolCallCut,
        /// An in-stream upstream error mid-arguments: Anthropic `event: error`
        /// (`overloaded_error`) then EOF; OpenAI an `error` object then `[DONE]`.
        ToolCallThenError,
        /// The output budget ends inside the call: Anthropic closes the block
        /// with `stop_reason: max_tokens`; OpenAI sends `finish_reason: length`.
        ToolCallOutOfTokens,
        /// A call that got its name but no argument bytes, then the message
        /// ends with this stop (Anthropic `stop_reason`, OpenAI `finish_reason`).
        NameOnlyThenStop(&'static str),
        /// Anthropic: an in-stream `event: error` with this message mid-arguments.
        ToolCallThenErrorSaying(&'static str),
        /// Half of a call's arguments, then a normal end (Anthropic `end_turn`,
        /// OpenAI `stop`): the call reaches the loop with invalid arguments.
        PartialCallThenStop,
        /// A finished call whose arguments are exactly this text (valid JSON or
        /// not), ended normally (Anthropic `tool_use`, OpenAI `tool_calls`).
        CallWithArguments(&'static str),
        /// OpenAI: a complete call, then `finish_reason: stop` (a known server
        /// quirk for forced tool choice) and `[DONE]`.
        CompleteCallThenStop,
        Text(&'static str),
    }

    /// `message_start`, a `fixture_write` block and half of its arguments.
    fn anthropic_tool_use_prefix() -> String {
        [
            anthropic_record(
                "message_start",
                &serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0}}}),
            ),
            anthropic_record(
                "content_block_start",
                &serde_json::json!({"type":"content_block_start","index":0,
                    "content_block":{"type":"tool_use","id":"toolu_s1","name":FIXTURE_TOOL,"input":{}}}),
            ),
            anthropic_record(
                "content_block_delta",
                &serde_json::json!({"type":"content_block_delta","index":0,
                    "delta":{"type":"input_json_delta","partial_json":"{\"value\":"}}),
            ),
        ]
        .concat()
    }

    fn anthropic_record(event: &str, data: &serde_json::Value) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    /// The first half of a `fixture_write` call's arguments.
    fn openai_tool_call_prefix() -> String {
        openai_data(&serde_json::json!({"choices":[{"index":0,
            "delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_s1","type":"function",
                "function":{"name":FIXTURE_TOOL,"arguments":"{\"value\":"}}]}}]}))
    }

    fn openai_data(data: &serde_json::Value) -> String {
        format!("data: {data}\n\n")
    }

    /// Loopback stub that behaves like the real API for this scenario:
    /// request 1 gets a tool call only if it offered the tool (otherwise plain
    /// text), and every later request gets the final text `done`.
    async fn spawn_stub(wire: Wire) -> (String, Arc<Mutex<Vec<serde_json::Value>>>, tokio::task::JoinHandle<()>) {
        spawn_stub_with(wire, StubReply::ToolCall).await
    }

    /// [`spawn_stub`], with `tool_reply` as the answer to an offering request 1.
    async fn spawn_stub_with(
        wire: Wire,
        tool_reply: StubReply,
    ) -> (String, Arc<Mutex<Vec<serde_json::Value>>>, tokio::task::JoinHandle<()>) {
        let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        let app = Router::new().route(
            wire.path(),
            post(move |Json(request): Json<serde_json::Value>| {
                let sink = Arc::clone(&sink);
                async move {
                    let reply = {
                        let mut requests = sink.lock();
                        let reply = if !requests.is_empty() {
                            StubReply::Text("done")
                        } else if wire.offers_fixture_tool(&request) {
                            tool_reply
                        } else {
                            StubReply::Text("no tools offered")
                        };
                        requests.push(request);
                        reply
                    };
                    ([(header::CONTENT_TYPE, "text/event-stream")], wire.sse_body(reply))
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

    struct CountingFixtureTool {
        calls: Arc<AtomicUsize>,
        arguments: Arc<Mutex<Vec<serde_json::Value>>>,
        /// Whether the schema requires `value`. Without it, the execution
        /// contract accepts `{}`, so only the stream and loop layers stand
        /// between a half-received call and its execution.
        requires_value: bool,
    }

    #[async_trait]
    impl Tool for CountingFixtureTool {
        fn name(&self) -> &str {
            FIXTURE_TOOL
        }

        fn description(&self) -> &str {
            "Write a fixture value"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            let mut schema = serde_json::json!({
                "type": "object",
                "properties": { "value": { "type": "string" } }
            });
            if self.requires_value {
                schema["required"] = serde_json::json!(["value"]);
            }
            schema
        }

        async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.arguments.lock().push(arguments);
            Ok(ToolResult {
                success: true,
                output: "fixture-tool-ok".to_string(),
                error: None,
            })
        }
    }

    struct ChatRun {
        actions: Vec<Action>,
        requests: Vec<serde_json::Value>,
        tool_calls: usize,
        tool_arguments: Vec<serde_json::Value>,
        compiled_request_events: Vec<serde_json::Value>,
    }

    /// Drive one real chat turn (`drive_start_turn_stream`, which streams from the
    /// provider) against `provider`, and collect what the turn did.
    async fn run_chat_turn(
        provider: Arc<dyn Provider>,
        captured: &Arc<Mutex<Vec<serde_json::Value>>>,
        selection: (&str, Option<&str>),
    ) -> ChatRun {
        run_chat_turn_with(provider, captured, true, selection).await
    }

    async fn run_chat_turn_with(
        provider: Arc<dyn Provider>,
        captured: &Arc<Mutex<Vec<serde_json::Value>>>,
        requires_value: bool,
        selection: (&str, Option<&str>),
    ) -> ChatRun {
        run_chat_turn_after(provider, captured, requires_value, 0, selection).await
    }

    /// [`run_chat_turn_with`], after `earlier_pairs` earlier user/assistant
    /// exchanges: a history long enough for overflow compaction to shorten.
    async fn run_chat_turn_after(
        provider: Arc<dyn Provider>,
        captured: &Arc<Mutex<Vec<serde_json::Value>>>,
        requires_value: bool,
        earlier_pairs: usize,
        selection: (&str, Option<&str>),
    ) -> ChatRun {
        let calls = Arc::new(AtomicUsize::new(0));
        let arguments = Arc::new(Mutex::new(Vec::new()));
        let registry = crate::agent::test_admission::registry_with_search(vec![Box::new(CountingFixtureTool {
            calls: Arc::clone(&calls),
            arguments: Arc::clone(&arguments),
            requires_value,
        })]);

        let (action_tx, mut action_rx) = mpsc::channel::<Action>(128);
        let cancellation = CancellationToken::new();
        let ledger_dir = tempfile::TempDir::new().expect("test: chat ledger dir");
        let mut history = vec![crate::providers::traits::ChatMessage::system(FIXTURE_SYSTEM)];
        for pair in 0..earlier_pairs {
            history.push(crate::providers::traits::ChatMessage::user(format!(
                "earlier question {pair}"
            )));
            history.push(crate::providers::traits::ChatMessage::assistant(format!(
                "earlier answer {pair}"
            )));
        }
        history.push(crate::providers::traits::ChatMessage::user("write x"));

        let admitted = super::super::turn_characterization_tests::admit_chat_fixture(
            Arc::clone(&provider),
            Some(registry),
            ledger_dir.path(),
            (selection.0, FIXTURE_MODEL, selection.1),
            (FIXTURE_DRAFT, &history.last().expect("source input").content),
            action_tx.clone(),
            cancellation.clone(),
        )
        .await;
        let principal = admitted.context.envelope.memory_principal();
        let ledger = admitted.fabric.memory();

        drive_start_turn_stream(
            Some(admitted.binding),
            None,
            provider,
            history.clone(),
            history,
            FIXTURE_MODEL.to_string(),
            0.0,
            None,
            cancellation,
            FIXTURE_DRAFT.to_string(),
            action_tx.clone(),
            Some(admitted.registry),
            Some(admitted.service),
            admitted.context,
            admitted.fabric,
            None,
            crate::agent::turn_contracts::ChatMode::Edit,
            Arc::new(crate::observability::noop::NoopObserver),
            Arc::new(crate::hooks::HookManager::new(std::path::PathBuf::new())),
        )
        .await;

        drop(action_tx);
        let mut actions = Vec::new();
        while let Some(action) = action_rx.recv().await {
            actions.push(action);
        }

        // The request-accounting events land in the fabric's workspace, under the
        // chat envelope's session scope.
        let compiled_request_events = ledger
            .list_message_events_since(&principal, 0, 100)
            .await
            .expect("test: list message events")
            .into_iter()
            .filter(|event| event.event_type == "llm.request.context.compiled")
            .map(|event| {
                serde_json::from_str(event.raw_payload_json.as_deref().expect("test: request payload"))
                    .expect("test: request payload is JSON")
            })
            .collect();

        let tool_calls = calls.load(Ordering::SeqCst);
        let tool_arguments = arguments.lock().clone();
        let requests = captured.lock().clone();
        ChatRun {
            actions,
            requests,
            tool_calls,
            tool_arguments,
            compiled_request_events,
        }
    }

    fn fixture_spec() -> ToolSpec {
        ToolSpec {
            name: FIXTURE_TOOL.to_string(),
            description: "Write a fixture value".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "required": ["value"]
            }),
        }
    }

    /// The assertions shared by both providers; `result_carried` checks the
    /// provider-specific shape of the tool result in the second request.
    fn assert_streamed_native_tool_call_ran(
        wire: Wire,
        run: &ChatRun,
        result_carried: impl Fn(&serde_json::Value) -> bool,
    ) {
        let finals: Vec<&str> = run
            .actions
            .iter()
            .filter_map(|action| match action {
                Action::StreamCompleted { final_text, .. } => Some(final_text.as_str()),
                _ => None,
            })
            .collect();

        // 1. The first request offered the tool natively.
        let first = run.requests.first().expect("test: at least one provider request");
        assert_eq!(first["stream"], true, "the chat driver streams from the provider");
        assert!(
            wire.offers_fixture_tool(first),
            "the streaming request must offer {FIXTURE_TOOL} (turn outcome: tool runs={}, provider requests={}, \
             final texts={finals:?}, request 1 accounted tools={} native_tool_bytes={}); body: {first}",
            run.tool_calls,
            run.requests.len(),
            run.compiled_request_events
                .first()
                .map_or(serde_json::Value::Null, |event| event["tool_snapshot"]["count"].clone()),
            run.compiled_request_events
                .first()
                .map_or(serde_json::Value::Null, |event| {
                    event["request_estimate"]["native_tool_bytes"].clone()
                }),
        );

        // 2. The tool ran exactly once with the streamed arguments.
        assert_eq!(run.tool_calls, 1, "the streamed native tool call must execute once");
        // The executor adds its own `_zc_*` scope fields; the model's argument is `value`.
        assert_eq!(run.tool_arguments.len(), 1);
        assert_eq!(
            run.tool_arguments
                .first()
                .and_then(|arguments| arguments.get("value"))
                .and_then(serde_json::Value::as_str),
            Some("x")
        );

        // 3. Exactly one follow-up request, 4. carrying the tool result.
        assert_eq!(run.requests.len(), 2, "tool round trip = exactly two provider requests");
        let second = run.requests.get(1).expect("test: second request");
        assert!(
            result_carried(second),
            "the second request must carry the tool result; body: {second}"
        );
        assert!(
            wire.replays_assistant_call(second),
            "the second request must replay the model's call on the assistant side; body: {second}"
        );

        // 5. The turn ends with the final text.
        assert_eq!(finals, ["done"], "one terminal StreamCompleted with the final text");

        // 6. Prompt pin (D-020 Q5): native mode is unchanged, so the system text
        //    never carries the prompt-guided tool protocol or its grammar.
        for request in &run.requests {
            let system = wire.system_text(request);
            assert!(system.contains(FIXTURE_SYSTEM), "system prompt kept: {system}");
            assert!(
                !system.contains("<tool_call>"),
                "a native streaming turn must not teach the prompt-guided grammar: {system}"
            );
        }
        let prompt_guided = crate::tools::prompt::render_prompt_guided_tool_protocol(&[fixture_spec()]);
        assert!(!prompt_guided.is_empty());
        assert!(
            !wire.system_text(first).contains(prompt_guided.trim()),
            "the prompt-guided protocol must not be rendered into the system prompt"
        );

        // 7. Request accounting is truthful: every compiled-request event that
        //    counted a native tool payload belongs to a request that carried one.
        assert_eq!(
            run.compiled_request_events.len(),
            run.requests.len(),
            "one compiled-request event per provider request"
        );
        for (event, request) in run.compiled_request_events.iter().zip(&run.requests) {
            let counted_bytes = event["request_estimate"]["native_tool_bytes"]
                .as_u64()
                .expect("test: native_tool_bytes");
            let counted_tools = event["tool_snapshot"]["count"].as_u64().expect("test: tool count");
            let wire_tools = request["tools"].as_array().map_or(0, Vec::len);
            assert!(counted_bytes > 0, "a native tool payload was measured: {event}");
            assert_eq!(
                usize::try_from(counted_tools).ok(),
                Some(wire_tools),
                "the event counts exactly the tools the request carried"
            );
        }
    }

    #[tokio::test]
    async fn chat_driver_executes_a_streamed_native_tool_call_on_anthropic() {
        let (base, captured, server) = spawn_stub(Wire::Anthropic).await;
        let provider: Arc<dyn Provider> = Arc::new(crate::providers::anthropic::AnthropicProvider::with_base_url(
            Some("test-key"),
            Some(&base),
        ));

        let run = run_chat_turn(provider, &captured, ("anthropic", Some(&base))).await;
        server.abort();

        assert_streamed_native_tool_call_ran(Wire::Anthropic, &run, |second| {
            second["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "user"
                        && message["content"].as_array().is_some_and(|blocks| {
                            blocks
                                .iter()
                                .any(|block| block["type"] == "tool_result" && block["tool_use_id"] == "toolu_s1")
                        })
                })
            })
        });
    }

    #[tokio::test]
    async fn chat_driver_executes_a_streamed_native_tool_call_on_openai() {
        let (base, captured, server) = spawn_stub(Wire::OpenAi).await;
        let provider: Arc<dyn Provider> = Arc::new(crate::providers::openai::OpenAiProvider::with_base_url(
            Some(&base),
            Some("test-key"),
        ));

        let run = run_chat_turn(provider, &captured, ("openai", Some(&base))).await;
        server.abort();

        assert_streamed_native_tool_call_ran(Wire::OpenAi, &run, |second| {
            second["messages"].as_array().is_some_and(|messages| {
                messages
                    .iter()
                    .any(|message| message["role"] == "tool" && message["tool_call_id"] == "call_s1")
            })
        });
    }

    // -----------------------------------------------------------------
    // STREAM-f: a half-received tool call never executes
    // -----------------------------------------------------------------

    fn stream_failures(run: &ChatRun) -> Vec<&str> {
        run.actions
            .iter()
            .filter_map(|action| match action {
                Action::StreamFailed { err, .. } => Some(err.as_str()),
                _ => None,
            })
            .collect()
    }

    fn stream_completions(run: &ChatRun) -> Vec<&str> {
        run.actions
            .iter()
            .filter_map(|action| match action {
                Action::StreamCompleted { final_text, .. } => Some(final_text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Drive a chat turn whose offering request is answered with `reply`, with
    /// a tool whose schema requires nothing (so `{}` would pass the execution
    /// contract), and require that the tool never ran and the turn failed with
    /// an error naming `expected_cause`.
    async fn assert_truncated_call_never_runs(wire: Wire, reply: StubReply, expected_cause: &str) {
        let (base, captured, server) = spawn_stub_with(wire, reply).await;
        let provider: Arc<dyn Provider> = match wire {
            Wire::Anthropic => Arc::new(crate::providers::anthropic::AnthropicProvider::with_base_url(
                Some("test-key"),
                Some(&base),
            )),
            Wire::OpenAi => Arc::new(crate::providers::openai::OpenAiProvider::with_base_url(
                Some(&base),
                Some("test-key"),
            )),
        };
        let run = run_chat_turn_with(
            provider,
            &captured,
            false,
            (
                match wire {
                    Wire::Anthropic => "anthropic",
                    Wire::OpenAi => "openai",
                },
                Some(&base),
            ),
        )
        .await;
        server.abort();

        let failures = stream_failures(&run);
        assert_eq!(
            run.tool_calls,
            0,
            "a half-received tool call must never execute (arguments seen: {:?}, requests: {}, \
             completions: {:?}, failures: {failures:?})",
            run.tool_arguments,
            run.requests.len(),
            stream_completions(&run)
        );
        assert_eq!(
            run.requests.len(),
            1,
            "a stream that failed after output is not replayed"
        );
        assert!(
            stream_completions(&run).is_empty(),
            "the turn must not complete as a success"
        );
        assert_eq!(failures.len(), 1, "the turn fails visibly: {:?}", run.actions.len());
        assert!(
            failures.iter().any(|err| err.contains(expected_cause)),
            "the failure names its cause '{expected_cause}': {failures:?}"
        );
    }

    #[tokio::test]
    async fn chat_driver_never_runs_an_anthropic_tool_call_cut_mid_arguments() {
        assert_truncated_call_never_runs(Wire::Anthropic, StubReply::ToolCallCut, "message_stop").await;
    }

    #[tokio::test]
    async fn chat_driver_never_runs_an_anthropic_tool_call_ended_by_an_error_event() {
        assert_truncated_call_never_runs(Wire::Anthropic, StubReply::ToolCallThenError, "overloaded_error").await;
    }

    #[tokio::test]
    async fn chat_driver_never_runs_an_anthropic_tool_call_cut_by_max_tokens() {
        assert_truncated_call_never_runs(Wire::Anthropic, StubReply::ToolCallOutOfTokens, "output cap").await;
    }

    #[tokio::test]
    async fn chat_driver_never_runs_an_openai_tool_call_cut_mid_arguments() {
        assert_truncated_call_never_runs(Wire::OpenAi, StubReply::ToolCallCut, "[DONE]").await;
    }

    #[tokio::test]
    async fn chat_driver_never_runs_an_openai_tool_call_ended_by_an_error_object() {
        assert_truncated_call_never_runs(Wire::OpenAi, StubReply::ToolCallThenError, "server_error").await;
    }

    #[tokio::test]
    async fn chat_driver_never_runs_an_openai_tool_call_cut_by_length() {
        assert_truncated_call_never_runs(Wire::OpenAi, StubReply::ToolCallOutOfTokens, "length").await;
    }

    /// A provider that reports a completed call whose arguments are not JSON,
    /// as any provider without the stream-layer checks would: the loop itself
    /// must refuse to run it.
    struct UnparseableArgumentsProvider {
        histories: Mutex<Vec<Vec<crate::providers::traits::ChatMessage>>>,
    }

    #[async_trait]
    impl Provider for UnparseableArgumentsProvider {
        fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
            crate::providers::traits::ProviderCapabilities {
                native_tool_calling: true,
                vision: false,
            }
        }

        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }

        fn supports_streaming(&self) -> bool {
            true
        }

        fn stream_chat_with_history(
            &self,
            messages: &[crate::providers::traits::ChatMessage],
            _model: &str,
            _temperature: f64,
            _options: crate::providers::traits::StreamOptions,
        ) -> futures::stream::BoxStream<
            'static,
            crate::providers::traits::StreamResult<crate::providers::traits::StreamChunk>,
        > {
            use crate::providers::traits::{StreamChunk, ToolCallChunk};
            use futures::StreamExt;

            let first = {
                let mut histories = self.histories.lock();
                histories.push(messages.to_vec());
                histories.len() == 1
            };
            let frames = if first {
                vec![
                    Ok(StreamChunk::tool_call_chunk(vec![ToolCallChunk::new(
                        "call_bad",
                        FIXTURE_TOOL,
                        r#"{"value":"#,
                        0,
                    )])),
                    Ok(StreamChunk::final_chunk()),
                ]
            } else {
                vec![Ok(StreamChunk::delta("done")), Ok(StreamChunk::final_chunk())]
            };
            futures::stream::iter(frames).boxed()
        }
    }

    #[tokio::test]
    async fn the_loop_never_runs_a_native_tool_call_with_unparseable_arguments() {
        let provider = Arc::new(UnparseableArgumentsProvider {
            histories: Mutex::new(Vec::new()),
        });
        let captured = Arc::new(Mutex::new(Vec::new()));
        let run = run_chat_turn_with(
            Arc::clone(&provider) as Arc<dyn Provider>,
            &captured,
            false,
            ("ollama", None),
        )
        .await;

        assert_eq!(
            run.tool_calls, 0,
            "arguments that are not JSON must not run as `{{}}` (seen: {:?})",
            run.tool_arguments
        );
        let finished: Vec<(bool, Option<&str>)> = run
            .actions
            .iter()
            .filter_map(|action| match action {
                Action::ToolFinished { success, result, .. } => Some((*success, result.as_deref())),
                _ => None,
            })
            .collect();
        assert_eq!(finished.len(), 1, "the user sees the refused call");
        assert!(!finished[0].0, "the refused call is reported as failed");
        assert!(
            finished[0]
                .1
                .is_some_and(|result| result.contains("not a complete JSON object")),
            "the user sees why: {finished:?}"
        );

        // The model is told, on the same call id, and the turn carries on.
        let histories = provider.histories.lock().clone();
        assert_eq!(histories.len(), 2, "the refusal goes back to the model");
        let tool_result = histories[1]
            .iter()
            .find(|message| message.role == "tool")
            .expect("test: the second request carries a tool result");
        let payload: serde_json::Value =
            serde_json::from_str(&tool_result.content).expect("test: native tool result is JSON");
        assert_eq!(payload["tool_call_id"], "call_bad");
        let content = payload["content"].as_str().unwrap_or_default();
        assert!(content.starts_with("Error"), "the model sees an error: {content}");
        assert!(
            content.contains("not a complete JSON object"),
            "the model sees why: {content}"
        );
        assert!(
            content.contains("not executed"),
            "the model sees it did not run: {content}"
        );
        assert_eq!(stream_completions(&run), ["done"]);
    }

    // -----------------------------------------------------------------
    // STREAM-g: truncation is never compacted and replayed; no unfinished
    // call runs with `{}`
    // -----------------------------------------------------------------

    fn anthropic_provider(base: &str) -> Arc<dyn Provider> {
        Arc::new(crate::providers::anthropic::AnthropicProvider::with_base_url(
            Some("test-key"),
            Some(base),
        ))
    }

    fn openai_provider(base: &str) -> Arc<dyn Provider> {
        Arc::new(crate::providers::openai::OpenAiProvider::with_base_url(
            Some(base),
            Some("test-key"),
        ))
    }

    /// F-1: a tool call cut by the output cap on a long conversation must fail
    /// the turn. It must not be read as a context overflow, which would
    /// compact the history and replay the request past the unsafe-replay
    /// refusal.
    #[tokio::test]
    async fn chat_driver_never_compacts_and_replays_a_tool_call_cut_by_max_tokens_on_a_long_history() {
        let (base, captured, server) = spawn_stub_with(Wire::Anthropic, StubReply::ToolCallOutOfTokens).await;
        let run = run_chat_turn_after(
            anthropic_provider(&base),
            &captured,
            false,
            30,
            ("anthropic", Some(&base)),
        )
        .await;
        server.abort();

        let first_len = run
            .requests
            .first()
            .and_then(|request| request["messages"].as_array().map(Vec::len))
            .unwrap_or_default();
        assert!(
            first_len >= 60,
            "the history is long enough to compact: {first_len} messages"
        );
        let failures = stream_failures(&run);
        assert_eq!(
            run.requests.len(),
            1,
            "no compaction and replay (request sizes: {:?}, completions: {:?}, failures: {failures:?})",
            run.requests
                .iter()
                .map(|request| request["messages"].as_array().map_or(0, Vec::len))
                .collect::<Vec<_>>(),
            stream_completions(&run)
        );
        assert_eq!(run.tool_calls, 0);
        assert!(
            stream_completions(&run).is_empty(),
            "the turn must not complete as a success"
        );
        assert_eq!(failures.len(), 1, "the truncation surfaces as an error");
        assert!(
            failures.iter().all(|err| !err.contains("compaction")),
            "the failure is not reported as a failed compaction: {failures:?}"
        );
        assert!(
            run.actions.iter().all(|action| !matches!(
                action,
                Action::HistoryCompacted { .. } | Action::HistoryCompactionPatchApplied { .. }
            )),
            "no compaction patch reaches the chat"
        );
    }

    /// F-1, loop side: a mid-stream upstream error whose text reads like a
    /// context overflow still fails the turn once model output was emitted.
    /// The loop must not compact and replay it, whatever the words say.
    #[tokio::test]
    async fn chat_driver_never_compacts_and_replays_after_output_even_when_the_error_reads_as_overflow() {
        for wire in [Wire::Anthropic, Wire::OpenAi] {
            let (base, captured, server) = spawn_stub_with(
                wire,
                StubReply::ToolCallThenErrorSaying("prompt is too long for the context window"),
            )
            .await;
            let provider = match wire {
                Wire::Anthropic => anthropic_provider(&base),
                Wire::OpenAi => openai_provider(&base),
            };
            let run = run_chat_turn_after(
                provider,
                &captured,
                false,
                30,
                (
                    match wire {
                        Wire::Anthropic => "anthropic",
                        Wire::OpenAi => "openai",
                    },
                    Some(&base),
                ),
            )
            .await;
            server.abort();

            assert_eq!(run.requests.len(), 1, "no compaction and replay");
            assert_eq!(run.tool_calls, 0);
            assert!(stream_completions(&run).is_empty());
            assert_eq!(stream_failures(&run).len(), 1, "the failure is visible");
        }
    }

    /// F-2: a call that got its name but no argument bytes, then a stop other
    /// than `tool_use`, would otherwise run with `{}`.
    #[tokio::test]
    async fn chat_driver_never_runs_an_anthropic_name_only_call_ended_by_refusal() {
        assert_truncated_call_never_runs(
            Wire::Anthropic,
            StubReply::NameOnlyThenStop("refusal"),
            "the model refused",
        )
        .await;
    }

    #[tokio::test]
    async fn chat_driver_never_runs_an_anthropic_name_only_call_ended_by_the_context_window() {
        assert_truncated_call_never_runs(
            Wire::Anthropic,
            StubReply::NameOnlyThenStop("model_context_window_exceeded"),
            "inside a tool call",
        )
        .await;
    }

    /// F-4: `stop` after a complete call (a known server quirk) runs the call,
    /// as the buffered path would.
    #[tokio::test]
    async fn chat_driver_runs_a_complete_openai_call_ended_by_stop() {
        let (base, captured, server) = spawn_stub_with(Wire::OpenAi, StubReply::CompleteCallThenStop).await;
        let run = run_chat_turn_with(openai_provider(&base), &captured, true, ("openai", Some(&base))).await;
        server.abort();

        assert_eq!(run.tool_calls, 1, "the complete call runs");
        assert_eq!(
            run.tool_arguments
                .first()
                .and_then(|arguments| arguments.get("value"))
                .and_then(serde_json::Value::as_str),
            Some("x")
        );
        assert_eq!(run.requests.len(), 2);
        assert_eq!(stream_completions(&run), ["done"]);
    }

    /// F-4: `stop` with a call that never got its arguments must not run it.
    #[tokio::test]
    async fn chat_driver_never_runs_an_openai_name_only_call_ended_by_stop() {
        assert_truncated_call_never_runs(Wire::OpenAi, StubReply::NameOnlyThenStop("stop"), "incomplete").await;
    }

    /// F-4: `content_filter` fails the turn visibly.
    #[tokio::test]
    async fn chat_driver_fails_visibly_on_an_openai_content_filter_stop() {
        assert_truncated_call_never_runs(
            Wire::OpenAi,
            StubReply::NameOnlyThenStop("content_filter"),
            "content_filter",
        )
        .await;
    }

    // -----------------------------------------------------------------
    // COMPAT-STREAM (F-6): a refused call is replayed with sanitized arguments
    // -----------------------------------------------------------------

    /// The loop refuses a call whose arguments are not JSON. The follow-up
    /// request must replay that call with `{}` (the tool result explains why),
    /// not with the raw invalid JSON, which a strict server may reject.
    #[tokio::test]
    async fn a_refused_call_is_replayed_with_sanitized_arguments() {
        for wire in [Wire::Anthropic, Wire::OpenAi] {
            let (base, captured, server) = spawn_stub_with(wire, StubReply::PartialCallThenStop).await;
            let provider = match wire {
                Wire::Anthropic => anthropic_provider(&base),
                Wire::OpenAi => openai_provider(&base),
            };
            let run = run_chat_turn_with(
                provider,
                &captured,
                false,
                (
                    match wire {
                        Wire::Anthropic => "anthropic",
                        Wire::OpenAi => "openai",
                    },
                    Some(&base),
                ),
            )
            .await;
            server.abort();

            assert_eq!(run.tool_calls, 0, "the refused call never runs");
            assert_eq!(run.requests.len(), 2, "the refusal goes back to the model");
            let second = run.requests.get(1).expect("test: second request");
            let replayed: Vec<serde_json::Value> = second["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|message| message["role"] == "assistant")
                .flat_map(|message| match wire {
                    Wire::Anthropic => message["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|block| block["type"] == "tool_use")
                        .map(|block| block["input"].clone())
                        .collect::<Vec<_>>(),
                    Wire::OpenAi => message["tool_calls"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|call| call["function"]["arguments"].clone())
                        .collect::<Vec<_>>(),
                })
                .collect();
            let expected = match wire {
                Wire::Anthropic => serde_json::json!({}),
                Wire::OpenAi => serde_json::json!("{}"),
            };
            assert_eq!(
                replayed,
                vec![expected],
                "the refused call is replayed with `{{}}`: {second}"
            );
            let explained = second.to_string().contains("not a complete JSON object");
            assert!(explained, "the tool result still explains the refusal: {second}");
            assert_eq!(stream_completions(&run), ["done"]);
        }
    }

    /// COMPATf (N-5): arguments that are valid JSON but not an object are
    /// refused by the loop like invalid JSON, and replayed as `{}`. Anthropic
    /// rejects a non-object `input` with a 400, so a raw replay would break the
    /// follow-up request (for example after a fallback to Anthropic).
    #[tokio::test]
    async fn a_call_with_non_object_arguments_is_refused_and_replayed_as_an_empty_object() {
        for wire in [Wire::Anthropic, Wire::OpenAi] {
            for arguments in ["[1]", "\"x\""] {
                let (base, captured, server) = spawn_stub_with(wire, StubReply::CallWithArguments(arguments)).await;
                let provider = match wire {
                    Wire::Anthropic => anthropic_provider(&base),
                    Wire::OpenAi => openai_provider(&base),
                };
                let run = run_chat_turn_with(
                    provider,
                    &captured,
                    false,
                    (
                        match wire {
                            Wire::Anthropic => "anthropic",
                            Wire::OpenAi => "openai",
                        },
                        Some(&base),
                    ),
                )
                .await;
                server.abort();

                assert_eq!(run.tool_calls, 0, "{arguments}: the call never runs");
                assert_eq!(run.requests.len(), 2, "{arguments}: the refusal goes back to the model");
                let second = run.requests.get(1).expect("test: second request");
                let replayed: Vec<serde_json::Value> = second["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|message| message["role"] == "assistant")
                    .flat_map(|message| match wire {
                        Wire::Anthropic => message["content"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter(|block| block["type"] == "tool_use")
                            .map(|block| block["input"].clone())
                            .collect::<Vec<_>>(),
                        Wire::OpenAi => message["tool_calls"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|call| call["function"]["arguments"].clone())
                            .collect::<Vec<_>>(),
                    })
                    .collect();
                let expected = match wire {
                    Wire::Anthropic => serde_json::json!({}),
                    Wire::OpenAi => serde_json::json!("{}"),
                };
                assert_eq!(replayed, vec![expected], "{arguments}: replayed as `{{}}`: {second}");
                let body = second.to_string();
                assert!(body.contains("was not executed"), "{arguments}: {body}");
                assert!(body.contains("not a complete JSON object"), "{arguments}: {body}");
            }
        }
    }
}

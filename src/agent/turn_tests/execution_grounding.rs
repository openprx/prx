#![cfg(test)]

use super::*;
use crate::agent::turn_request::{TurnDelivery, TurnRequest, TurnSchedule, TurnServices};
use parking_lot::Mutex as ParkingMutex;

struct GroundingProvider {
    responses: ParkingMutex<VecDeque<ChatResponse>>,
    requests: ParkingMutex<Vec<Vec<ChatMessage>>>,
}

#[async_trait]
impl Provider for GroundingProvider {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("fixture requires the full request")
    }

    async fn chat(&self, request: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
        self.requests.lock().push(request.messages.to_vec());
        self.responses
            .lock()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("unexpected extra provider call"))
    }
    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        _: &str,
        _: f64,
        _: crate::providers::traits::StreamOptions,
    ) -> futures::stream::BoxStream<
        'static,
        crate::providers::traits::StreamResult<crate::providers::traits::StreamChunk>,
    > {
        use futures::StreamExt as _;
        self.requests.lock().push(messages.to_vec());
        let response = self.responses.lock().pop_front().expect("bounded provider calls");
        futures::stream::iter(vec![Ok(crate::providers::traits::StreamChunk {
            delta: response.text.unwrap(),
            is_final: true,
            ..Default::default()
        })])
        .boxed()
    }
}

async fn exercise_grounding(
    explicit_execution: bool,
    texts: &[&str],
) -> (anyhow::Result<String>, Vec<Vec<ChatMessage>>, Vec<ChatMessage>, String) {
    exercise_grounding_on_stream(explicit_execution, texts, false).await
}

#[derive(Default)]
struct GroundingEvents(ParkingMutex<String>);
#[async_trait]
impl crate::agent::turn_contracts::ToolLoopEventSink for GroundingEvents {
    async fn emit(&self, event: crate::agent::turn_contracts::ToolLoopEvent) -> anyhow::Result<()> {
        if let crate::agent::turn_contracts::ToolLoopEvent::TextDelta(delta) = event {
            self.0.lock().push_str(&delta);
        }
        Ok(())
    }
}

async fn exercise_grounding_on_stream(
    explicit_execution: bool,
    texts: &[&str],
    streaming: bool,
) -> (anyhow::Result<String>, Vec<Vec<ChatMessage>>, Vec<ChatMessage>, String) {
    let temp = TempDir::new().unwrap();
    let input = if explicit_execution {
        "Use file_read to inspect the requested report."
    } else {
        "Explain what shell and file_read do without running anything."
    };
    let execution_order = Arc::new(Mutex::new(Vec::new()));
    let registry = crate::agent::turn_tests::admission::registry_with_search(vec![
        Box::new(RecordingTool {
            name: "file_read".into(),
            execution_order: Arc::clone(&execution_order),
        }),
        Box::new(RecordingTool {
            name: "shell".into(),
            execution_order: Arc::clone(&execution_order),
        }),
    ]);
    let provider = GroundingProvider {
        responses: ParkingMutex::new(
            texts
                .iter()
                .map(|text| ChatResponse {
                    text: Some((*text).into()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
                .collect(),
        ),
        requests: ParkingMutex::new(Vec::new()),
    };
    let driver = if explicit_execution {
        crate::agent::agent::native_decision_fixture::Driver::start("native_tool")
            .await
            .unwrap()
    } else {
        crate::agent::agent::native_decision_fixture::Driver::start_answer()
            .await
            .unwrap()
    };
    let endpoint = driver.endpoint().to_owned();
    driver
        .run_factory(true, || async {
            let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new()).unwrap();
            cleanup
                .run(q79_boxed_fixture_work(async {
                    let mut config = crate::config::Config::default();
                    config.workspace_dir = temp.path().to_path_buf();
                    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
                    let admission = crate::agent::turn_tests::admission::AdmittedTurn::configured_mechanics(
                        config,
                        input,
                        Arc::clone(&registry),
                        memory,
                        &endpoint,
                        &provider,
                    )
                    .await;
                    assert_eq!(
                        matches!(
                            admission.decision().context.directive(),
                            crate::causal_tree::directive::TurnExecutionDirective::Execute { .. }
                        ),
                        explicit_execution
                    );
                    // An old successful-looking assistant message must not count as this turn's execution.
                    let mut history = vec![
                        ChatMessage::system("sys"),
                        ChatMessage::user("Earlier request"),
                        ChatMessage::assistant("NETWORK_PASS, exit status 0."),
                        ChatMessage::user(input),
                    ];
                    let (tx, mut rx) = tokio::sync::mpsc::channel(128);
                    let events = Arc::new(GroundingEvents::default());
                    let mut adapter = admission.adapter();
                    adapter.stream_provider = streaming;
                    adapter.verify_unexecuted_reply = true;
                    adapter.events = Some(events.clone());
                    let result = run_turn(
                        &mut history,
                        TurnRequest {
                            channel_name: "cli",
                            scope: None,
                            mode: ChatMode::default(),
                            silent: true,
                            expose_stay_silent: false,
                            multimodal: &crate::config::MultimodalConfig::default(),
                            schedule: TurnSchedule {
                                read_only_tool_concurrency_window: 2,
                                priority_scheduling_enabled: false,
                                low_priority_tool_names: Vec::new(),
                            },
                            compaction: None,
                            decision: admission.decision(),
                        },
                        TurnServices {
                            provider: &provider,
                            provider_name: "ollama",
                            model: "prx-local-model",
                            temperature: 0.0,
                            tools_registry: Arc::clone(&registry),
                            observer: &NoopObserver,
                            hooks: &HookManager::new(temp.path().to_path_buf()),
                            approval: None,
                            approval_resolver: None,
                            runtime_adapter: Some(adapter),
                            memory: admission.memory_runtime(),
                        },
                        TurnDelivery {
                            cancellation: None,
                            on_delta: Some(tx),
                            on_tool_call: None,
                        },
                    )
                    .await
                    .map(|(outcome, _)| outcome.into_text());
                    let trace = admission.decision().context.terminal_execution_trace();
                    assert_eq!(
                        trace.provider_requests as usize,
                        provider.requests.lock().len(),
                        "the assessment must count as a real provider request"
                    );
                    assert_eq!(trace.provider_pending, 0);
                    let expected_calls = usize::from(texts.iter().any(|text| text.contains(r#""name":"file_read""#)));
                    assert_eq!(execution_order.lock().unwrap().len(), expected_calls);

                    let mut streamed = events.0.lock().clone();
                    while let Ok(delta) = rx.try_recv() {
                        streamed.push_str(&delta);
                    }
                    Ok((result, provider.requests.lock().clone(), history, streamed))
                }))
                .await
                .unwrap()
        })
        .await
}

#[tokio::test]
async fn no_execution_assessment_accepts_explanations_without_side_effects() {
    let answer = "A shell interprets commands. Nothing was executed.";
    let (result, requests, history, streamed) =
        exercise_grounding(false, &[answer, r#"{"requirement":"informational"}"#]).await;
    assert_eq!(result.unwrap(), answer);
    assert_eq!(requests.len(), 2);
    assert!(streamed.contains(answer));
    assert!(!history.iter().any(|message| message.content.contains("requirement")));
}

#[tokio::test]
async fn missing_execution_is_rejected_after_one_correction_in_both_delivery_modes() {
    for streaming in [false, true] {
        let fake = "All operations completed successfully; the report is ready.";
        let (result, requests, history, streamed) = exercise_grounding_on_stream(
            true,
            &[fake, r#"{"requirement":"execution_required"}"#, fake],
            streaming,
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("no tool result"));
        assert_eq!(requests.len(), 3);
        assert!(!history.iter().any(|message| message.content == fake));
        assert!(
            streamed.is_empty(),
            "unverified draft must never reach either text sink"
        );
    }
}

#[tokio::test]
async fn required_execution_can_recover_with_one_actual_call() {
    let fake = "The report has already been inspected.";
    let call = r#"<tool_call>{"name":"file_read","arguments":{"path":"report.txt"}}</tool_call>"#;
    let (result, requests, history, _) = exercise_grounding(
        true,
        &[
            fake,
            r#"{"requirement":"execution_required"}"#,
            call,
            "The requested read returned file_read.",
        ],
    )
    .await;
    assert!(result.unwrap().contains("returned file_read"));
    assert_eq!(requests.len(), 4);
    assert!(!history.iter().any(|message| message.content == fake));
}

#[tokio::test]
async fn actual_tool_result_skips_semantic_assessment() {
    let call = r#"<tool_call>{"name":"file_read","arguments":{"path":"report.txt"}}</tool_call>"#;
    let (result, requests, _, _) = exercise_grounding(true, &[call, "The read completed."]).await;
    assert_eq!(result.unwrap(), "The read completed.");
    assert_eq!(requests.len(), 2);
}

#[tokio::test]
async fn uncertain_or_invalid_assessment_never_delivers_a_completion_claim() {
    for verdict in [r#"{"requirement":"uncertain"}"#, "not valid JSON"] {
        let fake = "The command ran successfully.";
        let (result, requests, history, streamed) = exercise_grounding(true, &[fake, verdict]).await;
        assert!(result.is_err());
        assert_eq!(requests.len(), 2);
        assert_eq!(streamed, "");
        assert!(!history.iter().any(|message| message.content == fake));
    }
}

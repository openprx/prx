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
    let input = if explicit_execution {
        "Use file_read to inspect the requested report."
    } else {
        "Explain what shell and file_read do without running anything."
    };
    exercise_grounding_request(explicit_execution, texts, streaming, input).await
}

async fn exercise_grounding_request(
    explicit_execution: bool,
    texts: &[&str],
    streaming: bool,
    input: &str,
) -> (anyhow::Result<String>, Vec<Vec<ChatMessage>>, Vec<ChatMessage>, String) {
    let temp = TempDir::new().unwrap();
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
                    if !crate::causal_tree::decision_select::pure_textual_reply(input) {
                        assert_eq!(
                            matches!(
                                admission.decision().context.directive(),
                                crate::causal_tree::directive::TurnExecutionDirective::Execute { .. }
                            ),
                            explicit_execution
                        );
                    }
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
                    adapter.verify_chat_completion = true;
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
                    let expected_calls = texts
                        .iter()
                        .filter(|text| text.contains(r#""name":"file_read""#))
                        .count();
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
async fn pure_textual_reply_degrades_failed_execution_prediction_to_answer() {
    let hello = "hello";
    let (result, requests, history, streamed) = exercise_grounding_request(
        true,
        &[
            hello,
            r#"{"requirement":"execution_required"}"#,
            hello,
            r#"{"requirement":"execution_required"}"#,
        ],
        false,
        "Reply with exactly one word: hello",
    )
    .await;
    let delivered = result.expect("pure textual reply delivers the draft");
    assert_eq!(delivered, hello);
    assert!(!requests.is_empty());
    assert!(
        history.iter().any(|message| message.content == hello) || streamed.contains(hello),
        "the draft must be delivered on a history or stream sink"
    );
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
            r#"{"decision":"accept","reason":"read result supports reply","constraints":[]}"#,
        ],
    )
    .await;
    assert!(result.unwrap().contains("returned file_read"));
    assert_eq!(requests.len(), 5);
    assert!(!history.iter().any(|message| message.content == fake));
}

#[tokio::test]
async fn actual_tool_result_is_reviewed_against_current_request() {
    let call = r#"<tool_call>{"name":"file_read","arguments":{"path":"report.txt"}}</tool_call>"#;
    for streaming in [false, true] {
        let (result, requests, _, _) = exercise_grounding_on_stream(
            true,
            &[
                call,
                r#"{"requirement":"execution_required"}"#,
                "The read completed.",
                r#"{"decision":"accept","reason":"supported","constraints":[]}"#,
            ],
            streaming,
        )
        .await;
        assert_eq!(result.unwrap(), "The read completed.");
        assert_eq!(requests.len(), 4);
        let review = requests.last().unwrap().last().unwrap();
        assert!(review.content.contains("observed_tools"));
        assert!(review.content.contains("report.txt"));
        assert!(review.content.contains("succeeded"));
        let data: serde_json::Value = serde_json::from_str(&review.content).unwrap();
        for field in ["selected_model", "served_model"] {
            assert_eq!(
                data.pointer(&format!("/runtime_model_identity/{field}/text"))
                    .and_then(serde_json::Value::as_str),
                Some("prx-local-model")
            );
        }
    }
}

#[tokio::test]
async fn completion_review_rejects_missing_deliverable_draft_and_allows_verified_correction() {
    for streaming in [false, true] {
        let call = r#"<tool_call>{"name":"file_read","arguments":{"path":"report.txt"}}</tool_call>"#;
        let fake = "Every requested deliverable has been saved and verified.";
        let answer = "The requested report was inspected; no implementation was requested.";
        let (result, requests, history, streamed) = exercise_grounding_on_stream(true, &[
            call, r#"{"requirement":"execution_required"}"#, fake,
            r#"{"decision":"revise","reason":"The draft claims saved deliverables without evidence; inspect the requested report and answer only that request.","constraints":[]}"#,
            call, answer,
            r#"{"decision":"accept","reason":"The corrected reply matches the actual requested read.","constraints":[]}"#,
        ], streaming).await;
        assert_eq!(result.unwrap(), answer);
        assert_eq!(requests.len(), 7);
        assert!(!history.iter().any(|m| m.content == fake));
        assert!(!streamed.contains(fake));
        assert!(streamed.contains(answer));
        assert!(
            requests
                .iter()
                .flatten()
                .any(|m| m.content.contains("Preserve the user's scope"))
        );
    }
}

#[tokio::test]
async fn completion_review_does_not_loop_or_deliver_a_rejected_draft() {
    let call = r#"<tool_call>{"name":"file_read","arguments":{"path":"report.txt"}}</tool_call>"#;
    let fake = "The implementation file is ready.";
    let verdict = r#"{"decision":"revise","reason":"The requested implementation is not evidenced.","constraints":[]}"#;
    let (result, requests, history, streamed) = exercise_grounding(
        true,
        &[
            call,
            r#"{"requirement":"execution_required"}"#,
            fake,
            verdict,
            fake,
            verdict,
        ],
    )
    .await;
    assert!(result.unwrap_err().to_string().contains("after one corrective attempt"));
    assert_eq!(requests.len(), 6);
    assert!(!history.iter().any(|m| m.content == fake));
    assert!(!streamed.contains(fake));
}

#[tokio::test]
async fn completion_review_rejects_invalid_or_uncertain_evidence() {
    let call = r#"<tool_call>{"name":"file_read","arguments":{"path":"report.txt"}}</tool_call>"#;
    for verdict in [
        "invalid",
        r#"{"decision":"uncertain","reason":"Required evidence was omitted.","constraints":[]}"#,
    ] {
        let (result, _, history, streamed) = exercise_grounding(
            true,
            &[call, r#"{"requirement":"execution_required"}"#, "All done.", verdict],
        )
        .await;
        assert!(result.is_err());
        assert!(!history.iter().any(|m| m.content == "All done."));
        assert!(!streamed.contains("All done."));
    }
}

#[tokio::test]
async fn uncertain_or_invalid_assessment_never_delivers_a_completion_claim() {
    for verdict in [r#"{"requirement":"uncertain"}"#, "not valid JSON"] {
        let fake = "The command ran successfully.";
        let (result, requests, history, streamed) = exercise_grounding(true, &[fake, verdict]).await;
        if verdict.contains("uncertain") {
            let clarification = result.unwrap();
            assert!(clarification.contains("clarify"));
            assert!(!clarification.contains(fake));
            assert!(!streamed.contains(fake));
        } else {
            assert!(result.is_err());
            assert_eq!(streamed, "");
        }
        assert_eq!(requests.len(), 2);
        assert!(!history.iter().any(|message| message.content == fake));
    }
}

#[tokio::test]
async fn completion_review_cannot_accept_an_exceeded_call_limit_or_stream_its_draft() {
    for streaming in [false, true] {
        let call = r#"<tool_call>{"name":"file_read","arguments":{"path":"report.txt"}}</tool_call>"#;
        let fake = "I read the report exactly once.";
        let (result, requests, history, streamed) = exercise_grounding_request(true, &[
            call, r#"{"requirement":"execution_required"}"#, call, fake,
            r#"{"decision":"accept","reason":"The requested read succeeded.","constraints":[{"kind":"max_calls","request_quote":"file_read exactly once","tool":"file_read","limit":1}]}"#,
        ], streaming, "Use file_read exactly once to inspect report.txt.").await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("violated an explicit request constraint")
        );
        assert_eq!(
            requests.len(),
            5,
            "a violation must not cause another execution attempt"
        );
        assert!(!history.iter().any(|message| message.content == fake));
        assert!(!streamed.contains(fake));
    }
}

#[tokio::test]
async fn completion_review_repairs_extraction_once_without_repeating_execution() {
    let call = r#"<tool_call>{"name":"file_read","arguments":{"path":"report.txt"}}</tool_call>"#;
    let invalid = r#"{"decision":"accept","reason":"Read succeeded.","constraints":[{"kind":"max_calls","request_quote":"read the file only once","tool":"file_read","limit":1}]}"#;
    let valid = r#"{"decision":"accept","reason":"Read succeeded.","constraints":[{"kind":"max_calls","request_quote":"file_read exactly once","tool":"file_read","limit":1}]}"#;
    for streaming in [false, true] {
        for repaired in [true, false] {
            let answer = "The report was inspected once.";
            let (result, requests, history, streamed) = exercise_grounding_request(
                true,
                &[
                    call,
                    r#"{"requirement":"execution_required"}"#,
                    answer,
                    invalid,
                    if repaired { valid } else { invalid },
                ],
                streaming,
                "Use file_read exactly once to inspect report.txt.",
            )
            .await;
            assert_eq!(
                requests.len(),
                5,
                "one intent assessment, one execution and two auxiliary reviews only"
            );
            assert!(
                requests
                    .last()
                    .unwrap()
                    .iter()
                    .any(|m| m.content.contains("invalid_review"))
            );
            let repaired_request = requests.last().unwrap();
            let evidence: serde_json::Value = serde_json::from_str(&repaired_request.last().unwrap().content).unwrap();
            assert_eq!(
                evidence["current_request"],
                "Use file_read exactly once to inspect report.txt."
            );
            assert_eq!(evidence["executed_tool_counts"]["file_read"], 1);
            assert_eq!(evidence["proposed_final_reply"], answer);
            assert_eq!(
                repaired_request
                    .iter()
                    .filter(|message| message.role == "system")
                    .count(),
                1
            );
            assert_eq!(repaired_request.first().unwrap().role, "system");
            if repaired {
                assert_eq!(result.unwrap(), answer);
                assert!(streamed.contains(answer));
            } else {
                assert!(result.unwrap_err().to_string().contains("not quoted"));
                assert!(!streamed.contains(answer));
                assert!(!history.iter().any(|m| m.content == answer));
            }
        }
    }
}

#[tokio::test]
async fn proposed_real_writes_wait_for_one_source_intent_even_at_full_autonomy() {
    for (requirement, repeated_proposal, writes) in [
        ("informational", false, false),
        ("informational", true, false),
        ("uncertain", false, false),
        ("invalid", false, false),
        ("execution_required", false, true),
    ] {
        let temp = TempDir::new().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = temp.path().to_owned();
        config.autonomy.level = crate::security::AutonomyLevel::Full;
        let security = Arc::new(crate::security::SecurityPolicy::from_config(
            &config.autonomy,
            temp.path(),
        ));
        let registry = crate::agent::turn_tests::admission::registry_with_search(vec![Box::new(
            crate::tools::file_write::FileWriteTool::new(security),
        )]);
        let call = r#"<tool_call>{"name":"file_write","arguments":{"path":"prx-intent.txt","content":"PRX_INTENT_VERIFIED"}}</tool_call>"#;
        let mut texts = vec![
            call.to_string(),
            if requirement == "invalid" {
                "not valid JSON".to_string()
            } else {
                format!("{{\"requirement\":\"{requirement}\"}}")
            },
        ];
        if writes {
            texts.extend([
                "The artifact was created.".to_string(),
                r#"{"decision":"accept","reason":"The actual write supports the reply.","constraints":[]}"#.to_string(),
            ]);
        } else if requirement == "informational" {
            texts.push(
                if repeated_proposal {
                    call
                } else {
                    "Present. No operation has run."
                }
                .to_string(),
            );
        }
        let provider = GroundingProvider {
            responses: ParkingMutex::new(
                texts
                    .into_iter()
                    .map(|text| ChatResponse {
                        text: Some(text),
                        tool_calls: Vec::new(),
                        reasoning_content: None,
                    })
                    .collect(),
            ),
            requests: ParkingMutex::new(Vec::new()),
        };
        let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
            .await
            .unwrap();
        let endpoint = driver.endpoint().to_owned();
        driver
            .run_factory(true, || async {
                let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new()).unwrap();
                cleanup
                    .run(q79_boxed_fixture_work(async {
                        let input = if writes {
                            "Create the requested artifact in the workspace."
                        } else {
                            "Are you still there?"
                        };
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
                        let mut adapter = admission.adapter();
                        adapter.verify_chat_completion = true;
                        let mut history = vec![ChatMessage::system("sys"), ChatMessage::user(input)];
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
                                    read_only_tool_concurrency_window: 1,
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
                                hooks: &HookManager::new(temp.path().to_owned()),
                                approval: None,
                                approval_resolver: None,
                                runtime_adapter: Some(adapter),
                                memory: admission.memory_runtime(),
                            },
                            TurnDelivery {
                                cancellation: None,
                                on_delta: None,
                                on_tool_call: None,
                            },
                        )
                        .await;
                        if requirement == "invalid" {
                            assert!(result.is_err());
                        } else {
                            let reply = result.unwrap().0.into_text();
                            if requirement == "uncertain" || repeated_proposal {
                                assert!(reply.contains("clarify"));
                            } else {
                                assert_ne!(reply, "");
                            }
                        }
                        let path = temp.path().join("prx-intent.txt");
                        assert_eq!(path.exists(), writes, "unexpected file effect for {requirement}");
                        if writes {
                            assert_eq!(std::fs::read_to_string(path).unwrap(), "PRX_INTENT_VERIFIED");
                        }
                        let trace = admission.decision().context.terminal_execution_trace();
                        assert_eq!(trace.started_attempts, u64::from(writes));
                        let requests = provider.requests.lock();
                        assert_eq!(trace.provider_requests as usize, requests.len());
                        assert_eq!(trace.provider_pending, 0);
                        assert_eq!(
                            requests
                                .iter()
                                .filter(|messages| messages.iter().any(|message| {
                                    message.role == "system"
                                        && message.content.contains("Classify the execution requirement")
                                }))
                                .count(),
                            1,
                            "intent must not be reinterpreted after a proposal"
                        );
                        assert!(
                            !history
                                .iter()
                                .any(|message| message.content.contains("proposed tool calls were not dispatched"))
                        );
                        Ok(())
                    }))
                    .await
                    .unwrap()
            })
            .await;
    }
}

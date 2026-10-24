#![cfg(test)]
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

#[tokio::test]
async fn run_tool_call_loop_executes_read_only_tools_with_bounded_parallelism() {
    let provider = ScriptedProvider::from_text_responses(vec![
        r#"<tool_call>
{"name":"capability_search","arguments":{"query":"file_read","limit":1}}
</tool_call>
<tool_call>
{"name":"capability_search","arguments":{"query":"memory_get","limit":1}}
</tool_call>"#,
        r#"<tool_call>
{"name":"file_read","arguments":{"value":"A"}}
</tool_call>
<tool_call>
{"name":"memory_get","arguments":{"value":"B"}}
</tool_call>"#,
        "done",
    ]);

    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let tools_registry: Vec<Box<dyn Tool>> = vec![
        Box::new(crate::tools::capability::search_tool::CapabilitySearchTool),
        Box::new(DelayTool::new(
            "file_read",
            200,
            Arc::clone(&active),
            Arc::clone(&max_active),
        )),
        Box::new(DelayTool::new(
            "memory_get",
            200,
            Arc::clone(&active),
            Arc::clone(&max_active),
        )),
    ];

    let workspace = TempDir::new().unwrap();
    let input = "Use file_read and memory_get.";
    let mut history = vec![ChatMessage::system("test-system"), ChatMessage::user(input)];
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let mut mechanics_config = crate::config::Config::default();
    mechanics_config.workspace_dir = workspace.path().to_path_buf();
    let mechanics_memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).expect("mechanics memory"));
    // All targeted fixture tools use native adapters with no filesystem/memory category.
    // Driver validates an actual use_tool/native_tool candidate in the compiled request.
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("native_tool")
        .await
        .expect("owned mechanics decision listener");
    let endpoint = driver.endpoint().to_owned();
    driver
        .run_factory(true, || async {
            use futures::FutureExt as _;
            let cleanup =
                crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                    .expect("mechanics cleanup owner");
            let mut original_panic = None;
            let drained = cleanup
                .run(q79_boxed_fixture_work(async {
                    let caught = std::panic::AssertUnwindSafe(q79_boxed_fixture_work(async {
                        let (admission, progress_trace) = crate::runtime::turn_decision::progress_diagnostic::capture(
                            crate::agent::turn_tests::admission::AdmittedTurn::configured_mechanics(
                                mechanics_config,
                                input,
                                Arc::clone(&tools_registry),
                                mechanics_memory,
                                &endpoint,
                                &provider,
                            ),
                        )
                        .await;
                        assert!(!progress_trace.overflowed(), "complete bounded stop-operand diagnostic");
                        eprintln!("actual_parallel_progress={progress_trace:?}");
                        // Observe the actual admitted fixture before the original engine assertions.
                        let decision = admission.decision();
                        let diagnostic = decision
                            .context
                            .audit_hops()
                            .map(|hop| {
                                serde_json::json!({
                                    "selection": format!("{:?}", hop.selection),
                                    "prediction": format!("{:?}", hop.prediction),
                                    "candidates": format!("{:?}", hop.candidates),
                                    "post_rehearsal": format!("{:?}", hop.post_rehearsal),
                                    "pre_commit": format!("{:?}", hop.pre_commit),
                                    "audit": format!("{:?}", hop.audit_trace),
                                })
                            })
                            .collect::<Vec<_>>();
                        eprintln!(
                            "actual_parallel_admission={}",
                            serde_json::to_string(&diagnostic).unwrap()
                        );
                        let observer = NoopObserver;

                        let started = std::time::Instant::now();
                        let result = run_turn(
                            &mut history,
                            crate::agent::turn_request::TurnRequest {
                                channel_name: "cli",
                                scope: None,
                                mode: ChatMode::default(),
                                silent: true,
                                expose_stay_silent: false,
                                multimodal: &crate::config::MultimodalConfig::default(),
                                schedule: crate::agent::turn_request::TurnSchedule {
                                    read_only_tool_concurrency_window: 2,
                                    priority_scheduling_enabled: false,
                                    low_priority_tool_names: Vec::new(),
                                },
                                compaction: None,
                                decision: admission.decision(),
                            },
                            crate::agent::turn_request::TurnServices {
                                provider: &provider,
                                provider_name: "ollama",
                                model: "prx-local-model",
                                temperature: 0.0,
                                tools_registry,
                                observer: &observer,
                                hooks: &crate::hooks::HookManager::new(std::env::temp_dir()),
                                approval: None,
                                approval_resolver: None,
                                runtime_adapter: Some(admission.adapter()),
                                // no tool tiering
                                memory: admission.memory_runtime(),
                            },
                            crate::agent::turn_request::TurnDelivery {
                                cancellation: None,
                                on_delta: None,
                                // no scope context
                                on_tool_call: None,
                            },
                        )
                        .await
                        .map(|(outcome, _trace)| outcome.into_text())
                        .expect("read-only parallel execution should complete");
                        let elapsed = started.elapsed();

                        assert_eq!(result, "done");
                        let business_results = history
                            .iter()
                            .find(|message| {
                                message.role == "user"
                                    && message.content.starts_with("[Tool results]")
                                    && message.content.contains("name=\"file_read\"")
                                    && message.content.contains("name=\"memory_get\"")
                            })
                            .expect("both business results must be in the same tool round");
                        assert!(business_results.content.contains("ok:A"));
                        assert!(business_results.content.contains("ok:B"));
                        assert!(
                            elapsed < Duration::from_secs(2),
                            "parallel execution should complete within a reasonable bound; elapsed={elapsed:?}"
                        );
                        assert!(
                            max_active.load(Ordering::SeqCst) >= 2,
                            "both tools should overlap in execution"
                        );

                        let tool_results_message = business_results;
                        let idx_a = tool_results_message
                            .content
                            .find("name=\"file_read\"")
                            .expect("file_read result should be present");
                        let idx_b = tool_results_message
                            .content
                            .find("name=\"memory_get\"")
                            .expect("memory_get result should be present");
                        assert!(
                            idx_a < idx_b,
                            "tool results should preserve input order for tool call mapping"
                        );
                    }))
                    .catch_unwind()
                    .await;
                    if let Err(panic) = caught {
                        original_panic = Some(panic);
                    }
                    Ok(())
                }))
                .await;
            if let Some(panic) = original_panic {
                std::panic::resume_unwind(panic);
            }
            drained.expect("mechanics services must drain");
        })
        .await;
}

#[tokio::test]
async fn run_tool_call_loop_persists_large_tool_output_documents() {
    // Size the payload off the constant so it stays "large" regardless of
    // Behavior-limits Phase 1 raising MAX_TOOL_RESULT_INLINE_CHARS.
    let segment = "large document payload ";
    let repeats = MAX_TOOL_RESULT_INLINE_CHARS / segment.len() + 100;
    let large_output = format!("{}\n{}", "needle source evidence marker", segment.repeat(repeats));
    assert!(large_output.len() > MAX_TOOL_RESULT_INLINE_CHARS);
    let provider = ScriptedProvider::from_text_responses(vec![
        r#"<tool_call>
{"name":"large_output","arguments":{}}
</tool_call>"#,
        "done",
    ]);
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(LargeOutputTool {
        output: large_output.clone(),
    })];
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let input = "Use large_output.";
    let mut history = vec![ChatMessage::system("test-system"), ChatMessage::user(input)];
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let mut config = crate::config::Config::default();
    config.workspace_dir = tmp.path().to_path_buf();
    // All targeted fixture tools use native adapters with no filesystem/memory category.
    // Driver validates an actual use_tool/native_tool candidate in the compiled request.
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("native_tool")
        .await
        .expect("owned mechanics decision listener");
    let endpoint = driver.endpoint().to_owned();
    driver
        .run_factory(true, || async {
            use futures::FutureExt as _;
            let cleanup =
                crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                    .expect("mechanics cleanup owner");
            let mut original_panic = None;
            let drained = cleanup
                .run(q79_boxed_fixture_work(async {
                    let caught = std::panic::AssertUnwindSafe(q79_boxed_fixture_work(async {
                        let admission = crate::agent::turn_tests::admission::AdmittedTurn::configured_mechanics(
                            config,
                            input,
                            Arc::clone(&tools_registry),
                            Arc::clone(&memory),
                            &endpoint,
                            &provider,
                        )
                        .await;
                        admission.assert_core_specs(&tool_specs_of(&tools_registry));

                        let result = run_turn(
                            &mut history,
                            crate::agent::turn_request::TurnRequest {
                                channel_name: "cli",
                                scope: None,
                                mode: ChatMode::default(),
                                silent: true,
                                expose_stay_silent: false,
                                multimodal: &crate::config::MultimodalConfig::default(),
                                schedule: crate::agent::turn_request::TurnSchedule {
                                    read_only_tool_concurrency_window: 2,
                                    priority_scheduling_enabled: false,
                                    low_priority_tool_names: Vec::new(),
                                },
                                compaction: None,
                                decision: admission.decision(),
                            },
                            crate::agent::turn_request::TurnServices {
                                provider: &provider,
                                provider_name: "ollama",
                                model: "prx-local-model",
                                temperature: 0.0,
                                tools_registry,
                                observer: &NoopObserver,
                                hooks: &crate::hooks::HookManager::new(tmp.path().to_path_buf()),
                                approval: None,
                                approval_resolver: None,
                                runtime_adapter: Some(admission.adapter()),
                                memory: admission.memory_runtime_with_ingest(),
                            },
                            crate::agent::turn_request::TurnDelivery {
                                cancellation: None,
                                on_delta: None,
                                on_tool_call: None,
                            },
                        )
                        .await
                        .map(|(outcome, _trace)| outcome.into_text())
                        .expect("large output tool loop should complete");

                        assert_eq!(result, "done");
                        let tool_history = history
                            .iter()
                            .map(|message| message.content.as_str())
                            .find(|content| content.contains("[document_ingest_ref]"))
                            .expect("tool history should include document ingest ref");
                        assert!(tool_history.contains("status: persisted_document_backend"));

                        let chunks = memory
                            .search_document_chunks(&admission.principal(), "needle source evidence", 5)
                            .await
                            .unwrap();
                        assert_eq!(chunks.len(), 1);
                        assert!(chunks[0].chunk.document_id.starts_with("tool-output:large_output:"));
                        assert_eq!(
                            chunks[0].chunk.source_anchor,
                            format!("{}#chunk-0", chunks[0].chunk.document_id)
                        );
                    }))
                    .catch_unwind()
                    .await;
                    if let Err(panic) = caught {
                        original_panic = Some(panic);
                    }
                    Ok(())
                }))
                .await;
            if let Some(panic) = original_panic {
                std::panic::resume_unwind(panic);
            }
            drained.expect("mechanics services must drain");
        })
        .await;
}

#[tokio::test]
async fn run_tool_call_loop_keeps_stateful_tools_strictly_serial() {
    let provider = ScriptedProvider::from_text_responses(vec![
        r#"<tool_call>
{"name":"capability_search","arguments":{"query":"delay_stateful_a","limit":1}}
</tool_call>
<tool_call>
{"name":"capability_search","arguments":{"query":"delay_stateful_b","limit":1}}
</tool_call>"#,
        r#"<tool_call>
{"name":"delay_stateful_a","arguments":{"value":"A"}}
</tool_call>
<tool_call>
{"name":"delay_stateful_b","arguments":{"value":"B"}}
</tool_call>"#,
        "done",
    ]);

    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let tools_registry: Vec<Box<dyn Tool>> = vec![
        Box::new(crate::tools::capability::search_tool::CapabilitySearchTool),
        Box::new(DelayTool::new(
            "delay_stateful_a",
            200,
            Arc::clone(&active),
            Arc::clone(&max_active),
        )),
        Box::new(DelayTool::new(
            "delay_stateful_b",
            200,
            Arc::clone(&active),
            Arc::clone(&max_active),
        )),
    ];

    let workspace = TempDir::new().unwrap();
    let input = "Use delay_stateful_a and delay_stateful_b.";
    let mut history = vec![ChatMessage::system("test-system"), ChatMessage::user(input)];
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let mut mechanics_config = crate::config::Config::default();
    mechanics_config.workspace_dir = workspace.path().to_path_buf();
    let mechanics_memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).expect("mechanics memory"));
    // All targeted fixture tools use native adapters with no filesystem/memory category.
    // Driver validates an actual use_tool/native_tool candidate in the compiled request.
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("native_tool")
        .await
        .expect("owned mechanics decision listener");
    let endpoint = driver.endpoint().to_owned();
    driver
        .run_factory(true, || async {
            use futures::FutureExt as _;
            let cleanup =
                crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                    .expect("mechanics cleanup owner");
            let mut original_panic = None;
            let drained = cleanup
                .run(q79_boxed_fixture_work(async {
                    let caught = std::panic::AssertUnwindSafe(q79_boxed_fixture_work(async {
                        let admission = crate::agent::turn_tests::admission::AdmittedTurn::configured_mechanics(
                            mechanics_config,
                            input,
                            Arc::clone(&tools_registry),
                            mechanics_memory,
                            &endpoint,
                            &provider,
                        )
                        .await;
                        let observer = NoopObserver;

                        let started = std::time::Instant::now();
                        let result = run_turn(
                            &mut history,
                            crate::agent::turn_request::TurnRequest {
                                channel_name: "cli",
                                scope: None,
                                mode: ChatMode::default(),
                                silent: true,
                                expose_stay_silent: false,
                                multimodal: &crate::config::MultimodalConfig::default(),
                                schedule: crate::agent::turn_request::TurnSchedule {
                                    read_only_tool_concurrency_window: 2,
                                    priority_scheduling_enabled: false,
                                    low_priority_tool_names: Vec::new(),
                                },
                                compaction: None,
                                decision: admission.decision(),
                            },
                            crate::agent::turn_request::TurnServices {
                                provider: &provider,
                                provider_name: "ollama",
                                model: "prx-local-model",
                                temperature: 0.0,
                                tools_registry,
                                observer: &observer,
                                hooks: &crate::hooks::HookManager::new(std::env::temp_dir()),
                                approval: None,
                                approval_resolver: None,
                                runtime_adapter: Some(admission.adapter()),
                                // no tool tiering
                                memory: admission.memory_runtime(),
                            },
                            crate::agent::turn_request::TurnDelivery {
                                cancellation: None,
                                on_delta: None,
                                // no scope context
                                on_tool_call: None,
                            },
                        )
                        .await
                        .map(|(outcome, _trace)| outcome.into_text())
                        .expect("stateful serial execution should complete");
                        let elapsed = started.elapsed();

                        assert_eq!(result, "done");
                        let business_results = history
                            .iter()
                            .find(|message| {
                                message.role == "user"
                                    && message.content.starts_with("[Tool results]")
                                    && message.content.contains("name=\"delay_stateful_a\"")
                                    && message.content.contains("name=\"delay_stateful_b\"")
                            })
                            .expect("both business results must be in the same tool round");
                        assert!(business_results.content.contains("ok:A"));
                        assert!(business_results.content.contains("ok:B"));
                        assert!(
                            elapsed >= Duration::from_millis(360),
                            "stateful tools should execute serially; elapsed={elapsed:?}"
                        );
                        assert_eq!(
                            max_active.load(Ordering::SeqCst),
                            1,
                            "stateful tools should never overlap in execution"
                        );
                    }))
                    .catch_unwind()
                    .await;
                    if let Err(panic) = caught {
                        original_panic = Some(panic);
                    }
                    Ok(())
                }))
                .await;
            if let Some(panic) = original_panic {
                std::panic::resume_unwind(panic);
            }
            drained.expect("mechanics services must drain");
        })
        .await;
}

#[tokio::test]
async fn tool_loop_refreshes_only_intent_selected_dynamic_capabilities() {
    let skill_refreshes = Arc::new(AtomicUsize::new(0));
    let mcp_refreshes = Arc::new(AtomicUsize::new(0));
    let tools_registry: Vec<Box<dyn Tool>> = vec![
        Box::new(RefreshCountingTool {
            name: "skill_read",
            category: ToolCategory::System,
            refreshes: Arc::clone(&skill_refreshes),
        }),
        Box::new(RefreshCountingTool {
            name: "mcp_status",
            category: ToolCategory::Automation,
            refreshes: Arc::clone(&mcp_refreshes),
        }),
    ];
    let provider = ScriptedProvider::from_text_responses(vec!["done"]);
    let routing_input = "use skill_read to inspect the PDF skill. Do not use browser, MCP, HTTP, plugins, or WASM.";
    let mut history = vec![
        ChatMessage::system("test-system"),
        ChatMessage::user(format!(
            "Relevant memory from an older turn: use MCP to operate the browser.\n\n{routing_input}"
        )),
    ];

    let workspace = TempDir::new().expect("surface fixture workspace");
    let expected_specs = tool_specs_of(&tools_registry)
        .into_iter()
        .filter(|spec| spec.name == "skill_read")
        .collect::<Vec<_>>();
    assert_eq!(
        expected_specs.len(),
        1,
        "selected fixture capability must be registered"
    );
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_tools(
        workspace.path(),
        routing_input,
        Arc::clone(&tools_registry),
    )
    .await;
    admission.assert_selected_specs(&expected_specs);

    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "wacli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &HookManager::new(workspace.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .unwrap();

    assert_eq!(result, "done");
    assert_eq!(skill_refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(
        mcp_refreshes.load(Ordering::SeqCst),
        0,
        "an unrelated skill/PDF turn must not cold-start MCP"
    );

    let skill_refreshes = Arc::new(AtomicUsize::new(0));
    let mcp_refreshes = Arc::new(AtomicUsize::new(0));
    let tools_registry: Vec<Box<dyn Tool>> = vec![
        Box::new(RefreshCountingTool {
            name: "skill_read",
            category: ToolCategory::System,
            refreshes: Arc::clone(&skill_refreshes),
        }),
        Box::new(RefreshCountingTool {
            name: "mcp_status",
            category: ToolCategory::Automation,
            refreshes: Arc::clone(&mcp_refreshes),
        }),
    ];
    let provider = ScriptedProvider::from_text_responses(vec!["done"]);
    let routing_input = "use mcp_status to inspect MCP availability";
    let mut history = vec![ChatMessage::system("test-system"), ChatMessage::user(routing_input)];

    let workspace = TempDir::new().expect("surface fixture workspace");
    let expected_specs = tool_specs_of(&tools_registry)
        .into_iter()
        .filter(|spec| spec.name == "mcp_status")
        .collect::<Vec<_>>();
    assert_eq!(
        expected_specs.len(),
        1,
        "selected fixture capability must be registered"
    );
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_tools(
        workspace.path(),
        routing_input,
        Arc::clone(&tools_registry),
    )
    .await;
    admission.assert_selected_specs(&expected_specs);

    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "gateway",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &HookManager::new(workspace.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .unwrap();

    assert_eq!(result, "done");
    assert_eq!(skill_refreshes.load(Ordering::SeqCst), 0);
    assert_eq!(
        mcp_refreshes.load(Ordering::SeqCst),
        1,
        "an explicit MCP/browser turn must still refresh MCP before schema exposure"
    );
}

#[tokio::test]
async fn prompt_guided_provider_receives_only_the_compiled_turn_tool_snapshot() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let provider = PayloadCapturingProvider {
        captured: Arc::clone(&captured),
    };
    let tools_registry: Vec<Box<dyn Tool>> = vec![
        Box::new(RefreshCountingTool {
            name: "skill_read",
            category: ToolCategory::System,
            refreshes: Arc::new(AtomicUsize::new(0)),
        }),
        Box::new(RefreshCountingTool {
            name: "mcp_status",
            category: ToolCategory::Automation,
            refreshes: Arc::new(AtomicUsize::new(0)),
        }),
    ];
    let routing_input = "use skill_read to inspect the PDF skill. Do not use browser, MCP, HTTP, plugins, or WASM.";
    let mut history = vec![ChatMessage::system("base-system"), ChatMessage::user(routing_input)];

    let workspace = TempDir::new().expect("surface fixture workspace");
    let expected_specs = tool_specs_of(&tools_registry)
        .into_iter()
        .filter(|spec| spec.name == "skill_read")
        .collect::<Vec<_>>();
    assert_eq!(
        expected_specs.len(),
        1,
        "selected fixture capability must be registered"
    );
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_tools(
        workspace.path(),
        routing_input,
        Arc::clone(&tools_registry),
    )
    .await;
    admission.assert_selected_specs(&expected_specs);

    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "gateway",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &HookManager::new(workspace.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .expect("prompt-guided turn should complete");

    assert_eq!(result, "answered");
    let payload = captured.lock().expect("captured payload should be available");
    let system = payload
        .iter()
        .find(|message| message.role == "system")
        .expect("request should contain a system message");
    assert_eq!(system.content.matches("## Runtime Capabilities").count(), 1);
    assert!(system.content.contains("`skill_read`"));
    assert!(!system.content.contains("`mcp_status`"));
    assert_eq!(system.content.matches("## Tool Use Protocol").count(), 1);
    assert!(system.content.contains("**skill_read**"));
    assert!(
        !system.content.contains("**mcp_status**"),
        "prompt-guided instructions must not advertise a tool excluded by turn intent"
    );
}

#[tokio::test]
async fn tool_loop_rejects_a_registered_tool_not_exposed_for_the_turn() {
    let provider = ScriptedProvider::from_text_responses(vec![
        "<tool_call>\n{\"name\":\"mcp_status\",\"arguments\":{}}\n</tool_call>",
        "done",
    ]);
    let tools_registry: Vec<Box<dyn Tool>> = vec![
        Box::new(RefreshCountingTool {
            name: "skill_read",
            category: ToolCategory::System,
            refreshes: Arc::new(AtomicUsize::new(0)),
        }),
        Box::new(RefreshCountingTool {
            name: "mcp_status",
            category: ToolCategory::Automation,
            refreshes: Arc::new(AtomicUsize::new(0)),
        }),
    ];
    let routing_input = "use skill_read to inspect the PDF skill. Do not use browser, MCP, HTTP, plugins, or WASM.";
    let mut history = vec![ChatMessage::system("base-system"), ChatMessage::user(routing_input)];

    let workspace = TempDir::new().expect("surface fixture workspace");
    let expected_specs = tool_specs_of(&tools_registry)
        .into_iter()
        .filter(|spec| spec.name == "skill_read")
        .collect::<Vec<_>>();
    assert_eq!(
        expected_specs.len(),
        1,
        "selected fixture capability must be registered"
    );
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_tools(
        workspace.path(),
        routing_input,
        Arc::clone(&tools_registry),
    )
    .await;
    admission.assert_selected_specs(&expected_specs);

    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "gateway",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &HookManager::new(workspace.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .expect("turn should recover after rejecting an unexposed tool");

    assert_eq!(result, "done");
    assert!(history.iter().any(|message| {
        message
            .content
            .contains("tool 'mcp_status' was not exposed for this turn and was not executed")
    }));
}

#[tokio::test]
async fn turn_exposure_guard_preserves_unknown_tool_diagnostics() {
    let provider = ScriptedProvider::from_text_responses(vec![
        "<tool_call>\n{\"name\":\"invented_probe\",\"arguments\":{}}\n</tool_call>",
        "done",
    ]);
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(RefreshCountingTool {
        name: "skill_read",
        category: ToolCategory::System,
        refreshes: Arc::new(AtomicUsize::new(0)),
    })];
    let routing_input = "use skill_read to inspect the PDF skill";
    let mut history = vec![ChatMessage::system("base-system"), ChatMessage::user(routing_input)];

    let workspace = TempDir::new().expect("surface fixture workspace");
    let expected_specs = tool_specs_of(&tools_registry)
        .into_iter()
        .filter(|spec| spec.name == "skill_read")
        .collect::<Vec<_>>();
    assert_eq!(
        expected_specs.len(),
        1,
        "selected fixture capability must be registered"
    );
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_tools(
        workspace.path(),
        routing_input,
        Arc::clone(&tools_registry),
    )
    .await;
    admission.assert_selected_specs(&expected_specs);

    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "gateway",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &HookManager::new(workspace.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .expect("turn should recover after reporting an unknown tool");

    assert_eq!(result, "done");
    assert!(
        history
            .iter()
            .any(|message| message.content.contains("unknown tool 'invented_probe'"))
    );
    assert!(history.iter().all(|message| {
        !message
            .content
            .contains("tool 'invented_probe' was not exposed for this turn")
    }));
}

/// A provider whose call never returns and never errors — the exact shape
/// of the hang the idle detector exists to recover from.
struct HangingProvider;

#[async_trait]
impl Provider for HangingProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        std::future::pending().await
    }

    async fn chat(&self, _request: ChatRequest<'_>, _model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
        std::future::pending().await
    }
}

/// Core evidence 1, at the production entrypoint: a turn whose provider
/// call never returns is terminated by the idle detector, and the error
/// identifies it as a hang rather than a failure or an operator kill.
///
/// Runs on the paused runtime clock for the reason given on
/// `production_turn_far_longer_than_the_idle_window_survives_on_progress`:
/// the window is then measured in the same virtual time the watchdog
/// sleeps in, so the verdict is reached because the turn is silent and not
/// because a loaded machine got there first.
#[tokio::test(start_paused = true)]
async fn production_turn_that_hangs_is_terminated_by_the_idle_detector() {
    // Q52: evidence prerequisite only; preserve idle watchdog failure and reason.
    q79_boxed_fixture_work(
        crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(q79_boxed_fixture_work(async {
            let provider = HangingProvider;
            let mut history = vec![ChatMessage::system("test-system"), ChatMessage::user("hang please")];
            let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
            let observer = NoopObserver;

            let workspace = TempDir::new().expect("idle fixture workspace");
            let admission =
                crate::agent::turn_tests::admission::AdmittedTurn::empty(workspace.path(), "hang please").await;

            let guard = crate::agent::idle::IdleGuard {
                idle: Some(Duration::from_millis(300)),
            };
            let error = crate::agent::idle::with_guard(guard, async {
                run_turn(
                    &mut history,
                    crate::agent::turn_request::TurnRequest {
                        channel_name: "telegram",
                        scope: None,
                        // no document ingest
                        mode: ChatMode::default(),
                        silent: true,
                        expose_stay_silent: false,
                        multimodal: &crate::config::MultimodalConfig::default(),
                        schedule: crate::agent::turn_request::TurnSchedule {
                            read_only_tool_concurrency_window: 2,
                            priority_scheduling_enabled: false,
                            low_priority_tool_names: Vec::new(),
                        },
                        compaction: None,
                        decision: admission.decision(),
                    },
                    crate::agent::turn_request::TurnServices {
                        provider: &provider,
                        provider_name: "ollama",
                        model: "prx-local-model",
                        temperature: 0.0,
                        tools_registry: Arc::new(tools_registry),
                        observer: &observer,
                        hooks: &crate::hooks::HookManager::new(workspace.path().to_path_buf()),
                        approval: None,
                        approval_resolver: None,
                        runtime_adapter: Some(admission.adapter()),
                        // no tool tiering
                        memory: ToolLoopMemory::none(),
                    },
                    crate::agent::turn_request::TurnDelivery {
                        cancellation: None,
                        on_delta: None,
                        // no scope context
                        on_tool_call: None,
                    },
                )
                .await
                .map(|(outcome, _trace)| outcome.into_text())
            })
            .await
            .expect_err("a turn whose provider never responds must be terminated");

            assert!(
                crate::agent::idle::is_hang_termination(&error),
                "the production loop must surface a hang termination, got: {error}"
            );
            assert!(
                error.to_string().contains("no observable progress"),
                "the message must say why: {error}"
            );
        })),
    )
    .await;
}

/// Core evidence 2, at the production entrypoint, and the whole point of
/// this mechanism: a turn that runs many times longer than the idle window
/// but keeps working runs to its natural end untouched.
///
/// MUTATION: make the idle check in `crate::agent::idle::verdict` read
/// total elapsed time instead of time since the last progress event and
/// this test fails, because the turn's total run time is six times the
/// window while its longest silence is a fraction of it.
///
/// # Why the runtime clock is paused
///
/// The fixture window is 200ms, which is 150 times below the smallest
/// window `[runtime] idle_hang_secs` will accept
/// ([`crate::agent::idle::MIN_IDLE_HANG_SECS`]). On wall-clock time that
/// turns every ordinary silent moment of the real tool round trip into a
/// candidate verdict: the durable idempotency-ledger writes around each
/// call are `spawn_blocking` hops into SQLite, and on a contended CI runner
/// one of them outlasts 200ms perfectly often — which is exactly how this
/// test failed on GitHub run 35396387399 ("Last progress: tool_end after 14
/// event(s)", the turn sitting in `commit_idempotent_execution`).
///
/// Pausing the clock removes the race instead of hiding it. Virtual time
/// advances only when the runtime has nothing to run, and tokio refuses to
/// auto-advance while a blocking task is outstanding, so the real SQLite
/// writes — which this test deliberately keeps, they are the production
/// path — cost zero virtual time while the tool's own
/// `tokio::time::sleep` still spends the full 120ms of it. The guard
/// measures silence on that same clock
/// ([`crate::agent::idle::ProgressBeat`] reads `tokio::time::Instant`), so
/// what is being asserted is the contract and nothing else. The control
/// below proves virtual time really does reach the verdict when the turn
/// really does go quiet.
#[tokio::test(start_paused = true)]
async fn production_turn_far_longer_than_the_idle_window_survives_on_progress() {
    const ROUNDS: usize = 10;
    const TOOL_DELAY_MS: u64 = 120;
    let idle_window = Duration::from_millis(200);

    let mut scripted: Vec<&str> = vec![
        r#"<tool_call>
{"name":"delay_progress","arguments":{"value":"tick"}}
</tool_call>"#;
        ROUNDS
    ];
    scripted.push("done");
    let provider = ScriptedProvider::from_text_responses(scripted);

    let executions = Arc::new(AtomicUsize::new(0));
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(SteadyTool {
        delay: Duration::from_millis(TOOL_DELAY_MS),
        executions: Arc::clone(&executions),
    })];
    let workspace = TempDir::new().unwrap();
    let input = "Use delay_progress.";
    let mut history = vec![ChatMessage::system("test-system"), ChatMessage::user(input)];
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let mut mechanics_config = crate::config::Config::default();
    mechanics_config.workspace_dir = workspace.path().to_path_buf();
    let mechanics_memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).expect("mechanics memory"));
    // All targeted fixture tools use native adapters with no filesystem/memory category.
    // Driver validates an actual use_tool/native_tool candidate in the compiled request.
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("native_tool")
        .await
        .expect("owned mechanics decision listener");
    let endpoint = driver.endpoint().to_owned();
    driver
        .run_factory(true, || async {
            use futures::FutureExt as _;
            let cleanup =
                crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                    .expect("mechanics cleanup owner");
            let mut original_panic = None;
            let drained = cleanup
                .run(q79_boxed_fixture_work(async {
                    let caught = std::panic::AssertUnwindSafe(q79_boxed_fixture_work(async {
                        // The compatible decision endpoint is real loopback I/O. Let that
                        // admission run on wall time, then restore the paused clock before
                        // the idle guard starts measuring the production tool loop.
                        tokio::time::resume();
                        let admission = crate::agent::turn_tests::admission::AdmittedTurn::configured_mechanics(
                            mechanics_config,
                            input,
                            Arc::clone(&tools_registry),
                            mechanics_memory,
                            &endpoint,
                            &provider,
                        )
                        .await;
                        tokio::time::pause();
                        admission.assert_core_specs(&tool_specs_of(&tools_registry));
                        let observer = NoopObserver;

                        let guard = crate::agent::idle::IdleGuard {
                            idle: Some(idle_window),
                        };
                        // The runtime clock, so the elapsed-time assertion below measures the
                        // same time the guard does.
                        let started = tokio::time::Instant::now();
                        let result = crate::agent::idle::with_guard(guard, async {
                            run_turn(
                                &mut history,
                                crate::agent::turn_request::TurnRequest {
                                    channel_name: "cli",
                                    scope: None,
                                    mode: ChatMode::default(),
                                    silent: true,
                                    expose_stay_silent: false,
                                    multimodal: &crate::config::MultimodalConfig::default(),
                                    schedule: crate::agent::turn_request::TurnSchedule {
                                        read_only_tool_concurrency_window: 2,
                                        priority_scheduling_enabled: false,
                                        low_priority_tool_names: Vec::new(),
                                    },
                                    compaction: None,
                                    decision: admission.decision(),
                                },
                                crate::agent::turn_request::TurnServices {
                                    provider: &provider,
                                    provider_name: "ollama",
                                    model: "prx-local-model",
                                    temperature: 0.0,
                                    tools_registry,
                                    observer: &observer,
                                    hooks: &crate::hooks::HookManager::new(std::env::temp_dir()),
                                    approval: None,
                                    approval_resolver: None,
                                    runtime_adapter: Some(admission.adapter()),
                                    // no tool tiering
                                    memory: admission.memory_runtime(),
                                },
                                crate::agent::turn_request::TurnDelivery {
                                    cancellation: None,
                                    on_delta: None,
                                    // no scope context
                                    on_tool_call: None,
                                },
                            )
                            .await
                            .map(|(outcome, _trace)| outcome.into_text())
                        })
                        .await
                        .expect("a turn that keeps making progress must never be terminated");
                        let elapsed = started.elapsed();

                        assert_eq!(result, "done", "the turn must reach its own natural end");
                        assert!(
                            elapsed > idle_window * 4,
                            "the test is only meaningful if the turn outlives the idle window several times over; \
             elapsed={elapsed:?} window={idle_window:?} rounds={}",
                            executions.load(Ordering::SeqCst)
                        );
                        assert_eq!(
                            executions.load(Ordering::SeqCst),
                            ROUNDS,
                            "every scripted round must have done real work"
                        );
                    }))
                    .catch_unwind()
                    .await;
                    if let Err(panic) = caught {
                        original_panic = Some(panic);
                    }
                    Ok(())
                }))
                .await;
            if let Some(panic) = original_panic {
                std::panic::resume_unwind(panic);
            }
            drained.expect("mechanics services must drain");
        })
        .await;
}

/// The control for the test above, and the half of the contract that must
/// never be traded away for a green: the same production entrypoint, the
/// same fixture, the same paused clock — but the tool stays quiet for
/// longer than one whole window, and the turn is terminated with the hang
/// report.
///
/// Without this, the survival test's green would be ambiguous: a paused
/// clock that never advanced would also "survive". Here virtual time
/// advances to the watchdog's deadline while the turn is genuinely silent,
/// the verdict is reached mid-tool, and the tool never gets to finish —
/// so the guard is provably still armed under exactly the conditions the
/// other test runs in.
#[tokio::test(start_paused = true)]
async fn production_turn_whose_tool_goes_silent_past_the_window_is_terminated() {
    let idle_window = Duration::from_millis(200);

    let provider = ScriptedProvider::from_text_responses(vec![
        r#"<tool_call>
{"name":"delay_progress","arguments":{"value":"tick"}}
</tool_call>"#,
        "done",
    ]);

    let executions = Arc::new(AtomicUsize::new(0));
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(SteadyTool {
        // Five windows of complete silence inside a single tool call: this
        // is a stall by the module's own definition, not a slow turn.
        delay: idle_window * 5,
        executions: Arc::clone(&executions),
    })];
    let workspace = TempDir::new().unwrap();
    let input = "Use delay_progress.";
    let mut history = vec![ChatMessage::system("test-system"), ChatMessage::user(input)];
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let mut mechanics_config = crate::config::Config::default();
    mechanics_config.workspace_dir = workspace.path().to_path_buf();
    let mechanics_memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).expect("mechanics memory"));
    // All targeted fixture tools use native adapters with no filesystem/memory category.
    // Driver validates an actual use_tool/native_tool candidate in the compiled request.
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("native_tool")
        .await
        .expect("owned mechanics decision listener");
    let endpoint = driver.endpoint().to_owned();
    driver
        .run_factory(true, || async {
            use futures::FutureExt as _;
            let cleanup =
                crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                    .expect("mechanics cleanup owner");
            let mut original_panic = None;
            let drained = cleanup
                .run(q79_boxed_fixture_work(async {
                    let caught = std::panic::AssertUnwindSafe(q79_boxed_fixture_work(async {
                        // Admission uses a real compatible loopback endpoint; restore the
                        // virtual clock only after that I/O, before the watchdog is armed.
                        tokio::time::resume();
                        let admission = crate::agent::turn_tests::admission::AdmittedTurn::configured_mechanics(
                            mechanics_config,
                            input,
                            Arc::clone(&tools_registry),
                            mechanics_memory,
                            &endpoint,
                            &provider,
                        )
                        .await;
                        tokio::time::pause();
                        admission.assert_core_specs(&tool_specs_of(&tools_registry));
                        let observer = NoopObserver;

                        let guard = crate::agent::idle::IdleGuard {
                            idle: Some(idle_window),
                        };
                        let error = crate::agent::idle::with_guard(guard, async {
                            run_turn(
                                &mut history,
                                crate::agent::turn_request::TurnRequest {
                                    channel_name: "cli",
                                    scope: None,
                                    mode: ChatMode::default(),
                                    silent: true,
                                    expose_stay_silent: false,
                                    multimodal: &crate::config::MultimodalConfig::default(),
                                    schedule: crate::agent::turn_request::TurnSchedule {
                                        read_only_tool_concurrency_window: 2,
                                        priority_scheduling_enabled: false,
                                        low_priority_tool_names: Vec::new(),
                                    },
                                    compaction: None,
                                    decision: admission.decision(),
                                },
                                crate::agent::turn_request::TurnServices {
                                    provider: &provider,
                                    provider_name: "ollama",
                                    model: "prx-local-model",
                                    temperature: 0.0,
                                    tools_registry,
                                    observer: &observer,
                                    hooks: &crate::hooks::HookManager::new(std::env::temp_dir()),
                                    approval: None,
                                    approval_resolver: None,
                                    runtime_adapter: Some(admission.adapter()),
                                    // no tool tiering
                                    memory: admission.memory_runtime(),
                                },
                                crate::agent::turn_request::TurnDelivery {
                                    cancellation: None,
                                    on_delta: None,
                                    // no scope context
                                    on_tool_call: None,
                                },
                            )
                            .await
                            .map(|(outcome, _trace)| outcome.into_text())
                        })
                        .await
                        .expect_err("a turn silent for longer than the whole window must be terminated");
                        // The watchdog assertion above uses virtual time. Queue
                        // shutdown below awaits real SQLite / blocking work; do
                        // not let an idle virtual clock skip its drain deadline.
                        tokio::time::resume();

                        assert!(
                            crate::agent::idle::is_hang_termination(&error),
                            "the stall must surface as a hang termination, got: {error}"
                        );
                        assert!(
                            error.to_string().contains("no observable progress"),
                            "the hang report must say why: {error}"
                        );
                        assert_eq!(
                            executions.load(Ordering::SeqCst),
                            0,
                            "the verdict must land while the tool is still silent, not after it returned"
                        );
                    }))
                    .catch_unwind()
                    .await;
                    if let Err(panic) = caught {
                        original_panic = Some(panic);
                    }
                    Ok(())
                }))
                .await;
            if let Some(panic) = original_panic {
                std::panic::resume_unwind(panic);
            }
            drained.expect("mechanics services must drain");
        })
        .await;
}

/// Appends one line per execution, so "did it run twice?" is answered by
/// counting lines in a real file rather than by trusting a counter.
struct AuditAppendTool {
    path: std::path::PathBuf,
}

#[async_trait]
impl Tool for AuditAppendTool {
    fn name(&self) -> &str {
        "append_line"
    }

    fn description(&self) -> &str {
        "Append one line to the audit file"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        writeln!(file, "appended")?;
        Ok(crate::tools::ToolResult {
            success: true,
            output: "appended one line".to_string(),
            error: None,
        })
    }
}

/// Never returns, so its batch is still in flight when the turn is ended.
struct NeverReturningTool;

#[async_trait]
impl Tool for NeverReturningTool {
    fn name(&self) -> &str {
        "wedge"
    }

    fn description(&self) -> &str {
        "Never returns"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        std::future::pending().await
    }
}

fn interrupted_batch_registry(audit: &std::path::Path) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(AuditAppendTool {
            path: audit.to_path_buf(),
        }) as Box<dyn Tool>,
        Box::new(NeverReturningTool) as Box<dyn Tool>,
    ]
}

/// **Core evidence 3** — a tool batch interrupted partway through still
/// leaves the calls that already ran recorded in history.
///
/// History used to be written only once the *whole* batch had finished. An
/// interruption partway through it — the commonest way a batch ends badly —
/// therefore left the calls that had already run, and had already had their
/// side effects, with no record at all, and anything that rebuilt the turn
/// from that history re-issued every one of them.
///
/// Here `append_line` runs (one line in the audit file) and `wedge` never
/// returns, so the stall detector ends the turn with the batch half done.
/// The assistant message that requested the calls and the result of the call
/// that completed must both survive, and the call that did not complete must
/// be marked as not having happened.
///
/// MUTATION: moving the `history.push(ChatMessage::assistant(..))` and the
/// tool-result messages back to after `execute_tools_with_service` — that
/// is, deleting `ExecutedToolRecords` — turns this test red.
#[tokio::test]
async fn an_interrupted_tool_batch_still_records_the_calls_that_ran() {
    let dir = TempDir::new().expect("test: temp dir should be creatable");
    let audit = dir.path().join("audit.log");
    let ledger_dir = TempDir::new().expect("test: ledger dir should be creatable");
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(interrupted_batch_registry(&audit));
    let provider = ScriptedProvider::from_text_responses(vec![
        r#"<tool_call>
{"name":"capability_search","arguments":{"query":"wedge","limit":1}}
</tool_call>"#,
        "<tool_call>\n{\"name\":\"append_line\",\"arguments\":{}}\n</tool_call>\n<tool_call>\n{\"name\":\"wedge\",\"arguments\":{}}\n</tool_call>",
    ]);
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("Use append_line, then wedge."),
    ];
    let hooks_dir = TempDir::new().expect("test: hooks dir should be creatable");
    let mut mechanics_config = crate::config::Config::default();
    mechanics_config.workspace_dir = ledger_dir.path().to_path_buf();
    let mechanics_memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(ledger_dir.path()).expect("mechanics memory"));
    // All targeted fixture tools use native adapters with no filesystem/memory category.
    // Driver validates an actual use_tool/native_tool candidate in the compiled request.
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("native_tool")
        .await
        .expect("owned mechanics decision listener");
    let endpoint = driver.endpoint().to_owned();
    driver
        .run_factory(true, || async {
            use futures::FutureExt as _;
            let cleanup =
                crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                    .expect("mechanics cleanup owner");
            let mut original_panic = None;
            let drained = cleanup
                .run(q79_boxed_fixture_work(async {
                    let caught = std::panic::AssertUnwindSafe(q79_boxed_fixture_work(async {
                        let admission = crate::agent::turn_tests::admission::AdmittedTurn::configured_mechanics(
                            mechanics_config,
                            &history.last().expect("source input").content,
                            Arc::clone(&tools_registry),
                            mechanics_memory,
                            &endpoint,
                            &provider,
                        )
                        .await;

                        let result = crate::agent::idle::with_guard(
                            crate::agent::idle::IdleGuard {
                                idle: Some(Duration::from_millis(200)),
                            },
                            run_turn(
                                &mut history,
                                crate::agent::turn_request::TurnRequest {
                                    channel_name: "cli",
                                    scope: None,
                                    mode: ChatMode::default(),
                                    silent: true,
                                    expose_stay_silent: false,
                                    multimodal: &crate::config::MultimodalConfig::default(),
                                    schedule: crate::agent::turn_request::TurnSchedule {
                                        read_only_tool_concurrency_window: 2,
                                        priority_scheduling_enabled: false,
                                        low_priority_tool_names: Vec::new(),
                                    },
                                    compaction: None,
                                    decision: admission.decision(),
                                },
                                crate::agent::turn_request::TurnServices {
                                    provider: &provider,
                                    provider_name: "ollama",
                                    model: "prx-local-model",
                                    temperature: 0.0,
                                    tools_registry,
                                    observer: &NoopObserver,
                                    hooks: &crate::hooks::HookManager::new(hooks_dir.path().to_path_buf()),
                                    approval: None,
                                    approval_resolver: None,
                                    runtime_adapter: Some(admission.adapter()),
                                    memory: admission.memory_runtime(),
                                },
                                crate::agent::turn_request::TurnDelivery {
                                    cancellation: None,
                                    on_delta: None,
                                    on_tool_call: None,
                                },
                            ),
                        )
                        .await;

                        let error = result.expect_err("the wedged batch must end the turn");
                        assert!(
                            crate::agent::idle::is_hang_termination(&error),
                            "the turn must end through the stall detector, not a clock: {error}"
                        );

                        let audited = std::fs::read_to_string(&audit)
                            .expect("test: the appending tool must have written the file");
                        assert_eq!(
                            audited.lines().filter(|line| !line.is_empty()).count(),
                            1,
                            "the appending tool must have run exactly once: {audited:?}"
                        );

                        assert!(
                            history
                                .iter()
                                .any(|message| message.role == "assistant" && message.content.contains("append_line")),
                            "the assistant message requesting the batch must survive the interruption: {history:?}"
                        );
                        let record = history
                            .iter()
                            .find(|message| {
                                message.role == "user"
                                    && message
                                        .content
                                        .starts_with(crate::agent::turn_contracts::TOOL_RESULTS_PREFIX)
                                    && message.content.contains("name=\"append_line\"")
                            })
                            .expect("the executed call must leave a tool-result record in history");
                        assert!(
                            record.content.contains("appended one line"),
                            "the completed call's own output must be what is recorded: {:?}",
                            record.content
                        );
                        assert!(
                            record.content.contains(INTERRUPTED_TOOL_RESULT),
                            "the call that never completed must be marked as not having happened: {:?}",
                            record.content
                        );
                    }))
                    .catch_unwind()
                    .await;
                    if let Err(panic) = caught {
                        original_panic = Some(panic);
                    }
                    Ok(())
                }))
                .await;
            if let Some(panic) = original_panic {
                std::panic::resume_unwind(panic);
            }
            drained.expect("mechanics services must drain");
        })
        .await;
}

/// A tool whose only job is to be present in the registry under a chosen
/// name, so a test can ask which names one turn published.
struct SurfaceProbeTool {
    name: &'static str,
}

#[async_trait]
impl Tool for SurfaceProbeTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "surface probe"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        Ok(crate::tools::ToolResult {
            success: true,
            output: String::new(),
            error: None,
        })
    }
}

/// A backend that publishes a name other than its own, the way an MCP
/// router or a WASM alias does. `channel_exclude` names the *published*
/// tool, so an exclusion matched against the root name alone never reaches
/// this one.
struct SurfaceAliasProbeTool;

#[async_trait]
impl Tool for SurfaceAliasProbeTool {
    fn name(&self) -> &str {
        "ops_router"
    }

    fn description(&self) -> &str {
        "surface alias probe"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    fn specs(&self) -> Vec<crate::tools::ToolSpec> {
        vec![crate::tools::ToolSpec {
            name: "nodes".to_string(),
            description: "surface alias probe".to_string(),
            parameters: self.parameters_schema(),
        }]
    }

    fn supports_name(&self, name: &str) -> bool {
        name == "ops_router" || name == "nodes"
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        Ok(crate::tools::ToolResult {
            success: true,
            output: String::new(),
            error: None,
        })
    }
}

/// Uses the actual dispatch-issued caller to construct an ordinary child.
struct ChildSurfaceProbe {
    parent_committed: Arc<std::sync::atomic::AtomicBool>,
    controlled_family: &'static str,
    registry: Arc<std::sync::OnceLock<std::sync::Weak<Vec<Box<dyn Tool>>>>>,
}

#[async_trait]
impl Tool for ChildSurfaceProbe {
    fn name(&self) -> &str {
        "sessions_spawn"
    }
    fn description(&self) -> &str {
        "Spawn the requested capability publication fixture"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"input":{"type":"string"}},"required":["input"]})
    }
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        let caller = crate::tools::execution::current_child_call_context()
            .expect("child fixture must run through the execution service");
        caller.verify_tool("sessions_spawn")?;
        let publication = caller
            .context()
            .ordinary_child_publication
            .as_ref()
            .expect("selected parent delegation publication");
        publication.verify_caller(caller.context())?;
        anyhow::ensure!(
            self.parent_committed
                .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
            "child phase requires one committed parent dispatch"
        );

        let (provider_name, model, credential) = publication.model_source();
        let binding = crate::runtime::child_source::ChildProviderBinding::build_ordinary(
            &publication.generation().effective,
            publication.provider_catalog(),
            provider_name,
            model,
            credential.clone(),
            Some(publication),
        )?;
        let provider = binding.provider();
        let input = args["input"].as_str().expect("child input");
        let parent = &caller.context().envelope;
        let mut scope = parent.message_scope();
        scope.source = crate::memory::MessageEventSource::SessionsSpawn;
        scope.visibility = crate::memory::MemoryVisibility::Workspace;
        scope.session_key = Some("prx_child_session".to_owned());
        scope.run_id = Some("prx_child_run".to_owned());
        scope.agent_id = None;
        binding.bind_ordinary_input_scope(&provider, &parent.workspace_id, &mut scope, Some(&caller))?;
        let fabric = publication.source_fabric();
        let receipt = crate::runtime::child_source::record_child_input(&fabric, scope.clone(), input, None, None, None)
            .await?
            .expect("durable child input");
        let source = binding.ordinary_execution_source(&provider, &caller, &scope, input, Some(&receipt))?;
        let mut service = publication.child_service(caller.context(), None)?;
        crate::runtime::turn_decision::TurnDecisionService::install_ordinary_child_fixture(
            &mut service,
            "use_tool",
            self.controlled_family,
        )?;
        let owner = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
        let cancellation = tokio_util::sync::CancellationToken::new();
        owner.retain_service(Arc::clone(&service));
        use futures::FutureExt as _;
        let caught = std::panic::AssertUnwindSafe(async {
            let child = crate::runtime::child_turn::ChildSegmentTurn::admit(
                service,
                source,
                fabric,
                Arc::clone(&publication.generation().effective),
                &owner,
                &cancellation,
                None,
            )
            .await?;
            anyhow::ensure!(
                child.routing().context.ordinary_fixture_prediction_verified(),
                "actual child Mock prediction required"
            );
            let child_routing = child.routing();
            let child_selection = child_routing
                .service
                .agent_execution_specs(child_routing.proof, child_routing.context)
                .unwrap_or_else(|error| {
                    let hops = child_routing
                        .context
                        .audit_hops()
                        .map(|hop| {
                            format!(
                                "selection={:?} candidates={:?} prediction={:?} pre_commit={:?}",
                                hop.selection, hop.candidates, hop.prediction, hop.pre_commit
                            )
                        })
                        .collect::<Vec<_>>();
                    panic!("actual child tool selection required: {error:#}; {hops:?}")
                });
            assert!(
                child_selection.specs.iter().any(|spec| spec.name == input),
                "child must select its requested allowed capability: {:?}",
                child_selection.specs
            );
            let registry = self
                .registry
                .get()
                .and_then(std::sync::Weak::upgrade)
                .expect("live parent registry");
            let runtime = publication.loop_runtime()?;
            let mut history = vec![ChatMessage::system("child fixture"), ChatMessage::user(input)];
            let multimodal = crate::config::MultimodalConfig::default();
            let result = run_turn(
                &mut history,
                crate::agent::turn_request::TurnRequest {
                    channel_name: "sessions_spawn",
                    scope: None,
                    mode: ChatMode::default(),
                    silent: true,
                    expose_stay_silent: false,
                    multimodal: &multimodal,
                    schedule: crate::agent::turn_request::TurnSchedule {
                        read_only_tool_concurrency_window: 2,
                        priority_scheduling_enabled: false,
                        low_priority_tool_names: Vec::new(),
                    },
                    compaction: None,
                    decision: child_routing,
                },
                crate::agent::turn_request::TurnServices {
                    provider: provider.as_ref(),
                    provider_name,
                    model,
                    temperature: 0.0,
                    tools_registry: registry,
                    observer: runtime.observer.as_ref(),
                    hooks: runtime.hooks.as_ref(),
                    approval: None,
                    approval_resolver: None,
                    runtime_adapter: Some(child.adapter()),
                    memory: ToolLoopMemory::none(),
                },
                crate::agent::turn_request::TurnDelivery {
                    cancellation: Some(cancellation),
                    on_delta: None,
                    on_tool_call: None,
                },
            )
            .await?;
            child.complete(&result.0, &result.1).await?;
            Ok(crate::tools::ToolResult {
                success: true,
                output: "child completed".to_owned(),
                error: None,
            })
        })
        .catch_unwind()
        .await;
        let drained = owner.drain(tokio::time::Instant::now() + Duration::from_secs(15)).await;
        match caught {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(result) => {
                drained?;
                result
            }
        }
    }
}

struct ChildFixtureServer(tokio::task::JoinHandle<()>);

impl Drop for ChildFixtureServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn published_child_tools(tiering: crate::config::ToolTieringConfig, input: &str) -> Vec<String> {
    let captured = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let requests = Arc::clone(&captured);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/api/chat",
        axum::routing::post(move |axum::Json(request): axum::Json<serde_json::Value>| {
            let requests = Arc::clone(&requests);
            async move {
                requests.lock().unwrap().push(request);
                axum::Json(serde_json::json!({
                    "model":"prx-local-model", "message":{"role":"assistant","content":"done"},
                    "done":true, "prompt_eval_count":1, "eval_count":1,
                }))
            }
        }),
    );
    let mut server = ChildFixtureServer(tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }));
    let tmp = TempDir::new().unwrap();
    let mut config = crate::config::Config::default();
    config.workspace_dir = tmp.path().to_owned();
    config.causal_tree.enabled = false;
    config.default_provider = Some("ollama".to_owned());
    config.api_url = Some(endpoint);
    config.tool_tiering = tiering;
    config.autonomy.level = crate::security::AutonomyLevel::Full;
    config.agent.compaction.max_context_tokens_explicit = true;
    config.causal_tree.policy.extra_latency_budget_ms = 5_000;
    config.cost.prices.insert(
        "prx-local-model".to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let parent_committed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let controlled_family = match input {
        "shell" => "shell",
        "gateway" | "nodes" => "native_tool",
        _ => panic!("unreviewed ordinary child target"),
    };
    let registry_slot = Arc::new(std::sync::OnceLock::new());
    let registry = crate::agent::test_admission::registry_with_search(vec![
        Box::new(SurfaceProbeTool { name: "gateway" }),
        Box::new(SurfaceProbeTool { name: "shell" }),
        Box::new(SurfaceAliasProbeTool),
        Box::new(ChildSurfaceProbe {
            parent_committed: Arc::clone(&parent_committed),
            controlled_family,
            registry: Arc::clone(&registry_slot),
        }),
    ]);
    registry_slot.set(Arc::downgrade(&registry)).unwrap();
    let parent_input = "sessions_spawn shell gateway nodes";
    let message = crate::channels::traits::ChannelMessage {
        id: "prx-parent-input".to_owned(),
        sender: "prx_user".to_owned(),
        reply_target: "prx_channel".to_owned(),
        content: parent_input.to_owned(),
        channel: "wacli".to_owned(),
        timestamp: 0,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        thread_ts: None,
        mentioned_uuids: Vec::new(),
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    };
    let mut server_joined = false;
    use futures::FutureExt as _;
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
        .expect("ordinary parent cleanup owner");
    let mut caught = None;
    let drained = cleanup
        .run(q79_boxed_fixture_work(async {
            caught = Some(
                std::panic::AssertUnwindSafe(q79_boxed_fixture_work(async {
                    let admitted = crate::agent::test_admission::AdmittedTurn::ordinary_controlled_channel(
                        config,
                        message,
                        Arc::clone(&registry),
                    )
                    .await;
                    assert!(
                        matches!(
                            admitted.decision().context.directive(),
                            crate::causal_tree::directive::TurnExecutionDirective::Delegate { .. }
                        ),
                        "actual parent Delegate required"
                    );
                    assert!(
                        admitted.decision().context.ordinary_fixture_prediction_verified(),
                        "actual parent validated Mock required"
                    );
                    parent_committed.store(true, Ordering::SeqCst);
                    let call = format!(
                        "<tool_call>\n{}\n</tool_call>",
                        serde_json::json!({
                            "name":"sessions_spawn", "arguments":{"input":input},
                        })
                    );
                    let provider = ScriptedProvider::from_text_responses(vec![&call, "done"]);
                    let mut history = vec![ChatMessage::system("parent fixture"), ChatMessage::user(parent_input)];
                    let hooks = crate::hooks::HookManager::new(tmp.path().to_owned());
                    let multimodal = crate::config::MultimodalConfig::default();
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(20),
                        run_turn(
                            &mut history,
                            crate::agent::turn_request::TurnRequest {
                                channel_name: "wacli",
                                scope: None,
                                mode: ChatMode::default(),
                                silent: true,
                                expose_stay_silent: false,
                                multimodal: &multimodal,
                                schedule: crate::agent::turn_request::TurnSchedule {
                                    read_only_tool_concurrency_window: 2,
                                    priority_scheduling_enabled: false,
                                    low_priority_tool_names: Vec::new(),
                                },
                                compaction: Some(admitted.compaction()),
                                decision: admitted.decision(),
                            },
                            crate::agent::turn_request::TurnServices {
                                provider: &provider,
                                provider_name: "ollama",
                                model: "prx-local-model",
                                temperature: 0.0,
                                tools_registry: registry,
                                observer: &NoopObserver,
                                hooks: &hooks,
                                approval: None,
                                approval_resolver: None,
                                runtime_adapter: Some(admitted.adapter()),
                                memory: admitted.memory_runtime(),
                            },
                            crate::agent::turn_request::TurnDelivery {
                                cancellation: None,
                                on_delta: None,
                                on_tool_call: None,
                            },
                        ),
                    )
                    .await;
                    server.0.abort();
                    let _ = (&mut server.0).await;
                    server_joined = true;
                    result.expect("bounded local fixture").expect("parent turn");
                    assert!(
                        history.iter().any(|message| {
                            message.role == "user"
                                && message.content.starts_with("[Tool results]")
                                && message.content.contains("child completed")
                        }),
                        "the child must finish through its actual dispatch and terminal path: {history:?}"
                    );
                    let captured = captured.lock().unwrap();
                    assert_eq!(captured.len(), 1, "one genuine child provider request");
                    captured[0]["tools"]
                        .as_array()
                        .expect("native child tools")
                        .iter()
                        .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
                        .collect()
                }))
                .catch_unwind()
                .await,
            );
            Ok(())
        }))
        .await;
    if !server_joined {
        server.0.abort();
        let _ = (&mut server.0).await;
    }
    let result = match caught.expect("completed original child fixture") {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    drained.expect("ordinary parent services must drain");
    result
}

#[tokio::test]
async fn a_spawned_sub_agent_inherits_the_parent_channels_tool_exclusions() {
    let inherited = published_child_tools(Default::default(), "shell").await;
    assert!(inherited.iter().any(|name| name == "shell"));
    assert!(
        !inherited
            .iter()
            .any(|name| matches!(name.as_str(), "gateway" | "nodes"))
    );
    let mut unrestricted = crate::config::ToolTieringConfig::default();
    unrestricted.channel_exclude.clear();
    for target in ["gateway", "nodes"] {
        let offered = published_child_tools(unrestricted.clone(), target).await;
        assert!(
            offered.iter().any(|name| name == target),
            "positive control must publish {target}: {offered:?}"
        );
    }
    let reopened = crate::config::ToolTieringConfig {
        always_include: vec!["gateway".to_owned()],
        ..Default::default()
    };
    let offered = published_child_tools(reopened, "gateway").await;
    assert!(offered.iter().any(|name| name == "gateway"));
    assert!(!offered.iter().any(|name| name == "nodes"));
}

#[inline(never)]
fn q79_boxed_fixture_work<'a, F: std::future::Future + 'a>(
    work: F,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = F::Output> + 'a>> {
    Box::pin(work)
}

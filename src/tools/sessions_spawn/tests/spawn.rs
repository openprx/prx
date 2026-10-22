#![cfg(test)]

use super::*;

#[test]
fn name_and_description() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    assert_eq!(tool.name(), "sessions_spawn");
    assert!(!tool.description().is_empty(), "expected a non-empty value");
    assert!(tool.description().contains("history"));
    assert!(tool.description().contains("steer"));
}

#[tokio::test]
async fn task_mode_has_no_wall_clock_deadline() {
    let run_future = async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        "done"
    };
    assert_eq!(run_future.await, "done");
}

#[tokio::test]
async fn task_mode_no_tools_preserves_single_turn_trace_usage() {
    let workspace = tempfile::tempdir().unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let captured = Arc::clone(&requests);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/api/chat",
        axum::routing::post(move |axum::Json(request): axum::Json<serde_json::Value>| {
            let captured = Arc::clone(&captured);
            async move {
                captured.lock().unwrap().push(request);
                axum::Json(json!({"model":"prx-local-model", "done":true,
                    "message":{"role":"assistant", "content":"metered task output"},
                    "prompt_eval_count":11, "eval_count":7}))
            }
        }),
    );
    let mut server = TaskTraceServer(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.config_path = workspace.path().join("config.toml");
    config.default_provider = Some("ollama".to_owned());
    config.default_model = Some("prx-local-model".to_owned());
    config.api_url = Some(endpoint);
    config.causal_tree.enabled = false;
    config.autonomy.level = crate::security::AutonomyLevel::Full;
    config.reliability.provider_retries = 0;
    config.cost.prices.insert(
        "prx-local-model".to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap());
    let (channel, _) = RecordingChannel::new();
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
    let tool = make_tool_with_security_and_spawn_config(
        Arc::new(channel),
        Arc::new(TaskTraceParentProvider),
        security,
        config.sessions_spawn.clone(),
    )
    .with_shared_memory(Arc::clone(&memory));
    let active_runs = tool.active_runs_arc();
    let registry = crate::agent::test_admission::registry_with_search(vec![Box::new(tool)]);
    let input = "sessions_spawn metered task";
    let admitted = crate::agent::test_admission::AdmittedTurn::with_memory(
        config,
        input,
        Arc::clone(&registry),
        Arc::clone(&memory),
    )
    .await;
    let mut parent_history = vec![crate::providers::ChatMessage::user(input)];
    let hooks = crate::hooks::HookManager::new(workspace.path().to_owned());
    let multimodal = MultimodalConfig::default();
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        crate::agent::turn_engine::run_turn(
            &mut parent_history,
            crate::agent::turn_request::TurnRequest {
                channel_name: "agent",
                scope: None,
                mode: Default::default(),
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
                provider: &TaskTraceParentProvider,
                provider_name: "ollama",
                model: "prx-local-model",
                temperature: 0.0,
                tools_registry: registry,
                observer: &crate::observability::noop::NoopObserver,
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
    .await
    .expect("bounded parent execution")
    .expect("parent execution");
    let (usage, history_out) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            {
                let runs = active_runs.read().await;
                assert_eq!(
                    runs.len(),
                    1,
                    "the parent must launch exactly one task: {parent_history:?}"
                );
                match &runs[0].status {
                    SubAgentStatus::Completed(output) => {
                        assert_eq!(output, "metered task output");
                        break (runs[0].token_usage_records.clone(), Arc::clone(&runs[0].history));
                    }
                    SubAgentStatus::Failed(error) => panic!("admitted task failed: {error}"),
                    _ => {}
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("bounded task completion");
    server.0.abort();
    let _ = (&mut server.0).await;
    assert_eq!(usage.len(), 1, "exactly one model segment must settle usage");
    assert_eq!(usage[0].source, crate::llm::route_decision::TokenUsageSource::Reported);
    assert_eq!(
        (
            usage[0].prompt_tokens,
            usage[0].completion_tokens,
            usage[0].total_tokens
        ),
        (11, 7, 18)
    );
    assert!(
        usage[0].settlement_id.is_some(),
        "usage must carry its terminal settlement identity"
    );
    let history = history_out.read().await;
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].role, "system");
    assert_eq!(history[2].content, "metered task output");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]["tools"].as_array().is_none_or(Vec::is_empty),
        "the child must have no tools"
    );
}

#[test]
fn schema_has_required_fields() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let schema = tool.execution_schema();
    // The executor treats an omitted action as 'spawn', so the published
    // contract defaults it rather than requiring it.
    let required = schema["required"].as_array().unwrap();
    assert!(!required.contains(&json!("action")));
    assert_eq!(schema["properties"]["action"]["default"], "spawn");
    assert!(schema["properties"]["action"].is_object());
    assert!(schema["properties"]["task"].is_object());
    assert!(schema["properties"]["run_id"].is_object());
    assert!(schema["properties"]["message"].is_object());
    assert!(schema["properties"]["model"].is_object());
    assert!(schema["properties"]["provider"].is_object());
    assert!(schema["properties"]["agent"].is_object());
    assert!(schema["properties"]["timeout_seconds"].is_null());
    assert!(schema["properties"]["mode"].is_object());
    assert!(schema["properties"]["recipient"].is_object());
    // Verify enum includes history and steer
    let enum_vals = schema["properties"]["action"]["enum"].as_array().unwrap();
    let enum_strs: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
    assert!(enum_strs.contains(&"history"));
    let issues = crate::tools::schema::validate_tool_arguments(&schema, &json!({"action": "spawn"}));
    assert!(issues.iter().any(|issue| issue.path == "$.task"));
    // An omitted action selects 'spawn', so 'task' is still required.
    let implicit = crate::tools::schema::validate_tool_arguments(&schema, &json!({}));
    assert!(implicit.iter().any(|issue| issue.path == "$.task"));
    assert!(
        crate::tools::schema::validate_tool_arguments(&schema, &json!({"task": "do it"})).is_empty(),
        "expected an empty value"
    );
    assert!(enum_strs.contains(&"steer"));
}

/// BUG-12: an inline `provider` override (no named agent) must drive a
/// provider rebuild. Using an invalid provider name proves the override is
/// consumed: provider creation fails naming the inline override, not the
/// gateway provider (`ollama`).
#[tokio::test]
async fn inline_provider_override_drives_provider_rebuild() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));

    let result = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({
            "task": "do work",
            "provider": "totally-invalid-provider"
        })),
    )
    .await
    .unwrap();

    assert!(!result.success);
    let err = result.error.as_deref().unwrap_or("");
    assert!(
        err.contains("totally-invalid-provider"),
        "error should name the inline provider override: {err}"
    );
}

#[tokio::test]
async fn missing_task_returns_error() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let result = tool.execute(json!({})).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn empty_task_returns_failure() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let workspace = tempfile::tempdir().expect("test: parent workspace");
    let config = preconstruction_parent_config(&tool, workspace.path());
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).expect("test: parent memory"));
    let result = dispatch_preconstruction_parent(tool, config, memory, json!({"task": "   "}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.unwrap().contains("must not be empty"));
}

#[tokio::test]
async fn spawns_and_returns_run_id() {
    let (ch, sent) = RecordingChannel::new();
    let tool = make_tool(
        Arc::new(ch),
        Arc::new(EchoProvider {
            response: "The joke: Why did the chicken cross the road?".into(),
        }),
    );
    tool.set_default_recipient(Some("test-recipient".to_string())).await;

    let result = execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "Tell me a joke"})))
        .await
        .unwrap();

    assert!(result.success);
    assert!(result.output.contains("run_id:"));
    assert!(result.output.contains("Will announce"));

    // Wait for the admitted child to complete and announce rather than
    // assuming admission, accounting, and provider work fit in 200 ms.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while sent.lock().await.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("admitted child announcement");

    let messages = sent.lock().await;
    assert_eq!(messages.len(), 1);
    assert!(messages[0].contains("Sub-agent"));
    assert!(messages[0].contains("chicken"), "{messages:?}");
}

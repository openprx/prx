#![cfg(test)]

use super::*;

#[tokio::test]
async fn no_recipient_skips_announcement() {
    let (ch, sent) = RecordingChannel::new();
    let tool = make_tool(
        Arc::new(ch),
        Arc::new(EchoProvider {
            response: "done".into(),
        }),
    );
    // No default_recipient set, no recipient in args

    let result = execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "Do something"})))
        .await
        .unwrap();
    assert!(result.success);

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let messages = sent.lock().await;
    assert!(messages.is_empty(), "Should not announce without recipient");
}

#[tokio::test]
async fn explicit_recipient_overrides_default() {
    let (ch, sent) = RecordingChannel::new();
    let tool = make_tool(
        Arc::new(ch),
        Arc::new(EchoProvider {
            response: "result".into(),
        }),
    );
    tool.set_default_recipient(Some("default-recipient".to_string())).await;

    let result = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({
            "task": "Test task",
            "recipient": "explicit-recipient"
        })),
    )
    .await
    .unwrap();
    assert!(result.success);

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let messages = sent.lock().await;
    // Should have sent to explicit-recipient (check channel.sent has a message)
    assert_eq!(messages.len(), 1);
}

/// The runtime places no ceiling on how many sub-agents run at once, how
/// deeply they nest, or how many one session owns. This fans out past every
/// ceiling that used to exist — `max_concurrent` = 64 and, because all 100
/// runs share one session scope key, `max_children_per_agent` = 32 — and
/// requires that every single spawn is accepted and stays live.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_fan_out_is_uncapped_by_concurrency_or_children() {
    /// Comfortably past both removed ceilings (64 global, 32 per session).
    const FAN_OUT: usize = 100;

    let (ch, _) = RecordingChannel::new();
    // Every run parks inside the provider, so all FAN_OUT of them are
    // simultaneously Running when the assertions below look at the registry.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    // No workaround of any kind: the default policy is what a real spawn
    // runs under, and nothing in it rations how many spawns may happen.
    let tool = make_tool(
        Arc::new(ch),
        Arc::new(GatedProvider {
            gate: Arc::clone(&gate),
        }),
    );
    tool.set_default_recipient(Some("test-recipient".to_string())).await;

    for index in 0..FAN_OUT {
        let result = execute_admitted_spawn(
            &tool,
            with_spawn_grant(json!({
                "task": format!("fan-out child {index}"),
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "sender": "openprx_user",
                    "channel": "signal",
                    "chat_type": "direct",
                    "chat_id": "+15551234567"
                }
            })),
        )
        .await
        .unwrap();
        assert!(
            result.success,
            "spawn {index} of {FAN_OUT} must be accepted, got error: {:?}",
            result.error
        );
    }

    let runs = tool.active_runs_snapshot().await;
    assert_eq!(
        running_run_count(&runs),
        FAN_OUT,
        "all {FAN_OUT} runs must be live at once; none may be capped away"
    );
    let scope_keys: std::collections::HashSet<&str> = runs.iter().map(|run| run.session_scope_key.as_str()).collect();
    assert_eq!(
        scope_keys.len(),
        1,
        "the fan-out must share one session scope, which is what the removed \
         per-session ceiling counted"
    );

    // Let the parked runs finish so nothing is left blocked on the gate.
    gate.add_permits(FAN_OUT * 4);
}

/// Nesting has no ceiling either: a chain of spawn-run contexts, each one
/// deeper than the last, is accepted well past the depth the removed
/// `max_spawn_depth` = 8 allowed. The depth is still reported, because a
/// fan-out is only acceptable while an operator can see its shape.
#[tokio::test]
async fn spawn_nesting_is_uncapped_and_still_reports_depth() {
    /// Comfortably past the removed depth ceiling of 8.
    const DEEPEST: usize = 40;

    let (ch, _) = RecordingChannel::new();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    // No workaround of any kind: the default policy is what a real spawn
    // runs under, and nothing in it rations how many spawns may happen.
    let tool = make_tool(
        Arc::new(ch),
        Arc::new(GatedProvider {
            gate: Arc::clone(&gate),
        }),
    );
    tool.set_default_recipient(Some("test-recipient".to_string())).await;

    for parent_depth in 0..DEEPEST {
        let result = SPAWN_EXECUTION_CONTEXT
            .scope(
                SpawnExecutionContext {
                    operator_authority: None,
                    ordinary_authority: None,
                    incoming_grant: None,
                    provider_binding: None,
                    terminal_usage: None,
                    input_receipt: None,
                    run_id: format!("spawn-run-{parent_depth}"),
                    session_scope_key: format!("signal:direct:+1555000{parent_depth}"),
                    spawn_depth: parent_depth,
                    owner_id: None,
                    owner_isolated: false,
                    topic_id: None,
                    source_message_event_id: None,
                    is_turn_root: false,
                    channel_excluded_tools: Vec::new(),
                },
                async { execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "nested"}))).await },
            )
            .await
            .unwrap();
        assert!(
            result.success,
            "a child at depth {} must be accepted, got error: {:?}",
            parent_depth + 1,
            result.error
        );
    }

    let runs = tool.active_runs_snapshot().await;
    let deepest = runs
        .iter()
        .map(|run| run.spawn_depth)
        .max()
        .expect("test: the nested runs must be registered");
    assert_eq!(
        deepest, DEEPEST,
        "the deepest child must still report its nesting depth"
    );

    gate.add_permits(DEEPEST * 4);
}

/// D8-4: a turn-root context (is_turn_root = true, spawn_depth = 0)
/// represents the turn itself, so its first child reports depth 0 — exactly
/// as if no context had been seeded. Seeding a turn must not inflate the
/// nesting its children report (the complement is
/// spawn_run_parent_reports_child_at_depth_one).
#[tokio::test]
async fn turn_root_seed_reports_first_child_at_depth_zero() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    tool.set_default_recipient(Some("test-recipient".to_string())).await;

    let result = SPAWN_EXECUTION_CONTEXT
        .scope(
            SpawnExecutionContext::seed_turn_context(
                "turn-root-run".to_string(),
                "signal:+15551234567:openprx_user".to_string(),
            ),
            async { execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "child of a turn"}))).await },
        )
        .await
        .unwrap();

    assert!(result.success, "a turn-root seed must not block its first child");

    let runs = tool.active_runs_snapshot().await;
    let child = runs.first().expect("the spawned child run must be registered");
    assert_eq!(child.spawn_depth, 0, "turn-root first child must report spawn_depth 0");
    assert_eq!(
        child.parent_run_id.as_deref(),
        Some("turn-root-run"),
        "child must inherit parent_run_id = the per-turn run_id"
    );
}

/// D8-4: a real spawn-run parent at depth 0 (is_turn_root = false) is the
/// case the turn root must not mimic — its child reports depth 1, one hop
/// deeper. Same spawn_depth = 0 seed, opposite is_turn_root, opposite
/// reported depth.
#[tokio::test]
async fn spawn_run_parent_reports_child_at_depth_one() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    tool.set_default_recipient(Some("test-recipient".to_string())).await;

    let result = SPAWN_EXECUTION_CONTEXT
        .scope(
            SpawnExecutionContext {
                operator_authority: None,
                ordinary_authority: None,
                incoming_grant: None,
                provider_binding: None,
                terminal_usage: None,
                input_receipt: None,
                run_id: "spawn-run".to_string(),
                session_scope_key: "signal:+15551234567:openprx_user".to_string(),
                spawn_depth: 0,
                owner_id: None,
                owner_isolated: false,
                topic_id: None,
                source_message_event_id: None,
                is_turn_root: false,
                channel_excluded_tools: Vec::new(),
            },
            async { execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "nested"}))).await },
        )
        .await
        .unwrap();

    assert!(result.success, "a nested spawn must not be blocked by its depth");

    let runs = tool.active_runs_snapshot().await;
    let child = runs.first().expect("the spawned child run must be registered");
    assert_eq!(child.spawn_depth, 1, "a spawn-run parent's child is one hop deeper");
}

#[tokio::test]
async fn failed_provider_announces_error() {
    let (ch, sent) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(FailingProvider));
    tool.set_default_recipient(Some("user".to_string())).await;

    let result = execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "This will fail"})))
        .await
        .unwrap();
    assert!(result.success); // spawn succeeds; failure is in the sub-agent

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let messages = sent.lock().await;
    assert_eq!(messages.len(), 1);
    assert!(
        messages[0].contains("error") || messages[0].contains("Error"),
        "Error message should be announced: {}",
        messages[0]
    );
}

#[test]
fn announce_message_compacts_success_body() {
    let status = SubAgentStatus::Completed("first line\nsecond line".to_string());
    let message = format_announce_message("run-abc", &status, "first line\nsecond line");

    assert_eq!(message, "🤖 Sub-agent `run-abc` completed: first line");
    assert!(!message.contains("second line"));
}

#[test]
fn announce_message_compacts_failed_body() {
    let status = SubAgentStatus::Failed("provider failure\nstack trace".to_string());
    let message = format_announce_message("run-abc", &status, "provider failure\nstack trace");

    assert_eq!(message, "🤖 Sub-agent `run-abc` FAILED with error: provider failure");
    assert!(!message.contains("stack trace"));
}

#[tokio::test]
async fn active_runs_tracked() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(
        Arc::new(ch),
        Arc::new(EchoProvider {
            response: "done".into(),
        }),
    );

    // Spawn a run
    let _ = execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "Some task"})))
        .await
        .unwrap();

    let runs = tool.active_runs_snapshot().await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].task, "Some task");
}

/// Bug-V5-1 regression: a `sessions_spawn` call made *inside* a turn-root
/// `SPAWN_EXECUTION_CONTEXT` scope (i.e. the model invoking the tool mid-turn)
/// must capture the per-turn run id as the child's `parent_run_id`. The
/// capture is synchronous — read at the top of `execute`, **before** any
/// `tokio::spawn` — so the task-local is always present when read and never
/// lost across the spawn boundary. The chat `/sessions` projection reads this
/// `Some(parent)` as model-origin (see `SessionOrigin::from_parent_run_id`,
/// asserted in the chat-side `model.rs` tests, which can reach that module).
#[tokio::test]
async fn model_spawn_within_turn_scope_captures_parent_run_id() -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let workspace = tempfile::tempdir()?;
    use futures_util::FutureExt as _;
    let phase = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let parent_calls = Arc::new(AtomicUsize::new(0));
    let child_calls = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let serving_phase = Arc::clone(&phase);
    let serving_parent_calls = Arc::clone(&parent_calls);
    let serving_child_calls = Arc::clone(&child_calls);
    let app = axum::Router::new().route("/api/chat", axum::routing::post(
        move |axum::Json(request): axum::Json<serde_json::Value>| {
            let phase = Arc::clone(&serving_phase);
            let parent_calls = Arc::clone(&serving_parent_calls);
            let child_calls = Arc::clone(&serving_child_calls);
            async move {
                if request.get("model").and_then(serde_json::Value::as_str) != Some("prx-parent-spawn-model") {
                    return Err(axum::http::StatusCode::BAD_REQUEST);
                }
                let message = match phase.load(Ordering::SeqCst) {
                    0 => {
                        let has_spawn = request.get("tools").and_then(serde_json::Value::as_array)
                            .is_some_and(|tools| tools.iter().any(|tool| tool.get("function").and_then(|f| f.get("name")).and_then(serde_json::Value::as_str) == Some("sessions_spawn")));
                        let original_input = request.get("messages").and_then(serde_json::Value::as_array)
                            .and_then(|messages| messages.last()).and_then(|m| m.get("content")).and_then(serde_json::Value::as_str) == Some("spawn model child");
                        if !has_spawn || !original_input || phase.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst).is_err() {
                            return Err(axum::http::StatusCode::BAD_REQUEST);
                        }
                        parent_calls.fetch_add(1, Ordering::SeqCst);
                        json!({"role":"assistant", "content":"", "tool_calls":[{"function":{"name":"sessions_spawn", "arguments":{"task":"model child"}}}]})
                    }
                    2 => {
                        child_calls.fetch_add(1, Ordering::SeqCst);
                        json!({"role":"assistant", "content":"ok"})
                    }
                    _ => return Err(axum::http::StatusCode::CONFLICT),
                };
                Ok(axum::Json(json!({"model":"prx-parent-spawn-model", "done":true, "message":message})))
            }
        }
    ));
    let mut server = TaskTraceServer(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let work = async {
        let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())?;
        cleanup
            .run(Box::pin(async {
                let mut config = crate::config::Config::default();
                config.workspace_dir = workspace.path().to_path_buf();
                config.config_path = workspace.path().join("config.toml");
                config.default_provider = Some("ollama".to_owned());
                config.default_model = Some("prx-parent-spawn-model".to_owned());
                config.api_url = Some(endpoint);
                config.reliability.provider_retries = 0;
                config.cost.prices.insert(
                    "ollama/prx-parent-spawn-model".to_owned(),
                    crate::config::schema::ModelPricing {
                        input: 1.0,
                        output: 1.0,
                        cache_write: 1.0,
                        cache_read: 1.0,
                    },
                );
                config.providers.models.push(serde_json::from_value(json!({
                "provider": "ollama", "model": "prx-parent-spawn-model", "operations": ["chat"], "native_tools": true
            }))?);
                config.save().await?;
                let generation = crate::config::new_shared(config).pin();
                let config = &generation.effective;
                let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path())?);
                let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
                let model_catalog = crate::providers::selection::catalog_for(config);
                let resolved = crate::providers::selection::check(
                    config,
                    "ollama",
                    "prx-parent-spawn-model",
                    &crate::providers::catalog::Requirement::chat()
                        .with(crate::providers::catalog::Feature::NativeTools),
                    crate::providers::catalog::UnknownPolicy::Invocation,
                )?;
                let provider: Arc<dyn Provider> = Arc::from(crate::providers::create_provider_for(
                    &resolved,
                    &model_catalog,
                    &config.reliability,
                    &config.cost,
                    &crate::providers::provider_runtime_options_from_config(config),
                )?);
                let (channel, _) = NamedRecordingChannel::new("terminal");
                let tool = SessionsSpawnTool::new(
                    channel,
                    Arc::clone(&provider),
                    "ollama",
                    "prx-parent-spawn-model",
                    0.7,
                    Arc::clone(&security),
                    workspace.path().to_path_buf(),
                    config.multimodal.clone(),
                    config.agent.compaction.clone(),
                    config.agents_with_references().into_owned(),
                    None,
                    crate::providers::provider_runtime_options_from_config(config),
                    config.sessions_spawn.clone(),
                )
                .with_configured_providers(&crate::providers::ConfiguredProviders::from_config(config))
                .with_model_catalog(crate::providers::selection::catalog_for(config))
                .with_hint_config(config)
                .with_shared_memory(Arc::clone(&memory))
                .with_event_recording(config.memory.event_recording_config());
                tool.set_default_recipient(Some("test-recipient".to_string())).await;
                let runs = tool.active_runs_arc();
                let tools_handle = tool.tools_handle();
                let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(tool)]);
                assert!(tools_handle.set(&registry));
                let result = SPAWN_EXECUTION_CONTEXT
                    .scope(
                        SpawnExecutionContext::seed_turn_context(
                            "turn-run-xyz".to_string(),
                            "chat:session-1".to_string(),
                        ),
                        crate::chat::decision_source::admission_failure_tests::dispatch_recorded_model_spawn(
                            crate::runtime::turn_publication::TurnDecisionPublication {
                                generation: Arc::clone(&generation),
                                registry: Arc::clone(&registry),
                                mcp_tool: None,
                                security,
                                observer: Arc::new(NoopObserver),
                                hooks: Arc::new(HookManager::new(workspace.path().to_path_buf())),
                                memory,
                                plugin_generation: 0,
                            },
                            provider,
                            with_spawn_grant(json!({"task": "model child"})),
                            Arc::clone(&phase),
                        ),
                    )
                    .await?;
                assert!(result.success, "spawn inside a turn scope must succeed");
                assert_eq!(parent_calls.load(Ordering::SeqCst), 1);
                tokio::time::timeout(std::time::Duration::from_secs(20), async {
                    loop {
                        if runs.read().await.first().is_some_and(|run| {
                            matches!(&run.status, SubAgentStatus::Completed(_) | SubAgentStatus::Failed(_))
                        }) {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await?;
                assert!(
                    child_calls.load(Ordering::SeqCst) > 0,
                    "actual child provider request required"
                );
                let runs = runs.read().await;
                let child = runs.first().expect("the spawned child run must be registered");
                assert_eq!(
                    child.parent_run_id.as_deref(),
                    Some("turn-run-xyz"),
                    "child captured the per-turn run id as parent before the spawn boundary (=> model origin)"
                );
                Ok::<_, anyhow::Error>(())
            }))
            .await
    };
    let result = std::panic::AssertUnwindSafe(work).catch_unwind().await;
    server.0.abort();
    let _ = (&mut server.0).await;
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Local operator spawns inherit their authenticated source run even without
/// an ambient model-turn spawn context.
#[tokio::test]
async fn operator_spawn_without_ambient_context_uses_source_run_as_parent() -> anyhow::Result<()> {
    use crate::runtime::turn_source::TurnSourceProof as _;

    let workspace = tempfile::tempdir()?;
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())?;
    cleanup
        .run(Box::pin(async {
            let mut config = crate::config::Config::default();
            config.workspace_dir = workspace.path().to_path_buf();
            config.config_path = workspace.path().join("config.toml");
            config.default_provider = Some("ollama".to_owned());
            config.default_model = Some("prx-local-model".to_owned());
            config.providers.models.push(serde_json::from_value(json!({
                "provider": "ollama", "model": "prx-local-model", "operations": ["chat"]
            }))?);
            config.save().await?;
            let generation = crate::config::new_shared(config).pin();
            let config = &generation.effective;
            let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path())?);
            let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
            let provider: Arc<dyn Provider> = Arc::new(EchoProvider { response: "ok".into() });
            let (channel, _) = NamedRecordingChannel::new("terminal");
            let tool = SessionsSpawnTool::new(
                channel,
                Arc::clone(&provider),
                "ollama",
                "prx-local-model",
                0.7,
                Arc::clone(&security),
                workspace.path().to_path_buf(),
                config.multimodal.clone(),
                config.agent.compaction.clone(),
                config.agents_with_references().into_owned(),
                None,
                crate::providers::provider_runtime_options_from_config(config),
                config.sessions_spawn.clone(),
            )
            .with_configured_providers(&crate::providers::ConfiguredProviders::from_config(config))
            .with_model_catalog(crate::providers::selection::catalog_for(config))
            .with_hint_config(config)
            .with_shared_memory(Arc::clone(&memory))
            .with_event_recording(config.memory.event_recording_config());
            tool.set_default_recipient(Some("test-recipient".to_string())).await;
            let runs = tool.active_runs_arc();
            let tools_handle = tool.tools_handle();
            let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(tool)]);
            assert!(tools_handle.set(&registry));
            assert!(current_spawn_execution_context().is_none());
            let (args, source) = crate::chat::decision_source::admission_failure_tests::recorded_local_operator_spawn(
                crate::runtime::turn_publication::TurnDecisionPublication {
                    generation: Arc::clone(&generation),
                    registry: Arc::clone(&registry),
                    mcp_tool: None,
                    security,
                    observer: Arc::new(NoopObserver),
                    hooks: Arc::new(HookManager::new(workspace.path().to_path_buf())),
                    memory,
                    plugin_generation: 0,
                },
                provider,
                "operator child",
            )
            .await?;
            let expected_parent = source.proof().run_id().to_owned();
            source
                .scope(async {
                    assert!(current_spawn_execution_context().is_none());
                    let result = registry
                        .first()
                        .expect("spawn tool is published")
                        .execute(with_spawn_grant(args))
                        .await?;
                    assert!(result.success);
                    assert!(current_spawn_execution_context().is_none());
                    let runs = runs.read().await;
                    let child = runs.first().expect("the spawned child run must be registered");
                    assert_eq!(child.parent_run_id.as_deref(), Some(expected_parent.as_str()));
                    Ok::<_, anyhow::Error>(())
                })
                .await
        }))
        .await
}

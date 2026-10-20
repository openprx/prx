#![cfg(test)]

use super::*;

#[tokio::test]
async fn spawn_rejects_unknown_agent() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())?;
    cleanup
        .run(Box::pin(async {
            let (ch, _) = RecordingChannel::new();
            let mut agents = HashMap::new();
            agents.insert("alpha".to_string(), make_agent_config(None));
            let provider: Arc<dyn Provider> = Arc::new(EchoProvider { response: "ok".into() });
            let mut config = crate::config::Config::default();
            config.workspace_dir = workspace.path().to_path_buf();
            config.config_path = workspace.path().join("config.toml");
            config.default_provider = Some("ollama".to_owned());
            config.default_model = Some("prx-local-model".to_owned());
            config.agents = agents.clone();
            config.providers.models.push(serde_json::from_value(json!({
                "provider": "ollama", "model": "prx-local-model", "operations": ["chat"],
                "native_tools": true
            }))?);
            config.cost.prices.insert(
                "ollama/prx-local-model".to_owned(),
                crate::config::schema::ModelPricing {
                    input: 1.0,
                    output: 1.0,
                    cache_write: 1.0,
                    cache_read: 1.0,
                },
            );
            config.save().await?;
            let generation = crate::config::new_shared(config).pin();
            let security = test_security();
            let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path())?);
            let tool = SessionsSpawnTool::new(
                Arc::new(ch),
                "ollama",
                "prx-local-model",
                0.7,
                Arc::clone(&security),
                std::path::PathBuf::from("/tmp"),
                crate::config::MultimodalConfig::default(),
                agents,
                None,
                crate::providers::ProviderRuntimeOptions::default(),
                crate::config::SessionsSpawnConfig::default(),
            )
            .with_test_parent_provider(Arc::clone(&provider));
            let tools_handle = tool.tools_handle();
            let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(tool)]);
            assert!(tools_handle.set(&registry));
            assert_eq!(generation.effective.agents.len(), 1);
            assert!(generation.effective.agents.contains_key("alpha"));
            assert!(!generation.effective.agents.contains_key("missing"));
            let result = crate::chat::decision_source::admission_failure_tests::dispatch_unknown_agent_fixture(
                crate::runtime::turn_publication::TurnDecisionPublication {
                    generation,
                    registry,
                    mcp_tool: None,
                    security,
                    observer: Arc::new(NoopObserver),
                    hooks: Arc::new(HookManager::new(workspace.path().to_path_buf())),
                    memory,
                    plugin_generation: 0,
                },
                provider,
                with_spawn_grant(json!({"task": "hello", "agent": "missing"})),
            )
            .await?;
            assert!(!result.success);
            assert!(result.error.unwrap_or_default().contains("Unknown agent"));
            Ok(())
        }))
        .await
}

#[tokio::test]
async fn spawn_rejects_spawn_disabled_agent() {
    let (ch, _) = RecordingChannel::new();
    let mut agents = HashMap::new();
    let mut cfg = make_agent_config(None);
    cfg.spawn_enabled = Some(false);
    agents.insert("alpha".to_string(), cfg);
    let tool = SessionsSpawnTool::new(
        Arc::new(ch),
        "ollama",
        "prx-local-model",
        0.7,
        test_security(),
        std::path::PathBuf::from("/tmp"),
        crate::config::MultimodalConfig::default(),
        agents,
        None,
        crate::providers::ProviderRuntimeOptions::default(),
        crate::config::SessionsSpawnConfig::default(),
    )
    .with_test_parent_provider(Arc::new(EchoProvider { response: "ok".into() }));
    let workspace = tempfile::tempdir().expect("test: parent workspace");
    let config = preconstruction_parent_config(&tool, workspace.path());
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).expect("test: parent memory"));
    let result = dispatch_preconstruction_parent(
        tool,
        config,
        memory,
        with_spawn_grant(json!({"task": "hello", "agent": "alpha"})),
    )
    .await
    .unwrap();
    assert!(!result.success);
    assert!(result.error.unwrap_or_default().contains("spawn_enabled=false"));
}

#[tokio::test]
async fn spawn_agent_uses_identity_prompt() {
    let ws = tempfile::TempDir::new().unwrap();
    let identity_dir = ws.path().join("identities/alpha");
    std::fs::create_dir_all(&identity_dir).unwrap();
    std::fs::write(identity_dir.join("SOUL.md"), "Identity Soul").unwrap();

    let mut agents = HashMap::new();
    agents.insert(
        "alpha".to_string(),
        make_agent_config(Some("identities/alpha".to_string())),
    );

    let (ch, sent) = RecordingChannel::new();
    let tool = SessionsSpawnTool::new(
        Arc::new(ch),
        "ollama",
        "prx-local-model",
        0.7,
        test_security(),
        ws.path().to_path_buf(),
        crate::config::MultimodalConfig::default(),
        agents,
        None,
        crate::providers::ProviderRuntimeOptions::default(),
        crate::config::SessionsSpawnConfig::default(),
    )
    .with_test_parent_provider(Arc::new(EchoSystemProvider));
    tool.set_default_recipient(Some("test-recipient".to_string())).await;

    let result = execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "t", "agent": "alpha"})))
        .await
        .unwrap();
    assert!(result.success);

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let messages = sent.lock().await;
    assert_eq!(messages.len(), 1);
    assert!(messages[0].contains("completed:"), "{}", messages[0]);
    assert!(!messages[0].contains("Identity Soul"), "{}", messages[0]);
    drop(messages);

    let runs = tool.active_runs_snapshot().await;
    let run = runs.first().expect("spawned run should be retained");
    let SubAgentStatus::Completed(full_output) = &run.status else {
        panic!("spawned run should be completed");
    };
    assert!(full_output.contains("### SOUL.md"));
    assert!(full_output.contains("Identity Soul"));
}

#[test]
fn process_mode_task_arg_is_not_json_encoded() {
    let manifest = WorkerManifest {
        parent_capability: Some("capability".to_string()),
        capability_manifest: None,
        child_source: None,
        ordinary_source: None,
        operator_source: None,
        run_id: "run".to_string(),
        task: "say \"hello\"".to_string(),
        provider_name: "provider".to_string(),
        model: "model".to_string(),
        api_key: None,
        route_hint: None,
        route_source_provider: None,
        route_credential: None,
        route_uses_root_connection: false,
        direct_uses_root_connection: false,
        temperature: 0.7,
        temperature_override: None,
        config_dir: std::path::PathBuf::from("/tmp/openprx"),
        config_generation: "0".repeat(64),
        runtime_config_generation_id: None,
        runtime_config_source_revision: None,
        workspace_dir: std::path::PathBuf::from("/tmp/ws"),
        memory_db_path: std::path::PathBuf::from("/tmp/ws/brain.db"),
        memory_workspace_id: Some("/tmp/ws".to_string()),
        memory_strategy: Some("shared_fabric".to_string()),
        memory_backend: "sqlite".to_string(),
        shared_memory_db_path: Some(std::path::PathBuf::from("/tmp/ws/memory/brain.db")),
        worker_memory_db_path: Some(std::path::PathBuf::from("/tmp/worker/brain.db")),
        agent_id: Some("agent-a".to_string()),
        persona_id: None,
        memory_event_recording: MemoryEventRecording::default(),
        allowed_tools: vec!["shell".to_string()],
        channel_excluded_tools: Vec::new(),
        system_prompt: None,
        identity_dir: None,
        scope_sender: None,
        scope_channel: None,
        scope_chat_type: None,
        scope_chat_id: None,
        owner_id: None,
        owner_isolated: false,
        topic_id: None,
        parent_task_id: None,
        source_message_event_id: None,
        spawn_depth: 0,
        session_scope_key: "sessions_spawn:global".to_string(),
        parent_run_id: None,
        compaction_config: None,
    };

    let args = build_session_worker_cli_args(&manifest).unwrap();
    assert!(
        args.iter().any(|arg| arg == "--config-dir"),
        "process worker must receive the resolved parent config directory"
    );
    let task_index = args.iter().position(|arg| arg == "--task").unwrap();
    assert_eq!(args[task_index + 1], manifest.task);
}

#[test]
fn process_mode_manifest_uses_parent_workspace_memory_db() {
    let workspace = std::path::Path::new("/tmp/openprx-workspace");
    assert_eq!(
        shared_worker_memory_db_path(workspace),
        workspace.join("memory").join("brain.db")
    );
    assert_eq!(
        private_worker_memory_db_path(std::path::Path::new("/tmp/openprx-worker")),
        std::path::Path::new("/tmp/openprx-worker").join("brain.db")
    );
}

#[test]
fn process_memory_strategy_is_explicitly_validated() {
    assert_eq!(normalize_process_memory_strategy("").unwrap(), "shared_fabric");
    assert_eq!(
        normalize_process_memory_strategy("shared_fabric").unwrap(),
        "shared_fabric"
    );
    assert_eq!(
        normalize_process_memory_strategy("isolated_private").unwrap(),
        "isolated_private"
    );
    let hybrid = normalize_process_memory_strategy("hybrid").unwrap_err().to_string();
    assert!(hybrid.contains("production merge consumer"));
    assert!(hybrid.contains("merge/reject/ack/cleanup protocol"));
    assert!(normalize_process_memory_strategy("worker-only").is_err());
}

#[tokio::test]
async fn hybrid_process_spawn_is_rejected_before_events_or_registry_side_effects() {
    let tmp = tempfile::TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let mut spawn_config = crate::config::SessionsSpawnConfig::default();
    spawn_config.process_memory_strategy = "hybrid".to_string();
    let (channel, _) = RecordingChannel::new();
    let tool = make_tool_with_spawn_config(
        Arc::new(channel),
        Arc::new(EchoProvider { response: "ok".into() }),
        spawn_config,
    )
    .with_shared_memory(memory.clone());

    let mut config = preconstruction_parent_config(&tool, tmp.path());
    config.sessions_spawn.process_memory_strategy = "hybrid".to_string();
    let runs = tool.active_runs_arc();
    let error = dispatch_preconstruction_parent(
        tool,
        config,
        Arc::clone(&memory),
        json!({"task": "must not start", "mode": "process"}),
    )
    .await
    .unwrap_err()
    .root_cause()
    .to_string();

    assert_eq!(error, crate::config::HYBRID_PROCESS_MEMORY_UNAVAILABLE);
    assert!(runs.read().await.is_empty());
    let events = memory
        .list_memory_events_since(
            &MemoryPrincipal {
                require_owner_match: false,
                workspace_id: "/tmp".to_string(),
                agent_id: None,
                persona_id: None,
                session_key: Some("sessions_spawn:global".to_string()),
                channel: None,
                sender: None,
                owner_id: None,
                legacy_session_key: None,
            },
            0,
            10,
        )
        .await
        .unwrap();
    assert!(events.is_empty(), "hybrid rejection must not record spawn events");
}

#[test]
fn spawn_event_scope_is_derived_from_runtime_envelope() {
    let scope = SpawnScope {
        sender: "alice".to_string(),
        channel: "telegram".to_string(),
        chat_type: "direct".to_string(),
        chat_id: "chat-1".to_string(),
        owner_id: None,
        owner_isolated: false,
        topic_id: Some("topic-a".to_string()),
        parent_task_id: Some("parent-task".to_string()),
        source_message_event_id: Some("msg-a".to_string()),
        config_generation_id: Some(17),
        config_source_revision: Some("revision-17".to_string()),
    };
    let event_scope = spawn_event_scope(
        "/tmp/ws",
        "run-child",
        "telegram:chat-1:alice",
        Some("run-parent"),
        Some("agent-a"),
        Some(&scope),
    );

    assert_eq!(event_scope.source, "sessions_spawn");
    assert_eq!(event_scope.config_generation_id, Some(17));
    assert_eq!(event_scope.config_source_revision.as_deref(), Some("revision-17"));
    assert_eq!(event_scope.channel.as_deref(), Some("telegram"));
    assert_eq!(event_scope.session_key.as_deref(), Some("telegram:chat-1:alice"));
    assert_eq!(event_scope.run_id.as_deref(), Some("run-child"));
    assert_eq!(event_scope.parent_run_id.as_deref(), Some("run-parent"));
    assert_eq!(event_scope.agent_id.as_deref(), Some("agent-a"));
    assert_eq!(event_scope.sender.as_deref(), Some("alice"));
    assert_eq!(event_scope.recipient.as_deref(), Some("chat-1"));
    assert_eq!(event_scope.owner_id.as_deref(), Some("owner:/tmp/ws:telegram:alice"));
    let lineage = spawn_lineage(&event_scope, None, Some(&scope));
    assert_eq!(lineage.owner_id.as_deref(), Some("owner:/tmp/ws:telegram:alice"));
    assert_eq!(lineage.topic_id.as_deref(), Some("topic-a"));
    assert_eq!(lineage.parent_task_id.as_deref(), Some("parent-task"));
    assert_eq!(lineage.source_message_event_id.as_deref(), Some("msg-a"));
}

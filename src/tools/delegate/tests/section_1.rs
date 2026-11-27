    use super::*;
    use crate::memory::{MemoryPrincipal, SqliteMemory};
    use crate::providers::{ChatMessage, ChatRequest, ChatResponse, ToolCall};
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Workspace for the delegate tests, unique per process run.
    ///
    /// The tool-execution ledger lives inside the workspace and is durable on
    /// purpose, so the default `.` workspace would both write into the source
    /// tree and let one run's records replay into the next.
    fn test_workspace() -> std::path::PathBuf {
        static WORKSPACE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        WORKSPACE
            .get_or_init(|| {
                std::env::temp_dir().join(format!(
                    "prx-delegate-tests-{}-{}",
                    std::process::id(),
                    uuid::Uuid::now_v7()
                ))
            })
            .clone()
    }

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            workspace_dir: test_workspace(),
            ..SecurityPolicy::default()
        })
    }

    fn sample_agents() -> HashMap<String, DelegateAgentConfig> {
        let mut agents = HashMap::new();
        agents.insert(
            "researcher".to_string(),
            DelegateAgentConfig {
                messaging: crate::config::schema::AgentMessaging::default(),
                approval_wait_secs: 600,
                limits: crate::config::AgentLimits::default(),
                provider: "ollama".to_string(),
                model: "llama3".to_string(),
                system_prompt: Some("You are a research assistant.".to_string()),
                api_key: None,
                credential_ref: None,
                temperature: Some(0.3),
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
                enabled: true,
                autonomy: None,
            },
        );
        agents.insert(
            "coder".to_string(),
            DelegateAgentConfig {
                messaging: crate::config::schema::AgentMessaging::default(),
                approval_wait_secs: 600,
                limits: crate::config::AgentLimits::default(),
                provider: "openrouter".to_string(),
                model: "anthropic/claude-sonnet-4-20250514".to_string(),
                system_prompt: None,
                api_key: Some("delegate-test-credential".to_string()),
                credential_ref: None,
                temperature: None,
                max_depth: 2,
                agentic: false,
                allowed_tools: Vec::new(),
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
                enabled: true,
                autonomy: None,
            },
        );
        agents
    }

    /// Counts its own executions, so a test can tell "the delegated sub-agent ran
    /// the tool" from "the tool was refused and the refusal was handed back as
    /// tool output".
    #[derive(Default)]
    struct EchoTool {
        executions: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }

        fn description(&self) -> &str {
            "Echoes the `value` argument."
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "value": {"type": "string"}
                },
                "required": ["value"]
            })
        }

        async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            let value = args
                .get("value")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(ToolResult {
                success: true,
                output: format!("echo:{value}"),
                error: None,
            })
        }
    }

    struct DynamicAliasTool;

    #[async_trait]
    impl Tool for DynamicAliasTool {
        fn name(&self) -> &str {
            "dynamic_root"
        }

        fn description(&self) -> &str {
            "Dynamic alias fixture"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }

        fn specs(&self) -> Vec<super::super::traits::ToolSpec> {
            vec![super::super::traits::ToolSpec {
                name: "dynamic_alias".to_string(),
                description: self.description().to_string(),
                parameters: self.parameters_schema(),
            }]
        }

        fn supports_name(&self, name: &str) -> bool {
            matches!(name, "dynamic_root" | "dynamic_alias")
        }

        async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
            self.execute_named("dynamic_root", args).await
        }

        async fn execute_named(&self, name: &str, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: self.supports_name(name),
                output: name.to_string(),
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn delegated_tool_reference_preserves_dynamic_alias_contract() {
        let delegated = ToolArcRef::new(Arc::new(DynamicAliasTool));
        assert!(delegated.supports_name("dynamic_alias"));
        assert_eq!(delegated.specs()[0].name, "dynamic_alias");
        let result = delegated.execute_named("dynamic_alias", json!({})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "dynamic_alias");

        let restricted = ToolArcRef::for_name(Arc::new(DynamicAliasTool), "dynamic_alias").unwrap();
        assert_eq!(restricted.name(), "dynamic_alias");
        assert_eq!(restricted.specs().len(), 1);
        assert!(restricted.supports_name("dynamic_alias"));
        assert!(!restricted.supports_name("dynamic_root"));
        assert!(restricted.execute_named("dynamic_root", json!({})).await.is_err());
    }

    struct OneToolThenFinalProvider;

    #[async_trait]
    impl Provider for OneToolThenFinalProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("unused".to_string())
        }

        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            let has_tool_message = request.messages.iter().any(|m| m.role == "tool");
            if has_tool_message {
                Ok(ChatResponse {
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            } else {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "call_1".to_string(),
                        name: "echo_tool".to_string(),
                        arguments: "{\"value\":\"ping\"}".to_string(),
                    }],
                    reasoning_content: None,
                })
            }
        }
    }

    fn agentic_config(allowed_tools: Vec<String>) -> DelegateAgentConfig {
        DelegateAgentConfig {
            messaging: crate::config::schema::AgentMessaging::default(),
            approval_wait_secs: 600,
            limits: crate::config::AgentLimits::default(),
            provider: "openrouter".to_string(),
            identity_dir: None,
            memory_scope: None,
            spawn_enabled: None,
            enabled: true,
            autonomy: None,
            model: "model-test".to_string(),
            system_prompt: Some("You are agentic.".to_string()),
            api_key: Some("delegate-test-credential".to_string()),
            credential_ref: None,
            temperature: Some(0.2),
            max_depth: 3,
            agentic: true,
            allowed_tools,
        }
    }

    fn router_model(provider: &str, model_id: &str, max_context: usize) -> crate::config::RouterModelConfig {
        crate::config::RouterModelConfig {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            cost_per_million_tokens: 1.0,
            max_context,
            reserved_output_tokens: None,
            latency_ms: 1_000,
            categories: vec!["code".to_string()],
            elo_rating: 1_000.0,
        }
    }

    #[test]
    fn name_and_schema() {
        let tool = DelegateTool::new(sample_agents(), test_security());
        assert_eq!(tool.name(), "delegate");
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["agent"].is_object());
        assert!(schema["properties"]["prompt"].is_object());
        assert!(schema["properties"]["context"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("agent")));
        assert!(required.contains(&json!("prompt")));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["properties"]["agent"]["minLength"], json!(1));
        assert_eq!(schema["properties"]["prompt"]["minLength"], json!(1));
    }

    #[test]
    fn description_not_empty() {
        let tool = DelegateTool::new(sample_agents(), test_security());
        assert!(!tool.description().is_empty(), "expected a non-empty value");
    }

    #[test]
    fn schema_lists_agent_names() {
        let tool = DelegateTool::new(sample_agents(), test_security());
        let schema = tool.parameters_schema();
        let desc = schema["properties"]["agent"]["description"].as_str().unwrap();
        assert!(desc.contains("researcher") || desc.contains("coder"));
    }

    #[tokio::test]
    async fn missing_agent_param() {
        let tool = DelegateTool::new(sample_agents(), test_security());
        let result = tool.execute(json!({"prompt": "test"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn missing_prompt_param() {
        let tool = DelegateTool::new(sample_agents(), test_security());
        let result = tool.execute(json!({"agent": "researcher"})).await;
        assert!(result.is_err());
    }

    async fn dispatch_configured_delegate(mut tool: DelegateTool, arguments: serde_json::Value) -> ToolResult {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.autonomy.level = AutonomyLevel::Full;
        config.agents = tool.agents.as_ref().clone();
        tool.security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
        dispatch_admitted_delegate(config, tool, arguments).await
    }

    #[tokio::test]
    async fn unknown_agent_returns_error() {
        let tool = DelegateTool::new(sample_agents(), test_security());
        let result = dispatch_configured_delegate(tool, json!({"agent":"nonexistent", "prompt":"test"})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown agent"));
    }

    #[tokio::test]
    async fn depth_limit_enforced() {
        let tool = DelegateTool::with_depth(sample_agents(), test_security(), 3);
        let result = dispatch_configured_delegate(tool, json!({"agent":"researcher", "prompt":"test"})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("depth limit"));
    }

    #[tokio::test]
    async fn depth_limit_per_agent() {
        // coder has max_depth=2, so depth=2 should be blocked
        let tool = DelegateTool::with_depth(sample_agents(), test_security(), 2);
        let result = dispatch_configured_delegate(tool, json!({"agent":"coder", "prompt":"test"})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("depth limit"));
    }

    #[test]
    fn empty_agents_schema() {
        let tool = DelegateTool::new(HashMap::new(), test_security());
        let schema = tool.parameters_schema();
        let desc = schema["properties"]["agent"]["description"].as_str().unwrap();
        assert!(desc.contains("none configured"));
    }

    #[tokio::test]
    async fn invalid_provider_returns_error() {
        let mut agents = HashMap::new();
        agents.insert(
            "broken".to_string(),
            DelegateAgentConfig {
                messaging: crate::config::schema::AgentMessaging::default(),
                approval_wait_secs: 600,
                limits: crate::config::AgentLimits::default(),
                provider: "totally-invalid-provider".to_string(),
                model: "model".to_string(),
                system_prompt: None,
                api_key: None,
                credential_ref: None,
                temperature: None,
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
                enabled: true,
                autonomy: None,
            },
        );
        let tool = DelegateTool::new(agents, test_security());
        let result = dispatch_configured_delegate(tool, json!({"agent":"broken", "prompt":"test"})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("is not one PRX can construct"));
    }

    #[tokio::test]
    async fn blank_agent_rejected() {
        let tool = DelegateTool::new(sample_agents(), test_security());
        let result = tool.execute(json!({"agent": "  ", "prompt": "test"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("must not be empty"));
    }

    #[tokio::test]
    async fn blank_prompt_rejected() {
        let tool = DelegateTool::new(sample_agents(), test_security());
        let result = tool
            .execute(json!({"agent": "researcher", "prompt": "  \t  "}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("must not be empty"));
    }

    #[tokio::test]
    async fn whitespace_agent_name_trimmed_and_found() {
        let tool = DelegateTool::new(single_agent("researcher", "mock", "test-model"), test_security());
        let result = dispatch_configured_delegate(tool, json!({"agent":" researcher ", "prompt":"test"})).await;
        assert!(result.success, "trimmed agent must execute: {:?}", result.error);
        assert!(result.output.contains("mock/test-model"));
    }

    #[tokio::test]
    async fn delegation_blocked_in_readonly_mode() {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.autonomy.level = AutonomyLevel::ReadOnly;
        config.agents = single_agent("researcher", "mock", "test-model");
        let readonly = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
        let tool = DelegateTool::new(config.agents.clone(), readonly);
        let (result, executions, history) =
            observe_delegate_dispatch(config, tool, json!({"agent":"researcher", "prompt":"test"}), None).await;
        assert_eq!(executions, 0, "readonly execution must reject before entering delegate");
        assert!(result.is_none());
        assert!(
            history.iter().any(|message| message.role == "user"
                && message
                    .content
                    .starts_with(crate::agent::turn_contracts::TOOL_RESULTS_PREFIX)
                && message
                    .content
                    .contains("tool 'delegate' is not permitted under the current execution policy")),
            "the parent must receive the actual policy denial: {history:?}"
        );
    }

    #[tokio::test]
    async fn delegate_context_is_prepended_to_prompt() {
        let (result, requests) = run_configured_delegate_fixture(
            vec!["dynamic_alias".to_owned()],
            vec![Arc::new(DynamicAliasTool)],
            "dynamic_alias",
            json!({}),
            None,
            false,
            Some("some context data"),
        )
        .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(requests.len(), 2, "child must execute the tool and finish");
        let messages = requests[0]["messages"].as_array().unwrap();
        let user_message = messages
            .iter()
            .find(|message| message["role"] == "user")
            .expect("the provider must receive the delegated user input");
        assert_eq!(
            user_message["content"],
            "[Context]\nsome context data\n\n[Task]\ndynamic_alias"
        );
    }

    #[tokio::test]
    async fn delegate_records_request_and_result_message_events() {
        let tmp = tempfile::TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let mut agents = HashMap::new();
        agents.insert(
            "tester".to_string(),
            DelegateAgentConfig {
                messaging: crate::config::schema::AgentMessaging::default(),
                approval_wait_secs: 600,
                limits: crate::config::AgentLimits::default(),
                provider: "mock".to_string(),
                model: "test-model".to_string(),
                system_prompt: None,
                api_key: None,
                credential_ref: None,
                temperature: None,
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
                enabled: true,
                autonomy: None,
            },
        );
        let mut config = crate::config::Config::default();
        config.workspace_dir = tmp.path().to_owned();
        config.config_path = tmp.path().join("config.toml");
        config.autonomy.level = AutonomyLevel::Full;
        config.agents = agents.clone();
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, tmp.path()));
        let tool = DelegateTool::new(agents, security).with_shared_memory(memory.clone());
        let channel = crate::channels::traits::ChannelMessage {
            id: "prx-source-message".to_owned(),
            sender: "prx_user".to_owned(),
            reply_target: "prx_channel".to_owned(),
            content: String::new(),
            channel: "telegram".to_owned(),
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
        let result = dispatch_admitted_delegate_from(
            config,
            tool,
            json!({"agent":"tester", "prompt":"delegate through fabric"}),
            Some(channel),
        )
        .await;
        assert!(result.success);

        let events = memory
            .list_message_events_since(
                &MemoryPrincipal {
                    require_owner_match: false,
                    workspace_id: tmp.path().to_string_lossy().to_string(),
                    agent_id: Some("tester".to_string()),
                    persona_id: None,
                    session_key: None,
                    channel: Some("telegram".to_string()),
                    sender: Some("prx_user".to_string()),
                    owner_id: None,
                    legacy_session_key: None,
                    agent_partition: None,
                },
                0,
                100,
            )
            .await
            .unwrap();

        let request_event = events
            .iter()
            .find(|event| event.role == "user" && event.source == "delegate")
            .expect("delegate request event");
        let result_event = events
            .iter()
            .find(|event| event.role == "event" && event.content.contains("tester"))
            .expect("delegate domain result event");
        assert_eq!(request_event.source, "delegate");
        assert_eq!(request_event.content, "delegate through fabric");
        assert_eq!(result_event.source, "delegate");
        assert_eq!(events.iter().filter(|event| event.role == "assistant").count(), 1);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "provider.final_outcome")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "turn.finalized")
                .count(),
            1
        );

        let parent_id = request_event
            .causation_event_id
            .as_deref()
            .expect("delegate input must reference the actual parent source event");
        let parent_events = memory
            .list_message_events_recent(
                &MemoryPrincipal {
                    require_owner_match: false,
                    workspace_id: tmp.path().to_string_lossy().to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: None,
                    channel: Some("telegram".to_owned()),
                    sender: Some("prx_user".to_owned()),
                    owner_id: None,
                    legacy_session_key: None,
                    agent_partition: None,
                },
                100,
            )
            .await
            .unwrap();
        let parent = parent_events
            .iter()
            .find(|event| event.event_id == parent_id)
            .expect("the referenced source event must actually be durable");
        assert_eq!(parent.role, "user");
        assert_eq!(parent.content, "delegate delegate through fabric");
        assert_eq!(request_event.parent_run_id, parent.run_id);
        assert_eq!(request_event.parent_session_key, parent.session_key);

        let task_events = memory
            .list_memory_events_since(
                &MemoryPrincipal {
                    require_owner_match: false,
                    workspace_id: tmp.path().to_string_lossy().to_string(),
                    agent_id: Some("tester".to_string()),
                    persona_id: None,
                    session_key: None,
                    channel: Some("telegram".to_string()),
                    sender: Some("prx_user".to_string()),
                    owner_id: None,
                    legacy_session_key: None,
                    agent_partition: None,
                },
                0,
                100,
            )
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.subject_table == "tasks")
            .collect::<Vec<_>>();
        let event_types = task_events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>();
        assert!(event_types.contains(&"delegate.task.started"));
        assert!(event_types.contains(&"delegate.task.completed"));
        assert!(task_events.iter().all(|event| !event.subject_id.is_empty()));
        assert!(task_events.iter().any(|event| {
            event.payload_json.as_deref().is_some_and(|payload| {
                serde_json::from_str::<serde_json::Value>(payload)
                    .is_ok_and(|value| value["source_message_event_id"] == parent_id)
            })
        }));
    }

    #[test]
    fn delegate_event_scope_is_derived_from_runtime_envelope() {
        let scope = DelegateScope {
            sender: "alice".to_string(),
            channel: "telegram".to_string(),
            chat_id: "chat-1".to_string(),
            owner_id: Some("owner:/tmp/ws:telegram:alice".to_string()),
            topic_id: Some("topic-a".to_string()),
            task_id: Some("task-a".to_string()),
            source_message_event_id: Some("msg-a".to_string()),
            config_generation_id: Some(17),
            config_source_revision: Some("revision-17".to_string()),
        };
        let event_scope = delegate_event_scope("/tmp/ws", "run-delegate", "tester", Some(&scope));

        assert_eq!(event_scope.source, "delegate");
        assert_eq!(event_scope.channel.as_deref(), Some("telegram"));
        assert_eq!(
            event_scope.session_key.as_deref(),
            Some("delegate:telegram:chat-1:alice")
        );
        assert_eq!(event_scope.run_id.as_deref(), Some("run-delegate"));
        assert_eq!(event_scope.config_generation_id, Some(17));
        assert_eq!(event_scope.config_source_revision.as_deref(), Some("revision-17"));
        assert_eq!(event_scope.agent_id.as_deref(), Some("tester"));
        assert_eq!(event_scope.sender.as_deref(), Some("alice"));
        assert_eq!(event_scope.recipient.as_deref(), Some("chat-1"));
        assert_eq!(event_scope.owner_id.as_deref(), Some("owner:/tmp/ws:telegram:alice"));
        assert_eq!(
            event_scope.subject,
            Some(crate::memory::MessageEventSubject::Task("run-delegate".into()))
        );
    }

    /// Two delegates under one parent task write their own task identity, so a
    /// sibling's live request and prior segment never reach the other child,
    /// while the shared main-channel event stays visible to both.
    #[tokio::test]
    async fn delegates_under_one_parent_task_recall_only_their_own_task() {
        use crate::agent::context_recall::{RECALL_CONTEXT_BUDGET, RecallScope, scoped_recall};
        use crate::memory::{MessageEventSource, MessageEventSubject, SqliteMemory};

        let tmp = tempfile::TempDir::new().expect("test: temp dir");
        let ws = tmp.path().to_string_lossy().to_string();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: sqlite"));
        let fabric = MemoryFabric::new(Arc::clone(&memory), ws.clone());
        let parent = DelegateScope {
            sender: "alice".to_string(),
            channel: "telegram".to_string(),
            chat_id: "chat-1".to_string(),
            owner_id: Some("owner-T".to_string()),
            topic_id: None,
            task_id: Some("task-T".to_string()),
            source_message_event_id: None,
            config_generation_id: Some(1),
            config_source_revision: Some("rev-1".to_string()),
        };
        let mut shared = delegate_event_scope(&ws, "run-main", "tester", Some(&parent));
        shared.source = MessageEventSource::Channel;
        shared.subject = None;
        fabric
            .record_inbound_user_message(shared, "SHARED-MAIN-POSITIVE", None, None)
            .await
            .expect("test: shared record");

        let mut lanes = Vec::new();
        for run in ["run-a", "run-b"] {
            let scope = delegate_event_scope(&ws, run, "tester", Some(&parent));
            assert_eq!(scope.subject, Some(MessageEventSubject::Task(run.to_string())));
            assert_ne!(scope.subject, Some(MessageEventSubject::Task("task-T".to_string())));
            assert_eq!(scope.session_key.as_deref(), Some("delegate:telegram:chat-1:alice"));
            let live = fabric
                .record_inbound_user_message(scope.clone(), format!("SENTINEL-{run}"), None, None)
                .await
                .expect("test: live record");
            assert!(live.id > 0);
            let mut prior = scope.clone();
            prior.run_id = Some(format!("child-segment:{run}:1"));
            fabric
                .record_inbound_user_message(prior, format!("OWN-PRIOR-{run}"), None, None)
                .await
                .expect("test: prior record");
            lanes.push((run, live));
        }

        for (run, live) in &lanes {
            let mut env = RuntimeEnvelope::delegate(&ws, "delegate:telegram:chat-1:alice", *run)
                .with_agent_id("tester")
                .with_channel("telegram")
                .with_sender("alice")
                .with_owner_id("owner-T")
                .with_owner_isolation();
            env.task_id = crate::runtime::turn_source::subject_task_id(live.subject.as_ref()).map(str::to_owned);
            env.source_message_event_id = Some(live.event_id.clone());
            let config = crate::config::MemoryConfig {
                min_relevance_score: 0.0,
                ..Default::default()
            };
            let scope = RecallScope::from_envelope(&env, &config, "private");
            let recalled = scoped_recall(memory.as_ref(), &scope, "delegate live request", RECALL_CONTEXT_BUDGET).await;
            let preamble = recalled.preamble;
            let (own, other) = if *run == "run-a" {
                ("run-a", "run-b")
            } else {
                ("run-b", "run-a")
            };
            assert!(
                preamble.contains(&format!("OWN-PRIOR-{own}")),
                "{run} lost its own prior segment: {preamble}"
            );
            assert!(
                preamble.contains("SHARED-MAIN-POSITIVE"),
                "{run} lost the shared main event: {preamble}"
            );
            assert!(
                !preamble.contains(&format!("SENTINEL-{other}")),
                "{run} recalled the sibling live request: {preamble}"
            );
            assert!(
                !preamble.contains(&format!("OWN-PRIOR-{other}")),
                "{run} recalled the sibling prior segment: {preamble}"
            );
        }
    }

    #[tokio::test]
    async fn delegate_task_input_preserves_inherited_remote_provenance() {
        use crate::tools::sessions_spawn::{SPAWN_EXECUTION_CONTEXT, SpawnExecutionContext};
        struct InputProbe(tokio::sync::mpsc::UnboundedSender<Option<crate::messaging::InheritedProvenance>>);
        #[async_trait]
        impl Tool for InputProbe {
            fn name(&self) -> &str {
                "dynamic_alias"
            }
            fn description(&self) -> &str {
                "Report inherited task input"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                json!({"type":"object","properties":{}})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
                let input =
                    SPAWN_EXECUTION_CONTEXT.try_with(|context| context.task_input.inherited_provenance.clone())?;
                self.0
                    .send(input)
                    .map_err(|_| anyhow::anyhow!("task input probe receiver closed"))?;
                Ok(ToolResult {
                    success: true,
                    output: "dynamic_alias".into(),
                    error: None,
                })
            }
        }
        let provenance = crate::messaging::InheritedProvenance {
            remote: crate::messaging::RemotePeerAttr {
                issuer: "spiffe://prx".into(),
                subject: "spiffe://prx/peer".into(),
                message_id: uuid::Uuid::from_u128(42),
            },
            chain: crate::messaging::CausalChain {
                trigger: uuid::Uuid::from_u128(42),
                root: uuid::Uuid::from_u128(42),
                depth: 2,
                ancestry: Vec::new(),
            },
        };
        let mut parent = SpawnExecutionContext::seed_turn_context("prx-parent".into(), "prx-session".into());
        provenance.seal_into_task_input(&mut parent.task_input);
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let (result, requests) = SPAWN_EXECUTION_CONTEXT
            .scope(
                parent,
                run_configured_delegate_fixture(
                    vec!["dynamic_alias".into()],
                    vec![Arc::new(InputProbe(sender))],
                    "dynamic_alias",
                    json!({}),
                    None,
                    false,
                    None,
                ),
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(
            requests.len(),
            2,
            "real delegated child must execute its tool and finish"
        );
        let actual = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .expect("delegated tool observes task input")
            .expect("probe result");
        assert_eq!(actual.as_ref(), Some(&provenance));
    }

    #[test]
    fn parse_delegate_scope_preserves_owner_topic_task_lineage() {
        let scope = parse_delegate_scope(&json!({
            "_zc_scope_trusted": true,
            "_zc_scope": {
                "sender": "alice",
                "channel": "telegram",
                "chat_type": "direct",
                "chat_id": "chat-1",
                "owner_id": "owner:/tmp/ws:telegram:alice",
                "topic_id": "topic-a",
                "task_id": "task-a",
                "source_message_event_id": "msg-a",
                "config_generation_id": 17,
                "config_source_revision": "revision-17"
            }
        }))
        .expect("trusted scope should parse");

        assert_eq!(scope.owner_id.as_deref(), Some("owner:/tmp/ws:telegram:alice"));
        assert_eq!(scope.topic_id.as_deref(), Some("topic-a"));
        assert_eq!(scope.task_id.as_deref(), Some("task-a"));
        assert_eq!(scope.source_message_event_id.as_deref(), Some("msg-a"));
        assert_eq!(scope.config_generation_id, Some(17));
        assert_eq!(scope.config_source_revision.as_deref(), Some("revision-17"));
    }

    #[tokio::test]
    async fn delegate_empty_context_omits_prefix() {
        assert_empty_delegate_context().await;
    }

    async fn assert_empty_delegate_context() {
        let (result, requests) = run_configured_delegate_fixture(
            vec!["dynamic_alias".to_owned()],
            vec![Arc::new(DynamicAliasTool)],
            "dynamic_alias",
            json!({}),
            None,
            false,
            Some(""),
        )
        .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(
            requests.len(),
            2,
            "child must execute the tool and finish: {}",
            result.output
        );
        let messages = requests[0]["messages"].as_array().unwrap();
        let user_message = messages
            .iter()
            .find(|message| message["role"] == "user")
            .expect("the provider must receive the delegated user input");
        assert_eq!(user_message["content"], "dynamic_alias");
    }

    /// The endpoint stalls only after a real child decision request arrives.
    /// This spends more wall time than the unchanged 500 ms decision budget.
    #[tokio::test(start_paused = true)]
    async fn delegate_empty_context_executes_child_after_wall_preemption() {
        let fixture = DELEGATE_CHILD_RESPONSE.scope(
            crate::agent::agent::native_decision_fixture::ChildResponseControl::WallStall(
                std::time::Duration::from_millis(600),
            ),
            async {
                #[cfg(unix)]
                super::delegate_diagnostic::scope(assert_empty_delegate_context)
                    .await
                    .expect("owned preemption diagnostic");
                #[cfg(not(unix))]
                assert_empty_delegate_context().await;
            },
        );
        tokio::pin!(fixture);
        let started = std::time::Instant::now();
        loop {
            if futures::poll!(fixture.as_mut()).is_ready() {
                break;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(20),
                "fixture wall hang guard"
            );
            // Keep this task runnable while real socket/SQLite I/O completes;
            // paused Tokio transport timers must not auto-advance on idle.
            tokio::task::yield_now().await;
        }
    }

    /// Spending the child's logical decision time no longer refuses the
    /// child: after R9 the engine is advisory only, its analysis is charged to
    /// the turn and skipped when the budget is tight, and the request is never
    /// refused because of it. The child executes its tool once and finishes.
    #[tokio::test]
    async fn delegate_child_logical_deadline_never_refuses_the_child() {
        let clock = crate::runtime::logical_clock::ManualClock::new();
        let control = crate::agent::agent::native_decision_fixture::ChildResponseControl::AdvanceClock(
            Arc::clone(&clock),
            std::time::Duration::from_millis(600),
        );
        let (result, requests) = crate::runtime::logical_clock::scope_local(
            clock,
            DELEGATE_CHILD_RESPONSE.scope(
                control,
                run_configured_delegate_fixture(
                    vec!["dynamic_alias".to_owned()],
                    vec![Arc::new(DynamicAliasTool)],
                    "dynamic_alias",
                    json!({}),
                    None,
                    false,
                    Some(""),
                ),
            ),
        )
        .await;
        assert!(
            result.success,
            "test: a spent logical deadline never refuses the child: {:?}",
            result.error
        );
        assert_eq!(
            requests.len(),
            2,
            "test: the child executes the tool, then finishes: {}",
            result.output
        );
        let tool_replies: Vec<&str> = requests
            .iter()
            .filter_map(|request| request["messages"].as_array())
            .flatten()
            .filter(|message| message["role"] == "tool")
            .filter_map(|message| message["content"].as_str())
            .collect();
        assert_eq!(
            tool_replies,
            ["dynamic_alias"],
            "test: the allowed tool executes exactly once"
        );
        assert!(
            result.output.ends_with("\ndone"),
            "test: the child returns its final reply: {}",
            result.output
        );
    }

    #[test]
    fn delegate_depth_construction() {
        let tool = DelegateTool::with_depth(sample_agents(), test_security(), 5);
        assert_eq!(tool.depth, 5);
    }

    #[tokio::test]
    async fn delegate_no_agents_configured() {
        let tool = DelegateTool::new(HashMap::new(), test_security());
        let result = dispatch_configured_delegate(tool, json!({"agent":"any", "prompt":"test"})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("none configured"));
    }

    struct CapturedDelegate {
        inner: DelegateTool,
        result: Arc<parking_lot::Mutex<Option<ToolResult>>>,
        executions: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for CapturedDelegate {
        fn name(&self) -> &str {
            self.inner.name()
        }
        fn description(&self) -> &str {
            self.inner.description()
        }
        fn parameters_schema(&self) -> serde_json::Value {
            self.inner.parameters_schema()
        }
        fn tier(&self) -> ToolTier {
            self.inner.tier()
        }
        fn categories(&self) -> &'static [ToolCategory] {
            self.inner.categories()
        }
        async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
            #[cfg(unix)]
            super::delegate_diagnostic::phase("captor-entered.json");
            self.executions.fetch_add(1, Ordering::SeqCst);
            let make = || self.inner.execute(args);
            #[cfg(unix)]
            super::delegate_diagnostic::future_layout("delegate-trait-future-layout.json", &make);
            match make().await {
                Ok(result) => {
                    *self.result.lock() = Some(result.clone());
                    Ok(result)
                }
                Err(error) => {
                    *self.result.lock() = Some(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(error.to_string()),
                    });
                    Err(error)
                }
            }
        }
    }

    struct DelegateParentProvider {
        arguments: serde_json::Value,
    }

    #[async_trait]
    impl Provider for DelegateParentProvider {
        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            anyhow::bail!("fixture must use the shared parent tool loop")
        }
        async fn chat(&self, request: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
            let content = if request.messages.iter().any(|message| {
                message.role == "tool"
                    || (message.role == "user"
                        && message
                            .content
                            .starts_with(crate::agent::turn_contracts::TOOL_RESULTS_PREFIX))
            }) {
                "parent complete".to_owned()
            } else {
                format!(
                    "<tool_call>\n{}\n</tool_call>",
                    json!({
                        "name":"delegate", "arguments":self.arguments
                    })
                )
            };
            Ok(ChatResponse {
                text: Some(content),
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }
    }

    struct NativeDelegateParentProvider {
        arguments: serde_json::Value,
    }

    #[async_trait]
    impl Provider for NativeDelegateParentProvider {
        fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
            crate::providers::traits::ProviderCapabilities {
                native_tool_calling: true,
                vision: false,
            }
        }
        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            anyhow::bail!("fixture must use the shared parent tool loop")
        }
        async fn chat(&self, request: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
            if request.messages.iter().any(|message| {
                message.role == "tool"
                    || (message.role == "user"
                        && message
                            .content
                            .starts_with(crate::agent::turn_contracts::TOOL_RESULTS_PREFIX))
            }) {
                return Ok(ChatResponse {
                    text: Some("parent complete".to_owned()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                });
            }
            Ok(ChatResponse {
                text: None,
                tool_calls: vec![crate::providers::traits::ToolCall {
                    id: "prx-parent-delegate".to_owned(),
                    name: "delegate".to_owned(),
                    arguments: self.arguments.to_string(),
                }],
                reasoning_content: None,
            })
        }
    }

    struct DelegateFixtureServer(tokio::task::JoinHandle<()>);
    impl Drop for DelegateFixtureServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    tokio::task_local! { static CONFIGURED_DELEGATE_ENDPOINT: String; }
    tokio::task_local! {
        static DELEGATE_CHILD_RESPONSE: crate::agent::agent::native_decision_fixture::ChildResponseControl;
    }
    // Set by a test whose child reaches its tool through `capability_search`
    // first (R28: a child that owns the discovery control keeps deferred
    // tools behind search).
    tokio::task_local! { static DELEGATE_CHILD_SEARCHES_FIRST: (); }

    async fn run_configured_delegate_fixture(
        allowed_tools: Vec<String>,
        parent_tools: Vec<Arc<dyn Tool>>,
        tool_name: &'static str,
        arguments: serde_json::Value,
        provider_error: Option<&'static str>,
        custom_userinfo: bool,
        context: Option<&str>,
    ) -> (ToolResult, Vec<serde_json::Value>) {
        let retained = tempfile::tempdir().expect("private decision evidence");
        let driver = crate::agent::agent::native_decision_fixture::Driver::start_delegation("native_tool")
            .await
            .expect("owned compatible decision listener")
            .retain_evidence_at(retained.path().to_path_buf());
        if let Ok(control) = DELEGATE_CHILD_RESPONSE.try_with(Clone::clone) {
            driver.control_child_response(control);
        }
        let search_first = DELEGATE_CHILD_SEARCHES_FIRST.try_with(|()| ()).is_ok();
        let endpoint = driver.endpoint().to_owned();
        let clock =
            crate::runtime::logical_clock::capture().unwrap_or_else(crate::runtime::logical_clock::ManualClock::new);
        // These tests assert child tool behavior. Exclude OS preemption from
        // admission while leaving production budgets and real I/O intact.
        crate::runtime::logical_clock::scope_local(
            clock,
            driver.run_factory(move || async move {
                observe_owned_delegate_workspace(move |workspace| async move {
                    CONFIGURED_DELEGATE_ENDPOINT
                        .scope(
                            endpoint,
                            boxed_delegate_observation(|| {
                                run_admitted_delegate_fixture(
                                    workspace,
                                    allowed_tools,
                                    parent_tools,
                                    tool_name,
                                    arguments,
                                    provider_error,
                                    custom_userinfo,
                                    context,
                                    search_first,
                                )
                            }),
                        )
                        .await
                })
                .await
            }),
        )
        .await
    }

    async fn observe_owned_delegate_workspace<T, F, Fut>(work: F) -> T
    where
        F: FnOnce(std::path::PathBuf) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        use futures::FutureExt as _;
        // The work future is dropped before CleanupScope drains its writers.
        // Its temporary files must therefore belong to this enclosing owner.
        let workspace = tempfile::tempdir().expect("delegate fixture workspace");
        #[cfg(unix)]
        let workspace = super::delegate_diagnostic::workspace(workspace);
        let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
            .expect("delegate fixture cleanup owner");
        let mut observed = None;
        let drained = cleanup
            .run(boxed_delegate_observation(|| async {
                observed = Some(
                    std::panic::AssertUnwindSafe(boxed_delegate_observation(|| work(workspace.path().to_owned())))
                        .catch_unwind()
                        .await,
                );
                Ok(())
            }))
            .await;
        if drained.is_err() {
            // An unconfirmed writer may still be using this directory. Keep
            // that failure's files, but never leak a confirmed workspace.
            #[cfg(unix)]
            if let super::delegate_diagnostic::Workspace::Temporary(directory) = workspace {
                let _retained = directory.keep();
            }
            #[cfg(not(unix))]
            let _retained = workspace.keep();
            tracing::warn!("delegate fixture workspace retained after unconfirmed cleanup");
        }
        match observed {
            Some(Err(panic)) => std::panic::resume_unwind(panic),
            Some(Ok(value)) => {
                drained.expect("delegate fixture writers drained");
                value
            }
            None => {
                drained.expect("delegate fixture cleanup entered");
                panic!("delegate fixture did not run")
            }
        }
    }

    #[tokio::test]
    async fn a_panicking_delegate_fixture_keeps_its_workspace_until_owned_writers_drain() {
        use futures::FutureExt as _;
        let (finished, observed_writer) = tokio::sync::oneshot::channel();
        let result: Result<(), _> =
            std::panic::AssertUnwindSafe(observe_owned_delegate_workspace(move |workspace| async move {
                let owner = crate::runtime::turn_cleanup::current_tracker().expect("fixture cleanup owner");
                let cancellation = tokio_util::sync::CancellationToken::new();
                let ticket = owner.register(cancellation.clone()).expect("owned fixture writer");
                tokio::spawn(async move {
                    cancellation.cancelled().await;
                    let existed = workspace.is_dir();
                    let written = tokio::fs::write(workspace.join("owned-drain-proof"), b"drained")
                        .await
                        .is_ok();
                    finished
                        .send((workspace, existed, written))
                        .expect("fixture writer observation");
                    ticket.finish(written);
                });
                panic!("controlled delegate fixture abort");
            }))
            .catch_unwind()
            .await;
        assert!(result.is_err(), "the original fixture panic must escape");
        assert_eq!(
            result
                .as_ref()
                .err()
                .and_then(|panic| panic.downcast_ref::<&str>())
                .copied(),
            Some("controlled delegate fixture abort"),
            "cleanup must preserve the original panic rather than replace it"
        );
        let (workspace, existed, written) = observed_writer.await.expect("owned writer completed");
        assert!(existed, "the real cleanup drain must retain the fixture workspace");
        assert!(
            written,
            "the owned writer must finish before temporary-directory cleanup"
        );
        assert!(!workspace.exists(), "the confirmed fixture workspace must not leak");
    }

    #[tokio::test]
    async fn an_unconfirmed_delegate_fixture_keeps_its_workspace() {
        use futures::FutureExt as _;
        let (finished, observed_workspace) = tokio::sync::oneshot::channel();
        let result: Result<(), _> =
            std::panic::AssertUnwindSafe(observe_owned_delegate_workspace(move |workspace| async move {
                let owner = crate::runtime::turn_cleanup::current_tracker().expect("fixture cleanup owner");
                let ticket = owner
                    .register(tokio_util::sync::CancellationToken::new())
                    .expect("owned fixture writer");
                ticket.finish(false);
                finished.send(workspace).expect("fixture workspace observation");
            }))
            .catch_unwind()
            .await;
        assert!(result.is_err(), "unconfirmed cleanup must not be accepted");
        let workspace = observed_workspace.await.expect("fixture work completed");
        assert!(workspace.is_dir(), "unconfirmed writer files must be retained");
    }

    async fn run_admitted_delegate_fixture(
        workspace: std::path::PathBuf,
        allowed_tools: Vec<String>,
        parent_tools: Vec<Arc<dyn Tool>>,
        tool_name: &'static str,
        arguments: serde_json::Value,
        provider_error: Option<&'static str>,
        custom_userinfo: bool,
        context: Option<&str>,
        search_first: bool,
    ) -> (ToolResult, Vec<serde_json::Value>) {
        let requests = Arc::new(parking_lot::Mutex::new(Vec::<serde_json::Value>::new()));
        let capture = Arc::clone(&requests);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let handler = axum::routing::post(move |axum::Json(request): axum::Json<serde_json::Value>| {
            let capture = Arc::clone(&capture);
            let arguments = arguments.clone();
            async move {
                let results = request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|message| message["role"] == "tool")
                    .count();
                capture.lock().push(request);
                if let Some(error) = provider_error {
                    return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error":error})));
                }
                let completed = results > usize::from(search_first);
                let message = if completed {
                    json!({"role":"assistant", "content":"done"})
                } else if search_first && results == 0 {
                    json!({"role":"assistant", "content":"", "tool_calls":[{
                        "function":{"name":"capability_search", "arguments":{"query":tool_name, "limit":1}}
                    }]})
                } else if custom_userinfo {
                    json!({"role":"assistant", "content":null, "tool_calls":[{
                        "id":"call_dynamic", "type":"function",
                        "function":{"name":tool_name, "arguments":arguments.to_string()}
                    }]})
                } else {
                    json!({"role":"assistant", "content":"", "tool_calls":[{
                        "function":{"name":tool_name, "arguments":arguments}
                    }]})
                };
                let body = if custom_userinfo {
                    json!({"id":"fixture-response", "object":"chat.completion", "created":0,
                            "model":"prx-local-model", "choices":[{"index":0, "message":message,
                            "finish_reason":if completed { "stop" } else { "tool_calls" }}],
                            "usage":{"prompt_tokens":1, "completion_tokens":1, "total_tokens":2}})
                } else {
                    json!({"model":"prx-local-model", "message":message,
                            "done":true, "prompt_eval_count":1, "eval_count":1})
                };
                (axum::http::StatusCode::OK, axum::Json(body))
            }
        });
        let app = axum::Router::new()
            .route("/api/chat", handler.clone())
            .route("/v1/chat/completions", handler);
        let mut server = DelegateFixtureServer(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.clone();
        config.config_path = workspace.join("config.toml");
        config.api_url = Some(endpoint.clone());
        config.default_provider = Some("ollama".to_owned());
        config.default_model = Some("prx-local-model".to_owned());
        config.reliability.provider_retries = 0;
        config.autonomy.level = AutonomyLevel::Full;
        let mut agent = agentic_config(allowed_tools);
        agent.provider = if custom_userinfo {
            format!(
                "custom:http://prx_user:DELEGATE-SECRET-pw@{}/v1",
                endpoint.trim_start_matches("http://")
            )
        } else {
            "ollama".to_owned()
        };
        agent.model = "prx-local-model".to_owned();
        if custom_userinfo {
            config.providers.models.push(
                serde_json::from_value(json!({
                    "provider": agent.provider,
                    "model": agent.model,
                    "operations": ["chat"],
                    "native_tools": true,
                }))
                .expect("custom child fixture model descriptor"),
            );
        }
        if !custom_userinfo {
            // The local Ollama fixture owns one root connection: key and URL
            // must resolve together, not mix an agent key with a root URL.
            config.api_key = agent.api_key.take();
        }
        let fixture_price_key = if custom_userinfo {
            format!("{}/{}", agent.provider, agent.model)
        } else {
            "prx-local-model".to_owned()
        };
        config.agents.insert("tester".to_owned(), agent);
        config.cost.prices.insert(
            fixture_price_key,
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            },
        );
        let delegate = DelegateTool::new(
            config.agents.clone(),
            Arc::new(SecurityPolicy::from_config(&config.autonomy, &workspace)),
        )
        .with_parent_tools(Arc::new(parent_tools.clone()));
        let delegate = if let Some(hooks) = crate::tools::child_hooks::parent_hook_manager(&parent_tools) {
            delegate.with_parent_hooks(hooks)
        } else {
            delegate
        };
        use futures::FutureExt as _;
        let mut arguments = json!({"agent":"tester", "prompt":tool_name});
        if let Some(context) = context {
            arguments["context"] = json!(context);
        }
        let result = std::panic::AssertUnwindSafe(dispatch_admitted_delegate(config, delegate, arguments))
            .catch_unwind()
            .await;
        server.0.abort();
        let _ = (&mut server.0).await;
        let requests = requests.lock().clone();
        match result {
            Ok(result) => (result, requests),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    pub(super) async fn dispatch_admitted_delegate(
        config: crate::config::Config,
        delegate: DelegateTool,
        arguments: serde_json::Value,
    ) -> ToolResult {
        dispatch_admitted_delegate_from(config, delegate, arguments, None).await
    }

    async fn dispatch_admitted_delegate_from(
        config: crate::config::Config,
        delegate: DelegateTool,
        arguments: serde_json::Value,
        channel: Option<crate::channels::traits::ChannelMessage>,
    ) -> ToolResult {
        let (result, executions, _) = observe_delegate_dispatch(config, delegate, arguments, channel).await;
        assert_eq!(executions, 1, "the admitted delegate must execute exactly once");
        result.expect("delegate must actually execute")
    }

    // Construct the large observer state in a separate synchronous frame.
    // The enclosing async function still defers all work until first poll.
    #[inline(never)]
    fn boxed_delegate_observation<F, Fut>(make_work: F) -> std::pin::Pin<Box<Fut>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future,
    {
        Box::pin(make_work())
    }

    async fn observe_delegate_dispatch(
        config: crate::config::Config,
        delegate: DelegateTool,
        arguments: serde_json::Value,
        channel: Option<crate::channels::traits::ChannelMessage>,
    ) -> (Option<ToolResult>, usize, Vec<ChatMessage>) {
        boxed_delegate_observation(move || async move {
            let workspace = config.workspace_dir.clone();
            let memory = delegate.memory.clone();
            let parent_tools = delegate.parent_tools.as_ref().clone();
            let captured = Arc::new(parking_lot::Mutex::new(None));
            let executions = Arc::new(AtomicUsize::new(0));
            let mut tools: Vec<Box<dyn Tool>> = parent_tools
                .into_iter()
                .filter(|tool| tool.name() != "delegate")
                .map(|tool| Box::new(ToolArcRef::new(tool)) as Box<dyn Tool>)
                .collect();
            tools.push(Box::new(CapturedDelegate {
                inner: delegate,
                result: Arc::clone(&captured),
                executions: Arc::clone(&executions),
            }));
            let registry = crate::agent::test_admission::registry_with_search(tools);
            let input = format!("delegate {}", arguments["prompt"].as_str().unwrap_or_default());
            let configured_endpoint = CONFIGURED_DELEGATE_ENDPOINT.try_with(Clone::clone).ok();
            let parent_provider: Box<dyn Provider> = if configured_endpoint.is_some() {
                Box::new(NativeDelegateParentProvider { arguments })
            } else {
                Box::new(DelegateParentProvider { arguments })
            };
            #[cfg(unix)]
            let dispatch_audit = (configured_endpoint.is_some() && super::delegate_diagnostic::active())
                .then(|| Arc::new(super::delegate_diagnostic::DispatchAudit::default()));
            #[cfg(unix)]
            let audit_sink = dispatch_audit
                .as_ref()
                .map(|audit| Arc::clone(audit) as Arc<dyn crate::tools::execution::ToolExecutionAuditSink>);
            #[cfg(not(unix))]
            let audit_sink = None;
            let admitted = if let Some(endpoint) = configured_endpoint.as_deref() {
                assert!(
                    channel.is_none() && memory.is_none(),
                    "configured alias owns its CLI SQLite source"
                );
                crate::agent::test_admission::AdmittedTurn::configured_delegation(
                    config,
                    &input,
                    Arc::clone(&registry),
                    endpoint,
                    parent_provider.as_ref(),
                    audit_sink,
                )
                .await
            } else {
                match (channel, memory) {
                    (Some(mut message), Some(memory)) => {
                        message.content = input.clone();
                        crate::agent::test_admission::AdmittedTurn::with_channel_memory(
                            config,
                            message,
                            Arc::clone(&registry),
                            memory,
                        )
                        .await
                    }
                    (Some(_), None) => panic!("channel delegate fixture requires its observed memory"),
                    (None, Some(memory)) => {
                        crate::agent::test_admission::AdmittedTurn::with_memory(
                            config,
                            &input,
                            Arc::clone(&registry),
                            memory,
                        )
                        .await
                    }
                    (None, None) => {
                        crate::agent::test_admission::AdmittedTurn::with_config(config, &input, Arc::clone(&registry))
                            .await
                    }
                }
            };
            #[cfg(unix)]
            if super::delegate_diagnostic::active() {
                super::delegate_diagnostic::admission(admitted.decision().context, parent_provider.capabilities());
            }
            if configured_endpoint.is_some() {
                // Prediction-free opening (R9): the configured parent publishes
                // the delegator with its sealed outgoing child manifest, and
                // the model chooses the call.
                let decision = admitted.decision();
                let published = decision
                    .service
                    .agent_execution_specs(decision.proof, decision.context)
                    .expect("configured parent publication");
                assert!(
                    published.specs.iter().any(|spec| spec.name == "delegate"),
                    "configured parent must publish the delegate tool: {:?}",
                    published.specs.iter().map(|spec| &spec.name).collect::<Vec<_>>()
                );
                assert!(
                    decision.context.runtime().outgoing_delegation.is_some(),
                    "configured parent must seal an outgoing child manifest"
                );
            }
            let mut history = vec![crate::providers::ChatMessage::user(input)];
            let hooks = HookManager::new(workspace);
            let multimodal = crate::config::MultimodalConfig::default();
            let parent_cleanup = crate::runtime::turn_cleanup::current_tracker();
            let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(
                parent_cleanup.as_ref(),
                tokio_util::sync::CancellationToken::new(),
            )
            .expect("delegate fixture cleanup owner");
            let result = cleanup
                .run(async {
                    Ok(tokio::time::timeout(
                        std::time::Duration::from_secs(20),
                        crate::agent::turn_engine::run_turn(
                            &mut history,
                            crate::agent::turn_request::TurnRequest {
                                advice: None,
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
                                provider: parent_provider.as_ref(),
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
                    .await)
                })
                .await;
            #[cfg(unix)]
            if super::delegate_diagnostic::active() {
                let kind = match &result {
                    Err(_) => "cleanup_error",
                    Ok(Err(_)) => "timeout",
                    Ok(Ok(Err(_))) => "engine_error",
                    Ok(Ok(Ok(_))) => "engine_success",
                };
                super::delegate_diagnostic::outcome(
                    kind,
                    executions.load(Ordering::SeqCst),
                    captured.lock().is_some(),
                    &admitted.decision().context.terminal_execution_trace(),
                    dispatch_audit.as_deref(),
                );
            }
            if configured_endpoint.is_some() {
                let _ = admitted.decision().context.terminal_execution_trace();
            }
            result
                .expect("delegate fixture cleanup")
                .expect("bounded local delegate fixture")
                .expect("parent execution");
            let result = captured.lock().take();
            (result, executions.load(Ordering::SeqCst), history)
        })
        .await
    }

    /// Finding 96b: a delegate child emits its hook events through the
    /// parent's live manager. Every delegate child runs under its ordinary
    /// parent runtime (P2-R15), whose hooks are that manager.
    #[tokio::test]
    async fn a_delegate_child_shares_the_parents_hook_manager() {
        let temp = tempfile::tempdir().unwrap();
        let hook_workspace = temp.path().join("hooks-ws");
        std::fs::create_dir_all(&hook_workspace).unwrap();
        let log = temp.path().join("llm-request.log");
        let hooks = json!({
            "timeout_ms": 60_000,
            "hooks": {"llm_request": [{
                "command": "sh",
                "args": ["-c", format!("echo hit >> '{}'", log.display())],
                "stdin_json": false
            }]}
        });
        std::fs::write(hook_workspace.join("hooks.json"), hooks.to_string()).unwrap();
        let manager = Arc::new(HookManager::new(hook_workspace).allowing_workspace_hooks(temp.path().join("hooks")));
        let mut parent_tools: Vec<Arc<dyn Tool>> = vec![Arc::new(DynamicAliasTool)];
        parent_tools.extend(manager.control_tool_arcs(test_security()));

        let (result, _) = run_configured_delegate_fixture(
            vec!["dynamic_alias".to_owned()],
            parent_tools,
            "dynamic_alias",
            json!({}),
            None,
            false,
            None,
        )
        .await;
        assert!(result.success, "{:?}", result.error);
        assert!(manager.drain(std::time::Duration::from_secs(30)).await);
        assert!(
            manager.dispatch_stats().delivered >= 1,
            "the child's events reach the parent's queue"
        );
        let recorded = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(recorded.contains("hit"), "the parent's hook recorded the child's event");
    }

    #[tokio::test]
    async fn agentic_mode_accepts_and_executes_one_explicit_dynamic_alias() {
        let work = || {
            run_configured_delegate_fixture(
                vec!["dynamic_alias".to_owned()],
                vec![Arc::new(DynamicAliasTool)],
                "dynamic_alias",
                json!({}),
                None,
                false,
                None,
            )
        };
        #[cfg(unix)]
        let (result, requests) = super::delegate_diagnostic::scope(work)
            .await
            .expect("complete private delegate diagnostic");
        #[cfg(not(unix))]
        let (result, requests) = work().await;
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.ends_with("\ndone"));
        assert_eq!(requests.len(), 2, "child must call its tool and finish");
        assert!(
            requests[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool" && message["content"] == "dynamic_alias"),
            "the alias must execute, not merely be offered"
        );
    }

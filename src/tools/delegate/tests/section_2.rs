    /// INT-2B USERINFO-3, RED before: the agentic path logged the child's
    /// provider label raw in its compaction-window debug line. After: a
    /// `custom:` URL's `user:password@` is redacted.
    #[tokio::test]
    async fn agentic_mode_never_logs_provider_url_userinfo() {
        // Log-capturing tests share per-callsite interest process-wide.
        let _serial = crate::config::secret_paths::fixture::LOG_CALLSITE_LOCK.lock().await;
        #[derive(Clone, Default)]
        struct Sink(Arc<parking_lot::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer({
                let sink = sink.clone();
                move || sink.clone()
            })
            .finish();
        use tracing::instrument::WithSubscriber as _;

        let (result, requests) = run_configured_delegate_fixture(
            vec!["dynamic_alias".to_owned()],
            vec![Arc::new(DynamicAliasTool)],
            "dynamic_alias",
            json!({}),
            None,
            true,
            None,
        )
        .with_subscriber(crate::config::secret_paths::fixture::capture_dispatch(subscriber))
        .await;
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.ends_with("\ndone"));
        assert_eq!(requests.len(), 2, "custom provider must execute and finish");
        assert!(
            requests[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool" && message["content"] == "dynamic_alias")
        );
        let logs = String::from_utf8_lossy(&sink.0.lock()).to_string();
        assert!(
            logs.contains("delegate resolved child compaction window"),
            "the sensitive logging callsite must actually execute: {logs}"
        );
        assert!(logs.contains("custom:http://[redacted]@127.0.0.1:"), "{logs}");
        assert!(!logs.contains("DELEGATE-SECRET-pw"), "{logs}");
    }

    /// The `*`-inheriting child owns `capability_search`, so the inherited
    /// deferred `echo_tool` stays behind search (R28): the child searches,
    /// then calls the tool it found, then finishes.
    #[tokio::test]
    async fn agentic_mode_inherits_parent_tools_only_with_explicit_wildcard() {
        let executions = Arc::new(AtomicUsize::new(0));
        let (result, requests) = DELEGATE_CHILD_SEARCHES_FIRST
            .scope(
                (),
                run_configured_delegate_fixture(
                    vec!["*".to_owned()],
                    vec![
                        Arc::new(EchoTool {
                            executions: Arc::clone(&executions),
                        }),
                        Arc::new(crate::tools::capability::search_tool::CapabilitySearchTool),
                        Arc::new(DelegateTool::new(HashMap::new(), test_security())),
                    ],
                    "echo_tool",
                    json!({"value":"fixture"}),
                    None,
                    false,
                    None,
                ),
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("(ollama/prx-local-model, agentic)"));
        assert!(result.output.ends_with("\ndone"));
        assert_eq!(requests.len(), 3, "search, call, finish");
        let offered = |request: &serde_json::Value| -> Vec<String> {
            request["tools"]
                .as_array()
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let first = offered(&requests[0]);
        assert!(first.iter().any(|name| name == "capability_search"), "{first:?}");
        assert!(
            first.iter().all(|name| name != "echo_tool"),
            "the inherited deferred tool is not declared before search: {first:?}"
        );
        assert_eq!(
            executions.load(Ordering::SeqCst),
            1,
            "the delegated tool must actually run, not be refused"
        );
    }

    #[tokio::test]
    async fn agentic_mode_rejects_wildcard_mixed_with_named_tools() {
        let config = agentic_config(vec!["*".to_string(), "echo_tool".to_string()]);
        let tool = DelegateTool::new(HashMap::new(), test_security())
            .with_parent_tools(Arc::new(vec![Arc::new(EchoTool::default())]));
        let provider = OneToolThenFinalProvider;

        let result = tool
            .boxed_delegate_loop(
                "agentic",
                &config,
                "openrouter",
                "model-test",
                &provider,
                "run",
                0.2,
                None,
                None,
            )
            .await
            .unwrap();

        assert!(!result.result.success);
        assert!(
            result
                .result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("must use allowed_tools=['*'] exclusively")
        );
    }

    #[tokio::test]
    async fn execute_agentic_runs_tool_call_loop_with_filtered_tools() {
        let executions = Arc::new(AtomicUsize::new(0));
        let (result, requests) = run_configured_delegate_fixture(
            vec!["echo_tool".to_owned()],
            vec![
                Arc::new(EchoTool {
                    executions: Arc::clone(&executions),
                }),
                Arc::new(DelegateTool::new(HashMap::new(), test_security())),
            ],
            "echo_tool",
            json!({"value":"fixture"}),
            None,
            false,
            None,
        )
        .await;
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("(ollama/prx-local-model, agentic)"));
        assert!(result.output.ends_with("\ndone"));
        assert_eq!(requests.len(), 2);
        assert_eq!(
            executions.load(Ordering::SeqCst),
            1,
            "the delegated tool must actually run, not be refused"
        );
    }

    #[test]
    fn delegate_inline_model_override_drives_child_compaction_resolution() {
        let mut config = crate::config::Config::default();
        config.router.models = vec![
            router_model("openrouter", "config-model", 1_000_000),
            router_model("openrouter", "inline-small", 200_000),
        ];

        let resolved = resolve_pinned_child_compaction(&config, "openrouter", "inline-small");

        assert_eq!(resolved.config.max_context_tokens, 200_000);
        assert_eq!(resolved.selected_model, "inline-small");
        assert_ne!(resolved.config.max_context_tokens, 1_000_000);
    }

    #[test]
    fn delegate_child_compaction_uses_the_pinned_generation_catalog_after_reload() {
        let mut startup = crate::config::Config::default();
        startup.providers.models.push(
            toml::from_str("provider = \"openrouter\"\nmodel = \"vendor/reload-model\"\ncontext_window = 200000\n")
                .expect("test: startup declaration"),
        );
        let mut reloaded = startup.clone();
        reloaded.providers.models.clear();
        reloaded.providers.models.push(
            toml::from_str("provider = \"openrouter\"\nmodel = \"vendor/reload-model\"\ncontext_window = 64000\n")
                .expect("test: reloaded declaration"),
        );

        let before = resolve_pinned_child_compaction(&startup, "openrouter", "vendor/reload-model");
        let after = resolve_pinned_child_compaction(&reloaded, "openrouter", "vendor/reload-model");

        assert_eq!(before.config.max_context_tokens, 200_000);
        assert_eq!(after.config.max_context_tokens, 64_000);
        assert_eq!(
            after.max_context_source,
            crate::providers::context_window::ContextWindowSource::Catalog
        );
    }

    #[tokio::test]
    async fn execute_agentic_rejects_delegate_as_ineligible() {
        let config = agentic_config(vec!["delegate".to_string()]);
        let tool = DelegateTool::new(HashMap::new(), test_security()).with_parent_tools(Arc::new(vec![Arc::new(
            DelegateTool::new(HashMap::new(), test_security()),
        )]));

        let provider = OneToolThenFinalProvider;
        let result = tool
            .boxed_delegate_loop(
                "agentic",
                &config,
                "openrouter",
                "model-test",
                &provider,
                "run",
                0.2,
                None,
                None,
            )
            .await
            .unwrap();

        assert!(!result.result.success);
        assert!(
            result
                .result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("unknown or ineligible tools: delegate")
        );
    }

    #[tokio::test]
    async fn execute_agentic_propagates_provider_errors() {
        let executions = Arc::new(AtomicUsize::new(0));
        let (result, requests) = run_configured_delegate_fixture(
            vec!["echo_tool".to_owned()],
            vec![Arc::new(EchoTool {
                executions: Arc::clone(&executions),
            })],
            "echo_tool",
            json!({"value":"fixture"}),
            Some("provider boom"),
            false,
            None,
        )
        .await;
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("provider boom"));
        assert_eq!(requests.len(), 1, "the admitted child must reach the failing provider");
        assert_eq!(
            executions.load(Ordering::SeqCst),
            0,
            "a provider failure must not execute the child tool"
        );
    }

    fn single_agent(name: &str, provider: &str, model: &str) -> HashMap<String, DelegateAgentConfig> {
        let mut agents = HashMap::new();
        agents.insert(
            name.to_string(),
            DelegateAgentConfig {
                messaging: crate::config::schema::AgentMessaging::default(),
                approval_wait_secs: 600,
                limits: crate::config::AgentLimits::default(),
                provider: provider.to_string(),
                model: model.to_string(),
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
        agents
    }

    #[test]
    fn delegate_fallback_credential_is_scoped_to_its_provider() {
        let mut child = single_agent("tester", "kimi-code", "k3")
            .remove("tester")
            .expect("test agent");
        let tool = DelegateTool::new(HashMap::new(), test_security()).with_root_connection(RootConnection {
            provider: Some("custom:http://127.0.0.1:18083/v1".to_string()),
            key: Some("root-key".to_string()),
            api_url: None,
        });

        assert_eq!(tool.child_connection(&child, "kimi-code").api_key, None);
        assert_eq!(
            tool.child_connection(&child, "CUSTOM:http://127.0.0.1:18083/v1")
                .api_key,
            Some("root-key")
        );

        child.api_key = Some("child-key".to_string());
        assert_eq!(tool.child_connection(&child, "kimi-code").api_key, Some("child-key"));
    }

    #[test]
    fn schema_exposes_inline_model_and_provider() {
        let tool = DelegateTool::new(sample_agents(), test_security());
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["model"].is_object());
        assert!(schema["properties"]["provider"].is_object());
        // Inline overrides must remain optional for backward compatibility.
        let required = schema["required"].as_array().unwrap();
        assert!(!required.contains(&json!("model")));
        assert!(!required.contains(&json!("provider")));
    }

    #[tokio::test]
    async fn delegate_inline_model_overrides_agent_config() {
        // Agent config model is "config-model"; inline override pins "pinned-model".
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.autonomy.level = AutonomyLevel::Full;
        config.agents = single_agent("tester", "mock", "config-model");
        let tool = DelegateTool::new(
            config.agents.clone(),
            Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path())),
        );
        let result = dispatch_admitted_delegate(
            config,
            tool,
            json!({"agent":"tester", "prompt":"hello", "model":"pinned-model"}),
        )
        .await;

        assert!(result.success, "mock provider should succeed: {:?}", result.error);
        // The formatted output records the effective (overridden) model.
        assert!(
            result.output.contains("pinned-model"),
            "output should reflect inline model: {}",
            result.output
        );
        assert!(
            !result.output.contains("config-model"),
            "inline model must replace config model: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn delegate_inline_provider_overrides_agent_config() {
        // Agent config provider is valid ("mock"); inline override forces an
        // invalid provider so we can observe that the override path is taken
        // (provider creation fails naming the override, not the config value).
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.autonomy.level = AutonomyLevel::Full;
        config.agents = single_agent("tester", "mock", "m");
        let tool = DelegateTool::new(
            config.agents.clone(),
            Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path())),
        );
        let result = dispatch_admitted_delegate(
            config,
            tool,
            json!({"agent":"tester", "prompt":"hello", "provider":"totally-invalid-provider"}),
        )
        .await;

        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or("");
        assert!(
            err.contains("totally-invalid-provider"),
            "error should name the inline provider override: {err}"
        );
    }

    #[tokio::test]
    async fn delegate_without_inline_uses_agent_config() {
        // No inline model/provider → backward-compatible: agent config wins.
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.autonomy.level = AutonomyLevel::Full;
        config.agents = single_agent("tester", "mock", "config-model");
        let tool = DelegateTool::new(
            config.agents.clone(),
            Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path())),
        );
        let result = dispatch_admitted_delegate(config, tool, json!({"agent":"tester", "prompt":"hello"})).await;

        assert!(result.success, "mock provider should succeed: {:?}", result.error);
        assert!(
            result.output.contains("mock/config-model"),
            "output should reflect agent config provider/model: {}",
            result.output
        );
    }

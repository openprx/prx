#[cfg(test)]
mod tests {
    mod console_recovery;
    mod targeting;


    use super::*;
    use crate::config::Config;
    use crate::gateway::{GatewayRateLimiter, IdempotencyStore};
    use crate::hooks::HookManager;
    use crate::memory::{ConversationTurn, Memory, MemoryCategory, MemoryEntry};
    use crate::providers::{ChatRequest, ChatResponse, Provider};
    use crate::security::pairing::PairingGuard;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::broadcast;

    pub(super) async fn drive_search_console_entry(config: Config, input: &str) -> anyhow::Result<()> {
        use crate::agent::turn_tests::reachability::{boxed_root_probe, provider, registry};
        let workspace = config.workspace_dir.clone();
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(&workspace)?);
        let provider: Arc<dyn Provider> = Arc::from(provider(&config)?);
        let mut state = test_app_state_with_config(config.clone(), provider);
        state.tools_registry = registry(&config, Arc::clone(&memory));
        state.mem = memory;
        state.hooks = Arc::new(HookManager::new(workspace.clone()));
        let cleanup =
            crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())?;
        cleanup
            .run(boxed_root_probe(|| async move {
                let envelope = console_runtime_envelope_for_workspace(
                    workspace.to_string_lossy().to_string(),
                    "prx_search_console",
                    "console",
                    "prx_user",
                );
                let (runtime, _, _, proof) = console_test_source(&mut state, envelope, input).await;
                let _ = run_console_message_job(
                    runtime,
                    state,
                    input.to_owned(),
                    Vec::new(),
                    ConsoleTurnSource::Message(proof),
                    None,
                )
                .await
                .map_err(anyhow::Error::msg)?;
                Ok(())
            }))
            .await?;
        Ok(())
    }

    pub(super) async fn drive_disabled_engine_console_entry(workspace: &std::path::Path) {
        let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
            .expect("test: console engine-off cleanup");
        cleanup
            .run(Box::pin(async {
                let mut config = Config::default();
                config.workspace_dir = workspace.to_owned();
                config.config_path = workspace.join("config.toml");
                config.default_provider = Some("ollama".to_owned());
                config.default_model = Some("test-model".to_owned());
                config.providers.models.push(
                    toml::from_str(
                        "provider = \"ollama\"\nmodel = \"test-model\"\noperations = [\"chat\"]\nnative_tools = false\n",
                    )
                    .expect("test: console model declaration"),
                );
                config.cost.prices.insert(
                    "ollama/test-model".to_owned(),
                    crate::config::schema::ModelPricing {
                        input: 1.0,
                        output: 1.0,
                        cache_write: 1.0,
                        cache_read: 1.0,
                    },
                );
                let captured = Arc::new(CapturingProvider::default());
                let provider: Arc<dyn Provider> = captured.clone();
                let mut state = test_app_state_with_config(config, provider);
                let envelope = console_runtime_envelope_for_workspace(
                    workspace.to_string_lossy().to_string(),
                    "console-engine-off",
                    "console",
                    "prx_user",
                );
                let (runtime, _, _, proof) = console_test_source(&mut state, envelope, "hello").await;
                run_console_message_job(
                    runtime,
                    state,
                    "hello".to_string(),
                    Vec::new(),
                    ConsoleTurnSource::Message(proof),
                    None,
                )
                .await.map_err(anyhow::Error::msg)?;
                assert_eq!(captured.requests.lock().len(), 1, "test: console provider called once");
                Ok::<(), anyhow::Error>(())
            }))
            .await
            .expect("test: console engine-off entry");
    }

    #[test]
    fn console_error_kind_never_carries_error_text() {
        const MARKER: &str = "PRIVATE-STORED-MARKER";
        let decode = serde_json::from_str::<u32>(&format!("\"{MARKER}\""))
            .expect_err("test: a string is not a valid integer capsule field");
        assert!(
            decode.to_string().contains(MARKER),
            "the fixture must reproduce a decode error that quotes stored text"
        );
        let error = anyhow::Error::from(decode).context(format!("recovery scan failed for {MARKER}"));
        assert!(
            format!("{error:#}").contains(MARKER),
            "the unrestricted chain carries the marker"
        );
        let kind = console_error_kind(&error);
        assert_eq!(
            kind, "decode",
            "a serde failure anywhere in the chain is a decode failure"
        );
        assert!(!kind.contains(MARKER), "the logged category must not carry error text");

        let untyped = anyhow::anyhow!("untyped failure quoting {MARKER}");
        assert_eq!(
            console_error_kind(&untyped),
            "other",
            "an untyped error is reported by a static category only"
        );
        let conflict = anyhow::Error::from(crate::runtime::ingress_recovery::AlreadyAccepted).context(MARKER);
        assert_eq!(
            console_error_kind(&conflict),
            "conflict",
            "a typed conflict keeps its category under added context"
        );
        let io = anyhow::Error::from(std::io::Error::other(MARKER));
        assert_eq!(console_error_kind(&io), "io", "an I/O failure is reported as io");
    }

    #[test]
    fn console_runtime_envelope_preserves_session_channel_and_sender() {
        let envelope = console_runtime_envelope_for_workspace("workspace", "signal_alice", "signal", "alice");
        let scope = envelope.message_scope();

        assert_eq!(scope.source, "console");
        assert_eq!(scope.session_key.as_deref(), Some("signal_alice"));
        assert_eq!(scope.channel.as_deref(), Some("signal"));
        assert_eq!(scope.sender.as_deref(), Some("alice"));
    }

    // D4 C5: the console external session_id is NOT canonicalized. The durable
    // session_key stays the raw external id (path/list contract value), and no
    // legacy_session_key is carried (console persistence/recall stay single-key on
    // the external id). This guards against accidentally migrating the console
    // durable key the way the gateway *fabric* path was migrated in C4.
    #[test]
    fn d4_console_session_id_is_kept_legacy_not_canonicalized() {
        let session_id = "signal_alice";
        let envelope = console_runtime_envelope_for_workspace("workspace", session_id, "signal", "alice");

        // Durable write key == external id (no recipient-aware canonical).
        assert_eq!(envelope.session_key, session_id);
        assert_eq!(envelope.message_scope().session_key.as_deref(), Some(session_id));
        // No legacy key carried -> single-key recall on the external id.
        let principal = envelope.memory_principal();
        assert_eq!(principal.session_key.as_deref(), Some(session_id));
        assert_eq!(principal.legacy_session_key, None);
        assert_eq!(principal.session_key_candidates(), vec![session_id.to_string()]);
        // The canonical derivation differs from the external id; C5 deliberately
        // does NOT adopt it for the durable key (would break the path/list id).
        assert_ne!(envelope.canonical_session_key(), session_id);
        assert_eq!(envelope.canonical_session_key(), "console:signal:alice:-");
    }

    #[derive(Default)]
    struct TestMemory;

    #[async_trait]
    impl Memory for TestMemory {
        fn name(&self) -> &str {
            "test"
        }

        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct CapturingProvider {
        requests: parking_lot::Mutex<Vec<Vec<ChatMessage>>>,
    }

    #[async_trait]
    impl Provider for CapturingProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            anyhow::bail!("console runtime should use structured Provider::chat");
        }

        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            self.requests.lock().push(request.messages.to_vec());
            Ok(ChatResponse {
                text: Some("console-runtime-ok".to_string()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }
    }

    struct TestConfigParticipant;
    struct TestPreparedConfig;

    impl crate::config::ConfigGenerationParticipant for TestConfigParticipant {
        fn name(&self) -> &'static str {
            "gateway_sessions_test_runtime"
        }

        fn supports_rebuild_field(&self, _field: &str) -> bool {
            true
        }

        fn supports_controlled_restart_field(&self, _field: &str) -> bool {
            true
        }

        fn prepares_for_field(&self, _field: &str) -> bool {
            true
        }

        fn prepare(
            &self,
            _generation: Arc<crate::config::ConfigGeneration>,
            _changed_fields: &[String],
        ) -> anyhow::Result<Box<dyn crate::config::PreparedConfigGeneration>> {
            Ok(Box::new(TestPreparedConfig))
        }
    }

    impl crate::config::PreparedConfigGeneration for TestPreparedConfig {
        fn commit(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn rollback(&mut self) {}
    }

    fn test_app_state(mut config: Config, provider: Arc<dyn Provider>) -> AppState {
        config.default_provider.get_or_insert_with(|| "ollama".to_owned());
        config.default_model.get_or_insert_with(|| "test-model".to_owned());
        test_app_state_with_config(config, provider)
    }

    fn test_app_state_with_config(config: Config, provider: Arc<dyn Provider>) -> AppState {
        let model = config.default_model.clone().expect("fixture config model");
        static PARTICIPANT: std::sync::OnceLock<Arc<dyn crate::config::ConfigGenerationParticipant>> =
            std::sync::OnceLock::new();
        let config = crate::config::new_shared(config);
        config.register_participant(PARTICIPANT.get_or_init(|| Arc::new(TestConfigParticipant)));
        AppState {
            rf_streams: Default::default(),
            rf_approvals: Default::default(),
            config,
            provider,
            model,
            temperature: 0.0,
            mem: Arc::new(TestMemory),
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            turn_runtime: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: None,
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_mins(5), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_runtime: None,
        }
    }

    #[tokio::test]
    async fn session_creation_route_binds_owner_and_creates_no_message() {
        use tower::ServiceExt;
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_owned();
        config.autonomy.level = crate::security::AutonomyLevel::Full;
        let mut state = test_app_state(config.clone(), Arc::new(CapturingProvider::default()));
        let memory = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
        state.mem = memory.clone();
        let router = super::super::router(state.clone()).with_state(state.clone());
        let request = |body: &'static str| {
            axum::http::Request::builder()
                .method("POST")
                .uri("/sessions")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
        };
        let response = router.clone().oneshot(request("{}")).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        let id = body.get("session_id").unwrap().as_str().unwrap();
        let stored = memory.get_conversation_session(id).await.unwrap().unwrap();
        assert_eq!(stored.message_count, 0);
        assert_eq!(stored.channel, "console");
        assert_eq!(stored.sender, "api:local");
        assert_eq!(stored.last_message_preview, "");
        assert!(memory.create_conversation_session(id, "api:other").await.is_err());
        assert_eq!(
            memory.get_conversation_session(id).await.unwrap().unwrap().sender,
            "api:local"
        );
        assert_eq!(
            router
                .oneshot(request(r#"{"owner_id":"api:other"}"#))
                .await
                .unwrap()
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        let mut denied = test_app_state(config, Arc::new(CapturingProvider::default()));
        denied.mem = memory.clone();
        let response = super::super::router(denied.clone())
            .with_state(denied)
            .oneshot(request("{}"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(memory.list_conversation_sessions(100, 0, None).await.unwrap().len(), 1);
    }

    fn console_provider_fixture_config(config: &mut Config, workspace: &std::path::Path, model: &str, endpoint: &str) {
        config.workspace_dir = workspace.to_owned();
        config.config_path = workspace.join("config.toml");
        config.default_provider = Some("ollama".to_owned());
        config.default_model = Some(model.to_owned());
        config.cost.prices.insert(
            format!("ollama/{model}"),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            },
        );
        assert!(matches!(crate::llm::route_decision::usage_cost_resolution(
            "ollama", model, 1, 0, 0, 0, &config.cost,
        ), crate::llm::route_decision::UsagePrice::Priced(value) if value > 0.0));
        let native_tools = model == CONSOLE_WINDOW_MODEL;
        config.providers.models.push(
            toml::from_str(&format!(
                "provider = \"ollama\"\nmodel = {model:?}\noperations = [\"chat\"]\nnative_tools = {native_tools}\n"
            ))
            .unwrap(),
        );
        let selected = crate::providers::selection::check(
            config,
            "ollama",
            model,
            &if native_tools {
                crate::providers::catalog::Requirement::chat().with(crate::providers::catalog::Feature::NativeTools)
            } else {
                crate::providers::catalog::Requirement::chat()
            },
            crate::providers::catalog::UnknownPolicy::Invocation,
        )
        .unwrap();
        assert_eq!(
            selected.verification(),
            &crate::providers::catalog::Verification::Verified
        );
        crate::agent::agent::native_decision_fixture::configure(config, endpoint).unwrap();
    }

    async fn console_test_source(
        state: &mut AppState,
        envelope: RuntimeEnvelope,
        input: &str,
    ) -> (
        Arc<crate::gateway::TurnRuntimeGeneration>,
        RuntimeEnvelope,
        String,
        crate::gateway::turn_source::GatewayTurnProof,
    ) {
        if state.mem.name() == "test" {
            state.mem =
                Arc::new(crate::memory::SqliteMemory::new(&state.config.pin().effective.workspace_dir).unwrap());
        }
        let runtime = console_test_runtime(state);
        let principal = super::super::ApiPrincipal::for_tests();
        let envelope = envelope
            .with_run_id(uuid::Uuid::new_v4().to_string())
            .with_config_generation(&runtime.config_generation)
            .with_owner_id(principal.owner_id.clone())
            .with_owner_isolation();
        let event_id = append_console_turn(state, &envelope, "user", input, true, false)
            .await
            .unwrap();
        let proof = crate::gateway::turn_source::GatewaySourceReceipt::console_proof(
            state.mem.as_ref(),
            &runtime,
            &principal,
            &envelope,
            input,
            &event_id,
        )
        .await
        .unwrap();
        (runtime, proof.envelope().clone(), event_id, proof)
    }

    /// The Console turn runtime of `state`: its pinned generation with the
    /// production provider binding and a fresh publication slot.
    fn console_test_runtime(state: &AppState) -> Arc<crate::gateway::TurnRuntimeGeneration> {
        let mut runtime = state.pin_turn_runtime();
        let config = &runtime.config_generation.effective;
        let provider_name = config.default_provider.as_deref().unwrap();
        let catalog = crate::providers::selection::catalog_for(config);
        let resolved = crate::providers::selection::check_with(
            config,
            &catalog,
            provider_name,
            &runtime.model,
            crate::providers::selection::credential_for(config, provider_name),
            &crate::providers::catalog::Requirement::chat(),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )
        .unwrap();
        let binding = crate::gateway::turn_source::GatewayProviderBinding::new(runtime.provider.clone(), resolved);
        let security = Arc::new(crate::security::SecurityPolicy::from_config(
            &config.autonomy,
            &config.workspace_dir,
        ));
        let mutable = Arc::get_mut(&mut runtime).unwrap();
        mutable.provider_binding = Some(Arc::new(binding));
        mutable.publication_slot =
            Arc::new(crate::gateway::turn_source::publication_slot::GatewayPublicationSlot::new(security, 0));
        runtime
    }

    #[tokio::test]
    async fn console_runtime_turn_uses_shared_tool_loop_history() {
        let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
            .await
            .unwrap();
        let endpoint = driver.endpoint().to_owned();
        driver
            .run(async move {
                let tmp = tempfile::tempdir().unwrap();
                let cleanup =
                    crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                        .unwrap();
                cleanup
                    .run(Box::pin(async {
                        let mut config = Config::default();
                        console_provider_fixture_config(&mut config, tmp.path(), "test-model", &endpoint);
                        let provider_impl = Arc::new(CapturingProvider::default());
                        let mut state = test_app_state_with_config(config, provider_impl.clone());
                        let envelope = console_runtime_envelope_for_workspace(
                            tmp.path().to_string_lossy().to_string(),
                            "console-session",
                            "console",
                            "prx_user",
                        );
                        let previous_turns = vec![ConversationTurn {
                            id: 1,
                            session_key: "console-session".to_string(),
                            role: "assistant".to_string(),
                            content: "previous answer".to_string(),
                            timestamp: "2026-05-27T00:00:00Z".to_string(),
                            message_id: Some("previous-event".to_string()),
                        }];

                        let (runtime, _, _, proof) =
                            console_test_source(&mut state, envelope, "current question").await;
                        let service = runtime
                            .publication_slot
                            .publish(&runtime, &state)
                            .await
                            .unwrap()
                            .clone();
                        let cancellation = tokio_util::sync::CancellationToken::new();
                        let context = service
                            .open_issued_turn_context(
                                &proof,
                                crate::runtime::turn_context::AgentTurnKind::SingleShot,
                                &cancellation,
                            )
                            .await
                            .unwrap();
                        let decision = ConsoleDecisionTurn {
                            service: service.clone(),
                            context,
                            advice: None,
                            proof: &proof,
                            cancellation,
                            provider_trace: None,
                        };
                        assert!(
                            run_console_runtime_turn(
                                runtime.clone(),
                                &state,
                                "different unadmitted input",
                                Vec::new(),
                                &decision,
                                None,
                            )
                            .await
                            .is_err()
                        );
                        assert!(
                            provider_impl.requests.lock().is_empty(),
                            "source mismatch must fail before provider transport"
                        );
                        let turn = run_console_runtime_turn(
                            runtime,
                            &state,
                            "current question",
                            previous_turns,
                            &decision,
                            None,
                        )
                        .await
                        .unwrap();

                        service
                            .shutdown(std::time::Instant::now() + Duration::from_secs(2))
                            .await;
                        assert_eq!(turn.reply, "console-runtime-ok");
                        let requests = provider_impl.requests.lock();
                        assert_eq!(requests.len(), 1);
                        let history = requests.first().expect("request should be recorded");
                        assert_eq!(history.first().map(|m| m.role.as_str()), Some("system"));
                        assert!(
                            history
                                .iter()
                                .any(|m| m.role == "assistant" && m.content == "previous answer")
                        );
                        assert_eq!(history.last().map(|m| m.role.as_str()), Some("user"));
                        assert!(history.last().is_some_and(|m| m.content.contains("current question")));
                        Ok::<(), anyhow::Error>(())
                    }))
                    .await
                    .expect("actual fixture decision transport and cleanup ownership");
            })
            .await;
    }

    /// A plain Console message uses the default enabled builtin decision plane
    /// and reaches the serving provider through the same admitted runtime as a
    /// production Console request.
    #[tokio::test]
    async fn console_plain_message_reaches_the_provider_with_default_causal_config() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
            .expect("test: Console cleanup owner");
        cleanup
            .run(Box::pin(async {
                let mut config = Config::default();
                config.workspace_dir = tmp.path().to_owned();
                config.config_path = tmp.path().join("config.toml");
                config.default_provider = Some("ollama".to_owned());
                config.default_model = Some("test-model".to_owned());
                config.providers.models.push(
                    toml::from_str(
                        "provider = \"ollama\"\nmodel = \"test-model\"\noperations = [\"chat\"]\nnative_tools = false\n",
                    )
                    .expect("test: Console model declaration"),
                );
                config.cost.prices.insert(
                    "ollama/test-model".to_owned(),
                    crate::config::schema::ModelPricing {
                        input: 1.0,
                        output: 1.0,
                        cache_write: 1.0,
                        cache_read: 1.0,
                    },
                );
                assert_eq!(
                    config.decision_engine,
                    crate::config::schema::decision_engine::DecisionEngineConfig::default(),
                    "the Console proof must retain the default decision engine configuration"
                );

                let provider_impl = Arc::new(CapturingProvider::default());
                let mut state = test_app_state_with_config(config, provider_impl.clone());
                let envelope = console_runtime_envelope_for_workspace(
                    tmp.path().to_string_lossy().to_string(),
                    "console-plain-session",
                    "console",
                    "prx_user",
                );
                let input = "hello";
                let (runtime, _, _, proof) = console_test_source(&mut state, envelope, input).await;
                let service = runtime.publication_slot.publish(&runtime, &state).await?.clone();
                let cancellation = tokio_util::sync::CancellationToken::new();
                let context = service
                    .open_issued_turn_context(
                        &proof,
                        crate::runtime::turn_context::AgentTurnKind::SingleShot,
                        &cancellation,
                    )
                    .await?;
                let decision = ConsoleDecisionTurn {
                    service: Arc::clone(&service),
                    context,
                    advice: None,
                    proof: &proof,
                    cancellation,
                    provider_trace: None,
                };
                let turn = run_console_runtime_turn(runtime, &state, input, Vec::new(), &decision, None).await?;
                service
                    .shutdown(std::time::Instant::now() + Duration::from_secs(2))
                    .await;

                assert_eq!(turn.reply, "console-runtime-ok");
                assert_eq!(provider_impl.requests.lock().len(), 1);
                Ok::<(), anyhow::Error>(())
            }))
            .await
            .expect("test: default Console turn and cleanup complete");
    }

    // ── R5 / OQ-23: the Console turn budgets on the model's effective window ──

    const CONSOLE_BULKY_TOOL: &str = "file_read";
    const CONSOLE_BULKY_FILE: &str = "prx-console-window.txt";
    const CONSOLE_BULKY_MARKER: &str = "CONSOLE-BULKY";
    const CONSOLE_WINDOW_MODEL: &str = "narrow-window-model";
    const CONSOLE_WINDOW_PROVIDER: &str = "ollama";
    const CONSOLE_ROUTER_WINDOW: usize = 33_200;

    /// A read-only tool whose output overruns the narrow fixture window but
    /// fits the 128,000-token base window and the unbudgeted inline cap.
    struct ConsoleBulkyTool {
        inner: crate::tools::file_read::FileReadTool,
        executions: Arc<std::sync::atomic::AtomicUsize>,
        expected: String,
    }

    #[async_trait]
    impl crate::tools::Tool for ConsoleBulkyTool {
        fn name(&self) -> &str {
            self.inner.name()
        }
        fn description(&self) -> &str {
            self.inner.description()
        }
        fn parameters_schema(&self) -> serde_json::Value {
            self.inner.parameters_schema()
        }
        fn tier(&self) -> crate::tools::traits::ToolTier {
            self.inner.tier()
        }
        fn categories(&self) -> &'static [crate::tools::traits::ToolCategory] {
            self.inner.categories()
        }
        fn supports_owner_filesystem(&self) -> bool {
            self.inner.supports_owner_filesystem()
        }
        async fn execute_with_cancellation(
            &self,
            args: serde_json::Value,
            cancellation: Option<tokio_util::sync::CancellationToken>,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            self.executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let result = self.inner.execute_with_cancellation(args, cancellation).await?;
            assert!(result.success, "actual private FileRead must succeed: {result:?}");
            assert_eq!(
                result.output, self.expected,
                "the full physical file must reach budgeting"
            );
            Ok(result)
        }
        async fn execute(&self, args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
            self.execute_with_cancellation(args, None).await
        }
    }

    /// Native-tool spy: asks for the bulky tool on its first call, answers in
    /// text afterwards, and records every request it serves.
    #[derive(Default)]
    struct ToolRoundSpyProvider {
        requests: parking_lot::Mutex<Vec<Vec<ChatMessage>>>,
    }

    #[async_trait]
    impl Provider for ToolRoundSpyProvider {
        fn capabilities(&self) -> crate::providers::ProviderCapabilities {
            crate::providers::ProviderCapabilities {
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
            Ok("summary".into())
        }

        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            let call_index = {
                let mut requests = self.requests.lock();
                requests.push(request.messages.to_vec());
                requests.len()
            };
            let tool_calls = if call_index == 1 {
                vec![crate::providers::ToolCall {
                    id: "call-console-bulky-1".into(),
                    name: CONSOLE_BULKY_TOOL.into(),
                    arguments: serde_json::json!({"path": CONSOLE_BULKY_FILE}).to_string(),
                }]
            } else {
                Vec::new()
            };
            Ok(ChatResponse {
                text: tool_calls.is_empty().then(|| "done".to_string()),
                tool_calls,
                reasoning_content: None,
            })
        }
    }

    /// Base config: the default 128,000-token window (not set explicitly), and
    /// a `[[router.models]]` entry that gives the served model a narrow window.
    /// Only the effective resolution sees the router entry. The fixture window
    /// is large enough for the sealed decision request's existing reservation;
    /// the paired payload remains larger than the main-turn share.
    fn narrow_router_window_config(workspace: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.workspace_dir = workspace.to_path_buf();
        config.default_provider = Some(CONSOLE_WINDOW_PROVIDER.to_string());
        config.default_model = Some(CONSOLE_WINDOW_MODEL.to_string());
        config.agent.compaction = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Off,
            ..crate::config::AgentCompactionConfig::default()
        };
        config.router.models = vec![crate::config::RouterModelConfig {
            model_id: CONSOLE_WINDOW_MODEL.to_string(),
            provider: CONSOLE_WINDOW_PROVIDER.to_string(),
            cost_per_million_tokens: 1.0,
            max_context: CONSOLE_ROUTER_WINDOW,
            reserved_output_tokens: Some(256),
            latency_ms: 1_000,
            categories: vec!["conversation".to_string()],
            elo_rating: 1_000.0,
        }];
        config.tool_tiering.always_include = vec![CONSOLE_BULKY_TOOL.to_string()];
        config
    }

    /// A Console turn budgets on the served model's effective window, the one
    /// the CLI, TUI and webhook budget on (OQ-23 / R5).
    ///
    /// The base config keeps the 128,000-token default, so a turn budgeted on
    /// the base window forwards the whole token-dense tool payload to the
    /// follow-up request. The router entry gives the served model a narrower
    /// window; budgeted on that, the follow-up must fit it.
    ///
    /// MUTATION GUARD: pass `Some(&config_snapshot.agent.compaction)` to the
    /// loop again and the follow-up request carries the whole payload.
    #[tokio::test]
    async fn console_turn_budgets_on_the_served_models_effective_window() {
        let driver = crate::agent::agent::native_decision_fixture::Driver::start("filesystem")
            .await
            .unwrap();
        let endpoint = driver.endpoint().to_owned();
        driver
            .run(async move {
                let tmp = tempfile::tempdir().expect("test: tempdir");
                let cleanup =
                    crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                        .unwrap();
                cleanup.run(Box::pin(async {
                let workspace = tmp.path().join("workspace");
                let private = tmp.path().join("private");
                std::fs::create_dir_all(&workspace).unwrap();
                std::fs::create_dir_all(&private).unwrap();
                let mut config = narrow_router_window_config(&workspace);
                console_provider_fixture_config(&mut config, &workspace, CONSOLE_WINDOW_MODEL, &endpoint);
                config.config_path = private.join("config.toml");
                let provider_impl = Arc::new(ToolRoundSpyProvider::default());
                // Keep the fixture's existing autonomy; owner isolation still gates
                // the actual registered private-filesystem backend independently.
                config.autonomy.level = crate::security::AutonomyLevel::Full;
                let mut state = test_app_state_with_config(config, provider_impl.clone());
                state.model = CONSOLE_WINDOW_MODEL.into();
                let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let mut payload = String::with_capacity(47_000);
                let mut index = 0usize;
                while payload.len() < 47_000 {
                    use std::fmt::Write as _;
                    let _ = write!(payload, "{CONSOLE_BULKY_MARKER}-{index}-갇걇겇곇괇굇궇귇긇깇꺇껇꼇꽇꾇꿇뀇끇낇냇넇녇놇뇇 ");
                    index += 1;
                }
                assert!(payload.len() < 48_000, "fixture must remain below the native inline cap");
                assert!(
                    payload.matches(CONSOLE_BULKY_MARKER).count() > 500,
                    "the base-window mutation must retain enough payload segments to fail"
                );
                assert!(
                    crate::tokens::count::count_tokens(&payload) > 31_000,
                    "the paired payload must overrun the narrow main-turn window"
                );
                let principal = super::super::ApiPrincipal::for_tests();
                let filesystem = crate::tools::owner_filesystem::OwnerFilesystem::open(
                    &private, workspace.to_string_lossy().as_ref(), &principal.owner_id, &workspace,
                ).unwrap();
                let target = filesystem.resolve(CONSOLE_BULKY_FILE).unwrap();
                filesystem.write_authorized(&target, payload.as_bytes()).unwrap();
                let security = Arc::new(crate::security::SecurityPolicy::from_config(
                    &state.config.pin().effective.autonomy, &workspace,
                ));
                state.tools_registry = Arc::new(vec![Box::new(ConsoleBulkyTool {
                    inner: crate::tools::file_read::FileReadTool::new(security, false),
                    executions: Arc::clone(&executions), expected: payload,
                })]);
                let envelope = console_runtime_envelope_for_workspace(
                    workspace.to_string_lossy().to_string(),
                    "console-window-session",
                    "console",
                    "prx_user",
                );

                // Tool publication is not selection authority. Name the actual
                // fixture capability in the raw source admitted by the service.
                const INPUT: &str = "file_read prx-console-window.txt: summarise the console bulky probe";
                let (runtime, _, _, proof) =
                    console_test_source(&mut state, envelope, INPUT).await;
                let service = runtime.publication_slot.publish(&runtime, &state).await.unwrap().clone();
                let cancellation = tokio_util::sync::CancellationToken::new();
                let context = service
                    .open_issued_turn_context(
                        &proof,
                        crate::runtime::turn_context::AgentTurnKind::SingleShot,
                        &cancellation,
                    )
                    .await
                    .expect("bounded actual Console decision");
                let decision = ConsoleDecisionTurn {
                    service: service.clone(),
                    context,
                    advice: None,
                    proof: &proof,
                    cancellation,
                    provider_trace: None,
                };
                let turn = run_console_runtime_turn(
                    runtime,
                    &state,
                    INPUT,
                    Vec::new(),
                    &decision,
                    None,
                )
                .await
                .expect("test: console turn completes");
                service
                    .shutdown(std::time::Instant::now() + Duration::from_secs(2))
                    .await;
                assert_eq!(turn.reply, "done");
                assert_eq!(
                    executions.load(std::sync::atomic::Ordering::SeqCst),
                    1,
                    "the full bulky payload must be physically produced once"
                );

                let requests = provider_impl.requests.lock().clone();
                assert_eq!(requests.len(), 2, "one tool round, then the answer");
                let follow_up = requests.get(1).expect("test: the follow-up request was recorded");
                let payload_occurrences: usize = follow_up
                    .iter()
                    .map(|message| message.content.matches(CONSOLE_BULKY_MARKER).count())
                    .sum();
                assert!(
                    payload_occurrences < 500,
                    "the follow-up request must be budgeted on the model's {CONSOLE_ROUTER_WINDOW}-token window, \
             not the 128,000-token base window; found {payload_occurrences} payload segments"
                );
                let follow_up_chars: usize = follow_up.iter().map(|message| message.content.chars().count()).sum();
                assert!(
                    follow_up_chars < CONSOLE_ROUTER_WINDOW * 4,
                    "the follow-up request must fit the effective window; it has {follow_up_chars} characters"
                );
                assert!(
                    follow_up.iter().any(|message| message.role == "user"
                        && message.content.contains("summarise the console bulky probe")),
                    "the turn's own request must survive the budget (D-011)"
                );
                    Ok::<(), anyhow::Error>(())
                })).await.expect("actual fixture decision transport and cleanup ownership");
            })
            .await;
    }

    /// C2b-2: the console session API returns a tool-schema refusal as a
    /// stable code with every refused tool. Before, the job failed with the
    /// aggregate clipped to 200 characters, which cut the tool list.
    #[tokio::test]
    async fn a_console_tool_schema_refusal_returns_a_stable_code_and_the_tools() {
        let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
            .await
            .unwrap();
        let endpoint = driver.endpoint().to_owned();
        driver
            .run(async move {
                let tmp = tempfile::tempdir().unwrap();
                let mut config = Config::default();
                console_provider_fixture_config(&mut config, tmp.path(), "test-model", &endpoint);
                let mut state = test_app_state_with_config(
                    config,
                    Arc::new(crate::providers::tool_schema_refusal_tests::RefusingProvider),
                );
                let envelope = console_runtime_envelope_for_workspace(
                    tmp.path().to_string_lossy().to_string(),
                    "console-session",
                    "console",
                    "prx_user",
                );

                let (runtime, _, _, proof) = console_test_source(&mut state, envelope, "hello").await;
                let output = match run_console_message_job(
                    runtime,
                    state,
                    "hello".to_string(),
                    Vec::new(),
                    ConsoleTurnSource::Message(proof),
                    None,
                )
                .await
                {
                    Ok(output) => output,
                    Err(message) => {
                        panic!("the refusal must be answered with a stable code, not a failed job: {message}")
                    }
                };
                assert_eq!(output.status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
                assert_eq!(
                    output.body.get("error"),
                    Some(&serde_json::json!("tool_schema_refused")),
                    "{}",
                    output.body
                );
                assert_eq!(
                    output.body.get("tools"),
                    Some(&serde_json::json!(["fixture_conditional", "fixture_alternatives"])),
                    "{}",
                    output.body
                );
            })
            .await;
    }

    // ── D-053 / REDACT-TURN: console turns persist no provider secret ──

    /// Answers every structured chat with a reply that echoes a secret.
    struct SecretReplyProvider(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait]
    impl Provider for SecretReplyProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            anyhow::bail!("console runtime should use structured Provider::chat");
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let secret = crate::agent::terminal::persistence_probe::PROBE_SECRET;
            Ok(ChatResponse {
                text: Some(format!("Here is the key {secret} you asked for")),
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }
    }

    /// Fails every structured chat with an error that echoes a secret.
    struct SecretErrorProvider(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait]
    impl Provider for SecretErrorProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            anyhow::bail!("console runtime should use structured Provider::chat");
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let secret = crate::agent::terminal::persistence_probe::PROBE_SECRET;
            anyhow::bail!("upstream rejected key {secret}");
        }
    }

    /// A console state whose memory is a real SQLite store and whose
    /// workspace (cost ledger, skills) is the same tempdir, so everything the
    /// turn persists lands under `tmp`. The user turn is recorded first, as
    /// `post_session_message` does, and its event id returned.
    async fn console_probe_state(
        tmp: &tempfile::TempDir,
        provider: Arc<dyn Provider>,
        session_id: &str,
        endpoint: &str,
    ) -> (
        AppState,
        Arc<crate::gateway::TurnRuntimeGeneration>,
        RuntimeEnvelope,
        String,
        crate::gateway::turn_source::GatewayTurnProof,
    ) {
        let mut config = Config::default();
        console_provider_fixture_config(&mut config, tmp.path(), "test-model", endpoint);
        let mut state = test_app_state_with_config(config, provider);
        state.mem = Arc::new(crate::memory::SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
        let envelope = console_runtime_envelope_for_workspace(
            tmp.path().to_string_lossy().to_string(),
            session_id,
            "console",
            "prx_user",
        );
        let (runtime, envelope, event_id, proof) = console_test_source(&mut state, envelope, "what is my key?").await;
        (state, runtime, envelope, event_id, proof)
    }

    /// D-053 / REDACT-TURN probe: drives `run_console_message_job` through the
    /// completed `finalize_turn` commit and the assistant `append_console_turn`
    /// with a reply that echoes a secret; it must fail before the REDACT-TURN commits.
    #[tokio::test]
    async fn console_persists_no_secret_from_a_reply() {
        let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
            .await
            .unwrap();
        let endpoint = driver.endpoint().to_owned();
        driver
            .run(async move {
                let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let tmp = tempfile::tempdir().expect("test: tempdir");
                let session_id = "console-probe-reply";
                let (state, runtime, envelope, _, proof) = console_probe_state(
                    &tmp,
                    Arc::new(SecretReplyProvider(Arc::clone(&calls))),
                    session_id,
                    &endpoint,
                )
                .await;
                let mem = Arc::clone(&state.mem);
                let principal = envelope.memory_principal();

                // Keep the actual writer/runtime alive through the full file scan.
                let retained_runtime = Arc::clone(&runtime);
                let service = runtime
                    .publication_slot
                    .publish(&runtime, &state)
                    .await
                    .unwrap()
                    .clone();
                let output = run_console_message_job(
                    runtime,
                    state,
                    "what is my key?".to_string(),
                    Vec::new(),
                    ConsoleTurnSource::Message(proof),
                    None,
                )
                .await
                .expect("test: the console turn completes");
                assert_eq!(output.status, axum::http::StatusCode::OK, "{}", output.body);

                let turns = mem
                    .list_conversation_turns(&principal, session_id, 50, 0)
                    .await
                    .expect("test: list the console turns");
                assert!(
                    turns
                        .iter()
                        .any(|turn| turn.role == "assistant" && turn.content.contains("[REDACTED]")),
                    "the reply must be persisted as a redacted assistant turn: {turns:?}"
                );

                assert_eq!(
                    calls.load(std::sync::atomic::Ordering::SeqCst),
                    1,
                    "the controlled provider must actually run once"
                );
                retained_runtime
                    .publication_slot
                    .cleanup
                    .drain(tokio::time::Instant::now() + Duration::from_secs(5))
                    .await
                    .expect("Console cleanup must drain before persisted-file inspection");
                assert!(
                    service.drain_writes(Instant::now() + Duration::from_secs(5)).await,
                    "actual decision writes must drain before persisted-file inspection"
                );
                crate::agent::terminal::persistence_probe::assert_nothing_under_persists_the_probe_secret(tmp.path());
                service.shutdown(Instant::now() + Duration::from_secs(2)).await;
                let retirement = Arc::clone(&retained_runtime.publication_slot.cleanup);
                drop(retained_runtime);
                retirement
                    .drain(tokio::time::Instant::now() + Duration::from_secs(5))
                    .await
                    .expect("Console runtime retirement must drain before scratch removal");
            })
            .await;
    }

    /// D-053 / REDACT-TURN probe: drives `run_console_message_job` into the
    /// failed `finalize_turn` commit of `run_console_runtime_turn` with a provider
    /// error that echoes a secret; it must fail before the REDACT-TURN commits.
    #[tokio::test]
    async fn console_persists_no_secret_from_a_provider_error() {
        let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
            .await
            .unwrap();
        let endpoint = driver.endpoint().to_owned();
        driver
            .run(async move {
                let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let tmp = tempfile::tempdir().expect("test: tempdir");
                let (state, runtime, _, _, proof) = console_probe_state(
                    &tmp,
                    Arc::new(SecretErrorProvider(Arc::clone(&calls))),
                    "console-probe-error",
                    &endpoint,
                )
                .await;

                // Keep the actual writer/runtime alive through the full file scan.
                let retained_runtime = Arc::clone(&runtime);
                let service = runtime
                    .publication_slot
                    .publish(&runtime, &state)
                    .await
                    .unwrap()
                    .clone();
                let result = run_console_message_job(
                    runtime,
                    state,
                    "what is my key?".to_string(),
                    Vec::new(),
                    ConsoleTurnSource::Message(proof),
                    None,
                )
                .await;
                assert!(
                    result.is_err(),
                    "the provider failure must fail the console job, not answer it"
                );

                assert_eq!(
                    calls.load(std::sync::atomic::Ordering::SeqCst),
                    1,
                    "the controlled provider must actually run once"
                );
                retained_runtime
                    .publication_slot
                    .cleanup
                    .drain(tokio::time::Instant::now() + Duration::from_secs(5))
                    .await
                    .expect("Console cleanup must drain before persisted-file inspection");
                assert!(
                    service.drain_writes(Instant::now() + Duration::from_secs(5)).await,
                    "actual decision writes must drain before persisted-file inspection"
                );
                crate::agent::terminal::persistence_probe::assert_nothing_under_persists_the_probe_secret(tmp.path());
                service.shutdown(Instant::now() + Duration::from_secs(2)).await;
                let retirement = Arc::clone(&retained_runtime.publication_slot.cleanup);
                drop(retained_runtime);
                retirement
                    .drain(tokio::time::Instant::now() + Duration::from_secs(5))
                    .await
                    .expect("Console runtime retirement must drain before scratch removal");
            })
            .await;
    }

    /// Gateway authorization reads the active ConfigGeneration, so a config
    /// reload that lowers autonomy to ReadOnly flips a previously-allowed mutation
    /// to denied without a process restart or a second cached config owner.
    #[test]
    fn authz_reads_hot_shared_config_after_reload() {
        use crate::security::policy::{AutonomyLevel, ResourceRiskLevel};

        // Start in the default autonomous policy: a low-risk gateway mutation is
        // allowed.
        let provider: Arc<dyn Provider> = Arc::new(CapturingProvider::default());
        let state = test_app_state(Config::default(), provider);
        assert!(
            crate::gateway::api::authorize_resource_mutation(
                &state,
                "gateway_api:config:update",
                ResourceRiskLevel::Low,
            )
            .is_ok(),
            "default autonomous policy should allow a low-risk mutation"
        );

        // Hot-reload: publish a ReadOnly config into D only. C is intentionally left
        // stale to prove authorization no longer depends on it.
        let read_only = Config {
            autonomy: crate::config::AutonomyConfig {
                level: AutonomyLevel::ReadOnly,
                ..crate::config::AutonomyConfig::default()
            },
            ..Config::default()
        };
        state
            .config
            .apply_runtime_config(read_only, crate::config::ConfigReloadTrigger::Test)
            .expect("apply read-only config");

        // Same call, same risk — now denied because the authz path reads D.
        let denied = crate::gateway::api::authorize_resource_mutation(
            &state,
            "gateway_api:config:update",
            ResourceRiskLevel::Low,
        )
        .expect_err("ReadOnly autonomy published to SharedConfig must deny the mutation");
        assert_eq!(denied.0, axum::http::StatusCode::FORBIDDEN);
        assert!(
            denied
                .1
                .0
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .contains("read-only mode"),
            "denial reason should reflect read-only autonomy"
        );
    }

    /// D2 / fix 3 regression: `post_config` merges the incoming delta onto the hot
    /// SharedConfig (D), NOT the cached C Mutex. We seed D with a hot field value that
    /// differs from C, POST an unrelated delta, and assert the hot field SURVIVES — if
    /// the merge had used stale C as its base it would silently revert the hot field.
    #[tokio::test]
    async fn post_config_merge_base_is_hot_shared_config_not_stale_cache() {
        use super::super::config::post_config;

        let tmp = tempfile::tempdir().expect("tempdir");
        let config_path = tmp.path().join("config.toml");
        let workspace = tmp.path().join("workspace");

        // C (cached) base: default temperature, valid config path on disk.
        let mut cached = Config::default();
        cached.config_path = config_path.clone();
        cached.workspace_dir = workspace.clone();
        cached.default_temperature = 0.0;

        let provider: Arc<dyn Provider> = Arc::new(CapturingProvider::default());
        let state = test_app_state(cached, provider);

        // D (hot) holds a DIFFERENT default_temperature — simulating a prior hot
        // reload that C has not yet observed.
        let mut hot = Config::default();
        hot.config_path = config_path.clone();
        hot.workspace_dir = workspace;
        hot.default_temperature = 0.42;
        state
            .config
            .apply_runtime_config(hot, crate::config::ConfigReloadTrigger::Test)
            .expect("apply hot config");

        // POST a delta that does NOT touch default_temperature.
        let resp = post_config(
            axum::extract::State(state.clone()),
            axum::extract::Query(crate::gateway::api::config::ConfigWriteQuery::default()),
            axum::http::HeaderMap::new(),
            axum::Json(serde_json::json!({ "auto_save": true })),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        // The hot value from D survived the merge (would be 0.0 if C were the base).
        let after = state.config.load_full();
        assert!(
            (after.default_temperature - 0.42).abs() < 1e-9,
            "merge base must be D: hot default_temperature should be preserved, got {}",
            after.default_temperature
        );
        // C is re-synced to D (C == D invariant restored).
        assert!((state.config.load_full().default_temperature - 0.42).abs() < 1e-9);
    }

    async fn response_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("test: approval inbox body");
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::json!({ "raw": String::from_utf8_lossy(&bytes).to_string() }))
    }

    #[tokio::test]
    async fn approval_inbox_disabled_plane_is_conflict() {
        let mut config = Config::default();
        config.workspace_dir = std::env::temp_dir();
        // OV-13: approvals are disabled when user-message recording is off.
        config.memory.events.record_user_messages = false;
        let state = test_app_state(config, Arc::new(CapturingProvider::default()));
        let response = super::get_session_approvals(
            State(state),
            Query(ApprovalInboxQuery {
                limit: None,
                cursor: None,
            }),
            axum::Extension(super::super::ApiPrincipal::for_tests()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = response_json(response).await;
        assert_eq!(
            body.get("error").and_then(serde_json::Value::as_str),
            Some("Decision approvals are disabled")
        );
    }

    fn pending_event(workspace: &str, owner: &str, session: &str, decision: &str) -> crate::memory::MessageEventInput {
        crate::memory::MessageEventInput {
            agent_partition: None,
            event_id: None,
            idempotency_key: Some(format!("decision-approval:{decision}:pending")),
            workspace_id: workspace.to_owned(),
            owner_id: Some(owner.to_owned()),
            source: crate::memory::MessageEventSource::Console,
            channel: Some("console".to_owned()),
            session_key: Some(session.to_owned()),
            parent_session_key: None,
            run_id: None,
            parent_run_id: None,
            agent_id: None,
            persona_id: None,
            sender: None,
            recipient: None,
            role: "system".to_owned(),
            event_type: "approval.decision.pending".to_owned(),
            subject: None,
            goal_id: None,
            causation_event_id: None,
            correlation_id: None,
            attempt_id: None,
            lease_epoch: None,
            config_generation_id: None,
            config_source_revision: None,
            content: "pending".to_owned(),
            raw_payload_json: None,
            visibility: crate::memory::MemoryVisibility::Session,
        }
    }

    #[tokio::test]
    async fn approval_inbox_query_returns_only_the_requested_owner() {
        let temp = tempfile::tempdir().expect("test: inbox scratch");
        let memory = crate::memory::SqliteMemory::new(temp.path()).expect("test: sqlite");
        let workspace = temp.path().to_string_lossy().to_string();
        memory
            .append_message_event(pending_event(
                &workspace,
                "api:owner-a",
                "session-a",
                "11111111-1111-4111-8111-111111111111",
            ))
            .await
            .expect("test: owner a event");
        memory
            .append_message_event(pending_event(
                &workspace,
                "api:owner-b",
                "session-b",
                "22222222-2222-4222-8222-222222222222",
            ))
            .await
            .expect("test: owner b event");
        let rows = memory
            .list_owned_message_events_by_type(&workspace, "api:owner-a", "approval.decision.pending", 0, 20)
            .await
            .expect("test: owner query");
        assert_eq!(rows.len(), 1);
        let row = rows.first().expect("test: owner row");
        assert_eq!(row.owner_id.as_deref(), Some("api:owner-a"));
        assert_eq!(row.session_key.as_deref(), Some("session-a"));
        let foreign = memory
            .list_owned_message_events_by_type(&workspace, "api:owner-b", "approval.decision.pending", 0, 20)
            .await
            .expect("test: foreign query");
        assert_eq!(foreign.len(), 1);
        let foreign_row = foreign.first().expect("test: foreign row");
        assert_eq!(foreign_row.session_key.as_deref(), Some("session-b"));
    }

    #[tokio::test]
    async fn approval_inbox_empty_when_the_owner_has_none() {
        let temp = tempfile::tempdir().expect("test: inbox scratch");
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_owned();
        config.config_path = temp.path().join("config.toml");
        config.autonomy.level = crate::security::AutonomyLevel::Supervised;
        config.default_provider = Some("ollama".to_owned());
        config.default_model = Some("prx-local-model".to_owned());
        config.providers.models.push(
            toml::from_str(
                "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\ncontext_window = 65536\n",
            )
            .expect("test: model catalog"),
        );
        crate::providers::selection::check(
            &config,
            "ollama",
            "prx-local-model",
            &crate::providers::catalog::Requirement::chat().with(crate::providers::catalog::Feature::NativeTools),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )
        .expect("test: model resolves");
        let mut state = test_app_state_with_config(config, Arc::new(CapturingProvider::default()));
        let memory = Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("test: sqlite"));
        state.mem = memory.clone();
        let mut runtime = state.pin_turn_runtime();
        {
            let security = Arc::new(crate::security::SecurityPolicy::from_config(
                &runtime.config_generation.effective.autonomy,
                &runtime.config_generation.effective.workspace_dir,
            ));
            let mutable = Arc::get_mut(&mut runtime).expect("test: runtime uniquely owned");
            mutable.publication_slot =
                Arc::new(crate::gateway::turn_source::publication_slot::GatewayPublicationSlot::new(security, 0));
        }
        let current = Arc::try_unwrap(runtime).unwrap_or_else(|_| panic!("test: runtime uniquely owned"));
        state.turn_runtime = Some(Arc::new(crate::gateway::GatewayTurnRuntimeOwner::new(
            current,
            Arc::clone(&state.config),
            Arc::clone(&state.mem),
            None,
            None,
            None,
            None,
            Arc::clone(&state.hooks),
            #[cfg(feature = "wasm-plugins")]
            None,
        )));
        let runtime = state.pin_turn_runtime();
        let service = runtime
            .publication_slot
            .publish(&runtime, &state)
            .await
            .expect("test: decision plane publishes");
        let principal = super::super::ApiPrincipal::for_tests();
        let response = super::get_session_approvals(
            State(state.clone()),
            Query(ApprovalInboxQuery {
                limit: Some(0),
                cursor: None,
            }),
            axum::Extension(principal.clone()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{}", response_json(response).await);
        let response = super::get_session_approvals(
            State(state.clone()),
            Query(ApprovalInboxQuery {
                limit: None,
                cursor: None,
            }),
            axum::Extension(principal.clone()),
        )
        .await;
        let body = response_json(response).await;
        assert_eq!(
            body.get("approvals")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(0),
            "{body}"
        );
        assert!(
            body.get("next_cursor").is_some_and(serde_json::Value::is_null),
            "{body}"
        );
        let workspace = state.config.pin().effective.workspace_dir.to_string_lossy().to_string();
        memory
            .append_message_event(pending_event(
                &workspace,
                &principal.owner_id,
                "session-own",
                "33333333-3333-4333-8333-333333333333",
            ))
            .await
            .expect("test: unsigned own event");
        memory
            .append_message_event(pending_event(
                &workspace,
                "api:someone-else",
                "session-foreign",
                "44444444-4444-4444-8444-444444444444",
            ))
            .await
            .expect("test: unsigned foreign event");
        let response = super::get_session_approvals(
            State(state.clone()),
            Query(ApprovalInboxQuery {
                limit: None,
                cursor: None,
            }),
            axum::Extension(principal),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        let rendered = body.to_string();
        assert!(!rendered.contains("session-own"), "{body}");
        assert!(!rendered.contains("session-foreign"), "{body}");
        assert_eq!(
            body.get("approvals")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(0),
            "{body}"
        );
        let _ = service.drain_writes(Instant::now() + Duration::from_secs(5)).await;
        let _ = runtime
            .publication_slot
            .cleanup
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await;
    }

    #[tokio::test]
    async fn approval_inbox_fail_closed_memory_is_not_conflict() {
        let temp = tempfile::tempdir().expect("test: inbox scratch");
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_owned();
        config.config_path = temp.path().join("config.toml");
        config.autonomy.level = crate::security::AutonomyLevel::Supervised;
        config.default_provider = Some("ollama".to_owned());
        config.default_model = Some("prx-local-model".to_owned());
        let state = test_app_state_with_config(config, Arc::new(CapturingProvider::default()));
        let response = super::get_session_approvals(
            State(state),
            Query(ApprovalInboxQuery {
                limit: None,
                cursor: None,
            }),
            axum::Extension(super::super::ApiPrincipal::for_tests()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response_json(response).await;
        let error = body.get("error").and_then(serde_json::Value::as_str).unwrap_or("");
        assert_ne!(error, "Decision approvals are disabled", "{body}");
        assert!(!error.to_ascii_lowercase().contains("disabled"), "{body}");
    }
}

async fn call_approval_action(
    state: AppState,
    session: String,
    id: String,
    principal: super::ApiPrincipal,
    change: Option<DecisionApprovalChange>,
    owner_authenticated: bool,
) -> Response {
    use crate::security::approval_ledger::keys::RunCallAuthority;
    let authority = match RunCallAuthority::parse_call_id(&id) {
        Ok(value) => value,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "Invalid call id"),
    };
    let runtime = match state.admit_turn_runtime() {
        Ok(runtime) => runtime,
        Err(refusal) => return super::provider_not_ready(refusal),
    };
    if let Some(response) = approvals_unavailable(&runtime.config_generation.effective, state.mem.as_ref()) {
        return response;
    }
    if change.is_some()
        && !owner_authenticated
        && let Err(message) = console_decision_control_policy(&runtime.config_generation.effective)
    {
        return json_error(StatusCode::FORBIDDEN, message);
    }
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cleanup =
        match crate::runtime::turn_cleanup::CleanupScope::new(Some(&runtime.publication_slot.cleanup), cancellation) {
            Ok(value) => value,
            Err(error) => {
                warn_cleanup_refused(&runtime.publication_slot.cleanup, "Console call approval", &error);
                return json_error(StatusCode::SERVICE_UNAVAILABLE, "Gateway is draining");
            }
        };
    let result = cleanup
        .run(Box::pin(async {
            let service = runtime.publication_slot.publish(&runtime, &state).await?;
            let mut envelope = RuntimeEnvelope::console(
                runtime
                    .config_generation
                    .effective
                    .workspace_dir
                    .to_string_lossy()
                    .as_ref(),
                &session,
            );
            envelope.owner_id = Some(principal.owner_id.clone());
            let adapter = service.call_approval_strategy(envelope)?;
            let pending = adapter.port.inspect(&principal.owner_id, &session, &authority).await?;
            let receipt = match change {
                None => {
                    anyhow::ensure!(!adapter.port.is_rf_decided(&pending).await?, "approval already decided");
                    serde_json::to_value(pending.view())?
                }
                Some(DecisionApprovalChange::Review(approved)) => {
                    let expires_at = adapter
                        .port
                        .review(&principal.owner_id, &session, &authority, approved)
                        .await?;
                    serde_json::json!({"decision_id":"", "call_id":id, "approved":approved,
                        "grant_count":usize::from(approved), "expires_at":expires_at})
                }
                Some(DecisionApprovalChange::Revoke) => {
                    adapter.port.revoke(&principal.owner_id, &session, &authority).await?;
                    serde_json::json!({"decision_id":"", "call_id":id, "revoked_grants":1})
                }
            };
            Ok::<_, anyhow::Error>(receipt)
        }))
        .await;
    result.map_or_else(
        |_| {
            json_error(
                StatusCode::CONFLICT,
                "Call approval is unavailable, expired, or already resolved",
            )
        },
        |view| Json(view).into_response(),
    )
}

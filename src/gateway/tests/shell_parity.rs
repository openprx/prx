#![cfg(test)]
use super::*;
use crate::providers::{ChatRequest, ChatResponse};
use std::sync::atomic::AtomicBool;

const RESPONSE_LIMIT: usize = 64 * 1024;

#[derive(Default)]
struct GitProvider {
    calls: AtomicUsize,
    saw_git: AtomicBool,
}

#[async_trait]
impl Provider for GitProvider {
    fn capabilities(&self) -> crate::providers::ProviderCapabilities {
        crate::providers::ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        Ok("done".into())
    }
    async fn chat(&self, request: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        if request
            .messages
            .iter()
            .any(|message| message.role == "tool" && message.content.contains("git version"))
        {
            self.saw_git.store(true, Ordering::SeqCst);
        }
        Ok(ChatResponse {
            text: (!first).then(|| "done".into()),
            tool_calls: if first {
                vec![crate::providers::ToolCall {
                    id: "parity-git-call".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command":"git --version"}).to_string(),
                }]
            } else {
                vec![]
            },
            reasoning_content: None,
        })
    }
}

#[tokio::test]
async fn full_gateway_turn_runs_git_through_native_shell() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let driver = crate::agent::agent::native_decision_fixture::Driver::start("shell")
            .await
            .expect("test: decision endpoint");
        let endpoint = driver.endpoint().to_owned();
        driver
            .run(true, async move {
                let root = tempfile::tempdir().expect("test: workspace");
                let mut config = Config::default();
                config.workspace_dir = root.path().join("workspace");
                config.config_path = root.path().join("config.toml");
                config.default_provider = Some("ollama".into());
                config.default_model = Some("prx-shell-parity".into());
                config.autonomy.level = crate::security::AutonomyLevel::Full;
                config.tool_tiering.always_include = vec!["shell".into()];
                config.providers.models.push(
                    serde_json::from_value(serde_json::json!({
                        "provider":"ollama","model":"prx-shell-parity","operations":["chat"],"native_tools":true
                    }))
                    .expect("test: chat declaration"),
                );
                config.cost.prices.insert(
                    "ollama/prx-shell-parity".into(),
                    crate::config::schema::ModelPricing {
                        input: 1.0,
                        output: 1.0,
                        cache_write: 1.0,
                        cache_read: 1.0,
                    },
                );
                crate::agent::agent::native_decision_fixture::configure(&mut config, &endpoint)
                    .expect("test: native decision config");
                let resolved =
                    providers::selection::check_default(&config, providers::catalog::UnknownPolicy::Invocation)
                        .expect("test: provider selection");
                let model = resolved.model().to_owned();
                let security = Arc::new(crate::security::SecurityPolicy::from_config(
                    &config.autonomy,
                    &config.workspace_dir,
                ));
                let memory: Arc<dyn Memory> =
                    Arc::new(SqliteMemory::new(&config.workspace_dir).expect("test: durable memory"));
                let provider_impl = Arc::new(GitProvider::default());
                let provider: Arc<dyn Provider> = provider_impl.clone();
                let mut state = webhook_test_state_at(provider.clone(), config.workspace_dir.clone());
                state.config = crate::config::new_shared(config);
                state.model = model.clone();
                state.mem = memory.clone();
                state.hooks = Arc::new(HookManager::new(root.path().into()));
                state.tools_registry = Arc::new(vec![Box::new(crate::tools::shell::ShellTool::new(
                    Arc::new(crate::runtime::NativeRuntime::new()),
                    root.path().into(),
                ))]);
                let runtime = TurnRuntimeGeneration {
                    provider_binding: Some(Arc::new(decision_source::GatewayProviderBinding::new(
                        provider.clone(),
                        resolved,
                    ))),
                    decision_slot: Arc::new(decision_runtime::GatewayDecisionSlot::new(security, 0)),
                    config_generation: state.config.pin(),
                    provider,
                    model,
                    temperature: state.temperature,
                    tools_registry: state.tools_registry.clone(),
                    mcp_tool: None,
                };
                let cleanup = runtime.decision_slot.cleanup.clone();
                let owner = Arc::new(GatewayTurnRuntimeOwner::new(
                    runtime,
                    state.config.clone(),
                    memory,
                    None,
                    None,
                    None,
                    None,
                    state.hooks.clone(),
                    #[cfg(feature = "wasm-plugins")]
                    None,
                ));
                state.turn_runtime = Some(owner.clone());
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("test: gateway bind");
                let address = listener.local_addr().expect("test: gateway address");
                let stop = tokio_util::sync::CancellationToken::new();
                let stopping = stop.clone();
                let router = axum::Router::new()
                    .route("/webhook", axum::routing::post(handle_webhook))
                    .with_state(state);
                let server = tokio::spawn(async move {
                    axum::serve(
                        listener,
                        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                    )
                    .with_graceful_shutdown(stopping.cancelled_owned())
                    .await
                });
                let result = cleanup
                    .scope(async {
                        let mut response = reqwest::Client::new()
                            .post(format!("http://{address}/webhook"))
                            .json(&serde_json::json!({"message":"shell git --version"}))
                            .send()
                            .await?;
                        let status = response.status();
                        let mut bytes = Vec::new();
                        while let Some(chunk) = response.chunk().await? {
                            anyhow::ensure!(
                                bytes.len().saturating_add(chunk.len()) <= RESPONSE_LIMIT,
                                "gateway response exceeded cap"
                            );
                            bytes.extend_from_slice(&chunk);
                        }
                        Ok::<_, anyhow::Error>((status, String::from_utf8(bytes)?))
                    })
                    .await;
                stop.cancel();
                tokio::time::timeout(Duration::from_secs(5), server)
                    .await
                    .expect("test: server deadline")
                    .expect("test: server task")
                    .expect("test: server shutdown");
                drop(owner);
                cleanup
                    .drain(tokio::time::Instant::now() + Duration::from_secs(10))
                    .await
                    .expect("test: gateway cleanup");
                let (status, body) = result.expect("test: real HTTP gateway request");
                assert_eq!(status, reqwest::StatusCode::OK, "test: gateway result: {body}");
                assert!(
                    provider_impl.saw_git.load(Ordering::SeqCst),
                    "test: real git output reached provider: {body}"
                );
                assert_eq!(
                    provider_impl.calls.load(Ordering::SeqCst),
                    2,
                    "test: actual tool round trip"
                );
            })
            .await;
    })
    .await
    .expect("test: gateway deadline");
}

#[tokio::test]
async fn generation_swap_changes_git_outcome_between_calls() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let root = tempfile::tempdir().expect("test: root");
        let mut config = Config::default();
        config.workspace_dir = root.path().join("workspace");
        config.config_path = root.path().join("config.toml");
        config.default_provider = Some("ollama".into());
        config.default_model = Some("prx-shell-parity".into());
        config.causal_tree.enabled = false;
        config.autonomy.level = crate::security::AutonomyLevel::Full;
        config.providers.models.push(serde_json::from_value(serde_json::json!({"provider":"ollama","model":"prx-shell-parity","operations":["chat"],"native_tools":true})).expect("test: model"));
        config.cost.prices.insert("ollama/prx-shell-parity".into(), crate::config::schema::ModelPricing { input: 1.0, output: 1.0, cache_write: 1.0, cache_read: 1.0 });
        let shared = crate::config::new_shared(config.clone());
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&config.workspace_dir).expect("test: memory"));
        let hooks = Arc::new(HookManager::new(config.workspace_dir.clone()));
        let runtime = build_gateway_turn_runtime(shared.pin(), shared.clone(), memory.clone(), None, None, None, None, hooks.clone(), #[cfg(feature = "wasm-plugins")] None).expect("test: initial gateway");
        let owner = Arc::new(GatewayTurnRuntimeOwner::new(runtime, shared.clone(), memory.clone(), None, None, None, None, hooks.clone(), #[cfg(feature = "wasm-plugins")] None));
        let participant: Arc<dyn crate::config::ConfigGenerationParticipant> = owner.clone();
        shared.register_participant(&participant);
        let mut state = webhook_test_state_at(Arc::new(GitProvider::default()), config.workspace_dir.clone());
        state.config = shared.clone();
        state.mem = memory;
        state.hooks = hooks;
        state.turn_runtime = Some(owner.clone());
        for level in [crate::security::AutonomyLevel::Full, crate::security::AutonomyLevel::Supervised] {
            config.autonomy.level = level;
            shared.apply_runtime_config(config.clone(), crate::config::ConfigReloadTrigger::Test).expect("test: live gateway reload");
            let runtime = owner.pin();
            let service = runtime.decision_slot.publish(&runtime, &state).await.expect("test: live publication");
            let context = crate::tools::ToolExecutionContext::new(crate::runtime::envelope::RuntimeEnvelope::agent("workspace-a", "parity-reload").with_owner_id("prx_user").with_channel("terminal"), "dm");
            let outcome = service.execution().execute(crate::tools::ToolExecutionCommand::new("shell", serde_json::json!({"command":"git --version"})), context, None).await;
            let success = outcome.result.as_ref().is_some_and(|result| result.success);
            assert_eq!(success, level == crate::security::AutonomyLevel::Full, "test: existing gateway adopts generation policy");
        }
        let cleanup = owner.cleanup.clone();
        drop(state);
        drop(participant);
        drop(owner);
        cleanup.drain(tokio::time::Instant::now() + Duration::from_secs(5)).await.expect("test: reload cleanup");
    }).await.expect("test: live reload deadline");
}

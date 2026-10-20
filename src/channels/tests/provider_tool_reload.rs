#![cfg(test)]

//! Reload proofs for tools whose registry outlives provider credentials.

use super::*;
use crate::config::{ConfigGeneration, ConfigGenerationParticipant, PreparedConfigGeneration};
use crate::tools::execution::{ToolExecutionCommand, ToolExecutionContext, ToolExecutionService};

const MODEL: &str = "gpt-4o";
const RELOADED_IMAGE_MODEL: &str = "prx-reloaded-vision";
const PNG_DATA_URI: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

struct ReloadAck;
struct PreparedReloadAck;

impl ConfigGenerationParticipant for ReloadAck {
    fn name(&self) -> &'static str {
        "prx-provider-tool-reload"
    }

    fn supports_rebuild_field(&self, field: &str) -> bool {
        matches!(
            field,
            "api_key" | "api_url" | "default_provider" | "default_model" | "default_temperature"
        )
    }

    fn prepare(
        &self,
        _generation: Arc<ConfigGeneration>,
        changed_fields: &[String],
    ) -> anyhow::Result<Box<dyn PreparedConfigGeneration>> {
        anyhow::ensure!(
            changed_fields.iter().all(|field| self.supports_rebuild_field(field)),
            "unexpected provider-tool reload field"
        );
        Ok(Box::new(PreparedReloadAck))
    }
}

impl PreparedConfigGeneration for PreparedReloadAck {
    fn commit(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn rollback(&mut self) {}
}

struct ProviderEndpoint {
    url: String,
    requests: Arc<Mutex<Vec<(String, String, serde_json::Value)>>>,
    received: Arc<tokio::sync::Notify>,
    shutdown: tokio_util::sync::CancellationToken,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl ProviderEndpoint {
    async fn start(marker: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("provider reload probe listener");
        let url = format!(
            "http://{}/v1",
            listener.local_addr().expect("provider reload probe address")
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::new(tokio::sync::Notify::new());
        let captured = Arc::clone(&requests);
        let notification = Arc::clone(&received);
        let app = axum::Router::new().fallback(axum::routing::any(
            move |uri: axum::http::Uri, headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let captured = Arc::clone(&captured);
                let notification = Arc::clone(&notification);
                async move {
                    let authorization = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    let body = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                    captured.lock().push((uri.path().to_owned(), authorization, body));
                    notification.notify_one();
                    axum::Json(serde_json::json!({
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": marker},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    }))
                }
            },
        ));
        let shutdown = tokio_util::sync::CancellationToken::new();
        let server_shutdown = shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(server_shutdown.cancelled_owned())
                .await
        });
        Self {
            url,
            requests,
            received,
            shutdown,
            server,
        }
    }

    async fn wait_for_request(&self) {
        let notified = self.received.notified();
        if self.requests.lock().is_empty() {
            tokio::time::timeout(Duration::from_secs(10), notified)
                .await
                .expect("reloaded provider request");
        }
    }

    async fn close(self) {
        self.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), self.server)
            .await
            .expect("provider reload probe shutdown timeout")
            .expect("provider reload probe join")
            .expect("provider reload probe server");
    }
}

struct SilentChannel;

#[async_trait::async_trait]
impl Channel for SilentChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn send(&self, _message: &traits::SendMessage) -> anyhow::Result<()> {
        Ok(())
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>) -> anyhow::Result<()> {
        Ok(())
    }
}

fn provider_config(workspace: &std::path::Path, endpoint: &ProviderEndpoint, key: &str) -> Config {
    let mut config = Config::default();
    config.workspace_dir = workspace.to_owned();
    config.config_path = workspace.join("config.toml");
    config.default_provider = Some("openai".to_owned());
    config.default_model = Some(MODEL.to_owned());
    config.default_temperature = 0.0;
    config.api_key = Some(key.to_owned());
    config.api_url = Some(endpoint.url.clone());
    config.causal_tree.enabled = false;
    config.autonomy.level = crate::security::AutonomyLevel::Full;
    config.reliability.provider_retries = 0;
    config.memory.auto_save = false;
    config.providers.models.push(
        serde_json::from_value(serde_json::json!({
            "provider": "openai",
            "model": MODEL,
            "operations": ["chat"],
            "native_tools": true,
            "vision_input": true
        }))
        .expect("provider reload model declaration"),
    );
    config.cost.prices.insert(
        format!("openai/{MODEL}"),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config
}

fn apply_provider_reload(
    shared: &crate::config::SharedConfig,
    endpoint: &ProviderEndpoint,
    key: &str,
) -> Arc<ConfigGeneration> {
    let mut desired = shared.pin().effective.as_ref().clone();
    desired.api_key = Some(key.to_owned());
    desired.api_url = Some(endpoint.url.clone());
    let report = shared
        .apply_runtime_config(desired, crate::config::ConfigReloadTrigger::Test)
        .expect("provider-tool hot reload");
    assert_eq!(report.participant_acks, vec!["prx-provider-tool-reload"]);
    shared.pin()
}

fn apply_image_provider_reload(
    shared: &crate::config::SharedConfig,
    endpoint: &ProviderEndpoint,
    key: &str,
    provider: &str,
) -> Arc<ConfigGeneration> {
    let mut desired = shared.pin().effective.as_ref().clone();
    desired.default_provider = Some(provider.to_owned());
    desired.default_model = Some(RELOADED_IMAGE_MODEL.to_owned());
    desired.default_temperature = 0.37;
    desired.api_key = Some(key.to_owned());
    desired.api_url = Some(endpoint.url.clone());
    let report = shared
        .apply_runtime_config(desired, crate::config::ConfigReloadTrigger::Test)
        .expect("image provider/model hot reload");
    assert_eq!(report.participant_acks, vec!["prx-provider-tool-reload"]);
    shared.pin()
}

fn provider_resolver(
    shared: &crate::config::SharedConfig,
    generation: &Arc<ConfigGeneration>,
    provider: Arc<dyn Provider>,
) -> Arc<dyn providers::generation::PinnedProviderResolver> {
    let cache = Arc::new(Mutex::new(HashMap::from([(
        (generation.id.0, "openai".to_owned(), MODEL.to_owned()),
        provider,
    )])));
    Arc::new(providers::generation::ConfigGenerationProviderResolver::with_cache(
        Arc::clone(shared),
        providers::provider_runtime_options_from_config(&generation.effective),
        cache,
    ))
}

fn channel_message() -> traits::ChannelMessage {
    traits::ChannelMessage {
        id: "prx-provider-reload-spawn".to_owned(),
        sender: "prx_user".to_owned(),
        reply_target: "prx_channel".to_owned(),
        content: "use sessions_spawn for this task".to_owned(),
        channel: "telegram".to_owned(),
        timestamp: 1,
        thread_ts: None,
        chat_kind: traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: Vec::new(),
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    }
}

async fn wait_for_completed_marker(
    runs: &Arc<tokio::sync::RwLock<Vec<crate::tools::sessions_spawn::SubAgentRun>>>,
    marker: &str,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = runs.read().await.first().map(|run| run.status.clone());
            match status {
                Some(crate::tools::sessions_spawn::SubAgentStatus::Completed(output)) => {
                    assert!(
                        output.contains(marker),
                        "spawn completed without the provider marker: {output}"
                    );
                    return;
                }
                Some(crate::tools::sessions_spawn::SubAgentStatus::Failed(error)) => {
                    panic!("spawn failed before returning the provider marker: {error}");
                }
                _ => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .expect("spawn did not complete with the reloaded provider");
}

#[tokio::test]
async fn image_uses_the_provider_owned_by_the_reloaded_generation() {
    let workspace = tempfile::tempdir().expect("image reload workspace");
    let first = ProviderEndpoint::start("startup image provider").await;
    let second = ProviderEndpoint::start("reloaded image provider").await;
    let reloaded_provider = format!("custom:{}", second.url);
    let mut initial = provider_config(workspace.path(), &first, "prx-image-key-zero");
    initial.providers.models.push(
        serde_json::from_value(serde_json::json!({
            "provider": reloaded_provider,
            "model": RELOADED_IMAGE_MODEL,
            "operations": ["chat"],
            "native_tools": true,
            "vision_input": true
        }))
        .expect("reloaded image model declaration"),
    );
    initial.cost.prices.insert(
        format!("{reloaded_provider}/{RELOADED_IMAGE_MODEL}"),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let shared = crate::config::new_shared(initial);
    let initial_generation = shared.pin();
    let participant: Arc<dyn ConfigGenerationParticipant> = Arc::new(ReloadAck);
    shared.register_participant(&participant);
    let (startup_provider, _, model) = build_channel_startup_provider(
        &initial_generation.effective,
        &providers::provider_runtime_options_from_config(&initial_generation.effective),
    )
    .expect("startup image provider");
    let resolver = provider_resolver(&shared, &initial_generation, Arc::clone(&startup_provider));
    let generation = apply_image_provider_reload(&shared, &second, "prx-image-key-one", &reloaded_provider);
    let security = crate::runtime::bootstrap::build_security_policy(&generation.effective);
    let image = crate::tools::ImageTool::new(
        startup_provider,
        model,
        0.0,
        Arc::clone(&security),
        generation.effective.multimodal.clone(),
    )
    .with_pinned_provider_resolver(resolver);
    let service = ToolExecutionService::with_security_policy(vec![Box::new(image)], security);
    let envelope = crate::runtime::envelope::RuntimeEnvelope::channel(
        workspace.path().to_string_lossy(),
        "telegram",
        "prx_user",
        Some("prx_channel".to_owned()),
    )
    .with_config_generation(&generation);
    let outcome = service
        .execute(
            ToolExecutionCommand::new(
                "image",
                serde_json::json!({"image": PNG_DATA_URI, "prompt": "describe the pixel"}),
            ),
            ToolExecutionContext::new(envelope, "dm"),
            None,
        )
        .await;
    assert!(
        outcome.succeeded(),
        "image reload execution failed: {:?}",
        outcome.error
    );
    assert!(outcome.model_content.contains("reloaded image provider"));
    assert!(first.requests.lock().is_empty(), "startup image provider was reused");
    second.wait_for_request().await;
    let reloaded = second.requests.lock().clone();
    let authenticated = reloaded
        .iter()
        .filter(|(path, authorization, body)| {
            path.ends_with("/chat/completions") && !authorization.is_empty() && body.get("model").is_some()
        })
        .collect::<Vec<_>>();
    assert!(!authenticated.is_empty(), "reloaded image chat request missing");
    assert!(authenticated.iter().all(|(_, authorization, body)| {
        authorization == "Bearer prx-image-key-one"
            && body["model"] == RELOADED_IMAGE_MODEL
            && body["temperature"] == 0.37
    }));
    drop(participant);
    first.close().await;
    second.close().await;
}

#[tokio::test]
async fn sessions_spawn_uses_the_provider_owned_by_the_reloaded_generation() {
    let workspace = tempfile::tempdir().expect("spawn reload workspace");
    let first = ProviderEndpoint::start("startup spawn provider").await;
    let second = ProviderEndpoint::start("reloaded spawn provider").await;
    let initial = provider_config(workspace.path(), &first, "prx-spawn-key-zero");
    let shared = crate::config::new_shared(initial);
    let initial_generation = shared.pin();
    let participant: Arc<dyn ConfigGenerationParticipant> = Arc::new(ReloadAck);
    shared.register_participant(&participant);
    let (startup_provider, provider_name, model) = build_channel_startup_provider(
        &initial_generation.effective,
        &providers::provider_runtime_options_from_config(&initial_generation.effective),
    )
    .expect("startup spawn provider");
    let generation = apply_provider_reload(&shared, &second, "prx-spawn-key-one");
    let security = crate::runtime::bootstrap::build_security_policy(&generation.effective);
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).expect("spawn reload memory"));
    let spawn = crate::tools::SessionsSpawnTool::new(
        Arc::new(SilentChannel),
        provider_name,
        model,
        0.0,
        security,
        workspace.path().to_owned(),
        generation.effective.multimodal.clone(),
        generation.effective.agents_with_references().into_owned(),
        generation.effective.api_key.clone(),
        providers::provider_runtime_options_from_config(&generation.effective),
        generation.effective.sessions_spawn.clone(),
    )
    .with_test_parent_provider(startup_provider)
    .with_configured_providers(&providers::ConfiguredProviders::from_config(&generation.effective))
    .with_model_catalog(providers::selection::catalog_for(&generation.effective))
    .with_hint_config(&generation.effective)
    .with_shared_memory(Arc::clone(&memory));
    let runs = spawn.active_runs_arc();
    let tools_handle = spawn.tools_handle();
    let registry = crate::agent::test_admission::registry_with_search(vec![Box::new(spawn)]);
    assert!(tools_handle.set(&registry), "spawn reload registry injection");
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
        .expect("spawn reload cleanup owner");
    cleanup
        .scope_admission(Box::pin(async {
            let admitted = crate::agent::test_admission::AdmittedTurn::ordinary_controlled_channel_with_generation(
                Arc::clone(&generation),
                channel_message(),
                Arc::clone(&registry),
                Arc::clone(&memory),
                ("openai", MODEL),
            )
            .await;
            let outcome = admitted
                .execute_tool("sessions_spawn", serde_json::json!({"task": "report the marker"}))
                .await
                .expect("admitted spawn reload execution");
            assert!(outcome.success, "spawn reload rejected: {:?}", outcome.error);
            wait_for_completed_marker(&runs, "reloaded spawn provider").await;
            Ok::<_, anyhow::Error>(())
        }))
        .await
        .expect("spawn reload admitted scope");
    let controls = runs
        .read()
        .await
        .iter()
        .filter_map(|run| run.task_control.clone())
        .collect::<Vec<_>>();
    for control in controls {
        let _ = control.request_termination("provider reload fixture cleanup").await;
    }
    cleanup.run(async { Ok(()) }).await.expect("spawn reload cleanup");
    assert!(first.requests.lock().is_empty(), "startup spawn provider was reused");
    second.wait_for_request().await;
    let reloaded = second.requests.lock().clone();
    let authenticated = reloaded
        .iter()
        .filter(|(path, header, body)| {
            path.ends_with("/chat/completions") && !header.is_empty() && body.get("model").is_some()
        })
        .collect::<Vec<_>>();
    assert!(!authenticated.is_empty(), "reloaded spawn chat request missing");
    assert!(
        authenticated
            .iter()
            .all(|(_, header, body)| { header == "Bearer prx-spawn-key-one" && body["model"] == MODEL })
    );
    drop(participant);
    first.close().await;
    second.close().await;
}

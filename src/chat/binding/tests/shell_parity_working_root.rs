#![cfg(test)]
#![cfg(unix)]
//! Genuine recorded Chat model issuance to the companion production worker executable.
use super::*;
use crate::tools::{
    Tool,
    sessions_spawn::{SessionsSpawnTool, SubAgentRun, SubAgentStatus},
};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const MAX_PROVIDER_REQUEST_BYTES: usize = 1024 * 1024;

struct ParentConfigScope {
    previous: Option<PathBuf>,
    _lock: parking_lot::MutexGuard<'static, ()>,
}
impl Drop for ParentConfigScope {
    fn drop(&mut self) {
        crate::config::schema::swap_process_config_dir_for_tests(self.previous.take());
    }
}
struct NoAnnouncements;
#[async_trait::async_trait]
impl crate::channels::traits::Channel for NoAnnouncements {
    fn name(&self) -> &str {
        "terminal"
    }
    async fn send(&self, _: &crate::channels::traits::SendMessage) -> anyhow::Result<()> {
        Ok(())
    }
    async fn listen(
        &self,
        _: tokio::sync::mpsc::Sender<crate::channels::traits::ChannelMessage>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("fixture does not listen")
    }
}

#[tokio::test]
async fn chat_workdir_reaches_real_worker_and_unset_keeps_worker_workspace() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(120), async {
        for override_root in [true, false] {
            Box::pin(root_pwd(override_root)).await?;
        }
        Ok(())
    })
    .await
    .context("test: real worker deadline")?
}

async fn root_pwd(override_root: bool) -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))?;
    let config_dir = root.path().join("config");
    let workspace = config_dir.join("workspace");
    let workdir = root.path().join("working-root");
    let workers = root.path().join("workers");
    for dir in [&config_dir, &workspace, &workdir, &workers] {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    // Use the existing serialized process-config fixture scope; restore before unlocking.
    let _config_scope = ParentConfigScope {
        _lock: crate::config::schema::process_config_dir_test_lock(),
        previous: crate::config::schema::swap_process_config_dir_for_tests(Some(config_dir.clone())),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: provider bind");
    let address = listener.local_addr().expect("test: provider address");
    let child_calls = Arc::new(AtomicUsize::new(0));
    let child_observed = Arc::new(AtomicBool::new(false));
    let observed = child_observed.clone();
    let child_count = child_calls.clone();
    let captured = Arc::new(parking_lot::Mutex::new(String::new()));
    let captured_for_server = captured.clone();
    let app = axum::Router::new().fallback(axum::routing::post(move |axum::Json(request): axum::Json<Value>| {
        let child_calls = child_count.clone();
        let child_observed = observed.clone();
        let captured = captured_for_server.clone();
        async move {
            let messages = request.get("messages").and_then(Value::as_array).expect("test: request messages");
            let text = request.to_string();
            let tool_call = |name: &str, arguments: Value| json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("call-{}", uuid::Uuid::new_v4()),"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]});
            let assistant = if text.contains("Review the proposed final reply against the CURRENT request") {
                json!({"role":"assistant","content":r#"{"decision":"accept","reason":"fixture evidence","constraints":[]}"#})
            } else if text.contains("Classify the execution requirement of current_request") {
                json!({"role":"assistant","content":r#"{"requirement":"execution_required"}"#})
            } else if child_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    tool_call("shell", json!({"command":"pwd"}))
                } else {
                    const MAX_PWD_RESULT_BYTES: usize = 4096;
                    let result = messages.iter().rev().find(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
                        .and_then(|message| message.get("content")).and_then(Value::as_str).expect("test: real shell result on wire");
                    assert!(result.len() <= MAX_PWD_RESULT_BYTES, "test: shell pwd result exceeds fixture bound");
                    *captured.lock() = result.to_owned();
                    child_observed.store(true, Ordering::SeqCst);
                    json!({"role":"assistant","content":"PRX-PWD-DONE"})
            };
            let body = json!({"id":"prx-pwd-response","object":"chat.completion","model":"prx-pwd-model","choices":[{"index":0,"message":assistant,"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
            if request.get("stream").and_then(Value::as_bool) == Some(true) {
                let (delta, reason) = assistant.get("tool_calls").and_then(Value::as_array).map_or_else(
                    || (json!({"content":assistant.get("content")}), "stop"),
                    |calls| {
                        let mut calls = calls.clone();
                        for (index, call) in calls.iter_mut().enumerate() {
                            call.as_object_mut().expect("test: tool-call object").insert("index".into(), json!(index));
                        }
                        (json!({"tool_calls":calls}), "tool_calls")
                    },
                );
                let chunk = json!({"id":"prx-pwd-stream","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
                let finish = json!({"id":"prx-pwd-stream","choices":[{"index":0,"delta":{},"finish_reason":reason}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n")).into_response()
            } else {
                axum::Json(body).into_response()
            }
        }
    }));
    let app = app.layer(axum::extract::DefaultBodyLimit::max(MAX_PROVIDER_REQUEST_BYTES));
    let stop = tokio_util::sync::CancellationToken::new();
    let stopping = stop.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stopping.cancelled_owned())
            .await
    });
    let provider = format!("custom:http://{address}/v1");
    let mut config = crate::config::Config {
        workspace_dir: workspace.clone(),
        config_path: config_dir.join("config.toml"),
        default_provider: Some(provider.clone()),
        default_model: Some("prx-pwd-model".into()),
        api_key: Some("prx-fixture-key".into()),
        ..crate::config::Config::default()
    };
    let driver = crate::agent::agent::native_decision_fixture::Driver::start_delegation("shell").await?;
    crate::agent::agent::native_decision_fixture::configure(&mut config, driver.endpoint())?;
    config.secrets.encrypt = false;
    config.memory.auto_save = false;
    config.runtime.workdir = None;
    config.sessions_spawn.default_mode = "process".into();
    config.sessions_spawn.process_memory_strategy = "isolated_private".into();
    config.sessions_spawn.worker_workspace_root = Some(workers.to_string_lossy().into_owned());
    config.sessions_spawn.cleanup_on_complete = false;
    config.tool_tiering.always_include = vec!["shell".into(), "sessions_spawn".into()];
    config.providers.models.push(
        serde_json::from_value(
            json!({"provider":provider,"model":"prx-pwd-model","operations":["chat"],"native_tools":true}),
        )
        .expect("test: model"),
    );
    config.cost.prices.insert(
        format!("{provider}/prx-pwd-model"),
        crate::config::schema::ModelPricing {
            input: 0.0,
            output: 0.0,
            cache_write: 0.0,
            cache_read: 0.0,
        },
    );
    // Disk has no root. Only the parent's effective generation carries it.
    std::fs::write(&config.config_path, toml::to_string(&config)?)?;
    std::fs::set_permissions(&config.config_path, std::fs::Permissions::from_mode(0o600))?;
    assert!(
        toml::from_str::<crate::config::Config>(&std::fs::read_to_string(&config.config_path)?)?
            .runtime
            .workdir
            .is_none(),
        "test: disk must contain no root"
    );
    config = crate::config::Config::load_existing_read_only_with_config_dir(Some(
        config_dir.to_str().context("test: config path UTF-8")?,
    ))
    .await?;
    config.runtime.workdir = override_root.then(|| workdir.clone());
    let executable = std::env::current_exe()?
        .parent()
        .and_then(|path| path.parent())
        .context("test: cargo target directory")?
        .join(format!("prx{}", std::env::consts::EXE_SUFFIX));
    const MAX_BINARY_BYTES: u64 = 1024 * 1024 * 1024;
    let mut binary = std::fs::File::open(&executable)
        .context("test: build companion binary with --test shell_parity_cli")?
        .take(MAX_BINARY_BYTES + 1);
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut bytes = 0u64;
    loop {
        let count = binary.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        anyhow::ensure!(bytes <= MAX_BINARY_BYTES, "test: companion binary bound");
        hash.update(buffer.get(..count).context("test: read buffer count")?);
    }
    let generation = crate::config::new_shared(config.clone()).pin();
    let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::SqliteMemory::new(&workspace)?);
    let fabric = crate::memory::MemoryFabric::new(memory.clone(), workspace.to_string_lossy())
        .with_event_recording(config.memory.event_recording_config());
    let security = Arc::new(crate::security::SecurityPolicy::from_config(
        &config.autonomy,
        &workspace,
    ));
    let provider: Arc<dyn crate::providers::Provider> =
        Arc::from(crate::providers::create_provider(&provider, config.api_key.as_deref())?);
    let provider_name = config.default_provider.as_deref().context("test: provider")?;
    let model = "prx-pwd-model";
    let binding =
        ChatProviderBinding::capture(&generation, &provider, provider_name, model)?.context("test: Chat binding")?;
    let runs = Arc::new(tokio::sync::RwLock::new(Vec::<SubAgentRun>::new()));
    let spawn = SessionsSpawnTool::new_with_registry(
        Arc::new(NoAnnouncements),
        provider_name,
        model,
        0.0,
        security.clone(),
        workspace.clone(),
        config.multimodal.clone(),
        Default::default(),
        None,
        crate::providers::provider_runtime_options_from_config(&config),
        config.sessions_spawn.clone(),
        runs.clone(),
    )
    .with_test_parent_provider(provider.clone())
    .with_configured_providers(&crate::providers::ConfiguredProviders::from_config(&config))
    .with_hint_config(&config)
    .with_model_catalog(crate::providers::selection::catalog_for(&config))
    .with_shared_memory(memory.clone())
    .with_source_fabric(fabric.clone())
    .with_event_recording(config.memory.event_recording_config())
    .with_test_worker_executable(executable, hex::encode(hash.finalize()), root.path().to_path_buf())?;
    let handle = spawn.tools_handle();
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
        Box::new(spawn),
        Box::new(crate::tools::shell::ShellTool::new(
            Arc::new(crate::runtime::NativeRuntime::new()),
            workdir.clone(),
        )),
    ]);
    anyhow::ensure!(handle.set(&registry), "test: registry publication");
    let owner = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    let scope =
        crate::runtime::turn_cleanup::CleanupScope::new(Some(&owner), tokio_util::sync::CancellationToken::new())?;
    let result = driver
        .run(
            true,
            scope.run(Box::pin(async {
                let publication = crate::runtime::turn_publication::TurnDecisionPublication {
                    generation: generation.clone(),
                    registry: registry.clone(),
                    mcp_tool: None,
                    security: security.clone(),
                    observer: Arc::new(crate::observability::noop::NoopObserver),
                    hooks: Arc::new(crate::hooks::HookManager::new(workspace.clone())),
                    memory: memory.clone(),
                    plugin_generation: 0,
                }
                .publish(crate::tools::capability::turn_state::TurnSurfaceKind::Interactive)
                .await?;
                owner.retain_service(publication.clone());
                use crate::runtime::turn_source::TurnSourceProof as _;
                let task = "Run shell pwd for PRX-PWD-CHILD";
                let input = "Use sessions_spawn in process mode; the worker must run shell pwd for PRX-PWD-CHILD.";
                let session_key = "prx_working_root_session";
                let run_id = "prx_working_root_parent";
                let session = crate::chat::session::ChatSession::new(provider_name, model);
                let event = crate::chat::record_chat_user_message_event_in_generation(
                    &fabric,
                    &session,
                    session_key,
                    run_id,
                    provider_name,
                    model,
                    1,
                    input,
                    Some(&generation),
                )
                .await?;
                let receipt =
                    ChatSourceReceipt::issue(memory.as_ref(), &generation, session_key, run_id, input, &event).await?;
                let proof = receipt.bind_provider(
                    &binding,
                    ChatAdmissionMode::Model(crate::agent::turn_contracts::ChatMode::Edit),
                )?;
                let cancellation = tokio_util::sync::CancellationToken::new();
                let context = publication
                    .open_issued_turn_context(
                        proof.as_ref(),
                        crate::runtime::turn_decision::AgentTurnKind::Repl,
                        &cancellation,
                    )
                    .await?;
                let selected = publication.agent_execution_specs(proof.as_ref(), &context)?;
                anyhow::ensure!(
                    selected.specs.len() == 1
                        && selected.specs.first().is_some_and(|spec| spec.name == "sessions_spawn"),
                    "test: actual Chat decision must select sessions_spawn"
                );
                let original = crate::tools::ToolExecutionContext::new(proof.envelope().clone(), "private")
                    .with_chat_id(session_key);
                let (execution, bound) =
                    publication.bind_agent_execution(proof.as_ref(), &context, &original, &selected.specs)?;
                anyhow::ensure!(
                    bound.child_grant.is_some(),
                    "test: real parent-issued child grant required"
                );
                let spawn_context = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
                    proof.run_id().to_owned(),
                    proof.session_key().to_owned(),
                );
                let result = crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
                    .scope(
                        spawn_context,
                        execution.execute(
                            crate::tools::ToolExecutionCommand::new(
                                "sessions_spawn",
                                json!({"action":"spawn_batch", "mode":"process", "tasks":[task]}),
                            ),
                            bound,
                            Some(cancellation),
                        ),
                    )
                    .await;
                anyhow::ensure!(
                    result.succeeded(),
                    "test: actual Chat process launch rejected: {result:?}"
                );
                tokio::time::timeout(Duration::from_secs(60), async {
                    loop {
                        {
                            let runs = runs.read().await;
                            let run = runs.first().context("test: actual worker run")?;
                            anyhow::ensure!(run.process_control.is_some(), "test: actual worker process required");
                            if run.finished_at.is_some() {
                                anyhow::ensure!(
                                    matches!(run.status, SubAgentStatus::Completed(_)),
                                    "test: real worker failed: {:?}",
                                    run.status
                                );
                                return Ok::<_, anyhow::Error>(());
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .context("test: worker completion")?
            })),
        )
        .await;
    // Always reap owned runs, including a failed worker carry check.
    let controls: Vec<_> = {
        let runs = runs.read().await;
        runs.iter().filter_map(|run| run.process_control.clone()).collect()
    };
    for control in controls {
        if control.finalization().is_none() {
            tokio::time::timeout(
                Duration::from_secs(5),
                control.request_termination("working-root fixture cleanup"),
            )
            .await?;
        }
    }
    owner
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await?;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(5), server).await???;
    result?;
    anyhow::ensure!(
        child_observed.load(Ordering::SeqCst),
        "test: real shell result on wire required"
    );
    assert_eq!(
        child_calls.load(Ordering::SeqCst),
        2,
        "test: real worker must execute exactly one shell call"
    );
    let wire = captured.lock().clone();
    let parsed = serde_json::from_str::<Value>(&wire).ok();
    let output = parsed
        .as_ref()
        .and_then(|value| value.get("output"))
        .and_then(Value::as_str)
        .unwrap_or(&wire);
    anyhow::ensure!(
        output.starts_with("Process exited with code 0\nProcess started: true\nOutput complete: true\n"),
        "test: successful native shell: {output}"
    );
    let actual = output
        .split_once("stdout:\n")
        .and_then(|(_, text)| text.rsplit_once("\nstderr:").map(|(stdout, _)| stdout))
        .context("test: shell stdout")?
        .trim();
    let expected = if override_root {
        workdir
    } else {
        let directories: Vec<_> = std::fs::read_dir(&workers)?.take(2).collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(directories.len() == 1, "test: exactly one worker workspace");
        directories.first().context("test: worker workspace")?.path()
    };
    assert_eq!(
        std::path::Path::new(actual),
        expected,
        "test: root must come only from signed carry or default worker workspace"
    );
    Ok(())
}

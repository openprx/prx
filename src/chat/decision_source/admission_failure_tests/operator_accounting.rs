#![cfg(test)]
//! Genuine local operator source and task decision accounting; no OS worker claim.
use super::*;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    routing::post,
};
use futures_util::FutureExt as _;
use serde_json::{Value, json};
use std::{
    panic::AssertUnwindSafe,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

struct NoOperatorAnnouncements;

#[async_trait::async_trait]
impl crate::channels::traits::Channel for NoOperatorAnnouncements {
    fn name(&self) -> &str {
        "terminal"
    }
    async fn send(&self, _: &crate::channels::traits::SendMessage) -> anyhow::Result<()> {
        anyhow::bail!("operator accounting fixture must not send channel messages")
    }
    async fn listen(
        &self,
        _: tokio::sync::mpsc::Sender<crate::channels::traits::ChannelMessage>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("operator accounting fixture must not start a channel listener")
    }
}

async fn decision_reply(State(calls): State<Arc<AtomicUsize>>, Json(body): Json<Value>) -> Json<Value> {
    assert_eq!(body.get("model").and_then(Value::as_str), Some("prx-operator-decision"));
    assert!(
        body.get("questions")
            .and_then(Value::as_object)
            .is_some_and(|q| !q.is_empty())
    );
    calls.fetch_add(1, Ordering::SeqCst);
    // A decoded response with real usage but missing answers must settle its
    // physical request before semantic rejection and truthful builtin fallback.
    // No private wire question IDs are guessed or forged here.
    Json(json!({"model":"prx-operator-decision", "answers":{},
        "usage":{"input_tokens":20,"output_tokens":10}}))
}

#[tokio::test]
async fn actual_operator_task_decision_accounts_physical_send_and_drains() -> anyhow::Result<()> {
    run_operator_case(false).await
}

#[tokio::test]
async fn restored_operator_process_decision_accounts_physical_send_without_os_spawn() -> anyhow::Result<()> {
    run_operator_case(true).await
}

async fn run_operator_case(process: bool) -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let owner = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    let cleanup_scope = crate::runtime::turn_cleanup::CleanupScope::new(Some(&owner), CancellationToken::new())?;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let calls = Arc::new(AtomicUsize::new(0));
    let stop = CancellationToken::new();
    let server_stop = stop.clone();
    let app = Router::new()
        .route("/v1/systemone", post(decision_reply))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(Arc::clone(&calls));
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(server_stop.cancelled_owned())
            .await
    });
    let result = AssertUnwindSafe(cleanup_scope.run(Box::pin(async {
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.config_path = workspace.path().join("config.toml");
        config.causal_tree.enabled = true;
        config.causal_tree.backend.kind =
            crate::causal_tree::config::BackendSelection::Fixed(crate::decision::BackendKind::CompatibleHttp);
        config.causal_tree.backend.endpoint = endpoint.clone();
        config.causal_tree.backend.model = "prx-operator-decision".to_owned();
        assert_eq!(config.causal_tree.policy.extra_latency_budget_ms, 500);
        let priced_provider = format!("custom:{endpoint}");
        config.providers.models.push(serde_json::from_value(json!({
            "provider":priced_provider, "model":"prx-operator-decision", "operations":["decision_endpoint"]
        }))?);
        config.cost.prices.insert(
            format!("{priced_provider}/prx-operator-decision"),
            crate::config::schema::ModelPricing {
                input: 0.1,
                output: 0.2,
                cache_write: 0.3,
                cache_read: 0.1,
            },
        );
        config.providers.models.push(serde_json::from_value(json!({
            "provider": "ollama", "model": "prx-local-model",
            "operations": ["chat"], "native_tools": true
        }))?);
        let selected = crate::providers::selection::check(
            &config,
            "ollama",
            "prx-local-model",
            &crate::providers::catalog::Requirement::chat().with(crate::providers::catalog::Feature::NativeTools),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )?;
        assert_eq!(
            selected.verification(),
            &crate::providers::catalog::Verification::Verified
        );
        config.save().await?;
        let generation = crate::config::new_shared(config).pin();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path())?);
        let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.path().to_string_lossy())
            .with_event_recording(generation.effective.memory.event_recording_config());
        let provider: Arc<dyn crate::providers::Provider> =
            Arc::new(crate::providers::ollama::OllamaProvider::new(None, None));
        let provider_binding = ChatProviderBinding::capture(&generation, &provider, "ollama", "prx-local-model")?
            .context("actual chat provider binding missing")?;
        let security = Arc::new(crate::security::SecurityPolicy::from_config(
            &generation.effective.autonomy,
            workspace.path(),
        ));
        // Publish the same real spawn capability required by the Chat source
        // validator. This fixture issues child authority without executing the
        // tool or launching an OS worker.
        let config = &generation.effective;
        let spawn = crate::tools::SessionsSpawnTool::new(
            Arc::new(NoOperatorAnnouncements),
            Arc::clone(&provider),
            "ollama",
            "prx-local-model",
            0.0,
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
        let tools_handle = spawn.tools_handle();
        let registry: Arc<Vec<Box<dyn crate::tools::Tool>>> = Arc::new(vec![Box::new(spawn)]);
        assert!(tools_handle.set(&registry));
        let runtime = crate::runtime::turn_decision::TurnLoopRuntime {
            hooks: Arc::new(crate::hooks::HookManager::new(workspace.path().to_path_buf())),
            observer: Arc::new(crate::observability::noop::NoopObserver),
        };
        let service = crate::runtime::turn_publication::TurnDecisionPublication {
            generation: Arc::clone(&generation),
            registry: Arc::clone(&registry),
            mcp_tool: None,
            security: Arc::clone(&security),
            observer: Arc::clone(&runtime.observer),
            hooks: Arc::clone(&runtime.hooks),
            memory: Arc::clone(&memory),
            plugin_generation: 0,
        }
        .publish(crate::causal_tree::state::TurnSurfaceKind::Interactive)
        .await?;
        owner.retain_service(Arc::clone(&service));
        let task = "What is 2 plus 2?";
        let message = crate::channels::traits::ChannelMessage {
            id: "prx-operator-command".to_owned(),
            sender: "user".to_owned(),
            channel: "terminal".to_owned(),
            content: format!("/bg {task}"),
            ..Default::default()
        };
        let command = ChatOperatorSpawn::capture(
            &message,
            task,
            &provider_binding,
            &generation,
            &provider,
            "ollama",
            "prx-local-model",
        )?;
        let (args, source) = command
            .admit(
                Arc::clone(&memory),
                "prx_operator_session",
                Arc::clone(&security),
                service,
            )
            .await?;
        source
            .scope(async {
                let claimed =
                    crate::runtime::operator_spawn::claim(&args)?.context("actual operator source missing")?;
                let binding = Arc::new(crate::runtime::child_source::ChildProviderBinding::build_operator(
                    &claimed,
                )?);
                let mut scope = crate::runtime::envelope::RuntimeEnvelope::sessions_spawn(
                    fabric.workspace_id(),
                    "prx_operator_session",
                    "prx_operator_task",
                )
                .message_scope();
                claimed.bind_input_scope(&mut scope)?;
                let receipt = crate::runtime::child_source::record_child_input(
                    &fabric,
                    scope.clone(),
                    task,
                    Some("prx-operator-task-input".to_owned()),
                    None,
                    None,
                )
                .await?;
                assert!(
                    receipt.as_ref().is_some_and(|receipt| receipt.event().id > 0),
                    "operator task input must be actually durable"
                );
                let cancellation = CancellationToken::new();
                let (_task_lifetime, _process_lifetime);
                let (child_service, segment_source) = if process {
                    let (service, source, lifetime) = process_segment(
                        &claimed,
                        &scope,
                        receipt.clone(),
                        task,
                        workspace.path(),
                        registry,
                        (runtime, security),
                    )
                    .await?;
                    _task_lifetime = None;
                    _process_lifetime = Some(lifetime);
                    (service, source)
                } else {
                    let (authority, lifetime) =
                        claimed.issue_task(binding, &scope, task, receipt.as_deref(), &cancellation)?;
                    let service = authority.publication().task_service(Arc::clone(&authority), registry)?;
                    owner.retain_child_service(Arc::clone(&service))?;
                    let mut segments = authority.segments()?;
                    let source = segments.record(task).await?;
                    _task_lifetime = Some(lifetime);
                    _process_lifetime = None;
                    (service, source)
                };
                let turn = crate::runtime::child_turn::ChildSegmentTurn::admit(
                    child_service,
                    segment_source,
                    fabric.clone(),
                    Arc::clone(&generation.effective),
                    &owner,
                    &cancellation,
                    None,
                )
                .await?;
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    1,
                    "configured slot must perform exactly one actual HTTP request"
                );
                assert!(
                    turn.is_close()?,
                    "missing decision answers must fall back to a fixed local close"
                );
                assert!(turn.close(&cancellation).await?.is_some());
                drop(turn);
                let journal = std::fs::read_to_string(workspace.path().join("state/costs.reservations.jsonl"))?;
                let rows: Vec<Value> = journal.lines().map(serde_json::from_str).collect::<Result<_, _>>()?;
                let matching = |event: &str| {
                    rows.iter()
                        .filter(|row| row.get("event").and_then(Value::as_str) == Some(event))
                        .collect::<Vec<_>>()
                };
                let reserved = matching("estimated_reserved_v1");
                let dispatched = matching("dispatched");
                let settled = matching("estimated_settled_v1");
                assert_eq!((reserved.len(), dispatched.len(), settled.len()), (1, 1, 1));
                let attempt = reserved
                    .first()
                    .and_then(|row| row.pointer("/request/reservation/attempt_id"))
                    .and_then(Value::as_str)
                    .context("actual reservation identity missing")?;
                assert!(attempt.starts_with("decision:"));
                assert_eq!(
                    dispatched
                        .first()
                        .and_then(|row| row.get("attempt_id"))
                        .and_then(Value::as_str),
                    Some(attempt)
                );
                assert_eq!(
                    settled
                        .first()
                        .and_then(|row| row.get("attempt_id"))
                        .and_then(Value::as_str),
                    Some(attempt)
                );
                assert_eq!(
                    settled
                        .first()
                        .and_then(|row| row.get("actual_tokens"))
                        .and_then(Value::as_str),
                    Some("30")
                );
                assert_eq!(
                    std::fs::read_to_string(workspace.path().join("state/costs.jsonl"))?
                        .lines()
                        .count(),
                    1
                );
                Ok::<_, anyhow::Error>(())
            })
            .await
    })))
    .catch_unwind();
    let result = tokio::time::timeout(Duration::from_secs(5), result).await;
    let cleanup = tokio::time::timeout(
        Duration::from_secs(5),
        owner.drain(tokio::time::Instant::now() + Duration::from_secs(5)),
    )
    .await;
    stop.cancel();
    let server_cleanup = match tokio::time::timeout(Duration::from_secs(5), &mut server).await {
        Ok(result) => matches!(result, Ok(Ok(()))),
        Err(_) => {
            server.abort();
            let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
            false
        }
    };
    let confirmed = matches!(cleanup, Ok(Ok(()))) && server_cleanup;
    if !confirmed {
        let retained = workspace.keep();
        tracing::error!(path=%retained.display(), "operator accounting fixture cleanup unconfirmed; workspace retained");
    }
    match result {
        Ok(Ok(result)) => {
            anyhow::ensure!(confirmed, "operator accounting cleanup unconfirmed");
            result
        }
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(error) => Err(error.into()),
    }
}

// Test-only composition fragment. Not a frozen candidate or executed test.
async fn restore_operator_process(
    source: &std::sync::Arc<crate::runtime::operator_spawn::OperatorSpawnSource>,
    scope: &crate::memory::MessageEventScope,
    receipt: Option<std::sync::Arc<crate::runtime::child_source::ChildInputReceipt>>,
    task: &str,
    config_dir: &std::path::Path,
) -> anyhow::Result<(
    std::sync::Arc<crate::runtime::child_source::operator_process::OperatorProcessAuthority>,
    crate::runtime::child_source::operator_process::OperatorProcessLifetime,
)> {
    use crate::runtime::operator_spawn::process::{OperatorProcessSnapshots, ProcessSourceLaunch};
    use crate::runtime::turn_source::TurnSourceProof;
    let config = source.generation().effective.as_ref();
    let config_generation = crate::session_worker::protocol::config_source_generation(config_dir)?;
    let launch = source.capture_process(scope, task, receipt, config_generation.clone())?;
    let workspace = &config.workspace_dir;
    let mut manifest = crate::session_worker::protocol::WorkerManifest {
        parent_capability: None,
        capability_manifest: None,
        child_source: None,
        ordinary_source: None,
        operator_source: None,
        run_id: scope
            .run_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing child run"))?,
        task: task.to_owned(),
        provider_name: source.resolved().provider_label().to_owned(),
        model: source.resolved().model().to_owned(),
        api_key: None,
        route_hint: source.route().map(|route| route.hint.clone()),
        route_source_provider: None,
        route_credential: None,
        route_uses_root_connection: false,
        direct_uses_root_connection: false,
        temperature: 0.0,
        config_dir: config_dir.to_path_buf(),
        config_generation,
        runtime_config_generation_id: scope.config_generation_id,
        runtime_config_source_revision: scope.config_source_revision.clone(),
        workspace_dir: workspace.clone(),
        memory_db_path: workspace.join("memory/brain.db"),
        memory_workspace_id: Some(source.proof().workspace_id().to_owned()),
        memory_strategy: Some("shared_fabric".to_owned()),
        memory_backend: "sqlite".to_owned(),
        shared_memory_db_path: Some(workspace.join("memory/brain.db")),
        worker_memory_db_path: None,
        agent_id: scope.agent_id.clone(),
        persona_id: scope.persona_id.clone(),
        memory_event_recording: config.memory.event_recording_config(),
        allowed_tools: Vec::new(),
        channel_excluded_tools: Vec::new(),
        system_prompt: None,
        identity_dir: None,
        scope_sender: scope.sender.clone(),
        scope_channel: scope.channel.clone(),
        scope_chat_type: Some(source.proof().policy_chat_type().to_owned()),
        scope_chat_id: scope.recipient.clone(),
        owner_id: scope.owner_id.clone(),
        owner_isolated: source.proof().owner_isolated(),
        topic_id: None,
        parent_task_id: None,
        source_message_event_id: source.proof().source_event_id().map(str::to_owned),
        spawn_depth: 0,
        session_scope_key: scope
            .session_key
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing child session"))?,
        parent_run_id: scope.parent_run_id.clone(),
        compaction_config: None,
    };
    // This exercises the real process-launch binding producer. The fixture
    // does not claim outer worker-channel authentication or an OS spawn.
    let channel_key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"prx_operator_process_fixture_channel");
    ProcessSourceLaunch::Operator(launch).bind_worker_manifest(&channel_key, &mut manifest)?;
    let key = crate::decision::key::DecisionKey::from_worker_channel(
        &channel_key,
        source.proof().workspace_id(),
        &manifest.run_id,
        &manifest.config_generation,
    )?;
    let snapshots = OperatorProcessSnapshots::restore(&manifest, config.clone(), key)?;
    snapshots
        .issue(
            &manifest,
            source.fabric().clone(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
}

async fn process_segment(
    claimed: &std::sync::Arc<crate::runtime::operator_spawn::OperatorSpawnSource>,
    scope: &crate::memory::MessageEventScope,
    receipt: Option<std::sync::Arc<crate::runtime::child_source::ChildInputReceipt>>,
    task: &str,
    config_dir: &std::path::Path,
    registry: std::sync::Arc<Vec<Box<dyn crate::tools::Tool>>>,
    runtime_security: (
        crate::runtime::turn_decision::TurnLoopRuntime,
        std::sync::Arc<crate::security::SecurityPolicy>,
    ),
) -> anyhow::Result<(
    std::sync::Arc<crate::runtime::turn_decision::TurnDecisionService>,
    (
        std::sync::Arc<crate::runtime::turn_source::OwnedTurnSourceProof>,
        crate::tools::ToolExecutionContext,
    ),
    crate::runtime::child_source::operator_process::OperatorProcessLifetime,
)> {
    let (authority, lifetime) = restore_operator_process(claimed, scope, receipt, task, config_dir).await?;
    let service = crate::runtime::turn_publication::OperatorProcessDecisionPublication {
        authority: std::sync::Arc::clone(&authority),
        registry,
        security: runtime_security.1,
        runtime: runtime_security.0,
        surface_policy: crate::tools::capability::catalog::SurfacePolicy::default(),
    }
    .publish()
    .await?;
    let mut segments = authority.segments()?;
    let source = segments.record(task).await?;
    Ok((service, source, lifetime))
}

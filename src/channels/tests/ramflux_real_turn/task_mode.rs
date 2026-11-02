//! Task-mode delegation over the bridge, native memory tool and host gate.
use super::*;
use crate::tools::sessions_spawn::{SessionsSpawnTool, SubAgentStatus};

struct SpawnParent {
    parent: AtomicUsize,
    child: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    runs: Arc<tokio::sync::RwLock<Vec<crate::tools::sessions_spawn::SubAgentRun>>>,
}
#[async_trait::async_trait]
impl Provider for SpawnParent {
    async fn chat_with_system(&self, _: Option<&str>, message: &str, model: &str, _: f64) -> anyhow::Result<String> {
        self.requests.lock().push((
            model.to_owned(),
            format!(
                "approval={} memory={}: {message}",
                crate::agent::channel_approval::current_installed().is_some(),
                crate::memory::active_view::current().is_some()
            ),
        ));
        if model == "child-scope-model" {
            return Ok(if self.child.fetch_add(1, Ordering::SeqCst) == 0 {
                "<tool_call>\n{\"name\":\"memory_store\",\"arguments\":{\"key\":\"child-owned\",\"content\":\"task child scoped retention positive control\",\"category\":\"core\"}}\n</tool_call>".into()
            } else {
                "child finished".into()
            });
        }
        if message.contains("<tool_result name=\"memory_store\">") {
            return Ok("child finished".into());
        }
        if self.parent.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok("<tool_call>\n{\"name\":\"sessions_spawn\",\"arguments\":{\"action\":\"spawn\",\"mode\":\"task\",\"task\":\"store the child marker\",\"model\":\"child-scope-model\",\"announce\":false}}\n</tool_call>".into())
        } else {
            loop {
                let ready =
                    self.runs.read().await.first().is_some_and(|run| {
                        matches!(run.status, SubAgentStatus::Completed(_) | SubAgentStatus::Failed(_))
                    });
                if ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok("parent finished".into())
        }
    }
}

async fn child_scope_round(supervised: bool) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let channel = Arc::new(
        RamfluxChannel::new(
            RamfluxConfig {
                socket_path: Some(dir.path().join("ramflux.sock")),
                ..RamfluxConfig::default()
            },
            dir.path(),
        )
        .unwrap(),
    );
    let memory = Arc::new(crate::memory::SqliteMemory::new(dir.path()).unwrap());
    let provider_name = format!("custom:http://127.0.0.1/prx-task-scope-{}", uuid::Uuid::new_v4());
    let mut config = crate::config::Config {
        workspace_dir: dir.path().to_path_buf(),
        config_path: dir.path().join("config.toml"),
        default_provider: Some(provider_name.clone()),
        default_model: Some("parent-scope-model".into()),
        ..crate::config::Config::default()
    };
    config.autonomy.level = crate::security::AutonomyLevel::Full;
    config.reliability.provider_retries = 0;
    config.tool_tiering.always_include = vec!["sessions_spawn".into(), "memory_store".into()];
    for model in ["parent-scope-model", "child-scope-model"] {
        config.cost.prices.insert(
            format!("{provider_name}/{model}"),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_read: 1.0,
                cache_write: 1.0,
            },
        );
        config.providers.models.push(
            serde_json::from_value(
                json!({"provider":provider_name,"model":model,"operations":["chat"],"native_tools":true}),
            )
            .unwrap(),
        );
    }
    let security = Arc::new(crate::security::SecurityPolicy::from_config(
        &config.autonomy,
        dir.path(),
    ));
    let spawn = SessionsSpawnTool::new(
        channel.clone(),
        &provider_name,
        "parent-scope-model",
        0.0,
        security.clone(),
        dir.path().to_path_buf(),
        config.multimodal.clone(),
        HashMap::new(),
        None,
        crate::providers::provider_runtime_options_from_config(&config),
        config.sessions_spawn.clone(),
    )
    .with_configured_providers(&crate::providers::ConfiguredProviders::from_config(&config))
    .with_model_catalog(crate::providers::selection::catalog_for(&config))
    .with_hint_config(&config)
    .with_shared_memory(memory.clone());
    let tools_handle = spawn.tools_handle();
    let runs = spawn.active_runs_arc();
    let child_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(SpawnParent {
        parent: AtomicUsize::new(0),
        child: child_calls.clone(),
        requests: requests.clone(),
        runs: runs.clone(),
    });
    crate::providers::test_fixture::register(&provider_name, parent.clone());
    let mut ctx = runtime(
        dir.path(),
        channel.clone(),
        parent,
        vec![
            Box::new(spawn),
            Box::new(crate::tools::MemoryStoreTool::new(memory.clone(), security)),
        ],
        memory.clone(),
        crate::config::ToolTieringConfig {
            always_include: vec!["sessions_spawn".into(), "memory_store".into()],
            ..crate::config::ToolTieringConfig::default()
        },
    );
    assert!(tools_handle.set(&ctx.tools_registry));
    let mutable = Arc::get_mut(&mut ctx).unwrap();
    mutable.default_provider = Arc::new(provider_name);
    mutable.model = Arc::new("parent-scope-model".into());
    mutable.config = crate::config::new_shared(config);
    mutable.config_generation = mutable.config.pin();
    channel.start().await.unwrap();
    spawn_turns(channel.clone(), ctx);
    let mut host = Host::connect(channel.socket_path()).await;
    hello(&mut host, if supervised { "supervised" } else { "full" }).await;
    host.send(&inbound_message(
        210,
        "conv-1",
        "child-source",
        "use sessions_spawn for a task-mode child that calls memory_store",
        1,
    ))
    .await;
    let mut child_approval = false;
    let mut parent_done = false;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            tokio::select! {
                frame = host.recv() => {
                    let method = frame["method"].as_str().unwrap_or("");
                    if method == "approval.request" {
                        let is_child = frame["params"]["tool_name"] == "memory_store";
                        if is_child {
                            child_approval = true;
                            assert_eq!(frame["params"]["conversation_id"], "conv-1");
                            assert!(memory.get("child-owned").await.unwrap().is_none(), "child ran before approval");
                        }
                        ack(&mut host,&frame).await;
                        host.send(&json!({"jsonrpc":"2.0","id":211,"method":"approval.decision","params":{
                            "conversation_id":"conv-1","approval_id":frame["params"]["approval_id"],
                            "decision":if is_child { "denied" } else { "approved" },"decided_by":sender(),"decided_at_ms":3}})).await;
                    } else if !method.is_empty() {
                        if method == "turn.status" && frame["params"]["state"] == "done" { parent_done = true; }
                        ack(&mut host,&frame).await;
                    }
                }
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
            let snapshot = runs.read().await;
            if let Some(run) = snapshot.first() {
                if let SubAgentStatus::Failed(error) = &run.status { panic!("test: task child failed: {error}; logs: {}",recent_logs()); }
                if parent_done && matches!(run.status,SubAgentStatus::Completed(_)) { break; }
            }
        }
    }).await.unwrap_or_else(|_| panic!("test: real parent and task child must finish; requests={:?}; runs={:?}; logs={}", requests.lock(), runs.try_read().map(|runs| runs.iter().map(|run| run.status.clone()).collect::<Vec<_>>()), recent_logs()));
    let child_status = runs.read().await.first().map(|run| format!("{:?}", run.status));
    assert!(
        child_calls.load(Ordering::SeqCst) > 0,
        "child must call its admitted provider: {:?}; child status: {:?}; logs: {}",
        requests.lock(),
        child_status,
        recent_logs()
    );
    let scoped = RichChannel::memory_for_turn(channel.as_ref(), memory.clone(), "conv-1");
    if supervised {
        assert!(
            child_approval,
            "task-mode child bypassed host approval; requests={:?}; logs={}",
            requests.lock(),
            recent_logs()
        );
        assert!(scoped.get("child-owned").await.unwrap().is_none());
    } else {
        assert!(
            scoped.get("child-owned").await.unwrap().is_some(),
            "task-mode tool lost scoped memory; requests={:?}; logs={}",
            requests.lock(),
            recent_logs()
        );
        assert!(
            memory.get("child-owned").await.unwrap().is_none(),
            "unscoped tool write escaped"
        );
        host.send(
            &json!({"jsonrpc":"2.0","id":212,"method":"inbound.message_deleted","params":{
            "conversation_id":"conv-1","message_id":"child-source","deleted_by":sender(),"timestamp_ms":4}}),
        )
        .await;
        loop {
            let frame = host.recv().await;
            if frame["id"] == 212 {
                assert_eq!(frame["result"]["erased"], true, "{frame}");
                break;
            }
            if frame.get("method").is_some() {
                ack(&mut host, &frame).await;
            }
        }
        assert!(
            scoped.get("child-owned").await.unwrap().is_none(),
            "deleted parent retained child memory"
        );
    }
    assert!(memory.get("child-owned").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r5_task_mode_memory_store_is_scoped_indexed_and_erased() {
    child_scope_round(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r5_task_mode_supervised_tool_uses_the_host_approval_gate() {
    child_scope_round(true).await;
}

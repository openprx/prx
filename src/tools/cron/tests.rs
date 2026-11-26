use super::*;
use crate::config::{Config, SharedConfig, new_shared};
use crate::security::AutonomyLevel;
use crate::tools::schema::validate_tool_arguments;
use tempfile::TempDir;

/// Every action that cannot be called without a parameter has to say so in
/// that parameter's own description.
///
/// `cron` lost the convention silently once: the quotes around the action
/// names were dropped during a schema compression and no test noticed,
/// because the contract lived only inside `sessions_spawn`.
#[tokio::test]
async fn schema_names_every_action_in_the_parameters_it_requires() {
    let config = new_shared(Config::default());
    let snapshot = config.load_full();
    let tool = CronTool::new(Arc::clone(&config), test_security(&snapshot));
    crate::tools::schema::assert_flat_execution_contract(&tool, tool.execution_schema()).await;
    crate::tools::schema::action_contract::assert_action_tool_contract(
        "cron",
        tool.description(),
        tool.execution_schema(),
    );
}

#[test]
fn schema_requires_a_schedule_and_executable_content() {
    let config = new_shared(Config::default());
    let snapshot = config.load_full();
    let tool = CronTool::new(Arc::clone(&config), test_security(&snapshot));
    let schema = tool.execution_schema();

    assert!(
        !validate_tool_arguments(schema, &json!({"action": "add"})).is_empty(),
        "expected a non-empty value"
    );
    assert!(
        !validate_tool_arguments(schema, &json!({"action": "add", "expression": "0 9 * * *"})).is_empty(),
        "expected a non-empty value"
    );
    assert!(
        validate_tool_arguments(
            schema,
            &json!({"action": "add", "expression": "0 9 * * *", "command": "date"})
        )
        .is_empty(),
        "expected an empty value"
    );
    assert!(
        validate_tool_arguments(
            schema,
            &json!({
                "action": "schedule",
                "schedule": {"kind": "every", "every_ms": 1000},
                "payload": {"kind": "agentTurn", "message": "inspect state"}
            })
        )
        .is_empty(),
        "expected an empty value"
    );
    assert!(
        !validate_tool_arguments(
            schema,
            &json!({
                "action": "schedule",
                "schedule": {"kind": "cron"},
                "payload": {"kind": "agentTurn", "message": "inspect state"}
            })
        )
        .is_empty(),
        "expected a non-empty value"
    );
    assert!(
        !validate_tool_arguments(schema, &json!({"action": "once", "command": "date"})).is_empty(),
        "expected a non-empty value"
    );
    assert!(
        validate_tool_arguments(schema, &json!({"action": "once", "command": "date", "delay": "5m"})).is_empty(),
        "expected an empty value"
    );
    assert!(
        validate_tool_arguments(
            schema,
            &json!({"action": "once", "command": "date", "run_at": "2030-01-01T00:00:00Z"})
        )
        .is_empty(),
        "expected an empty value"
    );
    assert!(
        !validate_tool_arguments(
            schema,
            &json!({
                "action": "once",
                "command": "date",
                "delay": "5m",
                "run_at": "2030-01-01T00:00:00Z"
            })
        )
        .is_empty(),
        "expected a non-empty value"
    );
}

async fn test_config(tmp: &TempDir) -> SharedConfig {
    let config = Config {
        workspace_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..Config::default()
    };
    tokio::fs::create_dir_all(&config.workspace_dir).await.unwrap();
    new_shared(config)
}

fn test_security(cfg: &Config) -> Arc<SecurityPolicy> {
    Arc::new(SecurityPolicy::from_config(&cfg.autonomy, &cfg.workspace_dir))
}

#[tokio::test]
async fn list_empty() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let result = tool.execute(json!({"action": "list"})).await.unwrap();
    assert!(result.success);
    assert!(result.output.contains("No scheduled cron jobs"));
}

#[tokio::test]
async fn add_and_list_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let add = tool
        .execute(json!({"action": "add", "expression": "*/5 * * * *", "command": "echo hi"}))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);

    let list = tool.execute(json!({"action": "list"})).await.unwrap();
    assert!(list.success);
    assert!(list.output.contains("echo hi"));
}

#[tokio::test]
async fn add_with_schedule_object_creates_shell_job() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
            "job_type": "shell",
            "command": "echo sched-obj"
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);

    let list = tool.execute(json!({"action": "list"})).await.unwrap();
    assert!(list.output.contains("echo sched-obj"));
}

#[tokio::test]
async fn add_agent_job_via_payload() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "payload": { "kind": "agentTurn", "message": "summarize my day" }
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);

    let jobs = cron::list_jobs(&cfg_snap).unwrap();
    assert_eq!(jobs.len(), 1);
    let job = jobs.first().expect("test: one job present");
    assert!(matches!(job.job_type, JobType::Agent));
}

#[tokio::test]
async fn add_agent_job_requires_prompt() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let result = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
            "job_type": "agent"
        }))
        .await
        .unwrap_err();
    let typed = result
        .downcast_ref::<crate::tools::schema::ToolArgumentError>()
        .unwrap();
    assert!(
        typed
            .issues
            .iter()
            .any(|issue| issue.path == "$" && issue.message.contains("required-field set"))
    );
    assert!(cron::list_jobs(&cfg_snap).unwrap().is_empty());
    let valid = tool
        .execute(json!({
            "action": "add", "schedule": {"kind": "cron", "expr": "*/5 * * * *"},
            "job_type": "agent", "prompt": "fixture task"
        }))
        .await
        .unwrap();
    assert!(valid.success, "{:?}", valid.error);
    assert_eq!(cron::list_jobs(&cfg_snap).unwrap().len(), 1);
}

#[tokio::test]
async fn status_reports_job_counts() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    cron::add_job(&cfg_snap, "*/5 * * * *", "echo status-test").unwrap();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let result = tool.execute(json!({"action": "status"})).await.unwrap();
    assert!(result.success);
    assert!(result.output.contains("1 total"));
}

/// INT2C-N3 (R-INT2C N-3): an owner-scoped read of a job row that cannot
/// be decoded answers `not found`, which is right for the caller, but the
/// cause was logged at `debug!` and so was invisible by default. It is a
/// warning now, naming the job; a job that simply does not exist stays
/// quiet.
#[tokio::test]
async fn an_unreadable_job_row_is_logged_at_warn_for_an_owner_scoped_read() {
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
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let alice = json!({ "sender": "alice", "channel": "telegram", "chat_type": "direct" });
    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "job_type": "agent",
            "prompt": "brief me",
            "_zc_scope_trusted": true,
            "_zc_scope": alice
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);
    let job_id = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .next()
        .expect("test: one job")
        .id;
    let db = cfg_snap.workspace_dir.join("cron").join("jobs.db");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE cron_jobs SET next_run = 'not-a-timestamp' WHERE id = ?1",
            [&job_id],
        )
        .unwrap();

    let read_args = json!({ "_zc_scope_trusted": true, "_zc_scope": alice });
    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let (unreadable, missing) = tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        (
            CronTool::hide_unless_caller_may_read(&cfg_snap, &read_args, &job_id),
            CronTool::hide_unless_caller_may_read(&cfg_snap, &read_args, "no-such-job"),
        )
    });
    let unreadable = unreadable.expect("test: an unreadable job reads as not found");
    assert_eq!(
        unreadable.error.as_deref(),
        Some(format!("Job '{job_id}' not found").as_str())
    );
    assert!(missing.is_some(), "a missing job reads as not found");

    let logs = String::from_utf8_lossy(&sink.0.lock()).to_string();
    let warned = logs
        .lines()
        .filter(|line| line.contains("WARN") && line.contains(job_id.as_str()))
        .count();
    assert_eq!(warned, 1, "one WARN naming the unreadable job: {logs}");
    assert!(
        !logs
            .lines()
            .any(|line| line.contains("WARN") && line.contains("no-such-job")),
        "a job that does not exist is not a warning: {logs}"
    );
}

#[tokio::test]
async fn get_job_details() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let job = cron::add_job(&cfg_snap, "*/5 * * * *", "echo get-test").unwrap();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let result = tool.execute(json!({"action": "get", "job_id": job.id})).await.unwrap();
    assert!(result.success);
    assert!(result.output.contains("echo get-test"));
}

#[tokio::test]
async fn read_surfaces_expose_claim_and_attempt_identity() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let job = cron::add_job(&cfg_snap, "*/5 * * * *", "echo claimed").unwrap();
    let now = Utc::now();
    let claim = cron::claim_job_if_current_for_manual_run(
        &cfg_snap,
        &job,
        "worker-visible",
        now,
        ChronoDuration::seconds(90),
    )
    .unwrap()
    .unwrap();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let list = tool.execute(json!({"action": "list"})).await.unwrap();
    assert!(list.output.contains("claimed_by=worker-visible"));
    assert!(list.output.contains(&claim.attempt_id));
    assert!(list.output.contains(&claim.expires_at.to_rfc3339()));

    let get = tool.execute(json!({"action": "get", "job_id": job.id})).await.unwrap();
    let detail: serde_json::Value = serde_json::from_str(&get.output).unwrap();
    assert_eq!(detail.pointer("/claim/worker_id"), Some(&json!("worker-visible")));
    assert_eq!(
        detail.pointer("/claim/attempt_id").and_then(serde_json::Value::as_str),
        Some(claim.attempt_id.as_str())
    );

    let status = tool.execute(json!({"action": "status"})).await.unwrap();
    assert!(status.output.contains("1 claimed"));

    cron::finish_claimed_run(&cfg_snap, &job, &claim, now, now, now, true, "ok", 0, false).unwrap();
    let runs = tool.execute(json!({"action": "runs", "job_id": job.id})).await.unwrap();
    let history: serde_json::Value = serde_json::from_str(&runs.output).unwrap();
    let first_run = history.as_array().and_then(|runs| runs.first());
    assert_eq!(
        first_run
            .and_then(|run| run.get("attempt_id"))
            .and_then(serde_json::Value::as_str),
        Some(claim.attempt_id.as_str())
    );
    assert_eq!(
        first_run
            .and_then(|run| run.get("worker_id"))
            .and_then(serde_json::Value::as_str),
        Some("worker-visible")
    );
}

#[tokio::test]
async fn events_lists_job_lifecycle_events() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let job = cron::add_job(&cfg_snap, "*/5 * * * *", "echo events-test").unwrap();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let result = tool
        .execute(json!({"action": "events", "job_id": job.id}))
        .await
        .unwrap();

    assert!(result.success, "{:?}", result.error);
    assert!(result.output.contains("cron.job.created"));
}

#[tokio::test]
async fn remove_job() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let job = cron::add_job(&cfg_snap, "*/5 * * * *", "echo remove-test").unwrap();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let result = tool
        .execute(json!({"action": "remove", "job_id": job.id}))
        .await
        .unwrap();
    assert!(result.success);
    assert!(cron::list_jobs(&cfg_snap).unwrap().is_empty());
}

#[tokio::test]
async fn update_job_enabled() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let job = cron::add_job(&cfg_snap, "*/5 * * * *", "echo upd-test").unwrap();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let result = tool
        .execute(json!({"action": "update", "job_id": job.id, "patch": {"enabled": false}}))
        .await
        .unwrap();
    assert!(result.success, "{:?}", result.error);
    assert!(result.output.contains("\"enabled\": false"));
}

#[tokio::test]
async fn cron_is_always_available() {
    let tmp = TempDir::new().unwrap();
    let config = Config {
        workspace_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..Config::default()
    };
    std::fs::create_dir_all(&config.workspace_dir).unwrap();
    let cfg_snap = Arc::new(config.clone());
    let cfg = new_shared(config);
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let result = tool.execute(json!({"action": "list"})).await.unwrap();
    assert!(result.success, "{:?}", result.error);
}

#[tokio::test]
async fn readonly_blocks_add() {
    let tmp = TempDir::new().unwrap();
    let mut config = Config {
        workspace_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..Config::default()
    };
    config.autonomy.level = AutonomyLevel::ReadOnly;
    std::fs::create_dir_all(&config.workspace_dir).unwrap();
    let cfg_snap = Arc::new(config.clone());
    let cfg = new_shared(config);
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let result = tool
        .execute(json!({"action": "add", "expression": "*/5 * * * *", "command": "echo x"}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.unwrap_or_default().contains("read-only"));
}

#[tokio::test]
async fn unknown_action_returns_error() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let error = tool.execute(json!({"action": "explode"})).await.unwrap_err();
    let typed = error.downcast_ref::<crate::tools::schema::ToolArgumentError>().unwrap();
    assert!(typed.issues.iter().any(|issue| issue.path == "$.action"));
    assert!(Arc::ptr_eq(&cfg_snap, &cfg.load_full()));
    assert!(cron::list_jobs(&cfg_snap).unwrap().is_empty());
    assert!(tool.execute(json!({"action":"list"})).await.unwrap().success);
}

// ── Focused agent-add coverage (fields easily lost in the consolidation) ──

/// `payload.kind=systemEvent` must route to the MAIN session (vs agentTurn's
/// isolated default), and `payload.text` must be accepted as the prompt.
#[tokio::test]
async fn add_agent_system_event_targets_main_and_uses_text() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "payload": { "kind": "systemEvent", "text": "daily standup ping" }
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);

    let job = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .next()
        .expect("test: one job");
    assert!(matches!(job.job_type, JobType::Agent));
    assert_eq!(job.session_target, SessionTarget::Main);
    assert_eq!(job.prompt.as_deref(), Some("daily standup ping"));
}

/// agentTurn defaults to the isolated session and accepts `payload.message`.
#[tokio::test]
async fn add_agent_turn_defaults_isolated() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "payload": { "kind": "agentTurn", "message": "summarize inbox" }
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);

    let job = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .next()
        .expect("test: one job");
    assert_eq!(job.session_target, SessionTarget::Isolated);
    assert_eq!(job.prompt.as_deref(), Some("summarize inbox"));
}

/// `payload.message` takes precedence over the top-level `prompt` field.
#[tokio::test]
async fn add_agent_payload_message_overrides_prompt_field() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "payload": { "kind": "agentTurn", "message": "from-payload" },
            "prompt": "from-prompt-field"
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);

    let job = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .next()
        .expect("test: one job");
    assert_eq!(job.prompt.as_deref(), Some("from-payload"));
}

/// The creating turn's trusted scope becomes the job's delivery principal.
///
/// MUTATION GUARD: this is the only place a cron announcement's
/// authorization subject can be captured — the turn is over by the time the
/// job fires. Dropping the capture leaves every scheduled job anonymous,
/// which the scheduler then treats as the least privileged caller.
#[tokio::test]
async fn add_captures_the_creating_turns_trusted_scope_as_the_delivery_principal() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "job_type": "agent",
            "prompt": "brief me",
            "delivery": { "mode": "announce", "channel": "signal", "to": "+15551234567" },
            "_zc_scope_trusted": true,
            "_zc_scope": { "sender": "alice", "channel": "telegram", "chat_type": "direct" }
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);

    let job = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .next()
        .expect("test: one job");
    assert_eq!(job.delivery_principal.sender(), "alice");
    assert_eq!(job.delivery_principal.channel(), "telegram");
    assert_eq!(job.delivery_principal.chat_type(), "direct");
}

/// A scope the runtime did not vouch for proves nothing, so a model that
/// writes its own `_zc_scope` gets an anonymous principal — not the
/// identity it claimed.
#[tokio::test]
async fn an_untrusted_scope_cannot_claim_a_delivery_principal() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "job_type": "agent",
            "prompt": "brief me",
            "delivery": { "mode": "announce", "channel": "signal", "to": "+15551234567" },
            "_zc_scope": { "sender": "root", "channel": "signal", "chat_type": "direct" }
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);

    let job = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .next()
        .expect("test: one job");
    assert!(
        job.delivery_principal.is_anonymous(),
        "test: an unvouched scope must not become an authorization subject"
    );
}

/// Rewriting `delivery` re-anchors the principal onto the caller doing the
/// rewrite; a patch that leaves `delivery` alone leaves the identity alone.
/// Since CRON-ORIGIN only the creator or the local operator may rewrite a
/// job, so the rewriting caller here is the local operator and an
/// unrelated principal is refused outright.
#[tokio::test]
async fn patching_delivery_re_anchors_the_principal_onto_the_updating_caller() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let operator_scope = json!({
        "sender": "agent", "channel": "cli", "chat_type": "private",
        "chat_id": "agent:local", "runtime_source": "chat"
    });

    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "job_type": "agent",
            "prompt": "brief me",
            "_zc_scope_trusted": true,
            "_zc_scope": { "sender": "alice", "channel": "telegram", "chat_type": "direct" }
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);
    let job_id = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .next()
        .expect("test: one job")
        .id;

    let stranger = tool
        .execute(json!({
            "action": "update",
            "job_id": job_id,
            "patch": { "delivery": { "mode": "announce", "channel": "signal", "to": "+15550009999" } },
            "_zc_scope_trusted": true,
            "_zc_scope": { "sender": "bob", "channel": "signal", "chat_type": "direct" }
        }))
        .await
        .unwrap();
    assert!(!stranger.success, "another principal may not rewrite alice's job");

    let unrelated = tool
        .execute(json!({
            "action": "update",
            "job_id": job_id,
            "patch": { "enabled": false },
            "_zc_scope_trusted": true,
            "_zc_scope": operator_scope
        }))
        .await
        .unwrap();
    assert!(unrelated.success, "{:?}", unrelated.error);
    assert_eq!(
        cron::get_job(&cfg_snap, &job_id).unwrap().delivery_principal.sender(),
        "alice",
        "test: a patch that does not touch delivery must not move the identity"
    );

    let rewritten = tool
        .execute(json!({
            "action": "update",
            "job_id": job_id,
            "patch": { "delivery": { "mode": "announce", "channel": "signal", "to": "+15550009999" } },
            "_zc_scope_trusted": true,
            "_zc_scope": operator_scope
        }))
        .await
        .unwrap();
    assert!(rewritten.success, "{:?}", rewritten.error);
    let job = cron::get_job(&cfg_snap, &job_id).unwrap();
    assert_eq!(
        job.delivery_principal.sender(),
        "agent",
        "test: whoever writes the destination is who the delivery is authorized as"
    );
    assert_eq!(job.delivery_principal.channel(), "cli");
}

/// A test configuration in which `model` is a declared chat model of the
/// default provider, so a model-created job may select it (D-035).
async fn test_config_declaring(tmp: &TempDir, model: &str) -> SharedConfig {
    let mut config = Config {
        workspace_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..Config::default()
    };
    config.providers.models.push(
        toml::from_str(&format!(
            "provider = \"openrouter\"\nmodel = \"{model}\"\noperations = [\"chat\"]\n"
        ))
        .unwrap(),
    );
    tokio::fs::create_dir_all(&config.workspace_dir).await.unwrap();
    new_shared(config)
}

/// D-035 / R-PMMCf R-5: a job the model creates is a model-made durable
/// selection, so its `model` must be verified: an id-only model is
/// refused and no job is written.
#[tokio::test]
async fn add_agent_refuses_an_unverified_model() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "job_type": "agent",
            "prompt": "brief me",
            "model": "vendor/never-seen"
        }))
        .await
        .unwrap();
    assert!(!add.success, "a model-created job selected an unverified model");
    assert!(add.error.unwrap_or_default().contains("not verified"));
    assert!(cron::list_jobs(&cfg_snap).unwrap().is_empty(), "no job is written");
}

/// R-PMMCg C-1: `update`/`patch` with `patch.model` is the same
/// model-made durable selection as `add`: an unverified model is refused
/// and the stored job keeps its model; a verified one is accepted.
#[tokio::test]
async fn update_refuses_an_unverified_model() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config_declaring(&tmp, "claude-test-model").await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "job_type": "agent",
            "prompt": "brief me",
            "model": "claude-test-model"
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);
    let job_id = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .next()
        .expect("test: one job")
        .id;

    for action in ["update", "patch"] {
        let refused = tool
            .execute(json!({
                "action": action,
                "job_id": job_id,
                "patch": { "model": "vendor/never-seen" }
            }))
            .await
            .unwrap();
        assert!(!refused.success, "{action} stored an unverified model");
        assert!(refused.error.unwrap_or_default().contains("not verified"));
        let job = cron::get_job(&cfg_snap, &job_id).unwrap();
        assert_eq!(job.model.as_deref(), Some("claude-test-model"));
    }

    let accepted = tool
        .execute(json!({
            "action": "update",
            "job_id": job_id,
            "patch": { "model": "claude-test-model", "prompt": "brief me again" }
        }))
        .await
        .unwrap();
    assert!(accepted.success, "{:?}", accepted.error);
    assert_eq!(
        cron::get_job(&cfg_snap, &job_id).unwrap().prompt.as_deref(),
        Some("brief me again")
    );
}

/// R-PMMCh C-2: the model a job stores is the trimmed name the check
/// resolved, on `add` and on `update`/`patch`.
#[tokio::test]
async fn a_job_stores_its_model_trimmed() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config_declaring(&tmp, "claude-test-model").await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "job_type": "agent",
            "prompt": "brief me",
            "model": "  claude-test-model \t"
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);
    let job = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .next()
        .expect("test: one job");
    assert_eq!(job.model.as_deref(), Some("claude-test-model"));

    let patched = tool
        .execute(json!({
            "action": "patch",
            "job_id": job.id,
            "patch": { "model": " claude-test-model " }
        }))
        .await
        .unwrap();
    assert!(patched.success, "{:?}", patched.error);
    assert_eq!(
        cron::get_job(&cfg_snap, &job.id).unwrap().model.as_deref(),
        Some("claude-test-model")
    );
}

/// `prompt` is used when no `payload` is given (job_type=agent), and `name`,
/// `model` and `delivery` are all carried through onto the persisted job.
#[tokio::test]
async fn add_agent_named_with_model_and_delivery() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config_declaring(&tmp, "claude-test-model").await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "job_type": "agent",
            "name": "morning-brief",
            "prompt": "brief me",
            "model": "claude-test-model",
            "delivery": { "mode": "announce", "channel": "signal", "to": "+15551234567" }
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);

    let job = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .next()
        .expect("test: one job");
    assert_eq!(job.name.as_deref(), Some("morning-brief"));
    assert_eq!(job.prompt.as_deref(), Some("brief me"));
    assert_eq!(job.model.as_deref(), Some("claude-test-model"));
    assert_eq!(job.delivery.mode, "announce");
    assert_eq!(job.delivery.channel.as_deref(), Some("signal"));
    assert_eq!(job.delivery.to.as_deref(), Some("+15551234567"));
}

/// A malformed `delivery` object must surface a parse error and create no job.
#[tokio::test]
async fn add_agent_invalid_delivery_errors() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let result = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "job_type": "agent",
            "prompt": "x",
            // `mode` must be a string; an integer fails DeliveryConfig parsing.
            "delivery": { "mode": 123 }
        }))
        .await
        .unwrap_err();
    let typed = result
        .downcast_ref::<crate::tools::schema::ToolArgumentError>()
        .unwrap();
    assert!(typed.issues.iter().any(|issue| issue.path == "$.delivery.mode"));
    assert!(cron::list_jobs(&cfg_snap).unwrap().is_empty());
    let valid = tool
        .execute(json!({
            "action": "add", "schedule": {"kind": "cron", "expr": "0 9 * * *"},
            "job_type": "agent", "prompt": "x", "delivery": {"mode": "none"}
        }))
        .await
        .unwrap();
    assert!(valid.success, "{:?}", valid.error);
    assert_eq!(cron::list_jobs(&cfg_snap).unwrap().len(), 1);
}

/// `delete_after_run` defaults to false for recurring (cron) agent jobs.
#[tokio::test]
async fn add_agent_delete_after_run_defaults_false_for_cron() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    tool.execute(json!({
        "action": "add",
        "schedule": { "kind": "cron", "expr": "0 9 * * *" },
        "job_type": "agent",
        "prompt": "recurring"
    }))
    .await
    .unwrap();

    let job = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .next()
        .expect("test: one job");
    assert!(!job.delete_after_run);
}

/// An explicit `delete_after_run=true` overrides the default.
#[tokio::test]
async fn add_agent_delete_after_run_explicit_true() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    tool.execute(json!({
        "action": "add",
        "schedule": { "kind": "cron", "expr": "0 9 * * *" },
        "job_type": "agent",
        "prompt": "recurring",
        "delete_after_run": true
    }))
    .await
    .unwrap();

    let job = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .next()
        .expect("test: one job");
    assert!(job.delete_after_run);
}

/// `Schedule::At` agent jobs persist the `At` schedule and default
/// `delete_after_run` to true (one-shot semantics).
#[tokio::test]
async fn add_agent_schedule_at_one_shot_defaults() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let at = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "at", "at": at },
            "job_type": "agent",
            "prompt": "once-at"
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);

    let job = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .next()
        .expect("test: one job");
    assert!(matches!(job.schedule, Schedule::At { .. }));
    assert!(job.delete_after_run, "At-scheduled agent jobs are one-shot by default");
}

#[tokio::test]
async fn add_shell_schedule_at_persists_explicit_delete_after_run() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let at = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();

    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "at", "at": at },
            "job_type": "shell",
            "command": "echo once",
            "delete_after_run": false
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);

    let job = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .next()
        .expect("test: one job");
    assert!(matches!(job.schedule, Schedule::At { .. }));
    assert!(!job.delete_after_run);
}

#[tokio::test]
async fn force_run_consumes_nonterminal_at_but_allows_terminal_manual_rerun() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let at = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "at", "at": at },
            "job_type": "shell",
            "command": "echo force-once",
            "delete_after_run": false
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);
    let job = cron::list_jobs(&cfg_snap).unwrap().remove(0);

    let first = tool.execute(json!({"action": "run", "job_id": job.id})).await.unwrap();
    assert!(first.success, "{:?}", first.error);
    let terminal = cron::get_job(&cfg_snap, &job.id).unwrap();
    assert_eq!(terminal.terminal_state, Some(cron::CronJobTerminalState::Succeeded));

    let second = tool.execute(json!({"action": "run", "job_id": job.id})).await.unwrap();
    assert!(second.success, "{:?}", second.error);
    let rerun = cron::get_job(&cfg_snap, &job.id).unwrap();
    assert_eq!(rerun.terminal_state, Some(cron::CronJobTerminalState::Succeeded));
    assert_eq!(cron::list_runs(&cfg_snap, &job.id, 10).unwrap().len(), 2);
}

#[tokio::test]
async fn failed_force_run_retains_auto_delete_at_audit() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let at = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "at", "at": at },
            "job_type": "shell",
            "command": "false"
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);
    let job = cron::list_jobs(&cfg_snap).unwrap().remove(0);

    let run = tool.execute(json!({"action": "run", "job_id": job.id})).await.unwrap();
    assert!(!run.success);
    let retained = cron::get_job(&cfg_snap, &job.id).unwrap();
    assert_eq!(retained.terminal_state, Some(cron::CronJobTerminalState::Failed));
    assert_eq!(cron::list_runs(&cfg_snap, &job.id, 10).unwrap().len(), 1);
}

#[tokio::test]
async fn terminal_rerun_claim_conflict_does_not_execute_side_effect() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let marker = cfg_snap.workspace_dir.join("must-not-exist");
    let command = "touch must-not-exist";
    let at = Utc::now() + chrono::Duration::hours(1);
    let job = cron::add_shell_job(
        &cfg_snap,
        Some("terminal-conflict".into()),
        Schedule::At { at },
        command,
    )
    .unwrap();
    let now = Utc::now();
    let initial = cron::claim_job_if_current_for_manual_run(
        &cfg_snap,
        &job,
        "initial-terminalizer",
        now,
        ChronoDuration::seconds(90),
    )
    .unwrap()
    .unwrap();
    cron::finish_claimed_run(
        &cfg_snap,
        &job,
        &initial,
        now,
        now,
        now,
        true,
        "seed terminal",
        0,
        false,
    )
    .unwrap();
    let terminal = cron::get_job(&cfg_snap, &job.id).unwrap();
    cron::claim_terminal_job_for_manual_rerun(
        &cfg_snap,
        &terminal,
        "blocking-rerun",
        now,
        ChronoDuration::seconds(90),
    )
    .unwrap()
    .unwrap();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let run = tool.execute(json!({"action": "run", "job_id": job.id})).await.unwrap();

    assert!(!run.success);
    assert!(!marker.exists(), "claim rejection must happen before command execution");
}

#[tokio::test]
async fn manual_run_claim_conflict_leaves_the_job_runnable_after_release() {
    let tmp = TempDir::new().unwrap();
    let mut config = Config {
        workspace_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..Config::default()
    };
    config.autonomy.level = AutonomyLevel::Full;
    tokio::fs::create_dir_all(&config.workspace_dir).await.unwrap();
    let cfg_snap = Arc::new(config.clone());
    let cfg = new_shared(config);
    let job = cron::add_job(&cfg_snap, "*/5 * * * *", "echo budget-preserved").unwrap();
    let blocking_claim = cron::claim_job_if_current_for_manual_run(
        &cfg_snap,
        &job,
        "blocking-manual-run",
        Utc::now(),
        ChronoDuration::seconds(90),
    )
    .unwrap()
    .unwrap();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let conflict = tool.execute(json!({"action": "run", "job_id": job.id})).await.unwrap();
    assert!(!conflict.success);
    assert!(conflict.error.unwrap_or_default().contains("already running"));

    assert!(
        cron::abandon_job_claim(
            &cfg_snap,
            &job.id,
            &blocking_claim,
            job.last_status.as_deref(),
            "test releases blocking manual claim",
        )
        .unwrap()
    );
    let retry = tool.execute(json!({"action": "run", "job_id": job.id})).await.unwrap();
    assert!(
        retry.success,
        "releasing the blocking claim must make the job runnable again: {:?}",
        retry.error
    );
}

#[tokio::test]
async fn manual_run_claim_conflict_does_not_consume_single_use_grant() {
    use crate::acl::approval_grant::{ApprovalGrantV2, IssuerAuthority, RiskLevel, Subject, WitnessKeyring};
    use crate::security::policy::{CommandRiskLevel, RUNTIME_APPROVAL_GRANT_ARG};

    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let command = "touch grant-marker".to_string();
    let job = cron::add_job(&cfg_snap, "*/5 * * * *", &command).unwrap();
    cron::claim_job_if_current_for_manual_run(
        &cfg_snap,
        &job,
        "blocking-grant-run",
        Utc::now(),
        ChronoDuration::seconds(90),
    )
    .unwrap()
    .unwrap();
    let security = test_security(&cfg_snap);
    let risk = match security.command_risk_level(&command) {
        CommandRiskLevel::Low => RiskLevel::Low,
        CommandRiskLevel::Medium => RiskLevel::Medium,
        CommandRiskLevel::High => RiskLevel::High,
    };
    let grant_v2 = ApprovalGrantV2::issue_one_shot(
        WitnessKeyring::global().unwrap(),
        Subject {
            agent_id: "prx:test:cron".to_string(),
            principal_id: "test:cron-user".to_string(),
            owner_id: "test:owner".to_string(),
            workspace_id: "test:workspace".to_string(),
            session_key: Some("test:session".to_string()),
        },
        IssuerAuthority::HumanReview,
        &command,
        risk,
    )
    .unwrap();
    let wire_grant = ApprovalGrant::from_verified_v2("cron", "test", grant_v2);
    let args = json!({
        "action": "run",
        "job_id": job.id,
        RUNTIME_APPROVAL_GRANT_ARG: serde_json::to_value(&wire_grant).unwrap(),
        "_zc_scope_trusted": true,
        // The local operator (local-operator CLI tuple): the job has no creator of its
        // own, so only the operator may run it (CRON-ORIGIN).
        "_zc_scope": {
            "principal_id": "test:cron-user",
            "sender": "agent", "channel": "cli", "chat_type": "private",
            "chat_id": "agent:local", "runtime_source": "chat"
        },
    });
    let tool = CronTool::new(Arc::clone(&cfg), Arc::clone(&security));

    let conflict = tool.execute(args.clone()).await.unwrap();
    assert!(!conflict.success);
    assert!(conflict.error.unwrap_or_default().contains("already running"));

    let still_usable = ApprovalGrant::from_runtime_args("cron", &args).unwrap();
    assert!(
        crate::security::SideEffectGate::new(security.as_ref())
            .authorize_command_execution("cron", &command, Some(&still_usable))
            .is_ok(),
        "claim conflict must not consume the single-use approval grant"
    );
}

#[tokio::test]
async fn manual_shell_run_needs_no_grant() {
    let tmp = TempDir::new().unwrap();
    let base = test_config(&tmp).await;
    let cfg = new_shared((*base.load_full()).clone());
    let cfg_snap = cfg.load_full();
    let command = "touch single-use-marker";
    let job = cron::add_job(&cfg_snap, "*/5 * * * *", command).unwrap();
    let security = test_security(&cfg_snap);
    let tool = CronTool::new(Arc::clone(&cfg), Arc::clone(&security));

    let result = tool.execute(json!({"action": "run", "job_id": job.id})).await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert!(cfg_snap.workspace_dir.join("single-use-marker").exists());
    assert!(!cfg_snap.workspace_dir.join("audit.log").exists());
}

#[tokio::test]
async fn manual_shell_run_executes_updated_command_and_releases_claim() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let marker = cfg_snap.workspace_dir.join("must-remain-absent");
    let job = cron::add_job(&cfg_snap, "*/5 * * * *", "echo historical-output").unwrap();
    let tool = CronTool::new(
        Arc::clone(&cfg),
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: cfg_snap.workspace_dir.clone(),
            ..SecurityPolicy::default()
        }),
    );
    let seeded = tool.execute(json!({"action": "run", "job_id": job.id})).await.unwrap();
    assert!(seeded.success, "{:?}", seeded.error);
    let before_rejection = cron::get_job(&cfg_snap, &job.id).unwrap();
    assert_eq!(before_rejection.last_status.as_deref(), Some("ok"));
    assert!(before_rejection.last_run.is_some());
    assert!(
        before_rejection
            .last_output
            .as_deref()
            .is_some_and(|output| output.contains("historical-output"))
    );
    cron::update_job(
        &cfg_snap,
        &job.id,
        CronJobPatch {
            command: Some("touch must-remain-absent".into()),
            ..CronJobPatch::default()
        },
    )
    .unwrap();

    let result = tool.execute(json!({"action": "run", "job_id": job.id})).await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert!(marker.exists());

    let released = cron::get_job(&cfg_snap, &job.id).unwrap();
    assert!(released.claim.is_none(), "completed direct run must release its claim");
    assert_ne!(released.last_run, before_rejection.last_run);
    assert_eq!(released.last_status.as_deref(), Some("ok"));
    let replacement = cron::claim_job_if_current_for_manual_run(
        &cfg_snap,
        &released,
        "replacement-owner",
        Utc::now(),
        ChronoDuration::seconds(90),
    )
    .unwrap();
    assert!(replacement.is_some(), "completed job must be immediately reclaimable");
    let events = cron::list_job_events(&cfg_snap, &job.id).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "cron.job.claim_abandoned")
            .count(),
        0
    );
}

#[tokio::test]
async fn force_run_paused_at_preserves_manual_run_compatibility() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
    let at = Utc::now() + chrono::Duration::hours(1);
    let job = cron::add_shell_job(
        &cfg_snap,
        Some("paused-manual-at".to_string()),
        Schedule::At { at },
        "echo paused-manual",
    )
    .unwrap();
    let paused = cron::update_job(
        &cfg_snap,
        &job.id,
        CronJobPatch {
            enabled: Some(false),
            ..CronJobPatch::default()
        },
    )
    .unwrap();
    assert!(!paused.enabled);

    let run = tool.execute(json!({"action": "run", "job_id": job.id})).await.unwrap();
    assert!(run.success, "{:?}", run.error);
    let terminal = cron::get_job(&cfg_snap, &job.id).unwrap();
    assert!(!terminal.enabled);
    assert_eq!(terminal.terminal_state, Some(cron::CronJobTerminalState::Succeeded));
}

/// `Schedule::Every` agent jobs persist the `Every` schedule (every_ms).
#[tokio::test]
async fn add_agent_schedule_every_persists() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

    let add = tool
        .execute(json!({
            "action": "add",
            "schedule": { "kind": "every", "every_ms": 30000 },
            "job_type": "agent",
            "prompt": "every-30s"
        }))
        .await
        .unwrap();
    assert!(add.success, "{:?}", add.error);

    let job = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .next()
        .expect("test: one job");
    assert!(matches!(job.schedule, Schedule::Every { every_ms: 30000 }));
}

/// CRON-ORIGIN: every creating action records who created the job, from
/// the service-authored scope only. A network surface is recorded as
/// itself with its owner, the local-operator CLI tuple and terminal UI as the local
/// operator, and a scope the runtime did not author as nobody. An update
/// never changes the origin.
///
/// MUTATION GUARD: drop `origin` from `lineage_from_trusted_scope` and the
/// webhook and operator rows go red.
#[tokio::test]
async fn creating_actions_record_the_creating_origin() {
    use crate::cron::origin_test_support::{Surface, WEBHOOK_OWNER, execute_as, surface_context};

    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = || Box::new(CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap)));
    let agent_add = json!({
        "action": "add",
        "schedule": { "kind": "cron", "expr": "0 9 * * *" },
        "job_type": "agent",
        "prompt": "brief me"
    });
    let shell_once = json!({"action": "once", "delay": "10m", "command": "echo once"});

    for (surface, args) in [
        (Surface::Webhook, agent_add.clone()),
        (Surface::Webhook, shell_once.clone()),
        (Surface::AgentCli, agent_add.clone()),
        (Surface::Terminal, shell_once.clone()),
    ] {
        let before = cron::list_jobs(&cfg_snap).unwrap().len();
        let created = execute_as(tool(), surface_context(&cfg_snap, surface), args).await;
        assert!(created.success, "{surface:?}: {created:?}");
        let jobs = cron::list_jobs(&cfg_snap).unwrap();
        assert_eq!(jobs.len(), before + 1);
        let job = jobs.iter().max_by_key(|job| job.created_at).expect("test: the new job");
        match surface {
            Surface::Webhook => {
                assert_eq!(
                    job.origin,
                    cron::JobOrigin::Principal {
                        channel: "webhook".to_string(),
                        sender: "client-a".to_string(),
                        chat_type: "gateway".to_string(),
                        owner_id: Some(WEBHOOK_OWNER.to_string()),
                        owner_isolated: false,
                    }
                );
                assert_eq!(job.owner_id.as_deref(), Some(WEBHOOK_OWNER));
            }
            Surface::AgentCli | Surface::Terminal => {
                assert_eq!(job.origin, cron::JobOrigin::LocalOperator, "{surface:?}");
            }
        }
    }

    // Called without the execution service: no runtime-authored scope.
    let direct = tool().execute(agent_add).await.unwrap();
    assert!(direct.success, "{:?}", direct.error);
    let unattributed = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .max_by_key(|job| job.created_at)
        .expect("test: the direct job");
    assert_eq!(unattributed.origin, cron::JobOrigin::Unattributed);

    // The operator rewriting the prompt of that job does not adopt it.
    let rewritten = execute_as(
        tool(),
        surface_context(&cfg_snap, Surface::Terminal),
        json!({"action": "update", "job_id": unattributed.id, "patch": {"prompt": "operator text"}}),
    )
    .await;
    assert!(rewritten.success, "{rewritten:?}");
    assert_eq!(
        cron::get_job(&cfg_snap, &unattributed.id).unwrap().origin,
        cron::JobOrigin::Unattributed
    );
}

/// CRON-ORIGIN: an origin is only meaningful if nobody else can rewrite
/// the job. Without a check, a webhook caller could `update` the prompt of
/// an agent job the operator created — and that prompt then runs with the
/// operator's authority — or remove, pause, resume or run it. A caller that
/// is not the local operator may now change or run only jobs its own owner
/// created; the operator may manage every job, and doing so never adopts
/// it (the origin is unchanged).
///
/// MUTATION GUARD: drop the `caller_may_manage` check from any of the five
/// actions and its row goes red.
#[tokio::test]
async fn only_the_creator_or_the_local_operator_may_change_or_run_a_job() {
    use crate::cron::origin_test_support::{Surface, execute_as, surface_context};

    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = || Box::new(CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap)));
    let add = |prompt: &str| {
        json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "job_type": "agent",
            "prompt": prompt
        })
    };
    let newest = || {
        cron::list_jobs(&cfg_snap)
            .unwrap()
            .into_iter()
            .max_by_key(|job| job.created_at)
            .expect("test: a job")
    };

    let created = execute_as(
        tool(),
        surface_context(&cfg_snap, Surface::Terminal),
        add("operator prompt"),
    )
    .await;
    assert!(created.success, "{created:?}");
    let operator_job = newest();

    for args in [
        json!({"action": "update", "job_id": operator_job.id, "patch": {"prompt": "webhook text"}}),
        json!({"action": "patch", "job_id": operator_job.id, "patch": {"prompt": "webhook text"}}),
        json!({"action": "pause", "job_id": operator_job.id}),
        json!({"action": "resume", "job_id": operator_job.id}),
        json!({"action": "run", "job_id": operator_job.id}),
        json!({"action": "remove", "job_id": operator_job.id}),
        json!({"action": "cancel", "job_id": operator_job.id}),
    ] {
        let refused = execute_as(tool(), surface_context(&cfg_snap, Surface::Webhook), args.clone()).await;
        assert!(
            !refused.success
                && refused
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("only its creator or the local operator")),
            "a webhook caller must not manage an operator job: {args} -> {refused:?}"
        );
    }
    let untouched = cron::get_job(&cfg_snap, &operator_job.id).expect("test: the operator job survives");
    assert_eq!(untouched.prompt.as_deref(), Some("operator prompt"));
    assert!(untouched.enabled);

    // The creator manages its own job; the operator manages it too.
    let created = execute_as(
        tool(),
        surface_context(&cfg_snap, Surface::Webhook),
        add("webhook prompt"),
    )
    .await;
    assert!(created.success, "{created:?}");
    let webhook_job = newest();
    let own = execute_as(
        tool(),
        surface_context(&cfg_snap, Surface::Webhook),
        json!({"action": "update", "job_id": webhook_job.id, "patch": {"prompt": "webhook prompt 2"}}),
    )
    .await;
    assert!(own.success, "the creator may change its own job: {own:?}");
    let paused = execute_as(
        tool(),
        surface_context(&cfg_snap, Surface::AgentCli),
        json!({"action": "pause", "job_id": webhook_job.id}),
    )
    .await;
    assert!(paused.success, "the operator may manage every job: {paused:?}");
    let removed = execute_as(
        tool(),
        surface_context(&cfg_snap, Surface::Webhook),
        json!({"action": "remove", "job_id": webhook_job.id}),
    )
    .await;
    assert!(removed.success, "the creator may remove its own job: {removed:?}");
}

/// R-CRON F-3 (CRON-READ-SCOPE): `list`, `get`, `runs`/`history`,
/// `events` and `status` answered every trusted caller with every job —
/// operator shell commands (often carrying tokens) and run outputs
/// included. A caller that is not the local operator now sees only the
/// jobs its own owner created, as `xin` does; a job it may not see reads
/// exactly like one that does not exist. The operator sees every job.
///
/// MUTATION GUARD: drop the read check from any of the five actions and
/// its assertion goes red.
#[tokio::test]
async fn a_caller_reads_only_its_own_jobs_and_the_operator_reads_all() {
    use crate::cron::origin_test_support::{Surface, execute_as, surface_context};
    const SECRET: &str = "curl -H 'Authorization: Bearer CRON-READ-SECRET' https://example.invalid";

    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = || Box::new(CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap)));
    let add_shell = |command: &str| json!({"action": "add", "expression": "0 9 * * *", "command": command});

    let created = execute_as(tool(), surface_context(&cfg_snap, Surface::Terminal), add_shell(SECRET)).await;
    assert!(created.success, "{created:?}");
    let created = execute_as(
        tool(),
        surface_context(&cfg_snap, Surface::Webhook),
        add_shell("echo mine"),
    )
    .await;
    assert!(created.success, "{created:?}");
    let jobs = cron::list_jobs(&cfg_snap).unwrap();
    let operator_job = jobs
        .iter()
        .find(|job| job.command == SECRET)
        .expect("test: operator job")
        .id
        .clone();
    let own_job = jobs
        .iter()
        .find(|job| job.command == "echo mine")
        .expect("test: own job")
        .id
        .clone();

    let as_webhook =
        |args: serde_json::Value| execute_as(tool(), surface_context(&cfg_snap, Surface::Webhook), args);
    let listed = as_webhook(json!({"action": "list"})).await;
    assert!(listed.success, "{listed:?}");
    assert!(listed.output.contains(&own_job), "{}", listed.output);
    assert!(
        !listed.output.contains(&operator_job) && !listed.output.contains("CRON-READ-SECRET"),
        "{}",
        listed.output
    );
    let status = as_webhook(json!({"action": "status"})).await;
    assert!(status.output.contains("Jobs:     1 total"), "{}", status.output);
    for action in ["get", "runs", "history", "events"] {
        let denied = as_webhook(json!({"action": action, "job_id": operator_job})).await;
        assert!(!denied.success, "{action}: {denied:?}");
        assert_eq!(
            denied.error.as_deref(),
            Some(format!("Job '{operator_job}' not found").as_str()),
            "{action}: indistinguishable from a missing job"
        );
        let own = as_webhook(json!({"action": action, "job_id": own_job})).await;
        assert!(own.success, "{action}: the creator reads its own job: {own:?}");
    }

    let as_operator = execute_as(
        tool(),
        surface_context(&cfg_snap, Surface::AgentCli),
        json!({"action": "list"}),
    )
    .await;
    assert!(
        as_operator.output.contains(&operator_job) && as_operator.output.contains(&own_job),
        "{}",
        as_operator.output
    );
    let operator_get = execute_as(
        tool(),
        surface_context(&cfg_snap, Surface::Terminal),
        json!({"action": "get", "job_id": own_job}),
    )
    .await;
    assert!(operator_get.success, "{operator_get:?}");
}

/// R-CRONf N-1: `runs`/`history`/`events` looked the job up only to
/// refuse another principal's, and a job id that resolved to nothing fell
/// through to the run and event tables, which answer `[]` — or, for a
/// removed job, its surviving event rows. An owner-scoped caller could
/// therefore tell a missing id from another principal's job. Both now read
/// the same `not found`. The local operator still reads a removed job's
/// events, which outlive the job.
#[tokio::test]
async fn an_owner_scoped_caller_reads_a_missing_job_exactly_like_a_foreign_one() {
    use crate::cron::origin_test_support::{Surface, execute_as, surface_context};

    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let cfg_snap = cfg.load_full();
    let tool = || Box::new(CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap)));

    let created = execute_as(
        tool(),
        surface_context(&cfg_snap, Surface::AgentCli),
        json!({"action": "add", "expression": "0 9 * * *", "command": "echo removed-later"}),
    )
    .await;
    assert!(created.success, "{created:?}");
    let removed_job = cron::list_jobs(&cfg_snap)
        .unwrap()
        .into_iter()
        .find(|job| job.command == "echo removed-later")
        .expect("test: the operator's job")
        .id;
    let removed = execute_as(
        tool(),
        surface_context(&cfg_snap, Surface::AgentCli),
        json!({"action": "remove", "job_id": removed_job}),
    )
    .await;
    assert!(removed.success, "{removed:?}");
    let never_existed = uuid::Uuid::new_v4().to_string();

    for job_id in [removed_job.as_str(), never_existed.as_str()] {
        for action in ["get", "runs", "history", "events"] {
            let answer = execute_as(
                tool(),
                surface_context(&cfg_snap, Surface::Webhook),
                json!({"action": action, "job_id": job_id}),
            )
            .await;
            assert!(!answer.success, "{action} {job_id}: {answer:?}");
            assert_eq!(
                answer.error.as_deref(),
                Some(format!("Job '{job_id}' not found").as_str()),
                "{action} {job_id}: a missing job must read exactly like another principal's"
            );
        }
    }

    let operator_events = execute_as(
        tool(),
        surface_context(&cfg_snap, Surface::AgentCli),
        json!({"action": "events", "job_id": removed_job}),
    )
    .await;
    assert!(operator_events.success, "{operator_events:?}");
    assert!(
        operator_events.output.contains(&removed_job),
        "the operator still reads a removed job's events: {}",
        operator_events.output
    );
}

#![cfg(test)]

use super::*;

#[tokio::test]
async fn spawn_action_obeys_readonly_resource_gate() {
    let workspace = tempfile::tempdir().expect("test: isolated gate workspace");
    let probe = crate::providers::credential_probe::CredentialProbe::start().await;
    let task = json!({"task": "sessions_spawn: run the blocked child task"});
    let (control_channel, _) = RecordingChannel::new();
    let mut control = make_tool_with_security_and_spawn_config(
        Arc::new(control_channel),
        Arc::new(EchoProvider {
            response: "control completed".into(),
        }),
        Arc::new(SecurityPolicy {
            autonomy: crate::security::AutonomyLevel::Full,
            workspace_dir: workspace.path().to_path_buf(),
            ..SecurityPolicy::default()
        }),
        crate::config::SessionsSpawnConfig::default(),
    );
    control.workspace_dir = workspace.path().to_path_buf();
    control.provider_name = probe.custom_provider();
    control.model = crate::providers::credential_probe::PROBE_MODEL.to_owned();
    control.configured_providers =
        providers::ConfiguredProviders::default().with_providers([control.provider_name.as_str()]);
    let allowed = execute_enabled_admitted_spawn(&control, task.clone())
        .await
        .expect("the same task must reach the enabled spawn path without the read-only gate");
    assert!(allowed.success, "ungated control did not execute: {:?}", allowed.error);
    let runs = control.active_runs_arc();
    let run_id = runs
        .read()
        .await
        .first()
        .expect("test: control registered a child")
        .id
        .clone();
    assert_eq!(
        await_terminal_reason(&runs, &run_id).await,
        crate::providers::credential_probe::PROBE_REPLY
    );
    assert_eq!(probe.requests().len(), 1, "test: control reached the child provider");

    let (ch, _) = RecordingChannel::new();
    let readonly_security = Arc::new(SecurityPolicy {
        autonomy: crate::security::policy::AutonomyLevel::ReadOnly,
        workspace_dir: workspace.path().to_path_buf(),
        ..SecurityPolicy::default()
    });
    let tool = SessionsSpawnTool::new(
        Arc::new(ch),
        probe.custom_provider(),
        crate::providers::credential_probe::PROBE_MODEL,
        0.7,
        readonly_security,
        workspace.path().to_path_buf(),
        crate::config::MultimodalConfig::default(),
        HashMap::new(),
        None,
        crate::providers::ProviderRuntimeOptions::default(),
        crate::config::SessionsSpawnConfig::default(),
    )
    .with_test_parent_provider(Arc::new(EchoProvider {
        response: "done".into(),
    }));

    let error = execute_enabled_admitted_spawn(&tool, task)
        .await
        .expect_err("a side-effect tool must not be offered on a read-only surface");
    assert_eq!(
        error.to_string(),
        "admitted fixture did not select tool 'sessions_spawn'",
        "a read-only turn must close before publishing an executable spawn selection"
    );
    assert!(tool.active_runs_snapshot().await.is_empty());
    assert_eq!(
        probe.requests().len(),
        1,
        "gated turn must not contact the child provider"
    );
}

#[tokio::test]
async fn kill_action_requires_resource_grant() {
    let tmp = tempfile::TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool_with_security_and_spawn_config(
        Arc::new(ch),
        Arc::new(EchoProvider {
            response: "done".into(),
        }),
        Arc::new(SecurityPolicy {
            autonomy: crate::security::AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        }),
        crate::config::SessionsSpawnConfig::default(),
    )
    .with_shared_memory(memory.clone());
    let owned_task = TaskRunControl::observe_fixture_task(tokio::spawn(std::future::pending::<()>()));
    let with_owner_scope = |mut arguments: serde_json::Value| {
        let object = arguments.as_object_mut().expect("kill arguments object");
        object.insert("_zc_scope_trusted".to_owned(), json!(true));
        object.insert(
            "_zc_scope".to_owned(),
            json!({
                "workspace_id": "/tmp",
                "channel": "telegram",
                "sender": "alice",
                "chat_type": "direct",
                "chat_id": "chat-1",
                "owner_id": "owner-a"
            }),
        );
        arguments
    };
    {
        let mut runs = tool.active_runs.write().await;
        runs.push(SubAgentRun {
            progress: crate::agent::idle::child_beat(),
            batch_id: None,
            id: "run-1".to_string(),
            task: "task".to_string(),
            owner_id: Some("owner-a".to_string()),
            topic_id: Some("topic-a".to_string()),
            source_message_event_id: Some("msg-a".to_string()),
            started_at: Utc::now(),
            finished_at: None,
            status: SubAgentStatus::Running,
            recipient: None,
            channel_name: None,
            abort_handle: Some(owned_task.abort_handle()),
            process_control: None,
            task_control: Some(owned_task.control()),
            history: Arc::new(RwLock::new(Vec::new())),
            steer_tx: None,
            parent_run_id: None,
            session_scope_key: "test-session".to_string(),
            spawn_depth: 0,
            delegation: None,
            token_usage_records: Vec::new(),
        });
    }

    let denied = tool
        .execute(with_owner_scope(json!({"action": "kill", "run_id": "run-1"})))
        .await
        .unwrap();
    assert!(!denied.success);
    assert!(denied.error.unwrap_or_default().contains("runtime approval grant"));

    let allowed = tool
        .execute(with_owner_scope(json!({
            "action": "kill",
            "run_id": "run-1",
            crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG:
                serde_json::to_value(ApprovalGrant::for_resource_operation(
                    "sessions_spawn",
                    "sessions_spawn:kill:run-1",
                    "test",
                    None
                )).unwrap()
        })))
        .await
        .unwrap();
    assert!(allowed.success);

    let events = memory
        .list_memory_events_since(
            &MemoryPrincipal {
                require_owner_match: false,
                workspace_id: "/tmp".to_string(),
                agent_id: None,
                persona_id: None,
                session_key: Some("test-session".to_string()),
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
    let killed = events
        .iter()
        .find(|event| event.event_type == "task.killed")
        .expect("task.killed event should be persisted");
    assert_eq!(killed.subject_table, "tasks");
    assert_eq!(killed.subject_id, "run-1");
    let payload: serde_json::Value = serde_json::from_str(killed.payload_json.as_deref().unwrap()).unwrap();
    assert_eq!(payload["owner_id"].as_str(), Some("owner-a"));
    assert_eq!(payload["topic_id"].as_str(), Some("topic-a"));
    assert_eq!(payload["source_message_event_id"].as_str(), Some("msg-a"));
}

#[tokio::test]
async fn sessions_spawn_process_kill_waits_for_owner_and_is_idempotent() {
    let (channel, _) = RecordingChannel::new();
    let tool = make_tool(
        Arc::new(channel),
        Arc::new(EchoProvider {
            response: "done".to_string(),
        }),
    );
    let control = ProcessRunControl::new();
    let mut run = restore_test_run("process-run", SubAgentStatus::Running);
    run.process_control = Some(control.clone());
    tool.active_runs.write().await.push(run);
    let owner = {
        let control = control.clone();
        let runs = tool.active_runs.clone();
        tokio::spawn(async move {
            let reason = control.termination_requested().await;
            assert_eq!(reason, "killed by user");
            runs.write().await[0].status = SubAgentStatus::Failed(reason);
            control.finalize(ProcessFinalization::Terminated);
        })
    };
    let args = || {
        json!({
            "action": "kill",
            "run_id": "process-run",
            crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG:
                serde_json::to_value(ApprovalGrant::for_resource_operation(
                    "sessions_spawn",
                    "sessions_spawn:kill:process-run",
                    "test",
                    None
                )).unwrap()
        })
    };

    let first = tool.execute(args()).await.unwrap();
    assert!(first.success);
    owner.await.unwrap();
    let repeated = tool.execute(args()).await.unwrap();
    assert!(repeated.success);
    assert_eq!(control.finalization(), Some(ProcessFinalization::Terminated));
}

#[tokio::test]
async fn sessions_spawn_process_kill_reports_pending_without_claiming_owner_failure() {
    let (channel, _) = RecordingChannel::new();
    let tool = make_tool(
        Arc::new(channel),
        Arc::new(EchoProvider {
            response: "done".to_string(),
        }),
    );
    let control = ProcessRunControl::new_with_request_timeout(std::time::Duration::from_millis(20));
    let mut run = restore_test_run("pending-process", SubAgentStatus::Running);
    run.process_control = Some(control.clone());
    tool.active_runs.write().await.push(run);

    let result = tool
        .execute(json!({
            "action": "kill",
            "run_id": "pending-process",
            crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG:
                serde_json::to_value(ApprovalGrant::for_resource_operation(
                    "sessions_spawn",
                    "sessions_spawn:kill:pending-process",
                    "test",
                    None
                )).unwrap()
        }))
        .await
        .unwrap();

    assert!(!result.success);
    let error = result.error.unwrap();
    assert!(error.contains("still pending reap"));
    assert!(!error.contains("could not be terminated"));
    assert_eq!(control.finalization(), None);
    assert!(matches!(
        tool.active_runs.read().await[0].status,
        SubAgentStatus::Running
    ));
}

#[tokio::test]
async fn active_runs_store_owner_topic_lineage() {
    let memory_dir = tempfile::TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(memory_dir.path()).unwrap());
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(
        Arc::new(ch),
        Arc::new(EchoProvider {
            response: "done".into(),
        }),
    )
    .with_shared_memory(Arc::clone(&memory));

    let result = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({
            "task": "lineage task",
            "_zc_scope_trusted": true,
            "_zc_scope": {
                "sender": "alice",
                "channel": "telegram",
                "chat_type": "direct",
                "chat_id": "chat-1",
                "topic_id": "topic-a",
                "task_id": "parent-task",
                "message_event_id": "msg-a"
            }
        })),
    )
    .await
    .unwrap();
    assert!(result.success);

    let runs = tool.active_runs_snapshot().await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].owner_id.as_deref(), Some("owner:/tmp:telegram:alice"));
    assert_eq!(runs[0].topic_id.as_deref(), Some("topic-a"));
    assert_eq!(runs[0].parent_run_id.as_deref(), None);
    let events = memory
        .list_message_events_since(
            &MemoryPrincipal {
                require_owner_match: false,
                workspace_id: "/tmp".to_string(),
                agent_id: None,
                persona_id: None,
                session_key: Some("telegram:chat-1:alice".to_string()),
                channel: Some("telegram".to_string()),
                sender: Some("alice".to_string()),
                owner_id: None,
                legacy_session_key: None,
            },
            0,
            10,
        )
        .await
        .unwrap();
    let parent_event = events
        .iter()
        .find(|event| event.role == "user" && event.source == "channel")
        .expect("admitted parent event");
    assert_eq!(
        runs[0].source_message_event_id.as_deref(),
        Some(parent_event.event_id.as_str())
    );
    assert_ne!(runs[0].source_message_event_id.as_deref(), Some("msg-a"));
}

#[tokio::test]
async fn default_recipient_handle_shared() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let handle = tool.default_recipient_handle();
    *handle.write().await = Some("via-handle".to_string());

    let val = tool.default_recipient.read().await.clone();
    assert_eq!(val.as_deref(), Some("via-handle"));
}

#[tokio::test]
async fn history_action_returns_no_run_error() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let result = tool
        .execute(json!({"action": "history", "run_id": "nonexistent"}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.unwrap().contains("No runtime run found"));
}

#[tokio::test]
async fn kill_memory_backed_run_reports_not_manageable() {
    let tmp = tempfile::TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    memory
        .append_memory_event(MemoryEventInput {
            owner_id: None,
            event_id: None,
            workspace_id: "/tmp".to_string(),
            event_type: "task.spawned".to_string(),
            subject_table: "tasks".to_string(),
            subject_id: "memory-only-run".to_string(),
            session_key: Some("test-session".to_string()),
            run_id: None,
            parent_run_id: None,
            agent_id: None,
            persona_id: None,
            visibility: MemoryVisibility::Workspace,
            payload_json: Some(json!({"task": "stale task"}).to_string()),
        })
        .await
        .unwrap();
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() })).with_shared_memory(memory);

    let result = tool
        .execute(json!({"action": "kill", "run_id": "memory-only-run"}))
        .await
        .unwrap();

    assert!(!result.success);
    let error = result.error.unwrap_or_default();
    assert!(error.contains("source=memory, manageable=false"), "{error}");
    assert!(error.contains("not present in the current runtime registry"), "{error}");
}

#[tokio::test]
async fn history_action_requires_run_id() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let result = tool.execute(json!({"action": "history"})).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn steer_action_requires_message() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let result = tool.execute(json!({"action": "steer", "run_id": "xxx"})).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn steer_action_returns_no_run_error() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let result = tool
        .execute(json!({"action": "steer", "run_id": "nonexistent", "message": "pivot!"}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.unwrap().contains("No run found"));
}

/// G4/G5: a process-mode run must publish a live steer channel.
///
/// Before this existed the process branch registered `steer_tx: None`, so
/// every `sessions_send` / `sessions_spawn:steer` against an OS-process
/// sub-agent was rejected as a "legacy run without steer support". Flipping
/// `new_process_mode_run` back to `None` turns this test red.
#[tokio::test]
async fn process_mode_run_is_steerable_and_not_reported_as_legacy() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let (steer_tx, mut steer_rx) = tokio::sync::mpsc::channel(STEER_CHANNEL_CAPACITY);
    let lineage = SpawnLineage {
        owner_id: None,
        owner_isolated: false,
        topic_id: None,
        parent_task_id: None,
        source_message_event_id: None,
    };
    let run = new_process_mode_run(ProcessModeRunSeed {
        progress: crate::agent::idle::child_beat(),
        batch_id: None,
        id: "run-process-steer".to_string(),
        task: "long task".to_string(),
        lineage: &lineage,
        recipient: None,
        channel_name: None,
        process_control: ProcessRunControl::new(),
        history: Arc::new(RwLock::new(Vec::new())),
        steer_tx,
        parent_run_id: None,
        session_scope_key: "test-session".to_string(),
        spawn_depth: 0,
        delegation: None,
    });
    assert!(
        run.steer_tx.is_some(),
        "a process-mode run must own a steer channel or sessions_send cannot reach it"
    );
    assert!(run.process_control.is_some(), "this must remain a process-mode row");
    tool.active_runs.write().await.push(run);

    let result = tool
        .execute(json!({
            "action": "steer",
            "run_id": "run-process-steer",
            "message": "switch to plan B",
        }))
        .await
        .expect("steer tool call");

    assert!(result.success, "{result:?}");
    let error = result.error.unwrap_or_default();
    assert!(
        !error.contains("legacy run"),
        "process-mode steering must not report legacy-run: {error}"
    );
    assert_eq!(steer_rx.recv().await.as_deref(), Some("switch to plan B"));
}

/// Frames must reach the worker's stdin as line-delimited JSON, and each
/// delivered frame must show up in the run history `sessions_history` reads.
#[cfg(unix)]
#[tokio::test]
async fn steer_frames_reach_worker_stdin_and_land_in_history() {
    use tokio::io::AsyncBufReadExt;

    // Stand-in worker: echo every stdin line straight back on stdout.
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg("while IFS= read -r line; do printf '%s\\n' \"$line\"; done")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("stand-in worker spawns");
    let stdin = child.stdin.take().expect("worker stdin");
    let stdout = child.stdout.take().expect("worker stdout");

    let history: Arc<RwLock<Vec<HistoryEntry>>> = Arc::new(RwLock::new(Vec::new()));
    let (steer_tx, steer_rx) = tokio::sync::mpsc::channel(STEER_CHANNEL_CAPACITY);
    let pump = tokio::spawn(pump_worker_steer_frames(
        stdin,
        steer_rx,
        Arc::clone(&history),
        "run-pipe".to_string(),
    ));

    steer_tx.send("pivot to X".to_string()).await.expect("first steer");
    steer_tx.send("now do Y".to_string()).await.expect("second steer");

    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut received = Vec::new();
    for _ in 0..2 {
        let line = tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line())
            .await
            .expect("worker echoes each steer frame")
            .expect("stdout read")
            .expect("stdout line");
        received.push(serde_json::from_str::<WorkerControlFrame>(&line).expect("frame is line JSON"));
    }
    assert_eq!(
        received,
        vec![
            WorkerControlFrame::Steer {
                message: "pivot to X".to_string()
            },
            WorkerControlFrame::Steer {
                message: "now do Y".to_string()
            },
        ]
    );

    // Closing the channel ends the pump; history is complete afterwards.
    drop(steer_tx);
    tokio::time::timeout(std::time::Duration::from_secs(10), pump)
        .await
        .expect("pump finishes once the steer channel closes")
        .expect("pump task");
    let entries = history.read().await;
    assert_eq!(
        entries.iter().map(|e| e.content.clone()).collect::<Vec<_>>(),
        vec![steering_instruction("pivot to X"), steering_instruction("now do Y"),],
        "every delivered steer must be visible to sessions_history"
    );
    assert!(entries.iter().all(|entry| entry.role == "user"));
}

/// Back-pressure on the pipe itself: a worker that never reads its stdin
/// must slow the producer down rather than lose frames. No wall-clock
/// deadline is involved anywhere on that chain.
#[cfg(unix)]
#[tokio::test]
async fn steer_pump_backpressures_when_the_worker_never_reads_stdin() {
    let mut command = tokio::process::Command::new("sleep");
    command
        .arg("300")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("stand-in worker spawns");
    let stdin = child.stdin.take().expect("worker stdin");

    let history: Arc<RwLock<Vec<HistoryEntry>>> = Arc::new(RwLock::new(Vec::new()));
    // Capacity 1 keeps the test fast: one frame is in flight in the pump,
    // one sits in the queue, and the third producer send must park.
    let (steer_tx, steer_rx) = tokio::sync::mpsc::channel(1);
    let pump = tokio::spawn(pump_worker_steer_frames(
        stdin,
        steer_rx,
        Arc::clone(&history),
        "run-blocked".to_string(),
    ));

    // Fill the pipe: `sleep` never reads, so once the kernel buffer is full
    // the pump parks mid-write and stops draining the queue.
    let mut parked = false;
    let filler = "x".repeat(8192);
    for _ in 0..64 {
        if tokio::time::timeout(std::time::Duration::from_millis(200), steer_tx.send(filler.clone()))
            .await
            .is_err()
        {
            parked = true;
            break;
        }
    }
    assert!(parked, "a worker that never drains stdin must park the producer");
    assert!(
        !steer_tx.is_closed(),
        "parking must not close the channel: nothing may be dropped"
    );

    pump.abort();
    let _ = child.kill().await;
}

/// Task mode must be untouched by the process-mode work: the injected turn
/// keeps its exact historical wording, now shared with the worker.
#[test]
fn task_mode_steering_wording_is_unchanged() {
    assert_eq!(
        steering_instruction("pivot now"),
        "[Steering instruction from operator] pivot now"
    );
}

/// Backpressure contract for a sub-agent's steering channel.
///
/// The channel is bounded, so when a sub-agent has not drained it the
/// steering tool must *park* rather than drop the message or fail. Just as
/// importantly, it must park without holding the `active_runs` guard —
/// otherwise the very loop that would free a slot (which takes a write
/// guard right after `recv`) could never run, and backpressure would become
/// deadlock.
#[tokio::test]
async fn steer_backpressures_when_full_without_dropping_or_holding_the_registry_lock() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let (steer_tx, mut steer_rx) = tokio::sync::mpsc::channel(STEER_CHANNEL_CAPACITY);
    let filler = steer_tx.clone();
    {
        let mut runs = tool.active_runs.write().await;
        runs.push(SubAgentRun {
            progress: crate::agent::idle::child_beat(),
            batch_id: None,
            id: "run-backpressure".to_string(),
            task: "task".to_string(),
            owner_id: None,
            topic_id: None,
            source_message_event_id: None,
            started_at: Utc::now(),
            finished_at: None,
            status: SubAgentStatus::Running,
            recipient: None,
            channel_name: None,
            abort_handle: None,
            process_control: None,
            task_control: None,
            history: Arc::new(RwLock::new(Vec::new())),
            steer_tx: Some(steer_tx),
            parent_run_id: None,
            session_scope_key: "test-session".to_string(),
            spawn_depth: 0,
            delegation: None,
            token_usage_records: Vec::new(),
        });
    }

    for i in 0..STEER_CHANNEL_CAPACITY {
        filler
            .try_send(format!("filler-{i}"))
            .expect("channel accepts messages up to its capacity");
    }
    assert!(
        filler.try_send("overflow".to_string()).is_err(),
        "steer channel must be bounded at STEER_CHANNEL_CAPACITY"
    );

    let steer = tool.execute(json!({
        "action": "steer",
        "run_id": "run-backpressure",
        "message": "pivot now",
    }));
    tokio::pin!(steer);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut steer)
            .await
            .is_err(),
        "a full steer queue must slow the producer, not complete or drop"
    );

    // The parked steer released the registry guard before awaiting, so the
    // sub-agent loop can still reach the receiver. This read would hang if
    // the guard were held across the send.
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), tool.active_runs.read())
            .await
            .expect("registry stays readable while a steer is parked")
            .len(),
        1
    );

    // Freeing one slot wakes the parked producer.
    assert_eq!(steer_rx.recv().await.as_deref(), Some("filler-0"));
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), steer)
        .await
        .expect("parked steer resumes once the sub-agent drains")
        .expect("steer tool call succeeds");
    assert!(result.success, "{result:?}");

    // Nothing was dropped: every filler still arrives, in order, followed by
    // the message that had been waiting on backpressure.
    for i in 1..STEER_CHANNEL_CAPACITY {
        assert_eq!(steer_rx.recv().await, Some(format!("filler-{i}")));
    }
    assert_eq!(steer_rx.recv().await.as_deref(), Some("pivot now"));
}

#[tokio::test]
async fn steer_action_persists_task_event() {
    let tmp = tempfile::TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let (ch, _) = RecordingChannel::new();
    let tool =
        make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() })).with_shared_memory(memory.clone());
    let (steer_tx, mut steer_rx) = tokio::sync::mpsc::channel(STEER_CHANNEL_CAPACITY);
    {
        let mut runs = tool.active_runs.write().await;
        runs.push(SubAgentRun {
            progress: crate::agent::idle::child_beat(),
            batch_id: None,
            id: "run-steer".to_string(),
            task: "task".to_string(),
            owner_id: Some("owner-a".to_string()),
            topic_id: Some("topic-a".to_string()),
            source_message_event_id: Some("msg-a".to_string()),
            started_at: Utc::now(),
            finished_at: None,
            status: SubAgentStatus::Running,
            recipient: None,
            channel_name: None,
            abort_handle: None,
            process_control: None,
            task_control: None,
            history: Arc::new(RwLock::new(Vec::new())),
            steer_tx: Some(steer_tx),
            parent_run_id: None,
            session_scope_key: "test-session".to_string(),
            spawn_depth: 0,
            delegation: None,
            token_usage_records: Vec::new(),
        });
    }

    let result = tool
        .execute(json!({"action": "steer", "run_id": "run-steer", "message": "pivot now"}))
        .await
        .unwrap();
    assert!(result.success);
    assert_eq!(steer_rx.recv().await.as_deref(), Some("pivot now"));

    let events = memory
        .list_memory_events_since(
            &MemoryPrincipal {
                require_owner_match: false,
                workspace_id: "/tmp".to_string(),
                agent_id: None,
                persona_id: None,
                session_key: Some("test-session".to_string()),
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
    let steered = events
        .iter()
        .find(|event| event.event_type == "task.steered")
        .expect("task.steered event should be persisted");
    assert_eq!(steered.subject_table, "tasks");
    assert_eq!(steered.subject_id, "run-steer");
    let payload: serde_json::Value = serde_json::from_str(steered.payload_json.as_deref().unwrap()).unwrap();
    assert_eq!(payload["owner_id"].as_str(), Some("owner-a"));
    assert_eq!(payload["detail"]["message_preview"].as_str(), Some("pivot now"));
}

#[tokio::test]
async fn history_populated_after_no_tools_run() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(
        Arc::new(ch),
        Arc::new(EchoProvider {
            response: "finished work".into(),
        }),
    );

    // Spawn without tool registry (no tools set)
    let spawn_result = execute_admitted_spawn(&tool, with_spawn_grant(json!({"task": "Do a thing"})))
        .await
        .unwrap();
    assert!(spawn_result.success);
    let run_id = spawn_result
        .output
        .split("run_id: ")
        .nth(1)
        .unwrap()
        .split(')')
        .next()
        .unwrap()
        .trim()
        .to_string();

    // Wait for task to complete
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Check history
    let hist_result = tool
        .execute(json!({"action": "history", "run_id": run_id}))
        .await
        .unwrap();
    assert!(hist_result.success);
    assert!(hist_result.output.contains("user"));
    assert!(hist_result.output.contains("assistant"));
}

#[tokio::test]
async fn history_action_returns_tail_usage_and_truncation_controls() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let history = Arc::new(RwLock::new(Vec::new()));
    {
        let mut entries = history.write().await;
        for idx in 0..25 {
            entries.push(HistoryEntry {
                role: "assistant".to_string(),
                content: format!("entry-{idx} {}", "x".repeat(120)),
                timestamp: Utc::now(),
            });
        }
    }
    {
        let mut runs = tool.active_runs.write().await;
        runs.push(SubAgentRun {
            progress: crate::agent::idle::child_beat(),
            batch_id: None,
            id: "run-history-tail".to_string(),
            task: "task".to_string(),
            owner_id: None,
            topic_id: None,
            source_message_event_id: None,
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            status: SubAgentStatus::Completed("done".to_string()),
            recipient: None,
            channel_name: None,
            abort_handle: None,
            process_control: None,
            task_control: None,
            history,
            steer_tx: None,
            parent_run_id: Some("parent".to_string()),
            session_scope_key: "test-session".to_string(),
            spawn_depth: 0,
            delegation: None,
            token_usage_records: vec![crate::llm::route_decision::MeteredTokenUsageRecord {
                settlement_id: None,
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                prompt_tokens: 1000,
                completion_tokens: 500,
                total_tokens: 1500,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                source: crate::llm::route_decision::TokenUsageSource::Reported,
                cost_usd: Some(0.0042),
            }],
        });
    }

    let result = tool
        .execute(json!({
            "action": "history",
            "run_id": "run-history-tail",
            "last_n": 3,
            "max_chars_per_entry": 80
        }))
        .await
        .unwrap();

    assert!(result.success);
    assert!(result.output.contains("showing last 3"), "{}", result.output);
    assert!(result.output.contains("usage=1.5k tok | $0.0042"), "{}", result.output);
    assert!(result.output.contains("entry-22"), "{}", result.output);
    assert!(result.output.contains("entry-24"), "{}", result.output);
    assert!(!result.output.contains("entry-0"), "{}", result.output);
    assert!(result.output.contains("[entry truncated]"), "{}", result.output);
    assert!(
        result.output.contains("[history omitted: 22 older entries"),
        "{}",
        result.output
    );
}

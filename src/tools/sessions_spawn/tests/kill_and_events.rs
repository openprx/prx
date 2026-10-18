#![cfg(test)]

use super::*;

/// P0 concurrency race (kill variant): killing a run must notify on the run's
/// per-turn channel + recipient bound at spawn time — never the shared active
/// channel that a later, concurrently-processed message may have overwritten.
#[tokio::test]
async fn kill_uses_per_turn_channel_not_shared_state() {
    let (signal_ch, signal_sent) = NamedRecordingChannel::new("signal");
    let (wacli_ch, wacli_sent) = NamedRecordingChannel::new("wacli");
    let tool = make_tool_with_channels(
        signal_ch.clone(),
        // A long-lived run so it is still Running when we kill it.
        Arc::new(SleepyProvider {
            delay_ms: 5_000,
            response: "never reached".into(),
        }),
        vec![signal_ch, wacli_ch],
    );

    // Spawn A on wacli (its scope), in task mode so the abort handle exists.
    let result = execute_admitted_spawn(
        &tool,
        with_spawn_grant(with_scope(
            json!({"task": "long A work", "mode": "task"}),
            "wacli",
            "A-wacli-recipient",
        )),
    )
    .await
    .unwrap();
    assert!(result.success, "spawn should succeed: {:?}", result.error);
    let run_id = {
        let runs = tool.active_runs_snapshot().await;
        runs.first().expect("one run registered").id.clone()
    };

    // A concurrent message B (signal) overwrites the shared active channel
    // *before* the kill — exactly the race the fix must defeat.
    tool.set_active_channel({
        let (b_ch, _) = NamedRecordingChannel::new("signal");
        b_ch as Arc<dyn Channel>
    })
    .await;
    tool.set_active_recipient("B-signal-recipient").await;

    let kill_grant = serde_json::to_value(ApprovalGrant::for_resource_operation(
        "sessions_spawn",
        &format!("sessions_spawn:kill:{run_id}"),
        "test",
        None,
    ))
    .unwrap();
    let kill = tool
        .execute(json!({
            "action": "kill",
            "run_id": run_id,
            crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG: kill_grant,
        }))
        .await
        .unwrap();
    assert!(kill.success, "kill should succeed: {:?}", kill.error);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let on_wacli = wacli_sent.lock().await;
    let on_signal = signal_sent.lock().await;
    assert_eq!(on_wacli.len(), 1, "kill notice must route to A's own (wacli) channel");
    assert!(on_wacli[0].contains("killed"));
    assert!(
        on_signal.is_empty(),
        "kill notice must NOT leak onto signal (the racing message B's channel)"
    );
}

/// FIX-P0-37: spawning is a Medium-risk side effect. Under explicit
/// Supervised autonomy and with NO approval grant supplied, the gate must
/// deny the spawn outright — no run is registered and no announcement fires.
#[tokio::test]
async fn spawn_denied_without_grant_under_supervised() {
    let workspace = tempfile::tempdir().expect("test: isolated gate workspace");
    let probe = crate::providers::credential_probe::CredentialProbe::start().await;
    let task = json!({"task": "sessions_spawn: tell a joke in a child task"});
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
        .expect("the same task must reach the enabled spawn path without the supervised gate");
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

    let (ch, sent) = RecordingChannel::new();
    let mut tool = make_tool_with_security_and_spawn_config(
        Arc::new(ch),
        Arc::new(EchoProvider {
            response: "should never run".into(),
        }),
        Arc::new(SecurityPolicy {
            autonomy: crate::security::AutonomyLevel::Supervised,
            workspace_dir: workspace.path().to_path_buf(),
            ..SecurityPolicy::default()
        }),
        crate::config::SessionsSpawnConfig::default(),
    );
    tool.workspace_dir = workspace.path().to_path_buf();
    tool.provider_name = probe.custom_provider();
    tool.model = crate::providers::credential_probe::PROBE_MODEL.to_owned();
    tool.configured_providers = providers::ConfiguredProviders::default().with_providers([tool.provider_name.as_str()]);
    tool.set_default_recipient(Some("test-recipient".to_string())).await;

    // No grant injected → Medium-risk gate denies under supervised autonomy.
    let denied = execute_enabled_admitted_spawn(&tool, task)
        .await
        .expect_err("an unapproved side-effect tool must not be offered for execution");
    assert_eq!(
        denied.to_string(),
        "admitted fixture did not select tool 'sessions_spawn'",
        "the admitted surface must exclude the unapproved tool"
    );

    // No run should have been registered.
    assert!(tool.active_runs_snapshot().await.is_empty());

    // Give any (non-existent) async work a chance; nothing should be sent.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(sent.lock().await.len(), 0);
    assert_eq!(
        probe.requests().len(),
        1,
        "gated turn must not contact the child provider"
    );
}

#[tokio::test]
async fn spawn_records_request_and_result_message_events() {
    let tmp = tempfile::TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(
        Arc::new(ch),
        Arc::new(EchoProvider {
            response: "fabric result".into(),
        }),
    )
    .with_shared_memory(memory.clone());

    let result = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({
            "task": "write through fabric",
            "_zc_scope_trusted": true,
            "_zc_scope": {
                "sender": "alice",
                "channel": "telegram",
                "chat_type": "direct",
                "chat_id": "chat-1",
                "topic_id": "topic-1",
                "source_message_event_id": "msg-1"
            }
        })),
    )
    .await
    .unwrap();
    assert!(result.success);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

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
        .expect("admitted parent message event");
    let request_event = events
        .iter()
        .find(|event| event.role == "user" && event.source == "sessions_spawn")
        .expect("spawn request event");
    let result_event = events
        .iter()
        .find(|event| event.role == "event" && event.content.contains("fabric result"))
        .expect("spawn domain result event");
    assert_eq!(request_event.source, "sessions_spawn");
    assert_eq!(request_event.owner_id.as_deref(), Some("owner:/tmp:telegram:alice"));
    assert_eq!(request_event.content, "write through fabric");
    assert_eq!(result_event.source, "sessions_spawn");
    assert_eq!(result_event.owner_id.as_deref(), Some("owner:/tmp:telegram:alice"));
    assert_eq!(events.iter().filter(|event| event.role == "assistant").count(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "provider.final_outcome")
            .count(),
        1
    );
    let logical_terminal_key = format!(
        "turn:{}:final",
        request_event.run_id.as_deref().expect("spawn request run id")
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.event_type == "turn.finalized"
                    && event.idempotency_key.as_deref() == Some(logical_terminal_key.as_str())
            })
            .count(),
        1
    );
    let request_payload: serde_json::Value = serde_json::from_str(request_event.raw_payload_json.as_deref().unwrap())
        .expect("request payload should be json");
    assert_eq!(request_payload["topic_id"].as_str(), Some("topic-1"));
    assert_eq!(
        request_payload["source_message_event_id"].as_str(),
        Some(parent_event.event_id.as_str())
    );

    let memory_events = memory
        .list_memory_events_since(
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
            20,
        )
        .await
        .unwrap();
    let task_events = memory_events
        .iter()
        .filter(|event| event.subject_table == "tasks")
        .collect::<Vec<_>>();
    assert_eq!(
        task_events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["task.spawned", "task.completed"]
    );
    assert!(
        task_events
            .iter()
            .all(|event| event.subject_id == request_event.run_id.as_deref().unwrap())
    );
    let task_payload: serde_json::Value =
        serde_json::from_str(task_events[0].payload_json.as_deref().unwrap()).unwrap();
    assert_eq!(task_payload["topic_id"].as_str(), Some("topic-1"));
    assert_eq!(
        task_payload["source_message_event_id"].as_str(),
        Some(parent_event.event_id.as_str())
    );
}

/// A provider whose every call fails with an error carrying the D-053 probe
/// secret.
struct SecretFailingProvider;

#[async_trait::async_trait]
impl crate::providers::Provider for SecretFailingProvider {
    async fn chat_with_system(
        &self,
        _system: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Err(anyhow!(
            "upstream rejected key {}",
            crate::agent::terminal::persistence_probe::PROBE_SECRET
        ))
    }

    async fn chat(
        &self,
        _request: crate::providers::ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::ChatResponse> {
        Err(anyhow!(
            "upstream rejected key {}",
            crate::agent::terminal::persistence_probe::PROBE_SECRET
        ))
    }
}

/// Spawn one task-mode run through the real `execute` path with the
/// memory in `tmp`, wait (bounded) until the run leaves `Running` — its
/// terminal commit and result events are written before the status flips —
/// and return the final status plus the recorded `turn.finalized` count.
async fn spawn_probe_run(
    tmp: &tempfile::TempDir,
    provider: Arc<dyn crate::providers::Provider>,
) -> (SubAgentStatus, usize) {
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: open sqlite memory in tempdir"));
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), provider).with_shared_memory(memory.clone());

    let result = execute_admitted_spawn(
        &tool,
        with_spawn_grant(json!({
            "task": "probe the persisted terminal",
            "_zc_scope_trusted": true,
            "_zc_scope": {
                "sender": "alice",
                "channel": "telegram",
                "chat_type": "direct",
                "chat_id": "chat-1",
                "topic_id": "topic-1",
                "source_message_event_id": "msg-1"
            }
        })),
    )
    .await
    .expect("test: spawn tool call");
    assert!(result.success, "{result:?}");
    let run_id = {
        let runs = tool.active_runs_snapshot().await;
        runs.first().expect("test: the spawned run is registered").id.clone()
    };
    let _ = await_terminal_reason(&tool.active_runs, &run_id).await;
    let status = tool
        .active_runs_snapshot()
        .await
        .into_iter()
        .find(|run| run.id == run_id)
        .expect("test: the finished run stays registered")
        .status;

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
            50,
        )
        .await
        .expect("test: list message events");
    let logical_terminal_key = format!("turn:{run_id}:final");
    let finalized = events
        .iter()
        .filter(|event| {
            event.event_type == "turn.finalized"
                && event.idempotency_key.as_deref() == Some(logical_terminal_key.as_str())
        })
        .count();
    (status, finalized)
}

/// D-053 / REDACT-TURN probe: a task-mode `sessions_spawn` run whose provider
/// reply carries a secret goes through `finalize_turn` (Completed) and
/// `record_spawn_result_event`; this must fail before the REDACT-TURN commits.
#[tokio::test]
async fn spawn_persists_no_secret_from_a_reply() {
    use crate::agent::terminal::persistence_probe::{PROBE_SECRET, assert_nothing_under_persists_the_probe_secret};
    let tmp = tempfile::TempDir::new().expect("test: tempdir");
    let (status, finalized) = spawn_probe_run(
        &tmp,
        Arc::new(EchoProvider {
            response: format!("Here is the key {PROBE_SECRET} you asked for"),
        }),
    )
    .await;

    assert!(
        matches!(&status, SubAgentStatus::Completed(output) if output.contains(PROBE_SECRET)),
        "the run must complete with the provider's reply: {status:?}"
    );
    assert_eq!(finalized, 1, "the terminal commit must have been recorded");
    // `make_tool` hard-codes workspace_dir=/tmp (cost ledger lives there),
    // so only the tempdir holding the SQLite memory is scanned.
    assert_nothing_under_persists_the_probe_secret(tmp.path());
}

/// D-053 / REDACT-TURN probe: a task-mode `sessions_spawn` run whose provider
/// fails with an error carrying a secret goes through `finalize_turn` (Failed)
/// and `record_spawn_result_event`; this must fail before the REDACT-TURN commits.
#[tokio::test]
async fn spawn_persists_no_secret_from_a_provider_error() {
    use crate::agent::terminal::persistence_probe::assert_nothing_under_persists_the_probe_secret;
    let tmp = tempfile::TempDir::new().expect("test: tempdir");
    let (status, finalized) = spawn_probe_run(&tmp, Arc::new(SecretFailingProvider)).await;

    assert!(
        matches!(&status, SubAgentStatus::Failed(_)),
        "the run must fail with the provider error: {status:?}"
    );
    assert_eq!(finalized, 1, "the terminal commit must have been recorded");
    // `make_tool` hard-codes workspace_dir=/tmp (cost ledger lives there),
    // so only the tempdir holding the SQLite memory is scanned.
    assert_nothing_under_persists_the_probe_secret(tmp.path());
}

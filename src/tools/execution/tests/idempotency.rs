#![cfg(test)]

use super::*;

/// Reads the turn's idle beat from inside the tool, which is the only
/// vantage point from which the *reservation* write can be observed: it
/// happens after the call is announced and before the backend is polled.
struct BeatProbeTool {
    seen_before_invoke: Arc<Mutex<Option<crate::agent::idle::ProgressKind>>>,
}

#[async_trait]
impl Tool for BeatProbeTool {
    fn name(&self) -> &str {
        "native_write"
    }

    fn description(&self) -> &str {
        "records what the turn was last seen doing"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"value": {}},
            "required": ["value"]
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        *self.seen_before_invoke.lock() = crate::agent::idle::current_beat().map(|beat| beat.last_kind());
        Ok(ToolResult {
            success: true,
            output: "executed".to_string(),
            error: None,
        })
    }
}

/// The durable idempotency-ledger writes that bracket a side-effecting tool
/// call are progress, and are recorded as such.
///
/// Both of them are `spawn_blocking` hops into the database — work the turn
/// owes and is visibly doing — and both sit outside the `tool_start` /
/// `tool_end` pair, so until they were recorded a turn waiting on its own
/// ledger was indistinguishable from a turn that had gone quiet. That is
/// not a theoretical gap: it is where GitHub run 35396387399 found the
/// agent loop ("Last progress: tool_end after 14 event(s)" is the turn
/// inside `commit_idempotent_execution`).
///
/// MUTATION GUARD: delete either `ProgressKind::DurableWrite` beat and one
/// assertion below goes red. The reservation beat is read from inside the
/// backend, so nothing recorded later can stand in for it; the terminal
/// beat is read after the call returned, where `ToolEnd` would otherwise be
/// the last word.
#[tokio::test]
async fn the_durable_idempotency_ledger_writes_are_recorded_as_progress() {
    let temp = tempfile::tempdir().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
    let seen_before_invoke = Arc::new(Mutex::new(None));
    let stages = Arc::new(Mutex::new(Vec::new()));
    let service = ToolExecutionService::new(
        vec![Arc::new(BeatProbeTool {
            seen_before_invoke: Arc::clone(&seen_before_invoke),
        })],
        Arc::new(FixedPolicy {
            decision: ToolExecutionDecision::Allow,
            stages: Arc::clone(&stages),
        }),
        Arc::new(FixedApproval {
            decision: approved(),
            stages: Arc::clone(&stages),
        }),
        Arc::new(RecordingPreparation {
            stages: Arc::clone(&stages),
            allowed: true,
        }),
        Arc::new(RecordingAudit {
            records: Arc::new(Mutex::new(Vec::new())),
            stages: Arc::clone(&stages),
        }),
    )
    .with_idempotency_memory(memory);

    let beat = crate::agent::idle::child_beat();
    let outcome = crate::agent::idle::scope_beat(Some(Arc::clone(&beat)), async {
        service
            .execute(
                ToolExecutionCommand::new("native_write", serde_json::json!({"value":"ok"}))
                    .with_operation_id("ledger-progress")
                    .with_idempotency_key("ledger-progress"),
                context(),
                None,
            )
            .await
    })
    .await;

    assert_eq!(outcome.status, ToolExecutionStatus::Succeeded);
    assert_eq!(
        *seen_before_invoke.lock(),
        Some(crate::agent::idle::ProgressKind::DurableWrite),
        "the reservation write must be recorded before the backend is polled, so a turn \
             waiting on the ledger is not read as a silent one"
    );
    assert_eq!(
        beat.last_kind(),
        crate::agent::idle::ProgressKind::DurableWrite,
        "the terminal write is the last thing the call does; the turn must be seen doing it"
    );
}

#[tokio::test]
async fn same_provider_call_id_is_isolated_between_semantic_turns() {
    let temp = tempfile::tempdir().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
    let fixture = fixture();
    let service = fixture
        .service(
            "native_write",
            Vec::new(),
            ToolExecutionDecision::Allow,
            approved(),
            true,
        )
        .with_idempotency_memory(memory);
    let command = || {
        ToolExecutionCommand::new("native_write", serde_json::json!({"value":"repeat"})).with_idempotency_key("call_0")
    };

    let first = service
        .execute(command(), context().with_semantic_turn_id("turn-a"), None)
        .await;
    let second = service
        .execute(command(), context().with_semantic_turn_id("turn-b"), None)
        .await;

    assert_eq!(first.status, ToolExecutionStatus::Succeeded);
    assert_eq!(second.status, ToolExecutionStatus::Succeeded);
    assert!(!first.replayed);
    assert!(!second.replayed);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn idempotent_mutation_fails_closed_without_durable_ledger() {
    let fixture = fixture();
    let service = fixture.service(
        "native_write",
        Vec::new(),
        ToolExecutionDecision::Allow,
        approved(),
        true,
    );
    let command = || {
        ToolExecutionCommand::new("native_write", serde_json::json!({"value":"unsafe"}))
            .with_idempotency_key("call-without-ledger")
    };

    let first = service.execute(command(), context(), None).await;
    let second = service.execute(command(), context(), None).await;

    assert_eq!(first.status, ToolExecutionStatus::Indeterminate);
    assert_eq!(second.status, ToolExecutionStatus::Indeterminate);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

fn registry_observing_service(
    fixture: &Fixture,
    observed: &Arc<Mutex<Option<(crate::runtime::registry::WorkId, String, String)>>>,
    started: &Arc<tokio::sync::Notify>,
    wait_for_cancellation: bool,
) -> ToolExecutionService {
    ToolExecutionService::from_backends(
        vec![Arc::new(RegistryObservingBackend {
            observed: Arc::clone(observed),
            started: Arc::clone(started),
            wait_for_cancellation,
        })],
        Arc::new(FixedPolicy {
            decision: ToolExecutionDecision::Allow,
            stages: Arc::clone(&fixture.stages),
        }),
        Arc::new(FixedApproval {
            decision: approved(),
            stages: Arc::clone(&fixture.stages),
        }),
        Arc::new(RecordingPreparation {
            stages: Arc::clone(&fixture.stages),
            allowed: true,
        }),
        Arc::new(RecordingAudit {
            records: Arc::clone(&fixture.records),
            stages: Arc::clone(&fixture.stages),
        }),
    )
}

#[tokio::test]
async fn production_execute_registers_the_call_and_deregisters_on_return() {
    let fixture = fixture();
    let observed = Arc::new(Mutex::new(None));
    let started = Arc::new(tokio::sync::Notify::new());
    let service = registry_observing_service(&fixture, &observed, &started, false);

    let outcome = service
        .execute(
            ToolExecutionCommand::new("native_write", serde_json::json!({"value":"ok"})),
            context(),
            None,
        )
        .await;
    assert!(outcome.succeeded());

    let (id, name, run_id) = observed.lock().clone().expect("the call must register a work item");
    assert_eq!(name, "native_write", "the row must be named after the tool");
    assert_eq!(run_id, "task-a", "the row must carry the envelope's run correlation");
    assert!(
        crate::runtime::registry::snapshot(id).is_none(),
        "the row must be gone once execute returns"
    );
}

#[tokio::test]
async fn killing_a_registered_tool_call_cancels_only_that_call() {
    let fixture = fixture();
    let observed = Arc::new(Mutex::new(None));
    let started = Arc::new(tokio::sync::Notify::new());
    let service = Arc::new(registry_observing_service(&fixture, &observed, &started, true));

    // The turn token stands in for the surrounding agent turn: killing one
    // tool call must not cancel it.
    let turn_token = CancellationToken::new();
    let task = {
        let service = Arc::clone(&service);
        let turn_token = turn_token.clone();
        tokio::spawn(async move {
            service
                .execute(
                    ToolExecutionCommand::new("native_write", serde_json::json!({"value":"slow"})),
                    context(),
                    Some(turn_token),
                )
                .await
        })
    };

    started.notified().await;
    let id = observed
        .lock()
        .as_ref()
        .map(|(id, _, _)| *id)
        .expect("test: registered id");
    let results = crate::runtime::registry::kill(id, false).await;
    assert_eq!(
        results.first().map(|result| result.outcome),
        Some(crate::runtime::registry::KillOutcome::Killed),
        "the kill must be confirmed by the row disappearing"
    );

    let outcome = task.await.expect("test: join");
    assert_eq!(outcome.status, ToolExecutionStatus::Cancelled);
    assert!(
        !turn_token.is_cancelled(),
        "killing one tool call must not cancel the whole turn"
    );
}

#[tokio::test]
async fn cancellation_before_reservation_allows_same_idempotency_key_to_retry() {
    let temp = tempfile::tempdir().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
    let fixture = fixture();
    let service = fixture
        .service(
            "native_write",
            Vec::new(),
            ToolExecutionDecision::Allow,
            approved(),
            true,
        )
        .with_idempotency_memory(memory);
    let command = || {
        ToolExecutionCommand::new("native_write", serde_json::json!({"value":"after-cancel"}))
            .with_idempotency_key("cancel-before-reserve")
    };
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let cancelled = service.execute(command(), context(), Some(cancellation)).await;
    let retried = service.execute(command(), context(), None).await;

    assert_eq!(cancelled.status, ToolExecutionStatus::Cancelled);
    assert_eq!(retried.status, ToolExecutionStatus::Succeeded);
    assert!(!retried.replayed);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_after_reservation_abandons_before_backend_and_allows_retry() {
    let temp = tempfile::tempdir().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
    let fixture = fixture();
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(TestPostReservationGate {
        armed: AtomicBool::new(true),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let service = Arc::new(
        ToolExecutionService::from_backends(
            vec![Arc::new(PollRecordingBackend {
                calls: Arc::clone(&calls),
            })],
            Arc::new(FixedPolicy {
                decision: ToolExecutionDecision::Allow,
                stages: Arc::clone(&fixture.stages),
            }),
            Arc::new(FixedApproval {
                decision: approved(),
                stages: Arc::clone(&fixture.stages),
            }),
            Arc::new(RecordingPreparation {
                stages: Arc::clone(&fixture.stages),
                allowed: true,
            }),
            Arc::new(RecordingAudit {
                records: Arc::clone(&fixture.records),
                stages: Arc::clone(&fixture.stages),
            }),
        )
        .with_idempotency_memory(memory)
        .with_post_reservation_gate(Arc::clone(&gate)),
    );
    let command = || {
        ToolExecutionCommand::new("native_write", serde_json::json!({"value":"cancelled"}))
            .with_idempotency_key("cancel-after-reserve")
    };
    let cancellation = CancellationToken::new();
    let task = {
        let service = Arc::clone(&service);
        let cancellation = cancellation.clone();
        tokio::spawn(async move { service.execute(command(), context(), Some(cancellation)).await })
    };

    gate.entered.notified().await;
    cancellation.cancel();
    gate.release.notify_one();
    let cancelled = task.await.unwrap();
    let retried = service.execute(command(), context(), None).await;

    assert_eq!(cancelled.status, ToolExecutionStatus::Cancelled);
    assert!(!cancelled.replayed);
    assert_eq!(retried.status, ToolExecutionStatus::Succeeded);
    assert!(!retried.replayed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_after_backend_start_commits_one_terminal_and_never_reexecutes() {
    let temp = tempfile::tempdir().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
    let fixture = fixture();
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let service = Arc::new(
        ToolExecutionService::from_backends(
            vec![Arc::new(CancellationAwareBackend {
                calls: Arc::clone(&calls),
                started: Arc::clone(&started),
            })],
            Arc::new(FixedPolicy {
                decision: ToolExecutionDecision::Allow,
                stages: Arc::clone(&fixture.stages),
            }),
            Arc::new(FixedApproval {
                decision: approved(),
                stages: Arc::clone(&fixture.stages),
            }),
            Arc::new(RecordingPreparation {
                stages: Arc::clone(&fixture.stages),
                allowed: true,
            }),
            Arc::new(RecordingAudit {
                records: Arc::clone(&fixture.records),
                stages: Arc::clone(&fixture.stages),
            }),
        )
        .with_idempotency_memory(memory),
    );
    let command = || {
        ToolExecutionCommand::new("native_write", serde_json::json!({"value":"cancelled"}))
            .with_idempotency_key("cancel-after-start")
    };
    let cancellation = CancellationToken::new();
    let task = {
        let service = Arc::clone(&service);
        let cancellation = cancellation.clone();
        tokio::spawn(async move { service.execute(command(), context(), Some(cancellation)).await })
    };

    started.notified().await;
    cancellation.cancel();
    let cancelled = task.await.unwrap();
    let replay = service.execute(command(), context(), None).await;

    assert_eq!(cancelled.status, ToolExecutionStatus::Cancelled);
    assert!(!cancelled.replayed);
    assert_eq!(replay.status, ToolExecutionStatus::Cancelled);
    assert!(replay.replayed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn idempotency_key_reuse_with_different_command_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
    let fixture = fixture();
    let service = fixture
        .service(
            "native_write",
            Vec::new(),
            ToolExecutionDecision::Allow,
            approved(),
            true,
        )
        .with_idempotency_memory(memory);

    let first = service
        .execute(
            ToolExecutionCommand::new("native_write", serde_json::json!({"value":"first"}))
                .with_idempotency_key("reused-key"),
            context(),
            None,
        )
        .await;
    let conflict = service
        .execute(
            ToolExecutionCommand::new("native_write", serde_json::json!({"value":"second"}))
                .with_idempotency_key("reused-key"),
            context(),
            None,
        )
        .await;

    assert_eq!(first.status, ToolExecutionStatus::Succeeded);
    assert_eq!(conflict.status, ToolExecutionStatus::IdempotencyConflict);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn orphaned_idempotency_reservation_refuses_duplicate_side_effect() {
    let temp = tempfile::tempdir().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
    let fixture = fixture();
    let service = fixture
        .service(
            "native_write",
            Vec::new(),
            ToolExecutionDecision::Allow,
            approved(),
            true,
        )
        .with_idempotency_memory(Arc::clone(&memory));
    let command = ToolExecutionCommand::new("native_write", serde_json::json!({"value":"uncertain"}))
        .with_operation_id("recovery-call")
        .with_idempotency_key("recovery-key");
    let execution_context = context();
    let input_sha256 = format!("{:x}", Sha256::digest(command.arguments.to_string().as_bytes()));
    let (reservation_key, _) = tool_execution_ledger_keys(&execution_context, "recovery-key");
    let reservation = ToolExecutionReservation {
        operation_id: "crashed-owner".to_string(),
        reservation_id: "crashed-owner-token".to_string(),
        attempt: 0,
        capability: command.name.clone(),
        input_sha256,
    };
    memory
        .append_message_event(tool_execution_event_input(
            &execution_context,
            &reservation_key,
            "tool.execution.reserved",
            "crashed owner".to_string(),
            serde_json::to_string(&reservation).unwrap(),
        ))
        .await
        .unwrap();

    let outcome = service.execute(command, execution_context, None).await;

    assert_eq!(outcome.status, ToolExecutionStatus::Indeterminate);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn policy_deny_prevents_approval_preparation_and_execution_but_audits() {
    let fixture = fixture();
    let service = fixture.service(
        "native_write",
        Vec::new(),
        ToolExecutionDecision::Deny,
        approved(),
        true,
    );
    let outcome = service
        .execute(
            ToolExecutionCommand::new("native_write", serde_json::json!({"value":"blocked"})),
            context(),
            None,
        )
        .await;

    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(&*fixture.stages.lock(), &["policy", "audit"]);
    assert_eq!(fixture.records.lock().len(), 1);
}

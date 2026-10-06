#![cfg(test)]
//! Real service/SQLite execution and ledger outcomes, not a forged gate grant.
use super::*;
use crate::causal_tree::state::exact_calls::{BackendCompletion, LedgerDisposition};
use crate::tools::capability::execution_trace::{ExecutionTrace, ExecutionTraceSnapshot};

struct FailingTool(Arc<AtomicUsize>);
#[async_trait]
impl Tool for FailingTool {
    fn name(&self) -> &str {
        "native_write"
    }
    fn description(&self) -> &str {
        "controlled physical failure"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object", "required":["value"]})
    }
    async fn execute(&self, _arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            success: false,
            output: "controlled failure".into(),
            error: Some("controlled failure".into()),
        })
    }
}

fn traced_context(
    workspace: &std::path::Path,
    key: &Arc<crate::decision::DecisionKey>,
    source: &str,
) -> (ToolExecutionContext, ExecutionTrace) {
    let trace = ExecutionTrace::default();
    trace
        .bind_exact_call_key(Arc::clone(key))
        .expect("same real observation key");
    let mut context = ToolExecutionContext::new(
        RuntimeEnvelope::agent(workspace.to_string_lossy(), "prx_session")
            .with_owner_id("prx_owner")
            .with_run_id(source),
        "dm",
    );
    context.execution_trace = Some(trace.clone());
    (context, trace)
}

fn only_call(snapshot: &ExecutionTraceSnapshot) -> &crate::causal_tree::state::exact_calls::ExactCallObservation {
    let facts = snapshot
        .exact_call_observations
        .as_ref()
        .expect("actual service observed the exact call");
    facts.validate().expect("valid bounded facts");
    assert!(!facts.incomplete);
    assert_eq!(facts.calls.len(), 1);
    facts.calls.first().expect("one call")
}

#[tokio::test]
async fn failed_cached_call_is_not_another_physical_failure_and_fresh_ids_still_execute() {
    let temp = tempfile::tempdir().expect("temporary SQLite workspace");
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("SQLite"));
    let key = crate::decision::DecisionKey::ephemeral();
    let fixture = fixture();
    let service = ToolExecutionService::new(
        vec![Arc::new(FailingTool(Arc::clone(&fixture.calls)))],
        Arc::new(FixedPolicy {
            decision: ToolExecutionDecision::Allow,
            stages: Arc::clone(&fixture.stages),
        }),
        Arc::new(FixedApproval {
            decision: ToolApprovalDecision::Denied {
                reason: "unexpected approval request".into(),
            },
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
    .with_idempotency_memory(memory);
    let command = |id: &str, value: &str| {
        ToolExecutionCommand::new("native_write", serde_json::json!({"value":value}))
            .with_operation_id(id)
            .with_idempotency_key(id)
    };
    let (context, trace) = traced_context(temp.path(), &key, "prx_source");
    let first = service.execute(command("prx_call", "prx_payload"), context, None).await;
    assert_eq!(first.status, ToolExecutionStatus::Failed);
    assert!(!first.replayed);
    let first = trace.seal();
    assert_eq!(
        first
            .exact_call_observations
            .as_ref()
            .expect("facts")
            .physical_failure_count(),
        1
    );
    assert_eq!(only_call(&first).completion, BackendCompletion::Failed);
    assert!(only_call(&first).backend_started);
    assert_eq!(only_call(&first).ledger, LedgerDisposition::Reserved);

    let (context, trace) = traced_context(temp.path(), &key, "prx_source");
    let replay = service.execute(command("prx_call", "prx_payload"), context, None).await;
    assert_eq!(replay.status, ToolExecutionStatus::Failed);
    assert!(replay.replayed);
    let replay = trace.seal();
    assert_eq!(only_call(&replay).fingerprint, only_call(&first).fingerprint);
    assert_eq!(
        replay
            .exact_call_observations
            .as_ref()
            .expect("facts")
            .physical_failure_count(),
        0
    );
    assert_eq!(
        replay
            .exact_call_observations
            .as_ref()
            .expect("facts")
            .cached_return_count(),
        1
    );
    assert_eq!(only_call(&replay).ledger, LedgerDisposition::CachedTerminal);
    assert_eq!(only_call(&replay).completion, BackendCompletion::NotStarted);
    assert!(!only_call(&replay).backend_started);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);

    let (context, trace) = traced_context(temp.path(), &key, "prx_source");
    let conflict = service
        .execute(command("prx_call", "changed_payload"), context, None)
        .await;
    assert_eq!(conflict.status, ToolExecutionStatus::IdempotencyConflict);
    assert_eq!(only_call(&trace.seal()).ledger, LedgerDisposition::Conflict);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);

    for (id, source) in [("prx_fresh", "prx_source"), ("prx_call", "prx_new_source")] {
        let (context, trace) = traced_context(temp.path(), &key, source);
        let result = service.execute(command(id, "prx_payload"), context, None).await;
        assert_eq!(result.status, ToolExecutionStatus::Failed);
        assert!(!result.replayed);
        let snapshot = trace.seal();
        assert_eq!(only_call(&snapshot).completion, BackendCompletion::Failed);
        assert_ne!(only_call(&snapshot).fingerprint, only_call(&first).fingerprint);
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 3);
    let encoded = serde_json::to_string(&first).expect("serialize actual observations");
    assert!(!encoded.contains("prx_payload") && !encoded.contains("prx_owner") && !encoded.contains("prx_call"));
    let roundtrip: ExecutionTraceSnapshot = serde_json::from_str(&encoded).expect("strict trace reader");
    assert_eq!(roundtrip, first);
}

#[tokio::test]
async fn actual_unsent_abandonment_continues_but_dropped_reservation_stays_unknown() {
    for cancel_before_drop in [true, false] {
        let temp = tempfile::tempdir().expect("temporary SQLite workspace");
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("SQLite"));
        let key = crate::decision::DecisionKey::ephemeral();
        let fixture = fixture();
        let gate = Arc::new(TestPostReservationGate {
            armed: AtomicBool::new(true),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let service = fixture
            .service(
                "native_write",
                Vec::new(),
                ToolExecutionDecision::Allow,
                ToolApprovalDecision::Denied {
                    reason: "unexpected approval".into(),
                },
                true,
            )
            .with_idempotency_memory(memory)
            .with_post_reservation_gate(Arc::clone(&gate));
        let command = || {
            ToolExecutionCommand::new("native_write", serde_json::json!({"value":"prx_value"}))
                .with_operation_id("prx_call")
                .with_idempotency_key("prx_call")
        };
        let cancellation = CancellationToken::new();
        let (context, trace) = traced_context(temp.path(), &key, "prx_source");
        let mut work = Box::pin(service.execute(command(), context, Some(cancellation.clone())));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                result = work.as_mut() => panic!("reservation gate returned early: {result:?}"),
                () = gate.entered.notified() => {},
            }
        })
        .await
        .expect("actual reservation reached");
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        if cancel_before_drop {
            cancellation.cancel();
            gate.release.notify_one();
            let cancelled = tokio::time::timeout(std::time::Duration::from_secs(5), work.as_mut())
                .await
                .expect("owned cancellation");
            assert_eq!(cancelled.status, ToolExecutionStatus::Cancelled);
        }
        drop(work);
        let prior = trace.seal();
        let facts = prior.exact_call_observations.expect("reservation fact");
        assert!(
            facts
                .calls
                .iter()
                .all(|call| !call.backend_started && call.completion != BackendCompletion::Failed)
        );
        let (context, trace) = traced_context(temp.path(), &key, "prx_source");
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            service.execute(command(), context, None),
        )
        .await
        .expect("read prior ledger");
        let snapshot = trace.seal();
        let observed = only_call(&snapshot);
        if cancel_before_drop {
            assert_eq!(result.status, ToolExecutionStatus::Succeeded);
            assert_eq!(observed.ledger, LedgerDisposition::UnsentContinuation);
            assert_eq!(observed.reservation_attempt, Some(1));
            assert_eq!(observed.completion, BackendCompletion::Succeeded);
            assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        } else {
            assert_eq!(result.status, ToolExecutionStatus::Indeterminate);
            assert_eq!(observed.ledger, LedgerDisposition::Indeterminate);
            assert_eq!(observed.completion, BackendCompletion::NotStarted);
            assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        }
    }
}

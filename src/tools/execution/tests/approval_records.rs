#![cfg(test)]
//! Durable `tool.approval.resolved` rows on the real SQLite ledger.
use super::*;
use std::collections::VecDeque;

/// Bound on the whole test, including both service calls.
const TEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
/// Most approval rows read back in one bounded page; one more fails the test.
const MAX_APPROVAL_ROWS: usize = 16;
/// Most bytes one stored approval field may hold; more fails the test.
const MAX_FIELD_BYTES: usize = 4096;

/// Answers each approval request with the next scripted decision.
struct SequencedApproval(Mutex<VecDeque<ToolApprovalDecision>>);

#[async_trait]
impl ApprovalStrategy for SequencedApproval {
    async fn resolve(&self, _request: ToolApprovalRequest) -> ToolApprovalDecision {
        self.0
            .lock()
            .pop_front()
            .unwrap_or_else(|| ToolApprovalDecision::Denied {
                cause: ApprovalDenialCause::NoResolver,
                reason: "test: approval script exhausted".to_string(),
            })
    }
}

/// `(content, payload)` of every approval row of [`context`]'s workspace and
/// owner, in insert order, read through the SQLite repository in one page of
/// at most `MAX_APPROVAL_ROWS + 1` rows; overflow fails the test.
async fn approval_rows(memory: &dyn Memory) -> Vec<(String, serde_json::Value)> {
    let envelope = context().envelope;
    let owner_id = envelope.owner_id.expect("test: the fixture context has an owner");
    let rows = memory
        .list_owned_message_events_by_type(
            &envelope.workspace_id,
            &owner_id,
            super::super::approval_record::TOOL_APPROVAL_RESOLVED_EVENT,
            0,
            MAX_APPROVAL_ROWS + 1,
        )
        .await
        .expect("test: approval rows");
    assert!(
        rows.len() <= MAX_APPROVAL_ROWS,
        "more than {MAX_APPROVAL_ROWS} approval rows"
    );
    rows.into_iter()
        .map(|event| {
            let payload = event.raw_payload_json.expect("test: approval payload");
            for field in [&event.content, &payload] {
                assert!(
                    field.len() <= MAX_FIELD_BYTES,
                    "an approval row field exceeds {MAX_FIELD_BYTES} bytes"
                );
            }
            let payload = serde_json::from_str(&payload).expect("test: approval payload is JSON");
            (event.content, payload)
        })
        .collect()
}

/// Two calls that share one execution idempotency key in the same session and
/// turn each leave their own approval row: the approval-event key carries a
/// fresh resolution id, so the store's key deduplication cannot fold the
/// second resolution into the first.
#[tokio::test]
async fn approval_rows_survive_shared_idempotency_key() {
    tokio::time::timeout(TEST_DEADLINE, async {
        let temp = tempfile::tempdir().expect("test: ledger workspace");
        let memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("test: SQLite ledger"));
        let fixture = fixture();
        let tool = RecordingTool {
            root_name: "native_write",
            aliases: Vec::new(),
            calls: Arc::clone(&fixture.calls),
            arguments: Arc::clone(&fixture.arguments),
            stages: Arc::clone(&fixture.stages),
        };
        let service = ToolExecutionService::new(
            vec![Arc::new(tool)],
            Arc::new(FixedPolicy {
                decision: ToolExecutionDecision::Ask,
                stages: Arc::clone(&fixture.stages),
            }),
            Arc::new(SequencedApproval(Mutex::new(VecDeque::from([
                ToolApprovalDecision::Denied {
                    cause: ApprovalDenialCause::UserRejected,
                    reason: APPROVAL_DENIED_USER_REJECTED.to_string(),
                },
                approved(),
            ])))),
            Arc::new(RecordingPreparation {
                stages: Arc::clone(&fixture.stages),
                allowed: true,
            }),
            Arc::new(RecordingAudit {
                records: Arc::clone(&fixture.records),
                stages: Arc::clone(&fixture.stages),
            }),
        )
        .with_idempotency_memory(Arc::clone(&memory));
        let command = |operation_id: &str| {
            ToolExecutionCommand::new("native_write", serde_json::json!({"value": operation_id}))
                .with_operation_id(operation_id)
                .with_idempotency_key("prx-shared-key")
        };

        let denied = service.execute(command("prx-op-denied"), context(), None).await;
        assert_eq!(
            denied.status,
            ToolExecutionStatus::ApprovalDenied,
            "the first call is denied"
        );
        assert_eq!(
            denied.model_content,
            "Denied: User rejected tool approval. The native_write call did not run and produced no output.",
            "the model reads that the denied call did not run"
        );
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0, "a denied call never runs");
        let approved = service.execute(command("prx-op-approved"), context(), None).await;
        assert_eq!(
            approved.status,
            ToolExecutionStatus::Succeeded,
            "the second call is approved and runs"
        );
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1, "only the approved call runs");

        let rows = approval_rows(memory.as_ref()).await;
        let contents: Vec<&str> = rows.iter().map(|(content, _)| content.as_str()).collect();
        assert_eq!(
            contents,
            [
                "operation_id=prx-op-denied capability=native_write verdict=denied cause=user_rejected",
                "operation_id=prx-op-approved capability=native_write verdict=approved",
            ],
            "both approval outcomes stay observable"
        );
        let (_, denied_payload) = rows.first().expect("test: denied row");
        assert_eq!(denied_payload["verdict"], "denied", "denied payload verdict");
        assert_eq!(denied_payload["cause"], "user_rejected", "denied payload cause");
        assert_eq!(
            denied_payload["reason"], APPROVAL_DENIED_USER_REJECTED,
            "denied payload keeps the display reason"
        );
        let (_, approved_payload) = rows.get(1).expect("test: approved row");
        assert_eq!(approved_payload["verdict"], "approved", "approved payload verdict");
        assert!(approved_payload["cause"].is_null(), "an approval has no cause");
        assert_ne!(
            denied_payload["input_sha256"], approved_payload["input_sha256"],
            "each row fingerprints its own call input"
        );
    })
    .await
    .expect("test: approval_rows_survive_shared_idempotency_key within TEST_DEADLINE");
}

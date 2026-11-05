use super::keys::{ApprovalAuthority, DecisionAuthority};
use crate::causal_tree::ids::DecisionId;

#[test]
fn decision_keys_are_byte_identical_to_pre_extraction() {
    let id = DecisionId::parse("00000000-0000-0000-0000-000000000001").unwrap();
    let authority = DecisionAuthority(id);
    assert_eq!(
        authority.pending_key(),
        "decision-approval:00000000-0000-0000-0000-000000000001:pending"
    );
    assert_eq!(
        authority.review_key(),
        "decision-approval:00000000-0000-0000-0000-000000000001:review"
    );
    assert_eq!(
        authority.issued_key("grant_fixture"),
        "decision-approval:grant_fixture:issued"
    );
    assert_eq!(
        authority.resolution_key("fixture operation"),
        "decision-approval:00000000-0000-0000-0000-000000000001:ad3f377ae32bf3fca6b60175d758bb069396189058c30fbee26b15b9036e2f2a:resolution"
    );
    let binding = crate::acl::approval_grant::DecisionApprovalBinding {
        decision_id: "00000000-0000-0000-0000-000000000001".into(),
        catalog_generation: [0; 32],
        call_digest: [0; 32],
    };
    assert_eq!(
        super::keys::decision_resolution_key(&binding, "fixture operation"),
        "decision-approval:00000000-0000-0000-0000-000000000001:ad3f377ae32bf3fca6b60175d758bb069396189058c30fbee26b15b9036e2f2a:resolution"
    );
    assert_eq!(
        authority.resume_key("source_fixture"),
        "decision-approval:00000000-0000-0000-0000-000000000001:resume:source_fixture"
    );
}

// Captured baseline field order, optional-field omission and payload shapes.
// These fixtures carry no execution authority or real identity data.
#[test]
fn pending_and_review_payloads_keep_pre_extraction_bytes() {
    const PENDING: &str = r#"{"record":{"subject":{"agent_id":"prx_agent","principal_id":"prx_principal","owner_id":"prx_owner","workspace_id":"prx_workspace","session_key":"prx_session"},"binding":{"decision_id":"00000000-0000-0000-0000-000000000001","catalog_generation":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"call_digest":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]},"source_event_id":"prx_source","source_mac":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"run_id":"prx_run","decision_digests":[["00000000-0000-0000-0000-000000000001","prx_digest"]],"policy_version":"prx_policy","capabilities":[["prx_capability",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]]],"op_ids":["prx_operation"],"effect":"local_write","expires_at":"2030-01-01T00:00:00Z"},"mac":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}"#;
    const REVIEW: &str = r#"{"record":{"pending_mac":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"approved":false,"grants":[]},"mac":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}"#;
    // Synthetic populated wire fixtures pin optional fields and nonzero MAC bytes.
    // They are serialization data, never authenticated approval authority.
    const POPULATED_PENDING: &str = r#"{"record":{"h2_origin":{"version":1,"authority_expires_at":"2030-01-01T00:00:00Z","kind":"gateway","boot":"prx_boot","profile":"prx_profile","actor":"prx_owner","target_owner":"prx_owner","workspace":"prx_workspace","session":"prx_session","run":"prx_run","event":"prx_source","input":"prx_input","routing":"prx_routing","sequence":7},"subject":{"agent_id":"prx_agent","principal_id":"prx_principal","owner_id":"prx_owner","workspace_id":"prx_workspace","session_key":"prx_session"},"binding":{"decision_id":"00000000-0000-0000-0000-000000000001","catalog_generation":[32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63],"call_digest":[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31]},"source_event_id":"prx_source","source_mac":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32],"run_id":"prx_run","decision_digests":[["00000000-0000-0000-0000-000000000001","prx_digest"]],"policy_version":"prx_policy","capabilities":[["prx_capability",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]]],"op_ids":["prx_operation"],"effect":"local_write","risk":"High","schema_dialect":"gemini","sealed_call":{"version":1,"nonce":[1,2,3,4,5,6,7,8,9,10,11,12],"ciphertext":"01020304"},"expires_at":"2030-01-01T00:00:00Z"},"mac":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32]}"#;
    const POPULATED_REVIEW: &str = r#"{"record":{"local_owner":{"body":{"version":1,"boot":"prx_boot","profile":"prx_profile","original":{"version":1,"authority_expires_at":"2030-01-01T00:00:00Z","kind":"gateway","boot":"prx_boot","profile":"prx_profile","actor":"prx_owner","target_owner":"prx_owner","workspace":"prx_workspace","session":"prx_session","run":"prx_run","event":"prx_source","input":"prx_input","routing":"prx_routing","sequence":7},"pending_mac":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32],"pending_id":"00000000-0000-0000-0000-000000000001","call_digest":[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31],"args_sha256":"prx_arguments","capability_generation":[32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63],"catalog_generation":[32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63],"expires":"2030-01-01T00:00:00Z"},"mac":[32,31,30,29,28,27,26,25,24,23,22,21,20,19,18,17,16,15,14,13,12,11,10,9,8,7,6,5,4,3,2,1]},"pending_mac":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32],"approved":true,"grants":[{"grant_id":"prx_grant","version":2,"subject":{"agent_id":"prx_agent","principal_id":"prx_principal","owner_id":"prx_owner","workspace_id":"prx_workspace","session_key":"prx_session"},"issuer":{"authority":"HumanReview","authority_id":"prx_reviewer","public_key_id":"prx_witness"},"capability":{"op_id":"prx_operation","op_id_match":"Exact","risk_level":"High"},"resource_constraints":{"abs_path_prefix":"/prx_workspace","url_host_allowlist":["example.com"],"recipient_allowlist":["prx_recipient"],"max_payload_bytes":1024,"max_concurrent_calls":1},"issued_at":"2029-12-31T23:55:00Z","not_before":"2029-12-31T23:55:00Z","expires_at":"2030-01-01T00:00:00Z","max_uses":1,"uses_consumed":0,"witness_signature":{"alg":"Ed25519","signature_b64":"cHJ4X2ZpeHR1cmU=","signed_payload_sha256":"prx_signed_payload"},"related_task_id":"prx_task","related_message_event_id":9,"revoked_at":"2030-01-01T00:01:00Z","revocation_reason":"prx_fixture","decision_binding":{"decision_id":"00000000-0000-0000-0000-000000000001","catalog_generation":[32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63],"call_digest":[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31]}}]},"mac":[32,31,30,29,28,27,26,25,24,23,22,21,20,19,18,17,16,15,14,13,12,11,10,9,8,7,6,5,4,3,2,1]}"#;
    for expected in [PENDING, POPULATED_PENDING] {
        let pending: super::pending::SignedPending = serde_json::from_str(expected).unwrap();
        assert_eq!(serde_json::to_string(&pending).unwrap(), expected);
    }
    for expected in [REVIEW, POPULATED_REVIEW] {
        let review: super::review::SignedReview = serde_json::from_str(expected).unwrap();
        assert_eq!(serde_json::to_string(&review).unwrap(), expected);
    }
}

#[tokio::test]
async fn pre_extraction_pending_and_issued_events_are_resolved_by_the_ledger() {
    Box::pin(crate::runtime::turn_decision::historical_approval_events()).await;
}

#[tokio::test]
async fn review_denial_blocks_issue() {
    Box::pin(crate::runtime::turn_decision::exact_file_collection_for_review(false)).await;
}

// Field order and omission follow the writer on 663db817, approval.rs:20-44.
#[test]
fn consumed_grant_claim_keeps_pre_extraction_bytes() {
    const EXPECTED: &str = r#"{"disposition":"consumed","grant_id":"prx_grant","signed_payload_sha256":"prx_signed_payload","binding":{"decision_id":"00000000-0000-0000-0000-000000000001","catalog_generation":[32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63],"call_digest":[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31]},"nonce":"prx_nonce","execution":{"decision_id":"prx_resume","source_event_id":"prx_source","run_id":"prx_run"}}"#;
    let value: super::resolution::GrantClaim = serde_json::from_str(EXPECTED).unwrap();
    let serialized = serde_json::to_string(&value).unwrap();
    assert_eq!(serialized, EXPECTED);
    let restored: super::resolution::GrantClaim = serde_json::from_str(&serialized).unwrap();
    assert!(value == restored);
}

// Field order and omission follow the writer on 663db817, approval.rs:20-44.
#[test]
fn revoked_grant_claim_keeps_pre_extraction_bytes() {
    const EXPECTED: &str = r#"{"disposition":"revoked","grant_id":"prx_grant","signed_payload_sha256":"prx_signed_payload","binding":{"decision_id":"00000000-0000-0000-0000-000000000001","catalog_generation":[32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63],"call_digest":[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31]},"nonce":"prx_nonce"}"#;
    let value: super::resolution::GrantClaim = serde_json::from_str(EXPECTED).unwrap();
    let serialized = serde_json::to_string(&value).unwrap();
    assert_eq!(serialized, EXPECTED);
    let restored: super::resolution::GrantClaim = serde_json::from_str(&serialized).unwrap();
    assert!(value == restored);
}

// Field order follows the writer on 663db817, pending_approval.rs:728-742.
#[test]
fn signed_resume_intent_keeps_pre_extraction_bytes() {
    const EXPECTED: &str = r#"{"record":{"binding":{"decision_id":"00000000-0000-0000-0000-000000000001","catalog_generation":[32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63],"call_digest":[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31]},"source_event_id":"prx_source","source_mac":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32],"run_id":"prx_run","policy_version":"prx_policy","reviewed_grants_mac":[32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63]},"mac":[32,31,30,29,28,27,26,25,24,23,22,21,20,19,18,17,16,15,14,13,12,11,10,9,8,7,6,5,4,3,2,1]}"#;
    let value: super::resume::SignedResumeIntent = serde_json::from_str(EXPECTED).unwrap();
    let serialized = serde_json::to_string(&value).unwrap();
    assert_eq!(serialized, EXPECTED);
    let restored: super::resume::SignedResumeIntent = serde_json::from_str(&serialized).unwrap();
    assert!(value == restored);
}

use super::pending::{PendingRecord, VerifiedPendingDecisionApproval, scope_from_source};
use super::resolution::ResumeClaimScope;
use super::{ApprovalLedger, ApprovalRuntime, GrantScope};
use crate::acl::approval_grant::{ApprovalGrantV2, DecisionApprovalBinding, RiskLevel, Subject, WitnessKeyring};
use crate::causal_tree::store::{CausalDecisionStore, DecisionScope, Retention, StoreMode};
use crate::decision::{DecisionKey, KeyDomain};
use crate::memory::{Memory, MemoryFabric, MemoryVisibility, MessageEventScope, SqliteMemory};
use std::sync::Arc;

// This port isolates ledger persistence; runtime admission is outside this test scope.
struct LedgerTestPort;
#[async_trait::async_trait]
impl ApprovalRuntime for LedgerTestPort {
    fn check_live_parity(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn verify_pending_console_file_operation(&self, _: &VerifiedPendingDecisionApproval) -> anyhow::Result<()> {
        Ok(())
    }
    fn open_pending_call(
        &self,
        _: &VerifiedPendingDecisionApproval,
    ) -> anyhow::Result<crate::tools::ToolExecutionCommand> {
        anyhow::bail!("fixture has no sealed call")
    }
    fn verify_h2_review_requirement(
        &self,
        _: &VerifiedPendingDecisionApproval,
        _: bool,
        _: Option<&crate::runtime::h2_entry::LocalOwnerReviewStamp>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn retire_pending(&self, _: &PendingRecord) {}
}

struct LifecycleFixture {
    memory: Arc<dyn Memory>,
    key: Arc<DecisionKey>,
    store: Arc<CausalDecisionStore>,
    subject: Subject,
    binding: DecisionApprovalBinding,
    scope: MessageEventScope,
    grant: ApprovalGrantV2,
}

impl LifecycleFixture {
    async fn new(path: &std::path::Path) -> Self {
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(path).unwrap());
        let key = DecisionKey::load_or_create(path).unwrap();
        let store = CausalDecisionStore::open_shared(
            path,
            Retention::days(0, 0),
            Arc::new(crate::decision::telemetry::testing::RecordingSink::default()),
            StoreMode::ReadWrite,
        )
        .unwrap();
        let workspace = store.workspace_id().unwrap();
        let subject = Subject {
            agent_id: "prx_agent".into(),
            principal_id: "prx_principal".into(),
            owner_id: "prx_owner".into(),
            workspace_id: workspace.to_string(),
            session_key: Some("prx_session".into()),
        };
        let id = DecisionId::new_runtime();
        let binding = DecisionApprovalBinding {
            decision_id: id.to_string(),
            catalog_generation: [7; 32],
            call_digest: [8; 32],
        };
        let mut scope = MessageEventScope::new("cli", MemoryVisibility::Private);
        scope.owner_id = Some(subject.owner_id.clone());
        scope.session_key = subject.session_key.clone();
        scope.run_id = Some("prx_run".into());
        let source = MemoryFabric::new(memory.clone(), workspace.to_string())
            .record_inbound_user_message(scope, "project operation", None, None)
            .await
            .unwrap();
        let scope = scope_from_source(&source);
        let mut row = crate::causal_tree::store::testing::record(
            id,
            &workspace,
            Some(&subject.owner_id),
            chrono::Utc::now().timestamp_millis(),
        );
        row.lineage.session_key = Some(Arc::from("prx_session"));
        row.lineage.run_id = Some(Arc::from("prx_run"));
        let writer = store.writer().unwrap();
        writer.record_decision(row);
        assert!(
            writer
                .drain(std::time::Instant::now() + std::time::Duration::from_secs(5))
                .await
        );
        let digest = store
            .reader()
            .replay_record(
                &DecisionScope::Owner {
                    workspace_id: workspace.clone(),
                    owner_id: Arc::from("prx_owner"),
                },
                &id,
            )
            .unwrap()
            .unwrap()
            .digest;
        let expires_at = chrono::Utc::now() + chrono::Duration::minutes(4);
        let grant = ApprovalGrantV2::issue_decision_one_shot(
            WitnessKeyring::global_for_tests(),
            subject.clone(),
            "prx_operation",
            RiskLevel::Medium,
            binding.clone(),
            expires_at,
        )
        .unwrap();
        let fixture = Self {
            memory,
            key,
            store,
            subject,
            binding,
            scope,
            grant,
        };
        fixture
            .ledger()
            .persist_pending_record_receipt(
                PendingRecord {
                    h2_origin: None,
                    subject: fixture.subject.clone(),
                    binding: fixture.binding.clone(),
                    source_event_id: source.event_id.clone(),
                    source_mac: fixture.key.hmac(
                        KeyDomain::ApprovalState,
                        &[b"source", &serde_json::to_vec(&source).unwrap()],
                    ),
                    run_id: "prx_run".into(),
                    decision_digests: vec![(id.to_string(), digest)],
                    policy_version: "prx_policy".into(),
                    capabilities: vec![],
                    op_ids: vec!["prx_operation".into()],
                    effect: "local_write".into(),
                    risk: None,
                    schema_dialect: None,
                    sealed_call: None,
                    expires_at,
                },
                &source,
            )
            .await
            .unwrap();
        fixture
            .ledger()
            .store_decision_grant(
                fixture.scope.clone(),
                &fixture.grant,
                &fixture.expected(),
                WitnessKeyring::global_for_tests(),
            )
            .await
            .unwrap();
        fixture
    }
    fn ledger(&self) -> ApprovalLedger<'_> {
        ApprovalLedger {
            memory: &self.memory,
            state_key: &self.key,
            store: &self.store,
            workspace: self.subject.workspace_id.clone(),
            catalog_generation: self.binding.catalog_generation,
            policy_version: "prx_policy",
            enabled: true,
            port: &LedgerTestPort,
        }
    }
    fn expected(&self) -> GrantScope<'_> {
        GrantScope {
            subject: &self.subject,
            binding: &self.binding,
            op_id: "prx_operation",
            risk: RiskLevel::Medium,
        }
    }
    async fn claim(&self) -> anyhow::Result<crate::memory::MessageEvent> {
        self.ledger()
            .claim_decision_grant(
                self.scope.clone(),
                &self.grant,
                &self.expected(),
                WitnessKeyring::global_for_tests(),
                ResumeClaimScope {
                    decision_id: "prx_resume".into(),
                    source_event_id: self.scope.causation_event_id.clone().unwrap(),
                    run_id: "prx_run".into(),
                },
            )
            .await
    }
    async fn revoke(&self) -> anyhow::Result<crate::memory::MessageEvent> {
        self.ledger()
            .revoke_decision_grant(
                self.scope.clone(),
                &self.grant,
                &self.expected(),
                WitnessKeyring::global_for_tests(),
            )
            .await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_and_revoke_race_has_exactly_one_winner() {
    let workspace = tempfile::tempdir().unwrap();
    let fixture = Arc::new(LifecycleFixture::new(workspace.path()).await);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let claimant = fixture.clone();
    let claim_barrier = barrier.clone();
    let claim = tokio::spawn(async move {
        claim_barrier.wait().await;
        claimant.claim().await
    });
    let revoker = fixture.clone();
    let revoke = tokio::spawn(async move {
        barrier.wait().await;
        revoker.revoke().await
    });
    let claimed = claim.await.unwrap();
    let revoked = revoke.await.unwrap();
    assert_ne!(claimed.is_ok(), revoked.is_ok(), "exactly one resolution wins");
    let key = super::keys::decision_resolution_key(&fixture.binding, "prx_operation");
    let durable = fixture
        .memory
        .find_message_event_by_idempotency_key(&fixture.subject.workspace_id, &key)
        .await
        .unwrap()
        .unwrap();
    let winner = claimed.as_ref().or(revoked.as_ref()).unwrap();
    assert_eq!(durable.event_id, winner.event_id);
    let payload: serde_json::Value = serde_json::from_str(durable.raw_payload_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        payload.get("disposition").expect("durable disposition"),
        if claimed.is_ok() { "consumed" } else { "revoked" }
    );
    assert!(fixture.claim().await.is_err());
}

#[tokio::test]
async fn consumed_grant_cannot_be_revoked_or_reclaimed_after_restart() {
    let workspace = tempfile::tempdir().unwrap();
    let mut fixture = LifecycleFixture::new(workspace.path()).await;
    let consumed = fixture.claim().await.unwrap();
    // Release the last SQLite memory handle before reopening the same database.
    drop(fixture.memory);
    fixture.memory = Arc::new(SqliteMemory::new(workspace.path()).unwrap());
    assert!(fixture.revoke().await.is_err(), "consumption cannot become revocation");
    assert!(
        fixture.claim().await.is_err(),
        "a new nonce cannot reclaim a consumed grant"
    );
    let key = super::keys::decision_resolution_key(&fixture.binding, "prx_operation");
    let durable = fixture
        .memory
        .find_message_event_by_idempotency_key(&fixture.subject.workspace_id, &key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.event_id, consumed.event_id);
    assert_eq!(durable.raw_payload_json, consumed.raw_payload_json);
}

#[tokio::test]
async fn revoked_grant_cannot_be_claimed_after_restart() {
    let workspace = tempfile::tempdir().unwrap();
    let mut fixture = LifecycleFixture::new(workspace.path()).await;
    let revoked = fixture.revoke().await.unwrap();
    drop(fixture.memory);
    fixture.memory = Arc::new(SqliteMemory::new(workspace.path()).unwrap());
    assert!(fixture.claim().await.is_err());
    let repeated = fixture.revoke().await.unwrap();
    assert_eq!(repeated.event_id, revoked.event_id);
    assert_eq!(repeated.raw_payload_json, revoked.raw_payload_json);
}

#[tokio::test]
async fn wrong_owner_or_session_cannot_review_or_claim() {
    let workspace = tempfile::tempdir().unwrap();
    let fixture = LifecycleFixture::new(workspace.path()).await;
    let id = DecisionId::parse(&fixture.binding.decision_id).unwrap();
    // A valid pending read proves the negative reviews have a real authenticated target.
    fixture
        .ledger()
        .load_pending_decision_approval("prx_owner", "prx_session", id)
        .await
        .unwrap();
    for (owner, session) in [("prx_other_owner", "prx_session"), ("prx_owner", "prx_other_session")] {
        let error = fixture
            .ledger()
            .review_pending_decision_approval(owner, session, id, true)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("belongs to another scope"), "{error}");
        let mut scope = fixture.scope.clone();
        scope.owner_id = Some(owner.into());
        scope.session_key = Some(session.into());
        let error = fixture
            .ledger()
            .claim_decision_grant(
                scope,
                &fixture.grant,
                &fixture.expected(),
                WitnessKeyring::global_for_tests(),
                ResumeClaimScope {
                    decision_id: "prx_resume".into(),
                    source_event_id: fixture.scope.causation_event_id.clone().unwrap(),
                    run_id: "prx_run".into(),
                },
            )
            .await
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("scope differs from pinned authority"),
            "{error}"
        );
    }
    assert!(
        fixture
            .memory
            .find_message_event_by_idempotency_key(&fixture.subject.workspace_id, &DecisionAuthority(id).review_key())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fixture.claim().await.is_ok(),
        "rejected attempts must not consume the grant"
    );
}

use super::keys::RunCallAuthority;
use super::run_call::{ApprovalLedgerPort, PendingCall};

struct CallFixture {
    memory: Arc<dyn Memory>,
    key: Arc<DecisionKey>,
    pending: PendingCall,
}
impl CallFixture {
    async fn new(path: &std::path::Path) -> Self {
        Self::in_workspace(path, "prx_workspace").await
    }
    async fn in_workspace(path: &std::path::Path, workspace: &str) -> Self {
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(path).unwrap());
        let key = DecisionKey::load_or_create(path).unwrap();
        let mut scope = MessageEventScope::new("cli", MemoryVisibility::Private);
        scope.owner_id = Some("prx_owner".into());
        scope.session_key = Some("prx_session".into());
        scope.run_id = Some("prx_run".into());
        let source = MemoryFabric::new(memory.clone(), workspace)
            .record_inbound_user_message(scope, "project call", None, None)
            .await
            .unwrap();
        let mut envelope = crate::runtime::envelope::RuntimeEnvelope::console(workspace, "prx_session");
        envelope.session_key = "prx_session".into();
        envelope.owner_id = Some("prx_owner".into());
        envelope.run_id = source.run_id.clone();
        envelope.source_message_event_id = Some(source.event_id.clone());
        let pending = ApprovalLedgerPort {
            memory: &memory,
            state_key: &key,
            workspace,
            keyring: WitnessKeyring::global_for_tests(),
        }
        .record_pending(
            &envelope,
            &call_capability(),
            "memory_reindex",
            &serde_json::json!({"path":"project_a"}),
        )
        .await
        .unwrap();
        Self { memory, key, pending }
    }
    fn port(&self) -> ApprovalLedgerPort<'_> {
        ApprovalLedgerPort {
            memory: &self.memory,
            state_key: &self.key,
            workspace: &self.pending.subject().workspace_id,
            keyring: WitnessKeyring::global_for_tests(),
        }
    }
    async fn review(&self) {
        self.port()
            .review("prx_owner", "prx_session", self.pending.authority(), true)
            .await
            .unwrap();
    }
    async fn claim(&self) -> anyhow::Result<crate::memory::MessageEvent> {
        self.port()
            .claim("prx_owner", "prx_session", self.pending.authority())
            .await
    }
}
fn call_authority(source: &crate::memory::MessageEvent, args: &serde_json::Value) -> RunCallAuthority {
    RunCallAuthority::for_call(
        &call_capability(),
        crate::tools::capability::execution_binding::CallDigestSource {
            workspace: &source.workspace_id,
            owner: source.owner_id.as_deref().unwrap(),
            session: source.session_key.as_deref().unwrap(),
            run: source.run_id.as_deref(),
            event: Some(&source.event_id),
            channel: source.channel.as_deref(),
            sender: source.sender.as_deref(),
            generation: source.config_generation_id,
            revision: source.config_source_revision.as_deref(),
        },
        args,
        "memory_reindex:rebuild",
        RiskLevel::Medium,
    )
    .unwrap()
}

#[tokio::test]
async fn run_keyed_pending_survives_restart_and_claims_once() {
    let path = tempfile::tempdir().unwrap();
    let fixture = CallFixture::new(path.path()).await;
    let pending = fixture.pending.clone();
    drop(fixture);
    let fixture = CallFixture {
        memory: Arc::new(SqliteMemory::new(path.path()).unwrap()),
        key: DecisionKey::load_or_create(path.path()).unwrap(),
        pending,
    };
    assert_eq!(
        fixture
            .port()
            .list_pending_calls("prx_owner", 0, 10)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    fixture.review().await;
    drop(fixture);
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(path.path()).unwrap());
    let key = DecisionKey::load_or_create(path.path()).unwrap();
    let port = ApprovalLedgerPort {
        memory: &memory,
        state_key: &key,
        workspace: "prx_workspace",
        keyring: WitnessKeyring::global_for_tests(),
    };
    let page = port.list_pending_calls("prx_owner", 0, 10).await.unwrap();
    let authority = page.items.first().unwrap().authority();
    port.claim("prx_owner", "prx_session", authority).await.unwrap();
    assert_eq!(
        port.claim("prx_owner", "prx_session", authority)
            .await
            .unwrap_err()
            .to_string(),
        "call approval was already consumed or revoked"
    );
    assert!(
        port.list_pending_calls("prx_owner", 0, 10)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_keyed_claim_and_revoke_race_has_exactly_one_winner() {
    let path = tempfile::tempdir().unwrap();
    let fixture = CallFixture::new(path.path()).await;
    fixture.review().await;
    let other_memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(path.path()).unwrap());
    let port = ApprovalLedgerPort {
        memory: &other_memory,
        state_key: &fixture.key,
        workspace: "prx_workspace",
        keyring: WitnessKeyring::global_for_tests(),
    };
    let (claim, revoke) = tokio::join!(
        fixture.claim(),
        port.revoke("prx_owner", "prx_session", fixture.pending.authority())
    );
    assert_ne!(claim.is_ok(), revoke.is_ok());
    let loser = if let Err(error) = claim {
        error
    } else {
        revoke.unwrap_err()
    };
    assert_eq!(loser.to_string(), "call approval was already consumed or revoked");
}

#[tokio::test]
async fn changed_arguments_do_not_match_the_pending_digest() {
    let path = tempfile::tempdir().unwrap();
    let fixture = CallFixture::new(path.path()).await;
    fixture.review().await;
    let source = fixture
        .memory
        .find_owned_message_event("prx_workspace", "prx_owner", fixture.pending.source_event_id())
        .await
        .unwrap()
        .unwrap();
    let changed = call_authority(&source, &serde_json::json!({"path":"project_b"}));
    assert_ne!(changed, *fixture.pending.authority());
    assert_eq!(
        fixture
            .port()
            .claim("prx_owner", "prx_session", &changed)
            .await
            .unwrap_err()
            .to_string(),
        "pending call approval not found"
    );
    fixture.claim().await.unwrap();
}

#[tokio::test]
async fn expired_run_keyed_grant_cannot_be_claimed() {
    let path = tempfile::tempdir().unwrap();
    let fixture = CallFixture::new(path.path()).await;
    fixture.review().await;
    let authority = fixture.pending.authority();
    let event = fixture
        .memory
        .find_message_event_by_idempotency_key("prx_workspace", &authority.issued_key(""))
        .await
        .unwrap()
        .unwrap();
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Review {
        pending_mac: [u8; 32],
        approved: bool,
        grant: Option<ApprovalGrantV2>,
    }
    #[derive(serde::Serialize, serde::Deserialize)]
    struct SignedReview {
        record: Review,
        mac: [u8; 32],
    }
    let mut payload: SignedReview = serde_json::from_str(event.raw_payload_json.as_deref().unwrap()).unwrap();
    let grant = payload.record.grant.as_mut().unwrap();
    grant.expires_at = chrono::Utc::now() - chrono::Duration::seconds(1);
    crate::acl::approval_grant::sign_grant(WitnessKeyring::global_for_tests(), grant).unwrap();
    payload.mac = fixture.key.hmac(
        KeyDomain::ApprovalState,
        &[b"call.review", &serde_json::to_vec(&payload.record).unwrap()],
    );
    SqliteMemory::new(path.path())
        .unwrap()
        .replace_event_payload_for_test(&event.event_id, &serde_json::to_string(&payload).unwrap())
        .unwrap();
    assert_eq!(
        fixture.claim().await.unwrap_err().to_string(),
        "approval grant is outside its validity window"
    );
    assert!(
        fixture
            .memory
            .find_message_event_by_idempotency_key("prx_workspace", &authority.resolution_key(""))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn run_keyed_and_decision_keyed_listings_do_not_mix() {
    let path = tempfile::tempdir().unwrap();
    let decision = LifecycleFixture::new(path.path()).await;
    let fixture = CallFixture::in_workspace(path.path(), &decision.subject.workspace_id).await;
    let page = fixture.port().list_pending_calls("prx_owner", 0, 1).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(*page.items.first().unwrap().authority(), *fixture.pending.authority());
    assert!(page.next_cursor.is_some());
    assert!(
        fixture
            .port()
            .list_pending_calls("prx_other_owner", 0, 10)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let decisions = decision
        .ledger()
        .list_pending_decision_approvals("prx_owner", 0, 10)
        .await
        .unwrap();
    let raw_decisions = decision
        .memory
        .list_owned_message_events_by_type(
            &decision.subject.workspace_id,
            "prx_owner",
            "approval.decision.pending",
            0,
            100,
        )
        .await
        .unwrap();
    assert_eq!(
        raw_decisions.len(),
        1,
        "call events must not change the raw decision event count"
    );
    assert_eq!(decisions.items.len(), 1);
    assert_eq!(
        decisions.items.first().unwrap().view.decision_id,
        decision.binding.decision_id
    );
}

#[tokio::test]
async fn run_keyed_review_by_wrong_owner_is_refused() {
    let path = tempfile::tempdir().unwrap();
    let fixture = CallFixture::new(path.path()).await;
    for (owner, session) in [("prx_other_owner", "prx_session"), ("prx_owner", "prx_other_session")] {
        assert_eq!(
            fixture
                .port()
                .review(owner, session, fixture.pending.authority(), true)
                .await
                .unwrap_err()
                .to_string(),
            "pending call approval belongs to another scope"
        );
        assert_eq!(
            fixture
                .port()
                .claim(owner, session, fixture.pending.authority())
                .await
                .unwrap_err()
                .to_string(),
            "pending call approval belongs to another scope"
        );
    }
    fixture.review().await;
    fixture.claim().await.unwrap();
}

#[tokio::test]
async fn decision_claim_checks_stored_grant_owner() {
    let path = tempfile::tempdir().unwrap();
    let fixture = LifecycleFixture::new(path.path()).await;
    SqliteMemory::new(path.path())
        .unwrap()
        .replace_event_owner_for_test(
            &format!("decision-approval:{}:issued", fixture.grant.grant_id),
            "prx_other_owner",
        )
        .unwrap();
    assert_eq!(
        fixture.claim().await.unwrap_err().to_string(),
        "stored approval belongs to another scope"
    );
}

#[test]
fn run_keyed_authorities_cannot_share_delimited_keys() {
    let first = RunCallAuthority {
        run_id: "prx_run".into(),
        call_digest: [1; 32],
    };
    let second = RunCallAuthority {
        run_id: format!("prx_run:{}:resume:prx_source", hex::encode(first.call_digest)),
        call_digest: [2; 32],
    };
    assert_ne!(
        first.resume_key(&format!("prx_source:{}:pending", hex::encode(second.call_digest))),
        second.pending_key()
    );
    assert_ne!(first.pending_key(), second.pending_key());
}

#[tokio::test]
async fn corrupt_run_keyed_pending_is_skipped() {
    let path = tempfile::tempdir().unwrap();
    let fixture = CallFixture::new(path.path()).await;
    let event = fixture
        .memory
        .find_message_event_by_idempotency_key("prx_workspace", &fixture.pending.authority().pending_key())
        .await
        .unwrap()
        .unwrap();
    SqliteMemory::new(path.path())
        .unwrap()
        .replace_event_payload_for_test(&event.event_id, "invalid")
        .unwrap();
    let page = fixture.port().list_pending_calls("prx_owner", 0, 10).await.unwrap();
    assert!(page.items.is_empty());
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn corrupt_run_keyed_review_is_skipped() {
    let path = tempfile::tempdir().unwrap();
    let fixture = CallFixture::new(path.path()).await;
    fixture.review().await;
    let event = fixture
        .memory
        .find_message_event_by_idempotency_key("prx_workspace", &fixture.pending.authority().review_key())
        .await
        .unwrap()
        .unwrap();
    SqliteMemory::new(path.path())
        .unwrap()
        .replace_event_payload_for_test(&event.event_id, "invalid")
        .unwrap();
    let page = fixture.port().list_pending_calls("prx_owner", 0, 10).await.unwrap();
    assert!(page.items.is_empty());
    assert!(page.next_cursor.is_none());
}

fn call_capability() -> crate::causal_tree::branch::CapabilityRef {
    use crate::causal_tree::branch::*;
    CapabilityRef {
        id: CapabilityId::new(CapabilityTarget::NativeTool, "prx", "memory_reindex").unwrap(),
        fingerprint: SchemaFingerprint([7; 32]),
        generation: CatalogGeneration([8; 32]),
    }
}

#[tokio::test]
async fn run_keyed_pending_uses_durable_source_identity() {
    let path = tempfile::tempdir().unwrap();
    let fixture = CallFixture::new(path.path()).await;
    let source = fixture
        .memory
        .find_owned_message_event("prx_workspace", "prx_owner", fixture.pending.source_event_id())
        .await
        .unwrap()
        .unwrap();
    let original = call_authority(&source, &serde_json::json!({"path":"project_a"}));
    for field in 0..3 {
        let mut changed = source.clone();
        match field {
            0 => changed.channel = Some("prx_channel".into()),
            1 => changed.sender = Some("prx_sender".into()),
            _ => changed.config_generation_id = Some(42),
        }
        let mut scope = MessageEventScope::new("cli", MemoryVisibility::Private);
        scope.owner_id = changed.owner_id;
        scope.session_key = changed.session_key;
        scope.run_id = changed.run_id;
        scope.channel = changed.channel;
        scope.sender = changed.sender;
        scope.config_generation_id = changed.config_generation_id;
        let durable = MemoryFabric::new(fixture.memory.clone(), "prx_workspace")
            .record_inbound_user_message(scope, "changed source", None, None)
            .await
            .unwrap();
        let mut envelope = crate::runtime::envelope::RuntimeEnvelope::console("prx_workspace", "prx_session");
        envelope.session_key = "prx_session".into();
        envelope.owner_id = Some("prx_owner".into());
        envelope.run_id = durable.run_id.clone();
        envelope.source_message_event_id = Some(durable.event_id.clone());
        let pending = fixture
            .port()
            .record_pending(
                &envelope,
                &call_capability(),
                "memory_reindex",
                &serde_json::json!({"path":"project_a"}),
            )
            .await
            .unwrap();
        assert_ne!(pending.authority(), &original);
        fixture
            .port()
            .review("prx_owner", "prx_session", pending.authority(), true)
            .await
            .unwrap();
    }
    assert!(
        fixture
            .port()
            .claim("prx_owner", "prx_session", &original)
            .await
            .is_err()
    );
}

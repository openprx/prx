use crate::acl::approval_grant::{ApprovalGrantV2, RiskLevel, WitnessKeyring};
use crate::memory::{Memory, MemoryFabric, MemoryVisibility, MessageEventScope, SqliteMemory};
use crate::security::runtime_key::{DecisionKey, KeyDomain};
use std::sync::Arc;

use super::keys::{ApprovalAuthority, RunCallAuthority};
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
        .find_message_event_by_idempotency_key("prx_workspace", &authority.issued_key())
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
            .find_message_event_by_idempotency_key("prx_workspace", &authority.resolution_key())
            .await
            .unwrap()
            .is_none()
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
    assert_ne!(first.issued_key(), second.pending_key());
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

fn call_capability() -> crate::tools::capability::contract::CapabilityRef {
    use crate::tools::capability::contract::*;
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
    assert_eq!(&original, fixture.pending.authority());
    for field in 0..3 {
        let mut changed = source.clone();
        match field {
            0 => changed.channel = Some("prx_channel".into()),
            1 => changed.sender = Some("prx_sender".into()),
            _ => changed.config_generation_id = Some(42),
        }
        assert_eq!(changed.event_id, source.event_id);
        assert_ne!(
            call_authority(&changed, &serde_json::json!({"path":"project_a"})),
            original
        );
    }
    fixture.review().await;
    SqliteMemory::new(path.path())
        .unwrap()
        .replace_event_channel_for_test(&source.event_id, "prx_changed_channel")
        .unwrap();
    assert_eq!(
        fixture.claim().await.unwrap_err().to_string(),
        "call approval source authentication failed"
    );
}

#[tokio::test]
async fn expired_run_keyed_pending_cannot_be_reviewed_or_claimed() {
    let path = tempfile::tempdir().unwrap();
    let fixture = CallFixture::new(path.path()).await;
    let event = fixture
        .memory
        .find_message_event_by_idempotency_key("prx_workspace", &fixture.pending.authority().pending_key())
        .await
        .unwrap()
        .unwrap();
    let mut payload: serde_json::Value = serde_json::from_str(event.raw_payload_json.as_deref().unwrap()).unwrap();
    let record = payload.get_mut("record").unwrap();
    *record.get_mut("expires_at").unwrap() =
        serde_json::to_value(chrono::Utc::now() - chrono::Duration::seconds(1)).unwrap();
    let typed: PendingCall = serde_json::from_value(record.clone()).unwrap();
    let mac = fixture.key.hmac(
        KeyDomain::ApprovalState,
        &[b"call.pending", &serde_json::to_vec(&typed).unwrap()],
    );
    *payload.get_mut("mac").unwrap() = serde_json::to_value(mac).unwrap();
    SqliteMemory::new(path.path())
        .unwrap()
        .replace_event_payload_for_test(&event.event_id, &payload.to_string())
        .unwrap();
    assert_eq!(
        fixture
            .port()
            .inspect("prx_owner", "prx_session", fixture.pending.authority())
            .await
            .err()
            .unwrap()
            .to_string(),
        "pending call approval expired"
    );
    assert_eq!(
        fixture
            .port()
            .review("prx_owner", "prx_session", fixture.pending.authority(), true)
            .await
            .unwrap_err()
            .to_string(),
        "pending call approval expired"
    );
    assert_eq!(
        fixture.claim().await.unwrap_err().to_string(),
        "pending call approval expired"
    );
}

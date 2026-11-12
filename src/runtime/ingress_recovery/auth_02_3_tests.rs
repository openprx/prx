#![cfg(test)]

use super::recovery_config;
use crate::agent::terminal::{
    TurnTerminalStatus, bind_terminal_fixture, bind_terminal_fixture_mode, completed_commit, finalize_turn,
    priced_cost_config,
};
use crate::config::schema::CostConfig;
use crate::cost::types::CostSettlement;
use crate::llm::route_decision::RouteDecision;
use crate::memory::fabric::MemoryFabric;
use crate::memory::recovery_authority::RecoveryMacDomain;
use crate::memory::sqlite::fixture_readback;
use crate::memory::{Memory, SqliteMemory};
use crate::runtime::ingress_recovery::tombstone_fixture;
use crate::runtime::ingress_recovery::{RecoveryScope, backend_memory};
use crate::runtime::turn_cleanup::CleanupTracker;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const OUTBOX_FACT_KEYS: [&str; 7] = [
    "v",
    "terminal_id",
    "status",
    "pricing",
    "usage",
    "usage_event_id",
    "final_event_id",
];

fn home_workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tombstone_fixture::home_dir();
    let workspace = dir.path().canonicalize().expect("test: workspace");
    (dir, workspace)
}

fn ledger_lines(workspace: &Path, terminal_id: &str) -> usize {
    match std::fs::read_to_string(workspace.join("state").join("costs.jsonl")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => panic!("test: read cost ledger: {error}"),
    }
    .lines()
    .filter(|line| line.contains(terminal_id))
    .count()
}

fn assert_closed_outbox_facts(facts: &[u8]) {
    let value: serde_json::Value = serde_json::from_slice(facts).expect("test: outbox facts");
    let object = value.as_object().expect("test: outbox object");
    assert_eq!(
        object.len(),
        OUTBOX_FACT_KEYS.len(),
        "test: outbox facts schema changed; every authenticated field must be accounted for"
    );
    assert!(
        OUTBOX_FACT_KEYS.iter().all(|key| object.contains_key(*key)),
        "test: outbox facts lost a required field and can no longer bind the complete usage"
    );
}

fn first_outbox(workspace: &Path) -> fixture_readback::OutboxRow {
    fixture_readback::outbox_rows(workspace)
        .into_iter()
        .next()
        .expect("test: outbox row")
}

fn first_claim_payload(workspace: &Path) -> String {
    fixture_readback::claim_payloads(workspace)
        .into_iter()
        .next()
        .expect("test: claim payload")
}

struct SkipInline;
impl Drop for SkipInline {
    fn drop(&mut self) {
        crate::agent::terminal::recovery_commit::skip_inline_projection(false);
    }
}

async fn prepare_scope(
    workspace: &Path,
    memory: Arc<dyn Memory>,
    plane: bool,
    recording: bool,
    cost: CostConfig,
) -> (Arc<RecoveryScope>, Arc<CleanupTracker>) {
    let mut config = recovery_config(workspace);
    config.workspace_dir = workspace.to_path_buf();
    config.causal_tree.enabled = plane;
    config.memory.events.record_user_messages = recording;
    config.cost = cost;
    let generation = crate::config::ConfigGenerationManager::new(config).pin();
    let tracker = Arc::new(CleanupTracker::default());
    let recovery = RecoveryScope::prepare(
        &generation,
        memory,
        crate::causal_tree::state::TurnSurfaceKind::AgentLoop,
        None,
        &tracker,
        None,
        crate::runtime::ingress_recovery::PreparedAdvisory::default(),
    )
    .await
    .expect("test: recovery prepare");
    (recovery, tracker)
}

pub(super) async fn recovery_terminal_loser_touches_no_outbox_or_ledger() {
    let (dir, workspace) = home_workspace();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace).expect("test: open recovery database"));
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());
    let cost = priced_cost_config();
    let terminal_id = "prx-loser";
    let mut commit = completed_commit(terminal_id);
    let service = bind_terminal_fixture(&workspace, Arc::clone(&memory), &mut commit, &cost).await;
    let winner = finalize_turn(&fabric, commit.clone(), &cost, &workspace)
        .await
        .expect("test: winner");
    assert!(
        matches!(winner.cost_settlement, Some(CostSettlement::ProjectionPending)),
        "test: winning terminal must retain its durable cost intent until projection"
    );
    let winner_row = first_outbox(&workspace);
    assert_eq!(
        winner_row.settlement_id, terminal_id,
        "test: winning outbox must belong to the committed terminal"
    );
    let winner_owner = winner_row.owner_id.clone();
    assert_eq!(
        ledger_lines(&workspace, terminal_id),
        1,
        "test: winning terminal must be charged exactly once"
    );

    let mut loser = commit;
    loser.scope.owner_id = Some("prx_other_owner".into());
    let error = finalize_turn(&fabric, loser, &cost, &workspace)
        .await
        .expect_err("test: loser");
    assert!(
        error.to_string().contains("scope does not match the pinned turn"),
        "loser: {error:#}"
    );
    // The runtime rejects a mismatched owner before storage. Exercise the
    // atomic store's independent owner collision using the actual final bytes.
    let mut event: crate::memory::MessageEventInput =
        serde_json::from_value(serde_json::to_value(&winner.terminal_event).expect("test: final event JSON"))
            .expect("test: final event input");
    event.event_id = Some(uuid::Uuid::new_v4().to_string());
    event.owner_id = Some("prx_other_owner".into());
    event.session_key = Some("prx_other_session".into());
    event.run_id = Some("prx_other_run".into());
    let outcome = memory
        .commit_recovery_terminal(
            crate::memory::recovery_authority::RecoveryTerminalCommit {
                events: vec![event],
                ingress: None,
                publication: crate::memory::recovery_authority::RecoveryPublication {
                    chain: crate::memory::recovery_authority::RecoveryChain {
                        workspace_id: fabric.workspace_id().into(),
                        owner_id: "prx_other_owner".into(),
                        source: winner.terminal_event.source.as_str().into(),
                        run_id: "prx_other_run".into(),
                    },
                    input: None,
                    terminal: None,
                    facts: winner_row.facts.clone(),
                },
                outbox: crate::memory::recovery_authority::RecoveryCostIntent {
                    pricing: crate::memory::recovery_authority::RecoveryPricing::Metered,
                    facts: winner_row.facts.clone(),
                },
            },
            Instant::now() + Duration::from_secs(15),
        )
        .await
        .expect("test: owner collision transaction");
    assert!(
        matches!(
            outcome,
            crate::memory::recovery_authority::RecoveryTerminalOutcome::Conflict
        ),
        "test: competing terminal must be refused to preserve the original owner"
    );
    let loser_row = first_outbox(&workspace);
    assert_eq!(
        loser_row.owner_id, winner_owner,
        "test: losing commit must not replace the winning outbox owner"
    );
    assert_eq!(
        loser_row.settlement_id, terminal_id,
        "test: losing commit must not replace the winning settlement identity"
    );
    assert_eq!(
        ledger_lines(&workspace, terminal_id),
        1,
        "test: losing commit must not add a second charge"
    );
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    drop(dir);
}

pub(super) async fn recovery_nonmonetary_pricing_commits_disposition() {
    let (dir, workspace) = home_workspace();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace).expect("test: open recovery database"));
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());

    let unknown_id = "prx-unknown";
    let mut unknown = completed_commit(unknown_id);
    let unknown_service =
        bind_terminal_fixture(&workspace, Arc::clone(&memory), &mut unknown, &CostConfig::default()).await;
    let unknown_receipt = finalize_turn(&fabric, unknown, &CostConfig::default(), &workspace)
        .await
        .expect("test: unknown");
    assert!(
        matches!(unknown_receipt.cost_settlement, Some(CostSettlement::UnknownPricing)),
        "test: unpriced usage must remain explicitly classified so it cannot be billed as metered"
    );
    assert_eq!(
        ledger_lines(&workspace, unknown_id),
        0,
        "test: unknown pricing must not invent a monetary charge"
    );
    unknown_service.shutdown(Instant::now() + Duration::from_secs(5)).await;

    let sub_id = "prx-subscription";
    let mut subscription = completed_commit(sub_id);
    subscription.provider_outcome = Some(
        crate::llm::route_decision::ProviderExecutionOutcome::success_for_decision_with_usage(
            &RouteDecision::single_candidate("kimi-code", "kimi-for-coding"),
            subscription.telemetry.started_at,
            crate::llm::route_decision::TokenUsage::reported(Some(10), Some(5), Some(15)),
        ),
    );
    let sub_service = bind_terminal_fixture(
        &workspace,
        Arc::clone(&memory),
        &mut subscription,
        &CostConfig::default(),
    )
    .await;
    let sub_receipt = finalize_turn(&fabric, subscription, &CostConfig::default(), &workspace)
        .await
        .expect("test: subscription");
    assert!(
        matches!(sub_receipt.cost_settlement, Some(CostSettlement::Subscription { .. })),
        "test: subscription usage must retain its nonmonetary disposition"
    );
    assert_eq!(
        ledger_lines(&workspace, sub_id),
        0,
        "test: subscription usage must not create a monetary ledger entry"
    );

    let rows = fixture_readback::outbox_rows(&workspace);
    assert!(
        rows.iter().all(|row| row.state == "acknowledged"),
        "test: nonmonetary dispositions must finish without leaving retryable work"
    );
    assert!(
        rows.iter().any(|row| row.pricing == "unknown"),
        "test: unknown pricing disposition must survive outbox readback"
    );
    assert!(
        rows.iter().any(|row| row.pricing == "subscription"),
        "test: subscription disposition must survive outbox readback"
    );
    for row in &rows {
        assert_closed_outbox_facts(&row.facts);
    }
    sub_service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    drop(dir);
}

pub(super) async fn recovery_terminal_claim_each_bound_field_is_refused() {
    let (dir, workspace) = home_workspace();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace).expect("test: open recovery database"));
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());
    let cost = priced_cost_config();
    let mut draft = completed_commit("prx-claim-bound");
    let service =
        crate::agent::terminal::bind_terminal_parameter_fixture(&workspace, Arc::clone(&memory), &mut draft, &cost)
            .await;
    crate::agent::terminal::recovery_commit::capture_parameters_for_test(&mut draft).await;
    finalize_turn(&fabric, draft.clone(), &cost, &workspace)
        .await
        .expect("test: claim winner");
    let commit: crate::agent::terminal::TurnTerminalCommit = draft.clone().try_into().expect("test: commit");
    let recorded = commit.decision.is_recorded();
    let payload: serde_json::Value = serde_json::from_str(&first_claim_payload(&workspace)).expect("test: claim json");
    let key_id = payload
        .get("recovery_key_id")
        .and_then(serde_json::Value::as_str)
        .expect("test: recovery_key_id")
        .to_owned();
    let semantic = crate::agent::terminal::recovery_commit::claim_semantic_for_test(
        &commit,
        fabric.workspace_id(),
        recorded,
        &key_id,
    )
    .expect("test: semantic");
    let mac = hex::decode(
        payload
            .get("semantic_mac_v3")
            .and_then(serde_json::Value::as_str)
            .expect("test: semantic_mac_v3"),
    )
    .expect("test: mac hex");
    let tag = crate::memory::recovery_authority::RecoveryTag {
        key_id: key_id.clone(),
        tag: mac.as_slice().try_into().expect("test: mac length"),
    };
    let repository = memory.recovery_repository().expect("test: repository");
    let mut preimage = serde_json::to_vec(&semantic).expect("test: semantic bytes");
    preimage.extend_from_slice(key_id.as_bytes());
    repository
        .verify_recovery_mac(&tag, RecoveryMacDomain::ClaimV3, &preimage)
        .expect("test: original claim mac");

    let refuse_field = |cell: &str, pointer: &str, replacement: serde_json::Value| {
        let mut mutated = semantic.clone();
        let slot = mutated
            .pointer_mut(pointer)
            .unwrap_or_else(|| panic!("test: {cell}: production claim does not bind {pointer}"));
        assert_ne!(*slot, replacement, "test: {cell}: replacement equals the bound value");
        *slot = replacement;
        let mut bytes = serde_json::to_vec(&mutated).expect("test: mutated semantic");
        bytes.extend_from_slice(key_id.as_bytes());
        let error = repository
            .verify_recovery_mac(&tag, RecoveryMacDomain::ClaimV3, &bytes)
            .expect_err(cell);
        assert!(
            error.to_string().contains("invalid recovery authentication"),
            "{cell}: {error:#}"
        );
    };
    refuse_field("terminal_id", "/terminal_id", serde_json::json!("other"));
    refuse_field(
        "decision_id",
        "/decision_id",
        serde_json::json!(uuid::Uuid::nil().to_string()),
    );
    refuse_field("workspace", "/workspace_id", serde_json::json!("other-workspace"));
    refuse_field(
        "source_event",
        "/source_event_id",
        serde_json::json!(uuid::Uuid::nil().to_string()),
    );
    refuse_field("status", "/status", serde_json::json!("failed"));
    refuse_field("scope", "/scope/session_key", serde_json::json!("other"));
    refuse_field("history", "/history/assistant_content", serde_json::json!("mutated"));
    refuse_field(
        "provider_outcome",
        "/provider_outcome/final_model",
        serde_json::json!("other-model"),
    );
    refuse_field("delivery_intent", "/delivery_intent", serde_json::json!("none"));
    refuse_field(
        "execution_trace",
        "/execution_trace",
        serde_json::json!({"mutated": true}),
    );
    refuse_field("decision_chain", "/decision_chain", serde_json::json!(["other"]));
    refuse_field("catalog_generation", "/catalog_generation", serde_json::json!("00"));
    refuse_field(
        "model_catalog_revision",
        "/model_catalog_revision",
        serde_json::json!("other"),
    );
    refuse_field(
        "model_catalog_content_sha256",
        "/model_catalog_content_sha256",
        serde_json::json!("00"),
    );
    refuse_field("policy_version", "/policy_version", serde_json::json!(0));
    refuse_field(
        "history_scope",
        "/history_scope",
        serde_json::json!({"session_key": "other"}),
    );
    refuse_field("telemetry", "/telemetry/summary", serde_json::json!("mutated"));
    refuse_field(
        "prepared_parameters",
        "/prepared_parameters",
        serde_json::json!([{"mutated": true}]),
    );
    let mut key_preimage = serde_json::to_vec(&semantic).expect("test: key preimage");
    key_preimage.extend_from_slice(b"other-key");
    let key_err = repository
        .verify_recovery_mac(&tag, RecoveryMacDomain::ClaimV3, &key_preimage)
        .expect_err("test: key id");
    assert!(
        key_err.to_string().contains("invalid recovery authentication"),
        "key id: {key_err:#}"
    );

    // Digest-only claims retain byte-exact binding without expanding the MAC budget.
    let mut oversized = commit.clone();
    let reply = "π".repeat(crate::memory::recovery_authority::MAX_SERIALIZED_RECOVERY_FACTS);
    oversized.history.as_mut().expect("test: history").assistant_content = reply.clone();
    let bounded = crate::agent::terminal::recovery_commit::claim_semantic_for_test(
        &oversized,
        fabric.workspace_id(),
        recorded,
        &key_id,
    )
    .expect("test: bounded reply semantics");
    assert!(!bounded.to_string().contains(&reply), "claim retained oversized text");
    assert_eq!(
        bounded
            .get("replay_unavailable")
            .and_then(|value| value.get("reply_bytes"))
            .and_then(serde_json::Value::as_u64),
        Some(reply.len() as u64),
        "test: claim binds UTF-8 bytes rather than characters"
    );
    assert_eq!(
        bounded
            .get("replay_unavailable")
            .and_then(|value| value.get("reply_sha256"))
            .and_then(serde_json::Value::as_str),
        Some("35bf14943aaacbb1c3adadceb560e297980c563951479567bac94f4807b23ed4"),
        "test: digest differs from the UTF-8 reply vector"
    );
    let mut bytes = serde_json::to_vec(&bounded).expect("test: bounded bytes");
    bytes.extend_from_slice(key_id.as_bytes());
    assert!(
        bytes.len() <= crate::memory::recovery_authority::MAX_SERIALIZED_RECOVERY_FACTS,
        "test: compact claim exceeded unchanged MAC budget"
    );
    let bounded_tag = repository
        .recovery_mac(Some(&key_id), RecoveryMacDomain::ClaimV3, &bytes)
        .expect("test: bounded claim MAC");
    for field in ["reply_sha256", "reply_bytes"] {
        let mut changed = bounded.clone();
        let changed_reference = changed
            .get_mut("replay_unavailable")
            .and_then(serde_json::Value::as_object_mut)
            .expect("test: reference object");
        changed_reference.insert(
            field.to_owned(),
            if field == "reply_bytes" {
                serde_json::json!(reply.len() + 1)
            } else {
                serde_json::json!("00".repeat(32))
            },
        );
        let mut changed_bytes = serde_json::to_vec(&changed).expect("test: changed reference");
        changed_bytes.extend_from_slice(key_id.as_bytes());
        let error = repository
            .verify_recovery_mac(&bounded_tag, RecoveryMacDomain::ClaimV3, &changed_bytes)
            .expect_err(field);
        assert!(
            error.to_string().contains("invalid recovery authentication"),
            "{field}: {error:#}"
        );
    }

    let mut replay = draft.clone();
    replay.status = TurnTerminalStatus::Failed;
    let error = finalize_turn(&fabric, replay, &cost, &workspace)
        .await
        .expect_err("test: replay mutated status");
    assert!(
        error.to_string().contains("recovery terminal commit conflict") || error.to_string().contains("conflict"),
        "replay mutated status: {error:#}"
    );
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;

    let mut unrecorded = completed_commit("prx-claim-unrecorded");
    let unrecorded_service =
        bind_terminal_fixture_mode(&workspace, Arc::clone(&memory), &mut unrecorded, &cost, false, None).await;
    finalize_turn(&fabric, unrecorded, &cost, &workspace)
        .await
        .expect("test: unrecorded");
    let last = fixture_readback::claim_payloads(&workspace)
        .into_iter()
        .last()
        .expect("test: unrecorded claim");
    let value: serde_json::Value = serde_json::from_str(&last).expect("test: unrecorded json");
    assert!(
        value.get("decision_id").is_none(),
        "unrecorded claim has no decision_id"
    );
    assert!(value.get("semantic_mac_v2").is_none(), "unrecorded claim has no v2 MAC");
    assert!(
        value
            .get("semantic_mac_v3")
            .and_then(serde_json::Value::as_str)
            .is_some(),
        "test: terminal claim must carry a MAC so replay can authenticate its semantics"
    );
    assert!(
        value
            .get("recovery_key_id")
            .and_then(serde_json::Value::as_str)
            .is_some(),
        "test: terminal claim must identify the key needed for replay verification"
    );
    unrecorded_service
        .shutdown(Instant::now() + Duration::from_secs(5))
        .await;
    drop(dir);
}

pub(super) async fn recovery_terminal_commit_wrapped_backends_contract() {
    for backend in ["sqlite", "lucid", "encrypted"] {
        let (dir, workspace) = home_workspace();
        let memory = backend_memory(backend, &workspace).expect("test: backend");
        let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());
        let cost = priced_cost_config();
        let terminal_id = format!("prx-wrap-{backend}");
        let mut commit = completed_commit(&terminal_id);
        let service = bind_terminal_fixture(&workspace, Arc::clone(&memory), &mut commit, &cost).await;
        let first = finalize_turn(&fabric, commit.clone(), &cost, &workspace)
            .await
            .unwrap_or_else(|error| panic!("test: {backend} commit: {error:#}"));
        assert!(
            matches!(first.cost_settlement, Some(CostSettlement::ProjectionPending)),
            "test: terminal commit must retain a pending cost intent before projection"
        );
        assert_eq!(
            crate::cost::tracker::settle_metered_call_count(&workspace),
            0,
            "{backend} pre-commit cost spy"
        );
        if backend == "encrypted" {
            let stored = fixture_readback::stored_contents(&workspace, "assistant");
            assert!(
                stored.iter().all(|content| content != "done"),
                "encrypted backend stored plaintext assistant content"
            );
            let terminals = fixture_readback::terminal_rows(&workspace).expect("test: encrypted terminal rows");
            assert!(
                terminals
                    .iter()
                    .all(|row| serde_json::from_str::<serde_json::Value>(&row.raw_payload).is_err()),
                "encrypted backend stored plaintext terminal payload"
            );
            assert_eq!(
                first.assistant_event.as_ref().map(|event| event.content.as_str()),
                Some("done"),
                "test: replay must preserve the assistant reply committed by the winner"
            );
        }
        let second = finalize_turn(&fabric, commit.clone(), &cost, &workspace)
            .await
            .unwrap_or_else(|error| panic!("test: {backend} replay: {error:#}"));
        assert_eq!(
            first.terminal_event.event_id, second.terminal_event.event_id,
            "test: replay must reuse the original terminal event instead of creating a duplicate"
        );
        assert_eq!(
            ledger_lines(&workspace, &terminal_id),
            1,
            "test: replaying a committed terminal must not duplicate its charge"
        );
        let owner = commit.scope.owner_id.clone().expect("test: owner");
        let (recovery, tracker) = prepare_scope(&workspace, Arc::clone(&memory), false, true, cost.clone()).await;
        recovery
            .rejoin_terminal(&terminal_id, &owner)
            .await
            .unwrap_or_else(|error| panic!("test: {backend} plane-off rejoin: {error:#}"));
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        tracker
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .expect("test: drain cleanup");
        drop(dir);
    }

    let (dir, workspace) = home_workspace();
    let memory = backend_memory("postgres_class", &workspace).expect("test: postgres_class");
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());
    let cost = priced_cost_config();
    let mut commit = completed_commit("prx-wrap-postgres");
    let service = bind_terminal_fixture(&workspace, Arc::clone(&memory), &mut commit, &cost).await;
    let receipt = finalize_turn(&fabric, commit, &cost, &workspace)
        .await
        .expect("test: postgres_class");
    assert!(
        matches!(receipt.cost_settlement, Some(CostSettlement::Recorded { .. })),
        "test: priced usage must reach the ledger before reporting recorded settlement"
    );
    assert_eq!(
        crate::cost::tracker::settle_metered_call_count(&workspace),
        1,
        "test: single terminal projection must invoke metered settlement exactly once"
    );
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    drop(dir);
}

#[tokio::test]
async fn oversized_reply_remains_in_next_turn_history_and_replay_verifies() {
    let (_dir, workspace) = home_workspace();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace).expect("test: database"));
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());
    let cost = priced_cost_config();
    let reply = "x".repeat(
        crate::memory::recovery_authority::MAX_SERIALIZED_RECOVERY_FACTS
            .max(crate::memory::ingress_recovery::MAX_RECOVERY_EVENT_BYTES)
            + 1,
    );
    let mut draft = completed_commit("prx-large-history");
    draft.history.as_mut().expect("test: history").assistant_content = reply.clone();
    let service = bind_terminal_fixture_mode(
        &workspace,
        Arc::clone(&memory),
        &mut draft,
        &cost,
        true,
        Some(Arc::clone(&memory)),
    )
    .await;
    let receipt = finalize_turn(&fabric, draft.clone(), &cost, &workspace)
        .await
        .expect("test: oversized commit");
    let assistant = receipt.assistant_event.expect("test: assistant event");
    let history = memory
        .load_recent_session_context(crate::memory::SessionContextQuery {
            principal: crate::memory::MemoryPrincipal {
                workspace_id: fabric.workspace_id().to_owned(),
                session_key: assistant.session_key.clone(),
                owner_id: assistant.owner_id.clone(),
                require_owner_match: true,
                ..Default::default()
            },
            since_event_id: None,
            limit: 24,
            include_roles: vec!["assistant".into()],
        })
        .await
        .expect("test: next turn history read");
    assert!(
        history.iter().any(|event| event.content == reply),
        "test: next turn must retain the real oversized reply"
    );
    let unavailable = crate::agent::terminal::oversized_reply::verify_terminal_for_test(
        Arc::clone(&memory),
        receipt.terminal_event,
        draft.scope.causation_event_id.clone().expect("test: input event"),
    )
    .await
    .expect("test: oversized replay authenticates");
    assert!(
        unavailable.is_some(),
        "test: oversized replay remains unavailable after authenticating"
    );
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
}

#[tokio::test]
async fn committed_turn_survives_concurrent_scan_and_tampered_outbox() {
    let (_dir, workspace) = home_workspace();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace).expect("test: database"));
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());
    let cost = priced_cost_config();
    let mut commit = completed_commit("prx-scan-first");
    let service = bind_terminal_fixture(&workspace, Arc::clone(&memory), &mut commit, &cost).await;
    let scan_memory = Arc::clone(&memory);
    let scan_workspace = workspace.clone();
    let scan_cost = cost.clone();
    crate::agent::terminal::recovery_commit::before_inline_projection(Box::new(move || {
        let scan = std::thread::spawn(move || {
            crate::agent::terminal::recovery_commit::scan_pending_recovery_costs(
                scan_memory.as_ref(),
                &scan_workspace.to_string_lossy(),
                &scan_workspace,
                &scan_cost,
            )
            .expect("test: scan wins before inline lookup");
        });
        scan.join().expect("test: concurrent restart scan");
    }));
    finalize_turn(&fabric, commit, &cost, &workspace)
        .await
        .expect("test: committed turn survives scan-first race");
    assert_eq!(
        ledger_lines(&workspace, "prx-scan-first"),
        1,
        "test: exactly one scan-first ledger entry"
    );
    let repository = memory.recovery_repository().expect("test: repository");
    let usage_digest: [u8; 32] = first_outbox(&workspace)
        .usage_digest
        .try_into()
        .expect("test: digest length");
    repository
        .finish_recovery_cost(
            &workspace.to_string_lossy(),
            "prx-scan-first",
            &usage_digest,
            crate::memory::recovery_authority::RecoveryCostState::Acknowledged,
        )
        .expect("test: concurrent ack is idempotent");
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;

    let mut corrupt = completed_commit("prx-corrupt");
    let service = bind_terminal_fixture(&workspace, Arc::clone(&memory), &mut corrupt, &cost).await;
    crate::agent::terminal::recovery_commit::skip_inline_projection(true);
    let _skip = SkipInline;
    finalize_turn(&fabric, corrupt, &cost, &workspace)
        .await
        .expect("test: seed pending row");
    crate::agent::terminal::recovery_commit::skip_inline_projection(false);
    // Keep the row retryable while invalidating its authenticated state.
    crate::memory::sqlite::forge_error_cost_for_test(&workspace, "prx-corrupt")
        .expect("test: invalidate outbox state MAC");
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    for id in ["prx-later-one", "prx-later-two"] {
        let mut later = completed_commit(id);
        let service = bind_terminal_fixture(&workspace, Arc::clone(&memory), &mut later, &cost).await;
        finalize_turn(&fabric, later, &cost, &workspace)
            .await
            .expect("test: tampered outbox must not fail later committed turns");
        assert_eq!(
            ledger_lines(&workspace, id),
            1,
            "test: a tampered sibling row must not stop this turn's cost projection"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }
    assert_eq!(
        ledger_lines(&workspace, "prx-corrupt"),
        0,
        "test: tampered row must never project"
    );
    let listed = repository
        .pending_recovery_costs(&workspace.to_string_lossy(), None, 64)
        .expect("test: listing skips the tampered row");
    assert!(
        listed.iter().all(|row| row.settlement_id != "prx-corrupt"),
        "test: a tampered row is never offered for projection"
    );
}

pub(super) async fn recovery_terminal_commit_survives_restart() {
    async fn priced_setup(
        label: &str,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        Arc<dyn Memory>,
        MemoryFabric,
        crate::agent::terminal::TurnTerminalDraft,
        CostConfig,
        Arc<crate::runtime::turn_decision::TurnDecisionService>,
    ) {
        let (dir, workspace) = home_workspace();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace).expect("test: open recovery database"));
        let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());
        let cost = priced_cost_config();
        let mut commit = completed_commit(label);
        let service = bind_terminal_fixture(&workspace, Arc::clone(&memory), &mut commit, &cost).await;
        (dir, workspace, memory, fabric, commit, cost, service)
    }

    {
        let (dir, workspace, memory, fabric, commit, cost, service) = priced_setup("prx-crash-before-commit").await;
        crate::memory::sqlite::crash_before_commit(true);
        let error = finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect_err("test: crash before commit");
        assert!(error.to_string().contains("test: crash"), "{error:#}");
        assert!(
            fixture_readback::outbox_rows(&workspace).is_empty(),
            "test: a crash before commit must leave no outbox intent to project"
        );
        assert_eq!(
            ledger_lines(&workspace, "prx-crash-before-commit"),
            0,
            "test: a crash before commit must not charge uncommitted usage"
        );
        crate::agent::terminal::recovery_commit::scan_pending_recovery_costs(
            memory.as_ref(),
            &workspace.to_string_lossy(),
            &workspace,
            &cost,
        )
        .expect("test: scan before commit");
        assert!(
            fixture_readback::outbox_rows(&workspace).is_empty(),
            "test: restart scan must not manufacture an outbox for an uncommitted terminal"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(dir);
    }

    {
        let (dir, workspace, memory, fabric, commit, cost, service) = priced_setup("prx-crash-before-append").await;
        crate::cost::tracker::crash_before_append(true);
        finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect("test: crash before append still commits");
        assert_eq!(
            fixture_readback::outbox_rows(&workspace).len(),
            1,
            "test: crash before append must preserve exactly one durable cost intent"
        );
        assert_eq!(
            first_outbox(&workspace).state,
            "pending",
            "test: crash before append must leave the intent retryable for restart"
        );
        assert_eq!(
            ledger_lines(&workspace, "prx-crash-before-append"),
            0,
            "test: crash before append must not report usage as charged"
        );
        crate::agent::terminal::recovery_commit::scan_pending_recovery_costs(
            memory.as_ref(),
            &workspace.to_string_lossy(),
            &workspace,
            &cost,
        )
        .expect("test: scan after crash before append");
        assert_eq!(
            first_outbox(&workspace).state,
            "acknowledged",
            "test: restart must acknowledge the recovered append so it stops retrying"
        );
        assert_eq!(
            ledger_lines(&workspace, "prx-crash-before-append"),
            1,
            "test: restart must recover the missing charge exactly once"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(dir);
    }

    {
        let (dir, workspace, memory, fabric, commit, cost, service) = priced_setup("prx-crash-after-append").await;
        crate::cost::tracker::crash_after_append(true);
        finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect("test: crash after append still commits");
        assert_eq!(
            first_outbox(&workspace).state,
            "pending",
            "test: crash after append must leave the intent retryable until acknowledgement"
        );
        assert_eq!(
            ledger_lines(&workspace, "prx-crash-after-append"),
            1,
            "test: crash after append must preserve the already durable charge"
        );
        crate::agent::terminal::recovery_commit::scan_pending_recovery_costs(
            memory.as_ref(),
            &workspace.to_string_lossy(),
            &workspace,
            &cost,
        )
        .expect("test: scan after crash after append");
        assert_eq!(
            first_outbox(&workspace).state,
            "acknowledged",
            "test: restart must acknowledge the previously appended charge"
        );
        assert_eq!(
            ledger_lines(&workspace, "prx-crash-after-append"),
            1,
            "test: restart after append must not duplicate the durable charge"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(dir);
    }

    {
        let (dir, workspace, memory, fabric, commit, cost, service) = priced_setup("prx-crash-before-ack").await;
        crate::agent::terminal::recovery_commit::crash_before_ack(true);
        finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect("test: crash before ack still commits");
        crate::agent::terminal::recovery_commit::crash_before_ack(false);
        assert_eq!(
            first_outbox(&workspace).state,
            "pending",
            "test: crash before acknowledgement must leave recovery work pending"
        );
        assert_eq!(
            ledger_lines(&workspace, "prx-crash-before-ack"),
            1,
            "test: crash before acknowledgement must preserve the durable ledger entry"
        );
        crate::agent::terminal::recovery_commit::scan_pending_recovery_costs(
            memory.as_ref(),
            &workspace.to_string_lossy(),
            &workspace,
            &cost,
        )
        .expect("test: scan after crash before ack");
        assert_eq!(
            first_outbox(&workspace).state,
            "acknowledged",
            "test: restart must finish the interrupted acknowledgement"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(dir);
    }

    {
        let (dir, workspace, memory, fabric, commit, cost, service) = priced_setup("prx-skip-inline").await;
        let _skip = SkipInline;
        crate::agent::terminal::recovery_commit::skip_inline_projection(true);
        let receipt = finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect("test: skip inline");
        assert_eq!(
            first_outbox(&workspace).state,
            "pending",
            "test: skipping inline projection must retain durable work for the scanner"
        );
        let repository = memory.recovery_repository().expect("test: repository");
        let rows = repository
            .pending_recovery_costs(&workspace.to_string_lossy(), None, 1)
            .expect("test: pending usage authentication");
        let row = rows.first().expect("test: pending usage");
        repository
            .verify_recovery_mac(
                &crate::memory::recovery_authority::RecoveryTag {
                    key_id: row.key_id.clone(),
                    tag: row.usage_digest,
                },
                RecoveryMacDomain::UsageDigestV1,
                &serde_json::to_vec(&receipt.usage_settlement).expect("test: typed usage serialization"),
            )
            .expect("test: outbox binds the typed usage bytes");
        let (disabled, disabled_tracker) =
            prepare_scope(&workspace, Arc::clone(&memory), false, false, cost.clone()).await;
        drop(disabled);
        disabled_tracker
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .expect("test: recording-off drain");
        assert_eq!(
            first_outbox(&workspace).state,
            "pending",
            "recording-off scan acknowledged usage"
        );
        assert_eq!(
            ledger_lines(&workspace, "prx-skip-inline"),
            0,
            "test: recording disabled must not append pending usage to the ledger"
        );
        let (recovery, tracker) = prepare_scope(&workspace, Arc::clone(&memory), false, true, cost.clone()).await;
        drop(recovery);
        tracker
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .expect("test: drain cleanup");
        assert_eq!(
            first_outbox(&workspace).state,
            "acknowledged",
            "test: enabled recovery must acknowledge the usage deferred while recording was disabled"
        );
        assert_eq!(
            ledger_lines(&workspace, "prx-skip-inline"),
            1,
            "test: enabled recovery must charge the deferred usage exactly once"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(dir);
    }

    {
        let (dir, workspace, memory, fabric, commit, cost, service) = priced_setup("prx-io-error").await;
        crate::cost::tracker::force_io_error(true);
        finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect("test: I/O still commits");
        assert_eq!(
            first_outbox(&workspace).state,
            "error",
            "test: ledger I/O failure must retain a retryable error disposition"
        );
        crate::agent::terminal::recovery_commit::scan_pending_recovery_costs(
            memory.as_ref(),
            &workspace.to_string_lossy(),
            &workspace,
            &cost,
        )
        .expect("test: scan error row");
        assert_eq!(
            first_outbox(&workspace).state,
            "acknowledged",
            "test: retry after I/O failure must acknowledge the repaired projection"
        );
        assert_eq!(
            ledger_lines(&workspace, "prx-io-error"),
            1,
            "test: retry after I/O failure must charge the terminal exactly once"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(dir);
    }

    {
        let (dir, workspace, _memory, fabric, commit, cost, service) = priced_setup("prx-forged-ack").await;
        let _skip = SkipInline;
        crate::agent::terminal::recovery_commit::skip_inline_projection(true);
        finalize_turn(&fabric, commit.clone(), &cost, &workspace)
            .await
            .expect("test: pending before forged ack");
        crate::memory::sqlite::forge_acknowledged_cost_for_test(&workspace, "prx-forged-ack")
            .expect("test: relabel without the state MAC");
        let error = finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect_err("test: replay must authenticate an acknowledged state");
        assert!(
            error.to_string().contains("state_mac is invalid"),
            "forged ack: {error:#}"
        );
        assert_eq!(
            ledger_lines(&workspace, "prx-forged-ack"),
            0,
            "test: forged acknowledgement must not authorize a ledger append"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(dir);
    }

    {
        let (dir, workspace, _memory, fabric, commit, cost, service) = priced_setup("prx-missing-outbox").await;
        let _skip = SkipInline;
        crate::agent::terminal::recovery_commit::skip_inline_projection(true);
        finalize_turn(&fabric, commit.clone(), &cost, &workspace)
            .await
            .expect("test: terminal and pending outbox");
        crate::memory::sqlite::remove_cost_for_test(&workspace, "prx-missing-outbox")
            .expect("test: remove the paired outbox row");
        let error = finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect_err("test: a replay cannot regenerate a missing atomic outbox");
        assert!(
            error.to_string().contains("recovery terminal commit conflict"),
            "missing outbox: {error:#}"
        );
        assert!(
            fixture_readback::outbox_rows(&workspace).is_empty(),
            "refused replay recreated the outbox"
        );
        assert_eq!(
            ledger_lines(&workspace, "prx-missing-outbox"),
            0,
            "test: missing atomic outbox must prevent replay from charging usage"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(dir);
    }

    {
        let (dir, workspace, memory, fabric, commit, cost, service) = priced_setup("prx-torn-tail").await;
        let _skip = SkipInline;
        crate::agent::terminal::recovery_commit::skip_inline_projection(true);
        finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect("test: torn commit");
        std::fs::create_dir_all(workspace.join("state")).expect("test: create ledger directory");
        std::fs::write(workspace.join("state").join("costs.jsonl"), "{\"usage\":{}}")
            .expect("test: seed incomplete ledger tail");
        crate::agent::terminal::recovery_commit::scan_pending_recovery_costs(
            memory.as_ref(),
            &workspace.to_string_lossy(),
            &workspace,
            &cost,
        )
        .expect("test: torn scan");
        assert_eq!(
            first_outbox(&workspace).state,
            "blocked",
            "test: torn ledger tail must block projection to prevent further corruption"
        );
        let after = std::fs::read_to_string(workspace.join("state").join("costs.jsonl"))
            .expect("test: read unchanged ledger tail");
        assert_eq!(
            after, "{\"usage\":{}}",
            "test: blocked projection must preserve the torn ledger bytes for repair"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(dir);
    }

    {
        let (dir, workspace, memory, fabric, commit, cost, service) = priced_setup("prx-lock-timeout").await;
        let _skip = SkipInline;
        crate::agent::terminal::recovery_commit::skip_inline_projection(true);
        finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect("test: lock commit");
        std::fs::create_dir_all(workspace.join("state")).expect("test: create ledger directory");
        let lock_path = workspace.join("state").join("costs.jsonl.lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .expect("test: open ledger lock");
        lock_file.lock().expect("test: hold ledger lock");
        let (recovery, tracker) = prepare_scope(&workspace, Arc::clone(&memory), true, true, cost.clone()).await;
        drop(lock_file);
        drop(recovery);
        tracker
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .expect("test: drain cleanup");
        assert_eq!(
            first_outbox(&workspace).state,
            "error",
            "test: ledger lock timeout must retain an error disposition for a later retry"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(dir);
    }

    {
        let (dir, workspace, memory, fabric, commit, cost, service) = priced_setup("prx-concurrent").await;
        let _skip = SkipInline;
        crate::agent::terminal::recovery_commit::skip_inline_projection(true);
        let first = finalize_turn(&fabric, commit.clone(), &cost, &workspace)
            .await
            .expect("test: concurrent first");
        let workspace_id = workspace.to_string_lossy().into_owned();
        let left = Arc::clone(&memory);
        let right = Arc::clone(&memory);
        let left_dir = workspace.clone();
        let right_dir = workspace.clone();
        let left_cost = cost.clone();
        let right_cost = cost.clone();
        let left_id = workspace_id.clone();
        let handle = std::thread::spawn(move || {
            crate::agent::terminal::recovery_commit::scan_pending_recovery_costs(
                left.as_ref(),
                &left_id,
                &left_dir,
                &left_cost,
            )
        });
        crate::agent::terminal::recovery_commit::scan_pending_recovery_costs(
            right.as_ref(),
            &workspace_id,
            &right_dir,
            &right_cost,
        )
        .expect("test: concurrent right");
        handle
            .join()
            .expect("test: concurrent left")
            .expect("test: concurrent left scan");
        let second = finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect("test: concurrent replay");
        assert_eq!(
            first.terminal_event.event_id, second.terminal_event.event_id,
            "test: concurrent replays must converge on the original terminal event"
        );
        assert_eq!(
            ledger_lines(&workspace, "prx-concurrent"),
            1,
            "test: concurrent recovery must append only one charge"
        );
        assert_eq!(
            first_outbox(&workspace).state,
            "acknowledged",
            "test: concurrent recovery must complete acknowledgement after the single append"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(dir);
    }

    {
        let (dir, workspace, memory, fabric, commit, cost, service) = priced_setup("prx-conflicting-ledger").await;
        let _skip = SkipInline;
        crate::agent::terminal::recovery_commit::skip_inline_projection(true);
        let first = finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect("test: conflict commit");
        let mut conflicting = first.usage_settlement.expect("test: priced usage");
        conflicting.prompt_tokens += 1;
        conflicting.total_tokens += 1;
        let tracker = crate::cost::tracker::CostTracker::for_workspace(cost.clone(), &workspace)
            .expect("test: conflicting ledger tracker");
        tracker
            .settle_metered(&conflicting)
            .expect("test: append real conflicting settlement");
        let ledger = workspace.join("state/costs.jsonl");
        let before = std::fs::read(&ledger).expect("test: conflict bytes before");
        crate::agent::terminal::recovery_commit::scan_pending_recovery_costs(
            memory.as_ref(),
            &workspace.to_string_lossy(),
            &workspace,
            &cost,
        )
        .expect("test: conflict scan");
        assert_eq!(
            first_outbox(&workspace).state,
            "blocked",
            "test: conflicting ledger entry must block settlement instead of overwriting history"
        );
        assert_eq!(
            std::fs::read(&ledger).expect("test: conflict bytes after"),
            before,
            "test: blocked settlement must preserve the conflicting ledger bytes"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(dir);
    }

    for backend in ["lucid", "encrypted"] {
        let (dir, workspace) = home_workspace();
        let memory = backend_memory(backend, &workspace).expect("test: restart backend");
        let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());
        let cost = priced_cost_config();
        let terminal_id = format!("prx-restart-{backend}");
        let mut commit = completed_commit(&terminal_id);
        let service = bind_terminal_fixture(&workspace, Arc::clone(&memory), &mut commit, &cost).await;
        let owner = commit.scope.owner_id.clone().expect("test: restart owner");
        let _skip = SkipInline;
        crate::agent::terminal::recovery_commit::skip_inline_projection(true);
        let first = finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect("test: backend commit");
        assert_eq!(
            first_outbox(&workspace).state,
            "pending",
            "test: skipped inline projection must preserve the pending intent for replay"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(service);
        drop(fabric);
        drop(memory);
        let reopened = backend_memory(backend, &workspace).expect("test: reopen backend");
        let (recovery, tracker) = prepare_scope(&workspace, Arc::clone(&reopened), false, true, cost).await;
        recovery
            .rejoin_terminal(&terminal_id, &owner)
            .await
            .expect("test: reopened settled rejoin");
        let final_event = reopened
            .find_message_event_by_idempotency_key(&workspace.to_string_lossy(), &format!("turn:{terminal_id}:final"))
            .await
            .expect("test: reopened terminal read")
            .expect("test: reopened terminal");
        assert_eq!(
            final_event.event_id, first.terminal_event.event_id,
            "test: successful replay must retain the original terminal event identity"
        );
        assert_eq!(
            final_event.raw_payload_json, first.terminal_event.raw_payload_json,
            "test: successful replay must not rewrite the authenticated terminal payload"
        );
        assert_eq!(
            first_outbox(&workspace).state,
            "acknowledged",
            "test: successful replay must acknowledge the recovered cost intent"
        );
        assert_eq!(
            ledger_lines(&workspace, &terminal_id),
            1,
            "test: successful replay must settle the original terminal exactly once"
        );
        drop(recovery);
        tracker
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .expect("test: reopened drain");
        drop(dir);
    }

    let (dir, workspace) = home_workspace();
    let memory = backend_memory("postgres_class", &workspace).expect("test: postgres_class restart");
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());
    let cost = priced_cost_config();
    let mut commit = completed_commit("prx-pg-restart");
    let service = bind_terminal_fixture(&workspace, Arc::clone(&memory), &mut commit, &cost).await;
    let receipt = finalize_turn(&fabric, commit, &cost, &workspace)
        .await
        .expect("test: postgres_class restart");
    assert!(
        matches!(receipt.cost_settlement, Some(CostSettlement::Recorded { .. })),
        "test: recorded priced terminal must report the durable ledger settlement"
    );
    assert_eq!(
        crate::cost::tracker::settle_metered_call_count(&workspace),
        1,
        "test: recorded terminal must not repeat the metered settlement call"
    );
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    drop(dir);
}

pub(super) async fn recovery_erasure_indexes_settlement_terminal() {
    let (dir, workspace) = home_workspace();
    let sqlite = SqliteMemory::new(&workspace).expect("test: open recovery database");
    let origin = tombstone_fixture::erasure_origin();
    let memory = sqlite.with_erasure_origin(origin.clone()).expect("test: erasure view");
    let input_origin = crate::memory::erasure::ErasureOrigin::new("prx_account", "prx_conversation", "prx_input");
    let ingress_memory = sqlite
        .with_erasure_origin(input_origin)
        .expect("test: ingress erasure view");
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());
    let cost = priced_cost_config();
    let mut commit = completed_commit("prx-erase");
    let service = bind_terminal_fixture_mode(
        &workspace,
        Arc::clone(&memory),
        &mut commit,
        &cost,
        true,
        Some(ingress_memory),
    )
    .await;
    let pending_chain = crate::memory::recovery_authority::RecoveryChain {
        workspace_id: fabric.workspace_id().into(),
        owner_id: commit.scope.owner_id.clone().expect("test: pending owner"),
        source: commit.scope.source.as_str().into(),
        run_id: commit.scope.run_id.clone().expect("test: pending run"),
    };
    let pending = memory
        .recovery_repository()
        .expect("test: pending repository")
        .read_recovery_receipt(&pending_chain)
        .expect("test: authenticated pending receipt")
        .expect("test: pending ingress attestation");
    assert_eq!(
        pending.state, "pending",
        "test: deferred settlement must remain pending until its projection runs"
    );
    assert_eq!(
        fixture_readback::erasure_index_rows_for(&workspace, "recovery_attestations", &origin),
        0,
        "terminal origin was already indexed before terminal commit"
    );
    finalize_turn(&fabric, commit, &cost, &workspace)
        .await
        .expect("test: erase commit");
    let tables = fixture_readback::indexed_tables(&workspace);
    assert!(
        tables.iter().any(|table| table == "recovery_attestations"),
        "attestation was not indexed: {tables:?}"
    );
    assert!(
        fixture_readback::erasure_index_rows_for(&workspace, "recovery_attestations", &origin) > 0,
        "terminal UPDATE did not inherit the terminal origin"
    );
    let before_outbox = first_outbox(&workspace);
    assert_closed_outbox_facts(&before_outbox.facts);
    let store = memory.erasure_store().expect("test: conversation erasure store");
    let origins = store
        .origins(&origin.account, Some(&origin.conversation))
        .expect("test: conversation origins");
    for origin in &origins {
        store.begin_delete(origin).expect("test: conversation deletion fence");
    }
    for origin in &origins {
        store.erase_rows(origin).expect("test: conversation erasure");
    }
    assert_eq!(
        fixture_readback::row_count(&workspace, "recovery_attestations"),
        0,
        "test: erasure must remove recovery attestations so deleted evidence cannot authenticate"
    );
    for table in ["message_events", "ingress_recovery", "recovery_transport_index"] {
        assert_eq!(
            fixture_readback::row_count(&workspace, table),
            0,
            "{table} retained orphaned rows"
        );
    }

    assert!(
        fixture_readback::table_exists(&workspace, "ramflux_tombstones"),
        "test: erasure must retain the tombstone table to prevent resurrection"
    );
    assert!(
        fixture_readback::row_count(&workspace, "ramflux_tombstones") >= 1,
        "test: erasure must leave a durable tombstone for the removed evidence"
    );
    assert!(
        fixture_readback::row_count(&workspace, "recovery_tombstones") >= 1,
        "conversation erasure did not retain the stable recovery input key"
    );
    let after_outbox = first_outbox(&workspace);
    assert_closed_outbox_facts(&after_outbox.facts);
    assert_eq!(
        after_outbox.settlement_id, before_outbox.settlement_id,
        "test: erasure must preserve settlement identity for cost recovery"
    );
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    drop(dir);
}

pub(super) fn recovery_restart_from_killed_process() {
    let (dir, workspace) = home_workspace();
    let (mut child, ack) = spawn_recovery_child(&workspace, "commit");
    let payload = ack.recv_timeout(Duration::from_secs(120));
    child.kill().expect("test: kill committed process");
    child.wait().expect("test: reap killed process");
    let payload = payload.expect("test: child ack");
    let value: serde_json::Value = serde_json::from_str(&payload).expect("test: ack json");
    assert_eq!(
        value.get("outbox_state").and_then(serde_json::Value::as_str),
        Some("pending"),
        "test: commit phase left the outbox row non-pending before restart"
    );
    let terminal_before = fixture_readback::terminal_rows(&workspace)
        .expect("test: terminal before restart")
        .into_iter()
        .map(|row| row.raw_payload)
        .collect::<Vec<_>>();
    assert_eq!(
        terminal_before.len(),
        1,
        "test: killed process must leave exactly one durable terminal to recover"
    );
    assert!(
        fixture_readback::indexed_tables(&workspace)
            .iter()
            .any(|table| table == "recovery_attestations"),
        "test: committed recovery evidence must be indexed so erasure can find it"
    );

    let (mut restarted, ack) = spawn_recovery_child(&workspace, "repair");
    let repaired = ack.recv_timeout(Duration::from_secs(120));
    if repaired.is_err() {
        restarted.kill().expect("test: kill process without acknowledgement");
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = restarted.try_wait().expect("test: poll restarted process") {
            break status;
        }
        assert!(Instant::now() < deadline, "test: restarted process exceeded deadline");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "restarted recovery process failed");
    let repaired: serde_json::Value =
        serde_json::from_str(&repaired.expect("test: repair ack")).expect("test: repair json");
    assert_eq!(
        repaired.get("outbox_state").and_then(serde_json::Value::as_str),
        Some("acknowledged"),
        "test: fresh process must acknowledge the interrupted settlement during repair"
    );
    assert_eq!(
        fixture_readback::outbox_rows(&workspace).len(),
        1,
        "test: restart repair must reuse the original outbox intent"
    );
    assert_eq!(
        first_outbox(&workspace).state,
        "acknowledged",
        "test: restart repair must finish acknowledgement so future scans skip the row"
    );
    assert_eq!(
        ledger_lines(&workspace, "prx-killed"),
        1,
        "test: restart repair must append exactly one ledger row"
    );
    let terminal_after = fixture_readback::terminal_rows(&workspace)
        .expect("test: terminal after restart")
        .into_iter()
        .map(|row| row.raw_payload)
        .collect::<Vec<_>>();
    assert_eq!(
        terminal_before, terminal_after,
        "restart changed committed terminal bytes"
    );
    assert!(
        fixture_readback::indexed_tables(&workspace)
            .iter()
            .any(|table| table == "recovery_attestations"),
        "test: restart must preserve the recovery evidence index for later erasure"
    );
    drop(dir);
}

struct RecoveryChild(std::process::Child);
impl std::ops::Deref for RecoveryChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for RecoveryChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for RecoveryChild {
    fn drop(&mut self) {
        // A child must be reaped even while a parent assertion unwinds.
        match self.0.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "test: child cleanup status failed"),
        }
        if let Err(error) = self.0.kill() {
            tracing::warn!(%error, "test: child cleanup kill failed");
        }
        if let Err(error) = self.0.wait() {
            tracing::warn!(%error, "test: child cleanup wait failed");
        }
    }
}

fn spawn_recovery_child(workspace: &Path, phase: &str) -> (RecoveryChild, std::sync::mpsc::Receiver<String>) {
    let child = std::process::Command::new(std::env::current_exe().expect("test: recovery child executable"))
        .args([
            "runtime::ingress_recovery::tests::fresh_process_recovery_child",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("PRX_RECOVERY_TEST_WORKSPACE", workspace)
        .env("PRX_RECOVERY_TEST_PHASE", phase)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("test: spawn recovery child");
    let mut child = RecoveryChild(child);
    let stdout = child.stdout.take().expect("test: child stdout");
    let (sent, ack) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut sent = Some(sent);
        for line in std::io::BufReader::new(std::io::Read::take(stdout, 64 * 1024)).lines() {
            let line = line.expect("test: child line");
            if let Some((_, json)) = line.split_once("PRX_RECOVERY_ACK:")
                && let Some(sent) = sent.take()
            {
                sent.send(json.to_owned()).expect("test: send child acknowledgement");
            }
        }
    });
    (child, ack)
}

pub(super) async fn fresh_process_recovery_child() {
    let workspace =
        std::path::PathBuf::from(std::env::var("PRX_RECOVERY_TEST_WORKSPACE").expect("test: workspace env"));
    let memory = SqliteMemory::new(&workspace)
        .expect("test: process database")
        .with_erasure_origin(tombstone_fixture::erasure_origin())
        .expect("test: process erase scope");
    if std::env::var("PRX_RECOVERY_TEST_PHASE").as_deref() == Ok("repair") {
        let (recovery, tracker) =
            prepare_scope(&workspace, Arc::clone(&memory), false, true, priced_cost_config()).await;
        drop(recovery);
        tracker
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .expect("test: restarted process drain");
        println!(
            "PRX_RECOVERY_ACK:{}",
            serde_json::json!({"outbox_state": first_outbox(&workspace).state})
        );
        return;
    }

    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());
    let cost = priced_cost_config();
    let mut commit = completed_commit("prx-killed");
    let service = bind_terminal_fixture(&workspace, Arc::clone(&memory), &mut commit, &cost).await;
    crate::agent::terminal::recovery_commit::skip_inline_projection(true);
    finalize_turn(&fabric, commit, &cost, &workspace)
        .await
        .expect("test: child commit");
    crate::agent::terminal::recovery_commit::skip_inline_projection(false);
    let state = fixture_readback::outbox_rows(&workspace)
        .first()
        .map(|row| row.state.clone())
        .expect("test: child committed outbox row");
    println!(
        "PRX_RECOVERY_ACK:{}",
        serde_json::json!({ "outbox_state": state, "pid": std::process::id() })
    );
    use std::io::Write;
    std::io::stdout().flush().expect("test: flush child acknowledgement");
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    std::future::pending::<()>().await;
}

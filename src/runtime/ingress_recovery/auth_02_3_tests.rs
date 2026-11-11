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
    std::fs::read_to_string(workspace.join("state").join("costs.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(terminal_id))
        .count()
}

fn assert_closed_outbox_facts(facts: &[u8]) {
    let value: serde_json::Value = serde_json::from_slice(facts).expect("test: outbox facts");
    let object = value.as_object().expect("test: outbox object");
    assert_eq!(object.len(), OUTBOX_FACT_KEYS.len());
    assert!(OUTBOX_FACT_KEYS.iter().all(|key| object.contains_key(*key)));
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
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace).unwrap());
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());
    let cost = priced_cost_config();
    let terminal_id = "prx-loser";
    let mut commit = completed_commit(terminal_id);
    let service = bind_terminal_fixture(&workspace, Arc::clone(&memory), &mut commit, &cost).await;
    let winner = finalize_turn(&fabric, commit.clone(), &cost, &workspace)
        .await
        .expect("test: winner");
    assert!(matches!(
        winner.cost_settlement,
        Some(CostSettlement::ProjectionPending)
    ));
    let winner_row = first_outbox(&workspace);
    assert_eq!(winner_row.settlement_id, terminal_id);
    let winner_owner = winner_row.owner_id.clone();
    assert_eq!(ledger_lines(&workspace, terminal_id), 1);

    let mut loser = commit;
    loser.scope.owner_id = Some("prx_other_owner".into());
    let error = finalize_turn(&fabric, loser, &cost, &workspace)
        .await
        .expect_err("loser");
    assert!(
        error.to_string().contains("recovery terminal commit conflict") || error.to_string().contains("Conflict"),
        "loser: {error:#}"
    );
    let loser_row = first_outbox(&workspace);
    assert_eq!(loser_row.owner_id, winner_owner);
    assert_eq!(loser_row.settlement_id, terminal_id);
    assert_eq!(ledger_lines(&workspace, terminal_id), 1);
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    drop(dir);
}

pub(super) async fn recovery_nonmonetary_pricing_commits_disposition() {
    let (dir, workspace) = home_workspace();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace).unwrap());
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());

    let unknown_id = "prx-unknown";
    let mut unknown = completed_commit(unknown_id);
    let unknown_service =
        bind_terminal_fixture(&workspace, Arc::clone(&memory), &mut unknown, &CostConfig::default()).await;
    let unknown_receipt = finalize_turn(&fabric, unknown, &CostConfig::default(), &workspace)
        .await
        .expect("test: unknown");
    assert!(matches!(
        unknown_receipt.cost_settlement,
        Some(CostSettlement::UnknownPricing)
    ));
    assert_eq!(ledger_lines(&workspace, unknown_id), 0);
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
    assert!(matches!(
        sub_receipt.cost_settlement,
        Some(CostSettlement::Subscription { .. })
    ));
    assert_eq!(ledger_lines(&workspace, sub_id), 0);

    let rows = fixture_readback::outbox_rows(&workspace);
    assert!(rows.iter().all(|row| row.state == "acknowledged"));
    assert!(rows.iter().any(|row| row.pricing == "unknown"));
    assert!(rows.iter().any(|row| row.pricing == "subscription"));
    for row in &rows {
        assert_closed_outbox_facts(&row.facts);
    }
    sub_service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    drop(dir);
}

pub(super) async fn recovery_terminal_claim_each_bound_field_is_refused() {
    let (dir, workspace) = home_workspace();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace).unwrap());
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());
    let cost = priced_cost_config();
    let mut draft = completed_commit("prx-claim-bound");
    let service = bind_terminal_fixture(&workspace, Arc::clone(&memory), &mut draft, &cost).await;
    finalize_turn(&fabric, draft.clone(), &cost, &workspace)
        .await
        .expect("test: claim winner");
    let commit: crate::agent::terminal::TurnTerminalCommit = draft.clone().try_into().expect("test: commit");
    let recorded = commit.decision.is_recorded();
    let semantic =
        crate::agent::terminal::recovery_commit::claim_semantic_for_test(&commit, fabric.workspace_id(), recorded)
            .expect("test: semantic");
    let payload: serde_json::Value = serde_json::from_str(&first_claim_payload(&workspace)).expect("test: claim json");
    let key_id = payload
        .get("recovery_key_id")
        .and_then(serde_json::Value::as_str)
        .expect("test: recovery_key_id")
        .to_owned();
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

    let refuse_field = |cell: &str, mutate: &dyn Fn(&mut serde_json::Value)| {
        let mut mutated = semantic.clone();
        mutate(&mut mutated);
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
    refuse_field("terminal_id", &|value| {
        value
            .as_object_mut()
            .expect("test: semantic object")
            .insert("terminal_id".into(), serde_json::json!("other"));
    });
    refuse_field("decision_id", &|value| {
        value
            .as_object_mut()
            .expect("test: semantic object")
            .insert("decision_id".into(), serde_json::json!(uuid::Uuid::nil().to_string()));
    });
    refuse_field("workspace", &|value| {
        value
            .as_object_mut()
            .expect("test: semantic object")
            .insert("workspace_id".into(), serde_json::json!("other-workspace"));
    });
    refuse_field("source_event", &|value| {
        value.as_object_mut().expect("test: semantic object").insert(
            "source_event_id".into(),
            serde_json::json!(uuid::Uuid::nil().to_string()),
        );
    });
    refuse_field("status", &|value| {
        value
            .as_object_mut()
            .expect("test: semantic object")
            .insert("status".into(), serde_json::json!("failed"));
    });
    refuse_field("scope", &|value| {
        value
            .get_mut("scope")
            .expect("test: nested semantic")
            .as_object_mut()
            .expect("test: nested object")
            .insert("session_key".into(), serde_json::json!("other"));
    });
    refuse_field("history", &|value| {
        value
            .get_mut("history")
            .expect("test: nested semantic")
            .as_object_mut()
            .expect("test: nested object")
            .insert("assistant_content".into(), serde_json::json!("mutated"));
    });
    refuse_field("provider_outcome", &|value| {
        value
            .get_mut("provider_outcome")
            .expect("test: nested semantic")
            .as_object_mut()
            .expect("test: nested object")
            .insert("final_model".into(), serde_json::json!("other-model"));
    });
    refuse_field("delivery_intent", &|value| {
        value
            .as_object_mut()
            .expect("test: semantic object")
            .insert("delivery_intent".into(), serde_json::json!("none"));
    });
    refuse_field("execution_trace", &|value| {
        value
            .as_object_mut()
            .expect("test: semantic object")
            .insert("execution_trace".into(), serde_json::json!({"mutated": true}));
    });
    refuse_field("decision_chain", &|value| {
        value
            .as_object_mut()
            .expect("test: semantic object")
            .insert("decision_chain".into(), serde_json::json!(["other"]));
    });
    refuse_field("catalog_generation", &|value| {
        value
            .as_object_mut()
            .expect("test: semantic object")
            .insert("catalog_generation".into(), serde_json::json!("00"));
    });
    refuse_field("model_catalog_revision", &|value| {
        value
            .as_object_mut()
            .expect("test: semantic object")
            .insert("model_catalog_revision".into(), serde_json::json!("other"));
    });
    refuse_field("model_catalog_content_sha256", &|value| {
        value
            .as_object_mut()
            .expect("test: semantic object")
            .insert("model_catalog_content_sha256".into(), serde_json::json!("00"));
    });
    refuse_field("policy_version", &|value| {
        value
            .as_object_mut()
            .expect("test: semantic object")
            .insert("policy_version".into(), serde_json::json!(0));
    });
    refuse_field("history_scope", &|value| {
        value
            .as_object_mut()
            .expect("test: semantic object")
            .insert("history_scope".into(), serde_json::json!({"session_key": "other"}));
    });
    refuse_field("telemetry", &|value| {
        value
            .get_mut("telemetry")
            .expect("test: nested semantic")
            .as_object_mut()
            .expect("test: nested object")
            .insert("summary".into(), serde_json::json!("mutated"));
    });
    refuse_field("prepared_parameters", &|value| {
        value
            .as_object_mut()
            .expect("test: semantic object")
            .insert("prepared_parameters".into(), serde_json::json!([{"mutated": true}]));
    });
    let mut key_preimage = serde_json::to_vec(&semantic).expect("test: key preimage");
    key_preimage.extend_from_slice(b"other-key");
    let key_err = repository
        .verify_recovery_mac(&tag, RecoveryMacDomain::ClaimV3, &key_preimage)
        .expect_err("key id");
    assert!(
        key_err.to_string().contains("invalid recovery authentication"),
        "key id: {key_err:#}"
    );

    let mut replay = draft.clone();
    replay.status = TurnTerminalStatus::Failed;
    let error = finalize_turn(&fabric, replay, &cost, &workspace)
        .await
        .expect_err("replay mutated status");
    assert!(
        error.to_string().contains("recovery terminal commit conflict") || error.to_string().contains("conflict"),
        "replay mutated status: {error:#}"
    );
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;

    let mut unrecorded = completed_commit("prx-claim-unrecorded");
    let unrecorded_service =
        bind_terminal_fixture_mode(&workspace, Arc::clone(&memory), &mut unrecorded, &cost, false).await;
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
            .is_some()
    );
    assert!(
        value
            .get("recovery_key_id")
            .and_then(serde_json::Value::as_str)
            .is_some()
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
            .unwrap_or_else(|error| panic!("{backend} commit: {error:#}"));
        assert!(matches!(first.cost_settlement, Some(CostSettlement::ProjectionPending)));
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
                Some("done")
            );
        }
        let second = finalize_turn(&fabric, commit.clone(), &cost, &workspace)
            .await
            .unwrap_or_else(|error| panic!("{backend} replay: {error:#}"));
        assert_eq!(first.terminal_event.event_id, second.terminal_event.event_id);
        assert_eq!(ledger_lines(&workspace, &terminal_id), 1);
        let owner = commit.scope.owner_id.clone().expect("test: owner");
        let (recovery, tracker) = prepare_scope(&workspace, Arc::clone(&memory), false, true, cost.clone()).await;
        recovery
            .rejoin_terminal(&terminal_id, &owner)
            .await
            .unwrap_or_else(|error| panic!("{backend} plane-off rejoin: {error:#}"));
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        tracker
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .ok();
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
    assert!(matches!(receipt.cost_settlement, Some(CostSettlement::Recorded { .. })));
    assert_eq!(crate::cost::tracker::settle_metered_call_count(&workspace), 1);
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    drop(dir);
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
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace).unwrap());
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
            .expect_err("crash before commit");
        assert!(error.to_string().contains("test: crash"), "{error:#}");
        assert!(fixture_readback::outbox_rows(&workspace).is_empty());
        assert_eq!(ledger_lines(&workspace, "prx-crash-before-commit"), 0);
        let _ = crate::agent::terminal::recovery_commit::scan_pending_recovery_costs(
            memory.as_ref(),
            &workspace.to_string_lossy(),
            &workspace,
            &cost,
        );
        assert!(fixture_readback::outbox_rows(&workspace).is_empty());
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(dir);
    }

    {
        let (dir, workspace, memory, fabric, commit, cost, service) = priced_setup("prx-crash-before-append").await;
        crate::cost::tracker::crash_before_append(true);
        finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect("test: crash before append still commits");
        assert_eq!(fixture_readback::outbox_rows(&workspace).len(), 1);
        assert_eq!(first_outbox(&workspace).state, "pending");
        assert_eq!(ledger_lines(&workspace, "prx-crash-before-append"), 0);
        crate::agent::terminal::recovery_commit::scan_pending_recovery_costs(
            memory.as_ref(),
            &workspace.to_string_lossy(),
            &workspace,
            &cost,
        )
        .expect("test: scan after crash before append");
        assert_eq!(first_outbox(&workspace).state, "acknowledged");
        assert_eq!(ledger_lines(&workspace, "prx-crash-before-append"), 1);
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(dir);
    }

    {
        let (dir, workspace, memory, fabric, commit, cost, service) = priced_setup("prx-crash-after-append").await;
        crate::cost::tracker::crash_after_append(true);
        finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect("test: crash after append still commits");
        assert_eq!(first_outbox(&workspace).state, "pending");
        assert_eq!(ledger_lines(&workspace, "prx-crash-after-append"), 1);
        crate::agent::terminal::recovery_commit::scan_pending_recovery_costs(
            memory.as_ref(),
            &workspace.to_string_lossy(),
            &workspace,
            &cost,
        )
        .expect("test: scan after crash after append");
        assert_eq!(first_outbox(&workspace).state, "acknowledged");
        assert_eq!(ledger_lines(&workspace, "prx-crash-after-append"), 1);
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
        assert_eq!(first_outbox(&workspace).state, "pending");
        assert_eq!(ledger_lines(&workspace, "prx-crash-before-ack"), 1);
        crate::agent::terminal::recovery_commit::scan_pending_recovery_costs(
            memory.as_ref(),
            &workspace.to_string_lossy(),
            &workspace,
            &cost,
        )
        .expect("test: scan after crash before ack");
        assert_eq!(first_outbox(&workspace).state, "acknowledged");
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(dir);
    }

    {
        let (dir, workspace, memory, fabric, commit, cost, service) = priced_setup("prx-skip-inline").await;
        let _skip = SkipInline;
        crate::agent::terminal::recovery_commit::skip_inline_projection(true);
        finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect("test: skip inline");
        assert_eq!(first_outbox(&workspace).state, "pending");
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
        assert_eq!(ledger_lines(&workspace, "prx-skip-inline"), 0);
        let (recovery, tracker) = prepare_scope(&workspace, Arc::clone(&memory), false, true, cost.clone()).await;
        drop(recovery);
        tracker
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .ok();
        assert_eq!(first_outbox(&workspace).state, "acknowledged");
        assert_eq!(ledger_lines(&workspace, "prx-skip-inline"), 1);
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(dir);
    }

    {
        let (dir, workspace, memory, fabric, commit, cost, service) = priced_setup("prx-io-error").await;
        crate::cost::tracker::force_io_error(true);
        finalize_turn(&fabric, commit, &cost, &workspace)
            .await
            .expect("test: I/O still commits");
        assert_eq!(first_outbox(&workspace).state, "error");
        crate::agent::terminal::recovery_commit::scan_pending_recovery_costs(
            memory.as_ref(),
            &workspace.to_string_lossy(),
            &workspace,
            &cost,
        )
        .expect("test: scan error row");
        assert_eq!(first_outbox(&workspace).state, "acknowledged");
        assert_eq!(ledger_lines(&workspace, "prx-io-error"), 1);
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
        std::fs::create_dir_all(workspace.join("state")).unwrap();
        std::fs::write(workspace.join("state").join("costs.jsonl"), "{\"usage\":{}}").unwrap();
        crate::agent::terminal::recovery_commit::scan_pending_recovery_costs(
            memory.as_ref(),
            &workspace.to_string_lossy(),
            &workspace,
            &cost,
        )
        .expect("test: torn scan");
        assert_eq!(first_outbox(&workspace).state, "blocked");
        let after = std::fs::read_to_string(workspace.join("state").join("costs.jsonl")).unwrap();
        assert_eq!(after, "{\"usage\":{}}");
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
        std::fs::create_dir_all(workspace.join("state")).unwrap();
        let lock_path = workspace.join("state").join("costs.jsonl.lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        lock_file.lock().unwrap();
        let (recovery, tracker) = prepare_scope(&workspace, Arc::clone(&memory), true, true, cost.clone()).await;
        drop(lock_file);
        drop(recovery);
        tracker
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .ok();
        assert_eq!(first_outbox(&workspace).state, "error");
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
        assert_eq!(first.terminal_event.event_id, second.terminal_event.event_id);
        assert_eq!(ledger_lines(&workspace, "prx-concurrent"), 1);
        assert_eq!(first_outbox(&workspace).state, "acknowledged");
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
        assert_eq!(first_outbox(&workspace).state, "blocked");
        assert_eq!(std::fs::read(&ledger).expect("test: conflict bytes after"), before);
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
        assert_eq!(first_outbox(&workspace).state, "pending");
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
        assert_eq!(final_event.event_id, first.terminal_event.event_id);
        assert_eq!(final_event.raw_payload_json, first.terminal_event.raw_payload_json);
        assert_eq!(first_outbox(&workspace).state, "acknowledged");
        assert_eq!(ledger_lines(&workspace, &terminal_id), 1);
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
    assert!(matches!(receipt.cost_settlement, Some(CostSettlement::Recorded { .. })));
    assert_eq!(crate::cost::tracker::settle_metered_call_count(&workspace), 1);
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    drop(dir);
}

pub(super) async fn recovery_erasure_indexes_settlement_terminal() {
    let (dir, workspace) = home_workspace();
    let sqlite = SqliteMemory::new(&workspace).unwrap();
    let origin = tombstone_fixture::erasure_origin();
    let memory = sqlite.with_erasure_origin(origin.clone()).expect("test: erasure view");
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());
    let cost = priced_cost_config();
    let mut commit = completed_commit("prx-erase");
    let service = bind_terminal_fixture(&workspace, Arc::clone(&memory), &mut commit, &cost).await;
    finalize_turn(&fabric, commit, &cost, &workspace)
        .await
        .expect("test: erase commit");
    let tables = fixture_readback::indexed_tables(&workspace);
    assert!(
        tables.iter().any(|table| table == "recovery_attestations"),
        "attestation was not indexed: {tables:?}"
    );
    let before_outbox = first_outbox(&workspace);
    assert_closed_outbox_facts(&before_outbox.facts);
    tombstone_fixture::erase(&memory);
    assert_eq!(fixture_readback::row_count(&workspace, "recovery_attestations"), 0);
    assert!(fixture_readback::table_exists(&workspace, "ramflux_tombstones"));
    assert!(fixture_readback::row_count(&workspace, "ramflux_tombstones") >= 1);
    let after_outbox = first_outbox(&workspace);
    assert_closed_outbox_facts(&after_outbox.facts);
    assert_eq!(after_outbox.settlement_id, before_outbox.settlement_id);
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    drop(dir);
}

pub(super) async fn recovery_restart_from_killed_process() {
    let (dir, workspace) = home_workspace();
    let (mut child, ack) = spawn_recovery_child(&workspace, "commit");
    let payload = ack.recv_timeout(Duration::from_secs(120));
    let _ = child.kill();
    let _ = child.wait();
    let payload = payload.expect("test: child ack");
    let value: serde_json::Value = serde_json::from_str(&payload).expect("test: ack json");
    assert_eq!(
        value.get("outbox_state").and_then(serde_json::Value::as_str),
        Some("pending")
    );
    let terminal_before = fixture_readback::terminal_rows(&workspace)
        .expect("test: terminal before restart")
        .into_iter()
        .map(|row| row.raw_payload)
        .collect::<Vec<_>>();
    assert_eq!(terminal_before.len(), 1);
    assert!(
        fixture_readback::indexed_tables(&workspace)
            .iter()
            .any(|table| table == "recovery_attestations")
    );

    let (mut restarted, ack) = spawn_recovery_child(&workspace, "repair");
    let repaired = ack.recv_timeout(Duration::from_secs(120));
    if repaired.is_err() {
        let _ = restarted.kill();
    }
    let status = restarted.wait().expect("test: wait for restarted process");
    assert!(status.success(), "restarted recovery process failed");
    let repaired: serde_json::Value =
        serde_json::from_str(&repaired.expect("test: repair ack")).expect("test: repair json");
    assert_eq!(
        repaired.get("outbox_state").and_then(serde_json::Value::as_str),
        Some("acknowledged")
    );
    assert_eq!(fixture_readback::outbox_rows(&workspace).len(), 1);
    assert_eq!(first_outbox(&workspace).state, "acknowledged");
    assert_eq!(ledger_lines(&workspace, "prx-killed"), 1);
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
            .any(|table| table == "recovery_attestations")
    );
    drop(dir);
}

fn spawn_recovery_child(workspace: &Path, phase: &str) -> (std::process::Child, std::sync::mpsc::Receiver<String>) {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
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
    let stdout = child.stdout.take().expect("test: child stdout");
    let (sent, ack) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout).lines() {
            let line = line.expect("test: child line");
            if let Some((_, json)) = line.split_once("PRX_RECOVERY_ACK:") {
                sent.send(json.to_owned()).ok();
                break;
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
        .unwrap_or_default();
    println!(
        "PRX_RECOVERY_ACK:{}",
        serde_json::json!({ "outbox_state": state, "pid": std::process::id() })
    );
    use std::io::Write;
    std::io::stdout().flush().ok();
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    std::future::pending::<()>().await;
}

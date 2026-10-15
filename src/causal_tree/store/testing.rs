//! Fixtures for the store's and evaluation's tests: records, and
//! outcomes written straight to the database (the outcome writer is
//! CDP-22's).
#![cfg(test)]

use super::*;

/// A synthetic complete single-call settlement for scoped latency-reader
/// integration tests. Production outcomes use the bounded writer.
pub(crate) fn write_single_call_latency(
    workspace_dir: &Path,
    decision_id: DecisionId,
    settlement_id: &str,
    capability_id: &str,
    latency_ms: u32,
) {
    let conn = Connection::open(db_path(workspace_dir)).expect("test: open store");
    let family = capability_id.split(':').next().expect("test: family");
    let ids = serde_json::to_string(&[capability_id]).expect("test: ids");
    let calls = serde_json::to_string(&[RecordedToolCall {
        capability_id: capability_id.to_owned(),
        status: "success".to_owned(),
    }])
    .expect("test: calls");
    conn.execute(
        "INSERT INTO outcomes (decision_id, settlement_id, recorded_at_ms, classification,
             executed_action, executed_target, tool_calls_json, capability_ids_json, latency_ms,
             completeness, record_hash) VALUES (?1, ?2, ?3, 'success', 'use_tool', ?4, ?5, ?6, ?7,
             'complete', 'synthetic-test')",
        params![
            decision_id.to_string(),
            settlement_id,
            now_ms(),
            family,
            calls,
            ids,
            i64::from(latency_ms)
        ],
    )
    .expect("test: committed latency outcome");
}

/// A complete decision record for `id` in `workspace`, owned by `owner`.
pub(crate) fn record(id: DecisionId, workspace: &str, owner: Option<&str>, created_at_ms: i64) -> DecisionRecord {
    DecisionRecord {
        decision_id: id,
        created_at_ms,
        lineage: RecordLineage {
            workspace_id: Arc::from(workspace),
            owner_id: owner.map(Arc::from),
            run_id: Some(Arc::from("run-1")),
            ..RecordLineage::default()
        },
        enabled_snapshot: true,
        state_fingerprint: "f".repeat(64),
        redacted_projection: None,
        protocol_version: 1,
        policy_version: "policy-v1".to_string(),
        catalog_generation: "catalog-1".to_string(),
        scorer_version: "scorer-v1".to_string(),
        backend_kind: "builtin".to_string(),
        backend_locality: "builtin".to_string(),
        configured_model: "builtin-v1".to_string(),
        backend_endpoint: None,
        claimed_model: None,
        provider_request_id: None,
        candidates: vec![
            RecordedCandidate {
                id: "c-answer".to_string(),
                action: "answer".to_string(),
                target: "none".to_string(),
                propensity: None,
            },
            RecordedCandidate {
                id: "c-retrieve".to_string(),
                action: "retrieve".to_string(),
                target: "memory".to_string(),
                propensity: None,
            },
        ],
        gates: serde_json::json!([]),
        distributions: RecordedDistributions::default(),
        builtin_shadow: RecordedShadow::default(),
        features: serde_json::json!({}),
        selected_candidate_id: Some("c-answer".to_string()),
        directive_kind: "answer".to_string(),
        fallback_order: vec!["builtin".to_string()],
        abstention_reason: None,
        propensity: 1.0,
        explored: false,
        latency_ms: 12,
        usage: None,
        attempts: Vec::new(),
        error_class: None,
        cache_hit: false,
        response_sha256: None,
        legacy_v1: false,
    }
}

/// An outcome row, written directly.
pub(crate) struct OutcomeFixture<'a> {
    pub(crate) settlement_id: &'a str,
    pub(crate) classification: &'a str,
    pub(crate) executed_action: Option<&'a str>,
    pub(crate) executed_target: Option<&'a str>,
    pub(crate) reward: Option<serde_json::Value>,
    pub(crate) cost_micro_usd: Option<i64>,
    pub(crate) provider_attempts: usize,
    pub(crate) bytes: Option<(i64, i64, i64, i64)>,
    pub(crate) denied_tool_attempts: Option<i64>,
    pub(crate) completeness: &'a str,
}

impl Default for OutcomeFixture<'_> {
    fn default() -> Self {
        Self {
            settlement_id: "settlement-1",
            classification: "success",
            executed_action: Some("answer"),
            executed_target: Some("none"),
            reward: None,
            cost_micro_usd: None,
            provider_attempts: 1,
            bytes: None,
            denied_tool_attempts: None,
            completeness: "complete",
        }
    }
}

/// Write `outcome` for `decision` into the store of `workspace_dir`.
pub(crate) fn write_outcome(workspace_dir: &Path, decision: DecisionId, outcome: &OutcomeFixture<'_>) {
    let conn = Connection::open(db_path(workspace_dir)).expect("test: open");
    let attempts = serde_json::to_string(&vec![serde_json::json!({}); outcome.provider_attempts]).expect("test");
    let (prompt_before, prompt_after, tool_before, tool_after) = match outcome.bytes {
        Some((a, b, c, d)) => (Some(a), Some(b), Some(c), Some(d)),
        None => (None, None, None, None),
    };
    conn.execute(
        "INSERT INTO outcomes (decision_id, settlement_id, recorded_at_ms, classification, executed_action, \
             executed_target, provider_attempts_json, reward_json, cost_micro_usd, prompt_bytes_before, \
             prompt_bytes_after, tool_bytes_before, tool_bytes_after, denied_tool_attempts, completeness, \
             record_hash) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 'test')",
        params![
            decision.to_string(),
            outcome.settlement_id,
            outcome.classification,
            outcome.executed_action,
            outcome.executed_target,
            attempts,
            outcome.reward.as_ref().map(ToString::to_string),
            outcome.cost_micro_usd,
            prompt_before,
            prompt_after,
            tool_before,
            tool_after,
            outcome.denied_tool_attempts,
            outcome.completeness,
        ],
    )
    .expect("test: outcome");
}

/// Open a read-write store over `workspace_dir` that is not shared with
/// other tests.
pub(crate) fn open_private(workspace_dir: &Path, events: Arc<dyn CausalEventSink>) -> CausalDecisionStore {
    CausalDecisionStore::open(
        workspace_dir,
        Retention::days(30, 30),
        events,
        StoreMode::ReadWrite,
        WriterSetup::default(),
    )
    .expect("test: open")
}

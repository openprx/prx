#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use super::testing::{OutcomeFixture, open_private, record, write_outcome};
use super::*;

// Frozen pre-optimization implementation: independent owned encoding/hash oracle.
// Keep its original field ordering and typed hash logic when changing the writer.
struct LegacyOwnedDecision {
    columns: Vec<(&'static str, rusqlite::types::Value)>,
    record_hash: String,
}
impl LegacyOwnedDecision {
    fn of(record: &DecisionRecord) -> Result<Self, serde_json::Error> {
        use rusqlite::types::Value as V;
        let lineage = &record.lineage;
        let columns = vec![
            ("decision_id", V::Text(record.decision_id.to_string())),
            ("created_at_ms", V::Integer(record.created_at_ms)),
            ("workspace_id", V::Text(lineage.workspace_id.to_string())),
            (
                "prior_decision_id",
                text(lineage.prior_decision_id.map(|id| id.to_string()).as_deref()),
            ),
            ("owner_id", text(lineage.owner_id.as_deref())),
            ("session_key", text(lineage.session_key.as_deref())),
            ("run_id", text(lineage.run_id.as_deref())),
            ("task_id", text(lineage.task_id.as_deref())),
            ("source_event_id", text(lineage.source_event_id.as_deref())),
            ("causation_id", text(lineage.causation_id.as_deref())),
            ("channel", text(lineage.channel.as_deref())),
            ("enabled_snapshot", V::Integer(i64::from(record.enabled_snapshot))),
            ("state_fingerprint", V::Text(record.state_fingerprint.clone())),
            ("redacted_projection", text(record.redacted_projection.as_deref())),
            ("protocol_version", V::Integer(i64::from(record.protocol_version))),
            ("policy_version", V::Text(record.policy_version.clone())),
            ("catalog_generation", V::Text(record.catalog_generation.clone())),
            ("scorer_version", V::Text(record.scorer_version.clone())),
            ("backend_kind", V::Text(record.backend_kind.clone())),
            ("backend_locality", V::Text(record.backend_locality.clone())),
            ("configured_model", V::Text(record.configured_model.clone())),
            ("backend_endpoint", text(record.backend_endpoint.as_deref())),
            ("claimed_model", text(record.claimed_model.as_deref())),
            ("provider_request_id", text(record.provider_request_id.as_deref())),
            ("candidates_json", V::Text(serde_json::to_string(&record.candidates)?)),
            ("gates_json", V::Text(serde_json::to_string(&record.gates)?)),
            (
                "distributions_json",
                V::Text(serde_json::to_string(&record.distributions)?),
            ),
            (
                "builtin_shadow_json",
                V::Text(serde_json::to_string(&record.builtin_shadow)?),
            ),
            ("features_json", V::Text(serde_json::to_string(&record.features)?)),
            ("selected_candidate_id", text(record.selected_candidate_id.as_deref())),
            ("directive_kind", V::Text(record.directive_kind.clone())),
            (
                "fallback_order_json",
                V::Text(serde_json::to_string(&record.fallback_order)?),
            ),
            ("abstention_reason", text(record.abstention_reason.as_deref())),
            ("propensity", V::Real(record.propensity)),
            ("explored", V::Integer(i64::from(record.explored))),
            ("latency_ms", integer(record.latency_ms)),
            (
                "usage_json",
                match &record.usage {
                    Some(usage) => V::Text(serde_json::to_string(usage)?),
                    None => V::Null,
                },
            ),
            ("attempts_json", V::Text(serde_json::to_string(&record.attempts)?)),
            ("error_class", text(record.error_class.as_deref())),
            ("cache_hit", V::Integer(i64::from(record.cache_hit))),
            ("response_sha256", text(record.response_sha256.as_deref())),
            ("legacy_v1", V::Integer(i64::from(record.legacy_v1))),
        ];
        // The hash covers a fixed, versioned list of columns in a fixed
        // order, so the same record always hashes the same (the idempotency
        // comparison), and a column a later schema adds never turns a resent
        // record into a false conflict (D-105 K-2).
        let mut hasher = Sha256::new();
        hasher.update(RECORD_HASH_DOMAIN);
        for (name, value) in columns.iter().filter(|(name, _)| RECORD_HASH_COLUMNS.contains(name)) {
            hasher.update(name.as_bytes());
            hasher.update([0]);
            match value {
                V::Null => hasher.update(b"n"),
                V::Integer(number) => {
                    hasher.update(b"i");
                    hasher.update(number.to_be_bytes());
                }
                V::Real(number) => {
                    hasher.update(b"r");
                    hasher.update(number.to_bits().to_be_bytes());
                }
                V::Text(text) => {
                    hasher.update(b"t");
                    hasher.update((text.len() as u64).to_be_bytes());
                    hasher.update(text.as_bytes());
                }
                V::Blob(bytes) => {
                    hasher.update(b"b");
                    hasher.update((bytes.len() as u64).to_be_bytes());
                    hasher.update(bytes);
                }
            }
        }
        Ok(Self {
            columns,
            record_hash: hex::encode(hasher.finalize()),
        })
    }
}

fn encoding_fixture(rich: bool) -> DecisionRecord {
    let mut value = record(id(21), "prx_encoding_workspace", Some("prx_user"), 10);
    if rich {
        value.lineage.prior_decision_id = Some(id(20));
        value.lineage.session_key = Some(Arc::from("session"));
        value.lineage.task_id = Some(Arc::from("task"));
        value.lineage.source_event_id = Some(Arc::from("source"));
        value.lineage.causation_id = Some(Arc::from("cause"));
        value.lineage.channel = Some(Arc::from("channel"));
        value.redacted_projection = Some("quoted \"\\ \u{03bb}\n".to_owned());
        value.backend_endpoint = Some("https://example.com/v1".to_owned());
        value.claimed_model = Some(String::new());
        value.provider_request_id = Some("request".to_owned());
        value.abstention_reason = Some("reason".to_owned());
        value.error_class = Some("refused".to_owned());
        value.response_sha256 = Some("a".repeat(64));
        value.usage = Some(serde_json::json!({"input_tokens": 3, "output_tokens": 2}));
        value.gates = serde_json::json!([{ "pass": false, "reason": "fixed" }]);
        value.features = serde_json::json!({"value": [null, true, "\u{03bb}"]});
        value.propensity = 0.25;
        value.latency_ms = u64::MAX;
        value.explored = true;
        value.cache_hit = true;
        value.legacy_v1 = true;
    }
    value
}

#[test]
fn borrowed_decision_encoding_matches_frozen_owned_columns_and_hash() {
    for rich in [false, true] {
        let record = encoding_fixture(rich);
        let actual = EncodedDecision::of(&record).expect("borrowed encoding");
        let legacy = LegacyOwnedDecision::of(&record).expect("legacy encoding");
        assert_eq!(actual.columns.len(), legacy.columns.len());
        for ((name, value), (old_name, old_value)) in actual.columns.iter().zip(&legacy.columns) {
            assert_eq!(name, old_name, "column ordering");
            assert_eq!(
                value.as_value_ref(),
                rusqlite::types::ValueRef::from(old_value),
                "column {name}"
            );
        }
        assert_eq!(actual.record_hash, legacy.record_hash, "versioned record hash");
        let mut changed = record.clone();
        changed.policy_version.push_str("-changed");
        assert_ne!(
            EncodedDecision::of(&changed).expect("changed encoding").record_hash,
            legacy.record_hash
        );
    }
}

#[test]
fn fixed_decision_insert_preserves_owned_sqlite_rows_replay_and_conflicts() {
    let mut conn = Connection::open_in_memory().expect("isolated SQLite");
    store_schema::migrate(&mut conn).expect("schema");
    let scope = DecisionScope::Owner {
        workspace_id: Arc::from("prx_encoding_workspace"),
        owner_id: Arc::from("prx_user"),
    };
    for rich in [false, true] {
        let mut record = encoding_fixture(rich);
        record.decision_id = if rich { id(22) } else { id(21) };
        let legacy = LegacyOwnedDecision::of(&record).expect("legacy encoding");
        assert_eq!(insert_decision(&conn, &record).expect("insert"), Inserted::Recorded);
        assert_eq!(
            insert_decision(&conn, &record).expect("repeat"),
            Inserted::AlreadyRecorded
        );
        conn.query_row(
            "SELECT * FROM decisions WHERE decision_id = ?1",
            [record.decision_id.to_string()],
            |row| {
                for (name, expected) in &legacy.columns {
                    assert_eq!(
                        row.get_ref(*name)?,
                        rusqlite::types::ValueRef::from(expected),
                        "persisted {name}"
                    );
                }
                assert_eq!(row.get::<_, String>("record_hash")?, legacy.record_hash);
                Ok(())
            },
        )
        .expect("all old column values preserved");
        let mut digest = Sha256::new();
        digest.update(b"prx.decision.terminal-replay-row.v1\0");
        for name in RECORD_HASH_COLUMNS
            .iter()
            .copied()
            .chain(["prior_decision_id", "backend_endpoint"])
        {
            let (_, value) = legacy
                .columns
                .iter()
                .find(|(column, _)| *column == name)
                .expect("legacy replay column");
            replay_column_hash(&mut digest, name, value.into());
        }
        let expected = hex::encode(digest.finalize());
        assert_eq!(
            record.replay_digest("prx_encoding_workspace").expect("issued replay"),
            expected
        );
        let stored = replay_record_on_connection(&conn, &scope, &record.decision_id)
            .expect("actual replay")
            .expect("owned row");
        assert_eq!(stored.digest, expected);
        let mut conflict = record.clone();
        conflict.lineage.prior_decision_id = Some(id(19));
        assert_eq!(
            insert_decision(&conn, &conflict).expect("prior conflict"),
            Inserted::Conflict
        );
        conflict = record.clone();
        conflict.policy_version.push_str("-different");
        assert_eq!(
            insert_decision(&conn, &conflict).expect("hash conflict"),
            Inserted::Conflict
        );
        assert_eq!(
            replay_record_on_connection(&conn, &scope, &record.decision_id)
                .expect("reread")
                .expect("preserved row")
                .digest,
            expected
        );
    }
}

/// R-I1-confirm Note 1: a store another holder shares is not closed; the
/// last holder's close takes it out of the registry, so the next shared
/// open gets a fresh, writable store instead of the closed one.
#[tokio::test]
async fn the_last_holder_closes_and_unregisters_the_store() {
    let temp = tempfile::TempDir::new().unwrap();
    let open = || {
        CausalDecisionStore::open_shared(
            temp.path(),
            Retention::days(30, 30),
            Arc::new(RecordingSink::default()),
            StoreMode::ReadWrite,
        )
        .unwrap()
    };
    let first = open();
    let second = open();
    assert!(Arc::ptr_eq(&first, &second), "one shared store");
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    CausalDecisionStore::close(first, deadline).await;
    assert!(
        matches!(
            second.writer().unwrap().record_decision(record(
                DecisionId::parse("0192f0c4-0000-7000-8000-000000000001").unwrap(),
                "ws",
                Some("a"),
                10
            )),
            Enqueue::Queued
        ),
        "another holder keeps the store open"
    );
    // Keep the allocation reserved after the final strong reference is
    // dropped: a raw address alone can be reused by a fresh Arc.
    let closed = Arc::downgrade(&second);
    CausalDecisionStore::close(second, deadline).await;
    assert!(closed.upgrade().is_none(), "the last holder releases the closed store");
    let fresh = open();
    assert!(
        !std::sync::Weak::ptr_eq(&closed, &Arc::downgrade(&fresh)),
        "a closed store is never handed out again"
    );
    assert!(
        matches!(
            fresh.writer().unwrap().record_decision(record(
                DecisionId::parse("0192f0c4-0000-7000-8000-000000000002").unwrap(),
                "ws",
                Some("a"),
                11
            )),
            Enqueue::Queued
        ),
        "the fresh store writes"
    );
    CausalDecisionStore::close(fresh, deadline).await;
}

/// D-105 K-2: the record hash covers the fixed schema-1 column list: a
/// columns added later (`backend_endpoint`, `prior_decision_id`) change nothing;
/// every listed column does, and every listed column is a stored one.
#[test]
fn the_record_hash_covers_a_fixed_versioned_column_list() {
    let first_id = DecisionId::parse("0192f0c4-0000-7000-8000-000000000001").unwrap();
    let base = record(first_id, "ws", Some("a"), 10);
    let hash = |record: &DecisionRecord| EncodedDecision::of(record).unwrap().record_hash;
    let mut added = base.clone();
    added.backend_endpoint = Some("http://127.0.0.1:8900/v1".to_string());
    assert_eq!(hash(&base), hash(&added), "a later column is not hashed");
    added.lineage.prior_decision_id = Some(id(2));
    assert_eq!(hash(&base), hash(&added), "legacy record hashes stay stable");
    let mut claimed = base.clone();
    claimed.claimed_model = Some("another".to_string());
    assert_ne!(hash(&base), hash(&claimed), "a listed column is hashed");
    let stored: Vec<&str> = EncodedDecision::of(&base)
        .unwrap()
        .columns
        .iter()
        .map(|(name, _)| *name)
        .collect();
    for column in RECORD_HASH_COLUMNS {
        assert!(stored.contains(&column), "{column} is stored");
    }
    assert_eq!(
        stored
            .iter()
            .filter(|name| !RECORD_HASH_COLUMNS.contains(name))
            .collect::<Vec<_>>(),
        [&"prior_decision_id", &"backend_endpoint"],
        "only later schema columns are outside the list"
    );
}

#[test]
fn prior_decision_link_is_typed_and_part_of_duplicate_conflict_check() {
    let mut conn = Connection::open_in_memory().expect("store");
    store_schema::migrate(&mut conn).expect("schema");
    let mut first = record(id(2), "ws", Some("prx_user"), 10);
    first.lineage.prior_decision_id = Some(id(1));
    assert_eq!(insert_decision(&conn, &first).expect("insert"), Inserted::Recorded);
    assert_eq!(
        insert_decision(&conn, &first).expect("repeat"),
        Inserted::AlreadyRecorded
    );
    let stored: Option<String> = conn
        .query_row(
            "SELECT prior_decision_id FROM decisions WHERE decision_id = ?1",
            [first.decision_id.to_string()],
            |row| row.get(0),
        )
        .expect("stored parent");
    let prior = id(1).to_string();
    assert_eq!(stored.as_deref(), Some(prior.as_str()));
    let columns = decision_columns(&conn).expect("columns");
    let select = format!("SELECT {columns} FROM decisions d WHERE d.decision_id = ?1");
    let read = |conn: &Connection| {
        conn.query_row(&select, [first.decision_id.to_string()], RawDecision::read)
            .expect("read row")
            .decode(Vec::new())
    };
    assert_eq!(read(&conn).expect("typed row").prior_decision_id, Some(id(1)));
    for parent in [None, Some(id(3))] {
        let mut changed = first.clone();
        changed.lineage.prior_decision_id = parent;
        assert_eq!(insert_decision(&conn, &changed).expect("conflict"), Inserted::Conflict);
    }
    let legacy = record(id(4), "ws", Some("prx_user"), 10);
    assert_eq!(insert_decision(&conn, &legacy).expect("legacy"), Inserted::Recorded);
    assert_eq!(
        insert_decision(&conn, &legacy).expect("repeat legacy"),
        Inserted::AlreadyRecorded
    );
    let mut linked = legacy;
    linked.lineage.prior_decision_id = Some(id(3));
    assert_eq!(
        insert_decision(&conn, &linked).expect("legacy conflict"),
        Inserted::Conflict
    );
    conn.execute(
        "UPDATE decisions SET prior_decision_id = 'invalid' WHERE decision_id = ?1",
        [first.decision_id.to_string()],
    )
    .expect("corrupt parent fixture");
    assert!(
        read(&conn).is_err(),
        "a malformed stored parent cannot become an unlinked row"
    );
}

#[tokio::test]
async fn old_read_only_stores_project_absent_columns_without_migration() {
    for version in [1_i64, 2] {
        let temp = tempfile::TempDir::new().expect("workspace");
        let store = Arc::new(open_private(temp.path(), sink()));
        let writer = store.writer().expect("writer");
        let decision_id = id(u8::try_from(version).expect("version"));
        let original = record(decision_id, "ws", Some("prx_user"), now_ms());
        writer.record_decision(original.clone());
        flushed(&store).await;
        CausalDecisionStore::close(store, Instant::now() + std::time::Duration::from_secs(5)).await;

        let path = db_path(temp.path());
        let conn = Connection::open(&path).expect("downgrade fixture");
        conn.execute_batch(
            "DROP TABLE child_outcomes; \
                 DROP INDEX decisions_scope_time; \
                 CREATE INDEX decisions_scope_time ON decisions(workspace_id, owner_id, created_at_ms);",
        )
        .expect("remove v5 child settlements and restore the v1 scope index");
        conn.execute_batch("ALTER TABLE decisions DROP COLUMN prior_decision_id")
            .expect("remove v3 column");
        if version == 1 {
            conn.execute_batch("ALTER TABLE decisions DROP COLUMN backend_endpoint")
                .expect("remove v2 column");
        }
        conn.pragma_update(None, "user_version", version).expect("old version");
        drop(conn);
        let before = std::fs::read(&path).expect("database bytes");
        let read_only = CausalDecisionStore::open_unshared_read_only(temp.path());
        let view = read_only
            .reader()
            .get(&owner(&read_only, "prx_user"), &decision_id)
            .expect("old schema read")
            .expect("owned decision");
        assert_eq!(view.prior_decision_id, None);
        assert_eq!(view.backend_endpoint, None);
        assert_eq!(
            read_only
                .reader()
                .get(&owner(&read_only, "other_user"), &decision_id)
                .expect("scope"),
            None
        );
        assert_eq!(std::fs::read(&path).expect("unchanged database"), before);
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("version read");
        assert_eq!(store_schema::version(&conn).expect("version"), version);
        let hash_before: String = conn
            .query_row(
                "SELECT record_hash FROM decisions WHERE decision_id = ?1",
                [decision_id.to_string()],
                |row| row.get(0),
            )
            .expect("old hash");
        drop(conn);
        drop(read_only);

        let events = sink();
        let migrated = Arc::new(open_private(
            temp.path(),
            Arc::clone(&events) as Arc<dyn CausalEventSink>,
        ));
        let view = migrated
            .reader()
            .get(&owner(&migrated, "prx_user"), &decision_id)
            .expect("migrated read")
            .expect("migrated decision");
        assert_eq!(view.prior_decision_id, None);
        migrated.writer().expect("writer").record_decision(original);
        flushed(&migrated).await;
        assert!(incomplete(&events).is_empty(), "legacy replay remains idempotent");
        CausalDecisionStore::close(migrated, Instant::now() + std::time::Duration::from_secs(5)).await;
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("migrated database");
        assert_eq!(
            store_schema::version(&conn).expect("migrated version"),
            store_schema::SCHEMA_VERSION
        );
        let hash_after: String = conn
            .query_row(
                "SELECT record_hash FROM decisions WHERE decision_id = ?1",
                [decision_id.to_string()],
                |row| row.get(0),
            )
            .expect("migrated hash");
        assert_eq!(hash_after, hash_before);
    }
}
use crate::decision::telemetry::testing::RecordingSink;

fn id(n: u8) -> DecisionId {
    DecisionId::legacy(&[n; 32])
}

fn sink() -> Arc<RecordingSink> {
    Arc::new(RecordingSink::default())
}

#[tokio::test]
async fn writer_drain_reports_unacknowledged_and_disconnected_queue() {
    let (queue, receiver) = std::sync::mpsc::sync_channel(1);
    let writer = DecisionStoreWriter {
        queue,
        events: sink(),
        workspace_id: Arc::from("prx_workspace"),
    };
    assert!(
        !writer
            .drain(Instant::now() + std::time::Duration::from_millis(10))
            .await,
        "a queued marker without a writer acknowledgement is incomplete"
    );
    drop(receiver);
    assert!(
        !writer.drain(Instant::now()).await,
        "a disconnected writer is incomplete"
    );
}

async fn flushed(store: &CausalDecisionStore) {
    store
            .writer()
            .expect("test: writer")
            // Long enough for thousands of queued inserts on a loaded
            // machine: a test that proceeds before the drain would read a
            // half-written store.
            .drain(Instant::now() + std::time::Duration::from_mins(2))
            .await;
}

fn observed_outcome(decision_id: DecisionId) -> OutcomeRecord {
    OutcomeRecord {
        decision_id,
        settlement_id: "prx_terminal_1".to_owned(),
        recorded_at_ms: now_ms(),
        classification: OutcomeClassification::Refusal,
        executed_action: Some("refuse".to_owned()),
        executed_target: Some("none".to_owned()),
        provider_attempts: Some(Vec::new()),
        tool_calls: None,
        capability_ids: None,
        approvals: None,
        tokens_in: None,
        tokens_out: None,
        latency_ms: Some(3),
        cost_micro_usd: None,
        idempotency_status: None,
        reward: None,
        prompt_bytes_before: None,
        prompt_bytes_after: None,
        tool_bytes_before: None,
        tool_bytes_after: None,
        denied_tool_attempts: None,
        completeness: OutcomeCompleteness::Partial,
        coverage: None,
    }
}

#[tokio::test]
async fn capability_latency_uses_only_complete_single_call_owner_and_generation_samples() {
    let temp = tempfile::tempdir().expect("workspace");
    let store = open_private(temp.path(), sink() as Arc<dyn CausalEventSink>);
    let writer = store.writer().expect("writer");
    let target_id = "native_tool:prx_project:read";
    let sparse_id = "native_tool:prx_project:sparse";
    let now = now_ms();
    for n in 1_u8..=14 {
        let decision_id = id(n);
        let owner = if n == 10 { "foreign_owner" } else { "prx_user" };
        let mut decision = record(decision_id, "ws", Some(owner), now + i64::from(n));
        if n == 11 {
            decision.catalog_generation = "old_catalog".to_owned();
        }
        assert_eq!(writer.record_decision(decision), Enqueue::Queued);
        let capability = if (6..=9).contains(&n) { sparse_id } else { target_id };
        let mut outcome = observed_outcome(decision_id);
        outcome.settlement_id = format!("prx_terminal_{n}");
        outcome.classification = OutcomeClassification::Success;
        outcome.executed_action = Some("use_tool".to_owned());
        outcome.executed_target = Some("native_tool".to_owned());
        outcome.capability_ids = Some(vec![capability.to_owned()]);
        outcome.tool_calls = Some(vec![RecordedToolCall {
            capability_id: capability.to_owned(),
            status: "success".to_owned(),
        }]);
        if n == 13 {
            outcome.tool_calls.as_mut().expect("calls").push(RecordedToolCall {
                capability_id: capability.to_owned(),
                status: "success".to_owned(),
            });
        }
        if n == 14 {
            outcome.classification = OutcomeClassification::Failed;
        }
        outcome.approvals = Some(Vec::new());
        outcome.coverage = Some(1.0);
        outcome.completeness = if n == 12 {
            OutcomeCompleteness::Partial
        } else {
            OutcomeCompleteness::Complete
        };
        outcome.latency_ms = Some(u64::from(n) * 10);
        assert_eq!(writer.record_outcome(outcome), Enqueue::Queued);
    }
    flushed(&store).await;
    assert_eq!(store.reader().counts().expect("committed rows").outcomes, 14);
    let workspace_id = store.workspace_id().expect("stable workspace id");
    let p50 = store
        .reader()
        .capability_latency_p50(&workspace_id, "prx_user", "catalog-1")
        .expect("scoped complete samples");
    assert_eq!(p50.get(target_id), Some(&30));
    assert!(!p50.contains_key(sparse_id), "four samples retain the family default");
    assert!(
        store
            .reader()
            .capability_latency_p50(&workspace_id, "foreign_owner", "catalog-1")
            .expect("foreign scope")
            .is_empty()
    );
    let plan = store
        .reader()
        .with(Vec::<String>::new(), |conn| {
            let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {CAPABILITY_LATENCY_SQL}"))?;
            let mut rows = statement.query(params![workspace_id.as_ref(), "prx_user", "catalog-1"])?;
            let mut details = Vec::new();
            while let Some(row) = rows.next()? {
                details.push(row.get(3)?);
            }
            Ok(details)
        })
        .expect("bounded history query plan");
    assert!(
        plan.iter().any(|step| step.contains("decisions_scope_time")),
        "owner history must use the scope/time index: {plan:?}"
    );
    let recent_plan = store
        .reader()
        .with(Vec::<String>::new(), |conn| {
            let mut statement = conn.prepare(
                "EXPLAIN QUERY PLAN SELECT decision_id, catalog_generation, created_at_ms
                     FROM decisions WHERE workspace_id = ?1 AND owner_id = ?2
                     ORDER BY created_at_ms DESC, decision_id DESC LIMIT 1024",
            )?;
            let mut rows = statement.query(params![workspace_id.as_ref(), "prx_user"])?;
            let mut details = Vec::new();
            while let Some(row) = rows.next()? {
                details.push(row.get(3)?);
            }
            Ok(details)
        })
        .expect("recent-decision index order");
    assert!(
        recent_plan.iter().any(|step| step.contains("decisions_scope_time"))
            && recent_plan.iter().all(|step| !step.contains("TEMP B-TREE")),
        "the bounded recent scan must avoid sorting a timestamp tie: {recent_plan:?}"
    );
}

#[tokio::test]
async fn old_read_only_store_keeps_default_latency_without_a_tie_order_index() {
    let temp = tempfile::tempdir().expect("workspace");
    let store = Arc::new(open_private(temp.path(), sink() as Arc<dyn CausalEventSink>));
    let writer = store.writer().expect("writer");
    let capability = "native_tool:prx_project:read";
    for n in 80_u8..85 {
        let decision_id = id(n);
        assert_eq!(
            writer.record_decision(record(decision_id, "ws", Some("prx_user"), now_ms() + i64::from(n))),
            Enqueue::Queued
        );
    }
    flushed(&store).await;
    for n in 80_u8..85 {
        let decision_id = id(n);
        let mut outcome = observed_outcome(decision_id);
        outcome.settlement_id = format!("prx_terminal_{n}");
        outcome.classification = OutcomeClassification::Success;
        outcome.executed_action = Some("use_tool".to_owned());
        outcome.executed_target = Some("native_tool".to_owned());
        outcome.capability_ids = Some(vec![capability.to_owned()]);
        outcome.tool_calls = Some(vec![RecordedToolCall {
            capability_id: capability.to_owned(),
            status: "success".to_owned(),
        }]);
        outcome.approvals = Some(Vec::new());
        outcome.coverage = Some(1.0);
        outcome.completeness = OutcomeCompleteness::Complete;
        outcome.latency_ms = Some(30);
        assert_eq!(writer.record_outcome(outcome), Enqueue::Queued);
    }
    flushed(&store).await;
    let workspace_id = store.workspace_id().expect("stable workspace id");
    assert_eq!(
        store
            .reader()
            .capability_latency_p50(&workspace_id, "prx_user", "catalog-1")
            .expect("current schema")
            .get(capability),
        Some(&30)
    );
    drop(writer);
    CausalDecisionStore::close(store, Instant::now() + std::time::Duration::from_secs(5)).await;
    let path = db_path(temp.path());
    let conn = Connection::open(&path).expect("version-three fixture");
    conn.execute_batch(
        "DROP INDEX decisions_scope_time;
             CREATE INDEX decisions_scope_time ON decisions(workspace_id, owner_id, created_at_ms);",
    )
    .expect("old index");
    conn.pragma_update(None, "user_version", 3).expect("old version");
    drop(conn);
    let before = std::fs::read(&path).expect("database bytes");
    let old = CausalDecisionStore::open_unshared_read_only(temp.path());
    assert!(
        old.reader()
            .capability_latency_p50(&workspace_id, "prx_user", "catalog-1")
            .expect("old read-only schema")
            .is_empty(),
        "old index must retain the versioned family latency default"
    );
    assert_eq!(std::fs::read(&path).expect("immutable database"), before);
}

#[tokio::test]
async fn late_outcome_joins_once_and_a_conflicting_settlement_is_refused() {
    let temp = tempfile::tempdir().expect("workspace");
    let events = sink();
    let decision_id = id(67);
    let first = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
    let writer = first.writer().expect("writer");
    assert_eq!(
        writer.record_decision(record(decision_id, "ws", Some("prx_user"), now_ms())),
        Enqueue::Queued
    );
    flushed(&first).await;
    writer.close(Instant::now() + std::time::Duration::from_secs(5)).await;
    drop(writer);
    drop(first);

    let reopened = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
    let writer = reopened.writer().expect("reopened writer");
    let outcome = observed_outcome(decision_id);
    assert_eq!(writer.record_outcome(outcome.clone()), Enqueue::Queued);
    flushed(&reopened).await;
    let row = reopened
        .reader()
        .get(&operator(&reopened), &decision_id)
        .expect("read")
        .expect("decision");
    assert_eq!(row.outcomes.len(), 1);
    assert_eq!(row.outcomes.first().expect("outcome").classification, "refusal");
    assert_eq!(reopened.reader().counts().expect("counts").outcomes, 1);

    let mut replay = outcome.clone();
    replay.recorded_at_ms = outcome.recorded_at_ms + 43_000;
    assert_eq!(writer.record_outcome(replay), Enqueue::Queued);
    let mut conflicting = outcome.clone();
    conflicting.cost_micro_usd = Some(7);
    assert_eq!(writer.record_outcome(conflicting), Enqueue::Queued);
    assert_eq!(writer.record_outcome(observed_outcome(id(68))), Enqueue::Queued);
    flushed(&reopened).await;
    assert_eq!(reopened.reader().counts().expect("counts").outcomes, 1);
    let stored_at: i64 = Connection::open(db_path(temp.path()))
        .expect("database")
        .query_row(
            "SELECT recorded_at_ms FROM outcomes WHERE decision_id = ?1",
            [decision_id.to_string()],
            |row| row.get(0),
        )
        .expect("first outcome timestamp");
    assert_eq!(
        stored_at, outcome.recorded_at_ms,
        "a delayed replay retains the first arrival timestamp"
    );
    assert_eq!(
        events
            .events
            .lock()
            .iter()
            .filter(|event| event.kind == CausalEventKind::OutcomeRecorded)
            .count(),
        1,
        "only a new committed row emits the recorded event"
    );
    assert_eq!(
        incomplete(&events),
        [IncompleteReason::Conflict, IncompleteReason::UnknownDecision]
    );
}

#[tokio::test]
async fn partial_outcome_at_provider_retention_boundary_reopens_and_replays() {
    let temp = tempfile::tempdir().expect("workspace");
    let events = sink();
    let decision_id = id(70);
    let store = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
    let writer = store.writer().expect("writer");
    assert_eq!(
        writer.record_decision(record(decision_id, "ws", Some("prx_user"), now_ms())),
        Enqueue::Queued
    );
    flushed(&store).await;
    let mut outcome = observed_outcome(decision_id);
    outcome.provider_attempts = Some(
        (1..=128)
            .map(|seq| RecordedOutcomeAttempt {
                seq,
                status: "success".to_owned(),
                error_class: None,
            })
            .collect(),
    );
    assert_eq!(outcome.completeness, OutcomeCompleteness::Partial);
    assert_eq!(outcome.coverage, None);
    assert_eq!(writer.record_outcome(outcome.clone()), Enqueue::Queued);
    flushed(&store).await;
    writer.close(Instant::now() + std::time::Duration::from_secs(5)).await;
    drop(writer);
    drop(store);
    let reopened = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
    let writer = reopened.writer().expect("reopened writer");
    let row = reopened
        .reader()
        .get(&operator(&reopened), &decision_id)
        .expect("read")
        .expect("decision");
    assert_eq!(row.outcomes.len(), 1);
    let retained = row.outcomes.first().expect("bounded outcome");
    assert_eq!(retained.provider_attempts, 128);
    assert_eq!(retained.completeness, "partial");
    assert!(
        reopened
            .reader()
            .outcome_matches(&operator(&reopened), &outcome)
            .expect("exact stored payload")
    );
    assert!(incomplete(&events).is_empty(), "expected an empty value");
    assert_eq!(writer.record_outcome(outcome.clone()), Enqueue::Queued);
    flushed(&reopened).await;
    assert_eq!(reopened.reader().counts().expect("counts").outcomes, 1);
    assert!(
        reopened
            .reader()
            .outcome_matches(&operator(&reopened), &outcome)
            .expect("exact replay payload")
    );
    assert!(incomplete(&events).is_empty(), "expected an empty value");
    outcome
        .provider_attempts
        .as_mut()
        .expect("attempts")
        .push(RecordedOutcomeAttempt {
            seq: 129,
            status: "success".to_owned(),
            error_class: None,
        });
    assert_eq!(writer.record_outcome(outcome), Enqueue::Dropped);
    flushed(&reopened).await;
    assert_eq!(reopened.reader().counts().expect("counts").outcomes, 1);
    assert_eq!(incomplete(&events), [IncompleteReason::Conflict]);
}

#[tokio::test]
async fn outcome_queue_refuses_oversized_or_raw_metadata_without_a_row() {
    assert!(bounded_capability_id("mcp:prx_space:search/r\u{e9}sum\u{e9}"));
    assert!(!bounded_capability_id("mcp:prx_space:private:extra"));
    let temp = tempfile::tempdir().expect("workspace");
    let events = sink();
    let store = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
    let writer = store.writer().expect("writer");
    let decision_id = id(69);
    assert_eq!(
        writer.record_decision(record(decision_id, "ws", Some("prx_user"), now_ms())),
        Enqueue::Queued
    );
    flushed(&store).await;
    let base = observed_outcome(decision_id);
    let private_marker = "private@example.com";
    let mut invalid = Vec::new();
    let mut raw = base.clone();
    raw.settlement_id = format!("prx_{private_marker}");
    invalid.push(raw);
    let mut oversized = base.clone();
    oversized.settlement_id = "x".repeat(200_000);
    invalid.push(oversized);
    let mut action = base.clone();
    action.executed_action = Some("raw user text".to_owned());
    invalid.push(action);
    let mut attempts = base.clone();
    attempts.provider_attempts = Some(
        (1..=129)
            .map(|seq| RecordedOutcomeAttempt {
                seq,
                status: "failed".to_owned(),
                error_class: Some("unavailable".to_owned()),
            })
            .collect(),
    );
    invalid.push(attempts);
    let mut negative_cost = base.clone();
    negative_cost.cost_micro_usd = Some(-1);
    invalid.push(negative_cost);
    let mut negative_time = base.clone();
    negative_time.recorded_at_ms = -1;
    invalid.push(negative_time);
    for outcome in &invalid {
        assert!(!format!("{outcome:?}").contains(private_marker));
    }
    for outcome in invalid {
        assert_eq!(writer.record_outcome(outcome), Enqueue::Dropped);
    }
    flushed(&store).await;
    assert_eq!(store.reader().counts().expect("counts").outcomes, 0);
    assert_eq!(incomplete(&events), vec![IncompleteReason::Conflict; 6]);
}

/// The local operator's scope of `store`.
fn operator(store: &CausalDecisionStore) -> DecisionScope {
    DecisionScope::LocalOperator {
        workspace_id: store.workspace_id().unwrap_or_else(|| Arc::from("no store")),
    }
}

/// `owner`'s scope in `store`.
fn owner(store: &CausalDecisionStore, owner: &str) -> DecisionScope {
    DecisionScope::Owner {
        workspace_id: store.workspace_id().expect("test: id"),
        owner_id: Arc::from(owner),
    }
}

fn incomplete(sink: &RecordingSink) -> Vec<IncompleteReason> {
    sink.events
        .lock()
        .iter()
        .filter_map(|event| event.fields.incomplete)
        .collect()
}

/// V8.04: the same record twice is stored once; a different record
/// under the same id is refused and reported, the first kept.
#[tokio::test]
async fn a_conflicting_decision_is_refused_not_overwritten() {
    let temp = tempfile::TempDir::new().unwrap();
    let events = sink();
    let store = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
    let writer = store.writer().unwrap();
    let first = record(id(1), "ws", Some("alice"), now_ms());
    assert_eq!(writer.record_decision(first.clone()), Enqueue::Queued);
    assert_eq!(writer.record_decision(first.clone()), Enqueue::Queued);
    let mut changed = first.clone();
    changed.directive_kind = "refuse".to_string();
    writer.record_decision(changed);
    flushed(&store).await;
    let view = store.reader().get(&operator(&store), &id(1)).unwrap().unwrap();
    assert_eq!(view.directive_kind, "answer", "the stored record is kept");
    assert_eq!(store.reader().counts().unwrap().decisions, 1);
    assert_eq!(incomplete(&events), [IncompleteReason::Conflict]);
}

#[tokio::test]
async fn decision_debug_omits_private_endpoint_projection_and_lineage() {
    let temp = tempfile::TempDir::new().expect("workspace");
    let events = sink();
    let store = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
    let mut entry = record(id(55), "prx_private_workspace", Some("prx_user"), now_ms());
    entry.backend_endpoint = Some("http://127.0.0.1/prx_private_route".to_string());
    entry.redacted_projection = Some("{\"goal\":\"prx_private_goal\"}".to_string());
    let record_debug = format!("{entry:?}");
    for private in ["prx_private_workspace", "prx_private_route", "prx_private_goal"] {
        assert!(!record_debug.contains(private));
    }
    assert_eq!(store.writer().expect("writer").record_decision(entry), Enqueue::Queued);
    flushed(&store).await;
    let view = store
        .reader()
        .get(&operator(&store), &id(55))
        .expect("read")
        .expect("stored");
    let view_debug = format!("{view:?}");
    for private in ["prx_private_workspace", "prx_private_route", "prx_private_goal"] {
        assert!(!view_debug.contains(private));
    }
    assert_eq!(
        view.backend_endpoint.as_deref(),
        Some("http://127.0.0.1/prx_private_route")
    );
}

#[tokio::test]
async fn decision_chain_transaction_rolls_back_on_conflict_and_emits_commit_after_storage() {
    let temp = tempfile::TempDir::new().expect("workspace");
    let events = sink();
    let store = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
    let writer = store.writer().expect("writer");
    let existing = record(id(2), "ws", Some("prx_user"), 10);
    writer.record_decision(existing.clone());
    flushed(&store).await;

    let first = record(id(1), "ws", Some("prx_user"), 10);
    let mut conflicting_second = existing.clone();
    conflicting_second.lineage.prior_decision_id = Some(first.decision_id);
    assert_eq!(
        writer.record_chain(vec![first.clone(), conflicting_second]),
        Enqueue::Queued
    );
    flushed(&store).await;
    assert!(
        store
            .reader()
            .get(&operator(&store), &first.decision_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(store.reader().counts().unwrap().decisions, 1);
    assert_eq!(
        incomplete(&events),
        [IncompleteReason::Conflict, IncompleteReason::Conflict]
    );

    let mut second = record(id(3), "ws", Some("prx_user"), 10);
    second.lineage.prior_decision_id = Some(first.decision_id);
    let committed_before = events
        .events
        .lock()
        .iter()
        .filter(|event| event.kind == CausalEventKind::DecisionCommitted)
        .count();
    assert_eq!(
        writer.record_chain(vec![first.clone(), second.clone()]),
        Enqueue::Queued
    );
    flushed(&store).await;
    assert_eq!(store.reader().counts().unwrap().decisions, 3);
    assert_eq!(
        store
            .reader()
            .get(&operator(&store), &second.decision_id)
            .unwrap()
            .unwrap()
            .prior_decision_id,
        Some(first.decision_id)
    );
    let committed_after = events
        .events
        .lock()
        .iter()
        .filter(|event| event.kind == CausalEventKind::DecisionCommitted)
        .count();
    assert_eq!(committed_after - committed_before, 2);
    writer.record_chain(vec![first, second]);
    flushed(&store).await;
    assert_eq!(
        events
            .events
            .lock()
            .iter()
            .filter(|event| event.kind == CausalEventKind::DecisionCommitted)
            .count(),
        committed_after,
        "idempotent replay does not emit another commit"
    );
}

#[tokio::test]
async fn decision_chain_rejects_orphan_and_foreign_scope_before_queueing() {
    let temp = tempfile::TempDir::new().expect("workspace");
    let events = sink();
    let store = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
    let writer = store.writer().expect("writer");
    let first = record(id(10), "ws", Some("prx_user"), 10);
    let mut second = record(id(11), "ws", Some("prx_user"), 10);
    second.lineage.prior_decision_id = Some(first.decision_id);

    let mut orphan = record(id(12), "ws", Some("prx_user"), 10);
    orphan.lineage.prior_decision_id = Some(id(99));
    assert_eq!(writer.record_decision(orphan), Enqueue::Dropped);

    let mut invalid = second.clone();
    invalid.lineage.owner_id = Some(Arc::from("other_user"));
    assert_eq!(writer.record_chain(vec![first.clone(), invalid]), Enqueue::Dropped);
    let mut invalid = second.clone();
    invalid.lineage.run_id = Some(Arc::from("other_run"));
    assert_eq!(writer.record_chain(vec![first.clone(), invalid]), Enqueue::Dropped);
    let mut invalid = second.clone();
    invalid.lineage.source_event_id = Some(Arc::from("other_event"));
    assert_eq!(writer.record_chain(vec![first.clone(), invalid]), Enqueue::Dropped);
    let mut invalid = second.clone();
    invalid.lineage.prior_decision_id = Some(id(99));
    assert_eq!(writer.record_chain(vec![first.clone(), invalid]), Enqueue::Dropped);
    let mut invalid = second;
    invalid.decision_id = first.decision_id;
    assert_eq!(writer.record_chain(vec![first.clone(), invalid]), Enqueue::Dropped);

    flushed(&store).await;
    assert_eq!(store.reader().counts().unwrap().decisions, 0);
    assert!(
        events
            .events
            .lock()
            .iter()
            .all(|event| event.kind != CausalEventKind::DecisionCommitted)
    );
    assert_eq!(writer.record_decision(first), Enqueue::Queued);
    flushed(&store).await;
    assert_eq!(store.reader().counts().unwrap().decisions, 1);
}

/// V6.04, store half: another owner's decision reads exactly like an
/// unknown one; the local operator sees every decision of the
/// workspace, including those without an owner; another workspace sees
/// nothing.
#[tokio::test]
async fn inspect_other_owner_decision_denied() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = open_private(temp.path(), sink());
    let writer = store.writer().unwrap();
    writer.record_decision(record(id(1), "ws", Some("alice"), now_ms()));
    writer.record_decision(record(id(2), "ws", None, now_ms()));
    flushed(&store).await;
    let reader = store.reader();
    let alice = owner(&store, "alice");
    let bob = owner(&store, "bob");
    assert!(reader.get(&alice, &id(1)).unwrap().is_some());
    let foreign = reader.get(&bob, &id(1));
    let unknown = reader.get(&bob, &id(9));
    assert!(matches!(foreign, Ok(None)) && matches!(unknown, Ok(None)));
    assert!(reader.get(&alice, &id(2)).unwrap().is_none(), "no owner: operator only");
    assert!(reader.get(&operator(&store), &id(2)).unwrap().is_some());
    assert!(
        reader
            .get(
                &DecisionScope::LocalOperator {
                    workspace_id: Arc::from("another-workspace"),
                },
                &id(1),
            )
            .unwrap()
            .is_none()
    );
}

/// V6.10: the files are private; a corrupt database is moved aside
/// (0600), a fresh store works, and the recovery is reported.
#[tokio::test]
async fn store_permissions_and_corrupt_db_recovery() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::TempDir::new().unwrap();
    let path = db_path(temp.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        b"this is not a sqlite database, only text that looks nothing like one",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let events = sink();
    let store = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
    assert_eq!(incomplete(&events), [IncompleteReason::StoreRecovered]);
    let aside: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("decisions.db.corrupt-"))
        .collect();
    assert_eq!(aside.len(), 1);
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&aside[0].path()), 0o600);
    store
        .writer()
        .unwrap()
        .record_decision(record(id(1), "ws", Some("a"), now_ms()));
    flushed(&store).await;
    assert_eq!(store.reader().counts().unwrap().decisions, 1);
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    for suffix in ["-wal", "-shm"] {
        let side = side_file(&path, suffix);
        if side.exists() {
            assert_eq!(mode(&side), 0o600, "{suffix}");
        }
    }
}

/// A database written by a newer build opens read-only: no writer, reads
/// work, and the file is not changed.
#[tokio::test]
async fn a_newer_schema_opens_read_only() {
    let temp = tempfile::TempDir::new().unwrap();
    {
        let store = open_private(temp.path(), sink());
        store
            .writer()
            .unwrap()
            .record_decision(record(id(1), "ws", Some("a"), now_ms()));
        flushed(&store).await;
    }
    let path = db_path(temp.path());
    {
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", store_schema::SCHEMA_VERSION + 1)
            .unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    let store = open_private(temp.path(), sink());
    assert_eq!(store.mode(), StoreMode::ReadOnly);
    assert!(store.writer().is_none());
    assert!(store.reader().get(&operator(&store), &id(1)).unwrap().is_some());
    drop(store);
    assert_eq!(std::fs::read(&path).unwrap(), before, "the newer database is untouched");
}

mod files_and_recovery;

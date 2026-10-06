//! The decision store's schema (SP-CDP-A 5.10, CDP-PLAN 1.8).
//!
//! Versioning uses `PRAGMA user_version` with forward-only migrations, all
//! applied in one IMMEDIATE transaction that reads the version inside it,
//! so two processes opening one database at once migrate it exactly once
//! (D-091 F-2). A database newer than this build is left untouched: the
//! store opens it read-only, so a downgrade never loses data.
//!
//! The same transaction gives a new store its stable workspace id
//! (`store_meta.workspace_id`, a random UUID), the one scope key of its rows
//! (D-091 F-13).
//!
//! No table name contains `event` or `ledger`. The `decisions` table follows
//! SP-CDP-A 5.10 without the removed `mode` column and with the
//! `enabled_snapshot` of the decision (D-040). `outcomes` carries the
//! executed action and target of a settled turn, which evaluation compares
//! the predicted distributions against (CDP 15).

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

/// The schema version this build writes.
pub(crate) const SCHEMA_VERSION: i64 = 5;

/// Version 1: decisions, outcomes and the store's own metadata.
const V1: &str = "
CREATE TABLE decisions (
  decision_id TEXT PRIMARY KEY,
  created_at_ms INTEGER NOT NULL,
  workspace_id TEXT NOT NULL,
  owner_id TEXT,
  session_key TEXT,
  run_id TEXT,
  task_id TEXT,
  source_event_id TEXT,
  causation_id TEXT,
  channel TEXT,
  enabled_snapshot INTEGER NOT NULL,
  state_fingerprint TEXT NOT NULL,
  redacted_projection TEXT,
  protocol_version INTEGER NOT NULL,
  policy_version TEXT NOT NULL,
  catalog_generation TEXT NOT NULL,
  scorer_version TEXT NOT NULL,
  backend_kind TEXT NOT NULL,
  backend_locality TEXT NOT NULL,
  configured_model TEXT NOT NULL,
  claimed_model TEXT,
  provider_request_id TEXT,
  candidates_json TEXT NOT NULL,
  gates_json TEXT NOT NULL,
  distributions_json TEXT NOT NULL,
  builtin_shadow_json TEXT NOT NULL,
  features_json TEXT NOT NULL,
  selected_candidate_id TEXT,
  directive_kind TEXT NOT NULL,
  fallback_order_json TEXT NOT NULL,
  abstention_reason TEXT,
  propensity REAL NOT NULL,
  explored INTEGER NOT NULL DEFAULT 0,
  latency_ms INTEGER NOT NULL,
  usage_json TEXT,
  attempts_json TEXT NOT NULL,
  error_class TEXT,
  cache_hit INTEGER NOT NULL,
  response_sha256 TEXT,
  legacy_v1 INTEGER NOT NULL DEFAULT 0,
  record_hash TEXT NOT NULL
);
CREATE INDEX decisions_scope_time ON decisions(workspace_id, owner_id, created_at_ms);
CREATE INDEX decisions_time ON decisions(created_at_ms);
CREATE TABLE outcomes (
  decision_id TEXT NOT NULL REFERENCES decisions(decision_id) ON DELETE CASCADE,
  settlement_id TEXT NOT NULL,
  recorded_at_ms INTEGER NOT NULL,
  classification TEXT NOT NULL,
  executed_action TEXT,
  executed_target TEXT,
  provider_attempts_json TEXT,
  tool_calls_json TEXT,
  capability_ids_json TEXT,
  approvals_json TEXT,
  tokens_in INTEGER,
  tokens_out INTEGER,
  latency_ms INTEGER,
  cost_micro_usd INTEGER,
  idempotency_status TEXT,
  reward_json TEXT,
  prompt_bytes_before INTEGER,
  prompt_bytes_after INTEGER,
  tool_bytes_before INTEGER,
  tool_bytes_after INTEGER,
  denied_tool_attempts INTEGER,
  completeness TEXT NOT NULL,
  coverage REAL,
  record_hash TEXT NOT NULL,
  PRIMARY KEY (decision_id, settlement_id)
);
CREATE TABLE store_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

/// Version 2 (D-102 H-2): the endpoint identity a decision's backend was
/// configured with, so a calibration measured on one server is never applied
/// to another serving the same model. `NULL` on rows written before it.
const V2: &str = "ALTER TABLE decisions ADD COLUMN backend_endpoint TEXT;";

/// Version 3: an explicit preceding decision in one bounded hop chain.
/// The link has no foreign key because retention may prune an earlier hop.
const V3: &str = "ALTER TABLE decisions ADD COLUMN prior_decision_id TEXT;";

/// Version 4: the owner-scoped latency reader orders equal-millisecond rows
/// by decision id. Replace the old index in the same migration transaction
/// so a large timestamp tie can be read in index order before LIMIT.
const V4: &str = "
DROP INDEX decisions_scope_time;
CREATE INDEX decisions_scope_time ON decisions(workspace_id, owner_id, created_at_ms, decision_id);
";

/// Version 5: append child settlements without rewriting the parent's own
/// outcome or counting child usage twice in parent latency/cost samples.
const V5: &str = "
CREATE TABLE child_outcomes (
  parent_decision_id TEXT NOT NULL REFERENCES decisions(decision_id) ON DELETE CASCADE,
  child_decision_id TEXT NOT NULL,
  settlement_id TEXT NOT NULL,
  parent_digest TEXT NOT NULL,
  child_digest TEXT NOT NULL,
  outcome_hash TEXT NOT NULL,
  PRIMARY KEY(child_decision_id, settlement_id),
  FOREIGN KEY(child_decision_id, settlement_id) REFERENCES outcomes(decision_id, settlement_id) ON DELETE CASCADE,
  CHECK(parent_decision_id != child_decision_id)
);
CREATE INDEX child_outcomes_parent ON child_outcomes(parent_decision_id, child_decision_id, settlement_id);
";

/// The migrations, in order: entry `n` moves the schema from `n` to `n + 1`.
const MIGRATIONS: [&str; 5] = [V1, V2, V3, V4, V5];

/// What the schema check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchemaState {
    /// The database was at this build's version, or was migrated to it.
    Current,
    /// The database was written by a newer build (its version).
    Newer(i64),
}

/// The schema version of `conn`'s database.
///
/// # Errors
///
/// The SQLite error of the read (a corrupt database fails here).
pub(crate) fn version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

/// Bring `conn`'s database to [`SCHEMA_VERSION`] and give it a workspace
/// id, or report that it is newer and leave it untouched. The version is
/// read inside the IMMEDIATE transaction, so a concurrent migration by
/// another process is seen, not repeated.
///
/// # Errors
///
/// The SQLite error of a failed migration, which is rolled back.
pub(crate) fn migrate(conn: &mut Connection) -> rusqlite::Result<SchemaState> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = version(&transaction)?;
    if current > SCHEMA_VERSION {
        return Ok(SchemaState::Newer(current));
    }
    let from = usize::try_from(current.max(0)).unwrap_or(0);
    for step in MIGRATIONS.iter().skip(from) {
        transaction.execute_batch(step)?;
    }
    if current < SCHEMA_VERSION {
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    transaction.execute(
        "INSERT OR IGNORE INTO store_meta (key, value) VALUES ('workspace_id', ?1)",
        [uuid::Uuid::new_v4().to_string()],
    )?;
    transaction.commit()?;
    Ok(SchemaState::Current)
}

/// The store's workspace id, if it has one.
///
/// # Errors
///
/// The SQLite error of the read.
pub(crate) fn workspace_id(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM store_meta WHERE key = 'workspace_id'", [], |row| {
        row.get(0)
    })
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D-091 F-2: two connections migrating one fresh database at once both
    /// succeed, and the schema and the workspace id are created once.
    #[test]
    fn concurrent_first_opens_migrate_once() {
        let temp = tempfile::TempDir::new().expect("test: temp");
        let path = temp.path().join("store.db");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let (path, barrier) = (path.clone(), std::sync::Arc::clone(&barrier));
                std::thread::spawn(move || {
                    let mut conn = Connection::open(&path).expect("test: open");
                    crate::runtime::sqlite_pool::install_bounded_busy_handler(&conn).expect("test: busy");
                    conn.execute_batch("PRAGMA journal_mode = WAL;").ok();
                    barrier.wait();
                    migrate(&mut conn).expect("test: migrate")
                })
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().expect("test: thread"), SchemaState::Current);
        }
        let conn = Connection::open(&path).expect("test: open");
        assert_eq!(version(&conn).expect("test: version"), SCHEMA_VERSION);
        let ids: i64 = conn
            .query_row(
                "SELECT count(*) FROM store_meta WHERE key = 'workspace_id'",
                [],
                |row| row.get(0),
            )
            .expect("test: ids");
        assert_eq!(ids, 1);
    }

    /// D-102 H-2: a version-1 store gains the endpoint column, its rows keep
    /// `NULL` there, and the workspace id is kept.
    #[test]
    fn a_version_one_store_gains_the_endpoint_column() {
        let mut conn = Connection::open_in_memory().expect("test: open");
        conn.execute_batch(V1).expect("test: v1");
        conn.pragma_update(None, "user_version", 1).expect("test: version 1");
        conn.execute(
            "INSERT INTO store_meta (key, value) VALUES ('workspace_id', 'ws-1')",
            [],
        )
        .expect("test: id");
        assert_eq!(migrate(&mut conn).expect("test: migrate"), SchemaState::Current);
        assert_eq!(version(&conn).expect("test: version"), SCHEMA_VERSION);
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('decisions')")
            .and_then(|mut statement| statement.query_map([], |row| row.get(0)).and_then(Iterator::collect))
            .expect("test: columns");
        assert!(columns.iter().any(|name| name == "backend_endpoint"), "{columns:?}");
        assert!(columns.iter().any(|name| name == "prior_decision_id"), "{columns:?}");
        assert_eq!(workspace_id(&conn).expect("test: id").as_deref(), Some("ws-1"));
    }

    #[test]
    fn version_two_store_adds_nullable_prior_link_without_changing_existing_rows() {
        let mut conn = Connection::open_in_memory().expect("test: open");
        conn.execute_batch(V1).expect("test: v1");
        conn.execute_batch(V2).expect("test: v2");
        conn.pragma_update(None, "user_version", 2).expect("test: version 2");
        assert_eq!(migrate(&mut conn).expect("test: migrate"), SchemaState::Current);
        assert_eq!(version(&conn).expect("test: version"), SCHEMA_VERSION);
        let nullable: i64 = conn
            .query_row(
                "SELECT [notnull] FROM pragma_table_info('decisions') WHERE name = 'prior_decision_id'",
                [],
                |row| row.get(0),
            )
            .expect("test: prior column");
        assert_eq!(nullable, 0);
    }

    #[test]
    fn version_three_store_rebuilds_scope_index_with_decision_tiebreaker() {
        let mut conn = Connection::open_in_memory().expect("test: open");
        conn.execute_batch(V1).expect("test: v1");
        conn.execute_batch(V2).expect("test: v2");
        conn.execute_batch(V3).expect("test: v3");
        conn.pragma_update(None, "user_version", 3).expect("test: version 3");
        conn.execute(
            "INSERT INTO store_meta (key, value) VALUES ('workspace_id', 'ws-1')",
            [],
        )
        .expect("test: id");
        assert_eq!(migrate(&mut conn).expect("test: migrate"), SchemaState::Current);
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_index_info('decisions_scope_time') ORDER BY seqno")
            .and_then(|mut statement| statement.query_map([], |row| row.get(0)).and_then(Iterator::collect))
            .expect("test: index columns");
        assert_eq!(columns, ["workspace_id", "owner_id", "created_at_ms", "decision_id"]);
        assert_eq!(workspace_id(&conn).expect("test: id").as_deref(), Some("ws-1"));
        assert_eq!(version(&conn).expect("test: version"), SCHEMA_VERSION);
    }

    #[test]
    fn migration_is_forward_only_and_idempotent() {
        let mut conn = Connection::open_in_memory().expect("test: open");
        assert_eq!(migrate(&mut conn).expect("test: migrate"), SchemaState::Current);
        assert_eq!(version(&conn).expect("test: version"), SCHEMA_VERSION);
        assert_eq!(migrate(&mut conn).expect("test: again"), SchemaState::Current);
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .and_then(|mut statement| statement.query_map([], |row| row.get(0)).and_then(Iterator::collect))
            .expect("test: tables");
        assert_eq!(tables, ["child_outcomes", "decisions", "outcomes", "store_meta"]);
        assert!(
            tables
                .iter()
                .all(|name| !name.contains("event") && !name.contains("ledger")),
            "no store table joins the event inventory"
        );
        let id = workspace_id(&conn)
            .expect("test: id")
            .expect("test: a new store has an id");
        assert_eq!(migrate(&mut conn).expect("test: again"), SchemaState::Current);
        assert_eq!(workspace_id(&conn).expect("test: id"), Some(id), "the id is stable");
        conn.pragma_update(None, "user_version", 9).expect("test: newer");
        assert_eq!(migrate(&mut conn).expect("test: newer"), SchemaState::Newer(9));
        assert_eq!(version(&conn).expect("test: version"), 9, "a newer schema is untouched");
    }
}

//! Multi-agent registry and erase schema (MA-01b1a): DDL, constraints and
//! triggers only. Minting, retirement and erase behaviour live in later parts.

/// The closed inventory of erase-manifest stores (design inventory enum).
const MANIFEST_STORES: [&str; 33] = [
    "conversation_turns",
    "message_events",
    "memories",
    "documents",
    "document_chunks",
    "embedding_cache",
    "memory_events",
    "memory_drafts",
    "memory_trash",
    "memory_links",
    "retrieval_traces",
    "compaction_runs",
    "topics",
    "topic_participants",
    "topic_aliases",
    "router_history_v1",
    "router_learning_state",
    "router_learning_receipts",
    "chat_profiles",
    "chat_sessions",
    "chat_assignments",
    "chat_results",
    "decisions",
    "outcomes",
    "child_outcomes",
    "engine_outcomes",
    "access_audit_log",
    "approval_grants",
    "approval_grant_events",
    "approval_grant_revocations",
    "ingress_recovery",
    "recovery_attestations",
    "ramflux_blobs",
];

/// Byte length of a text column.
fn bytes(col: &str) -> String {
    format!("length(CAST({col} AS BLOB))")
}

/// No embedded NUL byte. GLOB and `length()` stop at the first NUL, so every
/// text validator must also carry this conjunct.
fn no_nul(col: &str) -> String {
    format!("instr(CAST({col} AS BLOB), x'00') = 0")
}

/// Canonical lowercase 36-byte UUID.
fn uuid(col: &str) -> String {
    let hex = |n: usize| "[0-9a-f]".repeat(n);
    format!(
        "{} = 36 AND {} AND {col} GLOB '{}-{}-{}-{}-{}'",
        bytes(col),
        no_nul(col),
        hex(8),
        hex(4),
        hex(4),
        hex(4),
        hex(12)
    )
}

/// Lowercase SHA-256 hex, exactly 64 bytes.
fn hex64(col: &str) -> String {
    format!(
        "{} = 64 AND {} AND {col} NOT GLOB '*[^0-9a-f]*'",
        bytes(col),
        no_nul(col)
    )
}

/// `m` for Main, or `n:` plus the AgentName grammar; `n:main` is reserved.
fn agent_key(col: &str) -> String {
    format!(
        "{} AND ({col} = 'm' OR ({col} GLOB 'n:[a-z]*' AND substr({col}, 3) NOT GLOB '*[^a-z0-9_-]*' \
         AND {} <= 66)) AND {col} <> 'n:main'",
        no_nul(col),
        bytes(col)
    )
}

fn nullable(col: &str, check: &str) -> String {
    format!("{col} IS NULL OR ({check})")
}

fn schema_sql() -> String {
    let stores = MANIFEST_STORES.join("','");
    let w = |c: &str| format!("{} <= 4096 AND {}", bytes(c), no_nul(c));
    let o = |c: &str| format!("{} <= 512 AND {}", bytes(c), no_nul(c));
    format!(
        "CREATE TABLE IF NOT EXISTS agent_incarnations (
            workspace_id TEXT NOT NULL CHECK({w_ws}),
            owner_id TEXT NOT NULL CHECK({o_owner}),
            agent_key TEXT NOT NULL CHECK({k_key}),
            incarnation TEXT NOT NULL CHECK({u_inc}),
            state TEXT NOT NULL CHECK(state IN ('active','retiring','retired')),
            config_revision TEXT NOT NULL CHECK({h_rev}),
            PRIMARY KEY (workspace_id, owner_id, agent_key, incarnation)
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS agent_incarnations_one_active
            ON agent_incarnations (workspace_id, owner_id, agent_key) WHERE state = 'active';

        CREATE TABLE IF NOT EXISTS agent_scopes (
            scope_id TEXT NOT NULL PRIMARY KEY CHECK({u_scope}),
            workspace_id TEXT NOT NULL CHECK({w_ws}),
            owner_id TEXT NOT NULL CHECK({o_owner}),
            session_key TEXT CHECK(session_key IS NULL OR {o_session}),
            agent_key TEXT NOT NULL CHECK({k_key}),
            incarnation TEXT NOT NULL CHECK({u_inc}),
            generation INTEGER NOT NULL CHECK(generation >= 1),
            CHECK(agent_key <> 'm' OR session_key IS NOT NULL),
            UNIQUE (scope_id, workspace_id, owner_id, incarnation, generation),
            UNIQUE (scope_id, workspace_id, owner_id),
            FOREIGN KEY (workspace_id, owner_id, agent_key, incarnation)
                REFERENCES agent_incarnations (workspace_id, owner_id, agent_key, incarnation)
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS agent_scopes_session_generation
            ON agent_scopes (workspace_id, owner_id, session_key, agent_key, incarnation, generation)
            WHERE session_key IS NOT NULL;
        CREATE UNIQUE INDEX IF NOT EXISTS agent_scopes_private_generation
            ON agent_scopes (workspace_id, owner_id, agent_key, incarnation, generation)
            WHERE session_key IS NULL;

        CREATE TABLE IF NOT EXISTS agent_erase_fences (
            scope_id TEXT NOT NULL PRIMARY KEY CHECK({u_scope}) REFERENCES agent_scopes (scope_id),
            erase_id TEXT NOT NULL CHECK({u_erase}),
            state TEXT NOT NULL CHECK(state IN ('draining','erasing','verifying','erased')),
            next_generation INTEGER NOT NULL CHECK(next_generation >= 1)
        ) STRICT;

        CREATE TABLE IF NOT EXISTS agent_erase_receipts (
            workspace_id TEXT NOT NULL CHECK({w_ws}),
            owner_id TEXT NOT NULL CHECK({o_owner}),
            erase_id TEXT NOT NULL UNIQUE CHECK({u_erase}),
            scope_id TEXT NOT NULL CHECK({u_scope}),
            incarnation TEXT NOT NULL CHECK({u_inc}),
            generation INTEGER NOT NULL CHECK(generation >= 1),
            state TEXT NOT NULL CHECK(state IN ('pending','erased')),
            remaining INTEGER,
            completed_at INTEGER,
            PRIMARY KEY (workspace_id, owner_id, erase_id),
            CHECK((state = 'pending' AND remaining IS NULL AND completed_at IS NULL)
               OR (state = 'erased' AND remaining IS NOT NULL AND remaining = 0
                   AND completed_at IS NOT NULL)),
            FOREIGN KEY (scope_id, workspace_id, owner_id, incarnation, generation)
                REFERENCES agent_scopes (scope_id, workspace_id, owner_id, incarnation, generation)
        ) STRICT;
        CREATE TRIGGER IF NOT EXISTS agent_erase_receipts_immutable_update
            BEFORE UPDATE ON agent_erase_receipts WHEN OLD.state = 'erased'
            BEGIN SELECT RAISE(ABORT, 'erased receipt is immutable'); END;
        -- A REPLACE can hit the primary key or the UNIQUE erase_id; both keys
        -- are checked so neither can displace an erased receipt.
        CREATE TRIGGER IF NOT EXISTS agent_erase_receipts_immutable_insert
            BEFORE INSERT ON agent_erase_receipts
            WHEN EXISTS (SELECT 1 FROM agent_erase_receipts r
                         WHERE r.state = 'erased'
                           AND (r.erase_id = NEW.erase_id
                                OR (r.workspace_id = NEW.workspace_id
                                    AND r.owner_id = NEW.owner_id
                                    AND r.erase_id = NEW.erase_id)))
            BEGIN SELECT RAISE(ABORT, 'erased receipt is immutable'); END;
        CREATE TRIGGER IF NOT EXISTS agent_erase_receipts_immutable_update_onto
            BEFORE UPDATE ON agent_erase_receipts
            WHEN EXISTS (SELECT 1 FROM agent_erase_receipts r
                         WHERE r.state = 'erased' AND r.rowid <> OLD.rowid
                           AND (r.erase_id = NEW.erase_id
                                OR (r.workspace_id = NEW.workspace_id
                                    AND r.owner_id = NEW.owner_id
                                    AND r.erase_id = NEW.erase_id)))
            BEGIN SELECT RAISE(ABORT, 'erased receipt is immutable'); END;
        CREATE TRIGGER IF NOT EXISTS agent_erase_receipts_immutable_delete
            BEFORE DELETE ON agent_erase_receipts WHEN OLD.state = 'erased'
            BEGIN SELECT RAISE(ABORT, 'erased receipt is immutable'); END;

        CREATE TABLE IF NOT EXISTS agent_erase_keys (
            workspace_id TEXT NOT NULL CHECK({w_ws}),
            owner_id TEXT NOT NULL CHECK({o_owner}),
            idempotency_key TEXT NOT NULL
                CHECK({b_idem} BETWEEN 16 AND 128 AND {n_idem} AND idempotency_key NOT GLOB '*[^ -~]*'),
            request_method TEXT NOT NULL CHECK({n_method}),
            canonical_path TEXT NOT NULL CHECK({n_path}),
            scope_id TEXT NOT NULL CHECK({u_scope}),
            request_hash TEXT NOT NULL CHECK({h_hash}),
            erase_id TEXT NOT NULL CHECK({u_erase}),
            created_at INTEGER NOT NULL,
            expires_at INTEGER,
            PRIMARY KEY (workspace_id, owner_id, idempotency_key),
            FOREIGN KEY (scope_id, workspace_id, owner_id)
                REFERENCES agent_scopes (scope_id, workspace_id, owner_id),
            FOREIGN KEY (workspace_id, owner_id, erase_id)
                REFERENCES agent_erase_receipts (workspace_id, owner_id, erase_id)
        ) STRICT;
        CREATE INDEX IF NOT EXISTS agent_erase_keys_expiry ON agent_erase_keys (expires_at);

        CREATE TABLE IF NOT EXISTS agent_erase_manifest (
            erase_id TEXT NOT NULL CHECK({u_erase}),
            store TEXT NOT NULL CHECK(store IN ('{stores}')),
            record_key TEXT NOT NULL CHECK({b_record} <= 1024 AND {n_record}),
            fts_rowid INTEGER CHECK(fts_rowid IS NULL OR fts_rowid > 0),
            PRIMARY KEY (erase_id, store, record_key)
        ) STRICT;

        CREATE TABLE IF NOT EXISTS agent_purge_progress (
            workspace_id TEXT NOT NULL CHECK({w_ws}),
            owner_id TEXT NOT NULL CHECK({o_owner}),
            agent_key TEXT NOT NULL CHECK({k_key} AND agent_key GLOB 'n:*'),
            incarnation TEXT NOT NULL CHECK({u_inc}),
            purge_id TEXT NOT NULL CHECK({u_purge}),
            state TEXT NOT NULL CHECK(state IN ('draining','scanning','verifying','erased')),
            after_scope_id TEXT CHECK({u_after}),
            current_erase_id TEXT CHECK({u_current}),
            PRIMARY KEY (workspace_id, owner_id, agent_key, incarnation),
            FOREIGN KEY (workspace_id, owner_id, agent_key, incarnation)
                REFERENCES agent_incarnations (workspace_id, owner_id, agent_key, incarnation)
        ) STRICT;",
        w_ws = w("workspace_id"),
        o_owner = o("owner_id"),
        o_session = o("session_key"),
        k_key = agent_key("agent_key"),
        u_inc = uuid("incarnation"),
        u_scope = uuid("scope_id"),
        u_erase = uuid("erase_id"),
        u_purge = uuid("purge_id"),
        u_after = nullable("after_scope_id", &uuid("after_scope_id")),
        u_current = nullable("current_erase_id", &uuid("current_erase_id")),
        h_rev = hex64("config_revision"),
        h_hash = hex64("request_hash"),
        b_idem = bytes("idempotency_key"),
        b_record = bytes("record_key"),
        n_idem = no_nul("idempotency_key"),
        n_record = no_nul("record_key"),
        n_method = no_nul("request_method"),
        n_path = no_nul("canonical_path"),
    )
}

pub(super) fn init_schema(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    conn.execute_batch(&schema_sql())?;
    Ok(())
}

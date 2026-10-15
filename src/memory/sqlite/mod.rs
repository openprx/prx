mod ingress_recovery;
mod router_history;
mod router_learning;
use super::embeddings::EmbeddingProvider;
use super::filter::{MemorySafetyFilter, SourceMetadata, safety_rejection_message};
use super::principal::{
    ChatType, MemoryWriteContext, OwnerPrincipal, Principal, Role, Visibility, classify_memory, log_access,
    post_filter, resolve_principal,
};
use super::topic::resolve_topic;
use super::traits::{
    ChatProfile, CompactionRun, CompactionRunInput, ConversationSessionSummary, ConversationTurn, DecisionMemoryHit,
    DocumentChunkRecord, DocumentIngestInput, DocumentRecord, DocumentSearchResult, Memory, MemoryCategory,
    MemoryDraft, MemoryDraftInput, MemoryEntry, MemoryEvent, MemoryEventInput, MemoryLink, MemoryLinkInput,
    MemoryPrincipal, MemoryReadMode, MemoryStoreMetadata, MemoryVisibility, MessageEvent, MessageEventInput,
    RetrievalTrace, RetrievalTraceInput, SessionContextQuery, SharedContextQuery, validate_memory_write_target,
};
use super::vector;
use crate::runtime::sqlite_pool::SqliteConnectionPool;
use crate::self_system::evolution::record::Actor;
use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Local, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter, types::Value};
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

const DEFAULT_CONVERSATION_LIMIT: usize = 50;
const MAX_CONVERSATION_QUERY_LIMIT: usize = 500;
/// Default ceiling on the embedded memories `vector_search` will score, used
/// when no configuration is supplied.
///
/// Cosine similarity cannot use an index, so the scan is bounded by insertion
/// order. Operators override this with `[memory] vector_candidate_cap`.
const VECTOR_CANDIDATE_CAP: usize = 2_000;

/// Candidate count for one vector recall.
///
/// The configured cap is authoritative — it is the number an operator tunes —
/// and is only ever raised to `limit`, because scoring fewer candidates than
/// the caller asked for results would return a short list for no reason.
/// Before 0.8.126 this was `max(limit * 50, 2000)` with no way to change it,
/// so a store larger than the cap lost recall with nothing to turn.
const fn vector_candidate_cap(limit: usize, configured: usize) -> usize {
    if limit > configured { limit } else { configured }
}

struct VectorCandidateQuery {
    sql: String,
    params: Vec<Box<dyn rusqlite::types::ToSql>>,
    candidate_cap: usize,
}

fn vector_candidate_query(
    provider: &str,
    model: &str,
    dimensions: usize,
    limit: usize,
    category: Option<&str>,
    session_id: Option<&str>,
    configured_candidate_cap: usize,
) -> VectorCandidateQuery {
    let mut sql = "SELECT id, embedding FROM memories
                   WHERE embedding IS NOT NULL
                     AND embedding_provider = ?1
                     AND embedding_model = ?2
                     AND embedding_dimensions = ?3"
        .to_string();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
        Box::new(provider.to_string()),
        Box::new(model.to_string()),
        Box::new(i64::try_from(dimensions).unwrap_or(i64::MAX)),
    ];
    let mut idx = 4;

    if let Some(category) = category {
        let _ = write!(sql, " AND category = ?{idx}");
        params.push(Box::new(category.to_string()));
        idx += 1;
    }
    if let Some(session_id) = session_id {
        let _ = write!(sql, " AND session_id = ?{idx}");
        params.push(Box::new(session_id.to_string()));
        idx += 1;
    }
    let candidate_cap = vector_candidate_cap(limit, configured_candidate_cap);
    let _ = write!(sql, " ORDER BY update_seq DESC, rowid DESC LIMIT ?{idx}");
    params.push(Box::new(i64::try_from(candidate_cap).unwrap_or(i64::MAX)));

    VectorCandidateQuery {
        sql,
        params,
        candidate_cap,
    }
}

/// One process says "your memory store has outgrown the candidate cap" once.
///
/// This is a warning, not a debug line: past this point vector recall is
/// silently incomplete — an old-but-relevant memory is never scored — and the
/// only fix is an operator raising `[memory] vector_candidate_cap`. Emitting it
/// per recall would bury the turn logs, so it is latched per process.
static VECTOR_CANDIDATE_CAP_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Placeholder dialect for D4 read-merge `session_key` predicate fragments.
const SQLITE_DIALECT: crate::memory::session_predicate::PlaceholderDialect =
    crate::memory::session_predicate::PlaceholderDialect::Sqlite;

/// The single construction of a read principal from a trusted write context.
///
/// Every scoped SQLite read and the context-carrying write path resolve their
/// principal here, so the C03 / V02-R5 owner anchor cannot be attached at one
/// site and forgotten at another.
fn sqlite_read_principal(conn: &Connection, context: &MemoryWriteContext) -> Principal {
    let fallback = Principal {
        user_id: "anonymous:unknown:unknown".to_string(),
        role: Role::Anonymous,
        projects: Vec::new(),
        visibility_ceiling: Visibility::Private,
        blocked_patterns: Vec::new(),
        current_channel: context.channel.clone().unwrap_or_default(),
        current_chat_id: context.chat_id.clone().unwrap_or_default(),
        current_chat_type: context
            .chat_type
            .as_deref()
            .map(ChatType::from_str)
            .unwrap_or(ChatType::Dm),
        raw_sender: context.raw_sender.clone().unwrap_or_default(),
        acl_enforced: true,
        owner_id: None,
        require_owner_match: false,
    };
    if context.channel.is_some() && context.raw_sender.is_some() {
        resolve_principal(conn, context).unwrap_or(fallback)
    } else {
        fallback
    }
    .with_owner_scope(context)
}

/// The backend's own unrestricted principal, used for compatibility reads that
/// deliberately bypass the ACL (Observe mode).
///
/// It carries no owner anchor and never requires an owner match: it must never
/// be handed to an owner-isolated caller, which is why every isolated read
/// forces Enforce mode before the choice is made.
fn sqlite_owner_read_principal() -> Principal {
    Principal {
        user_id: "system:memory_backend".to_string(),
        role: Role::Owner,
        projects: Vec::new(),
        visibility_ceiling: Visibility::Public,
        blocked_patterns: Vec::new(),
        current_channel: String::new(),
        current_chat_id: String::new(),
        current_chat_type: ChatType::Dm,
        raw_sender: String::new(),
        acl_enforced: false,
        owner_id: None,
        require_owner_match: false,
    }
}

pub struct SqliteTaskEventMirror<'a> {
    pub workspace_id: &'a str,
    pub task_id: &'a str,
    pub event_type: &'a str,
    pub session_key: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub persona_id: Option<&'a str>,
    pub payload_json: Option<&'a str>,
}

pub fn append_task_event_mirror(workspace_dir: &Path, input: SqliteTaskEventMirror<'_>) -> anyhow::Result<i64> {
    append_task_event_mirror_idempotent(workspace_dir, &Uuid::new_v4().to_string(), input)
}

/// Append a task event mirror with a caller-owned stable event id. Replaying
/// the same outbox row returns the existing mirror instead of duplicating it.
pub fn append_task_event_mirror_idempotent(
    workspace_dir: &Path,
    event_id: &str,
    input: SqliteTaskEventMirror<'_>,
) -> anyhow::Result<i64> {
    let db_path = workspace_dir.join("memory").join("brain.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create memory directory: {}", parent.display()))?;
    }
    let conn =
        Connection::open(&db_path).with_context(|| format!("Failed to open memory DB: {}", db_path.display()))?;
    // Same policy as the pooled connections onto this file: a lock held by
    // another process makes this call wait (and warn), never fail.
    crate::runtime::sqlite_pool::install_busy_handler(&conn)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_events (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id      TEXT NOT NULL UNIQUE,
            workspace_id  TEXT NOT NULL,
            owner_id      TEXT,
            event_type    TEXT NOT NULL,
            subject_table TEXT NOT NULL,
            subject_id    TEXT NOT NULL,
            session_key   TEXT,
            agent_id      TEXT,
            persona_id    TEXT,
            visibility    TEXT NOT NULL DEFAULT 'workspace',
            payload_json  TEXT,
            created_at    TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_memory_events_workspace_id
            ON memory_events(workspace_id, id);
        CREATE INDEX IF NOT EXISTS idx_memory_events_type
            ON memory_events(workspace_id, event_type, id);
        CREATE INDEX IF NOT EXISTS idx_memory_events_session
            ON memory_events(workspace_id, session_key, id);",
    )?;

    conn.execute(
        "INSERT OR IGNORE INTO memory_events (
            event_id, workspace_id, event_type, subject_table, subject_id,
            session_key, agent_id, persona_id, visibility, payload_json, created_at
         )
         VALUES (?1, ?2, ?3, 'tasks', ?4, ?5, ?6, ?7, 'workspace', ?8, ?9)",
        params![
            event_id,
            input.workspace_id,
            input.event_type,
            input.task_id,
            input.session_key,
            input.agent_id,
            input.persona_id,
            input.payload_json,
            Utc::now().to_rfc3339(),
        ],
    )?;
    conn.query_row(
        "SELECT id FROM memory_events WHERE event_id = ?1",
        params![event_id],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

pub fn init_approval_grant_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS approval_grants (
            grant_id                 TEXT PRIMARY KEY,
            version                  INTEGER NOT NULL,
            owner_id                 TEXT NOT NULL,
            principal_id             TEXT NOT NULL,
            workspace_id             TEXT NOT NULL,
            agent_id                 TEXT NOT NULL,
            session_key              TEXT,
            issuer_authority         TEXT NOT NULL,
            issuer_authority_id      TEXT NOT NULL,
            issuer_public_key_id     TEXT NOT NULL,
            capability_op_id         TEXT NOT NULL,
            capability_op_id_match   TEXT NOT NULL,
            capability_risk_level    TEXT NOT NULL CHECK (capability_risk_level IN ('low','medium','high','critical')),
            resource_constraints_json TEXT NOT NULL DEFAULT '{}',
            grant_json               TEXT NOT NULL,
            signature_alg            TEXT NOT NULL,
            signed_payload_sha256    TEXT NOT NULL,
            issued_at                TEXT NOT NULL,
            not_before               TEXT NOT NULL,
            expires_at               TEXT NOT NULL,
            max_uses                 INTEGER NOT NULL,
            uses_consumed            INTEGER NOT NULL DEFAULT 0,
            related_task_id          TEXT,
            related_message_event_id INTEGER,
            revoked_at               TEXT,
            revocation_reason        TEXT,
            created_at               TEXT NOT NULL,
            updated_at               TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_approval_grants_owner
            ON approval_grants(workspace_id, owner_id, issued_at DESC);
        CREATE INDEX IF NOT EXISTS idx_approval_grants_principal
            ON approval_grants(workspace_id, principal_id, issued_at DESC);
        CREATE INDEX IF NOT EXISTS idx_approval_grants_capability
            ON approval_grants(workspace_id, capability_op_id, expires_at);
        CREATE INDEX IF NOT EXISTS idx_approval_grants_active
            ON approval_grants(workspace_id, expires_at, revoked_at);

        CREATE TABLE IF NOT EXISTS approval_grant_events (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id      TEXT NOT NULL UNIQUE,
            grant_id      TEXT NOT NULL,
            event_type    TEXT NOT NULL CHECK (event_type IN (
                'grant.issued','grant.verified','grant.consumed','grant.revoked',
                'grant.rejected','grant.expired'
            )),
            actor         TEXT NOT NULL,
            occurred_at   TEXT NOT NULL,
            payload_json  TEXT,
            FOREIGN KEY (grant_id) REFERENCES approval_grants(grant_id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_approval_grant_events_grant
            ON approval_grant_events(grant_id, occurred_at);
        CREATE INDEX IF NOT EXISTS idx_approval_grant_events_type
            ON approval_grant_events(event_type, occurred_at);

        CREATE TABLE IF NOT EXISTS approval_grant_revocations (
            grant_id     TEXT PRIMARY KEY,
            revoked_at   TEXT NOT NULL,
            reason       TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_approval_grant_revocations_revoked_at
            ON approval_grant_revocations(revoked_at);",
    )?;
    Ok(())
}

/// FIX-P3-06: record an out-of-band revocation for `grant_id`.
///
/// Idempotent: a second revocation of the same grant overwrites the previous
/// `revoked_at` / `reason` rather than failing on the primary-key conflict, so
/// re-revoking is always safe. The signed grant payload is never mutated; the
/// revocation lives only in this side-table and is consulted by
/// [`is_approval_grant_revoked`].
pub fn revoke_approval_grant(
    conn: &Connection,
    grant_id: &str,
    revoked_at: &str,
    reason: Option<&str>,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO approval_grant_revocations (grant_id, revoked_at, reason) \
         VALUES (?1, ?2, ?3) \
         ON CONFLICT(grant_id) DO UPDATE SET revoked_at = excluded.revoked_at, reason = excluded.reason",
        params![grant_id, revoked_at, reason],
    )?;
    Ok(())
}

/// FIX-P3-06: report whether `grant_id` has an out-of-band revocation row.
///
/// The gate path (to be wired up under separate coordination — the gate
/// verification currently lives outside this module's boundary) MUST treat a
/// `true` result as fail-closed and deny the operation, in addition to the
/// in-grant `revoked_at` check performed by
/// [`crate::acl::approval_grant::ApprovalGrantV2::verify_for_operation`].
pub fn is_approval_grant_revoked(conn: &Connection, grant_id: &str) -> anyhow::Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM approval_grant_revocations WHERE grant_id = ?1",
            params![grant_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}
const MAX_HYDRATED_SESSIONS: usize = 100;
const SESSION_PREVIEW_CHARS: usize = 120;

/// Connection-scoped settings replayed on every pooled reader.
///
/// PRAGMAs are per-connection state, so a pooled reader is useless unless it is
/// configured exactly like the writer. `query_only` is not listed here: the
/// pool always adds it, and it is the safety net for this module's read/write
/// split — a statement that mutates the database fails loudly on a reader
/// instead of silently competing with the writer for SQLite's write lock.
const SQLITE_READER_PRAGMAS: &str = "PRAGMA foreign_keys  = ON;
     PRAGMA mmap_size     = 8388608;
     PRAGMA cache_size    = -2000;
     PRAGMA temp_store    = MEMORY;";

/// Production-grade PRAGMA tuning for the writer.
///
/// `journal_mode = WAL` is absent on purpose: the pool sets it itself, because
/// pooled readers are unsound without it. The rest are per-connection — normal
/// sync (2x write speed, still durable on WAL), mmap 8 MB (let the OS page
/// cache serve hot reads), cache 2 MB (~500 hot pages in-process), and temp
/// tables that never hit disk.
const SQLITE_WRITER_PRAGMAS: &str = "PRAGMA synchronous   = NORMAL;
     PRAGMA foreign_keys  = ON;
     PRAGMA mmap_size     = 8388608;
     PRAGMA cache_size    = -2000;
     PRAGMA temp_store    = MEMORY;";

/// Reader-pool and writer-contention counters for this backend.
///
/// Re-exported from the shared pool so callers keep reaching it at
/// `memory::sqlite::SqlitePoolStats`.
pub use crate::runtime::sqlite_pool::SqlitePoolStats;

/// SQLite-backed persistent memory — the brain
///
/// Full-stack search engine:
/// - **Vector DB**: embeddings stored as BLOB, cosine similarity search
/// - **Keyword Search**: FTS5 virtual table with BM25 scoring
/// - **Hybrid Merge**: weighted fusion of vector + keyword results
/// - **Embedding Cache**: LRU-evicted cache to avoid redundant API calls
/// - **Safe Reindex**: temp DB → seed → sync → atomic swap → rollback
pub struct SqliteMemory {
    pool: Arc<SqliteConnectionPool>,
    ingress_owners: super::ingress_recovery::IngressOwnerCache,
    db_path: PathBuf,
    acl_enabled: bool,
    embedder: Arc<dyn EmbeddingProvider>,
    vector_weight: f32,
    keyword_weight: f32,
    cache_max: usize,
    /// Ceiling on the candidates one vector recall scores; see
    /// `[memory] vector_candidate_cap`.
    vector_candidate_cap: usize,
}

impl SqliteMemory {
    async fn list_conversation_turns_ordered(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
        recorded_order: bool,
    ) -> anyhow::Result<Vec<ConversationTurn>> {
        let pool = self.pool.clone();
        let ordering = if recorded_order {
            "julianday(timestamp) DESC, COALESCE((SELECT event.id FROM message_events event WHERE event.event_id = conversation_turns.message_event_id), id) DESC, id DESC"
        } else {
            "id DESC"
        };
        let owner_id = principal
            .owner_id
            .as_deref()
            .filter(|owner| !owner.trim().is_empty())
            .map(str::to_string);
        let session_key = session_key.to_string();
        let limit = Self::sanitize_conversation_limit(limit);
        let offset = Self::sanitize_conversation_offset(offset);
        let legacy_visible = 1_i64;
        // D4 read-merge: the explicit `session_key` arg is the canonical key
        // (bound at `?1`); the principal may carry a distinct legacy key bound
        // at trailing placeholders starting at `?6`. With no legacy key the
        // predicate degrades to the byte-identical `session_key = ?1`.
        let legacy_session_keys: Vec<String> = principal
            .legacy_session_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty() && *key != session_key.trim())
            .map(|key| vec![key.to_string()])
            .unwrap_or_default();
        let mut session_indices = vec![1usize];
        for offset_idx in 0..legacy_session_keys.len() {
            session_indices.push(6 + offset_idx);
        }
        let session_fragment =
            crate::memory::session_predicate::session_key_match_fragment(SQLITE_DIALECT, &session_indices);

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<ConversationTurn>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(&format!(
                "SELECT id, session_key, role, content, timestamp, message_id
                 FROM conversation_turns
                 WHERE {session_fragment}
                   AND (
                       ?4 = 'system:*'
                       OR
                       owner_id = ?4
                       OR (?5 = 1 AND (owner_id IS NULL OR owner_id = 'legacy:' || session_key))
                   )
                 ORDER BY {ordering}
                 LIMIT ?2 OFFSET ?3",
                session_fragment = session_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> =
                vec![&session_key, &limit, &offset, &owner_id, &legacy_visible];
            for key in &legacy_session_keys {
                bind.push(key);
            }
            let rows = stmt.query_map(bind.as_slice(), |row| {
                Ok(ConversationTurn {
                    id: row.get(0)?,
                    session_key: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                    timestamp: row.get(4)?,
                    message_id: row.get(5)?,
                })
            })?;

            let mut turns = Vec::new();
            for row in rows {
                turns.push(row?);
            }
            turns.reverse();
            Ok(turns)
        })
        .await?
    }

    fn open_rehearsal_reader(db_path: &Path, deadline: std::time::Instant) -> anyhow::Result<Connection> {
        anyhow::ensure!(std::time::Instant::now() < deadline, "rehearsal count deadline expired");
        // macOS may expose a trusted temporary parent through `/var` ->
        // `/private/var`. Resolve the parent while refusing a symlink at the
        // database leaf; a replaced regular file remains a separate threat.
        let parent = db_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("missing decision database parent"))?;
        let file_name = db_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("missing decision database name"))?;
        let read_path = parent.canonicalize()?.join(file_name);
        anyhow::ensure!(std::time::Instant::now() < deadline, "rehearsal count deadline expired");
        let conn = Connection::open_with_flags(
            read_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        conn.busy_timeout(std::time::Duration::from_millis(25))?;
        conn.progress_handler(1_000, Some(move || std::time::Instant::now() >= deadline))?;
        Ok(conn)
    }

    fn chat_profile_from_row(row: &Row<'_>) -> rusqlite::Result<ChatProfile> {
        let tags_json: String = row.get(7)?;
        let tags = serde_json::from_str::<Vec<String>>(&tags_json).unwrap_or_default();
        Ok(ChatProfile {
            id: row.get(0)?,
            channel: row.get(1)?,
            chat_id: row.get(2)?,
            chat_kind: row.get(3)?,
            title: row.get(4)?,
            purpose: row.get(5)?,
            notes: row.get(6)?,
            tags,
            updated_by: row.get(8)?,
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
        })
    }

    pub fn new(workspace_dir: &Path) -> anyhow::Result<Self> {
        Self::with_embedder_with_acl(
            workspace_dir,
            Arc::new(super::embeddings::NoopEmbedding),
            0.7,
            0.3,
            10_000,
            None,
            false,
            None,
        )
    }

    /// Build SQLite memory using an explicit database file path.
    ///
    /// Uses the noop embedder and default weights/cache values, matching `new`.
    pub fn new_with_path(db_path: PathBuf) -> anyhow::Result<Self> {
        Self::with_embedder_and_path_with_acl(
            db_path,
            Arc::new(super::embeddings::NoopEmbedding),
            0.7,
            0.3,
            10_000,
            None,
            false,
            None,
        )
    }

    /// Build SQLite memory using an explicit database file path and ACL mode.
    pub fn new_with_path_and_acl(db_path: PathBuf, acl_enabled: bool) -> anyhow::Result<Self> {
        Self::with_embedder_and_path_with_acl(
            db_path,
            Arc::new(super::embeddings::NoopEmbedding),
            0.7,
            0.3,
            10_000,
            None,
            acl_enabled,
            None,
        )
    }

    /// Inspect one key namespace through a byte-for-byte read-only connection.
    /// This is used by migration dry-runs that must not initialize or mutate
    /// the configured database.
    pub async fn read_key_prefix_read_only(
        workspace_dir: &Path,
        prefix: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let db_path = workspace_dir.join("memory/brain.db");
        if !db_path.exists() {
            return Ok(Vec::new());
        }
        let prefix = prefix.to_string();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<(String, String)>> {
            let connection = Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("failed to open {} read-only", db_path.display()))?;
            let escaped = prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            let pattern = format!("{escaped}%");
            let mut statement =
                connection.prepare("SELECT key, content FROM memories WHERE key LIKE ?1 ESCAPE '\\'")?;
            let rows = statement.query_map([pattern], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await?
    }

    /// Create a consistent SQLite backup without stopping readers or copying a
    /// live WAL file set byte-for-byte.
    pub async fn backup_database_to(&self, target: &Path) -> anyhow::Result<()> {
        let pool = self.pool.clone();
        let target = target.to_path_buf();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            anyhow::ensure!(!target.exists(), "backup target already exists: {}", target.display());
            let conn = pool.write();
            conn.execute("VACUUM INTO ?1", params![target.to_string_lossy().to_string()])?;
            Ok(())
        })
        .await?
    }

    /// Delete an exact key namespace in one transaction. SQLite FTS triggers
    /// remove the corresponding search rows in the same commit.
    pub async fn delete_key_prefix_transactional(&self, prefix: &str) -> anyhow::Result<usize> {
        let pool = self.pool.clone();
        let escaped = prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("{escaped}%");
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<usize> {
            let mut conn = pool.write();
            let tx = conn.transaction()?;
            let affected = tx.execute("DELETE FROM memories WHERE key LIKE ?1 ESCAPE '\\'", params![pattern])?;
            tx.commit()?;
            Ok(affected)
        })
        .await?
    }

    /// Build SQLite memory with optional open timeout.
    ///
    /// If `open_timeout_secs` is `Some(n)`, opening the database is limited to `n` seconds
    /// (capped at 300). Useful when the DB file may be locked or on slow storage.
    /// `None` = wait indefinitely (default).
    pub fn with_embedder(
        workspace_dir: &Path,
        embedder: Arc<dyn EmbeddingProvider>,
        vector_weight: f32,
        keyword_weight: f32,
        cache_max: usize,
        open_timeout_secs: Option<u64>,
    ) -> anyhow::Result<Self> {
        Self::with_embedder_with_acl(
            workspace_dir,
            embedder,
            vector_weight,
            keyword_weight,
            cache_max,
            open_timeout_secs,
            false,
            None,
        )
    }

    /// Build SQLite memory with optional open timeout, ACL mode and reader-pool size.
    ///
    /// `read_pool_size` bounds how many reader connections may exist, not how
    /// much work may run: a caller that finds every reader busy queues for one.
    /// `None` derives it from the available CPUs — see
    /// [`crate::runtime::sqlite_pool::default_read_pool_size`].
    #[allow(clippy::too_many_arguments)]
    pub fn with_embedder_with_acl(
        workspace_dir: &Path,
        embedder: Arc<dyn EmbeddingProvider>,
        vector_weight: f32,
        keyword_weight: f32,
        cache_max: usize,
        open_timeout_secs: Option<u64>,
        acl_enabled: bool,
        read_pool_size: Option<usize>,
    ) -> anyhow::Result<Self> {
        let db_path = workspace_dir.join("memory").join("brain.db");
        Self::with_embedder_and_path_with_acl(
            db_path,
            embedder,
            vector_weight,
            keyword_weight,
            cache_max,
            open_timeout_secs,
            acl_enabled,
            read_pool_size,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_embedder_and_path_with_acl(
        db_path: PathBuf,
        embedder: Arc<dyn EmbeddingProvider>,
        vector_weight: f32,
        keyword_weight: f32,
        cache_max: usize,
        open_timeout_secs: Option<u64>,
        acl_enabled: bool,
        read_pool_size: Option<usize>,
    ) -> anyhow::Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // `reset_transactions_on_release` is on because this module issues bare
        // `BEGIN` through `execute_batch` in places: `rusqlite::Transaction`
        // would clean up after itself, a bare `BEGIN` would not, and a leftover
        // transaction stalls every later write (writer) or pins a stale WAL
        // snapshot for every later caller (reader).
        let pool = SqliteConnectionPool::builder(db_path.clone())
            .max_readers(read_pool_size.unwrap_or_else(crate::runtime::sqlite_pool::default_read_pool_size))
            .open_timeout_secs(open_timeout_secs)
            .writer_pragmas(SQLITE_WRITER_PRAGMAS)
            .reader_pragmas(SQLITE_READER_PRAGMAS)
            .reset_transactions_on_release(true)
            .build()?;

        Self::init_schema(&pool.write())?;

        let pool = Arc::new(pool);
        SqliteConnectionPool::publish_metrics(&pool);

        Ok(Self {
            pool,
            ingress_owners: Default::default(),
            db_path,
            acl_enabled,
            embedder,
            vector_weight,
            keyword_weight,
            cache_max,
            vector_candidate_cap: VECTOR_CANDIDATE_CAP,
        })
    }

    /// Override how many embedded memories one vector recall may score.
    ///
    /// Wired from `[memory] vector_candidate_cap`. A cap of zero would score
    /// nothing at all, so it is clamped back to the shipped floor rather than
    /// silently disabling vector recall.
    #[must_use]
    pub const fn with_vector_candidate_cap(mut self, cap: usize) -> Self {
        self.vector_candidate_cap = if cap == 0 { VECTOR_CANDIDATE_CAP } else { cap };
        self
    }

    /// Snapshot of reader-pool and writer contention counters.
    ///
    /// Nothing here throttles: the numbers exist so saturation is observable
    /// rather than enforced.
    pub fn pool_stats(&self) -> SqlitePoolStats {
        self.pool.stats()
    }

    fn sanitize_conversation_limit(limit: usize) -> i64 {
        #[allow(clippy::cast_possible_wrap)]
        let normalized = if limit == 0 {
            DEFAULT_CONVERSATION_LIMIT
        } else {
            limit.min(MAX_CONVERSATION_QUERY_LIMIT)
        };
        normalized as i64
    }

    fn sanitize_conversation_offset(offset: usize) -> i64 {
        #[allow(clippy::cast_possible_wrap)]
        {
            offset.min(i64::MAX as usize) as i64
        }
    }

    fn sanitize_hydrated_sessions_limit(limit: usize) -> i64 {
        #[allow(clippy::cast_possible_wrap)]
        {
            let normalized = if limit == 0 {
                MAX_HYDRATED_SESSIONS
            } else {
                limit.min(MAX_HYDRATED_SESSIONS)
            };
            normalized as i64
        }
    }

    fn normalize_conversation_timestamp(timestamp: Option<&str>) -> String {
        if let Some(value) = timestamp.map(str::trim).filter(|value| !value.is_empty()) {
            if DateTime::parse_from_rfc3339(value).is_ok() {
                return value.to_string();
            }
        }
        Utc::now().to_rfc3339()
    }

    fn conversation_preview(content: &str) -> String {
        let preview: String = content.chars().take(SESSION_PREVIEW_CHARS).collect();
        if content.chars().count() > SESSION_PREVIEW_CHARS {
            format!("{preview}...")
        } else {
            preview
        }
    }

    fn is_system_principal(principal: &MemoryPrincipal) -> bool {
        principal
            .agent_id
            .as_deref()
            .is_some_and(super::principal::is_system_principal)
            || principal
                .persona_id
                .as_deref()
                .is_some_and(super::principal::is_system_principal)
    }

    /// D4 read-merge: legacy `session_key` candidate(s) that bind to *new
    /// trailing* SQL placeholders, in addition to the canonical key which keeps
    /// its original placeholder. Returns the deduplicated tail of
    /// [`MemoryPrincipal::session_key_candidates`] (everything after the
    /// canonical key). Empty when there is no distinct legacy key, in which case
    /// the predicate degrades to the historical single-key form.
    fn legacy_session_key_params(principal: &MemoryPrincipal) -> Vec<String> {
        let mut candidates = principal.session_key_candidates();
        if candidates.is_empty() {
            candidates
        } else {
            candidates.split_off(1)
        }
    }

    /// C03 owner isolation: 1-based placeholder index for the owner parameter.
    ///
    /// The owner id always binds *after* the D4 legacy session keys, so the
    /// index is the first free trailing slot once those are accounted for.
    /// `legacy_start` is the same first-trailing-placeholder index the legacy
    /// keys were laid out from.
    const fn owner_index(legacy_start: usize, legacy_key_count: usize) -> usize {
        legacy_start + legacy_key_count
    }

    /// D4 read-merge: placeholder indices for the `session_key` predicate.
    ///
    /// `canonical_index` is the existing placeholder bound to
    /// `principal.session_key`; `legacy_start` is the first *new trailing*
    /// placeholder index. With no distinct legacy key the result is
    /// `[canonical_index]` (byte-identical single-key predicate); otherwise the
    /// canonical index is followed by consecutive trailing indices for each
    /// legacy key. When the principal has no `session_key` at all the result is
    /// empty (predicate becomes `FALSE`, matching nothing, as before).
    fn session_indices(
        canonical_index: usize,
        legacy_start: usize,
        principal: &MemoryPrincipal,
        legacy_keys: &[String],
    ) -> Vec<usize> {
        if principal
            .session_key
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            return Vec::new();
        }
        let mut indices = Vec::with_capacity(1 + legacy_keys.len());
        indices.push(canonical_index);
        for offset in 0..legacy_keys.len() {
            indices.push(legacy_start + offset);
        }
        indices
    }

    /// Initialize all tables: memories, FTS5, `embedding_cache`
    fn init_schema(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            "-- Core memories table
            CREATE TABLE IF NOT EXISTS memories (
                id          TEXT PRIMARY KEY,
                key         TEXT NOT NULL UNIQUE,
                content     TEXT NOT NULL,
                category    TEXT NOT NULL DEFAULT 'core',
                embedding   BLOB,
                embedding_provider TEXT,
                embedding_model TEXT,
                embedding_dimensions INTEGER,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL,
                workspace_id TEXT,
                owner_id    TEXT,
                agent_id     TEXT,
                persona_id   TEXT,
                source_event_id TEXT,
                source       TEXT,
                channel      TEXT,
                chat_type    TEXT,
                chat_id      TEXT,
                sender_id    TEXT,
                raw_sender   TEXT,
                topic_id     TEXT,
                -- FIX-P2-04: unify the `memories.visibility` default with Postgres
                -- (`'workspace'`). Inserts always set visibility explicitly, so this
                -- only affects rows created without an explicit value.
                visibility   TEXT NOT NULL DEFAULT 'workspace',
                sensitivity  TEXT NOT NULL DEFAULT 'normal',
                risk_signals TEXT DEFAULT '[]',
                policy_version INTEGER DEFAULT 1,
                useful_count INTEGER NOT NULL DEFAULT 0,
                -- Monotonic per-store write counter, maintained by the
                -- `memories_update_seq_*` triggers created after the column
                -- migrations below. It is the only ordering key vector recall
                -- trusts: `updated_at` is a local-offset RFC 3339 string, and
                -- `rowid` never moves when an existing key is refreshed.
                update_seq INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_memories_category ON memories(category);
            CREATE INDEX IF NOT EXISTS idx_memories_key ON memories(key);
            -- The vector-recall candidate index and the update-sequence
            -- triggers are created after the column migrations below, because
            -- the columns they span are added by ALTER TABLE on stores that
            -- predate them.

            -- FTS5 full-text search (BM25 scoring)
            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                key, content, content=memories, content_rowid=rowid
            );

            -- FTS5 triggers: keep in sync with memories table
            CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts(rowid, key, content)
                VALUES (new.rowid, new.key, new.content);
            END;
            CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, key, content)
                VALUES ('delete', old.rowid, old.key, old.content);
            END;
            -- Superseded by `memories_au_fts`: it rewrote the FTS index for
            -- every UPDATE, including updates that touch no indexed column.
            -- That is wasted work for a metadata write, and it is a hard error
            -- for an update issued from inside another trigger on this table
            -- (`memories_update_seq_ai` below): the FTS 'delete' command then
            -- runs against a row whose index entry the insert trigger may not
            -- have written yet, which FTS5 reports as a corrupt external
            -- content table. Renaming rather than redefining keeps
            -- `CREATE TRIGGER IF NOT EXISTS` a no-op on later opens.
            DROP TRIGGER IF EXISTS memories_au;
            CREATE TRIGGER IF NOT EXISTS memories_au_fts AFTER UPDATE ON memories
            WHEN new.key IS NOT old.key OR new.content IS NOT old.content
            BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, key, content)
                VALUES ('delete', old.rowid, old.key, old.content);
                INSERT INTO memories_fts(rowid, key, content)
                VALUES (new.rowid, new.key, new.content);
            END;

            -- Embedding cache with LRU eviction.
            -- FIX-P0-26: the cache key is the composite (content_hash, provider,
            -- model, dimensions) so the same content cached under different
            -- providers/models/dimensions does not collide (parity with Postgres).
            CREATE TABLE IF NOT EXISTS embedding_cache (
                content_hash TEXT NOT NULL,
                embedding    BLOB NOT NULL,
                provider     TEXT NOT NULL DEFAULT '',
                model        TEXT NOT NULL DEFAULT '',
                dimensions   INTEGER NOT NULL DEFAULT 0,
                created_at   TEXT NOT NULL,
                accessed_at  TEXT NOT NULL,
                PRIMARY KEY (content_hash, provider, model, dimensions)
            );
            CREATE INDEX IF NOT EXISTS idx_cache_accessed ON embedding_cache(accessed_at);",
        )?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS identity_bindings (
                id              TEXT PRIMARY KEY,
                user_id         TEXT NOT NULL,
                channel         TEXT NOT NULL,
                channel_account TEXT NOT NULL,
                display_name    TEXT,
                bound_at        TEXT NOT NULL,
                bound_by        TEXT NOT NULL,
                UNIQUE(channel, channel_account)
            );
            CREATE INDEX IF NOT EXISTS idx_ib_user ON identity_bindings(user_id);
            CREATE INDEX IF NOT EXISTS idx_ib_channel_account ON identity_bindings(channel, channel_account);

            CREATE TABLE IF NOT EXISTS chat_profiles (
                id          TEXT PRIMARY KEY,
                channel     TEXT NOT NULL,
                chat_id     TEXT NOT NULL,
                chat_kind   TEXT NOT NULL,
                title       TEXT,
                purpose     TEXT,
                notes       TEXT,
                tags        TEXT NOT NULL DEFAULT '[]',
                updated_by  TEXT NOT NULL,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL,
                UNIQUE(channel, chat_id)
            );
            CREATE INDEX IF NOT EXISTS idx_chat_profiles_lookup ON chat_profiles(channel, chat_id);

            CREATE TABLE IF NOT EXISTS agent_identity_bindings (
                binding_id        TEXT PRIMARY KEY,
                external_subject  TEXT NOT NULL,
                external_issuer   TEXT NOT NULL,
                auth_method       TEXT NOT NULL,
                prx_owner_id      TEXT NOT NULL,
                prx_principal_id  TEXT NOT NULL,
                capabilities      TEXT NOT NULL,
                expires_at        TEXT,
                created_at        TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                last_used_at      TEXT,
                UNIQUE (external_issuer, external_subject, auth_method)
            );
            CREATE INDEX IF NOT EXISTS idx_agent_bindings_lookup
                ON agent_identity_bindings(external_issuer, external_subject);
            CREATE INDEX IF NOT EXISTS idx_agent_bindings_owner
                ON agent_identity_bindings(prx_owner_id);

            CREATE TABLE IF NOT EXISTS user_policies (
                user_id             TEXT PRIMARY KEY,
                role                TEXT NOT NULL DEFAULT 'guest',
                projects            TEXT NOT NULL DEFAULT '[]',
                visibility_ceiling  TEXT NOT NULL DEFAULT 'private',
                blocked_patterns    TEXT NOT NULL DEFAULT '[]',
                policy_version      INTEGER NOT NULL DEFAULT 1,
                updated_at          TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS topics (
                id              TEXT PRIMARY KEY,
                title           TEXT NOT NULL,
                project         TEXT,
                external_id     TEXT,
                external_url    TEXT,
                fingerprint     TEXT,
                status          TEXT NOT NULL DEFAULT 'open',
                tags            TEXT DEFAULT '[]',
                summary         TEXT,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL,
                resolved_at     TEXT,
                UNIQUE(project, external_id),
                UNIQUE(fingerprint)
            );
            CREATE INDEX IF NOT EXISTS idx_topic_project ON topics(project);
            CREATE INDEX IF NOT EXISTS idx_topic_status ON topics(status);
            CREATE INDEX IF NOT EXISTS idx_topic_external ON topics(external_id);

            CREATE VIRTUAL TABLE IF NOT EXISTS topics_fts
                USING fts5(title, summary, tags, content='topics', content_rowid='rowid');

            CREATE TRIGGER IF NOT EXISTS topics_ai AFTER INSERT ON topics BEGIN
                INSERT INTO topics_fts(rowid, title, summary, tags)
                VALUES (new.rowid, new.title, new.summary, new.tags);
            END;

            CREATE TRIGGER IF NOT EXISTS topics_ad AFTER DELETE ON topics BEGIN
                INSERT INTO topics_fts(topics_fts, rowid, title, summary, tags)
                VALUES ('delete', old.rowid, old.title, old.summary, old.tags);
            END;

            CREATE TRIGGER IF NOT EXISTS topics_au AFTER UPDATE ON topics BEGIN
                INSERT INTO topics_fts(topics_fts, rowid, title, summary, tags)
                VALUES ('delete', old.rowid, old.title, old.summary, old.tags);
                INSERT INTO topics_fts(rowid, title, summary, tags)
                VALUES (new.rowid, new.title, new.summary, new.tags);
            END;

            CREATE TABLE IF NOT EXISTS topic_participants (
                topic_id    TEXT NOT NULL,
                user_id     TEXT NOT NULL,
                role        TEXT NOT NULL DEFAULT 'participant',
                joined_at   TEXT NOT NULL,
                PRIMARY KEY (topic_id, user_id),
                FOREIGN KEY (topic_id) REFERENCES topics(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS topic_aliases (
                from_topic_id TEXT NOT NULL,
                to_topic_id   TEXT NOT NULL,
                reason        TEXT,
                operator      TEXT NOT NULL,
                created_at    TEXT NOT NULL,
                PRIMARY KEY (from_topic_id),
                FOREIGN KEY (from_topic_id) REFERENCES topics(id) ON DELETE CASCADE,
                FOREIGN KEY (to_topic_id) REFERENCES topics(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS access_audit_log (
                id          TEXT PRIMARY KEY,
                timestamp   TEXT NOT NULL,
                requester   TEXT NOT NULL,
                action      TEXT NOT NULL,
                query       TEXT,
                memory_id   TEXT,
                policy_rule TEXT,
                result      TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_audit_time ON access_audit_log(timestamp DESC);
            CREATE INDEX IF NOT EXISTS idx_audit_requester ON access_audit_log(requester);

            CREATE TABLE IF NOT EXISTS sessions (
                session_key          TEXT PRIMARY KEY,
                channel              TEXT NOT NULL,
                sender               TEXT NOT NULL,
                owner_id             TEXT,
                created_at           TEXT NOT NULL,
                updated_at           TEXT NOT NULL,
                message_count        INTEGER NOT NULL DEFAULT 0,
                last_message_preview TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_updated_at ON sessions(updated_at DESC);
            CREATE INDEX IF NOT EXISTS idx_sessions_channel_updated_at ON sessions(channel, updated_at DESC);

            CREATE TABLE IF NOT EXISTS conversation_turns (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                session_key      TEXT NOT NULL,
                owner_id         TEXT,
                role             TEXT NOT NULL,
                content          TEXT NOT NULL,
                timestamp        TEXT NOT NULL,
                message_id       TEXT,
                message_event_id TEXT,
                agent_id         TEXT,
                persona_id       TEXT,
                visibility       TEXT,
                FOREIGN KEY (session_key) REFERENCES sessions(session_key) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_conversation_turns_session_key ON conversation_turns(session_key);
            CREATE INDEX IF NOT EXISTS idx_conversation_turns_timestamp ON conversation_turns(timestamp DESC);

            CREATE TABLE IF NOT EXISTS message_events (
                id                 INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id           TEXT NOT NULL UNIQUE,
                idempotency_key    TEXT,
                workspace_id       TEXT NOT NULL,
                owner_id           TEXT,
                source             TEXT NOT NULL,
                channel            TEXT,
                session_key        TEXT,
                parent_session_key TEXT,
                run_id             TEXT,
                parent_run_id      TEXT,
                agent_id           TEXT,
                persona_id         TEXT,
                sender             TEXT,
                recipient          TEXT,
                role               TEXT NOT NULL,
                event_type         TEXT NOT NULL,
                source_ref_json    TEXT,
                subject_ref_json   TEXT,
                goal_id            TEXT,
                causation_event_id TEXT,
                correlation_id     TEXT,
                attempt_id         TEXT,
                lease_epoch        INTEGER,
                config_generation_id INTEGER,
                config_source_revision TEXT,
                content            TEXT NOT NULL,
                content_hash       TEXT,
                raw_payload_json   TEXT,
                visibility         TEXT NOT NULL DEFAULT 'workspace',
                created_at         TEXT NOT NULL,
                updated_at         TEXT NOT NULL,
                UNIQUE (workspace_id, idempotency_key)
            );
            CREATE INDEX IF NOT EXISTS idx_message_events_workspace_id
                ON message_events(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_session
                ON message_events(workspace_id, session_key, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_agent
                ON message_events(workspace_id, agent_id, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_channel_sender
                ON message_events(workspace_id, channel, sender, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_visibility
                ON message_events(workspace_id, visibility, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_created_at
                ON message_events(created_at);

            CREATE TABLE IF NOT EXISTS memory_events (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id      TEXT NOT NULL UNIQUE,
                workspace_id  TEXT NOT NULL,
                owner_id      TEXT,
                event_type    TEXT NOT NULL,
                subject_table TEXT NOT NULL,
                subject_id    TEXT NOT NULL,
                session_key   TEXT,
                run_id        TEXT,
                parent_run_id TEXT,
                agent_id      TEXT,
                persona_id    TEXT,
                visibility    TEXT NOT NULL DEFAULT 'workspace',
                payload_json  TEXT,
                created_at    TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_memory_events_workspace_id
                ON memory_events(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_events_type
                ON memory_events(workspace_id, event_type, id);
            CREATE INDEX IF NOT EXISTS idx_memory_events_session
                ON memory_events(workspace_id, session_key, id);
            -- NOTE: idx_memory_events_run / idx_memory_events_parent_run are created
            -- later (after the run_id/parent_run_id ALTER backfill below), because a
            -- legacy `memory_events` table predating schema v7 lacks those columns and
            -- a `CREATE INDEX ... ON memory_events(run_id)` here would fail with
            -- `no such column: run_id` inside this single batch.

            CREATE TABLE IF NOT EXISTS memory_trash (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                trash_id      TEXT NOT NULL UNIQUE,
                memory_key    TEXT NOT NULL,
                content       TEXT NOT NULL,
                category      TEXT NOT NULL,
                reason        TEXT NOT NULL,
                trashed_at    TEXT NOT NULL,
                grace_until   TEXT NOT NULL,
                restored_at   TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_memory_trash_key
                ON memory_trash(memory_key, id);
            CREATE INDEX IF NOT EXISTS idx_memory_trash_grace
                ON memory_trash(grace_until) WHERE restored_at IS NULL;

            CREATE TABLE IF NOT EXISTS memory_drafts (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                draft_id        TEXT NOT NULL UNIQUE,
                workspace_id    TEXT NOT NULL,
                owner_id        TEXT,
                worker_run_id   TEXT NOT NULL,
                parent_run_id   TEXT,
                session_key     TEXT,
                agent_id        TEXT,
                persona_id      TEXT,
                key             TEXT NOT NULL,
                content         TEXT NOT NULL,
                category        TEXT NOT NULL,
                source_event_id TEXT,
                visibility      TEXT NOT NULL DEFAULT 'workspace',
                status          TEXT NOT NULL DEFAULT 'pending',
                payload_json    TEXT,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_memory_drafts_worker_run
                ON memory_drafts(worker_run_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_drafts_status
                ON memory_drafts(status, id);
            -- NOTE: idx_memory_drafts_source_event is created later (after the
            -- memory_drafts ALTER backfill below). A legacy `memory_drafts` table
            -- predating the `source_event_id` column would make this inline
            -- `CREATE INDEX ... ON memory_drafts(source_event_id)` fail with
            -- `no such column: source_event_id` inside this single batch.

            CREATE TABLE IF NOT EXISTS documents (
                id                      INTEGER PRIMARY KEY AUTOINCREMENT,
                document_id             TEXT NOT NULL UNIQUE,
                workspace_id            TEXT NOT NULL,
                owner_id                TEXT,
                topic_id                TEXT,
                task_id                 TEXT,
                source_message_event_id TEXT,
                source_kind             TEXT NOT NULL,
                source_uri              TEXT,
                title                   TEXT,
                content_sha256          TEXT NOT NULL,
                mime_type               TEXT,
                visibility              TEXT NOT NULL DEFAULT 'workspace',
                metadata_json           TEXT,
                chunk_count             INTEGER NOT NULL DEFAULT 0,
                created_at              TEXT NOT NULL,
                updated_at              TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_documents_workspace
                ON documents(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_documents_owner
                ON documents(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_documents_topic
                ON documents(workspace_id, topic_id, id);
            CREATE INDEX IF NOT EXISTS idx_documents_task
                ON documents(workspace_id, task_id, id);
            CREATE INDEX IF NOT EXISTS idx_documents_hash
                ON documents(content_sha256);

            CREATE TABLE IF NOT EXISTS document_chunks (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                chunk_id        TEXT NOT NULL UNIQUE,
                document_id     TEXT NOT NULL,
                workspace_id    TEXT NOT NULL,
                owner_id        TEXT,
                topic_id        TEXT,
                task_id         TEXT,
                chunk_index     INTEGER NOT NULL,
                heading         TEXT,
                content         TEXT NOT NULL,
                content_sha256  TEXT NOT NULL,
                embedding       BLOB,
                embedding_provider TEXT,
                embedding_model TEXT,
                embedding_dimensions INTEGER,
                source_anchor   TEXT NOT NULL,
                token_estimate  INTEGER NOT NULL DEFAULT 0,
                created_at      TEXT NOT NULL,
                FOREIGN KEY (document_id) REFERENCES documents(document_id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_document_chunks_document
                ON document_chunks(document_id, chunk_index);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_workspace
                ON document_chunks(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_owner
                ON document_chunks(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_topic
                ON document_chunks(workspace_id, topic_id, id);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_task
                ON document_chunks(workspace_id, task_id, id);

            CREATE VIRTUAL TABLE IF NOT EXISTS document_chunks_fts USING fts5(
                content, heading, content='document_chunks', content_rowid='rowid'
            );
            CREATE TRIGGER IF NOT EXISTS document_chunks_ai AFTER INSERT ON document_chunks BEGIN
                INSERT INTO document_chunks_fts(rowid, content, heading)
                VALUES (new.rowid, new.content, new.heading);
            END;
            CREATE TRIGGER IF NOT EXISTS document_chunks_ad AFTER DELETE ON document_chunks BEGIN
                INSERT INTO document_chunks_fts(document_chunks_fts, rowid, content, heading)
                VALUES ('delete', old.rowid, old.content, old.heading);
            END;
            CREATE TRIGGER IF NOT EXISTS document_chunks_au AFTER UPDATE ON document_chunks BEGIN
                INSERT INTO document_chunks_fts(document_chunks_fts, rowid, content, heading)
                VALUES ('delete', old.rowid, old.content, old.heading);
                INSERT INTO document_chunks_fts(rowid, content, heading)
                VALUES (new.rowid, new.content, new.heading);
            END;

            CREATE TABLE IF NOT EXISTS memory_links (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                link_id          TEXT NOT NULL UNIQUE,
                workspace_id     TEXT NOT NULL,
                owner_id         TEXT,
                memory_key       TEXT,
                memory_event_id  TEXT,
                message_event_id TEXT,
                document_id      TEXT NOT NULL,
                chunk_id         TEXT,
                link_type        TEXT NOT NULL,
                payload_json     TEXT,
                created_at       TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_memory_links_workspace
                ON memory_links(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_links_owner
                ON memory_links(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_links_memory_key
                ON memory_links(workspace_id, memory_key, id);
            CREATE INDEX IF NOT EXISTS idx_memory_links_document
                ON memory_links(document_id, chunk_id, id);

            CREATE TABLE IF NOT EXISTS retrieval_traces (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                trace_id            TEXT NOT NULL UNIQUE,
                workspace_id        TEXT NOT NULL,
                owner_id            TEXT,
                session_key         TEXT,
                agent_id            TEXT,
                persona_id          TEXT,
                source              TEXT NOT NULL,
                query               TEXT NOT NULL,
                candidate_count     INTEGER NOT NULL,
                selected_count      INTEGER NOT NULL,
                dropped_count       INTEGER NOT NULL,
                budget_tokens       INTEGER,
                selected_json       TEXT,
                dropped_json        TEXT,
                payload_json        TEXT,
                created_at          TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_retrieval_traces_workspace
                ON retrieval_traces(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_retrieval_traces_owner
                ON retrieval_traces(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_retrieval_traces_session
                ON retrieval_traces(workspace_id, session_key, id);
            CREATE INDEX IF NOT EXISTS idx_retrieval_traces_source
                ON retrieval_traces(workspace_id, source, id);

            CREATE TABLE IF NOT EXISTS compaction_runs (
                id                        INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id                    TEXT NOT NULL UNIQUE,
                workspace_id              TEXT NOT NULL,
                owner_id                  TEXT,
                session_key               TEXT,
                agent_id                  TEXT,
                persona_id                TEXT,
                trigger                   TEXT NOT NULL,
                mode                      TEXT NOT NULL,
                source_message_count      INTEGER NOT NULL,
                source_token_estimate     INTEGER NOT NULL,
                summary                   TEXT NOT NULL,
                summary_memory_key        TEXT,
                source_event_ids_json     TEXT,
                source_event_range_json   TEXT,
                source_document_refs_json TEXT,
                fidelity_status           TEXT NOT NULL,
                payload_json              TEXT,
                created_at                TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_workspace
                ON compaction_runs(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_owner
                ON compaction_runs(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_session
                ON compaction_runs(workspace_id, session_key, id);
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_trigger
                ON compaction_runs(workspace_id, \"trigger\", id);

            CREATE TABLE IF NOT EXISTS evolution_proposals (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                draft_id TEXT NOT NULL UNIQUE,
                owner_id TEXT NOT NULL,
                principal_id TEXT NOT NULL,
                workspace_id TEXT NOT NULL,
                topic_id TEXT,
                task_id TEXT,
                source_message_event_ids_json TEXT NOT NULL DEFAULT '[]',
                source_memory_event_ids_json TEXT NOT NULL DEFAULT '[]',
                evidence_hashes_json TEXT NOT NULL DEFAULT '[]',
                target_resource_json TEXT NOT NULL,
                proposed_change_json TEXT NOT NULL,
                risk_level TEXT NOT NULL CHECK (risk_level IN ('low','medium','high','critical')),
                mode TEXT NOT NULL CHECK (mode IN ('draft_only','shadow','auto')),
                created_at TEXT NOT NULL,
                created_by_runtime TEXT NOT NULL,
                judge_verdict_json TEXT,
                applied_at TEXT,
                applied_by TEXT,
                rollback_anchor_json TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_evolution_proposals_owner_workspace
                ON evolution_proposals(owner_id, workspace_id, created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_evolution_proposals_pending
                ON evolution_proposals(workspace_id, applied_at) WHERE applied_at IS NULL;
            CREATE INDEX IF NOT EXISTS idx_evolution_proposals_task
                ON evolution_proposals(task_id) WHERE task_id IS NOT NULL;

            CREATE TABLE IF NOT EXISTS evolution_proposal_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                draft_id TEXT NOT NULL,
                event_type TEXT NOT NULL CHECK (event_type IN (
                    'proposal.drafted','proposal.judged','proposal.approved',
                    'proposal.rejected','proposal.applied','proposal.rollback','proposal.evidence_mismatch'
                )),
                occurred_at TEXT NOT NULL,
                actor TEXT NOT NULL,
                payload_json TEXT,
                FOREIGN KEY (draft_id) REFERENCES evolution_proposals(draft_id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_evolution_proposal_events_draft
                ON evolution_proposal_events(draft_id, occurred_at);",
        )?;
        init_approval_grant_schema(conn)?;

        let mut column_stmt = conn.prepare("PRAGMA table_info(memories)")?;
        let existing_columns = column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut names = std::collections::HashSet::new();
        for column in existing_columns {
            names.insert(column?);
        }

        // Migration: add missing columns for backward compatibility.
        let missing_columns = [
            ("session_id", "ALTER TABLE memories ADD COLUMN session_id TEXT"),
            ("workspace_id", "ALTER TABLE memories ADD COLUMN workspace_id TEXT"),
            ("owner_id", "ALTER TABLE memories ADD COLUMN owner_id TEXT"),
            ("agent_id", "ALTER TABLE memories ADD COLUMN agent_id TEXT"),
            ("persona_id", "ALTER TABLE memories ADD COLUMN persona_id TEXT"),
            (
                "source_event_id",
                "ALTER TABLE memories ADD COLUMN source_event_id TEXT",
            ),
            ("source", "ALTER TABLE memories ADD COLUMN source TEXT"),
            ("channel", "ALTER TABLE memories ADD COLUMN channel TEXT"),
            ("chat_type", "ALTER TABLE memories ADD COLUMN chat_type TEXT"),
            ("chat_id", "ALTER TABLE memories ADD COLUMN chat_id TEXT"),
            ("sender_id", "ALTER TABLE memories ADD COLUMN sender_id TEXT"),
            ("raw_sender", "ALTER TABLE memories ADD COLUMN raw_sender TEXT"),
            ("topic_id", "ALTER TABLE memories ADD COLUMN topic_id TEXT"),
            (
                "visibility",
                // FIX-P2-04: match the CREATE TABLE default and Postgres parity.
                "ALTER TABLE memories ADD COLUMN visibility TEXT NOT NULL DEFAULT 'workspace'",
            ),
            (
                "sensitivity",
                "ALTER TABLE memories ADD COLUMN sensitivity TEXT NOT NULL DEFAULT 'normal'",
            ),
            (
                "risk_signals",
                "ALTER TABLE memories ADD COLUMN risk_signals TEXT DEFAULT '[]'",
            ),
            (
                "policy_version",
                "ALTER TABLE memories ADD COLUMN policy_version INTEGER DEFAULT 1",
            ),
            (
                "useful_count",
                "ALTER TABLE memories ADD COLUMN useful_count INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "embedding_provider",
                "ALTER TABLE memories ADD COLUMN embedding_provider TEXT",
            ),
            (
                "embedding_model",
                "ALTER TABLE memories ADD COLUMN embedding_model TEXT",
            ),
            (
                "embedding_dimensions",
                "ALTER TABLE memories ADD COLUMN embedding_dimensions INTEGER",
            ),
            (
                "update_seq",
                "ALTER TABLE memories ADD COLUMN update_seq INTEGER NOT NULL DEFAULT 0",
            ),
        ];
        for (name, alter_sql) in missing_columns {
            if !names.contains(name) {
                match conn.execute_batch(alter_sql) {
                    Ok(()) => {}
                    Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
                        if msg.contains("duplicate column name") =>
                    {
                        tracing::debug!("Column memories.{name} already exists (concurrent migration): {err}");
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to add memories.{name}: {e}"));
                    }
                }
            }
        }

        // Only now do `memories.embedding_provider` / `embedding_model` /
        // `embedding_dimensions` exist on every store: a database created by an
        // older build (or by `snapshot::hydrate_from_snapshot`, which writes a
        // minimal `memories` table) gets them from the ALTER TABLE block above,
        // so an index spanning them cannot live in the CREATE TABLE batch.
        conn.execute_batch(
            // Backfill: every row written before `update_seq` existed takes its
            // `rowid`, which is the insertion order the 0.8.126 window used, so
            // an untouched legacy store keeps the exact candidate set it had.
            // `rowid` is >= 1, so a backfilled row is never confused with the
            // column default again, and the next write continues above the
            // highest backfilled value. The `WHERE` clause makes this a no-op on
            // every later open, and the trigger below does not fire for it
            // because it changes `update_seq` itself.
            "UPDATE memories SET update_seq = rowid WHERE update_seq = 0;

             -- Superseded index: it led with `updated_at`, a local-offset RFC
             -- 3339 string whose lexicographic order is not chronological across
             -- timezone or DST changes, and it carried none of the
             -- embedding-identity columns every vector query filters on, so a
             -- store whose embedding model had changed scanned past large runs
             -- of non-matching rows to fill the candidate set.
             DROP INDEX IF EXISTS idx_memories_embedding_recency;
             -- Superseded index: same three leading columns, but without
             -- `update_seq` it could only order the window by `rowid`, which
             -- does not move when `ON CONFLICT(key) DO UPDATE` refreshes an
             -- existing key. A just-refreshed old memory therefore stayed
             -- outside the window. Renaming the index rather than redefining it
             -- keeps `CREATE INDEX IF NOT EXISTS` a no-op on later opens instead
             -- of rebuilding the index on every process start.
             DROP INDEX IF EXISTS idx_memories_embedding_candidates;
             -- Leads with the three columns `vector_search` always
             -- equality-filters and ends with the ordering key, so the
             -- newest-first candidate window is a backward index scan with no
             -- sort and no scan of rows belonging to another embedding model.
             CREATE INDEX IF NOT EXISTS idx_memories_embedding_candidates_seq
                 ON memories(embedding_provider, embedding_model, embedding_dimensions, update_seq)
                 WHERE embedding IS NOT NULL;
             -- Serves the `MAX(update_seq)` lookup in the triggers below as a
             -- single covering-index descent instead of a table scan per write.
             CREATE INDEX IF NOT EXISTS idx_memories_update_seq ON memories(update_seq);

             -- The update sequence is maintained in the schema, not in the call
             -- sites: `memories` is written by upserts here, by metadata and
             -- useful-count updates, by draft merges and by the webhook and
             -- fitness writers. A trigger is the one place that cannot be
             -- forgotten by a new writer, and it also covers rows an older
             -- build wrote without it.
             --
             -- Both bodies update only `update_seq`, which is why
             -- `memories_au_fts` is guarded on `key`/`content`: an unguarded
             -- FTS sync trigger would fire for these updates and corrupt the
             -- external content index.
             CREATE TRIGGER IF NOT EXISTS memories_update_seq_ai
             AFTER INSERT ON memories
             BEGIN
                 UPDATE memories
                    SET update_seq = (SELECT COALESCE(MAX(update_seq), 0) + 1 FROM memories)
                  WHERE rowid = new.rowid;
             END;
             -- The `WHEN` guard is what makes the body's own UPDATE terminate:
             -- it raises `update_seq`, so a re-entry (which only happens under
             -- `PRAGMA recursive_triggers = ON`) sees new.update_seq <>
             -- old.update_seq and does nothing. It also leaves the backfill
             -- above and any deliberate sequence rewrite alone.
             CREATE TRIGGER IF NOT EXISTS memories_update_seq_au
             AFTER UPDATE ON memories
             WHEN new.update_seq = old.update_seq
             BEGIN
                 UPDATE memories
                    SET update_seq = (SELECT COALESCE(MAX(update_seq), 0) + 1 FROM memories)
                  WHERE rowid = new.rowid;
             END;",
        )?;

        let mut chunk_column_stmt = conn.prepare("PRAGMA table_info(document_chunks)")?;
        let existing_chunk_columns = chunk_column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut chunk_names = std::collections::HashSet::new();
        for column in existing_chunk_columns {
            chunk_names.insert(column?);
        }
        for (name, alter_sql) in [
            ("embedding", "ALTER TABLE document_chunks ADD COLUMN embedding BLOB"),
            (
                "embedding_provider",
                "ALTER TABLE document_chunks ADD COLUMN embedding_provider TEXT",
            ),
            (
                "embedding_model",
                "ALTER TABLE document_chunks ADD COLUMN embedding_model TEXT",
            ),
            (
                "embedding_dimensions",
                "ALTER TABLE document_chunks ADD COLUMN embedding_dimensions INTEGER",
            ),
        ] {
            if !chunk_names.contains(name) {
                match conn.execute_batch(alter_sql) {
                    Ok(()) => {}
                    Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
                        if msg.contains("duplicate column name") =>
                    {
                        tracing::debug!("Column document_chunks.{name} already exists (concurrent migration): {err}");
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to add document_chunks.{name}: {e}"));
                    }
                }
            }
        }

        let mut cache_column_stmt = conn.prepare("PRAGMA table_info(embedding_cache)")?;
        let existing_cache_columns = cache_column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut cache_names = std::collections::HashSet::new();
        for column in existing_cache_columns {
            cache_names.insert(column?);
        }
        for (name, alter_sql) in [
            ("provider", "ALTER TABLE embedding_cache ADD COLUMN provider TEXT"),
            ("model", "ALTER TABLE embedding_cache ADD COLUMN model TEXT"),
            (
                "dimensions",
                "ALTER TABLE embedding_cache ADD COLUMN dimensions INTEGER",
            ),
        ] {
            if !cache_names.contains(name) {
                match conn.execute_batch(alter_sql) {
                    Ok(()) => {}
                    Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
                        if msg.contains("duplicate column name") =>
                    {
                        tracing::debug!("Column embedding_cache.{name} already exists (concurrent migration): {err}");
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to add embedding_cache.{name}: {e}"));
                    }
                }
            }
        }

        // FIX-P0-26: upgrade a legacy single-column `embedding_cache` primary key
        // (`content_hash`) to the composite `(content_hash, provider, model,
        // dimensions)` so different providers/models/dimensions for the same
        // content no longer collide. SQLite cannot alter a primary key in place,
        // so the table is rebuilt when the legacy PK is detected.
        Self::upgrade_embedding_cache_primary_key(&conn)?;

        let mut msg_column_stmt = conn.prepare("PRAGMA table_info(message_events)")?;
        let existing_msg_columns = msg_column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut msg_names = std::collections::HashSet::new();
        for column in existing_msg_columns {
            msg_names.insert(column?);
        }
        if !msg_names.contains("owner_id") {
            match conn.execute_batch("ALTER TABLE message_events ADD COLUMN owner_id TEXT") {
                Ok(()) => {}
                Err(rusqlite::Error::SqliteFailure(err, Some(ref msg))) if msg.contains("duplicate column name") => {
                    tracing::debug!("Column message_events.owner_id already exists (concurrent migration): {err}");
                }
                Err(e) => return Err(anyhow::anyhow!("Failed to add message_events.owner_id: {e}")),
            }
        }
        if !msg_names.contains("event_type") {
            match conn.execute_batch("ALTER TABLE message_events ADD COLUMN event_type TEXT") {
                Ok(()) => {}
                Err(rusqlite::Error::SqliteFailure(err, Some(ref msg))) if msg.contains("duplicate column name") => {
                    tracing::debug!("Column message_events.event_type already exists (concurrent migration): {err}");
                }
                Err(e) => return Err(anyhow::anyhow!("Failed to add message_events.event_type: {e}")),
            }
        }
        for (name, sql_type) in [
            ("source_ref_json", "TEXT"),
            ("subject_ref_json", "TEXT"),
            ("goal_id", "TEXT"),
            ("causation_event_id", "TEXT"),
            ("correlation_id", "TEXT"),
            ("attempt_id", "TEXT"),
            ("lease_epoch", "INTEGER"),
            ("config_generation_id", "INTEGER"),
            ("config_source_revision", "TEXT"),
        ] {
            if msg_names.contains(name) {
                continue;
            }
            let alter_sql = format!("ALTER TABLE message_events ADD COLUMN {name} {sql_type}");
            match conn.execute_batch(&alter_sql) {
                Ok(()) => {}
                Err(rusqlite::Error::SqliteFailure(err, Some(ref msg))) if msg.contains("duplicate column name") => {
                    tracing::debug!("Column message_events.{name} already exists (concurrent migration): {err}");
                }
                Err(e) => return Err(anyhow::anyhow!("Failed to add message_events.{name}: {e}")),
            }
        }
        Self::upgrade_message_event_idempotency_scope(conn)?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_message_events_event_type
                ON message_events(workspace_id, event_type, id);
             CREATE INDEX IF NOT EXISTS idx_message_events_correlation
                ON message_events(workspace_id, correlation_id, id);
             CREATE INDEX IF NOT EXISTS idx_message_events_config_generation
                ON message_events(workspace_id, config_generation_id, id);",
        )?;

        let mut session_column_stmt = conn.prepare("PRAGMA table_info(sessions)")?;
        let existing_session_columns = session_column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut session_names = std::collections::HashSet::new();
        for column in existing_session_columns {
            session_names.insert(column?);
        }
        if !session_names.contains("owner_id") {
            match conn.execute_batch("ALTER TABLE sessions ADD COLUMN owner_id TEXT") {
                Ok(()) => {}
                Err(rusqlite::Error::SqliteFailure(err, Some(ref msg))) if msg.contains("duplicate column name") => {
                    tracing::debug!("Column sessions.owner_id already exists (concurrent migration): {err}");
                }
                Err(e) => return Err(anyhow::anyhow!("Failed to add sessions.owner_id: {e}")),
            }
        }

        let mut turn_column_stmt = conn.prepare("PRAGMA table_info(conversation_turns)")?;
        let existing_turn_columns = turn_column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut turn_names = std::collections::HashSet::new();
        for column in existing_turn_columns {
            turn_names.insert(column?);
        }
        let missing_turn_columns = [
            ("owner_id", "ALTER TABLE conversation_turns ADD COLUMN owner_id TEXT"),
            (
                "message_event_id",
                "ALTER TABLE conversation_turns ADD COLUMN message_event_id TEXT",
            ),
            ("agent_id", "ALTER TABLE conversation_turns ADD COLUMN agent_id TEXT"),
            (
                "persona_id",
                "ALTER TABLE conversation_turns ADD COLUMN persona_id TEXT",
            ),
            (
                "visibility",
                "ALTER TABLE conversation_turns ADD COLUMN visibility TEXT",
            ),
        ];
        for (name, alter_sql) in missing_turn_columns {
            if !turn_names.contains(name) {
                match conn.execute_batch(alter_sql) {
                    Ok(()) => {}
                    Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
                        if msg.contains("duplicate column name") =>
                    {
                        tracing::debug!(
                            "Column conversation_turns.{name} already exists (concurrent migration): {err}"
                        );
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to add conversation_turns.{name}: {e}"));
                    }
                }
            }
        }

        let mut draft_column_stmt = conn.prepare("PRAGMA table_info(memory_drafts)")?;
        let existing_draft_columns = draft_column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut draft_names = std::collections::HashSet::new();
        for column in existing_draft_columns {
            draft_names.insert(column?);
        }
        let missing_draft_columns = [
            ("owner_id", "ALTER TABLE memory_drafts ADD COLUMN owner_id TEXT"),
            (
                "parent_run_id",
                "ALTER TABLE memory_drafts ADD COLUMN parent_run_id TEXT",
            ),
            ("agent_id", "ALTER TABLE memory_drafts ADD COLUMN agent_id TEXT"),
            ("persona_id", "ALTER TABLE memory_drafts ADD COLUMN persona_id TEXT"),
            (
                "source_event_id",
                "ALTER TABLE memory_drafts ADD COLUMN source_event_id TEXT",
            ),
            (
                "visibility",
                "ALTER TABLE memory_drafts ADD COLUMN visibility TEXT NOT NULL DEFAULT 'workspace'",
            ),
            ("payload_json", "ALTER TABLE memory_drafts ADD COLUMN payload_json TEXT"),
        ];
        for (name, alter_sql) in missing_draft_columns {
            if !draft_names.contains(name) {
                match conn.execute_batch(alter_sql) {
                    Ok(()) => {}
                    Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
                        if msg.contains("duplicate column name") =>
                    {
                        tracing::debug!("Column memory_drafts.{name} already exists (concurrent migration): {err}");
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to add memory_drafts.{name}: {e}"));
                    }
                }
            }
        }

        let mut compaction_column_stmt = conn.prepare("PRAGMA table_info(compaction_runs)")?;
        let existing_compaction_columns = compaction_column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut compaction_names = std::collections::HashSet::new();
        for column in existing_compaction_columns {
            compaction_names.insert(column?);
        }
        if !compaction_names.contains("source_event_range_json") {
            match conn.execute_batch("ALTER TABLE compaction_runs ADD COLUMN source_event_range_json TEXT") {
                Ok(()) => {}
                Err(rusqlite::Error::SqliteFailure(err, Some(ref msg))) if msg.contains("duplicate column name") => {
                    tracing::debug!(
                        "Column compaction_runs.source_event_range_json already exists (concurrent migration): {err}"
                    );
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Failed to add compaction_runs.source_event_range_json: {e}"
                    ));
                }
            }
        }

        // Schema v7 (`memory_events_run_lineage`) added `run_id` / `parent_run_id`
        // to `memory_events`. A legacy table created under v4 lacks these columns,
        // and the checksum-anchor registry alone never alters an existing table, so
        // the gateway hit `no such column: run_id` at runtime. Backfill the columns
        // idempotently (mirrors the `memories` / `memory_drafts` blocks above), then
        // create the lineage indexes — both must run *after* the ALTERs.
        let mut event_column_stmt = conn.prepare("PRAGMA table_info(memory_events)")?;
        let existing_event_columns = event_column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut event_names = std::collections::HashSet::new();
        for column in existing_event_columns {
            event_names.insert(column?);
        }
        let missing_event_columns = [
            ("run_id", "ALTER TABLE memory_events ADD COLUMN run_id TEXT"),
            (
                "parent_run_id",
                "ALTER TABLE memory_events ADD COLUMN parent_run_id TEXT",
            ),
            // Schema v16 (C03 / V02-R4): owner attribution for memory events, so
            // an owner-isolated principal can be denied another owner's
            // `session`/`private` rows. Existing rows keep a NULL owner and are
            // never rewritten; NULL means unattributed.
            ("owner_id", "ALTER TABLE memory_events ADD COLUMN owner_id TEXT"),
        ];
        for (name, alter_sql) in missing_event_columns {
            if !event_names.contains(name) {
                match conn.execute_batch(alter_sql) {
                    Ok(()) => {}
                    Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
                        if msg.contains("duplicate column name") =>
                    {
                        tracing::debug!("Column memory_events.{name} already exists (concurrent migration): {err}");
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to add memory_events.{name}: {e}"));
                    }
                }
            }
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_memory_events_run
                 ON memory_events(workspace_id, run_id, id);
             CREATE INDEX IF NOT EXISTS idx_memory_events_parent_run
                 ON memory_events(workspace_id, parent_run_id, id);
             CREATE INDEX IF NOT EXISTS idx_memory_events_owner
                 ON memory_events(workspace_id, owner_id, id);",
        )?;

        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_memories_session ON memories(session_id);
             CREATE INDEX IF NOT EXISTS idx_mem_owner ON memories(owner_id);
             CREATE INDEX IF NOT EXISTS idx_mem_vis_chan_type_chat
                 ON memories(visibility, channel, chat_type, chat_id, sensitivity, created_at DESC);
             CREATE INDEX IF NOT EXISTS idx_mem_sender ON memories(sender_id);
             CREATE INDEX IF NOT EXISTS idx_mem_topic_time ON memories(topic_id, created_at DESC);
             CREATE INDEX IF NOT EXISTS idx_mem_channel ON memories(channel);
             CREATE INDEX IF NOT EXISTS idx_mem_workspace_agent ON memories(workspace_id, agent_id, persona_id);
             CREATE INDEX IF NOT EXISTS idx_mem_source_event ON memories(source_event_id);
             CREATE INDEX IF NOT EXISTS idx_message_events_owner
                 ON message_events(workspace_id, owner_id, id);
             CREATE INDEX IF NOT EXISTS idx_sessions_owner
                 ON sessions(owner_id);
             CREATE INDEX IF NOT EXISTS idx_conversation_turns_owner_session
                 ON conversation_turns(owner_id, session_key);
             CREATE INDEX IF NOT EXISTS idx_conversation_turns_message_event
                 ON conversation_turns(message_event_id);
             CREATE INDEX IF NOT EXISTS idx_memory_drafts_worker_run
                 ON memory_drafts(worker_run_id, id);
             CREATE INDEX IF NOT EXISTS idx_memory_drafts_owner
                 ON memory_drafts(workspace_id, owner_id, id);
             CREATE INDEX IF NOT EXISTS idx_memory_drafts_status
                 ON memory_drafts(status, id);
             CREATE INDEX IF NOT EXISTS idx_memory_drafts_source_event
                 ON memory_drafts(source_event_id);",
        )?;

        conn.execute_batch(
            "UPDATE sessions
             SET owner_id = (
                 SELECT me.owner_id
                 FROM message_events me
                 WHERE me.session_key = sessions.session_key
                   AND me.owner_id IS NOT NULL
                 ORDER BY me.id ASC
                 LIMIT 1
             )
             WHERE owner_id IS NULL;
             UPDATE sessions
             SET owner_id = 'legacy:' || session_key
             WHERE owner_id IS NULL;
             UPDATE conversation_turns
             SET owner_id = (
                 SELECT s.owner_id
                 FROM sessions s
                 WHERE s.session_key = conversation_turns.session_key
             )
             WHERE owner_id IS NULL;
             UPDATE conversation_turns
             SET owner_id = 'legacy:' || session_key
             WHERE owner_id IS NULL;",
        )?;

        // FIX-P2-04: normalize legacy rows whose `visibility` was left implicitly
        // NULL or empty so the column always carries the unified 'workspace'
        // default. Explicit values (including an intentional 'private') are left
        // untouched — this never downgrades a stricter visibility.
        conn.execute(
            "UPDATE memories SET visibility = 'workspace' \
             WHERE visibility IS NULL OR visibility = ''",
            [],
        )?;

        Self::run_memory_schema_migrations(conn)?;

        Ok(())
    }

    /// FIX-P0-26: rebuild a legacy `embedding_cache` whose primary key is the
    /// single `content_hash` column into one keyed by the composite
    /// `(content_hash, provider, model, dimensions)`.
    ///
    /// Detection uses `PRAGMA table_info`: if exactly one column has a non-zero
    /// `pk` ordinal the table predates the composite key and is rebuilt. The
    /// rebuild coalesces legacy NULL provider/model/dimensions to the same
    /// defaults the fresh schema uses and de-duplicates on the composite key,
    /// keeping the most recently accessed row.
    fn upgrade_embedding_cache_primary_key(conn: &Connection) -> anyhow::Result<()> {
        let pk_columns: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('embedding_cache') WHERE pk > 0",
            [],
            |row| row.get(0),
        )?;
        // 0 ⇒ table missing (fresh CREATE handles it); >1 ⇒ already composite.
        if pk_columns != 1 {
            return Ok(());
        }

        // De-duplicate on the composite key first: legacy rows could differ only
        // by NULL vs '' provider/model/dimensions that collapse to the same
        // normalized key. `GROUP BY` keeps one row per composite key (the one with
        // the most recent `accessed_at` via MAX), avoiding a UNIQUE violation that
        // `INSERT OR REPLACE` does not reliably resolve inside `execute_batch`.
        conn.execute_batch(
            "CREATE TABLE embedding_cache_v2 (
                content_hash TEXT NOT NULL,
                embedding    BLOB NOT NULL,
                provider     TEXT NOT NULL DEFAULT '',
                model        TEXT NOT NULL DEFAULT '',
                dimensions   INTEGER NOT NULL DEFAULT 0,
                created_at   TEXT NOT NULL,
                accessed_at  TEXT NOT NULL,
                PRIMARY KEY (content_hash, provider, model, dimensions)
            );
            INSERT INTO embedding_cache_v2 (
                content_hash, embedding, provider, model, dimensions, created_at, accessed_at
            )
            SELECT content_hash, embedding, norm_provider, norm_model, norm_dimensions,
                   created_at, accessed_at
            FROM (
                SELECT content_hash, embedding,
                       COALESCE(provider, '') AS norm_provider,
                       COALESCE(model, '')    AS norm_model,
                       COALESCE(dimensions, 0) AS norm_dimensions,
                       created_at, accessed_at,
                       ROW_NUMBER() OVER (
                           PARTITION BY content_hash, COALESCE(provider, ''),
                                        COALESCE(model, ''), COALESCE(dimensions, 0)
                           ORDER BY accessed_at DESC
                       ) AS rn
                FROM embedding_cache
            )
            WHERE rn = 1;
            DROP TABLE embedding_cache;
            ALTER TABLE embedding_cache_v2 RENAME TO embedding_cache;
            CREATE INDEX IF NOT EXISTS idx_cache_accessed ON embedding_cache(accessed_at);",
        )?;
        Ok(())
    }

    /// Rebuild legacy `message_events` tables whose `idempotency_key` was
    /// globally unique. Idempotency belongs to a workspace boundary: two
    /// independent workspaces may legitimately receive the same external
    /// provider key, while retries inside one workspace must still converge.
    fn upgrade_message_event_idempotency_scope(conn: &Connection) -> anyhow::Result<()> {
        let mut index_stmt = conn.prepare("PRAGMA index_list('message_events')")?;
        let indexes = index_stmt.query_map([], |row| Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)?)))?;
        for index in indexes {
            let (index_name, unique) = index?;
            if unique == 0 {
                continue;
            }
            let mut columns_stmt = conn.prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")?;
            let columns = columns_stmt
                .query_map([index_name], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if columns == ["workspace_id", "idempotency_key"] {
                return Ok(());
            }
        }

        let rebuild = conn.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE message_events RENAME TO message_events_legacy_idempotency_scope;
             CREATE TABLE message_events (
                 id                 INTEGER PRIMARY KEY AUTOINCREMENT,
                 event_id           TEXT NOT NULL UNIQUE,
                 idempotency_key    TEXT,
                 workspace_id       TEXT NOT NULL,
                 owner_id           TEXT,
                 source             TEXT NOT NULL,
                 channel            TEXT,
                 session_key        TEXT,
                 parent_session_key TEXT,
                 run_id             TEXT,
                 parent_run_id      TEXT,
                 agent_id           TEXT,
                 persona_id         TEXT,
                 sender             TEXT,
                 recipient          TEXT,
                 role               TEXT NOT NULL,
                 event_type         TEXT NOT NULL,
                 source_ref_json    TEXT,
                 subject_ref_json   TEXT,
                 goal_id            TEXT,
                 causation_event_id TEXT,
                 correlation_id     TEXT,
                 attempt_id         TEXT,
                 lease_epoch        INTEGER,
                 config_generation_id INTEGER,
                 config_source_revision TEXT,
                 content            TEXT NOT NULL,
                 content_hash       TEXT,
                 raw_payload_json   TEXT,
                 visibility         TEXT NOT NULL DEFAULT 'workspace',
                 created_at         TEXT NOT NULL,
                 updated_at         TEXT NOT NULL,
                 UNIQUE (workspace_id, idempotency_key)
             );
             INSERT INTO message_events (
                 id, event_id, idempotency_key, workspace_id, owner_id, source, channel,
                 session_key, parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                 sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                 goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch,
                 config_generation_id, config_source_revision,
                 content, content_hash, raw_payload_json, visibility, created_at, updated_at
             )
             SELECT
                 id, event_id, idempotency_key, workspace_id, owner_id, source, channel,
                 session_key, parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                 sender, recipient, role, COALESCE(event_type, 'message.legacy'),
                 source_ref_json, subject_ref_json, goal_id, causation_event_id,
                 correlation_id, attempt_id, lease_epoch, config_generation_id,
                 config_source_revision, content, content_hash,
                 raw_payload_json, visibility, created_at, updated_at
             FROM message_events_legacy_idempotency_scope;
             DROP TABLE message_events_legacy_idempotency_scope;
             CREATE INDEX idx_message_events_workspace_id
                 ON message_events(workspace_id, id);
             CREATE INDEX idx_message_events_owner
                 ON message_events(workspace_id, owner_id, id);
             CREATE INDEX idx_message_events_session
                 ON message_events(workspace_id, session_key, id);
             CREATE INDEX idx_message_events_agent
                 ON message_events(workspace_id, agent_id, id);
             CREATE INDEX idx_message_events_channel_sender
                 ON message_events(workspace_id, channel, sender, id);
             CREATE INDEX idx_message_events_visibility
                 ON message_events(workspace_id, visibility, id);
             CREATE INDEX idx_message_events_event_type
                 ON message_events(workspace_id, event_type, id);
             CREATE INDEX idx_message_events_correlation
                 ON message_events(workspace_id, correlation_id, id);
             CREATE INDEX idx_message_events_created_at
                 ON message_events(created_at);
             CREATE INDEX idx_message_events_config_generation
                 ON message_events(workspace_id, config_generation_id, id);
             COMMIT;",
        );
        if let Err(error) = rebuild {
            let _ = conn.execute_batch("ROLLBACK;");
            return Err(error).context("failed to scope message event idempotency to workspace");
        }
        Ok(())
    }

    /// Record-and-verify versioned schema migrations (FIX-P0-25).
    ///
    /// The legacy `init_schema` above creates / upgrades tables with idempotent
    /// `CREATE TABLE IF NOT EXISTS` + `ALTER TABLE ... ADD COLUMN` statements and
    /// previously swallowed ALTER failures via `tracing::debug`. This ledger gives
    /// every logical schema step a stable `version`, a `name`, and a SHA-256
    /// `checksum` of its canonical descriptor, so drift is detected at startup:
    ///
    /// - already-applied version, checksum matches → skipped;
    /// - already-applied version, checksum differs → `bail!` (fail-fast: an
    ///   engineer changed a registered step without bumping its version);
    /// - unapplied version → recorded (the DDL itself already ran above).
    ///
    /// New schema changes MUST append a new `(version, name, sql)` entry to
    /// [`Self::memory_schema_migration_registry`] and NEVER mutate an existing
    /// entry's text.
    fn run_memory_schema_migrations(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_schema_migrations (
                version    INTEGER PRIMARY KEY,
                name       TEXT NOT NULL,
                checksum   TEXT NOT NULL,
                applied_at TEXT NOT NULL
            );",
        )?;

        for (version, name, sql) in Self::memory_schema_migration_registry() {
            let checksum = Self::schema_migration_checksum(sql);
            let recorded: Option<String> = conn
                .query_row(
                    "SELECT checksum FROM memory_schema_migrations WHERE version = ?1",
                    params![version],
                    |row| row.get(0),
                )
                .optional()?;
            match recorded {
                Some(recorded) => {
                    if recorded != checksum {
                        anyhow::bail!(
                            "memory schema migration checksum mismatch for version {version} ({name}): \
                             expected {checksum}, found {recorded}"
                        );
                    }
                }
                None => {
                    conn.execute(
                        "INSERT INTO memory_schema_migrations (version, name, checksum, applied_at) \
                         VALUES (?1, ?2, ?3, ?4)",
                        params![version, name, checksum, Utc::now().to_rfc3339()],
                    )?;
                }
            }
        }
        Ok(())
    }

    /// SHA-256 (hex) of a migration's canonical descriptor text.
    pub(crate) fn schema_migration_checksum(sql: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(sql.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Versioned registry of the canonical schema steps created by `init_schema`,
    /// retro-fitted in execution order. Each text is a stable canonical descriptor
    /// used only as a checksum anchor (version unchanged ⇒ text unchanged).
    pub(crate) const fn memory_schema_migration_registry() -> &'static [(i64, &'static str, &'static str)] {
        &[
            (
                1,
                "memories_core",
                "memories(id,key,content,category,embedding,embedding_provider,embedding_model,embedding_dimensions,created_at,updated_at,workspace_id,owner_id,agent_id,persona_id,source_event_id,source,channel,chat_type,chat_id,sender_id,raw_sender,topic_id,visibility,sensitivity,risk_signals,policy_version,useful_count) + memories_fts + embedding_cache",
            ),
            (
                2,
                "identity_topics_audit",
                "identity_bindings + agent_identity_bindings + user_policies + topics + topics_fts + topic_participants + topic_aliases + access_audit_log",
            ),
            (
                3,
                "sessions_and_turns",
                "sessions(session_key,channel,sender,owner_id,created_at,updated_at,message_count,last_message_preview) + conversation_turns(id,session_key,owner_id,role,content,timestamp,message_id,message_event_id,agent_id,persona_id,visibility)",
            ),
            (
                4,
                "message_and_memory_events",
                "message_events(id,event_id,idempotency_key,workspace_id,owner_id,source,channel,session_key,parent_session_key,run_id,parent_run_id,agent_id,persona_id,sender,recipient,role,event_type,content,content_hash,raw_payload_json,visibility,created_at,updated_at) + memory_events(id,event_id,workspace_id,event_type,subject_table,subject_id,session_key,agent_id,persona_id,visibility,payload_json,created_at)",
            ),
            (
                5,
                "memory_drafts",
                "memory_drafts(id,draft_id,workspace_id,owner_id,worker_run_id,parent_run_id,session_key,agent_id,persona_id,key,content,category,source_event_id,visibility,status,payload_json,created_at,updated_at)",
            ),
            (
                6,
                "documents_links_traces_compaction",
                "documents + document_chunks + memory_links + retrieval_traces + compaction_runs + evolution_proposals + evolution_proposal_events",
            ),
            (
                7,
                "memory_events_run_lineage",
                "memory_events + run_id + parent_run_id + idx_memory_events_run + idx_memory_events_parent_run",
            ),
            (
                8,
                "memory_trash",
                "memory_trash(id,trash_id,memory_key,content,category,reason,trashed_at,grace_until,restored_at) + idx_memory_trash_key + idx_memory_trash_grace",
            ),
            (
                9,
                "memories_visibility_default_workspace",
                // FIX-P2-04: the `memories.visibility` column default is unified to
                // 'workspace' (CREATE TABLE + ALTER) for parity with Postgres. The
                // backfill below normalizes legacy rows whose visibility was left
                // implicitly NULL/empty; it never downgrades an explicit value.
                "memories.visibility default 'workspace' (parity with postgres) + backfill NULL/'' -> 'workspace'",
            ),
            (
                10,
                "embedding_cache_composite_primary_key",
                // FIX-P0-26: embedding_cache primary key upgraded from single
                // `content_hash` to composite `(content_hash, provider, model,
                // dimensions)` (parity with Postgres). The legacy table is rebuilt
                // in-place by `upgrade_embedding_cache_primary_key`.
                "embedding_cache PRIMARY KEY (content_hash, provider, model, dimensions) + rebuild legacy single-key table",
            ),
            (
                11,
                "approval_grant_revocations",
                // FIX-P3-06: out-of-band grant revocation ledger. A grant can be
                // revoked without rewriting its signed payload by inserting a row
                // here; the gate consults this table in addition to the in-grant
                // `revoked_at` field. Created by `init_approval_grant_schema`.
                "approval_grant_revocations(grant_id,revoked_at,reason) + idx_approval_grant_revocations_revoked_at",
            ),
            (
                12,
                "compaction_source_event_range",
                "compaction_runs.source_event_ids_json contains only real MessageEvent event_id strings + source_event_range_json(first_event_id,last_event_id,first_row_id,last_row_id,source_event_count)",
            ),
            (
                13,
                "message_event_workspace_idempotency",
                "message_events UNIQUE(workspace_id,idempotency_key) replaces global UNIQUE(idempotency_key) + workspace-scoped conflict lookup",
            ),
            (
                14,
                "message_event_config_generation",
                "message_events + config_generation_id + config_source_revision + idx_message_events_config_generation",
            ),
            (
                15,
                "message_event_execution_metadata",
                "message_events + source_ref_json + subject_ref_json + goal_id + causation_event_id + correlation_id + attempt_id + lease_epoch",
            ),
            (
                16,
                "memory_event_owner_isolation",
                // C03 / V02-R4: `memory_events` gains `owner_id` so an
                // externally authenticated (owner-isolated) principal can be
                // refused another owner's `session`/`private` rows. Added by an
                // idempotent ALTER; legacy rows keep NULL (unattributed).
                "memory_events + owner_id + idx_memory_events_owner",
            ),
        ]
    }

    fn category_to_str(cat: &MemoryCategory) -> String {
        match cat {
            MemoryCategory::Core => "core".into(),
            MemoryCategory::Daily => "daily".into(),
            MemoryCategory::Conversation => "conversation".into(),
            MemoryCategory::Custom(name) => name.clone(),
        }
    }

    fn str_to_category(s: &str) -> MemoryCategory {
        match s {
            "core" => MemoryCategory::Core,
            "daily" => MemoryCategory::Daily,
            "conversation" => MemoryCategory::Conversation,
            other => MemoryCategory::Custom(other.to_string()),
        }
    }

    fn message_event_from_row(row: &Row<'_>) -> rusqlite::Result<MessageEvent> {
        let source_legacy: String = row.get(5)?;
        let source_ref_json: Option<String> = row.get(17)?;
        let source = source_ref_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or_else(|| source_legacy.into());
        let subject_ref_json: Option<String> = row.get(18)?;
        let subject = subject_ref_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok());
        let visibility_raw: String = row.get(29)?;
        Ok(MessageEvent {
            id: row.get(0)?,
            event_id: row.get(1)?,
            idempotency_key: row.get(2)?,
            workspace_id: row.get(3)?,
            owner_id: row.get(4)?,
            source,
            channel: row.get(6)?,
            session_key: row.get(7)?,
            parent_session_key: row.get(8)?,
            run_id: row.get(9)?,
            parent_run_id: row.get(10)?,
            agent_id: row.get(11)?,
            persona_id: row.get(12)?,
            sender: row.get(13)?,
            recipient: row.get(14)?,
            role: row.get(15)?,
            event_type: row
                .get::<_, Option<String>>(16)?
                .unwrap_or_else(|| "message.legacy".to_string()),
            subject,
            goal_id: row.get(19)?,
            causation_event_id: row.get(20)?,
            correlation_id: row.get(21)?,
            attempt_id: row.get(22)?,
            lease_epoch: row.get(23)?,
            config_generation_id: row
                .get::<_, Option<i64>>(24)?
                .and_then(|value| u64::try_from(value).ok()),
            config_source_revision: row.get(25)?,
            content: row.get(26)?,
            content_hash: row.get(27)?,
            raw_payload_json: row.get(28)?,
            visibility: visibility_raw.parse().unwrap_or(MemoryVisibility::Workspace),
            created_at: row.get(30)?,
            updated_at: row.get(31)?,
        })
    }

    /// Owner-id constraint for evolution-proposal ACL.
    ///
    /// System principals (`self_system`/`router`/`internal`/`system` as agent or
    /// owner) may query/act globally → `None`. Everyone else is constrained to
    /// their effective owner id (or empty string, which matches no row).
    fn evolution_owner_scope(principal: &MemoryPrincipal) -> Option<String> {
        const SYSTEM_IDS: &[&str] = &["self_system", "router", "internal", "system"];
        let is_system = principal.agent_id.as_deref().is_some_and(|id| SYSTEM_IDS.contains(&id))
            || principal
                .persona_id
                .as_deref()
                .is_some_and(|id| SYSTEM_IDS.contains(&id))
            || principal.owner_id.as_deref().is_some_and(|id| SYSTEM_IDS.contains(&id));
        if is_system {
            return None;
        }
        Some(principal.effective_owner_id().unwrap_or_default())
    }

    fn evolution_proposal_from_row(
        row: &Row<'_>,
    ) -> rusqlite::Result<anyhow::Result<crate::self_system::evolution::EvolutionProposalDraft>> {
        use crate::self_system::evolution::proposal::ProposalRowValues;
        let draft_id: String = row.get(0)?;
        let owner_id: String = row.get(1)?;
        let principal_id: String = row.get(2)?;
        let workspace_id: String = row.get(3)?;
        let topic_id: Option<String> = row.get(4)?;
        let task_id: Option<String> = row.get(5)?;
        let source_message_event_ids_json: String = row.get(6)?;
        let source_memory_event_ids_json: String = row.get(7)?;
        let evidence_hashes_json: String = row.get(8)?;
        let target_resource_json: String = row.get(9)?;
        let proposed_change_json: String = row.get(10)?;
        let risk_level: String = row.get(11)?;
        let mode: String = row.get(12)?;
        let created_at_raw: String = row.get(13)?;
        let created_by_runtime: String = row.get(14)?;
        let judge_verdict_json: Option<String> = row.get(15)?;
        let applied_at_raw: Option<String> = row.get(16)?;
        let applied_by: Option<String> = row.get(17)?;
        let rollback_anchor_json: Option<String> = row.get(18)?;

        Ok((move || {
            let created_at = DateTime::parse_from_rfc3339(&created_at_raw)?.with_timezone(&Utc);
            let applied_at = match applied_at_raw.as_deref() {
                Some(raw) => Some(DateTime::parse_from_rfc3339(raw)?.with_timezone(&Utc)),
                None => None,
            };
            ProposalRowValues::decode(
                draft_id,
                owner_id,
                principal_id,
                workspace_id,
                topic_id,
                task_id,
                &source_message_event_ids_json,
                &source_memory_event_ids_json,
                &evidence_hashes_json,
                &target_resource_json,
                &proposed_change_json,
                &risk_level,
                &mode,
                created_at,
                created_by_runtime,
                judge_verdict_json.as_deref(),
                applied_at,
                applied_by,
                rollback_anchor_json.as_deref(),
            )
        })())
    }

    fn memory_event_from_row(row: &Row<'_>) -> rusqlite::Result<MemoryEvent> {
        let visibility_raw: String = row.get(12)?;
        Ok(MemoryEvent {
            id: row.get(0)?,
            event_id: row.get(1)?,
            workspace_id: row.get(2)?,
            owner_id: row.get(3)?,
            event_type: row.get(4)?,
            subject_table: row.get(5)?,
            subject_id: row.get(6)?,
            session_key: row.get(7)?,
            run_id: row.get(8)?,
            parent_run_id: row.get(9)?,
            agent_id: row.get(10)?,
            persona_id: row.get(11)?,
            visibility: visibility_raw.parse().unwrap_or(MemoryVisibility::Workspace),
            payload_json: row.get(13)?,
            created_at: row.get(14)?,
        })
    }

    fn memory_draft_from_row(row: &Row<'_>) -> rusqlite::Result<MemoryDraft> {
        let category_raw: String = row.get(11)?;
        let visibility_raw: String = row.get(13)?;
        Ok(MemoryDraft {
            id: row.get(0)?,
            draft_id: row.get(1)?,
            workspace_id: row.get(2)?,
            owner_id: row.get(3)?,
            worker_run_id: row.get(4)?,
            parent_run_id: row.get(5)?,
            session_key: row.get(6)?,
            agent_id: row.get(7)?,
            persona_id: row.get(8)?,
            key: row.get(9)?,
            content: row.get(10)?,
            category: Self::str_to_category(&category_raw),
            source_event_id: row.get(12)?,
            visibility: visibility_raw.parse().unwrap_or(MemoryVisibility::Workspace),
            status: row.get(14)?,
            payload_json: row.get(15)?,
            created_at: row.get(16)?,
            updated_at: row.get(17)?,
        })
    }

    fn document_from_row(row: &Row<'_>) -> rusqlite::Result<DocumentRecord> {
        let visibility_raw: String = row.get(12)?;
        let chunk_count_raw: i64 = row.get(14)?;
        Ok(DocumentRecord {
            id: row.get(0)?,
            document_id: row.get(1)?,
            workspace_id: row.get(2)?,
            owner_id: row.get(3)?,
            topic_id: row.get(4)?,
            task_id: row.get(5)?,
            source_message_event_id: row.get(6)?,
            source_kind: row.get(7)?,
            source_uri: row.get(8)?,
            title: row.get(9)?,
            content_sha256: row.get(10)?,
            mime_type: row.get(11)?,
            visibility: visibility_raw.parse().unwrap_or(MemoryVisibility::Workspace),
            metadata_json: row.get(13)?,
            chunk_count: usize::try_from(chunk_count_raw).unwrap_or(0),
            created_at: row.get(15)?,
            updated_at: row.get(16)?,
        })
    }

    fn document_chunk_from_row(row: &Row<'_>) -> rusqlite::Result<DocumentChunkRecord> {
        let chunk_index_raw: i64 = row.get(7)?;
        let token_estimate_raw: i64 = row.get(12)?;
        Ok(DocumentChunkRecord {
            id: row.get(0)?,
            chunk_id: row.get(1)?,
            document_id: row.get(2)?,
            workspace_id: row.get(3)?,
            owner_id: row.get(4)?,
            topic_id: row.get(5)?,
            task_id: row.get(6)?,
            chunk_index: usize::try_from(chunk_index_raw).unwrap_or(0),
            heading: row.get(8)?,
            content: row.get(9)?,
            content_sha256: row.get(10)?,
            source_anchor: row.get(11)?,
            token_estimate: usize::try_from(token_estimate_raw).unwrap_or(0),
            created_at: row.get(13)?,
        })
    }

    fn memory_link_from_row(row: &Row<'_>) -> rusqlite::Result<MemoryLink> {
        Ok(MemoryLink {
            id: row.get(0)?,
            link_id: row.get(1)?,
            workspace_id: row.get(2)?,
            owner_id: row.get(3)?,
            memory_key: row.get(4)?,
            memory_event_id: row.get(5)?,
            message_event_id: row.get(6)?,
            document_id: row.get(7)?,
            chunk_id: row.get(8)?,
            link_type: row.get(9)?,
            payload_json: row.get(10)?,
            created_at: row.get(11)?,
        })
    }

    fn retrieval_trace_from_row(row: &Row<'_>) -> rusqlite::Result<RetrievalTrace> {
        let candidate_count_raw: i64 = row.get(9)?;
        let selected_count_raw: i64 = row.get(10)?;
        let dropped_count_raw: i64 = row.get(11)?;
        let budget_tokens_raw: Option<i64> = row.get(12)?;
        Ok(RetrievalTrace {
            id: row.get(0)?,
            trace_id: row.get(1)?,
            workspace_id: row.get(2)?,
            owner_id: row.get(3)?,
            session_key: row.get(4)?,
            agent_id: row.get(5)?,
            persona_id: row.get(6)?,
            source: row.get(7)?,
            query: row.get(8)?,
            candidate_count: usize::try_from(candidate_count_raw).unwrap_or(0),
            selected_count: usize::try_from(selected_count_raw).unwrap_or(0),
            dropped_count: usize::try_from(dropped_count_raw).unwrap_or(0),
            budget_tokens: budget_tokens_raw.and_then(|value| usize::try_from(value).ok()),
            selected_json: row.get(13)?,
            dropped_json: row.get(14)?,
            payload_json: row.get(15)?,
            created_at: row.get(16)?,
        })
    }

    fn compaction_run_from_row(row: &Row<'_>) -> rusqlite::Result<CompactionRun> {
        let source_message_count_raw: i64 = row.get(9)?;
        let source_token_estimate_raw: i64 = row.get(10)?;
        Ok(CompactionRun {
            id: row.get(0)?,
            run_id: row.get(1)?,
            workspace_id: row.get(2)?,
            owner_id: row.get(3)?,
            session_key: row.get(4)?,
            agent_id: row.get(5)?,
            persona_id: row.get(6)?,
            trigger: row.get(7)?,
            mode: row.get(8)?,
            source_message_count: usize::try_from(source_message_count_raw).unwrap_or(0),
            source_token_estimate: usize::try_from(source_token_estimate_raw).unwrap_or(0),
            summary: row.get(11)?,
            summary_memory_key: row.get(12)?,
            source_event_ids_json: row.get(13)?,
            source_event_range_json: row.get(14)?,
            source_document_refs_json: row.get(15)?,
            fidelity_status: row.get(16)?,
            payload_json: row.get(17)?,
            created_at: row.get(18)?,
        })
    }

    fn backup_file_path(db_path: &Path, category: &MemoryCategory) -> PathBuf {
        let memory_dir = db_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let workspace_dir = if memory_dir.file_name().and_then(|n| n.to_str()) == Some("memory") {
            memory_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| memory_dir.clone())
        } else {
            memory_dir.clone()
        };

        match category {
            MemoryCategory::Core => workspace_dir.join("MEMORY.md"),
            _ => {
                let date = Local::now().format("%Y-%m-%d").to_string();
                memory_dir.join(format!("{date}.md"))
            }
        }
    }

    fn append_backup_entry(db_path: &Path, key: &str, content: &str, category: &MemoryCategory) {
        let path = Self::backup_file_path(db_path, category);
        if let Some(parent) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                tracing::warn!("memory backup mkdir failed ({}): {error}", parent.display());
                return;
            }
        }

        // NOTE: TOCTOU benign — the exists() check only decides whether to
        // prepend a header. The file is opened with O_CREAT | O_APPEND below,
        // so a race at worst produces a missing or duplicate header (cosmetic).
        let header = if path.exists() {
            None
        } else if matches!(category, MemoryCategory::Core) {
            Some("# Long-Term Memory\n\n".to_string())
        } else {
            let date = Local::now().format("%Y-%m-%d").to_string();
            Some(format!("# Daily Log — {date}\n\n"))
        };

        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
        // R-MCPB F-1: the key and the content are escaped so a writer can
        // neither start a line of its own nor embed another key's entry line.
        let key = Self::mirror_line_text(key);
        let body = format!(
            "- [{timestamp}] **{key}**: {}\n",
            Self::mirror_line_text(content.trim())
        );

        if matches!(category, MemoryCategory::Core) {
            if let Err(error) = Self::upsert_core_backup_entry(&path, &key, &body) {
                tracing::warn!("core memory backup update failed ({}): {error}", path.display());
            }
            return;
        }

        let payload = match header {
            Some(header) => format!("{header}{body}"),
            None => body,
        };

        let open_result = OpenOptions::new().create(true).append(true).open(&path);
        match open_result {
            Ok(mut file) => {
                if let Err(error) = file.write_all(payload.as_bytes()) {
                    tracing::warn!("memory backup append failed ({}): {error}", path.display());
                }
            }
            Err(error) => {
                tracing::warn!("memory backup open failed ({}): {error}", path.display());
            }
        }
    }

    /// Keep `MEMORY.md` as a projection of the latest value for each generated
    /// key. Daily logs remain append-only, but core memory keys are upserts in
    /// SQLite and must not grow a second, stale copy on every scheduled write.
    fn upsert_core_backup_entry(path: &Path, key: &str, body: &str) -> anyhow::Result<()> {
        let lock_path = path.with_extension("md.lock");
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        fs2::FileExt::lock_exclusive(&lock_file)?;

        let existing = std::fs::read_to_string(path).unwrap_or_else(|_| "# Long-Term Memory\n\n".to_string());
        let mut blocks: Vec<(Option<String>, String)> = Vec::new();
        for line in existing.split_inclusive('\n') {
            let entry_key = Self::generated_backup_entry_key(line).map(str::to_string);
            if entry_key.is_some() || blocks.is_empty() {
                blocks.push((entry_key, line.to_string()));
            } else if let Some((_, block)) = blocks.last_mut() {
                block.push_str(line);
            }
        }

        let mut last_by_key = std::collections::HashMap::new();
        for (index, (block_key, _)) in blocks.iter().enumerate() {
            if let Some(block_key) = block_key {
                last_by_key.insert(block_key.as_str(), index);
            }
        }

        let mut rewritten = String::with_capacity(existing.len().saturating_add(body.len()));
        for (index, (block_key, block)) in blocks.iter().enumerate() {
            match block_key {
                None => rewritten.push_str(block),
                Some(block_key) if block_key != key && last_by_key.get(block_key.as_str()) == Some(&index) => {
                    rewritten.push_str(block);
                }
                Some(_) => {}
            }
        }
        if !rewritten.ends_with('\n') {
            rewritten.push('\n');
        }
        rewritten.push_str(body);

        let temp_path = path.with_extension(format!("md.tmp.{}", Uuid::new_v4()));
        std::fs::write(&temp_path, rewritten)?;
        std::fs::rename(&temp_path, path)?;
        fs2::FileExt::unlock(&lock_file)?;
        Ok(())
    }

    /// Text as it may appear inside one generated Markdown mirror line.
    ///
    /// The mirror (`MEMORY.md`, daily logs) is keyed by lines of the form
    /// `- [..] **<key>**: <content>` and `MEMORY.md` is inlined into system
    /// prompts, so mirrored text must stay on its own line and must not contain
    /// the `**` that delimits a key. Backslashes, line breaks (`\n`, `\r`,
    /// `\r\n`), `*`, and U+2028, U+2029, NEL, VT and FF (as `\u{..}`) are
    /// escaped; the escaped form is unambiguous and still readable.
    fn mirror_line_text(text: &str) -> String {
        let mut escaped = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(character) = chars.next() {
            match character {
                '\\' => escaped.push_str("\\\\"),
                '*' => escaped.push_str("\\*"),
                '\n' => escaped.push_str("\\n"),
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    escaped.push_str("\\n");
                }
                // Separators a model may read as a line break (R-MCPBfg N-3).
                // Writing to a `String` cannot fail.
                '\u{2028}' | '\u{2029}' | '\u{0085}' | '\u{000B}' | '\u{000C}' => {
                    let _ = write!(escaped, "\\u{{{:04x}}}", u32::from(character));
                }
                other => escaped.push(other),
            }
        }
        escaped
    }

    fn generated_backup_entry_key(line: &str) -> Option<&str> {
        let rest = line.strip_prefix("- [")?;
        let (_, rest) = rest.split_once("] **")?;
        let (key, _) = rest.split_once("**:")?;
        (!key.is_empty()).then_some(key)
    }

    async fn store_internal(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        metadata: Option<MemoryStoreMetadata>,
    ) -> anyhow::Result<()> {
        validate_memory_write_target(key, session_id)?;
        // V02-R9: an owner-isolated write is confined to the caller's own keys.
        // `Some(owner)` only for an isolated context; a blank owner id refuses
        // here, before anything is written.
        let isolated_owner = crate::memory::owner_predicate::isolated_store_owner(context)?;
        self.check_memory_safety(content, context).await?;

        // Only long-lived categories need vector embeddings.
        let needs_embedding = matches!(&category, MemoryCategory::Core | MemoryCategory::Custom(_));
        let embedding_bytes = if needs_embedding {
            self.get_or_compute_embedding(content)
                .await?
                .map(|emb| vector::vec_to_bytes(&emb))
        } else {
            None
        };
        let embedding_provider = embedding_bytes.as_ref().map(|_| self.embedding_provider_name());
        let embedding_model = embedding_bytes.as_ref().map(|_| self.embedding_model_name());
        let embedding_dimensions = embedding_bytes.as_ref().map(|_| self.embedding_dimensions_i64());

        let pool = self.pool.clone();
        let key = key.to_string();
        let content = content.to_string();
        let sid = session_id.map(String::from);
        let write_ctx = context.cloned();
        let mut metadata = metadata.unwrap_or_default();
        if let Some(owner) = isolated_owner.as_ref() {
            // The row an isolated caller writes is attributed to its
            // authenticated owner and nothing else.
            metadata.owner_id = Some(owner.clone());
        }
        let owner_guard =
            crate::memory::owner_predicate::store_conflict_guard(SQLITE_DIALECT, "memories", isolated_owner.is_some());

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = pool.write();
            // R-MCPB F-4: an owner-isolated write — ownership check, guarded
            // upsert and guarded metadata update — is one immediate
            // transaction, so a refusal at any step leaves nothing behind.
            // Non-isolated writes keep their historical autocommit statements.
            let isolated_tx = isolated_owner
                .as_ref()
                .map(|_| rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate))
                .transpose()?;
            let now = Local::now().to_rfc3339();
            let cat = Self::category_to_str(&category);
            let id = Uuid::new_v4().to_string();

            if let Some(ctx) = write_ctx {
                // Checked before topic resolution so a refused write leaves no
                // trace in the topic tables. The conflict guard below is the
                // atomic backstop: another process (a session worker) may
                // write the same database between this read and the upsert.
                if let Some(owner) = isolated_owner.as_deref() {
                    let stored_owner: Option<Option<String>> = conn
                        .query_row(
                            "SELECT owner_id FROM memories WHERE key = ?1",
                            params![&key],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if stored_owner.is_some_and(|stored| stored.as_deref() != Some(owner)) {
                        return Err(crate::memory::owner_predicate::OwnerScopedStoreRefusal::KeyNotAvailable {
                            key: key.clone(),
                        }
                        .into());
                    }
                }
                let principal = sqlite_read_principal(&conn, &ctx);
                let classified = classify_memory(&ctx, &content, &principal);
                let topic_id = match resolve_topic(&conn, &content, &principal) {
                    Ok(topic_id) => topic_id,
                    Err(error) => {
                        tracing::warn!("memory topic resolve failed: {error}");
                        None
                    }
                };
                let risk_json = serde_json::to_string(&classified.risk_signals)?;
                let sender_id = if ctx.channel.is_some() && ctx.raw_sender.is_some() {
                    Some(principal.user_id)
                } else {
                    None
                };
                let explicit_sender_id = ctx.sender_id.or(sender_id);
                let owner_id = metadata.owner_id.clone().or_else(|| explicit_sender_id.clone());
                let chat_type = ctx
                    .chat_type
                    .map(|raw| ChatType::from_str(&raw).as_str().to_string());

                let changed = conn.execute(
                    &format!("INSERT INTO memories (
                        id, key, content, category, embedding, embedding_provider,
                        embedding_model, embedding_dimensions, created_at, updated_at,
                        session_id, workspace_id, owner_id, channel, chat_type, chat_id,
                        sender_id, raw_sender, topic_id, visibility, sensitivity,
                        risk_signals, policy_version
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)
                     ON CONFLICT(key) DO UPDATE SET
                        content = excluded.content,
                        category = excluded.category,
                        embedding = excluded.embedding,
                        embedding_provider = excluded.embedding_provider,
                        embedding_model = excluded.embedding_model,
                        embedding_dimensions = excluded.embedding_dimensions,
                        updated_at = excluded.updated_at,
                        session_id = excluded.session_id,
                        workspace_id = excluded.workspace_id,
                        owner_id = excluded.owner_id,
                        channel = excluded.channel,
                        chat_type = excluded.chat_type,
                        chat_id = excluded.chat_id,
                        sender_id = excluded.sender_id,
                        raw_sender = excluded.raw_sender,
                        topic_id = excluded.topic_id,
                        visibility = excluded.visibility,
                        sensitivity = excluded.sensitivity,
                        risk_signals = excluded.risk_signals,
                        policy_version = excluded.policy_version{owner_guard}"),
                    params![
                        id,
                        &key,
                        &content,
                        cat,
                        embedding_bytes,
                        embedding_provider,
                        embedding_model,
                        embedding_dimensions,
                        now,
                        now,
                        sid,
                        metadata.workspace_id.clone(),
                        owner_id,
                        ctx.channel,
                        chat_type,
                        ctx.chat_id,
                        explicit_sender_id,
                        ctx.raw_sender,
                        topic_id,
                        classified.visibility.as_str(),
                        classified.sensitivity.as_str(),
                        risk_json,
                        classified.policy_version,
                    ],
                )?;
                // The guard suppressed the update: the key exists under
                // another owner (or none). Nothing was written.
                if isolated_owner.is_some() && changed == 0 {
                    return Err(crate::memory::owner_predicate::OwnerScopedStoreRefusal::KeyNotAvailable {
                        key: key.clone(),
                    }
                    .into());
                }
            } else {
                conn.execute(
                    "INSERT INTO memories (
                        id, key, content, category, embedding, embedding_provider,
                        embedding_model, embedding_dimensions, created_at, updated_at,
                        session_id, workspace_id, owner_id, agent_id, persona_id,
                        source_event_id, source, channel, topic_id
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
                     ON CONFLICT(key) DO UPDATE SET
                        content = excluded.content,
                        category = excluded.category,
                        embedding = excluded.embedding,
                        embedding_provider = excluded.embedding_provider,
                        embedding_model = excluded.embedding_model,
                        embedding_dimensions = excluded.embedding_dimensions,
                        updated_at = excluded.updated_at,
                        session_id = excluded.session_id,
                        workspace_id = excluded.workspace_id,
                        owner_id = excluded.owner_id,
                        agent_id = excluded.agent_id,
                        persona_id = excluded.persona_id,
                        source_event_id = excluded.source_event_id,
                        source = excluded.source,
                        channel = excluded.channel,
                        topic_id = excluded.topic_id",
                    params![
                        id,
                        &key,
                        &content,
                        cat,
                        embedding_bytes,
                        embedding_provider,
                        embedding_model,
                        embedding_dimensions,
                        now,
                        now,
                        sid,
                        metadata.workspace_id.clone(),
                        metadata.owner_id.clone(),
                        metadata.agent_id.clone(),
                        metadata.persona_id.clone(),
                        metadata.source_event_id.clone(),
                        metadata.source.clone(),
                        // FIX-P1-08: persist the originating channel so anonymous
                        // principals can resolve channel scope on later recall.
                        metadata.channel.clone(),
                        metadata.topic_id.clone(),
                    ],
                )?;
            }

            if metadata.workspace_id.is_some()
                || metadata.owner_id.is_some()
                || metadata.agent_id.is_some()
                || metadata.persona_id.is_some()
                || metadata.source_event_id.is_some()
                || metadata.source.is_some()
            {
                // R-MCPB F-4: for an owner-isolated write the update is
                // confined to the caller's own row (`?2` is its owner id), so
                // a row another writer changed after the upsert is neither
                // re-owned nor rewritten; that write is refused and rolled
                // back instead.
                let metadata_owner_guard = if isolated_owner.is_some() {
                    " AND owner_id IS ?2"
                } else {
                    ""
                };
                let updated = conn.execute(
                    &format!(
                        "UPDATE memories
                     SET workspace_id = ?1,
                         owner_id = COALESCE(?2, owner_id),
                         agent_id = ?3,
                         persona_id = ?4,
                         source_event_id = ?5,
                         source = ?6,
                         updated_at = ?7
                     WHERE key = ?8{metadata_owner_guard}"
                    ),
                    params![
                        metadata.workspace_id,
                        metadata.owner_id,
                        metadata.agent_id,
                        metadata.persona_id,
                        metadata.source_event_id,
                        metadata.source,
                        Local::now().to_rfc3339(),
                        &key,
                    ],
                )?;
                if isolated_owner.is_some() && updated == 0 {
                    return Err(crate::memory::owner_predicate::OwnerScopedStoreRefusal::KeyNotAvailable {
                        key: key.clone(),
                    }
                    .into());
                }
            }
            if let Some(tx) = isolated_tx {
                tx.commit()?;
            }

            // D-026: no write is mirrored into the workspace Markdown files
            // here. `MEMORY.md` is inlined into every surface's system prompt,
            // so only a write the local operator made is mirrored, explicitly,
            // through `Memory::mirror_operator_write`.
            Ok(())
        })
        .await?
    }

    async fn check_memory_safety(&self, content: &str, context: Option<&MemoryWriteContext>) -> anyhow::Result<()> {
        let source = SourceMetadata {
            actor: Self::safety_actor_for_context(context),
            historical_accuracy: None,
        };
        let result = MemorySafetyFilter::default().check(content, &source).await;
        if result.passed {
            Ok(())
        } else {
            anyhow::bail!("{}", safety_rejection_message(&result.issues));
        }
    }

    const fn safety_actor_for_context(context: Option<&MemoryWriteContext>) -> Actor {
        match context {
            Some(ctx) if ctx.raw_sender.is_some() || ctx.sender_id.is_some() => Actor::User,
            Some(_) => Actor::Agent,
            None => Actor::Agent,
        }
    }

    /// Deterministic content hash for embedding cache.
    /// Uses SHA-256 (truncated) instead of DefaultHasher, which is
    /// explicitly documented as unstable across Rust versions.
    fn content_hash(text: &str) -> String {
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(text.as_bytes());
        // First 16 bytes → 32 hex chars. 128-bit hash reduces collision risk
        // to negligible levels (~2^-64 birthday bound vs ~2^-32 with 64-bit).
        let mut hex = String::with_capacity(32);
        // SAFETY: SHA-256 output is always 32 bytes, so ..16 is always valid
        #[allow(clippy::indexing_slicing)]
        for byte in &hash[..16] {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    fn content_sha256_hex(text: &str) -> String {
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(text.as_bytes());
        let mut hex = String::with_capacity(64);
        for byte in hash {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    fn document_owner_for_principal(principal: &MemoryPrincipal) -> Option<String> {
        // C03 / V02-R4: an owner-isolated principal's channel/sender pair is the
        // caller-chosen external subject alone, so deriving the document owner
        // from it would let two issuers sharing a subject read each other's
        // private documents. Use the authenticated owner id instead; when it is
        // absent the principal admits no owner-scoped document at all.
        if principal.require_owner_match {
            return crate::memory::owner_predicate::has_owner_id(principal)
                .then(|| principal.owner_id.clone())
                .flatten();
        }
        let channel = principal.channel.as_deref()?.trim();
        let sender = principal.sender.as_deref()?.trim();
        if channel.is_empty() || sender.is_empty() {
            return None;
        }
        Some(
            OwnerPrincipal::new(
                principal.workspace_id.clone(),
                channel,
                sender,
                principal.session_key.clone().unwrap_or_default(),
                vec![Role::Anonymous],
            )
            .owner_id,
        )
    }

    /// The guard appended to the *visibility* disjunct of every document chunk
    /// search statement (V02-R8).
    ///
    /// Those statements admit a chunk when the owning document is `global` or
    /// `workspace` **or** when the chunk's owner matches the caller. The first
    /// disjunct short-circuits the second, and `documents.visibility` defaults
    /// to `workspace`, so an owner-isolated caller was handed a snippet of every
    /// other owner's documents — including the conversation transcript the
    /// OS-paging evictor ingests verbatim.
    ///
    /// `document_search` renders chunk content to its caller, so it has to agree
    /// with `document_get_chunk`'s `chunk_visible_to_principal`, which is
    /// already the intended semantic: an owner-isolated principal reads a chunk
    /// only when the chunk's `owner_id` equals its authenticated owner id. A
    /// `global` or `workspace` document owned by someone else and an
    /// unattributed chunk are both hidden, and a caller with no owner id reaches
    /// nothing. The owner disjunct beside this guard already expresses exactly
    /// that test, so the whole correction is to stop the visibility disjunct
    /// short-circuiting it.
    ///
    /// The fragment comes from
    /// [`crate::memory::owner_predicate::owner_match_suffix`] — the single
    /// owner-isolation seam — asked for an arm that carries no owner of its own
    /// (`has_owner = false`), which is precisely what this arm is: a document's
    /// visibility proves nothing about who owns the chunk. It therefore renders
    /// the empty string for a non-isolated principal, leaving the statement and
    /// its bind list byte-identical, and `" AND FALSE"` for an isolated one.
    /// It binds no parameter in either case, so the placeholder accounting of
    /// these statements is unchanged.
    fn document_visibility_guard(principal: &MemoryPrincipal) -> String {
        // Unused: this arm never binds, because it never has an owner to bind.
        const UNBOUND_INDEX: usize = 1;
        crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            UNBOUND_INDEX,
            principal.require_owner_match,
            false,
        )
        .sql
    }

    /// The owner id an owner-isolated delete is confined to, with blanks
    /// treated as absent (the rule
    /// [`crate::memory::owner_predicate::has_owner_id`] applies to reads).
    ///
    /// A non-isolated context never yields one, so the guard below renders
    /// nothing for it and the historical statement is unchanged.
    fn forget_owner_anchor(context: &MemoryWriteContext) -> Option<String> {
        if !context.require_owner_match {
            return None;
        }
        context
            .owner_id
            .as_deref()
            .map(str::trim)
            .filter(|owner| !owner.is_empty())
            .map(str::to_string)
    }

    /// The guard appended to `forget_with_context`'s visibility scope
    /// (R-B1d D-1).
    ///
    /// Delete-by-key was confined to the shared visibility predicate alone.
    /// That predicate owner-qualifies every **non-public** arm for an isolated
    /// caller, but `visibility = 'public'` stays shared by design — correct for
    /// a read, wrong for a destructive operation, because it let an
    /// owner-isolated caller remove a public row belonging to someone else.
    /// This guard is therefore appended at the top level of the `WHERE` rather
    /// than inside one arm: an owner-isolated caller deletes only rows whose
    /// `owner_id` equals its authenticated owner id, whatever their visibility.
    ///
    /// The fragment comes from
    /// [`crate::memory::owner_predicate::owner_match_suffix`] — the single
    /// owner-isolation seam — so it renders the empty string for a
    /// non-isolated caller (statement and bind list byte-identical),
    /// `" AND FALSE"` for an isolated caller with no usable owner id (nothing
    /// is deleted and the call reports that nothing was found), and
    /// `" AND owner_id = ?N"` otherwise. A stored `NULL` `owner_id` compares to
    /// `NULL`, so an unattributed row is not deletable by an isolated caller
    /// either. `index` is the caller's next free placeholder, so the
    /// one-marker-one-parameter accounting is preserved.
    fn forget_owner_guard(
        context: &MemoryWriteContext,
        index: usize,
    ) -> crate::memory::owner_predicate::OwnerMatchPredicate {
        crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            index,
            context.require_owner_match,
            Self::forget_owner_anchor(context).is_some(),
        )
    }

    fn embedding_provider_name(&self) -> String {
        self.embedder.name().to_string()
    }

    fn embedding_model_name(&self) -> String {
        self.embedder.model().to_string()
    }

    fn embedding_dimensions_i64(&self) -> i64 {
        i64::try_from(self.embedder.dimensions()).unwrap_or(i64::MAX)
    }

    /// Get embedding from cache, or compute + cache it
    async fn get_or_compute_embedding(&self, text: &str) -> anyhow::Result<Option<Vec<f32>>> {
        if self.embedder.dimensions() == 0 {
            return Ok(None); // Noop embedder
        }

        let hash = Self::content_hash(text);
        let now = Local::now().to_rfc3339();
        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();

        // Check cache (offloaded to blocking thread)
        let pool = self.pool.clone();
        let hash_c = hash.clone();
        let now_c = now.clone();
        let provider_c = provider_name.clone();
        let model_c = model_name.clone();
        let cached = crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<Vec<f32>>> {
            let conn = pool.write();
            let mut stmt = conn.prepare(
                "SELECT embedding FROM embedding_cache
                 WHERE content_hash = ?1
                   AND provider = ?2
                   AND model = ?3
                   AND dimensions = ?4",
            )?;
            let blob: Option<Vec<u8>> = stmt
                .query_row(params![hash_c, provider_c, model_c, dimensions], |row| row.get(0))
                .ok();
            if let Some(bytes) = blob {
                // FIX-P0-26: touch the exact composite-key row, not every row
                // sharing this content_hash.
                conn.execute(
                    "UPDATE embedding_cache SET accessed_at = ?1 \
                     WHERE content_hash = ?2 AND provider = ?3 AND model = ?4 AND dimensions = ?5",
                    params![now_c, hash_c, provider_c, model_c, dimensions],
                )?;
                let embedding = vector::bytes_to_vec(&bytes);
                if embedding.len() == usize::try_from(dimensions).unwrap_or(usize::MAX) {
                    return Ok(Some(embedding));
                }
            }
            Ok(None)
        })
        .await??;

        if cached.is_some() {
            return Ok(cached);
        }

        // Compute the embedding. Remote providers await network I/O here; the
        // default local provider is synchronous CPU work that offloads itself to
        // the blocking pool once a batch is large enough to matter.
        let embedding = self.embedder.embed_one(text).await?;
        if embedding.len() != self.embedder.dimensions() {
            anyhow::bail!(
                "embedding dimension mismatch: provider={} model={} expected={} got={}",
                provider_name,
                model_name,
                self.embedder.dimensions(),
                embedding.len()
            );
        }
        let bytes = vector::vec_to_bytes(&embedding);

        // Store in cache + LRU eviction (offloaded to blocking thread)
        let pool = self.pool.clone();
        #[allow(clippy::cast_possible_wrap)]
        let cache_max = self.cache_max as i64;
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = pool.write();
            conn.execute(
                "INSERT OR REPLACE INTO embedding_cache (
                    content_hash, embedding, provider, model, dimensions, created_at, accessed_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![hash, bytes, provider_name, model_name, dimensions, now, now],
            )?;
            // Two-step LRU eviction: count first, then delete oldest if over limit.
            // Avoids relying on MAX() as a scalar function in a LIMIT clause.
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM embedding_cache", [], |row| row.get(0))?;
            if count > cache_max {
                let to_delete = count - cache_max;
                conn.execute(
                    "DELETE FROM embedding_cache WHERE content_hash IN (
                        SELECT content_hash FROM embedding_cache
                        ORDER BY accessed_at ASC LIMIT ?1
                    )",
                    params![to_delete],
                )?;
            }
            Ok(())
        })
        .await??;

        Ok(Some(embedding))
    }

    /// FTS5 BM25 keyword search
    fn fts5_search(conn: &Connection, query: &str, limit: usize) -> anyhow::Result<Vec<(String, f32)>> {
        // Escape FTS5 special chars and build query
        let fts_query: String = super::topic::build_safe_fts_query(query);

        if fts_query.is_empty() {
            return Ok(Vec::new());
        }

        let sql = "SELECT m.id, bm25(memories_fts) as score
                   FROM memories_fts f
                   JOIN memories m ON m.rowid = f.rowid
                   WHERE memories_fts MATCH ?1
                   ORDER BY score
                   LIMIT ?2";

        let mut stmt = conn.prepare(sql)?;
        #[allow(clippy::cast_possible_wrap)]
        let limit_i64 = limit as i64;

        let rows = stmt.query_map(params![fts_query, limit_i64], |row| {
            let id: String = row.get(0)?;
            let score: f64 = row.get(1)?;
            // BM25 returns negative scores (lower = better), negate for ranking
            #[allow(clippy::cast_possible_truncation)]
            Ok((id, (-score) as f32))
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Vector similarity search: scan embeddings and compute cosine similarity.
    ///
    /// Optional `category` and `session_id` filters reduce full-table scans
    /// when the caller already knows the scope of relevant memories.
    ///
    /// Cosine similarity has no index, so the candidate set is bounded before
    /// scoring: without a bound this is O(N) over every embedded memory on a
    /// path that runs once per turn.
    ///
    /// The bound orders by `update_seq`, not by `updated_at` and not by
    /// `rowid`.
    ///
    /// `updated_at` is `Local::now().to_rfc3339()` — an RFC 3339 string carrying
    /// the writer's UTC offset — and it is ordered lexicographically by SQLite.
    /// Within one fixed offset that happens to match chronological order, but a
    /// DST transition, a machine timezone change, or a store shared between
    /// processes in different zones reorders whole blocks of rows, and any row
    /// historically written with `Utc::now()` sorts in a third order again. The
    /// rows dropped by `LIMIT` would then be the wrong ones.
    ///
    /// `rowid` cannot be reordered by a clock, but it does not move either:
    /// every writer here upserts with `ON CONFLICT(key) DO UPDATE`, which
    /// rewrites the content and the embedding of an existing key in place. A
    /// long-lived key refreshed a second ago therefore kept its original
    /// position, and in a store larger than the cap it stayed outside the
    /// window — the memory whose embedding was *just* made relevant was the one
    /// that could not be scored.
    ///
    /// `update_seq` is a per-store counter bumped by a trigger on every insert
    /// and every update (see `init_schema`). It is an integer, so it has no
    /// timezone, no offset and no DST; it is monotonic, so it cannot be
    /// reordered by a clock; and it follows the row when the row is refreshed.
    /// "Newest" here therefore means most recently *written*, created or
    /// refreshed. `rowid` breaks ties so the window is deterministic on a store
    /// whose sequence was backfilled by the migration.
    ///
    /// `candidate_cap` is the configured `[memory] vector_candidate_cap`. Hitting it is
    /// a real recall regression — an old-but-relevant memory is never scored —
    /// so it is reported once per process at `warn`, not at `debug`.
    fn vector_search(
        conn: &Connection,
        query_embedding: &[f32],
        provider: &str,
        model: &str,
        dimensions: usize,
        limit: usize,
        category: Option<&str>,
        session_id: Option<&str>,
        candidate_cap: usize,
    ) -> anyhow::Result<Vec<(String, f32)>> {
        let VectorCandidateQuery {
            sql,
            params,
            candidate_cap,
        } = vector_candidate_query(provider, model, dimensions, limit, category, session_id, candidate_cap);

        let mut stmt = conn.prepare(&sql)?;
        let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(AsRef::as_ref).collect();
        let rows = stmt.query_map(params_ref.as_slice(), |row| {
            let id: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            Ok((id, blob))
        })?;

        let mut scored: Vec<(String, f32)> = Vec::new();
        let mut candidates = 0usize;
        for row in rows {
            let (id, blob) = row?;
            candidates += 1;
            let emb = vector::bytes_to_vec(&blob);
            if emb.len() != dimensions {
                tracing::debug!(
                    memory_id = %id,
                    expected_dimensions = dimensions,
                    actual_dimensions = emb.len(),
                    "Skipping stale memory embedding with mismatched dimensions"
                );
                continue;
            }
            let sim = vector::cosine_similarity(query_embedding, &emb);
            if sim > 0.0 {
                scored.push((id, sim));
            }
        }

        if candidates >= candidate_cap && !VECTOR_CANDIDATE_CAP_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!(
                candidate_cap,
                limit,
                "Vector recall hit its candidate cap: older embedded memories are no longer scored and \
                 cannot be recalled. Raise [memory] vector_candidate_cap to score more of the store."
            );
        }

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored)
    }

    /// Safe reindex: rebuild FTS5 + embeddings with rollback on failure
    pub async fn reindex(&self) -> anyhow::Result<usize> {
        // Step 1: Rebuild FTS5
        {
            let pool = self.pool.clone();
            crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
                let conn = pool.write();
                conn.execute_batch(
                    "INSERT INTO memories_fts(memories_fts) VALUES('rebuild');
                     INSERT INTO document_chunks_fts(document_chunks_fts) VALUES('rebuild');",
                )?;
                Ok(())
            })
            .await??;
        }

        // Step 2: Re-embed eligible memories that lack embeddings
        if self.embedder.dimensions() == 0 {
            return Ok(0);
        }

        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();
        let pool = self.pool.clone();
        let entries: Vec<(String, String)> = crate::runtime::blocking::spawn_blocking(move || {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(
                "SELECT id, content
                 FROM memories
                 WHERE category NOT IN ('daily', 'conversation')
                   AND (
                       embedding IS NULL
                       OR embedding_provider IS NULL
                       OR embedding_model IS NULL
                       OR embedding_dimensions IS NULL
                       OR embedding_provider != ?1
                       OR embedding_model != ?2
                       OR embedding_dimensions != ?3
                   )",
            )?;
            let rows = stmt.query_map(params![provider_name, model_name, dimensions], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            Ok::<_, anyhow::Error>(rows.filter_map(std::result::Result::ok).collect())
        })
        .await??;

        let mut count = 0;
        for (id, content) in &entries {
            if let Ok(Some(emb)) = self.get_or_compute_embedding(content).await {
                let bytes = vector::vec_to_bytes(&emb);
                let pool = self.pool.clone();
                let id = id.clone();
                let provider_name = self.embedding_provider_name();
                let model_name = self.embedding_model_name();
                let dimensions = self.embedding_dimensions_i64();
                crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
                    let conn = pool.write();
                    conn.execute(
                        "UPDATE memories
                         SET embedding = ?1,
                             embedding_provider = ?2,
                             embedding_model = ?3,
                             embedding_dimensions = ?4
                         WHERE id = ?5",
                        params![bytes, provider_name, model_name, dimensions, id],
                    )?;
                    Ok(())
                })
                .await??;
                count += 1;
            }
        }

        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();
        let pool = self.pool.clone();
        let chunks: Vec<(String, String)> = crate::runtime::blocking::spawn_blocking(move || {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(
                "SELECT chunk_id, content
                 FROM document_chunks
                 WHERE embedding IS NULL
                    OR embedding_provider IS NULL
                    OR embedding_model IS NULL
                    OR embedding_dimensions IS NULL
                    OR embedding_provider != ?1
                    OR embedding_model != ?2
                    OR embedding_dimensions != ?3",
            )?;
            let rows = stmt.query_map(params![provider_name, model_name, dimensions], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            Ok::<_, anyhow::Error>(rows.filter_map(std::result::Result::ok).collect())
        })
        .await??;

        for (chunk_id, content) in &chunks {
            if let Ok(Some(emb)) = self.get_or_compute_embedding(content).await {
                let bytes = vector::vec_to_bytes(&emb);
                let pool = self.pool.clone();
                let chunk_id = chunk_id.clone();
                let provider_name = self.embedding_provider_name();
                let model_name = self.embedding_model_name();
                let dimensions = self.embedding_dimensions_i64();
                crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
                    let conn = pool.write();
                    conn.execute(
                        "UPDATE document_chunks
                         SET embedding = ?1,
                             embedding_provider = ?2,
                             embedding_model = ?3,
                             embedding_dimensions = ?4
                         WHERE chunk_id = ?5",
                        params![bytes, provider_name, model_name, dimensions, chunk_id],
                    )?;
                    Ok(())
                })
                .await??;
                count += 1;
            }
        }

        Ok(count)
    }
}

#[async_trait]
impl Memory for SqliteMemory {
    fn name(&self) -> &str {
        "sqlite"
    }

    async fn upsert_chat_profile_metadata(
        &self,
        channel: &str,
        chat_id: &str,
        chat_kind: &str,
        title: Option<&str>,
    ) -> anyhow::Result<()> {
        let pool = self.pool.clone();
        let channel = channel.to_string();
        let chat_id = chat_id.to_string();
        let chat_kind = chat_kind.to_string();
        let title = title.map(str::to_string);
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            let now = Utc::now().to_rfc3339();
            let id = Uuid::new_v4().to_string();
            let conn = pool.write();
            conn.execute(
                "INSERT INTO chat_profiles
                    (id, channel, chat_id, chat_kind, title, purpose, notes, tags, updated_by, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, '[]', 'auto', ?6, ?6)
                 ON CONFLICT(channel, chat_id) DO UPDATE SET
                    chat_kind = excluded.chat_kind,
                    title = CASE
                        WHEN excluded.title IS NOT NULL THEN excluded.title
                        ELSE chat_profiles.title
                    END,
                    updated_at = excluded.updated_at",
                params![id, channel, chat_id, chat_kind, title, now],
            )?;
            Ok(())
        })
        .await?
    }

    async fn update_chat_profile(
        &self,
        channel: &str,
        chat_id: &str,
        chat_kind: &str,
        purpose: Option<&str>,
        notes: Option<&str>,
        tags: Option<&[String]>,
        updated_by: &str,
    ) -> anyhow::Result<ChatProfile> {
        let pool = self.pool.clone();
        let channel = channel.to_string();
        let chat_id = chat_id.to_string();
        let chat_kind = chat_kind.to_string();
        let purpose = purpose.map(str::to_string);
        let notes = notes.map(str::to_string);
        let tags_json = tags.map(serde_json::to_string).transpose()?;
        let updated_by = updated_by.to_string();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<ChatProfile> {
            let now = Utc::now().to_rfc3339();
            let id = Uuid::new_v4().to_string();
            let conn = pool.write();
            let profile = conn.query_row(
                "INSERT INTO chat_profiles
                    (id, channel, chat_id, chat_kind, title, purpose, notes, tags, updated_by, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, COALESCE(?7, '[]'), ?8, ?9, ?9)
                 ON CONFLICT(channel, chat_id) DO UPDATE SET
                    chat_kind = excluded.chat_kind,
                    purpose = COALESCE(excluded.purpose, chat_profiles.purpose),
                    notes = COALESCE(excluded.notes, chat_profiles.notes),
                    tags = COALESCE(?7, chat_profiles.tags),
                    updated_by = excluded.updated_by,
                    updated_at = excluded.updated_at
                 RETURNING id, channel, chat_id, chat_kind, title, purpose, notes, tags, updated_by, created_at, updated_at",
                params![id, channel, chat_id, chat_kind, purpose, notes, tags_json, updated_by, now],
                Self::chat_profile_from_row,
            )?;
            Ok(profile)
        })
        .await?
    }

    async fn get_chat_profile(&self, channel: &str, chat_id: &str) -> anyhow::Result<Option<ChatProfile>> {
        let pool = self.pool.clone();
        let channel = channel.to_string();
        let chat_id = chat_id.to_string();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<ChatProfile>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let profile = conn
                .query_row(
                    "SELECT id, channel, chat_id, chat_kind, title, purpose, notes, tags, updated_by, created_at, updated_at
                     FROM chat_profiles
                     WHERE channel = ?1 AND chat_id = ?2",
                    params![channel, chat_id],
                    Self::chat_profile_from_row,
                )
                .optional()?;
            Ok(profile)
        })
        .await?
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.store_internal(key, content, category, session_id, None, None)
            .await
    }

    async fn store_with_context(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<()> {
        self.store_internal(key, content, category, session_id, context, None)
            .await
    }

    async fn mirror_operator_write(&self, key: &str, content: &str, category: &MemoryCategory) -> anyhow::Result<()> {
        // With the memory ACL on the Markdown mirror is off altogether, as it
        // always was.
        if self.acl_enabled {
            return Ok(());
        }
        let db_path = self.db_path.clone();
        let key = key.to_string();
        let content = content.to_string();
        let category = category.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            Self::append_backup_entry(&db_path, &key, &content, &category);
        })
        .await?;
        Ok(())
    }

    async fn store_with_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        metadata: MemoryStoreMetadata,
    ) -> anyhow::Result<()> {
        self.store_internal(key, content, category, session_id, None, Some(metadata))
            .await
    }

    async fn store_with_context_and_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        metadata: MemoryStoreMetadata,
    ) -> anyhow::Result<()> {
        self.store_internal(key, content, category, session_id, context, Some(metadata))
            .await
    }

    async fn recall(&self, query: &str, limit: usize, session_id: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        // Compute query embedding (async, before blocking work)
        let query_embedding = self.get_or_compute_embedding(query).await?;

        let pool = self.pool.clone();
        let query = query.to_string();
        let sid = session_id.map(String::from);
        let vector_weight = self.vector_weight;
        let keyword_weight = self.keyword_weight;
        let embedding_provider = self.embedding_provider_name();
        let embedding_model = self.embedding_model_name();
        let embedding_dimensions = self.embedder.dimensions();
        let vector_candidate_cap = self.vector_candidate_cap;

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEntry>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let session_ref = sid.as_deref();

            // FTS5 BM25 keyword search
            let keyword_results = match Self::fts5_search(&conn, &query, limit * 2) {
                Ok(results) => results,
                Err(e) => {
                    tracing::warn!("FTS5 search failed (returning empty): {e}");
                    Vec::new()
                }
            };

            // Vector similarity search (if embeddings available)
            let vector_results = query_embedding.as_ref().map_or_else(Vec::new, |qe| {
                match Self::vector_search(
                    &conn,
                    qe,
                    &embedding_provider,
                    &embedding_model,
                    embedding_dimensions,
                    limit * 2,
                    None,
                    session_ref,
                    vector_candidate_cap,
                ) {
                    Ok(results) => results,
                    Err(e) => {
                        tracing::warn!("Vector search failed (returning empty): {e}");
                        Vec::new()
                    }
                }
            });

            // Hybrid merge
            let merged = if vector_results.is_empty() {
                keyword_results
                    .iter()
                    .map(|(id, score)| vector::ScoredResult {
                        id: id.clone(),
                        vector_score: None,
                        keyword_score: Some(*score),
                        final_score: *score,
                    })
                    .collect::<Vec<_>>()
            } else {
                vector::hybrid_merge(&vector_results, &keyword_results, vector_weight, keyword_weight, limit)
            };

            // Fetch full entries for merged results in a single query
            // instead of N round-trips (N+1 pattern).
            let mut results = Vec::new();
            if !merged.is_empty() {
                let placeholders: String = (1..=merged.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "SELECT id, key, content, category, created_at, session_id, useful_count \
                     FROM memories WHERE id IN ({placeholders})"
                );
                let mut stmt = conn.prepare(&sql)?;
                let id_params: Vec<Box<dyn rusqlite::types::ToSql>> = merged
                    .iter()
                    .map(|s| Box::new(s.id.clone()) as Box<dyn rusqlite::types::ToSql>)
                    .collect();
                let params_ref: Vec<&dyn rusqlite::types::ToSql> = id_params.iter().map(AsRef::as_ref).collect();
                let rows = stmt.query_map(params_ref.as_slice(), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<u32>>(6)?,
                    ))
                })?;

                let mut entry_map = std::collections::HashMap::new();
                for row in rows {
                    let (id, key, content, cat, ts, sid, useful_count) = row?;
                    entry_map.insert(id, (key, content, cat, ts, sid, useful_count));
                }

                for scored in &merged {
                    if let Some((key, content, cat, ts, sid, useful_count)) = entry_map.remove(&scored.id) {
                        let entry = MemoryEntry {
                            id: scored.id.clone(),
                            key,
                            content,
                            category: Self::str_to_category(&cat),
                            timestamp: ts,
                            session_id: sid,
                            score: Some(f64::from(scored.final_score)),
                            tags: None,
                            access_count: None,
                            useful_count,
                            source: None,
                            source_confidence: None,
                            verification_status: None,
                            lifecycle_state: None,
                            compressed_from: None,
                        };
                        if let Some(filter_sid) = session_ref {
                            if entry.session_id.as_deref() != Some(filter_sid) {
                                continue;
                            }
                        }
                        results.push(entry);
                    }
                }
            }

            // If hybrid returned nothing, fall back to LIKE search.
            // Cap keyword count so we don't create too many SQL shapes,
            // which helps prepared-statement cache efficiency.
            if results.is_empty() {
                const MAX_LIKE_KEYWORDS: usize = 8;
                let keywords: Vec<String> = query
                    .split_whitespace()
                    .take(MAX_LIKE_KEYWORDS)
                    .map(|w| format!("%{w}%"))
                    .collect();
                if !keywords.is_empty() {
                    let conditions: Vec<String> = keywords
                        .iter()
                        .enumerate()
                        .map(|(i, _)| format!("(content LIKE ?{} OR key LIKE ?{})", i * 2 + 1, i * 2 + 2))
                        .collect();
                    let where_clause = conditions.join(" OR ");
                    let sql = format!(
                        "SELECT id, key, content, category, created_at, session_id, useful_count FROM memories
                         WHERE {where_clause}
                         ORDER BY updated_at DESC
                         LIMIT ?{}",
                        keywords.len() * 2 + 1
                    );
                    let mut stmt = conn.prepare(&sql)?;
                    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                    for kw in &keywords {
                        param_values.push(Box::new(kw.clone()));
                        param_values.push(Box::new(kw.clone()));
                    }
                    #[allow(clippy::cast_possible_wrap)]
                    param_values.push(Box::new(limit as i64));
                    let params_ref: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(AsRef::as_ref).collect();
                    let rows = stmt.query_map(params_ref.as_slice(), |row| {
                        Ok(MemoryEntry {
                            id: row.get(0)?,
                            key: row.get(1)?,
                            content: row.get(2)?,
                            category: Self::str_to_category(&row.get::<_, String>(3)?),
                            timestamp: row.get(4)?,
                            session_id: row.get(5)?,
                            score: Some(1.0),
                            tags: None,
                            access_count: None,
                            useful_count: row.get(6)?,
                            source: None,
                            source_confidence: None,
                            verification_status: None,
                            lifecycle_state: None,
                            compressed_from: None,
                        })
                    })?;
                    for row in rows {
                        let entry = row?;
                        if let Some(sid) = session_ref {
                            if entry.session_id.as_deref() != Some(sid) {
                                continue;
                            }
                        }
                        results.push(entry);
                    }
                }
            }

            results.truncate(limit);
            Ok(results)
        })
        .await?
    }

    async fn recall_with_context(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let Some(context) = context.cloned() else {
            return self.recall(query, limit, session_id).await;
        };

        let entries = self
            .recall(query, limit.saturating_mul(3).max(limit), session_id)
            .await?;
        if entries.is_empty() {
            return Ok(entries);
        }

        let pool = self.pool.clone();
        let capped_limit = limit;

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEntry>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let principal = sqlite_read_principal(&conn, &context);
            // FIX-P1-06: the Anonymous `(channel, chat_id, raw_sender)` triple is
            // now produced by `build_sql_scope` itself, so all roles share the
            // single scope builder (no hardcoded branch).
            let (scope_sql, scope_params) = principal.build_sql_scope();

            let mut allowed_ids = std::collections::HashSet::new();
            if !entries.is_empty() {
                let id_placeholders = (0..entries.len()).map(|_| "?").collect::<Vec<_>>().join(",");
                let sql = format!("SELECT id FROM memories WHERE id IN ({id_placeholders}) AND ({scope_sql})");
                let mut params = entries
                    .iter()
                    .map(|entry| Value::from(entry.id.clone()))
                    .collect::<Vec<_>>();
                params.extend(scope_params);
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params_from_iter(params), |row| row.get::<_, String>(0))?;
                for row in rows {
                    allowed_ids.insert(row?);
                }
            }

            let visible = entries
                .into_iter()
                .filter(|entry| allowed_ids.contains(&entry.id))
                .collect::<Vec<_>>();
            let mut visible = post_filter(visible, &principal, |entry| entry.content.as_str());
            visible.truncate(capped_limit);
            Ok(visible)
        })
        .await?
    }

    async fn read_router_history(
        &self,
        evidence: super::router_history::RouterHistoryReadEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<super::router_history::RouterHistoryRead> {
        self.router_history_read(evidence, deadline).await
    }
    async fn commit_router_history(
        &self,
        evidence: &super::router_history::RouterHistoryWriteEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<super::router_history::RouterHistoryCommitReceipt> {
        self.router_history_commit(evidence, deadline).await
    }
    async fn read_router_learning(
        &self,
        evidence: &super::traits::RouterLearningEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<super::traits::RouterLearningRead> {
        self.router_learning_transaction(evidence, None, deadline)
            .await
            .map(|(read, _)| read)
    }
    async fn commit_router_learning(
        &self,
        evidence: &super::traits::RouterLearningEvidence,
        expected_version: i64,
        next: &super::traits::RouterLearningSnapshot,
        deadline: std::time::Instant,
    ) -> anyhow::Result<super::traits::RouterLearningCommit> {
        let (read, conflict) = self
            .router_learning_transaction(evidence, Some((expected_version, next.clone())), deadline)
            .await?;
        if conflict {
            Ok(super::traits::RouterLearningCommit::Conflict)
        } else {
            Ok(super::traits::RouterLearningCommit::Committed(
                read.snapshot.context("Router committed snapshot missing")?,
            ))
        }
    }

    async fn count_rehearsal_hits(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        deadline: std::time::Instant,
    ) -> anyhow::Result<u32> {
        anyhow::ensure!(
            query.len() <= 1_024 && (1..=16).contains(&limit),
            "invalid rehearsal count bounds"
        );
        anyhow::ensure!(std::time::Instant::now() < deadline, "rehearsal count deadline expired");
        let fts_query = super::topic::build_safe_fts_query(query);
        if fts_query.is_empty() {
            return Ok(0);
        }
        let db_path = self.db_path.clone();
        let context = context.clone();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<u32> {
            anyhow::ensure!(std::time::Instant::now() < deadline, "rehearsal count deadline expired");
            // A dedicated read-only connection cannot wait for a pooled reader
            // after the decision deadline. SQLite itself interrupts slow FTS.
            let conn = Self::open_rehearsal_reader(&db_path, deadline)?;
            let principal = sqlite_read_principal(&conn, &context);
            anyhow::ensure!(std::time::Instant::now() < deadline, "rehearsal count deadline expired");
            let (scope_sql, scope_params) = principal.build_sql_scope();
            let sql = format!(
                "SELECT m.content FROM memories_fts f JOIN memories m ON m.rowid = f.rowid \
                 WHERE memories_fts MATCH ? AND ({scope_sql}) \
                 AND length(CAST(m.content AS BLOB)) <= 4096 \
                 ORDER BY bm25(memories_fts) LIMIT ?"
            );
            let mut params = Vec::with_capacity(scope_params.len() + 2);
            params.push(Value::from(fts_query));
            params.extend(scope_params);
            params.push(Value::from(i64::from(limit) * 3));
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(params), |row| row.get::<_, String>(0))?;
            let candidates = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            anyhow::ensure!(std::time::Instant::now() < deadline, "rehearsal count deadline expired");
            let visible = post_filter(candidates, &principal, String::as_str);
            anyhow::ensure!(std::time::Instant::now() < deadline, "rehearsal count deadline expired");
            Ok(u32::try_from(visible.len().min(usize::from(limit)))?)
        })
        .await?
    }

    async fn retrieve_decision_memory(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        session_id: Option<&str>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<DecisionMemoryHit>> {
        anyhow::ensure!(
            query.len() <= 1_024 && (1..=5).contains(&limit),
            "invalid decision Memory bounds"
        );
        anyhow::ensure!(std::time::Instant::now() < deadline, "decision Memory deadline expired");
        anyhow::ensure!(
            session_id.is_none_or(|id| !id.is_empty() && id.len() <= 512),
            "invalid decision Memory session"
        );
        let fts_query = super::topic::build_safe_fts_query(query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let session_id = session_id.map(str::to_owned);
        let db_path = self.db_path.clone();
        let context = context.clone();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<DecisionMemoryHit>> {
            anyhow::ensure!(std::time::Instant::now() < deadline, "decision Memory deadline expired");
            let conn = Self::open_rehearsal_reader(&db_path, deadline)?;
            let principal = sqlite_read_principal(&conn, &context);
            let (scope_sql, scope_params) = principal.build_sql_scope();
            let sql = format!(
                "SELECT m.id, m.content, m.created_at, m.source_event_id, m.key, m.category, m.session_id FROM memories_fts f JOIN memories m ON m.rowid = f.rowid \
                 WHERE memories_fts MATCH ? AND ({scope_sql}) \
                 AND length(CAST(m.id AS BLOB)) <= 128 \
                 AND length(CAST(m.content AS BLOB)) <= 4096 \
                 AND length(CAST(m.created_at AS BLOB)) <= 64 \
                 AND (m.source_event_id IS NULL OR length(CAST(m.source_event_id AS BLOB)) <= 128) \
                 AND length(CAST(m.key AS BLOB)) <= 1024 AND length(CAST(m.category AS BLOB)) <= 128 \
                 AND (m.session_id IS NULL OR length(CAST(m.session_id AS BLOB)) <= 512) \
                 AND (? IS NULL OR m.session_id = ?) \
                 ORDER BY bm25(memories_fts) LIMIT ?"
            );
            let mut params = Vec::with_capacity(scope_params.len() + 4);
            params.push(Value::from(fts_query));
            params.extend(scope_params);
            params.push(session_id.clone().map(Value::from).unwrap_or(Value::Null));
            params.push(session_id.map(Value::from).unwrap_or(Value::Null));
            params.push(Value::from(i64::from(limit) * 3));
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(params), |row| {
                Ok(DecisionMemoryHit {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    created_at: row.get(2)?,
                    source_event_id: row.get(3)?,
                    key: row.get(4)?, category: Self::str_to_category(&row.get::<_, String>(5)?),
                    session_id: row.get(6)?,
                })
            })?;
            let candidates = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            anyhow::ensure!(std::time::Instant::now() < deadline, "decision Memory deadline expired");
            let mut visible = post_filter(candidates, &principal, |hit| hit.content.as_str());
            visible.truncate(usize::from(limit));
            anyhow::ensure!(std::time::Instant::now() < deadline, "decision Memory deadline expired");
            Ok(visible)
        })
        .await?
    }

    async fn recall_with_context_mode(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let context = context.cloned().unwrap_or_default();
        // C03 / V02-R5 defence in depth: an owner-isolated context is enforced
        // whatever mode was requested. The tool already selects Enforce for an
        // isolated caller; the backend must not be talked out of it by a
        // caller, a config flag or a future call site.
        let mode = if context.require_owner_match {
            MemoryReadMode::Enforce
        } else {
            mode
        };
        let unrestricted = self.recall(query, limit, session_id).await?;
        let scoped = self
            .recall_with_context(query, limit, session_id, Some(&context))
            .await?;
        let pool = self.pool.clone();
        let query_owned = query.to_string();
        let query_for_topics = query_owned.clone();
        let mut selected = match mode {
            MemoryReadMode::Enforce => scoped.clone(),
            MemoryReadMode::Observe => unrestricted.clone(),
        };
        let mut selected_ids = selected
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<std::collections::HashSet<_>>();
        let topic_rows =
            crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<(Vec<MemoryEntry>, Principal)> {
                let reader = pool.read()?;
                let conn = reader.connection()?;
                let principal = sqlite_read_principal(&conn, &context);
                let topic_principal = match mode {
                    MemoryReadMode::Enforce => principal.clone(),
                    MemoryReadMode::Observe => sqlite_owner_read_principal(),
                };
                let mut rows = Vec::new();
                for topic in super::topic::search_topics_fts(&conn, &query_for_topics, 3)? {
                    for entry in super::topic::query_topic_context(&conn, &topic.id, &topic_principal, limit.max(1))? {
                        rows.push(MemoryEntry {
                            id: entry.id,
                            key: entry.key,
                            content: entry.content,
                            category: MemoryCategory::Conversation,
                            timestamp: entry.created_at,
                            session_id: None,
                            score: Some(0.55),
                            tags: None,
                            access_count: None,
                            useful_count: None,
                            source: Some("topic_projection".to_string()),
                            source_confidence: None,
                            verification_status: None,
                            lifecycle_state: None,
                            compressed_from: None,
                        });
                    }
                }
                Ok((rows, principal))
            })
            .await??;
        for entry in topic_rows.0 {
            if selected_ids.insert(entry.id.clone()) {
                selected.push(entry);
            }
        }
        selected.sort_by(|left, right| {
            right
                .score
                .unwrap_or(0.0)
                .partial_cmp(&left.score.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.key.cmp(&right.key))
        });
        selected.truncate(limit.max(1));

        let would_deny = unrestricted
            .iter()
            .any(|entry| !scoped.iter().any(|allowed| allowed.id == entry.id));
        let pool = self.pool.clone();
        let principal = topic_rows.1;
        let selected_empty = selected.is_empty();
        crate::runtime::blocking::spawn_blocking(move || {
            let conn = pool.write();
            log_access(
                &conn,
                &principal,
                "search",
                Some(&query_owned),
                None,
                Some(match mode {
                    MemoryReadMode::Enforce => "acl_enforced",
                    MemoryReadMode::Observe => "observe_mode",
                }),
                if mode == MemoryReadMode::Observe && would_deny {
                    "would_deny"
                } else if selected_empty {
                    "no_results"
                } else {
                    "allowed"
                },
            );
        })
        .await?;
        Ok(selected)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let pool = self.pool.clone();
        let key = key.to_string();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<MemoryEntry>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(
                "SELECT id, key, content, category, created_at, session_id, useful_count FROM memories WHERE key = ?1",
            )?;

            let mut rows = stmt.query_map(params![key], |row| {
                Ok(MemoryEntry {
                    id: row.get(0)?,
                    key: row.get(1)?,
                    content: row.get(2)?,
                    category: Self::str_to_category(&row.get::<_, String>(3)?),
                    timestamp: row.get(4)?,
                    session_id: row.get(5)?,
                    score: None,
                    tags: None,
                    access_count: None,
                    useful_count: row.get(6)?,
                    source: None,
                    source_confidence: None,
                    verification_status: None,
                    lifecycle_state: None,
                    compressed_from: None,
                })
            })?;

            match rows.next() {
                Some(Ok(entry)) => Ok(Some(entry)),
                _ => Ok(None),
            }
        })
        .await?
    }

    async fn get_with_context(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        let context = context.cloned().unwrap_or_default();
        let pool = self.pool.clone();
        let key = key.to_string();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<MemoryEntry>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let principal = sqlite_read_principal(&conn, &context);
            let (scope_sql, scope_params) = principal.build_sql_scope();
            let mut query_params = Vec::with_capacity(scope_params.len() + 1);
            query_params.push(Value::from(key.clone()));
            query_params.extend(scope_params);
            let sql = format!(
                "SELECT id, key, content, category, created_at, session_id, useful_count
                 FROM memories WHERE key = ?1 AND ({scope_sql}) LIMIT 1"
            );
            let entry = conn
                .query_row(&sql, params_from_iter(query_params), |row| {
                    Ok(MemoryEntry {
                        id: row.get(0)?,
                        key: row.get(1)?,
                        content: row.get(2)?,
                        category: Self::str_to_category(&row.get::<_, String>(3)?),
                        timestamp: row.get(4)?,
                        session_id: row.get(5)?,
                        score: None,
                        tags: None,
                        access_count: None,
                        useful_count: row.get(6)?,
                        source: None,
                        source_confidence: None,
                        verification_status: None,
                        lifecycle_state: None,
                        compressed_from: None,
                    })
                })
                .optional()?;
            let mut visible = post_filter(entry.into_iter().collect(), &principal, |entry| entry.content.as_str());
            Ok(visible.pop())
        })
        .await?
    }

    async fn get_with_context_mode(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        let context = context.cloned().unwrap_or_default();
        // C03 / V02-R5: see `recall_with_context_mode` — isolation outranks the
        // requested mode.
        let mode = if context.require_owner_match {
            MemoryReadMode::Enforce
        } else {
            mode
        };
        let scoped = self.get_with_context(key, Some(&context)).await?;
        let unrestricted = self.get(key).await?;
        let selected = match mode {
            MemoryReadMode::Enforce => scoped.clone(),
            MemoryReadMode::Observe => unrestricted.clone(),
        };
        let would_deny = unrestricted.is_some() && scoped.is_none();
        let memory_id = selected.as_ref().map(|entry| entry.id.clone());
        let selected_is_some = selected.is_some();
        let key = key.to_string();
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            let conn = pool.write();
            let principal = sqlite_read_principal(&conn, &context);
            log_access(
                &conn,
                &principal,
                "get",
                None,
                memory_id.as_deref().or(Some(key.as_str())),
                Some(match mode {
                    MemoryReadMode::Enforce => "acl_enforced",
                    MemoryReadMode::Observe => "observe_mode",
                }),
                if mode == MemoryReadMode::Observe && would_deny {
                    "would_deny"
                } else if selected_is_some {
                    "allowed"
                } else {
                    "denied"
                },
            );
        })
        .await?;
        Ok(selected)
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        const DEFAULT_LIST_LIMIT: i64 = 1000;

        let pool = self.pool.clone();
        let category = category.cloned();
        let sid = session_id.map(String::from);

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEntry>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let session_ref = sid.as_deref();
            let mut results = Vec::new();

            let row_mapper = |row: &rusqlite::Row| -> rusqlite::Result<MemoryEntry> {
                Ok(MemoryEntry {
                    id: row.get(0)?,
                    key: row.get(1)?,
                    content: row.get(2)?,
                    category: Self::str_to_category(&row.get::<_, String>(3)?),
                    timestamp: row.get(4)?,
                    session_id: row.get(5)?,
                    score: None,
                    tags: None,
                    access_count: None,
                    useful_count: row.get(6)?,
                    source: None,
                    source_confidence: None,
                    verification_status: None,
                    lifecycle_state: None,
                    compressed_from: None,
                })
            };

            if let Some(ref cat) = category {
                let cat_str = Self::category_to_str(cat);
                let mut stmt = conn.prepare(
                    "SELECT id, key, content, category, created_at, session_id, useful_count FROM memories
                     WHERE category = ?1 ORDER BY updated_at DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![cat_str, DEFAULT_LIST_LIMIT], row_mapper)?;
                for row in rows {
                    let entry = row?;
                    if let Some(sid) = session_ref {
                        if entry.session_id.as_deref() != Some(sid) {
                            continue;
                        }
                    }
                    results.push(entry);
                }
            } else {
                let mut stmt = conn.prepare(
                    "SELECT id, key, content, category, created_at, session_id, useful_count FROM memories
                     ORDER BY updated_at DESC LIMIT ?1",
                )?;
                let rows = stmt.query_map(params![DEFAULT_LIST_LIMIT], row_mapper)?;
                for row in rows {
                    let entry = row?;
                    if let Some(sid) = session_ref {
                        if entry.session_id.as_deref() != Some(sid) {
                            continue;
                        }
                    }
                    results.push(entry);
                }
            }

            Ok(results)
        })
        .await?
    }

    async fn list_owner_ids(
        &self,
        category: &MemoryCategory,
    ) -> anyhow::Result<Option<std::collections::HashMap<String, Option<String>>>> {
        let pool = self.pool.clone();
        let category = Self::category_to_str(category);
        crate::runtime::blocking::spawn_blocking(
            move || -> anyhow::Result<Option<std::collections::HashMap<String, Option<String>>>> {
                let reader = pool.read()?;
                let conn = reader.connection()?;
                let mut stmt = conn.prepare("SELECT id, owner_id FROM memories WHERE category = ?1")?;
                let owners = stmt
                    .query_map(params![category], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<std::collections::HashMap<String, Option<String>>>>()?;
                Ok(Some(owners))
            },
        )
        .await?
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        let pool = self.pool.clone();
        let key = key.to_string();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<bool> {
            let conn = pool.write();
            let affected = conn.execute("DELETE FROM memories WHERE key = ?1", params![key])?;
            Ok(affected > 0)
        })
        .await?
    }

    async fn forget_with_context(&self, key: &str, context: Option<&MemoryWriteContext>) -> anyhow::Result<bool> {
        let Some(context) = context.cloned() else {
            return self.forget(key).await;
        };

        let pool = self.pool.clone();
        let key = key.to_string();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<bool> {
            let conn = pool.write();
            let principal = sqlite_read_principal(&conn, &context);
            // FIX-P1-06: unified scope builder for all roles (see recall_with_context).
            let (scope_sql, scope_params) = principal.build_sql_scope();
            // R-B1d D-1: the delete guard. The fixed parameter is ?1 = key and
            // the scope predicate occupies ?2..; the owner guard therefore
            // binds at the next free index and its value is appended last, so
            // the template order is preserved.
            let owner_anchor = Self::forget_owner_anchor(&context);
            let owner_guard = Self::forget_owner_guard(&context, scope_params.len() + 2);
            let mut query_params = Vec::with_capacity(scope_params.len() + 2);
            query_params.push(Value::from(key.clone()));
            query_params.extend(scope_params);
            if let (true, Some(owner)) = (owner_guard.bound_owner, owner_anchor.as_deref()) {
                query_params.push(Value::from(owner.to_string()));
            }
            let owner_guard_sql = owner_guard.sql;
            let query = format!("SELECT id FROM memories WHERE key = ?1 AND ({scope_sql}){owner_guard_sql} LIMIT 1");
            let visible_id = conn
                .query_row(&query, params_from_iter(query_params), |row| row.get::<_, String>(0))
                .optional()?;

            let Some(memory_id) = visible_id else {
                log_access(&conn, &principal, "forget", Some(&key), None, None, "denied");
                return Ok(false);
            };

            let affected = conn.execute("DELETE FROM memories WHERE id = ?1", params![memory_id])?;
            log_access(
                &conn,
                &principal,
                "forget",
                Some(&key),
                Some(&memory_id),
                None,
                "allowed",
            );
            Ok(affected > 0)
        })
        .await?
    }

    async fn move_to_trash(&self, key: &str, reason: &str, grace_days: u32) -> anyhow::Result<Option<String>> {
        let pool = self.pool.clone();
        let key = key.to_string();
        let reason = reason.to_string();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<String>> {
            let conn = pool.write();
            // FIX-P1-11: snapshot the row's value before soft-deleting it. We DO NOT
            // physically delete from `memories`; we record a trash row so it can be
            // restored within the grace window.
            let row: Option<(String, String)> = conn
                .query_row(
                    "SELECT content, category FROM memories WHERE key = ?1",
                    params![key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((content, category)) = row else {
                return Ok(None);
            };
            // Idempotency: if an unrestored trash entry already exists for this key,
            // return it rather than duplicating.
            let existing: Option<String> = conn
                .query_row(
                    "SELECT trash_id FROM memory_trash WHERE memory_key = ?1 AND restored_at IS NULL LIMIT 1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(trash_id) = existing {
                return Ok(Some(trash_id));
            }
            let trash_id = format!("trash-{}", Uuid::now_v7());
            let now = Utc::now();
            let grace_until = now + chrono::Duration::days(i64::from(grace_days));
            conn.execute(
                "INSERT INTO memory_trash (
                    trash_id, memory_key, content, category, reason, trashed_at, grace_until
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    trash_id,
                    key,
                    content,
                    category,
                    reason,
                    now.to_rfc3339(),
                    grace_until.to_rfc3339(),
                ],
            )?;
            Ok(Some(trash_id))
        })
        .await?
    }

    async fn create_evolution_proposal(
        &self,
        draft: crate::self_system::evolution::EvolutionProposalDraft,
    ) -> anyhow::Result<String> {
        use crate::self_system::evolution::proposal::ProposalRowValues;
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<String> {
            let values = ProposalRowValues::encode(&draft)?;
            let conn = pool.write();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO evolution_proposals (
                    draft_id, owner_id, principal_id, workspace_id, topic_id, task_id,
                    source_message_event_ids_json, source_memory_event_ids_json, evidence_hashes_json,
                    target_resource_json, proposed_change_json, risk_level, mode,
                    created_at, created_by_runtime, judge_verdict_json, applied_at, applied_by, rollback_anchor_json
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
                params![
                    values.draft_id,
                    values.owner_id,
                    values.principal_id,
                    values.workspace_id,
                    values.topic_id,
                    values.task_id,
                    values.source_message_event_ids_json,
                    values.source_memory_event_ids_json,
                    values.evidence_hashes_json,
                    values.target_resource_json,
                    values.proposed_change_json,
                    values.risk_level,
                    values.mode,
                    values.created_at.to_rfc3339(),
                    values.created_by_runtime,
                    values.judge_verdict_json,
                    values.applied_at.map(|ts| ts.to_rfc3339()),
                    values.applied_by,
                    values.rollback_anchor_json,
                ],
            )?;
            conn.execute(
                "INSERT INTO evolution_proposal_events (draft_id, event_type, occurred_at, actor, payload_json)
                 VALUES (?1, 'proposal.drafted', ?2, ?3, ?4)",
                params![
                    values.draft_id,
                    now,
                    values.created_by_runtime,
                    Some(serde_json::json!({ "mode": values.mode, "risk_level": values.risk_level }).to_string()),
                ],
            )?;
            Ok(values.draft_id)
        })
        .await?
    }

    async fn list_evolution_proposals(
        &self,
        principal: &MemoryPrincipal,
        filter: crate::self_system::evolution::ProposalFilter,
    ) -> anyhow::Result<Vec<crate::self_system::evolution::EvolutionProposalDraft>> {
        let pool = self.pool.clone();
        let owner_scope = Self::evolution_owner_scope(principal);
        crate::runtime::blocking::spawn_blocking(
            move || -> anyhow::Result<Vec<crate::self_system::evolution::EvolutionProposalDraft>> {
                let reader = pool.read()?;
                let conn = reader.connection()?;
                // Parameterized dynamic WHERE: every predicate binds a value; no
                // user text is interpolated into the SQL string.
                let mut sql = String::from(
                    "SELECT draft_id, owner_id, principal_id, workspace_id, topic_id, task_id,
                            source_message_event_ids_json, source_memory_event_ids_json, evidence_hashes_json,
                            target_resource_json, proposed_change_json, risk_level, mode,
                            created_at, created_by_runtime, judge_verdict_json, applied_at, applied_by,
                            rollback_anchor_json
                       FROM evolution_proposals
                      WHERE 1 = 1",
                );
                let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
                if let Some(owner_id) = owner_scope {
                    sql.push_str(" AND owner_id = ?");
                    binds.push(Box::new(owner_id));
                }
                if let Some(workspace_id) = filter.workspace_id {
                    sql.push_str(" AND workspace_id = ?");
                    binds.push(Box::new(workspace_id));
                }
                if let Some(owner_id) = filter.owner_id {
                    sql.push_str(" AND owner_id = ?");
                    binds.push(Box::new(owner_id));
                }
                if let Some(topic_id) = filter.topic_id {
                    sql.push_str(" AND topic_id = ?");
                    binds.push(Box::new(topic_id));
                }
                if let Some(task_id) = filter.task_id {
                    sql.push_str(" AND task_id = ?");
                    binds.push(Box::new(task_id));
                }
                if let Some(mode) = filter.mode {
                    sql.push_str(" AND mode = ?");
                    binds.push(Box::new(
                        crate::self_system::evolution::proposal::mode_to_db(&mode).to_string(),
                    ));
                }
                if let Some(judged) = filter.judged {
                    if judged {
                        sql.push_str(" AND judge_verdict_json IS NOT NULL");
                    } else {
                        sql.push_str(" AND judge_verdict_json IS NULL");
                    }
                }
                if let Some(applied) = filter.applied {
                    if applied {
                        sql.push_str(" AND applied_at IS NOT NULL");
                    } else {
                        sql.push_str(" AND applied_at IS NULL");
                    }
                }
                if let Some(since) = filter.since {
                    sql.push_str(" AND created_at >= ?");
                    binds.push(Box::new(since.to_rfc3339()));
                }
                sql.push_str(" ORDER BY id DESC");
                if filter.limit > 0 {
                    sql.push_str(" LIMIT ?");
                    binds.push(Box::new(i64::try_from(filter.limit).unwrap_or(i64::MAX)));
                }

                let mut stmt = conn.prepare(&sql)?;
                let bind_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| &**b as &dyn rusqlite::ToSql).collect();
                let rows = stmt.query_map(bind_refs.as_slice(), Self::evolution_proposal_from_row)?;
                let mut proposals = Vec::new();
                for row in rows {
                    proposals.push(row??);
                }
                Ok(proposals)
            },
        )
        .await?
    }

    async fn get_evolution_proposal(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
    ) -> anyhow::Result<Option<crate::self_system::evolution::EvolutionProposalDraft>> {
        let pool = self.pool.clone();
        let owner_scope = Self::evolution_owner_scope(principal);
        let draft_id = draft_id.to_string();
        crate::runtime::blocking::spawn_blocking(
            move || -> anyhow::Result<Option<crate::self_system::evolution::EvolutionProposalDraft>> {
                let reader = pool.read()?;
                let conn = reader.connection()?;
                let proposal = conn
                    .query_row(
                        "SELECT draft_id, owner_id, principal_id, workspace_id, topic_id, task_id,
                                source_message_event_ids_json, source_memory_event_ids_json, evidence_hashes_json,
                                target_resource_json, proposed_change_json, risk_level, mode,
                                created_at, created_by_runtime, judge_verdict_json, applied_at, applied_by,
                                rollback_anchor_json
                           FROM evolution_proposals
                          WHERE draft_id = ?1",
                        params![draft_id],
                        Self::evolution_proposal_from_row,
                    )
                    .optional()?;
                let Some(proposal) = proposal else {
                    return Ok(None);
                };
                let proposal = proposal?;
                // Cross-owner access returns NotFound (None) rather than Forbidden
                // to avoid a cross-owner existence side channel.
                if let Some(owner_id) = owner_scope {
                    if proposal.owner_id != owner_id {
                        return Ok(None);
                    }
                }
                Ok(Some(proposal))
            },
        )
        .await?
    }

    async fn update_evolution_proposal_status(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        update: crate::self_system::evolution::ProposalStatusUpdate,
    ) -> anyhow::Result<()> {
        use crate::self_system::evolution::ProposalStatusUpdate;
        let pool = self.pool.clone();
        let owner_scope = Self::evolution_owner_scope(principal);
        let draft_id = draft_id.to_string();
        let actor = principal
            .owner_id
            .clone()
            .or_else(|| principal.agent_id.clone())
            .unwrap_or_else(|| "self_system".to_string());
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = pool.write();
            // Fetch the row first to enforce owner ACL and re-judge guard.
            let existing: Option<(String, Option<String>, Option<String>)> = conn
                .query_row(
                    "SELECT owner_id, judge_verdict_json, applied_at FROM evolution_proposals WHERE draft_id = ?1",
                    params![draft_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((owner_id, judge_verdict_json, applied_at)) = existing else {
                anyhow::bail!("evolution proposal {draft_id} not found");
            };
            if let Some(scope_owner) = owner_scope {
                if owner_id != scope_owner {
                    anyhow::bail!("evolution proposal {draft_id} not found");
                }
            }
            let now = Utc::now().to_rfc3339();
            match update {
                ProposalStatusUpdate::Judged { verdict } => {
                    if judge_verdict_json.is_some() {
                        anyhow::bail!("evolution proposal {draft_id} already judged; refusing re-judge");
                    }
                    let verdict_json = serde_json::to_string(&verdict)?;
                    conn.execute(
                        "UPDATE evolution_proposals SET judge_verdict_json = ?1 WHERE draft_id = ?2",
                        params![verdict_json, draft_id],
                    )?;
                    conn.execute(
                        "INSERT INTO evolution_proposal_events (draft_id, event_type, occurred_at, actor, payload_json)
                         VALUES (?1, 'proposal.judged', ?2, ?3, ?4)",
                        params![draft_id, now, actor, Some(verdict_json)],
                    )?;
                }
                ProposalStatusUpdate::Applied {
                    applied_by,
                    rollback_anchor,
                } => {
                    let anchor_json = serde_json::to_string(&rollback_anchor)?;
                    conn.execute(
                        "UPDATE evolution_proposals
                            SET applied_at = ?1, applied_by = ?2, rollback_anchor_json = ?3
                          WHERE draft_id = ?4",
                        params![now, applied_by, anchor_json, draft_id],
                    )?;
                    conn.execute(
                        "INSERT INTO evolution_proposal_events (draft_id, event_type, occurred_at, actor, payload_json)
                         VALUES (?1, 'proposal.applied', ?2, ?3, ?4)",
                        params![draft_id, now, actor, Some(anchor_json)],
                    )?;
                }
                ProposalStatusUpdate::RolledBack => {
                    if applied_at.is_none() {
                        anyhow::bail!("evolution proposal {draft_id} is not applied; cannot roll back");
                    }
                    conn.execute(
                        "UPDATE evolution_proposals SET applied_at = NULL WHERE draft_id = ?1",
                        params![draft_id],
                    )?;
                    conn.execute(
                        "INSERT INTO evolution_proposal_events (draft_id, event_type, occurred_at, actor, payload_json)
                         VALUES (?1, 'proposal.rollback', ?2, ?3, NULL)",
                        params![draft_id, now, actor],
                    )?;
                }
            }
            Ok(())
        })
        .await?
    }

    async fn append_evolution_proposal_event(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        event_type: &str,
        actor: &str,
        payload_json: Option<&str>,
    ) -> anyhow::Result<()> {
        let pool = self.pool.clone();
        let owner_scope = Self::evolution_owner_scope(principal);
        let draft_id = draft_id.to_string();
        let event_type = event_type.to_string();
        let actor = actor.to_string();
        let payload_json = payload_json.map(str::to_string);
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = pool.write();
            let owner: Option<String> = conn
                .query_row(
                    "SELECT owner_id FROM evolution_proposals WHERE draft_id = ?1",
                    params![draft_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(owner) = owner else {
                anyhow::bail!("evolution proposal {draft_id} not found");
            };
            if let Some(scope_owner) = owner_scope {
                if owner != scope_owner {
                    anyhow::bail!("evolution proposal {draft_id} not found");
                }
            }
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO evolution_proposal_events (draft_id, event_type, occurred_at, actor, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![draft_id, event_type, now, actor, payload_json],
            )?;
            Ok(())
        })
        .await?
    }

    async fn increment_useful_count(&self, id: &str) -> anyhow::Result<()> {
        let pool = self.pool.clone();
        let id = id.to_string();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = pool.write();
            conn.execute(
                "UPDATE memories
                 SET useful_count = COALESCE(useful_count, 0) + 1,
                     updated_at = ?1
                 WHERE id = ?2",
                params![Local::now().to_rfc3339(), id],
            )?;
            Ok(())
        })
        .await?
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let pool = self.pool.clone();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<usize> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            Ok(count as usize)
        })
        .await?
    }

    #[allow(clippy::too_many_arguments)]
    async fn append_conversation_turn(
        &self,
        session_key: &str,
        channel: &str,
        sender: &str,
        role: &str,
        content: &str,
        timestamp: Option<&str>,
        message_id: Option<&str>,
        owner_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let pool = self.pool.clone();
        let session_key = session_key.to_string();
        let channel = channel.to_string();
        let sender = sender.to_string();
        let role = role.to_string();
        let content = content.to_string();
        let timestamp = Self::normalize_conversation_timestamp(timestamp);
        let message_id = message_id.map(str::to_string);
        let owner_id = owner_id
            .map(str::to_string)
            .or_else(|| Some(format!("legacy:{session_key}")));
        let preview = Self::conversation_preview(&content);

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = pool.write();
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO sessions (
                    session_key,
                    channel,
                    sender,
                    owner_id,
                    created_at,
                    updated_at,
                    message_count,
                    last_message_preview
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?5, 1, ?6)
                 ON CONFLICT(session_key) DO UPDATE SET
                    channel = excluded.channel,
                    sender = excluded.sender,
                    owner_id = COALESCE(excluded.owner_id, sessions.owner_id),
                    updated_at = excluded.updated_at,
                    message_count = COALESCE(sessions.message_count, 0) + 1,
                    last_message_preview = excluded.last_message_preview",
                params![
                    &session_key,
                    &channel,
                    &sender,
                    owner_id.as_deref(),
                    &timestamp,
                    &preview
                ],
            )?;
            tx.execute(
                "INSERT INTO conversation_turns (session_key, owner_id, role, content, timestamp, message_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    &session_key,
                    owner_id.as_deref(),
                    &role,
                    &content,
                    &timestamp,
                    message_id.as_deref()
                ],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await?
    }

    async fn project_console_message(&self, event: &MessageEvent) -> anyhow::Result<()> {
        self.project_console_message_with_deadline(event, None).await
    }

    async fn list_console_terminal_events(
        &self,
        workspace_id: &str,
        session_key: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        anyhow::ensure!(
            !workspace_id.is_empty() && !session_key.is_empty(),
            "console recovery scope is empty"
        );
        let workspace_id = workspace_id.to_owned();
        let session_key = session_key.to_owned();
        let limit = i64::try_from(limit.clamp(1, 128))?;
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MessageEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare("SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                FROM message_events WHERE workspace_id = ?1 AND session_key = ?2
                AND source = 'console' AND event_type = 'turn.finalized' AND id > ?3
                ORDER BY id ASC LIMIT ?4")?;
            Ok(stmt.query_map(params![workspace_id, session_key, after_id, limit], Self::message_event_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        }).await?
    }

    async fn list_conversation_sessions(
        &self,
        limit: usize,
        offset: usize,
        channel: Option<&str>,
    ) -> anyhow::Result<Vec<ConversationSessionSummary>> {
        let pool = self.pool.clone();
        let limit = Self::sanitize_conversation_limit(limit);
        let offset = Self::sanitize_conversation_offset(offset);
        let channel = channel.map(str::to_string);

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<ConversationSessionSummary>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut sessions = Vec::new();

            if let Some(channel_filter) = channel {
                let mut stmt = conn.prepare(
                    "SELECT session_key, channel, sender, created_at, updated_at, message_count, last_message_preview
                     FROM sessions
                     WHERE channel = ?1
                     ORDER BY updated_at DESC
                     LIMIT ?2 OFFSET ?3",
                )?;
                let rows = stmt.query_map(params![channel_filter, limit, offset], |row| {
                    let message_count: i64 = row.get(5)?;
                    #[allow(clippy::cast_sign_loss)]
                    let message_count = message_count.max(0) as u64;
                    Ok(ConversationSessionSummary {
                        session_key: row.get(0)?,
                        channel: row.get(1)?,
                        sender: row.get(2)?,
                        created_at: row.get(3)?,
                        updated_at: row.get(4)?,
                        message_count,
                        last_message_preview: row.get(6)?,
                    })
                })?;
                for row in rows {
                    sessions.push(row?);
                }
            } else {
                let mut stmt = conn.prepare(
                    "SELECT session_key, channel, sender, created_at, updated_at, message_count, last_message_preview
                     FROM sessions
                     ORDER BY updated_at DESC
                     LIMIT ?1 OFFSET ?2",
                )?;
                let rows = stmt.query_map(params![limit, offset], |row| {
                    let message_count: i64 = row.get(5)?;
                    #[allow(clippy::cast_sign_loss)]
                    let message_count = message_count.max(0) as u64;
                    Ok(ConversationSessionSummary {
                        session_key: row.get(0)?,
                        channel: row.get(1)?,
                        sender: row.get(2)?,
                        created_at: row.get(3)?,
                        updated_at: row.get(4)?,
                        message_count,
                        last_message_preview: row.get(6)?,
                    })
                })?;
                for row in rows {
                    sessions.push(row?);
                }
            }

            Ok(sessions)
        })
        .await?
    }

    async fn get_conversation_session(&self, session_key: &str) -> anyhow::Result<Option<ConversationSessionSummary>> {
        let pool = self.pool.clone();
        let session_key = session_key.to_string();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<ConversationSessionSummary>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(
                "SELECT session_key, channel, sender, created_at, updated_at, message_count, last_message_preview
                     FROM sessions WHERE session_key = ?1",
            )?;
            let row = stmt.query_row(params![session_key], |row| {
                let message_count: i64 = row.get(5)?;
                #[allow(clippy::cast_sign_loss)]
                let message_count = message_count.max(0) as u64;
                Ok(ConversationSessionSummary {
                    session_key: row.get(0)?,
                    channel: row.get(1)?,
                    sender: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                    message_count,
                    last_message_preview: row.get(6)?,
                })
            });

            match row {
                Ok(summary) => Ok(Some(summary)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(error) => Err(error.into()),
            }
        })
        .await?
    }

    async fn list_conversation_turns(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<ConversationTurn>> {
        self.list_conversation_turns_ordered(principal, session_key, limit, offset, false)
            .await
    }

    async fn list_console_conversation_turns(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<ConversationTurn>> {
        self.list_conversation_turns_ordered(principal, session_key, limit, offset, true)
            .await
    }
    async fn load_recent_conversation_histories(
        &self,
        principal: &MemoryPrincipal,
        max_turns_per_session: usize,
        max_sessions: usize,
    ) -> anyhow::Result<std::collections::HashMap<String, Vec<ConversationTurn>>> {
        let pool = self.pool.clone();
        let owner_id = principal
            .owner_id
            .as_deref()
            .filter(|owner| !owner.trim().is_empty())
            .map(str::to_string);
        let max_turns_per_session = Self::sanitize_conversation_limit(max_turns_per_session);
        let max_sessions = Self::sanitize_hydrated_sessions_limit(max_sessions);
        let legacy_visible = 1_i64;

        crate::runtime::blocking::spawn_blocking(
            move || -> anyhow::Result<std::collections::HashMap<String, Vec<ConversationTurn>>> {
                let reader = pool.read()?;
                let conn = reader.connection()?;
                let mut stmt = conn.prepare(
                    "SELECT id, session_key, role, content, timestamp, message_id
                     FROM (
                         SELECT
                             ct.id,
                             ct.session_key,
                             ct.role,
                             ct.content,
                             ct.timestamp,
                             ct.message_id,
                             ROW_NUMBER() OVER (PARTITION BY ct.session_key ORDER BY ct.id DESC) AS row_num
                         FROM conversation_turns ct
                         INNER JOIN (
                             SELECT session_key
                             FROM sessions
                             WHERE ?3 = 'system:*'
                                OR owner_id = ?3
                                OR (?4 = 1 AND (owner_id IS NULL OR owner_id = 'legacy:' || session_key))
                             ORDER BY updated_at DESC
                             LIMIT ?2
                         ) recent_sessions
                         ON recent_sessions.session_key = ct.session_key
                         WHERE ?3 = 'system:*'
                            OR ct.owner_id = ?3
                            OR (?4 = 1 AND (ct.owner_id IS NULL OR ct.owner_id = 'legacy:' || ct.session_key))
                     )
                     WHERE row_num <= ?1
                     ORDER BY session_key ASC, id ASC",
                )?;
                let rows = stmt.query_map(
                    params![max_turns_per_session, max_sessions, owner_id, legacy_visible],
                    |row| {
                        Ok(ConversationTurn {
                            id: row.get(0)?,
                            session_key: row.get(1)?,
                            role: row.get(2)?,
                            content: row.get(3)?,
                            timestamp: row.get(4)?,
                            message_id: row.get(5)?,
                        })
                    },
                )?;

                let mut histories: std::collections::HashMap<String, Vec<ConversationTurn>> =
                    std::collections::HashMap::new();
                for row in rows {
                    let turn = row?;
                    histories.entry(turn.session_key.clone()).or_default().push(turn);
                }
                Ok(histories)
            },
        )
        .await?
    }

    async fn acquire_ingress_lifetime(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Arc<dyn super::ingress_recovery::IngressLifetime>> {
        super::ingress_recovery::remaining(deadline)?;
        let entry = self.ingress_owners.cell(workspace_id)?;
        let lifetime = if let Some(lifetime) = entry.cell.get() {
            lifetime.clone()
        } else {
            // Only queueing for initialization is timed out. Once this caller
            // owns the gate, admission's registered owner task awaits the real
            // backend completion, including a late blocking open/write.
            let _gate = tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline),
                entry.init_gate.clone().lock_owned(),
            )
            .await
            .map_err(|_| anyhow::anyhow!("ingress owner initialization queue deadline elapsed"))?;
            super::ingress_recovery::remaining(deadline)?;
            entry
                .cell
                .get_or_try_init(|| async {
                    ingress_recovery::acquire(
                        self.pool.clone(),
                        self.db_path.clone(),
                        workspace_id.to_owned(),
                        token.to_owned(),
                        deadline,
                        false,
                    )
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("ingress lifetime is already owned"))
                })
                .await?
                .clone()
        };
        // Validation failures retain the original cell: no token replacement or
        // implicit authority renewal after loss of the issued owner.
        lifetime.validate(deadline).await?;
        Ok(lifetime)
    }

    async fn append_ingress_input(
        &self,
        input: MessageEventInput,
        capsule: super::ingress_recovery::IngressCapsule,
        lifetime: Arc<dyn super::ingress_recovery::IngressLifetime>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<super::ingress_recovery::IngressAppendOutcome> {
        let (event, inserted) = self
            .append_event_with_recovery(
                input,
                Some(super::ingress_recovery::AtomicIngressMutation::Input {
                    capsule,
                    lifetime,
                    deadline,
                }),
                None,
            )
            .await?;
        Ok(if inserted {
            super::ingress_recovery::IngressAppendOutcome::Inserted(event)
        } else {
            super::ingress_recovery::IngressAppendOutcome::AlreadyAccepted(event)
        })
    }

    async fn list_ingress_recovery_candidates(
        &self,
        scope: &super::ingress_recovery::IngressRecoveryScope,
        current: Arc<dyn super::ingress_recovery::IngressLifetime>,
        phase: super::ingress_recovery::IngressRecoveryScan,
        after_id: i64,
        limit: usize,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<super::ingress_recovery::IngressRecoveryCandidate>> {
        ingress_recovery::list(
            self.pool.clone(),
            scope.clone(),
            current,
            phase,
            after_id,
            limit,
            deadline,
        )
        .await
    }

    async fn read_ingress_event(
        &self,
        scope: &super::ingress_recovery::IngressRecoveryScope,
        lookup: &super::ingress_recovery::IngressEventLookup,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Option<MessageEvent>> {
        ingress_recovery::read_event(self.pool.clone(), scope.clone(), lookup.clone(), deadline).await
    }

    async fn claim_ingress_recovery(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Option<super::ingress_recovery::IngressRecoveryClaim>> {
        Ok(ingress_recovery::acquire(
            self.pool.clone(),
            self.db_path.clone(),
            workspace_id.to_owned(),
            token.to_owned(),
            deadline,
            true,
        )
        .await?
        .map(|lifetime| super::ingress_recovery::IngressRecoveryClaim { lifetime }))
    }

    async fn commit_ingress_recovery(
        &self,
        commit: super::ingress_recovery::IngressRecoveryCommit,
    ) -> anyhow::Result<MessageEvent> {
        // Bound the caller-owned evidence before duplicating it for the shared
        // append path; the blocking transaction rechecks scope and authority.
        ingress_recovery::validate_input_bounds(&commit.evidence)?;
        self.append_event_with_recovery(
            commit.evidence.clone(),
            Some(super::ingress_recovery::AtomicIngressMutation::Recovery(commit)),
            None,
        )
        .await
        .map(|(event, _)| event)
    }

    async fn append_message_event_until(
        &self,
        input: MessageEventInput,
        deadline: std::time::Instant,
    ) -> anyhow::Result<MessageEvent> {
        ingress_recovery::validate_input_bounds(&input)?;
        self.append_event_with_recovery(input, None, Some(deadline))
            .await
            .map(|(event, _)| event)
    }

    async fn project_console_message_until(
        &self,
        event: &MessageEvent,
        deadline: std::time::Instant,
    ) -> anyhow::Result<()> {
        self.project_console_message_with_deadline(event, Some(deadline)).await
    }

    async fn append_message_event(&self, input: MessageEventInput) -> anyhow::Result<MessageEvent> {
        self.append_event_with_recovery(input, None, None)
            .await
            .map(|(event, _)| event)
    }

    async fn find_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        let workspace_id = workspace_id.to_string();
        let idempotency_key = idempotency_key.to_string();
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            conn.query_row(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE workspace_id = ?1 AND idempotency_key = ?2
                 LIMIT 1",
                params![workspace_id, idempotency_key],
                Self::message_event_from_row,
            )
            .optional()
            .map_err(Into::into)
        })
        .await?
    }

    async fn list_worker_launch_events(
        &self,
        workspace_id: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        anyhow::ensure!(
            !workspace_id.is_empty() && after_id >= 0 && (1..=128).contains(&limit),
            "worker launch discovery requires workspace, nonnegative cursor and limit 1..=128"
        );
        let workspace_id = workspace_id.to_owned();
        let limit = i64::try_from(limit)?;
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE workspace_id = ?1 AND event_type IN ('worker.recovery.registered', 'worker.ordinary.recovery.registered', 'worker.operator.recovery.registered') AND id > ?2
                 ORDER BY id ASC LIMIT ?3",
            )?;
            stmt.query_map(params![workspace_id, after_id, limit], Self::message_event_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into)
        })
        .await?
    }

    async fn find_owned_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        owner_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        anyhow::ensure!(
            !workspace_id.is_empty() && !owner_id.is_empty() && !idempotency_key.is_empty(),
            "owned event lookup requires workspace, owner and key"
        );
        // The workspace/key index yields at most one row; filter owner before
        // returning any event content to the caller.
        Ok(self
            .find_message_event_by_idempotency_key(workspace_id, idempotency_key)
            .await?
            .filter(|event| event.owner_id.as_deref() == Some(owner_id)))
    }

    async fn find_owned_message_event(
        &self,
        workspace_id: &str,
        owner_id: &str,
        event_id: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        let workspace_id = workspace_id.to_string();
        if workspace_id.is_empty() || owner_id.is_empty() || event_id.is_empty() {
            return Ok(None);
        }
        let owner_id = owner_id.to_string();
        let event_id = event_id.to_string();
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            conn.query_row(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE workspace_id = ?1 AND owner_id = ?2 AND event_id = ?3
                 LIMIT 1",
                params![workspace_id, owner_id, event_id],
                Self::message_event_from_row,
            )
            .optional()
            .map_err(Into::into)
        })
        .await?
    }

    async fn list_message_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let pool = self.pool.clone();
        let principal = principal.clone();
        let limit = Self::sanitize_conversation_limit(limit);
        let system_allowed = Self::is_system_principal(&principal);

        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its original `?5` binding; legacy key(s) bind to
        // new trailing placeholders starting at `?9` (after limit at `?8`).
        let session_indices = Self::session_indices(5, 9, &principal, &legacy_session_keys);
        let session_fragment =
            crate::memory::session_predicate::session_visibility_or_fragment(SQLITE_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MessageEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(&format!(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE id > ?1
                   AND (
                       visibility = 'global'
                       OR (
                           workspace_id = ?2
                           AND (
                               visibility = 'workspace'{owner_fragment}
                               OR (visibility = 'agent' AND (
                                   (?3 IS NOT NULL AND agent_id = ?3)
                                   OR (?4 IS NOT NULL AND persona_id = ?4)
                               ))
                               OR (visibility = 'session' AND {session_fragment}{owner_fragment})
                               OR (visibility = 'private' AND (
                                   (?3 IS NOT NULL AND agent_id = ?3)
                                   OR (?4 IS NOT NULL AND persona_id = ?4)
                                   OR (?6 IS NOT NULL AND sender = ?6)
                               ){owner_required_fragment})
                               OR (visibility = 'system' AND ?7)
                           )
                       )
                   )
                 ORDER BY id ASC
                 LIMIT ?8",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                &after_id,
                &principal.workspace_id,
                &principal.agent_id,
                &principal.persona_id,
                &principal.session_key,
                &principal.sender,
                &system_allowed,
                &limit,
            ];
            for key in &legacy_session_keys {
                bind.push(key);
            }
            if owner_required_fragment.bound_owner {
                bind.push(&principal.owner_id);
            }
            let rows = stmt.query_map(bind.as_slice(), Self::message_event_from_row)?;

            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await?
    }

    async fn list_message_events_recent(
        &self,
        principal: &MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let pool = self.pool.clone();
        let principal = principal.clone();
        let limit = Self::sanitize_conversation_limit(limit);
        let system_allowed = Self::is_system_principal(&principal);

        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its `?4` binding; legacy key(s) bind at trailing
        // placeholders starting at `?8` (after limit at `?7`).
        let session_indices = Self::session_indices(4, 8, &principal, &legacy_session_keys);
        let session_fragment =
            crate::memory::session_predicate::session_visibility_or_fragment(SQLITE_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(8, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(8, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MessageEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(&format!(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE (
                       visibility = 'global'
                       OR (
                           workspace_id = ?1
                           AND (
                               visibility = 'workspace'{owner_fragment}
                               OR (visibility = 'agent' AND (
                                   (?2 IS NOT NULL AND agent_id = ?2)
                                   OR (?3 IS NOT NULL AND persona_id = ?3)
                               ))
                               OR (visibility = 'session' AND {session_fragment}{owner_fragment})
                               OR (visibility = 'private' AND (
                                   (?2 IS NOT NULL AND agent_id = ?2)
                                   OR (?3 IS NOT NULL AND persona_id = ?3)
                                   OR (?5 IS NOT NULL AND sender = ?5)
                               ){owner_required_fragment})
                               OR (visibility = 'system' AND ?6)
                           )
                       )
                   )
                 ORDER BY id DESC
                 LIMIT ?7",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                &principal.workspace_id,
                &principal.agent_id,
                &principal.persona_id,
                &principal.session_key,
                &principal.sender,
                &system_allowed,
                &limit,
            ];
            for key in &legacy_session_keys {
                bind.push(key);
            }
            if owner_required_fragment.bound_owner {
                bind.push(&principal.owner_id);
            }
            let rows = stmt.query_map(bind.as_slice(), Self::message_event_from_row)?;

            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await?
    }

    async fn list_message_events_time_range(
        &self,
        principal: &MemoryPrincipal,
        descendant_workspace_root: Option<&str>,
        start: &str,
        end: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let pool = self.pool.clone();
        let principal = principal.clone();
        let descendant_workspace_root = descendant_workspace_root.map(str::to_string);
        let start = start.to_string();
        let end = end.to_string();
        let limit = i64::try_from(limit.min(100_000)).unwrap_or(100_000);
        let system_allowed = Self::is_system_principal(&principal);
        anyhow::ensure!(
            descendant_workspace_root.is_none() || system_allowed,
            "descendant workspace event queries require a system principal"
        );
        let descendant_pattern = descendant_workspace_root.as_ref().map(|root| {
            let escaped = root.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            format!("{escaped}{}%", std::path::MAIN_SEPARATOR)
        });
        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        let session_indices = Self::session_indices(4, 12, &principal, &legacy_session_keys);
        let session_fragment =
            crate::memory::session_predicate::session_visibility_or_fragment(SQLITE_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(12, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(12, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MessageEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(&format!(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE created_at >= ?9 AND created_at < ?10
                   AND (
                       visibility = 'global'
                       OR (
                           (
                               workspace_id = ?1
                               OR (?6 AND ?7 IS NOT NULL AND (
                                   workspace_id = ?7 OR workspace_id LIKE ?8 ESCAPE '\\'
                               ))
                           )
                           AND (
                               (?6 AND ?7 IS NOT NULL)
                               OR visibility = 'workspace'{owner_fragment}
                               OR (visibility = 'agent' AND (
                                   (?2 IS NOT NULL AND agent_id = ?2)
                                   OR (?3 IS NOT NULL AND persona_id = ?3)
                               ))
                               OR (visibility = 'session' AND {session_fragment}{owner_fragment})
                               OR (visibility = 'private' AND (
                                   (?2 IS NOT NULL AND agent_id = ?2)
                                   OR (?3 IS NOT NULL AND persona_id = ?3)
                                   OR (?5 IS NOT NULL AND sender = ?5)
                               ){owner_required_fragment})
                               OR (visibility = 'system' AND ?6)
                           )
                       )
                 )
                 ORDER BY created_at ASC, id ASC
                 LIMIT ?11",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                &principal.workspace_id,
                &principal.agent_id,
                &principal.persona_id,
                &principal.session_key,
                &principal.sender,
                &system_allowed,
                &descendant_workspace_root,
                &descendant_pattern,
                &start,
                &end,
                &limit,
            ];
            for key in &legacy_session_keys {
                bind.push(key);
            }
            if owner_required_fragment.bound_owner {
                bind.push(&principal.owner_id);
            }
            let rows = stmt.query_map(bind.as_slice(), Self::message_event_from_row)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await?
    }

    async fn load_recent_shared_context(&self, query: SharedContextQuery) -> anyhow::Result<Vec<MessageEvent>> {
        let pool = self.pool.clone();
        let principal = query.principal;
        let limit = Self::sanitize_conversation_limit(query.limit);
        let after_id = query.since_event_id.unwrap_or(0);
        let system_allowed = Self::is_system_principal(&principal);
        let include_roles = query
            .include_roles
            .into_iter()
            .map(|role| role.trim().to_ascii_lowercase())
            .filter(|role| !role.is_empty())
            .collect::<std::collections::HashSet<_>>();

        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its `?5` binding; legacy key(s) bind at trailing
        // placeholders starting at `?9` (after limit at `?8`).
        let session_indices = Self::session_indices(5, 9, &principal, &legacy_session_keys);
        let session_fragment =
            crate::memory::session_predicate::session_visibility_or_fragment(SQLITE_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MessageEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(&format!(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE id > ?1
                   AND (
                       visibility = 'global'
                       OR (
                           workspace_id = ?2
                           AND (
                               visibility = 'workspace'{owner_required_fragment}
                               OR (visibility = 'agent' AND (
                                   (?3 IS NOT NULL AND agent_id = ?3)
                                   OR (?4 IS NOT NULL AND persona_id = ?4)
                               ))
                               OR (visibility = 'session' AND {session_fragment}{owner_fragment})
                               OR (visibility = 'private' AND (
                                   (?3 IS NOT NULL AND agent_id = ?3)
                                   OR (?4 IS NOT NULL AND persona_id = ?4)
                                   OR (?6 IS NOT NULL AND sender = ?6)
                               ){owner_required_fragment})
                               OR (visibility = 'system' AND ?7)
                           )
                       )
                   )
                 ORDER BY id DESC
                 LIMIT ?8",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                &after_id,
                &principal.workspace_id,
                &principal.agent_id,
                &principal.persona_id,
                &principal.session_key,
                &principal.sender,
                &system_allowed,
                &limit,
            ];
            for key in &legacy_session_keys {
                bind.push(key);
            }
            if owner_required_fragment.bound_owner {
                bind.push(&principal.owner_id);
            }
            let rows = stmt.query_map(bind.as_slice(), Self::message_event_from_row)?;

            let mut events = rows.collect::<Result<Vec<_>, _>>()?;
            events.reverse();
            if include_roles.is_empty() {
                return Ok(events);
            }
            Ok(events
                .into_iter()
                .filter(|event| include_roles.contains(&event.role.to_ascii_lowercase()))
                .collect())
        })
        .await?
    }

    async fn retrieve_decision_session(
        &self,
        principal: &MemoryPrincipal,
        before_event_id: &str,
        limit: u8,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<super::traits::DecisionSessionHit>> {
        anyhow::ensure!(
            (1..=5).contains(&limit) && !before_event_id.is_empty() && before_event_id.len() <= 128,
            "invalid decision Session bounds"
        );
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "decision Session deadline expired"
        );
        let session_key = principal
            .session_key
            .as_ref()
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("selected Session requires a session"))?
            .clone();
        anyhow::ensure!(
            crate::memory::owner_predicate::has_owner_id(principal),
            "selected Session requires an authenticated owner"
        );
        let principal = principal.clone();
        let before_event_id = before_event_id.to_owned();
        let db_path = self.db_path.clone();
        let limit = i64::from(limit);
        let system_allowed = Self::is_system_principal(&principal);
        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its `?3` binding; legacy key(s) bind at trailing
        // placeholders starting at `?9` (after limit at `?8`). The top-level
        // `session_key` hard filter becomes an `IN (...)` read-merge union.
        let mut session_indices = vec![3usize];
        for offset in 0..legacy_session_keys.len() {
            session_indices.push(9 + offset);
        }
        let session_fragment =
            crate::memory::session_predicate::session_key_match_fragment(SQLITE_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-C: `workspace` is owner-qualified for an owner-isolated reader;
        // every other reader's statement is byte-identical.
        let shared_tiers_arm = crate::memory::owner_predicate::session_context_shared_tiers(&owner_fragment);

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<super::traits::DecisionSessionHit>> {
            let conn = Self::open_rehearsal_reader(&db_path, deadline)?;
            let before_id: i64 = conn
                .query_row(
                    "SELECT id FROM message_events WHERE event_id = ?1 AND workspace_id = ?2
                 AND session_key = ?3 AND owner_id = ?4 AND role = 'user' AND event_type = 'message.created'",
                    params![before_event_id, principal.workspace_id, session_key, principal.owner_id],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| anyhow::anyhow!("selected Session source event is unavailable"))?;
            let mut stmt = conn.prepare(&format!(
                "SELECT event_id, role, content, created_at
                 FROM message_events
                 WHERE id < ?1
                   AND workspace_id = ?2
                   AND {session_fragment}
                   AND (
                       {shared_tiers_arm}
                       OR (visibility = 'agent' AND (
                           (?4 IS NOT NULL AND agent_id = ?4)
                           OR (?5 IS NOT NULL AND persona_id = ?5)
                       ))
                       OR (visibility = 'session'{owner_fragment})
                       OR (visibility = 'private' AND (
                           (?4 IS NOT NULL AND agent_id = ?4)
                           OR (?5 IS NOT NULL AND persona_id = ?5)
                           OR (?6 IS NOT NULL AND sender = ?6)
                       ){owner_required_fragment})
                       OR (visibility = 'system' AND ?7)
                   )
                   AND role IN ('user', 'assistant') AND event_type = 'message.created'
                   AND length(CAST(event_id AS BLOB)) <= 128
                   AND length(CAST(content AS BLOB)) <= 4096
                   AND length(CAST(created_at AS BLOB)) <= 64
                 ORDER BY id DESC
                 LIMIT ?8",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                &before_id,
                &principal.workspace_id,
                &session_key,
                &principal.agent_id,
                &principal.persona_id,
                &principal.sender,
                &system_allowed,
                &limit,
            ];
            for key in &legacy_session_keys {
                bind.push(key);
            }
            if owner_required_fragment.bound_owner {
                bind.push(&principal.owner_id);
            }
            let rows = stmt.query_map(bind.as_slice(), |row| {
                Ok(super::traits::DecisionSessionHit {
                    event_id: row.get(0)?,
                    role: row.get(1)?,
                    content: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })?;
            let mut hits = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "decision Session deadline expired"
            );
            hits.reverse();
            Ok(hits)
        })
        .await?
    }

    async fn load_recent_session_context(&self, query: SessionContextQuery) -> anyhow::Result<Vec<MessageEvent>> {
        let Some(session_key) = query.principal.session_key.clone() else {
            return Ok(Vec::new());
        };
        let pool = self.pool.clone();
        let principal = query.principal;
        let limit = Self::sanitize_conversation_limit(query.limit);
        let after_id = query.since_event_id.unwrap_or(0);
        let system_allowed = Self::is_system_principal(&principal);
        let include_roles = query
            .include_roles
            .into_iter()
            .map(|role| role.trim().to_ascii_lowercase())
            .filter(|role| !role.is_empty())
            .collect::<std::collections::BTreeSet<_>>();

        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its `?3` binding; legacy key(s) bind at trailing
        // placeholders starting at `?9` (after limit at `?8`). The top-level
        // `session_key` hard filter becomes an `IN (...)` read-merge union.
        let mut session_indices = vec![3usize];
        for offset in 0..legacy_session_keys.len() {
            session_indices.push(9 + offset);
        }
        let session_fragment =
            crate::memory::session_predicate::session_key_match_fragment(SQLITE_DIALECT, &session_indices);
        let owner_index = Self::owner_index(9, legacy_session_keys.len());
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            owner_index,
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            owner_index,
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        let first_role_index = owner_index + usize::from(owner_required_fragment.bound_owner);
        let role_fragment = if include_roles.is_empty() {
            "1 = 1".to_string()
        } else {
            format!(
                "role IN ({})",
                (first_role_index..first_role_index + include_roles.len())
                    .map(|index| format!("?{index}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        // OV-C: `workspace` is owner-qualified for an owner-isolated reader;
        // every other reader's statement is byte-identical.
        let shared_tiers_arm = crate::memory::owner_predicate::session_context_shared_tiers(&owner_fragment);

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MessageEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(&format!(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE id > ?1
                   AND workspace_id = ?2
                   AND {session_fragment}
                   AND {role_fragment}
                   AND (
                       {shared_tiers_arm}
                       OR (visibility = 'agent' AND (
                           (?4 IS NOT NULL AND agent_id = ?4)
                           OR (?5 IS NOT NULL AND persona_id = ?5)
                       ))
                       OR (visibility = 'session'{owner_fragment})
                       OR (visibility = 'private' AND (
                           (?4 IS NOT NULL AND agent_id = ?4)
                           OR (?5 IS NOT NULL AND persona_id = ?5)
                           OR (?6 IS NOT NULL AND sender = ?6)
                       ){owner_required_fragment})
                       OR (visibility = 'system' AND ?7)
                   )
                 ORDER BY id DESC
                 LIMIT ?8",
                session_fragment = session_fragment.sql,
                role_fragment = role_fragment,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                &after_id,
                &principal.workspace_id,
                &session_key,
                &principal.agent_id,
                &principal.persona_id,
                &principal.sender,
                &system_allowed,
                &limit,
            ];
            for key in &legacy_session_keys {
                bind.push(key);
            }
            if owner_required_fragment.bound_owner {
                bind.push(&principal.owner_id);
            }
            for role in &include_roles {
                bind.push(role);
            }
            let rows = stmt.query_map(bind.as_slice(), Self::message_event_from_row)?;

            let mut events = rows.collect::<Result<Vec<_>, _>>()?;
            events.reverse();
            Ok(events)
        })
        .await?
    }

    async fn append_memory_event(&self, input: MemoryEventInput) -> anyhow::Result<MemoryEvent> {
        let pool = self.pool.clone();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<MemoryEvent> {
            let conn = pool.write();
            let now = Utc::now().to_rfc3339();
            let event_id = input.event_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            conn.execute(
                "INSERT OR IGNORE INTO memory_events (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                    session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    event_id,
                    input.workspace_id,
                    input.owner_id,
                    input.event_type,
                    input.subject_table,
                    input.subject_id,
                    input.session_key,
                    input.run_id,
                    input.parent_run_id,
                    input.agent_id,
                    input.persona_id,
                    input.visibility.as_str(),
                    input.payload_json,
                    now
                ],
            )?;

            let event = conn.query_row(
                "SELECT id, event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                        session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                 FROM memory_events
                 WHERE event_id = ?1
                 LIMIT 1",
                params![event_id],
                Self::memory_event_from_row,
            )?;
            Ok(event)
        })
        .await?
    }

    async fn list_memory_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEvent>> {
        let pool = self.pool.clone();
        let principal = principal.clone();
        let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);

        // C03: the owner parameter binds at the first free trailing slot (`?8`).
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            8,
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            8,
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let system_allowed =
                principal.agent_id.as_deref() == Some("system") || principal.persona_id.as_deref() == Some("system");
            let mut stmt = conn.prepare(&format!(
                "SELECT id, event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                        session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                   FROM memory_events
                  WHERE id > ?1
                    AND (
                        visibility = 'global'
                        OR (
                            workspace_id = ?2
                            AND (
                                visibility = 'workspace'{owner_fragment}
                                OR (visibility = 'agent' AND (
                                    (?3 IS NOT NULL AND agent_id = ?3)
                                    OR (?4 IS NOT NULL AND persona_id = ?4)
                                ))
                                OR (visibility = 'session' AND ?5 IS NOT NULL AND session_key = ?5{owner_fragment})
                                OR (visibility = 'private' AND (
                                    (?3 IS NOT NULL AND agent_id = ?3)
                                    OR (?4 IS NOT NULL AND persona_id = ?4)
                                    OR (event_type = 'document.ingested' AND agent_id IS NULL AND persona_id IS NULL)
                                ){owner_required_fragment})
                                OR (visibility = 'system' AND ?6)
                            )
                        )
                    )
                  ORDER BY id ASC
                  LIMIT ?7",
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                &after_id,
                &principal.workspace_id,
                &principal.agent_id,
                &principal.persona_id,
                &principal.session_key,
                &system_allowed,
                &limit,
            ];
            if owner_required_fragment.bound_owner {
                bind.push(&principal.owner_id);
            }
            let rows = stmt.query_map(bind.as_slice(), Self::memory_event_from_row)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await?
    }

    async fn list_memory_events_recent(
        &self,
        principal: &MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEvent>> {
        let pool = self.pool.clone();
        let principal = principal.clone();
        let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);

        // C03: the owner parameter binds at the first free trailing slot (`?7`).
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            7,
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            7,
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let system_allowed =
                principal.agent_id.as_deref() == Some("system") || principal.persona_id.as_deref() == Some("system");
            let mut stmt = conn.prepare(&format!(
                "SELECT id, event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                        session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                   FROM memory_events
                  WHERE (
                        visibility = 'global'
                        OR (
                            workspace_id = ?1
                            AND (
                                visibility = 'workspace'{owner_fragment}
                                OR (visibility = 'agent' AND (
                                    (?2 IS NOT NULL AND agent_id = ?2)
                                    OR (?3 IS NOT NULL AND persona_id = ?3)
                                ))
                                OR (visibility = 'session' AND ?4 IS NOT NULL AND session_key = ?4{owner_fragment})
                                OR (visibility = 'private' AND (
                                    (?2 IS NOT NULL AND agent_id = ?2)
                                    OR (?3 IS NOT NULL AND persona_id = ?3)
                                    OR (event_type = 'document.ingested' AND agent_id IS NULL AND persona_id IS NULL)
                                ){owner_required_fragment})
                                OR (visibility = 'system' AND ?5)
                            )
                        )
                    )
                  ORDER BY id DESC
                  LIMIT ?6",
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                &principal.workspace_id,
                &principal.agent_id,
                &principal.persona_id,
                &principal.session_key,
                &system_allowed,
                &limit,
            ];
            if owner_required_fragment.bound_owner {
                bind.push(&principal.owner_id);
            }
            let rows = stmt.query_map(bind.as_slice(), Self::memory_event_from_row)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await?
    }

    async fn create_memory_draft(&self, input: MemoryDraftInput) -> anyhow::Result<MemoryDraft> {
        let pool = self.pool.clone();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<MemoryDraft> {
            let mut conn = pool.write();
            let tx = conn.transaction()?;
            let now = Utc::now().to_rfc3339();
            let draft_id = input.draft_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let category = Self::category_to_str(&input.category);
            let visibility = input.visibility.as_str().to_string();

            tx.execute(
                "INSERT OR IGNORE INTO memory_drafts (
                    draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                    agent_id, persona_id, key, content, category, source_event_id,
                    visibility, status, payload_json, created_at, updated_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'pending', ?14, ?15, ?15)",
                params![
                    draft_id,
                    input.workspace_id,
                    input.owner_id,
                    input.worker_run_id,
                    input.parent_run_id,
                    input.session_key,
                    input.agent_id,
                    input.persona_id,
                    input.key,
                    input.content,
                    category,
                    input.source_event_id,
                    visibility,
                    input.payload_json,
                    now,
                ],
            )?;

            let draft = tx.query_row(
                "SELECT id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                        agent_id, persona_id, key, content, category, source_event_id,
                        visibility, status, payload_json, created_at, updated_at
                   FROM memory_drafts
                  WHERE draft_id = ?1
                  LIMIT 1",
                params![draft_id],
                Self::memory_draft_from_row,
            )?;

            tx.execute(
                // C03: the draft's own owner is recorded in the column, not only
                // inside `payload_json`, so a `session`/`private` row born here
                // is attributed and stays readable by its owner-isolated owner.
                "INSERT INTO memory_events (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                    session_key, agent_id, persona_id, visibility, payload_json, created_at
                 )
                 VALUES (?1, ?2, ?3, 'memory.draft.created', 'memory_drafts', ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    Uuid::new_v4().to_string(),
                    draft.workspace_id,
                    draft.owner_id,
                    draft.draft_id,
                    draft.session_key,
                    draft.agent_id,
                    draft.persona_id,
                    draft.visibility.as_str(),
                    serde_json::json!({
                        "worker_run_id": draft.worker_run_id,
                        "owner_id": draft.owner_id,
                        "parent_run_id": draft.parent_run_id,
                        "key": draft.key
                    })
                    .to_string(),
                    Utc::now().to_rfc3339(),
                ],
            )?;

            tx.commit()?;
            Ok(draft)
        })
        .await?
    }

    async fn list_memory_drafts_for_run(
        &self,
        principal: &MemoryPrincipal,
        worker_run_id: &str,
    ) -> anyhow::Result<Vec<MemoryDraft>> {
        let pool = self.pool.clone();
        let worker_run_id = worker_run_id.to_string();
        // Owner ACL: a caller may only see drafts it owns, plus drafts with no
        // owner (legacy / system-created). System principals bypass the filter.
        let owner = if Self::is_system_principal(principal) {
            None
        } else {
            principal
                .owner_id
                .as_deref()
                .filter(|owner| !owner.trim().is_empty())
                .map(str::to_string)
        };

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MemoryDraft>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            if let Some(owner) = owner {
                let mut stmt = conn.prepare(
                    "SELECT id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                            agent_id, persona_id, key, content, category, source_event_id,
                            visibility, status, payload_json, created_at, updated_at
                       FROM memory_drafts
                      WHERE worker_run_id = ?1 AND (owner_id = ?2 OR owner_id IS NULL)
                      ORDER BY id ASC",
                )?;
                let rows = stmt.query_map(params![worker_run_id, owner], Self::memory_draft_from_row)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
            } else {
                let mut stmt = conn.prepare(
                    "SELECT id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                            agent_id, persona_id, key, content, category, source_event_id,
                            visibility, status, payload_json, created_at, updated_at
                       FROM memory_drafts
                      WHERE worker_run_id = ?1
                      ORDER BY id ASC",
                )?;
                let rows = stmt.query_map(params![worker_run_id], Self::memory_draft_from_row)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
            }
        })
        .await?
    }

    async fn merge_memory_draft(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
    ) -> anyhow::Result<Option<MemoryDraft>> {
        let pool = self.pool.clone();
        let draft_id = draft_id.to_string();
        let owner = if Self::is_system_principal(principal) {
            None
        } else {
            principal
                .owner_id
                .as_deref()
                .filter(|owner| !owner.trim().is_empty())
                .map(str::to_string)
        };

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<MemoryDraft>> {
            let mut conn = pool.write();
            let tx = conn.transaction()?;
            let mut draft = match tx.query_row(
                "SELECT id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                        agent_id, persona_id, key, content, category, source_event_id,
                        visibility, status, payload_json, created_at, updated_at
                   FROM memory_drafts
                  WHERE draft_id = ?1
                  LIMIT 1",
                params![draft_id],
                Self::memory_draft_from_row,
            ) {
                Ok(draft) => draft,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
                Err(error) => return Err(error.into()),
            };

            // Owner ACL: reject merge attempts on drafts owned by a different principal.
            if let Some(owner) = owner.as_deref() {
                if let Some(draft_owner) = draft.owner_id.as_deref() {
                    if draft_owner != owner {
                        anyhow::bail!("memory draft {} is owned by a different principal", draft.draft_id);
                    }
                }
            }

            if draft.status != "pending" && draft.status != "merge_requested" {
                return Ok(Some(draft));
            }

            let now = Utc::now().to_rfc3339();
            let category = Self::category_to_str(&draft.category);
            let memory_id = Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO memories (
                    id, key, content, category, created_at, updated_at, session_id,
                    workspace_id, owner_id, agent_id, persona_id, source_event_id, source, visibility
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'memory_draft', ?12)
                 ON CONFLICT(key) DO UPDATE SET
                    content = excluded.content,
                    category = excluded.category,
                    updated_at = excluded.updated_at,
                    session_id = excluded.session_id,
                    workspace_id = excluded.workspace_id,
                    owner_id = excluded.owner_id,
                    agent_id = excluded.agent_id,
                    persona_id = excluded.persona_id,
                    source_event_id = excluded.source_event_id,
                    source = excluded.source,
                    visibility = excluded.visibility",
                params![
                    memory_id,
                    draft.key,
                    draft.content,
                    category,
                    now,
                    draft.session_key,
                    draft.workspace_id,
                    draft.owner_id,
                    draft.agent_id,
                    draft.persona_id,
                    draft.source_event_id,
                    draft.visibility.as_str(),
                ],
            )?;

            tx.execute(
                "UPDATE memory_drafts SET status = 'merged', updated_at = ?2 WHERE draft_id = ?1",
                params![draft.draft_id, now],
            )?;
            draft.status = "merged".to_string();
            draft.updated_at = now;

            for (event_type, subject_table, subject_id) in [
                ("memory.draft.merged", "memory_drafts", draft.draft_id.as_str()),
                ("memory.stored", "memories", draft.key.as_str()),
            ] {
                tx.execute(
                    // C03: see `create_memory_draft` - the owner goes in the column.
                    "INSERT INTO memory_events (
                        event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                        session_key, agent_id, persona_id, visibility, payload_json, created_at
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![
                        Uuid::new_v4().to_string(),
                        draft.workspace_id,
                        draft.owner_id,
                        event_type,
                        subject_table,
                        subject_id,
                        draft.session_key,
                        draft.agent_id,
                        draft.persona_id,
                        draft.visibility.as_str(),
                        serde_json::json!({
                            "draft_id": draft.draft_id,
                            "owner_id": draft.owner_id,
                            "worker_run_id": draft.worker_run_id,
                            "parent_run_id": draft.parent_run_id,
                            "key": draft.key
                        })
                        .to_string(),
                        Utc::now().to_rfc3339(),
                    ],
                )?;
            }

            tx.commit()?;
            Ok(Some(draft))
        })
        .await?
    }

    async fn reject_memory_draft(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        reason: Option<&str>,
    ) -> anyhow::Result<Option<MemoryDraft>> {
        let pool = self.pool.clone();
        let draft_id = draft_id.to_string();
        let reason = reason.map(str::to_string);
        let owner = if Self::is_system_principal(principal) {
            None
        } else {
            principal
                .owner_id
                .as_deref()
                .filter(|owner| !owner.trim().is_empty())
                .map(str::to_string)
        };

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<MemoryDraft>> {
            let mut conn = pool.write();
            let tx = conn.transaction()?;
            let mut draft = match tx.query_row(
                "SELECT id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                        agent_id, persona_id, key, content, category, source_event_id,
                        visibility, status, payload_json, created_at, updated_at
                   FROM memory_drafts
                  WHERE draft_id = ?1
                  LIMIT 1",
                params![draft_id],
                Self::memory_draft_from_row,
            ) {
                Ok(draft) => draft,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
                Err(error) => return Err(error.into()),
            };

            // Owner ACL: reject attempts on drafts owned by a different principal.
            if let Some(owner) = owner.as_deref() {
                if let Some(draft_owner) = draft.owner_id.as_deref() {
                    if draft_owner != owner {
                        anyhow::bail!("memory draft {} is owned by a different principal", draft.draft_id);
                    }
                }
            }

            if draft.status == "merged" || draft.status == "rejected" {
                return Ok(Some(draft));
            }

            let now = Utc::now().to_rfc3339();
            tx.execute(
                "UPDATE memory_drafts SET status = 'rejected', updated_at = ?2 WHERE draft_id = ?1",
                params![draft.draft_id, now],
            )?;
            draft.status = "rejected".to_string();
            draft.updated_at = now;

            tx.execute(
                // C03: see `create_memory_draft` - the owner goes in the column.
                "INSERT INTO memory_events (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                    session_key, agent_id, persona_id, visibility, payload_json, created_at
                 )
                 VALUES (?1, ?2, ?3, 'memory.draft.rejected', 'memory_drafts', ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    Uuid::new_v4().to_string(),
                    draft.workspace_id,
                    draft.owner_id,
                    draft.draft_id,
                    draft.session_key,
                    draft.agent_id,
                    draft.persona_id,
                    draft.visibility.as_str(),
                    serde_json::json!({
                        "draft_id": draft.draft_id,
                        "owner_id": draft.owner_id,
                        "worker_run_id": draft.worker_run_id,
                        "parent_run_id": draft.parent_run_id,
                        "key": draft.key,
                        "reason": reason
                    })
                    .to_string(),
                    Utc::now().to_rfc3339(),
                ],
            )?;

            tx.commit()?;
            Ok(Some(draft))
        })
        .await?
    }

    async fn ingest_document(&self, input: DocumentIngestInput) -> anyhow::Result<DocumentRecord> {
        let pool = self.pool.clone();
        let embedding_provider = self.embedding_provider_name();
        let embedding_model = self.embedding_model_name();
        let embedding_dimensions = self.embedding_dimensions_i64();
        let raw_chunks: Vec<(usize, Option<String>, String)> = super::chunker::chunk_markdown(&input.content, 1_000)
            .into_iter()
            .map(|chunk| {
                (
                    chunk.index,
                    chunk.heading.as_ref().map(|heading| heading.to_string()),
                    chunk.content,
                )
            })
            .collect();
        let mut prepared_chunks = Vec::with_capacity(raw_chunks.len());
        for (chunk_index, heading, content) in raw_chunks {
            let embedding_bytes = if self.embedder.dimensions() == 0 {
                None
            } else {
                self.get_or_compute_embedding(&content)
                    .await?
                    .map(|embedding| vector::vec_to_bytes(&embedding))
            };
            prepared_chunks.push((chunk_index, heading, content, embedding_bytes));
        }

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<DocumentRecord> {
            let mut conn = pool.write();
            let tx = conn.transaction()?;
            let now = Utc::now().to_rfc3339();
            let document_id = input.document_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let content_sha256 = Self::content_sha256_hex(&input.content);
            let visibility = input.visibility.as_str().to_string();
            let chunk_count = prepared_chunks.len();

            tx.execute(
                "INSERT INTO documents (
                    document_id, workspace_id, owner_id, topic_id, task_id, source_message_event_id,
                    source_kind, source_uri, title, content_sha256, mime_type, visibility,
                    metadata_json, chunk_count, created_at, updated_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?15)
                 ON CONFLICT(document_id) DO UPDATE SET
                    workspace_id = excluded.workspace_id,
                    owner_id = excluded.owner_id,
                    topic_id = excluded.topic_id,
                    task_id = excluded.task_id,
                    source_message_event_id = excluded.source_message_event_id,
                    source_kind = excluded.source_kind,
                    source_uri = excluded.source_uri,
                    title = excluded.title,
                    content_sha256 = excluded.content_sha256,
                    mime_type = excluded.mime_type,
                    visibility = excluded.visibility,
                    metadata_json = excluded.metadata_json,
                    chunk_count = excluded.chunk_count,
                    updated_at = excluded.updated_at",
                params![
                    document_id,
                    input.workspace_id,
                    input.owner_id,
                    input.topic_id,
                    input.task_id,
                    input.source_message_event_id,
                    input.source_kind,
                    input.source_uri,
                    input.title,
                    content_sha256,
                    input.mime_type,
                    visibility,
                    input.metadata_json,
                    i64::try_from(chunk_count).unwrap_or(i64::MAX),
                    now,
                ],
            )?;
            tx.execute(
                "DELETE FROM document_chunks WHERE document_id = ?1",
                params![document_id],
            )?;

            for (chunk_index, heading, content, embedding_bytes) in prepared_chunks {
                let chunk_id = format!("{document_id}:chunk:{chunk_index}");
                let source_anchor = format!("{document_id}#chunk-{chunk_index}");
                let token_estimate = content.chars().count().div_ceil(4);
                let chunk_hash = Self::content_sha256_hex(&content);
                let chunk_embedding_provider = embedding_bytes.as_ref().map(|_| embedding_provider.clone());
                let chunk_embedding_model = embedding_bytes.as_ref().map(|_| embedding_model.clone());
                let chunk_embedding_dimensions = embedding_bytes.as_ref().map(|_| embedding_dimensions);
                tx.execute(
                    "INSERT INTO document_chunks (
                        chunk_id, document_id, workspace_id, owner_id, topic_id, task_id,
                        chunk_index, heading, content, content_sha256, embedding,
                        embedding_provider, embedding_model, embedding_dimensions,
                        source_anchor, token_estimate, created_at
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                    params![
                        chunk_id,
                        document_id,
                        input.workspace_id,
                        input.owner_id,
                        input.topic_id,
                        input.task_id,
                        i64::try_from(chunk_index).unwrap_or(i64::MAX),
                        heading,
                        content,
                        chunk_hash,
                        embedding_bytes,
                        chunk_embedding_provider,
                        chunk_embedding_model,
                        chunk_embedding_dimensions,
                        source_anchor,
                        i64::try_from(token_estimate).unwrap_or(i64::MAX),
                        now,
                    ],
                )?;
            }

            tx.execute(
                // C03: the ingested document's own owner is recorded in the
                // column, so a `session`/`private` ingest event is attributed.
                // V02-R6: the turn's scoping keys are bound too, so the event is
                // reachable through the `session`/`agent`/`private` arms of the
                // read predicate instead of carrying unmatchable NULLs.
                "INSERT INTO memory_events (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                    session_key, agent_id, persona_id, visibility, payload_json, created_at
                 )
                 VALUES (?1, ?2, ?3, 'document.ingested', 'documents', ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    Uuid::new_v4().to_string(),
                    input.workspace_id,
                    input.owner_id,
                    document_id,
                    input.session_key,
                    input.agent_id,
                    input.persona_id,
                    input.visibility.as_str(),
                    serde_json::json!({
                        "owner_id": input.owner_id,
                        "topic_id": input.topic_id,
                        "task_id": input.task_id,
                        "source_message_event_id": input.source_message_event_id,
                        "chunk_count": chunk_count,
                        "content_sha256": content_sha256
                    })
                    .to_string(),
                    Utc::now().to_rfc3339(),
                ],
            )?;

            let document = tx.query_row(
                "SELECT id, document_id, workspace_id, owner_id, topic_id, task_id,
                        source_message_event_id, source_kind, source_uri, title,
                        content_sha256, mime_type, visibility, metadata_json,
                        chunk_count, created_at, updated_at
                 FROM documents
                 WHERE document_id = ?1
                 LIMIT 1",
                params![document_id],
                Self::document_from_row,
            )?;
            tx.commit()?;
            Ok(document)
        })
        .await?
    }

    async fn retrieve_decision_documents(
        &self,
        query: &str,
        limit: u8,
        principal: &MemoryPrincipal,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<super::traits::DecisionDocumentHit>> {
        anyhow::ensure!(
            query.len() <= 1_024 && (1..=5).contains(&limit),
            "invalid decision Document bounds"
        );
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "decision Document deadline expired"
        );
        let fts_query = super::topic::build_safe_fts_query(query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let db_path = self.db_path.clone();
        let principal = principal.clone();
        let owner_id = Self::document_owner_for_principal(&principal);
        let visibility_guard = Self::document_visibility_guard(&principal);
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<super::traits::DecisionDocumentHit>> {
            let conn = Self::open_rehearsal_reader(&db_path, deadline)?;
            let mut stmt = conn.prepare(&format!(
                "SELECT c.chunk_id, c.document_id, c.content, c.created_at
                 FROM document_chunks_fts f
                 JOIN document_chunks c ON c.rowid = f.rowid
                 JOIN documents d ON d.document_id = c.document_id
                 WHERE document_chunks_fts MATCH ?1
                   AND c.workspace_id = ?2 AND d.workspace_id = ?2
                   AND (d.visibility IN ('global', 'workspace'){visibility_guard}
                        OR (?3 IS NOT NULL AND c.owner_id = ?3))
                   AND length(CAST(c.chunk_id AS BLOB)) <= 128
                   AND length(CAST(c.document_id AS BLOB)) <= 128
                   AND length(CAST(c.content AS BLOB)) <= 4096
                   AND length(CAST(c.created_at AS BLOB)) <= 64
                 ORDER BY bm25(document_chunks_fts), c.chunk_id LIMIT ?4"
            ))?;
            let rows = stmt.query_map(
                params![fts_query, principal.workspace_id, owner_id, i64::from(limit)],
                |row| {
                    Ok(super::traits::DecisionDocumentHit {
                        chunk_id: row.get(0)?,
                        document_id: row.get(1)?,
                        content: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                },
            )?;
            let hits = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "decision Document deadline expired"
            );
            Ok(hits)
        })
        .await?
    }

    async fn search_document_chunks(
        &self,
        principal: &MemoryPrincipal,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<DocumentSearchResult>> {
        let pool = self.pool.clone();
        let principal = principal.clone();
        let query = query.to_string();
        let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let owner_id = Self::document_owner_for_principal(&principal);
        // V02-R8: empty for every non-isolated principal, so both statements
        // below stay byte-identical for them.
        let visibility_guard = Self::document_visibility_guard(&principal);
        let query_embedding = self.get_or_compute_embedding(&query).await?;
        let embedding_provider = self.embedding_provider_name();
        let embedding_model = self.embedding_model_name();
        let embedding_dimensions = self.embedder.dimensions();
        let embedding_dimensions_i64 = self.embedding_dimensions_i64();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<DocumentSearchResult>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let fts_query = super::topic::build_safe_fts_query(&query);
            let mut results = Vec::new();
            let mut seen = std::collections::HashSet::new();
            if !fts_query.is_empty() {
                let mut stmt = conn.prepare(&format!(
                    "SELECT c.id, c.chunk_id, c.document_id, c.workspace_id, c.owner_id, c.topic_id, c.task_id,
                            c.chunk_index, c.heading, c.content, c.content_sha256, c.source_anchor,
                            c.token_estimate, c.created_at, bm25(document_chunks_fts) AS score,
                            d.source_kind AS source_kind
                     FROM document_chunks_fts f
                     JOIN document_chunks c ON c.rowid = f.rowid
                     JOIN documents d ON d.document_id = c.document_id
                     WHERE document_chunks_fts MATCH ?1
                       AND c.workspace_id = ?2
                       AND (
                           d.visibility IN ('global', 'workspace'){visibility_guard}
                           OR (?3 IS NOT NULL AND c.owner_id = ?3)
                       )
                     ORDER BY score ASC
                     LIMIT ?4",
                ))?;
                let rows = stmt.query_map(params![fts_query, principal.workspace_id, owner_id, limit], |row| {
                    let chunk = Self::document_chunk_from_row(row)?;
                    let score: f32 = row.get(14)?;
                    let source_kind: Option<String> = row.get(15)?;
                    Ok(DocumentSearchResult {
                        chunk,
                        score,
                        source_kind,
                    })
                })?;
                for row in rows {
                    let result = row?;
                    seen.insert(result.chunk.chunk_id.clone());
                    results.push(result);
                }
            }

            if let Some(query_embedding) = query_embedding {
                let mut stmt = conn.prepare(&format!(
                    "SELECT c.id, c.chunk_id, c.document_id, c.workspace_id, c.owner_id, c.topic_id, c.task_id,
                            c.chunk_index, c.heading, c.content, c.content_sha256, c.source_anchor,
                            c.token_estimate, c.created_at, c.embedding, d.source_kind AS source_kind
                     FROM document_chunks c
                     JOIN documents d ON d.document_id = c.document_id
                     WHERE c.embedding IS NOT NULL
                       AND c.embedding_provider = ?1
                       AND c.embedding_model = ?2
                       AND c.embedding_dimensions = ?3
                       AND c.workspace_id = ?4
                       AND (
                           d.visibility IN ('global', 'workspace'){visibility_guard}
                           OR (?5 IS NOT NULL AND c.owner_id = ?5)
                       )",
                ))?;
                let rows = stmt.query_map(
                    params![
                        embedding_provider,
                        embedding_model,
                        embedding_dimensions_i64,
                        principal.workspace_id,
                        owner_id
                    ],
                    |row| {
                        let chunk = Self::document_chunk_from_row(row)?;
                        let embedding_blob: Vec<u8> = row.get(14)?;
                        let source_kind: Option<String> = row.get(15)?;
                        Ok((chunk, embedding_blob, source_kind))
                    },
                )?;
                let mut vector_results = Vec::new();
                for row in rows {
                    let (chunk, embedding_blob, source_kind) = row?;
                    if seen.contains(&chunk.chunk_id) {
                        continue;
                    }
                    let embedding = vector::bytes_to_vec(&embedding_blob);
                    if embedding.len() != embedding_dimensions {
                        tracing::debug!(
                            chunk_id = %chunk.chunk_id,
                            expected_dimensions = embedding_dimensions,
                            actual_dimensions = embedding.len(),
                            "Skipping stale document chunk embedding with mismatched dimensions"
                        );
                        continue;
                    }
                    let score = vector::cosine_similarity(&query_embedding, &embedding);
                    if score > 0.0 {
                        vector_results.push(DocumentSearchResult {
                            chunk,
                            score,
                            source_kind,
                        });
                    }
                }
                vector_results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
                for result in vector_results {
                    if results.len() >= usize::try_from(limit).unwrap_or(usize::MAX) {
                        break;
                    }
                    seen.insert(result.chunk.chunk_id.clone());
                    results.push(result);
                }
            }

            Ok(results)
        })
        .await?
    }

    async fn get_document_chunk(&self, chunk_id: &str) -> anyhow::Result<Option<DocumentChunkRecord>> {
        let pool = self.pool.clone();
        let chunk_id = chunk_id.to_string();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<DocumentChunkRecord>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            conn.query_row(
                "SELECT id, chunk_id, document_id, workspace_id, owner_id, topic_id, task_id,
                        chunk_index, heading, content, content_sha256, source_anchor,
                        token_estimate, created_at
                 FROM document_chunks
                 WHERE chunk_id = ?1
                 LIMIT 1",
                params![chunk_id],
                Self::document_chunk_from_row,
            )
            .optional()
            .map_err(Into::into)
        })
        .await?
    }

    async fn link_memory_source(&self, input: MemoryLinkInput) -> anyhow::Result<MemoryLink> {
        let pool = self.pool.clone();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<MemoryLink> {
            let conn = pool.write();
            let now = Utc::now().to_rfc3339();
            let link_id = input.link_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            conn.execute(
                "INSERT OR IGNORE INTO memory_links (
                    link_id, workspace_id, owner_id, memory_key, memory_event_id,
                    message_event_id, document_id, chunk_id, link_type, payload_json, created_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    link_id,
                    input.workspace_id,
                    input.owner_id,
                    input.memory_key,
                    input.memory_event_id,
                    input.message_event_id,
                    input.document_id,
                    input.chunk_id,
                    input.link_type,
                    input.payload_json,
                    now,
                ],
            )?;
            conn.query_row(
                "SELECT id, link_id, workspace_id, owner_id, memory_key, memory_event_id,
                        message_event_id, document_id, chunk_id, link_type, payload_json, created_at
                 FROM memory_links
                 WHERE link_id = ?1
                 LIMIT 1",
                params![link_id],
                Self::memory_link_from_row,
            )
            .map_err(Into::into)
        })
        .await?
    }

    async fn append_retrieval_trace(&self, input: RetrievalTraceInput) -> anyhow::Result<RetrievalTrace> {
        let pool = self.pool.clone();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<RetrievalTrace> {
            let conn = pool.write();
            let now = Utc::now().to_rfc3339();
            let trace_id = input.trace_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let candidate_count = i64::try_from(input.candidate_count).unwrap_or(i64::MAX);
            let selected_count = i64::try_from(input.selected_count).unwrap_or(i64::MAX);
            let dropped_count = i64::try_from(input.dropped_count).unwrap_or(i64::MAX);
            let budget_tokens = input
                .budget_tokens
                .map(|value| i64::try_from(value).unwrap_or(i64::MAX));
            conn.execute(
                "INSERT INTO retrieval_traces (
                    trace_id, workspace_id, owner_id, session_key, agent_id, persona_id,
                    source, query, candidate_count, selected_count, dropped_count,
                    budget_tokens, selected_json, dropped_json, payload_json, created_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                 ON CONFLICT(trace_id) DO UPDATE SET
                    workspace_id = excluded.workspace_id,
                    owner_id = excluded.owner_id,
                    session_key = excluded.session_key,
                    agent_id = excluded.agent_id,
                    persona_id = excluded.persona_id,
                    source = excluded.source,
                    query = excluded.query,
                    candidate_count = excluded.candidate_count,
                    selected_count = excluded.selected_count,
                    dropped_count = excluded.dropped_count,
                    budget_tokens = excluded.budget_tokens,
                    selected_json = excluded.selected_json,
                    dropped_json = excluded.dropped_json,
                    payload_json = excluded.payload_json",
                params![
                    trace_id,
                    input.workspace_id,
                    input.owner_id,
                    input.session_key,
                    input.agent_id,
                    input.persona_id,
                    input.source,
                    input.query,
                    candidate_count,
                    selected_count,
                    dropped_count,
                    budget_tokens,
                    input.selected_json,
                    input.dropped_json,
                    input.payload_json,
                    now,
                ],
            )?;
            conn.query_row(
                "SELECT id, trace_id, workspace_id, owner_id, session_key, agent_id,
                        persona_id, source, query, candidate_count, selected_count,
                        dropped_count, budget_tokens, selected_json, dropped_json,
                        payload_json, created_at
                 FROM retrieval_traces
                 WHERE trace_id = ?1
                 LIMIT 1",
                params![trace_id],
                Self::retrieval_trace_from_row,
            )
            .map_err(Into::into)
        })
        .await?
    }

    async fn append_compaction_run(&self, input: CompactionRunInput) -> anyhow::Result<CompactionRun> {
        input.validate_source_event_provenance()?;
        let pool = self.pool.clone();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<CompactionRun> {
            let conn = pool.write();
            let now = Utc::now().to_rfc3339();
            let run_id = input.run_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let source_message_count = i64::try_from(input.source_message_count).unwrap_or(i64::MAX);
            let source_token_estimate = i64::try_from(input.source_token_estimate).unwrap_or(i64::MAX);
            conn.execute(
                "INSERT INTO compaction_runs (
                    run_id, workspace_id, owner_id, session_key, agent_id, persona_id,
                    \"trigger\", mode, source_message_count, source_token_estimate,
                    summary, summary_memory_key, source_event_ids_json,
                    source_event_range_json, source_document_refs_json,
                    fidelity_status, payload_json, created_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
                 ON CONFLICT(run_id) DO UPDATE SET
                    workspace_id = excluded.workspace_id,
                    owner_id = excluded.owner_id,
                    session_key = excluded.session_key,
                    agent_id = excluded.agent_id,
                    persona_id = excluded.persona_id,
                    \"trigger\" = excluded.\"trigger\",
                    mode = excluded.mode,
                    source_message_count = excluded.source_message_count,
                    source_token_estimate = excluded.source_token_estimate,
                    summary = excluded.summary,
                    summary_memory_key = excluded.summary_memory_key,
                    source_event_ids_json = excluded.source_event_ids_json,
                    source_event_range_json = excluded.source_event_range_json,
                    source_document_refs_json = excluded.source_document_refs_json,
                    fidelity_status = excluded.fidelity_status,
                    payload_json = excluded.payload_json",
                params![
                    run_id,
                    input.workspace_id,
                    input.owner_id,
                    input.session_key,
                    input.agent_id,
                    input.persona_id,
                    input.trigger,
                    input.mode,
                    source_message_count,
                    source_token_estimate,
                    input.summary,
                    input.summary_memory_key,
                    input.source_event_ids_json,
                    input.source_event_range_json,
                    input.source_document_refs_json,
                    input.fidelity_status,
                    input.payload_json,
                    now,
                ],
            )?;
            conn.query_row(
                "SELECT id, run_id, workspace_id, owner_id, session_key, agent_id,
                        persona_id, \"trigger\", mode, source_message_count, source_token_estimate,
                        summary, summary_memory_key, source_event_ids_json,
                        source_event_range_json, source_document_refs_json,
                        fidelity_status, payload_json, created_at
                 FROM compaction_runs
                 WHERE run_id = ?1
                 LIMIT 1",
                params![run_id],
                Self::compaction_run_from_row,
            )
            .map_err(Into::into)
        })
        .await?
    }

    async fn health_check(&self) -> bool {
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            // Probed on a reader: readers are opened lazily, so this is the
            // first place a database that read traffic can no longer open
            // becomes visible.
            pool.read()
                .and_then(|reader| reader.connection().map(|conn| conn.execute_batch("SELECT 1").is_ok()))
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false)
    }

    async fn reindex(&self) -> anyhow::Result<usize> {
        Self::reindex(self).await
    }
}

#[cfg(test)]
mod tests;

impl SqliteMemory {
    async fn append_event_with_recovery(
        &self,
        input: MessageEventInput,
        mutation: Option<crate::memory::ingress_recovery::AtomicIngressMutation>,
        repair_deadline: Option<std::time::Instant>,
    ) -> anyhow::Result<(MessageEvent, bool)> {
        input.validate()?;
        let pool = self.pool.clone();
        let deadline = mutation.as_ref().map(ingress_recovery::deadline).or(repair_deadline);
        let repair_owner = repair_deadline
            .map(|_| self.repair_owner(&input.workspace_id))
            .transpose()?;

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<(MessageEvent, bool)> {
            // The mutation owns the genuine lease inside this blocking closure,
            // including when its async waiter is cancelled.
            let conn = match deadline { Some(deadline) => pool.write_until(deadline)?, None => pool.write() };
            let _budget = deadline.map(|deadline| ingress_recovery::ConnectionDeadline::new(&conn, deadline)).transpose()?;
            let tx = conn.unchecked_transaction()?;
            if let (Some(owner), Some(deadline)) = (&repair_owner, repair_deadline) {
                ingress_recovery::validate_owner(&tx, owner, &input.workspace_id, deadline)?;
            }
            if let Some(operation) = &mutation { ingress_recovery::before_append(&tx, operation, &input)?; }
            let now = Utc::now().to_rfc3339();
            let event_id = input.event_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let content_hash = Self::content_hash(&input.content);
            let visibility = input.visibility.as_str().to_string();
            let source = input.source.as_str().to_string();
            let source_ref_json = serde_json::to_string(&input.source)?;
            let subject_ref_json = input.subject.as_ref().map(serde_json::to_string).transpose()?;
            let config_generation_id = input.config_generation_id.map(i64::try_from).transpose()?;

            let inserted = tx.execute(
                "INSERT OR IGNORE INTO message_events (
                    event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                    parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                    sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                    goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                    content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                         ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29,
                         ?30, ?31)",
                params![
                    event_id,
                    input.idempotency_key,
                    input.workspace_id,
                    input.owner_id,
                    source,
                    input.channel,
                    input.session_key,
                    input.parent_session_key,
                    input.run_id,
                    input.parent_run_id,
                    input.agent_id,
                    input.persona_id,
                    input.sender,
                    input.recipient,
                    input.role,
                    input.event_type,
                    source_ref_json,
                    subject_ref_json,
                    input.goal_id,
                    input.causation_event_id,
                    input.correlation_id,
                    input.attempt_id,
                    input.lease_epoch,
                    config_generation_id,
                    input.config_source_revision,
                    input.content,
                    content_hash,
                    input.raw_payload_json,
                    visibility,
                    now,
                    now
                ],
            )?;

            if deadline.is_some() {
                ingress_recovery::check_receipt_size(&tx, &event_id, input.idempotency_key.as_deref(), &input.workspace_id)?;
            }

            let event = if let Some(ref idempotency_key) = input.idempotency_key {
                tx.query_row(
                    "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                            parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                            sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                            goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                            content, content_hash, raw_payload_json, visibility, created_at, updated_at
                     FROM message_events
                     WHERE event_id = ?1
                        OR (workspace_id = ?3 AND idempotency_key = ?2)
                     ORDER BY CASE WHEN event_id = ?1 THEN 0 ELSE 1 END
                     LIMIT 1",
                    params![event_id, idempotency_key, input.workspace_id],
                    Self::message_event_from_row,
                )?
            } else {
                tx.query_row(
                    "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                            parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                            sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                            goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                            content, content_hash, raw_payload_json, visibility, created_at, updated_at
                     FROM message_events
                     WHERE event_id = ?1
                     LIMIT 1",
                    params![event_id],
                    Self::message_event_from_row,
                )?
            };

            if inserted > 0 {
                tx.execute(
                    "INSERT INTO memory_events (
                        event_id, workspace_id, owner_id, event_type, subject_table, subject_id, session_key,
                        agent_id, persona_id, visibility, payload_json, created_at
                     )
                     VALUES (?1, ?2, ?3, ?4, 'message_events', ?5, ?6, ?7, ?8, ?9, NULL, ?10)",
                    params![
                        Uuid::new_v4().to_string(),
                        event.workspace_id,
                        event.owner_id,
                        event.event_type,
                        event.event_id,
                        event.session_key,
                        event.agent_id,
                        event.persona_id,
                        event.visibility.as_str(),
                        Utc::now().to_rfc3339()
                    ],
                )?;
            }

            if let Some(operation) = &mutation { ingress_recovery::after_append(&tx, operation, &event, inserted > 0)?; }
            if let (Some(owner), Some(deadline)) = (&repair_owner, repair_deadline) {
                ingress_recovery::validate_owner(&tx, owner, &event.workspace_id, deadline)?;
            }
            if let Some(deadline) = deadline { super::ingress_recovery::remaining(deadline)?; }
            tx.commit()?;
            if let Some(deadline) = deadline { super::ingress_recovery::remaining(deadline)?; }
            Ok((event, inserted > 0))
        })
        .await?
    }
}

impl SqliteMemory {
    fn repair_owner(&self, workspace: &str) -> anyhow::Result<Arc<dyn super::ingress_recovery::IngressLifetime>> {
        self.ingress_owners
            .cell(workspace)?
            .cell
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("SQLite ingress repair requires an initialized owner"))
    }

    async fn project_console_message_with_deadline(
        &self,
        event: &MessageEvent,
        deadline: Option<std::time::Instant>,
    ) -> anyhow::Result<()> {
        if let Some(deadline) = deadline {
            super::ingress_recovery::remaining(deadline)?;
            ingress_recovery::validate_event_bounds(event)?;
        }
        event.validate_console_projection()?;
        let repair_owner = deadline.map(|_| self.repair_owner(&event.workspace_id)).transpose()?;
        let event = event.clone();
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = match deadline { Some(deadline) => pool.write_until(deadline)?, None => pool.write() };
            let _budget = deadline.map(|deadline| ingress_recovery::ConnectionDeadline::new(&conn, deadline)).transpose()?;
            let tx = conn.unchecked_transaction()?;
            if let (Some(owner), Some(deadline)) = (&repair_owner, deadline) {
                ingress_recovery::validate_owner(&tx, owner, &event.workspace_id, deadline)?;
                ingress_recovery::check_receipt_size(&tx, &event.event_id, None, &event.workspace_id)?;
                ingress_recovery::check_console_projection_size(&tx, &event.event_id)?;
            }
            let durable = tx.query_row(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events WHERE event_id = ?1 AND workspace_id = ?2 AND owner_id = ?3",
                params![event.event_id, event.workspace_id, event.owner_id], Self::message_event_from_row,
            )?;
            anyhow::ensure!(serde_json::to_value(&durable)? == serde_json::to_value(&event)?,
                "console projection differs from its durable event");
            let existing: Vec<(String, Option<String>, String, String)> = {
                let mut stmt = tx.prepare("SELECT session_key, owner_id, role, content FROM conversation_turns
                    WHERE message_event_id = ?1 OR message_id = ?1 LIMIT 2")?;
                stmt.query_map(params![event.event_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            if let Some(row) = existing.first() {
                anyhow::ensure!(existing.len() == 1 && Some(&row.0) == event.session_key.as_ref()
                    && row.1 == event.owner_id && row.2 == event.role && row.3 == event.content,
                    "console message projection conflicts with its durable event");
                if let (Some(owner), Some(deadline)) = (&repair_owner, deadline) {
                    ingress_recovery::validate_owner(&tx, owner, &event.workspace_id, deadline)?;
                }
                return Ok(());
            }
            let preview = Self::conversation_preview(&event.content);
            tx.execute("INSERT INTO sessions
                (session_key, channel, sender, owner_id, created_at, updated_at, message_count, last_message_preview)
                VALUES (?1, ?2, ?3, ?4, ?5, ?5, 1, ?6)
                ON CONFLICT(session_key) DO UPDATE SET
                    message_count = COALESCE(sessions.message_count, 0) + 1,
                    channel = CASE WHEN excluded.updated_at >= sessions.updated_at THEN excluded.channel ELSE sessions.channel END,
                    sender = CASE WHEN excluded.updated_at >= sessions.updated_at THEN excluded.sender ELSE sessions.sender END,
                    owner_id = CASE WHEN excluded.updated_at >= sessions.updated_at THEN excluded.owner_id ELSE sessions.owner_id END,
                    last_message_preview = CASE WHEN excluded.updated_at >= sessions.updated_at THEN excluded.last_message_preview ELSE sessions.last_message_preview END,
                    updated_at = MAX(sessions.updated_at, excluded.updated_at)",
                params![event.session_key, event.channel, event.sender, event.owner_id, event.created_at, preview])?;
            tx.execute("INSERT INTO conversation_turns
                (session_key, owner_id, role, content, timestamp, message_id, message_event_id, agent_id, persona_id, visibility)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, 'session')",
                params![event.session_key, event.owner_id, event.role, event.content, event.created_at,
                    event.event_id, event.agent_id, event.persona_id])?;
            if let (Some(owner), Some(deadline)) = (&repair_owner, deadline) {
                ingress_recovery::validate_owner(&tx, owner, &event.workspace_id, deadline)?;
            }
            tx.commit()?;
            if let Some(deadline) = deadline { super::ingress_recovery::remaining(deadline)?; }
            Ok(())
        }).await?
    }
}

/// Narrow read-only observations over actual owned fixture databases.
/// No arbitrary SQL or raw connection is exposed to fixture callers.
#[cfg(test)]
pub(crate) mod fixture_readback {
    pub(crate) struct TerminalRow {
        #[cfg(feature = "llm-router")]
        pub(crate) run_id: String,
        pub(crate) owner_id: Option<String>,
        pub(crate) raw_payload: String,
    }
    pub(crate) fn terminal_rows(workspace: &std::path::Path) -> rusqlite::Result<Vec<TerminalRow>> {
        let connection = rusqlite::Connection::open_with_flags(
            workspace.join("memory/brain.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        connection.prepare("SELECT run_id, owner_id, raw_payload_json FROM message_events WHERE event_type='turn.finalized' ORDER BY id")?
            .query_map([], |row| Ok(TerminalRow { #[cfg(feature = "llm-router")] run_id: row.get(0)?, owner_id: row.get(1)?, raw_payload: row.get(2)? }))?
            .collect()
    }
    pub(crate) fn tool_event_count(workspace: &std::path::Path) -> rusqlite::Result<i64> {
        let connection = rusqlite::Connection::open_with_flags(
            workspace.join("memory/brain.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        connection.query_row("SELECT count(*) FROM message_events WHERE event_type IN ('tool.execution.reserved','tool.execution.finalized')", [], |row| row.get(0))
    }
    #[cfg(feature = "llm-router")]
    pub(crate) fn router_consumption_count(
        workspace: &std::path::Path,
        run: &str,
        source_workspace: &str,
        owner: &str,
        source: &str,
    ) -> rusqlite::Result<i64> {
        let connection = rusqlite::Connection::open_with_flags(
            workspace.join("memory/brain.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        connection.query_row(
            "SELECT COUNT(*) FROM router_learning_receipts r
                 JOIN router_learning_state l ON l.domain=r.domain
                 JOIN message_events t ON t.run_id=r.terminal AND t.workspace_id=r.workspace AND t.owner_id=r.owner
                 JOIN message_events s ON s.event_id=t.causation_event_id
                 WHERE r.terminal=?1 AND r.workspace=?2 AND r.owner=?3
                   AND length(r.domain)=64 AND length(r.fingerprint)=64 AND l.version=1
                   AND t.event_type='turn.finalized' AND s.event_id=?4
                   AND s.event_type='message.created' AND s.role='user'
                   AND s.owner_id=t.owner_id AND s.workspace_id=t.workspace_id
                   AND s.session_key=t.session_key AND s.run_id=t.run_id AND s.id<t.id",
            rusqlite::params![run, source_workspace, owner, source],
            |row| row.get(0),
        )
    }

    /// Fixed observations of the actual Automix receipts; connection stays private.
    #[cfg(feature = "llm-router")]
    pub(crate) struct AutomixReadback {
        connection: rusqlite::Connection,
    }
    #[cfg(feature = "llm-router")]
    impl AutomixReadback {
        pub(crate) fn open(workspace: &std::path::Path) -> rusqlite::Result<Self> {
            Ok(Self {
                connection: rusqlite::Connection::open_with_flags(
                    workspace.join("memory/brain.db"),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )?,
            })
        }
        pub(crate) fn auxiliary_count(&self) -> rusqlite::Result<i64> {
            self.connection.query_row(
                "SELECT COUNT(*) FROM message_events WHERE event_type='provider.auxiliary.settled'",
                [],
                |row| row.get(0),
            )
        }
        pub(crate) fn auxiliary_payload(&self) -> rusqlite::Result<String> {
            self.connection.query_row(
                "SELECT raw_payload_json FROM message_events WHERE event_type='provider.auxiliary.settled'",
                [],
                |row| row.get(0),
            )
        }
        pub(crate) fn auxiliary_terminal_count(&self, run: &str, workspace: &str) -> rusqlite::Result<i64> {
            self.connection.query_row(
                "SELECT COUNT(*) FROM message_events a JOIN message_events t
                 ON a.workspace_id=t.workspace_id AND a.owner_id=t.owner_id
                   AND a.session_key=t.session_key AND a.run_id=t.run_id
                 WHERE a.event_type='provider.auxiliary.settled' AND t.event_type='turn.finalized'
                   AND a.id<t.id AND t.run_id=?1 AND a.workspace_id=?2
                   AND a.idempotency_key=('turn:' || t.run_id || ':automix_premium')",
                rusqlite::params![run, workspace],
                |row| row.get(0),
            )
        }
        pub(crate) fn learning_state(&self) -> rusqlite::Result<(String, String)> {
            self.connection
                .query_row("SELECT domain,snapshot FROM router_learning_state", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
        }
        pub(crate) fn learning_receipt_count(&self) -> rusqlite::Result<i64> {
            self.connection
                .query_row("SELECT COUNT(*) FROM router_learning_receipts", [], |row| row.get(0))
        }
        pub(crate) fn learning_terminal_count(
            &self,
            run: &str,
            domain: &str,
            workspace: &str,
        ) -> rusqlite::Result<i64> {
            self.connection.query_row(
                "SELECT COUNT(*) FROM router_learning_receipts r
                 JOIN message_events t ON t.run_id=r.terminal AND t.workspace_id=r.workspace AND t.owner_id=r.owner
                 JOIN message_events s ON s.event_id=t.causation_event_id
                 WHERE r.terminal=?1 AND r.domain=?2 AND r.workspace=?3
                   AND length(r.fingerprint)=64 AND t.event_type='turn.finalized'
                   AND s.event_type='message.created' AND s.role='user'
                   AND s.owner_id=t.owner_id AND s.workspace_id=t.workspace_id
                   AND s.session_key=t.session_key AND s.run_id=t.run_id AND s.id<t.id",
                rusqlite::params![run, domain, workspace],
                |row| row.get(0),
            )
        }
    }
}

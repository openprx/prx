mod approval_grants;
mod documents;
mod drafts;
mod event_batch;
mod events;
mod evolution;
mod ingress_recovery;
mod memory_impl;
mod mirror;
mod recall;
mod router_history;
mod router_learning;
mod rows;
mod schema;

use super::embeddings::EmbeddingProvider;
use super::principal::{ChatType, MemoryWriteContext, Principal, Role, Visibility, resolve_principal};
use super::traits::{MemoryPrincipal, MessageEvent, MessageEventInput};
use super::vector;
use crate::runtime::sqlite_pool::SqliteConnectionPool;
use anyhow::Context;
use chrono::{DateTime, Utc};
use recall::VECTOR_CANDIDATE_CAP;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use approval_grants::{
    SqliteTaskEventMirror, append_task_event_mirror, append_task_event_mirror_idempotent, init_approval_grant_schema,
    is_approval_grant_revoked, revoke_approval_grant,
};

const DEFAULT_CONVERSATION_LIMIT: usize = 50;
const MAX_CONVERSATION_QUERY_LIMIT: usize = 500;
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
/// tables that never hit disk. `secure_delete` zeros a cell when it is
/// removed so a later `wal_checkpoint(TRUNCATE)` does not leave the payload
/// in a free block of `brain.db`.
const SQLITE_WRITER_PRAGMAS: &str = "PRAGMA synchronous   = NORMAL;
     PRAGMA foreign_keys  = ON;
     PRAGMA secure_delete = ON;
     PRAGMA mmap_size     = 8388608;
     PRAGMA cache_size    = -2000;
     PRAGMA temp_store    = MEMORY;";

/// Reader-pool and writer-contention counters for this backend.
///
/// Re-exported from the shared pool so callers keep reaching it at
/// `memory::sqlite::SqlitePoolStats`.
pub use crate::runtime::sqlite_pool::SqlitePoolStats;

/// Make `path` and its parent owner-only, plus any sqlite wal siblings.
fn restrict_owner_mode(db_path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let restrict = |path: &Path, mode: u32| -> anyhow::Result<()> {
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    };
    if let Some(parent) = db_path.parent() {
        restrict(parent, 0o700)?;
    }
    restrict(db_path, 0o600)?;
    let mut wal = db_path.as_os_str().to_os_string();
    wal.push("-wal");
    restrict(Path::new(&wal), 0o600)?;
    let mut shm = db_path.as_os_str().to_os_string();
    shm.push("-shm");
    restrict(Path::new(&shm), 0o600)?;
    Ok(())
}

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

#[cfg(test)]
pub struct WriterRelease {
    flag: parking_lot::Mutex<bool>,
    cv: parking_lot::Condvar,
}

#[cfg(test)]
impl WriterRelease {
    pub fn new() -> Self {
        Self {
            flag: parking_lot::Mutex::new(false),
            cv: parking_lot::Condvar::new(),
        }
    }

    pub fn wait(&self) {
        let mut flag = self.flag.lock();
        while !*flag {
            self.cv.wait(&mut flag);
        }
    }

    pub fn release(&self) {
        *self.flag.lock() = true;
        self.cv.notify_all();
    }

    pub fn reset(&self) {
        *self.flag.lock() = false;
    }
}

#[cfg(test)]
pub type WriterHold = std::sync::Arc<dyn Fn(&WriterRelease, tokio::sync::oneshot::Sender<()>) + Send + Sync>;

impl SqliteMemory {
    /// Hold the sqlite writer until `release` is signaled, then notify `entered`.
    #[cfg(test)]
    #[must_use]
    pub fn writer_hold(&self) -> WriterHold {
        let pool = std::sync::Arc::clone(&self.pool);
        std::sync::Arc::new(
            move |release: &WriterRelease, entered: tokio::sync::oneshot::Sender<()>| {
                let _guard = pool.write();
                let _ = entered.send(());
                release.wait();
            },
        )
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
        // SQLite creates the file as 0666 masked by the process umask. A
        // group-writable umask makes the ingress lease reject the database.
        // Tighten the directory and the file without changing the process umask.
        restrict_owner_mode(&db_path)?;

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
}

#[cfg(test)]
mod tests;

/// Narrow read-only observations over actual owned fixture databases.
/// No arbitrary SQL or raw connection is exposed to fixture callers.
#[cfg(test)]
pub(crate) mod fixture_readback;

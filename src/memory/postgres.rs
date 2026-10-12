mod decision_reads;
mod ingress_recovery;
mod router_history;
mod router_learning;

use super::principal::{ChatType, MemoryWriteContext, Principal, Role, ScopeParam, Visibility, classify_memory};
use super::traits::{
    ChatProfile, CompactionRun, CompactionRunInput, ConversationTurn, DocumentChunkRecord, DocumentIngestInput,
    DocumentRecord, DocumentSearchResult, Memory, MemoryCategory, MemoryDraft, MemoryDraftInput, MemoryEntry,
    MemoryEvent, MemoryEventInput, MemoryLink, MemoryLinkInput, MemoryPrincipal, MemoryReadMode, MemoryStoreMetadata,
    MemoryVisibility, MessageEvent, MessageEventInput, RetrievalTrace, RetrievalTraceInput, SessionContextQuery,
    SharedContextQuery, validate_memory_write_target,
};
use super::{embeddings, vector};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::{Condvar, Mutex, MutexGuard};
use postgres::{Client, GenericClient, NoTls, Row};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// D11: lower the shared dialect-agnostic scope parameters into owned strings
/// that the Postgres backend can bind by reference.
///
/// Every scope bind is a text column, so each [`ScopeParam`] becomes a `String`.
/// Returning owned values lets the caller hold them alive for the duration of
/// the `client.query` call and bind `&String` (which implements
/// `postgres::types::ToSql`) — keeping the query fully parameterized (rule 9).
fn scope_params_to_strings(params: Vec<ScopeParam>) -> Vec<String> {
    params
        .into_iter()
        .map(|param| match param {
            ScopeParam::Text(text) => text,
        })
        .collect()
}

/// Maximum allowed connect timeout (seconds) to avoid unreasonable waits.
/// Placeholder dialect for D4 read-merge `session_key` predicate fragments.
const PG_DIALECT: crate::memory::session_predicate::PlaceholderDialect =
    crate::memory::session_predicate::PlaceholderDialect::Postgres;
const POSTGRES_CONNECT_TIMEOUT_CAP_SECS: u64 = 300;
const POSTGRES_EMBEDDING_CACHE_MAX_ROWS: i64 = 10_000;
/// D-028: the read-only policy that shares `public` memory rows across owners.
const MEMORIES_PUBLIC_READ_POLICY: &str = "memories_public_read";

/// Migrations the isolation evidence verifies as recorded with the checksum
/// this build carries: 21 (owner-isolation policies), 23 (their shared read
/// arm closed to owner-isolated callers), 24 (`memories_public_read`).
const VERIFIED_ISOLATION_MIGRATIONS: [i64; 3] = [21, 23, 24];

/// One `pg_policies` row, as the isolation evidence reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PolicyRow {
    name: String,
    /// `PERMISSIVE` or `RESTRICTIVE`.
    permissive: String,
    roles: Vec<String>,
}

/// A vector table must carry exactly the `expected` policies, each
/// permissive and granted to `PUBLIC`, as `init_vector_rls` installs them.
///
/// Any other policy fails the evidence: a permissive one widens access by
/// OR, and a restrictive or role-limited variant of an expected one breaks
/// the reads it exists to grant.
fn check_policy_inventory(target: &str, rows: &[PolicyRow], expected: &[&str]) -> Result<()> {
    let mut found = rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>();
    found.sort_unstable();
    let mut wanted = expected.to_vec();
    wanted.sort_unstable();
    anyhow::ensure!(
        found == wanted,
        "A04 table {target} carries policies {found:?}; expected exactly {wanted:?}"
    );
    for row in rows {
        anyhow::ensure!(
            row.permissive == "PERMISSIVE" && row.roles == ["public"],
            "A04 policy {} on {target} is not a permissive policy for PUBLIC",
            row.name
        );
    }
    Ok(())
}

/// Default ceiling on live pooled PostgreSQL connections.
///
/// PRX is positioned as an LLM agent runtime that does not cap task
/// concurrency, so the pool is sized generously and exists for connection
/// *reuse*, never as a throttle: a caller that finds it saturated queues for a
/// connection instead of failing. Deployments with a different
/// `max_connections` budget override this via
/// `[storage.provider.config].pool_max_size`.
pub(crate) const POSTGRES_DEFAULT_POOL_MAX_SIZE: usize = 32;

/// Default timeout for *establishing* a connection when none is configured.
///
/// This is deliberately the only timeout ordinary pooled operations apply. A stalled TCP or
/// startup handshake is a network fault rather than a long-running task, so
/// bounding it keeps a dead endpoint from wedging checkouts forever. Statement
/// execution on that pool is never bounded and waiting for a pooled connection
/// is never bounded either,
/// because long queries and queueing are both normal for agent workloads.
/// Decision rehearsal uses its own read-only connection and caller deadline;
/// it never changes these ordinary pool settings.
const POSTGRES_DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 15;

/// FIX-P2-03: safety cap on the BYTEA-fallback (no-pgvector) candidate scan.
///
/// When pgvector is unavailable we cannot `ORDER BY` vector distance in SQL, so
/// candidates must be re-ranked in-process by cosine similarity. The original
/// code fetched *every* matching row (unbounded scan = DoS risk on large
/// tables); the fix bounds the scan. This is a **pure safety valve** to prevent
/// an unbounded scan — NOT a relevance proxy. It is deliberately much larger
/// than the typical `4 * limit` candidate pool the pgvector path uses, so the
/// in-process cosine re-rank still sees enough genuinely-similar rows and we do
/// not silently drop "older but more similar" records by truncating on
/// recency. This is the known, documented degradation tradeoff of running
/// without pgvector. Candidates are bounded *after* the scope/owner/session
/// `WHERE` filter, so a narrow scope keeps the scan small regardless.
const POSTGRES_BYTEA_FALLBACK_CANDIDATE_CAP: i64 = 4_096;

/// PostgreSQL-backed persistent memory.
///
/// This backend focuses on reliable CRUD and keyword recall using SQL, without
/// requiring extension setup (for example pgvector).
pub struct PostgresMemory {
    ingress_owners: crate::memory::ingress_recovery::IngressOwnerCache,
    ingress_owner_id: std::sync::OnceLock<uuid::Uuid>,
    client: Arc<PostgresConnectionPool>,
    /// Quoted schema identifier (for example `"public"`). Used to build the
    /// qualified names of schema-scoped helper tables (sessions,
    /// conversation_turns, identity_bindings, user_policies, access_audit_log)
    /// that live directly under the schema rather than off the base table.
    schema_ident: String,
    qualified_table: String,
    qualified_message_events_table: String,
    qualified_memory_events_table: String,
    qualified_drafts_table: String,
    qualified_documents_table: String,
    qualified_document_chunks_table: String,
    qualified_memory_links_table: String,
    qualified_retrieval_traces_table: String,
    qualified_compaction_runs_table: String,
    qualified_embedding_cache_table: String,
    embedding_cache_max_rows: i64,
    pgvector_available: bool,
    embedder: Arc<dyn embeddings::EmbeddingProvider>,
    vector_weight: f32,
    keyword_weight: f32,
}

/// Point-in-time snapshot of PostgreSQL connection pool utilisation.
///
/// Exposed so an observability surface can chart saturation without reaching
/// into pool internals. `in_use + idle` is the number of live connections the
/// pool currently owns, and `waiters` is how many checkouts are queued because
/// the pool is already at `max_size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PostgresPoolStats {
    /// Configured ceiling on live connections.
    pub max_size: usize,
    /// Connections checked out, including in-flight connect attempts.
    pub in_use: usize,
    /// Established connections parked for reuse.
    pub idle: usize,
    /// Checkouts currently blocked waiting for a connection to come back.
    pub waiters: usize,
    /// Total checkouts served since startup.
    pub acquisitions: u64,
    /// Checkouts that had to wait because the pool was saturated.
    pub saturated_acquisitions: u64,
    /// Cumulative time callers spent waiting to obtain a connection.
    pub total_wait: Duration,
    /// Connections established since startup.
    pub connects: u64,
    /// Connections torn down instead of recycled.
    pub discards: u64,
}

#[derive(Debug, Default)]
struct PostgresPoolMetrics {
    acquisitions: AtomicU64,
    saturated_acquisitions: AtomicU64,
    wait_nanos: AtomicU64,
    connects: AtomicU64,
    discards: AtomicU64,
}

/// Mutable pool state guarded by a single lock.
struct PostgresPoolState {
    /// Established connections available for reuse.
    idle: Vec<Client>,
    /// Connections handed to callers plus slots reserved for connect attempts.
    /// Tracked separately from `idle` so `leased + idle.len()` never exceeds
    /// `max_size`.
    leased: usize,
    /// Callers currently parked on `available`.
    waiters: usize,
    /// Set during teardown so queued callers fail instead of hanging forever.
    closed: bool,
}

/// The row-level-security context a checkout is placed in before any caller
/// SQL runs.
///
/// PostgreSQL RLS settings are session-scoped while pooled connections are
/// reused across callers, so the context is a property of the *checkout*, not
/// of the connection: [`PostgresConnectionPool::acquire_reset`] re-establishes
/// it on every checkout, including recycled ones. That is what keeps one
/// caller's privileges from surviving into the next caller's work, in either
/// direction, without the two having to know about each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PostgresSessionContext {
    /// Trusted runtime storage (memory, cron). Bypasses row-level security:
    /// these callers own the isolation contract themselves and bind
    /// transaction-local workspace/owner scoping wherever a principal is
    /// involved.
    System,
    /// Callers that ingest externally-originated data (webhook). Row-level
    /// security applies in full, and no workspace/owner context is bound, so
    /// the `memories` policies deny. This is the same boundary these callers
    /// had while they opened their own connections, and moving them onto the
    /// shared pool must not widen it.
    Restricted,
}

impl PostgresSessionContext {
    /// The statement that pins this context onto a freshly checked-out
    /// connection.
    ///
    /// Session-scoped (`set_config` `is_local = false`) rather than
    /// transaction-local, because callers also issue statements outside an
    /// explicit transaction, where a transaction-local setting would revert
    /// before it could take effect. `Restricted` additionally clears the
    /// owner-scoping GUCs so its denial does not depend on what ran on this
    /// connection previously.
    const fn reset_statement(self) -> &'static str {
        match self {
            Self::System => "SELECT set_config('prx.rls_bypass', 'on', false)",
            Self::Restricted => {
                "SELECT set_config('prx.rls_bypass', 'off', false), \
                        set_config('prx.current_workspace', '', false), \
                        set_config('prx.current_owner', '', false)"
            }
        }
    }
}

/// A bounded pool of synchronous PostgreSQL connections.
///
/// The `postgres` crate ships a blocking client with no pool of its own, and
/// every caller here already runs inside `tokio::task::spawn_blocking`, so a
/// blocking pool is the right shape. It is built on the `parking_lot`
/// primitives this crate already depends on rather than pulling in `r2d2`,
/// because `r2d2` cannot express an unbounded checkout wait: its `get()`
/// always carries a deadline, and failing work merely because the pool is
/// momentarily busy is exactly the behaviour this runtime must not have.
///
/// The pool is a connection *reuse* mechanism, not a concurrency limiter.
/// Connections are opened lazily on first demand.
pub(crate) struct PostgresConnectionPool {
    state: Mutex<PostgresPoolState>,
    /// Signalled whenever a connection is returned or a reserved slot is freed.
    available: Condvar,
    db_url: String,
    connect_timeout: Duration,
    max_size: usize,
    metrics: PostgresPoolMetrics,
}

impl PostgresConnectionPool {
    pub(crate) fn new(db_url: String, connect_timeout_secs: Option<u64>, max_size: usize) -> Self {
        let timeout_secs = connect_timeout_secs
            .unwrap_or(POSTGRES_DEFAULT_CONNECT_TIMEOUT_SECS)
            .clamp(1, POSTGRES_CONNECT_TIMEOUT_CAP_SECS);
        Self {
            state: Mutex::new(PostgresPoolState {
                idle: Vec::new(),
                leased: 0,
                waiters: 0,
                closed: false,
            }),
            available: Condvar::new(),
            db_url,
            connect_timeout: Duration::from_secs(timeout_secs),
            max_size: max_size.max(1),
            metrics: PostgresPoolMetrics::default(),
        }
    }

    /// Open a new connection.
    ///
    /// Only the handshake is bounded: `connect_timeout` covers TCP/startup, and
    /// nothing here sets `statement_timeout`.
    fn connect_new(&self) -> Result<Client> {
        let mut config: postgres::Config = self.db_url.parse().context("invalid PostgreSQL connection URL")?;
        config.connect_timeout(self.connect_timeout);
        config
            .connect(NoTls)
            .context("failed to connect to PostgreSQL memory backend")
    }

    /// Check out a connection, waiting indefinitely when the pool is saturated.
    ///
    /// Queueing is the intended back-pressure: this runtime does not cap task
    /// concurrency, so a busy pool must slow callers down rather than fail work
    /// that is only waiting its turn. The wait is therefore deadline-free, and
    /// its duration is recorded instead so saturation stays observable.
    fn acquire(&self) -> Result<PooledConnection<'_>> {
        let started = Instant::now();
        let mut waited = false;
        let mut guard = self.state.lock();
        loop {
            if guard.closed {
                anyhow::bail!("PostgreSQL memory backend pool is closed");
            }

            if let Some(client) = guard.idle.pop() {
                if client.is_closed() {
                    // Never hand out a connection the server already closed.
                    MutexGuard::unlocked(&mut guard, || {
                        self.metrics.discards.fetch_add(1, Ordering::Relaxed);
                        drop_client_off_runtime(client);
                    });
                    continue;
                }
                guard.leased += 1;
                self.record_acquisition(started, waited);
                return Ok(PooledConnection {
                    pool: self,
                    client: Some(client),
                    recycled: true,
                });
            }

            if guard.leased < self.max_size {
                // Reserve the slot before unlocking so concurrent callers
                // cannot collectively overshoot `max_size` while this handshake
                // is in flight.
                guard.leased += 1;
                match MutexGuard::unlocked(&mut guard, || self.connect_new()) {
                    Ok(client) => {
                        self.metrics.connects.fetch_add(1, Ordering::Relaxed);
                        self.record_acquisition(started, waited);
                        return Ok(PooledConnection {
                            pool: self,
                            client: Some(client),
                            recycled: false,
                        });
                    }
                    Err(error) => {
                        // Hand the reserved slot back, otherwise a transient
                        // connect failure would permanently shrink the pool.
                        guard.leased = guard.leased.saturating_sub(1);
                        self.available.notify_one();
                        return Err(error);
                    }
                }
            }

            waited = true;
            guard.waiters += 1;
            self.available.wait(&mut guard);
            guard.waiters = guard.waiters.saturating_sub(1);
        }
    }

    fn record_acquisition(&self, started: Instant, waited: bool) {
        let elapsed = started.elapsed();
        self.metrics.acquisitions.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .wait_nanos
            .fetch_add(u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX), Ordering::Relaxed);
        if waited {
            self.metrics.saturated_acquisitions.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                waited_ms = elapsed.as_millis(),
                max_size = self.max_size,
                "PostgreSQL connection pool saturated; checkout was queued"
            );
        }
    }

    /// Return a connection, or release a reserved slot when `client` is `None`.
    fn release_slot(&self, client: Option<Client>) {
        let discarded = {
            let mut guard = self.state.lock();
            guard.leased = guard.leased.saturating_sub(1);
            match client {
                // A connection the server closed underneath us must never be
                // recycled, or the next caller inherits the failure.
                Some(client) if !guard.closed && !client.is_closed() => {
                    guard.idle.push(client);
                    None
                }
                other => other,
            }
        };
        self.available.notify_one();
        if let Some(client) = discarded {
            self.metrics.discards.fetch_add(1, Ordering::Relaxed);
            drop_client_off_runtime(client);
        }
    }

    fn describe(&self) -> String {
        // Never the URL: it carries credentials. The pool is identified by the
        // database it serves, which is what an operator needs to tell two pools
        // apart.
        self.db_url
            .rsplit('/')
            .next()
            .and_then(|tail| tail.split('?').next())
            .filter(|name| !name.is_empty())
            .map_or_else(|| "postgres".to_string(), ToString::to_string)
    }

    pub(crate) fn stats(&self) -> PostgresPoolStats {
        let guard = self.state.lock();
        PostgresPoolStats {
            max_size: self.max_size,
            in_use: guard.leased,
            idle: guard.idle.len(),
            waiters: guard.waiters,
            acquisitions: self.metrics.acquisitions.load(Ordering::Relaxed),
            saturated_acquisitions: self.metrics.saturated_acquisitions.load(Ordering::Relaxed),
            total_wait: Duration::from_nanos(self.metrics.wait_nanos.load(Ordering::Relaxed)),
            connects: self.metrics.connects.load(Ordering::Relaxed),
            discards: self.metrics.discards.load(Ordering::Relaxed),
        }
    }

    /// Check out a connection whose session context has been reset to
    /// `context`, reconnecting once when a recycled connection turns out to
    /// have been closed by the server (restart, failover, idle reaper).
    fn acquire_reset(&self, context: PostgresSessionContext) -> Result<PooledConnection<'_>> {
        // Bounded so a pathological churn cycle cannot spin forever. Every
        // iteration either returns or permanently removes one connection from
        // the pool, and an exhausted pool opens a fresh connection, so the loop
        // makes progress and only ever reaches the bound under active churn.
        let attempts = self.max_size.saturating_add(2);
        let mut last_error = None;
        for _ in 0..attempts {
            let mut checkout = self.acquire()?;
            let recycled = checkout.recycled;
            // Pooled clients are reused across requests, while PostgreSQL RLS
            // settings are session-scoped. Always start a checkout in the
            // context its caller asked for; owner-scoped operations override
            // this on the same checkout before issuing their query. Because the
            // reset runs unconditionally, a connection returned by a restricted
            // caller is restored to `System` the moment a trusted caller checks
            // it out, and vice versa: neither context can leak into the other.
            // It is also why a connection is never shared concurrently.
            let reset = checkout.client_mut().and_then(|client| {
                client
                    .execute(context.reset_statement(), &[])
                    .context("failed to reset PostgreSQL session context")
            });
            match reset {
                Ok(_) => return Ok(checkout),
                Err(error) => {
                    checkout.discard();
                    // A recycled connection may have been closed by the server
                    // (restart, failover, idle reaper) since its last use, so
                    // drop it and try the next one. A *freshly opened*
                    // connection failing the reset is a real fault instead, and
                    // is reported as-is.
                    if !recycled {
                        return Err(error);
                    }
                    tracing::debug!(%error, "discarding stale pooled PostgreSQL connection");
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("failed to obtain a usable PostgreSQL connection")))
    }

    pub(crate) fn with_client<T>(&self, f: impl FnOnce(&mut Client) -> Result<T>) -> Result<T> {
        self.with_client_in(PostgresSessionContext::System, f)
    }

    /// [`Self::with_client`] for callers that must run under a context other
    /// than [`PostgresSessionContext::System`].
    pub(crate) fn with_client_in<T>(
        &self,
        context: PostgresSessionContext,
        f: impl FnOnce(&mut Client) -> Result<T>,
    ) -> Result<T> {
        let mut checkout = self.checkout_in(context)?;
        f(checkout.client_mut()?)
    }

    /// Check out a connection for callers that cannot express their work as a
    /// single closure (long method bodies with early returns).
    ///
    /// The returned guard hands the connection back on drop exactly like
    /// [`Self::with_client_in`] does, so neither path can leak a slot.
    pub(crate) fn checkout_in(&self, context: PostgresSessionContext) -> Result<PooledConnection<'_>> {
        self.acquire_reset(context)
    }
}

impl Drop for PostgresConnectionPool {
    fn drop(&mut self) {
        let idle = {
            let mut guard = self.state.lock();
            guard.closed = true;
            std::mem::take(&mut guard.idle)
        };
        // Wake anyone still queued so they observe the closed pool.
        self.available.notify_all();
        if idle.is_empty() {
            return;
        }

        let handle = std::thread::Builder::new()
            .name("postgres-memory-drop".to_string())
            .spawn(move || drop(idle));

        if let Ok(handle) = handle {
            let _ = handle.join();
        }
    }
}

/// A connection checked out of [`PostgresConnectionPool`].
///
/// Returning it to the pool happens in `Drop`, so a caller that fails or
/// unwinds mid-query can never leak a slot.
impl crate::runtime::registry::PoolStatsSource for PostgresConnectionPool {
    fn pool_kind(&self) -> &'static str {
        "postgres"
    }

    fn pool_name(&self) -> String {
        self.describe()
    }

    fn pool_metrics(&self) -> serde_json::Value {
        let stats = self.stats();
        serde_json::json!({
            "max_size": stats.max_size,
            "in_use": stats.in_use,
            "idle": stats.idle,
            "waiters": stats.waiters,
            "acquisitions": stats.acquisitions,
            "saturated_acquisitions": stats.saturated_acquisitions,
            "total_wait_ms": stats.total_wait.as_millis(),
            "connects": stats.connects,
            "discards": stats.discards,
        })
    }
}

pub(crate) struct PooledConnection<'pool> {
    pool: &'pool PostgresConnectionPool,
    client: Option<Client>,
    /// `true` when the connection came from the idle list and therefore may
    /// have been closed by the server since its last use.
    recycled: bool,
}

impl PooledConnection<'_> {
    pub(crate) fn client_mut(&mut self) -> Result<&mut Client> {
        self.client
            .as_mut()
            .context("pooled PostgreSQL connection is unavailable")
    }

    /// Tear this connection down instead of recycling it.
    fn discard(&mut self) {
        if let Some(client) = self.client.take() {
            self.pool.metrics.discards.fetch_add(1, Ordering::Relaxed);
            drop_client_off_runtime(client);
        }
    }
}

impl Drop for PooledConnection<'_> {
    fn drop(&mut self) {
        self.pool.release_slot(self.client.take());
    }
}

/// Tear a client down on a dedicated OS thread.
///
/// `postgres::Client` owns an internal Tokio runtime, and dropping a runtime
/// from within a runtime context panics. Checkouts run on `spawn_blocking`
/// workers that still carry that context, so connections are always dropped off
/// the runtime rather than inline.
fn drop_client_off_runtime(client: Client) {
    if let Err(error) = std::thread::Builder::new()
        .name("postgres-memory-drop".to_string())
        .spawn(move || drop(client))
    {
        tracing::warn!(%error, "failed to spawn PostgreSQL connection teardown thread");
    }
}

impl PostgresMemory {
    async fn list_conversation_turns_ordered(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
        recorded_order: bool,
    ) -> Result<Vec<ConversationTurn>> {
        let client = self.client.clone();
        let turns_table = self.qualified_conversation_turns_table();
        let events_table = self.qualified_message_events_table.clone();
        let ordering = if recorded_order {
            format!(
                "timestamp::timestamptz DESC, COALESCE((SELECT event.id FROM {events_table} event WHERE event.event_id = {turns_table}.message_event_id), id) DESC, id DESC"
            )
        } else {
            "id DESC".to_owned()
        };
        let owner_id = principal
            .owner_id
            .as_deref()
            .filter(|owner| !owner.trim().is_empty())
            .map(str::to_string);
        let workspace_id = principal.workspace_id.trim().to_string();
        let session_key = session_key.to_string();
        #[allow(clippy::cast_possible_wrap)]
        let limit = limit.clamp(1, 500) as i64;
        #[allow(clippy::cast_possible_wrap)]
        let offset = offset.min(100_000) as i64;
        let system_allowed = Self::is_system_principal(principal);
        let owner_isolated = principal.require_owner_match;
        // D4 read-merge: the explicit `session_key` arg is the canonical key
        // (bound at `$1`); the principal may carry a distinct legacy key bound
        // at trailing placeholders starting at `$6`.
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
            crate::memory::session_predicate::session_key_match_fragment(PG_DIALECT, &session_indices);

        tokio::task::spawn_blocking(move || -> Result<Vec<ConversationTurn>> {
            let stmt = format!(
                "SELECT id, session_key, role, content, timestamp, message_id
                 FROM {turns_table}
                 WHERE {session_fragment}
                   AND (
                       $4
                       OR owner_id = $5
                       OR owner_id IS NULL
                       OR owner_id = 'legacy:' || session_key
                   )
                 ORDER BY {ordering}
                 LIMIT $2 OFFSET $3",
                session_fragment = session_fragment.sql,
            );
            let rows = client.with_client(|client| {
                let mut tx = client.transaction()?;
                Self::apply_rls_context_raw(
                    &mut tx,
                    system_allowed,
                    Some(&workspace_id),
                    owner_id.as_deref(),
                    owner_isolated,
                )?;
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> =
                    vec![&session_key, &limit, &offset, &system_allowed, &owner_id];
                for key in &legacy_session_keys {
                    bind.push(key);
                }
                let rows = tx.query(&stmt, &bind)?;
                tx.commit()?;
                Ok(rows)
            })?;
            let mut turns: Vec<ConversationTurn> = rows
                .iter()
                .map(|row| ConversationTurn {
                    id: row.get(0),
                    session_key: row.get(1),
                    role: row.get(2),
                    content: row.get(3),
                    timestamp: row.get(4),
                    message_id: row.get(5),
                })
                .collect();
            turns.reverse();
            Ok(turns)
        })
        .await?
    }

    /// Read-only proof that the A04 vector isolation migration and policies are
    /// active on the configured PostgreSQL deployment. The returned reference
    /// contains only migration/policy hashes, never credentials or row data.
    pub fn verify_vector_isolation(
        db_url: &str,
        schema: &str,
        table: &str,
        connect_timeout_secs: Option<u64>,
    ) -> Result<String> {
        let db_url = db_url.to_string();
        let schema = schema.to_string();
        let table = table.to_string();
        std::thread::Builder::new()
            .name("postgres-vector-evidence".to_string())
            .spawn(move || Self::verify_vector_isolation_blocking(&db_url, &schema, &table, connect_timeout_secs))
            .context("failed to spawn PostgreSQL evidence verifier thread")?
            .join()
            .map_err(|_| anyhow::anyhow!("PostgreSQL evidence verifier thread panicked"))?
    }

    fn verify_vector_isolation_blocking(
        db_url: &str,
        schema: &str,
        table: &str,
        connect_timeout_secs: Option<u64>,
    ) -> Result<String> {
        validate_identifier(schema, "schema")?;
        validate_identifier(table, "table")?;
        let chunks = related_table_name(table, "_document_chunks")?;
        let cache = related_table_name(table, "_embedding_cache")?;
        let mut pg_config: postgres::Config = db_url.parse().context("invalid PostgreSQL connection URL")?;
        // A stalled handshake is a network fault, not a long-running task, so it
        // stays bounded even when the operator configures nothing. See
        // `POSTGRES_DEFAULT_CONNECT_TIMEOUT_SECS`.
        pg_config.connect_timeout(Duration::from_secs(
            connect_timeout_secs
                .unwrap_or(POSTGRES_DEFAULT_CONNECT_TIMEOUT_SECS)
                .clamp(1, POSTGRES_CONNECT_TIMEOUT_CAP_SECS),
        ));
        let mut client = pg_config
            .connect(NoTls)
            .context("PostgreSQL isolation evidence connection failed")?;
        let migration_table = format!("{}.memory_schema_migrations", quote_identifier(schema));
        // 21 installed the owner-isolation policies, 23 closed their shared
        // read arm to owner-isolated callers, 24 added the public read grant:
        // the evidence names all three.
        let mut migration_proof = String::from("postgres");
        for wanted in VERIFIED_ISOLATION_MIGRATIONS {
            let (version, _, descriptor) = Self::memory_schema_migration_registry()
                .iter()
                .find(|(version, _, _)| *version == wanted)
                .with_context(|| format!("A04 schema migration {wanted} is not registered"))?;
            let expected_checksum = Self::schema_migration_checksum(descriptor);
            let row = client
                .query_opt(
                    &format!("SELECT checksum FROM {migration_table} WHERE version = $1"),
                    &[version],
                )?
                .with_context(|| format!("A04 schema migration {wanted} is not recorded"))?;
            let recorded_checksum: String = row.get(0);
            anyhow::ensure!(
                recorded_checksum == expected_checksum,
                "A04 schema migration {wanted} checksum does not match the runtime evaluator"
            );
            migration_proof = format!("{migration_proof}:migration:{version}:sha256:{expected_checksum}");
        }

        let targets = [
            (table, "memories_owner_isolation"),
            (chunks.as_str(), "document_chunks_owner_isolation"),
            (cache.as_str(), "embedding_cache_owner_isolation"),
        ];
        let mut policy_proof = String::new();
        for (target, policy) in targets {
            let qualified = format!("{}.{}", quote_identifier(schema), quote_identifier(target));
            let flags = client
                .query_opt(
                    "SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE oid = to_regclass($1::text)",
                    &[&qualified],
                )?
                .with_context(|| format!("A04 vector table {target} is missing"))?;
            anyhow::ensure!(
                flags.get::<_, bool>(0) && flags.get::<_, bool>(1),
                "A04 RLS is not enabled and forced for {target}"
            );
            // Enumerate, not just look up: a stray permissive policy — added by
            // hand, or left behind by another build — ORs its own access in
            // while every named policy still checks out (R-B2 F-5).
            let inventory = client
                .query(
                    "SELECT policyname::text, permissive, roles::text[] \
                     FROM pg_policies WHERE schemaname = $1 AND tablename = $2",
                    &[&schema, &target],
                )?
                .iter()
                .map(|row| PolicyRow {
                    name: row.get(0),
                    permissive: row.get(1),
                    roles: row.get(2),
                })
                .collect::<Vec<_>>();
            let expected: &[&str] = if target == table {
                &[policy, MEMORIES_PUBLIC_READ_POLICY]
            } else {
                &[policy]
            };
            check_policy_inventory(target, &inventory, expected)?;
            let policy_row = client
                .query_opt(
                    "SELECT COALESCE(qual, ''), COALESCE(with_check, '') \
                     FROM pg_policies \
                     WHERE schemaname = $1 AND tablename = $2 AND policyname = $3",
                    &[&schema, &target, &policy],
                )?
                .with_context(|| format!("A04 policy {policy} is missing"))?;
            let using_expr: String = policy_row.get(0);
            let check_expr: String = policy_row.get(1);
            for required in ["prx.current_workspace", "prx.current_owner"] {
                anyhow::ensure!(
                    using_expr.contains(required) && check_expr.contains(required),
                    "A04 policy {policy} lacks required transaction context"
                );
            }
            // OQ-8: a shared read arm must be closed to owner-isolated callers.
            anyhow::ensure!(
                !using_expr.contains("'workspace'") || using_expr.contains("prx.owner_isolated"),
                "A04 policy {policy} shares workspace rows with owner-isolated callers"
            );
            policy_proof.push_str(policy);
            policy_proof.push('\0');
            policy_proof.push_str(&using_expr);
            policy_proof.push('\0');
            policy_proof.push_str(&check_expr);
            policy_proof.push('\0');
            // D-028: the `public` tier is granted by its own read-only policy,
            // never by the owner-isolation policy that also governs writes.
            anyhow::ensure!(
                !using_expr.contains("'public'") && !check_expr.contains("'public'"),
                "A04 policy {policy} admits public rows outside the read-only public policy"
            );
        }
        let public_row = client
            .query_opt(
                "SELECT cmd, COALESCE(qual, ''), COALESCE(with_check, '') \
                 FROM pg_policies \
                 WHERE schemaname = $1 AND tablename = $2 AND policyname = $3",
                &[&schema, &table, &MEMORIES_PUBLIC_READ_POLICY],
            )?
            .with_context(|| format!("A04 policy {MEMORIES_PUBLIC_READ_POLICY} is missing"))?;
        let public_cmd: String = public_row.get(0);
        let public_using: String = public_row.get(1);
        let public_check: String = public_row.get(2);
        anyhow::ensure!(
            public_cmd == "SELECT"
                && public_check.is_empty()
                && public_using.contains("'public'")
                && public_using.contains("prx.current_workspace")
                && public_using.contains("prx.current_owner"),
            "A04 policy {MEMORIES_PUBLIC_READ_POLICY} is not a read-only, workspace-scoped public grant"
        );
        policy_proof.push_str(MEMORIES_PUBLIC_READ_POLICY);
        policy_proof.push('\0');
        policy_proof.push_str(&public_cmd);
        policy_proof.push('\0');
        policy_proof.push_str(&public_using);
        policy_proof.push('\0');
        let policy_hash = Self::schema_migration_checksum(&policy_proof);
        Ok(format!("{migration_proof}:policy-sha256:{policy_hash}"))
    }

    /// Inspect a key namespace without initializing or mutating the configured
    /// PostgreSQL schema. Identifiers are validated before interpolation and
    /// the prefix remains a bound query parameter.
    pub fn read_key_prefix_read_only(
        db_url: &str,
        schema: &str,
        table: &str,
        connect_timeout_secs: Option<u64>,
        prefix: &str,
    ) -> Result<Vec<(String, String)>> {
        validate_identifier(schema, "storage schema")?;
        validate_identifier(table, "storage table")?;
        let qualified_table = format!("{}.{}", quote_identifier(schema), quote_identifier(table));
        let mut config = db_url
            .parse::<postgres::Config>()
            .context("invalid postgres connection URL")?;
        config.connect_timeout(Duration::from_secs(
            connect_timeout_secs
                .unwrap_or(POSTGRES_DEFAULT_CONNECT_TIMEOUT_SECS)
                .min(POSTGRES_CONNECT_TIMEOUT_CAP_SECS),
        ));
        let mut client = config
            .connect(NoTls)
            .context("connect to postgres memory for fitness inspection")?;
        let mut transaction = client
            .build_transaction()
            .read_only(true)
            .start()
            .context("start read-only fitness inspection")?;
        transaction.execute("SELECT set_config('prx.rls_bypass', 'on', true)", &[])?;
        let escaped = prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("{escaped}%");
        let statement =
            format!("SELECT key, content FROM {qualified_table} WHERE key LIKE $1 ESCAPE '\\' ORDER BY key");
        let rows = transaction.query(&statement, &[&pattern])?;
        transaction.rollback()?;
        Ok(rows.into_iter().map(|row| (row.get(0), row.get(1))).collect())
    }

    /// Delete an exact key namespace in one PostgreSQL transaction.
    pub fn delete_key_prefix_transactional(
        db_url: &str,
        schema: &str,
        table: &str,
        connect_timeout_secs: Option<u64>,
        prefix: &str,
    ) -> Result<usize> {
        validate_identifier(schema, "storage schema")?;
        validate_identifier(table, "storage table")?;
        let qualified_table = format!("{}.{}", quote_identifier(schema), quote_identifier(table));
        let mut config = db_url
            .parse::<postgres::Config>()
            .context("invalid postgres connection URL")?;
        config.connect_timeout(Duration::from_secs(
            connect_timeout_secs
                .unwrap_or(POSTGRES_DEFAULT_CONNECT_TIMEOUT_SECS)
                .min(POSTGRES_CONNECT_TIMEOUT_CAP_SECS),
        ));
        let mut client = config
            .connect(NoTls)
            .context("connect to postgres memory for fitness migration")?;
        let mut transaction = client.transaction()?;
        transaction.execute("SELECT set_config('prx.rls_bypass', 'on', true)", &[])?;
        let escaped = prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("{escaped}%");
        let statement = format!("DELETE FROM {qualified_table} WHERE key LIKE $1 ESCAPE '\\'");
        let affected = transaction.execute(&statement, &[&pattern])?;
        transaction.commit()?;
        usize::try_from(affected).context("PostgreSQL returned an invalid deleted fitness row count")
    }

    pub fn new(db_url: &str, schema: &str, table: &str, connect_timeout_secs: Option<u64>) -> Result<Self> {
        Self::with_embedder(
            db_url,
            schema,
            table,
            connect_timeout_secs,
            Arc::new(embeddings::NoopEmbedding),
            0.35,
            0.65,
        )
    }

    pub fn with_embedder(
        db_url: &str,
        schema: &str,
        table: &str,
        connect_timeout_secs: Option<u64>,
        embedder: Arc<dyn embeddings::EmbeddingProvider>,
        vector_weight: f32,
        keyword_weight: f32,
    ) -> Result<Self> {
        Self::with_embedder_and_cache(
            db_url,
            schema,
            table,
            connect_timeout_secs,
            embedder,
            vector_weight,
            keyword_weight,
            usize::try_from(POSTGRES_EMBEDDING_CACHE_MAX_ROWS).unwrap_or(10_000),
            POSTGRES_DEFAULT_POOL_MAX_SIZE,
        )
    }

    pub fn with_embedder_and_cache(
        db_url: &str,
        schema: &str,
        table: &str,
        connect_timeout_secs: Option<u64>,
        embedder: Arc<dyn embeddings::EmbeddingProvider>,
        vector_weight: f32,
        keyword_weight: f32,
        cache_max: usize,
        pool_max_size: usize,
    ) -> Result<Self> {
        validate_identifier(schema, "storage schema")?;
        validate_identifier(table, "storage table")?;

        let schema_ident = quote_identifier(schema);
        let table_ident = quote_identifier(table);
        let message_events_table = related_table_name(table, "_message_events")?;
        let memory_events_table = related_table_name(table, "_memory_events")?;
        let drafts_table = related_table_name(table, "_drafts")?;
        let documents_table = related_table_name(table, "_documents")?;
        let document_chunks_table = related_table_name(table, "_document_chunks")?;
        let memory_links_table = related_table_name(table, "_memory_links")?;
        let retrieval_traces_table = related_table_name(table, "_retrieval_traces")?;
        let compaction_runs_table = related_table_name(table, "_compaction_runs")?;
        let embedding_cache_table = related_table_name(table, "_embedding_cache")?;
        let message_events_table_ident = quote_identifier(&message_events_table);
        let memory_events_table_ident = quote_identifier(&memory_events_table);
        let drafts_table_ident = quote_identifier(&drafts_table);
        let documents_table_ident = quote_identifier(&documents_table);
        let document_chunks_table_ident = quote_identifier(&document_chunks_table);
        let memory_links_table_ident = quote_identifier(&memory_links_table);
        let retrieval_traces_table_ident = quote_identifier(&retrieval_traces_table);
        let compaction_runs_table_ident = quote_identifier(&compaction_runs_table);
        let embedding_cache_table_ident = quote_identifier(&embedding_cache_table);
        let qualified_table = format!("{schema_ident}.{table_ident}");
        let qualified_message_events_table = format!("{schema_ident}.{message_events_table_ident}");
        let qualified_memory_events_table = format!("{schema_ident}.{memory_events_table_ident}");
        let qualified_drafts_table = format!("{schema_ident}.{drafts_table_ident}");
        let qualified_documents_table = format!("{schema_ident}.{documents_table_ident}");
        let qualified_document_chunks_table = format!("{schema_ident}.{document_chunks_table_ident}");
        let qualified_memory_links_table = format!("{schema_ident}.{memory_links_table_ident}");
        let qualified_retrieval_traces_table = format!("{schema_ident}.{retrieval_traces_table_ident}");
        let qualified_compaction_runs_table = format!("{schema_ident}.{compaction_runs_table_ident}");
        let qualified_embedding_cache_table = format!("{schema_ident}.{embedding_cache_table_ident}");

        let schema_ident_field = schema_ident.clone();
        let (pool, pgvector_available) = Self::initialize_pool(
            db_url.to_string(),
            connect_timeout_secs,
            pool_max_size,
            schema_ident,
            qualified_table.clone(),
            qualified_message_events_table.clone(),
            qualified_memory_events_table.clone(),
            qualified_drafts_table.clone(),
            qualified_documents_table.clone(),
            qualified_document_chunks_table.clone(),
            qualified_memory_links_table.clone(),
            qualified_retrieval_traces_table.clone(),
            qualified_compaction_runs_table.clone(),
            qualified_embedding_cache_table.clone(),
            embedder.dimensions(),
        )?;

        Ok(Self {
            client: pool,
            schema_ident: schema_ident_field,
            qualified_table,
            ingress_owners: Default::default(),
            ingress_owner_id: std::sync::OnceLock::new(),
            qualified_message_events_table,
            qualified_memory_events_table,
            qualified_drafts_table,
            qualified_documents_table,
            qualified_document_chunks_table,
            qualified_memory_links_table,
            qualified_retrieval_traces_table,
            qualified_compaction_runs_table,
            qualified_embedding_cache_table,
            embedding_cache_max_rows: i64::try_from(cache_max.max(1)).unwrap_or(POSTGRES_EMBEDDING_CACHE_MAX_ROWS),
            pgvector_available,
            embedder,
            vector_weight,
            keyword_weight,
        })
    }

    /// Build the connection pool and run schema initialisation through it.
    ///
    /// Initialisation runs on a dedicated OS thread because this constructor is
    /// reachable from an async context, and the blocking `postgres` client
    /// cannot open a connection from inside a Tokio runtime.
    fn initialize_pool(
        db_url: String,
        connect_timeout_secs: Option<u64>,
        pool_max_size: usize,
        schema_ident: String,
        qualified_table: String,
        qualified_message_events_table: String,
        qualified_memory_events_table: String,
        qualified_drafts_table: String,
        qualified_documents_table: String,
        qualified_document_chunks_table: String,
        qualified_memory_links_table: String,
        qualified_retrieval_traces_table: String,
        qualified_compaction_runs_table: String,
        qualified_embedding_cache_table: String,
        embedding_dimensions: usize,
    ) -> Result<(Arc<PostgresConnectionPool>, bool)> {
        let pool = Arc::new(PostgresConnectionPool::new(db_url, connect_timeout_secs, pool_max_size));
        let concrete_pool = Arc::clone(&pool);
        let metrics_source: Arc<dyn crate::runtime::registry::PoolStatsSource> = concrete_pool;
        crate::runtime::registry::register_pool(&metrics_source);
        let init_pool = Arc::clone(&pool);
        let init_handle = std::thread::Builder::new()
            .name("postgres-memory-init".to_string())
            .spawn(move || -> Result<bool> {
                init_pool.with_client(|client| {
                    Self::init_schema(
                        client,
                        &schema_ident,
                        &qualified_table,
                        &qualified_message_events_table,
                        &qualified_memory_events_table,
                        &qualified_drafts_table,
                        &qualified_documents_table,
                        &qualified_document_chunks_table,
                        &qualified_memory_links_table,
                        &qualified_retrieval_traces_table,
                        &qualified_compaction_runs_table,
                        &qualified_embedding_cache_table,
                        embedding_dimensions,
                    )
                })
            })
            .context("failed to spawn PostgreSQL initializer thread")?;

        let pgvector_available = init_handle
            .join()
            .map_err(|_| anyhow::anyhow!("PostgreSQL initializer thread panicked"))??;
        Ok((pool, pgvector_available))
    }

    /// Current connection pool utilisation, for observability surfaces.
    pub fn pool_stats(&self) -> PostgresPoolStats {
        self.client.stats()
    }

    // SAFETY: `schema_ident` and `qualified_table` are validated+quoted at
    // construction time via `validate_identifier()` + `quote_identifier()`,
    // which enforce `^[a-zA-Z_][a-zA-Z0-9_]{0,62}$`. SQL injection is not
    // possible through these interpolated identifiers.
    fn init_schema(
        client: &mut Client,
        schema_ident: &str,
        qualified_table: &str,
        qualified_message_events_table: &str,
        qualified_memory_events_table: &str,
        qualified_drafts_table: &str,
        qualified_documents_table: &str,
        qualified_document_chunks_table: &str,
        qualified_memory_links_table: &str,
        qualified_retrieval_traces_table: &str,
        qualified_compaction_runs_table: &str,
        qualified_embedding_cache_table: &str,
        embedding_dimensions: usize,
    ) -> Result<bool> {
        client.batch_execute(&format!(
            "
            CREATE SCHEMA IF NOT EXISTS {schema_ident};

            CREATE TABLE IF NOT EXISTS {qualified_table} (
                id TEXT PRIMARY KEY,
                key TEXT UNIQUE NOT NULL,
                content TEXT NOT NULL,
                category TEXT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL,
                session_id TEXT,
                useful_count INTEGER NOT NULL DEFAULT 0,
                workspace_id TEXT,
                owner_id TEXT,
                agent_id TEXT,
                persona_id TEXT,
                source_event_id TEXT,
                source TEXT,
                embedding BYTEA,
                embedding_provider TEXT,
                embedding_model TEXT,
                embedding_dimensions BIGINT,
                channel TEXT,
                chat_type TEXT,
                chat_id TEXT,
                sender_id TEXT,
                raw_sender TEXT,
                topic_id TEXT,
                visibility TEXT NOT NULL DEFAULT 'workspace',
                sensitivity TEXT NOT NULL DEFAULT 'normal',
                risk_signals TEXT,
                policy_version BIGINT
            );

            CREATE INDEX IF NOT EXISTS idx_memories_category ON {qualified_table}(category);
            CREATE INDEX IF NOT EXISTS idx_memories_session_id ON {qualified_table}(session_id);
            CREATE INDEX IF NOT EXISTS idx_memories_updated_at ON {qualified_table}(updated_at DESC);
            CREATE INDEX IF NOT EXISTS idx_memories_context_scope
                ON {qualified_table}(channel, chat_id, raw_sender);
            CREATE INDEX IF NOT EXISTS idx_memories_owner_scope
                ON {qualified_table}(owner_id);
            CREATE INDEX IF NOT EXISTS idx_memories_sender_scope
                ON {qualified_table}(sender_id);
            CREATE INDEX IF NOT EXISTS idx_memories_visibility
                ON {qualified_table}(visibility, sensitivity);
            CREATE INDEX IF NOT EXISTS idx_memories_embedding_current
                ON {qualified_table}(embedding_provider, embedding_model, embedding_dimensions, session_id, id)
                WHERE embedding IS NOT NULL;

            CREATE TABLE IF NOT EXISTS {schema_ident}.agent_identity_bindings (
                binding_id TEXT PRIMARY KEY,
                external_subject TEXT NOT NULL,
                external_issuer TEXT NOT NULL,
                auth_method TEXT NOT NULL,
                prx_owner_id TEXT NOT NULL,
                prx_principal_id TEXT NOT NULL,
                capabilities TEXT NOT NULL,
                expires_at TIMESTAMPTZ,
                created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
                last_used_at TIMESTAMPTZ,
                UNIQUE (external_issuer, external_subject, auth_method)
            );
            CREATE INDEX IF NOT EXISTS idx_agent_bindings_lookup
                ON {schema_ident}.agent_identity_bindings(external_issuer, external_subject);
            CREATE INDEX IF NOT EXISTS idx_agent_bindings_owner
                ON {schema_ident}.agent_identity_bindings(prx_owner_id);

            CREATE TABLE IF NOT EXISTS {schema_ident}.approval_grants (
                grant_id TEXT PRIMARY KEY,
                version BIGINT NOT NULL,
                owner_id TEXT NOT NULL,
                principal_id TEXT NOT NULL,
                workspace_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                session_key TEXT,
                issuer_authority TEXT NOT NULL,
                issuer_authority_id TEXT NOT NULL,
                issuer_public_key_id TEXT NOT NULL,
                capability_op_id TEXT NOT NULL,
                capability_op_id_match TEXT NOT NULL,
                capability_risk_level TEXT NOT NULL CHECK (capability_risk_level IN ('low','medium','high','critical')),
                resource_constraints_json JSONB NOT NULL DEFAULT '{{}}'::jsonb,
                grant_json JSONB NOT NULL,
                signature_alg TEXT NOT NULL,
                signed_payload_sha256 TEXT NOT NULL,
                issued_at TIMESTAMPTZ NOT NULL,
                not_before TIMESTAMPTZ NOT NULL,
                expires_at TIMESTAMPTZ NOT NULL,
                max_uses BIGINT NOT NULL,
                uses_consumed BIGINT NOT NULL DEFAULT 0,
                related_task_id TEXT,
                related_message_event_id BIGINT,
                revoked_at TIMESTAMPTZ,
                revocation_reason TEXT,
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_approval_grants_owner
                ON {schema_ident}.approval_grants(workspace_id, owner_id, issued_at DESC);
            CREATE INDEX IF NOT EXISTS idx_approval_grants_principal
                ON {schema_ident}.approval_grants(workspace_id, principal_id, issued_at DESC);
            CREATE INDEX IF NOT EXISTS idx_approval_grants_capability
                ON {schema_ident}.approval_grants(workspace_id, capability_op_id, expires_at);
            CREATE INDEX IF NOT EXISTS idx_approval_grants_active
                ON {schema_ident}.approval_grants(workspace_id, expires_at, revoked_at);

            CREATE TABLE IF NOT EXISTS {schema_ident}.approval_grant_events (
                id BIGSERIAL PRIMARY KEY,
                event_id TEXT UNIQUE NOT NULL,
                grant_id TEXT NOT NULL REFERENCES {schema_ident}.approval_grants(grant_id) ON DELETE CASCADE,
                event_type TEXT NOT NULL CHECK (event_type IN (
                    'grant.issued','grant.verified','grant.consumed','grant.revoked',
                    'grant.rejected','grant.expired'
                )),
                actor TEXT NOT NULL,
                occurred_at TIMESTAMPTZ NOT NULL,
                payload_json JSONB
            );
            CREATE INDEX IF NOT EXISTS idx_approval_grant_events_grant
                ON {schema_ident}.approval_grant_events(grant_id, occurred_at);
            CREATE INDEX IF NOT EXISTS idx_approval_grant_events_type
                ON {schema_ident}.approval_grant_events(event_type, occurred_at);

            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS useful_count INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS workspace_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS owner_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS agent_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS persona_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS source_event_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS source TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS embedding BYTEA;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS embedding_provider TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS embedding_model TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS embedding_dimensions BIGINT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS channel TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS chat_type TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS chat_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS sender_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS raw_sender TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS topic_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS visibility TEXT NOT NULL DEFAULT 'workspace';
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS sensitivity TEXT NOT NULL DEFAULT 'normal';
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS risk_signals TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS policy_version BIGINT;

            CREATE TABLE IF NOT EXISTS {qualified_embedding_cache_table} (
                workspace_id TEXT NOT NULL,
                owner_id TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                embedding BYTEA NOT NULL,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                dimensions BIGINT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL,
                accessed_at TIMESTAMPTZ NOT NULL,
                PRIMARY KEY (workspace_id, owner_id, content_hash, provider, model, dimensions)
            );
            ALTER TABLE {qualified_embedding_cache_table}
                ADD COLUMN IF NOT EXISTS workspace_id TEXT NOT NULL DEFAULT '__prx_system__';
            ALTER TABLE {qualified_embedding_cache_table}
                ADD COLUMN IF NOT EXISTS owner_id TEXT NOT NULL DEFAULT '__prx_system__';
            CREATE INDEX IF NOT EXISTS idx_embedding_cache_accessed
                ON {qualified_embedding_cache_table}(accessed_at);
            CREATE INDEX IF NOT EXISTS idx_embedding_cache_provider_model
                ON {qualified_embedding_cache_table}(provider, model, dimensions, accessed_at);

            CREATE TABLE IF NOT EXISTS {qualified_message_events_table} (
                id BIGSERIAL PRIMARY KEY,
                event_id TEXT UNIQUE NOT NULL,
                idempotency_key TEXT,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                source TEXT NOT NULL,
                channel TEXT,
                session_key TEXT,
                parent_session_key TEXT,
                run_id TEXT,
                parent_run_id TEXT,
                agent_id TEXT,
                persona_id TEXT,
                sender TEXT,
                recipient TEXT,
                role TEXT NOT NULL,
                event_type TEXT NOT NULL,
                source_ref_json TEXT,
                subject_ref_json TEXT,
                goal_id TEXT,
                causation_event_id TEXT,
                correlation_id TEXT,
                attempt_id TEXT,
                lease_epoch BIGINT,
                config_generation_id BIGINT,
                config_source_revision TEXT,
                content TEXT NOT NULL,
                content_hash TEXT,
                raw_payload_json TEXT,
                visibility TEXT NOT NULL DEFAULT 'workspace',
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL
            );
            ALTER TABLE {qualified_message_events_table}
                ADD COLUMN IF NOT EXISTS event_type TEXT;
            ALTER TABLE {qualified_message_events_table}
                ADD COLUMN IF NOT EXISTS source_ref_json TEXT,
                ADD COLUMN IF NOT EXISTS subject_ref_json TEXT,
                ADD COLUMN IF NOT EXISTS goal_id TEXT,
                ADD COLUMN IF NOT EXISTS causation_event_id TEXT,
                ADD COLUMN IF NOT EXISTS correlation_id TEXT,
                ADD COLUMN IF NOT EXISTS attempt_id TEXT,
                ADD COLUMN IF NOT EXISTS lease_epoch BIGINT,
                ADD COLUMN IF NOT EXISTS config_generation_id BIGINT,
                ADD COLUMN IF NOT EXISTS config_source_revision TEXT;

            CREATE INDEX IF NOT EXISTS idx_message_events_workspace_id
                ON {qualified_message_events_table}(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_owner_id
                ON {qualified_message_events_table}(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_session
                ON {qualified_message_events_table}(workspace_id, session_key, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_agent
                ON {qualified_message_events_table}(workspace_id, agent_id, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_channel_sender
                ON {qualified_message_events_table}(workspace_id, channel, sender, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_visibility
                ON {qualified_message_events_table}(workspace_id, visibility, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_event_type
                ON {qualified_message_events_table}(workspace_id, event_type, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_correlation
                ON {qualified_message_events_table}(workspace_id, correlation_id, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_created_at
                ON {qualified_message_events_table}(created_at);
            CREATE INDEX IF NOT EXISTS idx_message_events_config_generation
                ON {qualified_message_events_table}(workspace_id, config_generation_id, id);

            CREATE TABLE IF NOT EXISTS {qualified_memory_events_table} (
                id BIGSERIAL PRIMARY KEY,
                event_id TEXT UNIQUE NOT NULL,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                event_type TEXT NOT NULL,
                subject_table TEXT NOT NULL,
                subject_id TEXT NOT NULL,
                session_key TEXT,
                run_id TEXT,
                parent_run_id TEXT,
                agent_id TEXT,
                persona_id TEXT,
                visibility TEXT NOT NULL DEFAULT 'workspace',
                payload_json TEXT,
                created_at TIMESTAMPTZ NOT NULL
            );
            ALTER TABLE {qualified_memory_events_table} ADD COLUMN IF NOT EXISTS run_id TEXT;
            ALTER TABLE {qualified_memory_events_table} ADD COLUMN IF NOT EXISTS parent_run_id TEXT;
            -- Schema v16 (C03 / V02-R4): owner attribution for memory events.
            -- Existing rows keep a NULL owner (unattributed) and are never
            -- rewritten; an owner-isolated principal does not admit them.
            ALTER TABLE {qualified_memory_events_table} ADD COLUMN IF NOT EXISTS owner_id TEXT;

            CREATE INDEX IF NOT EXISTS idx_memory_events_workspace_id
                ON {qualified_memory_events_table}(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_events_type
                ON {qualified_memory_events_table}(workspace_id, event_type, id);
            CREATE INDEX IF NOT EXISTS idx_memory_events_session
                ON {qualified_memory_events_table}(workspace_id, session_key, id);
            CREATE INDEX IF NOT EXISTS idx_memory_events_run
                ON {qualified_memory_events_table}(workspace_id, run_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_events_parent_run
                ON {qualified_memory_events_table}(workspace_id, parent_run_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_events_owner
                ON {qualified_memory_events_table}(workspace_id, owner_id, id);

            CREATE TABLE IF NOT EXISTS {qualified_drafts_table} (
                id BIGSERIAL PRIMARY KEY,
                draft_id TEXT UNIQUE NOT NULL,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                worker_run_id TEXT NOT NULL,
                parent_run_id TEXT,
                session_key TEXT,
                agent_id TEXT,
                persona_id TEXT,
                key TEXT NOT NULL,
                content TEXT NOT NULL,
                category TEXT NOT NULL,
                source_event_id TEXT,
                visibility TEXT NOT NULL DEFAULT 'workspace',
                status TEXT NOT NULL DEFAULT 'pending',
                payload_json TEXT,
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL
            );

            -- Backfill legacy columns BEFORE creating indexes that reference them:
            -- a legacy drafts table predating `source_event_id` would make the
            -- `idx_memory_drafts_source_event` index below fail (`column ...
            -- does not exist`) and abort this whole batch.
            ALTER TABLE {qualified_drafts_table}
                ADD COLUMN IF NOT EXISTS owner_id TEXT;
            ALTER TABLE {qualified_drafts_table}
                ADD COLUMN IF NOT EXISTS parent_run_id TEXT;
            ALTER TABLE {qualified_drafts_table}
                ADD COLUMN IF NOT EXISTS agent_id TEXT;
            ALTER TABLE {qualified_drafts_table}
                ADD COLUMN IF NOT EXISTS persona_id TEXT;
            ALTER TABLE {qualified_drafts_table}
                ADD COLUMN IF NOT EXISTS source_event_id TEXT;
            ALTER TABLE {qualified_drafts_table}
                ADD COLUMN IF NOT EXISTS visibility TEXT NOT NULL DEFAULT 'workspace';
            ALTER TABLE {qualified_drafts_table}
                ADD COLUMN IF NOT EXISTS payload_json TEXT;

            CREATE INDEX IF NOT EXISTS idx_memory_drafts_worker_run
                ON {qualified_drafts_table}(worker_run_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_drafts_owner
                ON {qualified_drafts_table}(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_drafts_status
                ON {qualified_drafts_table}(status, id);
            CREATE INDEX IF NOT EXISTS idx_memory_drafts_source_event
                ON {qualified_drafts_table}(source_event_id);

            CREATE TABLE IF NOT EXISTS {qualified_documents_table} (
                id BIGSERIAL PRIMARY KEY,
                document_id TEXT UNIQUE NOT NULL,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                topic_id TEXT,
                task_id TEXT,
                source_message_event_id TEXT,
                source_kind TEXT NOT NULL,
                source_uri TEXT,
                title TEXT,
                content_sha256 TEXT NOT NULL,
                mime_type TEXT,
                visibility TEXT NOT NULL DEFAULT 'workspace',
                metadata_json TEXT,
                chunk_count BIGINT NOT NULL DEFAULT 0,
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL
            );

            CREATE TABLE IF NOT EXISTS {qualified_document_chunks_table} (
                id BIGSERIAL PRIMARY KEY,
                chunk_id TEXT UNIQUE NOT NULL,
                document_id TEXT NOT NULL,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                topic_id TEXT,
                task_id TEXT,
                chunk_index BIGINT NOT NULL,
                heading TEXT,
                content TEXT NOT NULL,
                content_sha256 TEXT NOT NULL,
                embedding BYTEA,
                embedding_provider TEXT,
                embedding_model TEXT,
                embedding_dimensions BIGINT,
                visibility TEXT NOT NULL DEFAULT 'workspace',
                source_anchor TEXT NOT NULL,
                token_estimate BIGINT NOT NULL DEFAULT 0,
                created_at TIMESTAMPTZ NOT NULL
            );

            ALTER TABLE {qualified_document_chunks_table}
                ADD COLUMN IF NOT EXISTS embedding BYTEA;
            ALTER TABLE {qualified_document_chunks_table}
                ADD COLUMN IF NOT EXISTS embedding_provider TEXT;
            ALTER TABLE {qualified_document_chunks_table}
                ADD COLUMN IF NOT EXISTS embedding_model TEXT;
            ALTER TABLE {qualified_document_chunks_table}
                ADD COLUMN IF NOT EXISTS embedding_dimensions BIGINT;
            ALTER TABLE {qualified_document_chunks_table}
                ADD COLUMN IF NOT EXISTS visibility TEXT NOT NULL DEFAULT 'workspace';

            CREATE TABLE IF NOT EXISTS {qualified_memory_links_table} (
                id BIGSERIAL PRIMARY KEY,
                link_id TEXT UNIQUE NOT NULL,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                memory_key TEXT,
                memory_event_id TEXT,
                message_event_id TEXT,
                document_id TEXT NOT NULL,
                chunk_id TEXT,
                link_type TEXT NOT NULL,
                payload_json TEXT,
                created_at TIMESTAMPTZ NOT NULL
            );

            CREATE TABLE IF NOT EXISTS {qualified_retrieval_traces_table} (
                id BIGSERIAL PRIMARY KEY,
                trace_id TEXT UNIQUE NOT NULL,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                session_key TEXT,
                agent_id TEXT,
                persona_id TEXT,
                source TEXT NOT NULL,
                query TEXT NOT NULL,
                candidate_count BIGINT NOT NULL,
                selected_count BIGINT NOT NULL,
                dropped_count BIGINT NOT NULL,
                budget_tokens BIGINT,
                selected_json TEXT,
                dropped_json TEXT,
                payload_json TEXT,
                created_at TIMESTAMPTZ NOT NULL
            );

            CREATE TABLE IF NOT EXISTS {qualified_compaction_runs_table} (
                id BIGSERIAL PRIMARY KEY,
                run_id TEXT UNIQUE NOT NULL,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                session_key TEXT,
                agent_id TEXT,
                persona_id TEXT,
                trigger TEXT NOT NULL,
                mode TEXT NOT NULL,
                source_message_count BIGINT NOT NULL,
                source_token_estimate BIGINT NOT NULL,
                summary TEXT NOT NULL,
                summary_memory_key TEXT,
                source_event_ids_json TEXT,
                source_event_range_json TEXT,
                source_document_refs_json TEXT,
                fidelity_status TEXT NOT NULL,
                payload_json TEXT,
                created_at TIMESTAMPTZ NOT NULL
            );
            ALTER TABLE {qualified_compaction_runs_table}
                ADD COLUMN IF NOT EXISTS source_event_range_json TEXT;

            CREATE INDEX IF NOT EXISTS idx_documents_workspace
                ON {qualified_documents_table}(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_documents_owner
                ON {qualified_documents_table}(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_documents_topic
                ON {qualified_documents_table}(workspace_id, topic_id, id);
            CREATE INDEX IF NOT EXISTS idx_documents_task
                ON {qualified_documents_table}(workspace_id, task_id, id);
            CREATE INDEX IF NOT EXISTS idx_documents_hash
                ON {qualified_documents_table}(workspace_id, content_sha256);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_document
                ON {qualified_document_chunks_table}(document_id, chunk_index);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_workspace
                ON {qualified_document_chunks_table}(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_owner
                ON {qualified_document_chunks_table}(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_topic
                ON {qualified_document_chunks_table}(workspace_id, topic_id, id);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_content_tsv
                ON {qualified_document_chunks_table}
                USING GIN (to_tsvector('simple', content));
            CREATE INDEX IF NOT EXISTS idx_document_chunks_embedding_current
                ON {qualified_document_chunks_table}(embedding_provider, embedding_model, embedding_dimensions, workspace_id, owner_id, id)
                WHERE embedding IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_memory_links_workspace
                ON {qualified_memory_links_table}(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_links_document
                ON {qualified_memory_links_table}(document_id, chunk_id);
            CREATE INDEX IF NOT EXISTS idx_memory_links_memory_key
                ON {qualified_memory_links_table}(workspace_id, memory_key);
            CREATE INDEX IF NOT EXISTS idx_retrieval_traces_workspace
                ON {qualified_retrieval_traces_table}(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_retrieval_traces_owner
                ON {qualified_retrieval_traces_table}(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_retrieval_traces_session
                ON {qualified_retrieval_traces_table}(workspace_id, session_key, id);
            CREATE INDEX IF NOT EXISTS idx_retrieval_traces_source
                ON {qualified_retrieval_traces_table}(workspace_id, source, id);
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_workspace
                ON {qualified_compaction_runs_table}(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_owner
                ON {qualified_compaction_runs_table}(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_session
                ON {qualified_compaction_runs_table}(workspace_id, session_key, id);
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_trigger
                ON {qualified_compaction_runs_table}(workspace_id, trigger, id);

            -- FIX-P0-21: principal resolution tables (parity with SQLite).
            CREATE TABLE IF NOT EXISTS {schema_ident}.identity_bindings (
                id              TEXT PRIMARY KEY,
                user_id         TEXT NOT NULL,
                channel         TEXT NOT NULL,
                channel_account TEXT NOT NULL,
                display_name    TEXT,
                bound_at        TEXT NOT NULL,
                bound_by        TEXT NOT NULL,
                UNIQUE(channel, channel_account)
            );
            CREATE INDEX IF NOT EXISTS idx_ib_user
                ON {schema_ident}.identity_bindings(user_id);
            CREATE INDEX IF NOT EXISTS idx_ib_channel_account
                ON {schema_ident}.identity_bindings(channel, channel_account);

            CREATE TABLE IF NOT EXISTS {schema_ident}.chat_profiles (
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
            CREATE INDEX IF NOT EXISTS idx_chat_profiles_lookup
                ON {schema_ident}.chat_profiles(channel, chat_id);

            CREATE TABLE IF NOT EXISTS {schema_ident}.user_policies (
                user_id             TEXT PRIMARY KEY,
                role                TEXT NOT NULL DEFAULT 'guest',
                projects            TEXT NOT NULL DEFAULT '[]',
                visibility_ceiling  TEXT NOT NULL DEFAULT 'private',
                blocked_patterns    TEXT NOT NULL DEFAULT '[]',
                policy_version      BIGINT NOT NULL DEFAULT 1,
                updated_at          TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS {schema_ident}.access_audit_log (
                id          TEXT PRIMARY KEY,
                timestamp   TEXT NOT NULL,
                requester   TEXT NOT NULL,
                action      TEXT NOT NULL,
                query       TEXT,
                memory_id   TEXT,
                policy_rule TEXT,
                result      TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_audit_time
                ON {schema_ident}.access_audit_log(timestamp DESC);
            CREATE INDEX IF NOT EXISTS idx_audit_requester
                ON {schema_ident}.access_audit_log(requester);

            -- FIX-P0-19/20: conversation sessions + turns with owner ACL (parity
            -- with SQLite). owner_id is nullable in Phase 1/2; legacy rows carry
            -- `legacy:<session_key>` and are gated by the legacy visibility flag.
            CREATE TABLE IF NOT EXISTS {schema_ident}.sessions (
                session_key          TEXT PRIMARY KEY,
                channel              TEXT NOT NULL,
                sender               TEXT NOT NULL,
                owner_id             TEXT,
                created_at           TEXT NOT NULL,
                updated_at           TEXT NOT NULL,
                message_count        BIGINT NOT NULL DEFAULT 0,
                last_message_preview TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_updated_at
                ON {schema_ident}.sessions(updated_at DESC);
            CREATE INDEX IF NOT EXISTS idx_sessions_channel_updated_at
                ON {schema_ident}.sessions(channel, updated_at DESC);
            CREATE INDEX IF NOT EXISTS idx_sessions_owner
                ON {schema_ident}.sessions(owner_id);

            CREATE TABLE IF NOT EXISTS {schema_ident}.conversation_turns (
                id               BIGSERIAL PRIMARY KEY,
                session_key      TEXT NOT NULL,
                owner_id         TEXT,
                role             TEXT NOT NULL,
                content          TEXT NOT NULL,
                timestamp        TEXT NOT NULL,
                message_id       TEXT,
                message_event_id TEXT,
                agent_id         TEXT,
                persona_id       TEXT,
                visibility       TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_conversation_turns_session_key
                ON {schema_ident}.conversation_turns(session_key);
            CREATE INDEX IF NOT EXISTS idx_conversation_turns_timestamp
                ON {schema_ident}.conversation_turns(timestamp DESC);
            CREATE INDEX IF NOT EXISTS idx_conversation_turns_owner_session
                ON {schema_ident}.conversation_turns(owner_id, session_key);

            -- FIX-P1-18: additional hot-path indexes for owner/topic scoped reads.
            CREATE INDEX IF NOT EXISTS idx_document_chunks_task
                ON {qualified_document_chunks_table}(workspace_id, task_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_links_owner
                ON {qualified_memory_links_table}(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_links_memory_event
                ON {qualified_memory_links_table}(memory_event_id);
            CREATE INDEX IF NOT EXISTS idx_memory_links_message_event
                ON {qualified_memory_links_table}(message_event_id);
            CREATE INDEX IF NOT EXISTS idx_documents_source_event
                ON {qualified_documents_table}(source_message_event_id);
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_summary_key
                ON {qualified_compaction_runs_table}(workspace_id, summary_memory_key, id);
            "
        ))?;

        Self::ensure_message_event_idempotency_scope(client, qualified_message_events_table)?;

        // FIX-P1-19: document_chunks → documents FK with ON DELETE CASCADE.
        // Added as a guarded step (not in the idempotent batch) because adding a
        // constraint that already exists raises an error rather than being a
        // no-op; we therefore detect-then-add and tolerate the duplicate.
        Self::ensure_document_chunks_fk(client, qualified_documents_table, qualified_document_chunks_table)?;
        Self::ensure_embedding_cache_scope_key(client, qualified_embedding_cache_table)?;

        // A04: vector-bearing memory and document rows are protected by an
        // authoritative, fail-closed RLS migration. Startup must fail if the
        // configured role cannot install or verify these policies; silently
        // running without isolation would make the attestation untruthful.
        Self::init_vector_rls(
            client,
            qualified_table,
            qualified_document_chunks_table,
            qualified_embedding_cache_table,
        )?;

        // FIX-P0-25: record-and-verify versioned schema migrations. The DDL was
        // already executed by the idempotent batch above; this ledger records a
        // stable (version, name, checksum) per logical step so drift is detected
        // (checksum mismatch → bail) at startup.
        Self::run_memory_schema_migrations(client, schema_ident)?;

        Ok(Self::try_init_pgvector(
            client,
            qualified_table,
            qualified_document_chunks_table,
            embedding_dimensions,
        ))
    }

    /// SHA-256 (hex) of a migration's canonical descriptor text.
    pub(crate) fn schema_migration_checksum(text: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Versioned registry of the canonical schema steps created by `init_schema`,
    /// retro-fitted in execution order. Each text is a stable canonical descriptor
    /// used only as a checksum anchor (version unchanged ⇒ text unchanged). NEVER
    /// mutate an existing entry's text; append a new version for new schema changes.
    pub(crate) const fn memory_schema_migration_registry() -> &'static [(i64, &'static str, &'static str)] {
        &[
            (
                1,
                "memories_core",
                "memories(id,key,content,category,created_at,updated_at,session_id,useful_count,workspace_id,owner_id,agent_id,persona_id,source_event_id,source,embedding,embedding_provider,embedding_model,embedding_dimensions,channel,chat_type,chat_id,sender_id,raw_sender,topic_id,visibility,sensitivity,risk_signals,policy_version)",
            ),
            (
                2,
                "agent_identity_bindings",
                "agent_identity_bindings(binding_id,external_subject,external_issuer,auth_method,prx_owner_id,prx_principal_id,capabilities,expires_at,created_at,last_used_at)",
            ),
            (3, "approval_grants", "approval_grants + approval_grant_events"),
            (
                4,
                "embedding_cache",
                "embedding_cache(content_hash,embedding,provider,model,dimensions,created_at,accessed_at)",
            ),
            (
                5,
                "message_events",
                "message_events(id,event_id,idempotency_key,workspace_id,owner_id,source,channel,session_key,parent_session_key,run_id,parent_run_id,agent_id,persona_id,sender,recipient,role,event_type,content,content_hash,raw_payload_json,visibility,created_at,updated_at)",
            ),
            (
                6,
                "memory_events",
                "memory_events(event_id,workspace_id,event_type,subject_table,subject_id,session_key,agent_id,persona_id,visibility,payload_json,created_at)",
            ),
            (
                7,
                "memory_drafts",
                "memory_drafts(id,draft_id,workspace_id,owner_id,worker_run_id,parent_run_id,session_key,agent_id,persona_id,key,content,category,source_event_id,visibility,status,payload_json,created_at,updated_at)",
            ),
            (
                8,
                "documents",
                "documents(id,document_id,workspace_id,owner_id,topic_id,task_id,source_message_event_id,source_kind,source_uri,title,content_sha256,mime_type,visibility,metadata_json,chunk_count,created_at,updated_at)",
            ),
            (
                9,
                "document_chunks",
                "document_chunks(id,document_id,chunk_index,content,content_sha256,embedding,token_count,owner_id,created_at)",
            ),
            (
                10,
                "memory_links_traces_compaction",
                "memory_links + retrieval_traces + compaction_runs",
            ),
            (
                11,
                "evolution_proposals",
                "evolution_proposals(id,draft_id,owner_id,principal_id,workspace_id,topic_id,task_id,source_message_event_ids_json,source_memory_event_ids_json,evidence_hashes_json,target_resource_json,proposed_change_json,risk_level,mode,created_at,created_by_runtime,judge_verdict_json,applied_at,applied_by,rollback_anchor_json) + evolution_proposal_events(id,draft_id,event_type,occurred_at,actor,payload_json)",
            ),
            (
                12,
                "memory_events_run_lineage",
                "memory_events + run_id + parent_run_id + idx_memory_events_run + idx_memory_events_parent_run",
            ),
            (
                13,
                "principal_resolution_tables",
                "identity_bindings(id,user_id,channel,channel_account,display_name,bound_at,bound_by) + user_policies(user_id,role,projects,visibility_ceiling,blocked_patterns,policy_version,updated_at) + access_audit_log(id,timestamp,requester,action,query,memory_id,policy_rule,result)",
            ),
            (
                14,
                "sessions_and_conversation_turns",
                "sessions(session_key,channel,sender,owner_id,created_at,updated_at,message_count,last_message_preview) + conversation_turns(id,session_key,owner_id,role,content,timestamp,message_id,message_event_id,agent_id,persona_id,visibility) + idx_conversation_turns_owner_session",
            ),
            (
                15,
                "owner_topic_indexes_and_chunk_fk",
                "idx_document_chunks_task + idx_memory_links_owner + idx_memory_links_memory_event + idx_memory_links_message_event + idx_documents_source_event + idx_compaction_runs_summary_key + fk_document_chunks_document(ON DELETE CASCADE)",
            ),
            (
                16,
                "memories_row_level_security",
                "memories ENABLE+FORCE ROW LEVEL SECURITY + POLICY memories_owner_isolation(USING/WITH CHECK: current_setting('prx.rls_bypass', true)='on' OR current_setting('prx.current_owner', true) IS NULL OR owner_id IS NOT DISTINCT FROM current_setting('prx.current_owner', true))",
            ),
            (
                17,
                "compaction_source_event_range",
                "compaction_runs.source_event_ids_json contains only real MessageEvent event_id strings + source_event_range_json(first_event_id,last_event_id,first_row_id,last_row_id,source_event_count)",
            ),
            (
                18,
                "message_event_workspace_idempotency",
                "message_events UNIQUE(workspace_id,idempotency_key) replaces global UNIQUE(idempotency_key) + workspace-scoped conflict lookup",
            ),
            (
                19,
                "message_event_config_generation",
                "message_events + config_generation_id + config_source_revision + idx_message_events_config_generation",
            ),
            (
                20,
                "message_event_execution_metadata",
                "message_events + source_ref_json + subject_ref_json + goal_id + causation_event_id + correlation_id + attempt_id + lease_epoch",
            ),
            (
                21,
                "vector_owner_workspace_row_level_security",
                "memories + document_chunks + embedding_cache scoped by workspace_id/owner_id and ENABLE+FORCE ROW LEVEL SECURITY; transaction-local workspace required; private/cache owner exact; shared reads tenant-scoped; writes owner exact; missing context denies",
            ),
            (
                22,
                "memory_event_owner_isolation",
                // C03 / V02-R4, parity with SQLite v16: `memory_events` gains
                // `owner_id` so an externally authenticated (owner-isolated)
                // principal can be refused another owner's `session`/`private`
                // rows. Added by `ADD COLUMN IF NOT EXISTS`; legacy rows keep
                // NULL (unattributed).
                "memory_events + owner_id + idx_memory_events_owner",
            ),
            (
                23,
                "vector_rls_owner_isolated_reads",
                // OQ-8 / D-019: the policy DDL itself is reinstalled on every
                // start by `init_vector_rls`; this entry records the contract
                // change so a schema initialized by this build is identifiable.
                "memories + document_chunks POLICY *_owner_isolation USING: shared global/workspace read arm additionally requires COALESCE(current_setting('prx.owner_isolated', true), '') <> 'on'; prx.owner_isolated set transaction-local from require_owner_match; WITH CHECK and embedding_cache unchanged",
            ),
            (
                24,
                "memories_public_read_policy",
                // D-028: `public` memory rows are readable across owners, as
                // the shared SQL predicate and SQLite already allow. Read only.
                "memories POLICY memories_public_read FOR SELECT USING: transaction-local workspace and owner present AND workspace_id = prx.current_workspace AND visibility = 'public'; permissive, read only (UPDATE/DELETE/INSERT still need memories_owner_isolation); independent of prx.owner_isolated; document_chunks and embedding_cache unchanged",
            ),
        ]
    }

    /// Record-and-verify versioned schema migrations (FIX-P0-25, Postgres).
    ///
    /// - already-applied version, matching checksum → skipped;
    /// - already-applied version, differing checksum → `bail!` (fail-fast);
    /// - unapplied version → recorded.
    ///
    /// SAFETY: `schema_ident` is validated+quoted at construction via
    /// `validate_identifier()` + `quote_identifier()`; the interpolated table
    /// name cannot carry SQL injection. All values are passed as bind params.
    fn run_memory_schema_migrations(client: &mut Client, schema_ident: &str) -> Result<()> {
        let migrations_table = format!("{schema_ident}.memory_schema_migrations");
        client.execute(
            &format!(
                "CREATE TABLE IF NOT EXISTS {migrations_table} (
                    version    BIGINT PRIMARY KEY,
                    name       TEXT NOT NULL,
                    checksum   TEXT NOT NULL,
                    applied_at TEXT NOT NULL
                )"
            ),
            &[],
        )?;

        let select_sql = format!("SELECT checksum FROM {migrations_table} WHERE version = $1");
        let insert_sql =
            format!("INSERT INTO {migrations_table} (version, name, checksum, applied_at) VALUES ($1, $2, $3, $4)");

        for (version, name, text) in Self::memory_schema_migration_registry() {
            let checksum = Self::schema_migration_checksum(text);
            match client.query_opt(&select_sql, &[version])? {
                Some(row) => {
                    let recorded: String = row.get(0);
                    if recorded != checksum {
                        anyhow::bail!(
                            "memory schema migration checksum mismatch for version {version} ({name}): \
                             expected {checksum}, found {recorded}"
                        );
                    }
                }
                None => {
                    let name = (*name).to_string();
                    client.execute(&insert_sql, &[version, &name, &checksum, &Utc::now().to_rfc3339()])?;
                }
            }
        }
        Ok(())
    }

    /// FIX-P1-19: ensure `document_chunks.document_id` references
    /// `documents.document_id` with `ON DELETE CASCADE`.
    ///
    /// `ADD CONSTRAINT` is not idempotent in PostgreSQL, so we first probe
    /// `pg_constraint` for the named constraint and only add it when missing.
    ///
    /// SAFETY: both qualified table identifiers are validated and quoted at
    /// construction; all dynamic metadata values use bind params.
    fn ensure_document_chunks_fk(
        client: &mut Client,
        qualified_documents_table: &str,
        qualified_document_chunks_table: &str,
    ) -> Result<()> {
        const FK_NAME: &str = "fk_document_chunks_document";
        let exists: bool = client
            .query_one(
                "SELECT EXISTS (
                    SELECT 1
                    FROM pg_constraint
                    WHERE conrelid = to_regclass($1::text)
                      AND conname = $2
                )",
                &[&qualified_document_chunks_table, &FK_NAME],
            )?
            .get(0);
        if exists {
            return Ok(());
        }
        let add_fk = format!(
            "ALTER TABLE {qualified_document_chunks_table}
             ADD CONSTRAINT {FK_NAME}
             FOREIGN KEY (document_id)
             REFERENCES {qualified_documents_table}(document_id)
             ON DELETE CASCADE"
        );
        if let Err(error) = client.batch_execute(&add_fk) {
            // Tolerate a concurrent initializer that added the same constraint
            // between our probe and ALTER; surface anything else.
            let refreshed: bool = client
                .query_one(
                    "SELECT EXISTS (
                        SELECT 1
                        FROM pg_constraint
                        WHERE conrelid = to_regclass($1::text)
                          AND conname = $2
                    )",
                    &[&qualified_document_chunks_table, &FK_NAME],
                )?
                .get(0);
            if !refreshed {
                return Err(anyhow::anyhow!("failed to add document_chunks foreign key: {error}"));
            }
        }
        Ok(())
    }

    /// Upgrade the embedding cache from its historical global content key to
    /// a workspace/owner-scoped key. The cache stores vectors and therefore
    /// must not reveal reuse or content-derived state across principals.
    fn ensure_embedding_cache_scope_key(client: &mut Client, qualified_cache_table: &str) -> Result<()> {
        let primary = client.query_opt(
            "SELECT c.conname, pg_get_constraintdef(c.oid) \
             FROM pg_constraint c \
             WHERE c.conrelid = to_regclass($1::text) AND c.contype = 'p'",
            &[&qualified_cache_table],
        )?;
        if let Some(row) = primary {
            let name: String = row.get(0);
            let definition: String = row.get(1);
            if definition.contains("workspace_id") && definition.contains("owner_id") {
                return Ok(());
            }
            let constraint = quote_identifier(&name);
            client.batch_execute(&format!(
                "ALTER TABLE {qualified_cache_table} DROP CONSTRAINT {constraint}; \
                 ALTER TABLE {qualified_cache_table} \
                 ADD PRIMARY KEY (workspace_id, owner_id, content_hash, provider, model, dimensions);"
            ))?;
        } else {
            client.batch_execute(&format!(
                "ALTER TABLE {qualified_cache_table} \
                 ADD PRIMARY KEY (workspace_id, owner_id, content_hash, provider, model, dimensions);"
            ))?;
        }
        Ok(())
    }

    /// Replace the legacy global MessageEvent idempotency constraint with a
    /// workspace-scoped unique index. The constraint name is discovered from
    /// PostgreSQL metadata because derived table names may be truncated.
    fn ensure_message_event_idempotency_scope(client: &mut Client, qualified_message_events_table: &str) -> Result<()> {
        let legacy_constraint = client.query_opt(
            "SELECT conname
             FROM pg_constraint
             WHERE conrelid = to_regclass($1::text)
               AND contype = 'u'
               AND pg_get_constraintdef(oid) = 'UNIQUE (idempotency_key)'
             LIMIT 1",
            &[&qualified_message_events_table],
        )?;
        if let Some(row) = legacy_constraint {
            let constraint_name: String = row.get(0);
            validate_identifier(&constraint_name, "message event idempotency constraint")?;
            let constraint_ident = quote_identifier(&constraint_name);
            client.batch_execute(&format!(
                "ALTER TABLE {qualified_message_events_table}
                 DROP CONSTRAINT IF EXISTS {constraint_ident}"
            ))?;
        }
        client.batch_execute(&format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_message_events_workspace_idempotency
             ON {qualified_message_events_table}(workspace_id, idempotency_key)
             WHERE idempotency_key IS NOT NULL"
        ))?;
        Ok(())
    }

    fn try_init_pgvector(
        client: &mut Client,
        qualified_table: &str,
        qualified_document_chunks_table: &str,
        embedding_dimensions: usize,
    ) -> bool {
        if embedding_dimensions == 0 {
            return false;
        }
        let Ok(dimensions) = i64::try_from(embedding_dimensions) else {
            return false;
        };
        let init = format!(
            "
            CREATE EXTENSION IF NOT EXISTS vector;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS embedding_vector vector({dimensions});
            ALTER TABLE {qualified_document_chunks_table}
                ADD COLUMN IF NOT EXISTS embedding_vector vector({dimensions});
            CREATE INDEX IF NOT EXISTS idx_memories_embedding_vector_hnsw
                ON {qualified_table}
                USING hnsw (embedding_vector vector_cosine_ops)
                WHERE embedding_vector IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_document_chunks_embedding_vector_hnsw
                ON {qualified_document_chunks_table}
                USING hnsw (embedding_vector vector_cosine_ops)
                WHERE embedding_vector IS NOT NULL;
            "
        );
        match client.batch_execute(&init) {
            Ok(()) => true,
            Err(error) => {
                tracing::debug!(error = %error, "Postgres pgvector/ANN initialization skipped");
                false
            }
        }
    }

    /// Install the A04 vector isolation policy on every table that stores a
    /// retrievable vector. Missing workspace/owner context is deliberately not
    /// an allow case. The migration is fatal because starting without the policy
    /// would expose a different security contract than the report advertises.
    fn init_vector_rls(
        client: &mut Client,
        qualified_memories_table: &str,
        qualified_document_chunks_table: &str,
        qualified_embedding_cache_table: &str,
    ) -> Result<()> {
        let mut tx = client.transaction()?;
        for (table, policy, shared_reads) in [
            (qualified_memories_table, "memories_owner_isolation", true),
            (qualified_document_chunks_table, "document_chunks_owner_isolation", true),
            (
                qualified_embedding_cache_table,
                "embedding_cache_owner_isolation",
                false,
            ),
        ] {
            tx.batch_execute(&Self::vector_rls_policy_sql(table, policy, shared_reads))?;
        }
        tx.batch_execute(&Self::memories_public_read_policy_sql(
            qualified_memories_table,
            MEMORIES_PUBLIC_READ_POLICY,
        ))?;
        tx.commit()?;
        Ok(())
    }

    /// The read-only `public` policy on `memories` (D-028).
    ///
    /// `public` is the explicit sharing path (D-016), and the shared SQL
    /// predicate admits another owner's `public` memory row for every caller,
    /// owner-isolated or not. This permissive `FOR SELECT` policy lets RLS
    /// agree, within the caller's workspace and only with a full
    /// transaction-local context. It grants nothing to `UPDATE`, `DELETE` or
    /// `INSERT`: those still need the owner-exact `memories_owner_isolation`
    /// policy, so another owner's `public` row stays unwritable. It does not
    /// read `prx.owner_isolated`. `document_chunks` has no `public` tier.
    fn memories_public_read_policy_sql(table: &str, policy: &str) -> String {
        format!(
            "
            DROP POLICY IF EXISTS {policy} ON {table};
            CREATE POLICY {policy} ON {table}
                FOR SELECT
                USING (
                    NULLIF(current_setting('prx.current_workspace', true), '') IS NOT NULL
                    AND NULLIF(current_setting('prx.current_owner', true), '') IS NOT NULL
                    AND workspace_id = current_setting('prx.current_workspace', true)
                    AND visibility = 'public'
                );
            "
        )
    }

    /// The row-level-security DDL for one vector table.
    ///
    /// Reads and writes both require the transaction-local workspace and
    /// owner. Inserted and updated rows must carry the caller's owner (WITH
    /// CHECK). Update and delete *targeting* is governed by USING, the read
    /// scope below: owner-isolated callers can update or delete only their
    /// own rows, but for other callers the policy also admits other owners'
    /// `workspace`/`global` rows, and the statement's own SQL predicate
    /// decides (R-B2 F-3). On the shared tables (`memories`,
    /// `document_chunks`) a read also admits another owner's `global` or
    /// `workspace` row, but only for a caller that is not owner-isolated:
    /// OQ-8 / D-019 make the shared SQL predicate the owner contract, and an
    /// owner-isolated caller (`prx.owner_isolated = 'on'`, set by
    /// [`Self::apply_rls_context_raw`]) reaches only its own rows here, so the
    /// policy floor matches that predicate instead of being wider than it. An
    /// unset flag keeps the historical tenant-wide read scope. The embedding
    /// cache has no shared arm at all.
    fn vector_rls_policy_sql(table: &str, policy: &str, shared_reads: bool) -> String {
        let read_scope = if shared_reads {
            "(owner_id = current_setting('prx.current_owner', true) \
             OR (visibility IN ('global', 'workspace') \
                 AND COALESCE(current_setting('prx.owner_isolated', true), '') <> 'on'))"
        } else {
            "owner_id = current_setting('prx.current_owner', true)"
        };
        format!(
            "
            ALTER TABLE {table} ENABLE ROW LEVEL SECURITY;
            ALTER TABLE {table} FORCE ROW LEVEL SECURITY;
            DROP POLICY IF EXISTS {policy} ON {table};
            CREATE POLICY {policy} ON {table}
                USING (
                    current_setting('prx.rls_bypass', true) = 'on'
                    OR (
                        NULLIF(current_setting('prx.current_workspace', true), '') IS NOT NULL
                        AND NULLIF(current_setting('prx.current_owner', true), '') IS NOT NULL
                        AND workspace_id = current_setting('prx.current_workspace', true)
                        AND {read_scope}
                    )
                )
                WITH CHECK (
                    current_setting('prx.rls_bypass', true) = 'on'
                    OR (
                        NULLIF(current_setting('prx.current_workspace', true), '') IS NOT NULL
                        AND NULLIF(current_setting('prx.current_owner', true), '') IS NOT NULL
                        AND workspace_id = current_setting('prx.current_workspace', true)
                        AND owner_id = current_setting('prx.current_owner', true)
                    )
                );
            "
        )
    }

    /// Bind trusted isolation context to the current transaction only. Values
    /// come from resolved runtime principals, never prompt/tool arguments. A
    /// non-system operation without both identifiers fails before SQL executes.
    ///
    /// `owner_isolated` is the caller's `require_owner_match` bit: when set,
    /// the vector RLS policies drop their shared `global`/`workspace` read arm
    /// for this transaction (see [`Self::vector_rls_policy_sql`]).
    fn apply_rls_context_raw<C: GenericClient>(
        client: &mut C,
        system: bool,
        workspace_id: Option<&str>,
        owner_id: Option<&str>,
        owner_isolated: bool,
    ) -> Result<()> {
        if system {
            client.execute("SELECT set_config('prx.rls_bypass', 'on', true)", &[])?;
            return Ok(());
        }

        let workspace = workspace_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("PostgreSQL vector isolation requires a trusted workspace context")?;
        let owner = owner_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("PostgreSQL vector isolation requires a trusted owner context")?;
        client.execute("SELECT set_config('prx.rls_bypass', 'off', true)", &[])?;
        client.execute("SELECT set_config('prx.current_workspace', $1, true)", &[&workspace])?;
        // The owner and its isolation bit are bound in one statement: the
        // policy reads them together, and it saves a round trip.
        let isolation = if owner_isolated { "on" } else { "off" };
        client.execute(
            "SELECT set_config('prx.current_owner', $1, true), set_config('prx.owner_isolated', $2, true)",
            &[&owner, &isolation],
        )?;
        Ok(())
    }

    fn category_to_str(category: &MemoryCategory) -> String {
        match category {
            MemoryCategory::Core => "core".to_string(),
            MemoryCategory::Daily => "daily".to_string(),
            MemoryCategory::Conversation => "conversation".to_string(),
            MemoryCategory::Custom(name) => name.clone(),
        }
    }

    fn content_hash(content: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    fn content_sha256_hex(content: &str) -> String {
        Self::content_hash(content)
    }

    /// Render an embedding in pgvector's text input form (`[a,b,c]`).
    ///
    /// PG-VECTOR: the result is bound as a Rust `String`, so every statement
    /// that receives it casts the parameter to `text` first and only then to
    /// `vector`. A parameter cast straight to `vector` makes the server type
    /// the parameter as `vector`, which a `String` cannot bind to, and the
    /// statement fails before it runs. `tests/pgvector_binding_regression.rs`
    /// guards every such cast.
    fn pgvector_literal(embedding: &[f32]) -> Option<String> {
        if embedding.is_empty() || embedding.iter().any(|value| !value.is_finite()) {
            return None;
        }
        Some(format!(
            "[{}]",
            embedding
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ))
    }

    const fn memory_category_needs_embedding(category: &MemoryCategory) -> bool {
        matches!(category, MemoryCategory::Core | MemoryCategory::Custom(_))
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

    async fn get_or_compute_embedding(
        &self,
        text: &str,
        workspace_id: Option<&str>,
        owner_id: Option<&str>,
        system: bool,
    ) -> Result<Option<Vec<f32>>> {
        if self.embedder.dimensions() == 0 {
            return Ok(None);
        }
        let (scope_workspace, scope_owner) = if system {
            ("__prx_system__".to_string(), "__prx_system__".to_string())
        } else {
            (
                workspace_id
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .context("PostgreSQL embedding cache requires workspace context")?
                    .to_string(),
                owner_id
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .context("PostgreSQL embedding cache requires owner context")?
                    .to_string(),
            )
        };
        let content_hash = Self::content_hash(text);
        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();
        let cache_table = self.qualified_embedding_cache_table.clone();
        let client = self.client.clone();
        let cache_hash = content_hash.clone();
        let cache_provider = provider_name.clone();
        let cache_model = model_name.clone();
        let select_workspace = scope_workspace.clone();
        let select_owner = scope_owner.clone();
        let cached = tokio::task::spawn_blocking(move || -> Result<Option<Vec<f32>>> {
            let select_stmt = format!(
                "
                SELECT embedding
                FROM {cache_table}
                WHERE workspace_id = $1
                  AND owner_id = $2
                  AND content_hash = $3
                  AND provider = $4
                  AND model = $5
                  AND dimensions = $6
                LIMIT 1
                "
            );
            let update_stmt = format!(
                "
                UPDATE {cache_table}
                SET accessed_at = $7
                WHERE workspace_id = $1
                  AND owner_id = $2
                  AND content_hash = $3
                  AND provider = $4
                  AND model = $5
                  AND dimensions = $6
                "
            );
            client.with_client(|client| {
                let mut tx = client.transaction()?;
                // The cache policy is owner exact; it has no shared arm to narrow.
                Self::apply_rls_context_raw(&mut tx, system, Some(&select_workspace), Some(&select_owner), false)?;
                let row = tx.query_opt(
                    &select_stmt,
                    &[
                        &select_workspace,
                        &select_owner,
                        &cache_hash,
                        &cache_provider,
                        &cache_model,
                        &dimensions,
                    ],
                )?;
                let Some(row) = row else {
                    tx.commit()?;
                    return Ok(None);
                };
                let bytes: Vec<u8> = row.get(0);
                let embedding = vector::bytes_to_vec(&bytes);
                if embedding.len() != usize::try_from(dimensions).unwrap_or(usize::MAX) {
                    tx.commit()?;
                    return Ok(None);
                }
                let now = Utc::now();
                tx.execute(
                    &update_stmt,
                    &[
                        &select_workspace,
                        &select_owner,
                        &cache_hash,
                        &cache_provider,
                        &cache_model,
                        &dimensions,
                        &now,
                    ],
                )?;
                tx.commit()?;
                Ok(Some(embedding))
            })
        })
        .await??;
        if cached.is_some() {
            return Ok(cached);
        }

        let embedding = self.embedder.embed_one(text).await?;
        if embedding.len() != self.embedder.dimensions() {
            anyhow::bail!(
                "embedding dimension mismatch: provider={} model={} expected={} got={}",
                self.embedder.name(),
                self.embedder.model(),
                self.embedder.dimensions(),
                embedding.len()
            );
        }
        let cache_table = self.qualified_embedding_cache_table.clone();
        let client = self.client.clone();
        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();
        let cache_max = self.embedding_cache_max_rows;
        let bytes = vector::vec_to_bytes(&embedding);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let insert_stmt = format!(
                "
                INSERT INTO {cache_table} (
                    workspace_id, owner_id, content_hash, embedding, provider, model,
                    dimensions, created_at, accessed_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)
                ON CONFLICT (workspace_id, owner_id, content_hash, provider, model, dimensions)
                DO UPDATE SET embedding = EXCLUDED.embedding, accessed_at = EXCLUDED.accessed_at
                "
            );
            let evict_stmt = format!(
                "
                DELETE FROM {cache_table}
                WHERE workspace_id = $1 AND owner_id = $2
                  AND (content_hash, provider, model, dimensions) IN (
                    SELECT content_hash, provider, model, dimensions
                    FROM {cache_table}
                    WHERE workspace_id = $1 AND owner_id = $2
                    ORDER BY accessed_at ASC
                    LIMIT GREATEST(
                        (SELECT COUNT(*) FROM {cache_table}
                         WHERE workspace_id = $1 AND owner_id = $2) - $3,
                        0
                    )
                )
                "
            );
            client.with_client(|client| {
                let mut tx = client.transaction()?;
                // The cache policy is owner exact; it has no shared arm to narrow.
                Self::apply_rls_context_raw(&mut tx, system, Some(&scope_workspace), Some(&scope_owner), false)?;
                let now = Utc::now();
                tx.execute(
                    &insert_stmt,
                    &[
                        &scope_workspace,
                        &scope_owner,
                        &content_hash,
                        &bytes,
                        &provider_name,
                        &model_name,
                        &dimensions,
                        &now,
                    ],
                )?;
                tx.execute(&evict_stmt, &[&scope_workspace, &scope_owner, &cache_max])?;
                tx.commit()?;
                Ok(())
            })
        })
        .await??;
        Ok(Some(embedding))
    }

    async fn embedding_metadata_for_category(
        &self,
        category: &MemoryCategory,
        content: &str,
        workspace_id: Option<&str>,
        owner_id: Option<&str>,
        system: bool,
    ) -> Result<(
        Option<Vec<u8>>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
    )> {
        if !Self::memory_category_needs_embedding(category) {
            return Ok((None, None, None, None, None));
        }
        let Some(embedding) = self
            .get_or_compute_embedding(content, workspace_id, owner_id, system)
            .await?
        else {
            return Ok((None, None, None, None, None));
        };
        let pgvector_literal = self
            .pgvector_available
            .then(|| Self::pgvector_literal(&embedding))
            .flatten();
        Ok((
            Some(vector::vec_to_bytes(&embedding)),
            Some(self.embedding_provider_name()),
            Some(self.embedding_model_name()),
            Some(self.embedding_dimensions_i64()),
            pgvector_literal,
        ))
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
            super::principal::OwnerPrincipal::new(
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
    /// search statement (V02-R8). Twin of `SqliteMemory::document_visibility_guard`.
    ///
    /// Those statements admit a chunk when the owning document is `global` or
    /// `workspace` **or** when the chunk's owner matches the caller. The first
    /// disjunct short-circuits the second, and a document's visibility defaults
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
    /// the empty string for a non-isolated principal, leaving the statements and
    /// their bind lists byte-identical, and `" AND FALSE"` for an isolated one.
    /// It binds no parameter in either case, so the placeholder accounting of
    /// these statements is unchanged.
    ///
    /// This is a statement-level filter. The row-level security policy on the
    /// chunk table is a second, independent layer and is untouched.
    fn document_visibility_guard(principal: &MemoryPrincipal) -> String {
        // Unused: this arm never binds, because it never has an owner to bind.
        const UNBOUND_INDEX: usize = 1;
        crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            UNBOUND_INDEX,
            principal.require_owner_match,
            false,
        )
        .sql
    }

    /// The owner id an owner-isolated delete is confined to, with blanks
    /// treated as absent — the same rule
    /// [`Self::isolated_read_without_owner`] applies to the keyed reads, so the
    /// short-circuit and this anchor can never disagree about what "no owner"
    /// means. A non-isolated context never yields one.
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
    /// non-isolated caller (statement and bind list byte-identical) and
    /// `" AND owner_id = $N"` for an isolated one. The `" AND FALSE"` case
    /// cannot be reached here: an isolated context with no usable owner id has
    /// already returned above, before any statement or RLS call. `index` is the
    /// caller's next free placeholder, so the one-marker-one-parameter
    /// accounting is preserved.
    ///
    /// This is a statement-level filter; the row-level security policy on the
    /// memory table is a second, independent layer and is untouched.
    fn forget_owner_guard(
        context: &MemoryWriteContext,
        index: usize,
    ) -> crate::memory::owner_predicate::OwnerMatchPredicate {
        crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            index,
            context.require_owner_match,
            Self::forget_owner_anchor(context).is_some(),
        )
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

    /// D4 read-merge: distinct legacy `session_key` candidate(s) bound to *new
    /// trailing* `$N` placeholders. See the SQLite twin
    /// `SqliteMemory::legacy_session_key_params` for the shared contract.
    fn legacy_session_key_params(principal: &MemoryPrincipal) -> Vec<String> {
        let mut candidates = principal.session_key_candidates();
        if candidates.is_empty() {
            candidates
        } else {
            candidates.split_off(1)
        }
    }

    /// D4 read-merge: placeholder indices for the canonical key plus trailing
    /// legacy key(s). Mirrors `SqliteMemory::session_indices`.
    /// C03 owner isolation: 1-based placeholder index for the owner parameter.
    ///
    /// The owner id always binds *after* the D4 legacy session keys, so the
    /// index is the first free trailing slot once those are accounted for.
    const fn owner_index(legacy_start: usize, legacy_key_count: usize) -> usize {
        legacy_start + legacy_key_count
    }

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

    /// Whether an owner-isolated read must answer empty without touching the
    /// database (C03 / V02-R5, follow-up F1).
    ///
    /// Under isolation the RLS owner follows `context.owner_id` alone, and a
    /// blank or absent owner id leaves it `None`. `apply_rls_context_raw`
    /// rejects a non-system read that carries no owner, so without this
    /// decision the read would return an *error* where the SQLite adapter
    /// answers with an empty result — the same fail-closed intent, two
    /// different contracts for the caller. The blank rule is the one
    /// [`crate::memory::owner_predicate::owner_match_suffix`] applies to the
    /// SQL arm, so both ends agree on what "no owner" means.
    ///
    /// A non-isolated context is never short-circuited: its read path and its
    /// RLS owner are byte-identical to the historical ones.
    #[must_use]
    fn isolated_read_without_owner(context: &MemoryWriteContext) -> bool {
        context.require_owner_match && context.owner_id.as_deref().map(str::trim).is_none_or(str::is_empty)
    }

    fn principal_from_context(context: &MemoryWriteContext) -> Principal {
        let current_channel = context.channel.clone().unwrap_or_default();
        let current_chat_id = context.chat_id.clone().unwrap_or_default();
        let current_chat_type = context
            .chat_type
            .as_deref()
            .map(ChatType::from_str)
            .unwrap_or(ChatType::Dm);
        let user_id = context.sender_id.clone().unwrap_or_else(|| {
            let channel = context.channel.as_deref().unwrap_or("unknown");
            let raw_sender = context.raw_sender.as_deref().unwrap_or("unknown");
            format!("anonymous:{channel}:{raw_sender}")
        });
        let role = if context.sender_id.is_some() {
            Role::Member
        } else {
            Role::Anonymous
        };

        Principal {
            user_id,
            role,
            projects: Vec::new(),
            visibility_ceiling: Visibility::Private,
            blocked_patterns: Vec::new(),
            current_channel,
            current_chat_id,
            current_chat_type,
            // FIX-P1-06: carry the raw sender anchor so build_sql_scope can match
            // self-authored rows for anonymous principals.
            raw_sender: context.raw_sender.clone().unwrap_or_default(),
            acl_enforced: true,
            owner_id: None,
            require_owner_match: false,
        }
        .with_owner_scope(context)
    }

    /// Qualified name of a schema-scoped helper table (lives directly under the
    /// schema, like `agent_identity_bindings` / `approval_grants`).
    ///
    /// `name` is a hard-coded ASCII identifier supplied only by this module, so
    /// it carries no injection risk; `schema_ident` is validated+quoted at
    /// construction.
    fn schema_scoped_table(&self, name: &str) -> String {
        format!("{}.{}", self.schema_ident, name)
    }

    fn qualified_sessions_table(&self) -> String {
        self.schema_scoped_table("sessions")
    }

    fn qualified_conversation_turns_table(&self) -> String {
        self.schema_scoped_table("conversation_turns")
    }

    fn qualified_identity_bindings_table(&self) -> String {
        self.schema_scoped_table("identity_bindings")
    }

    fn qualified_chat_profiles_table(&self) -> String {
        self.schema_scoped_table("chat_profiles")
    }

    fn qualified_user_policies_table(&self) -> String {
        self.schema_scoped_table("user_policies")
    }

    fn qualified_access_audit_log_table(&self) -> String {
        self.schema_scoped_table("access_audit_log")
    }

    /// FIX-P0-22: append an access-audit row (parity with SQLite `log_access`).
    /// System/owner principals are not audited. Best-effort: a logging failure
    /// must not abort the caller's operation, so errors are traced and dropped.
    fn log_access_best_effort_blocking(
        client: &PostgresConnectionPool,
        audit_table: &str,
        principal: &Principal,
        action: &str,
        query: Option<&str>,
        memory_id: Option<&str>,
        policy_rule: Option<&str>,
        result: &str,
    ) {
        if principal.role == Role::Owner {
            return;
        }
        let sql = format!(
            "INSERT INTO {audit_table} \
             (id, timestamp, requester, action, query, memory_id, policy_rule, result) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"
        );
        let id = Uuid::new_v4().to_string();
        let timestamp = Utc::now().to_rfc3339();
        let outcome = client.with_client(|client| {
            client.execute(
                &sql,
                &[
                    &id,
                    &timestamp,
                    &principal.user_id,
                    &action,
                    &query,
                    &memory_id,
                    &policy_rule,
                    &result,
                ],
            )?;
            Ok(())
        });
        if let Err(error) = outcome {
            tracing::debug!(error = %error, "Postgres access-audit log write failed (best-effort)");
        }
    }

    async fn log_access_best_effort(
        client: Arc<PostgresConnectionPool>,
        audit_table: String,
        principal: Principal,
        action: &'static str,
        query: Option<String>,
        memory_id: Option<String>,
        policy_rule: Option<&'static str>,
        result: &'static str,
    ) {
        let outcome = tokio::task::spawn_blocking(move || {
            Self::log_access_best_effort_blocking(
                &client,
                &audit_table,
                &principal,
                action,
                query.as_deref(),
                memory_id.as_deref(),
                policy_rule,
                result,
            );
        })
        .await;
        if let Err(error) = outcome {
            tracing::debug!(error = %error, "Postgres access-audit task failed (best-effort)");
        }
    }

    /// FIX-P0-21 (#1 F1): resolve a full ACL [`Principal`] from a write context
    /// by querying `identity_bindings` + `user_policies` (parity with the SQLite
    /// `resolve_principal`). Falls back to an anonymous principal when no binding
    /// or policy is found, or when the context lacks a channel/raw_sender.
    ///
    /// This is the async PrincipalResolver: it runs the lookups on a blocking
    /// task so the synchronous `postgres` client can be used without blocking the
    /// async runtime.
    async fn resolve_principal_from_context(&self, context: &MemoryWriteContext) -> Principal {
        let current_channel = context.channel.clone().unwrap_or_default();
        let current_chat_id = context.chat_id.clone().unwrap_or_default();
        let current_chat_type = context
            .chat_type
            .as_deref()
            .map(ChatType::from_str)
            .unwrap_or(ChatType::Dm);

        // FIX-P1-06: raw sender anchor shared by the fallback and resolved paths.
        let raw_sender_anchor = context.raw_sender.clone().unwrap_or_default();
        let fallback = |user_id: String| {
            Principal {
                user_id,
                role: Role::Anonymous,
                projects: Vec::new(),
                visibility_ceiling: Visibility::Private,
                blocked_patterns: Vec::new(),
                current_channel: current_channel.clone(),
                current_chat_id: current_chat_id.clone(),
                current_chat_type: current_chat_type.clone(),
                raw_sender: raw_sender_anchor.clone(),
                acl_enforced: true,
                owner_id: None,
                require_owner_match: false,
            }
            .with_owner_scope(context)
        };

        let (Some(channel), Some(raw_sender)) = (context.channel.clone(), context.raw_sender.clone()) else {
            let channel = context.channel.as_deref().unwrap_or("unknown");
            let raw_sender = context.raw_sender.as_deref().unwrap_or("unknown");
            return fallback(format!("anonymous:{channel}:{raw_sender}"));
        };

        let client = self.client.clone();
        let bindings_table = self.qualified_identity_bindings_table();
        let policies_table = self.qualified_user_policies_table();
        let lookup_channel = channel.clone();
        let lookup_raw_sender = raw_sender.clone();

        let resolved = tokio::task::spawn_blocking(
            move || -> Result<Option<(String, Option<(String, String, String, String)>)>> {
                client.with_client(|client| {
                    let binding_sql = format!(
                        "SELECT user_id FROM {bindings_table} WHERE channel = $1 AND channel_account = $2 LIMIT 1"
                    );
                    let Some(binding_row) = client.query_opt(&binding_sql, &[&lookup_channel, &lookup_raw_sender])?
                    else {
                        return Ok(None);
                    };
                    let user_id: String = binding_row.get(0);

                    let policy_sql = format!(
                        "SELECT role, projects, visibility_ceiling, blocked_patterns \
                     FROM {policies_table} WHERE user_id = $1 LIMIT 1"
                    );
                    let policy = client.query_opt(&policy_sql, &[&user_id])?.map(|row| {
                        (
                            row.get::<_, String>(0),
                            row.get::<_, String>(1),
                            row.get::<_, String>(2),
                            row.get::<_, String>(3),
                        )
                    });
                    Ok(Some((user_id, policy)))
                })
            },
        )
        .await;

        let lookup = match resolved {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                tracing::debug!(error = %error, "Postgres principal resolution query failed; using anonymous");
                None
            }
            Err(join_error) => {
                tracing::debug!(error = %join_error, "Postgres principal resolution task panicked; using anonymous");
                None
            }
        };

        let Some((user_id, policy)) = lookup else {
            return fallback(format!("anonymous:{channel}:{raw_sender}"));
        };

        super::principal::principal_from_policy(
            user_id,
            policy,
            current_channel,
            current_chat_id,
            current_chat_type,
            raw_sender_anchor,
        )
        .with_owner_scope(context)
    }

    fn parse_category(value: &str) -> MemoryCategory {
        match value {
            "core" => MemoryCategory::Core,
            "daily" => MemoryCategory::Daily,
            "conversation" => MemoryCategory::Conversation,
            other => MemoryCategory::Custom(other.to_string()),
        }
    }

    fn row_to_entry(row: &Row) -> Result<MemoryEntry> {
        let timestamp: DateTime<Utc> = row.get(4);

        Ok(MemoryEntry {
            id: row.get(0),
            key: row.get(1),
            content: row.get(2),
            category: Self::parse_category(&row.get::<_, String>(3)),
            timestamp: timestamp.to_rfc3339(),
            session_id: row.get(5),
            score: row.try_get(6).ok(),
            tags: None,
            access_count: None,
            useful_count: row.try_get(7).ok(),
            source: None,
            source_confidence: None,
            verification_status: None,
            lifecycle_state: None,
            compressed_from: None,
        })
    }

    fn row_to_message_event(row: &Row) -> Result<MessageEvent> {
        let source_legacy: String = row.get(5);
        let source_ref_json: Option<String> = row.get(17);
        let source = source_ref_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or_else(|| source_legacy.into());
        let subject_ref_json: Option<String> = row.get(18);
        let subject = subject_ref_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok());
        let created_at: DateTime<Utc> = row.get(30);
        let updated_at: DateTime<Utc> = row.get(31);
        let visibility = row
            .get::<_, String>(29)
            .parse::<MemoryVisibility>()
            .unwrap_or(MemoryVisibility::Workspace);

        Ok(MessageEvent {
            id: row.get(0),
            event_id: row.get(1),
            idempotency_key: row.get(2),
            workspace_id: row.get(3),
            owner_id: row.get(4),
            source,
            channel: row.get(6),
            session_key: row.get(7),
            parent_session_key: row.get(8),
            run_id: row.get(9),
            parent_run_id: row.get(10),
            agent_id: row.get(11),
            persona_id: row.get(12),
            sender: row.get(13),
            recipient: row.get(14),
            role: row.get(15),
            event_type: row
                .get::<_, Option<String>>(16)
                .unwrap_or_else(|| "message.legacy".to_string()),
            subject,
            goal_id: row.get(19),
            causation_event_id: row.get(20),
            correlation_id: row.get(21),
            attempt_id: row.get(22),
            lease_epoch: row.get(23),
            config_generation_id: row
                .get::<_, Option<i64>>(24)
                .and_then(|value| u64::try_from(value).ok()),
            config_source_revision: row.get(25),
            content: row.get(26),
            content_hash: row.get(27),
            raw_payload_json: row.get(28),
            visibility,
            created_at: created_at.to_rfc3339(),
            updated_at: updated_at.to_rfc3339(),
        })
    }

    fn row_to_memory_event(row: &Row) -> Result<MemoryEvent> {
        let created_at: DateTime<Utc> = row.get(14);
        let visibility = row
            .get::<_, String>(12)
            .parse::<MemoryVisibility>()
            .unwrap_or(MemoryVisibility::Workspace);

        Ok(MemoryEvent {
            id: row.get(0),
            event_id: row.get(1),
            workspace_id: row.get(2),
            owner_id: row.get(3),
            event_type: row.get(4),
            subject_table: row.get(5),
            subject_id: row.get(6),
            session_key: row.get(7),
            run_id: row.get(8),
            parent_run_id: row.get(9),
            agent_id: row.get(10),
            persona_id: row.get(11),
            visibility,
            payload_json: row.get(13),
            created_at: created_at.to_rfc3339(),
        })
    }

    fn row_to_draft(row: &Row) -> Result<MemoryDraft> {
        let created_at: DateTime<Utc> = row.get(16);
        let updated_at: DateTime<Utc> = row.get(17);
        let visibility = row
            .get::<_, String>(13)
            .parse::<MemoryVisibility>()
            .unwrap_or(MemoryVisibility::Workspace);

        Ok(MemoryDraft {
            id: row.get(0),
            draft_id: row.get(1),
            workspace_id: row.get(2),
            owner_id: row.get(3),
            worker_run_id: row.get(4),
            parent_run_id: row.get(5),
            session_key: row.get(6),
            agent_id: row.get(7),
            persona_id: row.get(8),
            key: row.get(9),
            content: row.get(10),
            category: Self::parse_category(&row.get::<_, String>(11)),
            source_event_id: row.get(12),
            visibility,
            status: row.get(14),
            payload_json: row.get(15),
            created_at: created_at.to_rfc3339(),
            updated_at: updated_at.to_rfc3339(),
        })
    }

    fn row_to_document(row: &Row) -> Result<DocumentRecord> {
        let created_at: DateTime<Utc> = row.get(15);
        let updated_at: DateTime<Utc> = row.get(16);
        let visibility = row
            .get::<_, String>(12)
            .parse::<MemoryVisibility>()
            .unwrap_or(MemoryVisibility::Workspace);
        let chunk_count: i64 = row.get(14);

        Ok(DocumentRecord {
            id: row.get(0),
            document_id: row.get(1),
            workspace_id: row.get(2),
            owner_id: row.get(3),
            topic_id: row.get(4),
            task_id: row.get(5),
            source_message_event_id: row.get(6),
            source_kind: row.get(7),
            source_uri: row.get(8),
            title: row.get(9),
            content_sha256: row.get(10),
            mime_type: row.get(11),
            visibility,
            metadata_json: row.get(13),
            chunk_count: usize::try_from(chunk_count.max(0)).unwrap_or(usize::MAX),
            created_at: created_at.to_rfc3339(),
            updated_at: updated_at.to_rfc3339(),
        })
    }

    fn row_to_document_chunk(row: &Row) -> Result<DocumentChunkRecord> {
        let created_at: DateTime<Utc> = row.get(13);
        let chunk_index: i64 = row.get(7);
        let token_estimate: i64 = row.get(12);
        Ok(DocumentChunkRecord {
            id: row.get(0),
            chunk_id: row.get(1),
            document_id: row.get(2),
            workspace_id: row.get(3),
            owner_id: row.get(4),
            topic_id: row.get(5),
            task_id: row.get(6),
            chunk_index: usize::try_from(chunk_index.max(0)).unwrap_or(usize::MAX),
            heading: row.get(8),
            content: row.get(9),
            content_sha256: row.get(10),
            source_anchor: row.get(11),
            token_estimate: usize::try_from(token_estimate.max(0)).unwrap_or(usize::MAX),
            created_at: created_at.to_rfc3339(),
        })
    }

    fn row_to_memory_link(row: &Row) -> Result<MemoryLink> {
        let created_at: DateTime<Utc> = row.get(11);
        Ok(MemoryLink {
            id: row.get(0),
            link_id: row.get(1),
            workspace_id: row.get(2),
            owner_id: row.get(3),
            memory_key: row.get(4),
            memory_event_id: row.get(5),
            message_event_id: row.get(6),
            document_id: row.get(7),
            chunk_id: row.get(8),
            link_type: row.get(9),
            payload_json: row.get(10),
            created_at: created_at.to_rfc3339(),
        })
    }

    fn row_to_retrieval_trace(row: &Row) -> Result<RetrievalTrace> {
        let created_at: DateTime<Utc> = row.get(16);
        let candidate_count: i64 = row.get(9);
        let selected_count: i64 = row.get(10);
        let dropped_count: i64 = row.get(11);
        let budget_tokens: Option<i64> = row.get(12);
        Ok(RetrievalTrace {
            id: row.get(0),
            trace_id: row.get(1),
            workspace_id: row.get(2),
            owner_id: row.get(3),
            session_key: row.get(4),
            agent_id: row.get(5),
            persona_id: row.get(6),
            source: row.get(7),
            query: row.get(8),
            candidate_count: usize::try_from(candidate_count.max(0)).unwrap_or(usize::MAX),
            selected_count: usize::try_from(selected_count.max(0)).unwrap_or(usize::MAX),
            dropped_count: usize::try_from(dropped_count.max(0)).unwrap_or(usize::MAX),
            budget_tokens: budget_tokens.and_then(|value| usize::try_from(value.max(0)).ok()),
            selected_json: row.get(13),
            dropped_json: row.get(14),
            payload_json: row.get(15),
            created_at: created_at.to_rfc3339(),
        })
    }

    fn row_to_compaction_run(row: &Row) -> Result<CompactionRun> {
        let created_at: DateTime<Utc> = row.get(18);
        let source_message_count: i64 = row.get(9);
        let source_token_estimate: i64 = row.get(10);
        Ok(CompactionRun {
            id: row.get(0),
            run_id: row.get(1),
            workspace_id: row.get(2),
            owner_id: row.get(3),
            session_key: row.get(4),
            agent_id: row.get(5),
            persona_id: row.get(6),
            trigger: row.get(7),
            mode: row.get(8),
            source_message_count: usize::try_from(source_message_count.max(0)).unwrap_or(usize::MAX),
            source_token_estimate: usize::try_from(source_token_estimate.max(0)).unwrap_or(usize::MAX),
            summary: row.get(11),
            summary_memory_key: row.get(12),
            source_event_ids_json: row.get(13),
            source_event_range_json: row.get(14),
            source_document_refs_json: row.get(15),
            fidelity_status: row.get(16),
            payload_json: row.get(17),
            created_at: created_at.to_rfc3339(),
        })
    }
}

pub(crate) fn validate_identifier(value: &str, field_name: &str) -> Result<()> {
    // PostgreSQL identifiers: start with letter or underscore, followed by
    // letters, digits, or underscores.  Maximum length is 63 bytes (NAMEDATALEN-1).
    // We enforce the regex ^[a-zA-Z_][a-zA-Z0-9_]{0,62}$ to prevent any
    // possibility of SQL injection through identifier manipulation.
    if value.is_empty() {
        anyhow::bail!("{field_name} must not be empty");
    }

    if value.len() > 63 {
        anyhow::bail!(
            "{field_name} exceeds PostgreSQL identifier limit of 63 characters; got {} characters",
            value.len()
        );
    }

    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        anyhow::bail!("{field_name} must not be empty");
    };

    if !(first.is_ascii_alphabetic() || first == '_') {
        anyhow::bail!("{field_name} must start with an ASCII letter or underscore; got '{value}'");
    }

    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        anyhow::bail!("{field_name} can only contain ASCII letters, numbers, and underscores; got '{value}'");
    }

    Ok(())
}

pub(crate) fn related_table_name(base: &str, suffix: &str) -> Result<String> {
    let max_base_len = 63usize
        .checked_sub(suffix.len())
        .context("related table suffix exceeds PostgreSQL identifier length")?;
    let prefix = if base.len() > max_base_len {
        &base[..max_base_len]
    } else {
        base
    };
    let name = format!("{prefix}{suffix}");
    validate_identifier(&name, "related storage table")?;
    Ok(name)
}

pub(crate) fn quote_identifier(value: &str) -> String {
    format!("\"{value}\"")
}

fn chat_profile_from_pg_row(row: &Row) -> ChatProfile {
    let tags_json: String = row.get(7);
    let tags = serde_json::from_str::<Vec<String>>(&tags_json).unwrap_or_default();
    ChatProfile {
        id: row.get(0),
        channel: row.get(1),
        chat_id: row.get(2),
        chat_kind: row.get(3),
        title: row.get(4),
        purpose: row.get(5),
        notes: row.get(6),
        tags,
        updated_by: row.get(8),
        created_at: row.get(9),
        updated_at: row.get(10),
    }
}

#[async_trait]
impl Memory for PostgresMemory {
    fn name(&self) -> &str {
        "postgres"
    }

    async fn upsert_chat_profile_metadata(
        &self,
        channel: &str,
        chat_id: &str,
        chat_kind: &str,
        title: Option<&str>,
    ) -> Result<()> {
        let client = self.client.clone();
        let table = self.qualified_chat_profiles_table();
        let channel = channel.to_string();
        let chat_id = chat_id.to_string();
        let chat_kind = chat_kind.to_string();
        let title = title.map(str::to_string);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let now = Utc::now().to_rfc3339();
            let id = Uuid::new_v4().to_string();
            let stmt = format!(
                "INSERT INTO {table}
                    (id, channel, chat_id, chat_kind, title, purpose, notes, tags, updated_by, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, NULL, NULL, '[]', 'auto', $6, $6)
                 ON CONFLICT (channel, chat_id) DO UPDATE SET
                    chat_kind = EXCLUDED.chat_kind,
                    title = CASE
                        WHEN EXCLUDED.title IS NOT NULL THEN EXCLUDED.title
                        ELSE {table}.title
                    END,
                    updated_at = EXCLUDED.updated_at"
            );
            client.with_client(|client| {
                client.execute(&stmt, &[&id, &channel, &chat_id, &chat_kind, &title, &now])?;
                Ok(())
            })
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
    ) -> Result<ChatProfile> {
        let client = self.client.clone();
        let table = self.qualified_chat_profiles_table();
        let channel = channel.to_string();
        let chat_id = chat_id.to_string();
        let chat_kind = chat_kind.to_string();
        let purpose = purpose.map(str::to_string);
        let notes = notes.map(str::to_string);
        let tags_json = tags.map(serde_json::to_string).transpose()?;
        let updated_by = updated_by.to_string();
        tokio::task::spawn_blocking(move || -> Result<ChatProfile> {
            let now = Utc::now().to_rfc3339();
            let id = Uuid::new_v4().to_string();
            let stmt = format!(
                "INSERT INTO {table}
                    (id, channel, chat_id, chat_kind, title, purpose, notes, tags, updated_by, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, NULL, $5, $6, COALESCE($7, '[]'), $8, $9, $9)
                 ON CONFLICT (channel, chat_id) DO UPDATE SET
                    chat_kind = EXCLUDED.chat_kind,
                    purpose = COALESCE(EXCLUDED.purpose, {table}.purpose),
                    notes = COALESCE(EXCLUDED.notes, {table}.notes),
                    tags = COALESCE($7, {table}.tags),
                    updated_by = EXCLUDED.updated_by,
                    updated_at = EXCLUDED.updated_at
                 RETURNING id, channel, chat_id, chat_kind, title, purpose, notes, tags, updated_by, created_at, updated_at"
            );
            client.with_client(|client| {
                let row = client.query_one(
                    &stmt,
                    &[&id, &channel, &chat_id, &chat_kind, &purpose, &notes, &tags_json, &updated_by, &now],
                )?;
                Ok(chat_profile_from_pg_row(&row))
            })
        })
        .await?
    }

    async fn get_chat_profile(&self, channel: &str, chat_id: &str) -> Result<Option<ChatProfile>> {
        let client = self.client.clone();
        let table = self.qualified_chat_profiles_table();
        let channel = channel.to_string();
        let chat_id = chat_id.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<ChatProfile>> {
            let stmt = format!(
                "SELECT id, channel, chat_id, chat_kind, title, purpose, notes, tags, updated_by, created_at, updated_at
                 FROM {table}
                 WHERE channel = $1 AND chat_id = $2"
            );
            client.with_client(|client| {
                let row = client.query_opt(&stmt, &[&channel, &chat_id])?;
                Ok(row.as_ref().map(chat_profile_from_pg_row))
            })
        })
        .await?
    }

    async fn store(&self, key: &str, content: &str, category: MemoryCategory, session_id: Option<&str>) -> Result<()> {
        self.store_with_metadata(key, content, category, session_id, MemoryStoreMetadata::default())
            .await
    }

    async fn store_with_context(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> Result<()> {
        self.store_with_context_and_metadata(
            key,
            content,
            category,
            session_id,
            context,
            MemoryStoreMetadata::default(),
        )
        .await
    }

    async fn store_with_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        metadata: MemoryStoreMetadata,
    ) -> Result<()> {
        validate_memory_write_target(key, session_id)?;
        let (embedding, embedding_provider, embedding_model, embedding_dimensions, embedding_vector) = self
            .embedding_metadata_for_category(&category, content, None, None, true)
            .await?;
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let pgvector_available = self.pgvector_available;
        let key = key.to_string();
        let content = content.to_string();
        let category = Self::category_to_str(&category);
        let sid = session_id.map(str::to_string);

        tokio::task::spawn_blocking(move || -> Result<()> {
            let now = Utc::now();
            let stmt = format!(
                "
                INSERT INTO {qualified_table}
                    (
                        id, key, content, category, created_at, updated_at, session_id,
                        workspace_id, owner_id, agent_id, persona_id, source_event_id, source,
                        embedding, embedding_provider, embedding_model, embedding_dimensions
                    )
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
                ON CONFLICT (key) DO UPDATE SET
                    content = EXCLUDED.content,
                    category = EXCLUDED.category,
                    updated_at = EXCLUDED.updated_at,
                    session_id = EXCLUDED.session_id,
                    workspace_id = EXCLUDED.workspace_id,
                    owner_id = EXCLUDED.owner_id,
                    agent_id = EXCLUDED.agent_id,
                    persona_id = EXCLUDED.persona_id,
                    source_event_id = EXCLUDED.source_event_id,
                    source = EXCLUDED.source,
                    embedding = EXCLUDED.embedding,
                    embedding_provider = EXCLUDED.embedding_provider,
                    embedding_model = EXCLUDED.embedding_model,
                    embedding_dimensions = EXCLUDED.embedding_dimensions
                "
            );

            let id = Uuid::new_v4().to_string();
            client.with_client(|client| {
                client.execute(
                    &stmt,
                    &[
                        &id,
                        &key,
                        &content,
                        &category,
                        &now,
                        &now,
                        &sid,
                        &metadata.workspace_id,
                        &metadata.owner_id,
                        &metadata.agent_id,
                        &metadata.persona_id,
                        &metadata.source_event_id,
                        &metadata.source,
                        &embedding,
                        &embedding_provider,
                        &embedding_model,
                        &embedding_dimensions,
                    ],
                )?;
                if pgvector_available {
                    let update_vector_stmt =
                        format!("UPDATE {qualified_table} SET embedding_vector = $1::text::vector WHERE key = $2");
                    let clear_vector_stmt =
                        format!("UPDATE {qualified_table} SET embedding_vector = NULL WHERE key = $1");
                    if let Some(embedding_vector) = embedding_vector.as_ref() {
                        client.execute(&update_vector_stmt, &[embedding_vector, &key])?;
                    } else {
                        client.execute(&clear_vector_stmt, &[&key])?;
                    }
                }
                Ok(())
            })?;
            Ok(())
        })
        .await?
    }

    async fn store_with_context_and_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        metadata: MemoryStoreMetadata,
    ) -> Result<()> {
        let Some(context) = context.cloned() else {
            return self
                .store_with_metadata(key, content, category, session_id, metadata)
                .await;
        };

        validate_memory_write_target(key, session_id)?;
        // V02-R9: an owner-isolated write is confined to the caller's own keys,
        // exactly as on SQLite. A blank owner id refuses before any SQL runs,
        // and the isolated row is attributed to the authenticated owner alone.
        let isolated_owner = super::owner_predicate::isolated_store_owner(Some(&context))?;
        let mut metadata = metadata;
        if let Some(owner) = isolated_owner.as_ref() {
            metadata.owner_id = Some(owner.clone());
        }
        let isolation_workspace = metadata.workspace_id.clone().or_else(|| context.workspace_id.clone());
        let isolation_owner = metadata.owner_id.clone().or_else(|| {
            context.sender_id.clone().or_else(|| {
                context
                    .channel
                    .as_ref()
                    .zip(context.raw_sender.as_ref())
                    .map(|(channel, sender)| format!("anonymous:{channel}:{sender}"))
            })
        });
        let isolation_system = isolation_owner
            .as_deref()
            .is_some_and(super::principal::is_system_principal);
        let (embedding, embedding_provider, embedding_model, embedding_dimensions, embedding_vector) = self
            .embedding_metadata_for_category(
                &category,
                content,
                isolation_workspace.as_deref(),
                isolation_owner.as_deref(),
                isolation_system,
            )
            .await?;
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let pgvector_available = self.pgvector_available;
        let key = key.to_string();
        let content = content.to_string();
        let category = Self::category_to_str(&category);
        let sid = session_id.map(str::to_string);
        let rls_owner_isolated = isolated_owner.is_some();
        let owner_guard =
            super::owner_predicate::store_conflict_guard(PG_DIALECT, &qualified_table, isolated_owner.is_some());

        tokio::task::spawn_blocking(move || -> Result<()> {
            let now = Utc::now();
            let principal = Self::principal_from_context(&context);
            let classified = classify_memory(&context, &content, &principal);
            let risk_json = serde_json::to_string(&classified.risk_signals)?;
            let chat_type = context
                .chat_type
                .as_deref()
                .map(ChatType::from_str)
                .map(|chat_type| chat_type.as_str().to_string());
            let sender_id = context.sender_id.clone().or_else(|| {
                if context.channel.is_some() && context.raw_sender.is_some() {
                    Some(principal.user_id.clone())
                } else {
                    None
                }
            });
            let owner_id = metadata.owner_id.clone().or_else(|| sender_id.clone());
            let workspace_id = metadata.workspace_id.clone().or_else(|| context.workspace_id.clone());
            let rls_system = super::principal::is_system_principal(&principal.user_id);
            // FIX-P0-23 (#4 F4): persist topic_id instead of hard-coded NULL.
            // Prefer the explicit metadata topic_id; fall back to source_event_id
            // (source_event_id as the fallback) so a write still threads back to its
            // originating event for Project-visibility scope resolution.
            let topic_id = metadata.topic_id.clone().or_else(|| metadata.source_event_id.clone());
            let stmt = format!(
                "
                INSERT INTO {qualified_table}
                    (
                        id, key, content, category, created_at, updated_at, session_id,
                        workspace_id, owner_id, agent_id, persona_id, source_event_id, source,
                        embedding, embedding_provider, embedding_model, embedding_dimensions,
                        channel, chat_type, chat_id, sender_id, raw_sender, topic_id,
                        visibility, sensitivity, risk_signals, policy_version
                    )
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                     $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27)
                ON CONFLICT (key) DO UPDATE SET
                    content = EXCLUDED.content,
                    category = EXCLUDED.category,
                    updated_at = EXCLUDED.updated_at,
                    session_id = EXCLUDED.session_id,
                    workspace_id = EXCLUDED.workspace_id,
                    owner_id = EXCLUDED.owner_id,
                    agent_id = EXCLUDED.agent_id,
                    persona_id = EXCLUDED.persona_id,
                    source_event_id = EXCLUDED.source_event_id,
                    source = EXCLUDED.source,
                    embedding = EXCLUDED.embedding,
                    embedding_provider = EXCLUDED.embedding_provider,
                    embedding_model = EXCLUDED.embedding_model,
                    embedding_dimensions = EXCLUDED.embedding_dimensions,
                    channel = EXCLUDED.channel,
                    chat_type = EXCLUDED.chat_type,
                    chat_id = EXCLUDED.chat_id,
                    sender_id = EXCLUDED.sender_id,
                    raw_sender = EXCLUDED.raw_sender,
                    topic_id = EXCLUDED.topic_id,
                    visibility = EXCLUDED.visibility,
                    sensitivity = EXCLUDED.sensitivity,
                    risk_signals = EXCLUDED.risk_signals,
                    policy_version = EXCLUDED.policy_version{owner_guard}
                "
            );

            let id = Uuid::new_v4().to_string();
            let refused = || -> anyhow::Error {
                super::owner_predicate::OwnerScopedStoreRefusal::KeyNotAvailable { key: key.clone() }.into()
            };
            client.with_client(|client| {
                let mut tx = client.transaction()?;
                Self::apply_rls_context_raw(
                    &mut tx,
                    rls_system,
                    workspace_id.as_deref(),
                    owner_id.as_deref(),
                    rls_owner_isolated,
                )?;
                let written = tx.execute(
                    &stmt,
                    &[
                        &id,
                        &key,
                        &content,
                        &category,
                        &now,
                        &now,
                        &sid,
                        &workspace_id,
                        &owner_id,
                        &metadata.agent_id,
                        &metadata.persona_id,
                        &metadata.source_event_id,
                        &metadata.source,
                        &embedding,
                        &embedding_provider,
                        &embedding_model,
                        &embedding_dimensions,
                        &context.channel,
                        &chat_type,
                        &context.chat_id,
                        &sender_id,
                        &context.raw_sender,
                        &topic_id,
                        &classified.visibility.as_str(),
                        &classified.sensitivity.as_str(),
                        &risk_json,
                        &classified.policy_version,
                    ],
                );
                // An isolated write that meets another owner's key ends in one
                // of two ways, and both are the same refusal: the conflict
                // guard suppresses the update (zero rows), or the
                // `memories_owner_isolation` policy rejects the existing row
                // outright (`42501`) — which of the two PostgreSQL reports for
                // a non-shared row is pinned by
                // `postgres_owner_isolated_store_refuses_a_cross_owner_upsert`.
                let written = match written {
                    Ok(rows) => rows,
                    Err(error)
                        if isolated_owner.is_some()
                            && error.code() == Some(&postgres::error::SqlState::INSUFFICIENT_PRIVILEGE) =>
                    {
                        return Err(refused());
                    }
                    Err(error) => return Err(error.into()),
                };
                if isolated_owner.is_some() && written == 0 {
                    return Err(refused());
                }
                if pgvector_available {
                    let update_vector_stmt = format!(
                        "UPDATE {qualified_table} SET embedding_vector = $1::text::vector \
                         WHERE key = $2 AND workspace_id = $3 AND owner_id = $4"
                    );
                    let clear_vector_stmt = format!(
                        "UPDATE {qualified_table} SET embedding_vector = NULL \
                         WHERE key = $1 AND workspace_id = $2 AND owner_id = $3"
                    );
                    if let Some(embedding_vector) = embedding_vector.as_ref() {
                        tx.execute(&update_vector_stmt, &[embedding_vector, &key, &workspace_id, &owner_id])?;
                    } else {
                        tx.execute(&clear_vector_stmt, &[&key, &workspace_id, &owner_id])?;
                    }
                }
                tx.commit()?;
                Ok(())
            })?;
            Ok(())
        })
        .await?
    }

    async fn recall(&self, query: &str, limit: usize, session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let query = query.trim().to_string();
        let sid = session_id.map(str::to_string);
        let query_embedding = if query.is_empty() {
            None
        } else {
            self.get_or_compute_embedding(&query, None, None, true).await?
        };
        let embedding_provider = self.embedding_provider_name();
        let embedding_model = self.embedding_model_name();
        let embedding_dimensions = self.embedder.dimensions();
        let embedding_dimensions_i64 = self.embedding_dimensions_i64();
        let vector_weight = f64::from(self.vector_weight);
        let keyword_weight = f64::from(self.keyword_weight);
        let pgvector_available = self.pgvector_available;
        let query_vector = query_embedding
            .as_ref()
            .and_then(|embedding| Self::pgvector_literal(embedding));

        tokio::task::spawn_blocking(move || -> Result<Vec<MemoryEntry>> {
            let stmt = format!(
                "
                SELECT id, key, content, category, created_at, session_id,
                       (
                         CASE WHEN key ILIKE '%' || $1 || '%' THEN 2.0 ELSE 0.0 END +
                         CASE WHEN content ILIKE '%' || $1 || '%' THEN 1.0 ELSE 0.0 END
                       ) AS score,
                       useful_count
                FROM {qualified_table}
                WHERE ($2::TEXT IS NULL OR session_id = $2)
                  AND ($1 = '' OR key ILIKE '%' || $1 || '%' OR content ILIKE '%' || $1 || '%')
                ORDER BY score DESC, updated_at DESC
                LIMIT $3
                "
            );

            #[allow(clippy::cast_possible_wrap)]
            let limit_i64 = limit as i64;

            let rows = client.with_client(|client| Ok(client.query(&stmt, &[&query, &sid, &limit_i64])?))?;
            let mut by_id = std::collections::HashMap::<String, MemoryEntry>::new();
            for row in &rows {
                let mut entry = Self::row_to_entry(row)?;
                entry.score = entry.score.map(|score| score * keyword_weight);
                by_id.insert(entry.id.clone(), entry);
            }

            if let Some(query_embedding) = query_embedding {
                if pgvector_available {
                    if let Some(query_vector) = query_vector.as_ref() {
                        let vector_stmt = format!(
                            "
                            SELECT id, key, content, category, created_at, session_id,
                                   (1.0 - (embedding_vector <=> $1::text::vector))::DOUBLE PRECISION AS score,
                                   useful_count
                            FROM {qualified_table}
                            WHERE embedding_vector IS NOT NULL
                              AND embedding_provider = $2
                              AND embedding_model = $3
                              AND embedding_dimensions = $4
                              AND ($5::TEXT IS NULL OR session_id = $5)
                            ORDER BY embedding_vector <=> $1::text::vector
                            LIMIT $6
                            "
                        );
                        let candidate_limit = limit_i64.saturating_mul(4).max(limit_i64);
                        let vector_rows = client.with_client(|client| {
                            Ok(client.query(
                                &vector_stmt,
                                &[
                                    query_vector,
                                    &embedding_provider,
                                    &embedding_model,
                                    &embedding_dimensions_i64,
                                    &sid,
                                    &candidate_limit,
                                ],
                            )?)
                        })?;
                        for row in &vector_rows {
                            let score: f64 = row.try_get(6).unwrap_or(0.0);
                            let score = score * vector_weight;
                            if score <= 0.0 {
                                continue;
                            }
                            let mut entry = Self::row_to_entry(row)?;
                            entry.score = Some(score);
                            by_id
                                .entry(entry.id.clone())
                                .and_modify(|existing| {
                                    let combined = existing.score.unwrap_or(0.0) + score;
                                    existing.score = Some(combined);
                                })
                                .or_insert(entry);
                        }
                    }
                } else {
                    // FIX-P2-03: BYTEA fallback (no pgvector). SQL *cannot* order
                    // by vector distance here, so we fetch a bounded candidate set
                    // and re-rank it in-process by cosine similarity below. The
                    // LIMIT is a pure safety valve against an unbounded full-table
                    // scan (DoS), NOT a relevance filter — it is deliberately set far
                    // larger than the pgvector path's `4 * limit` so the in-process
                    // re-rank still sees enough genuinely-similar rows and we do not
                    // truncate "older but more similar" records on recency. The cap
                    // applies *after* the scope/owner/session WHERE filter. Ordering
                    // by id keeps the (cap-only) truncation deterministic without
                    // masquerading recency as similarity. This is the documented
                    // degradation tradeoff of running without pgvector.
                    let candidate_limit = POSTGRES_BYTEA_FALLBACK_CANDIDATE_CAP;
                    let vector_stmt = format!(
                        "
                        SELECT id, key, content, category, created_at, session_id,
                               NULL::DOUBLE PRECISION AS score,
                               useful_count,
                               embedding
                        FROM {qualified_table}
                        WHERE embedding IS NOT NULL
                          AND embedding_provider = $1
                          AND embedding_model = $2
                          AND embedding_dimensions = $3
                          AND ($4::TEXT IS NULL OR session_id = $4)
                        ORDER BY id
                        LIMIT $5
                        "
                    );
                    let vector_rows = client.with_client(|client| {
                        Ok(client.query(
                            &vector_stmt,
                            &[
                                &embedding_provider,
                                &embedding_model,
                                &embedding_dimensions_i64,
                                &sid,
                                &candidate_limit,
                            ],
                        )?)
                    })?;
                    for row in &vector_rows {
                        let embedding_blob: Vec<u8> = row.get(8);
                        let embedding = vector::bytes_to_vec(&embedding_blob);
                        if embedding.len() != embedding_dimensions {
                            tracing::debug!(
                                memory_id = %row.get::<_, String>(0),
                                expected_dimensions = embedding_dimensions,
                                actual_dimensions = embedding.len(),
                                "Skipping stale Postgres memory embedding with mismatched dimensions"
                            );
                            continue;
                        }
                        let score = f64::from(vector::cosine_similarity(&query_embedding, &embedding)) * vector_weight;
                        if score <= 0.0 {
                            continue;
                        }
                        let mut entry = Self::row_to_entry(row)?;
                        entry.score = Some(score);
                        by_id
                            .entry(entry.id.clone())
                            .and_modify(|existing| {
                                let combined = existing.score.unwrap_or(0.0) + score;
                                existing.score = Some(combined);
                            })
                            .or_insert(entry);
                    }
                }
            }

            let mut entries: Vec<MemoryEntry> = by_id.into_values().collect();
            entries.sort_by(|a, b| {
                let left = a.score.unwrap_or(0.0);
                let right = b.score.unwrap_or(0.0);
                right.partial_cmp(&left).unwrap_or(std::cmp::Ordering::Equal)
            });
            entries.truncate(limit.max(1));
            Ok(entries)
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
        deadline: Instant,
    ) -> Result<super::traits::RouterLearningRead> {
        self.router_learning_transaction(evidence, None, deadline)
            .await
            .map(|(read, _)| read)
    }
    async fn commit_router_learning(
        &self,
        evidence: &super::traits::RouterLearningEvidence,
        expected_version: i64,
        next: &super::traits::RouterLearningSnapshot,
        deadline: Instant,
    ) -> Result<super::traits::RouterLearningCommit> {
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
        deadline: Instant,
    ) -> Result<u32> {
        self.count_decision_rehearsal(query, limit, context, deadline).await
    }

    async fn retrieve_decision_memory(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        session_id: Option<&str>,
        deadline: Instant,
    ) -> Result<Vec<super::traits::DecisionMemoryHit>> {
        self.read_decision_memory(query, limit, context, session_id, deadline)
            .await
    }

    async fn retrieve_decision_documents(
        &self,
        query: &str,
        limit: u8,
        principal: &MemoryPrincipal,
        deadline: Instant,
    ) -> Result<Vec<super::traits::DecisionDocumentHit>> {
        self.read_decision_documents(query, limit, principal, deadline).await
    }

    async fn retrieve_decision_session(
        &self,
        principal: &MemoryPrincipal,
        before_event_id: &str,
        limit: u8,
        deadline: Instant,
    ) -> Result<Vec<super::traits::DecisionSessionHit>> {
        self.read_decision_session(principal, before_event_id, limit, deadline)
            .await
    }

    async fn recall_with_context(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> Result<Vec<MemoryEntry>> {
        let Some(context) = context.cloned() else {
            return self.recall(query, limit, session_id).await;
        };
        // C03 / V02-R5 (F1): an owner-isolated caller with no usable owner id
        // reaches nothing owned. Decide that here, before the RLS context is
        // applied, so the answer is the empty set the SQLite adapter returns
        // rather than the "requires a trusted owner context" error
        // `apply_rls_context_raw` would raise for a missing owner.
        if Self::isolated_read_without_owner(&context) {
            tracing::warn!("owner-isolated memory recall carries no owner id; returning no rows");
            return Ok(Vec::new());
        }
        let rls_workspace = context.workspace_id.clone().unwrap_or_default();

        // FIX-P0-22 (#3 F3): resolve a full ACL principal (identity_bindings +
        // user_policies) so blocked_patterns and role are populated, then apply
        // the post_filter after the SQL visibility scope (parity with SQLite).
        let principal = self.resolve_principal_from_context(&context).await;

        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let query = query.trim().to_string();
        let sid = session_id.map(str::to_string);
        // D11: the visibility scope is now derived from the resolved `principal`
        // (shared predicate), so the raw context triple is no longer bound into
        // the recall SQL. `sender_id` is still needed for the RLS owner context.
        let sender_id = context.sender_id.clone().or_else(|| {
            if context.channel.is_some() && context.raw_sender.is_some() {
                Some(format!(
                    "anonymous:{}:{}",
                    context.channel.as_deref().unwrap_or("unknown"),
                    context.raw_sender.as_deref().unwrap_or("unknown")
                ))
            } else {
                None
            }
        });
        // Over-fetch so the post_filter has candidates to remove without
        // starving the requested limit.
        let fetch_limit = limit.saturating_mul(3).max(limit);

        // FIX-P3-04: capture the RLS context so the DB-layer owner policy is set
        // before the read (defense-in-depth behind the SQL visibility scope).
        // FIX-P3-04: the RLS owner mirrors the row `owner_id`, which writes set to
        // the effective `sender_id` (see store_with_context_and_metadata). System
        // principals (resolved via the ACL user_id) get the policy bypass.
        let rls_system = super::principal::is_system_principal(&principal.user_id);
        // C03 / V02-R5: for an owner-isolated caller the RLS owner is the
        // authenticated `owner_id` itself, so the row-level policy and the SQL
        // predicate gate on the same string by construction rather than by the
        // coincidence that these tools also copy the owner into `sender_id`.
        let rls_owner_isolated = context.require_owner_match;
        let rls_owner = if rls_owner_isolated {
            context.owner_id.as_deref()
        } else {
            sender_id.as_deref()
        }
        .filter(|owner| !owner.trim().is_empty())
        .map(str::to_string);

        // D11: render the ACL visibility scope from the shared, dialect-agnostic
        // predicate (single source with SQLite). The fixed parameters are
        // $1=query, $2=session_id, $3=limit, so the scope predicate is rendered
        // with `$N` starting at $4 and its values appended after them. This
        // tightens the Postgres scope to the SQLite canonical truth table,
        // removing the previously over-permissive `visibility IS NULL` /
        // `visibility IN ('workspace', ...)` allow paths.
        let (scope_sql, scope_params) = principal.build_sql_scope_pg(4);
        let scope_values = scope_params_to_strings(scope_params);

        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<MemoryEntry>> {
            let stmt = format!(
                "
                SELECT id, key, content, category, created_at, session_id,
                       (
                         CASE WHEN key ILIKE '%' || $1 || '%' THEN 2.0 ELSE 0.0 END +
                         CASE WHEN content ILIKE '%' || $1 || '%' THEN 1.0 ELSE 0.0 END
                       ) AS score,
                       useful_count
                FROM {qualified_table}
                WHERE ($2::TEXT IS NULL OR session_id = $2)
                  AND ($1 = '' OR key ILIKE '%' || $1 || '%' OR content ILIKE '%' || $1 || '%')
                  AND ({scope_sql})
                ORDER BY score DESC, updated_at DESC
                LIMIT $3
                "
            );

            #[allow(clippy::cast_possible_wrap)]
            let limit_i64 = fetch_limit.max(1) as i64;
            let rows = client.with_client(|client| {
                let mut tx = client.transaction()?;
                Self::apply_rls_context_raw(
                    &mut tx,
                    rls_system,
                    Some(&rls_workspace),
                    rls_owner.as_deref(),
                    rls_owner_isolated,
                )?;
                let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&query, &sid, &limit_i64];
                for value in &scope_values {
                    params.push(value);
                }
                let rows = tx.query(&stmt, &params)?;
                tx.commit()?;
                Ok(rows)
            })?;
            rows.iter()
                .map(Self::row_to_entry)
                .collect::<Result<Vec<MemoryEntry>>>()
        })
        .await??;

        let mut visible = super::principal::post_filter(rows, &principal, |entry| entry.content.as_str());
        visible.truncate(limit.max(1));
        Ok(visible)
    }

    async fn recall_with_context_mode(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> Result<Vec<MemoryEntry>> {
        let context = context.cloned().unwrap_or_default();
        // C03 / V02-R5 defence in depth: an owner-isolated context is enforced
        // whatever mode was requested (parity with the SQLite adapter).
        let mode = if context.require_owner_match {
            MemoryReadMode::Enforce
        } else {
            mode
        };
        let scoped = self
            .recall_with_context(query, limit, session_id, Some(&context))
            .await?;
        let unrestricted = self.recall(query, limit, session_id).await?;
        let would_deny = unrestricted
            .iter()
            .any(|entry| !scoped.iter().any(|allowed| allowed.id == entry.id));
        let selected = match mode {
            MemoryReadMode::Enforce => scoped,
            MemoryReadMode::Observe => unrestricted,
        };
        let principal = self.resolve_principal_from_context(&context).await;
        Self::log_access_best_effort(
            Arc::clone(&self.client),
            self.qualified_access_audit_log_table(),
            principal,
            "search",
            Some(query.to_string()),
            None,
            Some(match mode {
                MemoryReadMode::Enforce => "acl_enforced",
                MemoryReadMode::Observe => "observe_mode",
            }),
            if mode == MemoryReadMode::Observe && would_deny {
                "would_deny"
            } else if selected.is_empty() {
                "no_results"
            } else {
                "allowed"
            },
        )
        .await;
        Ok(selected)
    }

    async fn get(&self, key: &str) -> Result<Option<MemoryEntry>> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let key = key.to_string();

        tokio::task::spawn_blocking(move || -> Result<Option<MemoryEntry>> {
            let stmt = format!(
                "
                SELECT id, key, content, category, created_at, session_id,
                       NULL::DOUBLE PRECISION AS score,
                       useful_count
                FROM {qualified_table}
                WHERE key = $1
                LIMIT 1
                "
            );

            let row = client.with_client(|client| Ok(client.query_opt(&stmt, &[&key])?))?;
            row.as_ref().map(Self::row_to_entry).transpose()
        })
        .await?
    }

    async fn get_with_context(&self, key: &str, context: Option<&MemoryWriteContext>) -> Result<Option<MemoryEntry>> {
        let context = context.cloned().unwrap_or_default();
        // C03 / V02-R5 (F1): see `recall_with_context` — the keyed read answers
        // "no entry", not an error, when an isolated caller has no owner id.
        if Self::isolated_read_without_owner(&context) {
            tracing::warn!("owner-isolated memory keyed read carries no owner id; returning no entry");
            return Ok(None);
        }
        let rls_workspace = context.workspace_id.clone().unwrap_or_default();
        let principal = self.resolve_principal_from_context(&context).await;
        let sender_id = context.sender_id.clone().or_else(|| {
            if context.channel.is_some() && context.raw_sender.is_some() {
                Some(format!(
                    "anonymous:{}:{}",
                    context.channel.as_deref().unwrap_or("unknown"),
                    context.raw_sender.as_deref().unwrap_or("unknown")
                ))
            } else {
                None
            }
        });
        let rls_system = super::principal::is_system_principal(&principal.user_id);
        // C03 / V02-R5: see `recall_with_context` — an isolated caller's RLS
        // owner is its authenticated `owner_id`.
        let rls_owner_isolated = context.require_owner_match;
        let rls_owner = if rls_owner_isolated {
            context.owner_id.as_deref()
        } else {
            sender_id.as_deref()
        }
        .filter(|owner| !owner.trim().is_empty())
        .map(str::to_string);
        let (scope_sql, scope_params) = principal.build_sql_scope_pg(2);
        let scope_values = scope_params_to_strings(scope_params);
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let key = key.to_string();
        let principal_for_filter = principal.clone();

        tokio::task::spawn_blocking(move || -> Result<Option<MemoryEntry>> {
            let stmt = format!(
                "SELECT id, key, content, category, created_at, session_id,
                        NULL::DOUBLE PRECISION AS score, useful_count
                   FROM {qualified_table}
                  WHERE key = $1 AND ({scope_sql})
                  LIMIT 1"
            );
            let row = client.with_client(|client| {
                let mut tx = client.transaction()?;
                Self::apply_rls_context_raw(
                    &mut tx,
                    rls_system,
                    Some(&rls_workspace),
                    rls_owner.as_deref(),
                    rls_owner_isolated,
                )?;
                let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&key];
                for value in &scope_values {
                    params.push(value);
                }
                let row = tx.query_opt(&stmt, &params)?;
                tx.commit()?;
                Ok(row)
            })?;
            let entry = row.as_ref().map(Self::row_to_entry).transpose()?;
            let mut visible =
                super::principal::post_filter(entry.into_iter().collect(), &principal_for_filter, |entry| {
                    entry.content.as_str()
                });
            Ok(visible.pop())
        })
        .await?
    }

    async fn get_with_context_mode(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> Result<Option<MemoryEntry>> {
        let context = context.cloned().unwrap_or_default();
        // C03 / V02-R5: isolation outranks the requested mode.
        let mode = if context.require_owner_match {
            MemoryReadMode::Enforce
        } else {
            mode
        };
        let scoped = self.get_with_context(key, Some(&context)).await?;
        let unrestricted = self.get(key).await?;
        let would_deny = unrestricted.is_some() && scoped.is_none();
        let selected = match mode {
            MemoryReadMode::Enforce => scoped,
            MemoryReadMode::Observe => unrestricted,
        };
        let principal = self.resolve_principal_from_context(&context).await;
        Self::log_access_best_effort(
            Arc::clone(&self.client),
            self.qualified_access_audit_log_table(),
            principal,
            "get",
            None,
            selected
                .as_ref()
                .map(|entry| entry.id.clone())
                .or_else(|| Some(key.to_string())),
            Some(match mode {
                MemoryReadMode::Enforce => "acl_enforced",
                MemoryReadMode::Observe => "observe_mode",
            }),
            if mode == MemoryReadMode::Observe && would_deny {
                "would_deny"
            } else if selected.is_some() {
                "allowed"
            } else {
                "denied"
            },
        )
        .await;
        Ok(selected)
    }

    async fn list(&self, category: Option<&MemoryCategory>, session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let category = category.map(Self::category_to_str);
        let sid = session_id.map(str::to_string);

        tokio::task::spawn_blocking(move || -> Result<Vec<MemoryEntry>> {
            let stmt = format!(
                "
                SELECT id, key, content, category, created_at, session_id,
                       NULL::DOUBLE PRECISION AS score,
                       useful_count
                FROM {qualified_table}
                WHERE ($1::TEXT IS NULL OR category = $1)
                  AND ($2::TEXT IS NULL OR session_id = $2)
                ORDER BY updated_at DESC
                "
            );

            let category_ref = category.as_deref();
            let session_ref = sid.as_deref();
            let rows = client.with_client(|client| Ok(client.query(&stmt, &[&category_ref, &session_ref])?))?;
            rows.iter()
                .map(Self::row_to_entry)
                .collect::<Result<Vec<MemoryEntry>>>()
        })
        .await?
    }

    async fn list_owner_ids(
        &self,
        category: &MemoryCategory,
    ) -> Result<Option<std::collections::HashMap<String, Option<String>>>> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let category = Self::category_to_str(category);

        tokio::task::spawn_blocking(
            move || -> Result<Option<std::collections::HashMap<String, Option<String>>>> {
                let stmt = format!("SELECT id, owner_id FROM {qualified_table} WHERE category = $1");
                let rows = client.with_client(|client| Ok(client.query(&stmt, &[&category])?))?;
                Ok(Some(
                    rows.iter()
                        .map(|row| (row.get::<_, String>(0), row.get::<_, Option<String>>(1)))
                        .collect(),
                ))
            },
        )
        .await?
    }

    async fn forget(&self, key: &str) -> Result<bool> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let key = key.to_string();

        tokio::task::spawn_blocking(move || -> Result<bool> {
            let stmt = format!("DELETE FROM {qualified_table} WHERE key = $1");
            let deleted = client.with_client(|client| Ok(client.execute(&stmt, &[&key])?))?;
            Ok(deleted > 0)
        })
        .await?
    }

    async fn forget_with_context(&self, key: &str, context: Option<&MemoryWriteContext>) -> Result<bool> {
        let Some(context) = context.cloned() else {
            return self.forget(key).await;
        };
        // R-B1d D-1: an owner-isolated caller with no usable owner id deletes
        // nothing. Decide that here, before the RLS context is applied — and
        // before any statement runs — so the answer is the "nothing forgotten"
        // the SQLite adapter gives rather than the "requires a trusted owner
        // context" error `apply_rls_context_raw` would raise for a missing
        // owner. Log line: operation only, no key and no content.
        if Self::isolated_read_without_owner(&context) {
            tracing::warn!("owner-isolated memory forget carries no owner id; removing nothing");
            return Ok(false);
        }
        let rls_workspace = context.workspace_id.clone().unwrap_or_default();

        // FIX-P0-22 (#3 F3): resolve the ACL principal and record an access-audit
        // entry for both allowed and denied deletes (parity with SQLite).
        let principal = self.resolve_principal_from_context(&context).await;
        let rls_system = super::principal::is_system_principal(&principal.user_id);
        // C03 / V02-R5 / R-B1d D-1: for an owner-isolated caller the RLS owner
        // is the authenticated `owner_id` itself, so the row-level policy and
        // the SQL predicate gate on the same string by construction. The
        // non-isolated expression is the historical one, unchanged.
        let rls_owner_isolated = context.require_owner_match;
        let rls_owner = if rls_owner_isolated {
            Self::forget_owner_anchor(&context)
        } else {
            context.sender_id.clone().or_else(|| {
                context
                    .channel
                    .as_ref()
                    .zip(context.raw_sender.as_ref())
                    .map(|(channel, sender)| format!("anonymous:{channel}:{sender}"))
            })
        };

        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let audit_table = self.qualified_access_audit_log_table();
        let key = key.to_string();

        // D11: derive the delete-visibility scope from the shared predicate
        // (single source with SQLite). The fixed parameter is $1=key, so the
        // scope predicate is rendered with `$N` starting at $2 and its values
        // appended after the key. This tightens the Postgres forget scope to the
        // SQLite canonical truth table (removing the over-permissive
        // `visibility IS NULL` / `visibility IN ('workspace', ...)` allow paths)
        // and folds the secret-exclusion into the shared predicate, so an owner
        // can delete its own secrets exactly as on SQLite.
        let (scope_sql, scope_params) = principal.build_sql_scope_pg(2);
        let scope_values = scope_params_to_strings(scope_params);
        // R-B1d D-1: the delete guard, at the next free placeholder after the
        // key and the scope values. See `Self::forget_owner_guard`.
        let owner_guard = Self::forget_owner_guard(&context, scope_values.len() + 2);
        let owner_value = owner_guard
            .bound_owner
            .then(|| Self::forget_owner_anchor(&context))
            .flatten();
        let owner_guard_sql = owner_guard.sql;

        tokio::task::spawn_blocking(move || -> Result<bool> {
            let stmt = format!(
                "
                DELETE FROM {qualified_table}
                WHERE key = $1
                  AND ({scope_sql}){owner_guard_sql}
                "
            );
            let deleted = client.with_client(|client| {
                let mut tx = client.transaction()?;
                Self::apply_rls_context_raw(
                    &mut tx,
                    rls_system,
                    Some(&rls_workspace),
                    rls_owner.as_deref(),
                    rls_owner_isolated,
                )?;
                let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&key];
                for value in &scope_values {
                    params.push(value);
                }
                if let Some(owner) = &owner_value {
                    params.push(owner);
                }
                let deleted = tx.execute(&stmt, &params)?;
                tx.commit()?;
                Ok(deleted)
            })?;
            let result = if deleted > 0 { "allowed" } else { "denied" };
            Self::log_access_best_effort_blocking(
                &client,
                &audit_table,
                &principal,
                "forget",
                Some(&key),
                None,
                None,
                result,
            );
            Ok(deleted > 0)
        })
        .await?
    }

    async fn count(&self) -> Result<usize> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();

        tokio::task::spawn_blocking(move || -> Result<usize> {
            let stmt = format!("SELECT COUNT(*) FROM {qualified_table}");
            let count: i64 = client.with_client(|client| Ok(client.query_one(&stmt, &[])?.get(0)))?;
            let count = usize::try_from(count).context("PostgreSQL returned a negative memory count")?;
            Ok(count)
        })
        .await?
    }

    // FIX-P0-19/20 (C5b #2): conversation persistence with owner ACL, mirrored
    // from the SQLite backend so switching to Postgres preserves the same
    // isolation guarantees. Non-system principals only see their own owner_id
    // (legacy NULL / `legacy:<session_key>` rows remain visible during the
    // phased rollout, matching SQLite's `legacy_visible = 1` default).
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
    ) -> Result<()> {
        let client = self.client.clone();
        let sessions_table = self.qualified_sessions_table();
        let turns_table = self.qualified_conversation_turns_table();
        let session_key = session_key.to_string();
        let channel = channel.to_string();
        let sender = sender.to_string();
        let role = role.to_string();
        let content = content.to_string();
        let timestamp = timestamp.map(str::to_string).unwrap_or_else(|| Utc::now().to_rfc3339());
        let message_id = message_id.map(str::to_string);
        let owner_id = owner_id
            .map(str::to_string)
            .or_else(|| Some(format!("legacy:{session_key}")));
        let preview: String = content.chars().take(200).collect();

        tokio::task::spawn_blocking(move || -> Result<()> {
            client.with_client(|client| {
                let mut tx = client.transaction()?;
                let session_sql = format!(
                    "INSERT INTO {sessions_table} (
                         session_key, channel, sender, owner_id,
                         created_at, updated_at, message_count, last_message_preview
                     ) VALUES ($1, $2, $3, $4, $5, $5, 1, $6)
                     ON CONFLICT (session_key) DO UPDATE SET
                         channel = EXCLUDED.channel,
                         sender = EXCLUDED.sender,
                         owner_id = COALESCE(EXCLUDED.owner_id, {sessions_table}.owner_id),
                         updated_at = EXCLUDED.updated_at,
                         message_count = {sessions_table}.message_count + 1,
                         last_message_preview = EXCLUDED.last_message_preview"
                );
                tx.execute(
                    &session_sql,
                    &[&session_key, &channel, &sender, &owner_id, &timestamp, &preview],
                )?;

                let turn_sql = format!(
                    "INSERT INTO {turns_table} (session_key, owner_id, role, content, timestamp, message_id)
                     VALUES ($1, $2, $3, $4, $5, $6)"
                );
                tx.execute(
                    &turn_sql,
                    &[&session_key, &owner_id, &role, &content, &timestamp, &message_id],
                )?;
                tx.commit()?;
                Ok(())
            })
        })
        .await?
    }

    async fn project_console_message(&self, event: &MessageEvent) -> Result<()> {
        event.validate_console_projection()?;
        let event = event.clone();
        let client = self.client.clone();
        let events_table = self.qualified_message_events_table.clone();
        let sessions_table = self.qualified_sessions_table();
        let turns_table = self.qualified_conversation_turns_table();
        tokio::task::spawn_blocking(move || -> Result<()> {
            client.with_client(|client| {
                let mut tx = client.transaction()?;
                // The event row lock serializes competing retries of this exact
                // projection across backend instances, before checking history.
                let row = tx.query_one(&format!(
                    "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                            parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                            sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                            goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                            content, content_hash, raw_payload_json, visibility, created_at, updated_at
                     FROM {events_table} WHERE event_id = $1 AND workspace_id = $2 AND owner_id = $3 FOR UPDATE"),
                    &[&event.event_id, &event.workspace_id, &event.owner_id])?;
                let durable = Self::row_to_message_event(&row)?;
                anyhow::ensure!(serde_json::to_value(&durable)? == serde_json::to_value(&event)?,
                    "console projection differs from its durable event");
                let existing = tx.query(&format!("SELECT session_key, owner_id, role, content FROM {turns_table}
                    WHERE message_event_id = $1 OR message_id = $1 LIMIT 2"), &[&event.event_id])?;
                if let Some(row) = existing.first() {
                    anyhow::ensure!(existing.len() == 1
                        && Some(row.get::<_, String>(0)) == event.session_key
                        && row.get::<_, Option<String>>(1) == event.owner_id
                        && row.get::<_, String>(2) == event.role && row.get::<_, String>(3) == event.content,
                        "console message projection conflicts with its durable event");
                    return Ok(());
                }
                let preview: String = event.content.chars().take(200).collect();
                tx.execute(&format!("INSERT INTO {sessions_table} AS current_session
                    (session_key, channel, sender, owner_id, created_at, updated_at, message_count, last_message_preview)
                    VALUES ($1, $2, $3, $4, $5, $5, 1, $6)
                    ON CONFLICT(session_key) DO UPDATE SET
                        message_count = COALESCE(current_session.message_count, 0) + 1,
                        channel = CASE WHEN EXCLUDED.updated_at >= current_session.updated_at THEN EXCLUDED.channel ELSE current_session.channel END,
                        sender = CASE WHEN EXCLUDED.updated_at >= current_session.updated_at THEN EXCLUDED.sender ELSE current_session.sender END,
                        owner_id = CASE WHEN EXCLUDED.updated_at >= current_session.updated_at THEN EXCLUDED.owner_id ELSE current_session.owner_id END,
                        last_message_preview = CASE WHEN EXCLUDED.updated_at >= current_session.updated_at THEN EXCLUDED.last_message_preview ELSE current_session.last_message_preview END,
                        updated_at = GREATEST(current_session.updated_at, EXCLUDED.updated_at)"),
                    &[&event.session_key, &event.channel, &event.sender, &event.owner_id, &event.created_at, &preview])?;
                tx.execute(&format!("INSERT INTO {turns_table}
                    (session_key, owner_id, role, content, timestamp, message_id, message_event_id, agent_id, persona_id, visibility)
                    VALUES ($1, $2, $3, $4, $5, $6, $6, $7, $8, 'session')"),
                    &[&event.session_key, &event.owner_id, &event.role, &event.content, &event.created_at,
                        &event.event_id, &event.agent_id, &event.persona_id])?;
                tx.commit()?;
                Ok(())
            })
        }).await?
    }

    async fn list_console_terminal_events(
        &self,
        workspace_id: &str,
        session_key: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<MessageEvent>> {
        anyhow::ensure!(
            !workspace_id.is_empty() && !session_key.is_empty(),
            "console recovery scope is empty"
        );
        let workspace_id = workspace_id.to_owned();
        let session_key = session_key.to_owned();
        let limit = i64::try_from(limit.clamp(1, 128))?;
        let client = self.client.clone();
        let table = self.qualified_message_events_table.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<MessageEvent>> {
            client.with_client(|client| {
                let rows = client.query(&format!("SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                    FROM {table} WHERE workspace_id = $1 AND session_key = $2
                    AND source = 'console' AND event_type = 'turn.finalized' AND id > $3
                    ORDER BY id ASC LIMIT $4"), &[&workspace_id, &session_key, &after_id, &limit])?;
                rows.iter().map(Self::row_to_message_event).collect()
            })
        }).await?
    }

    async fn list_conversation_turns(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationTurn>> {
        self.list_conversation_turns_ordered(principal, session_key, limit, offset, false)
            .await
    }

    async fn list_console_conversation_turns(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationTurn>> {
        self.list_conversation_turns_ordered(principal, session_key, limit, offset, true)
            .await
    }
    async fn load_recent_conversation_histories(
        &self,
        principal: &MemoryPrincipal,
        max_turns_per_session: usize,
        max_sessions: usize,
    ) -> Result<std::collections::HashMap<String, Vec<ConversationTurn>>> {
        let client = self.client.clone();
        let sessions_table = self.qualified_sessions_table();
        let turns_table = self.qualified_conversation_turns_table();
        let owner_id = principal
            .owner_id
            .as_deref()
            .filter(|owner| !owner.trim().is_empty())
            .map(str::to_string);
        #[allow(clippy::cast_possible_wrap)]
        let max_turns = max_turns_per_session.clamp(1, 500) as i64;
        #[allow(clippy::cast_possible_wrap)]
        let max_sessions = max_sessions.clamp(1, 500) as i64;
        let system_allowed = Self::is_system_principal(principal);

        tokio::task::spawn_blocking(
            move || -> Result<std::collections::HashMap<String, Vec<ConversationTurn>>> {
                let stmt = format!(
                    "SELECT id, session_key, role, content, timestamp, message_id
                     FROM (
                         SELECT
                             ct.id, ct.session_key, ct.role, ct.content, ct.timestamp, ct.message_id,
                             ROW_NUMBER() OVER (PARTITION BY ct.session_key ORDER BY ct.id DESC) AS row_num
                         FROM {turns_table} ct
                         INNER JOIN (
                             SELECT session_key
                             FROM {sessions_table}
                             WHERE $3
                                OR owner_id = $4
                                OR owner_id IS NULL
                                OR owner_id = 'legacy:' || session_key
                             ORDER BY updated_at DESC
                             LIMIT $2
                         ) recent_sessions
                         ON recent_sessions.session_key = ct.session_key
                         WHERE $3
                            OR ct.owner_id = $4
                            OR ct.owner_id IS NULL
                            OR ct.owner_id = 'legacy:' || ct.session_key
                     ) sub
                     WHERE row_num <= $1
                     ORDER BY session_key ASC, id ASC"
                );
                let rows = client.with_client(|client| {
                    Ok(client.query(&stmt, &[&max_turns, &max_sessions, &system_allowed, &owner_id])?)
                })?;
                let mut histories: std::collections::HashMap<String, Vec<ConversationTurn>> =
                    std::collections::HashMap::new();
                for row in &rows {
                    let turn = ConversationTurn {
                        id: row.get(0),
                        session_key: row.get(1),
                        role: row.get(2),
                        content: row.get(3),
                        timestamp: row.get(4),
                        message_id: row.get(5),
                    };
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
        deadline: Instant,
    ) -> Result<Arc<dyn crate::memory::ingress_recovery::IngressLifetime>> {
        let entry = self.ingress_owners.cell(workspace_id)?;
        let _gate = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            entry.init_gate.clone().lock_owned(),
        )
        .await
        .context("PostgreSQL ingress initialization deadline elapsed")?;
        let lifetime = if let Some(lifetime) = entry.cell.get() {
            lifetime.clone()
        } else {
            let lifetime = self
                .new_ingress_lifetime(workspace_id, token, deadline, false)
                .await?
                .context("ingress lifetime lock unavailable")?;
            entry
                .cell
                .set(lifetime.clone())
                .map_err(|_| anyhow::anyhow!("ingress lifetime initialized concurrently"))?;
            lifetime
        };
        lifetime.validate(deadline).await?;
        Ok(lifetime)
    }
    async fn append_ingress_input(
        &self,
        input: MessageEventInput,
        capsule: crate::memory::ingress_recovery::IngressCapsule,
        lifetime: Arc<dyn crate::memory::ingress_recovery::IngressLifetime>,
        deadline: Instant,
    ) -> Result<crate::memory::ingress_recovery::IngressAppendOutcome> {
        use crate::memory::ingress_recovery::{AtomicIngressMutation, IngressAppendOutcome};
        let (event, inserted) = self
            .ingress_append_atomic(
                input,
                AtomicIngressMutation::Input {
                    capsule,
                    lifetime,
                    deadline,
                },
            )
            .await?;
        Ok(if inserted {
            IngressAppendOutcome::Inserted(event)
        } else {
            IngressAppendOutcome::AlreadyAccepted(event)
        })
    }
    async fn list_ingress_recovery_candidates(
        &self,
        scope: &crate::memory::ingress_recovery::IngressRecoveryScope,
        current: Arc<dyn crate::memory::ingress_recovery::IngressLifetime>,
        phase: crate::memory::ingress_recovery::IngressRecoveryScan,
        after_id: i64,
        limit: usize,
        deadline: Instant,
    ) -> Result<Vec<crate::memory::ingress_recovery::IngressRecoveryCandidate>> {
        self.ingress_candidates(scope, current, phase, after_id, limit, deadline)
            .await
    }
    async fn read_ingress_event(
        &self,
        scope: &crate::memory::ingress_recovery::IngressRecoveryScope,
        lookup: &crate::memory::ingress_recovery::IngressEventLookup,
        deadline: Instant,
    ) -> Result<Option<MessageEvent>> {
        self.ingress_read(scope, lookup, deadline).await
    }
    async fn claim_ingress_recovery(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: Instant,
    ) -> Result<Option<crate::memory::ingress_recovery::IngressRecoveryClaim>> {
        Ok(self
            .new_ingress_lifetime(workspace_id, token, deadline, true)
            .await?
            .map(|lifetime| crate::memory::ingress_recovery::IngressRecoveryClaim { lifetime }))
    }
    async fn commit_ingress_recovery(
        &self,
        commit: crate::memory::ingress_recovery::IngressRecoveryCommit,
    ) -> Result<MessageEvent> {
        self.ingress_append_atomic(
            commit.evidence.clone(),
            crate::memory::ingress_recovery::AtomicIngressMutation::Recovery(commit),
        )
        .await
        .map(|(event, _)| event)
    }

    async fn append_message_event_until(&self, input: MessageEventInput, deadline: Instant) -> Result<MessageEvent> {
        self.ingress_repair_append(input, deadline).await
    }
    async fn project_console_message_until(&self, event: &MessageEvent, deadline: Instant) -> Result<()> {
        self.ingress_repair_console(event, deadline).await
    }
    async fn append_message_event(&self, input: MessageEventInput) -> anyhow::Result<MessageEvent> {
        self.append_message_event_ordinary(input).await
    }

    async fn find_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<MessageEvent>> {
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let workspace_id = workspace_id.to_string();
        let idempotency_key = idempotency_key.to_string();
        tokio::task::spawn_blocking(move || {
            client.with_client(|client| {
                let stmt = format!(
                    "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                            parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                            sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                            goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                            content, content_hash, raw_payload_json, visibility, created_at, updated_at
                     FROM {qualified_message_events_table}
                     WHERE workspace_id = $1 AND idempotency_key = $2
                     LIMIT 1"
                );
                client
                    .query_opt(&stmt, &[&workspace_id, &idempotency_key])?
                    .map(|row| Self::row_to_message_event(&row))
                    .transpose()
            })
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
        let client = self.client.clone();
        let table = self.qualified_message_events_table.clone();
        tokio::task::spawn_blocking(move || {
            client.with_client(|client| {
                let query = format!(
                    "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                     FROM {table}
                     WHERE workspace_id = $1 AND event_type IN ('worker.recovery.registered', 'worker.ordinary.recovery.registered', 'worker.operator.recovery.registered') AND id > $2
                     ORDER BY id ASC LIMIT $3"
                );
                client.query(&query, &[&workspace_id, &after_id, &limit])?
                    .iter().map(Self::row_to_message_event).collect()
            })
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
    ) -> Result<Option<MessageEvent>> {
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let workspace_id = workspace_id.to_string();
        if workspace_id.is_empty() || owner_id.is_empty() || event_id.is_empty() {
            return Ok(None);
        }
        let owner_id = owner_id.to_string();
        let event_id = event_id.to_string();
        tokio::task::spawn_blocking(move || {
            client.with_client(|client| {
                let stmt = format!(
                    "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                            parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                            sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                            goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                            content, content_hash, raw_payload_json, visibility, created_at, updated_at
                     FROM {qualified_message_events_table}
                     WHERE workspace_id = $1 AND owner_id = $2 AND event_id = $3
                     LIMIT 1"
                );
                client
                    .query_opt(&stmt, &[&workspace_id, &owner_id, &event_id])?
                    .map(|row| Self::row_to_message_event(&row))
                    .transpose()
            })
        })
        .await?
    }

    async fn list_message_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<MessageEvent>> {
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let principal = principal.clone();
        let limit_i64 = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let system_allowed = Self::is_system_principal(&principal);

        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its `$5` binding; legacy key(s) bind at trailing
        // placeholders starting at `$9` (after limit at `$8`).
        let session_indices = Self::session_indices(5, 9, &principal, &legacy_session_keys);
        let session_fragment =
            crate::memory::session_predicate::session_visibility_or_fragment(PG_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        tokio::task::spawn_blocking(move || -> Result<Vec<MessageEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                       goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                       content, content_hash, raw_payload_json, visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE id > $1
                  AND (
                      visibility = 'global'
                      OR (
                          workspace_id = $2
                          AND (
                              visibility = 'workspace'{owner_fragment}
                              OR (visibility = 'agent' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                              ))
                              OR (visibility = 'session' AND {session_fragment}{owner_fragment})
                              OR (visibility = 'private' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                                  OR ($6::TEXT IS NOT NULL AND sender = $6)
                              ){owner_required_fragment})
                              OR (visibility = 'system' AND $7::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id ASC
                LIMIT $8
                ",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            );
            let rows = client.with_client(|client| {
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
                    &after_id,
                    &principal.workspace_id,
                    &principal.agent_id,
                    &principal.persona_id,
                    &principal.session_key,
                    &principal.sender,
                    &system_allowed,
                    &limit_i64,
                ];
                for key in &legacy_session_keys {
                    bind.push(key);
                }
                if owner_required_fragment.bound_owner {
                    bind.push(&principal.owner_id);
                }
                Ok(client.query(&stmt, &bind)?)
            })?;
            rows.iter()
                .map(Self::row_to_message_event)
                .collect::<Result<Vec<MessageEvent>>>()
        })
        .await?
    }

    async fn list_message_events_recent(&self, principal: &MemoryPrincipal, limit: usize) -> Result<Vec<MessageEvent>> {
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let principal = principal.clone();
        let limit_i64 = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let system_allowed = Self::is_system_principal(&principal);

        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its `$4` binding; legacy key(s) bind at trailing
        // placeholders starting at `$8` (after limit at `$7`).
        let session_indices = Self::session_indices(4, 8, &principal, &legacy_session_keys);
        let session_fragment =
            crate::memory::session_predicate::session_visibility_or_fragment(PG_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(8, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(8, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        tokio::task::spawn_blocking(move || -> Result<Vec<MessageEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                       goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                       content, content_hash, raw_payload_json, visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE (
                      visibility = 'global'
                      OR (
                          workspace_id = $1
                          AND (
                              visibility = 'workspace'{owner_fragment}
                              OR (visibility = 'agent' AND (
                                  ($2::TEXT IS NOT NULL AND agent_id = $2)
                                  OR ($3::TEXT IS NOT NULL AND persona_id = $3)
                              ))
                              OR (visibility = 'session' AND {session_fragment}{owner_fragment})
                              OR (visibility = 'private' AND (
                                  ($2::TEXT IS NOT NULL AND agent_id = $2)
                                  OR ($3::TEXT IS NOT NULL AND persona_id = $3)
                                  OR ($5::TEXT IS NOT NULL AND sender = $5)
                              ){owner_required_fragment})
                              OR (visibility = 'system' AND $6::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id DESC
                LIMIT $7
                ",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            );
            let rows = client.with_client(|client| {
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
                    &principal.workspace_id,
                    &principal.agent_id,
                    &principal.persona_id,
                    &principal.session_key,
                    &principal.sender,
                    &system_allowed,
                    &limit_i64,
                ];
                for key in &legacy_session_keys {
                    bind.push(key);
                }
                if owner_required_fragment.bound_owner {
                    bind.push(&principal.owner_id);
                }
                Ok(client.query(&stmt, &bind)?)
            })?;
            rows.iter()
                .map(Self::row_to_message_event)
                .collect::<Result<Vec<MessageEvent>>>()
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
    ) -> Result<Vec<MessageEvent>> {
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let principal = principal.clone();
        let descendant_workspace_root = descendant_workspace_root.map(str::to_string);
        let start = chrono::DateTime::parse_from_rfc3339(start)?.with_timezone(&chrono::Utc);
        let end = chrono::DateTime::parse_from_rfc3339(end)?.with_timezone(&chrono::Utc);
        let limit_i64 = i64::try_from(limit.clamp(1, 100_000)).unwrap_or(100_000);
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
        let mut session_indices = Self::session_indices(4, 12, &principal, &legacy_session_keys);
        // This query has fixed placeholders after the session key. Keep $4 in
        // the predicate even when it is NULL so PostgreSQL can infer its type.
        if session_indices.is_empty() {
            session_indices.push(4);
        }
        let session_fragment =
            crate::memory::session_predicate::session_visibility_or_fragment(PG_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(12, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(12, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        tokio::task::spawn_blocking(move || -> Result<Vec<MessageEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                       goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                       content, content_hash, raw_payload_json, visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE created_at >= $9 AND created_at < $10
                  AND (
                      visibility = 'global'
                      OR (
                          (
                              workspace_id = $1
                              OR ($6::BOOLEAN AND $7::TEXT IS NOT NULL AND (
                                  workspace_id = $7 OR workspace_id LIKE $8 ESCAPE '\\'
                              ))
                          )
                          AND (
                              ($6::BOOLEAN AND $7::TEXT IS NOT NULL)
                              OR visibility = 'workspace'{owner_fragment}
                              OR (visibility = 'agent' AND (
                                  ($2::TEXT IS NOT NULL AND agent_id = $2)
                                  OR ($3::TEXT IS NOT NULL AND persona_id = $3)
                              ))
                              OR (visibility = 'session' AND {session_fragment}{owner_fragment})
                              OR (visibility = 'private' AND (
                                  ($2::TEXT IS NOT NULL AND agent_id = $2)
                                  OR ($3::TEXT IS NOT NULL AND persona_id = $3)
                                  OR ($5::TEXT IS NOT NULL AND sender = $5)
                              ){owner_required_fragment})
                              OR (visibility = 'system' AND $6::BOOLEAN)
                          )
                      )
                )
                ORDER BY created_at ASC, id ASC
                LIMIT $11
                ",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            );
            let rows = client.with_client(|client| {
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
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
                    &limit_i64,
                ];
                for key in &legacy_session_keys {
                    bind.push(key);
                }
                if owner_required_fragment.bound_owner {
                    bind.push(&principal.owner_id);
                }
                Ok(client.query(&stmt, &bind)?)
            })?;
            rows.iter().map(Self::row_to_message_event).collect::<Result<Vec<_>>>()
        })
        .await?
    }

    async fn load_recent_shared_context(&self, query: SharedContextQuery) -> Result<Vec<MessageEvent>> {
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let principal = query.principal;
        let after_id = query.since_event_id.unwrap_or(0);
        let limit_i64 = i64::try_from(query.limit.max(1)).unwrap_or(i64::MAX);
        let system_allowed = Self::is_system_principal(&principal);
        let include_roles = query
            .include_roles
            .into_iter()
            .map(|role| role.trim().to_ascii_lowercase())
            .filter(|role| !role.is_empty())
            .collect::<std::collections::HashSet<_>>();

        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its `$5` binding; legacy key(s) bind at trailing
        // placeholders starting at `$9` (after limit at `$8`).
        let session_indices = Self::session_indices(5, 9, &principal, &legacy_session_keys);
        let session_fragment =
            crate::memory::session_predicate::session_visibility_or_fragment(PG_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        tokio::task::spawn_blocking(move || -> Result<Vec<MessageEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                       goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                       content, content_hash, raw_payload_json, visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE id > $1
                  AND (
                      visibility = 'global'
                      OR (
                          workspace_id = $2
                          AND (
                              visibility = 'workspace'{owner_required_fragment}
                              OR (visibility = 'agent' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                              ))
                              OR (visibility = 'session' AND {session_fragment}{owner_fragment})
                              OR (visibility = 'private' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                                  OR ($6::TEXT IS NOT NULL AND sender = $6)
                              ){owner_required_fragment})
                              OR (visibility = 'system' AND $7::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id DESC
                LIMIT $8
                ",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            );
            let rows = client.with_client(|client| {
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
                    &after_id,
                    &principal.workspace_id,
                    &principal.agent_id,
                    &principal.persona_id,
                    &principal.session_key,
                    &principal.sender,
                    &system_allowed,
                    &limit_i64,
                ];
                for key in &legacy_session_keys {
                    bind.push(key);
                }
                if owner_required_fragment.bound_owner {
                    bind.push(&principal.owner_id);
                }
                Ok(client.query(&stmt, &bind)?)
            })?;
            let mut events = rows
                .iter()
                .map(Self::row_to_message_event)
                .collect::<Result<Vec<MessageEvent>>>()?;
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

    async fn load_recent_session_context(&self, query: SessionContextQuery) -> Result<Vec<MessageEvent>> {
        let Some(session_key) = query.principal.session_key.clone() else {
            return Ok(Vec::new());
        };
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let principal = query.principal;
        let after_id = query.since_event_id.unwrap_or(0);
        let limit_i64 = i64::try_from(query.limit.max(1)).unwrap_or(i64::MAX);
        let system_allowed = Self::is_system_principal(&principal);
        let include_roles = query
            .include_roles
            .into_iter()
            .map(|role| role.trim().to_ascii_lowercase())
            .filter(|role| !role.is_empty())
            .collect::<std::collections::HashSet<_>>();

        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its `$3` binding; legacy key(s) bind at trailing
        // placeholders starting at `$9` (after limit at `$8`). The top-level
        // `session_key` hard filter becomes an `IN (...)` read-merge union.
        let mut session_indices = vec![3usize];
        for offset in 0..legacy_session_keys.len() {
            session_indices.push(9 + offset);
        }
        let session_fragment =
            crate::memory::session_predicate::session_key_match_fragment(PG_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-C: `workspace` is owner-qualified for an owner-isolated reader;
        // every other reader's statement is byte-identical.
        let shared_tiers_arm = crate::memory::owner_predicate::session_context_shared_tiers(&owner_fragment);

        tokio::task::spawn_blocking(move || -> Result<Vec<MessageEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                       goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                       content, content_hash, raw_payload_json, visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE id > $1
                  AND workspace_id = $2
                  AND {session_fragment}
                  AND (
                      {shared_tiers_arm}
                      OR (visibility = 'agent' AND (
                          ($4::TEXT IS NOT NULL AND agent_id = $4)
                          OR ($5::TEXT IS NOT NULL AND persona_id = $5)
                      ))
                      OR (visibility = 'session'{owner_fragment})
                      OR (visibility = 'private' AND (
                          ($4::TEXT IS NOT NULL AND agent_id = $4)
                          OR ($5::TEXT IS NOT NULL AND persona_id = $5)
                          OR ($6::TEXT IS NOT NULL AND sender = $6)
                      ){owner_required_fragment})
                      OR (visibility = 'system' AND $7::BOOLEAN)
                  )
                ORDER BY id DESC
                LIMIT $8
                ",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            );
            let rows = client.with_client(|client| {
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
                    &after_id,
                    &principal.workspace_id,
                    &session_key,
                    &principal.agent_id,
                    &principal.persona_id,
                    &principal.sender,
                    &system_allowed,
                    &limit_i64,
                ];
                for key in &legacy_session_keys {
                    bind.push(key);
                }
                if owner_required_fragment.bound_owner {
                    bind.push(&principal.owner_id);
                }
                Ok(client.query(&stmt, &bind)?)
            })?;
            let mut events = rows
                .iter()
                .map(Self::row_to_message_event)
                .collect::<Result<Vec<MessageEvent>>>()?;
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

    async fn append_memory_event(&self, input: MemoryEventInput) -> Result<MemoryEvent> {
        let client = self.client.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();

        tokio::task::spawn_blocking(move || -> Result<MemoryEvent> {
            let now = Utc::now();
            let event_id = input.event_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let insert_stmt = format!(
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                    session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                ON CONFLICT (event_id) DO NOTHING
                "
            );
            let select_stmt = format!(
                "
                SELECT id, event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                       session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                FROM {qualified_memory_events_table}
                WHERE event_id = $1
                LIMIT 1
                "
            );
            client.with_client(|client| {
                client.execute(
                    &insert_stmt,
                    &[
                        &event_id,
                        &input.workspace_id,
                        &input.owner_id,
                        &input.event_type,
                        &input.subject_table,
                        &input.subject_id,
                        &input.session_key,
                        &input.run_id,
                        &input.parent_run_id,
                        &input.agent_id,
                        &input.persona_id,
                        &input.visibility.as_str(),
                        &input.payload_json,
                        &now,
                    ],
                )?;
                let row = client.query_one(&select_stmt, &[&event_id])?;
                Self::row_to_memory_event(&row)
            })
        })
        .await?
    }

    async fn list_memory_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<MemoryEvent>> {
        let client = self.client.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();
        let principal = principal.clone();
        let limit_i64 = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let system_allowed = Self::is_system_principal(&principal);

        // C03: the owner parameter binds at the first free trailing slot (`$8`).
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            8,
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            8,
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        tokio::task::spawn_blocking(move || -> Result<Vec<MemoryEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                       session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                FROM {qualified_memory_events_table}
                WHERE id > $1
                  AND (
                      visibility = 'global'
                      OR (
                          workspace_id = $2
                          AND (
                              visibility = 'workspace'{owner_fragment}
                              OR (visibility = 'agent' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                              ))
                              OR (visibility = 'session' AND $5::TEXT IS NOT NULL AND session_key = $5{owner_fragment})
                              OR (visibility = 'private' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                              ){owner_required_fragment})
                              OR (visibility = 'system' AND $6::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id ASC
                LIMIT $7
                ",
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            );
            let rows = client.with_client(|client| {
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
                    &after_id,
                    &principal.workspace_id,
                    &principal.agent_id,
                    &principal.persona_id,
                    &principal.session_key,
                    &system_allowed,
                    &limit_i64,
                ];
                if owner_required_fragment.bound_owner {
                    bind.push(&principal.owner_id);
                }
                Ok(client.query(&stmt, &bind)?)
            })?;
            rows.iter()
                .map(Self::row_to_memory_event)
                .collect::<Result<Vec<MemoryEvent>>>()
        })
        .await?
    }

    async fn list_memory_events_recent(&self, principal: &MemoryPrincipal, limit: usize) -> Result<Vec<MemoryEvent>> {
        let client = self.client.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();
        let principal = principal.clone();
        let limit_i64 = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let system_allowed = Self::is_system_principal(&principal);

        // C03: the owner parameter binds at the first free trailing slot (`$7`).
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            7,
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            7,
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        tokio::task::spawn_blocking(move || -> Result<Vec<MemoryEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                       session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                FROM {qualified_memory_events_table}
                WHERE (
                      visibility = 'global'
                      OR (
                          workspace_id = $1
                          AND (
                              visibility = 'workspace'{owner_fragment}
                              OR (visibility = 'agent' AND (
                                  ($2::TEXT IS NOT NULL AND agent_id = $2)
                                  OR ($3::TEXT IS NOT NULL AND persona_id = $3)
                              ))
                              OR (visibility = 'session' AND $4::TEXT IS NOT NULL AND session_key = $4{owner_fragment})
                              OR (visibility = 'private' AND (
                                  ($2::TEXT IS NOT NULL AND agent_id = $2)
                                  OR ($3::TEXT IS NOT NULL AND persona_id = $3)
                              ){owner_required_fragment})
                              OR (visibility = 'system' AND $5::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id DESC
                LIMIT $6
                ",
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            );
            let rows = client.with_client(|client| {
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
                    &principal.workspace_id,
                    &principal.agent_id,
                    &principal.persona_id,
                    &principal.session_key,
                    &system_allowed,
                    &limit_i64,
                ];
                if owner_required_fragment.bound_owner {
                    bind.push(&principal.owner_id);
                }
                Ok(client.query(&stmt, &bind)?)
            })?;
            rows.iter()
                .map(Self::row_to_memory_event)
                .collect::<Result<Vec<MemoryEvent>>>()
        })
        .await?
    }

    async fn create_memory_draft(&self, input: MemoryDraftInput) -> Result<MemoryDraft> {
        let client = self.client.clone();
        let qualified_drafts_table = self.qualified_drafts_table.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();
        let draft_id = input.draft_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let category = Self::category_to_str(&input.category);
        let visibility = input.visibility.as_str().to_string();

        tokio::task::spawn_blocking(move || -> Result<MemoryDraft> {
            let now = Utc::now();
            let stmt = format!(
                "
                INSERT INTO {qualified_drafts_table}
                    (
                        draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                        agent_id, persona_id, key, content, category, source_event_id,
                        visibility, status, payload_json, created_at, updated_at
                    )
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, 'pending', $14, $15, $16)
                ON CONFLICT (draft_id) DO UPDATE SET
                    workspace_id = EXCLUDED.workspace_id,
                    owner_id = EXCLUDED.owner_id,
                    worker_run_id = EXCLUDED.worker_run_id,
                    parent_run_id = EXCLUDED.parent_run_id,
                    session_key = EXCLUDED.session_key,
                    agent_id = EXCLUDED.agent_id,
                    persona_id = EXCLUDED.persona_id,
                    key = EXCLUDED.key,
                    content = EXCLUDED.content,
                    category = EXCLUDED.category,
                    source_event_id = EXCLUDED.source_event_id,
                    visibility = EXCLUDED.visibility,
                    payload_json = EXCLUDED.payload_json,
                    updated_at = EXCLUDED.updated_at
                RETURNING
                    id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                    agent_id, persona_id, key, content, category, source_event_id,
                    visibility, status, payload_json, created_at, updated_at
                "
            );
            let outbox_stmt = format!(
                // C03: the draft's own owner is recorded in the column, not only
                // inside `payload_json`, so a `session`/`private` row born here
                // is attributed and stays readable by its owner-isolated owner.
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id, session_key,
                    agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, 'memory.draft.created', 'memory_drafts', $4, $5, $6, $7, $8, $9, $10)
                "
            );
            let row = client.with_client(|client| {
                let mut tx = client.transaction()?;
                let row = tx.query_one(
                    &stmt,
                    &[
                        &draft_id,
                        &input.workspace_id,
                        &input.owner_id,
                        &input.worker_run_id,
                        &input.parent_run_id,
                        &input.session_key,
                        &input.agent_id,
                        &input.persona_id,
                        &input.key,
                        &input.content,
                        &category,
                        &input.source_event_id,
                        &visibility,
                        &input.payload_json,
                        &now,
                        &now,
                    ],
                )?;
                let draft = Self::row_to_draft(&row)?;
                let payload_json = serde_json::json!({
                    "worker_run_id": draft.worker_run_id,
                    "owner_id": draft.owner_id,
                    "parent_run_id": draft.parent_run_id,
                    "key": draft.key,
                })
                .to_string();
                tx.execute(
                    &outbox_stmt,
                    &[
                        &Uuid::new_v4().to_string(),
                        &draft.workspace_id,
                        &draft.owner_id,
                        &draft.draft_id,
                        &draft.session_key,
                        &draft.agent_id,
                        &draft.persona_id,
                        &draft.visibility.as_str(),
                        &payload_json,
                        &Utc::now(),
                    ],
                )?;
                tx.commit()?;
                Ok(row)
            })?;
            Self::row_to_draft(&row)
        })
        .await?
    }

    async fn list_memory_drafts_for_run(
        &self,
        principal: &MemoryPrincipal,
        worker_run_id: &str,
    ) -> Result<Vec<MemoryDraft>> {
        let client = self.client.clone();
        let qualified_drafts_table = self.qualified_drafts_table.clone();
        let worker_run_id = worker_run_id.to_string();
        // Owner ACL: only return drafts owned by the principal (or ownerless /
        // system-created drafts). System principals bypass the filter.
        let owner = if Self::is_system_principal(principal) {
            None
        } else {
            principal
                .owner_id
                .as_deref()
                .filter(|owner| !owner.trim().is_empty())
                .map(str::to_string)
        };

        tokio::task::spawn_blocking(move || -> Result<Vec<MemoryDraft>> {
            let owner_clause = if owner.is_some() {
                "AND (owner_id = $2 OR owner_id IS NULL)"
            } else {
                ""
            };
            let stmt = format!(
                "
                SELECT
                    id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                    agent_id, persona_id, key, content, category, source_event_id,
                    visibility, status, payload_json, created_at, updated_at
                FROM {qualified_drafts_table}
                WHERE worker_run_id = $1 {owner_clause}
                ORDER BY id
                "
            );
            let rows = client.with_client(|client| {
                if let Some(owner) = owner.as_deref() {
                    Ok(client.query(&stmt, &[&worker_run_id, &owner])?)
                } else {
                    Ok(client.query(&stmt, &[&worker_run_id])?)
                }
            })?;
            rows.iter()
                .map(Self::row_to_draft)
                .collect::<Result<Vec<MemoryDraft>>>()
        })
        .await?
    }

    async fn merge_memory_draft(&self, principal: &MemoryPrincipal, draft_id: &str) -> Result<Option<MemoryDraft>> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_drafts_table = self.qualified_drafts_table.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();
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

        tokio::task::spawn_blocking(move || -> Result<Option<MemoryDraft>> {
            let select_stmt = format!(
                "
                SELECT
                    id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                    agent_id, persona_id, key, content, category, source_event_id,
                    visibility, status, payload_json, created_at, updated_at
                FROM {qualified_drafts_table}
                WHERE draft_id = $1
                LIMIT 1
                "
            );
            let Some(row) = client.with_client(|client| Ok(client.query_opt(&select_stmt, &[&draft_id])?))? else {
                return Ok(None);
            };
            let draft = Self::row_to_draft(&row)?;
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

            let category = Self::category_to_str(&draft.category);
            let now = Utc::now();
            let memory_id = Uuid::new_v4().to_string();
            let upsert_stmt = format!(
                "
                INSERT INTO {qualified_table}
                    (
                        id, key, content, category, created_at, updated_at, session_id,
                        workspace_id, owner_id, agent_id, persona_id, source_event_id, source
                    )
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 'memory_draft')
                ON CONFLICT (key) DO UPDATE SET
                    content = EXCLUDED.content,
                    category = EXCLUDED.category,
                    updated_at = EXCLUDED.updated_at,
                    session_id = EXCLUDED.session_id,
                    workspace_id = EXCLUDED.workspace_id,
                    owner_id = EXCLUDED.owner_id,
                    agent_id = EXCLUDED.agent_id,
                    persona_id = EXCLUDED.persona_id,
                    source_event_id = EXCLUDED.source_event_id,
                    source = EXCLUDED.source
                "
            );
            let update_stmt = format!(
                "
                UPDATE {qualified_drafts_table}
                SET status = 'merged', updated_at = $2
                WHERE draft_id = $1
                RETURNING
                    id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                    agent_id, persona_id, key, content, category, source_event_id,
                    visibility, status, payload_json, created_at, updated_at
                "
            );
            let outbox_stmt = format!(
                // C03: see `create_memory_draft` - the owner goes in the column.
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id, session_key,
                    agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                "
            );
            let row = client.with_client(|client| {
                let mut tx = client.transaction()?;
                tx.execute(
                    &upsert_stmt,
                    &[
                        &memory_id,
                        &draft.key,
                        &draft.content,
                        &category,
                        &now,
                        &now,
                        &draft.session_key,
                        &draft.workspace_id,
                        &draft.owner_id,
                        &draft.agent_id,
                        &draft.persona_id,
                        &draft.source_event_id,
                    ],
                )?;
                let row = tx.query_one(&update_stmt, &[&draft_id, &now])?;
                let merged = Self::row_to_draft(&row)?;
                let payload_json = serde_json::json!({
                    "draft_id": merged.draft_id,
                    "owner_id": merged.owner_id,
                    "worker_run_id": merged.worker_run_id,
                    "parent_run_id": merged.parent_run_id,
                    "key": merged.key,
                })
                .to_string();
                for (event_type, subject_table, subject_id) in [
                    ("memory.draft.merged", "memory_drafts", merged.draft_id.as_str()),
                    ("memory.stored", "memories", merged.key.as_str()),
                ] {
                    tx.execute(
                        &outbox_stmt,
                        &[
                            &Uuid::new_v4().to_string(),
                            &merged.workspace_id,
                            &merged.owner_id,
                            &event_type,
                            &subject_table,
                            &subject_id,
                            &merged.session_key,
                            &merged.agent_id,
                            &merged.persona_id,
                            &merged.visibility.as_str(),
                            &payload_json,
                            &Utc::now(),
                        ],
                    )?;
                }
                tx.commit()?;
                Ok(row)
            })?;
            Ok(Some(Self::row_to_draft(&row)?))
        })
        .await?
    }

    async fn reject_memory_draft(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        reason: Option<&str>,
    ) -> Result<Option<MemoryDraft>> {
        let client = self.client.clone();
        let qualified_drafts_table = self.qualified_drafts_table.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();
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

        tokio::task::spawn_blocking(move || -> Result<Option<MemoryDraft>> {
            let now = Utc::now();
            // Owner ACL: reject attempts on drafts owned by a different principal.
            if let Some(owner) = owner.as_deref() {
                let select_owner = format!("SELECT owner_id FROM {qualified_drafts_table} WHERE draft_id = $1 LIMIT 1");
                let existing = client.with_client(|client| Ok(client.query_opt(&select_owner, &[&draft_id])?))?;
                if let Some(row) = existing {
                    let draft_owner: Option<String> = row.get(0);
                    if let Some(draft_owner) = draft_owner {
                        if draft_owner != owner {
                            anyhow::bail!("memory draft {draft_id} is owned by a different principal");
                        }
                    }
                }
            }
            let payload = reason
                .as_ref()
                .map(|reason| serde_json::json!({ "reason": reason }).to_string());
            let stmt = format!(
                "
                UPDATE {qualified_drafts_table}
                SET
                    status = 'rejected',
                    payload_json = COALESCE($3, payload_json),
                    updated_at = $2
                WHERE draft_id = $1
                RETURNING
                    id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                    agent_id, persona_id, key, content, category, source_event_id,
                    visibility, status, payload_json, created_at, updated_at
                "
            );
            let outbox_stmt = format!(
                // C03: see `create_memory_draft` - the owner goes in the column.
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id, session_key,
                    agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, 'memory.draft.rejected', 'memory_drafts', $4, $5, $6, $7, $8, $9, $10)
                "
            );
            let row = client.with_client(|client| {
                let mut tx = client.transaction()?;
                let row = tx.query_opt(&stmt, &[&draft_id, &now, &payload])?;
                if let Some(row) = &row {
                    let draft = Self::row_to_draft(row)?;
                    let payload_json = serde_json::json!({
                        "draft_id": draft.draft_id,
                        "owner_id": draft.owner_id,
                        "worker_run_id": draft.worker_run_id,
                        "parent_run_id": draft.parent_run_id,
                        "key": draft.key,
                        "reason": reason.as_deref(),
                    })
                    .to_string();
                    tx.execute(
                        &outbox_stmt,
                        &[
                            &Uuid::new_v4().to_string(),
                            &draft.workspace_id,
                            &draft.owner_id,
                            &draft.draft_id,
                            &draft.session_key,
                            &draft.agent_id,
                            &draft.persona_id,
                            &draft.visibility.as_str(),
                            &payload_json,
                            &Utc::now(),
                        ],
                    )?;
                }
                tx.commit()?;
                Ok(row)
            })?;
            row.as_ref().map(Self::row_to_draft).transpose()
        })
        .await?
    }

    async fn increment_useful_count(&self, _id: &str) -> Result<()> {
        Ok(())
    }

    async fn ingest_document(&self, input: DocumentIngestInput) -> Result<DocumentRecord> {
        let workspace_context = input.workspace_id.trim();
        anyhow::ensure!(
            !workspace_context.is_empty(),
            "PostgreSQL vector isolation requires document workspace_id"
        );
        let owner_context = input
            .owner_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("PostgreSQL vector isolation requires document owner_id")?;
        let workspace_context = workspace_context.to_string();
        let owner_context = owner_context.to_string();
        let client = self.client.clone();
        let qualified_documents_table = self.qualified_documents_table.clone();
        let qualified_document_chunks_table = self.qualified_document_chunks_table.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();
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
            let embedding = self
                .get_or_compute_embedding(&content, Some(&workspace_context), Some(&owner_context), false)
                .await?;
            let embedding_vector = embedding.as_ref().and_then(|embedding| {
                self.pgvector_available
                    .then(|| Self::pgvector_literal(embedding))
                    .flatten()
            });
            let embedding_bytes = embedding.map(|embedding| vector::vec_to_bytes(&embedding));
            prepared_chunks.push((chunk_index, heading, content, embedding_bytes, embedding_vector));
        }
        let pgvector_available = self.pgvector_available;

        tokio::task::spawn_blocking(move || -> Result<DocumentRecord> {
            let now = Utc::now();
            let document_id = input.document_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let content_sha256 = Self::content_sha256_hex(&input.content);
            let visibility = input.visibility.as_str().to_string();
            let chunk_count = i64::try_from(prepared_chunks.len()).unwrap_or(i64::MAX);
            let insert_document_stmt = format!(
                "
                INSERT INTO {qualified_documents_table} (
                    document_id, workspace_id, owner_id, topic_id, task_id, source_message_event_id,
                    source_kind, source_uri, title, content_sha256, mime_type, visibility,
                    metadata_json, chunk_count, created_at, updated_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
                ON CONFLICT (document_id) DO UPDATE SET
                    workspace_id = EXCLUDED.workspace_id,
                    owner_id = EXCLUDED.owner_id,
                    topic_id = EXCLUDED.topic_id,
                    task_id = EXCLUDED.task_id,
                    source_message_event_id = EXCLUDED.source_message_event_id,
                    source_kind = EXCLUDED.source_kind,
                    source_uri = EXCLUDED.source_uri,
                    title = EXCLUDED.title,
                    content_sha256 = EXCLUDED.content_sha256,
                    mime_type = EXCLUDED.mime_type,
                    visibility = EXCLUDED.visibility,
                    metadata_json = EXCLUDED.metadata_json,
                    chunk_count = EXCLUDED.chunk_count,
                    updated_at = EXCLUDED.updated_at
                "
            );
            let delete_chunks_stmt = format!(
                "DELETE FROM {qualified_document_chunks_table} \
                 WHERE document_id = $1 AND workspace_id = $2 AND owner_id = $3"
            );
            let insert_chunk_stmt = format!(
                "
                INSERT INTO {qualified_document_chunks_table} (
                    chunk_id, document_id, workspace_id, owner_id, topic_id, task_id,
                    chunk_index, heading, content, content_sha256, embedding,
                    embedding_provider, embedding_model, embedding_dimensions,
                    visibility, source_anchor, token_estimate, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
                "
            );
            let insert_event_stmt = format!(
                // C03: the ingested document's own owner is recorded in the
                // column, so a `session`/`private` ingest event is attributed.
                // V02-R6: the turn's scoping keys are bound too, so the event is
                // reachable through the `session`/`agent`/`private` arms of the
                // read predicate instead of carrying unmatchable NULLs.
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                    session_key, agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, 'document.ingested', 'documents', $4, $5, $6, $7, $8, $9, $10)
                "
            );
            let select_document_stmt = format!(
                "
                SELECT id, document_id, workspace_id, owner_id, topic_id, task_id,
                       source_message_event_id, source_kind, source_uri, title,
                       content_sha256, mime_type, visibility, metadata_json,
                       chunk_count, created_at, updated_at
                FROM {qualified_documents_table}
                WHERE document_id = $1 AND workspace_id = $2 AND owner_id = $3
                LIMIT 1
                "
            );
            client.with_client(|client| {
                let mut tx = client.transaction()?;
                // Every ingest statement is exact on the document's own owner, so the
                // narrowest read scope costs nothing and admits no other owner.
                Self::apply_rls_context_raw(&mut tx, false, Some(&workspace_context), Some(&owner_context), true)?;
                tx.execute(
                    &insert_document_stmt,
                    &[
                        &document_id,
                        &input.workspace_id,
                        &input.owner_id,
                        &input.topic_id,
                        &input.task_id,
                        &input.source_message_event_id,
                        &input.source_kind,
                        &input.source_uri,
                        &input.title,
                        &content_sha256,
                        &input.mime_type,
                        &visibility,
                        &input.metadata_json,
                        &chunk_count,
                        &now,
                        &now,
                    ],
                )?;
                tx.execute(&delete_chunks_stmt, &[&document_id, &workspace_context, &owner_context])?;
                let update_chunk_vector_stmt = if pgvector_available {
                    Some(format!(
                        "UPDATE {qualified_document_chunks_table} SET embedding_vector = $1::text::vector \
                         WHERE chunk_id = $2 AND workspace_id = $3 AND owner_id = $4"
                    ))
                } else {
                    None
                };
                for (chunk_index, heading, content, embedding_bytes, embedding_vector) in prepared_chunks {
                    let chunk_id = format!("{document_id}:chunk:{chunk_index}");
                    let source_anchor = format!("{document_id}#chunk-{chunk_index}");
                    let token_estimate = i64::try_from(content.chars().count().div_ceil(4)).unwrap_or(i64::MAX);
                    let chunk_index = i64::try_from(chunk_index).unwrap_or(i64::MAX);
                    let chunk_hash = Self::content_sha256_hex(&content);
                    let chunk_embedding_provider = embedding_bytes.as_ref().map(|_| embedding_provider.clone());
                    let chunk_embedding_model = embedding_bytes.as_ref().map(|_| embedding_model.clone());
                    let chunk_embedding_dimensions = embedding_bytes.as_ref().map(|_| embedding_dimensions);
                    tx.execute(
                        &insert_chunk_stmt,
                        &[
                            &chunk_id,
                            &document_id,
                            &input.workspace_id,
                            &input.owner_id,
                            &input.topic_id,
                            &input.task_id,
                            &chunk_index,
                            &heading,
                            &content,
                            &chunk_hash,
                            &embedding_bytes,
                            &chunk_embedding_provider,
                            &chunk_embedding_model,
                            &chunk_embedding_dimensions,
                            &visibility,
                            &source_anchor,
                            &token_estimate,
                            &now,
                        ],
                    )?;
                    if let (Some(stmt), Some(embedding_vector)) =
                        (update_chunk_vector_stmt.as_ref(), embedding_vector.as_ref())
                    {
                        tx.execute(stmt, &[embedding_vector, &chunk_id, &workspace_context, &owner_context])?;
                    }
                }
                let payload_json = serde_json::json!({
                    "owner_id": input.owner_id,
                    "topic_id": input.topic_id,
                    "task_id": input.task_id,
                    "source_message_event_id": input.source_message_event_id,
                    "chunk_count": chunk_count,
                    "content_sha256": content_sha256
                })
                .to_string();
                tx.execute(
                    &insert_event_stmt,
                    &[
                        &Uuid::new_v4().to_string(),
                        &input.workspace_id,
                        &input.owner_id,
                        &document_id,
                        &input.session_key,
                        &input.agent_id,
                        &input.persona_id,
                        &visibility,
                        &payload_json,
                        &Utc::now(),
                    ],
                )?;
                let row = tx.query_one(
                    &select_document_stmt,
                    &[&document_id, &workspace_context, &owner_context],
                )?;
                let document = Self::row_to_document(&row)?;
                tx.commit()?;
                Ok(document)
            })
        })
        .await?
    }

    async fn search_document_chunks(
        &self,
        principal: &MemoryPrincipal,
        query: &str,
        limit: usize,
    ) -> Result<Vec<DocumentSearchResult>> {
        let client = self.client.clone();
        let qualified_documents_table = self.qualified_documents_table.clone();
        let qualified_document_chunks_table = self.qualified_document_chunks_table.clone();
        let principal = principal.clone();
        let query = query.trim().to_string();
        let owner_id = if principal.require_owner_match {
            // V02-R8: a blank `owner_id` is not an owner. Taking it verbatim
            // would bind whitespace as the caller's identity, here and in the
            // RLS context below; `document_owner_for_principal` applies the
            // shared blank rule and yields `None`, which the statements read as
            // "admit nothing".
            Self::document_owner_for_principal(&principal)
        } else {
            principal
                .owner_id
                .clone()
                .or_else(|| Self::document_owner_for_principal(&principal))
        };
        // V02-R8: empty for every non-isolated principal, so all three
        // statements below stay byte-identical for them.
        let visibility_guard = Self::document_visibility_guard(&principal);
        // The decision `isolated_read_without_owner` takes for the keyed reads,
        // applied to the same blank rule here: an owner-isolated caller with no
        // usable owner id answers empty rather than erroring.
        // `apply_rls_context_raw` below rejects a non-system read that carries
        // no owner, so without this the chunk search would return an error
        // where the SQLite adapter returns no rows. A non-isolated principal is
        // never short-circuited.
        if principal.require_owner_match && owner_id.is_none() {
            tracing::warn!(
                operation = "search_document_chunks",
                "owner-isolated document search carries no owner id; returning no rows"
            );
            return Ok(Vec::new());
        }
        let rls_system = Self::is_system_principal(&principal);
        let rls_owner_isolated = principal.require_owner_match;
        let rls_workspace = principal.workspace_id.trim().to_string();
        let limit_i64 = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let query_embedding = self
            .get_or_compute_embedding(&query, Some(&rls_workspace), owner_id.as_deref(), rls_system)
            .await?;
        let embedding_provider = self.embedding_provider_name();
        let embedding_model = self.embedding_model_name();
        let embedding_dimensions = self.embedder.dimensions();
        let embedding_dimensions_i64 = self.embedding_dimensions_i64();
        let vector_weight = self.vector_weight;
        let keyword_weight = self.keyword_weight;
        let pgvector_available = self.pgvector_available;
        let query_vector = query_embedding
            .as_ref()
            .and_then(|embedding| Self::pgvector_literal(embedding));

        tokio::task::spawn_blocking(move || -> Result<Vec<DocumentSearchResult>> {
            let stmt = format!(
                "
                SELECT c.id, c.chunk_id, c.document_id, c.workspace_id, c.owner_id, c.topic_id, c.task_id,
                       c.chunk_index, c.heading, c.content, c.content_sha256, c.source_anchor,
                       c.token_estimate, c.created_at,
                       ts_rank_cd(to_tsvector('simple', c.content), plainto_tsquery('simple', $1)) AS score,
                       d.source_kind AS source_kind
                FROM {qualified_document_chunks_table} c
                JOIN {qualified_documents_table} d ON d.document_id = c.document_id
                WHERE c.workspace_id = $2
                  AND (
                      d.visibility IN ('global', 'workspace'){visibility_guard}
                      OR ($3::TEXT IS NOT NULL AND c.owner_id = $3)
                  )
                  AND (
                      to_tsvector('simple', c.content) @@ plainto_tsquery('simple', $1)
                      OR c.content ILIKE '%' || $1 || '%'
                      OR c.heading ILIKE '%' || $1 || '%'
                  )
                ORDER BY score DESC, c.id ASC
                LIMIT $4
                "
            );
            let rows = client.with_client(|client| {
                let mut tx = client.transaction()?;
                Self::apply_rls_context_raw(
                    &mut tx,
                    rls_system,
                    Some(&rls_workspace),
                    owner_id.as_deref(),
                    rls_owner_isolated,
                )?;
                let rows = tx.query(&stmt, &[&query, &principal.workspace_id, &owner_id, &limit_i64])?;
                tx.commit()?;
                Ok(rows)
            })?;
            let mut by_chunk = std::collections::HashMap::<String, DocumentSearchResult>::new();
            for row in &rows {
                let mut result = {
                    let chunk = Self::row_to_document_chunk(row)?;
                    let score: f32 = row.try_get::<_, f32>(14).unwrap_or(0.0);
                    let source_kind: Option<String> = row.try_get::<_, Option<String>>(15).unwrap_or(None);
                    DocumentSearchResult {
                        chunk,
                        score,
                        source_kind,
                    }
                };
                result.score *= keyword_weight;
                by_chunk.insert(result.chunk.chunk_id.clone(), result);
            }

            if let Some(query_embedding) = query_embedding {
                if pgvector_available {
                    if let Some(query_vector) = query_vector.as_ref() {
                        let vector_stmt = format!(
                            "
                            SELECT c.id, c.chunk_id, c.document_id, c.workspace_id, c.owner_id, c.topic_id, c.task_id,
                                   c.chunk_index, c.heading, c.content, c.content_sha256, c.source_anchor,
                                   c.token_estimate, c.created_at,
                                   (1.0 - (c.embedding_vector <=> $1::text::vector))::REAL AS score,
                                   d.source_kind AS source_kind
                            FROM {qualified_document_chunks_table} c
                            JOIN {qualified_documents_table} d ON d.document_id = c.document_id
                            WHERE c.embedding_vector IS NOT NULL
                              AND c.embedding_provider = $2
                              AND c.embedding_model = $3
                              AND c.embedding_dimensions = $4
                              AND c.workspace_id = $5
                              AND (
                                  d.visibility IN ('global', 'workspace'){visibility_guard}
                                  OR ($6::TEXT IS NOT NULL AND c.owner_id = $6)
                              )
                            ORDER BY c.embedding_vector <=> $1::text::vector
                            LIMIT $7
                            "
                        );
                        let candidate_limit = limit_i64.saturating_mul(4).max(limit_i64);
                        let rows = client.with_client(|client| {
                            let mut tx = client.transaction()?;
                            Self::apply_rls_context_raw(
                                &mut tx,
                                rls_system,
                                Some(&rls_workspace),
                                owner_id.as_deref(),
                                rls_owner_isolated,
                            )?;
                            let rows = tx.query(
                                &vector_stmt,
                                &[
                                    query_vector,
                                    &embedding_provider,
                                    &embedding_model,
                                    &embedding_dimensions_i64,
                                    &principal.workspace_id,
                                    &owner_id,
                                    &candidate_limit,
                                ],
                            )?;
                            tx.commit()?;
                            Ok(rows)
                        })?;
                        for row in &rows {
                            let score = row.try_get::<_, f32>(14).unwrap_or(0.0) * vector_weight;
                            if score <= 0.0 {
                                continue;
                            }
                            let chunk = Self::row_to_document_chunk(row)?;
                            let source_kind: Option<String> = row.try_get::<_, Option<String>>(15).unwrap_or(None);
                            by_chunk
                                .entry(chunk.chunk_id.clone())
                                .and_modify(|existing| {
                                    existing.score += score;
                                })
                                .or_insert(DocumentSearchResult {
                                    chunk,
                                    score,
                                    source_kind,
                                });
                        }
                    }
                } else {
                    // FIX-P2-03: BYTEA document-chunk fallback (no pgvector). As with
                    // the memory-entry fallback above, SQL cannot order by vector
                    // distance, so candidates are bounded by a pure anti-DoS safety
                    // cap (after the workspace/visibility/owner WHERE filter) and
                    // re-ranked in-process by cosine similarity. The cap is not a
                    // relevance filter; `ORDER BY c.id` keeps the (cap-only)
                    // truncation deterministic without faking similarity. Known
                    // degradation tradeoff of running without pgvector.
                    let candidate_limit = POSTGRES_BYTEA_FALLBACK_CANDIDATE_CAP;
                    let vector_stmt = format!(
                        "
                        SELECT c.id, c.chunk_id, c.document_id, c.workspace_id, c.owner_id, c.topic_id, c.task_id,
                               c.chunk_index, c.heading, c.content, c.content_sha256, c.source_anchor,
                               c.token_estimate, c.created_at, c.embedding, d.source_kind AS source_kind
                        FROM {qualified_document_chunks_table} c
                        JOIN {qualified_documents_table} d ON d.document_id = c.document_id
                        WHERE c.embedding IS NOT NULL
                          AND c.embedding_provider = $1
                          AND c.embedding_model = $2
                          AND c.embedding_dimensions = $3
                          AND c.workspace_id = $4
                          AND (
                              d.visibility IN ('global', 'workspace'){visibility_guard}
                              OR ($5::TEXT IS NOT NULL AND c.owner_id = $5)
                          )
                        ORDER BY c.id
                        LIMIT $6
                        "
                    );
                    let rows = client.with_client(|client| {
                        let mut tx = client.transaction()?;
                        Self::apply_rls_context_raw(
                            &mut tx,
                            rls_system,
                            Some(&rls_workspace),
                            owner_id.as_deref(),
                            rls_owner_isolated,
                        )?;
                        let rows = tx.query(
                            &vector_stmt,
                            &[
                                &embedding_provider,
                                &embedding_model,
                                &embedding_dimensions_i64,
                                &principal.workspace_id,
                                &owner_id,
                                &candidate_limit,
                            ],
                        )?;
                        tx.commit()?;
                        Ok(rows)
                    })?;
                    for row in &rows {
                        let embedding_blob: Vec<u8> = row.get(14);
                        let embedding = vector::bytes_to_vec(&embedding_blob);
                        if embedding.len() != embedding_dimensions {
                            tracing::debug!(
                                chunk_id = %row.get::<_, String>(1),
                                expected_dimensions = embedding_dimensions,
                                actual_dimensions = embedding.len(),
                                "Skipping stale Postgres document chunk embedding with mismatched dimensions"
                            );
                            continue;
                        }
                        let score = vector::cosine_similarity(&query_embedding, &embedding) * vector_weight;
                        if score <= 0.0 {
                            continue;
                        }
                        let chunk = Self::row_to_document_chunk(row)?;
                        let source_kind: Option<String> = row.try_get::<_, Option<String>>(15).unwrap_or(None);
                        by_chunk
                            .entry(chunk.chunk_id.clone())
                            .and_modify(|existing| {
                                existing.score += score;
                            })
                            .or_insert(DocumentSearchResult {
                                chunk,
                                score,
                                source_kind,
                            });
                    }
                }
            }

            let mut results: Vec<DocumentSearchResult> = by_chunk.into_values().collect();
            results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
            results.truncate(limit.max(1));
            Ok(results)
        })
        .await?
    }

    async fn get_document_chunk(&self, chunk_id: &str) -> Result<Option<DocumentChunkRecord>> {
        let client = self.client.clone();
        let qualified_document_chunks_table = self.qualified_document_chunks_table.clone();
        let chunk_id = chunk_id.to_string();

        tokio::task::spawn_blocking(move || -> Result<Option<DocumentChunkRecord>> {
            let stmt = format!(
                "
                SELECT id, chunk_id, document_id, workspace_id, owner_id, topic_id, task_id,
                       chunk_index, heading, content, content_sha256, source_anchor,
                       token_estimate, created_at
                FROM {qualified_document_chunks_table}
                WHERE chunk_id = $1
                LIMIT 1
                "
            );
            let row = client.with_client(|client| Ok(client.query_opt(&stmt, &[&chunk_id])?))?;
            row.as_ref().map(Self::row_to_document_chunk).transpose()
        })
        .await?
    }

    async fn link_memory_source(&self, input: MemoryLinkInput) -> Result<MemoryLink> {
        let client = self.client.clone();
        let qualified_memory_links_table = self.qualified_memory_links_table.clone();
        let link_id = input.link_id.unwrap_or_else(|| Uuid::new_v4().to_string());

        tokio::task::spawn_blocking(move || -> Result<MemoryLink> {
            let now = Utc::now();
            let insert_stmt = format!(
                "
                INSERT INTO {qualified_memory_links_table} (
                    link_id, workspace_id, owner_id, memory_key, memory_event_id,
                    message_event_id, document_id, chunk_id, link_type, payload_json, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                ON CONFLICT (link_id) DO NOTHING
                "
            );
            let select_stmt = format!(
                "
                SELECT id, link_id, workspace_id, owner_id, memory_key, memory_event_id,
                       message_event_id, document_id, chunk_id, link_type, payload_json, created_at
                FROM {qualified_memory_links_table}
                WHERE link_id = $1
                LIMIT 1
                "
            );
            client.with_client(|client| {
                client.execute(
                    &insert_stmt,
                    &[
                        &link_id,
                        &input.workspace_id,
                        &input.owner_id,
                        &input.memory_key,
                        &input.memory_event_id,
                        &input.message_event_id,
                        &input.document_id,
                        &input.chunk_id,
                        &input.link_type,
                        &input.payload_json,
                        &now,
                    ],
                )?;
                let row = client.query_one(&select_stmt, &[&link_id])?;
                Self::row_to_memory_link(&row)
            })
        })
        .await?
    }

    async fn append_retrieval_trace(&self, input: RetrievalTraceInput) -> Result<RetrievalTrace> {
        let client = self.client.clone();
        let qualified_retrieval_traces_table = self.qualified_retrieval_traces_table.clone();
        let trace_id = input.trace_id.unwrap_or_else(|| Uuid::new_v4().to_string());

        tokio::task::spawn_blocking(move || -> Result<RetrievalTrace> {
            let now = Utc::now();
            let candidate_count = i64::try_from(input.candidate_count).unwrap_or(i64::MAX);
            let selected_count = i64::try_from(input.selected_count).unwrap_or(i64::MAX);
            let dropped_count = i64::try_from(input.dropped_count).unwrap_or(i64::MAX);
            let budget_tokens = input
                .budget_tokens
                .map(|value| i64::try_from(value).unwrap_or(i64::MAX));
            let insert_stmt = format!(
                "
                INSERT INTO {qualified_retrieval_traces_table} (
                    trace_id, workspace_id, owner_id, session_key, agent_id, persona_id,
                    source, query, candidate_count, selected_count, dropped_count,
                    budget_tokens, selected_json, dropped_json, payload_json, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
                ON CONFLICT (trace_id) DO UPDATE SET
                    workspace_id = EXCLUDED.workspace_id,
                    owner_id = EXCLUDED.owner_id,
                    session_key = EXCLUDED.session_key,
                    agent_id = EXCLUDED.agent_id,
                    persona_id = EXCLUDED.persona_id,
                    source = EXCLUDED.source,
                    query = EXCLUDED.query,
                    candidate_count = EXCLUDED.candidate_count,
                    selected_count = EXCLUDED.selected_count,
                    dropped_count = EXCLUDED.dropped_count,
                    budget_tokens = EXCLUDED.budget_tokens,
                    selected_json = EXCLUDED.selected_json,
                    dropped_json = EXCLUDED.dropped_json,
                    payload_json = EXCLUDED.payload_json
                "
            );
            let select_stmt = format!(
                "
                SELECT id, trace_id, workspace_id, owner_id, session_key, agent_id,
                       persona_id, source, query, candidate_count, selected_count,
                       dropped_count, budget_tokens, selected_json, dropped_json,
                       payload_json, created_at
                FROM {qualified_retrieval_traces_table}
                WHERE trace_id = $1
                LIMIT 1
                "
            );
            client.with_client(|client| {
                client.execute(
                    &insert_stmt,
                    &[
                        &trace_id,
                        &input.workspace_id,
                        &input.owner_id,
                        &input.session_key,
                        &input.agent_id,
                        &input.persona_id,
                        &input.source,
                        &input.query,
                        &candidate_count,
                        &selected_count,
                        &dropped_count,
                        &budget_tokens,
                        &input.selected_json,
                        &input.dropped_json,
                        &input.payload_json,
                        &now,
                    ],
                )?;
                let row = client.query_one(&select_stmt, &[&trace_id])?;
                Self::row_to_retrieval_trace(&row)
            })
        })
        .await?
    }

    async fn append_compaction_run(&self, input: CompactionRunInput) -> Result<CompactionRun> {
        input.validate_source_event_provenance()?;
        let client = self.client.clone();
        let qualified_compaction_runs_table = self.qualified_compaction_runs_table.clone();
        let run_id = input.run_id.unwrap_or_else(|| Uuid::new_v4().to_string());

        tokio::task::spawn_blocking(move || -> Result<CompactionRun> {
            let now = Utc::now();
            let source_message_count = i64::try_from(input.source_message_count).unwrap_or(i64::MAX);
            let source_token_estimate = i64::try_from(input.source_token_estimate).unwrap_or(i64::MAX);
            let insert_stmt = format!(
                "
                INSERT INTO {qualified_compaction_runs_table} (
                    run_id, workspace_id, owner_id, session_key, agent_id, persona_id,
                    trigger, mode, source_message_count, source_token_estimate,
                    summary, summary_memory_key, source_event_ids_json,
                    source_event_range_json, source_document_refs_json,
                    fidelity_status, payload_json, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
                ON CONFLICT (run_id) DO UPDATE SET
                    workspace_id = EXCLUDED.workspace_id,
                    owner_id = EXCLUDED.owner_id,
                    session_key = EXCLUDED.session_key,
                    agent_id = EXCLUDED.agent_id,
                    persona_id = EXCLUDED.persona_id,
                    trigger = EXCLUDED.trigger,
                    mode = EXCLUDED.mode,
                    source_message_count = EXCLUDED.source_message_count,
                    source_token_estimate = EXCLUDED.source_token_estimate,
                    summary = EXCLUDED.summary,
                    summary_memory_key = EXCLUDED.summary_memory_key,
                    source_event_ids_json = EXCLUDED.source_event_ids_json,
                    source_event_range_json = EXCLUDED.source_event_range_json,
                    source_document_refs_json = EXCLUDED.source_document_refs_json,
                    fidelity_status = EXCLUDED.fidelity_status,
                    payload_json = EXCLUDED.payload_json
                "
            );
            let select_stmt = format!(
                "
                SELECT id, run_id, workspace_id, owner_id, session_key, agent_id,
                       persona_id, trigger, mode, source_message_count,
                       source_token_estimate, summary, summary_memory_key,
                       source_event_ids_json, source_event_range_json,
                       source_document_refs_json, fidelity_status, payload_json,
                       created_at
                FROM {qualified_compaction_runs_table}
                WHERE run_id = $1
                LIMIT 1
                "
            );
            client.with_client(|client| {
                client.execute(
                    &insert_stmt,
                    &[
                        &run_id,
                        &input.workspace_id,
                        &input.owner_id,
                        &input.session_key,
                        &input.agent_id,
                        &input.persona_id,
                        &input.trigger,
                        &input.mode,
                        &source_message_count,
                        &source_token_estimate,
                        &input.summary,
                        &input.summary_memory_key,
                        &input.source_event_ids_json,
                        &input.source_event_range_json,
                        &input.source_document_refs_json,
                        &input.fidelity_status,
                        &input.payload_json,
                        &now,
                    ],
                )?;
                let row = client.query_one(&select_stmt, &[&run_id])?;
                Self::row_to_compaction_run(&row)
            })
        })
        .await?
    }

    async fn reindex(&self) -> Result<usize> {
        if self.embedder.dimensions() == 0 {
            return Ok(0);
        }

        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let entries: Vec<(String, String)> = tokio::task::spawn_blocking(move || -> Result<Vec<(String, String)>> {
            let stmt = format!(
                "
                SELECT id, content
                FROM {qualified_table}
                WHERE category NOT IN ('daily', 'conversation')
                  AND (
                      embedding IS NULL
                      OR embedding_provider IS NULL
                      OR embedding_model IS NULL
                      OR embedding_dimensions IS NULL
                      OR embedding_provider != $1
                      OR embedding_model != $2
                      OR embedding_dimensions != $3
                  )
                "
            );
            client.with_client(|client| {
                let rows = client.query(&stmt, &[&provider_name, &model_name, &dimensions])?;
                Ok(rows
                    .iter()
                    .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
                    .collect())
            })
        })
        .await??;

        let mut count = 0usize;
        for (id, content) in entries {
            if let Some(embedding) = self.get_or_compute_embedding(&content, None, None, true).await? {
                let embedding_bytes = vector::vec_to_bytes(&embedding);
                let embedding_vector = self
                    .pgvector_available
                    .then(|| Self::pgvector_literal(&embedding))
                    .flatten();
                let provider_name = self.embedding_provider_name();
                let model_name = self.embedding_model_name();
                let dimensions = self.embedding_dimensions_i64();
                let client = self.client.clone();
                let qualified_table = self.qualified_table.clone();
                let pgvector_available = self.pgvector_available;
                tokio::task::spawn_blocking(move || -> Result<()> {
                    let stmt = format!(
                        "
                        UPDATE {qualified_table}
                        SET embedding = $1,
                            embedding_provider = $2,
                            embedding_model = $3,
                            embedding_dimensions = $4
                        WHERE id = $5
                        "
                    );
                    client.with_client(|client| {
                        client.execute(
                            &stmt,
                            &[&embedding_bytes, &provider_name, &model_name, &dimensions, &id],
                        )?;
                        if pgvector_available {
                            let vector_stmt = format!(
                                "UPDATE {qualified_table} SET embedding_vector = $1::text::vector WHERE id = $2"
                            );
                            if let Some(embedding_vector) = embedding_vector.as_ref() {
                                client.execute(&vector_stmt, &[embedding_vector, &id])?;
                            }
                        }
                        Ok(())
                    })
                })
                .await??;
                count += 1;
            }
        }

        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();
        let client = self.client.clone();
        let qualified_document_chunks_table = self.qualified_document_chunks_table.clone();
        let chunks: Vec<(String, String)> = tokio::task::spawn_blocking(move || -> Result<Vec<(String, String)>> {
            let stmt = format!(
                "
                SELECT chunk_id, content
                FROM {qualified_document_chunks_table}
                WHERE embedding IS NULL
                   OR embedding_provider IS NULL
                   OR embedding_model IS NULL
                   OR embedding_dimensions IS NULL
                   OR embedding_provider != $1
                   OR embedding_model != $2
                   OR embedding_dimensions != $3
                "
            );
            client.with_client(|client| {
                let rows = client.query(&stmt, &[&provider_name, &model_name, &dimensions])?;
                Ok(rows
                    .iter()
                    .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
                    .collect())
            })
        })
        .await??;

        for (chunk_id, content) in chunks {
            if let Some(embedding) = self.get_or_compute_embedding(&content, None, None, true).await? {
                let embedding_bytes = vector::vec_to_bytes(&embedding);
                let embedding_vector = self
                    .pgvector_available
                    .then(|| Self::pgvector_literal(&embedding))
                    .flatten();
                let provider_name = self.embedding_provider_name();
                let model_name = self.embedding_model_name();
                let dimensions = self.embedding_dimensions_i64();
                let client = self.client.clone();
                let qualified_document_chunks_table = self.qualified_document_chunks_table.clone();
                let pgvector_available = self.pgvector_available;
                tokio::task::spawn_blocking(move || -> Result<()> {
                    let stmt = format!(
                        "
                        UPDATE {qualified_document_chunks_table}
                        SET embedding = $1,
                            embedding_provider = $2,
                            embedding_model = $3,
                            embedding_dimensions = $4
                        WHERE chunk_id = $5
                        "
                    );
                    client.with_client(|client| {
                        client.execute(
                            &stmt,
                            &[&embedding_bytes, &provider_name, &model_name, &dimensions, &chunk_id],
                        )?;
                        if pgvector_available {
                            let vector_stmt = format!(
                                "UPDATE {qualified_document_chunks_table} SET embedding_vector = $1::text::vector WHERE chunk_id = $2"
                            );
                            if let Some(embedding_vector) = embedding_vector.as_ref() {
                                client.execute(&vector_stmt, &[embedding_vector, &chunk_id])?;
                            }
                        }
                        Ok(())
                    })
                })
                .await??;
                count += 1;
            }
        }

        Ok(count)
    }

    async fn health_check(&self) -> bool {
        let client = self.client.clone();
        tokio::task::spawn_blocking(move || client.with_client(|client| Ok(client.simple_query("SELECT 1").is_ok())))
            .await
            .unwrap_or(Ok(false))
            .unwrap_or(false)
    }
}

/// URL of the live PostgreSQL that the crate's `#[ignore]`d PostgreSQL tests
/// need (memory, cron, webhook, schema migration and fitness CLI).
///
/// Those tests used to `return` early when `OPENPRX_TEST_POSTGRES_URL` was
/// unset and so reported a skip as a pass. They are now ignored by default
/// (counted as ignored, never as passed) and run only on request, e.g.
/// `OPENPRX_TEST_POSTGRES_URL=postgres://... cargo test --all-features --lib
/// -- --ignored postgres`. Asking for them without the URL is a failure, not a
/// silent pass. This is the only place in the crate that reads the variable.
#[cfg(test)]
pub(crate) fn live_postgres_url() -> String {
    std::env::var("OPENPRX_TEST_POSTGRES_URL").expect(
        "test: OPENPRX_TEST_POSTGRES_URL must point at a disposable PostgreSQL \
         database when the live PostgreSQL tests are run with --ignored",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct CountingEmbedding {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl embeddings::EmbeddingProvider for CountingEmbedding {
        fn name(&self) -> &str {
            "counting"
        }

        fn dimensions(&self) -> usize {
            3
        }

        fn model(&self) -> &str {
            "counting-v1"
        }

        async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(texts.len(), Ordering::SeqCst);
            Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
        }
    }

    #[test]
    fn valid_identifiers_pass_validation() {
        assert!(validate_identifier("public", "schema").is_ok());
        assert!(validate_identifier("_memories_01", "table").is_ok());
    }

    #[test]
    fn invalid_identifiers_are_rejected() {
        assert!(validate_identifier("", "schema").is_err());
        assert!(validate_identifier("1bad", "schema").is_err());
        assert!(validate_identifier("bad-name", "table").is_err());
    }

    #[test]
    fn identifier_length_limit_enforced() {
        // Exactly 63 chars — valid
        let max_len = "a".repeat(63);
        assert!(validate_identifier(&max_len, "table").is_ok());

        // 64 chars — exceeds PostgreSQL NAMEDATALEN-1 limit
        let too_long = "a".repeat(64);
        assert!(validate_identifier(&too_long, "table").is_err());

        // SQL injection attempts
        assert!(validate_identifier("table; DROP TABLE users", "table").is_err());
        assert!(validate_identifier("table\"--", "table").is_err());
        assert!(validate_identifier("table\x00name", "table").is_err());
    }

    #[test]
    fn related_table_name_adds_suffix_with_identifier_limit() {
        assert_eq!(related_table_name("memories", "_drafts").unwrap(), "memories_drafts");

        let max_len = "a".repeat(63);
        let related = related_table_name(&max_len, "_drafts").unwrap();
        assert_eq!(related.len(), 63);
        assert!(related.ends_with("_drafts"));
    }

    #[test]
    fn parse_category_maps_known_and_custom_values() {
        assert_eq!(PostgresMemory::parse_category("core"), MemoryCategory::Core);
        assert_eq!(PostgresMemory::parse_category("daily"), MemoryCategory::Daily);
        assert_eq!(
            PostgresMemory::parse_category("conversation"),
            MemoryCategory::Conversation
        );
        assert_eq!(
            PostgresMemory::parse_category("custom_notes"),
            MemoryCategory::Custom("custom_notes".into())
        );
    }

    #[test]
    fn pgvector_literal_rejects_empty_or_non_finite_vectors() {
        assert_eq!(
            PostgresMemory::pgvector_literal(&[0.1, 0.2, 0.3]).as_deref(),
            Some("[0.1,0.2,0.3]")
        );
        assert!(PostgresMemory::pgvector_literal(&[]).is_none());
        assert!(PostgresMemory::pgvector_literal(&[f32::NAN]).is_none());
        assert!(PostgresMemory::pgvector_literal(&[f32::INFINITY]).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn new_does_not_panic_inside_tokio_runtime() {
        let outcome = std::panic::catch_unwind(|| {
            PostgresMemory::new(
                "postgres://openprx:password@127.0.0.1:1/openprx",
                "public",
                "memories",
                Some(1),
            )
        });

        assert!(outcome.is_ok(), "PostgresMemory::new should not panic");
        assert!(
            outcome.unwrap().is_err(),
            "PostgresMemory::new should return a connect error for an unreachable endpoint"
        );
    }

    fn fitness_message_input(workspace_id: &str, content: &str) -> MessageEventInput {
        MessageEventInput {
            event_id: Some(format!("fitness-event-{}", Uuid::new_v4())),
            idempotency_key: None,
            workspace_id: workspace_id.to_string(),
            owner_id: None,
            source: crate::memory::MessageEventSource::Other("fitness-postgres-test".to_string()),
            channel: None,
            session_key: Some("fitness-session".to_string()),
            parent_session_key: None,
            run_id: None,
            parent_run_id: None,
            agent_id: None,
            persona_id: None,
            sender: None,
            recipient: None,
            role: "system".to_string(),
            event_type: "turn.finalized".to_string(),
            subject: None,
            goal_id: None,
            causation_event_id: None,
            correlation_id: None,
            attempt_id: None,
            lease_epoch: None,
            config_generation_id: None,
            config_source_revision: None,
            content: content.to_string(),
            raw_payload_json: Some(r#"{"status":"completed"}"#.to_string()),
            visibility: MemoryVisibility::Workspace,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_time_range_query_includes_only_configured_worker_descendants_from_env() {
        let db_url = live_postgres_url();
        let schema = format!("prx_fitness_events_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
        for (workspace, content) in [
            ("/workspace", "root event"),
            ("/workspace/workers/run-1", "worker event"),
            ("/workspace/workers-old/run-2", "prefix sibling hidden"),
            ("/other/workers/run-3", "other workspace hidden"),
        ] {
            mem.append_message_event(fitness_message_input(workspace, content))
                .await
                .unwrap();
        }
        let principal = MemoryPrincipal {
            workspace_id: "/workspace".to_string(),
            agent_id: Some("self_system".to_string()),
            ..MemoryPrincipal::default()
        };
        let start = (Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let end = (Utc::now() + chrono::Duration::minutes(1)).to_rfc3339();

        let visible = mem
            .list_message_events_time_range(&principal, Some("/workspace/workers"), &start, &end, 20)
            .await
            .unwrap();
        let contents = visible.iter().map(|event| event.content.as_str()).collect::<Vec<_>>();
        assert_eq!(contents, vec!["root event", "worker event"]);

        let non_system = MemoryPrincipal {
            agent_id: Some("regular-agent".to_string()),
            ..principal
        };
        assert!(
            mem.list_message_events_time_range(&non_system, Some("/workspace/workers"), &start, &end, 20)
                .await
                .is_err()
        );

        drop(mem);
        tokio::task::spawn_blocking(move || {
            let mut client = db_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
            client
                .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_identifier(&schema)))
                .unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_fitness_prefix_migration_sql_is_exact_and_transactional_from_env() {
        let db_url = live_postgres_url();
        let schema = format!("prx_fitness_migration_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
        let table = mem.qualified_table.clone();
        let insert_url = db_url.clone();
        tokio::task::spawn_blocking(move || {
            let mut client = insert_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
            let mut transaction = client.transaction().unwrap();
            transaction
                .execute("SELECT set_config('prx.rls_bypass', 'on', true)", &[])
                .unwrap();
            for (id, key) in [
                ("legacy-one", "self/fitness/daily/2026-09-10"),
                ("legacy-two", "self/fitness/daily/2026-09-11"),
                ("keep", "self/fitness/daily_extra/keep"),
            ] {
                transaction
                    .execute(
                        &format!(
                            "INSERT INTO {table} (id, key, content, category, created_at, updated_at) \
                             VALUES ($1, $2, 'payload', 'core', NOW(), NOW())"
                        ),
                        &[&id, &key],
                    )
                    .unwrap();
            }
            transaction.commit().unwrap();
        })
        .await
        .unwrap();

        let query_url = db_url.clone();
        let query_schema = schema.clone();
        let rows = tokio::task::spawn_blocking(move || {
            PostgresMemory::read_key_prefix_read_only(
                &query_url,
                &query_schema,
                "memories",
                Some(5),
                "self/fitness/daily/",
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
            vec!["self/fitness/daily/2026-09-10", "self/fitness/daily/2026-09-11"]
        );
        let delete_url = db_url.clone();
        let delete_schema = schema.clone();
        let deleted = tokio::task::spawn_blocking(move || {
            PostgresMemory::delete_key_prefix_transactional(
                &delete_url,
                &delete_schema,
                "memories",
                Some(5),
                "self/fitness/daily/",
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(deleted, 2);
        let verify_url = db_url.clone();
        let verify_schema = schema.clone();
        let remaining = tokio::task::spawn_blocking(move || {
            PostgresMemory::read_key_prefix_read_only(
                &verify_url,
                &verify_schema,
                "memories",
                Some(5),
                "self/fitness/daily/",
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert!(remaining.is_empty(), "expected an empty value");
        assert!(mem.get("self/fitness/daily_extra/keep").await.unwrap().is_some());

        drop(mem);
        tokio::task::spawn_blocking(move || {
            let mut client = db_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
            client
                .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_identifier(&schema)))
                .unwrap();
        })
        .await
        .unwrap();
    }

    /// Exercises the connection pool against a live PostgreSQL instance:
    /// concurrent checkouts genuinely overlap, a saturated pool makes callers
    /// queue instead of fail, and a connection killed server-side is replaced
    /// transparently on the next use.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_connection_pool_saturates_and_recovers_from_env() {
        let db_url = live_postgres_url();
        let schema = format!("prx_pool_{}", Uuid::new_v4().simple());
        const POOL_SIZE: usize = 4;
        const CONCURRENCY: usize = 24;

        let mem = Arc::new(
            PostgresMemory::with_embedder_and_cache(
                &db_url,
                &schema,
                "memories",
                Some(5),
                Arc::new(embeddings::NoopEmbedding),
                0.35,
                0.65,
                256,
                POOL_SIZE,
            )
            .unwrap(),
        );

        // Far more concurrent work than connections: every call must complete.
        let mut tasks = Vec::new();
        for index in 0..CONCURRENCY {
            let mem = Arc::clone(&mem);
            tasks.push(tokio::spawn(async move {
                mem.store(
                    &format!("pool-key-{index}"),
                    "pool contention probe",
                    MemoryCategory::Core,
                    None,
                )
                .await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }

        let stats = mem.pool_stats();
        assert_eq!(stats.max_size, POOL_SIZE, "pool must honour the configured size");
        assert!(
            stats.connects <= POOL_SIZE as u64,
            "pool must never open more connections than max_size, opened {}",
            stats.connects
        );
        assert!(stats.idle <= POOL_SIZE, "idle connections must stay within max_size");
        assert_eq!(stats.in_use, 0, "every checkout must be returned");
        assert!(
            stats.acquisitions >= CONCURRENCY as u64,
            "every operation must have checked a connection out"
        );
        assert!(
            stats.connects > 1,
            "concurrent load must open more than one connection, opened {}",
            stats.connects
        );
        assert!(
            stats.saturated_acquisitions > 0,
            "a pool of {POOL_SIZE} under {CONCURRENCY}-way load must have queued callers"
        );
        assert!(
            stats.total_wait > Duration::ZERO,
            "queued callers must record their wait time"
        );

        // Kill every pooled backend server-side, then prove the next call still
        // succeeds: stale connections are discarded and replaced, not surfaced.
        let kill_url = db_url.clone();
        let kill_schema = schema.clone();
        tokio::task::spawn_blocking(move || {
            let mut admin = kill_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
            admin
                .execute(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE pid <> pg_backend_pid() AND application_name IS NOT NULL \
                     AND query LIKE $1",
                    &[&format!("%{kill_schema}%")],
                )
                .unwrap();
        })
        .await
        .unwrap();

        let discards_before = mem.pool_stats().discards;
        let recovered = mem.get("pool-key-0").await.unwrap();
        assert!(
            recovered.is_some(),
            "pool must transparently reconnect after the server closes connections"
        );
        let stats = mem.pool_stats();
        assert_eq!(stats.in_use, 0, "recovery must not leak a checkout slot");
        assert!(stats.discards >= discards_before, "discard counter must not regress");

        let cleanup_url = db_url.clone();
        let cleanup_schema = schema.clone();
        drop(mem);
        tokio::task::spawn_blocking(move || {
            let mut client = cleanup_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
            client
                .batch_execute(&format!(
                    "DROP SCHEMA IF EXISTS {} CASCADE",
                    quote_identifier(&cleanup_schema)
                ))
                .unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_memory_fabric_conformance_from_env() {
        let db_url = live_postgres_url();
        let schema = format!("prx_test_{}", Uuid::new_v4().simple());
        let table = "memories";
        let mem = PostgresMemory::new(&db_url, &schema, table, Some(5)).unwrap();

        let user = mem
            .append_message_event(MessageEventInput {
                event_id: Some("event-user-1".to_string()),
                idempotency_key: Some("idem-user-1".to_string()),
                workspace_id: "workspace-a".to_string(),
                owner_id: Some("owner-a".to_string()),
                source: "postgres-test".into(),
                channel: Some("telegram".to_string()),
                session_key: Some("telegram_sender-1".to_string()),
                parent_session_key: None,
                run_id: Some("run-1".to_string()),
                parent_run_id: None,
                agent_id: Some("agent-a".to_string()),
                persona_id: Some("persona-a".to_string()),
                sender: Some("sender-1".to_string()),
                recipient: Some("prx".to_string()),
                role: "user".to_string(),
                event_type: "message.created".to_string(),
                subject: Some(crate::memory::MessageEventSubject::Task("task-pg-1".to_string())),
                goal_id: Some("goal-pg-1".to_string()),
                causation_event_id: Some("event-parent-pg".to_string()),
                correlation_id: Some("correlation-pg-1".to_string()),
                attempt_id: Some("attempt-pg-2".to_string()),
                lease_epoch: Some(3),
                config_generation_id: Some(42),
                config_source_revision: Some("sha256:postgres-test".to_string()),
                content: "hello from postgres".to_string(),
                raw_payload_json: None,
                visibility: MemoryVisibility::Workspace,
            })
            .await
            .unwrap();
        let duplicate = mem
            .append_message_event(MessageEventInput {
                event_id: Some("event-user-duplicate".to_string()),
                idempotency_key: Some("idem-user-1".to_string()),
                workspace_id: "workspace-a".to_string(),
                owner_id: Some("owner-a".to_string()),
                source: "postgres-test".into(),
                channel: Some("telegram".to_string()),
                session_key: Some("telegram_sender-1".to_string()),
                parent_session_key: None,
                run_id: Some("run-1".to_string()),
                parent_run_id: None,
                agent_id: Some("agent-a".to_string()),
                persona_id: Some("persona-a".to_string()),
                sender: Some("sender-1".to_string()),
                recipient: Some("prx".to_string()),
                role: "user".to_string(),
                event_type: "message.created".to_string(),
                subject: None,
                goal_id: None,
                causation_event_id: None,
                correlation_id: None,
                attempt_id: None,
                lease_epoch: None,
                config_generation_id: Some(0),
                config_source_revision: None,
                content: "duplicate should not replace".to_string(),
                raw_payload_json: None,
                visibility: MemoryVisibility::Workspace,
            })
            .await
            .unwrap();
        assert_eq!(duplicate.event_id, user.event_id);
        assert_eq!(duplicate.content, user.content);
        let other_workspace = mem
            .append_message_event(MessageEventInput {
                event_id: Some("event-user-workspace-b".to_string()),
                idempotency_key: Some("idem-user-1".to_string()),
                workspace_id: "workspace-b".to_string(),
                owner_id: Some("owner-b".to_string()),
                source: "postgres-test".into(),
                channel: Some("telegram".to_string()),
                session_key: Some("telegram_sender-1".to_string()),
                parent_session_key: None,
                run_id: Some("run-b".to_string()),
                parent_run_id: None,
                agent_id: Some("agent-b".to_string()),
                persona_id: Some("persona-b".to_string()),
                sender: Some("sender-b".to_string()),
                recipient: Some("prx".to_string()),
                role: "user".to_string(),
                event_type: "message.created".to_string(),
                subject: None,
                goal_id: None,
                causation_event_id: None,
                correlation_id: None,
                attempt_id: None,
                lease_epoch: None,
                config_generation_id: Some(0),
                config_source_revision: None,
                content: "same idempotency key in another workspace".to_string(),
                raw_payload_json: None,
                visibility: MemoryVisibility::Workspace,
            })
            .await
            .unwrap();
        assert_eq!(other_workspace.workspace_id, "workspace-b");
        assert_ne!(other_workspace.event_id, user.event_id);
        assert_eq!(user.event_type, "message.created");
        assert_eq!(user.source, "postgres-test");
        assert_eq!(
            user.subject,
            Some(crate::memory::MessageEventSubject::Task("task-pg-1".to_string()))
        );
        assert_eq!(user.goal_id.as_deref(), Some("goal-pg-1"));
        assert_eq!(user.causation_event_id.as_deref(), Some("event-parent-pg"));
        assert_eq!(user.correlation_id.as_deref(), Some("correlation-pg-1"));
        assert_eq!(user.attempt_id.as_deref(), Some("attempt-pg-2"));
        assert_eq!(user.lease_epoch, Some(3));
        assert_eq!(user.config_generation_id, Some(42));
        assert_eq!(user.config_source_revision.as_deref(), Some("sha256:postgres-test"));

        let assistant = mem
            .append_message_event(MessageEventInput {
                event_id: Some("event-assistant-1".to_string()),
                idempotency_key: None,
                workspace_id: "workspace-a".to_string(),
                owner_id: Some("owner-a".to_string()),
                source: "postgres-test".into(),
                channel: Some("telegram".to_string()),
                session_key: Some("telegram_sender-1".to_string()),
                parent_session_key: None,
                run_id: Some("run-1".to_string()),
                parent_run_id: None,
                agent_id: Some("agent-a".to_string()),
                persona_id: Some("persona-a".to_string()),
                sender: Some("prx".to_string()),
                recipient: Some("sender-1".to_string()),
                role: "assistant".to_string(),
                event_type: "message.created".to_string(),
                subject: None,
                goal_id: None,
                causation_event_id: None,
                correlation_id: None,
                attempt_id: None,
                lease_epoch: None,
                config_generation_id: Some(0),
                config_source_revision: None,
                content: "postgres reply".to_string(),
                raw_payload_json: None,
                visibility: MemoryVisibility::Workspace,
            })
            .await
            .unwrap();

        let principal = MemoryPrincipal {
            require_owner_match: false,
            workspace_id: "workspace-a".to_string(),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            session_key: Some("telegram_sender-1".to_string()),
            channel: Some("telegram".to_string()),
            sender: Some("sender-1".to_string()),
            owner_id: Some("owner-a".to_string()),
            legacy_session_key: None,
        };
        let events = mem.list_message_events_since(&principal, 0, 10).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events.first().map(|event| event.event_id.as_str()),
            Some("event-user-1")
        );
        assert_eq!(
            events.get(1).map(|event| event.event_id.as_str()),
            Some("event-assistant-1")
        );
        let recent_events = mem.list_message_events_recent(&principal, 10).await.unwrap();
        assert_eq!(recent_events.len(), 2);
        assert_eq!(
            recent_events.first().map(|event| event.event_id.as_str()),
            Some("event-assistant-1")
        );

        let shared = mem
            .load_recent_shared_context(SharedContextQuery {
                principal: principal.clone(),
                since_event_id: None,
                limit: 10,
                include_roles: vec!["assistant".to_string()],
            })
            .await
            .unwrap();
        assert_eq!(shared.len(), 1);
        assert_eq!(
            shared.first().map(|event| event.event_id.as_str()),
            Some(assistant.event_id.as_str())
        );

        let session = mem
            .load_recent_session_context(SessionContextQuery {
                principal: principal.clone(),
                since_event_id: Some(user.id),
                limit: 10,
                include_roles: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(session.len(), 1);
        assert_eq!(
            session.first().map(|event| event.event_id.as_str()),
            Some("event-assistant-1")
        );

        let outbox_count = mem
            .list_memory_events_since(&principal, 0, 10)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.subject_table == "message_events")
            .count();
        assert_eq!(outbox_count, 2);

        let custom_event = mem
            .append_memory_event(MemoryEventInput {
                owner_id: None,
                event_id: Some("memory-event-1".to_string()),
                workspace_id: "workspace-a".to_string(),
                event_type: "memory.custom".to_string(),
                subject_table: "memories".to_string(),
                subject_id: "subject-1".to_string(),
                session_key: Some("telegram_sender-1".to_string()),
                run_id: None,
                parent_run_id: None,
                agent_id: Some("agent-a".to_string()),
                persona_id: Some("persona-a".to_string()),
                visibility: MemoryVisibility::Workspace,
                payload_json: Some("{\"ok\":true}".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(custom_event.event_type, "memory.custom");
        let recent_outbox = mem.list_memory_events_recent(&principal, 10).await.unwrap();
        assert_eq!(
            recent_outbox.first().map(|event| event.event_id.as_str()),
            Some("memory-event-1")
        );

        let draft = mem
            .create_memory_draft(MemoryDraftInput {
                draft_id: Some("draft-1".to_string()),
                workspace_id: "workspace-a".to_string(),
                owner_id: Some("owner-a".to_string()),
                worker_run_id: "worker-run-1".to_string(),
                parent_run_id: Some("parent-run-1".to_string()),
                session_key: Some("telegram_sender-1".to_string()),
                agent_id: Some("agent-a".to_string()),
                persona_id: Some("persona-a".to_string()),
                key: "draft-key".to_string(),
                content: "draft content".to_string(),
                category: MemoryCategory::Conversation,
                source_event_id: Some(user.event_id.clone()),
                visibility: MemoryVisibility::Private,
                payload_json: None,
            })
            .await
            .unwrap();
        assert_eq!(draft.status, "pending");
        let test_principal = MemoryPrincipal {
            require_owner_match: false,
            workspace_id: "workspace".to_string(),
            agent_id: Some("system".to_string()),
            persona_id: None,
            session_key: None,
            channel: None,
            sender: None,
            owner_id: None,
            legacy_session_key: None,
        };
        let merged = mem
            .merge_memory_draft(&test_principal, "draft-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(merged.status, "merged");
        let stored = mem.get("draft-key").await.unwrap().unwrap();
        assert_eq!(stored.content, "draft content");

        let rejected = mem
            .create_memory_draft(MemoryDraftInput {
                draft_id: Some("draft-2".to_string()),
                workspace_id: "workspace-a".to_string(),
                owner_id: Some("owner-a".to_string()),
                worker_run_id: "worker-run-1".to_string(),
                parent_run_id: Some("parent-run-1".to_string()),
                session_key: Some("telegram_sender-1".to_string()),
                agent_id: Some("agent-a".to_string()),
                persona_id: Some("persona-a".to_string()),
                key: "rejected-draft-key".to_string(),
                content: "rejected draft content".to_string(),
                category: MemoryCategory::Conversation,
                source_event_id: Some(user.event_id.clone()),
                visibility: MemoryVisibility::Private,
                payload_json: None,
            })
            .await
            .unwrap();
        assert_eq!(rejected.status, "pending");
        let rejected = mem
            .reject_memory_draft(&test_principal, "draft-2", Some("not useful"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rejected.status, "rejected");

        let event_types: Vec<String> = mem
            .list_memory_events_since(&principal, 0, 50)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.event_type)
            .collect();
        assert!(event_types.contains(&"memory.draft.created".to_string()));
        assert!(event_types.contains(&"memory.draft.merged".to_string()));
        assert!(event_types.contains(&"memory.draft.rejected".to_string()));
        assert!(event_types.contains(&"memory.stored".to_string()));

        let document = mem
            .ingest_document(DocumentIngestInput {
                document_id: Some("doc-postgres-1".to_string()),
                workspace_id: "workspace-a".to_string(),
                owner_id: Some("owner-a".to_string()),
                topic_id: Some("topic-a".to_string()),
                task_id: Some("task-a".to_string()),
                source_message_event_id: Some(user.event_id.clone()),
                source_kind: "tool_output".to_string(),
                source_uri: Some("tool:file_read".to_string()),
                title: Some("Postgres Document".to_string()),
                content: "Durable postgres document fact about vector retrieval and source anchors.".to_string(),
                mime_type: Some("text/plain".to_string()),
                visibility: MemoryVisibility::Workspace,
                metadata_json: Some("{\"tool\":\"file_read\"}".to_string()),
                session_key: None,
                agent_id: None,
                persona_id: None,
            })
            .await
            .unwrap();
        assert_eq!(document.document_id, "doc-postgres-1");
        assert_eq!(document.chunk_count, 1);
        assert_eq!(document.content_sha256.len(), 64);

        let document_results = mem
            .search_document_chunks(&principal, "vector retrieval", 10)
            .await
            .unwrap();
        assert_eq!(document_results.len(), 1);
        let document_result = document_results.first().expect("document result should exist");
        assert_eq!(document_result.chunk.document_id, "doc-postgres-1");
        assert_eq!(document_result.chunk.source_anchor, "doc-postgres-1#chunk-0");

        let chunk = mem
            .get_document_chunk(&document_result.chunk.chunk_id)
            .await
            .unwrap()
            .expect("document chunk should be retrievable");
        assert!(chunk.content.contains("source anchors"));

        let link = mem
            .link_memory_source(MemoryLinkInput {
                link_id: Some("link-postgres-1".to_string()),
                workspace_id: "workspace-a".to_string(),
                owner_id: Some("owner-a".to_string()),
                memory_key: Some("draft-key".to_string()),
                memory_event_id: None,
                message_event_id: Some(user.event_id.clone()),
                document_id: document.document_id.clone(),
                chunk_id: Some(chunk.chunk_id.clone()),
                link_type: "evidence".to_string(),
                payload_json: None,
            })
            .await
            .unwrap();
        assert_eq!(link.link_id, "link-postgres-1");
        assert_eq!(link.chunk_id.as_deref(), Some(chunk.chunk_id.as_str()));

        let trace = mem
            .append_retrieval_trace(RetrievalTraceInput {
                trace_id: Some("trace-postgres-1".to_string()),
                workspace_id: "workspace-a".to_string(),
                owner_id: Some("owner-a".to_string()),
                session_key: Some("telegram_sender-1".to_string()),
                agent_id: Some("agent-a".to_string()),
                persona_id: Some("persona-a".to_string()),
                source: "agent_context.document_evidence".to_string(),
                query: "vector retrieval".to_string(),
                candidate_count: document_results.len(),
                selected_count: 1,
                dropped_count: 0,
                budget_tokens: Some(512),
                selected_json: Some(format!(r#"[{{"chunk_id":"{}"}}]"#, chunk.chunk_id)),
                dropped_json: Some("[]".to_string()),
                payload_json: Some(r#"{"phase":"postgres_conformance"}"#.to_string()),
            })
            .await
            .unwrap();
        assert_eq!(trace.trace_id, "trace-postgres-1");
        assert_eq!(trace.source, "agent_context.document_evidence");
        assert!(trace.selected_json.unwrap().contains("doc-postgres-1:chunk:0"));

        let compaction = mem
            .append_compaction_run(CompactionRunInput {
                run_id: Some("compact-postgres-1".to_string()),
                workspace_id: "workspace-a".to_string(),
                owner_id: Some("owner-a".to_string()),
                session_key: Some("telegram_sender-1".to_string()),
                agent_id: Some("agent-a".to_string()),
                persona_id: Some("persona-a".to_string()),
                trigger: "overflow_retry".to_string(),
                mode: "safeguard".to_string(),
                source_message_count: 3,
                source_token_estimate: 512,
                summary: "## Decisions\n- preserve source anchors".to_string(),
                summary_memory_key: Some("compaction_summary_pg".to_string()),
                source_event_ids_json: Some(format!(r#"["{}"]"#, user.event_id)),
                source_event_range_json: Some(
                    serde_json::json!({
                        "first_event_id": user.event_id,
                        "last_event_id": user.event_id,
                        "first_row_id": user.id,
                        "last_row_id": user.id,
                        "source_event_count": 1
                    })
                    .to_string(),
                ),
                source_document_refs_json: Some(format!(r#"[{{"chunk_id":"{}"}}]"#, chunk.chunk_id)),
                fidelity_status: "accepted".to_string(),
                payload_json: Some(r#"{"phase":"postgres_conformance"}"#.to_string()),
            })
            .await
            .unwrap();
        assert_eq!(compaction.run_id, "compact-postgres-1");
        assert_eq!(compaction.summary_memory_key.as_deref(), Some("compaction_summary_pg"));
        assert!(
            compaction
                .source_event_range_json
                .as_deref()
                .is_some_and(|json| json.contains(&user.event_id))
        );
        assert_eq!(compaction.fidelity_status, "accepted");

        crate::memory::traits::conformance::assert_scoped_memory_acl_conformance(&mem, "postgres-scoped-conformance")
            .await;

        drop(mem);
        tokio::task::spawn_blocking(move || {
            let mut client = db_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
            client
                .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_identifier(&schema)))
                .unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_embedding_vector_reindex_from_env() {
        let db_url = live_postgres_url();
        let schema = format!("prx_vector_test_{}", Uuid::new_v4().simple());
        let table = "memories";
        let calls = Arc::new(AtomicUsize::new(0));
        let mem = PostgresMemory::with_embedder(
            &db_url,
            &schema,
            table,
            Some(5),
            Arc::new(CountingEmbedding {
                calls: Arc::clone(&calls),
            }),
            1.0,
            0.0,
        )
        .unwrap();

        mem.store("core-vector", "alpha beta gamma", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("core-vector-copy", "alpha beta gamma", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("daily-vector", "daily alpha beta", MemoryCategory::Daily, None)
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "Postgres should cache repeated durable core/custom memory embeddings"
        );

        let vector_results = mem.recall("semantic-neighbor", 5, None).await.unwrap();
        assert!(
            vector_results.iter().any(|entry| entry.key == "core-vector"),
            "Postgres memory recall should use app-level vector scan when lexical search has no match"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "Postgres should compute the first query embedding"
        );
        let _ = mem.recall("semantic-neighbor", 5, None).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "Postgres should reuse cached query embeddings"
        );

        mem.ingest_document(DocumentIngestInput {
            document_id: Some("doc-vector-postgres-1".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner-a".to_string()),
            topic_id: None,
            task_id: None,
            source_message_event_id: None,
            source_kind: "test".to_string(),
            source_uri: None,
            title: Some("Vector Postgres Doc".to_string()),
            content: "delta epsilon zeta".to_string(),
            mime_type: Some("text/plain".to_string()),
            visibility: MemoryVisibility::Workspace,
            metadata_json: None,
            session_key: None,
            agent_id: None,
            persona_id: None,
        })
        .await
        .unwrap();
        mem.ingest_document(DocumentIngestInput {
            document_id: Some("doc-vector-postgres-2".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner-a".to_string()),
            topic_id: None,
            task_id: None,
            source_message_event_id: None,
            source_kind: "test".to_string(),
            source_uri: None,
            title: Some("Vector Postgres Doc Copy".to_string()),
            content: "delta epsilon zeta".to_string(),
            mime_type: Some("text/plain".to_string()),
            visibility: MemoryVisibility::Workspace,
            metadata_json: None,
            session_key: None,
            agent_id: None,
            persona_id: None,
        })
        .await
        .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "Postgres should cache repeated document chunk embeddings"
        );
        let principal = MemoryPrincipal {
            require_owner_match: false,
            workspace_id: "workspace-a".to_string(),
            agent_id: None,
            persona_id: None,
            session_key: Some("chat:postgres-vector".to_string()),
            channel: Some("terminal".to_string()),
            sender: Some("local-user".to_string()),
            owner_id: Some("owner-a".to_string()),
            legacy_session_key: None,
        };
        let document_results = mem
            .search_document_chunks(&principal, "semantic-neighbor", 5)
            .await
            .unwrap();
        assert!(
            document_results
                .iter()
                .any(|result| result.chunk.document_id == "doc-vector-postgres-1"),
            "Postgres document search should use app-level vector scan when lexical search has no match"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "Postgres document search must not reuse a system-scoped query embedding across owners"
        );
        {
            let client_slot = mem.client.clone();
            let qualified_embedding_cache_table = mem.qualified_embedding_cache_table.clone();
            tokio::task::spawn_blocking(move || -> Result<()> {
                client_slot.with_client(|client| {
                    let count: i64 = client
                        .query_one(&format!("SELECT COUNT(*) FROM {qualified_embedding_cache_table}"), &[])
                        .map(|row| row.get(0))?;
                    assert!(
                        count >= 4,
                        "Postgres embedding cache should retain stored/query embeddings"
                    );
                    Ok(())
                })
            })
            .await
            .unwrap()
            .unwrap();
        }

        {
            let client_slot = mem.client.clone();
            let qualified_table = mem.qualified_table.clone();
            let qualified_document_chunks_table = mem.qualified_document_chunks_table.clone();
            tokio::task::spawn_blocking(move || -> Result<()> {
                client_slot.with_client(|client| {
                    client.execute(
                        &format!(
                            "UPDATE {} SET embedding_provider = 'stale-provider',
                         embedding_model = 'stale-model',
                         embedding_dimensions = 999
                         WHERE key = 'core-vector'",
                            qualified_table
                        ),
                        &[],
                    )?;
                    client.execute(
                        &format!(
                            "UPDATE {} SET embedding_provider = 'stale-provider',
                         embedding_model = 'stale-model',
                         embedding_dimensions = 999
                         WHERE document_id = 'doc-vector-postgres-1'",
                            qualified_document_chunks_table
                        ),
                        &[],
                    )?;
                    Ok(())
                })
            })
            .await
            .unwrap()
            .unwrap();
        }

        let stale_memory = mem.recall("semantic-neighbor", 5, None).await.unwrap();
        assert!(
            stale_memory.iter().all(|entry| entry.key != "core-vector"),
            "stale Postgres memory vector metadata must be ignored before reindex"
        );
        let stale_document = mem
            .search_document_chunks(&principal, "semantic-neighbor", 5)
            .await
            .unwrap();
        assert!(
            stale_document
                .iter()
                .all(|result| result.chunk.document_id != "doc-vector-postgres-1"),
            "stale Postgres document chunk vector metadata must be ignored before reindex"
        );

        let repaired = mem.reindex().await.unwrap();
        assert_eq!(repaired, 2);

        let restored_memory = mem.recall("semantic-neighbor", 5, None).await.unwrap();
        assert!(
            restored_memory.iter().any(|entry| entry.key == "core-vector"),
            "Postgres memory vector recall should work after reindex"
        );
        let restored_document = mem
            .search_document_chunks(&principal, "semantic-neighbor", 5)
            .await
            .unwrap();
        assert!(
            restored_document
                .iter()
                .any(|result| result.chunk.document_id == "doc-vector-postgres-1"),
            "Postgres document vector recall should work after reindex"
        );

        {
            let client_slot = mem.client.clone();
            let qualified_table = mem.qualified_table.clone();
            tokio::task::spawn_blocking(move || -> Result<()> {
                client_slot.with_client(|client| {
                    let (provider, model, dimensions): (String, String, i64) = client
                        .query_one(
                            &format!(
                                "SELECT embedding_provider, embedding_model, embedding_dimensions
                             FROM {} WHERE key = 'core-vector'",
                                qualified_table
                            ),
                            &[],
                        )
                        .map(|row| (row.get(0), row.get(1), row.get(2)))?;
                    assert_eq!(provider, "counting");
                    assert_eq!(model, "counting-v1");
                    assert_eq!(dimensions, 3);
                    Ok(())
                })
            })
            .await
            .unwrap()
            .unwrap();
        }

        drop(mem);
        tokio::task::spawn_blocking(move || {
            let mut client = db_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
            client
                .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_identifier(&schema)))
                .unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_vector_rls_denies_missing_and_cross_owner_context_from_env() {
        let db_url = live_postgres_url();
        let schema = format!("prx_vector_rls_test_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::with_embedder(
            &db_url,
            &schema,
            "memories",
            Some(5),
            Arc::new(CountingEmbedding {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            1.0,
            0.0,
        )
        .unwrap();

        let context = |workspace: &str, owner: &str| MemoryWriteContext {
            workspace_id: Some(workspace.to_string()),
            channel: Some("test".to_string()),
            chat_type: Some("private".to_string()),
            chat_id: Some(format!("chat-{owner}")),
            sender_id: Some(owner.to_string()),
            raw_sender: Some(owner.to_string()),
            owner_id: None,
            require_owner_match: false,
        };
        let metadata = |workspace: &str, owner: &str| MemoryStoreMetadata {
            workspace_id: Some(workspace.to_string()),
            owner_id: Some(owner.to_string()),
            ..MemoryStoreMetadata::default()
        };
        let owner_a = context("workspace-a", "owner-a");
        let owner_b = context("workspace-a", "owner-b");
        let other_workspace = context("workspace-b", "owner-a");

        mem.store_with_context_and_metadata(
            "vector-owner-a",
            "isolation-token shared",
            MemoryCategory::Core,
            None,
            Some(&owner_a),
            metadata("workspace-a", "owner-a"),
        )
        .await
        .unwrap();
        mem.store_with_context_and_metadata(
            "vector-owner-b",
            "isolation-token shared",
            MemoryCategory::Core,
            None,
            Some(&owner_b),
            metadata("workspace-a", "owner-b"),
        )
        .await
        .unwrap();

        let visible_a = mem
            .recall_with_context("isolation-token", 10, None, Some(&owner_a))
            .await
            .unwrap();
        assert_eq!(visible_a.len(), 1);
        assert_eq!(visible_a.first().unwrap().key, "vector-owner-a");
        let visible_b = mem
            .recall_with_context("isolation-token", 10, None, Some(&owner_b))
            .await
            .unwrap();
        assert_eq!(visible_b.len(), 1);
        assert_eq!(visible_b.first().unwrap().key, "vector-owner-b");
        assert!(
            mem.recall_with_context("isolation-token", 10, None, Some(&other_workspace))
                .await
                .unwrap()
                .is_empty()
        );

        let missing_workspace = MemoryWriteContext {
            workspace_id: None,
            ..owner_a.clone()
        };
        let missing_error = mem
            .recall_with_context("isolation-token", 10, None, Some(&missing_workspace))
            .await
            .unwrap_err();
        assert!(missing_error.to_string().contains("trusted workspace context"));

        let cache_table = mem.qualified_embedding_cache_table.clone();
        let memories_table = mem.qualified_table.clone();
        let chunks_table = mem.qualified_document_chunks_table.clone();
        let scoped_cache_rows = tokio::task::spawn_blocking({
            let client = Arc::clone(&mem.client);
            move || {
                client.with_client(|client| {
                    let count: i64 = client
                        .query_one(
                            &format!(
                                "SELECT COUNT(*) FROM {cache_table} \
                                 WHERE workspace_id = 'workspace-a' \
                                 AND owner_id IN ('owner-a', 'owner-b')"
                            ),
                            &[],
                        )?
                        .get(0);
                    for table in [&memories_table, &chunks_table, &cache_table] {
                        let row = client.query_one(
                            "SELECT c.relrowsecurity, c.relforcerowsecurity \
                             FROM pg_class c WHERE c.oid = to_regclass($1::text)",
                            &[table],
                        )?;
                        assert!(row.get::<_, bool>(0));
                        assert!(row.get::<_, bool>(1));
                    }
                    Ok(count)
                })
            }
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            scoped_cache_rows, 2,
            "identical model/cache dimensions remain owner-scoped"
        );
        let evidence = PostgresMemory::verify_vector_isolation(&db_url, &schema, "memories", Some(5)).unwrap();
        assert!(evidence.starts_with("postgres:migration:21:sha256:"));
        assert!(evidence.contains(":policy-sha256:"));

        drop(mem);
        tokio::task::spawn_blocking(move || {
            let mut client = db_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
            client
                .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_identifier(&schema)))
                .unwrap();
        })
        .await
        .unwrap();
    }

    /// The security boundary that separates trusted runtime storage from
    /// callers that ingest externally-originated data, verified against a live
    /// PostgreSQL instance under a **non-superuser** role.
    ///
    /// A superuser (or `BYPASSRLS`) role short-circuits every policy, so this
    /// test asserts up front that the configured role is neither; running it as
    /// `postgres` would pass vacuously and prove nothing.
    ///
    /// Three properties, in the order they matter:
    /// 1. the restricted context is denied a write to the RLS-protected
    ///    `memories` table -- the pre-pool boundary, reproduced;
    /// 2. the system context on the *same pool* is allowed that write -- so
    ///    property 1 is a real difference, not a broken fixture;
    /// 3. a connection released by a restricted checkout comes back with its
    ///    bypass intact for the next system checkout -- the contexts cannot
    ///    contaminate each other.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_pool_session_contexts_are_isolated_from_env() {
        let db_url = live_postgres_url();
        let schema = format!("prx_ctx_{}", Uuid::new_v4().simple());

        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
        let qualified = format!("{}.{}", quote_identifier(&schema), quote_identifier("memories"));

        // A pool of exactly one connection, so "the next checkout" is
        // necessarily the very same backend rather than a fresh one that would
        // hide a leak behind a clean session.
        let pool = Arc::new(PostgresConnectionPool::new(db_url.clone(), Some(5), 1));

        let insert_sql = format!(
            "INSERT INTO {qualified} (id, key, content, category, created_at, updated_at, \
                                      workspace_id, owner_id, visibility) \
             VALUES ($1, $2, 'ctx probe', 'conversation', NOW(), NOW(), $3, $4, 'owner')"
        );

        let probe_pool = Arc::clone(&pool);
        let probe_sql = insert_sql.clone();
        let qualified_probe = qualified.clone();
        tokio::task::spawn_blocking(move || {
            // Guard: an RLS-exempt role makes every assertion below meaningless.
            let (is_super, bypasses): (bool, bool) = probe_pool
                .with_client_in(PostgresSessionContext::System, |client| {
                    let row = client.query_one(
                        "SELECT rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user",
                        &[],
                    )?;
                    Ok((row.get(0), row.get(1)))
                })
                .unwrap();
            assert!(
                !is_super && !bypasses,
                "this test only proves anything under a NOSUPERUSER/NOBYPASSRLS role; \
                 point OPENPRX_TEST_POSTGRES_URL at an unprivileged role"
            );

            // 1. Restricted context: RLS applies, no owner context is bound, so
            //    both policy branches are false and the write is rejected.
            let restricted_backend: i32 = {
                let mut checkout = probe_pool.checkout_in(PostgresSessionContext::Restricted).unwrap();
                let client = checkout.client_mut().unwrap();

                let bypass: String = client
                    .query_one("SELECT current_setting('prx.rls_bypass', true)", &[])
                    .unwrap()
                    .get(0);
                assert_eq!(bypass, "off", "restricted checkout must not carry an RLS bypass");

                let denied = client.execute(
                    &probe_sql,
                    &[
                        &Uuid::new_v4().to_string(),
                        &format!("restricted:{}", Uuid::new_v4()),
                        &"ws-probe".to_string(),
                        &"owner-probe".to_string(),
                    ],
                );
                let error = denied.expect_err("restricted context must be denied by row-level security");
                // `postgres::Error` renders as a bare "db error", so the
                // assertion has to reach into the `DbError`. SQLSTATE 42501
                // alone is not enough: it also covers a plain `permission
                // denied for table`, which would let a mis-granted fixture
                // masquerade as a policy denial. The message pins it to RLS.
                let db_error = error
                    .as_db_error()
                    .unwrap_or_else(|| panic!("expected a PostgreSQL error, got: {error}"));
                assert_eq!(
                    db_error.code(),
                    &postgres::error::SqlState::INSUFFICIENT_PRIVILEGE,
                    "expected SQLSTATE 42501, got: {}",
                    db_error.message()
                );
                assert!(
                    db_error.message().contains("row-level security"),
                    "expected a row-level security denial, got: {}",
                    db_error.message()
                );

                client
                    .query_one("SELECT pg_backend_pid()", &[])
                    .unwrap()
                    .get::<_, i32>(0)
            };

            // 2. System context on the same pool: the write succeeds, proving
            //    the denial above is the context and not the fixture.
            //
            // 3. The connection is the one the restricted checkout just
            //    released, and its bypass is back -- no contamination in
            //    either direction.
            let system_backend: i32 = probe_pool
                .with_client_in(PostgresSessionContext::System, |client| {
                    let bypass: String = client
                        .query_one("SELECT current_setting('prx.rls_bypass', true)", &[])?
                        .get(0);
                    assert_eq!(
                        bypass, "on",
                        "a restricted checkout must not strip the bypass from the recycled connection"
                    );
                    client.execute(
                        &probe_sql,
                        &[
                            &Uuid::new_v4().to_string(),
                            &"system:probe".to_string(),
                            &"ws-probe".to_string(),
                            &"owner-probe".to_string(),
                        ],
                    )?;
                    let rows: i64 = client
                        .query_one(&format!("SELECT COUNT(*) FROM {qualified_probe}"), &[])?
                        .get(0);
                    assert_eq!(rows, 1, "only the system write landed");
                    Ok(client.query_one("SELECT pg_backend_pid()", &[])?.get::<_, i32>(0))
                })
                .unwrap();

            assert_eq!(
                restricted_backend, system_backend,
                "the cross-contamination check is only meaningful on a reused backend"
            );

            // Alternating contexts on that same single connection stay stable
            // rather than degrading after the first hand-back.
            for _ in 0..3 {
                let mut restricted = probe_pool.checkout_in(PostgresSessionContext::Restricted).unwrap();
                let value: String = restricted
                    .client_mut()
                    .unwrap()
                    .query_one("SELECT current_setting('prx.rls_bypass', true)", &[])
                    .unwrap()
                    .get(0);
                assert_eq!(value, "off");
                drop(restricted);

                let value: String = probe_pool
                    .with_client_in(PostgresSessionContext::System, |client| {
                        Ok(client
                            .query_one("SELECT current_setting('prx.rls_bypass', true)", &[])?
                            .get::<_, String>(0))
                    })
                    .unwrap();
                assert_eq!(value, "on");
            }
        })
        .await
        .unwrap();

        drop(mem);
        drop(pool);
        tokio::task::spawn_blocking(move || {
            let mut client = db_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
            client
                .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_identifier(&schema)))
                .unwrap();
        })
        .await
        .unwrap();
    }

    // ---------------------------------------------------------------------
    // C03 / V02-R4: Postgres parity for owner-qualified identity isolation.
    //
    // Requires a reachable instance: set `OPENPRX_TEST_POSTGRES_URL` and run
    // with `--ignored`, exactly like every other Postgres test here.
    // ---------------------------------------------------------------------

    const C03_WORKSPACE: &str = "ws-c03";
    const C03_SESSION_KEY: &str = "mcp:shared-subject";

    fn c03_owner(issuer: &str) -> String {
        format!("owner:{C03_WORKSPACE}:agent:{issuer}:shared-subject")
    }

    fn c03_message_input(
        workspace_id: &str,
        owner_id: Option<&str>,
        session_key: &str,
        visibility: MemoryVisibility,
        content: &str,
    ) -> MessageEventInput {
        MessageEventInput {
            event_id: None,
            idempotency_key: None,
            workspace_id: workspace_id.to_string(),
            owner_id: owner_id.map(str::to_string),
            source: "gateway".into(),
            channel: Some("mcp".to_string()),
            session_key: Some(session_key.to_string()),
            parent_session_key: None,
            run_id: None,
            parent_run_id: None,
            agent_id: None,
            persona_id: None,
            sender: Some("shared-subject".to_string()),
            recipient: Some("shared-subject".to_string()),
            role: "assistant".to_string(),
            event_type: "message.assistant".to_string(),
            subject: None,
            goal_id: None,
            causation_event_id: None,
            correlation_id: None,
            attempt_id: None,
            lease_epoch: None,
            config_generation_id: None,
            config_source_revision: None,
            content: content.to_string(),
            raw_payload_json: None,
            visibility,
        }
    }

    fn c03_principal(owner_id: Option<&str>, isolated: bool) -> MemoryPrincipal {
        MemoryPrincipal {
            workspace_id: C03_WORKSPACE.to_string(),
            agent_id: None,
            persona_id: None,
            session_key: Some(C03_SESSION_KEY.to_string()),
            channel: Some("mcp".to_string()),
            sender: Some("shared-subject".to_string()),
            owner_id: owner_id.map(str::to_string),
            require_owner_match: isolated,
            legacy_session_key: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_c03_owner_isolation_matrix_for_session_and_private_rows() {
        let db_url = live_postgres_url();
        let schema = format!("prx_c03_isolation_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
        let owner_a = c03_owner("issuer-a");
        let owner_b = c03_owner("issuer-b");

        for (owner, visibility, content) in [
            (&owner_a, MemoryVisibility::Session, "a-session"),
            (&owner_a, MemoryVisibility::Private, "a-private"),
            (&owner_b, MemoryVisibility::Session, "b-session"),
            (&owner_a, MemoryVisibility::Workspace, "a-workspace"),
        ] {
            mem.append_message_event(c03_message_input(
                C03_WORKSPACE,
                Some(owner),
                C03_SESSION_KEY,
                visibility,
                content,
            ))
            .await
            .unwrap();
        }
        mem.append_message_event(c03_message_input(
            "ws-other",
            Some(&owner_a),
            C03_SESSION_KEY,
            MemoryVisibility::Session,
            "other-workspace",
        ))
        .await
        .unwrap();
        mem.append_message_event(c03_message_input(
            C03_WORKSPACE,
            None,
            C03_SESSION_KEY,
            MemoryVisibility::Session,
            "legacy-unowned",
        ))
        .await
        .unwrap();

        let seen_by_b = mem
            .list_message_events_recent(&c03_principal(Some(&owner_b), true), 50)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.content)
            .collect::<Vec<_>>();
        assert!(
            !seen_by_b.iter().any(|content| content == "a-session"),
            "issuer B must not read issuer A's session row: {seen_by_b:?}"
        );
        assert!(
            !seen_by_b.iter().any(|content| content == "a-private"),
            "issuer B must not read issuer A's private row: {seen_by_b:?}"
        );
        assert!(
            !seen_by_b.iter().any(|content| content == "legacy-unowned"),
            "an unattributed row must not reach an isolated principal: {seen_by_b:?}"
        );
        assert!(
            seen_by_b.iter().any(|content| content == "b-session"),
            "issuer B must still read its own row: {seen_by_b:?}"
        );
        assert!(
            !seen_by_b.iter().any(|content| content == "a-workspace"),
            "OWNER-VIS OV-C: an isolated caller no longer reads another owner's workspace row: {seen_by_b:?}"
        );
        assert!(
            !seen_by_b.iter().any(|content| content == "other-workspace"),
            "a different workspace stays isolated: {seen_by_b:?}"
        );

        let no_owner = mem
            .list_message_events_recent(&c03_principal(None, true), 50)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.content)
            .collect::<Vec<_>>();
        assert!(
            no_owner.is_empty(),
            "OWNER-VIS OV-C: an isolated principal with no owner id admits nothing: {no_owner:?}"
        );

        let unisolated = mem
            .list_message_events_recent(&c03_principal(Some(&owner_b), false), 50)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.content)
            .collect::<Vec<_>>();
        assert!(
            unisolated.iter().any(|content| content == "a-session")
                && !unisolated.iter().any(|content| content == "a-private"),
            "a non-isolated principal keeps the session semantics, and since OWNER-VIS OV-E \
             reads another owner's private row no longer: {unisolated:?}"
        );
    }

    // ---------------------------------------------------------------------
    // V02-R8: Postgres parity for owner isolation over `document_chunks`.
    //
    // All three chunk search statements — the keyword/ILIKE hybrid, the
    // pgvector arm and the BYTEA fallback arm — carried the same
    // `visibility IN ('global','workspace') OR owner matches` disjunct, whose
    // first half short-circuited the second. The guard below is the twin of the
    // SQLite one, so `document_search` agrees with `document_get_chunk` on both
    // backends.
    //
    // The pure test runs anywhere. The behavioural matrix is gated like every
    // other Postgres test in this module: `#[ignore]`d, run only with
    // `OPENPRX_TEST_POSTGRES_URL` and `--ignored`, so it is **written but not
    // run** on the authoring host, which has no PostgreSQL.
    // ---------------------------------------------------------------------

    /// The guard is the SQLite one, in the PostgreSQL dialect: suppressed for
    /// an owner-isolated principal only, binding nothing either way.
    ///
    /// MUTATION GUARD: render the guard unconditionally and the first row goes
    /// red; render it never and the isolated rows do.
    #[test]
    fn postgres_document_visibility_guard_suppresses_the_arm_only_for_an_isolated_principal() {
        assert_eq!(
            PostgresMemory::document_visibility_guard(&c03_principal(Some("owner-second"), false)),
            "",
            "a non-isolated principal must leave all three statements byte-identical"
        );
        for owner in [None, Some("owner-second"), Some("   ")] {
            let guard = PostgresMemory::document_visibility_guard(&c03_principal(owner, true));
            assert_eq!(
                guard, " AND FALSE",
                "an owner-isolated principal is never admitted by visibility alone (owner: {owner:?})"
            );
            assert!(
                !guard.contains('$'),
                "the guard binds no parameter, so no placeholder index moves: {guard}"
            );
        }
    }

    /// NOT RUN on this host: no PostgreSQL. Written so the behaviour is pinned
    /// the moment an instance is attached.
    ///
    /// Mirrors `document_chunk_keyword_search_is_owner_qualified_for_an_isolated_principal`
    /// in the SQLite adapter, over the same four fixture cases.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_document_chunk_search_is_owner_qualified_for_an_isolated_principal() {
        let db_url = live_postgres_url();
        let schema = format!("prx_r8_documents_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
        let owner_first = c03_owner("issuer-a");
        let owner_second = c03_owner("issuer-b");

        for (document_id, owner_id, content, visibility) in [
            (
                "document-first-owner",
                Some(owner_first.as_str()),
                "kestrel migration notes recorded for the first owner",
                MemoryVisibility::Workspace,
            ),
            (
                "document-second-owner",
                Some(owner_second.as_str()),
                "kestrel migration notes recorded for the second owner",
                MemoryVisibility::Workspace,
            ),
            (
                "document-unowned",
                None,
                "kestrel migration notes recorded with no owner at all",
                MemoryVisibility::Workspace,
            ),
            (
                "document-global",
                Some(owner_first.as_str()),
                "kestrel migration notes published at global visibility",
                MemoryVisibility::Global,
            ),
        ] {
            mem.ingest_document(DocumentIngestInput {
                document_id: Some(document_id.to_string()),
                workspace_id: C03_WORKSPACE.to_string(),
                owner_id: owner_id.map(str::to_string),
                topic_id: None,
                task_id: None,
                source_message_event_id: None,
                source_kind: "conversation_page".to_string(),
                source_uri: None,
                title: None,
                content: content.to_string(),
                mime_type: Some("text/plain".to_string()),
                visibility,
                metadata_json: None,
                session_key: None,
                agent_id: None,
                persona_id: None,
            })
            .await
            .unwrap();
        }

        let shared = mem
            .search_document_chunks(&c03_principal(Some(&owner_second), false), "kestrel", 10)
            .await
            .unwrap();
        assert_eq!(
            shared.len(),
            4,
            "a non-isolated principal keeps the workspace-shared view: {shared:?}"
        );

        let isolated = mem
            .search_document_chunks(&c03_principal(Some(&owner_second), true), "kestrel", 10)
            .await
            .unwrap()
            .into_iter()
            .map(|result| result.chunk.document_id)
            .collect::<Vec<_>>();
        assert_eq!(
            isolated,
            vec!["document-second-owner".to_string()],
            "an isolated principal reads its own chunk and no other: {isolated:?}"
        );

        for blank in [None, Some("   ")] {
            let nothing = mem
                .search_document_chunks(&c03_principal(blank, true), "kestrel", 10)
                .await
                .unwrap();
            assert!(
                nothing.is_empty(),
                "an isolated principal with no usable owner id answers empty rather than erroring \
                 (owner: {blank:?}): {nothing:?}"
            );
        }
    }

    // ---------------------------------------------------------------------
    // V02-R5: Postgres parity for owner isolation over the `memories` table.
    //
    // Same gating as every other Postgres test in this module: `#[ignore]`d,
    // run only with `OPENPRX_TEST_POSTGRES_URL` and `--ignored`, so a host
    // without PostgreSQL counts them as ignored, never as passed. They were
    // written but **not run** on the authoring host (no PostgreSQL). They need
    // a reachable instance the test role may create and drop a schema in; the
    // RLS pin below additionally needs a role that RLS actually applies to (not
    // a superuser and not `BYPASSRLS`), as `postgres_vector_rls_*` already
    // requires.
    // ---------------------------------------------------------------------

    // ---------------------------------------------------------------------
    // V02-R5 follow-up F1: SQLite/PostgreSQL parity on the no-owner path.
    //
    // Under isolation the RLS owner follows `context.owner_id` alone. With a
    // blank or absent owner id that is `None`, and `apply_rls_context_raw`
    // rejects a non-system read that carries no owner — so the read would
    // **error** where SQLite answers with an empty result. Both ends of that
    // divergence need a decision that is provable without a database, because
    // this repository has no PostgreSQL to run the matrices above against.
    //
    // The two tests below are that proof: one pins the decision itself as a
    // pure function, the other pins that the decision is taken *before* the
    // RLS context is applied, which is the only placement that turns the error
    // into the empty answer SQLite already gives.
    // ---------------------------------------------------------------------

    /// The production text of this file, with its test module removed.
    fn r5_production_source() -> &'static str {
        include_str!("postgres.rs")
            .rsplit_once("\n#[cfg(test)]\nmod tests {")
            .expect("test: postgres.rs ends with its test module")
            .0
    }

    /// Drop every line comment, so a comment that merely *names* a call is not
    /// mistaken for the call itself.
    fn r5_without_comments(text: &str) -> String {
        text.lines()
            .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One method's production text: from its declaration to the next method
    /// declaration at the same indentation.
    ///
    /// Deliberately textual rather than brace-matched: these bodies build SQL
    /// with `format!`, so their literals carry braces of their own.
    fn r5_method_text(production: &'static str, declaration: &str) -> &'static str {
        let start = production
            .find(declaration)
            .unwrap_or_else(|| panic!("test: production declaration not found: {declaration}"));
        let body_start = start + declaration.len();
        let rest = &production[body_start..];
        let end = ["\n    async fn ", "\n    fn ", "\n    pub fn ", "\n    pub async fn "]
            .iter()
            .filter_map(|marker| rest.find(marker))
            .min()
            .unwrap_or(rest.len());
        &production[start..body_start + end]
    }

    /// The decision, at the level that runs on a host without PostgreSQL: an
    /// owner-isolated read whose context carries no usable owner id answers
    /// empty. SQLite reaches the same answer through the ` AND FALSE` arm of
    /// the shared predicate; this pins that PostgreSQL agrees rather than
    /// erroring, and that a blank owner counts as no owner on both.
    #[test]
    fn postgres_isolated_read_without_an_owner_id_decides_empty() {
        for blank in [None, Some(""), Some("   "), Some("\t\n")] {
            let context = r5_context(blank, true);
            assert!(
                PostgresMemory::isolated_read_without_owner(&context),
                "an isolated context with owner {blank:?} must decide the empty answer"
            );
            let base = Principal {
                user_id: "anonymous:mcp:shared-subject".to_string(),
                role: Role::Anonymous,
                projects: Vec::new(),
                visibility_ceiling: Visibility::Private,
                blocked_patterns: Vec::new(),
                current_channel: "mcp".to_string(),
                current_chat_id: C03_SESSION_KEY.to_string(),
                current_chat_type: ChatType::Dm,
                raw_sender: "shared-subject".to_string(),
                acl_enforced: true,
                owner_id: None,
                require_owner_match: false,
            };
            let (isolated_sql, isolated_params) = base.clone().with_owner_scope(&context).build_sql_scope_pg(1);
            let (_, unisolated_params) = base.build_sql_scope_pg(1);
            assert!(
                isolated_sql.contains("AND FALSE"),
                "the shared predicate must fail closed for the same context: {isolated_sql}"
            );
            assert_eq!(
                isolated_params, unisolated_params,
                "a fail-closed arm binds no owner parameter of its own"
            );
        }

        let owned = r5_context(Some("owner:ws:agent:issuer-a.example:shared-subject"), true);
        assert!(
            !PostgresMemory::isolated_read_without_owner(&owned),
            "an isolated context that carries an owner id reads normally"
        );
        let mut unisolated = r5_context(None, false);
        unisolated.sender_id = Some("anonymous:mcp:shared-subject".to_string());
        assert!(
            !PostgresMemory::isolated_read_without_owner(&unisolated),
            "a non-isolated context is never short-circuited, whatever its owner id"
        );
    }

    /// MUTATION GUARD: remove either short-circuit and this goes red. The
    /// placement is the whole point — after `apply_rls_context_raw` the read
    /// has already failed with "requires a trusted owner context" instead of
    /// answering empty.
    #[test]
    fn postgres_isolated_reads_short_circuit_before_applying_the_rls_owner() {
        let production = r5_production_source();
        for declaration in [
            "    async fn recall_with_context(",
            "    async fn get_with_context(&self, key: &str, context: Option<&MemoryWriteContext>)",
        ] {
            let method = r5_without_comments(r5_method_text(production, declaration));
            let short_circuit = method.find("isolated_read_without_owner").unwrap_or_else(|| {
                panic!("{declaration} must short-circuit an isolated read that carries no owner id")
            });
            let rls = method
                .find("apply_rls_context_raw")
                .unwrap_or_else(|| panic!("{declaration} must still apply the RLS context"));
            assert!(
                short_circuit < rls,
                "{declaration} must decide the empty answer before applying the RLS owner, \
                 otherwise a blank owner errors instead of reading nothing"
            );
        }
    }

    /// The `memories`-table write context an MCP caller produces: the channel,
    /// chat id and raw sender are the external subject only, so two issuers
    /// differ in `owner_id` and nothing else.
    fn r5_context(owner_id: Option<&str>, isolated: bool) -> MemoryWriteContext {
        MemoryWriteContext {
            workspace_id: Some(C03_WORKSPACE.to_string()),
            channel: Some("mcp".to_string()),
            chat_type: Some("gateway".to_string()),
            chat_id: Some(C03_SESSION_KEY.to_string()),
            sender_id: owner_id.map(str::to_string),
            raw_sender: Some("shared-subject".to_string()),
            owner_id: owner_id.map(str::to_string),
            require_owner_match: isolated,
        }
    }

    fn r5_metadata(owner_id: &str) -> MemoryStoreMetadata {
        MemoryStoreMetadata {
            workspace_id: Some(C03_WORKSPACE.to_string()),
            owner_id: Some(owner_id.to_string()),
            channel: Some("mcp".to_string()),
            ..MemoryStoreMetadata::default()
        }
    }

    async fn r5_seed(mem: &PostgresMemory, owner_id: &str, key: &str, content: &str) {
        mem.store_with_context_and_metadata(
            key,
            content,
            MemoryCategory::Conversation,
            None,
            Some(&r5_context(Some(owner_id), false)),
            r5_metadata(owner_id),
        )
        .await
        .expect("test: store an owner-attributed memory row");
    }

    /// Force one seeded row to `public` visibility. `classify_memory` never
    /// produces `public`, so a deliberately shared row can only be made here.
    async fn r5_make_public(mem: &PostgresMemory, key: &str) {
        let client_slot = mem.client.clone();
        let qualified_table = mem.qualified_table.clone();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || -> Result<()> {
            client_slot.with_client(|client| {
                client.execute(
                    &format!(
                        "UPDATE {qualified_table} SET visibility = 'public', sensitivity = 'normal' WHERE key = $1"
                    ),
                    &[&key],
                )?;
                Ok(())
            })
        })
        .await
        .expect("test: publish task")
        .expect("test: mark the row public");
    }

    async fn r5_drop_schema(db_url: String, schema: String) {
        tokio::task::spawn_blocking(move || {
            let mut client = db_url
                .parse::<postgres::Config>()
                .expect("test: parse the postgres url")
                .connect(NoTls)
                .expect("test: connect to postgres");
            client
                .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_identifier(&schema)))
                .expect("test: drop the test schema");
        })
        .await
        .expect("test: schema drop task");
    }

    /// R-INT2A F1, the offline half: the write path a plugin's `memory.store`
    /// takes (`store_with_metadata`, carrying only the plugin's channel)
    /// stays system-scoped here. It computes the embedding as the system and
    /// never applies a caller's RLS context, which would demand a workspace
    /// and owner a plugin does not have.
    ///
    /// MUTATION GUARD: route `store_with_metadata` through
    /// `store_with_context_and_metadata` or `apply_rls_context_raw` and this
    /// goes red.
    #[test]
    fn postgres_metadata_writes_stay_system_scoped_for_plugin_rows() {
        let production = r5_production_source();
        let declaration = "    async fn store_with_metadata(";
        let method = r5_without_comments(r5_method_text(production, declaration));
        assert!(
            method.contains("embedding_metadata_for_category(&category, content, None, None, true)"),
            "{declaration} must compute its embedding with system scope"
        );
        for caller_scoped in [
            "apply_rls_context_raw",
            "store_with_context_and_metadata",
            "resolve_principal",
        ] {
            assert!(
                !method.contains(caller_scoped),
                "{declaration} must not take the caller-scoped path ({caller_scoped})"
            );
        }
    }

    /// R-INT2A F1, the live half: a plugin's `memory.store` succeeds on
    /// PostgreSQL and the row is readable. **NOT RUN on this host: no
    /// PostgreSQL.**
    #[cfg(feature = "wasm-plugins")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_plugin_memory_store_succeeds() {
        let db_url = live_postgres_url();
        let schema = format!("prx_int2af_plugin_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
        let key =
            crate::plugins::capabilities::common::store_plugin_memory(&mem, "sample-plugin", "plugin text", "core")
                .await
                .expect("test: a plugin write succeeds on PostgreSQL");
        let stored = mem.get(&key).await.expect("test: read back");
        assert_eq!(stored.map(|entry| entry.content).as_deref(), Some("plugin text"));
        drop(mem);
        r5_drop_schema(db_url, schema).await;
    }

    /// The direct analogue of the C03 session/private matrix, over `memories`.
    ///
    /// Under a NOSUPERUSER/NOBYPASSRLS role RLS applies to every read here, so
    /// issuer B sees issuer A's `public` row only through the read-only
    /// `memories_public_read` policy (D-028); before it, RLS hid that row and
    /// the `r5-a-public` assertion failed on such a role. Under a superuser the
    /// RLS half is bypassed and only the SQL predicate is exercised.
    /// **NOT RUN on this host: no PostgreSQL.**
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_owner_isolated_memory_recall_matrix_for_the_same_subject() {
        let db_url = live_postgres_url();
        let schema = format!("prx_r5_recall_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
        let owner_a = c03_owner("issuer-a");
        let owner_b = c03_owner("issuer-b");

        r5_seed(&mem, &owner_a, "r5-a", "shared keyword first issuer note").await;
        r5_seed(&mem, &owner_b, "r5-b", "shared keyword second issuer note").await;
        r5_seed(&mem, &owner_a, "r5-a-public", "shared keyword announcement").await;
        mem.store(
            "r5-legacy",
            "shared keyword unattributed note",
            MemoryCategory::Conversation,
            None,
        )
        .await
        .expect("test: store a legacy row with no owner");
        r5_make_public(&mem, "r5-a-public").await;

        let keys = |entries: Vec<MemoryEntry>| {
            entries
                .into_iter()
                .map(|entry| entry.key)
                .collect::<std::collections::HashSet<_>>()
        };

        let seen_by_b = keys(
            mem.recall_with_context("shared keyword", 50, None, Some(&r5_context(Some(&owner_b), true)))
                .await
                .expect("test: isolated recall as issuer B"),
        );
        assert!(!seen_by_b.contains("r5-a"), "{seen_by_b:?}");
        assert!(!seen_by_b.contains("r5-legacy"), "{seen_by_b:?}");
        assert!(seen_by_b.contains("r5-b"), "{seen_by_b:?}");
        assert!(seen_by_b.contains("r5-a-public"), "{seen_by_b:?}");

        let seen_by_a = keys(
            mem.recall_with_context("shared keyword", 50, None, Some(&r5_context(Some(&owner_a), true)))
                .await
                .expect("test: isolated recall as issuer A"),
        );
        assert!(seen_by_a.contains("r5-a"), "{seen_by_a:?}");
        assert!(!seen_by_a.contains("r5-b"), "{seen_by_a:?}");

        // Observe must not reopen the boundary (the backend mode override).
        let observed = keys(
            mem.recall_with_context_mode(
                "shared keyword",
                50,
                None,
                Some(&r5_context(Some(&owner_b), true)),
                MemoryReadMode::Observe,
            )
            .await
            .expect("test: observe-mode recall under isolation"),
        );
        assert!(!observed.contains("r5-a"), "{observed:?}");

        drop(mem);
        r5_drop_schema(db_url, schema).await;
    }

    /// The same matrix for the exact-key read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_owner_isolated_memory_get_matrix_for_the_same_subject() {
        let db_url = live_postgres_url();
        let schema = format!("prx_r5_get_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
        let owner_a = c03_owner("issuer-a");
        let owner_b = c03_owner("issuer-b");

        r5_seed(&mem, &owner_a, "r5-a", "shared keyword first issuer note").await;

        assert!(
            mem.get_with_context("r5-a", Some(&r5_context(Some(&owner_b), true)))
                .await
                .expect("test: isolated keyed read as issuer B")
                .is_none(),
            "an exact key must not cross the owner boundary"
        );
        assert!(
            mem.get_with_context("r5-a", Some(&r5_context(Some(&owner_a), true)))
                .await
                .expect("test: isolated keyed read as issuer A")
                .is_some(),
            "the owning caller must still read its own row"
        );
        assert!(
            mem.get_with_context_mode("r5-a", Some(&r5_context(Some(&owner_b), true)), MemoryReadMode::Observe)
                .await
                .expect("test: observe-mode keyed read under isolation")
                .is_none(),
            "Observe must not reopen the owner boundary"
        );
        // F1 parity: this is an empty answer, not an error. The isolated
        // no-owner case is decided before the RLS context is applied, so
        // PostgreSQL returns `Ok(None)` exactly as SQLite returns no rows.
        assert!(
            mem.get_with_context("r5-a", Some(&r5_context(None, true)))
                .await
                .expect("test: isolated keyed read with no owner id")
                .is_none(),
            "an isolated caller with no owner id must read nothing owned"
        );
        assert!(
            mem.recall_with_context("shared keyword", 50, None, Some(&r5_context(None, true)))
                .await
                .expect("test: isolated recall with no owner id")
                .is_empty(),
            "the recall half of the same no-owner case must also answer empty"
        );

        drop(mem);
        r5_drop_schema(db_url, schema).await;
    }

    /// The RLS owner an isolated read sets must be the same string the SQL
    /// predicate gates on. Guards the `recall_with_context` /
    /// `get_with_context` `rls_owner` change: if the two drifted apart, a read
    /// whose `sender_id` differs from its `owner_id` would silently fall back
    /// to the RLS policy alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_owner_isolated_rls_owner_matches_the_predicate_owner() {
        let db_url = live_postgres_url();
        let schema = format!("prx_r5_rls_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
        let owner_a = c03_owner("issuer-a");

        r5_seed(&mem, &owner_a, "r5-a", "shared keyword first issuer note").await;

        // `sender_id` deliberately disagrees with `owner_id`: the RLS owner and
        // the predicate owner must both follow `owner_id` under isolation.
        let mut divergent = r5_context(Some(&owner_a), true);
        divergent.sender_id = Some("some-other-anchor".to_string());
        assert!(
            mem.get_with_context("r5-a", Some(&divergent))
                .await
                .expect("test: isolated keyed read with a divergent sender anchor")
                .is_some(),
            "the RLS owner must follow owner_id, not sender_id, under isolation"
        );

        drop(mem);
        r5_drop_schema(db_url, schema).await;
    }

    /// R-B1d D-1, the delete half. The twin of
    /// `sqlite_owner_isolated_forget_excludes_another_owners_row_by_key` and
    /// its public-row case, over the one `DELETE` statement this adapter runs.
    ///
    /// **NOT RUN on a host without PostgreSQL:** it is `#[ignore]`d there, so
    /// the runner counts it as ignored rather than printing a vacuous `ok`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_owner_isolated_forget_is_owner_qualified() {
        let db_url = live_postgres_url();
        let schema = format!("prx_d1_forget_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
        let owner_a = c03_owner("issuer-a");
        let owner_b = c03_owner("issuer-b");

        r5_seed(&mem, &owner_a, "d1-a", "shared keyword first issuer note").await;
        assert!(
            mem.get_with_context("d1-a", Some(&r5_context(Some(&owner_a), true)))
                .await
                .expect("test: read back the fixture")
                .is_some(),
            "sanity: the fixture row must exist, or every assertion below passes vacuously"
        );

        assert!(
            !mem.forget_with_context("d1-a", Some(&r5_context(Some(&owner_b), true)))
                .await
                .expect("test: isolated forget as issuer B"),
            "an isolated caller must not delete another owner's row by key"
        );
        // A blank owner is decided before any statement or RLS call runs, so
        // this is `Ok(false)` and not the "requires a trusted owner context"
        // error `apply_rls_context_raw` would raise.
        for blank in [None, Some("")] {
            assert!(
                !mem.forget_with_context("d1-a", Some(&r5_context(blank, true)))
                    .await
                    .expect("test: isolated forget with no owner id"),
                "an isolated caller with owner {blank:?} must delete nothing"
            );
        }
        // The public arm the shared visibility predicate leaves open for reads.
        r5_make_public(&mem, "d1-a").await;
        assert!(
            !mem.forget_with_context("d1-a", Some(&r5_context(Some(&owner_b), true)))
                .await
                .expect("test: isolated forget of a public row as issuer B"),
            "a public row is shared for reading, but it is still another owner's row to delete"
        );

        // Positive control: the owner still deletes its own row.
        assert!(
            mem.forget_with_context("d1-a", Some(&r5_context(Some(&owner_a), true)))
                .await
                .expect("test: isolated forget as issuer A"),
            "the owning caller must still delete its own row"
        );
        assert!(
            mem.get_with_context("d1-a", Some(&r5_context(Some(&owner_a), true)))
                .await
                .expect("test: read back after the delete")
                .is_none(),
            "the row must really be gone"
        );

        drop(mem);
        r5_drop_schema(db_url, schema).await;
    }

    /// The placement, at the level that runs on a host without PostgreSQL: the
    /// isolated no-owner delete is decided before the RLS context is applied,
    /// exactly as the two keyed reads decide it.
    ///
    /// MUTATION GUARD: move the short-circuit below `apply_rls_context_raw`,
    /// or let it fall through instead of returning `Ok(false)`, and this goes
    /// red.
    #[test]
    fn postgres_isolated_forget_short_circuits_before_applying_the_rls_owner() {
        let production = r5_production_source();
        let declaration = "    async fn forget_with_context(";
        let method = r5_without_comments(r5_method_text(production, declaration));
        let short_circuit = method
            .find("isolated_read_without_owner")
            .unwrap_or_else(|| panic!("{declaration} must short-circuit an isolated delete with no owner id"));
        let rls = method
            .find("apply_rls_context_raw")
            .unwrap_or_else(|| panic!("{declaration} must still apply the RLS context"));
        assert!(
            short_circuit < rls,
            "{declaration} must decide the empty answer before applying the RLS owner, \
             otherwise a blank owner errors instead of deleting nothing"
        );
        // R-B1g F-6: the call being first is not enough; the branch must
        // answer. Falling through would still apply the RLS owner.
        assert!(
            method
                .get(short_circuit..rls)
                .is_some_and(|between| between.contains("return Ok(false)")),
            "{declaration} must return `Ok(false)` from the short-circuit, not fall through to the delete"
        );
    }

    /// The guard itself, runnable without a database: its decision, its
    /// placeholder, and that a non-isolated delete renders no fragment at all
    /// so its statement and bind list stay byte-identical.
    #[test]
    fn postgres_forget_owner_guard_binds_only_for_an_isolated_caller_with_an_owner() {
        let owner_a = c03_owner("issuer-a");

        let mut unisolated = r5_context(Some(&owner_a), false);
        unisolated.sender_id = Some(owner_a.clone());
        let guard = PostgresMemory::forget_owner_guard(&unisolated, 9);
        assert_eq!(guard.sql, "", "a non-isolated delete must render no fragment at all");
        assert!(!guard.bound_owner);
        assert_eq!(PostgresMemory::forget_owner_anchor(&unisolated), None);

        let isolated = r5_context(Some(&owner_a), true);
        let guard = PostgresMemory::forget_owner_guard(&isolated, 9);
        assert_eq!(guard.sql, " AND owner_id = $9");
        assert!(guard.bound_owner, "one marker, one parameter");
        assert_eq!(
            PostgresMemory::forget_owner_anchor(&isolated).as_deref(),
            Some(owner_a.as_str())
        );

        // The blank case never reaches the guard — the short-circuit above
        // answers first — but the seam must still fail closed if it ever did.
        for blank in [None, Some(""), Some("   ")] {
            let context = r5_context(blank, true);
            assert!(PostgresMemory::isolated_read_without_owner(&context));
            let guard = PostgresMemory::forget_owner_guard(&context, 9);
            assert_eq!(guard.sql, " AND FALSE", "owner {blank:?} must fail closed");
            assert!(!guard.bound_owner, "a fail-closed guard binds nothing");
        }
    }

    /// Pin what the unscoped compatibility reads do with **no** RLS context.
    ///
    /// `recall()` and `get()` never call `apply_rls_context_raw`, so their
    /// result depends on whether RLS applies to the connected role: an
    /// unprivileged role sees nothing, a superuser / `BYPASSRLS` role sees
    /// everything. This records that split as a stated contract instead of an
    /// accident, and asserts the one thing that must hold either way: the
    /// *scoped* read is unaffected by it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_unscoped_recall_behaviour_under_rls_is_pinned() {
        let db_url = live_postgres_url();
        let schema = format!("prx_r5_unscoped_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
        let owner_a = c03_owner("issuer-a");
        let owner_b = c03_owner("issuer-b");

        r5_seed(&mem, &owner_a, "r5-a", "shared keyword first issuer note").await;

        let unscoped = mem
            .recall("shared keyword", 50, None)
            .await
            .expect("test: unscoped recall");
        let bypasses_rls = unscoped.iter().any(|entry| entry.key == "r5-a");

        let scoped = mem
            .recall_with_context("shared keyword", 50, None, Some(&r5_context(Some(&owner_b), true)))
            .await
            .expect("test: isolated recall as issuer B");
        assert!(
            !scoped.iter().any(|entry| entry.key == "r5-a"),
            "the scoped read must isolate regardless of whether the connected role \
             bypasses RLS (observed bypass: {bypasses_rls})"
        );

        drop(mem);
        r5_drop_schema(db_url, schema).await;
    }

    /// C03 (P2-4), Postgres twin: the draft-lifecycle and document-ingest
    /// inserts must record the owner in the `memory_events.owner_id` column.
    /// The draft rows are written at `session` visibility, the arm the owner
    /// predicate guards, so they exercise attribution and isolation; the
    /// `document.ingested` row is written workspace-visible here so this test
    /// stays about attribution; the `session`-visibility ingest arm has its own
    /// tests below.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_c03_draft_lifecycle_and_ingest_events_carry_the_owner() {
        let db_url = live_postgres_url();
        let schema = format!("prx_c03_lifecycle_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
        let owner_a = c03_owner("issuer-a");
        let owner_b = c03_owner("issuer-b");

        for (owner, draft_id, key) in [(&owner_a, "draft-a", "key-a"), (&owner_b, "draft-b", "key-b")] {
            mem.create_memory_draft(MemoryDraftInput {
                draft_id: Some(draft_id.to_string()),
                workspace_id: C03_WORKSPACE.to_string(),
                owner_id: Some(owner.clone()),
                worker_run_id: format!("run-{draft_id}"),
                parent_run_id: None,
                session_key: Some(C03_SESSION_KEY.to_string()),
                agent_id: None,
                persona_id: None,
                key: key.to_string(),
                content: "draft body".to_string(),
                category: MemoryCategory::Core,
                source_event_id: None,
                visibility: MemoryVisibility::Session,
                payload_json: None,
            })
            .await
            .unwrap();
        }
        mem.merge_memory_draft(&c03_principal(Some(&owner_b), false), "draft-b")
            .await
            .unwrap();
        mem.reject_memory_draft(&c03_principal(Some(&owner_a), false), "draft-a", Some("not wanted"))
            .await
            .unwrap();

        for (owner, document_id, body) in [
            (&owner_a, "doc-a", "issuer-a document"),
            (&owner_b, "doc-b", "issuer-b document"),
        ] {
            mem.ingest_document(DocumentIngestInput {
                document_id: Some(document_id.to_string()),
                workspace_id: C03_WORKSPACE.to_string(),
                owner_id: Some(owner.clone()),
                topic_id: None,
                task_id: None,
                source_message_event_id: None,
                source_kind: "test".to_string(),
                source_uri: None,
                title: None,
                content: body.to_string(),
                mime_type: Some("text/plain".to_string()),
                visibility: MemoryVisibility::Workspace,
                metadata_json: None,
                session_key: None,
                agent_id: None,
                persona_id: None,
            })
            .await
            .unwrap();
        }

        let events = mem
            .list_memory_events_recent(&c03_principal(Some(&owner_a), false), 200)
            .await
            .unwrap();
        for (event_type, subject_id, expected_owner) in [
            ("memory.draft.created", "draft-a", &owner_a),
            ("memory.draft.created", "draft-b", &owner_b),
            ("memory.draft.merged", "draft-b", &owner_b),
            ("memory.stored", "key-b", &owner_b),
            ("memory.draft.rejected", "draft-a", &owner_a),
            ("document.ingested", "doc-a", &owner_a),
            ("document.ingested", "doc-b", &owner_b),
        ] {
            let event = events
                .iter()
                .find(|event| event.event_type == event_type && event.subject_id == subject_id)
                .unwrap_or_else(|| panic!("no `{event_type}` row for `{subject_id}`"));
            assert_eq!(
                event.owner_id.as_deref(),
                Some(expected_owner.as_str()),
                "`{event_type}` for `{subject_id}` must carry its own owner"
            );
        }

        let seen_by_b = mem
            .list_memory_events_recent(&c03_principal(Some(&owner_b), true), 200)
            .await
            .unwrap()
            .into_iter()
            .map(|event| (event.event_type, event.subject_id))
            .collect::<Vec<_>>();
        assert!(
            seen_by_b.contains(&("memory.draft.created".to_string(), "draft-b".to_string()))
                && seen_by_b.contains(&("memory.draft.merged".to_string(), "draft-b".to_string())),
            "issuer B must read its own draft lifecycle: {seen_by_b:?}"
        );
        assert!(
            !seen_by_b
                .iter()
                .any(|(_, subject)| subject == "draft-a" || subject == "key-a"),
            "issuer B must not read issuer A's draft lifecycle: {seen_by_b:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_c03_memory_events_are_owner_qualified() {
        let db_url = live_postgres_url();
        let schema = format!("prx_c03_events_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
        let owner_a = c03_owner("issuer-a");
        let owner_b = c03_owner("issuer-b");

        for (owner, subject) in [
            (Some(&owner_a), "run-a"),
            (Some(&owner_b), "run-b"),
            (None, "run-legacy"),
        ] {
            mem.append_memory_event(MemoryEventInput {
                event_id: None,
                workspace_id: C03_WORKSPACE.to_string(),
                owner_id: owner.cloned(),
                event_type: "task.spawned".to_string(),
                subject_table: "tasks".to_string(),
                subject_id: subject.to_string(),
                session_key: Some(C03_SESSION_KEY.to_string()),
                run_id: Some(subject.to_string()),
                parent_run_id: None,
                agent_id: None,
                persona_id: None,
                visibility: MemoryVisibility::Session,
                payload_json: None,
            })
            .await
            .unwrap();
        }

        let seen = mem
            .list_memory_events_recent(&c03_principal(Some(&owner_b), true), 50)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.subject_id)
            .collect::<Vec<_>>();
        assert_eq!(
            seen,
            vec!["run-b".to_string()],
            "an isolated principal must only see its own task events: {seen:?}"
        );

        let all = mem
            .list_memory_events_recent(&c03_principal(Some(&owner_b), false), 50)
            .await
            .unwrap();
        assert_eq!(all.len(), 3, "the non-isolated read is unchanged");
        assert!(
            all.iter()
                .any(|event| event.owner_id.as_deref() == Some(owner_a.as_str())),
            "the owner column must round-trip through Postgres"
        );
    }

    // ---------------------------------------------------------------------
    // V02-R6, Postgres twins. `PostgresMemory::ingest_document` requires a
    // non-empty `owner_id` for vector isolation, so every fixture here sets
    // one; the SQLite `owner_id: None` cases have no Postgres counterpart.
    // ---------------------------------------------------------------------

    fn postgres_c03_ingest_input(
        document_id: &str,
        owner_id: &str,
        session_key: Option<&str>,
        visibility: MemoryVisibility,
    ) -> DocumentIngestInput {
        DocumentIngestInput {
            document_id: Some(document_id.to_string()),
            workspace_id: C03_WORKSPACE.to_string(),
            owner_id: Some(owner_id.to_string()),
            topic_id: None,
            task_id: None,
            source_message_event_id: None,
            source_kind: "test".to_string(),
            source_uri: None,
            title: None,
            content: format!("document body for {document_id}"),
            mime_type: Some("text/plain".to_string()),
            visibility,
            metadata_json: None,
            session_key: session_key.map(str::to_string),
            agent_id: None,
            persona_id: None,
        }
    }

    async fn postgres_c03_ingest_subjects(mem: &PostgresMemory, principal: &MemoryPrincipal) -> Vec<String> {
        mem.list_memory_events_recent(principal, 200)
            .await
            .expect("test: recent memory events")
            .into_iter()
            .filter(|event| event.event_type == "document.ingested")
            .map(|event| event.subject_id)
            .collect()
    }

    /// The V02-R6 regression on Postgres: the bound `session_key` is what makes
    /// the `session` arm match at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_c03_session_visibility_ingest_event_is_readable_by_its_own_session() {
        let db_url = live_postgres_url();
        let schema = format!("prx_c03_ingest_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
        let owner_a = c03_owner("issuer-a");

        mem.ingest_document(postgres_c03_ingest_input(
            "doc-s",
            &owner_a,
            Some(C03_SESSION_KEY),
            MemoryVisibility::Session,
        ))
        .await
        .expect("test: ingest session-visibility document");

        let events = mem
            .list_memory_events_recent(&c03_principal(Some(&owner_a), false), 200)
            .await
            .expect("test: recent memory events");
        let event = events
            .iter()
            .find(|event| event.event_type == "document.ingested" && event.subject_id == "doc-s")
            .expect("test: the session ingest event must be readable by its own session");
        assert_eq!(event.session_key.as_deref(), Some(C03_SESSION_KEY));
        assert_eq!(event.owner_id.as_deref(), Some(owner_a.as_str()));

        drop(mem);
        r5_drop_schema(db_url, schema).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_c03_session_visibility_ingest_event_is_readable_under_owner_isolation() {
        let db_url = live_postgres_url();
        let schema = format!("prx_c03_ingest_isolated_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
        let owner_a = c03_owner("issuer-a");

        mem.ingest_document(postgres_c03_ingest_input(
            "doc-s",
            &owner_a,
            Some(C03_SESSION_KEY),
            MemoryVisibility::Session,
        ))
        .await
        .expect("test: ingest session-visibility document");

        let seen = postgres_c03_ingest_subjects(&mem, &c03_principal(Some(&owner_a), true)).await;
        assert_eq!(
            seen,
            vec!["doc-s".to_string()],
            "an isolated owner must read its own session ingest event: {seen:?}"
        );

        drop(mem);
        r5_drop_schema(db_url, schema).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_c03_session_visibility_ingest_event_is_hidden_from_another_owner() {
        let db_url = live_postgres_url();
        let schema = format!("prx_c03_ingest_owners_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
        let owner_a = c03_owner("issuer-a");
        let owner_b = c03_owner("issuer-b");

        for (owner, document_id) in [(&owner_a, "doc-a"), (&owner_b, "doc-b")] {
            mem.ingest_document(postgres_c03_ingest_input(
                document_id,
                owner,
                Some(C03_SESSION_KEY),
                MemoryVisibility::Session,
            ))
            .await
            .expect("test: ingest session-visibility document");
        }

        let seen = postgres_c03_ingest_subjects(&mem, &c03_principal(Some(&owner_b), true)).await;
        assert_eq!(
            seen,
            vec!["doc-b".to_string()],
            "an isolated owner must not read another owner's ingest event: {seen:?}"
        );

        drop(mem);
        r5_drop_schema(db_url, schema).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_c03_ingest_event_is_hidden_from_another_session() {
        let db_url = live_postgres_url();
        let schema = format!("prx_c03_ingest_sessions_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
        let owner_a = c03_owner("issuer-a");

        mem.ingest_document(postgres_c03_ingest_input(
            "doc-other-session",
            &owner_a,
            Some("mcp:other-subject"),
            MemoryVisibility::Session,
        ))
        .await
        .expect("test: ingest session-visibility document");

        for isolated in [false, true] {
            let seen = postgres_c03_ingest_subjects(&mem, &c03_principal(Some(&owner_a), isolated)).await;
            assert!(
                seen.is_empty(),
                "a foreign session key must not admit the ingest event (isolated={isolated}): {seen:?}"
            );

            // Positive control, as in the SQLite twin: without it the emptiness
            // above also holds when the three scoping keys revert to NULL.
            let owning_session = MemoryPrincipal {
                session_key: Some("mcp:other-subject".to_string()),
                ..c03_principal(Some(&owner_a), isolated)
            };
            let served = postgres_c03_ingest_subjects(&mem, &owning_session).await;
            assert_eq!(
                served,
                vec!["doc-other-session".to_string()],
                "the ingesting session must read its own ingest event (isolated={isolated}): {served:?}"
            );
        }

        drop(mem);
        r5_drop_schema(db_url, schema).await;
    }

    // ---------------------------------------------------------------------
    // V02-R9: the owner-isolated `memory_store` upsert.
    //
    // The pure test runs anywhere. The behavioural twin is gated like every
    // other Postgres test in this module: `#[ignore]`d, run only with
    // `OPENPRX_TEST_POSTGRES_URL` and `--ignored`, so it is **written but not
    // run** on the authoring host, which has no PostgreSQL.
    // ---------------------------------------------------------------------

    /// The scoped upsert carries the shared conflict guard, refuses a write
    /// the guard suppressed, and maps an RLS rejection of the existing row to
    /// the same refusal; the context-free store path is untouched.
    ///
    /// MUTATION GUARD: drop `{owner_guard}`, the zero-rows check or the
    /// `42501` mapping and this goes red.
    #[test]
    fn postgres_scoped_store_upsert_carries_the_owner_conflict_guard_and_maps_both_refusals() {
        let production = r5_production_source();
        let scoped = r5_method_text(production, "async fn store_with_context_and_metadata(");
        for needle in [
            "isolated_store_owner(Some(&context))?",
            "policy_version = EXCLUDED.policy_version{owner_guard}",
            "store_conflict_guard(PG_DIALECT, &qualified_table, isolated_owner.is_some())",
            "SqlState::INSUFFICIENT_PRIVILEGE",
            "if isolated_owner.is_some() && written == 0 {",
        ] {
            assert!(
                scoped.contains(needle),
                "store_with_context_and_metadata lost `{needle}`"
            );
        }
        let plain = r5_method_text(production, "async fn store_with_metadata(");
        assert!(
            !plain.contains("owner_guard"),
            "a write without a context is never owner-isolated and keeps its historical statement"
        );
    }

    /// SP-MCP test 10. **NOT RUN on this host: no PostgreSQL.**
    ///
    /// An owner-isolated caller that names another owner's key is refused and
    /// changes nothing, for a `workspace`-visible existing row (admitted by the
    /// RLS `USING` clause, so only the conflict guard stops it) and for a
    /// non-shared one (where RLS may reject first; either way the caller sees
    /// the one refusal). Its own key still updates. Needs a role RLS applies to.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_owner_isolated_store_refuses_a_cross_owner_upsert() {
        let db_url = live_postgres_url();
        let schema = format!("prx_r9_store_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
        let qualified = format!("{}.{}", quote_identifier(&schema), quote_identifier("memories"));
        let owner_a = c03_owner("issuer-a");
        let owner_b = c03_owner("issuer-b");

        for (key, visibility) in [("r9-shared-key", "workspace"), ("r9-private-key", "private")] {
            mem.store_with_context(
                key,
                "first owner note",
                MemoryCategory::Core,
                None,
                Some(&r5_context(Some(&owner_a), true)),
            )
            .await
            .expect("test: the first owner stores its key");
            let update = format!("UPDATE {qualified} SET visibility = $1 WHERE key = $2");
            mem.client
                .with_client(|client| {
                    client.execute(&update, &[&visibility, &key])?;
                    Ok(())
                })
                .expect("test: set the seeded row's visibility");

            let refused = mem
                .store_with_context(
                    key,
                    "second owner overwrite",
                    MemoryCategory::Core,
                    None,
                    Some(&r5_context(Some(&owner_b), true)),
                )
                .await
                .expect_err("test: a cross-owner write must fail");
            assert_eq!(
                refused.to_string(),
                format!("key '{key}' is not available to this caller"),
                "{visibility}: one refusal, whichever layer stopped the write"
            );

            let select = format!("SELECT content, owner_id FROM {qualified} WHERE key = $1");
            let (content, owner): (String, Option<String>) = mem
                .client
                .with_client(|client| {
                    let row = client.query_one(&select, &[&key])?;
                    Ok((row.get(0), row.get(1)))
                })
                .expect("test: read the seeded row back");
            assert_eq!(content, "first owner note", "{visibility}: content untouched");
            assert_eq!(
                owner.as_deref(),
                Some(owner_a.as_str()),
                "{visibility}: owner untouched"
            );
        }

        mem.store_with_context(
            "r9-shared-key",
            "first owner update",
            MemoryCategory::Core,
            None,
            Some(&r5_context(Some(&owner_a), true)),
        )
        .await
        .expect("test: the owner still updates its own key");

        drop(mem);
        r5_drop_schema(db_url, schema).await;
    }

    // ---------------------------------------------------------------------
    // OWNER-VIS: event reads. The text guards run anywhere; the behavioural
    // twins are gated like every other Postgres test in this module and are
    // **written but not run** on the authoring host, which has no PostgreSQL.
    // ---------------------------------------------------------------------

    /// The seven event readers, as production text.
    fn ov_event_readers() -> &'static str {
        let production = r5_production_source();
        let start = production
            .find("    async fn list_message_events_since(")
            .expect("test: the first event reader");
        let end = production
            .find("    async fn create_memory_draft(")
            .expect("test: the end of the event readers");
        &production[start..end]
    }

    /// OV-C: every `workspace` arm of every event reader carries the
    /// owner-isolation fragment (empty for a non-isolated reader, so its
    /// statement is byte-identical), and the session-context read renders its
    /// shared tiers through the shared seam.
    ///
    /// MUTATION GUARD: drop `{owner_fragment}` from any `workspace` arm and
    /// this goes red.
    #[test]
    fn postgres_event_readers_owner_qualify_the_workspace_arm() {
        let readers = ov_event_readers();
        assert!(
            !readers.contains("visibility = 'workspace'\n"),
            "no event reader may keep an unqualified workspace arm"
        );
        assert_eq!(
            readers.matches("visibility = 'workspace'{").count(),
            6,
            "six readers carry an owner fragment on their workspace arm"
        );
        assert!(readers.contains("session_context_shared_tiers(&owner_fragment)"));
        assert!(!readers.contains("visibility IN ('global', 'workspace')"));
    }

    fn ov_memory_event(
        owner_id: Option<&str>,
        visibility: MemoryVisibility,
        agent_id: Option<&str>,
        marker: &str,
    ) -> MemoryEventInput {
        MemoryEventInput {
            owner_id: owner_id.map(str::to_string),
            event_id: None,
            workspace_id: C03_WORKSPACE.to_string(),
            event_type: "task.spawned".to_string(),
            subject_table: "tasks".to_string(),
            subject_id: marker.to_string(),
            session_key: Some(C03_SESSION_KEY.to_string()),
            run_id: Some(marker.to_string()),
            parent_run_id: None,
            agent_id: agent_id.map(str::to_string),
            persona_id: None,
            visibility,
            payload_json: Some(serde_json::json!({ "task": marker }).to_string()),
        }
    }

    async fn ov_reads(mem: &PostgresMemory, principal: &MemoryPrincipal) -> Vec<(&'static str, Vec<String>)> {
        let contents = |events: Vec<MessageEvent>| events.into_iter().map(|event| event.content).collect::<Vec<_>>();
        let query = || SharedContextQuery {
            principal: principal.clone(),
            since_event_id: None,
            limit: 50,
            include_roles: Vec::new(),
        };
        vec![
            (
                "message recent",
                contents(
                    mem.list_message_events_recent(principal, 50)
                        .await
                        .expect("test: recent"),
                ),
            ),
            (
                "shared context",
                contents(mem.load_recent_shared_context(query()).await.expect("test: shared")),
            ),
            (
                "session context",
                contents(
                    mem.load_recent_session_context(SessionContextQuery {
                        principal: principal.clone(),
                        since_event_id: None,
                        limit: 50,
                        include_roles: Vec::new(),
                    })
                    .await
                    .expect("test: session"),
                ),
            ),
            (
                "memory recent",
                mem.list_memory_events_recent(principal, 50)
                    .await
                    .expect("test: memory recent")
                    .into_iter()
                    .map(|event| event.subject_id)
                    .collect(),
            ),
        ]
    }

    /// OV-C behavioural twin. **NOT RUN on this host: no PostgreSQL.**
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_owner_isolated_event_reads_exclude_another_owners_workspace_rows() {
        let db_url = live_postgres_url();
        let schema = format!("prx_ov_c_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
        let owner_a = c03_owner("issuer-a");
        let owner_b = c03_owner("issuer-b");
        for (owner, marker) in [
            (Some(owner_a.as_str()), "a-workspace"),
            (Some(owner_b.as_str()), "b-workspace"),
            (None, "null-workspace"),
        ] {
            mem.append_message_event(c03_message_input(
                C03_WORKSPACE,
                owner,
                C03_SESSION_KEY,
                MemoryVisibility::Workspace,
                marker,
            ))
            .await
            .expect("test: message event");
            mem.append_memory_event(ov_memory_event(owner, MemoryVisibility::Workspace, None, marker))
                .await
                .expect("test: memory event");
        }
        for (reader, seen) in ov_reads(&mem, &c03_principal(Some(&owner_b), true)).await {
            assert!(seen.iter().any(|row| row == "b-workspace"), "{reader}: {seen:?}");
            assert!(
                !seen.iter().any(|row| row == "a-workspace" || row == "null-workspace"),
                "{reader}: {seen:?}"
            );
        }
        // The shared-context feed is confined for every reader (OV-D).
        for (reader, seen) in ov_reads(&mem, &c03_principal(Some(&owner_b), false))
            .await
            .into_iter()
            .filter(|(reader, _)| *reader != "shared context")
        {
            assert!(
                seen.iter().any(|row| row == "a-workspace"),
                "{reader}: control: {seen:?}"
            );
        }
        drop(mem);
        r5_drop_schema(db_url, schema).await;
    }

    /// OV-E: the `private` arm of every event reader requires the owner for
    /// every reader, and the owner is bound whenever that fragment binds.
    ///
    /// MUTATION GUARD: put `{owner_fragment}` back on a private arm, or bind on
    /// `owner_fragment.bound_owner`, and this goes red.
    #[test]
    fn postgres_event_readers_require_the_owner_on_the_private_arm() {
        let readers = ov_event_readers();
        assert_eq!(readers.matches("){owner_required_fragment})").count(), 7);
        assert!(!readers.contains("){owner_fragment})"));
        assert_eq!(readers.matches("if owner_required_fragment.bound_owner {").count(), 7);
        assert!(!readers.contains("if owner_fragment.bound_owner {"));
        assert_eq!(
            readers.matches("owner_match_suffix(\n            PG_DIALECT,").count(),
            14,
            "each reader renders one isolation fragment and one owner-required fragment"
        );
    }

    /// OV-E behavioural twin. **NOT RUN on this host: no PostgreSQL.**
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_private_event_is_not_readable_by_a_same_named_agent_under_another_owner() {
        let db_url = live_postgres_url();
        let schema = format!("prx_ov_e_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
        let owner_a = c03_owner("issuer-a");
        let owner_b = c03_owner("issuer-b");
        mem.append_memory_event(ov_memory_event(
            Some(&owner_a),
            MemoryVisibility::Private,
            Some("researcher"),
            "a-private-event",
        ))
        .await
        .expect("test: memory event");
        let reader = |owner: Option<&str>| MemoryPrincipal {
            agent_id: Some("researcher".to_string()),
            ..c03_principal(owner, false)
        };
        let seen = |events: Vec<MemoryEvent>| events.into_iter().map(|event| event.subject_id).collect::<Vec<_>>();
        assert!(
            seen(
                mem.list_memory_events_recent(&reader(Some(&owner_b)), 50)
                    .await
                    .expect("test: b")
            )
            .is_empty(),
            "expected an empty value"
        );
        assert!(
            seen(
                mem.list_memory_events_recent(&reader(None), 50)
                    .await
                    .expect("test: none")
            )
            .is_empty(),
            "expected an empty value"
        );
        assert_eq!(
            seen(
                mem.list_memory_events_recent(&reader(Some(&owner_a)), 50)
                    .await
                    .expect("test: a")
            ),
            ["a-private-event"]
        );
        drop(mem);
        r5_drop_schema(db_url, schema).await;
    }

    /// OV-D: the shared-context feed's `workspace` arm requires the owner for
    /// every reader; the other five `workspace` arms keep the isolation-only
    /// fragment.
    ///
    /// MUTATION GUARD: put `{owner_fragment}` back on the shared-context
    /// `workspace` arm and this goes red.
    #[test]
    fn postgres_shared_context_feed_requires_the_owner_on_the_workspace_arm() {
        let shared = r5_method_text(r5_production_source(), "async fn load_recent_shared_context(");
        assert!(shared.contains("visibility = 'workspace'{owner_required_fragment}"));
        assert!(!shared.contains("visibility = 'workspace'{owner_fragment}"));
        assert_eq!(
            ov_event_readers()
                .matches("visibility = 'workspace'{owner_fragment}")
                .count(),
            5
        );
    }

    /// OV-D behavioural twin. **NOT RUN on this host: no PostgreSQL.**
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_shared_context_excludes_another_owners_workspace_events() {
        let db_url = live_postgres_url();
        let schema = format!("prx_ov_d_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
        let owner_a = c03_owner("issuer-a");
        let owner_b = c03_owner("issuer-b");
        for (owner, session, marker) in [
            (Some(owner_a.as_str()), "session-a", "owner a spawn task text"),
            (Some(owner_b.as_str()), "session-b-earlier", "owner b earlier event"),
            (None, "session-unowned", "unowned workspace event"),
        ] {
            mem.append_message_event(c03_message_input(
                C03_WORKSPACE,
                owner,
                session,
                MemoryVisibility::Workspace,
                marker,
            ))
            .await
            .expect("test: message event");
        }
        let principal = MemoryPrincipal {
            session_key: Some("session-b-now".to_string()),
            ..c03_principal(Some(&owner_b), false)
        };
        let seen = mem
            .load_recent_shared_context(SharedContextQuery {
                principal,
                since_event_id: None,
                limit: 24,
                include_roles: Vec::new(),
            })
            .await
            .expect("test: shared context")
            .into_iter()
            .map(|event| event.content)
            .collect::<Vec<_>>();
        assert_eq!(seen, ["owner b earlier event"]);
        drop(mem);
        r5_drop_schema(db_url, schema).await;
    }

    // ---------------------------------------------------------------------
    // OQ-8 / D-019 (B2): the vector RLS policies close their shared
    // `global`/`workspace` read arm to owner-isolated callers, so the policy
    // floor matches the shared SQL predicate instead of being wider than it.
    //
    // The policy text, the transaction flag and the call-site bits are pinned
    // offline below. The behavioural twin needs a live PostgreSQL and a role
    // RLS applies to; it is **written but not run** on the authoring host.
    // ---------------------------------------------------------------------

    fn b2_collapse_whitespace(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Split one policy DDL into its `USING` and `WITH CHECK` halves.
    fn b2_policy_halves(sql: &str) -> (String, String) {
        let sql = b2_collapse_whitespace(sql);
        let (using, check) = sql
            .split_once("WITH CHECK")
            .expect("test: the policy DDL has a WITH CHECK clause");
        (using.to_string(), check.to_string())
    }

    const B2_ISOLATION_GATE: &str = "COALESCE(current_setting('prx.owner_isolated', true), '') <> 'on'";

    #[test]
    fn vector_rls_shared_read_arm_is_closed_to_owner_isolated_callers() {
        for policy in ["memories_owner_isolation", "document_chunks_owner_isolation"] {
            let (using, check) = b2_policy_halves(&PostgresMemory::vector_rls_policy_sql("s.t", policy, true));
            assert!(
                using.contains(&format!(
                    "OR (visibility IN ('global', 'workspace') AND {B2_ISOLATION_GATE}))"
                )),
                "{policy}: the shared read arm must be gated on the caller not being \
                 owner-isolated: {using}"
            );
            assert!(
                using.contains("(owner_id = current_setting('prx.current_owner', true) OR"),
                "{policy}: the caller's own rows stay readable: {using}"
            );
            for required in ["prx.current_workspace", "prx.current_owner", "prx.rls_bypass"] {
                assert!(
                    using.contains(required) && check.contains(required),
                    "{policy}: {required}"
                );
            }
            assert!(
                !check.contains("visibility") && !check.contains("prx.owner_isolated"),
                "{policy}: writes stay owner exact and independent of the flag: {check}"
            );
            assert!(
                check.contains("AND owner_id = current_setting('prx.current_owner', true)"),
                "{policy}: {check}"
            );
        }
    }

    #[test]
    fn vector_rls_embedding_cache_policy_has_no_shared_arm() {
        let (using, check) = b2_policy_halves(&PostgresMemory::vector_rls_policy_sql(
            "s.t",
            "embedding_cache_owner_isolation",
            false,
        ));
        for half in [&using, &check] {
            assert!(!half.contains("visibility"), "{half}");
            assert!(!half.contains("prx.owner_isolated"), "{half}");
            assert!(
                half.contains("owner_id = current_setting('prx.current_owner', true)"),
                "{half}"
            );
        }
    }

    /// `apply_rls_context_raw` must set the flag the policy reads, always
    /// (both values), transaction-local, and only on the non-system path.
    #[test]
    fn apply_rls_context_raw_sets_the_owner_isolation_flag() {
        let source = r5_production_source();
        let start = source
            .find("    fn apply_rls_context_raw<C: GenericClient>(")
            .expect("test: apply_rls_context_raw exists");
        let body = &source[start..];
        let body = &body[..body.find("\n    }\n").expect("test: apply_rls_context_raw ends")];
        let bypass_return = body.find("return Ok(());").expect("test: system early return");
        let flag = body
            .find("set_config('prx.owner_isolated', $2, true)")
            .expect("test: the owner isolation flag is set transaction-local");
        assert!(flag > bypass_return, "the flag is set on the non-system path");
        assert!(body.contains("if owner_isolated { \"on\" } else { \"off\" }"), "{body}");
    }

    /// Every production call site passes the caller's isolation bit. A literal
    /// `false` is only acceptable where the policy has no shared arm (the
    /// embedding cache); ingest passes `true` because all its statements are
    /// exact on the document's own owner.
    #[test]
    fn apply_rls_context_raw_call_sites_pass_the_callers_isolation_bit() {
        let source = r5_without_comments(r5_production_source());
        let mut last_args = Vec::new();
        for (offset, _) in source.match_indices("Self::apply_rls_context_raw(") {
            let call = &source[offset..];
            let call = &call[..call.find(")?;").expect("test: call ends with )?;")];
            let args = b2_collapse_whitespace(&call["Self::apply_rls_context_raw(".len()..]);
            let last = args
                .trim_end_matches(',')
                .rsplit(',')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            last_args.push(last);
        }
        let count = |value: &str| last_args.iter().filter(|arg| arg.as_str() == value).count();
        assert_eq!(count("false"), 2, "only the two embedding-cache calls: {last_args:?}");
        assert_eq!(count("true"), 1, "only ingest: {last_args:?}");
        assert!(
            last_args
                .iter()
                .all(|arg| ["false", "true", "rls_owner_isolated", "owner_isolated"].contains(&arg.as_str())),
            "{last_args:?}"
        );
        assert!(count("rls_owner_isolated") >= 7, "{last_args:?}");
    }

    #[test]
    fn vector_rls_owner_isolated_reads_is_a_recorded_migration() {
        let registry = PostgresMemory::memory_schema_migration_registry();
        assert!(
            registry.windows(2).all(|pair| matches!(pair, [a, b] if a.0 < b.0)),
            "migration versions stay strictly increasing"
        );
        let (_, name, descriptor) = registry
            .iter()
            .find(|(version, _, _)| *version == 23)
            .expect("test: OQ-8 migration 23 is registered");
        assert_eq!(*name, "vector_rls_owner_isolated_reads");
        assert_eq!(
            *descriptor,
            "memories + document_chunks POLICY *_owner_isolation USING: shared global/workspace read arm additionally requires COALESCE(current_setting('prx.owner_isolated', true), '') <> 'on'; prx.owner_isolated set transaction-local from require_owner_match; WITH CHECK and embedding_cache unchanged",
            "migration 23's recorded text must not change (checksum mismatch bails at startup)"
        );
        let (_, _, a04) = registry
            .iter()
            .find(|(version, _, _)| *version == 21)
            .expect("test: A04 migration 21 is registered");
        assert!(
            a04.starts_with("memories + document_chunks + embedding_cache scoped by workspace_id/owner_id"),
            "migration 21's recorded text must not change (checksum mismatch bails at startup)"
        );
    }

    // ---------------------------------------------------------------------
    // D-028 (B2-f): `public` memory rows are readable across owners through a
    // separate, read-only `FOR SELECT` policy; the owner-isolation policy that
    // also governs writes never names `public`. Pinned offline; the live
    // behaviour is covered by the recall matrix above, NOT RUN on this host.
    // ---------------------------------------------------------------------

    #[test]
    fn memories_public_read_policy_is_read_only_and_workspace_scoped() {
        let sql = b2_collapse_whitespace(&PostgresMemory::memories_public_read_policy_sql(
            "s.memories",
            MEMORIES_PUBLIC_READ_POLICY,
        ));
        assert!(
            sql.contains("CREATE POLICY memories_public_read ON s.memories FOR SELECT USING ("),
            "the public grant must be a FOR SELECT policy on memories: {sql}"
        );
        assert!(
            !sql.contains("WITH CHECK"),
            "a read-only policy has no WITH CHECK: {sql}"
        );
        for command in ["FOR ALL", "FOR UPDATE", "FOR DELETE", "FOR INSERT", "AS RESTRICTIVE"] {
            assert!(!sql.contains(command), "{command}: {sql}");
        }
        for required in [
            "NULLIF(current_setting('prx.current_workspace', true), '') IS NOT NULL",
            "NULLIF(current_setting('prx.current_owner', true), '') IS NOT NULL",
            "workspace_id = current_setting('prx.current_workspace', true)",
            "visibility = 'public'",
        ] {
            assert!(sql.contains(required), "{required}: {sql}");
        }
        assert!(
            !sql.contains("prx.owner_isolated") && !sql.contains("prx.rls_bypass") && !sql.contains("owner_id"),
            "public reads are the same for every caller and are not an owner grant: {sql}"
        );
    }

    #[test]
    fn owner_isolation_policies_never_grant_public_rows() {
        for (policy, shared) in [
            ("memories_owner_isolation", true),
            ("document_chunks_owner_isolation", true),
            ("embedding_cache_owner_isolation", false),
        ] {
            let sql = PostgresMemory::vector_rls_policy_sql("s.t", policy, shared);
            assert!(
                !sql.contains("'public'"),
                "{policy}: the policy that also governs writes must not name `public`: {sql}"
            );
        }
    }

    /// `init_vector_rls` installs the public grant exactly once, on the
    /// memories table, inside the same transaction as the isolation policies.
    #[test]
    fn init_vector_rls_installs_the_public_read_policy_on_memories_only() {
        let source = r5_production_source();
        let start = source
            .find("    fn init_vector_rls(")
            .expect("test: init_vector_rls exists");
        let body = &source[start..];
        let body = b2_collapse_whitespace(&body[..body.find("\n    }\n").expect("test: init_vector_rls ends")]);
        assert_eq!(body.matches("memories_public_read_policy_sql(").count(), 1, "{body}");
        assert!(
            body.contains(
                "tx.batch_execute(&Self::memories_public_read_policy_sql( qualified_memories_table, MEMORIES_PUBLIC_READ_POLICY, ))?; tx.commit()?;"
            ),
            "{body}"
        );
    }

    /// Migration 24's descriptor is checksummed into every database that
    /// recorded it, so any edit to the text bails at startup there. It is
    /// pinned verbatim and looked up by version, so appending migration 25
    /// leaves this test alone (R-B2 F-6).
    #[test]
    fn memories_public_read_policy_is_a_recorded_migration() {
        let (_, name, descriptor) = PostgresMemory::memory_schema_migration_registry()
            .iter()
            .find(|(version, _, _)| *version == 24)
            .expect("test: migration 24 is registered");
        assert_eq!(*name, "memories_public_read_policy");
        assert_eq!(
            *descriptor,
            "memories POLICY memories_public_read FOR SELECT USING: transaction-local workspace and owner present AND workspace_id = prx.current_workspace AND visibility = 'public'; permissive, read only (UPDATE/DELETE/INSERT still need memories_owner_isolation); independent of prx.owner_isolated; document_chunks and embedding_cache unchanged"
        );
    }

    /// R-B2 F-5, the offline half: the evidence accepts exactly the policy
    /// set `init_vector_rls` installs and refuses a stray policy, a missing
    /// one, and a restrictive or role-limited variant.
    #[test]
    fn policy_inventory_is_exact_permissive_and_public() {
        let row = |name: &str, permissive: &str, roles: &[&str]| PolicyRow {
            name: name.to_string(),
            permissive: permissive.to_string(),
            roles: roles.iter().map(ToString::to_string).collect(),
        };
        let expected = ["memories_owner_isolation", MEMORIES_PUBLIC_READ_POLICY];
        let installed = vec![
            row("memories_owner_isolation", "PERMISSIVE", &["public"]),
            row(MEMORIES_PUBLIC_READ_POLICY, "PERMISSIVE", &["public"]),
        ];
        check_policy_inventory("memories", &installed, &expected).expect("test: the installed set passes");

        let mut stray = installed;
        stray.push(row("dba_convenience_read", "PERMISSIVE", &["public"]));
        assert!(
            check_policy_inventory("memories", &stray, &expected).is_err(),
            "a stray policy"
        );

        let missing = vec![row("memories_owner_isolation", "PERMISSIVE", &["public"])];
        assert!(
            check_policy_inventory("memories", &missing, &expected).is_err(),
            "a missing policy"
        );

        let restrictive = vec![
            row("memories_owner_isolation", "PERMISSIVE", &["public"]),
            row(MEMORIES_PUBLIC_READ_POLICY, "RESTRICTIVE", &["public"]),
        ];
        assert!(
            check_policy_inventory("memories", &restrictive, &expected).is_err(),
            "a restrictive variant"
        );

        let role_limited = vec![
            row("memories_owner_isolation", "PERMISSIVE", &["public"]),
            row(MEMORIES_PUBLIC_READ_POLICY, "PERMISSIVE", &["reporting"]),
        ];
        assert!(
            check_policy_inventory("memories", &role_limited, &expected).is_err(),
            "a role-limited variant"
        );
    }

    async fn b2_make_workspace(mem: &PostgresMemory, key: &str) {
        let client_slot = mem.client.clone();
        let qualified_table = mem.qualified_table.clone();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || -> Result<()> {
            client_slot.with_client(|client| {
                client.execute(
                    &format!("UPDATE {qualified_table} SET visibility = 'workspace' WHERE key = $1"),
                    &[&key],
                )?;
                Ok(())
            })
        })
        .await
        .expect("test: visibility task")
        .expect("test: mark the row workspace-visible");
    }

    /// OQ-8 behavioural twin: under a role RLS applies to, a raw statement
    /// with no SQL owner predicate still sees another owner's `workspace` row
    /// for a non-isolated context, and does not for an isolated one.
    /// **NOT RUN on this host: no PostgreSQL.**
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
    async fn postgres_vector_rls_hides_other_owners_workspace_rows_from_isolated_callers() {
        let db_url = live_postgres_url();
        let schema = format!("prx_b2_rls_{}", Uuid::new_v4().simple());
        let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
        let owner_a = c03_owner("issuer-a");
        let owner_b = c03_owner("issuer-b");
        r5_seed(&mem, &owner_a, "b2-a", "b2 workspace note").await;
        b2_make_workspace(&mem, "b2-a").await;

        let qualified_table = mem.qualified_table.clone();
        let url = db_url.clone();
        let (shared, isolated) = tokio::task::spawn_blocking(move || -> Result<(i64, i64)> {
            let mut client = url.parse::<postgres::Config>()?.connect(NoTls)?;
            let row = client.query_one(
                "SELECT rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user",
                &[],
            )?;
            let (is_super, bypasses): (bool, bool) = (row.get(0), row.get(1));
            anyhow::ensure!(
                !is_super && !bypasses,
                "this test only proves anything under a NOSUPERUSER/NOBYPASSRLS role"
            );
            let count_stmt = format!("SELECT COUNT(*) FROM {qualified_table} WHERE key = 'b2-a'");
            let mut probe = |isolated: bool| -> Result<i64> {
                let mut tx = client.transaction()?;
                PostgresMemory::apply_rls_context_raw(&mut tx, false, Some(C03_WORKSPACE), Some(&owner_b), isolated)?;
                let count: i64 = tx.query_one(&count_stmt, &[])?.get(0);
                tx.commit()?;
                Ok(count)
            };
            Ok((probe(false)?, probe(true)?))
        })
        .await
        .expect("test: rls probe task")
        .expect("test: rls probe");
        assert_eq!(shared, 1, "a non-isolated context keeps the tenant-wide workspace read");
        assert_eq!(
            isolated, 0,
            "an owner-isolated context must not see another owner's workspace row"
        );

        drop(mem);
        r5_drop_schema(db_url, schema).await;
    }
}

impl PostgresMemory {
    async fn append_message_event_ordinary(&self, input: MessageEventInput) -> Result<MessageEvent> {
        input.validate()?;
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();

        tokio::task::spawn_blocking(move || -> Result<MessageEvent> {
            let now = Utc::now();
            let event_id = input.event_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let content_hash = Self::content_hash(&input.content);
            let visibility = input.visibility.as_str().to_string();
            let source = input.source.as_str().to_string();
            let source_ref_json = serde_json::to_string(&input.source)?;
            let subject_ref_json = input.subject.as_ref().map(serde_json::to_string).transpose()?;
            let config_generation_id = input.config_generation_id.map(i64::try_from).transpose()?;

            let insert_stmt = format!(
                "
                INSERT INTO {qualified_message_events_table} (
                    event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                    parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                    sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                    goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                    content, content_hash, raw_payload_json, visibility, created_at, updated_at
                )
                VALUES (
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                    $11, $12, $13, $14, $15, $16, $17, $18, $19, $20,
                    $21, $22, $23, $24, $25, $26, $27, $28, $29, $30,
                    $31
                )
                ON CONFLICT DO NOTHING
                "
            );
            let select_stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                       goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                       content, content_hash, raw_payload_json, visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE event_id = $1
                   OR ($2::TEXT IS NOT NULL AND workspace_id = $3 AND idempotency_key = $2)
                ORDER BY CASE WHEN event_id = $1 THEN 0 ELSE 1 END
                LIMIT 1
                "
            );
            let outbox_stmt = format!(
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id, session_key,
                    agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, $4, 'message_events', $5, $6, $7, $8, $9, NULL, $10)
                ON CONFLICT (event_id) DO NOTHING
                "
            );

            client.with_client(|client| {
                let mut tx = client.transaction()?;
                let inserted = tx.execute(
                    &insert_stmt,
                    &[
                        &event_id,
                        &input.idempotency_key,
                        &input.workspace_id,
                        &input.owner_id,
                        &source,
                        &input.channel,
                        &input.session_key,
                        &input.parent_session_key,
                        &input.run_id,
                        &input.parent_run_id,
                        &input.agent_id,
                        &input.persona_id,
                        &input.sender,
                        &input.recipient,
                        &input.role,
                        &input.event_type,
                        &source_ref_json,
                        &subject_ref_json,
                        &input.goal_id,
                        &input.causation_event_id,
                        &input.correlation_id,
                        &input.attempt_id,
                        &input.lease_epoch,
                        &config_generation_id,
                        &input.config_source_revision,
                        &input.content,
                        &content_hash,
                        &input.raw_payload_json,
                        &visibility,
                        &now,
                        &now,
                    ],
                )?;
                let row = tx.query_one(&select_stmt, &[&event_id, &input.idempotency_key, &input.workspace_id])?;
                let event = Self::row_to_message_event(&row)?;
                if inserted > 0 {
                    tx.execute(
                        &outbox_stmt,
                        &[
                            &Uuid::new_v4().to_string(),
                            &event.workspace_id,
                            &event.owner_id,
                            &event.event_type,
                            &event.event_id,
                            &event.session_key,
                            &event.agent_id,
                            &event.persona_id,
                            &event.visibility.as_str(),
                            &Utc::now(),
                        ],
                    )?;
                }
                tx.commit()?;
                Ok(event)
            })
        })
        .await?
    }
}

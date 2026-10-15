//! Blocking PostgreSQL connection pool and its per-checkout RLS session context.
use super::POSTGRES_CONNECT_TIMEOUT_CAP_SECS;
use anyhow::{Context, Result};
use parking_lot::{Condvar, Mutex, MutexGuard};
use postgres::{Client, NoTls};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
pub(super) const POSTGRES_DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 15;

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
    pub(super) const fn reset_statement(self) -> &'static str {
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
    pub(super) db_url: String,
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

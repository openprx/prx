use crate::config::Config;
use crate::memory::filter::{MemorySafetyFilter, SourceMetadata};
use crate::memory::{MemoryBackendKind, classify_memory_backend, effective_memory_backend_name};
use crate::security::policy::ResourceRiskLevel;
use crate::security::{SecurityPolicy, SideEffectGate};
use crate::self_system::evolution::record::Actor;
use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header::{AUTHORIZATION, RETRY_AFTER}},
    response::{IntoResponse, Response},
    routing::post,
};
use chrono::Utc;
use parking_lot::Mutex as ParkingMutex;
#[cfg(test)]
use postgres::{Client as PostgresClient, NoTls};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebhookEvent {
    pub source: String,
    pub event_type: String,
    pub project: Option<String>,
    pub external_id: String,
    pub external_url: Option<String>,
    pub title: String,
    pub content: String,
    pub actor: Option<String>,
    pub timestamp: String,
}

#[derive(Clone)]
struct WebhookState {
    token: Arc<str>,
    /// Optional HMAC signing secret for `X-Webhook-Signature` verification.
    signing_secret: Option<Arc<str>>,
    repository: Arc<dyn WebhookRepository>,
    rate_limiter: Arc<WebhookRateLimiter>,
    /// Security policy governing whether verified inbound events may be
    /// persisted into the topic store (FIX-P1-03). Under autonomy=ReadOnly the
    /// standalone webhook server must not write, so the persist step is gated
    /// through [`SideEffectGate`]. Held by value (it is `Clone`) so the gate can
    /// borrow it without extra allocation.
    security: Arc<SecurityPolicy>,
}

#[derive(Debug, Serialize)]
struct WebhookAck {
    topic_id: String,
}

const WEBHOOK_RATE_LIMIT_WINDOW_SECS: u64 = 60;
const WEBHOOK_DEFAULT_RATE_LIMIT_PER_MINUTE: u32 = 60;
const WEBHOOK_INGESTION_LEASE_SECS: i64 = 30;
const WEBHOOK_MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;

#[derive(Debug)]
struct WebhookRateLimiter {
    limit_per_window: u32,
    window: Duration,
    requests: Mutex<Vec<Instant>>,
}

impl WebhookRateLimiter {
    fn new(limit_per_window: u32, window: Duration) -> Self {
        Self {
            limit_per_window,
            window,
            requests: Mutex::new(Vec::new()),
        }
    }

    async fn allow(&self) -> bool {
        if self.limit_per_window == 0 {
            return true;
        }

        let now = Instant::now();
        let cutoff = now.checked_sub(self.window).unwrap_or_else(Instant::now);
        let mut requests = self.requests.lock().await;
        requests.retain(|instant| *instant > cutoff);
        if requests.len() >= self.limit_per_window as usize {
            return false;
        }
        requests.push(now);
        true
    }
}

#[derive(Debug, Clone)]
struct WebhookIngestionClaim {
    ingestion_key: String,
    event_identity: String,
    request_hash: String,
    generation: i64,
}

#[derive(Debug)]
enum WebhookClaimOutcome {
    Acquired(WebhookIngestionClaim),
    Committed { topic_id: String },
    Processing,
    Conflict,
}

#[async_trait]
trait WebhookRepository: Send + Sync {
    async fn claim(
        &self,
        ingestion_key: String,
        event_identity: String,
        request_hash: String,
        event: &WebhookEvent,
    ) -> Result<WebhookClaimOutcome>;

    async fn commit(
        &self,
        claim: &WebhookIngestionClaim,
        event: &WebhookEvent,
        memory_content: &str,
        memory_saved: bool,
    ) -> Result<String>;

    async fn fail(&self, claim: &WebhookIngestionClaim, error: &str) -> Result<()>;
}

#[derive(Clone)]
pub(crate) struct WebhookRepositoryHandle {
    repository: Arc<dyn WebhookRepository>,
}

#[derive(Debug)]
struct SqliteWebhookRepository {
    conn: Arc<ParkingMutex<Connection>>,
    workspace_id: Arc<str>,
}

/// Webhook ingestion runs on externally-originated payloads, so every one of
/// its checkouts stays subject to row-level security.
///
/// Before this repository shared the memory backend's pool it opened plain
/// connections that never set `prx.rls_bypass`, and RLS therefore denied its
/// writes to `memories`. The pool resets each checkout into the *trusted*
/// system context, so reusing it would have silently handed webhook ingestion a
/// bypass it never had. Requesting the restricted context keeps the boundary
/// exactly where it was; the pool restores the system context for the next
/// caller of the same connection.
const WEBHOOK_SESSION_CONTEXT: crate::memory::postgres::PostgresSessionContext =
    crate::memory::postgres::PostgresSessionContext::Restricted;

#[derive(Clone)]
struct PostgresWebhookRepository {
    pool: Arc<crate::memory::postgres::PostgresConnectionPool>,
    workspace_id: Arc<str>,
    qualified_memories: Arc<str>,
    qualified_memory_events: Arc<str>,
    qualified_topics: Arc<str>,
    qualified_topic_participants: Arc<str>,
    qualified_ingestions: Arc<str>,
}

#[derive(Debug)]
struct ExistingIngestion {
    ingestion_key: String,
    event_identity: String,
    request_hash: String,
    status: String,
    generation: i64,
    lease_expires_at: Option<i64>,
    topic_id: Option<String>,
}

impl SqliteWebhookRepository {
    fn new(db_path: PathBuf, workspace_id: String) -> Result<Self> {
        ensure_memory_schema(&db_path)?;
        let mut conn = Connection::open(&db_path)
            .with_context(|| format!("failed to open webhook repository {}", db_path.display()))?;
        // Same policy as the pooled connections onto this file: a lock held by
        // another writer makes this call wait (and warn), never fail.
        crate::runtime::sqlite_pool::install_busy_handler(&mut conn)
            .context("failed to install webhook repository busy handler")?;
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS webhook_ingestions (
                 ingestion_key    TEXT PRIMARY KEY,
                 event_identity   TEXT NOT NULL UNIQUE,
                 request_hash     TEXT NOT NULL,
                 source           TEXT NOT NULL,
                 project          TEXT,
                 external_id      TEXT NOT NULL,
                 event_type       TEXT NOT NULL,
                 status           TEXT NOT NULL CHECK(status IN ('pending', 'committed', 'failed')),
                 generation       INTEGER NOT NULL,
                 lease_expires_at INTEGER,
                 topic_id         TEXT,
                 memory_key       TEXT,
                 last_error       TEXT,
                 created_at       TEXT NOT NULL,
                 updated_at       TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_webhook_ingestions_status
                 ON webhook_ingestions(status, lease_expires_at);
             CREATE INDEX IF NOT EXISTS idx_webhook_ingestions_external
                 ON webhook_ingestions(source, project, external_id, event_type);",
        )
        .context("failed to initialize durable webhook ingestion schema")?;
        Ok(Self {
            conn: Arc::new(ParkingMutex::new(conn)),
            workspace_id: Arc::from(workspace_id),
        })
    }

    fn existing_ingestion(
        tx: &Transaction<'_>,
        ingestion_key: &str,
        event_identity: &str,
    ) -> Result<Option<ExistingIngestion>> {
        tx.query_row(
            "SELECT ingestion_key, event_identity, request_hash, status, generation,
                    lease_expires_at, topic_id
               FROM webhook_ingestions
              WHERE ingestion_key = ?1 OR event_identity = ?2
              ORDER BY CASE WHEN ingestion_key = ?1 THEN 0 ELSE 1 END
              LIMIT 1",
            params![ingestion_key, event_identity],
            |row| {
                Ok(ExistingIngestion {
                    ingestion_key: row.get(0)?,
                    event_identity: row.get(1)?,
                    request_hash: row.get(2)?,
                    status: row.get(3)?,
                    generation: row.get(4)?,
                    lease_expires_at: row.get(5)?,
                    topic_id: row.get(6)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    fn claim_blocking(
        &self,
        ingestion_key: String,
        event_identity: String,
        request_hash: String,
        event: &WebhookEvent,
    ) -> Result<WebhookClaimOutcome> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now_unix = Utc::now().timestamp();
        let now = Utc::now().to_rfc3339();
        let lease_expires_at = now_unix.saturating_add(WEBHOOK_INGESTION_LEASE_SECS);

        let outcome = if let Some(existing) = Self::existing_ingestion(&tx, &ingestion_key, &event_identity)? {
            if existing.request_hash != request_hash || existing.event_identity != event_identity {
                WebhookClaimOutcome::Conflict
            } else if existing.status == "committed" {
                let topic_id = existing
                    .topic_id
                    .context("committed webhook ingestion is missing topic identity")?;
                WebhookClaimOutcome::Committed { topic_id }
            } else if existing.status == "pending" && existing.lease_expires_at.is_some_and(|lease| lease > now_unix) {
                WebhookClaimOutcome::Processing
            } else {
                let generation = existing
                    .generation
                    .checked_add(1)
                    .context("webhook ingestion generation exhausted")?;
                tx.execute(
                    "UPDATE webhook_ingestions
                        SET status = 'pending', generation = ?2, lease_expires_at = ?3,
                            topic_id = NULL, memory_key = NULL, last_error = NULL, updated_at = ?4
                      WHERE ingestion_key = ?1",
                    params![existing.ingestion_key, generation, lease_expires_at, now],
                )?;
                WebhookClaimOutcome::Acquired(WebhookIngestionClaim {
                    ingestion_key: existing.ingestion_key,
                    event_identity,
                    request_hash,
                    generation,
                })
            }
        } else {
            tx.execute(
                "INSERT INTO webhook_ingestions (
                    ingestion_key, event_identity, request_hash, source, project, external_id,
                    event_type, status, generation, lease_expires_at, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', 1, ?8, ?9, ?9)",
                params![
                    ingestion_key,
                    event_identity,
                    request_hash,
                    event.source,
                    event.project,
                    event.external_id,
                    event.event_type,
                    lease_expires_at,
                    now,
                ],
            )?;
            WebhookClaimOutcome::Acquired(WebhookIngestionClaim {
                ingestion_key,
                event_identity,
                request_hash,
                generation: 1,
            })
        };

        tx.commit()?;
        Ok(outcome)
    }

    fn commit_blocking(
        &self,
        claim: &WebhookIngestionClaim,
        event: &WebhookEvent,
        memory_content: &str,
        memory_saved: bool,
    ) -> Result<String> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339();
        let owns_claim: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM webhook_ingestions
                 WHERE ingestion_key = ?1 AND generation = ?2 AND status = 'pending'
                   AND event_identity = ?3 AND request_hash = ?4
             )",
            params![
                claim.ingestion_key,
                claim.generation,
                claim.event_identity,
                claim.request_hash,
            ],
            |row| row.get(0),
        )?;
        if !owns_claim {
            anyhow::bail!("webhook ingestion ownership was lost before commit");
        }
        let topic_id = webhook_resolve_topic(&tx, event)?;

        if let Some(url) = event.external_url.as_deref() {
            tx.execute(
                "UPDATE topics
                    SET external_url = COALESCE(external_url, ?1), updated_at = ?2
                  WHERE id = ?3",
                params![url, now, topic_id],
            )?;
        }

        let system_sender = format!("system:{}", event.source);
        crate::memory::topic::add_participant(&tx, &topic_id, &system_sender, "observer")?;
        match event.event_type.as_str() {
            "issue.closed" => crate::memory::topic::update_topic_status(&tx, &topic_id, "resolved")?,
            "issue.reopened" => crate::memory::topic::update_topic_status(&tx, &topic_id, "open")?,
            _ => crate::memory::topic::touch_topic(&tx, &topic_id)?,
        }

        let memory_key = format!("webhook:{}:{}:{}", event.source, event.external_id, claim.ingestion_key);
        if memory_saved {
            let memory_id = Uuid::new_v4().to_string();
            let coalesce = crate::memory::sqlite::scope_coalesce_set("memories");
            let scope_guard = crate::memory::sqlite::scope_conflict_guard("memories", false);
            let changed = tx.execute(
                &format!(
                    "INSERT INTO memories (
                    id, key, content, category, created_at, updated_at, workspace_id,
                    owner_id, source, channel, chat_type, chat_id, sender_id, raw_sender,
                    topic_id, visibility, sensitivity, risk_signals, policy_version, ma_scope_id
                 ) VALUES (
                    ?1, ?2, ?3, 'conversation', ?4, ?4, ?5,
                    ?6, 'webhook', 'webhook', 'dm', ?7, ?6, ?6,
                    ?8, 'owner', 'normal', '[]', 1, NULL
                 )
                 ON CONFLICT(key) DO UPDATE SET
                    content = excluded.content,
                    updated_at = excluded.updated_at,
                    topic_id = excluded.topic_id,
                    {coalesce}
                 {scope_guard}"
                ),
                params![
                    memory_id,
                    memory_key,
                    memory_content,
                    event.timestamp,
                    self.workspace_id.as_ref(),
                    system_sender,
                    format!("{}:{}", event.source, event.external_id),
                    topic_id,
                ],
            )?;
            if changed == 0 {
                return Err(
                    crate::memory::owner_predicate::OwnerScopedStoreRefusal::KeyNotAvailable { key: memory_key }.into(),
                );
            }
        }

        let payload_json = serde_json::json!({
            "source": event.source,
            "project": event.project,
            "external_id": event.external_id,
            "event_type": event.event_type,
            "topic_id": topic_id,
            "memory_saved": memory_saved,
        })
        .to_string();
        tx.execute(
            "INSERT INTO memory_events (
                event_id, workspace_id, event_type, subject_table, subject_id,
                visibility, payload_json, created_at
             ) VALUES (?1, ?2, 'webhook.event.committed', 'webhook_ingestions', ?3,
                       'owner', ?4, ?5)",
            params![
                Uuid::new_v4().to_string(),
                self.workspace_id.as_ref(),
                claim.ingestion_key,
                payload_json,
                now,
            ],
        )?;

        let updated = tx.execute(
            "UPDATE webhook_ingestions
                SET status = 'committed', lease_expires_at = NULL, topic_id = ?3,
                    memory_key = ?4, last_error = NULL, updated_at = ?5
              WHERE ingestion_key = ?1 AND generation = ?2 AND status = 'pending'
                AND event_identity = ?6 AND request_hash = ?7",
            params![
                claim.ingestion_key,
                claim.generation,
                topic_id,
                memory_saved.then_some(memory_key),
                now,
                claim.event_identity,
                claim.request_hash,
            ],
        )?;
        if updated != 1 {
            anyhow::bail!("webhook ingestion ownership was lost before commit");
        }

        tx.commit()?;
        Ok(topic_id)
    }

    fn fail_blocking(&self, claim: &WebhookIngestionClaim, error: &str) -> Result<()> {
        let error = error.chars().take(1024).collect::<String>();
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE webhook_ingestions
                SET status = 'failed', lease_expires_at = NULL, last_error = ?3, updated_at = ?4
              WHERE ingestion_key = ?1 AND generation = ?2 AND status = 'pending'",
            params![claim.ingestion_key, claim.generation, error, now],
        )?;
        Ok(())
    }
}

#[async_trait]
impl WebhookRepository for SqliteWebhookRepository {
    async fn claim(
        &self,
        ingestion_key: String,
        event_identity: String,
        request_hash: String,
        event: &WebhookEvent,
    ) -> Result<WebhookClaimOutcome> {
        let repository = self.clone_for_worker();
        let event = event.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            repository.claim_blocking(ingestion_key, event_identity, request_hash, &event)
        })
        .await
        .context("webhook claim worker panicked")?
    }

    async fn commit(
        &self,
        claim: &WebhookIngestionClaim,
        event: &WebhookEvent,
        memory_content: &str,
        memory_saved: bool,
    ) -> Result<String> {
        let repository = self.clone_for_worker();
        let claim = claim.clone();
        let event = event.clone();
        let memory_content = memory_content.to_string();
        crate::runtime::blocking::spawn_blocking(move || {
            repository.commit_blocking(&claim, &event, &memory_content, memory_saved)
        })
        .await
        .context("webhook commit worker panicked")?
    }

    async fn fail(&self, claim: &WebhookIngestionClaim, error: &str) -> Result<()> {
        let repository = self.clone_for_worker();
        let claim = claim.clone();
        let error = error.to_string();
        crate::runtime::blocking::spawn_blocking(move || repository.fail_blocking(&claim, &error))
            .await
            .context("webhook failure worker panicked")?
    }
}

impl SqliteWebhookRepository {
    fn clone_for_worker(&self) -> Self {
        Self {
            conn: Arc::clone(&self.conn),
            workspace_id: Arc::clone(&self.workspace_id),
        }
    }
}

impl PostgresWebhookRepository {
    fn new(
        db_url: &str,
        schema: &str,
        memory_table: &str,
        connect_timeout_secs: Option<u64>,
        pool_max_size: usize,
        workspace_id: String,
    ) -> Result<Self> {
        crate::memory::postgres::validate_identifier(schema, "storage schema")?;
        crate::memory::postgres::validate_identifier(memory_table, "storage table")?;

        // Initialize the authoritative memory tables through the configured
        // backend before attaching webhook-owned topic/ingestion projections.
        let _memory = crate::memory::PostgresMemory::new(db_url, schema, memory_table, connect_timeout_secs)?;

        let schema_ident = crate::memory::postgres::quote_identifier(schema);
        let qualify_related = |suffix: &str| -> Result<String> {
            let table = crate::memory::postgres::related_table_name(memory_table, suffix)?;
            Ok(format!(
                "{schema_ident}.{}",
                crate::memory::postgres::quote_identifier(&table)
            ))
        };
        let qualified_memories = format!(
            "{schema_ident}.{}",
            crate::memory::postgres::quote_identifier(memory_table)
        );
        let qualified_memory_events = qualify_related("_memory_events")?;
        let qualified_topics = qualify_related("_topics")?;
        let qualified_topic_participants = qualify_related("_topic_participants")?;
        let qualified_ingestions = qualify_related("_webhook_ingestions")?;
        // PostgreSQL derives an index's schema from its target table and does
        // not accept a schema-qualified index name in CREATE INDEX.
        let related_index = |suffix: &str| -> Result<String> {
            let index = crate::memory::postgres::related_table_name(memory_table, suffix)?;
            Ok(crate::memory::postgres::quote_identifier(&index))
        };
        let ingestion_status_index = related_index("_wh_ingest_status_idx")?;
        let ingestion_external_index = related_index("_wh_ingest_external_idx")?;
        let topics_external_index = related_index("_wh_topics_external_idx")?;

        let schema_sql = format!(
            "
                CREATE TABLE IF NOT EXISTS {qualified_topics} (
                    id TEXT PRIMARY KEY,
                    title TEXT NOT NULL,
                    project TEXT NOT NULL,
                    external_id TEXT,
                    fingerprint TEXT NOT NULL UNIQUE,
                    status TEXT NOT NULL DEFAULT 'open',
                    external_url TEXT,
                    created_at TIMESTAMPTZ NOT NULL,
                    updated_at TIMESTAMPTZ NOT NULL,
                    resolved_at TIMESTAMPTZ
                );
                CREATE UNIQUE INDEX IF NOT EXISTS {topics_external_index}
                    ON {qualified_topics}(project, external_id)
                    WHERE external_id IS NOT NULL;

                CREATE TABLE IF NOT EXISTS {qualified_topic_participants} (
                    topic_id TEXT NOT NULL,
                    user_id TEXT NOT NULL,
                    role TEXT NOT NULL,
                    joined_at TIMESTAMPTZ NOT NULL,
                    PRIMARY KEY(topic_id, user_id)
                );

                CREATE TABLE IF NOT EXISTS {qualified_ingestions} (
                    ingestion_key TEXT PRIMARY KEY,
                    event_identity TEXT NOT NULL UNIQUE,
                    request_hash TEXT NOT NULL,
                    source TEXT NOT NULL,
                    project TEXT,
                    external_id TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    status TEXT NOT NULL CHECK(status IN ('pending', 'committed', 'failed')),
                    generation BIGINT NOT NULL,
                    lease_expires_at BIGINT,
                    topic_id TEXT,
                    memory_key TEXT,
                    last_error TEXT,
                    created_at TIMESTAMPTZ NOT NULL,
                    updated_at TIMESTAMPTZ NOT NULL
                );
                CREATE INDEX IF NOT EXISTS {ingestion_status_index}
                    ON {qualified_ingestions}(status, lease_expires_at);
                CREATE INDEX IF NOT EXISTS {ingestion_external_index}
                    ON {qualified_ingestions}(source, project, external_id, event_type);
                "
        );
        // Share the memory backend's waiting pool instead of pinning one
        // connection: webhook ingestion claims are per-request, so a single
        // connection made concurrent deliveries queue behind each other.
        let pool = Arc::new(crate::memory::postgres::PostgresConnectionPool::new(
            db_url.to_string(),
            connect_timeout_secs,
            pool_max_size,
        ));
        let init_pool = Arc::clone(&pool);
        // The blocking client cannot open a connection from inside a Tokio
        // runtime, and this constructor is reachable from one, so the first
        // checkout (which is what opens the connection) runs on its own thread.
        let init_handle = std::thread::Builder::new()
            .name("postgres-webhook-init".to_string())
            .spawn(move || -> Result<()> {
                init_pool.with_client_in(WEBHOOK_SESSION_CONTEXT, |client| {
                    client
                        .batch_execute(&schema_sql)
                        .context("failed to initialize PostgreSQL webhook repository schema")
                })
            })
            .context("failed to spawn PostgreSQL webhook initializer thread")?;
        init_handle
            .join()
            .map_err(|_| anyhow::anyhow!("PostgreSQL webhook initializer thread panicked"))??;

        Ok(Self {
            pool,
            workspace_id: Arc::from(workspace_id),
            qualified_memories: Arc::from(qualified_memories),
            qualified_memory_events: Arc::from(qualified_memory_events),
            qualified_topics: Arc::from(qualified_topics),
            qualified_topic_participants: Arc::from(qualified_topic_participants),
            qualified_ingestions: Arc::from(qualified_ingestions),
        })
    }

    fn existing_ingestion(
        tx: &mut postgres::Transaction<'_>,
        qualified_ingestions: &str,
        ingestion_key: &str,
        event_identity: &str,
    ) -> Result<Option<ExistingIngestion>> {
        let row = tx.query_opt(
            &format!(
                "SELECT ingestion_key, event_identity, request_hash, status, generation,
                        lease_expires_at, topic_id
                   FROM {qualified_ingestions}
                  WHERE ingestion_key = $1 OR event_identity = $2
                  ORDER BY CASE WHEN ingestion_key = $1 THEN 0 ELSE 1 END
                  LIMIT 1
                  FOR UPDATE"
            ),
            &[&ingestion_key, &event_identity],
        )?;
        Ok(row.map(|row| ExistingIngestion {
            ingestion_key: row.get(0),
            event_identity: row.get(1),
            request_hash: row.get(2),
            status: row.get(3),
            generation: row.get(4),
            lease_expires_at: row.get(5),
            topic_id: row.get(6),
        }))
    }

    fn claim_blocking(
        &self,
        ingestion_key: String,
        event_identity: String,
        request_hash: String,
        event: &WebhookEvent,
    ) -> Result<WebhookClaimOutcome> {
        let mut checkout = self.pool.checkout_in(WEBHOOK_SESSION_CONTEXT)?;
        let mut tx = checkout.client_mut()?.transaction()?;
        let now = Utc::now();
        let now_unix = now.timestamp();
        let lease_expires_at = now_unix.saturating_add(WEBHOOK_INGESTION_LEASE_SECS);

        let outcome = if let Some(existing) =
            Self::existing_ingestion(&mut tx, &self.qualified_ingestions, &ingestion_key, &event_identity)?
        {
            if existing.request_hash != request_hash || existing.event_identity != event_identity {
                WebhookClaimOutcome::Conflict
            } else if existing.status == "committed" {
                WebhookClaimOutcome::Committed {
                    topic_id: existing
                        .topic_id
                        .context("committed webhook ingestion is missing topic identity")?,
                }
            } else if existing.status == "pending" && existing.lease_expires_at.is_some_and(|lease| lease > now_unix) {
                WebhookClaimOutcome::Processing
            } else {
                let generation = existing
                    .generation
                    .checked_add(1)
                    .context("webhook ingestion generation exhausted")?;
                tx.execute(
                    &format!(
                        "UPDATE {}
                            SET status = 'pending', generation = $2, lease_expires_at = $3,
                                topic_id = NULL, memory_key = NULL, last_error = NULL, updated_at = $4
                          WHERE ingestion_key = $1",
                        self.qualified_ingestions
                    ),
                    &[&existing.ingestion_key, &generation, &lease_expires_at, &now],
                )?;
                WebhookClaimOutcome::Acquired(WebhookIngestionClaim {
                    ingestion_key: existing.ingestion_key,
                    event_identity,
                    request_hash,
                    generation,
                })
            }
        } else {
            let generation = 1_i64;
            tx.execute(
                &format!(
                    "INSERT INTO {} (
                        ingestion_key, event_identity, request_hash, source, project, external_id,
                        event_type, status, generation, lease_expires_at, created_at, updated_at
                     ) VALUES ($1, $2, $3, $4, $5, $6, $7, 'pending', $8, $9, $10, $10)",
                    self.qualified_ingestions
                ),
                &[
                    &ingestion_key,
                    &event_identity,
                    &request_hash,
                    &event.source,
                    &event.project,
                    &event.external_id,
                    &event.event_type,
                    &generation,
                    &lease_expires_at,
                    &now,
                ],
            )?;
            WebhookClaimOutcome::Acquired(WebhookIngestionClaim {
                ingestion_key,
                event_identity,
                request_hash,
                generation,
            })
        };

        tx.commit()?;
        Ok(outcome)
    }

    fn commit_blocking(
        &self,
        claim: &WebhookIngestionClaim,
        event: &WebhookEvent,
        memory_content: &str,
        memory_saved: bool,
    ) -> Result<String> {
        let mut checkout = self.pool.checkout_in(WEBHOOK_SESSION_CONTEXT)?;
        let mut tx = checkout.client_mut()?.transaction()?;
        let now = Utc::now();
        let owns_claim: bool = tx
            .query_one(
                &format!(
                    "SELECT EXISTS(
                        SELECT 1 FROM {}
                         WHERE ingestion_key = $1 AND generation = $2 AND status = 'pending'
                           AND event_identity = $3 AND request_hash = $4
                     )",
                    self.qualified_ingestions
                ),
                &[
                    &claim.ingestion_key,
                    &claim.generation,
                    &claim.event_identity,
                    &claim.request_hash,
                ],
            )?
            .get(0);
        if !owns_claim {
            anyhow::bail!("webhook ingestion ownership was lost before commit");
        }

        let project = crate::memory::topic::canonical_project_for_external(event.project.as_deref());
        let topic_id = if let Some(row) = tx.query_opt(
            &format!(
                "SELECT id FROM {} WHERE project = $1 AND external_id = $2",
                self.qualified_topics
            ),
            &[&project, &event.external_id],
        )? {
            row.get(0)
        } else {
            let candidate_id = Uuid::new_v4().to_string();
            let fingerprint = webhook_topic_fingerprint(event.project.as_deref(), &event.external_id, &event.title);
            tx.query_one(
                &format!(
                    "INSERT INTO {} (id, title, project, external_id, fingerprint, status, created_at, updated_at)
                     VALUES ($1, $2, $3, $4, $5, 'open', $6, $6)
                     ON CONFLICT (project, external_id) WHERE external_id IS NOT NULL
                     DO UPDATE SET updated_at = EXCLUDED.updated_at
                     RETURNING id",
                    self.qualified_topics
                ),
                &[
                    &candidate_id,
                    &event.title,
                    &project,
                    &event.external_id,
                    &fingerprint,
                    &now,
                ],
            )?
            .get(0)
        };

        if let Some(url) = event.external_url.as_deref() {
            tx.execute(
                &format!(
                    "UPDATE {} SET external_url = COALESCE(external_url, $1), updated_at = $2 WHERE id = $3",
                    self.qualified_topics
                ),
                &[&url, &now, &topic_id],
            )?;
        }

        let system_sender = format!("system:{}", event.source);
        tx.execute(
            &format!(
                "INSERT INTO {} (topic_id, user_id, role, joined_at)
                 VALUES ($1, $2, 'observer', $3)
                 ON CONFLICT(topic_id, user_id) DO NOTHING",
                self.qualified_topic_participants
            ),
            &[&topic_id, &system_sender, &now],
        )?;
        let topic_status = match event.event_type.as_str() {
            "issue.closed" => Some("resolved"),
            "issue.reopened" => Some("open"),
            _ => None,
        };
        if let Some(status) = topic_status {
            tx.execute(
                &format!(
                    "UPDATE {} SET status = $1, updated_at = $2,
                        resolved_at = CASE WHEN $1 = 'resolved' THEN $2 ELSE NULL END
                      WHERE id = $3",
                    self.qualified_topics
                ),
                &[&status, &now, &topic_id],
            )?;
        } else {
            tx.execute(
                &format!("UPDATE {} SET updated_at = $1 WHERE id = $2", self.qualified_topics),
                &[&now, &topic_id],
            )?;
        }

        let memory_key = format!("webhook:{}:{}:{}", event.source, event.external_id, claim.ingestion_key);
        if memory_saved {
            let memory_id = Uuid::new_v4().to_string();
            let event_timestamp = chrono::DateTime::parse_from_rfc3339(&event.timestamp)
                .map(|timestamp| timestamp.with_timezone(&Utc))
                .unwrap_or(now);
            let chat_id = format!("{}:{}", event.source, event.external_id);
            tx.execute(
                &format!(
                    "INSERT INTO {} (
                        id, key, content, category, created_at, updated_at, workspace_id,
                        owner_id, source, channel, chat_type, chat_id, sender_id, raw_sender,
                        topic_id, visibility, sensitivity, risk_signals, policy_version
                     ) VALUES (
                        $1, $2, $3, 'conversation', $4, $4, $5,
                        $6, 'webhook', 'webhook', 'dm', $7, $6, $6,
                        $8, 'owner', 'normal', '[]', 1
                     )
                     ON CONFLICT(key) DO UPDATE SET
                        content = EXCLUDED.content,
                        updated_at = EXCLUDED.updated_at,
                        topic_id = EXCLUDED.topic_id",
                    self.qualified_memories
                ),
                &[
                    &memory_id,
                    &memory_key,
                    &memory_content,
                    &event_timestamp,
                    &self.workspace_id.as_ref(),
                    &system_sender,
                    &chat_id,
                    &topic_id,
                ],
            )?;
        }

        let payload_json = serde_json::json!({
            "source": event.source,
            "project": event.project,
            "external_id": event.external_id,
            "event_type": event.event_type,
            "topic_id": topic_id,
            "memory_saved": memory_saved,
        })
        .to_string();
        let event_id = Uuid::new_v4().to_string();
        tx.execute(
            &format!(
                "INSERT INTO {} (
                    event_id, workspace_id, event_type, subject_table, subject_id,
                    visibility, payload_json, created_at
                 ) VALUES ($1, $2, 'webhook.event.committed', 'webhook_ingestions', $3,
                           'owner', $4, $5)",
                self.qualified_memory_events
            ),
            &[
                &event_id,
                &self.workspace_id.as_ref(),
                &claim.ingestion_key,
                &payload_json,
                &now,
            ],
        )?;

        let memory_key_value = memory_saved.then_some(memory_key);
        let updated = tx.execute(
            &format!(
                "UPDATE {} SET status = 'committed', lease_expires_at = NULL, topic_id = $3,
                    memory_key = $4, last_error = NULL, updated_at = $5
                  WHERE ingestion_key = $1 AND generation = $2 AND status = 'pending'
                    AND event_identity = $6 AND request_hash = $7",
                self.qualified_ingestions
            ),
            &[
                &claim.ingestion_key,
                &claim.generation,
                &topic_id,
                &memory_key_value,
                &now,
                &claim.event_identity,
                &claim.request_hash,
            ],
        )?;
        if updated != 1 {
            anyhow::bail!("webhook ingestion ownership was lost before commit");
        }
        tx.commit()?;
        Ok(topic_id)
    }

    fn fail_blocking(&self, claim: &WebhookIngestionClaim, error: &str) -> Result<()> {
        let error = error.chars().take(1024).collect::<String>();
        let now = Utc::now();
        let mut checkout = self.pool.checkout_in(WEBHOOK_SESSION_CONTEXT)?;
        checkout.client_mut()?.execute(
            &format!(
                "UPDATE {} SET status = 'failed', lease_expires_at = NULL,
                    last_error = $3, updated_at = $4
                  WHERE ingestion_key = $1 AND generation = $2 AND status = 'pending'",
                self.qualified_ingestions
            ),
            &[&claim.ingestion_key, &claim.generation, &error, &now],
        )?;
        Ok(())
    }
}

#[async_trait]
impl WebhookRepository for PostgresWebhookRepository {
    async fn claim(
        &self,
        ingestion_key: String,
        event_identity: String,
        request_hash: String,
        event: &WebhookEvent,
    ) -> Result<WebhookClaimOutcome> {
        let repository = self.clone();
        let event = event.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            repository.claim_blocking(ingestion_key, event_identity, request_hash, &event)
        })
        .await
        .context("PostgreSQL webhook claim worker panicked")?
    }

    async fn commit(
        &self,
        claim: &WebhookIngestionClaim,
        event: &WebhookEvent,
        memory_content: &str,
        memory_saved: bool,
    ) -> Result<String> {
        let repository = self.clone();
        let claim = claim.clone();
        let event = event.clone();
        let memory_content = memory_content.to_string();
        crate::runtime::blocking::spawn_blocking(move || {
            repository.commit_blocking(&claim, &event, &memory_content, memory_saved)
        })
        .await
        .context("PostgreSQL webhook commit worker panicked")?
    }

    async fn fail(&self, claim: &WebhookIngestionClaim, error: &str) -> Result<()> {
        let repository = self.clone();
        let claim = claim.clone();
        let error = error.to_string();
        crate::runtime::blocking::spawn_blocking(move || repository.fail_blocking(&claim, &error))
            .await
            .context("PostgreSQL webhook failure worker panicked")?
    }
}

/// Run the standalone webhook receiver from the authoritative application
/// configuration with the durable repository injected by the daemon assembly
/// boundary. This keeps backend selection out of the standalone HTTP service.
pub(crate) async fn run_configured_with_repository_generation(
    config: &Config,
    repository: WebhookRepositoryHandle,
    security: Arc<SecurityPolicy>,
    manager: crate::config::SharedConfig,
    generation_id: crate::config::ConfigGenerationId,
) -> Result<()> {
    let token = config
        .webhook
        .token
        .as_deref()
        .context("webhook.token must be configured")?;
    run_with_repository(
        &config.webhook.bind,
        token,
        config.webhook.signing_secret.as_deref(),
        repository.repository,
        security,
        Some((manager, generation_id)),
    )
    .await
}

pub(crate) fn repository_from_config(config: &Config) -> Result<WebhookRepositoryHandle> {
    let backend_name = effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config));
    match classify_memory_backend(&backend_name) {
        MemoryBackendKind::Sqlite | MemoryBackendKind::Lucid => {
            let db_path = config.workspace_dir.join("memory").join("brain.db");
            Ok(WebhookRepositoryHandle {
                repository: Arc::new(SqliteWebhookRepository::new(
                    db_path,
                    config.workspace_dir.to_string_lossy().to_string(),
                )?),
            })
        }
        MemoryBackendKind::Postgres => {
            let storage = &config.storage.provider.config;
            let db_url = storage
                .db_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .context("standalone webhook PostgreSQL repository requires storage.provider.config.db_url")?;
            Ok(WebhookRepositoryHandle {
                repository: Arc::new(PostgresWebhookRepository::new(
                    db_url,
                    &storage.schema,
                    &storage.table,
                    storage.connect_timeout_secs,
                    storage.pool_max_size,
                    config.workspace_dir.to_string_lossy().to_string(),
                )?),
            })
        }
        _ => anyhow::bail!(
            "standalone webhook durable ingestion does not support configured memory backend '{backend_name}'"
        ),
    }
}

async fn run_with_repository(
    bind: &str,
    token: &str,
    signing_secret: Option<&str>,
    repository: Arc<dyn WebhookRepository>,
    security: Arc<SecurityPolicy>,
    activation: Option<(crate::config::SharedConfig, crate::config::ConfigGenerationId)>,
) -> Result<()> {
    let trimmed_token = token.trim();
    if trimmed_token.is_empty() {
        anyhow::bail!("webhook token must not be empty when webhook is enabled");
    }

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind webhook server at {bind}"))?;
    let addr = listener.local_addr()?;

    tracing::info!("Webhook server listening on {}", addr);
    crate::health::mark_component_ok("webhook_receiver");
    if let Some((manager, generation_id)) = activation {
        while manager.active_generation_id() != generation_id {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    let state = WebhookState {
        token: Arc::<str>::from(trimmed_token.to_string()),
        signing_secret: signing_secret
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| Arc::<str>::from(s.to_string())),
        repository,
        rate_limiter: Arc::new(WebhookRateLimiter::new(
            WEBHOOK_DEFAULT_RATE_LIMIT_PER_MINUTE,
            Duration::from_secs(WEBHOOK_RATE_LIMIT_WINDOW_SECS),
        )),
        security,
    };

    run_with_listener(listener, state).await
}

async fn run_with_listener(listener: TcpListener, state: WebhookState) -> Result<()> {
    let app = router(state);
    axum::serve(listener, app)
        .await
        .context("webhook server stopped unexpectedly")?;
    Ok(())
}

fn router(state: WebhookState) -> Router {
    Router::new()
        .route("/webhook", post(handle_webhook_event))
        .route("/webhook/events", post(handle_webhook_event))
        .with_state(state)
}

/// The coded `429 rate_limited` of the webhook limit, in the gateway's
/// `{"error":{"code","message","retry_after_ms"}}` shape, with `Retry-After`
/// in whole seconds. The sliding window does not compute the exact wait, so
/// both carry the window length: an upper bound.
fn rate_limited_reply() -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({
            "error": {
                "code": "rate_limited",
                "message": "Too many webhook requests. Please retry later.",
                "retry_after_ms": WEBHOOK_RATE_LIMIT_WINDOW_SECS.saturating_mul(1000),
            }
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(RETRY_AFTER, axum::http::HeaderValue::from(WEBHOOK_RATE_LIMIT_WINDOW_SECS));
    response
}

async fn handle_webhook_event(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    if !state.rate_limiter.allow().await {
        return rate_limited_reply();
    }

    if !is_authorized(&headers, &state.token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Unauthorized" })),
        )
            .into_response();
    }

    // HMAC signature verification (when signing secret is configured)
    if let Some(ref signing_secret) = state.signing_secret {
        let signature = headers.get("X-Webhook-Signature").and_then(|v| v.to_str().ok());
        match signature {
            Some(sig) if verify_webhook_hmac_signature(signing_secret, &body, sig) => {}
            _ => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({ "error": "Invalid HMAC signature" })),
                )
                    .into_response();
            }
        }
    }

    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("Invalid JSON: {e}") })),
            )
                .into_response();
        }
    };

    let event = match parse_webhook_event(payload) {
        Ok(event) => event,
        Err(error) => {
            tracing::warn!("invalid webhook event payload: {error}");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Invalid webhook event" })),
            )
                .into_response();
        }
    };

    // ── Persist side-effect gate (FIX-P1-03) ────────────────────────────────
    // Writing a verified event into the topic store is a state mutation, so it
    // must respect autonomy: under ReadOnly the standalone webhook server must
    // not persist. Low/read-style judgement — denies only when ReadOnly (or the
    // action budget is exhausted). Authentication and payload validation have
    // already passed; a deny returns 403 without claiming durable ingestion.
    let persist_op = format!("webhook:{}:persist", event.source);
    if let Err(reason) = SideEffectGate::new(state.security.as_ref()).authorize_resource_operation(
        "webhook",
        &persist_op,
        ResourceRiskLevel::Low,
        None,
    ) {
        tracing::warn!(source = %event.source, "webhook persist blocked by SideEffectGate: {reason}");
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "Persist denied by security policy" })),
        )
            .into_response();
    }

    let raw_idempotency_key = match headers.get("X-Idempotency-Key") {
        None => None,
        Some(value) => {
            let Ok(value) = value.to_str() else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": "X-Idempotency-Key must be valid UTF-8" })),
                )
                    .into_response();
            };
            let value = value.trim();
            if value.is_empty() {
                None
            } else if value.len() > WEBHOOK_MAX_IDEMPOTENCY_KEY_BYTES {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": "X-Idempotency-Key exceeds the 256-byte limit" })),
                )
                    .into_response();
            } else {
                Some(value)
            }
        }
    };
    let request_hash = webhook_request_hash(&body);
    let event_identity = webhook_event_identity(&event, &request_hash);
    let ingestion_key = raw_idempotency_key.map_or_else(
        || format!("event:{event_identity}"),
        |key| format!("header:{}", webhook_scoped_key_digest(&state.token, key)),
    );
    let claim = match state
        .repository
        .claim(ingestion_key, event_identity, request_hash, &event)
        .await
    {
        Ok(WebhookClaimOutcome::Acquired(claim)) => claim,
        Ok(WebhookClaimOutcome::Committed { topic_id }) => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "duplicate",
                    "idempotent": true,
                    "topic_id": topic_id,
                    "message": "Request already processed"
                })),
            )
                .into_response();
        }
        Ok(WebhookClaimOutcome::Processing) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "status": "processing",
                    "idempotent": true,
                    "message": "Request is still processing"
                })),
            )
                .into_response();
        }
        Ok(WebhookClaimOutcome::Conflict) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "status": "request_conflict",
                    "idempotent": true,
                    "message": "Idempotency identity was used for a different event"
                })),
            )
                .into_response();
        }
        Err(error) => {
            tracing::error!("failed to claim webhook ingestion: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to claim event" })),
            )
                .into_response();
        }
    };

    let memory_content = format_event_memory(&event);
    let memory_saved = should_store_webhook_memory(&memory_content).await;
    match state
        .repository
        .commit(&claim, &event, &memory_content, memory_saved)
        .await
    {
        Ok(topic_id) => (StatusCode::OK, Json(serde_json::json!(WebhookAck { topic_id }))).into_response(),
        Err(error) => {
            if let Err(fail_error) = state.repository.fail(&claim, &error.to_string()).await {
                tracing::error!("failed to mark webhook ingestion failed: {fail_error}");
            }
            tracing::error!("failed to persist webhook event: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to persist event" })),
            )
                .into_response()
        }
    }
}

/// Verify HMAC-SHA256 signature of the request body.
/// Accepts signatures in the format `sha256=<hex>` or raw hex.
fn verify_webhook_hmac_signature(secret: &str, body: &[u8], signature_header: &str) -> bool {
    use hmac::{Hmac, Mac};

    let signature_hex = signature_header
        .trim()
        .strip_prefix("sha256=")
        .unwrap_or_else(|| signature_header.trim());
    let Ok(provided) = hex::decode(signature_hex) else {
        return false;
    };

    let Ok(mut mac) = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&provided).is_ok()
}

fn is_authorized(headers: &HeaderMap, expected_token: &str) -> bool {
    // Try X-Webhook-Token first (preferred, matches gateway behavior)
    if let Some(raw) = headers.get("X-Webhook-Token") {
        if let Ok(token) = raw.to_str() {
            return expected_token.as_bytes().ct_eq(token.trim().as_bytes()).into();
        }
    }

    // Fallback to Authorization: Bearer <token>
    let Some(raw) = headers.get(AUTHORIZATION) else {
        return false;
    };

    let Ok(value) = raw.to_str() else {
        return false;
    };

    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };

    expected_token.as_bytes().ct_eq(token.trim().as_bytes()).into()
}

fn parse_webhook_event(payload: Value) -> Result<WebhookEvent> {
    if is_openpr_payload(&payload) {
        return map_openpr_event(&payload);
    }

    let mut event: WebhookEvent =
        serde_json::from_value(payload).context("payload does not match generic webhook event format")?;

    normalize_event(&mut event);
    validate_event(&event)?;
    Ok(event)
}

fn normalize_event(event: &mut WebhookEvent) {
    event.source = event.source.trim().to_lowercase();
    event.event_type = event.event_type.trim().to_lowercase();
    event.external_id = event.external_id.trim().to_lowercase();
    event.title = event.title.trim().to_string();
    event.content = event.content.trim().to_string();
    if event.timestamp.trim().is_empty() {
        event.timestamp = Utc::now().to_rfc3339();
    } else {
        event.timestamp = event.timestamp.trim().to_string();
    }
    event.project = event
        .project
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    event.external_url = event
        .external_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    event.actor = event
        .actor
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
}

fn validate_event(event: &WebhookEvent) -> Result<()> {
    if event.source.is_empty()
        || event.event_type.is_empty()
        || event.external_id.is_empty()
        || event.title.is_empty()
        || event.content.is_empty()
    {
        anyhow::bail!("event contains required empty fields");
    }
    Ok(())
}

fn is_openpr_payload(payload: &Value) -> bool {
    payload.get("workspace_id").is_some()
        || payload.get("issue_id").is_some()
        || payload.get("issue_identifier").is_some()
        || payload.get("comment_id").is_some()
}

fn map_openpr_event(payload: &Value) -> Result<WebhookEvent> {
    let source = "openpr".to_string();

    let event_type = first_string(payload, &["event_type", "event", "type", "action"])
        .map(|value| normalize_openpr_event_type(&value))
        .unwrap_or_else(|| "issue.updated".to_string());

    let workspace_id = first_string(payload, &["workspace_id", "project", "project_id"]);

    let issue_identifier = first_string(payload, &["issue_identifier", "issue_id"]);
    let comment_id = first_string(payload, &["comment_id"]);

    let external_id = if let Some(identifier) = issue_identifier {
        format!("issue#{}", identifier.trim())
    } else if let Some(identifier) = comment_id {
        format!("comment#{}", identifier.trim())
    } else {
        anyhow::bail!("openpr payload missing issue/comment identifier");
    };

    let external_url = first_string(
        payload,
        &["external_url", "issue_url", "comment_url", "url", "html_url"],
    );

    let title = first_string(payload, &["title", "issue_title", "comment_title", "subject", "name"])
        .unwrap_or_else(|| format!("OpenPR {}", external_id));

    let content = first_string(
        payload,
        &["content", "body", "description", "text", "comment", "message"],
    )
    .unwrap_or_else(|| title.clone());

    let actor = first_string(payload, &["actor", "operator", "author", "user"]);

    let timestamp = first_string(payload, &["timestamp", "occurred_at", "created_at", "updated_at"])
        .unwrap_or_else(|| Utc::now().to_rfc3339());

    let mut event = WebhookEvent {
        source,
        event_type,
        project: workspace_id,
        external_id,
        external_url,
        title,
        content,
        actor,
        timestamp,
    };

    normalize_event(&mut event);
    validate_event(&event)?;
    Ok(event)
}

fn first_string(payload: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        let Some(value) = payload.get(*key) else {
            continue;
        };
        if let Some(as_str) = value.as_str() {
            let trimmed = as_str.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
            continue;
        }
        if value.is_number() || value.is_boolean() {
            return Some(value.to_string());
        }
    }
    None
}

fn normalize_openpr_event_type(raw: &str) -> String {
    let normalized = raw.trim().to_lowercase().replace('_', ".");
    if normalized.contains('.') {
        return normalized;
    }

    if normalized == "closed" {
        return "issue.closed".to_string();
    }
    if normalized == "reopened" {
        return "issue.reopened".to_string();
    }
    if normalized == "created" {
        return "issue.created".to_string();
    }

    format!("issue.{normalized}")
}

fn format_event_memory(event: &WebhookEvent) -> String {
    let mut lines = vec![
        format!("source: {}", event.source),
        format!("event_type: {}", event.event_type),
        format!("external_id: {}", event.external_id),
        format!("title: {}", event.title),
        format!("content: {}", event.content),
        format!("event_date: {}", webhook_event_date(&event.timestamp)),
    ];

    if let Some(project) = &event.project {
        lines.push(format!("project: {project}"));
    }
    if let Some(url) = &event.external_url {
        lines.push(format!("external_url: {url}"));
    }
    if let Some(actor) = &event.actor {
        lines.push(format!("actor: {actor}"));
    }

    lines.join("\n")
}

fn webhook_event_date(timestamp: &str) -> &str {
    timestamp.split_once('T').map_or(timestamp, |(date, _)| date)
}

/// Unstamped webhook writers may reuse NULL/Main topics. A Named topic is left
/// untouched and a sidecar without the colliding `(project, external_id)` key
/// is created instead (review H2).
fn webhook_resolve_topic(tx: &Transaction<'_>, event: &WebhookEvent) -> Result<String> {
    match crate::memory::topic::find_topic_by_project_and_external(tx, event.project.as_deref(), &event.external_id)? {
        Some(topic) if crate::memory::topic::unstamped_writer_may_use_topic(tx, &topic.id)? => Ok(topic.id),
        Some(_) => {
            let fingerprint = format!(
                "webhook-sidecar:{}",
                webhook_topic_fingerprint(event.project.as_deref(), &event.external_id, &event.title)
            );
            crate::memory::topic::create_topic(tx, &event.title, event.project.as_deref(), None, &fingerprint, None)
        }
        None => {
            let fingerprint = webhook_topic_fingerprint(event.project.as_deref(), &event.external_id, &event.title);
            crate::memory::topic::create_topic(
                tx,
                &event.title,
                event.project.as_deref(),
                Some(&event.external_id),
                &fingerprint,
                None,
            )
        }
    }
}

fn webhook_topic_fingerprint(project: Option<&str>, external_id: &str, title: &str) -> String {
    let payload = format!(
        "{}:{}:{}",
        project.unwrap_or_default().trim().to_lowercase(),
        external_id.trim().to_lowercase(),
        title.trim().to_lowercase(),
    );
    let digest = Sha256::digest(payload.as_bytes());
    format!("{digest:x}")
}

fn webhook_request_hash(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    format!("{digest:x}")
}

fn webhook_scoped_key_digest(token: &str, raw_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(Sha256::digest(token.as_bytes()));
    hasher.update([0]);
    hasher.update(raw_key.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn webhook_event_identity(event: &WebhookEvent, request_hash: &str) -> String {
    let payload = format!(
        "{}:{}:{}:{}:{}",
        event.source.trim().to_lowercase(),
        event.project.as_deref().unwrap_or("_global").trim().to_lowercase(),
        event.external_id.trim().to_lowercase(),
        event.event_type.trim().to_lowercase(),
        request_hash,
    );
    let digest = Sha256::digest(payload.as_bytes());
    format!("{digest:x}")
}

async fn should_store_webhook_memory(content: &str) -> bool {
    if !crate::memory::should_autosave_content(content) {
        return false;
    }
    let safety = MemorySafetyFilter::default()
        .check(
            content,
            &SourceMetadata {
                actor: Actor::System,
                historical_accuracy: Some(1.0),
            },
        )
        .await;
    if safety.passed {
        true
    } else {
        tracing::warn!(issues = ?safety.issues, "webhook memory autosave skipped by safety filter");
        false
    }
}

fn ensure_memory_schema(db_path: &Path) -> Result<()> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = crate::memory::SqliteMemory::new_with_path(db_path.to_path_buf())?;
    Ok(())
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests;

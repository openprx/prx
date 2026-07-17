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
use parking_lot::Mutex;
use postgres::{Client, NoTls, Row};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
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
    client: Arc<PostgresClientSlot>,
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

struct PostgresClientSlot {
    client: Mutex<Option<Client>>,
}

impl PostgresClientSlot {
    const fn new(client: Client) -> Self {
        Self {
            client: Mutex::new(Some(client)),
        }
    }

    fn with_client<T>(&self, f: impl FnOnce(&mut Client) -> Result<T>) -> Result<T> {
        let mut guard = self.client.lock();
        let client = guard.as_mut().context("PostgreSQL memory backend client is closed")?;
        // The synchronous client is reused across requests, while PostgreSQL
        // RLS settings are session-scoped. Always start a checkout in the
        // neutral system context; owner-scoped operations override this inside
        // the same lock before issuing their query. This prevents one
        // principal's owner setting from leaking into the next operation.
        client.execute("SELECT set_config('prx.rls_bypass', 'on', false)", &[])?;
        f(client)
    }
}

impl Drop for PostgresClientSlot {
    fn drop(&mut self) {
        let Some(client) = self.client.get_mut().take() else {
            return;
        };

        let handle = std::thread::Builder::new()
            .name("postgres-memory-drop".to_string())
            .spawn(move || drop(client));

        if let Ok(handle) = handle {
            let _ = handle.join();
        }
    }
}

impl PostgresMemory {
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
        let (client, pgvector_available) = Self::initialize_client(
            db_url.to_string(),
            connect_timeout_secs,
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
            client: Arc::new(PostgresClientSlot::new(client)),
            schema_ident: schema_ident_field,
            qualified_table,
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

    fn initialize_client(
        db_url: String,
        connect_timeout_secs: Option<u64>,
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
    ) -> Result<(Client, bool)> {
        let init_handle = std::thread::Builder::new()
            .name("postgres-memory-init".to_string())
            .spawn(move || -> Result<(Client, bool)> {
                let mut config: postgres::Config = db_url.parse().context("invalid PostgreSQL connection URL")?;

                if let Some(timeout_secs) = connect_timeout_secs {
                    let bounded = timeout_secs.min(POSTGRES_CONNECT_TIMEOUT_CAP_SECS);
                    config.connect_timeout(Duration::from_secs(bounded));
                }

                let mut client = config
                    .connect(NoTls)
                    .context("failed to connect to PostgreSQL memory backend")?;

                let pgvector_available = Self::init_schema(
                    &mut client,
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
                )?;
                Ok((client, pgvector_available))
            })
            .context("failed to spawn PostgreSQL initializer thread")?;

        init_handle
            .join()
            .map_err(|_| anyhow::anyhow!("PostgreSQL initializer thread panicked"))?
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
                content_hash TEXT NOT NULL,
                embedding BYTEA NOT NULL,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                dimensions BIGINT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL,
                accessed_at TIMESTAMPTZ NOT NULL,
                PRIMARY KEY (content_hash, provider, model, dimensions)
            );
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

        // FIX-P3-04: enable row-level security on `memories` so owner isolation is
        // enforced at the database layer (defense-in-depth: even if the app-level
        // ACL/post_filter is bypassed, the DB still scopes rows to the active
        // owner). Best-effort — if RLS cannot be configured (insufficient role
        // privileges) it degrades like pgvector init: logged and skipped, never
        // breaking the backend.
        Self::try_init_rls(client, qualified_table);

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

    /// FIX-P3-04: enable row-level security on the `memories` table so owner
    /// isolation is enforced inside PostgreSQL itself (defense-in-depth).
    ///
    /// Policy `memories_owner_isolation` admits a row when ANY holds:
    /// - the system-bypass flag `prx.rls_bypass` is `'on'` (set per-query for the
    ///   four canonical system principals so internal/router/system operations are
    ///   never locked out);
    /// - the owner session variable `prx.current_owner` is unset (NULL) — this
    ///   keeps the many code paths that do not carry a principal working exactly as
    ///   before, so RLS only tightens (never breaks) existing behaviour;
    /// - the row's `owner_id` matches `prx.current_owner` (NULL-safe via
    ///   `IS NOT DISTINCT FROM`, so a NULL-owner row matches a NULL setting).
    ///
    /// `FORCE ROW LEVEL SECURITY` is applied so the policy is honoured even when
    /// the connection role owns the table (table owners otherwise bypass RLS).
    ///
    /// Best-effort: if the role lacks privileges to alter the table / create the
    /// policy, the failure is logged and skipped — identical to pgvector init —
    /// so deployments without superuser/table-owner rights still start cleanly.
    ///
    /// SAFETY: `qualified_table` is validated + quoted at construction via
    /// `validate_identifier()` + `quote_identifier()`; no untrusted input is
    /// interpolated into the DDL. Session-variable values are passed as bind
    /// params in `apply_rls_context`.
    fn try_init_rls(client: &mut Client, qualified_table: &str) {
        // `CREATE POLICY` is not idempotent, so drop-then-create makes re-init
        // safe. `ENABLE`/`FORCE` are idempotent. Run as a single batch so the
        // policy is never left dropped if a later statement fails.
        let init = format!(
            "
            ALTER TABLE {qualified_table} ENABLE ROW LEVEL SECURITY;
            ALTER TABLE {qualified_table} FORCE ROW LEVEL SECURITY;
            DROP POLICY IF EXISTS memories_owner_isolation ON {qualified_table};
            CREATE POLICY memories_owner_isolation ON {qualified_table}
                USING (
                    current_setting('prx.rls_bypass', true) = 'on'
                    OR current_setting('prx.current_owner', true) IS NULL
                    OR owner_id IS NOT DISTINCT FROM current_setting('prx.current_owner', true)
                )
                WITH CHECK (
                    current_setting('prx.rls_bypass', true) = 'on'
                    OR current_setting('prx.current_owner', true) IS NULL
                    OR owner_id IS NOT DISTINCT FROM current_setting('prx.current_owner', true)
                );
            "
        );
        if let Err(error) = client.batch_execute(&init) {
            tracing::debug!(error = %error, "Postgres row-level security initialization skipped");
        }
    }

    /// FIX-P3-04: push the RLS session context for `principal` onto `client`
    /// before an owner-scoped query.
    ///
    /// - system principals get the bypass flag so internal/router/system actors
    ///   are never restricted by the policy;
    /// - everyone else binds their `owner_id` (or NULL when absent) into
    ///   `prx.current_owner`, restricting visible rows to that owner.
    ///
    /// Because the backend reuses a single long-lived connection, the variables
    /// must be (re)set on every owner-scoped query so a previous principal's
    /// context never leaks into the next.
    ///
    /// `set_config(name, value, is_local => false)` is used instead of `SET`
    /// because it accepts the value as a bind parameter, preserving the
    /// parameterized-query rule (no string interpolation of values).
    ///
    /// Owner-scoped read paths capture only the system-bypass flag and the owner
    /// id (not the whole principal) into their blocking task, so this takes those
    /// two values directly.
    fn apply_rls_context_raw(client: &mut Client, system: bool, owner_id: Option<&str>) -> Result<()> {
        if system {
            client.execute("SELECT set_config('prx.rls_bypass', 'on', false)", &[])?;
            let cleared: Option<&str> = None;
            client.execute("SELECT set_config('prx.current_owner', $1, false)", &[&cleared])?;
        } else {
            client.execute("SELECT set_config('prx.rls_bypass', 'off', false)", &[])?;
            let owner: Option<&str> = owner_id.filter(|owner| !owner.trim().is_empty());
            client.execute("SELECT set_config('prx.current_owner', $1, false)", &[&owner])?;
        }
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

    async fn get_or_compute_embedding(&self, text: &str) -> Result<Option<Vec<f32>>> {
        if self.embedder.dimensions() == 0 {
            return Ok(None);
        }
        let content_hash = Self::content_hash(text);
        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();
        let cache_table = self.qualified_embedding_cache_table.clone();
        let client = self.client.clone();
        let cache_hash = content_hash.clone();
        let cache_provider = provider_name.clone();
        let cache_model = model_name.clone();
        let cached = tokio::task::spawn_blocking(move || -> Result<Option<Vec<f32>>> {
            let select_stmt = format!(
                "
                SELECT embedding
                FROM {cache_table}
                WHERE content_hash = $1
                  AND provider = $2
                  AND model = $3
                  AND dimensions = $4
                LIMIT 1
                "
            );
            let update_stmt = format!(
                "
                UPDATE {cache_table}
                SET accessed_at = $5
                WHERE content_hash = $1
                  AND provider = $2
                  AND model = $3
                  AND dimensions = $4
                "
            );
            client.with_client(|client| {
                let row = client.query_opt(&select_stmt, &[&cache_hash, &cache_provider, &cache_model, &dimensions])?;
                let Some(row) = row else {
                    return Ok(None);
                };
                let bytes: Vec<u8> = row.get(0);
                let embedding = vector::bytes_to_vec(&bytes);
                if embedding.len() != usize::try_from(dimensions).unwrap_or(usize::MAX) {
                    return Ok(None);
                }
                let now = Utc::now();
                client.execute(
                    &update_stmt,
                    &[&cache_hash, &cache_provider, &cache_model, &dimensions, &now],
                )?;
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
                    content_hash, embedding, provider, model, dimensions, created_at, accessed_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $6)
                ON CONFLICT (content_hash, provider, model, dimensions)
                DO UPDATE SET embedding = EXCLUDED.embedding, accessed_at = EXCLUDED.accessed_at
                "
            );
            let evict_stmt = format!(
                "
                DELETE FROM {cache_table}
                WHERE (content_hash, provider, model, dimensions) IN (
                    SELECT content_hash, provider, model, dimensions
                    FROM {cache_table}
                    ORDER BY accessed_at ASC
                    LIMIT GREATEST(
                        (SELECT COUNT(*) FROM {cache_table}) - $1,
                        0
                    )
                )
                "
            );
            client.with_client(|client| {
                let mut tx = client.transaction()?;
                let now = Utc::now();
                tx.execute(
                    &insert_stmt,
                    &[&content_hash, &bytes, &provider_name, &model_name, &dimensions, &now],
                )?;
                tx.execute(&evict_stmt, &[&cache_max])?;
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
        let Some(embedding) = self.get_or_compute_embedding(content).await? else {
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
        }
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
        client: &PostgresClientSlot,
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
        client: Arc<PostgresClientSlot>,
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
        let fallback = |user_id: String| Principal {
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
        let created_at: DateTime<Utc> = row.get(13);
        let visibility = row
            .get::<_, String>(11)
            .parse::<MemoryVisibility>()
            .unwrap_or(MemoryVisibility::Workspace);

        Ok(MemoryEvent {
            id: row.get(0),
            event_id: row.get(1),
            workspace_id: row.get(2),
            event_type: row.get(3),
            subject_table: row.get(4),
            subject_id: row.get(5),
            session_key: row.get(6),
            run_id: row.get(7),
            parent_run_id: row.get(8),
            agent_id: row.get(9),
            persona_id: row.get(10),
            visibility,
            payload_json: row.get(12),
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
        let (embedding, embedding_provider, embedding_model, embedding_dimensions, embedding_vector) =
            self.embedding_metadata_for_category(&category, content).await?;
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
                        format!("UPDATE {qualified_table} SET embedding_vector = $1::vector WHERE key = $2");
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
        let (embedding, embedding_provider, embedding_model, embedding_dimensions, embedding_vector) =
            self.embedding_metadata_for_category(&category, content).await?;
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let pgvector_available = self.pgvector_available;
        let key = key.to_string();
        let content = content.to_string();
        let category = Self::category_to_str(&category);
        let sid = session_id.map(str::to_string);

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
            // FIX-P0-23 (#4 F4): persist topic_id instead of hard-coded NULL.
            // Prefer the explicit metadata topic_id; fall back to source_event_id
            // (source_event_id 兜底) so a write still threads back to its
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
                    policy_version = EXCLUDED.policy_version
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
                )?;
                if pgvector_available {
                    let update_vector_stmt =
                        format!("UPDATE {qualified_table} SET embedding_vector = $1::vector WHERE key = $2");
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

    async fn recall(&self, query: &str, limit: usize, session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let query = query.trim().to_string();
        let sid = session_id.map(str::to_string);
        let query_embedding = if query.is_empty() {
            None
        } else {
            self.get_or_compute_embedding(&query).await?
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
                                   (1.0 - (embedding_vector <=> $1::vector))::DOUBLE PRECISION AS score,
                                   useful_count
                            FROM {qualified_table}
                            WHERE embedding_vector IS NOT NULL
                              AND embedding_provider = $2
                              AND embedding_model = $3
                              AND embedding_dimensions = $4
                              AND ($5::TEXT IS NULL OR session_id = $5)
                            ORDER BY embedding_vector <=> $1::vector
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
        let rls_owner = sender_id
            .as_deref()
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
                Self::apply_rls_context_raw(client, rls_system, rls_owner.as_deref())?;
                let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&query, &sid, &limit_i64];
                for value in &scope_values {
                    params.push(value);
                }
                Ok(client.query(&stmt, &params)?)
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
        let rls_owner = sender_id
            .as_deref()
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
                Self::apply_rls_context_raw(client, rls_system, rls_owner.as_deref())?;
                let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&key];
                for value in &scope_values {
                    params.push(value);
                }
                Ok(client.query_opt(&stmt, &params)?)
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

        // FIX-P0-22 (#3 F3): resolve the ACL principal and record an access-audit
        // entry for both allowed and denied deletes (parity with SQLite).
        let principal = self.resolve_principal_from_context(&context).await;

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

        tokio::task::spawn_blocking(move || -> Result<bool> {
            let stmt = format!(
                "
                DELETE FROM {qualified_table}
                WHERE key = $1
                  AND ({scope_sql})
                "
            );
            let deleted = client.with_client(|client| {
                let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&key];
                for value in &scope_values {
                    params.push(value);
                }
                Ok(client.execute(&stmt, &params)?)
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

    async fn list_conversation_turns(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationTurn>> {
        let client = self.client.clone();
        let turns_table = self.qualified_conversation_turns_table();
        let owner_id = principal
            .owner_id
            .as_deref()
            .filter(|owner| !owner.trim().is_empty())
            .map(str::to_string);
        let session_key = session_key.to_string();
        #[allow(clippy::cast_possible_wrap)]
        let limit = limit.clamp(1, 500) as i64;
        #[allow(clippy::cast_possible_wrap)]
        let offset = offset.min(100_000) as i64;
        let system_allowed = Self::is_system_principal(principal);
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
                 ORDER BY id DESC
                 LIMIT $2 OFFSET $3",
                session_fragment = session_fragment.sql,
            );
            let rows = client.with_client(|client| {
                Self::apply_rls_context_raw(client, system_allowed, owner_id.as_deref())?;
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> =
                    vec![&session_key, &limit, &offset, &system_allowed, &owner_id];
                for key in &legacy_session_keys {
                    bind.push(key);
                }
                Ok(client.query(&stmt, &bind)?)
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

    async fn append_message_event(&self, input: MessageEventInput) -> Result<MessageEvent> {
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
                    event_id, workspace_id, event_type, subject_table, subject_id, session_key,
                    agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, 'message_events', $4, $5, $6, $7, $8, NULL, $9)
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
                              visibility = 'workspace'
                              OR (visibility = 'agent' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                              ))
                              OR (visibility = 'session' AND {session_fragment})
                              OR (visibility = 'private' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                                  OR ($6::TEXT IS NOT NULL AND sender = $6)
                              ))
                              OR (visibility = 'system' AND $7::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id ASC
                LIMIT $8
                ",
                session_fragment = session_fragment.sql,
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
                              visibility = 'workspace'
                              OR (visibility = 'agent' AND (
                                  ($2::TEXT IS NOT NULL AND agent_id = $2)
                                  OR ($3::TEXT IS NOT NULL AND persona_id = $3)
                              ))
                              OR (visibility = 'session' AND {session_fragment})
                              OR (visibility = 'private' AND (
                                  ($2::TEXT IS NOT NULL AND agent_id = $2)
                                  OR ($3::TEXT IS NOT NULL AND persona_id = $3)
                                  OR ($5::TEXT IS NOT NULL AND sender = $5)
                              ))
                              OR (visibility = 'system' AND $6::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id DESC
                LIMIT $7
                ",
                session_fragment = session_fragment.sql,
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
                Ok(client.query(&stmt, &bind)?)
            })?;
            rows.iter()
                .map(Self::row_to_message_event)
                .collect::<Result<Vec<MessageEvent>>>()
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
                              visibility = 'workspace'
                              OR (visibility = 'agent' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                              ))
                              OR (visibility = 'session' AND {session_fragment})
                              OR (visibility = 'private' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                                  OR ($6::TEXT IS NOT NULL AND sender = $6)
                              ))
                              OR (visibility = 'system' AND $7::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id DESC
                LIMIT $8
                ",
                session_fragment = session_fragment.sql,
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
                      visibility IN ('global', 'workspace')
                      OR (visibility = 'agent' AND (
                          ($4::TEXT IS NOT NULL AND agent_id = $4)
                          OR ($5::TEXT IS NOT NULL AND persona_id = $5)
                      ))
                      OR visibility = 'session'
                      OR (visibility = 'private' AND (
                          ($4::TEXT IS NOT NULL AND agent_id = $4)
                          OR ($5::TEXT IS NOT NULL AND persona_id = $5)
                          OR ($6::TEXT IS NOT NULL AND sender = $6)
                      ))
                      OR (visibility = 'system' AND $7::BOOLEAN)
                  )
                ORDER BY id DESC
                LIMIT $8
                ",
                session_fragment = session_fragment.sql,
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
                    event_id, workspace_id, event_type, subject_table, subject_id,
                    session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                ON CONFLICT (event_id) DO NOTHING
                "
            );
            let select_stmt = format!(
                "
                SELECT id, event_id, workspace_id, event_type, subject_table, subject_id,
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

        tokio::task::spawn_blocking(move || -> Result<Vec<MemoryEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, workspace_id, event_type, subject_table, subject_id,
                       session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                FROM {qualified_memory_events_table}
                WHERE id > $1
                  AND (
                      visibility = 'global'
                      OR (
                          workspace_id = $2
                          AND (
                              visibility = 'workspace'
                              OR (visibility = 'agent' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                              ))
                              OR (visibility = 'session' AND $5::TEXT IS NOT NULL AND session_key = $5)
                              OR (visibility = 'private' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                              ))
                              OR (visibility = 'system' AND $6::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id ASC
                LIMIT $7
                "
            );
            let rows = client.with_client(|client| {
                Ok(client.query(
                    &stmt,
                    &[
                        &after_id,
                        &principal.workspace_id,
                        &principal.agent_id,
                        &principal.persona_id,
                        &principal.session_key,
                        &system_allowed,
                        &limit_i64,
                    ],
                )?)
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

        tokio::task::spawn_blocking(move || -> Result<Vec<MemoryEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, workspace_id, event_type, subject_table, subject_id,
                       session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                FROM {qualified_memory_events_table}
                WHERE (
                      visibility = 'global'
                      OR (
                          workspace_id = $1
                          AND (
                              visibility = 'workspace'
                              OR (visibility = 'agent' AND (
                                  ($2::TEXT IS NOT NULL AND agent_id = $2)
                                  OR ($3::TEXT IS NOT NULL AND persona_id = $3)
                              ))
                              OR (visibility = 'session' AND $4::TEXT IS NOT NULL AND session_key = $4)
                              OR (visibility = 'private' AND (
                                  ($2::TEXT IS NOT NULL AND agent_id = $2)
                                  OR ($3::TEXT IS NOT NULL AND persona_id = $3)
                              ))
                              OR (visibility = 'system' AND $5::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id DESC
                LIMIT $6
                "
            );
            let rows = client.with_client(|client| {
                Ok(client.query(
                    &stmt,
                    &[
                        &principal.workspace_id,
                        &principal.agent_id,
                        &principal.persona_id,
                        &principal.session_key,
                        &system_allowed,
                        &limit_i64,
                    ],
                )?)
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
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, event_type, subject_table, subject_id, session_key,
                    agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, 'memory.draft.created', 'memory_drafts', $3, $4, $5, $6, $7, $8, $9)
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
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, event_type, subject_table, subject_id, session_key,
                    agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
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
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, event_type, subject_table, subject_id, session_key,
                    agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, 'memory.draft.rejected', 'memory_drafts', $3, $4, $5, $6, $7, $8, $9)
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
            let embedding = self.get_or_compute_embedding(&content).await?;
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
            let delete_chunks_stmt = format!("DELETE FROM {qualified_document_chunks_table} WHERE document_id = $1");
            let insert_chunk_stmt = format!(
                "
                INSERT INTO {qualified_document_chunks_table} (
                    chunk_id, document_id, workspace_id, owner_id, topic_id, task_id,
                    chunk_index, heading, content, content_sha256, embedding,
                    embedding_provider, embedding_model, embedding_dimensions,
                    source_anchor, token_estimate, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
                "
            );
            let insert_event_stmt = format!(
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, event_type, subject_table, subject_id,
                    session_key, agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, 'document.ingested', 'documents', $3, NULL, NULL, NULL, $4, $5, $6)
                "
            );
            let select_document_stmt = format!(
                "
                SELECT id, document_id, workspace_id, owner_id, topic_id, task_id,
                       source_message_event_id, source_kind, source_uri, title,
                       content_sha256, mime_type, visibility, metadata_json,
                       chunk_count, created_at, updated_at
                FROM {qualified_documents_table}
                WHERE document_id = $1
                LIMIT 1
                "
            );
            client.with_client(|client| {
                let mut tx = client.transaction()?;
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
                tx.execute(&delete_chunks_stmt, &[&document_id])?;
                let update_chunk_vector_stmt = if pgvector_available {
                    Some(format!(
                        "UPDATE {qualified_document_chunks_table} SET embedding_vector = $1::vector WHERE chunk_id = $2"
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
                            &source_anchor,
                            &token_estimate,
                            &now,
                        ],
                    )?;
                    if let (Some(stmt), Some(embedding_vector)) =
                        (update_chunk_vector_stmt.as_ref(), embedding_vector.as_ref())
                    {
                        tx.execute(stmt, &[embedding_vector, &chunk_id])?;
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
                        &document_id,
                        &visibility,
                        &payload_json,
                        &Utc::now(),
                    ],
                )?;
                let row = tx.query_one(&select_document_stmt, &[&document_id])?;
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
        let owner_id = Self::document_owner_for_principal(&principal);
        let limit_i64 = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let query_embedding = self.get_or_compute_embedding(&query).await?;
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
                      d.visibility IN ('global', 'workspace')
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
                Ok(client.query(&stmt, &[&query, &principal.workspace_id, &owner_id, &limit_i64])?)
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
                                   (1.0 - (c.embedding_vector <=> $1::vector))::REAL AS score,
                                   d.source_kind AS source_kind
                            FROM {qualified_document_chunks_table} c
                            JOIN {qualified_documents_table} d ON d.document_id = c.document_id
                            WHERE c.embedding_vector IS NOT NULL
                              AND c.embedding_provider = $2
                              AND c.embedding_model = $3
                              AND c.embedding_dimensions = $4
                              AND c.workspace_id = $5
                              AND (
                                  d.visibility IN ('global', 'workspace')
                                  OR ($6::TEXT IS NOT NULL AND c.owner_id = $6)
                              )
                            ORDER BY c.embedding_vector <=> $1::vector
                            LIMIT $7
                            "
                        );
                        let candidate_limit = limit_i64.saturating_mul(4).max(limit_i64);
                        let rows = client.with_client(|client| {
                            Ok(client.query(
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
                            )?)
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
                              d.visibility IN ('global', 'workspace')
                              OR ($5::TEXT IS NOT NULL AND c.owner_id = $5)
                          )
                        ORDER BY c.id
                        LIMIT $6
                        "
                    );
                    let rows = client.with_client(|client| {
                        Ok(client.query(
                            &vector_stmt,
                            &[
                                &embedding_provider,
                                &embedding_model,
                                &embedding_dimensions_i64,
                                &principal.workspace_id,
                                &owner_id,
                                &candidate_limit,
                            ],
                        )?)
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
            if let Some(embedding) = self.get_or_compute_embedding(&content).await? {
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
                            let vector_stmt =
                                format!("UPDATE {qualified_table} SET embedding_vector = $1::vector WHERE id = $2");
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
            if let Some(embedding) = self.get_or_compute_embedding(&content).await? {
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
                                "UPDATE {qualified_document_chunks_table} SET embedding_vector = $1::vector WHERE chunk_id = $2"
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn postgres_memory_fabric_conformance_from_env() {
        let Ok(db_url) = std::env::var("OPENPRX_TEST_POSTGRES_URL") else {
            return;
        };
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
            workspace_id: "workspace-a".to_string(),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            session_key: Some("telegram_sender-1".to_string()),
            channel: Some("telegram".to_string()),
            sender: Some("sender-1".to_string()),
            owner_id: None,
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
    async fn postgres_embedding_vector_reindex_from_env() {
        let Ok(db_url) = std::env::var("OPENPRX_TEST_POSTGRES_URL") else {
            return;
        };
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
            owner_id: None,
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
        })
        .await
        .unwrap();
        mem.ingest_document(DocumentIngestInput {
            document_id: Some("doc-vector-postgres-2".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: None,
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
        })
        .await
        .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "Postgres should cache repeated document chunk embeddings"
        );
        let principal = MemoryPrincipal {
            workspace_id: "workspace-a".to_string(),
            agent_id: None,
            persona_id: None,
            session_key: Some("chat:postgres-vector".to_string()),
            channel: Some("terminal".to_string()),
            sender: Some("local-user".to_string()),
            owner_id: None,
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
            3,
            "Postgres document search should reuse the cached query embedding"
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
                        count >= 3,
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
}

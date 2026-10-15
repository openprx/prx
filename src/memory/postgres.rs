mod access;
mod decision_reads;
mod embedding;
mod events;
mod ingress_recovery;
mod memory_impl;
mod pool;
mod router_history;
mod router_learning;
mod rows;
mod schema;
#[cfg(test)]
mod tests;

pub use pool::PostgresPoolStats;
pub(crate) use pool::{POSTGRES_DEFAULT_POOL_MAX_SIZE, PostgresConnectionPool, PostgresSessionContext};
pub(crate) use rows::{quote_identifier, related_table_name, validate_identifier};

use super::embeddings;
use super::principal::ScopeParam;
use anyhow::{Context, Result};
use pool::POSTGRES_DEFAULT_CONNECT_TIMEOUT_SECS;
use postgres::NoTls;
use std::sync::Arc;
use std::time::Duration;

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

impl PostgresMemory {
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

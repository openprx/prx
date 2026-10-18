pub mod backend;
pub mod chunker;
pub mod compaction;
pub mod document;
pub mod embeddings;
pub mod fabric;
pub mod filter;
pub mod hygiene;
pub mod ingress_recovery;
pub mod lucid;
pub mod markdown;
pub mod none;
pub mod owner_predicate;
pub mod postgres;
pub mod principal;
pub mod retrieval;
mod router_history;
pub mod session_predicate;
pub mod snapshot;
pub mod sqlite;
pub mod topic;
pub mod traits;
pub mod vector;

pub use backend::{
    MemoryBackendKind, classify_memory_backend, default_memory_backend_key, memory_backend_profile,
    selectable_memory_backends, serves_tool_execution_ledger,
};
#[allow(unused_imports)]
pub use fabric::{MemoryEventRecording, MemoryEventWatcher, MemoryFabric, MessageEventScope, RuntimeAuthorityGuard};
pub use filter::{should_autosave_content, should_autosave_content_with_min};
pub use lucid::LucidMemory;
pub use markdown::MarkdownMemory;
pub use none::NoneMemory;
pub use postgres::PostgresMemory;
pub use principal::MemoryWriteContext;
pub use router_history::{
    RouterHistoryCommitReceipt, RouterHistoryDomain, RouterHistoryRead, RouterHistoryReadEvidence, RouterHistoryRow,
    RouterHistoryWriteEvidence,
};
pub use sqlite::SqliteMemory;
pub use traits::LifecycleState;
pub use traits::Memory;
#[allow(unused_imports)]
pub use traits::{
    ChatProfile, CompactionRun, CompactionRunInput, CompactionSourceEventRange, ConversationSessionSummary,
    ConversationTurn, DocumentChunkRecord, DocumentIngestInput, DocumentRecord, DocumentSearchResult, MemoryCategory,
    MemoryDraft, MemoryDraftInput, MemoryEntry, MemoryEvent, MemoryEventInput, MemoryLink, MemoryLinkInput,
    MemoryPrincipal, MemoryReadMode, MemoryStoreMetadata, MemoryVisibility, MessageEvent, MessageEventInput,
    MessageEventSource, MessageEventSubject, RetrievalTrace, RetrievalTraceInput, RetrievedContextItem,
    SessionContextQuery, SharedContextQuery,
};

use crate::config::{
    EmbeddingRouteConfig, IdentityBindingConfig, MemoryConfig, StorageProviderConfig, UserPolicyConfig,
};
use anyhow::Context;
use chrono::Local;
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

fn create_memory_with_builders<F, G>(
    backend_name: &str,
    workspace_dir: &Path,
    memory_config: &MemoryConfig,
    mut sqlite_builder: F,
    mut postgres_builder: G,
    unknown_context: &str,
) -> anyhow::Result<Box<dyn Memory>>
where
    F: FnMut() -> anyhow::Result<SqliteMemory>,
    G: FnMut() -> anyhow::Result<PostgresMemory>,
{
    match classify_memory_backend(backend_name) {
        MemoryBackendKind::Sqlite => Ok(Box::new(sqlite_builder()?)),
        MemoryBackendKind::Lucid => {
            let local = sqlite_builder()?;
            Ok(Box::new(LucidMemory::new(workspace_dir, local, memory_config)))
        }
        MemoryBackendKind::Postgres => Ok(Box::new(postgres_builder()?)),
        MemoryBackendKind::Markdown => Ok(Box::new(MarkdownMemory::new(workspace_dir))),
        MemoryBackendKind::None => Ok(Box::new(NoneMemory::new())),
        MemoryBackendKind::Unknown => {
            tracing::warn!("Unknown memory backend '{backend_name}'{unknown_context}, falling back to markdown");
            Ok(Box::new(MarkdownMemory::new(workspace_dir)))
        }
    }
}

pub fn effective_memory_backend_name(memory_backend: &str, storage_provider: Option<&StorageProviderConfig>) -> String {
    if let Some(override_provider) = storage_provider
        .map(|cfg| cfg.provider.trim())
        .filter(|provider| !provider.is_empty())
    {
        return override_provider.to_ascii_lowercase();
    }

    memory_backend.trim().to_ascii_lowercase()
}

/// Legacy auto-save key used for model-authored assistant summaries.
/// These entries are treated as untrusted context and should not be re-injected.
pub fn is_assistant_autosave_key(key: &str) -> bool {
    let normalized = key.trim().to_ascii_lowercase();
    normalized == "assistant_resp" || normalized.starts_with("assistant_resp_")
}

#[derive(Clone, PartialEq, Eq)]
struct ResolvedEmbeddingConfig {
    provider: String,
    model: String,
    dimensions: usize,
    api_key: Option<String>,
    /// The base URL `api_key` belongs to, when it is the root key and the
    /// embedder honours a base URL. `None` means the vendor's default host.
    api_url: Option<String>,
}

impl std::fmt::Debug for ResolvedEmbeddingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedEmbeddingConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("dimensions", &self.dimensions)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("api_url", &self.api_url)
            .finish()
    }
}

/// The root credential and the provider it was issued for.
///
/// The memory embedder may be a different vendor from the chat provider
/// (`[memory] embedding_provider`, or an `[[embedding_routes]]` entry), so the
/// root `api_key` is released to it only when it is the same provider
/// identity as the one the key was configured for. A route's own `api_key` is
/// that route's credential and is always used. No `Debug`: it holds a secret.
#[derive(Clone, Copy, Default)]
pub struct EmbeddingRootCredential<'a> {
    owner: Option<&'a str>,
    api_key: Option<&'a str>,
    api_url: Option<&'a str>,
}

impl<'a> EmbeddingRootCredential<'a> {
    /// The root `api_key`, issued for the configured default provider (with
    /// the same catalog-owned fallback the provider resolution uses).
    #[must_use]
    pub fn from_config(config: &'a crate::config::Config) -> Self {
        Self {
            owner: Some(crate::providers::selection::default_provider(config)),
            api_key: config.api_key.as_deref(),
            api_url: config.api_url.as_deref(),
        }
    }

    /// A credential the caller vouches was issued for `provider`.
    #[must_use]
    pub const fn issued_for(provider: &'a str, api_key: Option<&'a str>) -> Self {
        Self {
            owner: Some(provider),
            api_key,
            api_url: None,
        }
    }

    /// No root credential at all.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            owner: None,
            api_key: None,
            api_url: None,
        }
    }

    /// The root key and the base URL it belongs to, if `embedding_provider` is
    /// the provider the key was issued for.
    ///
    /// The URL is attached where the embedder honours one, which is the
    /// `openai` embedder, as the `openai` chat provider does. Every other
    /// embedder addresses the same host the chat provider of that identity
    /// does without the root URL (`openrouter`) or carries it in its name
    /// (`custom:<url>`), so the key is never detached from its endpoint.
    fn release_to(&self, embedding_provider: &str) -> (Option<String>, Option<String>) {
        let Some(key) =
            crate::providers::credential_for_provider_identity(embedding_provider, &[(self.owner, self.api_key)])
        else {
            return (None, None);
        };
        let honours_base_url =
            crate::providers::ProviderId::of(embedding_provider) == crate::providers::ProviderId::of("openai");
        let url = honours_base_url
            .then_some(self.api_url)
            .flatten()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string);
        (Some(key.to_string()), url)
    }
}

/// The provider and model the memory embedder of `config` selects, after
/// its `hint:` embedding route: the embedding default the configuration
/// transaction validates (PMM-D-5, D-078 T-3). No credential is read. An
/// embedding hint no route carries comes back unresolved (`hint:<x>`).
#[must_use]
pub(crate) fn configured_embedder(config: &crate::config::Config) -> (String, String) {
    let resolved = resolve_embedding_config(
        &config.memory,
        &config.embedding_routes,
        EmbeddingRootCredential::none(),
    );
    (resolved.provider, resolved.model)
}

fn resolve_embedding_config(
    config: &MemoryConfig,
    embedding_routes: &[EmbeddingRouteConfig],
    root: EmbeddingRootCredential<'_>,
) -> ResolvedEmbeddingConfig {
    let fallback_provider = config.embedding_provider.trim();
    let (fallback_key, fallback_url) = root.release_to(fallback_provider);
    let fallback = ResolvedEmbeddingConfig {
        provider: fallback_provider.to_string(),
        model: config.embedding_model.trim().to_string(),
        dimensions: config.embedding_dimensions,
        api_key: fallback_key,
        api_url: fallback_url,
    };

    let Some(hint) = config
        .embedding_model
        .strip_prefix("hint:")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return fallback;
    };

    let Some(route) = embedding_routes.iter().find(|route| route.hint.trim() == hint) else {
        tracing::warn!(
            hint,
            "Unknown embedding route hint; falling back to [memory] embedding settings"
        );
        return fallback;
    };

    let provider = route.provider.trim();
    let model = route.model.trim();
    let dimensions = route.dimensions.unwrap_or(config.embedding_dimensions);
    if provider.is_empty() || model.is_empty() || dimensions == 0 {
        tracing::warn!(
            hint,
            "Invalid embedding route configuration; falling back to [memory] embedding settings"
        );
        return fallback;
    }

    let routed_api_key = route
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|value: &&str| !value.is_empty())
        .map(|value| value.to_string());

    // A route's own key is presented alone; otherwise the root key and its
    // base URL travel together or not at all.
    let (api_key, api_url) = routed_api_key.map_or_else(|| root.release_to(provider), |key| (Some(key), None));
    ResolvedEmbeddingConfig {
        provider: provider.to_string(),
        model: model.to_string(),
        dimensions,
        api_key,
        api_url,
    }
}

/// Preserve concrete local computation authority for Router admission. Other
/// providers remain ordinary dynamic embedders and gain no local-call permit.
pub(crate) enum ConfiguredEmbedding {
    LocalHash(Arc<embeddings::LocalHashEmbedding>),
    Other(Arc<dyn embeddings::EmbeddingProvider>),
}

pub(crate) fn create_configured_embedder(config: &crate::config::Config) -> ConfiguredEmbedding {
    let resolved = resolve_embedding_config(
        &config.memory,
        &config.embedding_routes_with_references(),
        EmbeddingRootCredential::from_config(config),
    );
    let catalog = crate::providers::selection::catalog_for(config);
    if resolved.provider.trim().eq_ignore_ascii_case("local")
        && crate::providers::selection::check_embedder(&catalog, &resolved.provider, &resolved.model).is_ok()
    {
        ConfiguredEmbedding::LocalHash(Arc::new(embeddings::LocalHashEmbedding::new(
            &resolved.model,
            resolved.dimensions,
        )))
    } else {
        ConfiguredEmbedding::Other(Arc::from(build_embedder(&resolved, &catalog)))
    }
}

/// A paid Router target produced from exactly the memory embedding resolver.
/// Credential ownership stays in that resolver and the concrete provider.
#[cfg(feature = "llm-router")]
pub(crate) struct ConfiguredPaidEmbedding {
    pub provider: String,
    pub model: String,
    pub dimensions: usize,
    pub embedder: Arc<dyn embeddings::EmbeddingProvider>,
}

#[cfg(feature = "llm-router")]
pub(crate) fn configured_paid_embedder(
    config: &crate::config::Config,
    catalog: &crate::providers::catalog::CatalogView,
) -> anyhow::Result<ConfiguredPaidEmbedding> {
    let resolved = resolve_embedding_config(
        &config.memory,
        &config.embedding_routes_with_references(),
        EmbeddingRootCredential::from_config(config),
    );
    anyhow::ensure!(
        !crate::providers::selection::is_prx_embedder(&resolved.provider),
        "Router paid embedding target is not paid"
    );
    crate::providers::selection::check_embedder(catalog, &resolved.provider, &resolved.model)?;
    let embedder: Arc<dyn embeddings::EmbeddingProvider> = Arc::from(build_embedder(&resolved, catalog));
    anyhow::ensure!(
        embedder.supports_attempt_accounting()
            && embedder.model() == resolved.model
            && embedder.dimensions() == resolved.dimensions,
        "Router configured embedding target is not instrumented"
    );
    Ok(ConfiguredPaidEmbedding {
        provider: resolved.provider,
        model: resolved.model,
        dimensions: resolved.dimensions,
        embedder,
    })
}

pub fn create_embedder_from_config(config: &crate::config::Config) -> Arc<dyn embeddings::EmbeddingProvider> {
    match create_configured_embedder(config) {
        ConfiguredEmbedding::LocalHash(embedder) => embedder,
        ConfiguredEmbedding::Other(embedder) => embedder,
    }
}

/// Build the embedder `resolved` selects, after resolving it for embeddings
/// against `catalog`. A selection the resolver refuses — a provider with no
/// embedding adapter, a model the evidence rules out for embeddings — is
/// reported and builds the no-op embedder (memory then recalls by keyword),
/// which is what an unknown embedder always built, now with the reason.
fn build_embedder(
    resolved: &ResolvedEmbeddingConfig,
    catalog: &crate::providers::catalog::CatalogView,
) -> Box<dyn embeddings::EmbeddingProvider> {
    if let Err(refusal) = crate::providers::selection::check_embedder(catalog, &resolved.provider, &resolved.model) {
        tracing::warn!(
            provider = %crate::providers::redact_endpoint_userinfo(&resolved.provider),
            model = %resolved.model,
            "embedder refused, memory recall falls back to keywords: {refusal}"
        );
        return Box::new(embeddings::NoopEmbedding);
    }
    embeddings::create_embedding_provider_with_base_url(
        &resolved.provider,
        resolved.api_key.as_deref(),
        resolved.api_url.as_deref(),
        &resolved.model,
        resolved.dimensions,
    )
}

/// File name of the dedicated tool-execution ledger, opened inside the
/// workspace `memory/` directory when the configured backend cannot serve as
/// one itself.
const TOOL_LEDGER_DB_FILE: &str = "tool-ledger.db";

/// Process-wide cache of dedicated ledgers, keyed by ledger file path.
///
/// One `SqliteMemory` per path keeps a single connection pool per ledger file
/// instead of reopening it for every turn.
static TOOL_LEDGERS: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashMap<PathBuf, Arc<SqliteMemory>>>> =
    std::sync::OnceLock::new();

/// Resolve the durable ledger that backs tool-execution idempotency.
///
/// The ledger is execution infrastructure rather than a memory feature: without
/// one, every side-effecting tool is refused. A configured backend that already
/// keeps a durable `MessageEvent` log serves as its own ledger; every other
/// backend (`markdown`, `none`, custom) gets a dedicated workspace-local SQLite
/// ledger so idempotency is preserved instead of silently disabling every
/// side-effecting tool.
///
/// Returns `None` only when the dedicated ledger cannot be opened. Callers then
/// run without a ledger, which keeps the fail-closed refusal intact.
pub fn tool_execution_ledger(memory: &Arc<dyn Memory>, workspace_dir: &Path) -> Option<Arc<dyn Memory>> {
    if backend::serves_tool_execution_ledger(memory.name()) {
        return Some(Arc::clone(memory));
    }
    dedicated_tool_execution_ledger(workspace_dir)
}

/// Open the workspace-local tool-execution ledger without consulting a memory
/// backend.
///
/// For runtimes that hold no memory handle at all. They still run
/// side-effecting tools, so they still need a ledger.
pub fn dedicated_tool_execution_ledger(workspace_dir: &Path) -> Option<Arc<dyn Memory>> {
    let path = workspace_dir.join("memory").join(TOOL_LEDGER_DB_FILE);
    let cache = TOOL_LEDGERS.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
    let mut guard = cache.lock();
    if let Some(existing) = guard.get(&path) {
        return Some(Arc::clone(existing) as Arc<dyn Memory>);
    }
    match SqliteMemory::new_with_path(path.clone()) {
        Ok(ledger) => {
            let ledger = Arc::new(ledger);
            guard.insert(path, Arc::clone(&ledger));
            Some(ledger as Arc<dyn Memory>)
        }
        Err(error) => {
            tracing::error!(
                path = %path.display(),
                error = %error,
                "unable to open the dedicated tool-execution ledger; side-effecting tools will be refused"
            );
            None
        }
    }
}

/// Factory: create the right memory backend from config.
///
/// `api_key`, when given, must be a credential issued for
/// `config.embedding_provider` itself; it is never another provider's key.
pub fn create_memory(
    config: &MemoryConfig,
    workspace_dir: &Path,
    api_key: Option<&str>,
) -> anyhow::Result<Box<dyn Memory>> {
    let root = EmbeddingRootCredential::issued_for(&config.embedding_provider, api_key);
    create_memory_with_storage_and_routes_with_acl(config, &[], None, workspace_dir, root, &[], &[])
}

/// Factory: create memory with optional storage-provider override.
pub fn create_memory_with_storage(
    config: &MemoryConfig,
    storage_provider: Option<&StorageProviderConfig>,
    workspace_dir: &Path,
    root: EmbeddingRootCredential<'_>,
) -> anyhow::Result<Box<dyn Memory>> {
    create_memory_with_storage_and_routes_with_acl(config, &[], storage_provider, workspace_dir, root, &[], &[])
}

/// Factory: create memory with optional storage-provider override and embedding routes.
pub fn create_memory_with_storage_and_routes(
    config: &MemoryConfig,
    embedding_routes: &[EmbeddingRouteConfig],
    storage_provider: Option<&StorageProviderConfig>,
    workspace_dir: &Path,
    root: EmbeddingRootCredential<'_>,
) -> anyhow::Result<Box<dyn Memory>> {
    create_memory_with_storage_and_routes_with_acl(
        config,
        embedding_routes,
        storage_provider,
        workspace_dir,
        root,
        &[],
        &[],
    )
}

/// Factory: create memory with optional storage override, embedding routes,
/// and startup ACL bootstrap records.
pub fn create_memory_with_storage_and_routes_with_acl(
    config: &MemoryConfig,
    embedding_routes: &[EmbeddingRouteConfig],
    storage_provider: Option<&StorageProviderConfig>,
    workspace_dir: &Path,
    root: EmbeddingRootCredential<'_>,
    identity_bindings: &[IdentityBindingConfig],
    user_policies: &[UserPolicyConfig],
) -> anyhow::Result<Box<dyn Memory>> {
    let backend_name = effective_memory_backend_name(&config.backend, storage_provider);
    #[cfg(feature = "wasm-plugins")]
    if backend_name.starts_with("wasm:") {
        return crate::plugins::runtime::resolve_process_storage(&backend_name)
            .ok_or_else(|| anyhow::anyhow!("WASM storage backend '{backend_name}' is not loaded in this process"));
    }
    let backend_kind = classify_memory_backend(&backend_name);
    let resolved_embedding = resolve_embedding_config(config, embedding_routes, root);

    // Auto-hydration: if brain.db is missing but MEMORY_SNAPSHOT.md exists,
    // restore the "soul" from the snapshot before creating the backend.
    if config.auto_hydrate
        && matches!(backend_kind, MemoryBackendKind::Sqlite | MemoryBackendKind::Lucid)
        && snapshot::should_hydrate(workspace_dir)
    {
        tracing::info!("🧬 Cold boot detected — hydrating from MEMORY_SNAPSHOT.md");
        match snapshot::hydrate_from_snapshot(workspace_dir) {
            Ok(count) => {
                if count > 0 {
                    tracing::info!("🧬 Hydrated {count} core memories from snapshot");
                }
            }
            Err(e) => {
                tracing::warn!("memory hydration failed: {e}");
            }
        }
    }

    fn build_sqlite_memory(
        config: &MemoryConfig,
        workspace_dir: &Path,
        resolved_embedding: &ResolvedEmbeddingConfig,
    ) -> anyhow::Result<SqliteMemory> {
        // The memory builders see `[memory]` only, so the evidence here is the
        // adapter table; operator declarations reach `create_embedder_from_config`.
        let embedder: Arc<dyn embeddings::EmbeddingProvider> = Arc::from(build_embedder(
            resolved_embedding,
            &crate::providers::catalog::CatalogView::default(),
        ));

        #[allow(clippy::cast_possible_truncation)]
        let mem = SqliteMemory::with_embedder_with_acl(
            workspace_dir,
            embedder,
            config.vector_weight as f32,
            config.keyword_weight as f32,
            config.embedding_cache_size,
            config.sqlite_open_timeout_secs,
            config.acl_enabled,
            config.sqlite_read_pool_size,
        )?
        .with_vector_candidate_cap(config.vector_candidate_cap);
        Ok(mem)
    }

    fn build_postgres_memory(
        storage_provider: Option<&StorageProviderConfig>,
        resolved_embedding: &ResolvedEmbeddingConfig,
        vector_weight: f64,
        keyword_weight: f64,
        embedding_cache_size: usize,
    ) -> anyhow::Result<PostgresMemory> {
        let storage_provider =
            storage_provider.context("memory backend 'postgres' requires [storage.provider.config] settings")?;
        let db_url = storage_provider
            .db_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("memory backend 'postgres' requires [storage.provider.config].db_url (or dbURL)")?;

        // The memory builders see `[memory]` only, so the evidence here is the
        // adapter table; operator declarations reach `create_embedder_from_config`.
        let embedder: Arc<dyn embeddings::EmbeddingProvider> = Arc::from(build_embedder(
            resolved_embedding,
            &crate::providers::catalog::CatalogView::default(),
        ));

        #[allow(clippy::cast_possible_truncation)]
        PostgresMemory::with_embedder_and_cache(
            db_url,
            &storage_provider.schema,
            &storage_provider.table,
            storage_provider.connect_timeout_secs,
            embedder,
            vector_weight as f32,
            keyword_weight as f32,
            embedding_cache_size,
            storage_provider.pool_max_size,
        )
    }

    let memory = create_memory_with_builders(
        &backend_name,
        workspace_dir,
        config,
        || build_sqlite_memory(config, workspace_dir, &resolved_embedding),
        || {
            build_postgres_memory(
                storage_provider,
                &resolved_embedding,
                config.vector_weight,
                config.keyword_weight,
                config.embedding_cache_size,
            )
        },
        "",
    )?;

    // Hygiene and the snapshot export read the memory tables, so they run
    // after the backend exists: on a fresh workspace whose `brain.db` another
    // store created first (the Xin or cron tables), running them earlier
    // failed with "no such table: memories" (D-067). Hydration above still
    // runs before the backend, because it restores a missing database.
    // Best-effort memory hygiene/retention pass (throttled by state file).
    if let Err(e) = hygiene::run_if_due(config, workspace_dir) {
        tracing::warn!(
            "memory hygiene skipped: {}",
            crate::security::redaction::redact_secrets(&format!("{e:#}"))
        );
    }
    if matches!(backend_kind, MemoryBackendKind::Sqlite | MemoryBackendKind::Lucid) {
        if let Err(e) = snapshot::export_snapshot(workspace_dir) {
            tracing::warn!(
                "memory snapshot skipped: {}",
                crate::security::redaction::redact_secrets(&format!("{e:#}"))
            );
        }
    }

    if matches!(backend_kind, MemoryBackendKind::Sqlite | MemoryBackendKind::Lucid) {
        if let Err(error) = upsert_acl_bootstrap(workspace_dir, identity_bindings, user_policies) {
            tracing::warn!("memory ACL bootstrap upsert skipped: {error}");
        }
    }

    Ok(memory)
}

fn upsert_acl_bootstrap(
    workspace_dir: &Path,
    identity_bindings: &[IdentityBindingConfig],
    user_policies: &[UserPolicyConfig],
) -> anyhow::Result<()> {
    if identity_bindings.is_empty() && user_policies.is_empty() {
        return Ok(());
    }

    let db_path = workspace_dir.join("memory").join("brain.db");
    let conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;

    let tx = conn.unchecked_transaction()?;
    let now = Local::now().to_rfc3339();

    for binding in identity_bindings {
        tx.execute(
            "INSERT INTO identity_bindings (id, user_id, channel, channel_account, display_name, bound_at, bound_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'system')
             ON CONFLICT(channel, channel_account) DO UPDATE SET
                 user_id = excluded.user_id,
                 display_name = excluded.display_name,
                 bound_at = excluded.bound_at,
                 bound_by = excluded.bound_by",
            params![
                Uuid::new_v4().to_string(),
                binding.user_id,
                binding.channel,
                binding.channel_account,
                binding.display_name,
                now
            ],
        )?;
    }

    for policy in user_policies {
        let projects = serde_json::to_string(&policy.projects)?;
        let blocked_patterns = serde_json::to_string(&policy.blocked_patterns)?;
        tx.execute(
            "INSERT INTO user_policies (user_id, role, projects, visibility_ceiling, blocked_patterns, policy_version, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)
             ON CONFLICT(user_id) DO UPDATE SET
                 role = excluded.role,
                 projects = excluded.projects,
                 visibility_ceiling = excluded.visibility_ceiling,
                 blocked_patterns = excluded.blocked_patterns,
                 policy_version = excluded.policy_version,
                 updated_at = excluded.updated_at",
            params![
                policy.user_id,
                policy.role,
                projects,
                policy.visibility_ceiling,
                blocked_patterns,
                now
            ],
        )?;
    }

    tx.commit()?;
    Ok(())
}

pub fn create_memory_for_migration(backend: &str, workspace_dir: &Path) -> anyhow::Result<Box<dyn Memory>> {
    if backend.starts_with("wasm:") {
        anyhow::bail!("memory migration for WASM storage backend '{backend}' is unsupported");
    }
    if matches!(classify_memory_backend(backend), MemoryBackendKind::None) {
        anyhow::bail!("memory backend 'none' disables persistence; choose sqlite, lucid, or markdown before migration");
    }

    if matches!(classify_memory_backend(backend), MemoryBackendKind::Postgres) {
        anyhow::bail!("memory migration for backend 'postgres' is unsupported; migrate with sqlite or markdown first");
    }

    let default_config = MemoryConfig::default();
    create_memory_with_builders(
        backend,
        workspace_dir,
        &default_config,
        || SqliteMemory::new(workspace_dir),
        || anyhow::bail!("postgres backend is not available in migration context"),
        " during migration",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EmbeddingRouteConfig, StorageProviderConfig};
    use tempfile::TempDir;

    /// D-067, RED before: on a fresh workspace whose `brain.db` another store
    /// had already created (without the `memories` table), hygiene and the
    /// snapshot export ran before the sqlite backend created its tables, so
    /// hygiene failed with "no such table: memories" and never recorded its
    /// run. After: they run once the tables exist; hygiene completes and
    /// writes its state file.
    #[test]
    fn hygiene_and_snapshot_run_after_the_schema_exists_on_a_fresh_workspace() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        // Another store's table, as `xin` or `cron` create first.
        Connection::open(memory_dir.join("brain.db"))
            .unwrap()
            .execute_batch("CREATE TABLE other_store (id TEXT PRIMARY KEY);")
            .unwrap();
        let cfg = MemoryConfig {
            backend: "sqlite".into(),
            ..MemoryConfig::default()
        };
        let mem = create_memory(&cfg, tmp.path(), None).unwrap();
        assert_eq!(mem.name(), "sqlite");
        assert!(
            tmp.path().join("state").join("memory_hygiene_state.json").is_file(),
            "hygiene completed and recorded its run"
        );
    }

    #[test]
    fn factory_sqlite() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "sqlite".into(),
            ..MemoryConfig::default()
        };
        let mem = create_memory(&cfg, tmp.path(), None).unwrap();
        assert_eq!(mem.name(), "sqlite");
    }

    #[test]
    fn assistant_autosave_key_detection_matches_legacy_patterns() {
        assert!(is_assistant_autosave_key("assistant_resp"));
        assert!(is_assistant_autosave_key("assistant_resp_1234"));
        assert!(is_assistant_autosave_key("ASSISTANT_RESP_abcd"));
        assert!(!is_assistant_autosave_key("assistant_response"));
        assert!(!is_assistant_autosave_key("user_msg_1234"));
    }

    #[test]
    fn factory_markdown() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem = create_memory(&cfg, tmp.path(), None).unwrap();
        assert_eq!(mem.name(), "markdown");
    }

    #[test]
    fn factory_lucid() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "lucid".into(),
            ..MemoryConfig::default()
        };
        let mem = create_memory(&cfg, tmp.path(), None).unwrap();
        assert_eq!(mem.name(), "lucid");
    }

    #[test]
    fn factory_none_uses_noop_memory() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "none".into(),
            ..MemoryConfig::default()
        };
        let mem = create_memory(&cfg, tmp.path(), None).unwrap();
        assert_eq!(mem.name(), "none");
    }

    #[test]
    fn factory_unknown_falls_back_to_markdown() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "redis".into(),
            ..MemoryConfig::default()
        };
        let mem = create_memory(&cfg, tmp.path(), None).unwrap();
        assert_eq!(mem.name(), "markdown");
    }

    #[test]
    fn migration_factory_lucid() {
        let tmp = TempDir::new().unwrap();
        let mem = create_memory_for_migration("lucid", tmp.path()).unwrap();
        assert_eq!(mem.name(), "lucid");
    }

    #[test]
    fn migration_factory_none_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let error = create_memory_for_migration("none", tmp.path())
            .err()
            .expect("backend=none should be rejected for migration");
        assert!(error.to_string().contains("disables persistence"));
    }

    #[test]
    fn effective_backend_name_prefers_storage_override() {
        let storage = StorageProviderConfig {
            provider: "postgres".into(),
            ..StorageProviderConfig::default()
        };

        assert_eq!(effective_memory_backend_name("sqlite", Some(&storage)), "postgres");
    }

    #[test]
    fn factory_postgres_without_db_url_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "postgres".into(),
            ..MemoryConfig::default()
        };

        let storage = StorageProviderConfig {
            provider: "postgres".into(),
            db_url: None,
            ..StorageProviderConfig::default()
        };

        let error = create_memory_with_storage(&cfg, Some(&storage), tmp.path(), EmbeddingRootCredential::none())
            .err()
            .expect("postgres without db_url should be rejected");
        assert!(error.to_string().contains("db_url"));
    }

    #[test]
    fn resolve_embedding_config_uses_base_config_when_model_is_not_hint() {
        let cfg = MemoryConfig {
            embedding_provider: "openai".into(),
            embedding_model: "text-embedding-3-small".into(),
            embedding_dimensions: 1536,
            ..MemoryConfig::default()
        };

        let resolved = resolve_embedding_config(
            &cfg,
            &[],
            EmbeddingRootCredential::issued_for("openai", Some("base-key")),
        );
        assert_eq!(
            resolved,
            ResolvedEmbeddingConfig {
                provider: "openai".into(),
                model: "text-embedding-3-small".into(),
                dimensions: 1536,
                api_key: Some("base-key".into()),
                api_url: None,
            }
        );
    }

    #[test]
    fn resolve_embedding_config_uses_matching_route_with_api_key_override() {
        let cfg = MemoryConfig {
            embedding_provider: "none".into(),
            embedding_model: "hint:semantic".into(),
            embedding_dimensions: 1536,
            ..MemoryConfig::default()
        };
        let routes = vec![EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "custom:https://api.example.com/v1".into(),
            model: "custom-embed-v2".into(),
            dimensions: Some(1024),
            api_key: Some("route-key".into()),
            credential_ref: None,
        }];

        let resolved = resolve_embedding_config(
            &cfg,
            &routes,
            EmbeddingRootCredential::issued_for("openai", Some("base-key")),
        );
        assert_eq!(
            resolved,
            ResolvedEmbeddingConfig {
                provider: "custom:https://api.example.com/v1".into(),
                model: "custom-embed-v2".into(),
                dimensions: 1024,
                api_key: Some("route-key".into()),
                api_url: None,
            }
        );
    }

    #[test]
    fn resolve_embedding_config_falls_back_when_hint_is_missing() {
        let cfg = MemoryConfig {
            embedding_provider: "openai".into(),
            embedding_model: "hint:semantic".into(),
            embedding_dimensions: 1536,
            ..MemoryConfig::default()
        };

        let resolved = resolve_embedding_config(
            &cfg,
            &[],
            EmbeddingRootCredential::issued_for("openai", Some("base-key")),
        );
        assert_eq!(
            resolved,
            ResolvedEmbeddingConfig {
                provider: "openai".into(),
                model: "hint:semantic".into(),
                dimensions: 1536,
                api_key: Some("base-key".into()),
                api_url: None,
            }
        );
    }

    #[test]
    fn resolve_embedding_config_falls_back_when_route_is_invalid() {
        let cfg = MemoryConfig {
            embedding_provider: "openai".into(),
            embedding_model: "hint:semantic".into(),
            embedding_dimensions: 1536,
            ..MemoryConfig::default()
        };
        let routes = vec![EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: String::new(),
            model: "text-embedding-3-small".into(),
            dimensions: Some(0),
            api_key: None,
            credential_ref: None,
        }];

        let resolved = resolve_embedding_config(
            &cfg,
            &routes,
            EmbeddingRootCredential::issued_for("openai", Some("base-key")),
        );
        assert_eq!(
            resolved,
            ResolvedEmbeddingConfig {
                provider: "openai".into(),
                model: "hint:semantic".into(),
                dimensions: 1536,
                api_key: Some("base-key".into()),
                api_url: None,
            }
        );
    }
    #[test]
    fn tool_execution_ledger_reuses_a_backend_that_already_keeps_an_event_log() {
        let dir = tempfile::TempDir::new().expect("test: tempdir");
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).expect("test: sqlite memory"));
        let ledger = tool_execution_ledger(&memory, dir.path()).expect("test: ledger");
        assert!(
            Arc::ptr_eq(&memory, &ledger),
            "a ledger-capable backend must serve as its own ledger"
        );
    }

    #[test]
    fn tool_execution_ledger_opens_a_dedicated_store_for_a_backend_without_an_event_log() {
        let dir = tempfile::TempDir::new().expect("test: tempdir");
        let memory: Arc<dyn Memory> = Arc::new(MarkdownMemory::new(dir.path()));
        let ledger = tool_execution_ledger(&memory, dir.path()).expect("test: ledger");
        assert_eq!(ledger.name(), "sqlite");
        assert!(
            dir.path().join("memory").join(TOOL_LEDGER_DB_FILE).exists(),
            "the dedicated ledger must be a real file in the workspace"
        );
        let again = tool_execution_ledger(&memory, dir.path()).expect("test: ledger");
        assert!(
            Arc::ptr_eq(&ledger, &again),
            "the dedicated ledger must be opened once per workspace"
        );
    }

    #[test]
    fn tool_execution_ledger_is_absent_when_the_dedicated_store_cannot_be_opened() {
        let dir = tempfile::TempDir::new().expect("test: tempdir");
        let blocked = dir.path().join("workspace-is-a-file");
        std::fs::write(&blocked, "not a directory").expect("test: block the workspace");
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        assert!(
            tool_execution_ledger(&memory, &blocked).is_none(),
            "an unopenable ledger must stay absent so side effects keep being refused"
        );
    }
}

/// C0-D: the root key belongs to the configured default provider. The
/// embedder, whether chosen by `[memory] embedding_provider` or by an
/// `[[embedding_routes]]` entry, receives it only when it is that provider.
/// Driven through the production embedder factory and a loopback endpoint;
/// every credential is a sentinel.
#[cfg(test)]
mod embedder_credential_tests {
    use crate::config::Config;
    use crate::providers::credential_probe::CredentialProbe;

    const ROOT_SENTINEL: &str = "C0-EMBED-ROOT-SENTINEL";
    const ROUTE_SENTINEL: &str = "C0-EMBED-ROUTE-SENTINEL";

    fn root_config(temp: &tempfile::TempDir) -> Config {
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        config.config_path = temp.path().join("config.toml");
        config.default_provider = Some("openai".to_string());
        config.api_key = Some(ROOT_SENTINEL.to_string());
        config.memory.embedding_model = "probe-embed".to_string();
        config.memory.embedding_dimensions = 3;
        config
    }

    #[tokio::test]
    async fn an_embedding_provider_other_than_the_default_does_not_receive_the_root_key() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let probe = CredentialProbe::start().await;
        let mut config = root_config(&temp);
        config.memory.embedding_provider = probe.custom_provider();

        let embedder = super::create_embedder_from_config(&config);
        let _vectors = embedder.embed(&["probe"]).await;

        assert_eq!(probe.requests().len(), 1);
        assert!(
            !probe.saw(ROOT_SENTINEL),
            "the root key reached another embedding vendor"
        );
    }

    #[tokio::test]
    async fn a_keyless_embedding_route_does_not_inherit_the_root_key() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let probe = CredentialProbe::start().await;
        let mut config = root_config(&temp);
        config.memory.embedding_model = "hint:semantic".to_string();
        config.embedding_routes = vec![crate::config::EmbeddingRouteConfig {
            hint: "semantic".to_string(),
            provider: probe.custom_provider(),
            model: "probe-embed".to_string(),
            dimensions: Some(3),
            api_key: None,
            credential_ref: None,
        }];

        let embedder = super::create_embedder_from_config(&config);
        let _vectors = embedder.embed(&["probe"]).await;

        assert_eq!(probe.requests().len(), 1);
        assert!(
            !probe.saw(ROOT_SENTINEL),
            "the root key reached a keyless embedding route"
        );
    }

    /// Guard: the default provider's own embedder keeps the root key, and a
    /// route's own key is still presented.
    #[tokio::test]
    async fn own_credentials_still_reach_their_embedders() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let own = CredentialProbe::start().await;
        let routed = CredentialProbe::start().await;
        let mut config = root_config(&temp);
        config.default_provider = Some(own.custom_provider());
        config.memory.embedding_provider = own.custom_provider();

        let embedder = super::create_embedder_from_config(&config);
        let _vectors = embedder.embed(&["probe"]).await;
        assert!(own.saw(ROOT_SENTINEL));

        config.memory.embedding_model = "hint:semantic".to_string();
        config.embedding_routes = vec![crate::config::EmbeddingRouteConfig {
            hint: "semantic".to_string(),
            provider: routed.custom_provider(),
            model: "probe-embed".to_string(),
            dimensions: Some(3),
            api_key: Some(ROUTE_SENTINEL.to_string()),
            credential_ref: None,
        }];
        let embedder = super::create_embedder_from_config(&config);
        let _vectors = embedder.embed(&["probe"]).await;
        assert!(routed.saw(ROUTE_SENTINEL) && !routed.saw(ROOT_SENTINEL));
    }

    #[tokio::test]
    async fn migrated_embedding_routes_send_each_stored_key_only_to_its_route() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let first = CredentialProbe::start().await;
        let second = CredentialProbe::start().await;
        let mut config = root_config(&temp);
        config.secrets.encrypt = false;
        config.memory.embedding_model = "hint:first".to_string();
        config.embedding_routes = [
            (&first, "first", "PRX-EMBED-FIRST-KEY"),
            (&second, "second", "PRX-EMBED-SECOND-KEY"),
        ]
        .into_iter()
        .map(|(probe, hint, key)| crate::config::EmbeddingRouteConfig {
            hint: hint.to_string(),
            provider: probe.custom_provider(),
            model: "probe-embed".to_string(),
            dimensions: Some(3),
            api_key: Some(key.to_string()),
            credential_ref: None,
        })
        .collect();

        let report = crate::config::manage::migrate::report(&config).expect("test: migration report");
        let values = crate::config::manage::migrate::inline_values(&config, &report);
        let references =
            crate::config::manage::credentials::CredentialReferences::for_config(&config).expect("test: references");
        for key in &report.inline_keys {
            if key.path.starts_with("embedding_routes[") {
                references
                    .set(
                        &key.provider,
                        &key.reference_name,
                        values.get(&key.path).expect("test: inline route key"),
                    )
                    .expect("test: store migrated route key");
            }
        }
        let mut migrated = crate::config::manage::migrate::migrated(&config, &report);
        assert!(migrated.embedding_routes.iter().all(|route| route.api_key.is_none()));
        assert_ne!(
            migrated
                .embedding_routes
                .first()
                .and_then(|route| route.credential_ref.as_ref()),
            migrated
                .embedding_routes
                .get(1)
                .and_then(|route| route.credential_ref.as_ref())
        );

        super::create_embedder_from_config(&migrated)
            .embed(&["probe"])
            .await
            .expect("test: first migrated embedding request");
        assert_eq!(first.requests().len(), 1);
        assert!(
            first
                .requests()
                .first()
                .is_some_and(|request| request.path.ends_with("/embeddings"))
        );
        assert!(first.saw("PRX-EMBED-FIRST-KEY"));
        assert!(!first.saw("PRX-EMBED-SECOND-KEY"));
        assert!(!first.saw(ROOT_SENTINEL));
        assert!(second.requests().is_empty());

        migrated.memory.embedding_model = "hint:second".to_string();
        super::create_embedder_from_config(&migrated)
            .embed(&["probe"])
            .await
            .expect("test: second migrated embedding request");
        assert_eq!(second.requests().len(), 1);
        assert!(
            second
                .requests()
                .first()
                .is_some_and(|request| request.path.ends_with("/embeddings"))
        );
        assert!(second.saw("PRX-EMBED-SECOND-KEY"));
        assert!(!second.saw("PRX-EMBED-FIRST-KEY"));
        assert!(!second.saw(ROOT_SENTINEL));
    }

    /// C0-f F-8: the root key of an `openai` default configured with its own
    /// base URL travels with that URL; it is never presented at the vendor's
    /// default host.
    #[test]
    fn the_root_key_reaches_the_embedder_only_with_its_base_url() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let mut config = root_config(&temp);
        config.api_url = Some("https://gateway.corp.example/v1".to_string());
        config.memory.embedding_provider = "openai".to_string();

        let resolved = super::resolve_embedding_config(
            &config.memory,
            &config.embedding_routes,
            super::EmbeddingRootCredential::from_config(&config),
        );
        assert_eq!(resolved.api_key.as_deref(), Some(ROOT_SENTINEL));
        assert_eq!(resolved.api_url.as_deref(), Some("https://gateway.corp.example/v1"));
    }

    /// On the wire: the embedder posts to the root URL with the root key.
    #[tokio::test]
    async fn f8_wire_the_openai_embedder_uses_the_root_url() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let probe = CredentialProbe::start().await;
        let mut config = root_config(&temp);
        config.api_url = Some(probe.base_url());
        config.memory.embedding_provider = "openai".to_string();

        let embedder = super::create_embedder_from_config(&config);
        let _vectors = embedder.embed(&["probe"]).await;

        assert_eq!(probe.requests().len(), 1);
        assert!(probe.saw(ROOT_SENTINEL));
    }
}

/// PMM-C (embedding surface): the embedder a configuration selects is
/// resolved for embeddings before it is built.
#[cfg(test)]
mod embedder_selection_tests {
    use super::create_embedder_from_config;
    use crate::config::{Config, EmbeddingRouteConfig};

    fn config_with(provider: &str, model: &str) -> Config {
        let mut config = Config::default();
        config.memory.embedding_provider = provider.to_string();
        config.memory.embedding_model = model.to_string();
        config.memory.embedding_dimensions = 8;
        config
    }

    #[test]
    fn an_embedding_route_to_a_chat_only_model_builds_no_embedder() {
        let mut config = config_with("none", "hint:semantic");
        config.embedding_routes = vec![EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            dimensions: Some(8),
            api_key: Some("PMMC-EMBED-KEY".into()),
            credential_ref: None,
        }];
        config.providers.models.push(
            toml::from_str("provider = \"openai\"\nmodel = \"gpt-4o\"\noperations = [\"chat\"]\n")
                .expect("test: declaration"),
        );
        assert_eq!(
            create_embedder_from_config(&config).name(),
            "none",
            "a model declared chat-only was built as an embedder"
        );
    }

    #[test]
    fn every_spelling_of_an_embedding_provider_builds_its_embedder() {
        assert_eq!(
            create_embedder_from_config(&config_with("OpenAI", "text-embedding-3-small")).name(),
            "openai"
        );
        assert_eq!(
            create_embedder_from_config(&config_with(" openai ", "text-embedding-3-small")).name(),
            "openai"
        );
        assert_eq!(
            create_embedder_from_config(&config_with("local", "prx-local-hash-v1")).name(),
            "local"
        );
        assert_eq!(create_embedder_from_config(&config_with("none", "x")).name(), "none");
        assert_eq!(
            create_embedder_from_config(&config_with("anthropic", "claude-embed")).name(),
            "none",
            "no embedding adapter"
        );
    }
}

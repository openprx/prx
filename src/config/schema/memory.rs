use super::{Deserialize, JsonSchema, Result, Serialize, default_true};

// ── Memory ───────────────────────────────────────────────────

/// Persistent storage configuration (`[storage]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct StorageConfig {
    /// Storage provider settings (e.g. sqlite, postgres).
    #[serde(default)]
    pub provider: StorageProviderSection,
}

/// Wrapper for the storage provider configuration section.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct StorageProviderSection {
    /// Storage provider backend settings.
    #[serde(default)]
    pub config: StorageProviderConfig,
}

/// Storage provider backend configuration (e.g. postgres connection details).
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct StorageProviderConfig {
    /// Storage engine key (e.g. "postgres", "sqlite").
    #[serde(default)]
    pub provider: String,

    /// Connection URL for remote providers.
    /// Accepts legacy aliases: dbURL, database_url, databaseUrl.
    #[serde(default, alias = "dbURL", alias = "database_url", alias = "databaseUrl")]
    pub db_url: Option<String>,

    /// Database schema for SQL backends.
    #[serde(default = "default_storage_schema")]
    pub schema: String,

    /// Table name for memory entries.
    #[serde(default = "default_storage_table")]
    pub table: String,

    /// Optional timeout in seconds for *establishing* a connection to a remote
    /// provider. Defaults to 15s when unset.
    ///
    /// This bounds the TCP/startup handshake only. Statement execution is never
    /// bounded (no `statement_timeout` is applied) because long-running queries
    /// are normal for agent workloads.
    #[serde(default)]
    pub connect_timeout_secs: Option<u64>,

    /// Maximum number of pooled connections for remote providers.
    ///
    /// The pool exists for connection *reuse*, not to throttle work: a caller
    /// that finds the pool saturated queues for a connection rather than
    /// failing. Size it against the database's own `max_connections` budget,
    /// not against expected task concurrency.
    #[serde(default = "default_storage_pool_max_size")]
    pub pool_max_size: usize,
}

pub(super) fn default_storage_schema() -> String {
    "public".into()
}

pub(super) fn default_storage_table() -> String {
    "memories".into()
}

pub(super) const fn default_storage_pool_max_size() -> usize {
    32
}

impl Default for StorageProviderConfig {
    fn default() -> Self {
        Self {
            provider: String::new(),
            db_url: None,
            schema: default_storage_schema(),
            table: default_storage_table(),
            connect_timeout_secs: None,
            pool_max_size: default_storage_pool_max_size(),
        }
    }
}

/// Memory backend configuration (`[memory]` section).
///
/// Controls conversation memory storage, embeddings, hybrid search, response caching,
/// and memory snapshot/hydration.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[allow(clippy::struct_excessive_bools)]
pub struct MemoryConfig {
    /// "sqlite" | "lucid" | "postgres" | "markdown" | "none" (`none` = explicit no-op memory)
    ///
    /// `postgres` requires `[storage.provider.config]` with `db_url` (`dbURL` alias supported).
    #[serde(default = "default_memory_backend")]
    pub backend: String,
    /// Backward-compatible semantic auto-promotion gate.
    ///
    /// Message events are controlled by `[memory.events]`; this controls whether
    /// eligible user messages are promoted into long-term semantic memories.
    #[serde(default = "default_memory_auto_save")]
    pub auto_save: bool,
    /// Raw/quasi-raw message event recording controls (`[memory.events]`).
    #[serde(default)]
    pub events: MemoryEventsConfig,
    /// Semantic memory promotion controls (`[memory.semantic]`).
    #[serde(default)]
    pub semantic: MemorySemanticConfig,
    /// Feature gate for memory ACL enforcement. Phase 0 keeps this disabled.
    #[serde(default)]
    pub acl_enabled: bool,
    /// Phase 1/2 compatibility gate for legacy conversation turns while owner_id
    /// backfill rolls out. Phase 3 can disable and remove this after NULL=0.
    #[serde(default = "default_true")]
    pub legacy_conversation_turns_visible: bool,
    /// Archive daily/session files older than this many days
    #[serde(default = "default_archive_after_days")]
    pub archive_after_days: u32,
    /// Purge archived files older than this many days
    #[serde(default = "default_purge_after_days")]
    pub purge_after_days: u32,
    /// For sqlite backend: prune conversation rows older than this many days
    #[serde(default = "default_conversation_retention_days")]
    pub conversation_retention_days: u32,
    /// For sqlite backend: prune daily rows older than this many days
    #[serde(default = "default_daily_retention_days")]
    pub daily_retention_days: u32,
    /// Embedding provider: "local" | "none" | "openai" | "custom:URL"
    #[serde(default = "default_embedding_provider")]
    pub embedding_provider: String,
    /// Internal presence bit populated by `Config::load_from_path`. The serde
    /// default for `embedding_provider` flipped from `"none"` to `"local"`, so
    /// the deserialized value alone cannot tell an operator who asked for local
    /// vector recall from one who simply never wrote the key and was upgraded
    /// into it.
    #[serde(default, skip_serializing)]
    #[schemars(skip)]
    pub embedding_provider_explicit: bool,
    /// Embedding model name (e.g. "text-embedding-3-small")
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    /// Embedding vector dimensions
    #[serde(default = "default_embedding_dims")]
    pub embedding_dimensions: usize,
    /// Weight for vector similarity in hybrid search (0.0–1.0)
    #[serde(default = "default_vector_weight")]
    pub vector_weight: f64,
    /// Weight for keyword BM25 in hybrid search (0.0–1.0)
    #[serde(default = "default_keyword_weight")]
    pub keyword_weight: f64,
    /// Minimum hybrid score (0.0–1.0) for a memory to be included in context.
    /// Memories scoring below this threshold are dropped to prevent irrelevant
    /// context from bleeding into conversations. Default: 0.4
    #[serde(default = "default_min_relevance_score")]
    pub min_relevance_score: f64,
    /// Max embedding cache entries before LRU eviction
    #[serde(default = "default_cache_size")]
    pub embedding_cache_size: usize,
    /// How many embedded memories one vector recall may score.
    ///
    /// Cosine similarity cannot use an index, so the candidate set is bounded
    /// by insertion order (newest first) before scoring; without a bound this
    /// is a full scan of every embedded memory, once per turn. The trade-off is
    /// real and not free: in a store with more embedded memories than this cap,
    /// an old-but-highly-relevant memory is never scored and therefore never
    /// recalled. Raise it to buy recall with latency, lower it to buy latency
    /// with recall. Vector recall logs a warning the first time a recall
    /// actually hits the cap, so a store that has outgrown it says so.
    #[serde(default = "default_vector_candidate_cap")]
    pub vector_candidate_cap: usize,

    // ── Memory Snapshot (soul backup to Markdown) ─────────────
    /// Auto-hydrate from MEMORY_SNAPSHOT.md when brain.db is missing
    #[serde(default = "default_true")]
    pub auto_hydrate: bool,

    // ── SQLite backend options ─────────────────────────────────
    /// For sqlite backend: max seconds to wait when opening the DB (e.g. file locked).
    /// None = wait indefinitely (default). Recommended max: 300.
    #[serde(default)]
    pub sqlite_open_timeout_secs: Option<u64>,

    /// For sqlite backend: how many reader connections may be opened.
    ///
    /// SQLite allows one writer but genuinely concurrent readers under WAL, so
    /// only reads are pooled. The pool is a connection *reuse* mechanism, not a
    /// concurrency limiter: a caller that finds every reader busy queues for one
    /// instead of failing. Unset derives the size from the available CPUs (see
    /// [`crate::runtime::sqlite_pool::default_read_pool_size`]); if set it must
    /// be greater than 0. Deliberately a separate key from
    /// `channels_config.whatsapp.read_pool_size`, so the two SQLite databases
    /// can be sized independently.
    #[serde(default)]
    pub sqlite_read_pool_size: Option<usize>,

    // ── Lucid memory backend options ─────────────────────────────
    /// Path to lucid binary (replaces ZEROCLAW_LUCID_CMD / OPENPRX_LUCID_CMD)
    #[serde(default)]
    pub lucid_cmd: Option<String>,

    /// Token budget for lucid (replaces ZEROCLAW_LUCID_BUDGET / OPENPRX_LUCID_BUDGET)
    #[serde(default)]
    pub lucid_budget: Option<u32>,

    /// Recall timeout in ms (replaces OPENPRX_LUCID_RECALL_TIMEOUT_MS)
    #[serde(default = "default_lucid_recall_timeout_ms")]
    pub lucid_recall_timeout_ms: u64,

    /// Store timeout in ms (replaces OPENPRX_LUCID_STORE_TIMEOUT_MS)
    #[serde(default = "default_lucid_store_timeout_ms")]
    pub lucid_store_timeout_ms: u64,

    /// Local hit threshold: minimum number of local results before skipping
    /// remote Lucid recall. (replaces OPENPRX_LUCID_LOCAL_HIT_THRESHOLD)
    #[serde(default = "default_lucid_local_hit_threshold")]
    pub lucid_local_hit_threshold: usize,

    /// Failure cooldown in ms (replaces OPENPRX_LUCID_FAILURE_COOLDOWN_MS)
    #[serde(default = "default_lucid_failure_cooldown_ms")]
    pub lucid_failure_cooldown_ms: u64,
}

/// Raw message event recording configuration (`[memory.events]`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MemoryEventsConfig {
    /// Record inbound/user messages.
    #[serde(default = "default_true")]
    pub record_user_messages: bool,
    /// Record assistant replies.
    #[serde(default = "default_true")]
    pub record_assistant_messages: bool,
    /// Record tool/event payloads by default.
    #[serde(default)]
    pub record_tool_events: bool,
}

/// Semantic memory promotion configuration (`[memory.semantic]`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MemorySemanticConfig {
    /// Auto-promote eligible user messages into long-term semantic memories.
    #[serde(default = "default_true")]
    pub auto_promote_user_messages: bool,
    /// Auto-promote eligible assistant messages. Disabled by default to reduce noise.
    #[serde(default)]
    pub auto_promote_assistant_messages: bool,
    /// Minimum content length for semantic auto-promotion.
    #[serde(default = "default_semantic_auto_promote_min_chars")]
    pub min_chars: usize,
}

pub(super) fn default_embedding_provider() -> String {
    "local".into()
}

/// The memory backend a `[memory]` section gets when it does not name one.
pub(super) fn default_memory_backend() -> String {
    "sqlite".into()
}

/// Whether eligible user messages are promoted into long-term semantic
/// memory when `[memory]` does not say.
pub(super) const fn default_memory_auto_save() -> bool {
    true
}

pub(super) const fn default_archive_after_days() -> u32 {
    7
}
pub(super) const fn default_purge_after_days() -> u32 {
    30
}
pub(super) const fn default_conversation_retention_days() -> u32 {
    3
}
pub(super) const fn default_daily_retention_days() -> u32 {
    7
}
pub(super) fn default_embedding_model() -> String {
    "prx-local-hash-v1".into()
}
pub(super) const fn default_embedding_dims() -> usize {
    384
}
pub(super) const fn default_vector_weight() -> f64 {
    0.7
}
pub(super) const fn default_keyword_weight() -> f64 {
    0.3
}
pub(super) const fn default_min_relevance_score() -> f64 {
    0.4
}
pub(super) const fn default_cache_size() -> usize {
    10_000
}
/// Default ceiling on the embedded memories one vector recall scores.
///
/// Well above any realistic per-turn working set; the bound exists to stop a
/// very large store from making every turn slower, not to trim ordinary recall.
pub(super) const fn default_vector_candidate_cap() -> usize {
    2_000
}
pub(super) const fn default_semantic_auto_promote_min_chars() -> usize {
    30
}
pub(super) const fn default_lucid_recall_timeout_ms() -> u64 {
    5000
}
pub(super) const fn default_lucid_store_timeout_ms() -> u64 {
    3000
}
pub(super) const fn default_lucid_local_hit_threshold() -> usize {
    3
}
pub(super) const fn default_lucid_failure_cooldown_ms() -> u64 {
    60_000
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            backend: default_memory_backend(),
            auto_save: default_memory_auto_save(),
            events: MemoryEventsConfig::default(),
            semantic: MemorySemanticConfig::default(),
            acl_enabled: false,
            legacy_conversation_turns_visible: true,
            archive_after_days: default_archive_after_days(),
            purge_after_days: default_purge_after_days(),
            conversation_retention_days: default_conversation_retention_days(),
            daily_retention_days: default_daily_retention_days(),
            embedding_provider: default_embedding_provider(),
            embedding_provider_explicit: false,
            embedding_model: default_embedding_model(),
            embedding_dimensions: default_embedding_dims(),
            vector_weight: default_vector_weight(),
            keyword_weight: default_keyword_weight(),
            min_relevance_score: default_min_relevance_score(),
            embedding_cache_size: default_cache_size(),
            vector_candidate_cap: default_vector_candidate_cap(),
            auto_hydrate: true,
            sqlite_open_timeout_secs: None,
            sqlite_read_pool_size: None,
            lucid_cmd: None,
            lucid_budget: None,
            lucid_recall_timeout_ms: default_lucid_recall_timeout_ms(),
            lucid_store_timeout_ms: default_lucid_store_timeout_ms(),
            lucid_local_hit_threshold: default_lucid_local_hit_threshold(),
            lucid_failure_cooldown_ms: default_lucid_failure_cooldown_ms(),
        }
    }
}

impl Default for MemoryEventsConfig {
    fn default() -> Self {
        Self {
            record_user_messages: true,
            record_assistant_messages: true,
            record_tool_events: false,
        }
    }
}

impl Default for MemorySemanticConfig {
    fn default() -> Self {
        Self {
            auto_promote_user_messages: true,
            auto_promote_assistant_messages: false,
            min_chars: default_semantic_auto_promote_min_chars(),
        }
    }
}

impl MemoryConfig {
    /// Whether user content should be promoted from the event stream into
    /// durable semantic memory. This deliberately does not control event logs.
    pub fn should_auto_promote_user_message(&self, content: &str) -> bool {
        self.auto_save
            && self.semantic.auto_promote_user_messages
            && crate::memory::should_autosave_content_with_min(content, self.semantic.min_chars)
    }

    /// Whether assistant content should be promoted into durable semantic memory.
    pub fn should_auto_promote_assistant_message(&self, content: &str) -> bool {
        self.auto_save
            && self.semantic.auto_promote_assistant_messages
            && crate::memory::should_autosave_content_with_min(content, self.semantic.min_chars)
    }

    /// Whether normalized user message events should be recorded.
    pub const fn should_record_user_message_event(&self) -> bool {
        self.events.record_user_messages
    }

    /// Whether normalized assistant message events should be recorded.
    pub const fn should_record_assistant_message_event(&self) -> bool {
        self.events.record_assistant_messages
    }

    pub const fn event_recording_config(&self) -> crate::memory::MemoryEventRecording {
        crate::memory::MemoryEventRecording {
            enabled: true,
            record_user_messages: self.events.record_user_messages,
            record_assistant_messages: self.events.record_assistant_messages,
            record_tool_events: self.events.record_tool_events,
        }
    }

    /// Known memory backend identifiers accepted by the runtime.
    const KNOWN_BACKENDS: &'static [&'static str] = &["sqlite", "lucid", "postgres", "markdown", "none"];

    /// Validates the `[memory]` subtree (FIX-P2-09).
    ///
    /// Checks backend selection, embedding provider/model/dimension coherence,
    /// hybrid-scoring weights, and the recall relevance threshold. This is
    /// invoked from [`Config::validate`]; previously the memory subtree was not
    /// validated at all.
    pub fn validate(&self) -> Result<()> {
        // 1. Backend must be a recognized identifier.
        let backend = self.backend.trim().to_ascii_lowercase();
        if !Self::KNOWN_BACKENDS.contains(&backend.as_str()) {
            anyhow::bail!(
                "memory.backend '{}' is not a known backend (expected one of: {})",
                self.backend,
                Self::KNOWN_BACKENDS.join(", ")
            );
        }

        // 2. Embedding provider: "local" | "none" | "openai" | "custom:<url>".
        //    Empty is invalid (use "none" to disable embeddings explicitly).
        let provider = self.embedding_provider.trim();
        if provider.is_empty() {
            anyhow::bail!("memory.embedding_provider must not be empty (use \"none\" to disable)");
        }
        if let Some(url) = provider.strip_prefix("custom:") {
            // A custom embedding endpoint must be a well-formed http(s) URL.
            let url = url.trim();
            let parsed = reqwest::Url::parse(url)
                .map_err(|e| anyhow::anyhow!("memory.embedding_provider custom endpoint is not a valid URL: {e}"))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                anyhow::bail!(
                    "memory.embedding_provider custom endpoint must use http/https, got '{}'",
                    parsed.scheme()
                );
            }
        }

        // 3. Embeddings, when enabled (provider != "none"), need a non-zero
        //    dimension and a non-empty model identifier.
        let embeddings_enabled = !provider.eq_ignore_ascii_case("none");
        if embeddings_enabled {
            if self.embedding_dimensions == 0 {
                anyhow::bail!("memory.embedding_dimensions must be greater than zero when embeddings are enabled");
            }
            if self.embedding_model.trim().is_empty() {
                anyhow::bail!("memory.embedding_model must not be empty when embeddings are enabled");
            }
        }
        // An absurdly large dimension is almost certainly a misconfiguration.
        if self.embedding_dimensions > 65536 {
            anyhow::bail!("memory.embedding_dimensions exceeds maximum (65536)");
        }

        // 4. Hybrid scoring weights must be non-negative and finite.
        for (name, value) in [
            ("memory.vector_weight", self.vector_weight),
            ("memory.keyword_weight", self.keyword_weight),
        ] {
            if !value.is_finite() || value < 0.0 {
                anyhow::bail!("{name} must be a finite, non-negative number, got {value}");
            }
        }
        if self.vector_weight == 0.0 && self.keyword_weight == 0.0 {
            anyhow::bail!("memory.vector_weight and memory.keyword_weight must not both be zero");
        }

        // 5. Minimum relevance score is a similarity threshold in [0.0, 1.0].
        if !(0.0..=1.0).contains(&self.min_relevance_score) {
            anyhow::bail!(
                "memory.min_relevance_score must be in [0.0, 1.0], got {}",
                self.min_relevance_score
            );
        }

        Ok(())
    }
}

/// Startup identity binding entry (`[[identity_bindings]]`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IdentityBindingConfig {
    pub user_id: String,
    pub channel: String,
    pub channel_account: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

/// Startup user policy entry (`[[user_policies]]`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UserPolicyConfig {
    pub user_id: String,
    #[serde(default = "default_user_policy_role")]
    pub role: String,
    #[serde(default)]
    pub projects: Vec<String>,
    #[serde(default = "default_user_policy_visibility_ceiling")]
    pub visibility_ceiling: String,
    #[serde(default)]
    pub blocked_patterns: Vec<String>,
}

pub(super) fn default_user_policy_role() -> String {
    "guest".into()
}

pub(super) fn default_user_policy_visibility_ceiling() -> String {
    "private".into()
}

use super::{Deserialize, JsonSchema, Serialize, default_true, recursive_spawn};

/// Agent orchestration configuration (`[agent]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentConfig {
    /// When true: bootstrap_max_chars=6000. Use for 13B or smaller models.
    #[serde(default)]
    pub compact_context: bool,
    /// Max concurrent read-only tool calls in an iteration batch.
    /// Defaults to 2 to preserve existing scheduler behavior.
    #[serde(default = "default_read_only_tool_concurrency_window")]
    pub read_only_tool_concurrency_window: usize,
    /// Enables priority scheduling so foreground tool calls run before background batches.
    /// Defaults to false to preserve existing scheduler behavior.
    #[serde(default)]
    pub priority_scheduling_enabled: bool,
    /// Tool names treated as low-priority when `priority_scheduling_enabled = true`.
    #[serde(default = "default_agent_low_priority_tools")]
    pub low_priority_tools: Vec<String>,
    /// Context compaction controls (`[agent.compaction]`).
    #[serde(default)]
    pub compaction: AgentCompactionConfig,
}

/// Compaction mode for long conversation contexts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AgentCompactionMode {
    /// Disable proactive compaction.
    Off,
    /// Replace cold context with an exact transcript reference and keep the
    /// durable transcript unchanged.
    #[default]
    Switch,
    /// Conservative compaction that prefers summary replacement.
    Safeguard,
    /// Aggressive truncation mode for tighter contexts.
    Aggressive,
}

/// Agent context compaction configuration (`[agent.compaction]`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentCompactionConfig {
    /// Context rollover strategy (`off`, `switch`, `safeguard`, `aggressive`).
    #[serde(default)]
    pub mode: AgentCompactionMode,
    /// Tokens reserved for the next model response.
    #[serde(default = "default_agent_compaction_reserve_tokens")]
    pub reserve_tokens: usize,
    /// Number of recent non-system messages to keep after compaction.
    #[serde(default = "default_agent_compaction_keep_recent_messages")]
    pub keep_recent_messages: usize,
    /// Run memory flush extraction before legacy summary/truncation modes.
    /// Exact `switch` handoffs never invoke a summarizer.
    #[serde(default = "default_true")]
    pub memory_flush: bool,
    /// Total model context window in tokens. This uses the same unit as
    /// `RouterModelConfig.max_context`; runtime trigger thresholds subtract
    /// `reserve_tokens` from this total window.
    #[serde(default = "default_agent_compaction_max_context_tokens")]
    pub max_context_tokens: usize,
    /// Internal presence bit populated by `Config::load_from_path`. Serde
    /// defaults alone cannot distinguish a missing value from an explicit
    /// operator override equal to the default.
    #[serde(default, skip_serializing)]
    #[schemars(skip)]
    pub max_context_tokens_explicit: bool,
    /// Letta/MemGPT-style OS-paging controls (`[agent.compaction.os_paging]`).
    ///
    /// Enabled by default (Phase 1). When enabled, oldest messages are evicted
    /// to the durable document store (non-destructively, preserving full
    /// content) and semantically recalled on later turns, instead of being
    /// collapsed into a lossy summary.
    #[serde(default)]
    pub os_paging: OsPagingConfig,
}

impl Default for AgentCompactionConfig {
    fn default() -> Self {
        Self {
            mode: AgentCompactionMode::Switch,
            reserve_tokens: default_agent_compaction_reserve_tokens(),
            keep_recent_messages: default_agent_compaction_keep_recent_messages(),
            memory_flush: true,
            max_context_tokens: default_agent_compaction_max_context_tokens(),
            max_context_tokens_explicit: false,
            os_paging: OsPagingConfig::default(),
        }
    }
}

/// Where recalled context pages are injected back into the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalInjectionRole {
    /// Inject recalled pages as a leading `system` message (not user-visible).
    #[default]
    System,
    /// Inject recalled pages as a leading `user` message.
    User,
}

/// Letta/MemGPT-style OS-paging configuration (`[agent.compaction.os_paging]`).
///
/// OS-paging is an additive layer for the legacy compaction modes. The exact
/// `switch` mode bypasses it so paging cannot evict history before a durable
/// handoff is created. Configuration controls paging thresholds and recall.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OsPagingConfig {
    /// Token capacity ratio (0.0-1.0, relative to the input window after
    /// subtracting reserved response tokens) above
    /// which eviction is triggered. Letta recommends ~0.70; the legacy
    /// compaction path uses 0.85.
    #[serde(default = "default_os_paging_eviction_threshold")]
    pub eviction_threshold: f64,
    /// Whether to proactively recall relevant evicted pages on every turn.
    /// When `false`, recall happens only after a context-overflow error.
    #[serde(default = "default_true")]
    pub proactive_retrieval: bool,
    /// Maximum number of evicted pages (document chunks) recalled per turn.
    #[serde(default = "default_os_paging_max_recalled_pages")]
    pub max_recalled_pages: usize,
    /// Role under which recalled pages are injected.
    #[serde(default)]
    pub retrieval_injection_role: RetrievalInjectionRole,
    /// Number of recent non-system messages always retained in the hot window
    /// during eviction (older messages are paged out).
    #[serde(default = "default_os_paging_keep_recent_messages")]
    pub keep_recent_messages: usize,
}

impl Default for OsPagingConfig {
    fn default() -> Self {
        Self {
            eviction_threshold: default_os_paging_eviction_threshold(),
            proactive_retrieval: true,
            max_recalled_pages: default_os_paging_max_recalled_pages(),
            retrieval_injection_role: RetrievalInjectionRole::System,
            keep_recent_messages: default_os_paging_keep_recent_messages(),
        }
    }
}

pub(super) const fn default_os_paging_eviction_threshold() -> f64 {
    0.70
}

pub(super) const fn default_os_paging_max_recalled_pages() -> usize {
    // Behavior-limits Phase 1: raised 3 -> 10 to recall more paged-out history
    // per turn now that os_paging is on by default.
    10
}

pub(super) const fn default_os_paging_keep_recent_messages() -> usize {
    12
}

/// Sessions spawn configuration (`[sessions_spawn]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionsSpawnConfig {
    /// Permit an explicitly requested, sealed recursive child grant.
    #[serde(default)]
    pub allow_recursive_delegation: bool,
    /// Maximum additional delegation levels granted to an opted-in child.
    #[serde(default = "recursive_spawn::default_depth")]
    pub max_recursive_depth: u8,
    /// Default execution mode for sessions_spawn (`task` or `process`).
    #[serde(default = "default_sessions_spawn_mode")]
    pub default_mode: String,
    /// Memory strategy for process-mode workers.
    ///
    /// Supported values:
    /// - `shared_fabric`: worker uses the parent workspace memory DB.
    /// - `isolated_private`: worker uses its private DB.
    #[serde(default = "default_sessions_spawn_process_memory_strategy")]
    pub process_memory_strategy: String,
    /// Optional root directory for process-mode worker workspaces.
    ///
    /// When unset, falls back to `<workspace>/workers`.
    #[serde(default)]
    pub worker_workspace_root: Option<String>,
    /// Remove worker workspace directory after process-mode completion.
    #[serde(default = "default_sessions_spawn_cleanup_on_complete")]
    pub cleanup_on_complete: bool,
}

pub const HYBRID_PROCESS_MEMORY_UNAVAILABLE: &str = "sessions_spawn.process_memory_strategy='hybrid' is unavailable: the production merge consumer and merge/reject/ack/cleanup protocol do not exist; use 'shared_fabric' or 'isolated_private'";

impl SessionsSpawnConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        recursive_spawn::validate(self)?;
        match self.process_memory_strategy.trim() {
            "" | "shared_fabric" | "isolated_private" => Ok(()),
            "hybrid" => anyhow::bail!(HYBRID_PROCESS_MEMORY_UNAVAILABLE),
            other => anyhow::bail!(
                "Invalid sessions_spawn.process_memory_strategy '{other}'. Expected 'shared_fabric' or 'isolated_private'."
            ),
        }
    }
}

pub(super) fn default_sessions_spawn_mode() -> String {
    "task".to_string()
}

pub(super) fn default_sessions_spawn_process_memory_strategy() -> String {
    "shared_fabric".to_string()
}

pub(super) const fn default_sessions_spawn_cleanup_on_complete() -> bool {
    true
}

/// Self-system experimental automation config (`[self_system]`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SelfSystemConfig {
    /// Legacy fitness report interval in hours.
    ///
    /// Kept for configuration compatibility. Calendar scheduling is controlled
    /// by `[self_system.fitness]` and this value is no longer used by the
    /// daemon worker.
    #[serde(default = "default_self_system_fitness_interval_hours")]
    pub fitness_interval_hours: u64,
    /// Daily fitness snapshot policy.
    #[serde(default)]
    pub fitness: FitnessConfig,
    /// Evolution scheduler interval in hours (daemon mode).
    #[serde(default = "default_self_system_evolution_interval_hours")]
    pub evolution_interval_hours: u32,

    /// Path to evolution config file (replaces OPENPRX_EVOLUTION_CONFIG / ZEROCLAW_EVOLUTION_CONFIG)
    #[serde(default)]
    pub evolution_config_path: Option<String>,

    /// Enable debug raw output for evolution (replaces OPENPRX_EVOLUTION_DEBUG_RAW / ZEROCLAW_EVOLUTION_DEBUG_RAW)
    #[serde(default)]
    pub evolution_debug_raw: bool,

    /// Seconds one evolution judge call may take (1 to 3600, default 180).
    /// A judge that has not answered by then gives no verdict, so an applied
    /// change is rolled back.
    #[serde(default = "default_self_system_evolution_judge_timeout_secs")]
    pub evolution_judge_timeout_secs: u64,
}

/// Calendar schedule, evidence thresholds, and retention for fitness v2.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FitnessConfig {
    /// Run the daily fitness worker in daemon mode.
    #[serde(default = "default_true")]
    pub auto_run: bool,
    /// IANA timezone used to define closed calendar days.
    #[serde(default = "default_fitness_timezone")]
    pub timezone: String,
    /// Local wall-clock time (`HH:MM`) at which the previous day is scored.
    #[serde(default = "default_fitness_run_at")]
    pub run_at: String,
    /// Number of daily JSON reports retained on disk.
    #[serde(default = "default_fitness_retention_days")]
    pub retention_days: u32,
    /// Maximum number of closed days recovered after downtime.
    #[serde(default = "default_fitness_max_backfill_days")]
    pub max_backfill_days: u32,
    /// Minimum samples required before an evidence-derived subscore is usable.
    #[serde(default = "default_fitness_min_samples")]
    pub min_samples: usize,
    /// Minimum available metric weight required to publish a final score.
    #[serde(default = "default_fitness_min_coverage")]
    pub min_coverage: f64,
}

pub(super) fn default_fitness_timezone() -> String {
    "UTC".to_string()
}

pub(super) fn default_fitness_run_at() -> String {
    "00:10".to_string()
}

pub(super) const fn default_fitness_retention_days() -> u32 {
    180
}

pub(super) const fn default_fitness_max_backfill_days() -> u32 {
    7
}

pub(super) const fn default_fitness_min_samples() -> usize {
    5
}

pub(super) const fn default_fitness_min_coverage() -> f64 {
    0.60
}

impl Default for FitnessConfig {
    fn default() -> Self {
        Self {
            auto_run: true,
            timezone: default_fitness_timezone(),
            run_at: default_fitness_run_at(),
            retention_days: default_fitness_retention_days(),
            max_backfill_days: default_fitness_max_backfill_days(),
            min_samples: default_fitness_min_samples(),
            min_coverage: default_fitness_min_coverage(),
        }
    }
}

pub(super) const fn default_self_system_fitness_interval_hours() -> u64 {
    24
}

pub(super) const fn default_self_system_evolution_interval_hours() -> u32 {
    24
}

pub(super) const fn default_self_system_evolution_judge_timeout_secs() -> u64 {
    180
}

/// Upper bound on `self_system.evolution_judge_timeout_secs`: one hour.
pub(super) const MAX_EVOLUTION_JUDGE_TIMEOUT_SECS: u64 = 3600;

impl Default for SelfSystemConfig {
    fn default() -> Self {
        Self {
            fitness_interval_hours: default_self_system_fitness_interval_hours(),
            fitness: FitnessConfig::default(),
            evolution_interval_hours: default_self_system_evolution_interval_hours(),
            evolution_config_path: None,
            evolution_debug_raw: false,
            evolution_judge_timeout_secs: default_self_system_evolution_judge_timeout_secs(),
        }
    }
}

pub(super) const fn default_read_only_tool_concurrency_window() -> usize {
    2
}

pub(super) fn default_agent_low_priority_tools() -> Vec<String> {
    // NOTE: `cron` is intentionally NOT in this list. Low-priority classification
    // is matched by tool NAME only (it cannot see the `action` argument), so
    // listing the whole `cron` tool would wrongly demote every action
    // (add/remove/update/pause/resume), not just the previously-demoted forced
    // `run`. Until the classifier can match `cron`+`action=run`, omit it entirely.
    vec!["sessions_spawn", "delegate"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

pub(super) const fn default_agent_compaction_reserve_tokens() -> usize {
    4096
}

pub(super) const fn default_agent_compaction_keep_recent_messages() -> usize {
    12
}

pub(super) const fn default_agent_compaction_max_context_tokens() -> usize {
    128_000
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            compact_context: false,
            read_only_tool_concurrency_window: default_read_only_tool_concurrency_window(),
            priority_scheduling_enabled: false,
            low_priority_tools: default_agent_low_priority_tools(),
            compaction: AgentCompactionConfig::default(),
        }
    }
}

/// Skills loading configuration (`[skills]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillsConfig {
    /// Optional path to a local open-skills repository.
    /// If unset, defaults to `$HOME/open-skills` when enabled.
    #[serde(default)]
    pub open_skills_dir: Option<String>,
    /// Optional path to a local openclaw-skills clone directory.
    /// If unset, defaults to `$HOME/.openprx/openclaw-skills/` when enabled.
    #[serde(default)]
    pub openclaw_skills_dir: Option<String>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            open_skills_dir: None,
            openclaw_skills_dir: None,
        }
    }
}

/// Dynamic skill retrieval configuration (`[skill_rag]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillRagConfig {
    /// How many skills the system prompt may advertise.
    ///
    /// The section lists metadata only (instruction bodies stay behind
    /// `skill_read`), so a catalog at or below this size is published whole and
    /// the section becomes a pure function of the installed skills — which is
    /// what keeps the provider's cacheable prefix identical across turns.
    /// Larger catalogs fall back to relevance retrieval, which does move with
    /// the wording; entry points that hold a session narrow that churn with a
    /// session-scoped union (`SessionSkillExposure`).
    #[serde(default = "default_skill_rag_top_k")]
    pub top_k: usize,
}

pub(super) const fn default_skill_rag_top_k() -> usize {
    32
}

impl Default for SkillRagConfig {
    fn default() -> Self {
        Self {
            top_k: default_skill_rag_top_k(),
        }
    }
}

impl SkillRagConfig {
    pub const fn available(&self) -> bool {
        true
    }
}

/// Multimodal (image) handling configuration (`[multimodal]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MultimodalConfig {
    /// Maximum number of image attachments accepted per request.
    #[serde(default = "default_multimodal_max_images")]
    pub max_images: usize,
    /// Maximum image payload size in MiB before base64 encoding.
    #[serde(default = "default_multimodal_max_image_size_mb")]
    pub max_image_size_mb: usize,
    /// Allow fetching remote image URLs (http/https). Disabled by default.
    #[serde(default)]
    pub allow_remote_fetch: bool,
}

pub(super) const fn default_multimodal_max_images() -> usize {
    4
}

pub(super) const fn default_multimodal_max_image_size_mb() -> usize {
    5
}

impl MultimodalConfig {
    /// Clamp configured values to safe runtime bounds.
    pub fn effective_limits(&self) -> (usize, usize) {
        let max_images = self.max_images.clamp(1, 16);
        let max_image_size_mb = self.max_image_size_mb.clamp(1, 20);
        (max_images, max_image_size_mb)
    }
}

impl Default for MultimodalConfig {
    fn default() -> Self {
        Self {
            max_images: default_multimodal_max_images(),
            max_image_size_mb: default_multimodal_max_image_size_mb(),
            allow_remote_fetch: false,
        }
    }
}

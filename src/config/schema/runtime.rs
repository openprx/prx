use super::{Deserialize, JsonSchema, Serialize, default_true};

// ── Runtime ──────────────────────────────────────────────────────

/// Runtime adapter configuration (`[runtime]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeConfig {
    /// Runtime kind (`native` | `docker`).
    #[serde(default = "default_runtime_kind")]
    pub kind: String,

    /// Docker runtime settings (used when `kind = "docker"`).
    #[serde(default)]
    pub docker: DockerRuntimeConfig,

    /// Global reasoning override for providers that expose explicit controls.
    /// - `None`: provider default behavior
    /// - `Some(true)`: request reasoning/thinking when supported
    /// - `Some(false)`: disable reasoning/thinking when supported
    #[serde(default)]
    pub reasoning_enabled: Option<bool>,

    /// Codex stream idle timeout in seconds (replaces ZEROCLAW_CODEX_STREAM_IDLE_TIMEOUT_SECS)
    #[serde(default)]
    pub codex_stream_idle_timeout_secs: Option<u64>,

    /// Codex reasoning effort (replaces ZEROCLAW_CODEX_REASONING_EFFORT)
    #[serde(default)]
    pub codex_reasoning_effort: Option<String>,

    /// Hard cap on the tokio blocking-thread pool.
    ///
    /// PRX places no ceiling on concurrent turns, sub-agents or sessions, which
    /// makes this pool the last implicit concurrency gate in the process — and
    /// it fails by deadlock, not by rejection: once every slot is held, further
    /// blocking work queues forever with no timeout. Tokio's built-in default
    /// is a hardware-agnostic 512; leaving this unset instead derives the cap
    /// from `available_parallelism()` (cgroup-aware) with a 2048 floor. Raise
    /// it when `[runtime] blocking pool is saturated` warnings appear.
    ///
    /// Read during process bootstrap, before the runtime exists, so it takes
    /// effect only on restart — config hot-reload cannot resize a live pool.
    #[serde(default)]
    pub max_blocking_threads: Option<usize>,

    /// Seconds after which a still-running work item is reported in the log.
    ///
    /// **Notification only — nothing is ever terminated because of it.** PRX
    /// treats long tasks as normal business: a research run or a build can
    /// legitimately take hours, and ending one on a timer is a worse outcome
    /// than letting it finish. This threshold exists so that a task which is
    /// genuinely wedged becomes *visible*, leaving the decision to an operator
    /// (`prx tasks list` / `prx tasks kill`).
    ///
    /// Leave unset for the 900s default; set `0` to silence the warning
    /// entirely. Enabled values must be at least 10 seconds, below which the
    /// sweeper would warn about ordinary tool calls.
    #[serde(default)]
    pub long_task_warn_secs: Option<u64>,

    /// Seconds an agent turn may produce **no observable progress at all**
    /// before it is judged hung and terminated.
    ///
    /// **This is not a turn timeout.** The window is reset by every sign of
    /// life — a provider stream chunk, a tool call starting or finishing,
    /// history compaction, output written to a channel — so a turn that keeps
    /// working runs for as long as it needs to, however long that is. Only
    /// total silence for the whole window counts, which is a stall rather than
    /// a slow task.
    ///
    /// Contrast [`RuntimeConfig::long_task_warn_secs`], which measures elapsed
    /// run time, is *not* refreshed by progress, and never terminates anything.
    /// The two must not be conflated or merged: one reports that work is long,
    /// this one recovers from work that is wedged.
    ///
    /// Leave unset for the 1800s default; set `0` to disable hang detection
    /// entirely. Enabled values must be at least 30 seconds.
    #[serde(default)]
    pub idle_hang_secs: Option<u64>,
}

/// Docker runtime configuration (`[runtime.docker]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DockerRuntimeConfig {
    /// Runtime image used to execute shell commands.
    #[serde(default = "default_docker_image")]
    pub image: String,

    /// Docker network mode (`none`, `bridge`, etc.).
    #[serde(default = "default_docker_network")]
    pub network: String,

    /// Optional memory limit in MB (`None` = no explicit limit).
    #[serde(default = "default_docker_memory_limit_mb")]
    pub memory_limit_mb: Option<u64>,

    /// Optional CPU limit (`None` = no explicit limit).
    #[serde(default = "default_docker_cpu_limit")]
    pub cpu_limit: Option<f64>,

    /// Mount root filesystem as read-only.
    #[serde(default = "default_true")]
    pub read_only_rootfs: bool,

    /// Mount configured workspace into `/workspace`.
    #[serde(default = "default_true")]
    pub mount_workspace: bool,

    /// Optional workspace root allowlist for Docker mount validation.
    #[serde(default)]
    pub allowed_workspace_roots: Vec<String>,
}

pub(super) fn default_runtime_kind() -> String {
    "native".into()
}

pub(super) fn default_docker_image() -> String {
    "alpine:3.20".into()
}

pub(super) fn default_docker_network() -> String {
    "none".into()
}

pub(super) const fn default_docker_memory_limit_mb() -> Option<u64> {
    Some(512)
}

pub(super) const fn default_docker_cpu_limit() -> Option<f64> {
    Some(1.0)
}

impl Default for DockerRuntimeConfig {
    fn default() -> Self {
        Self {
            image: default_docker_image(),
            network: default_docker_network(),
            memory_limit_mb: default_docker_memory_limit_mb(),
            cpu_limit: default_docker_cpu_limit(),
            read_only_rootfs: true,
            mount_workspace: true,
            allowed_workspace_roots: Vec::new(),
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            kind: default_runtime_kind(),
            docker: DockerRuntimeConfig::default(),
            reasoning_enabled: None,
            codex_stream_idle_timeout_secs: None,
            codex_reasoning_effort: None,
            max_blocking_threads: None,
            long_task_warn_secs: None,
            idle_hang_secs: None,
        }
    }
}

// ── Reliability / supervision ────────────────────────────────────

/// Reliability and supervision configuration (`[reliability]` section).
///
/// Controls provider retries, fallback chains, API key rotation, and channel restart backoff.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReliabilityConfig {
    /// Retries per provider before failing over.
    #[serde(default = "default_provider_retries")]
    pub provider_retries: u32,
    /// Base backoff (ms) for provider retry delay.
    #[serde(default = "default_provider_backoff_ms")]
    pub provider_backoff_ms: u64,
    /// Fallback provider chain (e.g. `["anthropic", "openai"]`).
    #[serde(default)]
    pub fallback_providers: Vec<String>,
    /// Operator acknowledgement that no provider failover is configured.
    /// This suppresses doctor noise only; it does not add a fallback.
    #[serde(default)]
    pub acknowledge_single_provider_risk: bool,
    /// Additional API keys for round-robin rotation on rate-limit (429) errors.
    /// The primary `api_key` is always tried first; these are extras.
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// Per-model fallback chains. When a model fails, try these alternatives in order.
    /// Example: `{ "claude-opus-4-20250514" = ["claude-sonnet-4-20250514", "gpt-4o"] }`
    #[serde(default)]
    pub model_fallbacks: std::collections::HashMap<String, Vec<String>>,
    /// Initial backoff for channel/daemon restarts.
    #[serde(default = "default_channel_backoff_secs")]
    pub channel_initial_backoff_secs: u64,
    /// Max backoff for channel/daemon restarts.
    #[serde(default = "default_channel_backoff_max_secs")]
    pub channel_max_backoff_secs: u64,
    /// Scheduler polling cadence in seconds.
    #[serde(default = "default_scheduler_poll_secs")]
    pub scheduler_poll_secs: u64,
    /// Max retries for cron shell attempts and Agent startup before decision admission.
    /// Admitted Agent turns are not replanned automatically; provider request retries
    /// remain controlled by `provider_retries`.
    #[serde(default = "default_scheduler_retries")]
    pub scheduler_retries: u32,
}

pub(super) const fn default_provider_retries() -> u32 {
    2
}

pub(super) const fn default_provider_backoff_ms() -> u64 {
    500
}

pub(super) const fn default_channel_backoff_secs() -> u64 {
    2
}

pub(super) const fn default_channel_backoff_max_secs() -> u64 {
    60
}

pub(super) const fn default_scheduler_poll_secs() -> u64 {
    15
}

pub(super) const fn default_scheduler_retries() -> u32 {
    2
}

impl Default for ReliabilityConfig {
    fn default() -> Self {
        Self {
            provider_retries: default_provider_retries(),
            provider_backoff_ms: default_provider_backoff_ms(),
            fallback_providers: Vec::new(),
            acknowledge_single_provider_risk: false,
            api_keys: Vec::new(),
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: default_channel_backoff_secs(),
            channel_max_backoff_secs: default_channel_backoff_max_secs(),
            scheduler_poll_secs: default_scheduler_poll_secs(),
            scheduler_retries: default_scheduler_retries(),
        }
    }
}

// ── Scheduler ────────────────────────────────────────────────────

/// Scheduler configuration for periodic task execution (`[scheduler]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SchedulerConfig {
    /// Maximum number of persisted scheduled tasks.
    #[serde(default = "default_scheduler_max_tasks")]
    pub max_tasks: usize,
    /// Execution claim lease. Active jobs renew this lease every one third of
    /// the interval; expired claims may be recovered by another scheduler.
    #[serde(default = "default_scheduler_claim_lease_secs")]
    pub claim_lease_secs: u64,
}

pub(super) const fn default_scheduler_max_tasks() -> usize {
    64
}

pub(super) const fn default_scheduler_claim_lease_secs() -> u64 {
    90
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_tasks: default_scheduler_max_tasks(),
            claim_lease_secs: default_scheduler_claim_lease_secs(),
        }
    }
}

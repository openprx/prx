use super::{Deserialize, JsonSchema, Serialize};

// ── Heartbeat ────────────────────────────────────────────────────

/// Heartbeat configuration for periodic health pings (`[heartbeat]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HeartbeatConfig {
    /// Interval in minutes between heartbeat pings. Default: `30`.
    pub interval_minutes: u32,
    /// Inclusive active hour window `[start_hour, end_hour]` in local time.
    /// Heartbeat ticks outside this range are skipped.
    #[serde(default = "default_heartbeat_active_hours")]
    pub active_hours: Vec<u8>,
    /// Prompt used by heartbeat worker when dispatching tasks.
    #[serde(default = "default_heartbeat_prompt")]
    pub prompt: String,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval_minutes: 30,
            active_hours: default_heartbeat_active_hours(),
            prompt: default_heartbeat_prompt(),
        }
    }
}

pub(super) fn default_heartbeat_active_hours() -> Vec<u8> {
    vec![8, 23]
}

pub(super) fn default_heartbeat_prompt() -> String {
    "Check HEARTBEAT.md and follow instructions.".to_string()
}

/// Policy for direct-message handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DmPolicy {
    /// Future pairing handshake flow (placeholder; runtime currently falls back to allowlist).
    Pairing,
    /// Accept only senders listed in `allowed_from`.
    #[default]
    Allowlist,
    /// Accept any direct sender.
    Open,
    /// Ignore direct messages.
    Disabled,
}

/// Policy for group-message handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GroupPolicy {
    /// Accept only groups listed in `group_allow_from`.
    #[default]
    Allowlist,
    /// Accept any group.
    Open,
    /// Ignore group messages.
    Disabled,
}

/// How the bot decides whether to reply in a group/guild conversation.
///
/// This is orthogonal to [`GroupPolicy`] (which decides *which* groups are
/// visible at all): `GroupReplyMode` decides, within a visible group, *whether
/// to speak*. It refines the legacy per-channel `mention_only` boolean.
///
/// Backward compatibility: per-channel config carries
/// `group_reply_mode: Option<GroupReplyMode>`. When `None`, the mode is derived
/// from the existing `mention_only` flag (`true => MentionOnly`,
/// `false => Off`), so existing configs behave exactly as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum GroupReplyMode {
    /// Never reply to group messages unless explicitly @-mentioned at the
    /// channel layer (equivalent to the legacy `mention_only = false` behavior:
    /// non-@ group messages are dropped at the channel layer as today).
    Off,
    /// Reply only when the bot is @-mentioned (legacy `mention_only = true`).
    #[default]
    MentionOnly,
    /// Read every group message and let the model decide whether to reply or
    /// stay silent (via the `stay_silent` tool). Only honored for channels that
    /// support smart group-reply (currently Telegram, Discord, and WhatsApp).
    Smart,
}

impl GroupReplyMode {
    /// Resolve the effective mode from an explicit `group_reply_mode` plus the
    /// legacy `mention_only` flag.
    ///
    /// When `explicit` is `Some`, it wins. When `None`, derive from
    /// `mention_only` so existing configs are byte-for-byte unchanged:
    /// `mention_only = true => MentionOnly`, `mention_only = false => Off`.
    #[must_use]
    pub const fn resolve(explicit: Option<Self>, mention_only: bool) -> Self {
        match explicit {
            Some(mode) => mode,
            None if mention_only => Self::MentionOnly,
            None => Self::Off,
        }
    }

    /// Whether this mode lets the model autonomously decide to reply or stay
    /// silent in groups.
    #[must_use]
    pub const fn is_smart(self) -> bool {
        matches!(self, Self::Smart)
    }

    /// Whether non-@ group messages should be dropped at the channel layer.
    ///
    /// `Off`/`MentionOnly` keep the historical drop-on-no-mention behavior;
    /// `Smart` passes everything through to the central pipeline.
    #[must_use]
    pub const fn drops_unmentioned_at_channel_layer(self) -> bool {
        !self.is_smart()
    }
}

pub(super) const fn default_pre_gate_classifier_timeout_secs() -> u64 {
    5
}

pub(super) const fn default_pre_gate_classifier_temperature() -> f64 {
    0.0
}

pub(super) const fn default_pre_gate_max_context_messages() -> usize {
    3
}

/// Token-saving pre-gate for smart group-reply (`[smart_group]`).
///
/// The pre-gate runs after source decision admission and before the full agent
/// loop for smart-mode group messages that do NOT explicitly @-mention the bot.
/// A 0-token heuristic sends clearly relevant messages straight to the loop;
/// obvious noise and uncertain messages require a yes/no confirmation from a
/// resolver-built, physically accounted classifier before silence is allowed.
///
/// @-mentions, DMs, and non-smart modes NEVER reach the pre-gate. The pre-gate
/// is fail-open: any classifier error/timeout enters the loop (it must never
/// silently drop a message that should have been answered).
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct SmartGroupConfig {
    /// Provider id/alias for the Tier-2 classifier. When empty, the admitted
    /// channel route's provider is resolved for this auxiliary call.
    #[serde(default)]
    pub classifier_provider: String,
    /// Model id for the Tier-2 classifier. When empty, the admitted channel
    /// route's model is resolved. Set this to a cheap-tier model id (e.g. one listed in
    /// `router.automix.cheap_model_tiers`) to minimize cost.
    #[serde(default)]
    pub classifier_model: String,
    /// Per-call timeout (seconds) for the Tier-2 classifier. On timeout the
    /// pre-gate fails open (enters the loop). Default: `5`.
    #[serde(default = "default_pre_gate_classifier_timeout_secs")]
    pub classifier_timeout_secs: u64,
    /// Sampling temperature for the classifier call. Default: `0.0` (the
    /// decision should be deterministic).
    #[serde(default = "default_pre_gate_classifier_temperature")]
    pub classifier_temperature: f64,
    /// How many recent conversation turns to feed the classifier as context.
    /// Default: `3`. Bounded to keep the classifier prompt (and cost) tiny.
    #[serde(default = "default_pre_gate_max_context_messages")]
    pub classifier_max_context_messages: usize,
}

impl Default for SmartGroupConfig {
    fn default() -> Self {
        Self {
            classifier_provider: String::new(),
            classifier_model: String::new(),
            classifier_timeout_secs: default_pre_gate_classifier_timeout_secs(),
            classifier_temperature: default_pre_gate_classifier_temperature(),
            classifier_max_context_messages: default_pre_gate_max_context_messages(),
        }
    }
}

impl SmartGroupConfig {
    /// Validate numeric ranges. Called from [`Config`] validation.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !(0.0..=2.0).contains(&self.classifier_temperature) {
            anyhow::bail!("smart_group.classifier_temperature must be in [0,2]");
        }
        if self.classifier_timeout_secs == 0 {
            anyhow::bail!("smart_group.classifier_timeout_secs must be > 0");
        }
        Ok(())
    }
}

// ── Cron ────────────────────────────────────────────────────────

/// Cron job configuration (`[cron]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CronConfig {
    /// Maximum number of historical cron run records to retain. Default: `50`.
    #[serde(default = "default_max_run_history")]
    pub max_run_history: u32,
}

pub(super) const fn default_max_run_history() -> u32 {
    50
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            max_run_history: default_max_run_history(),
        }
    }
}

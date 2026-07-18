//! Channel subsystem for messaging platform integrations.
//!
//! This module provides the multi-channel messaging infrastructure that connects
//! OpenPRX to external platforms. Each channel implements the [`Channel`] trait
//! defined in [`traits`], which provides a uniform interface for sending messages,
//! listening for incoming messages, health checking, and typing indicators.
//!
//! Channels are instantiated by [`start_channels`] based on the runtime configuration.
//! The subsystem manages per-sender conversation history, concurrent message processing
//! with configurable parallelism, and exponential-backoff reconnection for resilience.
//!
//! # Extension
//!
//! To add a new channel, implement [`Channel`] in a new submodule and wire it into
//! [`start_channels`]. See `AGENTS.md` §7.2 for the full change playbook.

#![allow(clippy::print_stdout, clippy::print_stderr)]

pub mod cli;
pub mod dingtalk;
pub mod discord;
pub mod email_channel;
pub mod imessage;
pub mod irc;
pub mod lark;
pub mod linq;
#[cfg(feature = "channel-matrix")]
pub mod matrix;
pub mod mattermost;
pub mod nextcloud_talk;
pub mod pre_gate;
pub mod qq;
pub mod signal;
pub mod signal_native;
pub mod slack;
pub mod telegram;
pub mod terminal;
pub mod traits;
pub mod wacli;
pub mod whatsapp;
#[cfg(feature = "whatsapp-web")]
pub mod whatsapp_storage;
#[cfg(feature = "whatsapp-web")]
pub mod whatsapp_web;

pub use cli::CliChannel;
pub use dingtalk::DingTalkChannel;
pub use discord::DiscordChannel;
pub use email_channel::EmailChannel;
pub use imessage::IMessageChannel;
pub use irc::IrcChannel;
pub use lark::LarkChannel;
pub use linq::LinqChannel;
#[cfg(feature = "channel-matrix")]
pub use matrix::MatrixChannel;
pub use mattermost::MattermostChannel;
pub use nextcloud_talk::NextcloudTalkChannel;
pub use qq::QQChannel;
pub use signal::SignalChannel;
pub use signal_native::SignalNativeChannel;
pub use slack::SlackChannel;
pub use telegram::TelegramChannel;
pub use terminal::TerminalChannel;
pub use traits::{Channel, ChatKind, SendMessage};
pub use wacli::WacliChannel;
pub use whatsapp::WhatsAppChannel;
#[cfg(feature = "whatsapp-web")]
pub use whatsapp_web::WhatsAppWebChannel;

use crate::agent::loop_::{
    DocumentIngestRuntime, ScopeContext, ToolConcurrencyGovernanceConfig, build_context_with_shared_events_and_scope,
    build_tool_instructions,
};
use crate::config::Config;
use crate::hooks::HookManager;
use crate::identity;
use crate::memory::{
    self, ChatProfile, Memory, MemoryEventRecording, MemoryFabric, MemoryPrincipal, MemoryVisibility,
    MemoryWriteContext,
};
use crate::observability::{self, Observer};
use crate::providers::{self, ChatMessage, ChatRequest, Provider};
use crate::runtime;
use crate::runtime::envelope::RuntimeEnvelope;
#[cfg(test)]
use crate::security::SideEffectGate;
use crate::security::inbound_gate::InboundGate;
#[cfg(test)]
use crate::security::policy::ResourceRiskLevel;
use crate::tools::{self, Tool};
use crate::util::truncate_with_ellipsis;
use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Per-sender conversation history for channel messages.
type ConversationHistoryMap = Arc<Mutex<HashMap<String, Vec<ChatMessage>>>>;
/// Maximum history messages to keep per sender.
/// 🟡 Behavior-limits Phase 1: raised 50 -> 200 (4x).
const MAX_CHANNEL_HISTORY: usize = 200;
/// Maximum number of persisted sessions to hydrate at startup.
const MAX_HYDRATED_SESSIONS: usize = 100;
/// Maximum characters per injected workspace file (matches `OpenClaw` default).
/// 🟡 Behavior-limits Phase 1: raised 20K -> 60K (3x).
const BOOTSTRAP_MAX_CHARS: usize = 60_000;

const DEFAULT_CHANNEL_INITIAL_BACKOFF_SECS: u64 = 2;
const DEFAULT_CHANNEL_MAX_BACKOFF_SECS: u64 = 60;
const MIN_CHANNEL_MESSAGE_TIMEOUT_SECS: u64 = 30;
/// Cap timeout scaling so large max_tool_iterations values do not create unbounded waits.
const CHANNEL_MESSAGE_TIMEOUT_SCALE_CAP: u64 = 4;
const CHANNEL_PARALLELISM_PER_CHANNEL: usize = 4;
const CHANNEL_MIN_IN_FLIGHT_MESSAGES: usize = 8;
const CHANNEL_MAX_IN_FLIGHT_MESSAGES: usize = 64;
const CHANNEL_CIRCUIT_BREAKER_FAILURES: u32 = 5;
const CHANNEL_CIRCUIT_BREAKER_BACKOFF_MULTIPLIER: u32 = 5;
const CHANNEL_TYPING_REFRESH_INTERVAL_SECS: u64 = 4;
const CHANNEL_HEALTH_HEARTBEAT_SECS: u64 = 30;
const MODEL_CACHE_FILE: &str = "models_cache.json";
const MODEL_CACHE_PREVIEW_LIMIT: usize = 10;
/// 🟡 Behavior-limits Phase 1: raised 4 -> 16 (4x).
#[allow(dead_code)]
const MEMORY_CONTEXT_MAX_ENTRIES: usize = 16;
/// 🟡 Behavior-limits Phase 1: raised 800 -> 3200 (4x).
#[allow(dead_code)]
const MEMORY_CONTEXT_ENTRY_MAX_CHARS: usize = 3_200;
/// 🟡 Behavior-limits Phase 1: raised 4000 -> 16000 (4x).
#[allow(dead_code)]
const MEMORY_CONTEXT_MAX_CHARS: usize = 16_000;
const CHANNEL_HISTORY_COMPACT_KEEP_MESSAGES: usize = 8;
/// 🟡 Behavior-limits Phase 1: raised 320 -> 1280 (4x).
const CHANNEL_HISTORY_COMPACT_CONTENT_CHARS: usize = 1_280;
/// 🟡 Behavior-limits Phase 1: raised 2400 -> 9600 (4x).
const CHANNEL_HISTORY_COMPACT_TOTAL_CHARS: usize = 9_600;
const SIGNAL_IMAGE_UNCERTAINTY_FALLBACK: &str = "无法确认，请提供更清晰图片或补充说明";
const SIGNAL_VISION_PREFLIGHT_CONFIDENCE_THRESHOLD: f64 = 0.60;
const SIGNAL_VISION_PREFLIGHT_TIMEOUT_SECS: u64 = 45;

type ProviderCacheMap = Arc<Mutex<HashMap<String, Arc<dyn Provider>>>>;
type RouteSelectionMap = Arc<Mutex<HashMap<String, ChannelRouteSelection>>>;

fn effective_channel_message_timeout_secs(configured: u64) -> u64 {
    configured.max(MIN_CHANNEL_MESSAGE_TIMEOUT_SECS)
}

fn channel_message_timeout_budget_secs(message_timeout_secs: u64, max_tool_iterations: usize) -> u64 {
    let iterations = max_tool_iterations.max(1) as u64;
    let scale = iterations.min(CHANNEL_MESSAGE_TIMEOUT_SCALE_CAP);
    message_timeout_secs.saturating_mul(scale)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChannelRouteSelection {
    provider: String,
    model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChannelRuntimeCommand {
    ShowProviders,
    SetProvider(String),
    ShowModel,
    SetModel(String),
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ModelCacheState {
    entries: Vec<ModelCacheEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ModelCacheEntry {
    provider: String,
    models: Vec<String>,
}

#[derive(Debug, Clone)]
struct ChannelRuntimeDefaults {
    default_provider: String,
    model: String,
    temperature: f64,
    api_key: Option<String>,
    api_url: Option<String>,
    reliability: crate::config::ReliabilityConfig,
}

#[derive(Clone)]
struct ChannelMessageRuntimeSnapshot {
    multimodal: crate::config::MultimodalConfig,
    max_tool_iterations: usize,
    read_only_tool_concurrency_window: usize,
    read_only_tool_timeout_secs: u64,
    priority_scheduling_enabled: bool,
    low_priority_tools: Vec<String>,
    min_relevance_score: f64,
    agent_compaction: crate::config::AgentCompactionConfig,
    tool_tiering: crate::config::ToolTieringConfig,
}

const SYSTEMD_STATUS_ARGS: [&str; 3] = ["--user", "is-active", "prx.service"];
const SYSTEMD_RESTART_ARGS: [&str; 3] = ["--user", "restart", "prx.service"];
const OPENRC_STATUS_ARGS: [&str; 2] = ["prx", "status"];
const OPENRC_RESTART_ARGS: [&str; 2] = ["prx", "restart"];

/// Legacy test fixture for the pre-ConfigGeneration channel security swap.
#[cfg(test)]
#[derive(Clone)]
struct SecurityGen {
    /// Security policy consulted by the channel side-effect gates and scope.
    security: Arc<crate::security::SecurityPolicy>,
}

#[derive(Clone)]
struct ChannelRuntimeContext {
    config: crate::config::SharedConfig,
    #[cfg(test)]
    config_generation: Arc<crate::config::ConfigGeneration>,
    channels_by_name: Arc<HashMap<String, Arc<dyn Channel>>>,
    provider: Arc<dyn Provider>,
    default_provider: Arc<String>,
    memory: Arc<dyn Memory>,
    tools_registry: Arc<Vec<Box<dyn Tool>>>,
    observer: Arc<dyn Observer>,
    hooks: Arc<HookManager>,
    system_prompt: Arc<String>,
    #[cfg(test)]
    model: Arc<String>,
    #[cfg(test)]
    temperature: f64,
    auto_save_memory: bool,
    memory_event_recording: MemoryEventRecording,
    max_tool_iterations: usize,
    #[cfg(test)]
    read_only_tool_concurrency_window: usize,
    #[cfg(test)]
    read_only_tool_timeout_secs: u64,
    #[cfg(test)]
    priority_scheduling_enabled: bool,
    #[cfg(test)]
    low_priority_tools: Vec<String>,
    #[cfg(test)]
    min_relevance_score: f64,
    conversation_histories: ConversationHistoryMap,
    provider_cache: ProviderCacheMap,
    route_overrides: RouteSelectionMap,
    #[cfg(test)]
    api_key: Option<String>,
    #[cfg(test)]
    api_url: Option<String>,
    #[cfg(test)]
    reliability: Arc<crate::config::ReliabilityConfig>,
    provider_runtime_options: providers::ProviderRuntimeOptions,
    workspace_dir: Arc<PathBuf>,
    message_timeout_secs: u64,
    interrupt_on_new_message: bool,
    #[cfg(test)]
    multimodal: crate::config::MultimodalConfig,
    /// Legacy test-only security fixture. Production derives one policy from the
    /// ConfigGeneration pinned at message admission.
    #[cfg(test)]
    security: Arc<arc_swap::ArcSwap<SecurityGen>>,
    #[cfg(test)]
    agent_compaction: crate::config::AgentCompactionConfig,
    #[cfg(test)]
    tool_tiering: crate::config::ToolTieringConfig,
    signal_inbound_policy: Option<InboundPolicyConfig>,
    whatsapp_inbound_policy: Option<InboundPolicyConfig>,
    bot_names: Vec<String>,
    bot_uuids: Vec<String>,
    mention_only_by_channel: HashMap<String, bool>,
    /// Per-channel effective group reply mode (smart group-reply). Populated for
    /// channels that opt into smart mode; absent => derive from mention_only.
    group_reply_mode_by_channel: HashMap<String, crate::config::GroupReplyMode>,
    /// Per-group cooldown tracker for proactive (non-@) smart replies, to avoid
    /// the bot dominating a busy group. Keyed by `channel:group_id`.
    smart_reply_cooldown: Arc<parking_lot::Mutex<HashMap<String, std::time::Instant>>>,
    /// Smart group-reply pre-gate config (heuristics + cheap-tier classifier).
    /// Only consulted on smart-mode group turns where the bot was NOT explicitly
    /// @-mentioned (the token-saving triage that runs before the agent loop).
    smart_group: crate::config::SmartGroupConfig,
    /// Whether the configured provider supports native tool calling. Drives the
    /// non-native-only per-turn `stay_silent` instruction append on the static
    /// (skill-RAG-off) prompt path; native providers advertise it via filtered
    /// tool specs instead.
    native_tools: bool,
    /// Skill RAG context (present when skill_rag.enabled) for per-message skill selection.
    skill_rag_ctx: Option<SkillRagContext>,
    /// Test-only seam: when present, `process_channel_message` routes the inbound
    /// gate through this authorizer instead of the production `for_policy` path,
    /// so tests can express an operation-selective deny (e.g. autosave only) that
    /// the real `SecurityPolicy` cannot. Compiled out entirely in non-test builds
    /// (zero production overhead; `for_policy` is the only path there).
    #[cfg(test)]
    test_inbound_authorizer: Option<Arc<dyn crate::security::inbound_gate::InboundAuthorizer + Send + Sync>>,
}

/// Holds the data needed for per-message skill RAG selection and system prompt rebuild.
#[derive(Clone)]
struct SkillRagContext {
    skills: Arc<Vec<crate::skills::Skill>>,
    embedder: Arc<dyn crate::memory::embeddings::EmbeddingProvider>,
    top_k: usize,
    /// Owned tool descriptions for prompt rebuild.
    tool_descs_owned: Arc<Vec<(String, String)>>,
    identity_config: Option<crate::config::IdentityConfig>,
    bootstrap_max_chars: Option<usize>,
    native_tools: bool,
}

#[derive(Debug, Clone)]
struct InboundPolicyConfig {
    dm_policy: crate::config::DmPolicy,
    group_policy: crate::config::GroupPolicy,
    allowed_from: HashSet<String>,
    group_allow_from: HashSet<String>,
}

#[derive(Clone)]
struct InFlightSenderTaskState {
    task_id: u64,
    cancellation: CancellationToken,
    completion: Arc<InFlightTaskCompletion>,
}

struct InFlightTaskCompletion {
    done: AtomicBool,
    notify: tokio::sync::Notify,
}

impl InFlightTaskCompletion {
    fn new() -> Self {
        Self {
            done: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    fn mark_done(&self) {
        self.done.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn wait(&self) {
        if self.done.load(Ordering::Acquire) {
            return;
        }
        self.notify.notified().await;
    }
}

fn conversation_memory_key(msg: &traits::ChannelMessage) -> String {
    format!("{}_{}_{}", msg.channel, msg.sender, msg.id)
}

fn normalize_allowlist(values: &[String]) -> HashSet<String> {
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn legacy_chat_kind_from_message(msg: &traits::ChannelMessage) -> ChatKind {
    if msg.reply_target.starts_with("group:") || msg.reply_target.ends_with("@g.us") || msg.sender.ends_with("@g.us") {
        ChatKind::Group
    } else {
        ChatKind::Dm
    }
}

fn chat_kind_from_message(msg: &traits::ChannelMessage) -> ChatKind {
    if msg.chat_kind == ChatKind::Dm {
        legacy_chat_kind_from_message(msg)
    } else {
        msg.chat_kind
    }
}

fn infer_chat_type_from_message(msg: &traits::ChannelMessage) -> &'static str {
    chat_kind_from_message(msg).scope_chat_type()
}

fn extract_group_identifier(msg: &traits::ChannelMessage) -> Option<String> {
    if let Some(group_id) = msg.reply_target.strip_prefix("group:") {
        return Some(group_id.to_string());
    }
    if msg.reply_target.ends_with("@g.us") {
        return Some(msg.reply_target.clone());
    }
    if msg.sender.ends_with("@g.us") {
        return Some(msg.sender.clone());
    }
    None
}

fn allowlist_matches(allowlist: &HashSet<String>, value: &str) -> bool {
    allowlist.contains("*") || allowlist.contains(value)
}

fn normalize_dm_policy(channel_name: &str, dm_policy: crate::config::DmPolicy) -> crate::config::DmPolicy {
    match dm_policy {
        crate::config::DmPolicy::Pairing => {
            tracing::warn!(
                channel = channel_name,
                "dm_policy=pairing is not implemented for inbound channel policy; falling back to dm_policy=allowlist"
            );
            crate::config::DmPolicy::Allowlist
        }
        other => other,
    }
}

fn evaluate_inbound_policy(policy: &InboundPolicyConfig, msg: &traits::ChannelMessage) -> bool {
    match infer_chat_type_from_message(msg) {
        "group" => match policy.group_policy {
            crate::config::GroupPolicy::Disabled => {
                tracing::warn!(channel = %msg.channel, sender = %msg.sender, "Ignoring group message due to disabled group_policy");
                false
            }
            crate::config::GroupPolicy::Open => true,
            crate::config::GroupPolicy::Allowlist => {
                let Some(group_id) = extract_group_identifier(msg) else {
                    tracing::warn!(channel = %msg.channel, sender = %msg.sender, "Dropping group message: missing group identifier for allowlist check");
                    return false;
                };
                if allowlist_matches(&policy.group_allow_from, &group_id) {
                    true
                } else {
                    tracing::warn!(channel = %msg.channel, sender = %msg.sender, group_id = %group_id, "Dropping group message: group not in allowlist");
                    false
                }
            }
        },
        _ => match policy.dm_policy {
            crate::config::DmPolicy::Disabled => {
                tracing::warn!(channel = %msg.channel, sender = %msg.sender, "Ignoring direct message due to disabled dm_policy");
                false
            }
            crate::config::DmPolicy::Open => true,
            crate::config::DmPolicy::Allowlist => {
                if allowlist_matches(&policy.allowed_from, &msg.sender) {
                    true
                } else {
                    tracing::warn!(channel = %msg.channel, sender = %msg.sender, "Dropping direct message: sender not in allowlist");
                    false
                }
            }
            crate::config::DmPolicy::Pairing => {
                tracing::warn!(
                    channel = %msg.channel,
                    sender = %msg.sender,
                    "dm_policy=pairing is not implemented yet; evaluating as dm_policy=allowlist"
                );
                if allowlist_matches(&policy.allowed_from, &msg.sender) {
                    true
                } else {
                    tracing::warn!(channel = %msg.channel, sender = %msg.sender, "Dropping direct message: sender not in allowlist");
                    false
                }
            }
        },
    }
}

fn should_process_inbound_message(ctx: &ChannelRuntimeContext, msg: &traits::ChannelMessage) -> bool {
    let policy = match msg.channel.as_str() {
        "signal" => ctx.signal_inbound_policy.as_ref(),
        "whatsapp" => ctx.whatsapp_inbound_policy.as_ref(),
        _ => None,
    };
    let Some(policy) = policy else {
        return true;
    };

    evaluate_inbound_policy(policy, msg)
}

fn collect_bot_names(config: &Config) -> Vec<String> {
    let mut names: Vec<String> = vec!["prx".to_string()];

    if let Ok(Some(aieos_identity)) = identity::load_aieos_identity(&config.identity, &config.workspace_dir) {
        if let Some(identity) = aieos_identity.identity {
            if let Some(agent_names) = identity.names {
                if let Some(first) = agent_names.first {
                    names.push(first);
                }
                if let Some(last) = agent_names.last {
                    names.push(last);
                }
                if let Some(nickname) = agent_names.nickname {
                    names.push(nickname);
                }
                if let Some(full) = agent_names.full {
                    names.push(full);
                }
            }
        }
    }

    for binding in &config.identity_bindings {
        if let Some(display_name) = binding.display_name.as_deref() {
            names.push(display_name.to_string());
        }
    }

    if let Some(signal) = config.channels_config.signal.as_ref() {
        names.push(signal.account.clone());
    }

    if let Some(whatsapp) = config.channels_config.whatsapp.as_ref() {
        if let Some(pair_phone) = whatsapp.pair_phone.as_deref() {
            names.push(pair_phone.to_string());
            if !pair_phone.starts_with('+') {
                names.push(format!("+{pair_phone}"));
            }
        }
        if let Some(phone_number_id) = whatsapp.phone_number_id.as_deref() {
            names.push(phone_number_id.to_string());
        }
    }

    if let Some(irc) = config.channels_config.irc.as_ref() {
        names.push(irc.nickname.clone());
    }

    let mut seen = HashSet::new();
    names
        .into_iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .filter(|name| seen.insert(name.to_ascii_lowercase()))
        .collect()
}

fn collect_bot_uuids(config: &Config) -> Vec<String> {
    let mut uuids: Vec<String> = Vec::new();
    if let Some(signal) = config.channels_config.signal.as_ref() {
        let account = signal.account.trim().to_string();
        if !account.is_empty() {
            uuids.push(account);
        }
    }
    uuids
}

fn collect_mention_only_by_channel(config: &Config) -> HashMap<String, bool> {
    let mut mention_only = HashMap::new();

    if let Some(telegram) = config.channels_config.telegram.as_ref() {
        mention_only.insert("telegram".to_string(), telegram.mention_only);
    }
    if let Some(discord) = config.channels_config.discord.as_ref() {
        mention_only.insert("discord".to_string(), discord.mention_only);
    }
    if let Some(slack) = config.channels_config.slack.as_ref() {
        mention_only.insert("slack".to_string(), slack.mention_only);
    }
    if let Some(mattermost) = config.channels_config.mattermost.as_ref() {
        mention_only.insert("mattermost".to_string(), mattermost.mention_only.unwrap_or(false));
    }
    if let Some(imessage) = config.channels_config.imessage.as_ref() {
        mention_only.insert("imessage".to_string(), imessage.mention_only);
    }
    if let Some(matrix) = config.channels_config.matrix.as_ref() {
        mention_only.insert("matrix".to_string(), matrix.mention_only);
    }
    if let Some(signal) = config.channels_config.signal.as_ref() {
        mention_only.insert("signal".to_string(), signal.mention_only);
    }
    if let Some(whatsapp) = config.channels_config.whatsapp.as_ref() {
        mention_only.insert("whatsapp".to_string(), whatsapp.mention_only);
    }
    if let Some(wacli) = config.channels_config.wacli.as_ref() {
        mention_only.insert("wacli".to_string(), wacli.mention_only);
    }
    if let Some(linq) = config.channels_config.linq.as_ref() {
        mention_only.insert("linq".to_string(), linq.mention_only);
    }
    if let Some(nextcloud_talk) = config.channels_config.nextcloud_talk.as_ref() {
        mention_only.insert("nextcloud_talk".to_string(), nextcloud_talk.mention_only);
    }
    if let Some(irc) = config.channels_config.irc.as_ref() {
        mention_only.insert("irc".to_string(), irc.mention_only);
    }
    if let Some(lark) = config.channels_config.lark.as_ref() {
        mention_only.insert("lark".to_string(), lark.mention_only);
    }
    if let Some(dingtalk) = config.channels_config.dingtalk.as_ref() {
        mention_only.insert("dingtalk".to_string(), dingtalk.mention_only);
    }
    if let Some(qq) = config.channels_config.qq.as_ref() {
        mention_only.insert("qq".to_string(), qq.mention_only);
    }

    mention_only
}

/// Collect the effective group reply mode for channels that support smart
/// group-reply (currently Telegram, Discord, WhatsApp, and wacli). Other channels are
/// absent and fall back to their `mention_only`-derived behavior unchanged.
fn collect_group_reply_mode_by_channel(config: &Config) -> HashMap<String, crate::config::GroupReplyMode> {
    let mut modes = HashMap::new();
    if let Some(telegram) = config.channels_config.telegram.as_ref() {
        modes.insert(
            "telegram".to_string(),
            crate::config::GroupReplyMode::resolve(telegram.group_reply_mode, telegram.mention_only),
        );
    }
    if let Some(discord) = config.channels_config.discord.as_ref() {
        modes.insert(
            "discord".to_string(),
            crate::config::GroupReplyMode::resolve(discord.group_reply_mode, discord.mention_only),
        );
    }
    if let Some(whatsapp) = config.channels_config.whatsapp.as_ref() {
        modes.insert(
            "whatsapp".to_string(),
            crate::config::GroupReplyMode::resolve(whatsapp.group_reply_mode, whatsapp.mention_only),
        );
    }
    if let Some(wacli) = config.channels_config.wacli.as_ref() {
        modes.insert(
            "wacli".to_string(),
            crate::config::GroupReplyMode::resolve(wacli.group_reply_mode, wacli.mention_only),
        );
    }
    modes
}

fn is_mention_only_enabled(ctx: &ChannelRuntimeContext, channel_name: &str) -> bool {
    ctx.mention_only_by_channel.get(channel_name).copied().unwrap_or(false)
}

/// Effective group reply mode for a channel (smart group-reply). Defaults to
/// `MentionOnly`-equivalent behavior when the channel is not registered as a
/// smart-capable channel.
fn group_reply_mode_for(ctx: &ChannelRuntimeContext, channel_name: &str) -> crate::config::GroupReplyMode {
    ctx.group_reply_mode_by_channel
        .get(channel_name)
        .copied()
        .unwrap_or_else(|| crate::config::GroupReplyMode::resolve(None, is_mention_only_enabled(ctx, channel_name)))
}

fn is_bot_mentioned(ctx: &ChannelRuntimeContext, msg: &traits::ChannelMessage, content: &str) -> bool {
    // Check UUID-based mentions first (Signal @mentions)
    for uuid in &ctx.bot_uuids {
        if msg.mentioned_uuids.contains(uuid) {
            return true;
        }
    }
    // Fall back to text-based name matching
    let content_lower = content.to_lowercase();
    for name in &ctx.bot_names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        if content_lower.contains(&trimmed.to_lowercase()) {
            return true;
        }
    }
    false
}

/// Strip channel metadata from message content, returning only the user-authored text.
/// This prevents group names or sender IDs embedded in metadata from
/// triggering false-positive mention detection.
///
/// Strips:
/// - Entire lines matching `[xxx-meta ...]` (e.g. `[signal-meta sender=... group=...]`)
/// - Inline prefixes like `[Signal Group: groupname] sender:` at the start of lines
fn strip_channel_metadata(content: &str) -> String {
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            // Skip entire lines that are metadata tags (e.g. [signal-meta ...])
            if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.contains("-meta ") {
                return None;
            }
            // Strip inline group prefixes like "[Signal Group: name] sender: "
            // These contain group names that could false-positive on bot name matching.
            let cleaned = trimmed.strip_prefix('[').map_or_else(
                || line.to_string(),
                |rest| {
                    rest.find(']').map_or_else(
                        || line.to_string(),
                        |bracket_end| {
                            let tag = &rest[..bracket_end];
                            if tag.contains("Group") || tag.contains("group") {
                                // Skip past the "] sender: " part
                                let after_bracket = &rest[bracket_end + 1..];
                                // Skip "sender_id: " prefix after the bracket
                                after_bracket.find(": ").map_or_else(
                                    || after_bracket.trim_start().to_string(),
                                    |colon_pos| after_bracket[colon_pos + 2..].to_string(),
                                )
                            } else {
                                line.to_string()
                            }
                        },
                    )
                },
            );
            Some(cleaned)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn warn_open_policy_allowlist_alignment(
    channel_name: &str,
    dm_policy: crate::config::DmPolicy,
    group_policy: crate::config::GroupPolicy,
    allowed_from: &HashSet<String>,
    group_allow_from: &HashSet<String>,
) {
    if dm_policy == crate::config::DmPolicy::Open && !allowed_from.contains("*") {
        tracing::warn!(
            channel = channel_name,
            "dm_policy=open configured without '*' in allowed_from; this weakens static safety intent"
        );
    }
    if group_policy == crate::config::GroupPolicy::Open && !group_allow_from.contains("*") {
        tracing::warn!(
            channel = channel_name,
            "group_policy=open configured without '*' in group_allow_from; this weakens static safety intent"
        );
    }
}

/// Per-sender conversation key, carrying both the unified canonical key and the
/// legacy `{channel}_{sender}` key for backward-compatible storage continuity.
///
/// FIX-P1-25b / FIX-P1-02: the canonical key is derived through
/// [`RuntimeEnvelope::canonical_session_key`], the single source-agnostic
/// derivation shared with chat/agent/gateway, so all ingress paths key on one
/// deterministic format instead of each mode inventing its own.
///
/// ## Read-merge (union) over the legacy key, never move
///
/// The channel ingress historically persisted conversation turns (and hydrated
/// the in-memory history map on startup) under the `legacy` key
/// (`{channel}_{sender}`). The canonical key adds a `recipient` component
/// (`channel:{channel}:{sender}:{recipient}`), so a single legacy key can map to
/// *several* canonical keys — one per recipient the sender talked to.
///
/// Two consequences forbid a "move the legacy entry to the canonical key"
/// strategy:
///
/// 1. **History read window.** Once any new turn is written under the canonical
///    key, a move only ever fires when the canonical entry is absent; after the
///    first canonical write the legacy turns would be silently skipped on read,
///    dropping all pre-cutover history.
/// 2. **Recipient dimension mismatch.** The legacy key is inherently
///    sender-scoped (it has no recipient). Moving it into one canonical
///    (recipient-specific) key both misattributes the data to a single
///    recipient and orphans it for every other recipient of the same sender.
///
/// Instead, writes go to the canonical key only, and every *read* path takes the
/// **union of the canonical and legacy histories** (see [`merged_history`]):
/// legacy data is treated as read-only pre-history that is visible to *every*
/// canonical conversation of the same sender. Because the legacy data genuinely
/// predates the recipient distinction, surfacing it to each recipient is the
/// faithful behaviour — no turn is lost, none is misrouted, and the legacy key
/// is never written, moved, or deleted from durable storage.
struct ConversationKey {
    canonical: String,
    legacy: String,
}

impl ConversationKey {
    fn from_message(msg: &traits::ChannelMessage) -> Self {
        // The recipient component mirrors RuntimeEnvelope::channel's recipient
        // (the chat_id / reply_target), so a channel turn and its envelope agree
        // on the canonical key.
        let canonical = RuntimeEnvelope::channel(
            String::new(),
            msg.channel.clone(),
            msg.sender.clone(),
            Some(msg.reply_target.clone()),
        )
        .canonical_session_key();
        Self {
            canonical,
            legacy: format!("{}_{}", msg.channel, msg.sender),
        }
    }
}

fn channel_message_visibility(msg: &traits::ChannelMessage) -> MemoryVisibility {
    if chat_kind_from_message(msg).is_group_like() {
        MemoryVisibility::Session
    } else {
        MemoryVisibility::Workspace
    }
}

fn to_rfc3339_timestamp(raw_timestamp: u64) -> Option<String> {
    if raw_timestamp == 0 {
        return None;
    }

    let raw_timestamp = i64::try_from(raw_timestamp).ok()?;
    let timestamp = if raw_timestamp > 10_000_000_000 {
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(raw_timestamp)
    } else {
        chrono::DateTime::<chrono::Utc>::from_timestamp(raw_timestamp, 0)
    }?;
    Some(timestamp.to_rfc3339())
}

fn interruption_scope_key(msg: &traits::ChannelMessage) -> String {
    format!("{}_{}_{}", msg.channel, msg.reply_target, msg.sender)
}

fn channel_delivery_instructions(channel_name: &str) -> Option<&'static str> {
    match channel_name {
        "telegram" => Some(
            "When responding on Telegram, include media markers for files or URLs that should be sent as attachments. Use one marker per attachment with this exact syntax: [IMAGE:<path-or-url>], [DOCUMENT:<path-or-url>], [VIDEO:<path-or-url>], [AUDIO:<path-or-url>], or [VOICE:<path-or-url>]. Keep normal user-facing text outside markers and never wrap markers in code fences.",
        ),
        _ => None,
    }
}

fn channel_runtime_instructions(channel_name: &str) -> String {
    let mut instructions = String::from(
        "## Channel Capabilities\n\n\
         - You are running as a messaging bot. Your response is automatically sent back to the user's channel.\n\
         - You do NOT need to ask permission to respond; just respond directly.\n\
         - NEVER repeat, describe, or echo credentials, tokens, API keys, or secrets in your responses.\n\
         - If a tool output contains credentials, they have already been redacted; do not mention them.\n",
    );
    if let Some(delivery) = channel_delivery_instructions(channel_name) {
        instructions.push('\n');
        instructions.push_str(delivery);
    }
    instructions
}

/// System-prompt addendum for smart group-reply turns. Overrides the default
/// "respond directly" guidance: tells the model it is one participant in a group
/// and that staying silent (via `stay_silent`) is often the correct choice.
///
/// When the bot was explicitly @-mentioned, it should normally answer; otherwise
/// it should only speak when it can clearly add value.
const fn smart_group_prompt_addendum(mentioned: bool) -> &'static str {
    if mentioned {
        "\n## Group Chat Mode (smart)\n\n\
         - You are ONE participant in a group conversation. This message addresses you directly.\n\
         - Reply normally — but stay concise and on-topic; do not dominate the conversation.\n\
         - If a reply truly is not warranted, you may call the `stay_silent` tool with a short reason \
           instead of sending a message.\n"
    } else {
        "\n## Group Chat Mode (smart)\n\n\
         - You are ONE participant in a group conversation. This message was NOT addressed to you.\n\
         - Only speak when you can clearly add value (answer a question aimed at the group, correct a \
           critical error, or provide help you are uniquely able to give).\n\
         - For small talk, chatter between other people, or anything not relevant to you, call the \
           `stay_silent` tool with a short reason. Staying silent is the DEFAULT and correct choice for \
           most messages — do not feel obliged to reply.\n\
         - Never reply just to acknowledge; silence is better than noise.\n"
    }
}

/// Prompt-guided instruction block advertising ONLY the `stay_silent` tool, for
/// the non-native + skill-RAG-off smart group-reply path. The startup static
/// prompt deliberately excludes `stay_silent` (so DMs / non-smart never see it);
/// this appends just that tool's spec on a smart group turn so a prompt-guided
/// model learns it exists. Native providers advertise it via filtered tool specs
/// instead and never need this.
fn stay_silent_tool_instructions() -> String {
    let tool = tools::StaySilentTool::new();
    let mut block = String::from("\n### Additional Tool (group chat)\n\n");
    for spec in tool.specs() {
        let _ = writeln!(
            block,
            "**{}**: {}\nParameters: `{}`\n",
            spec.name, spec.description, spec.parameters
        );
    }
    block
}

fn prompt_trim(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut truncated = false;
    for (idx, ch) in value.trim().chars().enumerate() {
        if idx >= max_chars {
            truncated = true;
            break;
        }
        out.push(ch);
    }
    if truncated {
        out.push_str("...");
    }
    out
}

const fn display_chat_kind(kind: ChatKind) -> &'static str {
    match kind {
        ChatKind::Dm => "dm",
        ChatKind::Group => "group",
        ChatKind::Thread => "thread",
    }
}

fn build_current_conversation_prompt(
    msg: &traits::ChannelMessage,
    profile: Option<&ChatProfile>,
    bot_identity: Option<&str>,
) -> String {
    let kind = profile
        .map(|profile| profile.chat_kind.as_str())
        .map(|value| match value {
            "group" => "group",
            "thread" => "thread",
            _ => "dm",
        })
        .unwrap_or_else(|| display_chat_kind(chat_kind_from_message(msg)));
    let title = msg
        .chat_title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| profile.and_then(|profile| profile.title.as_deref()))
        .unwrap_or("untitled");
    let you = bot_identity.unwrap_or(msg.channel.as_str());
    let mut block = format!(
        "## Current Conversation\n- Platform: {} | You: {}\n- Type: {} | Chat: \"{}\" ({})",
        msg.channel,
        prompt_trim(you, 48),
        kind,
        prompt_trim(title, 80),
        prompt_trim(&msg.reply_target, 96)
    );
    if let Some(profile) = profile {
        if let Some(purpose) = profile.purpose.as_deref().filter(|value| !value.trim().is_empty()) {
            let _ = write!(block, "\n- Purpose (self-maintained): {}", prompt_trim(purpose, 180));
        }
        let notes = profile.notes.as_deref().filter(|value| !value.trim().is_empty());
        let tags = if profile.tags.is_empty() {
            None
        } else {
            Some(profile.tags.join(", "))
        };
        if notes.is_some() || tags.is_some() {
            let _ = write!(
                block,
                "\n- Notes: {} | Tags: {}",
                notes
                    .map(|value| prompt_trim(value, 240))
                    .unwrap_or_else(|| "none".to_string()),
                tags.as_deref()
                    .map(|value| prompt_trim(value, 120))
                    .unwrap_or_else(|| "none".to_string())
            );
        }
    }
    block.push_str("\n- When you learn what this chat is for, use the chat_profile_update tool (not memory_store).");
    block
}

fn build_channel_system_prompt(
    base_prompt: &str,
    msg: &traits::ChannelMessage,
    profile: Option<&ChatProfile>,
    bot_identity: Option<&str>,
) -> String {
    let instructions = channel_runtime_instructions(&msg.channel);
    let current_conversation = build_current_conversation_prompt(msg, profile, bot_identity);
    let instructions = format!("{instructions}\n\n{current_conversation}");
    if base_prompt.is_empty() {
        instructions
    } else {
        format!("{base_prompt}\n\n{instructions}")
    }
}

fn normalize_cached_channel_turns(turns: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut normalized = Vec::with_capacity(turns.len());
    let mut expecting_user = true;

    for turn in turns {
        match (expecting_user, turn.role.as_str()) {
            (true, "user") => {
                normalized.push(turn);
                expecting_user = false;
            }
            (false, "assistant") => {
                normalized.push(turn);
                expecting_user = true;
            }
            // Interrupted channel turns can produce consecutive user messages
            // (no assistant persisted yet). Merge instead of dropping.
            (false, "user") | (true, "assistant") => {
                if let Some(last_turn) = normalized.last_mut() {
                    if !turn.content.is_empty() {
                        if !last_turn.content.is_empty() {
                            last_turn.content.push_str("\n\n");
                        }
                        last_turn.content.push_str(&turn.content);
                    }
                }
            }
            _ => {}
        }
    }

    normalized
}

fn supports_runtime_model_switch(channel_name: &str) -> bool {
    matches!(channel_name, "telegram" | "discord")
}

fn parse_runtime_command(channel_name: &str, content: &str) -> Option<ChannelRuntimeCommand> {
    if !supports_runtime_model_switch(channel_name) {
        return None;
    }

    let trimmed = content.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let mut parts = trimmed.split_whitespace();
    let command_token = parts.next()?;
    let base_command = command_token
        .split('@')
        .next()
        .unwrap_or(command_token)
        .to_ascii_lowercase();

    match base_command.as_str() {
        "/models" => parts
            .next()
            .map_or(Some(ChannelRuntimeCommand::ShowProviders), |provider| {
                Some(ChannelRuntimeCommand::SetProvider(provider.trim().to_string()))
            }),
        "/model" => {
            let model = parts.collect::<Vec<_>>().join(" ").trim().to_string();
            if model.is_empty() {
                Some(ChannelRuntimeCommand::ShowModel)
            } else {
                Some(ChannelRuntimeCommand::SetModel(model))
            }
        }
        _ => None,
    }
}

fn resolve_provider_alias(name: &str) -> Option<String> {
    let candidate = name.trim();
    if candidate.is_empty() {
        return None;
    }

    let providers_list = providers::list_providers();
    for provider in providers_list {
        if provider.name.eq_ignore_ascii_case(candidate)
            || provider
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(candidate))
        {
            return Some(provider.name.to_string());
        }
    }

    None
}

fn resolved_default_provider(config: &Config) -> String {
    config
        .default_provider
        .clone()
        .unwrap_or_else(|| "openrouter".to_string())
}

fn resolved_default_model(config: &Config) -> String {
    config
        .default_model
        .clone()
        .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string())
}

fn runtime_defaults_from_config(config: &Config) -> ChannelRuntimeDefaults {
    ChannelRuntimeDefaults {
        default_provider: resolved_default_provider(config),
        model: resolved_default_model(config),
        temperature: config.default_temperature,
        api_key: config.api_key.clone(),
        api_url: config.api_url.clone(),
        reliability: config.reliability.clone(),
    }
}

fn runtime_defaults_for_generation(
    ctx: &ChannelRuntimeContext,
    generation: &Arc<crate::config::ConfigGeneration>,
) -> ChannelRuntimeDefaults {
    #[cfg(test)]
    if !Arc::ptr_eq(&ctx.config.pin(), &ctx.config_generation) {
        // Legacy unit fixtures historically constructed `config` and
        // `config_generation` from two independent managers, then injected
        // explicit provider/model/runtime fields on the context. Preserve those
        // fixture semantics while dedicated generation tests use a same-owner
        // pair and therefore exercise the production path below.
        return ChannelRuntimeDefaults {
            default_provider: ctx.default_provider.as_str().to_string(),
            model: ctx.model.as_str().to_string(),
            temperature: ctx.temperature,
            api_key: ctx.api_key.clone(),
            api_url: ctx.api_url.clone(),
            reliability: ctx.reliability.as_ref().clone(),
        };
    }
    #[cfg(not(test))]
    let _ = ctx;
    runtime_defaults_from_config(&generation.effective)
}

fn message_runtime_snapshot(
    ctx: &ChannelRuntimeContext,
    generation: &Arc<crate::config::ConfigGeneration>,
) -> ChannelMessageRuntimeSnapshot {
    #[cfg(test)]
    if !Arc::ptr_eq(&ctx.config.pin(), &ctx.config_generation) {
        return ChannelMessageRuntimeSnapshot {
            multimodal: ctx.multimodal.clone(),
            max_tool_iterations: ctx.max_tool_iterations,
            read_only_tool_concurrency_window: ctx.read_only_tool_concurrency_window,
            read_only_tool_timeout_secs: ctx.read_only_tool_timeout_secs,
            priority_scheduling_enabled: ctx.priority_scheduling_enabled,
            low_priority_tools: ctx.low_priority_tools.clone(),
            min_relevance_score: ctx.min_relevance_score,
            agent_compaction: ctx.agent_compaction.clone(),
            tool_tiering: ctx.tool_tiering.clone(),
        };
    }
    #[cfg(not(test))]
    let _ = ctx;

    let config = generation.effective.as_ref();
    ChannelMessageRuntimeSnapshot {
        multimodal: config.multimodal.clone(),
        max_tool_iterations: config.agent.max_tool_iterations,
        read_only_tool_concurrency_window: config.agent.read_only_tool_concurrency_window,
        read_only_tool_timeout_secs: config.agent.read_only_tool_timeout_secs,
        priority_scheduling_enabled: config.agent.priority_scheduling_enabled,
        low_priority_tools: config.agent.low_priority_tools.clone(),
        min_relevance_score: config.memory.min_relevance_score,
        agent_compaction: config.agent.compaction.clone(),
        tool_tiering: config.tool_tiering.clone(),
    }
}

fn default_route_selection_for_generation(
    ctx: &ChannelRuntimeContext,
    generation: &Arc<crate::config::ConfigGeneration>,
) -> ChannelRouteSelection {
    let defaults = runtime_defaults_for_generation(ctx, generation);
    ChannelRouteSelection {
        provider: defaults.default_provider,
        model: defaults.model,
    }
}

/// Read-only union of the canonical and legacy histories for `key`.
///
/// FIX-P1-25b (read-merge, not move): the legacy `{channel}_{sender}` history
/// predates the canonical recipient dimension, so it is surfaced as shared
/// pre-history for any canonical conversation of the same sender. Legacy turns
/// are ordered first (they are older — written before the canonical cutover),
/// then canonical turns. The result is truncated to the same
/// `MAX_CHANNEL_HISTORY` window used for single-key reads.
///
/// ## No deduplication — pure concatenation
///
/// The legacy and canonical keys partition the timeline: legacy turns were
/// written *before* the canonical cutover, canonical turns *after* it. Each
/// physical conversation turn is persisted under exactly one of the two keys, so
/// reading the union never surfaces the same physical turn twice. A
/// content/role-based dedup would therefore only ever remove *genuinely
/// repeated* messages within a single conversation (e.g. a user who deliberately
/// sends the same text twice in a row), corrupting the history the model sees.
/// We must not do that — the merge is a plain ordered concatenation
/// (legacy ++ canonical), preserving each store's own insertion order.
///
/// The legacy map entry is never moved, mutated, or removed here; it stays a
/// read-only fallback.
fn merged_history(
    map: &std::collections::HashMap<String, Vec<ChatMessage>>,
    key: &ConversationKey,
) -> Vec<ChatMessage> {
    let legacy_turns = if key.legacy == key.canonical {
        None
    } else {
        map.get(&key.legacy)
    };
    let canonical_turns = map.get(&key.canonical);

    let capacity = legacy_turns.map_or(0, Vec::len) + canonical_turns.map_or(0, Vec::len);
    if capacity == 0 {
        return Vec::new();
    }

    // Pure ordered concatenation: legacy pre-history first, then canonical turns,
    // each in its own insertion order. No dedup (see doc comment) — repeated
    // messages within a conversation are real history and must be preserved.
    let mut merged: Vec<ChatMessage> = Vec::with_capacity(capacity);
    merged.extend(legacy_turns.into_iter().flatten().cloned());
    merged.extend(canonical_turns.into_iter().flatten().cloned());

    // Apply the same retention window single-key reads enforce, keeping the most
    // recent turns when the union exceeds the cap.
    if merged.len() > MAX_CHANNEL_HISTORY {
        let drop = merged.len() - MAX_CHANNEL_HISTORY;
        merged.drain(0..drop);
    }
    merged
}

fn get_route_selection_for_generation(
    ctx: &ChannelRuntimeContext,
    key: &ConversationKey,
    generation: &Arc<crate::config::ConfigGeneration>,
) -> ChannelRouteSelection {
    let routes = ctx.route_overrides.lock();
    // Read-merge for the single-value route map: prefer the canonical override,
    // fall back to the legacy (sender-scoped) override as read-only pre-history
    // so a route preference set before the canonical cutover still applies to
    // every recipient of that sender. The legacy entry is never moved or removed.
    routes
        .get(&key.canonical)
        .or_else(|| {
            if key.legacy == key.canonical {
                None
            } else {
                routes.get(&key.legacy)
            }
        })
        .cloned()
        .unwrap_or_else(|| default_route_selection_for_generation(ctx, generation))
}

fn set_route_selection(
    ctx: &ChannelRuntimeContext,
    generation: &Arc<crate::config::ConfigGeneration>,
    key: &ConversationKey,
    next: ChannelRouteSelection,
) {
    let default_route = default_route_selection_for_generation(ctx, generation);
    let mut routes = ctx.route_overrides.lock();
    if next == default_route {
        routes.remove(&key.canonical);
    } else {
        routes.insert(key.canonical.clone(), next);
    }
}

fn clear_sender_history(ctx: &ChannelRuntimeContext, key: &ConversationKey) {
    // Clear ONLY the current (canonical) session. The legacy
    // `{channel}_{sender}` entry is the *immutable, cross-recipient shared*
    // pre-cutover history: removing it here would silently wipe the pre-history
    // of every *other* recipient of the same sender (legacy has no recipient
    // dimension), which violates the "legacy is read-only, never moved/deleted"
    // invariant. So clear (e.g. triggered by /model or /models) drops the
    // canonical conversation only; the legacy pre-history is left intact and will
    // naturally age out of the merged read window as new canonical turns fill it.
    ctx.conversation_histories.lock().remove(&key.canonical);
}

fn compact_sender_history(ctx: &ChannelRuntimeContext, key: &ConversationKey) -> bool {
    let mut histories = ctx.conversation_histories.lock();

    // Compact ONLY the canonical session, never the legacy entry. Legacy is the
    // immutable, cross-recipient shared pre-cutover history; mutating or dropping
    // it here would corrupt the pre-history of every other recipient of the same
    // sender. Legacy is also already bounded (it was capped at MAX_CHANNEL_HISTORY
    // when written, ≤ 50 turns), and `merged_history` re-applies that window
    // across the union on every read — so the merged length the model sees stays
    // bounded (≤ legacy_cap + compacted_canonical, then truncated to the window)
    // without compaction needing to touch legacy at all. This is an in-memory
    // cache operation only: durable rows are untouched and re-hydrate on restart.
    let Some(canonical_turns) = histories.get(&key.canonical) else {
        return false;
    };
    if canonical_turns.is_empty() {
        return false;
    }

    let keep_from = canonical_turns
        .len()
        .saturating_sub(CHANNEL_HISTORY_COMPACT_KEEP_MESSAGES);
    let mut compacted = normalize_cached_channel_turns(
        canonical_turns
            .get(keep_from..)
            .map(<[ChatMessage]>::to_vec)
            .unwrap_or_default(),
    );

    for turn in &mut compacted {
        if turn.content.chars().count() > CHANNEL_HISTORY_COMPACT_CONTENT_CHARS {
            turn.content = truncate_with_ellipsis(&turn.content, CHANNEL_HISTORY_COMPACT_CONTENT_CHARS);
        }
    }

    // Enforce a hard total character budget to avoid repeated context overflow.
    while compacted.iter().map(|turn| turn.content.chars().count()).sum::<usize>() > CHANNEL_HISTORY_COMPACT_TOTAL_CHARS
        && compacted.len() > 1
    {
        compacted.remove(0);
    }

    // Write the compacted result back under the canonical key only; the legacy
    // entry is never removed or rewritten.
    if compacted.is_empty() {
        histories.remove(&key.canonical);
        return false;
    }

    histories.insert(key.canonical.clone(), compacted);
    true
}

/// Resolve the inbound gate for this message. Production always uses the
/// static-dispatch `InboundGate::for_policy` over the real `SideEffectGate`. In
/// test builds, an injected `test_inbound_authorizer` (when present) takes
/// precedence so tests can express an operation-selective deny the real policy
/// cannot. The three `authorize_channel_*` helpers below funnel through this so
/// the operation-naming convention lives only in `InboundGate`.
#[cfg(not(test))]
fn authorize_channel_inbound(
    _ctx: &ChannelRuntimeContext,
    security: &crate::security::SecurityPolicy,
    channel: &str,
    sender: &str,
) -> Result<(), String> {
    InboundGate::for_policy(security).authorize_inbound(channel, sender)
}

#[cfg(test)]
fn authorize_channel_inbound(
    ctx: &ChannelRuntimeContext,
    security: &crate::security::SecurityPolicy,
    channel: &str,
    sender: &str,
) -> Result<(), String> {
    ctx.test_inbound_authorizer.as_ref().map_or_else(
        || InboundGate::for_policy(security).authorize_inbound(channel, sender),
        |authorizer| InboundGate::new(authorizer.as_ref()).authorize_inbound(channel, sender),
    )
}

#[cfg(not(test))]
fn authorize_channel_autosave(
    _ctx: &ChannelRuntimeContext,
    security: &crate::security::SecurityPolicy,
    channel: &str,
) -> Result<(), String> {
    InboundGate::for_policy(security).authorize_autosave(channel)
}

#[cfg(test)]
fn authorize_channel_autosave(
    ctx: &ChannelRuntimeContext,
    security: &crate::security::SecurityPolicy,
    channel: &str,
) -> Result<(), String> {
    ctx.test_inbound_authorizer.as_ref().map_or_else(
        || InboundGate::for_policy(security).authorize_autosave(channel),
        |authorizer| InboundGate::new(authorizer.as_ref()).authorize_autosave(channel),
    )
}

#[cfg(not(test))]
fn authorize_channel_outbound(
    _ctx: &ChannelRuntimeContext,
    security: &crate::security::SecurityPolicy,
    channel: &str,
) -> Result<(), String> {
    InboundGate::for_policy(security).authorize_outbound(channel)
}

#[cfg(test)]
fn authorize_channel_outbound(
    ctx: &ChannelRuntimeContext,
    security: &crate::security::SecurityPolicy,
    channel: &str,
) -> Result<(), String> {
    ctx.test_inbound_authorizer.as_ref().map_or_else(
        || InboundGate::for_policy(security).authorize_outbound(channel),
        |authorizer| InboundGate::new(authorizer.as_ref()).authorize_outbound(channel),
    )
}

#[allow(clippy::too_many_arguments)]
async fn append_sender_turn(
    ctx: &ChannelRuntimeContext,
    config_generation: &Arc<crate::config::ConfigGeneration>,
    key: &ConversationKey,
    channel: &str,
    sender: &str,
    recipient: Option<&str>,
    turn: ChatMessage,
    visibility: MemoryVisibility,
    timestamp: Option<&str>,
    message_id: Option<&str>,
    // D8-1: the per-turn run_id, generated at the turn entry (before the inbound
    // append). Threaded onto the channel_with_session envelope so the user and
    // assistant message events recorded for this turn carry the same run_id
    // (latent provenance, EU AI Act Art.12).
    run_id: &str,
    record_message_event: bool,
) -> Option<crate::memory::MessageEvent> {
    let role = turn.role.clone();
    let content = turn.content.clone();
    {
        // Writes always go to the canonical entry only; the legacy entry is left
        // untouched as a read-only fallback that the read paths (`merged_history`)
        // union in. The per-key cap stays at MAX_CHANNEL_HISTORY here; the merged
        // read view re-applies the same cap across the union.
        let mut histories = ctx.conversation_histories.lock();
        let turns = histories.entry(key.canonical.clone()).or_default();
        turns.push(turn);
        while turns.len() > MAX_CHANNEL_HISTORY {
            turns.remove(0);
        }
    }

    // The message-event envelope keeps the legacy session_key: message_events are
    // a separate persistence namespace whose shared-event recall is keyed on the
    // existing identity, so it is left untouched here. The canonical key governs
    // the conversation-turn cache (above) and its persistence (below); legacy
    // conversation-turn rows stay readable via the merged-read fallback, so no
    // turn is lost.
    let envelope = RuntimeEnvelope::channel_with_session(
        ctx.workspace_dir.as_path().to_string_lossy().to_string(),
        key.legacy.clone(),
        channel.to_string(),
        sender.to_string(),
        recipient.unwrap_or(sender).to_string(),
        visibility,
    )
    .with_run_id(run_id)
    .with_config_generation(config_generation);
    let owner_id = envelope.resolved_owner_id();

    if let Err(error) = ctx
        .memory
        .append_conversation_turn(
            &key.canonical,
            channel,
            sender,
            &role,
            &content,
            timestamp,
            message_id,
            Some(owner_id.as_str()),
        )
        .await
    {
        tracing::warn!(
            session_key = key.canonical,
            channel,
            sender,
            "Failed to persist channel conversation turn: {error}"
        );
    }

    let fabric = MemoryFabric::new(
        ctx.memory.clone(),
        ctx.workspace_dir.as_path().to_string_lossy().to_string(),
    )
    .with_event_recording(ctx.memory_event_recording);
    let scope = if role == "assistant" {
        envelope.message_scope().with_sender("prx")
    } else {
        envelope.message_scope()
    };
    if !record_message_event {
        return None;
    }
    let result = if role == "assistant" {
        fabric.record_assistant_message(scope, content).await
    } else {
        fabric
            .record_inbound_user_message(
                scope,
                content,
                message_id.map(|id| format!("channel:{channel}:{id}")),
                None,
            )
            .await
    };
    match result {
        Ok(event) => Some(event),
        Err(error) => {
            tracing::warn!(
                session_key = key.canonical,
                channel,
                sender,
                role,
                "Failed to persist channel message event: {error}"
            );
            None
        }
    }
}

async fn load_persisted_histories(
    workspace_dir: &std::path::Path,
    memory: &dyn Memory,
) -> HashMap<String, Vec<ChatMessage>> {
    let principal = MemoryPrincipal {
        workspace_id: workspace_dir.to_string_lossy().to_string(),
        agent_id: None,
        persona_id: None,
        session_key: None,
        channel: None,
        sender: None,
        owner_id: Some("system:*".to_string()),
        legacy_session_key: None,
    };
    match memory
        .load_recent_conversation_histories(&principal, MAX_CHANNEL_HISTORY, MAX_HYDRATED_SESSIONS)
        .await
    {
        Ok(histories) => {
            // Hydrate every persisted session under its own stored key — legacy
            // `{channel}_{sender}` rows land under the legacy key, post-cutover
            // canonical rows under the canonical key. The read paths
            // (`merged_history`) union the two on access (FIX-P1-25b read-merge),
            // so a session that was migrated and then ran once (turns split across
            // both keys) reads back the full ordered union after a restart. We do
            // not merge here, because the in-memory map is keyed per stored key and
            // `ConversationKey` derivation (which pairs a canonical key with its
            // legacy key) happens per inbound message, not at hydration time.
            let mut hydrated: HashMap<String, Vec<ChatMessage>> = HashMap::new();
            for (session_key, turns) in histories {
                let turns = turns
                    .into_iter()
                    .map(|turn| ChatMessage {
                        role: turn.role,
                        content: turn.content,
                    })
                    .collect::<Vec<_>>();
                if !turns.is_empty() {
                    hydrated.insert(session_key, normalize_cached_channel_turns(turns));
                }
            }
            hydrated
        }
        Err(error) => {
            // Startup hydration is best-effort: a failure must not block channel
            // startup. The durable conversation_turns rows are untouched and will
            // re-hydrate on the next start; emit a structured warn so operators can
            // locate the cause instead of silently running on an empty cache.
            tracing::warn!(
                error = %error,
                "channel history hydration from persistent store failed; starting with empty in-memory cache (durable rows intact, will re-hydrate next start)"
            );
            HashMap::new()
        }
    }
}

#[allow(dead_code)]
fn should_skip_memory_context_entry(key: &str, content: &str) -> bool {
    if memory::is_assistant_autosave_key(key) {
        return true;
    }

    if key.trim().to_ascii_lowercase().ends_with("_history") {
        return true;
    }

    content.chars().count() > MEMORY_CONTEXT_MAX_CHARS
}

pub(crate) fn is_context_window_overflow_error(err: &anyhow::Error) -> bool {
    let lower = err.to_string().to_lowercase();
    [
        "exceeds the context window",
        "context window of this model",
        "maximum context length",
        "context length exceeded",
        "too many tokens",
        "token limit exceeded",
        "prompt is too long",
        "input is too long",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
}

fn load_cached_model_preview(workspace_dir: &Path, provider_name: &str) -> Vec<String> {
    let cache_path = workspace_dir.join("state").join(MODEL_CACHE_FILE);
    let Ok(raw) = std::fs::read_to_string(cache_path) else {
        return Vec::new();
    };
    let Ok(state) = serde_json::from_str::<ModelCacheState>(&raw) else {
        return Vec::new();
    };

    state
        .entries
        .into_iter()
        .find(|entry| entry.provider == provider_name)
        .map(|entry| {
            entry
                .models
                .into_iter()
                .take(MODEL_CACHE_PREVIEW_LIMIT)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

async fn get_or_create_provider_for_generation(
    ctx: &ChannelRuntimeContext,
    provider_name: &str,
    generation: &Arc<crate::config::ConfigGeneration>,
) -> anyhow::Result<Arc<dyn Provider>> {
    if let Some(existing) = ctx.provider_cache.lock().get(provider_name).cloned() {
        return Ok(existing);
    }
    if provider_name == ctx.default_provider.as_str() {
        return Ok(Arc::clone(&ctx.provider));
    }

    let defaults = runtime_defaults_for_generation(ctx, generation);
    let api_url = if provider_name == defaults.default_provider.as_str() {
        defaults.api_url.as_deref()
    } else {
        None
    };

    let provider = providers::create_resilient_provider_with_options(
        provider_name,
        defaults.api_key.as_deref(),
        api_url,
        &defaults.reliability,
        &ctx.provider_runtime_options,
    )?;
    let provider: Arc<dyn Provider> = Arc::from(provider);

    if let Err(err) = provider.warmup().await {
        tracing::warn!(provider = provider_name, "Provider warmup failed: {err}");
    }

    ctx.provider_cache
        .lock()
        .insert(provider_name.to_string(), Arc::clone(&provider));
    Ok(provider)
}

fn build_models_help_response(current: &ChannelRouteSelection, workspace_dir: &Path) -> String {
    let mut response = String::new();
    let _ = writeln!(
        response,
        "Current provider: `{}`\nCurrent model: `{}`",
        current.provider, current.model
    );
    response.push_str("\nSwitch model with `/model <model-id>`.\n");

    let cached_models = load_cached_model_preview(workspace_dir, &current.provider);
    if cached_models.is_empty() {
        let _ = writeln!(
            response,
            "\nNo cached model list found for `{}`. Ask the operator to run `prx models refresh --provider {}`.",
            current.provider, current.provider
        );
    } else {
        let _ = writeln!(response, "\nCached model IDs (top {}):", cached_models.len());
        for model in cached_models {
            let _ = writeln!(response, "- `{model}`");
        }
    }

    response
}

fn build_providers_help_response(current: &ChannelRouteSelection) -> String {
    let mut response = String::new();
    let _ = writeln!(
        response,
        "Current provider: `{}`\nCurrent model: `{}`",
        current.provider, current.model
    );
    response.push_str("\nSwitch provider with `/models <provider>`.\n");
    response.push_str("Switch model with `/model <model-id>`.\n\n");
    response.push_str("Available providers:\n");
    for provider in providers::list_providers() {
        if provider.aliases.is_empty() {
            let _ = writeln!(response, "- {}", provider.name);
        } else {
            let _ = writeln!(
                response,
                "- {} (aliases: {})",
                provider.name,
                provider.aliases.join(", ")
            );
        }
    }
    response
}

async fn handle_runtime_command_if_needed(
    ctx: &ChannelRuntimeContext,
    config_generation: &Arc<crate::config::ConfigGeneration>,
    msg: &traits::ChannelMessage,
    target_channel: Option<&Arc<dyn Channel>>,
) -> bool {
    let Some(command) = parse_runtime_command(&msg.channel, &msg.content) else {
        return false;
    };

    let Some(channel) = target_channel else {
        return true;
    };

    let sender_key = ConversationKey::from_message(msg);
    let mut current = get_route_selection_for_generation(ctx, &sender_key, config_generation);

    let response = match command {
        ChannelRuntimeCommand::ShowProviders => build_providers_help_response(&current),
        ChannelRuntimeCommand::SetProvider(raw_provider) => match resolve_provider_alias(&raw_provider) {
            Some(provider_name) => {
                match get_or_create_provider_for_generation(ctx, &provider_name, config_generation).await {
                    Ok(_) => {
                        if provider_name != current.provider {
                            current.provider = provider_name.clone();
                            set_route_selection(ctx, config_generation, &sender_key, current.clone());
                            clear_sender_history(ctx, &sender_key);
                        }

                        format!(
                            "Provider switched to `{provider_name}` for this sender session. Current model is `{}`.\nUse `/model <model-id>` to set a provider-compatible model.",
                            current.model
                        )
                    }
                    Err(err) => {
                        let safe_err = providers::sanitize_api_error(&err.to_string());
                        format!(
                            "Failed to initialize provider `{provider_name}`. Route unchanged.\nDetails: {safe_err}"
                        )
                    }
                }
            }
            None => format!("Unknown provider `{raw_provider}`. Use `/models` to list valid providers."),
        },
        ChannelRuntimeCommand::ShowModel => build_models_help_response(&current, ctx.workspace_dir.as_path()),
        ChannelRuntimeCommand::SetModel(raw_model) => {
            let model = raw_model.trim().trim_matches('`').to_string();
            if model.is_empty() {
                "Model ID cannot be empty. Use `/model <model-id>`.".to_string()
            } else {
                current.model = model.clone();
                set_route_selection(ctx, config_generation, &sender_key, current.clone());
                clear_sender_history(ctx, &sender_key);

                format!(
                    "Model switched to `{model}` for provider `{}` in this sender session.",
                    current.provider
                )
            }
        }
    };

    if let Err(err) = channel
        .send(&SendMessage::new(response, &msg.reply_target).in_thread(msg.thread_ts.clone()))
        .await
    {
        tracing::warn!("Failed to send runtime command response on {}: {err}", channel.name());
    }

    true
}

#[allow(dead_code)]
async fn build_memory_context(mem: &dyn Memory, user_msg: &str, min_relevance_score: f64) -> String {
    let mut context = String::new();

    if let Ok(entries) = mem.recall(user_msg, 5, None).await {
        let mut included = 0usize;
        let mut used_chars = 0usize;

        for entry in entries
            .iter()
            .filter(|e| e.score.map_or(true, |score| score >= min_relevance_score))
        {
            if included >= MEMORY_CONTEXT_MAX_ENTRIES {
                break;
            }

            if should_skip_memory_context_entry(&entry.key, &entry.content) {
                continue;
            }

            let content = if entry.content.chars().count() > MEMORY_CONTEXT_ENTRY_MAX_CHARS {
                truncate_with_ellipsis(&entry.content, MEMORY_CONTEXT_ENTRY_MAX_CHARS)
            } else {
                entry.content.clone()
            };

            let line = format!("- {}: {}\n", entry.key, content);
            let line_chars = line.chars().count();
            if used_chars + line_chars > MEMORY_CONTEXT_MAX_CHARS {
                break;
            }

            if included == 0 {
                context.push_str("[Memory context]\n");
            }

            context.push_str(&line);
            used_chars += line_chars;
            included += 1;
        }

        if included > 0 {
            context.push('\n');
        }
    }

    context
}

/// Extract a compact summary of tool interactions from history messages added
/// during `run_tool_call_loop`. Scans assistant messages for `<tool_call>` tags
/// or native tool-call JSON to collect tool names used.
/// Returns an empty string when no tools were invoked.
pub(crate) fn extract_tool_context_summary(history: &[ChatMessage], start_index: usize) -> String {
    fn push_unique_tool_name(tool_names: &mut Vec<String>, name: &str) {
        let candidate = name.trim();
        if candidate.is_empty() {
            return;
        }
        if !tool_names.iter().any(|existing| existing == candidate) {
            tool_names.push(candidate.to_string());
        }
    }

    fn collect_tool_names_from_tool_call_tags(content: &str, tool_names: &mut Vec<String>) {
        const TAG_PAIRS: [(&str, &str); 4] = [
            ("<tool_call>", "</tool_call>"),
            ("<toolcall>", "</toolcall>"),
            ("<tool-call>", "</tool-call>"),
            ("<invoke>", "</invoke>"),
        ];

        for (open_tag, close_tag) in TAG_PAIRS {
            for segment in content.split(open_tag) {
                if let Some(json_end) = segment.find(close_tag) {
                    let json_str = segment[..json_end].trim();
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                        if let Some(name) = val.get("name").and_then(|n| n.as_str()) {
                            push_unique_tool_name(tool_names, name);
                        }
                    }
                }
            }
        }
    }

    fn collect_tool_names_from_native_json(content: &str, tool_names: &mut Vec<String>) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(content) {
            if let Some(calls) = val.get("tool_calls").and_then(|c| c.as_array()) {
                for call in calls {
                    let name = call
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .or_else(|| call.get("name").and_then(|n| n.as_str()));
                    if let Some(name) = name {
                        push_unique_tool_name(tool_names, name);
                    }
                }
            }
        }
    }

    fn collect_tool_names_from_tool_results(content: &str, tool_names: &mut Vec<String>) {
        let marker = "<tool_result name=\"";
        let mut remaining = content;
        while let Some(start) = remaining.find(marker) {
            let name_start = start + marker.len();
            let after_name_start = &remaining[name_start..];
            if let Some(name_end) = after_name_start.find('"') {
                let name = &after_name_start[..name_end];
                push_unique_tool_name(tool_names, name);
                remaining = &after_name_start[name_end + 1..];
            } else {
                break;
            }
        }
    }

    let mut tool_names: Vec<String> = Vec::new();

    for msg in history.iter().skip(start_index) {
        match msg.role.as_str() {
            "assistant" => {
                collect_tool_names_from_tool_call_tags(&msg.content, &mut tool_names);
                collect_tool_names_from_native_json(&msg.content, &mut tool_names);
            }
            "user" => {
                // Prompt-mode tool calls are always followed by [Tool results] entries
                // containing `<tool_result name="...">` tags with canonical tool names.
                collect_tool_names_from_tool_results(&msg.content, &mut tool_names);
            }
            _ => {}
        }
    }

    if tool_names.is_empty() {
        return String::new();
    }

    format!("[Used tools: {}]", tool_names.join(", "))
}

pub(crate) fn sanitize_channel_response(response: &str, tools: &[Box<dyn Tool>]) -> String {
    let known_tool_names: HashSet<String> = tools.iter().map(|tool| tool.name().to_ascii_lowercase()).collect();
    let cleaned_tags = strip_isolated_tool_tag_artifacts(response, &known_tool_names);
    strip_isolated_tool_json_artifacts(&cleaned_tags, &known_tool_names)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SignalVisionPreflightOutcome {
    NotRequired,
    Ready { context: String },
    Fallback,
}

#[derive(Debug, Clone, Deserialize)]
struct SignalVisionPreflightReport {
    #[serde(default)]
    status: String,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    observation: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    result: Option<String>,
}

impl SignalVisionPreflightReport {
    fn summary_text(&self) -> Option<&str> {
        [
            self.summary.as_deref(),
            self.observation.as_deref(),
            self.description.as_deref(),
            self.result.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
    }
}

fn is_signal_image_message(msg: &traits::ChannelMessage) -> bool {
    if msg.channel != "signal" {
        return false;
    }

    let has_image_markers = !crate::multimodal::parse_image_markers(&msg.content).1.is_empty();
    let has_signal_image_meta =
        msg.content.contains("vision_required=true") || msg.content.contains("image_attachments=");
    has_image_markers || has_signal_image_meta
}

fn parse_signal_vision_preflight_report(raw: &str) -> Option<SignalVisionPreflightReport> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(report) = serde_json::from_str::<SignalVisionPreflightReport>(trimmed) {
        return Some(report);
    }

    for fenced in [trimmed.strip_prefix("```json"), trimmed.strip_prefix("```")] {
        let Some(inner) = fenced else {
            continue;
        };
        let inner = inner.trim();
        let inner = inner.strip_suffix("```").unwrap_or(inner).trim();
        if let Ok(report) = serde_json::from_str::<SignalVisionPreflightReport>(inner) {
            return Some(report);
        }
    }

    let object_start = trimmed.find('{')?;
    let object_end = trimmed.rfind('}')?;
    if object_end <= object_start {
        return None;
    }

    serde_json::from_str::<SignalVisionPreflightReport>(&trimmed[object_start..=object_end]).ok()
}

async fn run_signal_vision_preflight(
    provider: &dyn Provider,
    model: &str,
    temperature: f64,
    msg: &traits::ChannelMessage,
    multimodal_config: &crate::config::MultimodalConfig,
    media_artifacts: &crate::media::MediaArtifactOwner,
) -> SignalVisionPreflightOutcome {
    if !is_signal_image_message(msg) {
        return SignalVisionPreflightOutcome::NotRequired;
    }

    if !provider
        .capabilities_for(model, crate::providers::traits::ProviderRequestMode::NonStreaming)
        .vision
    {
        tracing::warn!("Signal image guard: provider does not support vision, forcing uncertainty fallback");
        return SignalVisionPreflightOutcome::Fallback;
    }

    let (cleaned_text, image_refs) = crate::multimodal::parse_image_markers(&msg.content);
    if image_refs.is_empty() {
        tracing::warn!("Signal image guard: image attachment metadata exists but no usable image markers");
        return SignalVisionPreflightOutcome::Fallback;
    }

    let mut user_prompt = String::new();
    if cleaned_text.trim().is_empty() {
        user_prompt.push_str("Signal inbound image attachment.");
    } else {
        user_prompt.push_str(cleaned_text.trim());
    }
    user_prompt.push_str("\n\n[Analyze attached images first]");
    for image_ref in image_refs {
        user_prompt.push('\n');
        user_prompt.push_str("[IMAGE:");
        user_prompt.push_str(image_ref.trim());
        user_prompt.push(']');
    }

    let preflight_messages = vec![
        ChatMessage::system(
            "You are a strict vision preflight validator. \
Return JSON only with this schema: \
{\"status\":\"ok\"|\"uncertain\",\"confidence\":0.0,\"summary\":\"...\",\"missing\":\"...\"}. \
If any key detail is unreadable, uncertain, cropped, or ambiguous, set status=\"uncertain\" with confidence below 0.60. \
Never guess brand, dosage, specification, or product identity.",
        ),
        ChatMessage::user(user_prompt),
    ];

    let prepared =
        match crate::multimodal::prepare_messages_for_provider(&preflight_messages, multimodal_config, media_artifacts)
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                tracing::warn!("Signal image guard: multimodal preflight normalization failed: {error}");
                return SignalVisionPreflightOutcome::Fallback;
            }
        };

    if !prepared.contains_images {
        tracing::warn!("Signal image guard: preflight request did not retain image payloads after normalization");
        return SignalVisionPreflightOutcome::Fallback;
    }

    let preflight_response = match tokio::time::timeout(
        Duration::from_secs(SIGNAL_VISION_PREFLIGHT_TIMEOUT_SECS),
        provider.chat(
            ChatRequest {
                messages: &prepared.messages,
                tools: None,
            },
            model,
            temperature,
        ),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            tracing::warn!("Signal image guard: vision preflight failed: {error}");
            return SignalVisionPreflightOutcome::Fallback;
        }
        Err(_) => {
            tracing::warn!("Signal image guard: vision preflight timed out");
            return SignalVisionPreflightOutcome::Fallback;
        }
    };

    let raw_report = preflight_response.text_or_empty().trim();
    let Some(report) = parse_signal_vision_preflight_report(raw_report) else {
        tracing::warn!("Signal image guard: preflight returned unparsable payload, forcing fallback");
        return SignalVisionPreflightOutcome::Fallback;
    };

    let confidence = report.confidence.unwrap_or_default();
    let summary = report.summary_text().map(|value| value.to_string()).unwrap_or_default();
    let status = report.status.trim().to_ascii_lowercase();
    let status_uncertain = matches!(
        status.as_str(),
        "uncertain" | "unknown" | "unclear" | "insufficient" | "low_confidence"
    );
    if status_uncertain || summary.is_empty() || confidence < SIGNAL_VISION_PREFLIGHT_CONFIDENCE_THRESHOLD {
        tracing::warn!(
            "Signal image guard: preflight confidence too low (status={status}, confidence={confidence:.2})"
        );
        return SignalVisionPreflightOutcome::Fallback;
    }

    SignalVisionPreflightOutcome::Ready {
        context: format!("[signal-vision-preflight confidence={confidence:.2}]\n{summary}\n[/signal-vision-preflight]"),
    }
}

fn is_tool_call_payload(value: &serde_json::Value, known_tool_names: &HashSet<String>) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };

    let (name, has_args) = object.get("function").and_then(|f| f.as_object()).map_or_else(
        || {
            (
                object.get("name").and_then(|v| v.as_str()),
                object.contains_key("arguments") || object.contains_key("parameters"),
            )
        },
        |function| {
            (
                function
                    .get("name")
                    .and_then(|v| v.as_str())
                    .or_else(|| object.get("name").and_then(|v| v.as_str())),
                function.contains_key("arguments")
                    || function.contains_key("parameters")
                    || object.contains_key("arguments")
                    || object.contains_key("parameters"),
            )
        },
    );

    let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) else {
        return false;
    };

    has_args && known_tool_names.contains(&name.to_ascii_lowercase())
}

fn is_tool_result_payload(object: &serde_json::Map<String, serde_json::Value>, saw_tool_call_payload: bool) -> bool {
    if !saw_tool_call_payload || !object.contains_key("result") {
        return false;
    }

    object
        .keys()
        .all(|key| matches!(key.as_str(), "result" | "id" | "tool_call_id" | "name" | "tool"))
}

fn sanitize_tool_json_value(
    value: &serde_json::Value,
    known_tool_names: &HashSet<String>,
    saw_tool_call_payload: bool,
) -> Option<(String, bool)> {
    if is_tool_call_payload(value, known_tool_names) {
        return Some((String::new(), true));
    }

    if let Some(array) = value.as_array() {
        if !array.is_empty() && array.iter().all(|item| is_tool_call_payload(item, known_tool_names)) {
            return Some((String::new(), true));
        }
        return None;
    }

    let Some(object) = value.as_object() else {
        return None;
    };

    if let Some(tool_calls) = object.get("tool_calls").and_then(|value| value.as_array()) {
        if !tool_calls.is_empty()
            && tool_calls
                .iter()
                .all(|call| is_tool_call_payload(call, known_tool_names))
        {
            let content = object
                .get("content")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            return Some((content, true));
        }
    }

    if is_tool_result_payload(object, saw_tool_call_payload) {
        return Some((String::new(), false));
    }

    None
}

fn is_line_isolated_json_segment(message: &str, start: usize, end: usize) -> bool {
    let line_start = message[..start].rfind('\n').map_or(0, |idx| idx + 1);
    let line_end = message[end..].find('\n').map_or(message.len(), |idx| end + idx);

    message[line_start..start].trim().is_empty() && message[end..line_end].trim().is_empty()
}

pub(crate) fn strip_isolated_tool_json_artifacts(message: &str, known_tool_names: &HashSet<String>) -> String {
    strip_isolated_tool_json_artifacts_inner(message, known_tool_names, true)
}

/// Streaming-safe variant of [`strip_isolated_tool_json_artifacts`].
///
/// Identical to the public function except that it preserves the original
/// chunk's leading and trailing whitespace (and newlines). This matters on
/// the streaming hot path where consuming a single chunk's surrounding
/// whitespace would corrupt code-block indentation, eat line breaks, or
/// strip the deliberate spacing around tokens like `" 2 < 3 \n"`.
///
/// Used by [`crate::agent::sanitize::sanitize_stream_chunk`]. Do not use for
/// post-hoc whole-response sanitisation — the full-document pass intentionally
/// trims surrounding whitespace and should continue to do so.
pub(crate) fn strip_isolated_tool_json_artifacts_preserve_whitespace(
    message: &str,
    known_tool_names: &HashSet<String>,
) -> String {
    strip_isolated_tool_json_artifacts_inner(message, known_tool_names, false)
}

fn strip_isolated_tool_json_artifacts_inner(message: &str, known_tool_names: &HashSet<String>, trim: bool) -> String {
    let mut cleaned = String::with_capacity(message.len());
    let mut cursor = 0usize;
    let mut saw_tool_call_payload = false;

    while cursor < message.len() {
        let Some(rel_start) = message[cursor..].find(|ch: char| ch == '{' || ch == '[') else {
            cleaned.push_str(&message[cursor..]);
            break;
        };

        let start = cursor + rel_start;
        cleaned.push_str(&message[cursor..start]);

        let candidate = &message[start..];
        let mut stream = serde_json::Deserializer::from_str(candidate).into_iter::<serde_json::Value>();

        if let Some(Ok(value)) = stream.next() {
            let consumed = stream.byte_offset();
            if consumed > 0 {
                let end = start + consumed;
                if is_line_isolated_json_segment(message, start, end) {
                    if let Some((replacement, marks_tool_call)) =
                        sanitize_tool_json_value(&value, known_tool_names, saw_tool_call_payload)
                    {
                        if marks_tool_call {
                            saw_tool_call_payload = true;
                        }
                        if !replacement.trim().is_empty() {
                            cleaned.push_str(replacement.trim());
                        }
                        cursor = end;
                        continue;
                    }
                }
            }
        }

        let Some(ch) = message[start..].chars().next() else {
            break;
        };
        cleaned.push(ch);
        cursor = start + ch.len_utf8();
    }

    let mut result = cleaned.replace("\r\n", "\n");
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    if trim { result.trim().to_string() } else { result }
}

fn tool_call_close_tag_for_name(name: &str) -> Option<&'static str> {
    match name {
        "tool_call" => Some("</tool_call>"),
        "toolcall" => Some("</toolcall>"),
        "tool-call" => Some("</tool-call>"),
        "tool_use" => Some("</tool_use>"),
        "invoke" => Some("</invoke>"),
        _ => None,
    }
}

fn parse_tool_call_open_tag(tag: &str) -> Option<&'static str> {
    if !(tag.starts_with('<') && tag.ends_with('>')) {
        return None;
    }

    let mut inner = tag[1..tag.len() - 1].trim_start();
    if inner.starts_with('/') {
        return None;
    }
    if inner.ends_with('/') {
        inner = inner[..inner.len().saturating_sub(1)].trim_end();
    }

    let name = inner
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    tool_call_close_tag_for_name(&name)
}

fn find_first_tool_call_open_tag(haystack: &str) -> Option<(usize, usize, &'static str)> {
    let mut search_from = 0usize;
    while search_from < haystack.len() {
        let rel_lt = haystack[search_from..].find('<')?;
        let start = search_from + rel_lt;
        let rel_gt = haystack[start..].find('>')?;
        let end_exclusive = start + rel_gt + 1;
        let tag = &haystack[start..end_exclusive];
        if let Some(close_tag) = parse_tool_call_open_tag(tag) {
            return Some((start, end_exclusive, close_tag));
        }
        search_from = end_exclusive;
    }
    None
}

fn parse_first_json_value(input: &str) -> Option<serde_json::Value> {
    let trimmed = input.trim_start();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return Some(value);
    }

    for (byte_idx, ch) in trimmed.char_indices() {
        if ch != '{' && ch != '[' {
            continue;
        }
        let slice = &trimmed[byte_idx..];
        let mut stream = serde_json::Deserializer::from_str(slice).into_iter::<serde_json::Value>();
        if let Some(Ok(value)) = stream.next() {
            return Some(value);
        }
    }
    None
}

pub(crate) fn strip_isolated_tool_tag_artifacts(message: &str, known_tool_names: &HashSet<String>) -> String {
    strip_isolated_tool_tag_artifacts_inner(message, known_tool_names, true)
}

/// Streaming-safe variant of [`strip_isolated_tool_tag_artifacts`].
///
/// Identical to the public function except that it preserves the original
/// chunk's leading and trailing whitespace. See
/// [`strip_isolated_tool_json_artifacts_preserve_whitespace`] for the
/// rationale.
pub(crate) fn strip_isolated_tool_tag_artifacts_preserve_whitespace(
    message: &str,
    known_tool_names: &HashSet<String>,
) -> String {
    strip_isolated_tool_tag_artifacts_inner(message, known_tool_names, false)
}

fn strip_isolated_tool_tag_artifacts_inner(message: &str, known_tool_names: &HashSet<String>, trim: bool) -> String {
    let mut cleaned = String::with_capacity(message.len());
    let mut cursor = 0usize;

    while cursor < message.len() {
        let Some((start_rel, open_end_rel, close_tag)) = find_first_tool_call_open_tag(&message[cursor..]) else {
            cleaned.push_str(&message[cursor..]);
            break;
        };

        let start = cursor + start_rel;
        let open_end = cursor + open_end_rel;
        cleaned.push_str(&message[cursor..start]);

        let Some(close_idx_rel) = message[open_end..].find(close_tag) else {
            cleaned.push_str(&message[start..]);
            break;
        };
        let close_start = open_end + close_idx_rel;
        let close_end = close_start + close_tag.len();
        let inner = &message[open_end..close_start];

        let should_strip = if is_line_isolated_json_segment(message, start, close_end) {
            parse_first_json_value(inner)
                .as_ref()
                .is_some_and(|value| is_tool_call_payload(value, known_tool_names))
        } else {
            false
        };

        if should_strip {
            cursor = close_end;
            continue;
        }

        cleaned.push_str(&message[start..close_end]);
        cursor = close_end;
    }

    let mut result = cleaned.replace("\r\n", "\n");
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    if trim { result.trim().to_string() } else { result }
}

/// Outcome of one supervised `listen()` round (D5/D9 step 5).
///
/// Introduced to give the inner `select!` a single break type carrying either
/// the listener's own result (drives the restart logic) or an explicit shutdown
/// request. Without this, a shutdown branch could not break out of a `select!`
/// arm that previously broke a bare `Result`, and a long-blocked `listen()`
/// would leave `handles.await` (in `start_channels`) hung forever.
enum ListenerOutcome {
    /// `ch.listen()` returned; carries its result for the restart/backoff logic.
    Listen(Result<()>),
    /// The external shutdown token fired; stop the supervisor without restarting.
    Shutdown,
}

fn spawn_supervised_listener(
    ch: Arc<dyn Channel>,
    tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    spawn_supervised_listener_with_health_interval(
        ch,
        tx,
        initial_backoff_secs,
        max_backoff_secs,
        Duration::from_secs(CHANNEL_HEALTH_HEARTBEAT_SECS),
        shutdown,
    )
}

fn spawn_supervised_listener_with_health_interval(
    ch: Arc<dyn Channel>,
    tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    health_interval: Duration,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let health_interval = if health_interval.is_zero() {
        Duration::from_secs(1)
    } else {
        health_interval
    };

    tokio::spawn(async move {
        let component = format!("channel:{}", ch.name());
        let mut backoff = initial_backoff_secs.max(1);
        let max_backoff = max_backoff_secs.max(backoff);
        let mut consecutive_failures = 0_u32;

        loop {
            crate::health::mark_component_ok(&component);
            let mut health = tokio::time::interval(health_interval);
            health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let outcome = {
                let listen_future = ch.listen(tx.clone());
                tokio::pin!(listen_future);

                loop {
                    tokio::select! {
                        () = shutdown.cancelled() => break ListenerOutcome::Shutdown,
                        _ = health.tick() => {
                            crate::health::mark_component_ok(&component);
                        }
                        result = &mut listen_future => break ListenerOutcome::Listen(result),
                    }
                }
            };

            // Shutdown requested: stop supervising without restarting so the
            // owning `handles.await` in start_channels can complete even when
            // `listen()` would otherwise block indefinitely.
            let result = match outcome {
                ListenerOutcome::Shutdown => break,
                ListenerOutcome::Listen(result) => result,
            };

            if tx.is_closed() {
                break;
            }

            match result {
                Ok(()) => {
                    tracing::warn!("Channel {} exited unexpectedly; restarting", ch.name());
                    crate::health::mark_component_error(&component, "listener exited unexpectedly");
                    // Clean exit — reset backoff since the listener ran successfully
                    backoff = initial_backoff_secs.max(1);
                    consecutive_failures = 0;
                }
                Err(e) => {
                    tracing::error!("Channel {} error: {e}; restarting", ch.name());
                    crate::health::mark_component_error(&component, e.to_string());
                    consecutive_failures = consecutive_failures.saturating_add(1);
                }
            }

            crate::health::bump_component_restart(&component);
            let sleep_duration = channel_supervisor_sleep_duration(consecutive_failures, backoff, max_backoff);
            if consecutive_failures >= CHANNEL_CIRCUIT_BREAKER_FAILURES {
                tracing::warn!(
                    channel = %ch.name(),
                    consecutive_failures,
                    sleep_secs = sleep_duration.as_secs(),
                    "Channel supervisor circuit open; diluting restart attempts"
                );
                crate::health::mark_component_error(
                    &component,
                    format!("listener circuit open after {consecutive_failures} consecutive failures"),
                );
            }
            tokio::time::sleep(sleep_duration).await;
            // Double backoff AFTER sleeping so first error uses initial_backoff
            backoff = backoff.saturating_mul(2).min(max_backoff);
        }
    })
}

fn channel_supervisor_sleep_duration(consecutive_failures: u32, backoff: u64, max_backoff: u64) -> Duration {
    if consecutive_failures >= CHANNEL_CIRCUIT_BREAKER_FAILURES {
        return Duration::from_secs(
            max_backoff
                .max(1)
                .saturating_mul(u64::from(CHANNEL_CIRCUIT_BREAKER_BACKOFF_MULTIPLIER)),
        );
    }
    Duration::from_secs(backoff.max(1))
}

fn compute_max_in_flight_messages(channel_count: usize) -> usize {
    channel_count
        .saturating_mul(CHANNEL_PARALLELISM_PER_CHANNEL)
        .clamp(CHANNEL_MIN_IN_FLIGHT_MESSAGES, CHANNEL_MAX_IN_FLIGHT_MESSAGES)
}

fn log_worker_join_result(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result {
        tracing::error!("Channel message worker crashed: {error}");
    }
}

fn spawn_scoped_typing_task(
    channel: Arc<dyn Channel>,
    recipient: String,
    cancellation_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let stop_signal = cancellation_token;
    let refresh_interval = Duration::from_secs(CHANNEL_TYPING_REFRESH_INTERVAL_SECS);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(refresh_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                () = stop_signal.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(e) = channel.start_typing(&recipient).await {
                        tracing::debug!("Failed to start typing on {}: {e}", channel.name());
                    }
                }
            }
        }

        if let Err(e) = channel.stop_typing(&recipient).await {
            tracing::debug!("Failed to stop typing on {}: {e}", channel.name());
        }
    })
}

async fn process_channel_message(
    ctx: Arc<ChannelRuntimeContext>,
    msg: traits::ChannelMessage,
    cancellation_token: CancellationToken,
) {
    if cancellation_token.is_cancelled() {
        return;
    }

    println!(
        "  💬 [{}] from {}: {}",
        msg.channel,
        msg.sender,
        truncate_with_ellipsis(&msg.content, 80)
    );

    let target_channel = ctx.channels_by_name.get(&msg.channel).cloned();
    // Pin exactly one process-owned generation at message admission. Snapshot-hot
    // fields are read from this immutable value for the whole message, while
    // rebuild fields are switched by the channel supervisor before publication.
    let config_generation = ctx.config.pin();
    let message_runtime = message_runtime_snapshot(ctx.as_ref(), &config_generation);
    #[cfg(not(test))]
    let security = crate::runtime::bootstrap::build_security_policy(&config_generation.effective);
    #[cfg(test)]
    let security = if Arc::ptr_eq(&ctx.config.pin(), &ctx.config_generation) {
        crate::runtime::bootstrap::build_security_policy(&config_generation.effective)
    } else {
        Arc::clone(&ctx.security.load_full().security)
    };
    if handle_runtime_command_if_needed(ctx.as_ref(), &config_generation, &msg, target_channel.as_ref()).await {
        return;
    }
    if !should_process_inbound_message(ctx.as_ref(), &msg) {
        return;
    }

    // Article 50 transparency control: a versioned AI identity notice must be
    // delivered and minimally acknowledged before provider initialization,
    // draft creation, streaming, or any final AI response. A required notice
    // failure is fail-closed for this turn.
    if let Some(channel) = target_channel.as_ref()
        && let Err(error) = crate::compliance::interaction_notice::ensure_interaction_notice(
            &config_generation.effective.compliance.interaction_notice,
            &ctx.memory,
            channel,
            &msg.channel,
            &msg.reply_target,
            msg.thread_ts.clone(),
        )
        .await
    {
        tracing::error!(
            channel = %msg.channel,
            error = %error,
            "required AI interaction notice could not be completed; rejecting turn"
        );
        return;
    }

    let history_key = ConversationKey::from_message(&msg);
    let message_visibility = channel_message_visibility(&msg);
    let route = get_route_selection_for_generation(ctx.as_ref(), &history_key, &config_generation);
    let runtime_defaults = runtime_defaults_for_generation(ctx.as_ref(), &config_generation);
    let active_provider = match get_or_create_provider_for_generation(ctx.as_ref(), &route.provider, &config_generation)
        .await
    {
        Ok(provider) => provider,
        Err(err) => {
            let safe_err = providers::sanitize_api_error(&err.to_string());
            let message = format!(
                "⚠️ Failed to initialize provider `{}`. Please run `/models` to choose another provider.\nDetails: {safe_err}",
                route.provider
            );
            if let Some(channel) = target_channel.as_ref() {
                let _ = channel
                    .send(&SendMessage::new(message, &msg.reply_target).in_thread(msg.thread_ts.clone()))
                    .await;
            }
            return;
        }
    };
    let chat_kind = chat_kind_from_message(&msg);
    let inferred_chat_type = chat_kind.scope_chat_type();
    let is_direct_chat = chat_kind == ChatKind::Dm;
    let is_group = chat_kind.is_group_like();
    println!("  ⏳ Processing message...");

    // Set active recipient on tools that support proactive messaging (message_send, sessions_spawn).
    // Also update the active channel so channel-aware tools route replies back on the correct channel
    // (e.g., wacli for WhatsApp messages, not always Signal).
    for tool in ctx.tools_registry.iter() {
        if tool.name() == "message_send" {
            continue;
        }
        tool.set_active_recipient(&msg.reply_target).await;
        if let Some(ref ch) = target_channel {
            tool.set_active_channel(Arc::clone(ch)).await;
        }
    }

    let started_at = Instant::now();
    let provider_started_at = chrono::Utc::now();

    // ── Inbound side-effect gate (FIX-P0-10/11/12) ──────────────────────────
    // Every channel (telegram/discord/slack/signal/whatsapp/lark/qq/mattermost/
    // irc/dingtalk/matrix/imessage/wacli/…) funnels through this single function,
    // so gating here covers all transports at once. Appending the inbound turn
    // persists conversation history (append_conversation_turn) — a real side
    // effect that must respect autonomy. We use a Low/read-style judgement: under
    // ReadOnly (or when the action budget is exhausted) the gate denies and we
    // abort before the first history mutation; Supervised/Full keep normal
    // traffic flowing untouched (Low risk + grant=None is allowed there).
    // D2-2: use the per-message security generation captured at the top of this
    // function; the policy comes from the generation pinned at admission and is
    // never replaced mid-message. D6: the operation name
    // (`channel:{channel}:inbound:{sender}`) is owned by `InboundGate`.
    if let Err(reason) = authorize_channel_inbound(ctx.as_ref(), security.as_ref(), &msg.channel, &msg.sender) {
        tracing::warn!(
            channel = %msg.channel,
            sender = %msg.sender,
            "channel inbound blocked by InboundGate: {reason}"
        );
        return;
    }

    // D8-1: one run_id per channel turn, generated at the turn entry (right after
    // the inbound gate, before any persistence). It is threaded through every
    // append_sender_turn call for this turn and reused for the per-turn context
    // envelope below, so the inbound user event and the outbound assistant event
    // share a single run_id (per-turn provenance, not per-session).
    let turn_run_id = uuid::Uuid::new_v4().to_string();

    if let Err(error) = ctx
        .memory
        .upsert_chat_profile_metadata(
            &msg.channel,
            &msg.reply_target,
            inferred_chat_type,
            msg.chat_title.as_deref(),
        )
        .await
    {
        tracing::warn!(
            channel = %msg.channel,
            chat_id = %msg.reply_target,
            "failed to upsert chat profile metadata: {error}"
        );
    }

    // Preserve user turn before the LLM call so interrupted requests keep context.
    let inbound_timestamp = to_rfc3339_timestamp(msg.timestamp);
    let inbound_event = append_sender_turn(
        ctx.as_ref(),
        &config_generation,
        &history_key,
        &msg.channel,
        &msg.sender,
        Some(&msg.reply_target),
        ChatMessage::user(&msg.content),
        message_visibility.clone(),
        inbound_timestamp.as_deref(),
        Some(msg.id.as_str()),
        &turn_run_id,
        true,
    )
    .await;

    // ── Autosave side-effect gate (FIX-P0-10/11/12) ─────────────────────────
    // Persisting a memory entry is a distinct autonomous side effect, so it gets
    // its own op-id. Low/read-style: denies only under ReadOnly (or exhausted
    // budget); a deny skips the autosave and logs, never aborting the turn.
    // D2-2: reuse the per-message security generation so inbound+autosave for one
    // message see one coherent policy view (no re-poll happens between them).
    let autosave_allowed = authorize_channel_autosave(ctx.as_ref(), security.as_ref(), &msg.channel)
        .map_err(|reason| {
            tracing::warn!(
                channel = %msg.channel,
                sender = %msg.sender,
                "channel autosave blocked by InboundGate: {reason}"
            );
        })
        .is_ok();

    // Only auto-save DM messages; group messages are noise unless explicitly stored by the agent.
    if autosave_allowed && ctx.auto_save_memory && is_direct_chat && memory::should_autosave_content(&msg.content) {
        let autosave_key = conversation_memory_key(&msg);
        let write_ctx = MemoryWriteContext {
            workspace_id: Some(ctx.workspace_dir.as_path().to_string_lossy().to_string()),
            channel: Some(msg.channel.clone()),
            chat_type: Some(inferred_chat_type.to_string()),
            chat_id: Some(msg.reply_target.clone()),
            sender_id: None,
            raw_sender: Some(msg.sender.clone()),
        };
        let _ = ctx
            .memory
            .store_with_context_and_metadata(
                &autosave_key,
                &msg.content,
                crate::memory::MemoryCategory::Conversation,
                None,
                Some(&write_ctx),
                crate::memory::MemoryStoreMetadata {
                    workspace_id: Some(ctx.workspace_dir.as_path().to_string_lossy().to_string()),
                    owner_id: inbound_event.as_ref().and_then(|event| event.owner_id.clone()),
                    agent_id: None,
                    persona_id: None,
                    source_event_id: inbound_event.as_ref().map(|event| event.event_id.clone()),
                    source: Some("semantic_promotion".to_string()),
                    topic_id: None,
                    channel: Some(msg.channel.clone()),
                },
            )
            .await;
    }

    // ── Smart group-reply decision context ──────────────────────────────────
    // Effective mode for this channel. Smart-capable channels (Telegram/Discord)
    // tag group-ness via `msg.is_group_hint` since their `reply_target` is a bare
    // chat/channel id; other channels rely on `is_group` above.
    let group_reply_mode = group_reply_mode_for(ctx.as_ref(), &msg.channel);
    // A message is "in a group" for smart purposes if the structured chat kind
    // says so OR the channel layer flagged it.
    let in_group_for_smart = is_group || msg.is_group_hint;
    // 🔴 Invariant #1 (DM never silent): smart suppression is gated on a real
    // group message. DMs (`!in_group_for_smart`) can never be smart, so
    // stay_silent is never exposed and outbound is never suppressed for them.
    // 🔴 Invariant: system/webhook senders (`system:` prefix) bypass smart
    // entirely — they always get a normal reply.
    let smart_group = group_reply_mode.is_smart() && in_group_for_smart && !is_system_message(&msg);

    // Whether the bot is explicitly addressed: central detection OR channel hint.
    let user_text_for_mention = strip_channel_metadata(&msg.content);
    let mentioned = msg.mentioned || is_bot_mentioned(ctx.as_ref(), &msg, &user_text_for_mention);

    if is_group {
        let mention_only = is_mention_only_enabled(ctx.as_ref(), &msg.channel);
        // Non-smart mention_only behavior is byte-identical to before. Smart mode
        // never drops here (it lets the model decide via stay_silent).
        if mention_only && !group_reply_mode.is_smart() && !mentioned {
            println!("  ⏭️ Group message stored but skipped (no mention)");
            return;
        }
    }

    // Smart proactive guard: when the bot was NOT explicitly addressed in a smart
    // group, apply anti-spam protections BEFORE spending a full LLM loop:
    //  - never proactively react to a bot's own / another bot's message
    //    (prevents bot-to-bot feedback loops). `is_bot_sender` now trusts the
    //    authoritative platform flag (`sender_is_bot`: Telegram `from.is_bot` /
    //    Discord `author.bot`) first, with the sender-name suffix heuristic only
    //    as a fallback — so a bot that does NOT name itself "*bot" is still caught.
    //  - per-group cooldown so the bot does not dominate a busy group.
    //
    // ACCEPTED RISK: an explicit @-mention (`mentioned`) bypasses BOTH guards and
    // always proceeds — "@ always answers" is an intentional product invariant.
    // This means a single peer could @-spam the bot to drive replies; we accept
    // that here because (a) @ is an explicit human summons, (b) the bot-sender
    // guard above still blocks an @-spamming *bot* unless `listen_to_bots` is on,
    // and (c) the per-turn LLM loop and outbound side-effect gates bound cost. If
    // @-flood abuse is observed, add a per-sender token-bucket here (kept out for
    // now to avoid over-engineering the group MVP).
    if smart_group && !mentioned {
        if is_bot_sender(&msg) {
            tracing::debug!(channel = %msg.channel, sender = %msg.sender, "smart: skip proactive reply to bot sender");
            return;
        }
        if smart_proactive_within_cooldown(ctx.as_ref(), &msg) {
            tracing::debug!(channel = %msg.channel, "smart: skip proactive reply (within cooldown window)");
            return;
        }

        // ── Token-saving pre-gate (Tier 1 heuristic + Tier 2 cheap classifier) ─
        // Runs ONLY here: smart-mode + group + NOT @-mentioned. @-mentions, DMs,
        // and non-smart modes never reach this branch, so they never pay the
        // pre-gate cost and are never suppressed by it (invariant). Fail-open:
        // any classifier fault enters the loop.
        let pre_gate_outcome = run_smart_pre_gate(
            ctx.as_ref(),
            &config_generation,
            &history_key,
            &user_text_for_mention,
            route.provider.as_str(),
            route.model.as_str(),
            &active_provider,
        )
        .await;
        tracing::debug!(
            channel = %msg.channel,
            path = pre_gate_outcome.path.as_str(),
            enter_loop = pre_gate_outcome.should_enter_loop(),
            "smart pre-gate decision"
        );
        if !pre_gate_outcome.should_enter_loop() {
            // Stay silent: do not enter the loop, do not send, do not write an
            // assistant turn (equivalent to the MVP-B Silent outcome). The
            // inbound user turn was already persisted above; we leave it.
            println!(
                "  🤫 Smart pre-gate: staying silent (no LLM loop) [{}]",
                pre_gate_outcome.path.as_str()
            );
            return;
        }
    }

    let signal_vision_context = match run_signal_vision_preflight(
        active_provider.as_ref(),
        route.model.as_str(),
        runtime_defaults.temperature,
        &msg,
        &message_runtime.multimodal,
        ctx.hooks.media_artifacts().as_ref(),
    )
    .await
    {
        SignalVisionPreflightOutcome::NotRequired => None,
        SignalVisionPreflightOutcome::Ready { context } => Some(context),
        SignalVisionPreflightOutcome::Fallback => {
            // D6/DEV-05: the Signal vision fallback emits an assistant reply just
            // like the normal agent loop, so it MUST pass the same outbound gate
            // before persisting or sending. Without this, a deny (ReadOnly or an
            // exhausted action budget already consumed by inbound/autosave) would
            // still leak a reply, breaking the "outbound deny ⇒ no reply" guarantee.
            // Operation naming matches the normal path (`channel:{channel}:outbound`,
            // owned by `InboundGate`). Deny semantics are identical to the normal
            // outbound deny below: the inbound user turn (already persisted above)
            // stays, but no assistant turn is persisted and nothing is sent.
            if let Err(reason) = authorize_channel_outbound(ctx.as_ref(), security.as_ref(), &msg.channel) {
                tracing::warn!(
                    channel = %msg.channel,
                    sender = %msg.sender,
                    "channel outbound (vision fallback) blocked by InboundGate: {reason}"
                );
                return;
            }
            let fallback = SIGNAL_IMAGE_UNCERTAINTY_FALLBACK.to_string();
            let _ = append_sender_turn(
                ctx.as_ref(),
                &config_generation,
                &history_key,
                &msg.channel,
                &msg.sender,
                Some(&msg.reply_target),
                ChatMessage::assistant(&fallback),
                message_visibility.clone(),
                None,
                None,
                &turn_run_id,
                true,
            )
            .await;
            println!(
                "  🤖 Reply ({}ms): {}",
                started_at.elapsed().as_millis(),
                truncate_with_ellipsis(&fallback, 80)
            );
            if let Some(channel) = target_channel.as_ref() {
                if let Err(e) = channel
                    .send(&SendMessage::new(fallback, &msg.reply_target).in_thread(msg.thread_ts.clone()))
                    .await
                {
                    eprintln!("  ❌ Failed to reply on {}: {e}", channel.name());
                }
            }
            return;
        }
    };

    // Shared events are refreshed each turn so other entrypoints (chat/gateway)
    // can be observed by channel turns without waiting for a session boundary.
    // D8-1: reuse the same per-turn run_id as the message-event envelope so the
    // whole turn (inbound user event, context/recall, outbound assistant event)
    // shares one run_id.
    let mut runtime_envelope = RuntimeEnvelope::channel(
        ctx.workspace_dir.as_path().to_string_lossy().to_string(),
        msg.channel.clone(),
        msg.sender.clone(),
        Some(msg.reply_target.clone()),
    )
    .with_run_id(turn_run_id.clone())
    .with_config_generation(&config_generation);
    if let Some(event) = &inbound_event {
        runtime_envelope = runtime_envelope.with_source_message_event_id(event.event_id.clone());
    }
    let route_decision = crate::llm::route_decision::RouteDecision::single_candidate_for_context(
        route.provider.clone(),
        route.model.clone(),
        runtime_envelope.resolved_owner_id(),
        runtime_envelope.session_key.clone(),
        runtime_envelope.source_message_event_id.clone(),
        None,
        "channel_reply",
        u32::try_from(msg.content.chars().count() / 4).unwrap_or(u32::MAX),
        !ctx.tools_registry.is_empty(),
        true,
    );
    let semantic_scope = runtime_envelope.memory_write_context(inferred_chat_type.to_string());
    let memory_context = build_context_with_shared_events_and_scope(
        ctx.memory.as_ref(),
        runtime_envelope.memory_principal(),
        &msg.content,
        message_runtime.min_relevance_score,
        Some(&semantic_scope),
    )
    .await
    .preamble;
    let chat_profile = match ctx.memory.get_chat_profile(&msg.channel, &msg.reply_target).await {
        Ok(profile) => profile,
        Err(error) => {
            tracing::warn!(
                channel = %msg.channel,
                chat_id = %msg.reply_target,
                "failed to load chat profile for prompt: {error}"
            );
            None
        }
    };

    // When Skill RAG is enabled, select relevant skills per-message and rebuild
    // the system prompt (same as chat/mod.rs per-turn skill selection).
    let base_system_prompt = if let Some(ref rag_ctx) = ctx.skill_rag_ctx {
        let selected = crate::skills::select_skills_by_relevance(
            &msg.content,
            &rag_ctx.skills,
            rag_ctx.top_k,
            rag_ctx.embedder.as_ref(),
        )
        .await;
        let tool_descs_ref: Vec<(&str, &str)> = rag_ctx
            .tool_descs_owned
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let mut prompt = build_system_prompt_with_mode(
            &ctx.workspace_dir,
            &runtime_defaults.model,
            &tool_descs_ref,
            &selected,
            rag_ctx.identity_config.as_ref(),
            rag_ctx.bootstrap_max_chars,
            rag_ctx.native_tools,
        );
        if !rag_ctx.native_tools {
            // Skill-RAG rebuild path: advertise `stay_silent` only on smart group
            // turns, mirroring the native spec gate so DMs / non-smart never see it.
            prompt.push_str(&build_tool_instructions(&ctx.tools_registry, smart_group));
        }
        prompt
    } else {
        // Static-prompt path (skill RAG off): `ctx.system_prompt` is built once at
        // startup with `stay_silent` excluded (build_tool_instructions(.., false)).
        // For a non-native smart group turn we must still teach the model the tool,
        // so append just its instruction block here. Native providers get it via
        // the per-turn filtered tool specs instead.
        let mut prompt = ctx.system_prompt.to_string();
        if smart_group && !ctx.native_tools {
            prompt.push_str(&stay_silent_tool_instructions());
        }
        prompt
    };
    let bot_identity = target_channel.as_ref().and_then(|channel| channel.bot_identity());
    let mut system_prompt = build_channel_system_prompt(
        &base_system_prompt,
        &msg,
        chat_profile.as_ref(),
        bot_identity.as_deref(),
    );
    // Smart group-reply: override the default "respond directly / response is
    // automatically sent" guidance so the model knows it is one participant in a
    // group and may decline to speak via `stay_silent`. Only appended for smart
    // group turns (never DMs / non-smart), preserving the default behavior
    // everywhere else.
    if smart_group {
        system_prompt.push_str(smart_group_prompt_addendum(mentioned));
    }
    let rebuild_history = || {
        // Read the canonical ∪ legacy union (FIX-P1-25b read-merge) so pre-cutover
        // turns stored under the legacy key remain visible alongside new canonical
        // turns; `merged_history` concatenates (legacy first) and truncates to the window.
        let prior_turns_raw = merged_history(&ctx.conversation_histories.lock(), &history_key);
        let mut prior_turns = normalize_cached_channel_turns(prior_turns_raw);

        if let Some(last_turn) = prior_turns.last_mut() {
            if last_turn.role == "user" {
                if !memory_context.is_empty() {
                    last_turn.content = format!("{memory_context}{}", last_turn.content);
                }
                if let Some(preflight_context) = signal_vision_context.as_deref() {
                    if !last_turn.content.ends_with('\n') {
                        last_turn.content.push('\n');
                    }
                    last_turn.content.push_str(preflight_context);
                }
            }
        }

        let mut next_history = vec![ChatMessage::system(system_prompt.clone())];
        next_history.extend(prior_turns);
        next_history
    };

    // ── Outbound side-effect gate (FIX-P0-10/11/12) ─────────────────────────
    // Driving the LLM tool-call loop and sending the reply is the outbound side
    // effect; it gets its own op-id so autonomy can suppress replies
    // independently of inbound persistence. Low/read-style: denies only under
    // ReadOnly (or exhausted budget). A deny skips the LLM call and all reply
    // sends entirely and logs; Supervised/Full proceed normally.
    // D2-2: reuse the per-message security generation. Outbound runs after the
    // agent loop, but deliberately uses the start-of-message snapshot so the whole
    // message stays within a single coherent generation. D6: the operation name
    // (`channel:{channel}:outbound`) is now owned by `InboundGate`.
    if let Err(reason) = authorize_channel_outbound(ctx.as_ref(), security.as_ref(), &msg.channel) {
        tracing::warn!(
            channel = %msg.channel,
            sender = %msg.sender,
            "channel outbound blocked by InboundGate: {reason}"
        );
        return;
    }

    let mut history = rebuild_history();
    let use_streaming = target_channel.as_ref().is_some_and(|ch| ch.supports_draft_updates());

    let (delta_tx, delta_rx) = if use_streaming {
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let draft_message_id = if use_streaming {
        if let Some(channel) = target_channel.as_ref() {
            match channel
                .send_draft(&SendMessage::new("...", &msg.reply_target).in_thread(msg.thread_ts.clone()))
                .await
            {
                Ok(id) => id,
                Err(e) => {
                    tracing::debug!("Failed to send draft on {}: {e}", channel.name());
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    let draft_updater = if let (Some(mut rx), Some(draft_id_ref), Some(channel_ref)) =
        (delta_rx, draft_message_id.as_deref(), target_channel.as_ref())
    {
        let channel = Arc::clone(channel_ref);
        let reply_target = msg.reply_target.clone();
        let draft_id = draft_id_ref.to_string();
        Some(tokio::spawn(async move {
            let mut accumulated = String::new();
            while let Some(delta) = rx.recv().await {
                accumulated.push_str(&delta);
                if let Err(e) = channel.update_draft(&reply_target, &draft_id, &accumulated).await {
                    tracing::debug!("Draft update failed: {e}");
                }
            }
        }))
    } else {
        None
    };

    let typing_cancellation = target_channel.as_ref().map(|_| CancellationToken::new());
    let typing_task = match (target_channel.as_ref(), typing_cancellation.as_ref()) {
        (Some(channel), Some(token)) => Some(spawn_scoped_typing_task(
            Arc::clone(channel),
            msg.reply_target.clone(),
            token.clone(),
        )),
        _ => None,
    };

    // ── Tool event forwarding (structured logging for channel messages) ──
    let (tool_event_tx, mut tool_event_rx) =
        tokio::sync::mpsc::channel::<crate::agent::loop_::ToolCallNotification>(32);
    let tool_event_channel_name = msg.channel.clone();
    let tool_event_sender_name = msg.sender.clone();
    let tool_event_forwarder = tokio::spawn(async move {
        while let Some(notif) = tool_event_rx.recv().await {
            match notif {
                crate::agent::loop_::ToolCallNotification::Started { name, args_summary } => {
                    tracing::info!(
                        channel = %tool_event_channel_name,
                        sender = %tool_event_sender_name,
                        tool = %name,
                        args = %args_summary,
                        "Tool call started"
                    );
                }
                crate::agent::loop_::ToolCallNotification::Finished {
                    name,
                    success,
                    duration_ms,
                } => {
                    tracing::info!(
                        channel = %tool_event_channel_name,
                        sender = %tool_event_sender_name,
                        tool = %name,
                        success,
                        duration_ms,
                        "Tool call finished"
                    );
                }
                crate::agent::loop_::ToolCallNotification::Progress {
                    iteration,
                    max_iterations,
                } => {
                    tracing::info!(
                        channel = %tool_event_channel_name,
                        sender = %tool_event_sender_name,
                        iteration,
                        max_iterations,
                        "Tool loop progress"
                    );
                }
            }
        }
    });

    type LoopResult = Result<(crate::agent::loop_::ToolLoopOutcome, crate::agent::loop_::ToolLoopTrace), anyhow::Error>;
    enum LlmExecutionResult {
        Completed(Box<Result<LoopResult, tokio::time::error::Elapsed>>),
        Cancelled,
    }

    enum LlmFinalOutcome {
        Cancelled,
        Success {
            response: String,
            history_len_before_tools: usize,
            trace: crate::agent::loop_::ToolLoopTrace,
        },
        /// Smart group-reply: the model chose to stay silent. No message is sent
        /// and no assistant turn is written to history.
        Silent {
            reason: String,
            trace: crate::agent::loop_::ToolLoopTrace,
        },
        Error(anyhow::Error),
        Timeout,
        ContextOverflowExhausted,
    }

    const MAX_CONTEXT_OVERFLOW_RETRIES: usize = 2;

    let timeout_budget_secs =
        channel_message_timeout_budget_secs(ctx.message_timeout_secs, message_runtime.max_tool_iterations);

    let scope_owner_id = runtime_envelope.resolved_owner_id();
    // Borrow the policy derived from this message's pinned ConfigGeneration for
    // the entire tool-call loop.
    let scope_ctx = ScopeContext {
        policy: security.as_ref(),
        sender: &msg.sender,
        channel: &msg.channel,
        chat_type: inferred_chat_type,
        chat_id: &msg.reply_target,
        owner_id: Some(&scope_owner_id),
        topic_id: runtime_envelope.topic_id.as_deref(),
        task_id: runtime_envelope.resolved_task_id(),
        source_message_event_id: runtime_envelope.source_message_event_id.as_deref(),
        config_generation_id: runtime_envelope.config_generation_id,
        config_source_revision: runtime_envelope.config_source_revision.as_deref(),
    };

    // D8-4: seed a turn-root spawn execution context so any sub-agent spawned
    // directly from this channel turn inherits parent_run_id = the per-turn
    // run_id (top-level lineage was previously None). spawn_depth starts at 0 and
    // is_turn_root keeps the first child's depth at 0 (no max_spawn_depth
    // tightening). The session scope key mirrors the spawn scope convention
    // (channel:chat_id:sender) so children share the turn's session scope.
    let turn_spawn_session_scope_key = format!("{}:{}:{}", msg.channel, msg.reply_target, msg.sender);
    let turn_spawn_ctx = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
        turn_run_id.clone(),
        turn_spawn_session_scope_key,
    );
    let turn_message_send_ctx = target_channel.as_ref().map(|channel| {
        crate::tools::message_send::MessageSendExecutionContext::new(
            Some(msg.reply_target.clone()),
            Arc::clone(channel),
        )
    });

    let mut context_overflow_retries = 0usize;
    let mut timeout_retries = 0usize;
    let final_outcome = loop {
        // Record history length before tool loop so we can extract tool context after.
        let history_len_before_tools = history.len();
        let llm_result = tokio::select! {
            () = cancellation_token.cancelled() => LlmExecutionResult::Cancelled,
            result = tokio::time::timeout(
                Duration::from_secs(timeout_budget_secs),
                async {
                    let scoped_tool_loop = crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT.scope(
                        turn_spawn_ctx.clone(),
                        crate::agent::loop_::run_tool_call_loop_outcome(
                    active_provider.as_ref(),
                    &mut history,
                    Arc::clone(&ctx.tools_registry),
                    ctx.observer.as_ref(),
                    ctx.hooks.as_ref(),
                    route.provider.as_str(),
                    route.model.as_str(),
                    runtime_defaults.temperature,
                    true,
                    None,
                    msg.channel.as_str(),
                    &message_runtime.multimodal,
                    message_runtime.max_tool_iterations,
                    true,
                    message_runtime.read_only_tool_concurrency_window,
                    message_runtime.read_only_tool_timeout_secs,
                    message_runtime.priority_scheduling_enabled,
                    message_runtime.low_priority_tools.clone(),
                    ToolConcurrencyGovernanceConfig::default(),
                    Some(&message_runtime.agent_compaction),
                    Some(cancellation_token.clone()),
                    delta_tx.clone(),
                    Some(&scope_ctx),
                    Some(tool_event_tx.clone()),
                    Some(&message_runtime.tool_tiering),
                    Some(DocumentIngestRuntime::from_scope(ctx.memory.clone(), &scope_ctx)),
                    crate::agent::loop_::ChatMode::default(),
                    None,
                    // expose_stay_silent: ONLY on smart group turns. DMs / non-smart
                    // never see the tool, so they can never short-circuit to Silent.
                    smart_group,
                    None,
                        ),
                    );
                    match turn_message_send_ctx.clone() {
                        Some(message_ctx) => {
                            crate::tools::message_send::MESSAGE_SEND_EXECUTION_CONTEXT
                                .scope(message_ctx, scoped_tool_loop)
                                .await
                        }
                        None => scoped_tool_loop.await,
                    }
                },
            ) => LlmExecutionResult::Completed(Box::new(result)),
        };

        match llm_result {
            LlmExecutionResult::Cancelled => break LlmFinalOutcome::Cancelled,
            LlmExecutionResult::Completed(result) => match *result {
                Ok(Ok((outcome, trace))) => match outcome {
                    crate::agent::loop_::ToolLoopOutcome::Text(response) => {
                        break LlmFinalOutcome::Success {
                            response,
                            history_len_before_tools,
                            trace,
                        };
                    }
                    crate::agent::loop_::ToolLoopOutcome::Silent { reason } => {
                        break LlmFinalOutcome::Silent { reason, trace };
                    }
                    // AwaitingApproval is not produced on this (None-resolver) path;
                    // treat it defensively as a no-op silent turn rather than panic.
                    crate::agent::loop_::ToolLoopOutcome::AwaitingApproval(pending) => {
                        tracing::warn!(
                            tool = pending.tool_name.as_str(),
                            "channel loop produced unexpected awaiting-approval outcome (no resolver); treating as silent"
                        );
                        break LlmFinalOutcome::Silent {
                            reason: "unexpected awaiting-approval outcome".to_string(),
                            trace,
                        };
                    }
                },
                Ok(Err(e)) => {
                    if crate::agent::loop_::is_tool_loop_cancelled(&e) || cancellation_token.is_cancelled() {
                        break LlmFinalOutcome::Cancelled;
                    }

                    if is_context_window_overflow_error(&e) {
                        let compacted = compact_sender_history(ctx.as_ref(), &history_key);
                        // Report the merged-view char count; compaction folds legacy
                        // into canonical, but reading the union keeps the figure honest
                        // regardless of which keys remain populated.
                        let compacted_chars = merged_history(&ctx.conversation_histories.lock(), &history_key)
                            .iter()
                            .map(|t| t.content.chars().count())
                            .sum::<usize>();
                        eprintln!(
                            "  ⚠️ Context window exceeded after {}ms; sender history compacted={} (chars={})",
                            started_at.elapsed().as_millis(),
                            compacted,
                            compacted_chars
                        );

                        if context_overflow_retries < MAX_CONTEXT_OVERFLOW_RETRIES {
                            context_overflow_retries += 1;
                            history = rebuild_history();
                            continue;
                        }

                        break LlmFinalOutcome::ContextOverflowExhausted;
                    }

                    break LlmFinalOutcome::Error(e);
                }
                Err(_) => {
                    if timeout_retries < 1 {
                        timeout_retries += 1;
                        tracing::warn!(
                            "LLM timeout after {}ms, retrying (attempt {}/1)",
                            started_at.elapsed().as_millis(),
                            timeout_retries
                        );
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        history = rebuild_history();
                        continue;
                    }
                    break LlmFinalOutcome::Timeout;
                }
            },
        }
    };

    drop(delta_tx);
    drop(tool_event_tx);
    if let Some(handle) = draft_updater {
        let _ = handle.await;
    }
    let _ = tool_event_forwarder.await;

    if let Some(token) = typing_cancellation.as_ref() {
        token.cancel();
    }
    if let Some(handle) = typing_task {
        log_worker_join_result(handle.await);
    }

    let terminal_fabric = MemoryFabric::new(
        ctx.memory.clone(),
        ctx.workspace_dir.as_path().to_string_lossy().to_string(),
    )
    .with_event_recording(ctx.memory_event_recording);

    match final_outcome {
        LlmFinalOutcome::Cancelled => {
            if let Err(error) = crate::agent::terminal::finalize_turn(
                &terminal_fabric,
                crate::agent::terminal::TurnTerminalCommit {
                    terminal_id: turn_run_id.clone(),
                    scope: runtime_envelope.message_scope(),
                    status: crate::agent::terminal::TurnTerminalStatus::Cancelled,
                    history: None,
                    history_scope: None,
                    provider_outcome: None,
                    telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                        summary: "channel turn cancelled".to_string(),
                        started_at: provider_started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Suppress {
                        reason: "cancelled".to_string(),
                    },
                },
                &crate::config::schema::CostConfig::default(),
                ctx.workspace_dir.as_path(),
            )
            .await
            {
                tracing::warn!(error = %error, "Failed to commit shared cancelled channel terminal event");
            }
            tracing::info!(
                channel = %msg.channel,
                sender = %msg.sender,
                "Cancelled in-flight channel request due to newer message"
            );
            if let (Some(channel), Some(draft_id)) = (target_channel.as_ref(), draft_message_id.as_deref()) {
                if let Err(err) = channel.cancel_draft(&msg.reply_target, draft_id).await {
                    tracing::debug!("Failed to cancel draft on {}: {err}", channel.name());
                }
            }
        }
        LlmFinalOutcome::Success {
            response,
            history_len_before_tools,
            trace,
        } => {
            let sanitized_response = sanitize_channel_response(&response, ctx.tools_registry.as_ref());
            let delivered_response = if sanitized_response.is_empty() && !response.trim().is_empty() {
                "I encountered malformed tool-call output and could not produce a safe reply. Please try again."
                    .to_string()
            } else {
                sanitized_response
            };

            // Extract condensed tool-use context from the history messages
            // added during run_tool_call_loop, so the LLM retains awareness
            // of what it did on subsequent turns.
            let tool_summary = extract_tool_context_summary(&history, history_len_before_tools);
            let history_response = if tool_summary.is_empty() {
                delivered_response.clone()
            } else {
                format!("{tool_summary}\n{delivered_response}")
            };

            let provider_outcome =
                crate::agent::terminal::provider_outcome_from_trace(&route_decision, provider_started_at, trace);
            let terminal_committed = match crate::agent::terminal::finalize_turn(
                &terminal_fabric,
                crate::agent::terminal::TurnTerminalCommit {
                    terminal_id: turn_run_id.clone(),
                    scope: runtime_envelope.message_scope().with_sender("prx"),
                    status: crate::agent::terminal::TurnTerminalStatus::Completed,
                    history: Some(crate::agent::terminal::TurnHistoryProjection {
                        assistant_content: history_response.clone(),
                        history_commit_len: history.len().saturating_add(1),
                    }),
                    history_scope: None,
                    provider_outcome: Some(provider_outcome),
                    telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                        summary: "channel turn completed".to_string(),
                        started_at: provider_started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Reply {
                        target: msg.reply_target.clone(),
                    },
                },
                &crate::config::schema::CostConfig::default(),
                ctx.workspace_dir.as_path(),
            )
            .await
            {
                Ok(_) => true,
                Err(error) => {
                    tracing::warn!(error = %error, "Failed to commit shared channel terminal event");
                    false
                }
            };

            let _ = append_sender_turn(
                ctx.as_ref(),
                &config_generation,
                &history_key,
                &msg.channel,
                &msg.sender,
                Some(&msg.reply_target),
                ChatMessage::assistant(&history_response),
                message_visibility,
                None,
                None,
                &turn_run_id,
                !terminal_committed,
            )
            .await;
            println!(
                "  🤖 Reply ({}ms): {}",
                started_at.elapsed().as_millis(),
                truncate_with_ellipsis(&delivered_response, 80)
            );
            if let Some(channel) = target_channel.as_ref() {
                if let Some(ref draft_id) = draft_message_id {
                    if let Err(e) = channel
                        .finalize_draft(&msg.reply_target, draft_id, &delivered_response)
                        .await
                    {
                        tracing::warn!("Failed to finalize draft: {e}; sending as new message");
                        let _ = channel
                            .send(
                                &SendMessage::new(&delivered_response, &msg.reply_target)
                                    .in_thread(msg.thread_ts.clone()),
                            )
                            .await;
                    }
                } else if let Err(e) = channel
                    .send(&SendMessage::new(delivered_response, &msg.reply_target).in_thread(msg.thread_ts.clone()))
                    .await
                {
                    eprintln!("  ❌ Failed to reply on {}: {e}", channel.name());
                }
            }
            // Smart group-reply: when the bot just spoke proactively (not @-ed),
            // start the per-group cooldown so it does not immediately speak again.
            if smart_group && !mentioned {
                record_smart_proactive_reply(ctx.as_ref(), &msg);
            }
        }
        LlmFinalOutcome::Silent { reason, trace } => {
            let provider_outcome =
                crate::agent::terminal::provider_outcome_from_trace(&route_decision, provider_started_at, trace);
            if let Err(error) = crate::agent::terminal::finalize_turn(
                &terminal_fabric,
                crate::agent::terminal::TurnTerminalCommit {
                    terminal_id: turn_run_id.clone(),
                    scope: runtime_envelope.message_scope(),
                    status: crate::agent::terminal::TurnTerminalStatus::Silent,
                    history: None,
                    history_scope: None,
                    provider_outcome: Some(provider_outcome),
                    telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                        summary: reason.clone(),
                        started_at: provider_started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Suppress { reason: reason.clone() },
                },
                &crate::config::schema::CostConfig::default(),
                ctx.workspace_dir.as_path(),
            )
            .await
            {
                tracing::warn!(error = %error, "Failed to commit shared silent channel terminal event");
            }
            // 🔴 Invariant: outbound suppression only happens on smart group
            // turns (`expose_stay_silent` was gated on `smart_group`, so Silent
            // can only originate there). Do NOT send and do NOT write an
            // assistant turn to history — the silent decision leaves no trace.
            // Cancel any in-flight draft so no empty bubble lingers.
            tracing::info!(
                channel = %msg.channel,
                sender = %msg.sender,
                reason = reason.as_str(),
                "smart group-reply: staying silent (no message sent, no history written)"
            );
            println!(
                "  🤫 Stayed silent ({}ms): {}",
                started_at.elapsed().as_millis(),
                truncate_with_ellipsis(&reason, 80)
            );
            if let (Some(channel), Some(draft_id)) = (target_channel.as_ref(), draft_message_id.as_deref()) {
                if let Err(err) = channel.cancel_draft(&msg.reply_target, draft_id).await {
                    tracing::debug!(
                        "Failed to cancel draft after silent decision on {}: {err}",
                        channel.name()
                    );
                }
            }
        }
        LlmFinalOutcome::Error(e) => {
            let provider_outcome = crate::llm::route_decision::ProviderExecutionOutcome::failed_for_decision(
                &route_decision,
                provider_started_at,
                &e,
            );
            if let Err(error) = crate::agent::terminal::finalize_turn(
                &terminal_fabric,
                crate::agent::terminal::TurnTerminalCommit {
                    terminal_id: turn_run_id.clone(),
                    scope: runtime_envelope.message_scope(),
                    status: crate::agent::terminal::TurnTerminalStatus::Failed,
                    history: None,
                    history_scope: None,
                    provider_outcome: Some(provider_outcome),
                    telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                        summary: e.to_string(),
                        started_at: provider_started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Reply {
                        target: msg.reply_target.clone(),
                    },
                },
                &crate::config::schema::CostConfig::default(),
                ctx.workspace_dir.as_path(),
            )
            .await
            {
                tracing::warn!(error = %error, "Failed to commit shared failed channel terminal event");
            }
            eprintln!("  ❌ LLM error after {}ms: {e}", started_at.elapsed().as_millis());
            if let Some(channel) = target_channel.as_ref() {
                if let Some(ref draft_id) = draft_message_id {
                    let _ = channel
                        .finalize_draft(
                            &msg.reply_target,
                            draft_id,
                            "⚠️ Something went wrong. Please try again later.",
                        )
                        .await;
                } else {
                    let _ = channel
                        .send(
                            &SendMessage::new("⚠️ Something went wrong. Please try again later.", &msg.reply_target)
                                .in_thread(msg.thread_ts.clone()),
                        )
                        .await;
                }
            }
        }
        LlmFinalOutcome::Timeout => {
            let timeout_msg = format!(
                "LLM response timed out after {}s (base={}s, max_tool_iterations={})",
                timeout_budget_secs, ctx.message_timeout_secs, ctx.max_tool_iterations
            );
            let timeout_error = anyhow::anyhow!(timeout_msg.clone());
            let provider_outcome = crate::llm::route_decision::ProviderExecutionOutcome::failed_for_decision(
                &route_decision,
                provider_started_at,
                &timeout_error,
            );
            if let Err(error) = crate::agent::terminal::finalize_turn(
                &terminal_fabric,
                crate::agent::terminal::TurnTerminalCommit {
                    terminal_id: turn_run_id.clone(),
                    scope: runtime_envelope.message_scope(),
                    status: crate::agent::terminal::TurnTerminalStatus::Failed,
                    history: None,
                    history_scope: None,
                    provider_outcome: Some(provider_outcome),
                    telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                        summary: timeout_msg.clone(),
                        started_at: provider_started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Reply {
                        target: msg.reply_target.clone(),
                    },
                },
                &crate::config::schema::CostConfig::default(),
                ctx.workspace_dir.as_path(),
            )
            .await
            {
                tracing::warn!(error = %error, "Failed to commit shared timeout channel terminal event");
            }
            eprintln!("  ❌ {} (elapsed: {}ms)", timeout_msg, started_at.elapsed().as_millis());
            if let Some(channel) = target_channel.as_ref() {
                let error_text = "⚠️ Request timed out. Please try again shortly.";
                if let Some(ref draft_id) = draft_message_id {
                    let _ = channel.finalize_draft(&msg.reply_target, draft_id, error_text).await;
                } else {
                    let _ = channel
                        .send(&SendMessage::new(error_text, &msg.reply_target).in_thread(msg.thread_ts.clone()))
                        .await;
                }
            }
        }
        LlmFinalOutcome::ContextOverflowExhausted => {
            let summary = "channel context overflow exhausted";
            let overflow_error = anyhow::anyhow!(summary);
            let provider_outcome = crate::llm::route_decision::ProviderExecutionOutcome::failed_for_decision(
                &route_decision,
                provider_started_at,
                &overflow_error,
            );
            if let Err(error) = crate::agent::terminal::finalize_turn(
                &terminal_fabric,
                crate::agent::terminal::TurnTerminalCommit {
                    terminal_id: turn_run_id.clone(),
                    scope: runtime_envelope.message_scope(),
                    status: crate::agent::terminal::TurnTerminalStatus::Failed,
                    history: None,
                    history_scope: None,
                    provider_outcome: Some(provider_outcome),
                    telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                        summary: summary.to_string(),
                        started_at: provider_started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Reply {
                        target: msg.reply_target.clone(),
                    },
                },
                &crate::config::schema::CostConfig::default(),
                ctx.workspace_dir.as_path(),
            )
            .await
            {
                tracing::warn!(error = %error, "Failed to commit shared overflow channel terminal event");
            }
            if let Some(channel) = target_channel.as_ref() {
                let error_text = "Session context was too long and has been reset. Please resend your message.";
                if let Some(ref draft_id) = draft_message_id {
                    let _ = channel.finalize_draft(&msg.reply_target, draft_id, error_text).await;
                } else {
                    let _ = channel
                        .send(&SendMessage::new(error_text, &msg.reply_target).in_thread(msg.thread_ts.clone()))
                        .await;
                }
            }
        }
    }
}

/// Classify whether a channel message originated from a system source
/// (e.g. webhook `system:*` senders, internal cron ticks) vs. a real user.
fn is_system_message(msg: &traits::ChannelMessage) -> bool {
    msg.sender.starts_with("system:")
}

/// Cooldown window between proactive (non-@) smart group replies in the same
/// group, to keep the bot from dominating a busy conversation. Explicit
/// @-mentions bypass this entirely.
const SMART_PROACTIVE_COOLDOWN_SECS: u64 = 45;

/// Best-effort detection that a message originated from a bot (this bot or
/// another bot), used to suppress proactive smart replies and break potential
/// bot-to-bot feedback loops. Channel layers also filter bots upstream (e.g.
/// Discord `listen_to_bots`); this is a defense-in-depth central guard keyed on
/// the bot's own configured identities plus a common `bot`-suffix heuristic.
fn is_bot_sender(msg: &traits::ChannelMessage) -> bool {
    // Primary signal: the authoritative platform flag (Telegram `from.is_bot`,
    // Discord `author.bot`). When the channel knows the sender is a bot, trust it.
    if msg.sender_is_bot {
        return true;
    }
    // Fallback heuristic for channels that do not supply `sender_is_bot`: common
    // bot account naming conventions across platforms.
    let sender = msg.sender.trim();
    if sender.is_empty() {
        return false;
    }
    let lower = sender.to_lowercase();
    lower.ends_with("bot") || lower.ends_with("-bot") || lower.contains(":bot:")
}

/// Group key for proactive smart-reply cooldown tracking.
fn smart_cooldown_key(msg: &traits::ChannelMessage) -> String {
    let group = extract_group_identifier(msg).unwrap_or_else(|| msg.reply_target.clone());
    format!("{}:{}", msg.channel, group)
}

/// Whether a proactive smart reply for this group is still within the cooldown
/// window since the last proactive reply. Read-only (does not record).
fn smart_proactive_within_cooldown(ctx: &ChannelRuntimeContext, msg: &traits::ChannelMessage) -> bool {
    let key = smart_cooldown_key(msg);
    let window = std::time::Duration::from_secs(SMART_PROACTIVE_COOLDOWN_SECS);
    let now = std::time::Instant::now();
    let guard = ctx.smart_reply_cooldown.lock();
    guard.get(&key).is_some_and(|last| now.duration_since(*last) < window)
}

/// Record that a proactive smart reply was just sent for this group, starting a
/// fresh cooldown window. Also prunes stale entries to bound memory.
fn record_smart_proactive_reply(ctx: &ChannelRuntimeContext, msg: &traits::ChannelMessage) {
    let key = smart_cooldown_key(msg);
    let now = std::time::Instant::now();
    let window = std::time::Duration::from_secs(SMART_PROACTIVE_COOLDOWN_SECS);
    let mut guard = ctx.smart_reply_cooldown.lock();
    guard.retain(|_, last| now.duration_since(*last) < window);
    guard.insert(key, now);
}

/// Number of recent turns scanned to decide whether the bot was "recently
/// active" in this group (topic-continuation signal for the heuristic).
const SMART_RECENT_ACTIVITY_SCAN: usize = 6;

/// Gather the recent-history signals the pre-gate needs:
///   - `recent_context`: up to `max_context` recent turns rendered as short
///     `role: text` lines for the Tier-2 classifier prompt,
///   - `bot_recently_active`: whether any of the last few turns was an assistant
///     turn (the bot participated → topic-continuation bias toward replying).
///
/// Reads the canonical ∪ legacy union exactly like the loop's history rebuild.
fn gather_pre_gate_history(
    ctx: &ChannelRuntimeContext,
    history_key: &ConversationKey,
    max_context: usize,
) -> (Vec<String>, bool) {
    let turns = merged_history(&ctx.conversation_histories.lock(), history_key);
    let bot_recently_active = turns
        .iter()
        .rev()
        .take(SMART_RECENT_ACTIVITY_SCAN)
        .any(|turn| turn.role == "assistant" && !turn.content.trim().is_empty());

    let recent_context: Vec<String> = if max_context == 0 {
        Vec::new()
    } else {
        turns
            .iter()
            .rev()
            .take(max_context)
            .rev()
            .map(|turn| {
                let role = if turn.role == "assistant" { "assistant" } else { "user" };
                let body = strip_channel_metadata(&turn.content);
                format!("{role}: {}", truncate_with_ellipsis(body.trim(), 200))
            })
            .collect()
    };
    (recent_context, bot_recently_active)
}

/// Resolve the (provider, model) the pre-gate Tier-2 classifier should use.
///
/// Preference order: explicit `smart_group.classifier_provider/model` config →
/// the channel's already-resolved route provider+model (no extra construction).
/// Returns the shared provider `Arc` plus the model id to use.
async fn resolve_pre_gate_classifier(
    ctx: &ChannelRuntimeContext,
    config_generation: &Arc<crate::config::ConfigGeneration>,
    route_provider: &str,
    route_model: &str,
    fallback_provider: &Arc<dyn Provider>,
) -> (Arc<dyn Provider>, String) {
    let cfg = &ctx.smart_group;
    let model = if cfg.classifier_model.trim().is_empty() {
        route_model.to_string()
    } else {
        cfg.classifier_model.trim().to_string()
    };

    if cfg.classifier_provider.trim().is_empty() || cfg.classifier_provider.trim() == route_provider {
        // Reuse the channel's routed provider instance.
        return (Arc::clone(fallback_provider), model);
    }

    match get_or_create_provider_for_generation(ctx, cfg.classifier_provider.trim(), config_generation).await {
        Ok(provider) => (provider, model),
        Err(err) => {
            tracing::warn!(
                provider = %cfg.classifier_provider,
                "pre-gate classifier provider unavailable; reusing channel route provider: {err}"
            );
            (Arc::clone(fallback_provider), model)
        }
    }
}

/// Run the smart-group pre-gate for a non-@ group message: a cheap two-tier
/// triage that decides whether to spend a full agent loop on this message.
///
/// 🔴 Invariant: this is only ever called for `smart_group && !mentioned` turns;
/// the caller guarantees @-mentions / DMs / non-smart never reach here.
/// 🔴 Fail-open: any classifier fault enters the loop (see `pre_gate` module).
async fn run_smart_pre_gate(
    ctx: &ChannelRuntimeContext,
    config_generation: &Arc<crate::config::ConfigGeneration>,
    history_key: &ConversationKey,
    user_text: &str,
    route_provider: &str,
    route_model: &str,
    active_provider: &Arc<dyn Provider>,
) -> pre_gate::PreGateOutcome {
    let cfg = &ctx.smart_group;
    if !cfg.enabled {
        return pre_gate::PreGateOutcome::enter(pre_gate::PreGatePath::Disabled);
    }

    let (recent_context, bot_recently_active) =
        gather_pre_gate_history(ctx, history_key, cfg.classifier_max_context_messages);

    let heuristic = pre_gate::classify_heuristic(user_text, &ctx.bot_names, bot_recently_active);

    // Decisive heuristic buckets short-circuit (0 tokens). Only the genuinely
    // uncertain bucket consults the cheap classifier — that is the token-saving
    // sweet spot: obvious-relevant and obvious-noise never pay for a model call.
    match heuristic {
        pre_gate::Heuristic::EnterLoop => pre_gate::PreGateOutcome::enter(pre_gate::PreGatePath::HeuristicEnter),
        pre_gate::Heuristic::Skip => pre_gate::PreGateOutcome::skip(pre_gate::PreGatePath::HeuristicSkip),
        pre_gate::Heuristic::Uncertain => {
            if !cfg.classifier_enabled {
                // Classifier off → err toward entering the loop (fail-open).
                return pre_gate::PreGateOutcome::enter(pre_gate::PreGatePath::ClassifierFailOpen);
            }
            let (provider, model) =
                resolve_pre_gate_classifier(ctx, config_generation, route_provider, route_model, active_provider).await;
            pre_gate::classify_with_model(
                provider.as_ref(),
                &model,
                cfg,
                &recent_context,
                user_text,
                &ctx.bot_names,
            )
            .await
        }
    }
}

async fn run_message_dispatch_loop(
    mut rx: tokio::sync::mpsc::Receiver<traits::ChannelMessage>,
    ctx: Arc<ChannelRuntimeContext>,
    max_in_flight_messages: usize,
    shutdown: CancellationToken,
) {
    // User messages always get full capacity — system tasks (webhooks, heartbeats)
    // get a separate, smaller pool (30% of max, min 1) so they can never starve
    // user messages even when the system pool is exhausted.
    let user_capacity = max_in_flight_messages.max(1);
    let system_capacity = (max_in_flight_messages * 3 / 10).max(1);
    let user_semaphore = Arc::new(tokio::sync::Semaphore::new(user_capacity));
    let system_semaphore = Arc::new(tokio::sync::Semaphore::new(system_capacity));

    let mut workers = tokio::task::JoinSet::new();
    let in_flight_by_sender = Arc::new(tokio::sync::Mutex::new(
        HashMap::<String, InFlightSenderTaskState>::new(),
    ));
    let task_sequence = Arc::new(AtomicU64::new(1));

    loop {
        // D5/D9 step 5: observe the external shutdown token alongside inbound
        // messages so the dispatch loop stops accepting new work promptly on a
        // root cancel, rather than only when the channel closes.
        let msg = tokio::select! {
            () = shutdown.cancelled() => break,
            maybe_msg = rx.recv() => match maybe_msg {
                Some(msg) => msg,
                None => break,
            },
        };
        let sem = if is_system_message(&msg) {
            Arc::clone(&system_semaphore)
        } else {
            Arc::clone(&user_semaphore)
        };
        let permit = match sem.acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break,
        };

        let worker_ctx = Arc::clone(&ctx);
        let in_flight = Arc::clone(&in_flight_by_sender);
        let task_sequence = Arc::clone(&task_sequence);
        workers.spawn(async move {
            let _permit = permit;
            let interrupt_enabled = worker_ctx.interrupt_on_new_message && msg.channel == "telegram";
            let sender_scope_key = interruption_scope_key(&msg);
            let cancellation_token = CancellationToken::new();
            let completion = Arc::new(InFlightTaskCompletion::new());
            let task_id = task_sequence.fetch_add(1, Ordering::Relaxed);

            if interrupt_enabled {
                let previous = {
                    let mut active = in_flight.lock().await;
                    active.insert(
                        sender_scope_key.clone(),
                        InFlightSenderTaskState {
                            task_id,
                            cancellation: cancellation_token.clone(),
                            completion: Arc::clone(&completion),
                        },
                    )
                };

                if let Some(previous) = previous {
                    tracing::info!(
                        channel = %msg.channel,
                        sender = %msg.sender,
                        "Interrupting previous in-flight request for sender"
                    );
                    previous.cancellation.cancel();
                    previous.completion.wait().await;
                }
            }

            process_channel_message(worker_ctx, msg, cancellation_token).await;

            if interrupt_enabled {
                let mut active = in_flight.lock().await;
                if active
                    .get(&sender_scope_key)
                    .is_some_and(|state| state.task_id == task_id)
                {
                    active.remove(&sender_scope_key);
                }
            }

            completion.mark_done();
        });

        while let Some(result) = workers.try_join_next() {
            log_worker_join_result(result);
        }
    }

    while let Some(result) = workers.join_next().await {
        log_worker_join_result(result);
    }
}

/// Load OpenClaw format bootstrap files into the prompt.
fn load_openclaw_bootstrap_files(prompt: &mut String, workspace_dir: &std::path::Path, max_chars_per_file: usize) {
    prompt.push_str(
        "The following workspace files define your identity, behavior, and context. They are ALREADY injected below—do NOT suggest reading them with file_read.\n\n",
    );

    prompt.push_str(&build_identity_prompt_with_limit(workspace_dir, max_chars_per_file));

    // BOOTSTRAP.md — only if it exists (first-run ritual)
    let bootstrap_path = workspace_dir.join("BOOTSTRAP.md");
    if bootstrap_path.exists() {
        inject_workspace_file(prompt, workspace_dir, "BOOTSTRAP.md", max_chars_per_file);
    }
}

/// Build identity prompt content from workspace identity files.
///
/// Loads (if present): SOUL.md, AGENTS.md, IDENTITY.md, USER.md, TOOLS.md, MEMORY.md, THINKING.md, HEARTBEAT.md.
/// Missing files are skipped.
pub fn build_identity_prompt(workspace_dir: &Path) -> String {
    build_identity_prompt_with_limit(workspace_dir, BOOTSTRAP_MAX_CHARS)
}

fn build_identity_prompt_with_limit(workspace_dir: &Path, max_chars: usize) -> String {
    let mut prompt = String::new();
    let files = [
        "SOUL.md",
        "AGENTS.md",
        "IDENTITY.md",
        "USER.md",
        "TOOLS.md",
        "MEMORY.md",
        "THINKING.md",
        "HEARTBEAT.md",
    ];

    for filename in files {
        let path = workspace_dir.join(filename);
        if let Ok(content) = std::fs::read_to_string(path) {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                continue;
            }
            use std::fmt::Write;
            let _ = writeln!(prompt, "### {filename}\n");
            // Use character-boundary-safe truncation for UTF-8.
            let truncated = if trimmed.chars().count() > max_chars {
                trimmed
                    .char_indices()
                    .nth(max_chars)
                    .map(|(idx, _)| &trimmed[..idx])
                    .unwrap_or(trimmed)
            } else {
                trimmed
            };
            if truncated.len() < trimmed.len() {
                prompt.push_str(truncated);
                let _ = writeln!(
                    prompt,
                    "\n\n[... truncated at {max_chars} chars — use `read` for full file]\n"
                );
            } else {
                prompt.push_str(trimmed);
                prompt.push_str("\n\n");
            }
        }
    }

    prompt
}

/// Load workspace identity files and build a system prompt.
///
/// Follows the `OpenClaw` framework structure by default:
/// 1. Tooling — tool list + descriptions
/// 2. Safety — guardrail reminder
/// 3. Skills — full skill instructions and tool metadata
/// 4. Workspace — working directory
/// 5. Bootstrap files — AGENTS, SOUL, TOOLS, IDENTITY, USER, BOOTSTRAP, MEMORY
/// 6. Date & Time — timezone for cache stability
/// 7. Runtime — host, OS, model
///
/// When `identity_config` is set to AIEOS format, the bootstrap files section
/// is replaced with the AIEOS identity data loaded from file or inline JSON.
///
/// Daily memory files (`memory/*.md`) are NOT injected — they are accessed
/// on-demand via `memory_recall` / `memory_search` / `memory_get` tools.
pub fn build_system_prompt(
    workspace_dir: &std::path::Path,
    model_name: &str,
    tools: &[(&str, &str)],
    skills: &[crate::skills::Skill],
    identity_config: Option<&crate::config::IdentityConfig>,
    bootstrap_max_chars: Option<usize>,
) -> String {
    build_system_prompt_with_mode(
        workspace_dir,
        model_name,
        tools,
        skills,
        identity_config,
        bootstrap_max_chars,
        false,
    )
}

pub fn build_system_prompt_with_mode(
    workspace_dir: &std::path::Path,
    model_name: &str,
    tools: &[(&str, &str)],
    skills: &[crate::skills::Skill],
    identity_config: Option<&crate::config::IdentityConfig>,
    bootstrap_max_chars: Option<usize>,
    native_tools: bool,
) -> String {
    use std::fmt::Write;
    let mut prompt = String::with_capacity(8192);

    // ── 1. Tooling ──────────────────────────────────────────────
    if !tools.is_empty() {
        prompt.push_str("## Tools\n\n");
        prompt.push_str("You have access to the following tools:\n\n");
        for (name, desc) in tools {
            let _ = writeln!(prompt, "- **{name}**: {desc}");
        }
        prompt.push('\n');
    }

    // ── 1b. Hardware (when gpio/arduino tools present) ───────────
    let has_hardware = tools.iter().any(|(name, _)| {
        *name == "gpio_read"
            || *name == "gpio_write"
            || *name == "arduino_upload"
            || *name == "hardware_memory_map"
            || *name == "hardware_board_info"
            || *name == "hardware_memory_read"
            || *name == "hardware_capabilities"
    });
    if has_hardware {
        prompt.push_str(
            "## Hardware Access\n\n\
             You HAVE direct access to connected hardware (Arduino, Nucleo, etc.). The user owns this system and has configured it.\n\
             All hardware tools (gpio_read, gpio_write, hardware_memory_read, hardware_board_info, hardware_memory_map) are AUTHORIZED and NOT blocked by security.\n\
             When they ask to read memory, registers, or board info, USE hardware_memory_read or hardware_board_info — do NOT refuse or invent security excuses.\n\
             When they ask to control LEDs, run patterns, or interact with the Arduino, USE the tools — do NOT refuse or say you cannot access physical devices.\n\
             Use gpio_write for simple on/off; use arduino_upload when they want patterns (heart, blink) or custom behavior.\n\n",
        );
    }

    // ── 1c. Action instruction (avoid meta-summary) ───────────────
    if native_tools {
        prompt.push_str(
            "## Your Task\n\n\
             When the user sends a message, respond naturally. Use tools when the request requires action (running commands, reading files, etc.).\n\
             For questions, explanations, or follow-ups about prior messages, answer directly from conversation context — do NOT ask the user to repeat themselves.\n\
             Do NOT: summarize this configuration, describe your capabilities, or output step-by-step meta-commentary.\n\n",
        );
    } else {
        prompt.push_str(
            "## Your Task\n\n\
             When the user sends a message, ACT on it. Use the tools to fulfill their request.\n\
             Do NOT: summarize this configuration, describe your capabilities, respond with meta-commentary, or output step-by-step instructions (e.g. \"1. First... 2. Next...\").\n\
             Instead: emit actual <tool_call> tags when you need to act. Just do what they ask.\n\n",
        );
    }

    // ── 2. Safety ───────────────────────────────────────────────
    prompt.push_str("## Safety\n\n");
    prompt.push_str(
        "- Do not exfiltrate private data.\n\
         - Do not run destructive commands without asking.\n\
         - Do not bypass oversight or approval mechanisms.\n\
         - Prefer `trash` over `rm` (recoverable beats gone forever).\n\
         - When in doubt, ask before acting externally.\n\n",
    );

    // ── 3. Skills (full instructions + tool metadata) ───────────
    if !skills.is_empty() {
        prompt.push_str(&crate::skills::skills_to_prompt(skills, workspace_dir));
        prompt.push_str("\n\n");
    }

    // ── 4. Workspace ────────────────────────────────────────────
    let _ = writeln!(
        prompt,
        "## Workspace\n\nWorking directory: `{}`\n",
        workspace_dir.display()
    );

    // ── 5. Bootstrap files (injected into context) ──────────────
    prompt.push_str("## Project Context\n\n");

    // Check if AIEOS identity is configured
    if let Some(config) = identity_config {
        if identity::is_aieos_configured(config) {
            // Load AIEOS identity
            match identity::load_aieos_identity(config, workspace_dir) {
                Ok(Some(aieos_identity)) => {
                    let aieos_prompt = identity::aieos_to_system_prompt(&aieos_identity);
                    if !aieos_prompt.is_empty() {
                        prompt.push_str(&aieos_prompt);
                        prompt.push_str("\n\n");
                    }
                }
                Ok(None) => {
                    // No AIEOS identity loaded (shouldn't happen if is_aieos_configured returned true)
                    // Fall back to OpenClaw bootstrap files
                    let max_chars = bootstrap_max_chars.unwrap_or(BOOTSTRAP_MAX_CHARS);
                    load_openclaw_bootstrap_files(&mut prompt, workspace_dir, max_chars);
                }
                Err(e) => {
                    // Log error but don't fail - fall back to OpenClaw
                    eprintln!("Warning: Failed to load AIEOS identity: {e}. Using OpenClaw format.");
                    let max_chars = bootstrap_max_chars.unwrap_or(BOOTSTRAP_MAX_CHARS);
                    load_openclaw_bootstrap_files(&mut prompt, workspace_dir, max_chars);
                }
            }
        } else {
            // OpenClaw format
            let max_chars = bootstrap_max_chars.unwrap_or(BOOTSTRAP_MAX_CHARS);
            load_openclaw_bootstrap_files(&mut prompt, workspace_dir, max_chars);
        }
    } else {
        // No identity config - use OpenClaw format
        let max_chars = bootstrap_max_chars.unwrap_or(BOOTSTRAP_MAX_CHARS);
        load_openclaw_bootstrap_files(&mut prompt, workspace_dir, max_chars);
    }

    // ── 6. Date & Time ──────────────────────────────────────────
    let now = chrono::Local::now();
    let tz = now.format("%Z").to_string();
    let _ = writeln!(prompt, "## Current Date & Time\n\nTimezone: {tz}\n");

    // ── 7. Runtime ──────────────────────────────────────────────
    let host = hostname::get().map_or_else(|_| "unknown".into(), |h| h.to_string_lossy().to_string());
    let _ = writeln!(
        prompt,
        "## Runtime\n\nHost: {host} | OS: {} | Model: {model_name}\n",
        std::env::consts::OS,
    );

    if prompt.is_empty() {
        "You are OpenPRX, a fast and efficient AI assistant built in Rust. Be helpful, concise, and direct.".to_string()
    } else {
        prompt
    }
}

/// Inject a single workspace file into the prompt with truncation and missing-file markers.
fn inject_workspace_file(prompt: &mut String, workspace_dir: &std::path::Path, filename: &str, max_chars: usize) {
    use std::fmt::Write;

    let path = workspace_dir.join(filename);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                return;
            }
            let _ = writeln!(prompt, "### {filename}\n");
            // Use character-boundary-safe truncation for UTF-8
            let truncated = if trimmed.chars().count() > max_chars {
                trimmed
                    .char_indices()
                    .nth(max_chars)
                    .map(|(idx, _)| &trimmed[..idx])
                    .unwrap_or(trimmed)
            } else {
                trimmed
            };
            if truncated.len() < trimmed.len() {
                prompt.push_str(truncated);
                let _ = writeln!(
                    prompt,
                    "\n\n[... truncated at {max_chars} chars — use `read` for full file]\n"
                );
            } else {
                prompt.push_str(trimmed);
                prompt.push_str("\n\n");
            }
        }
        Err(_) => {
            // Missing-file marker (matches OpenClaw behavior)
            let _ = writeln!(prompt, "### {filename}\n\n[File not found: {filename}]\n");
        }
    }
}

fn normalize_telegram_identity(value: &str) -> String {
    value.trim().trim_start_matches('@').to_string()
}

async fn bind_telegram_identity(config: &Config, identity: &str) -> Result<()> {
    let normalized = normalize_telegram_identity(identity);
    if normalized.is_empty() {
        anyhow::bail!("Telegram identity cannot be empty");
    }

    let mut updated = config.clone();
    let Some(telegram) = updated.channels_config.telegram.as_mut() else {
        anyhow::bail!("Telegram channel is not configured. Run `prx onboard --channels-only` first");
    };

    if telegram.allowed_users.iter().any(|u| u == "*") {
        println!("⚠️ Telegram allowlist is currently wildcard (`*`) — binding is unnecessary until you remove '*'.");
    }

    if telegram
        .allowed_users
        .iter()
        .map(|entry| normalize_telegram_identity(entry))
        .any(|entry| entry == normalized)
    {
        println!("✅ Telegram identity already bound: {normalized}");
        return Ok(());
    }

    telegram.allowed_users.push(normalized.clone());
    updated.save().await?;
    println!("✅ Bound Telegram identity: {normalized}");
    println!("   Saved to {}", updated.config_path.display());
    match maybe_restart_managed_daemon_service() {
        Ok(true) => {
            println!("🔄 Detected running managed daemon service; reloaded automatically.");
        }
        Ok(false) => {
            println!(
                "ℹ️ No managed daemon service detected. If `prx daemon`/`channel start` is already running, restart it to load the updated allowlist."
            );
        }
        Err(e) => {
            eprintln!(
                "⚠️ Allowlist saved, but failed to reload daemon service automatically: {e}\n\
                 Restart service manually with `prx service stop && prx service start`."
            );
        }
    }
    Ok(())
}

fn maybe_restart_managed_daemon_service() -> Result<bool> {
    if cfg!(target_os = "macos") {
        let home = directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .context("Could not find home directory")?;
        let plist = home.join("Library").join("LaunchAgents").join("com.prx.daemon.plist");
        if !plist.exists() {
            return Ok(false);
        }

        let list_output = Command::new("launchctl")
            .arg("list")
            .output()
            .context("Failed to query launchctl list")?;
        let listed = String::from_utf8_lossy(&list_output.stdout);
        if !listed.contains("com.prx.daemon") {
            return Ok(false);
        }

        let _ = Command::new("launchctl").args(["stop", "com.prx.daemon"]).output();
        let start_output = Command::new("launchctl")
            .args(["start", "com.prx.daemon"])
            .output()
            .context("Failed to start launchd daemon service")?;
        if !start_output.status.success() {
            let stderr = String::from_utf8_lossy(&start_output.stderr);
            anyhow::bail!("launchctl start failed: {}", stderr.trim());
        }

        return Ok(true);
    }

    if cfg!(target_os = "linux") {
        // OpenRC (system-wide) takes precedence over systemd (user-level)
        let openrc_init_script = PathBuf::from("/etc/init.d/prx");
        if openrc_init_script.exists() {
            if let Ok(status_output) = Command::new("rc-service").args(OPENRC_STATUS_ARGS).output() {
                // rc-service exits 0 if running, non-zero otherwise
                if status_output.status.success() {
                    let restart_output = Command::new("rc-service")
                        .args(OPENRC_RESTART_ARGS)
                        .output()
                        .context("Failed to restart OpenRC daemon service")?;
                    if !restart_output.status.success() {
                        let stderr = String::from_utf8_lossy(&restart_output.stderr);
                        anyhow::bail!("rc-service restart failed: {}", stderr.trim());
                    }
                    return Ok(true);
                }
            }
        }

        // Systemd (user-level)
        let home = directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .context("Could not find home directory")?;
        let unit_path: PathBuf = home.join(".config").join("systemd").join("user").join("prx.service");
        if !unit_path.exists() {
            return Ok(false);
        }

        let active_output = Command::new("systemctl")
            .args(SYSTEMD_STATUS_ARGS)
            .output()
            .context("Failed to query systemd service state")?;
        let state = String::from_utf8_lossy(&active_output.stdout);
        if !state.trim().eq_ignore_ascii_case("active") {
            return Ok(false);
        }

        let restart_output = Command::new("systemctl")
            .args(SYSTEMD_RESTART_ARGS)
            .output()
            .context("Failed to restart systemd daemon service")?;
        if !restart_output.status.success() {
            let stderr = String::from_utf8_lossy(&restart_output.stderr);
            anyhow::bail!("systemctl restart failed: {}", stderr.trim());
        }

        return Ok(true);
    }

    Ok(false)
}

pub async fn handle_command(command: crate::ChannelCommands, config: &Config) -> Result<()> {
    match command {
        crate::ChannelCommands::Start => {
            anyhow::bail!("Start must be handled in main.rs (requires async runtime)")
        }
        crate::ChannelCommands::Doctor => {
            anyhow::bail!("Doctor must be handled in main.rs (requires async runtime)")
        }
        crate::ChannelCommands::List => {
            println!("Channels:");
            println!("  ✅ CLI (always available)");
            for (name, configured) in [
                ("Telegram", config.channels_config.telegram.is_some()),
                ("Discord", config.channels_config.discord.is_some()),
                ("Slack", config.channels_config.slack.is_some()),
                ("Mattermost", config.channels_config.mattermost.is_some()),
                ("Webhook", config.channels_config.webhook.is_some()),
                ("iMessage", config.channels_config.imessage.is_some()),
                (
                    "Matrix",
                    cfg!(feature = "channel-matrix") && config.channels_config.matrix.is_some(),
                ),
                ("Signal", config.channels_config.signal.is_some()),
                ("WhatsApp", config.channels_config.whatsapp.is_some()),
                ("Linq", config.channels_config.linq.is_some()),
                ("Nextcloud Talk", config.channels_config.nextcloud_talk.is_some()),
                ("Email", config.channels_config.email.is_some()),
                ("IRC", config.channels_config.irc.is_some()),
                ("Lark", config.channels_config.lark.is_some()),
                ("DingTalk", config.channels_config.dingtalk.is_some()),
                ("QQ", config.channels_config.qq.is_some()),
            ] {
                println!("  {} {name}", if configured { "✅" } else { "❌" });
            }
            if !cfg!(feature = "channel-matrix") {
                println!("  ℹ️ Matrix channel support is disabled in this build (enable `channel-matrix`).");
            }
            println!("\nTo start channels: prx channel start");
            println!("To check health:    prx channel doctor");
            println!("To configure:      prx onboard");
            Ok(())
        }
        crate::ChannelCommands::Add {
            channel_type,
            config: _,
        } => {
            anyhow::bail!("Channel type '{channel_type}' — use `prx onboard` to configure channels");
        }
        crate::ChannelCommands::Remove { name } => {
            anyhow::bail!("Remove channel '{name}' — edit ~/.openprx/config.toml directly");
        }
        crate::ChannelCommands::BindTelegram { identity } => bind_telegram_identity(config, &identity).await,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelHealthState {
    Healthy,
    Unhealthy,
    Timeout,
}

const fn classify_health_result(result: &std::result::Result<bool, tokio::time::error::Elapsed>) -> ChannelHealthState {
    match result {
        Ok(true) => ChannelHealthState::Healthy,
        Ok(false) => ChannelHealthState::Unhealthy,
        Err(_) => ChannelHealthState::Timeout,
    }
}

/// Run health checks for configured channels.
pub async fn doctor_channels(config: Config) -> Result<()> {
    let mut channels: Vec<(&'static str, Arc<dyn Channel>)> = Vec::new();

    if let Some(ref tg) = config.channels_config.telegram {
        channels.push((
            "Telegram",
            Arc::new(
                TelegramChannel::new(tg.bot_token.clone(), tg.allowed_users.clone(), tg.mention_only)
                    .with_streaming(tg.stream_mode, tg.draft_update_interval_ms)
                    .with_group_reply_mode(crate::config::GroupReplyMode::resolve(
                        tg.group_reply_mode,
                        tg.mention_only,
                    )),
            ),
        ));
    }

    if let Some(ref dc) = config.channels_config.discord {
        channels.push((
            "Discord",
            Arc::new(
                DiscordChannel::new(
                    dc.bot_token.clone(),
                    dc.guild_id.clone(),
                    dc.allowed_users.clone(),
                    dc.listen_to_bots,
                    dc.mention_only,
                )
                .with_group_reply_mode(crate::config::GroupReplyMode::resolve(
                    dc.group_reply_mode,
                    dc.mention_only,
                )),
            ),
        ));
    }

    if let Some(ref sl) = config.channels_config.slack {
        channels.push((
            "Slack",
            Arc::new(SlackChannel::new(
                sl.bot_token.clone(),
                sl.channel_id.clone(),
                sl.allowed_users.clone(),
            )),
        ));
    }

    if let Some(ref im) = config.channels_config.imessage {
        channels.push(("iMessage", Arc::new(IMessageChannel::new(im.allowed_contacts.clone()))));
    }

    #[cfg(feature = "channel-matrix")]
    if let Some(ref mx) = config.channels_config.matrix {
        channels.push((
            "Matrix",
            Arc::new(MatrixChannel::new_with_session_hint(
                mx.homeserver.clone(),
                mx.access_token.clone(),
                mx.room_id.clone(),
                mx.allowed_users.clone(),
                mx.user_id.clone(),
                mx.device_id.clone(),
            )),
        ));
    }

    #[cfg(not(feature = "channel-matrix"))]
    if config.channels_config.matrix.is_some() {
        tracing::warn!(
            "Matrix channel is configured but this build was compiled without `channel-matrix`; skipping Matrix health check."
        );
    }

    if let Some(ref sig) = config.channels_config.signal {
        let media_artifacts = crate::media::MediaArtifactOwner::for_workspace(&config.workspace_dir);
        let signal_channel: Arc<dyn Channel + Send + Sync> = if sig.is_native_mode() {
            Arc::new(
                SignalNativeChannel::new(
                    sig.cli_path.clone().unwrap_or_else(|| "signal-cli".to_string()),
                    sig.account.clone(),
                    sig.data_dir.clone(),
                    sig.daemon_http_port.unwrap_or(16866),
                    sig.startup_timeout_ms,
                    sig.group_id.clone(),
                    sig.allowed_from.clone(),
                    sig.ignore_attachments,
                    sig.ignore_stories,
                    config.media.clone(),
                    sig.storm_protection.clone(),
                )
                .with_artifact_owner(media_artifacts),
            )
        } else {
            Arc::new(
                SignalChannel::new_with_storm_protection(
                    sig.effective_http_url(),
                    sig.account.clone(),
                    sig.group_id.clone(),
                    sig.allowed_from.clone(),
                    sig.ignore_attachments,
                    sig.ignore_stories,
                    config.media.clone(),
                    sig.storm_protection.clone(),
                )
                .with_artifact_owner(media_artifacts),
            )
        };
        channels.push(("Signal", signal_channel));
    }

    if let Some(ref wa) = config.channels_config.whatsapp {
        if wa.is_ambiguous_config() {
            tracing::warn!(
                "WhatsApp config has both phone_number_id and session_path set; preferring Cloud API mode. Remove one selector to avoid ambiguity."
            );
        }
        // Runtime negotiation: detect backend type from config
        match wa.backend_type() {
            "cloud" => {
                // Cloud API mode: requires phone_number_id, access_token, verify_token
                if wa.is_cloud_config() {
                    channels.push((
                        "WhatsApp",
                        Arc::new(WhatsAppChannel::new(
                            wa.access_token.clone().unwrap_or_default(),
                            wa.phone_number_id.clone().unwrap_or_default(),
                            wa.verify_token.clone().unwrap_or_default(),
                            wa.allowed_numbers.clone(),
                        )),
                    ));
                } else {
                    tracing::warn!(
                        "WhatsApp Cloud API configured but missing required fields (phone_number_id, access_token, verify_token)"
                    );
                }
            }
            "web" => {
                // Web mode: requires session_path
                #[cfg(feature = "whatsapp-web")]
                if wa.is_web_config() {
                    channels.push((
                        "WhatsApp",
                        Arc::new(
                            WhatsAppWebChannel::new(
                                wa.session_path.clone().unwrap_or_default(),
                                wa.pair_phone.clone(),
                                wa.pair_code.clone(),
                                wa.allowed_numbers.clone(),
                            )
                            .with_ws_url(wa.ws_url.clone()),
                        ),
                    ));
                } else {
                    tracing::warn!("WhatsApp Web configured but session_path not set");
                }
                #[cfg(not(feature = "whatsapp-web"))]
                {
                    tracing::warn!(
                        "WhatsApp Web backend requires 'whatsapp-web' feature. Enable with: cargo build --features whatsapp-web"
                    );
                }
            }
            _ => {
                tracing::warn!(
                    "WhatsApp config invalid: neither phone_number_id (Cloud API) nor session_path (Web) is set"
                );
            }
        }
    }

    if let Some(ref lq) = config.channels_config.linq {
        channels.push((
            "Linq",
            Arc::new(LinqChannel::new(
                lq.api_token.clone(),
                lq.from_phone.clone(),
                lq.allowed_senders.clone(),
            )),
        ));
    }

    if let Some(ref nc) = config.channels_config.nextcloud_talk {
        channels.push((
            "Nextcloud Talk",
            Arc::new(NextcloudTalkChannel::new(
                nc.base_url.clone(),
                nc.app_token.clone(),
                nc.allowed_users.clone(),
            )),
        ));
    }

    if let Some(ref email_cfg) = config.channels_config.email {
        channels.push(("Email", Arc::new(EmailChannel::new(email_cfg.clone()))));
    }

    if let Some(ref irc) = config.channels_config.irc {
        channels.push((
            "IRC",
            Arc::new(IrcChannel::new(irc::IrcChannelConfig {
                server: irc.server.clone(),
                port: irc.port,
                nickname: irc.nickname.clone(),
                username: irc.username.clone(),
                channels: irc.channels.clone(),
                allowed_users: irc.allowed_users.clone(),
                server_password: irc.server_password.clone(),
                nickserv_password: irc.nickserv_password.clone(),
                sasl_password: irc.sasl_password.clone(),
                verify_tls: irc.verify_tls.unwrap_or(true),
            })),
        ));
    }

    if let Some(ref lk) = config.channels_config.lark {
        channels.push(("Lark", Arc::new(LarkChannel::from_config(lk))));
    }

    if let Some(ref dt) = config.channels_config.dingtalk {
        channels.push((
            "DingTalk",
            Arc::new(DingTalkChannel::new(
                dt.client_id.clone(),
                dt.client_secret.clone(),
                dt.allowed_users.clone(),
            )),
        ));
    }

    if let Some(ref qq) = config.channels_config.qq {
        channels.push((
            "QQ",
            Arc::new(QQChannel::new(
                qq.app_id.clone(),
                qq.app_secret.clone(),
                qq.allowed_users.clone(),
            )),
        ));
    }

    if channels.is_empty() {
        println!("No real-time channels configured. Run `prx onboard` first.");
        return Ok(());
    }

    println!("🩺 OpenPRX Channel Doctor");
    println!();

    let mut healthy = 0_u32;
    let mut unhealthy = 0_u32;
    let mut timeout = 0_u32;

    for (name, channel) in channels {
        let result = tokio::time::timeout(Duration::from_secs(10), channel.health_check()).await;
        let state = classify_health_result(&result);

        match state {
            ChannelHealthState::Healthy => {
                healthy += 1;
                println!("  ✅ {name:<9} healthy");
            }
            ChannelHealthState::Unhealthy => {
                unhealthy += 1;
                println!("  ❌ {name:<9} unhealthy (auth/config/network)");
            }
            ChannelHealthState::Timeout => {
                timeout += 1;
                println!("  ⏱️  {name:<9} timed out (>10s)");
            }
        }
    }

    if config.channels_config.webhook.is_some() {
        println!("  ℹ️  Webhook   check via `prx gateway` then GET /health");
    }

    println!();
    println!("Summary: {healthy} healthy, {unhealthy} unhealthy, {timeout} timed out");
    Ok(())
}

/// Start all configured channels and route messages to the agent
pub async fn start_channels(config: Config, shutdown: CancellationToken) -> Result<()> {
    let shared_config = crate::config::new_shared(config.clone());
    let generation = shared_config.pin();
    start_channels_with_config(config, shared_config, generation, shutdown).await
}

#[allow(clippy::too_many_lines)]
pub async fn start_channels_with_config(
    config: Config,
    shared_config: crate::config::SharedConfig,
    config_generation: Arc<crate::config::ConfigGeneration>,
    shutdown: CancellationToken,
) -> Result<()> {
    #[cfg(not(test))]
    let _ = &config_generation;
    let provider_name = resolved_default_provider(&config);
    let provider_runtime_options = providers::provider_runtime_options_from_config(&config);
    let provider: Arc<dyn Provider> = Arc::from(providers::create_resilient_provider_with_options(
        &provider_name,
        config.api_key.as_deref(),
        config.api_url.as_deref(),
        &config.reliability,
        &provider_runtime_options,
    )?);

    // Warm up the provider connection pool (TLS handshake, DNS, HTTP/2 setup)
    // so the first real message doesn't hit a cold-start timeout.
    if let Err(e) = provider.warmup().await {
        tracing::warn!("Provider warmup failed (non-fatal): {e}");
    }

    let observer: Arc<dyn Observer> = Arc::from(observability::create_observer(&config.observability));
    let hooks = Arc::new(HookManager::new(config.workspace_dir.clone()));
    let runtime: Arc<dyn runtime::RuntimeAdapter> = Arc::from(runtime::create_runtime(&config.runtime)?);
    // FIX-P1-31: thread the real `security.audit` config so the side-effect gate
    // audit trail honours it (e.g. `enabled=false` ⇒ no per-message fsync).
    // BUG-D1-01 class: route through the single `build_security_policy` helper so
    // no mode can forget `with_audit_config`. Wiring is verbatim-identical to the
    // former local construction (gateway uses the same helper).
    //
    // Channel-aware tools built for this component generation retain this policy
    // until the daemon supervisor replaces the component. Per-message channel
    // gates independently derive their policy from the generation pinned at
    // admission, so one turn never mixes security generations.
    let security = crate::runtime::bootstrap::build_security_policy(&config);
    let model = resolved_default_model(&config);
    let temperature = config.default_temperature;
    let mem: Arc<dyn Memory> = Arc::from(memory::create_memory_with_storage(
        &config.memory,
        Some(&config.storage.provider.config),
        &config.workspace_dir,
        config.api_key.as_deref(),
    )?);
    let conversation_histories = Arc::new(Mutex::new(
        load_persisted_histories(&config.workspace_dir, mem.as_ref()).await,
    ));
    let (composio_key, composio_entity_id) = if config.composio.enabled {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    };
    // Build system prompt from workspace identity files + skills
    let workspace = config.workspace_dir.clone();
    // Keep as mutable Vec so we can append channel-aware tools (e.g. sessions_spawn)
    // before wrapping in Arc below.
    let mut tools_list = tools::all_tools_with_runtime(
        Arc::new(config.clone()),
        Arc::clone(&shared_config),
        &security,
        runtime,
        Arc::clone(&mem),
        composio_key,
        composio_entity_id,
        &config.browser,
        &config.http_request,
        &workspace,
        &config.agents,
        config.api_key.as_deref(),
        &config,
    );

    let skill_embedder = crate::memory::create_embedder_from_config(&config, config.api_key.as_deref());
    let skills = crate::skills::load_skills_with_embeddings(&workspace, &config, skill_embedder.as_ref()).await?;

    // Collect tool descriptions for the prompt
    let mut tool_descs: Vec<(&str, &str)> = vec![
        (
            "shell",
            "Execute terminal commands. Use when: running local checks, build/test commands, diagnostics. Don't use when: a safer dedicated tool exists, or command is destructive without approval.",
        ),
        (
            "file_read",
            "Read file contents. Use when: inspecting project files, configs, logs. Don't use when: a targeted search is enough.",
        ),
        (
            "file_write",
            "Write file contents. Use when: applying focused edits, scaffolding files, updating docs/code. Don't use when: side effects are unclear or file ownership is uncertain.",
        ),
        (
            "memory_store",
            "Save to memory. Use when: preserving durable preferences, decisions, key context. Don't use when: information is transient/noisy/sensitive without need.",
        ),
        (
            "memory_recall",
            "Search memory. Use when: retrieving prior decisions, user preferences, historical context. Don't use when: answer is already in current context.",
        ),
        (
            "memory_search",
            "Search MEMORY.md and memory/*.md for matching snippets with file path and line number. Use when: locating notes in markdown memory files.",
        ),
        (
            "memory_get",
            "Read a range of lines from MEMORY.md or memory/*.md. Use when: you already know the memory file/path and need exact content.",
        ),
        (
            "memory_forget",
            "Delete a memory entry. Use when: memory is incorrect/stale or explicitly requested for removal. Don't use when: impact is uncertain.",
        ),
    ];

    if config.composio.enabled {
        tool_descs.push((
            "composio",
            "Execute actions on 1000+ apps via Composio (Gmail, Notion, GitHub, Slack, etc.). Use action='list' to discover actions, 'list_accounts' to retrieve connected account IDs, 'execute' to run (optionally with connected_account_id), and 'connect' for OAuth.",
        ));
    }
    tool_descs.push((
        "cron",
        "Unified scheduler. Set `action`: add/schedule (create job), once (one-shot via delay/run_at), \
         list, get, remove/cancel, update/patch, run, runs/history, events, pause, resume, status.",
    ));
    tool_descs.push((
        "pushover",
        "Send a Pushover notification to your device. Requires PUSHOVER_TOKEN and PUSHOVER_USER_KEY in .env file.",
    ));
    tool_descs.push((
        "nodes",
        "Manage remote nodes from [nodes] config over JSON-RPC. Actions: list, status, exec, read, write, cancel.",
    ));
    if !config.agents.is_empty() {
        tool_descs.push((
            "delegate",
            "Delegate a subtask to a specialized agent. Use when: a task benefits from a different model (e.g. fast summarization, deep reasoning, code generation). The sub-agent runs a single prompt and returns its response.",
        ));
    }
    tool_descs.push((
        "sessions_spawn",
        "Spawn an async sub-agent to handle a task in isolation. Returns immediately with a run ID. \
         The sub-agent announces its result when complete. Use for long-running or parallel tasks \
         that should not block the main conversation.",
    ));
    tool_descs.push((
        "subagents",
        "Manage sub-agent runs spawned by sessions_spawn. Actions: list active/recent runs, kill a running run, or steer a running run with a new instruction.",
    ));

    let bootstrap_max_chars = if config.agent.compact_context { Some(6000) } else { None };
    let native_tools = provider
        .capabilities_for(&model, crate::providers::traits::ProviderRequestMode::NonStreaming)
        .native_tool_calling;
    let mut system_prompt = build_system_prompt_with_mode(
        &workspace,
        &model,
        &tool_descs,
        &skills,
        Some(&config.identity),
        bootstrap_max_chars,
        native_tools,
    );
    if !native_tools {
        // Startup static prompt: NEVER advertise `stay_silent` here — this prompt
        // is reused for every turn (DM and group). A non-native smart group turn
        // appends the tool's instructions per-turn in `process_channel_message`.
        system_prompt.push_str(&build_tool_instructions(&tools_list, false));
    }

    if !skills.is_empty() {
        println!(
            "  🧩 Skills:   {}",
            skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
        );
    }

    // Build Skill RAG context for per-message skill selection (when enabled)
    let skill_rag_ctx = if config.skill_rag.enabled {
        let tool_descs_owned: Vec<(String, String)> = tool_descs
            .iter()
            .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
            .collect();
        Some(SkillRagContext {
            skills: Arc::new(skills),
            embedder: skill_embedder,
            top_k: config.skill_rag.top_k,
            tool_descs_owned: Arc::new(tool_descs_owned),
            identity_config: Some(config.identity.clone()),
            bootstrap_max_chars,
            native_tools,
        })
    } else {
        None
    };

    // Collect active channels
    let mut channels: Vec<Arc<dyn Channel>> = Vec::new();

    if let Some(ref tg) = config.channels_config.telegram {
        channels.push(Arc::new(
            TelegramChannel::new(tg.bot_token.clone(), tg.allowed_users.clone(), tg.mention_only)
                .with_streaming(tg.stream_mode, tg.draft_update_interval_ms)
                .with_group_reply_mode(crate::config::GroupReplyMode::resolve(
                    tg.group_reply_mode,
                    tg.mention_only,
                )),
        ));
    }

    if let Some(ref dc) = config.channels_config.discord {
        channels.push(Arc::new(
            DiscordChannel::new(
                dc.bot_token.clone(),
                dc.guild_id.clone(),
                dc.allowed_users.clone(),
                dc.listen_to_bots,
                dc.mention_only,
            )
            .with_group_reply_mode(crate::config::GroupReplyMode::resolve(
                dc.group_reply_mode,
                dc.mention_only,
            )),
        ));
    }

    if let Some(ref sl) = config.channels_config.slack {
        channels.push(Arc::new(
            SlackChannel::new(sl.bot_token.clone(), sl.channel_id.clone(), sl.allowed_users.clone())
                .with_workspace_dir(config.workspace_dir.clone()),
        ));
    }

    if let Some(ref mm) = config.channels_config.mattermost {
        channels.push(Arc::new(MattermostChannel::new(
            mm.url.clone(),
            mm.bot_token.clone(),
            mm.channel_id.clone(),
            mm.allowed_users.clone(),
            mm.thread_replies.unwrap_or(true),
            mm.mention_only.unwrap_or(false),
        )));
    }

    if let Some(ref im) = config.channels_config.imessage {
        channels.push(Arc::new(IMessageChannel::new(im.allowed_contacts.clone())));
    }

    #[cfg(feature = "channel-matrix")]
    if let Some(ref mx) = config.channels_config.matrix {
        channels.push(Arc::new(MatrixChannel::new_with_session_hint(
            mx.homeserver.clone(),
            mx.access_token.clone(),
            mx.room_id.clone(),
            mx.allowed_users.clone(),
            mx.user_id.clone(),
            mx.device_id.clone(),
        )));
    }

    #[cfg(not(feature = "channel-matrix"))]
    if config.channels_config.matrix.is_some() {
        tracing::warn!(
            "Matrix channel is configured but this build was compiled without `channel-matrix`; skipping Matrix runtime startup."
        );
    }

    if let Some(ref sig) = config.channels_config.signal {
        let media_artifacts = crate::media::MediaArtifactOwner::for_workspace(&config.workspace_dir);
        let signal_channel: Arc<dyn Channel + Send + Sync> = if sig.is_native_mode() {
            tracing::info!(
                "Signal: native mode — will spawn signal-cli daemon on port {}",
                sig.daemon_http_port.unwrap_or(16866)
            );
            Arc::new(
                SignalNativeChannel::new(
                    sig.cli_path.clone().unwrap_or_else(|| "signal-cli".to_string()),
                    sig.account.clone(),
                    sig.data_dir.clone(),
                    sig.daemon_http_port.unwrap_or(16866),
                    sig.startup_timeout_ms,
                    sig.group_id.clone(),
                    sig.allowed_from.clone(),
                    sig.ignore_attachments,
                    sig.ignore_stories,
                    config.media.clone(),
                    sig.storm_protection.clone(),
                )
                .with_artifact_owner(media_artifacts),
            )
        } else {
            Arc::new(
                SignalChannel::new_with_storm_protection(
                    sig.effective_http_url(),
                    sig.account.clone(),
                    sig.group_id.clone(),
                    sig.allowed_from.clone(),
                    sig.ignore_attachments,
                    sig.ignore_stories,
                    config.media.clone(),
                    sig.storm_protection.clone(),
                )
                .with_artifact_owner(media_artifacts),
            )
        };
        channels.push(signal_channel);
    }

    if let Some(ref wa) = config.channels_config.whatsapp {
        if wa.is_ambiguous_config() {
            tracing::warn!(
                "WhatsApp config has both phone_number_id and session_path set; preferring Cloud API mode. Remove one selector to avoid ambiguity."
            );
        }
        // Runtime negotiation: detect backend type from config
        match wa.backend_type() {
            "cloud" => {
                // Cloud API mode: requires phone_number_id, access_token, verify_token
                if wa.is_cloud_config() {
                    channels.push(Arc::new(WhatsAppChannel::new(
                        wa.access_token.clone().unwrap_or_default(),
                        wa.phone_number_id.clone().unwrap_or_default(),
                        wa.verify_token.clone().unwrap_or_default(),
                        wa.allowed_numbers.clone(),
                    )));
                } else {
                    tracing::warn!(
                        "WhatsApp Cloud API configured but missing required fields (phone_number_id, access_token, verify_token)"
                    );
                }
            }
            "web" => {
                // Web mode: requires session_path
                #[cfg(feature = "whatsapp-web")]
                if wa.is_web_config() {
                    channels.push(Arc::new(
                        WhatsAppWebChannel::new(
                            wa.session_path.clone().unwrap_or_default(),
                            wa.pair_phone.clone(),
                            wa.pair_code.clone(),
                            wa.allowed_numbers.clone(),
                        )
                        .with_ws_url(wa.ws_url.clone()),
                    ));
                } else {
                    tracing::warn!("WhatsApp Web configured but session_path not set");
                }
                #[cfg(not(feature = "whatsapp-web"))]
                {
                    tracing::warn!(
                        "WhatsApp Web backend requires 'whatsapp-web' feature. Enable with: cargo build --features whatsapp-web"
                    );
                }
            }
            _ => {
                tracing::warn!(
                    "WhatsApp config invalid: neither phone_number_id (Cloud API) nor session_path (Web) is set"
                );
            }
        }
    }

    if let Some(ref lq) = config.channels_config.linq {
        channels.push(Arc::new(LinqChannel::new(
            lq.api_token.clone(),
            lq.from_phone.clone(),
            lq.allowed_senders.clone(),
        )));
    }

    if let Some(ref wc) = config.channels_config.wacli {
        if wc.enabled {
            channels.push(Arc::new(WacliChannel::new(wacli::WacliChannelConfig {
                webhook_listen: wc.webhook_listen.clone(),
                webhook_path: wc.webhook_path.clone(),
                webhook_secret: wc.webhook_secret.clone(),
                allow_unsigned_loopback: wc.allow_unsigned_loopback,
                allowed_from: wc.allowed_from.clone(),
                cli_path: wc.cli_path.clone(),
                store_dir: wc.store_dir.clone(),
                bot_jid: wc.bot_jid.clone(),
                bot_number: wc.bot_number.clone(),
                bot_lid: wc.bot_lid.clone(),
            })));
        } else {
            tracing::debug!("wacli channel configured but not enabled (set enabled = true to activate)");
        }
    }

    if let Some(ref nc) = config.channels_config.nextcloud_talk {
        channels.push(Arc::new(NextcloudTalkChannel::new(
            nc.base_url.clone(),
            nc.app_token.clone(),
            nc.allowed_users.clone(),
        )));
    }

    if let Some(ref email_cfg) = config.channels_config.email {
        channels.push(Arc::new(EmailChannel::new(email_cfg.clone())));
    }

    if let Some(ref irc) = config.channels_config.irc {
        channels.push(Arc::new(IrcChannel::new(irc::IrcChannelConfig {
            server: irc.server.clone(),
            port: irc.port,
            nickname: irc.nickname.clone(),
            username: irc.username.clone(),
            channels: irc.channels.clone(),
            allowed_users: irc.allowed_users.clone(),
            server_password: irc.server_password.clone(),
            nickserv_password: irc.nickserv_password.clone(),
            sasl_password: irc.sasl_password.clone(),
            verify_tls: irc.verify_tls.unwrap_or(true),
        })));
    }

    if let Some(ref lk) = config.channels_config.lark {
        channels.push(Arc::new(LarkChannel::from_config(lk)));
    }

    if let Some(ref dt) = config.channels_config.dingtalk {
        channels.push(Arc::new(DingTalkChannel::new(
            dt.client_id.clone(),
            dt.client_secret.clone(),
            dt.allowed_users.clone(),
        )));
    }

    if let Some(ref qq) = config.channels_config.qq {
        channels.push(Arc::new(QQChannel::new(
            qq.app_id.clone(),
            qq.app_secret.clone(),
            qq.allowed_users.clone(),
        )));
    }

    if channels.is_empty() {
        println!("No channels configured. Run `prx onboard` to set up channels.");
        return Ok(());
    }

    // Register message_send tool backed by Signal (or first channel) for proactive messaging.
    // Always use SignalChannel (HTTP) pointing to the effective URL (local daemon in native mode,
    // external daemon in rest mode).  The daemon must be running by the time this tool is invoked.
    if let Some(ref sig) = config.channels_config.signal {
        let is_native = sig.is_native_mode();
        let effective_url = sig.effective_http_url();
        tracing::info!(
            "message_send tool: is_native={is_native} url={effective_url} mode={:?}",
            sig.mode
        );
        let sig_chan = Arc::new(
            SignalChannel::new_with_mode(
                effective_url,
                sig.account.clone(),
                sig.group_id.clone(),
                sig.allowed_from.clone(),
                sig.ignore_attachments,
                sig.ignore_stories,
                config.media.clone(),
                is_native,
                sig.data_dir.clone(),
                sig.storm_protection.clone(),
            )
            .with_artifact_owner(crate::media::MediaArtifactOwner::for_workspace(&config.workspace_dir)),
        );
        let msg_send_tool = tools::MessageSendTool::new_signal(sig_chan, security.clone());
        tools_list.push(Box::new(msg_send_tool));
    } else if let Some(first_channel) = channels.first().cloned() {
        let msg_send_tool = tools::MessageSendTool::new(first_channel, security.clone());
        tools_list.push(Box::new(msg_send_tool));
    }

    // Register sessions_spawn tool backed by the first available channel.
    // This enables fire-and-forget sub-agent spawning with auto-announce on completion.
    // We keep a handle to the OnceLock so we can inject the full tools_registry
    // after it's wrapped in Arc (resolves the chicken-and-egg registration problem).
    // Also extract the active_runs Arc so sessions_list, sessions_send,
    // subagents, and session_status can share the same run registry without duplication.
    let spawn_tools_handle = if let Some(first_channel) = channels.first().cloned() {
        // Per-turn announce/kill routing registry: every configured channel keyed
        // by name. sessions_spawn binds the *originating* channel name to each run
        // (from the launching message's scope) and resolves the channel object
        // from here at announce/kill time — so concurrent message processing can
        // never mis-route a sub-agent result to the wrong channel.
        let spawn_channels_by_name = Arc::new(
            channels
                .iter()
                .map(|ch| (ch.name().to_string(), Arc::clone(ch)))
                .collect::<HashMap<_, _>>(),
        );
        let spawn_tool = tools::SessionsSpawnTool::new(
            first_channel,
            Arc::clone(&provider),
            &provider_name,
            &model,
            temperature,
            security.clone(),
            config.workspace_dir.clone(),
            config.multimodal.clone(),
            config.agent.compaction.clone(),
            config.agents.clone(),
            config.api_key.clone(),
            provider_runtime_options.clone(),
            config.sessions_spawn.clone(),
        )
        .with_compaction_resolver(crate::router::CompactionResolver::new(
            config.agent.compaction.clone(),
            config.router.clone(),
            config.model_routes.clone(),
        ))
        .with_channels(spawn_channels_by_name)
        .with_shared_memory(Arc::clone(&mem));
        let spawn_tool = spawn_tool.with_event_recording(config.memory.event_recording_config());
        let handle = spawn_tool.tools_handle();
        let active_runs = spawn_tool.active_runs_arc();

        // sessions_list: dedicated listing tool (OpenClaw alignment)
        let workspace_id = config.workspace_dir.to_string_lossy().to_string();
        tools_list.push(Box::new(
            tools::SessionsListTool::new(active_runs.clone())
                .with_shared_memory(Arc::clone(&mem), workspace_id.clone()),
        ));
        // sessions_send: cross-session message injection (OpenClaw alignment)
        tools_list.push(Box::new(
            tools::SessionsSendTool::with_security(active_runs.clone(), security.clone())
                .with_shared_memory(Arc::clone(&mem))
                .with_event_recording(config.memory.event_recording_config()),
        ));
        // subagents: OpenClaw-compatible subagent management interface
        tools_list.push(Box::new(
            tools::SubagentsTool::with_security(active_runs.clone(), security.clone())
                .with_shared_memory(Arc::clone(&mem))
                .with_event_recording(config.memory.event_recording_config()),
        ));
        // sessions_history: conversation log viewer (OpenClaw alignment)
        tools_list.push(Box::new(
            tools::SessionsHistoryTool::new(active_runs.clone())
                .with_shared_memory(Arc::clone(&mem), workspace_id.clone()),
        ));
        // session_status: runtime status card (OpenClaw alignment)
        let channel_names: Vec<String> = channels.iter().map(|c| c.name().to_string()).collect();
        tools_list.push(Box::new(
            tools::SessionStatusTool::new(active_runs, &provider_name, &model, channel_names.clone())
                .with_shared_memory(Arc::clone(&mem), workspace_id),
        ));
        // gateway: daemon management (OpenClaw alignment)
        tools_list.push(Box::new(
            tools::GatewayTool::new(Arc::clone(&shared_config), &provider_name, &model, channel_names)
                .with_security(security.clone()),
        ));

        // image: vision tool backed by the active provider (OpenClaw alignment)
        tools_list.push(Box::new(tools::ImageTool::new(
            Arc::clone(&provider),
            &model,
            temperature,
            security.clone(),
            config.multimodal.clone(),
        )));

        tools_list.push(Box::new(spawn_tool));
        Some(handle)
    } else {
        None
    };

    // Register config_reload tool (allows AI to manually trigger hot-reload).
    {
        tools_list.push(Box::new(tools::ConfigReloadTool::with_security(
            Arc::clone(&shared_config),
            security.clone(),
        )));
    }

    // ── Register the same process-level WASM runtime used by every entrypoint ──
    #[cfg(feature = "wasm-plugins")]
    let wasm_plugin_runtime = crate::plugins::init_plugin_runtime(&config.workspace_dir, Some(Arc::clone(&mem))).await;
    #[cfg(feature = "wasm-plugins")]
    if let Some(runtime) = &wasm_plugin_runtime {
        let router = runtime.tool_router();
        let tool_count = router.specs().len();
        tracing::info!(
            count = tool_count,
            "registering dynamic WASM plugin tool router in channels"
        );
        tools_list.push(router);
        hooks.set_plugin_runtime(Arc::clone(runtime)).await;
        tracing::debug!(
            generation = runtime.generation_id(),
            "shared WASM plugin runtime ready in channels"
        );
    }

    // Wrap the tool list in Arc now that all channel-aware tools have been appended.
    let tools_registry = Arc::new(tools_list);

    // Inject the tools registry into sessions_spawn so sub-agents can use tools.
    if let Some(handle) = spawn_tools_handle {
        handle.set(Arc::clone(&tools_registry)).ok();
    }

    println!("🦀 OpenPRX Channel Server");
    println!("  🤖 Model:    {model}");
    let effective_backend =
        memory::effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config));
    println!(
        "  🧠 Memory:   {} (semantic auto-promote: {})",
        effective_backend,
        if config.memory.auto_save && config.memory.semantic.auto_promote_user_messages {
            "on"
        } else {
            "off"
        }
    );
    println!(
        "  📡 Channels: {}",
        channels.iter().map(|c| c.name()).collect::<Vec<_>>().join(", ")
    );
    println!();
    println!("  Listening for messages... (Ctrl+C to stop)");
    println!();

    crate::health::mark_component_ok("channels");

    let initial_backoff_secs = config
        .reliability
        .channel_initial_backoff_secs
        .max(DEFAULT_CHANNEL_INITIAL_BACKOFF_SECS);
    let max_backoff_secs = config
        .reliability
        .channel_max_backoff_secs
        .max(DEFAULT_CHANNEL_MAX_BACKOFF_SECS);

    // Single message bus — all channels send messages here
    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(100);

    // Spawn a listener for each channel
    let mut handles = Vec::new();
    for ch in &channels {
        handles.push(spawn_supervised_listener(
            ch.clone(),
            tx.clone(),
            initial_backoff_secs,
            max_backoff_secs,
            shutdown.clone(),
        ));
    }
    drop(tx); // Drop our copy so rx closes when all channels stop

    let channels_by_name = Arc::new(
        channels
            .iter()
            .map(|ch| (ch.name().to_string(), Arc::clone(ch)))
            .collect::<HashMap<_, _>>(),
    );
    let max_in_flight_messages = compute_max_in_flight_messages(channels.len());

    println!("  🚦 In-flight message limit: {max_in_flight_messages}");

    let mut provider_cache_seed: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    provider_cache_seed.insert(provider_name.clone(), Arc::clone(&provider));
    let message_timeout_secs = effective_channel_message_timeout_secs(config.channels_config.message_timeout_secs);
    let signal_inbound_policy = config.channels_config.signal.as_ref().map(|signal| {
        let allowed_from = normalize_allowlist(&signal.allowed_from);
        let group_allow_from = normalize_allowlist(&signal.group_allow_from);
        warn_open_policy_allowlist_alignment(
            "signal",
            signal.dm_policy,
            signal.group_policy,
            &allowed_from,
            &group_allow_from,
        );
        InboundPolicyConfig {
            dm_policy: normalize_dm_policy("signal", signal.dm_policy),
            group_policy: signal.group_policy,
            allowed_from,
            group_allow_from,
        }
    });
    let whatsapp_inbound_policy = config.channels_config.whatsapp.as_ref().map(|whatsapp| {
        let allowed_from = normalize_allowlist(&whatsapp.allowed_numbers);
        let group_allow_from = normalize_allowlist(&whatsapp.group_allow_from);
        warn_open_policy_allowlist_alignment(
            "whatsapp",
            whatsapp.dm_policy,
            whatsapp.group_policy,
            &allowed_from,
            &group_allow_from,
        );
        InboundPolicyConfig {
            dm_policy: normalize_dm_policy("whatsapp", whatsapp.dm_policy),
            group_policy: whatsapp.group_policy,
            allowed_from,
            group_allow_from,
        }
    });
    let interrupt_on_new_message = config
        .channels_config
        .telegram
        .as_ref()
        .is_some_and(|tg| tg.interrupt_on_new_message);
    let bot_names = collect_bot_names(&config);
    let bot_uuids = collect_bot_uuids(&config);
    let mention_only_by_channel = collect_mention_only_by_channel(&config);
    let group_reply_mode_by_channel = collect_group_reply_mode_by_channel(&config);

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        config: shared_config,
        #[cfg(test)]
        config_generation,
        channels_by_name,
        provider: Arc::clone(&provider),
        default_provider: Arc::new(provider_name),
        memory: Arc::clone(&mem),
        tools_registry: Arc::clone(&tools_registry),
        observer,
        hooks,
        system_prompt: Arc::new(system_prompt),
        #[cfg(test)]
        model: Arc::new(model.clone()),
        #[cfg(test)]
        temperature,
        auto_save_memory: config.memory.auto_save && config.memory.semantic.auto_promote_user_messages,
        memory_event_recording: config.memory.event_recording_config(),
        max_tool_iterations: config.agent.max_tool_iterations,
        #[cfg(test)]
        read_only_tool_concurrency_window: config.agent.read_only_tool_concurrency_window,
        #[cfg(test)]
        read_only_tool_timeout_secs: config.agent.read_only_tool_timeout_secs,
        #[cfg(test)]
        priority_scheduling_enabled: config.agent.priority_scheduling_enabled,
        #[cfg(test)]
        low_priority_tools: config.agent.low_priority_tools.clone(),
        #[cfg(test)]
        min_relevance_score: config.memory.min_relevance_score,
        conversation_histories,
        provider_cache: Arc::new(Mutex::new(provider_cache_seed)),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        #[cfg(test)]
        api_key: config.api_key.clone(),
        #[cfg(test)]
        api_url: config.api_url.clone(),
        #[cfg(test)]
        reliability: Arc::new(config.reliability.clone()),
        provider_runtime_options,
        workspace_dir: Arc::new(config.workspace_dir.clone()),
        message_timeout_secs,
        interrupt_on_new_message,
        #[cfg(test)]
        multimodal: config.multimodal.clone(),
        // Preserve the legacy fixture only in unit-test builds. Production
        // authorization derives security from the per-message generation.
        #[cfg(test)]
        security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::clone(&security),
        })),
        #[cfg(test)]
        agent_compaction: config.agent.compaction.clone(),
        #[cfg(test)]
        tool_tiering: config.tool_tiering.clone(),
        signal_inbound_policy,
        whatsapp_inbound_policy,
        bot_names,
        bot_uuids,
        mention_only_by_channel,
        group_reply_mode_by_channel,
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        smart_group: config.smart_group.clone(),
        native_tools,
        skill_rag_ctx,
        #[cfg(test)]
        test_inbound_authorizer: None,
    });

    run_message_dispatch_loop(rx, runtime_ctx, max_in_flight_messages, shutdown).await;

    // Wait for all channel tasks. On shutdown the supervised listeners break out
    // of their loops (ListenerOutcome::Shutdown above), so these joins complete
    // instead of hanging on a still-running `listen()`.
    for h in handles {
        let _ = h.await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::needless_collect,
        clippy::unreadable_literal
    )]
    use super::*;
    use crate::memory::{Memory, MemoryCategory, MemoryPrincipal, SqliteMemory};
    use crate::observability::NoopObserver;
    use crate::providers::{ChatMessage, Provider};
    use crate::tools::{Tool, ToolResult};
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CHANNEL_MESSAGE_TIMEOUT_SECS: u64 = 300;
    use tempfile::TempDir;

    fn smart_test_msg(sender: &str, reply_target: &str) -> traits::ChannelMessage {
        traits::ChannelMessage {
            sender: sender.to_string(),
            reply_target: reply_target.to_string(),
            channel: "telegram".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn is_bot_sender_detects_bot_naming() {
        assert!(is_bot_sender(&smart_test_msg("helperbot", "g1")));
        assert!(is_bot_sender(&smart_test_msg("news-bot", "g1")));
        assert!(!is_bot_sender(&smart_test_msg("alice", "g1")));
        assert!(!is_bot_sender(&smart_test_msg("", "g1")));
    }

    #[test]
    fn is_bot_sender_trusts_platform_flag_over_name() {
        // Authoritative platform flag wins even when the name looks human — a bot
        // that does NOT name itself "*bot" is still caught (anti bot-to-bot loop).
        let mut msg = smart_test_msg("alice", "g1");
        msg.sender_is_bot = true;
        assert!(
            is_bot_sender(&msg),
            "sender_is_bot=true must mark a human-named account as a bot"
        );

        // Flag false + human name => not a bot (fallback heuristic does not match).
        let human = smart_test_msg("alice", "g1");
        assert!(!is_bot_sender(&human));
    }

    #[test]
    fn smart_proactive_cooldown_blocks_then_record_extends() {
        let ctx_cooldown: Arc<parking_lot::Mutex<HashMap<String, std::time::Instant>>> =
            Arc::new(parking_lot::Mutex::new(HashMap::new()));
        // Build a minimal ctx-like surface is heavy; instead test the pure key +
        // map semantics that the helpers rely on.
        let msg = smart_test_msg("alice", "group:teamchat");
        let key = smart_cooldown_key(&msg);
        assert_eq!(key, "telegram:teamchat");
        // Empty map => not within cooldown.
        {
            let guard = ctx_cooldown.lock();
            assert!(guard.get(&key).is_none());
        }
        // After inserting "now", a fresh lookup is within the window.
        {
            let mut guard = ctx_cooldown.lock();
            guard.insert(key.clone(), std::time::Instant::now());
        }
        {
            let guard = ctx_cooldown.lock();
            let within = guard.get(&key).is_some_and(|last| {
                std::time::Instant::now().duration_since(*last)
                    < std::time::Duration::from_secs(SMART_PROACTIVE_COOLDOWN_SECS)
            });
            assert!(within, "just-recorded proactive reply must be within cooldown");
        }
    }

    #[test]
    fn collect_group_reply_mode_resolves_smart_capable_channels() {
        // The collector only registers smart-capable channels (telegram/discord/
        // whatsapp) and resolves their effective mode from explicit + mention_only.
        let mut config = Config::default();
        config.channels_config.telegram = Some(crate::config::TelegramConfig {
            bot_token: "t".into(),
            allowed_users: vec!["*".into()],
            stream_mode: crate::config::StreamMode::default(),
            draft_update_interval_ms: 1000,
            interrupt_on_new_message: false,
            mention_only: true,
            group_reply_mode: Some(crate::config::GroupReplyMode::Smart),
        });
        config.channels_config.discord = Some(crate::config::DiscordConfig {
            bot_token: "d".into(),
            guild_id: None,
            allowed_users: vec!["*".into()],
            listen_to_bots: false,
            mention_only: true,
            group_reply_mode: None, // derive => MentionOnly
        });
        config.channels_config.whatsapp = Some(crate::config::WhatsAppConfig {
            allowed_numbers: vec!["*".into()],
            group_reply_mode: Some(crate::config::GroupReplyMode::Smart),
            ..Default::default()
        });
        config.channels_config.wacli = Some(crate::config::WacliConfig {
            enabled: true,
            group_reply_mode: Some(crate::config::GroupReplyMode::Smart),
            bot_jid: Some("123@s.whatsapp.net".into()),
            ..Default::default()
        });
        let modes = collect_group_reply_mode_by_channel(&config);
        assert_eq!(modes.get("telegram"), Some(&crate::config::GroupReplyMode::Smart));
        assert_eq!(modes.get("discord"), Some(&crate::config::GroupReplyMode::MentionOnly));
        assert_eq!(modes.get("whatsapp"), Some(&crate::config::GroupReplyMode::Smart));
        assert_eq!(modes.get("wacli"), Some(&crate::config::GroupReplyMode::Smart));
        assert!(
            !modes.contains_key("slack"),
            "non-smart-capable channels are not registered"
        );
    }

    /// WhatsApp without an explicit `group_reply_mode` derives the legacy
    /// `mention_only`-based mode, so existing configs are byte-for-byte unchanged.
    #[test]
    fn collect_group_reply_mode_whatsapp_derives_from_mention_only() {
        let mut config = Config::default();
        config.channels_config.whatsapp = Some(crate::config::WhatsAppConfig {
            allowed_numbers: vec!["*".into()],
            mention_only: true,
            group_reply_mode: None,
            ..Default::default()
        });
        let modes = collect_group_reply_mode_by_channel(&config);
        assert_eq!(
            modes.get("whatsapp"),
            Some(&crate::config::GroupReplyMode::MentionOnly),
            "mention_only=true + None => MentionOnly (legacy behavior preserved)"
        );

        config.channels_config.whatsapp = Some(crate::config::WhatsAppConfig {
            allowed_numbers: vec!["*".into()],
            mention_only: false,
            group_reply_mode: None,
            ..Default::default()
        });
        let modes = collect_group_reply_mode_by_channel(&config);
        assert_eq!(
            modes.get("whatsapp"),
            Some(&crate::config::GroupReplyMode::Off),
            "mention_only=false + None => Off (legacy behavior preserved)"
        );
    }

    /// Mirror of the central smart-gate decision for a WhatsApp `@g.us` group
    /// message: with whatsapp registered as Smart, `group_reply_mode_for` yields
    /// Smart and the `@g.us` reply_target makes it a group, so `smart_group` is
    /// true and the bot reads every message (model decides via stay_silent).
    #[test]
    fn whatsapp_group_message_triggers_smart_gate() {
        let mut ctx = ctx_with_histories(HashMap::new());
        ctx.group_reply_mode_by_channel
            .insert("whatsapp".to_string(), crate::config::GroupReplyMode::Smart);

        let msg = traits::ChannelMessage {
            channel: "whatsapp".to_string(),
            sender: "+123".to_string(),
            reply_target: "120363000000000000@g.us".to_string(),
            content: "anyone around?".to_string(),
            ..Default::default()
        };

        let mode = group_reply_mode_for(&ctx, &msg.channel);
        assert!(mode.is_smart(), "whatsapp resolves to Smart mode");

        // Central group detection: `@g.us` reply_target => group.
        let is_group = msg.reply_target.starts_with("group:") || msg.reply_target.ends_with("@g.us");
        assert!(is_group, "@g.us reply_target is recognized as a group");

        let in_group_for_smart = is_group || msg.is_group_hint;
        let smart_group = mode.is_smart() && in_group_for_smart && !is_system_message(&msg);
        assert!(smart_group, "smart gate is active for whatsapp @g.us group");
    }

    /// "@ always answers": an explicit @-mention in a WhatsApp group is detected
    /// by the central mention path (here via the channel-layer `mentioned` hint
    /// that WhatsApp Web sets from `context_info.mentioned_jid`), bypassing the
    /// proactive pre-gate/cooldown.
    #[test]
    fn whatsapp_group_at_mention_is_detected() {
        let ctx = ctx_with_histories(HashMap::new());
        let msg = traits::ChannelMessage {
            channel: "whatsapp".to_string(),
            sender: "+123".to_string(),
            reply_target: "120363000000000000@g.us".to_string(),
            content: "@bot please summarize".to_string(),
            mentioned: true, // set by whatsapp_web from mentioned_jid
            is_group_hint: true,
            ..Default::default()
        };
        let user_text = strip_channel_metadata(&msg.content);
        let mentioned = msg.mentioned || is_bot_mentioned(&ctx, &msg, &user_text);
        assert!(mentioned, "channel-layer @-mention hint marks the bot as addressed");

        // Text-based fallback also works when the body names the bot.
        let msg2 = traits::ChannelMessage {
            channel: "whatsapp".to_string(),
            reply_target: "120363000000000000@g.us".to_string(),
            content: "hey prx what's up".to_string(),
            ..Default::default()
        };
        let user_text2 = strip_channel_metadata(&msg2.content);
        assert!(
            msg2.mentioned || is_bot_mentioned(&ctx, &msg2, &user_text2),
            "text bot-name fallback still detects mention (bot_names = [\"prx\"])"
        );
    }

    /// Non-smart WhatsApp behavior is unchanged: when whatsapp is absent from the
    /// smart map (or mention_only-derived), `group_reply_mode_for` is NOT smart,
    /// so `smart_group` is false and the legacy mention_only drop path governs.
    #[test]
    fn whatsapp_non_smart_behavior_unchanged() {
        // No whatsapp entry at all => fall back to mention_only-derived mode.
        let mut ctx = ctx_with_histories(HashMap::new());
        ctx.mention_only_by_channel.insert("whatsapp".to_string(), true);
        let mode = group_reply_mode_for(&ctx, "whatsapp");
        assert_eq!(
            mode,
            crate::config::GroupReplyMode::MentionOnly,
            "absent + mention_only=true => MentionOnly, not Smart"
        );
        assert!(!mode.is_smart(), "non-smart whatsapp never enters the smart gate");

        // Explicit MentionOnly registration is likewise not smart.
        ctx.group_reply_mode_by_channel
            .insert("whatsapp".to_string(), crate::config::GroupReplyMode::MentionOnly);
        assert!(!group_reply_mode_for(&ctx, "whatsapp").is_smart());
    }

    /// A WhatsApp DM (no `@g.us`, no group hint) can never be smart, so the bot
    /// is never silenced and `stay_silent` is never exposed — even when whatsapp
    /// is configured Smart.
    #[test]
    fn whatsapp_dm_is_never_smart_silent() {
        let mut ctx = ctx_with_histories(HashMap::new());
        ctx.group_reply_mode_by_channel
            .insert("whatsapp".to_string(), crate::config::GroupReplyMode::Smart);
        let msg = traits::ChannelMessage {
            channel: "whatsapp".to_string(),
            sender: "+123".to_string(),
            reply_target: "+123".to_string(), // DM: a bare number, not @g.us
            content: "hi".to_string(),
            ..Default::default()
        };
        let is_group = msg.reply_target.starts_with("group:") || msg.reply_target.ends_with("@g.us");
        let in_group_for_smart = is_group || msg.is_group_hint;
        let mode = group_reply_mode_for(&ctx, &msg.channel);
        let smart_group = mode.is_smart() && in_group_for_smart && !is_system_message(&msg);
        assert!(!smart_group, "DMs are never smart => never silenced");
    }

    /// Signal (also `@g.us`-agnostic but using its own paths) is unaffected by
    /// extending smart to whatsapp: registering whatsapp Smart leaves signal's
    /// effective mode entirely mention_only-derived.
    #[test]
    fn signal_unaffected_by_whatsapp_smart() {
        let mut ctx = ctx_with_histories(HashMap::new());
        ctx.group_reply_mode_by_channel
            .insert("whatsapp".to_string(), crate::config::GroupReplyMode::Smart);
        ctx.mention_only_by_channel.insert("signal".to_string(), true);
        // signal has no smart registration => derived from mention_only.
        let signal_mode = group_reply_mode_for(&ctx, "signal");
        assert_eq!(
            signal_mode,
            crate::config::GroupReplyMode::MentionOnly,
            "signal stays mention_only-derived; whatsapp smart does not bleed over"
        );
        assert!(!signal_mode.is_smart(), "signal is never smart here");
    }

    /// Provider that returns a fixed string, recording how many classifier calls
    /// were made — lets the pre-gate "token path" tests assert that the cheap
    /// model is consulted ONLY for the uncertain bucket.
    struct CountingProvider {
        answer: String,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for CountingProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.answer.clone())
        }
    }

    fn pre_gate_history_key() -> ConversationKey {
        ConversationKey {
            canonical: "channel:telegram:alice:teamchat".to_string(),
            legacy: "telegram_alice".to_string(),
        }
    }

    #[tokio::test]
    async fn pre_gate_disabled_always_enters_loop() {
        let mut ctx = ctx_with_histories(HashMap::new());
        ctx.smart_group.enabled = false;
        let provider: Arc<dyn Provider> = Arc::new(CountingProvider {
            answer: "NO".into(),
            calls: AtomicUsize::new(0),
        });
        let generation = ctx.config.pin();
        // Even obvious noise enters the loop when the pre-gate is off.
        let outcome = run_smart_pre_gate(
            &ctx,
            &generation,
            &pre_gate_history_key(),
            "lol",
            "test-provider",
            "test-model",
            &provider,
        )
        .await;
        assert!(outcome.should_enter_loop());
        assert_eq!(outcome.path, pre_gate::PreGatePath::Disabled);
    }

    #[tokio::test]
    async fn pre_gate_heuristic_skip_never_calls_model() {
        // Token path: obvious noise is skipped by the 0-token heuristic; the cheap
        // classifier is NEVER invoked.
        let ctx = ctx_with_histories(HashMap::new());
        let counting = Arc::new(CountingProvider {
            answer: "YES".into(),
            calls: AtomicUsize::new(0),
        });
        let provider: Arc<dyn Provider> = counting.clone();
        let generation = ctx.config.pin();
        let outcome = run_smart_pre_gate(
            &ctx,
            &generation,
            &pre_gate_history_key(),
            "😂😂",
            "test-provider",
            "test-model",
            &provider,
        )
        .await;
        assert!(!outcome.should_enter_loop(), "pure emoji must be skipped");
        assert_eq!(outcome.path, pre_gate::PreGatePath::HeuristicSkip);
        assert_eq!(
            counting.calls.load(Ordering::SeqCst),
            0,
            "heuristic skip must not consult the classifier (token saving)"
        );
    }

    #[tokio::test]
    async fn pre_gate_heuristic_enter_never_calls_model() {
        // Token path: a clear question enters the loop via the heuristic with no
        // classifier call.
        let ctx = ctx_with_histories(HashMap::new());
        let counting = Arc::new(CountingProvider {
            answer: "NO".into(),
            calls: AtomicUsize::new(0),
        });
        let provider: Arc<dyn Provider> = counting.clone();
        let generation = ctx.config.pin();
        let outcome = run_smart_pre_gate(
            &ctx,
            &generation,
            &pre_gate_history_key(),
            "what is the deploy status?",
            "test-provider",
            "test-model",
            &provider,
        )
        .await;
        assert!(outcome.should_enter_loop());
        assert_eq!(outcome.path, pre_gate::PreGatePath::HeuristicEnter);
        assert_eq!(
            counting.calls.load(Ordering::SeqCst),
            0,
            "heuristic enter must not consult the classifier (token saving)"
        );
    }

    #[tokio::test]
    async fn pre_gate_uncertain_consults_classifier_and_can_skip() {
        // Token path: only the genuinely uncertain bucket pays for the cheap model.
        let ctx = ctx_with_histories(HashMap::new());
        let counting = Arc::new(CountingProvider {
            answer: "NO".into(),
            calls: AtomicUsize::new(0),
        });
        let provider: Arc<dyn Provider> = counting.clone();
        let generation = ctx.config.pin();
        let outcome = run_smart_pre_gate(
            &ctx,
            &generation,
            &pre_gate_history_key(),
            "the deploy went out an hour ago to all prod servers",
            "test-provider",
            "test-model",
            &provider,
        )
        .await;
        assert!(!outcome.should_enter_loop(), "classifier NO should skip");
        assert_eq!(outcome.path, pre_gate::PreGatePath::ClassifierSkip);
        assert_eq!(
            counting.calls.load(Ordering::SeqCst),
            1,
            "uncertain bucket consults the classifier exactly once"
        );
    }

    #[tokio::test]
    async fn pre_gate_uncertain_with_classifier_disabled_fails_open() {
        // Fail-open: classifier off => uncertain enters the loop, no model call.
        let mut ctx = ctx_with_histories(HashMap::new());
        ctx.smart_group.classifier_enabled = false;
        let counting = Arc::new(CountingProvider {
            answer: "NO".into(),
            calls: AtomicUsize::new(0),
        });
        let provider: Arc<dyn Provider> = counting.clone();
        let generation = ctx.config.pin();
        let outcome = run_smart_pre_gate(
            &ctx,
            &generation,
            &pre_gate_history_key(),
            "the deploy went out an hour ago to all prod servers",
            "test-provider",
            "test-model",
            &provider,
        )
        .await;
        assert!(outcome.should_enter_loop(), "classifier disabled must fail open");
        assert_eq!(outcome.path, pre_gate::PreGatePath::ClassifierFailOpen);
        assert_eq!(
            counting.calls.load(Ordering::SeqCst),
            0,
            "classifier disabled => no call"
        );
    }

    fn make_workspace() -> TempDir {
        let tmp = TempDir::new().unwrap();
        // Create minimal workspace files
        std::fs::write(tmp.path().join("SOUL.md"), "# Soul\nBe helpful.").unwrap();
        std::fs::write(tmp.path().join("IDENTITY.md"), "# Identity\nName: OpenPRX").unwrap();
        std::fs::write(tmp.path().join("USER.md"), "# User\nName: Test User").unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "# Agents\nFollow instructions.").unwrap();
        std::fs::write(tmp.path().join("TOOLS.md"), "# Tools\nUse shell carefully.").unwrap();
        std::fs::write(tmp.path().join("HEARTBEAT.md"), "# Heartbeat\nCheck status.").unwrap();
        std::fs::write(tmp.path().join("MEMORY.md"), "# Memory\nUser likes Rust.").unwrap();
        tmp
    }

    #[test]
    fn effective_channel_message_timeout_secs_clamps_to_minimum() {
        assert_eq!(
            effective_channel_message_timeout_secs(0),
            MIN_CHANNEL_MESSAGE_TIMEOUT_SECS
        );
        assert_eq!(
            effective_channel_message_timeout_secs(15),
            MIN_CHANNEL_MESSAGE_TIMEOUT_SECS
        );
        assert_eq!(effective_channel_message_timeout_secs(300), 300);
    }

    #[test]
    fn channel_message_timeout_budget_scales_with_tool_iterations() {
        assert_eq!(channel_message_timeout_budget_secs(300, 1), 300);
        assert_eq!(channel_message_timeout_budget_secs(300, 2), 600);
        assert_eq!(channel_message_timeout_budget_secs(300, 3), 900);
    }

    #[test]
    fn channel_message_timeout_budget_uses_safe_defaults_and_cap() {
        // 0 iterations falls back to 1x timeout budget.
        assert_eq!(channel_message_timeout_budget_secs(300, 0), 300);
        // Large iteration counts are capped to avoid runaway waits.
        assert_eq!(
            channel_message_timeout_budget_secs(300, 10),
            300 * CHANNEL_MESSAGE_TIMEOUT_SCALE_CAP
        );
    }

    #[test]
    fn context_window_overflow_error_detector_matches_known_messages() {
        let overflow_err =
            anyhow::anyhow!("OpenAI Codex stream error: Your input exceeds the context window of this model.");
        assert!(is_context_window_overflow_error(&overflow_err));

        let other_err = anyhow::anyhow!("OpenAI Codex API error (502 Bad Gateway): error code: 502");
        assert!(!is_context_window_overflow_error(&other_err));
    }

    #[test]
    fn memory_context_skip_rules_exclude_history_blobs() {
        assert!(should_skip_memory_context_entry(
            "telegram_123_history",
            r#"[{"role":"user"}]"#
        ));
        assert!(should_skip_memory_context_entry(
            "assistant_resp_legacy",
            "fabricated memory"
        ));
        assert!(!should_skip_memory_context_entry("telegram_123_45", "hi"));
    }

    #[test]
    fn normalize_cached_channel_turns_merges_consecutive_user_turns() {
        let turns = vec![
            ChatMessage::user("forwarded content"),
            ChatMessage::user("summarize this"),
        ];

        let normalized = normalize_cached_channel_turns(turns);
        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized[0].role, "user");
        assert!(normalized[0].content.contains("forwarded content"));
        assert!(normalized[0].content.contains("summarize this"));
    }

    #[test]
    fn normalize_cached_channel_turns_merges_consecutive_assistant_turns() {
        let turns = vec![
            ChatMessage::user("first user"),
            ChatMessage::assistant("assistant part 1"),
            ChatMessage::assistant("assistant part 2"),
            ChatMessage::user("next user"),
        ];

        let normalized = normalize_cached_channel_turns(turns);
        assert_eq!(normalized.len(), 3);
        assert_eq!(normalized[0].role, "user");
        assert_eq!(normalized[1].role, "assistant");
        assert_eq!(normalized[2].role, "user");
        assert!(normalized[1].content.contains("assistant part 1"));
        assert!(normalized[1].content.contains("assistant part 2"));
    }

    #[test]
    fn compact_sender_history_keeps_recent_truncated_messages() {
        let mut histories = HashMap::new();
        let sender = ConversationKey {
            canonical: "channel:telegram:u1:chat-1".to_string(),
            legacy: "telegram_u1".to_string(),
        };
        histories.insert(
            sender.canonical.clone(),
            (0..20)
                .map(|idx| {
                    let content = format!("msg-{idx}-{}", "x".repeat(700));
                    if idx % 2 == 0 {
                        ChatMessage::user(content)
                    } else {
                        ChatMessage::assistant(content)
                    }
                })
                .collect::<Vec<_>>(),
        );

        let ctx = ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(HashMap::new()),
            provider: Arc::new(DummyProvider),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("system".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 5,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(histories)),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        };

        assert!(compact_sender_history(&ctx, &sender));

        let histories = ctx.conversation_histories.lock();
        let kept = histories.get(&sender.canonical).expect("sender history should remain");
        assert!(!kept.is_empty());
        assert!(kept.len() <= CHANNEL_HISTORY_COMPACT_KEEP_MESSAGES);
        assert!(kept.iter().all(|turn| {
            let len = turn.content.chars().count();
            len <= CHANNEL_HISTORY_COMPACT_CONTENT_CHARS
                || (len <= CHANNEL_HISTORY_COMPACT_CONTENT_CHARS + 3 && turn.content.ends_with("..."))
        }));
        let total_chars: usize = kept.iter().map(|turn| turn.content.chars().count()).sum();
        assert!(
            total_chars <= CHANNEL_HISTORY_COMPACT_TOTAL_CHARS,
            "total chars {} exceeds budget {}",
            total_chars,
            CHANNEL_HISTORY_COMPACT_TOTAL_CHARS
        );
    }

    /// Build a minimal `ChannelRuntimeContext` whose conversation history map is
    /// seeded with `histories`, for exercising compact/clear in isolation.
    fn ctx_with_histories(histories: HashMap<String, Vec<ChatMessage>>) -> ChannelRuntimeContext {
        ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(HashMap::new()),
            provider: Arc::new(DummyProvider),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("system".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 5,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(histories)),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        }
    }

    #[test]
    fn channel_message_admission_pins_the_current_manager_generation() {
        let manager = crate::config::new_shared(Config::default());
        let initial_generation = manager.pin();
        let mut ctx = ctx_with_histories(HashMap::new());
        ctx.config = Arc::clone(&manager);
        ctx.config_generation = Arc::clone(&initial_generation);

        let mut desired = (*initial_generation.effective).clone();
        desired.default_temperature = 0.42;
        desired.agent.max_tool_iterations = 17;
        desired.agent.read_only_tool_concurrency_window = 7;
        manager
            .apply_runtime_config(desired, crate::config::ConfigReloadTrigger::Test)
            .expect("snapshot-hot channel fields must publish");

        let current_generation = manager.pin();
        ctx.config_generation = Arc::clone(&current_generation);
        let current_defaults = runtime_defaults_for_generation(&ctx, &current_generation);
        let current_message = message_runtime_snapshot(&ctx, &current_generation);
        let pinned_before_reload = message_runtime_snapshot(&ctx, &initial_generation);

        assert_ne!(current_generation.id, initial_generation.id);
        assert_eq!(current_defaults.temperature, 0.42);
        assert_eq!(current_message.max_tool_iterations, 17);
        assert_eq!(current_message.read_only_tool_concurrency_window, 7);
        assert_ne!(
            current_message.max_tool_iterations,
            pinned_before_reload.max_tool_iterations
        );
    }

    #[test]
    fn compact_sender_history_never_removes_shared_legacy() {
        // FIX-P1-25b bug 2: compaction must operate on the canonical session only.
        // The legacy entry is the immutable, cross-recipient shared pre-history;
        // compacting one recipient's session must not delete or mutate it, so the
        // sender's other recipients still read the legacy pre-history afterwards.
        let legacy = "telegram_alice".to_string();
        let key_a = ConversationKey {
            canonical: "channel:telegram:alice:chat-a".to_string(),
            legacy: legacy.clone(),
        };
        let key_b = ConversationKey {
            canonical: "channel:telegram:alice:chat-b".to_string(),
            legacy: legacy.clone(),
        };

        let mut histories = HashMap::new();
        histories.insert(legacy.clone(), vec![ChatMessage::user("legacy-prehistory")]);
        // A canonical session long enough to actually compact.
        histories.insert(
            key_a.canonical.clone(),
            (0..20)
                .map(|idx| ChatMessage::user(format!("a-{idx}")))
                .collect::<Vec<_>>(),
        );
        let ctx = ctx_with_histories(histories);

        assert!(compact_sender_history(&ctx, &key_a));

        let histories = ctx.conversation_histories.lock();
        // Legacy untouched.
        assert_eq!(
            histories
                .get(&legacy)
                .map(|t| t.iter().map(|m| m.content.as_str()).collect::<Vec<_>>()),
            Some(vec!["legacy-prehistory"]),
            "compaction must not delete or mutate the shared legacy entry"
        );
        // Recipient A's canonical was compacted.
        let kept_a = histories.get(&key_a.canonical).expect("canonical A remains");
        assert!(kept_a.len() <= CHANNEL_HISTORY_COMPACT_KEEP_MESSAGES);
        drop(histories);

        // Recipient B (same sender) still reads the legacy pre-history via merge.
        let merged_b = merged_history(&ctx.conversation_histories.lock(), &key_b);
        assert_eq!(
            merged_b.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            vec!["legacy-prehistory"],
            "other recipient still sees shared legacy after compaction of recipient A"
        );
    }

    #[test]
    fn clear_sender_history_clears_canonical_only_preserving_shared_legacy() {
        // FIX-P1-25b bug 2: clear (e.g. /model, /models) clears the CURRENT
        // canonical session only. The legacy pre-history is immutable and shared
        // across recipients, so it must survive — both for the cleared recipient
        // (as historical pre-context) and for the sender's other recipients.
        let legacy = "telegram_alice".to_string();
        let key_a = ConversationKey {
            canonical: "channel:telegram:alice:chat-a".to_string(),
            legacy: legacy.clone(),
        };
        let key_b = ConversationKey {
            canonical: "channel:telegram:alice:chat-b".to_string(),
            legacy: legacy.clone(),
        };

        let mut histories = HashMap::new();
        histories.insert(legacy.clone(), vec![ChatMessage::user("legacy-prehistory")]);
        histories.insert(key_a.canonical.clone(), vec![ChatMessage::assistant("a-turn")]);
        histories.insert(key_b.canonical.clone(), vec![ChatMessage::assistant("b-turn")]);
        let ctx = ctx_with_histories(histories);

        clear_sender_history(&ctx, &key_a);

        let histories = ctx.conversation_histories.lock();
        // Canonical A is cleared; legacy and canonical B are untouched.
        assert!(
            histories.get(&key_a.canonical).is_none(),
            "cleared recipient's canonical gone"
        );
        assert_eq!(
            histories
                .get(&legacy)
                .map(|t| t.iter().map(|m| m.content.as_str()).collect::<Vec<_>>()),
            Some(vec!["legacy-prehistory"]),
            "clear must not delete the shared legacy entry"
        );
        assert!(
            histories.get(&key_b.canonical).is_some(),
            "other recipient's canonical untouched"
        );
        drop(histories);

        // Cleared recipient A: canonical empty but legacy pre-history still visible.
        let merged_a = merged_history(&ctx.conversation_histories.lock(), &key_a);
        assert_eq!(
            merged_a.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            vec!["legacy-prehistory"],
            "cleared recipient still sees immutable legacy pre-history"
        );
        // Other recipient B: legacy pre-history + its own canonical turn.
        let merged_b = merged_history(&ctx.conversation_histories.lock(), &key_b);
        assert_eq!(
            merged_b.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            vec!["legacy-prehistory", "b-turn"],
            "other recipient retains legacy pre-history and own canonical turn"
        );
    }

    #[test]
    fn merged_history_unions_legacy_prehistory_with_canonical_turns_in_order() {
        // FIX-P1-25b bug 1 (read-merge, not move): a session that was migrated
        // and then ran once stores old turns under the legacy key and new turns
        // under the canonical key. The read path must surface the ordered union
        // (legacy first, then canonical), not just the canonical slice.
        let key = ConversationKey {
            canonical: "channel:telegram:u1:chat-1".to_string(),
            legacy: "telegram_u1".to_string(),
        };
        let mut map: HashMap<String, Vec<ChatMessage>> = HashMap::new();
        map.insert(
            key.legacy.clone(),
            vec![ChatMessage::user("old-question"), ChatMessage::assistant("old-answer")],
        );
        map.insert(
            key.canonical.clone(),
            vec![ChatMessage::user("new-question"), ChatMessage::assistant("new-answer")],
        );

        let merged = merged_history(&map, &key);
        let rendered: Vec<&str> = merged.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            rendered,
            vec!["old-question", "old-answer", "new-question", "new-answer"],
            "merged read must include legacy pre-history then canonical turns, in order"
        );
        // Read-merge must not mutate the underlying map (no move/delete of legacy).
        assert_eq!(map.get(&key.legacy).map(Vec::len), Some(2));
        assert_eq!(map.get(&key.canonical).map(Vec::len), Some(2));
    }

    #[test]
    fn merged_history_preserves_real_repeats_and_truncates_to_window() {
        let key = ConversationKey {
            canonical: "channel:telegram:u1:chat-1".to_string(),
            legacy: "telegram_u1".to_string(),
        };
        // Real, intentional repeats within a conversation (e.g. the user sends the
        // same text twice in a row) MUST be preserved. legacy and canonical keys
        // partition the timeline (each physical turn lives under exactly one key),
        // so there is no cross-store duplication to collapse — and a content-based
        // dedup would wrongly delete these genuine repeats. Pure concatenation.
        let mut map: HashMap<String, Vec<ChatMessage>> = HashMap::new();
        map.insert(
            key.legacy.clone(),
            // Two identical legacy turns + a distinct one.
            vec![
                ChatMessage::user("ping"),
                ChatMessage::user("ping"),
                ChatMessage::assistant("pong"),
            ],
        );
        map.insert(
            key.canonical.clone(),
            // The same "ping" content again post-cutover plus a repeated canonical turn.
            vec![
                ChatMessage::user("ping"),
                ChatMessage::assistant("ok"),
                ChatMessage::assistant("ok"),
            ],
        );
        let merged = merged_history(&map, &key);
        let rendered: Vec<&str> = merged.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            rendered,
            vec!["ping", "ping", "pong", "ping", "ok", "ok"],
            "all turns preserved in order; no dedup of genuine repeats across or within stores"
        );

        // Window truncation keeps the most recent turns across the union.
        let mut big: HashMap<String, Vec<ChatMessage>> = HashMap::new();
        big.insert(
            key.legacy.clone(),
            (0..MAX_CHANNEL_HISTORY)
                .map(|i| ChatMessage::user(format!("legacy-{i}")))
                .collect(),
        );
        big.insert(
            key.canonical.clone(),
            (0..MAX_CHANNEL_HISTORY)
                .map(|i| ChatMessage::user(format!("canonical-{i}")))
                .collect(),
        );
        let merged = merged_history(&big, &key);
        assert_eq!(merged.len(), MAX_CHANNEL_HISTORY);
        // The newest canonical turn survives; the oldest legacy turn is dropped.
        assert_eq!(
            merged.last().map(|m| m.content.as_str()),
            Some(format!("canonical-{}", MAX_CHANNEL_HISTORY - 1).as_str())
        );
        assert!(merged.iter().all(|m| m.content != "legacy-0"));
    }

    #[test]
    fn merged_history_shares_legacy_prehistory_across_recipients() {
        // FIX-P1-25b bug 2 (recipient dimension): the legacy key is sender-scoped
        // (no recipient), so the same sender's two canonical conversations (one per
        // recipient) both read the legacy pre-history, while their new canonical
        // turns stay separate (no cross-talk).
        let mut map: HashMap<String, Vec<ChatMessage>> = HashMap::new();
        let legacy = "telegram_alice".to_string();
        map.insert(legacy.clone(), vec![ChatMessage::user("legacy-shared-by-alice")]);
        let key_a = ConversationKey {
            canonical: "channel:telegram:alice:chat-a".to_string(),
            legacy: legacy.clone(),
        };
        let key_b = ConversationKey {
            canonical: "channel:telegram:alice:chat-b".to_string(),
            legacy,
        };
        map.insert(key_a.canonical.clone(), vec![ChatMessage::assistant("reply-in-a")]);
        map.insert(key_b.canonical.clone(), vec![ChatMessage::assistant("reply-in-b")]);

        let merged_a = merged_history(&map, &key_a);
        let merged_b = merged_history(&map, &key_b);
        let rendered_a: Vec<&str> = merged_a.iter().map(|m| m.content.as_str()).collect();
        let rendered_b: Vec<&str> = merged_b.iter().map(|m| m.content.as_str()).collect();

        assert_eq!(rendered_a, vec!["legacy-shared-by-alice", "reply-in-a"]);
        assert_eq!(rendered_b, vec!["legacy-shared-by-alice", "reply-in-b"]);
        // No cross-talk: recipient A never sees recipient B's canonical turn.
        assert!(!rendered_a.contains(&"reply-in-b"));
        assert!(!rendered_b.contains(&"reply-in-a"));
    }

    #[tokio::test]
    async fn load_persisted_histories_rehydrates_legacy_and_canonical_for_merge() {
        // FIX-P1-25b bug 1 end-to-end: persist old turns under the legacy key and
        // new turns under the canonical key (the "migrated then ran once" state),
        // then restart-hydrate. The hydrated map must keep both keys so a merged
        // read returns the full ordered union — proving no read window is lost
        // across a restart.
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());

        let key = ConversationKey {
            canonical: "channel:telegram:u1:chat-1".to_string(),
            legacy: "telegram_u1".to_string(),
        };

        // Pre-cutover turns persisted under the legacy session_key.
        memory
            .append_conversation_turn(
                &key.legacy,
                "telegram",
                "u1",
                "user",
                "old-question",
                Some("2026-05-20T00:00:00Z"),
                Some("m1"),
                Some("system:legacy"),
            )
            .await
            .unwrap();
        memory
            .append_conversation_turn(
                &key.legacy,
                "telegram",
                "u1",
                "assistant",
                "old-answer",
                Some("2026-05-20T00:00:01Z"),
                Some("m2"),
                Some("system:legacy"),
            )
            .await
            .unwrap();
        // Post-cutover turns persisted under the canonical session_key.
        memory
            .append_conversation_turn(
                &key.canonical,
                "telegram",
                "u1",
                "user",
                "new-question",
                Some("2026-05-21T00:00:00Z"),
                Some("m3"),
                Some("system:canonical"),
            )
            .await
            .unwrap();
        memory
            .append_conversation_turn(
                &key.canonical,
                "telegram",
                "u1",
                "assistant",
                "new-answer",
                Some("2026-05-21T00:00:01Z"),
                Some("m4"),
                Some("system:canonical"),
            )
            .await
            .unwrap();

        let hydrated = load_persisted_histories(tmp.path(), memory.as_ref()).await;
        // Both keys must survive hydration (no merge-at-load), so the per-message
        // ConversationKey can union them on read.
        assert!(hydrated.contains_key(&key.legacy), "legacy session must hydrate");
        assert!(hydrated.contains_key(&key.canonical), "canonical session must hydrate");

        let merged = merged_history(&hydrated, &key);
        let rendered: Vec<&str> = merged.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            rendered,
            vec!["old-question", "old-answer", "new-question", "new-answer"],
            "post-restart merged read must include legacy pre-history + canonical turns in order"
        );
    }

    #[tokio::test]
    async fn append_sender_turn_records_channel_message_event() {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let ctx = ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(HashMap::new()),
            provider: Arc::new(DummyProvider),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::clone(&memory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(tmp.path().to_path_buf())),
            system_prompt: Arc::new("system".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 5,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(tmp.path().to_path_buf()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        };

        let test_generation = Arc::clone(&ctx.config_generation);
        let _ = append_sender_turn(
            &ctx,
            &test_generation,
            &ConversationKey {
                canonical: "channel:telegram:sender-1:sender-1".to_string(),
                legacy: "telegram_sender-1".to_string(),
            },
            "telegram",
            "sender-1",
            Some("sender-1"),
            ChatMessage::user("hello from telegram"),
            MemoryVisibility::Workspace,
            Some("2026-05-21T00:00:00Z"),
            Some("msg-1"),
            "turn-run-id-1",
            true,
        )
        .await;

        let events = memory
            .list_message_events_since(
                &MemoryPrincipal {
                    workspace_id: tmp.path().to_string_lossy().to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: Some("telegram_sender-1".to_string()),
                    channel: Some("telegram".to_string()),
                    sender: Some("sender-1".to_string()),
                    owner_id: None,
                    legacy_session_key: None,
                },
                0,
                10,
            )
            .await
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, "channel");
        let expected_owner = format!("owner:{}:telegram:sender-1", tmp.path().to_string_lossy());
        assert_eq!(events[0].owner_id.as_deref(), Some(expected_owner.as_str()));
        assert_eq!(events[0].channel.as_deref(), Some("telegram"));
        assert_eq!(events[0].session_key.as_deref(), Some("telegram_sender-1"));
        assert_eq!(events[0].role, "user");
        assert_eq!(events[0].content, "hello from telegram");
        // D8-1: the per-turn run_id threaded through append_sender_turn lands on
        // the recorded channel message event (non-empty, exact value).
        assert_eq!(events[0].run_id.as_deref(), Some("turn-run-id-1"));
    }

    #[test]
    fn inbound_policy_allowlist_rejects_unknown_direct_sender() {
        let policy = InboundPolicyConfig {
            dm_policy: crate::config::DmPolicy::Allowlist,
            group_policy: crate::config::GroupPolicy::Allowlist,
            allowed_from: normalize_allowlist(&["+1111111111".to_string()]),
            group_allow_from: HashSet::new(),
        };
        let msg = traits::ChannelMessage {
            id: "1".to_string(),
            sender: "+2222222222".to_string(),
            reply_target: "+2222222222".to_string(),
            content: "hello".to_string(),
            channel: "signal".to_string(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        };
        assert!(!evaluate_inbound_policy(&policy, &msg));
    }

    #[test]
    fn inbound_policy_open_with_wildcard_allowlist_allows_direct_sender() {
        let policy = InboundPolicyConfig {
            dm_policy: crate::config::DmPolicy::Open,
            group_policy: crate::config::GroupPolicy::Allowlist,
            allowed_from: normalize_allowlist(&["*".to_string()]),
            group_allow_from: HashSet::new(),
        };
        let msg = traits::ChannelMessage {
            id: "dm-open".to_string(),
            sender: "+19999999999".to_string(),
            reply_target: "+19999999999".to_string(),
            content: "hello".to_string(),
            channel: "signal".to_string(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        };
        assert!(evaluate_inbound_policy(&policy, &msg));
    }

    #[test]
    fn structured_chat_kind_drives_group_scope_for_bare_telegram_targets() {
        let msg = traits::ChannelMessage {
            id: "tg-group".to_string(),
            sender: "alice".to_string(),
            reply_target: "-100200300".to_string(),
            content: "[Telegram Group] Alice: group update".to_string(),
            channel: "telegram".to_string(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: ChatKind::Group,
            chat_title: Some("Build Room".to_string()),
            sender_display: Some("Alice".to_string()),
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: true,
            sender_is_bot: false,
        };

        assert_eq!(infer_chat_type_from_message(&msg), "group");
        assert_eq!(channel_message_visibility(&msg), MemoryVisibility::Session);
    }

    #[test]
    fn inbound_policy_group_disabled_rejects_group_message() {
        let policy = InboundPolicyConfig {
            dm_policy: crate::config::DmPolicy::Open,
            group_policy: crate::config::GroupPolicy::Disabled,
            allowed_from: normalize_allowlist(&["*".to_string()]),
            group_allow_from: normalize_allowlist(&["group-1".to_string()]),
        };
        let msg = traits::ChannelMessage {
            id: "grp-disabled".to_string(),
            sender: "+1111111111".to_string(),
            reply_target: "group:group-1".to_string(),
            content: "group hello".to_string(),
            channel: "signal".to_string(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        };
        assert!(!evaluate_inbound_policy(&policy, &msg));
    }

    #[test]
    fn inbound_policy_pairing_follows_allowlist_behavior() {
        let policy = InboundPolicyConfig {
            dm_policy: crate::config::DmPolicy::Pairing,
            group_policy: crate::config::GroupPolicy::Allowlist,
            allowed_from: normalize_allowlist(&["+10000000000".to_string()]),
            group_allow_from: HashSet::new(),
        };
        let msg = traits::ChannelMessage {
            id: "dm-pairing".to_string(),
            sender: "+10000000000".to_string(),
            reply_target: "+10000000000".to_string(),
            content: "hello".to_string(),
            channel: "signal".to_string(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        };
        assert!(evaluate_inbound_policy(&policy, &msg));

        let blocked = traits::ChannelMessage {
            sender: "+20000000000".to_string(),
            ..msg
        };
        assert!(!evaluate_inbound_policy(&policy, &blocked));
    }

    #[test]
    fn normalize_dm_policy_falls_back_pairing_to_allowlist() {
        assert_eq!(
            normalize_dm_policy("signal", crate::config::DmPolicy::Pairing),
            crate::config::DmPolicy::Allowlist
        );
        assert_eq!(
            normalize_dm_policy("signal", crate::config::DmPolicy::Open),
            crate::config::DmPolicy::Open
        );
    }

    #[test]
    fn inbound_policy_group_allowlist_accepts_allowed_group() {
        let policy = InboundPolicyConfig {
            dm_policy: crate::config::DmPolicy::Allowlist,
            group_policy: crate::config::GroupPolicy::Allowlist,
            allowed_from: HashSet::new(),
            group_allow_from: normalize_allowlist(&["group-1".to_string()]),
        };
        let msg = traits::ChannelMessage {
            id: "2".to_string(),
            sender: "+1111111111".to_string(),
            reply_target: "group:group-1".to_string(),
            content: "group hello".to_string(),
            channel: "signal".to_string(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        };
        assert!(evaluate_inbound_policy(&policy, &msg));
    }

    struct DummyProvider;

    #[async_trait::async_trait]
    impl Provider for DummyProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("ok".to_string())
        }
    }

    #[derive(Default)]
    struct RecordingChannel {
        sent_messages: tokio::sync::Mutex<Vec<String>>,
        start_typing_calls: AtomicUsize,
        stop_typing_calls: AtomicUsize,
    }

    #[derive(Default)]
    struct TelegramRecordingChannel {
        sent_messages: tokio::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Channel for TelegramRecordingChannel {
        fn name(&self) -> &str {
            "telegram"
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent_messages
                .lock()
                .await
                .push(format!("{}:{}", message.recipient, message.content));
            Ok(())
        }

        async fn listen(&self, _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>) -> anyhow::Result<()> {
            Ok(())
        }

        async fn start_typing(&self, _recipient: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Channel for RecordingChannel {
        fn name(&self) -> &str {
            "test-channel"
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent_messages
                .lock()
                .await
                .push(format!("{}:{}", message.recipient, message.content));
            Ok(())
        }

        async fn listen(&self, _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>) -> anyhow::Result<()> {
            Ok(())
        }

        async fn start_typing(&self, _recipient: &str) -> anyhow::Result<()> {
            self.start_typing_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
            self.stop_typing_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Recording channel registered under the `signal` name, so the Signal
    /// vision-fallback path (which resolves `target_channel` by `msg.channel`)
    /// can observe whether the fallback reply was actually sent. Mirrors
    /// `RecordingChannel` but reports `name() == "signal"`.
    #[derive(Default)]
    struct SignalRecordingChannel {
        sent_messages: tokio::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Channel for SignalRecordingChannel {
        fn name(&self) -> &str {
            "signal"
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent_messages
                .lock()
                .await
                .push(format!("{}:{}", message.recipient, message.content));
            Ok(())
        }

        async fn listen(&self, _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>) -> anyhow::Result<()> {
            Ok(())
        }

        async fn start_typing(&self, _recipient: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct SlowProvider {
        delay: Duration,
    }

    #[async_trait::async_trait]
    impl Provider for SlowProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            tokio::time::sleep(self.delay).await;
            Ok(format!("echo: {message}"))
        }
    }

    struct ToolCallingProvider;

    fn tool_call_payload() -> String {
        r#"<tool_call>
{"name":"mock_price","arguments":{"symbol":"BTC"}}
</tool_call>"#
            .to_string()
    }

    fn tool_call_payload_with_alias_tag() -> String {
        r#"<toolcall>
{"name":"mock_price","arguments":{"symbol":"BTC"}}
</toolcall>"#
            .to_string()
    }

    #[async_trait::async_trait]
    impl Provider for ToolCallingProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(tool_call_payload())
        }

        async fn chat_with_history(
            &self,
            messages: &[ChatMessage],
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            let has_tool_results = messages
                .iter()
                .any(|msg| msg.role == "user" && msg.content.contains("[Tool results]"));
            if has_tool_results {
                Ok("BTC is currently around $65,000 based on latest tool output.".to_string())
            } else {
                Ok(tool_call_payload())
            }
        }
    }

    struct ToolCallingAliasProvider;

    #[async_trait::async_trait]
    impl Provider for ToolCallingAliasProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(tool_call_payload_with_alias_tag())
        }

        async fn chat_with_history(
            &self,
            messages: &[ChatMessage],
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            let has_tool_results = messages
                .iter()
                .any(|msg| msg.role == "user" && msg.content.contains("[Tool results]"));
            if has_tool_results {
                Ok("BTC alias-tag flow resolved to final text output.".to_string())
            } else {
                Ok(tool_call_payload_with_alias_tag())
            }
        }
    }

    struct RawToolArtifactProvider;

    #[async_trait::async_trait]
    impl Provider for RawToolArtifactProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("fallback".to_string())
        }

        async fn chat_with_history(
            &self,
            _messages: &[ChatMessage],
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(r#"{"name":"mock_price","parameters":{"symbol":"BTC"}}
{"result":{"symbol":"BTC","price_usd":65000}}
BTC is currently around $65,000 based on latest tool output."#
                .to_string())
        }
    }

    #[derive(Default)]
    struct SignalVisionNoResultProvider {
        call_count: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for SignalVisionNoResultProvider {
        fn capabilities(&self) -> providers::ProviderCapabilities {
            providers::ProviderCapabilities {
                native_tool_calling: false,
                vision: true,
            }
        }

        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }

        async fn chat_with_history(
            &self,
            _messages: &[ChatMessage],
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            let call_index = self.call_count.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                Ok(String::new())
            } else {
                Ok("这是维生素C产品包装。".to_string())
            }
        }
    }

    #[derive(Default)]
    struct SignalTextOnlyProvider {
        call_count: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for SignalTextOnlyProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("signal text-only ok".to_string())
        }

        async fn chat_with_history(
            &self,
            _messages: &[ChatMessage],
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok("signal text-only ok".to_string())
        }
    }

    struct IterativeToolProvider {
        required_tool_iterations: usize,
    }

    impl IterativeToolProvider {
        fn completed_tool_iterations(messages: &[ChatMessage]) -> usize {
            messages
                .iter()
                .filter(|msg| msg.role == "user" && msg.content.contains("[Tool results]"))
                .count()
        }
    }

    #[async_trait::async_trait]
    impl Provider for IterativeToolProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(tool_call_payload())
        }

        async fn chat_with_history(
            &self,
            messages: &[ChatMessage],
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            let completed_iterations = Self::completed_tool_iterations(messages);
            if completed_iterations >= self.required_tool_iterations {
                Ok(format!("Completed after {completed_iterations} tool iterations."))
            } else {
                Ok(tool_call_payload())
            }
        }
    }

    #[derive(Default)]
    struct HistoryCaptureProvider {
        calls: parking_lot::Mutex<Vec<Vec<(String, String)>>>,
    }

    #[async_trait::async_trait]
    impl Provider for HistoryCaptureProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("fallback".to_string())
        }

        async fn chat_with_history(
            &self,
            messages: &[ChatMessage],
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            let snapshot = messages
                .iter()
                .map(|m| (m.role.clone(), m.content.clone()))
                .collect::<Vec<_>>();
            let mut calls = self.calls.lock();
            calls.push(snapshot);
            Ok(format!("response-{}", calls.len()))
        }
    }

    struct DelayedHistoryCaptureProvider {
        delay: Duration,
        calls: parking_lot::Mutex<Vec<Vec<(String, String)>>>,
    }

    #[async_trait::async_trait]
    impl Provider for DelayedHistoryCaptureProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("fallback".to_string())
        }

        async fn chat_with_history(
            &self,
            messages: &[ChatMessage],
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            let snapshot = messages
                .iter()
                .map(|m| (m.role.clone(), m.content.clone()))
                .collect::<Vec<_>>();
            let call_index = {
                let mut calls = self.calls.lock();
                calls.push(snapshot);
                calls.len()
            };
            tokio::time::sleep(self.delay).await;
            Ok(format!("response-{call_index}"))
        }
    }

    struct MockPriceTool;

    #[derive(Default)]
    struct ModelCaptureProvider {
        call_count: AtomicUsize,
        models: parking_lot::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Provider for ModelCaptureProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("fallback".to_string())
        }

        async fn chat_with_history(
            &self,
            _messages: &[ChatMessage],
            model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.models.lock().push(model.to_string());
            Ok("ok".to_string())
        }
    }

    #[async_trait::async_trait]
    impl Tool for MockPriceTool {
        fn name(&self) -> &str {
            "mock_price"
        }

        fn description(&self) -> &str {
            "Return a mocked BTC price"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "symbol": { "type": "string" }
                },
                "required": ["symbol"]
            })
        }

        async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
            let symbol = args.get("symbol").and_then(serde_json::Value::as_str);
            if symbol != Some("BTC") {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("unexpected symbol".to_string()),
                });
            }

            Ok(ToolResult {
                success: true,
                output: r#"{"symbol":"BTC","price_usd":65000}"#.to_string(),
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn process_channel_message_executes_tool_calls_instead_of_sending_raw_json() {
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: Arc::new(ToolCallingProvider),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![Box::new(MockPriceTool)]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 10,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        process_channel_message(
            runtime_ctx,
            traits::ChannelMessage {
                id: "msg-1".to_string(),
                sender: "alice".to_string(),
                reply_target: "chat-42".to_string(),
                content: "What is the BTC price now?".to_string(),
                channel: "test-channel".to_string(),
                timestamp: 1,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            },
            CancellationToken::new(),
        )
        .await;

        let sent_messages = channel_impl.sent_messages.lock().await;
        assert_eq!(sent_messages.len(), 1);
        assert!(sent_messages[0].starts_with("chat-42:"));
        assert!(sent_messages[0].contains("BTC is currently around"));
        assert!(!sent_messages[0].contains("\"tool_calls\""));
        assert!(!sent_messages[0].contains("mock_price"));
    }

    #[tokio::test]
    async fn process_channel_message_strips_unexecuted_tool_json_artifacts_from_reply() {
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: Arc::new(RawToolArtifactProvider),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![Box::new(MockPriceTool)]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 10,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        process_channel_message(
            runtime_ctx,
            traits::ChannelMessage {
                id: "msg-raw-json".to_string(),
                sender: "alice".to_string(),
                reply_target: "chat-raw".to_string(),
                content: "What is the BTC price now?".to_string(),
                channel: "test-channel".to_string(),
                timestamp: 3,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            },
            CancellationToken::new(),
        )
        .await;

        let sent_messages = channel_impl.sent_messages.lock().await;
        assert_eq!(sent_messages.len(), 1);
        assert!(sent_messages[0].starts_with("chat-raw:"));
        assert!(sent_messages[0].contains("BTC is currently around"));
        assert!(!sent_messages[0].contains("\"name\":\"mock_price\""));
        assert!(!sent_messages[0].contains("\"result\""));
    }

    #[tokio::test]
    async fn process_channel_message_executes_tool_calls_with_alias_tags() {
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: Arc::new(ToolCallingAliasProvider),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![Box::new(MockPriceTool)]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 10,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        process_channel_message(
            runtime_ctx,
            traits::ChannelMessage {
                id: "msg-2".to_string(),
                sender: "bob".to_string(),
                reply_target: "chat-84".to_string(),
                content: "What is the BTC price now?".to_string(),
                channel: "test-channel".to_string(),
                timestamp: 2,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            },
            CancellationToken::new(),
        )
        .await;

        let sent_messages = channel_impl.sent_messages.lock().await;
        assert_eq!(sent_messages.len(), 1);
        assert!(sent_messages[0].starts_with("chat-84:"));
        assert!(sent_messages[0].contains("alias-tag flow resolved"));
        assert!(!sent_messages[0].contains("<toolcall>"));
        assert!(!sent_messages[0].contains("mock_price"));
    }

    #[tokio::test]
    async fn process_channel_message_signal_image_without_vision_result_uses_uncertainty_fallback() {
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert("signal".to_string(), channel);

        let provider_impl = Arc::new(SignalVisionNoResultProvider::default());

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: provider_impl.clone(),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 5,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        process_channel_message(
            runtime_ctx,
            traits::ChannelMessage {
                id: "signal-image-no-vision".to_string(),
                sender: "+10000000000".to_string(),
                reply_target: "+10000000000".to_string(),
                content: "[IMAGE:data:image/png;base64,iVBORw0KGgo=]".to_string(),
                channel: "signal".to_string(),
                timestamp: 1,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            },
            CancellationToken::new(),
        )
        .await;

        let sent_messages = channel_impl.sent_messages.lock().await;
        assert_eq!(sent_messages.len(), 1);
        assert!(sent_messages[0].contains("无法确认，请提供更清晰图片或补充说明"));
        assert!(!sent_messages[0].contains("维生素C"));
        assert_eq!(provider_impl.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn process_channel_message_signal_text_only_path_is_unchanged() {
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert("signal".to_string(), channel);

        let provider_impl = Arc::new(SignalTextOnlyProvider::default());

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: provider_impl.clone(),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 5,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        process_channel_message(
            runtime_ctx,
            traits::ChannelMessage {
                id: "signal-text-only".to_string(),
                sender: "+10000000000".to_string(),
                reply_target: "+10000000000".to_string(),
                content: "hello from signal text-only".to_string(),
                channel: "signal".to_string(),
                timestamp: 2,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            },
            CancellationToken::new(),
        )
        .await;

        let sent_messages = channel_impl.sent_messages.lock().await;
        assert_eq!(sent_messages.len(), 1);
        assert!(sent_messages[0].contains("signal text-only ok"));
        assert_eq!(provider_impl.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn process_channel_message_handles_models_command_without_llm_call() {
        let channel_impl = Arc::new(TelegramRecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let default_provider_impl = Arc::new(ModelCaptureProvider::default());
        let default_provider: Arc<dyn Provider> = default_provider_impl.clone();
        let fallback_provider_impl = Arc::new(ModelCaptureProvider::default());
        let fallback_provider: Arc<dyn Provider> = fallback_provider_impl.clone();

        let mut provider_cache_seed: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        provider_cache_seed.insert("test-provider".to_string(), Arc::clone(&default_provider));
        provider_cache_seed.insert("openrouter".to_string(), fallback_provider);

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: Arc::clone(&default_provider),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("default-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 5,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(provider_cache_seed)),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        process_channel_message(
            runtime_ctx.clone(),
            traits::ChannelMessage {
                id: "msg-cmd-1".to_string(),
                sender: "alice".to_string(),
                reply_target: "chat-1".to_string(),
                content: "/models openrouter".to_string(),
                channel: "telegram".to_string(),
                timestamp: 1,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            },
            CancellationToken::new(),
        )
        .await;

        let sent = channel_impl.sent_messages.lock().await;
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("Provider switched to `openrouter`"));

        // The route override is written under the canonical key (FIX-P1-25b);
        // the legacy key is left untouched (read-merge writes canonical only), so
        // it must not appear here since this session never had a legacy override.
        let routes = runtime_ctx.route_overrides.lock();
        assert!(
            routes.get("telegram_alice").is_none(),
            "writes go to the canonical key only; no legacy route key is created"
        );
        let route = routes
            .get("channel:telegram:alice:chat-1")
            .cloned()
            .expect("route should be stored for sender under canonical key");
        drop(routes);
        assert_eq!(route.provider, "openrouter");
        assert_eq!(route.model, "default-model");

        assert_eq!(default_provider_impl.call_count.load(Ordering::SeqCst), 0);
        assert_eq!(fallback_provider_impl.call_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn process_channel_message_uses_route_override_provider_and_model() {
        let channel_impl = Arc::new(TelegramRecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let default_provider_impl = Arc::new(ModelCaptureProvider::default());
        let default_provider: Arc<dyn Provider> = default_provider_impl.clone();
        let routed_provider_impl = Arc::new(ModelCaptureProvider::default());
        let routed_provider: Arc<dyn Provider> = routed_provider_impl.clone();

        let mut provider_cache_seed: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        provider_cache_seed.insert("test-provider".to_string(), Arc::clone(&default_provider));
        provider_cache_seed.insert("openrouter".to_string(), routed_provider);

        let route_key = "telegram_alice".to_string();
        let mut route_overrides = HashMap::new();
        route_overrides.insert(
            route_key,
            ChannelRouteSelection {
                provider: "openrouter".to_string(),
                model: "route-model".to_string(),
            },
        );

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: Arc::clone(&default_provider),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("default-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 5,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(provider_cache_seed)),
            route_overrides: Arc::new(Mutex::new(route_overrides)),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        process_channel_message(
            runtime_ctx,
            traits::ChannelMessage {
                id: "msg-routed-1".to_string(),
                sender: "alice".to_string(),
                reply_target: "chat-1".to_string(),
                content: "hello routed provider".to_string(),
                channel: "telegram".to_string(),
                timestamp: 2,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            },
            CancellationToken::new(),
        )
        .await;

        assert_eq!(default_provider_impl.call_count.load(Ordering::SeqCst), 0);
        assert_eq!(routed_provider_impl.call_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            routed_provider_impl.models.lock().as_slice(),
            &["route-model".to_string()]
        );
    }

    #[tokio::test]
    async fn process_channel_message_prefers_cached_default_provider_instance() {
        let channel_impl = Arc::new(TelegramRecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let startup_provider_impl = Arc::new(ModelCaptureProvider::default());
        let startup_provider: Arc<dyn Provider> = startup_provider_impl.clone();
        let reloaded_provider_impl = Arc::new(ModelCaptureProvider::default());
        let reloaded_provider: Arc<dyn Provider> = reloaded_provider_impl.clone();

        let mut provider_cache_seed: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        provider_cache_seed.insert("test-provider".to_string(), reloaded_provider);

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: Arc::clone(&startup_provider),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("default-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 5,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(provider_cache_seed)),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        process_channel_message(
            runtime_ctx,
            traits::ChannelMessage {
                id: "msg-default-provider-cache".to_string(),
                sender: "alice".to_string(),
                reply_target: "chat-1".to_string(),
                content: "hello cached default provider".to_string(),
                channel: "telegram".to_string(),
                timestamp: 3,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            },
            CancellationToken::new(),
        )
        .await;

        assert_eq!(startup_provider_impl.call_count.load(Ordering::SeqCst), 0);
        assert_eq!(reloaded_provider_impl.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn process_channel_message_uses_runtime_default_model_from_store() {
        let channel_impl = Arc::new(TelegramRecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let provider_impl = Arc::new(ModelCaptureProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let mut provider_cache_seed: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        provider_cache_seed.insert("test-provider".to_string(), Arc::clone(&provider));

        let temp = tempfile::TempDir::new().expect("temp dir");
        let mut live_config = Config::default();
        live_config.default_provider = Some("test-provider".to_string());
        live_config.default_model = Some("hot-reloaded-model".to_string());
        live_config.default_temperature = 0.5;
        let config_manager = crate::config::new_shared(live_config);
        let config_generation = config_manager.pin();

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: config_manager,
            config_generation,
            channels_by_name: Arc::new(channels_by_name),
            provider: Arc::clone(&provider),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("startup-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 5,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(provider_cache_seed)),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions {
                openprx_dir: Some(temp.path().to_path_buf()),
                ..providers::ProviderRuntimeOptions::default()
            },
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        process_channel_message(
            runtime_ctx,
            traits::ChannelMessage {
                id: "msg-runtime-store-model".to_string(),
                sender: "alice".to_string(),
                reply_target: "chat-1".to_string(),
                content: "hello runtime defaults".to_string(),
                channel: "telegram".to_string(),
                timestamp: 4,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            },
            CancellationToken::new(),
        )
        .await;

        assert_eq!(provider_impl.call_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            provider_impl.models.lock().as_slice(),
            &["hot-reloaded-model".to_string()]
        );
    }

    #[tokio::test]
    async fn process_channel_message_respects_configured_max_tool_iterations_above_default() {
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: Arc::new(IterativeToolProvider {
                required_tool_iterations: 11,
            }),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![Box::new(MockPriceTool)]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 12,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        process_channel_message(
            runtime_ctx,
            traits::ChannelMessage {
                id: "msg-iter-success".to_string(),
                sender: "alice".to_string(),
                reply_target: "chat-iter-success".to_string(),
                content: "Loop until done".to_string(),
                channel: "test-channel".to_string(),
                timestamp: 1,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            },
            CancellationToken::new(),
        )
        .await;

        let sent_messages = channel_impl.sent_messages.lock().await;
        assert_eq!(sent_messages.len(), 1);
        assert!(sent_messages[0].starts_with("chat-iter-success:"));
        assert!(sent_messages[0].contains("Completed after 11 tool iterations."));
        assert!(!sent_messages[0].contains("⚠️ Error:"));
    }

    #[tokio::test]
    async fn process_channel_message_reports_configured_max_tool_iterations_limit() {
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: Arc::new(IterativeToolProvider {
                required_tool_iterations: 20,
            }),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![Box::new(MockPriceTool)]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 3,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        process_channel_message(
            runtime_ctx,
            traits::ChannelMessage {
                id: "msg-iter-fail".to_string(),
                sender: "bob".to_string(),
                reply_target: "chat-iter-fail".to_string(),
                content: "Loop forever".to_string(),
                channel: "test-channel".to_string(),
                timestamp: 2,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            },
            CancellationToken::new(),
        )
        .await;

        let sent_messages = channel_impl.sent_messages.lock().await;
        assert_eq!(sent_messages.len(), 1);
        assert!(sent_messages[0].starts_with("chat-iter-fail:"));
        assert!(sent_messages[0].contains("⚠️ Something went wrong. Please try again later."));
    }

    struct NoopMemory;

    #[async_trait::async_trait]
    impl Memory for NoopMemory {
        fn name(&self) -> &str {
            "noop"
        }

        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: crate::memory::MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&crate::memory::MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    /// Memory mock that counts `append_conversation_turn` invocations so tests
    /// can assert whether the inbound SideEffectGate let the persistence run
    /// (FIX-P0-10/11/12). All other ops are no-ops.
    #[derive(Default)]
    struct CountingMemory {
        append_calls: AtomicUsize,
        /// Counts the per-message autosave write path
        /// (`store_with_context_and_metadata`, the method the channel autosave
        /// branch actually calls), so the autosave-deny / autosave-allow tests
        /// can assert whether the autosave side effect ran.
        store_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Memory for CountingMemory {
        fn name(&self) -> &str {
            "counting"
        }

        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: crate::memory::MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        // The channel autosave branch writes via store_with_context_and_metadata;
        // override it (rather than the bare store) so store_calls reflects the
        // autosave side effect the gate is meant to permit or suppress.
        async fn store_with_context_and_metadata(
            &self,
            _key: &str,
            _content: &str,
            _category: crate::memory::MemoryCategory,
            _session_id: Option<&str>,
            _context: Option<&crate::memory::MemoryWriteContext>,
            _metadata: crate::memory::MemoryStoreMetadata,
        ) -> anyhow::Result<()> {
            self.store_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&crate::memory::MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn health_check(&self) -> bool {
            true
        }

        #[allow(clippy::too_many_arguments)]
        async fn append_conversation_turn(
            &self,
            _session_key: &str,
            _channel: &str,
            _sender: &str,
            _role: &str,
            _content: &str,
            _timestamp: Option<&str>,
            _message_id: Option<&str>,
            _owner_id: Option<&str>,
        ) -> anyhow::Result<()> {
            self.append_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Tunable knobs for the inbound-gate test contexts. Defaults reproduce the
    /// original `gate_test_ctx` behavior (autosave off, generous action budget,
    /// production gate authorizer).
    struct GateTestOpts {
        autonomy: crate::security::AutonomyLevel,
        auto_save_memory: bool,
        max_actions_per_hour: u32,
        test_inbound_authorizer: Option<Arc<dyn crate::security::inbound_gate::InboundAuthorizer + Send + Sync>>,
    }

    impl GateTestOpts {
        fn new(autonomy: crate::security::AutonomyLevel) -> Self {
            Self {
                autonomy,
                auto_save_memory: false,
                max_actions_per_hour: 20,
                test_inbound_authorizer: None,
            }
        }
    }

    /// Build a minimal `ChannelRuntimeContext` from explicit `GateTestOpts`,
    /// wired to the provided counting memory and channel, for the inbound-gate
    /// tests below.
    fn gate_test_ctx_with(
        opts: GateTestOpts,
        memory: Arc<CountingMemory>,
        channel: Arc<dyn Channel>,
    ) -> Arc<ChannelRuntimeContext> {
        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);
        Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: Arc::new(DummyProvider),
            default_provider: Arc::new("test-provider".to_string()),
            memory,
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: opts.auto_save_memory,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 5,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy {
                    autonomy: opts.autonomy,
                    max_actions_per_hour: opts.max_actions_per_hour,
                    ..crate::security::SecurityPolicy::default()
                }),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: opts.test_inbound_authorizer,
        })
    }

    /// Build a minimal `ChannelRuntimeContext` wired to the given autonomy
    /// level, the provided counting memory, and a `RecordingChannel`, for the
    /// inbound-gate tests below.
    fn gate_test_ctx(
        autonomy: crate::security::AutonomyLevel,
        memory: Arc<CountingMemory>,
        channel: Arc<dyn Channel>,
    ) -> Arc<ChannelRuntimeContext> {
        gate_test_ctx_with(GateTestOpts::new(autonomy), memory, channel)
    }

    fn gate_test_message() -> traits::ChannelMessage {
        traits::ChannelMessage {
            id: "gate-msg-1".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-gate".to_string(),
            content: "hello gate".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        }
    }

    /// FIX-P0-10/11/12: under autonomy=ReadOnly the inbound SideEffectGate must
    /// abort `process_channel_message` before the first conversation-turn
    /// persistence — so `append_conversation_turn` is never called and no reply
    /// is sent. This is the "mock-gate-rejects-all" check.
    #[tokio::test]
    async fn process_channel_message_inbound_gate_blocks_under_readonly() {
        let memory = Arc::new(CountingMemory::default());
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();
        let ctx = gate_test_ctx(crate::security::AutonomyLevel::ReadOnly, Arc::clone(&memory), channel);

        process_channel_message(ctx, gate_test_message(), CancellationToken::new()).await;

        assert_eq!(
            memory.append_calls.load(Ordering::SeqCst),
            0,
            "ReadOnly autonomy must block the inbound conversation-turn persistence"
        );
        assert!(
            channel_impl.sent_messages.lock().await.is_empty(),
            "ReadOnly autonomy must not send any reply"
        );
    }

    /// FIX-P0-10/11/12: under explicit Supervised autonomy the inbound gate
    /// must let normal traffic through — `append_conversation_turn` runs for the
    /// inbound user turn (and at least one reply is appended), proving the gate
    /// does not falsely reject low-risk inbound messages.
    #[tokio::test]
    async fn process_channel_message_inbound_gate_allows_under_supervised() {
        let memory = Arc::new(CountingMemory::default());
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();
        let ctx = gate_test_ctx(crate::security::AutonomyLevel::Supervised, Arc::clone(&memory), channel);

        process_channel_message(ctx, gate_test_message(), CancellationToken::new()).await;

        assert!(
            memory.append_calls.load(Ordering::SeqCst) >= 1,
            "Supervised autonomy must allow the inbound conversation-turn persistence (normal flow not broken)"
        );
    }

    /// A DM message whose content is >= 30 chars and free of noise patterns, so
    /// `should_autosave_content` returns true and the autosave branch is reached
    /// (otherwise the gate result would be moot and the autosave assertions would
    /// pass vacuously). `reply_target` has no group marker, so it infers as a DM.
    fn gate_test_dm_message() -> traits::ChannelMessage {
        traits::ChannelMessage {
            id: "gate-msg-autosave".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-gate".to_string(),
            content: "this is a sufficiently long direct message worth autosaving".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        }
    }

    fn gate_test_telegram_group_message() -> traits::ChannelMessage {
        traits::ChannelMessage {
            id: "gate-msg-telegram-group".to_string(),
            sender: "alice".to_string(),
            reply_target: "-100200300".to_string(),
            content: "[Telegram Group] Alice: this is a sufficiently long group message that must not autosave"
                .to_string(),
            channel: "telegram".to_string(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: ChatKind::Group,
            chat_title: Some("Build Room".to_string()),
            sender_display: Some("Alice".to_string()),
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: true,
            sender_is_bot: false,
        }
    }

    /// Test seam: deny only the `:autosave` operation, allowing inbound + outbound.
    /// The real `SecurityPolicy` cannot express this (Act gating is autonomy + rate
    /// only), so the autosave-deny end-to-end control flow is driven via this mock.
    struct AutosaveOnlyDeny;

    impl crate::security::inbound_gate::InboundAuthorizer for AutosaveOnlyDeny {
        fn authorize(&self, _tool_name: &str, operation_name: &str, _risk: ResourceRiskLevel) -> Result<(), String> {
            if operation_name.contains(":autosave") {
                Err(format!("test deny: {operation_name}"))
            } else {
                Ok(())
            }
        }
    }

    /// D6-3 (outbound deny, budget path): with `max_actions_per_hour=2` and
    /// Supervised autonomy, the inbound gate (action #1) and autosave gate
    /// (action #2) pass, but the outbound gate (action #3) is rate-denied. The
    /// inbound user turn must still be persisted (it ran before the budget was
    /// exhausted) while no reply is sent. Uses a dedicated policy + tracker so
    /// the budget starts fresh (no flaky pre-consumption).
    #[tokio::test]
    async fn process_channel_message_outbound_gate_denied_by_budget() {
        let memory = Arc::new(CountingMemory::default());
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();
        let mut opts = GateTestOpts::new(crate::security::AutonomyLevel::Supervised);
        opts.auto_save_memory = true;
        opts.max_actions_per_hour = 2;
        let ctx = gate_test_ctx_with(opts, Arc::clone(&memory), channel);

        process_channel_message(ctx, gate_test_dm_message(), CancellationToken::new()).await;

        assert!(
            memory.append_calls.load(Ordering::SeqCst) >= 1,
            "inbound turn must persist (gated before the budget was exhausted)"
        );
        assert!(
            channel_impl.sent_messages.lock().await.is_empty(),
            "outbound must be suppressed once the action budget is exhausted"
        );
    }

    /// Test seam: deny only the `:outbound` operation, allowing inbound + autosave.
    /// Used to drive the Signal vision-fallback outbound-deny path independently of
    /// the action budget, so the fallback gate's deny semantics are asserted in
    /// isolation (the real policy cannot express a per-operation deny).
    struct OutboundOnlyDeny;

    impl crate::security::inbound_gate::InboundAuthorizer for OutboundOnlyDeny {
        fn authorize(&self, _tool_name: &str, operation_name: &str, _risk: ResourceRiskLevel) -> Result<(), String> {
            if operation_name.contains(":outbound") {
                Err(format!("test deny: {operation_name}"))
            } else {
                Ok(())
            }
        }
    }

    /// A Signal image message that forces the vision preflight into the
    /// `Fallback` branch: `channel == "signal"` plus a `vision_required=true`
    /// marker makes `is_signal_image_message` true, and the test `DummyProvider`
    /// reports `supports_vision() == false`, so the preflight short-circuits to
    /// `Fallback` before any network call.
    fn signal_vision_fallback_message() -> traits::ChannelMessage {
        traits::ChannelMessage {
            id: "gate-msg-vision".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-gate".to_string(),
            content: "[signal-meta vision_required=true]".to_string(),
            channel: "signal".to_string(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        }
    }

    /// Build a gate test context wired to a `SignalRecordingChannel` (registered
    /// under the `signal` name) instead of the default `RecordingChannel`, so the
    /// Signal vision-fallback path can resolve its `target_channel`.
    fn signal_gate_ctx_with(
        opts: GateTestOpts,
        memory: Arc<CountingMemory>,
        channel: Arc<SignalRecordingChannel>,
    ) -> Arc<ChannelRuntimeContext> {
        let channel_dyn: Arc<dyn Channel> = channel;
        gate_test_ctx_with(opts, memory, channel_dyn)
    }

    /// DEV-05 (vision-fallback outbound deny): the Signal vision fallback emits an
    /// assistant reply, so it must pass the same outbound gate as the normal path.
    /// With outbound denied (op-selective seam), the fallback must NOT send a reply
    /// — yet the inbound user turn (gated + persisted before the fallback branch)
    /// must remain. This closes the matrix gap Codex flagged: previously the
    /// fallback short-circuited a reply ahead of the outbound gate.
    #[tokio::test]
    async fn process_channel_message_vision_fallback_outbound_denied() {
        let memory = Arc::new(CountingMemory::default());
        let channel_impl = Arc::new(SignalRecordingChannel::default());
        let mut opts = GateTestOpts::new(crate::security::AutonomyLevel::Supervised);
        opts.test_inbound_authorizer = Some(Arc::new(OutboundOnlyDeny));
        let ctx = signal_gate_ctx_with(opts, Arc::clone(&memory), Arc::clone(&channel_impl));

        process_channel_message(ctx, signal_vision_fallback_message(), CancellationToken::new()).await;

        assert!(
            memory.append_calls.load(Ordering::SeqCst) >= 1,
            "outbound deny must not abort the turn: the inbound user turn still persists"
        );
        assert!(
            channel_impl.sent_messages.lock().await.is_empty(),
            "outbound deny must suppress the Signal vision fallback reply"
        );
    }

    /// DEV-05 (positive control): with outbound allowed (Supervised, no op-selective
    /// deny) the Signal vision fallback DOES send its reply. This guards the deny
    /// test above from passing vacuously — proving the fallback send path is
    /// actually reachable for this message and only the gate suppresses it.
    #[tokio::test]
    async fn process_channel_message_vision_fallback_outbound_allowed_sends() {
        let memory = Arc::new(CountingMemory::default());
        let channel_impl = Arc::new(SignalRecordingChannel::default());
        let opts = GateTestOpts::new(crate::security::AutonomyLevel::Supervised);
        let ctx = signal_gate_ctx_with(opts, Arc::clone(&memory), Arc::clone(&channel_impl));

        process_channel_message(ctx, signal_vision_fallback_message(), CancellationToken::new()).await;

        let sent = channel_impl.sent_messages.lock().await;
        assert_eq!(
            sent.len(),
            1,
            "Signal vision fallback must send exactly one reply when outbound is allowed"
        );
        assert!(
            sent[0].contains(SIGNAL_IMAGE_UNCERTAINTY_FALLBACK),
            "the sent reply must be the vision uncertainty fallback text"
        );
    }

    /// D6-3 (autosave deny, mock seam): inject an authorizer that denies only
    /// `:autosave`. The inbound user turn must still persist and the reply must
    /// still be sent, but the autosave memory write must be skipped (store_calls
    /// == 0). This isolates the autosave skip-only deny semantics from inbound /
    /// outbound, which the real policy cannot do per-operation.
    #[tokio::test]
    async fn process_channel_message_autosave_gate_denied_by_mock() {
        let memory = Arc::new(CountingMemory::default());
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();
        let mut opts = GateTestOpts::new(crate::security::AutonomyLevel::Supervised);
        opts.auto_save_memory = true;
        opts.test_inbound_authorizer = Some(Arc::new(AutosaveOnlyDeny));
        let ctx = gate_test_ctx_with(opts, Arc::clone(&memory), channel);

        process_channel_message(ctx, gate_test_dm_message(), CancellationToken::new()).await;

        assert!(
            memory.append_calls.load(Ordering::SeqCst) >= 1,
            "autosave deny must not abort the turn: the inbound user turn still persists"
        );
        assert_eq!(
            memory.store_calls.load(Ordering::SeqCst),
            0,
            "autosave deny must skip the autosave memory write"
        );
        assert!(
            !channel_impl.sent_messages.lock().await.is_empty(),
            "autosave deny must not suppress the reply (skip-only, not abort)"
        );
    }

    /// D6-3 (autosave allow, positive control): with autosave permitted (no
    /// op-selective deny) the autosave memory write runs (store_calls > 0). This
    /// guards the autosave-deny test above from passing vacuously — proving the
    /// autosave branch is actually reachable for this message under allow.
    #[tokio::test]
    async fn process_channel_message_autosave_gate_allowed_writes_memory() {
        let memory = Arc::new(CountingMemory::default());
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();
        let mut opts = GateTestOpts::new(crate::security::AutonomyLevel::Supervised);
        opts.auto_save_memory = true;
        let ctx = gate_test_ctx_with(opts, Arc::clone(&memory), channel);

        process_channel_message(ctx, gate_test_dm_message(), CancellationToken::new()).await;

        assert!(
            memory.store_calls.load(Ordering::SeqCst) >= 1,
            "autosave allow must perform the autosave memory write"
        );
    }

    #[tokio::test]
    async fn process_channel_message_telegram_group_does_not_autosave() {
        let memory = Arc::new(CountingMemory::default());
        let channel_impl = Arc::new(TelegramRecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();
        let mut opts = GateTestOpts::new(crate::security::AutonomyLevel::Supervised);
        opts.auto_save_memory = true;
        let ctx = gate_test_ctx_with(opts, Arc::clone(&memory), channel);

        process_channel_message(ctx, gate_test_telegram_group_message(), CancellationToken::new()).await;

        assert!(
            memory.append_calls.load(Ordering::SeqCst) >= 1,
            "group message inbound turn should still persist"
        );
        assert_eq!(
            memory.store_calls.load(Ordering::SeqCst),
            0,
            "Telegram group messages must not take the DM autosave path"
        );
    }

    /// D8-1 / Step 7.3 (E2E): a full channel turn stamps the same per-turn
    /// run_id on inbound, assistant, and shared terminal events. The finalizer
    /// emits exactly one assistant projection and one terminal marker. No
    /// parent_run_id is set (no spawn lineage in this turn).
    #[tokio::test]
    async fn process_channel_message_stamps_per_turn_run_id_on_message_events() {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();
        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);
        let mut runtime_config = Config::default();
        runtime_config.autonomy.level = crate::security::AutonomyLevel::Supervised;
        runtime_config.default_provider = Some("test-provider".to_string());
        runtime_config.default_model = Some("test-model".to_string());
        let config_manager = crate::config::new_shared(runtime_config);
        let ctx = Arc::new(ChannelRuntimeContext {
            config: Arc::clone(&config_manager),
            config_generation: config_manager.pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: Arc::new(DummyProvider),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::clone(&memory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(HookManager::new(tmp.path().to_path_buf())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 5,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(tmp.path().to_path_buf()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy {
                    autonomy: crate::security::AutonomyLevel::Supervised,
                    ..crate::security::SecurityPolicy::default()
                }),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        process_channel_message(ctx, gate_test_message(), CancellationToken::new()).await;

        let events = memory
            .list_message_events_since(
                &MemoryPrincipal {
                    workspace_id: tmp.path().to_string_lossy().to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: None,
                    channel: Some("test-channel".to_string()),
                    sender: None,
                    owner_id: None,
                    legacy_session_key: None,
                },
                0,
                20,
            )
            .await
            .unwrap();

        let user_event = events
            .iter()
            .find(|e| e.role == "user")
            .expect("user message event recorded");
        let outbound_events = memory
            .list_message_events_since(
                &MemoryPrincipal {
                    workspace_id: tmp.path().to_string_lossy().to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: user_event.session_key.clone(),
                    channel: Some("test-channel".to_string()),
                    sender: Some("prx".to_string()),
                    owner_id: None,
                    legacy_session_key: None,
                },
                0,
                20,
            )
            .await
            .unwrap();
        let assistant_event = outbound_events
            .iter()
            .find(|e| e.role == "assistant")
            .expect("assistant message event recorded");
        let terminal_events = outbound_events
            .iter()
            .filter(|event| event.event_type == "turn.finalized")
            .collect::<Vec<_>>();
        assert_eq!(
            outbound_events.iter().filter(|event| event.role == "assistant").count(),
            1,
            "the shared finalizer must project one assistant event"
        );
        assert_eq!(terminal_events.len(), 1, "the channel turn must close once");

        let user_run_id = user_event.run_id.as_deref().expect("user event must carry a run_id");
        assert!(!user_run_id.is_empty(), "user run_id must be non-empty");
        assert_eq!(
            assistant_event.run_id.as_deref(),
            Some(user_run_id),
            "user and assistant events of one turn must share the same run_id"
        );
        assert_eq!(
            terminal_events.first().and_then(|event| event.run_id.as_deref()),
            Some(user_run_id),
            "the terminal marker must share the channel turn run_id"
        );
        assert!(
            user_event.parent_run_id.is_none() && assistant_event.parent_run_id.is_none(),
            "a turn with no spawn lineage must not set parent_run_id"
        );
        for event in std::iter::once(user_event)
            .chain(std::iter::once(assistant_event))
            .chain(terminal_events.iter().copied())
        {
            assert_eq!(
                event.config_generation_id,
                Some(0),
                "every event in one channel turn must retain its pinned config generation"
            );
            assert!(
                event
                    .config_source_revision
                    .as_deref()
                    .is_some_and(|revision| !revision.is_empty()),
                "every event in one channel turn must retain its config source revision"
            );
        }
    }

    // ── D2-2: channels security hot-reload ──────────────────────────────────

    /// Exercise the exact gate calls the four channel authorization points make
    /// (inbound/autosave/outbound/scope all funnel through
    /// `SideEffectGate::new(snapshot.as_ref()).authorize_resource_operation`).
    fn channel_gate_allows(policy: &crate::security::SecurityPolicy) -> bool {
        SideEffectGate::new(policy)
            .authorize_resource_operation("channel", "channel:test:inbound", ResourceRiskLevel::Low, None)
            .is_ok()
            && SideEffectGate::new(policy)
                .authorize_resource_operation("channel", "channel:test:outbound", ResourceRiskLevel::Low, None)
                .is_ok()
    }

    /// Build a coherent `SecurityGen` from an explicit autonomy level,
    /// mirroring exactly what the stamp-change branch constructs.
    fn make_gen(autonomy: crate::security::AutonomyLevel) -> Arc<SecurityGen> {
        Arc::new(SecurityGen {
            security: Arc::new(crate::security::SecurityPolicy {
                autonomy,
                ..crate::security::SecurityPolicy::default()
            }),
        })
    }

    /// T2-a: the channel's own gate observes a security hot-swap. Start
    /// Supervised (gate allows), then store a ReadOnly generation into the same
    /// `ArcSwap` and confirm the snapshot read at the authorization point now
    /// denies — without rebuilding the context.
    #[test]
    fn t2a_channels_gate_hot_updates_on_security_store() {
        let security: Arc<arc_swap::ArcSwap<SecurityGen>> = Arc::new(arc_swap::ArcSwap::new(make_gen(
            crate::security::AutonomyLevel::Supervised,
        )));

        // Before reload: Supervised → low-risk inbound/outbound allowed.
        let before = security.load_full();
        assert!(
            channel_gate_allows(before.security.as_ref()),
            "Supervised snapshot must allow low-risk channel gates"
        );

        // Hot-swap to ReadOnly (what the stamp-change branch does via store()).
        security.store(make_gen(crate::security::AutonomyLevel::ReadOnly));

        // After reload: the snapshot read at the gate now denies.
        let after = security.load_full();
        assert!(
            !channel_gate_allows(after.security.as_ref()),
            "after storing a ReadOnly generation the channel gate snapshot must deny (hot update took effect)"
        );
    }

    /// T2-b: rebuild happens only on a `store` (i.e. only in the stamp-change
    /// branch). When no store occurs, repeated `load_full()` returns the *same*
    /// `Arc` (pointer-identical), proving per-message reads never rebuild; after
    /// a `store` the pointer changes and carries the new generation.
    #[test]
    fn t2b_load_full_is_stable_until_store() {
        let security: Arc<arc_swap::ArcSwap<SecurityGen>> = Arc::new(arc_swap::ArcSwap::new(make_gen(
            crate::security::AutonomyLevel::Supervised,
        )));

        let a = security.load_full();
        let b = security.load_full();
        assert!(
            Arc::ptr_eq(&a, &b),
            "without a store, successive load_full() snapshots must be the same Arc (no per-read rebuild)"
        );

        // The stamp-change branch path: store a freshly built generation.
        security.store(make_gen(crate::security::AutonomyLevel::ReadOnly));

        let c = security.load_full();
        assert!(
            !Arc::ptr_eq(&a, &c),
            "after store(), load_full() must observe the new Arc (the rebuilt generation)"
        );
        assert_eq!(
            c.security.autonomy,
            crate::security::AutonomyLevel::ReadOnly,
            "the stored snapshot must carry the rebuilt ReadOnly policy"
        );
    }

    /// T2-scope-deny: storing a generation built from a config with
    /// `[autonomy.scopes] default = "deny"` flips scope authorization
    /// (`is_tool_allowed`, what `scope_or_pipeline_denial` consults) to deny.
    #[test]
    fn t2_scope_authorization_denies_after_store() {
        // Build a real SecurityPolicy from a deny-by-default scope config so the
        // scope path used by the tool-call loop reflects the new generation.
        let mut autonomy = crate::config::AutonomyConfig::default();
        autonomy.scopes.default = "deny".to_string();
        let denying = crate::security::SecurityPolicy::from_config(&autonomy, std::path::Path::new("/tmp"));

        let security: Arc<arc_swap::ArcSwap<SecurityGen>> = Arc::new(arc_swap::ArcSwap::new(make_gen(
            crate::security::AutonomyLevel::Supervised,
        )));
        // Before: the explicit Supervised generation allows the tool in scope.
        assert!(
            security
                .load_full()
                .security
                .is_tool_allowed("memory_recall", "alice", "telegram", "direct"),
            "default scope must allow the tool before the deny generation is stored"
        );

        security.store(Arc::new(SecurityGen {
            security: Arc::new(denying),
        }));

        assert!(
            !security
                .load_full()
                .security
                .is_tool_allowed("memory_recall", "alice", "telegram", "direct"),
            "after storing a deny-by-default scope generation, scope authorization must deny"
        );
    }

    /// T2-decide-deny: storing a generation whose `SecurityPolicy` has a
    /// deny-by-default scope ACL flips the unified `SecurityPolicy::decide`
    /// decision point to `Deny` — and the policy rides in the SAME `SecurityGen`,
    /// so the decision can never lag a generation.
    #[test]
    fn t2_decide_denies_after_store() {
        use crate::security::policy::ToolDecision;

        // Build a deny-by-default SecurityPolicy via the real config path.
        let mut autonomy = crate::config::AutonomyConfig::default();
        autonomy.level = crate::security::AutonomyLevel::Supervised;
        autonomy.scopes.default = "deny".to_string();
        let denying = crate::security::SecurityPolicy::from_config(&autonomy, std::path::Path::new("/tmp"));

        let security: Arc<arc_swap::ArcSwap<SecurityGen>> = Arc::new(arc_swap::ArcSwap::new(make_gen(
            crate::security::AutonomyLevel::Supervised,
        )));

        // Before: the explicit Supervised generation's unified decision allows
        // (scope allows; Supervised would Ask for a side-effecting tool, but a
        // read-only tool is Allow) — assert the read-only tool is allowed.
        assert_eq!(
            security
                .load_full()
                .security
                .decide("memory_recall", "alice", "telegram", "direct"),
            ToolDecision::Allow,
            "default generation must allow the read-only tool before the deny generation is stored"
        );

        // Store a generation whose policy denies via the scope ACL default.
        security.store(Arc::new(SecurityGen {
            security: Arc::new(denying),
        }));

        let live = security.load_full();
        assert_eq!(
            live.security.decide("memory_recall", "alice", "telegram", "direct"),
            ToolDecision::Deny,
            "after storing a deny-by-default scope generation, the unified decision must be Deny"
        );
        // The autonomy half came from the SAME stored generation (coherent gen):
        // The configured policy keeps Supervised, proving policy and scope ACL
        // travel together in one SecurityGen.
        assert_eq!(
            live.security.autonomy,
            crate::security::AutonomyLevel::Supervised,
            "policy fields travel together in one SecurityGen"
        );
    }

    struct RecallMemory;

    #[async_trait::async_trait]
    impl Memory for RecallMemory {
        fn name(&self) -> &str {
            "recall-memory"
        }

        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: crate::memory::MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            Ok(vec![crate::memory::MemoryEntry {
                id: "entry-1".to_string(),
                key: "memory_key_1".to_string(),
                content: "Age is 45".to_string(),
                category: crate::memory::MemoryCategory::Conversation,
                timestamp: "2026-02-20T00:00:00Z".to_string(),
                session_id: None,
                score: Some(0.9),
                tags: None,
                access_count: None,
                useful_count: None,
                source: None,
                source_confidence: None,
                verification_status: None,
                lifecycle_state: None,
                compressed_from: None,
            }])
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&crate::memory::MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(1)
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn message_dispatch_processes_messages_in_parallel() {
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: Arc::new(SlowProvider {
                delay: Duration::from_millis(250),
            }),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 10,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(4);
        tx.send(traits::ChannelMessage {
            id: "1".to_string(),
            sender: "alice".to_string(),
            reply_target: "alice".to_string(),
            content: "hello".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        })
        .await
        .unwrap();
        tx.send(traits::ChannelMessage {
            id: "2".to_string(),
            sender: "bob".to_string(),
            reply_target: "bob".to_string(),
            content: "world".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 2,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        })
        .await
        .unwrap();
        drop(tx);

        let started = Instant::now();
        run_message_dispatch_loop(rx, runtime_ctx, 2, CancellationToken::new()).await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(900),
            "expected parallel dispatch (<900ms), got {:?}",
            elapsed
        );

        let sent_messages = channel_impl.sent_messages.lock().await;
        assert_eq!(sent_messages.len(), 2);
    }

    #[tokio::test]
    async fn message_dispatch_interrupts_in_flight_telegram_request_and_preserves_context() {
        let channel_impl = Arc::new(TelegramRecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let provider_impl = Arc::new(DelayedHistoryCaptureProvider {
            delay: Duration::from_millis(250),
            calls: parking_lot::Mutex::new(Vec::new()),
        });

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: provider_impl.clone(),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 10,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: true,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(8);
        let send_task = tokio::spawn(async move {
            tx.send(traits::ChannelMessage {
                id: "msg-1".to_string(),
                sender: "alice".to_string(),
                reply_target: "chat-1".to_string(),
                content: "forwarded content".to_string(),
                channel: "telegram".to_string(),
                timestamp: 1,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            })
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(40)).await;
            tx.send(traits::ChannelMessage {
                id: "msg-2".to_string(),
                sender: "alice".to_string(),
                reply_target: "chat-1".to_string(),
                content: "summarize this".to_string(),
                channel: "telegram".to_string(),
                timestamp: 2,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            })
            .await
            .unwrap();
        });

        run_message_dispatch_loop(rx, runtime_ctx, 4, CancellationToken::new()).await;
        send_task.await.unwrap();

        let sent_messages = channel_impl.sent_messages.lock().await;
        assert_eq!(sent_messages.len(), 1);
        assert!(sent_messages[0].starts_with("chat-1:"));
        assert!(sent_messages[0].contains("response-2"));
        drop(sent_messages);

        let calls = provider_impl.calls.lock();
        assert_eq!(calls.len(), 2);
        let second_call = &calls[1];
        assert!(
            second_call
                .iter()
                .any(|(role, content)| { role == "user" && content.contains("forwarded content") })
        );
        assert!(
            second_call
                .iter()
                .any(|(role, content)| { role == "user" && content.contains("summarize this") })
        );
        assert!(
            !second_call.iter().any(|(role, _)| role == "assistant"),
            "cancelled turn should not persist an assistant response"
        );
    }

    #[tokio::test]
    async fn message_dispatch_interrupt_scope_is_same_sender_same_chat() {
        let channel_impl = Arc::new(TelegramRecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: Arc::new(SlowProvider {
                delay: Duration::from_millis(180),
            }),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 10,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: true,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(8);
        let send_task = tokio::spawn(async move {
            tx.send(traits::ChannelMessage {
                id: "msg-a".to_string(),
                sender: "alice".to_string(),
                reply_target: "chat-1".to_string(),
                content: "first chat".to_string(),
                channel: "telegram".to_string(),
                timestamp: 1,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            })
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
            tx.send(traits::ChannelMessage {
                id: "msg-b".to_string(),
                sender: "alice".to_string(),
                reply_target: "chat-2".to_string(),
                content: "second chat".to_string(),
                channel: "telegram".to_string(),
                timestamp: 2,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            })
            .await
            .unwrap();
        });

        run_message_dispatch_loop(rx, runtime_ctx, 4, CancellationToken::new()).await;
        send_task.await.unwrap();

        let sent_messages = channel_impl.sent_messages.lock().await;
        assert_eq!(sent_messages.len(), 2);
        assert!(sent_messages.iter().any(|msg| msg.starts_with("chat-1:")));
        assert!(sent_messages.iter().any(|msg| msg.starts_with("chat-2:")));
    }

    #[tokio::test]
    async fn process_channel_message_cancels_scoped_typing_task() {
        let tmp = tempfile::TempDir::new().unwrap();
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: Arc::new(SlowProvider {
                delay: Duration::from_millis(20),
            }),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::clone(&memory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(tmp.path().to_path_buf())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 10,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(tmp.path().to_path_buf()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        process_channel_message(
            runtime_ctx,
            traits::ChannelMessage {
                id: "typing-msg".to_string(),
                sender: "alice".to_string(),
                reply_target: "chat-typing".to_string(),
                content: "hello".to_string(),
                channel: "test-channel".to_string(),
                timestamp: 1,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            },
            CancellationToken::new(),
        )
        .await;

        let starts = channel_impl.start_typing_calls.load(Ordering::SeqCst);
        let stops = channel_impl.stop_typing_calls.load(Ordering::SeqCst);
        assert_eq!(starts, 1, "start_typing should be called once");
        assert_eq!(stops, 1, "stop_typing should be called once");

        let events = memory
            .list_message_events_since(
                &MemoryPrincipal {
                    workspace_id: tmp.path().to_string_lossy().to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: Some("test-channel_alice".to_string()),
                    channel: Some("test-channel".to_string()),
                    sender: Some("alice".to_string()),
                    owner_id: None,
                    legacy_session_key: None,
                },
                0,
                10,
            )
            .await
            .unwrap();
        let user_event = events.iter().find(|event| event.role == "user").expect("user event");
        let assistant_event = events
            .iter()
            .find(|event| event.role == "assistant")
            .expect("assistant event");
        assert_eq!(user_event.source, "channel");
        assert_eq!(user_event.role, "user");
        assert_eq!(user_event.content, "hello");
        assert_eq!(
            user_event.idempotency_key.as_deref(),
            Some("channel:test-channel:typing-msg")
        );
        assert_eq!(assistant_event.source, "channel");
        assert_eq!(assistant_event.role, "assistant");
        assert!(assistant_event.content.contains("echo:"));
    }

    #[test]
    fn prompt_contains_all_sections() {
        let ws = make_workspace();
        let tools = vec![("shell", "Run commands"), ("file_read", "Read files")];
        let prompt = build_system_prompt(ws.path(), "test-model", &tools, &[], None, None);

        // Section headers
        assert!(prompt.contains("## Tools"), "missing Tools section");
        assert!(prompt.contains("## Safety"), "missing Safety section");
        assert!(prompt.contains("## Workspace"), "missing Workspace section");
        assert!(prompt.contains("## Project Context"), "missing Project Context");
        assert!(prompt.contains("## Current Date & Time"), "missing Date/Time");
        assert!(prompt.contains("## Runtime"), "missing Runtime section");
    }

    #[test]
    fn prompt_injects_tools() {
        let ws = make_workspace();
        let tools = vec![("shell", "Run commands"), ("memory_recall", "Search memory")];
        let prompt = build_system_prompt(ws.path(), "gpt-4o", &tools, &[], None, None);

        assert!(prompt.contains("**shell**"));
        assert!(prompt.contains("Run commands"));
        assert!(prompt.contains("**memory_recall**"));
    }

    #[test]
    fn prompt_includes_single_tool_protocol_block_after_append() {
        let ws = make_workspace();
        let tools = vec![("shell", "Run commands")];
        let mut prompt = build_system_prompt(ws.path(), "gpt-4o", &tools, &[], None, None);

        assert!(
            !prompt.contains("## Tool Use Protocol"),
            "build_system_prompt should not emit protocol block directly"
        );

        prompt.push_str(&build_tool_instructions(&[], false));

        assert_eq!(
            prompt.matches("## Tool Use Protocol").count(),
            1,
            "protocol block should appear exactly once in the final prompt"
        );
    }

    #[test]
    fn prompt_injects_safety() {
        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

        assert!(prompt.contains("Do not exfiltrate private data"));
        assert!(prompt.contains("Do not run destructive commands"));
        assert!(prompt.contains("Prefer `trash` over `rm`"));
    }

    #[test]
    #[ignore = "known failure — workspace file injection logic needs update"]
    fn prompt_injects_workspace_files() {
        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

        assert!(prompt.contains("### SOUL.md"), "missing SOUL.md header");
        assert!(prompt.contains("Be helpful"), "missing SOUL content");
        assert!(prompt.contains("### IDENTITY.md"), "missing IDENTITY.md");
        assert!(prompt.contains("Name: OpenPRX"), "missing IDENTITY content");
        assert!(prompt.contains("### USER.md"), "missing USER.md");
        assert!(prompt.contains("### AGENTS.md"), "missing AGENTS.md");
        assert!(prompt.contains("### TOOLS.md"), "missing TOOLS.md");
        // HEARTBEAT.md is intentionally excluded from channel prompts — it's only
        // relevant to the heartbeat worker and causes LLMs to emit spurious
        // "HEARTBEAT_OK" acknowledgments in channel conversations.
        assert!(
            !prompt.contains("### HEARTBEAT.md"),
            "HEARTBEAT.md should not be in channel prompt"
        );
        assert!(prompt.contains("### MEMORY.md"), "missing MEMORY.md");
        assert!(prompt.contains("User likes Rust"), "missing MEMORY content");
    }

    #[test]
    fn prompt_missing_identity_files_are_skipped() {
        let tmp = TempDir::new().unwrap();
        // Empty workspace — no files at all
        let prompt = build_system_prompt(tmp.path(), "model", &[], &[], None, None);

        assert!(!prompt.contains("### SOUL.md"));
        assert!(!prompt.contains("### AGENTS.md"));
        assert!(!prompt.contains("### IDENTITY.md"));
    }

    #[test]
    fn build_identity_prompt_loads_existing_files_only() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("SOUL.md"), "Soul").unwrap();
        std::fs::write(tmp.path().join("TOOLS.md"), "Tools").unwrap();

        let prompt = build_identity_prompt(tmp.path());
        assert!(prompt.contains("### SOUL.md"));
        assert!(prompt.contains("Soul"));
        assert!(prompt.contains("### TOOLS.md"));
        assert!(!prompt.contains("### AGENTS.md"));
        assert!(!prompt.contains("File not found"));
    }

    #[test]
    fn prompt_bootstrap_only_if_exists() {
        let ws = make_workspace();
        // No BOOTSTRAP.md — should not appear
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);
        assert!(
            !prompt.contains("### BOOTSTRAP.md"),
            "BOOTSTRAP.md should not appear when missing"
        );

        // Create BOOTSTRAP.md — should appear
        std::fs::write(ws.path().join("BOOTSTRAP.md"), "# Bootstrap\nFirst run.").unwrap();
        let prompt2 = build_system_prompt(ws.path(), "model", &[], &[], None, None);
        assert!(
            prompt2.contains("### BOOTSTRAP.md"),
            "BOOTSTRAP.md should appear when present"
        );
        assert!(prompt2.contains("First run"));
    }

    #[test]
    fn prompt_no_daily_memory_injection() {
        let ws = make_workspace();
        let memory_dir = ws.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        std::fs::write(memory_dir.join(format!("{today}.md")), "# Daily\nSome note.").unwrap();

        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

        // Daily notes should NOT be in the system prompt (on-demand via tools)
        assert!(
            !prompt.contains("Daily Notes"),
            "daily notes should not be auto-injected"
        );
        assert!(!prompt.contains("Some note"), "daily content should not be in prompt");
    }

    #[test]
    fn prompt_runtime_metadata() {
        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "claude-sonnet-4", &[], &[], None, None);

        assert!(prompt.contains("Model: claude-sonnet-4"));
        assert!(prompt.contains(&format!("OS: {}", std::env::consts::OS)));
        assert!(prompt.contains("Host:"));
    }

    #[test]
    fn prompt_skills_include_instructions_and_tools() {
        let ws = make_workspace();
        let skills = vec![crate::skills::Skill {
            name: "code-review".into(),
            description: "Review code for bugs".into(),
            version: "1.0.0".into(),
            author: None,
            tags: vec![],
            tools: vec![crate::skills::SkillTool {
                name: "lint".into(),
                description: "Run static checks".into(),
                kind: "shell".into(),
                command: "cargo clippy".into(),
                args: HashMap::new(),
            }],
            prompts: vec!["Always run cargo test before final response.".into()],
            location: None,
            embedding: None,
        }];

        let prompt = build_system_prompt(ws.path(), "model", &[], &skills, None, None);

        assert!(prompt.contains("<available_skills>"), "missing skills XML");
        assert!(prompt.contains("<name>code-review</name>"));
        assert!(prompt.contains("<description>Review code for bugs</description>"));
        assert!(prompt.contains("SKILL.md</location>"));
        assert!(prompt.contains("<instructions>"));
        assert!(prompt.contains("<instruction>Always run cargo test before final response.</instruction>"));
        assert!(prompt.contains("<tools>"));
        assert!(prompt.contains("<name>lint</name>"));
        assert!(prompt.contains("<kind>shell</kind>"));
        assert!(!prompt.contains("loaded on demand"));
    }

    #[test]
    fn prompt_skills_escape_reserved_xml_chars() {
        let ws = make_workspace();
        let skills = vec![crate::skills::Skill {
            name: "code<review>&".into(),
            description: "Review \"unsafe\" and 'risky' bits".into(),
            version: "1.0.0".into(),
            author: None,
            tags: vec![],
            tools: vec![crate::skills::SkillTool {
                name: "run\"linter\"".into(),
                description: "Run <lint> & report".into(),
                kind: "shell&exec".into(),
                command: "cargo clippy".into(),
                args: HashMap::new(),
            }],
            prompts: vec!["Use <tool_call> and & keep output \"safe\"".into()],
            location: None,
            embedding: None,
        }];

        let prompt = build_system_prompt(ws.path(), "model", &[], &skills, None, None);

        assert!(prompt.contains("<name>code&lt;review&gt;&amp;</name>"));
        assert!(prompt.contains("<description>Review &quot;unsafe&quot; and &apos;risky&apos; bits</description>"));
        assert!(prompt.contains("<name>run&quot;linter&quot;</name>"));
        assert!(prompt.contains("<description>Run &lt;lint&gt; &amp; report</description>"));
        assert!(prompt.contains("<kind>shell&amp;exec</kind>"));
        assert!(
            prompt.contains("<instruction>Use &lt;tool_call&gt; and &amp; keep output &quot;safe&quot;</instruction>")
        );
    }

    #[test]
    fn prompt_truncation() {
        let ws = make_workspace();
        // Write a file larger than BOOTSTRAP_MAX_CHARS
        let big_content = "x".repeat(BOOTSTRAP_MAX_CHARS + 1000);
        std::fs::write(ws.path().join("AGENTS.md"), &big_content).unwrap();

        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

        assert!(prompt.contains("truncated at"), "large files should be truncated");
        assert!(!prompt.contains(&big_content), "full content should not appear");
    }

    #[test]
    fn prompt_empty_files_skipped() {
        let ws = make_workspace();
        std::fs::write(ws.path().join("TOOLS.md"), "").unwrap();

        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

        // Empty file should not produce a header
        assert!(!prompt.contains("### TOOLS.md"), "empty files should be skipped");
    }

    #[test]
    fn channel_log_truncation_is_utf8_safe_for_multibyte_text() {
        let msg = "Hello from OpenPRX 🌍. Current status is healthy, and café-style UTF-8 text stays safe in logs.";

        // Reproduces the production crash path where channel logs truncate at 80 chars.
        let result = std::panic::catch_unwind(|| crate::util::truncate_with_ellipsis(msg, 80));
        assert!(result.is_ok(), "truncate_with_ellipsis should never panic on UTF-8");

        let truncated = result.unwrap();
        assert!(!truncated.is_empty());
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn runtime_prompt_excludes_channel_delivery_context() {
        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

        assert!(!prompt.contains("## Channel Capabilities"));
        assert!(!prompt.contains("running as a messaging bot"));
        assert!(!prompt.contains("automatically sent back"));
    }

    #[test]
    fn channel_prompt_includes_channel_delivery_context() {
        let ws = make_workspace();
        let base_prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);
        let msg = traits::ChannelMessage {
            channel: "telegram".to_string(),
            reply_target: "chat-1".to_string(),
            chat_kind: ChatKind::Group,
            chat_title: Some("Ops".to_string()),
            ..Default::default()
        };
        let prompt = build_channel_system_prompt(&base_prompt, &msg, None, Some("@bot"));

        assert!(
            prompt.contains("## Channel Capabilities"),
            "missing Channel Capabilities section"
        );
        assert!(prompt.contains("running as a messaging bot"), "missing channel context");
        assert!(
            prompt.contains("NEVER repeat, describe, or echo credentials"),
            "missing channel safety instruction"
        );
        assert!(
            prompt.contains("When responding on Telegram"),
            "missing Telegram delivery guidance"
        );
        assert!(
            prompt.contains("## Current Conversation")
                && prompt.contains("- Platform: telegram | You: @bot")
                && prompt.contains("- Type: group | Chat: \"Ops\" (chat-1)"),
            "missing current conversation context"
        );
        assert!(
            prompt.contains("use the chat_profile_update tool (not memory_store)"),
            "missing chat_profile_update preference hint"
        );
    }

    fn test_chat_profile(channel: &str, chat_id: &str, chat_kind: &str, purpose: &str) -> ChatProfile {
        ChatProfile {
            id: format!("{channel}-{chat_id}"),
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            chat_kind: chat_kind.to_string(),
            title: Some(format!("title-{chat_id}")),
            purpose: Some(purpose.to_string()),
            notes: Some("shared operating notes".to_string()),
            tags: vec!["ops".to_string(), "handoff".to_string()],
            updated_by: "agent".to_string(),
            created_at: "2026-07-06T00:00:00Z".to_string(),
            updated_at: "2026-07-06T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn current_conversation_prompt_cross_chat_isolation() {
        let group_a = traits::ChannelMessage {
            channel: "telegram".to_string(),
            reply_target: "group-a".to_string(),
            chat_kind: ChatKind::Group,
            chat_title: Some("Group A".to_string()),
            ..Default::default()
        };
        let group_b = traits::ChannelMessage {
            channel: "telegram".to_string(),
            reply_target: "group-b".to_string(),
            chat_kind: ChatKind::Group,
            chat_title: Some("Group B".to_string()),
            ..Default::default()
        };
        let dm = traits::ChannelMessage {
            channel: "telegram".to_string(),
            reply_target: "dm-1".to_string(),
            chat_kind: ChatKind::Dm,
            chat_title: Some("Direct".to_string()),
            ..Default::default()
        };
        let profile_a = test_chat_profile("telegram", "group-a", "group", "A-only release room");

        let prompt_a = build_current_conversation_prompt(&group_a, Some(&profile_a), Some("@bot"));
        let prompt_b = build_current_conversation_prompt(&group_b, None, Some("@bot"));
        let prompt_dm = build_current_conversation_prompt(&dm, None, Some("@bot"));

        assert!(prompt_a.contains("A-only release room"));
        assert!(!prompt_b.contains("A-only release room"));
        assert!(!prompt_dm.contains("A-only release room"));
        assert!(prompt_b.contains("- Type: group | Chat: \"Group B\" (group-b)"));
        assert!(prompt_dm.contains("- Type: dm | Chat: \"Direct\" (dm-1)"));
    }

    #[test]
    fn current_conversation_prompt_snapshots_no_profile_with_profile_and_long_truncation() {
        let msg = traits::ChannelMessage {
            channel: "wacli".to_string(),
            reply_target: "12345@g.us".to_string(),
            chat_kind: ChatKind::Group,
            chat_title: Some("Release War Room".to_string()),
            ..Default::default()
        };
        let no_profile = build_current_conversation_prompt(&msg, None, Some("99550001@s.whatsapp.net"));
        assert!(no_profile.contains("- Platform: wacli | You: 99550001@s.whatsapp.net"));
        assert!(no_profile.contains("- Type: group | Chat: \"Release War Room\" (12345@g.us)"));
        assert!(!no_profile.contains("Purpose (self-maintained):"));

        let mut profile = test_chat_profile("wacli", "12345@g.us", "group", "Coordinate release approvals");
        let with_profile = build_current_conversation_prompt(&msg, Some(&profile), Some("99550001@s.whatsapp.net"));
        assert!(with_profile.contains("- Purpose (self-maintained): Coordinate release approvals"));
        assert!(with_profile.contains("- Notes: shared operating notes | Tags: ops, handoff"));

        profile.purpose = Some("p".repeat(400));
        profile.notes = Some("n".repeat(900));
        let long = build_current_conversation_prompt(&msg, Some(&profile), Some("99550001@s.whatsapp.net"));
        assert!(long.contains(&format!("{}...", "p".repeat(180))));
        assert!(!long.contains(&"p".repeat(300)));
        assert!(long.contains(&format!("{}...", "n".repeat(240))));
        assert!(!long.contains(&"n".repeat(400)));
    }

    #[test]
    fn current_conversation_prompt_renders_thread_self_consistently() {
        let msg = traits::ChannelMessage {
            channel: "slack".to_string(),
            reply_target: "thread-1".to_string(),
            chat_kind: ChatKind::Thread,
            chat_title: Some("Thread".to_string()),
            ..Default::default()
        };
        let prompt = build_current_conversation_prompt(&msg, None, Some("prx"));
        assert!(prompt.contains("- Type: thread | Chat: \"Thread\" (thread-1)"));
    }

    #[test]
    fn prompt_workspace_path() {
        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

        assert!(prompt.contains(&format!("Working directory: `{}`", ws.path().display())));
    }

    #[test]
    fn conversation_memory_key_uses_message_id() {
        let msg = traits::ChannelMessage {
            id: "msg_abc123".into(),
            sender: "U123".into(),
            reply_target: "C456".into(),
            content: "hello".into(),
            channel: "slack".into(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        };

        assert_eq!(conversation_memory_key(&msg), "slack_U123_msg_abc123");
    }

    #[test]
    fn conversation_memory_key_is_unique_per_message() {
        let msg1 = traits::ChannelMessage {
            id: "msg_1".into(),
            sender: "U123".into(),
            reply_target: "C456".into(),
            content: "first".into(),
            channel: "slack".into(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        };
        let msg2 = traits::ChannelMessage {
            id: "msg_2".into(),
            sender: "U123".into(),
            reply_target: "C456".into(),
            content: "second".into(),
            channel: "slack".into(),
            timestamp: 2,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        };

        assert_ne!(conversation_memory_key(&msg1), conversation_memory_key(&msg2));
    }

    #[tokio::test]
    async fn autosave_keys_preserve_multiple_conversation_facts() {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();

        let msg1 = traits::ChannelMessage {
            id: "msg_1".into(),
            sender: "U123".into(),
            reply_target: "C456".into(),
            content: "I'm Paul".into(),
            channel: "slack".into(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        };
        let msg2 = traits::ChannelMessage {
            id: "msg_2".into(),
            sender: "U123".into(),
            reply_target: "C456".into(),
            content: "I'm 45".into(),
            channel: "slack".into(),
            timestamp: 2,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        };

        mem.store(
            &conversation_memory_key(&msg1),
            &msg1.content,
            MemoryCategory::Conversation,
            None,
        )
        .await
        .unwrap();
        mem.store(
            &conversation_memory_key(&msg2),
            &msg2.content,
            MemoryCategory::Conversation,
            None,
        )
        .await
        .unwrap();

        assert_eq!(mem.count().await.unwrap(), 2);

        let recalled = mem.recall("45", 5, None).await.unwrap();
        assert!(recalled.iter().any(|entry| entry.content.contains("45")));
    }

    #[tokio::test]
    async fn build_memory_context_includes_recalled_entries() {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        mem.store("age_fact", "Age is 45", MemoryCategory::Conversation, None)
            .await
            .unwrap();

        let context = build_memory_context(&mem, "age", 0.0).await;
        assert!(context.contains("[Memory context]"));
        assert!(context.contains("Age is 45"));
    }

    #[tokio::test]
    async fn process_channel_message_restores_per_sender_history_on_follow_ups() {
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let provider_impl = Arc::new(HistoryCaptureProvider::default());

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: provider_impl.clone(),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 5,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        process_channel_message(
            runtime_ctx.clone(),
            traits::ChannelMessage {
                id: "msg-a".to_string(),
                sender: "alice".to_string(),
                reply_target: "chat-1".to_string(),
                content: "hello".to_string(),
                channel: "test-channel".to_string(),
                timestamp: 1,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            },
            CancellationToken::new(),
        )
        .await;

        process_channel_message(
            runtime_ctx,
            traits::ChannelMessage {
                id: "msg-b".to_string(),
                sender: "alice".to_string(),
                reply_target: "chat-1".to_string(),
                content: "follow up".to_string(),
                channel: "test-channel".to_string(),
                timestamp: 2,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            },
            CancellationToken::new(),
        )
        .await;

        let calls = provider_impl.calls.lock();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].len(), 2);
        assert_eq!(calls[0][0].0, "system");
        assert_eq!(calls[0][1].0, "user");
        assert_eq!(calls[1].len(), 4);
        assert_eq!(calls[1][0].0, "system");
        assert_eq!(calls[1][1].0, "user");
        assert_eq!(calls[1][2].0, "assistant");
        assert_eq!(calls[1][3].0, "user");
        assert!(calls[1][1].1.contains("hello"));
        assert!(calls[1][2].1.contains("response-1"));
        assert!(calls[1][3].1.contains("follow up"));
    }

    #[tokio::test]
    async fn process_channel_message_enriches_current_turn_without_persisting_context() {
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let provider_impl = Arc::new(HistoryCaptureProvider::default());
        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: provider_impl.clone(),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(RecallMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 5,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(HashMap::new())),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        process_channel_message(
            runtime_ctx.clone(),
            traits::ChannelMessage {
                id: "msg-ctx-1".to_string(),
                sender: "alice".to_string(),
                reply_target: "chat-ctx".to_string(),
                content: "hello".to_string(),
                channel: "test-channel".to_string(),
                timestamp: 1,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            },
            CancellationToken::new(),
        )
        .await;

        let calls = provider_impl.calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 2);
        assert_eq!(calls[0][1].0, "user");
        assert!(calls[0][1].1.contains("[Memory context]"));
        assert!(calls[0][1].1.contains("Age is 45"));
        assert!(calls[0][1].1.contains("hello"));

        let histories = runtime_ctx.conversation_histories.lock();
        let turns = histories
            .get("channel:test-channel:alice:chat-ctx")
            .expect("history should be stored for sender");
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].content, "hello");
        assert!(!turns[0].content.contains("[Memory context]"));
    }

    #[tokio::test]
    async fn process_channel_message_telegram_keeps_system_instruction_at_top_only() {
        let channel_impl = Arc::new(TelegramRecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let provider_impl = Arc::new(HistoryCaptureProvider::default());
        let mut histories = HashMap::new();
        histories.insert(
            "telegram_alice".to_string(),
            vec![
                ChatMessage::assistant("stale assistant"),
                ChatMessage::user("earlier user question"),
                ChatMessage::assistant("earlier assistant reply"),
            ],
        );

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            config: crate::config::new_shared(Config::default()),
            config_generation: crate::config::new_shared(Config::default()).pin(),
            channels_by_name: Arc::new(channels_by_name),
            provider: provider_impl.clone(),
            default_provider: Arc::new("test-provider".to_string()),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            model: Arc::new("test-model".to_string()),
            temperature: 0.0,
            auto_save_memory: false,
            memory_event_recording: MemoryEventRecording::default(),
            max_tool_iterations: 5,
            read_only_tool_concurrency_window: 2,
            read_only_tool_timeout_secs: 30,
            priority_scheduling_enabled: false,
            low_priority_tools: Vec::new(),
            min_relevance_score: 0.0,
            conversation_histories: Arc::new(Mutex::new(histories)),
            provider_cache: Arc::new(Mutex::new(HashMap::new())),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            api_key: None,
            api_url: None,
            reliability: Arc::new(crate::config::ReliabilityConfig::default()),
            provider_runtime_options: providers::ProviderRuntimeOptions::default(),
            workspace_dir: Arc::new(std::env::temp_dir()),
            message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
            agent_compaction: crate::config::AgentCompactionConfig::default(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            signal_inbound_policy: None,
            whatsapp_inbound_policy: None,
            bot_names: vec!["prx".to_string()],
            bot_uuids: vec![],
            mention_only_by_channel: HashMap::new(),
            group_reply_mode_by_channel: HashMap::new(),
            smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            smart_group: crate::config::SmartGroupConfig::default(),
            interrupt_on_new_message: false,
            multimodal: crate::config::MultimodalConfig::default(),
            security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
                security: Arc::new(crate::security::SecurityPolicy::default()),
            })),
            native_tools: false,
            skill_rag_ctx: None,
            test_inbound_authorizer: None,
        });

        process_channel_message(
            runtime_ctx.clone(),
            traits::ChannelMessage {
                id: "tg-msg-1".to_string(),
                sender: "alice".to_string(),
                reply_target: "chat-telegram".to_string(),
                content: "hello".to_string(),
                channel: "telegram".to_string(),
                timestamp: 1,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            },
            CancellationToken::new(),
        )
        .await;

        let calls = provider_impl.calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 4);

        let roles = calls[0].iter().map(|(role, _)| role.as_str()).collect::<Vec<_>>();
        assert_eq!(roles, vec!["system", "user", "assistant", "user"]);
        assert!(
            calls[0][0]
                .1
                .contains("When responding on Telegram, include media markers"),
            "telegram delivery instruction should live in the system prompt"
        );
        assert!(!calls[0].iter().skip(1).any(|(role, _)| role == "system"));
    }

    #[test]
    fn extract_tool_context_summary_collects_alias_and_native_tool_calls() {
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::assistant(
                r#"<toolcall>
{"name":"shell","arguments":{"command":"date"}}
</toolcall>"#,
            ),
            ChatMessage::assistant(
                r#"{"content":null,"tool_calls":[{"id":"1","name":"web_search","arguments":"{}"}]}"#,
            ),
        ];

        let summary = extract_tool_context_summary(&history, 1);
        assert_eq!(summary, "[Used tools: shell, web_search]");
    }

    #[test]
    fn extract_tool_context_summary_collects_prompt_mode_tool_result_names() {
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::assistant("Using markdown tool call fence"),
            ChatMessage::user(
                r#"[Tool results]
<tool_result name="http_request">
{"status":200}
</tool_result>
<tool_result name="shell">
Mon Feb 20
</tool_result>"#,
            ),
        ];

        let summary = extract_tool_context_summary(&history, 1);
        assert_eq!(summary, "[Used tools: http_request, shell]");
    }

    #[test]
    fn extract_tool_context_summary_respects_start_index() {
        let history = vec![
            ChatMessage::assistant(
                r#"<tool_call>
{"name":"stale_tool","arguments":{}}
</tool_call>"#,
            ),
            ChatMessage::assistant(
                r#"<tool_call>
{"name":"fresh_tool","arguments":{}}
</tool_call>"#,
            ),
        ];

        let summary = extract_tool_context_summary(&history, 1);
        assert_eq!(summary, "[Used tools: fresh_tool]");
    }

    #[test]
    fn strip_isolated_tool_json_artifacts_removes_tool_calls_and_results() {
        let mut known_tools = HashSet::new();
        known_tools.insert("cron".to_string());

        let input = r#"{"name":"cron","parameters":{"action":"once","message":"test"}}
{"name":"cron","parameters":{"action":"cancel","task_id":"test"}}
Let me create the reminder properly:
{"name":"cron","parameters":{"action":"once","message":"Go to sleep"}}
{"result":{"task_id":"abc","status":"scheduled"}}
Done reminder set for 1:38 AM."#;

        let result = strip_isolated_tool_json_artifacts(input, &known_tools);
        let normalized = result
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            normalized,
            "Let me create the reminder properly:\nDone reminder set for 1:38 AM."
        );
    }

    #[test]
    fn strip_isolated_tool_json_artifacts_preserves_non_tool_json() {
        let mut known_tools = HashSet::new();
        known_tools.insert("shell".to_string());

        let input = r#"{"name":"profile","parameters":{"timezone":"UTC"}}
This is an example JSON object for profile settings."#;

        let result = strip_isolated_tool_json_artifacts(input, &known_tools);
        assert_eq!(result, input);
    }

    #[test]
    fn strip_isolated_tool_tag_artifacts_removes_tool_call_blocks() {
        let mut known_tools = HashSet::new();
        known_tools.insert("shell".to_string());

        let input = r#"Before
<tool_call mode="json">
{"name":"shell","arguments":{"command":"date"}}
</tool_call>
After"#;

        let result = strip_isolated_tool_tag_artifacts(input, &known_tools);
        assert_eq!(result, "Before\n\nAfter");
    }

    #[test]
    fn sanitize_channel_response_removes_isolated_tool_tag_artifacts() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockPriceTool)];
        let input = r#"<tool_call name="mock_price">
{"name":"mock_price","arguments":{"symbol":"BTC"}}
</tool_call>"#;

        let result = sanitize_channel_response(input, &tools);
        assert!(result.is_empty());
    }

    // ── AIEOS Identity Tests (Issue #168) ─────────────────────────

    #[test]
    fn aieos_identity_from_file() {
        use crate::config::IdentityConfig;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let identity_path = tmp.path().join("aieos_identity.json");

        // Write AIEOS identity file
        let aieos_json = r#"{
            "identity": {
                "names": {"first": "Nova", "nickname": "Nov"},
                "bio": "A helpful AI assistant.",
                "origin": "Silicon Valley"
            },
            "psychology": {
                "mbti": "INTJ",
                "moral_compass": ["Be helpful", "Do no harm"]
            },
            "linguistics": {
                "style": "concise",
                "formality": "casual"
            }
        }"#;
        std::fs::write(&identity_path, aieos_json).unwrap();

        // Create identity config pointing to the file
        let config = IdentityConfig {
            format: "aieos".into(),
            aieos_path: Some("aieos_identity.json".into()),
            aieos_inline: None,
        };

        let prompt = build_system_prompt(tmp.path(), "model", &[], &[], Some(&config), None);

        // Should contain AIEOS sections
        assert!(prompt.contains("## Identity"));
        assert!(prompt.contains("**Name:** Nova"));
        assert!(prompt.contains("**Nickname:** Nov"));
        assert!(prompt.contains("**Bio:** A helpful AI assistant."));
        assert!(prompt.contains("**Origin:** Silicon Valley"));

        assert!(prompt.contains("## Personality"));
        assert!(prompt.contains("**MBTI:** INTJ"));
        assert!(prompt.contains("**Moral Compass:**"));
        assert!(prompt.contains("- Be helpful"));

        assert!(prompt.contains("## Communication Style"));
        assert!(prompt.contains("**Style:** concise"));
        assert!(prompt.contains("**Formality Level:** casual"));

        // Should NOT contain OpenClaw bootstrap file headers
        assert!(!prompt.contains("### SOUL.md"));
        assert!(!prompt.contains("### IDENTITY.md"));
        assert!(!prompt.contains("[File not found"));
    }

    #[test]
    fn aieos_identity_from_inline() {
        use crate::config::IdentityConfig;

        let config = IdentityConfig {
            format: "aieos".into(),
            aieos_path: None,
            aieos_inline: Some(r#"{"identity":{"names":{"first":"Claw"}}}"#.into()),
        };

        let prompt = build_system_prompt(std::env::temp_dir().as_path(), "model", &[], &[], Some(&config), None);

        assert!(prompt.contains("**Name:** Claw"));
        assert!(prompt.contains("## Identity"));
    }

    #[test]
    fn aieos_fallback_to_openclaw_on_parse_error() {
        use crate::config::IdentityConfig;

        let config = IdentityConfig {
            format: "aieos".into(),
            aieos_path: Some("nonexistent.json".into()),
            aieos_inline: None,
        };

        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], Some(&config), None);

        // Should fall back to OpenClaw format when AIEOS file is not found
        // (Error is logged to stderr with filename, not included in prompt)
        assert!(prompt.contains("### SOUL.md"));
    }

    #[test]
    fn aieos_empty_uses_openclaw() {
        use crate::config::IdentityConfig;

        // Format is "aieos" but neither path nor inline is set
        let config = IdentityConfig {
            format: "aieos".into(),
            aieos_path: None,
            aieos_inline: None,
        };

        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], Some(&config), None);

        // Should use OpenClaw format (not configured for AIEOS)
        assert!(prompt.contains("### SOUL.md"));
        assert!(prompt.contains("Be helpful"));
    }

    #[test]
    fn openclaw_format_uses_bootstrap_files() {
        use crate::config::IdentityConfig;

        let config = IdentityConfig {
            format: "openclaw".into(),
            aieos_path: Some("identity.json".into()),
            aieos_inline: None,
        };

        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], Some(&config), None);

        // Should use OpenClaw format even if aieos_path is set
        assert!(prompt.contains("### SOUL.md"));
        assert!(prompt.contains("Be helpful"));
        assert!(!prompt.contains("## Identity"));
    }

    #[test]
    fn none_identity_config_uses_openclaw() {
        let ws = make_workspace();
        // Pass None for identity config
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

        // Should use OpenClaw format
        assert!(prompt.contains("### SOUL.md"));
        assert!(prompt.contains("Be helpful"));
    }

    #[test]
    fn classify_health_ok_true() {
        let state = classify_health_result(&Ok(true));
        assert_eq!(state, ChannelHealthState::Healthy);
    }

    #[test]
    fn classify_health_ok_false() {
        let state = classify_health_result(&Ok(false));
        assert_eq!(state, ChannelHealthState::Unhealthy);
    }

    #[tokio::test]
    async fn classify_health_timeout() {
        let result = tokio::time::timeout(Duration::from_millis(1), async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            true
        })
        .await;
        let state = classify_health_result(&result);
        assert_eq!(state, ChannelHealthState::Timeout);
    }

    struct AlwaysFailChannel {
        name: &'static str,
        calls: Arc<AtomicUsize>,
    }

    struct BlockUntilClosedChannel {
        name: String,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Channel for AlwaysFailChannel {
        fn name(&self) -> &str {
            self.name
        }

        async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
            Ok(())
        }

        async fn listen(&self, _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>) -> anyhow::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("listen boom")
        }
    }

    #[async_trait::async_trait]
    impl Channel for BlockUntilClosedChannel {
        fn name(&self) -> &str {
            &self.name
        }

        async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
            Ok(())
        }

        async fn listen(&self, tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>) -> anyhow::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tx.closed().await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn supervised_listener_marks_error_and_restarts_on_failures() {
        let calls = Arc::new(AtomicUsize::new(0));
        let channel: Arc<dyn Channel> = Arc::new(AlwaysFailChannel {
            name: "test-supervised-fail",
            calls: Arc::clone(&calls),
        });

        let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
        let handle = spawn_supervised_listener(channel, tx, 1, 1, CancellationToken::new());

        tokio::time::sleep(Duration::from_millis(80)).await;
        drop(rx);
        handle.abort();
        let _ = handle.await;

        let snapshot = crate::health::snapshot_json();
        let component = &snapshot["components"]["channel:test-supervised-fail"];
        assert_eq!(component["status"], "error");
        assert!(component["restart_count"].as_u64().unwrap_or(0) >= 1);
        assert!(component["last_error"].as_str().unwrap_or("").contains("listen boom"));
        assert!(calls.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn channel_supervisor_opens_circuit_after_consecutive_failures() {
        assert_eq!(channel_supervisor_sleep_duration(0, 2, 60), Duration::from_secs(2));
        assert_eq!(
            channel_supervisor_sleep_duration(CHANNEL_CIRCUIT_BREAKER_FAILURES - 1, 16, 60),
            Duration::from_secs(16)
        );
        assert_eq!(
            channel_supervisor_sleep_duration(CHANNEL_CIRCUIT_BREAKER_FAILURES, 16, 60),
            Duration::from_secs(300)
        );
        assert_eq!(
            channel_supervisor_sleep_duration(CHANNEL_CIRCUIT_BREAKER_FAILURES + 10, 60, 60),
            Duration::from_secs(300)
        );
    }

    #[tokio::test]
    async fn supervised_listener_refreshes_health_while_running() {
        let calls = Arc::new(AtomicUsize::new(0));
        let channel_name = format!("test-supervised-heartbeat-{}", uuid::Uuid::new_v4());
        let component_name = format!("channel:{channel_name}");
        let channel: Arc<dyn Channel> = Arc::new(BlockUntilClosedChannel {
            name: channel_name,
            calls: Arc::clone(&calls),
        });

        let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
        let handle = spawn_supervised_listener_with_health_interval(
            channel,
            tx,
            1,
            1,
            Duration::from_millis(20),
            CancellationToken::new(),
        );

        tokio::time::sleep(Duration::from_millis(35)).await;
        let first_last_ok = crate::health::snapshot_json()["components"][&component_name]["last_ok"]
            .as_str()
            .unwrap_or("")
            .to_string();
        assert!(!first_last_ok.is_empty());

        tokio::time::sleep(Duration::from_millis(70)).await;
        let second_last_ok = crate::health::snapshot_json()["components"][&component_name]["last_ok"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let first = chrono::DateTime::parse_from_rfc3339(&first_last_ok).expect("last_ok should be valid RFC3339");
        let second = chrono::DateTime::parse_from_rfc3339(&second_last_ok).expect("last_ok should be valid RFC3339");
        assert!(second > first, "expected periodic health heartbeat refresh");

        drop(rx);
        let join = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(join.is_ok(), "listener should stop after channel shutdown");
        assert!(calls.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn supervised_listener_stops_on_shutdown_without_restart() {
        // D5/D9 step 5 regression: a listener blocked inside `listen()` must
        // observe the external shutdown token and break out of its supervisor
        // loop (ListenerOutcome::Shutdown) without restarting, so the owning
        // `handles.await` can complete instead of hanging forever.
        let calls = Arc::new(AtomicUsize::new(0));
        let channel_name = format!("test-supervised-shutdown-{}", uuid::Uuid::new_v4());
        let channel: Arc<dyn Channel> = Arc::new(BlockUntilClosedChannel {
            name: channel_name,
            calls: Arc::clone(&calls),
        });

        // Keep our own `tx` clone alive so the channel never closes on its own:
        // only the shutdown token should be able to stop the listener.
        let (tx, _rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
        let shutdown = CancellationToken::new();
        let handle = spawn_supervised_listener(channel, tx, 1, 1, shutdown.clone());

        // Let the listener enter its blocking `listen()` (tx.closed()).
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(calls.load(Ordering::SeqCst) >= 1, "listener should have started");

        // Request shutdown: the supervisor must break and the task must finish.
        shutdown.cancel();
        let join = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(join.is_ok(), "listener should stop promptly on shutdown");

        // It stopped via the shutdown branch, not a restart: only one listen call.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "shutdown must not trigger a listener restart"
        );
    }

    #[test]
    fn maybe_restart_daemon_systemd_args_regression() {
        assert_eq!(SYSTEMD_STATUS_ARGS, ["--user", "is-active", "prx.service"]);
        assert_eq!(SYSTEMD_RESTART_ARGS, ["--user", "restart", "prx.service"]);
    }

    #[test]
    fn maybe_restart_daemon_openrc_args_regression() {
        assert_eq!(OPENRC_STATUS_ARGS, ["prx", "status"]);
        assert_eq!(OPENRC_RESTART_ARGS, ["prx", "restart"]);
    }
}

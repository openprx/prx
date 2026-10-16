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

pub mod activity;
pub mod cli;
mod decision_runtime;
pub(crate) mod decision_source;
mod decision_turn;
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
pub mod outbound_registry;
pub mod pre_gate;
pub mod qq;
#[cfg(test)]
mod recording_fixture;
pub mod signal;
pub mod signal_native;
pub mod slack;
pub mod telegram;
pub mod terminal;
pub mod traits;
pub mod wacli;
pub(crate) mod webhook_server;
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

use crate::agent::document_ingest::DocumentIngestRuntime;
/// Canonical maximum characters per injected workspace file.
pub use crate::agent::prompt::{BOOTSTRAP_MAX_CHARS, build_identity_prompt};
use crate::agent::response_guard::strip_fabricated_tool_markers_from_assistant_turns;
use crate::agent::turn_contracts::ScopeContext;
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Per-sender conversation history for channel messages.
type ConversationHistoryMap = Arc<Mutex<HashMap<String, Vec<ChatMessage>>>>;
/// Maximum history messages to keep per sender.
/// 🟡 Behavior-limits Phase 1: raised 50 -> 200 (4x).
const MAX_CHANNEL_HISTORY: usize = 200;
/// Maximum number of persisted sessions to hydrate at startup.
const MAX_HYDRATED_SESSIONS: usize = 100;
const DEFAULT_CHANNEL_INITIAL_BACKOFF_SECS: u64 = 2;
const DEFAULT_CHANNEL_MAX_BACKOFF_SECS: u64 = 60;
/// Depth of the single inbound message bus shared by every channel listener.
///
/// Bounded on purpose: it is the process's backpressure surface. Listeners
/// publish with `send(..).await`, so a producer that outruns the process is
/// slowed down rather than having its messages dropped, and the platforms
/// themselves stop advancing (Telegram's offset, an IMAP IDLE cycle) instead of
/// PRX buffering without limit. Since `run_message_dispatch_loop` no longer
/// blocks on anything a turn holds, a queue that stays full now means the
/// machine cannot spawn turns fast enough — a real signal — rather than one
/// wedged conversation holding the door shut for everyone.
const CHANNEL_INBOUND_QUEUE_CAPACITY: usize = 100;
const CHANNEL_CIRCUIT_BREAKER_FAILURES: u32 = 5;
const CHANNEL_CIRCUIT_BREAKER_BACKOFF_MULTIPLIER: u32 = 5;
const CHANNEL_TYPING_REFRESH_INTERVAL_SECS: u64 = 4;
const CHANNEL_HEALTH_HEARTBEAT_SECS: u64 = 30;
const MODEL_CACHE_FILE: &str = "models_cache.json";
const MODEL_CACHE_PREVIEW_LIMIT: usize = 10;
const SIGNAL_IMAGE_UNCERTAINTY_FALLBACK: &str =
    "I could not identify this image with confidence. Please send a clearer picture or add some context.";

type ProviderCacheKey = providers::generation::GenerationProviderCacheKey;
type ProviderCacheMap = providers::generation::GenerationProviderCache;
type RouteSelectionMap = Arc<Mutex<HashMap<String, ChannelRouteSelection>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChannelRouteSelection {
    provider: String,
    model: String,
}

impl ChannelRouteSelection {
    /// The provider as it may be shown in a chat: the default route is the
    /// configured label, and a `custom:` URL may carry `user:password@`.
    fn display_provider(&self) -> std::borrow::Cow<'_, str> {
        providers::redact_endpoint_userinfo(&self.provider)
    }
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
}

#[derive(Clone)]
struct ChannelMessageRuntimeSnapshot {
    multimodal: crate::config::MultimodalConfig,
    read_only_tool_concurrency_window: usize,
    priority_scheduling_enabled: bool,
    low_priority_tools: Vec<String>,
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
    decisions: Arc<decision_runtime::ChannelDecisionSlot>,
    config: crate::config::SharedConfig,
    #[cfg(test)]
    config_generation: Arc<crate::config::ConfigGeneration>,
    channels_by_name: Arc<HashMap<String, Arc<dyn Channel>>>,
    #[cfg(test)]
    provider: Arc<dyn Provider>,
    #[cfg(test)]
    default_provider: Arc<String>,
    memory: Arc<dyn Memory>,
    tools_registry: Arc<Vec<Box<dyn Tool>>>,
    observer: Arc<dyn Observer>,
    hooks: Arc<HookManager>,
    #[cfg(test)]
    model: Arc<String>,
    #[cfg(test)]
    temperature: f64,
    auto_save_memory: bool,
    memory_event_recording: MemoryEventRecording,
    #[cfg(test)]
    read_only_tool_concurrency_window: usize,
    #[cfg(test)]
    priority_scheduling_enabled: bool,
    #[cfg(test)]
    low_priority_tools: Vec<String>,
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
    interrupt_on_new_message: bool,
    #[cfg(test)]
    multimodal: crate::config::MultimodalConfig,
    /// Legacy test-only security fixture. Production derives one policy from the
    /// ConfigGeneration pinned at message admission.
    #[cfg(test)]
    security: Arc<arc_swap::ArcSwap<SecurityGen>>,
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
    /// Test-only seam: when present, `process_channel_message` routes the inbound
    /// gate through this authorizer instead of the production `for_policy` path,
    /// so tests can express an operation-selective deny (e.g. autosave only) that
    /// the real `SecurityPolicy` cannot. Compiled out entirely in non-test builds
    /// (zero production overhead; `for_policy` is the only path there).
    #[cfg(test)]
    test_inbound_authorizer: Option<Arc<dyn crate::security::inbound_gate::InboundAuthorizer + Send + Sync>>,
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

struct InFlightTaskCompletionGuard(Arc<InFlightTaskCompletion>);

impl Drop for InFlightTaskCompletionGuard {
    fn drop(&mut self) {
        self.0.mark_done();
    }
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
        // There is exactly one successor for each replaced map entry. `notify_one`
        // retains a permit if completion lands between the done check and await.
        self.notify.notify_one();
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

/// Reshape cached turns into the strict user/assistant alternation the
/// providers expect, and drop fabricated `[Used tools: ...]` lines from the
/// assistant turns on the way out.
///
/// The strip happens on this replay copy only: the persisted
/// `conversation_turns` rows keep exactly what the model produced. It is what
/// stops ~150 historical fabrications from teaching the model, turn after turn,
/// that announcing tool use without calling a tool is the house style.
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

    strip_fabricated_tool_markers_from_assistant_turns(&mut normalized);
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

/// The provider a `/models <name>` switch selects: a catalog provider only
/// (a chat message never names a URL), spelled by its catalog row unless the
/// spelling selects a different regional endpoint, which it then keeps — so
/// `/models qwen-us` switches to the US endpoint, not to `qwen`.
fn resolve_provider_alias(name: &str) -> Option<String> {
    let candidate = name.trim().to_ascii_lowercase();
    let identity = providers::ProviderId::of(&candidate);
    if identity.namespace() != providers::ProviderNamespace::Catalog {
        return None;
    }
    let row = identity.catalog_name()?;
    if providers::ProviderId::of(row) == identity {
        Some(row.to_string())
    } else {
        Some(candidate)
    }
}

fn resolved_default_provider(config: &Config) -> String {
    providers::selection::default_provider(config).to_string()
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
            read_only_tool_concurrency_window: ctx.read_only_tool_concurrency_window,
            priority_scheduling_enabled: ctx.priority_scheduling_enabled,
            low_priority_tools: ctx.low_priority_tools.clone(),
            tool_tiering: ctx.tool_tiering.for_channel_surface(),
        };
    }
    #[cfg(not(test))]
    let _ = ctx;

    let config = generation.effective.as_ref();
    ChannelMessageRuntimeSnapshot {
        multimodal: config.multimodal.clone(),
        read_only_tool_concurrency_window: config.agent.read_only_tool_concurrency_window,
        priority_scheduling_enabled: config.agent.priority_scheduling_enabled,
        low_priority_tools: config.agent.low_priority_tools.clone(),
        // IM channel turns run under the channel tiering policy: the
        // operations/development surface is folded into `always_exclude` here,
        // once, so capability routing and the prompt compilers below cannot
        // disagree about what this turn may reach.
        tool_tiering: config.tool_tiering.for_channel_surface(),
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

    // Fabricated `[Used tools: ...]` lines are stripped here too, not only in
    // `normalize_cached_channel_turns`: this function is the read path for every
    // caller that consumes the union directly (transcript lookups, compaction
    // accounting), several of which never run the normalizer. The cached map and
    // the sqlite rows behind it are untouched — `merged` is a fresh clone.
    strip_fabricated_tool_markers_from_assistant_turns(&mut merged);
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
    // The reply is persisted (conversation turn and message event) redacted like
    // the terminal commit (D-053); the live history keeps the delivered text.
    let content = if role == "assistant" {
        crate::agent::terminal::redact_persisted_text(&turn.content)
    } else {
        turn.content.clone()
    };
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
        // Local history hydration under the system owner; not an external origin.
        require_owner_match: false,
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
                // stored user turns are neutralised like fresh input
                let turns = turns
                    .into_iter()
                    .map(|turn| crate::agent::turn_contracts::stored_turn_message(turn.role, turn.content))
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

/// Whether a history message carries the result of an executed tool call.
///
/// Native-tool mode emits a dedicated `tool` role message per call; prompt mode
/// folds every result of the iteration into one `user` message behind
/// [`crate::agent::turn_contracts::TOOL_RESULTS_PREFIX`].
#[cfg(test)]
fn is_tool_result_message(message: &ChatMessage) -> bool {
    message.role == "tool"
        || (message.role == "user"
            && message
                .content
                .starts_with(crate::agent::turn_contracts::TOOL_RESULTS_PREFIX))
}

/// Number of hex digits in the short failure code shown to the user.
const TURN_ERROR_ID_LEN: usize = 8;

/// Short, user-quotable code identifying one failed turn.
///
/// It is derived deterministically from the turn's run id, which is the value
/// the failure is logged under (`turn_run_id`) and the id the durable terminal
/// event is committed with. A user who quotes the code therefore pins an
/// operator to exactly one turn instead of "somewhere in the last few hours".
/// The run id is a UUIDv4, so its leading hex digits carry the same
/// randomness; a run id that is not hex-shaped (not produced today, but the
/// field is a plain `String`) is hashed instead so the function stays total.
pub(crate) fn turn_error_id(turn_run_id: &str) -> String {
    let hex: String = turn_run_id
        .chars()
        .filter(char::is_ascii_hexdigit)
        .map(|c| c.to_ascii_lowercase())
        .take(TURN_ERROR_ID_LEN)
        .collect();
    if hex.len() == TURN_ERROR_ID_LEN {
        return hex;
    }
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(turn_run_id.as_bytes());
    digest
        .iter()
        .take(TURN_ERROR_ID_LEN / 2)
        .fold(String::with_capacity(TURN_ERROR_ID_LEN), |mut acc, byte| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{byte:02x}");
            acc
        })
}

/// What went wrong on a channel turn, at the coarse granularity a chat user can
/// actually act on.
///
/// Every variant maps to a fixed sentence. Error text is never interpolated
/// into what the user sees, so no upstream message can carry a filesystem path,
/// a credential, an internal hostname or a stack out into the chat — the worst
/// a misclassification can do is give slightly wrong advice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnFailureKind {
    /// An image or file the conversation refers to could not be loaded.
    Attachment,
    /// The conversation no longer fits in the model's context window.
    ContextTooLong,
    /// Upstream asked us to slow down; waiting helps.
    RateLimited,
    /// Upstream refused on billing/entitlement grounds; waiting does not help.
    QuotaExhausted,
    /// We cannot authenticate to the model service.
    ServiceCredentials,
    /// The model service answered, but is failing or overloaded.
    ServiceUnavailable,
    /// The model service could not be reached at all.
    Network,
    /// The model service cannot accept some of the tool definitions this
    /// assistant is configured to offer it, so the request was not sent.
    /// Retrying cannot help; the configuration has to change.
    ToolSchemaRefused,
    /// Nothing matched. Deliberately says as little as possible.
    Unclassified,
}

impl TurnFailureKind {
    /// Stable machine token logged beside the full error, so a reported error
    /// id can be cross-checked against the class the classifier picked.
    pub(crate) const fn log_label(self) -> &'static str {
        match self {
            Self::Attachment => "attachment",
            Self::ContextTooLong => "context_too_long",
            Self::RateLimited => "rate_limited",
            Self::QuotaExhausted => "quota_exhausted",
            Self::ServiceCredentials => "service_credentials",
            Self::ServiceUnavailable => "service_unavailable",
            Self::Network => "network",
            Self::ToolSchemaRefused => "tool_schema_refused",
            Self::Unclassified => "unclassified",
        }
    }

    /// The user-visible sentence for this class.
    ///
    /// Every arm is a compile-time constant written for a non-engineer and says
    /// what the reader can do next. Nothing derived from the error is allowed
    /// in here.
    pub(crate) const fn advice(self) -> &'static str {
        match self {
            Self::Attachment => {
                "⚠️ I could not open an image or file that this conversation refers to — it looks like it has expired or been removed. Please send it again, or start a new conversation to clear it."
            }
            Self::ContextTooLong => {
                "⚠️ This conversation has grown too long for me to work through in one go. Please start a new conversation, or ask a shorter question."
            }
            Self::RateLimited => {
                "⚠️ There are too many requests going through right now, so I was asked to slow down. Please wait a minute and send your message again."
            }
            Self::QuotaExhausted => {
                "⚠️ My AI service account has run out of allowance, so I cannot answer right now. Trying again will not help — please let an administrator know."
            }
            Self::ServiceCredentials => {
                "⚠️ I cannot sign in to the AI service, so I cannot answer right now. An administrator needs to fix this — please pass on the error ID below."
            }
            Self::ServiceUnavailable => {
                "⚠️ The AI service is having trouble at the moment. Please try again in a few minutes."
            }
            Self::Network => "⚠️ I could not reach the AI service — the connection failed. Please try again.",
            Self::ToolSchemaRefused => {
                "⚠️ The AI service I am set up to use cannot accept some of the tools I am configured to offer it, so I cannot answer. This is a configuration problem that sending the message again will not fix — please pass the error ID below to an administrator."
            }
            Self::Unclassified => "⚠️ Something went wrong. Please try again later.",
        }
    }
}

/// Build the user-visible failure notice: fixed advice plus the code that ties
/// this message to one log record.
pub(crate) fn turn_failure_notice(kind: TurnFailureKind, error_id: &str) -> String {
    format!("{}\n(Error ID: {error_id})", kind.advice())
}

/// Classify a failed turn by scanning the whole `anyhow` cause chain.
///
/// The chain, not just the outermost message: a multimodal read failure or an
/// upstream 429 routinely arrives wrapped in a generic context line, and
/// matching only the head would classify all of those as unknown.
pub(crate) fn classify_turn_failure(err: &anyhow::Error) -> TurnFailureKind {
    // Typed first: a tool-schema refusal (or a failover chain that ended in
    // one) is a configuration problem, whatever words its tool names contain.
    // After a boundary that carried it as text, a cause that *is* a rendered
    // refusal still counts; upstream text that only quotes one does not
    // (SP-INT2 a51).
    if providers::tool_schema_refused_tools(err).is_some()
        || err
            .chain()
            .any(|cause| providers::is_tool_schema_refusal_text(&cause.to_string()))
    {
        return TurnFailureKind::ToolSchemaRefused;
    }
    if crate::recovery::overflow::is_context_overflow_error(err) {
        return TurnFailureKind::ContextTooLong;
    }
    let mut chain_text = String::new();
    for cause in err.chain() {
        use std::fmt::Write as _;
        let _ = writeln!(chain_text, "{cause}");
    }
    chain_text.make_ascii_lowercase();
    classify_turn_failure_text(&chain_text)
}

/// Classifier core, split out so it can be exercised on literal error text.
///
/// `lower` must already be lowercased. The hint strings were taken from the
/// error shapes this branch has actually produced in production plus the
/// `Display` impls that can reach it (`MultimodalError`, `StreamError`, the
/// per-provider `"{provider} API error ({status}): …"` renderings and the
/// reliability layer's aggregate). Order is deliberate: narrow, unambiguous
/// markers first, and a billing refusal is recognised before the generic 429
/// wording it arrives with. Anything matching nothing stays `Unclassified` —
/// saying too little is recoverable, telling a user the wrong thing is not.
fn classify_turn_failure_text(lower: &str) -> TurnFailureKind {
    /// Emitted only by the multimodal / media-artifact preparation stage. The
    /// production incident this classifier exists for lands on the first entry.
    const ATTACHMENT_HINTS: [&str; 8] = [
        "failed to read local image",
        "failed to download remote image",
        "invalid multimodal image marker",
        "multimodal remote image",
        "multimodal image",
        "image url targets a private/local address",
        "media artifact i/o failed",
        "media source is not a regular readable file",
    ];
    /// Billing / entitlement refusals: retrying cannot clear these.
    const QUOTA_HINTS: [&str; 9] = [
        "insufficient balance",
        "insufficient_balance",
        "insufficient quota",
        "insufficient_quota",
        "quota exhausted",
        "exceeded your current quota",
        "credit balance is too low",
        "plan does not include",
        "billing",
    ];
    /// Authentication and credential-configuration failures. `reqwest` renders
    /// a status as `401 Unauthorized`, so the reason phrase is the reliable
    /// marker; a bare `401` is not matched, because it also occurs inside
    /// request ids and token counts.
    const CREDENTIAL_HINTS: [&str; 12] = [
        "unauthorized",
        "403 forbidden",
        "http 403",
        "invalid authentication",
        "invalid_authentication",
        "authentication_error",
        "api key appears to be invalid",
        "invalid api key",
        "incorrect api key",
        "invalid_api_key",
        "api key not set",
        "oauth refresh failed",
    ];
    /// Explicit 5xx statuses. Checked before the rate-limit hints because the
    /// streaming layer reports a 503 through a variant whose `Display` still
    /// begins with "Rate limited".
    const SERVER_STATUS_HINTS: [&str; 9] = [
        "500 internal server error",
        "502 bad gateway",
        "503 service unavailable",
        "504 gateway timeout",
        "http 500",
        "http 502",
        "http 503",
        "http 504",
        "http 529",
    ];
    /// Throttling that clears on its own.
    const RATE_LIMIT_HINTS: [&str; 6] = [
        "too many requests",
        "http 429",
        "rate limited",
        "rate limit",
        "rate_limit",
        "slow down",
    ];
    /// Upstream capacity problems stated in prose rather than as a status
    /// (Anthropic `overloaded_error`, Kimi `engine_overloaded_error`). Checked
    /// after the rate-limit hints so a 429 that merely explains itself as
    /// "overloaded" still gets the "wait a minute" advice.
    const OVERLOAD_TEXT_HINTS: [&str; 6] = [
        "overloaded",
        "service unavailable",
        "bad gateway",
        "gateway timeout",
        "internal server error",
        "temporarily unavailable",
    ];
    /// Transport-level failures: nothing upstream was reached at all.
    const NETWORK_HINTS: [&str; 12] = [
        "error sending request",
        "error decoding response body",
        "network retries exhausted",
        "connection refused",
        "connection reset",
        "connection closed",
        "timed out",
        "timeout",
        "dns error",
        "failed to lookup address",
        "network is unreachable",
        "broken pipe",
    ];

    let hit = |hints: &[&str]| hints.iter().any(|hint| lower.contains(hint));

    // A tool-schema refusal is recognised by `classify_turn_failure` before
    // this text is built, and never by a marker anywhere in it: upstream
    // bodies are part of this text.
    if hit(&ATTACHMENT_HINTS) {
        TurnFailureKind::Attachment
    } else if hit(&QUOTA_HINTS) {
        TurnFailureKind::QuotaExhausted
    } else if hit(&CREDENTIAL_HINTS) {
        TurnFailureKind::ServiceCredentials
    } else if hit(&SERVER_STATUS_HINTS) {
        TurnFailureKind::ServiceUnavailable
    } else if hit(&RATE_LIMIT_HINTS) {
        TurnFailureKind::RateLimited
    } else if hit(&OVERLOAD_TEXT_HINTS) {
        TurnFailureKind::ServiceUnavailable
    } else if hit(&NETWORK_HINTS) {
        TurnFailureKind::Network
    } else {
        TurnFailureKind::Unclassified
    }
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
    model_name: &str,
    generation: &Arc<crate::config::ConfigGeneration>,
) -> anyhow::Result<Arc<dyn Provider>> {
    providers::generation::get_or_create_provider_for_generation(
        &ctx.config,
        &ctx.provider_cache,
        &ctx.provider_runtime_options,
        provider_name,
        model_name,
        generation,
    )
    .await
}

/// A hint is one selected route, not a provider-wide chain. Constructing the
/// selected destination separately keeps two hints for the same provider from
/// sharing whichever credential was cached first.
fn build_channel_hint_provider(
    config: &Config,
    selected_provider: &str,
    selected_model: &str,
    options: &providers::ProviderRuntimeOptions,
) -> anyhow::Result<Option<(ChannelRouteSelection, Arc<dyn Provider>)>> {
    if !selected_model.trim().starts_with("hint:") {
        return Ok(None);
    }
    let catalog = providers::selection::catalog_for(config);
    let resolved = providers::selection::check_with(
        config,
        &catalog,
        selected_provider,
        selected_model,
        providers::selection::credential_for(config, selected_provider),
        &providers::catalog::Requirement::chat(),
        providers::catalog::UnknownPolicy::Invocation,
    )?;
    let provider = providers::create_provider_for(&resolved, &catalog, &config.reliability, &config.cost, options)?;
    let route = ChannelRouteSelection {
        provider: resolved.provider_label().to_string(),
        model: resolved.model().to_string(),
    };
    Ok(Some((route, Arc::from(provider))))
}

/// Build the default channel provider from the exact resolver release used by
/// channel turns. The returned names are the wire destination after a route
/// hint has been resolved.
pub(crate) fn build_channel_startup_provider(
    config: &Config,
    options: &providers::ProviderRuntimeOptions,
) -> anyhow::Result<(Arc<dyn Provider>, String, String)> {
    let catalog = providers::selection::catalog_for(config);
    let resolved = providers::selection::check_default(config, providers::catalog::UnknownPolicy::Invocation)?;
    let provider: Arc<dyn Provider> = Arc::from(providers::create_routed_provider_for(
        &resolved, &catalog, config, options,
    )?);
    Ok((
        provider,
        resolved.provider_label().to_string(),
        resolved.model().to_string(),
    ))
}

fn build_models_help_response(current: &ChannelRouteSelection, workspace_dir: &Path) -> String {
    let mut response = String::new();
    let _ = writeln!(
        response,
        "Current provider: `{}`\nCurrent model: `{}`",
        current.display_provider(),
        current.model
    );
    response.push_str("\nSwitch model with `/model <model-id>`.\n");

    let cached_models = load_cached_model_preview(workspace_dir, &current.provider);
    if cached_models.is_empty() {
        let _ = writeln!(
            response,
            "\nNo cached model list found for `{}`. Ask the operator to run `prx models refresh --provider {}`.",
            current.display_provider(),
            current.display_provider()
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
        current.display_provider(),
        current.model
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
                // The new provider is resolved with the sender's current model
                // before anything is built or switched.
                let catalog = providers::selection::catalog_for(&config_generation.effective);
                let route = providers::selection::route_target(&catalog, &provider_name, &current.model)
                    .ok()
                    .flatten();
                let refusal = providers::selection::check_with(
                    &config_generation.effective,
                    &catalog,
                    &provider_name,
                    &current.model,
                    providers::selection::credential_for(&config_generation.effective, &provider_name),
                    &providers::catalog::Requirement::chat(),
                    providers::catalog::UnknownPolicy::Invocation,
                )
                .map_err(|refusal| providers::selection::route_refusal(route, refusal))
                .err();
                if let Some(refusal) = refusal {
                    format!(
                        "{refusal}. Route unchanged; switch the model first with `/model <model-id>` or pick \
                         another provider."
                    )
                } else {
                    let ready = if current.model.trim().starts_with("hint:") {
                        build_channel_hint_provider(
                            &config_generation.effective,
                            &provider_name,
                            &current.model,
                            &providers::provider_runtime_options_from_config(&config_generation.effective),
                        )
                        .map(|_| ())
                    } else {
                        get_or_create_provider_for_generation(ctx, &provider_name, &current.model, config_generation)
                            .await
                            .map(|_| ())
                    };
                    match ready {
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
            }
            None => format!("Unknown provider `{raw_provider}`. Use `/models` to list valid providers."),
        },
        ChannelRuntimeCommand::ShowModel => build_models_help_response(&current, ctx.workspace_dir.as_path()),
        ChannelRuntimeCommand::SetModel(raw_model) => {
            let model = raw_model.trim().trim_matches('`').trim().to_string();
            let refusal = (!model.is_empty())
                .then(|| {
                    providers::selection::check(
                        &config_generation.effective,
                        &current.provider,
                        &model,
                        &providers::catalog::Requirement::chat(),
                        providers::catalog::UnknownPolicy::Invocation,
                    )
                    .err()
                })
                .flatten();
            if model.is_empty() {
                "Model ID cannot be empty. Use `/model <model-id>`.".to_string()
            } else if let Some(refusal) = refusal {
                format!("{refusal}. Route unchanged.")
            } else {
                current.model = model.clone();
                set_route_selection(ctx, config_generation, &sender_key, current.clone());
                clear_sender_history(ctx, &sender_key);

                format!(
                    "Model switched to `{model}` for provider `{}` in this sender session.",
                    current.display_provider()
                )
            }
        }
    };

    if let Err(err) = send_recording_activity(
        channel.as_ref(),
        &SendMessage::new(response, &msg.reply_target).in_thread(msg.thread_ts.clone()),
    )
    .await
    {
        tracing::warn!("Failed to send runtime command response on {}: {err}", channel.name());
    }

    true
}

/// Exercise the production `/models` handler with a retained route refusal.
/// Kept test-only because the runtime owns its context and outbound channel.
#[cfg(test)]
pub(crate) async fn broken_route_models_command_response(config: &Config) -> String {
    #[derive(Default)]
    struct RefusalChannel {
        sent: tokio::sync::Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl Channel for RefusalChannel {
        fn name(&self) -> &str {
            "telegram"
        }
        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent.lock().await.push(message.content.clone());
            Ok(())
        }
        async fn listen(&self, _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let shared = crate::config::new_shared(config.clone());
    let generation = shared.pin();
    let channel_impl = Arc::new(RefusalChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let mut ctx = tests::ctx_with_histories(HashMap::new());
    ctx.config = shared;
    ctx.config_generation = Arc::clone(&generation);

    let msg = traits::ChannelMessage {
        id: "prx-broken-route-command".to_string(),
        sender: "prx_user".to_string(),
        reply_target: "prx_channel".to_string(),
        content: "/models openai".to_string(),
        channel: "telegram".to_string(),
        ..traits::ChannelMessage::default()
    };
    let key = ConversationKey::from_message(&msg);
    set_route_selection(
        &ctx,
        &generation,
        &key,
        ChannelRouteSelection {
            provider: providers::selection::default_provider(config).to_string(),
            model: "hint:broken".to_string(),
        },
    );
    assert!(
        handle_runtime_command_if_needed(&ctx, &generation, &msg, Some(&channel)).await,
        "the production /models handler did not claim the command"
    );
    channel_impl
        .sent
        .lock()
        .await
        .last()
        .cloned()
        .expect("the production /models handler did not send a refusal")
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
    Fallback { reason: &'static str },
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

    serde_json::from_str::<SignalVisionPreflightReport>(trimmed.get(object_start..=object_end)?).ok()
}

struct SignalVisionProvider<'a> {
    provider: &'a dyn Provider,
    model: &'a str,
    temperature: f64,
}

struct SignalVisionScope {
    accounting: Arc<crate::runtime::auxiliary_call::AuxiliaryAccounting>,
    observation: Arc<crate::runtime::auxiliary_call::AuxiliaryProviderCall>,
}

async fn run_signal_vision_preflight(
    target: SignalVisionProvider<'_>,
    msg: &traits::ChannelMessage,
    multimodal_config: &crate::config::MultimodalConfig,
    media_artifacts: &crate::media::MediaArtifactOwner,
    scope: Option<&SignalVisionScope>,
    cancellation: &CancellationToken,
) -> SignalVisionPreflightOutcome {
    let SignalVisionProvider {
        provider,
        model,
        temperature,
    } = target;
    if !is_signal_image_message(msg) {
        return SignalVisionPreflightOutcome::NotRequired;
    }
    let fallback = |reason| SignalVisionPreflightOutcome::Fallback { reason };
    let Some(SignalVisionScope {
        accounting,
        observation,
    }) = scope
    else {
        return fallback("auxiliary vision source or accounting admission refused");
    };
    if !provider
        .capabilities_for(model, crate::providers::traits::ProviderRequestMode::NonStreaming)
        .vision
    {
        return fallback("auxiliary vision unsupported by selected provider");
    }
    let deadline = tokio::time::Instant::from_std(accounting.deadline());
    if cancellation.is_cancelled() || tokio::time::Instant::now() >= deadline {
        return fallback("auxiliary vision cancelled or expired");
    }
    let (cleaned_text, image_refs) = crate::multimodal::parse_image_markers(&msg.content);
    if image_refs.is_empty() {
        return fallback("auxiliary vision has no usable image markers");
    }
    let mut user_prompt = cleaned_text.trim().to_owned();
    user_prompt.push_str("\n\n[Analyze attached images first]");
    for image_ref in image_refs {
        user_prompt.push_str("\n[IMAGE:");
        user_prompt.push_str(image_ref.trim());
        user_prompt.push(']');
    }
    let messages = vec![
        ChatMessage::system(
            "You are a strict vision preflight validator. Return JSON only: {\"status\":\"ok\"|\"uncertain\",\"confidence\":0.0,\"summary\":\"...\"}. If any detail is unreadable, cropped, ambiguous or uncertain, use uncertain and confidence below 0.60. Never guess brand, dosage, specification or product identity.",
        ),
        crate::agent::turn_contracts::inbound_user_message_owned(user_prompt),
    ];
    let prepared = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return fallback("auxiliary vision preparation cancelled"),
        result = tokio::time::timeout_at(deadline, crate::multimodal::prepare_messages_for_provider(&messages, multimodal_config, media_artifacts)) => {
            match result {
                Ok(Ok(prepared)) if prepared.contains_images => prepared,
                _ => return fallback("auxiliary vision preparation failed or expired"),
            }
        }
    };
    if observation.begin().is_err() {
        return fallback("auxiliary vision observation admission failed");
    }
    let accounting: Arc<dyn crate::providers::attempt_accounting::AttemptAccounting> = accounting.clone();
    let trace = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return fallback("auxiliary vision cancelled"),
        result = tokio::time::timeout_at(deadline, crate::providers::attempt_accounting::chat_traced(
            provider, ChatRequest { messages: &prepared.messages, tools: None }, model, temperature, accounting,
        )) => {
            match result {
                Ok(Ok(trace)) => trace,
                _ => return fallback("auxiliary vision accounted request failed or expired"),
            }
        }
    };
    if observation.complete(&trace).is_err() {
        return fallback("auxiliary vision observation completion failed");
    }
    signal_vision_report_context(trace.response.text_or_empty()).map_or_else(
        || fallback("auxiliary vision report is invalid or uncertain"),
        |context| SignalVisionPreflightOutcome::Ready { context },
    )
}

fn signal_vision_report_context(raw: &str) -> Option<String> {
    let report = parse_signal_vision_preflight_report(raw)?;
    let confidence = report.confidence?;
    let summary = report.summary_text()?;
    if report.status.trim() != "ok" || !confidence.is_finite() || !(0.60..=1.0).contains(&confidence) {
        return None;
    }
    Some(format!(
        "[signal-vision-preflight confidence={confidence:.2}]\n{summary}\n[/signal-vision-preflight]"
    ))
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
            // Declare how this incarnation of the listener proves liveness and
            // reset its activity baseline, so a restarted listener is not judged
            // on the silence of the one it replaced.
            activity::register(ch.name(), ch.liveness_expectation());
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
                            publish_listener_health(ch.name(), &component);
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
                    // The whole chain ("failed to bind …: address in use"), the
                    // log copy redacted (D-072 L-3, L-4).
                    tracing::error!(
                        "Channel {} error: {}; restarting",
                        ch.name(),
                        crate::security::redaction::redact_secrets(&format!("{e:#}"))
                    );
                    crate::health::mark_component_error(&component, format!("{e:#}"));
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

/// Publish what the listener has actually been observed doing.
///
/// This replaces an unconditional `mark_component_ok` on every heartbeat tick.
/// That older signal only proved the heartbeat timer was still scheduled, so a
/// listener wedged forever inside `listen()` — a long poll that never returns, a
/// socket that never errors — kept publishing perfect health while receiving
/// nothing. Health is now derived from `channels::activity`, which records real
/// traffic and completed upstream round-trips.
///
/// The stall verdict is a *report*, and must stay one. The listener is left
/// running untouched and returns to `Ready` on its own the moment activity
/// resumes. Never wire a restart, an abort, a reconnect or a cancellation to it:
/// prx imposes no execution timeouts, and only an operator can tell a wedged
/// channel from a deliberately quiet one.
fn publish_listener_health(channel: &str, component: &str) {
    let Some(status) = activity::status(channel) else {
        // Nothing has ever been recorded for this channel, so there is no
        // evidence to publish. Refresh freshness without claiming readiness
        // rather than asserting a health we cannot back up.
        crate::health::touch_component(component);
        return;
    };

    crate::health::set_component_activity(component, Some(status.to_component_activity()));
    if status.stalled {
        crate::health::mark_component_degraded(component, status.stall_summary());
    } else {
        crate::health::mark_component_ok(component);
    }
}

/// Deliver a reply and record the outbound activity when it succeeds.
///
/// A successful send proves the credentials, the network and the remote API are
/// still working. It is tracked separately from the receive path and is
/// deliberately excluded from the stall verdict in `channels::activity`: a
/// channel can keep answering perfectly while its listener is deaf, and masking
/// that with outbound traffic is exactly the false-health this design removes.
async fn send_recording_activity(channel: &dyn Channel, message: &SendMessage) -> anyhow::Result<()> {
    let result = channel.send(message).await;
    if result.is_ok() {
        activity::record_outbound(channel.name());
    }
    result
}

/// Finalize a streamed draft and record the outbound activity when it succeeds.
async fn finalize_draft_recording_activity(
    channel: &dyn Channel,
    recipient: &str,
    message_id: &str,
    text: &str,
) -> anyhow::Result<()> {
    let result = channel.finalize_draft(recipient, message_id, text).await;
    if result.is_ok() {
        activity::record_outbound(channel.name());
    }
    result
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

fn log_worker_join_result(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result {
        tracing::error!("Channel message worker crashed: {error}");
    }
}

/// Streamed assistant text written into a channel draft, one draft per turn.
///
/// `has_text` goes true the moment the first delta arrives and is the interlock
/// with [`spawn_channel_progress_reporter`]: progress notes are only ever
/// written into a draft that has no real text in it yet, so a long turn shows
/// what it is doing without a reply ever being overwritten by a status line.
/// Every update rewrites the whole accumulated string, so even a lost race with
/// a progress note repairs itself on the next delta.
fn spawn_channel_draft_updater(
    channel: Arc<dyn Channel>,
    reply_target: String,
    draft_id: String,
    mut rx: tokio::sync::mpsc::Receiver<String>,
    has_text: Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut accumulated = String::new();
        while let Some(delta) = rx.recv().await {
            accumulated.push_str(&delta);
            has_text.store(true, std::sync::atomic::Ordering::Release);
            if let Err(e) = channel.update_draft(&reply_target, &draft_id, &accumulated).await {
                tracing::debug!("Draft update failed: {e}");
            }
        }
    })
}

/// One line of "still working" for a turn that has not produced text yet.
fn channel_progress_line(tool: &str, iteration: usize, elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    let elapsed_text = if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    };
    if tool.is_empty() {
        format!("⏳ working… (step {iteration}, {elapsed_text})")
    } else {
        format!("⏳ {tool}… (step {iteration}, {elapsed_text})")
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

/// Registry label for one channel turn — enough to tell two live turns apart in
/// `prx tasks list` without spilling message content into an operator listing.
///
/// The correspondent is named by the same fingerprint the outbound path uses
/// ([`crate::security::op_id::ref_for_channel_recipient`]), never in plaintext.
/// A sender on a messaging platform *is* an address — a phone number, a JID, a
/// chat id — and this row is not a private note: it is what
/// `GET /api/runtime/tasks` hands to every caller the pairing check admits, and
/// pairing is off by default. Spelling the sender out here would have made the
/// busiest row in the runtime, minted once per inbound message, the place a
/// correspondent's address leaks, while every send *to* that same person was
/// carefully fingerprinted.
///
/// Fingerprinting through that shared function rather than a local one is what
/// makes the listing still useful: the digest of an inbound turn matches the
/// digest of an outbound send to the same correspondent on the same channel, so
/// "this turn and that send are the same person" survives the redaction, and
/// the channel name — not sensitive, and the only part that says *where* — is
/// left readable.
///
/// MUTATION GUARD: interpolate `msg.sender` or `msg.reply_target` directly and
/// `a_channel_turn_label_fingerprints_the_correspondent` fails.
fn channel_turn_label(msg: &traits::ChannelMessage) -> String {
    use std::fmt::Write as _;
    let mut label = format!(
        "channel:{}:{}",
        msg.channel,
        crate::security::op_id::ref_for_channel_recipient(&msg.channel, &msg.sender)
    );
    if !msg.reply_target.is_empty() && msg.reply_target != msg.sender {
        // Writing into a String is infallible; the Result is discarded rather
        // than unwrapped so this stays panic-free by construction.
        let _ = write!(
            label,
            " in {}",
            crate::security::op_id::ref_for_channel_recipient(&msg.channel, &msg.reply_target)
        );
    }
    label
}

/// Handle one inbound channel message as a registered agent turn.
///
/// Nothing ends a turn on a clock any more, so a turn that wedges — in a tool,
/// in a provider call, or anywhere between them — can only be dealt with if an
/// operator can *see* it and end it by hand. That is what this wrapper exists
/// for: the turn is registered before any of its own work starts, and the whole
/// turn runs inside [`crate::runtime::registry::scoped`], so the tool calls,
/// sub-agents and child processes it goes on to start resolve it as their
/// parent and `prx tasks kill --cascade <turn>` reaches every one of them.
///
/// The guard is owned by the awaited future rather than by this scope, so the
/// row disappears with the turn however the turn ends — normal return, panic,
/// or a dropped future.
async fn process_channel_message(
    ctx: Arc<ChannelRuntimeContext>,
    msg: traits::ChannelMessage,
    cancellation_token: CancellationToken,
) {
    // D8-1: one run_id per channel turn. Minted here rather than deeper inside
    // the turn so the registry row and the turn's stored provenance (every
    // append_sender_turn call, the per-turn context envelope, the terminal
    // record) all carry the same id, and an operator reading `prx tasks list`
    // can correlate a live row with what it later wrote.
    let turn_run_id = uuid::Uuid::new_v4().to_string();
    let guard = crate::runtime::registry::register_turn(
        &channel_turn_label(&msg),
        &turn_run_id,
        Some(cancellation_token.clone()),
    );
    crate::runtime::registry::scoped(guard, boxed_channel_work(ctx, msg, cancellation_token, turn_run_id)).await;
}

// Construct the large channel-work future outside the caller's async poll
// frame. One Box is owned by the unchanged registry scope through completion
// or cancellation; constructing it does not poll the channel work.
fn boxed_channel_work(
    ctx: Arc<ChannelRuntimeContext>,
    msg: traits::ChannelMessage,
    cancellation_token: CancellationToken,
    turn_run_id: String,
) -> std::pin::Pin<Box<impl std::future::Future<Output = ()>>> {
    Box::pin(run_channel_turn(ctx, msg, cancellation_token, turn_run_id))
}

async fn run_channel_turn(
    ctx: Arc<ChannelRuntimeContext>,
    msg: traits::ChannelMessage,
    cancellation_token: CancellationToken,
    turn_run_id: String,
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

    let history_key = ConversationKey::from_message(&msg);
    let message_visibility = channel_message_visibility(&msg);
    let route = get_route_selection_for_generation(ctx.as_ref(), &history_key, &config_generation);
    let runtime_defaults = runtime_defaults_for_generation(ctx.as_ref(), &config_generation);
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
    let input_owner = match crate::runtime::turn_cleanup::current_tracker() {
        Some(owner) => owner,
        None => {
            tracing::warn!("channel input cleanup owner missing");
            return;
        }
    };
    let input_ticket = match input_owner.register(cancellation_token.clone()) {
        Ok(ticket) => ticket,
        Err(_) => {
            tracing::warn!("channel input admission is closed");
            return;
        }
    };
    let (inbound_event, channel_source, mut admission_guard) =
        match decision_source::ChannelSourceReceipt::record_guarded(
            Arc::clone(&ctx),
            Arc::clone(&config_generation),
            msg.clone(),
            turn_run_id.clone(),
            input_ticket,
        )
        .await
        {
            Ok(input) => input,
            Err(error) if error.is::<crate::runtime::ingress_recovery::AlreadyAccepted>() => {
                tracing::info!(
                    outcome = "already_accepted_unconfirmed",
                    "channel input was previously accepted; execution was not repeated"
                );
                if is_direct_chat
                    && authorize_channel_outbound(ctx.as_ref(), security.as_ref(), &msg.channel).is_ok()
                    && ensure_notice_before_channel_output(
                        ctx.as_ref(),
                        &config_generation,
                        &msg,
                        target_channel.as_ref(),
                    )
                    .await
                {
                    if let Some(channel) = target_channel.as_ref() {
                        let _=send_recording_activity(channel.as_ref(),&SendMessage::new("This input was already accepted. Execution was not repeated; its outcome remains unconfirmed.",&msg.reply_target).in_thread(msg.thread_ts.clone())).await;
                    }
                }
                return;
            }
            Err(_) => {
                tracing::warn!("channel input persistence is unconfirmed");
                return;
            }
        };

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
            // V02-R5: a channel turn is never owner-isolated.
            owner_id: None,
            require_owner_match: false,
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
            admission_guard.finish_without_decision();
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
            admission_guard.finish_without_decision();
            return;
        }
        if smart_proactive_within_cooldown(ctx.as_ref(), &msg) {
            tracing::debug!(channel = %msg.channel, "smart: skip proactive reply (within cooldown window)");
            admission_guard.finish_without_decision();
            return;
        }
    }

    // Heuristics may prepare the admitted pre-gate path, but never terminate a
    // turn. A possible Skip is confirmed only by the resolver-built auxiliary
    // classifier after source decision admission below.
    let smart_pre_gate_input = (smart_group && !mentioned).then(|| {
        let cfg = &config_generation.effective.smart_group;
        let (recent_context, bot_recently_active) =
            gather_pre_gate_history(ctx.as_ref(), &history_key, cfg.classifier_max_context_messages);
        let heuristic = pre_gate::classify_heuristic(&user_text_for_mention, &ctx.bot_names, bot_recently_active);
        (recent_context, heuristic)
    });

    let mut decision_turn = match decision_turn::ChannelDecisionTurn::admit(
        ctx.as_ref(),
        &channel_source,
        &config_generation,
        &security,
        &route,
        &cancellation_token,
        &mut admission_guard,
    )
    .await
    {
        Ok(turn) => turn,
        Err(error) => {
            if admission_guard
                .finish_failure(cancellation_token.is_cancelled())
                .await
                .is_err()
            {
                tracing::warn!("channel admission failure persistence is unconfirmed");
            }
            tracing::warn!(
                "channel decision admission failed: {}",
                providers::sanitize_api_error(&error.to_string())
            );
            return;
        }
    };
    if let Some((recent_context, heuristic)) = smart_pre_gate_input {
        let pre_gate = run_admitted_smart_pre_gate(
            ctx.as_ref(),
            &config_generation,
            &mut decision_turn,
            heuristic,
            &recent_context,
            &user_text_for_mention,
            &cancellation_token,
        )
        .await;
        tracing::debug!(
            channel = %msg.channel,
            path = pre_gate.outcome.path.as_str(),
            enter_loop = pre_gate.outcome.should_enter_loop(),
            "admitted smart pre-gate decision"
        );
        if !pre_gate.outcome.should_enter_loop() {
            let Some(provider_outcome) = pre_gate.provider_outcome else {
                tracing::warn!("smart pre-gate refused silence without a completed classifier outcome");
                return;
            };
            if let Err(error) = decision_turn
                .commit(crate::agent::terminal::TurnTerminalDraft {
                    decision: None,
                    terminal_id: turn_run_id.clone(),
                    scope: channel_source.envelope().message_scope(),
                    status: crate::agent::terminal::TurnTerminalStatus::Silent,
                    history: None,
                    history_scope: None,
                    provider_outcome: Some(provider_outcome),
                    telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                        summary: "smart-group classifier declined a proactive reply".to_owned(),
                        started_at: pre_gate.started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Suppress {
                        reason: "smart-group classifier declined a proactive reply".to_owned(),
                    },
                })
                .await
            {
                tracing::warn!(%error, "smart pre-gate silent terminal failed");
                return;
            }
            println!(
                "  🤫 Smart pre-gate: staying silent (no agent loop) [{}]",
                pre_gate.outcome.path.as_str()
            );
            return;
        }
    }
    match decision_turn.close_reply() {
        Ok(Some(reply)) => {
            if let Err(error) = decision_turn
                .close(ctx.as_ref(), security.as_ref(), &msg, &cancellation_token)
                .await
            {
                tracing::warn!(%error, "channel decision close failed");
                return;
            }
            let _ = append_sender_turn(
                ctx.as_ref(),
                &config_generation,
                &history_key,
                &msg.channel,
                &msg.sender,
                Some(&msg.reply_target),
                ChatMessage::assistant(&reply),
                message_visibility.clone(),
                None,
                None,
                &turn_run_id,
                false,
            )
            .await;
            if let Some(channel) = target_channel.as_ref() {
                let _ = send_recording_activity(
                    channel.as_ref(),
                    &SendMessage::new(reply, &msg.reply_target).in_thread(msg.thread_ts.clone()),
                )
                .await;
            }
            return;
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(%error, "invalid channel decision disposition");
            return;
        }
    }
    let vision_required = is_signal_image_message(&msg);
    if vision_required {
        if authorize_channel_outbound(ctx.as_ref(), security.as_ref(), &msg.channel).is_err()
            || !ensure_notice_before_channel_output(ctx.as_ref(), &config_generation, &msg, target_channel.as_ref())
                .await
        {
            return;
        }
    }
    let mut vision_scope = None;
    let decision_turn = if vision_required {
        match decision_turn.vision_accounting() {
            Ok(accounting) => {
                let (turn, observation) = decision_turn.with_vision_observation(&accounting);
                vision_scope = Some(SignalVisionScope {
                    accounting,
                    observation,
                });
                turn
            }
            Err(_) => decision_turn,
        }
    } else {
        decision_turn
    };
    let preflight = if vision_required {
        if let Some(scope) = vision_scope.as_ref() {
            let provider = scope.accounting.provider();
            run_signal_vision_preflight(
                SignalVisionProvider {
                    provider: provider.as_ref(),
                    model: scope.accounting.selected_model().model(),
                    temperature: runtime_defaults.temperature,
                },
                &msg,
                &message_runtime.multimodal,
                ctx.hooks.media_artifacts().as_ref(),
                Some(scope),
                &cancellation_token,
            )
            .await
        } else {
            SignalVisionPreflightOutcome::Fallback {
                reason: "auxiliary vision source or accounting admission refused",
            }
        }
    } else {
        SignalVisionPreflightOutcome::NotRequired
    };
    let signal_vision_context = match preflight {
        SignalVisionPreflightOutcome::NotRequired => None,
        SignalVisionPreflightOutcome::Ready { context } => Some(context),
        SignalVisionPreflightOutcome::Fallback { reason } => {
            // Image-only admission already passed outbound and identity gates.
            let fallback = SIGNAL_IMAGE_UNCERTAINTY_FALLBACK.to_string();
            if let Err(error) = decision_turn
                .fail(&anyhow::anyhow!(reason), cancellation_token.is_cancelled())
                .await
            {
                tracing::warn!(%error, "channel vision fallback terminal failed");
                return;
            }

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
                if let Err(e) = send_recording_activity(
                    channel.as_ref(),
                    &SendMessage::new(fallback, &msg.reply_target).in_thread(msg.thread_ts.clone()),
                )
                .await
                {
                    eprintln!("  ❌ Failed to reply on {}: {e}", channel.name());
                }
            }
            return;
        }
    };

    let route_display = route.display_provider().into_owned();
    let selected_provider = vision_scope.as_ref().map_or_else(
        || decision_turn.build_provider(),
        |scope| {
            let selected = scope.accounting.selected_model();
            Ok((
                ChannelRouteSelection {
                    provider: selected.provider_label().to_owned(),
                    model: selected.model().to_owned(),
                },
                scope.accounting.provider(),
            ))
        },
    );
    let (route, active_provider) = match selected_provider {
        Ok(selected) => selected,
        Err(err) => {
            if let Err(error) = decision_turn.fail(&err, cancellation_token.is_cancelled()).await {
                tracing::warn!(%error, "channel provider setup terminal failed");
                return;
            }

            if authorize_channel_outbound(ctx.as_ref(), security.as_ref(), &msg.channel).is_err()
                || !ensure_notice_before_channel_output(ctx.as_ref(), &config_generation, &msg, target_channel.as_ref())
                    .await
            {
                return;
            }
            let safe_err = providers::sanitize_api_error(&err.to_string());
            let message = format!(
                "⚠️ Failed to initialize provider `{}`. Please run `/models` to choose another provider.\nDetails: {safe_err}",
                route_display
            );
            if let Some(channel) = target_channel.as_ref() {
                let _ = send_recording_activity(
                    channel.as_ref(),
                    &SendMessage::new(message, &msg.reply_target).in_thread(msg.thread_ts.clone()),
                )
                .await;
            }
            return;
        }
    };

    // Shared events are refreshed each turn so other entrypoints (chat/gateway)
    // can be observed by channel turns without waiting for a session boundary.
    // D8-1: reuse the same per-turn run_id as the message-event envelope so the
    // whole turn (inbound user event, context/recall, outbound assistant event)
    // shares one run_id.
    let runtime_envelope = channel_source.envelope().clone();
    let route_decision = decision_turn.route.clone();
    let memory_context = match decision_turn.memory_context() {
        Ok(context) => context,
        Err(error) => {
            tracing::warn!(%error, "channel decision retrieval failed");
            return;
        }
    };
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

    let selected_skills = match decision_turn.prompt_skills() {
        Ok(skills) => skills,
        Err(error) => {
            tracing::warn!(%error, "channel decision skill metadata unavailable");
            return;
        }
    };
    let base_system_prompt = build_system_prompt_with_mode(
        &ctx.workspace_dir,
        &route.model,
        &selected_skills,
        Some(&config_generation.effective.identity),
        if config_generation.effective.agent.compact_context {
            Some(6000)
        } else {
            None
        },
        active_provider
            .capabilities_for(
                &route.model,
                crate::providers::traits::ProviderRequestMode::NonStreaming,
            )
            .native_tool_calling,
    );
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
                if let Some(context) = signal_vision_context.as_deref() {
                    last_turn.content.push('\n');
                    last_turn.content.push_str(context);
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
    if !vision_required {
        if let Err(reason) = authorize_channel_outbound(ctx.as_ref(), security.as_ref(), &msg.channel) {
            tracing::warn!(
                channel = %msg.channel,
                sender = %msg.sender,
                "channel outbound blocked by InboundGate: {reason}"
            );
            return;
        }
        // Article 50 transparency control: after inbound/outbound authorization but
        // before draft creation, streaming, or the final AI response, deliver and
        // minimally acknowledge the versioned AI identity notice. A required notice
        // failure is fail-closed for this turn.
        if !ensure_notice_before_channel_output(ctx.as_ref(), &config_generation, &msg, target_channel.as_ref()).await {
            return;
        }
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

    let draft_has_text = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let draft_updater = if let (Some(rx), Some(draft_id_ref), Some(channel_ref)) =
        (delta_rx, draft_message_id.as_deref(), target_channel.as_ref())
    {
        Some(spawn_channel_draft_updater(
            Arc::clone(channel_ref),
            msg.reply_target.clone(),
            draft_id_ref.to_string(),
            rx,
            Arc::clone(&draft_has_text),
        ))
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
    // The typing refresher is a detached task that stops only when its token
    // is cancelled, and a token is not cancelled on drop. When the runtime
    // aborts this turn (a generation swap past its drain, a shutdown), this
    // future is dropped before the cancel below runs; the guard cancels the
    // token then, so the refresher sends its stop and ends (D-072 M-2).
    let _typing_stop_on_abort = typing_cancellation.as_ref().map(|token| token.clone().drop_guard());

    // ── Tool event forwarding: structured logging plus user-visible progress ──
    //
    // Nothing ends a turn on a clock any more, so a turn that legitimately runs
    // for an hour used to look, from the user's side, exactly like one that had
    // died. These notifications already existed and were only logged; feeding
    // them into the draft turns them into the presence signal a long turn needs.
    // This reports, it never terminates: no branch here can end any work.
    let (tool_event_tx, mut tool_event_rx) =
        tokio::sync::mpsc::channel::<crate::agent::turn_contracts::ToolCallNotification>(32);
    let tool_event_channel_name = msg.channel.clone();
    let tool_event_sender_name = msg.sender.clone();
    let progress_draft = match (target_channel.as_ref(), draft_message_id.as_deref()) {
        (Some(channel), Some(draft_id)) => Some((
            Arc::clone(channel),
            msg.reply_target.clone(),
            draft_id.to_string(),
            Arc::clone(&draft_has_text),
        )),
        _ => None,
    };
    let progress_started_at = started_at;
    let tool_event_forwarder = tokio::spawn(async move {
        let mut iteration_seen = 0usize;
        while let Some(notif) = tool_event_rx.recv().await {
            // A progress note only ever lands in a draft that has no streamed
            // text in it yet, so it can never overwrite part of a reply.
            let progress = match &notif {
                crate::agent::turn_contracts::ToolCallNotification::Started { name, .. } => Some(
                    channel_progress_line(name, iteration_seen, progress_started_at.elapsed()),
                ),
                crate::agent::turn_contracts::ToolCallNotification::Progress { iteration, .. } => {
                    iteration_seen = *iteration;
                    Some(channel_progress_line("", *iteration, progress_started_at.elapsed()))
                }
                crate::agent::turn_contracts::ToolCallNotification::Finished { .. } => None,
            };
            if let (Some(line), Some((channel, reply_target, draft_id, has_text))) = (progress, progress_draft.as_ref())
            {
                if !has_text.load(std::sync::atomic::Ordering::Acquire) {
                    if let Err(e) = channel.update_draft(reply_target, draft_id, &line).await {
                        tracing::debug!("Draft progress update failed: {e}");
                    }
                }
            }
            match notif {
                crate::agent::turn_contracts::ToolCallNotification::Started { name, args_summary } => {
                    tracing::info!(
                        channel = %tool_event_channel_name,
                        sender = %tool_event_sender_name,
                        tool = %name,
                        args = %args_summary,
                        "Tool call started"
                    );
                }
                crate::agent::turn_contracts::ToolCallNotification::Finished {
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
                crate::agent::turn_contracts::ToolCallNotification::Progress { iteration } => {
                    tracing::info!(
                        channel = %tool_event_channel_name,
                        sender = %tool_event_sender_name,
                        iteration,
                        "Tool loop progress"
                    );
                }
            }
        }
    });

    type LoopResult = Result<
        (
            crate::agent::turn_contracts::ToolLoopOutcome,
            crate::agent::turn_contracts::ToolLoopTrace,
        ),
        anyhow::Error,
    >;
    enum LlmExecutionResult {
        Completed(Box<LoopResult>),
        Cancelled,
    }

    enum LlmFinalOutcome {
        Cancelled,
        Success {
            response: String,
            trace: crate::agent::turn_contracts::ToolLoopTrace,
        },
        /// Smart group-reply: the model chose to stay silent. No message is sent
        /// and no assistant turn is written to history.
        Silent {
            reason: String,
            trace: crate::agent::turn_contracts::ToolLoopTrace,
        },
        Error(anyhow::Error),
    }

    let scope_owner_id = runtime_envelope.resolved_owner_id();
    // Borrow the policy derived from this message's pinned ConfigGeneration for
    // the entire tool-call loop.
    let scope_ctx = ScopeContext {
        policy: security.as_ref(),
        sender: &msg.sender,
        channel: &msg.channel,
        chat_type: inferred_chat_type,
        chat_id: &msg.reply_target,
        session_key: Some(&runtime_envelope.session_key),
        owner_id: Some(&scope_owner_id),
        owner_isolated: runtime_envelope.owner_isolated,
        topic_id: runtime_envelope.topic_id.as_deref(),
        task_id: runtime_envelope.resolved_task_id(),
        source_message_event_id: runtime_envelope.source_message_event_id.as_deref(),
        config_generation_id: runtime_envelope.config_generation_id,
        config_source_revision: runtime_envelope.config_source_revision.as_deref(),
    };

    // D8-4: seed a turn-root spawn execution context so any sub-agent spawned
    // directly from this channel turn inherits parent_run_id = the per-turn
    // run_id (top-level lineage was previously None). spawn_depth starts at 0 and
    // is_turn_root keeps the first child's reported depth at 0 (a turn is not
    // itself a spawn). The session scope key mirrors the spawn scope convention
    // (channel:chat_id:sender) so children share the turn's session scope.
    let turn_spawn_session_scope_key = format!("{}:{}:{}", msg.channel, msg.reply_target, msg.sender);
    // The channel surface's hidden tool names travel with the context: a
    // sub-agent spawned from this turn answers the same conversation partner, so
    // it runs behind the same boundary instead of the full operator surface.
    let turn_spawn_ctx = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
        turn_run_id.clone(),
        turn_spawn_session_scope_key,
    )
    .with_channel_excluded_tools(message_runtime.tool_tiering.always_exclude.clone());
    let turn_message_send_ctx = target_channel.as_ref().map(|channel| {
        crate::tools::message_send::MessageSendExecutionContext::new(
            Some(msg.reply_target.clone()),
            Arc::clone(channel),
        )
    });

    // No wall clock wraps this turn. It runs for as long as it keeps
    // working; the only automatic end is the stall detector inside
    // `run_turn` (see `crate::agent::idle`), and the only
    // manual one is cancellation — a newer message or `prx tasks kill`.
    // `run_turn` owns provider overflow compaction and replay. This outer scope
    // owns descendant cleanup only, while the terminal guard remains outside
    // it. Forced drop must acknowledge descendant cleanup before the guard may
    // persist the parent outcome.
    let child_owner = crate::runtime::turn_cleanup::current_tracker();
    let (child_cleanup_tx, child_cleanup_rx) = tokio::sync::oneshot::channel();
    let channel_child_cleanup =
        match crate::runtime::turn_cleanup::CleanupScope::new(child_owner.as_ref(), CancellationToken::new()) {
            Ok(cleanup) => cleanup.with_acknowledgement(Some(child_cleanup_tx)),
            Err(error) => {
                tracing::warn!(%error, "channel child cleanup scope unavailable");
                return;
            }
        };
    let decision_turn = decision_turn.with_child_cleanup(child_cleanup_rx);
    let final_outcome = channel_child_cleanup.run(async { Ok({
        let llm_work = async {
            tokio::select! {
                () = cancellation_token.cancelled() => LlmExecutionResult::Cancelled,
                result = async {
                        let scoped_tool_loop = crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT.scope(
                            turn_spawn_ctx.clone(),
                            crate::agent::turn_engine::run_turn(
                                &mut history,
                                crate::agent::turn_request::TurnRequest {
                                    channel_name: msg.channel.as_str(),
                                    scope: Some(&scope_ctx),
                                    mode: crate::agent::turn_contracts::ChatMode::default(),
                                    silent: true,
                                    // expose_stay_silent: ONLY on smart group turns. DMs / non-smart
                                    // never see the tool, so they can never short-circuit to Silent.
                                    expose_stay_silent: smart_group,
                                    multimodal: &message_runtime.multimodal,
                                    schedule: crate::agent::turn_request::TurnSchedule {
                                        read_only_tool_concurrency_window: message_runtime.read_only_tool_concurrency_window,
                                        priority_scheduling_enabled: message_runtime.priority_scheduling_enabled,
                                        low_priority_tool_names: message_runtime.low_priority_tools.clone(),
                                    },
                                    compaction: Some(decision_turn.compaction()),
                                    decision: decision_turn.routing(),
                                },
                                crate::agent::turn_request::TurnServices {
                                    provider: active_provider.as_ref(),
                                    provider_name: route.provider.as_str(),
                                    model: route.model.as_str(),
                                    temperature: runtime_defaults.temperature,
                                    tools_registry: Arc::clone(&ctx.tools_registry),
                                    observer: ctx.observer.as_ref(),
                                    hooks: ctx.hooks.as_ref(),
                                    approval: None,
                                    approval_resolver: None,
                                    runtime_adapter: Some(decision_turn.adapter()),
                                    memory: crate::agent::turn_request::ToolLoopMemory::new(
                                            &ctx.memory,
                                            &ctx.workspace_dir,
                                            Some(DocumentIngestRuntime::from_scope(ctx.memory.clone(), &scope_ctx)),
                                        )
                                        .with_event_fabric(
                                            MemoryFabric::new(
                                                ctx.memory.clone(),
                                                ctx.workspace_dir.as_path().to_string_lossy().to_string(),
                                            )
                                            .with_event_recording(ctx.memory_event_recording),
                                        ),
                                },
                                crate::agent::turn_request::TurnDelivery {
                                    cancellation: Some(cancellation_token.clone()),
                                    on_delta: delta_tx.clone(),
                                    on_tool_call: Some(tool_event_tx.clone()),
                                },
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
                    } => LlmExecutionResult::Completed(Box::new(result)),
            }
        };
        let llm_result = if let Some(owner) = crate::runtime::turn_cleanup::current_tracker() {
            let result = match crate::runtime::turn_cleanup::CleanupScope::new(Some(&owner), CancellationToken::new()) {
                Ok(cleanup) => cleanup.run(async { Ok(llm_work.await) }).await,
                Err(error) => {
                    drop(llm_work);
                    Err(error.context(crate::runtime::turn_cleanup::CleanupIncomplete))
                }
            };
            result.unwrap_or_else(|error| LlmExecutionResult::Completed(Box::new(Err(error))))
        } else {
            llm_work.await
        };

        match llm_result {
            LlmExecutionResult::Cancelled => LlmFinalOutcome::Cancelled,
            LlmExecutionResult::Completed(result) => match *result {
                Ok((outcome, trace)) => match outcome {
                    crate::agent::turn_contracts::ToolLoopOutcome::Text(response)
                    | crate::agent::turn_contracts::ToolLoopOutcome::AwaitingApproval { reply: response } => {
                        LlmFinalOutcome::Success { response, trace }
                    }
                    crate::agent::turn_contracts::ToolLoopOutcome::Silent { reason } => {
                        LlmFinalOutcome::Silent { reason, trace }
                    }
                },
                Err(e) => {
                    if e.downcast_ref::<crate::runtime::turn_cleanup::CleanupIncomplete>()
                        .is_some()
                    {
                        LlmFinalOutcome::Error(e)
                    } else if crate::agent::turn_contracts::is_tool_loop_cancelled(&e)
                        || cancellation_token.is_cancelled()
                    {
                        LlmFinalOutcome::Cancelled
                    } else {
                        LlmFinalOutcome::Error(e)
                    }
                }
            },
        }
    }) }).await.unwrap_or_else(LlmFinalOutcome::Error);

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

    match final_outcome {
        LlmFinalOutcome::Cancelled => {
            if let Err(error) = decision_turn
                .fail(&anyhow::anyhow!("channel turn cancelled"), true)
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
        LlmFinalOutcome::Success { response, trace } => {
            let sanitized_response = sanitize_channel_response(&response, ctx.tools_registry.as_ref());
            let delivered_response = if sanitized_response.is_empty() && !response.trim().is_empty() {
                "I encountered malformed tool-call output and could not produce a safe reply. Please try again."
                    .to_string()
            } else {
                sanitized_response
            };

            let provider_outcome =
                crate::agent::terminal::provider_outcome_from_trace(&route_decision, provider_started_at, trace);
            let terminal_committed = match decision_turn
                .commit(crate::agent::terminal::TurnTerminalDraft {
                    decision: None,
                    terminal_id: turn_run_id.clone(),
                    scope: runtime_envelope.message_scope().with_sender("prx"),
                    status: crate::agent::terminal::TurnTerminalStatus::Completed,
                    history: Some(crate::agent::terminal::TurnHistoryProjection {
                        assistant_content: delivered_response.clone(),
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
                })
                .await
            {
                Ok(_) => true,
                Err(error) => {
                    tracing::warn!(error = %error, "Failed to commit shared channel terminal event");
                    false
                }
            };

            if !terminal_committed {
                return;
            }

            let _ = append_sender_turn(
                ctx.as_ref(),
                &config_generation,
                &history_key,
                &msg.channel,
                &msg.sender,
                Some(&msg.reply_target),
                ChatMessage::assistant(&delivered_response),
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
                    if let Err(e) = finalize_draft_recording_activity(
                        channel.as_ref(),
                        &msg.reply_target,
                        draft_id,
                        &delivered_response,
                    )
                    .await
                    {
                        tracing::warn!("Failed to finalize draft: {e}; sending as new message");
                        let _ = send_recording_activity(
                            channel.as_ref(),
                            &SendMessage::new(&delivered_response, &msg.reply_target).in_thread(msg.thread_ts.clone()),
                        )
                        .await;
                    }
                } else if let Err(e) = send_recording_activity(
                    channel.as_ref(),
                    &SendMessage::new(delivered_response, &msg.reply_target).in_thread(msg.thread_ts.clone()),
                )
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
            if let Err(error) = decision_turn
                .commit(crate::agent::terminal::TurnTerminalDraft {
                    decision: None,
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
                })
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
            if let Err(error) = decision_turn.fail(&e, false).await {
                tracing::warn!(error = %error, "Failed to commit shared failed channel terminal event");
                return;
            }
            // What the user is told is chosen from a closed set of fixed
            // sentences keyed on the failure class; the error's own text never
            // reaches the chat. That is what keeps a leaked filesystem path,
            // API key, internal hostname or stack out of an outbound message
            // while still saying something the reader can act on.
            let failure_kind = classify_turn_failure(&e);
            let error_id = turn_error_id(&turn_run_id);
            // The operator-facing side keeps the error verbatim, and now also
            // carries the code the user will quote plus the class that was
            // picked, so a reported id resolves to exactly one turn.
            eprintln!(
                "  ❌ LLM error after {}ms [error_id={error_id} class={}]: {e}",
                started_at.elapsed().as_millis(),
                failure_kind.log_label()
            );
            tracing::error!(
                error_id = %error_id,
                turn_run_id = %turn_run_id,
                failure_class = failure_kind.log_label(),
                channel = %msg.channel,
                error = ?e,
                "channel turn failed"
            );
            if let Some(channel) = target_channel.as_ref() {
                let error_text = turn_failure_notice(failure_kind, &error_id);
                if let Some(ref draft_id) = draft_message_id {
                    let _ =
                        finalize_draft_recording_activity(channel.as_ref(), &msg.reply_target, draft_id, &error_text)
                            .await;
                } else {
                    let _ = send_recording_activity(
                        channel.as_ref(),
                        &SendMessage::new(error_text.as_str(), &msg.reply_target).in_thread(msg.thread_ts.clone()),
                    )
                    .await;
                }
            }
        }
    }
}

async fn ensure_notice_before_channel_output(
    ctx: &ChannelRuntimeContext,
    config_generation: &crate::config::ConfigGeneration,
    msg: &traits::ChannelMessage,
    target_channel: Option<&Arc<dyn Channel>>,
) -> bool {
    let Some(channel) = target_channel else {
        return true;
    };
    if let Err(error) = crate::compliance::interaction_notice::ensure_interaction_notice(
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
        return false;
    }
    true
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

struct AdmittedSmartPreGate {
    outcome: pre_gate::PreGateOutcome,
    provider_outcome: Option<crate::llm::route_decision::ProviderExecutionOutcome>,
    started_at: chrono::DateTime<chrono::Utc>,
}

/// Run the smart-group classifier only after the source decision exists. A
/// heuristic may enter the main loop directly, but neither heuristic nor a
/// caller-owned provider can authorize silence.
async fn run_admitted_smart_pre_gate(
    ctx: &ChannelRuntimeContext,
    config_generation: &Arc<crate::config::ConfigGeneration>,
    decision_turn: &mut decision_turn::ChannelDecisionTurn,
    heuristic: pre_gate::Heuristic,
    recent_context: &[String],
    user_text: &str,
    cancellation: &CancellationToken,
) -> AdmittedSmartPreGate {
    let started_at = chrono::Utc::now();
    if heuristic == pre_gate::Heuristic::EnterLoop {
        return AdmittedSmartPreGate {
            outcome: pre_gate::PreGateOutcome::enter(pre_gate::PreGatePath::HeuristicEnter),
            provider_outcome: None,
            started_at,
        };
    }
    let classifier = match decision_turn.smart_group_classifier() {
        Ok(classifier) => classifier,
        Err(error) => {
            tracing::warn!(
                "admitted smart-group classifier unavailable; failing open: {}",
                providers::sanitize_api_error(&error.to_string())
            );
            return AdmittedSmartPreGate {
                outcome: pre_gate::PreGateOutcome::enter(pre_gate::PreGatePath::ClassifierFailOpen),
                provider_outcome: None,
                started_at,
            };
        }
    };
    let pending = match classifier.begin() {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(
                "admitted smart-group classifier observation refused; failing open: {}",
                providers::sanitize_api_error(&error.to_string())
            );
            return AdmittedSmartPreGate {
                outcome: pre_gate::PreGateOutcome::enter(pre_gate::PreGatePath::ClassifierFailOpen),
                provider_outcome: None,
                started_at,
            };
        }
    };
    let provider = classifier.provider();
    let model = classifier.model().to_owned();
    let accounting = classifier.accounting();
    let cfg = &config_generation.effective.smart_group;
    let result = pre_gate::classify_with_model(
        provider.as_ref(),
        &model,
        cfg,
        recent_context,
        user_text,
        &ctx.bot_names,
        accounting,
        cancellation,
    )
    .await;
    match result.call {
        pre_gate::ClassifierCall::Completed(trace) => {
            if let Err(error) = classifier.complete(pending, &trace) {
                tracing::warn!(
                    "admitted smart-group classifier settlement failed; failing open: {}",
                    providers::sanitize_api_error(&error.to_string())
                );
                return AdmittedSmartPreGate {
                    outcome: pre_gate::PreGateOutcome::enter(pre_gate::PreGatePath::ClassifierFailOpen),
                    provider_outcome: None,
                    started_at,
                };
            }
            let provider_outcome = classifier.provider_outcome(&trace, started_at);
            if !result.outcome.should_enter_loop()
                && let Err(error) = classifier.authorize_silence()
            {
                tracing::warn!(
                    "admitted smart-group classifier silence authorization failed; failing open: {}",
                    providers::sanitize_api_error(&error.to_string())
                );
                return AdmittedSmartPreGate {
                    outcome: pre_gate::PreGateOutcome::enter(pre_gate::PreGatePath::ClassifierFailOpen),
                    provider_outcome: None,
                    started_at,
                };
            }
            AdmittedSmartPreGate {
                outcome: result.outcome,
                provider_outcome: Some(provider_outcome),
                started_at,
            }
        }
        pre_gate::ClassifierCall::Failed => {
            classifier.fail(pending, false);
            AdmittedSmartPreGate {
                outcome: result.outcome,
                provider_outcome: None,
                started_at,
            }
        }
        pre_gate::ClassifierCall::Cancelled => {
            classifier.fail(pending, true);
            AdmittedSmartPreGate {
                outcome: result.outcome,
                provider_outcome: None,
                started_at,
            }
        }
    }
}

/// Fan every inbound channel message out to its own turn.
///
/// **There is deliberately no concurrency gate here, and nothing may
/// reintroduce one.** This loop used to acquire a semaphore permit — one pool
/// for user messages, a smaller one for `system:` senders — on the dispatch
/// task itself, before it could come back around to `rx.recv()`. That was
/// survivable only while a turn was guaranteed to end on a clock. Now that a
/// turn can legitimately run for hours and nothing expires on its own, a
/// handful of wedged turns would hold every permit forever, the loop would stop
/// receiving, the bounded inbound queue would fill, and every listener's
/// `tx.send(..).await` would park: one stuck conversation deafens the whole
/// process, for every channel and every user, until it is restarted. Spawning
/// unconditionally is what keeps a stuck turn a *local* failure.
///
/// Removing the gate does not remove backpressure, it moves it to where it can
/// be honest. The inbound channel is still bounded and every listener publishes
/// with `send(..).await`, so a producer that outruns the process is slowed down
/// rather than having its messages dropped. What changed is what a full queue
/// *means*: the loop no longer blocks on anything a turn holds, so it can only
/// fall behind if the machine genuinely cannot spawn tasks fast enough. Live
/// turns are the memory that grows, and every one of them is registered,
/// listable and killable — see [`process_channel_message`].
///
/// The old user/system split is not replaced by another mechanism because it
/// never delivered the property it claimed. Both semaphores gated the same
/// task, so an exhausted *system* pool stalled user messages exactly as hard as
/// an exhausted user pool; with no shared permits at all there is nothing left
/// for system traffic to starve user traffic out of. The distinction survives
/// where it does real work — [`is_system_message`] at the smart-group gate, and
/// the `system:` sender prefix that a turn carries into its registry label.
async fn run_message_dispatch_loop_with_cleanup(
    mut rx: tokio::sync::mpsc::Receiver<traits::ChannelMessage>,
    ctx: Arc<ChannelRuntimeContext>,
    shutdown: CancellationToken,
) -> Result<()> {
    let cleanup = crate::runtime::turn_cleanup::current_tracker()
        .unwrap_or_else(|| Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default()));
    let mut workers = tokio::task::JoinSet::new();
    let in_flight_by_sender = Arc::new(Mutex::new(HashMap::<String, InFlightSenderTaskState>::new()));
    let task_sequence = Arc::new(AtomicU64::new(1));
    let mut health_heartbeat = tokio::time::interval(std::time::Duration::from_mins(1));
    health_heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // D5/D9 step 5: observe the external shutdown token alongside inbound
        // messages so the dispatch loop stops accepting new work promptly on a
        // root cancel, rather than only when the channel closes.
        let msg = tokio::select! {
            () = shutdown.cancelled() => break,
            _ = health_heartbeat.tick() => {
                crate::health::mark_component_ok("channels");
                continue;
            }
            // Reap finished workers as they land instead of only when the
            // next message arrives, so a burst followed by silence does not
            // leave completed handles sitting in the set. Guarded because
            // `join_next` on an empty set resolves immediately.
            Some(result) = workers.join_next(), if !workers.is_empty() => {
                log_worker_join_result(result);
                continue;
            }
            maybe_msg = rx.recv() => match maybe_msg {
                Some(msg) => {
                    // The single point every channel's inbound traffic passes
                    // through, webhook-delivered messages included.
                    activity::record_inbound(&msg.channel);
                    msg
                }
                None => break,
            },
        };

        // Register interruptible turns in receive order, before spawning. If
        // registration happens inside independently scheduled tasks, an older
        // message can run second and incorrectly cancel the newer one.
        let interrupt_enabled = ctx.interrupt_on_new_message && msg.channel == "telegram";
        let sender_scope_key = interruption_scope_key(&msg);
        let cancellation_token = CancellationToken::new();
        let completion = Arc::new(InFlightTaskCompletion::new());
        let task_id = task_sequence.fetch_add(1, Ordering::Relaxed);
        let previous = if interrupt_enabled {
            in_flight_by_sender.lock().insert(
                sender_scope_key.clone(),
                InFlightSenderTaskState {
                    task_id,
                    cancellation: cancellation_token.clone(),
                    completion: Arc::clone(&completion),
                },
            )
        } else {
            None
        };

        let worker_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(Some(&cleanup), CancellationToken::new())?;
        let worker_ctx = Arc::clone(&ctx);
        let in_flight = Arc::clone(&in_flight_by_sender);
        workers.spawn(async move {
            let _completion_guard = InFlightTaskCompletionGuard(Arc::clone(&completion));

            let work = async {
                if let Some(previous) = previous {
                    tracing::info!(
                        channel = %msg.channel,
                        sender = %msg.sender,
                        "Interrupting previous in-flight request for sender"
                    );
                    previous.cancellation.cancel();
                    previous.completion.wait().await;
                }

                process_channel_message(worker_ctx, msg, cancellation_token).await;
                Ok(())
            };
            if let Err(error) = worker_cleanup.run(work).await {
                tracing::warn!(%error, "Channel worker cleanup did not complete");
            }

            if interrupt_enabled {
                let mut active = in_flight.lock();
                if active
                    .get(&sender_scope_key)
                    .is_some_and(|state| state.task_id == task_id)
                {
                    active.remove(&sender_scope_key);
                }
            }
        });

        while let Some(result) = workers.try_join_next() {
            log_worker_join_result(result);
        }
    }

    // Closing the input stream is graceful; explicit shutdown cancels active
    // scope owners. In neither case may a child drain be mistaken for completion.
    if !shutdown.is_cancelled() {
        while let Some(result) = workers.join_next().await {
            log_worker_join_result(result);
        }
    }
    let drained = cleanup
        .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(15))
        .await;
    if drained.is_err() {
        workers.abort_all();
    }
    while let Some(result) = workers.join_next().await {
        log_worker_join_result(result);
    }
    drained
}

#[cfg(test)]
async fn run_message_dispatch_loop(
    rx: tokio::sync::mpsc::Receiver<traits::ChannelMessage>,
    ctx: Arc<ChannelRuntimeContext>,
    shutdown: CancellationToken,
) {
    run_message_dispatch_loop_with_cleanup(rx, normalize_legacy_channel_fixture(ctx), shutdown)
        .await
        .expect("channel cleanup");
}

#[cfg(test)]
fn normalize_legacy_channel_fixture(mut ctx: Arc<ChannelRuntimeContext>) -> Arc<ChannelRuntimeContext> {
    let split_generation = !Arc::ptr_eq(&ctx.config.pin(), &ctx.config_generation);
    let injected_provider = !crate::providers::ProviderId::of(ctx.default_provider.as_str()).is_resolvable();
    if !split_generation && !injected_provider {
        let owned = Arc::make_mut(&mut ctx);
        if !matches!(owned.memory.name(), "sqlite" | "lucid" | "postgres" | "none") {
            recording_fixture::attach_ledger_only(owned);
        }
        return ctx;
    }
    let mut config = ctx.config.load_full().as_ref().clone();
    let fixture_provider_name = ctx.default_provider.as_str().to_owned();
    let provider_cache = ctx.provider_cache.lock().clone();
    let fixture_provider = provider_cache
        .get(&(
            ctx.config_generation.id.0,
            fixture_provider_name.clone(),
            ctx.model.as_str().to_owned(),
        ))
        .cloned()
        .or_else(|| {
            provider_cache
                .iter()
                .find(|((_, provider, _), _)| provider == &fixture_provider_name)
                .map(|(_, provider)| Arc::clone(provider))
        })
        .unwrap_or_else(|| Arc::clone(&ctx.provider));
    let fixture_label_for = |provider: &Arc<dyn Provider>| {
        let identity = Arc::as_ptr(provider).cast::<()>() as usize;
        format!("custom:http://127.0.0.1/prx-channel-fixture-{identity:x}")
    };
    let mut fixture_providers = HashMap::new();
    fixture_providers.insert(
        fixture_provider_name.clone(),
        (fixture_label_for(&fixture_provider), Arc::clone(&fixture_provider)),
    );
    for ((_, name, _), provider) in provider_cache {
        fixture_providers
            .entry(name)
            .or_insert_with(|| (fixture_label_for(&provider), provider));
    }
    for (label, provider) in fixture_providers.values() {
        crate::providers::test_fixture::register(label, Arc::clone(provider));
    }
    let fixture_label = fixture_providers
        .get(&fixture_provider_name)
        .map(|(label, _)| label.clone())
        .expect("legacy channel fixture default provider");
    config.workspace_dir = ctx.workspace_dir.as_ref().clone();
    config.config_path = config.workspace_dir.join("config.toml");
    if split_generation {
        config.default_model = Some(ctx.model.as_str().to_owned());
        config.default_temperature = ctx.temperature;
        config.api_key.clone_from(&ctx.api_key);
        config.api_url.clone_from(&ctx.api_url);
        config.reliability = ctx.reliability.as_ref().clone();
        config.autonomy.level = ctx.security.load_full().security.autonomy;
        config.multimodal = ctx.multimodal.clone();
        config.agent.read_only_tool_concurrency_window = ctx.read_only_tool_concurrency_window;
        config.agent.priority_scheduling_enabled = ctx.priority_scheduling_enabled;
        config.agent.low_priority_tools = ctx.low_priority_tools.clone();
        config.tool_tiering = ctx.tool_tiering.clone();
        config.memory.auto_save = ctx.auto_save_memory;
        config.memory.events.record_user_messages = ctx.memory_event_recording.record_user_messages;
        config.memory.events.record_assistant_messages = ctx.memory_event_recording.record_assistant_messages;
        config.memory.events.record_tool_events = ctx.memory_event_recording.record_tool_events;
    }
    config.default_provider = Some(fixture_label.clone());
    for route in &mut config.model_routes {
        if let Some((label, _)) = fixture_providers.get(&route.provider) {
            route.provider.clone_from(label);
        }
    }
    for route in &mut config.router.models {
        if let Some((label, _)) = fixture_providers.get(&route.provider) {
            route.provider.clone_from(label);
        }
    }
    for model in &mut config.providers.models {
        if let Some((label, _)) = fixture_providers.get(&model.provider) {
            model.provider.clone_from(label);
        }
    }
    let mut fixture_routes = vec![(
        fixture_label.clone(),
        config
            .default_model
            .clone()
            .unwrap_or_else(|| ctx.model.as_str().to_owned()),
        Arc::clone(&fixture_provider),
    )];
    for route in ctx.route_overrides.lock().values() {
        if let Some((label, provider)) = fixture_providers.get(&route.provider) {
            fixture_routes.push((label.clone(), route.model.clone(), Arc::clone(provider)));
        }
    }
    fixture_routes.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
    fixture_routes.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);
    for (label, model, provider) in fixture_routes {
        if !config.providers.models.iter().any(|entry| {
            entry.provider.eq_ignore_ascii_case(&label)
                && entry
                    .model
                    .as_deref()
                    .is_some_and(|declared| declared.eq_ignore_ascii_case(&model))
        }) {
            config.providers.models.push(
                toml::from_str(&format!(
                    "provider = {label:?}\nmodel = {model:?}\noperations = [\"chat\"]\nnative_tools = {}\nvision_input = {}\n",
                    !config.tool_tiering.always_include.is_empty()
                        || provider
                            .capabilities_for(&model, crate::providers::traits::ProviderRequestMode::NonStreaming)
                            .native_tool_calling,
                    provider
                        .capabilities_for(&model, crate::providers::traits::ProviderRequestMode::NonStreaming)
                        .vision
                ))
                .expect("legacy channel fixture model declaration"),
            );
        }
        config
            .cost
            .prices
            .entry(format!("{label}/{model}"))
            .or_insert(crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            });
    }
    for route in ctx.route_overrides.lock().values() {
        let provider = fixture_providers
            .get(&route.provider)
            .map_or(route.provider.as_str(), |(label, _)| label.as_str());
        config
            .cost
            .prices
            .entry(format!("{provider}/{}", route.model))
            .or_insert(crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            });
    }
    let owned = Arc::make_mut(&mut ctx);
    for route in owned.route_overrides.lock().values_mut() {
        if let Some((label, _)) = fixture_providers.get(&route.provider) {
            route.provider.clone_from(label);
        }
    }
    owned.default_provider = Arc::new(fixture_label);
    if !matches!(owned.memory.name(), "sqlite" | "lucid" | "postgres" | "none") {
        recording_fixture::attach_ledger_only(owned);
    }
    let shared = crate::config::new_shared(config);
    owned.config_generation = shared.pin();
    owned.config = shared;
    ctx
}

/// Load workspace identity files and build a system prompt.
///
/// Follows the `OpenClaw` framework structure by default:
/// 1. Task-mode guidance
/// 2. Safety — guardrail reminder
/// 3. Skills — lazy catalog metadata; instructions are read with `skill_read`
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
    skills: &[crate::skills::Skill],
    identity_config: Option<&crate::config::IdentityConfig>,
    bootstrap_max_chars: Option<usize>,
) -> String {
    build_system_prompt_with_mode(
        workspace_dir,
        model_name,
        skills,
        identity_config,
        bootstrap_max_chars,
        false,
    )
}

pub fn build_system_prompt_with_mode(
    workspace_dir: &std::path::Path,
    model_name: &str,
    skills: &[crate::skills::Skill],
    identity_config: Option<&crate::config::IdentityConfig>,
    bootstrap_max_chars: Option<usize>,
    native_tools: bool,
) -> String {
    let context = crate::agent::prompt::PromptContext {
        workspace_dir,
        model_name,
        skills,
        identity_config,
        bootstrap_max_chars,
        native_tools,
    };
    crate::agent::prompt::SystemPromptBuilder::with_defaults()
        .build(&context)
        .unwrap_or_else(|error| {
            tracing::error!(%error, "failed to build canonical system prompt");
            "You are OpenPRX, a fast and efficient AI assistant built in Rust. Be helpful, concise, and direct."
                .to_string()
        })
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
    match maybe_restart_managed_daemon_service().await {
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

async fn maybe_restart_managed_daemon_service() -> Result<bool> {
    if cfg!(target_os = "macos") {
        let home = crate::config::user_home_dir().context("Could not find home directory")?;
        let plist = home.join("Library").join("LaunchAgents").join("com.prx.daemon.plist");
        if !plist.exists() {
            return Ok(false);
        }

        let list_output = Command::new("launchctl")
            .arg("list")
            .kill_on_drop(true)
            .output()
            .await
            .context("Failed to query launchctl list")?;
        let listed = String::from_utf8_lossy(&list_output.stdout);
        if !listed.contains("com.prx.daemon") {
            return Ok(false);
        }

        let _ = Command::new("launchctl")
            .args(["stop", "com.prx.daemon"])
            .kill_on_drop(true)
            .output()
            .await;
        let start_output = Command::new("launchctl")
            .args(["start", "com.prx.daemon"])
            .kill_on_drop(true)
            .output()
            .await
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
            if let Ok(status_output) = Command::new("rc-service")
                .args(OPENRC_STATUS_ARGS)
                .kill_on_drop(true)
                .output()
                .await
            {
                // rc-service exits 0 if running, non-zero otherwise
                if status_output.status.success() {
                    let restart_output = Command::new("rc-service")
                        .args(OPENRC_RESTART_ARGS)
                        .kill_on_drop(true)
                        .output()
                        .await
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
        let home = crate::config::user_home_dir().context("Could not find home directory")?;
        let unit_path: PathBuf = home.join(".config").join("systemd").join("user").join("prx.service");
        if !unit_path.exists() {
            return Ok(false);
        }

        let active_output = Command::new("systemctl")
            .args(SYSTEMD_STATUS_ARGS)
            .kill_on_drop(true)
            .output()
            .await
            .context("Failed to query systemd service state")?;
        let state = String::from_utf8_lossy(&active_output.stdout);
        if !state.trim().eq_ignore_ascii_case("active") {
            return Ok(false);
        }

        let restart_output = Command::new("systemctl")
            .args(SYSTEMD_RESTART_ARGS)
            .kill_on_drop(true)
            .output()
            .await
            .context("Failed to restart systemd daemon service")?;
        if !restart_output.status.success() {
            let stderr = String::from_utf8_lossy(&restart_output.stderr);
            anyhow::bail!("systemctl restart failed: {}", stderr.trim());
        }

        return Ok(true);
    }

    Ok(false)
}

/// Return the configured state of every real-time channel supported by this build.
///
/// CLI diagnostics share this list so newly added channels cannot disappear from
/// one status command while remaining visible in another.
pub fn channel_configuration_statuses(config: &Config) -> Vec<(&'static str, bool)> {
    vec![
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
        ("Wacli", config.channels_config.wacli.is_some()),
        ("Linq", config.channels_config.linq.is_some()),
        ("Nextcloud Talk", config.channels_config.nextcloud_talk.is_some()),
        ("Email", config.channels_config.email.is_some()),
        ("IRC", config.channels_config.irc.is_some()),
        ("Lark", config.channels_config.lark.is_some()),
        ("DingTalk", config.channels_config.dingtalk.is_some()),
        ("QQ", config.channels_config.qq.is_some()),
    ]
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
            for (name, configured) in channel_configuration_statuses(config) {
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
                            .with_ws_url(wa.ws_url.clone())
                            .with_read_pool_size(wa.read_pool_size),
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

    if let Some(ref wc) = config.channels_config.wacli {
        channels.push((
            "Wacli",
            Arc::new(WacliChannel::new(wacli::WacliChannelConfig {
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
                self_chat_mode: wc.self_chat_mode,
                outbound_media_workspace_only: wc.outbound_media_workspace_only,
            })),
        ));
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

/// Resolve the channel runtime's default provider and model for chat before
/// anything is built, so a default that cannot chat stops start-up (an
/// existing id-only default keeps running, with the resolver's warning).
fn check_channel_startup_default(config: &Config) -> Result<()> {
    providers::selection::check(
        config,
        &resolved_default_provider(config),
        &resolved_default_model(config),
        &providers::catalog::Requirement::chat(),
        providers::catalog::UnknownPolicy::Invocation,
    )?;
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
    #[cfg(feature = "wasm-plugins")]
    let wasm_early_runtime = if provider_name.starts_with("wasm:")
        || memory::effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config))
            .starts_with("wasm:")
    {
        crate::plugins::init_plugin_runtime(&config.workspace_dir, None).await
    } else {
        None
    };
    let provider_runtime_options = providers::provider_runtime_options_from_config(&config);
    check_channel_startup_default(&config)?;
    let default_is_hint = resolved_default_model(&config).trim().starts_with("hint:");
    let (provider, turn_provider_name, model) = build_channel_startup_provider(&config, &provider_runtime_options)?;

    // Warm up the provider connection pool (TLS handshake, DNS, HTTP/2 setup)
    // so the first real message doesn't hit a cold-start timeout.
    if let Err(e) = provider.warmup().await {
        tracing::warn!("Provider warmup failed (non-fatal): {e}");
    }

    let observer: Arc<dyn Observer> = Arc::from(observability::create_observer(&config.observability));
    let hooks = Arc::new(HookManager::new(config.workspace_dir.clone()).with_config(Arc::clone(&shared_config)));
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
    let temperature = config.default_temperature;
    let mem: Arc<dyn Memory> = Arc::from(memory::create_memory_with_storage(
        &config.memory,
        Some(&config.storage.provider.config),
        &config.workspace_dir,
        memory::EmbeddingRootCredential::from_config(&config),
    )?);
    #[cfg(feature = "wasm-plugins")]
    let wasm_plugin_runtime = if let Some(runtime) = wasm_early_runtime {
        runtime
            .attach_memory(Arc::clone(&mem))
            .await
            .map_err(|error| anyhow::anyhow!("failed to attach WASM memory backend in channels: {error}"))?;
        Some(runtime)
    } else {
        crate::plugins::init_plugin_runtime(&config.workspace_dir, Some(Arc::clone(&mem))).await
    };
    #[cfg(feature = "wasm-plugins")]
    if let Some(runtime) = &wasm_plugin_runtime {
        hooks.set_plugin_runtime(Arc::clone(runtime)).await;
    }
    let conversation_histories = Arc::new(Mutex::new(
        load_persisted_histories(&config.workspace_dir, mem.as_ref()).await,
    ));
    let (composio_key, composio_entity_id) = if config.composio.configured() {
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
    #[allow(unused_mut)]
    let mut extensions = hooks.control_tool_arcs(Arc::clone(&security));
    #[cfg(feature = "wasm-plugins")]
    if let Some(runtime) = &wasm_plugin_runtime {
        extensions.extend(runtime.control_tool_arcs(Arc::clone(&security)));
    }
    let tool_build = tools::all_tools_with_runtime_ext_and_extensions(
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
        &config.agents_with_references(),
        config.api_key.as_deref(),
        &config,
        extensions,
    );
    let mcp_tool = tool_build.mcp_tool;
    let mut tools_list = tool_build.tools;

    // Skill metadata is published with the decision catalog; prompts are built
    // from each admitted selection. Startup performs no embedding selection.

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
                        .with_ws_url(wa.ws_url.clone())
                        .with_read_pool_size(wa.read_pool_size),
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
        let (_, max_image_size_mb) = config.multimodal.effective_limits();
        channels.push(Arc::new(
            WacliChannel::new(wacli::WacliChannelConfig {
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
                self_chat_mode: wc.self_chat_mode,
                outbound_media_workspace_only: wc.outbound_media_workspace_only,
            })
            .with_media_artifacts(
                crate::media::MediaArtifactOwner::for_workspace(&config.workspace_dir),
                max_image_size_mb.saturating_mul(1024 * 1024),
            ),
        ));
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

    // Per-turn routing registry, keyed by `Channel::name`: one map shared by
    // `message_send` (destination of an explicit `channel` argument) and
    // `sessions_spawn` (announce/kill routing). Sharing it is what guarantees the
    // two tools agree on exactly which channels exist.
    let channels_registry = Arc::new(
        channels
            .iter()
            .map(|ch| (ch.name().to_string(), Arc::clone(ch)))
            .collect::<HashMap<_, _>>(),
    );
    let mut provider_cache_seed: HashMap<ProviderCacheKey, Arc<dyn Provider>> = HashMap::new();
    if !default_is_hint {
        provider_cache_seed.insert(
            (config_generation.id.0, provider_name.clone(), model.clone()),
            Arc::clone(&provider),
        );
    }
    let provider_cache: ProviderCacheMap = Arc::new(Mutex::new(provider_cache_seed));
    let pinned_provider_resolver: Arc<dyn providers::generation::PinnedProviderResolver> =
        Arc::new(providers::generation::ConfigGenerationProviderResolver::with_cache(
            Arc::clone(&shared_config),
            provider_runtime_options.clone(),
            Arc::clone(&provider_cache),
        ));
    // Publish the same registry process-wide for as long as these channels are
    // running, so the gateway control plane (`POST /api/channels/{name}/send`)
    // can reach the very channel objects that own the live connections. The
    // guard is held for the lifetime of this component: a reconfigured or
    // stopped channels supervisor withdraws its entries instead of leaving dead
    // channel objects addressable.
    let _outbound_publication = outbound_registry::publish(Arc::clone(&channels_registry));

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
        let msg_send_tool = tools::MessageSendTool::new_signal(sig_chan, security.clone())
            .with_channels(Arc::clone(&channels_registry));
        tools_list.push(Box::new(msg_send_tool));
    } else if let Some(first_channel) = channels.first().cloned() {
        let msg_send_tool =
            tools::MessageSendTool::new(first_channel, security.clone()).with_channels(Arc::clone(&channels_registry));
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
        let spawn_channels_by_name = Arc::clone(&channels_registry);
        let spawn_tool = tools::SessionsSpawnTool::new(
            first_channel,
            Arc::clone(&provider),
            &provider_name,
            &resolved_default_model(&config),
            temperature,
            security.clone(),
            config.workspace_dir.clone(),
            config.multimodal.clone(),
            config.agent.compaction.clone(),
            config.agents_with_references().into_owned(),
            config.api_key.clone(),
            provider_runtime_options.clone(),
            config.sessions_spawn.clone(),
        )
        .with_compaction_resolver(crate::router::CompactionResolver::new(
            config.agent.compaction.clone(),
            config.router.clone(),
            config.model_routes.clone(),
        ))
        .with_reliability(config.reliability.clone())
        .with_configured_providers(&providers::ConfiguredProviders::from_config(&config))
        .with_model_catalog(providers::selection::catalog_for(&config))
        .with_hint_config(&config)
        .with_pinned_provider_resolver(Arc::clone(&pinned_provider_resolver))
        .with_channels(spawn_channels_by_name)
        .with_shared_memory(Arc::clone(&mem));
        let spawn_tool = spawn_tool
            .with_event_recording(config.memory.event_recording_config())
            .with_process_recovery(config.causal_tree.enabled)?;
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
            tools::SessionStatusTool::new(active_runs, &turn_provider_name, &model, channel_names.clone())
                .with_shared_memory(Arc::clone(&mem), workspace_id),
        ));
        // gateway: daemon management (OpenClaw alignment)
        tools_list.push(Box::new(
            tools::GatewayTool::new(Arc::clone(&shared_config), &turn_provider_name, &model, channel_names)
                .with_security(security.clone()),
        ));

        // image: vision tool backed by the active provider (OpenClaw alignment)
        tools_list.push(Box::new(
            tools::ImageTool::new(
                Arc::clone(&provider),
                &model,
                temperature,
                security.clone(),
                config.multimodal.clone(),
            )
            .with_pinned_provider_resolver(Arc::clone(&pinned_provider_resolver), &turn_provider_name),
        ));

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
    #[cfg(feature = "wasm-plugins")]
    if let Some(runtime) = &wasm_plugin_runtime {
        tracing::debug!(
            generation = runtime.generation_id(),
            "shared WASM plugin runtime ready in channels"
        );
    }

    // Wrap the tool list in Arc now that all channel-aware tools have been appended.
    let tools_registry = Arc::new(tools_list);

    // Inject the tools registry into sessions_spawn so sub-agents can use tools.
    if let Some(handle) = spawn_tools_handle {
        let _ = handle.set(&tools_registry);
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
    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(CHANNEL_INBOUND_QUEUE_CAPACITY);

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

    let channels_by_name = Arc::clone(&channels_registry);
    // No cap: a wedged turn must not be able to stop the dispatch loop from
    // receiving. `prx tasks list` is where in-flight turns are counted now.
    println!("  🚦 In-flight messages: unbounded (inspect with `prx tasks list`)");

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

    let plugin_generation = {
        #[cfg(feature = "wasm-plugins")]
        {
            wasm_plugin_runtime
                .as_ref()
                .map_or(0, |runtime| runtime.generation_id())
        }
        #[cfg(not(feature = "wasm-plugins"))]
        {
            0
        }
    };
    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::new(mcp_tool, plugin_generation)),
        config: shared_config,
        #[cfg(test)]
        config_generation,
        channels_by_name,
        #[cfg(test)]
        provider: Arc::clone(&provider),
        #[cfg(test)]
        default_provider: Arc::new(provider_name),
        memory: Arc::clone(&mem),
        tools_registry: Arc::clone(&tools_registry),
        observer,
        hooks,
        #[cfg(test)]
        model: Arc::new(model.clone()),
        #[cfg(test)]
        temperature,
        auto_save_memory: config.memory.auto_save && config.memory.semantic.auto_promote_user_messages,
        memory_event_recording: config.memory.event_recording_config(),
        #[cfg(test)]
        read_only_tool_concurrency_window: config.agent.read_only_tool_concurrency_window,
        #[cfg(test)]
        priority_scheduling_enabled: config.agent.priority_scheduling_enabled,
        #[cfg(test)]
        low_priority_tools: config.agent.low_priority_tools.clone(),
        conversation_histories,
        provider_cache,
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        #[cfg(test)]
        api_key: config.api_key.clone(),
        #[cfg(test)]
        api_url: config.api_url.clone(),
        #[cfg(test)]
        reliability: Arc::new(config.reliability.clone()),
        provider_runtime_options,
        workspace_dir: Arc::new(config.workspace_dir.clone()),
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
        tool_tiering: config.tool_tiering.clone(),
        signal_inbound_policy,
        whatsapp_inbound_policy,
        bot_names,
        bot_uuids,
        mention_only_by_channel,
        group_reply_mode_by_channel,
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        #[cfg(test)]
        test_inbound_authorizer: None,
    });
    #[cfg(not(test))]
    drop(provider);

    let dispatch_result = run_message_dispatch_loop_with_cleanup(rx, runtime_ctx, shutdown.clone()).await;
    if dispatch_result.is_err() {
        // A failed cleanup closes this component's admission and listeners;
        // otherwise waiting for live listeners here could hide the error forever.
        shutdown.cancel();
    }

    // Wait for all channel tasks. On shutdown the supervised listeners break out
    // of their loops (ListenerOutcome::Shutdown above), so these joins complete
    // instead of hanging on a still-running `listen()`.
    for h in handles {
        let _ = h.await;
    }

    dispatch_result
}

#[cfg(test)]
mod tests;

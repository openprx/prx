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

mod dispatch;
mod doctor;
mod history;
mod inbound_policy;
mod prompt_context;
mod response_sanitize;
mod runtime_commands;
mod service;
mod signal_vision;
mod smart_gate;
mod startup;
mod supervisor;
mod turn;
mod turn_failure;

pub use self::doctor::*;
use self::history::append_sender_turn;
use self::inbound_policy::{ConversationKey, authorize_channel_outbound, chat_kind_from_message};
pub use self::prompt_context::*;
pub(crate) use self::response_sanitize::*;
#[cfg(test)]
pub(crate) use self::runtime_commands::*;

pub use self::startup::*;

use self::turn::ensure_notice_before_channel_output;
#[cfg(test)]
pub(crate) use self::turn_failure::*;

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

#[cfg(test)]
mod tests;

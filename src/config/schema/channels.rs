use super::{Context, Deserialize, DmPolicy, GroupPolicy, GroupReplyMode, JsonSchema, Result, Serialize};

// ── Channels ─────────────────────────────────────────────────────

/// Top-level channel configurations (`[channels_config]` section).
///
/// Each channel sub-section (e.g. `telegram`, `discord`) is optional;
/// setting it to `Some(...)` enables that channel.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChannelsConfig {
    /// Enable the CLI interactive channel. Default: `true`.
    pub cli: bool,
    /// Telegram bot channel configuration.
    pub telegram: Option<TelegramConfig>,
    /// Discord bot channel configuration.
    pub discord: Option<DiscordConfig>,
    /// Slack bot channel configuration.
    pub slack: Option<SlackConfig>,
    /// Mattermost bot channel configuration.
    pub mattermost: Option<MattermostConfig>,
    /// Webhook channel configuration.
    pub webhook: Option<WebhookConfig>,
    /// iMessage channel configuration (macOS only).
    pub imessage: Option<IMessageConfig>,
    /// Matrix channel configuration.
    pub matrix: Option<MatrixConfig>,
    /// Signal channel configuration.
    pub signal: Option<SignalConfig>,
    /// WhatsApp channel configuration (Cloud API or Web mode).
    pub whatsapp: Option<WhatsAppConfig>,
    /// wacli JSON-RPC daemon channel configuration.
    #[serde(default)]
    pub wacli: Option<WacliConfig>,
    /// Linq Partner API channel configuration.
    pub linq: Option<LinqConfig>,
    /// Nextcloud Talk bot channel configuration.
    pub nextcloud_talk: Option<NextcloudTalkConfig>,
    /// Email channel configuration.
    pub email: Option<crate::channels::email_channel::EmailConfig>,
    /// IRC channel configuration.
    pub irc: Option<IrcConfig>,
    /// Lark/Feishu channel configuration.
    pub lark: Option<LarkConfig>,
    /// DingTalk channel configuration.
    pub dingtalk: Option<DingTalkConfig>,
    /// QQ Official Bot channel configuration.
    pub qq: Option<QQConfig>,
    /// Local ramflux agent-host bridge.
    #[serde(default)]
    pub ramflux: Option<super::RamfluxConfig>,
}

impl Default for ChannelsConfig {
    fn default() -> Self {
        Self {
            cli: true,
            telegram: None,
            discord: None,
            slack: None,
            mattermost: None,
            webhook: None,
            imessage: None,
            matrix: None,
            signal: None,
            whatsapp: None,
            wacli: None,
            linq: None,
            nextcloud_talk: None,
            email: None,
            irc: None,
            lark: None,
            dingtalk: None,
            qq: None,
            ramflux: None,
        }
    }
}

/// Streaming mode for channels that support progressive message updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StreamMode {
    /// No streaming -- send the complete response as a single message (default).
    #[default]
    Off,
    /// Update a draft message with every flush interval.
    Partial,
}

pub(super) const fn default_draft_update_interval_ms() -> u64 {
    1000
}

/// Telegram bot channel configuration.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct TelegramConfig {
    /// Telegram Bot API token (from @BotFather).
    pub bot_token: String,
    /// Allowed Telegram user IDs or usernames. Empty = deny all.
    pub allowed_users: Vec<String>,
    /// Streaming mode for progressive response delivery via message edits.
    #[serde(default)]
    pub stream_mode: StreamMode,
    /// Minimum interval (ms) between draft message edits to avoid rate limits.
    #[serde(default = "default_draft_update_interval_ms")]
    pub draft_update_interval_ms: u64,
    /// When true, a newer Telegram message from the same sender in the same chat
    /// cancels the in-flight request and starts a fresh response with preserved history.
    #[serde(default)]
    pub interrupt_on_new_message: bool,
    /// When true, only respond to messages that @-mention the bot in groups.
    /// Direct messages are always processed.
    #[serde(default)]
    pub mention_only: bool,
    /// Group reply decision mode. When `None` (default), derived from
    /// `mention_only` for backward compatibility (`true => MentionOnly`,
    /// `false => Off`). Set to `smart` to let the model decide per-message
    /// whether to reply or stay silent in groups.
    #[serde(default)]
    pub group_reply_mode: Option<GroupReplyMode>,
}

/// Discord bot channel configuration.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct DiscordConfig {
    /// Discord bot token (from Discord Developer Portal).
    pub bot_token: String,
    /// Optional guild (server) ID to restrict the bot to a single guild.
    pub guild_id: Option<String>,
    /// Allowed Discord user IDs. Empty = deny all.
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// When true, process messages from other bots (not just humans).
    /// The bot still ignores its own messages to prevent feedback loops.
    #[serde(default)]
    pub listen_to_bots: bool,
    /// When true, only respond to messages that @-mention the bot.
    /// Other messages in the guild are silently ignored.
    #[serde(default)]
    pub mention_only: bool,
    /// Group reply decision mode. When `None` (default), derived from
    /// `mention_only` for backward compatibility (`true => MentionOnly`,
    /// `false => Off`). Set to `smart` to let the model decide per-message
    /// whether to reply or stay silent in guild channels.
    #[serde(default)]
    pub group_reply_mode: Option<GroupReplyMode>,
}

/// Slack bot channel configuration.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct SlackConfig {
    /// Slack bot OAuth token (xoxb-...).
    pub bot_token: String,
    /// Slack app-level token for Socket Mode (xapp-...).
    pub app_token: Option<String>,
    /// Optional channel ID to restrict the bot to a single channel.
    pub channel_id: Option<String>,
    /// Allowed Slack user IDs. Empty = deny all.
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

/// Mattermost bot channel configuration.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct MattermostConfig {
    /// Mattermost server URL (e.g. `"https://mattermost.example.com"`).
    pub url: String,
    /// Mattermost bot access token.
    pub bot_token: String,
    /// Optional channel ID to restrict the bot to a single channel.
    pub channel_id: Option<String>,
    /// Allowed Mattermost user IDs. Empty = deny all.
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// When true (default), replies thread on the original post.
    /// When false, replies go to the channel root.
    #[serde(default)]
    pub thread_replies: Option<bool>,
    /// When true, only respond to messages that @-mention the bot.
    /// Other messages in the channel are silently ignored.
    #[serde(default)]
    pub mention_only: Option<bool>,
}

/// Webhook channel configuration.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebhookConfig {
    /// Port to listen on for incoming webhooks.
    pub port: u16,
    /// Optional shared secret for webhook signature verification.
    pub secret: Option<String>,
}

/// iMessage channel configuration (macOS only).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IMessageConfig {
    /// Allowed iMessage contacts (phone numbers or email addresses). Empty = deny all.
    pub allowed_contacts: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

/// Matrix channel configuration.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct MatrixConfig {
    /// Matrix homeserver URL (e.g. `"https://matrix.org"`).
    pub homeserver: String,
    /// Matrix access token for the bot account.
    pub access_token: String,
    /// Optional Matrix user ID (e.g. `"@bot:matrix.org"`).
    #[serde(default)]
    pub user_id: Option<String>,
    /// Optional Matrix device ID.
    #[serde(default)]
    pub device_id: Option<String>,
    /// Matrix room ID to listen in (e.g. `"!abc123:matrix.org"`).
    pub room_id: String,
    /// Allowed Matrix user IDs. Empty = deny all.
    pub allowed_users: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SignalStormProtectionConfig {
    /// Deduplication TTL in seconds for `(channel + sender + event_type + normalized_content)`.
    #[serde(default = "default_signal_storm_dedupe_ttl_secs")]
    pub dedupe_ttl_secs: u64,
    /// Minimum interval in seconds between replies for the same `(channel + reply_target)`.
    #[serde(default = "default_signal_storm_min_reply_interval_secs")]
    pub min_reply_interval_secs: u64,
    /// Non-user event threshold within `abnormal_window_secs` before tripping the breaker.
    #[serde(default = "default_signal_storm_abnormal_threshold")]
    pub abnormal_threshold: usize,
    /// Sliding window in seconds for non-user event counting.
    #[serde(default = "default_signal_storm_abnormal_window_secs")]
    pub abnormal_window_secs: u64,
    /// Circuit breaker duration in seconds after threshold is reached.
    #[serde(default = "default_signal_storm_breaker_duration_secs")]
    pub breaker_duration_secs: u64,
}

pub(super) const fn default_signal_storm_dedupe_ttl_secs() -> u64 {
    60
}

pub(super) const fn default_signal_storm_min_reply_interval_secs() -> u64 {
    2
}

pub(super) const fn default_signal_storm_abnormal_threshold() -> usize {
    10
}

pub(super) const fn default_signal_storm_abnormal_window_secs() -> u64 {
    60
}

pub(super) const fn default_signal_storm_breaker_duration_secs() -> u64 {
    300
}

impl Default for SignalStormProtectionConfig {
    fn default() -> Self {
        Self {
            dedupe_ttl_secs: default_signal_storm_dedupe_ttl_secs(),
            min_reply_interval_secs: default_signal_storm_min_reply_interval_secs(),
            abnormal_threshold: default_signal_storm_abnormal_threshold(),
            abnormal_window_secs: default_signal_storm_abnormal_window_secs(),
            breaker_duration_secs: default_signal_storm_breaker_duration_secs(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct SignalConfig {
    /// Base URL for the signal-cli HTTP daemon (e.g. "http://127.0.0.1:16866").
    /// In "native" mode this is ignored; the daemon is spawned on `daemon_http_port`.
    #[serde(default = "default_signal_http_url")]
    pub http_url: String,
    /// E.164 phone number of the signal-cli account (e.g. "+1234567890").
    pub account: String,
    /// Channel mode: "native" (spawn signal-cli daemon locally) or "rest" (external daemon/Docker).
    /// Default: "rest" for backward compatibility.
    #[serde(default)]
    pub mode: Option<String>,
    /// Path to signal-cli binary. Only used in native mode. Default: "signal-cli" (PATH lookup).
    #[serde(default)]
    pub cli_path: Option<String>,
    /// signal-cli data directory. Only used in native mode.
    /// Default: $HOME/.local/share/signal-cli (signal-cli's standard XDG location).
    #[serde(default)]
    pub data_dir: Option<String>,
    /// Local HTTP port for the spawned signal-cli daemon. Only used in native mode. Default: 16866.
    #[serde(default)]
    pub daemon_http_port: Option<u16>,
    /// Deprecated: ignored. Accepted for backward compatibility with older config files.
    #[serde(default)]
    pub daemon_mode: Option<bool>,
    /// Optional group ID to filter messages.
    /// - `None` or omitted: accept all messages (DMs and groups)
    /// - `"dm"`: only accept direct messages
    /// - Specific group ID: only accept messages from that group
    #[serde(default)]
    pub group_id: Option<String>,
    /// Allowed sender phone numbers (E.164) or "*" for all.
    #[serde(default)]
    pub allowed_from: Vec<String>,
    /// Skip messages that are attachment-only (no text body).
    #[serde(default)]
    pub ignore_attachments: bool,
    /// Skip incoming story messages.
    #[serde(default)]
    pub ignore_stories: bool,
    /// Signal ingress storm protection controls.
    #[serde(default)]
    pub storm_protection: SignalStormProtectionConfig,
    /// Startup readiness timeout in milliseconds for native `signal-cli` daemon mode.
    #[serde(default = "default_signal_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
    /// Direct-message policy.
    #[serde(default)]
    pub dm_policy: DmPolicy,
    /// Group-message policy.
    #[serde(default)]
    pub group_policy: GroupPolicy,
    /// Allowlisted group IDs used when `group_policy = "allowlist"`.
    #[serde(default)]
    pub group_allow_from: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

pub(super) fn default_signal_http_url() -> String {
    "http://localhost:8080".to_string()
}

pub(super) const fn default_signal_startup_timeout_ms() -> u64 {
    30_000
}

impl Default for SignalConfig {
    fn default() -> Self {
        Self {
            http_url: default_signal_http_url(),
            account: String::new(),
            mode: None,
            cli_path: None,
            data_dir: None,
            daemon_http_port: None,
            daemon_mode: None,
            group_id: None,
            allowed_from: Vec::new(),
            ignore_attachments: false,
            ignore_stories: false,
            storm_protection: SignalStormProtectionConfig::default(),
            startup_timeout_ms: default_signal_startup_timeout_ms(),
            dm_policy: DmPolicy::default(),
            group_policy: GroupPolicy::default(),
            group_allow_from: Vec::new(),
            mention_only: false,
        }
    }
}

impl SignalConfig {
    /// Returns the effective HTTP URL for the signal-cli daemon.
    /// In native mode, uses the local daemon port (ignoring `http_url`).
    /// In rest mode, returns `http_url` as-is.
    pub fn effective_http_url(&self) -> String {
        if self.mode.as_deref() == Some("native") {
            let port = self.daemon_http_port.unwrap_or(16866);
            format!("http://127.0.0.1:{port}")
        } else {
            self.http_url.clone()
        }
    }

    /// Returns true if native mode is configured.
    pub fn is_native_mode(&self) -> bool {
        self.mode.as_deref() == Some("native")
    }
}

/// WhatsApp channel configuration (Cloud API or Web mode).
///
/// Set `phone_number_id` for Cloud API mode, or `session_path` for Web mode.
#[derive(Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct WhatsAppConfig {
    /// Access token from Meta Business Suite (Cloud API mode)
    #[serde(default)]
    pub access_token: Option<String>,
    /// Phone number ID from Meta Business API (Cloud API mode)
    #[serde(default)]
    pub phone_number_id: Option<String>,
    /// Webhook verify token (you define this, Meta sends it back for verification)
    /// Only used in Cloud API mode
    #[serde(default)]
    pub verify_token: Option<String>,
    /// App secret from Meta Business Suite (for webhook signature verification)
    /// Can also be set via `OPENPRX_WHATSAPP_APP_SECRET` (legacy: `OPENPRX_WHATSAPP_APP_SECRET`) environment variable
    /// Only used in Cloud API mode
    #[serde(default)]
    pub app_secret: Option<String>,
    /// Session database path for WhatsApp Web client (Web mode)
    /// When set, enables native WhatsApp Web mode with wa-rs
    #[serde(default)]
    pub session_path: Option<String>,
    /// Phone number for pair code linking (Web mode, optional)
    /// Format: country code + number (e.g., "15551234567")
    /// If not set, QR code pairing will be used
    #[serde(default)]
    pub pair_phone: Option<String>,
    /// Custom pair code for linking (Web mode, optional)
    /// Leave empty to let WhatsApp generate one
    #[serde(default)]
    pub pair_code: Option<String>,
    /// Allowed phone numbers (E.164 format: +1234567890) or "*" for all
    #[serde(default, alias = "allowed_from")]
    pub allowed_numbers: Vec<String>,
    /// Direct-message policy.
    #[serde(default)]
    pub dm_policy: DmPolicy,
    /// Group-message policy.
    #[serde(default)]
    pub group_policy: GroupPolicy,
    /// Allowlisted group IDs used when `group_policy = "allowlist"`.
    #[serde(default)]
    pub group_allow_from: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
    /// Group reply decision mode. When `None` (default), derived from
    /// `mention_only` for backward compatibility (`true => MentionOnly`,
    /// `false => Off`). Set to `smart` to let the model decide per-message
    /// whether to reply or stay silent in WhatsApp group chats (`@g.us`).
    #[serde(default)]
    pub group_reply_mode: Option<GroupReplyMode>,

    /// WebSocket URL override (replaces WHATSAPP_WS_URL)
    #[serde(default)]
    pub ws_url: Option<String>,

    /// Concurrent read connections against the Web-mode session database.
    ///
    /// The session store is SQLite in WAL mode: readers run in parallel while
    /// writes stay serialised, so this sizes the read side only. When unset the
    /// size is derived from the available CPUs. Must be greater than 0 when
    /// set; a saturated pool makes callers wait, it never fails them.
    #[serde(default)]
    pub read_pool_size: Option<usize>,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct LinqConfig {
    /// Linq Partner API token (Bearer auth)
    pub api_token: String,
    /// Phone number to send from (E.164 format)
    pub from_phone: String,
    /// Webhook signing secret for signature verification
    #[serde(default)]
    pub signing_secret: Option<String>,
    /// Allowed sender handles (phone numbers) or "*" for all
    #[serde(default)]
    pub allowed_senders: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

/// Nextcloud Talk bot configuration (webhook receive + OCS send API).
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct NextcloudTalkConfig {
    /// Nextcloud base URL (e.g. "https://cloud.example.com").
    pub base_url: String,
    /// Bot app token used for OCS API bearer auth.
    pub app_token: String,
    /// Shared secret for webhook signature verification.
    ///
    /// Can also be set via `OPENPRX_NEXTCLOUD_TALK_WEBHOOK_SECRET` (legacy: `OPENPRX_NEXTCLOUD_TALK_WEBHOOK_SECRET`).
    #[serde(default)]
    pub webhook_secret: Option<String>,
    /// Allowed Nextcloud actor IDs (`[]` = deny all, `"*"` = allow all).
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

impl WhatsAppConfig {
    /// Detect which backend to use based on config fields.
    /// Returns "cloud" if phone_number_id is set, "web" if session_path is set.
    pub const fn backend_type(&self) -> &'static str {
        if self.phone_number_id.is_some() {
            "cloud"
        } else if self.session_path.is_some() {
            "web"
        } else {
            // Default to Cloud API for backward compatibility
            "cloud"
        }
    }

    /// Check if this is a valid Cloud API config
    pub const fn is_cloud_config(&self) -> bool {
        self.phone_number_id.is_some() && self.access_token.is_some() && self.verify_token.is_some()
    }

    /// Check if this is a valid Web config
    pub const fn is_web_config(&self) -> bool {
        self.session_path.is_some()
    }

    /// Returns true when both Cloud and Web selectors are present.
    ///
    /// Runtime currently prefers Cloud mode in this case for backward compatibility.
    pub const fn is_ambiguous_config(&self) -> bool {
        self.phone_number_id.is_some() && self.session_path.is_some()
    }
}

/// wacli (official) channel configuration.
///
/// Connect OpenPRX to WhatsApp via the **official** `steipete/wacli` real-time
/// interface: inbound via `sync --webhook` (signed HTTP POST) and outbound via
/// the `wacli send` CLI. The legacy self-maintained JSON-RPC daemon (`host` /
/// `port`) is no longer supported.
///
/// Deployment (run alongside PRX, e.g. a systemd user service):
/// ```text
/// wacli sync --follow \
///   --webhook http://127.0.0.1:16868/wacli \
///   --webhook-secret <SECRET> \
///   --store ~/.wacli
/// ```
///
/// Example TOML:
/// ```toml
/// [channels_config.wacli]
/// webhook_listen = "127.0.0.1:16868"
/// webhook_path = "/wacli"
/// webhook_secret = "<SECRET>"     # must match --webhook-secret
/// store_dir = "/prx_workspace/.wacli"
/// bot_jid = "1234567890@s.whatsapp.net"
/// bot_number = "1234567890"
/// self_chat_mode = true
/// allowed_from = ["*"]
/// group_reply_mode = "smart"
/// ```
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct WacliConfig {
    /// Address the inbound webhook server binds to (default: "127.0.0.1:16868").
    #[serde(default = "default_wacli_webhook_listen")]
    pub webhook_listen: String,
    /// HTTP path the webhook server serves (default: "/wacli").
    #[serde(default = "default_wacli_webhook_path")]
    pub webhook_path: String,
    /// HMAC-SHA256 secret shared with `wacli sync --webhook-secret`. Required
    /// unless `allow_unsigned_loopback` is set.
    #[serde(default)]
    pub webhook_secret: Option<String>,
    /// Allow processing unsigned requests when bound to a loopback address.
    /// Defaults to false (signature verification required).
    #[serde(default)]
    pub allow_unsigned_loopback: bool,
    /// Sender JID allowlist. Use `["*"]` to accept all senders.
    /// JIDs look like `1234567890@s.whatsapp.net` for individuals or
    /// `1234567890-1234567890@g.us` for groups.
    #[serde(default = "default_wacli_allowed_from")]
    pub allowed_from: Vec<String>,
    /// Path to the `wacli` binary used for outbound sends. When `None`, `wacli`
    /// is resolved from `PATH`.
    #[serde(default)]
    pub cli_path: Option<String>,
    /// wacli store directory passed as `--store` on outbound sends and used as
    /// a read-only chat-title fallback for older webhook payloads.
    #[serde(default)]
    pub store_dir: Option<String>,
    /// Bot's own JID, used for reply-to-bot mention detection.
    #[serde(default)]
    pub bot_jid: Option<String>,
    /// Bot's own phone number (digits), used for `@<number>` mention detection.
    #[serde(default)]
    pub bot_number: Option<String>,
    /// The bot's own WhatsApp LID, used for @mention detection inside groups.
    /// Accepts either the bare digits `263767598346470` or the full
    /// `263767598346470@lid` form; a device suffix such as `:3` is stripped.
    #[serde(default)]
    pub bot_lid: Option<String>,
    /// Treat messages authored by the linked account in WhatsApp's Message
    /// Yourself chat as owner input. Disabled by default. When enabled, PRX
    /// invokes wacli's experimental self-send path and suppresses reply echoes
    /// by their outbound message IDs.
    #[serde(default)]
    pub self_chat_mode: bool,
    /// Confine **outbound** media markers (`[IMAGE:…]` and friends) to the
    /// workspace again.
    ///
    /// Defaults to `false`: a marker may name any path the daemon can read,
    /// because agents attach files from the operator's own working directories.
    /// Set it to `true` to restore the workspace-confined, `O_NOFOLLOW` loader,
    /// after which a marker pointing outside the workspace degrades to literal
    /// text. It changes nothing else: the SSRF policy for `http(s)` sources, the
    /// per-category size caps, byte-derived typing, the private temp copy handed
    /// to wacli, and the `INFO` audit log all apply in both modes.
    #[serde(default)]
    pub outbound_media_workspace_only: bool,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
    /// Smart group-reply mode. When `None`, derived from `mention_only`.
    #[serde(default)]
    pub group_reply_mode: Option<GroupReplyMode>,
    /// REMOVED: legacy JSON-RPC daemon host. Retained only to detect and reject
    /// stale configs with a clear migration error in [`WacliConfig::validate`].
    #[serde(default, skip_serializing)]
    pub host: Option<String>,
    /// REMOVED: legacy JSON-RPC daemon port. See `host`.
    #[serde(default, skip_serializing)]
    pub port: Option<u16>,
}

pub(super) fn default_wacli_webhook_listen() -> String {
    "127.0.0.1:16868".to_string()
}

pub(super) fn default_wacli_webhook_path() -> String {
    "/wacli".to_string()
}

pub(super) fn default_wacli_allowed_from() -> Vec<String> {
    vec!["*".to_string()]
}

impl Default for WacliConfig {
    fn default() -> Self {
        Self {
            webhook_listen: default_wacli_webhook_listen(),
            webhook_path: default_wacli_webhook_path(),
            webhook_secret: None,
            allow_unsigned_loopback: false,
            allowed_from: default_wacli_allowed_from(),
            cli_path: None,
            store_dir: None,
            bot_jid: None,
            bot_number: None,
            bot_lid: None,
            self_chat_mode: false,
            outbound_media_workspace_only: false,
            mention_only: false,
            group_reply_mode: None,
            host: None,
            port: None,
        }
    }
}

impl WacliConfig {
    /// Validate wacli channel settings (only meaningful when `enabled`).
    ///
    /// Detects removed JSON-RPC fields, enforces secure-by-default HMAC, and
    /// warns when mention/smart modes lack the bot identity needed for `@`
    /// detection.
    pub fn validate(&self) -> Result<()> {
        // Reject stale JSON-RPC daemon configs regardless of `enabled` so the
        // operator gets a clear migration error instead of silent no-op.
        if self.host.is_some() || self.port.is_some() {
            anyhow::bail!(
                "channels_config.wacli: the wacli daemon JSON-RPC config (host/port) is no \
                 longer supported; use webhook_listen/webhook_path/webhook_secret with the \
                 official `wacli sync --webhook` interface"
            );
        }

        // Address must be parseable so the listener can bind.
        self.webhook_listen
            .parse::<std::net::SocketAddr>()
            .with_context(|| format!("invalid channels_config.wacli.webhook_listen: {}", self.webhook_listen))?;

        // webhook_path must be non-empty, start with '/', and contain no axum
        // capture/wildcard syntax ({}, *, or /:param segments).
        let path = self.webhook_path.trim();
        if path.is_empty() {
            anyhow::bail!("channels_config.wacli.webhook_path must not be empty");
        }
        if !path.starts_with('/') {
            anyhow::bail!("channels_config.wacli.webhook_path must start with '/' (got {path:?})");
        }
        if path.contains('{') || path.contains('}') || path.contains('*') {
            anyhow::bail!(
                "channels_config.wacli.webhook_path must not contain axum capture/wildcard syntax \
                 ('{{', '}}', '*') (got {path:?})"
            );
        }
        // Reject path segments that start with ':' (e.g. "/:foo" axum named params).
        if path.split('/').any(|seg| seg.starts_with(':')) {
            anyhow::bail!(
                "channels_config.wacli.webhook_path must not contain ':param' segments \
                 (got {path:?})"
            );
        }

        // Secure-by-default: a secret is mandatory unless unsigned-loopback is
        // explicitly opted into.
        let has_secret = self
            .webhook_secret
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
        if !has_secret && !self.allow_unsigned_loopback {
            anyhow::bail!(
                "channels_config.wacli.webhook_secret is required when enabled \
                 (set allow_unsigned_loopback = true only for loopback testing)"
            );
        }
        if self.allow_unsigned_loopback && !has_secret {
            let bind: std::net::SocketAddr = self
                .webhook_listen
                .parse()
                .with_context(|| format!("invalid channels_config.wacli.webhook_listen: {}", self.webhook_listen))?;
            if !bind.ip().is_loopback() {
                anyhow::bail!(
                    "channels_config.wacli.allow_unsigned_loopback = true requires a loopback \
                     webhook_listen address (got {bind})"
                );
            }
        }

        // Outbound media path policy is a deliberate deployment choice, so it
        // is stated at startup instead of being silently implied by a default.
        if self.outbound_media_workspace_only {
            tracing::info!(
                "channels_config.wacli: outbound media is confined to the workspace \
                 (outbound_media_workspace_only = true)"
            );
        } else {
            tracing::info!(
                "channels_config.wacli: outbound media may reference any readable path \
                 (set outbound_media_workspace_only = true to confine it to the workspace)"
            );
        }

        // mention / smart modes need a bot identity to detect `@`s. The official
        // payload has no mentionedJids, so without bot_jid/bot_number every
        // mention would be missed. Warn rather than hard-fail (smart still works
        // via the LLM gate; mentions just degrade).
        let smart = matches!(self.group_reply_mode, Some(GroupReplyMode::Smart));
        let has_identity = self.bot_jid.as_deref().map(str::trim).is_some_and(|s| !s.is_empty())
            || self.bot_number.as_deref().map(str::trim).is_some_and(|s| !s.is_empty());
        if self.self_chat_mode && !has_identity {
            anyhow::bail!(
                "channels_config.wacli.self_chat_mode requires bot_jid or bot_number so PRX can distinguish Message Yourself from ordinary outgoing chats"
            );
        }
        if (self.mention_only || smart) && !has_identity {
            tracing::warn!(
                "channels_config.wacli: mention_only/smart enabled but neither bot_jid nor \
                 bot_number is set; @-mention detection will not work (set bot_jid)"
            );
        }

        Ok(())
    }
}

/// IRC channel configuration.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct IrcConfig {
    /// IRC server hostname
    pub server: String,
    /// IRC server port (default: 6697 for TLS)
    #[serde(default = "default_irc_port")]
    pub port: u16,
    /// Bot nickname
    pub nickname: String,
    /// Username (defaults to nickname if not set)
    pub username: Option<String>,
    /// Channels to join on connect
    #[serde(default)]
    pub channels: Vec<String>,
    /// Allowed nicknames (case-insensitive) or "*" for all
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// When true, only process channel/group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
    /// Server password (for bouncers like ZNC)
    pub server_password: Option<String>,
    /// NickServ IDENTIFY password
    pub nickserv_password: Option<String>,
    /// SASL PLAIN password (IRCv3)
    pub sasl_password: Option<String>,
    /// Verify TLS certificate (default: true)
    pub verify_tls: Option<bool>,
}

pub(super) const fn default_irc_port() -> u16 {
    6697
}

/// How OpenPRX receives events from Feishu / Lark.
///
/// - `websocket` (default) — persistent WSS long-connection; no public URL required.
/// - `webhook`             — HTTP callback server; requires a public HTTPS endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LarkReceiveMode {
    #[default]
    Websocket,
    Webhook,
}

/// Lark/Feishu configuration for messaging integration.
/// Lark is the international version; Feishu is the Chinese version.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct LarkConfig {
    /// App ID from Lark/Feishu developer console
    pub app_id: String,
    /// App Secret from Lark/Feishu developer console
    pub app_secret: String,
    /// Encrypt key for webhook message decryption (optional)
    #[serde(default)]
    pub encrypt_key: Option<String>,
    /// Verification token for webhook validation (optional)
    #[serde(default)]
    pub verification_token: Option<String>,
    /// Allowed user IDs or union IDs (empty = deny all, "*" = allow all)
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// Whether to use the Feishu (Chinese) endpoint instead of Lark (International)
    #[serde(default)]
    pub use_feishu: bool,
    /// Event receive mode: "websocket" (default) or "webhook"
    #[serde(default)]
    pub receive_mode: LarkReceiveMode,
    /// HTTP port for webhook mode only. Must be set when receive_mode = "webhook".
    /// Not required (and ignored) for websocket mode.
    #[serde(default)]
    pub port: Option<u16>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

impl LarkConfig {
    /// Webhook mode authenticates every event by `verification_token`, so a
    /// webhook-mode channel without one is refused at load, not only when its
    /// listener starts (LARK, D-060 L-4). WebSocket mode does not use it.
    pub fn validate(&self) -> Result<()> {
        if self.receive_mode == LarkReceiveMode::Webhook {
            anyhow::ensure!(
                self.verification_token
                    .as_deref()
                    .is_some_and(|token| !token.trim().is_empty()),
                "channels_config.lark.verification_token must be set when channels_config.lark.receive_mode = \
                 \"webhook\": without it every event would be accepted"
            );
        }
        Ok(())
    }
}

/// DingTalk configuration for Stream Mode messaging
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct DingTalkConfig {
    /// Client ID (AppKey) from DingTalk developer console
    pub client_id: String,
    /// Client Secret (AppSecret) from DingTalk developer console
    pub client_secret: String,
    /// Allowed user IDs (staff IDs). Empty = deny all, "*" = allow all
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

/// QQ Official Bot configuration (Tencent QQ Bot SDK)
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct QQConfig {
    /// App ID from QQ Bot developer console
    pub app_id: String,
    /// App Secret from QQ Bot developer console
    pub app_secret: String,
    /// Allowed user IDs. Empty = deny all, "*" = allow all
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

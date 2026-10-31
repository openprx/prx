//! `[channels_config.ramflux]` — local bridge to a ramflux agent-host.
//!
//! The socket is a same-user 0600 Unix socket. prx does not hold ramflux keys.
//! `authorization_mode = "full"` does not by itself grant full autonomy; the
//! agent-host must also declare `authorization = "full"` in `bridge.hello`.

use super::{Deserialize, JsonSchema, Result, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

const fn default_max_in_flight() -> u8 {
    1
}

const fn default_turn_wall_clock_secs() -> u64 {
    120
}

const fn default_max_blocks() -> u32 {
    32
}

const fn default_max_actions() -> u32 {
    16
}

const fn default_max_bytes() -> u32 {
    65_536
}

const fn default_attachment_max_bytes() -> u64 {
    5 * 1024 * 1024
}

const fn default_attachment_max_per_turn() -> u8 {
    4
}

const fn default_encrypt_at_rest() -> bool {
    true
}

fn default_block_types() -> Vec<String> {
    [
        "Text",
        "Markdown",
        "Heading",
        "KeyValue",
        "Metric",
        "List",
        "TableFallback",
        "Form",
        "Input",
        "Select",
        "Progress",
        "LogStream",
        "Warning",
        "Error",
        "Success",
        "Approval",
        "ActionGroup",
        "FilePreview",
        "Fallback",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// How prx combines its own setting with `bridge.hello.authorization`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum RamfluxAuthorizationMode {
    /// Use the agent-host value. Until hello arrives, the effective mode is supervised.
    #[default]
    FollowRf,
    /// Always supervised, even when the agent-host declares full.
    Supervised,
    /// Full only when the agent-host also declares full. Config alone never yields full.
    Full,
}

impl RamfluxAuthorizationMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FollowRf => "follow_rf",
            Self::Supervised => "supervised",
            Self::Full => "full",
        }
    }
}

/// What to do with a new inbound message while a turn for that conversation is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum RamfluxOnNewMessage {
    #[default]
    Queue,
    Interrupt,
}

/// Limits for `reply.surface` blocks and actions.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RamfluxSurfaceLimits {
    #[serde(default = "default_max_blocks")]
    pub max_blocks: u32,
    #[serde(default = "default_max_actions")]
    pub max_actions: u32,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u32,
}

impl Default for RamfluxSurfaceLimits {
    fn default() -> Self {
        Self {
            max_blocks: default_max_blocks(),
            max_actions: default_max_actions(),
            max_bytes: default_max_bytes(),
        }
    }
}

/// Per-turn caps for `attachment.fetch`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RamfluxAttachmentFetch {
    #[serde(default = "default_attachment_max_bytes")]
    pub max_bytes: u64,
    #[serde(default = "default_attachment_max_per_turn")]
    pub max_per_turn: u8,
}

impl Default for RamfluxAttachmentFetch {
    fn default() -> Self {
        Self {
            max_bytes: default_attachment_max_bytes(),
            max_per_turn: default_attachment_max_per_turn(),
        }
    }
}

/// Persona for one ramflux AI account. Missing fields fall back to the global agent.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct RamfluxAccountConfig {
    /// Path to a system-prompt file. Relative paths are resolved from the workspace.
    #[serde(default)]
    pub system_prompt_file: Option<PathBuf>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub scope_profile: Option<String>,
    /// Cross-conversation memory retrieval. Off unless the owner sets it.
    #[serde(default)]
    pub cross_conversation_retrieval: bool,
}

/// Local ramflux agent-host bridge (`[channels_config.ramflux]`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RamfluxConfig {
    /// Unix socket path. `None` uses `$XDG_RUNTIME_DIR/prx/ramflux.sock`, or
    /// `<workspace>/ramflux.sock` when that variable is unset.
    #[serde(default)]
    pub socket_path: Option<PathBuf>,
    #[serde(default)]
    pub authorization_mode: RamfluxAuthorizationMode,
    #[serde(default = "default_max_in_flight")]
    pub max_in_flight_per_conversation: u8,
    #[serde(default)]
    pub on_new_message: RamfluxOnNewMessage,
    /// Advertised turn wall clock, in seconds. The agent-host enforces it with `turn.cancel`.
    #[serde(default = "default_turn_wall_clock_secs")]
    pub turn_wall_clock_secs: u64,
    #[serde(default)]
    pub surface_limits: RamfluxSurfaceLimits,
    #[serde(default = "default_block_types")]
    pub allowed_block_types: Vec<String>,
    #[serde(default)]
    pub attachment_fetch: RamfluxAttachmentFetch,
    #[serde(default = "default_encrypt_at_rest")]
    pub encrypt_at_rest: bool,
    #[serde(default)]
    pub accounts: BTreeMap<String, RamfluxAccountConfig>,
}

impl Default for RamfluxConfig {
    fn default() -> Self {
        Self {
            socket_path: None,
            authorization_mode: RamfluxAuthorizationMode::FollowRf,
            max_in_flight_per_conversation: default_max_in_flight(),
            on_new_message: RamfluxOnNewMessage::Queue,
            turn_wall_clock_secs: default_turn_wall_clock_secs(),
            surface_limits: RamfluxSurfaceLimits::default(),
            allowed_block_types: default_block_types(),
            attachment_fetch: RamfluxAttachmentFetch::default(),
            encrypt_at_rest: default_encrypt_at_rest(),
            accounts: BTreeMap::new(),
        }
    }
}

impl RamfluxConfig {
    /// Reject limits and account ids that cannot be enforced.
    ///
    /// # Errors
    ///
    /// Returns an error when a limit is zero, the block allow-list is empty,
    /// or an account id is empty or contains `/`.
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.max_in_flight_per_conversation >= 1,
            "channels_config.ramflux.max_in_flight_per_conversation must be at least 1"
        );
        anyhow::ensure!(
            self.turn_wall_clock_secs >= 1,
            "channels_config.ramflux.turn_wall_clock_secs must be at least 1"
        );
        anyhow::ensure!(
            self.surface_limits.max_blocks >= 1
                && self.surface_limits.max_actions >= 1
                && self.surface_limits.max_bytes >= 1,
            "channels_config.ramflux.surface_limits values must be at least 1"
        );
        anyhow::ensure!(
            self.attachment_fetch.max_bytes >= 1 && self.attachment_fetch.max_per_turn >= 1,
            "channels_config.ramflux.attachment_fetch limits must be at least 1"
        );
        anyhow::ensure!(
            !self.allowed_block_types.is_empty(),
            "channels_config.ramflux.allowed_block_types must not be empty"
        );
        if let Some(path) = &self.socket_path {
            anyhow::ensure!(
                !path.as_os_str().is_empty(),
                "channels_config.ramflux.socket_path must not be empty"
            );
        }
        for account_id in self.accounts.keys() {
            anyhow::ensure!(
                !account_id.is_empty() && !account_id.contains('/') && !account_id.contains('\\'),
                "channels_config.ramflux.accounts key must be a non-empty account id without a path separator"
            );
        }
        Ok(())
    }

    /// Resolve the socket path from config, `$XDG_RUNTIME_DIR`, or the workspace.
    #[must_use]
    pub fn resolve_socket_path(&self, workspace_dir: &std::path::Path) -> PathBuf {
        if let Some(path) = &self.socket_path {
            return path.clone();
        }
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            if !runtime.is_empty() {
                return PathBuf::from(runtime).join("prx").join("ramflux.sock");
            }
        }
        workspace_dir.join("ramflux.sock")
    }
}

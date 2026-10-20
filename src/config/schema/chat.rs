use super::{Deserialize, JsonSchema, Result, Serialize};

/// Interactive chat configuration (`[chat]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChatConfig {
    /// Maximum concurrent visible provider turns in the TUI Redux driver.
    #[serde(default = "default_chat_max_concurrent_visible_turns")]
    pub max_concurrent_visible_turns: usize,
    /// How this chat process reaches the daemon it borrows channels from.
    #[serde(default)]
    pub daemon: ChatDaemonConfig,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            max_concurrent_visible_turns: default_chat_max_concurrent_visible_turns(),
            daemon: ChatDaemonConfig::default(),
        }
    }
}

impl ChatConfig {
    pub fn validate(&self) -> Result<()> {
        self.daemon.validate()
    }
}

pub(super) const fn default_chat_max_concurrent_visible_turns() -> usize {
    2
}

/// Where `prx chat` sends its outbound messages (`[chat.daemon]`).
///
/// A chat session opens no IM connection of its own — that would race the
/// daemon for inbound messages on the same account — so `message_send` in chat
/// asks the daemon to deliver. That is an ordinary authenticated call to the
/// daemon's control plane, so it needs the same address and token `prx tasks`
/// uses.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChatDaemonConfig {
    /// Base URL of the daemon gateway. Empty (the default) derives it from
    /// `[gateway] host`/`port`, which is right whenever both run on this host.
    #[serde(default)]
    pub url: String,
    /// Bearer token for the daemon control plane. Empty sends no credential,
    /// which only works when the daemon runs with `gateway.require_pairing =
    /// false`; otherwise the daemon answers 401 and the send is refused.
    #[serde(default)]
    pub token: String,
}

impl ChatDaemonConfig {
    pub fn validate(&self) -> Result<()> {
        let url = self.url.trim();
        if !url.is_empty() {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                anyhow::bail!("chat.daemon.url must start with http:// or https:// (got {url:?})");
            }
            // A scheme with nothing after it would resolve to a request against
            // an empty host, which fails much later and much less clearly.
            let authority = url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/');
            if authority.is_empty() {
                anyhow::bail!("chat.daemon.url must name a host (got {url:?})");
            }
        }
        if self.token.trim() != self.token {
            anyhow::bail!("chat.daemon.token must not have leading or trailing whitespace");
        }
        if self.token.chars().any(char::is_control) {
            anyhow::bail!("chat.daemon.token must not contain control characters");
        }
        Ok(())
    }
}

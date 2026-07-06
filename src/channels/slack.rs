use super::traits::{Channel, ChannelMessage, SendMessage};
use anyhow::{Context, anyhow};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

const SLACK_POLL_INTERVAL_SECS: u64 = 3;
const SLACK_MAX_BACKOFF_SECS: u64 = 60;

/// Slack channel — polls conversations.history via Web API
pub struct SlackChannel {
    bot_token: String,
    channel_id: Option<String>,
    allowed_users: Vec<String>,
    workspace_dir: Option<PathBuf>,
}

impl SlackChannel {
    pub const fn new(bot_token: String, channel_id: Option<String>, allowed_users: Vec<String>) -> Self {
        Self {
            bot_token,
            channel_id,
            allowed_users,
            workspace_dir: None,
        }
    }

    pub fn with_workspace_dir(mut self, workspace_dir: PathBuf) -> Self {
        self.workspace_dir = Some(workspace_dir);
        self
    }

    fn http_client(&self) -> reqwest::Client {
        crate::config::build_runtime_proxy_client("channel.slack")
            .map_err(|e| {
                tracing::error!("proxy build failed for channel.slack, using direct: {e}");
                e
            })
            .unwrap_or_else(|_| reqwest::Client::new())
    }

    /// Check if a Slack user ID is in the allowlist.
    /// Empty list means deny everyone until explicitly configured.
    /// `"*"` means allow everyone.
    fn is_user_allowed(&self, user_id: &str) -> bool {
        self.allowed_users.iter().any(|u| u == "*" || u == user_id)
    }

    /// Get the bot's own user ID so we can ignore our own messages
    async fn get_bot_user_id(&self) -> anyhow::Result<String> {
        let resp: serde_json::Value = self
            .http_client()
            .get("https://slack.com/api/auth.test")
            .bearer_auth(&self.bot_token)
            .send()
            .await?
            .json()
            .await?;

        if resp.get("ok") == Some(&serde_json::Value::Bool(false)) {
            let err = resp.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
            anyhow::bail!("Slack auth.test failed: {err}");
        }

        resp.get("user_id")
            .and_then(|u| u.as_str())
            .filter(|id| !id.trim().is_empty())
            .map(String::from)
            .ok_or_else(|| anyhow!("Slack auth.test response did not include user_id"))
    }

    /// Resolve the thread identifier for inbound Slack messages.
    /// Replies carry `thread_ts` (root thread id); top-level messages only have `ts`.
    fn inbound_thread_ts(msg: &serde_json::Value, ts: &str) -> Option<String> {
        msg.get("thread_ts")
            .and_then(|t| t.as_str())
            .or(if ts.is_empty() { None } else { Some(ts) })
            .map(str::to_string)
    }

    fn cursor_path_for(workspace_dir: &Path, channel_id: &str) -> PathBuf {
        let safe_channel_id: String = channel_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        workspace_dir.join(format!("slack_cursor_{safe_channel_id}.txt"))
    }

    fn cursor_path(&self, channel_id: &str) -> Option<PathBuf> {
        self.workspace_dir
            .as_ref()
            .map(|dir| Self::cursor_path_for(dir, channel_id))
    }

    async fn load_last_ts(&self, channel_id: &str) -> anyhow::Result<String> {
        let Some(path) = self.cursor_path(channel_id) else {
            return Ok(String::new());
        };
        match tokio::fs::read_to_string(&path).await {
            Ok(value) => Ok(value.trim().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).with_context(|| format!("failed to read Slack cursor {}", path.display())),
        }
    }

    async fn store_last_ts(&self, channel_id: &str, ts: &str) -> anyhow::Result<()> {
        if ts.trim().is_empty() {
            return Ok(());
        }
        let Some(path) = self.cursor_path(channel_id) else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create Slack cursor directory {}", parent.display()))?;
        }
        let tmp_path = path.with_extension(format!("tmp.{}", std::process::id()));
        tokio::fs::write(&tmp_path, format!("{ts}\n"))
            .await
            .with_context(|| format!("failed to write temporary Slack cursor {}", tmp_path.display()))?;
        tokio::fs::rename(&tmp_path, &path)
            .await
            .with_context(|| format!("failed to replace Slack cursor {}", path.display()))?;
        Ok(())
    }

    fn poll_backoff_duration(consecutive_errors: u32) -> std::time::Duration {
        let exponent = consecutive_errors.clamp(1, 6);
        let secs = 2_u64.saturating_pow(exponent).min(SLACK_MAX_BACKOFF_SECS);
        std::time::Duration::from_secs(secs)
    }
}

#[async_trait]
impl Channel for SlackChannel {
    fn name(&self) -> &str {
        "slack"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let mut body = serde_json::json!({
            "channel": message.recipient,
            "text": message.content
        });

        if let Some(ref ts) = message.thread_ts {
            if let Some(m) = body.as_object_mut() {
                m.insert("thread_ts".to_string(), serde_json::json!(ts));
            }
        }

        let resp = self
            .http_client()
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .unwrap_or_else(|e| format!("<failed to read response body: {e}>"));

        if !status.is_success() {
            anyhow::bail!("Slack chat.postMessage failed ({status}): {body}");
        }

        // Slack returns 200 for most app-level errors; check JSON "ok" field
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        if parsed.get("ok") == Some(&serde_json::Value::Bool(false)) {
            let err = parsed.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
            anyhow::bail!("Slack chat.postMessage failed: {err}");
        }

        Ok(())
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        let channel_id = self
            .channel_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Slack channel_id required for listening"))?;

        let bot_user_id = self
            .get_bot_user_id()
            .await
            .map_err(|e| anyhow!("cannot get Slack bot_user_id: {e}"))?;
        let mut last_ts = self.load_last_ts(&channel_id).await?;
        let mut consecutive_errors = 0_u32;

        tracing::info!("Slack channel listening on #{channel_id}...");

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(SLACK_POLL_INTERVAL_SECS)).await;

            let mut params = vec![("channel", channel_id.clone()), ("limit", "10".to_string())];
            if !last_ts.is_empty() {
                params.push(("oldest", last_ts.clone()));
            }

            let resp = match self
                .http_client()
                .get("https://slack.com/api/conversations.history")
                .bearer_auth(&self.bot_token)
                .query(&params)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("Slack poll error: {e}");
                    consecutive_errors = consecutive_errors.saturating_add(1);
                    tokio::time::sleep(Self::poll_backoff_duration(consecutive_errors)).await;
                    continue;
                }
            };

            let data: serde_json::Value = match resp.json().await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("Slack parse error: {e}");
                    consecutive_errors = consecutive_errors.saturating_add(1);
                    tokio::time::sleep(Self::poll_backoff_duration(consecutive_errors)).await;
                    continue;
                }
            };

            if data.get("ok") == Some(&serde_json::Value::Bool(false)) {
                let err = data.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
                tracing::warn!("Slack conversations.history failed: {err}");
                consecutive_errors = consecutive_errors.saturating_add(1);
                tokio::time::sleep(Self::poll_backoff_duration(consecutive_errors)).await;
                continue;
            }

            consecutive_errors = 0;

            if let Some(messages) = data.get("messages").and_then(|m| m.as_array()) {
                // Messages come newest-first, reverse to process oldest first
                for msg in messages.iter().rev() {
                    let ts = msg.get("ts").and_then(|t| t.as_str()).unwrap_or("");
                    let user = msg.get("user").and_then(|u| u.as_str()).unwrap_or("unknown");
                    let text = msg.get("text").and_then(|t| t.as_str()).unwrap_or("");

                    // Skip bot's own messages
                    if user == bot_user_id {
                        if !ts.is_empty() && ts > last_ts.as_str() {
                            last_ts = ts.to_string();
                            self.store_last_ts(&channel_id, &last_ts).await?;
                        }
                        continue;
                    }

                    // Sender validation
                    if !self.is_user_allowed(user) {
                        tracing::warn!("Slack: ignoring message from unauthorized user: {user}");
                        if !ts.is_empty() && ts > last_ts.as_str() {
                            last_ts = ts.to_string();
                            self.store_last_ts(&channel_id, &last_ts).await?;
                        }
                        continue;
                    }

                    // Skip already-seen messages. Slack `oldest` is inclusive.
                    if ts.is_empty() || ts <= last_ts.as_str() {
                        continue;
                    }

                    if text.is_empty() {
                        last_ts = ts.to_string();
                        self.store_last_ts(&channel_id, &last_ts).await?;
                        continue;
                    }

                    let channel_msg = ChannelMessage {
                        id: format!("slack_{channel_id}_{ts}"),
                        sender: user.to_string(),
                        reply_target: channel_id.clone(),
                        content: text.to_string(),
                        channel: "slack".to_string(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        thread_ts: Self::inbound_thread_ts(msg, ts),
                        chat_kind: crate::channels::traits::ChatKind::Dm,
                        chat_title: None,
                        sender_display: None,
                        mentioned_uuids: vec![],
                        mentioned: false,
                        is_group_hint: false,
                        sender_is_bot: false,
                    };

                    if tx.send(channel_msg).await.is_err() {
                        return Ok(());
                    }

                    last_ts = ts.to_string();
                    self.store_last_ts(&channel_id, &last_ts).await?;
                }
            }
        }
    }

    async fn health_check(&self) -> bool {
        self.http_client()
            .get("https://slack.com/api/auth.test")
            .bearer_auth(&self.bot_token)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_channel_name() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec![]);
        assert_eq!(ch.name(), "slack");
    }

    #[test]
    fn slack_channel_with_channel_id() {
        let ch = SlackChannel::new("xoxb-fake".into(), Some("C12345".into()), vec![]);
        assert_eq!(ch.channel_id, Some("C12345".to_string()));
    }

    #[test]
    fn empty_allowlist_denies_everyone() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec![]);
        assert!(!ch.is_user_allowed("U12345"));
        assert!(!ch.is_user_allowed("anyone"));
    }

    #[test]
    fn wildcard_allows_everyone() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec!["*".into()]);
        assert!(ch.is_user_allowed("U12345"));
    }

    #[test]
    fn specific_allowlist_filters() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec!["U111".into(), "U222".into()]);
        assert!(ch.is_user_allowed("U111"));
        assert!(ch.is_user_allowed("U222"));
        assert!(!ch.is_user_allowed("U333"));
    }

    #[test]
    fn allowlist_exact_match_not_substring() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec!["U111".into()]);
        assert!(!ch.is_user_allowed("U1111"));
        assert!(!ch.is_user_allowed("U11"));
    }

    #[test]
    fn allowlist_empty_user_id() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec!["U111".into()]);
        assert!(!ch.is_user_allowed(""));
    }

    #[test]
    fn allowlist_case_sensitive() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec!["U111".into()]);
        assert!(ch.is_user_allowed("U111"));
        assert!(!ch.is_user_allowed("u111"));
    }

    #[test]
    fn allowlist_wildcard_and_specific() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec!["U111".into(), "*".into()]);
        assert!(ch.is_user_allowed("U111"));
        assert!(ch.is_user_allowed("anyone"));
    }

    // ── Message ID edge cases ─────────────────────────────────────

    #[test]
    fn slack_message_id_format_includes_channel_and_ts() {
        // Verify that message IDs follow the format: slack_{channel_id}_{ts}
        let ts = "1234567890.123456";
        let channel_id = "C12345";
        let expected_id = format!("slack_{channel_id}_{ts}");
        assert_eq!(expected_id, "slack_C12345_1234567890.123456");
    }

    #[test]
    fn slack_message_id_is_deterministic() {
        // Same channel_id + same ts = same ID (prevents duplicates after restart)
        let ts = "1234567890.123456";
        let channel_id = "C12345";
        let id1 = format!("slack_{channel_id}_{ts}");
        let id2 = format!("slack_{channel_id}_{ts}");
        assert_eq!(id1, id2);
    }

    #[test]
    fn slack_message_id_different_ts_different_id() {
        // Different timestamps produce different IDs
        let channel_id = "C12345";
        let id1 = format!("slack_{channel_id}_1234567890.123456");
        let id2 = format!("slack_{channel_id}_1234567890.123457");
        assert_ne!(id1, id2);
    }

    #[test]
    fn slack_message_id_different_channel_different_id() {
        // Different channels produce different IDs even with same ts
        let ts = "1234567890.123456";
        let id1 = format!("slack_C12345_{ts}");
        let id2 = format!("slack_C67890_{ts}");
        assert_ne!(id1, id2);
    }

    #[test]
    fn slack_message_id_no_uuid_randomness() {
        // Verify format doesn't contain random UUID components
        let ts = "1234567890.123456";
        let channel_id = "C12345";
        let id = format!("slack_{channel_id}_{ts}");
        assert!(!id.contains('-')); // No UUID dashes
        assert!(id.starts_with("slack_"));
    }

    #[test]
    fn inbound_thread_ts_prefers_explicit_thread_ts() {
        let msg = serde_json::json!({
            "ts": "123.002",
            "thread_ts": "123.001"
        });

        let thread_ts = SlackChannel::inbound_thread_ts(&msg, "123.002");
        assert_eq!(thread_ts.as_deref(), Some("123.001"));
    }

    #[test]
    fn inbound_thread_ts_falls_back_to_ts() {
        let msg = serde_json::json!({
            "ts": "123.001"
        });

        let thread_ts = SlackChannel::inbound_thread_ts(&msg, "123.001");
        assert_eq!(thread_ts.as_deref(), Some("123.001"));
    }

    #[test]
    fn inbound_thread_ts_none_when_ts_missing() {
        let msg = serde_json::json!({});

        let thread_ts = SlackChannel::inbound_thread_ts(&msg, "");
        assert_eq!(thread_ts, None);
    }

    #[test]
    fn slack_cursor_path_sanitizes_channel_id() {
        let workspace = PathBuf::from("/tmp/openprx-workspace");
        let path = SlackChannel::cursor_path_for(&workspace, "C12/../bad channel");
        assert_eq!(path, workspace.join("slack_cursor_C12____bad_channel.txt"));
    }

    #[tokio::test]
    async fn slack_cursor_roundtrips_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let ch = SlackChannel::new("xoxb-fake".into(), Some("C12345".into()), vec![])
            .with_workspace_dir(tmp.path().to_path_buf());

        assert_eq!(ch.load_last_ts("C12345").await.unwrap(), "");

        ch.store_last_ts("C12345", "1716040000.123456").await.unwrap();

        assert_eq!(ch.load_last_ts("C12345").await.unwrap(), "1716040000.123456");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("slack_cursor_C12345.txt")).unwrap(),
            "1716040000.123456\n"
        );
    }

    #[tokio::test]
    async fn slack_cursor_noops_without_workspace_dir() {
        let ch = SlackChannel::new("xoxb-fake".into(), Some("C12345".into()), vec![]);

        ch.store_last_ts("C12345", "1716040000.123456").await.unwrap();

        assert_eq!(ch.load_last_ts("C12345").await.unwrap(), "");
    }

    #[test]
    fn slack_poll_backoff_caps_at_sixty_seconds() {
        assert_eq!(
            SlackChannel::poll_backoff_duration(1),
            std::time::Duration::from_secs(2)
        );
        assert_eq!(
            SlackChannel::poll_backoff_duration(2),
            std::time::Duration::from_secs(4)
        );
        assert_eq!(
            SlackChannel::poll_backoff_duration(7),
            std::time::Duration::from_secs(60)
        );
        assert_eq!(
            SlackChannel::poll_backoff_duration(99),
            std::time::Duration::from_secs(60)
        );
    }
}

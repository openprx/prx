use super::traits::{Channel, ChannelMessage, SendMessage};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::{Message, protocol::WebSocketConfig};

const DINGTALK_BOT_CALLBACK_TOPIC: &str = "/v1.0/im/bot/messages/get";
/// Base URL of the DingTalk OpenAPI.
const DINGTALK_OPENAPI_BASE: &str = "https://api.dingtalk.com";
/// Longest gap between two stream frames that is still considered normal.
///
/// The stream gateway drives its own ping/pong; this budget is generous enough
/// to cover a slow keepalive and is used for reporting only, never as a timeout.
const DINGTALK_STREAM_SILENCE_BUDGET_SECS: u64 = 180;
/// How long a delivered `msgId` is remembered so a stream redelivery of the same
/// callback does not run the turn twice.
///
/// The stream gateway redelivers a callback it considers unacknowledged, and a
/// reconnect can replay one that was already handed to the agent. Matches the
/// window Lark's websocket path uses for the same reason.
const DINGTALK_SEEN_MESSAGE_TTL: Duration = Duration::from_mins(30);
/// Safety margin subtracted from `sessionWebhookExpiredTime`.
///
/// The expiry is a server-side instant compared against a locally read clock, so
/// a webhook that is about to lapse is treated as already lapsed rather than
/// spent on a post that would be rejected on arrival.
const DINGTALK_WEBHOOK_EXPIRY_MARGIN_MS: u64 = 2_000;
/// Margin applied to a cached OpenAPI access token's lifetime.
const DINGTALK_TOKEN_EXPIRY_MARGIN: Duration = Duration::from_mins(2);

/// Where a reply to one chat can be delivered.
///
/// The session webhook is the fast path, but DingTalk hands it out as a
/// *temporary* address and states its lapse instant in the same callback
/// (`sessionWebhookExpiredTime`). An agent turn has no upper bound, so for a
/// long turn the address is routinely dead by the time the answer exists — hence
/// the proactive addressing kept alongside it.
#[derive(Clone, Debug)]
struct SessionRoute {
    webhook: String,
    /// `sessionWebhookExpiredTime`, in Unix milliseconds, when the callback
    /// carried it.
    expires_at_unix_ms: Option<u64>,
    /// `robotCode` from the callback; the OpenAPI requires it to identify the
    /// sending robot.
    robot_code: String,
    target: ProactiveTarget,
}

/// Addressing for a proactive (non-webhook) send.
#[derive(Clone, Debug)]
enum ProactiveTarget {
    /// One-to-one chat, addressed by the sender's staff id.
    User { staff_id: String },
    /// Group chat, addressed by the conversation the robot was called in.
    Group { open_conversation_id: String },
}

impl SessionRoute {
    /// Whether the session webhook can still be expected to accept a post.
    ///
    /// A callback without an expiry field is treated as usable: DingTalk always
    /// sends one in practice, and refusing the fast path on a missing field
    /// would push every reply onto the proactive quota for no evidence.
    fn webhook_is_live(&self, now_unix_ms: u64) -> bool {
        self.expires_at_unix_ms
            .is_none_or(|expires| now_unix_ms.saturating_add(DINGTALK_WEBHOOK_EXPIRY_MARGIN_MS) < expires)
    }
}

/// Cached OpenAPI access token used by the proactive send path.
#[derive(Clone, Debug)]
struct AccessToken {
    value: String,
    expires_at: Instant,
}

/// DingTalk channel — connects via Stream Mode WebSocket for real-time messages.
///
/// Replies prefer the per-message session webhook and fall back to the OpenAPI
/// proactive message APIs once that webhook has lapsed.
pub struct DingTalkChannel {
    client_id: String,
    client_secret: String,
    allowed_users: Vec<String>,
    /// Per-chat reply routes (chat id -> route). DingTalk provides a fresh
    /// session webhook with every incoming message.
    session_webhooks: Arc<RwLock<HashMap<String, SessionRoute>>>,
    /// Recently delivered `msgId`s, for inbound deduplication.
    seen_message_ids: Arc<RwLock<HashMap<String, Instant>>>,
    /// Cached OpenAPI token for proactive sends.
    access_token: Arc<RwLock<Option<AccessToken>>>,
    /// OpenAPI base URL. Production always uses [`DINGTALK_OPENAPI_BASE`]; tests
    /// point it at a local server so the expired-webhook fallback can be
    /// exercised end to end without reaching DingTalk.
    openapi_base: String,
}

/// Response from DingTalk gateway connection registration.
#[derive(serde::Deserialize)]
struct GatewayResponse {
    endpoint: String,
    ticket: String,
}

/// Response from the OpenAPI token endpoint.
#[derive(serde::Deserialize)]
struct AccessTokenResponse {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "expireIn")]
    expire_in: Option<u64>,
}

/// Non-zero `errcode` carried by a DingTalk response body, if any.
///
/// The session webhook lives on the legacy `oapi.dingtalk.com` surface, which
/// answers a rejected post with **HTTP 200** and an `errcode` in the body. A
/// status-only check therefore reads a refusal as a delivered reply — exactly
/// the silent loss this channel must not have — so the body is inspected too.
fn response_errcode_failure(body: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("errcode")?
        .as_i64()
        .filter(|code| *code != 0)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_millis()).ok())
        .unwrap_or(0)
}

impl DingTalkChannel {
    pub fn new(client_id: String, client_secret: String, allowed_users: Vec<String>) -> Self {
        Self {
            client_id,
            client_secret,
            allowed_users,
            session_webhooks: Arc::new(RwLock::new(HashMap::new())),
            seen_message_ids: Arc::new(RwLock::new(HashMap::new())),
            access_token: Arc::new(RwLock::new(None)),
            openapi_base: DINGTALK_OPENAPI_BASE.to_string(),
        }
    }

    fn http_client(&self) -> reqwest::Client {
        crate::config::build_runtime_proxy_client("channel.dingtalk")
            .map_err(|e| {
                tracing::error!("proxy build failed for channel.dingtalk, using direct: {e}");
                e
            })
            .unwrap_or_else(|_| reqwest::Client::new())
    }

    fn is_user_allowed(&self, user_id: &str) -> bool {
        self.allowed_users.iter().any(|u| u == "*" || u == user_id)
    }

    fn parse_stream_data(frame: &serde_json::Value) -> Option<serde_json::Value> {
        match frame.get("data") {
            Some(serde_json::Value::String(raw)) => serde_json::from_str(raw).ok(),
            Some(serde_json::Value::Object(_)) => frame.get("data").cloned(),
            _ => None,
        }
    }

    fn resolve_chat_id(data: &serde_json::Value, sender_id: &str) -> String {
        if Self::is_private_chat(data) {
            sender_id.to_string()
        } else {
            data.get("conversationId")
                .and_then(|c| c.as_str())
                .unwrap_or(sender_id)
                .to_string()
        }
    }

    /// Read the reply route out of one chatbot callback payload.
    ///
    /// `sessionWebhookExpiredTime` is a Unix **millisecond** timestamp; DingTalk
    /// documents it as the expiry of the temporary session webhook but does not
    /// publish a fixed lifetime, so the field itself is the only reliable source
    /// and is carried here verbatim rather than approximated by a constant.
    fn parse_session_route(
        data: &serde_json::Value,
        sender_id: &str,
        default_robot_code: &str,
    ) -> Option<SessionRoute> {
        let webhook = data.get("sessionWebhook").and_then(|w| w.as_str())?.to_string();
        let expires_at_unix_ms = data.get("sessionWebhookExpiredTime").and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_i64().and_then(|raw| u64::try_from(raw).ok()))
                .or_else(|| value.as_str().and_then(|raw| raw.trim().parse::<u64>().ok()))
        });
        let robot_code = data
            .get("robotCode")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .unwrap_or(default_robot_code)
            .to_string();
        let target = data
            .get("conversationId")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty() && !Self::is_private_chat(data))
            .map_or_else(
                || ProactiveTarget::User {
                    staff_id: sender_id.to_string(),
                },
                |conversation_id| ProactiveTarget::Group {
                    open_conversation_id: conversation_id.to_string(),
                },
            );

        Some(SessionRoute {
            webhook,
            expires_at_unix_ms,
            robot_code,
            target,
        })
    }

    /// OpenAPI access token for the proactive send path, cached until shortly
    /// before it expires.
    async fn openapi_access_token(&self) -> anyhow::Result<String> {
        if let Some(token) = self.access_token.read().await.as_ref() {
            if Instant::now() < token.expires_at {
                return Ok(token.value.clone());
            }
        }

        let body = serde_json::json!({
            "appKey": self.client_id,
            "appSecret": self.client_secret,
        });
        let resp = self
            .http_client()
            .post(format!("{}/v1.0/oauth2/accessToken", self.openapi_base))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            anyhow::bail!("DingTalk access token request failed ({status}): {err}");
        }
        let token: AccessTokenResponse = resp.json().await?;
        if token.access_token.is_empty() {
            anyhow::bail!("DingTalk access token response carried an empty token");
        }

        let lifetime = Duration::from_secs(token.expire_in.unwrap_or(7200));
        let expires_at = Instant::now() + lifetime.saturating_sub(DINGTALK_TOKEN_EXPIRY_MARGIN);
        let value = token.access_token;
        *self.access_token.write().await = Some(AccessToken {
            value: value.clone(),
            expires_at,
        });
        Ok(value)
    }

    /// Send a reply without the session webhook, using the robot's proactive
    /// message APIs.
    ///
    /// This is what keeps a long turn's answer from vanishing: once the session
    /// webhook has lapsed there is no other route back to the user, and dropping
    /// the reply would make the whole turn silently invisible to the person who
    /// asked for it.
    async fn send_proactive(&self, route: &SessionRoute, title: &str, text: &str) -> anyhow::Result<()> {
        let token = self.openapi_access_token().await?;
        let msg_param = serde_json::to_string(&serde_json::json!({
            "title": title,
            "text": text,
        }))?;

        let (url, body) = match &route.target {
            ProactiveTarget::User { staff_id } => (
                format!("{}/v1.0/robot/oToMessages/batchSend", self.openapi_base),
                serde_json::json!({
                    "robotCode": route.robot_code,
                    "userIds": [staff_id],
                    "msgKey": "sampleMarkdown",
                    "msgParam": msg_param,
                }),
            ),
            ProactiveTarget::Group { open_conversation_id } => (
                format!("{}/v1.0/robot/groupMessages/send", self.openapi_base),
                serde_json::json!({
                    "robotCode": route.robot_code,
                    "openConversationId": open_conversation_id,
                    "msgKey": "sampleMarkdown",
                    "msgParam": msg_param,
                }),
            ),
        };

        let resp = self
            .http_client()
            .post(url)
            .header("x-acs-dingtalk-access-token", token)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let payload = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("DingTalk proactive send failed ({status}): {payload}");
        }
        if let Some(errcode) = response_errcode_failure(&payload) {
            anyhow::bail!("DingTalk proactive send was refused (errcode {errcode}): {payload}");
        }
        Ok(())
    }

    /// Whether this callback describes a one-to-one chat.
    fn is_private_chat(data: &serde_json::Value) -> bool {
        data.get("conversationType")
            .and_then(|value| {
                value
                    .as_str()
                    .map(|v| v == "1")
                    .or_else(|| value.as_i64().map(|v| v == 1))
            })
            .unwrap_or(true)
    }

    /// Whether this `msgId` was already handed to the agent recently.
    ///
    /// Records the id as seen when it is new, so the caller can drop a
    /// redelivery instead of running the turn a second time.
    async fn is_duplicate_delivery(&self, msg_id: &str) -> bool {
        let now = Instant::now();
        let mut seen = self.seen_message_ids.write().await;
        seen.retain(|_, at| now.saturating_duration_since(*at) < DINGTALK_SEEN_MESSAGE_TTL);
        if seen.contains_key(msg_id) {
            return true;
        }
        seen.insert(msg_id.to_string(), now);
        false
    }

    /// Register a connection with DingTalk's gateway to get a WebSocket endpoint.
    async fn register_connection(&self) -> anyhow::Result<GatewayResponse> {
        let body = serde_json::json!({
            "clientId": self.client_id,
            "clientSecret": self.client_secret,
            "subscriptions": [
                {
                    "type": "CALLBACK",
                    "topic": DINGTALK_BOT_CALLBACK_TOPIC,
                }
            ],
        });

        let resp = self
            .http_client()
            .post(format!("{}/v1.0/gateway/connections/open", self.openapi_base))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            anyhow::bail!("DingTalk gateway registration failed ({status}): {err}");
        }

        let gw: GatewayResponse = resp.json().await?;
        Ok(gw)
    }
}

#[async_trait]
impl Channel for DingTalkChannel {
    fn name(&self) -> &str {
        "dingtalk"
    }

    /// The stream gateway pings periodically and every ping arrives as a frame.
    ///
    /// Report-only (see `channels::activity`): never a timeout.
    fn liveness_expectation(&self) -> crate::channels::activity::LivenessModel {
        crate::channels::activity::LivenessModel::Bounded(std::time::Duration::from_secs(
            DINGTALK_STREAM_SILENCE_BUDGET_SECS,
        ))
    }

    /// Deliver a reply, preferring the session webhook and falling back to the
    /// robot's proactive message API once that webhook has lapsed.
    ///
    /// DingTalk is the one channel here whose reply address expires. Every other
    /// channel answers through a durable API, so a turn that took an hour still
    /// reaches the user; DingTalk hands out a temporary `sessionWebhook` with the
    /// inbound message and states its expiry in the same payload. Posting to a
    /// lapsed webhook fails, and failing there used to be the end of the road:
    /// the answer was simply never delivered. Both routes are therefore tried,
    /// and if both are gone the failure is logged and returned rather than
    /// swallowed — a lost reply must never be silent.
    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let route = {
            let webhooks = self.session_webhooks.read().await;
            webhooks.get(&message.recipient).cloned()
        };
        let route = route.ok_or_else(|| {
            anyhow::anyhow!(
                "No session webhook found for chat {}. \
                 The user must send a message first to establish a session.",
                message.recipient
            )
        })?;

        let title = message.subject.as_deref().unwrap_or("OpenPRX");

        if route.webhook_is_live(now_unix_ms()) {
            let body = serde_json::json!({
                "msgtype": "markdown",
                "markdown": {
                    "title": title,
                    "text": message.content,
                }
            });

            match self.http_client().post(&route.webhook).json(&body).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let payload = resp.text().await.unwrap_or_default();
                    let errcode = response_errcode_failure(&payload);
                    if status.is_success() && errcode.is_none() {
                        return Ok(());
                    }
                    // The clock said the webhook was live and DingTalk disagreed.
                    // Treat the server as authoritative and try the durable route
                    // rather than reporting a reply that was never delivered.
                    tracing::warn!(
                        "DingTalk session webhook rejected the reply for {} ({status}): {payload} — \
                         retrying through the proactive message API",
                        message.recipient
                    );
                }
                Err(e) => {
                    // A transport failure hides whether the post landed, so the
                    // retry can in principle double a delivered reply. That is
                    // the deliberate trade: a duplicate answer is visible and
                    // recoverable, a dropped one is neither.
                    tracing::warn!(
                        "DingTalk session webhook post failed for {}: {e} — \
                         retrying through the proactive message API",
                        message.recipient
                    );
                }
            }
        } else {
            tracing::info!(
                "DingTalk session webhook for {} expired before the reply was ready — \
                 delivering through the proactive message API",
                message.recipient
            );
        }

        self.send_proactive(&route, title, &message.content).await.map_err(|e| {
            tracing::error!(
                recipient = %message.recipient,
                "DingTalk reply could not be delivered: the session webhook is unusable and \
                 the proactive message API failed: {e:#}"
            );
            e.context(format!(
                "DingTalk reply to {} could not be delivered by either the session webhook or the proactive API",
                message.recipient
            ))
        })
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        tracing::info!("DingTalk: registering gateway connection...");

        let gw = self.register_connection().await?;
        let ws_url = format!("{}?ticket={}", gw.endpoint, gw.ticket);

        tracing::info!("DingTalk: connecting to stream WebSocket...");
        let mut ws_config = WebSocketConfig::default();
        ws_config.max_message_size = Some(2 * 1024 * 1024);
        ws_config.max_frame_size = Some(1024 * 1024);
        let (ws_stream, _) = tokio_tungstenite::connect_async_with_config(&ws_url, Some(ws_config), false).await?;
        let (mut write, mut read) = ws_stream.split();

        tracing::info!("DingTalk: connected and listening for messages...");

        while let Some(msg) = read.next().await {
            // Any frame counts, including the server's SYSTEM ping, so an idle
            // conversation still proves the stream is carrying traffic.
            crate::channels::activity::record_upstream(self.name());

            let msg = match msg {
                Ok(Message::Text(t)) => t,
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    tracing::warn!("DingTalk WebSocket error: {e}");
                    break;
                }
                _ => continue,
            };

            let frame: serde_json::Value = match serde_json::from_str(msg.as_ref()) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let frame_type = frame.get("type").and_then(|t| t.as_str()).unwrap_or("");

            match frame_type {
                "SYSTEM" => {
                    // Respond to system pings to keep the connection alive
                    let message_id = frame
                        .get("headers")
                        .and_then(|h| h.get("messageId"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("");

                    let pong = serde_json::json!({
                        "code": 200,
                        "headers": {
                            "contentType": "application/json",
                            "messageId": message_id,
                        },
                        "message": "OK",
                        "data": "",
                    });

                    if let Err(e) = write.send(Message::Text(pong.to_string().into())).await {
                        tracing::warn!("DingTalk: failed to send pong: {e}");
                        break;
                    }
                }
                "EVENT" | "CALLBACK" => {
                    // Parse the chatbot callback data from the frame.
                    let data = match Self::parse_stream_data(&frame) {
                        Some(v) => v,
                        None => {
                            tracing::debug!("DingTalk: frame has no parseable data payload");
                            continue;
                        }
                    };

                    // Extract message content
                    let content = data
                        .get("text")
                        .and_then(|t| t.get("content"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .trim();

                    if content.is_empty() {
                        continue;
                    }

                    let sender_id = data.get("senderStaffId").and_then(|s| s.as_str()).unwrap_or("unknown");

                    if !self.is_user_allowed(sender_id) {
                        tracing::warn!("DingTalk: ignoring message from unauthorized user: {sender_id}");
                        continue;
                    }

                    // Private chat uses sender ID, group chat uses conversation ID.
                    let chat_id = Self::resolve_chat_id(&data, sender_id);

                    // Store the reply route (webhook, its expiry, and proactive
                    // addressing) for later replies.
                    if let Some(route) = Self::parse_session_route(&data, sender_id, &self.client_id) {
                        let mut webhooks = self.session_webhooks.write().await;
                        // Use both keys so reply routing works for both group and private flows.
                        webhooks.insert(chat_id.clone(), route.clone());
                        webhooks.insert(sender_id.to_string(), route);
                    }

                    // Acknowledge the event
                    let message_id = frame
                        .get("headers")
                        .and_then(|h| h.get("messageId"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("");

                    let ack = serde_json::json!({
                        "code": 200,
                        "headers": {
                            "contentType": "application/json",
                            "messageId": message_id,
                        },
                        "message": "OK",
                        "data": "",
                    });
                    let _ = write.send(Message::Text(ack.to_string().into())).await;

                    // The stream gateway redelivers a callback it treats as
                    // unacknowledged, and a reconnect can replay one that was
                    // already dispatched. `msgId` is stable across both, so it is
                    // what decides whether this is a new message; it also becomes
                    // the message id, which keeps every downstream key derived
                    // from it stable across a redelivery instead of minting a
                    // fresh UUID that defeats deduplication further down.
                    let dingtalk_msg_id = data
                        .get("msgId")
                        .and_then(|value| value.as_str())
                        .filter(|value| !value.is_empty())
                        .map(str::to_string);
                    // A callback with no `msgId` cannot be deduplicated, but
                    // dropping it would lose a real message to protect against a
                    // possible duplicate — the wrong trade. It is dispatched with
                    // a synthetic id and a warning instead.
                    let message_key = match dingtalk_msg_id {
                        Some(msg_id) => {
                            if self.is_duplicate_delivery(&msg_id).await {
                                tracing::debug!("DingTalk: redelivery of {msg_id} — not dispatched again");
                                continue;
                            }
                            msg_id
                        }
                        None => {
                            tracing::warn!("DingTalk: callback carried no msgId; dispatching without deduplication");
                            uuid::Uuid::new_v4().to_string()
                        }
                    };

                    let channel_msg = ChannelMessage {
                        id: message_key,
                        sender: sender_id.to_string(),
                        reply_target: chat_id,
                        content: content.to_string(),
                        channel: "dingtalk".to_string(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        thread_ts: None,
                        chat_kind: crate::channels::traits::ChatKind::Dm,
                        chat_title: None,
                        sender_display: None,
                        mentioned_uuids: vec![],
                        mentioned: false,
                        is_group_hint: false,
                        sender_is_bot: false,
                    };

                    if tx.send(channel_msg).await.is_err() {
                        tracing::warn!("DingTalk: message channel closed");
                        break;
                    }
                }
                _ => {}
            }
        }

        anyhow::bail!("DingTalk WebSocket stream ended")
    }

    async fn health_check(&self) -> bool {
        self.register_connection().await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_name() {
        let ch = DingTalkChannel::new("id".into(), "secret".into(), vec![]);
        assert_eq!(ch.name(), "dingtalk");
    }

    #[test]
    fn test_user_allowed_wildcard() {
        let ch = DingTalkChannel::new("id".into(), "secret".into(), vec!["*".into()]);
        assert!(ch.is_user_allowed("anyone"));
    }

    #[test]
    fn test_user_allowed_specific() {
        let ch = DingTalkChannel::new("id".into(), "secret".into(), vec!["user123".into()]);
        assert!(ch.is_user_allowed("user123"));
        assert!(!ch.is_user_allowed("other"));
    }

    #[test]
    fn test_user_denied_empty() {
        let ch = DingTalkChannel::new("id".into(), "secret".into(), vec![]);
        assert!(!ch.is_user_allowed("anyone"));
    }

    #[test]
    fn test_config_serde() {
        let toml_str = r#"
client_id = "app_id_123"
client_secret = "secret_456"
allowed_users = ["user1", "*"]
"#;
        let config: crate::config::schema::DingTalkConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.client_id, "app_id_123");
        assert_eq!(config.client_secret, "secret_456");
        assert_eq!(config.allowed_users, vec!["user1", "*"]);
    }

    #[test]
    fn test_config_serde_defaults() {
        let toml_str = r#"
client_id = "id"
client_secret = "secret"
"#;
        let config: crate::config::schema::DingTalkConfig = toml::from_str(toml_str).unwrap();
        assert!(config.allowed_users.is_empty());
    }

    #[test]
    fn parse_stream_data_supports_string_payload() {
        let frame = serde_json::json!({
            "data": "{\"text\":{\"content\":\"hello\"}}"
        });
        let parsed = DingTalkChannel::parse_stream_data(&frame).unwrap();
        assert_eq!(
            parsed.get("text").and_then(|v| v.get("content")),
            Some(&serde_json::json!("hello"))
        );
    }

    #[test]
    fn parse_stream_data_supports_object_payload() {
        let frame = serde_json::json!({
            "data": {"text": {"content": "hello"}}
        });
        let parsed = DingTalkChannel::parse_stream_data(&frame).unwrap();
        assert_eq!(
            parsed.get("text").and_then(|v| v.get("content")),
            Some(&serde_json::json!("hello"))
        );
    }

    // ── sessionWebhook expiry ──────────────────────────────────────────────
    //
    // DingTalk's reply address is temporary and its lapse instant arrives in the
    // same callback (`sessionWebhookExpiredTime`, Unix milliseconds). Before
    // these paths existed, a turn that outlived the webhook posted to a dead URL
    // and the answer was lost with nothing but a local log line to show for it.

    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    #[derive(Clone, Default)]
    struct FakeDingTalk {
        webhook_hits: Arc<AtomicUsize>,
        token_hits: Arc<AtomicUsize>,
        proactive_hits: Arc<AtomicUsize>,
        proactive_fails: bool,
    }

    /// Local stand-in for DingTalk: one session-webhook endpoint, the OpenAPI
    /// token endpoint, and the one-to-one proactive endpoint.
    async fn spawn_fake_dingtalk(fake: FakeDingTalk) -> String {
        async fn session_webhook(State(fake): State<FakeDingTalk>) -> Json<serde_json::Value> {
            fake.webhook_hits.fetch_add(1, Ordering::SeqCst);
            Json(serde_json::json!({"errcode": 0}))
        }
        async fn access_token(State(fake): State<FakeDingTalk>) -> Json<serde_json::Value> {
            fake.token_hits.fetch_add(1, Ordering::SeqCst);
            Json(serde_json::json!({"accessToken": "test-token", "expireIn": 7200}))
        }
        async fn proactive(State(fake): State<FakeDingTalk>) -> (axum::http::StatusCode, Json<serde_json::Value>) {
            fake.proactive_hits.fetch_add(1, Ordering::SeqCst);
            if fake.proactive_fails {
                return (
                    axum::http::StatusCode::FORBIDDEN,
                    Json(serde_json::json!({"code": "Forbidden.AccessDenied"})),
                );
            }
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({"processQueryKey": "k"})),
            )
        }

        let app = Router::new()
            .route("/session/webhook", post(session_webhook))
            .route("/v1.0/oauth2/accessToken", post(access_token))
            .route("/v1.0/robot/oToMessages/batchSend", post(proactive))
            .with_state(fake);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
        let addr = listener.local_addr().expect("test: addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    async fn channel_with_route(base: &str, route: SessionRoute) -> DingTalkChannel {
        let channel = DingTalkChannel {
            openapi_base: base.to_string(),
            ..DingTalkChannel::new("client-id".into(), "client-secret".into(), vec!["*".into()])
        };
        channel.session_webhooks.write().await.insert("chat-1".into(), route);
        channel
    }

    fn route_expiring_at(base: &str, expires_at_unix_ms: Option<u64>) -> SessionRoute {
        SessionRoute {
            webhook: format!("{base}/session/webhook"),
            expires_at_unix_ms,
            robot_code: "robot-code".into(),
            target: ProactiveTarget::User {
                staff_id: "staff-1".into(),
            },
        }
    }

    #[test]
    fn parse_session_route_reads_expiry_robot_code_and_group_target() {
        let data = serde_json::json!({
            "sessionWebhook": "https://oapi.dingtalk.com/robot/sendBySession?session=abc",
            "sessionWebhookExpiredTime": 1_695_204_671_648_u64,
            "robotCode": "ding-robot",
            "conversationType": "2",
            "conversationId": "cid-group",
        });
        let route = DingTalkChannel::parse_session_route(&data, "staff-1", "fallback-code").expect("test: route");
        assert_eq!(route.expires_at_unix_ms, Some(1_695_204_671_648));
        assert_eq!(route.robot_code, "ding-robot");
        match route.target {
            ProactiveTarget::Group { open_conversation_id } => assert_eq!(open_conversation_id, "cid-group"),
            ProactiveTarget::User { .. } => panic!("test: group callback must produce a group target"),
        }
    }

    #[test]
    fn parse_session_route_falls_back_to_client_id_and_dm_target() {
        let data = serde_json::json!({
            "sessionWebhook": "https://oapi.dingtalk.com/robot/sendBySession?session=abc",
            "sessionWebhookExpiredTime": "1695204671648",
            "conversationType": 1,
            "conversationId": "cid-dm",
        });
        let route = DingTalkChannel::parse_session_route(&data, "staff-9", "fallback-code").expect("test: route");
        assert_eq!(route.expires_at_unix_ms, Some(1_695_204_671_648));
        assert_eq!(route.robot_code, "fallback-code");
        match route.target {
            ProactiveTarget::User { staff_id } => assert_eq!(staff_id, "staff-9"),
            ProactiveTarget::Group { .. } => panic!("test: private callback must produce a user target"),
        }
    }

    #[test]
    fn expired_session_webhook_is_not_considered_live() {
        let now = 1_000_000_000_000_u64;
        let expired = SessionRoute {
            webhook: "https://example.invalid".into(),
            expires_at_unix_ms: Some(now - 1),
            robot_code: "r".into(),
            target: ProactiveTarget::User { staff_id: "s".into() },
        };
        assert!(!expired.webhook_is_live(now));

        let live = SessionRoute {
            expires_at_unix_ms: Some(now + DINGTALK_WEBHOOK_EXPIRY_MARGIN_MS + 1_000),
            ..expired.clone()
        };
        assert!(live.webhook_is_live(now));

        // Inside the margin the webhook is treated as already gone.
        let marginal = SessionRoute {
            expires_at_unix_ms: Some(now + DINGTALK_WEBHOOK_EXPIRY_MARGIN_MS),
            ..expired.clone()
        };
        assert!(!marginal.webhook_is_live(now));

        // A callback without the field keeps the fast path.
        let unknown = SessionRoute {
            expires_at_unix_ms: None,
            ..expired
        };
        assert!(unknown.webhook_is_live(now));
    }

    #[tokio::test]
    async fn expired_session_webhook_reply_is_delivered_proactively() {
        let fake = FakeDingTalk::default();
        let base = spawn_fake_dingtalk(fake.clone()).await;
        // Expired an hour ago: exactly the state a long turn leaves behind.
        let expired_at = now_unix_ms().saturating_sub(3_600_000);
        let channel = channel_with_route(&base, route_expiring_at(&base, Some(expired_at))).await;

        channel
            .send(&SendMessage::new("the answer", "chat-1"))
            .await
            .expect("test: expired webhook must still deliver");

        assert_eq!(
            fake.webhook_hits.load(Ordering::SeqCst),
            0,
            "a lapsed webhook must not be posted to"
        );
        assert_eq!(fake.token_hits.load(Ordering::SeqCst), 1);
        assert_eq!(fake.proactive_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn live_session_webhook_is_still_the_fast_path() {
        let fake = FakeDingTalk::default();
        let base = spawn_fake_dingtalk(fake.clone()).await;
        let expires_at = now_unix_ms().saturating_add(600_000);
        let channel = channel_with_route(&base, route_expiring_at(&base, Some(expires_at))).await;

        channel
            .send(&SendMessage::new("the answer", "chat-1"))
            .await
            .expect("test: live webhook must deliver");

        assert_eq!(fake.webhook_hits.load(Ordering::SeqCst), 1);
        assert_eq!(fake.proactive_hits.load(Ordering::SeqCst), 0);
        assert_eq!(fake.token_hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn undeliverable_reply_fails_loudly_instead_of_vanishing() {
        let fake = FakeDingTalk {
            proactive_fails: true,
            ..FakeDingTalk::default()
        };
        let base = spawn_fake_dingtalk(fake.clone()).await;
        let expired_at = now_unix_ms().saturating_sub(3_600_000);
        let channel = channel_with_route(&base, route_expiring_at(&base, Some(expired_at))).await;

        let error = channel
            .send(&SendMessage::new("the answer", "chat-1"))
            .await
            .expect_err("test: a reply that cannot be delivered must be reported");

        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("could not be delivered"),
            "the failure must name the undelivered reply, got: {rendered}"
        );
        assert!(
            rendered.contains("chat-1"),
            "the failure must name the chat, got: {rendered}"
        );
        assert_eq!(fake.proactive_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn session_webhook_refusal_with_http_200_still_falls_back() {
        // The legacy `oapi.dingtalk.com` surface reports a refusal as HTTP 200
        // plus an `errcode`, so a status-only check would call a lost reply
        // delivered.
        async fn refusing_webhook(State(fake): State<FakeDingTalk>) -> Json<serde_json::Value> {
            fake.webhook_hits.fetch_add(1, Ordering::SeqCst);
            Json(serde_json::json!({"errcode": 300_001, "errmsg": "token is not exist"}))
        }
        async fn access_token(State(fake): State<FakeDingTalk>) -> Json<serde_json::Value> {
            fake.token_hits.fetch_add(1, Ordering::SeqCst);
            Json(serde_json::json!({"accessToken": "test-token", "expireIn": 7200}))
        }
        async fn proactive(State(fake): State<FakeDingTalk>) -> Json<serde_json::Value> {
            fake.proactive_hits.fetch_add(1, Ordering::SeqCst);
            Json(serde_json::json!({"processQueryKey": "k"}))
        }

        let fake = FakeDingTalk::default();
        let app = Router::new()
            .route("/session/webhook", post(refusing_webhook))
            .route("/v1.0/oauth2/accessToken", post(access_token))
            .route("/v1.0/robot/oToMessages/batchSend", post(proactive))
            .with_state(fake.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
        let addr = listener.local_addr().expect("test: addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let base = format!("http://{addr}");

        // Still inside its stated validity window, so the fast path is tried.
        let expires_at = now_unix_ms().saturating_add(600_000);
        let channel = channel_with_route(&base, route_expiring_at(&base, Some(expires_at))).await;

        channel
            .send(&SendMessage::new("the answer", "chat-1"))
            .await
            .expect("test: a refused webhook must fall back, not report success");

        assert_eq!(fake.webhook_hits.load(Ordering::SeqCst), 1);
        assert_eq!(fake.proactive_hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn response_errcode_failure_only_flags_real_refusals() {
        assert_eq!(response_errcode_failure(r#"{"errcode":300001}"#), Some(300_001));
        assert_eq!(response_errcode_failure(r#"{"errcode":0}"#), None);
        assert_eq!(response_errcode_failure(r#"{"processQueryKey":"k"}"#), None);
        assert_eq!(response_errcode_failure("not json"), None);
    }

    #[tokio::test]
    async fn stream_redelivery_of_the_same_msg_id_is_dropped() {
        let channel = DingTalkChannel::new("id".into(), "secret".into(), vec!["*".into()]);
        assert!(!channel.is_duplicate_delivery("msgid-1").await);
        assert!(channel.is_duplicate_delivery("msgid-1").await);
        assert!(!channel.is_duplicate_delivery("msgid-2").await);
    }

    #[test]
    fn resolve_chat_id_handles_numeric_group_conversation_type() {
        let data = serde_json::json!({
            "conversationType": 2,
            "conversationId": "cid-group",
        });
        let chat_id = DingTalkChannel::resolve_chat_id(&data, "staff-1");
        assert_eq!(chat_id, "cid-group");
    }
}

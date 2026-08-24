use super::{AppState, authorize_resource_mutation};
use crate::channels::outbound_registry;
use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
use crate::security::policy::ResourceRiskLevel;
use crate::tools::MessageSendTool;
use crate::tools::message_send::{MESSAGE_SEND_EXECUTION_CONTEXT, MessageSendExecutionContext};
use crate::tools::traits::Tool;
use async_trait::async_trait;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize)]
struct ChannelStatus {
    name: String,
    enabled: bool,
    #[serde(rename = "type")]
    channel_type: String,
    status: String,
}

#[derive(Serialize)]
pub(super) struct ChannelsStatusResponse {
    channels: Vec<ChannelStatus>,
}

pub async fn get_channels_status(State(state): State<AppState>) -> Json<ChannelsStatusResponse> {
    let config = state.config.load_full();
    let mut channels = Vec::new();

    let mut push_channel = |name: &str, enabled: bool| {
        if enabled {
            channels.push(ChannelStatus {
                name: name.to_string(),
                enabled,
                channel_type: name.to_string(),
                status: "configured".to_string(),
            });
        }
    };

    push_channel("cli", config.channels_config.cli);
    push_channel("telegram", config.channels_config.telegram.is_some());
    push_channel("discord", config.channels_config.discord.is_some());
    push_channel("slack", config.channels_config.slack.is_some());
    push_channel("mattermost", config.channels_config.mattermost.is_some());
    push_channel("webhook", config.channels_config.webhook.is_some());
    push_channel("imessage", config.channels_config.imessage.is_some());
    push_channel("matrix", config.channels_config.matrix.is_some());
    push_channel("signal", config.channels_config.signal.is_some());
    push_channel("whatsapp", config.channels_config.whatsapp.is_some());
    push_channel("wacli", config.channels_config.wacli.is_some());
    push_channel("linq", config.channels_config.linq.is_some());
    push_channel("nextcloud_talk", config.channels_config.nextcloud_talk.is_some());
    push_channel("email", config.channels_config.email.is_some());
    push_channel("irc", config.channels_config.irc.is_some());
    push_channel("lark", config.channels_config.lark.is_some());
    push_channel("dingtalk", config.channels_config.dingtalk.is_some());
    push_channel("qq", config.channels_config.qq.is_some());

    channels.sort_by(|a, b| a.name.cmp(&b.name));
    Json(ChannelsStatusResponse { channels })
}

// ── Outbound send on behalf of a channel-less entry point ────────────────────

/// Channel name the operator plane sends *from*.
///
/// An HTTP send has no inbound conversation behind it, so there is no turn
/// channel to inherit. Naming the operator plane explicitly is what makes every
/// send through this endpoint a *cross-channel* send in the eyes of the outbound
/// scope rules: it is denied unless an operator opted in with `send_allow`,
/// exactly like any other cross-channel delivery. It is deliberately not a
/// [`Channel::name`] any implementation returns, so it can never collide with a
/// real destination.
pub(crate) const OPERATOR_PLANE_CHANNEL: &str = "api";

/// Longest channel name accepted from the request path. Names are compared
/// against a registry of short identifiers; anything longer is a mistake or an
/// attempt to stuff the error message, and is refused before it is echoed back.
const MAX_CHANNEL_NAME_LEN: usize = 64;
/// Longest recipient accepted. Comfortably above every platform identifier
/// (phone numbers, JIDs, room ids, e-mail addresses) the channels accept.
const MAX_RECIPIENT_LEN: usize = 512;
/// Longest message body accepted. Well past every platform's own limit, so the
/// destination's rejection — not this one — is what an operator normally sees.
const MAX_MESSAGE_LEN: usize = 64 * 1024;

/// The operator plane as a [`Channel`], purely so the outbound decision has a
/// concrete origin to name.
///
/// It never delivers: `send` is an error, because "the operator plane" is not a
/// place a message can arrive. Only its name is load-bearing.
struct OperatorPlaneChannel;

#[async_trait]
impl Channel for OperatorPlaneChannel {
    fn name(&self) -> &str {
        OPERATOR_PLANE_CHANNEL
    }

    async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "the operator plane is not a deliverable channel; name a configured channel in the request path"
        ))
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("the operator plane has no inbound side"))
    }
}

#[derive(Deserialize)]
pub(super) struct ChannelSendRequest {
    recipient: String,
    message: String,
    /// Carried so the refusal is explicit rather than a silent downgrade: a
    /// caller that asked for a voice note must be told it cannot travel here,
    /// not handed a plain-text send it did not ask for.
    #[serde(default)]
    as_voice: bool,
}

#[derive(Serialize)]
pub(super) struct ChannelSendResponse {
    channel: String,
    delivered: bool,
    detail: String,
}

fn refusal(status: StatusCode, detail: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": detail.into() })))
}

/// Validate one free-text field from the request boundary.
fn bounded(value: &str, field: &str, limit: usize) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(refusal(StatusCode::BAD_REQUEST, format!("{field} must not be empty")));
    }
    if trimmed.len() > limit {
        return Err(refusal(
            StatusCode::BAD_REQUEST,
            format!("{field} must be at most {limit} bytes"),
        ));
    }
    Ok(trimmed.to_string())
}

/// Send one message on a configured channel, on behalf of an entry point that
/// holds no channel object of its own (`prx chat`, which must not open inbound
/// connections that would race the daemon's).
///
/// **Every decision here is the server's.** The request body carries a
/// recipient and a message and nothing else: the sending identity, the
/// originating channel and the policy verdict are all constructed here, so a
/// client cannot widen its own reach by claiming a scope, and cannot skip a
/// check by having "already run" it. The authoritative gate chain is
/// [`MessageSendTool::execute`] — the same code an in-process `message_send`
/// call runs, invoked with the operator plane pinned as the turn channel:
///
/// 1. resolve the named channel, erroring with the addressable set if unknown;
/// 2. authorize the destination against the outbound scope rules — always a
///    cross-channel decision here (see [`OPERATOR_PLANE_CHANNEL`]);
/// 3. refuse media, which cannot travel between channels.
///
/// The pre-flight checks below re-run those predicates for one purpose only:
/// choosing the HTTP status. The *verdict and its wording* always come from the
/// tool, so this endpoint cannot drift into a second, weaker policy.
pub async fn post_channel_send(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<ChannelSendRequest>,
) -> Result<Json<ChannelSendResponse>, (StatusCode, Json<serde_json::Value>)> {
    authorize_resource_mutation(&state, "channel_send", ResourceRiskLevel::Medium)?;

    let channel_name = bounded(&name, "channel name", MAX_CHANNEL_NAME_LEN)?;
    let recipient = bounded(&request.recipient, "recipient", MAX_RECIPIENT_LEN)?;
    let message = bounded(&request.message, "message", MAX_MESSAGE_LEN)?;

    let config = state.config.load_full();
    let policy = crate::runtime::bootstrap::build_security_policy(&config);
    let registry = outbound_registry::snapshot();

    // Arguments are built here, from typed fields. Anything else the caller put
    // in the body — a `_zc_scope`, a `channel`, a claim of prior approval — is
    // not carried into the tool, so the identity behind this send is always the
    // unknown operator plane and only rules that match it can permit anything.
    let args = serde_json::json!({
        "action": "send",
        "channel": channel_name,
        "target": recipient,
        "message": message,
        "as_voice": request.as_voice,
    });

    // Status classification only — see the doc comment.
    let unknown_channel = !registry.contains_key(&channel_name);
    let denied_by_policy = !policy.is_outbound_allowed(
        UNKNOWN_IDENTITY,
        OPERATOR_PLANE_CHANNEL,
        UNKNOWN_IDENTITY,
        &channel_name,
        &recipient,
    );
    let media_refused = MessageSendTool::reject_cross_channel_media(&args, &channel_name).is_err();

    let operator_plane: Arc<dyn Channel> = Arc::new(OperatorPlaneChannel);
    let tool = MessageSendTool::new(Arc::clone(&operator_plane), policy).with_channels(Arc::new(registry));
    // Pin the turn channel to the operator plane for this call. Without it the
    // tool would read whatever task-local context happens to be in scope, and an
    // ambient turn's channel would silently become the *source* of this send —
    // turning a cross-channel decision into a same-channel one.
    let context = MessageSendExecutionContext::new(None, Arc::clone(&operator_plane));
    let result = MESSAGE_SEND_EXECUTION_CONTEXT
        .scope(context, tool.execute(args))
        .await
        .map_err(|error| {
            refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("outbound send failed: {error}"),
            )
        })?;

    if result.success {
        return Ok(Json(ChannelSendResponse {
            channel: channel_name,
            delivered: true,
            detail: result.output,
        }));
    }

    let detail = result
        .error
        .unwrap_or_else(|| "the channel refused the message without a reason".to_string());
    // The order mirrors the tool's gate order: an unknown destination is not a
    // policy question, and a policy refusal outranks a media one so a denied
    // send is never reported as a formatting problem.
    let status = if unknown_channel {
        StatusCode::NOT_FOUND
    } else if denied_by_policy {
        StatusCode::FORBIDDEN
    } else if media_refused {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::BAD_GATEWAY
    };
    Err(refusal(status, detail))
}

/// Sender and chat type behind an operator-plane send.
///
/// There is no inbound message here, so there is no sender to speak of. Scope
/// rules that name a `user` or a `chat_type` therefore do not match this path;
/// rules for it are written against `channel = "api"`.
const UNKNOWN_IDENTITY: &str = "unknown";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::traits::ChannelCapabilities;
    use crate::config::{Config, ScopeRule};
    use crate::gateway::{GatewayRateLimiter, IdempotencyStore};
    use crate::hooks::HookManager;
    use crate::memory::SqliteMemory;
    use crate::observability::NoopObserver;
    use crate::providers::Provider;
    use crate::security::pairing::PairingGuard;
    use crate::security::policy::AutonomyLevel;
    use crate::tools::DaemonMessageSendTool;
    use serial_test::serial;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    const TOKEN: &str = "zc_channel_send_test_token";
    const CHANNEL: &str = "fake-im";
    const RECIPIENT: &str = "+15550009999";
    const POLICY_MARKER: &str = "not permitted by the configured scope rules";

    /// Channel that keeps what it was handed, so a delivery can be proven
    /// rather than inferred from a 200.
    struct RecordingChannel {
        sent: tokio::sync::Mutex<Vec<SendMessage>>,
        fails: bool,
    }

    impl RecordingChannel {
        fn new(fails: bool) -> Arc<Self> {
            Arc::new(Self {
                sent: tokio::sync::Mutex::new(Vec::new()),
                fails,
            })
        }

        async fn deliveries(&self) -> Vec<(String, String)> {
            self.sent
                .lock()
                .await
                .iter()
                .map(|message| (message.recipient.clone(), message.content.clone()))
                .collect()
        }
    }

    #[async_trait]
    impl Channel for RecordingChannel {
        fn name(&self) -> &str {
            CHANNEL
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            if self.fails {
                return Err(anyhow::anyhow!("the fake channel is down"));
            }
            self.sent.lock().await.push(message.clone());
            Ok(())
        }

        async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
            Ok(())
        }

        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities::default()
        }
    }

    /// Provider the gateway state needs but these tests never reach.
    struct UnusedProvider;

    #[async_trait]
    impl Provider for UnusedProvider {
        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Err(anyhow::anyhow!("test: the provider is never called"))
        }

        async fn chat(
            &self,
            _request: crate::providers::ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<crate::providers::ChatResponse> {
            Err(anyhow::anyhow!("test: the provider is never called"))
        }
    }

    fn scoped_config(workspace: &std::path::Path, send_allow: &[&str], user: Option<&str>) -> Config {
        let mut config = Config::default();
        config.workspace_dir = workspace.to_path_buf();
        config.autonomy.level = AutonomyLevel::Full;
        if !send_allow.is_empty() {
            config.autonomy.scopes.rules = vec![ScopeRule {
                user: user.map(str::to_string),
                send_allow: send_allow.iter().map(|entry| (*entry).to_string()).collect(),
                ..ScopeRule::default()
            }];
        }
        config
    }

    fn test_app_state(config: Config) -> AppState {
        let workspace = config.workspace_dir.clone();
        let memory = SqliteMemory::new(&workspace).expect("test: sqlite memory");
        AppState {
            config: crate::config::new_shared(config),
            provider: Arc::new(UnusedProvider),
            model: "test-model".into(),
            temperature: 0.0,
            mem: Arc::new(memory),
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            turn_runtime: None,
            hooks: Arc::new(HookManager::new(workspace)),
            webhook_token_hash: None,
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(true, &[TOKEN.to_string()])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(10_000, 10_000, 10_000, 10_000)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_mins(5), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_runtime: None,
        }
    }

    /// Serve the real API router — same routes, same auth and rate-limit
    /// layers the daemon mounts — on an ephemeral port.
    async fn serve(state: AppState) -> (String, u16, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new()
            .nest("/api", super::super::router(state.clone()))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind ephemeral port");
        let port = listener.local_addr().expect("test: local addr").port();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://127.0.0.1:{port}"), port, handle)
    }

    /// POST a body of the caller's choosing — including fields a well-behaved
    /// client would never send.
    async fn post_raw(base_url: &str, channel: &str, token: Option<&str>, body: serde_json::Value) -> (u16, String) {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("test: http client");
        let request = client
            .post(format!("{base_url}/api/channels/{channel}/send"))
            .json(&body);
        let request = match token {
            Some(token) => request.bearer_auth(token),
            None => request,
        };
        let response = request.send().await.expect("test: request");
        let status = response.status().as_u16();
        let body = response.text().await.expect("test: response body");
        (status, body)
    }

    fn published(channel: &Arc<RecordingChannel>) -> outbound_registry::OutboundChannelsPublication {
        let mut registry: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        registry.insert(CHANNEL.to_string(), Arc::clone(channel) as Arc<dyn Channel>);
        outbound_registry::publish(Arc::new(registry))
    }

    /// Chat's own tool, pointed at the served daemon — the real client of this
    /// endpoint, not a hand-rolled request.
    fn chat_tool(base_url: &str, config: &Config) -> DaemonMessageSendTool {
        let mut chat_config = config.clone();
        chat_config.chat.daemon.url = base_url.to_string();
        chat_config.chat.daemon.token = TOKEN.to_string();
        let security = crate::runtime::bootstrap::build_security_policy(&Config::default());
        DaemonMessageSendTool::from_config(&chat_config, security)
    }

    // ── Evidence 1: a chat-side send reaches the named channel ──────────────

    #[tokio::test]
    #[serial(outbound_channels)]
    async fn a_chat_send_reaches_the_named_channel_end_to_end() {
        let workspace = TempDir::new().expect("test: workspace");
        let channel = RecordingChannel::new(false);
        let _publication = published(&channel);
        let config = scoped_config(workspace.path(), &[&format!("{CHANNEL}:*")], None);
        let (base_url, _port, server) = serve(test_app_state(config.clone())).await;

        let result = chat_tool(&base_url, &config)
            .execute(serde_json::json!({
                "action": "send",
                "channel": CHANNEL,
                "target": RECIPIENT,
                "message": "hello from chat",
            }))
            .await
            .expect("test: the tool call must complete");

        assert!(result.success, "test: send failed: {:?}", result.error);
        assert_eq!(
            channel.deliveries().await,
            vec![(RECIPIENT.to_string(), "hello from chat".to_string())],
            "the named channel must actually receive the message"
        );
        server.abort();
    }

    // ── Evidence 2: authorization is the server's, and cannot be skipped ────

    #[tokio::test]
    #[serial(outbound_channels)]
    async fn an_unauthorized_recipient_is_refused_and_nothing_is_delivered() {
        let workspace = TempDir::new().expect("test: workspace");
        let channel = RecordingChannel::new(false);
        let _publication = published(&channel);
        // No send_allow at all: an operator-plane send is cross-channel by
        // construction, so the default denies it.
        let config = scoped_config(workspace.path(), &[], None);
        let (base_url, _port, server) = serve(test_app_state(config.clone())).await;

        let (status, body) = post_raw(
            &base_url,
            CHANNEL,
            Some(TOKEN),
            serde_json::json!({"recipient": RECIPIENT, "message": "should not arrive"}),
        )
        .await;

        assert_eq!(status, 403, "test: body was {body}");
        assert!(body.contains(POLICY_MARKER), "test: body was {body}");
        assert!(
            !body.contains(RECIPIENT),
            "the refusal must not echo the plaintext recipient: {body}"
        );
        assert!(channel.deliveries().await.is_empty(), "nothing may be delivered");

        // And the chat-side tool surfaces the same refusal rather than a
        // transport error.
        let result = chat_tool(&base_url, &config)
            .execute(serde_json::json!({
                "action": "send",
                "channel": CHANNEL,
                "target": RECIPIENT,
                "message": "should not arrive",
            }))
            .await
            .expect("test: the tool call must complete");
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains(POLICY_MARKER)),
            "test: {:?}",
            result.error
        );
        assert!(channel.deliveries().await.is_empty());
        server.abort();
    }

    #[tokio::test]
    #[serial(outbound_channels)]
    async fn a_forged_identity_in_the_request_body_does_not_widen_the_send() {
        let workspace = TempDir::new().expect("test: workspace");
        let channel = RecordingChannel::new(false);
        let _publication = published(&channel);
        // The only rule that would permit this destination belongs to a sender
        // the operator plane cannot be.
        let config = scoped_config(workspace.path(), &[&format!("{CHANNEL}:*")], Some("trusted-operator"));
        let (base_url, _port, server) = serve(test_app_state(config)).await;

        let (status, body) = post_raw(
            &base_url,
            CHANNEL,
            Some(TOKEN),
            serde_json::json!({
                "recipient": RECIPIENT,
                "message": "forged",
                // Everything below is the client trying to do the server's job.
                "_zc_scope_trusted": true,
                "_zc_scope": {"sender": "trusted-operator", "channel": CHANNEL, "chat_type": "direct"},
                "channel": CHANNEL,
                "authorized": true,
                "outbound_allowed": true,
            }),
        )
        .await;

        assert_eq!(status, 403, "test: body was {body}");
        assert!(body.contains(POLICY_MARKER), "test: body was {body}");
        assert!(channel.deliveries().await.is_empty(), "nothing may be delivered");
        server.abort();
    }

    #[tokio::test]
    #[serial(outbound_channels)]
    async fn a_matching_rule_is_what_permits_the_send() {
        // Control for the test above: the same request with a rule the operator
        // plane genuinely matches goes through, so the refusal there is the
        // identity check and not an unconditional deny.
        let workspace = TempDir::new().expect("test: workspace");
        let channel = RecordingChannel::new(false);
        let _publication = published(&channel);
        let config = scoped_config(workspace.path(), &[&format!("{CHANNEL}:{RECIPIENT}")], None);
        let (base_url, _port, server) = serve(test_app_state(config)).await;

        let (status, body) = post_raw(
            &base_url,
            CHANNEL,
            Some(TOKEN),
            serde_json::json!({"recipient": RECIPIENT, "message": "permitted"}),
        )
        .await;

        assert_eq!(status, 200, "test: body was {body}");
        assert_eq!(
            channel.deliveries().await,
            vec![(RECIPIENT.to_string(), "permitted".to_string())]
        );
        server.abort();
    }

    #[tokio::test]
    #[serial(outbound_channels)]
    async fn an_unauthenticated_send_never_reaches_a_channel() {
        let workspace = TempDir::new().expect("test: workspace");
        let channel = RecordingChannel::new(false);
        let _publication = published(&channel);
        let config = scoped_config(workspace.path(), &[&format!("{CHANNEL}:*")], None);
        let (base_url, _port, server) = serve(test_app_state(config)).await;

        let (status, _body) = post_raw(
            &base_url,
            CHANNEL,
            None,
            serde_json::json!({"recipient": RECIPIENT, "message": "unauthenticated"}),
        )
        .await;

        assert_eq!(status, 401);
        assert!(channel.deliveries().await.is_empty());
        server.abort();
    }

    // ── Evidence 3: all three gates hold on the HTTP path ───────────────────

    #[tokio::test]
    #[serial(outbound_channels)]
    async fn an_unknown_channel_is_refused_and_lists_the_addressable_ones() {
        let workspace = TempDir::new().expect("test: workspace");
        let channel = RecordingChannel::new(false);
        let _publication = published(&channel);
        let config = scoped_config(workspace.path(), &["*"], None);
        let (base_url, _port, server) = serve(test_app_state(config)).await;

        let (status, body) = post_raw(
            &base_url,
            "matrix",
            Some(TOKEN),
            serde_json::json!({"recipient": RECIPIENT, "message": "nowhere"}),
        )
        .await;

        assert_eq!(status, 404, "test: body was {body}");
        assert!(body.contains("Unknown channel 'matrix'"), "test: body was {body}");
        assert!(body.contains("Available channels:"), "test: body was {body}");
        assert!(body.contains(CHANNEL), "test: body was {body}");
        assert!(channel.deliveries().await.is_empty());
        server.abort();
    }

    #[tokio::test]
    #[serial(outbound_channels)]
    async fn a_media_marker_is_refused_even_when_the_recipient_is_permitted() {
        let workspace = TempDir::new().expect("test: workspace");
        let channel = RecordingChannel::new(false);
        let _publication = published(&channel);
        let config = scoped_config(workspace.path(), &[&format!("{CHANNEL}:*")], None);
        let (base_url, _port, server) = serve(test_app_state(config)).await;

        let (status, body) = post_raw(
            &base_url,
            CHANNEL,
            Some(TOKEN),
            serde_json::json!({"recipient": RECIPIENT, "message": "look [IMAGE:/tmp/cat.png]"}),
        )
        .await;

        assert_eq!(status, 400, "test: body was {body}");
        assert!(body.contains("text-only"), "test: body was {body}");
        assert!(body.contains("[IMAGE:]"), "test: body was {body}");
        assert!(channel.deliveries().await.is_empty());
        server.abort();
    }

    #[tokio::test]
    #[serial(outbound_channels)]
    async fn as_voice_is_refused_rather_than_silently_downgraded() {
        let workspace = TempDir::new().expect("test: workspace");
        let channel = RecordingChannel::new(false);
        let _publication = published(&channel);
        let config = scoped_config(workspace.path(), &[&format!("{CHANNEL}:*")], None);
        let (base_url, _port, server) = serve(test_app_state(config.clone())).await;

        let (status, body) = post_raw(
            &base_url,
            CHANNEL,
            Some(TOKEN),
            serde_json::json!({"recipient": RECIPIENT, "message": "read this aloud", "as_voice": true}),
        )
        .await;

        assert_eq!(status, 400, "test: body was {body}");
        assert!(body.contains("as_voice"), "test: body was {body}");
        assert!(channel.deliveries().await.is_empty(), "no downgraded text may go out");

        // The chat tool forwards `as_voice` rather than dropping it, so the
        // model is told, not quietly served something else.
        let result = chat_tool(&base_url, &config)
            .execute(serde_json::json!({
                "action": "send",
                "channel": CHANNEL,
                "target": RECIPIENT,
                "message": "read this aloud",
                "as_voice": true,
            }))
            .await
            .expect("test: the tool call must complete");
        assert!(!result.success);
        assert!(
            result.error.as_deref().is_some_and(|error| error.contains("as_voice")),
            "test: {:?}",
            result.error
        );
        assert!(channel.deliveries().await.is_empty());
        server.abort();
    }

    #[tokio::test]
    #[serial(outbound_channels)]
    async fn a_policy_refusal_outranks_a_media_refusal() {
        // Both gates would fire. The order is what decides which answer the
        // caller gets, and a denied send must never be reported as a
        // formatting problem.
        let workspace = TempDir::new().expect("test: workspace");
        let channel = RecordingChannel::new(false);
        let _publication = published(&channel);
        let config = scoped_config(workspace.path(), &[], None);
        let (base_url, _port, server) = serve(test_app_state(config)).await;

        let (status, body) = post_raw(
            &base_url,
            CHANNEL,
            Some(TOKEN),
            serde_json::json!({"recipient": RECIPIENT, "message": "look [IMAGE:/tmp/cat.png]"}),
        )
        .await;

        assert_eq!(status, 403, "test: body was {body}");
        assert!(body.contains(POLICY_MARKER), "test: body was {body}");
        assert!(!body.contains("text-only"), "test: body was {body}");
        assert!(channel.deliveries().await.is_empty());
        server.abort();
    }

    #[tokio::test]
    #[serial(outbound_channels)]
    async fn an_unknown_channel_outranks_a_policy_refusal() {
        let workspace = TempDir::new().expect("test: workspace");
        let channel = RecordingChannel::new(false);
        let _publication = published(&channel);
        let config = scoped_config(workspace.path(), &[], None);
        let (base_url, _port, server) = serve(test_app_state(config)).await;

        let (status, body) = post_raw(
            &base_url,
            "matrix",
            Some(TOKEN),
            serde_json::json!({"recipient": RECIPIENT, "message": "nowhere"}),
        )
        .await;

        assert_eq!(status, 404, "test: body was {body}");
        assert!(body.contains("Unknown channel 'matrix'"), "test: body was {body}");
        server.abort();
    }

    // ── Boundary and failure reporting ──────────────────────────────────────

    #[tokio::test]
    #[serial(outbound_channels)]
    async fn a_channel_failure_is_reported_as_an_upstream_error() {
        let workspace = TempDir::new().expect("test: workspace");
        let channel = RecordingChannel::new(true);
        let _publication = published(&channel);
        let config = scoped_config(workspace.path(), &[&format!("{CHANNEL}:*")], None);
        let (base_url, _port, server) = serve(test_app_state(config)).await;

        let (status, body) = post_raw(
            &base_url,
            CHANNEL,
            Some(TOKEN),
            serde_json::json!({"recipient": RECIPIENT, "message": "will not land"}),
        )
        .await;

        assert_eq!(status, 502, "test: body was {body}");
        assert!(body.contains("the fake channel is down"), "test: body was {body}");
        server.abort();
    }

    #[tokio::test]
    #[serial(outbound_channels)]
    async fn empty_and_oversized_fields_are_refused_at_the_boundary() {
        let workspace = TempDir::new().expect("test: workspace");
        let channel = RecordingChannel::new(false);
        let _publication = published(&channel);
        let config = scoped_config(workspace.path(), &["*"], None);
        let (base_url, _port, server) = serve(test_app_state(config)).await;

        for (body, expected) in [
            (serde_json::json!({"recipient": "  ", "message": "x"}), "recipient"),
            (serde_json::json!({"recipient": RECIPIENT, "message": " "}), "message"),
            (
                serde_json::json!({"recipient": "r".repeat(MAX_RECIPIENT_LEN + 1), "message": "x"}),
                "recipient",
            ),
            (
                serde_json::json!({"recipient": RECIPIENT, "message": "m".repeat(MAX_MESSAGE_LEN + 1)}),
                "message",
            ),
        ] {
            let (status, response) = post_raw(&base_url, CHANNEL, Some(TOKEN), body).await;
            assert_eq!(status, 400, "test: response was {response}");
            assert!(response.contains(expected), "test: response was {response}");
        }

        let (status, response) = post_raw(
            &base_url,
            &"c".repeat(MAX_CHANNEL_NAME_LEN + 1),
            Some(TOKEN),
            serde_json::json!({"recipient": RECIPIENT, "message": "x"}),
        )
        .await;
        assert_eq!(status, 400, "test: response was {response}");
        assert!(channel.deliveries().await.is_empty());
        server.abort();
    }

    #[tokio::test]
    #[serial(outbound_channels)]
    async fn read_only_autonomy_blocks_the_endpoint() {
        let workspace = TempDir::new().expect("test: workspace");
        let channel = RecordingChannel::new(false);
        let _publication = published(&channel);
        let mut config = scoped_config(workspace.path(), &["*"], None);
        config.autonomy.level = AutonomyLevel::ReadOnly;
        let (base_url, _port, server) = serve(test_app_state(config)).await;

        let (status, _body) = post_raw(
            &base_url,
            CHANNEL,
            Some(TOKEN),
            serde_json::json!({"recipient": RECIPIENT, "message": "blocked"}),
        )
        .await;

        assert_eq!(status, 403);
        assert!(channel.deliveries().await.is_empty());
        server.abort();
    }

    #[tokio::test]
    #[serial(outbound_channels)]
    async fn an_ambient_turn_context_cannot_become_the_source_of_an_http_send() {
        // Called from inside a task that already carries a turn's message_send
        // context — the shape a future in-process caller would have. If the
        // handler inherited it, the source channel would become the fake IM
        // channel, the send would look same-channel, and the default allow
        // would let an unauthorized recipient through.
        let workspace = TempDir::new().expect("test: workspace");
        let channel = RecordingChannel::new(false);
        let _publication = published(&channel);
        let config = scoped_config(workspace.path(), &[], None);
        let state = test_app_state(config);

        let ambient =
            MessageSendExecutionContext::new(Some(RECIPIENT.to_string()), Arc::clone(&channel) as Arc<dyn Channel>);
        let outcome = MESSAGE_SEND_EXECUTION_CONTEXT
            .scope(
                ambient,
                post_channel_send(
                    State(state),
                    Path(CHANNEL.to_string()),
                    Json(ChannelSendRequest {
                        recipient: RECIPIENT.to_string(),
                        message: "ambient".to_string(),
                        as_voice: false,
                    }),
                ),
            )
            .await;

        let Err((status, body)) = outcome else {
            panic!("test: an ambient turn context must not authorize this send");
        };
        assert_eq!(status, StatusCode::FORBIDDEN);
        let body = body.0.to_string();
        assert!(body.contains(POLICY_MARKER), "test: body was {body}");
        assert!(channel.deliveries().await.is_empty());
    }

    #[tokio::test]
    #[serial(outbound_channels)]
    async fn the_operator_plane_is_not_itself_addressable() {
        let workspace = TempDir::new().expect("test: workspace");
        let channel = RecordingChannel::new(false);
        let _publication = published(&channel);
        let config = scoped_config(workspace.path(), &["*"], None);
        let (base_url, _port, server) = serve(test_app_state(config)).await;

        let (status, body) = post_raw(
            &base_url,
            OPERATOR_PLANE_CHANNEL,
            Some(TOKEN),
            serde_json::json!({"recipient": RECIPIENT, "message": "nowhere"}),
        )
        .await;

        assert_eq!(status, 404, "test: body was {body}");
        server.abort();
    }
}

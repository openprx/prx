use super::*;
use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
use crate::security::{AutonomyLevel, SecurityPolicy};
use async_trait::async_trait;

struct DummyChannel {
    pub name: &'static str,
    pub sent: Arc<tokio::sync::Mutex<Vec<String>>>,
    pub recipients: Arc<tokio::sync::Mutex<Vec<String>>>,
}

impl DummyChannel {
    fn new() -> (Arc<Self>, Arc<tokio::sync::Mutex<Vec<String>>>) {
        let (channel, sent, _recipients) = Self::new_named("dummy");
        (channel, sent)
    }

    fn new_named(
        name: &'static str,
    ) -> (
        Arc<Self>,
        Arc<tokio::sync::Mutex<Vec<String>>>,
        Arc<tokio::sync::Mutex<Vec<String>>>,
    ) {
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let recipients = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                name,
                sent: sent.clone(),
                recipients: recipients.clone(),
            }),
            sent,
            recipients,
        )
    }
}

#[async_trait]
impl Channel for DummyChannel {
    fn name(&self) -> &str {
        self.name
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        self.sent.lock().await.push(message.content.clone());
        self.recipients.lock().await.push(message.recipient.clone());
        Ok(())
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        Ok(())
    }
}

fn test_security(level: AutonomyLevel) -> Arc<SecurityPolicy> {
    Arc::new(SecurityPolicy {
        autonomy: level,
        workspace_dir: std::env::temp_dir(),
        ..SecurityPolicy::default()
    })
}

#[test]
fn tool_name_and_description() {
    let (ch, _) = DummyChannel::new();
    let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));
    assert_eq!(tool.name(), "message_send");
    assert!(!tool.description().is_empty(), "expected a non-empty value");
}

#[test]
fn parameters_schema_defaults_the_action_and_binds_its_contract() {
    let (ch, _) = DummyChannel::new();
    let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));
    let schema = tool.parameters_schema();
    assert_eq!(schema["type"], "object");
    // The executor treats an omitted action as 'send', so the contract
    // publishes that default instead of demanding the discriminator.
    let required = schema["required"].as_array().unwrap();
    assert!(!required.iter().any(|v| v.as_str() == Some("action")));
    assert_eq!(schema["properties"]["action"]["default"], "send");
    assert!(
        !crate::tools::schema::validate_tool_arguments(&schema, &json!({"action": "send"})).is_empty(),
        "expected a non-empty value"
    );
    assert!(
        !crate::tools::schema::validate_tool_arguments(&schema, &json!({})).is_empty(),
        "expected a non-empty value"
    );
    assert!(
        crate::tools::schema::validate_tool_arguments(&schema, &json!({"action": "send", "message": "hello"}))
            .is_empty(),
        "expected an empty value"
    );
    assert!(
        crate::tools::schema::validate_tool_arguments(&schema, &json!({"message": "hello"})).is_empty(),
        "expected an empty value"
    );
}

/// Both message_send schemas owe the model the same thing `cron` and
/// `sessions_spawn` owe it: a parameter an action cannot be called without
/// has to name that action.
#[test]
fn schemas_name_every_action_in_the_parameters_they_require() {
    crate::tools::schema::action_contract::assert_action_tool_contract(
        "message_send",
        CHANNEL_MESSAGE_SEND_DESCRIPTION,
        &channel_message_send_parameters_schema(),
    );
    crate::tools::schema::action_contract::assert_action_tool_contract(
        "message_send (daemon)",
        DAEMON_MESSAGE_SEND_DESCRIPTION,
        &daemon_message_send_parameters_schema(),
    );
}

#[test]
fn daemon_schema_exposes_only_complete_send_calls() {
    let schema = daemon_message_send_parameters_schema();
    assert_eq!(schema["properties"]["action"]["enum"], json!(["send"]));
    assert!(
        !crate::tools::schema::validate_tool_arguments(
            &schema,
            &json!({"action": "send", "channel": "wacli", "target": "owner"})
        )
        .is_empty(),
        "expected a non-empty value"
    );
    assert!(
        crate::tools::schema::validate_tool_arguments(
            &schema,
            &json!({"action": "send", "channel": "wacli", "target": "owner", "message": "hello"})
        )
        .is_empty(),
        "expected an empty value"
    );
    assert!(
        !crate::tools::schema::validate_tool_arguments(
            &schema,
            &json!({"action": "react", "channel": "wacli", "target": "owner", "message": "hello"})
        )
        .is_empty(),
        "expected a non-empty value"
    );
}

#[tokio::test]
async fn send_action_delivers_message() {
    let (ch, sent) = DummyChannel::new();
    let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));

    let result = tool
        .execute(json!({
            "action": "send",
            "target": "+15551234567",
            "message": "Hello from PRX!"
        }))
        .await
        .unwrap();

    assert!(result.success, "Expected success, got: {:?}", result.error);
    let msgs = sent.lock().await;
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0], "Hello from PRX!");
}

#[tokio::test]
async fn send_uses_default_recipient_when_target_omitted() {
    let (ch, sent) = DummyChannel::new();
    let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));
    tool.set_default_recipient(Some("+19998887777".to_string())).await;

    let result = tool
        .execute(json!({
            "action": "send",
            "message": "Using default recipient"
        }))
        .await
        .unwrap();

    assert!(result.success, "Expected success, got: {:?}", result.error);
    let msgs = sent.lock().await;
    assert_eq!(msgs.len(), 1);
}

#[tokio::test]
async fn task_local_context_beats_mutated_fallback_defaults() {
    let (channel_a, sent_a, recipients_a) = DummyChannel::new_named("channel-a");
    let (channel_b, sent_b, recipients_b) = DummyChannel::new_named("channel-b");
    let tool = MessageSendTool::new(channel_a.clone(), test_security(AutonomyLevel::Full));

    let turn_a_context = MessageSendExecutionContext::new(
        Some("recipient-a".to_string()),
        channel_a as Arc<dyn crate::channels::traits::Channel>,
    );

    let result = MESSAGE_SEND_EXECUTION_CONTEXT
        .scope(turn_a_context, async {
            // Simulate another inbound turn updating the legacy fallback while
            // turn A is still in progress. The send below omits `target`; it
            // must still route to A's task-local recipient/channel.
            tool.set_default_recipient(Some("recipient-b".to_string())).await;
            tool.set_active_channel(channel_b as Arc<dyn crate::channels::traits::Channel>)
                .await;
            tool.execute(json!({
                "action": "send",
                "message": "reply from A"
            }))
            .await
        })
        .await
        .unwrap();

    assert!(result.success, "Expected success, got: {:?}", result.error);
    assert_eq!(&*sent_a.lock().await, &["reply from A".to_string()]);
    assert_eq!(&*recipients_a.lock().await, &["recipient-a".to_string()]);
    assert!(
        sent_b.lock().await.is_empty(),
        "fallback channel must not receive turn A send"
    );
    assert!(
        recipients_b.lock().await.is_empty(),
        "fallback recipient must not receive turn A send"
    );
}

#[tokio::test]
async fn fallback_default_still_works_without_turn_context() {
    let (ch, sent, recipients) = DummyChannel::new_named("fallback");
    let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));
    tool.set_default_recipient(Some("fallback-recipient".to_string())).await;

    let result = tool
        .execute(json!({
            "action": "send",
            "message": "fallback send"
        }))
        .await
        .unwrap();

    assert!(result.success, "Expected success, got: {:?}", result.error);
    assert_eq!(&*sent.lock().await, &["fallback send".to_string()]);
    assert_eq!(&*recipients.lock().await, &["fallback-recipient".to_string()]);
}

#[tokio::test]
async fn send_fails_without_target_and_no_default() {
    let (ch, _) = DummyChannel::new();
    let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));

    let result = tool
        .execute(json!({ "action": "send", "message": "no target" }))
        .await
        .unwrap();

    assert!(!result.success);
    assert!(result.error.unwrap().contains("target"));
}

#[tokio::test]
async fn react_fails_without_signal_channel() {
    let (ch, _) = DummyChannel::new();
    let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));

    let result = tool
        .execute(json!({
            "action": "react",
            "target": "+15551234567",
            "emoji": "👍",
            "target_author": "+10001112222",
            "target_timestamp": 1_700_000_000_000u64
        }))
        .await
        .unwrap();

    assert!(!result.success);
    assert!(result.error.unwrap().contains("Signal"));
}

#[tokio::test]
async fn react_fails_missing_emoji() {
    let (ch, _) = DummyChannel::new();
    let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));

    let result = tool
        .execute(json!({
            "action": "react",
            "target": "+15551234567",
            "target_author": "+10001112222",
            "target_timestamp": 1_700_000_000_000u64
        }))
        .await
        .unwrap();

    assert!(!result.success);
    assert!(result.error.unwrap().contains("emoji"));
}

#[tokio::test]
async fn unknown_action_returns_error() {
    let (ch, _) = DummyChannel::new();
    let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));

    let result = tool.execute(json!({ "action": "destroy" })).await.unwrap();

    assert!(!result.success);
    assert!(result.error.unwrap().contains("Unknown action"));
}

#[tokio::test]
async fn execute_blocks_readonly_mode() {
    let (ch, _) = DummyChannel::new();
    let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::ReadOnly));

    let result = tool
        .execute(json!({
            "action": "send",
            "target": "+15551234567",
            "message": "test"
        }))
        .await
        .unwrap();

    assert!(!result.success);
    assert!(result.error.unwrap().contains("read-only"));
}

#[tokio::test]
async fn default_recipient_handle_allows_external_update() {
    let (ch, sent) = DummyChannel::new();
    let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));
    let handle = tool.default_recipient_handle();

    // Update via handle (as the gateway would do per-message)
    *handle.write().await = Some("+19998887777".to_string());

    let result = tool
        .execute(json!({ "action": "send", "message": "via handle" }))
        .await
        .unwrap();

    assert!(result.success, "Expected success, got: {:?}", result.error);
    let msgs = sent.lock().await;
    assert_eq!(msgs.len(), 1);
}

/// Regression test: verify that set_active_channel routes messages through the new channel
/// instead of the original channel. This covers the bug where WhatsApp messages were
/// routed through Signal because MessageSendTool used a fixed channel field.
#[tokio::test]
async fn set_active_channel_routes_to_new_channel() {
    let (original_ch, original_sent) = DummyChannel::new();
    let tool = MessageSendTool::new(original_ch, test_security(AutonomyLevel::Full));
    tool.set_active_recipient("+15551234567").await;

    // First send goes to original channel
    let _ = tool
        .execute(json!({ "action": "send", "message": "first" }))
        .await
        .unwrap();
    assert_eq!(
        original_sent.lock().await.len(),
        1,
        "first send should go to original channel"
    );

    // Simulate wacli message arriving: gateway switches active channel
    let (new_ch, new_sent) = DummyChannel::new();
    let new_ch_arc: Arc<dyn crate::channels::traits::Channel> = new_ch;
    tool.set_active_channel(new_ch_arc).await;

    // Second send should now go to the new channel, not the original
    let result = tool
        .execute(json!({ "action": "send", "message": "second" }))
        .await
        .unwrap();
    assert!(result.success, "Expected success on second send: {:?}", result.error);
    assert_eq!(
        original_sent.lock().await.len(),
        1,
        "original channel should still have only 1 message"
    );
    assert_eq!(
        new_sent.lock().await.len(),
        1,
        "new channel should have received the second message"
    );
}

// ── Outbound recipient authorization (plan a1) ──────────────────────────

const DENIED_RECIPIENT: &str = "+15550001111";
const POLICY_ERROR_MARKER: &str = "not permitted by the configured scope rules";

fn scoped_security(send_allow: &[&str], send_deny: &[&str]) -> Arc<SecurityPolicy> {
    Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        workspace_dir: std::env::temp_dir(),
        scope_rules: vec![crate::config::ScopeRule {
            user: None,
            channel: None,
            chat_type: None,
            tools_allow: vec![],
            tools_deny: vec![],
            send_allow: send_allow.iter().map(|s| (*s).to_string()).collect(),
            send_deny: send_deny.iter().map(|s| (*s).to_string()).collect(),
            assign_allow: vec![],
            assign_deny: vec![],
        }],
        ..SecurityPolicy::default()
    })
}

/// Every action that reaches a recipient, with a target that a `send_deny`
/// entry can match.
fn denied_recipient_actions() -> Vec<serde_json::Value> {
    vec![
        json!({"action": "send", "target": DENIED_RECIPIENT, "message": "hi"}),
        json!({
            "action": "react", "target": DENIED_RECIPIENT, "emoji": "\u{1F44D}",
            "target_author": "+19998887777", "target_timestamp": 1_700_000_000_000u64
        }),
        json!({"action": "edit", "target": DENIED_RECIPIENT, "message_id": "1", "message": "hi"}),
        json!({"action": "delete", "target": DENIED_RECIPIENT, "message_id": "1"}),
        json!({"action": "unsend", "target": DENIED_RECIPIENT, "message_id": "1"}),
        json!({"action": "thread", "target": DENIED_RECIPIENT, "thread_id": "t1", "message": "hi"}),
    ]
}

#[tokio::test]
async fn send_deny_blocks_every_recipient_action_without_leaking_the_recipient() {
    let (ch, sent) = DummyChannel::new();
    let tool = MessageSendTool::new(ch, scoped_security(&[], &["*:+15550001111"]));

    for args in denied_recipient_actions() {
        let action = args["action"].as_str().unwrap().to_string();
        let result = tool.execute(args).await.unwrap();
        assert!(!result.success, "action {action} must be denied");
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains(POLICY_ERROR_MARKER),
            "action {action} must fail on the outbound policy, got: {error}"
        );
        assert!(
            !error.contains(DENIED_RECIPIENT),
            "action {action} error must not leak the plaintext recipient: {error}"
        );
    }
    assert!(sent.lock().await.is_empty(), "no message may reach the channel");
}

// ── R3: the turn's origin channel on channel-less surfaces ─────────────

fn origin_scoped_security(channel: &str, send_deny: &[&str]) -> Arc<SecurityPolicy> {
    Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        workspace_dir: std::env::temp_dir(),
        scope_rules: vec![crate::config::ScopeRule {
            user: None,
            channel: Some(channel.to_string()),
            chat_type: None,
            tools_allow: vec![],
            tools_deny: vec![],
            send_allow: vec![],
            send_deny: send_deny.iter().map(|s| (*s).to_string()).collect(),
            assign_allow: vec![],
            assign_deny: vec![],
        }],
        ..SecurityPolicy::default()
    })
}

/// A gateway webhook turn replies through whichever channel handle the
/// daemon registered — `signal` in production. Without an origin, a rule
/// written for the surface the message really came from never matches.
#[tokio::test]
async fn named_origin_channel_brings_that_channel_s_rules_into_play() {
    let (reply_handle, sent, _recipients) = DummyChannel::new_named("signal");
    let tool = MessageSendTool::new(reply_handle, origin_scoped_security("whatsapp", &["*:+15550001111"]));
    let args = json!({"action": "send", "target": "+15550001111", "message": "hi"});

    // Without the origin the turn reads as a signal turn and the whatsapp
    // rule is never consulted.
    let unnamed = tool.execute(args.clone()).await.unwrap();
    assert!(
        unnamed.success,
        "baseline: the rule does not match a signal-origin turn"
    );
    sent.lock().await.clear();

    let named = with_outbound_origin_channel("whatsapp", tool.execute(args))
        .await
        .unwrap();
    assert!(!named.success, "a whatsapp-origin turn must hit the whatsapp rule");
    let error = named.error.unwrap_or_default();
    assert!(error.contains(POLICY_ERROR_MARKER), "got: {error}");
    assert!(sent.lock().await.is_empty(), "no message may reach the channel");
}

/// Zero-regression guard for R3: naming an origin that differs from the
/// reply handle must not mute a turn no rule constrains. The same-channel
/// default keeps measuring against the channel the turn replies through.
#[tokio::test]
async fn named_origin_channel_does_not_mute_an_unconstrained_turn() {
    let (reply_handle, sent, _recipients) = DummyChannel::new_named("signal");
    let tool = MessageSendTool::new(reply_handle, test_security(AutonomyLevel::Full));

    for args in denied_recipient_actions() {
        let action = args["action"].as_str().unwrap().to_string();
        let result = with_outbound_origin_channel("webhook", tool.execute(args))
            .await
            .unwrap();
        let error = result.error.unwrap_or_default();
        assert!(
            !error.contains(POLICY_ERROR_MARKER),
            "action {action} must not be refused by the outbound policy: {error}"
        );
    }
    assert!(!sent.lock().await.is_empty(), "the send must still reach the channel");
}

/// The same holds with a rule present that simply does not constrain the
/// destination: a `webhook`-origin turn still reaches its reply channel.
#[tokio::test]
async fn named_origin_channel_still_reaches_the_reply_channel() {
    let (reply_handle, sent, _recipients) = DummyChannel::new_named("signal");
    let tool = MessageSendTool::new(reply_handle, origin_scoped_security("webhook", &["telegram:*"]));
    let args = json!({"action": "send", "target": "+15550001111", "message": "hi"});

    let result = with_outbound_origin_channel("webhook", tool.execute(args))
        .await
        .unwrap();
    assert!(result.success, "got: {:?}", result.error);
    assert_eq!(sent.lock().await.len(), 1);
}

/// Zero-regression guard: with no scope rules configured, not one action
/// changes behaviour — in particular none of them can fail on the new
/// outbound decision point.
#[tokio::test]
async fn without_scope_rules_no_action_hits_the_outbound_policy() {
    let (ch, sent) = DummyChannel::new();
    let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));

    for args in denied_recipient_actions() {
        let action = args["action"].as_str().unwrap().to_string();
        let result = tool.execute(args).await.unwrap();
        let error = result.error.clone().unwrap_or_default();
        assert!(
            !error.contains(POLICY_ERROR_MARKER),
            "action {action} must not be touched by the outbound policy, got: {error}"
        );
    }
    // `send` is the only action DummyChannel implements; it still went out.
    assert_eq!(sent.lock().await.len(), 1);
}

#[tokio::test]
async fn send_deny_outranks_send_allow() {
    let (ch, sent) = DummyChannel::new();
    let tool = MessageSendTool::new(ch, scoped_security(&["*"], &["*:+15550001111"]));

    let denied = tool
        .execute(json!({"action": "send", "target": DENIED_RECIPIENT, "message": "hi"}))
        .await
        .unwrap();
    assert!(!denied.success);
    assert!(denied.error.unwrap_or_default().contains(POLICY_ERROR_MARKER));

    // The wildcard allow still lets every other recipient through.
    let allowed = tool
        .execute(json!({"action": "send", "target": "+15550002222", "message": "hi"}))
        .await
        .unwrap();
    assert!(allowed.success, "got: {:?}", allowed.error);
    assert_eq!(sent.lock().await.len(), 1);
}

// ── Explicit destination channel (plan a2) ─────────────────────────────

/// Build the shared channel registry the way the runtime does: keyed by
/// [`Channel::name`], the same `Arc` `sessions_spawn` receives.
fn registry(channels: &[Arc<DummyChannel>]) -> Arc<HashMap<String, Arc<dyn Channel>>> {
    Arc::new(
        channels
            .iter()
            .map(|ch| (ch.name().to_string(), Arc::clone(ch) as Arc<dyn Channel>))
            .collect(),
    )
}

fn registry_with(channels: Vec<Arc<dyn Channel>>) -> Arc<HashMap<String, Arc<dyn Channel>>> {
    Arc::new(channels.into_iter().map(|ch| (ch.name().to_string(), ch)).collect())
}

/// A channel whose outbound parser knows a syntax no shared regex does.
///
/// Stands in for the next channel someone adds: the gate has never heard of
/// `<<attach …>>`, so the only way this can be refused is by asking the
/// channel itself.
struct ExoticChannel {
    name: &'static str,
    sent: Arc<tokio::sync::Mutex<Vec<String>>>,
    recipients: Arc<tokio::sync::Mutex<Vec<String>>>,
}

impl ExoticChannel {
    fn new_named(
        name: &'static str,
    ) -> (
        Arc<Self>,
        Arc<tokio::sync::Mutex<Vec<String>>>,
        Arc<tokio::sync::Mutex<Vec<String>>>,
    ) {
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let recipients = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                name,
                sent: sent.clone(),
                recipients: recipients.clone(),
            }),
            sent,
            recipients,
        )
    }
}

#[async_trait]
impl Channel for ExoticChannel {
    fn name(&self) -> &str {
        self.name
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        self.sent.lock().await.push(message.content.clone());
        self.recipients.lock().await.push(message.recipient.clone());
        Ok(())
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        Ok(())
    }

    fn outbound_attachment(&self, text: &str) -> Option<crate::channels::traits::OutboundAttachment> {
        text.contains("<<attach ")
            .then_some(crate::channels::traits::OutboundAttachment::BarePath)
    }
}

/// A turn channel plus a second configured channel, wired into one registry.
#[allow(clippy::type_complexity)]
fn two_channel_tool(
    security: Arc<SecurityPolicy>,
) -> (
    MessageSendTool,
    Arc<tokio::sync::Mutex<Vec<String>>>,
    Arc<tokio::sync::Mutex<Vec<String>>>,
    Arc<tokio::sync::Mutex<Vec<String>>>,
) {
    let (current, current_sent, _current_recipients) = DummyChannel::new_named("dummy");
    let (other, other_sent, other_recipients) = DummyChannel::new_named("telegram");
    let tool = MessageSendTool::new(Arc::clone(&current) as Arc<dyn Channel>, security)
        .with_channels(registry(&[current, other]));
    (tool, current_sent, other_sent, other_recipients)
}

/// Evidence 2: an authorized explicit `channel` really hands the message to
/// that channel object — and to no other.
#[tokio::test]
async fn explicit_channel_delivers_to_the_named_channel() {
    let (tool, current_sent, other_sent, other_recipients) =
        two_channel_tool(scoped_security(&["telegram:*"], &[]));

    let result = tool
        .execute(json!({
            "action": "send", "channel": "telegram",
            "target": "12345", "message": "across the wire"
        }))
        .await
        .unwrap();

    assert!(result.success, "got: {:?}", result.error);
    assert_eq!(&*other_sent.lock().await, &["across the wire".to_string()]);
    assert_eq!(&*other_recipients.lock().await, &["12345".to_string()]);
    assert!(
        current_sent.lock().await.is_empty(),
        "the turn's own channel must not also receive a cross-channel send"
    );
}

/// Evidence 3 — the load-bearing one. An unauthorized cross-channel send is
/// refused outright; nothing may leak onto the turn's own channel, because a
/// fallback would dress a denied send up as a delivered one.
#[tokio::test]
async fn unauthorized_cross_channel_send_never_falls_back_to_the_current_channel() {
    // No scope rules at all: same-channel stays allowed, cross-channel denied.
    let (tool, current_sent, other_sent, other_recipients) =
        two_channel_tool(test_security(AutonomyLevel::Supervised));

    let result = tool
        .execute(json!({
            "action": "send", "channel": "telegram",
            "target": "12345", "message": "must not be delivered"
        }))
        .await
        .unwrap();

    assert!(!result.success, "cross-channel send must be denied by default");
    let error = result.error.unwrap_or_default();
    assert!(error.contains(POLICY_ERROR_MARKER), "got: {error}");
    assert!(
        current_sent.lock().await.is_empty(),
        "denied cross-channel send must not fall back to the current channel"
    );
    assert!(other_sent.lock().await.is_empty());
    assert!(other_recipients.lock().await.is_empty());
}

/// Same guarantee for every other recipient-bearing action that can be routed.
#[tokio::test]
async fn unauthorized_cross_channel_is_refused_for_every_routable_action() {
    let (tool, current_sent, other_sent, _other_recipients) =
        two_channel_tool(test_security(AutonomyLevel::Supervised));

    for args in [
        json!({"action": "send", "channel": "telegram", "target": "1", "message": "x"}),
        json!({"action": "edit", "channel": "telegram", "target": "1", "message_id": "9", "message": "x"}),
        json!({"action": "delete", "channel": "telegram", "target": "1", "message_id": "9"}),
        json!({"action": "unsend", "channel": "telegram", "target": "1", "message_id": "9"}),
        json!({"action": "thread", "channel": "telegram", "target": "1", "thread_id": "t", "message": "x"}),
    ] {
        let action = args["action"].as_str().unwrap_or_default().to_string();
        let result = tool.execute(args).await.unwrap();
        assert!(!result.success, "action {action} must be denied");
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains(POLICY_ERROR_MARKER),
            "action {action} must fail on the outbound policy, got: {error}"
        );
    }
    assert!(current_sent.lock().await.is_empty());
    assert!(other_sent.lock().await.is_empty());
}

/// Evidence 4: an unknown channel name is an error that names what *is*
/// addressable — never a silent send on the current channel.
#[tokio::test]
async fn unknown_channel_name_is_rejected_and_lists_the_available_channels() {
    let (tool, current_sent, other_sent, _other_recipients) = two_channel_tool(scoped_security(&["*"], &[]));

    let result = tool
        .execute(json!({
            "action": "send", "channel": "carrier-pigeon", "target": "12345", "message": "hi"
        }))
        .await
        .unwrap();

    assert!(!result.success);
    let error = result.error.unwrap_or_default();
    assert!(error.contains("Unknown channel 'carrier-pigeon'"), "got: {error}");
    assert!(error.contains("Available channels: dummy, telegram"), "got: {error}");
    assert!(
        current_sent.lock().await.is_empty(),
        "unknown channel must not fall back"
    );
    assert!(other_sent.lock().await.is_empty());
}

/// Without an injected registry only the turn's own channel is addressable,
/// and the error says so instead of pretending the send went out.
#[tokio::test]
async fn without_a_registry_only_the_turn_channel_is_addressable() {
    let (ch, sent) = DummyChannel::new();
    let tool = MessageSendTool::new(ch, scoped_security(&["*"], &[]));

    let result = tool
        .execute(json!({
            "action": "send", "channel": "telegram", "target": "12345", "message": "hi"
        }))
        .await
        .unwrap();

    assert!(!result.success);
    let error = result.error.unwrap_or_default();
    assert!(error.contains("Unknown channel 'telegram'"), "got: {error}");
    assert!(error.contains("Available channels: dummy"), "got: {error}");
    assert!(sent.lock().await.is_empty());
}

/// Evidence 5: cross-channel carries text only. A media marker is refused
/// with the reason, not quietly stripped or shipped as a bare local path.
#[tokio::test]
async fn cross_channel_media_marker_is_refused_with_a_reason() {
    let (tool, current_sent, other_sent, _other_recipients) = two_channel_tool(scoped_security(&["*"], &[]));

    let result = tool
        .execute(json!({
            "action": "send", "channel": "telegram", "target": "12345",
            "message": "look at this [IMAGE:/tmp/cat.png]"
        }))
        .await
        .unwrap();

    assert!(!result.success, "cross-channel media must be refused");
    let error = result.error.unwrap_or_default();
    assert!(error.contains("text-only"), "got: {error}");
    assert!(error.contains("[IMAGE:]"), "got: {error}");
    assert!(current_sent.lock().await.is_empty());
    assert!(other_sent.lock().await.is_empty());
}

/// `as_voice` synthesises a local file, so it is media too.
#[tokio::test]
async fn cross_channel_as_voice_is_refused() {
    let (tool, current_sent, other_sent, _other_recipients) = two_channel_tool(scoped_security(&["*"], &[]));

    let result = tool
        .execute(json!({
            "action": "send", "channel": "telegram", "target": "12345",
            "message": "read this out", "as_voice": true
        }))
        .await
        .unwrap();

    assert!(!result.success);
    assert!(result.error.unwrap_or_default().contains("as_voice"));
    assert!(current_sent.lock().await.is_empty());
    assert!(other_sent.lock().await.is_empty());
}

/// The bug this gate had: Telegram's parser reads far more than the shared
/// marker regex does, so every spelling it accepts had to be spelled the
/// regex's way to be refused. Lower-case kinds, the `PHOTO`/`FILE` aliases
/// and a bare path with no marker at all all reached Telegram intact and
/// were uploaded from the local disk.
#[tokio::test]
async fn cross_channel_refuses_every_spelling_the_destination_would_upload() {
    for message in [
        "look at this [image:/etc/shadow]",
        "look at this [ImAgE:/etc/shadow]",
        "look at this [photo:/etc/shadow]",
        "look at this [PHOTO:/etc/shadow]",
        "look at this [file:/etc/shadow]",
        "look at this [FILE:/etc/shadow]",
        "look at this [document:/etc/shadow]",
        "look at this [voice:/etc/shadow]",
        "/etc/shadow",
        "  /etc/passwd  ",
        "`/etc/passwd`",
        "file:///etc/passwd",
        "secrets.env",
    ] {
        let (tool, current_sent, other_sent, _other_recipients) = two_channel_tool(scoped_security(&["*"], &[]));
        let result = tool
            .execute(json!({
                "action": "send", "channel": "telegram", "target": "12345", "message": message
            }))
            .await
            .unwrap();

        assert!(!result.success, "must be refused: {message}");
        let error = result.error.unwrap_or_default();
        assert!(error.contains("text-only"), "got: {error}");
        assert!(
            !error.contains("/etc/shadow") && !error.contains("/etc/passwd"),
            "refusal must not echo the local path: {error}"
        );
        assert!(
            current_sent.lock().await.is_empty(),
            "leaked on the turn channel: {message}"
        );
        assert!(other_sent.lock().await.is_empty(), "leaked cross-channel: {message}");
    }
}

/// Plain prose still crosses. The gate is default-deny about attachments,
/// not about cross-channel sends.
#[tokio::test]
async fn cross_channel_plain_text_still_crosses() {
    for message in [
        "the build is green",
        "see the report I mentioned",
        "an [UNKNOWN:/etc/shadow] marker nobody resolves",
        "done",
    ] {
        let (tool, _current_sent, other_sent, _other_recipients) = two_channel_tool(scoped_security(&["*"], &[]));
        let result = tool
            .execute(json!({
                "action": "send", "channel": "telegram", "target": "12345", "message": message
            }))
            .await
            .unwrap();

        assert!(result.success, "must cross: {message} — {:?}", result.error);
        assert_eq!(&*other_sent.lock().await, &[message.to_string()]);
    }
}

/// A channel with a private attachment syntax nobody else knows is still
/// refused, because the gate asks *it* rather than pattern-matching for it.
/// This is what stops the next channel from re-opening the same hole.
#[tokio::test]
async fn cross_channel_asks_the_destination_channel_for_its_own_verdict() {
    let (current, current_sent, _current_recipients) = DummyChannel::new_named("dummy");
    let (other, other_sent, _other_recipients) = ExoticChannel::new_named("exotic");
    let tool =
        MessageSendTool::new(Arc::clone(&current) as Arc<dyn Channel>, scoped_security(&["*"], &[])).with_channels(
            registry_with(vec![current as Arc<dyn Channel>, other as Arc<dyn Channel>]),
        );

    let result = tool
        .execute(json!({
            "action": "send", "channel": "exotic", "target": "12345",
            "message": "here you go <<attach /etc/shadow>>"
        }))
        .await
        .unwrap();

    assert!(!result.success, "the destination said it would upload a file");
    let error = result.error.unwrap_or_default();
    assert!(error.contains("text-only"), "got: {error}");
    assert!(
        !error.contains("/etc/shadow"),
        "refusal must not echo the path: {error}"
    );
    assert!(current_sent.lock().await.is_empty());
    assert!(other_sent.lock().await.is_empty());
}

/// A media marker on the turn's *own* channel is untouched by the guard —
/// the restriction is about crossing channels, not about media.
#[tokio::test]
async fn same_channel_media_marker_is_unaffected() {
    let (tool, current_sent, other_sent, _other_recipients) = two_channel_tool(test_security(AutonomyLevel::Full));

    for args in [
        json!({"action": "send", "target": "12345", "message": "pic [IMAGE:/tmp/cat.png]"}),
        json!({"action": "send", "channel": "dummy", "target": "12345", "message": "pic [IMAGE:/tmp/cat.png]"}),
    ] {
        let result = tool.execute(args).await.unwrap();
        assert!(result.success, "got: {:?}", result.error);
    }
    assert_eq!(current_sent.lock().await.len(), 2);
    assert!(other_sent.lock().await.is_empty());
}

/// Evidence 1: with a registry injected but no `channel` argument, routing is
/// byte-for-byte what it was — and naming the turn's own channel is a no-op.
#[tokio::test]
async fn omitting_channel_keeps_the_existing_routing_verbatim() {
    let (tool, current_sent, other_sent, other_recipients) = two_channel_tool(test_security(AutonomyLevel::Full));

    for args in [
        json!({"action": "send", "target": "12345", "message": "implicit"}),
        json!({"action": "send", "channel": "dummy", "target": "12345", "message": "explicit-same"}),
        json!({"action": "send", "channel": "", "target": "12345", "message": "blank-is-absent"}),
    ] {
        let result = tool.execute(args).await.unwrap();
        assert!(result.success, "got: {:?}", result.error);
    }

    assert_eq!(
        &*current_sent.lock().await,
        &[
            "implicit".to_string(),
            "explicit-same".to_string(),
            "blank-is-absent".to_string()
        ]
    );
    assert!(other_sent.lock().await.is_empty());
    assert!(other_recipients.lock().await.is_empty());
}

/// The turn's task-local channel — not the construction-time slot — is what
/// `channel` is measured against, so a concurrent turn cannot redefine what
/// "the current channel" means for this call.
#[tokio::test]
async fn explicit_channel_is_measured_against_the_task_local_turn_channel() {
    let (current, current_sent, _r1) = DummyChannel::new_named("dummy");
    let (other, other_sent, _r2) = DummyChannel::new_named("telegram");
    let tool = MessageSendTool::new(
        Arc::clone(&other) as Arc<dyn Channel>,
        test_security(AutonomyLevel::Full),
    )
    .with_channels(registry(&[Arc::clone(&current), Arc::clone(&other)]));

    // The turn is anchored to "dummy" even though the tool was built on
    // "telegram"; asking for "dummy" must therefore stay same-channel.
    let context =
        MessageSendExecutionContext::new(Some("12345".to_string()), Arc::clone(&current) as Arc<dyn Channel>);
    let result = MESSAGE_SEND_EXECUTION_CONTEXT
        .scope(context, async {
            tool.execute(json!({"action": "send", "channel": "dummy", "message": "in-turn"}))
                .await
        })
        .await
        .unwrap();

    assert!(result.success, "got: {:?}", result.error);
    assert_eq!(&*current_sent.lock().await, &["in-turn".to_string()]);
    assert!(other_sent.lock().await.is_empty());
}

/// `react` is delivered by the Signal handle, not by the resolved channel
/// object, so an explicit `channel` naming anything else is refused rather
/// than accepted and ignored.
#[tokio::test]
async fn react_refuses_an_explicit_channel_it_cannot_honour() {
    let (tool, current_sent, other_sent, _other_recipients) = two_channel_tool(test_security(AutonomyLevel::Full));

    let result = tool
        .execute(json!({
            "action": "react", "channel": "telegram", "target": "12345",
            "emoji": "\u{1F44D}", "target_author": "+19998887777",
            "target_timestamp": 1_700_000_000_000u64
        }))
        .await
        .unwrap();

    assert!(!result.success);
    let error = result.error.unwrap_or_default();
    assert!(error.contains("Reactions cannot be redirected"), "got: {error}");
    assert!(error.contains("telegram"), "got: {error}");
    assert!(current_sent.lock().await.is_empty());
    assert!(other_sent.lock().await.is_empty());

    // Naming the handle that really delivers is accepted (and then fails on
    // the pre-existing "no Signal channel" path, unchanged).
    let same = tool
        .execute(json!({
            "action": "react", "channel": "dummy", "target": "12345",
            "emoji": "\u{1F44D}", "target_author": "+19998887777",
            "target_timestamp": 1_700_000_000_000u64
        }))
        .await
        .unwrap();
    assert!(!same.success);
    assert!(same.error.unwrap_or_default().contains("Signal"));
}

/// An explicit channel routes `edit`/`delete`/`thread` too, once authorized.
#[tokio::test]
async fn explicit_channel_routes_the_other_recipient_actions() {
    let (tool, _current_sent, _other_sent, _other_recipients) = two_channel_tool(scoped_security(&["*"], &[]));

    // DummyChannel does not implement these verbs, so success is not the
    // observable here; what matters is that the call reached the *named*
    // channel's default implementation rather than the policy or router.
    for args in [
        json!({"action": "edit", "channel": "telegram", "target": "1", "message_id": "9", "message": "x"}),
        json!({"action": "delete", "channel": "telegram", "target": "1", "message_id": "9"}),
        json!({"action": "thread", "channel": "telegram", "target": "1", "thread_id": "t", "message": "x"}),
    ] {
        let action = args["action"].as_str().unwrap_or_default().to_string();
        let result = tool.execute(args).await.unwrap();
        let error = result.error.unwrap_or_default();
        assert!(
            !error.contains(POLICY_ERROR_MARKER) && !error.contains("Unknown channel"),
            "action {action} must have been routed, got: {error}"
        );
    }
}

#[tokio::test]
async fn outbound_identity_comes_from_the_trusted_scope_only() {
    let (ch, sent) = DummyChannel::new();
    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        workspace_dir: std::env::temp_dir(),
        scope_rules: vec![crate::config::ScopeRule {
            user: Some("uuid:mallory".into()),
            channel: None,
            chat_type: None,
            tools_allow: vec![],
            tools_deny: vec![],
            send_allow: vec![],
            send_deny: vec!["*".into()],
            assign_allow: vec![],
            assign_deny: vec![],
        }],
        ..SecurityPolicy::default()
    });
    let tool = MessageSendTool::new(ch, security);

    // Runtime-injected trusted scope: the rule matches and denies.
    let denied = tool
        .execute(json!({
            "action": "send", "target": "+15550002222", "message": "hi",
            "_zc_scope_trusted": true,
            "_zc_scope": {"sender": "uuid:mallory", "channel": "dummy", "chat_type": "direct"}
        }))
        .await
        .unwrap();
    assert!(!denied.success);
    assert!(denied.error.unwrap_or_default().contains(POLICY_ERROR_MARKER));

    // A model-supplied scope without the trusted marker must not be believed:
    // the identity stays unknown, so mallory's rule cannot be dodged *or*
    // impersonated. Here it simply does not match.
    let untrusted = tool
        .execute(json!({
            "action": "send", "target": "+15550002222", "message": "hi",
            "_zc_scope": {"sender": "uuid:mallory", "channel": "dummy", "chat_type": "direct"}
        }))
        .await
        .unwrap();
    assert!(untrusted.success, "got: {:?}", untrusted.error);
    assert_eq!(sent.lock().await.len(), 1);
}
// ── Chat-side variant (routes through the daemon) ───────────────────────

fn chat_tool_for(url: &str, level: AutonomyLevel) -> DaemonMessageSendTool {
    let mut config = crate::config::Config::default();
    config.chat.daemon.url = url.to_string();
    config.chat.daemon.token = "zc_test".to_string();
    DaemonMessageSendTool::from_config(&config, test_security(level))
}

/// An address nothing is listening on: bind, learn the port, release it.
async fn closed_port_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: bind ephemeral port");
    let port = listener.local_addr().expect("test: local addr").port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn message_send_entry_points_share_identity_but_publish_process_specific_contracts() {
    let (channel, _sent) = DummyChannel::new();
    let in_process = MessageSendTool::new(channel, test_security(AutonomyLevel::Full));
    let through_daemon = chat_tool_for("http://127.0.0.1:1", AutonomyLevel::Full);
    assert_eq!(in_process.name(), through_daemon.name());
    assert_ne!(in_process.parameters_schema(), through_daemon.parameters_schema());
    assert_eq!(
        through_daemon.parameters_schema()["properties"]["action"]["enum"],
        json!(["send"])
    );
    assert_eq!(in_process.tier(), through_daemon.tier());
    assert_eq!(in_process.categories(), through_daemon.categories());
}

#[tokio::test]
async fn an_unreachable_daemon_is_reported_and_the_turn_continues() {
    let tool = chat_tool_for(&closed_port_url().await, AutonomyLevel::Full);
    let result = tool
        .execute(json!({
            "action": "send",
            "channel": "wacli",
            "target": "+15550001111",
            "message": "hello",
        }))
        .await
        .expect("test: an unreachable daemon must not abort the tool call");
    assert!(!result.success);
    let error = result.error.unwrap_or_default();
    assert!(
        error.contains("cannot reach a running PRX process"),
        "the operator must be told what to start: {error}"
    );
    assert!(error.contains("prx daemon"), "got: {error}");
}

#[tokio::test]
async fn a_send_from_chat_must_name_its_destination() {
    let tool = chat_tool_for("http://127.0.0.1:1", AutonomyLevel::Full);
    // No channel: chat has no conversation to inherit one from, and
    // guessing would be how a message reaches the wrong platform.
    let result = tool
        .execute(json!({"action": "send", "target": "+15550001111", "message": "hi"}))
        .await
        .expect("test: the tool call must complete");
    assert!(!result.success);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Missing 'channel'")),
        "test: {:?}",
        result.error
    );

    for missing in [
        json!({"action": "send", "channel": "wacli", "message": "hi"}),
        json!({"action": "send", "channel": "wacli", "target": "  ", "message": "hi"}),
    ] {
        let result = tool.execute(missing).await.expect("test: the tool call must complete");
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Missing 'target'")),
            "test: {:?}",
            result.error
        );
    }

    let result = tool
        .execute(json!({"action": "send", "channel": "wacli", "target": "+1", "message": "   "}))
        .await
        .expect("test: the tool call must complete");
    assert!(!result.success);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Missing 'message'")),
        "test: {:?}",
        result.error
    );
}

#[tokio::test]
async fn chat_refuses_the_actions_the_daemon_route_cannot_carry() {
    let tool = chat_tool_for("http://127.0.0.1:1", AutonomyLevel::Full);
    for action in ["react", "edit", "delete", "unsend", "thread"] {
        let result = tool
            .execute(json!({
                "action": action,
                "channel": "wacli",
                "target": "+15550001111",
                "message": "hi",
            }))
            .await
            .expect("test: the tool call must complete");
        assert!(!result.success, "action {action} must be refused");
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("is not available from chat")),
            "test: {action}: {:?}",
            result.error
        );
    }
}

#[tokio::test]
async fn chat_refuses_arguments_it_would_otherwise_drop_silently() {
    let tool = chat_tool_for("http://127.0.0.1:1", AutonomyLevel::Full);
    let result = tool
        .execute(json!({
            "action": "send",
            "channel": "wacli",
            "target": "+15550001111",
            "message": "hi",
            "quote_timestamp": 1_700_000_000_000_u64,
            "quote_author": "+15550002222",
        }))
        .await
        .expect("test: the tool call must complete");
    assert!(!result.success);
    let error = result.error.unwrap_or_default();
    assert!(error.contains("quote_timestamp"), "got: {error}");
    assert!(error.contains("quote_author"), "got: {error}");
}

#[tokio::test]
async fn read_only_autonomy_blocks_the_chat_variant_before_any_request() {
    let tool = chat_tool_for(&closed_port_url().await, AutonomyLevel::ReadOnly);
    let result = tool
        .execute(json!({
            "action": "send",
            "channel": "wacli",
            "target": "+15550001111",
            "message": "hi",
        }))
        .await
        .expect("test: the tool call must complete");
    assert!(!result.success);
    assert_eq!(result.error.as_deref(), Some("Action blocked: autonomy is read-only"));
}

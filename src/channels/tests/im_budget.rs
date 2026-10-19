#![cfg(test)]
use super::*;

const IM_BULKY_TOOL: &str = "im_bulky_probe";
const IM_BULKY_MARKER: &str = "IM-BULKY";
const IM_WINDOW_PROVIDER: &str = "window-test-provider";
const IM_WINDOW_MODEL: &str = "narrow-window-model";
const IM_ROUTER_WINDOW: usize = 6_000;
const IM_BUDGET_REQUEST: &str = "summarise the im bulky probe";

/// A read-only tool whose output overruns a 6,000-token window but fits the
/// 128,000-token base window and the unbudgeted inline cap.
struct ImBulkyTool;

#[async_trait::async_trait]
impl Tool for ImBulkyTool {
    fn name(&self) -> &str {
        IM_BULKY_TOOL
    }

    fn description(&self) -> &str {
        "Return a large diagnostic payload."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        use std::fmt::Write as _;
        let mut output = String::with_capacity(40_000);
        let mut index = 0usize;
        while output.len() < 40_000 {
            let _ = write!(output, "{IM_BULKY_MARKER}-{index} ");
            index += 1;
        }
        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

/// Native-tool spy: asks for the bulky tool on its first call, answers in
/// text afterwards, and records every request it serves.
#[derive(Default)]
struct ImToolRoundSpyProvider {
    requests: parking_lot::Mutex<Vec<Vec<ChatMessage>>>,
}

#[async_trait::async_trait]
impl Provider for ImToolRoundSpyProvider {
    fn capabilities(&self) -> crate::providers::ProviderCapabilities {
        crate::providers::ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("summary".into())
    }

    async fn chat(
        &self,
        request: crate::providers::ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::ChatResponse> {
        let call_index = {
            let mut requests = self.requests.lock();
            requests.push(request.messages.to_vec());
            requests.len()
        };
        let tool_calls = if call_index == 1 {
            vec![crate::providers::ToolCall {
                id: "call-im-bulky-1".into(),
                name: IM_BULKY_TOOL.into(),
                arguments: "{}".into(),
            }]
        } else {
            Vec::new()
        };
        Ok(crate::providers::ChatResponse {
            text: tool_calls.is_empty().then(|| "done".to_string()),
            tool_calls,
            reasoning_content: None,
        })
    }
}

/// An IM channel turn budgets on the served model's effective window, the
/// one the CLI, TUI and webhook budget on (OQ-23 / R5).
///
/// The context uses a same-owner config pair, so the turn reads its
/// provider, model and compaction config from the pinned generation, as in
/// production. The base config keeps the 128,000-token default; a
/// `[[router.models]]` entry gives the served model a 6,000-token window.
/// Budgeted on the base window, the follow-up request carries the whole
/// 40,000-character tool payload.
///
/// MUTATION GUARD: pass `Some(&message_runtime.agent_compaction)` to the
/// loop again and the follow-up request carries the whole payload.
#[tokio::test]
async fn im_channel_turn_budgets_on_the_served_models_effective_window() {
    let tmp = TempDir::new().expect("test: tempdir");
    let mut config = Config::default();
    config.workspace_dir = tmp.path().to_path_buf();
    config.default_provider = Some(IM_WINDOW_PROVIDER.to_string());
    config.default_model = Some(IM_WINDOW_MODEL.to_string());
    config.agent.compaction = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Off,
        ..crate::config::AgentCompactionConfig::default()
    };
    config.router.models = vec![crate::config::RouterModelConfig {
        model_id: IM_WINDOW_MODEL.to_string(),
        provider: IM_WINDOW_PROVIDER.to_string(),
        cost_per_million_tokens: 1.0,
        max_context: IM_ROUTER_WINDOW,
        reserved_output_tokens: Some(256),
        latency_ms: 1_000,
        categories: vec!["conversation".to_string()],
        elo_rating: 1_000.0,
    }];
    config.tool_tiering.always_include = vec![IM_BULKY_TOOL.to_string()];
    let manager = crate::config::new_shared(config);
    let generation = manager.pin();

    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);
    let provider_impl = Arc::new(ImToolRoundSpyProvider::default());

    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.config = Arc::clone(&manager);
    ctx.config_generation = Arc::clone(&generation);
    ctx.channels_by_name = Arc::new(channels_by_name);
    ctx.provider = provider_impl.clone();
    ctx.default_provider = Arc::new(IM_WINDOW_PROVIDER.to_string());
    ctx.tools_registry = Arc::new(vec![Box::new(ImBulkyTool)]);
    ctx.workspace_dir = Arc::new(tmp.path().to_path_buf());

    run_owned_channel_probe(
        Arc::new(ctx),
        traits::ChannelMessage {
            id: "msg-im-window".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-window".to_string(),
            content: IM_BUDGET_REQUEST.to_string(),
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

    let requests = provider_impl.requests.lock().clone();
    assert_eq!(requests.len(), 2, "one tool round, then the answer");
    let follow_up = requests.get(1).expect("test: the follow-up request was recorded");
    let payload_occurrences: usize = follow_up
        .iter()
        .map(|message| message.content.matches(IM_BULKY_MARKER).count())
        .sum();
    assert!(
        payload_occurrences < 500,
        "the follow-up request must be budgeted on the model's {IM_ROUTER_WINDOW}-token window, \
         not the 128,000-token base window; found {payload_occurrences} payload segments"
    );
    let follow_up_chars: usize = follow_up.iter().map(|message| message.content.chars().count()).sum();
    assert!(
        follow_up_chars < IM_ROUTER_WINDOW * 4,
        "the follow-up request must fit the effective window; it has {follow_up_chars} characters"
    );
    assert!(
        follow_up
            .iter()
            .any(|message| message.role == "user" && message.content.contains(IM_BUDGET_REQUEST)),
        "the turn's own request must survive the budget (D-011)"
    );
    let sent_messages = channel_impl.sent_messages.lock().await;
    assert!(
        response_messages(&sent_messages)
            .iter()
            .any(|message| message.contains("done")),
        "the turn must deliver its answer: {sent_messages:?}"
    );
}

/// A `/models` override is budgeted on the model it selects, not on the
/// configured default (SP-INT2 a45).
///
/// The configured default model has no `[[router.models]]` entry, so its
/// window is the 128,000-token base; the conversation's route override
/// (`set_route_selection`, what `/models` and `/model` write) selects the
/// 6,000-token model. The follow-up request must fit the override's window.
///
/// MUTATION GUARD: resolve the effective window from
/// `runtime_defaults.default_provider` / `runtime_defaults.model` instead
/// of `route.provider` / `route.model` and the follow-up request carries
/// the whole payload.
#[tokio::test]
async fn im_channel_turn_budgets_on_the_route_override_window() {
    const WIDE_DEFAULT_MODEL: &str = "wide-default-model";
    let tmp = TempDir::new().expect("test: tempdir");
    let mut config = Config::default();
    config.workspace_dir = tmp.path().to_path_buf();
    config.config_path = tmp.path().join("config.toml");
    config.default_provider = Some(IM_WINDOW_PROVIDER.to_string());
    config.default_model = Some(WIDE_DEFAULT_MODEL.to_string());
    config.agent.compaction = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Off,
        ..crate::config::AgentCompactionConfig::default()
    };
    config.router.models = vec![crate::config::RouterModelConfig {
        model_id: IM_WINDOW_MODEL.to_string(),
        provider: IM_WINDOW_PROVIDER.to_string(),
        cost_per_million_tokens: 1.0,
        max_context: IM_ROUTER_WINDOW,
        reserved_output_tokens: Some(256),
        latency_ms: 1_000,
        categories: vec!["conversation".to_string()],
        elo_rating: 1_000.0,
    }];
    config.tool_tiering.always_include = vec![IM_BULKY_TOOL.to_string()];
    let manager = crate::config::new_shared(config);
    let generation = manager.pin();

    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);
    let provider_impl = Arc::new(ImToolRoundSpyProvider::default());

    let mut ctx = ctx_with_histories(HashMap::new());
    ctx.config = Arc::clone(&manager);
    ctx.config_generation = Arc::clone(&generation);
    ctx.channels_by_name = Arc::new(channels_by_name);
    ctx.provider = provider_impl.clone();
    ctx.default_provider = Arc::new(IM_WINDOW_PROVIDER.to_string());
    ctx.tools_registry = Arc::new(vec![Box::new(ImBulkyTool)]);
    ctx.workspace_dir = Arc::new(tmp.path().to_path_buf());

    let message = traits::ChannelMessage {
        id: "msg-im-route-window".to_string(),
        sender: "alice".to_string(),
        reply_target: "chat-route-window".to_string(),
        content: IM_BUDGET_REQUEST.to_string(),
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
    };
    let key = ConversationKey::from_message(&message);
    assert_eq!(
        get_route_selection_for_generation(&ctx, &key, &generation).model,
        WIDE_DEFAULT_MODEL,
        "test: without an override the conversation serves the wide default"
    );
    set_route_selection(
        &ctx,
        &generation,
        &key,
        ChannelRouteSelection {
            provider: IM_WINDOW_PROVIDER.to_string(),
            model: IM_WINDOW_MODEL.to_string(),
        },
    );

    run_owned_channel_probe(Arc::new(ctx), message, CancellationToken::new()).await;

    let requests = provider_impl.requests.lock().clone();
    assert_eq!(requests.len(), 2, "one tool round, then the answer");
    let follow_up = requests.get(1).expect("test: the follow-up request was recorded");
    let payload_occurrences: usize = follow_up
        .iter()
        .map(|message| message.content.matches(IM_BULKY_MARKER).count())
        .sum();
    assert!(
        payload_occurrences < 500,
        "the follow-up request must be budgeted on the override's {IM_ROUTER_WINDOW}-token window, not \
         the default model's; found {payload_occurrences} payload segments"
    );
    let follow_up_chars: usize = follow_up.iter().map(|message| message.content.chars().count()).sum();
    assert!(
        follow_up_chars < IM_ROUTER_WINDOW * 4,
        "the follow-up request must fit the override's window; it has {follow_up_chars} characters"
    );
    assert!(
        follow_up
            .iter()
            .any(|message| message.role == "user" && message.content.contains(IM_BUDGET_REQUEST)),
        "the turn's own request must survive the budget (D-011)"
    );
}

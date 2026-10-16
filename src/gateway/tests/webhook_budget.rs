#![cfg(test)]

use super::*;

const BULKY_PAYLOAD_MARKER: &str = "BULKY-PAYLOAD";

const WEBHOOK_BUDGET_REQUEST: &str = "summarise the bulky probe for me";

/// A read-only tool whose output is large enough to overrun a small
/// context window, but under the unbudgeted 48,000-character inline cap.
struct BulkyProbeTool;

#[async_trait]
impl Tool for BulkyProbeTool {
    fn name(&self) -> &str {
        BULKY_PROBE_TOOL
    }

    fn description(&self) -> &str {
        "Return a large diagnostic payload."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        use std::fmt::Write as _;
        let mut output = String::with_capacity(40_000);
        let mut index = 0usize;
        while output.len() < 40_000 {
            let _ = write!(output, "{BULKY_PAYLOAD_MARKER}-{index} ");
            index += 1;
        }
        Ok(crate::tools::ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

/// Native-tool spy: records every request it serves, asks for
/// `bulky_probe` on its first call when scripted to, and answers in text
/// afterwards.
#[derive(Default)]
struct BudgetSpyProvider {
    call_tool_first: bool,
    requests: Mutex<Vec<Vec<ChatMessage>>>,
}

impl BudgetSpyProvider {
    fn calling_the_bulky_probe() -> Self {
        Self {
            call_tool_first: true,
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Provider for BudgetSpyProvider {
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
        let tool_calls = if self.call_tool_first && call_index == 1 {
            vec![crate::providers::ToolCall {
                id: "call-bulky-1".into(),
                name: BULKY_PROBE_TOOL.into(),
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

#[derive(Default)]
struct ReloadPinnedProvider {
    calls: AtomicUsize,
    first_started: Notify,
    release_first: Notify,
    models: Mutex<Vec<String>>,
}

#[async_trait]
impl Provider for ReloadPinnedProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
        self.models.lock().push(model.to_string());
        assert_eq!(call_index, 0, "the pinned turn needs exactly one provider round");
        self.first_started.notify_one();
        self.release_first.notified().await;
        Ok("reload-pinned".to_string())
    }
}

fn budget_webhook_state(
    tmp: &tempfile::TempDir,
    config: Config,
    provider: Arc<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    mcp_tool: Option<Arc<McpTool>>,
) -> (AppState, Arc<dyn Memory>) {
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
    let mut state = AppState {
        config: crate::config::new_shared(config),
        mem: Arc::clone(&memory),
        tools_registry: Arc::new(tools),
        mcp_tool,
        hooks: Arc::new(HookManager::new(tmp.path().to_path_buf())),
        ..webhook_test_state(provider)
    };
    install_webhook_turn_runtime(&mut state);
    (state, memory)
}

/// A default config rooted in `tmp`: the workspace and the config path
/// (so the state directory, auth store included) are the temp dir's,
/// never the real `~/.openprx`.
fn budget_test_config(tmp: &tempfile::TempDir) -> Config {
    let mut config = webhook_test_config();
    config.workspace_dir = tmp.path().to_path_buf();
    config.config_path = tmp.path().join("config.toml");
    config
}

async fn post_budget_webhook(state: AppState, message: &str) {
    let diagnostic_state = state.clone();
    let body = serde_json::json!({"message": message, "reply_target": "client-a"}).to_string();
    let response = handle_webhook(
        State(state),
        test_connect_info(),
        test_uri(),
        HeaderMap::new(),
        Bytes::from(body),
    )
    .await
    .into_response();
    if response.status() != StatusCode::OK {
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("test: error response body")
            .to_bytes();
        let workspace = diagnostic_state.config.pin().effective.workspace_dir.clone();
        let events = all_workspace_message_events(&diagnostic_state, &workspace).await;
        panic!(
            "the webhook turn must complete; status={status}, body={}, events={events:?}",
            String::from_utf8_lossy(&body)
        );
    }
}

/// A real webhook turn pins one runtime generation before admission and
/// keeps its provider, model and event scope through completion,
/// even when the gateway commits a replacement generation mid-request.
#[tokio::test]
async fn a_webhook_turn_stays_on_one_runtime_generation_across_reload() {
    let tmp = tempfile::TempDir::new().expect("test: temp dir");
    let config = budget_test_config(&tmp);
    let old_provider_impl = Arc::new(ReloadPinnedProvider::default());
    let old_provider: Arc<dyn Provider> = old_provider_impl.clone();
    let (state, _) = budget_webhook_state(&tmp, config, old_provider, Vec::new(), None);
    let event_state = state.clone();
    let owner = Arc::clone(state.turn_runtime.as_ref().expect("test: turn runtime owner"));
    let first_started = old_provider_impl.first_started.notified();
    tokio::pin!(first_started);
    let mut turn = tokio::spawn(post_budget_webhook(state, "hello across reload"));

    tokio::select! {
        () = &mut first_started => {}
        result = &mut turn => panic!("the webhook ended before reaching its provider: {result:?}"),
        () = tokio::time::sleep(Duration::from_secs(15)) => {
            turn.abort();
            panic!("the webhook did not reach its provider within 15 seconds");
        }
    }
    let previous = owner.current.load_full();
    let replacement_provider_impl = Arc::new(MockProvider::default());
    let replacement_provider: Arc<dyn Provider> = replacement_provider_impl.clone();
    let config = previous.config_generation.effective.as_ref();
    let provider_name = crate::providers::selection::default_provider(config);
    let resolved = crate::providers::selection::check_with(
        config,
        &crate::providers::selection::catalog_for(config),
        provider_name,
        &previous.model,
        crate::providers::selection::credential_for(config, provider_name),
        &crate::providers::catalog::Requirement::chat(),
        crate::providers::catalog::UnknownPolicy::Invocation,
    )
    .expect("test: replacement provider binding");
    let next_generation = Arc::new(crate::config::ConfigGeneration {
        id: crate::config::ConfigGenerationId(previous.config_generation.id.0 + 1),
        source_revision: previous.config_generation.source_revision.clone(),
        effective: Arc::clone(&previous.config_generation.effective),
        applied_at: chrono::Utc::now(),
        trigger: crate::config::ConfigReloadTrigger::Test,
        deferred_changes: Arc::from([]),
    });
    let mut replacement_slot = decision_runtime::GatewayDecisionSlot::new(
        Arc::new(crate::security::SecurityPolicy::from_config(
            &config.autonomy,
            &config.workspace_dir,
        )),
        0,
    );
    replacement_slot.cleanup = Arc::clone(&owner.cleanup);
    let mut prepared = PreparedGatewayTurnRuntime {
        current: Arc::clone(&owner.current),
        previous: Arc::clone(&previous),
        generation: TurnRuntimeGeneration {
            provider_binding: Some(Arc::new(decision_source::GatewayProviderBinding::new(
                Arc::clone(&replacement_provider),
                resolved,
            ))),
            decision_slot: Arc::new(replacement_slot),
            config_generation: Arc::clone(&next_generation),
            provider: replacement_provider,
            model: previous.model.clone(),
            temperature: 0.9,
            tools_registry: Arc::new(Vec::new()),
            mcp_tool: None,
        },
    };
    crate::config::PreparedConfigGeneration::commit(&mut prepared).expect("test: commit replacement runtime");
    assert_eq!(owner.pin().config_generation.id, next_generation.id);
    assert!(owner.pin().tools_registry.is_empty());

    old_provider_impl.release_first.notify_one();
    turn.await.expect("test: webhook task");

    assert_eq!(old_provider_impl.calls.load(Ordering::SeqCst), 1);
    assert_eq!(old_provider_impl.models.lock().as_slice(), ["test-model"]);
    assert_eq!(replacement_provider_impl.calls.load(Ordering::SeqCst), 0);
    assert_eq!(owner.pin().config_generation.id, next_generation.id);
    let events = all_workspace_message_events(&event_state, tmp.path()).await;
    assert!(
        events.iter().any(|event| event.event_type == "turn.finalized"),
        "the in-flight turn must finish after the replacement commits"
    );
    assert!(
        events
            .iter()
            .all(|event| event.config_generation_id == Some(previous.config_generation.id.0)),
        "every event from the admitted turn must retain generation zero: {events:?}"
    );
}

/// The durable `llm.request.context.compiled` payloads of the turn, in
/// iteration order.
async fn compiled_request_payloads(memory: &Arc<dyn Memory>, tmp: &tempfile::TempDir) -> Vec<serde_json::Value> {
    let principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: tmp.path().to_string_lossy().to_string(),
        agent_id: None,
        persona_id: None,
        session_key: Some("gateway:webhook:public:client-a".to_string()),
        channel: Some("webhook".to_string()),
        sender: Some("client-a".to_string()),
        owner_id: None,
        legacy_session_key: None,
    };
    let events = memory
        .list_message_events_since(&principal, 0, 50)
        .await
        .expect("test: list message events");
    events
        .iter()
        .filter(|event| event.event_type == "llm.request.context.compiled")
        .map(|event| {
            serde_json::from_str(
                event
                    .raw_payload_json
                    .as_deref()
                    .expect("test: request context payload"),
            )
            .expect("test: request context payload is JSON")
        })
        .collect()
}

/// A webhook turn is budgeted, and its compiled-request event says so.
///
/// Before OQ-23 the event carried `budgeted_total_tokens: null`, which is
/// the loop's own marker for "this entry point passed no budget".
///
/// MUTATION GUARD: pass `None` as the webhook's compaction config again and
/// the budgeted totals go back to `null`.
#[tokio::test]
async fn a_webhook_turn_reports_its_budgeted_request_in_the_compiled_event() {
    let tmp = tempfile::TempDir::new().unwrap();
    let provider_impl = Arc::new(BudgetSpyProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let (state, memory) = budget_webhook_state(&tmp, budget_test_config(&tmp), provider, Vec::new(), None);

    post_budget_webhook(state, "hello budget").await;

    let payloads = compiled_request_payloads(&memory, &tmp).await;
    let payload = payloads.first().expect("the turn must record a compiled-request event");
    let estimate = &payload["request_estimate"];
    let budgeted_total = estimate["budgeted_total_tokens"]
        .as_u64()
        .unwrap_or_else(|| panic!("the webhook request must be budgeted, got {estimate}"));
    let budgeted_messages = estimate["budgeted_messages_tokens"]
        .as_u64()
        .unwrap_or_else(|| panic!("the webhook history must be budgeted, got {estimate}"));
    assert!(budgeted_messages > 0, "the budget must have measured the history");
    assert!(
        budgeted_total >= budgeted_messages,
        "the budgeted request covers at least the budgeted history"
    );
    assert_eq!(provider_impl.requests.lock().len(), 1);
}

/// An over-budget webhook tool round is bounded before the next provider
/// call, and the request survives the bounding (D-011).
///
/// The window is set explicitly and far below what the probe's 40,000
/// characters need. Without a budget the second provider call received
/// the whole payload inline (under the unbudgeted 48,000-character cap).
/// With the preflight the tool result is sized to the remaining window and
/// the history is trimmed to fit, while the turn's own request is restored
/// rather than trimmed away.
///
/// `mode = off` keeps the proof to the budget itself: no summariser call
/// joins the spy's request log.
///
/// MUTATION GUARD: pass `None` as the webhook's compaction config again and
/// the second request carries the whole payload.
#[tokio::test]
async fn an_over_budget_webhook_tool_round_is_bounded_and_keeps_the_request() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = budget_test_config(&tmp);
    config.agent.compaction = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Off,
        reserve_tokens: 256,
        max_context_tokens: 6_000,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    config.tool_tiering.always_include = vec![BULKY_PROBE_TOOL.to_string()];
    let provider_impl = Arc::new(BudgetSpyProvider::calling_the_bulky_probe());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let (state, memory) = budget_webhook_state(&tmp, config, provider, vec![Box::new(BulkyProbeTool)], None);

    post_budget_webhook(state, WEBHOOK_BUDGET_REQUEST).await;

    let requests = provider_impl.requests.lock().clone();
    assert_eq!(requests.len(), 2, "one tool round, then the answer");
    let follow_up = &requests[1];
    // The probe emits about 2,100 marker segments. Unbudgeted, all of them
    // reached the follow-up request; budgeted, the result is cut to what
    // the 6,000-token window has left.
    let payload_occurrences: usize = follow_up
        .iter()
        .map(|message| message.content.matches(BULKY_PAYLOAD_MARKER).count())
        .sum();
    assert!(
        payload_occurrences < 500,
        "the follow-up request must not carry the unbudgeted tool payload; found {payload_occurrences} \
             payload segments"
    );
    // Four characters per token is a generous ceiling for this text, so a
    // request longer than that could not fit the window whatever the
    // tokenizer. Unbudgeted, the follow-up was over 40,000 characters.
    let follow_up_chars: usize = follow_up.iter().map(|message| message.content.chars().count()).sum();
    assert!(
        follow_up_chars < 6_000 * 4,
        "the follow-up request must fit the 6,000-token window; it has {follow_up_chars} characters"
    );
    assert!(
        follow_up
            .iter()
            .any(|message| message.role == "user" && message.content.contains(WEBHOOK_BUDGET_REQUEST)),
        "the turn's own request must survive the budget (D-011)"
    );

    let payloads = compiled_request_payloads(&memory, &tmp).await;
    assert_eq!(payloads.len(), 2, "each provider call records its compiled request");
    for payload in &payloads {
        assert!(
            payload["request_estimate"]["budgeted_total_tokens"].is_u64(),
            "every iteration of the webhook turn is budgeted: {payload}"
        );
    }
}

/// The same over-budget round on the default `switch` mode, served by the
/// production gateway registry (SP-INT2 a44).
///
/// The `off` test above proves the budget alone. The default mode adds a
/// rollover, and a `switch` turn whose history starts over the warning
/// threshold refuses to run unless `transcript_history_lookup` is
/// registered, because the handoff it may create points the model at that
/// tool. A large workspace `AGENTS.md` puts this turn's system prompt over
/// the threshold. The turn must not bail, its follow-up must not carry the
/// tool payload, its own request must survive (D-011), and the rollover
/// must be recorded: a `context.switch.created` handoff, or a
/// `context.compaction.degraded` record when no exact handoff was possible.
///
/// MUTATION GUARD: drop `TranscriptHistoryLookupTool` from the registry
/// `tools::all_tools_with_runtime_ext_and_extensions` builds and the turn
/// bails before its first provider request.
#[tokio::test]
async fn an_over_budget_webhook_tool_round_rolls_over_on_the_default_switch_mode() {
    let tmp = tempfile::TempDir::new().expect("test: temp dir");
    std::fs::write(tmp.path().join("AGENTS.md"), "agents-guidance ".repeat(2_000)).expect("test: write AGENTS.md");
    let mut config = budget_test_config(&tmp);
    config.agent.compaction = crate::config::AgentCompactionConfig {
        reserve_tokens: 256,
        max_context_tokens: 6_000,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    assert!(
        matches!(config.agent.compaction.mode, crate::config::AgentCompactionMode::Switch),
        "test: the default compaction mode is `switch`"
    );
    config.tool_tiering.always_include = vec![BULKY_PROBE_TOOL.to_string()];
    // The registry builders construct the configured provider chain, which
    // needs a credential; the turn itself is served by the spy.
    config.api_key = Some("test-key".to_string());
    let (_daemon, mut tools, mcp_tool) = production_gateway_registries(&config);
    tools.push(Box::new(BulkyProbeTool));
    let provider_impl = Arc::new(BudgetSpyProvider::calling_the_bulky_probe());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let (state, memory) = budget_webhook_state(&tmp, config, provider, tools, mcp_tool);

    post_budget_webhook(state, WEBHOOK_BUDGET_REQUEST).await;

    let requests = provider_impl.requests.lock().clone();
    assert!(
        requests.len() >= 2,
        "the turn must reach its follow-up request instead of bailing: {} request(s)",
        requests.len()
    );
    let follow_up = &requests[1];
    let payload_occurrences: usize = follow_up
        .iter()
        .map(|message| message.content.matches(BULKY_PAYLOAD_MARKER).count())
        .sum();
    assert!(
        payload_occurrences < 500,
        "the follow-up request must not carry the unbudgeted tool payload; found {payload_occurrences} \
             payload segments"
    );
    // The system prompt cannot be trimmed; everything else must fit.
    let follow_up_chars: usize = follow_up
        .iter()
        .filter(|message| message.role != "system")
        .map(|message| message.content.chars().count())
        .sum();
    assert!(
        follow_up_chars < 6_000 * 4,
        "the follow-up request must fit the 6,000-token window; it has {follow_up_chars} characters \
             besides the system prompt"
    );
    assert!(
        follow_up
            .iter()
            .any(|message| message.role == "user" && message.content.contains(WEBHOOK_BUDGET_REQUEST)),
        "the turn's own request must survive the rollover (D-011)"
    );

    let principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: tmp.path().to_string_lossy().to_string(),
        agent_id: None,
        persona_id: None,
        session_key: Some("gateway:webhook:public:client-a".to_string()),
        channel: Some("webhook".to_string()),
        sender: Some("client-a".to_string()),
        owner_id: None,
        legacy_session_key: None,
    };
    let event_types: Vec<String> = memory
        .list_message_events_since(&principal, 0, 200)
        .await
        .expect("test: list message events")
        .into_iter()
        .map(|event| event.event_type)
        .collect();
    assert!(
        event_types
            .iter()
            .any(|kind| kind == "context.switch.created" || kind == "context.compaction.degraded"),
        "the rollover must be recorded as a handoff or a degraded trim: {event_types:?}"
    );
}

/// `[agent] compact_context` caps each bootstrap file on the webhook turn
/// too (E3, webhook half).
///
/// The flag means "6,000 characters per workspace bootstrap file instead of
/// 60,000". CLI, chat, Console sessions and channels honoured it; the
/// webhook built its prompt with a hardcoded `None` and sent the whole file.
///
/// MUTATION GUARD: build the webhook prompt with `bootstrap_max_chars =
/// None` again and the marker disappears.
#[tokio::test]
async fn a_webhook_turn_honours_compact_context_in_its_system_prompt() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("AGENTS.md"), "agents-guidance ".repeat(700)).expect("test: write AGENTS.md");
    let mut config = budget_test_config(&tmp);
    config.agent.compact_context = true;
    let provider_impl = Arc::new(BudgetSpyProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let (state, _memory) = budget_webhook_state(&tmp, config, provider, Vec::new(), None);

    post_budget_webhook(state, "hello compact").await;

    let requests = provider_impl.requests.lock().clone();
    let first = requests.first().expect("the turn must reach the provider");
    let system = first
        .iter()
        .find(|message| message.role == "system")
        .expect("the request carries a system prompt");
    assert!(
        system.content.contains("[... truncated at 6000 chars"),
        "compact_context must cap the 11,200-character AGENTS.md at 6,000 characters"
    );
}

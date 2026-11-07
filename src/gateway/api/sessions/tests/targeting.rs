#![cfg(test)]
//! Targeted agent selection: selector, name grammar, rollout switch, autonomy
//! cap and allowlist semantics.
use super::*;
use crate::config::DelegateAgentConfig;
use crate::config::agent_name::{AGENT_NAME_MAX_LEN, AgentName, AgentNameError, is_targetable};
use crate::security::AutonomyLevel;
use std::collections::BTreeSet;

use super::super::target::{AgentField, TargetError, ToolAllowlist, TurnTarget, effective_autonomy, resolve_target};

fn agent(agentic: bool, allowed_tools: &[&str]) -> DelegateAgentConfig {
    DelegateAgentConfig {
        provider: "ollama".to_owned(),
        model: "test-model".to_owned(),
        system_prompt: None,
        api_key: None,
        credential_ref: None,
        temperature: None,
        max_depth: 8,
        agentic,
        allowed_tools: allowed_tools.iter().map(|name| (*name).to_owned()).collect(),
        identity_dir: None,
        memory_scope: None,
        spawn_enabled: None,
        enabled: true,
        autonomy: None,
    }
}

fn config_with(agents: Vec<(&str, DelegateAgentConfig)>, switch: bool) -> Config {
    let mut config = Config::default();
    config.gateway.targeted_agents = switch;
    config.agents = agents.into_iter().map(|(name, cfg)| (name.to_owned(), cfg)).collect();
    config
}

fn named(raw: &str) -> TurnTarget {
    TurnTarget::from_field(AgentField::Text(raw.to_owned())).expect("test: a valid name")
}

fn http_request(body: serde_json::Value) -> Request {
    let mut request = Request::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("test: request");
    request
        .extensions_mut()
        .insert(super::super::super::ApiPrincipal::of_token("prx-test-token"));
    request
}

struct Fixture {
    state: AppState,
    provider: Arc<CapturingProvider>,
    memory: Arc<crate::memory::SqliteMemory>,
    _temp: tempfile::TempDir,
}

const SESSION: &str = "prx-target-session";

async fn fixture(config: impl FnOnce(&mut Config)) -> Fixture {
    let temp = tempfile::tempdir().expect("test: tempdir");
    let mut base = Config::default();
    base.workspace_dir = temp.path().to_owned();
    config(&mut base);
    let provider = Arc::new(CapturingProvider::default());
    let mut state = test_app_state(base, provider.clone());
    let memory = Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("test: sqlite memory"));
    state.mem = memory.clone();
    memory
        .create_conversation_session(SESSION, "api:local")
        .await
        .expect("test: session");
    Fixture {
        state,
        provider,
        memory,
        _temp: temp,
    }
}

async fn post(fixture: &Fixture, session: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let response = post_session_message(
        State(fixture.state.clone()),
        Path(session.to_owned()),
        http_request(body),
    )
    .await
    .into_response();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("test: body");
    (status, serde_json::from_slice(&bytes).expect("test: json body"))
}

fn error_code(body: &serde_json::Value) -> Option<&str> {
    body.pointer("/error/code").and_then(serde_json::Value::as_str)
}

async fn assert_untouched(fixture: &Fixture) {
    assert!(fixture.provider.requests.lock().is_empty(), "no provider access");
    let stored = fixture
        .memory
        .get_conversation_session(SESSION)
        .await
        .expect("test: read session")
        .expect("test: session exists");
    assert_eq!(stored.message_count, 0, "no input was recorded");
}

#[tokio::test]
async fn target_selector_preserves_main() {
    // Omission and "main" reach the Main path (here: the session lookup, which
    // answers a plain string error for a session that does not exist), with the
    // switch off and on, and whatever `[agents.main]` holds.
    for switch in [false, true] {
        let fixture = fixture(|config| {
            config.gateway.targeted_agents = switch;
            config.agents.insert("main".to_owned(), agent(false, &[]));
        })
        .await;
        for body in [
            serde_json::json!({"message": "hello"}),
            serde_json::json!({"message": "hello", "agent": "main"}),
        ] {
            let (status, value) = post(&fixture, "no-such-session", body).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(
                value.get("error").and_then(serde_json::Value::as_str),
                Some("Session not found"),
                "the Main path was entered: {value}"
            );
        }
        // Explicit null is the one refused spelling.
        let (status, value) = post(
            &fixture,
            SESSION,
            serde_json::json!({"message": "hello", "agent": null}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error_code(&value), Some("agent_invalid"));
        let (status, value) = post(&fixture, SESSION, serde_json::json!({"message": "hello", "agent": 7})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error_code(&value), Some("agent_invalid"));
        assert_untouched(&fixture).await;
    }
    assert_eq!(TurnTarget::from_field(AgentField::Omitted), Ok(TurnTarget::Main));
    assert_eq!(
        TurnTarget::from_field(AgentField::Text("main".to_owned())),
        Ok(TurnTarget::Main)
    );
    assert_eq!(TurnTarget::from_field(AgentField::Null), Err(TargetError::Invalid));
}

#[tokio::test]
async fn target_unknown_or_disabled_has_no_effects() {
    let mut off = agent(false, &[]);
    off.enabled = false;
    // Switch off: every otherwise-valid named target is 409, existing or not,
    // and never runs as Main.
    let closed = fixture(|config| {
        config.agents.insert("helper".to_owned(), agent(false, &[]));
    })
    .await;
    for name in ["helper", "ghost"] {
        let (status, value) = post(&closed, SESSION, serde_json::json!({"message": "hi", "agent": name})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{name}: {value}");
        assert_eq!(error_code(&value), Some("agent_disabled"));
    }
    assert_untouched(&closed).await;

    // Switch on: unknown is 404, disabled is 409, both resolved before the
    // session lookup, so even a missing session reports the target error.
    let open = fixture(|config| {
        config.gateway.targeted_agents = true;
        config.agents.insert("helper".to_owned(), agent(false, &[]));
        config.agents.insert("sleeper".to_owned(), off);
    })
    .await;
    for (name, session, status, code) in [
        ("ghost", SESSION, StatusCode::NOT_FOUND, "agent_unknown"),
        ("sleeper", SESSION, StatusCode::CONFLICT, "agent_disabled"),
        ("ghost", "no-such-session", StatusCode::NOT_FOUND, "agent_unknown"),
    ] {
        let (got, value) = post(&open, session, serde_json::json!({"message": "hi", "agent": name})).await;
        assert_eq!(got, status, "{name}: {value}");
        assert_eq!(error_code(&value), Some(code));
    }
    assert_untouched(&open).await;
}

#[test]
fn target_autonomy_is_capped() {
    use AutonomyLevel::{Full, ReadOnly, Supervised};
    for (agent_level, main, expected) in [
        (None, Supervised, Supervised),
        (Some(Full), Supervised, Supervised),
        (Some(ReadOnly), Full, ReadOnly),
        (Some(Supervised), Full, Supervised),
        (Some(Full), ReadOnly, ReadOnly),
        (Some(Full), Full, Full),
    ] {
        assert_eq!(effective_autonomy(agent_level, main), expected);
    }

    let mut capped = agent(false, &[]);
    capped.autonomy = Some(Full);
    let mut config = config_with(vec![("worker", capped)], true);
    config.autonomy.level = Supervised;
    let plan = resolve_target(&config, &named("worker"))
        .expect("test: resolves")
        .expect("test: a named plan");
    assert_eq!(plan.autonomy, Supervised, "a sub-agent never exceeds Main");
    // The policy the target's turn is published with carries that level.
    let mut snapshot = config.clone();
    snapshot.autonomy.level = plan.autonomy;
    assert_eq!(
        crate::runtime::bootstrap::build_security_policy(&snapshot).autonomy,
        Supervised
    );

    config.agents.get_mut("worker").expect("test: worker").autonomy = Some(ReadOnly);
    config.autonomy.level = Full;
    let plan = resolve_target(&config, &named("worker"))
        .expect("test: resolves")
        .expect("test: a named plan");
    assert_eq!(plan.autonomy, ReadOnly, "a lower sub-agent level is kept");
}

#[test]
fn agent_name_rejects_invalid_start() {
    for raw in [
        "",
        "1agent",
        "9",
        "Agent",
        "A",
        "-agent",
        "_agent",
        " agent",
        "\u{e9}agent",
    ] {
        assert_eq!(AgentName::parse(raw), Err(AgentNameError::InvalidStart), "{raw:?}");
        assert_eq!(
            TurnTarget::from_field(AgentField::Text(raw.to_owned())),
            Err(TargetError::Invalid),
            "{raw:?}"
        );
    }
}

#[tokio::test]
async fn agent_name_rejects_invalid_characters() {
    for raw in [
        "a b", "a/b", "a.b", "a\\b", "a\u{e9}", "a\u{3b1}", "aB", "a\u{0}", "a\n", "a%2Fb",
    ] {
        assert_eq!(AgentName::parse(raw), Err(AgentNameError::InvalidCharacter), "{raw:?}");
    }
    let fixture = fixture(|config| config.gateway.targeted_agents = true).await;
    let (status, value) = post(&fixture, SESSION, serde_json::json!({"message": "hi", "agent": "a b"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&value), Some("agent_invalid"));
    assert_untouched(&fixture).await;
}

#[tokio::test]
async fn agent_name_rejects_overlength() {
    assert!(AgentName::parse(&"a".repeat(AGENT_NAME_MAX_LEN)).is_ok());
    let long = "a".repeat(AGENT_NAME_MAX_LEN + 1);
    assert_eq!(AgentName::parse(&long), Err(AgentNameError::TooLong));
    let fixture = fixture(|config| config.gateway.targeted_agents = true).await;
    let (status, value) = post(&fixture, SESSION, serde_json::json!({"message": "hi", "agent": long})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&value), Some("agent_invalid"));
    assert_untouched(&fixture).await;
}

#[tokio::test]
async fn agent_name_is_exact_at_all_ingresses() {
    // Config load: non-conforming and reserved legacy names load (with a
    // warning) for delegate/spawn, but are never targetable.
    let config = config_with(
        vec![
            ("main", agent(false, &[])),
            ("Researcher", agent(false, &[])),
            ("white space", agent(false, &[])),
            ("ok-agent", agent(false, &[])),
        ],
        true,
    );
    config.validate().expect("test: legacy agent names still load");
    for key in config.agents.keys() {
        assert_eq!(is_targetable(key), key == "ok-agent", "{key}");
    }
    assert!(
        resolve_target(&config, &named("ok-agent"))
            .expect("test: resolves")
            .is_some()
    );

    // Request field / decoded path segment: exact match, no folding or trimming.
    for raw in [
        "Researcher",
        "OK-AGENT",
        "Ok-Agent",
        " ok-agent",
        "ok-agent ",
        "ok-agent\n",
        "white space",
    ] {
        assert_eq!(
            TurnTarget::from_field(AgentField::Text(raw.to_owned())),
            Err(TargetError::Invalid),
            "{raw:?}"
        );
    }
    // `main` is the one explicit Main spelling; it never resolves to the legacy entry.
    assert_eq!(
        TurnTarget::from_field(AgentField::Text("main".to_owned())),
        Ok(TurnTarget::Main)
    );
    assert!(
        resolve_target(&config, &TurnTarget::Main)
            .expect("test: Main resolves")
            .is_none()
    );

    // Over HTTP the same strings are refused before any effect.
    let fixture = fixture(|cfg| {
        cfg.gateway.targeted_agents = true;
        cfg.agents.insert("ok-agent".to_owned(), agent(false, &[]));
    })
    .await;
    for raw in ["OK-AGENT", "Researcher", " ok-agent"] {
        let (status, value) = post(&fixture, SESSION, serde_json::json!({"message": "hi", "agent": raw})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{raw:?}");
        assert_eq!(error_code(&value), Some("agent_invalid"));
    }
    assert_untouched(&fixture).await;
}

#[tokio::test]
async fn target_allowlist_semantics() {
    let resolve = |cfg: DelegateAgentConfig| {
        resolve_target(&config_with(vec![("worker", cfg)], true), &named("worker"))
            .map(|plan| plan.map(|plan| plan.tools))
    };
    // Non-agentic: no general execution tools, whatever the list holds.
    assert_eq!(resolve(agent(false, &["file_read"])), Ok(Some(ToolAllowlist::None)));
    // Wildcard inherits only the admitted host surface.
    assert_eq!(resolve(agent(true, &["*"])), Ok(Some(ToolAllowlist::Inherit)));
    assert_eq!(resolve(agent(true, &[" * "])), Ok(Some(ToolAllowlist::Inherit)));
    // Mixed wildcard and empty lists are refused.
    for list in [&["*", "file_read"][..], &[][..], &[" ", ""][..]] {
        assert_eq!(resolve(agent(true, list)), Err(TargetError::ToolsInvalid), "{list:?}");
    }
    // Explicit names intersect the host surface; cron and xin never reach a target.
    let explicit = resolve(agent(true, &["file_read", "cron"]))
        .expect("test: resolves")
        .expect("test: plan");
    assert_eq!(
        explicit,
        ToolAllowlist::Only(BTreeSet::from(["file_read".to_owned(), "cron".to_owned()]))
    );
    let surface = explicit.surface();
    assert_eq!(surface.allowed_tools, Some(BTreeSet::from(["file_read".to_owned()])));
    let excluded = BTreeSet::from(["cron".to_owned(), "xin".to_owned()]);
    assert_eq!(surface.excluded_names, excluded);
    let inherit = ToolAllowlist::Inherit.surface();
    assert_eq!(inherit.allowed_tools, None);
    assert_eq!(
        inherit.excluded_names, excluded,
        "the wildcard does not widen past the exclusion"
    );
    assert_eq!(ToolAllowlist::None.surface().allowed_tools, Some(BTreeSet::new()));

    // Over HTTP an invalid agentic list is a typed 400 with no effects.
    let fixture = fixture(|config| {
        config.gateway.targeted_agents = true;
        config
            .agents
            .insert("worker".to_owned(), agent(true, &["*", "file_read"]));
    })
    .await;
    let (status, value) = post(
        &fixture,
        SESSION,
        serde_json::json!({"message": "hi", "agent": "worker"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&value), Some("agent_tools_invalid"));
    assert_untouched(&fixture).await;
}

// ── Enforcement through the real targeted console turn ──────────────────────
//
// These tests run `run_console_runtime_turn` with a target built by the real
// `resolve_target` and `prepare_target` (capped policy, per-target decision
// slot and published tool surface). Only the model transport is scripted: the
// target's provider instance is replaced by a provider that emits fixed tool
// calls, and its binding is rebuilt around that instance.

type ToolBox = Box<dyn crate::tools::Tool>;

/// Emits the scripted calls in its first reply and answers in text after that.
struct ScriptedCalls {
    calls: Vec<(&'static str, serde_json::Value)>,
    requests: parking_lot::Mutex<Vec<Vec<ChatMessage>>>,
}

#[async_trait]
impl Provider for ScriptedCalls {
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
        Ok("summary".to_owned())
    }

    async fn chat(&self, request: ChatRequest<'_>, _model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
        let first = {
            let mut requests = self.requests.lock();
            requests.push(request.messages.to_vec());
            requests.len() == 1
        };
        let tool_calls: Vec<crate::providers::ToolCall> = if first {
            self.calls
                .iter()
                .enumerate()
                .map(|(index, (name, arguments))| crate::providers::ToolCall {
                    id: format!("call-targeted-{index}"),
                    name: (*name).to_owned(),
                    arguments: arguments.to_string(),
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(ChatResponse {
            text: tool_calls.is_empty().then(|| "done".to_owned()),
            tool_calls,
            reasoning_content: None,
        })
    }
}

/// A registered tool that counts backend executions. It borrows the published
/// metadata of `file_read` (or of `file_write` under that name), so it is
/// catalogued like a real filesystem tool.
struct CountingTool {
    name: &'static str,
    inner: ToolBox,
    executions: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl crate::tools::Tool for CountingTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }
    fn tier(&self) -> crate::tools::traits::ToolTier {
        self.inner.tier()
    }
    fn categories(&self) -> &'static [crate::tools::traits::ToolCategory] {
        self.inner.categories()
    }
    fn supports_owner_filesystem(&self) -> bool {
        self.inner.supports_owner_filesystem()
    }
    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        self.executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(crate::tools::ToolResult {
            success: true,
            output: "probe-output".to_owned(),
            error: None,
        })
    }
}

const NOT_EXPOSED: &str = "was not exposed for this turn and was not executed";
const APPROVAL_DENIED: &str = "approval is not available for this agent";
const APPROVAL_EVENT_TYPES: [&str; 8] = [
    "approval.call.pending",
    "approval.call.reviewed",
    "approval.decision.pending",
    "approval.decision.reviewed",
    "approval.decision.issued",
    "approval.decision.resolved",
    "approval.decision.resume_requested",
    "approval.collection.declined",
];

impl Driven {
    fn ran(&self, tool: &str) -> usize {
        self.executions.get(tool).copied().expect("test: counted tool")
    }
}

struct Driven {
    awaiting_approval: bool,
    successful_attempts: u64,
    denied_attempts: u64,
    awaiting_approval_attempts: u64,
    /// The text of every message of the request that followed the tool round.
    follow_up: String,
    executions: std::collections::BTreeMap<&'static str, usize>,
    /// Durable approval rows by event type, read from the memory store.
    rows: std::collections::BTreeMap<&'static str, usize>,
    pending_decisions: usize,
    pending_calls: Option<usize>,
    /// How often the channel approval resolver was consulted.
    approval_asked: usize,
}

/// Records every consultation and denies.
struct DenyingResolver {
    asked: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl crate::agent::turn_contracts::ApprovalResolver for DenyingResolver {
    async fn resolve(
        &self,
        _request: &crate::approval::ApprovalRequest,
        _channel: &str,
    ) -> crate::agent::turn_contracts::ApprovalDecision {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::agent::turn_contracts::ApprovalDecision::Deny
    }
}

/// Run one console turn. `agent_config` is `Some` for a targeted turn on agent
/// `worker`, `None` for a Main turn. Every name in `registry` is a registered
/// counting tool. With `ask_gate`, the run is installed with a channel approval
/// policy that makes every call ask first (the runtime route a read tool takes
/// to "needs approval" in a Run-disposition console turn).
async fn drive(
    main_level: AutonomyLevel,
    agent_config: Option<DelegateAgentConfig>,
    registry: &[&'static str],
    calls: Vec<(&'static str, serde_json::Value)>,
    input: &'static str,
    ask_gate: bool,
) -> Driven {
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("filesystem")
        .await
        .expect("test: decision fixture");
    let endpoint = driver.endpoint().to_owned();
    let registry = registry.to_vec();
    let targeted_request = agent_config.is_some();
    driver
        .run(true, async move {
            let tmp = tempfile::tempdir().expect("test: tempdir");
            let cleanup =
                crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                    .expect("test: cleanup scope");
            let driven = cleanup
                .run(Box::pin(async {
                    let workspace = tmp.path().join("workspace");
                    let private = tmp.path().join("private");
                    std::fs::create_dir_all(&workspace).expect("test: workspace");
                    std::fs::create_dir_all(&private).expect("test: private root");
                    let mut config = Config::default();
                    console_provider_fixture_config(&mut config, &workspace, CONSOLE_WINDOW_MODEL, &endpoint);
                    config.config_path = private.join("config.toml");
                    config.autonomy.level = main_level;
                    config.gateway.targeted_agents = true;
                    config.tool_tiering.always_include = registry.iter().map(|name| (*name).to_owned()).collect();
                    if let Some(mut worker) = agent_config {
                        worker.model = CONSOLE_WINDOW_MODEL.to_owned();
                        config.agents.insert("worker".to_owned(), worker);
                    }
                    let scripted = Arc::new(ScriptedCalls {
                        calls,
                        requests: parking_lot::Mutex::new(Vec::new()),
                    });
                    let mut state = test_app_state_with_config(config, Arc::new(CapturingProvider::default()));
                    state.mem = Arc::new(
                        crate::memory::SqliteMemory::new(&state.config.pin().effective.workspace_dir)
                            .expect("test: sqlite memory"),
                    );
                    let security = Arc::new(crate::security::SecurityPolicy::from_config(
                        &state.config.pin().effective.autonomy,
                        &workspace,
                    ));
                    let counters: std::collections::BTreeMap<&'static str, Arc<std::sync::atomic::AtomicUsize>> =
                        registry
                            .iter()
                            .map(|name| (*name, Arc::new(std::sync::atomic::AtomicUsize::new(0))))
                            .collect();
                    state.tools_registry = Arc::new(
                        registry
                            .iter()
                            .map(|name| {
                                let inner: ToolBox = if *name == "file_write" {
                                    Box::new(crate::tools::file_write::FileWriteTool::new(Arc::clone(&security)))
                                } else {
                                    Box::new(crate::tools::file_read::FileReadTool::new(Arc::clone(&security), false))
                                };
                                Box::new(CountingTool {
                                    name,
                                    inner,
                                    executions: Arc::clone(counters.get(name).expect("test: counter")),
                                }) as ToolBox
                            })
                            .collect(),
                    );

                    // The parent runtime, exactly as the console fixtures build it.
                    let mut parent = state.pin_turn_runtime();
                    let effective = Arc::clone(&parent.config_generation.effective);
                    let catalog = crate::providers::selection::catalog_for(&effective);
                    let resolve = |provider: &str| {
                        crate::providers::selection::check_with(
                            &effective,
                            &catalog,
                            provider,
                            CONSOLE_WINDOW_MODEL,
                            crate::providers::selection::credential_for(&effective, provider),
                            &crate::providers::catalog::Requirement::chat(),
                            crate::providers::catalog::UnknownPolicy::Invocation,
                        )
                        .expect("test: model resolves")
                    };
                    {
                        let parent = Arc::get_mut(&mut parent).expect("test: unshared runtime");
                        parent.provider = scripted.clone();
                        parent.model = CONSOLE_WINDOW_MODEL.to_owned();
                        parent.provider_binding =
                            Some(Arc::new(crate::gateway::decision_source::GatewayProviderBinding::new(
                                scripted.clone(),
                                resolve("ollama"),
                            )));
                        parent.decision_slot = Arc::new(crate::gateway::decision_runtime::GatewayDecisionSlot::new(
                            Arc::new(crate::security::SecurityPolicy::from_config(
                                &effective.autonomy,
                                &workspace,
                            )),
                            0,
                        ));
                    }

                    // The real target resolution and runtime derivation; only the
                    // provider instance is swapped for the scripted transport.
                    let plan = if targeted_request {
                        resolve_target(&effective, &named("worker")).expect("test: resolves")
                    } else {
                        None
                    };
                    let (runtime, targeted) = match plan {
                        Some(plan) => {
                            let (mut runtime, turn) = prepare_target(&parent, &plan).expect("test: target runtime");
                            {
                                let derived = Arc::get_mut(&mut runtime).expect("test: unshared target runtime");
                                derived.provider = scripted.clone();
                                derived.provider_binding =
                                    Some(Arc::new(crate::gateway::decision_source::GatewayProviderBinding::new(
                                        scripted.clone(),
                                        resolve("ollama"),
                                    )));
                            }
                            (runtime, Some(turn))
                        }
                        None => (parent, None),
                    };

                    let principal = super::super::super::ApiPrincipal::of_token("prx-test-token");
                    let envelope = console_runtime_envelope_for_workspace(
                        workspace.to_string_lossy().to_string(),
                        "console-targeted-session",
                        "console",
                        "prx_user",
                    )
                    .with_run_id(uuid::Uuid::new_v4().to_string())
                    .with_config_generation(&runtime.config_generation)
                    .with_owner_id(principal.owner_id.clone())
                    .with_owner_isolation();
                    let event_id = append_console_turn(&state, &envelope, "user", input, true, false)
                        .await
                        .expect("test: record the input");
                    let proof = crate::gateway::decision_source::GatewaySourceReceipt::console_proof(
                        state.mem.as_ref(),
                        &runtime,
                        &principal,
                        &envelope,
                        input,
                        &event_id,
                    )
                    .await
                    .expect("test: admitted source");
                    let resolver = Arc::new(DenyingResolver {
                        asked: std::sync::atomic::AtomicUsize::new(0),
                    });
                    if ask_gate {
                        let run_id = envelope.run_id.clone().expect("test: run id");
                        crate::agent::channel_approval::install(&run_id, resolver.clone(), true, false);
                    }
                    let service = runtime
                        .decision_slot
                        .publish(&runtime, &state)
                        .await
                        .expect("test: publish")
                        .clone();
                    let cancellation = tokio_util::sync::CancellationToken::new();
                    let context = service
                        .decide_agent_initial(
                            &proof,
                            crate::runtime::turn_decision::AgentTurnKind::SingleShot,
                            &cancellation,
                        )
                        .await
                        .expect("test: decision");
                    assert!(
                        matches!(
                            context.disposition(),
                            Ok(crate::runtime::turn_decision::TurnDisposition::Run)
                        ),
                        "the turn must reach the tool loop, not collection or a direct reply"
                    );
                    let decision = ConsoleDecisionTurn {
                        service: Arc::clone(&service),
                        context,
                        proof: &proof,
                        cancellation,
                        provider_trace: None,
                    };
                    let turn = run_console_runtime_turn(
                        Arc::clone(&runtime),
                        &state,
                        input,
                        Vec::new(),
                        &decision,
                        targeted.as_ref(),
                    )
                    .await
                    .expect("test: console turn completes");

                    let owner = principal.owner_id.clone();
                    let workspace_id = proof.envelope().workspace_id.clone();
                    let mut rows = std::collections::BTreeMap::new();
                    for event_type in APPROVAL_EVENT_TYPES {
                        let found = state
                            .mem
                            .list_owned_message_events_by_type(&workspace_id, &owner, event_type, 0, 50)
                            .await
                            .expect("test: read approval rows");
                        rows.insert(event_type, found.len());
                    }
                    let pending_decisions = service
                        .list_pending_decision_approvals(&owner, 0, 10)
                        .await
                        .expect("test: pending decision approvals")
                        .items
                        .len();
                    let pending_calls = match service.call_approval_strategy(proof.envelope().clone()) {
                        Ok(adapter) => Some(
                            adapter
                                .port
                                .list_pending_calls(&owner, 0, 10)
                                .await
                                .expect("test: pending call approvals")
                                .items
                                .len(),
                        ),
                        Err(_) => None,
                    };
                    service
                        .shutdown(std::time::Instant::now() + Duration::from_secs(2))
                        .await;
                    if let Some(run_id) = envelope.run_id.as_deref() {
                        crate::agent::channel_approval::clear(run_id);
                    }

                    let requests = scripted.requests.lock().clone();
                    let follow_up = requests
                        .get(1)
                        .map(|messages| {
                            messages
                                .iter()
                                .map(|message| message.content.as_str())
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    let snapshot = turn.trace.execution_trace.clone().expect("test: execution trace");
                    Ok::<_, anyhow::Error>(Driven {
                        awaiting_approval: turn.awaiting_approval,
                        successful_attempts: snapshot.successful_attempts,
                        denied_attempts: snapshot.denied_attempts,
                        awaiting_approval_attempts: snapshot.awaiting_approval_attempts,
                        follow_up,
                        executions: counters
                            .iter()
                            .map(|(name, count)| (*name, count.load(std::sync::atomic::Ordering::SeqCst)))
                            .collect(),
                        rows,
                        pending_decisions,
                        pending_calls,
                        approval_asked: resolver.asked.load(std::sync::atomic::Ordering::SeqCst),
                    })
                }))
                .await;
            driven.expect("test: scripted turn and cleanup")
        })
        .await
}

#[tokio::test]
async fn targeted_turn_cannot_dispatch_excluded_or_unlisted_tool() {
    // `cron` is excluded even under a wildcard; `file_grep` is registered but
    // not on the agent's explicit allowlist; `file_read` is listed. The model
    // calls all three in one reply.
    let probe = serde_json::json!({"path": "probe.txt"});
    let calls = vec![
        ("cron", probe.clone()),
        ("file_grep", probe.clone()),
        ("file_read", probe),
    ];
    let input = "file_read probe.txt: show the probe file";
    for allowlist in [&["file_read", "cron"][..], &["*"][..]] {
        let driven = drive(
            AutonomyLevel::Full,
            Some(agent(true, allowlist)),
            &["cron", "file_grep", "file_read"],
            calls.clone(),
            input,
            false,
        )
        .await;
        assert_eq!(driven.ran("cron"), 0, "cron never runs ({allowlist:?})");
        assert_eq!(driven.ran("file_grep"), 0, "unlisted never runs ({allowlist:?})");
        assert_eq!(
            driven.denied_attempts, 2,
            "both refused calls end denied ({allowlist:?})"
        );
        assert_eq!(
            driven.follow_up.matches(NOT_EXPOSED).count(),
            2,
            "{allowlist:?}: {}",
            driven.follow_up
        );
        assert_eq!(
            driven.ran("file_read"),
            1,
            "the listed tool still works ({allowlist:?})"
        );
        assert_eq!(driven.successful_attempts, 1, "{allowlist:?}");
        assert!(driven.follow_up.contains("probe-output"), "{allowlist:?}");
        assert!(!driven.awaiting_approval);
    }
}

#[tokio::test]
async fn targeted_turn_approval_call_ends_denied_without_pending_state() {
    // The turn runs `file_read` under a run policy that makes every call ask
    // first. The agent asks for Full, but is capped at Main's level.
    let mut worker = agent(true, &["file_read"]);
    worker.autonomy = Some(AutonomyLevel::Full);
    let calls = vec![("file_read", serde_json::json!({"path": "probe.txt"}))];
    let input = "file_read probe.txt: show the probe file";

    let targeted = drive(
        AutonomyLevel::Supervised,
        Some(worker),
        &["file_read"],
        calls.clone(),
        input,
        true,
    )
    .await;
    assert_eq!(targeted.ran("file_read"), 0, "the backend never runs");
    assert!(
        targeted.follow_up.contains(&format!("Denied: {APPROVAL_DENIED}")),
        "the call ended ApprovalDenied: {}",
        targeted.follow_up
    );
    assert_eq!(targeted.approval_asked, 0, "no approval route was entered");
    assert_eq!(targeted.denied_attempts, 1);
    assert_eq!(targeted.awaiting_approval_attempts, 0);
    assert_eq!(targeted.successful_attempts, 0);
    assert!(!targeted.awaiting_approval, "the turn is not waiting for anyone");
    assert_eq!(targeted.pending_decisions, 0, "no pending decision");
    assert_eq!(targeted.pending_calls.unwrap_or(0), 0, "no pending call");
    for (event_type, count) in &targeted.rows {
        assert_eq!(*count, 0, "no {event_type} row was written");
    }

    // Control: the same call on a Main turn reaches the ordinary approval path.
    let main = drive(AutonomyLevel::Supervised, None, &["file_read"], calls, input, true).await;
    assert_eq!(main.approval_asked, 1, "Main consults the approval resolver");
    assert_eq!(main.ran("file_read"), 0, "the resolver denied the call");
    assert!(!main.follow_up.contains(APPROVAL_DENIED));
}

// ── Upload ownership ────────────────────────────────────────────────────────

const BOUNDARY: &str = "prx-test-boundary";

fn multipart_request(body: String) -> Request {
    let mut request = Request::builder()
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .expect("test: request");
    request
        .extensions_mut()
        .insert(super::super::super::ApiPrincipal::of_token("prx-test-token"));
    request
}

fn text_part(name: &str, value: &str) -> String {
    format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
}

fn file_part(file_name: &str, content: &str) -> String {
    format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{file_name}\"\r\n\
         Content-Type: text/plain\r\n\r\n{content}\r\n"
    )
}

async fn post_multipart(fixture: &Fixture, session: &str, body: String) -> StatusCode {
    post_session_message(
        State(fixture.state.clone()),
        Path(session.to_owned()),
        multipart_request(body),
    )
    .await
    .into_response()
    .status()
}

fn stored_uploads(fixture: &Fixture) -> Vec<(String, String)> {
    let root = fixture.state.config.pin().effective.workspace_dir.join("uploads");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut files: Vec<(String, String)> = entries
        .map(|entry| {
            let path = entry.expect("test: entry").path();
            (
                path.file_name().expect("test: name").to_string_lossy().into_owned(),
                std::fs::read_to_string(&path).expect("test: read upload"),
            )
        })
        .collect();
    files.sort();
    files
}

#[tokio::test]
async fn refused_target_cleanup_removes_only_its_own_upload() {
    // The switch is off, so the named request is refused after its file is
    // stored. A Main request sent the same file name in the same instant (its
    // session does not exist, so its file stays behind). The refusal must
    // remove only the file the refused request created.
    let fixture = fixture(|config| config.gateway.targeted_agents = false).await;
    let main_body = format!(
        "{}{}--{BOUNDARY}--\r\n",
        text_part("message", "main request"),
        file_part("same.txt", "MAIN-CONTENT")
    );
    assert_eq!(
        post_multipart(&fixture, "no-such-session", main_body).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(stored_uploads(&fixture).len(), 1, "the Main upload is stored");

    let named_body = format!(
        "{}{}{}--{BOUNDARY}--\r\n",
        text_part("message", "named request"),
        text_part("agent", "helper"),
        file_part("same.txt", "NAMED-CONTENT")
    );
    assert_eq!(
        post_multipart(&fixture, SESSION, named_body).await,
        StatusCode::CONFLICT
    );
    let left = stored_uploads(&fixture);
    assert_eq!(left.len(), 1, "only the refused request's file was removed: {left:?}");
    assert_eq!(
        left.first().map(|(_, content)| content.as_str()),
        Some("MAIN-CONTENT"),
        "the Main request's file is untouched"
    );
    assert_untouched(&fixture).await;
}

#[tokio::test]
async fn multipart_parse_error_after_a_file_leaves_no_upload() {
    // A completed file followed by a truncated `agent` field: the parser
    // fails after the file is on disk, and nothing may remain.
    let fixture = fixture(|config| config.gateway.targeted_agents = true).await;
    let body = format!(
        "{}{}--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"agent\"\r\n\r\nhelp",
        text_part("message", "hi"),
        file_part("kept.txt", "CONTENT")
    );
    let status = post_multipart(&fixture, SESSION, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(stored_uploads(&fixture), Vec::new(), "no upload is left behind");
    assert_untouched(&fixture).await;
}

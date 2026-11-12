#![cfg(test)]
//! Targeted agent selection: selector, name grammar, rollout switch, autonomy
//! cap and allowlist semantics.
use super::*;
use crate::config::DelegateAgentConfig;
use crate::config::agent_name::{AGENT_NAME_MAX_LEN, AgentName, AgentNameError, is_targetable};
use crate::memory::embeddings::EmbeddingProvider;
use crate::memory::owner_predicate::OwnerScopedStoreRefusal;
use crate::memory::{
    AgentPartition, AgentScopeRefused, AgentScopeUnsupported, CompactionRunInput, Memory, MemoryCategory,
    MemoryDraftInput, MemoryFabric, MemoryPrincipal, MemoryVisibility, MemoryWriteContext, NoneMemory, PartitionKey,
    RetrievalTraceInput,
};
use crate::security::AutonomyLevel;
use crate::tools::document_ingest::DocumentIngestTool;
use crate::tools::memory_store::MemoryStoreTool;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};

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

/// Like [`fixture`], but keeps a caller-configured decision backend and the
/// Console private-filesystem layout `drive()` uses.
///
/// `test_app_state` (`sessions.rs`, `causal_tree.enabled = false`) would drop
/// the native-tool surface this POST needs, even after `configure(...)`.
/// Default `config_path` is the user config dir; without a temp private root,
/// `console_filesystem` stays `None` and owner-isolated `file_read` is gated.
async fn fixture_with_decision_backend(config: impl FnOnce(&mut Config)) -> Fixture {
    let temp = tempfile::tempdir().expect("test: tempdir");
    let workspace = temp.path().join("workspace");
    let private = temp.path().join("private");
    std::fs::create_dir_all(&workspace).expect("test: workspace");
    std::fs::create_dir_all(&private).expect("test: private root");
    let mut base = Config::default();
    base.workspace_dir = workspace.clone();
    base.config_path = private.join("config.toml");
    config(&mut base);
    let provider = Arc::new(CapturingProvider::default());
    let mut state = test_app_state_with_config(base, provider.clone());
    let memory = Arc::new(crate::memory::SqliteMemory::new(&workspace).expect("test: sqlite memory"));
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

/// Queue of call batches. Each `chat` pops one; an empty queue answers in text.
struct ScriptedCalls {
    batches: parking_lot::Mutex<Vec<Vec<(&'static str, serde_json::Value)>>>,
    requests: parking_lot::Mutex<Vec<Vec<ChatMessage>>>,
}

impl ScriptedCalls {
    fn batches(batches: Vec<Vec<(&'static str, serde_json::Value)>>) -> Self {
        Self {
            batches: parking_lot::Mutex::new(batches),
            requests: parking_lot::Mutex::new(Vec::new()),
        }
    }
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
        self.requests.lock().push(request.messages.to_vec());
        let batch = {
            let mut batches = self.batches.lock();
            if batches.is_empty() {
                None
            } else {
                Some(batches.remove(0))
            }
        };
        let tool_calls: Vec<crate::providers::ToolCall> = batch
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(index, (name, arguments))| crate::providers::ToolCall {
                id: format!("call-targeted-{index}"),
                name: (*name).to_owned(),
                arguments: arguments.to_string(),
            })
            .collect();
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
                    let scripted = Arc::new(ScriptedCalls::batches(vec![calls]));
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
        // Async mode answers 202 once the input is admitted, without waiting for the turn.
        .uri("/?mode=async")
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

async fn post_multipart(fixture: &Fixture, session: &str, body: String) -> (StatusCode, String) {
    let response = post_session_message(
        State(fixture.state.clone()),
        Path(session.to_owned()),
        multipart_request(body),
    )
    .await
    .into_response();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("test: body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
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

const ADMITTED_SESSION: &str = "prx-admitted-session";

/// Wire the fixture like a running gateway, so a Main request on
/// `ADMITTED_SESSION` is actually admitted (input recorded, job submitted).
async fn make_admissible(fixture: &mut Fixture) -> anyhow::Result<()> {
    let workspace = fixture.state.config.pin().effective.workspace_dir.clone();
    let envelope = console_runtime_envelope_for_workspace(
        workspace.to_string_lossy().as_ref(),
        ADMITTED_SESSION,
        "console",
        "prx_user",
    );
    let (runtime, _, _, anchor) = console_test_source(&mut fixture.state, envelope, "Admission fixture anchor").await;
    drop(anchor);
    let current = Arc::try_unwrap(runtime).map_err(|_| anyhow::anyhow!("fixture runtime unexpectedly shared"))?;
    fixture.state.turn_runtime = Some(Arc::new(crate::gateway::GatewayTurnRuntimeOwner::new(
        current,
        Arc::clone(&fixture.state.config),
        Arc::clone(&fixture.state.mem),
        None,
        None,
        None,
        None,
        Arc::clone(&fixture.state.hooks),
        #[cfg(feature = "wasm-plugins")]
        None,
    )));
    Ok(())
}

#[tokio::test]
async fn refused_target_cleanup_removes_only_its_own_upload() -> anyhow::Result<()> {
    // The switch is off, so the named request is refused after its file is
    // stored. An ADMITTED Main request sent the same file name (its session
    // exists, so its file now belongs to the turn). The refusal must remove
    // only the file the refused request created.
    let mut fixture = fixture(|config| config.gateway.targeted_agents = false).await;
    make_admissible(&mut fixture).await?;
    let main_body = format!(
        "{}{}--{BOUNDARY}--\r\n",
        text_part("message", "main request"),
        file_part("same.txt", "MAIN-CONTENT")
    );
    let (status, reply) = post_multipart(&fixture, ADMITTED_SESSION, main_body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{reply}");
    assert_eq!(stored_uploads(&fixture).len(), 1, "the admitted Main upload is stored");

    let named_body = format!(
        "{}{}{}--{BOUNDARY}--\r\n",
        text_part("message", "named request"),
        text_part("agent", "helper"),
        file_part("same.txt", "NAMED-CONTENT")
    );
    let (status, reply) = post_multipart(&fixture, SESSION, named_body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    let left = stored_uploads(&fixture);
    assert_eq!(left.len(), 1, "only the refused request's file was removed: {left:?}");
    assert_eq!(
        left.first().map(|(_, content)| content.as_str()),
        Some("MAIN-CONTENT"),
        "the admitted Main request's file is untouched"
    );
    Ok(())
}

#[tokio::test]
async fn failed_main_admission_removes_its_upload() {
    // A Main request whose session does not exist fails admission after its
    // file is stored; the file must not outlive the request.
    let fixture = fixture(|config| config.gateway.targeted_agents = false).await;
    let body = format!(
        "{}{}--{BOUNDARY}--\r\n",
        text_part("message", "main request"),
        file_part("gone.txt", "MAIN-CONTENT")
    );
    let (status, reply) = post_multipart(&fixture, "no-such-session", body).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{reply}");
    assert_eq!(stored_uploads(&fixture), Vec::new(), "no upload is left behind");
    assert_untouched(&fixture).await;
}

#[test]
fn upload_path_is_owned_before_creation_and_cancellation_leaves_no_file() {
    use std::sync::atomic::Ordering;
    let temp = tempfile::tempdir().expect("test: tempdir");

    // Ordering invariant: the guard knows the path while the file does not
    // exist yet.
    let early = temp.path().join("early.bin");
    let mut guard = UploadGuard::default();
    let abandoned = guard.begin(early.clone());
    assert!(guard.paths.contains(&early), "the path is registered");
    assert!(!early.exists(), "before the file exists");

    // Cancellation between registration and creation: the guard drops first,
    // the blocking worker runs afterwards and must not leave a file.
    drop(guard);
    assert!(abandoned.load(Ordering::SeqCst), "the abandonment is published");
    assert!(create_upload_file(&early, &abandoned).is_err());
    assert!(!early.exists(), "a worker running after the drop removes its own file");

    // Cancellation after creation: the guard removes the file it registered.
    let late = temp.path().join("late.bin");
    let mut guard = UploadGuard::default();
    let abandoned = guard.begin(late.clone());
    let file = create_upload_file(&late, &abandoned).expect("test: created");
    assert!(late.exists());
    drop(guard);
    drop(file);
    assert!(!late.exists(), "the guard removes a created file");

    // A released guard hands the file over.
    let kept = temp.path().join("kept.bin");
    let mut guard = UploadGuard::default();
    let abandoned = guard.begin(kept.clone());
    drop(create_upload_file(&kept, &abandoned).expect("test: created"));
    guard.release();
    assert!(kept.exists(), "a released upload stays");
}

#[test]
fn upload_already_exists_preserves_the_existing_file() {
    let temp = tempfile::tempdir().expect("test: tempdir");
    let sentinel = temp.path().join("sentinel.bin");
    std::fs::write(&sentinel, b"PRE-EXISTING").expect("test: sentinel");

    let mut guard = UploadGuard::default();
    let abandoned = guard.begin(sentinel.clone());
    let outcome = guard.settle(&sentinel, create_upload_file(&sentinel, &abandoned));
    let error = outcome.expect_err("test: create_new must refuse an existing file");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(
        guard.paths.is_empty(),
        "a path this request did not create is unregistered"
    );

    drop(guard);
    assert_eq!(
        std::fs::read(&sentinel).expect("test: sentinel must still exist"),
        b"PRE-EXISTING"
    );
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
    let (status, reply) = post_multipart(&fixture, SESSION, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
    assert_eq!(stored_uploads(&fixture), Vec::new(), "no upload is left behind");
    assert_untouched(&fixture).await;
}

const TARGET_U: &str = "00000000-0000-4000-8000-0000000000a1";
const TARGET_SCOPE: &str = "00000000-0000-4000-8000-0000000000c1";
const TARGET_MAIN_U: &str = "00000000-0000-4000-8000-0000000000b1";
const TARGET_MAIN_SCOPE: &str = "00000000-0000-4000-8000-0000000000d1";
const TARGET_REV: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const CAPTURE_PROVIDER: &str = "custom:http://127.0.0.1/prx-ma-01b2-partition";
const PARTITION_PROVIDER: &str = "custom:http://127.0.0.1/prx-ma-01b3-partition";
const WRITE_SCOPE_PROVIDER: &str = "custom:http://127.0.0.1/prx-ma-01b3-1-scope";
const FENCE_ERASE: &str = "00000000-0000-4000-8000-0000000000e1";
const FENCE_MAIN_ERASE: &str = "00000000-0000-4000-8000-0000000000e2";
const RETIRED_U: &str = "00000000-0000-4000-8000-0000000000a9";
const WRONG_U: &str = "00000000-0000-4000-8000-0000000000a8";

fn capture_provider_config(config: &mut Config, provider: &str, agent_cfg: DelegateAgentConfig) {
    config.default_provider = Some(provider.to_owned());
    config.default_model = Some("test-model".to_owned());
    config.cost.prices.insert(
        format!("{provider}/test-model"),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config.providers.models.push(
        toml::from_str(&format!(
            "provider = {provider:?}\nmodel = \"test-model\"\noperations = [\"chat\"]\nnative_tools = true\n"
        ))
        .expect("test: catalog"),
    );
    let mut agent_cfg = agent_cfg;
    agent_cfg.provider = provider.to_owned();
    agent_cfg.model = "test-model".to_owned();
    config.agents.insert("x".to_owned(), agent_cfg);
}

fn captured_contents(provider: &CapturingProvider, index: usize) -> Vec<String> {
    provider
        .requests
        .lock()
        .get(index)
        .expect("test: captured request")
        .iter()
        .map(|message| message.content.clone())
        .collect()
}

fn assert_history_partition(contents: &[String], present: &[&str], absent: &[&str], label: &str) {
    for item in present {
        assert!(
            contents.iter().any(|content| content.contains(item)),
            "{label} missing {item} in {contents:?}"
        );
    }
    for item in absent {
        assert!(
            !contents.iter().any(|content| content.contains(item)),
            "{label} leaked {item} in {contents:?}"
        );
    }
}

fn named_partition(workspace: &str, owner: &str, name: &str, incarnation: &str) -> AgentPartition {
    AgentPartition {
        workspace_id: workspace.to_owned(),
        owner_id: owner.to_owned(),
        key: PartitionKey::Named {
            name: AgentName::parse(name).expect("test: agent name"),
            incarnation: incarnation.parse().expect("test: incarnation"),
        },
    }
}

fn main_partition(workspace: &str, owner: &str) -> AgentPartition {
    AgentPartition {
        workspace_id: workspace.to_owned(),
        owner_id: owner.to_owned(),
        key: PartitionKey::Main,
    }
}

fn history_reader(workspace: &str, session: &str, partition: AgentPartition) -> MemoryPrincipal {
    MemoryPrincipal {
        workspace_id: workspace.to_owned(),
        agent_id: Some("system".to_owned()),
        persona_id: None,
        session_key: Some(session.to_owned()),
        channel: Some("console".to_owned()),
        sender: Some("api:local".to_owned()),
        owner_id: Some("system:*".to_owned()),
        require_owner_match: false,
        legacy_session_key: None,
        agent_partition: Some(partition),
    }
}

fn stamp_console_rows(dir: &std::path::Path, workspace: &str, owner: &str, session: &str) {
    let conn = rusqlite::Connection::open(dir.join("memory").join("brain.db")).expect("test: open store");
    conn.pragma_update(None, "foreign_keys", "ON").expect("test: fk");
    conn.execute(
        "INSERT INTO agent_incarnations VALUES (?1, ?2, 'n:x', ?3, 'active', ?4)",
        rusqlite::params![workspace, owner, TARGET_U, TARGET_REV],
    )
    .expect("test: incarnation x");
    conn.execute(
        "INSERT INTO agent_incarnations VALUES (?1, ?2, 'm', ?3, 'active', ?4)",
        rusqlite::params![workspace, owner, TARGET_MAIN_U, TARGET_REV],
    )
    .expect("test: incarnation main");
    conn.execute(
        "INSERT INTO agent_scopes VALUES (?1, ?2, ?3, ?4, 'n:x', ?5, 1)",
        rusqlite::params![TARGET_SCOPE, workspace, owner, session, TARGET_U],
    )
    .expect("test: scope x");
    conn.execute(
        "INSERT INTO agent_scopes VALUES (?1, ?2, ?3, ?4, 'm', ?5, 1)",
        rusqlite::params![TARGET_MAIN_SCOPE, workspace, owner, session, TARGET_MAIN_U],
    )
    .expect("test: scope main");
    for (label, scope) in [
        ("x-only", Some(TARGET_SCOPE)),
        ("main-row", Some(TARGET_MAIN_SCOPE)),
        ("null-row", None),
    ] {
        conn.execute(
            "INSERT INTO conversation_turns (session_key, owner_id, role, content, timestamp, ma_scope_id)
             VALUES (?1, ?2, 'user', ?3, 't', ?4)",
            rusqlite::params![session, owner, label, scope],
        )
        .expect("test: stamp turn");
    }
}

#[tokio::test]
async fn sqlite_targeted_scope_supported() {
    let fixture = fixture(|config| {
        config.gateway.targeted_agents = true;
        config.autonomy.level = AutonomyLevel::Full;
        capture_provider_config(config, CAPTURE_PROVIDER, agent(false, &[]));
    })
    .await;
    crate::providers::test_fixture::register(
        CAPTURE_PROVIDER,
        Arc::clone(&fixture.provider) as Arc<dyn crate::providers::Provider>,
    );
    assert!(fixture.memory.supports_agent_scope());
    let workspace = fixture
        .state
        .config
        .pin()
        .effective
        .workspace_dir
        .to_string_lossy()
        .into_owned();
    let owner = super::super::super::ApiPrincipal::of_token("prx-test-token").owner_id;
    stamp_console_rows(std::path::Path::new(&workspace), &workspace, &owner, SESSION);

    let named = history_reader(&workspace, SESSION, named_partition(&workspace, &owner, "x", TARGET_U));
    let named_turns = fixture
        .memory
        .list_console_conversation_turns(&named, SESSION, 50, 0)
        .await
        .expect("test: named turns");
    let named_labels: Vec<&str> = named_turns.iter().map(|turn| turn.content.as_str()).collect();
    assert_eq!(named_labels, ["x-only"], "Named X sees no Y/Main/NULL rows");

    let main = history_reader(&workspace, SESSION, main_partition(&workspace, &owner));
    let main_turns = fixture
        .memory
        .list_console_conversation_turns(&main, SESSION, 50, 0)
        .await
        .expect("test: main turns");
    let main_labels: Vec<&str> = main_turns.iter().map(|turn| turn.content.as_str()).collect();
    assert!(main_labels.contains(&"main-row") && main_labels.contains(&"null-row"));
    assert!(!main_labels.contains(&"x-only"));

    let api_principal = super::super::super::ApiPrincipal::of_token("prx-test-token");
    let selected = super::super::select_turn_target(
        &fixture.state,
        fixture.state.pin_turn_runtime(),
        &api_principal,
        AgentField::Text("x".to_owned()),
    )
    .expect("test: named target");
    assert_eq!(selected.incarnation.as_deref(), Some(TARGET_U));

    let (status, value) = post(&fixture, SESSION, serde_json::json!({"message": "hi", "agent": "x"})).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_history_partition(
        &captured_contents(&fixture.provider, 0),
        &["x-only"],
        &["main-row", "null-row"],
        "Named POST",
    );

    let (status, value) = post(&fixture, SESSION, serde_json::json!({"message": "hi"})).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_history_partition(
        &captured_contents(&fixture.provider, 1),
        &["main-row", "null-row"],
        &["x-only"],
        "Main POST",
    );
}

const SCOPE_TOOL: &str = "file_read";

/// Records every argument set the runtime delivers to it, `_zc_scope` included.
/// Named and catalogued as `file_read` (CountingTool pattern) so the published
/// surface includes it.
struct ScopeRecorder {
    seen: Arc<parking_lot::Mutex<Vec<serde_json::Value>>>,
    inner: crate::tools::file_read::FileReadTool,
}

#[async_trait]
impl crate::tools::Tool for ScopeRecorder {
    fn name(&self) -> &str {
        SCOPE_TOOL
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
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        self.seen.lock().push(args);
        Ok(crate::tools::ToolResult {
            success: true,
            output: "recorded".to_owned(),
            error: None,
        })
    }
}

/// A console input literal whose only varying field is the in-process partition (R0-2).
fn digest_cell_input(agent_partition: Option<AgentPartition>) -> crate::memory::MessageEventInput {
    crate::memory::MessageEventInput {
        agent_partition,
        event_id: None,
        idempotency_key: Some("console:run-1:input".to_owned()),
        workspace_id: "workspace-a".to_owned(),
        owner_id: Some("owner-a".to_owned()),
        source: crate::memory::MessageEventSource::Console,
        channel: Some("console".to_owned()),
        session_key: Some(SESSION.to_owned()),
        parent_session_key: None,
        run_id: None,
        parent_run_id: None,
        agent_id: None,
        persona_id: None,
        sender: None,
        recipient: None,
        role: "user".to_owned(),
        event_type: "message".to_owned(),
        subject: None,
        goal_id: None,
        causation_event_id: None,
        correlation_id: None,
        attempt_id: None,
        lease_epoch: None,
        config_generation_id: None,
        config_source_revision: None,
        content: "hi".to_owned(),
        raw_payload_json: None,
        visibility: crate::memory::MemoryVisibility::Session,
    }
}

/// MA-01b3 part 0: the partition and session reach every writer input. The
/// envelope views are asserted directly; the POST views assert what the
/// runtime delivers to a tool (`_zc_scope`) and what `parse_scope_ctx` reads
/// back from it.
#[tokio::test]
async fn agent_partition_reaches_every_writer_input() {
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("filesystem")
        .await
        .expect("test: decision fixture");
    let endpoint = driver.endpoint().to_owned();
    driver
        .run(true, async move {
            let mut fixture = fixture_with_decision_backend(|config| {
                config.gateway.targeted_agents = true;
                config.autonomy.level = AutonomyLevel::Full;
                config.tool_tiering.always_include = vec![SCOPE_TOOL.to_owned()];
                capture_provider_config(config, PARTITION_PROVIDER, agent(true, &[SCOPE_TOOL]));
                crate::agent::agent::native_decision_fixture::configure(config, &endpoint)
                    .expect("test: decision backend");
            })
            .await;
            let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let workspace_dir = fixture.state.config.pin().effective.workspace_dir.clone();
            let security = Arc::new(crate::security::SecurityPolicy::from_config(
                &fixture.state.config.pin().effective.autonomy,
                &workspace_dir,
            ));
            fixture.state.tools_registry = Arc::new(vec![Box::new(ScopeRecorder {
                seen: Arc::clone(&seen),
                inner: crate::tools::file_read::FileReadTool::new(security, false),
            }) as ToolBox]);
            let workspace = workspace_dir.to_string_lossy().into_owned();
            let owner = super::super::super::ApiPrincipal::of_token("prx-test-token").owner_id;
            stamp_console_rows(std::path::Path::new(&workspace), &workspace, &owner, SESSION);

            let named = named_partition(&workspace, &owner, "x", TARGET_U);
            let envelope = crate::runtime::envelope::RuntimeEnvelope::console(workspace.clone(), SESSION.to_owned())
                .with_agent_partition(named.clone());
            let write_context = envelope.memory_write_context("direct");
            assert_eq!(
                serde_json::to_vec(&digest_cell_input(Some(named.clone()))).expect("test: input json"),
                serde_json::to_vec(&digest_cell_input(None)).expect("test: input json"),
                "MessageEventInput serialisation (capsule digest input) ignores the partition"
            );
            assert_eq!(
                write_context.agent_partition.as_ref(),
                Some(&named),
                "write context carries the partition"
            );
            assert_eq!(
                write_context.session_key.as_deref(),
                Some(SESSION),
                "write context carries the session"
            );
            assert_eq!(
                envelope.message_scope().agent_partition.as_ref(),
                Some(&named),
                "message scope carries the partition"
            );

            for (label, body, expected) in [
                (
                    "Named POST",
                    serde_json::json!({"message": "hi", "agent": "x"}),
                    named.clone(),
                ),
                (
                    "Main POST",
                    serde_json::json!({"message": "hi"}),
                    main_partition(&workspace, &owner),
                ),
            ] {
                let scripted = Arc::new(ScriptedCalls::batches(vec![vec![(
                    SCOPE_TOOL,
                    serde_json::json!({"path": "probe.txt"}),
                )]]));
                // The fixture holds only a Weak handle: keep this Arc alive for the POST.
                let provider = Arc::clone(&scripted) as Arc<dyn crate::providers::Provider>;
                crate::providers::test_fixture::register(PARTITION_PROVIDER, Arc::clone(&provider));
                // Main pins `AppState.provider`; Named builds through the factory.
                fixture.state.provider = Arc::clone(&provider);
                seen.lock().clear();
                let (status, value) = post(&fixture, SESSION, body).await;
                drop(provider);
                assert_eq!(status, StatusCode::OK, "{label}: {value}");
                let args = seen.lock().last().cloned().expect("test: recorder ran");
                let scope = args.get("_zc_scope").expect("test: runtime scope");
                assert_eq!(
                    scope.get("agent_partition"),
                    Some(&serde_json::to_value(&expected).expect("test: partition json")),
                    "{label}: _zc_scope carries the partition"
                );
                assert_eq!(
                    scope.get("session_key").and_then(serde_json::Value::as_str),
                    Some(SESSION),
                    "{label}: _zc_scope carries the session"
                );
                let context = crate::tools::memory_store::parse_scope_ctx(&args).expect("test: trusted scope");
                assert_eq!(
                    context.agent_partition.as_ref(),
                    Some(&expected),
                    "{label}: parsed partition"
                );
                assert_eq!(context.session_key.as_deref(), Some(SESSION), "{label}: parsed session");
            }
        })
        .await;
}

fn brain(workspace: &str) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(std::path::Path::new(workspace).join("memory").join("brain.db"))
        .expect("test: brain");
    conn.pragma_update(None, "foreign_keys", "ON").expect("test: fk");
    conn
}

fn seed_named_x(dir: &std::path::Path, workspace: &str, owner: &str, session: &str) {
    let conn = rusqlite::Connection::open(dir.join("memory").join("brain.db")).expect("test: open store");
    conn.pragma_update(None, "foreign_keys", "ON").expect("test: fk");
    conn.execute(
        "INSERT INTO agent_incarnations VALUES (?1, ?2, 'n:x', ?3, 'active', ?4)",
        rusqlite::params![workspace, owner, TARGET_U, TARGET_REV],
    )
    .expect("test: incarnation x");
    conn.execute(
        "INSERT INTO agent_scopes VALUES (?1, ?2, ?3, ?4, 'n:x', ?5, 1)",
        rusqlite::params![TARGET_SCOPE, workspace, owner, session, TARGET_U],
    )
    .expect("test: scope x");
}

fn fence_scope(conn: &rusqlite::Connection, scope_id: &str, erase_id: &str) {
    conn.execute(
        "INSERT INTO agent_erase_fences VALUES (?1, ?2, 'draining', 2)",
        rusqlite::params![scope_id, erase_id],
    )
    .expect("test: fence");
}

fn table_columns(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    let sql = match table {
        "message_events" | "conversation_turns" | "memory_events" | "memories" | "documents" | "document_chunks"
        | "embedding_cache" | "memory_drafts" | "memory_trash" => {
            format!("PRAGMA table_info({table})")
        }
        _ => panic!("test: unknown store {table}"),
    };
    let mut statement = conn.prepare(&sql).expect("test: pragma");
    statement
        .query_map([], |row| row.get::<_, String>(1))
        .expect("test: columns")
        .map(|column| column.expect("test: column name"))
        .collect()
}

fn scope_rows(conn: &rusqlite::Connection, sql: &str, session: &str) -> Vec<Option<String>> {
    let mut statement = conn.prepare(sql).expect("test: prepare");
    statement
        .query_map([session], |row| row.get(0))
        .expect("test: query")
        .map(|value| value.expect("test: scope"))
        .collect()
}

fn write_input(
    workspace: &str,
    owner: &str,
    partition: Option<AgentPartition>,
    session: Option<&str>,
    content: &str,
) -> crate::memory::MessageEventInput {
    let mut input = digest_cell_input(partition);
    input.workspace_id = workspace.to_owned();
    input.owner_id = Some(owner.to_owned());
    input.session_key = session.map(str::to_owned);
    input.idempotency_key = None;
    input.content = content.to_owned();
    input
}

/// MA-01b3 part 1: transcript/event writers stamp the resolved agent scope.
/// Real POST for Named X, Main mint (D-CR1), D-CR3 column probe, and refusals.
#[tokio::test]
async fn memory_writes_carry_scope() {
    let fixture = fixture(|config| {
        config.gateway.targeted_agents = true;
        config.autonomy.level = AutonomyLevel::Full;
        capture_provider_config(config, WRITE_SCOPE_PROVIDER, agent(false, &[]));
    })
    .await;
    crate::providers::test_fixture::register(
        WRITE_SCOPE_PROVIDER,
        Arc::clone(&fixture.provider) as Arc<dyn crate::providers::Provider>,
    );
    let workspace = fixture
        .state
        .config
        .pin()
        .effective
        .workspace_dir
        .to_string_lossy()
        .into_owned();
    let owner = super::super::super::ApiPrincipal::of_token("prx-test-token").owner_id;
    seed_named_x(std::path::Path::new(&workspace), &workspace, &owner, SESSION);
    let conn = brain(&workspace);

    const NINE: [&str; 9] = [
        "message_events",
        "conversation_turns",
        "memory_events",
        "memories",
        "documents",
        "document_chunks",
        "embedding_cache",
        "memory_drafts",
        "memory_trash",
    ];
    let mut dcr3 = Vec::new();
    for table in NINE {
        let columns = table_columns(&conn, table);
        assert!(
            columns.iter().any(|column| column == "ma_scope_id"),
            "{table} has ma_scope_id"
        );
        dcr3.push(format!("{table}: {}", columns.join(",")));
    }
    // D-CR3: no unambiguous FK identifies pre-01b3 targeted rows (target.rs has
    // selector state only). Parent FKs (message_event_id, subject_id) serve D-CR2.
    eprintln!("D-CR3 columns: {}", dcr3.join(" | "));

    let (status, value) = post(
        &fixture,
        SESSION,
        serde_json::json!({"message": "named-x", "agent": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "Named POST: {value}");

    let named_events = scope_rows(
        &conn,
        "SELECT ma_scope_id FROM message_events
         WHERE session_key = ?1 AND role IN ('user', 'assistant')
         ORDER BY id",
        SESSION,
    );
    let named_turns = scope_rows(
        &conn,
        "SELECT ma_scope_id FROM conversation_turns
         WHERE session_key = ?1 AND role IN ('user', 'assistant')
         ORDER BY id",
        SESSION,
    );
    let named_mirrors = scope_rows(
        &conn,
        "SELECT ma_scope_id FROM memory_events
         WHERE session_key = ?1 AND subject_table = 'message_events'
           AND event_type IN ('message', 'message.created')
         ORDER BY id",
        SESSION,
    );
    let named_user: i64 = conn
        .query_row(
            "SELECT count(*) FROM message_events WHERE session_key = ?1 AND role = 'user' AND ma_scope_id = ?2",
            rusqlite::params![SESSION, TARGET_SCOPE],
            |row| row.get(0),
        )
        .expect("test: named user events");
    let named_assistant: i64 = conn
        .query_row(
            "SELECT count(*) FROM message_events WHERE session_key = ?1 AND role = 'assistant' AND ma_scope_id = ?2",
            rusqlite::params![SESSION, TARGET_SCOPE],
            |row| row.get(0),
        )
        .expect("test: named assistant events");
    assert!(named_user >= 1, "Named POST wrote input message_events");
    assert!(named_assistant >= 1, "Named POST wrote assistant message_events");
    assert!(!named_events.is_empty(), "Named POST wrote message_events");
    assert!(
        named_events.iter().all(|scope| scope.as_deref() == Some(TARGET_SCOPE)),
        "message_events row carries X's scope: {named_events:?}"
    );
    assert!(
        named_turns.iter().all(|scope| scope.as_deref() == Some(TARGET_SCOPE)),
        "conversation_turns row carries X's scope: {named_turns:?}"
    );
    assert!(
        named_mirrors.iter().all(|scope| scope.as_deref() == Some(TARGET_SCOPE)),
        "memory_events row carries X's scope: {named_mirrors:?}"
    );
    assert!(
        named_events.iter().all(Option::is_some)
            && named_turns.iter().all(Option::is_some)
            && named_mirrors.iter().all(Option::is_some),
        "Named X wrote zero NULL rows"
    );

    let main_incarnations: i64 = conn
        .query_row(
            "SELECT count(*) FROM agent_incarnations WHERE workspace_id = ?1 AND owner_id = ?2 AND agent_key = 'm'",
            rusqlite::params![&workspace, &owner],
            |row| row.get(0),
        )
        .expect("test: main incarnation count");
    assert_eq!(main_incarnations, 0, "D-CR1: no Main incarnation before the Main POST");

    let (status, value) = post(&fixture, SESSION, serde_json::json!({"message": "main-hi"})).await;
    assert_eq!(status, StatusCode::OK, "Main POST: {value}");

    let minted_u: String = conn
        .query_row(
            "SELECT incarnation FROM agent_incarnations
             WHERE workspace_id = ?1 AND owner_id = ?2 AND agent_key = 'm' AND state = 'active'",
            rusqlite::params![&workspace, &owner],
            |row| row.get(0),
        )
        .expect("test: D-CR1 minted m");
    let main_scopes: Vec<String> = {
        let mut statement = conn
            .prepare(
                "SELECT scope_id FROM agent_scopes
                 WHERE workspace_id = ?1 AND owner_id = ?2 AND session_key = ?3 AND agent_key = 'm'",
            )
            .expect("test: main scopes");
        statement
            .query_map(rusqlite::params![&workspace, &owner, SESSION], |row| row.get(0))
            .expect("test: main scope rows")
            .map(|value| value.expect("test: scope id"))
            .collect()
    };
    assert_eq!(main_scopes.len(), 1, "D-CR1: one Main scope");
    let main_scope = main_scopes.first().expect("test: D-CR1 one Main scope");
    let main_event_scope: Option<String> = conn
        .query_row(
            "SELECT ma_scope_id FROM message_events WHERE session_key = ?1 AND content = 'main-hi'",
            [SESSION],
            |row| row.get(0),
        )
        .expect("test: main event");
    assert_eq!(
        main_event_scope.as_deref(),
        Some(main_scope.as_str()),
        "Main rows carry the minted scope"
    );
    assert_eq!(minted_u.len(), 36, "D-CR1 minted an incarnation UUID");

    let events_before: i64 = conn
        .query_row(
            "SELECT count(*) FROM message_events WHERE session_key = ?1",
            [SESSION],
            |row| row.get(0),
        )
        .expect("test: count");
    fence_scope(&conn, TARGET_SCOPE, FENCE_ERASE);
    let (status, value) = post(
        &fixture,
        SESSION,
        serde_json::json!({"message": "fenced-x", "agent": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "fenced Named: {value}");
    assert_eq!(error_code(&value), Some("agent_scope_refused"));
    let events_after: i64 = conn
        .query_row(
            "SELECT count(*) FROM message_events WHERE session_key = ?1",
            [SESSION],
            |row| row.get(0),
        )
        .expect("test: count after fence");
    assert_eq!(events_after, events_before, "fenced Named wrote zero rows");

    fence_scope(&conn, main_scope, FENCE_MAIN_ERASE);
    let (status, value) = post(&fixture, SESSION, serde_json::json!({"message": "fenced-main"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "fenced Main: {value}");
    assert_eq!(error_code(&value), Some("agent_scope_refused"));
    let fenced_main: i64 = conn
        .query_row(
            "SELECT count(*) FROM message_events WHERE content = 'fenced-main'",
            [],
            |row| row.get(0),
        )
        .expect("test: fenced main rows");
    assert_eq!(fenced_main, 0, "fenced Main wrote zero rows");

    let named = named_partition(&workspace, &owner, "x", TARGET_U);
    let envelope = crate::runtime::envelope::RuntimeEnvelope::console(workspace.clone(), SESSION.to_owned())
        .with_agent_partition(named.clone())
        .with_owner_id(owner.clone());
    let error = append_console_turn(&fixture.state, &envelope, "user", "no-parent", false, false)
        .await
        .expect_err("test: Named missing parent event");
    assert!(
        error.downcast_ref::<AgentScopeRefused>().is_some(),
        "W4 Named + missing parent: {error:#}"
    );

    conn.execute(
        "INSERT INTO agent_incarnations VALUES (?1, ?2, 'n:x', ?3, 'retired', ?4)",
        rusqlite::params![&workspace, &owner, RETIRED_U, TARGET_REV],
    )
    .expect("test: retired incarnation");
    for (label, input) in [
        (
            "retired incarnation",
            write_input(
                &workspace,
                &owner,
                Some(named_partition(&workspace, &owner, "x", RETIRED_U)),
                Some(SESSION),
                "retired",
            ),
        ),
        (
            "wrong U",
            write_input(
                &workspace,
                &owner,
                Some(named_partition(&workspace, &owner, "x", WRONG_U)),
                Some(SESSION),
                "wrong-u",
            ),
        ),
        (
            "missing session_key",
            write_input(&workspace, &owner, Some(named.clone()), None, "no-session"),
        ),
        (
            "workspace mismatch",
            write_input(
                "other-workspace",
                &owner,
                Some(named.clone()),
                Some(SESSION),
                "mismatch",
            ),
        ),
        (
            "owner mismatch",
            write_input(
                &workspace,
                "other-owner",
                Some(named.clone()),
                Some(SESSION),
                "owner-mismatch",
            ),
        ),
    ] {
        let error = fixture.memory.append_message_event(input).await.expect_err(label);
        assert!(
            error.downcast_ref::<AgentScopeRefused>().is_some(),
            "{label}: {error:#}"
        );
    }
    for content in ["retired", "wrong-u", "no-session", "mismatch", "owner-mismatch"] {
        let leaked: i64 = conn
            .query_row(
                "SELECT count(*) FROM message_events WHERE content = ?1",
                [content],
                |row| row.get(0),
            )
            .expect("test: leak");
        assert_eq!(leaked, 0, "{content} wrote zero rows");
    }

    complete_memory_family_scope_stamps().await;
}

#[tokio::test]
async fn none_targeted_scope_rejected() {
    let memory = NoneMemory::new();
    assert!(!memory.supports_agent_scope());
    let principal = history_reader(
        "prx_workspace",
        "session",
        named_partition("prx_workspace", "prx_owner", "x", TARGET_U),
    );
    let turns = memory
        .list_console_conversation_turns(&principal, "session", 10, 0)
        .await
        .expect_err("test: named turns");
    assert!(turns.downcast_ref::<AgentScopeUnsupported>().is_some(), "{turns:#}");
    let events = memory
        .list_message_events_since(&principal, 0, 10)
        .await
        .expect_err("test: named events");
    assert!(events.downcast_ref::<AgentScopeUnsupported>().is_some(), "{events:#}");

    // Job snapshots span all fixtures in this process. Identify only this
    // rejected request rather than another test's legitimately submitted job.
    let session_key = format!("prx-rejected-scope-{}", uuid::Uuid::new_v4());
    let temp = tempfile::tempdir().expect("test: tempdir");
    let mut config = Config::default();
    config.workspace_dir = temp.path().to_owned();
    config.gateway.targeted_agents = true;
    config.agents.insert("x".to_owned(), agent(false, &[]));
    let provider = Arc::new(CapturingProvider::default());
    let mut state = test_app_state(config, provider.clone());
    state.mem = Arc::new(NoneMemory::new());
    let response = post_session_message(
        State(state.clone()),
        Path(session_key.clone()),
        http_request(serde_json::json!({"message": "hi", "agent": "x"})),
    )
    .await
    .into_response();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("test: body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("test: json");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_code(&body), Some("agent_backend_unsupported"));
    assert!(provider.requests.lock().is_empty(), "no provider access");
    assert!(state.mem.agent_registry().is_none(), "no registry row");
    let session = state
        .mem
        .get_conversation_session(&session_key)
        .await
        .expect("test: read session");
    assert!(session.is_none(), "no input event");
    let job_label = format!("gateway:session_message:{session_key}");
    assert!(
        crate::gateway::jobs::snapshot_all()
            .iter()
            .all(|job| job.label != job_label),
        "no job"
    );
}

const STORE_SCOPE_PROVIDER: &str = "custom:http://127.0.0.1/prx-ma-01b3-2-store";
const DOC_SCOPE_PROVIDER: &str = "custom:http://127.0.0.1/prx-ma-01b3-3-docs";
const TOPIC_SCOPE_PROVIDER: &str = "custom:http://127.0.0.1/prx-ma-01b3-3-topics";
const SESSION_S2: &str = "prx-target-session-s2";
const STORE_TOOL: &str = "memory_store";
const INGEST_TOOL: &str = "document_ingest";

struct ScopeCountingEmbedder {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl EmbeddingProvider for ScopeCountingEmbedder {
    fn name(&self) -> &str {
        "counting"
    }

    fn dimensions(&self) -> usize {
        4
    }

    fn model(&self) -> &str {
        "counting-v1"
    }

    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3, 0.4]).collect())
    }
}

async fn fixture_with_scope_embedder(config: impl FnOnce(&mut Config), embed_calls: Arc<AtomicUsize>) -> Fixture {
    let temp = tempfile::tempdir().expect("test: tempdir");
    let workspace = temp.path().join("workspace");
    let private = temp.path().join("private");
    std::fs::create_dir_all(&workspace).expect("test: workspace");
    std::fs::create_dir_all(&private).expect("test: private root");
    let mut base = Config::default();
    base.workspace_dir = workspace.clone();
    base.config_path = private.join("config.toml");
    config(&mut base);
    let provider = Arc::new(CapturingProvider::default());
    let mut state = test_app_state_with_config(base, provider.clone());
    let memory = Arc::new(
        crate::memory::SqliteMemory::with_embedder(
            &workspace,
            Arc::new(ScopeCountingEmbedder { calls: embed_calls }),
            0.7,
            0.3,
            1000,
            None,
        )
        .expect("test: sqlite memory"),
    );
    state.mem = memory.clone();
    memory
        .create_conversation_session(SESSION, "api:local")
        .await
        .expect("test: session");
    memory
        .create_conversation_session(SESSION_S2, "api:local")
        .await
        .expect("test: session s2");
    Fixture {
        state,
        provider,
        memory,
        _temp: temp,
    }
}

fn install_scope_tools(fixture: &mut Fixture) {
    let workspace_dir = fixture.state.config.pin().effective.workspace_dir.clone();
    let security = Arc::new(crate::security::SecurityPolicy::from_config(
        &fixture.state.config.pin().effective.autonomy,
        &workspace_dir,
    ));
    let memory: Arc<dyn crate::memory::Memory> = fixture.memory.clone();
    fixture.state.tools_registry = Arc::new(vec![
        Box::new(MemoryStoreTool::new(Arc::clone(&memory), Arc::clone(&security))) as ToolBox,
        Box::new(DocumentIngestTool::new(workspace_dir, memory, security)) as ToolBox,
    ]);
}

async fn post_scripted(
    fixture: &mut Fixture,
    provider: &str,
    session: &str,
    body: serde_json::Value,
    tool: &'static str,
    args: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let scripted = Arc::new(ScriptedCalls::batches(vec![vec![(tool, args)]]));
    let provider_arc = Arc::clone(&scripted) as Arc<dyn crate::providers::Provider>;
    crate::providers::test_fixture::register(provider, Arc::clone(&provider_arc));
    fixture.state.provider = Arc::clone(&provider_arc);
    let result = post(fixture, session, body).await;
    drop(provider_arc);
    result
}

fn partitioned_write_ctx(workspace: &str, owner: &str, session: &str, partition: AgentPartition) -> MemoryWriteContext {
    MemoryWriteContext {
        workspace_id: Some(workspace.to_owned()),
        owner_id: Some(owner.to_owned()),
        session_key: Some(session.to_owned()),
        agent_partition: Some(partition),
        channel: Some("console".to_owned()),
        ..Default::default()
    }
}

fn configure_stamped_scope(config: &mut Config, endpoint: &str, provider: &str, tools: &[&str]) {
    config.gateway.targeted_agents = true;
    config.autonomy.level = AutonomyLevel::Full;
    config.tool_tiering.always_include = tools.iter().map(|tool| (*tool).to_owned()).collect();
    capture_provider_config(config, provider, agent(true, tools));
    crate::agent::agent::native_decision_fixture::configure(config, endpoint).expect("test: decision backend");
}

fn truncated_content_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .take(16)
        .fold(String::with_capacity(32), |mut hex, byte| {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn assert_named_store_row(conn: &rusqlite::Connection, key: &str, content: &str) {
    let (scope, topic): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT ma_scope_id, topic_id FROM memories WHERE key = ?1",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("test: stored memories row");
    assert_eq!(
        scope.as_deref(),
        Some(TARGET_SCOPE),
        "stored memories row carries X's scope"
    );
    assert!(topic.is_none(), "X's memory has topic_id NULL");
    let fts: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories_fts f JOIN memories m ON m.rowid = f.rowid WHERE m.key = ?1",
            [key],
            |row| row.get(0),
        )
        .expect("test: memories_fts");
    assert_eq!(fts, 1, "memories_fts carries X's memory");
    let salted: i64 = conn
        .query_row(
            "SELECT count(*) FROM embedding_cache WHERE ma_scope_id = ?1 AND length(content_hash) = 64",
            [TARGET_SCOPE],
            |row| row.get(0),
        )
        .expect("test: salted cache");
    assert!(salted >= 1, "salted embedding_cache row carries X's scope");
    let plain_hash = truncated_content_hash(content);
    let plain: i64 = conn
        .query_row(
            "SELECT count(*) FROM embedding_cache WHERE content_hash = ?1",
            [plain_hash],
            |row| row.get(0),
        )
        .expect("test: plain hash");
    assert_eq!(plain, 0, "no plain-hash embedding_cache row for the stored text");
}

/// Part 2 completion of `memory_writes_carry_scope`: memory-family writers.
async fn complete_memory_family_scope_stamps() {
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("memory")
        .await
        .expect("test: decision fixture");
    let endpoint = driver.endpoint().to_owned();
    driver
        .run(true, async move {
            let mut fixture = fixture_with_scope_embedder(
                |config| configure_stamped_scope(config, &endpoint, STORE_SCOPE_PROVIDER, &[STORE_TOOL]),
                Arc::new(AtomicUsize::new(0)),
            )
            .await;
            install_scope_tools(&mut fixture);
            let workspace = fixture
                .state
                .config
                .pin()
                .effective
                .workspace_dir
                .to_string_lossy()
                .into_owned();
            let owner = super::super::super::ApiPrincipal::of_token("prx-test-token").owner_id;
            seed_named_x(std::path::Path::new(&workspace), &workspace, &owner, SESSION);
            let named = named_partition(&workspace, &owner, "x", TARGET_U);
            let main = main_partition(&workspace, &owner);
            let named_ctx = partitioned_write_ctx(&workspace, &owner, SESSION, named.clone());
            let main_s1 = partitioned_write_ctx(&workspace, &owner, SESSION, main.clone());
            let main_s2 = partitioned_write_ctx(&workspace, &owner, SESSION_S2, main.clone());

            let (status, value) = post_scripted(
                &mut fixture,
                STORE_SCOPE_PROVIDER,
                SESSION,
                serde_json::json!({"message": "store-x", "agent": "x"}),
                STORE_TOOL,
                serde_json::json!({"key": "named-core", "content": "hi", "category": "core"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "Named store POST: {value}");
            let conn = brain(&workspace);
            assert_named_store_row(&conn, "named-core", "hi");

            let (status, value) = post_scripted(
                &mut fixture,
                STORE_SCOPE_PROVIDER,
                SESSION,
                serde_json::json!({"message": "store-main"}),
                STORE_TOOL,
                serde_json::json!({"key": "main-core", "content": "ok", "category": "core"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "Main store POST: {value}");
            let main_scope: String = conn
                .query_row(
                    "SELECT ma_scope_id FROM memories WHERE key = 'main-core'",
                    [],
                    |row| row.get::<_, Option<String>>(0),
                )
                .expect("test: main store")
                .expect("test: main stamp");

            fixture
                .memory
                .store_with_context("session-key", "s1", MemoryCategory::Core, None, Some(&main_s1))
                .await
                .expect("test: Main S1");
            fixture
                .memory
                .store_with_context("session-key", "s2", MemoryCategory::Core, None, Some(&main_s2))
                .await
                .expect("test: Main S2");
            let s2_scope: String = conn
                .query_row(
                    "SELECT ma_scope_id FROM memories WHERE key = 'session-key'",
                    [],
                    |row| row.get::<_, Option<String>>(0),
                )
                .expect("test: S2 row")
                .expect("test: S2 stamp");
            let s2_id: String = conn
                .query_row(
                    "SELECT scope_id FROM agent_scopes WHERE session_key = ?1 AND agent_key = 'm'",
                    [SESSION_S2],
                    |row| row.get(0),
                )
                .expect("test: S2 scope");
            assert_eq!(s2_scope, s2_id, "Main S2 write carries the S2 scope");

            conn.execute(
                "INSERT INTO memories (id, key, content, category, created_at, updated_at) VALUES ('legacy-1', 'legacy-null', 'old', 'core', 't', 't')",
                [],
            )
            .expect("test: null legacy");
            fixture
                .memory
                .store_with_context("legacy-null", "new", MemoryCategory::Core, None, Some(&main_s1))
                .await
                .expect("test: Main over NULL");
            let stamped: Option<String> = conn
                .query_row("SELECT ma_scope_id FROM memories WHERE key = 'legacy-null'", [], |row| {
                    row.get(0)
                })
                .expect("test: legacy stamp");
            assert!(stamped.is_some(), "Main over a NULL legacy row succeeds");

            conn.execute(
                "INSERT INTO memories (id, key, content, category, created_at, updated_at) VALUES ('null-2', 'named-over-null', 'keep', 'core', 't', 't')",
                [],
            )
            .expect("test: named-over-null seed");
            let cache_before: i64 = conn
                .query_row("SELECT count(*) FROM embedding_cache", [], |row| row.get(0))
                .expect("test: cache before refuse");
            let refused = fixture
                .memory
                .store_with_context(
                    "named-over-null",
                    "take",
                    MemoryCategory::Core,
                    None,
                    Some(&named_ctx),
                )
                .await
                .expect_err("test: Named over NULL");
            assert!(
                refused.downcast_ref::<OwnerScopedStoreRefusal>().is_some(),
                "Named over NULL: {refused:#}"
            );
            let kept: String = conn
                .query_row("SELECT content FROM memories WHERE key = 'named-over-null'", [], |row| {
                    row.get(0)
                })
                .expect("test: original");
            assert_eq!(kept, "keep", "original unchanged");
            let cache_after: i64 = conn
                .query_row("SELECT count(*) FROM embedding_cache", [], |row| row.get(0))
                .expect("test: cache after refuse");
            assert_eq!(cache_after, cache_before, "a refused write leaves no embedding_cache row");

            let foreign = fixture
                .memory
                .store_with_context("foreign-key", "main-owns", MemoryCategory::Core, None, Some(&main_s1))
                .await;
            foreign.expect("test: foreign seed");
            let refused = fixture
                .memory
                .store_with_context("foreign-key", "named-take", MemoryCategory::Core, None, Some(&named_ctx))
                .await
                .expect_err("test: Named over foreign");
            assert!(
                refused.downcast_ref::<OwnerScopedStoreRefusal>().is_some(),
                "Named over foreign: {refused:#}"
            );

            let refused = fixture
                .memory
                .store("named-core", "unstamped", MemoryCategory::Core, None)
                .await
                .expect_err("test: unstamped over Named");
            assert!(
                refused.downcast_ref::<OwnerScopedStoreRefusal>().is_some(),
                "unstamped over Named: {refused:#}"
            );
            fixture
                .memory
                .store("main-core", "ctx-less-main", MemoryCategory::Core, None)
                .await
                .expect("test: ctx-less over Main");
            let kept_main: Option<String> = conn
                .query_row("SELECT ma_scope_id FROM memories WHERE key = 'main-core'", [], |row| {
                    row.get(0)
                })
                .expect("test: ctx-less main stamp");
            assert_eq!(kept_main.as_deref(), Some(main_scope.as_str()), "ctx-less upsert keeps the Main stamp");

            let draft = fixture
                .memory
                .create_memory_draft(MemoryDraftInput {
                    agent_partition: Some(named.clone()),
                    draft_id: Some("draft-named".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    worker_run_id: "run-1".to_owned(),
                    parent_run_id: None,
                    session_key: Some(SESSION.to_owned()),
                    agent_id: None,
                    persona_id: None,
                    key: "main-core".to_owned(),
                    content: "draft-take".to_owned(),
                    category: MemoryCategory::Core,
                    source_event_id: None,
                    visibility: MemoryVisibility::Workspace,
                    payload_json: None,
                })
                .await
                .expect("test: named draft");
            let principal = history_reader(&workspace, SESSION, named.clone());
            let merge_err = fixture
                .memory
                .merge_memory_draft(&principal, &draft.draft_id)
                .await
                .expect_err("test: Named draft merge over Main");
            assert!(
                merge_err.downcast_ref::<OwnerScopedStoreRefusal>().is_some(),
                "Named draft merge over Main: {merge_err:#}"
            );

            fixture
                .memory
                .append_retrieval_trace(RetrievalTraceInput {
                    agent_partition: Some(main.clone()),
                    trace_id: Some("trace-shared".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    session_key: Some(SESSION.to_owned()),
                    agent_id: None,
                    persona_id: None,
                    source: "test".to_owned(),
                    query: "q".to_owned(),
                    candidate_count: 1,
                    selected_count: 1,
                    dropped_count: 0,
                    budget_tokens: None,
                    selected_json: None,
                    dropped_json: None,
                    payload_json: None,
                })
                .await
                .expect("test: main trace");
            let trace_err = fixture
                .memory
                .append_retrieval_trace(RetrievalTraceInput {
                    agent_partition: Some(named.clone()),
                    trace_id: Some("trace-shared".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    session_key: Some(SESSION.to_owned()),
                    agent_id: None,
                    persona_id: None,
                    source: "test".to_owned(),
                    query: "q2".to_owned(),
                    candidate_count: 1,
                    selected_count: 1,
                    dropped_count: 0,
                    budget_tokens: None,
                    selected_json: None,
                    dropped_json: None,
                    payload_json: None,
                })
                .await
                .expect_err("test: trace collision");
            assert!(
                trace_err.downcast_ref::<AgentScopeRefused>().is_some(),
                "trace_id collision: {trace_err:#}"
            );

            fixture
                .memory
                .append_compaction_run(CompactionRunInput {
                    agent_partition: Some(main.clone()),
                    run_id: Some("run-shared".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    session_key: Some(SESSION.to_owned()),
                    agent_id: None,
                    persona_id: None,
                    trigger: "test".to_owned(),
                    mode: "summary".to_owned(),
                    source_message_count: 1,
                    source_token_estimate: 1,
                    summary: "s".to_owned(),
                    summary_memory_key: None,
                    source_event_ids_json: None,
                    source_event_range_json: None,
                    source_document_refs_json: None,
                    fidelity_status: "full".to_owned(),
                    payload_json: None,
                })
                .await
                .expect("test: main compaction");
            let run_err = fixture
                .memory
                .append_compaction_run(CompactionRunInput {
                    agent_partition: Some(named.clone()),
                    run_id: Some("run-shared".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    session_key: Some(SESSION.to_owned()),
                    agent_id: None,
                    persona_id: None,
                    trigger: "test".to_owned(),
                    mode: "summary".to_owned(),
                    source_message_count: 1,
                    source_token_estimate: 1,
                    summary: "other".to_owned(),
                    summary_memory_key: None,
                    source_event_ids_json: None,
                    source_event_range_json: None,
                    source_document_refs_json: None,
                    fidelity_status: "full".to_owned(),
                    payload_json: None,
                })
                .await
                .expect_err("test: run collision");
            assert!(
                run_err.downcast_ref::<AgentScopeRefused>().is_some(),
                "run_id collision: {run_err:#}"
            );

            let fabric = MemoryFabric::new(fixture.memory.clone(), workspace.clone());
            let envelope = crate::runtime::envelope::RuntimeEnvelope::console(workspace.clone(), SESSION.to_owned())
                .with_agent_partition(named.clone())
                .with_owner_id(owner.clone());
            let draft = fabric
                .create_worker_memory_draft(
                    &envelope.message_scope(),
                    "run-fabric",
                    "fabric-draft",
                    "fabric body",
                    MemoryCategory::Core,
                    None,
                    None,
                )
                .await
                .expect("test: fabric draft");
            let draft_scope: Option<String> = conn
                .query_row(
                    "SELECT ma_scope_id FROM memory_drafts WHERE draft_id = ?1",
                    [&draft.draft_id],
                    |row| row.get(0),
                )
                .expect("test: draft stamp");
            assert_eq!(draft_scope.as_deref(), Some(TARGET_SCOPE));
            fixture
                .memory
                .reject_memory_draft(&principal, &draft.draft_id, Some("no"))
                .await
                .expect("test: fabric reject");
            let event_scope: Option<String> = conn
                .query_row(
                    "SELECT ma_scope_id FROM memory_events WHERE subject_id = ?1 AND event_type = 'memory.draft.rejected'",
                    [&draft.draft_id],
                    |row| row.get(0),
                )
                .expect("test: reject event");
            assert_eq!(event_scope.as_deref(), Some(TARGET_SCOPE), "derived draft event equals parent scope");

            fixture
                .memory
                .move_to_trash("named-core", "test", 7)
                .await
                .expect("test: trash");
            let trash_scope: Option<String> = conn
                .query_row(
                    "SELECT ma_scope_id FROM memory_trash WHERE memory_key = 'named-core'",
                    [],
                    |row| row.get(0),
                )
                .expect("test: trash stamp");
            assert_eq!(trash_scope.as_deref(), Some(TARGET_SCOPE));
        })
        .await;
}

#[tokio::test]
async fn documents_writes_carry_scope() {
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("document")
        .await
        .expect("test: decision fixture");
    let endpoint = driver.endpoint().to_owned();
    driver
        .run(true, async move {
            let mut fixture = fixture_with_scope_embedder(
                |config| configure_stamped_scope(config, &endpoint, DOC_SCOPE_PROVIDER, &[INGEST_TOOL]),
                Arc::new(AtomicUsize::new(0)),
            )
            .await;
            install_scope_tools(&mut fixture);
            let workspace = fixture
                .state
                .config
                .pin()
                .effective
                .workspace_dir
                .to_string_lossy()
                .into_owned();
            let owner = super::super::super::ApiPrincipal::of_token("prx-test-token").owner_id;
            seed_named_x(std::path::Path::new(&workspace), &workspace, &owner, SESSION);
            let named = named_partition(&workspace, &owner, "x", TARGET_U);
            let main = main_partition(&workspace, &owner);
            let body = format!(
                "# First heading\n{}\n\n# Second heading\n{}",
                "alpha ".repeat(400),
                "bravo ".repeat(400)
            );
            let (status, value) = post_scripted(
                &mut fixture,
                DOC_SCOPE_PROVIDER,
                SESSION,
                serde_json::json!({"message": "ingest-x", "agent": "x"}),
                INGEST_TOOL,
                serde_json::json!({"content": body, "title": "named-doc", "document_id": "doc-x"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "Named ingest POST: {value}");
            let conn = brain(&workspace);
            let stored_docs: Vec<(String, Option<String>)> = conn
                .prepare("SELECT document_id, title FROM documents")
                .expect("test: list documents")
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .expect("test: document rows")
                .map(|row| row.expect("test: document row"))
                .collect();
            let (doc_id, doc_scope): (String, Option<String>) = conn
                .query_row(
                    "SELECT document_id, ma_scope_id FROM documents WHERE title = 'named-doc'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap_or_else(|error| {
                    panic!("test: documents row: {error}; post={value}; stored={stored_docs:?}")
                });
            assert_eq!(
                doc_scope.as_deref(),
                Some(TARGET_SCOPE),
                "documents row carries X's scope"
            );
            let chunks: i64 = conn
                .query_row(
                    "SELECT count(*) FROM document_chunks WHERE document_id = ?1",
                    [&doc_id],
                    |row| row.get(0),
                )
                .expect("test: chunks");
            assert!(chunks > 1, "multi-chunk body");
            let chunk_null: i64 = conn
                .query_row(
                    "SELECT count(*) FROM document_chunks WHERE document_id = ?1 AND (ma_scope_id IS NULL OR ma_scope_id != ?2)",
                    rusqlite::params![&doc_id, TARGET_SCOPE],
                    |row| row.get(0),
                )
                .expect("test: chunk stamps");
            assert_eq!(chunk_null, 0, "every document_chunks row carries X's scope");
            let events: i64 = conn
                .query_row(
                    "SELECT count(*) FROM memory_events WHERE subject_table = 'documents' AND subject_id = ?1 AND ma_scope_id = ?2",
                    rusqlite::params![&doc_id, TARGET_SCOPE],
                    |row| row.get(0),
                )
                .expect("test: document events");
            assert!(events >= 1, "document memory_events carry X's scope");
            let salted: i64 = conn
                .query_row(
                    "SELECT count(*) FROM embedding_cache WHERE ma_scope_id = ?1 AND length(content_hash) = 64",
                    [TARGET_SCOPE],
                    |row| row.get(0),
                )
                .expect("test: chunk cache");
            assert!(salted >= 1, "chunk embeddings are salted cache rows");
            let fts: i64 = conn
                .query_row(
                    "SELECT count(*) FROM document_chunks_fts f JOIN document_chunks c ON c.rowid = f.rowid WHERE c.document_id = ?1",
                    [&doc_id],
                    |row| row.get(0),
                )
                .expect("test: chunks fts");
            assert!(fts > 1, "document_chunks_fts matches");

            let fabric = MemoryFabric::new(fixture.memory.clone(), workspace.clone());
            let marker = format!("[document_ingest_ref]\ndocument_id: {doc_id}\n[/document_ingest_ref]");
            fabric
                .record_semantic_memory_from_event(
                    "semantic-x",
                    &marker,
                    MemoryCategory::Core,
                    Some(SESSION),
                    None,
                    None,
                    None,
                    Some(named.clone()),
                )
                .await
                .expect("test: semantic link");
            let link_scope: Option<String> = conn
                .query_row(
                    "SELECT ma_scope_id FROM memory_links WHERE document_id = ?1",
                    [&doc_id],
                    |row| row.get(0),
                )
                .expect("test: link stamp");
            assert_eq!(link_scope.as_deref(), Some(TARGET_SCOPE), "memory_links row carries X's scope");
            let promoted_scope: Option<String> = conn
                .query_row(
                    "SELECT ma_scope_id FROM memories WHERE key = 'semantic-x'",
                    [],
                    |row| row.get(0),
                )
                .expect("test: promoted memories stamp");
            assert_eq!(
                promoted_scope.as_deref(),
                Some(TARGET_SCOPE),
                "promoted memories row carries X's scope"
            );
            let promoted_event: Option<String> = conn
                .query_row(
                    "SELECT ma_scope_id FROM memory_events WHERE subject_id = 'semantic-x' AND event_type = 'memory.stored'",
                    [],
                    |row| row.get(0),
                )
                .expect("test: promoted memory_events stamp");
            assert_eq!(
                promoted_event.as_deref(),
                Some(TARGET_SCOPE),
                "promoted memory_events row carries X's scope"
            );

            fixture
                .memory
                .ingest_document(crate::memory::DocumentIngestInput {
                    agent_partition: None,
                    document_id: Some("doc-null".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    topic_id: None,
                    task_id: None,
                    source_message_event_id: None,
                    source_kind: "test".to_owned(),
                    source_uri: None,
                    title: Some("null".to_owned()),
                    content: "null document".to_owned(),
                    mime_type: None,
                    visibility: MemoryVisibility::Workspace,
                    metadata_json: None,
                    session_key: Some(SESSION.to_owned()),
                    agent_id: None,
                    persona_id: None,
                })
                .await
                .expect("test: null document");
            fixture
                .memory
                .link_memory_source(crate::memory::MemoryLinkInput {
                    agent_partition: Some(main.clone()),
                    session_key: Some(SESSION.to_owned()),
                    link_id: Some("link-main-null".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    memory_key: Some("semantic-x".to_owned()),
                    memory_event_id: None,
                    message_event_id: None,
                    document_id: "doc-null".to_owned(),
                    chunk_id: None,
                    link_type: "derived_from".to_owned(),
                    payload_json: None,
                })
                .await
                .expect("test: Main may link a NULL document");
            let named_null = fixture
                .memory
                .link_memory_source(crate::memory::MemoryLinkInput {
                    agent_partition: Some(named.clone()),
                    session_key: Some(SESSION.to_owned()),
                    link_id: Some("link-named-null".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    memory_key: Some("semantic-x".to_owned()),
                    memory_event_id: None,
                    message_event_id: None,
                    document_id: "doc-null".to_owned(),
                    chunk_id: None,
                    link_type: "derived_from".to_owned(),
                    payload_json: None,
                })
                .await
                .expect_err("test: Named may not link a NULL document");
            assert!(
                named_null.downcast_ref::<AgentScopeRefused>().is_some(),
                "Named NULL document link: {named_null:#}"
            );

            fixture
                .memory
                .ingest_document(crate::memory::DocumentIngestInput {
                    agent_partition: Some(main.clone()),
                    document_id: Some("doc-collide".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    topic_id: None,
                    task_id: None,
                    source_message_event_id: None,
                    source_kind: "test".to_owned(),
                    source_uri: None,
                    title: Some("main".to_owned()),
                    content: "main document".to_owned(),
                    mime_type: None,
                    visibility: MemoryVisibility::Workspace,
                    metadata_json: None,
                    session_key: Some(SESSION.to_owned()),
                    agent_id: None,
                    persona_id: None,
                })
                .await
                .expect("test: main collide seed");
            let collide = fixture
                .memory
                .ingest_document(crate::memory::DocumentIngestInput {
                    agent_partition: Some(named.clone()),
                    document_id: Some("doc-collide".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    topic_id: None,
                    task_id: None,
                    source_message_event_id: None,
                    source_kind: "test".to_owned(),
                    source_uri: None,
                    title: Some("named".to_owned()),
                    content: "named document".to_owned(),
                    mime_type: None,
                    visibility: MemoryVisibility::Workspace,
                    metadata_json: None,
                    session_key: Some(SESSION.to_owned()),
                    agent_id: None,
                    persona_id: None,
                })
                .await
                .expect_err("test: cross-scope document_id");
            assert!(
                collide.downcast_ref::<AgentScopeRefused>().is_some(),
                "cross-scope document_id: {collide:#}"
            );
            let title: String = conn
                .query_row("SELECT title FROM documents WHERE document_id = 'doc-collide'", [], |row| {
                    row.get(0)
                })
                .expect("test: original title");
            assert_eq!(title, "main", "original unchanged");

            let (status, value) = post_scripted(
                &mut fixture,
                DOC_SCOPE_PROVIDER,
                SESSION,
                serde_json::json!({"message": "ingest-main"}),
                INGEST_TOOL,
                serde_json::json!({"content": "main control document", "title": "main-doc", "document_id": "doc-main"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "Main ingest POST: {value}");
            let main_doc: Option<String> = conn
                .query_row(
                    "SELECT ma_scope_id FROM documents WHERE title = 'main-doc'",
                    [],
                    |row| row.get(0),
                )
                .expect("test: main document");
            assert!(main_doc.is_some(), "Main control document is stamped");
        })
        .await;
}

#[tokio::test]
async fn topics_writes_carry_scope() {
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("memory")
        .await
        .expect("test: decision fixture");
    let endpoint = driver.endpoint().to_owned();
    driver
        .run(true, async move {
            let mut fixture = fixture_with_scope_embedder(
                |config| configure_stamped_scope(config, &endpoint, TOPIC_SCOPE_PROVIDER, &[STORE_TOOL]),
                Arc::new(AtomicUsize::new(0)),
            )
            .await;
            install_scope_tools(&mut fixture);
            let workspace = fixture
                .state
                .config
                .pin()
                .effective
                .workspace_dir
                .to_string_lossy()
                .into_owned();
            let owner = super::super::super::ApiPrincipal::of_token("prx-test-token").owner_id;
            seed_named_x(std::path::Path::new(&workspace), &workspace, &owner, SESSION);
            let main = main_partition(&workspace, &owner);
            let shared = "prx shared topic collision body for named isolation";
            let seed_ctx = MemoryWriteContext {
                workspace_id: Some(workspace.clone()),
                owner_id: Some(owner.clone()),
                session_key: Some(SESSION.to_owned()),
                agent_partition: None,
                channel: Some("console".to_owned()),
                ..Default::default()
            };
            fixture
                .memory
                .store_with_context(
                    "shared-topic-seed",
                    shared,
                    MemoryCategory::Core,
                    None,
                    Some(&seed_ctx),
                )
                .await
                .expect("test: shared topic seed");
            let conn = brain(&workspace);
            let (shared_id, shared_updated, shared_scope): (String, String, Option<String>) = conn
                .query_row(
                    "SELECT id, updated_at, ma_scope_id FROM topics ORDER BY id LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("test: shared topic");

            let (status, value) = post_scripted(
                &mut fixture,
                TOPIC_SCOPE_PROVIDER,
                SESSION,
                serde_json::json!({"message": "topic-x", "agent": "x"}),
                STORE_TOOL,
                serde_json::json!({"key": "named-shared", "content": shared, "category": "core"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "Named shared-topic POST: {value}");
            let (updated_at, scope): (String, Option<String>) = conn
                .query_row(
                    "SELECT updated_at, ma_scope_id FROM topics WHERE id = ?1",
                    [&shared_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("test: shared after");
            assert_eq!(updated_at, shared_updated, "shared-topic-unchanged updated_at");
            assert_eq!(scope, shared_scope, "shared-topic-unchanged ma_scope_id");
            let named_topic: Option<String> = conn
                .query_row(
                    "SELECT topic_id FROM memories WHERE key = 'named-shared'",
                    [],
                    |row| row.get(0),
                )
                .expect("test: named topic_id");
            assert!(named_topic.is_none(), "X's memory has topic_id NULL");

            let unique = "prx unique named topic body for isolation test";
            let (status, value) = post_scripted(
                &mut fixture,
                TOPIC_SCOPE_PROVIDER,
                SESSION,
                serde_json::json!({"message": "topic-x-unique", "agent": "x"}),
                STORE_TOOL,
                serde_json::json!({"key": "named-unique", "content": unique, "category": "core"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "Named unique-topic POST: {value}");
            let x_topics: i64 = conn
                .query_row(
                    "SELECT count(*) FROM topics WHERE ma_scope_id = ?1",
                    [TARGET_SCOPE],
                    |row| row.get(0),
                )
                .expect("test: X topics");
            assert_eq!(x_topics, 1, "X's memory_store creates a topics row stamped X");
            let fts: i64 = conn
                .query_row(
                    "SELECT count(*) FROM topics_fts f JOIN topics t ON t.rowid = f.rowid WHERE t.ma_scope_id = ?1",
                    [TARGET_SCOPE],
                    |row| row.get(0),
                )
                .expect("test: topics_fts");
            assert_eq!(fts, 1, "topics_fts has X's topic only");
            let participants: i64 = conn
                .query_row(
                    "SELECT count(*) FROM topic_participants WHERE topic_id IN (SELECT id FROM topics WHERE ma_scope_id = ?1)",
                    [TARGET_SCOPE],
                    |row| row.get(0),
                )
                .expect("test: participants");
            assert_eq!(participants, 0, "topic_participants have no X rows");
            let aliases: i64 = conn
                .query_row("SELECT count(*) FROM topic_aliases", [], |row| row.get(0))
                .expect("test: aliases");
            assert_eq!(aliases, 0, "topic_aliases have no X rows");

            let main_ctx = partitioned_write_ctx(&workspace, &owner, SESSION, main);
            fixture
                .memory
                .store_with_context("main-unique", "prx main unique topic body for reuse", MemoryCategory::Core, None, Some(&main_ctx))
                .await
                .expect("test: Main unique topic");
            fixture
                .memory
                .store_with_context("main-shared", shared, MemoryCategory::Core, None, Some(&main_ctx))
                .await
                .expect("test: Main reuses NULL/Main topic");
            let main_reuse: Option<String> = conn
                .query_row(
                    "SELECT topic_id FROM memories WHERE key = 'main-shared'",
                    [],
                    |row| row.get(0),
                )
                .expect("test: main reuse");
            assert_eq!(main_reuse.as_deref(), Some(shared_id.as_str()), "Main reuses a NULL/Main topic as today");
        })
        .await;
}

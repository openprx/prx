#![cfg(test)]
//! G5b R4.8: a child's `sessions_spawn` passes exactly the gates of a root
//! spawn at the same autonomy level. Every dispatch runs through the
//! production execution service: the root through an admitted parent turn,
//! the child through its own task turn. The first child uses the scripted in-process model of
//! the ordinary provider fixture; every deeper child reaches the loopback
//! Ollama endpoint below through the production provider selection.

use super::*;
use crate::security::AutonomyLevel;
use std::time::Duration;

/// Marks the arguments of the spawn a scripted model makes next.
const NEXT: &str = "PRX-NEXT ";
const END: &str = " PRX-END";
const DONE: &str = "PRX-RECURSIVE-DONE";
/// Bound on recorded model requests; further requests are answered but not kept.
const MAX_SEEN: usize = 64;
const RUN_DEADLINE: Duration = Duration::from_secs(30);
/// The existing dispatch-approval refusal (`kill_and_events.rs`).
const NO_RESOLVER: &str = "approval required but no approval resolver is available";
const NOT_STRICT: &str = "recursive child manifest refused: child capabilities are not a strict subset of the parent";
/// Loopback Ollama model with no price row: zero-price exempt.
const FREE_MODEL: &str = "prx-free-model";
/// A word `PRX-SEARCH=<query>` in a task makes its model call
/// `capability_search` for `<query>`, one query per request, in order.
const SEARCH: &str = "PRX-SEARCH=";
/// An effectful (`local_write`) tool registered only by the tests that ask.
const WRITE_TOOL: &str = "prx_local_write";

/// One model request as the scripted model saw it.
#[derive(Clone, Debug)]
struct Seen {
    own_task: String,
    tools: Vec<String>,
    results: Vec<String>,
}

#[derive(Clone, Default)]
struct Script {
    seen: Arc<parking_lot::Mutex<Vec<Seen>>>,
}

impl Script {
    /// Record the request and return the tool call to make, if any: the
    /// searches its task names, in order, else (with no tool result yet) the
    /// spawn its task names.
    fn answer(&self, seen: Seen, first_user: &str) -> Option<(&'static str, serde_json::Value)> {
        let next = next_call(seen.results.len(), first_user);
        let mut log = self.seen.lock();
        if log.len() < MAX_SEEN {
            log.push(seen);
        }
        next
    }

    fn for_task(&self, label: &str) -> Vec<Seen> {
        self.seen
            .lock()
            .iter()
            .filter(|seen| seen.own_task.contains(label))
            .cloned()
            .collect()
    }
}

/// The call after `answered` tool results of a task's model.
fn next_call(answered: usize, text: &str) -> Option<(&'static str, serde_json::Value)> {
    let task = own_task(text);
    if let Some(query) = task
        .split_whitespace()
        .filter_map(|word| word.strip_prefix(SEARCH))
        .nth(answered)
    {
        return Some(("capability_search", json!({"query": query, "limit": 1})));
    }
    if answered == 0 {
        next_arguments(text).map(|arguments| ("sessions_spawn", arguments))
    } else {
        None
    }
}

fn next_arguments(text: &str) -> Option<serde_json::Value> {
    let start = text.find(NEXT)?.checked_add(NEXT.len())?;
    let end = text.rfind(END)?;
    serde_json::from_str(text.get(start..end)?).ok()
}

fn own_task(text: &str) -> String {
    text.split(NEXT).next().unwrap_or_default().to_owned()
}

/// A task whose model spawns `next` (spawn arguments) before answering.
fn chain(label: &str, next: Option<serde_json::Value>) -> String {
    next.map_or_else(|| label.to_owned(), |next| format!("{label} {NEXT}{next}{END}"))
}

fn spawn_args(task: String) -> serde_json::Value {
    json!({"task": task, "mode": "task"})
}

/// The in-process scripted model: the root parent's model and the first
/// child's model (through the ordinary provider fixture). It calls tools
/// natively, so each request carries the turn's published tool specs.
struct ScriptedProvider(Script);

#[async_trait]
impl crate::providers::Provider for ScriptedProvider {
    fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
        crate::providers::traits::ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }

    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        Ok(DONE.to_owned())
    }

    async fn chat(
        &self,
        request: crate::providers::ChatRequest<'_>,
        _: &str,
        _: f64,
    ) -> anyhow::Result<crate::providers::ChatResponse> {
        let first_user = request
            .messages
            .iter()
            .find(|message| message.role == "user")
            .map_or("", |message| message.content.as_str());
        let results = request
            .messages
            .iter()
            .filter(|message| {
                message.role == "tool"
                    || (message.role == "user"
                        && message
                            .content
                            .starts_with(crate::agent::turn_contracts::TOOL_RESULTS_PREFIX))
            })
            .map(|message| message.content.clone())
            .collect();
        let tools = request
            .tools
            .map(|specs| specs.iter().map(|spec| spec.name.clone()).collect())
            .unwrap_or_default();
        let next = self.0.answer(
            Seen {
                own_task: own_task(first_user),
                tools,
                results,
            },
            first_user,
        );
        let (text, tool_calls) = next.map_or_else(
            || (Some(DONE.to_owned()), Vec::new()),
            |(name, arguments)| {
                (
                    None,
                    vec![crate::providers::ToolCall {
                        id: format!("prx-recursive-{name}"),
                        name: name.to_owned(),
                        arguments: arguments.to_string(),
                    }],
                )
            },
        );
        Ok(crate::providers::ChatResponse {
            text,
            tool_calls,
            reasoning_content: None,
        })
    }
}

/// The same script as a loopback Ollama `/api/chat` endpoint, with native
/// tool calls.
fn ollama_reply(script: &Script, request: &serde_json::Value) -> serde_json::Value {
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let first_user = messages
        .iter()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default();
    let results = messages
        .iter()
        .filter(|message| {
            message["role"] == "tool"
                || (message["role"] == "user"
                    && message["content"]
                        .as_str()
                        .is_some_and(|content| content.starts_with(crate::agent::turn_contracts::TOOL_RESULTS_PREFIX)))
        })
        .filter_map(|message| message["content"].as_str().map(str::to_owned))
        .collect();
    let tools = request["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let next = script.answer(
        Seen {
            own_task: own_task(first_user),
            tools,
            results,
        },
        first_user,
    );
    let message = next.map_or_else(
        || json!({"role": "assistant", "content": DONE}),
        |(name, arguments)| {
            json!({"role": "assistant", "content": "",
                "tool_calls": [{"function": {"name": name, "arguments": arguments}}]})
        },
    );
    json!({"model": request["model"], "done": true, "message": message,
        "prompt_eval_count": 1, "eval_count": 1})
}

struct Fixture {
    tool: SessionsSpawnTool,
    script: Script,
    registry: Arc<Vec<Box<dyn Tool>>>,
    _server: TaskTraceServer,
    /// The real store every level records into.
    memory: Arc<dyn Memory>,
    _workspace: tempfile::TempDir,
}

/// An effectful tool the fixture's model may discover but never calls.
struct ProbeWriteTool;

#[async_trait]
impl Tool for ProbeWriteTool {
    fn name(&self) -> &str {
        WRITE_TOOL
    }

    fn description(&self) -> &str {
        "Record a release note"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
        anyhow::bail!("the probe write tool is discovered, never dispatched")
    }
}

struct Options {
    level: AutonomyLevel,
    daily_cap_cents: Option<u32>,
}

/// An operator resolver that grants every request and counts consultations.
#[derive(Default)]
struct GrantingResolver {
    asked: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl crate::agent::turn_contracts::ApprovalResolver for GrantingResolver {
    async fn resolve(
        &self,
        request: &crate::approval::ApprovalRequest,
        _channel: &str,
    ) -> crate::agent::turn_contracts::ApprovalDecision {
        if request.tool_name == "sessions_spawn" {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        crate::agent::turn_contracts::ApprovalDecision::Grant
    }
}

/// The production run approval strategy a root turn's adapter runs under:
/// an approval manager and `resolver` (CR-CI-3: a Supervised root spawn is
/// approved through it, never through a grant in the arguments).
fn run_approval(tool: &SessionsSpawnTool, resolver: Arc<GrantingResolver>) -> Arc<dyn crate::tools::ApprovalStrategy> {
    crate::agent::execution_policy::run_approval_strategy(
        Some(Arc::new(crate::approval::ApprovalManager::new())),
        Some(resolver as Arc<dyn crate::agent::turn_contracts::ApprovalResolver>),
        "wacli",
        Arc::clone(&tool.security),
        None,
    )
}

async fn fixture(options: Options) -> Fixture {
    fixture_with(options, None).await
}

/// A fixture whose child registry also serves [`ProbeWriteTool`].
async fn fixture_with_write_tool(options: Options) -> Fixture {
    build_fixture(options, None, vec![Box::new(ProbeWriteTool)]).await
}

/// `factory` is the run approval resolver factory each task child's run
/// mints its resolver from (production: chat `/bg`).
async fn fixture_with(
    options: Options,
    factory: Option<crate::agent::turn_request::SpawnApprovalResolverFactory>,
) -> Fixture {
    build_fixture(options, factory, Vec::new()).await
}

/// `extra` tools join the spawn tool in the children's task registry.
async fn build_fixture(
    options: Options,
    factory: Option<crate::agent::turn_request::SpawnApprovalResolverFactory>,
    extra: Vec<Box<dyn Tool>>,
) -> Fixture {
    let workspace = tempfile::tempdir().expect("test: recursive fixture workspace");
    let script = Script::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: bind loopback model");
    let endpoint = format!("http://{}", listener.local_addr().expect("test: loopback address"));
    let served = script.clone();
    let app = axum::Router::new().route(
        "/api/chat",
        axum::routing::post(move |axum::Json(request): axum::Json<serde_json::Value>| {
            let served = served.clone();
            async move { axum::Json(ollama_reply(&served, &request)) }
        }),
    );
    let server = TaskTraceServer(tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::warn!("recursive fixture model stopped: {error}");
        }
    }));
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.config_path = workspace.path().join("config.toml");
    config.default_provider = Some("ollama".to_owned());
    config.default_model = Some("prx-local-model".to_owned());
    config.api_url = Some(endpoint);
    config.reliability.provider_retries = 0;
    config.autonomy.level = options.level;
    if let Some(cap) = options.daily_cap_cents {
        config.autonomy.max_cost_per_day_cents = cap;
    }
    for model in ["prx-local-model", FREE_MODEL] {
        config.providers.models.push(
            serde_json::from_value(json!({"provider": "ollama", "model": model,
                "operations": ["chat"], "native_tools": true}))
            .expect("test: native model metadata"),
        );
    }
    config.cost.prices.insert(
        "ollama/prx-local-model".to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).expect("test: fixture memory"));
    let (channel, _) = RecordingChannel::new();
    let mut tool = make_tool_with_security_and_spawn_config(
        Arc::new(channel),
        Arc::new(ScriptedProvider(script.clone())),
        security,
        config.sessions_spawn.clone(),
    )
    .with_shared_memory(Arc::clone(&memory));
    if let Some(factory) = factory {
        tool = tool.with_approval_resolver_factory(factory);
    }
    tool.workspace_dir = workspace.path().to_owned();
    tool.hint_config = Some(Arc::new(config));
    // A child's task registry is the parent's registry: the same tool
    // handles, so every level registers its runs in one roster.
    let registry = crate::agent::test_admission::registry_with_search(
        std::iter::once(crate::tools::ArcDelegatingTool::boxed(Arc::new(admission_clone(&tool))))
            .chain(extra)
            .collect(),
    );
    assert!(tool.tools_handle().set(&registry), "test: registry set once");
    Fixture {
        tool,
        script,
        registry,
        _server: server,
        memory,
        _workspace: workspace,
    }
}

#[derive(Debug)]
struct Run {
    id: String,
    parent: Option<String>,
    task: String,
    status: SubAgentStatus,
}

impl Run {
    fn output(&self) -> Option<&str> {
        match &self.status {
            SubAgentStatus::Completed(output) => Some(output),
            _ => None,
        }
    }
}

/// Wait until at least `expected` runs exist and none is still running.
async fn settled(tool: &SessionsSpawnTool, expected: usize) -> Vec<Run> {
    let runs = tool.active_runs_arc();
    let deadline = tokio::time::Instant::now() + RUN_DEADLINE;
    loop {
        {
            let runs = runs.read().await;
            if runs.len() >= expected
                && runs.iter().all(|run| {
                    !matches!(
                        run.status,
                        SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. }
                    )
                })
            {
                return runs
                    .iter()
                    .map(|run| Run {
                        id: run.id.clone(),
                        parent: run.parent_run_id.clone(),
                        task: run.task.clone(),
                        status: run.status.clone(),
                    })
                    .collect();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "runs did not settle: {:?}",
            runs.read().await.iter().map(|run| &run.status).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn run_for<'a>(runs: &'a [Run], label: &str) -> &'a Run {
    runs.iter()
        .find(|run| own_task(&run.task).contains(label))
        .unwrap_or_else(|| panic!("no run for {label}: {runs:?}"))
}

/// H1 task path: the default spawn runs its child under the derived
/// publication, whose model request lists `sessions_spawn`; a sibling with an
/// explicit opt-out keeps the captured scope and does not.
#[tokio::test]
async fn task_child_runs_under_the_derived_publication() {
    let fixture = fixture(Options {
        level: AutonomyLevel::Full,
        daily_cap_cents: None,
    })
    .await;
    let derived = execute_admitted_spawn(&fixture.tool, spawn_args("PRX-DERIVED.".to_owned()))
        .await
        .expect("test: default spawn");
    assert!(derived.success, "{:?}", derived.error);
    let mut captured = spawn_args("PRX-CAPTURED.".to_owned());
    captured["allow_delegation"] = json!(false);
    let captured = execute_admitted_spawn(&fixture.tool, captured)
        .await
        .expect("test: opt-out spawn");
    assert!(captured.success, "{:?}", captured.error);
    let runs = settled(&fixture.tool, 2).await;
    assert_eq!(run_for(&runs, "PRX-DERIVED.").output(), Some(DONE));
    assert_eq!(run_for(&runs, "PRX-CAPTURED.").output(), Some(DONE));
    let derived = fixture.script.for_task("PRX-DERIVED.");
    let captured = fixture.script.for_task("PRX-CAPTURED.");
    assert!(
        derived
            .first()
            .is_some_and(|seen| seen.tools.iter().any(|tool| tool == "sessions_spawn")),
        "the derived child lists sessions_spawn: {derived:?}"
    );
    assert!(
        captured
            .first()
            .is_some_and(|seen| !seen.tools.iter().any(|tool| tool == "sessions_spawn")),
        "the opted-out sibling does not list sessions_spawn: {captured:?}"
    );
}

/// Full: neither the root nor its child needs a grant to spawn.
#[tokio::test]
async fn full_root_and_child_spawn_without_grant() {
    let fixture = fixture(Options {
        level: AutonomyLevel::Full,
        daily_cap_cents: None,
    })
    .await;
    let task = chain("PRX-CHILD.", Some(spawn_args("PRX-GRANDCHILD.".to_owned())));
    let result = execute_admitted_spawn(&fixture.tool, spawn_args(task))
        .await
        .expect("test: ungranted Full root spawn");
    assert!(result.success, "{:?}", result.error);
    let runs = settled(&fixture.tool, 2).await;
    let child = run_for(&runs, "PRX-CHILD.");
    let grandchild = run_for(&runs, "PRX-GRANDCHILD.");
    assert_eq!(child.output(), Some(DONE));
    assert_eq!(grandchild.output(), Some(DONE));
    assert_eq!(grandchild.parent.as_deref(), Some(child.id.as_str()));
}

/// Supervised, root and recursive child alike, in two layers. (1) Without an
/// approval resolver both dispatches are refused at dispatch approval with
/// the same literal, with no run and no model request for the refused spawn.
/// (2) Through the production run approval strategy with an approving
/// resolver both dispatches are approved, the runtime grant each approval
/// issues passes the spawn's side-effect gate, and the child runs.
#[tokio::test]
async fn supervised_child_follows_root_dispatch_approval() {
    // (1) Root: the execution service's default strategy refuses.
    let refused = fixture(Options {
        level: AutonomyLevel::Supervised,
        daily_cap_cents: None,
    })
    .await;
    let error = execute_admitted_spawn(&refused.tool, spawn_args("PRX-REFUSED-ROOT.".to_owned()))
        .await
        .expect_err("an unapproved root spawn must not execute");
    assert_eq!(error.to_string(), NO_RESOLVER);
    assert!(refused.tool.active_runs_snapshot().await.is_empty());
    assert!(refused.script.for_task("PRX-REFUSED-ROOT.").is_empty());
    // (1) Child: a root spawn approved by the run approval resolver starts a
    // child whose run has no resolver (no factory); its own spawn is refused
    // with the same literal and starts nothing.
    let task = chain(
        "PRX-SUPERVISED-CHILD.",
        Some(spawn_args("PRX-REFUSED-LEAF.".to_owned())),
    );
    let root_approval = run_approval(&refused.tool, Arc::new(GrantingResolver::default()));
    let started = execute_admitted_spawn_with_approval(&refused.tool, spawn_args(task), root_approval)
        .await
        .expect("test: approved root spawn");
    assert!(started.success, "{:?}", started.error);
    let runs = settled(&refused.tool, 1).await;
    assert_eq!(runs.len(), 1, "the refused child spawn registered no run: {runs:?}");
    let child = refused.script.for_task("PRX-SUPERVISED-CHILD.");
    assert!(
        child
            .iter()
            .any(|seen| seen.results.iter().any(|result| result.contains(NO_RESOLVER))),
        "the child's spawn is refused at dispatch approval: {child:?}"
    );
    assert!(refused.script.for_task("PRX-REFUSED-LEAF.").is_empty());

    // (2) The root runs under the run approval strategy (an approval manager
    // and the approving resolver); each task child's run mints the same
    // resolver from the run factory. Neither dispatch carries a grant.
    let resolver = Arc::new(GrantingResolver::default());
    let minted: Arc<dyn crate::agent::turn_contracts::ApprovalResolver> = resolver.clone();
    let approving = fixture_with(
        Options {
            level: AutonomyLevel::Supervised,
            daily_cap_cents: None,
        },
        Some(crate::agent::turn_request::SpawnApprovalResolverFactory::new(
            move |_| Arc::clone(&minted),
        )),
    )
    .await;
    let root_approval = run_approval(&approving.tool, Arc::clone(&resolver));
    let task = chain("PRX-APPROVED-CHILD.", Some(spawn_args("PRX-APPROVED-LEAF.".to_owned())));
    let started = execute_admitted_spawn_with_approval(&approving.tool, spawn_args(task), root_approval)
        .await
        .expect("test: approved root spawn");
    assert!(started.success, "{:?}", started.error);
    let runs = settled(&approving.tool, 2).await;
    let child = run_for(&runs, "PRX-APPROVED-CHILD.");
    let leaf = run_for(&runs, "PRX-APPROVED-LEAF.");
    assert_eq!(child.output(), Some(DONE));
    assert_eq!(leaf.output(), Some(DONE));
    assert_eq!(leaf.parent.as_deref(), Some(child.id.as_str()));
    assert!(
        approving
            .script
            .for_task("PRX-APPROVED-CHILD.")
            .iter()
            .all(|seen| seen.results.iter().all(|result| !result.contains(NO_RESOLVER))),
        "the child's spawn is not refused at dispatch approval"
    );
    assert_eq!(
        resolver.asked.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the resolver decided the root spawn and the child spawn"
    );
}

/// ReadOnly: the opt-in changes nothing; the root spawn is refused exactly
/// as without it and no child exists.
#[tokio::test]
async fn read_only_root_spawn_is_refused_as_without_recursion() {
    let fixture = fixture(Options {
        level: AutonomyLevel::ReadOnly,
        daily_cap_cents: None,
    })
    .await;
    let mut outcomes = Vec::new();
    for flag in [true, false] {
        let mut args = with_spawn_grant(spawn_args("PRX-READ-ONLY.".to_owned()));
        args["allow_delegation"] = json!(flag);
        let outcome = execute_admitted_spawn(&fixture.tool, args)
            .await
            .map(|result| (result.success, result.error));
        assert!(
            outcome.as_ref().map_or(true, |(success, _)| !success),
            "ReadOnly must refuse the spawn: {outcome:?}"
        );
        outcomes.push(outcome.map_err(|error| error.to_string()));
    }
    assert_eq!(outcomes.first(), outcomes.get(1), "the opt-in changes the refusal");
    assert!(fixture.tool.active_runs_snapshot().await.is_empty());
    assert!(fixture.script.for_task("PRX-READ-ONLY.").is_empty());
}

/// A root whose catalog has `sessions_spawn` and other tools but no
/// `delegate` derives a recursive child.
#[tokio::test]
async fn root_without_delegate_derives_a_recursive_child() {
    let fixture = fixture(Options {
        level: AutonomyLevel::Supervised,
        daily_cap_cents: None,
    })
    .await;
    assert!(
        fixture.registry.iter().all(|tool| tool.name() != "delegate") && fixture.registry.len() > 1,
        "test: the catalog has other tools and no delegate"
    );
    let root_approval = run_approval(&fixture.tool, Arc::new(GrantingResolver::default()));
    let result =
        execute_admitted_spawn_with_approval(&fixture.tool, spawn_args("PRX-NO-DELEGATE.".to_owned()), root_approval)
            .await
            .expect("test: approved root spawn");
    assert!(result.success, "{:?}", result.error);
    let runs = settled(&fixture.tool, 1).await;
    assert_eq!(run_for(&runs, "PRX-NO-DELEGATE.").output(), Some(DONE));
    let seen = fixture.script.for_task("PRX-NO-DELEGATE.");
    assert!(
        seen.first()
            .is_some_and(|seen| seen.tools.iter().any(|tool| tool == "sessions_spawn")
                && !seen.tools.iter().any(|tool| tool == "delegate")),
        "the child keeps sessions_spawn without a delegate tool: {seen:?}"
    );
}

/// An exhausted allowance carves a zero-funded child: its paid send reaches
/// no model and fails with the accounting refusal, while a zero-price model
/// completes.
#[tokio::test]
async fn exhausted_allowance_spawns_a_zero_funded_child() {
    // A one-cent daily cap seals one cent for children; half of it rounds
    // to a zero-funded share.
    let fixture = fixture(Options {
        level: AutonomyLevel::Full,
        daily_cap_cents: Some(1),
    })
    .await;
    let paid = execute_admitted_spawn(&fixture.tool, spawn_args("PRX-PAID.".to_owned()))
        .await
        .expect("test: zero-funded paid spawn");
    assert!(paid.success, "{:?}", paid.error);
    let mut free = spawn_args("PRX-FREE.".to_owned());
    free["model"] = json!(FREE_MODEL);
    let free = execute_admitted_spawn(&fixture.tool, free)
        .await
        .expect("test: zero-funded free spawn");
    assert!(free.success, "{:?}", free.error);
    let runs = settled(&fixture.tool, 2).await;
    let paid = run_for(&runs, "PRX-PAID.");
    assert!(
        matches!(&paid.status, SubAgentStatus::Failed(reason) if reason.contains("provider send refused")),
        "the paid send is refused by its accounting owner: {:?}",
        paid.status
    );
    assert!(
        fixture.script.for_task("PRX-PAID.").is_empty(),
        "no paid request was sent"
    );
    assert_eq!(run_for(&runs, "PRX-FREE.").output(), Some(DONE));
    assert!(!fixture.script.for_task("PRX-FREE.").is_empty());
}

/// Parity default (controller rulings, G5b c2a and 2b): an omitted flag
/// derives a child that can spawn. Nesting ends at the sealed bound: each
/// level keeps a strict subset of its parent and the root's sealed depth is
/// its set size, so the last level's recursive derivation is refused by the
/// strict-subset rule. There an omitted flag falls back to a non-delegating
/// leaf, as `false` does, while an explicit `true` keeps the typed refusal.
#[tokio::test]
async fn omitted_flag_child_spawns_until_the_depth_bound() {
    let fixture = fixture(Options {
        level: AutonomyLevel::Full,
        daily_cap_cents: None,
    })
    .await;
    let task = chain("PRX-LEVEL-1.", Some(spawn_args("PRX-LEVEL-2.".to_owned())));
    let result = execute_admitted_spawn(&fixture.tool, json!({"task": task}))
        .await
        .expect("test: default root spawn");
    assert!(result.success, "{:?}", result.error);
    let mut refused = spawn_args("PRX-REFUSED-LEAF.".to_owned());
    refused["allow_delegation"] = json!(true);
    let task = chain("PRX-EXPLICIT.", Some(refused));
    let result = execute_admitted_spawn(&fixture.tool, spawn_args(task))
        .await
        .expect("test: default root spawn with an explicitly recursive child");
    assert!(result.success, "{:?}", result.error);
    let runs = settled(&fixture.tool, 3).await;
    assert_eq!(
        runs.len(),
        3,
        "exactly level 1, its fallback leaf and the explicit child: {runs:?}"
    );

    let level_one = fixture.script.for_task("PRX-LEVEL-1.");
    assert!(
        level_one
            .first()
            .is_some_and(|seen| seen.tools.iter().any(|tool| tool == "sessions_spawn")),
        "the default child can spawn: {level_one:?}"
    );
    let level_one = run_for(&runs, "PRX-LEVEL-1.");
    let level_two = run_for(&runs, "PRX-LEVEL-2.");
    assert_eq!(level_one.output(), Some(DONE));
    assert_eq!(level_two.output(), Some(DONE));
    assert_eq!(level_two.parent.as_deref(), Some(level_one.id.as_str()));
    assert!(
        fixture
            .script
            .for_task("PRX-LEVEL-2.")
            .first()
            .is_some_and(|seen| !seen.tools.iter().any(|tool| tool == "sessions_spawn")),
        "the fallback leaf at the bound cannot spawn"
    );

    let explicit = fixture.script.for_task("PRX-EXPLICIT.");
    assert!(
        explicit
            .iter()
            .any(|seen| seen.results.iter().any(|result| result.contains(NOT_STRICT))),
        "an explicit true at the bound is refused: {explicit:?}"
    );
    assert_eq!(run_for(&runs, "PRX-EXPLICIT.").output(), Some(DONE));
    assert!(fixture.script.for_task("PRX-REFUSED-LEAF.").is_empty());
}

/// Page size and page bound of a store read-back through the repository.
const STORE_PAGE: usize = 256;
const MAX_STORE_PAGES: usize = 16;

/// A reader of the fixture workspace scoped to `session_key`: it sees every
/// workspace-visible row plus that session's own rows.
fn store_principal(fixture: &Fixture, session_key: Option<&str>) -> MemoryPrincipal {
    MemoryPrincipal {
        workspace_id: fixture.tool.workspace_dir.to_string_lossy().into_owned(),
        session_key: session_key.map(str::to_owned),
        ..MemoryPrincipal::default()
    }
}

/// Every message event `principal` may read, oldest first (bounded).
async fn stored_rows(fixture: &Fixture, principal: &MemoryPrincipal) -> Vec<crate::memory::MessageEvent> {
    let mut rows = Vec::new();
    let mut after = 0;
    for page_index in 0..MAX_STORE_PAGES {
        let page = fixture
            .memory
            .list_message_events_since(principal, after, STORE_PAGE)
            .await
            .expect("test: read message events");
        let Some(last) = page.last() else {
            return rows;
        };
        assert!(last.id > after, "test: the message-event cursor advances");
        after = last.id;
        rows.extend(page);
        assert!(
            page_index + 1 < MAX_STORE_PAGES,
            "test: the store exceeded the read bound"
        );
    }
    rows
}

/// `(run id, child_delegation)` of every stored `task.spawned` event.
async fn task_spawned_delegations(fixture: &Fixture) -> Vec<(String, Option<String>)> {
    let principal = store_principal(fixture, None);
    let mut recorded = Vec::new();
    let mut after = 0;
    for page_index in 0..MAX_STORE_PAGES {
        let page = fixture
            .memory
            .list_memory_events_since(&principal, after, STORE_PAGE)
            .await
            .expect("test: read memory events");
        let Some(last) = page.last() else {
            return recorded;
        };
        assert!(last.id > after, "test: the memory-event cursor advances");
        after = last.id;
        recorded.extend(
            page.into_iter()
                .filter(|event| event.event_type == "task.spawned")
                .map(|event| {
                    let code = event
                        .payload_json
                        .and_then(|payload| serde_json::from_str::<serde_json::Value>(&payload).ok())
                        .and_then(|payload| payload.get("child_delegation")?.as_str().map(str::to_owned));
                    (event.subject_id, code)
                }),
        );
        assert!(
            page_index + 1 < MAX_STORE_PAGES,
            "test: the store exceeded the read bound"
        );
    }
    recorded
}

/// The `capability_search` hit for `name` among a model's tool results.
fn search_hit(results: &[String], name: &str) -> Option<serde_json::Value> {
    results
        .iter()
        .filter_map(|result| serde_json::from_str::<serde_json::Value>(result).ok())
        .filter_map(|result| result.get("hits").and_then(serde_json::Value::as_array).cloned())
        .flatten()
        .find(|hit| hit.pointer("/schema/name").and_then(serde_json::Value::as_str) == Some(name))
}

/// I1: a Full child that inherited `sessions_spawn` keeps it on its surface
/// even though its input names a tool outside its sealed manifest
/// (`capability_search`); the named tool is dropped from the request, never
/// the delegation.
#[tokio::test]
async fn full_child_naming_an_unsealed_tool_still_spawns_a_grandchild() {
    let fixture = fixture(Options {
        level: AutonomyLevel::Full,
        daily_cap_cents: None,
    })
    .await;
    let task = chain(
        "PRX-NAMING-CHILD. Use sessions_spawn and capability_search.",
        Some(spawn_args("PRX-NAMED-GRANDCHILD.".to_owned())),
    );
    let result = execute_admitted_spawn(&fixture.tool, spawn_args(task))
        .await
        .expect("test: Full root spawn");
    assert!(result.success, "{:?}", result.error);
    let runs = settled(&fixture.tool, 2).await;
    let seen = fixture.script.for_task("PRX-NAMING-CHILD.");
    assert!(
        seen.first()
            .is_some_and(|seen| seen.tools.iter().any(|tool| tool == "sessions_spawn")),
        "the naming child lists sessions_spawn: {seen:?}"
    );
    let child = run_for(&runs, "PRX-NAMING-CHILD.");
    let grandchild = run_for(&runs, "PRX-NAMED-GRANDCHILD.");
    assert_eq!(grandchild.parent.as_deref(), Some(child.id.as_str()));
    assert_eq!(child.output(), Some(DONE));
    assert_eq!(grandchild.output(), Some(DONE));
}

/// I3: the spawn result names whether the child can spawn and, when it
/// cannot, why; the `task.spawned` record carries the same classification.
#[tokio::test]
async fn spawn_result_and_task_event_state_child_delegation() {
    let fixture = fixture(Options {
        level: AutonomyLevel::Full,
        daily_cap_cents: None,
    })
    .await;
    let recursive = execute_admitted_spawn(&fixture.tool, spawn_args("PRX-CAN-SPAWN.".to_owned()))
        .await
        .expect("test: default spawn");
    assert!(recursive.success, "{:?}", recursive.error);
    assert!(
        recursive.output.contains("). Child can spawn: yes. "),
        "{}",
        recursive.output
    );
    let mut opted_out = spawn_args("PRX-OPTED-OUT.".to_owned());
    opted_out["allow_delegation"] = json!(false);
    let opted_out = execute_admitted_spawn(&fixture.tool, opted_out)
        .await
        .expect("test: opt-out spawn");
    assert!(opted_out.success, "{:?}", opted_out.error);
    assert!(
        opted_out
            .output
            .contains("). Child can spawn: no (allow_delegation=false). "),
        "{}",
        opted_out.output
    );
    let task = chain("PRX-BOUND-1.", Some(spawn_args("PRX-BOUND-2.".to_owned())));
    let chained = execute_admitted_spawn(&fixture.tool, json!({"task": task}))
        .await
        .expect("test: chained default spawn");
    assert!(chained.success, "{:?}", chained.error);
    assert!(
        chained.output.contains("). Child can spawn: yes. "),
        "{}",
        chained.output
    );
    let runs = settled(&fixture.tool, 4).await;

    let level_one = fixture.script.for_task("PRX-BOUND-1.");
    assert!(
        level_one.iter().any(|seen| seen
            .results
            .iter()
            .any(|result| result.contains("Child can spawn: no (not_strict_subset)."))),
        "level 1 sees its leaf child's reason: {level_one:?}"
    );

    let recorded = task_spawned_delegations(&fixture).await;
    for (label, code) in [
        ("PRX-CAN-SPAWN.", "recursive"),
        ("PRX-OPTED-OUT.", "opted_out"),
        ("PRX-BOUND-1.", "recursive"),
        ("PRX-BOUND-2.", "leaf:not_strict_subset"),
    ] {
        let run = run_for(&runs, label);
        assert_eq!(run.output(), Some(DONE), "{label}");
        assert!(
            recorded
                .iter()
                .any(|(id, recorded)| *id == run.id && recorded.as_deref() == Some(code)),
            "{label}: task.spawned carries child_delegation={code}: {recorded:?}"
        );
    }
}

/// I2 in a real Full child turn: a child without an outgoing manifest sees
/// `sessions_spawn` as unavailable with the reason, and an effectful tool its
/// turn admits as callable without approval.
#[tokio::test]
async fn full_child_capability_search_hits_report_the_turns_own_admission() {
    let fixture = fixture_with_write_tool(Options {
        level: AutonomyLevel::Full,
        daily_cap_cents: None,
    })
    .await;
    let mut args = spawn_args(format!(
        "PRX-SEARCHING-CHILD. Find sessions_spawn and {WRITE_TOOL}. {SEARCH}sessions_spawn {SEARCH}{WRITE_TOOL}"
    ));
    args["allow_delegation"] = json!(false);
    let result = execute_admitted_spawn_with_tools(&fixture.tool, args, vec![Box::new(ProbeWriteTool)])
        .await
        .expect("test: opt-out spawn");
    assert!(result.success, "{:?}", result.error);
    let runs = settled(&fixture.tool, 1).await;
    assert_eq!(run_for(&runs, "PRX-SEARCHING-CHILD.").output(), Some(DONE));
    let seen = fixture.script.for_task("PRX-SEARCHING-CHILD.");
    assert!(
        seen.first()
            .is_some_and(|seen| !seen.tools.iter().any(|tool| tool == "sessions_spawn")),
        "the opted-out child publishes no sessions_spawn: {seen:?}"
    );
    let results: Vec<String> = seen.iter().flat_map(|seen| seen.results.iter().cloned()).collect();
    let spawn = search_hit(&results, "sessions_spawn").unwrap_or_else(|| panic!("no sessions_spawn hit: {seen:?}"));
    assert_eq!(
        spawn.get("approval").and_then(serde_json::Value::as_str),
        Some("unavailable")
    );
    assert_eq!(
        spawn.get("unavailable_reason").and_then(serde_json::Value::as_str),
        Some("spawn_not_granted"),
        "{spawn}"
    );
    let write = search_hit(&results, WRITE_TOOL).unwrap_or_else(|| panic!("no {WRITE_TOOL} hit: {seen:?}"));
    assert_eq!(
        write.get("effect").and_then(serde_json::Value::as_str),
        Some("local_write")
    );
    assert_eq!(
        write.get("approval").and_then(serde_json::Value::as_str),
        Some("none"),
        "{write}"
    );
    assert!(write.get("unavailable_reason").is_none(), "{write}");
}

/// I4: every spawned run records under `spawn:<run_id>` and links to its
/// caller through `parent_session_key`; nothing lands under the turn root's
/// chat key.
#[tokio::test]
async fn spawned_runs_record_under_their_own_session_keys() {
    const ROOT_SCOPE: &str = "chat:prx-session";
    let fixture = fixture(Options {
        level: AutonomyLevel::Full,
        daily_cap_cents: None,
    })
    .await;
    let task = chain("PRX-KEYED-CHILD.", Some(spawn_args("PRX-KEYED-GRANDCHILD.".to_owned())));
    let root = SpawnExecutionContext::seed_turn_context("prx-root-turn".to_owned(), ROOT_SCOPE.to_owned());
    let result = SPAWN_EXECUTION_CONTEXT
        .scope(root, execute_admitted_spawn(&fixture.tool, spawn_args(task)))
        .await
        .expect("test: Full root spawn under a chat turn root");
    assert!(result.success, "{:?}", result.error);
    let runs = settled(&fixture.tool, 2).await;
    let child = run_for(&runs, "PRX-KEYED-CHILD.");
    let grandchild = run_for(&runs, "PRX-KEYED-GRANDCHILD.");
    assert_eq!(grandchild.parent.as_deref(), Some(child.id.as_str()));

    let child_key = format!("spawn:{}", child.id);
    let grandchild_key = format!("spawn:{}", grandchild.id);
    let child_rows = stored_rows(&fixture, &store_principal(&fixture, Some(&child_key))).await;
    // The caller link the child's input carries is the root caller's own
    // conversation key: the caller's message is recorded under it.
    let root_key = child_rows
        .iter()
        .find(|row| {
            row.source == "sessions_spawn" && row.role == "user" && row.run_id.as_deref() == Some(child.id.as_str())
        })
        .and_then(|row| row.parent_session_key.clone())
        .expect("test: the child's input names its caller's session");
    assert!(root_key != ROOT_SCOPE && !root_key.starts_with("spawn:"), "{root_key}");
    let root_rows = stored_rows(&fixture, &store_principal(&fixture, Some(&root_key))).await;
    assert!(
        root_rows.iter().any(|row| row.source == "channel"
            && row.role == "user"
            && row.session_key.as_deref() == Some(root_key.as_str())),
        "the root caller's message is recorded under {root_key}"
    );
    assert!(
        root_rows
            .iter()
            .all(|row| !(row.source == "sessions_spawn" && row.session_key.as_deref() == Some(root_key.as_str()))),
        "no spawned-run row lands under the caller's key"
    );
    for (run, key, parent_key) in [
        (child, child_key.as_str(), root_key.as_str()),
        (grandchild, grandchild_key.as_str(), child_key.as_str()),
    ] {
        let rows = stored_rows(&fixture, &store_principal(&fixture, Some(key))).await;
        let own: Vec<_> = rows
            .iter()
            .filter(|row| row.source == "sessions_spawn" && row.run_id.as_deref() == Some(run.id.as_str()))
            .collect();
        assert!(!own.is_empty(), "{key}: the run recorded rows");
        assert!(
            own.iter().all(|row| row.session_key.as_deref() == Some(key)),
            "{key}: every row of the run uses its own key: {own:?}"
        );
        let keyed: Vec<_> = rows
            .iter()
            .filter(|row| row.source == "sessions_spawn" && row.session_key.as_deref() == Some(key))
            .collect();
        assert!(
            keyed
                .iter()
                .all(|row| row.parent_session_key.as_deref() == Some(parent_key)),
            "{key}: every row links to its caller {parent_key}: {keyed:?}"
        );
    }
    let child_tools: Vec<_> = child_rows
        .iter()
        .filter(|row| {
            row.event_type.starts_with("tool.execution.") && row.session_key.as_deref() == Some(child_key.as_str())
        })
        .collect();
    assert!(!child_tools.is_empty(), "the child's spawn call recorded tool rows");
    assert!(
        child_tools
            .iter()
            .all(|row| row.parent_session_key.as_deref() == Some(root_key.as_str())),
        "the child's tool rows name its parent: {child_tools:?}"
    );
    let scope_rows = stored_rows(&fixture, &store_principal(&fixture, Some(ROOT_SCOPE))).await;
    assert!(
        scope_rows
            .iter()
            .all(|row| !(row.source == "sessions_spawn" && row.session_key.as_deref() == Some(ROOT_SCOPE))),
        "no spawned-run row lands under the turn root's chat key"
    );
}

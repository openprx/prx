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
    /// Record the request and return the spawn arguments to call, if any: a
    /// model with no tool result yet makes the spawn its task names.
    fn answer(&self, seen: Seen, first_user: &str) -> Option<serde_json::Value> {
        let next = if seen.results.is_empty() {
            next_arguments(first_user)
        } else {
            None
        };
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
/// child's model (through the ordinary provider fixture).
struct ScriptedProvider(Script);

#[async_trait]
impl crate::providers::Provider for ScriptedProvider {
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
        let text = next.map_or_else(
            || DONE.to_owned(),
            |arguments| {
                format!(
                    "<tool_call>\n{}\n</tool_call>",
                    json!({"name": "sessions_spawn", "arguments": arguments})
                )
            },
        );
        Ok(crate::providers::ChatResponse {
            text: Some(text),
            tool_calls: Vec::new(),
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
        |arguments| {
            json!({"role": "assistant", "content": "",
                "tool_calls": [{"function": {"name": "sessions_spawn", "arguments": arguments}}]})
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
    _workspace: tempfile::TempDir,
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

async fn fixture(options: Options) -> Fixture {
    fixture_with(options, None).await
}

/// `factory` is the run approval resolver factory each task child's run
/// mints its resolver from (production: chat `/bg`).
async fn fixture_with(
    options: Options,
    factory: Option<crate::agent::turn_request::SpawnApprovalResolverFactory>,
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
    let registry = crate::agent::test_admission::registry_with_search(vec![crate::tools::ArcDelegatingTool::boxed(
        Arc::new(admission_clone(&tool)),
    )]);
    assert!(tool.tools_handle().set(&registry), "test: registry set once");
    Fixture {
        tool,
        script,
        registry,
        _server: server,
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
    // (1) Child: a granted root spawn starts a child with no resolver; its
    // own spawn is refused with the same literal and starts nothing.
    let task = chain(
        "PRX-SUPERVISED-CHILD.",
        Some(spawn_args("PRX-REFUSED-LEAF.".to_owned())),
    );
    let started = execute_admitted_spawn(&refused.tool, with_spawn_grant(spawn_args(task)))
        .await
        .expect("test: granted root spawn");
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
    let root_approval = crate::agent::execution_policy::run_approval_strategy(
        Some(Arc::new(crate::approval::ApprovalManager::new())),
        Some(Arc::clone(&resolver) as Arc<dyn crate::agent::turn_contracts::ApprovalResolver>),
        "wacli",
        Arc::clone(&approving.tool.security),
        None,
    );
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
    let result = execute_admitted_spawn(
        &fixture.tool,
        with_spawn_grant(spawn_args("PRX-NO-DELEGATE.".to_owned())),
    )
    .await
    .expect("test: granted root spawn");
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

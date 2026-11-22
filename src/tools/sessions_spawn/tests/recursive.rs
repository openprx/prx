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
/// Loopback Ollama model with no price row: zero-price exempt.
const FREE_MODEL: &str = "prx-free-model";
/// A word `PRX-SEARCH=<query>` in a task makes its model call
/// `capability_search` for `<query>`, one query per request, in order.
const SEARCH: &str = "PRX-SEARCH=";

/// One model request as the scripted model saw it.
#[derive(Clone, Debug)]
struct Seen {
    own_task: String,
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
        let results: Vec<String> = request
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
        let answered = results.len();
        let next = self.0.answer(
            Seen {
                own_task: own_task(first_user),
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
                        // One id per call: a reused id replays the first
                        // call's idempotency key.
                        id: format!("prx-recursive-{name}-{answered}"),
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
    let next = script.answer(
        Seen {
            own_task: own_task(first_user),
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
    _registry: Arc<Vec<Box<dyn Tool>>>,
    _server: TaskTraceServer,
    _workspace: tempfile::TempDir,
}

struct Options {
    level: AutonomyLevel,
    daily_cap_cents: Option<u32>,
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
        _registry: registry,
        _server: server,
        _workspace: workspace,
    }
}

#[derive(Debug)]
struct Run {
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

/// Full: neither the root nor its child needs a grant to spawn.
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

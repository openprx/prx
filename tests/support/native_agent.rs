//! Case-local physical native admission for original positive tool scenarios.
//! Predictions cross the real compiler, accounting, parser and gates; they are not grants.
use anyhow::{Context, Result};
use axum::response::IntoResponse;
use futures::FutureExt;
use openprx::agent::agent::{Agent, AgentBuilder};
use openprx::agent::dispatcher::NativeToolDispatcher;
use openprx::config::{Config, schema::ModelPricing};
use openprx::decision::protocol::{BackendKind, BackendLocality};
use openprx::decision::telemetry::{CausalEvent, CausalEventKind, CausalLineage, ServedLabel};
use openprx::memory::SqliteMemory;
use openprx::observability::{Observer, ObserverEvent, ObserverMetric};
use openprx::providers::{ChatRequest, ChatResponse, Provider};
use serde_json::{Value, json};
use std::io::Write;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const DECISION_MODEL: &str = "prx-integration-native";
const MAX_RECORDS: usize = 16;
const MAX_CAPTURE_BYTES: usize = 262_144;
struct ReplyBinding {
    snapshot: CausalLineage,
    prediction: CausalLineage,
    start_index: usize,
    request: Value,
    reply: Value,
}
#[derive(Clone, Copy, Default)]
enum DecisionMode {
    #[default]
    NativeTool,
    Delegate,
}
impl DecisionMode {
    const fn action_target(self) -> (&'static str, &'static str) {
        match self {
            Self::NativeTool => ("use_tool", "native_tool"),
            Self::Delegate => ("delegate", "session"),
        }
    }
}
#[derive(Default)]
struct Observations {
    mode: DecisionMode,
    requests: usize,
    rejected: usize,
    records: Vec<Value>,
    predictions: Vec<CausalEvent>,
    bindings: Vec<ReplyBinding>,
    overflow: bool,
}
struct InputObserver(Arc<parking_lot::Mutex<Observations>>);
impl Observer for InputObserver {
    fn record_event(&self, event: &ObserverEvent) {
        if let ObserverEvent::Causal(event) = event
            && matches!(
                event.kind,
                CausalEventKind::SnapshotCreated
                    | CausalEventKind::PredictionStarted
                    | CausalEventKind::PredictionSucceeded
                    | CausalEventKind::PredictionFailed
                    | CausalEventKind::BackendFallback
            )
        {
            let mut observed = self.0.lock();
            if observed.predictions.len() == MAX_RECORDS * 8 {
                observed.overflow = true;
            } else {
                observed.predictions.push((**event).clone());
            }
        }
    }
    fn record_metric(&self, _: &ObserverMetric) {}
    fn name(&self) -> &'static str {
        "prx_native_integration_fixture"
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
fn lineage_capture(lineage: &CausalLineage) -> Value {
    json!({"workspace_id":lineage.workspace_id.as_ref(),"owner_id":lineage.owner_id.as_deref(),
        "session_key":lineage.session_key.as_deref(),"run_id":lineage.run_id.as_deref(),"task_id":lineage.task_id.as_deref(),
        "source_event_id":lineage.source_event_id.as_deref(),"causation_event_id":lineage.causation_event_id.as_deref(),"decision_id":lineage.decision_id.as_deref()})
}
fn same_input(left: &CausalLineage, right: &CausalLineage) -> bool {
    left.workspace_id == right.workspace_id
        && left.owner_id == right.owner_id
        && left.session_key == right.session_key
        && left.run_id == right.run_id
        && left.source_event_id == right.source_event_id
}
fn configured_backend(event: &CausalEvent) -> bool {
    event.fields.backend == Some(BackendKind::CompatibleHttp)
        && event.fields.locality == Some(BackendLocality::Loopback)
        && event.fields.configured_model.as_deref() == Some(DECISION_MODEL)
}
struct NativeScriptedProvider {
    inner: Box<dyn Provider>,
    primary_tool: &'static str,
}
#[async_trait::async_trait]
impl Provider for NativeScriptedProvider {
    fn supports_native_tools(&self) -> bool {
        true
    }
    async fn chat_with_system(
        &self,
        system: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> Result<String> {
        self.inner.chat_with_system(system, message, model, temperature).await
    }
    async fn chat(&self, request: ChatRequest<'_>, model: &str, temperature: f64) -> Result<ChatResponse> {
        anyhow::ensure!(model == "prx-local-model", "pinned original chat model");
        anyhow::ensure!(
            request
                .tools
                .is_some_and(|tools| tools.iter().any(|tool| tool.name == self.primary_tool)),
            "provider receives actually admitted tool schema"
        );
        self.inner.chat(request, model, temperature).await
    }
}
struct Server {
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
        // Drop requests cancellation; only finish actually awaits the join.
    }
}
impl Server {
    async fn finish(&mut self) -> Result<()> {
        self.stop.cancel();
        let task = self.task.as_mut().context("listener handle missing")?;
        let joined = tokio::time::timeout(std::time::Duration::from_secs(5), &mut *task).await;
        if let Ok(result) = joined {
            self.task.take();
            result.context("listener task failed")?.context("listener failed")
        } else {
            task.abort();
            let _ = task.await;
            self.task.take();
            anyhow::bail!("listener graceful shutdown timed out")
        }
    }
}
async fn reply(
    axum::extract::State(state): axum::extract::State<Arc<parking_lot::Mutex<Observations>>>,
    body: std::result::Result<axum::Json<Value>, axum::extract::rejection::JsonRejection>,
) -> axum::response::Response {
    let mut observed = state.lock();
    let result = (|| -> Result<Value> {
        anyhow::ensure!(observed.requests < MAX_RECORDS, "physical request bound");
        let axum::Json(body) = body.map_err(|_| anyhow::anyhow!("invalid decision JSON body"))?;
        anyhow::ensure!(!observed.overflow, "observer evidence overflow");
        let (snapshot_index, snapshot) = observed
            .predictions
            .iter()
            .enumerate()
            .rev()
            .find(|(_, event)| event.kind == CausalEventKind::SnapshotCreated)
            .context("actual current snapshot")?;
        let prediction = observed.predictions.last().context("actual active prediction")?;
        anyhow::ensure!(
            prediction.kind == CausalEventKind::PredictionStarted
                && configured_backend(prediction)
                && same_input(&prediction.lineage, &snapshot.lineage)
                && observed
                    .predictions
                    .iter()
                    .skip(snapshot_index + 1)
                    .filter(|event| event.kind == CausalEventKind::PredictionStarted)
                    .count()
                    == 1,
            "unique configured prediction follows actual input snapshot"
        );
        let snapshot = snapshot.lineage.clone();
        let prediction = prediction.lineage.clone();
        let start_index = observed.predictions.len() - 1;
        if let Some(previous) = observed.bindings.iter().find(|binding| binding.snapshot == snapshot) {
            anyhow::ensure!(
                previous.prediction == prediction && previous.start_index == start_index && previous.request == body,
                "inconsistent physical retry"
            );
            return Ok(previous.reply.clone());
        }
        let reply = decision_answers(&body, observed.mode)?;
        anyhow::ensure!(observed.bindings.len() < MAX_RECORDS, "logical binding bound");
        observed.bindings.push(ReplyBinding {
            snapshot,
            prediction,
            start_index,
            request: body,
            reply: reply.clone(),
        });
        Ok(reply)
    })();
    observed.requests += 1;
    if observed.requests > MAX_RECORDS {
        observed.overflow = true;
    }
    if result.is_err() {
        observed.rejected += 1;
    }
    if observed.records.len() < MAX_RECORDS {
        // Fixed labels and counts only: no request text, messages or credentials.
        observed.records.push(json!({
            "constructed": result.is_ok(),
            "answer_count": result.as_ref().ok().and_then(|value| value.get("answers"))
                .and_then(Value::as_object).map_or(0, serde_json::Map::len),
            "error": result.as_ref().err().map(|error| format!("{error:#}")),
        }));
    }
    result.map_or_else(
        |_| axum::http::StatusCode::BAD_REQUEST.into_response(),
        |answer| axum::Json(answer).into_response(),
    )
}

// Box only the integration fixture future; production and performance fixtures are unchanged.
pub fn run<T, F, Fut>(
    provider: Box<dyn Provider>,
    primary_tool: &'static str,
    expected_turns: usize,
    case: F,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<T>>>>
where
    F: FnOnce(AgentBuilder, tempfile::TempDir) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    Box::pin(run_mode(
        provider,
        primary_tool,
        expected_turns,
        DecisionMode::NativeTool,
        None,
        case,
    ))
}

// This entry is used only by the max-depth positive integration case.
#[allow(dead_code)]
pub fn run_delegate<T, F, Fut>(
    provider: Box<dyn Provider>,
    agents: std::collections::HashMap<String, openprx::config::DelegateAgentConfig>,
    case: F,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<T>>>>
where
    F: FnOnce(AgentBuilder, tempfile::TempDir) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    Box::pin(run_mode(
        provider,
        "delegate",
        1,
        DecisionMode::Delegate,
        Some(agents),
        case,
    ))
}

async fn run_mode<T, F, Fut>(
    provider: Box<dyn Provider>,
    primary_tool: &'static str,
    expected_turns: usize,
    mode: DecisionMode,
    agents: Option<std::collections::HashMap<String, openprx::config::DelegateAgentConfig>>,
    case: F,
) -> Result<T>
where
    F: FnOnce(AgentBuilder, tempfile::TempDir) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    anyhow::ensure!(
        (1..=MAX_RECORDS).contains(&expected_turns),
        "bounded nonempty case required"
    );
    let mut workspace_builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        workspace_builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let mut workspace = workspace_builder.tempdir()?;
    // Retain SQLite/evidence even if setup, the case, or its caller is cancelled.
    workspace.disable_cleanup(true);
    let subscriber = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(path = %workspace.path().display(), "retained integration native workspace");
    });
    let retained_workspace = workspace.path().to_owned();
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let state = Arc::new(parking_lot::Mutex::new(Observations {
        mode,
        ..Observations::default()
    }));
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    let app = axum::Router::new()
        .route("/v1/systemone", axum::routing::post(reply))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .with_state(Arc::clone(&state));
    let mut server = Server {
        stop,
        task: Some(tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        })),
    };
    // Configuration, build, every original assertion and Agent drop are inside
    // the panic boundary. The server owner remains outside until explicit join.
    let outcome = std::panic::AssertUnwindSafe(async {
        let mut config = Config {
            workspace_dir: workspace.path().to_owned(),
            config_path: workspace.path().join("config.toml"),
            default_model: Some("prx-local-model".to_owned()),
            ..Config::default()
        };
        if let Some(agents) = agents {
            config.agents = agents;
        }
        config.memory.auto_save = false;
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
        config.cost.prices.insert(
            "prx-local-model".to_owned(),
            ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            },
        );
        config.tool_tiering.always_include = vec![primary_tool.to_owned()];
        // Only this positive fixture publishes an actual structured tool provider.
        config.providers.models.push(serde_json::from_value(json!({
            "provider":"ollama", "model":"prx-local-model", "operations":["chat"], "native_tools":true
        }))?);
        config.causal_tree.enabled = true;
        config.causal_tree.backend.kind = serde_json::from_value(json!("compatible_http"))?;
        config.causal_tree.backend.endpoint.clone_from(&endpoint);
        DECISION_MODEL.clone_into(&mut config.causal_tree.backend.model);
        config.causal_tree.backend.remote_enabled = false;
        let decision_provider = format!("custom:{endpoint}");
        config.providers.models.push(serde_json::from_value(json!({
            "provider":decision_provider, "model":DECISION_MODEL, "operations":["decision_endpoint"]
        }))?);
        config.cost.prices.insert(
            format!("{decision_provider}/{DECISION_MODEL}"),
            ModelPricing {
                input: 0.1,
                output: 0.2,
                cache_write: 0.3,
                cache_read: 0.1,
            },
        );
        anyhow::ensure!(
            config.causal_tree.rehearsal.timeout_ms == 100,
            "unchanged rehearsal budget"
        );
        anyhow::ensure!(
            config.causal_tree.policy.extra_latency_budget_ms == 500,
            "unchanged turn budget"
        );
        let memory = Arc::new(SqliteMemory::new(workspace.path())?);
        let builder = Agent::builder()
            .runtime_config(config, "ollama")
            .provider(Box::new(NativeScriptedProvider {
                inner: provider,
                primary_tool,
            }))
            .tools(vec![])
            .memory(memory)
            .observer(Arc::new(InputObserver(Arc::clone(&state))))
            .tool_dispatcher(Box::new(NativeToolDispatcher));
        Ok::<T, anyhow::Error>(case(builder, workspace).await)
    })
    .catch_unwind()
    .await;
    let joined = server.finish().await;
    let validation = {
        let observed = state.lock();
        (|| -> Result<()> {
            anyhow::ensure!(
                !observed.overflow && observed.rejected == 0 && observed.bindings.len() == expected_turns,
                "physical prediction bindings incomplete"
            );
            anyhow::ensure!(
                observed.requests >= expected_turns
                    && observed.requests <= MAX_RECORDS
                    && observed.records.len() == observed.requests,
                "bounded physical evidence complete"
            );
            for binding in &observed.bindings {
                let prediction = &binding.prediction;
                anyhow::ensure!(
                    observed
                        .predictions
                        .iter()
                        .skip(binding.start_index + 1)
                        .filter(|event| event.lineage == *prediction
                            && event.kind == CausalEventKind::PredictionSucceeded
                            && configured_backend(event)
                            && event.fields.served == Some(ServedLabel::Primary))
                        .count()
                        == 1,
                    "configured prediction was not accepted"
                );
                anyhow::ensure!(
                    !observed.predictions.iter().any(|event| event.lineage == *prediction
                        && (matches!(
                            event.kind,
                            CausalEventKind::PredictionFailed | CausalEventKind::BackendFallback
                        ) || (event.kind == CausalEventKind::PredictionSucceeded
                            && (!configured_backend(event) || event.fields.served != Some(ServedLabel::Primary))))),
                    "prediction used failure fallback or cache"
                );
            }
            anyhow::ensure!(
                !observed
                    .predictions
                    .iter()
                    .any(|event| event.kind == CausalEventKind::PredictionSucceeded
                        && !observed
                            .bindings
                            .iter()
                            .any(|binding| binding.prediction == event.lineage)),
                "prediction success lacks physical binding"
            );
            Ok(())
        })()
    };
    let evidence = {
        let observed = state.lock();
        json!({
            "schema":1, "expected_turns":expected_turns, "requested_action":mode.action_target().0,
            "requested_target":mode.action_target().1,
            "validation_class":if validation.is_ok() { match mode { DecisionMode::NativeTool => "accepted_configured_native_prediction", DecisionMode::Delegate => "accepted_configured_delegate_prediction" } } else { "prediction_evidence_failed" },
            "predictions":observed.predictions.iter().map(|event| json!({"kind":event.kind.as_str(),"lineage":lineage_capture(&event.lineage),"backend":event.fields.backend.map(BackendKind::as_str),"locality":event.fields.locality.map(BackendLocality::as_str),"configured_model":event.fields.configured_model.as_deref(),"served":event.fields.served.map(ServedLabel::as_str)})).collect::<Vec<_>>(),
            "bindings":observed.bindings.iter().map(|binding| json!({"snapshot":lineage_capture(&binding.snapshot),"prediction":lineage_capture(&binding.prediction),"start_index":binding.start_index,"request":binding.request,"reply":binding.reply})).collect::<Vec<_>>(),
            "requests":observed.requests, "rejected":observed.rejected,
            "records":observed.records, "overflow":observed.overflow,"physical_overflow":observed.requests > MAX_RECORDS,
            "listener_joined":joined.is_ok(),
            "case_closure_completed":matches!(&outcome, Ok(Ok(_))),
        })
    };
    let retained = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(retained_workspace.join("native-admission.json"))?;
        let bytes = serde_json::to_vec_pretty(&evidence)?;
        anyhow::ensure!(bytes.len() <= MAX_CAPTURE_BYTES, "aggregate capture bound");
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    match outcome {
        Err(panic) => {
            if retained.is_err() {
                let _ = writeln!(std::io::stderr(), "native fixture capture failed");
            }
            if joined.is_err() {
                let _ = writeln!(std::io::stderr(), "native fixture cleanup failed");
            }
            std::panic::resume_unwind(panic)
        }
        Ok(result) => {
            retained.context("retain owned native evidence")?;
            joined?;
            validation?;
            let value = result?;
            Ok(value)
        }
    }
}

fn decision_answers(body: &serde_json::Value, mode: DecisionMode) -> anyhow::Result<serde_json::Value> {
    use serde_json::{Value, json};
    let (action, target) = mode.action_target();
    anyhow::ensure!(
        body.get("model").and_then(Value::as_str) == Some(DECISION_MODEL),
        "unexpected decision model"
    );
    let candidates = body
        .pointer("/state/candidates")
        .and_then(Value::as_array)
        .context("actual candidate projection")?;
    anyhow::ensure!(
        candidates
            .iter()
            .filter(
                |candidate| candidate.get("action").and_then(Value::as_str) == Some(action)
                    && candidate.get("target").and_then(Value::as_str) == Some(target)
            )
            .count()
            == 1,
        "unique actual primary tool family candidate required"
    );
    let target_index = candidates
        .iter()
        .position(|candidate| {
            candidate.get("action").and_then(Value::as_str) == Some(action)
                && candidate.get("target").and_then(Value::as_str) == Some(target)
        })
        .context("actual requested action/family candidate")?;
    let target_path = format!("$.candidates[{target_index}]");
    let questions = body
        .get("questions")
        .and_then(Value::as_object)
        .context("actual salted wire questions")?;
    anyhow::ensure!(!questions.is_empty(), "empty decision request");
    let mut answers = serde_json::Map::new();
    for (id, question) in questions {
        let instruction = question
            .get("instructions")
            .and_then(Value::as_str)
            .context("actual question instructions")?;
        let kind = question
            .get("type")
            .and_then(Value::as_str)
            .context("actual question type")?;
        let target_question = instruction.contains(&target_path);
        let candidate_question = candidates.iter().enumerate().any(|(index, _)| {
            let path = format!("$.candidates[{index}]");
            instruction == format!("Does {path} apply to this state?")
                || instruction == format!("Would {path} succeed?")
                || instruction == format!("Rate the risk of {path}.")
                || instruction == format!("Rate the task gain from {path}.")
        });
        let answer = match kind {
            "choice" => {
                let chosen = match instruction {
                    "Choose the best legal next action." => action,
                    "Choose the capability family needed by the next action." => target,
                    _ => anyhow::bail!("unexpected choice question"),
                };
                let options = question
                    .get("criteria")
                    .and_then(Value::as_object)
                    .context("choice criteria")?;
                anyhow::ensure!(options.contains_key(chosen), "required actual choice absent");
                let probabilities: serde_json::Map<String, Value> = options
                    .keys()
                    .map(|option| (option.clone(), json!(if option == chosen { 1.0 } else { 0.0 })))
                    .collect();
                json!({"type":"choice","choice":chosen,"probabilities":probabilities,"confidence":1.0})
            }
            "noul" => {
                let yes = match instruction {
                    "Is the observed evidence sufficient for the next action?" => true,
                    "Could the observed artifacts be stale?" => false,
                    _ if candidate_question
                        && (instruction.starts_with("Does ") || instruction.starts_with("Would ")) =>
                    {
                        target_question
                    }
                    _ => anyhow::bail!("unexpected noul question"),
                };
                json!({"type":"noul","noul":if yes {1.0} else {0.0}})
            }
            "score" => {
                let high = match instruction {
                    "Rate the expected task gain." => true,
                    "Rate the risk of the next action." => false,
                    _ if candidate_question && instruction.starts_with("Rate the risk of ") => false,
                    _ if candidate_question && instruction.starts_with("Rate the task gain from ") => target_question,
                    _ => anyhow::bail!("unexpected score question"),
                };
                let levels = question
                    .get("criteria")
                    .and_then(Value::as_array)
                    .context("score criteria")?;
                let last = levels.len().checked_sub(1).context("nonempty score legend")?;
                let selected = if high { last } else { 0 };
                let legend: serde_json::Map<String, Value> = levels
                    .iter()
                    .enumerate()
                    .map(|(index, level)| (index.to_string(), level.clone()))
                    .collect();
                let probabilities: serde_json::Map<String, Value> = levels
                    .iter()
                    .enumerate()
                    .map(|(index, _)| (index.to_string(), json!(if index == selected { 1.0 } else { 0.0 })))
                    .collect();
                json!({"type":"score","score":selected,"legend":legend,"probabilities":probabilities,"confidence":1.0})
            }
            _ => anyhow::bail!("unexpected decision primitive"),
        };
        answers.insert(id.clone(), answer);
    }
    Ok(json!({"model":DECISION_MODEL,"answers":answers,"usage":{"input_tokens":20,"output_tokens":10}}))
}

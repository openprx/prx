#![cfg(test)]
//! Controlled compatible decision endpoint for real embedded native-tool fixture admission.
//! This supplies predictions to the production compiler/validator/gates, never grants or selected refs.
use anyhow::{Context, Result};
use axum::response::IntoResponse;
use futures::FutureExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const DECISION_MODEL: &str = "prx-native-decision";
struct State {
    action: &'static str,
    target: &'static str,
    requests: parking_lot::Mutex<Vec<Value>>,
    replies: parking_lot::Mutex<Vec<Result<Value, String>>>,
}
async fn reply(
    axum::extract::State(state): axum::extract::State<Arc<State>>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    state.requests.lock().push(body.clone());
    let result = decision_answers(&body, state.action, state.target).map_err(|error| format!("{error:#}"));
    state.replies.lock().push(result.clone());
    result.map_or_else(
        |_| axum::http::StatusCode::BAD_REQUEST.into_response(),
        |answer| axum::Json(answer).into_response(),
    )
}

pub(crate) struct Driver {
    endpoint: String,
    diagnostics_dir: Option<std::path::PathBuf>,
    state: Arc<State>,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}
impl Driver {
    pub(crate) async fn start(target: &'static str) -> Result<Self> {
        Self::start_action("use_tool", target).await
    }
    pub(crate) async fn start_answer() -> Result<Self> {
        Self::start_action("answer", "none").await
    }
    async fn start_action(action: &'static str, target: &'static str) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let state = Arc::new(State {
            action,
            target,
            requests: parking_lot::Mutex::new(Vec::new()),
            replies: parking_lot::Mutex::new(Vec::new()),
        });
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let app = axum::Router::new()
            .route("/v1/systemone", axum::routing::post(reply))
            .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
            .with_state(Arc::clone(&state));
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        });
        Ok(Self {
            endpoint,
            diagnostics_dir: None,
            state,
            stop,
            task,
        })
    }
    pub(crate) fn retain_evidence_at(mut self, directory: std::path::PathBuf) -> Self {
        self.diagnostics_dir = Some(directory);
        self
    }
    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub(crate) async fn run<T>(mut self, require_request: bool, future: impl std::future::Future<Output = T>) -> T {
        // All test panics and ordinary returns pass the listener join before escape.
        let result = std::panic::AssertUnwindSafe(Box::pin(future)).catch_unwind().await;
        self.stop.cancel();
        let joined = tokio::time::timeout(std::time::Duration::from_secs(5), &mut self.task).await;
        if joined.is_err() {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
        // Only neutral fixture requests/responses are retained. Keep strict
        // response construction failures visible instead of accepting a400
        // followed by builtin fallback as configured-backend coverage.
        let requests = self.state.requests.lock().clone();
        let replies = self.state.replies.lock().clone();
        let diagnostic = json!({"fixture_action":self.state.action,"fixture_target":self.state.target,"requests":requests,"replies":replies});
        if let Some(directory) = self
            .diagnostics_dir
            .take()
            .or_else(|| std::env::var_os("PRX_NATIVE_DECISION_DIAGNOSTICS").map(std::path::PathBuf::from))
        {
            assert!(
                directory.is_absolute() && directory.is_dir(),
                "explicit existing fixture diagnostic directory required"
            );
            let path = directory.join(format!("decision-{}.json", uuid::Uuid::new_v4()));
            std::fs::write(
                path,
                serde_json::to_vec_pretty(&diagnostic).expect("neutral fixture diagnostic serialization"),
            )
            .expect("retain actual fixture request/response evidence");
        }
        match result {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(result) => {
                joined
                    .expect("local decision listener must drain")
                    .expect("local listener task")
                    .expect("local listener serving");
                // This proves the configured production transport was reached. Missing
                // executable choices are returned as400 and remain fail-closed at gates.
                if require_request {
                    assert!(
                        !self.state.requests.lock().is_empty(),
                        "actual compatible decision request missing: {diagnostic}"
                    );
                    assert_eq!(
                        requests.len(),
                        replies.len(),
                        "every actual request requires an actual response construction outcome: {diagnostic}"
                    );
                    assert!(
                        replies.iter().all(Result::is_ok),
                        "controlled decision response rejected; no fallback counts as positive backend admission: {diagnostic}"
                    );
                }
                result
            }
        }
    }
}

pub(crate) fn configure(config: &mut crate::config::Config, endpoint: &str) -> Result<()> {
    config.causal_tree.enabled = true;
    config.causal_tree.backend.kind =
        crate::causal_tree::config::BackendSelection::Fixed(crate::decision::BackendKind::CompatibleHttp);
    config.causal_tree.backend.endpoint = endpoint.to_owned();
    config.causal_tree.backend.model = DECISION_MODEL.to_owned();
    config.causal_tree.backend.remote_enabled = false;
    let provider = format!("custom:{endpoint}");
    config.providers.models.push(serde_json::from_value(json!({
        "provider":provider,"model":DECISION_MODEL,"operations":["decision_endpoint"]
    }))?);
    config.cost.prices.insert(
        format!("{provider}/{DECISION_MODEL}"),
        crate::config::schema::ModelPricing {
            input: 0.1,
            output: 0.2,
            cache_write: 0.3,
            cache_read: 0.1,
        },
    );
    assert_eq!(config.causal_tree.policy.extra_latency_budget_ms, 500);
    Ok(())
}
fn decision_answers(body: &serde_json::Value, action: &str, target: &str) -> anyhow::Result<serde_json::Value> {
    use serde_json::{Value, json};
    assert_eq!(body.get("model").and_then(Value::as_str), Some(DECISION_MODEL));
    let candidates = body
        .pointer("/state/candidates")
        .and_then(Value::as_array)
        .context("actual candidate projection")?;
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

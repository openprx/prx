//! Owned physical decision fixture for the two selected-memory integration cases.
//! Observed journal lineage scopes fixture data; it never supplies runtime authority.
use anyhow::{Context, Result, ensure};
use axum::response::IntoResponse;
use futures::FutureExt;
use openprx::decision::protocol::{BackendKind, BackendLocality};
use openprx::decision::telemetry::{CausalEvent, CausalEventKind, CausalLineage, ServedLabel};
use openprx::memory::{Memory, MemoryCategory, MemoryStoreMetadata, SqliteMemory};
use openprx::observability::{Observer, ObserverEvent, ObserverMetric};
use serde_json::{Value, json};
use std::io::Write;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const DECISION_MODEL: &str = "prx-selected-memory-decision";
const MAX_REQUESTS: usize = 16;
const MAX_PREDICTION_EVENTS: usize = MAX_REQUESTS * 4;
const MAX_CAPTURE_BYTES: usize = 262_144;
// Mirrors the existing source freshness classification; does not configure it.
const FRESH_MEMORY_MS: u64 = 5 * 60 * 1_000;

#[derive(Default)]
struct Snapshots {
    values: Vec<CausalLineage>,
    predictions: Vec<(u64, CausalEvent)>,
    next_sequence: u64,
    latest_snapshot_sequence: Option<u64>,
    overflow: bool,
}
#[derive(Default)]
pub struct InputObserver(parking_lot::Mutex<Snapshots>);
impl Observer for InputObserver {
    fn record_event(&self, event: &ObserverEvent) {
        if let ObserverEvent::Causal(event) = event {
            let mut snapshots = self.0.lock();
            if event.kind == CausalEventKind::SnapshotCreated {
                if snapshots.values.len() == MAX_REQUESTS {
                    snapshots.overflow = true;
                } else {
                    snapshots.values.push(event.lineage.clone());
                    snapshots.latest_snapshot_sequence = Some(snapshots.next_sequence);
                    snapshots.next_sequence += 1;
                }
            }
            if matches!(
                event.kind,
                CausalEventKind::PredictionStarted
                    | CausalEventKind::PredictionSucceeded
                    | CausalEventKind::PredictionFailed
                    | CausalEventKind::BackendFallback
            ) {
                if snapshots.predictions.len() == MAX_PREDICTION_EVENTS {
                    snapshots.overflow = true;
                } else {
                    let sequence = snapshots.next_sequence;
                    snapshots.next_sequence += 1;
                    snapshots.predictions.push((sequence, (**event).clone()));
                }
            }
        }
    }
    fn record_metric(&self, _: &ObserverMetric) {}
    fn name(&self) -> &'static str {
        "prx_selected_memory_fixture"
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct Turn {
    source: String,
    run: String,
    last_decision: String,
    last_hop: u64,
    retrieved_prediction: bool,
    answer_reply_constructed: bool,
}
struct ReplyEvidence {
    lineage: CausalLineage,
    prediction_lineage: CausalLineage,
    start_sequence: u64,
}
fn lineage_capture(lineage: &CausalLineage) -> Value {
    json!({"workspace_id":lineage.workspace_id.as_ref(),"owner_id":lineage.owner_id.as_deref(),
        "session_key":lineage.session_key.as_deref(),"run_id":lineage.run_id.as_deref(),
        "task_id":lineage.task_id.as_deref(),"source_event_id":lineage.source_event_id.as_deref(),
        "causation_event_id":lineage.causation_event_id.as_deref(),"decision_id":lineage.decision_id.as_deref()})
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
#[derive(Default)]
struct Progress {
    // A physical retry under the same actual decision must be byte-equivalent.
    replies: Vec<(String, Value, Value)>,
    reply_evidence: Vec<ReplyEvidence>,
    turns: Vec<Turn>,
    private_scope: Option<(String, String, u64, String)>,
    attempts: Vec<Value>,
    capture_bytes: usize,
    failed: bool,
}
struct State {
    memory: Arc<SqliteMemory>,
    observer: Arc<InputObserver>,
    workspace: std::path::PathBuf,
    fact: String,
    progress: tokio::sync::Mutex<Progress>,
}

async fn response(
    axum::extract::State(state): axum::extract::State<Arc<State>>,
    body: std::result::Result<axum::Json<Value>, axum::extract::rejection::JsonRejection>,
) -> axum::response::Response {
    let mut progress = state.progress.lock().await;
    let Ok(axum::Json(body)) = body else {
        progress.failed = true;
        capture_attempt(&mut progress, json!({"valid":false,"error_class":"invalid_json_body"}));
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };
    let result = respond(&state, &mut progress, &body).await;
    let record = json!({"request":body,"reply":result.as_ref().ok(),"valid":result.is_ok()});
    if !capture_attempt(&mut progress, record) {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    result.map_or_else(
        |_| {
            progress.failed = true;
            axum::http::StatusCode::BAD_REQUEST.into_response()
        },
        |reply| axum::Json(reply).into_response(),
    )
}

fn capture_attempt(progress: &mut Progress, record: Value) -> bool {
    let Ok(bytes) = serde_json::to_vec(&record) else {
        progress.failed = true;
        return false;
    };
    if progress.attempts.len() >= MAX_REQUESTS || progress.capture_bytes.saturating_add(bytes.len()) > MAX_CAPTURE_BYTES
    {
        progress.failed = true;
        return false;
    }
    progress.capture_bytes += bytes.len();
    progress.attempts.push(record);
    true
}

async fn respond(state: &State, progress: &mut Progress, body: &Value) -> Result<Value> {
    ensure!(
        !progress.failed && progress.attempts.len() < MAX_REQUESTS,
        "fixture request bound"
    );
    let lineage = {
        let snapshots = state.observer.0.lock();
        ensure!(!snapshots.overflow, "snapshot bound");
        snapshots.values.last().cloned().context("actual current snapshot")?
    };
    let owner = lineage.owner_id.as_deref().context("actual owner")?;
    let session = lineage.session_key.as_deref().context("actual session")?;
    let run = lineage.run_id.as_deref().context("actual run")?;
    let source = lineage.source_event_id.as_deref().context("actual source input")?;
    let decision = lineage.decision_id.as_deref().context("actual decision")?;
    ensure!(
        lineage.workspace_id.as_ref() == state.workspace.to_string_lossy().as_ref(),
        "workspace differs"
    );
    let event = state
        .memory
        .find_owned_message_event(&lineage.workspace_id, owner, source)
        .await?
        .context("actual durable input")?;
    ensure!(
        event.id > 0
            && event.event_id == source
            && event.workspace_id == lineage.workspace_id.as_ref()
            && event.owner_id.as_deref() == Some(owner)
            && event.source == openprx::memory::MessageEventSource::Agent
            && event.channel.as_deref() == Some("agent_api")
            && event.sender.as_deref() == Some("embedded-caller")
            && event.session_key.as_deref() == Some(session)
            && event.recipient.as_deref() == Some(session)
            && event.run_id.as_deref() == Some(run)
            && event.role == "user"
            && event.event_type == "message.created"
            && event.parent_session_key.is_none()
            && event.parent_run_id.is_none()
            && event.agent_id.is_none()
            && event.persona_id.is_none()
            && event.attempt_id.is_none()
            && event.lease_epoch.is_none()
            && event.subject.is_none()
            && event.causation_event_id.is_none()
            && event.visibility == openprx::memory::MemoryVisibility::Session
            && event.idempotency_key.as_deref() == Some(format!("agent_api:{run}:input").as_str()),
        "durable input differs from actual snapshot"
    );
    ensure!(
        body.pointer("/state/schema").and_then(Value::as_str) == Some("prx.causal.state.v1")
            && body.pointer("/state/latest_user_turn").and_then(Value::as_str) == Some(event.content.as_str()),
        "physical request differs from current input"
    );
    ensure!(
        state
            .observer
            .0
            .lock()
            .values
            .last()
            .and_then(|snapshot| snapshot.decision_id.as_deref())
            == Some(decision),
        "snapshot changed during journal lookup"
    );
    let (start_sequence, prediction_lineage) = {
        let observed = state.observer.0.lock();
        ensure!(!observed.overflow, "observer evidence overflow");
        let snapshot_sequence = observed
            .latest_snapshot_sequence
            .context("snapshot event sequence missing")?;
        let (sequence, prediction) = observed
            .predictions
            .iter()
            .rev()
            .find(|(_, prediction)| same_input(&prediction.lineage, &lineage))
            .context("current prediction start missing")?;
        ensure!(
            prediction.kind == CausalEventKind::PredictionStarted && configured_backend(prediction),
            "physical request is outside configured active prediction"
        );
        ensure!(
            *sequence > snapshot_sequence
                && observed
                    .predictions
                    .iter()
                    .filter(|(sequence, event)| *sequence > snapshot_sequence
                        && event.kind == CausalEventKind::PredictionStarted
                        && same_input(&event.lineage, &lineage))
                    .count()
                    == 1,
            "unique active snapshot prediction association required"
        );
        (*sequence, prediction.lineage.clone())
    };
    let generation = event.config_generation_id.context("actual input generation")?;
    let revision = event
        .config_source_revision
        .as_ref()
        .filter(|s| !s.is_empty())
        .context("actual generation revision")?;
    if let Some(scope) = &progress.private_scope {
        ensure!(
            scope == &(owner.to_owned(), session.to_owned(), generation, revision.clone()),
            "private scope drift"
        );
    }
    if let Some((_, previous, reply)) = progress.replies.iter().find(|(id, _, _)| id == decision) {
        ensure!(previous == body, "inconsistent physical retry");
        ensure!(
            progress.reply_evidence.iter().any(|reply| reply.lineage == lineage
                && reply.prediction_lineage == prediction_lineage
                && reply.start_sequence == start_sequence),
            "physical retry prediction association differs"
        );
        return Ok(reply.clone());
    }
    let hop = body
        .pointer("/state/hop")
        .and_then(Value::as_u64)
        .context("actual hop")?;
    let prior = body
        .pointer("/state/prior_decision_id")
        .context("actual prior decision field")?;
    let same_turn = progress.turns.last().is_some_and(|turn| turn.source == source);
    if same_turn {
        let turn = progress.turns.last().context("current turn")?;
        ensure!(
            turn.run == run
                && !turn.answer_reply_constructed
                && hop == turn.last_hop + 1
                && prior.as_str() == Some(turn.last_decision.as_str()),
            "actual decision chain differs"
        );
    } else {
        ensure!(
            progress.turns.len() < 2
                && hop == 0
                && prior.is_null()
                && !progress
                    .turns
                    .iter()
                    .any(|turn| turn.source == source || turn.run == run)
                && progress.turns.last().is_none_or(|turn| turn.answer_reply_constructed),
            "new turn chain differs"
        );
    }
    if progress.private_scope.is_none() {
        // This projects only write scope from the real event. No receipt, grant,
        // source proof or authority object is constructed or supplied to Agent.
        let mut envelope = openprx::runtime::envelope::RuntimeEnvelope::agent(state.workspace.to_string_lossy(), run)
            .with_channel("agent_api")
            .with_sender("embedded-caller")
            .with_recipient(session);
        envelope.session_key = session.to_owned();
        ensure!(
            envelope.resolved_owner_id() == owner,
            "fixture owner projection differs"
        );
        let mut scope = envelope.memory_write_context("private");
        scope.owner_id = Some(owner.to_owned());
        scope.require_owner_match = true;
        state
            .memory
            .store_with_context_and_metadata(
                "prx_selected_fact",
                &state.fact,
                MemoryCategory::Core,
                None,
                Some(&scope),
                MemoryStoreMetadata {
                    workspace_id: Some(event.workspace_id.clone()),
                    owner_id: Some(owner.to_owned()),
                    channel: Some("agent_api".to_owned()),
                    ..Default::default()
                },
            )
            .await?;
        progress.private_scope = Some((owner.to_owned(), session.to_owned(), generation, revision.clone()));
    }
    let steps = body
        .pointer("/state/completed_steps")
        .and_then(Value::as_array)
        .context("actual completed steps")?;
    let artifacts = body
        .pointer("/state/artifacts")
        .and_then(Value::as_array)
        .context("actual artifacts")?;
    let retrieved = steps.iter().any(|step| {
        step.get("kind").and_then(Value::as_str) == Some("retrieve")
            && step.get("status").and_then(Value::as_str) == Some("ok")
    });
    let fresh_fact = artifacts.iter().any(|artifact| {
        artifact.get("kind").and_then(Value::as_str) == Some("memory")
            && artifact
                .get("age_ms")
                .and_then(Value::as_u64)
                .is_some_and(|age| age < FRESH_MEMORY_MS)
            && artifact.get("summary").and_then(Value::as_str) == Some(state.fact.as_str())
    });
    let (action, target) = if same_turn {
        ensure!(
            progress.turns.last().is_some_and(|turn| turn.retrieved_prediction) && retrieved && fresh_fact,
            "current turn has no fresh successful retrieval"
        );
        ("answer", "none")
    } else {
        ensure!(!retrieved && !fresh_fact, "new turn must perform its own retrieval");
        ("retrieve", "memory")
    };
    let reply = decision_answers(body, action, target)?;
    if same_turn {
        let turn = progress.turns.last_mut().context("current turn")?;
        decision.clone_into(&mut turn.last_decision);
        turn.last_hop = hop;
        turn.answer_reply_constructed = true;
    } else {
        progress.turns.push(Turn {
            source: source.to_owned(),
            run: run.to_owned(),
            last_decision: decision.to_owned(),
            last_hop: hop,
            retrieved_prediction: true,
            answer_reply_constructed: false,
        });
    }
    progress.reply_evidence.push(ReplyEvidence {
        lineage: lineage.clone(),
        prediction_lineage,
        start_sequence,
    });
    progress
        .replies
        .push((decision.to_owned(), body.clone(), reply.clone()));
    Ok(reply)
}

pub struct Driver {
    pub endpoint: String,
    pub observer: Arc<InputObserver>,
    state: Arc<State>,
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}
impl Drop for Driver {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
        // Cancellation fallback only: Drop cannot confirm an asynchronous join.
    }
}
impl Driver {
    pub async fn start(memory: Arc<SqliteMemory>, workspace: std::path::PathBuf, fact: &str) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let observer = Arc::new(InputObserver::default());
        let state = Arc::new(State {
            memory,
            observer: Arc::clone(&observer),
            workspace,
            fact: fact.to_owned(),
            progress: tokio::sync::Mutex::new(Progress::default()),
        });
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let app = axum::Router::new()
            .route("/v1/systemone", axum::routing::post(response))
            .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
            .with_state(Arc::clone(&state));
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        });
        Ok(Self {
            endpoint,
            observer,
            state,
            stop,
            task: Some(task),
        })
    }
    pub async fn run<T>(mut self, expected_turns: usize, future: impl std::future::Future<Output = T>) -> T {
        let outcome = std::panic::AssertUnwindSafe(Box::pin(future)).catch_unwind().await;
        self.stop.cancel();
        let cleanup = async {
            let task = self.task.as_mut().context("owned listener")?;
            let joined = tokio::time::timeout(std::time::Duration::from_secs(5), &mut *task).await;
            if joined.is_err() {
                task.abort();
                let _ = (&mut *task).await;
            }
            let _ = self.task.take();
            joined.context("listener deadline")??.context("listener serving")?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        // A timed-out server may still have a connection holding progress.
        // Bound evidence acquisition too; it must not indefinitely delay the
        // original panic after an already-failed cleanup.
        let evidence = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.retain_and_verify(expected_turns, cleanup.is_ok(), outcome.is_err()),
        )
        .await
        {
            Ok(result) => result,
            Err(error) => Err(anyhow::Error::new(error).context("private evidence deadline")),
        };
        match outcome {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(value) => {
                assert!(cleanup.is_ok(), "actual listener cleanup must complete: {cleanup:?}");
                assert!(
                    evidence.is_ok(),
                    "physical retrieval then Answer required for every original turn: {evidence:?}"
                );
                value
            }
        }
    }
    async fn retain_and_verify(
        &self,
        expected_turns: usize,
        cleanup_confirmed: bool,
        business_panicked: bool,
    ) -> Result<()> {
        let progress = self.state.progress.lock().await;
        let (predictions, overflow) = {
            let observed = self.state.observer.0.lock();
            (observed.predictions.clone(), observed.overflow)
        };
        let validation = (|| -> Result<()> {
            ensure!(!overflow, "observer evidence overflow");
            ensure!(
                progress.reply_evidence.len() == expected_turns * 2,
                "original Retrieve then Answer decision count"
            );
            for reply in &progress.reply_evidence {
                let matching: Vec<_> = predictions
                    .iter()
                    .filter(|(_, event)| event.lineage == reply.prediction_lineage)
                    .collect();
                ensure!(
                    matching
                        .iter()
                        .filter(|(sequence, event)| *sequence > reply.start_sequence
                            && event.kind == CausalEventKind::PredictionSucceeded
                            && configured_backend(event)
                            && event.fields.served == Some(ServedLabel::Primary))
                        .count()
                        == 1,
                    "configured physical prediction was not accepted"
                );
                ensure!(
                    !matching.iter().any(|(_, event)| matches!(
                        event.kind,
                        CausalEventKind::PredictionFailed | CausalEventKind::BackendFallback
                    ) || (event.kind == CausalEventKind::PredictionSucceeded
                        && (!configured_backend(event) || event.fields.served != Some(ServedLabel::Primary)))),
                    "prediction used failure fallback or cache"
                );
            }
            ensure!(
                !predictions
                    .iter()
                    .any(|(_, event)| event.kind == CausalEventKind::PredictionSucceeded
                        && !progress
                            .reply_evidence
                            .iter()
                            .any(|reply| reply.prediction_lineage == event.lineage)),
                "prediction success lacks physical reply evidence"
            );
            ensure!(
                !progress.failed
                    && progress.turns.len() == expected_turns
                    && progress
                        .turns
                        .iter()
                        .all(|turn| turn.retrieved_prediction && turn.answer_reply_constructed),
                "physical evidence incomplete"
            );
            Ok(())
        })();
        let prediction_capture: Vec<_> = predictions
            .iter()
            .map(|(sequence, event)| {
                json!({
                    "sequence":sequence,"kind":event.kind.as_str(),"lineage":lineage_capture(&event.lineage),
                    "backend":event.fields.backend.map(BackendKind::as_str),
                    "locality":event.fields.locality.map(BackendLocality::as_str),
                    "configured_model":event.fields.configured_model.as_deref(),
                    "served":event.fields.served.map(ServedLabel::as_str),
                    "error_class":event.fields.error_class,"error_rule":event.fields.error_rule
                })
            })
            .collect();
        let reply_capture: Vec<_> = progress
            .reply_evidence
            .iter()
            .map(|reply| {
                json!({
                    "runtime_lineage":lineage_capture(&reply.lineage),
                    "prediction_lineage":lineage_capture(&reply.prediction_lineage),
                    "start_sequence":reply.start_sequence
                })
            })
            .collect();
        let capture = serde_json::to_vec(&json!({"attempts":progress.attempts,
            "failed":progress.failed,"predictions":prediction_capture,"reply_bindings":reply_capture,"validation_class":if validation.is_ok() { "accepted_configured_predictions" } else { "acceptance_evidence_failed" },"turns":progress.turns.len(),"listener_cleanup_confirmed":cleanup_confirmed,"business_panicked":business_panicked}))?;
        ensure!(capture.len() <= MAX_CAPTURE_BYTES, "private capture bound");
        let path = self.state.workspace.join("selected-memory-wire.json");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(&capture)?;
        file.sync_all()?;
        validation
    }
}

pub fn configure(config: &mut openprx::config::Config, endpoint: &str) -> Result<()> {
    config.causal_tree.enabled = true;
    config.causal_tree.backend.kind = serde_json::from_value(json!("compatible_http"))?;
    endpoint.clone_into(&mut config.causal_tree.backend.endpoint);
    DECISION_MODEL.clone_into(&mut config.causal_tree.backend.model);
    config.causal_tree.backend.remote_enabled = false;
    let provider = format!("custom:{endpoint}");
    config.providers.models.push(serde_json::from_value(json!({
        "provider":provider,"model":DECISION_MODEL,"operations":["decision_endpoint"]
    }))?);
    config.cost.prices.insert(
        format!("{provider}/{DECISION_MODEL}"),
        openprx::config::schema::ModelPricing {
            input: 0.1,
            output: 0.2,
            cache_write: 0.3,
            cache_read: 0.1,
        },
    );
    ensure!(
        config.causal_tree.policy.extra_latency_budget_ms == 500 && config.causal_tree.rehearsal.timeout_ms == 100,
        "original decision budgets"
    );
    Ok(())
}

pub struct AgentFixture(pub openprx::agent::agent::Agent);
impl AgentFixture {
    pub fn turn<'a>(
        &'a mut self,
        message: &'a str,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<String>> + 'a>> {
        Box::pin(self.0.turn(message))
    }
}
impl std::ops::Deref for AgentFixture {
    type Target = openprx::agent::agent::Agent;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn decision_answers(body: &serde_json::Value, action: &str, target: &str) -> anyhow::Result<serde_json::Value> {
    use serde_json::{Value, json};
    anyhow::ensure!(
        body.get("model").and_then(Value::as_str) == Some(DECISION_MODEL),
        "actual decision model"
    );
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
                    "Is the observed evidence sufficient for the next action?" => action == "answer",
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

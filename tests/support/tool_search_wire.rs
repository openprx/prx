//! Loopback provider for source-bound search, schema append and real reads.
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub const MODEL: &str = "prx-search-probe";
pub const PARENT_MODEL: &str = "prx-parent-probe";
pub const INPUT: &str = "Read the saved project probe";
pub const DONE: &str = "PRX_WIRE_FINISHED";
pub const STORED: &str = "PRX_STORED_SENTINEL";
pub const INSTRUCTIONS: &str = "PRX_INSTRUCTION_SENTINEL";

struct ProbeState {
    workspace: PathBuf,
    skill: Option<bool>,
    no_tools: AtomicBool,
    requests: parking_lot::Mutex<Vec<Value>>,
    errors: parking_lot::Mutex<Vec<String>>,
    completed: AtomicBool,
    completion: tokio::sync::Notify,
    parent_mode: Option<&'static str>,
    parent_calls: std::sync::atomic::AtomicUsize,
}

pub struct WireProbe {
    endpoint: String,
    state: Arc<ProbeState>,
    stop: CancellationToken,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl WireProbe {
    pub async fn start(workspace: PathBuf, skill: Option<bool>, parent_mode: Option<&'static str>) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let endpoint = format!("http://{}/v1", listener.local_addr()?);
        let state = Arc::new(ProbeState {
            workspace,
            skill,
            parent_mode,
            no_tools: AtomicBool::new(false),
            requests: parking_lot::Mutex::default(),
            errors: parking_lot::Mutex::default(),
            completed: AtomicBool::new(false),
            completion: tokio::sync::Notify::default(),
            parent_calls: std::sync::atomic::AtomicUsize::default(),
        });
        let stop = CancellationToken::new();
        let server_stop = stop.clone();
        let app = Router::new()
            .route("/v1/chat/completions", post(inference))
            .route(
                "/v1/models",
                get(|State(state): State<Arc<ProbeState>>| async move {
                    let mut models = vec![json!({"id":MODEL,"object":"model"})];
                    if state.parent_mode.is_some() {
                        models.push(json!({"id":PARENT_MODEL,"object":"model"}));
                    }
                    Json(json!({"object":"list","data":models}))
                }),
            )
            .with_state(Arc::clone(&state));
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(server_stop.cancelled_owned())
                .await
        });
        Ok(Self {
            endpoint,
            state,
            stop,
            server: Some(server),
        })
    }

    pub fn require_no_tools(&self) {
        self.state.no_tools.store(true, Ordering::Release);
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub fn provider(&self) -> String {
        format!("custom:{}", self.endpoint)
    }

    pub async fn finish(mut self) -> Result<()> {
        self.stop.cancel();
        let mut task = self.server.take().context("provider server owner")?;
        match tokio::time::timeout(Duration::from_secs(5), &mut task).await {
            Ok(result) => result??,
            Err(error) => {
                task.abort();
                let _ = task.await;
                return Err(error.into());
            }
        }
        let errors = self.state.errors.lock();
        ensure!(errors.is_empty(), "wire probe errors: {errors:?}");
        ensure!(
            self.state.completed.load(Ordering::Acquire),
            "probe never reached actual reader completion"
        );
        ensure!(
            self.state.requests.lock().len()
                == if self.state.no_tools.load(Ordering::Acquire) {
                    1
                } else if self.state.skill == Some(true) {
                    4
                } else {
                    3
                },
            "unexpected provider round count"
        );
        if self.state.parent_mode.is_some() {
            ensure!(
                self.state.parent_calls.load(Ordering::Acquire) >= 3,
                "parent did not actually spawn and await the child"
            );
        }
        Ok(())
    }
}

impl Drop for WireProbe {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

fn seed_row(workspace: &std::path::Path) -> Result<()> {
    let db = rusqlite::Connection::open(workspace.join("memory/brain.db"))?;
    db.busy_timeout(Duration::from_secs(5))?;
    // Data fixture only: ownership comes from the actual admitted input,
    // never from model arguments or a manufactured execution grant.
    let (owner, agent, session, event, source, channel, sender): (String, Option<String>, String, String, String, Option<String>, Option<String>) = db.query_row(
        "SELECT owner_id,agent_id,session_key,event_id,source,channel,sender FROM message_events WHERE role='user' ORDER BY id DESC LIMIT 1",
        [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?)),
    )?;
    ensure!(
        !owner.is_empty() && !session.is_empty() && !event.is_empty(),
        "reader fixture requires real durable ingress"
    );
    db.execute(
        "INSERT OR REPLACE INTO memories(id,key,content,category,created_at,updated_at,workspace_id,owner_id,agent_id,session_id,source_event_id,source,channel,chat_id,raw_sender,sender_id)
         VALUES ('prx_wire_row','memory/prx_probe.md',?1,'core','2026-11-11T00:00:00Z','2026-11-11T00:00:00Z',?2,?3,?4,?5,?6,?7,?8,?5,?9,?3)",
        rusqlite::params![STORED,workspace.to_string_lossy(),owner,agent,session,event,source,channel,sender],
    )?;
    Ok(())
}

fn names(request: &Value) -> Vec<&str> {
    request
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str))
        .collect()
}

fn tool_call(name: &str, args: &Value, round: usize) -> Value {
    json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("prx-wire-{round}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]})
}

async fn next_message(state: &ProbeState, request: &Value) -> Result<Value> {
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .context("actual provider messages")?;
    let system = messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
        .filter_map(|message| message.get("content").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if system.contains("Classify the execution requirement of current_request") {
        let informational = state.no_tools.load(Ordering::Acquire)
            && request.get("model").and_then(Value::as_str) != Some(PARENT_MODEL);
        return Ok(json!({"role":"assistant","content":if informational {
            r#"{"requirement":"informational"}"#
        } else { r#"{"requirement":"execution_required"}"# }}));
    }
    if system.contains("Review the proposed final reply against the CURRENT request") {
        return Ok(
            json!({"role":"assistant","content":r#"{"decision":"accept","reason":"actual probe tool completed","constraints":[]}"#}),
        );
    }
    if let Some(mode) = state.parent_mode
        && request.get("model").and_then(Value::as_str) == Some(PARENT_MODEL)
        && messages.iter().any(|message| {
            message.get("role").and_then(Value::as_str) == Some("user")
                && message
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains("PRX_PARENT"))
        })
    {
        let round = state.parent_calls.fetch_add(1, Ordering::AcqRel);
        if round == 0 {
            ensure!(
                names(request).contains(&"capability_search"),
                "parent has no discovery control"
            );
            ensure!(
                !names(request).contains(&"sessions_spawn"),
                "chat spawn must initially be deferred"
            );
            return Ok(tool_call(
                "capability_search",
                &json!({"query":"sessions_spawn","limit":1}),
                100,
            ));
        }
        if round == 1 {
            ensure!(
                names(request).contains(&"sessions_spawn"),
                "parent search did not admit spawn; tool replies={:?}",
                messages
                    .iter()
                    .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
                    .collect::<Vec<_>>()
            );
            return Ok(tool_call(
                "sessions_spawn",
                &json!({"action":"spawn","task":"hi","mode":mode,"model":MODEL}),
                101,
            ));
        }
        let latest = messages
            .iter()
            .rev()
            .find(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
            .context("actual parent tool result")?;
        if round == 2 {
            ensure!(
                latest.to_string().contains("Sub-agent spawned"),
                "parent spawn failed: {latest}"
            );
            return Ok(tool_call("sessions_spawn", &json!({"action":"list"}), 102));
        }
        ensure!(
            !latest.to_string().contains("failed:"),
            "worker failed before probe completion: {latest}"
        );
        let notified = state.completion.notified();
        if !state.completed.load(Ordering::Acquire)
            && tokio::time::timeout(Duration::from_secs(60), notified).await.is_err()
        {
            // Read the runtime's terminal status through the same admitted
            // tool before the next response can claim parent completion.
            return Ok(tool_call("sessions_spawn", &json!({"action":"list"}), 103));
        }
        return Ok(json!({"role":"assistant","content":"PRX_PARENT_FINISHED"}));
    }
    let round = state.requests.lock().len();
    let offered = names(request);
    let tool_results = || {
        messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
            .collect::<Vec<_>>()
    };
    if state.no_tools.load(Ordering::Acquire) {
        ensure!(offered.is_empty(), "non-tool model received tools");
        ensure!(
            !system.contains("<available_skills>"),
            "non-tool model received unreadable skill metadata"
        );
        ensure!(round == 0, "non-tool control unexpectedly retried");
        state.requests.lock().push(request.clone());
        state.completed.store(true, Ordering::Release);
        state.completion.notify_one();
        return Ok(json!({"role":"assistant","content":DONE}));
    }

    if round == 0 {
        ensure!(
            offered.contains(&"capability_search"),
            "first request lacks reachable capability_search"
        );
        ensure!(
            !offered.contains(&"memory_get"),
            "deferred memory reader was eagerly declared"
        );
        seed_row(&state.workspace)?;
        if state.skill == Some(false) {
            ensure!(
                !system.contains("<available_skills>"),
                "isolated source received unreadable skills"
            );
            ensure!(
                !offered.contains(&"skill_read"),
                "isolated source received an unusable reader"
            );
            ensure!(
                !request.to_string().contains(INSTRUCTIONS),
                "isolated source received a skill body"
            );
        }
        if state.skill == Some(true) {
            ensure!(
                system.contains("<name>prx_probe</name>"),
                "first request lacks installed skill metadata"
            );
            ensure!(offered.contains(&"skill_read"), "metadata has no same-request reader");
            ensure!(
                !request.to_string().contains(INSTRUCTIONS),
                "skill body was eagerly loaded"
            );
        }
    }
    let reader_round = usize::from(state.skill == Some(true));
    let message = if state.skill == Some(true) && round == 0 {
        tool_call("skill_read", &json!({"name":"prx_probe"}), round)
    } else if round == reader_round {
        if state.skill == Some(true) {
            ensure!(
                messages
                    .iter()
                    .any(|message| message.get("role").and_then(Value::as_str) == Some("tool")
                        && message.to_string().contains(INSTRUCTIONS)),
                "live skill_read did not return the installed body; tool_results={:?}",
                tool_results()
            );
        }
        tool_call("capability_search", &json!({"query":"memory_get","limit":1}), round)
    } else if round == reader_round + 1 {
        ensure!(
            offered.contains(&"memory_get"),
            "search hit not declared on the next request"
        );
        tool_call("memory_get", &json!({"path":"memory/prx_probe.md"}), round)
    } else {
        ensure!(round == reader_round + 2, "unexpected provider iteration");
        ensure!(
            messages
                .iter()
                .any(|message| message.get("role").and_then(Value::as_str) == Some("tool")
                    && message.to_string().contains(STORED)),
            "real memory_get did not return the durable row; tool_results={:?}",
            tool_results()
        );
        state.completed.store(true, Ordering::Release);
        state.completion.notify_one();
        json!({"role":"assistant","content":DONE})
    };
    state.requests.lock().push(request.clone());
    Ok(message)
}

async fn inference(State(state): State<Arc<ProbeState>>, Json(request): Json<Value>) -> Response {
    match next_message(&state, &request).await {
        Ok(message) => {
            let usage = json!({"prompt_tokens":20,"completion_tokens":10,"total_tokens":30});
            let model = request.get("model").and_then(Value::as_str).unwrap_or(MODEL);
            if request.get("stream").and_then(Value::as_bool) == Some(true) {
                let delta = message.get("tool_calls").and_then(Value::as_array).map_or_else(
                    || json!({"role":"assistant","content":message.get("content")}),
                    |calls| {
                        let indexed = calls
                            .iter()
                            .enumerate()
                            .map(|(index, call)| {
                                let mut call = call.clone();
                                if let Some(fields) = call.as_object_mut() {
                                    fields.insert("index".to_owned(), json!(index));
                                }
                                call
                            })
                            .collect::<Vec<_>>();
                        json!({"role":"assistant","tool_calls":indexed})
                    },
                );
                let chunk =
                    json!({"id":"prx-wire","model":model,"choices":[{"index":0,"delta":delta,"finish_reason":null}]});
                let finish = json!({"id":"prx-wire","model":model,"choices":[{"index":0,"delta":{},"finish_reason":if message.get("tool_calls").is_some(){"tool_calls"}else{"stop"}}],"usage":usage});
                (
                    [("content-type", "text/event-stream")],
                    format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
                )
                    .into_response()
            } else {
                Json(json!({"id":"prx-wire","model":model,"choices":[{"index":0,"message":message,"finish_reason":if message.get("tool_calls").is_some(){"tool_calls"}else{"stop"}}],"usage":usage})).into_response()
            }
        }
        Err(error) => {
            state.errors.lock().push(error.to_string());
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    }
}

#![cfg(test)]
//! Genuine embedded ingress -> registered HTTP provider -> Automix -> durable replay/learning.
use super::Agent;
use crate::agent::dispatcher::XmlToolDispatcher;
use crate::config::{Config, RouterConfig, RouterModelConfig};
use crate::memory::{Memory, SqliteMemory};
use crate::providers::compatible::{AuthStyle, OpenAiCompatibleProvider};
use crate::router::RouterEngine;
use anyhow::Context;
use futures::FutureExt;
use serde_json::{Value, json};
use std::{panic::AssertUnwindSafe, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const INPUT: &str = "memory_search: Explain the Rust expression `2 + 2`.";
const CHEAP: &str = "prx-cheap";
const PREMIUM: &str = "prx-premium";
const CHEAP_REPLY: &str = "I'm not sure, maybe the expression adds two integers.";
const PREMIUM_REPLY: &str = "The Rust expression adds two integers and equals four.";

struct ListenerOwner {
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    requests: Arc<parking_lot::Mutex<Vec<Value>>>,
    address: std::net::SocketAddr,
    premium_seen: Arc<tokio::sync::Notify>,
}
impl ListenerOwner {
    async fn start(replies: Vec<(u16, Value)>, block_premium: bool) -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let premium_seen = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let seen = Arc::clone(&premium_seen);
        let proceed = Arc::clone(&release);
        let task = tokio::spawn(async move {
            let mut replies = replies.into_iter();
            loop {
                let work = async {
                    let (mut socket, peer) = listener.accept().await?;
                    anyhow::ensure!(peer.ip().is_loopback(), "fixture accepted non-loopback peer");
                    let mut bytes = Vec::new();
                    let (header_end, length) = loop {
                        let mut chunk = [0_u8; 1024];
                        let read = socket.read(&mut chunk).await?;
                        anyhow::ensure!(read > 0, "HTTP request ended before headers");
                        bytes.extend_from_slice(chunk.get(..read).context("invalid read length")?);
                        anyhow::ensure!(bytes.len() <= 32 * 1024, "HTTP request exceeds fixture limit");
                        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                            let end = end + 4;
                            let header = std::str::from_utf8(bytes.get(..end).context("header missing")?)?;
                            let length = header
                                .lines()
                                .find_map(|line| {
                                    let (key, value) = line.split_once(':')?;
                                    key.eq_ignore_ascii_case("content-length").then_some(value.trim())
                                })
                                .context("content length missing")?
                                .parse::<usize>()?;
                            anyhow::ensure!(length <= 16 * 1024, "HTTP body exceeds fixture limit");
                            break (end, length);
                        }
                    };
                    while bytes.len() < header_end + length {
                        let mut chunk = [0_u8; 1024];
                        let read = socket.read(&mut chunk).await?;
                        anyhow::ensure!(read > 0, "HTTP body truncated");
                        bytes.extend_from_slice(chunk.get(..read).context("invalid read length")?);
                        anyhow::ensure!(bytes.len() <= 32 * 1024, "HTTP request exceeds fixture limit");
                    }
                    let header = std::str::from_utf8(bytes.get(..header_end).context("header missing")?)?;
                    anyhow::ensure!(
                        header
                            .lines()
                            .next()
                            .is_some_and(|line| line.starts_with("POST ") && line.contains("/chat/completions ")),
                        "unexpected endpoint"
                    );
                    let body: Value =
                        serde_json::from_slice(bytes.get(header_end..header_end + length).context("body missing")?)?;
                    anyhow::ensure!(
                        matches!(body.get("model").and_then(Value::as_str), Some(CHEAP | PREMIUM)),
                        "unexpected model"
                    );
                    let premium = body.get("model").expect("model field exists") == PREMIUM;
                    captured.lock().push(body);
                    if premium && block_premium {
                        seen.notify_one();
                        proceed.notified().await;
                    }
                    let (status, body) = replies.next().context("unexpected extra physical request")?;
                    let body = serde_json::to_vec(&body)?;
                    let head = format!(
                        "HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(head.as_bytes()).await?;
                    socket.write_all(&body).await?;
                    socket.shutdown().await?;
                    Ok::<_, anyhow::Error>(())
                };
                tokio::select! {
                    biased;
                    () = stop.cancelled() => return Ok(()),
                    result = tokio::time::timeout(Duration::from_secs(5), work) => result.context("fixture connection watchdog expired")??,
                }
            }
        });
        Ok(Self {
            cancel,
            task: Some(task),
            requests,
            address,
            premium_seen,
        })
    }
    async fn finish(&mut self) -> anyhow::Result<()> {
        self.cancel.cancel();
        let mut task = self.task.take().context("listener already joined")?;
        match tokio::time::timeout(Duration::from_secs(5), &mut task).await {
            Ok(joined) => joined.context("listener task panicked")?,
            Err(_) => {
                task.abort();
                let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
                anyhow::bail!("listener teardown watchdog expired");
            }
        }
    }
}
impl Drop for ListenerOwner {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn response(text: &str) -> Value {
    json!({"choices":[{"message":{"role":"assistant","content":text}}],
        "usage":{"prompt_tokens":20,"completion_tokens":10,"total_tokens":30}})
}

// Keep setup state out of the harness that polls the real Agent turn.
fn build_agent<'a>(
    workspace: &'a std::path::Path,
    listener: &'a ListenerOwner,
    recording: bool,
) -> std::pin::Pin<Box<impl std::future::Future<Output = anyhow::Result<Agent>> + 'a>> {
    Box::pin(build_agent_work(workspace, listener, recording))
}

async fn build_agent_work(
    workspace: &std::path::Path,
    listener: &ListenerOwner,
    recording: bool,
) -> anyhow::Result<Agent> {
    let url = format!("http://{}", listener.address);
    let label = format!("custom:{url}");
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace)?);
    let policy = RouterConfig {
        alpha: 0.0,
        beta: 0.0,
        gamma: 1.0,
        delta: 0.0,
        epsilon: 0.0,
        models: [CHEAP, PREMIUM]
            .into_iter()
            .map(|model| RouterModelConfig {
                model_id: model.into(),
                provider: label.clone(),
                cost_per_million_tokens: 0.2,
                max_context: 128_000,
                reserved_output_tokens: None,
                latency_ms: 1,
                categories: vec!["conversation".into(), "coding".into()],
                elo_rating: if model == CHEAP { 2_000.0 } else { 1_000.0 },
            })
            .collect(),
        automix: crate::config::AutomixConfig {
            cheap_model_tiers: vec![CHEAP.into()],
            premium_model_id: PREMIUM.into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let router = RouterEngine::new(policy.clone(), label.clone(), Vec::new(), Arc::clone(&memory), None).await?;
    let mut config = Config::default();
    config.workspace_dir = workspace.to_path_buf();
    config.config_path = workspace.join("config.toml");
    config.default_model = Some(CHEAP.into());
    config.causal_tree.enabled = recording;
    // A stricter supported retrieval fixture; never increase the default 500ms allowance.
    config.causal_tree.policy.max_decision_hops = 1;
    assert_eq!(config.causal_tree.policy.extra_latency_budget_ms, 500);
    config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
    config.router = policy;
    for model in [CHEAP, PREMIUM] {
        config.providers.models.push(serde_json::from_value(json!({
            "provider":label,"model":model,"operations":["chat"],"native_tools":true
        }))?);
        config.cost.prices.insert(
            format!("{label}/{model}"),
            crate::config::schema::ModelPricing {
                input: 0.1,
                output: 0.2,
                cache_write: 0.3,
                cache_read: 0.1,
            },
        );
        let resolved = crate::providers::selection::check(
            &config,
            &label,
            model,
            &crate::providers::catalog::Requirement::chat().with(crate::providers::catalog::Feature::NativeTools),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )?;
        assert_eq!(
            resolved.verification(),
            &crate::providers::catalog::Verification::Verified
        );
    }
    let provider = OpenAiCompatibleProvider::new_optional_credential(&label, &url, None, AuthStyle::Bearer);
    let agent = Agent::builder()
        .runtime_config(config, &label)
        .provider(Box::new(provider))
        .tools(vec![Box::new(crate::tools::memory_search::MemorySearchTool::new(
            workspace.to_owned(),
            Arc::clone(&memory),
            true,
        ))])
        .memory(memory)
        .observer(Arc::new(crate::observability::NoopObserver {}))
        .tool_dispatcher(Box::new(XmlToolDispatcher))
        .router(router)
        .build()?;
    // This helper records an actual source before deriving owner/private/chat metadata.
    // The anchor is not a model turn or a fabricated decision grant.
    agent.seed_scoped_query_memory_for_test(INPUT).await;
    Ok(agent)
}

fn turn(agent: &mut Agent) -> std::pin::Pin<Box<impl std::future::Future<Output = anyhow::Result<String>> + '_>> {
    Box::pin(agent.turn(INPUT))
}

// Verification owns no turn authority and need not inflate its polling caller.
fn verify_result<'a>(
    agent: &'a Agent,
    workspace: &'a std::path::Path,
    recording: bool,
    listener: &'a ListenerOwner,
) -> std::pin::Pin<Box<impl std::future::Future<Output = anyhow::Result<()>> + 'a>> {
    Box::pin(verify_result_work(agent, workspace, recording, listener))
}

async fn verify_result_work(
    agent: &Agent,
    workspace: &std::path::Path,
    recording: bool,
    listener: &ListenerOwner,
) -> anyhow::Result<()> {
    let requests = listener.requests.lock().clone();
    assert_eq!(
        requests.len(),
        2,
        "one actual cheap and one actual premium physical request"
    );
    assert_eq!(
        requests
            .first()
            .context("cheap request")?
            .get("model")
            .expect("model field exists"),
        CHEAP
    );
    assert_eq!(
        requests
            .last()
            .context("premium request")?
            .get("model")
            .expect("model field exists"),
        PREMIUM
    );
    assert!(
        requests.last().context("premium request")?.get("tools").is_none(),
        "premium receives no tool authority"
    );
    let database = rusqlite::Connection::open_with_flags(
        workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let (terminal_key, raw): (String, String) = database.query_row(
        "SELECT idempotency_key,raw_payload_json FROM message_events WHERE event_type='turn.finalized' ORDER BY id DESC LIMIT 1", [],
        |row| Ok((row.get(0)?, row.get(1)?)))?;
    let payload: Value = serde_json::from_str(&raw)?;
    assert_eq!(
        payload
            .get("provider_outcome")
            .expect("provider_outcome field exists")
            .get("final_model")
            .expect("final_model field exists"),
        CHEAP,
        "main aggregate attribution must not be rewritten"
    );
    let workspace_id = workspace.to_string_lossy();
    let terminal = agent
        .memory
        .find_message_event_by_idempotency_key(&workspace_id, &terminal_key)
        .await?
        .context("actual terminal")?;
    let run = terminal.run_id.as_deref().context("terminal run")?;
    let owner = terminal.owner_id.as_deref().context("terminal owner")?;
    let auxiliary = agent
        .memory
        .find_message_event_by_idempotency_key(&workspace_id, &format!("turn:{run}:automix_premium"))
        .await?
        .context("actual auxiliary receipt")?;
    let auxiliary_payload: Value =
        serde_json::from_str(auxiliary.raw_payload_json.as_deref().context("auxiliary payload")?)?;
    assert_eq!(
        auxiliary_payload.get("purpose").expect("purpose field exists"),
        "automix_premium"
    );
    assert_eq!(
        auxiliary_payload
            .get("provider_outcome")
            .expect("provider_outcome field exists")
            .get("final_model")
            .expect("final_model field exists"),
        PREMIUM
    );
    assert_eq!(auxiliary.owner_id, terminal.owner_id);
    assert_eq!(auxiliary.session_key, terminal.session_key);
    assert_eq!(auxiliary.run_id, terminal.run_id);
    assert!(auxiliary.id < terminal.id);
    assert_eq!(
        payload
            .get("served_auxiliary")
            .expect("served_auxiliary field exists")
            .get("provider_outcome")
            .expect("provider_outcome field exists")
            .get("final_model")
            .expect("final_model field exists"),
        PREMIUM
    );
    assert_eq!(
        payload
            .get("served_auxiliary")
            .expect("served_auxiliary field exists")
            .get("event_id")
            .expect("event_id field exists")
            .as_str()
            .expect("event_id is a string"),
        auxiliary.event_id.as_str()
    );
    let costs: Vec<Value> = std::fs::read_to_string(workspace.join("state/costs.jsonl"))?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(
        costs.len(),
        2,
        "cheap aggregate is deduplicated after early settlement; premium is separate"
    );
    assert_eq!(
        costs
            .iter()
            .filter(|row| row.get("settlement_id").expect("settlement_id field exists") == run)
            .count(),
        1
    );
    let premium_cost = costs
        .iter()
        .find(|row| {
            row.get("settlement_id")
                .expect("settlement_id field exists")
                .as_str()
                .is_some_and(|id| id.starts_with("automix_premium:"))
        })
        .context("physical premium debit")?;
    assert!(
        premium_cost
            .get("usage")
            .expect("usage field exists")
            .get("model")
            .expect("model field exists")
            .as_str()
            .is_some_and(|model| model.ends_with(PREMIUM))
    );
    let journal: Vec<Value> = std::fs::read_to_string(workspace.join("state/costs.reservations.jsonl"))?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    for kind in ["dispatched", "estimated_settled_v1"] {
        let rows: Vec<_> = journal
            .iter()
            .filter(|row| row.get("event").expect("event field exists") == kind)
            .collect();
        assert_eq!(rows.len(), 1, "one physical premium accounting receipt");
        assert_eq!(
            rows.first()
                .context("actual journal event")?
                .get("attempt_id")
                .expect("attempt_id field exists"),
            premium_cost.get("settlement_id").expect("settlement_id field exists")
        );
    }
    let state: String = database.query_row("SELECT snapshot FROM router_learning_state", [], |row| row.get(0))?;
    let state: crate::memory::traits::RouterLearningSnapshot = serde_json::from_str(&state)?;
    assert_eq!(state.version, 1);
    let cheap = state
        .models
        .iter()
        .find(|model| model.model == CHEAP)
        .context("cheap priors")?;
    let premium = state
        .models
        .iter()
        .find(|model| model.model == PREMIUM)
        .context("premium metrics")?;
    assert!(cheap.successes.is_empty(), "cheap was not the delivered answer");
    assert_eq!(premium.successes.iter().copied().collect::<Vec<_>>(), vec![true]);
    let receipts: i64 = database.query_row(
        "SELECT COUNT(*) FROM router_learning_receipts WHERE terminal=?1",
        [run],
        |row| row.get(0),
    )?;
    assert_eq!(receipts, 1, "one actual terminal-trained premium outcome");
    drop(database);
    if recording {
        assert!(payload.get("terminal_replay_v1").is_some());
        let store = crate::causal_tree::store::CausalDecisionStore::open_unshared_read_only(workspace);
        let key = crate::decision::key::DecisionKey::load_existing(workspace)?;
        let store_id = store.workspace_id().context("actual decision store UUID")?;
        crate::agent::terminal::verify_terminal_replay(
            agent.memory.as_ref(),
            &store.reader(),
            &key,
            &store_id,
            &workspace_id,
            owner,
            run,
            &terminal,
        )
        .await?;
        // A changed durable projection cannot be accepted by the real replay verifier.
        let mut tampered = terminal.clone();
        let mut changed = payload.clone();
        *changed
            .get_mut("served_auxiliary")
            .expect("served auxiliary exists")
            .get_mut("provider_outcome")
            .expect("served outcome exists")
            .get_mut("final_model")
            .expect("served model exists") = json!(CHEAP);
        tampered.raw_payload_json = Some(serde_json::to_string(&changed)?);
        assert!(
            crate::agent::terminal::verify_terminal_replay(
                agent.memory.as_ref(),
                &store.reader(),
                &key,
                &store_id,
                &workspace_id,
                owner,
                run,
                &tampered
            )
            .await
            .is_err()
        );
    } else {
        assert!(
            payload.get("terminal_replay_v1").is_none(),
            "recording-off cannot claim a signed decision replay"
        );
        assert!(
            !workspace.join("self/causal_tree/decisions.db").exists(),
            "off must not create a writable decision store"
        );
    }
    Ok(())
}

// Synchronous construction returns before the fixture future is polled.
fn run_case(recording: bool) -> std::pin::Pin<Box<impl std::future::Future<Output = anyhow::Result<()>>>> {
    Box::pin(run_case_work(recording))
}

async fn run_case_work(recording: bool) -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let mut listener = ListenerOwner::start(
        vec![(200, response(CHEAP_REPLY)), (200, response(PREMIUM_REPLY))],
        false,
    )
    .await?;
    let mut agent = None;
    let result = AssertUnwindSafe(async {
        agent = Some(build_agent(workspace.path(), &listener, recording).await?);
        let agent = agent.as_mut().context("built Agent")?;
        let reply = tokio::time::timeout(Duration::from_secs(5), turn(agent))
            .await
            .context("Automix turn fixture watchdog expired")??;
        assert_eq!(
            reply,
            PREMIUM_REPLY,
            "Automix premium not served; retained workspace={} physical_requests={}",
            workspace.path().display(),
            listener.requests.lock().len()
        );
        agent.cleanup_state.confirm().await?;
        verify_result(agent, workspace.path(), recording, &listener).await
    })
    .catch_unwind()
    .await;
    // These watchdogs bound fixture teardown only, never the original decision allowance.
    let agent_cleanup = if let Some(agent) = agent.as_mut() {
        matches!(
            tokio::time::timeout(Duration::from_secs(5), agent.cleanup_state.confirm()).await,
            Ok(Ok(()))
        )
    } else {
        true
    };
    let listener_cleanup = listener.finish().await;
    if !agent_cleanup || listener_cleanup.is_err() {
        let retained = workspace.keep();
        anyhow::bail!("Automix cleanup unconfirmed; retained {}", retained.display());
    }
    if !matches!(&result, Ok(Ok(()))) {
        let _retained_workspace = workspace.keep();
    }
    drop(agent);
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[tokio::test]
async fn embedded_automix_real_http_premium_has_signed_replay_separate_cost_and_learning() {
    run_case(true).await.expect("real recorded Automix ingress");
}

#[tokio::test]
async fn embedded_automix_causal_plane_off_keeps_premium_cost_and_learning_without_decision_replay() {
    run_case(false).await.expect("real causal-plane-off Automix ingress");
}

#[tokio::test]
async fn embedded_automix_drop_during_real_premium_send_retains_unknown_liability_without_served_link() {
    run_dropped_premium().await.expect("owned Automix drop cleanup");
}

fn run_dropped_premium() -> std::pin::Pin<Box<impl std::future::Future<Output = anyhow::Result<()>>>> {
    Box::pin(run_dropped_premium_work())
}

async fn run_dropped_premium_work() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let mut listener =
        ListenerOwner::start(vec![(200, response(CHEAP_REPLY)), (200, response(PREMIUM_REPLY))], true).await?;
    let mut agent = None;
    let result = AssertUnwindSafe(async {
        agent = Some(build_agent(workspace.path(), &listener, true).await?);
        let agent = agent.as_mut().context("built Agent")?;
        let mut future = turn(agent);
        tokio::select! {
            biased;
            () = listener.premium_seen.notified() => {},
            result = &mut future => anyhow::bail!("turn finished before actual premium barrier: {result:?}; retained workspace={} physical_requests={}", workspace.path().display(), listener.requests.lock().len()),
            () = tokio::time::sleep(Duration::from_secs(5)) => anyhow::bail!("premium barrier watchdog expired"),
        }
        // The real HTTP request is dispatched. Dropping the public future must
        // retain the owned cleanup and the unknown, already-dispatched charge.
        drop(future);
        tokio::time::timeout(Duration::from_secs(5), agent.cleanup_state.confirm()).await??;
        let database = rusqlite::Connection::open_with_flags(workspace.path().join("memory/brain.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let raw: String = database.query_row("SELECT raw_payload_json FROM message_events WHERE event_type='turn.finalized' ORDER BY id DESC LIMIT 1", [], |row| row.get(0))?;
        let payload: Value = serde_json::from_str(&raw)?;
        assert!(matches!(payload.get("status").expect("status field exists").as_str(), Some("failed" | "cancelled")));
        assert!(payload.get("served_auxiliary").is_none());
        assert!(payload.get("terminal_replay_v1").expect("terminal replay field exists").get("served_auxiliary").is_none());
        let auxiliary: String = database.query_row("SELECT raw_payload_json FROM message_events WHERE event_type='provider.auxiliary.settled' ORDER BY id DESC LIMIT 1", [], |row| row.get(0))?;
        let auxiliary: Value = serde_json::from_str(&auxiliary)?;
        assert_eq!(auxiliary.get("purpose").expect("purpose field exists"), "automix_premium");
        assert_eq!(auxiliary.get("usage_uncertain").expect("usage_uncertain field exists"), true);
        assert!(auxiliary.get("served_reply_sha256").expect("served reply hash field exists").is_null());
        let rows: Vec<Value> = std::fs::read_to_string(workspace.path().join("state/costs.reservations.jsonl"))?.lines().map(serde_json::from_str).collect::<Result<_,_>>()?;
        assert_eq!(rows.iter().filter(|row| row.get("event").expect("event field exists") == "dispatched").count(), 1);
        assert_eq!(rows.iter().filter(|row| row.get("event").expect("event field exists") == "estimated_settled_v1").count(), 0);
        let reservation = rows.iter().find(|row| row.get("event").expect("event field exists") == "estimated_reserved_v1").context("actual reservation")?;
        assert!(reservation.get("request").expect("request field exists").get("reservation").expect("reservation field exists").get("attempt_id").expect("attempt_id field exists").as_str().is_some_and(|id| id.starts_with("automix_premium:")));
        assert!(reservation.get("request").expect("request field exists").get("reservation").expect("reservation field exists").get("maximum_micro_usd").expect("maximum_micro_usd field exists").as_u64().is_some_and(|cost| cost > 0));
        let costs: Vec<Value> = std::fs::read_to_string(workspace.path().join("state/costs.jsonl"))?.lines().map(serde_json::from_str).collect::<Result<_,_>>()?;
        assert_eq!(costs.len(), 1, "known cheap debit remains; unknown premium is not a fake zero-cost settlement");
        assert!(costs.first().context("cheap cost")?.get("usage").expect("usage field exists").get("model").expect("model field exists").as_str().is_some_and(|model| model.ends_with(CHEAP)));
        assert_eq!(listener.requests.lock().len(), 2);
        Ok::<_, anyhow::Error>(())
    }).catch_unwind().await;
    let confirmed = if let Some(agent) = agent.as_mut() {
        matches!(
            tokio::time::timeout(Duration::from_secs(5), agent.cleanup_state.confirm()).await,
            Ok(Ok(()))
        )
    } else {
        true
    };
    // Cancel the listener while its response is blocked; no late response is
    // invented to reconcile the intentionally unknown attempt.
    let listener_cleanup = listener.finish().await;
    if !confirmed || listener_cleanup.is_err() {
        let retained = workspace.keep();
        anyhow::bail!("Automix drop cleanup unconfirmed; retained {}", retained.display());
    }
    if !matches!(&result, Ok(Ok(()))) {
        let _retained_workspace = workspace.keep();
    }
    drop(agent);
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

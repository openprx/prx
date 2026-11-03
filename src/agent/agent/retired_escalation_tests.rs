//! Embedded ingress with retired operator input must make one physical request.
use super::Agent;
use crate::agent::dispatcher::XmlToolDispatcher;
use crate::config::{Config, RouterConfig, RouterModelConfig};
use crate::memory::{Memory, SqliteMemory};
use crate::providers::compatible::{AuthStyle, OpenAiCompatibleProvider};
use crate::router::RouterEngine;
use anyhow::Context;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const INPUT: &str = "memory_search: Explain the Rust expression `2 + 2`.";
const CHEAP: &str = "prx-cheap";
const PREMIUM: &str = "prx-premium";
const CHEAP_REPLY: &str = "I'm not sure, maybe the expression adds two integers.";

struct ListenerOwner {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    requests: Arc<parking_lot::Mutex<Vec<Value>>>,
    address: std::net::SocketAddr,
}
impl ListenerOwner {
    async fn start() -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, peer) = tokio::select! {
                    biased;
                    () = stop.cancelled() => return Ok(()),
                    accepted = listener.accept() => accepted?,
                };
                let work = async {
                    anyhow::ensure!(peer.ip().is_loopback(), "non-loopback fixture peer");
                    let mut bytes = Vec::new();
                    let (end, length) = loop {
                        let mut chunk = [0; 1024];
                        let read = socket.read(&mut chunk).await?;
                        anyhow::ensure!(read > 0 && bytes.len() <= 32768, "invalid request");
                        bytes.extend_from_slice(chunk.get(..read).context("chunk")?);
                        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            let end = end + 4;
                            let header = std::str::from_utf8(bytes.get(..end).context("headers")?)?;
                            let length = header
                                .lines()
                                .find_map(|line| {
                                    let (key, value) = line.split_once(':')?;
                                    key.eq_ignore_ascii_case("content-length").then_some(value.trim())
                                })
                                .context("content length")?
                                .parse::<usize>()?;
                            anyhow::ensure!(length <= 16384, "oversized body");
                            break (end, length);
                        }
                    };
                    while bytes.len() < end + length {
                        let mut chunk = [0; 1024];
                        let read = socket.read(&mut chunk).await?;
                        anyhow::ensure!(read > 0, "truncated body");
                        bytes.extend_from_slice(chunk.get(..read).context("chunk")?);
                    }
                    let request: Value = serde_json::from_slice(bytes.get(end..end + length).context("body")?)?;
                    let model = request.get("model").and_then(Value::as_str).context("model")?;
                    anyhow::ensure!(matches!(model, CHEAP | PREMIUM), "unexpected model");
                    let reply = if model == CHEAP {
                        CHEAP_REPLY
                    } else {
                        "observable premium reply"
                    };
                    let body = serde_json::to_vec(
                        &json!({"choices":[{"message":{"role":"assistant","content":reply}}], "usage":{"prompt_tokens":20,"completion_tokens":10,"total_tokens":30}}),
                    )?;
                    captured.lock().push(request);
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(header.as_bytes()).await?;
                    socket.write_all(&body).await?;
                    socket.shutdown().await?;
                    Ok::<_, anyhow::Error>(())
                };
                tokio::select! {
                    biased;
                    () = stop.cancelled() => return Ok(()),
                    result = tokio::time::timeout(Duration::from_secs(5), work) => result??,
                }
            }
        });
        Ok(Self {
            cancel,
            task,
            requests,
            address,
        })
    }
    async fn finish(self) -> anyhow::Result<()> {
        self.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), self.task).await???;
        Ok(())
    }
}
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
        ..Default::default()
    };
    let router = RouterEngine::new(
        policy.clone(),
        label.clone(),
        Vec::new(),
        Arc::clone(&memory),
        None,
        &crate::config::schema::CostConfig::default(),
    )
    .await?;
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
    // Feed the retired operator input through the same merge/legacy loader as startup.
    let stored = config.to_stored_toml_string()?;
    std::fs::write(
        &config.config_path,
        format!(
            "{stored}\n[router.automix]\ncheap_model_tiers = ['{CHEAP}']\npremium_model_id = '{PREMIUM}'\nconfidence_threshold = 0.7\n"
        ),
    )?;
    let config = Config::load_from_path(&config.config_path, workspace.to_owned())?;
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

async fn check_legacy_input(recording: bool, turns: usize) -> anyhow::Result<()> {
    use futures::FutureExt;
    let workspace = tempfile::tempdir()?;
    let listener = ListenerOwner::start().await?;
    let result = Box::pin(
        std::panic::AssertUnwindSafe(async {
            let mut agent = build_agent(workspace.path(), &listener, recording).await?;
            for _ in 0..turns {
                let reply = tokio::time::timeout(Duration::from_secs(5), turn(&mut agent)).await??;
                agent.cleanup_state.confirm().await?;
                assert_eq!(reply, CHEAP_REPLY);
            }
            let requests = listener.requests.lock().clone();
            assert_eq!(
                requests.len(),
                turns,
                "one physical request per turn, including an observable premium endpoint"
            );
            for request in &requests {
                assert_eq!(request.get("model"), Some(&json!(CHEAP)));
            }
            let terminals = crate::memory::sqlite::fixture_readback::terminal_rows(workspace.path())?;
            assert_eq!(terminals.len(), turns);
            let mut run_ids = std::collections::HashSet::new();
            for terminal in &terminals {
                assert!(run_ids.insert(terminal.run_id.as_str()), "turn identity must be unique");
                let payload: Value = serde_json::from_str(&terminal.raw_payload)?;
                assert_eq!(payload.pointer("/provider_outcome/final_model"), Some(&json!(CHEAP)));
                assert!(payload.get("served_auxiliary").is_none());
                assert_eq!(payload.get("terminal_replay_v1").is_some(), recording);
            }
            let costs: Vec<Value> = std::fs::read_to_string(workspace.path().join("state/costs.jsonl"))?
                .lines()
                .map(serde_json::from_str)
                .collect::<Result<_, _>>()?;
            assert_eq!(costs.len(), turns, "one normal main settlement per turn");
            let mut settlement_ids = std::collections::HashSet::new();
            for cost in &costs {
                let settlement = cost
                    .get("settlement_id")
                    .and_then(Value::as_str)
                    .context("settlement")?;
                assert!(settlement_ids.insert(settlement));
                assert!(
                    cost.pointer("/usage/cost_usd")
                        .and_then(Value::as_f64)
                        .is_some_and(|cost| cost > 0.0)
                );
                assert!(
                    cost.pointer("/usage/model")
                        .and_then(Value::as_str)
                        .is_some_and(|model| model.ends_with(CHEAP))
                );
            }
            assert_eq!(settlement_ids, run_ids);
            let reader = crate::memory::sqlite::fixture_readback::RouterReadback::open(workspace.path())?;
            assert_eq!(reader.auxiliary_count()?, 0);
            assert_eq!(reader.learning_receipt_count()?, i64::try_from(turns)?);
            let journal = workspace.path().join("state/costs.reservations.jsonl");
            if journal.exists() {
                assert!(
                    std::fs::read_to_string(journal)?.trim().is_empty(),
                    "no premium hold or dispatch"
                );
            }
            let (domain, snapshot) = reader.learning_state()?;
            assert_eq!(domain.len(), 64);
            assert!(domain.bytes().all(|byte| byte.is_ascii_hexdigit()));
            for terminal in &terminals {
                assert_eq!(
                    reader.learning_terminal_count(
                        &terminal.run_id,
                        &domain,
                        workspace.path().to_string_lossy().as_ref()
                    )?,
                    1
                );
            }
            let snapshot: crate::memory::traits::RouterLearningSnapshot = serde_json::from_str(&snapshot)?;
            assert_eq!(snapshot.version, i64::try_from(turns)?);
            let cheap = snapshot
                .models
                .iter()
                .find(|model| model.model == CHEAP)
                .context("cheap learning")?;
            let premium = snapshot
                .models
                .iter()
                .find(|model| model.model == PREMIUM)
                .context("premium prior")?;
            assert_eq!(cheap.provider, format!("custom:http://{}", listener.address));
            assert_eq!(premium.provider, cheap.provider);
            assert!(cheap.elo > 2000.0);
            assert_eq!(cheap.successes.iter().copied().collect::<Vec<_>>(), vec![true; turns]);
            assert_eq!(premium.elo, 1000.0);
            assert!(premium.successes.is_empty());
            Ok::<_, anyhow::Error>(())
        })
        .catch_unwind(),
    )
    .await;
    listener.finish().await?;
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[tokio::test]
async fn legacy_reply_policy_serves_cheap_reply_once_with_and_without_recording() -> anyhow::Result<()> {
    for recording in [true, false] {
        Box::pin(check_legacy_input(recording, 1)).await?;
    }
    Ok(())
}

#[tokio::test]
async fn repeated_actual_turns_have_unique_cost_and_learning_receipts() -> anyhow::Result<()> {
    Box::pin(check_legacy_input(true, 5)).await
}

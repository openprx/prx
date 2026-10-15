#![cfg(test)]
//! Actual loopback HTTP -> sealed accounting -> durable journal integration.
//! This is not an ingress-proof or external-provider acceptance test.
use super::*;
use crate::providers::compatible::{AuthStyle, OpenAiCompatibleProvider};
use crate::providers::reliable::ReliableProvider;
use crate::providers::{ChatMessage, ChatRequest, Provider};
use crate::runtime::auxiliary_call::{AuxiliaryAccounting, AuxiliaryPurpose};
use crate::runtime::turn_cleanup::{CleanupScope, current_tracker};
use anyhow::Context;
use futures::FutureExt;
use serde_json::{Value, json};
use std::{collections::BTreeSet, panic::AssertUnwindSafe, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct ListenerOwner {
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    requests: Arc<parking_lot::Mutex<Vec<String>>>,
    address: std::net::SocketAddr,
}
impl ListenerOwner {
    async fn start(replies: Vec<(u16, Value)>) -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
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
                    let first = header.lines().next().context("request line missing")?.to_owned();
                    let body: Value =
                        serde_json::from_slice(bytes.get(header_end..header_end + length).context("body missing")?)?;
                    anyhow::ensure!(
                        body.get("model").and_then(Value::as_str) == Some("prx-accounted"),
                        "unexpected model"
                    );
                    captured.lock().push(first);
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

fn reply(usage: Option<Value>) -> Value {
    let mut value = json!({"choices":[{"message":{"role":"assistant","content":"PRX accounted response"}}]});
    if let Some(usage) = usage {
        value
            .as_object_mut()
            .expect("reply object")
            .insert("usage".into(), usage);
    }
    value
}
fn journal(workspace: &std::path::Path) -> anyhow::Result<Vec<Value>> {
    std::fs::read_to_string(workspace.join("state/costs.reservations.jsonl"))?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

#[derive(Clone, Copy)]
enum Case {
    FullRetry,
    FullResponsesFallback,
    Partial,
    Missing,
}

async fn run_case(case: Case) -> anyhow::Result<()> {
    let known = matches!(case, Case::FullRetry | Case::FullResponsesFallback);
    let fallback = matches!(case, Case::FullResponsesFallback);
    let partial = matches!(case, Case::Partial);
    let full = json!({"prompt_tokens":20,"completion_tokens":10,"total_tokens":30});
    let replies = if fallback {
        vec![
            (
                404,
                json!({"error":{"message":"endpoint not found"},"usage":full.clone()}),
            ),
            (
                200,
                json!({"output":[{"type":"message","content":[{"type":"output_text","text":"PRX accounted response"}]}],
                "usage":{"input_tokens":20,"output_tokens":10,"total_tokens":30}}),
            ),
        ]
    } else if known {
        vec![
            (
                500,
                json!({"error":{"message":"temporarily unavailable"},"usage":full.clone()}),
            ),
            (200, reply(Some(full))),
        ]
    } else {
        vec![(200, reply(partial.then(|| json!({"prompt_tokens":20}))))]
    };
    let workspace = tempfile::tempdir()?;
    let mut listener = ListenerOwner::start(replies).await?;
    let (ack, confirmed) = tokio::sync::oneshot::channel();
    let result = AssertUnwindSafe(async {
        let base_url = format!("http://{}", listener.address);
        let label = format!("custom:{base_url}");
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.providers.models.push(serde_json::from_value(json!({
            "provider":label,"model":"prx-accounted","operations":["chat"],"native_tools":false
        }))?);
        config.cost.prices.insert(
            format!("{label}/prx-accounted"),
            crate::config::schema::ModelPricing {
                input: 0.1,
                output: 0.2,
                cache_write: 0.3,
                cache_read: 0.1,
            },
        );
        let catalog = crate::providers::selection::catalog_for(&config);
        let selected = crate::providers::selection::check_with(
            &config,
            &catalog,
            &label,
            "prx-accounted",
            crate::providers::credentials::CredentialRef::None,
            &crate::providers::catalog::Requirement::chat(),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )?;
        let actual = OpenAiCompatibleProvider::new_optional_credential(&label, &base_url, None, AuthStyle::Bearer);
        let provider: Arc<dyn Provider> =
            Arc::new(ReliableProvider::new(vec![(label.clone(), Box::new(actual))], 1, 1));
        let config = Arc::new(config);
        let cancellation = CancellationToken::new();
        let initial_money = crate::cost::tracker::remaining_micro_usd_read_only(&config.cost, workspace.path())?;
        CleanupScope::new(None, cancellation)?
            .with_acknowledgement(Some(ack))
            .run(async {
                let key = DecisionKey::ephemeral();
                let owner = current_tracker().context("actual cleanup scope missing")?;
                // A test-owned accounting operation, not a fabricated ingress grant.
                let scope_id = key.hmac(
                    KeyDomain::AttemptAccounting,
                    &[
                        b"prx-loopback-accounting-regression",
                        workspace.path().as_os_str().as_encoded_bytes(),
                    ],
                );
                let budgets = TurnBudgets {
                    tokens: 8_000,
                    micro_usd: 5_000,
                    latency_ms: config.causal_tree.policy.extra_latency_budget_ms,
                };
                assert_eq!(budgets.latency_ms, 500, "business deadline remains the default");
                let deadline = Instant::now() + Duration::from_millis(budgets.latency_ms);
                let scope = Arc::new(AccountingScope::new(
                    Arc::clone(&config),
                    Arc::clone(&key),
                    owner,
                    scope_id,
                    budgets,
                    deadline,
                ));
                let accounting = AuxiliaryAccounting::new(
                    selected,
                    (label.clone(), "prx-accounted".to_owned()),
                    Arc::clone(&provider),
                    key,
                    Arc::clone(&scope),
                    AuxiliaryPurpose::SignalVision,
                    Default::default(),
                )?;
                let messages = [ChatMessage::user("Explain 2 + 2.")];
                let result = crate::providers::attempt_accounting::chat_traced(
                    provider.as_ref(),
                    ChatRequest {
                        messages: &messages,
                        tools: None,
                    },
                    "prx-accounted",
                    0.0,
                    accounting.clone(),
                )
                .await?;
                assert_eq!(result.response.text_or_empty(), "PRX accounted response");
                if known {
                    assert_eq!(
                        result.attempts.len(),
                        if fallback { 1 } else { 2 },
                        "logical trace does not fabricate nested endpoint attempts"
                    );
                    assert_eq!(scope.remaining_budgets().tokens, 8_000 - 60);
                } else {
                    assert_eq!(
                        scope.remaining_budgets().tokens,
                        0,
                        "unknown usage retains the full allowance"
                    );
                    let denied = crate::providers::attempt_accounting::chat_traced(
                        provider.as_ref(),
                        ChatRequest {
                            messages: &messages,
                            tools: None,
                        },
                        "prx-accounted",
                        0.0,
                        accounting,
                    )
                    .await;
                    assert!(denied.is_err(), "unknown liability cannot fund another send");
                }
                let rows = journal(workspace.path())?;
                let reserved: BTreeSet<_> = rows
                    .iter()
                    .filter(|row| row["event"] == "estimated_reserved_v1")
                    .map(|row| {
                        row.pointer("/request/reservation/attempt_id")
                            .and_then(Value::as_str)
                            .expect("real attempt id")
                            .to_owned()
                    })
                    .collect();
                let dispatched: BTreeSet<_> = rows
                    .iter()
                    .filter(|row| row["event"] == "dispatched")
                    .map(|row| row["attempt_id"].as_str().expect("dispatched id").to_owned())
                    .collect();
                let settled: BTreeSet<_> = rows
                    .iter()
                    .filter(|row| row["event"] == "estimated_settled_v1")
                    .map(|row| row["attempt_id"].as_str().expect("settled id").to_owned())
                    .collect();
                assert_eq!(reserved, dispatched);
                assert!(
                    reserved.iter().all(|id| id.starts_with("signal_vision:")),
                    "every actual request retains its admitted auxiliary purpose"
                );
                let expected = if known { 2 } else { 1 };
                assert_eq!(reserved.len(), expected);
                assert_eq!(
                    listener.requests.lock().len(),
                    expected,
                    "journal IDs match actual physical requests"
                );
                let requests = listener.requests.lock().clone();
                assert!(requests.iter().all(|line| line.starts_with("POST ")));
                assert!(requests.first().is_some_and(|line| line.contains("/chat/completions ")));
                if fallback {
                    assert!(requests.last().is_some_and(|line| line.contains("/responses ")));
                } else {
                    assert!(requests.iter().all(|line| line.contains("/chat/completions ")));
                }
                if known {
                    assert_eq!(settled, dispatched);
                    assert!(
                        rows.iter()
                            .filter(|row| row["event"] == "estimated_settled_v1")
                            .all(|row| row["actual_tokens"] == "30")
                    );
                    let cost_rows = std::fs::read_to_string(workspace.path().join("state/costs.jsonl"))?;
                    assert_eq!(
                        cost_rows.lines().count(),
                        2,
                        "one real debit per physical attempt including status error"
                    );
                } else {
                    assert!(
                        settled.is_empty(),
                        "semantic success cannot fabricate complete reported usage"
                    );
                    let remaining =
                        crate::cost::tracker::remaining_micro_usd_read_only(&config.cost, workspace.path())?;
                    let held = rows
                        .iter()
                        .find(|row| row["event"] == "estimated_reserved_v1")
                        .and_then(|row| row.pointer("/request/reservation/maximum_micro_usd"))
                        .and_then(Value::as_u64)
                        .context("reservation amount missing")?;
                    assert!(held > 0);
                    assert_eq!(
                        remaining + held,
                        initial_money,
                        "read-only reopen retains exact monetary liability"
                    );
                }
                Ok(())
            })
            .await
    })
    .catch_unwind()
    .await;
    let listener_cleanup = listener.finish().await;
    let accounting_cleanup = tokio::time::timeout(Duration::from_secs(5), confirmed).await;
    if listener_cleanup.is_err() || !matches!(accounting_cleanup, Ok(Ok(Ok(())))) {
        let retained = workspace.keep();
        anyhow::bail!("fixture cleanup unconfirmed; retained {}", retained.display());
    }
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[tokio::test]
async fn real_retry_full_usage_has_distinct_durable_attempts_and_cost_rows() {
    run_case(Case::FullRetry).await.expect("real accounted retry");
}
#[tokio::test]
async fn real_success_partial_usage_keeps_hold_and_prevents_second_send() {
    run_case(Case::Partial).await.expect("real partial usage liability");
}
#[tokio::test]
async fn real_success_missing_usage_keeps_hold_and_prevents_second_send() {
    run_case(Case::Missing).await.expect("real missing usage liability");
}

#[tokio::test]
async fn real_nested_responses_fallback_accounts_both_physical_sends() {
    run_case(Case::FullResponsesFallback)
        .await
        .expect("actual nested endpoint fallback");
}

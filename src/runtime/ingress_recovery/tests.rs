#![cfg(test)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::print_stdout)]

use super::*;
use crate::agent::{agent::Agent, dispatcher::XmlToolDispatcher};
use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, TokenUsage, TokenUsageSource};
use crate::memory::SqliteMemory;
use crate::providers::traits::ChatTrace;
use crate::providers::{ChatRequest, ChatResponse, Provider};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const CHILD: &str = "runtime::ingress_recovery::tests::fresh_process_ingress_child";
const MODEL: &str = "prx-ingress-model";
const INPUT: &str = "memory_search: Explain the Rust expression `2 + 2`.";
const PROBE: &str = "  ";
const ACK: &str = "PRX_INGRESS_ACK:";

#[derive(Serialize, Deserialize)]
struct Report {
    pid: u32,
    calls: usize,
    key_sha256: String,
    store_workspace_id: String,
    events: Vec<MessageEvent>,
}

async fn report(memory: &Arc<dyn Memory>, workspace: &Path, calls: usize) -> Report {
    let workspace_id = workspace.to_string_lossy().into_owned();
    let owner_id = format!("owner:{workspace_id}:agent_api:embedded-caller");
    let event_ids = {
        let conn = rusqlite::Connection::open_with_flags(
            workspace.join("memory/brain.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut statement = conn
            .prepare("SELECT event_id FROM message_events WHERE workspace_id=?1 AND owner_id=?2 ORDER BY id LIMIT 129")
            .unwrap();
        let rows = statement
            .query_map(rusqlite::params![workspace_id, owner_id], |row| row.get::<_, String>(0))
            .unwrap();
        let ids = rows.collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert!(ids.len() <= 128, "fixture must not truncate event evidence");
        ids
    }; // Close the read-only observer connection before awaiting the actual repository readback.
    assert!(!event_ids.is_empty());
    let mut events = Vec::new();
    for event_id in event_ids {
        let event = memory
            .find_owned_message_event(&workspace_id, &owner_id, &event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.workspace_id, workspace_id);
        assert_eq!(event.owner_id.as_deref(), Some(owner_id.as_str()));
        events.push(event);
    }
    events.sort_by_key(|row| row.id);
    assert!(
        events
            .windows(2)
            .all(|pair| matches!(pair, [left, right] if left.id < right.id)),
        "duplicate durable row IDs in evidence"
    );
    let mut event_ids = std::collections::BTreeSet::new();
    assert!(
        events.iter().all(|row| event_ids.insert(row.event_id.as_str())),
        "duplicate durable event identities in evidence"
    );
    Report {
        pid: std::process::id(),
        calls,
        store_workspace_id: crate::causal_tree::store::CausalDecisionStore::open_unshared_read_only(workspace)
            .workspace_id()
            .expect("actual decision Store UUID must already exist")
            .to_string(),
        key_sha256: hex::encode(Sha256::digest(
            std::fs::read(workspace.join("self/causal_tree/.decision_key")).unwrap(),
        )),
        events,
    }
}

fn acknowledge(report: &Report) {
    println!("{ACK}{}", serde_json::to_string(report).unwrap());
    std::io::stdout().flush().unwrap();
}

struct RecordedProvider {
    memory: Arc<dyn Memory>,
    workspace: PathBuf,
    calls: Arc<AtomicUsize>,
    mode: String,
}

#[async_trait::async_trait]
impl Provider for RecordedProvider {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("fixture requires the actual traced provider dispatch")
    }

    async fn chat_traced(&self, _: ChatRequest<'_>, model: &str, _: f64) -> anyhow::Result<ChatTrace> {
        assert_eq!(model, MODEL);
        let calls = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        assert_ne!(
            self.mode, "probe",
            "fresh probe must close without another provider dispatch"
        );
        assert_eq!(calls, 1, "a source may not be resent");
        if self.mode == "pending" {
            acknowledge(&report(&self.memory, &self.workspace, calls).await);
            // Parent kills only this owned process after the durable-source/provider-entry acknowledgement.
            // No fabricated provider completion, token usage or terminal is returned.
            std::future::pending::<()>().await;
        }
        let now = chrono::Utc::now();
        Ok(ChatTrace {
            response: ChatResponse {
                text: Some("The expression adds two integers.".into()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
            attempts: vec![ProviderAttempt {
                seq: 1,
                provider: "ollama".into(),
                model: model.into(),
                started_at: now,
                finished_at: now,
                status: AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "ollama".into(),
            final_model: model.into(),
            served_member: None,
            tokens_used: TokenUsage {
                prompt_tokens: Some(17),
                completion_tokens: Some(5),
                total_tokens: Some(22),
                source: TokenUsageSource::Reported,
                ..TokenUsage::default()
            },
        })
    }
}

fn turn<'a>(
    agent: &'a mut Agent,
    input: &'a str,
) -> std::pin::Pin<Box<impl std::future::Future<Output = anyhow::Result<String>> + 'a>> {
    Box::pin(agent.turn(input))
}

#[tokio::test]
#[ignore = "child mode is invoked only by the owned fresh-process ingress tests"]
async fn fresh_process_ingress_child() {
    let workspace = PathBuf::from(std::env::var_os("PRX_INGRESS_TEST_WORKSPACE").expect("owned child workspace"));
    let mode = std::env::var("PRX_INGRESS_TEST_MODE").expect("owned child mode");
    assert!(matches!(mode.as_str(), "completed" | "pending" | "probe"));
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace).unwrap());
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.clone();
    config.config_path = workspace.join("config.toml");
    config.default_model = Some(MODEL.into());
    config.causal_tree.enabled = true;
    config.causal_tree.policy.max_decision_hops = 1;
    config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
    config.providers.models.push(
        toml::from_str(&format!(
            "provider = \"ollama\"\nmodel = \"{MODEL}\"\noperations = [\"chat\"]\nnative_tools = true\n"
        ))
        .unwrap(),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent::builder()
        .runtime_config(config, "ollama")
        .provider(Box::new(RecordedProvider {
            memory: Arc::clone(&memory),
            workspace: workspace.clone(),
            calls: Arc::clone(&calls),
            mode: mode.clone(),
        }))
        .tools(vec![Box::new(crate::tools::memory_search::MemorySearchTool::new(
            workspace.clone(),
            Arc::clone(&memory),
            true,
        ))])
        .memory(Arc::clone(&memory))
        .observer(Arc::new(crate::observability::NoopObserver {}))
        .tool_dispatcher(Box::new(XmlToolDispatcher))
        .build()
        .unwrap();
    if mode != "probe" {
        agent.seed_scoped_query_memory_for_test(INPUT).await;
    }
    let result = turn(&mut agent, if mode == "probe" { PROBE } else { INPUT }).await;
    assert!(result.is_ok(), "actual ingress failed: {result:?}");
    assert_eq!(calls.load(Ordering::SeqCst), usize::from(mode == "completed"));
    drop(agent);
    acknowledge(&report(&memory, &workspace, calls.load(Ordering::SeqCst)).await);
}

struct OwnedChild {
    child: Child,
    ack: std::sync::mpsc::Receiver<Report>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl OwnedChild {
    fn start(workspace: &Path, mode: &str) -> Self {
        let stderr = std::fs::File::create(workspace.join(format!("{mode}.stderr"))).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([CHILD, "--exact", "--ignored", "--nocapture", "--test-threads=1"])
            .env("PRX_INGRESS_TEST_WORKSPACE", workspace)
            .env("PRX_INGRESS_TEST_MODE", mode)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr))
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sent, ack) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut consumed = 0;
            let mut sent = Some(sent);
            let bounded = std::io::Read::take(stdout, 256 * 1024 + 1);
            for line in std::io::BufReader::new(bounded).lines() {
                let line = line.unwrap();
                consumed += line.len();
                assert!(consumed <= 256 * 1024, "bounded child diagnostic output");
                if let Some((_, json)) = line.split_once(ACK) {
                    sent.take()
                        .expect("one acknowledgement per child")
                        .send(serde_json::from_str(json).unwrap())
                        .unwrap();
                }
            }
        });
        Self {
            child,
            ack,
            reader: Some(reader),
        }
    }

    fn acknowledgement(&self) -> Report {
        self.ack
            .recv_timeout(Duration::from_mins(1))
            .expect("actual child phase acknowledgement")
    }

    fn finish(&mut self, killed: bool) {
        if killed {
            self.child.kill().unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert_eq!(status.success(), !killed, "owned child exit: {status}");
                break;
            }
            assert!(Instant::now() < deadline, "owned child did not terminate");
            // Bounded exit watchdog only; neither crash nor recovery phase is selected by elapsed time.
            std::thread::park_timeout(Duration::from_millis(10));
        }
        self.reader.take().unwrap().join().unwrap();
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn source<'a>(report: &'a Report, input: &str) -> &'a MessageEvent {
    let sources: Vec<_> = report
        .events
        .iter()
        .filter(|row| {
            row.role == "user"
                && row.content == input
                && row
                    .raw_payload_json
                    .as_deref()
                    .is_some_and(|raw| raw.contains("prx_ingress_capsule"))
        })
        .collect();
    assert_eq!(sources.len(), 1);
    sources.into_iter().next().unwrap()
}

fn final_for<'a>(report: &'a Report, event: &MessageEvent) -> Option<&'a MessageEvent> {
    let key = format!("turn:{}:final", event.run_id.as_ref().unwrap());
    report
        .events
        .iter()
        .find(|row| row.idempotency_key.as_deref() == Some(&key))
}

fn verify_recovery(completed: bool) {
    let workspace = tempfile::tempdir().unwrap().keep();
    println!("retained ingress recovery fixture: {}", workspace.display());
    let mut first = OwnedChild::start(workspace.as_path(), if completed { "completed" } else { "pending" });
    let before = first.acknowledgement();
    assert_eq!(before.pid, first.child.id());
    assert_eq!(before.calls, 1);
    let original = source(&before, INPUT);
    let before_final = final_for(&before, original);
    assert_eq!(before_final.is_some(), completed);
    first.finish(!completed);
    let mut next = OwnedChild::start(workspace.as_path(), "probe");
    let after = next.acknowledgement();
    assert_eq!(after.pid, next.child.id());
    next.finish(false);
    assert_ne!(before.pid, after.pid);
    assert_eq!(after.calls, 0, "no historical provider call may be resent");
    assert_eq!(before.key_sha256, after.key_sha256);
    assert_eq!(before.store_workspace_id, after.store_workspace_id);
    let restored = source(&after, INPUT);
    assert_eq!(
        serde_json::to_value(original).unwrap(),
        serde_json::to_value(restored).unwrap()
    );
    let probe = source(&after, PROBE);
    assert_ne!(
        original.session_key, probe.session_key,
        "new instance is a new session, not restored live authority"
    );
    assert_ne!(original.run_id, probe.run_id);
    assert_eq!(original.owner_id, probe.owner_id);
    let old_capsule = capsule_from_event(original).unwrap();
    let new_capsule = capsule_from_event(probe).unwrap();
    assert_eq!(old_capsule.backend_namespace, new_capsule.backend_namespace);
    let probe_final = final_for(&after, probe).expect("actual fixed-close probe terminal");
    let probe_payload: serde_json::Value =
        serde_json::from_str(probe_final.raw_payload_json.as_ref().unwrap()).unwrap();
    assert_eq!(
        probe_payload
            .pointer("/terminal_replay_v1/fixed_close")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert!(
        probe_payload
            .get("provider_outcome")
            .is_none_or(serde_json::Value::is_null)
    );
    assert_eq!(
        probe_payload
            .pointer("/terminal_replay_v1/store_workspace_id")
            .and_then(serde_json::Value::as_str),
        Some(after.store_workspace_id.as_str())
    );
    let state = if completed { "reconciled" } else { "unconfirmed" };
    let recovery_key = format!("ingress:{}:{state}", original.event_id);
    let evidence: Vec<_> = after
        .events
        .iter()
        .filter(|row| row.idempotency_key.as_deref() == Some(&recovery_key))
        .collect();
    assert_eq!(evidence.len(), 1);
    let evidence = evidence.into_iter().next().unwrap();
    assert_eq!(evidence.causation_event_id.as_deref(), Some(original.event_id.as_str()));
    assert_eq!(evidence.run_id, original.run_id);
    assert_eq!(evidence.owner_id, original.owner_id);
    assert_eq!(evidence.session_key, original.session_key);
    let payload: serde_json::Value = serde_json::from_str(evidence.raw_payload_json.as_ref().unwrap()).unwrap();
    assert_eq!(
        payload
            .pointer("/ingress_recovery/state")
            .and_then(serde_json::Value::as_str),
        Some(state)
    );
    assert_eq!(
        payload.pointer("/ingress_recovery/capsule").expect("recovery capsule"),
        &serde_json::to_value(&old_capsule).unwrap()
    );
    if let Some(before_final) = before_final {
        let old_final = final_for(&after, original).unwrap();
        assert_eq!(
            serde_json::to_value(before_final).unwrap(),
            serde_json::to_value(old_final).unwrap()
        );
        let old: serde_json::Value = serde_json::from_str(old_final.raw_payload_json.as_ref().unwrap()).unwrap();
        let probe_final = final_for(&after, probe).unwrap();
        let new: serde_json::Value = serde_json::from_str(probe_final.raw_payload_json.as_ref().unwrap()).unwrap();
        assert!(
            old.get("terminal_replay_mac_v1")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|tag| !tag.is_empty())
        );
        assert_eq!(
            old.pointer("/provider_outcome/tokens_used/prompt_tokens")
                .and_then(serde_json::Value::as_u64),
            Some(17)
        );
        assert_eq!(
            old.pointer("/provider_outcome/tokens_used/completion_tokens")
                .and_then(serde_json::Value::as_u64),
            Some(5)
        );
        assert_eq!(
            old.pointer("/provider_outcome/tokens_used/total_tokens")
                .and_then(serde_json::Value::as_u64),
            Some(22)
        );
        let store = old
            .pointer("/terminal_replay_v1/store_workspace_id")
            .and_then(serde_json::Value::as_str)
            .expect("original store namespace");
        assert!(!store.is_empty());
        assert_eq!(
            new.pointer("/terminal_replay_v1/store_workspace_id")
                .and_then(serde_json::Value::as_str),
            Some(store)
        );
    } else {
        assert!(
            final_for(&after, original).is_none(),
            "missing completion stays unknown"
        );
        assert!(evidence.content.contains("usage remain unconfirmed"));
    }
}

#[test]
fn fresh_process_rejoins_original_signed_final_without_restoring_session_or_resending() {
    verify_recovery(true);
}

#[test]
fn killed_process_input_stays_unconfirmed_without_provider_resend_or_fake_terminal() {
    verify_recovery(false);
}

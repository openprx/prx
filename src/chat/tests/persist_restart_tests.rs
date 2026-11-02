#![cfg(test)]

//! Restart regressions use the real SQLite memory and decision stores.
use super::*;
use crate::chat::decision_source::{ChatAdmissionMode, ChatProviderBinding};
use crate::memory::{MessageEvent, MessageEventInput, SqliteMemory};
use crate::runtime::turn_cleanup::{CleanupScope, CleanupTracker};
use anyhow::Context;
use std::path::Path;

fn config_at(workspace: &Path) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_owned();
    config.config_path = workspace.join("config.toml");
    config.default_provider = Some("ollama".into());
    config.default_model = Some("prx-local-model".into());
    config.causal_tree.enabled = true;
    config.causal_tree.policy.max_decision_hops = 1;
    config
}

async fn publish(
    memory: Arc<dyn Memory>,
    generation: Arc<crate::config::ConfigGeneration>,
) -> Result<Arc<crate::runtime::turn_decision::TurnDecisionService>> {
    crate::runtime::turn_publication::TurnDecisionPublication {
        security: Arc::new(crate::security::SecurityPolicy::from_config(
            &generation.effective.autonomy,
            &generation.effective.workspace_dir,
        )),
        hooks: Arc::new(crate::hooks::HookManager::new(
            generation.effective.workspace_dir.clone(),
        )),
        generation,
        registry: Arc::new(Vec::new()),
        mcp_tool: None,
        observer: Arc::new(crate::observability::noop::NoopObserver),
        memory,
        plugin_generation: 0,
    }
    .publish(crate::causal_tree::state::TurnSurfaceKind::Interactive)
    .await
}

async fn guarded_input(
    memory: Arc<dyn Memory>,
    generation: &Arc<crate::config::ConfigGeneration>,
    run: &str,
) -> Result<(
    MessageEvent,
    Arc<crate::chat::decision_source::ChatTurnBinding>,
    crate::chat::decision_source::ChatAdmissionGuard,
)> {
    let provider: Arc<dyn Provider> = Arc::new(crate::providers::ollama::OllamaProvider::new(None, None));
    let selected = ChatProviderBinding::capture(generation, &provider, "ollama", "prx-local-model")?
        .context("selected provider")?;
    let fabric = MemoryFabric::new(memory, generation.effective.workspace_dir.to_string_lossy())
        .with_event_recording(generation.effective.memory.event_recording_config());
    let envelope = chat_runtime_envelope(fabric.workspace_id(), "prx-restart-session")
        .with_config_generation(generation)
        .with_run_id(run);
    let owner = crate::runtime::turn_cleanup::current_tracker().unwrap_or_else(|| Arc::new(CleanupTracker::default()));
    let (source, binding, guard) = selected
        .record_guarded_with_origin(
            fabric,
            (
                envelope.message_scope(),
                "prx-restart-session",
                ChatAdmissionMode::Model(crate::agent::turn_contracts::ChatMode::Edit),
                false,
            ),
            "  ",
            format!("chat:{run}:input"),
            owner.register(CancellationToken::new())?,
            None,
        )
        .await?;
    Ok((source.context("durable source")?, binding, guard))
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    FloatDrift,
    MissingSource,
}

async fn completed_chat(workspace: &Path, fault: Fault) -> MessageEvent {
    let cleanup = CleanupScope::new(None, CancellationToken::new()).unwrap();
    cleanup
        .scope_admission(async {
            let memory = Arc::new(SqliteMemory::new(workspace).unwrap());
            let generation = crate::config::new_shared(config_at(workspace)).pin();
            let (source, binding, guard) = guarded_input(memory.clone(), &generation, "prx-first-run")
                .await
                .unwrap();
            let service = publish(memory.clone(), generation).await.unwrap();
            let cancel = CancellationToken::new();
            binding.decide(&service, &cancel).await.unwrap();
            assert!(binding.fixed_reply().unwrap().is_some());
            let fabric = MemoryFabric::new(memory.clone(), workspace.to_string_lossy());
            let receipt = binding.close(&fabric, 2, chrono::Utc::now(), &cancel).await.unwrap();
            if !matches!(fault, Fault::None) {
                let key = crate::decision::DecisionKey::load_or_create(workspace).unwrap();
                let mut payload: serde_json::Value =
                    serde_json::from_str(receipt.terminal_event.raw_payload_json.as_deref().unwrap()).unwrap();
                let cost = 0.002_917_600_000_000_000_2_f64;
                if matches!(fault, Fault::MissingSource) {
                    *payload.pointer_mut("/terminal_replay_v1/source/event_id").unwrap() = "prx-missing-source".into();
                } else {
                    payload
                        .as_object_mut()
                        .unwrap()
                        .insert("usage_settlement".into(), serde_json::json!({"cost_usd": cost}));
                }
                crate::agent::terminal::reseal_payload_for_test(&key, &mut payload, receipt.assistant_event.as_ref())
                    .unwrap();
                // The old parser changed this cost by one ULP after sealing. Preserve
                // that historical reader output beside the original real seal.
                if matches!(fault, Fault::FloatDrift) {
                    *payload.pointer_mut("/usage_settlement/cost_usd").unwrap() =
                        serde_json::json!(f64::from_bits(cost.to_bits() + 1));
                }
                memory
                    .replace_event_payload_for_test(&receipt.terminal_event.event_id, &payload.to_string())
                    .unwrap();
                let error = service
                    .rejoin_terminal("prx-first-run", source.owner_id.as_deref().unwrap())
                    .await
                    .unwrap_err();
                let expected = if matches!(fault, Fault::FloatDrift) {
                    "terminal replay authentication failed"
                } else {
                    "original terminal source event is missing"
                };
                assert!(format!("{error:#}").contains(expected), "{error:#}");
            }
            guard.handoff();
            service
                .shutdown(std::time::Instant::now() + Duration::from_secs(5))
                .await;
            Ok(source)
        })
        .await
        .unwrap()
}

async fn restarted_chat(workspace: &Path, run: &str) -> MessageEvent {
    let cleanup = CleanupScope::new(None, CancellationToken::new()).unwrap();
    cleanup
        .scope_admission(async {
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace).unwrap());
            let generation = crate::config::new_shared(config_at(workspace)).pin();
            let (source, _binding, guard) = guarded_input(memory.clone(), &generation, run).await.unwrap();
            let service = publish(memory, generation).await.unwrap();
            guard.handoff();
            service
                .shutdown(std::time::Instant::now() + Duration::from_secs(5))
                .await;
            Ok(source)
        })
        .await
        .unwrap()
}

async fn marker(workspace: &Path, source: &MessageEvent) -> MessageEvent {
    SqliteMemory::new(workspace)
        .unwrap()
        .find_owned_message_event_by_idempotency_key(
            &source.workspace_id,
            source.owner_id.as_deref().unwrap(),
            &format!("ingress:{}:quarantine", source.event_id),
        )
        .await
        .unwrap()
        .expect("quarantine evidence")
}

#[tokio::test]
async fn chat_recovers_a_real_seal_that_failed_verification_before_restart() {
    let dir = tempfile::tempdir().unwrap();
    let first = completed_chat(dir.path(), Fault::FloatDrift).await;
    let next = restarted_chat(dir.path(), "prx-next-run").await;
    assert!(next.id > first.id);
    let evidence = marker(dir.path(), &first).await;
    assert!(evidence.content.contains("terminal replay authentication failed"));
    let mut payload: serde_json::Value = serde_json::from_str(evidence.raw_payload_json.as_deref().unwrap()).unwrap();
    let authentication = payload.as_object_mut().unwrap().remove("authentication").unwrap();
    let key = crate::decision::DecisionKey::load_or_create(dir.path()).unwrap();
    assert!(key.verify_hmac(
        crate::decision::KeyDomain::IngressRecovery,
        &[b"quarantine-v1", &serde_json::to_vec(&payload).unwrap()],
        &hex::decode(authentication.as_str().unwrap()).unwrap()
    ));
    restarted_chat(dir.path(), "prx-third-run").await;
    assert_eq!(marker(dir.path(), &first).await.event_id, evidence.event_id);
}

#[tokio::test(flavor = "current_thread")]
async fn empty_decision_store_beside_populated_memory_recovers_with_named_diagnostics() {
    // Other chat tests configure process-wide tracing. Isolate the subscriber
    // and its callsite cache while retaining the real restart and log assertions.
    const CHILD_FLAG: &str = "PRX_TEST_INGRESS_DIAGNOSTICS_CHILD";
    const TEST: &str = "chat::tests::persist_restart_tests::empty_decision_store_beside_populated_memory_recovers_with_named_diagnostics";
    if std::env::var_os(CHILD_FLAG).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([TEST, "--exact", "--nocapture"])
            .env(CHILD_FLAG, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let first = completed_chat(dir.path(), Fault::None).await;
    let decision_dir = dir.path().join("self/causal_tree");
    let saved = dir.path().join("previous-decisions");
    std::fs::create_dir(&saved).unwrap();
    for name in ["decisions.db", "decisions.db-wal", "decisions.db-shm"] {
        let file = decision_dir.join(name);
        if file.exists() {
            std::fs::rename(&file, saved.join(name)).unwrap();
        }
    }
    let log = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(CapturedLog(log.clone()))
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);
    restarted_chat(dir.path(), "prx-after-reset").await;
    let evidence = marker(dir.path(), &first).await;
    assert!(evidence.content.contains("binding does not match the committed marker"));
    let text = String::from_utf8(log.lock().clone()).unwrap();
    for expected in [
        "brain.db",
        "decisions.db",
        "prx-first-run",
        "quarantined",
        "startup recovery will continue",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
}

#[derive(Clone)]
struct CapturedLog(Arc<parking_lot::Mutex<Vec<u8>>>);
impl std::io::Write for CapturedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

#[tokio::test]
async fn forged_quarantine_marker_cannot_suppress_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let first = completed_chat(dir.path(), Fault::None).await;
    let memory = SqliteMemory::new(dir.path()).unwrap();
    let mut forged: MessageEventInput = serde_json::from_value(serde_json::to_value(&first).unwrap()).unwrap();
    forged.event_id = None;
    forged.role = "event".into();
    forged.event_type = "turn.ingress.quarantined".into();
    forged.idempotency_key = Some(format!("ingress:{}:quarantine", first.event_id));
    forged.causation_event_id = Some(first.event_id.clone());
    forged.content = "turn.ingress.quarantined forged marker".into();
    forged.raw_payload_json =
        Some(serde_json::json!({"capsule": serde_json::from_str::<serde_json::Value>(first.raw_payload_json.as_deref().unwrap()).unwrap().get("prx_ingress_capsule").unwrap(), "reason": "forged", "authentication": "00".repeat(32)}).to_string());
    memory.append_message_event(forged).await.unwrap();
    drop(memory);
    restarted_chat(dir.path(), "prx-after-forgery").await;
    let memory = SqliteMemory::new(dir.path()).unwrap();
    assert!(
        memory
            .find_owned_message_event_by_idempotency_key(
                &first.workspace_id,
                first.owner_id.as_deref().unwrap(),
                &format!("ingress:{}:reconciled", first.event_id)
            )
            .await
            .unwrap()
            .is_some(),
        "a forged marker must not hide genuine terminal recovery"
    );
}

#[tokio::test]
async fn guarded_inbound_failure_returns_store_cause_and_recovery_hint() {
    for enabled in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(SqliteMemory::new(dir.path()).unwrap());
        memory.reject_user_events_for_test().unwrap();
        let mut config = config_at(dir.path());
        config.causal_tree.enabled = enabled;
        let generation = crate::config::new_shared(config).pin();
        let result = guarded_input(memory, &generation, "prx-rejected-run").await;
        let text = match result {
            Err(error) => error.to_string(),
            Ok(_) => panic!("failed inbound write was accepted"),
        };
        for expected in [
            "chat input persistence failed",
            "brain.db",
            "inbound write rejected by store",
            "Restart to retry ingress recovery",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }
}

#[tokio::test]
async fn missing_terminal_source_is_quarantined_on_restart() {
    let dir = tempfile::tempdir().unwrap();
    let first = completed_chat(dir.path(), Fault::MissingSource).await;
    restarted_chat(dir.path(), "prx-after-missing-source").await;
    assert!(
        marker(dir.path(), &first)
            .await
            .content
            .contains("original terminal source event is missing")
    );
}

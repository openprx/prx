#![cfg(test)]

//! Restart regressions use the real SQLite memory and decision stores.
use super::*;
use crate::chat::decision_source::{ChatAdmissionMode, ChatProviderBinding};
use crate::memory::ingress_recovery::IngressCapsule;
use crate::memory::recovery_authority::{RecoveryMacDomain, RecoveryTag};
use crate::memory::{MessageEvent, MessageEventInput, SqliteMemory};
use crate::runtime::turn_cleanup::{CleanupScope, CleanupTracker};
use anyhow::Context;
use std::path::Path;

/// v3 expectation: the MAC of a real v3 capsule's artifact is produced by the
/// repository key named in that capsule, never by the legacy decision key.
fn repository_tag(workspace: &Path, capsule: &IngressCapsule, domain: RecoveryMacDomain, bytes: &[u8]) -> RecoveryTag {
    let store = SqliteMemory::new(workspace).expect("agent store");
    let repository = store.recovery_repository().expect("repository-backed store");
    repository
        .recovery_mac(capsule.key_id.as_deref(), domain, bytes)
        .expect("repository MAC for the capsule's key")
}

/// v3 verification of an authentication tag recorded by a real capsule's artifact.
fn repository_verifies(
    workspace: &Path,
    capsule: &IngressCapsule,
    domain: RecoveryMacDomain,
    bytes: &[u8],
    tag: &[u8],
) -> bool {
    let store = SqliteMemory::new(workspace).expect("agent store");
    let repository = store.recovery_repository().expect("repository-backed store");
    let tag = RecoveryTag {
        key_id: capsule.key_id.clone().expect("v3 capsule key id"),
        tag: tag.try_into().expect("32-byte recovery tag"),
    };
    repository.verify_recovery_mac(&tag, domain, bytes).is_ok()
}

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
    guarded_input_with(provider, memory, generation, run).await
}

async fn guarded_input_with(
    provider: Arc<dyn Provider>,
    memory: Arc<dyn Memory>,
    generation: &Arc<crate::config::ConfigGeneration>,
    run: &str,
) -> Result<(
    MessageEvent,
    Arc<crate::chat::decision_source::ChatTurnBinding>,
    crate::chat::decision_source::ChatAdmissionGuard,
)> {
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

/// Counts every provider dispatch; admission must never reach one.
#[derive(Default)]
struct DispatchCounter(std::sync::atomic::AtomicUsize);

struct CountingProvider(Arc<DispatchCounter>);

#[async_trait::async_trait]
impl Provider for CountingProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> Result<String> {
        self.0.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        anyhow::bail!("dispatch fixture")
    }
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    FloatDrift,
    MissingSource,
    /// A pre-journal marker: sealed without `journal_version`, so replay selects the store.
    LegacySelection,
}

async fn completed_chat(workspace: &Path, fault: Fault) -> MessageEvent {
    let cleanup = CleanupScope::new(None, CancellationToken::new()).unwrap();
    let lifetime_owner = cleanup
        .scope_admission(async {
            Ok(Arc::downgrade(
                &crate::runtime::turn_cleanup::current_tracker().expect("test: cleanup owner installed"),
            ))
        })
        .await
        .unwrap();
    let source = completed_chat_under_cleanup(workspace, fault, &cleanup).await;
    cleanup
        .run(async { Ok(()) })
        .await
        .expect("test: drain completed ingress lifetime before restart");
    assert!(
        lifetime_owner.upgrade().is_none(),
        "test: completed ingress lifetime owner must be released before restart"
    );
    source
}

/// Journal-backed close does not await the auxiliary decision-store queue.
/// A fixture that deliberately selects legacy store replay needs its own
/// durability acknowledgement before asserting that replay can verify it.
async fn drain_legacy_fixture_decisions(service: &crate::runtime::turn_decision::TurnDecisionService) {
    assert!(
        service
            .drain_writes(std::time::Instant::now() + Duration::from_secs(5))
            .await,
        "fixture decision writes must be durable before legacy replay"
    );
}

async fn completed_chat_under_cleanup(workspace: &Path, fault: Fault, cleanup: &CleanupScope) -> MessageEvent {
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
                if matches!(fault, Fault::LegacySelection) {
                    assert!(
                        payload
                            .pointer_mut("/terminal_replay_v1")
                            .unwrap()
                            .as_object_mut()
                            .unwrap()
                            .remove("journal_version")
                            .is_some(),
                        "test: capture must have sealed a journal-required marker"
                    );
                } else if matches!(fault, Fault::MissingSource) {
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
                if matches!(fault, Fault::LegacySelection) {
                    drain_legacy_fixture_decisions(&service).await;
                }
                let rejoin = service
                    .recovery_scope()
                    .rejoin_terminal("prx-first-run", source.owner_id.as_deref().unwrap())
                    .await;
                if matches!(fault, Fault::LegacySelection) {
                    // The legacy store arm still verifies while its store is intact.
                    rejoin.expect("legacy-selected marker rejoins against its own store");
                    guard.handoff();
                    service
                        .shutdown(std::time::Instant::now() + Duration::from_secs(5))
                        .await;
                    return Ok(source);
                }
                let error = rejoin.unwrap_err();
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
        .run(async {
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

fn run_in_isolated_process(test: &str, child_flag: &str) -> bool {
    if std::env::var_os(child_flag).is_some() {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture"])
        .env(child_flag, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

#[tokio::test(flavor = "current_thread")]
async fn chat_drained_service_releases_ingress_lease_before_restart() {
    let dir = tempfile::tempdir().unwrap();
    let cleanup = CleanupScope::new(None, CancellationToken::new()).unwrap();
    let (first, retained_sink, lease_token) = cleanup
        .scope_admission(async {
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).unwrap());
            let generation = crate::config::new_shared(config_at(dir.path())).pin();
            let (source, binding, guard) = guarded_input(memory.clone(), &generation, "prx-drained-run").await?;
            let lease = memory
                .acquire_ingress_lifetime(
                    &source.workspace_id,
                    &uuid::Uuid::new_v4().to_string(),
                    std::time::Instant::now() + Duration::from_secs(5),
                )
                .await?;
            let token = lease.token().to_owned();
            // Model the shared decision writer retaining an idle sink after
            // the service exits, independently of its thread's scheduling.
            let retained_sink = crate::runtime::causal_events::DurableCausalEventSink::new(
                &generation,
                memory.clone(),
                Arc::new(crate::decision::telemetry::ObserverCausalSink::new(Arc::new(
                    crate::observability::noop::NoopObserver,
                ))),
            )?;
            let service = publish(memory.clone(), generation).await?;
            let cancel = CancellationToken::new();
            binding.decide(&service, &cancel).await?;
            let fabric = MemoryFabric::new(memory, dir.path().to_string_lossy());
            binding.close(&fabric, 2, chrono::Utc::now(), &cancel).await?;
            guard.handoff();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            assert!(service.drain_writes(deadline).await, "first service writes must drain");
            assert!(retained_sink.drain(deadline).await, "retained sink must drain");
            Ok((source, retained_sink, token))
        })
        .await
        .unwrap();
    cleanup.run(async { Ok(()) }).await.unwrap();
    let probe = SqliteMemory::new(dir.path()).unwrap();
    let claim = probe
        .claim_ingress_recovery(
            &first.workspace_id,
            &lease_token,
            std::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        claim.is_some(),
        "drained service and retained idle sink must release the ingress lease"
    );
    drop(claim);
    let next = restarted_chat(dir.path(), "prx-after-drain").await;
    assert!(next.id > first.id);
    drop(retained_sink);
}

#[tokio::test]
async fn retry_after_active_key_loss_is_refused_at_admission() {
    let dir = tempfile::tempdir().unwrap();
    let _first = completed_chat(dir.path(), Fault::None).await;
    // Corrupt the active recovery key: preparation now yields no recorder.
    let key_id = SqliteMemory::new(dir.path())
        .unwrap()
        .recovery_repository()
        .expect("repository-backed store")
        .activate_recovery_authority()
        .unwrap();
    std::fs::write(
        dir.path()
            .join("memory")
            .join(".recovery_keys")
            .join(format!("{key_id}.key")),
        [0u8; 32],
    )
    .unwrap();
    let generation = crate::config::new_shared(config_at(dir.path())).pin();
    let counter = Arc::new(DispatchCounter::default());
    let provider: Arc<dyn Provider> = Arc::new(CountingProvider(Arc::clone(&counter)));
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).unwrap());
    let cleanup = CleanupScope::new(None, CancellationToken::new()).unwrap();
    // Same run, generation, scope and input as the accepted turn.
    let refused = cleanup
        .scope_admission(async move {
            Ok(guarded_input_with(provider, memory, &generation, "prx-first-run")
                .await
                .err())
        })
        .await
        .unwrap();
    assert!(
        refused
            .as_ref()
            .is_some_and(|error| error.is::<crate::runtime::ingress_recovery::AlreadyAccepted>()),
        "retry of an accepted input was admitted: {refused:?}"
    );
    assert_eq!(
        counter.0.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "dispatch reached"
    );
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
    let capsule: IngressCapsule = serde_json::from_value(payload.get("capsule").cloned().unwrap()).unwrap();
    assert!(repository_verifies(
        dir.path(),
        &capsule,
        RecoveryMacDomain::QuarantineV3,
        &serde_json::to_vec(&payload).unwrap(),
        &hex::decode(authentication.as_str().unwrap()).unwrap()
    ));
    restarted_chat(dir.path(), "prx-third-run").await;
    assert_eq!(marker(dir.path(), &first).await.event_id, evidence.event_id);
}

/// Which evidence authority the first run's terminal marker selects.
#[derive(Clone, Copy)]
enum StoreReset {
    /// Journal-required marker: the sealed journal is authoritative, so the reset store is not consulted.
    JournalAuthority,
    /// Pre-journal marker: the decision store is authoritative, so the reset is unverifiable.
    LegacyStore,
}

#[tokio::test(flavor = "current_thread")]
async fn empty_decision_store_beside_populated_memory_recovers_with_named_diagnostics() {
    // Other chat tests configure process-wide tracing. Isolate the subscriber
    // and its callsite cache while retaining the real restart and log assertions.
    const CHILD_FLAG: &str = "PRX_TEST_INGRESS_DIAGNOSTICS_CHILD";
    const TEST: &str = "chat::tests::persist_restart_tests::empty_decision_store_beside_populated_memory_recovers_with_named_diagnostics";
    if run_in_isolated_process(TEST, CHILD_FLAG) {
        return;
    }
    // The in-process arm is the legacy one: it must keep its named quarantine diagnostics.
    // The journal arm runs in its own isolated child process below.
    reset_store_and_restart(StoreReset::LegacyStore).await;
}

#[tokio::test(flavor = "current_thread")]
async fn empty_decision_store_beside_journal_authority_terminal_recovers_without_quarantine() {
    const CHILD_FLAG: &str = "PRX_TEST_INGRESS_JOURNAL_DIAGNOSTICS_CHILD";
    const TEST: &str = "chat::tests::persist_restart_tests::empty_decision_store_beside_journal_authority_terminal_recovers_without_quarantine";
    if run_in_isolated_process(TEST, CHILD_FLAG) {
        return;
    }
    reset_store_and_restart(StoreReset::JournalAuthority).await;
}

async fn reset_store_and_restart(arm: StoreReset) {
    let dir = tempfile::tempdir().unwrap();
    let first = completed_chat(
        dir.path(),
        match arm {
            StoreReset::JournalAuthority => Fault::None,
            StoreReset::LegacyStore => Fault::LegacySelection,
        },
    )
    .await;
    let decision_dir = dir.path().join("self/causal_tree");
    let saved = dir.path().join("previous-decisions");
    std::fs::create_dir(&saved).unwrap();
    let main_db = decision_dir.join("decisions.db");
    if main_db.exists() {
        std::fs::rename(&main_db, saved.join("decisions.db")).unwrap();
    }
    // Only the -wal/-shm sidecars tolerate NotFound: SQLite deletes a sidecar
    // when the last connection closes, so it can vanish between a probe and
    // the rename. No probe is made for them. The main db has no such tolerance.
    for name in ["decisions.db-wal", "decisions.db-shm"] {
        match std::fs::rename(decision_dir.join(name), saved.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("move sidecar {name} aside before the store reset: {error}"),
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
    let text = String::from_utf8(log.lock().clone()).unwrap();
    match arm {
        StoreReset::LegacyStore => {
            let evidence = marker(dir.path(), &first).await;
            assert!(evidence.content.contains("binding does not match the committed marker"));
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
        StoreReset::JournalAuthority => {
            // The sealed journal still verifies the terminal, so nothing is
            // unverifiable: no quarantine marker and no quarantine diagnostic.
            let absent = SqliteMemory::new(dir.path())
                .unwrap()
                .find_owned_message_event_by_idempotency_key(
                    &first.workspace_id,
                    first.owner_id.as_deref().unwrap(),
                    &format!("ingress:{}:quarantine", first.event_id),
                )
                .await
                .unwrap();
            assert!(absent.is_none(), "journal-authority terminal must not be quarantined");
            assert!(
                !text.contains("quarantined"),
                "unexpected quarantine diagnostic: {text}"
            );
        }
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

#[tokio::test(flavor = "current_thread")]
async fn recovery_failure_keeps_run_and_store_names_in_reply_and_chat_log() {
    const TEST: &str =
        "chat::tests::persist_restart_tests::recovery_failure_keeps_run_and_store_names_in_reply_and_chat_log";
    if run_in_isolated_process(TEST, "PRX_TEST_RECOVERY_LOG_CHILD") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // A second chat cannot claim this live lifetime. Replace only the decision
    // database, as in O12, while retaining the real completed source and owner.
    let first_cleanup = CleanupScope::new(None, CancellationToken::new()).unwrap();
    let first = completed_chat_under_cleanup(dir.path(), Fault::None, &first_cleanup).await;
    let saved = dir.path().join("previous-decisions");
    std::fs::create_dir(&saved).unwrap();
    for name in ["decisions.db", "decisions.db-wal", "decisions.db-shm"] {
        let file = dir.path().join("self/causal_tree").join(name);
        if file.exists() {
            std::fs::rename(&file, saved.join(name)).unwrap();
        }
    }
    let log_path = dir.path().join("chat.log");
    let file = std::fs::File::create(&log_path).unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || file.try_clone().unwrap())
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);
    let memory = Arc::new(SqliteMemory::new(dir.path()).unwrap());
    let generation = crate::config::new_shared(config_at(dir.path())).pin();
    let error = match guarded_input(memory, &generation, "prx-current-run").await {
        Err(error) => error,
        Ok(_) => panic!("a live prior lifetime must refuse recovery"),
    };
    let reply = error.to_string();
    let log = std::fs::read_to_string(&log_path).unwrap();
    let line = log
        .lines()
        .find(|line| line.contains("chat input persistence failed"))
        .unwrap();
    for expected in [
        first.run_id.as_deref().unwrap(),
        "brain.db",
        "decisions.db",
        "another ingress lifetime remains active",
    ] {
        assert!(reply.contains(expected), "missing {expected}: {reply}");
        assert!(line.contains(expected), "missing {expected}: {line}");
    }
    assert!(!reply.contains('/'), "full paths must stay out of the reply: {reply}");
    for store in [
        dir.path().join("memory/brain.db"),
        crate::causal_tree::store::db_path(dir.path()),
    ] {
        assert!(
            line.contains(store.to_str().unwrap()),
            "missing full store path: {line}"
        );
    }
    assert!(line.contains("run_id="), "missing structured run field: {line}");

    let private_error = anyhow::anyhow!("permission denied")
        .context("https://prx_user:prx_fixture_password@example.com/v1?key=prx_fixture_query")
        .context(crate::runtime::ingress_recovery::IngressRecoveryDiagnostic {
            run_id: "/prx-private-run".into(),
            memory_store: dir.path().join("memory/brain.db"),
            decision_store: crate::causal_tree::store::db_path(dir.path()),
        });
    let private_reply = crate::chat::decision_source::chat_input_persistence_failure(&private_error).to_string();
    assert!(!private_reply.contains('/'), "{private_reply}");
    assert!(!private_reply.contains("prx-private-run"), "{private_reply}");
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(!log.contains("prx_fixture_password"), "credential reached chat.log");
    assert!(!log.contains("prx_fixture_query"), "URL query secret reached chat.log");
    assert!(log.to_ascii_lowercase().contains("[redacted]"), "{log}");
    first_cleanup.run(async { Ok(()) }).await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn unauthenticated_marker_warns_once_per_run_with_recovery_hint() {
    const TEST: &str =
        "chat::tests::persist_restart_tests::unauthenticated_marker_warns_once_per_run_with_recovery_hint";
    if run_in_isolated_process(TEST, "PRX_TEST_MARKER_WARNING_CHILD") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let first = completed_chat(dir.path(), Fault::FloatDrift).await;
    let memory = SqliteMemory::new(dir.path()).unwrap();
    let capsule: crate::memory::ingress_recovery::IngressCapsule = serde_json::from_value(
        serde_json::from_str::<serde_json::Value>(first.raw_payload_json.as_deref().unwrap())
            .unwrap()
            .get("prx_ingress_capsule")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let mut legacy: MessageEventInput = serde_json::from_value(serde_json::to_value(&first).unwrap()).unwrap();
    legacy.event_id = None;
    legacy.role = "event".into();
    legacy.event_type = "turn.ingress.quarantined".into();
    legacy.idempotency_key = Some(format!("ingress:{}:quarantine", first.event_id));
    legacy.causation_event_id = Some(first.event_id.clone());
    legacy.content = "turn.ingress.quarantined legacy marker".into();
    legacy.raw_payload_json = Some(serde_json::json!({"capsule": capsule, "reason": "legacy marker"}).to_string());
    let legacy = memory.append_message_event(legacy).await.unwrap();
    drop(memory);
    let log_path = dir.path().join("chat.log");
    let file = std::fs::File::create(&log_path).unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || file.try_clone().unwrap())
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);
    restarted_chat(dir.path(), "prx-marker-warning-run").await;
    let log = std::fs::read_to_string(&log_path).unwrap();
    let warnings: Vec<_> = log
        .lines()
        .filter(|line| line.contains("ignored unauthenticated ingress quarantine marker"))
        .collect();
    assert_eq!(warnings.len(), 1, "duplicate warning within a run: {log}");
    let warning = warnings.first().unwrap();
    for expected in [
        "WARN",
        &legacy.event_id,
        "remove the unauthenticated marker row",
        "rerun ingress recovery",
    ] {
        assert!(warning.contains(expected), "missing {expected}: {warning}");
    }
    assert!(!warning.contains("ERROR"), "{warning}");
    let tag = repository_tag(
        dir.path(),
        &capsule,
        RecoveryMacDomain::QuarantineKeyV3,
        &serde_json::to_vec(&capsule).unwrap(),
    );
    let memory = SqliteMemory::new(dir.path()).unwrap();
    assert!(
        memory
            .find_owned_message_event_by_idempotency_key(
                &first.workspace_id,
                first.owner_id.as_deref().unwrap(),
                &format!("ingress:{}:quarantine:{}", first.event_id, hex::encode(tag.tag)),
            )
            .await
            .unwrap()
            .is_some(),
        "the invalid marker must not suppress genuine quarantine recovery"
    );
    drop(memory);
    restarted_chat(dir.path(), "prx-marker-warning-next-run").await;
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("ignored unauthenticated ingress quarantine marker"))
            .count(),
        2,
        "a new run may emit one warning for the retained legacy marker: {log}"
    );
    assert_eq!(marker(dir.path(), &first).await.event_id, legacy.event_id);
}

/// Hold the real writer so journal close can finish before a decision row
/// exists. Legacy replay must wait for an acknowledged queue drain.
#[tokio::test(flavor = "current_thread")]
async fn legacy_fixture_waits_for_queued_decision_writes() {
    let dir = tempfile::tempdir().expect("isolated decision store");
    let cleanup = CleanupScope::new(None, CancellationToken::new()).expect("cleanup owner");
    cleanup
        .run(async {
            let memory = Arc::new(SqliteMemory::new(dir.path())?);
            let generation = crate::config::new_shared(config_at(dir.path())).pin();
            let hold = Arc::new(parking_lot::Mutex::new(()));
            let held_store = crate::causal_tree::store::CausalDecisionStore::open_shared_held(
                dir.path(),
                crate::causal_tree::store::Retention::days(1, 1),
                Arc::new(crate::decision::telemetry::ObserverCausalSink::new(Arc::new(
                    crate::observability::noop::NoopObserver,
                ))),
                Arc::clone(&hold),
            )?;
            let (source, binding, guard) = guarded_input(memory.clone(), &generation, "prx-first-run").await?;
            let service = publish(memory.clone(), generation).await?;
            drop(held_store); // The actual service now owns the shared writer.

            let (ready, ready_rx) = std::sync::mpsc::channel();
            let (release, release_rx) = std::sync::mpsc::channel::<()>();
            let holder = std::thread::spawn(move || {
                let _held = hold.lock();
                ready.send(()).expect("writer hold acquired");
                // The sender also drops on unwind, so assertion failures release
                // the writer without a timing-based auto-release.
                let _ = release_rx.recv();
            });
            ready_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("writer hold ready");

            let cancel = CancellationToken::new();
            binding.decide(&service, &cancel).await?;
            assert!(binding.fixed_reply()?.is_some());
            let fabric = MemoryFabric::new(memory.clone(), dir.path().to_string_lossy());
            let receipt = binding.close(&fabric, 2, chrono::Utc::now(), &cancel).await?;
            let mut payload: serde_json::Value = serde_json::from_str(
                receipt
                    .terminal_event
                    .raw_payload_json
                    .as_deref()
                    .expect("real terminal payload"),
            )?;
            assert!(
                payload
                    .pointer_mut("/terminal_replay_v1")
                    .expect("replay binding")
                    .as_object_mut()
                    .expect("replay object")
                    .remove("journal_version")
                    .is_some()
            );
            let key = crate::decision::DecisionKey::load_or_create(dir.path())?;
            crate::agent::terminal::reseal_payload_for_test(&key, &mut payload, receipt.assistant_event.as_ref())?;
            memory.replace_event_payload_for_test(&receipt.terminal_event.event_id, &payload.to_string())?;

            let error = service
                .recovery_scope()
                .rejoin_terminal("prx-first-run", source.owner_id.as_deref().expect("source owner"))
                .await
                .expect_err("queued decision is not durable while writer is held");
            assert!(
                format!("{error:#}").contains("terminal replay decision row is missing"),
                "{error:#}"
            );

            let drain = drain_legacy_fixture_decisions(&service);
            tokio::pin!(drain);
            assert!(
                futures::poll!(drain.as_mut()).is_pending(),
                "fixture must wait for the held decision writer"
            );
            release.send(()).expect("release actual writer");
            drain.await;
            holder.join().expect("writer holder exits");
            service
                .recovery_scope()
                .rejoin_terminal("prx-first-run", source.owner_id.as_deref().expect("source owner"))
                .await
                .expect("acknowledged decision row supports legacy replay");
            guard.handoff();
            service
                .shutdown(std::time::Instant::now() + Duration::from_secs(5))
                .await;
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("real legacy replay and cleanup");
}

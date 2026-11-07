#![cfg(test)]
#![allow(clippy::expect_used)]

use super::*;

struct LinkedFixture {
    workspace: tempfile::TempDir,
    config: crate::config::Config,
    memory: Arc<dyn Memory>,
    service: Arc<TurnDecisionService>,
    proof: AgentTurnProof,
    context: Arc<TurnDecisionContext>,
    commit: crate::agent::terminal::TurnTerminalDraft,
    transports: CountingTransports,
    dispatches: Arc<AtomicUsize>,
}

impl LinkedFixture {
    async fn new() -> Self {
        Self::with_enabled(true).await
    }

    async fn with_enabled(enabled: bool) -> Self {
        let workspace = tempfile::tempdir().expect("owned workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.causal_tree.enabled = enabled;
        let generation = crate::config::new_shared(config.clone()).pin();
        let changed = Arc::new(AtomicBool::new(false));
        let dispatches = Arc::new(AtomicUsize::new(0));
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(DriftingCatalogTool {
            changed: Arc::clone(&changed),
            dispatches: Arc::clone(&dispatches),
        })]);
        let security = Arc::new(SecurityPolicy::default());
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("owning SQLite backend"));
        let transports = CountingTransports(AtomicUsize::new(0));
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let service = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::new(RecordingSink::default()),
                memory: Arc::clone(&memory),
                security,
            },
        )
        .await
        .expect("actual recorded service");
        let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
            &generation,
            Arc::clone(&memory),
            "  ",
            "prx_scope_run",
        )
        .await;
        changed.store(true, Ordering::SeqCst);
        let context = service
            .decide_agent_initial(&proof, AgentTurnKind::Repl, &CancellationToken::new())
            .await
            .expect("genuine catalog drift terminal");
        if enabled {
            assert_eq!(
                context.terminal_outcome_action(),
                Some(crate::causal_tree::branch::CausalActionKind::Clarify),
            );
        }
        let now = chrono::Utc::now();
        let scope = crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
            .with_owner_id(proof.owner_id())
            .with_channel("cli")
            .with_session_key(proof.session_key())
            .with_run_id(proof.run_id());
        let commit = crate::agent::terminal::TurnTerminalDraft {
            decision: Some(
                service
                    .terminal_settlement_ref(Arc::clone(&context))
                    .expect("actual source-bound link"),
            ),
            terminal_id: "prx_scope_terminal".to_owned(),
            scope: scope.clone(),
            status: crate::agent::terminal::TurnTerminalStatus::Completed,
            history: Some(crate::agent::terminal::TurnHistoryProjection {
                assistant_content: "PRX local terminal response".to_owned(),
                history_commit_len: 0,
            }),
            history_scope: Some(scope.with_sender("ollama/prx-local-model").with_recipient("local-user")),
            provider_outcome: None,
            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                summary: "linked local scope boundary".to_owned(),
                started_at: now,
                finished_at: now,
            },
            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
        };
        let fixture = Self {
            workspace,
            config,
            memory,
            service,
            proof,
            context,
            commit,
            transports,
            dispatches,
        };
        fixture.assert_no_outcome().await;
        fixture
    }

    fn fabric(&self, memory: Arc<dyn Memory>) -> MemoryFabric {
        MemoryFabric::new(memory, self.workspace.path().to_string_lossy())
    }

    async fn outcome_count(&self) -> usize {
        if !self.config.causal_tree.enabled {
            assert!(self.service.store.writer().is_none());
            return 0;
        }
        let writer = self.service.store.writer().expect("owning writer");
        assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
        let scope = DecisionScope::Owner {
            workspace_id: self.service.store.workspace_id().expect("owning decision scope"),
            owner_id: Arc::from(self.proof.owner_id()),
        };
        let row = self
            .service
            .store
            .reader()
            .get(&scope, &self.context.decision_id())
            .expect("read genuine chain")
            .expect("genuine durable decision");
        assert_eq!(row.decision_id, self.context.decision_id());
        row.outcomes.len()
    }

    async fn assert_no_outcome(&self) {
        assert_eq!(self.outcome_count().await, 0);
        assert_eq!(self.transports.0.load(Ordering::SeqCst), 0);
        assert_eq!(self.dispatches.load(Ordering::SeqCst), 0);
    }

    async fn marker_presence(&self, fabric: &MemoryFabric) -> [bool; 4] {
        let mut presence = [false; 4];
        for (slot, suffix) in presence
            .iter_mut()
            .zip(["decision-claim", "assistant", "usage", "final"])
        {
            *slot = fabric
                .memory()
                .find_message_event_by_idempotency_key(
                    fabric.workspace_id(),
                    &format!("turn:{}:{suffix}", self.commit.terminal_id),
                )
                .await
                .expect("owned marker lookup")
                .is_some();
        }
        presence
    }

    #[allow(clippy::print_stderr)]
    async fn reject_before_writes(&self, fabric: &MemoryFabric, commit: crate::agent::terminal::TurnTerminalDraft) {
        let result =
            crate::agent::terminal::finalize_turn(fabric, commit, &self.config.cost, self.workspace.path()).await;
        // Keep metadata-only red evidence from the genuine stores, not a fake result.
        let marker_presence = self.marker_presence(fabric).await;
        let outcomes = self.outcome_count().await;
        eprintln!(
            "PRX_FOUNDATION_TERMINAL_SCOPE accepted={} markers={marker_presence:?} outcomes={outcomes}",
            result.is_ok()
        );
        assert!(
            result.is_err(),
            "linked finalizer must reject foreign scope before any write"
        );
        assert_eq!(marker_presence, [false; 4]);
        assert_eq!(
            self.marker_presence(&self.fabric(Arc::clone(&self.memory))).await,
            [false; 4]
        );
        self.assert_no_outcome().await;
        assert!(!self.workspace.path().join("state/costs.jsonl").exists());
        self.service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }
}

#[tokio::test]
async fn linked_finalizer_rejects_same_label_foreign_sqlite_before_writes() {
    let fixture = LinkedFixture::new().await;
    let foreign_dir = tempfile::tempdir().expect("separate owned backend");
    let foreign: Arc<dyn Memory> =
        Arc::new(crate::memory::SqliteMemory::new(foreign_dir.path()).expect("foreign real SQLite"));
    assert!(!Arc::ptr_eq(&foreign, &fixture.memory));
    let fabric = fixture.fabric(foreign);
    assert_eq!(fabric.workspace_id(), fixture.workspace.path().to_string_lossy());
    fixture.reject_before_writes(&fabric, fixture.commit.clone()).await;
    assert!(!foreign_dir.path().join("state/costs.jsonl").exists());
}

async fn reject_history_change(mutate: fn(&mut crate::memory::MessageEventScope)) {
    let fixture = LinkedFixture::new().await;
    let mut commit = fixture.commit.clone();
    mutate(commit.history_scope.as_mut().expect("actual alternate history scope"));
    fixture
        .reject_before_writes(&fixture.fabric(Arc::clone(&fixture.memory)), commit)
        .await;
}

#[tokio::test]
async fn linked_history_rejects_foreign_owner_before_writes() {
    reject_history_change(|scope| scope.owner_id = Some("prx_foreign_owner".to_owned())).await;
}

#[tokio::test]
async fn linked_history_rejects_foreign_session_before_writes() {
    reject_history_change(|scope| scope.session_key = Some("prx_foreign_session".to_owned())).await;
}

#[tokio::test]
async fn linked_history_rejects_foreign_run_before_writes() {
    reject_history_change(|scope| scope.run_id = Some("prx_foreign_run".to_owned())).await;
}

#[tokio::test]
async fn linked_history_rejects_broader_visibility_before_writes() {
    reject_history_change(|scope| scope.visibility = crate::memory::MemoryVisibility::Global).await;
}

#[tokio::test]
async fn linked_terminal_rejects_broader_visibility_before_writes() {
    let fixture = LinkedFixture::new().await;
    let mut commit = fixture.commit.clone();
    commit.scope.visibility = crate::memory::MemoryVisibility::Global;
    fixture
        .reject_before_writes(&fixture.fabric(Arc::clone(&fixture.memory)), commit)
        .await;
}

#[tokio::test]
async fn linked_finalizer_accepts_own_memory_and_same_scope_presentation() {
    let fixture = LinkedFixture::new().await;
    let fabric = fixture.fabric(Arc::clone(&fixture.memory));
    let first = crate::agent::terminal::finalize_turn(
        &fabric,
        fixture.commit.clone(),
        &fixture.config.cost,
        fixture.workspace.path(),
    )
    .await
    .expect("owning link with matching history identity");
    let second = crate::agent::terminal::finalize_turn(
        &fabric,
        fixture.commit.clone(),
        &fixture.config.cost,
        fixture.workspace.path(),
    )
    .await
    .expect("idempotent same scope replay");
    assert_eq!(first.terminal_event.id, second.terminal_event.id);
    assert_eq!(
        first.assistant_event.as_ref().expect("actual history").id,
        second.assistant_event.as_ref().expect("same actual history").id
    );
    assert_eq!(fixture.outcome_count().await, 1);
    assert_eq!(fixture.marker_presence(&fabric).await, [true, true, false, true]);
    let history = first
        .assistant_event
        .as_ref()
        .expect("persisted alternate presentation");
    assert_eq!(history.owner_id.as_deref(), Some(fixture.proof.owner_id()));
    assert_eq!(history.session_key.as_deref(), Some(fixture.proof.session_key()));
    assert_eq!(history.run_id.as_deref(), Some(fixture.proof.run_id()));
    assert_eq!(history.channel.as_deref(), Some("cli"));
    assert_eq!(history.source, crate::memory::MessageEventSource::Agent);
    assert_eq!(history.visibility, crate::memory::MemoryVisibility::Session);
    assert_eq!(history.sender.as_deref(), Some("ollama/prx-local-model"));
    assert_eq!(history.recipient.as_deref(), Some("local-user"));
    assert_eq!(fixture.transports.0.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.dispatches.load(Ordering::SeqCst), 0);
    fixture.service.shutdown(Instant::now() + Duration::from_secs(5)).await;
}

#[tokio::test]
async fn terminal_guard_binding_restores_omitted_link_and_rejects_foreign_context() {
    let fixture = LinkedFixture::new().await;
    let owned = fixture.commit.decision.clone().expect("issued terminal link");
    let mut proposal = fixture.commit.clone();
    proposal.decision = None;
    crate::runtime::turn_terminal::bind_terminal_decision(&mut proposal, &owned).expect("guard restores its own link");
    assert!(proposal.decision.as_ref().expect("restored").same_context(&owned));
    crate::runtime::turn_terminal::bind_terminal_decision(&mut proposal, &owned)
        .expect("same context remains accepted");

    let foreign = LinkedFixture::new().await;
    let foreign_link = foreign.commit.decision.as_ref().expect("foreign issued link");
    assert!(crate::runtime::turn_terminal::bind_terminal_decision(&mut proposal, foreign_link).is_err());
    assert!(
        proposal
            .decision
            .as_ref()
            .expect("unchanged on rejection")
            .same_context(&owned)
    );
    fixture.assert_no_outcome().await;
    foreign.assert_no_outcome().await;
}

#[tokio::test]
async fn disabled_terminal_validates_source_without_decision_store_writes() {
    let fixture = LinkedFixture::with_enabled(false).await;
    let fabric = fixture.fabric(Arc::clone(&fixture.memory));
    let mut foreign = fixture.commit.clone();
    foreign.scope.owner_id = Some("prx_foreign_owner".to_owned());
    fixture.reject_before_writes(&fabric, foreign).await;
    assert!(
        fixture
            .commit
            .decision
            .as_ref()
            .expect("disabled source link")
            .validate_durable_chain()
            .await
            .is_err()
    );
    let receipt = crate::agent::terminal::finalize_turn(
        &fabric,
        fixture.commit.clone(),
        &fixture.config.cost,
        fixture.workspace.path(),
    )
    .await
    .expect("disabled terminal retains ordinary history behavior");
    assert!(receipt.assistant_event.is_some());
    assert_eq!(fixture.marker_presence(&fabric).await, [false, true, false, true]);
    assert!(fixture.service.store.writer().is_none());
    assert!(!fixture.workspace.path().join("self/causal_tree/.decision_key").exists());
    assert!(!fixture.workspace.path().join("self/causal_tree/decisions.db").exists());
    assert!(!fixture.service.recovery_scope().ingress_recovery_enabled());
    fixture.assert_no_outcome().await;
}

/// Where the persisted issued-state fingerprint is tampered.
#[derive(Clone, Copy)]
enum FingerprintTamper {
    /// The decision-store row. Capture no longer reads it, so it cannot reject.
    StoreRow,
    /// The journal `decision_evidence` row, resealed so only the fingerprint differs.
    JournalEvidence,
}

fn tampered_fingerprint(stored: &str, double_encoded: bool) -> String {
    let changed = if double_encoded {
        hex::encode(stored.as_bytes())
    } else {
        let mut changed = stored.to_owned();
        changed.replace_range(..1, if stored.starts_with('0') { "1" } else { "0" });
        changed
    };
    assert_ne!(changed, stored);
    changed
}

/// Replaces the sealed journal evidence of one decision with `fingerprint`.
fn reseal_journal_fingerprint(fixture: &LinkedFixture, owner: &str, decision: &str, double_encoded: bool) {
    let workspace_id = fixture.service.store.workspace_id().expect("owned store namespace");
    let key = &fixture.service.state_key;
    let columns = crate::runtime::child_source::authority_journal::read_decision_evidence(
        fixture.workspace.path(),
        &workspace_id,
        owner,
        decision,
        key,
    )
    .expect("authenticated journal evidence")
    .expect("journal evidence row of the issued hop");
    let mut named = Vec::with_capacity(columns.len());
    for (name, value) in columns {
        let name = crate::causal_tree::store::replay_column_names()
            .into_iter()
            .find(|candidate| *candidate == name)
            .expect("known replay column");
        named.push((name, value));
    }
    let slot = named
        .iter_mut()
        .find(|(name, _)| *name == "state_fingerprint")
        .expect("state_fingerprint column");
    let rusqlite::types::Value::Text(stored) = &slot.1 else {
        panic!("state_fingerprint evidence is not text");
    };
    assert_eq!(stored.len(), 64);
    slot.1 = rusqlite::types::Value::Text(tampered_fingerprint(stored, double_encoded));
    let journal = rusqlite::Connection::open(crate::runtime::child_source::authority_journal::path(
        fixture.workspace.path(),
    ))
    .expect("authority journal");
    assert_eq!(
        journal
            .execute(
                "DELETE FROM decision_evidence WHERE workspace = ?1 AND owner = ?2 AND decision = ?3",
                (workspace_id.as_ref(), owner, decision),
            )
            .expect("remove exactly the actual evidence row"),
        1
    );
    drop(journal);
    crate::runtime::child_source::authority_journal::write_decision_evidence(fixture.workspace.path(), key, &[named])
        .expect("reseal the tampered evidence row");
}

#[tokio::test]
async fn linked_finalizer_rejects_changed_or_double_encoded_persisted_fingerprint_before_writes() {
    for tamper in [FingerprintTamper::JournalEvidence, FingerprintTamper::StoreRow] {
        for double_encoded in [false, true] {
            let fixture = LinkedFixture::new().await;
            let workspace_id = fixture.service.store.workspace_id().expect("owned store namespace");
            let hop = fixture.context.audit_hops().next().expect("actual issued hop");
            let decision_id = hop.decision_id.to_string();
            let owner = fixture.proof.owner_id().to_owned();
            let expected = hop.state.material_fingerprint(&fixture.service.state_key);
            let connection = rusqlite::Connection::open(fixture.workspace.path().join("self/causal_tree/decisions.db"))
                .expect("actual decision store");
            let stored: String = connection
                .query_row(
                    "SELECT state_fingerprint FROM decisions WHERE workspace_id=?1 AND decision_id=?2",
                    rusqlite::params![workspace_id.as_ref(), &decision_id],
                    |row| row.get(0),
                )
                .expect("durable issued fingerprint");
            assert_eq!(stored, expected);
            assert_eq!(stored.len(), 64);
            assert!(stored.bytes().all(|byte| byte.is_ascii_hexdigit()));
            match tamper {
                FingerprintTamper::StoreRow => {
                    let changed = tampered_fingerprint(&stored, double_encoded);
                    assert_eq!(
                        connection
                            .execute(
                                "UPDATE decisions SET state_fingerprint=?1 WHERE workspace_id=?2 AND decision_id=?3",
                                rusqlite::params![changed, workspace_id.as_ref(), decision_id],
                            )
                            .expect("change exactly the actual owned row"),
                        1
                    );
                }
                FingerprintTamper::JournalEvidence => {
                    reseal_journal_fingerprint(&fixture, &owner, &decision_id, double_encoded);
                }
            }
            drop(connection);
            let fabric = fixture.fabric(Arc::clone(&fixture.memory));
            let result = crate::agent::terminal::finalize_turn(
                &fabric,
                fixture.commit.clone(),
                &fixture.config.cost,
                fixture.workspace.path(),
            )
            .await;
            match tamper {
                FingerprintTamper::JournalEvidence => {
                    let error = result.expect_err("altered journal fingerprint must reject terminal capture");
                    assert_eq!(
                        error.to_string(),
                        "persisted decision replay binding differs from the issued context"
                    );
                    assert_eq!(fixture.marker_presence(&fabric).await, [false; 4]);
                    fixture.assert_no_outcome().await;
                    assert!(!fixture.workspace.path().join("state/costs.jsonl").exists());
                }
                FingerprintTamper::StoreRow => {
                    // Journal authority never reads the store row, so its tamper
                    // is inert: capture still binds to the sealed journal evidence.
                    result.expect("store row is not terminal evidence under journal authority");
                    assert!(fixture.marker_presence(&fabric).await[3], "final marker committed");
                }
            }
            fixture.service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        }
    }
}

#[tokio::test]
async fn recorded_terminal_rejoins_after_service_drop_with_independent_recovery() {
    let fixture = LinkedFixture::new().await;
    let fabric = fixture.fabric(Arc::clone(&fixture.memory));
    let receipt = crate::agent::terminal::finalize_turn(
        &fabric,
        fixture.commit.clone(),
        &fixture.config.cost,
        fixture.workspace.path(),
    )
    .await
    .expect("authenticated historical terminal");
    let generation = Arc::clone(&fixture.service.generation);
    let service = Arc::downgrade(&fixture.service);
    drop(fixture.service);
    assert!(
        service.upgrade().is_none(),
        "recovery must not retain the decision service"
    );
    let cleanup = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    let recovery = crate::runtime::ingress_recovery::RecoveryScope::prepare(
        &generation,
        Arc::clone(&fixture.memory),
        TurnSurfaceKind::AgentLoop,
        Arc::new(RecordingSink::default()),
        None,
        &cleanup,
        Some(DecisionKey::load_existing(&generation.effective.workspace_dir).unwrap()),
    )
    .await
    .expect("independent recovery initialization");
    assert_eq!(
        recovery
            .rejoin_terminal(&receipt.terminal_id, fixture.proof.owner_id())
            .await
            .expect("authenticated terminal rejoins without a decision service"),
        Enqueue::Queued,
    );
    drop(recovery);
    cleanup
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await
        .expect("independently retained recovery queues drain");
}

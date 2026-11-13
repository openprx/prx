//! R54: a pre-switch pending decision approval resumes exactly once.
//!
//! The pending item is seeded the way the pre-switch runtime left it: both
//! sources come from the real chat source writer and receipt
//! (`recorded_approval_source`), the decision chain from the real journal and
//! decision-store writers, the call digest from `admitted_call_digest`, and
//! the pending marker through `MemoryFabric::record_runtime_event_idempotent`
//! with the signed record shape the production reader authenticates (the
//! pre-switch pending writer no longer exists, R177-1). Review, admission,
//! the V-17 resume adapter, the durable claim and the dispatch are the
//! production code paths; only the pinned publication and the fresh turn are
//! test ports over real ledger, catalog and execution objects.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::super::historical::{
    AdmittedDecisionResume, ExecutionResumeAdapter, HistoricalApprovalRuntime, ResumeTurn, admit_decision_resume,
    review_pending_decision_approval, revoke_pending_decision_approval,
};
use super::super::keys::{ApprovalAuthority, DecisionAuthority};
use super::super::pending::{PendingRecord, SignedPending, VerifiedPendingDecisionApproval, scope_from_source};
use super::super::{ApprovalLedger, ApprovalRuntime};
use crate::acl::approval_grant::{DecisionApprovalBinding, RiskLevel, Subject, WitnessKeyring};
use crate::agent::turn_contracts::ChatMode;
use crate::chat::binding::ChatTurnBinding;
use crate::chat::binding::tests::recorded_approval_source;
use crate::config::ConfigGeneration;
use crate::memory::{Memory, MemoryFabric, MessageEvent, SqliteMemory};
use crate::runtime::decision_journal::store::{CausalDecisionStore, DecisionScope, Retention, StoreMode};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::runtime::turn_source::TurnSourceProof;
use crate::security::SecurityPolicy;
use crate::security::decision_id::DecisionId;
use crate::security::runtime_key::{DecisionKey, KeyDomain};
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogSources, SurfacePolicy};
use crate::tools::capability::contract::CapabilityRef;
use crate::tools::capability::execution_binding::{CallDigestSource, DecisionResumePort, admitted_call_digest};
use crate::tools::execution::admission::TurnSurface;
use crate::tools::execution::{
    AdapterOwnedPreparation, DenyApprovalStrategy, SecurityEffectPolicy, ToolExecutionContext, ToolExecutionService,
    TracingToolExecutionAudit,
};
use crate::tools::schema::ToolSchemaDialect;
use crate::tools::traits::{Tool, ToolResult};
use crate::tools::{ToolExecutionCommand, owner_filesystem::OwnerFilesystem};

const ORIGINAL_RUN: &str = "prx_resume_original_run";
const FRESH_RUN: &str = "prx_resume_fresh_run";
const SESSION: &str = "prx_plan_discovery_session";
const POLICY: &str = "prx_policy";

/// A shell-shaped tool that only counts dispatches.
struct CountedShell {
    name: &'static str,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for CountedShell {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "Counted shell fixture"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            success: true,
            output: "ok".to_owned(),
            error: None,
        })
    }
}

/// The pinned publication of the historical approval, over real objects.
struct PinnedPublication {
    memory: Arc<dyn Memory>,
    key: Arc<DecisionKey>,
    store: Arc<CausalDecisionStore>,
    workspace: String,
    dir: std::path::PathBuf,
    catalog: CapabilityCatalog,
    execution: ToolExecutionService,
    service_id: uuid::Uuid,
}

#[async_trait::async_trait]
impl ApprovalRuntime for PinnedPublication {
    fn check_live_parity(&self) -> anyhow::Result<()> {
        self.catalog.check_parity(&self.execution).map_err(anyhow::Error::from)
    }
    async fn verify_pending_console_file_operation(&self, _: &VerifiedPendingDecisionApproval) -> anyhow::Result<()> {
        Ok(())
    }
    fn open_pending_call(&self, _: &VerifiedPendingDecisionApproval) -> anyhow::Result<ToolExecutionCommand> {
        anyhow::bail!("fixture has no sealed call")
    }
    fn verify_h2_review_requirement(
        &self,
        _: &VerifiedPendingDecisionApproval,
        _: bool,
        _: Option<&crate::runtime::h2_entry::LocalOwnerReviewStamp>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn retire_pending(&self, _: &PendingRecord) {}
}

#[async_trait::async_trait]
impl HistoricalApprovalRuntime for PinnedPublication {
    fn approval_ledger(&self) -> ApprovalLedger<'_> {
        ApprovalLedger {
            memory: &self.memory,
            state_key: &self.key,
            store: &self.store,
            workspace: self.workspace.clone(),
            workspace_dir: &self.dir,
            catalog_generation: self.catalog.generation().0,
            policy_version: POLICY,
            enabled: true,
            port: self,
        }
    }
    fn memory(&self) -> &Arc<dyn Memory> {
        &self.memory
    }
    fn catalog(&self) -> &CapabilityCatalog {
        &self.catalog
    }
    fn execution(&self) -> &ToolExecutionService {
        &self.execution
    }
    fn policy_version(&self) -> &str {
        POLICY
    }
    fn service_id(&self) -> uuid::Uuid {
        self.service_id
    }
    fn resolve_turn_model(&self, _: &dyn TurnSourceProof) -> anyhow::Result<()> {
        Ok(())
    }
    fn check_shell_path_arguments(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn pending_console_filesystem(&self, _: &MessageEvent) -> anyhow::Result<Option<Arc<OwnerFilesystem>>> {
        Ok(None)
    }
    async fn scoped_file_operation(
        &self,
        _: Option<&Arc<OwnerFilesystem>>,
        _: &str,
        _: &str,
    ) -> anyhow::Result<(String, RiskLevel)> {
        anyhow::bail!("fixture has no file operation")
    }
}

/// The fresh resume turn: its own decision id and open execution.
struct FreshTurn {
    resume: AdmittedDecisionResume,
    decision_id: DecisionId,
    service_id: uuid::Uuid,
    active: AtomicBool,
}

#[async_trait::async_trait]
impl ResumeTurn for FreshTurn {
    fn resume(&self) -> Option<&AdmittedDecisionResume> {
        Some(&self.resume)
    }
    fn service_id(&self) -> uuid::Uuid {
        self.service_id
    }
    fn decision_id(&self) -> DecisionId {
        self.decision_id
    }
    fn execution_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
    async fn validate_durable_chain(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    generation: Arc<ConfigGeneration>,
    runtime: Arc<PinnedPublication>,
    decision_id: DecisionId,
    owner: String,
    calls: Arc<AtomicUsize>,
}

fn request(decision_id: DecisionId, command: &str) -> String {
    serde_json::json!({
        "decision_id": decision_id.to_string(),
        "tool": "shell",
        "arguments": {"command": command}
    })
    .to_string()
}

impl Fixture {
    /// Seeds one reviewed pre-switch pending item for `shell {"command":"ls"}`.
    async fn new(expires_in: chrono::Duration) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let mut config = crate::config::Config::default();
        config.workspace_dir = path.to_owned();
        config.config_path = path.join("config.toml");
        config.autonomy.level = crate::security::AutonomyLevel::Supervised;
        config.providers.models.push(
            toml::from_str(
                "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
            )
            .unwrap(),
        );
        config.save().await.expect("write fixture-only configuration");
        let generation = crate::config::new_shared(config.clone()).pin();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(path).unwrap());
        let key = DecisionKey::load_or_create(path).unwrap();
        let store = CausalDecisionStore::open_shared(
            path,
            Retention::days(0, 0),
            Arc::new(crate::observability::causal_event::testing::RecordingSink::default()),
            StoreMode::ReadWrite,
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, path));
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
            Box::new(CountedShell {
                name: "shell",
                calls: Arc::clone(&calls),
            }),
            Box::new(CountedShell {
                name: "prx_other",
                calls: Arc::clone(&calls),
            }),
        ]);
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let execution = ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(security)),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        );
        let decision_id = DecisionId::new_runtime();
        // The original pending request, through the real chat source writer.
        let (original, _) = recorded_approval_source(
            &generation,
            Arc::clone(&memory),
            &request(decision_id, "ls"),
            ChatMode::Edit,
            ORIGINAL_RUN,
            1,
        )
        .await;
        let workspace = original.workspace_id().to_owned();
        let owner = original.owner_id().to_owned();
        let source = memory
            .find_owned_message_event(
                &workspace,
                &owner,
                original.source_event_id().expect("test: original source"),
            )
            .await
            .unwrap()
            .expect("test: durable original source");
        // The pre-switch decision chain, through the real journal writers.
        let store_workspace = store.workspace_id().unwrap();
        let mut row = crate::runtime::decision_journal::store::testing::record(
            decision_id,
            &store_workspace,
            Some(&owner),
            chrono::Utc::now().timestamp_millis(),
        );
        row.lineage.session_key = Some(Arc::from(SESSION));
        row.lineage.run_id = Some(Arc::from(ORIGINAL_RUN));
        let evidence = crate::runtime::child_source::authority_journal::decision_evidence_rows(
            std::slice::from_ref(&row),
            &store_workspace,
        )
        .unwrap();
        crate::runtime::child_source::authority_journal::write_decision_evidence(path, &key, &evidence).unwrap();
        let writer = store.writer().unwrap();
        writer.record_decision(row);
        assert!(
            writer
                .drain(std::time::Instant::now() + std::time::Duration::from_secs(5))
                .await
        );
        let digest = store
            .reader()
            .replay_record(
                &DecisionScope::Owner {
                    workspace_id: store_workspace.clone(),
                    owner_id: Arc::from(owner.as_str()),
                },
                &decision_id,
            )
            .unwrap()
            .unwrap()
            .digest;
        let entry = catalog.entry_for_public_name("shell").unwrap();
        let capability = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        };
        let call_digest = admitted_call_digest(
            &decision_id.to_string(),
            &capability,
            CallDigestSource {
                workspace: &workspace,
                owner: &owner,
                session: SESSION,
                run: source.run_id.as_deref(),
                event: Some(&source.event_id),
                channel: source.channel.as_deref(),
                sender: source.sender.as_deref(),
                generation: source.config_generation_id,
                revision: source.config_source_revision.as_deref(),
            },
            &serde_json::json!({"command": "ls"}),
            "ls",
            RiskLevel::High,
        )
        .unwrap();
        let record = PendingRecord {
            h2_origin: None,
            subject: Subject {
                agent_id: "prx_agent".into(),
                principal_id: "PRXOperator".into(),
                owner_id: owner.clone(),
                workspace_id: workspace.clone(),
                session_key: Some(SESSION.into()),
            },
            binding: DecisionApprovalBinding {
                decision_id: decision_id.to_string(),
                catalog_generation: catalog.generation().0,
                call_digest,
            },
            source_event_id: source.event_id.clone(),
            source_mac: key.hmac(
                KeyDomain::ApprovalState,
                &[b"source", &serde_json::to_vec(&source).unwrap()],
            ),
            run_id: ORIGINAL_RUN.into(),
            decision_digests: vec![(decision_id.to_string(), digest)],
            policy_version: POLICY.into(),
            capabilities: vec![(entry.id.as_str().to_owned(), entry.fingerprint.0, entry.generation.0)],
            op_ids: vec!["ls".into()],
            effect: "process_exec".into(),
            risk: Some(RiskLevel::High),
            schema_dialect: Some(ToolSchemaDialect::Raw),
            sealed_call: None,
            expires_at: chrono::Utc::now() + expires_in,
        };
        let signed = SignedPending {
            mac: key.hmac(
                KeyDomain::ApprovalState,
                &[b"pending", &serde_json::to_vec(&record).unwrap()],
            ),
            record,
        };
        MemoryFabric::new(Arc::clone(&memory), &workspace)
            .record_runtime_event_idempotent(
                scope_from_source(&source),
                "approval.decision.pending",
                "decision awaiting human approval",
                Some(serde_json::to_string(&signed).unwrap()),
                DecisionAuthority(decision_id).pending_key(),
            )
            .await
            .unwrap();
        let runtime = Arc::new(PinnedPublication {
            memory,
            key,
            store,
            workspace,
            dir: path.to_owned(),
            catalog,
            execution,
            service_id: uuid::Uuid::new_v4(),
        });
        let fixture = Self {
            _dir: dir,
            generation,
            runtime,
            decision_id,
            owner,
            calls,
        };
        if expires_in > chrono::Duration::zero() {
            review_pending_decision_approval(fixture.runtime.as_ref(), &fixture.owner, SESSION, decision_id, true)
                .await
                .expect("test: owner approves the historical item");
        }
        fixture
    }

    /// A fresh resume request in `run`, through the real chat source writer.
    async fn source(&self, run: &str, command: &str, turn: u64) -> (Arc<ChatTurnBinding>, RuntimeEnvelope) {
        recorded_approval_source(
            &self.generation,
            Arc::clone(&self.runtime.memory),
            &request(self.decision_id, command),
            ChatMode::Edit,
            run,
            turn,
        )
        .await
    }

    async fn admit(&self, proof: &dyn TurnSourceProof) -> anyhow::Result<AdmittedDecisionResume> {
        admit_decision_resume(self.runtime.as_ref(), proof).await
    }

    fn turn(&self, resume: AdmittedDecisionResume) -> Arc<FreshTurn> {
        Arc::new(FreshTurn {
            resume,
            decision_id: DecisionId::new_runtime(),
            service_id: self.runtime.service_id,
            active: AtomicBool::new(true),
        })
    }

    fn surface(&self, only: Option<&str>) -> TurnSurface {
        let execution = &self.runtime.execution;
        let full = TurnSurface::build_from(
            execution,
            execution.current_descriptors(),
            &self.generation.effective.tool_tiering,
            "prx-local-model",
            false,
            |_| true,
        );
        match only {
            Some(name) => full.restricted_to(name),
            None => full,
        }
    }

    /// Dispatch `command` through the real executor with the V-17 adapter.
    async fn dispatch(
        &self,
        turn: &Arc<FreshTurn>,
        proof: &dyn TurnSourceProof,
        envelope: &RuntimeEnvelope,
        command: ToolExecutionCommand,
        surface: TurnSurface,
    ) -> crate::tools::ToolExecutionOutcome {
        let adapter = ExecutionResumeAdapter::new(
            Arc::clone(&self.runtime) as Arc<dyn HistoricalApprovalRuntime>,
            Arc::clone(turn) as Arc<dyn ResumeTurn>,
            proof,
            envelope.clone(),
        );
        let mut context = ToolExecutionContext::new(envelope.clone(), "dm");
        context.turn_surface = Some(Arc::new(surface));
        context.schema_dialect = ToolSchemaDialect::Raw;
        context.decision_resume = Some(DecisionResumePort::new(Arc::new(adapter)));
        self.runtime.execution.execute(command, context, None).await
    }

    fn executions(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn historical_resume_executes_exact_call_once_and_refuses_drift() {
    let _keyring = WitnessKeyring::global_for_tests();
    let live = chrono::Duration::minutes(4);

    // Approve ⇒ exactly one execution of the prepared call; a replayed claim
    // and a revoke after the claim are refused.
    let fixture = Fixture::new(live).await;
    let (proof, envelope) = fixture.source(FRESH_RUN, "ls", 2).await;
    let turn = fixture.turn(fixture.admit(proof.as_ref()).await.expect("fresh resume admits"));
    let prepared = turn.resume.prepared_command().clone();
    fixture
        .dispatch(
            &turn,
            proof.as_ref(),
            &envelope,
            prepared.clone(),
            fixture.surface(Some("shell")),
        )
        .await;
    assert_eq!(fixture.executions(), 1, "approve executes the prepared call once");
    fixture
        .dispatch(
            &turn,
            proof.as_ref(),
            &envelope,
            prepared,
            fixture.surface(Some("shell")),
        )
        .await;
    assert_eq!(fixture.executions(), 1, "a replayed claim is refused");
    assert!(
        revoke_pending_decision_approval(fixture.runtime.as_ref(), &fixture.owner, SESSION, fixture.decision_id)
            .await
            .is_err(),
        "a consumed approval cannot be revoked"
    );

    // Resume reusing the original source event (and its run) is refused.
    let fixture = Fixture::new(live).await;
    let (original_run_source, _) = fixture.source(ORIGINAL_RUN, "ls", 2).await;
    assert!(
        fixture.admit(original_run_source.as_ref()).await.is_err(),
        "a resume in the original run is refused"
    );
    // A receipt admitted for one fresh run never binds another run.
    let (proof, envelope) = fixture.source(FRESH_RUN, "ls", 3).await;
    let admitted = fixture.admit(proof.as_ref()).await.expect("fresh resume admits");
    let (other_run, _) = fixture.source("prx_resume_other_run", "ls", 4).await;
    assert!(
        admitted
            .gate_evidence(fixture.runtime.as_ref(), other_run.as_ref())
            .is_err(),
        "receipt for another run is refused"
    );
    let turn = fixture.turn(admitted);
    // Changed arguments are not the prepared call.
    let changed = ToolExecutionCommand {
        arguments: serde_json::json!({"command": "pwd"}),
        ..turn.resume.prepared_command().clone()
    };
    fixture
        .dispatch(
            &turn,
            proof.as_ref(),
            &envelope,
            changed,
            fixture.surface(Some("shell")),
        )
        .await;
    // Another tool on the surface (widened or missing surface) is refused.
    let prepared = turn.resume.prepared_command().clone();
    fixture
        .dispatch(
            &turn,
            proof.as_ref(),
            &envelope,
            prepared.clone(),
            fixture.surface(None),
        )
        .await;
    fixture
        .dispatch(
            &turn,
            proof.as_ref(),
            &envelope,
            prepared.clone(),
            fixture.surface(Some("prx_other")),
        )
        .await;
    assert_eq!(fixture.executions(), 0, "drifted dispatches never execute");
    // The untouched approval still executes exactly once afterwards.
    fixture
        .dispatch(
            &turn,
            proof.as_ref(),
            &envelope,
            prepared,
            fixture.surface(Some("shell")),
        )
        .await;
    assert_eq!(fixture.executions(), 1, "refusals did not consume the approval");

    // A changed request at admission is refused.
    let fixture = Fixture::new(live).await;
    let (changed_request, _) = fixture.source(FRESH_RUN, "pwd", 2).await;
    assert!(
        fixture.admit(changed_request.as_ref()).await.is_err(),
        "changed arguments at admission are refused"
    );

    // Expired ⇒ refused, zero executions.
    let fixture = Fixture::new(chrono::Duration::seconds(-60)).await;
    let (proof, _) = fixture.source(FRESH_RUN, "ls", 2).await;
    assert!(
        fixture.admit(proof.as_ref()).await.is_err(),
        "expired approval is refused"
    );
    assert_eq!(fixture.executions(), 0);

    // Revoked first ⇒ the admitted resume can no longer claim.
    let fixture = Fixture::new(live).await;
    let (proof, envelope) = fixture.source(FRESH_RUN, "ls", 2).await;
    let turn = fixture.turn(fixture.admit(proof.as_ref()).await.expect("fresh resume admits"));
    revoke_pending_decision_approval(fixture.runtime.as_ref(), &fixture.owner, SESSION, fixture.decision_id)
        .await
        .expect("owner revokes first");
    let prepared = turn.resume.prepared_command().clone();
    fixture
        .dispatch(
            &turn,
            proof.as_ref(),
            &envelope,
            prepared,
            fixture.surface(Some("shell")),
        )
        .await;
    assert_eq!(fixture.executions(), 0, "revoked-first never executes");

    // Concurrent claim vs revoke ⇒ exactly one resolution.
    let fixture = Fixture::new(live).await;
    let (proof, envelope) = fixture.source(FRESH_RUN, "ls", 2).await;
    let turn = fixture.turn(fixture.admit(proof.as_ref()).await.expect("fresh resume admits"));
    let prepared = turn.resume.prepared_command().clone();
    let (_, revoked) = tokio::join!(
        fixture.dispatch(
            &turn,
            proof.as_ref(),
            &envelope,
            prepared,
            fixture.surface(Some("shell"))
        ),
        revoke_pending_decision_approval(fixture.runtime.as_ref(), &fixture.owner, SESSION, fixture.decision_id),
    );
    assert_eq!(
        fixture.executions() + usize::from(revoked.is_ok()),
        1,
        "claim and revoke resolve exactly once"
    );
}

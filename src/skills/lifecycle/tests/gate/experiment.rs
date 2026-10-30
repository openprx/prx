//! Gate provenance behavior at the real admitted runtime seam. Judge mocks only
//! count calls; physical-budget behavior is covered by the managed stage tests.
use super::*;
use crate::runtime::{experiment_stage::ExperimentStageRuntime, turn_cleanup::CleanupScope};
use crate::skills::lifecycle::experiment::{
    ExperimentEvent, ExperimentId, ExperimentStage, ExperimentStageKind, test_specification,
};
use crate::skills::lifecycle::{OwnerScope, SkillLifecycleStore};

async fn in_gate_runtime<F: std::future::Future>(
    fx: &Fixture,
    expected: &SkillProposal,
    pins: &[EvaluationPin],
    root: &str,
    stage: &str,
    future: F,
) -> anyhow::Result<F::Output> {
    let mut spec = test_specification();
    spec.id = ExperimentId::parse(root)?;
    spec.owner_scope = fx.store.owner_scope().clone();
    spec.created_at = Utc::now();
    spec.expires_at = spec.created_at + chrono::Duration::seconds(60);
    spec.stages = vec![ExperimentStage {
        id: stage.into(),
        kind: ExperimentStageKind::GateReview,
        dependencies: vec![],
        input_sha256: "d".repeat(64),
        allocation: spec.resources,
    }];
    let plan = spec.freeze()?;
    let store = Arc::new(SkillLifecycleStore::open(
        fx.tmp.path(),
        fx.store.owner_scope().clone(),
    )?);
    let mut journal = store.admit_experiment(&plan, Utc::now())?;
    for (id, event) in [
        ("reserve", ExperimentEvent::Reserved { stage: stage.into() }),
        ("dispatch", ExperimentEvent::Dispatched { stage: stage.into() }),
    ] {
        journal = store.append_experiment(
            &plan.spec().id,
            plan.digest(),
            journal.next_sequence(),
            &journal.head_sha256,
            id,
            event,
            Utc::now(),
        )?;
    }
    let config = crate::config::Config {
        workspace_dir: fx.tmp.path().into(),
        ..Default::default()
    };
    let cancel = CancellationToken::new();
    let cleanup = CleanupScope::new(None, cancel.clone())?;
    let runtime = cleanup
        .scope_admission(async { ExperimentStageRuntime::new(&config, &plan, stage, cancel, store) })
        .await?;
    runtime.bind_gate_subject(crate::skills::lifecycle::GateReviewSubject {
        proposal_id: expected.proposal_id.clone(),
        suite_digest: expected.required_suite.clone().unwrap(),
        evaluations: pins
            .iter()
            .map(|pin| (pin.evaluation_id.clone(), pin.receipt_digest.clone()))
            .collect(),
        operation: expected.operation,
        skill_id: expected.skill_id.clone(),
        baseline_revision: expected.baseline_revision.clone(),
        candidate_revision: expected.candidate_revision.clone().unwrap(),
        expected_generation: expected.expected_generation,
        evaluation_epoch: expected.evaluation_epoch,
        evaluating_since: expected.evaluating_since.unwrap(),
    })?;
    cleanup
        .run(runtime.scope(async { Ok::<_, anyhow::Error>(future.await) }))
        .await
}

#[tokio::test]
async fn unbound_cached_gate_retains_legacy_json_and_cannot_be_relabelled_by_a_stage() -> anyhow::Result<()> {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let initial = gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), None).await?;
    let serialized = serde_json::to_value(&initial.gate.decision)?;
    assert!(
        serialized.get("experiment").is_none(),
        "legacy None must add no serialized field"
    );
    let legacy: crate::skills::lifecycle::SkillGateDecision = serde_json::from_value(serialized)?;
    assert_eq!(legacy, initial.gate.decision);
    let path = fx
        .store
        .root()
        .join("gates")
        .join(format!("{}.json", initial.gate.decision.record_id));
    let before = std::fs::read(&path)?;
    let judge = MockJudge::new(Behavior::Approve);
    let result = in_gate_runtime(
        &fx,
        &proposal,
        std::slice::from_ref(&pin),
        "gate-unbound-cache",
        "gate-review",
        gate(
            &fx,
            &proposal,
            &suite,
            std::slice::from_ref(&pin),
            &policy(),
            Some(&judge),
        ),
    )
    .await?;
    assert!(matches!(result, Err(GateError::Unbound(_))));
    assert_eq!(
        judge.calls.load(Ordering::SeqCst),
        0,
        "cached external review cannot create a zero-cost root review"
    );
    assert_eq!(std::fs::read(path)?, before, "no gate relabel or overwrite");
    Ok(())
}

#[tokio::test]
async fn gate_context_is_actual_and_cross_root_stage_or_owner_refuses_before_judge() -> anyhow::Result<()> {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let judge = MockJudge::new(Behavior::Approve);
    let created = in_gate_runtime(
        &fx,
        &proposal,
        std::slice::from_ref(&pin),
        "gate-actual-owner",
        "gate-review",
        gate(
            &fx,
            &proposal,
            &suite,
            std::slice::from_ref(&pin),
            &policy(),
            Some(&judge),
        ),
    )
    .await??;
    let binding = created.gate.decision.experiment.as_ref().expect("actual stage binding");
    assert_eq!(binding.owner_scope, *fx.store.owner_scope());
    assert_eq!(binding.experiment.stage, "gate-review");
    assert_eq!(binding.experiment.input_sha256, "d".repeat(64));
    assert_eq!(binding.physical_scope_id.len(), 64);
    assert_eq!(judge.calls.load(Ordering::SeqCst), 1);
    let path = fx
        .store
        .root()
        .join("gates")
        .join(format!("{}.json", created.gate.decision.record_id));
    let before = std::fs::read(&path)?;
    for (root, stage) in [("gate-other-root", "gate-review"), ("gate-other-stage", "other-review")] {
        let judge = MockJudge::new(Behavior::Approve);
        let result = in_gate_runtime(
            &fx,
            &proposal,
            std::slice::from_ref(&pin),
            root,
            stage,
            gate(
                &fx,
                &proposal,
                &suite,
                std::slice::from_ref(&pin),
                &policy(),
                Some(&judge),
            ),
        )
        .await?;
        assert!(matches!(result, Err(GateError::Unbound(_))));
        assert_eq!(judge.calls.load(Ordering::SeqCst), 0);
    }
    let foreign_workspace = tempfile::tempdir_in(fx.tmp.path())?;
    let foreign = SkillLifecycleStore::open(foreign_workspace.path(), OwnerScope::new("owner:prx:foreign")?)?;
    let judge = MockJudge::new(Behavior::Approve);
    let policy = policy();
    let result = in_gate_runtime(
        &fx,
        &proposal,
        std::slice::from_ref(&pin),
        "gate-foreign-owner",
        "gate-review",
        foreign.gate_proposal(
            &GateRequest {
                proposal_id: &proposal.proposal_id,
                suite_digest: suite.digest(),
                evaluations: std::slice::from_ref(&pin),
                policy: &policy,
            },
            Some(&judge),
            &CancellationToken::new(),
        ),
    )
    .await?;
    assert!(matches!(result, Err(GateError::Unbound(_))));
    assert_eq!(judge.calls.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(path)?, before);
    Ok(())
}

struct ExternalWinnerJudge {
    store: Arc<SkillLifecycleStore>,
    proposal: String,
    suite: String,
    pins: Vec<EvaluationPin>,
}
#[async_trait]
impl GateJudge for ExternalWinnerJudge {
    fn identity(&self) -> String {
        "mock-judge".into()
    }
    async fn review(&self, _brief: &JudgeBrief<'_>, tally: Arc<TallyCounters>) -> Result<JudgeReply, JudgeError> {
        tally.record_reservation();
        let store = self.store.clone();
        let proposal = self.proposal.clone();
        let suite = self.suite.clone();
        let pins = self.pins.clone();
        // Tokio does not inherit the stage task-local. The joined external
        // writer legitimately wins the same epoch without that root binding.
        let winner = tokio::spawn(async move {
            store
                .gate_proposal(
                    &GateRequest {
                        proposal_id: &proposal,
                        suite_digest: &suite,
                        evaluations: &pins,
                        policy: &policy(),
                    },
                    None,
                    &CancellationToken::new(),
                )
                .await
        })
        .await;
        if !matches!(winner, Ok(Ok(_))) {
            return Err(JudgeError::Failed {
                detail: "external gate did not settle".into(),
                usage: tally.snapshot(),
            });
        }
        tally.record_settlement();
        Ok(JudgeReply {
            opinion: JudgeOpinion::Approve,
            usage: tally.snapshot(),
        })
    }
}

#[tokio::test]
async fn concurrent_unbound_winner_cannot_be_mislabelled_as_a_root_gate() -> anyhow::Result<()> {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let judge = ExternalWinnerJudge {
        store: Arc::new(SkillLifecycleStore::open(
            fx.tmp.path(),
            fx.store.owner_scope().clone(),
        )?),
        proposal: proposal.proposal_id.clone(),
        suite: suite.digest().into(),
        pins: vec![pin.clone()],
    };
    let result = in_gate_runtime(
        &fx,
        &proposal,
        std::slice::from_ref(&pin),
        "gate-concurrent-winner",
        "gate-review",
        gate(
            &fx,
            &proposal,
            &suite,
            std::slice::from_ref(&pin),
            &policy(),
            Some(&judge),
        ),
    )
    .await?;
    assert!(matches!(result, Err(GateError::Unbound(_))));
    let stored = fx
        .store
        .recorded_gate_for(&proposal)?
        .expect("external winner remains sealed");
    assert!(
        stored.decision.experiment.is_none(),
        "the winner is never rebound to this root"
    );
    assert_eq!(stored.decision.decision, GateDecision::Accept);
    assert_eq!(status(&fx, &proposal), ProposalStatus::Eligible);
    Ok(())
}

async fn switch_epoch_outside_stage(
    store: Arc<SkillLifecycleStore>,
    proposal: String,
    suite: String,
    pins: Vec<EvaluationPin>,
) -> anyhow::Result<SkillProposal> {
    tokio::spawn(async move {
        let unavailable = MockJudge::new(Behavior::Fail);
        store
            .gate_proposal(
                &GateRequest {
                    proposal_id: &proposal,
                    suite_digest: &suite,
                    evaluations: &pins,
                    policy: &policy(),
                },
                Some(&unavailable),
                &CancellationToken::new(),
            )
            .await?;
        store.begin_evaluation(&proposal, &suite, "external new evaluation epoch")?;
        Ok::<_, anyhow::Error>(store.advance_proposal(
            &proposal,
            ProposalStatus::Evaluated,
            "external epoch completed",
        )?)
    })
    .await?
}

#[tokio::test]
async fn epoch_switch_after_runtime_binding_before_gate_capture_sends_no_judge_request() -> anyhow::Result<()> {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let store = Arc::new(SkillLifecycleStore::open(
        fx.tmp.path(),
        fx.store.owner_scope().clone(),
    )?);
    let judge = MockJudge::new(Behavior::Approve);
    let result = in_gate_runtime(
        &fx,
        &proposal,
        std::slice::from_ref(&pin),
        "gate-before-capture",
        "gate-review",
        async {
            let changed = switch_epoch_outside_stage(
                store,
                proposal.proposal_id.clone(),
                suite.digest().into(),
                vec![pin.clone()],
            )
            .await?;
            assert!(changed.evaluation_epoch > proposal.evaluation_epoch);
            Ok::<_, anyhow::Error>(
                gate(
                    &fx,
                    &proposal,
                    &suite,
                    std::slice::from_ref(&pin),
                    &policy(),
                    Some(&judge),
                )
                .await,
            )
        },
    )
    .await??;
    assert!(matches!(result, Err(GateError::Unbound(_))));
    assert_eq!(
        judge.calls.load(Ordering::SeqCst),
        0,
        "actual core capture refuses a new epoch before constructing judge inputs"
    );
    Ok(())
}

struct EpochSwitchingJudge {
    store: Arc<SkillLifecycleStore>,
    proposal: String,
    suite: String,
    pins: Vec<EvaluationPin>,
    seen_epoch: AtomicU32,
}
#[async_trait]
impl GateJudge for EpochSwitchingJudge {
    fn identity(&self) -> String {
        "mock-judge".into()
    }
    async fn review(&self, brief: &JudgeBrief<'_>, tally: Arc<TallyCounters>) -> Result<JudgeReply, JudgeError> {
        self.seen_epoch.store(brief.proposal.evaluation_epoch, Ordering::SeqCst);
        tally.record_reservation();
        if switch_epoch_outside_stage(
            self.store.clone(),
            self.proposal.clone(),
            self.suite.clone(),
            self.pins.clone(),
        )
        .await
        .is_err()
        {
            return Err(JudgeError::Failed {
                detail: "external epoch switch failed".into(),
                usage: tally.snapshot(),
            });
        }
        tally.record_settlement();
        Ok(JudgeReply {
            opinion: JudgeOpinion::Approve,
            usage: tally.snapshot(),
        })
    }
}

#[tokio::test]
async fn epoch_switch_after_capture_keeps_exact_old_judge_subject_and_refuses_settlement() -> anyhow::Result<()> {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let judge = EpochSwitchingJudge {
        store: Arc::new(SkillLifecycleStore::open(
            fx.tmp.path(),
            fx.store.owner_scope().clone(),
        )?),
        proposal: proposal.proposal_id.clone(),
        suite: suite.digest().into(),
        pins: vec![pin.clone()],
        seen_epoch: AtomicU32::new(0),
    };
    let result = in_gate_runtime(
        &fx,
        &proposal,
        std::slice::from_ref(&pin),
        "gate-after-capture",
        "gate-review",
        gate(
            &fx,
            &proposal,
            &suite,
            std::slice::from_ref(&pin),
            &policy(),
            Some(&judge),
        ),
    )
    .await?;
    assert!(matches!(result, Err(GateError::Unbound(_))));
    assert_eq!(
        judge.seen_epoch.load(Ordering::SeqCst),
        proposal.evaluation_epoch,
        "the judge only sees the admitted immutable clone"
    );
    let current = fx.store.read_proposal(&proposal.proposal_id)?.unwrap();
    assert!(current.evaluation_epoch > proposal.evaluation_epoch);
    assert_eq!(
        current.status,
        ProposalStatus::Evaluated,
        "old subject cannot settle the new epoch eligible"
    );
    assert!(
        fx.store.recorded_gate_for(&current)?.is_none(),
        "no decision is fabricated for the new epoch"
    );
    Ok(())
}

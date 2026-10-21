//! The steps the scenarios share, each one through the production entry:
//! capturing failed tasks, consolidating, proposing, and driving a proposal
//! through review, evaluation and the gate.

use super::support::{ScriptedProposer, World, complete_observations, exp_id, read_skill_in_task, settle, skill};
use crate::config::Config;
use crate::skills::lifecycle::{
    EvalSplit, EvaluationPin, GateOutcome, KnowledgePattern, ProposeOutcome, ProposeRequest, ProposerBudget,
    SkillProposal, SkillsLifecycleService, TerminalKind, propose_revision,
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Runs `count` tasks that read the managed skill through `skill_read` and end
/// as failures with complete, attributable observations (the provider served,
/// nothing was refused, the declared tool was called). Returns the experience
/// ids.
pub(super) async fn capture_failed_tasks(world: &World, config: &Config, prefix: &str, count: usize) -> Vec<String> {
    let mut ids = Vec::new();
    for n in 0..count {
        let task = format!("{prefix}-{n}");
        let read = read_skill_in_task(world, config, &task).await;
        assert_eq!(read.get("managed"), Some(&serde_json::json!(true)), "{read}");
        settle(world, &task, TerminalKind::Failed, Some(complete_observations())).await;
        ids.push(exp_id(world, &task));
    }
    ids
}

pub(super) fn budget() -> ProposerBudget {
    ProposerBudget::new(64 * 1024, 16 * 1024, Duration::from_secs(5)).expect("test: budget")
}

pub(super) async fn propose(
    world: &World,
    request_id: &str,
    pattern: &KnowledgePattern,
    proposer: &ScriptedProposer,
) -> ProposeOutcome {
    propose_revision(
        std::sync::Arc::clone(&world.store),
        ProposeRequest {
            request_id: request_id.to_string(),
            skill_id: skill(),
            pattern_ids: vec![pattern.pattern_id.clone()],
            budget: budget(),
        },
        proposer,
        &CancellationToken::new(),
    )
    .await
    .expect("test: propose")
}

/// The staged proposal of an outcome that must have staged one.
pub(super) fn staged(outcome: ProposeOutcome) -> SkillProposal {
    match outcome {
        ProposeOutcome::Staged(proposal) => proposal,
        other => panic!("test: expected a staged proposal, got {other:?}"),
    }
}

/// Source review, evaluation of both splits through the real agent entry, and
/// the gate, all through the service. Returns the pins and the gate outcome.
pub(super) async fn review_evaluate_gate(
    world: &World,
    service: &SkillsLifecycleService,
    suite: &crate::skills::lifecycle::FrozenSuite,
    proposal_id: &str,
) -> (Vec<EvaluationPin>, GateOutcome) {
    service.install_operator_suite(suite).expect("test: install suite");
    let review = service.review_source(proposal_id, "operator").expect("test: review");
    assert!(
        review.record.passed,
        "source review failed: {:?}",
        review.record.findings
    );
    service
        .begin_evaluation(proposal_id, "acceptance")
        .expect("test: begin");
    let executor = world.executor();
    let mut pins = Vec::new();
    for split in [EvalSplit::Validation, EvalSplit::Holdout] {
        let recorded = service
            .evaluate(proposal_id, split, &executor, &CancellationToken::new())
            .await
            .expect("test: evaluate");
        pins.push(EvaluationPin {
            evaluation_id: recorded.receipt.record_id.clone(),
            receipt_digest: recorded.receipt_digest,
        });
    }
    service
        .finish_evaluation(proposal_id, "evaluations recorded")
        .expect("test: finish");
    let outcome = service
        .gate(proposal_id, &pins, None, &CancellationToken::new())
        .await
        .expect("test: gate");
    (pins, outcome)
}

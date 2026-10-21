#![cfg(test)]

//! Scenario C: reject a plausible harmful change. Each candidate fixes the
//! failing release rule, so it looks like an improvement, and each one does
//! something else that the frozen suite or the capability check must catch.
//! In every case the active revision and policy stay as they were and the
//! reasons stay on record.

use super::flows::{capture_failed_tasks, propose, review_evaluate_gate, staged};
use super::support::{
    REQUIRED_TOOL, ScriptedProposer, World, consolidate, multi_patch, notes_suite, nth, skill, skill_text,
    without_hotfix,
};
use crate::config::skills_lifecycle::SkillsLifecycleModeConfig;
use crate::skills::lifecycle::{
    EvalSplit, EvaluationPin, EvaluationRequest, GateError, GateReason, KnowledgePattern, ProposalFailureKind,
    ProposalStatus, ProposeOutcome, RevisionId, ServiceError, SuppressionStage,
};
use tokio_util::sync::CancellationToken;

const OLD_RELEASE: &str = "RULE release => changelog-v1";
const NEW_RELEASE: &str = "RULE release => changelog-v2";

struct Setup {
    world: World,
    a: RevisionId,
    suite: crate::skills::lifecycle::FrozenSuite,
    service: std::sync::Arc<crate::skills::lifecycle::SkillsLifecycleService>,
    pattern: KnowledgePattern,
}

async fn setup() -> Setup {
    let world = World::new().await;
    let a = world.onboard(&skill_text(&[
        ("hotfix", "hotfix-v1"),
        ("legacy", "legacy-v1"),
        ("release", "changelog-v1"),
    ]));
    let suite = notes_suite(&world.owner, "changelog-v2", "hotfix-v1");
    let auto = world.production_config(SkillsLifecycleModeConfig::Auto, &[(&skill(), &suite)]);
    let service = world.service(&auto);
    let evidence = capture_failed_tasks(&world, &auto, "wrong-rule", 2).await;
    let pattern = consolidate(&world, "The release rule names the wrong token.", &evidence);
    Setup {
        world,
        a,
        suite,
        service,
        pattern,
    }
}

fn assert_nothing_changed(s: &Setup) {
    let binding = s
        .world
        .store
        .active_binding(&skill())
        .expect("test: binding")
        .expect("test: bound");
    assert_eq!(
        (binding.revision_id.clone(), binding.generation),
        (s.a.clone(), 1),
        "the active revision is untouched"
    );
    let a = s.world.store.read_revision(&skill(), &s.a, None).expect("test: A");
    assert_eq!(
        a.revision.capability_requirements,
        vec![REQUIRED_TOOL.to_string()],
        "the policy inputs are untouched"
    );
}

/// The patch that fixes the release rule and quietly breaks the critical hotfix rule.
fn hotfix_breaking_patch() -> String {
    multi_patch(
        "fix the release rule",
        &[
            (OLD_RELEASE, NEW_RELEASE),
            ("RULE hotfix => hotfix-v1", "RULE hotfix => hotfix-v0"),
        ],
    )
}

/// Proposes the hotfix-breaking change, evaluates the validation split with
/// the `rigged` suite instead of the registered one, and gates the result.
/// Returns the rigged evaluation's summary, the gate result and the proposal id.
async fn gate_with_a_rigged_suite(
    s: &Setup,
    rigged: &crate::skills::lifecycle::FrozenSuite,
) -> (
    crate::skills::lifecycle::EvaluationSummary,
    Result<crate::skills::lifecycle::GateOutcome, ServiceError>,
    String,
) {
    let patch = hotfix_breaking_patch();
    let proposer = ScriptedProposer::new(vec![patch.clone(), patch]);
    let proposal = staged(propose(&s.world, "req-c1", &s.pattern, &proposer).await);

    // The service refuses to even store a suite that is not the registered one.
    assert_ne!(rigged.digest(), s.suite.digest());
    assert!(matches!(
        s.service.install_operator_suite(rigged),
        Err(ServiceError::SuiteNotRegistered { .. })
    ));
    s.service.install_operator_suite(&s.suite).expect("test: install suite");
    s.service
        .review_source(&proposal.proposal_id, "operator")
        .expect("test: review");
    s.service
        .begin_evaluation(&proposal.proposal_id, "acceptance")
        .expect("test: begin");
    let candidate = proposal.candidate_revision.clone().expect("test: candidate");
    let recorded = s
        .world
        .store
        .evaluate(
            &EvaluationRequest {
                suite: rigged,
                split: EvalSplit::Validation,
                baseline: Some(s.a.clone()),
                candidate,
                observed_generation: proposal.expected_generation,
            },
            &s.world.executor(),
            &CancellationToken::new(),
        )
        .await
        .expect("test: rigged evaluation");
    let verified = s
        .world
        .store
        .verify_evaluation(&recorded.receipt.record_id, Some(&recorded.receipt_digest))
        .expect("test: verify the rigged evaluation");
    let pin = EvaluationPin {
        evaluation_id: recorded.receipt.record_id.clone(),
        receipt_digest: recorded.receipt_digest,
    };
    s.service
        .finish_evaluation(&proposal.proposal_id, "rigged")
        .expect("test: finish");
    let gated = s
        .service
        .gate(&proposal.proposal_id, &[pin], None, &CancellationToken::new())
        .await;
    (verified.summary, gated, proposal.proposal_id)
}

fn assert_not_eligible(s: &Setup, proposal_id: &str) {
    assert_ne!(
        s.world
            .store
            .read_proposal(proposal_id)
            .expect("test: read")
            .expect("test: present")
            .status,
        ProposalStatus::Eligible
    );
    assert_nothing_changed(s);
}

#[tokio::test]
async fn a_suite_with_the_critical_cases_filtered_out_cannot_gate_a_proposal() {
    let s = setup().await;
    let filtered = without_hotfix(&s.suite);
    let (summary, gated, id) = gate_with_a_rigged_suite(&s, &filtered).await;
    assert_eq!(
        summary.critical_regressions, 0,
        "the filtered suite cannot see the regression"
    );
    assert!(
        matches!(&gated, Err(ServiceError::Gate(GateError::SuiteChanged(_)))),
        "{gated:?}"
    );
    assert_not_eligible(&s, &id);
}

#[tokio::test]
async fn a_suite_with_a_renamed_critical_case_cannot_gate_a_proposal() {
    let s = setup().await;
    // Same cases and prompts; the critical hotfix cases carry other ids, as if
    // the regression were filed under a case the policy does not know.
    let mut altered = s.suite.suite().clone();
    for case in altered.cases.iter_mut().filter(|case| case.task_family == "hotfix") {
        case.case_id = format!("{}-renamed", case.case_id);
    }
    let renamed = altered.freeze().expect("test: freeze the renamed suite");
    let (summary, gated, id) = gate_with_a_rigged_suite(&s, &renamed).await;
    assert!(
        summary.critical_regressions >= 1,
        "the regression is still measurable: {summary:?}"
    );
    assert!(
        matches!(&gated, Err(ServiceError::Gate(GateError::SuiteChanged(_)))),
        "{gated:?}"
    );
    assert_not_eligible(&s, &id);
}

#[tokio::test]
async fn a_suite_with_a_renamed_critical_family_cannot_gate_a_proposal() {
    let s = setup().await;
    // The hotfix family is renamed and loses its critical mark: this is the
    // rigging that would let the regression through if the suite content, not
    // its frozen digest, decided what is critical.
    let mut altered = s.suite.suite().clone();
    for case in altered.cases.iter_mut().filter(|case| case.task_family == "hotfix") {
        case.task_family = "notes".to_string();
        case.critical = false;
    }
    let renamed = altered.freeze().expect("test: freeze the renamed suite");
    let (summary, gated, id) = gate_with_a_rigged_suite(&s, &renamed).await;
    assert_eq!(
        summary.critical_regressions, 0,
        "the renamed family hides the critical regression"
    );
    assert!(
        matches!(&gated, Err(ServiceError::Gate(GateError::SuiteChanged(_)))),
        "{gated:?}"
    );
    assert_not_eligible(&s, &id);
}

#[tokio::test]
async fn a_change_that_breaks_a_critical_case_is_rejected_and_the_rejection_is_remembered() {
    let s = setup().await;
    let patch = multi_patch(
        "fix the release rule",
        &[
            (OLD_RELEASE, NEW_RELEASE),
            ("RULE hotfix => hotfix-v1", "RULE hotfix => hotfix-v0"),
        ],
    );
    let proposer = ScriptedProposer::new(vec![patch.clone(), patch]);
    let proposal = staged(propose(&s.world, "req-c1", &s.pattern, &proposer).await);
    let (pins, gated) = review_evaluate_gate(&s.world, &s.service, &s.suite, &proposal.proposal_id).await;

    // It improved the development examples and still lost.
    let validation = s
        .world
        .store
        .verify_evaluation(&nth(&pins, 0).evaluation_id, None)
        .expect("test: verify");
    assert!(
        validation.summary.candidate_passed > validation.summary.baseline_passed,
        "the release cases improved"
    );
    assert!(validation.summary.critical_regressions >= 1, "{:?}", validation.summary);
    assert_eq!(gated.proposal.status, ProposalStatus::Rejected);
    assert!(
        gated
            .gate
            .decision
            .reasons
            .iter()
            .any(|r| matches!(r, GateReason::Regression { critical, .. } if *critical >= 1)),
        "{:?}",
        gated.gate.decision.reasons
    );
    assert!(gated.rejection.is_some());
    assert!(
        s.service.publish(&proposal.proposal_id).is_err(),
        "a rejected proposal cannot be published"
    );
    assert_nothing_changed(&s);

    // The reasons stay available to the next proposal, which is suppressed
    // before it can spend a model call.
    let history = s.world.store.rejections_for(&skill()).expect("test: history");
    assert_eq!(history.len(), 1);
    let calls_before = proposer.calls();
    let again = propose(&s.world, "req-c1-retry", &s.pattern, &proposer).await;
    assert!(
        matches!(
            again,
            ProposeOutcome::Suppressed {
                stage: SuppressionStage::BeforeCall,
                ..
            }
        ),
        "{again:?}"
    );
    assert_eq!(proposer.calls(), calls_before, "no model call was made");
}

#[tokio::test]
async fn a_change_that_only_fails_the_holdout_is_rejected() {
    let s = setup().await;
    // Validation has no legacy case; the holdout does.
    let patch = multi_patch(
        "fix the release rule",
        &[
            (OLD_RELEASE, NEW_RELEASE),
            ("RULE legacy => legacy-v1", "RULE legacy => legacy-v0"),
        ],
    );
    let proposer = ScriptedProposer::new(vec![patch]);
    let proposal = staged(propose(&s.world, "req-c3", &s.pattern, &proposer).await);
    let (pins, gated) = review_evaluate_gate(&s.world, &s.service, &s.suite, &proposal.proposal_id).await;
    let validation = s
        .world
        .store
        .verify_evaluation(&nth(&pins, 0).evaluation_id, None)
        .expect("test: verify");
    assert_eq!(validation.summary.regressed, 0, "validation looks clean");
    assert!(validation.summary.candidate_passed > validation.summary.baseline_passed);
    let holdout = s
        .world
        .store
        .verify_evaluation(&nth(&pins, 1).evaluation_id, None)
        .expect("test: verify");
    assert_eq!(holdout.summary.regressed, 1);
    assert_eq!(gated.proposal.status, ProposalStatus::Rejected);
    assert!(
        gated.gate.decision.reasons.iter().any(|r| matches!(
            r,
            GateReason::Regression {
                split: EvalSplit::Holdout,
                ..
            }
        )),
        "{:?}",
        gated.gate.decision.reasons
    );
    assert_nothing_changed(&s);
}

#[tokio::test]
async fn a_change_that_widens_the_required_tools_never_becomes_eligible() {
    let s = setup().await;
    let mode_before = s.service.mode();
    let settings_before = format!("{:?}", s.service.settings());
    let widened = format!("required-tools: [{REQUIRED_TOOL}, filesystem:native:shell]");
    let patch = multi_patch(
        "fix the release rule and use the shell",
        &[
            (OLD_RELEASE, NEW_RELEASE),
            (&format!("required-tools: [{REQUIRED_TOOL}]"), &widened),
        ],
    );
    let proposer = ScriptedProposer::new(vec![patch]);
    let outcome = propose(&s.world, "req-c2", &s.pattern, &proposer).await;
    let ProposeOutcome::Failed(proposal) = outcome else {
        panic!("test: expected a failed proposal, got {outcome:?}");
    };
    assert_eq!(proposal.status, ProposalStatus::Failed);
    assert!(proposal.candidate_revision.is_none(), "nothing was staged");

    // The refusal and its reason are on record in the store, not just in the
    // returned value.
    let stored = s
        .world
        .store
        .read_proposal(&proposal.proposal_id)
        .expect("test: read")
        .expect("test: the refused proposal is persisted");
    assert_eq!(stored.status, ProposalStatus::Failed);
    assert!(stored.candidate_revision.is_none() && stored.diff.is_none());
    let failure = stored.failure.expect("test: the reason is persisted");
    // The frontmatter, where the required tools live, is not text the
    // proposer can edit: its replace target is not in the body.
    assert_eq!(failure.kind, ProposalFailureKind::PatchNotApplicable);
    assert!(
        failure.detail.contains("does not occur in the body"),
        "{}",
        failure.detail
    );
    let listed = s.service.list_proposals(Some(&skill())).expect("test: list");
    assert!(
        listed
            .iter()
            .any(|p| p.proposal_id == proposal.proposal_id && p.status == "failed"),
        "{listed:?}"
    );

    // The service mode and the policy did not move.
    assert_eq!(s.service.mode(), mode_before);
    assert_eq!(format!("{:?}", s.service.settings()), settings_before);
    assert_nothing_changed(&s);
}

#[tokio::test]
async fn a_candidate_whose_manifest_widens_the_required_tools_is_refused_by_the_capability_check() {
    use crate::skills::lifecycle::{LifecycleError, NewRevision, SourceKind};
    let s = setup().await;
    let patch = multi_patch("fix the release rule", &[(OLD_RELEASE, NEW_RELEASE)]);
    let proposer = ScriptedProposer::new(vec![patch]);
    let proposal = staged(propose(&s.world, "req-c5", &s.pattern, &proposer).await);

    // A candidate revision that a proposer could not produce, written by another
    // path: its manifest and record add the shell to the baseline's tools.
    let base = s.world.store.read_revision(&skill(), &s.a, None).expect("test: A");
    let text = String::from_utf8(base.bundle.file("SKILL.md").expect("test: SKILL.md").bytes().to_vec())
        .expect("test: utf8")
        .replace(
            &format!("required-tools: [{REQUIRED_TOOL}]"),
            &format!("required-tools: [{REQUIRED_TOOL}, filesystem:native:shell]"),
        );
    let widened = crate::skills::lifecycle::Bundle::from_entries(vec![crate::skills::lifecycle::BundleEntry {
        path: "SKILL.md".into(),
        bytes: text.into_bytes(),
        executable: false,
    }])
    .expect("test: bundle");
    let mut new = NewRevision::new(skill(), SourceKind::Learned);
    new.parent_revision_id = Some(s.a.clone());
    new.capability_requirements = vec![REQUIRED_TOOL.to_string(), "filesystem:native:shell".to_string()];
    let written = s.world.store.write_revision(&new, &widened).expect("test: write");
    let mut tampered = proposal;
    tampered.candidate_revision = Some(written.revision.revision_id);
    let refused = s.world.store.check_capabilities(&tampered, None);
    assert!(
        matches!(&refused, Err(LifecycleError::CapabilityExpansion { reason, .. })
            if reason.contains("newly requires the capability") && reason.contains("filesystem:native:shell")),
        "{refused:?}"
    );
    assert_nothing_changed(&s);
}

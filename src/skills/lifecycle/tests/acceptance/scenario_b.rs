#![cfg(test)]

//! Scenario B: improve an existing skill. Revision A carries a wrong release
//! rule; repeated failures lead to a bounded revision B that changes that one
//! line. A and B are compared on the same frozen suite (critical and negative
//! cases included), B is published, new tasks use B, and a task that was
//! already running on A keeps reading A.

use super::flows::{capture_failed_tasks, propose, review_evaluate_gate, staged};
use super::support::{
    Loopback, ScriptedProposer, World, consolidate, notes_suite, nth, replace_patch, skill, skill_text,
};
use crate::config::skills_lifecycle::SkillsLifecycleModeConfig;
use crate::skills::lifecycle::ProposalStatus;
use std::sync::Arc;

const OLD_RULE: &str = "RULE release => changelog-v1";
const NEW_RULE: &str = "RULE release => changelog-v2";

#[tokio::test]
async fn a_bounded_revision_replaces_the_active_one_while_a_running_task_keeps_its_pin() {
    let world = World::new().await;
    let a_text = skill_text(&[
        ("hotfix", "hotfix-v1"),
        ("legacy", "legacy-v1"),
        ("release", "changelog-v1"),
    ]);
    let a = world.onboard(&a_text);
    let suite = notes_suite(&world.owner, "changelog-v2", "hotfix-v1");
    let auto = world.production_config(SkillsLifecycleModeConfig::Auto, &[(&skill(), &suite)]);
    let service = world.service(&auto);

    // A demonstrated procedural failure on A, then the bounded revision.
    let pre = world
        .run_task_selected(&auto, "TASK[release] Write the release notes for build 4.")
        .await
        .expect("test: run on A");
    assert_eq!(pre, "FINAL:changelog-v1", "A follows its own (wrong) rule");
    let evidence = capture_failed_tasks(&world, &auto, "wrong-rule", 2).await;
    let pattern = consolidate(&world, "The release rule names the wrong token.", &evidence);
    let proposer = ScriptedProposer::new(vec![replace_patch("fix the release rule", OLD_RULE, NEW_RULE)]);
    let proposal = staged(propose(&world, "req-b", &pattern, &proposer).await);
    let (pins, gated) = review_evaluate_gate(&world, &service, &suite, &proposal.proposal_id).await;
    assert_eq!(
        gated.proposal.status,
        ProposalStatus::Eligible,
        "{:?}",
        gated.gate.decision.reasons
    );

    // Bounded, and everything else is preserved byte for byte.
    let b = proposal.candidate_revision.clone().expect("test: candidate");
    let before = world.store.read_revision(&skill(), &a, None).expect("test: A");
    let after = world.store.read_revision(&skill(), &b, None).expect("test: B");
    assert_eq!(
        after.bundle.file("SKILL.md").map(|f| f.bytes().to_vec()),
        Some(a_text.replace(OLD_RULE, NEW_RULE).into_bytes()),
        "exactly one line changed"
    );
    assert_eq!(
        before.revision.capability_requirements,
        after.revision.capability_requirements
    );
    assert_eq!(before.bundle.files().len(), after.bundle.files().len());
    assert_eq!(proposal.diff.as_ref().map(|d| d.edits.len()), Some(1));

    // The same frozen suite, both arms, critical and negative cases pass in both.
    for pin in &pins {
        let verified = world
            .store
            .verify_evaluation(&pin.evaluation_id, Some(&pin.receipt_digest))
            .expect("test: verify");
        assert_eq!(verified.receipt.suite_digest, suite.digest());
        assert_eq!(verified.summary.critical_regressions, 0);
        assert_eq!(verified.summary.invalid_runs, 0);
        assert!(verified.summary.candidate_passed > verified.summary.baseline_passed);
        for case in verified
            .cases
            .iter()
            .filter(|c| c.case_id.contains("hotfix") || c.case_id.contains("unrelated"))
        {
            assert!(case.verdict.is_pass(), "{} {:?}", case.case_id, case.verdict);
        }
    }

    // A task that started on A: the second read happens after B went live.
    let start = world.loopback.requests();
    let publisher = Arc::clone(&service);
    let id = proposal.proposal_id.clone();
    // The selection receipt of this run is on record before B goes live.
    let selected = Loopback::selection_gate(start + 1, &world.store);
    world.loopback.set_hook(Some(Arc::new(move |number| {
        selected(number);
        if number == start + 3 {
            publisher.publish(&id).expect("test: publish during the running task");
        }
    })));
    let running = world
        .run_task(&auto, "TASK[release] TWICE Write the release notes for build 41.")
        .await
        .expect("test: running task");
    world.loopback.set_hook(None);
    assert_eq!(running, "FINAL:changelog-v1", "the running task kept reading A");
    let binding = world
        .store
        .active_binding(&skill())
        .expect("test: binding")
        .expect("test: bound");
    assert_eq!(
        (binding.revision_id.clone(), binding.generation),
        (b.clone(), 2),
        "B went live meanwhile"
    );
    {
        let bodies = world.loopback.bodies.lock();
        let last = bodies.last().expect("test: a request");
        assert!(
            last.contains("changelog-v1") && !last.contains("changelog-v2"),
            "no B text reached the running task"
        );
    }
    let runs = world.production_runs(2).await;
    let running_exp = nth(&runs, 1);
    assert_ne!(running_exp.hydration_receipts.len(), 0);
    assert!(running_exp.hydration_receipts.iter().all(|h| h.revision_id == a));
    assert!(
        !running_exp.selection_receipts.is_empty(),
        "the run selected the skill: {running_exp:?}"
    );
    assert!(
        running_exp.selection_receipts.iter().all(|s| s.revision_id == a),
        "the selection receipts name A, the revision selected when the task started"
    );
    assert_eq!(running_exp.late_evidence, 0);

    // New tasks use B.
    let begin = world.loopback.requests();
    world
        .loopback
        .set_hook(Some(Loopback::selection_gate(begin + 1, &world.store)));
    let fresh = world
        .run_task(&auto, "TASK[release] Write the release notes for build 42.")
        .await
        .expect("test: new task");
    world.loopback.set_hook(None);
    assert_eq!(fresh, "FINAL:changelog-v2");
    let latest = world.production_runs(3).await;
    let newest = nth(&latest, 2);
    assert!(newest.hydration_receipts.iter().all(|h| h.revision_id == b));
    assert!(!newest.selection_receipts.is_empty(), "the new task selected the skill");
    assert!(
        newest.selection_receipts.iter().all(|s| s.revision_id == b),
        "the selection receipts name B"
    );
    assert_eq!(
        world
            .store
            .read_proposal(&proposal.proposal_id)
            .expect("test: read")
            .expect("test: present")
            .status,
        ProposalStatus::Committed
    );
    // A stays retained for rollback.
    assert!(world.store.read_revision(&skill(), &a, None).is_ok());
}

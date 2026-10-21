//! Scenario A: create a skill's missing procedure from observed repeated work.
//!
//! The starting skill is a managed stub that knows the hotfix rule only. Real
//! agent runs use it, repeated release tasks fail against it, the failures are
//! consolidated, a bounded revision is proposed (scripted proposer), evaluated
//! against a frozen baseline on validation and holdout through the real agent
//! entry, gated, published under valid authorization, and then unseen tasks run
//! through the same entry on the published revision.
//!
//! Boundary: `propose_revision` revises an active skill; it has no path that
//! creates a skill from nothing, and experiences exist only for hydrated
//! managed skills. "Create" therefore means the stub grows the procedure.

use super::flows::{capture_failed_tasks, propose, review_evaluate_gate, staged};
use super::support::{ScriptedProposer, World, consolidate, notes_suite, nth, replace_patch, skill, skill_text};
use crate::config::skills_lifecycle::SkillsLifecycleModeConfig;
use crate::skills::lifecycle::{ExperienceOutcome, FactState, LifecycleMode, ProposalStatus, ServiceError, SourceKind};

const RELEASE_TOKEN: &str = "changelog-v2";

#[tokio::test]
async fn a_repeated_release_failure_becomes_a_published_revision_that_unseen_tasks_use() {
    let world = World::new().await;
    let stub = skill_text(&[("hotfix", "hotfix-v1"), ("legacy", "legacy-v1")]);
    let a = world.onboard(&stub);
    let suite = notes_suite(&world.owner, RELEASE_TOKEN, "hotfix-v1");
    let auto = world.production_config(SkillsLifecycleModeConfig::Auto, &[(&skill(), &suite)]);
    let service = world.service(&auto);

    // 1. Real work without the procedure. Hotfix tasks succeed through the
    //    agent entry; a release task returns no usable answer.
    let hotfix = world
        .run_task(&auto, "TASK[hotfix] Write the hotfix note for patch 3.")
        .await
        .expect("test: hotfix run");
    assert_eq!(hotfix, "FINAL:hotfix-v1");
    let release = world
        .run_task(&auto, "TASK[release] Write the release notes for build 4.")
        .await
        .expect("test: release run");
    assert!(
        !release.contains(RELEASE_TOKEN),
        "the stub cannot produce the answer: {release}"
    );
    let production = world.production_runs(2).await;
    for experience in &production {
        let terminal = experience.terminal.as_ref().expect("test: terminal joined");
        assert_eq!(terminal.outcome, ExperienceOutcome::Success);
        assert_eq!(experience.hydration_receipts.len(), 1, "hydration recorded");
        assert_eq!(nth(&experience.hydration_receipts, 0).revision_id, a);
    }

    // 2. Repeated failures of release tasks on the stub.
    let evidence = capture_failed_tasks(&world, &auto, "release-fail", 3).await;
    let before: Vec<_> = evidence
        .iter()
        .map(|id| {
            world
                .store
                .read_experience_by_id(id)
                .expect("test: read")
                .expect("test: present")
        })
        .collect();
    for experience in &before {
        let terminal = experience.terminal.as_ref().expect("test: terminal");
        assert_eq!(terminal.outcome, ExperienceOutcome::Failure);
        assert_eq!(terminal.facts.infrastructure_unavailable, FactState::Absent);
        assert_eq!(terminal.facts.permission_refused, FactState::Absent);
    }

    // 3. Consolidation links the pattern to the tasks and leaves the receipts alone.
    let pattern = consolidate(&world, "The skill has no rule for release notes.", &evidence);
    assert!(pattern.usable_for_proposal());
    for id in &evidence {
        assert!(pattern.evidence_ids.contains(id), "the pattern cites {id}");
    }
    let after: Vec<_> = evidence
        .iter()
        .map(|id| {
            world
                .store
                .read_experience_by_id(id)
                .expect("test: read")
                .expect("test: present")
        })
        .collect();
    assert_eq!(before, after, "invariant 3: consolidation never rewrites a receipt");

    // 4. A bounded proposal is staged; nothing is active before the verdict.
    let proposer = ScriptedProposer::new(vec![replace_patch(
        "add the release rule",
        "RULES-END",
        &format!("RULE release => {RELEASE_TOKEN}\nRULES-END"),
    )]);
    let proposal = staged(propose(&world, "req-a", &pattern, &proposer).await);
    assert_eq!(proposer.calls(), 1);
    assert_eq!(proposal.status, ProposalStatus::Draft);
    assert_eq!(proposal.baseline_revision, a);
    assert!(proposal.basis.adherence_verified);
    let binding = world
        .store
        .active_binding(&skill())
        .expect("test: binding")
        .expect("test: bound");
    assert_eq!((binding.revision_id.clone(), binding.generation), (a.clone(), 1));

    // 5. Review, frozen evaluation and the gate. Shadow mode stops at Eligible.
    let (pins, gated) = review_evaluate_gate(&world, &service, &suite, &proposal.proposal_id).await;
    assert_eq!(
        gated.proposal.status,
        ProposalStatus::Eligible,
        "{:?}",
        gated.gate.decision.reasons
    );
    let validation = world
        .store
        .verify_evaluation(&nth(&pins, 0).evaluation_id, Some(&nth(&pins, 0).receipt_digest))
        .expect("test: verify");
    assert_eq!(
        validation.cases.len(),
        2 * 5,
        "every case ran in both arms, none filtered"
    );
    assert_eq!(validation.summary.candidate_passed, 5);
    assert_eq!(
        validation.summary.baseline_passed, 2,
        "hotfix and unrelated pass without the rule"
    );
    assert_eq!(validation.summary.critical_regressions, 0);
    let holdout = world
        .store
        .verify_evaluation(&nth(&pins, 1).evaluation_id, Some(&nth(&pins, 1).receipt_digest))
        .expect("test: verify");
    assert_eq!(holdout.cases.len(), 2 * 6);
    assert_eq!(holdout.summary.regressed, 0);

    let shadow_config = world.production_config(SkillsLifecycleModeConfig::Shadow, &[(&skill(), &suite)]);
    let shadow = world.service(&shadow_config);
    assert!(matches!(
        shadow.publish(&proposal.proposal_id),
        Err(ServiceError::ModeForbids {
            mode: LifecycleMode::Shadow,
            ..
        })
    ));
    assert_eq!(
        world
            .store
            .active_binding(&skill())
            .expect("test: binding")
            .expect("test: bound")
            .generation,
        1,
        "shadow never publishes"
    );

    // 6. Auto publication under the valid policy.
    let published = service.publish(&proposal.proposal_id).expect("test: publish");
    assert_eq!(published.proposal.status, ProposalStatus::Committed);
    let binding = world
        .store
        .active_binding(&skill())
        .expect("test: binding")
        .expect("test: bound");
    assert_eq!(binding.generation, 2);
    let candidate = proposal.candidate_revision.clone().expect("test: candidate");
    assert_eq!(binding.revision_id, candidate);
    let revision = world
        .store
        .read_revision(&skill(), &candidate, None)
        .expect("test: revision");
    assert_eq!(revision.revision.source_kind, SourceKind::Learned);
    assert_eq!(revision.revision.parent_revision_id.as_ref(), Some(&a));
    assert_eq!(
        revision.revision.proposal_id.as_deref(),
        Some(proposal.proposal_id.as_str())
    );

    // 7. Unseen tasks through the production entry use the published revision.
    for build in [31, 32, 33] {
        let answer = world
            .run_task(
                &auto,
                &format!("TASK[release] Write the release notes for build {build}."),
            )
            .await
            .expect("test: unseen run");
        assert_eq!(answer, format!("FINAL:{RELEASE_TOKEN}"));
    }
    assert_eq!(
        world
            .run_task(&auto, "TASK[hotfix] Write the hotfix note for patch 9.")
            .await
            .expect("test: hotfix"),
        "FINAL:hotfix-v1",
        "the critical behavior survived"
    );
    assert_eq!(
        world
            .run_task(&auto, "What is 40 plus 2?")
            .await
            .expect("test: negative"),
        "plain answer"
    );
    let served: Vec<_> = world
        .production_runs(6)
        .await
        .into_iter()
        .filter(|e| e.hydration_receipts.iter().any(|h| h.revision_id == candidate))
        .collect();
    assert_eq!(served.len(), 4, "the four skill tasks hydrated the published revision");
    assert!(served.iter().all(|e| {
        e.terminal
            .as_ref()
            .is_some_and(|t| t.outcome == ExperienceOutcome::Success)
    }));

    // 8. The chain: tasks -> pattern -> proposal -> revision -> binding.
    assert_eq!(proposal.pattern_ids, vec![pattern.pattern_id.clone()]);
    for id in &evidence {
        assert!(proposal.evidence_ids.contains(id), "the proposal cites {id}");
    }
    let record = world
        .store
        .read_proposal(&proposal.proposal_id)
        .expect("test: read")
        .expect("test: present");
    assert_eq!(
        record.publication.as_ref().map(|p| p.revision_id.clone()),
        Some(candidate)
    );
    assert!(
        world
            .loopback
            .bodies
            .lock()
            .iter()
            .all(|body| !body.contains(&format!("FINAL:{RELEASE_TOKEN}"))),
        "the expected answer never reached the model"
    );
}

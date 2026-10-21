#![cfg(test)]

//! Scenario A: create a skill from observed repeated work, starting from NO
//! skill (spec section 17 A, finding 108.1).
//!
//! Nothing exists at the start. Real tasks run through the agent entry
//! (`crate::agent::run`) without any managed skill and complete (this fixture's
//! replies fail the verifiable check; production has no such check); the
//! production terminal leaves an attributable experience for each.
//! Those experiences are consolidated into a pattern of uncovered work, a NEW
//! instruction skill is proposed from it (provenance, no baseline revision),
//! evaluated against a frozen no-skill baseline on validation and holdout
//! through the real agent entry, gated, published under the same authorization
//! as a revision (creating the skill and its initial binding), and then unseen
//! tasks run through the same entry on the new skill.
//!
//! Nothing here claims that PRX learns (invariant 11): the model is a loopback
//! fixture and the proposer is scripted.

use super::flows::{new_skill_request, propose_new, review_evaluate_gate, staged};
use super::support::{
    REQUIRED_TOOL, ScriptedProposer, World, append_patch, consolidate_uncovered, notes_suite, nth, skill,
    try_consolidate_uncovered,
};
use crate::config::skills_lifecycle::SkillsLifecycleModeConfig;
use crate::skills::lifecycle::{
    EditOp, ExperienceOutcome, FactState, FailureClass, LifecycleMode, ProposalOperation, ProposalStatus, ServiceError,
    SourceKind,
};

const RELEASE_TOKEN: &str = "changelog-v2";
const NEW_SKILL_TEXT: &str = "RULE release => changelog-v2\nRULE hotfix => hotfix-v1\nRULE legacy => legacy-v1\n";

#[tokio::test]
async fn repeated_work_without_a_skill_becomes_a_published_skill_that_unseen_tasks_use() {
    let world = World::new().await;
    let suite = notes_suite(&world.owner, RELEASE_TOKEN, "hotfix-v1");
    let auto = world.production_config(SkillsLifecycleModeConfig::Auto, &[(&skill(), &suite)]);
    let service = world.service(&auto);
    assert!(
        world.store.active_binding(&skill()).expect("test: binding").is_none(),
        "the world starts with no skill"
    );

    // 1. Real work without any skill. These are completed turns; that the
    //    replies lack the release token is a property of the fixture model.
    //    The terminal path records each task.
    for build in [4, 5, 6] {
        let answer = world
            .run_task(
                &auto,
                &format!("TASK[release] Write the release notes for build {build}."),
            )
            .await
            .expect("test: run without a skill");
        assert!(!answer.contains(RELEASE_TOKEN), "no skill, no procedure: {answer}");
    }
    let runs = world.uncovered_runs(3).await;
    let evidence: Vec<String> = runs.iter().map(|run| run.record_id.clone()).collect();
    for run in &runs {
        let terminal = run.terminal.as_ref().expect("test: the terminal joined the task");
        assert_eq!(
            terminal.outcome,
            ExperienceOutcome::Success,
            "the turn itself completed"
        );
        assert_eq!(terminal.facts.infrastructure_unavailable, FactState::Absent);
        assert_eq!(terminal.facts.permission_refused, FactState::Absent);
        assert!(terminal.facts.adherence.is_empty(), "no skill, nothing to adhere to");
        assert!(run.learning_eligible());
        assert_eq!((run.dropped_evidence, run.late_evidence), (0, 0));
    }

    // 2. Consolidation: a pattern of uncovered work, from three distinct root
    //    tasks, that cannot back a revision (it is about no skill).
    let pattern = consolidate_uncovered(&world, "Release notes recur and no skill covers them.", &evidence);
    assert_eq!(pattern.failure_class, FailureClass::MissingSkill);
    assert!(pattern.skill_id.is_none());
    assert!(pattern.usable_for_new_skill());
    assert!(
        !pattern.usable_for_proposal(),
        "an uncovered pattern is not a revision pattern"
    );
    assert_eq!(pattern.confidence_basis.distinct_tasks, 3);
    for id in &evidence {
        assert!(pattern.evidence_ids.contains(id), "the pattern cites {id}");
    }
    assert_eq!(
        runs,
        world.uncovered_runs(3).await,
        "invariant 3: consolidation never rewrites a receipt"
    );

    // 3. A bounded NEW skill is staged; nothing is active before the verdict.
    let proposer = ScriptedProposer::new(vec![append_patch("write the release procedure", NEW_SKILL_TEXT)]);
    let proposal = staged(
        propose_new(&world, new_skill_request("req-a", &pattern), &proposer)
            .await
            .expect("test: propose a new skill"),
    );
    assert_eq!(proposer.calls(), 1);
    assert_eq!(proposal.operation, ProposalOperation::Create);
    assert_eq!(proposal.status, ProposalStatus::Draft);
    assert!(
        proposal.baseline_revision.is_none(),
        "a new skill has no baseline revision"
    );
    assert_eq!(proposal.expected_generation, 0);
    assert!(!proposal.basis.adherence_verified);
    assert_eq!(proposal.basis.failed_tasks_on_baseline, 3);
    let diff = proposal.diff.as_ref().expect("test: a bounded diff");
    assert_eq!(diff.edits.len(), 1);
    assert_eq!(nth(&diff.edits, 0).op, EditOp::Append);
    assert!(diff.changed_tokens <= diff.allowed_tokens && diff.allowed_tokens == 300);
    assert!(
        world.store.active_binding(&skill()).expect("test: binding").is_none(),
        "staging activates nothing"
    );

    // 4. Review, frozen evaluation against the NO-SKILL baseline, the gate.
    let (pins, gated) = review_evaluate_gate(&world, &service, &suite, &proposal.proposal_id).await;
    assert_eq!(
        gated.proposal.status,
        ProposalStatus::Eligible,
        "{:?}",
        gated.gate.decision.reasons
    );
    assert!(gated.gate.decision.baseline_revision.is_none());
    let validation = world
        .store
        .verify_evaluation(&nth(&pins, 0).evaluation_id, Some(&nth(&pins, 0).receipt_digest))
        .expect("test: verify");
    assert!(
        validation.receipt.baseline.is_none(),
        "the baseline arm is no skill at all"
    );
    assert_eq!(validation.receipt.observed_generation, 0);
    assert_eq!(
        validation.cases.len(),
        2 * 5,
        "every case ran in both arms, none filtered"
    );
    assert_eq!(validation.summary.candidate_passed, 5);
    assert_eq!(
        validation.summary.baseline_passed, 1,
        "only the negative case passes without a skill"
    );
    assert_eq!(
        (
            validation.summary.critical_regressions,
            validation.summary.critical_failures
        ),
        (0, 0)
    );
    let holdout = world
        .store
        .verify_evaluation(&nth(&pins, 1).evaluation_id, Some(&nth(&pins, 1).receipt_digest))
        .expect("test: verify");
    assert!(holdout.receipt.baseline.is_none());
    assert_eq!(holdout.cases.len(), 2 * 6, "the holdout ran too, legacy case included");
    assert_eq!(
        (holdout.summary.candidate_passed, holdout.summary.baseline_passed),
        (6, 1)
    );
    assert_eq!(holdout.summary.regressed, 0);

    // 5. DraftOnly and Shadow never publish; nothing changes.
    for (mode, expected) in [
        (SkillsLifecycleModeConfig::DraftOnly, LifecycleMode::DraftOnly),
        (SkillsLifecycleModeConfig::Shadow, LifecycleMode::Shadow),
    ] {
        let restricted = world.production_config(mode, &[(&skill(), &suite)]);
        let refused = world.service(&restricted).publish(&proposal.proposal_id);
        assert!(
            matches!(&refused, Err(ServiceError::ModeForbids { mode, .. }) if *mode == expected),
            "{refused:?}"
        );
    }
    assert!(
        world.store.active_binding(&skill()).expect("test: binding").is_none(),
        "no restricted mode created the skill"
    );

    // 6. Auto publication under the valid policy creates the managed skill and
    //    its initial binding in one journaled transaction.
    let published = service.publish(&proposal.proposal_id).expect("test: publish");
    assert_eq!(published.proposal.status, ProposalStatus::Committed);
    let binding = world
        .store
        .active_binding(&skill())
        .expect("test: binding")
        .expect("test: the skill is bound now");
    assert_eq!(binding.generation, 1);
    let revision_id = proposal.candidate_revision.clone().expect("test: candidate");
    assert_eq!(binding.revision_id, revision_id);
    assert!(binding.enabled);
    let revision = world
        .store
        .read_revision(&skill(), &revision_id, Some(&binding.record_digest))
        .expect("test: revision");
    assert_eq!(revision.revision.source_kind, SourceKind::Learned);
    assert!(revision.revision.parent_revision_id.is_none());
    assert_eq!(
        revision.revision.proposal_id.as_deref(),
        Some(proposal.proposal_id.as_str())
    );
    assert_eq!(revision.revision.pattern_ids, vec![pattern.pattern_id.to_string()]);
    assert_eq!(
        revision.revision.capability_requirements,
        vec![REQUIRED_TOOL.to_string()]
    );
    assert_eq!(
        revision.bundle.files().iter().map(|f| f.path()).collect::<Vec<_>>(),
        vec!["SKILL.md"],
        "instruction-only"
    );

    // 7. Unseen tasks through the production entry use the new skill.
    for build in [31, 32, 33] {
        let answer = world
            .run_task_selected(
                &auto,
                &format!("TASK[release] Write the release notes for build {build}."),
            )
            .await
            .expect("test: unseen run");
        assert_eq!(answer, format!("FINAL:{RELEASE_TOKEN}"));
    }
    assert_eq!(
        world
            .run_task_selected(&auto, "TASK[hotfix] Write the hotfix note for patch 9.")
            .await
            .expect("test: hotfix"),
        "FINAL:hotfix-v1"
    );
    assert_eq!(
        world
            .run_task(&auto, "What is 40 plus 2?")
            .await
            .expect("test: negative"),
        "plain answer",
        "the negative case does not load the skill"
    );
    let served: Vec<_> = world
        .production_runs(4)
        .await
        .into_iter()
        .filter(|e| e.hydration_receipts.iter().any(|h| h.revision_id == revision_id))
        .collect();
    assert_eq!(served.len(), 4, "the four skill tasks hydrated the published revision");
    assert!(served.iter().all(|e| {
        e.terminal
            .as_ref()
            .is_some_and(|t| t.outcome == ExperienceOutcome::Success)
    }));
    assert!(
        world
            .loopback
            .bodies
            .lock()
            .iter()
            .all(|body| !body.contains(&format!("FINAL:{RELEASE_TOKEN}"))),
        "the expected answer never reached the model"
    );

    // 8. The chain: tasks -> pattern -> proposal -> revision -> binding.
    for id in &evidence {
        let experience = world
            .store
            .read_experience_by_id(id)
            .expect("test: read")
            .expect("test: present");
        assert!(
            experience.hydration_receipts.is_empty(),
            "the evidence ran without the skill"
        );
        assert!(pattern.evidence_ids.contains(id) && proposal.evidence_ids.contains(id));
    }
    assert_eq!(proposal.pattern_ids, vec![pattern.pattern_id.clone()]);
    let record = world
        .store
        .read_proposal(&proposal.proposal_id)
        .expect("test: read")
        .expect("test: present");
    let publication = record.publication.expect("test: the publication is recorded");
    assert_eq!((publication.revision_id, publication.generation), (revision_id, 1));
}

#[tokio::test]
async fn a_completed_run_without_a_skill_is_evidence_but_a_run_that_errors_is_not_recorded() {
    // 108.3. What qualifies is a completed root run that used no skill: the
    // production terminal records it (a completed turn is a Success; the runtime
    // has no task-outcome check) with facts from its own observations, none
    // hand-fed. That this fixture's reply fails the check is a property of the
    // loopback model, not something production sees. A run that ENDS IN AN
    // ERROR would be a Failure, but the runtime labels every agent error as an
    // unknown provider end, so its infrastructure fact is never positively
    // absent: it could never qualify and the terminal leaves no record of it.
    // That production cannot verify a task's outcome stays blocked-internal.
    let world = World::new().await;
    let suite = notes_suite(&world.owner, RELEASE_TOKEN, "hotfix-v1");
    let config = world.production_config(SkillsLifecycleModeConfig::Auto, &[(&skill(), &suite)]);
    let _service = world.service(&config);

    let wrong = world
        .run_task(&config, "TASK[release] Write the release notes for build 7.")
        .await
        .expect("test: a completed run");
    assert!(
        !wrong.contains(RELEASE_TOKEN),
        "the fixture's reply fails the check: {wrong}"
    );
    let failing = world
        .run_task(
            &config,
            "TASK[release] EMPTY-ON-MISSING Write the release notes for build 8.",
        )
        .await;
    assert!(failing.is_err(), "an empty model reply fails the run: {failing:?}");

    let runs = world.uncovered_runs(1).await;
    let completed = nth(&runs, 0);
    let terminal = completed.terminal.as_ref().expect("test: joined");
    assert_eq!(terminal.outcome, ExperienceOutcome::Success);
    assert_eq!(terminal.facts.infrastructure_unavailable, FactState::Absent);
    let kept = try_consolidate_uncovered(
        &world,
        "Release notes recur.",
        std::slice::from_ref(&completed.record_id),
    )
    .expect("test: the completed run qualifies");
    assert_eq!(kept.evidence_ids, vec![completed.record_id.clone()]);
    assert_eq!(world.experiences().len(), 1, "the errored run left no record");
}

#[tokio::test]
async fn a_disabled_lifecycle_records_no_work_even_with_a_store_present() {
    let world = World::new().await;
    let suite = notes_suite(&world.owner, RELEASE_TOKEN, "hotfix-v1");
    let mut config = world.production_config(SkillsLifecycleModeConfig::Auto, &[(&skill(), &suite)]);
    config.skills_lifecycle.enabled = false;
    world
        .run_task(&config, "TASK[release] Write the release notes for build 7.")
        .await
        .expect("test: run");
    // Let any (wrongly) spawned work settle before looking.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(world.experiences().is_empty(), "{:?}", world.experiences());
}

#[tokio::test]
async fn evidence_that_does_not_show_uncovered_work_cannot_back_a_new_skill() {
    let world = World::new().await;
    let suite = notes_suite(&world.owner, RELEASE_TOKEN, "hotfix-v1");
    let auto = world.production_config(SkillsLifecycleModeConfig::Auto, &[(&skill(), &suite)]);
    let _service = world.service(&auto);
    let runs = {
        for build in [4, 5] {
            world
                .run_task(
                    &auto,
                    &format!("TASK[release] Write the release notes for build {build}."),
                )
                .await
                .expect("test: run");
        }
        world.uncovered_runs(2).await
    };
    let evidence: Vec<String> = runs.iter().map(|run| run.record_id.clone()).collect();

    // One distinct task is not enough: the pattern is recorded but cannot back a skill.
    let single = consolidate_uncovered(
        &world,
        "One release task.",
        evidence.get(..1).expect("test: two experiences"),
    );
    assert!(!single.usable_for_new_skill());
    let proposer = ScriptedProposer::new(vec![append_patch("write it", NEW_SKILL_TEXT)]);
    let refused = propose_new(&world, new_skill_request("req-one", &single), &proposer).await;
    assert!(
        matches!(&refused, Err(crate::skills::lifecycle::LifecycleError::ProposalRefused(reason)) if reason.contains("cannot back a new skill")),
        "{refused:?}"
    );
    assert_eq!(proposer.calls(), 0, "refused before the model is called");

    // Work that read a managed skill is not uncovered work.
    let covered = World::new().await;
    covered.onboard(&super::support::skill_text(&[("hotfix", "hotfix-v1")]));
    let covered_config = covered.production_config(SkillsLifecycleModeConfig::Auto, &[]);
    covered
        .run_task_selected(&covered_config, "TASK[hotfix] Write the hotfix note for patch 3.")
        .await
        .expect("test: run on a skill");
    let hydrated = covered.production_runs(1).await;
    let refused = try_consolidate_uncovered(&covered, "Covered work.", &[nth(&hydrated, 0).record_id.clone()]);
    assert!(format!("{refused:?}").contains("covered"), "{refused:?}");
}

#[tokio::test]
async fn a_new_skill_never_overwrites_an_unmanaged_directory_of_the_same_name() {
    let world = World::new().await;
    let suite = notes_suite(&world.owner, RELEASE_TOKEN, "hotfix-v1");
    let auto = world.production_config(SkillsLifecycleModeConfig::Auto, &[(&skill(), &suite)]);
    let service = world.service(&auto);
    // Someone's own skill directory, not managed by the lifecycle, holds the name.
    let own = world.ws.join("skills").join(super::support::SKILL_DIR);
    std::fs::create_dir_all(&own).expect("test: unmanaged dir");
    let own_text = "---\nname: someone-elses\ndescription: Not managed\n---\n\nKeep me.\n";
    std::fs::write(own.join("SKILL.md"), own_text).expect("test: unmanaged skill");

    for build in [4, 5] {
        world
            .run_task(
                &auto,
                &format!("TASK[release] Write the release notes for build {build}."),
            )
            .await
            .expect("test: run without a managed skill");
    }
    let evidence: Vec<String> = world
        .uncovered_runs(2)
        .await
        .iter()
        .map(|r| r.record_id.clone())
        .collect();
    let pattern = consolidate_uncovered(&world, "Release notes recur and no skill covers them.", &evidence);
    let proposer = ScriptedProposer::new(vec![append_patch("write the release procedure", NEW_SKILL_TEXT)]);
    let proposal = staged(
        propose_new(&world, new_skill_request("req-collide", &pattern), &proposer)
            .await
            .expect("test: propose"),
    );
    let (_, gated) = review_evaluate_gate(&world, &service, &suite, &proposal.proposal_id).await;
    assert_eq!(gated.proposal.status, ProposalStatus::Eligible);

    let refused = service.publish(&proposal.proposal_id);
    let text = format!("{refused:?}");
    assert!(refused.is_err() && text.contains("UnsafeStorePath"), "{text}");
    assert!(world.store.active_binding(&skill()).expect("test: binding").is_none());
    assert_eq!(
        std::fs::read_to_string(own.join("SKILL.md")).expect("test: read"),
        own_text,
        "the unmanaged directory is untouched"
    );
}

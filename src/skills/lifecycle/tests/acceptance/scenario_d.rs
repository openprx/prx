//! Scenario D: roll back after a production regression. A published revision
//! passes the frozen suite but silently breaks a task family the suite does not
//! cover (an owned, controlled regression). The operator rolls back through
//! the CLI while a task is running on the bad revision; new tasks read the
//! previous bytes, the bad revision and its failures stay on record, and a
//! crash at either end of a rollback recovers to one auditable binding.

use super::flows::{capture_failed_tasks, propose, review_evaluate_gate, staged};
use super::support::{
    ScriptedProposer, World, consolidate, multi_patch, notes_suite, nth, read_skill_in_task, skill, skill_text, text_of,
};
use crate::SkillLifecycleCommands;
use crate::config::skills_lifecycle::SkillsLifecycleModeConfig;
use crate::skills::lifecycle::publish::crash_hooks;
use crate::skills::lifecycle::{
    Bundle, BundleEntry, ProposalStatus, RevisionId, SkillId, SkillLifecycleStore, SourceKind, TransactionKind,
    TransactionState,
};
use parking_lot::Mutex;
use std::sync::Arc;

async fn cli(
    command: SkillLifecycleCommands,
    service: &Arc<crate::skills::lifecycle::SkillsLifecycleService>,
    config: &crate::config::Config,
) -> anyhow::Result<String> {
    crate::skills::lifecycle_cli::execute(command, Some(Arc::clone(service)), config).await
}

fn rollback_command(to: &RevisionId, generation: u64) -> SkillLifecycleCommands {
    SkillLifecycleCommands::Rollback {
        skill: skill().as_str().to_string(),
        revision: to.as_str().to_string(),
        expected_generation: generation,
    }
}

#[tokio::test]
async fn an_operator_rollback_restores_the_previous_bytes_and_keeps_the_history() {
    let world = World::new().await;
    let a_text = skill_text(&[
        ("hotfix", "hotfix-v1"),
        ("legacy", "legacy-v1"),
        ("audit", "audit-v1"),
        ("release", "changelog-v1"),
    ]);
    let a = world.onboard(&a_text);
    let suite = notes_suite(&world.owner, "changelog-v2", "hotfix-v1");
    let auto = world.production_config(SkillsLifecycleModeConfig::Auto, &[(&skill(), &suite)]);
    let service = world.service(&auto);

    // B fixes the release rule and, unseen by the suite, breaks the audit rule.
    let evidence = capture_failed_tasks(&world, &auto, "wrong-rule", 2).await;
    let pattern = consolidate(&world, "The release rule names the wrong token.", &evidence);
    let proposer = ScriptedProposer::new(vec![multi_patch(
        "fix the release rule",
        &[
            ("RULE release => changelog-v1", "RULE release => changelog-v2"),
            ("RULE audit => audit-v1", "RULE audit => audit-v0"),
        ],
    )]);
    let proposal = staged(propose(&world, "req-d", &pattern, &proposer).await);
    let (_, gated) = review_evaluate_gate(&world, &service, &suite, &proposal.proposal_id).await;
    assert_eq!(
        gated.proposal.status,
        ProposalStatus::Eligible,
        "the suite cannot see the audit family"
    );
    service.publish(&proposal.proposal_id).expect("test: publish B");
    let b = proposal.candidate_revision.clone().expect("test: candidate");
    assert_eq!(
        world
            .store
            .active_binding(&skill())
            .expect("test: binding")
            .expect("test: bound")
            .generation,
        2
    );

    // The production regression signal, and B's failures on record.
    let signal = world
        .run_task(&auto, "TASK[audit] Write the audit note for module 1.")
        .await
        .expect("test: audit on B");
    assert_eq!(signal, "FINAL:audit-v0", "the audit family regressed under B");
    let b_failures = capture_failed_tasks(&world, &auto, "audit-fail", 2).await;
    let snapshot: Vec<_> = b_failures
        .iter()
        .map(|id| {
            world
                .store
                .read_experience_by_id(id)
                .expect("test: read")
                .expect("test: present")
        })
        .collect();
    assert!(
        snapshot
            .iter()
            .all(|e| e.hydration_receipts.iter().all(|h| h.revision_id == b))
    );

    // Roll back through the operator CLI while a task is running on B.
    let start = world.loopback.requests();
    let output: Arc<Mutex<Option<anyhow::Result<String>>>> = Arc::new(Mutex::new(None));
    let (slot, svc, cfg, target) = (Arc::clone(&output), Arc::clone(&service), auto.clone(), a.clone());
    let handle = tokio::runtime::Handle::current();
    world.loopback.set_hook(Some(Arc::new(move |number| {
        if number == start + 3 {
            let result = handle.block_on(cli(rollback_command(&target, 2), &svc, &cfg));
            *slot.lock() = Some(result);
        }
    })));
    let running = world
        .run_task(&auto, "TASK[audit] TWICE Write the audit note for module 2.")
        .await
        .expect("test: running task");
    world.loopback.set_hook(None);
    let text = output
        .lock()
        .take()
        .expect("test: the hook ran")
        .expect("test: rollback via the CLI");
    assert!(text.contains("Rolled") && text.contains("generation 3"), "{text}");
    assert_eq!(running, "FINAL:audit-v0", "the running task stayed on B");
    let runs = world.production_runs(2).await;
    let in_flight = nth(&runs, 1);
    assert!(in_flight.hydration_receipts.iter().all(|h| h.revision_id == b));

    // New tasks read A's bytes under a new generation.
    let binding = world
        .store
        .active_binding(&skill())
        .expect("test: binding")
        .expect("test: bound");
    assert_eq!((binding.revision_id.clone(), binding.generation), (a.clone(), 3));
    assert_eq!(
        world
            .run_task(&auto, "TASK[audit] Write the audit note for module 3.")
            .await
            .expect("test: audit on A"),
        "FINAL:audit-v1"
    );
    let latest = world.production_runs(3).await;
    assert!(nth(&latest, 2).hydration_receipts.iter().all(|h| h.revision_id == a));
    let read = read_skill_in_task(&world, &auto, "post-rollback").await;
    assert_eq!(
        text_of(&read, "content"),
        Some(a_text.as_str()),
        "the previous bytes, exactly"
    );

    // A stale rollback is refused and changes nothing.
    assert!(cli(rollback_command(&b, 2), &service, &auto).await.is_err());
    assert_eq!(
        world
            .store
            .active_binding(&skill())
            .expect("test: binding")
            .expect("test: bound")
            .generation,
        3
    );

    // B, its failures and the impact history are all still there.
    assert!(world.store.read_revision(&skill(), &b, None).is_ok(), "B is retained");
    let after: Vec<_> = b_failures
        .iter()
        .map(|id| {
            world
                .store
                .read_experience_by_id(id)
                .expect("test: read")
                .expect("test: present")
        })
        .collect();
    assert_eq!(snapshot, after, "B's failure receipts are unchanged");
    let record = world
        .store
        .read_proposal(&proposal.proposal_id)
        .expect("test: read")
        .expect("test: present");
    assert_eq!(
        record.publication.as_ref().map(|p| p.revision_id.clone()),
        Some(b.clone()),
        "the publication stays on record"
    );
    let journal = world.store.journal_entries().expect("test: journal");
    let rollback = journal
        .iter()
        .find(|e| e.kind == TransactionKind::Rollback)
        .expect("test: rollback entry");
    assert_eq!(
        (rollback.state, rollback.new_generation, &rollback.candidate_revision_id),
        (TransactionState::Committed, 3, &a)
    );
    assert!(journal.iter().any(|e| e.kind == TransactionKind::Publish
        && e.candidate_revision_id == b
        && e.state == TransactionState::Committed));

    // The CLI reports it.
    let status = cli(SkillLifecycleCommands::Status, &service, &auto)
        .await
        .expect("test: status");
    assert!(status.contains("mode: auto"), "{status}");
    let list = cli(SkillLifecycleCommands::List { skill: None }, &service, &auto)
        .await
        .expect("test: list");
    assert!(list.contains(&proposal.proposal_id), "{list}");
    let inspect = cli(
        SkillLifecycleCommands::Inspect {
            proposal_id: proposal.proposal_id.clone(),
        },
        &service,
        &auto,
    )
    .await
    .expect("test: inspect");
    assert!(inspect.contains(&proposal.proposal_id), "{inspect}");
}

/// A second revision published by the operator path, so the crash tests do not
/// repeat the whole pipeline.
fn world_with_two_revisions(world: &World) -> (RevisionId, RevisionId, String) {
    let a_text = skill_text(&[("hotfix", "hotfix-v1"), ("audit", "audit-v1")]);
    let a = world.onboard(&a_text);
    let b_text = skill_text(&[("hotfix", "hotfix-v1"), ("audit", "audit-v0")]);
    let bundle = Bundle::from_entries(vec![BundleEntry {
        path: "SKILL.md".into(),
        bytes: b_text.into_bytes(),
        executable: false,
    }])
    .expect("test: bundle");
    let b = crate::skills::lifecycle::test_support::publish_revision(
        &world.store,
        &skill(),
        SourceKind::Learned,
        &bundle,
        1,
    );
    (a, b, a_text)
}

fn rollback_entries(store: &SkillLifecycleStore) -> Vec<crate::skills::lifecycle::JournalEntry> {
    store
        .journal_entries()
        .expect("test: journal")
        .into_iter()
        .filter(|e| e.kind == TransactionKind::Rollback)
        .collect()
}

fn single_binding(store: &SkillLifecycleStore, skill: &SkillId) -> (RevisionId, u64) {
    let all = store.list_active_bindings().expect("test: bindings");
    assert_eq!(all.len(), 1, "exactly one active binding");
    let binding = store
        .active_binding(skill)
        .expect("test: binding")
        .expect("test: bound");
    (binding.revision_id, binding.generation)
}

#[tokio::test]
async fn a_crash_before_the_binding_swap_leaves_the_bad_revision_active_and_recovers_to_aborted() {
    let world = World::new().await;
    let (a, b, _) = world_with_two_revisions(&world);
    let auto = world.production_config(SkillsLifecycleModeConfig::Auto, &[]);
    let service = world.service(&auto);
    assert_eq!(single_binding(&world.store, &skill()), (b.clone(), 2));

    crash_hooks::arm("after_prepared");
    let crashed = service.rollback(&skill(), &a, 2);
    crash_hooks::disarm();
    assert!(crashed.is_err());
    assert_eq!(single_binding(&world.store, &skill()), (b.clone(), 2), "nothing moved");
    assert_eq!(
        nth(&rollback_entries(&world.store), 0).state,
        TransactionState::Prepared
    );

    let report = service.startup_recovery().expect("test: recovery");
    assert_eq!(report.recovery.aborted.len(), 1);
    assert_eq!(nth(&rollback_entries(&world.store), 0).state, TransactionState::Aborted);
    assert_eq!(single_binding(&world.store, &skill()), (b, 2));
    service.rollback(&skill(), &a, 2).expect("test: the retry succeeds");
    assert_eq!(single_binding(&world.store, &skill()), (a, 3));
}

#[tokio::test]
async fn a_crash_after_the_binding_swap_recovers_to_committed_and_serves_the_previous_bytes() {
    let world = World::new().await;
    let (a, b, a_text) = world_with_two_revisions(&world);
    let auto = world.production_config(SkillsLifecycleModeConfig::Auto, &[]);
    let service = world.service(&auto);

    crash_hooks::arm("after_binding");
    let crashed = service.rollback(&skill(), &a, 2);
    crash_hooks::disarm();
    assert!(crashed.is_err());
    assert_eq!(
        single_binding(&world.store, &skill()),
        (a.clone(), 3),
        "the swap had happened"
    );
    assert_eq!(
        nth(&rollback_entries(&world.store), 0).state,
        TransactionState::Prepared
    );

    let report = service.startup_recovery().expect("test: recovery");
    assert_eq!(report.recovery.committed.len(), 1);
    assert_eq!(
        nth(&rollback_entries(&world.store), 0).state,
        TransactionState::Committed
    );
    assert_eq!(single_binding(&world.store, &skill()), (a, 3));
    assert!(
        service
            .startup_recovery()
            .expect("test: recovery again")
            .recovery
            .committed
            .is_empty(),
        "idempotent"
    );
    let read = read_skill_in_task(&world, &auto, "after-crash").await;
    assert_eq!(text_of(&read, "content"), Some(a_text.as_str()));
    assert!(
        world.store.read_revision(&skill(), &b, None).is_ok(),
        "the bad revision is retained"
    );
}

#![cfg(test)]

use super::{activate, entry, md, owner};
use crate::skills::lifecycle::{
    Bundle, ExperienceOutcome, NewRevision, OwnerScope, SelectionRequest, SkillId, SkillLifecycleStore, SourceKind,
    TerminalKind, TerminalObservation, drain_background_for_test, experience_id, record_selected_skills,
    release_task_pins, schedule_selection_receipts, settle_task_terminal, task_pin,
};
use std::fs;
use tempfile::TempDir;

const NAME: &str = "Deploy helper";

fn skill() -> SkillId {
    SkillId::parse("deploy-helper").expect("test: skill id")
}

/// The identities of one test. The process-wide tables these tests touch (the
/// task pins, the selection claims, the dropped-evidence memory and the test
/// drain hook for background writes) are keyed by owner, run or task, so every
/// test derives its own owner and keys from its own temporary directory and
/// can never take, release or drain another test's entries.
struct Ids {
    owner: String,
    run: String,
    task: String,
}

impl Ids {
    fn of(tmp: &TempDir) -> Self {
        let tag: String = tmp
            .path()
            .file_name()
            .map(|name| {
                name.to_string_lossy()
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            owner: format!("owner:ws:cli:{tag}"),
            run: format!("run-{tag}"),
            task: format!("task-{tag}"),
        }
    }

    /// A run key of this test other than the main one.
    fn run_named(&self, name: &str) -> String {
        format!("{}-{name}", self.run)
    }

    fn scope(&self) -> OwnerScope {
        owner(&self.owner)
    }
}

fn setup() -> (TempDir, SkillLifecycleStore, Ids) {
    let tmp = TempDir::new().expect("test: tempdir");
    let ids = Ids::of(&tmp);
    let store = SkillLifecycleStore::open(tmp.path(), ids.scope()).expect("test: store");
    let bundle =
        Bundle::from_entries(vec![entry("SKILL.md", md(NAME, "Helps with deploys").as_bytes())]).expect("test: bundle");
    let written = store
        .write_revision(&NewRevision::new(skill(), SourceKind::Operator), &bundle)
        .expect("test: write");
    activate(
        &store,
        &skill(),
        &(written.revision.revision_id, written.record_digest),
        0,
        true,
    );
    (tmp, store, ids)
}

fn request<'a>(tmp: &'a TempDir, ids: &'a Ids, enabled: bool, names: &'a [&'a str]) -> SelectionRequest<'a> {
    SelectionRequest {
        workspace_dir: tmp.path(),
        enabled,
        owner_id: &ids.owner,
        run_id: &ids.run,
        task_id: Some(&ids.task),
        skill_names: names,
    }
}

fn observation(kind: TerminalKind, verified: bool) -> TerminalObservation {
    TerminalObservation {
        kind,
        awaiting_approval: false,
        verified,
        recovered: false,
        root: true,
    }
}

#[test]
fn a_selected_managed_skill_gets_one_receipt_even_when_selected_again() {
    let (tmp, store, ids) = setup();
    for _ in 0..3 {
        assert_eq!(
            record_selected_skills(&request(&tmp, &ids, true, &[NAME, "legacy-only"])),
            1
        );
    }
    // Keyed like hydration and the terminal: the run id wins over the task id.
    let experience = store
        .read_experience(&ids.run)
        .expect("test: read")
        .expect("test: record");
    assert_eq!(experience.selection_receipts.len(), 1);
    let receipt = experience.selection_receipts.first().expect("test: receipt");
    assert_eq!(receipt.skill_id, skill());
    assert_eq!(receipt.display_name, NAME);
    assert_eq!(receipt.correlation.run_id.as_deref(), Some(ids.run.as_str()));
    assert_eq!(receipt.correlation.task_id.as_deref(), Some(ids.task.as_str()));
    assert!(experience.used_skills().is_empty(), "selection never credits use");
    assert_eq!(experience.late_evidence, 0, "an idempotent repeat is not late evidence");
    release_task_pins(&ids.scope(), &ids.run);
}

#[test]
fn a_repeat_after_the_terminal_stays_idempotent() {
    let (tmp, store, ids) = setup();
    assert_eq!(record_selected_skills(&request(&tmp, &ids, true, &[NAME])), 1);
    store
        .join_terminal(&ids.run, "term-1", ExperienceOutcome::Success)
        .expect("test: join");
    assert_eq!(record_selected_skills(&request(&tmp, &ids, true, &[NAME])), 1);
    let experience = store
        .read_experience(&ids.run)
        .expect("test: read")
        .expect("test: record");
    assert_eq!(experience.selection_receipts.len(), 1);
    assert_eq!(experience.late_evidence, 0);
    release_task_pins(&ids.scope(), &ids.run);
}

#[test]
fn a_disabled_lifecycle_opens_and_writes_nothing() {
    let (tmp, store, ids) = setup();
    assert_eq!(record_selected_skills(&request(&tmp, &ids, false, &[NAME])), 0);
    assert!(store.read_experience(&ids.run).expect("test: read").is_none());

    let bare = TempDir::new().expect("test: tempdir");
    assert_eq!(record_selected_skills(&request(&bare, &ids, true, &[NAME])), 0);
    assert_eq!(
        fs::read_dir(bare.path()).expect("test: list").count(),
        0,
        "no store is created for a selection"
    );
}

#[test]
fn another_owner_records_nothing() {
    let (tmp, store, ids) = setup();
    let foreign_owner = format!("{}:mallory", ids.owner);
    let mut foreign = request(&tmp, &ids, true, &[NAME]);
    foreign.owner_id = &foreign_owner;
    assert_eq!(record_selected_skills(&foreign), 0);
    assert!(store.read_experience(&ids.run).expect("test: read").is_none());
}

#[test]
fn a_failing_capture_records_nothing_and_does_not_fail_the_caller() {
    let (tmp, store, ids) = setup();
    // The experiences directory cannot be created: the capture fails.
    fs::write(store.root().join("experiences"), b"not a directory").expect("test: block");
    assert_eq!(record_selected_skills(&request(&tmp, &ids, true, &[NAME])), 0);
    release_task_pins(&ids.scope(), &ids.run);
}

#[test]
fn only_verified_facts_make_a_claim() {
    assert_eq!(
        observation(TerminalKind::Completed, true).outcome(),
        Some(ExperienceOutcome::Success)
    );
    assert_eq!(
        observation(TerminalKind::Failed, true).outcome(),
        Some(ExperienceOutcome::Failure)
    );
    assert_eq!(
        TerminalObservation {
            awaiting_approval: true,
            ..observation(TerminalKind::Completed, true)
        }
        .outcome(),
        Some(ExperienceOutcome::Blocked)
    );
    for kind in [TerminalKind::Completed, TerminalKind::Failed, TerminalKind::Cancelled] {
        assert_eq!(
            observation(kind, false).outcome(),
            Some(ExperienceOutcome::Unknown),
            "unverified facts are Unknown"
        );
    }
    assert_eq!(observation(TerminalKind::Silent, false).outcome(), None);
    assert_eq!(observation(TerminalKind::Silent, true).outcome(), None);
}

#[tokio::test(flavor = "current_thread")]
async fn a_recovered_unverified_terminal_joins_unknown_once_and_never_qualifies() {
    let (tmp, store, ids) = setup();
    assert_eq!(record_selected_skills(&request(&tmp, &ids, true, &[NAME])), 1);
    let recovered = TerminalObservation {
        recovered: true,
        ..observation(TerminalKind::Completed, false)
    };
    for _ in 0..2 {
        settle_task_terminal(
            tmp.path(),
            Some(ids.owner.clone()),
            Some(ids.run.clone()),
            "term-1",
            recovered,
            None,
        )
        .await;
    }
    let experience = store
        .read_experience(&ids.run)
        .expect("test: read")
        .expect("test: record");
    let terminal = experience.terminal.as_ref().expect("test: joined");
    assert_eq!(terminal.terminal_receipt_id, "term-1");
    assert_eq!(experience.outcome, ExperienceOutcome::Unknown);
    assert!(!experience.learning_eligible(), "an Unknown terminal never qualifies");
    assert!(
        task_pin(&ids.scope(), &ids.run, &skill(), || Ok(None))
            .expect("test: pin table")
            .is_none(),
        "recovery releases the pins"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_recovered_terminal_keeps_the_join_the_live_path_made() {
    let (tmp, store, ids) = setup();
    assert_eq!(record_selected_skills(&request(&tmp, &ids, true, &[NAME])), 1);
    settle_task_terminal(
        tmp.path(),
        Some(ids.owner.clone()),
        Some(ids.run.clone()),
        "term-1",
        observation(TerminalKind::Completed, true),
        None,
    )
    .await;
    settle_task_terminal(
        tmp.path(),
        Some(ids.owner.clone()),
        Some(ids.run.clone()),
        "term-1",
        TerminalObservation {
            recovered: true,
            ..observation(TerminalKind::Completed, false)
        },
        None,
    )
    .await;
    let experience = store
        .read_experience(&ids.run)
        .expect("test: read")
        .expect("test: record");
    assert_eq!(experience.outcome, ExperienceOutcome::Success);
    assert!(experience.learning_eligible());
}

#[tokio::test(flavor = "current_thread")]
async fn scheduled_receipts_do_not_wait_for_a_locked_store_and_dedupe_in_memory() {
    let (tmp, store, ids) = setup();
    let locked = ids.run_named("locked");
    let scheduled = || SelectionRequest {
        run_id: &locked,
        ..request(&tmp, &ids, true, &[NAME])
    };
    let exp = SkillId::parse(&experience_id(&ids.scope(), &locked)).expect("test: experience id");
    let held = store
        .lock_skill(&exp, std::time::Duration::from_secs(1))
        .expect("test: hold the experience lock");
    let started = std::time::Instant::now();
    assert_eq!(schedule_selection_receipts(&scheduled()), 1);
    // The repeat assemblies of the same turn never reach the store.
    assert_eq!(schedule_selection_receipts(&scheduled()), 0);
    assert_eq!(schedule_selection_receipts(&scheduled()), 0);
    assert!(
        started.elapsed() < std::time::Duration::from_millis(250),
        "scheduling must not wait for the lock: {:?}",
        started.elapsed()
    );
    drop(held);
    drain_background_for_test(&locked).await;
    let experience = store
        .read_experience(&locked)
        .expect("test: read")
        .expect("test: record");
    assert_eq!(experience.selection_receipts.len(), 1);
    release_task_pins(&ids.scope(), &locked);
}

#[tokio::test(flavor = "current_thread")]
async fn a_disabled_lifecycle_schedules_nothing() {
    let (tmp, _store, ids) = setup();
    assert_eq!(schedule_selection_receipts(&request(&tmp, &ids, false, &[NAME])), 0);
}

#[test]
fn scheduling_outside_a_runtime_is_skipped_and_keeps_the_claim_free() {
    let (tmp, store, ids) = setup();
    let outside = ids.run_named("outside");
    let scheduled = || SelectionRequest {
        run_id: &outside,
        ..request(&tmp, &ids, true, &[NAME])
    };
    assert_eq!(schedule_selection_receipts(&scheduled()), 0);
    assert!(store.read_experience(&outside).expect("test: read").is_none());
    // A later call inside a runtime still records the skill.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test: runtime");
    runtime.block_on(async {
        assert_eq!(schedule_selection_receipts(&scheduled()), 1);
        drain_background_for_test(&outside).await;
    });
    let experience = store
        .read_experience(&outside)
        .expect("test: read")
        .expect("test: record");
    assert_eq!(experience.selection_receipts.len(), 1);
    release_task_pins(&ids.scope(), &outside);
}

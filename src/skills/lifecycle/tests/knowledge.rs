#![cfg(test)]
#![allow(clippy::indexing_slicing)]

use super::{activate, entry, md, owner};
use crate::skills::lifecycle::{
    Bundle, ConsolidationRequest, Correlation, ExperienceOutcome, FailureClass, FailureFacts, LifecycleError,
    NewRejection, NewRevision, PatternId, PatternKind, PatternScope, PinnedSkillRevision, ProposalOperation,
    RollbackRequest, SkillId, SkillLifecycleStore, SourceKind, TransactionId, classify_failure, material_digest,
};
use std::fs;
use tempfile::TempDir;

const OWNER: &str = "owner:ws:cli:alice";

fn skill() -> SkillId {
    SkillId::parse("deploy-helper").expect("test: skill id")
}

fn setup() -> (TempDir, SkillLifecycleStore, PinnedSkillRevision) {
    let tmp = TempDir::new().expect("test: tempdir");
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: store");
    let bundle =
        Bundle::from_entries(vec![entry("SKILL.md", md("Deploy helper", "Helps").as_bytes())]).expect("test: bundle");
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
    let pin = store.pin_active(&skill()).expect("test: pin").expect("test: bound");
    (tmp, store, pin)
}

/// An experience that hydrated the skill and has a joined terminal.
fn eligible(store: &SkillLifecycleStore, pin: &PinnedSkillRevision, task: &str) -> String {
    store
        .record_hydration(task, pin, "SKILL.md", b"x", &Correlation::default())
        .expect("test: hydrate");
    store
        .join_terminal(task, "term", ExperienceOutcome::Failure)
        .expect("test: join");
    crate::skills::lifecycle::experience_id(store.owner_scope(), task)
}

fn scope(environment: &str, capabilities: &[&str]) -> PatternScope {
    PatternScope {
        environment: environment.to_string(),
        capabilities: capabilities.iter().map(ToString::to_string).collect(),
    }
}

fn request(evidence: &[&String], counter: &[&String]) -> ConsolidationRequest {
    ConsolidationRequest {
        task_family: "deploy".to_string(),
        failure_class: FailureClass::MissingGuidance,
        skill_id: Some(skill()),
        scope: scope("local", &["shell"]),
        statement: "The deploy skill omits the migration step.".to_string(),
        applicability: vec!["database deploys".to_string()],
        exceptions: vec!["read-only environments".to_string()],
        evidence: evidence.iter().map(|id| (*id).clone()).collect(),
        counterevidence: counter.iter().map(|id| (*id).clone()).collect(),
    }
}

#[test]
fn failures_are_classified_with_infrastructure_and_policy_causes_first() {
    let facts = |f: fn(&mut FailureFacts)| {
        let mut facts = FailureFacts::default();
        f(&mut facts);
        classify_failure(&facts)
    };
    assert_eq!(facts(|_| {}), None, "unsupported facts teach nothing");
    assert_eq!(
        facts(|f| f.guidance_missing_or_wrong = true),
        Some(FailureClass::MissingGuidance)
    );
    assert_eq!(
        facts(|f| f.correct_guidance_not_followed = true),
        Some(FailureClass::GuidanceNotFollowed),
        "a correct skill that was not followed is not a content defect"
    );
    assert_eq!(
        facts(|f| f.wrong_skill_selected = true),
        Some(FailureClass::WrongSkillRetrieved)
    );
    assert_eq!(
        facts(|f| f.permission_refused = true),
        Some(FailureClass::PermissionRefusal)
    );
    assert_eq!(
        facts(|f| f.provider_or_tool_unavailable = true),
        Some(FailureClass::InfrastructureUnavailable)
    );
    assert_eq!(
        facts(|f| f.runtime_abort_or_cleanup_failure = true),
        Some(FailureClass::RuntimeDefect)
    );
    assert_eq!(
        facts(|f| f.contract_or_oracle_conflict = true),
        Some(FailureClass::ConflictingContract)
    );
    // Guidance that merely looks incomplete never outranks the real cause.
    assert_eq!(
        facts(|f| {
            f.guidance_missing_or_wrong = true;
            f.provider_or_tool_unavailable = true;
        }),
        Some(FailureClass::InfrastructureUnavailable)
    );
    assert_eq!(
        facts(|f| {
            f.guidance_missing_or_wrong = true;
            f.permission_refused = true;
        }),
        Some(FailureClass::PermissionRefusal)
    );
    for class in [
        FailureClass::GuidanceNotFollowed,
        FailureClass::InfrastructureUnavailable,
        FailureClass::PermissionRefusal,
        FailureClass::RuntimeDefect,
        FailureClass::ConflictingContract,
        FailureClass::WrongSkillRetrieved,
    ] {
        assert!(!class.allows_skill_revision(), "{class:?}");
    }
    assert!(FailureClass::MissingGuidance.allows_skill_revision());
}

#[test]
fn a_pattern_links_its_evidence_and_gains_support_by_superseding() {
    let (_tmp, store, pin) = setup();
    let first = eligible(&store, &pin, "task-1");
    let created = store
        .consolidate_pattern(&request(&[&first], &[]))
        .expect("test: consolidate");
    assert!(created.created);
    let one = created.pattern;
    assert_eq!(one.kind, PatternKind::Guidance);
    assert_eq!(one.evidence_ids, vec![first]);
    assert_eq!(
        (one.confidence_basis.evidence_count, one.confidence_basis.distinct_tasks),
        (1, 1)
    );
    assert!(!one.usable_for_proposal(), "a single incident cannot back a revision");

    let second = eligible(&store, &pin, "task-2");
    let two = store
        .consolidate_pattern(&request(&[&second], &[]))
        .expect("test: extend")
        .pattern;
    assert_eq!(two.supersedes, vec![one.pattern_id.clone()]);
    assert_eq!(two.evidence_ids.len(), 2, "earlier evidence is carried over");
    assert!(two.usable_for_proposal());

    let index = store.pattern_index().expect("test: index");
    assert_eq!(
        index.entries.iter().map(|e| &e.pattern_id).collect::<Vec<_>>(),
        vec![&two.pattern_id],
        "the index lists only the active pattern"
    );
    let active = store.active_patterns().expect("test: active");
    assert_eq!(active.len(), 1);
    assert_eq!(active.first().map(|p| &p.pattern_id), Some(&two.pattern_id));
    // The superseded pattern is still readable: history is kept.
    assert!(store.read_pattern(&one.pattern_id).expect("test: read").is_some());
}

#[test]
fn consolidation_is_idempotent_and_never_touches_experiences() {
    let (_tmp, store, pin) = setup();
    let id = eligible(&store, &pin, "task-1");
    let path = store.root().join("experiences").join(format!("{id}.json"));
    let before = fs::read(&path).expect("test: experience bytes");
    let first = store
        .consolidate_pattern(&request(&[&id], &[]))
        .expect("test: consolidate");
    let again = store.consolidate_pattern(&request(&[&id], &[])).expect("test: repeat");
    assert!(!again.created);
    assert_eq!(first.pattern.pattern_id, again.pattern.pattern_id);
    assert_eq!(store.pattern_index().expect("test: index").entries.len(), 1);
    assert_eq!(
        fs::read(&path).expect("test: experience bytes"),
        before,
        "original receipts are unchanged"
    );
}

#[test]
fn contradicting_evidence_is_preserved_and_lowers_trust() {
    let (_tmp, store, pin) = setup();
    let a = eligible(&store, &pin, "task-a");
    let b = eligible(&store, &pin, "task-b");
    let c = eligible(&store, &pin, "task-c");
    let d = eligible(&store, &pin, "task-d");
    let supported = store
        .consolidate_pattern(&request(&[&a, &b], &[]))
        .expect("test: consolidate")
        .pattern;
    assert!(supported.usable_for_proposal());

    let contested = store.consolidate_pattern(&request(&[], &[&c, &d])).err();
    assert!(
        contested.is_some(),
        "a request needs at least one supporting experience"
    );
    let contested = store
        .consolidate_pattern(&request(&[&a], &[&c, &d]))
        .expect("test: contradict")
        .pattern;
    assert_eq!(contested.counterevidence_ids, vec![c.clone(), d]);
    assert_eq!(contested.evidence_ids, vec![a.clone(), b], "support is not discarded");
    assert!(
        !contested.usable_for_proposal(),
        "as much contradiction as support: not usable"
    );

    let err = store
        .consolidate_pattern(&request(&[&c], &[&a]))
        .expect_err("test: flipping sides");
    assert!(matches!(err, LifecycleError::InvalidKnowledge(_)), "{err}");
    let err = store
        .consolidate_pattern(&request(&[&a], &[&a]))
        .expect_err("test: both sides");
    assert!(matches!(err, LifecycleError::InvalidKnowledge(_)), "{err}");
}

#[test]
fn unusable_evidence_is_refused() {
    let (_tmp, store, pin) = setup();
    let unknown = crate::skills::lifecycle::experience_id(store.owner_scope(), "never-ran");
    let err = store
        .consolidate_pattern(&request(&[&unknown], &[]))
        .expect_err("test: unknown");
    assert!(matches!(err, LifecycleError::EvidenceNotUsable { .. }), "{err}");

    store
        .record_hydration("open", &pin, "SKILL.md", b"x", &Correlation::default())
        .expect("test: hydrate");
    let open = crate::skills::lifecycle::experience_id(store.owner_scope(), "open");
    let err = store
        .consolidate_pattern(&request(&[&open], &[]))
        .expect_err("test: no terminal");
    assert!(matches!(err, LifecycleError::EvidenceNotUsable { .. }), "{err}");

    store
        .record_hydration("dropped", &pin, "SKILL.md", b"x", &Correlation::default())
        .expect("test: hydrate");
    store.mark_dropped_evidence("dropped");
    store
        .join_terminal("dropped", "term", ExperienceOutcome::Failure)
        .expect("test: join");
    let dropped = crate::skills::lifecycle::experience_id(store.owner_scope(), "dropped");
    assert!(
        store.consolidate_pattern(&request(&[&dropped], &[])).is_err(),
        "dropped evidence cannot support a claim"
    );

    // A joined experience that never hydrated the skill cannot support guidance about it.
    store
        .record_selection("selected-only", &pin, &Correlation::default())
        .expect("test: select");
    store
        .join_terminal("selected-only", "term", ExperienceOutcome::Failure)
        .expect("test: join");
    let selected = crate::skills::lifecycle::experience_id(store.owner_scope(), "selected-only");
    let err = store
        .consolidate_pattern(&request(&[&selected], &[]))
        .expect_err("test: never hydrated");
    assert!(matches!(err, LifecycleError::EvidenceNotUsable { .. }), "{err}");

    assert!(
        store
            .consolidate_pattern(&request(&[&"not-an-id".to_string()], &[]))
            .is_err()
    );
}

#[test]
fn patterns_merge_only_inside_an_identical_scope() {
    let (_tmp, store, pin) = setup();
    let a = eligible(&store, &pin, "task-a");
    let b = eligible(&store, &pin, "task-b");
    let local = store
        .consolidate_pattern(&request(&[&a], &[]))
        .expect("test: local")
        .pattern;
    let mut other_env = request(&[&b], &[]);
    other_env.scope = scope("staging", &["shell"]);
    let staging = store.consolidate_pattern(&other_env).expect("test: staging").pattern;
    let mut other_caps = request(&[&b], &[]);
    other_caps.scope = scope("local", &["shell", "network"]);
    let networked = store
        .consolidate_pattern(&other_caps)
        .expect("test: capabilities")
        .pattern;
    for pattern in [&staging, &networked] {
        assert!(pattern.supersedes.is_empty(), "no merge across scopes");
        assert_ne!(pattern.pattern_id, local.pattern_id);
    }
    assert_eq!(store.active_patterns().expect("test: active").len(), 3);
    // Capability order does not create a different scope.
    let mut reordered = request(&[&a], &[]);
    reordered.scope = scope("local", &["network", "shell"]);
    let same = store.consolidate_pattern(&reordered).expect("test: reordered").pattern;
    assert_eq!(same.supersedes, vec![networked.pattern_id]);
}

#[test]
fn blockers_and_lapses_never_become_workflow_rules() {
    let (_tmp, store, pin) = setup();
    let ids: Vec<String> = (0..4).map(|i| eligible(&store, &pin, &format!("task-{i}"))).collect();
    let refs: Vec<&String> = ids.iter().collect();
    for class in [
        FailureClass::InfrastructureUnavailable,
        FailureClass::PermissionRefusal,
        FailureClass::RuntimeDefect,
        FailureClass::ConflictingContract,
        FailureClass::GuidanceNotFollowed,
        FailureClass::WrongSkillRetrieved,
    ] {
        let mut req = request(&refs, &[]);
        req.failure_class = class;
        req.task_family = format!("family-{class:?}");
        let pattern = store.consolidate_pattern(&req).expect("test: consolidate").pattern;
        assert_eq!(pattern.kind, class.kind());
        assert!(
            !pattern.usable_for_proposal(),
            "{class:?} must not back a revision however often it repeats"
        );
    }
    let mut no_skill = request(&refs, &[]);
    no_skill.skill_id = None;
    assert!(
        store.consolidate_pattern(&no_skill).is_err(),
        "guidance needs the skill it is about"
    );
}

#[test]
fn bounds_are_enforced() {
    let (_tmp, store, pin) = setup();
    let id = eligible(&store, &pin, "task-1");
    let mut long = request(&[&id], &[]);
    long.statement = "x".repeat(2001);
    assert!(store.consolidate_pattern(&long).is_err());
    let mut many = request(&[&id], &[]);
    many.applicability = (0..17).map(|i| format!("case {i}")).collect();
    assert!(store.consolidate_pattern(&many).is_err());
    let mut crowded = request(&[&id], &[]);
    crowded.evidence = (0..257).map(|i| format!("exp-{i:032x}")).collect();
    assert!(store.consolidate_pattern(&crowded).is_err());
    let mut blank = request(&[&id], &[]);
    blank.task_family = "  ".to_string();
    assert!(store.consolidate_pattern(&blank).is_err());
}

#[test]
fn the_index_is_derived_and_repairable() {
    let (_tmp, store, pin) = setup();
    let a = eligible(&store, &pin, "task-a");
    let b = eligible(&store, &pin, "task-b");
    store.consolidate_pattern(&request(&[&a], &[])).expect("test: first");
    let latest = store
        .consolidate_pattern(&request(&[&b], &[]))
        .expect("test: second")
        .pattern;
    let index_path = store.root().join("patterns").join("index.json");
    fs::remove_file(&index_path).expect("test: remove index");
    assert_eq!(
        store.rebuild_pattern_index().expect("test: rebuild"),
        1,
        "history is not indexed"
    );
    let active = store.active_patterns().expect("test: active");
    assert_eq!(
        active.iter().map(|p| &p.pattern_id).collect::<Vec<_>>(),
        vec![&latest.pattern_id]
    );

    // A pattern file whose index update never happened is repaired by the next call.
    let index_text = fs::read_to_string(&index_path).expect("test: index");
    let mut index: crate::skills::lifecycle::PatternIndex = serde_json::from_str(&index_text).expect("test: parse");
    index.entries.retain(|entry| entry.pattern_id != latest.pattern_id);
    fs::write(&index_path, serde_json::to_vec_pretty(&index).expect("test: json")).expect("test: write");
    let repaired = store.consolidate_pattern(&request(&[&b], &[])).expect("test: repair");
    assert!(!repaired.created);
    assert_eq!(store.active_patterns().expect("test: active").len(), 1);
}

#[test]
fn forged_or_foreign_patterns_are_refused() {
    let (tmp, store, pin) = setup();
    let id = eligible(&store, &pin, "task-1");
    let pattern = store
        .consolidate_pattern(&request(&[&id], &[]))
        .expect("test: consolidate")
        .pattern;
    let path = store
        .root()
        .join("patterns")
        .join(format!("{}.json", pattern.pattern_id));
    let text = fs::read_to_string(&path).expect("test: pattern");
    fs::write(&path, text.replace(OWNER, "owner:ws:cli:mallory")).expect("test: forge");
    assert!(matches!(
        store.read_pattern(&pattern.pattern_id).expect_err("test: forged owner"),
        LifecycleError::OwnerMismatch { .. }
    ));
    fs::write(&path, &text).expect("test: restore");
    assert!(store.read_pattern(&pattern.pattern_id).expect("test: read").is_some());
    let other = PatternId::parse(&format!("pat-{}", "0".repeat(32))).expect("test: id");
    fs::write(store.root().join("patterns").join(format!("{other}.json")), &text).expect("test: misplace");
    assert!(matches!(
        store.read_pattern(&other).expect_err("test: misplaced"),
        LifecycleError::Tampered { .. } | LifecycleError::InvalidRecord { .. }
    ));
    drop(tmp);
}

#[test]
fn concurrent_consolidation_keeps_every_group_and_a_consistent_index() {
    let (_tmp, store, pin) = setup();
    let ids: Vec<String> = (0..4).map(|i| eligible(&store, &pin, &format!("task-{i}"))).collect();
    std::thread::scope(|scope_| {
        for (index, id) in ids.iter().enumerate() {
            let store = &store;
            scope_.spawn(move || {
                let mut req = request(&[id], &[]);
                req.task_family = format!("family-{index}");
                store.consolidate_pattern(&req).expect("test: concurrent consolidate");
            });
        }
    });
    assert_eq!(store.active_patterns().expect("test: active").len(), 4);
    assert_eq!(store.pattern_index().expect("test: index").entries.len(), 4);
}

fn rejection(proposal: &str, material: &str, evidence: &[&str], environment: &str) -> NewRejection {
    NewRejection {
        proposal_id: proposal.to_string(),
        operation: ProposalOperation::Revise,
        skill_id: skill(),
        material: vec![material.to_string()],
        pattern_ids: Vec::new(),
        evidence_ids: evidence.iter().map(ToString::to_string).collect(),
        environment_binding: environment.to_string(),
        reasons: vec!["regressed a critical case".to_string()],
    }
}

fn ask(
    store: &SkillLifecycleStore,
    material: &str,
    evidence: &[&str],
    env: &str,
    op: ProposalOperation,
) -> Option<crate::skills::lifecycle::RejectionRecord> {
    store
        .suppressing_rejection(
            &skill(),
            op,
            &[material],
            &evidence.iter().map(ToString::to_string).collect::<Vec<_>>(),
            env,
        )
        .expect("test: check")
}

#[test]
fn material_digests_ignore_layout_but_not_content() {
    let a = material_digest(&["## Step 1\n\n- Run **the migration**, then deploy."]);
    let b = material_digest(&["step 1 run the migration then deploy"]);
    assert_eq!(a, b);
    assert_ne!(a, material_digest(&["step 1 deploy then run the migration"]));
    assert_ne!(
        material_digest(&["ab", "c"]),
        material_digest(&["a", "bc"]),
        "text boundaries matter"
    );
    for (x, y) in [
        ("assert x != y", "assert x == y"),
        ("wait 1.5s", "wait 15s"),
        ("run rm -rf ./build", "run rm rf build"),
        ("run `rm -rf ./build.`", "run `rm -rf ./build`"),
    ] {
        assert_ne!(material_digest(&[x]), material_digest(&[y]), "{x:?} vs {y:?}");
    }
    assert_eq!(
        material_digest(&["Run `Make  Clean` now."]),
        material_digest(&["run `make clean` now"]),
        "case and whitespace do not matter, code spans keep their content"
    );
}

#[test]
fn an_equivalent_repeat_is_suppressed_until_evidence_or_environment_changes() {
    let (_tmp, store, pin) = setup();
    let (a, b, new) = (
        eligible(&store, &pin, "task-a"),
        eligible(&store, &pin, "task-b"),
        eligible(&store, &pin, "task-new"),
    );
    let recorded = store
        .record_rejection(&rejection(
            "prop-1",
            "Run the migration before deploying.",
            &[&a, &b],
            "env-1",
        ))
        .expect("test: record");
    let same = "run the MIGRATION before deploying!";
    assert_eq!(
        ask(&store, same, &[&a], "env-1", ProposalOperation::Revise).map(|r| r.record_id),
        Some(recorded.record_id.clone())
    );
    assert_eq!(
        ask(&store, same, &[&a, &b], "env-1", ProposalOperation::Revise).map(|r| r.record_id),
        Some(recorded.record_id)
    );
    assert!(
        ask(&store, same, &[&a, &new], "env-1", ProposalOperation::Revise).is_none(),
        "new evidence justifies a new attempt"
    );
    assert!(
        ask(&store, same, &[&a], "env-2", ProposalOperation::Revise).is_none(),
        "a changed environment does too"
    );
    assert!(ask(&store, "Something else.", &[&a], "env-1", ProposalOperation::Revise).is_none());
    assert!(ask(&store, same, &[&a], "env-1", ProposalOperation::Disable).is_none());
}

#[test]
fn made_up_evidence_neither_records_nor_defeats_suppression() {
    let (_tmp, store, pin) = setup();
    let a = eligible(&store, &pin, "task-a");
    let invented = format!("exp-{}", "7".repeat(32));
    let err = store
        .record_rejection(&rejection("prop-x", "text", &[&invented], "env"))
        .expect_err("test: unknown evidence");
    assert!(matches!(err, LifecycleError::EvidenceNotUsable { .. }), "{err}");
    store
        .record_rejection(&rejection("prop-1", "Run the migration.", &[&a], "env"))
        .expect("test: record");
    for bogus in [invented.as_str(), "not-an-id"] {
        assert!(
            ask(
                &store,
                "Run the migration.",
                &[&a, bogus],
                "env",
                ProposalOperation::Revise
            )
            .is_some(),
            "{bogus} is not evidence and must not unlock a repeat"
        );
    }
    let open = {
        store
            .record_hydration("open", &pin, "SKILL.md", b"x", &Correlation::default())
            .expect("test: hydrate");
        crate::skills::lifecycle::experience_id(store.owner_scope(), "open")
    };
    assert!(
        ask(
            &store,
            "Run the migration.",
            &[&a, &open],
            "env",
            ProposalOperation::Revise
        )
        .is_some(),
        "an experience without a terminal is not evidence either"
    );
    let fresh = eligible(&store, &pin, "task-fresh");
    assert!(
        ask(
            &store,
            "Run the migration.",
            &[&a, &fresh],
            "env",
            ProposalOperation::Revise
        )
        .is_none()
    );
}

#[test]
fn rejections_are_idempotent_immutable_and_validated() {
    let (_tmp, store, pin) = setup();
    let a = eligible(&store, &pin, "task-a");
    let first = store
        .record_rejection(&rejection("prop-1", "fp text", &[&a], "env"))
        .expect("test: record");
    let again = store
        .record_rejection(&rejection("prop-1", "fp  TEXT", &[&a], "env"))
        .expect("test: repeat");
    assert_eq!(first, again);
    let mut reordered = rejection("prop-1", "fp text", &[&a], "env");
    reordered.reasons = vec!["regressed a critical case".to_string(), "another".to_string()];
    let err = store.record_rejection(&reordered).expect_err("test: more reasons");
    assert!(matches!(err, LifecycleError::InvalidKnowledge(_)), "{err}");
    let mut shuffled = rejection("prop-1", "fp text", &[&a, &a], "env");
    shuffled.reasons = vec![
        "regressed a critical case".to_string(),
        "regressed a critical case".to_string(),
    ];
    assert_eq!(
        store
            .record_rejection(&shuffled)
            .expect("test: duplicates compare canonically"),
        first
    );
    let err = store
        .record_rejection(&rejection("prop-1", "different", &[&a], "env"))
        .expect_err("test: same proposal, other content");
    assert!(matches!(err, LifecycleError::InvalidKnowledge(_)), "{err}");
    let mut unknown = rejection("prop-2", "fp", &[], "env");
    unknown.pattern_ids = vec![PatternId::parse(&format!("pat-{}", "1".repeat(32))).expect("test: id")];
    assert!(
        store.record_rejection(&unknown).is_err(),
        "a rejection must reference real patterns"
    );
    let mut reasonless = rejection("prop-3", "fp", &[], "env");
    reasonless.reasons.clear();
    assert!(store.record_rejection(&reasonless).is_err());
    assert!(
        store
            .record_rejection(&rejection("prop-4", " \n ", &[], "env"))
            .is_err(),
        "empty material"
    );
    assert!(
        store.record_rejection(&rejection("prop\n5", "fp", &[], "env")).is_err(),
        "newline in an identifier"
    );
    assert_eq!(store.rejections_for(&skill()).expect("test: list").len(), 1);

    let path = store
        .root()
        .join("rejections")
        .join(format!("{}.json", first.record_id));
    let text = fs::read_to_string(&path).expect("test: record");
    fs::write(&path, text.replace(OWNER, "owner:ws:cli:mallory")).expect("test: forge");
    assert!(matches!(
        store.rejections_for(&skill()).expect_err("test: forged"),
        LifecycleError::OwnerMismatch { .. }
    ));
}

#[test]
fn rejection_listing_is_bounded() {
    let (_tmp, store, _pin) = setup();
    let dir = store.root().join("rejections");
    fs::create_dir_all(&dir).expect("test: dir");
    for i in 0..1025 {
        fs::write(dir.join(format!("rej-{i:032x}.json")), b"{}").expect("test: file");
    }
    let err = store.rejections_for(&skill()).expect_err("test: too many");
    assert!(matches!(err, LifecycleError::KnowledgeLimit { .. }), "{err}");
}

#[test]
fn rejection_history_survives_publication_and_rollback() {
    let (_tmp, store, pin) = setup();
    let exp = eligible(&store, &pin, "task-a");
    let a = pin.revision_id;
    let record = store
        .record_rejection(&rejection("prop-9", "text 9", &[&exp], "env"))
        .expect("test: record");

    let bundle =
        Bundle::from_entries(vec![entry("SKILL.md", md("Deploy helper", "Second").as_bytes())]).expect("test: bundle");
    let b = store
        .write_revision(&NewRevision::new(skill(), SourceKind::Learned), &bundle)
        .expect("test: write B");
    activate(&store, &skill(), &(b.revision.revision_id, b.record_digest), 1, true);
    store
        .rollback(&RollbackRequest {
            transaction_id: TransactionId::generate(),
            skill_id: skill(),
            target: a,
            expected_generation: 2,
        })
        .expect("test: rollback");

    assert_eq!(store.rejections_for(&skill()).expect("test: list"), vec![record]);
    assert!(
        ask(&store, "text 9", &[&exp], "env", ProposalOperation::Revise).is_some(),
        "a rolled-back skill still remembers the rejected attempt"
    );
}

fn lesson(statement: &str, evidence: &[&String]) -> ConsolidationRequest {
    let mut req = request(evidence, &[]);
    req.statement = statement.to_string();
    req
}

#[test]
fn different_statements_are_different_groups_and_a_to_b_to_a_stays_correct() {
    let (_tmp, store, pin) = setup();
    let ids: Vec<String> = (0..4).map(|i| eligible(&store, &pin, &format!("task-{i}"))).collect();
    let a1 = store
        .consolidate_pattern(&lesson("Lesson A: run the migration.", &[&ids[0], &ids[1]]))
        .expect("test: A")
        .pattern;
    let b1 = store
        .consolidate_pattern(&lesson("Lesson B: check the lockfile.", &[&ids[2]]))
        .expect("test: B")
        .pattern;
    assert!(b1.supersedes.is_empty(), "a different lesson supersedes nothing");
    assert_eq!(b1.evidence_ids, vec![ids[2].clone()], "and inherits no evidence");
    assert!(!b1.usable_for_proposal(), "one incident of lesson B");
    assert_eq!(store.active_patterns().expect("test: active").len(), 2);
    // A again, re-worded only by case and spacing, with the same evidence.
    let again = store
        .consolidate_pattern(&lesson("lesson a:  RUN the migration.", &[&ids[0]]))
        .expect("test: A again");
    assert!(!again.created);
    assert_eq!(again.pattern.pattern_id, a1.pattern_id);
    // A with new evidence extends A, not B.
    let a2 = store
        .consolidate_pattern(&lesson("Lesson A: run the migration.", &[&ids[3]]))
        .expect("test: A extended")
        .pattern;
    assert_eq!(a2.supersedes, vec![a1.pattern_id]);
    let active: Vec<_> = store
        .active_patterns()
        .expect("test: active")
        .into_iter()
        .map(|p| p.pattern_id)
        .collect();
    assert_eq!(active.len(), 2);
    assert!(active.contains(&a2.pattern_id) && active.contains(&b1.pattern_id));
}

#[test]
fn applicability_and_exceptions_are_merged_not_replaced() {
    let (_tmp, store, pin) = setup();
    let (a, b) = (eligible(&store, &pin, "task-a"), eligible(&store, &pin, "task-b"));
    store.consolidate_pattern(&request(&[&a], &[])).expect("test: first");
    let mut later = request(&[&b], &[]);
    later.applicability = vec!["staging deploys".to_string()];
    later.exceptions = Vec::new();
    let merged = store.consolidate_pattern(&later).expect("test: second").pattern;
    assert_eq!(
        merged.applicability,
        vec!["database deploys".to_string(), "staging deploys".to_string()]
    );
    assert_eq!(
        merged.exceptions,
        vec!["read-only environments".to_string()],
        "an exception a later request omits is not lost"
    );
}

#[test]
fn the_index_stays_bounded_and_a_full_index_is_refused_without_orphans() {
    let (_tmp, store, pin) = setup();
    let ids: Vec<String> = (0..3).map(|i| eligible(&store, &pin, &format!("task-{i}"))).collect();
    for id in &ids {
        store.consolidate_pattern(&request(&[id], &[])).expect("test: extend");
    }
    assert_eq!(
        store.pattern_index().expect("test: index").entries.len(),
        1,
        "history is not kept in the index"
    );
    // Many large groups: the index must refuse growth past the readable cap.
    let capabilities: Vec<String> = (0..32).map(|i| format!("{i:0>120}")).collect();
    let mut refused = None;
    for group in 0..120 {
        let mut req = request(&[&ids[0]], &[]);
        req.task_family = format!("family-{group}");
        req.scope.capabilities = capabilities.clone();
        if let Err(e) = store.consolidate_pattern(&req) {
            refused = Some((group, e));
            break;
        }
    }
    let (group, err) = refused.expect("test: the cap must be reached");
    assert!(matches!(err, LifecycleError::KnowledgeLimit { .. }), "{err}");
    let index = store.pattern_index().expect("test: the index is still readable");
    assert_eq!(index.entries.len(), group + 1, "the refused write left the index alone");
    let files = fs::read_dir(store.root().join("patterns"))
        .expect("test: dir")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("pat-"))
        .count();
    assert_eq!(files, group + 3, "no orphan pattern file for the refused request");
    assert_eq!(
        store.rebuild_pattern_index().expect("test: rebuild"),
        index.entries.len()
    );
}

#[test]
fn a_crash_between_pattern_and_index_write_resolves_to_one_active_pattern() {
    let (_tmp, store, pin) = setup();
    let ids: Vec<String> = (0..3).map(|i| eligible(&store, &pin, &format!("task-{i}"))).collect();
    let first = store
        .consolidate_pattern(&request(&[&ids[0]], &[]))
        .expect("test: first")
        .pattern;
    let index_path = store.root().join("patterns").join("index.json");
    let stale = fs::read(&index_path).expect("test: index");
    let second = store
        .consolidate_pattern(&request(&[&ids[1]], &[]))
        .expect("test: second")
        .pattern;
    // The crash: the second pattern file is on disk, the index never learned of it.
    fs::write(&index_path, &stale).expect("test: stale index");
    let third = store
        .consolidate_pattern(&request(&[&ids[2]], &[]))
        .expect("test: third")
        .pattern;
    assert_eq!(
        third.supersedes,
        vec![second.pattern_id.clone()],
        "extends the unindexed successor"
    );
    assert_eq!(third.evidence_ids.len(), 3);
    let active = store.active_patterns().expect("test: active");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].pattern_id, third.pattern_id);
    assert_ne!(first.pattern_id, second.pattern_id);
}

fn write_forged_pattern(store: &SkillLifecycleStore, pattern: &crate::skills::lifecycle::KnowledgePattern) {
    let path = store
        .root()
        .join("patterns")
        .join(format!("{}.json", pattern.pattern_id));
    fs::write(&path, serde_json::to_vec_pretty(pattern).expect("test: json")).expect("test: write");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("test: mode");
}

#[test]
fn rebuild_resolves_a_fork_by_pattern_id_not_by_clock() {
    let (_tmp, store, pin) = setup();
    let (a, b) = (eligible(&store, &pin, "task-a"), eligible(&store, &pin, "task-b"));
    let first = store
        .consolidate_pattern(&request(&[&a], &[]))
        .expect("test: first")
        .pattern;
    let second = store
        .consolidate_pattern(&request(&[&b], &[]))
        .expect("test: second")
        .pattern;
    // A rival successor of the first pattern; it is the EARLIER one by clock.
    let mut rival = second.clone();
    rival.applicability.push("zzz rival".to_string());
    rival.applicability.sort();
    rival.created_at = second.created_at - chrono::Duration::seconds(500);
    rival.supersedes = vec![first.pattern_id];
    let rival_id = PatternId::from_digest(&rival.content_digest());
    rival.pattern_id = rival_id.clone();
    rival.record_id = rival_id.to_string();
    write_forged_pattern(&store, &rival);
    let expected = std::cmp::max(second.pattern_id, rival_id);
    for _ in 0..2 {
        assert_eq!(store.rebuild_pattern_index().expect("test: rebuild"), 1);
        let active = store.active_patterns().expect("test: active");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].pattern_id, expected, "the greater id wins every time");
    }
    fs::remove_file(store.root().join("patterns").join("index.json")).expect("test: drop index");
    assert_eq!(store.rebuild_pattern_index().expect("test: rebuild"), 1);
    assert_eq!(store.active_patterns().expect("test: active")[0].pattern_id, expected);
}

#[test]
fn clock_skew_cannot_resurrect_superseded_history() {
    let (_tmp, store, pin) = setup();
    let ids: Vec<String> = (0..3).map(|i| eligible(&store, &pin, &format!("task-{i}"))).collect();
    let p0 = store
        .consolidate_pattern(&request(&[&ids[0]], &[]))
        .expect("test: p0")
        .pattern;
    store.consolidate_pattern(&request(&[&ids[1]], &[])).expect("test: p1");
    let p2 = store
        .consolidate_pattern(&request(&[&ids[2]], &[]))
        .expect("test: p2")
        .pattern;
    // The oldest pattern claims to be created far in the future.
    let mut skewed = p0;
    skewed.created_at = p2.created_at + chrono::Duration::days(30);
    write_forged_pattern(&store, &skewed);
    // Lose the index: both a rebuild and an adopting consolidation must keep P2.
    let index_path = store.root().join("patterns").join("index.json");
    fs::remove_file(&index_path).expect("test: drop index");
    let again = store
        .consolidate_pattern(&request(&[&ids[2]], &[]))
        .expect("test: consolidate on a lost index");
    assert!(!again.created);
    assert_eq!(again.pattern.pattern_id, p2.pattern_id);
    assert_eq!(store.rebuild_pattern_index().expect("test: rebuild"), 1);
    let active = store.active_patterns().expect("test: active");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].pattern_id, p2.pattern_id);
}

#[test]
fn steady_state_consolidation_reads_no_history_and_rebuild_stays_possible() {
    let (_tmp, store, pin) = setup();
    let ids: Vec<String> = (0..4).map(|i| eligible(&store, &pin, &format!("task-{i}"))).collect();
    for id in &ids {
        store.consolidate_pattern(&request(&[id], &[])).expect("test: extend");
    }
    let mut index = store.pattern_index().expect("test: index");
    assert_eq!(index.history.len(), 3, "the three superseded patterns are remembered");
    let adoption = store.adopt_unindexed(&mut index).expect("test: adopt");
    assert!(!adoption.changed);
    assert_eq!(adoption.files_read, 0, "no pattern file is read in steady state");
    // Corrupting history files changes nothing a steady-state call looks at.
    for id in &index.history {
        fs::write(store.root().join("patterns").join(format!("{id}.json")), b"garbage").expect("test: corrupt");
    }
    assert!(
        !store
            .consolidate_pattern(&request(&[&ids[3]], &[]))
            .expect("test: repeat")
            .created
    );
    // A rebuild reads everything; the corrupt history files are skipped, the
    // head is the only valid pattern left and stays active.
    assert_eq!(store.rebuild_pattern_index().expect("test: rebuild"), 1);
}

#[test]
fn withdrawn_evidence_is_capped_with_an_exact_count() {
    let (_tmp, store, pin) = setup();
    let ids: Vec<String> = (0..71).map(|i| eligible(&store, &pin, &format!("task-{i}"))).collect();
    let refs: Vec<&String> = ids.iter().take(70).collect();
    store
        .consolidate_pattern(&request(&refs, &[]))
        .expect("test: big pattern");
    for id in ids.iter().take(70) {
        fs::remove_file(store.root().join("experiences").join(format!("{id}.json"))).expect("test: retention");
    }
    let next = store
        .consolidate_pattern(&request(&[&ids[70]], &[]))
        .expect("test: the write is bounded")
        .pattern;
    assert_eq!(next.withdrawn_count, 70);
    assert_eq!(next.withdrawn_evidence.len(), 64);
    assert_eq!(next.evidence_ids, vec![ids[70].clone()]);
    let active = store.active_patterns().expect("test: the store still reads back");
    assert_eq!(active.len(), 1);
}

#[test]
fn applicability_overflow_is_refused_with_a_typed_error() {
    let (_tmp, store, pin) = setup();
    let (a, b) = (eligible(&store, &pin, "task-a"), eligible(&store, &pin, "task-b"));
    let mut first = request(&[&a], &[]);
    first.applicability = (0..16).map(|i| format!("case {i:02}")).collect();
    store.consolidate_pattern(&first).expect("test: full list");
    let mut more = request(&[&b], &[]);
    more.applicability = vec!["one more".to_string()];
    let err = store.consolidate_pattern(&more).expect_err("test: overflow");
    assert!(matches!(err, LifecycleError::KnowledgeLimit { limit: 16, .. }), "{err}");
    assert_eq!(
        store.active_patterns().expect("test: active")[0].applicability.len(),
        16
    );
}

#[test]
fn evidence_that_became_ineligible_is_withdrawn_not_blocking() {
    let (_tmp, store, pin) = setup();
    let ids: Vec<String> = (0..3).map(|i| eligible(&store, &pin, &format!("task-{i}"))).collect();
    store
        .consolidate_pattern(&request(&[&ids[0], &ids[1]], &[]))
        .expect("test: first");
    // Retention removes one experience.
    fs::remove_file(store.root().join("experiences").join(format!("{}.json", ids[1]))).expect("test: remove");
    let next = store
        .consolidate_pattern(&request(&[&ids[2]], &[]))
        .expect("test: the group is not blocked")
        .pattern;
    let mut expected = vec![ids[0].clone(), ids[2].clone()];
    expected.sort();
    assert_eq!(next.evidence_ids, expected);
    assert_eq!(next.withdrawn_evidence, vec![ids[1].clone()]);
    assert_eq!(next.confidence_basis.evidence_count, 2);
    assert_eq!(next.confidence_basis.distinct_tasks, 2);
    assert!(next.usable_for_proposal());
    // Idempotent afterwards.
    assert!(
        !store
            .consolidate_pattern(&request(&[&ids[2]], &[]))
            .expect("test: repeat")
            .created
    );
}

#[test]
fn a_successful_task_is_no_evidence_of_a_guidance_failure() {
    let (_tmp, store, pin) = setup();
    store
        .record_hydration("won", &pin, "SKILL.md", b"x", &Correlation::default())
        .expect("test: hydrate");
    store
        .join_terminal("won", "term", ExperienceOutcome::Success)
        .expect("test: join");
    let won = crate::skills::lifecycle::experience_id(store.owner_scope(), "won");
    let err = store
        .consolidate_pattern(&request(&[&won], &[]))
        .expect_err("test: success as support");
    assert!(matches!(err, LifecycleError::EvidenceNotUsable { .. }), "{err}");
    let failed = eligible(&store, &pin, "lost");
    store
        .consolidate_pattern(&request(&[&failed], &[&won]))
        .expect("test: a success may contradict the lesson");
}

#[test]
fn forged_pattern_content_fails_closed() {
    let (_tmp, store, pin) = setup();
    let id = eligible(&store, &pin, "task-1");
    let pattern = store
        .consolidate_pattern(&request(&[&id], &[]))
        .expect("test: consolidate")
        .pattern;
    let path = store
        .root()
        .join("patterns")
        .join(format!("{}.json", pattern.pattern_id));
    let original = fs::read_to_string(&path).expect("test: pattern");
    let edits: [(&str, serde_json::Value); 4] = [
        ("/confidence_basis/distinct_tasks", serde_json::json!(5)),
        ("/statement", serde_json::json!("Something else entirely.")),
        ("/evidence_ids/0", serde_json::json!("exp-forged")),
        ("/applicability", serde_json::json!(["a\u{7}b"])),
    ];
    for (pointer, value) in edits {
        let mut doc: serde_json::Value = serde_json::from_str(&original).expect("test: json");
        *doc.pointer_mut(pointer).expect("test: pointer") = value;
        fs::write(&path, serde_json::to_vec_pretty(&doc).expect("test: json")).expect("test: write");
        let err = store.read_pattern(&pattern.pattern_id).expect_err(pointer);
        assert!(matches!(err, LifecycleError::Tampered { .. }), "{pointer}: {err}");
    }
    fs::write(&path, &original).expect("test: restore");
    assert!(store.read_pattern(&pattern.pattern_id).expect("test: read").is_some());
}

#[test]
fn sub_runs_of_one_root_task_count_as_one_task() {
    let (_tmp, store, pin) = setup();
    let mut ids = Vec::new();
    for (task, run) in [("child-1", "run-c1"), ("child-2", "run-c2")] {
        let correlation = Correlation {
            run_id: Some(run.to_string()),
            parent_run_id: Some("run-root".to_string()),
            ..Correlation::default()
        };
        store
            .record_hydration(task, &pin, "SKILL.md", b"x", &correlation)
            .expect("test: hydrate");
        store
            .join_terminal(task, "term", ExperienceOutcome::Failure)
            .expect("test: join");
        ids.push(crate::skills::lifecycle::experience_id(store.owner_scope(), task));
    }
    let pattern = store
        .consolidate_pattern(&request(&[&ids[0], &ids[1]], &[]))
        .expect("test: consolidate")
        .pattern;
    assert_eq!(pattern.confidence_basis.evidence_count, 2);
    assert_eq!(pattern.confidence_basis.distinct_tasks, 1);
    assert!(
        !pattern.usable_for_proposal(),
        "two children of one task are one incident"
    );
}

#[test]
fn internal_lock_names_cannot_collide_with_a_skill() {
    assert!(SkillId::parse("_knowledge-patterns").is_err());
    assert!(SkillId::parse("_knowledge-rejections").is_err());
    let (_tmp, store, pin) = setup();
    // A real skill that carries the old lock names holds its own lock.
    let lookalikes: Vec<_> = ["knowledge-patterns", "knowledge-rejections"]
        .iter()
        .map(|name| SkillId::parse(name).expect("test: id"))
        .collect();
    let _held: Vec<_> = lookalikes
        .iter()
        .map(|id| {
            store
                .lock_skill(id, std::time::Duration::from_secs(1))
                .expect("test: lock lookalike")
        })
        .collect();
    let id = eligible(&store, &pin, "task-1");
    store
        .consolidate_pattern(&request(&[&id], &[]))
        .expect("test: consolidation is not blocked by a skill lock");
    store
        .record_rejection(&rejection("prop-1", "text", &[&id], "env"))
        .expect("test: neither is a rejection");
}

#[test]
fn identifier_fields_reject_newlines_and_control_characters() {
    let (_tmp, store, pin) = setup();
    let id = eligible(&store, &pin, "task-1");
    let mut family = request(&[&id], &[]);
    family.task_family = "deploy\nfamily".to_string();
    assert!(store.consolidate_pattern(&family).is_err());
    let mut env = request(&[&id], &[]);
    env.scope.environment = "lo\u{0}cal".to_string();
    assert!(store.consolidate_pattern(&env).is_err());
    let mut capability = request(&[&id], &[]);
    capability.scope.capabilities = vec!["sh\nell".to_string()];
    assert!(store.consolidate_pattern(&capability).is_err());
    let mut text = request(&[&id], &[]);
    text.statement = "line one\nline two".to_string();
    assert!(store.consolidate_pattern(&text).is_ok(), "a statement may span lines");
}

#[test]
fn unrelated_or_successful_experiences_do_not_lift_suppression() {
    let (_tmp, store, pin) = setup();
    let a = eligible(&store, &pin, "task-a");
    store
        .record_rejection(&rejection("prop-1", "Run the migration.", &[&a], "env"))
        .expect("test: record");
    // A real, eligible failure that never hydrated the skill.
    store
        .record_selection("selected-only", &pin, &Correlation::default())
        .expect("test: select");
    store
        .join_terminal("selected-only", "term", ExperienceOutcome::Failure)
        .expect("test: join");
    let unrelated = crate::skills::lifecycle::experience_id(store.owner_scope(), "selected-only");
    // A hydrated run that succeeded.
    store
        .record_hydration("won", &pin, "SKILL.md", b"x", &Correlation::default())
        .expect("test: hydrate");
    store
        .join_terminal("won", "term", ExperienceOutcome::Success)
        .expect("test: join");
    let won = crate::skills::lifecycle::experience_id(store.owner_scope(), "won");
    for other in [&unrelated, &won] {
        assert!(
            ask(
                &store,
                "Run the migration.",
                &[&a, other],
                "env",
                ProposalOperation::Revise
            )
            .is_some(),
            "{other} is not new evidence for this skill"
        );
    }
    let fresh = eligible(&store, &pin, "fresh");
    assert!(
        ask(
            &store,
            "Run the migration.",
            &[&fresh],
            "env",
            ProposalOperation::Revise
        )
        .is_none()
    );
}

#[test]
fn grandchildren_and_a_root_without_a_run_id_are_one_task() {
    let (_tmp, store, pin) = setup();
    let hydrate = |task: &str, run: Option<&str>, parent: Option<&str>| {
        let correlation = Correlation {
            run_id: run.map(ToString::to_string),
            parent_run_id: parent.map(ToString::to_string),
            ..Correlation::default()
        };
        store
            .record_hydration(task, &pin, "SKILL.md", b"x", &correlation)
            .expect("test: hydrate");
        store
            .join_terminal(task, "term", ExperienceOutcome::Failure)
            .expect("test: join");
        crate::skills::lifecycle::experience_id(store.owner_scope(), task)
    };
    let root = hydrate("root-task", None, None);
    let child = hydrate("child", Some("run-c"), Some("root-task"));
    let grandchild = hydrate("grandchild", Some("run-g"), Some("run-c"));
    let pattern = store
        .consolidate_pattern(&request(&[&root, &child, &grandchild], &[]))
        .expect("test: consolidate")
        .pattern;
    assert_eq!(pattern.confidence_basis.evidence_count, 3);
    assert_eq!(pattern.confidence_basis.distinct_tasks, 1);
    let other = hydrate("other-root", Some("run-o"), None);
    let two = store
        .consolidate_pattern(&request(&[&other], &[]))
        .expect("test: second root")
        .pattern;
    assert_eq!(two.confidence_basis.distinct_tasks, 2);
}

#[test]
fn a_forged_rejection_record_fails_closed() {
    let (_tmp, store, pin) = setup();
    let a = eligible(&store, &pin, "task-a");
    let mut new = rejection("prop-1", "text", &[&a], "env");
    new.reasons = vec!["first".to_string(), "second".to_string()];
    let record = store.record_rejection(&new).expect("test: record");
    let path = store
        .root()
        .join("rejections")
        .join(format!("{}.json", record.record_id));
    let original = fs::read_to_string(&path).expect("test: record text");
    let edits: [(&str, serde_json::Value); 4] = [
        ("/fingerprint", serde_json::json!("x")),
        ("/reasons", serde_json::json!(["second", "first"])),
        ("/evidence_ids/0", serde_json::json!("not-an-id")),
        ("/environment_binding", serde_json::json!("e\nnv")),
    ];
    for (pointer, value) in edits {
        let mut doc: serde_json::Value = serde_json::from_str(&original).expect("test: json");
        *doc.pointer_mut(pointer).expect("test: pointer") = value;
        fs::write(&path, serde_json::to_vec_pretty(&doc).expect("test: json")).expect("test: write");
        let err = store.rejections_for(&skill()).expect_err(pointer);
        assert!(matches!(err, LifecycleError::Tampered { .. }), "{pointer}: {err}");
    }
    fs::write(&path, &original).expect("test: restore");
    assert_eq!(store.rejections_for(&skill()).expect("test: read").len(), 1);
}

#[test]
fn staging_sweep_respects_the_reserved_knowledge_locks() {
    let (_tmp, store, _pin) = setup();
    let lock_id = SkillId::reserved("_knowledge-patterns").expect("test: reserved");
    let staged = store.stage_file(&lock_id, b"half written").expect("test: stage");
    let name = staged
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    assert!(name.starts_with("atomic-_knowledge-patterns-"), "{name}");
    {
        let _held = store
            .lock_skill(&lock_id, std::time::Duration::from_secs(1))
            .expect("test: lock");
        store.recover(std::time::Duration::ZERO).expect("test: recover");
        assert!(
            staged.exists(),
            "a staged file under a held knowledge lock is not swept"
        );
    }
    store.recover(std::time::Duration::ZERO).expect("test: recover");
    assert!(!staged.exists(), "an abandoned one is");
}

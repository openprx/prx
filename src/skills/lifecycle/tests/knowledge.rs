#![cfg(test)]

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
    let old = index
        .entries
        .iter()
        .find(|e| e.pattern_id == one.pattern_id)
        .expect("test: old entry");
    assert_eq!(old.superseded_by.as_ref(), Some(&two.pattern_id));
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
    assert_eq!(store.rebuild_pattern_index().expect("test: rebuild"), 2);
    let active = store.active_patterns().expect("test: active");
    assert_eq!(
        active.iter().map(|p| &p.pattern_id).collect::<Vec<_>>(),
        vec![&latest.pattern_id]
    );

    // A pattern file whose index update never happened is repaired by the next call.
    let index_text = fs::read_to_string(&index_path).expect("test: index");
    let mut index: crate::skills::lifecycle::PatternIndex = serde_json::from_str(&index_text).expect("test: parse");
    index.entries.retain(|entry| entry.pattern_id != latest.pattern_id);
    for entry in &mut index.entries {
        entry.superseded_by = None;
    }
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

fn rejection(proposal: &str, fingerprint: &str, evidence: &[&str], environment: &str) -> NewRejection {
    NewRejection {
        proposal_id: proposal.to_string(),
        operation: ProposalOperation::Revise,
        skill_id: skill(),
        fingerprint: fingerprint.to_string(),
        pattern_ids: Vec::new(),
        evidence_ids: evidence.iter().map(ToString::to_string).collect(),
        environment_binding: environment.to_string(),
        reasons: vec!["regressed a critical case".to_string()],
    }
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
}

#[test]
fn an_equivalent_repeat_is_suppressed_until_evidence_or_environment_changes() {
    let (_tmp, store, _pin) = setup();
    let fingerprint = material_digest(&["Run the migration before deploying."]);
    let recorded = store
        .record_rejection(&rejection("prop-1", &fingerprint, &["exp-a", "exp-b"], "env-1"))
        .expect("test: record");
    let ask = |fingerprint: &str, evidence: &[&str], env: &str, op: ProposalOperation| {
        store
            .suppressing_rejection(
                &skill(),
                op,
                fingerprint,
                &evidence.iter().map(ToString::to_string).collect::<Vec<_>>(),
                env,
            )
            .expect("test: check")
    };
    let same = material_digest(&["run the MIGRATION before deploying!"]);
    assert_eq!(
        ask(&same, &["exp-a"], "env-1", ProposalOperation::Revise).map(|r| r.record_id),
        Some(recorded.record_id.clone())
    );
    assert_eq!(
        ask(&same, &["exp-a", "exp-b"], "env-1", ProposalOperation::Revise).map(|r| r.record_id),
        Some(recorded.record_id)
    );
    assert!(
        ask(&same, &["exp-a", "exp-new"], "env-1", ProposalOperation::Revise).is_none(),
        "new evidence justifies a new attempt"
    );
    assert!(
        ask(&same, &["exp-a"], "env-2", ProposalOperation::Revise).is_none(),
        "a changed environment does too"
    );
    assert!(
        ask(
            &material_digest(&["Something else."]),
            &["exp-a"],
            "env-1",
            ProposalOperation::Revise
        )
        .is_none()
    );
    assert!(ask(&same, &["exp-a"], "env-1", ProposalOperation::Disable).is_none());
}

#[test]
fn rejections_are_idempotent_immutable_and_validated() {
    let (_tmp, store, _pin) = setup();
    let first = store
        .record_rejection(&rejection("prop-1", "fp", &["exp-a"], "env"))
        .expect("test: record");
    let again = store
        .record_rejection(&rejection("prop-1", "fp", &["exp-a"], "env"))
        .expect("test: repeat");
    assert_eq!(first, again);
    let err = store
        .record_rejection(&rejection("prop-1", "different", &["exp-a"], "env"))
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
fn rejection_history_survives_publication_and_rollback() {
    let (_tmp, store, pin) = setup();
    let a = pin.revision_id;
    let record = store
        .record_rejection(&rejection("prop-9", "fp-9", &["exp-a"], "env"))
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
    let suppressed = store
        .suppressing_rejection(
            &skill(),
            ProposalOperation::Revise,
            "fp-9",
            &["exp-a".to_string()],
            "env",
        )
        .expect("test: check");
    assert!(
        suppressed.is_some(),
        "a rolled-back skill still remembers the rejected attempt"
    );
}

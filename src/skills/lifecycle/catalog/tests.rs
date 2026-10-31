//! Real owner records and fault injection through the bounded catalog reader.
use super::*;
use crate::skills::lifecycle::tests::{activate, entry, md, owner};
use crate::skills::lifecycle::{
    Bundle, ConsolidationRequest, Correlation, ExperienceOutcome, FailureClass, NewRevision, PatternScope,
    PinnedSkillRevision, SourceKind,
};
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

fn fixture() -> (TempDir, SkillLifecycleStore) {
    let tmp = TempDir::new().expect("temporary owner workspace");
    let store = SkillLifecycleStore::open(tmp.path(), owner("owner:ws:cli:prx_user")).expect("owner store");
    (tmp, store)
}
fn skill(store: &SkillLifecycleStore, number: usize) -> PinnedSkillRevision {
    let id = SkillId::parse(&format!("catalog-skill-{number:03}")).expect("skill identity");
    let bundle = Bundle::from_entries(vec![
        entry(
            "SKILL.md",
            md(&format!("Catalog skill {number}"), "Bounded candidate metadata").as_bytes(),
        ),
        entry("references/private.md", b"PRIVATE-BUNDLE-BODY"),
    ])
    .expect("bundle");
    let revision = store
        .write_revision(&NewRevision::new(id.clone(), SourceKind::Operator), &bundle)
        .expect("revision");
    activate(
        store,
        &id,
        &(revision.revision.revision_id, revision.record_digest),
        0,
        true,
    );
    store.pin_active(&id).expect("pin read").expect("active pin")
}
fn patterns(
    store: &SkillLifecycleStore,
    pin: &PinnedSkillRevision,
    count: usize,
    escaped: bool,
) -> Vec<KnowledgePattern> {
    let evidence: Vec<String> = (0..2)
        .map(|number| {
            let task = format!("{}-incident-{number}", pin.skill_id);
            store
                .record_hydration(
                    &task,
                    pin,
                    "SKILL.md",
                    b"PRIVATE-EXPERIENCE-BODY",
                    &Correlation::default(),
                )
                .expect("hydration");
            store
                .join_terminal(&task, "catalog-terminal", ExperienceOutcome::Failure)
                .expect("terminal");
            crate::skills::lifecycle::experience_id(store.owner_scope(), &task)
        })
        .collect();
    (0..count)
        .map(|number| {
            store
                .consolidate_pattern(&ConsolidationRequest {
                    task_family: "catalog-family".into(),
                    failure_class: FailureClass::MissingGuidance,
                    skill_id: Some(pin.skill_id.clone()),
                    scope: PatternScope {
                        environment: "local".into(),
                        capabilities: vec![],
                    },
                    statement: format!(
                        "Candidate {number}: {}",
                        if escaped {
                            "\\".repeat(1800)
                        } else {
                            "Add a preflight check".into()
                        }
                    ),
                    applicability: vec!["PRIVATE-APPLICABILITY".into()],
                    exceptions: vec!["PRIVATE-EXCEPTION".into()],
                    evidence: evidence.clone(),
                    counterevidence: vec![],
                })
                .expect("real evidence consolidation")
                .pattern
        })
        .collect()
}
fn private_write(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).expect("fault fixture write");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private mode");
}
fn page(store: &SkillLifecycleStore) -> ExperimentCatalog {
    store.experiment_catalog(&ExperimentCatalogRequest::default())
}
fn has_issue(page: &ExperimentCatalog, needle: &str) -> bool {
    page.observations.iter().any(|issue| issue.contains(needle))
}

#[test]
fn catalog_returns_actual_active_candidates_without_private_payloads_or_writes() {
    let (_tmp, store) = fixture();
    let pin = skill(&store, 0);
    let patterns = patterns(&store, &pin, 1, false);
    let pattern = patterns.first().expect("pattern");
    let index_path = store.root().join("patterns/index.json");
    let before = fs::read(&index_path).expect("index bytes");
    let result = page(&store);
    assert_eq!(result.state, ExperimentCatalogState::Complete);
    assert_eq!(result.skills.len(), 1);
    let found = result.skills.first().expect("catalog skill");
    assert_eq!(found.skill_id, pin.skill_id);
    assert_eq!(
        found.patterns.first().expect("candidate").pattern_id,
        pattern.pattern_id.as_str()
    );
    assert!(
        store
            .read_pattern(&pattern.pattern_id)
            .expect("original pattern")
            .expect("present")
            .usable_for_proposal()
    );
    let encoded = serde_json::to_string(&result).expect("encoded catalog");
    for secret in [
        "PRIVATE-BUNDLE-BODY",
        "PRIVATE-EXPERIENCE-BODY",
        "PRIVATE-APPLICABILITY",
        "PRIVATE-EXCEPTION",
        "validators",
        "owner_mac",
        "approval",
        "evidence_ids",
    ] {
        assert!(!encoded.contains(secret), "private field leaked: {secret}");
    }
    assert_eq!(fs::read(index_path).expect("index unchanged"), before);
    assert!(result.input_bytes > 0 && result.input_bytes <= INPUT_BYTES);
}

#[test]
fn missing_index_and_corrupt_pattern_preserve_skill_but_report_partial_candidates() {
    let (_tmp, store) = fixture();
    let pin = skill(&store, 0);
    let patterns = patterns(&store, &pin, 1, false);
    let pattern_path = store
        .root()
        .join("patterns")
        .join(format!("{}.json", patterns.first().expect("pattern").pattern_id));
    private_write(&pattern_path, b"{broken");
    let corrupt = page(&store);
    assert_eq!(corrupt.state, ExperimentCatalogState::Partial);
    assert!(corrupt.skills.first().expect("skill retained").patterns.is_empty());
    assert!(has_issue(&corrupt, "cannot be decoded"));
    fs::remove_file(store.root().join("patterns/index.json")).expect("remove index");
    let missing = page(&store);
    assert_eq!(missing.state, ExperimentCatalogState::Partial);
    assert_eq!(missing.skills.len(), 1);
    assert!(missing.skills.first().expect("skill retained").patterns.is_empty());
    assert!(has_issue(&missing, "unavailable"));
}

#[test]
fn mismatched_revision_and_foreign_pattern_index_never_become_empty_complete_results() {
    let (_tmp, store) = fixture();
    let pin = skill(&store, 0);
    patterns(&store, &pin, 1, false);
    let index_path = store.root().join("patterns/index.json");
    let mut index: PatternIndex = serde_json::from_slice(&fs::read(&index_path).expect("index")).expect("index record");
    index.owner_scope = owner("owner:ws:cli:prx_other");
    private_write(&index_path, &serde_json::to_vec(&index).expect("index encoding"));
    let foreign = page(&store);
    assert_eq!(foreign.state, ExperimentCatalogState::Partial);
    assert!(has_issue(&foreign, "owner or schema mismatch"));
    let revision_path = store
        .root()
        .join("revisions")
        .join(pin.skill_id.as_str())
        .join(pin.revision_id.as_str())
        .join("revision.json");
    let mut revision: SkillRevision =
        serde_json::from_slice(&fs::read(&revision_path).expect("revision")).expect("revision record");
    revision.description = "Modified after its active pin".into();
    private_write(
        &revision_path,
        &serde_json::to_vec(&revision).expect("revision encoding"),
    );
    let mismatch = page(&store);
    assert_eq!(mismatch.state, ExperimentCatalogState::Unavailable);
    assert!(mismatch.skills.is_empty());
    assert!(has_issue(&mismatch, "pinned identity"));
}

#[test]
fn unsafe_or_oversized_record_is_unavailable_and_never_followed() {
    let (_tmp, store) = fixture();
    let pin = skill(&store, 0);
    let binding = store.root().join(ACTIVE_DIR).join(format!("{}.json", pin.skill_id));
    let saved = fs::read(&binding).expect("binding");
    fs::remove_file(&binding).expect("remove binding");
    let external = tempfile::NamedTempFile::new().expect("outside record");
    fs::write(external.path(), &saved).expect("outside bytes");
    std::os::unix::fs::symlink(external.path(), &binding).expect("unsafe link");
    assert_eq!(page(&store).state, ExperimentCatalogState::Unavailable);
    assert_eq!(fs::read(external.path()).expect("outside untouched"), saved);
    fs::remove_file(&binding).expect("remove link");
    private_write(
        &binding,
        &vec![b' '; usize::try_from(MAX_RECORD_BYTES + 1).expect("bounded size")],
    );
    let oversized = page(&store);
    assert_eq!(oversized.state, ExperimentCatalogState::Unavailable);
    assert!(has_issue(&oversized, "input bounds"));
}

#[test]
fn skill_cursor_is_exclusive_and_never_claims_exhaustive_first_page() {
    let (_tmp, store) = fixture();
    let pins: Vec<_> = (0..17).map(|number| skill(&store, number)).collect();
    store.rebuild_pattern_index().expect("empty real index");
    let first = page(&store);
    assert_eq!(first.skills.len(), SKILLS);
    assert_eq!(first.state, ExperimentCatalogState::Partial);
    let cursor = first.next_after_skill.clone().expect("bounded continuation");
    assert_eq!(Some(&cursor), pins.get(15).map(|pin| &pin.skill_id));
    let second = store.experiment_catalog(&ExperimentCatalogRequest {
        after_skill: Some(cursor.clone()),
    });
    assert_eq!(second.state, ExperimentCatalogState::Complete);
    assert_eq!(second.skills.len(), 1);
    assert!(second.skills.iter().all(|skill| skill.skill_id > cursor));
    assert_eq!(
        second.skills.first().map(|skill| &skill.skill_id),
        pins.last().map(|pin| &pin.skill_id)
    );
    assert!(second.next_after_skill.is_none());
    let exhausted = store.experiment_catalog(&ExperimentCatalogRequest {
        after_skill: Some(SkillId::parse("zzzz").expect("cursor")),
    });
    assert_eq!(exhausted.state, ExperimentCatalogState::Complete);
    assert!(exhausted.skills.is_empty());
}

#[test]
fn invalid_directory_names_still_exhaust_the_scan_budget_without_a_false_page() {
    let (_tmp, store) = fixture();
    skill(&store, 0);
    for number in 0..DIRECTORY_ENTRIES {
        private_write(
            &store.root().join(ACTIVE_DIR).join(format!("unexpected-{number}.tmp")),
            b"irrelevant",
        );
    }
    let result = page(&store);
    assert_eq!(result.state, ExperimentCatalogState::Unavailable);
    assert!(result.skills.is_empty());
    assert_eq!(result.scanned_entries, DIRECTORY_ENTRIES + 1);
    assert!(has_issue(&result, "scan limit"));
}

#[test]
fn pattern_window_and_escaped_json_output_are_bounded_with_an_honest_cursor() {
    let (_tmp, store) = fixture();
    for number in 0..16 {
        let pin = skill(&store, number);
        patterns(&store, &pin, 9, true);
    }
    let result = page(&store);
    assert_eq!(result.state, ExperimentCatalogState::Partial);
    assert!(result.skills.iter().all(|skill| skill.patterns.len() <= PATTERNS));
    assert!(result.skills.iter().any(|skill| skill.patterns.len() == PATTERNS));
    assert!(serde_json::to_vec(&result).expect("escaped output").len() <= OUTPUT_BYTES);
    assert!(has_issue(&result, "encoded output page limit"));
    assert_eq!(
        result.next_after_skill,
        result.skills.last().map(|skill| skill.skill_id.clone())
    );
}

#[test]
fn read_budget_rejects_deadline_and_remaining_input_before_decoding() {
    let (_tmp, store) = fixture();
    let pin = skill(&store, 0);
    let path = store.root().join(ACTIVE_DIR).join(format!("{}.json", pin.skill_id));
    let mut expired = ReadBudget {
        deadline: Instant::now() - Duration::from_secs(1),
        bytes: 0,
    };
    assert!(matches!(
        expired.read::<ActiveBinding>(store.root(), &path),
        Err("cooperative scan deadline reached")
    ));
    assert_eq!(expired.bytes, 0);
    let mut exhausted = ReadBudget {
        deadline: Instant::now() + Duration::from_secs(60),
        bytes: INPUT_BYTES - 1,
    };
    assert!(matches!(
        exhausted.read::<ActiveBinding>(store.root(), &path),
        Err("catalog record exceeds input bounds")
    ));
    assert_eq!(exhausted.bytes, INPUT_BYTES - 1);
}

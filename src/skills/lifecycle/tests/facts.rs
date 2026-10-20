#![cfg(test)]

use super::{activate, entry, md, owner};
use crate::skills::lifecycle::{
    Adherence, Bundle, Correlation, ExperienceOutcome, FactState, NewRevision, ObservedCallStatus, ObservedToolCall,
    PinnedSkillRevision, ProviderEnd, SkillId, SkillLifecycleStore, SourceKind, TaskTerminal, TerminalFacts,
    TerminalObservations, on_task_terminal,
};
use std::fs;
use tempfile::TempDir;

const OWNER: &str = "owner:ws:cli:alice";

fn skill() -> SkillId {
    SkillId::parse("deploy-helper").expect("test: skill id")
}

/// A skill that requires two canonical capability ids and also carries a
/// `SKILL.toml` tool (`lint`) that is not part of the oracle.
fn setup() -> (TempDir, SkillLifecycleStore, PinnedSkillRevision) {
    let tmp = TempDir::new().expect("test: tempdir");
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: store");
    let toml = "[skill]\nname = \"Deploy helper\"\ndescription = \"Helps with deploys\"\nversion = \"0.1.0\"\n\n\
                [[tools]]\nname = \"lint\"\ndescription = \"lint\"\nkind = \"shell\"\ncommand = \"true\"\n";
    let bundle = Bundle::from_entries(vec![
        entry("SKILL.md", md("Deploy helper", "Helps with deploys").as_bytes()),
        entry("SKILL.toml", toml.as_bytes()),
    ])
    .expect("test: bundle");
    let mut new = NewRevision::new(skill(), SourceKind::Operator);
    new.capability_requirements = vec![
        "native_tool:native:deploy_check".to_string(),
        "skill:native:skill__deploy-helper__lint".to_string(),
    ];
    let written = store.write_revision(&new, &bundle).expect("test: write");
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

fn call(id: &str, status: ObservedCallStatus) -> ObservedToolCall {
    ObservedToolCall {
        capability_id: id.to_string(),
        status,
    }
}

fn clean(calls: Vec<ObservedToolCall>) -> TerminalObservations {
    TerminalObservations {
        tool_calls: calls,
        complete: true,
        provider: ProviderEnd::Served,
        ..TerminalObservations::default()
    }
}

/// Hydrates the skill in `task`, joins a failure terminal with `observations`
/// through the production call-in and returns the stored facts.
fn terminal_facts(
    tmp: &TempDir,
    store: &SkillLifecycleStore,
    pin: &PinnedSkillRevision,
    task: &str,
    observations: Option<&TerminalObservations>,
) -> TerminalFacts {
    store
        .record_hydration(task, pin, "SKILL.md", b"x", &Correlation::default())
        .expect("test: hydrate");
    on_task_terminal(
        tmp.path(),
        &owner(OWNER),
        &TaskTerminal {
            task_id: task,
            terminal_receipt_id: "term-1",
            outcome: ExperienceOutcome::Failure,
            observations,
        },
    )
    .expect("test: terminal")
    .expect("test: joined")
    .experience
    .terminal
    .expect("test: terminal join")
    .facts
}

const BOTH_CALLED: [(&str, ObservedCallStatus); 2] = [
    ("native_tool:native:deploy_check", ObservedCallStatus::Succeeded),
    ("skill:native:skill__deploy-helper__lint", ObservedCallStatus::Failed),
];

fn both_called() -> Vec<ObservedToolCall> {
    BOTH_CALLED.iter().map(|(id, status)| call(id, *status)).collect()
}

#[test]
fn a_clean_run_that_called_the_declared_tools_is_verified() {
    let (tmp, store, pin) = setup();
    let facts = terminal_facts(&tmp, &store, &pin, "clean", Some(&clean(both_called())));
    assert_eq!(facts.infrastructure_unavailable, FactState::Absent);
    assert_eq!(facts.permission_refused, FactState::Absent);
    assert_eq!(facts.adherence_of(&pin.skill_id, &pin.revision_id), Adherence::Followed);
    assert!(facts.failure_attributable_to(&pin.skill_id, &pin.revision_id));
}

#[test]
fn an_unavailable_provider_is_recorded_and_not_attributable() {
    let (tmp, store, pin) = setup();
    let mut down = clean(both_called());
    down.provider = ProviderEnd::Unavailable;
    let facts = terminal_facts(&tmp, &store, &pin, "down", Some(&down));
    assert_eq!(facts.infrastructure_unavailable, FactState::Present);
    assert!(!facts.failure_attributable_to(&pin.skill_id, &pin.revision_id));

    // The provider class decides: availability problems count, a context overflow does not.
    for class in ["timeout", "rate_limit", "auth", "server_error"] {
        assert_eq!(
            ProviderEnd::from_failure_class(class),
            ProviderEnd::Unavailable,
            "{class}"
        );
    }
    // The catch-all label also covers any agent error (a max-iterations stop).
    for class in ["context_overflow", "provider_error", "anything-else"] {
        assert_eq!(ProviderEnd::from_failure_class(class), ProviderEnd::Unknown, "{class}");
    }

    // A tool whose backend was unavailable is infrastructure.
    let (tmp, store, pin) = setup();
    let mut backend = clean(both_called());
    backend.unavailable_attempts = 1;
    let facts = terminal_facts(&tmp, &store, &pin, "backend-down", Some(&backend));
    assert_eq!(facts.infrastructure_unavailable, FactState::Present);
    assert!(!facts.failure_attributable_to(&pin.skill_id, &pin.revision_id));

    // A tool that failed on its own (bad arguments) leaves a clean run clean.
    let (tmp, store, pin) = setup();
    let facts = terminal_facts(&tmp, &store, &pin, "bad-args", Some(&clean(both_called())));
    assert_eq!(facts.infrastructure_unavailable, FactState::Absent);
    assert!(facts.failure_attributable_to(&pin.skill_id, &pin.revision_id));
}

#[test]
fn a_refused_tool_call_is_recorded_as_a_permission_refusal() {
    let (tmp, store, pin) = setup();
    let mut denied = clean(both_called());
    denied.denied_attempts = 1;
    let facts = terminal_facts(&tmp, &store, &pin, "denied", Some(&denied));
    assert_eq!(facts.permission_refused, FactState::Present);
    assert!(!facts.failure_attributable_to(&pin.skill_id, &pin.revision_id));

    // A denied call in the list counts even when the counter was not carried.
    let (tmp, store, pin) = setup();
    let listed = clean(vec![call("shell:native:shell", ObservedCallStatus::Denied)]);
    let facts = terminal_facts(&tmp, &store, &pin, "listed", Some(&listed));
    assert_eq!(facts.permission_refused, FactState::Present);

    // A run still waiting for approval proves neither way.
    let (tmp, store, pin) = setup();
    let mut waiting = clean(both_called());
    waiting.awaiting_approval_attempts = 1;
    let facts = terminal_facts(&tmp, &store, &pin, "waiting", Some(&waiting));
    assert_eq!(facts.permission_refused, FactState::Unknown);
}

#[test]
fn adherence_needs_every_declared_tool_called_and_a_complete_trace() {
    let (tmp, store, pin) = setup();
    let verdict = |facts: &TerminalFacts| facts.adherence_of(&pin.skill_id, &pin.revision_id);

    // A declared tool that was never called, in a complete trace: not followed.
    let only_one = clean(vec![call(
        "native_tool:native:deploy_check",
        ObservedCallStatus::Succeeded,
    )]);
    let facts = terminal_facts(&tmp, &store, &pin, "missing", Some(&only_one));
    assert_eq!(verdict(&facts), Adherence::NotFollowed);
    assert_eq!(
        facts.adherence.first().map(|entry| entry.missing.clone()),
        Some(vec!["skill:native:skill__deploy-helper__lint".to_string()])
    );
    assert!(!facts.failure_attributable_to(&pin.skill_id, &pin.revision_id));

    // The same gap in an incomplete trace proves nothing.
    let (tmp, store, pin) = setup();
    let mut open = clean(vec![call(
        "native_tool:native:deploy_check",
        ObservedCallStatus::Succeeded,
    )]);
    open.complete = false;
    let facts = terminal_facts(&tmp, &store, &pin, "open", Some(&open));
    assert_eq!(facts.adherence_of(&pin.skill_id, &pin.revision_id), Adherence::Unknown);

    // A declared tool that was refused is neither followed nor ignored.
    let (tmp, store, pin) = setup();
    let refused = vec![
        call("native_tool:native:deploy_check", ObservedCallStatus::Denied),
        call("skill:native:skill__deploy-helper__lint", ObservedCallStatus::Failed),
    ];
    let facts = terminal_facts(&tmp, &store, &pin, "refused", Some(&clean(refused)));
    assert_eq!(facts.adherence_of(&pin.skill_id, &pin.revision_id), Adherence::Unknown);
}

#[test]
fn a_revision_that_declares_nothing_has_no_verifiable_adherence() {
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
    let facts = terminal_facts(&tmp, &store, &pin, "bare", Some(&clean(both_called())));
    assert_eq!(facts.adherence_of(&pin.skill_id, &pin.revision_id), Adherence::Unknown);
    assert!(!facts.failure_attributable_to(&pin.skill_id, &pin.revision_id));
}

#[test]
fn a_terminal_without_observations_leaves_every_fact_unknown_and_replays_keep_the_first() {
    let (tmp, store, pin) = setup();
    let facts = terminal_facts(&tmp, &store, &pin, "blind", None);
    assert_eq!(facts, TerminalFacts::default());
    assert_eq!(facts.infrastructure_unavailable, FactState::Unknown);
    assert!(!facts.failure_attributable_to(&pin.skill_id, &pin.revision_id));

    // The facts of the first join stand when the identical terminal is joined again.
    let (tmp, store, pin) = setup();
    let first = terminal_facts(&tmp, &store, &pin, "twice", Some(&clean(both_called())));
    let mut worse = clean(both_called());
    worse.provider = ProviderEnd::Unavailable;
    let second = terminal_facts(&tmp, &store, &pin, "twice", Some(&worse));
    assert_eq!(first, second);
}

#[test]
fn records_written_before_the_facts_existed_still_parse_as_unknown() {
    let (tmp, store, pin) = setup();
    terminal_facts(&tmp, &store, &pin, "old", Some(&clean(both_called())));
    let id = crate::skills::lifecycle::experience_id(store.owner_scope(), "old");
    let path = store.root().join("experiences").join(format!("{id}.json"));
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("test: read")).expect("test: json");
    let terminal = value
        .get_mut("terminal")
        .and_then(serde_json::Value::as_object_mut)
        .expect("test: terminal object");
    assert!(terminal.remove("facts").is_some(), "the new record carries facts");
    fs::write(&path, serde_json::to_vec(&value).expect("test: encode")).expect("test: write");

    let experience = store
        .read_experience("old")
        .expect("test: an old record stays readable")
        .expect("test: present");
    let facts = experience.terminal.expect("test: terminal").facts;
    assert_eq!(facts, TerminalFacts::default());
    assert!(!facts.failure_attributable_to(&pin.skill_id, &pin.revision_id));

    // Facts with only some fields present parse too.
    let partial: TerminalFacts = serde_json::from_str(r#"{"permission_refused":"present"}"#).expect("test: partial");
    assert_eq!(partial.permission_refused, FactState::Present);
    assert_eq!(partial.infrastructure_unavailable, FactState::Unknown);
    assert!(partial.adherence.is_empty(), "no adherence entries were given");
}

#[test]
fn only_the_full_capability_id_matches_a_requirement() {
    let (tmp, store, pin) = setup();
    // The public name alone, or another namespace, is not the required tool.
    let lookalikes = clean(vec![
        call("native_tool:native:lint", ObservedCallStatus::Succeeded),
        call("other:ns:deploy_check", ObservedCallStatus::Succeeded),
        call("deploy_check", ObservedCallStatus::Succeeded),
    ]);
    let facts = terminal_facts(&tmp, &store, &pin, "lookalike", Some(&lookalikes));
    assert_eq!(
        facts.adherence_of(&pin.skill_id, &pin.revision_id),
        Adherence::NotFollowed
    );
}

#[test]
fn a_skill_toml_tool_is_not_part_of_the_oracle() {
    // `lint` is a SKILL.toml tool and also required here; an `orphan` tool that
    // is declared only in SKILL.toml must not be required.
    let tmp = TempDir::new().expect("test: tempdir");
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: store");
    let toml = "[skill]\nname = \"Deploy helper\"\ndescription = \"Helps with deploys\"\nversion = \"0.1.0\"\n\n\
                [[tools]]\nname = \"orphan\"\ndescription = \"x\"\nkind = \"shell\"\ncommand = \"true\"\n";
    let bundle = Bundle::from_entries(vec![
        entry("SKILL.md", md_required("[native_tool:native:deploy_check]").as_bytes()),
        entry("SKILL.toml", toml.as_bytes()),
    ])
    .expect("test: bundle");
    let written = store
        .write_revision(&NewRevision::new(skill(), SourceKind::Operator), &bundle)
        .expect("test: write");
    assert_eq!(
        written.revision.capability_requirements,
        vec!["native_tool:native:deploy_check".to_string()]
    );
    activate(
        &store,
        &skill(),
        &(written.revision.revision_id, written.record_digest),
        0,
        true,
    );
    let pin = store.pin_active(&skill()).expect("test: pin").expect("test: bound");
    let called = clean(vec![call(
        "native_tool:native:deploy_check",
        ObservedCallStatus::Succeeded,
    )]);
    let facts = terminal_facts(&tmp, &store, &pin, "toml-tool", Some(&called));
    assert_eq!(facts.adherence_of(&pin.skill_id, &pin.revision_id), Adherence::Followed);
}

fn md_required(list: &str) -> String {
    format!("---\nname: Deploy helper\ndescription: Helps with deploys\nrequired-tools: {list}\n---\n\nBody.\n")
}

#[test]
fn a_run_that_read_several_skills_attributes_nothing_to_any_of_them() {
    let (tmp, store, pin) = setup();
    // A second managed skill, also required-tools clean, hydrated in the same run.
    let other = SkillId::parse("other-helper").expect("test: id");
    let bundle = Bundle::from_entries(vec![entry(
        "SKILL.md",
        md_required("[native_tool:native:deploy_check]").as_bytes(),
    )])
    .expect("test: bundle");
    let written = store
        .write_revision(&NewRevision::new(other.clone(), SourceKind::Operator), &bundle)
        .expect("test: write");
    activate(
        &store,
        &other,
        &(written.revision.revision_id, written.record_digest),
        0,
        true,
    );
    let other_pin = store.pin_active(&other).expect("test: pin").expect("test: bound");
    store
        .record_hydration("multi", &other_pin, "SKILL.md", b"x", &Correlation::default())
        .expect("test: hydrate second skill");
    let facts = terminal_facts(&tmp, &store, &pin, "multi", Some(&clean(both_called())));
    assert_eq!(facts.adherence.len(), 2, "both are listed");
    for entry in &facts.adherence {
        assert_eq!(entry.verdict, Adherence::Unknown);
    }
    assert!(!facts.failure_attributable_to(&pin.skill_id, &pin.revision_id));
    assert!(!facts.failure_attributable_to(&other, &other_pin.revision_id));
}

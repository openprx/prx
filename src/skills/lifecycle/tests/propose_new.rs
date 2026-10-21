#![cfg(test)]

//! The zero-baseline proposer (finding 108.1): a NEW instruction skill from
//! repeated work that no managed skill covered, at store level. The end-to-end
//! path through the agent entry, the evaluator, the gate and publication is
//! scenario A of the acceptance suite.

use super::{activate, entry, exec_entry, md, owner};
use crate::skills::lifecycle::proposer_model::TallyCounters;
use crate::skills::lifecycle::{
    Bundle, ConsolidationRequest, ExperienceOutcome, FactState, FailureClass, FailureFacts, KnowledgePattern,
    LifecycleError, ModelBinding, ModelCallError, ModelReply, NewRevision, NewSkillRequest, PatternScope,
    ProposalFailureKind, ProposalOperation, ProposalStatus, ProposeOutcome, ProposerBudget, ProposerModel,
    ReportedUsage, SkillId, SkillLifecycleStore, SourceKind, TerminalFacts, classify_failure, experience_id,
    propose_new_skill,
};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const OWNER: &str = "owner:ws:cli:alice";
const TOOL: &str = "filesystem:native:file_read";
const NAME: &str = "release-notes";
const DESCRIPTION: &str = "Writes release notes for a build";

fn skill() -> SkillId {
    SkillId::parse("release-notes-1").expect("test: skill id")
}

struct Fixture {
    _tmp: TempDir,
    store: Arc<SkillLifecycleStore>,
}

fn fixture() -> Fixture {
    let tmp = TempDir::new().expect("test: tempdir");
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: store");
    Fixture {
        _tmp: tmp,
        store: Arc::new(store),
    }
}

fn facts(infrastructure: FactState, permission: FactState) -> TerminalFacts {
    TerminalFacts {
        infrastructure_unavailable: infrastructure,
        permission_refused: permission,
        adherence: Vec::new(),
    }
}

/// An experience of a task that ran without any managed skill.
fn uncovered(f: &Fixture, task: &str, outcome: ExperienceOutcome, facts: &TerminalFacts) -> String {
    f.store
        .join_terminal_with(task, "term", outcome, facts)
        .expect("test: join");
    experience_id(f.store.owner_scope(), task)
}

fn success(f: &Fixture, task: &str) -> String {
    uncovered(
        f,
        task,
        ExperienceOutcome::Success,
        &facts(FactState::Absent, FactState::Absent),
    )
}

fn request_of(statement: &str, evidence: &[String]) -> ConsolidationRequest {
    ConsolidationRequest {
        task_family: "release".to_string(),
        failure_class: FailureClass::MissingSkill,
        skill_id: None,
        scope: PatternScope {
            environment: "local".to_string(),
            capabilities: vec![TOOL.to_string()],
        },
        statement: statement.to_string(),
        applicability: vec!["release notes tasks".to_string()],
        exceptions: Vec::new(),
        evidence: evidence.to_vec(),
        counterevidence: Vec::new(),
    }
}

fn consolidate(f: &Fixture, evidence: &[String]) -> KnowledgePattern {
    f.store
        .consolidate_pattern(&request_of("Release notes recur without a skill.", evidence))
        .expect("test: consolidate")
        .pattern
}

fn two_task_pattern(f: &Fixture) -> KnowledgePattern {
    let pattern = consolidate(f, &[success(f, "task-1"), success(f, "task-2")]);
    assert!(pattern.usable_for_new_skill());
    pattern
}

fn budget() -> ProposerBudget {
    ProposerBudget::new(64 * 1024, 16 * 1024, Duration::from_secs(5)).expect("test: budget")
}

fn request(id: &str, pattern: &KnowledgePattern) -> NewSkillRequest {
    NewSkillRequest {
        request_id: id.to_string(),
        skill_id: skill(),
        name: NAME.to_string(),
        description: DESCRIPTION.to_string(),
        required_tools: vec![TOOL.to_string()],
        pattern_ids: vec![pattern.pattern_id.clone()],
        budget: budget(),
    }
}

fn patch(edits: &serde_json::Value) -> String {
    json!({"summary": "write the procedure", "edits": edits}).to_string()
}

fn append(text: &str) -> String {
    patch(&json!([{"file": "SKILL.md", "op": "append", "new": text}]))
}

struct Scripted {
    binding: ModelBinding,
    replies: Mutex<VecDeque<String>>,
}

impl Scripted {
    fn new(replies: &[String]) -> Self {
        Self {
            binding: ModelBinding {
                provider_label: "mock".to_string(),
                model: "mock-model".to_string(),
                temperature_milli: 0,
            },
            replies: Mutex::new(replies.iter().cloned().collect()),
        }
    }
}

#[async_trait]
impl ProposerModel for Scripted {
    fn binding(&self) -> &ModelBinding {
        &self.binding
    }
    async fn complete(&self, _s: &str, _u: &str, _t: Arc<TallyCounters>) -> Result<ModelReply, ModelCallError> {
        let next = self.replies.lock().pop_front();
        next.map_or_else(
            || Err(ModelCallError::failed("script exhausted")),
            |text| {
                Ok(ModelReply {
                    text,
                    usage: ReportedUsage::default(),
                })
            },
        )
    }
}

async fn run(f: &Fixture, model: &Scripted, request: NewSkillRequest) -> Result<ProposeOutcome, LifecycleError> {
    propose_new_skill(Arc::clone(&f.store), request, model, &CancellationToken::new()).await
}

fn refused_with(result: &Result<ProposeOutcome, LifecycleError>, needle: &str) {
    match result {
        Err(LifecycleError::ProposalRefused(reason)) => {
            assert!(reason.contains(needle), "{reason:?} does not mention {needle:?}");
        }
        other => panic!("test: expected a refusal mentioning {needle:?}, got {other:?}"),
    }
}

fn failure_of(outcome: &ProposeOutcome) -> ProposalFailureKind {
    match outcome {
        ProposeOutcome::Failed(p) => p.failure.as_ref().expect("test: failure recorded").kind,
        other => panic!("test: expected a failed proposal, got {other:?}"),
    }
}

#[tokio::test]
async fn two_uncovered_tasks_stage_a_bounded_new_skill_and_activate_nothing() {
    let f = fixture();
    let pattern = two_task_pattern(&f);
    let model = Scripted::new(&[append("Step 1: list the changes.\nStep 2: group them.\n")]);
    let ProposeOutcome::Staged(proposal) = run(&f, &model, request("req-1", &pattern))
        .await
        .expect("test: propose")
    else {
        panic!("test: expected a staged proposal");
    };
    assert_eq!(proposal.operation, ProposalOperation::Create);
    assert_eq!(proposal.status, ProposalStatus::Draft);
    assert_eq!(
        (proposal.baseline_revision.clone(), proposal.expected_generation),
        (None, 0)
    );
    assert_eq!(proposal.pattern_ids, vec![pattern.pattern_id.clone()]);
    assert_eq!(proposal.evidence_ids.len(), 2);
    assert_eq!(proposal.basis.failed_tasks_on_baseline, 2);
    assert!(!proposal.basis.adherence_verified);
    let diff = proposal.diff.as_ref().expect("test: diff");
    assert_eq!((diff.edits.len(), diff.allowed_tokens), (1, 300));
    assert!(diff.changed_tokens > 0 && diff.changed_tokens <= 300);

    // The staged candidate is SKILL.md alone, with the request's frontmatter.
    let bundle = f
        .store
        .read_candidate_bundle(&proposal.proposal_id)
        .expect("test: candidate");
    assert_eq!(
        bundle.files().iter().map(|file| file.path()).collect::<Vec<_>>(),
        vec!["SKILL.md"]
    );
    let text =
        String::from_utf8(bundle.file("SKILL.md").expect("test: SKILL.md").bytes().to_vec()).expect("test: utf8");
    assert!(text.starts_with(&format!(
        "---\nname: {NAME}\ndescription: {DESCRIPTION}\nrequired-tools: [{TOOL}]\n---\n"
    )));
    assert!(text.contains("Step 2: group them."));
    assert!(
        f.store.active_binding(&skill()).expect("test: binding").is_none(),
        "nothing is active"
    );
    assert!(
        f.store.read_revision(&skill(), bundle.revision_id(), None).is_err(),
        "nothing is installed yet"
    );

    // The same request id with the same inputs is the same proposal.
    let again = run(&f, &Scripted::new(&[]), request("req-1", &pattern))
        .await
        .expect("test: reuse");
    assert!(matches!(again, ProposeOutcome::Reused(ref reused) if reused.proposal_id == proposal.proposal_id));
    // The same id for another skill is a conflict.
    let mut other = request("req-1", &pattern);
    other.skill_id = SkillId::parse("other-skill").expect("test: skill id");
    assert!(matches!(
        run(&f, &Scripted::new(&[]), other).await,
        Err(LifecycleError::ProposalConflict { .. })
    ));
}

#[tokio::test]
async fn one_distinct_task_cannot_back_a_new_skill() {
    let f = fixture();
    let pattern = consolidate(&f, &[success(&f, "task-1")]);
    assert!(!pattern.usable_for_new_skill());
    let model = Scripted::new(&[append("Step 1.\n")]);
    refused_with(
        &run(&f, &model, request("req-1", &pattern)).await,
        "cannot back a new skill",
    );
}

#[tokio::test]
async fn only_work_that_read_no_skill_and_has_clean_facts_is_evidence() {
    let f = fixture();
    let clean = facts(FactState::Absent, FactState::Absent);

    // A task that read a managed skill was covered.
    let skill_id = SkillId::parse("deploy-helper").expect("test: skill id");
    let bundle = Bundle::from_entries(vec![entry(
        "SKILL.md",
        md("Deploy helper", "Helps with deploys").as_bytes(),
    )])
    .expect("test: bundle");
    let written = f
        .store
        .write_revision(&NewRevision::new(skill_id.clone(), SourceKind::Operator), &bundle)
        .expect("test: write");
    activate(
        &f.store,
        &skill_id,
        &(written.revision.revision_id.clone(), written.record_digest),
        0,
        true,
    );
    let pin = f.store.pin_active(&skill_id).expect("test: pin").expect("test: bound");
    f.store
        .record_hydration(
            "covered",
            &pin,
            "SKILL.md",
            b"x",
            &crate::skills::lifecycle::Correlation::default(),
        )
        .expect("test: hydrate");
    f.store
        .join_terminal_with("covered", "term", ExperienceOutcome::Success, &clean)
        .expect("test: join");
    let covered = experience_id(f.store.owner_scope(), "covered");

    let refusal = |evidence: &[String]| {
        let result = f
            .store
            .consolidate_pattern(&request_of("Release notes recur.", evidence));
        match result {
            Err(LifecycleError::EvidenceNotUsable { reason, .. }) => reason,
            other => panic!("test: expected unusable evidence, got {other:?}"),
        }
    };
    assert!(refusal(&[covered]).contains("covered"));
    // A failure with unknown facts is not evidence; neither is an outage, a
    // refusal, a cancellation or a task waiting for approval.
    let unknown = uncovered(&f, "unknown", ExperienceOutcome::Failure, &TerminalFacts::default());
    assert!(refusal(&[unknown]).contains("not verified"));
    let outage = uncovered(
        &f,
        "outage",
        ExperienceOutcome::Success,
        &facts(FactState::Present, FactState::Absent),
    );
    assert!(refusal(&[outage]).contains("infrastructure"));
    let denied = uncovered(
        &f,
        "denied",
        ExperienceOutcome::Success,
        &facts(FactState::Absent, FactState::Present),
    );
    assert!(refusal(&[denied]).contains("refused"));
    for (task, outcome) in [
        ("cancelled", ExperienceOutcome::Cancelled),
        ("blocked", ExperienceOutcome::Blocked),
        ("unverified", ExperienceOutcome::Unknown),
    ] {
        let id = uncovered(&f, task, outcome, &clean);
        assert!(!refusal(&[id]).is_empty(), "{task}");
    }
    // A failure whose causes are positively absent is evidence, as is a success.
    let verified = uncovered(&f, "verified-failure", ExperienceOutcome::Failure, &clean);
    let pattern = consolidate(&f, &[verified, success(&f, "plain-success")]);
    assert!(pattern.usable_for_new_skill());
}

#[tokio::test]
async fn an_uncovered_pattern_is_about_no_skill_and_never_backs_a_revision() {
    let f = fixture();
    let evidence = [success(&f, "task-1"), success(&f, "task-2")];
    let mut about_a_skill = request_of("Release notes recur without a skill.", &evidence);
    about_a_skill.skill_id = Some(skill());
    assert!(matches!(
        f.store.consolidate_pattern(&about_a_skill),
        Err(LifecycleError::InvalidKnowledge(_))
    ));
    let pattern = consolidate(&f, &evidence);
    assert!(!pattern.usable_for_proposal());
    assert!(pattern.failure_class.allows_new_skill() && !pattern.failure_class.allows_skill_revision());
    assert_eq!(
        classify_failure(&FailureFacts {
            no_skill_covers_the_work: true,
            ..FailureFacts::default()
        }),
        Some(FailureClass::MissingSkill)
    );
    assert_eq!(
        classify_failure(&FailureFacts {
            no_skill_covers_the_work: true,
            provider_or_tool_unavailable: true,
            ..FailureFacts::default()
        }),
        Some(FailureClass::InfrastructureUnavailable),
        "a cause outside the skill outranks missing coverage"
    );
}

#[tokio::test]
async fn a_skill_that_exists_cannot_be_created_again() {
    let f = fixture();
    let pattern = two_task_pattern(&f);
    // Another managed skill already carries the name.
    let bundle = Bundle::from_entries(vec![entry("SKILL.md", md(NAME, "Someone else's notes").as_bytes())])
        .expect("test: bundle");
    let other = SkillId::parse("someone-else").expect("test: skill id");
    let written = f
        .store
        .write_revision(&NewRevision::new(other.clone(), SourceKind::Operator), &bundle)
        .expect("test: write");
    activate(
        &f.store,
        &other,
        &(written.revision.revision_id, written.record_digest),
        0,
        true,
    );
    let model = Scripted::new(&[append("Step 1.\n")]);
    refused_with(&run(&f, &model, request("req-name", &pattern)).await, "already named");

    // The skill id itself is already bound.
    let mut same_id = request("req-id", &pattern);
    same_id.skill_id = other;
    same_id.name = "another-name".to_string();
    refused_with(&run(&f, &model, same_id).await, "already has an active binding");
}

#[tokio::test]
async fn the_skeleton_and_the_tools_are_checked_before_any_model_call() {
    let f = fixture();
    let pattern = two_task_pattern(&f);
    let model = Scripted::new(&[append("Step 1.\n")]);
    let mut wide = request("req-tools", &pattern);
    wide.required_tools = vec![TOOL.to_string(), "filesystem:native:shell".to_string()];
    refused_with(&run(&f, &model, wide).await, "outside the capabilities");
    let mut bad_tool = request("req-bad-tool", &pattern);
    bad_tool.required_tools = vec!["not a capability id".to_string()];
    assert!(run(&f, &model, bad_tool).await.is_err());
    for name in ["", "../escape", "line\nbreak", "-leading"] {
        let mut bad = request("req-name", &pattern);
        bad.name = name.to_string();
        refused_with(&run(&f, &model, bad).await, "skill name");
    }
    for description in ["", "[list]", "a\nb", "# heading"] {
        let mut bad = request("req-description", &pattern);
        bad.description = description.to_string();
        refused_with(&run(&f, &model, bad).await, "description");
    }
}

#[tokio::test]
async fn the_proposer_output_is_bounded_and_instruction_only() {
    let f = fixture();
    let pattern = two_task_pattern(&f);
    let cases: Vec<(&str, String, ProposalFailureKind)> = vec![
        (
            "a replace edit",
            patch(&json!([{"file": "SKILL.md", "op": "replace", "old": "# release-notes", "new": "# other"}])),
            ProposalFailureKind::OutOfScope,
        ),
        (
            "a description",
            json!({"summary": "x", "description": "Other", "edits": [{"file": "SKILL.md", "op": "append", "new": "Step 1.\n"}]})
                .to_string(),
            ProposalFailureKind::OutOfScope,
        ),
        (
            "a script",
            patch(&json!([{"file": "scripts/run.sh", "op": "append", "new": "echo hi\n"}])),
            ProposalFailureKind::OutOfScope,
        ),
        (
            "more than 300 tokens",
            append(&vec!["word"; 301].join(" ")),
            ProposalFailureKind::EditBudgetExceeded,
        ),
        ("no edits", patch(&json!([])), ProposalFailureKind::MalformedOutput),
        ("prose", "I would write a procedure.".to_string(), ProposalFailureKind::MalformedOutput),
    ];
    for (n, (label, reply, expected)) in cases.into_iter().enumerate() {
        let model = Scripted::new(&[reply]);
        let outcome = run(&f, &model, request(&format!("req-{n}"), &pattern))
            .await
            .expect("test: propose");
        assert_eq!(failure_of(&outcome), expected, "{label}");
        let ProposeOutcome::Failed(proposal) = outcome else {
            panic!("test: {label} must fail");
        };
        assert!(proposal.candidate_revision.is_none(), "{label}: nothing is staged");
    }
    // Exactly 300 tokens fit.
    let model = Scripted::new(&[append(&vec!["word"; 300].join(" "))]);
    let outcome = run(&f, &model, request("req-limit", &pattern))
        .await
        .expect("test: propose");
    assert!(matches!(outcome, ProposeOutcome::Staged(_)), "{outcome:?}");
}

#[tokio::test]
async fn publication_checks_that_a_new_skill_is_instruction_only() {
    let f = fixture();
    let pattern = two_task_pattern(&f);
    let model = Scripted::new(&[append("Step 1: list the changes.\n")]);
    let ProposeOutcome::Staged(proposal) = run(&f, &model, request("req-1", &pattern))
        .await
        .expect("test: propose")
    else {
        panic!("test: expected a staged proposal");
    };
    let skeleton =
        format!("---\nname: {NAME}\ndescription: {DESCRIPTION}\nrequired-tools: [{TOOL}]\n---\n\n# {NAME}\n");
    let install = |entries: Vec<crate::skills::lifecycle::BundleEntry>, tools: &[&str]| {
        let mut new = NewRevision::new(skill(), SourceKind::Learned);
        new.capability_requirements = tools.iter().map(|tool| (*tool).to_string()).collect();
        new.proposal_id = Some(proposal.proposal_id.clone());
        let bundle = Bundle::from_entries(entries).expect("test: bundle");
        f.store
            .write_revision(&new, &bundle)
            .expect("test: write")
            .revision
            .revision_id
    };
    let check = |candidate| {
        let mut staged = proposal.clone();
        staged.candidate_revision = Some(candidate);
        f.store.check_capabilities(&staged, None)
    };

    // An instruction-only candidate within the scoped tools passes.
    let fine = install(vec![entry("SKILL.md", skeleton.as_bytes())], &[TOOL]);
    check(fine).expect("test: instruction-only is fine");
    // An executable bundle is refused.
    let script = install(
        vec![
            entry("SKILL.md", skeleton.as_bytes()),
            exec_entry("scripts/run.sh", b"#!/bin/sh\necho hi\n"),
        ],
        &[TOOL],
    );
    assert!(
        matches!(check(script), Err(LifecycleError::CapabilityExpansion { reason, .. }) if reason.contains("not allowed"))
    );
    // A tool outside the scope of its patterns is refused, and so are
    // requirements that differ from the manifest.
    let wide_text = skeleton.replace(TOOL, "filesystem:native:shell");
    let wide = install(
        vec![entry("SKILL.md", wide_text.as_bytes())],
        &["filesystem:native:shell"],
    );
    assert!(
        matches!(check(wide), Err(LifecycleError::CapabilityExpansion { reason, .. }) if reason.contains("scoped"))
    );
    let mismatch = install(
        vec![entry("SKILL.md", format!("{skeleton}\nmismatch\n").as_bytes())],
        &[],
    );
    assert!(
        matches!(check(mismatch), Err(LifecycleError::CapabilityExpansion { reason, .. }) if reason.contains("differ"))
    );
}

#[tokio::test]
async fn a_skill_published_meanwhile_makes_the_new_skill_proposal_stale() {
    let f = fixture();
    let pattern = two_task_pattern(&f);
    let model = Scripted::new(&[append("Step 1: list the changes.\n")]);
    let ProposeOutcome::Staged(proposal) = run(&f, &model, request("req-1", &pattern))
        .await
        .expect("test: propose")
    else {
        panic!("test: expected a staged proposal");
    };
    // Somebody publishes the skill id first.
    let bundle = Bundle::from_entries(vec![entry("SKILL.md", md("Rival notes", "A rival skill").as_bytes())])
        .expect("test: bundle");
    let written = f
        .store
        .write_revision(&NewRevision::new(skill(), SourceKind::Operator), &bundle)
        .expect("test: write");
    activate(
        &f.store,
        &skill(),
        &(written.revision.revision_id, written.record_digest),
        0,
        true,
    );
    let outcome = f
        .store
        .review_source(&proposal.proposal_id, "operator")
        .expect("test: review");
    assert!(!outcome.record.passed && outcome.record.skill_moved);
    assert_eq!(outcome.proposal.status, ProposalStatus::Stale);
}

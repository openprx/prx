#![cfg(test)]
#![allow(clippy::indexing_slicing)]

use super::{activate, entry, exec_entry, owner};
use crate::skills::lifecycle::proposal::ProposalStatus as Status;
use crate::skills::lifecycle::propose::{CallResult, Prepare};
use crate::skills::lifecycle::{
    AttemptOutcome, Bundle, BundleEntry, ConsolidationRequest, Correlation, ExperienceOutcome, FailureClass,
    KnowledgePattern, LifecycleError, ManagedProviderModel, ModelBinding, ModelCallError, ModelReply, NewRejection,
    NewRevision, PatternId, PatternScope, PinnedSkillRevision, ProposalFailureKind, ProposalOperation, ProposeOutcome,
    ProposeRequest, ProposerBudget, ProposerModel, PublishRequest, ReportedUsage, SkillId, SkillLifecycleStore,
    SourceKind, SuppressionStage, TransactionId, propose_revision,
};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::json;
use std::collections::VecDeque;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const OWNER: &str = "owner:ws:cli:alice";

fn skill() -> SkillId {
    SkillId::parse("deploy-helper").expect("test: skill id")
}

fn skill_md_with(steps: usize) -> String {
    let mut text = "---\nname: Deploy helper\ndescription: Helps with deploys\n---\n\n# Deploy\n\n".to_string();
    for n in 1..=steps {
        text.push_str(&format!("Step {n}: run the check number {n} before continuing.\n"));
    }
    text
}

fn tokens(text: &str) -> u64 {
    u64::try_from(text.split_whitespace().count()).expect("test: count")
}

fn baseline_entries(skill_md: &str) -> Vec<BundleEntry> {
    vec![
        entry("SKILL.md", skill_md.as_bytes()),
        entry("references/guide.md", b"# Guide\n"),
        exec_entry("scripts/run.sh", b"#!/bin/sh\necho hi\n"),
    ]
}

struct Fixture {
    tmp: TempDir,
    store: Arc<SkillLifecycleStore>,
    pin: PinnedSkillRevision,
}

fn fixture_with(skill_md: &str) -> Fixture {
    let tmp = TempDir::new().expect("test: tempdir");
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: store");
    let bundle = Bundle::from_entries(baseline_entries(skill_md)).expect("test: bundle");
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
    Fixture {
        tmp,
        store: Arc::new(store),
        pin,
    }
}

fn fixture() -> Fixture {
    fixture_with(&skill_md_with(20))
}

/// An experience that hydrated the pinned revision and ended as `outcome`.
fn experience(f: &Fixture, pin: &PinnedSkillRevision, task: &str, outcome: ExperienceOutcome) -> String {
    f.store
        .record_hydration(task, pin, "SKILL.md", b"x", &Correlation::default())
        .expect("test: hydrate");
    f.store.join_terminal(task, "term", outcome).expect("test: join");
    crate::skills::lifecycle::experience_id(f.store.owner_scope(), task)
}

fn failing(f: &Fixture, task: &str) -> String {
    experience(f, &f.pin, task, ExperienceOutcome::Failure)
}

fn consolidate(f: &Fixture, statement: &str, class: FailureClass, evidence: &[&String]) -> KnowledgePattern {
    f.store
        .consolidate_pattern(&ConsolidationRequest {
            task_family: "deploy".to_string(),
            failure_class: class,
            skill_id: Some(skill()),
            scope: PatternScope {
                environment: "local".to_string(),
                capabilities: vec!["shell".to_string()],
            },
            statement: statement.to_string(),
            applicability: vec!["database deploys".to_string()],
            exceptions: Vec::new(),
            evidence: evidence.iter().map(|id| (*id).clone()).collect(),
            counterevidence: Vec::new(),
        })
        .expect("test: consolidate")
        .pattern
}

const STATEMENT: &str = "The deploy skill omits the migration step.";

/// A usable pattern: two distinct failed tasks on the baseline revision.
fn usable_pattern(f: &Fixture) -> KnowledgePattern {
    let a = failing(f, "task-1");
    let b = failing(f, "task-2");
    let pattern = consolidate(f, STATEMENT, FailureClass::MissingGuidance, &[&a, &b]);
    assert!(pattern.usable_for_proposal());
    pattern
}

fn budget() -> ProposerBudget {
    ProposerBudget::new(64 * 1024, 16 * 1024, Duration::from_secs(5)).expect("test: budget")
}

fn request(id: &str, patterns: &[&KnowledgePattern]) -> ProposeRequest {
    ProposeRequest {
        request_id: id.to_string(),
        skill_id: skill(),
        pattern_ids: patterns.iter().map(|p| p.pattern_id.clone()).collect(),
        budget: budget(),
    }
}

const OLD_STEP: &str = "Step 3: run the check number 3 before continuing.";

fn patch(edits: &serde_json::Value) -> String {
    json!({"summary": "add the migration step", "edits": edits}).to_string()
}

fn good_patch() -> String {
    patch(&json!([{
        "file": "SKILL.md", "op": "replace", "old": OLD_STEP,
        "new": "Step 3: run the check number 3 and the migration step before continuing."
    }]))
}

fn append_patch(words: u64) -> String {
    let text = vec!["word"; usize::try_from(words).expect("test: words")].join(" ");
    patch(&json!([{"file": "SKILL.md", "op": "append", "new": text}]))
}

fn binding() -> ModelBinding {
    ModelBinding {
        provider_label: "mock".to_string(),
        model: "mock-model".to_string(),
        temperature_milli: 0,
    }
}

#[derive(Clone)]
enum Script {
    Reply(String),
    Denied,
    Fail(String),
    Hang,
}

struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct ScriptedModel {
    binding: ModelBinding,
    script: Mutex<VecDeque<Script>>,
    calls: AtomicUsize,
    before: Option<Box<dyn Fn() + Send + Sync>>,
    hung_dropped: Arc<AtomicBool>,
}

impl ScriptedModel {
    fn new(script: Vec<Script>) -> Self {
        Self {
            binding: binding(),
            script: Mutex::new(script.into()),
            calls: AtomicUsize::new(0),
            before: None,
            hung_dropped: Arc::new(AtomicBool::new(false)),
        }
    }
    fn replying(reply: &str) -> Self {
        Self::new(vec![Script::Reply(reply.to_string())])
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ProposerModel for ScriptedModel {
    fn binding(&self) -> &ModelBinding {
        &self.binding
    }
    async fn complete(&self, _system: &str, _user: &str) -> Result<ModelReply, ModelCallError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(before) = &self.before {
            before();
        }
        let next = self.script.lock().pop_front();
        match next {
            Some(Script::Reply(text)) => Ok(ModelReply {
                text,
                usage: ReportedUsage {
                    reported: true,
                    prompt_tokens: Some(100),
                    completion_tokens: Some(40),
                    total_tokens: Some(140),
                },
            }),
            Some(Script::Denied) => Err(ModelCallError::BudgetDenied),
            Some(Script::Fail(message)) => Err(ModelCallError::Failed(message)),
            Some(Script::Hang) => {
                let _guard = DropFlag(Arc::clone(&self.hung_dropped));
                std::future::pending::<Result<ModelReply, ModelCallError>>().await
            }
            None => Err(ModelCallError::Failed("script exhausted".to_string())),
        }
    }
}

async fn run(f: &Fixture, model: &dyn ProposerModel, request: ProposeRequest) -> ProposeOutcome {
    propose_revision(Arc::clone(&f.store), request, model, &CancellationToken::new())
        .await
        .expect("test: propose")
}

fn failure_of(outcome: &ProposeOutcome) -> ProposalFailureKind {
    match outcome {
        ProposeOutcome::Failed(p) => p.failure.as_ref().expect("test: failure recorded").kind,
        other => panic!("test: expected a failed proposal, got {other:?}"),
    }
}

fn proposal_dir(f: &Fixture, id: &str) -> std::path::PathBuf {
    f.store.root().join("proposals").join(id)
}

#[tokio::test]
async fn a_bounded_candidate_is_staged_and_nothing_is_activated() {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let before = f
        .store
        .active_binding(&skill())
        .expect("test: binding")
        .expect("test: bound");
    let model = ScriptedModel::replying(&good_patch());
    let ProposeOutcome::Staged(proposal) = run(&f, &model, request("req-1", &[&pattern])).await else {
        panic!("test: expected a staged proposal");
    };
    assert_eq!(proposal.status, Status::Draft);
    assert_eq!(proposal.operation, ProposalOperation::Revise);
    assert_eq!(proposal.pattern_ids, vec![pattern.pattern_id.clone()]);
    assert_eq!(proposal.evidence_ids.len(), 2);
    assert_eq!(proposal.basis.failed_tasks_on_baseline, 2);
    assert!(!proposal.basis.adherence_verified);
    assert_eq!(proposal.expected_generation, before.generation);
    assert_eq!(proposal.baseline_revision, before.revision_id);
    let diff = proposal.diff.as_ref().expect("test: diff");
    assert_eq!(diff.changed_tokens, 13);
    assert!(diff.changed_tokens <= diff.allowed_tokens && diff.allowed_tokens <= 300);
    assert_eq!(proposal.attempts.len(), 1);
    let attempt = &proposal.attempts[0];
    assert_eq!(attempt.outcome, AttemptOutcome::Completed);
    assert_eq!(attempt.usage.map(|u| u.prompt_tokens), Some(Some(100)));
    assert_eq!(proposal.author.model, binding());

    // The candidate: only SKILL.md differs from the baseline.
    let candidate = f
        .store
        .read_candidate_bundle(&proposal.proposal_id)
        .expect("test: candidate");
    assert_eq!(Some(candidate.revision_id()), proposal.candidate_revision.as_ref());
    assert_ne!(candidate.revision_id(), &before.revision_id);
    let baseline = f
        .store
        .read_revision(&skill(), &before.revision_id, None)
        .expect("test: baseline")
        .bundle;
    let paths: Vec<&str> = candidate.files().iter().map(|file| file.path()).collect();
    assert_eq!(paths, vec!["SKILL.md", "references/guide.md", "scripts/run.sh"]);
    for path in ["references/guide.md", "scripts/run.sh"] {
        let (a, b) = (
            candidate.file(path).expect("test: file"),
            baseline.file(path).expect("test: file"),
        );
        assert_eq!((a.bytes(), a.is_executable()), (b.bytes(), b.is_executable()), "{path}");
    }
    let text = String::from_utf8(candidate.file("SKILL.md").expect("test: md").bytes().to_vec()).expect("test: utf8");
    assert!(text.contains("and the migration step"));

    // Nothing was activated, published or projected.
    let after = f
        .store
        .active_binding(&skill())
        .expect("test: binding")
        .expect("test: bound");
    assert_eq!(after, before, "the active binding is untouched");
    assert!(
        matches!(
            f.store.read_revision(&skill(), candidate.revision_id(), None),
            Err(LifecycleError::RevisionNotFound(_))
        ),
        "the candidate is not a revision"
    );
    assert!(!f.tmp.path().join("skills").exists(), "no compatibility projection");
    assert_eq!(
        f.store.read_proposal(&proposal.proposal_id).expect("test: read"),
        Some(proposal.clone())
    );
    assert_eq!(f.store.list_proposals().expect("test: list").len(), 1);

    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &std::path::Path| fs::metadata(path).expect("test: meta").permissions().mode() & 0o777;
    let dir = proposal_dir(&f, &proposal.proposal_id);
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("proposal.json")), 0o600);
    assert_eq!(mode(&dir.join("candidate/scripts/run.sh")), 0o700);
}

#[tokio::test]
async fn the_same_request_id_is_idempotent_and_different_inputs_conflict() {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let model = ScriptedModel::replying(&good_patch());
    let ProposeOutcome::Staged(first) = run(&f, &model, request("req-1", &[&pattern])).await else {
        panic!("test: staged");
    };
    let ProposeOutcome::Reused(again) = run(&f, &model, request("req-1", &[&pattern])).await else {
        panic!("test: reused");
    };
    assert_eq!(first, again);
    assert_eq!(model.calls(), 1, "a reused proposal costs no second call");

    let other = consolidate(
        &f,
        "A different lesson about rollback.",
        FailureClass::MissingGuidance,
        &[&first.evidence_ids[0], &first.evidence_ids[1]],
    );
    let err = propose_revision(
        Arc::clone(&f.store),
        request("req-1", &[&other]),
        &model,
        &CancellationToken::new(),
    )
    .await
    .expect_err("test: same id, different inputs");
    assert!(matches!(err, LifecycleError::ProposalConflict { .. }), "{err}");
}

#[tokio::test]
async fn malformed_provider_output_fails_without_staging() {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let bad_replies = [
        "not json at all".to_string(),
        "{}".to_string(),
        json!({"edits": []}).to_string(),
        json!({"edits": [{"file": "SKILL.md", "op": "replace", "old": OLD_STEP, "new": "x", "extra": 1}]}).to_string(),
        json!({"trusted": true, "edits": [{"file": "SKILL.md", "op": "append", "new": "x"}]}).to_string(),
        json!({"edits": [{"file": "SKILL.md", "op": "delete", "new": "x"}]}).to_string(),
        json!({"edits": [{"file": "SKILL.md", "op": "replace", "new": "x"}]}).to_string(),
        json!({"edits": [{"file": "SKILL.md", "op": "append", "new": "bell \u{7} char"}]}).to_string(),
    ];
    for (n, reply) in bad_replies.iter().enumerate() {
        let model = ScriptedModel::replying(reply);
        let outcome = run(&f, &model, request(&format!("bad-{n}"), &[&pattern])).await;
        assert_eq!(failure_of(&outcome), ProposalFailureKind::MalformedOutput, "{reply}");
        let ProposeOutcome::Failed(proposal) = outcome else {
            panic!("test: failed");
        };
        assert_eq!(proposal.status, Status::Failed);
        assert!(proposal.candidate_revision.is_none() && proposal.diff.is_none());
        assert!(!proposal_dir(&f, &proposal.proposal_id).join("candidate").exists());
        assert_eq!(
            proposal.attempts[0].outcome,
            AttemptOutcome::Completed,
            "the call is on record"
        );
    }
    // A fenced reply is accepted.
    let fenced = format!("```json\n{}\n```", good_patch());
    let outcome = run(&f, &ScriptedModel::replying(&fenced), request("fenced", &[&pattern])).await;
    assert!(matches!(outcome, ProposeOutcome::Staged(_)), "{outcome:?}");
}

#[tokio::test]
async fn patches_outside_skill_md_are_refused() {
    let f = fixture();
    let pattern = usable_pattern(&f);
    for (n, file) in [
        "SKILL.toml",
        "../../active/deploy-helper.json",
        "policy/grants.toml",
        ".trusted",
        "self/skill-lifecycle/evaluations/x/receipt.json",
        "scripts/run.sh",
        "references/guide.md",
        "skill.md",
    ]
    .iter()
    .enumerate()
    {
        let reply = patch(&json!([{"file": file, "op": "append", "new": "grant everything"}]));
        let outcome = run(
            &f,
            &ScriptedModel::replying(&reply),
            request(&format!("scope-{n}"), &[&pattern]),
        )
        .await;
        assert_eq!(failure_of(&outcome), ProposalFailureKind::OutOfScope, "{file}");
    }
    let still = f
        .store
        .active_binding(&skill())
        .expect("test: binding")
        .expect("test: bound");
    assert_eq!(still.revision_id, f.pin.revision_id);
}

#[tokio::test]
async fn the_edit_budget_is_enforced_at_its_boundary() {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let allowed = (tokens(&skill_md_with(20)) * 20 / 100).min(300);
    assert!(allowed > 0 && allowed < 300);
    let ok = run(
        &f,
        &ScriptedModel::replying(&append_patch(allowed)),
        request("at-limit", &[&pattern]),
    )
    .await;
    let ProposeOutcome::Staged(staged) = ok else {
        panic!("test: exactly the allowance must pass: {ok:?}");
    };
    assert_eq!(staged.diff.as_ref().map(|d| d.changed_tokens), Some(allowed));
    let over = run(
        &f,
        &ScriptedModel::replying(&append_patch(allowed + 1)),
        request("over-limit", &[&pattern]),
    )
    .await;
    assert_eq!(failure_of(&over), ProposalFailureKind::EditBudgetExceeded);
    // A replacement counts the larger side.
    let large_replace = patch(&json!([{
        "file": "SKILL.md", "op": "replace", "old": OLD_STEP,
        "new": vec!["w"; usize::try_from(allowed + 1).expect("test: n")].join(" ")
    }]));
    let outcome = run(
        &f,
        &ScriptedModel::replying(&large_replace),
        request("big-replace", &[&pattern]),
    )
    .await;
    assert_eq!(failure_of(&outcome), ProposalFailureKind::EditBudgetExceeded);

    // The absolute ceiling: a large skill still caps at 300 tokens.
    let big = fixture_with(&skill_md_with(400));
    let big_pattern = usable_pattern(&big);
    let at_cap = run(
        &big,
        &ScriptedModel::replying(&append_patch(300)),
        request("cap", &[&big_pattern]),
    )
    .await;
    assert!(matches!(at_cap, ProposeOutcome::Staged(_)), "{at_cap:?}");
    let over_cap = run(
        &big,
        &ScriptedModel::replying(&append_patch(301)),
        request("cap-over", &[&big_pattern]),
    )
    .await;
    assert_eq!(failure_of(&over_cap), ProposalFailureKind::EditBudgetExceeded);
}

#[tokio::test]
async fn patches_must_apply_exactly_and_keep_the_skill_identity() {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let cases = [
        (
            "missing",
            patch(&json!([{"file": "SKILL.md", "op": "replace", "old": "no such text", "new": "x"}])),
        ),
        (
            "ambiguous",
            patch(&json!([{"file": "SKILL.md", "op": "replace", "old": "Step", "new": "Stage"}])),
        ),
        (
            "frontmatter",
            patch(&json!([{"file": "SKILL.md", "op": "replace", "old": "name: Deploy helper", "new": "name: Evil"}])),
        ),
        (
            "noop",
            patch(&json!([{"file": "SKILL.md", "op": "replace", "old": OLD_STEP, "new": OLD_STEP}])),
        ),
    ];
    for (name, reply) in cases {
        let outcome = run(&f, &ScriptedModel::replying(&reply), request(name, &[&pattern])).await;
        let expected = if name == "noop" {
            ProposalFailureKind::NoChange
        } else {
            ProposalFailureKind::PatchNotApplicable
        };
        assert_eq!(failure_of(&outcome), expected, "{name}");
    }
    // A description change is allowed and counted; a malformed one is not.
    let described = json!({"description": "Helps with deploys and migrations", "edits": []}).to_string();
    let ProposeOutcome::Staged(proposal) =
        run(&f, &ScriptedModel::replying(&described), request("desc", &[&pattern])).await
    else {
        panic!("test: description change");
    };
    let change = proposal
        .diff
        .as_ref()
        .and_then(|d| d.description_change.clone())
        .expect("test: change");
    assert_eq!(change.old, "Helps with deploys");
    let candidate = f
        .store
        .read_candidate_bundle(&proposal.proposal_id)
        .expect("test: candidate");
    let info = crate::skills::lifecycle::validate_manifest(&candidate).expect("test: manifest");
    assert_eq!(
        (info.name.as_str(), info.description.as_str()),
        ("Deploy helper", "Helps with deploys and migrations")
    );
    let injected = json!({"description": "x\nname: Evil", "edits": []}).to_string();
    let outcome = run(&f, &ScriptedModel::replying(&injected), request("inject", &[&pattern])).await;
    assert_eq!(failure_of(&outcome), ProposalFailureKind::InvalidCandidate);
}

#[tokio::test]
async fn provider_failures_are_recorded_attempts_and_never_stage() {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let denied = run(
        &f,
        &ScriptedModel::new(vec![Script::Denied]),
        request("denied", &[&pattern]),
    )
    .await;
    assert_eq!(failure_of(&denied), ProposalFailureKind::BudgetDenied);
    let ProposeOutcome::Failed(p) = denied else {
        panic!("test: failed")
    };
    assert_eq!(p.attempts[0].outcome, AttemptOutcome::Denied);
    assert!(p.attempts[0].usage.is_none());

    let failed = run(
        &f,
        &ScriptedModel::new(vec![Script::Fail("boom".into())]),
        request("boom", &[&pattern]),
    )
    .await;
    assert_eq!(failure_of(&failed), ProposalFailureKind::ProviderFailed);

    let hang = ScriptedModel::new(vec![Script::Hang]);
    let mut short = request("slow", &[&pattern]);
    short.budget = ProposerBudget::new(64 * 1024, 16 * 1024, Duration::from_millis(50)).expect("test: budget");
    let slow = run(&f, &hang, short).await;
    assert_eq!(failure_of(&slow), ProposalFailureKind::TimedOut);
    assert!(
        hang.hung_dropped.load(Ordering::SeqCst),
        "the pending call is dropped at the deadline"
    );
    let ProposeOutcome::Failed(p) = slow else {
        panic!("test: failed")
    };
    assert_eq!(p.attempts[0].outcome, AttemptOutcome::TimedOut);

    let mut small = request("small", &[&pattern]);
    small.budget = ProposerBudget::new(64 * 1024, 100, Duration::from_secs(5)).expect("test: budget");
    let large = run(&f, &ScriptedModel::replying(&good_patch()), small).await;
    assert_eq!(failure_of(&large), ProposalFailureKind::OutputTooLarge);

    assert!(
        f.store
            .list_proposals()
            .expect("test: list")
            .iter()
            .all(|p| p.candidate_revision.is_none())
    );
}

#[tokio::test]
async fn cancellation_stops_the_call_and_is_recorded() {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let model = Arc::new(ScriptedModel::new(vec![Script::Hang]));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let watched = Arc::clone(&model);
    // Cancel only once the call is really pending, however slow the host is.
    let canceller = tokio::spawn(async move {
        while watched.calls() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let outcome = propose_revision(
        Arc::clone(&f.store),
        request("cancel", &[&pattern]),
        model.as_ref(),
        &cancel,
    )
    .await
    .expect("test: propose");
    canceller.await.expect("test: join");
    let ProposeOutcome::Cancelled(proposal) = outcome else {
        panic!("test: expected cancelled, got {outcome:?}");
    };
    assert_eq!(proposal.status, Status::Cancelled);
    assert_eq!(proposal.attempts[0].outcome, AttemptOutcome::Cancelled);
    assert!(model.hung_dropped.load(Ordering::SeqCst), "the pending call is dropped");
    assert!(proposal.candidate_revision.is_none());

    // Cancelled before the call: the model is never called.
    let idle = ScriptedModel::replying(&good_patch());
    let done = CancellationToken::new();
    done.cancel();
    let outcome = propose_revision(Arc::clone(&f.store), request("early", &[&pattern]), &idle, &done)
        .await
        .expect("test: propose");
    assert!(matches!(outcome, ProposeOutcome::Cancelled(_)), "{outcome:?}");
    assert_eq!(idle.calls(), 0);
}

#[tokio::test]
async fn a_cancel_during_the_call_stands_and_keeps_the_real_usage() {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let binding = binding();
    let Prepare::Ready(prepared) = f
        .store
        .prepare_proposal(&request("race", &[&pattern]), &binding)
        .expect("test: prepare")
    else {
        panic!("test: ready");
    };
    let id = f.store.list_proposals().expect("test: list")[0].proposal_id.clone();
    assert_eq!(
        f.store
            .read_proposal(&id)
            .expect("test: read")
            .expect("test: some")
            .attempts[0]
            .outcome,
        AttemptOutcome::InFlight
    );
    let cancelled = f
        .store
        .cancel_proposal(&id, "operator stopped it")
        .expect("test: cancel");
    assert_eq!(cancelled.attempts[0].outcome, AttemptOutcome::Abandoned);
    let reply = ModelReply {
        text: good_patch(),
        usage: ReportedUsage {
            reported: true,
            prompt_tokens: Some(7),
            completion_tokens: Some(3),
            total_tokens: Some(10),
        },
    };
    let outcome = f
        .store
        .finish_proposal(*prepared, CallResult::Reply(reply))
        .expect("test: finish");
    let ProposeOutcome::Cancelled(proposal) = outcome else {
        panic!("test: the cancellation stands");
    };
    assert_eq!(proposal.status, Status::Cancelled);
    assert!(proposal.candidate_revision.is_none());
    assert_eq!(proposal.attempts[0].outcome, AttemptOutcome::Completed);
    assert_eq!(proposal.attempts[0].usage.and_then(|u| u.prompt_tokens), Some(7));
    let err = f.store.cancel_proposal(&id, "again").expect_err("test: second cancel");
    assert!(matches!(err, LifecycleError::InvalidProposalTransition { .. }), "{err}");
}

#[test]
fn the_state_machine_follows_the_specification() {
    use Status::{
        Cancelled, Committed, Draft, Eligible, Evaluated, Evaluating, Failed, Inconclusive, Monitored,
        PublicationAuthorized, Rejected, Retained, SourceReviewed, Stale,
    };
    let chain = [
        Draft,
        SourceReviewed,
        Evaluating,
        Evaluated,
        Eligible,
        PublicationAuthorized,
        Committed,
        Monitored,
        Retained,
    ];
    for pair in chain.windows(2) {
        assert!(pair[0].can_become(pair[1]), "{:?} -> {:?}", pair[0], pair[1]);
    }
    assert!(Evaluated.can_become(Rejected) && Evaluated.can_become(Inconclusive));
    for state in [
        Draft,
        SourceReviewed,
        Evaluating,
        Evaluated,
        Eligible,
        Rejected,
        Inconclusive,
        PublicationAuthorized,
    ] {
        for end in [Cancelled, Stale, Failed] {
            assert!(state.can_become(end), "{state:?} -> {end:?}");
        }
    }
    // Nothing skips a stage, nothing leaves a closed state, nothing commits unauthorised.
    assert!(!Draft.can_become(Eligible));
    assert!(!Draft.can_become(Committed));
    assert!(!Evaluated.can_become(PublicationAuthorized));
    assert!(!Eligible.can_become(Committed));
    assert!(!Committed.can_become(Cancelled) && !Monitored.can_become(Failed));
    for closed in [Cancelled, Stale, Failed] {
        for next in chain {
            assert!(!closed.can_become(next), "{closed:?} -> {next:?}");
        }
    }
}

#[tokio::test]
async fn patterns_that_do_not_rest_on_stored_failures_are_refused_before_any_call() {
    let f = fixture();
    let model = ScriptedModel::replying(&good_patch());

    // One incident only.
    let single = failing(&f, "solo-1");
    let one = consolidate(&f, "One incident.", FailureClass::MissingGuidance, &[&single]);
    let err = propose_revision(
        Arc::clone(&f.store),
        request("one", &[&one]),
        &model,
        &CancellationToken::new(),
    )
    .await
    .expect_err("test: single incident");
    assert!(matches!(err, LifecycleError::ProposalRefused(_)), "{err}");

    // The pattern claims guidance failure, but the stored terminals are cancelled/blocked.
    let c1 = experience(&f, &f.pin, "cancel-1", ExperienceOutcome::Cancelled);
    let c2 = experience(&f, &f.pin, "blocked-1", ExperienceOutcome::Blocked);
    let claimed = consolidate(
        &f,
        "Cancelled tasks look like missing guidance.",
        FailureClass::MissingGuidance,
        &[&c1, &c2],
    );
    assert!(claimed.usable_for_proposal(), "the pattern's own claim passes");
    let err = propose_revision(
        Arc::clone(&f.store),
        request("claimed", &[&claimed]),
        &model,
        &CancellationToken::new(),
    )
    .await
    .expect_err("test: cancelled evidence");
    assert!(
        err.to_string().contains("stored failure") || err.to_string().contains("failed task"),
        "{err}"
    );

    // Other classes never back a revision.
    let a = failing(&f, "task-a");
    let b = failing(&f, "task-b");
    let not_followed = consolidate(
        &f,
        "The skill was not followed.",
        FailureClass::GuidanceNotFollowed,
        &[&a, &b],
    );
    let err = propose_revision(
        Arc::clone(&f.store),
        request("nf", &[&not_followed]),
        &model,
        &CancellationToken::new(),
    )
    .await
    .expect_err("test: adherence pattern");
    assert!(err.to_string().contains("cannot back a skill revision"), "{err}");

    // The prompt must fit the input budget.
    let good = usable_pattern(&f);
    let mut tight = request("tight", &[&good]);
    tight.budget = ProposerBudget::new(64, 1024, Duration::from_secs(1)).expect("test: budget");
    let err = propose_revision(Arc::clone(&f.store), tight, &model, &CancellationToken::new())
        .await
        .expect_err("test: input budget");
    assert!(err.to_string().contains("input budget"), "{err}");

    // An unknown or unbound skill, and a made-up pattern id.
    let mut stranger = request("stranger", &[&good]);
    stranger.skill_id = SkillId::parse("other-skill").expect("test: id");
    assert!(matches!(
        propose_revision(Arc::clone(&f.store), stranger, &model, &CancellationToken::new()).await,
        Err(LifecycleError::ProposalRefused(_))
    ));
    let mut forged = request("forged", &[&good]);
    forged.pattern_ids = vec![PatternId::parse(&format!("pat-{}", "0".repeat(32))).expect("test: id")];
    assert!(matches!(
        propose_revision(Arc::clone(&f.store), forged, &model, &CancellationToken::new()).await,
        Err(LifecycleError::ProposalRefused(_))
    ));
    assert_eq!(model.calls(), 0, "no refusal reached the model");
    assert!(
        f.store.list_proposals().expect("test: list").is_empty(),
        "nothing was recorded"
    );
    assert!(ProposerBudget::new(0, 10, Duration::from_secs(1)).is_err());
    assert!(ProposerBudget::new(10, 0, Duration::from_secs(1)).is_err());
    assert!(ProposerBudget::new(10, 10, Duration::ZERO).is_err());
}

#[tokio::test]
async fn evidence_from_an_older_revision_cannot_back_a_revision_of_the_current_one() {
    let f = fixture();
    let pattern = usable_pattern(&f);
    // Publish revision B on top of A; the failures were all on A.
    let b_md = skill_md_with(21);
    let bundle = Bundle::from_entries(baseline_entries(&b_md)).expect("test: bundle");
    let mut new = NewRevision::new(skill(), SourceKind::Operator);
    new.parent_revision_id = Some(f.pin.revision_id.clone());
    let written = f.store.write_revision(&new, &bundle).expect("test: write");
    activate(
        &f.store,
        &skill(),
        &(written.revision.revision_id, written.record_digest),
        1,
        true,
    );
    let model = ScriptedModel::replying(&good_patch());
    let err = propose_revision(
        Arc::clone(&f.store),
        request("old", &[&pattern]),
        &model,
        &CancellationToken::new(),
    )
    .await
    .expect_err("test: old evidence");
    assert!(matches!(err, LifecycleError::ProposalRefused(_)), "{err}");
    assert_eq!(model.calls(), 0);
}

#[tokio::test]
async fn the_proposal_rests_on_the_reconsolidated_pattern() {
    let f = fixture();
    // Unique task names: the dropped-evidence fallback is process-wide per (owner, task).
    let (a, b, c) = (
        failing(&f, "refresh-1"),
        failing(&f, "refresh-2"),
        failing(&f, "refresh-3"),
    );
    let original = consolidate(&f, STATEMENT, FailureClass::MissingGuidance, &[&a, &b, &c]);
    // The third experience loses its eligibility after the pattern was written.
    f.store.mark_dropped_evidence("refresh-3");
    let model = ScriptedModel::replying(&good_patch());
    let ProposeOutcome::Staged(proposal) = run(&f, &model, request("refresh", &[&original])).await else {
        panic!("test: staged");
    };
    assert_ne!(
        proposal.pattern_ids,
        vec![original.pattern_id.clone()],
        "a successor pattern is used"
    );
    let head = f.store.active_patterns().expect("test: patterns");
    assert_eq!(head.len(), 1);
    assert_eq!(vec![head[0].pattern_id.clone()], proposal.pattern_ids);
    assert_eq!(head[0].supersedes, vec![original.pattern_id.clone()]);
    assert!(
        head[0].withdrawn_evidence.contains(&c),
        "the dropped evidence is withdrawn, not silently kept"
    );
    assert!(!proposal.evidence_ids.contains(&c));
    assert_eq!(proposal.evidence_ids.len(), 2);

    // The superseded id is no longer a head.
    let err = propose_revision(
        Arc::clone(&f.store),
        request("stale-id", &[&original]),
        &model,
        &CancellationToken::new(),
    )
    .await
    .expect_err("test: superseded pattern");
    assert!(err.to_string().contains("not an active pattern"), "{err}");
}

fn reject(proposal: &crate::skills::lifecycle::SkillProposal, f: &Fixture) {
    f.store
        .record_rejection(&NewRejection {
            proposal_id: proposal.proposal_id.clone(),
            operation: ProposalOperation::Revise,
            skill_id: skill(),
            material: proposal.material_texts(),
            pattern_ids: proposal.pattern_ids.clone(),
            evidence_ids: proposal.evidence_ids.clone(),
            environment_binding: proposal.environment_binding.clone(),
            reasons: vec!["regressed a critical case".to_string()],
        })
        .expect("test: record rejection");
}

#[tokio::test]
async fn an_equivalent_rejected_attempt_is_suppressed_before_the_call_until_new_evidence() {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let model = ScriptedModel::new(vec![Script::Reply(good_patch()), Script::Reply(good_patch())]);
    let ProposeOutcome::Staged(first) = run(&f, &model, request("req-1", &[&pattern])).await else {
        panic!("test: staged");
    };
    reject(&first, &f);

    let outcome = run(&f, &model, request("req-2", &[&pattern])).await;
    let ProposeOutcome::Suppressed {
        rejection,
        stage,
        proposal,
    } = outcome
    else {
        panic!("test: expected suppression, got {outcome:?}");
    };
    assert_eq!(stage, SuppressionStage::BeforeCall);
    assert!(proposal.is_none());
    assert_eq!(rejection.proposal_id, first.proposal_id);
    assert_eq!(model.calls(), 1, "no second call was paid for");
    assert_eq!(f.store.list_proposals().expect("test: list").len(), 1);

    // A new failed task on the baseline revision lifts the suppression.
    let third = failing(&f, "task-3");
    let successor = consolidate(&f, STATEMENT, FailureClass::MissingGuidance, &[&third]);
    assert_ne!(successor.pattern_id, pattern.pattern_id);
    let outcome = run(&f, &model, request("req-3", &[&successor])).await;
    assert!(matches!(outcome, ProposeOutcome::Staged(_)), "{outcome:?}");
    assert_eq!(model.calls(), 2);
}

#[tokio::test]
async fn generated_material_equal_to_a_rejected_one_is_suppressed_after_generation() {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let model = ScriptedModel::new(vec![Script::Reply(good_patch()), Script::Reply(good_patch())]);
    let ProposeOutcome::Staged(first) = run(&f, &model, request("req-1", &[&pattern])).await else {
        panic!("test: staged");
    };
    reject(&first, &f);
    // Another lesson from the same experiences: not a repeat before the call.
    let other = consolidate(
        &f,
        "A second lesson about the same failures.",
        FailureClass::MissingGuidance,
        &[&first.evidence_ids[0], &first.evidence_ids[1]],
    );
    let outcome = run(&f, &model, request("req-2", &[&other])).await;
    let ProposeOutcome::Suppressed { stage, proposal, .. } = outcome else {
        panic!("test: expected suppression, got {outcome:?}");
    };
    assert_eq!(stage, SuppressionStage::AfterGeneration);
    let proposal = proposal.expect("test: the call is recorded");
    assert_eq!(model.calls(), 2);
    assert_eq!(proposal.status, Status::Failed);
    assert_eq!(
        proposal.failure.as_ref().map(|x| x.kind),
        Some(ProposalFailureKind::SuppressedByRejection)
    );
    assert!(proposal.candidate_revision.is_none());
    assert!(!proposal_dir(&f, &proposal.proposal_id).join("candidate").exists());
}

#[tokio::test]
async fn a_publication_during_the_call_makes_the_proposal_stale() {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let workspace = f.tmp.path().to_path_buf();
    let parent = f.pin.revision_id.clone();
    let mut model = ScriptedModel::replying(&good_patch());
    model.before = Some(Box::new(move || {
        let other = SkillLifecycleStore::open(&workspace, owner(OWNER)).expect("test: second handle");
        let bundle = Bundle::from_entries(baseline_entries(&skill_md_with(22))).expect("test: bundle");
        let mut new = NewRevision::new(skill(), SourceKind::Operator);
        new.parent_revision_id = Some(parent.clone());
        let written = other.write_revision(&new, &bundle).expect("test: write");
        other
            .publish(&PublishRequest {
                transaction_id: TransactionId::generate(),
                skill_id: skill(),
                candidate: written.revision.revision_id,
                candidate_record_digest: written.record_digest,
                expected_generation: 1,
                enabled: true,
            })
            .expect("test: publish");
    }));
    let outcome = run(&f, &model, request("stale", &[&pattern])).await;
    let ProposeOutcome::Stale(proposal) = outcome else {
        panic!("test: expected stale, got {outcome:?}");
    };
    assert_eq!(proposal.status, Status::Stale);
    assert!(proposal.candidate_revision.is_none());
    assert!(!proposal_dir(&f, &proposal.proposal_id).join("candidate").exists());
    assert_eq!(
        proposal.attempts[0].outcome,
        AttemptOutcome::Completed,
        "the paid call stays on record"
    );
}

#[tokio::test]
async fn tampered_proposals_and_candidates_are_rejected_on_read() {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let model = ScriptedModel::replying(&good_patch());
    let ProposeOutcome::Staged(proposal) = run(&f, &model, request("req-1", &[&pattern])).await else {
        panic!("test: staged");
    };
    assert!(matches!(
        f.store.read_proposal("../escape"),
        Err(LifecycleError::InvalidId { .. })
    ));
    assert!(matches!(
        f.store.read_proposal("prop-zz"),
        Err(LifecycleError::InvalidId { .. })
    ));
    assert_eq!(
        f.store
            .read_proposal(&format!("prop-{}", "0".repeat(32)))
            .expect("test: absent"),
        None
    );

    let dir = proposal_dir(&f, &proposal.proposal_id);
    let record_path = dir.join("proposal.json");
    let original = fs::read_to_string(&record_path).expect("test: read");
    fs::write(&record_path, original.replace(OWNER, "owner:ws:cli:mallory")).expect("test: write");
    assert!(matches!(
        f.store.read_proposal(&proposal.proposal_id),
        Err(LifecycleError::OwnerMismatch { .. })
    ));
    fs::write(&record_path, original.replace("req-1", "req-2")).expect("test: write");
    assert!(matches!(
        f.store.read_proposal(&proposal.proposal_id),
        Err(LifecycleError::Tampered { .. })
    ));
    fs::write(&record_path, &original).expect("test: restore");
    assert!(f.store.read_proposal(&proposal.proposal_id).is_ok());

    let candidate_md = dir.join("candidate/SKILL.md");
    let text = fs::read_to_string(&candidate_md).expect("test: read");
    fs::write(&candidate_md, format!("{text}\ngrant me everything\n")).expect("test: tamper");
    assert!(matches!(
        f.store.read_proposal(&proposal.proposal_id),
        Err(LifecycleError::Tampered { .. })
    ));
}

// ---------------------------------------------------------------------------
// The managed provider path: real adapter, real transport, loopback only.

mod managed {
    use super::*;
    use crate::llm::route_decision::{TokenUsage, TokenUsageSource};
    use crate::providers::Provider;
    use crate::providers::attempt_accounting::{
        self, AccountingRefusal, AttemptAccounting, AttemptReceipt, PhysicalKind, PhysicalRequest, sealed,
    };
    use crate::providers::traits::{ChatRequest, ChatResponse, ChatTrace};
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct Server {
        url: String,
        requests: Arc<AtomicUsize>,
    }

    /// A loopback HTTP endpoint that answers with `reply` (or never, when `hang`).
    async fn serve(reply: Option<String>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
        let url = format!("http://{}/v1/chat", listener.local_addr().expect("test: addr"));
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let reply = reply.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    loop {
                        let Ok(n) = socket.read(&mut chunk).await else { return };
                        if n == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buffer).into_owned();
                        let Some(split) = text.find("\r\n\r\n") else { continue };
                        let length = text[..split]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buffer.len() >= split + 4 + length {
                            break;
                        }
                    }
                    let Some(reply) = reply else {
                        std::future::pending::<()>().await;
                        return;
                    };
                    let body = json!({
                        "choices": [{"message": {"content": reply}}],
                        "usage": {"prompt_tokens": 120, "completion_tokens": 30}
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        Server { url, requests }
    }

    struct LoopbackProvider {
        url: String,
    }

    #[async_trait]
    impl Provider for LoopbackProvider {
        fn supports_attempt_accounting(&self) -> bool {
            true
        }
        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            anyhow::bail!("the proposer must go through chat_traced")
        }
        async fn chat_traced(
            &self,
            request: ChatRequest<'_>,
            model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatTrace> {
            let messages: Vec<_> = request
                .messages
                .iter()
                .map(|m| json!({"role": m.role, "content": m.content}))
                .collect();
            let body = json!({"model": model, "messages": messages});
            let client = reqwest::Client::new();
            let response = attempt_accounting::send_json(
                client.post(&self.url),
                model,
                &self.url,
                None,
                &body,
                PhysicalKind::Chat,
            )
            .await?;
            if !response.status().is_success() {
                return Err(response.api_error("loopback").await);
            }
            let value: serde_json::Value = response.json().await?;
            let text = value["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let number = |name: &str| value["usage"][name].as_u64().and_then(|n| u32::try_from(n).ok());
            Ok(ChatTrace {
                response: ChatResponse {
                    text: Some(text),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                },
                attempts: Vec::new(),
                final_provider: "loopback".to_string(),
                final_model: model.to_string(),
                served_member: None,
                tokens_used: TokenUsage {
                    prompt_tokens: number("prompt_tokens"),
                    completion_tokens: number("completion_tokens"),
                    total_tokens: None,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    source: TokenUsageSource::Reported,
                },
            })
        }
    }

    #[derive(Default)]
    struct MockAccounting {
        deny: bool,
        begun: AtomicUsize,
        settled: Arc<Mutex<Vec<TokenUsage>>>,
    }

    impl sealed::AccountingOwner for MockAccounting {}

    #[async_trait]
    impl AttemptAccounting for MockAccounting {
        fn deadline(&self) -> Instant {
            Instant::now() + Duration::from_secs(60)
        }
        async fn begin(&self, request: PhysicalRequest<'_>) -> anyhow::Result<Box<dyn AttemptReceipt>> {
            assert!(
                !request.body().is_empty(),
                "the reservation is sized from the real body"
            );
            if self.deny {
                return Err(AccountingRefusal.into());
            }
            self.begun.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(MockReceipt(Arc::clone(&self.settled))))
        }
    }

    struct MockReceipt(Arc<Mutex<Vec<TokenUsage>>>);
    impl sealed::Receipt for MockReceipt {}
    #[async_trait]
    impl AttemptReceipt for MockReceipt {
        fn deadline(&self) -> Instant {
            Instant::now() + Duration::from_secs(60)
        }
        async fn settle(self: Box<Self>, usage: TokenUsage) -> anyhow::Result<()> {
            self.0.lock().push(usage);
            Ok(())
        }
    }

    fn managed(url: &str, accounting: Arc<MockAccounting>) -> ManagedProviderModel {
        ManagedProviderModel {
            provider: Arc::new(LoopbackProvider { url: url.to_string() }),
            binding: binding(),
            accounting,
        }
    }

    #[tokio::test]
    async fn the_managed_path_reserves_settles_and_records_reported_usage() {
        let f = fixture();
        let pattern = usable_pattern(&f);
        let server = serve(Some(good_patch())).await;
        let accounting = Arc::new(MockAccounting::default());
        let model = managed(&server.url, Arc::clone(&accounting));
        let outcome = run(&f, &model, request("managed", &[&pattern])).await;
        let ProposeOutcome::Staged(proposal) = outcome else {
            panic!("test: staged, got {outcome:?}");
        };
        assert_eq!(
            accounting.begun.load(Ordering::SeqCst),
            1,
            "one reservation per physical request"
        );
        let settled = accounting.settled.lock().clone();
        assert_eq!(settled.len(), 1);
        assert_eq!(
            (settled[0].prompt_tokens, settled[0].completion_tokens),
            (Some(120), Some(30))
        );
        let usage = proposal.attempts[0].usage.expect("test: usage on record");
        assert_eq!(
            (usage.reported, usage.prompt_tokens, usage.completion_tokens),
            (true, Some(120), Some(30))
        );
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_denied_reservation_sends_nothing_and_fails_the_proposal() {
        let f = fixture();
        let pattern = usable_pattern(&f);
        let server = serve(Some(good_patch())).await;
        let accounting = Arc::new(MockAccounting {
            deny: true,
            ..MockAccounting::default()
        });
        let model = managed(&server.url, Arc::clone(&accounting));
        let outcome = run(&f, &model, request("denied", &[&pattern])).await;
        assert_eq!(failure_of(&outcome), ProposalFailureKind::BudgetDenied);
        let ProposeOutcome::Failed(proposal) = outcome else {
            panic!("test: failed")
        };
        assert_eq!(proposal.attempts[0].outcome, AttemptOutcome::Denied);
        assert_eq!(server.requests.load(Ordering::SeqCst), 0, "the request never left");
        assert!(accounting.settled.lock().is_empty());
    }

    #[tokio::test]
    async fn cancelling_a_managed_call_leaves_its_reservation_unsettled() {
        let f = fixture();
        let pattern = usable_pattern(&f);
        let server = serve(None).await;
        let accounting = Arc::new(MockAccounting::default());
        let model = managed(&server.url, Arc::clone(&accounting));
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        let seen = Arc::clone(&server.requests);
        // Cancel only once the request reached the endpoint and is unanswered.
        let canceller = tokio::spawn(async move {
            while seen.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let outcome = propose_revision(
            Arc::clone(&f.store),
            request("managed-cancel", &[&pattern]),
            &model,
            &cancel,
        )
        .await
        .expect("test: propose");
        canceller.await.expect("test: join");
        let ProposeOutcome::Cancelled(proposal) = outcome else {
            panic!("test: expected cancelled, got {outcome:?}");
        };
        assert_eq!(proposal.attempts[0].outcome, AttemptOutcome::Cancelled);
        assert_eq!(accounting.begun.load(Ordering::SeqCst), 1, "the request was reserved");
        assert!(
            accounting.settled.lock().is_empty(),
            "an unfinished call is never settled as free: the liability stays open"
        );
        assert!(proposal.candidate_revision.is_none());
    }
}

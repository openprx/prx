#![cfg(test)]
#![allow(clippy::indexing_slicing)]

//! SL06: frozen suites and the baseline-versus-candidate evaluator. The
//! mechanics tests use a scripted executor; the entry-path tests at the end
//! drive the real agent entry against a loopback model.

use super::{activate, entry, owner};
use crate::skills::lifecycle::evaluate::{CaseReceipt, rewrite_evaluation_for_test, seal_digest};
use crate::skills::lifecycle::{
    Arm, Bundle, CaseExecutor, CaseVerdict, CleanupState, EvalCase, EvalSplit, EvalSuite, EvaluationError,
    EvaluationOutcome, EvaluationReceipt, EvaluationRequest, ExecutionRequest, ExperienceOutcome,
    ExperienceTerminalFact, FixtureFile, FrozenSuite, HydrationFact, IncompleteReason, InstalledSkill, InvalidReason,
    LifecycleError, NewRevision, RevisionId, RunObservation, RunTerminal, SideEffects, SkillId, SkillLifecycleStore,
    SourceKind, SuiteBudget, Validator,
};
use async_trait::async_trait;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const OWNER: &str = "owner:ws:cli:alice";
pub(super) const ORACLE: &str = "ORACLE-ANSWER-7731";

pub(super) fn skill() -> SkillId {
    SkillId::parse("release-notes").expect("test: skill id")
}

pub(super) struct Fixture {
    pub(super) tmp: TempDir,
    pub(super) store: SkillLifecycleStore,
    pub(super) baseline: RevisionId,
    pub(super) candidate: RevisionId,
}

/// Instruction text every fixture skill carries, so that a one-line edit stays
/// inside the 20% edit budget the source review recomputes.
fn padding() -> String {
    "Follow the release notes style guide carefully.\n".repeat(40)
}

pub(super) fn write_revision(store: &SkillLifecycleStore, body: &str) -> (RevisionId, String) {
    let bundle = candidate_bundle(body);
    let written = store
        .write_revision(&NewRevision::new(skill(), SourceKind::Operator), &bundle)
        .expect("test: write revision");
    (written.revision.revision_id, written.record_digest)
}

/// A store whose skill is at generation 1 with revision A active, and an
/// unpublished candidate revision B.
pub(super) fn fixture() -> Fixture {
    let tmp = TempDir::new().expect("test: tempdir");
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: store");
    let a = write_revision(&store, "ANSWER=draft");
    let b = write_revision(&store, "ANSWER=report-v2");
    activate(&store, &skill(), &a, 0, true);
    Fixture {
        tmp,
        store,
        baseline: a.0,
        candidate: b.0,
    }
}

/// Revision A is active (generation 1); the candidate is only a bundle id: no
/// revision B exists in the store. This is the state `propose_revision`
/// leaves behind, so the lifecycle service must be the one to write it.
pub(super) fn fixture_candidate_unwritten() -> (Fixture, Bundle) {
    let tmp = TempDir::new().expect("test: tempdir");
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: store");
    let a = write_revision(&store, "ANSWER=draft");
    activate(&store, &skill(), &a, 0, true);
    let bundle = candidate_bundle("ANSWER=report-v2");
    let fx = Fixture {
        tmp,
        store,
        baseline: a.0,
        candidate: bundle.revision_id().clone(),
    };
    (fx, bundle)
}

/// The bundle of the fixture skill with the given body.
pub(super) fn candidate_bundle(body: &str) -> Bundle {
    Bundle::from_entries(vec![entry(
        "SKILL.md",
        format!(
            "---\nname: release-notes\ndescription: Writes release notes\n---\n\n{}\n{body}\n",
            padding()
        )
        .as_bytes(),
    )])
    .expect("test: bundle")
}

pub(super) const OWNER_FOR_TESTS: &str = OWNER;

/// Like [`fixture`] but nothing is published yet (generation 0): a new skill.
fn fixture_unpublished() -> Fixture {
    let tmp = TempDir::new().expect("test: tempdir");
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: store");
    let a = write_revision(&store, "ANSWER=draft");
    let b = write_revision(&store, "ANSWER=report-v2");
    Fixture {
        tmp,
        store,
        baseline: a.0,
        candidate: b.0,
    }
}

pub(super) fn case(id: &str, split: EvalSplit, prompt: &str) -> EvalCase {
    EvalCase {
        case_id: id.to_string(),
        split,
        task_family: "notes".to_string(),
        prompt: prompt.to_string(),
        fixtures: Vec::new(),
        validators: vec![Validator::ResponseContains {
            text: ORACLE.to_string(),
        }],
        expect_skill_read: true,
        critical: false,
        max_elapsed_ms: 5_000,
    }
}

pub(super) fn suite_with(cases: Vec<EvalCase>) -> EvalSuite {
    EvalSuite {
        schema_version: 1,
        suite_id: "notes-suite".to_string(),
        owner_scope: owner(OWNER),
        skill_id: skill(),
        trials: 1,
        min_cases: 2,
        allowed_regressions: 0,
        deterministic: true,
        min_gain_cases: 1,
        validation_attempts: 3,
        holdout_attempts: 1,
        required_tools: Vec::new(),
        side_effects: SideEffects::None,
        budget: SuiteBudget {
            max_runs: 64,
            max_total_elapsed_ms: 600_000,
            max_total_tokens: 1_000_000,
            max_total_cost_micros: 1_000_000,
        },
        cases,
    }
}

pub(super) fn validation_suite(count: usize) -> FrozenSuite {
    let cases = (0..count)
        .map(|i| {
            case(
                &format!("case-{i}"),
                EvalSplit::Validation,
                &format!("write release notes number {i}"),
            )
        })
        .collect();
    suite_with(cases).freeze().expect("test: freeze")
}

pub(super) type Script = Box<dyn Fn(&ExecutionRequest<'_>) -> Result<RunObservation, EvaluationError> + Send + Sync>;

/// Scripted executor; remembers every request it was handed.
pub(super) struct Scripted {
    script: Script,
    seen: Mutex<Vec<String>>,
    cancel_after: Option<(usize, CancellationToken)>,
}

impl Scripted {
    pub(super) fn new(
        script: impl Fn(&ExecutionRequest<'_>) -> Result<RunObservation, EvaluationError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            script: Box::new(script),
            seen: Mutex::new(Vec::new()),
            cancel_after: None,
        }
    }
}

#[async_trait]
impl CaseExecutor for Scripted {
    fn identity(&self) -> String {
        "scripted-test-executor".to_string()
    }

    async fn run_case(
        &self,
        request: &ExecutionRequest<'_>,
        _cancel: &CancellationToken,
    ) -> Result<RunObservation, EvaluationError> {
        let count = {
            let mut seen = self.seen.lock();
            seen.push(format!("{request:?}"));
            seen.len()
        };
        if let Some((after, token)) = &self.cancel_after
            && count > *after
        {
            token.cancel();
        }
        (self.script)(request)
    }
}

/// An executor that never finishes a run.
struct Hanging;

#[async_trait]
impl CaseExecutor for Hanging {
    fn identity(&self) -> String {
        "hanging".to_string()
    }

    async fn run_case(
        &self,
        _request: &ExecutionRequest<'_>,
        _cancel: &CancellationToken,
    ) -> Result<RunObservation, EvaluationError> {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        Err(EvaluationError::Executor("never".to_string()))
    }
}

pub(super) fn is_candidate(request: &ExecutionRequest<'_>) -> bool {
    request.skill.is_some_and(|skill| {
        skill
            .bundle
            .files()
            .iter()
            .any(|f| String::from_utf8_lossy(f.bytes()).contains("report-v2"))
    })
}

/// A well-formed observation. The "model" answers with the oracle only when
/// the installed skill is the candidate; it reads the skill when one exists.
pub(super) fn good(request: &ExecutionRequest<'_>) -> RunObservation {
    let installed = request.skill.map(|skill| InstalledSkill {
        skill_id: skill.skill_id.clone(),
        revision_id: skill.bundle.revision_id().clone(),
        bundle_manifest_hash: skill.bundle.revision_id().digest_hex().to_string(),
        generation: 1,
    });
    let hydrations: Vec<HydrationFact> = installed
        .iter()
        .map(|installed| HydrationFact {
            skill_id: installed.skill_id.clone(),
            revision_id: installed.revision_id.clone(),
            generation: installed.generation,
            resource_path: "SKILL.md".to_string(),
            resource_sha256: request
                .skill
                .and_then(|skill| skill.bundle.file("SKILL.md"))
                .map(|file| hex::encode(Sha256::digest(file.bytes())))
                .unwrap_or_default(),
        })
        .collect();
    let experience_terminal = (!hydrations.is_empty()).then(|| ExperienceTerminalFact {
        terminal_receipt_id: "terminal-1".to_string(),
        outcome: ExperienceOutcome::Success,
    });
    RunObservation {
        run_id: request.run_id.to_string(),
        terminal: RunTerminal::Completed,
        response: if is_candidate(request) {
            format!("notes: {ORACLE}")
        } else {
            "notes: draft".to_string()
        },
        elapsed_ms: 10,
        environment_hash: "env-1".to_string(),
        installed,
        hydrations,
        experience_terminal,
        artifacts: BTreeMap::new(),
        cleanup: CleanupState::Removed,
        tokens: Some(10),
        cost_micros: Some(5),
        models_used: vec!["eval-model".to_string()],
        failure_detail: String::new(),
    }
}

pub(super) fn request<'a>(fx: &Fixture, suite: &'a FrozenSuite, split: EvalSplit) -> EvaluationRequest<'a> {
    EvaluationRequest {
        suite,
        split,
        baseline: Some(fx.baseline.clone()),
        candidate: fx.candidate.clone(),
        observed_generation: 1,
    }
}

async fn run(fx: &Fixture, suite: &FrozenSuite, exec: &Scripted) -> EvaluationReceipt {
    fx.store
        .evaluate(
            &request(fx, suite, EvalSplit::Validation),
            exec,
            &CancellationToken::new(),
        )
        .await
        .expect("test: evaluate")
        .receipt
}

// ---- suites ---------------------------------------------------------------

#[test]
fn a_suite_is_frozen_by_content_and_any_edit_changes_its_digest() {
    let base = validation_suite(2);
    let again = validation_suite(2);
    assert_eq!(base.digest(), again.digest());

    let mut edited = suite_with(base.suite().cases.clone());
    edited.cases[0].prompt.push_str(" please");
    assert_ne!(edited.freeze().expect("freeze").digest(), base.digest());

    let mut renamed = suite_with(base.suite().cases.clone());
    renamed.cases[0].case_id = "renamed".to_string();
    assert_ne!(renamed.freeze().expect("freeze").digest(), base.digest());

    let mut filtered = suite_with(base.suite().cases.clone());
    filtered.cases.pop();
    assert_ne!(filtered.freeze().expect("freeze").digest(), base.digest());

    let mut reordered = suite_with(base.suite().cases.clone());
    reordered.cases.reverse();
    assert_eq!(
        reordered.freeze().expect("freeze").digest(),
        base.digest(),
        "case order is canonical, not part of the identity"
    );
}

#[test]
fn an_unsound_suite_cannot_be_frozen() {
    let ok = case("a", EvalSplit::Validation, "prompt one");
    let reject = |suite: EvalSuite, needle: &str| {
        let error = suite.freeze().expect_err("test: must be rejected").to_string();
        assert!(error.contains(needle), "{error}");
    };
    reject(suite_with(Vec::new()), "cases");
    reject(suite_with(vec![ok.clone(), ok.clone()]), "duplicate case id");
    // The same task in two splits would leak the held-out set.
    reject(
        suite_with(vec![ok.clone(), case("b", EvalSplit::Holdout, "  PROMPT   one ")]),
        "disjoint",
    );
    let mut traversal = ok.clone();
    traversal.validators = vec![Validator::ArtifactAbsent {
        path: "../escape".to_string(),
    }];
    reject(suite_with(vec![traversal]), "relative path");
    let mut absolute = ok.clone();
    absolute.fixtures = vec![FixtureFile {
        path: "/etc/passwd".to_string(),
        content: "x".to_string(),
    }];
    reject(suite_with(vec![absolute]), "relative path");
    let mut no_validator = ok.clone();
    no_validator.validators.clear();
    reject(suite_with(vec![no_validator]), "validators");
    let mut trials = suite_with(vec![ok.clone()]);
    trials.trials = 0;
    reject(trials, "trials");
    let mut min = suite_with(vec![ok.clone()]);
    min.min_cases = 0;
    reject(min, "min_cases");
    let mut budget = suite_with(vec![ok, case("c", EvalSplit::Validation, "prompt two")]);
    budget.budget.max_runs = 3;
    reject(budget, "budget");
}

#[test]
fn a_stored_suite_that_was_edited_is_refused() {
    let fx = fixture();
    let suite = validation_suite(2);
    fx.store.write_suite(&suite).expect("test: write suite");
    assert_eq!(fx.store.read_suite(suite.digest()).expect("test: read"), suite);

    let path = fx.store.root().join("suites").join(format!("{}.json", suite.digest()));
    let text = fs::read_to_string(&path).expect("test: read file");
    fs::write(
        &path,
        text.replace("write release notes number 0", "write something easier"),
    )
    .expect("test: edit");
    let error = fx.store.read_suite(suite.digest()).expect_err("test: tampered");
    assert!(
        matches!(error, EvaluationError::Lifecycle(LifecycleError::Tampered { .. })),
        "{error}"
    );

    let mut foreign = suite_with(suite.suite().cases.clone());
    foreign.owner_scope = owner("owner:ws:cli:mallory");
    let foreign = foreign.freeze().expect("test: freeze");
    assert!(
        matches!(
            fx.store.write_suite(&foreign),
            Err(EvaluationError::Lifecycle(LifecycleError::OwnerMismatch { .. }))
        ),
        "a suite of another owner is never stored"
    );
}

// ---- the evaluator --------------------------------------------------------

#[tokio::test]
async fn a_candidate_that_does_better_on_every_case_is_improved_and_every_receipt_is_bound() {
    let fx = fixture();
    let suite = validation_suite(3);
    let exec = Scripted::new(|r| Ok(good(r)));
    let receipt = run(&fx, &suite, &exec).await;
    assert!(receipt.outcome.is_pass(), "{:?}", receipt.outcome);

    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert_eq!(verified.outcome, EvaluationOutcome::Improved);
    assert_eq!(verified.summary.candidate_passed, 3);
    assert_eq!(verified.summary.baseline_passed, 0);
    assert_eq!(verified.cases.len(), 6, "every case ran in both arms");
    let candidate = verified.receipt.candidate.clone();
    let baseline = verified.receipt.baseline.clone().expect("test: baseline");
    for case in &verified.cases {
        assert_eq!(case.suite_digest, suite.digest());
        assert_eq!(case.observed_generation, 1);
        assert_eq!(case.owner_scope, owner(OWNER));
        assert!(suite.case(&case.case_id).is_some());
        let arm = if case.arm == Arm::Candidate {
            &candidate
        } else {
            &baseline
        };
        assert_eq!(case.revision_id.as_ref(), Some(&arm.revision_id));
        assert_eq!(
            case.bundle_manifest_hash.as_deref(),
            Some(arm.bundle_manifest_hash.as_str())
        );
        assert!(case.run.run_id.starts_with("evalrun-"));
        assert_eq!(
            case.run.installed.as_ref().map(|i| &i.revision_id),
            Some(&arm.revision_id)
        );
        assert_eq!(case.run.hydrations.len(), 1, "the skill was actually read");
        assert_eq!(case.run_receipt_digest, seal_digest(&case.run).expect("test: seal"));
    }
    let run_ids: std::collections::BTreeSet<_> = verified.cases.iter().map(|c| c.run.run_id.clone()).collect();
    assert_eq!(run_ids.len(), 6, "every run has its own run receipt");
    assert!(fx.store.evaluation_is_current(&verified).expect("test: current"));
}

#[tokio::test]
async fn arm_order_is_counterbalanced() {
    let fx = fixture();
    let suite = validation_suite(4);
    let exec = Scripted::new(|r| Ok(good(r)));
    let receipt = run(&fx, &suite, &exec).await;
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    let mut first_arms: Vec<Arm> = Vec::new();
    for pair in verified.cases.chunks(2) {
        first_arms.push(pair[0].arm);
        assert_ne!(pair[0].arm, pair[1].arm);
    }
    assert!(first_arms.contains(&Arm::Baseline) && first_arms.contains(&Arm::Candidate));
    assert_eq!(first_arms[0], Arm::Baseline);
    assert_eq!(first_arms[1], Arm::Candidate);
}

#[tokio::test]
async fn a_tie_is_not_an_improvement() {
    let fx = fixture();
    let suite = validation_suite(2);
    let exec = Scripted::new(|r| {
        let mut o = good(r);
        o.response = format!("notes: {ORACLE}");
        Ok(o)
    });
    let receipt = run(&fx, &suite, &exec).await;
    assert_eq!(receipt.outcome, EvaluationOutcome::NotImproved);
    assert!(!receipt.outcome.is_pass());
}

#[tokio::test]
async fn a_critical_regression_hidden_by_an_average_is_regressed() {
    let fx = fixture();
    let mut cases: Vec<EvalCase> = (0..4)
        .map(|i| {
            case(
                &format!("case-{i}"),
                EvalSplit::Validation,
                &format!("write release notes number {i}"),
            )
        })
        .collect();
    cases[3].critical = true;
    let suite = suite_with(cases).freeze().expect("test: freeze");
    // Baseline passes only the critical case; the candidate passes the other three.
    let exec = Scripted::new(|r| {
        let mut o = good(r);
        let critical = r.prompt.ends_with("number 3");
        o.response = match (is_candidate(r), critical) {
            (true, false) | (false, true) => format!("notes: {ORACLE}"),
            _ => "notes: nothing".to_string(),
        };
        Ok(o)
    });
    let receipt = run(&fx, &suite, &exec).await;
    assert_eq!(receipt.outcome, EvaluationOutcome::Regressed);
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert_eq!(verified.summary.critical_regressions, 1);
    assert_eq!(
        verified.summary.candidate_passed, 3,
        "the average improved while the critical case regressed"
    );
    assert_eq!(verified.summary.baseline_passed, 1);
}

#[tokio::test]
async fn a_zero_body_run_is_invalid_and_the_evaluation_cannot_pass() {
    let fx = fixture();
    let suite = validation_suite(2);
    // An empty body satisfies an "excludes" validator, which is exactly why it must not count.
    let mut cases = suite.suite().cases.clone();
    for c in &mut cases {
        c.validators = vec![Validator::ResponseExcludes {
            text: "FORBIDDEN".to_string(),
        }];
        c.expect_skill_read = true;
    }
    let suite = suite_with(cases).freeze().expect("test: freeze");
    let exec = Scripted::new(|r| {
        let mut o = good(r);
        if is_candidate(r) {
            o.response = "  \n".to_string();
        }
        Ok(o)
    });
    let receipt = run(&fx, &suite, &exec).await;
    assert_eq!(
        receipt.outcome,
        EvaluationOutcome::Incomplete {
            reason: IncompleteReason::InvalidRuns { count: 2 }
        }
    );
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert!(verified.cases.iter().any(|c| matches!(
        c.verdict,
        CaseVerdict::Invalid {
            reason: InvalidReason::ZeroBody,
            ..
        }
    )));
    assert!(!verified.outcome.is_pass());
}

#[tokio::test]
async fn partial_timed_out_cancelled_unclean_and_terminalless_runs_are_invalid() {
    type Mutation = fn(&mut RunObservation);
    let cases: [(&str, Mutation, InvalidReason); 5] = [
        ("partial", |o| o.terminal = RunTerminal::Failed, InvalidReason::Partial),
        (
            "timed out",
            |o| o.terminal = RunTerminal::TimedOut,
            InvalidReason::TimedOut,
        ),
        (
            "cancelled",
            |o| o.terminal = RunTerminal::Cancelled,
            InvalidReason::Cancelled,
        ),
        (
            "unclean",
            |o| o.cleanup = CleanupState::Failed,
            InvalidReason::CleanupFailed,
        ),
        (
            "no terminal joined",
            |o| o.experience_terminal = None,
            InvalidReason::MissingTerminal,
        ),
    ];
    for (name, mutate, reason) in cases {
        let fx = fixture();
        let suite = validation_suite(2);
        let exec = Scripted::new(move |r| {
            let mut o = good(r);
            if is_candidate(r) {
                mutate(&mut o);
            }
            Ok(o)
        });
        let receipt = run(&fx, &suite, &exec).await;
        assert!(!receipt.outcome.is_pass(), "{name}: {:?}", receipt.outcome);
        let verified = fx
            .store
            .verify_evaluation(&receipt.record_id, None)
            .expect("test: verify");
        assert!(
            verified
                .cases
                .iter()
                .any(|c| matches!(&c.verdict, CaseVerdict::Invalid { reason: got, .. } if *got == reason)),
            "{name}: {:?}",
            verified.cases.iter().map(|c| &c.verdict).collect::<Vec<_>>()
        );
        assert!(
            matches!(verified.outcome, EvaluationOutcome::Incomplete { .. }),
            "{name}"
        );
    }
}

#[tokio::test]
async fn runs_not_bound_to_their_arm_are_invalid() {
    type Mutation = fn(&ExecutionRequest<'_>, &mut RunObservation, &RevisionId);
    let cases: [(&str, Mutation); 4] = [
        ("wrong run id", |_, o, _| o.run_id = "evalrun-someone-else".to_string()),
        ("wrong installed revision", |_, o, other| {
            if let Some(installed) = &mut o.installed {
                installed.revision_id = other.clone();
            }
        }),
        ("hydrated a different revision", |_, o, other| {
            for h in &mut o.hydrations {
                h.revision_id = other.clone();
            }
        }),
        ("stale hydration generation", |_, o, _| {
            for h in &mut o.hydrations {
                h.generation += 1;
            }
        }),
    ];
    for (name, mutate) in cases {
        let fx = fixture();
        let suite = validation_suite(2);
        let other = fx.baseline.clone();
        let exec = Scripted::new(move |r| {
            let mut o = good(r);
            if is_candidate(r) {
                mutate(r, &mut o, &other);
            }
            Ok(o)
        });
        let receipt = run(&fx, &suite, &exec).await;
        assert!(!receipt.outcome.is_pass(), "{name}");
        let verified = fx
            .store
            .verify_evaluation(&receipt.record_id, None)
            .expect("test: verify");
        assert!(
            verified.cases.iter().any(|c| matches!(
                &c.verdict,
                CaseVerdict::Invalid {
                    reason: InvalidReason::Unbound,
                    ..
                }
            )),
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_skill_read_in_the_no_skill_baseline_is_unbound() {
    let fx = fixture_unpublished();
    let suite = validation_suite(2);
    let exec = Scripted::new(|r| {
        let mut o = good(r);
        if r.skill.is_none() {
            o.hydrations.push(HydrationFact {
                skill_id: skill(),
                revision_id: RevisionId::parse(&format!("r1-{}", "0".repeat(64))).expect("test: id"),
                generation: 1,
                resource_path: "SKILL.md".to_string(),
                resource_sha256: "0".repeat(64),
            });
        }
        Ok(o)
    });
    let mut req = request(&fx, &suite, EvalSplit::Validation);
    req.baseline = None;
    req.observed_generation = 0;
    let receipt = fx
        .store
        .evaluate(&req, &exec, &CancellationToken::new())
        .await
        .expect("test: evaluate");
    assert!(!receipt.outcome.is_pass());
    assert!(receipt.baseline.is_none());
}

#[tokio::test]
async fn a_new_skill_is_compared_with_the_same_tasks_done_without_it() {
    let fx = fixture_unpublished();
    let suite = validation_suite(2);
    let exec = Scripted::new(|r| Ok(good(r)));
    let mut req = request(&fx, &suite, EvalSplit::Validation);
    req.baseline = None;
    req.observed_generation = 0;
    let receipt = fx
        .store
        .evaluate(&req, &exec, &CancellationToken::new())
        .await
        .expect("test: evaluate");
    assert_eq!(receipt.outcome, EvaluationOutcome::Improved);
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert!(
        verified
            .cases
            .iter()
            .filter(|c| c.arm == Arm::Baseline)
            .all(|c| c.revision_id.is_none() && c.run.installed.is_none())
    );
}

#[tokio::test]
async fn a_negative_trigger_case_fails_when_the_skill_is_loaded() {
    let fx = fixture();
    let mut cases = vec![
        case("pos-0", EvalSplit::Validation, "write release notes zero"),
        case("pos-1", EvalSplit::Validation, "write release notes one"),
    ];
    let mut negative = case("neg", EvalSplit::Validation, "what is the capital of France");
    negative.expect_skill_read = false;
    negative.validators = vec![Validator::ResponseContains {
        text: "notes:".to_string(),
    }];
    cases.push(negative);
    let suite = suite_with(cases).freeze().expect("test: freeze");
    // The candidate wrongly loads the skill on the unrelated task.
    let exec = Scripted::new(|r| Ok(good(r)));
    let receipt = run(&fx, &suite, &exec).await;
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    let negative_candidate = verified
        .cases
        .iter()
        .find(|c| c.case_id == "neg" && c.arm == Arm::Candidate)
        .expect("test: receipt");
    assert!(
        matches!(&negative_candidate.verdict, CaseVerdict::Fail { reasons } if reasons.iter().any(|r| r == "skill_read_unexpected")),
        "{:?}",
        negative_candidate.verdict
    );
}

#[tokio::test]
async fn a_case_passes_only_when_every_trial_passes() {
    let fx = fixture();
    let mut s = suite_with(
        (0..2)
            .map(|i| {
                case(
                    &format!("case-{i}"),
                    EvalSplit::Validation,
                    &format!("write release notes number {i}"),
                )
            })
            .collect(),
    );
    s.trials = 2;
    let suite = s.freeze().expect("test: freeze");
    let calls = Arc::new(Mutex::new(0_u32));
    let counter = Arc::clone(&calls);
    // The candidate answers right on the first trial and wrong on the second.
    let exec = Scripted::new(move |r| {
        let mut o = good(r);
        if is_candidate(r) {
            let mut n = counter.lock();
            *n += 1;
            if *n % 2 == 0 {
                o.response = "notes: wrong".to_string();
            }
        }
        Ok(o)
    });
    let receipt = run(&fx, &suite, &exec).await;
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert_eq!(verified.cases.len(), 8);
    assert_eq!(
        verified.summary.candidate_passed, 0,
        "a single failing trial fails the case; no best trial is chosen"
    );
    assert!(!verified.outcome.is_pass());
}

#[tokio::test]
async fn a_cancelled_evaluation_is_partial_and_never_passes() {
    let fx = fixture();
    let suite = validation_suite(3);
    let cancel = CancellationToken::new();
    let mut exec = Scripted::new(|r| Ok(good(r)));
    exec.cancel_after = Some((2, cancel.clone()));
    let receipt = fx
        .store
        .evaluate(&request(&fx, &suite, EvalSplit::Validation), &exec, &cancel)
        .await
        .expect("test: evaluate");
    assert!(receipt.cancelled);
    assert!(receipt.recorded_runs < receipt.planned_runs);
    assert!(!receipt.outcome.is_pass());
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert_eq!(
        verified.outcome,
        EvaluationOutcome::Incomplete {
            reason: IncompleteReason::Cancelled
        }
    );
}

fn reseal(dir: &std::path::Path, receipt: &EvaluationReceipt) {
    let digest = seal_digest(receipt).expect("test: seal");
    let body = serde_json::json!({ "receipt": receipt, "digest": digest });
    fs::write(
        dir.join("receipt.json"),
        serde_json::to_vec_pretty(&body).expect("test: json"),
    )
    .expect("test: write");
}

#[tokio::test]
async fn a_missing_case_receipt_makes_the_evaluation_incomplete() {
    let fx = fixture();
    let suite = validation_suite(3);
    let exec = Scripted::new(|r| Ok(good(r)));
    let receipt = run(&fx, &suite, &exec).await;
    assert!(receipt.outcome.is_pass());

    // Drop one case receipt and keep every count consistent; the stored
    // "improved" is still refused, because the outcome is recomputed.
    let drop_first = |r: &mut EvaluationReceipt, cases: &mut Vec<CaseReceipt>| {
        let removed = cases.remove(0);
        r.recorded_runs -= 1;
        r.total_elapsed_ms -= removed.run.elapsed_ms;
    };
    rewrite_evaluation_for_test(&fx.store, &receipt.record_id, drop_first);
    let error = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect_err("test: outcome no longer matches");
    assert!(error.to_string().contains("recomputed"), "{error}");

    // Once the stored outcome says incomplete too, verification agrees: still not a pass.
    rewrite_evaluation_for_test(&fx.store, &receipt.record_id, |r, _| {
        r.outcome = EvaluationOutcome::Incomplete {
            reason: IncompleteReason::MissingCases { count: 1 },
        };
    });
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert!(!verified.outcome.is_pass());
}

#[tokio::test]
async fn forged_scores_and_edited_receipts_are_refused() {
    let fx = fixture();
    let suite = validation_suite(2);
    // The candidate is no better than the baseline: the honest outcome is NotImproved.
    let exec = Scripted::new(|r| {
        let mut o = good(r);
        o.response = "notes: nothing useful".to_string();
        Ok(o)
    });
    let receipt = run(&fx, &suite, &exec).await;
    assert_eq!(receipt.outcome, EvaluationOutcome::NotImproved);
    let dir = fx.store.root().join("evaluations").join(&receipt.record_id);

    // 1. A fixed pass written into the receipt and correctly re-sealed.
    let mut forged = receipt.clone();
    forged.outcome = EvaluationOutcome::Improved;
    reseal(&dir, &forged);
    let error = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect_err("test: forged outcome");
    assert!(error.to_string().contains("recomputed"), "{error}");

    // 2. A flipped verdict inside a case receipt breaks that receipt's digest.
    reseal(&dir, &receipt);
    let path = dir.join("cases.jsonl");
    let original = fs::read_to_string(&path).expect("test: read");
    fs::write(&path, original.replacen("\"tokens\":10", "\"tokens\":11", 1)).expect("test: edit");
    let error = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect_err("test: edited case");
    assert!(error.to_string().to_lowercase().contains("digest"), "{error}");

    // 3. A pinned digest that is not the stored one is refused.
    fs::write(&path, &original).expect("test: restore");
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert!(
        fx.store
            .verify_evaluation(&receipt.record_id, Some(&verified.receipt_digest))
            .is_ok()
    );
    assert!(
        fx.store
            .verify_evaluation(&receipt.record_id, Some(&"0".repeat(64)))
            .is_err()
    );
}

#[tokio::test]
async fn a_case_receipt_of_another_evaluation_or_suite_does_not_count() {
    let fx = fixture();
    let suite = validation_suite(2);
    let exec = Scripted::new(|r| Ok(good(r)));
    let first = run(&fx, &suite, &exec).await;
    // Every receipt is individually sound but bound to the wrong evaluation.
    rewrite_evaluation_for_test(&fx.store, &first.record_id, |r, cases| {
        for case in cases.iter_mut() {
            case.evaluation_id = format!("eval-{}", "0".repeat(32));
        }
        r.outcome = EvaluationOutcome::Incomplete {
            reason: IncompleteReason::Unbound {
                case_id: "case-0".to_string(),
                detail: "the case receipt is bound to a different evaluation, suite, owner, skill or generation"
                    .to_string(),
            },
        };
    });
    let verified = fx
        .store
        .verify_evaluation(&first.record_id, None)
        .expect("test: verify");
    assert!(matches!(
        verified.outcome,
        EvaluationOutcome::Incomplete {
            reason: IncompleteReason::Unbound { .. }
        }
    ));
    assert!(!verified.outcome.is_pass());
}

#[tokio::test]
async fn a_stale_proposal_is_refused_before_anything_runs() {
    let fx = fixture();
    let suite = validation_suite(2);
    let exec = Scripted::new(|r| Ok(good(r)));
    let mut req = request(&fx, &suite, EvalSplit::Validation);
    req.observed_generation = 0;
    let error = fx
        .store
        .evaluate(&req, &exec, &CancellationToken::new())
        .await
        .expect_err("test: stale generation");
    assert!(matches!(error, EvaluationError::Stale(_)), "{error}");

    let mut req = request(&fx, &suite, EvalSplit::Validation);
    req.baseline = Some(fx.candidate.clone());
    req.candidate = fx.baseline.clone();
    let error = fx
        .store
        .evaluate(&req, &exec, &CancellationToken::new())
        .await
        .expect_err("test: baseline is not active");
    assert!(matches!(error, EvaluationError::Stale(_)), "{error}");
    assert!(exec.seen.lock().is_empty(), "no run was spent on a stale request");
    assert_eq!(
        fx.store.list_evaluation_ids().expect("test: list"),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn a_publication_during_the_run_makes_the_evaluation_stale() {
    let fx = fixture();
    let suite = validation_suite(2);
    let published = Arc::new(Mutex::new(false));
    let flag = Arc::clone(&published);
    let root = fx
        .store
        .root()
        .parent()
        .and_then(|p| p.parent())
        .map(std::path::Path::to_path_buf)
        .expect("test: workspace");
    let candidate = fx.candidate.clone();
    let exec = Scripted::new(move |r| {
        let mut done = flag.lock();
        if !*done {
            *done = true;
            // Another writer publishes the candidate while the evaluation runs.
            let other = SkillLifecycleStore::open(&root, owner(OWNER)).expect("test: reopen");
            let record = other
                .read_revision(&skill(), &candidate, None)
                .expect("test: revision")
                .record_digest;
            activate(&other, &skill(), &(candidate.clone(), record), 1, true);
        }
        Ok(good(r))
    });
    let receipt = run(&fx, &suite, &exec).await;
    assert!(receipt.stale);
    assert_eq!(receipt.outcome, EvaluationOutcome::Stale);
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert!(!verified.outcome.is_pass());
    assert!(!fx.store.evaluation_is_current(&verified).expect("test: current"));
}

#[tokio::test]
async fn environment_drift_between_runs_is_invalid() {
    let fx = fixture();
    let suite = validation_suite(2);
    let exec = Scripted::new(|r| {
        let mut o = good(r);
        if is_candidate(r) {
            o.environment_hash = "env-2".to_string();
        }
        Ok(o)
    });
    let receipt = run(&fx, &suite, &exec).await;
    assert!(!receipt.outcome.is_pass());
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert!(verified.cases.iter().any(|c| matches!(
        &c.verdict,
        CaseVerdict::Invalid {
            reason: InvalidReason::EnvironmentMismatch,
            ..
        }
    )));
}

#[tokio::test]
async fn a_failing_executor_error_reaches_the_receipt() {
    let fx = fixture();
    let suite = validation_suite(2);
    let marker = "could not switch to WAL";
    let exec = Scripted::new(|request| {
        let mut observation = good(request);
        observation.terminal = RunTerminal::Failed;
        observation.response.clear();
        observation.failure_detail = marker.to_string();
        if is_candidate(request) {
            observation.environment_hash = "env-failed".to_string();
        }
        Ok(observation)
    });
    let receipt = run(&fx, &suite, &exec).await;
    assert!(!receipt.outcome.is_pass());
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert!(
        verified
            .cases
            .iter()
            .all(|case| case.run.failure_detail.contains(marker))
    );
    assert!(verified.cases.iter().any(|case| matches!(
        &case.verdict,
        CaseVerdict::Invalid { detail, .. } if detail.contains(marker)
    )));
}

#[tokio::test]
async fn an_executor_error_is_a_recorded_invalid_run_not_a_missing_one() {
    let fx = fixture();
    let suite = validation_suite(2);
    let exec = Scripted::new(|r| {
        if is_candidate(r) {
            Err(EvaluationError::Executor("boom".to_string()))
        } else {
            Ok(good(r))
        }
    });
    let receipt = run(&fx, &suite, &exec).await;
    assert_eq!(receipt.recorded_runs, receipt.planned_runs);
    assert!(!receipt.outcome.is_pass());
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert!(verified.cases.iter().any(|c| matches!(
        &c.verdict,
        CaseVerdict::Invalid {
            reason: InvalidReason::ExecutorError,
            ..
        }
    )));
}

#[tokio::test]
async fn budgets_samples_and_attempts_are_enforced_before_spending() {
    let fx = fixture();
    let exec = Scripted::new(|r| Ok(good(r)));

    // Below the declared minimum sample.
    let small = suite_with(vec![case("only", EvalSplit::Validation, "write release notes only")])
        .freeze()
        .expect("test: freeze");
    let error = fx
        .store
        .evaluate(
            &request(&fx, &small, EvalSplit::Validation),
            &exec,
            &CancellationToken::new(),
        )
        .await
        .expect_err("test: too small");
    assert!(matches!(error, EvaluationError::SplitTooSmall { .. }), "{error}");

    // A split the suite does not contain.
    let suite = validation_suite(2);
    let error = fx
        .store
        .evaluate(
            &request(&fx, &suite, EvalSplit::Holdout),
            &exec,
            &CancellationToken::new(),
        )
        .await
        .expect_err("test: empty split");
    assert!(
        matches!(error, EvaluationError::SplitTooSmall { cases: 0, .. }),
        "{error}"
    );

    // The reserved holdout is evaluated once.
    let both: Vec<EvalCase> = vec![
        case("v-0", EvalSplit::Validation, "validation task zero"),
        case("v-1", EvalSplit::Validation, "validation task one"),
        case("h-0", EvalSplit::Holdout, "holdout task zero"),
        case("h-1", EvalSplit::Holdout, "holdout task one"),
    ];
    let suite = suite_with(both).freeze().expect("test: freeze");
    let first = fx
        .store
        .evaluate(
            &request(&fx, &suite, EvalSplit::Holdout),
            &exec,
            &CancellationToken::new(),
        )
        .await
        .expect("test: first holdout run");
    assert!(first.outcome.is_pass());
    let spent = exec.seen.lock().len();
    let error = fx
        .store
        .evaluate(
            &request(&fx, &suite, EvalSplit::Holdout),
            &exec,
            &CancellationToken::new(),
        )
        .await
        .expect_err("test: holdout is single use");
    assert!(
        matches!(error, EvaluationError::AttemptsExhausted { used: 1, limit: 1, .. }),
        "{error}"
    );
    assert_eq!(exec.seen.lock().len(), spent);

    // An elapsed budget that the runs overshoot cannot pass.
    let mut limited = suite_with(vec![
        case("t-0", EvalSplit::Validation, "timed task zero"),
        case("t-1", EvalSplit::Validation, "timed task one"),
    ]);
    limited.budget.max_total_elapsed_ms = 5;
    let limited = limited.freeze().expect("test: freeze");
    let slow = Scripted::new(|r| {
        let mut o = good(r);
        o.elapsed_ms = 50;
        Ok(o)
    });
    let receipt = fx
        .store
        .evaluate(
            &request(&fx, &limited, EvalSplit::Validation),
            &slow,
            &CancellationToken::new(),
        )
        .await
        .expect("test: evaluate");
    assert!(receipt.budget_exhausted);
    assert!(!receipt.outcome.is_pass());
}

#[tokio::test]
async fn the_executor_is_never_handed_the_oracle() {
    let fx = fixture();
    let mut cases = vec![
        case("a", EvalSplit::Validation, "produce the report for task a"),
        case("b", EvalSplit::Validation, "produce the report for task b"),
    ];
    for c in &mut cases {
        c.validators = vec![
            Validator::ResponseContains {
                text: ORACLE.to_string(),
            },
            Validator::ArtifactContains {
                path: "out/report.txt".to_string(),
                text: "EXPECTED-ARTIFACT-TEXT".to_string(),
            },
        ];
        c.fixtures = vec![FixtureFile {
            path: "input/data.txt".to_string(),
            content: "plain input".to_string(),
        }];
    }
    let suite = suite_with(cases).freeze().expect("test: freeze");
    let exec = Scripted::new(|r| {
        let mut o = good(r);
        o.artifacts
            .insert("out/report.txt".to_string(), Some(b"EXPECTED-ARTIFACT-TEXT".to_vec()));
        Ok(o)
    });
    run(&fx, &suite, &exec).await;
    let seen = exec.seen.lock();
    assert!(!seen.is_empty());
    for request in seen.iter() {
        assert!(!request.contains(ORACLE), "{request}");
        assert!(!request.contains("EXPECTED-ARTIFACT-TEXT"), "{request}");
        assert!(
            request.contains("out/report.txt"),
            "the executor learns which artifact to hand back, not its content"
        );
    }
}

#[tokio::test]
async fn evaluations_are_owner_scoped() {
    let fx = fixture();
    let suite = validation_suite(2);
    let exec = Scripted::new(|r| Ok(good(r)));
    let receipt = run(&fx, &suite, &exec).await;
    let other = SkillLifecycleStore::open(
        fx.store
            .root()
            .parent()
            .and_then(|p| p.parent())
            .expect("test: workspace"),
        owner(OWNER),
    )
    .expect("test: reopen");
    assert!(other.verify_evaluation(&receipt.record_id, None).is_ok());
    let foreign_error = SkillLifecycleStore::open(
        fx.store
            .root()
            .parent()
            .and_then(|p| p.parent())
            .expect("test: workspace"),
        owner("owner:ws:cli:mallory"),
    );
    assert!(foreign_error.is_err(), "the store itself refuses another owner");
}

// ---- review round: verdicts are re-derived, never trusted ------------------

#[tokio::test]
async fn a_flipped_and_resealed_verdict_cannot_turn_an_invalid_run_into_a_pass() {
    let fx = fixture();
    let suite = validation_suite(2);
    // Every candidate run was cancelled and returned nothing: invalid, so incomplete.
    let exec = Scripted::new(|r| {
        let mut o = good(r);
        if is_candidate(r) {
            o.terminal = RunTerminal::Cancelled;
            o.response = String::new();
        }
        Ok(o)
    });
    let receipt = run(&fx, &suite, &exec).await;
    assert!(matches!(receipt.outcome, EvaluationOutcome::Incomplete { .. }));

    // A writer flips every candidate verdict to Pass and the outcome to Improved,
    // re-sealing every digest. The run receipts still say cancelled with no body.
    rewrite_evaluation_for_test(&fx.store, &receipt.record_id, |r, cases| {
        for case in cases.iter_mut().filter(|c| c.arm == Arm::Candidate) {
            case.verdict = CaseVerdict::Pass;
        }
        r.outcome = EvaluationOutcome::Improved;
    });
    let error = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect_err("test: flipped");
    assert!(error.to_string().contains("recomputed"), "{error}");

    // Even with an honest-looking Incomplete outcome the flipped verdicts are named.
    rewrite_evaluation_for_test(&fx.store, &receipt.record_id, |r, _| {
        r.outcome = EvaluationOutcome::Incomplete {
            reason: IncompleteReason::InvalidRuns { count: 2 },
        };
    });
    let error = fx.store.verify_evaluation(&receipt.record_id, None);
    let verified = match error {
        Ok(v) => v,
        Err(e) => {
            assert!(e.to_string().contains("recomputed"), "{e}");
            return;
        }
    };
    assert!(!verified.outcome.is_pass());
    assert!(matches!(
        verified.outcome,
        EvaluationOutcome::Incomplete {
            reason: IncompleteReason::Unbound { .. }
        }
    ));
}

#[tokio::test]
async fn evaluation_level_fields_must_agree_with_the_receipts() {
    type Mutation = fn(&mut EvaluationReceipt, &mut Vec<CaseReceipt>);
    let mutations: [(&str, Mutation); 7] = [
        ("elapsed total", |r, _| r.total_elapsed_ms += 1),
        ("run count", |r, _| r.recorded_runs += 1),
        ("planned runs", |r, _| r.planned_runs += 1),
        ("planned cases", |r, _| r.planned_cases.reverse()),
        ("stale flag", |r, _| r.stale = true),
        ("end generation", |r, _| r.end_generation += 1),
        ("run order", |_, cases| {
            let first = cases[0].order;
            cases[0].order = cases[1].order;
            cases[1].order = first;
        }),
    ];
    for (name, mutate) in mutations {
        let fx = fixture();
        let suite = validation_suite(2);
        let exec = Scripted::new(|r| Ok(good(r)));
        let receipt = run(&fx, &suite, &exec).await;
        assert!(receipt.outcome.is_pass(), "{name}");
        rewrite_evaluation_for_test(&fx.store, &receipt.record_id, |r, cases| mutate(r, cases));
        // The stored outcome still says improved: verification must not agree.
        assert!(fx.store.verify_evaluation(&receipt.record_id, None).is_err(), "{name}");
    }
}

#[tokio::test]
async fn a_terminal_that_is_not_a_success_cannot_pass() {
    for (outcome, passes) in [
        (ExperienceOutcome::Success, true),
        (ExperienceOutcome::Failure, false),
        (ExperienceOutcome::Blocked, false),
        (ExperienceOutcome::Cancelled, false),
        (ExperienceOutcome::Incomplete, false),
    ] {
        let fx = fixture();
        let suite = validation_suite(2);
        let exec = Scripted::new(move |r| {
            let mut o = good(r);
            if is_candidate(r) {
                o.experience_terminal = Some(ExperienceTerminalFact {
                    terminal_receipt_id: "t".to_string(),
                    outcome,
                });
            }
            Ok(o)
        });
        let receipt = run(&fx, &suite, &exec).await;
        assert_eq!(receipt.outcome.is_pass(), passes, "{outcome:?}: {:?}", receipt.outcome);
    }
}

#[tokio::test]
async fn served_bytes_that_are_not_the_arms_bundle_are_unbound() {
    let fx = fixture();
    let suite = validation_suite(2);
    let exec = Scripted::new(|r| {
        let mut o = good(r);
        if is_candidate(r) {
            for h in &mut o.hydrations {
                h.resource_sha256 = "f".repeat(64);
            }
        }
        Ok(o)
    });
    let receipt = run(&fx, &suite, &exec).await;
    assert!(!receipt.outcome.is_pass());
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert!(verified.cases.iter().any(|c| matches!(
        &c.verdict,
        CaseVerdict::Invalid {
            reason: InvalidReason::Unbound,
            ..
        }
    )));
}

#[tokio::test]
async fn usage_is_recorded_and_ceilings_are_enforced() {
    let fx = fixture();
    let suite = validation_suite(2);
    let exec = Scripted::new(|r| Ok(good(r)));
    let receipt = run(&fx, &suite, &exec).await;
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert_eq!(verified.summary.total_tokens, 40);
    assert_eq!(verified.summary.total_cost_micros, 20);
    assert!(
        verified
            .cases
            .iter()
            .all(|c| c.run.tokens == Some(10) && c.run.models_used == ["eval-model"])
    );
    assert_ne!(receipt.seed_control, "");

    // Unmeasured usage is invalid, never free.
    let fx = fixture();
    let exec = Scripted::new(|r| {
        let mut o = good(r);
        o.tokens = None;
        Ok(o)
    });
    let receipt = run(&fx, &suite, &exec).await;
    assert!(!receipt.outcome.is_pass());
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert!(verified.cases.iter().all(|c| matches!(
        &c.verdict,
        CaseVerdict::Invalid {
            reason: InvalidReason::UsageUnmeasured,
            ..
        }
    )));

    // A token ceiling the runs overshoot cannot pass.
    let fx = fixture();
    let mut limited = suite_with(suite.suite().cases.clone());
    limited.budget.max_total_tokens = 15;
    let limited = limited.freeze().expect("test: freeze");
    let exec = Scripted::new(|r| Ok(good(r)));
    let receipt = run(&fx, &limited, &exec).await;
    assert_eq!(receipt.outcome, EvaluationOutcome::BudgetExceeded);
}

#[tokio::test]
async fn an_executor_error_is_charged_the_time_it_took() {
    let fx = fixture();
    let suite = validation_suite(2);
    let exec = Scripted::new(|_| {
        std::thread::sleep(std::time::Duration::from_millis(40));
        Err(EvaluationError::Executor("boom".to_string()))
    });
    let receipt = run(&fx, &suite, &exec).await;
    assert!(
        receipt.total_elapsed_ms >= 40 * u64::from(receipt.recorded_runs),
        "{}",
        receipt.total_elapsed_ms
    );
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert!(verified.cases.iter().all(|c| c.run.elapsed_ms >= 40));
}

#[tokio::test]
async fn an_active_revision_must_be_the_baseline_not_no_skill() {
    let fx = fixture();
    let suite = validation_suite(2);
    let exec = Scripted::new(|r| Ok(good(r)));
    let mut req = request(&fx, &suite, EvalSplit::Validation);
    req.baseline = None;
    let error = fx
        .store
        .evaluate(&req, &exec, &CancellationToken::new())
        .await
        .expect_err("test: no-skill baseline");
    assert!(matches!(error, EvaluationError::Stale(_)), "{error}");
    assert!(exec.seen.lock().is_empty());
}

// ---- attempts ---------------------------------------------------------------

fn holdout_suite(id: &str) -> FrozenSuite {
    let mut s = suite_with(vec![
        case("v-0", EvalSplit::Validation, "validation task zero"),
        case("v-1", EvalSplit::Validation, "validation task one"),
        case("h-0", EvalSplit::Holdout, "holdout task zero"),
        case("h-1", EvalSplit::Holdout, "holdout task one"),
    ]);
    s.suite_id = id.to_string();
    s.freeze().expect("test: freeze")
}

#[tokio::test]
async fn concurrent_evaluations_cannot_both_take_the_last_holdout_attempt() {
    let fx = fixture();
    let suite = holdout_suite("race-suite");
    let exec = Scripted::new(|r| Ok(good(r)));
    let cancel = CancellationToken::new();
    let req = request(&fx, &suite, EvalSplit::Holdout);
    let (a, b) = tokio::join!(
        fx.store.evaluate(&req, &exec, &cancel),
        fx.store.evaluate(&req, &exec, &cancel)
    );
    let ok = [&a, &b].iter().filter(|r| r.is_ok()).count();
    let exhausted = [&a, &b]
        .iter()
        .filter(|r| matches!(r, Err(EvaluationError::AttemptsExhausted { .. })))
        .count();
    assert_eq!((ok, exhausted), (1, 1), "{a:?} {b:?}");
}

#[tokio::test]
async fn a_dropped_evaluation_still_spends_its_attempt_and_the_split_key_ignores_the_suite_id() {
    let fx = fixture();
    let suite = holdout_suite("first-suite");
    let hang = Hanging;
    let cancel = CancellationToken::new();
    let req = request(&fx, &suite, EvalSplit::Holdout);
    // Drop the evaluation future partway: the attempt was reserved before the first run.
    let outcome = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        fx.store.evaluate(&req, &hang, &cancel),
    )
    .await;
    assert!(outcome.is_err(), "the evaluation was dropped mid-run");

    // A renamed suite with the same holdout cases (and other splits changed) is the same split.
    let mut other = suite_with(vec![
        case("v-9", EvalSplit::Validation, "a different validation task"),
        case("v-8", EvalSplit::Validation, "another validation task"),
        case("x-0", EvalSplit::Holdout, "Holdout task   ZERO"),
        case("x-1", EvalSplit::Holdout, "holdout task one"),
    ]);
    other.suite_id = "renamed-suite".to_string();
    let other = other.freeze().expect("test: freeze");
    let exec = Scripted::new(|r| Ok(good(r)));
    let error = fx
        .store
        .evaluate(
            &request(&fx, &other, EvalSplit::Holdout),
            &exec,
            &CancellationToken::new(),
        )
        .await
        .expect_err("test: attempt already spent");
    assert!(matches!(error, EvaluationError::AttemptsExhausted { .. }), "{error}");
}

// ---- suite statistics and hygiene ------------------------------------------

#[test]
fn spec_minimums_and_bounds_are_enforced() {
    let ok = || {
        suite_with(vec![
            case("a", EvalSplit::Validation, "prompt one"),
            case("b", EvalSplit::Validation, "prompt two"),
        ])
    };
    assert!(ok().freeze().is_ok());
    let reject = |mutate: fn(&mut EvalSuite), needle: &str| {
        let mut s = ok();
        mutate(&mut s);
        let error = s.freeze().expect_err("test: must be rejected").to_string();
        assert!(error.contains(needle), "{error}");
    };
    reject(|s| s.min_cases = 1, "min_cases");
    reject(|s| s.deterministic = false, "stochastic");
    reject(|s| s.min_gain_cases = 0, "min_gain_cases");
    reject(|s| s.allowed_regressions = 100, "allowed_regressions");
    reject(|s| s.holdout_attempts = 0, "holdout_attempts");
    reject(|s| s.validation_attempts = 1000, "validation_attempts");
    reject(|s| s.budget.max_total_tokens = 0, "budget");
    reject(|s| s.budget.max_total_cost_micros = 0, "budget");
    reject(|s| s.required_tools = vec!["skill_read".to_string()], "always offered");
    reject(
        |s| {
            s.cases[0].fixtures = vec![FixtureFile {
                path: "skills/evil/SKILL.md".to_string(),
                content: "x".to_string(),
            }];
        },
        "reserved",
    );
    reject(
        |s| {
            s.cases[0].fixtures = vec![FixtureFile {
                path: "Skills/evil/SKILL.md".to_string(),
                content: "x".to_string(),
            }];
        },
        "reserved",
    );
    reject(
        |s| {
            s.cases[0].fixtures = vec![FixtureFile {
                path: "config.toml".to_string(),
                content: "x".to_string(),
            }];
        },
        "reserved",
    );
}

#[test]
fn a_full_evaluation_history_is_a_typed_error_with_guidance() {
    let fx = fixture();
    let dir = fx.store.root().join("evaluations");
    fs::create_dir_all(&dir).expect("test: dir");
    for i in 0..4097_u32 {
        fs::create_dir(dir.join(format!("eval-{i:032x}"))).expect("test: entry");
    }
    let error = fx.store.list_evaluation_ids().expect_err("test: full");
    assert!(matches!(error, EvaluationError::HistoryFull { .. }), "{error}");
    assert!(error.to_string().contains("archive or remove"), "{error}");
}

#[test]
fn only_declared_read_tools_and_declared_writes_can_be_required() {
    let ok = |tools: &[&str], effects: SideEffects| {
        let mut s = suite_with(vec![
            case("a", EvalSplit::Validation, "prompt one"),
            case("b", EvalSplit::Validation, "prompt two"),
        ]);
        s.required_tools = tools.iter().map(|t| (*t).to_string()).collect();
        s.side_effects = effects;
        s.freeze()
    };
    assert!(ok(&["file_read", "web_search_tool"], SideEffects::None).is_ok());
    assert!(ok(&["file_write", "file_edit"], SideEffects::WorkspaceWrite).is_ok());
    for tool in [
        "shell",
        "skill_execute",
        "skills_manage",
        "http_request",
        "mcp_call",
        "cron",
        "delegate",
        "sessions_spawn",
        "xin",
        "pushover",
        "proxy_config",
        "document_ingest",
        "composio",
        "git_operations",
    ] {
        assert!(ok(&[tool], SideEffects::WorkspaceWrite).is_err(), "{tool}");
    }
    assert!(
        ok(&["file_write"], SideEffects::None).is_err(),
        "writes need the declared side effect"
    );
}

#[tokio::test]
async fn the_pinned_digest_catches_a_forged_substring_validator() {
    let fx = fixture();
    let suite = validation_suite(2);
    // The candidate gives the wrong answer: honestly NotImproved.
    let exec = Scripted::new(|r| {
        let mut o = good(r);
        o.response = "notes: nothing useful".to_string();
        Ok(o)
    });
    let recorded = fx
        .store
        .evaluate(
            &request(&fx, &suite, EvalSplit::Validation),
            &exec,
            &CancellationToken::new(),
        )
        .await
        .expect("test: evaluate");
    assert_eq!(recorded.receipt.outcome, EvaluationOutcome::NotImproved);
    assert!(
        fx.store
            .verify_evaluation(&recorded.receipt.record_id, Some(&recorded.receipt_digest))
            .is_ok()
    );

    // A writer marks the substring validators as passed, flips the verdicts and re-seals.
    rewrite_evaluation_for_test(&fx.store, &recorded.receipt.record_id, |r, cases| {
        for case in cases.iter_mut().filter(|c| c.arm == Arm::Candidate) {
            for v in &mut case.validators {
                v.passed = true;
            }
            case.verdict = CaseVerdict::Pass;
        }
        r.outcome = EvaluationOutcome::Improved;
    });
    // Substring validators cannot be rechecked from digests, so the forgery is self-consistent ...
    assert!(fx.store.verify_evaluation(&recorded.receipt.record_id, None).is_ok());
    // ... but it cannot match the digest pinned when the evaluation was created.
    let error = fx
        .store
        .verify_evaluation(&recorded.receipt.record_id, Some(&recorded.receipt_digest))
        .expect_err("test: pinned digest");
    assert!(error.to_string().contains("pinned"), "{error}");
}

#[tokio::test]
async fn unmeasured_runs_are_charged_their_share_of_the_ceiling() {
    let fx = fixture();
    let mut s = suite_with(vec![
        case("a", EvalSplit::Validation, "prompt one"),
        case("b", EvalSplit::Validation, "prompt two"),
    ]);
    s.budget.max_runs = 4;
    s.budget.max_total_tokens = 40;
    let suite = s.freeze().expect("test: freeze");
    let exec = Scripted::new(|r| {
        let mut o = good(r);
        o.tokens = None;
        o.cost_micros = None;
        Ok(o)
    });
    let receipt = run(&fx, &suite, &exec).await;
    assert!(!receipt.outcome.is_pass());
    let verified = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect("test: verify");
    assert_eq!(
        verified.summary.total_tokens, 40,
        "4 unmeasured runs at 10 tokens each, not 0"
    );
}

#[tokio::test]
async fn a_missing_environment_binding_is_a_defect() {
    let fx = fixture();
    let suite = validation_suite(2);
    let exec = Scripted::new(|r| Ok(good(r)));
    let receipt = run(&fx, &suite, &exec).await;
    assert!(receipt.outcome.is_pass());
    rewrite_evaluation_for_test(&fx.store, &receipt.record_id, |r, _| r.environment_hash = None);
    let error = fx
        .store
        .verify_evaluation(&receipt.record_id, None)
        .expect_err("test: env removed");
    assert!(error.to_string().contains("recomputed"), "{error}");
}

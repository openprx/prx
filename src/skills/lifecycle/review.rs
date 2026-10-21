//! Source review of a staged candidate (spec 8.1, SL07 hard gate).
//!
//! `source_reviewed` is not a recorded opinion: it is the result of a
//! deterministic review that checks applicability, bounded changes and content
//! safety against the real baseline and candidate bundles, and leaves a sealed
//! record next to the proposal (`proposals/<id>/source_review.json`). A proposal
//! reaches `SourceReviewed` only through [`SkillLifecycleStore::review_source`]
//! with a passing record, and the evaluation start, the gate and the
//! authorization all require that record again. The review does not substitute
//! for task evaluation.

use super::bundle::Bundle;
use super::manifest::validate_manifest;
use super::proposal::{ProposalFailure, ProposalFailureKind, ProposalStatus, SkillProposal, bounded, is_proposal_id};
use super::propose::{MAX_EDIT_TOKENS, MAX_EDITS, Mode, reapply};
use super::propose_new::{skeleton_bundle, skeleton_text};
use super::rejection::ProposalOperation;
use super::store::{SkillLifecycleStore, check_private, read_record, record_bytes};
use super::types::{LifecycleError, OwnerScope, RevisionId, SCHEMA_VERSION, SkillId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;

const SOURCE_REVIEW_FILE: &str = "source_review.json";
const SEAL_DOMAIN: &[u8] = b"prx-skill-source-review\0";
const SKILL_MD: &str = "SKILL.md";
const MAX_REVIEWER_BYTES: usize = 128;
/// Identity of the deterministic checks; part of every record.
pub const SOURCE_REVIEW_IDENTITY: &str = "prx-skill-source-review/1";

/// Phrases a candidate may not introduce (they are compared with the
/// baseline, so a skill that already talks about them is not blamed).
const FORBIDDEN_PHRASES: &[&str] = &[
    "ignore previous instructions",
    "ignore all previous",
    "ignore the above",
    "disregard the system prompt",
    "disregard previous",
    "bypass approval",
    "bypass the approval",
    "skip the approval",
    "disable the safety",
    "disable safety",
    "without asking for approval",
    "begin private key",
    "authorization: bearer",
    "rm -rf",
    "| sh",
    "| bash",
    "sudo ",
];

/// What one finding of the review is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewCheck {
    Applicability,
    BoundedChanges,
    ContentSafety,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFinding {
    pub check: ReviewCheck,
    pub passed: bool,
    pub detail: String,
}

/// The sealed result of a source review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceReviewRecord {
    pub schema_version: u32,
    pub record_id: String,
    pub owner_scope: OwnerScope,
    pub proposal_id: String,
    pub skill_id: SkillId,
    /// `None` for a new skill.
    pub baseline_revision: Option<RevisionId>,
    pub candidate_revision: RevisionId,
    /// Who asked for the review (an operator or the service); the verdict
    /// itself comes from [`SOURCE_REVIEW_IDENTITY`].
    pub reviewer: String,
    pub review_identity: String,
    pub findings: Vec<ReviewFinding>,
    /// All findings passed.
    pub passed: bool,
    /// The review failed because the active revision was no longer the
    /// proposal's baseline; the proposal then goes `Stale`, not `Failed`.
    #[serde(default)]
    pub skill_moved: bool,
    pub reviewed_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedReview {
    review: SourceReviewRecord,
    digest: String,
}

fn seal_digest(review: &SourceReviewRecord) -> Result<String, LifecycleError> {
    let bytes = serde_json::to_vec(review).map_err(|e| LifecycleError::InvalidRecord {
        path: SOURCE_REVIEW_FILE.into(),
        reason: e.to_string(),
    })?;
    let mut hasher = Sha256::new();
    hasher.update(SEAL_DOMAIN);
    hasher.update(SCHEMA_VERSION.to_be_bytes());
    hasher.update(bytes);
    Ok(hex::encode(hasher.finalize()))
}

/// What a review compares the candidate with.
struct Base {
    bundle: Bundle,
    description: String,
}

/// What a review did.
#[derive(Clone, Debug)]
pub struct SourceReviewOutcome {
    pub record: SourceReviewRecord,
    /// `SourceReviewed` when the review passed, `Stale` when the skill moved,
    /// `Failed` for any other refusal.
    pub proposal: SkillProposal,
}

fn finding(check: ReviewCheck, problems: Vec<String>, ok: &str) -> ReviewFinding {
    if problems.is_empty() {
        ReviewFinding {
            check,
            passed: true,
            detail: ok.to_string(),
        }
    } else {
        ReviewFinding {
            check,
            passed: false,
            detail: bounded(&problems.join("; ")),
        }
    }
}

const fn is_hidden(c: char) -> bool {
    (c.is_control() && !matches!(c, '\n' | '\t' | '\r'))
        || matches!(
            c,
            '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

fn normalised(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// Content safety of the candidate against its baseline: no hidden character
/// kind and no phrase that overrides instructions or approvals is introduced.
///
/// This is a first-line heuristic only. A fixed denylist cannot recognise
/// every way to phrase a harmful instruction; it catches the blunt cases and
/// hidden text. What actually protects a published skill is the rest of the
/// chain: the bounded edit budget, the unchanged non-instruction files, the
/// capability re-check, the measured evaluation and the gate.
fn safety_problems(baseline: &Bundle, candidate: &Bundle) -> Vec<String> {
    let mut problems = Vec::new();
    for file in candidate.files() {
        let Ok(text) = std::str::from_utf8(file.bytes()) else {
            continue;
        };
        let before = baseline
            .file(file.path())
            .and_then(|f| std::str::from_utf8(f.bytes()).ok())
            .unwrap_or_default();
        let hidden = |text: &str| -> BTreeSet<char> { text.chars().filter(|c| is_hidden(*c)).collect() };
        let known = hidden(before);
        if hidden(text).iter().any(|c| !known.contains(c)) {
            problems.push(format!("{} gains hidden or control characters", bounded(file.path())));
        }
        if file.path() != SKILL_MD {
            continue;
        }
        let (after, before) = (normalised(text), normalised(before));
        for phrase in FORBIDDEN_PHRASES {
            if occurrences(&after, phrase) > occurrences(&before, phrase) {
                problems.push(format!("SKILL.md introduces the text {phrase:?}"));
            }
        }
    }
    problems
}

/// Changes of the candidate against the baseline: only `SKILL.md` differs,
/// executable bits are unchanged, and the recorded diff is within its budget
/// and reproduces the candidate exactly.
fn bounded_problems(
    proposal: &SkillProposal,
    mode: Mode,
    baseline: &Bundle,
    baseline_description: &str,
    candidate: &Bundle,
) -> Vec<String> {
    let mut problems = Vec::new();
    let paths = |bundle: &Bundle| -> Vec<String> { bundle.files().iter().map(|f| f.path().to_string()).collect() };
    if paths(baseline) != paths(candidate) {
        problems.push("the candidate adds or removes files".to_string());
    }
    for file in candidate.files() {
        if let Some(old) = baseline.file(file.path()) {
            if old.is_executable() != file.is_executable() {
                problems.push(format!("{} changes its executable bit", bounded(file.path())));
            }
            if file.path() != SKILL_MD && old.bytes() != file.bytes() {
                problems.push(format!("{} changes; only {SKILL_MD} may", bounded(file.path())));
            }
        }
    }
    let Some(diff) = proposal.diff.as_ref() else {
        problems.push("the proposal records no diff".to_string());
        return problems;
    };
    if diff.edits.len() > MAX_EDITS {
        problems.push(format!("more than {MAX_EDITS} edits"));
    }
    let skill_md = |bundle: &Bundle| -> Option<String> {
        bundle
            .file(SKILL_MD)
            .and_then(|f| std::str::from_utf8(f.bytes()).ok())
            .map(str::to_string)
    };
    match (skill_md(baseline), skill_md(candidate)) {
        (Some(original), Some(staged)) => match reapply(mode, &original, baseline_description, diff) {
            Ok(rebuilt) => {
                if rebuilt.skill_md != staged {
                    problems.push("the recorded diff does not reproduce the staged candidate".to_string());
                }
                // The budget is recomputed from the texts, never read from the
                // record; the record must agree with the recomputation.
                if (diff.changed_tokens, diff.baseline_tokens, diff.allowed_tokens)
                    != (rebuilt.changed_tokens, rebuilt.baseline_tokens, rebuilt.allowed_tokens)
                {
                    problems.push("the recorded edit figures disagree with the recomputed ones".to_string());
                }
                if rebuilt.allowed_tokens > MAX_EDIT_TOKENS
                    || rebuilt.changed_tokens > rebuilt.allowed_tokens
                    || rebuilt.changed_bytes > rebuilt.allowed_bytes
                {
                    problems.push(format!(
                        "the edit changes {} tokens ({} bytes); at most {} tokens ({} bytes) are allowed",
                        rebuilt.changed_tokens, rebuilt.changed_bytes, rebuilt.allowed_tokens, rebuilt.allowed_bytes
                    ));
                }
            }
            Err(e) => problems.push(format!("the recorded diff does not apply: {e}")),
        },
        _ => problems.push("baseline or candidate has no readable SKILL.md".to_string()),
    }
    problems
}

impl SkillLifecycleStore {
    fn source_review_path(&self, proposal_id: &str) -> std::path::PathBuf {
        self.proposal_file(proposal_id, SOURCE_REVIEW_FILE)
    }

    /// Reads the sealed source review of a proposal; `None` when none was
    /// recorded. The seal, owner scope and identity are verified.
    pub fn read_source_review(&self, proposal_id: &str) -> Result<Option<SourceReviewRecord>, LifecycleError> {
        if !is_proposal_id(proposal_id) {
            return Err(LifecycleError::InvalidId {
                kind: "proposal id",
                reason: "must be prop- followed by 32 lowercase hex digits".to_string(),
            });
        }
        let path = self.source_review_path(proposal_id);
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(LifecycleError::io(&path, e)),
        }
        check_private(&path)?;
        let sealed: SealedReview = read_record(&path)?;
        let tampered = |reason: &str| LifecycleError::Tampered {
            path: path.clone(),
            reason: reason.to_string(),
        };
        if sealed.review.schema_version != SCHEMA_VERSION {
            return Err(LifecycleError::UnsupportedSchema {
                what: "source review",
                found: sealed.review.schema_version,
            });
        }
        if seal_digest(&sealed.review)? != sealed.digest {
            return Err(tampered("the source review does not match its digest"));
        }
        self.check_owner_scope(&sealed.review.owner_scope)?;
        if sealed.review.proposal_id != proposal_id || sealed.review.record_id != proposal_id {
            return Err(tampered("the source review belongs to another proposal"));
        }
        Ok(Some(sealed.review))
    }

    /// The passing source review of `proposal`, bound to its skill, baseline
    /// and candidate. Anything else is [`LifecycleError::SourceReviewRequired`].
    pub(super) fn require_source_review(&self, proposal: &SkillProposal) -> Result<SourceReviewRecord, LifecycleError> {
        let required = |reason: &str| LifecycleError::SourceReviewRequired {
            proposal_id: proposal.proposal_id.clone(),
            reason: reason.to_string(),
        };
        let review = self
            .read_source_review(&proposal.proposal_id)?
            .ok_or_else(|| required("no source review was recorded"))?;
        if !review.passed || review.findings.iter().any(|f| !f.passed) {
            return Err(required("the recorded source review did not pass"));
        }
        if review.review_identity != SOURCE_REVIEW_IDENTITY {
            return Err(required("the review was made by an unknown reviewer identity"));
        }
        if Some(&review.candidate_revision) != proposal.candidate_revision.as_ref()
            || review.baseline_revision != proposal.baseline_revision
            || review.skill_id != proposal.skill_id
        {
            return Err(required("the review belongs to another candidate or baseline"));
        }
        let checks = [
            ReviewCheck::Applicability,
            ReviewCheck::BoundedChanges,
            ReviewCheck::ContentSafety,
        ];
        if !checks.iter().all(|c| review.findings.iter().any(|f| f.check == *c)) {
            return Err(required("the review did not cover every check"));
        }
        Ok(review)
    }

    /// Reviews the staged candidate of a `Draft` proposal and records the
    /// result. A passing review moves the proposal to `SourceReviewed`; a review
    /// that finds the skill moved makes it `Stale`; any other failing review
    /// ends it `Failed`. Calling it again on a proposal that was reviewed
    /// returns the recorded result.
    ///
    /// Blocking: takes the proposal lock and reads the store.
    pub(in crate::skills) fn review_source(
        &self,
        proposal_id: &str,
        reviewer: &str,
    ) -> Result<SourceReviewOutcome, LifecycleError> {
        if reviewer.trim().is_empty() || reviewer.len() > MAX_REVIEWER_BYTES || reviewer.chars().any(char::is_control) {
            return Err(LifecycleError::ProposalRefused(
                "a reviewer name of 1..=128 bytes without control characters is required".to_string(),
            ));
        }
        let proposal = self
            .read_proposal(proposal_id)?
            .ok_or_else(|| LifecycleError::ProposalRefused(format!("no such proposal {proposal_id}")))?;
        let existing = self.read_source_review(proposal_id)?;
        let record = match existing {
            Some(record) => record,
            None => self.compute_review(&proposal, reviewer)?,
        };
        // Also for a record found after a crash: a review that found the skill
        // moved leaves the proposal stale, not failed.
        let moved = record.skill_moved && !record.passed;

        let lock = self.lock_proposals()?;
        let mut current = self
            .read_proposal_record(proposal_id)?
            .ok_or_else(|| LifecycleError::ProposalRefused(format!("no such proposal {proposal_id}")))?;
        if current.status != ProposalStatus::Draft {
            // Already reviewed (a retry, or a crash after the record was written).
            if self.read_source_review(proposal_id)?.is_some() {
                return Ok(SourceReviewOutcome {
                    record,
                    proposal: current,
                });
            }
            return Err(LifecycleError::InvalidProposalTransition {
                proposal_id: current.proposal_id,
                from: current.status.as_str(),
                to: ProposalStatus::SourceReviewed.as_str(),
            });
        }
        if self.read_source_review(proposal_id)?.is_none() {
            let sealed = SealedReview {
                digest: seal_digest(&record)?,
                review: record.clone(),
            };
            let path = self.source_review_path(proposal_id);
            self.write_atomic(&lock.lock_id, &path, &record_bytes(&sealed, &path)?)?;
        }
        let now = Utc::now();
        if record.passed {
            current.status = ProposalStatus::SourceReviewed;
            current.status_reason = Some(bounded(&format!("source review passed (requested by {reviewer})")));
        } else {
            let detail = record
                .findings
                .iter()
                .filter(|f| !f.passed)
                .map(|f| f.detail.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            if moved {
                current.status = ProposalStatus::Stale;
                current.status_reason = Some(bounded(&detail));
            } else {
                current.status = ProposalStatus::Failed;
                current.status_reason = Some(bounded("source review refused the candidate"));
                current.failure = Some(ProposalFailure {
                    kind: ProposalFailureKind::SourceReviewRefused,
                    detail: bounded(&detail),
                });
            }
        }
        current.updated_at = now;
        self.write_proposal(&lock, &current)?;
        Ok(SourceReviewOutcome {
            record,
            proposal: current,
        })
    }

    /// Runs the three checks; `skill_moved` on the record says the skill moved
    /// since the proposal.
    fn compute_review(&self, proposal: &SkillProposal, reviewer: &str) -> Result<SourceReviewRecord, LifecycleError> {
        let candidate_id = proposal.candidate_revision.clone().ok_or_else(|| {
            LifecycleError::ProposalRefused("the proposal has no staged candidate to review".to_string())
        })?;
        let candidate = self.read_candidate_bundle(&proposal.proposal_id)?;
        if candidate.revision_id() != &candidate_id {
            return Err(LifecycleError::Tampered {
                path: self.source_review_path(&proposal.proposal_id),
                reason: "the staged candidate does not hash to the proposal's candidate revision".to_string(),
            });
        }
        let mut applicability = Vec::new();
        let mut moved = false;
        let mode = Mode::of(proposal.operation);
        if proposal.operation == ProposalOperation::Disable {
            applicability.push("only revisions and new skills are reviewed".to_string());
        }
        if proposal.baseline_revision.as_ref() == Some(&candidate_id) {
            applicability.push("the candidate equals its baseline".to_string());
        }
        // What the candidate is compared with: the active revision, or for a
        // new skill the skeleton its frontmatter fixes (instruction-only: the
        // skeleton is `SKILL.md` alone, so any other file is an addition).
        let baseline: Option<Base> = match &proposal.baseline_revision {
            Some(revision) => match self.read_revision(&proposal.skill_id, revision, None) {
                Ok(verified) => Some(Base {
                    description: verified.revision.description.clone(),
                    bundle: verified.bundle,
                }),
                Err(e) => {
                    applicability.push(format!("the baseline revision cannot be read: {e}"));
                    None
                }
            },
            None => match validate_manifest(&candidate) {
                Ok(info) => {
                    match skeleton_bundle(&skeleton_text(&info.name, &info.description, &info.required_tools)) {
                        Ok(bundle) => Some(Base {
                            bundle,
                            description: info.description,
                        }),
                        Err(e) => {
                            applicability.push(format!("the skeleton cannot be rebuilt: {e}"));
                            None
                        }
                    }
                }
                Err(e) => {
                    applicability.push(format!("the candidate manifest is invalid: {e}"));
                    None
                }
            },
        };
        let binding = self.active_binding(&proposal.skill_id)?;
        let current = match (&proposal.baseline_revision, binding) {
            (Some(revision), Some(binding)) => {
                binding.revision_id == *revision && binding.generation == proposal.expected_generation
            }
            (None, None) => proposal.expected_generation == 0,
            _ => false,
        };
        if !current {
            moved = true;
            applicability.push(match mode {
                Mode::Revise => {
                    "the active revision is no longer the baseline the proposal was made against".to_string()
                }
                Mode::New => "the skill has been published since the proposal was made".to_string(),
            });
        }
        let mut findings = Vec::new();
        let missing = || vec!["no baseline to compare the candidate with".to_string()];
        let (bounded_list, safety_list) = baseline.as_ref().map_or_else(
            || (missing(), missing()),
            |base| {
                if mode == Mode::Revise {
                    match (validate_manifest(&base.bundle), validate_manifest(&candidate)) {
                        (Ok(before), Ok(after)) => {
                            if (before.name.as_str(), &before.semantic_version)
                                != (after.name.as_str(), &after.semantic_version)
                            {
                                applicability.push("the candidate changes the skill name or version".to_string());
                            }
                            if before.required_tools != after.required_tools {
                                applicability.push("the candidate changes the required tools".to_string());
                            }
                        }
                        (_, Err(e)) => applicability.push(format!("the candidate manifest is invalid: {e}")),
                        (Err(e), _) => applicability.push(format!("the baseline manifest is invalid: {e}")),
                    }
                }
                (
                    bounded_problems(proposal, mode, &base.bundle, &base.description, &candidate),
                    safety_problems(&base.bundle, &candidate),
                )
            },
        );
        findings.push(finding(
            ReviewCheck::Applicability,
            applicability,
            match mode {
                Mode::Revise => "the candidate revises the active revision of its skill",
                Mode::New => "the candidate is a new instruction skill on its fixed skeleton",
            },
        ));
        findings.push(finding(
            ReviewCheck::BoundedChanges,
            bounded_list,
            "only SKILL.md changed, within the edit budget, and the diff reproduces the candidate",
        ));
        findings.push(finding(
            ReviewCheck::ContentSafety,
            safety_list,
            "no hidden characters and no instruction-override text introduced",
        ));
        let passed = findings.iter().all(|f| f.passed);
        Ok(SourceReviewRecord {
            schema_version: SCHEMA_VERSION,
            record_id: proposal.proposal_id.clone(),
            owner_scope: self.owner_scope().clone(),
            proposal_id: proposal.proposal_id.clone(),
            skill_id: proposal.skill_id.clone(),
            baseline_revision: proposal.baseline_revision.clone(),
            candidate_revision: candidate_id,
            reviewer: reviewer.to_string(),
            review_identity: SOURCE_REVIEW_IDENTITY.to_string(),
            findings,
            passed,
            skill_moved: moved,
            reviewed_at: Utc::now(),
        })
    }
}

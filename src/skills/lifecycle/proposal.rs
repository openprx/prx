//! Proposal records and candidate staging (SL05).
//!
//! Layout inside the owner store:
//!
//! ```text
//! proposals/<proposal-id>/proposal.json      state machine + accounting record
//! proposals/<proposal-id>/candidate/...      the staged candidate bundle
//! ```
//!
//! A candidate is only ever staged here. Nothing in this module (or in the
//! proposer) writes `revisions/`, `active/` or any compatibility projection:
//! a staged candidate becomes a revision only through the publication path,
//! which needs a verdict that does not exist yet. `proposal.json` is the one
//! mutable record; every update replaces it atomically under the proposal lock.

use super::bundle::{Bundle, BundleEntry};
use super::consolidate::{check_ident, reserved_lock};
use super::generation::{SkillLock, StoreLock, ensure_dir_synced};
use super::knowledge::{MAX_PATTERN_EVIDENCE, PatternId};
use super::proposer_model::{ModelBinding, ReportedUsage};
use super::rejection::ProposalOperation;
use super::store::{
    SkillLifecycleStore, check_private, ensure_real_dir, read_record, record_bytes, sync_dir, write_private_file,
};
use super::types::{LifecycleError, OwnerScope, RevisionId, SCHEMA_VERSION, SkillId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(super) const PROPOSALS_DIR: &str = "proposals";
const PROPOSAL_FILE: &str = "proposal.json";
const CANDIDATE_DIR: &str = "candidate";
const PROPOSAL_LOCK_ID: &str = "_skill-proposals";
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
/// Proposal directories one listing will read; past this a typed error.
pub const MAX_PROPOSAL_LISTING: usize = 1024;
/// Model attempts one proposal record keeps.
pub const MAX_PROPOSAL_ATTEMPTS: usize = 8;
const MAX_REQUEST_ID_BYTES: usize = 128;
const MAX_DETAIL_BYTES: usize = 500;
/// Time past an attempt's own call deadline before it counts as lost. It must
/// cover the two lock waits (5 s each) and the blocking-pool hop between the
/// record write and the call start, with room to spare.
pub(super) const INTERRUPTED_GRACE: Duration = Duration::from_secs(60);

/// Lifecycle state of a proposal (spec section 8.1). SL05 creates proposals in
/// `Draft` and only ever moves them to `Cancelled`, `Stale` or `Failed`; the
/// other states belong to the review, evaluation, gate and publication
/// packages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Draft,
    SourceReviewed,
    Evaluating,
    Evaluated,
    Eligible,
    Rejected,
    Inconclusive,
    PublicationAuthorized,
    Committed,
    Monitored,
    Retained,
    Disabled,
    RolledBack,
    Cancelled,
    Stale,
    Failed,
}

impl ProposalStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::SourceReviewed => "source_reviewed",
            Self::Evaluating => "evaluating",
            Self::Evaluated => "evaluated",
            Self::Eligible => "eligible",
            Self::Rejected => "rejected",
            Self::Inconclusive => "inconclusive",
            Self::PublicationAuthorized => "publication_authorized",
            Self::Committed => "committed",
            Self::Monitored => "monitored",
            Self::Retained => "retained",
            Self::Disabled => "disabled",
            Self::RolledBack => "rolled_back",
            Self::Cancelled => "cancelled",
            Self::Stale => "stale",
            Self::Failed => "failed",
        }
    }

    /// States before the publication commit: the ones that can still be
    /// cancelled, found stale or failed.
    #[must_use]
    pub const fn is_pre_commit(self) -> bool {
        matches!(
            self,
            Self::Draft
                | Self::SourceReviewed
                | Self::Evaluating
                | Self::Evaluated
                | Self::Eligible
                | Self::Rejected
                | Self::Inconclusive
                | Self::PublicationAuthorized
        )
    }

    /// States of a proposal that can still go on to publication: pre-commit
    /// and not a dead end (`Rejected`, `Cancelled`, `Stale`, `Failed`).
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(
            self,
            Self::Draft
                | Self::SourceReviewed
                | Self::Evaluating
                | Self::Evaluated
                | Self::Eligible
                | Self::Inconclusive
                | Self::PublicationAuthorized
        )
    }

    /// Whether the state machine allows moving from `self` to `next`.
    #[must_use]
    pub const fn can_become(self, next: Self) -> bool {
        if self.is_pre_commit() && matches!(next, Self::Cancelled | Self::Stale | Self::Failed) {
            return true;
        }
        matches!(
            (self, next),
            (Self::Draft, Self::SourceReviewed)
                | (Self::SourceReviewed | Self::Inconclusive, Self::Evaluating)
                | (Self::Evaluating, Self::Evaluated)
                | (Self::Evaluated, Self::Eligible | Self::Rejected | Self::Inconclusive)
                | (Self::Eligible, Self::PublicationAuthorized)
                | (Self::PublicationAuthorized, Self::Committed)
                | (Self::Committed, Self::Monitored)
                | (Self::Monitored, Self::Retained | Self::Disabled | Self::RolledBack)
        )
    }
}

/// How one model attempt ended. Every attempt is recorded, including the ones
/// that produced nothing: a denied, failed or cancelled call is still a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// Started, not finished (also what a crash leaves behind).
    InFlight,
    /// The model replied; whether the reply was usable is the proposal's failure.
    Completed,
    /// No budget reservation could be issued; nothing was sent.
    Denied,
    Failed,
    TimedOut,
    Cancelled,
    /// An in-flight attempt that an operator cancelled; its liability is unknown.
    Abandoned,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptRecord {
    pub number: u32,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub outcome: AttemptOutcome,
    /// Usage the provider reported; `None` when no call was made or finished.
    pub usage: Option<ReportedUsage>,
    /// SHA-256 over the system and user prompt actually sent.
    pub prompt_sha256: String,
    pub detail: Option<String>,
}

/// Why a proposal ended `Failed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalFailureKind {
    BudgetDenied,
    ProviderFailed,
    TimedOut,
    OutputTooLarge,
    MalformedOutput,
    OutOfScope,
    PatchNotApplicable,
    EditBudgetExceeded,
    InvalidCandidate,
    NoChange,
    SuppressedByRejection,
    /// Every publication attempt of an authorized proposal aborted.
    PublicationAborted,
    /// The deterministic source review found the candidate not applicable, not
    /// bounded or not safe.
    SourceReviewRefused,
    /// The candidate asks for capabilities its baseline did not have.
    CapabilityExpansion,
    /// The attempt was in flight when the process died or an operator gave up
    /// on it; the model may have been called, so the liability is unknown.
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalFailure {
    pub kind: ProposalFailureKind,
    pub detail: String,
}

/// The frozen identity of the author of a proposal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorBinding {
    pub model: ModelBinding,
    pub system_prompt_version: String,
    pub system_prompt_sha256: String,
}

/// The stored facts a proposal rests on, read from the experience records at
/// proposal time (not from the pattern's own claims).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalBasis {
    /// Distinct root tasks whose stored experience is learning-eligible, ended
    /// in a failure terminal and hydrated the baseline revision.
    pub failed_tasks_on_baseline: usize,
    /// Distinct root tasks that contradict the patterns on the same revision.
    pub contradicting_tasks: usize,
    /// Always `false` today: the experience record carries no adherence fact,
    /// so "the guidance was loaded but not followed" cannot be excluded from
    /// stored facts alone. The failure class stays asserted on that axis.
    pub adherence_verified: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditOp {
    Replace,
    Append,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedEdit {
    pub op: EditOp,
    pub old: Option<String>,
    pub new: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescriptionChange {
    pub old: String,
    pub new: String,
}

/// The bounded patch the candidate is, relative to its baseline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundedDiff {
    pub edits: Vec<AppliedEdit>,
    pub description_change: Option<DescriptionChange>,
    /// Changed instruction tokens (whitespace-separated words).
    pub changed_tokens: u64,
    /// Instruction tokens of the baseline `SKILL.md`.
    pub baseline_tokens: u64,
    /// The ceiling that applied: `min(300, 20% of baseline_tokens)`.
    pub allowed_tokens: u64,
    /// The model's one-sentence summary, bounded.
    pub summary: Option<String>,
}

/// Budgets the proposal ran under, recorded for audit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetRecord {
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
    pub call_timeout_ms: u64,
}

/// What a proposal was authorized for publication against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationAuthorization {
    pub gate_id: String,
    /// Digest of the sealed gate decision the authorization was verified against.
    pub gate_digest: String,
    /// Pinned record digest of the candidate revision to publish.
    pub candidate_record_digest: String,
    pub authorized_at: DateTime<Utc>,
    /// Publication attempt; the transaction id derives from it. An attempt
    /// whose transaction aborted is never re-run, the next one gets a new id.
    pub attempt: u32,
}

/// The committed publication of a proposal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationRecord {
    pub transaction_id: String,
    pub revision_id: RevisionId,
    pub generation: u64,
    pub committed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillProposal {
    pub schema_version: u32,
    /// Stable record id; equals the proposal id.
    pub record_id: String,
    pub owner_scope: OwnerScope,
    pub proposal_id: String,
    /// Idempotency key of the request; the proposal id derives from it.
    pub request_id: String,
    pub operation: ProposalOperation,
    pub skill_id: SkillId,
    /// The pattern ids the caller asked for, sorted. The idempotency check
    /// compares these, not `pattern_ids` (which are the refreshed heads).
    pub requested_patterns: Vec<PatternId>,
    /// Active generation the proposal was prepared against; publication must
    /// recheck it.
    pub expected_generation: u64,
    pub baseline_revision: RevisionId,
    /// Identity of the staged candidate bundle; `None` until one is staged.
    pub candidate_revision: Option<RevisionId>,
    pub pattern_ids: Vec<PatternId>,
    pub evidence_ids: Vec<String>,
    pub basis: ProposalBasis,
    pub author: AuthorBinding,
    /// Model, prompt version and baseline revision digest; the key a rejection
    /// suppression compares.
    pub environment_binding: String,
    pub budget: BudgetRecord,
    pub status: ProposalStatus,
    pub status_reason: Option<String>,
    pub failure: Option<ProposalFailure>,
    pub diff: Option<BoundedDiff>,
    pub attempts: Vec<AttemptRecord>,
    /// Digest of the suite that governs this proposal's evaluation, bound by
    /// `begin_evaluation` (SL07); the gate never takes it from its caller.
    #[serde(default)]
    pub required_suite: Option<String>,
    /// Evaluation epochs opened so far; each one is a recorded attempt.
    #[serde(default)]
    pub evaluation_epoch: u32,
    /// When the proposal first entered evaluation; evaluations that began
    /// earlier do not count for it.
    #[serde(default)]
    pub evaluating_since: Option<DateTime<Utc>>,
    /// Set when the proposal became `PublicationAuthorized` (SL08).
    #[serde(default)]
    pub authorization: Option<PublicationAuthorization>,
    /// Set when the authorized revision was published (SL08).
    #[serde(default)]
    pub publication: Option<PublicationRecord>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SkillProposal {
    /// The texts a rejection of this proposal fingerprints (see
    /// `NewRejection::material`): one per edit and one for a description
    /// change, each carrying both the removed and the added text.
    #[must_use]
    pub fn material_texts(&self) -> Vec<String> {
        material_of(self.diff.as_ref())
    }

    /// Whether `other` was prepared from the same inputs: same operation,
    /// skill, baseline, generation, patterns, evidence and environment.
    pub(super) fn same_request(&self, other: &Self) -> bool {
        self.operation == other.operation
            && self.skill_id == other.skill_id
            && self.requested_patterns == other.requested_patterns
            && self.baseline_revision == other.baseline_revision
            && self.expected_generation == other.expected_generation
            && self.pattern_ids == other.pattern_ids
            && self.evidence_ids == other.evidence_ids
            && self.environment_binding == other.environment_binding
    }
}

/// Material texts of a diff; see [`SkillProposal::material_texts`].
pub(super) fn material_of(diff: Option<&BoundedDiff>) -> Vec<String> {
    let Some(diff) = diff else {
        return Vec::new();
    };
    let mut texts: Vec<String> = diff
        .edits
        .iter()
        .map(|edit| {
            format!(
                "{:?}\n{}\n=>\n{}",
                edit.op,
                edit.old.as_deref().unwrap_or_default(),
                edit.new
            )
        })
        .collect();
    if let Some(change) = &diff.description_change {
        texts.push(format!("description\n{}\n=>\n{}", change.old, change.new));
    }
    texts
}

/// A proposal directory whose record could not be read or verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnreadableProposal {
    pub proposal_id: String,
    pub reason: String,
}

fn sort_proposals(proposals: &mut [SkillProposal]) {
    proposals.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.proposal_id.cmp(&b.proposal_id))
    });
}

pub(super) fn is_proposal_id(id: &str) -> bool {
    id.strip_prefix("prop-").is_some_and(|digest| {
        digest.len() == 32 && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// Deterministic proposal id of `(owner, request id)`.
pub(super) fn proposal_id_of(owner: &OwnerScope, request_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"prx-skill-proposal\0");
    for part in [owner.as_str(), request_id] {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hex::encode(hasher.finalize());
    format!("prop-{}", digest.get(..32).unwrap_or(&digest))
}

pub(super) fn check_request_id(request_id: &str) -> Result<(), LifecycleError> {
    check_ident("request id", request_id, MAX_REQUEST_ID_BYTES)
}

/// Truncates free text to the record bound on a char boundary.
pub(super) fn bounded(text: &str) -> String {
    if text.len() <= MAX_DETAIL_BYTES {
        return text.to_string();
    }
    let mut end = MAX_DETAIL_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default().to_string()
}

/// Guards of the proposal lock; released on drop.
pub(super) struct ProposalLock {
    _store: StoreLock,
    _skill: SkillLock,
    pub(super) lock_id: SkillId,
}

impl SkillLifecycleStore {
    fn proposals_dir(&self) -> PathBuf {
        self.root().join(PROPOSALS_DIR)
    }

    fn proposal_dir(&self, id: &str) -> PathBuf {
        self.proposals_dir().join(id)
    }

    /// Path of a file inside a proposal's directory.
    pub(super) fn proposal_file(&self, id: &str, name: &str) -> PathBuf {
        self.proposal_dir(id).join(name)
    }

    pub(super) fn lock_proposals(&self) -> Result<ProposalLock, LifecycleError> {
        ensure_dir_synced(self.root(), &self.proposals_dir())?;
        let lock_id = reserved_lock(PROPOSAL_LOCK_ID)?;
        let store = self.lock_store(true, LOCK_TIMEOUT)?;
        let skill = self.lock_skill(&lock_id, LOCK_TIMEOUT)?;
        Ok(ProposalLock {
            _store: store,
            _skill: skill,
            lock_id,
        })
    }

    /// Reads one proposal; `None` when it does not exist. The record must
    /// belong to this owner, carry the id derived from its request id, and, if
    /// it names a staged candidate, that candidate must still hash to the
    /// recorded revision.
    pub fn read_proposal(&self, id: &str) -> Result<Option<SkillProposal>, LifecycleError> {
        let proposal = self.read_proposal_record(id)?;
        if let Some(proposal) = &proposal
            && let Some(revision) = &proposal.candidate_revision
        {
            let bundle = self.read_candidate_bundle(&proposal.proposal_id)?;
            if bundle.revision_id() != revision {
                return Err(LifecycleError::Tampered {
                    path: self.proposal_dir(id).join(CANDIDATE_DIR),
                    reason: "staged candidate does not hash to the recorded revision".to_string(),
                });
            }
        }
        Ok(proposal)
    }

    pub(super) fn read_proposal_record(&self, id: &str) -> Result<Option<SkillProposal>, LifecycleError> {
        if !is_proposal_id(id) {
            return Err(LifecycleError::InvalidId {
                kind: "proposal id",
                reason: "must be prop- followed by 32 lowercase hex digits".to_string(),
            });
        }
        let dir = self.proposal_dir(id);
        match fs::symlink_metadata(&dir) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(LifecycleError::io(&dir, e)),
        }
        check_private(&dir)?;
        let path = dir.join(PROPOSAL_FILE);
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(LifecycleError::io(&path, e)),
        }
        check_private(&path)?;
        let record: SkillProposal = read_record(&path)?;
        if record.schema_version != SCHEMA_VERSION {
            return Err(LifecycleError::UnsupportedSchema {
                what: "proposal",
                found: record.schema_version,
            });
        }
        self.check_owner_scope(&record.owner_scope)?;
        if record.record_id != id
            || record.proposal_id != id
            || proposal_id_of(self.owner_scope(), &record.request_id) != id
        {
            return Err(LifecycleError::Tampered {
                path,
                reason: "proposal identity does not match its location".to_string(),
            });
        }
        self.verify_proposal_bounds(&record, &path)?;
        Ok(Some(record))
    }

    fn verify_proposal_bounds(&self, record: &SkillProposal, path: &Path) -> Result<(), LifecycleError> {
        let tampered = |reason: &str| LifecycleError::Tampered {
            path: path.to_path_buf(),
            reason: reason.to_string(),
        };
        if record.attempts.len() > MAX_PROPOSAL_ATTEMPTS
            || record.pattern_ids.len() > MAX_PATTERN_EVIDENCE
            || record.evidence_ids.len() > MAX_PATTERN_EVIDENCE
        {
            return Err(tampered("proposal lists exceed their bounds"));
        }
        if record.candidate_revision.is_some() != record.diff.is_some() {
            return Err(tampered("candidate revision and diff must be present together"));
        }
        Ok(())
    }

    /// Every proposal, oldest first. Unreadable records fail the listing: a
    /// proposal that cannot be verified is not silently skipped.
    pub fn list_proposals(&self) -> Result<Vec<SkillProposal>, LifecycleError> {
        let mut proposals = Vec::new();
        for id in self.proposal_ids()? {
            if let Some(proposal) = self.read_proposal(&id)? {
                proposals.push(proposal);
            }
        }
        sort_proposals(&mut proposals);
        Ok(proposals)
    }

    /// Like [`Self::list_proposals`], but a record that cannot be read or
    /// verified is logged and returned in the second list instead of failing
    /// the call, so one damaged proposal never blocks the others. Only the
    /// directory scan itself can fail.
    pub fn list_proposals_lenient(&self) -> Result<(Vec<SkillProposal>, Vec<UnreadableProposal>), LifecycleError> {
        let mut proposals = Vec::new();
        let mut unreadable = Vec::new();
        for id in self.proposal_ids()? {
            match self.read_proposal(&id) {
                Ok(Some(proposal)) => proposals.push(proposal),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(proposal = %id, %error, "skipping an unreadable skill proposal record");
                    unreadable.push(UnreadableProposal {
                        proposal_id: id,
                        reason: bounded(&error.to_string()),
                    });
                }
            }
        }
        sort_proposals(&mut proposals);
        Ok((proposals, unreadable))
    }

    fn proposal_ids(&self) -> Result<Vec<String>, LifecycleError> {
        let dir = self.proposals_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(LifecycleError::io(&dir, e)),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| LifecycleError::io(&dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_proposal_id(&name) {
                continue;
            }
            if ids.len() >= MAX_PROPOSAL_LISTING {
                return Err(LifecycleError::KnowledgeLimit {
                    what: "proposal directories",
                    limit: MAX_PROPOSAL_LISTING,
                });
            }
            ids.push(name);
        }
        Ok(ids)
    }

    /// The staged candidate bundle of a proposal, verified as a whole.
    pub fn read_candidate_bundle(&self, id: &str) -> Result<Bundle, LifecycleError> {
        if !is_proposal_id(id) {
            return Err(LifecycleError::InvalidId {
                kind: "proposal id",
                reason: "must be prop- followed by 32 lowercase hex digits".to_string(),
            });
        }
        let dir = self.proposal_dir(id).join(CANDIDATE_DIR);
        let meta = fs::symlink_metadata(&dir).map_err(|e| LifecycleError::io(&dir, e))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(LifecycleError::UnsafeStorePath {
                path: dir,
                reason: "candidate must be a real directory".to_string(),
            });
        }
        Bundle::from_dir(&dir).map_err(|e| LifecycleError::Tampered {
            path: dir,
            reason: format!("candidate bundle invalid: {e}"),
        })
    }

    /// Writes (replaces) `proposal.json`. The caller holds the proposal lock.
    pub(super) fn write_proposal(&self, lock: &ProposalLock, proposal: &SkillProposal) -> Result<(), LifecycleError> {
        let dir = self.proposal_dir(&proposal.proposal_id);
        ensure_dir_synced(&self.proposals_dir(), &dir)?;
        let path = dir.join(PROPOSAL_FILE);
        self.write_atomic(&lock.lock_id, &path, &record_bytes(proposal, &path)?)
    }

    /// Stages `bundle` as the candidate of `id`, replacing a leftover from an
    /// interrupted earlier attempt. The proposal lock is held by the caller.
    pub(super) fn stage_candidate(&self, id: &str, bundle: &Bundle) -> Result<(), LifecycleError> {
        let proposal_dir = self.proposal_dir(id);
        ensure_dir_synced(&self.proposals_dir(), &proposal_dir)?;
        let target = proposal_dir.join(CANDIDATE_DIR);
        if fs::symlink_metadata(&target).is_ok() {
            fs::remove_dir_all(&target).map_err(|e| LifecycleError::io(&target, e))?;
        }
        ensure_real_dir(&target, true)?;
        let mut dirs: Vec<PathBuf> = vec![target.clone()];
        for file in bundle.files() {
            let path = target.join(file.path());
            if let Some(parent) = path.parent() {
                let mut chain = Vec::new();
                let mut cursor = parent;
                while cursor != target {
                    chain.push(cursor.to_path_buf());
                    match cursor.parent() {
                        Some(next) => cursor = next,
                        None => break,
                    }
                }
                for dir in chain.into_iter().rev() {
                    ensure_real_dir(&dir, true)?;
                    if !dirs.contains(&dir) {
                        dirs.push(dir);
                    }
                }
            }
            write_private_file(&path, file.bytes(), file.is_executable())?;
        }
        for dir in dirs.iter().rev() {
            sync_dir(dir)?;
        }
        sync_dir(&proposal_dir)?;
        let staged = Bundle::from_dir(&target).map_err(|e| LifecycleError::Tampered {
            path: target.clone(),
            reason: format!("staged candidate invalid: {e}"),
        })?;
        if staged.revision_id() != bundle.revision_id() {
            return Err(LifecycleError::Tampered {
                path: target,
                reason: "staged candidate does not hash to its revision".to_string(),
            });
        }
        Ok(())
    }

    /// Marks a proposal whose attempt was left in flight as failed. Only an
    /// attempt older than `grace` (counted from its start) is touched; a
    /// younger one may still be running in another process. The model may have
    /// been called, so the attempt is recorded as abandoned with unknown
    /// liability.
    pub(super) fn settle_interrupted(
        &self,
        lock: &ProposalLock,
        proposal: &mut SkillProposal,
        grace: Duration,
    ) -> Result<bool, LifecycleError> {
        let now = Utc::now();
        let limit = chrono::Duration::from_std(grace).unwrap_or(chrono::Duration::MAX);
        if proposal.status != ProposalStatus::Draft
            || !proposal
                .attempts
                .iter()
                .any(|a| a.outcome == AttemptOutcome::InFlight && now.signed_duration_since(a.started_at) >= limit)
        {
            return Ok(false);
        }
        for attempt in &mut proposal.attempts {
            if attempt.outcome == AttemptOutcome::InFlight {
                attempt.outcome = AttemptOutcome::Abandoned;
                attempt.finished_at = Some(now);
                attempt.detail = Some("interrupted while in flight; liability unknown".to_string());
            }
        }
        proposal.status = ProposalStatus::Failed;
        proposal.failure = Some(ProposalFailure {
            kind: ProposalFailureKind::Interrupted,
            detail: "the attempt never finished; the model may have been called and its cost is unknown".to_string(),
        });
        proposal.updated_at = now;
        self.write_proposal(lock, proposal)?;
        Ok(true)
    }

    /// Recovers proposals whose attempt has been in flight for at least `grace`
    /// (a process died mid-call): each becomes `Failed` with an abandoned
    /// attempt. Returns the recovered proposal ids.
    pub fn recover_interrupted_proposals(&self, grace: Duration) -> Result<Vec<String>, LifecycleError> {
        let lock = self.lock_proposals()?;
        let mut recovered = Vec::new();
        for mut proposal in self.list_proposals()? {
            // Never earlier than the proposal's own call deadline plus the margin.
            let own = Duration::from_millis(proposal.budget.call_timeout_ms) + INTERRUPTED_GRACE;
            if self.settle_interrupted(&lock, &mut proposal, grace.max(own))? {
                recovered.push(proposal.proposal_id);
            }
        }
        Ok(recovered)
    }

    /// Moves a pre-commit proposal to `Cancelled`. In-flight attempts are
    /// marked abandoned: their liability stays unknown. A staged candidate is
    /// kept (private) for audit; it can never be published from this state.
    pub(in crate::skills) fn cancel_proposal(&self, id: &str, reason: &str) -> Result<SkillProposal, LifecycleError> {
        let lock = self.lock_proposals()?;
        let mut proposal = self
            .read_proposal_record(id)?
            .ok_or_else(|| LifecycleError::ProposalRefused(format!("no such proposal {id}")))?;
        // A publication that is already live makes the proposal committed.
        self.settle_publication(&lock, &mut proposal)?;
        if !proposal.status.can_become(ProposalStatus::Cancelled) {
            return Err(LifecycleError::InvalidProposalTransition {
                proposal_id: proposal.proposal_id,
                from: proposal.status.as_str(),
                to: ProposalStatus::Cancelled.as_str(),
            });
        }
        let now = Utc::now();
        for attempt in &mut proposal.attempts {
            if attempt.outcome == AttemptOutcome::InFlight {
                attempt.outcome = AttemptOutcome::Abandoned;
                attempt.finished_at = Some(now);
            }
        }
        proposal.status = ProposalStatus::Cancelled;
        proposal.status_reason = Some(bounded(reason));
        proposal.updated_at = now;
        self.write_proposal(&lock, &proposal)?;
        Ok(proposal)
    }
}

/// Builds the entries of a candidate: every baseline file unchanged except
/// `SKILL.md`, which is replaced.
pub(super) fn candidate_entries(baseline: &Bundle, skill_md: &str, new_skill_md: Vec<u8>) -> Vec<BundleEntry> {
    baseline
        .files()
        .iter()
        .map(|file| BundleEntry {
            path: PathBuf::from(file.path()),
            bytes: if file.path() == skill_md {
                new_skill_md.clone()
            } else {
                file.bytes().to_vec()
            },
            executable: file.is_executable(),
        })
        .collect()
}

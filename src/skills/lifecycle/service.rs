//! The skill lifecycle service (SL09): the single policy owner every mutation
//! goes through.
//!
//! Operator settings ([`SkillsLifecycleSettings`]) decide what may happen:
//! the mode, the gate policy and the governing suite of each skill. The suite
//! registry is built by the operator's configuration code and has no setter
//! after construction, so neither the proposer nor an agent tool can choose
//! the suite a proposal is judged by. The service passes the policy and that
//! suite to `verify_gate`, `authorize_publication` and `publish_authorized`;
//! it never takes either from a proposal, a gate receipt or a caller.
//!
//! | mode        | review | evaluate, gate | authorize, publish, rollback |
//! | ----------- | ------ | -------------- | ---------------------------- |
//! | `DraftOnly` | yes    | no             | no                           |
//! | `Shadow`    | yes    | yes            | no                           |
//! | `Auto`      | yes    | yes            | only with a valid publication policy |
//!
//! A disabled service refuses every mutation except cancelling a proposal and
//! startup recovery. An installation without a valid publication policy stays
//! `DraftOnly`-like for publication (spec 8.2): [`SkillsLifecycleSettings::
//! publication_block`] says why.

use super::authorize::{Finalization, ForceClose, PublicationOutcome};
use super::evaluate::{CaseExecutor, EvaluationError};
use super::evaluator::{EvaluationRequest, RecordedEvaluation};
use super::gate::{GateError, GateOutcome, GatePolicy, GateRequest};
use super::gate_judge::GateJudge;
use super::proposal::{ProposalStatus, SkillProposal, bounded};
use super::publish::{RecoveryReport, RollbackRequest, TransactionOutcome};
use super::store::SkillLifecycleStore;
use super::suite::{EvalSplit, FrozenSuite};
use super::types::{LifecycleError, RevisionId, SkillId};
use super::{EvaluationPin, SourceReviewOutcome, SourceReviewRecord, TransactionId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// Smallest `min_cases` floor a publication policy may have (the SL07 hard
/// gate: the floor is above 2).
pub const MIN_PUBLICATION_CASES_FLOOR: u32 = 3;

/// What the lifecycle may do (spec 8.2). `Auto` is the spec's name for
/// publishing mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleMode {
    /// Drafts and reviews only: no evaluation spend, no publication.
    #[default]
    DraftOnly,
    /// Drafts plus bounded evaluation and gating; never publishes.
    Shadow,
    /// Eligible promotion after every gate, under a valid publication policy.
    Auto,
}

impl LifecycleMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DraftOnly => "draft_only",
            Self::Shadow => "shadow",
            Self::Auto => "auto",
        }
    }
}

impl fmt::Display for LifecycleMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The gate policy an installation gets unless the operator sets another one:
/// a holdout split is required and a governing suite needs more than 2 cases
/// per split.
#[must_use]
pub const fn default_gate_policy() -> GatePolicy {
    GatePolicy {
        version: 1,
        max_regressions: 0,
        valid_for_secs: 24 * 3600,
        judge_required: false,
        judge_timeout_ms: 60_000,
        require_holdout: true,
        min_cases_floor: MIN_PUBLICATION_CASES_FLOOR,
        judge_identity: None,
    }
}

/// Per-cycle limits from `[skills_lifecycle]`. They bound the work a proposing
/// or evaluating cycle may start. The service reports them in its status; the
/// proposal path will take them from here once it is wired (SL05 gate).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct LifecycleLimits {
    pub max_concurrent_evaluations: u32,
    pub max_proposals_per_cycle: u32,
    pub max_edit_tokens: u64,
    pub max_edit_fraction: f64,
}

impl Default for LifecycleLimits {
    fn default() -> Self {
        Self {
            max_concurrent_evaluations: 1,
            max_proposals_per_cycle: 1,
            max_edit_tokens: 300,
            max_edit_fraction: 0.20,
        }
    }
}

/// Operator-owned map from a skill to the digest of the frozen suite that
/// governs its proposals. It is built once from configuration; there is no way
/// to add, change or remove an entry afterwards.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SuiteRegistry {
    suites: BTreeMap<SkillId, String>,
}

impl SuiteRegistry {
    /// Builds the registry; every digest must be 64 lowercase hex digits.
    pub fn new(entries: impl IntoIterator<Item = (SkillId, String)>) -> Result<Self, ServiceError> {
        let mut suites = BTreeMap::new();
        for (skill, digest) in entries {
            let valid = digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !valid {
                return Err(ServiceError::Settings(format!(
                    "the suite digest of skill {skill} is not 64 lowercase hex digits"
                )));
            }
            suites.insert(skill, digest);
        }
        Ok(Self { suites })
    }

    #[must_use]
    pub fn suite_for(&self, skill: &SkillId) -> Option<&str> {
        self.suites.get(skill).map(String::as_str)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.suites.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.suites.is_empty()
    }
}

/// The operator's lifecycle settings. They belong to the service; no tool or
/// model output can replace them. (`[skills_lifecycle]` builds them; see
/// `config::skills_lifecycle`.)
#[derive(Clone, Debug)]
pub struct SkillsLifecycleSettings {
    pub enabled: bool,
    pub mode: LifecycleMode,
    pub gate_policy: GatePolicy,
    pub suites: SuiteRegistry,
    pub limits: LifecycleLimits,
}

impl Default for SkillsLifecycleSettings {
    /// Disabled, `DraftOnly`, the default gate policy and no suites.
    fn default() -> Self {
        Self {
            enabled: false,
            mode: LifecycleMode::DraftOnly,
            gate_policy: default_gate_policy(),
            suites: SuiteRegistry::default(),
            limits: LifecycleLimits::default(),
        }
    }
}

impl SkillsLifecycleSettings {
    /// Checks the settings are internally valid (the gate policy itself).
    pub fn validate(&self) -> Result<(), ServiceError> {
        self.gate_policy
            .validate()
            .map_err(|e| ServiceError::Settings(e.to_string()))
    }

    /// Why publication is not possible under these settings, `None` when it
    /// is. A publication policy is valid when it requires a holdout split and
    /// its suite floor is above 2.
    #[must_use]
    pub fn publication_block(&self) -> Option<String> {
        if !self.enabled {
            return Some("the lifecycle is disabled".to_string());
        }
        if self.mode != LifecycleMode::Auto {
            return Some(format!("the mode is {}, publication needs auto", self.mode));
        }
        if !self.gate_policy.require_holdout {
            return Some("the gate policy does not require a holdout split".to_string());
        }
        if self.gate_policy.min_cases_floor < MIN_PUBLICATION_CASES_FLOOR {
            return Some(format!(
                "the gate policy accepts suites below {MIN_PUBLICATION_CASES_FLOOR} cases per split"
            ));
        }
        None
    }
}

/// Failure of a service call. Every variant is a refusal that leaves the
/// proposal as the last successful step left it.
#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("the skill lifecycle is disabled")]
    Disabled,
    #[error("the lifecycle mode {mode} does not allow {action}")]
    ModeForbids { mode: LifecycleMode, action: &'static str },
    #[error("publication is blocked: {0}")]
    PublicationBlocked(String),
    #[error("invalid lifecycle settings: {0}")]
    Settings(String),
    #[error("no governing suite is registered for skill {0}")]
    NoGoverningSuite(String),
    #[error("the suite {digest} is not the registered suite of skill {skill}")]
    SuiteNotRegistered { skill: String, digest: String },
    #[error("no such proposal {0}")]
    NoSuchProposal(String),
    #[error("proposal {0} has no sealed gate decision to publish against")]
    NoGateDecision(String),
    /// Another process holds a lock the call needs. Nothing changed; try again.
    #[error("a lifecycle lock is held by another process ({path}); retry later")]
    Busy { path: String },
    #[error(transparent)]
    Lifecycle(LifecycleError),
    #[error(transparent)]
    Gate(GateError),
    #[error(transparent)]
    Evaluation(EvaluationError),
    #[error("lifecycle worker failed: {0}")]
    Worker(String),
}

impl ServiceError {
    /// Whether the same call can succeed later without any other change.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Busy { .. })
    }
}

fn busy_path(error: &LifecycleError) -> Option<String> {
    match error {
        LifecycleError::LockTimeout { path } => Some(path.display().to_string()),
        _ => None,
    }
}

impl From<LifecycleError> for ServiceError {
    fn from(error: LifecycleError) -> Self {
        busy_path(&error).map_or(Self::Lifecycle(error), |path| Self::Busy { path })
    }
}

impl From<GateError> for ServiceError {
    fn from(error: GateError) -> Self {
        match error {
            GateError::Lifecycle(inner) => inner.into(),
            GateError::Evaluation(inner) => inner.into(),
            other => Self::Gate(other),
        }
    }
}

impl From<EvaluationError> for ServiceError {
    fn from(error: EvaluationError) -> Self {
        match error {
            EvaluationError::Lifecycle(inner) => inner.into(),
            other => Self::Evaluation(other),
        }
    }
}

/// Most skipped proposals a [`StartupSummary`] lists; the rest are counted.
const MAX_SUMMARY_SKIPPED: usize = 32;
const STATUS_DIR: &str = "status";
const STATUS_LOCK_ID: &str = "_skill-status";
const STARTUP_SUMMARY_FILE: &str = "last_startup.json";

/// A proposal startup recovery could not read or settle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedProposal {
    pub proposal_id: String,
    pub reason: String,
}

/// The part of a [`StartupReport`] lifecycle status shows: what recovery
/// settled and which proposals it had to skip.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupSummary {
    pub transactions_committed: usize,
    pub transactions_aborted: usize,
    pub finalized_proposals: usize,
    pub interrupted_proposals: usize,
    pub skipped_total: usize,
    /// The first skipped proposals (bounded); `skipped_total` counts all.
    pub skipped: Vec<SkippedProposal>,
    pub warnings: Vec<String>,
}

impl StartupSummary {
    fn of(report: &StartupReport) -> Self {
        Self {
            transactions_committed: report.recovery.committed.len(),
            transactions_aborted: report.recovery.aborted.len(),
            finalized_proposals: report.finalized.len(),
            interrupted_proposals: report.interrupted.len(),
            skipped_total: report.skipped.len(),
            skipped: report
                .skipped
                .iter()
                .take(MAX_SUMMARY_SKIPPED)
                .map(|(proposal_id, reason)| SkippedProposal {
                    proposal_id: proposal_id.clone(),
                    reason: bounded(reason),
                })
                .collect(),
            warnings: report.warnings.iter().map(|w| bounded(w)).collect(),
        }
    }
}

/// What startup recovery did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartupReport {
    pub recovery: RecoveryReport,
    /// Authorized proposals whose committed publication was recorded.
    pub finalized: Vec<String>,
    /// Proposals that could not be read or settled (logged, never blocking the
    /// others), with the reason.
    pub skipped: Vec<(String, String)>,
    /// Proposals whose model call was in flight when the process died.
    pub interrupted: Vec<String>,
    /// Steps that failed without stopping recovery.
    pub warnings: Vec<String>,
}

/// One proposal as status output shows it: ids, state and reasons, never
/// candidate text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProposalSummary {
    pub proposal_id: String,
    pub skill_id: String,
    pub status: &'static str,
    pub status_reason: Option<String>,
    /// `None` for a new skill, which has no baseline revision.
    pub baseline_revision: Option<String>,
    pub candidate_revision: Option<String>,
    pub expected_generation: u64,
    pub evaluation_epoch: u32,
    /// Why the proposal cannot go on right now under the operator settings.
    pub blocked_by: Option<String>,
    /// The next prerequisite.
    pub next: &'static str,
}

/// A proposal in detail: the summary plus sizes, hashes and receipts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProposalDetail {
    #[serde(flatten)]
    pub summary: ProposalSummary,
    pub failure: Option<String>,
    pub changed_tokens: Option<u64>,
    pub allowed_tokens: Option<u64>,
    pub edit_count: Option<usize>,
    pub source_review: Option<ReviewSummary>,
    pub required_suite: Option<String>,
    pub gate_decision: Option<&'static str>,
    pub gate_digest: Option<String>,
    pub published_revision: Option<String>,
    pub published_generation: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewSummary {
    pub passed: bool,
    pub reviewer: String,
    pub findings: Vec<String>,
}

/// Read-only status of the lifecycle.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LifecycleStatus {
    pub enabled: bool,
    pub mode: LifecycleMode,
    /// The owner scope whose store this is (`[skills_lifecycle].owner_scope`).
    pub owner_scope: String,
    pub publication_block: Option<String>,
    pub registered_suites: usize,
    pub proposals_by_status: BTreeMap<&'static str, usize>,
    pub unreadable_proposals: usize,
    pub limits: LifecycleLimits,
    /// What the last startup recovery (of any process) did, `None` when none
    /// ran or its record is unreadable.
    pub last_startup: Option<StartupSummary>,
}

/// The policy owner. Cheap to share behind an `Arc`.
#[derive(Debug)]
pub struct SkillsLifecycleService {
    store: Arc<SkillLifecycleStore>,
    settings: SkillsLifecycleSettings,
    recovery_lock_timeout: Duration,
}

/// What the service does with a proposal next, and what stops it.
const fn next_step(status: ProposalStatus) -> &'static str {
    match status {
        ProposalStatus::Draft => "run the source review",
        ProposalStatus::SourceReviewed => "begin evaluation under the registered suite",
        ProposalStatus::Evaluating => "record validation and holdout evaluations, then mark it evaluated",
        ProposalStatus::Evaluated => "gate it with the recorded evaluations",
        ProposalStatus::Eligible => "authorize and publish it",
        ProposalStatus::Inconclusive => "begin a new evaluation epoch",
        ProposalStatus::PublicationAuthorized => "publish it",
        ProposalStatus::Committed | ProposalStatus::Monitored => "monitor the published revision",
        ProposalStatus::Rejected
        | ProposalStatus::Cancelled
        | ProposalStatus::Stale
        | ProposalStatus::Failed
        | ProposalStatus::Retained
        | ProposalStatus::Disabled
        | ProposalStatus::RolledBack => "nothing; the proposal is closed",
    }
}

impl SkillsLifecycleService {
    /// Builds the service over an owner store with the operator's settings.
    pub fn new(store: Arc<SkillLifecycleStore>, settings: SkillsLifecycleSettings) -> Result<Self, ServiceError> {
        settings.validate()?;
        Ok(Self {
            store,
            settings,
            recovery_lock_timeout: super::publish::PUBLICATION_LOCK_TIMEOUT,
        })
    }

    /// Overrides how long startup recovery waits for the store lock.
    #[must_use]
    pub const fn with_recovery_lock_timeout(mut self, timeout: Duration) -> Self {
        self.recovery_lock_timeout = timeout;
        self
    }

    #[must_use]
    pub const fn settings(&self) -> &SkillsLifecycleSettings {
        &self.settings
    }

    /// The owner scope whose store this service manages.
    #[must_use]
    pub fn owner_scope(&self) -> &super::types::OwnerScope {
        self.store.owner_scope()
    }

    #[must_use]
    pub const fn mode(&self) -> LifecycleMode {
        self.settings.mode
    }

    const fn require_enabled(&self) -> Result<(), ServiceError> {
        if self.settings.enabled {
            Ok(())
        } else {
            Err(ServiceError::Disabled)
        }
    }

    fn require_evaluation(&self, action: &'static str) -> Result<(), ServiceError> {
        self.require_enabled()?;
        if self.settings.mode < LifecycleMode::Shadow {
            return Err(ServiceError::ModeForbids {
                mode: self.settings.mode,
                action,
            });
        }
        Ok(())
    }

    fn require_publication(&self, action: &'static str) -> Result<(), ServiceError> {
        self.require_enabled()?;
        if self.settings.mode != LifecycleMode::Auto {
            return Err(ServiceError::ModeForbids {
                mode: self.settings.mode,
                action,
            });
        }
        self.settings
            .publication_block()
            .map_or(Ok(()), |reason| Err(ServiceError::PublicationBlocked(reason)))
    }

    fn registered_suite(&self, skill: &SkillId) -> Result<&str, ServiceError> {
        self.settings
            .suites
            .suite_for(skill)
            .ok_or_else(|| ServiceError::NoGoverningSuite(skill.to_string()))
    }

    fn proposal_of(&self, id: &str) -> Result<SkillProposal, ServiceError> {
        self.store
            .read_proposal(id)?
            .ok_or_else(|| ServiceError::NoSuchProposal(id.to_string()))
    }

    // ---- startup ------------------------------------------------------------

    /// Startup recovery: settles prepared publication transactions
    /// (`recover`), then records publications that went live
    /// (`finalize_published_proposals`), then settles proposals whose model
    /// call died. A proposal record that cannot be read is logged and
    /// skipped; it never blocks the others. A store lock held by another
    /// process is a retryable [`ServiceError::Busy`].
    ///
    /// Blocking; async callers use `spawn_blocking`. Runs in every mode, also
    /// when the lifecycle is disabled, because it only repairs.
    pub fn startup_recovery(&self) -> Result<StartupReport, ServiceError> {
        let recovery = self
            .store
            .recover_with_timeout(Duration::ZERO, self.recovery_lock_timeout)?;
        let Finalization { finalized, skipped } = self.store.finalize_published_proposals_lenient()?;
        let mut warnings = Vec::new();
        let interrupted = match self.store.recover_interrupted_proposals(Duration::ZERO) {
            Ok(ids) => ids,
            Err(error) => {
                tracing::warn!(%error, "could not settle interrupted skill proposals at startup");
                warnings.push(bounded(&format!("interrupted proposals were not settled: {error}")));
                Vec::new()
            }
        };
        for (proposal_id, reason) in &skipped {
            tracing::warn!(proposal_id = %proposal_id, reason = %reason, "startup recovery skipped a skill proposal");
        }
        let mut report = StartupReport {
            recovery,
            finalized,
            skipped,
            interrupted,
            warnings,
        };
        // Kept in the store so `status` from another process (the CLI) sees
        // what the daemon's recovery skipped.
        if let Err(error) = self.persist_startup_summary(&StartupSummary::of(&report)) {
            tracing::warn!(%error, "could not record the startup recovery summary");
            report
                .warnings
                .push(bounded(&format!("the startup summary was not recorded: {error}")));
        }
        Ok(report)
    }

    fn startup_summary_path(&self) -> std::path::PathBuf {
        self.store.root().join(STATUS_DIR).join(STARTUP_SUMMARY_FILE)
    }

    /// Writes the summary atomically with owner-only permissions.
    fn persist_startup_summary(&self, summary: &StartupSummary) -> Result<(), LifecycleError> {
        let path = self.startup_summary_path();
        if let Some(dir) = path.parent() {
            super::store::ensure_real_dir(dir, true)?;
        }
        let bytes = super::store::record_bytes(summary, &path)?;
        // Staged under a reserved id, which no skill id can equal.
        let staging = super::consolidate::reserved_lock(STATUS_LOCK_ID)?;
        self.store.write_atomic(&staging, &path, &bytes)
    }

    /// The summary the last startup recovery of any process left in the
    /// store; `None` when none ran or the record is unreadable (logged).
    fn read_startup_summary(&self) -> Option<StartupSummary> {
        let path = self.startup_summary_path();
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                tracing::warn!(%error, "could not look for the startup summary");
                return None;
            }
        }
        let read = super::store::check_private(&path).and_then(|()| super::store::read_record(&path));
        match read {
            Ok(summary) => Some(summary),
            Err(error) => {
                tracing::warn!(%error, "the startup summary is unreadable");
                None
            }
        }
    }

    // ---- read-only ------------------------------------------------------------

    /// Counts of proposals by state and the operator settings in force.
    pub fn status(&self) -> Result<LifecycleStatus, ServiceError> {
        let (proposals, unreadable) = self.store.list_proposals_lenient()?;
        let mut proposals_by_status = BTreeMap::new();
        for proposal in &proposals {
            *proposals_by_status.entry(proposal.status.as_str()).or_insert(0) += 1;
        }
        Ok(LifecycleStatus {
            enabled: self.settings.enabled,
            mode: self.settings.mode,
            owner_scope: self.store.owner_scope().to_string(),
            publication_block: self.settings.publication_block(),
            registered_suites: self.settings.suites.len(),
            proposals_by_status,
            unreadable_proposals: unreadable.len(),
            limits: self.settings.limits,
            last_startup: self.read_startup_summary(),
        })
    }

    fn summarize(&self, proposal: &SkillProposal) -> ProposalSummary {
        ProposalSummary {
            proposal_id: proposal.proposal_id.clone(),
            skill_id: proposal.skill_id.to_string(),
            status: proposal.status.as_str(),
            status_reason: proposal.status_reason.clone(),
            baseline_revision: proposal.baseline_revision.as_ref().map(ToString::to_string),
            candidate_revision: proposal.candidate_revision.as_ref().map(ToString::to_string),
            expected_generation: proposal.expected_generation,
            evaluation_epoch: proposal.evaluation_epoch,
            blocked_by: self.blocked_by(proposal),
            next: next_step(proposal.status),
        }
    }

    fn blocked_by(&self, proposal: &SkillProposal) -> Option<String> {
        if !proposal.status.is_live() {
            return None;
        }
        if !self.settings.enabled {
            return Some("the lifecycle is disabled".to_string());
        }
        match proposal.status {
            ProposalStatus::SourceReviewed
            | ProposalStatus::Inconclusive
            | ProposalStatus::Evaluating
            | ProposalStatus::Evaluated
                if self.settings.mode < LifecycleMode::Shadow =>
            {
                Some(format!(
                    "the mode is {}, evaluation needs shadow or auto",
                    self.settings.mode
                ))
            }
            ProposalStatus::SourceReviewed | ProposalStatus::Inconclusive
                if self.settings.suites.suite_for(&proposal.skill_id).is_none() =>
            {
                Some("no governing suite is registered for the skill".to_string())
            }
            ProposalStatus::Eligible | ProposalStatus::PublicationAuthorized => self.settings.publication_block(),
            _ => None,
        }
    }

    /// Proposals, oldest first, optionally of one skill. Unreadable records
    /// are counted by [`Self::status`], not listed.
    pub fn list_proposals(&self, skill: Option<&SkillId>) -> Result<Vec<ProposalSummary>, ServiceError> {
        let (proposals, _) = self.store.list_proposals_lenient()?;
        Ok(proposals
            .iter()
            .filter(|p| skill.is_none_or(|s| &p.skill_id == s))
            .map(|p| self.summarize(p))
            .collect())
    }

    /// One proposal with its review, gate and publication facts (no candidate
    /// text).
    pub fn inspect_proposal(&self, id: &str) -> Result<ProposalDetail, ServiceError> {
        let proposal = self.proposal_of(id)?;
        let review = self.store.read_source_review(id)?;
        let gate = self.store.recorded_gate_for(&proposal)?;
        Ok(ProposalDetail {
            summary: self.summarize(&proposal),
            failure: proposal
                .failure
                .as_ref()
                .map(|f| bounded(&format!("{:?}: {}", f.kind, f.detail))),
            changed_tokens: proposal.diff.as_ref().map(|d| d.changed_tokens),
            allowed_tokens: proposal.diff.as_ref().map(|d| d.allowed_tokens),
            edit_count: proposal.diff.as_ref().map(|d| d.edits.len()),
            source_review: review.as_ref().map(review_summary),
            required_suite: proposal.required_suite.clone(),
            gate_decision: gate.as_ref().map(|g| match g.decision.decision {
                super::gate::GateDecision::Accept => "accept",
                super::gate::GateDecision::Reject => "reject",
            }),
            gate_digest: gate.map(|g| g.digest),
            published_revision: proposal.publication.as_ref().map(|p| p.revision_id.to_string()),
            published_generation: proposal.publication.as_ref().map(|p| p.generation),
        })
    }

    // ---- mutations --------------------------------------------------------------

    /// Stores a frozen suite the operator registered. A suite whose digest is
    /// not the registry's entry for its skill is refused, so the store cannot
    /// be filled with suites the policy does not govern by.
    pub fn install_operator_suite(&self, suite: &FrozenSuite) -> Result<(), ServiceError> {
        self.require_enabled()?;
        let skill = &suite.suite().skill_id;
        let registered = self.registered_suite(skill)?;
        if registered != suite.digest() {
            return Err(ServiceError::SuiteNotRegistered {
                skill: skill.to_string(),
                digest: suite.digest().to_string(),
            });
        }
        Ok(self.store.write_suite(suite)?)
    }

    /// Runs the deterministic source review of a `Draft` proposal.
    pub fn review_source(&self, id: &str, reviewer: &str) -> Result<SourceReviewOutcome, ServiceError> {
        self.require_enabled()?;
        Ok(self.store.review_source(id, reviewer)?)
    }

    /// Moves a reviewed (or inconclusive) proposal into evaluation under the
    /// skill's registered suite. The candidate revision is written into the
    /// main store first (the single writer), refused before any evaluation
    /// spend when another live proposal carries the same content or the
    /// revision exists with foreign provenance.
    pub fn begin_evaluation(&self, id: &str, reason: &str) -> Result<SkillProposal, ServiceError> {
        self.require_evaluation("evaluating a proposal")?;
        let proposal = self.proposal_of(id)?;
        let suite = self.registered_suite(&proposal.skill_id)?.to_string();
        self.store.install_candidate(id)?;
        Ok(self.store.begin_evaluation(id, &suite, reason)?)
    }

    /// Runs one split of the registered suite for an `Evaluating` proposal.
    pub async fn evaluate(
        &self,
        id: &str,
        split: EvalSplit,
        executor: &dyn CaseExecutor,
        cancel: &CancellationToken,
    ) -> Result<RecordedEvaluation, ServiceError> {
        self.require_evaluation("evaluating a proposal")?;
        let proposal = self.proposal_of(id)?;
        if proposal.status != ProposalStatus::Evaluating {
            return Err(LifecycleError::InvalidProposalTransition {
                proposal_id: proposal.proposal_id,
                from: proposal.status.as_str(),
                to: ProposalStatus::Evaluating.as_str(),
            }
            .into());
        }
        let registered = self.registered_suite(&proposal.skill_id)?.to_string();
        if proposal.required_suite.as_deref() != Some(registered.as_str()) {
            return Err(ServiceError::SuiteNotRegistered {
                skill: proposal.skill_id.to_string(),
                digest: proposal.required_suite.unwrap_or_default(),
            });
        }
        let candidate = proposal
            .candidate_revision
            .clone()
            .ok_or_else(|| LifecycleError::ProposalRefused("the proposal has no staged candidate".to_string()))?;
        let suite = self.store.read_suite(&registered)?;
        let request = EvaluationRequest {
            suite: &suite,
            split,
            baseline: proposal.baseline_revision.clone(),
            candidate,
            observed_generation: proposal.expected_generation,
        };
        Ok(self.store.evaluate(&request, executor, cancel).await?)
    }

    /// Marks an `Evaluating` proposal `Evaluated`: its evaluations are
    /// recorded and the gate may look at them.
    pub fn finish_evaluation(&self, id: &str, note: &str) -> Result<SkillProposal, ServiceError> {
        self.require_evaluation("evaluating a proposal")?;
        Ok(self.store.advance_proposal(id, ProposalStatus::Evaluated, note)?)
    }

    /// Gates an `Evaluated` proposal under the operator's policy and the
    /// registered suite.
    pub async fn gate(
        &self,
        id: &str,
        evaluations: &[EvaluationPin],
        judge: Option<&dyn GateJudge>,
        cancel: &CancellationToken,
    ) -> Result<GateOutcome, ServiceError> {
        self.require_evaluation("gating a proposal")?;
        let proposal = self.proposal_of(id)?;
        let suite = self.registered_suite(&proposal.skill_id)?;
        Ok(self
            .store
            .gate_proposal(
                &GateRequest {
                    proposal_id: id,
                    suite_digest: suite,
                    evaluations,
                    policy: &self.settings.gate_policy,
                },
                judge,
                cancel,
            )
            .await?)
    }

    /// Authorizes (when the proposal is `Eligible`) and publishes a proposal.
    /// Only `Auto` mode under a valid publication policy gets here; the gate
    /// is verified again with the operator policy and registered suite at both
    /// steps. A lock held by another process is a retryable
    /// [`ServiceError::Busy`] and leaves the proposal authorized; calling
    /// again resumes.
    pub fn publish(&self, id: &str) -> Result<PublicationOutcome, ServiceError> {
        self.require_publication("publishing a proposal")?;
        let proposal = self.proposal_of(id)?;
        let suite = self.registered_suite(&proposal.skill_id)?.to_string();
        if proposal.status == ProposalStatus::Eligible {
            let gate = self
                .store
                .recorded_gate_for(&proposal)?
                .ok_or_else(|| ServiceError::NoGateDecision(id.to_string()))?;
            self.store
                .authorize_publication(id, &gate.digest, &self.settings.gate_policy, &suite)?;
        }
        Ok(self.store.publish_authorized(id, &self.settings.gate_policy, &suite)?)
    }

    /// Re-activates a revision that was published before. Same gate as
    /// publication: `Auto` mode under a valid publication policy. Rollback is
    /// refused in `DraftOnly` and `Shadow` on purpose: spec 8.2 gives those
    /// modes no active-revision behavior, and a rollback changes the active
    /// revision just as a publication does.
    pub fn rollback(
        &self,
        skill: &SkillId,
        target: &RevisionId,
        expected_generation: u64,
    ) -> Result<TransactionOutcome, ServiceError> {
        self.require_publication("rolling back a skill")?;
        Ok(self.store.rollback(&RollbackRequest {
            transaction_id: TransactionId::generate(),
            skill_id: skill.clone(),
            target: target.clone(),
            expected_generation,
        })?)
    }

    /// Operator repair: closes a `PublicationAuthorized` proposal whose
    /// publication cannot go on, in practice one stuck on a
    /// `TransactionConflict` that `cancel` and `publish` both keep hitting.
    /// The proposal ends `Failed` or `Stale` with the operator's reason on
    /// record. The active binding is left exactly as it is and nothing is
    /// published. When the publication turns out to be live, the proposal is
    /// not closed: it is recorded `Committed` from the journal entry and
    /// returned as such, so it never stays authorized beside a live revision.
    ///
    /// Operator-only: the CLI calls it, no agent tool action maps to it.
    /// Allowed in every mode (it only closes), but not when the lifecycle is
    /// disabled.
    pub fn force_close(&self, id: &str, reason: &str, outcome: ForceClose) -> Result<SkillProposal, ServiceError> {
        self.require_enabled()?;
        Ok(self.store.force_close_proposal(id, reason, outcome)?)
    }

    /// Cancels a pre-commit proposal. Allowed in every mode: it only closes.
    pub fn cancel(&self, id: &str, reason: &str) -> Result<SkillProposal, ServiceError> {
        // Truncated by bytes on a char boundary.
        Ok(self.store.cancel_proposal(id, &bounded(reason))?)
    }
}

fn review_summary(record: &SourceReviewRecord) -> ReviewSummary {
    ReviewSummary {
        passed: record.passed,
        reviewer: record.reviewer.clone(),
        findings: record
            .findings
            .iter()
            .map(|f| {
                format!(
                    "{:?} {}: {}",
                    f.check,
                    if f.passed { "passed" } else { "failed" },
                    f.detail
                )
            })
            .collect(),
    }
}

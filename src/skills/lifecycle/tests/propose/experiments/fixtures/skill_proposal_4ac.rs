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
    /// The active revision the proposal revises; `None` for a NEW skill
    /// (`operation == Create`), whose baseline is doing the task without the
    /// skill and whose expected generation is 0.
    pub baseline_revision: Option<RevisionId>,
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
    /// Append-only owner-local references to newly bound evaluation attempts.
    /// Empty legacy records acquire no inferred execution lease.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bound_evaluation_attempts: Vec<String>,
    /// Set when the proposal became `PublicationAuthorized` (SL08).
    #[serde(default)]
    pub authorization: Option<PublicationAuthorization>,
    /// Set when the authorized revision was published (SL08).
    #[serde(default)]
    pub publication: Option<PublicationRecord>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

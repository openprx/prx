//! Verifiable outcome facts of a task terminal (SL05 entry conditions).
//!
//! A failed task is evidence against a skill's guidance only when three things
//! are known, not assumed:
//!
//! * the provider and the tool backends were not the cause
//!   ([`TerminalFacts::infrastructure_unavailable`]);
//! * no tool call was refused by policy or approval
//!   ([`TerminalFacts::permission_refused`]);
//! * the skill's required tools were actually called ([`Adherence::Followed`]).
//!
//! Every fact has an `Unknown` state, which is also what an old record or a
//! terminal without observations carries. `Unknown` never satisfies the
//! entry condition: only a positive `Absent` / `Followed` does.
//!
//! The runtime reports plain observations ([`TerminalObservations`]: content
//! free capability ids with outcome codes, counters and the provider's end
//! state). All interpretation lives here, so the terminal owner passes values
//! through and decides nothing.

use super::capture::SkillExperience;
use super::store::SkillLifecycleStore;
use super::types::{RevisionId, SkillId};
use serde::{Deserialize, Serialize};

/// Skill revisions whose adherence is judged per terminal.
pub const MAX_ADHERENCE_SKILLS: usize = 32;
/// Declared names listed in one verdict.
pub const MAX_ADHERENCE_NAMES: usize = 16;
const MAX_NAME_BYTES: usize = 128;

/// A fact that may not be known. `Unknown` is the default and never counts as
/// "not the cause".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactState {
    #[default]
    Unknown,
    /// Observed completely and not present.
    Absent,
    Present,
}

/// Whether a run followed one skill revision's verifiable instructions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Adherence {
    /// No verifiable signal: the revision declares nothing checkable, the
    /// observation was incomplete, or a declared tool was refused.
    #[default]
    Unknown,
    /// Every declared tool was called.
    Followed,
    /// A declared tool was never called although the observation was complete.
    NotFollowed,
}

/// Adherence verdict of one hydrated skill revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillAdherence {
    pub skill_id: SkillId,
    pub revision_id: RevisionId,
    pub verdict: Adherence,
    /// Declared tools that were never called (only for `NotFollowed`).
    #[serde(default)]
    pub missing: Vec<String>,
}

/// Facts stored with the joined terminal. Every field defaults, so records
/// written before these facts existed read as "unknown".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalFacts {
    /// The provider was unavailable at the end of the run.
    #[serde(default)]
    pub infrastructure_unavailable: FactState,
    /// A tool call was refused by policy or approval.
    #[serde(default)]
    pub permission_refused: FactState,
    #[serde(default)]
    pub adherence: Vec<SkillAdherence>,
}

impl TerminalFacts {
    /// Adherence recorded for one skill revision; `Unknown` when none is.
    #[must_use]
    pub fn adherence_of(&self, skill: &SkillId, revision: &RevisionId) -> Adherence {
        self.adherence
            .iter()
            .find(|entry| entry.skill_id == *skill && entry.revision_id == *revision)
            .map_or(Adherence::Unknown, |entry| entry.verdict)
    }

    /// Whether a failure with these facts can be held against the guidance of
    /// `skill` at `revision`: neither infrastructure nor a permission refusal
    /// is a cause, and the skill's declared tools were used. Only positive
    /// facts qualify.
    #[must_use]
    pub fn failure_attributable_to(&self, skill: &SkillId, revision: &RevisionId) -> bool {
        self.infrastructure_unavailable == FactState::Absent
            && self.permission_refused == FactState::Absent
            && self.adherence_of(skill, revision) == Adherence::Followed
    }

    /// Why a failure cannot be held against the guidance, for refusal text.
    #[must_use]
    pub fn exclusion(&self, skill: &SkillId, revision: &RevisionId) -> Option<Exclusion> {
        if self.infrastructure_unavailable == FactState::Present {
            Some(Exclusion::InfrastructureUnavailable)
        } else if self.permission_refused == FactState::Present {
            Some(Exclusion::PermissionRefused)
        } else if self.failure_attributable_to(skill, revision) {
            None
        } else {
            Some(Exclusion::NotVerified)
        }
    }
}

/// Why one failed experience is not evidence for a revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exclusion {
    InfrastructureUnavailable,
    PermissionRefused,
    /// A needed fact is unknown, or adherence is not verified.
    NotVerified,
}

/// Outcome code of one observed tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservedCallStatus {
    Succeeded,
    Failed,
    /// Refused by policy, approval or a stale capability.
    Denied,
    Cancelled,
    AwaitingApproval,
}

impl ObservedCallStatus {
    /// Maps the runtime trace's outcome code; `None` for a code it does not know.
    #[must_use]
    pub fn from_trace_code(code: &str) -> Option<Self> {
        match code {
            "success" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "denied" => Some(Self::Denied),
            "cancelled" => Some(Self::Cancelled),
            "awaiting_approval" => Some(Self::AwaitingApproval),
            _ => None,
        }
    }
}

/// One tool call as the runtime trace saw it: a canonical capability id
/// (`family:namespace:name`) and its outcome; never arguments or results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedToolCall {
    pub capability_id: String,
    pub status: ObservedCallStatus,
}

/// How the provider call ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProviderEnd {
    /// Not observed, or ended for a reason that is not clearly availability.
    #[default]
    Unknown,
    /// A provider attempt succeeded and ended the run.
    Served,
    /// Every attempt failed for an availability reason.
    Unavailable,
}

impl ProviderEnd {
    /// Interprets the final error class of a failed provider run. Availability
    /// problems (timeout, rate limit, credentials, a failing endpoint) are not
    /// the skill's fault. The catch-all `provider_error` also labels any agent
    /// error (a max-iterations stop, for one) and a context overflow may come
    /// from the skill's own text, so those stay unknown.
    #[must_use]
    pub fn from_failure_class(class: &str) -> Self {
        match class {
            "timeout" | "rate_limit" | "rate_limited" | "auth" | "server_error" => Self::Unavailable,
            _ => Self::Unknown,
        }
    }
}

/// What the runtime observed at a task terminal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalObservations {
    pub tool_calls: Vec<ObservedToolCall>,
    /// Tool attempts refused by policy or approval, from the runtime counters.
    pub denied_attempts: u64,
    /// Tool attempts that failed because their backend was unavailable
    /// (connection, timeout, upstream 5xx, a down server). Argument and
    /// validation failures are not counted: they are the run's own errors.
    pub unavailable_attempts: u64,
    pub awaiting_approval_attempts: u64,
    /// The trace saw every attempt (nothing pending, unbound or over the bound).
    pub complete: bool,
    pub provider: ProviderEnd,
}

impl TerminalObservations {
    fn infrastructure(&self) -> FactState {
        if self.provider == ProviderEnd::Unavailable || self.unavailable_attempts > 0 {
            FactState::Present
        } else if self.provider == ProviderEnd::Served && self.complete {
            // A tool that failed on its arguments or on its own is the run's
            // error, not an unavailable backend: it does not hide a cause.
            FactState::Absent
        } else {
            FactState::Unknown
        }
    }

    fn permission(&self) -> FactState {
        let denied = self.denied_attempts > 0
            || self
                .tool_calls
                .iter()
                .any(|call| call.status == ObservedCallStatus::Denied);
        if denied {
            FactState::Present
        } else if self.complete && self.awaiting_approval_attempts == 0 {
            FactState::Absent
        } else {
            FactState::Unknown
        }
    }

    /// Whether a call of `name` (a full capability id, or its public name)
    /// reached the tool, whatever the tool then returned.
    fn called(&self, name: &str) -> Called {
        let mut result = Called::Never;
        for call in self
            .tool_calls
            .iter()
            .filter(|call| matches_name(&call.capability_id, name))
        {
            match call.status {
                ObservedCallStatus::Succeeded | ObservedCallStatus::Failed => return Called::Yes,
                ObservedCallStatus::Denied | ObservedCallStatus::Cancelled | ObservedCallStatus::AwaitingApproval => {
                    result = Called::Blocked;
                }
            }
        }
        result
    }
}

enum Called {
    Yes,
    /// Attempted but refused, cancelled or waiting: not proof either way.
    Blocked,
    Never,
}

/// Whether `declared` is exactly the canonical capability id of the call.
fn matches_name(capability_id: &str, declared: &str) -> bool {
    capability_id == declared
}

/// Tools a revision requires: exactly its stored `capability_requirements`
/// (the `required-tools` of its manifests), as canonical capability ids. A
/// skill's own `[[tools]]` entries are not scanned: they are exposed under
/// other ids and a learned revision could drop one.
fn declared_names(capability_requirements: &[String]) -> Vec<String> {
    let mut names: Vec<String> = capability_requirements
        .iter()
        .filter(|name| !name.is_empty() && name.len() <= MAX_NAME_BYTES && !name.chars().any(char::is_control))
        .cloned()
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Judges one revision's adherence from what it declares and what was called.
fn judge(observations: &TerminalObservations, declared: &[String]) -> (Adherence, Vec<String>) {
    if declared.is_empty() {
        return (Adherence::Unknown, Vec::new());
    }
    let mut missing = Vec::new();
    let mut blocked = false;
    for name in declared {
        match observations.called(name) {
            Called::Yes => {}
            Called::Blocked => blocked = true,
            Called::Never => missing.push(name.clone()),
        }
    }
    if !missing.is_empty() {
        // A call absent from an incomplete trace proves nothing.
        return if observations.complete {
            missing.truncate(MAX_ADHERENCE_NAMES);
            (Adherence::NotFollowed, missing)
        } else {
            (Adherence::Unknown, Vec::new())
        };
    }
    if blocked {
        (Adherence::Unknown, Vec::new())
    } else {
        (Adherence::Followed, Vec::new())
    }
}

/// Derives the stored facts of `experience` from the runtime's observations.
/// Without observations every fact stays unknown. A revision that cannot be
/// read has unknown adherence.
pub(super) fn derive(
    store: &SkillLifecycleStore,
    experience: &SkillExperience,
    observations: Option<&TerminalObservations>,
) -> TerminalFacts {
    let Some(observations) = observations else {
        return TerminalFacts::default();
    };
    let mut seen: Vec<(&SkillId, &RevisionId)> = Vec::new();
    for receipt in &experience.hydration_receipts {
        let key = (&receipt.skill_id, &receipt.revision_id);
        if !seen.contains(&key) {
            seen.push(key);
        }
    }
    seen.truncate(MAX_ADHERENCE_SKILLS);
    // With several skills in one run, one failure cannot be attributed to any
    // of them: their adherence stays unknown.
    let attributable = seen.len() == 1;
    let mut adherence = Vec::with_capacity(seen.len());
    for (skill, revision) in seen {
        let (verdict, missing) = match store.read_revision(skill, revision, None) {
            Ok(_) if !attributable => (Adherence::Unknown, Vec::new()),
            Ok(verified) => {
                let declared = declared_names(&verified.revision.capability_requirements);
                judge(observations, &declared)
            }
            Err(error) => {
                tracing::warn!(%error, skill = %skill, "adherence unknown: the hydrated revision could not be read");
                (Adherence::Unknown, Vec::new())
            }
        };
        adherence.push(SkillAdherence {
            skill_id: skill.clone(),
            revision_id: revision.clone(),
            verdict,
            missing,
        });
    }
    TerminalFacts {
        infrastructure_unavailable: observations.infrastructure(),
        permission_refused: observations.permission(),
        adherence,
    }
}

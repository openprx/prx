//! The versioned constants of the builtin decision model `builtin-v4`
//! (SP-CDP-A 5.7). Every number the builtin uses is here; changing one is a
//! new model version.

use super::features::Action;

/// The model identifier the builtin reports.
pub const MODEL_VERSION: &str = "builtin-v4";

/// An artifact younger than this (ms) is fresh evidence.
pub const FRESH_ARTIFACT_MS: u64 = 5 * 60 * 1000;
/// An artifact older than this (ms) may be stale.
pub const STALE_ARTIFACT_MS: u64 = 60 * 60 * 1000;

/// The evidence-sufficient probability of a plain conversational turn (or a
/// model-managed delegation) with no artifacts and no lookups. The builtin
/// returns exactly this value for such a turn, rather than whatever the
/// logistic of an empty evidence sum rounds to; the selector treats a value at
/// or above its threshold (also 0.5) as sufficient, so these turns answer
/// directly without a retrieve hop.
pub const PLAIN_TURN_EVIDENCE: f64 = 0.5;

/// Log-score bonus for a candidate the user named exactly.
pub const EXACT_MENTION: f64 = 2.0;
/// Log-score bonus for a web target when the text carries a URL. Above
/// `ln(0.30 / 0.10) = 1.099`, so a URL commits to an offered web tool over
/// a direct answer (D-061 M-2).
pub const URL_TO_WEB: f64 = 1.5;
/// Log-score bonus for a filesystem or document target when the text carries
/// a path (commits, as [`URL_TO_WEB`]).
pub const PATH_TO_FILES: f64 = 1.5;
/// Log-score bonus for a shell target when the text carries code (commits,
/// as [`URL_TO_WEB`]).
pub const CODE_TO_SHELL: f64 = 1.5;
/// Log-score penalty for a candidate whose capability already failed.
pub const FAILED_BEFORE: f64 = -3.0;
/// Log-score penalty for an unavailable candidate.
pub const UNAVAILABLE: f64 = -4.0;
/// Log-score penalty for using a tool that still needs approval.
pub const APPROVAL_STILL_REQUIRED: f64 = -0.5;
/// Log-score bonus for Retrieve when an artifact may be stale.
pub const STALE_TO_RETRIEVE: f64 = 0.8;
/// Log-score bonus for Clarify on an empty first turn. Large enough that
/// Clarify (prior 0.10) outranks Answer (prior 0.30) when there is nothing to
/// answer.
pub const EMPTY_TO_CLARIFY: f64 = 1.5;
/// Log-score bonus for Clarify after repeated retries.
pub const RETRIES_TO_CLARIFY: f64 = 0.5;
/// Retries at which [`RETRIES_TO_CLARIFY`] applies.
pub const RETRY_THRESHOLD: u64 = 2;
/// Log-score bonus for Discover when nothing is decisive.
pub const UNDECIDED_TO_DISCOVER: f64 = 0.7;
/// Log-score bonus for Answer when nothing is decisive.
pub const UNDECIDED_TO_ANSWER: f64 = 0.3;
/// Largest language-neutral request weight treated as a brief conversational
/// first turn. One or two contiguous alphanumeric runs are brief; longer
/// unrouted text may take the bounded discovery path before answering.
pub const BRIEF_REQUEST_MAX_WEIGHT: usize = 2;
/// Log-score bonus that makes Answer decisive for a brief first turn. It is
/// intentionally larger than the combined priors of the generic read and
/// discovery candidates so their mere availability cannot force Clarify.
pub const BRIEF_PLAIN_TURN_TO_ANSWER: f64 = 3.0;
/// Log-score bonus for RequestApproval while an approval is pending.
pub const PENDING_TO_APPROVAL: f64 = 1.0;
/// Log-score of an option that names no offered candidate (`other`, a code
/// no candidate carries).
pub const UNMATCHED_OPTION: f64 = -3.912_023_005_428_146;
/// Log-score shift for an action or target that is offered as an option but
/// that no candidate carries.
pub const OPTION_WITHOUT_CANDIDATE: f64 = -2.0;

/// Offset that maps a candidate log-score to its applicability logit
/// (`ln 0.1`): a candidate at a 10 % prior is even.
pub const APPLICABILITY_OFFSET: f64 = -std::f64::consts::LN_10;
/// Base logit of `candidate.*.success`.
pub const SUCCESS_BASE: f64 = 1.5;
/// Success logit per retry.
pub const SUCCESS_PER_RETRY: f64 = -0.5;
/// Width, in levels, of a Score distribution around its center.
pub const LEVEL_WIDTH: f64 = 0.75;
/// The highest aggregate confidence the builtin reports.
pub const CONFIDENCE_CAP: f64 = 0.9;
/// Risk added to a candidate whose capability already failed.
pub const FAILED_RISK: f64 = 0.2;
/// Expected-gain fraction when something is decisive.
pub const DECISIVE_GAIN: f64 = 0.7;
/// Expected-gain fraction when nothing is decisive.
pub const UNDECIDED_GAIN: f64 = 0.4;
/// Expected-gain reduction after an unsuccessful step.
pub const UNSUCCESSFUL_GAIN: f64 = -0.2;

/// The fixed prior of an action (policy, not prediction).
#[must_use]
pub const fn action_prior(action: Action) -> f64 {
    match action {
        Action::Answer => 0.30,
        Action::Discover => 0.25,
        Action::Retrieve => 0.15,
        Action::UseTool | Action::Clarify => 0.10,
        Action::Delegate => 0.05,
        Action::RequestApproval => 0.03,
        Action::Refuse => 0.02,
    }
}

/// The risk floor of a side-effect class code, in `[0, 1]`. An unknown code
/// is treated as the most dangerous class.
#[must_use]
pub fn side_effect_risk(code: &str) -> f64 {
    match code {
        "read_only" => 0.0,
        "local_write" => 0.4,
        "process_exec" | "external_send" => 0.6,
        "credential_or_config" => 0.8,
        _ => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priors_sum_to_one_and_the_logs_are_their_logarithms() {
        let all = [
            Action::Answer,
            Action::Retrieve,
            Action::Discover,
            Action::UseTool,
            Action::Delegate,
            Action::Clarify,
            Action::RequestApproval,
            Action::Refuse,
        ];
        let total: f64 = all.iter().map(|action| action_prior(*action)).sum();
        assert!((total - 1.0).abs() < 1e-12);
        assert!((UNMATCHED_OPTION - 0.02_f64.ln()).abs() < 1e-12);
        assert!((APPLICABILITY_OFFSET - 0.1_f64.ln()).abs() < 1e-12);
    }

    /// D-061 M-2: structural evidence lifts a tool (prior 0.10) above a
    /// direct answer (prior 0.30).
    #[test]
    fn structural_bonuses_commit_to_the_tool() {
        let tool = action_prior(Action::UseTool).ln();
        let answer = action_prior(Action::Answer).ln();
        for bonus in [URL_TO_WEB, PATH_TO_FILES, CODE_TO_SHELL] {
            assert!(tool + bonus > answer + 0.3, "{bonus}");
        }
    }

    #[test]
    fn unknown_side_effects_are_the_most_dangerous() {
        assert_eq!(side_effect_risk("read_only"), 0.0);
        assert_eq!(side_effect_risk("destructive"), 1.0);
        assert_eq!(side_effect_risk("something_new"), 1.0);
    }
}

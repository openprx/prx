//! The selection recorded with a legacy decision hop.
//!
//! The legacy selector was retired with the prediction hop (R9): a turn opens
//! prediction-free and commits its single Answer candidate. The type stays
//! for the decision-record writers and terminal replay until Part 2.

use super::branch::CandidateId;

/// The committed working candidate of a hop.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Selection {
    Commit {
        candidate_id: CandidateId,
        /// Always one: exploration was retired with the selector.
        propensity: f64,
        reason_code: Option<&'static str>,
    },
}

//! Score values recorded with a legacy decision hop.
//!
//! The scorer was retired with the prediction hop (R9). The record writers
//! keep the scorer version and the (always empty) score map until Part 2.

pub(crate) const SCORER_VERSION: &str = "scorer-v1";

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CandidateScore {
    pub semantic_utility: f64,
    pub execution_utility: f64,
}

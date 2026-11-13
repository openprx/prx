// Forwarding bridge until Part 2: candidate generation lives in
// `decision_engine::candidate_generator` (R21). Only the legacy decision-record
// modules of `causal_tree` read the candidate set through this path.
pub(crate) use crate::decision_engine::candidate_generator::CandidateSet;

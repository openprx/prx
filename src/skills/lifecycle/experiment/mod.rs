//! Owner-scoped bounded experiment records and state contracts.
//! Execution, accounting, verification and publication retain their existing owners.
mod admission;
pub use crate::runtime::h2_entry::{ControlledAdmissionPin, EntryOriginRecord};
pub use admission::ControlledAdmissionEnvelope;
mod cycle;
mod gate_review;
pub(in crate::skills::lifecycle) use gate_review::{GateReviewPhysical, GateReviewWitness, GateVerificationWindow};
mod journal;
mod operation;
mod probe;
mod state;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
mod tests;
pub use cycle::{InstructionCycleDraft, InstructionExperimentPlan, InstructionSchedule};
pub use operation::*;
pub use probe::DevelopmentProbeReceipt;
pub(in crate::skills::lifecycle) use probe::probe_run_count;
pub(crate) fn probe_within_budget(receipt: &DevelopmentProbeReceipt, suite: &super::FrozenSuite) -> bool {
    receipt.within_budget(suite)
}
mod types;
pub use journal::*;
pub use state::*;
pub use types::*;

#[derive(Debug, thiserror::Error)]
pub enum ExperimentError {
    #[error(transparent)]
    Lifecycle(#[from] super::LifecycleError),
    #[error("invalid experiment specification: {0}")]
    InvalidSpec(String),
    #[error("experiment identity conflicts with its immutable specification")]
    Conflict,
    #[error("experiment resource allowance exceeded")]
    BudgetExceeded,
    #[error("experiment deadline expired or clock moved before admission")]
    Expired,
    #[error("invalid experiment stage transition: {0}")]
    InvalidTransition(String),
    #[error("experiment evidence is incomplete or invalid: {0}")]
    InvalidEvidence(String),
}

#[cfg(test)]
pub(crate) use tests::specification as test_specification;

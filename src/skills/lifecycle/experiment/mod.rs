//! Owner-scoped bounded experiment records and state contracts.
//! Execution, accounting, verification and publication retain their existing owners.
mod journal;
mod operation;
mod state;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
mod tests;
pub use operation::*;
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

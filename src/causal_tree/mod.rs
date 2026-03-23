//! Causal Tree Engine — speculative multi-branch prediction for PRX.
//!
//! Default: disabled. Enable via `[causal_tree] enabled = true` in config.

// Re-exports are used by lib consumers but not by the bin crate.
#![allow(unused_imports)]

pub mod branch;
pub mod engine;
pub mod error;
pub mod expander;
pub mod feedback;
pub mod metrics;
pub mod policy;
pub mod rehearsal;
pub mod scorer;
pub mod selector;
pub mod state;

pub use branch::{BranchLabel, CausalBranch, CommitPolicy, CostEstimate, RehearsalLevel};
pub use engine::CausalTreeEngine;
pub use error::CausalTreeError;
pub use feedback::FeedbackWriter;
pub use metrics::CausalTreeMetrics;
pub use policy::{CausalPolicy, CausalTreeConfig};
pub use rehearsal::RehearsalEngine;
pub use scorer::BranchScorer;
pub use selector::PathSelector;
pub use state::{ArtifactRef, BudgetState, CausalState, RiskFlag, SideEffectMode, StepRecord};

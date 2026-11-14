//! Advisory intent analysis. This module never selects or authorizes execution.
//!
//! It owns the causal state, candidate generation, question compilation and
//! projection, the cost model and the
//! prediction backend stack (`prediction`, moved from `src/decision`, R21).
pub(crate) mod candidate;
pub(crate) mod candidate_generator;
pub mod causal_adapter;
// Forwarding path until Part 2: the schema lives in `config::schema::decision_engine`.
pub use crate::config::schema::decision_engine as config;
pub mod contract;
pub(crate) mod cost_model;
pub mod noop;
pub mod observe;
pub mod prediction;
pub(crate) mod prediction_port;
pub(crate) mod projection;
pub(crate) mod questions;
pub mod render;
pub(crate) mod state;
pub(crate) mod status;
pub(crate) mod turn;
pub use contract::*;
#[cfg(test)]
#[path = "turn_tests.rs"]
mod tests;

#[cfg(test)]
pub(crate) mod access_spy;

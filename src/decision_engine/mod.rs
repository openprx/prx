//! Advisory intent analysis. This module never selects or authorizes execution.
pub mod causal_adapter;
pub mod config;
pub mod contract;
pub mod noop;
pub mod observe;
pub mod render;
pub(crate) mod turn;
pub use contract::*;
#[cfg(test)]
#[path = "turn_tests.rs"]
mod tests;

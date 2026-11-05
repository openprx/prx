//! Advisory intent analysis. This module never selects or authorizes execution.
pub mod causal_adapter;
pub mod config;
pub mod contract;
pub mod noop;
pub mod observe;
pub mod render;
pub use contract::*;
#[cfg(test)]
mod tests;

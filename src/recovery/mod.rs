//! Runtime recovery helpers shared across the agent execution paths.
//!
//! The submodules here provide a single, canonical implementation of recovery
//! behaviours that were previously duplicated (and subtly divergent) across the
//! three LLM-call paths in [`crate::agent::loop_`] and [`crate::agent::agent`].
//! Centralizing them guarantees the streaming, tool-loop, and direct-turn paths
//! react identically to the same failure class (FIX-P1-12).

pub mod overflow;

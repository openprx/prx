//! The one place an autonomy level is decided.
//!
//! Every reader of a turn's, a call's or a scheduled run's level asks these
//! pure functions. Nothing here raises a child above its parent (that `min`
//! stays in the execution service and the child-scope derive).
use crate::security::AutonomyLevel;

/// A named agent's or Main's own turn: the agent's configured level, or
/// Main's when the agent sets none. No `min` with Main: a named agent
/// configured by the owner is a top-level agent with its own level.
#[must_use]
pub fn agent_level(own: Option<AutonomyLevel>, main: AutonomyLevel) -> AutonomyLevel {
    own.unwrap_or(main)
}

/// The level a tool-internal check reads: the call's level when the
/// execution service set one, else the level of the policy the tool holds.
#[must_use]
pub fn call_level(call: Option<AutonomyLevel>, held: AutonomyLevel) -> AutonomyLevel {
    call.unwrap_or(held)
}

//! Runtime recovery classification used by the agent turn engine.
//!
//! Entrypoints and provider adapters may classify a failure here, but the turn
//! engine owns every recovery action so no caller starts a second compaction or
//! replay loop.

pub mod overflow;

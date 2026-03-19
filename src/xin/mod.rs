//! `xin` (心) — the autonomous task heartbeat engine.
//!
//! A configurable timer-driven engine that manages system-level autonomous tasks:
//! health checks, memory evolution, fitness reports, and user-defined work units.
//! Tasks follow a lifecycle (pending → running → completed/failed/stale) and are
//! persisted in SQLite for crash recovery.

pub(crate) mod builtin;
pub mod config;
pub(crate) mod runner;
pub(crate) mod store;
pub(crate) mod types;

pub use config::XinConfig;

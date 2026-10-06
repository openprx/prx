//! Causal decision domain and bounded historical v1 import.
//!
//! Runtime entrypoints use the source-bound `TurnDecisionService` and its
//! pinned catalog, bounded prediction/rehearsal, directives and settlement.
//! The legacy speculative engine, snapshot builder and feedback writers are
//! removed. `v1_compat` retains the bounded decoder/importer for historical
//! feedback. Legacy runtime configuration views have been removed; historical
//! file keys migrate to canonical fields before deserialization.
//!
//! Configuration snapshots are not runtime evidence. Acceptance requires the
//! owning entrypoint's actual decision, execution and terminal records.

// Decision state and candidates are independent of the optional model router.
pub mod admin;
pub(crate) mod approval_collection;
pub mod branch;
pub(crate) mod candidate_generator;
pub mod config;
pub(crate) mod decision_engine;
pub(crate) mod decision_rehearsal;
pub(crate) mod decision_score;
pub(crate) mod decision_select;
pub(crate) mod directive;
pub mod evaluate;
pub(crate) mod exploration;
pub(crate) mod gates;
pub mod ids;
pub(crate) mod outcome;
pub mod projection;
pub(crate) mod questions;
pub(crate) mod report_catalog;
pub mod state;
pub mod store;
pub mod store_schema;
pub mod v1_compat;

pub use config::CausalTreeConfig;

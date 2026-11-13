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
// forwarding bridge until Part 2
pub(crate) mod candidate_generator;
// Forwarding path until Part 2: the schema lives in `config::schema::causal_tree`.
pub use crate::config::schema::causal_tree as config;
pub(crate) mod decision_engine;
pub(crate) mod decision_select;
pub(crate) mod directive;
pub mod evaluate;
pub(crate) mod gates;
pub(crate) mod hard_gates_probe;
// Forwarding path until Part 2: the id lives in `security::decision_id`.
pub use crate::security::decision_id as ids;
pub(crate) mod outcome;
pub(crate) mod refusal;
pub(crate) mod report_catalog;
// Forwarding paths until Part 2: the journal lives in `runtime::decision_journal`.
pub(crate) use crate::runtime::decision_journal::store;
pub(crate) use crate::runtime::decision_journal::store_preparation;
pub mod v1_compat;

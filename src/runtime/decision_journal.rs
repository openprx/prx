//! The legacy decision journal: the `decisions.db` store, its schema and its
//! blocking preparation (moved from `causal_tree`, R16). Recovery, terminal
//! replay, child issuance and the approval ledger read it; nothing here
//! imports the decision engine (R18).

pub(crate) mod store;
pub(crate) mod store_preparation;
pub mod store_schema;

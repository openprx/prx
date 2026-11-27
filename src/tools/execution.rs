//! Canonical tool execution contract.
//!
//! This module owns the fixed application pipeline for native tools and MCP
//! aliases. Existing [`Tool`] implementations remain adapter-owned during the
//! migration; callers submit a typed command and runtime context instead of
//! resolving and invoking adapters directly.

pub(crate) mod admission;
mod approval_record;
mod approval_route;
mod effective_output;
mod refusal_record;
pub use refusal_record::ToolCallRefusalReason;
pub(crate) use refusal_record::ToolRefusalNotice;
pub(crate) use refusal_record::record_rf_call_refusal;

include!("execution/section_1.rs");
include!("execution/section_2.rs");
include!("execution/section_3.rs");

#[cfg(test)]
pub(crate) mod tests;

/// T18 — local-operator origin marker (`_prx_scope_trusted`).
///
/// These tests pin the one property the destructive `gateway` guard rests on:
/// the marker is authored by the runtime from the turn's origin, and a caller
/// (model, IM user, HTTP body) can neither set it nor clear it.
#[cfg(test)]
mod local_operator_scope_tests;

#[cfg(test)]
mod shared_binding_state_tests;

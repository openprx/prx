//! Regression guards for architecture boundaries that are not expressed by Rust's type system.
//!
//! These allowlists intentionally describe the current tree. A new entry is a review event:
//! route the behavior through the owning repository/adapter/event fabric, or update the
//! allowlist with an explicit architecture decision.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::unwrap_used)]

mod causal_domain;
mod causal_policy;
mod compaction;
mod config;
mod decision_backend;
mod decision_seals;
mod helpers;
mod ingress;
mod labels;
mod loop_contracts;
mod loop_prompt;
mod loop_requests;
mod memory;
mod process;
mod providers;
mod scanner_meta;
mod scanner_probes;
mod strip_tests;
mod structure;
mod tool_scanner;
mod tool_tables;
mod tool_tests;

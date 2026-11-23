//! Axum-based HTTP gateway with proper HTTP/1.1 compliance and body limits.
//!
//! This module replaces the raw TCP implementation with axum for:
//! - Proper HTTP/1.1 parsing and compliance
//! - Content-Length validation (handled by hyper)
//! - Request body size limits (64KB max)
//! - Header sanitization (handled by axum/hyper)
//!
//! There is deliberately **no request timeout**. A gateway request can start an
//! agent turn, and an agent turn has no legitimate upper bound — research runs,
//! builds, and multi-hour tool chains are ordinary work here, not pathology. A
//! wall-clock cap on the outer request could only ever cut such a turn off
//! mid-flight, and it did: the former 60s `TimeoutLayer` sat *below* every inner
//! bound it was supposed to contain (single LLM call, compaction, sub-agent), so
//! it fired first and made failover and retry unreachable dead configuration.
//!
//! Work that can run long is therefore submitted as a detached job (see
//! [`jobs`]) rather than executed inside the request future, which decouples its
//! lifetime from the connection: a closed tab or a proxy hang-up no longer
//! destroys a turn halfway through its side effects. Visibility and termination
//! move to the operator instead of the clock — `prx tasks list` /
//! `prx tasks kill`, `GET /api/runtime/tasks`, and `GET /api/jobs`.

#![allow(clippy::print_stdout, clippy::print_stderr)]

mod agent_lifecycle;
mod api;
mod compat;
mod h2_control;
mod jobs;
mod live_frames;
mod mailbox;
#[cfg(test)]
mod platform_test_transport;
mod run_approvals;
#[cfg(test)]
mod tests;
mod turn_failure;
mod turn_readiness;
pub(crate) mod turn_source;
mod ui;
mod webhook_dedup;

include!("implementation/section_1.rs");
include!("implementation/section_2.rs");
include!("implementation/section_3.rs");

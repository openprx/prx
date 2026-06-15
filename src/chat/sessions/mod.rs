//! Chat-only multi-session runtime (v1a).
//!
//! This module provides the chat-side glue for managing background sessions
//! (agents now, shells later) **inside `prx chat`**. It does not introduce a
//! second registry or supervisor: the live run state continues to live in
//! `sessions_spawn`'s `Arc<RwLock<Vec<SubAgentRun>>>`. Here we only add:
//!
//! - [`id::SessionId`] — stable handle over the run UUID.
//! - [`model::ManagedSessionView`] / [`model::project_run`] — UI projection.
//! - [`command::SessionCommand`] / [`command::parse_session_command`] — parsing.
//! - [`runtime::ChatSessionsHandle`] — thin handle over the shared registry for
//!   `/sessions` and `/kill`.
//!
//! See `task/prx/chat-background-runtime-v1-execution-plan.md` (v1a) for scope.

pub mod command;
pub mod id;
pub mod model;
pub mod runtime;

pub use command::{SessionCommand, parse_session_command};
pub use runtime::ChatSessionsHandle;

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
//!   `/sessions`, `/kill`, `/steer`, and `/attach`.
//! - [`runtime::status_summary`] — persistent status-line summary builder.
//! - [`event::SessionEvent`] / [`event::SessionEventSink`] / [`event::SessionRing`]
//!   — the v1.1a event bridge: decoupled delta/tool streaming from background
//!   agents into per-session ring buffers for live read-only attach.
//! - [`focus::FocusTarget`] / [`focus::SwitcherState`] / [`focus::resolve_esc`]
//!   — the v1.1b input-routing target, Ctrl+G switcher overlay state, and the
//!   pure Esc decision function.
//!
//! See `task/prx/chat-background-runtime-v1-execution-plan.md` (v1a/v1b) for
//! scope.

pub mod command;
pub mod event;
pub mod focus;
pub mod id;
pub mod model;
pub mod runtime;
pub mod shell;

pub use command::{SessionCommand, parse_session_command};
pub use event::{SessionEvent, SessionEventSink, SessionRing};
pub use focus::{FocusTarget, SwitcherEntry, SwitcherState};
// `FinishedSession` / `TailLine` are returned by `ChatSessionsHandle` methods
// and reachable as `runtime::{FinishedSession, TailLine}`; not re-exported at
// this level until a caller needs to name them (avoids an unused-import warning).
pub use runtime::{ChatSessionsHandle, status_summary};

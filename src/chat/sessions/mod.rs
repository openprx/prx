//! Chat-only sessions runtime: the child TUI registry for `prx chat`.
//!
//! In the product model, every secondary work surface under the main chat TUI is
//! a **session**: a child TUI entry with a stable display id, kind, origin,
//! status, output ring, and input-routing target. `agent`, `shell`, and `pty`
//! are kinds of sessions, not separate architecture names.
//!
//! This module provides the chat-side glue for managing those child TUI
//! sessions **inside `prx chat`**. It does not introduce a second registry or
//! supervisor: the live run state continues to live in `sessions_spawn`'s
//! `Arc<RwLock<Vec<SubAgentRun>>>` plus the shell / PTY registries owned by the
//! chat main loop. Here we only add:
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

pub mod approval;
pub mod command;
pub mod event;
pub mod focus;
pub mod id;
pub mod model;
#[cfg(feature = "terminal-tui")]
pub mod pty;
pub mod runtime;
pub mod shell;

pub use approval::{PendingApprovals, build_resolver_factory};
pub use command::{SessionCommand, parse_session_command};
pub use event::{SessionEvent, SessionEventSink, SessionRing};
pub use focus::{
    ActiveSessionView, FocusTarget, PendingToolApprovalView, SessionDirection, SwitcherEntry, SwitcherState,
};
pub use model::PersistedSessionSummary;
// `FinishedSession` / `TailLine` are returned by `ChatSessionsHandle` methods
// and reachable as `runtime::{FinishedSession, TailLine}`; not re-exported at
// this level until a caller needs to name them (avoids an unused-import warning).
pub use runtime::{ChatSessionsHandle, status_summary};

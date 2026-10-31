//! `prx chat` entry point — rich terminal interactive chat.
//!
//! Wires up the full agent pipeline (memory, tools, providers, security, hooks,
//! observability) and uses [`TerminalChannel`] for streaming I/O through the
//! event-driven UI Actor.
//!
//! ## S4-A render source switch (completed 2026-05-16)
//!
//! In Pure mode the ratatui render source switched from `chat_mirror: Arc<Mutex<TuiState>>` to
//! `tokio::sync::watch::Receiver<Arc<state::UiSnapshot>>`. After the reducer returns
//! `ui_dirty=true` the dispatcher builds a new [`state::UiSnapshot`] and pushes it to the watch with
//! send_if_modified; `run_tui_unified_loop` borrows the current snapshot from the receiver through
//! [`RenderSource::Snapshot`], bypassing the chat_mirror lock.
//!
//! Off/Both/Redux modes keep the existing `chat_mirror` path ([`RenderSource::Mirror`]) so the
//! staged rollout stays controllable.
//!
//! S4-A completed commits: 327395d / 84ec8f1 / 0bb93bb / 55a2421 / 8d53140 /
//! ae3a9af / ae47ddd + this commit (Commit 7 docs). S4-B plan:
//! delete the chat_mirror field and every mirror-path call; see appendix D of the task document
//! (`/opt/worker/task/prx/prx-remaining-plan-2026-05-15.md`).
//!
//! ## Presentation stack is by-design separate from the runtime mode abstraction (FIX-P1-27)
//!
//! Channels, the gateway, the session worker, and the agent CLI converge on a
//! shared runtime ingress contract (see [`crate::runtime::envelope`]): one
//! `RuntimeEnvelope` derives the session identity, memory principal, message
//! scope, and task lineage for every mode. Layer D deliberately does **not**
//! fold `chat`'s streaming presentation stack — the TUI/Redux/Reedline pipeline
//! (Actor + reducer + `UiSnapshot`/`chat_mirror` render sources documented in
//! §S4-A above) — into that abstraction, for two reasons:
//!
//! 1. **Orthogonality.** Presentation (how turns are rendered/streamed to a
//!    terminal) is orthogonal to the runtime core (how turns are routed,
//!    remembered, and executed). The TUI loop owns terminal lifecycle, key
//!    handling, frame diffing, and draft rendering — concerns no other ingress
//!    mode has. Channels and the gateway emit plain transport payloads; only
//!    `chat` drives an interactive on-screen UI.
//! 2. **Single-implementer trait smell.** A unified "mode runner" trait over the
//!    presentation layer would have exactly one implementer (this module), so it
//!    would be a dead abstraction: extra indirection with no second consumer and
//!    no polymorphism to justify it. (There is intentionally no `ModeRunner`
//!    trait in the codebase for this reason.)
//!
//! What `chat` **does** reuse is the unified agent-loop core — tools, memory,
//! routing, security, hooks — via [`crate::agent::turn_engine::run_turn`],
//! decision-owned retrieval, and the
//! same `RuntimeEnvelope`-derived [`MemoryPrincipal`](crate::memory::MemoryPrincipal)/`MessageEventScope`
//! (see [`chat_runtime_envelope`]). Only the presentation/streaming stack stays
//! independent; the runtime semantics are shared with every other mode.
// Chat module: println!/eprintln! are intentional user-facing output (banners, status, errors).
#![allow(clippy::print_stdout, clippy::print_stderr)]

pub mod action;
pub mod commands;
pub(crate) mod decision_source;
pub mod diff_apply;
pub mod dispatcher;
pub mod error;
pub mod history_commit;
pub mod managed_session;
pub mod sanitize;
pub mod scheduled_input;
pub mod session;
pub mod sessions;
pub mod slash_types;
pub mod state;
pub mod terminal_proto;
pub mod turn_scheduler;
pub mod turn_worker;

#[cfg(feature = "terminal-tui")]
pub mod renderer;
#[cfg(feature = "terminal-tui")]
pub mod tui;

mod active_turn;
mod bootstrap;
mod chat_events;
mod compaction;
mod copy_select;
#[cfg(feature = "terminal-tui")]
mod external_editor;
mod file_mention;
mod input_queue;
mod model_commands;
mod output;
mod provider_turns;
mod pty_attach;
mod run;
mod session_glue;
mod session_store;
mod session_switch;
mod terminal;
mod tui_loop;
mod views;

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use self::bootstrap::chat_provider_chain;
use self::chat_events::{chat_runtime_envelope, chat_session_id_from_key};
#[cfg(test)]
use self::chat_events::{
    finalize_chat_non_success_turn, finalize_chat_success_turn, record_chat_user_message_event_in_generation,
};
pub(crate) use self::compaction::format_context_degraded_notice;
pub use self::compaction::print_embedding_upgrade_notice_to_stderr;
use self::compaction::{estimate_chat_history_tokens, format_compact_feedback};
use self::input_queue::is_local_operator_input;
pub(crate) use self::output::print_fallback_chat_output;
pub use self::run::{run, run_controlled};
pub use self::terminal::TerminalGuard;
pub use self::terminal::TracingChatGuard;
pub use self::terminal::setup_chat_tracing_to_file;

/// Window (ms) for double Ctrl+C to trigger exit.
const DOUBLE_CTRLC_WINDOW_MS: u64 = 500;

/// Capacity for the user-input mpsc channel.
const INPUT_CHANNEL_CAPACITY: usize = 16;
const CHAT_CONTROL_CHANNEL_CAPACITY: usize = 8;
const SYNTHETIC_UI_COMMAND_SENDER: &str = "prx-ui";
const SESSION_LOGS_MAX_LINES: usize = 200;

/// Capacity for the streaming delta (partial response) mpsc channel.
const DELTA_CHANNEL_CAPACITY: usize = 64;

/// Capacity for the tool-call notification mpsc channel (visual feedback).
const TOOL_EVENT_CHANNEL_CAPACITY: usize = 32;

// ─── S4-A Commit 4: RenderSource enum dual paths ─────────────────────────────

//! Lifecycle hooks: the event types, the workspace `hooks.json`, command
//! launch, the queued delivery and the manager that owns them.

mod child;
mod config;
pub(crate) mod context;
mod envelope;
mod event;
mod hook_core;
mod identity;
mod launch;
mod manager;
mod schedule;
mod tools;
mod webhook;

#[cfg(test)]
mod tests;

pub use config::{
    HookChange, HookDiagnostics, WORKSPACE_HOOKS_OPT_IN, blocked_hooks_note, workspace_hook_counts,
};
pub use event::{HookEvent, payload_error};
pub use identity::{HookIdentity, current as current_identity, notify, scope as identity_scope, scope_optional as identity_scope_optional, set_round};
pub use identity::current_manager;
pub use child::ChildRun;
pub use manager::HookManager;
pub use webhook::display_url as webhook_display_url;
pub use schedule::ScheduleRun;
#[cfg(test)]
use launch::truncate_for_log;

const HOOKS_JSON_FILE: &str = "hooks.json";
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const MAX_HOOKS_FILE_BYTES: u64 = 256 * 1024;
const MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: u64 = 16 * 1024;
const MAX_STDERR_CHARS: usize = 400;
const MAX_ACTIONS: usize = 256;
const MAX_ARGS_PER_ACTION: usize = 128;
const MAX_ENV_PER_ACTION: usize = 128;

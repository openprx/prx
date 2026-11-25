//! Lifecycle hooks: the event types, the workspace `hooks.json`, command
//! launch, the queued delivery and the manager that owns them.

mod config;
mod hook_core;
mod event;
mod launch;
mod manager;
mod tools;

#[cfg(test)]
mod tests;

pub use config::{
    HookChange, HookDiagnostics, WORKSPACE_HOOKS_OPT_IN, blocked_hooks_note, workspace_hook_actions,
};
pub use event::{HookEvent, payload_error};
pub use manager::HookManager;
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

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

#[cfg(test)]
mod tests;

use crate::agent::compaction::{apply_compaction_patch_exact, build_configurable_compaction_patch_with_source_history};
use crate::agent::context_budget::measure_history_tokens;
use crate::agent::context_recall::increment_recalled_useful_counts;
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::runtime_prompt::build_runtime_system_prompt;
use crate::agent::turn_contracts::{ScopeContext, ToolCallNotification, is_tool_loop_cancelled};
use crate::agent::turn_engine::run_turn;
use crate::approval::ApprovalManager;
use crate::channels::traits::extract_outgoing_media;
use crate::channels::{Channel, SendMessage, TerminalChannel, sanitize_channel_response};
use crate::chat::terminal_proto::DraftVersionCounter;
use crate::config::Config;
use crate::hooks::{HookEvent, payload_error};
use crate::llm::route_decision::{
    ProviderExecutionOutcome, RouteDecision, record_route_decision_event as record_raw_route_decision_event,
    route_event_scope,
};
#[cfg(test)]
use crate::memory::MemoryPrincipal;
use crate::memory::{CompactionRunInput, MemoryStoreMetadata};
use crate::memory::{Memory, MemoryCategory, MemoryFabric, MemoryVisibility, MessageEventScope};
use crate::observability::ObserverEvent;
use crate::providers::{self, ChatMessage, Provider};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::tools::Tool;
use crate::util::truncate_with_ellipsis;
use anyhow::{Context, Result};
#[cfg(feature = "terminal-tui")]
use std::collections::VecDeque;
use std::io::{IsTerminal as _, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
#[cfg(feature = "terminal-tui")]
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Window (ms) for double Ctrl+C to trigger exit.
const DOUBLE_CTRLC_WINDOW_MS: u64 = 500;

/// Keep last N non-system messages during history compaction.
const COMPACT_KEEP_MESSAGES: usize = 8;

/// Per-message character limit during compaction.
const COMPACT_CONTENT_CHARS: usize = 320;

/// Total character budget for compacted history (excluding system prompt).
const COMPACT_TOTAL_CHARS: usize = 2400;

/// Capacity for the user-input mpsc channel.
const INPUT_CHANNEL_CAPACITY: usize = 16;
const CHAT_CONTROL_CHANNEL_CAPACITY: usize = 8;
/// Queue depth for provider-turn lifecycle events (`HandleAttached`, `Started`,
/// `Exited`) travelling from the effect executor back to the chat loop.
///
/// Sized as a backpressure valve, not a concurrency limit. Each turn emits
/// exactly three of these small, `Copy`-sized records, so this depth absorbs
/// roughly 1300 turns' worth of lifecycle traffic — far more than the chat loop
/// can fall behind by in practice, since it drains the queue at several points
/// in every iteration. If it ever does fill, the emitting turn parks on
/// `send().await` instead of the queue growing without bound; slowing turn
/// startup is the correct outcome, because the worker registry driving aborts
/// and completion routing must not fall arbitrarily far behind reality.
pub(crate) const PROVIDER_TURN_LIFECYCLE_CHANNEL_CAPACITY: usize = 4096;
const SYNTHETIC_UI_COMMAND_SENDER: &str = "prx-ui";
const SESSION_LOGS_MAX_LINES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum InputQueuePriority {
    Normal,
    Priority,
    Control,
}

struct QueuedInputMessage {
    priority: InputQueuePriority,
    turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    msg: crate::channels::traits::ChannelMessage,
}

struct DequeuedInputMessage {
    priority: InputQueuePriority,
    turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    msg: crate::channels::traits::ChannelMessage,
}

#[derive(Debug, Clone)]
struct ProviderTurnCompletionEvent {
    task_id: crate::chat::turn_scheduler::TurnTaskId,
    // Only the TUI completion waiter fills these and only the TUI finalizer
    // reads them.
    #[cfg(feature = "terminal-tui")]
    outcome: Option<dispatcher::TurnOutcomeKind>,
    #[cfg(feature = "terminal-tui")]
    usage: crate::llm::route_decision::TokenUsage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProviderTurnCompletionContext {
    history_len_before_assistant: usize,
}

#[cfg(feature = "terminal-tui")]
struct PerTurnContext {
    cancellation: CancellationToken,
    turn_binding: Option<Arc<decision_source::ChatTurnBinding>>,
    /// The disposition captured before provider execution can seal the
    /// decision context. `Some` identifies the fixed-terminal path; `None`
    /// identifies a provider-backed completion.
    fixed_reply: Option<String>,
    task_id: crate::chat::turn_scheduler::TurnTaskId,
    draft_id: String,
    delta_tx: Option<mpsc::Sender<String>>,
    tool_event_tx: Option<mpsc::Sender<ToolCallNotification>>,
    draft_updater: Option<tokio::task::JoinHandle<()>>,
    tool_event_forwarder: Option<tokio::task::JoinHandle<()>>,
    user_input: String,
    turn_run_id: String,
    route_scope: MessageEventScope,
    route_decision: RouteDecision,
    provider_started_at: chrono::DateTime<chrono::Utc>,
    provider_name: String,
    model_name: String,
    history_len_before_user_turn: usize,
    history_user_message: ChatMessage,
    /// Set when this turn was started by daemon-assigned work, and carried all
    /// the way to the commit because that is where the reply exists.
    ///
    /// A visible turn is *launched* by the main loop and finalized somewhere
    /// else entirely, so a guard left behind in the loop body would report the
    /// moment the turn started — an answer of "no reply" to work that was about
    /// to produce one. Moving it into the context ties the report to the turn
    /// rather than to the iteration that started it.
    assignment: Option<crate::chat::sessions::assignment::AssignmentTurnGuard>,
}

#[cfg(feature = "terminal-tui")]
struct PendingOrderedProviderTurnCommit {
    context: PerTurnContext,
    terminal_plan: ProviderTurnTerminalPlan,
}

#[derive(Debug)]
#[cfg(feature = "terminal-tui")]
struct ResolvedProviderTurnCompletion {
    outcome: Option<dispatcher::TurnOutcomeKind>,
    usage: crate::llm::route_decision::TokenUsage,
}

#[derive(Debug, Clone)]
#[cfg(feature = "terminal-tui")]
enum ProviderTurnTerminalPlan {
    Completed {
        final_text: String,
        reasoning: String,
        recorded_response: String,
        empty_response: bool,
        usage: crate::llm::route_decision::TokenUsage,
        history_commit_len: usize,
        final_text_chars: usize,
        recorded_response_chars: usize,
        summary: &'static str,
    },
    Failed {
        err: String,
        history_commit_len: usize,
        summary: String,
    },
    Cancelled {
        summary: &'static str,
    },
}

#[cfg(feature = "terminal-tui")]
#[derive(Debug, Clone)]
struct ProviderTurnFinalizerEvent {
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    plan: ProviderTurnTerminalPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "terminal-tui")]
struct ProviderTurnFinalizerResult {
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    terminal_status: &'static str,
    finalized: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ProviderTurnVisibleAdmission {
    active_workers: usize,
    foreground_active: usize,
    detached_active: usize,
    effective_max_visible_turns: usize,
    can_start_visible: bool,
}

#[cfg(feature = "terminal-tui")]
enum ProviderTurnTerminalGate<'a> {
    Completed {
        history_commit_len: usize,
        final_text_chars: usize,
        recorded_response_chars: usize,
        usage: &'a crate::llm::route_decision::TokenUsage,
        summary: &'static str,
    },
    Failed {
        history_commit_len: usize,
        summary: String,
    },
    Cancelled {
        summary: &'static str,
    },
}

/// Capacity for the streaming delta (partial response) mpsc channel.
const DELTA_CHANNEL_CAPACITY: usize = 64;

/// Capacity for the tool-call notification mpsc channel (visual feedback).
const TOOL_EVENT_CHANNEL_CAPACITY: usize = 32;

const FILE_MENTION_MAX_FILES: usize = 5;
const FILE_MENTION_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileMention {
    token: String,
    path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileMentionEnrichment {
    prompt: String,
    visible_note: Option<String>,
}

fn extract_file_mentions(input: &str) -> Vec<FileMention> {
    let mut mentions = Vec::new();
    let mut iter = input.char_indices().peekable();

    while let Some((idx, ch)) = iter.next() {
        if ch != '@' {
            continue;
        }

        if idx > 0
            && input[..idx]
                .chars()
                .next_back()
                .is_some_and(is_file_mention_email_prefix)
        {
            continue;
        }

        let Some(&(start, first)) = iter.peek() else {
            continue;
        };
        if first.is_whitespace() || first == '@' || first == '"' || first == '\'' {
            continue;
        }

        let mut end = input.len();
        let mut saw_char = false;
        for (next_idx, next_ch) in input[start..].char_indices() {
            if next_ch.is_whitespace() {
                end = start + next_idx;
                break;
            }
            saw_char = true;
        }
        if !saw_char {
            continue;
        }

        let raw_path = &input[start..end];
        let path = raw_path.trim_end_matches(is_file_mention_trailing_punctuation);
        if path.is_empty() {
            continue;
        }

        mentions.push(FileMention {
            token: format!("@{path}"),
            path: path.to_string(),
        });
    }

    mentions
}

const fn is_file_mention_email_prefix(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '+')
}

const fn is_file_mention_trailing_punctuation(ch: char) -> bool {
    matches!(ch, ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}')
}

async fn enrich_file_mentions_for_prompt(user_input: &str, tools_registry: &[Box<dyn Tool>]) -> FileMentionEnrichment {
    let mentions = extract_file_mentions(user_input);
    if mentions.is_empty() {
        return FileMentionEnrichment {
            prompt: user_input.to_string(),
            visible_note: None,
        };
    }

    let mut sections = Vec::new();
    let mut visible_notes = Vec::new();
    let file_read = tools_registry.iter().find(|tool| tool.supports_name("file_read"));

    for mention in mentions.iter().take(FILE_MENTION_MAX_FILES) {
        match file_read {
            Some(tool) => {
                let args = serde_json::json!({
                    "path": mention.path.as_str(),
                    "max_bytes": FILE_MENTION_MAX_BYTES,
                });
                match tool.execute_named("file_read", args).await {
                    Ok(result) if result.success => {
                        let (content, truncated) = truncate_utf8_to_byte_cap(&result.output, FILE_MENTION_MAX_BYTES);
                        let mut section = format!("### {}\nPath: {}\n\n{}", mention.token, mention.path, content);
                        if truncated {
                            section.push_str("\n[content truncated: 64 KiB limit]");
                            visible_notes.push(format!("{}: content truncated to 64 KiB", mention.token));
                        }
                        sections.push(section);
                    }
                    Ok(result) => {
                        let note = file_mention_failure_note(&mention.token, result.error.as_deref());
                        sections.push(format!("### {}\nPath: {}\n\n{note}", mention.token, mention.path));
                        visible_notes.push(note);
                    }
                    Err(e) => {
                        let note = format!("{}: unavailable ({e})", mention.token);
                        sections.push(format!("### {}\nPath: {}\n\n{note}", mention.token, mention.path));
                        visible_notes.push(note);
                    }
                }
            }
            None => {
                let note = format!("{}: unavailable (file_read tool is not registered)", mention.token);
                sections.push(format!("### {}\nPath: {}\n\n{note}", mention.token, mention.path));
                visible_notes.push(note);
            }
        }
    }

    if mentions.len() > FILE_MENTION_MAX_FILES {
        let skipped = mentions.len().saturating_sub(FILE_MENTION_MAX_FILES);
        let note = format!("{skipped} file mention(s) skipped: maximum {FILE_MENTION_MAX_FILES} per message");
        sections.push(note.clone());
        visible_notes.push(note);
    }

    let prompt = if sections.is_empty() {
        user_input.to_string()
    } else {
        format!(
            "{user_input}\n\n[Attached file context from @path mentions]\n{}\n[End attached file context]",
            sections.join("\n\n")
        )
    };

    let visible_note = if visible_notes.is_empty() {
        None
    } else {
        Some(format!("File mention note: {}", visible_notes.join("; ")))
    };

    FileMentionEnrichment { prompt, visible_note }
}

fn file_mention_failure_note(token: &str, error: Option<&str>) -> String {
    let Some(error) = error else {
        return format!("{token}: unavailable");
    };

    if error.contains("not allowed")
        || error.contains("escapes workspace")
        || error.contains("Access denied")
        || error.contains("security policy")
    {
        format!("{token}: unavailable (blocked by policy)")
    } else if error.contains("Failed to resolve")
        || error.contains("No such file")
        || error.contains("not found")
        || error.contains("No such file or directory")
    {
        format!("{token}: unavailable (missing or inaccessible)")
    } else if error.contains("Is a directory") || error.contains("directory") {
        format!("{token}: unavailable (not a file)")
    } else if error.contains("File too large") {
        format!("{token}: unavailable (file too large)")
    } else {
        format!("{token}: unavailable")
    }
}

fn truncate_utf8_to_byte_cap(input: &str, max_bytes: usize) -> (String, bool) {
    if input.len() <= max_bytes {
        return (input.to_string(), false);
    }

    let mut end = 0usize;
    for (idx, ch) in input.char_indices() {
        let next = idx.saturating_add(ch.len_utf8());
        if next > max_bytes {
            break;
        }
        end = next;
    }

    (input[..end].to_string(), true)
}

const COPY_OSC52_MAX_BYTES: usize = 74 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CopySelection {
    content: String,
    ordinal: usize,
    truncated: bool,
}

fn is_copy_command(input: &str) -> bool {
    input == "/copy" || input.starts_with("/copy ")
}

fn select_copy_content(session: &session::ChatSession, input: &str) -> Result<CopySelection, String> {
    let raw = input.strip_prefix("/copy").unwrap_or_default().trim();
    let ordinal = if raw.is_empty() || raw.eq_ignore_ascii_case("latest") {
        1
    } else {
        raw.parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| "Usage: /copy [latest|N]".to_string())?
    };
    let Some(turn) = session
        .turns
        .iter()
        .rev()
        .filter(|turn| turn.role == "assistant")
        .nth(ordinal - 1)
    else {
        return Err("No assistant response to copy.".to_string());
    };
    let (content, truncated) = truncate_utf8_to_byte_cap(&turn.content, COPY_OSC52_MAX_BYTES);
    Ok(CopySelection {
        content,
        ordinal,
        truncated,
    })
}

fn copy_success_message(selection: &CopySelection) -> String {
    let suffix = if selection.truncated {
        " (truncated to 74 KiB for OSC 52)"
    } else {
        ""
    };
    if selection.ordinal == 1 {
        format!("Copied latest assistant response to clipboard{suffix}.")
    } else {
        format!(
            "Copied assistant response #{} from the end to clipboard{suffix}.",
            selection.ordinal
        )
    }
}

#[cfg(feature = "terminal-tui")]
fn mouse_capture_disabled_by_env(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

#[cfg(feature = "terminal-tui")]
fn mouse_capture_enabled_by_env(disable_value: Option<&str>) -> bool {
    if mouse_capture_disabled_by_env(disable_value) {
        return false;
    }
    true
}

/// Compact conversation history in-place to fit within context window limits.
///
/// Preserves the system prompt (index 0), keeps the last [`COMPACT_KEEP_MESSAGES`]
/// non-system messages, truncates each to [`COMPACT_CONTENT_CHARS`], and enforces
/// a total character budget of [`COMPACT_TOTAL_CHARS`].
fn compact_chat_history(history: &mut Vec<ChatMessage>) {
    if history.len() <= 1 {
        return;
    }

    // Separate system prompt from conversation turns
    let has_system = history.first().map(|m| m.role == "system").unwrap_or(false);
    let start = if has_system { 1 } else { 0 };

    // Keep only the last COMPACT_KEEP_MESSAGES conversation turns
    let turn_count = history.len() - start;
    if turn_count > COMPACT_KEEP_MESSAGES {
        let drain_end = start + turn_count - COMPACT_KEEP_MESSAGES;
        history.drain(start..drain_end);
    }

    // Truncate individual messages
    for msg in history.iter_mut().skip(start) {
        if msg.content.chars().count() > COMPACT_CONTENT_CHARS {
            msg.content = truncate_with_ellipsis(&msg.content, COMPACT_CONTENT_CHARS);
        }
    }

    // Enforce total character budget (drop oldest turns first)
    while history
        .iter()
        .skip(start)
        .map(|m| m.content.chars().count())
        .sum::<usize>()
        > COMPACT_TOTAL_CHARS
        && history.len() > start + 1
    {
        history.remove(start);
    }
}

fn estimate_chat_history_tokens(history: &[ChatMessage]) -> usize {
    measure_history_tokens(history)
}

fn format_compact_token_count(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

fn format_compact_feedback(
    turns_before: usize,
    turns_after: usize,
    tokens_before: usize,
    tokens_after: usize,
    model_window_tokens: usize,
) -> String {
    let reclaimed = tokens_before.saturating_sub(tokens_after);
    let reclaim_pct = if tokens_before == 0 {
        0
    } else {
        reclaimed.saturating_mul(100).saturating_div(tokens_before).min(100)
    };
    let window = format_compact_token_count(model_window_tokens);
    if turns_before == turns_after && tokens_before == tokens_after {
        format!(
            "Context already compact: {turns_after} turns / ~{tokens_after} tokens / {window} window (nothing to drop)."
        )
    } else {
        format!(
            "Compacted context: {turns_before} -> {turns_after} turns, ~{tokens_before} -> ~{tokens_after} tokens / {window} window; reclaimed ~{reclaimed} tokens ({reclaim_pct}%)."
        )
    }
}

/// One plain line saying that local vector recall is on because the shipped
/// default changed under this installation, not because anyone asked for it.
///
/// Collapsing whitespace guarantees a single line: the notice is rendered both
/// into the TUI transcript, where a newline would break the message frame, and
/// onto stderr, where it has to stay one grep-able line.
pub(crate) fn format_embedding_upgrade_notice(notice: &str) -> String {
    format!("note > {}", notice.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// The line `prx chat` shows for an inherited embedding default, or `None`
/// once the operator has written the key either way.
pub(crate) fn chat_embedding_upgrade_notice(config: &Config) -> Option<String> {
    config
        .embedding_default_upgrade_notice()
        .map(format_embedding_upgrade_notice)
}

/// Print the inherited-default notice where a `prx chat` user can actually see
/// it, before the TUI takes the screen.
///
/// `prx chat` redirects *tracing* into `chat.log`, so the warning `dispatch`
/// logs for every long-running runtime is the one thing chat users never read.
/// The process stderr is not redirected, so this reaches the terminal.
// stderr is the only channel this has: the log goes to a file and the TUI has
// not started yet.
#[allow(clippy::print_stderr)]
pub fn print_embedding_upgrade_notice_to_stderr(config: &Config) {
    if let Some(line) = chat_embedding_upgrade_notice(config) {
        eprintln!("{line}");
    }
}

/// One line telling the user that context was dropped and cannot be recovered.
///
/// A `switch` rollover that cannot resolve exact provenance has no summary and
/// no event to look the dropped messages up with, so the wording has to say
/// "lossy" rather than "compacted": the difference decides whether the user
/// re-states what the model just lost.
pub(crate) fn format_context_degraded_notice(dropped_messages: usize) -> String {
    let plural = if dropped_messages == 1 { "message" } else { "messages" };
    format!("Context trimmed (lossy): {dropped_messages} older {plural} dropped without a recoverable handoff.")
}

fn manual_compact_below_trigger_threshold(
    history: &[ChatMessage],
    compaction_config: &crate::config::AgentCompactionConfig,
) -> bool {
    !crate::agent::context_budget::plan_context_budget(
        history,
        compaction_config,
        crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
    )
    .over_hard_limit
}

fn format_nothing_to_compact_feedback(
    history: &[ChatMessage],
    compaction_config: &crate::config::AgentCompactionConfig,
) -> String {
    let system_count = usize::from(history.first().is_some_and(|m| m.role == "system"));
    let turns = history.len().saturating_sub(system_count);
    let tokens = estimate_chat_history_tokens(history);
    let window = format_compact_token_count(compaction_config.max_context_tokens);
    format!("Nothing to compact: {turns} turns / ~{tokens} tokens / {window} window.")
}

fn format_compact_feedback_after_history(
    turns_before: usize,
    tokens_before: usize,
    history: &[ChatMessage],
    compaction_config: &crate::config::AgentCompactionConfig,
) -> String {
    let system_count = usize::from(history.first().is_some_and(|m| m.role == "system"));
    format_compact_feedback(
        turns_before,
        history.len().saturating_sub(system_count),
        tokens_before,
        estimate_chat_history_tokens(history),
        compaction_config.max_context_tokens,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "terminal-tui")]
struct TuiContextBudgetStatus {
    used_context_tokens: usize,
    max_context_tokens: usize,
}

#[cfg(feature = "terminal-tui")]
fn context_budget_status_for_tui(
    history: &[ChatMessage],
    compaction_config: &crate::config::AgentCompactionConfig,
    terminal_tui_enabled: bool,
) -> Option<TuiContextBudgetStatus> {
    if !terminal_tui_enabled {
        return None;
    }
    let budget = crate::agent::context_budget::plan_context_budget(
        history,
        compaction_config,
        crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
    );
    Some(TuiContextBudgetStatus {
        used_context_tokens: budget.used_tokens,
        max_context_tokens: budget.max_context_tokens,
    })
}

#[cfg(feature = "terminal-tui")]
fn refresh_context_budget_for_tui(
    history: &[ChatMessage],
    compaction_config: &crate::config::AgentCompactionConfig,
    terminal_tui_enabled: bool,
    chat_mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
) {
    let Some(context_budget) = context_budget_status_for_tui(history, compaction_config, terminal_tui_enabled) else {
        return;
    };
    let context_used_tokens = Some(context_budget.used_context_tokens);
    let context_window_tokens = Some(context_budget.max_context_tokens);
    {
        let mut mirror = chat_mirror.lock();
        mirror.context_used_tokens = context_used_tokens;
        mirror.context_window_tokens = context_window_tokens;
    }
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ContextWindowUpdated {
            used_context_tokens: context_used_tokens,
            max_context_tokens: context_window_tokens,
        },
        "chat.context_window_updated",
    );
}

async fn build_chat_compaction_patch(
    budget_history: &[ChatMessage],
    source_history: &[ChatMessage],
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &str,
) -> Option<crate::agent::turn_contracts::CompactionPatch> {
    match build_configurable_compaction_patch_with_source_history(
        budget_history,
        source_history,
        provider,
        model,
        config,
        audit,
        trigger,
    )
    .await
    {
        Ok(patch) => patch,
        Err(error) => {
            tracing::warn!(%error, trigger, "chat summary compaction failed; falling back to deterministic trim");
            None
        }
    }
}

fn apply_chat_compaction_patch_and_sync(
    history: &mut Vec<ChatMessage>,
    patch_source_history: Option<&[ChatMessage]>,
    patch: crate::agent::turn_contracts::CompactionPatch,
    config: &crate::config::AgentCompactionConfig,
    reason: crate::chat::action::CompactReason,
    chat_dispatcher: &dispatcher::ChatDispatcher,
) {
    let replacement_len = patch.replacement.len();
    if let Some(source_history) = patch_source_history {
        let mut source_history = source_history.to_vec();
        apply_compaction_patch_exact(&mut source_history, &patch);
        *history = source_history;
    } else {
        apply_compaction_patch_exact(history, &patch);
    }
    let budget = crate::agent::context_budget::plan_context_budget(
        history,
        config,
        crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
    );
    if budget.over_hard_limit {
        let trimmed =
            crate::agent::context_budget::trim_history_to_context_budget_preserving_compaction_replacement_with_floor(
                history,
                config,
                replacement_len,
            );
        tracing::warn!(
            used_tokens = budget.used_tokens,
            hard_limit = budget.available_input_tokens,
            trimmed,
            "chat summary compaction applied preserving trim"
        );
    }
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::HistoryCompactionPatchApplied {
            reason,
            patch,
            compaction_config: config.clone(),
        },
        "chat.history_compaction_patch_applied",
    );
}

fn bounded_legacy_chat_compaction_audit_source(history: &[ChatMessage]) -> Vec<ChatMessage> {
    let has_system = history.first().is_some_and(|msg| msg.role == "system");
    let start = if has_system { 1 } else { 0 };
    let mut source = Vec::new();
    if let Some(system) = history.first().filter(|_| has_system) {
        source.push(ChatMessage::system(truncate_with_ellipsis(
            &system.content,
            COMPACT_CONTENT_CHARS,
        )));
    }
    let non_system = history.len().saturating_sub(start);
    let keep_start = start + non_system.saturating_sub(COMPACT_KEEP_MESSAGES);
    for msg in history.iter().skip(keep_start) {
        let content = truncate_with_ellipsis(&msg.content, COMPACT_CONTENT_CHARS);
        source.push(ChatMessage {
            role: msg.role.clone(),
            content,
        });
    }
    source
}

fn original_legacy_chat_compaction_audit_source(history: &[ChatMessage]) -> Vec<ChatMessage> {
    if history.len() <= 1 {
        return Vec::new();
    }

    let has_system = history.first().is_some_and(|msg| msg.role == "system");
    let start = if has_system { 1 } else { 0 };
    let non_system_count = history.len().saturating_sub(start);
    if non_system_count == 0 {
        return Vec::new();
    }

    let mut lost_indices = std::collections::BTreeSet::new();
    let mut retained: Vec<(usize, &ChatMessage)> = history.iter().enumerate().skip(start).collect();
    if non_system_count > COMPACT_KEEP_MESSAGES {
        let drop_count = non_system_count.saturating_sub(COMPACT_KEEP_MESSAGES);
        for (index, _) in retained.iter().take(drop_count) {
            lost_indices.insert(*index);
        }
        retained.drain(0..drop_count);
    }

    let mut compacted_chars = 0usize;
    let mut budget_retained = Vec::new();
    for (index, msg) in retained {
        if msg.content.chars().count() > COMPACT_CONTENT_CHARS {
            lost_indices.insert(index);
        }
        let compacted_len = msg.content.chars().count().min(COMPACT_CONTENT_CHARS);
        compacted_chars = compacted_chars.saturating_add(compacted_len);
        budget_retained.push((index, compacted_len));
    }

    while compacted_chars > COMPACT_TOTAL_CHARS && budget_retained.len() > 1 {
        let (index, compacted_len) = budget_retained.remove(0);
        lost_indices.insert(index);
        compacted_chars = compacted_chars.saturating_sub(compacted_len);
    }

    history
        .iter()
        .enumerate()
        .filter(|(index, _)| lost_indices.contains(index))
        .map(|(_, msg)| msg.clone())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LegacyCompactionTokenMetadata {
    provider_token_estimate: usize,
    persisted_token_estimate: usize,
    enrichment_token_delta: isize,
}

fn legacy_compaction_token_metadata(
    provider_history: &[ChatMessage],
    persisted_history: &[ChatMessage],
) -> LegacyCompactionTokenMetadata {
    let provider_token_estimate = estimate_chat_history_tokens(provider_history);
    let persisted_token_estimate = estimate_chat_history_tokens(persisted_history);
    LegacyCompactionTokenMetadata {
        provider_token_estimate,
        persisted_token_estimate,
        enrichment_token_delta: provider_token_estimate as isize - persisted_token_estimate as isize,
    }
}

async fn persist_legacy_chat_compaction_audit(
    mem: &dyn Memory,
    envelope: &RuntimeEnvelope,
    source_history: &[ChatMessage],
    summary_projection: &[ChatMessage],
    token_metadata: LegacyCompactionTokenMetadata,
    trigger: &str,
) {
    if source_history.is_empty() {
        return;
    }
    let run_id = uuid::Uuid::new_v4().to_string();
    let summary_memory_key = format!("compaction_summary_{}", run_id.replace('-', "_"));
    let source_message_count = source_history.len();
    let provenance = match crate::agent::compaction::resolve_compaction_event_provenance(
        mem,
        envelope.memory_principal(),
        source_history,
    )
    .await
    {
        Ok(provenance) => provenance,
        Err(error) => {
            tracing::debug!(error = %error, "failed to resolve legacy chat compaction MessageEvent provenance");
            None
        }
    };
    let provenance_status = if provenance.is_some() { "exact" } else { "unavailable" };
    let fidelity_status = if provenance.is_some() {
        "accepted_legacy_deterministic"
    } else {
        "accepted_legacy_content_hash_fallback"
    };
    let source_event_ids_json = provenance
        .as_ref()
        .and_then(|provenance| serde_json::to_string(&provenance.source_event_ids).ok());
    let source_event_range_json = provenance
        .as_ref()
        .and_then(|provenance| serde_json::to_string(&provenance.covered_range).ok());
    let source_document_refs_json = serde_json::to_string(
        &source_history
            .iter()
            .enumerate()
            .map(|(index, message)| {
                serde_json::json!({
                    "kind": "message_content_hash",
                    "index": index,
                    "role": message.role,
                    "content_sha256": crate::self_system::evolution::safety_utils::sha256_hex(
                        &sanitize::sanitize_for_persistence(&message.content)
                    )
                })
            })
            .collect::<Vec<_>>(),
    )
    .ok();
    let summary = format!(
        "Legacy chat context overflow compaction preserved the system prompt, kept the last {COMPACT_KEEP_MESSAGES} non-system messages, truncated turns to {COMPACT_CONTENT_CHARS} chars, and capped retained chat context at {COMPACT_TOTAL_CHARS} chars."
    );
    let owner = envelope.owner_principal();
    let metadata = MemoryStoreMetadata {
        workspace_id: Some(envelope.workspace_id.clone()),
        owner_id: Some(owner.owner_id.clone()),
        agent_id: envelope.agent_id.clone(),
        persona_id: envelope.persona_id.clone(),
        source_event_id: provenance
            .as_ref()
            .map(|provenance| provenance.covered_range.last_event_id.clone()),
        source: Some("legacy_chat_compaction_summary".to_string()),
        topic_id: None,
        channel: None,
    };
    if let Err(error) = mem
        .store_with_metadata(
            &summary_memory_key,
            &summary,
            MemoryCategory::Conversation,
            Some(&envelope.session_key),
            metadata,
        )
        .await
    {
        tracing::debug!(error = %error, "failed to persist legacy chat compaction summary memory");
    }
    let summary_message_event_id = if let Some(provenance) = provenance.as_ref() {
        let raw_payload_json = serde_json::json!({
            "compaction_run_id": run_id,
            "trigger": trigger,
            "mode": "legacy_chat_overflow",
            "fidelity_status": fidelity_status,
            "source_event_ids": provenance.source_event_ids,
            "covered_event_range": provenance.covered_range
        })
        .to_string();
        match mem
            .append_message_event(crate::memory::MessageEventInput {
                event_id: None,
                idempotency_key: Some(format!("compaction:{run_id}:summary")),
                workspace_id: envelope.workspace_id.clone(),
                owner_id: Some(owner.owner_id.clone()),
                source: "compaction".into(),
                channel: envelope.channel.clone(),
                session_key: Some(envelope.session_key.clone()),
                parent_session_key: None,
                run_id: Some(run_id.clone()),
                parent_run_id: None,
                agent_id: envelope.agent_id.clone(),
                persona_id: envelope.persona_id.clone(),
                sender: Some("compaction".to_string()),
                recipient: envelope.sender.clone(),
                role: "event".to_string(),
                event_type: "compaction.summary.created".to_string(),
                subject: Some(crate::memory::MessageEventSubject::Conversation(
                    envelope.session_key.clone(),
                )),
                goal_id: None,
                causation_event_id: Some(provenance.covered_range.last_event_id.clone()),
                correlation_id: Some(run_id.clone()),
                attempt_id: None,
                lease_epoch: None,
                config_generation_id: envelope.config_generation_id,
                config_source_revision: envelope.config_source_revision.clone(),
                content: sanitize::sanitize_for_persistence(&summary),
                raw_payload_json: Some(raw_payload_json),
                visibility: envelope.visibility.clone(),
            })
            .await
        {
            Ok(event) => Some(event.event_id),
            Err(error) => {
                tracing::debug!(error = %error, "failed to append legacy chat compaction summary MessageEvent");
                None
            }
        }
    } else {
        None
    };
    if let Err(error) = mem
        .append_compaction_run(CompactionRunInput {
            run_id: Some(run_id),
            workspace_id: envelope.workspace_id.clone(),
            owner_id: Some(owner.owner_id),
            session_key: Some(envelope.session_key.clone()),
            agent_id: envelope.agent_id.clone(),
            persona_id: envelope.persona_id.clone(),
            trigger: trigger.to_string(),
            mode: "legacy_chat_overflow".to_string(),
            source_message_count,
            source_token_estimate: estimate_chat_history_tokens(source_history),
            summary,
            summary_memory_key: Some(summary_memory_key),
            source_event_ids_json,
            source_event_range_json,
            source_document_refs_json,
            fidelity_status: fidelity_status.to_string(),
            payload_json: Some(
                serde_json::json!({
                    "compact_keep_messages": COMPACT_KEEP_MESSAGES,
                    "compact_content_chars": COMPACT_CONTENT_CHARS,
                    "compact_total_chars": COMPACT_TOTAL_CHARS,
                    "summary_projection_message_count": summary_projection.len(),
                    "summary_projection_token_estimate": estimate_chat_history_tokens(summary_projection),
                    "provider_token_estimate": token_metadata.provider_token_estimate,
                    "persisted_token_estimate": token_metadata.persisted_token_estimate,
                    "enrichment_token_delta": token_metadata.enrichment_token_delta,
                    "source_event_provenance_status": provenance_status,
                    "summary_message_event_id": summary_message_event_id
                })
                .to_string(),
            ),
        })
        .await
    {
        tracing::debug!(error = %error, "failed to append legacy chat compaction run");
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn print_fallback_chat_output(text: &str) {
    let out = format_fallback_chat_output_for(text, std::io::stdout().is_terminal());
    print!("{out}");
    let _ = std::io::stdout().flush();
}

/// Format the recap of child sessions restored from a reloaded chat
/// session (v4). Pure string builder (no I/O, no lock) so it is trivially
/// unit-testable. Each line shows the kind, display seq `#N`, terminal status,
/// title/command, and (when present) the completion summary. The header makes
/// it explicit that these are historical results — nothing has been revived.
fn format_reloaded_background_sessions(sessions: &[crate::chat::sessions::PersistedSessionSummary]) -> String {
    let mut out = String::new();
    out.push_str("[previous session — background task results (not resumed)]");
    for s in sessions {
        let summary = s.summary.trim();
        // v5: tag model-spawned sessions so the recap distinguishes them from
        // operator-initiated ones. User-initiated sessions stay untagged to keep
        // the common case quiet (origin defaults to "user" for legacy blobs).
        let origin_tag = if s.origin == "model" { " [model]" } else { "" };
        let usage = crate::chat::session::summarize_session_token_usage(&s.token_usage_records)
            .and_then(crate::chat::session::format_session_token_usage_inline);
        let usage = usage.map_or_else(String::new, |usage| format!(" {usage}"));
        if summary.is_empty() {
            out.push_str(&format!(
                "\n  · {} #{}{} {}{} — {}",
                s.kind, s.seq, origin_tag, s.status, usage, s.title
            ));
        } else {
            out.push_str(&format!(
                "\n  · {} #{}{} {}{} — {}: {}",
                s.kind, s.seq, origin_tag, s.status, usage, s.title, summary
            ));
        }
    }
    out
}

fn format_managed_sessions_list(views: &[crate::chat::sessions::model::ManagedSessionView]) -> String {
    if views.is_empty() {
        return "No child TUI sessions.".to_string();
    }
    let mut out = String::from("Background sessions:\n");
    for v in views {
        let usage = crate::chat::session::summarize_session_token_usage(&v.token_usage_records)
            .and_then(crate::chat::session::format_session_token_usage_inline);
        let usage = usage.map_or_else(String::new, |usage| format!(" {usage}"));
        out.push_str(&format!(
            "  #{} {} {} {} {}{} {}\n",
            v.seq,
            v.kind.as_str(),
            v.origin.as_str(),
            v.status.as_str(),
            crate::chat::sessions::model::session_elapsed_label(v),
            usage,
            v.title
        ));
    }
    out.trim_end().to_string()
}

/// Append this chat's in-flight daemon requests to a `/sessions` listing.
fn append_in_flight_daemon_requests(
    listing: String,
    registry: &crate::chat::sessions::daemon::InFlightRegistry,
) -> String {
    match crate::chat::sessions::daemon::in_flight_report(registry) {
        Some(report) => format!("{listing}\n\n{report}"),
        None => listing,
    }
}

async fn handle_local_session_command(
    action: &crate::chat::sessions::SessionCommand,
    chat_sessions: &mut crate::chat::sessions::ChatSessionsHandle,
    session_rings: &std::collections::HashMap<crate::chat::sessions::id::SessionId, crate::chat::sessions::SessionRing>,
    reaped_log_archive: &mut ReapedSessionLogArchive,
    reap_policy: &crate::chat::sessions::runtime::ReapPolicy,
    tools_registry: &[Box<dyn Tool>],
) -> Option<String> {
    use crate::chat::sessions::SessionCommand;

    match action {
        SessionCommand::Sessions => {
            let views = chat_sessions.snapshot().await;
            // A daemon request is not a child session — nothing local is
            // running for it — but it is in-flight work this chat is waiting
            // on, and the whole point of listing it is that a daemon which
            // accepted the connection and never answered must not be able to
            // hide here. Appended rather than merged so the two remain
            // visibly different kinds of thing.
            Some(append_in_flight_daemon_requests(
                format_managed_sessions_list(&views),
                &chat_sessions.daemon_requests(),
            ))
        }
        SessionCommand::Logs { seq } => {
            Some(format_local_session_logs(*seq, chat_sessions, session_rings, reaped_log_archive, reap_policy).await)
        }
        SessionCommand::Kill { seq } => Some(kill_local_session(*seq, chat_sessions, tools_registry).await),
        _ => None,
    }
}

async fn format_local_session_logs(
    seq: u64,
    chat_sessions: &mut crate::chat::sessions::ChatSessionsHandle,
    session_rings: &std::collections::HashMap<crate::chat::sessions::id::SessionId, crate::chat::sessions::SessionRing>,
    reaped_log_archive: &mut ReapedSessionLogArchive,
    reap_policy: &crate::chat::sessions::runtime::ReapPolicy,
) -> String {
    match chat_sessions.resolve_run_id(seq).await {
        Ok(run_id) => {
            let sid = crate::chat::sessions::id::SessionId::from_run_id(&run_id);
            session_rings.get(&sid).map_or_else(
                || format!("Session #{seq} has no buffered output yet."),
                |ring| {
                    // Replay the retained window without disturbing live-follow cursors.
                    let lines = ring.recent_lines(SESSION_LOGS_MAX_LINES);
                    if lines.is_empty() {
                        format!("Session #{seq} has no buffered output yet.")
                    } else {
                        format_session_logs(seq, &lines, ring.is_truncated())
                    }
                },
            )
        }
        Err(e) => {
            let now = chrono::Utc::now();
            reaped_log_archive
                .logs_message(seq, reap_policy, now)
                .unwrap_or_else(|| {
                    chat_sessions.reaped_session(seq).map_or_else(
                        || format!("Logs failed: {e}"),
                        |reaped| format_reaped_session_notice(seq, Some(&reaped.summary)),
                    )
                })
        }
    }
}

async fn kill_local_session(
    seq: u64,
    chat_sessions: &mut crate::chat::sessions::ChatSessionsHandle,
    tools_registry: &[Box<dyn Tool>],
) -> String {
    match chat_sessions.kind_for_seq(seq).await {
        Ok(crate::chat::sessions::model::ManagedKind::Shell) => {
            return match chat_sessions.kill_shell(seq).await {
                Ok(()) => format!("Killed background shell #{seq} (process group terminated)."),
                Err(e) => format!("Kill failed: {e}"),
            };
        }
        Ok(crate::chat::sessions::model::ManagedKind::Pty) => {
            #[cfg(feature = "terminal-tui")]
            {
                return match chat_sessions.kill_pty(seq).await {
                    Ok(()) => {
                        format!("Killed interactive PTY session #{seq} (process group terminated).")
                    }
                    Err(e) => format!("Kill failed: {e}"),
                };
            }
            #[cfg(not(feature = "terminal-tui"))]
            {
                return "Interactive PTY sessions are only available in the terminal TUI.".to_string();
            }
        }
        Ok(crate::chat::sessions::model::ManagedKind::Agent) => {}
        Ok(crate::chat::sessions::model::ManagedKind::Transcript) => {
            return "Transcript is a read-only viewer, not a killable child session.".to_string();
        }
        Ok(crate::chat::sessions::model::ManagedKind::Approval) => {
            return "Tool approval is a foreground prompt, not a killable child session.".to_string();
        }
        Ok(crate::chat::sessions::model::ManagedKind::Diff) => {
            return "Diff is a read-only viewer, not a killable child session.".to_string();
        }
        Ok(crate::chat::sessions::model::ManagedKind::Worker) => {
            return "Provider worker detail is a read-only viewer, not a killable child session.".to_string();
        }
        Err(e) => return format!("Kill failed: {e}"),
    }

    // Agent path: resolve `#N` -> run UUID and delegate to sessions_spawn `kill`
    // so side-effect grants, terminal-state checks, steer cleanup, and task
    // events remain identical to the normal `/kill` path.
    let run_id = match chat_sessions.resolve_run_id(seq).await {
        Ok(id) => id,
        Err(e) => return format!("Kill failed: {e}"),
    };
    let Some(tool) = tools_registry.iter().find(|t| t.supports_name("sessions_spawn")) else {
        return "Background sessions are not available in this session.".to_string();
    };

    let operation_name = format!("sessions_spawn:kill:{run_id}");
    let grant = crate::security::policy::ApprovalGrant::for_resource_operation(
        "sessions_spawn",
        &operation_name,
        "chat-operator",
        None,
    );
    let mut args = serde_json::json!({
        "action": "kill",
        "run_id": run_id,
    });
    match serde_json::to_value(&grant) {
        Ok(grant_value) => {
            if let Some(obj) = args.as_object_mut() {
                obj.insert(
                    crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.to_string(),
                    grant_value,
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to serialize kill approval grant; proceeding without it"
            );
        }
    }
    match tool.execute_named("sessions_spawn", args).await {
        Ok(result) => {
            if result.output.is_empty() {
                result
                    .error
                    .filter(|e| !e.is_empty())
                    .unwrap_or_else(|| "(no output)".to_string())
            } else {
                result.output
            }
        }
        Err(e) => format!("Kill failed: {e}"),
    }
}

fn format_observed_session_announcement(view: &crate::chat::sessions::model::ManagedSessionView) -> String {
    let elapsed = crate::chat::sessions::model::session_elapsed_label(view);
    let title = clamp_chars(&view.title, CHILD_STARTED_TITLE_MAX_CHARS);
    let verb = if matches!(
        view.status,
        crate::chat::sessions::model::ManagedStatus::Running | crate::chat::sessions::model::ManagedStatus::NeedsInput
    ) {
        "started"
    } else {
        "observed"
    };
    if title.is_empty() {
        format!(
            "[{} #{} {} {} {elapsed}] {verb}",
            view.kind.as_str(),
            view.seq,
            view.origin.as_str(),
            view.status.as_str(),
        )
    } else {
        format!(
            "[{} #{} {} {} {elapsed}] {verb}: {title}",
            view.kind.as_str(),
            view.seq,
            view.origin.as_str(),
            view.status.as_str(),
        )
    }
}

fn format_finished_session_announcement(fin: &crate::chat::sessions::runtime::FinishedSession) -> String {
    let kind = fin.kind.as_str();
    let marker = match fin.status {
        crate::chat::sessions::model::ManagedStatus::Completed => "✓",
        crate::chat::sessions::model::ManagedStatus::Failed
        | crate::chat::sessions::model::ManagedStatus::Cancelled => "✗",
        crate::chat::sessions::model::ManagedStatus::Running
        | crate::chat::sessions::model::ManagedStatus::NeedsInput => "•",
    };
    let elapsed = crate::chat::sessions::model::format_elapsed_compact(
        crate::chat::sessions::model::elapsed_seconds_between(fin.created_at, fin.updated_at),
    );
    let usage = if fin.kind == crate::chat::sessions::model::ManagedKind::Agent {
        crate::chat::session::summarize_session_token_usage(&fin.token_usage_records)
            .and_then(crate::chat::session::format_session_token_usage_inline)
    } else {
        None
    };
    let suffix = usage.map_or_else(|| elapsed.clone(), |usage| format!("{elapsed} · {usage}"));
    let summary = compact_child_completion_summary(&fin.summary);
    if summary.is_empty() {
        format!("[{kind} #{} {marker} {suffix}]", fin.seq)
    } else {
        format!("[{kind} #{} {marker} {suffix}] {summary}", fin.seq)
    }
}

const CHILD_COMPLETION_SUMMARY_MAX_CHARS: usize = 120;
const CHILD_STARTED_TITLE_MAX_CHARS: usize = 96;

fn compact_child_completion_summary(text: &str) -> String {
    let Some(line) = text.lines().map(str::trim).find(|line| is_useful_completion_line(line)) else {
        return String::new();
    };
    clamp_chars(line, CHILD_COMPLETION_SUMMARY_MAX_CHARS)
}

#[cfg(feature = "terminal-tui")]
fn refresh_sessions_cache(mirror: &mut tui::TuiState, entries: Vec<crate::chat::sessions::SwitcherEntry>) {
    mirror.sessions_cache = entries;
}

fn is_useful_completion_line(line: &str) -> bool {
    !line.is_empty() && !line.starts_with("```") && !line.starts_with("~~~")
}

fn clamp_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out = String::new();
    for _ in 0..max_chars {
        match chars.next() {
            Some(ch) => out.push(ch),
            None => return out,
        }
    }
    if chars.next().is_some() && max_chars > 0 {
        out.pop();
        out.push('…');
    }
    out
}

fn format_session_logs(seq: u64, lines: &[String], truncated: bool) -> String {
    let mut out = format!("Session #{seq} logs (last {} lines):\n", lines.len());
    if truncated {
        out.push_str("  [output truncated]\n");
    }
    for line in lines {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

#[derive(Debug, Clone)]
struct ArchivedReapedSessionLog {
    seq: u64,
    reaped_at: chrono::DateTime<chrono::Utc>,
    summary: crate::chat::sessions::PersistedSessionSummary,
    lines: Option<Vec<String>>,
    truncated: bool,
}

#[derive(Debug, Default)]
struct ReapedSessionLogArchive {
    entries: Vec<ArchivedReapedSessionLog>,
}

impl ReapedSessionLogArchive {
    fn archive_reaped(
        &mut self,
        reaped: &[crate::chat::sessions::runtime::ReapedSession],
        session_rings: &mut std::collections::HashMap<
            crate::chat::sessions::id::SessionId,
            crate::chat::sessions::SessionRing,
        >,
        policy: &crate::chat::sessions::runtime::ReapPolicy,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        for session in reaped {
            let (lines, truncated) = session_rings.remove(&session.id).map_or_else(
                || (Vec::new(), false),
                |ring| {
                    (
                        cap_archived_log_lines(
                            ring.recent_lines(policy.archive_max_lines),
                            policy.archive_max_lines,
                            policy.archive_max_bytes,
                        ),
                        ring.is_truncated(),
                    )
                },
            );
            self.entries.push(ArchivedReapedSessionLog {
                seq: session.seq,
                reaped_at: session.reaped_at,
                summary: session.summary.clone(),
                lines: Some(lines),
                truncated,
            });
        }
        self.prune(policy, now);
    }

    fn prune(&mut self, policy: &crate::chat::sessions::runtime::ReapPolicy, now: chrono::DateTime<chrono::Utc>) {
        self.entries
            .sort_by(|a, b| b.reaped_at.cmp(&a.reaped_at).then_with(|| b.seq.cmp(&a.seq)));
        for (idx, entry) in self.entries.iter_mut().enumerate() {
            let within_ttl = now.signed_duration_since(entry.reaped_at) <= policy.archive_ttl;
            if idx >= policy.archive_keep_last && !within_ttl {
                entry.lines = None;
            }
        }
    }

    fn logs_message(
        &mut self,
        seq: u64,
        policy: &crate::chat::sessions::runtime::ReapPolicy,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Option<String> {
        self.prune(policy, now);
        let entry = self.entries.iter().rev().find(|entry| entry.seq == seq)?;
        match entry.lines.as_ref() {
            Some(lines) if !lines.is_empty() => Some(format_session_logs(seq, lines, entry.truncated)),
            Some(_) => Some(format_reaped_session_notice(seq, Some(&entry.summary))),
            None => Some(format_reaped_session_notice(seq, Some(&entry.summary))),
        }
    }
}

fn cap_archived_log_lines(mut lines: Vec<String>, max_lines: usize, max_bytes: usize) -> Vec<String> {
    if lines.len() > max_lines {
        let drop = lines.len().saturating_sub(max_lines);
        lines.drain(0..drop);
    }
    let mut total = lines.iter().map(|line| line.len()).sum::<usize>();
    while total > max_bytes && !lines.is_empty() {
        let first = lines.remove(0);
        total = total.saturating_sub(first.len());
    }
    lines
}

fn format_reaped_session_notice(seq: u64, summary: Option<&crate::chat::sessions::PersistedSessionSummary>) -> String {
    let mut msg = format!("Session #{seq} was reaped from live sessions; use the persisted summary.");
    if let Some(summary) = summary {
        let body = summary.summary.trim();
        if !body.is_empty() {
            msg.push_str(&format!(" Summary: {body}"));
        }
    }
    msg
}

fn is_chat_quit_command(input: &str) -> bool {
    matches!(input, "/quit" | "/exit")
}

async fn shutdown_child_sessions_for_exit(
    chat_sessions: &mut crate::chat::sessions::ChatSessionsHandle,
    chat_session: &mut session::ChatSession,
    chat_dispatcher: &dispatcher::ChatDispatcher,
) -> crate::chat::sessions::runtime::ShutdownReport {
    let report = chat_sessions.shutdown_all("chat-exit").await;
    for persisted in report
        .summaries
        .iter()
        .filter(|summary| summary.status == crate::chat::sessions::model::STATUS_INTERRUPTED)
        .cloned()
    {
        chat_session.record_background_session(persisted.clone());
        let _ = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::BackgroundSessionRecorded { summary: persisted },
            "chat.bg_session_recorded_exit",
        );
    }
    report
}

#[cfg(feature = "terminal-tui")]
const MOUSE_WHEEL_TRANSCRIPT_ROWS: usize = 3;

#[cfg(feature = "terminal-tui")]
const DIRECTIONAL_SWITCH_DEBOUNCE: Duration = Duration::from_millis(100);

#[cfg(feature = "terminal-tui")]
fn debounce_directional_switch_dispatch(
    key: crossterm::event::KeyEvent,
    dispatch: tui::KeyDispatch,
    last_directional_switch_at: &mut Option<Instant>,
    now: Instant,
) -> tui::KeyDispatch {
    if key.modifiers == crossterm::event::KeyModifiers::NONE
        && matches!(
            key.code,
            crossterm::event::KeyCode::Left | crossterm::event::KeyCode::Right
        )
        && matches!(
            &dispatch,
            tui::KeyDispatch::AttachSession { .. }
                | tui::KeyDispatch::SwitchSession { .. }
                | tui::KeyDispatch::OpenProviderWorkerView { .. }
                | tui::KeyDispatch::CloseProviderWorkerView
                | tui::KeyDispatch::RequestDetach
        )
    {
        if last_directional_switch_at.is_some_and(|previous| now.duration_since(previous) < DIRECTIONAL_SWITCH_DEBOUNCE)
        {
            tui::KeyDispatch::Consumed
        } else {
            *last_directional_switch_at = Some(now);
            dispatch
        }
    } else {
        dispatch
    }
}

#[cfg(feature = "terminal-tui")]
fn apply_fullscreen_mouse_scroll(
    kind: crossterm::event::MouseEventKind,
    fullscreen_scroll: &mut tui::FullscreenTranscriptScroll,
) -> bool {
    match kind {
        crossterm::event::MouseEventKind::ScrollUp => {
            fullscreen_scroll.page_up(MOUSE_WHEEL_TRANSCRIPT_ROWS);
            true
        }
        crossterm::event::MouseEventKind::ScrollDown => {
            fullscreen_scroll.page_down(MOUSE_WHEEL_TRANSCRIPT_ROWS);
            true
        }
        _ => false,
    }
}

#[cfg(feature = "terminal-tui")]
fn copy_transcript_selection(
    render_source: &RenderSource,
    selection: tui::TranscriptSelection,
    width: u16,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: &mpsc::Sender<()>,
) {
    if !selection.moved() {
        return;
    }
    let ((start_row, start_column), (end_row, end_column)) = selection.ordered_points();
    let content = render_source.with_view(|view| {
        let rows = tui::transcript_plain_rows(view, width);
        if rows.is_empty() || start_row >= rows.len() {
            return String::new();
        }
        let end_row = end_row.min(rows.len().saturating_sub(1));
        (start_row..=end_row)
            .map(|row| {
                let text = rows.get(row).map(String::as_str).unwrap_or("");
                let width = unicode_width::UnicodeWidthStr::width(text);
                let (start, end) = if start_row == end_row {
                    (start_column, end_column.saturating_add(1).min(width))
                } else if row == start_row {
                    (start_column, width)
                } else if row == end_row {
                    (0, end_column.saturating_add(1).min(width))
                } else {
                    (0, width)
                };
                slice_display_columns(text, start, end)
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    if content.is_empty() {
        return;
    }
    let (content, truncated) = truncate_utf8_to_byte_cap(&content, COPY_OSC52_MAX_BYTES);
    if content.trim().is_empty() {
        return;
    }
    match terminal_proto::copy_to_clipboard(&content) {
        Ok(()) => {
            let suffix = if truncated { " (truncated to 74 KiB)" } else { "" };
            surface_session_message(
                chat_dispatcher,
                Some(redraw_tx),
                &format!("Copied selected transcript rows to clipboard{suffix}."),
            );
        }
        Err(error) => surface_session_message(chat_dispatcher, Some(redraw_tx), &format!("Copy failed: {error}")),
    }
}

#[cfg(feature = "terminal-tui")]
fn slice_display_columns(input: &str, start: usize, end: usize) -> String {
    if start >= end {
        return String::new();
    }
    let mut output = String::new();
    let mut column = 0usize;
    for ch in input.chars() {
        let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        let next_column = column.saturating_add(char_width);
        if next_column > start && column < end {
            output.push(ch);
        }
        column = next_column;
        if column >= end {
            break;
        }
    }
    output
}

/// Surface a background-session system message into the chat (v1b reflow).
///
/// Standalone (not the in-loop `emit_chat_output` closure) so the main loop's
/// timer-tick branch — which runs inside the `select!` header, before the
/// closure is in scope — can use it. Behaviour mirrors `emit_chat_output`:
/// route through the dispatcher (single source, reaches both render paths) and
/// nudge the renderer on the TUI path; fall back to plain stdout otherwise.
#[cfg_attr(not(feature = "terminal-tui"), allow(unused_variables))]
fn surface_session_message(dispatcher: &dispatcher::ChatDispatcher, redraw_tx: Option<&mpsc::Sender<()>>, text: &str) {
    #[cfg(feature = "terminal-tui")]
    {
        let _ = dispatcher.dispatch_or_log(
            crate::chat::action::Action::SystemMessageAdded { text: text.to_string() },
            "chat.system_message_session",
        );
        if let Some(tx) = redraw_tx {
            let _ = tx.try_send(());
        } else {
            print_fallback_chat_output(text);
        }
    }
    #[cfg(not(feature = "terminal-tui"))]
    {
        print_fallback_chat_output(text);
    }
}

#[cfg(feature = "terminal-tui")]
fn surface_active_turn_message(
    dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
    text: &str,
) {
    surface_session_message(dispatcher, redraw_tx, text);
}

/// Run one `--daemon`-scoped session command without holding the input loop (a5).
///
/// The runtime control API deliberately carries no request timeout — a running
/// task must not be abandoned because a clock expired — so awaiting it inline
/// would let an unresponsive daemon freeze the TUI, which is strictly worse
/// than the feature being missing. The request therefore runs in its own task
/// and reports back through the same system-message path background sessions
/// use; the loop returns to reading input immediately.
///
/// `local_fallback` is this chat's own `/sessions` listing, captured by the
/// caller *before* the task starts: `chat_sessions` is loop-owned mutable state
/// that cannot cross into a spawned task, so a listing that degrades on failure
/// has to bring its fallback with it.
///
/// The task is published in `in_flight` for as long as it runs. Without that a
/// daemon which accepts the connection and then answers nothing would leave a
/// task nobody can see and nobody can end — and the answer to a stall in this
/// runtime is being visible and killable, never a clock. The guard rides into
/// the task, so the row is retired by any ending: answered, failed, or ended by
/// an operator.
fn spawn_daemon_session_request(
    request: crate::chat::sessions::daemon::DaemonRequest,
    endpoint: crate::runtime::tasks_cli::TasksEndpoint,
    local_fallback: Option<String>,
    in_flight: &crate::chat::sessions::daemon::InFlightRegistry,
    dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
) {
    let cancel = CancellationToken::new();
    let guard =
        crate::chat::sessions::daemon::register_in_flight(in_flight, &request, endpoint.base_url(), cancel.clone());
    // Built before `request` is consumed by the call below.
    let cancelled = crate::chat::sessions::daemon::cancelled_notice(&request, &endpoint.shown_url());
    let dispatcher = dispatcher.clone();
    let redraw_tx = redraw_tx.cloned();
    tokio::spawn(async move {
        let _guard = guard;
        // `run` renders both outcomes, so there is no error path to drop here.
        // Cancellation drops the in-flight HTTP request with it, which is what
        // makes `/kill --daemon d<N>` an actual end rather than a hidden leak.
        let text = tokio::select! {
            () = cancel.cancelled() => cancelled,
            text = crate::chat::sessions::daemon::run(&endpoint, request, local_fallback.as_deref()) => text,
        };
        surface_session_message(&dispatcher, redraw_tx.as_ref(), &text);
    });
}

/// Everything a `--daemon`-scoped command needs to reach the daemon.
///
/// Bundled because the same three values have to reach both execution paths —
/// the idle input loop and the active-turn one — and the active-turn signature
/// is already long.
#[derive(Clone, Copy)]
struct DaemonControl<'a> {
    config: &'a Config,
    dispatcher: &'a dispatcher::ChatDispatcher,
    redraw_tx: Option<&'a mpsc::Sender<()>>,
}

/// Start one `--daemon`-scoped session command, returning the line to show now.
///
/// `None` for any action that is not daemon-scoped, so a caller can use this as
/// the daemon arm of its dispatch and fall through to the local handler.
///
/// This is the single execution point for the three `Daemon*` variants: chat
/// reaches them from two places (the idle loop and, while a provider turn is in
/// flight, the active-turn path), and a `--daemon` command that only worked in
/// one of them would be missing exactly when it is most wanted — a long turn is
/// running and the operator needs to look at, or end, what the daemon is doing.
///
/// `local_fallback` is only meaningful for a listing; the caller builds it
/// because only the caller holds `chat_sessions`.
fn start_daemon_session_command(
    action: &crate::chat::sessions::SessionCommand,
    local_fallback: Option<String>,
    in_flight: &crate::chat::sessions::daemon::InFlightRegistry,
    control: DaemonControl<'_>,
) -> Option<String> {
    use crate::chat::sessions::SessionCommand;

    let request = match action {
        // Chat sees the daemon's work as a local *operator*, not as the user
        // whose sessions `/sessions` lists; the local listing rides along only
        // as the fallback to show if the daemon turns out to be unreachable.
        SessionCommand::DaemonSessions => crate::chat::sessions::daemon::DaemonRequest::List,
        SessionCommand::DaemonKill { address } => {
            // A `d<N>` address names one of *this chat's* own in-flight
            // requests, which is the only address the daemon could not act on:
            // it is this process that is waiting, not the daemon that is
            // working. Every other address space belongs to the daemon and is
            // forwarded verbatim.
            if let Some(text) = crate::chat::sessions::daemon::cancel_in_flight(in_flight, address) {
                return Some(text);
            }
            // No local fallback below: a kill either happened in the daemon or
            // it did not, and this chat's own sessions are not a partial answer
            // to that question.
            crate::chat::sessions::daemon::DaemonRequest::Kill {
                address: address.clone(),
            }
        }
        // The daemon resolves the address and decides whether the target can be
        // steered at all (a turn has no steer channel and is refused there), so
        // chat does no kind dispatch of its own — unlike the local `/steer`,
        // which owns its session kinds.
        SessionCommand::DaemonSteer { address, message } => crate::chat::sessions::daemon::DaemonRequest::Steer {
            address: address.clone(),
            message: message.clone(),
        },
        _ => return None,
    };
    let endpoint = crate::chat::sessions::daemon::endpoint(control.config);
    let notice = crate::chat::sessions::daemon::pending_notice(&request, &endpoint.shown_url());
    spawn_daemon_session_request(
        request,
        endpoint,
        local_fallback,
        in_flight,
        control.dispatcher,
        control.redraw_tx,
    );
    Some(notice)
}

/// One chat process's enrolment for daemon-assigned work.
///
/// Held for the life of the chat loop so the exit path can withdraw it: nothing
/// on the daemon side removes a chat session on a clock — a session that
/// stopped pulling is *reported* as silent, never evicted — so an exit that
/// forgets this leaves a row that work can still be addressed to with nobody
/// left to run it.
struct AssignmentEnrolment {
    client: std::sync::Arc<crate::chat::sessions::assignment::AssignmentClient>,
    /// Ends the poll loop. Also the token behind its `/kill --daemon d<N>` row.
    cancel: CancellationToken,
}

/// The sink assignment machinery reports through.
///
/// The same system-message path background sessions use, so assigned work is
/// announced wherever the operator is already reading — TUI transcript on one
/// path, plain stdout on the other — instead of inventing a second surface.
fn assignment_notifier(
    dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
) -> std::sync::Arc<dyn Fn(&str) + Send + Sync> {
    let dispatcher = dispatcher.clone();
    let redraw_tx = redraw_tx.cloned();
    std::sync::Arc::new(move |text: &str| surface_session_message(&dispatcher, redraw_tx.as_ref(), text))
}

/// Start enrolling this chat for daemon-assigned work, without waiting for it.
///
/// Returns `None` when nothing in the configuration points this chat at a
/// daemon — the common case, and one that must stay silent: the fallback daemon
/// address is `[gateway]`'s own bind, which every chat has whether or not a
/// daemon listens there, so a chat that tried regardless would announce a
/// refusal on every single start.
///
/// Enrolment itself happens in the spawned task, so a daemon that is slow,
/// down, or refusing credentials never delays the prompt.
fn start_assignment_enrolment(
    config: &Config,
    chat_sessions: &crate::chat::sessions::ChatSessionsHandle,
    input_tx: &mpsc::Sender<crate::channels::traits::ChannelMessage>,
    dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
) -> Option<AssignmentEnrolment> {
    use crate::chat::sessions::assignment::{self, Enrolable};

    match assignment::enrolable(config) {
        Enrolable::No(reason) => {
            tracing::debug!(reason, "chat is not enrolling for daemon assignments");
            return None;
        }
        Enrolable::Yes => {}
    }
    let client = assignment::AssignmentClient::new(config);
    let cancel = CancellationToken::new();
    tokio::spawn(assignment::enrol_and_poll(
        std::sync::Arc::clone(&client),
        chat_sessions.daemon_requests(),
        input_tx.clone(),
        assignment_notifier(dispatcher, redraw_tx),
        cancel.clone(),
    ));
    Some(AssignmentEnrolment { client, cancel })
}

/// Arm the report for an input message that turns out to be assigned work.
///
/// `None` for ordinary input and for a chat that never enrolled, so the turn
/// loop pays nothing for the feature it is not using.
fn assignment_turn_guard(
    enrolment: Option<&AssignmentEnrolment>,
    chat_sessions: &crate::chat::sessions::ChatSessionsHandle,
    dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
    msg: &crate::channels::traits::ChannelMessage,
) -> Option<crate::chat::sessions::assignment::AssignmentTurnGuard> {
    let delivery = crate::chat::sessions::assignment::delivered(msg)?;
    let enrolment = enrolment?;
    Some(crate::chat::sessions::assignment::AssignmentTurnGuard::new(
        std::sync::Arc::clone(&enrolment.client)
            as std::sync::Arc<dyn crate::chat::sessions::assignment::AssignmentMailbox>,
        chat_sessions.daemon_requests(),
        assignment_notifier(dispatcher, redraw_tx),
        delivery.assignment_id,
    ))
}

/// Withdraw this chat from the daemon's registry on the way out.
///
/// Ends the poll loop first so nothing takes new work between here and the
/// deregistration, then hands back whatever was still outstanding. The daemon
/// records a `cancelled` result for each of those, which is how the assigner
/// learns the work is not coming rather than waiting on a session that no
/// longer exists.
///
/// The final turn's result is reported from its own task, so it races this
/// call. That race is not closed by waiting on it: the control API carries no
/// request timeout by design, so a daemon that accepted the connection and then
/// said nothing would hold this chat's exit open with nothing left on screen to
/// end it with. The losing side of the race is a `cancelled` verdict instead of
/// a `completed` one — still true, and still delivered.
async fn withdraw_assignment_enrolment(
    enrolment: Option<&AssignmentEnrolment>,
    dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
) {
    let Some(enrolment) = enrolment else {
        return;
    };
    enrolment.cancel.cancel();
    if !enrolment.client.is_enrolled() {
        return;
    }
    match enrolment.client.deregister().await {
        Ok(0) => tracing::info!("withdrew this chat session from the daemon assignment registry"),
        Ok(discarded) => surface_session_message(
            dispatcher,
            redraw_tx,
            &format!(
                "Withdrew this chat from the daemon; {discarded} assignment(s) it had not finished were \
                 handed back as cancelled."
            ),
        ),
        Err(error) => tracing::warn!(
            error = %error,
            "could not withdraw this chat session from the daemon assignment registry; it will be listed \
             as silent until an operator reaps it"
        ),
    }
}

/// Whether an action is one of the three `--daemon`-scoped variants.
#[cfg(feature = "terminal-tui")]
const fn is_daemon_session_command(action: &crate::chat::sessions::SessionCommand) -> bool {
    matches!(
        action,
        crate::chat::sessions::SessionCommand::DaemonSessions
            | crate::chat::sessions::SessionCommand::DaemonKill { .. }
            | crate::chat::sessions::SessionCommand::DaemonSteer { .. }
    )
}

#[cfg(feature = "terminal-tui")]
fn defer_resume_saved_session_if_provider_turn_pending(
    pending_turns: usize,
    deferred: &mut std::collections::VecDeque<String>,
    id: String,
) -> bool {
    if pending_turns == 0 {
        return false;
    }
    deferred.push_back(id);
    true
}

#[cfg(feature = "terminal-tui")]
const fn should_continue_event_pump_after_input_closed(pending_turns: usize) -> bool {
    pending_turns > 0
}

#[cfg(feature = "terminal-tui")]
fn should_drain_deferred_resume_after_visible_inputs(
    pending_turns: usize,
    backlog: &std::collections::VecDeque<QueuedInputMessage>,
    workers: &crate::chat::turn_worker::ProviderTurnWorkerRegistry,
) -> bool {
    pending_turns == 0
        && backlog.is_empty()
        && provider_turn_visible_admission(
            workers,
            crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
            1,
        )
        .can_start_visible
}

const fn consume_deferred_visible_input_pop(defer_visible_input_pop_once: &mut bool) -> bool {
    let should_defer = *defer_visible_input_pop_once;
    *defer_visible_input_pop_once = false;
    should_defer
}

const fn surface_turn_elapsed_message(
    _dispatcher: &dispatcher::ChatDispatcher,
    _redraw_tx: Option<&mpsc::Sender<()>>,
    _status: &str,
    _started_at: chrono::DateTime<chrono::Utc>,
    _finished_at: chrono::DateTime<chrono::Utc>,
) {
    // Completion timing remains available in structured state and diagnostics,
    // but it is intentionally not emitted into the conversation transcript.
}

fn format_fallback_chat_output_for(text: &str, stdout_is_terminal: bool) -> String {
    if !stdout_is_terminal {
        let mut out = String::with_capacity(text.len() + 1);
        out.push_str(text);
        out.push('\n');
        return out;
    }

    let mut out = String::with_capacity(text.len() + 2);
    let mut prev_was_cr = false;
    for ch in text.chars() {
        if ch == '\n' && !prev_was_cr {
            out.push('\r');
        }
        out.push(ch);
        prev_was_cr = ch == '\r';
    }
    out.push_str("\r\n");
    out
}

/// Run mode of the chat input path.
///
/// After the v0.4.1 cleanup the terminal TUI only supports the single reducer/driver route. The old
/// Off/Both/Redux staged-rollout modes were retired once v0.4.0 was accepted; `PRX_CHAT_REDUX` is
/// still read once to warn, but it no longer changes the run path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "terminal-tui")]
pub(crate) enum ReduxMode {
    Pure,
}

#[cfg(feature = "terminal-tui")]
impl ReduxMode {
    /// Read the retired `PRX_CHAT_REDUX` env only to warn operators when a
    /// stale deployment still tries to select a legacy mode.
    fn from_env() -> Self {
        if let Ok(raw) = std::env::var("PRX_CHAT_REDUX") {
            let value = raw.trim();
            if !value.is_empty() && !value.eq_ignore_ascii_case("pure") && value != "2" {
                tracing::warn!(
                    requested = value,
                    "PRX_CHAT_REDUX legacy modes are retired; using Pure reducer path"
                );
            }
        }
        Self::Pure
    }
}

fn autosave_memory_key(prefix: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{prefix}:{ts}")
}

/// Extract the raw chat session id from the legacy `chat:{id}` durable key.
///
/// D4 C6: the durable `session_key` is migrated to a stable canonical derived
/// from the immutable session id, so the helpers need the bare id. Defensive:
/// if the key is not in `chat:{id}` form it is returned unchanged (the canonical
/// derivation then keys on the whole string, still stable).
fn chat_session_id_from_key(chat_session_key: &str) -> &str {
    chat_session_key.strip_prefix("chat:").unwrap_or(chat_session_key)
}

/// Build a chat message-event write scope on the stable durable-canonical
/// `session_key` (D4 C6).
///
/// The durable `session_key` is the recipient-aware canonical derived from the
/// session id (`chat:terminal:local-user:{id}`), NOT the legacy `chat:{id}`, and
/// NOT `{provider}/{model}` (which would split one conversation across model
/// switches). provider/model is recorded on the event's `recipient` field only —
/// it does not feed the durable `session_key`. The legacy `chat:{id}` key is
/// carried for read-merge so pre-cutover history stays visible.
fn chat_message_event_scope(
    workspace_id: &str,
    chat_session_key: &str,
    chat_run_id: &str,
    provider_name: &str,
    model_name: &str,
) -> MessageEventScope {
    let chat_session_id = chat_session_id_from_key(chat_session_key);
    RuntimeEnvelope::chat_canonical(workspace_id, chat_session_id, MemoryVisibility::Workspace)
        .with_recipient(format!("{provider_name}/{model_name}"))
        .with_run_id(chat_run_id.to_string())
        .message_scope()
}

/// Build the chat read envelope on the same stable durable-canonical
/// `session_key` as the write scope (D4 C6), carrying the legacy `chat:{id}` key
/// for read-merge so the read principal unions canonical + legacy history.
fn chat_runtime_envelope(workspace_id: &str, chat_session_key: &str) -> RuntimeEnvelope {
    let chat_session_id = chat_session_id_from_key(chat_session_key);
    RuntimeEnvelope::chat_canonical(workspace_id.to_string(), chat_session_id, MemoryVisibility::Workspace)
        .with_recipient("terminal:user")
}

#[cfg(test)]
async fn record_chat_user_message_event(
    memory_fabric: &MemoryFabric,
    chat_session: &session::ChatSession,
    chat_session_key: &str,
    chat_run_id: &str,
    provider_name: &str,
    model_name: &str,
    turn_seq: u64,
    user_input: &str,
) -> anyhow::Result<crate::memory::MessageEvent> {
    record_chat_user_message_event_in_generation(
        memory_fabric,
        chat_session,
        chat_session_key,
        chat_run_id,
        provider_name,
        model_name,
        turn_seq,
        user_input,
        None,
    )
    .await
}

#[cfg(test)]
async fn record_chat_user_message_event_in_generation(
    memory_fabric: &MemoryFabric,
    chat_session: &session::ChatSession,
    chat_session_key: &str,
    chat_run_id: &str,
    provider_name: &str,
    model_name: &str,
    turn_seq: u64,
    user_input: &str,
    generation: Option<&crate::config::ConfigGeneration>,
) -> anyhow::Result<crate::memory::MessageEvent> {
    let mut scope = chat_message_event_scope(
        memory_fabric.workspace_id(),
        chat_session_key,
        chat_run_id,
        provider_name,
        model_name,
    );
    if let Some(generation) = generation {
        scope.config_generation_id = Some(generation.id.0);
        scope.config_source_revision = Some(generation.source_revision.fingerprint_sha256.clone());
    }
    memory_fabric
        .record_inbound_user_message(
            scope,
            sanitize::sanitize_for_persistence(user_input),
            Some(format!("chat:{}:{chat_run_id}:{turn_seq}:user", chat_session.id)),
            None,
        )
        .await
}

async fn record_route_decision_event(
    fabric: &MemoryFabric,
    scope: MessageEventScope,
    decision: &RouteDecision,
) -> anyhow::Result<crate::memory::MessageEvent> {
    let sanitized = sanitize::sanitize_json_structure(decision)?;
    record_raw_route_decision_event(fabric, scope, &sanitized).await
}

#[allow(clippy::too_many_arguments)]
async fn finalize_chat_success_turn(
    turn_binding: Option<&decision_source::ChatTurnBinding>,
    fabric: &MemoryFabric,
    _chat_session_key: &str,
    chat_run_id: &str,
    provider_name: &str,
    model_name: &str,
    _route_scope: MessageEventScope,
    provider_outcome: ProviderExecutionOutcome,
    recorded_response: Option<String>,
    history_commit_len: usize,
    summary: &str,
    cost_config: &crate::config::schema::CostConfig,
    workspace_dir: &std::path::Path,
) -> anyhow::Result<crate::agent::terminal::TurnTerminalReceipt> {
    let binding = turn_binding.context("chat terminal binding is missing")?;
    let decision = binding
        .settlement_ref(chat_run_id)?
        .context("chat terminal decision has not been issued")?;
    let route_scope = binding.envelope().message_scope();

    // `finalize_turn` redacts the reply and the provider outcome before it
    // persists them, for chat as for every other surface.
    let history = recorded_response.map(|assistant_content| crate::agent::terminal::TurnHistoryProjection {
        assistant_content,
        history_commit_len,
    });
    let history_scope = history.as_ref().map(|_| {
        route_scope
            .clone()
            .with_sender(format!("{provider_name}/{model_name}"))
            .with_recipient("local-user")
    });
    let receipt = binding
        .commit_terminal(
            fabric,
            crate::agent::terminal::TurnTerminalDraft {
                decision: Some(decision),
                terminal_id: chat_run_id.to_string(),
                scope: route_scope,
                status: crate::agent::terminal::TurnTerminalStatus::Completed,
                history,
                history_scope,
                provider_outcome: Some(provider_outcome.clone()),
                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                    summary: summary.to_string(),
                    started_at: provider_outcome.started_at,
                    finished_at: provider_outcome.finished_at,
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Reply {
                    target: "terminal:local-user".to_string(),
                },
            },
            cost_config,
            workspace_dir,
        )
        .await?;
    binding.rejoin_terminal(chat_run_id).await?;
    Ok(receipt)
}

#[allow(clippy::too_many_arguments)]
async fn finalize_chat_non_success_turn(
    turn_binding: Option<&decision_source::ChatTurnBinding>,
    fabric: &MemoryFabric,
    chat_run_id: &str,
    _route_scope: MessageEventScope,
    status: crate::agent::terminal::TurnTerminalStatus,
    provider_outcome: Option<ProviderExecutionOutcome>,
    started_at: chrono::DateTime<chrono::Utc>,
    summary: &str,
    cost_config: &crate::config::schema::CostConfig,
    workspace_dir: &std::path::Path,
) -> anyhow::Result<()> {
    let binding = turn_binding.context("chat terminal binding is missing")?;
    let Some(decision) = binding.settlement_ref(chat_run_id)? else {
        anyhow::ensure!(
            provider_outcome.is_none(),
            "pre-decision failure cannot settle a provider outcome"
        );
        return binding
            .record_predecision_failure(fabric, chat_run_id, status, summary)
            .await;
    };
    let route_scope = binding.envelope().message_scope();

    // `finalize_turn` redacts the provider outcome (with its error text)
    // before it persists it.
    let finished_at = provider_outcome
        .as_ref()
        .map_or_else(chrono::Utc::now, |outcome| outcome.finished_at);
    binding
        .commit_terminal(
            fabric,
            crate::agent::terminal::TurnTerminalDraft {
                decision: Some(decision),
                terminal_id: chat_run_id.to_string(),
                scope: route_scope,
                status,
                history: None,
                history_scope: None,
                provider_outcome,
                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                    summary: summary.to_string(),
                    started_at,
                    finished_at,
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Suppress {
                    reason: summary.to_string(),
                },
            },
            cost_config,
            workspace_dir,
        )
        .await?;
    binding.rejoin_terminal(chat_run_id).await?;
    Ok(())
}

fn sanitize_chat_semantic_memory_content(content: &str) -> String {
    sanitize::sanitize_for_persistence(content)
}

/// Aggregate the model's reasoning/thinking content from the turn's history
/// slice. The agent loop encodes assistant turns that carried reasoning as a
/// JSON object containing `{"reasoning_content": "..."}` (see
/// `build_native_assistant_history` in `agent/history.rs`). We pull those out
/// and join them with blank-line separators so the TUI can render a single
/// foldable card per turn.
///
/// Returns an empty string when no reasoning is present, signalling the
/// caller to skip pushing a card.
#[cfg(feature = "terminal-tui")]
fn collect_reasoning_from_history_slice(slice: &[ChatMessage]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for msg in slice {
        if msg.role != "assistant" {
            continue;
        }
        // Fast pre-filter to skip plain-text assistant turns without paying
        // the JSON parse cost.
        if !msg.content.contains("reasoning_content") {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&msg.content) else {
            continue;
        };
        if let Some(rc) = parsed.get("reasoning_content").and_then(serde_json::Value::as_str) {
            let trimmed = rc.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }
    }
    parts.join("\n\n")
}

// ── P3-2 / alt-screen Phase 0: TerminalGuard RAII + panic restore ────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "terminal-tui")]
struct ChatTuiSelection {
    enabled: bool,
}

#[cfg(feature = "terminal-tui")]
fn select_chat_tui(plain_mode: bool, stdin_is_terminal: bool, prx_tui_env: Option<&str>) -> ChatTuiSelection {
    let enabled = should_enable_terminal_tui(plain_mode, stdin_is_terminal, prx_tui_env);
    ChatTuiSelection { enabled }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalGuardState {
    raw_mode_active: bool,
    bracketed_paste_active: bool,
    keyboard_enhancement_active: bool,
    mouse_capture_active: bool,
    alternate_screen_active: bool,
}

impl TerminalGuardState {
    #[cfg(feature = "terminal-tui")]
    const fn inactive() -> Self {
        Self {
            raw_mode_active: false,
            bracketed_paste_active: false,
            keyboard_enhancement_active: false,
            mouse_capture_active: false,
            alternate_screen_active: false,
        }
    }
}

trait TerminalModeOps {
    #[cfg(feature = "terminal-tui")]
    fn enable_raw_mode(&mut self) -> std::io::Result<()>;
    fn disable_raw_mode(&mut self) -> std::io::Result<()>;
    #[cfg(feature = "terminal-tui")]
    fn supports_keyboard_enhancement(&mut self) -> std::io::Result<bool>;
    #[cfg(feature = "terminal-tui")]
    fn push_keyboard_enhancement_flags(&mut self) -> std::io::Result<()>;
    fn pop_keyboard_enhancement_flags(&mut self) -> std::io::Result<()>;
    #[cfg(feature = "terminal-tui")]
    fn enable_bracketed_paste(&mut self) -> std::io::Result<()>;
    fn disable_bracketed_paste(&mut self) -> std::io::Result<()>;
    #[cfg(feature = "terminal-tui")]
    fn enable_mouse_capture(&mut self) -> std::io::Result<()>;
    fn disable_mouse_capture(&mut self) -> std::io::Result<()>;
    #[cfg(feature = "terminal-tui")]
    fn enter_alternate_screen(&mut self) -> std::io::Result<()>;
    fn leave_alternate_screen(&mut self) -> std::io::Result<()>;
    fn show_cursor(&mut self) -> std::io::Result<()>;
}

struct CrosstermTerminalModeOps;

#[cfg(feature = "terminal-tui")]
fn chat_keyboard_enhancement_flags() -> crossterm::event::KeyboardEnhancementFlags {
    // Crossterm 0.29 does not expose kitty's REPORT_ASSOCIATED_TEXT flag.
    // REPORT_ALL_KEYS_AS_ESCAPE_CODES without associated text causes IME
    // commits (for example, Chinese input in iTerm2) to lose their text.
    // Keep ordinary text as UTF-8 while retaining the enhancements needed for
    // unambiguous shortcuts and key press/release handling.
    crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | crossterm::event::KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        | crossterm::event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES
}

impl TerminalModeOps for CrosstermTerminalModeOps {
    #[cfg(feature = "terminal-tui")]
    fn enable_raw_mode(&mut self) -> std::io::Result<()> {
        crossterm::terminal::enable_raw_mode()
    }

    fn disable_raw_mode(&mut self) -> std::io::Result<()> {
        crossterm::terminal::disable_raw_mode()
    }

    #[cfg(feature = "terminal-tui")]
    fn supports_keyboard_enhancement(&mut self) -> std::io::Result<bool> {
        crossterm::terminal::supports_keyboard_enhancement()
    }

    #[cfg(feature = "terminal-tui")]
    fn push_keyboard_enhancement_flags(&mut self) -> std::io::Result<()> {
        let flags = chat_keyboard_enhancement_flags();
        crossterm::execute!(std::io::stdout(), crossterm::event::PushKeyboardEnhancementFlags(flags)).map(|_| ())
    }

    fn pop_keyboard_enhancement_flags(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::PopKeyboardEnhancementFlags).map(|_| ())
    }

    #[cfg(feature = "terminal-tui")]
    fn enable_bracketed_paste(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste).map(|_| ())
    }

    fn disable_bracketed_paste(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste).map(|_| ())
    }

    #[cfg(feature = "terminal-tui")]
    fn enable_mouse_capture(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture).map(|_| ())
    }

    fn disable_mouse_capture(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture).map(|_| ())
    }

    #[cfg(feature = "terminal-tui")]
    fn enter_alternate_screen(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen).map(|_| ())
    }

    fn leave_alternate_screen(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen).map(|_| ())
    }

    fn show_cursor(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::cursor::Show).map(|_| ())
    }
}

static CHAT_FULLSCREEN_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CHAT_KEYBOARD_ENHANCEMENT_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CHAT_MOUSE_CAPTURE_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(feature = "terminal-tui")]
fn enter_terminal_state_with_ops(ops: &mut impl TerminalModeOps) -> std::io::Result<TerminalGuardState> {
    let mouse_enabled = mouse_capture_enabled_by_env(std::env::var("PRX_TUI_DISABLE_MOUSE").ok().as_deref());
    enter_terminal_state_with_ops_inner(ops, mouse_enabled)
}

#[cfg(feature = "terminal-tui")]
fn enter_terminal_state_with_ops_inner(
    ops: &mut impl TerminalModeOps,
    mouse_enabled: bool,
) -> std::io::Result<TerminalGuardState> {
    let mut state = TerminalGuardState::inactive();

    ops.enable_raw_mode()?;
    state.raw_mode_active = true;

    CHAT_FULLSCREEN_ACTIVE.store(true, std::sync::atomic::Ordering::Release);
    if let Err(e) = ops.enter_alternate_screen() {
        CHAT_FULLSCREEN_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
        let _ = ops.disable_raw_mode();
        return Err(e);
    }
    state.alternate_screen_active = true;

    if mouse_enabled {
        if let Err(e) = ops.enable_mouse_capture() {
            if state.alternate_screen_active {
                let _ = ops.leave_alternate_screen();
                CHAT_FULLSCREEN_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
            }
            let _ = ops.disable_raw_mode();
            return Err(e);
        }
        state.mouse_capture_active = true;
        CHAT_MOUSE_CAPTURE_ACTIVE.store(true, std::sync::atomic::Ordering::Release);
    }

    match ops.supports_keyboard_enhancement() {
        Ok(true) => {
            if let Err(e) = ops.push_keyboard_enhancement_flags() {
                if state.mouse_capture_active {
                    let _ = ops.disable_mouse_capture();
                    CHAT_MOUSE_CAPTURE_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
                }
                if state.alternate_screen_active {
                    let _ = ops.leave_alternate_screen();
                    CHAT_FULLSCREEN_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
                }
                let _ = ops.disable_raw_mode();
                return Err(e);
            }
            state.keyboard_enhancement_active = true;
            CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.store(true, std::sync::atomic::Ordering::Release);
        }
        Ok(false) => {}
        Err(e) => {
            tracing::debug!(error = %e, "terminal keyboard enhancement probe failed; skipping");
        }
    }

    if let Err(e) = ops.enable_bracketed_paste() {
        if state.keyboard_enhancement_active {
            let _ = ops.pop_keyboard_enhancement_flags();
            CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
        }
        if state.mouse_capture_active {
            let _ = ops.disable_mouse_capture();
            CHAT_MOUSE_CAPTURE_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
        }
        if state.alternate_screen_active {
            let _ = ops.leave_alternate_screen();
            CHAT_FULLSCREEN_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
        }
        let _ = ops.disable_raw_mode();
        return Err(e);
    }
    state.bracketed_paste_active = true;

    Ok(state)
}

fn leave_terminal_state_with_ops(ops: &mut impl TerminalModeOps, state: TerminalGuardState) {
    if state.bracketed_paste_active {
        let _ = ops.disable_bracketed_paste();
        let _ = ops.show_cursor();
    }
    if state.keyboard_enhancement_active {
        let _ = ops.pop_keyboard_enhancement_flags();
        CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
    }
    if state.mouse_capture_active {
        let _ = ops.disable_mouse_capture();
        CHAT_MOUSE_CAPTURE_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
    }
    if state.alternate_screen_active {
        let _ = ops.leave_alternate_screen();
        CHAT_FULLSCREEN_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
    }
    if state.raw_mode_active {
        let _ = ops.disable_raw_mode();
    }
}

fn restore_terminal_state_with_ops(ops: &mut impl TerminalModeOps, leave_alternate_screen: bool) {
    let state = TerminalGuardState {
        raw_mode_active: true,
        bracketed_paste_active: true,
        keyboard_enhancement_active: CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.swap(false, std::sync::atomic::Ordering::AcqRel),
        mouse_capture_active: CHAT_MOUSE_CAPTURE_ACTIVE.swap(false, std::sync::atomic::Ordering::AcqRel),
        alternate_screen_active: leave_alternate_screen,
    };
    leave_terminal_state_with_ops(ops, state);
}

/// Best-effort terminal restoration used by both [`TerminalGuard`] and the chat
/// panic hook installed via [`install_chat_panic_hook`].
///
/// Fullscreen mode records a process-global active flag so panic restore can
/// emit `LeaveAlternateScreen` before the chained hook prints its backtrace.
fn restore_terminal_state() {
    let leave_alternate_screen = CHAT_FULLSCREEN_ACTIVE.swap(false, std::sync::atomic::Ordering::AcqRel);
    let mut ops = CrosstermTerminalModeOps;
    restore_terminal_state_with_ops(&mut ops, leave_alternate_screen);
}

/// RAII guard for the chat TUI terminal state.
///
/// The fullscreen TUI lifecycle is raw mode + alternate screen + bracketed
/// paste. `enter()` is transactional: any partial failure rolls back
/// already-applied terminal state before returning `Err`.
pub struct TerminalGuard {
    raw_mode_active: std::sync::atomic::AtomicBool,
    bracketed_paste_active: std::sync::atomic::AtomicBool,
    keyboard_enhancement_active: std::sync::atomic::AtomicBool,
    mouse_capture_active: std::sync::atomic::AtomicBool,
    alternate_screen_active: std::sync::atomic::AtomicBool,
}

impl TerminalGuard {
    #[cfg(feature = "terminal-tui")]
    pub(crate) fn enter() -> Result<Self> {
        let mut ops = CrosstermTerminalModeOps;
        let state = enter_terminal_state_with_ops(&mut ops)
            .map_err(|e| anyhow::anyhow!("failed to enter chat fullscreen TUI terminal mode: {e}"))?;
        Ok(Self {
            raw_mode_active: std::sync::atomic::AtomicBool::new(state.raw_mode_active),
            bracketed_paste_active: std::sync::atomic::AtomicBool::new(state.bracketed_paste_active),
            keyboard_enhancement_active: std::sync::atomic::AtomicBool::new(state.keyboard_enhancement_active),
            mouse_capture_active: std::sync::atomic::AtomicBool::new(state.mouse_capture_active),
            alternate_screen_active: std::sync::atomic::AtomicBool::new(state.alternate_screen_active),
        })
    }

    /// Manual early teardown (e.g. before spawning a child process that needs a
    /// clean terminal). Idempotent across manual calls and Drop.
    pub fn leave(&self) {
        let mut ops = CrosstermTerminalModeOps;
        let state = TerminalGuardState {
            bracketed_paste_active: self
                .bracketed_paste_active
                .compare_exchange(
                    true,
                    false,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok(),
            mouse_capture_active: self
                .mouse_capture_active
                .compare_exchange(
                    true,
                    false,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok(),
            keyboard_enhancement_active: self
                .keyboard_enhancement_active
                .compare_exchange(
                    true,
                    false,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok(),
            alternate_screen_active: self
                .alternate_screen_active
                .compare_exchange(
                    true,
                    false,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok(),
            raw_mode_active: self
                .raw_mode_active
                .compare_exchange(
                    true,
                    false,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok(),
        };
        if state.alternate_screen_active {
            CHAT_FULLSCREEN_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
        }
        leave_terminal_state_with_ops(&mut ops, state);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.leave();
    }
}

#[cfg(feature = "terminal-tui")]
fn should_enable_terminal_tui(plain_mode: bool, stdin_is_terminal: bool, prx_tui_env: Option<&str>) -> bool {
    let tui_opt_out = prx_tui_env == Some("0");
    !plain_mode && !tui_opt_out && stdin_is_terminal
}

/// Install the chat-specific panic hook.
///
/// The hook restores the terminal (show cursor, leave alternate screen,
/// disable raw mode) **before** delegating to the previously installed hook
/// — so backtraces print to a usable terminal instead of a "bricked" raw-mode
/// alternate buffer.
///
/// A `OnceLock` guards against multiple concurrent panics each trying to run
/// the restoration sequence: only the first panic actually issues the
/// crossterm calls, subsequent panics skip straight to the chained backtrace
/// printer. This avoids fighting with `TerminalGuard::leave` (which may also
/// be running on the unwinding thread) and with each other.
///
/// Safe to call multiple times — only the first call installs the hook; later
/// calls return without rewrapping (avoids unbounded nesting and the original
/// hook being lost behind layers of restoration calls).
fn install_chat_panic_hook() {
    use std::sync::OnceLock;
    static INSTALLED: OnceLock<()> = OnceLock::new();
    if INSTALLED.set(()).is_err() {
        // Already installed by a previous call — do not stack another layer.
        return;
    }

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Defence in depth: even if a `TerminalGuard` is unwinding through
        // Drop on the panicking thread, this runs first (panic hook fires
        // before stack unwind) so the terminal is usable when the chained
        // hook prints the backtrace.
        //
        // A `OnceLock` ensures restoration runs at most once per process
        // even if multiple threads panic concurrently — preventing
        // interleaved escape sequences.
        static RESTORED: OnceLock<()> = OnceLock::new();
        if RESTORED.set(()).is_ok() {
            restore_terminal_state();
        }
        original_hook(info);
    }));
}

// ── P3-1: Redirect tracing to ~/.openprx/chat.log during chat ────────────

/// `main` redirects chat tracing before loading configuration so startup
/// diagnostics are not left behind the alternate screen and revealed on exit.
/// `chat::run` also calls `ensure_chat_tracing_to_file` for embedders that do
/// not enter through the binary bootstrap; this flag prevents a nested reload.
static CHAT_TRACING_TO_FILE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// RAII guard owning the `tracing_appender` non-blocking worker. When dropped
/// it:
///   1. swaps the global tracing writer back to stderr (best-effort), so
///      any post-chat logs (e.g. shutdown errors) remain visible;
///   2. drops `_worker_guard`, which flushes and joins the appender thread.
///
/// Held for the lifetime of `chat::run`. If construction fails we keep the
/// existing stderr writer — no panics, no silent data loss.
pub struct TracingChatGuard {
    _worker_guard: tracing_appender::non_blocking::WorkerGuard,
}

impl Drop for TracingChatGuard {
    fn drop(&mut self) {
        if let Some(handle) = crate::CHAT_TRACING_RELOAD.get() {
            let stderr_writer = tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stderr);
            let layer = tracing_subscriber::fmt::Layer::default().with_writer(stderr_writer);
            if let Err(e) = handle.reload(layer) {
                eprintln!("warning: failed to restore stderr tracing writer: {e}");
            }
        }
        CHAT_TRACING_TO_FILE_ACTIVE.store(false, Ordering::Release);
        // _worker_guard drops next → tracing-appender flushes pending lines.
    }
}

/// Compute `~/.openprx/` (or `$HOME/.openprx/`) for the chat log directory.
fn resolve_chat_log_dir() -> Result<std::path::PathBuf> {
    crate::config::default_openprx_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory for chat.log"))
}

/// Redirect the global `tracing` writer to `~/.openprx/chat.log` so
/// `INFO`/`WARN`/`ERROR` lines never collide with the ratatui TUI.
///
/// Returns a guard that MUST be held for the lifetime of the chat session.
/// On any failure (no HOME, dir not creatable, file not openable, no global
/// reload handle) returns `Err` without panicking — callers fall back to the
/// existing stderr writer.
pub fn setup_chat_tracing_to_file() -> Result<TracingChatGuard> {
    setup_chat_tracing_to_file_in(&resolve_chat_log_dir()?)
}

/// Test-friendly variant: redirects tracing to `<dir>/chat.log`. The directory
/// is created with `create_dir_all` if missing; the file is opened in append
/// mode so repeated chat invocations within one user session don't truncate
/// earlier logs.
pub(crate) fn setup_chat_tracing_to_file_in(dir: &std::path::Path) -> Result<TracingChatGuard> {
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("failed to create chat log directory {}: {e}", dir.display()))?;
    let log_path = dir.join("chat.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| anyhow::anyhow!("failed to open {} for writing: {e}", log_path.display()))?;

    let (non_blocking, worker_guard) = tracing_appender::non_blocking(file);
    let file_writer = tracing_subscriber::fmt::writer::BoxMakeWriter::new(non_blocking);
    // ANSI escape codes are useless (and noisy) inside a log file.
    let layer = tracing_subscriber::fmt::Layer::default()
        .with_writer(file_writer)
        .with_ansi(false);

    let handle = crate::CHAT_TRACING_RELOAD
        .get()
        .ok_or_else(|| anyhow::anyhow!("tracing reload handle not initialized (non-chat command?)"))?;
    handle
        .reload(layer)
        .map_err(|e| anyhow::anyhow!("failed to redirect tracing to chat.log: {e}"))?;

    CHAT_TRACING_TO_FILE_ACTIVE.store(true, Ordering::Release);

    Ok(TracingChatGuard {
        _worker_guard: worker_guard,
    })
}

/// Redirect tracing unless the binary bootstrap already owns the redirect.
fn ensure_chat_tracing_to_file() -> Result<Option<TracingChatGuard>> {
    if CHAT_TRACING_TO_FILE_ACTIVE.load(Ordering::Acquire) {
        return Ok(None);
    }
    setup_chat_tracing_to_file().map(Some)
}

/// Seed the reducer with the exact durable chat session before any turn can be
/// admitted. This must also run for a brand-new session with zero turns: leaving
/// `ChatState::new`'s placeholder id in place lets the first Redux save rename
/// the conversation, which splits message-event provenance across session keys.
fn initial_chat_dispatcher_state(
    provider_name: &str,
    model_name: &str,
    shutdown: CancellationToken,
    chat_session: &session::ChatSession,
) -> state::ChatState {
    let mut state = state::ChatState::new(Arc::from(provider_name), Arc::from(model_name), shutdown);
    let _ = state.reduce(crate::chat::action::Action::SessionLoaded(chat_session.clone()));
    state
}

/// Check a `/model <name>` switch on the current provider: resolved for chat
/// through the one selection check, so a model that cannot chat is refused
/// before the next turn uses it; an id-only model switches with a warning.
fn chat_model_switch(config: &Config, provider_name: &str, model_name: &str) -> Result<()> {
    providers::selection::check(
        config,
        provider_name,
        model_name,
        &providers::catalog::Requirement::chat(),
        providers::catalog::UnknownPolicy::Invocation,
    )?;
    Ok(())
}

/// Build the chat provider chain for `provider_name`.
///
/// Shared by startup and `/provider`. The selection and every model route
/// are resolved for chat first, so a model that cannot chat is refused before
/// any provider exists. The root credential and base URL are released only
/// when the selection is the configured provider's identity (any spelling of
/// it, `providers::configured_connection_for`), and the rotation keys in
/// `reliability.api_keys` follow them, because they are extra credentials for
/// that same root key.
pub(crate) fn chat_provider_chain<'a>(
    config: &'a Config,
    provider_name: &str,
    configured_provider_name: &str,
    model_name: &str,
    options: &providers::ProviderRuntimeOptions,
) -> Result<(providers::ProviderConnection<'a>, Box<dyn Provider>)> {
    let catalog = providers::selection::catalog_for(config);
    let resolved = providers::selection::check_with(
        config,
        &catalog,
        provider_name,
        model_name,
        providers::selection::credential_for(config, provider_name),
        &providers::catalog::Requirement::chat(),
        providers::catalog::UnknownPolicy::Invocation,
    )?;
    let connection = providers::configured_connection_for(
        provider_name,
        configured_provider_name,
        config.api_key.as_deref(),
        config.api_url.as_deref(),
    );
    let provider = providers::create_routed_provider_for(&resolved, &catalog, config, options)?;
    Ok((connection, provider))
}

/// The one-line banner every chat session opens with:
/// `prx <version> · provider/model`. A `custom:` URL may carry
/// `user:password@`, so the provider is printed through the one redaction
/// helper.
fn startup_banner(provider_name: &str, model_name: &str) -> String {
    format!(
        "prx {} \u{00B7} {}/{model_name}",
        env!("CARGO_PKG_VERSION"),
        providers::redact_endpoint_userinfo(provider_name)
    )
}

/// Run the interactive chat session with rich terminal UI.
#[allow(clippy::too_many_lines)]
pub async fn run(
    config: Config,
    provider_override: Option<String>,
    model_override: Option<String>,
    temperature: f64,
    plain_mode: bool,
    session_id: Option<String>,
    list_sessions: bool,
    shutdown: CancellationToken,
) -> Result<()> {
    // ── P3-1: Redirect tracing to ~/.openprx/chat.log ────────────────────
    // Held for the rest of this function. On failure (no HOME, log dir
    // unwritable, etc.) we fall back to the stderr writer that `main` set up
    // and emit a warning. Never panics.
    let _tracing_guard: Option<TracingChatGuard> = match ensure_chat_tracing_to_file() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(error = %e, "P3-1: keeping tracing on stderr (chat.log unavailable)");
            None
        }
    };

    // ── Panic hook: restore terminal state on crash ─────────────────────
    // Strengthened in P3-2: restoration runs before the chained hook so
    // backtraces print to a usable terminal. Idempotent across multiple
    // calls (OnceLock-guarded).
    install_chat_panic_hook();

    // P3-2 prep: `TerminalGuard::enter()` is intentionally NOT invoked from
    // this function yet. It will be wired up in P3-3 once the ratatui draw
    // loop replaces the reedline / UiActor path. The type is defined and the
    // panic hook is strengthened ahead of time so P3-3 can plug in without
    // any scaffolding changes — see `TerminalGuard` above.

    // ── Wire up subsystems via RuntimeBootstrap (D1 step 2) ──────
    // `list_sessions` is a CLI flag known before any subsystem is built, so we
    // pick the profile once: `MemoryOnly` early-exits after memory (no tools),
    // `Interactive` builds the full memory + tools set. Either way memory is
    // built exactly once — no duplicate construction. The bootstrap wires
    // observer → security(+audit) → memory → runtime → tools in the hard-ordered
    // sequence, replacing the former hand-wired block (behaviour-equivalent).
    let profile = if list_sessions {
        crate::runtime::bootstrap::BootstrapProfile::MemoryOnly
    } else {
        crate::runtime::bootstrap::BootstrapProfile::Interactive
    };
    let ctx = crate::runtime::bootstrap::RuntimeBootstrap::build(config, profile).await?;

    // `build` took ownership of `config`; reclaim a shared `Arc<Config>` for the
    // rest of this function. `Arc<Config>` deref-coerces to `&Config`, so almost
    // all `config.xxx` accesses below are unchanged.
    let config = Arc::clone(&ctx.config);

    let observer = Arc::clone(&ctx.observer);
    let security = Arc::clone(&ctx.security);
    let hooks = Arc::clone(&ctx.hooks);

    // ── Memory ───────────────────────────────────────────────────
    // Both MemoryOnly and Interactive profiles build memory, so it is always
    // Some here; take it explicitly without panicking (iron rules 1/6).
    let mem: Arc<dyn Memory> = ctx
        .memory
        .clone()
        .ok_or_else(|| anyhow::anyhow!("bootstrap did not build a memory backend for chat"))?;
    let memory_fabric = MemoryFabric::new(mem.clone(), config.workspace_dir.to_string_lossy())
        .with_event_recording(config.memory.event_recording_config());

    // ── List sessions (early return) ─────────────────────────────
    // Early-exit before tools/provider/TUI are needed (MemoryOnly profile never
    // built them), preserving the original `--list-sessions` semantics.
    if list_sessions {
        return list_saved_sessions(mem.as_ref()).await;
    }

    // ── Tools ────────────────────────────────────────────────────
    // The Interactive profile built the base tool registry inside the bootstrap
    // (security + runtime + memory all ready) and handed it to chat as an *owned*
    // `Vec` in `base_tools` (so chat can append its session tools after the
    // provider + TerminalChannel exist). Take it exactly once; we wrap it in
    // `Arc` ourselves once the chat session tools are appended (see "Chat session
    // runtime" below).
    let mut base_tools_vec: Vec<Box<dyn Tool>> = ctx
        .base_tools
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("bootstrap did not build the tool registry for chat"))?
        .lock()
        .take()
        .ok_or_else(|| anyhow::anyhow!("chat base tool registry was already taken"))?;

    // ── Resolve provider ─────────────────────────────────────────
    let configured_provider_name = providers::selection::default_provider(&config);
    let provider_name = provider_override.as_deref().unwrap_or(configured_provider_name);

    let model_name = model_override
        .as_deref()
        .unwrap_or_else(|| providers::selection::default_model(&config));

    let provider_runtime_options = providers::provider_runtime_options_from_config(&config);

    let (startup_connection, startup_provider) = chat_provider_chain(
        &config,
        provider_name,
        configured_provider_name,
        model_name,
        &provider_runtime_options,
    )?;
    let provider: Arc<dyn Provider> = Arc::from(startup_provider);
    let mut provider_binding =
        decision_source::ChatProviderBinding::capture(&ctx.config_generation, &provider, provider_name, model_name)?;

    observer.record_event(&ObserverEvent::AgentStart {
        provider: provider_name.to_string(),
        model: model_name.to_string(),
    });
    hooks
        .emit(
            HookEvent::AgentStart,
            serde_json::json!({
                "provider": provider_name,
                "model": model_name,
            }),
        )
        .await;

    // ── Skills ────────────────────────────────────────────────────
    let skills = crate::skills::load_skills_with_config(&config.workspace_dir, &config);

    // Computed once, at startup, for the mode the chat TUI streams in. The
    // agent loop recomputes the same question per turn at the mode that turn
    // actually runs in, and the two can disagree — a failover chain whose
    // members are all non-streaming advertises no native tools here and native
    // tools there. So this flag selects prompt *wording* only; nothing that
    // depends on the per-turn mode, tool-call syntax above all, may be decided
    // from it. `TaskSection` is written to that rule.
    let native_tools = provider
        .capabilities_for(&model_name, crate::providers::traits::ProviderRequestMode::Streaming)
        .native_tool_calling;

    // ── Approval manager ─────────────────────────────────────────
    let approval_manager = Arc::new(ApprovalManager::new());

    // ── Create TerminalChannel (Arc-wrapped for sharing with streaming tasks) ──
    let terminal: Arc<TerminalChannel> = Arc::new(TerminalChannel::new(plain_mode));

    // ── Chat session runtime (v1a registry wiring) ──────────────────
    // The single source of truth for the chat background-session runtime: one
    // `active_runs` registry, owned here, shared by reference (Arc clones) with
    // the four session tools (sessions_spawn/list/status/send) and the chat-side
    // `ChatSessionsHandle` used by `/sessions` and `/kill`. This is the v1a
    // "step 0" foundation (see chat-background-runtime-v1-execution-plan.md §C.0).
    //
    // Built here — after provider + TerminalChannel exist — because
    // `SessionsSpawnTool::new_with_registry` requires both; the generic tool
    // factory (`all_tools_with_runtime`) runs in bootstrap before either is
    // available, so chat appends these tools to its owned base registry instead.
    let active_runs: Arc<tokio::sync::RwLock<Vec<crate::tools::sessions_spawn::SubAgentRun>>> =
        Arc::new(tokio::sync::RwLock::new(Vec::new()));
    let sessions_workspace_id = config.workspace_dir.to_string_lossy().to_string();
    // Event bridge (v1.1a): the chat main loop owns the single `SessionEvent`
    // receiver; the spawn tool gets the matching library-level sink so task-mode
    // `/bg` sub-agents stream incremental output + tool calls (via a per-session
    // drainer) into per-session ring buffers for live read-only `/attach`.
    let (session_event_sink, mut session_event_rx) = crate::chat::sessions::SessionEventSink::channel();
    // Keep a clone for background `/shell` sessions (v2): they stream stdout/
    // stderr through the same event bridge / per-session drainer that agents use,
    // so live `/attach` and `/logs` work uniformly across both kinds. The other
    // clone is consumed by the spawn tool's `into_spawn_sink` below.
    let shell_event_sink = session_event_sink.clone();
    // Chat-side handle over the same single-source registries for `/sessions`,
    // `/kill`, and model-callable managed sessions. Build it before freezing the
    // tool registry so the LLM receives a handle to the same shell registry that
    // the TUI and slash commands render.
    let mut chat_sessions = crate::chat::sessions::ChatSessionsHandle::new(Arc::clone(&active_runs));
    let managed_shell_registry = chat_sessions.shell_registry();
    // NeedsInput (chat `/bg` only): shared pending-approval registry + per-run
    // resolver factory. When a background sub-agent hits the supervised approval
    // gate it suspends (NeedsInput) awaiting an operator `/approve` / `/deny`
    // decision instead of auto-failing. Only the chat path attaches this; the
    // channels/gateway spawn tools leave it `None` (auto-fail-on-gate preserved).
    let pending_approvals = crate::chat::sessions::PendingApprovals::new();
    let approval_resolver_factory = crate::chat::sessions::build_resolver_factory(
        session_event_sink.event_sender(),
        Arc::clone(&active_runs),
        pending_approvals.clone(),
        crate::chat::sessions::approval::DEFAULT_APPROVAL_TIMEOUT,
    );
    let spawn_tool = crate::tools::SessionsSpawnTool::new_with_registry(
        Arc::clone(&terminal) as Arc<dyn Channel>,
        Arc::clone(&provider),
        provider_name,
        model_name,
        temperature,
        security.clone(),
        config.workspace_dir.clone(),
        config.multimodal.clone(),
        config.agent.compaction.clone(),
        config.agents_with_references().into_owned(),
        // The spawn fallback is paired with `provider_name` (the `-p` override
        // when one is given), so it must be the credential released to that
        // provider, never the configured default's root key.
        startup_connection.api_key.map(str::to_string),
        provider_runtime_options.clone(),
        config.sessions_spawn.clone(),
        Arc::clone(&active_runs),
    )
    .with_compaction_resolver(crate::router::CompactionResolver::new(
        config.agent.compaction.clone(),
        config.router.clone(),
        config.model_routes.clone(),
    ))
    .with_cost_config(config.cost.clone())
    .with_reliability(config.reliability.clone())
    .with_configured_providers(&providers::ConfiguredProviders::from_config(&config))
    .with_model_catalog(providers::selection::catalog_for(&config))
    .with_hint_config(&config)
    .with_fallback_root_connection(startup_connection.released)
    .with_shared_memory(Arc::clone(&mem))
    .with_event_recording(config.memory.event_recording_config())
    .with_event_sink(session_event_sink.into_spawn_sink())
    .with_approval_resolver_factory(approval_resolver_factory)
    .with_process_recovery(config.causal_tree.enabled)?;
    let spawn_tools_handle = spawn_tool.tools_handle();
    let scheduled_input_handle = crate::chat::scheduled_input::ScheduledInputHandle::default();

    // Sibling tools share the same single-source registry (only the v1a four;
    // `subagents`/`sessions_history` are intentionally not registered in chat —
    // see plan §C.0 blocker 4).
    base_tools_vec.push(Box::new(
        crate::tools::SessionsListTool::new(Arc::clone(&active_runs))
            .with_shared_memory(Arc::clone(&mem), sessions_workspace_id.clone()),
    ));
    base_tools_vec.push(Box::new(
        crate::tools::SessionsSendTool::with_security(Arc::clone(&active_runs), security.clone())
            .with_shared_memory(Arc::clone(&mem))
            .with_event_recording(config.memory.event_recording_config()),
    ));
    base_tools_vec.push(Box::new(
        crate::tools::SessionStatusTool::new(
            Arc::clone(&active_runs),
            provider_name,
            model_name,
            vec![terminal.name().to_string()],
        )
        .with_shared_memory(Arc::clone(&mem), sessions_workspace_id),
    ));
    base_tools_vec.push(Box::new(spawn_tool));
    base_tools_vec.push(Box::new(crate::chat::managed_session::ManagedSessionTool::new(
        config.workspace_dir.clone(),
        managed_shell_registry,
        shell_event_sink.clone(),
    )));
    base_tools_vec.push(Box::new(crate::chat::scheduled_input::ScheduledInputTool::new(
        scheduled_input_handle.clone(),
    )));
    // Outbound IM. This process owns no channel object on purpose — a second
    // listener would race the daemon for inbound messages — so `message_send`
    // here is the variant that asks the daemon to deliver. Same tool name, same
    // schema as the daemon's, so a model writes the same call either side.
    base_tools_vec.push(Box::new(crate::tools::DaemonMessageSendTool::from_config(
        &config,
        security.clone(),
    )));

    // Wrap the now-complete registry in `Arc`, then inject it back into
    // sessions_spawn's tools OnceLock so spawned sub-agents can use the full tool
    // set (resolves the spawn-tool-needs-the-tool-table chicken-and-egg; mirrors
    // the channels path).
    let tools_registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(base_tools_vec);
    // Re-inject the completed registry into sessions_spawn's OnceLock. A failure
    // here means the handle was already set (the spawn tool's tool table never got
    // this full registry) — spawned sub-agents would then run with an incomplete
    // tool set, so surface it instead of swallowing silently.
    if !spawn_tools_handle.set(&tools_registry) {
        tracing::warn!(
            "sessions_spawn tools registry was already initialized; spawned sub-agents may have an incomplete tool set"
        );
    }

    let turn_decisions = ctx
        .publish_turn_decisions(
            Arc::clone(&tools_registry),
            crate::causal_tree::state::TurnSurfaceKind::Interactive,
        )
        .await?;
    // Keep the publisher alive through every chat task and drain it even when
    // session setup or an ingress operation returns an error.
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(
        crate::runtime::turn_cleanup::current_tracker().as_ref(),
        CancellationToken::new(),
    )?;
    let chat_result = cleanup.run(async {
    let child_cleanup = crate::runtime::turn_cleanup::current_tracker()
        .context("chat cleanup owner missing")?;
    child_cleanup.retain_service(Arc::clone(&turn_decisions));

    // v3a: coordination handle for interactive PTY terminal handoff. Shared with
    // the unified TUI render loop (which parks while a PTY is attached) and the
    // main loop's `/pty` handler (which pauses/resumes it around the passthrough).
    // Only the TUI path performs the handoff; the non-TUI fallback has no render
    // loop to suspend.
    #[cfg(feature = "terminal-tui")]
    let pty_handoff = Arc::new(crate::chat::sessions::pty::HandoffControl::new());

    // ── Session: resume or create new ───────────────────────────
    let mut chat_session = match session_id.as_deref() {
        // D10/C1: distinguish None (no session -> new) from Err (storage failure).
        // A storage Err must fail fast: ChatSession::new mints a *new* id, so
        // silently starting fresh on a transient DB error would fork the
        // conversation and bury the original context (session-loss illusion),
        // not "overwrite the same key".
        Some("last") => {
            match load_latest_session_with_message_events(mem.as_ref(), memory_fabric.workspace_id(), &config.cost)
                .await
            {
                Ok(Some(s)) => {
                    info!(id = %s.id, title = %s.title, turns = s.turn_count(), "Resumed session");
                    s
                }
                Ok(None) => {
                    info!("No previous session found, starting new");
                    session::ChatSession::new(provider_name, model_name)
                }
                Err(e) => {
                    anyhow::bail!(
                        "failed to load the most recent session: {e}; refusing to start a fresh session that would bury it"
                    );
                }
            }
        }
        Some(id) => {
            match load_session_by_id_with_message_events(mem.as_ref(), memory_fabric.workspace_id(), id, &config.cost)
                .await
            {
                Ok(Some(s)) => {
                    info!(id = %s.id, title = %s.title, turns = s.turn_count(), "Resumed session");
                    s
                }
                Ok(None) => {
                    eprintln!("Session '{id}' not found, starting new session.");
                    session::ChatSession::new(provider_name, model_name)
                }
                Err(e) => {
                    anyhow::bail!(
                        "failed to load session '{id}': {e}; refusing to start a fresh session that would bury it"
                    );
                }
            }
        }
        None => session::ChatSession::new(provider_name, model_name),
    };
    bind_session_to_runtime_provider_model(&mut chat_session, provider_name, model_name);
    // D8-2: run_id is per-turn, not per-session. It is generated inside the turn
    // loop (see `turn_run_id` below) so each user/assistant exchange gets a fresh
    // run_id. The session identity is carried by `chat_session_key`, never by
    // run_id, and turns deliberately set no parent_run_id (that field is reserved
    // for the spawn execution lineage, not for relating turns within a session).
    let mut chat_session_key = format!("chat:{}", chat_session.id);
    let mut fabric_turn_seq: u64 = 0;

    // ── Build banner text ────────────────────────────────────────
    // On the TUI path the banner is *not* printed to stdout here — printing
    // before `TerminalGuard::enter()` would pollute the parent shell's
    // scrollback, and printing after would corrupt the ratatui draw buffer
    // (raw mode strips `\r` from `\n`, producing ladder-shaped garbage).
    // Instead we capture it as a `String` and `push_system_message` it into
    // the shared `chat_mirror` after the guard is in place but before the
    // unified render loop spawns. The legacy fallback path (no TUI) prints
    // the banner the old way.
    // Claude-Code style single-line minimal banner: `prx <version> · provider/model`.
    // The previous multi-line "PRX Chat — ... \n Type /help for commands"
    // hint has moved to the persistent footer instead. `chat_session` is no
    // longer queried for the banner but stays in scope for downstream use.
    let banner = startup_banner(provider_name, model_name);

    // ── Conversation history ─────────────────────────────────────
    let mut history = history_for_session_with_system(&chat_session, &config, model_name, &skills, native_tools);

    // ── P3-3: shared TuiState mirror ─────────────────────────────
    //
    // A single `Arc<parking_lot::Mutex<TuiState>>` is bound to `chat::run`'s
    // lifetime and threaded into every producer that wants to mutate the
    // visible TUI state: the input task (keystrokes, history navigation),
    // the per-turn tool-event forwarder (`push_tool_result_*`), the reasoning
    // push at end-of-turn, and — once P3-4 lands — the UiActor draft/stream
    // bridge. The render task (also spawned here on the TUI path) only
    // **reads** the mirror under a short-lived lock.
    //
    // Replacing the previous two-instance design (`mirror_for_input` +
    // per-turn `tui_mirror`) collapses all observable mutations into a
    // single state machine so the renderer sees a consistent view.
    #[cfg(feature = "terminal-tui")]
    let initial_saved_session_entries = match saved_chat_sessions(mem.as_ref()).await {
        Ok(sessions) => sessions
            .iter()
            .map(|session| crate::chat::session::SavedSessionPickerEntry::from_session(session, &chat_session.id))
            .collect::<Vec<_>>(),
        Err(e) => {
            tracing::warn!(error = %e, "failed to load saved chat sessions for TUI slash-menu cache");
            Vec::new()
        }
    };

    #[cfg(feature = "terminal-tui")]
    let chat_mirror: Arc<parking_lot::Mutex<tui::TuiState>> = {
        let mut state = tui::TuiState::new(provider_name, model_name);
        state.chat_mode = chat_session.mode;
        state.autonomy_level = config.autonomy.level;
        state.token_usage_summary = chat_session.token_usage_summary();
        state.provider_model_catalog = tui::slash_provider_model_catalog_from_config(&config);
        state.saved_sessions_cache = initial_saved_session_entries.clone();
        Arc::new(parking_lot::Mutex::new(state))
    };

    // ── Input channel ────────────────────────────────────────────
    let (input_tx, mut input_rx) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
    scheduled_input_handle.set_input_sender(input_tx.clone());
    // Taken before the terminal listener consumes `input_tx`: daemon-assigned
    // work is submitted through the very same FIFO as typed input, which is
    // what makes it go through the ordinary turn machinery rather than a
    // parallel path with its own bugs.
    let assignment_input_tx = input_tx.clone();
    let (control_tx, mut control_rx) = mpsc::channel::<ChatControlEvent>(CHAT_CONTROL_CHANNEL_CAPACITY);
    #[cfg(not(feature = "terminal-tui"))]
    let _ = &control_tx;

    // ── Graceful shutdown signal ─────────────────────────────────
    // Instead of std::process::exit(), all signal handlers use this token to
    // break the main loop gracefully, allowing final session save + teardown.
    // The token is now supplied by the caller (D5/D9 step 1): dispatch owns the
    // root shutdown token and passes it down. The internal ctrl_c single/double
    // handlers below cancel this same external token, so chat remains the sole
    // owner of ctrl_c semantics while the caller can also request shutdown.

    // ── Step 5a-1: Redux dispatcher (shadow / real-deps mode) ────
    //
    // The global dispatcher channel + EffectExecutor + ChatState are built here.
    // The EffectExecutor mode is decided by the `PRX_CHAT_REDUX` env var:
    //   - Off (default): shadow mode, every business Effect is a no-op; the old path writes alone
    //   - Both: real mode, business Effects really execute while the old path still runs;
    //     dual_write_guard is raised after the reducer's persistence effect, and the old path checks
    //     the guard to skip the corresponding write
    //   - Redux: behaves like Both (the 5a-1 stage does not delete the old path; only 5a-3 really
    //     deletes it)
    //
    // bounded(2048): covers the Action stream of a typical chat session (user input + streaming
    // chunks + tool events + signals) while merging deltas through [`StreamChunkCoalescer`] under
    // backpressure, so it cannot grow without bound and OOM.
    let (chat_dispatcher, chat_action_rx) = dispatcher::ChatDispatcher::new();
    let dispatcher_shadow_state =
        initial_chat_dispatcher_state(provider_name, model_name, shutdown.clone(), &chat_session);
    // The TUI fields exist only with `terminal-tui`; without it the state is
    // used as built (no `mut` binding, so feature-less builds do not warn).
    #[cfg(feature = "terminal-tui")]
    let dispatcher_shadow_state = {
        let mut state = dispatcher_shadow_state;
        state.ui.chat_mode = chat_session.mode;
        state.ui.autonomy_level = config.autonomy.level;
        state.ui.provider_model_catalog = tui::slash_provider_model_catalog_from_config(&config);
        state.ui.saved_sessions_cache = initial_saved_session_entries.clone();
        state
    };

    // Shared dual-write guard (raised by the EffectExecutor in Both/Redux mode; the old path checks
    // the guard to decide whether to skip persistence. It is built even in Off mode, where the old
    // path's check is always false at zero cost.
    // P0-1 fix: dropped allow(unused_variables) — the guard is read at the end of an old-path turn,
    // so it is genuinely used under both feature configurations.)
    let dual_write_guard = dispatcher::RuntimeDualWriteGuard::new();

    // PRX_CHAT_REDUX is read once at the entry point and reused inside the function body, so the env
    // var is not parsed in several places.
    // S4-B: Pure is the only supported run path; non-Pure values are warned about and forced up.
    #[cfg(feature = "terminal-tui")]
    let top_redux_mode = { ReduxMode::from_env() };
    #[cfg(feature = "terminal-tui")]
    let max_concurrent_visible_turns = config.chat.max_concurrent_visible_turns.max(1);
    #[cfg(not(feature = "terminal-tui"))]
    let max_concurrent_visible_turns = 1usize;
    #[cfg(feature = "terminal-tui")]
    let visible_input_admission_kind = crate::chat::turn_worker::ProviderTurnWorkerKind::Detached;
    #[cfg(not(feature = "terminal-tui"))]
    let visible_input_admission_kind = crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited;

    // Class B — bounded. Three events are emitted per provider turn, and with
    // no cap on concurrent turns the emission rate is unbounded while the sole
    // consumer is this loop, which also renders and handles input. See
    // `PROVIDER_TURN_LIFECYCLE_CHANNEL_CAPACITY` for the sizing rationale.
    let (provider_turn_lifecycle_tx, mut provider_turn_lifecycle_rx) =
        mpsc::channel::<dispatcher::ProviderTurnLifecycleEvent>(PROVIDER_TURN_LIFECYCLE_CHANNEL_CAPACITY);
    #[cfg(not(feature = "terminal-tui"))]
    let _ = &provider_turn_lifecycle_tx;

    // Pick the EffectExecutor mode from the redux mode (TUI feature only)
    #[cfg(feature = "terminal-tui")]
    let effect_executor = {
        let mode = top_redux_mode;
        let deps = dispatcher::EffectDeps {
            provider: Arc::clone(&provider),
            memory: Arc::clone(&mem),
            memory_event_recording: config.memory.event_recording_config(),
            channel: Arc::clone(&terminal) as Arc<dyn crate::channels::Channel>,
            hooks: Arc::clone(&hooks),
            observer: Arc::clone(&observer),
            action_tx: chat_dispatcher.sender(),
            provider_turn_lifecycle_tx: Some(provider_turn_lifecycle_tx.clone()),
            dual_write_guard: dual_write_guard.clone(),
            redraw_tx: None,
            tui_mirror: Some(Arc::clone(&chat_mirror)),
            shutdown: shutdown.clone(),
            model: dispatcher::ModelSlot::new(Arc::from(model_name)),
            temperature,
            tools_registry: Some(Arc::clone(&tools_registry)),
            approval_router: Arc::new(dispatcher::ApprovalRouter::new()),
            tool_security_policy: Arc::clone(&security),
            tool_tiering: config.tool_tiering.clone(),
        };
        tracing::info!(mode = ?mode, "chat EffectExecutor in Pure real-deps mode");
        dispatcher::EffectExecutor::new_with_deps(deps).with_cleanup_owner(Arc::clone(&child_cleanup))
    };
    #[cfg(not(feature = "terminal-tui"))]
    let effect_executor = dispatcher::EffectExecutor::new_shadow();

    // P0-2 fix: take the redraw_slot Arc up front so redraw_tx can be injected after TUI
    // initialization completes. The EffectExecutor is consumed by the dispatcher
    // spawn (`spawn_dispatcher_task_full`), but the Arc is cloned before the spawn, so the real sender
    // can still be filled in afterwards through this Arc and RequestRedraw really executes.
    #[cfg(feature = "terminal-tui")]
    let executor_redraw_slot = effect_executor.redraw_handle();

    // BUG-07: take the model hot-swap slot handle up front (same idea as redraw_slot: cloned before
    // the spawn so that after the spawn `/model <name>` can still replace the model through this
    // handle, and drive_start_turn_stream of later turns reads the new value). Shadow mode has no
    // deps → None.
    #[cfg(feature = "terminal-tui")]
    let model_slot = effect_executor.model_handle();

    // Bug #3: the provider hot-swap slot handle (same idea as model_slot). Cloned before the spawn,
    // so that on `/provider <name>` the rebuilt provider handle can be set into it and the Redux
    // driver of later turns (drive_start_turn_stream) reads the new provider. Shadow mode has no
    // deps → None.
    #[cfg(feature = "terminal-tui")]
    let provider_slot = effect_executor.provider_handle();

    #[cfg(feature = "terminal-tui")]
    let approval_router = effect_executor.approval_router();
    #[cfg(not(feature = "terminal-tui"))]
    let approval_router: Option<Arc<dispatcher::ApprovalRouter>> = None;

    // Step 5a-4: TurnCompletionSignal — the switched-over Redux driver path uses this signal to await
    // turn completion in the chat::run main loop. After the dispatcher task consumes a terminal action
    // (StreamCompleted/Failed/Cancelled) it calls notify_waiters to wake the waiter.
    // The Off / legacy paths do not read the signal, and building it is very cheap (Arc<Notify>).
    let turn_signal = dispatcher::TurnCompletionSignal::with_turn_cancellation(shutdown.child_token());
    let _turn_cancellation_guard = turn_signal.cancel_turns_on_exit();

    // S4-A Commit 3: build the watch::channel<Arc<UiSnapshot>>; the dispatcher pushes a new snapshot
    // whenever ui_dirty=true. TUI render and child views consume
    // this reducer-owned snapshot as the primary UI source, with chat_mirror kept
    // only for synchronous key-thread compatibility and fallback.
    //
    // rx is wired into run_tui_unified_loop in Commit 4; this commit only traces the push rate, and
    // rx is kept as an `Option` for spawn_tui_unified_loop to use.
    #[cfg(feature = "terminal-tui")]
    let (snapshot_tx_for_dispatcher, snapshot_rx_for_tui) = {
        let mut initial = crate::chat::state::UiSnapshot::initial(
            std::sync::Arc::from(provider_name),
            std::sync::Arc::from(model_name),
        );
        initial.chat_mode = chat_session.mode;
        initial.autonomy_level = config.autonomy.level;
        initial.token_usage_summary = chat_session.token_usage_summary();
        let initial = std::sync::Arc::new(initial);
        let (tx, rx) = tokio::sync::watch::channel(initial);
        tracing::info!(mode = ?top_redux_mode, "snapshot_tx constructed for Pure chat mode");
        (Some(tx), Some(rx))
    };
    // Commit 4: snapshot_rx_for_tui is passed to run_tui_unified_loop (see the spawn_tui_unified_loop
    // call in the TUI branch).

    #[cfg(feature = "terminal-tui")]
    let dispatcher_handle = dispatcher::spawn_dispatcher_task_full(
        dispatcher_shadow_state,
        chat_action_rx,
        shutdown.clone(),
        effect_executor,
        Some(turn_signal.clone()),
        snapshot_tx_for_dispatcher,
    );
    #[cfg(not(feature = "terminal-tui"))]
    let dispatcher_handle = dispatcher::spawn_dispatcher_task_with_signal(
        dispatcher_shadow_state,
        chat_action_rx,
        shutdown.clone(),
        effect_executor,
        Some(turn_signal.clone()),
    );

    // ── Ctrl+C shared state ─────────────────────────────────────
    // Tracks the timestamp (ms) of the last Ctrl+C press for double-press detection.
    // Lifted above the input loop so the TUI dispatcher can fold its own
    // Ctrl+C presses into the same double-press → exit semantics.
    let last_ctrlc_ms = Arc::new(AtomicU64::new(0));
    // Non-TUI fallback still needs a shared cancellation slot for SIGINT.
    #[cfg(not(feature = "terminal-tui"))]
    let active_cancel: Arc<parking_lot::Mutex<Option<CancellationToken>>> = Arc::new(parking_lot::Mutex::new(None));

    // Spawn the appropriate input loop:
    //   - feature `terminal-tui` + TTY stdin + (PRX_TUI != "0") → ratatui/crossterm
    //     KeyEvent loop driving `dispatch_global_key` against the shared
    //     `chat_mirror`, plus a `spawn_render_task` that owns the
    //     `ratatui::Terminal` and redraws on demand.
    //   - otherwise → legacy reedline + BufRead fallback via TerminalChannel.
    //
    // `terminal_guard` is bound to this function's stack so its Drop runs at
    // chat::run exit (panic-safe via `install_chat_panic_hook` above). The
    // legacy path leaves `terminal_guard = None`, so no entry side-effects
    // are applied.
    // `redraw_tx_for_main` is `Some(sender)` only on the TUI path; the main
    // loop uses it to nudge the renderer after mutating `chat_mirror` (e.g.
    // echoing the user's submitted input so the conversation pane reflects
    // it immediately rather than waiting for the next async event).
    #[cfg(feature = "terminal-tui")]
    let (terminal_guard, redraw_tx_for_main): (Option<TerminalGuard>, Option<mpsc::Sender<()>>) = {
        use std::io::IsTerminal as _;
        // TUI is on by default in TTY. Opt out with PRX_TUI=0 (e.g. for
        // downstream scripts that scrape stdout, or to escape rendering
        // glitches). Non-TTY stdin (pipe / heredoc / scripted) always falls
        // through to the legacy reedline + BufRead path.
        let prx_tui_env = std::env::var("PRX_TUI").ok();
        let tui_selection = select_chat_tui(plain_mode, std::io::stdin().is_terminal(), prx_tui_env.as_deref());
        if tui_selection.enabled {
            // Order matters: `TerminalGuard::enter()` flips terminal mode
            // FIRST, then we wire up the UiActor
            // mirror BEFORE spawning the unified TUI loop (so no `UiEvent`
            // can sneak through to the old println!-based renderer in
            // `channels/terminal.rs`). On enter failure we fall back to the
            // legacy reedline path so the user is never left without a
            // prompt.
            match TerminalGuard::enter() {
                Ok(guard) => {
                    // S4-B: the chat_mirror side write is deleted; in Pure mode the reducer is the
                    // single source that owns the banner.
                    // S2-C Step 3: dual-write into the Redux UI mirror. Under Off/Both/Redux
                    // chat_mirror is still the TUI render source (this dispatch only keeps a
                    // consistent UI ledger for the Redux path plus test assertions); in Pure mode
                    // this is the single reducer-owned entry point.
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::SystemMessageAdded { text: banner.clone() },
                        "chat.banner",
                    );

                    // The redraw channel exists solely so the UiActor and
                    // background tasks can wake the unified loop on
                    // streaming events. cap=1 + try_send is the coalesce
                    // idiom: bursts collapse into a single deferred redraw.
                    let (redraw_tx, redraw_rx) = mpsc::channel::<()>(1);

                    // P0-2 fix: inject redraw_tx into the EffectExecutor's redraw_slot afterwards.
                    // The EffectExecutor has already been consumed by the dispatcher task, but the
                    // executor_redraw_slot Arc saved earlier lets the real sender be filled in across
                    // the spawn boundary, so the RequestRedraw effect really triggers a redraw.
                    *executor_redraw_slot.lock() = Some(redraw_tx.clone());
                    tracing::debug!("P0-2: redraw_tx injected into EffectExecutor redraw_slot");

                    // S4-B: the TuiStateMirrorSink path is deleted; Pure mode uses
                    // SnapshotDispatcherSink everywhere.
                    let sink: Box<dyn crate::channels::terminal::TuiMirrorSink> =
                        Box::new(tui::SnapshotDispatcherSink::new(chat_dispatcher.clone()));
                    terminal.with_tui_mirror(sink, redraw_tx.clone()).await;

                    // P3-rearch: single thread owns Terminal/stdout + reads
                    // crossterm events. No more spawn_render_task /
                    // spawn_redraw_tick_task / spawn_tui_input_task trio —
                    // they fought each other over the same stdout handle.
                    // Hand a clone to the main loop so it can request a
                    // redraw immediately after echoing the user's input
                    // into `chat_mirror`.
                    let redraw_tx_main = redraw_tx.clone();
                    let redraw_tx_loop = redraw_tx.clone();
                    // S4-A Commit 4: pass snapshot_rx to the unified loop so it borrows the
                    // reducer-owned snapshot from the watch::Receiver instead of calling
                    // chat_mirror.lock() on the render path.
                    spawn_tui_unified_loop(
                        input_tx,
                        control_tx.clone(),
                        Arc::clone(&chat_mirror),
                        redraw_rx,
                        redraw_tx_loop,
                        shutdown.clone(),
                        Arc::clone(&last_ctrlc_ms),
                        chat_dispatcher.clone(),
                        snapshot_rx_for_tui.clone(),
                        Arc::clone(&pty_handoff),
                        config.workspace_dir.clone(),
                        Arc::clone(&security),
                    );
                    (Some(guard), Some(redraw_tx_main))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "TerminalGuard::enter failed; falling back to reedline input");
                    // On guard failure the banner has not been printed yet
                    // and we are about to use the legacy non-TUI path, so
                    // print it the old way.
                    println!("{banner}");
                    let terminal_for_listen = TerminalChannel::new(plain_mode);
                    tokio::spawn(async move {
                        if let Err(e) = terminal_for_listen.listen(input_tx).await {
                            tracing::error!("Terminal input loop error: {e}");
                        }
                    });
                    (None, None)
                }
            }
        } else {
            // Fallback path (PRX_TUI=0 opt-out, or non-TTY pipe/heredoc) — keep
            // the legacy reedline + BufRead fallback via TerminalChannel and
            // print the banner the old way for parity with the previous
            // behaviour.
            println!("{banner}");
            let terminal_for_listen = TerminalChannel::new(plain_mode);
            tokio::spawn(async move {
                if let Err(e) = terminal_for_listen.listen(input_tx).await {
                    tracing::error!("Terminal input loop error: {e}");
                }
            });
            (None, None)
        }
    };
    #[cfg(not(feature = "terminal-tui"))]
    let redraw_tx_for_main: Option<mpsc::Sender<()>> = None;
    #[cfg(not(feature = "terminal-tui"))]
    {
        println!("{banner}");
        let terminal_for_listen = TerminalChannel::new(plain_mode);
        tokio::spawn(async move {
            if let Err(e) = terminal_for_listen.listen(input_tx).await {
                tracing::error!("Terminal input loop error: {e}");
            }
        });
    }

    let mut plain_mode_turn_failed = false;

    // Persistent Ctrl+C handler: runs for the entire chat session.
    // - If a generation is active: cancel it (first press) or exit (double press).
    // - If idle (no generation): exit on double press.
    //
    // Step 5b dual write: on every Ctrl+C, besides the old path's cancel/shutdown, also try_dispatch
    // `CancelRequested` / `ShutdownRequested` to the dispatcher (in shadow mode this only reaches the
    // reducer + log and takes no part in the real control flow). A full or closed try_send never
    // affects the old path's fallback.
    //
    // The handler must also exit when shutdown fires, so it does not hold a dispatcher sender and
    // block the dispatcher task from exiting (only when drop(chat_dispatcher) and the clone inside
    // this handler are both dropped can the channel really close and dispatcher_handle.await
    // return).
    {
        let last_ctrlc = Arc::clone(&last_ctrlc_ms);
        #[cfg(not(feature = "terminal-tui"))]
        let cancel_ref = Arc::clone(&active_cancel);
        let shutdown_signal = shutdown.clone();
        let dispatcher_for_signal = chat_dispatcher.clone();
        #[cfg(feature = "terminal-tui")]
        let mirror_for_signal = Arc::clone(&chat_mirror);
        #[cfg(feature = "terminal-tui")]
        let redraw_for_signal = redraw_tx_for_main.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = shutdown_signal.cancelled() => break,
                    res = tokio::signal::ctrl_c() => {
                        if res.is_err() {
                            break;
                        }
                    }
                }
                let now = now_ms();
                let prev = last_ctrlc.swap(now, Ordering::Relaxed);

                if now.saturating_sub(prev) < DOUBLE_CTRLC_WINDOW_MS {
                    // Double Ctrl+C → graceful shutdown
                    eprintln!("\nExiting...");
                    // Step 5b shadow: dispatch ShutdownRequested synchronously.
                    let _ = dispatcher_for_signal.dispatch_or_log(
                        crate::chat::action::Action::ShutdownRequested,
                        "chat.shutdown_double_ctrlc",
                    );
                    shutdown_signal.cancel();
                    break;
                }

                // Single Ctrl+C → cancel active generation if any
                // Step 5b shadow: dispatch CancelRequested synchronously for the reducer to observe.
                #[cfg(feature = "terminal-tui")]
                if mirror_for_signal.lock().clear_pending_tool_approval()
                    && let Some(tx) = redraw_for_signal.as_ref()
                {
                    let _ = tx.try_send(());
                }
                let _ = dispatcher_for_signal
                    .dispatch_or_log(crate::chat::action::Action::CancelRequested, "chat.cancel_single_ctrlc");
                #[cfg(not(feature = "terminal-tui"))]
                if let Some(token) = cancel_ref.lock().as_ref() {
                    token.cancel();
                }
            }
        });
    }

    // SIGTERM handler: signal graceful shutdown.
    //
    // Step 5b dual write: dispatch ShutdownRequested to the dispatcher (shadow observation) and also
    // call shutdown.cancel() as the fallback (the old path's exit protocol is kept).
    // This task must exit on its own when shutdown fires, so it does not hold a sender clone and
    // block the dispatcher task from closing.
    #[cfg(unix)]
    {
        let sigterm_result = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        match sigterm_result {
            Ok(mut sigterm) => {
                let shutdown_signal = shutdown.clone();
                let dispatcher_for_sigterm = chat_dispatcher.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        biased;
                        () = shutdown_signal.cancelled() => {
                            // The main path already shut down, so nothing more to trigger; exit and
                            // release the sender clone.
                        }
                        _ = sigterm.recv() => {
                            let _ = dispatcher_for_sigterm
                                .dispatch_or_log(crate::chat::action::Action::ShutdownRequested, "chat.shutdown_sigterm");
                            shutdown_signal.cancel();
                        }
                    }
                });
            }
            Err(e) => {
                tracing::warn!("Failed to register SIGTERM handler: {e}");
            }
        }
    }

    // BUG-07: the currently effective model name (owned, mutable). An online `/model <name>` switch
    // rewrites this value; the top of each loop iteration borrows it as `model_name: &str` for the
    // rest of the turn (system prompt / fabric events / snapshot). Its initial value matches the
    // `model_name` resolved at startup.
    let mut current_model_owned: String = model_name.to_string();

    // Bug #3: the currently effective provider name (owned, mutable). An online `/provider <name>`
    // switch rewrites this value; the top of each loop iteration borrows it as
    // `provider_name: &str`, covering every later use of the provider in the turn (system prompt /
    // fabric events / snapshot / the legacy tool-loop path's run_turn call). Its initial value matches the
    // `provider_name` resolved at startup.
    let mut current_provider_owned: String = provider_name.to_string();
    // Bug #3: the provider handle (the legacy tool-loop path's run_turn call uses `provider.as_ref()`
    // directly). On `/provider <name>` this Arc is rebuilt and replaced with the new provider, which
    // is also set into provider_slot (the Redux path).
    let mut provider = provider;

    // ── Background-session observation state (v1b) ────────────────
    // Owned by the main loop (single-threaded), per the iron law that runtime
    // state is only written here — the detached spawn tasks only mutate the
    // shared registry, never this. `reported_sessions` dedups the one-shot
    // summary reflow; `announced_started_sessions` dedups the started notices
    // that promote sessions_spawn agents into the same visible child-session
    // mode as shells; `last_sessions_summary` dedups the persistent status-line
    // action so we only dispatch on change. The 1s timer is a read-only poll of
    // the registry (no event bus until v1.1).
    let mut reported_sessions: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut announced_started_sessions: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut last_sessions_summary: String = String::new();
    let mut last_sessions_entries: Vec<crate::chat::sessions::SwitcherEntry> = Vec::new();
    let mut sessions_tick = tokio::time::interval(Duration::from_secs(1));
    sessions_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // ── Event bridge state (v1.1a) ────────────────────────────────
    // Per-session ring buffers, written ONLY here (single consumer; iron law:
    // the ring is never written by the background agent or the drainer). Live
    // `/attach` follows one session at a time: when `attached_follow` matches an
    // incoming event's session, its delta/tool lines are streamed inline to the
    // existing scrollback (read-only — no input routing; that is v1.1b).
    let mut session_rings: std::collections::HashMap<
        crate::chat::sessions::id::SessionId,
        crate::chat::sessions::SessionRing,
    > = std::collections::HashMap::new();
    let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
    let mut reaped_log_archive = ReapedSessionLogArchive::default();
    let mut ignored_session_events: std::collections::HashSet<crate::chat::sessions::id::SessionId> =
        std::collections::HashSet::new();
    let mut input_backlog: std::collections::VecDeque<QueuedInputMessage> = std::collections::VecDeque::new();
    let mut defer_visible_input_pop_once = false;
    let mut turn_scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let mut history_commit_coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
    let mut provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    let mut provider_turn_lifecycle_events_open = true;
    let (provider_completion_tx, mut provider_completion_rx) = mpsc::channel::<ProviderTurnCompletionEvent>(64);
    #[cfg(not(feature = "terminal-tui"))]
    let _ = &provider_completion_tx;
    #[cfg(feature = "terminal-tui")]
    let mut provider_turn_finalizer_events: std::collections::VecDeque<ProviderTurnFinalizerEvent> =
        std::collections::VecDeque::new();
    let mut pending_provider_completion_events: std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        ProviderTurnCompletionEvent,
    > = std::collections::HashMap::new();
    let mut provider_turn_completion_contexts: std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        ProviderTurnCompletionContext,
    > = std::collections::HashMap::new();
    #[cfg(feature = "terminal-tui")]
    let mut per_turn_contexts: std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, PerTurnContext> =
        std::collections::HashMap::new();
    #[cfg(feature = "terminal-tui")]
    let mut pending_ordered_provider_turn_commits: std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        PendingOrderedProviderTurnCommit,
    > = std::collections::HashMap::new();
    #[cfg(feature = "terminal-tui")]
    let mut deferred_resume_saved_session_ids: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut attached_follow: Option<crate::chat::sessions::id::SessionId> = None;
    // Display sequence `#N` of the currently-followed session, kept in lock-step
    // with `attached_follow`. Used purely to reconstruct the *previous* focus
    // target when an optimistic attach must be rolled back (v1.1b review P0): on
    // attach failure the key thread has already pointed the prompt at the new
    // seq, so the main loop restores `Main` (when None) or `Session { seq }`.
    let mut attached_follow_seq: Option<u64> = None;
    // Guards the event-drain select arm: once the event channel closes (only at
    // shutdown — the sender lives as long as the tool registry) we disable the
    // arm so a closed channel does not busy-spin returning `None`.
    let mut session_events_open = true;
    let mut control_events_open = true;
    // Renderer nudge handle, available in both feature configs (the TUI-only
    // `redraw_tx_for_main` is `Some` only on the TUI path; `None` otherwise so
    // the helpers fall back to plain stdout).
    #[cfg(feature = "terminal-tui")]
    let sessions_redraw_handle: Option<mpsc::Sender<()>> = redraw_tx_for_main.clone();
    #[cfg(not(feature = "terminal-tui"))]
    let sessions_redraw_handle: Option<mpsc::Sender<()>> = None;
    let mut pending_chat_rewind: Option<PendingChatRewind> = None;
    let mut pending_diff_apply: Option<PendingDiffApply> = None;
    #[cfg(feature = "terminal-tui")]
    let mut input_events_open = true;
    #[cfg(not(feature = "terminal-tui"))]
    let input_events_open = true;

    // ── Reload notice: historical child sessions (v4) ────────
    // If this chat session was resumed and carried persisted background-session
    // summaries, surface a one-shot recap so the user sees what their previous
    // background tasks produced. These are **summaries only** — no process,
    // sub-agent, or PTY is revived (those belonged to the prior process and are
    // long gone); any session that was still running at last exit shows as
    // `interrupted`.
    // ── Inherited embedding default ──────────────────────────────
    // The same notice `main` wrote to stderr, repeated on the first screen of
    // the session: the stderr line is behind the alternate screen for as long
    // as the TUI runs, and this is the entry point where vector recall costs
    // something every turn.
    if let Some(embedding_notice) = chat_embedding_upgrade_notice(&config) {
        surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &embedding_notice);
    }

    if !chat_session.background_sessions.is_empty() {
        let recap = format_reloaded_background_sessions(&chat_session.background_sessions);
        surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &recap);
    }

    // ── Daemon-assigned work (c2) ────────────────────────────────
    //
    // The other direction of the control link: a message on another channel can
    // name this chat session and a task, and the task has to run *here*. Chat
    // pulls its mailbox over the same control API it already uses, so nothing
    // listens and no new transport exists. Enrolment happens off this path — a
    // daemon that is down, slow or refusing credentials costs this chat the
    // ability to be assigned work and nothing else.
    let assignment_enrolment = start_assignment_enrolment(
        &config,
        &chat_sessions,
        &assignment_input_tx,
        &chat_dispatcher,
        sessions_redraw_handle.as_ref(),
    );

    // ── Main message loop ────────────────────────────────────────
    //
    // The inner `loop` lets a timer tick do background-session work (summary
    // reflow + status-line refresh) without producing a message or ending the
    // outer loop: it only `break`s with a real input message (or `None` on
    // shutdown). On a tick we poll the registry and surface results via the
    // dispatcher (single source, reaches both render paths), then keep waiting.
    while let Some(input) = loop {
        turn_signal.ensure_history_commits_healthy()?;
        #[cfg(feature = "terminal-tui")]
        macro_rules! handle_resume_saved_session_control {
            ($id:expr) => {{
                let provider_name = current_provider_owned.as_str();
                let model_name = current_model_owned.as_str();
                match resume_saved_session_by_id(
                    mem.as_ref(),
                    &$id,
                    ChatSwitchCtx {
                        chat_session: &mut chat_session,
                        chat_session_key: &mut chat_session_key,
                        fabric_turn_seq: &mut fabric_turn_seq,
                        history: &mut history,
                        approval_router: approval_router.as_ref(),
                        pending_chat_rewind: &mut pending_chat_rewind,
                        pending_diff_apply: &mut pending_diff_apply,
                        chat_sessions: &mut chat_sessions,
                        ignored_session_events: &mut ignored_session_events,
                        session_rings: &mut session_rings,
                        reported_sessions: &mut reported_sessions,
                        announced_started_sessions: &mut announced_started_sessions,
                        last_sessions_summary: &mut last_sessions_summary,
                        last_sessions_entries: &mut last_sessions_entries,
                        attached_follow: &mut attached_follow,
                        attached_follow_seq: &mut attached_follow_seq,
                        chat_dispatcher: &chat_dispatcher,
                        redraw_handle: sessions_redraw_handle.as_ref(),
                        config: &config,
                        provider_name,
                        model_name,
                        skills: &skills,
                        native_tools,
                        #[cfg(feature = "terminal-tui")]
                        chat_mirror: &chat_mirror,
                    },
                )
                .await
                {
                    Ok(message) => surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &message),
                    Err(e) => {
                        surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &e.to_string())
                    }
                }
            }};
        }
        #[cfg(feature = "terminal-tui")]
        if finalize_ready_per_turn_contexts(
            &mut per_turn_contexts,
            &mut pending_ordered_provider_turn_commits,
            &mut pending_provider_completion_events,
            &turn_signal,
            &mut provider_turn_completion_contexts,
            &mut history,
            &tools_registry,
            &mut provider_turn_finalizer_events,
            &mut turn_scheduler,
            &mut history_commit_coordinator,
            &mut provider_turn_workers,
            &chat_mirror,
            &chat_dispatcher,
            &terminal,
            &memory_fabric,
            &chat_session_key,
            &mut chat_session,
            &config,
            sessions_redraw_handle.as_ref(),
            redraw_tx_for_main.as_ref(),
            plain_mode,
            &mut plain_mode_turn_failed,
        )
        .await
        {
            continue;
        }
        if !input_events_open && input_backlog.is_empty() {
            #[cfg(feature = "terminal-tui")]
            if should_continue_event_pump_after_input_closed(per_turn_contexts.len()) {
                // Keep lifecycle/completion arms alive until the detached Redux turn
                // resolves; this mirrors the old inner wait, which ignored closed
                // input while the active turn was still running.
            } else {
                break None;
            }
            #[cfg(not(feature = "terminal-tui"))]
            break None;
        }
        #[cfg(feature = "terminal-tui")]
        if !provider_turn_finalizer_events.is_empty() {
            let results = drain_provider_turn_finalizer_events_and_publish(
                &mut turn_scheduler,
                &mut history_commit_coordinator,
                &mut provider_turn_workers,
                &mut provider_turn_finalizer_events,
                &chat_dispatcher,
            );
            #[cfg(feature = "terminal-tui")]
            let applied_ordered_commits = apply_ready_ordered_provider_turn_commits(
                results,
                &mut pending_ordered_provider_turn_commits,
                &mut history,
                &mut turn_scheduler,
                &provider_turn_workers,
                &chat_mirror,
                &chat_dispatcher,
                &terminal,
                &memory_fabric,
                &chat_session_key,
                &mut chat_session,
                &config,
                sessions_redraw_handle.as_ref(),
                redraw_tx_for_main.as_ref(),
                &turn_signal,
                plain_mode,
                &mut plain_mode_turn_failed,
            )
            .await;
            #[cfg(not(feature = "terminal-tui"))]
            let applied_ordered_commits = !results.is_empty();
            if applied_ordered_commits {
                continue;
            }
        }
        if consume_deferred_visible_input_pop(&mut defer_visible_input_pop_once) {
            // A post-route admission failure just requeued the same input at the
            // front. Skip one immediate visible pop so the event pump can consume
            // in-flight completion/lifecycle events that may free stricter Legacy
            // admission.
        } else if let Some(input) = pop_next_visible_input_task_with_scheduler(
            &mut input_backlog,
            &mut turn_scheduler,
            &provider_turn_workers,
            visible_input_admission_kind,
            max_concurrent_visible_turns,
        ) {
            publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
            break Some(input);
        }
        #[cfg(feature = "terminal-tui")]
        if should_drain_deferred_resume_after_visible_inputs(
            per_turn_contexts.len(),
            &input_backlog,
            &provider_turn_workers,
        ) && let Some(id) = deferred_resume_saved_session_ids.pop_front()
        {
            handle_resume_saved_session_control!(id);
            continue;
        }
        tokio::select! {
            msg = input_rx.recv(), if input_events_open => {
                let Some(msg) = msg else {
                    #[cfg(feature = "terminal-tui")]
                    {
                        if !per_turn_contexts.is_empty() {
                            input_events_open = false;
                            continue;
                        }
                    }
                    break None;
                };
                #[cfg(feature = "terminal-tui")]
                if let Some(active_task_id) = per_turn_contexts.keys().next().copied() {
                    let mut emit_active_turn_output =
                        |text: &str| surface_active_turn_message(&chat_dispatcher, redraw_tx_for_main.as_ref(), text);
                    let daemon_control = DaemonControl {
                        config: &config,
                        dispatcher: &chat_dispatcher,
                        redraw_tx: redraw_tx_for_main.as_ref(),
                    };
                    process_active_turn_input_batch(
                        msg,
                        &mut emit_active_turn_output,
                        &mut input_rx,
                        &mut input_backlog,
                        &mut turn_scheduler,
                        &mut provider_turn_workers,
                        Some(active_task_id),
                        &chat_dispatcher,
                        &chat_session,
                        &mut chat_sessions,
                        &session_rings,
                        &mut reaped_log_archive,
                        &reap_policy,
                        &tools_registry,
                        daemon_control,
                    )
                    .await;
                    publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                    continue;
                }
                enqueue_input_message_with_scheduler(
                    &mut input_backlog,
                    &mut turn_scheduler,
                    msg,
                    chat_session.turns.len(),
                );
                drain_available_input_messages(
                    &mut input_rx,
                    &mut input_backlog,
                    Some(&mut turn_scheduler),
                    chat_session.turns.len(),
                );
                publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                if let Some(next) = pop_next_visible_input_task_with_scheduler(
                    &mut input_backlog,
                    &mut turn_scheduler,
                    &provider_turn_workers,
                    visible_input_admission_kind,
                    max_concurrent_visible_turns,
                ) {
                    // The enqueue snapshot above reports this item as queued.
                    // Publish again after dequeue so fast slash commands, which
                    // never enter the provider-turn path, cannot leave a stale
                    // `queue:1` badge behind in the TUI.
                    publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                    break Some(next);
                }
                continue;
            },
            lifecycle = provider_turn_lifecycle_rx.recv(), if provider_turn_lifecycle_events_open => {
                let Some(lifecycle) = lifecycle else {
                    provider_turn_lifecycle_events_open = false;
                    continue;
                };
                record_provider_turn_lifecycle_event(
                    &mut provider_turn_workers,
                    lifecycle,
                );
                publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                continue;
            },
            completion = provider_completion_rx.recv() => {
                let Some(completion) = completion else {
                    continue;
                };
                let _ = route_provider_completion_event_and_publish(
                    &mut provider_turn_lifecycle_rx,
                    &mut provider_turn_lifecycle_events_open,
                    &mut provider_turn_workers,
                    &mut pending_provider_completion_events,
                    None,
                    completion,
                    &chat_dispatcher,
                );
                #[cfg(feature = "terminal-tui")]
                {
                    let _ = finalize_ready_per_turn_contexts(
                        &mut per_turn_contexts,
                        &mut pending_ordered_provider_turn_commits,
                        &mut pending_provider_completion_events,
                        &turn_signal,
                        &mut provider_turn_completion_contexts,
                        &mut history,
                        &tools_registry,
                        &mut provider_turn_finalizer_events,
                        &mut turn_scheduler,
                        &mut history_commit_coordinator,
                        &mut provider_turn_workers,
                        &chat_mirror,
                        &chat_dispatcher,
                        &terminal,
                        &memory_fabric,
                        &chat_session_key,
                        &mut chat_session,
                        &config,
                        sessions_redraw_handle.as_ref(),
                        redraw_tx_for_main.as_ref(),
                        plain_mode,
                        &mut plain_mode_turn_failed,
                    )
                    .await;
                }
                continue;
            },
            _ = shutdown.cancelled() => {
                #[cfg(feature = "terminal-tui")]
                {
                    if !per_turn_contexts.is_empty() {
                        finalize_all_per_turn_contexts_as_cancelled(
                            &mut per_turn_contexts,
                            &mut pending_ordered_provider_turn_commits,
                            &turn_signal,
                            &mut provider_turn_completion_contexts,
                            &mut history,
                            tools_registry.as_ref(),
                            &mut provider_turn_finalizer_events,
                            &mut turn_scheduler,
                            &mut history_commit_coordinator,
                            &mut provider_turn_workers,
                            &chat_mirror,
                            &chat_dispatcher,
                            &terminal,
                            &memory_fabric,
                            &chat_session_key,
                            &mut chat_session,
                            &config,
                            sessions_redraw_handle.as_ref(),
                            redraw_tx_for_main.as_ref(),
                            plain_mode,
                            &mut plain_mode_turn_failed,
                        )
                        .await;
                    }
                }
                break None
            },
            maybe_control = control_rx.recv(), if control_events_open => {
                let Some(control) = maybe_control else {
                    control_events_open = false;
                    continue;
                };
                match control {
                    #[cfg(feature = "terminal-tui")]
                    ChatControlEvent::ResumeSavedSession { id } => {
                        #[cfg(feature = "terminal-tui")]
                        if defer_resume_saved_session_if_provider_turn_pending(
                            per_turn_contexts.len(),
                            &mut deferred_resume_saved_session_ids,
                            id.clone(),
                        ) {
                            // Old Redux N=1 behavior waited inside the turn and did not
                            // poll control_rx, so session switching could not mutate
                            // chat_session/history until the active turn was finalized.
                            continue;
                        }
                        handle_resume_saved_session_control!(id);
                        continue;
                    }
                }
            }
            rewind_approval = async {
                match pending_chat_rewind.as_mut() {
                    Some(pending) => (&mut pending.approval_rx).await,
                    None => std::future::pending::<std::result::Result<
                        bool,
                        tokio::sync::oneshot::error::RecvError,
                    >>().await,
                }
            }, if pending_chat_rewind.is_some() => {
                let Some(pending) = pending_chat_rewind.take() else {
                    continue;
                };
                match resolve_rewind_approval(&pending.tool_id, rewind_approval) {
                    RewindApprovalOutcome::Apply => {
                        let target_id = pending.target_session.id.clone();
                        let target_turns = pending.target_session.turn_count();
                        if let Err(e) = save_session(mem.as_ref(), &pending.target_session).await {
                            surface_session_message(
                                &chat_dispatcher,
                                sessions_redraw_handle.as_ref(),
                                &format!("Rewind aborted: failed to save trimmed session: {e}"),
                            );
                            continue;
                        }
                        let provider_name = current_provider_owned.as_str();
                        let model_name = current_model_owned.as_str();
                        apply_chat_session_switch(ChatSwitchCtx {
                            chat_session: &mut chat_session,
                            chat_session_key: &mut chat_session_key,
                            fabric_turn_seq: &mut fabric_turn_seq,
                            history: &mut history,
                            approval_router: approval_router.as_ref(),
                            pending_chat_rewind: &mut pending_chat_rewind,
                            pending_diff_apply: &mut pending_diff_apply,
                            chat_sessions: &mut chat_sessions,
                            ignored_session_events: &mut ignored_session_events,
                            session_rings: &mut session_rings,
                            reported_sessions: &mut reported_sessions,
                            announced_started_sessions: &mut announced_started_sessions,
                            last_sessions_summary: &mut last_sessions_summary,
                            last_sessions_entries: &mut last_sessions_entries,
                            attached_follow: &mut attached_follow,
                            attached_follow_seq: &mut attached_follow_seq,
                            chat_dispatcher: &chat_dispatcher,
                            redraw_handle: sessions_redraw_handle.as_ref(),
                            config: &config,
                            provider_name,
                            model_name,
                            skills: &skills,
                            native_tools,
                            #[cfg(feature = "terminal-tui")]
                            chat_mirror: &chat_mirror,
                        }, pending.target_session).await;
                        surface_session_message(
                            &chat_dispatcher,
                            sessions_redraw_handle.as_ref(),
                            &format!("Rewound chat session {target_id} to {target_turns} turns."),
                        );
                    }
                    RewindApprovalOutcome::Cancelled(message) => {
                        surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &message);
                    }
                }
                continue;
            }
            apply_approval = async {
                match pending_diff_apply.as_mut() {
                    Some(pending) => (&mut pending.approval_rx).await,
                    None => std::future::pending::<std::result::Result<
                        bool,
                        tokio::sync::oneshot::error::RecvError,
                    >>().await,
                }
            }, if pending_diff_apply.is_some() => {
                let Some(pending) = pending_diff_apply.take() else {
                    continue;
                };
                match apply_approval {
                    Ok(true) => {
                        let security = crate::runtime::bootstrap::build_security_policy(&config);
                        match diff_apply::execute_plan(&pending.plan, security.as_ref()).await {
                            Ok(message) => surface_session_message(
                                &chat_dispatcher,
                                sessions_redraw_handle.as_ref(),
                                &message,
                            ),
                            Err(error) => surface_session_message(
                                &chat_dispatcher,
                                sessions_redraw_handle.as_ref(),
                                &format!("Diff apply aborted: {error}. Workspace unchanged."),
                            ),
                        }
                    }
                    Ok(false) => {
                        surface_session_message(
                            &chat_dispatcher,
                            sessions_redraw_handle.as_ref(),
                            "Diff apply cancelled; workspace unchanged.",
                        );
                    }
                    Err(_) => {
                        surface_session_message(
                            &chat_dispatcher,
                            sessions_redraw_handle.as_ref(),
                            &format!(
                                "Diff apply cancelled; approval channel closed for {} and workspace is unchanged.",
                                pending.tool_id
                            ),
                        );
                    }
                }
                continue;
            }
            _ = sessions_tick.tick() => {
                // 1) Summary reflow: surface each newly-finished session once,
                //    carrying its `#N` + status (plan §v1b). No auto-focus.
                let finished = chat_sessions.poll_finished(&mut reported_sessions).await;
                // 2) Persistent status line: recompute and dispatch only on change.
                let views = chat_sessions.snapshot().await;
                for fin in &finished {
                    let line = format_finished_session_announcement(fin);
                    let compact_summary = compact_child_completion_summary(&fin.summary);
                    surface_session_message(
                        &chat_dispatcher,
                        sessions_redraw_handle.as_ref(),
                        &line,
                    );
                    // v4: persist a summary of this finished child session
                    // into the chat session so a reload can show what it
                    // produced. Title / created_at come from the live view (the
                    // finished record itself only carries seq/status/summary);
                    // if the view is already gone we still record with the
                    // finished record's own fields so nothing is lost.
                    let persisted = views
                        .iter()
                        .find(|v| v.id.as_str() == fin.run_id)
                        .map_or_else(
                            || crate::chat::sessions::PersistedSessionSummary {
                                id: fin.run_id.clone(),
                                seq: fin.seq,
                                kind: fin.kind.as_str().to_string(),
                                origin: fin.origin.as_str().to_string(),
                                status: fin.status.as_str().to_string(),
                                title: String::new(),
                                summary: compact_summary.clone(),
                                token_usage_records: fin.token_usage_records.clone(),
                                created_at: fin.created_at,
                            },
                            |view| {
                                crate::chat::sessions::PersistedSessionSummary::from_view(
                                    view,
                                    compact_summary.clone(),
                                )
                            },
                        );
                    // Legacy (non-TUI) persistence path serializes `chat_session`
                    // directly, so mirror the record there too. The Redux/TUI
                    // path persists from SessionState, fed by the dispatched
                    // action below. Both write the same backward-compatible field.
                    chat_session.record_background_session(persisted.clone());
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::BackgroundSessionRecorded { summary: persisted },
                        "chat.bg_session_recorded",
                    );
                }
                let now = chrono::Utc::now();
                let reaped = chat_sessions.reap(&reap_policy, now).await;
                if !reaped.reaped.is_empty() {
                    let reaped_ids = reaped
                        .reaped
                        .iter()
                        .map(|session| session.id.clone())
                        .collect::<std::collections::HashSet<_>>();
                    for session in &reaped.reaped {
                        if chat_session
                            .background_sessions
                            .iter()
                            .any(|summary| summary.id == session.summary.id)
                        {
                            continue;
                        }
                        chat_session.record_background_session(session.summary.clone());
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::BackgroundSessionRecorded {
                                summary: session.summary.clone(),
                            },
                            "chat.bg_session_recorded_reap",
                        );
                    }
                    reaped_log_archive.archive_reaped(&reaped.reaped, &mut session_rings, &reap_policy, now);
                    if attached_follow.as_ref().is_some_and(|id| reaped_ids.contains(id)) {
                        attached_follow = None;
                        attached_follow_seq = None;
                        let focus = crate::chat::sessions::FocusTarget::Main;
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::SessionFocusChanged { focus },
                            "chat.session_focus_reaped",
                        );
                        #[cfg(feature = "terminal-tui")]
                        {
                            {
                                let mut mirror = chat_mirror.lock();
                                mirror.focus = focus;
                                mirror.active_session_view = None;
                            }
                            let _ = chat_dispatcher.dispatch_or_log(
                                crate::chat::action::Action::ActiveSessionViewUpdated { view: None },
                                "chat.active_session_view_reaped",
                            );
                        }
                    }
                }
                let views = chat_sessions.snapshot().await;
                for view in &views {
                    if view.kind != crate::chat::sessions::model::ManagedKind::Agent {
                        continue;
                    }
                    if announced_started_sessions.insert(view.id.as_str().to_string()) {
                        let line = format_observed_session_announcement(view);
                        surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &line);
                    }
                }
                // v1.1b: refresh the switcher cache the key thread reads on Ctrl+G
                // (it cannot run async registry queries itself). Display staleness
                // is harmless: switcher Enter re-resolves the seq via /attach.
                #[cfg(feature = "terminal-tui")]
                {
                    let idle_warnings = chat_sessions
                        .idle_warning_seqs(&reap_policy, chrono::Utc::now(), &session_rings)
                        .await;
                    let mut entries = crate::chat::sessions::focus::switcher_entries(&views);
                    for entry in &mut entries {
                        entry.idle_warning = idle_warnings.contains(&entry.seq);
                    }
                    {
                        let mut mirror = chat_mirror.lock();
                        refresh_sessions_cache(&mut mirror, entries.clone());
                    }
                    if entries != last_sessions_entries {
                        last_sessions_entries = entries.clone();
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::SessionsEntriesUpdated { entries },
                            "chat.sessions_entries",
                        );
                        if let Some(tx) = sessions_redraw_handle.as_ref() {
                            let _ = tx.try_send(());
                        }
                    }
                }
                let new_summary = crate::chat::sessions::status_summary(&views);
                if new_summary != last_sessions_summary {
                    last_sessions_summary = new_summary.clone();
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::SessionsStatusUpdated { summary: new_summary },
                        "chat.sessions_status",
                    );
                    if let Some(tx) = sessions_redraw_handle.as_ref() {
                        let _ = tx.try_send(());
                    }
                }
                // Keep waiting (do not break the inner loop / produce a message).
                continue;
            }
            maybe_event = session_event_rx.recv(), if session_events_open => {
                // Drain one background-session event: append it to that session's
                // ring (single-consumer write, no lock) and, if focused, refresh
                // the child viewport. P2 keeps child output out of main scrollback.
                let Some(event) = maybe_event else {
                    // Sender side closed (chat shutting down). Disable this arm so
                    // a closed channel does not busy-spin; other arms drive exit.
                    session_events_open = false;
                    continue;
                };
                let sid = event.session_id().clone();
                if should_ignore_session_event_after_chat_resume(&ignored_session_events, &event) {
                    continue;
                }
                let ring = session_rings
                    .entry(sid.clone())
                    .or_insert_with(|| crate::chat::sessions::SessionRing::with_capacity(
                        crate::chat::sessions::event::DEFAULT_RING_CAPACITY,
                    ));
                // A `Truncated` marker carries no output line; it only flags the
                // ring so `/attach` shows `[output truncated]` for events the
                // drainer had to drop on a full channel (P1 fix). Delta/ToolCall
                // append their text as a new ring line.
                let line = match &event {
                    crate::chat::sessions::SessionEvent::Delta { text, .. } => Some(text.clone()),
                    crate::chat::sessions::SessionEvent::ToolCall { summary, .. } => Some(summary.clone()),
                    crate::chat::sessions::SessionEvent::Truncated { .. } => {
                        ring.mark_truncated();
                        None
                    }
                    // NeedsInput: a background sub-agent suspended awaiting an
                    // operator approval decision. Record a ring line for `/attach`
                    // visibility; the non-intrusive `/approve` hint is surfaced
                    // below (after the seq is resolved). Status flips to
                    // `❓ needs-input` via the registry (already set by the
                    // resolver) on the next status refresh.
                    crate::chat::sessions::SessionEvent::NeedsInput { prompt, .. } => {
                        Some(format!("[needs approval] {prompt}"))
                    }
                    crate::chat::sessions::SessionEvent::Resumed { .. } => {
                        Some("[approval resolved — resuming]".to_string())
                    }
                };
                if let Some(line) = line {
                    ring.push(line);
                }
                // NeedsInput / Resumed are control signals (not stream output):
                // surface a non-intrusive operator hint with the session's `#N`
                // and refresh the status line / switcher so the `❓` glyph and
                // `needs-input` counter appear immediately, regardless of attach.
                match &event {
                    crate::chat::sessions::SessionEvent::NeedsInput { prompt, .. } => {
                        let seq = chat_sessions.seq_for_id(&sid).await;
                        let label = seq
                            .map(|n| format!("#{n}"))
                            .unwrap_or_else(|| sid.as_str().to_string());
                        surface_session_message(
                            &chat_dispatcher,
                            sessions_redraw_handle.as_ref(),
                            &format!(
                                "session {label} awaiting approval: {prompt} — /approve {} or /deny {} (/attach {} to inspect)",
                                seq.map(|n| n.to_string()).unwrap_or_else(|| label.clone()),
                                seq.map(|n| n.to_string()).unwrap_or_else(|| label.clone()),
                                seq.map(|n| n.to_string()).unwrap_or_else(|| label.clone()),
                            ),
                        );
                    }
                    crate::chat::sessions::SessionEvent::Resumed { .. } => {
                        if let Some(tx) = sessions_redraw_handle.as_ref() {
                            let _ = tx.try_send(());
                        }
                    }
                    _ => {}
                }
                #[cfg(feature = "terminal-tui")]
                if attached_follow.as_ref() == Some(&sid) {
                    refresh_attached_session_view_from_ring(
                        &chat_mirror,
                        &chat_dispatcher,
                        sessions_redraw_handle.as_ref(),
                        &sid,
                        ring,
                    );
                }
                continue;
            }
        }
    } {
        turn_signal.ensure_history_commits_healthy()?;
        let provider_queue_task_id = input.turn_task_id;
        let input_priority = input.priority;
        let msg = input.msg;
        let mut user_input = msg.content.clone();
        let mut resume_decision = false;
        let synthetic_ui_command = is_synthetic_ui_command(&msg);
        // Armed here, at the top, because the turn body has many exits — a
        // slash command short-circuits it, a cancellation unwinds it, `/quit`
        // breaks the loop — and an assigner left waiting forever is the worst
        // of them. The guard reports on whichever exit happens; the one that
        // requeues the input disarms it, because that work still has to run.
        let mut assignment_guard = assignment_turn_guard(
            assignment_enrolment.as_ref(),
            &chat_sessions,
            &chat_dispatcher,
            sessions_redraw_handle.as_ref(),
            &msg,
        );

        // Bug #3: the provider name effective for this iteration (borrowed from the mutable owned
        // value). The `/provider <name>` interception rewrites `current_provider_owned` and the
        // `provider` Arc, so on the next iteration this shadow points at the new provider name and
        // covers every later use of `provider_name` (including `/model` validation / system prompt /
        // fabric / the legacy tool-loop path's run_turn call).
        let provider_name: &str = current_provider_owned.as_str();

        // Step 5b dual write: every user input enters the dispatcher (shadow observation of the
        // reducer). InputSubmitted only records UI/LogTrace; RecordUserTurn really writes history +
        // session.turns and must only be dispatched after mem_context injection (using `enriched` so
        // it is byte-for-byte aligned with the legacy `history.push` — see the S2-B Step 4 risk
        // notes).
        if !synthetic_ui_command {
            let _ = chat_dispatcher.dispatch_or_log(
                crate::chat::action::Action::InputSubmitted(user_input.clone()),
                "chat.input_submitted",
            );
        }

        // Echo the user's input into the TUI conversation pane.
        //
        // Why: in raw-mode TUI the input box clears on submit, so without
        // this push the user has no visual confirmation of what they sent
        // until the assistant streams its reply. We push BEFORE the slash
        // command short-circuits below so that even `/quit`, `/clear`, etc.
        // produce a visible record of what was typed.
        //
        // Non-TUI (`--plain`, piped stdin, reedline fallback) is unaffected:
        // the terminal already echoes typed characters as cooked input, and
        // `redraw_tx_for_main` is `None` on those paths.
        #[cfg(feature = "terminal-tui")]
        {
            // S4-B: the legacy mirror push is deleted; the reducer is the single source through
            // UserMessageEchoed.
            if !synthetic_ui_command {
                let _ = chat_dispatcher.dispatch_or_log(
                    crate::chat::action::Action::UserMessageEchoed(user_input.clone()),
                    "chat.user_message_echoed",
                );
            }
            if let Some(tx) = redraw_tx_for_main.as_ref() {
                let _ = tx.try_send(());
            }
        }

        // Handle /quit and /exit immediately
        if is_chat_quit_command(user_input.as_str()) {
            break;
        }

        // Route any user-visible slash-command output into the right sink:
        // (defined before the bang handler so `!cmd` can emit its output).
        // ratatui mirror on the TUI path (so it survives raw-mode `\n`
        // mangling), plain stdout otherwise. Returns immediately for plain
        // mode so the legacy `--plain` / piped path is unchanged.
        #[cfg(feature = "terminal-tui")]
        let mut emit_chat_output = |text: &str| {
            // S4-B: the mirror side write is deleted; the reducer is the single source through
            // SystemMessageAdded.
            let _ = chat_dispatcher.dispatch_or_log(
                crate::chat::action::Action::SystemMessageAdded { text: text.to_string() },
                "chat.system_message_slash",
            );
            if let Some(tx) = redraw_tx_for_main.as_ref() {
                let _ = tx.try_send(());
            } else {
                print_fallback_chat_output(text);
            }
        };
        #[cfg(not(feature = "terminal-tui"))]
        let emit_chat_output = |text: &str| {
            print_fallback_chat_output(text);
        };

        if is_copy_command(&user_input) {
            match select_copy_content(&chat_session, &user_input) {
                Ok(selection) if plain_mode => {
                    println!("{}", selection.content);
                    let _ = std::io::stdout().flush();
                }
                Ok(selection) => match terminal_proto::copy_to_clipboard(&selection.content) {
                    Ok(()) => emit_chat_output(&copy_success_message(&selection)),
                    Err(error) => emit_chat_output(&format!("Copy failed: {error}")),
                },
                Err(message) => emit_chat_output(&message),
            }
            continue;
        }

        if is_queue_command(&user_input) {
            emit_chat_output(&format_input_backlog_report(&input_backlog, &turn_scheduler, 8));
            continue;
        }

        if is_workers_command(&user_input) {
            if let Some((output, signal_cancel)) = provider_workers_cancel_output_for_input(
                &msg,
                &user_input,
                &mut turn_scheduler,
                &mut provider_turn_workers,
                None,
            ) {
                emit_chat_output(&output);
                publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                dispatch_provider_worker_cancel_signal(&chat_dispatcher, signal_cancel);
            } else {
                emit_chat_output(&format_provider_worker_report(&provider_worker_status(
                    &provider_turn_workers,
                )));
            }
            continue;
        }

        // BUG-07: `/model <name>` switches the model online (same provider, different model).
        //
        // Intercepted before commands::dispatch, because taking effect requires (a) rewriting the main
        // loop's `current_model_owned` (which affects the system prompt / event records of later
        // turns), (b) writing the hot-swap slot of EffectDeps (which affects the model the dispatcher
        // subtask actually requests on the next turn), and (c) dispatching ModelChanged so the reducer
        // updates session.model → the status bar reflects it immediately. A bare `/model` (no
        // argument) is still handed to dispatch, which shows the current model.
        if let Some(raw) = user_input.strip_prefix("/model ") {
            let new_model = raw.trim();
            if new_model.is_empty() {
                emit_chat_output("Usage: /model <name>");
                continue;
            }
            let rebuilt = chat_model_switch(&config, provider_name, new_model).and_then(|()| {
                chat_provider_chain(
                    &config,
                    provider_name,
                    configured_provider_name,
                    new_model,
                    &provider_runtime_options,
                )
            });
            match rebuilt {
                Ok((_connection, built)) => {
                    let new_provider_arc: Arc<dyn Provider> = Arc::from(built);
                    let binding = match decision_source::ChatProviderBinding::capture(
                        &ctx.config_generation,
                        &new_provider_arc,
                        provider_name,
                        new_model,
                    ) {
                        Ok(binding) => binding,
                        Err(error) => {
                            emit_chat_output(&format!(
                                "Cannot bind model selection: {}",
                                providers::sanitize_api_error(&error.to_string())
                            ));
                            continue;
                        }
                    };
                    provider_binding = binding;
                    provider = Arc::clone(&new_provider_arc);
                    #[cfg(feature = "terminal-tui")]
                    if let Some(slot) = provider_slot.as_ref() {
                        slot.set(Arc::clone(&new_provider_arc));
                    }
                    current_model_owned = new_model.to_string();
                    #[cfg(feature = "terminal-tui")]
                    if let Some(slot) = model_slot.as_ref() {
                        slot.set(Arc::from(new_model));
                    }
                    #[cfg(feature = "terminal-tui")]
                    {
                        chat_mirror.lock().model = new_model.to_string();
                    }
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::ModelChanged {
                            model: new_model.to_string(),
                        },
                        "chat.model_changed",
                    );
                    // emit_chat_output already nudges the renderer (try_send on
                    // the TUI redraw channel) so the status bar repaints.
                    emit_chat_output(&format!(
                        "Switched model to {new_model} (provider {}). Applies from the next turn.",
                        providers::redact_endpoint_userinfo(&provider_name)
                    ));
                }
                Err(e) => {
                    emit_chat_output(&format!("Cannot switch to '{new_model}': {e}"));
                }
            }
            continue;
        }

        // Bug #3: `/provider <name> [model]` — hot-switch the provider within a session.
        //
        // Like `/model`, it is intercepted before commands::dispatch, because taking effect requires
        // the main loop to rebuild the provider instance and rewrite several pieces of runtime state:
        //   (a) rebuild the `Arc<dyn Provider>` from the new provider's auth/base/protocol, replacing
        //       the `provider` handle the legacy tool-loop path's `run_turn` call uses;
        //   (b) `set()` it into `provider_slot` (so the Redux driver subtask reads the new provider on
        //       its next turn);
        //   (c) rewrite `current_provider_owned` (which affects the system prompt / events / snapshot
        //       of later turns) and validate that the current model is valid for the new provider (if
        //       not, the command must carry a compatible model: `/provider <name> <model>`).
        // Credential resolution: when switching to a provider that is not the startup primary we pass
        // `api_key=None`/`api_url=None` and let the provider resolve its own credentials from the auth
        // profile / environment (reusing the startup `config.api_key` only makes sense for the
        // original primary); when switching back to the original primary we reuse
        // `config.api_key`/`config.api_url`.
        if let Some(raw) = user_input.strip_prefix("/provider ") {
            let mut parts = raw.split_whitespace();
            // Own the parsed tokens up front so we can freely reassign the runtime
            // provider/model state below without lingering borrows.
            let new_provider = parts.next().unwrap_or_default().to_string();
            let requested_model = parts.next().map(str::to_string);
            if new_provider.is_empty() {
                emit_chat_output("Usage: /provider <name> [model]");
                continue;
            }
            // Decide the model effective after the switch: prefer the one given explicitly on the
            // command, otherwise keep the current model (if compatible).
            let candidate_model = requested_model.unwrap_or_else(|| current_model_owned.clone());
            if let Err(e) = chat_model_switch(&config, &new_provider, &candidate_model) {
                let shown = providers::redact_endpoint_userinfo(&new_provider);
                emit_chat_output(&format!(
                    "Cannot switch to provider '{shown}': model '{candidate_model}' is incompatible ({}). \
Retry with a compatible model: /provider {shown} <model>",
                    providers::redact_endpoint_userinfo(&e.to_string())
                ));
                continue;
            }
            // When switching to a provider other than the configured primary, do not carry over the
            // primary's explicit credentials/URL; let the new provider resolve its own (so provider
            // A's key is never fed to provider B).
            match chat_provider_chain(
                &config,
                &new_provider,
                configured_provider_name,
                &candidate_model,
                &provider_runtime_options,
            ) {
                Ok((_switch_connection, built)) => {
                    let new_provider_arc: Arc<dyn Provider> = Arc::from(built);
                    let next_binding = match decision_source::ChatProviderBinding::capture(
                        &ctx.config_generation, &new_provider_arc, &new_provider, &candidate_model,
                    ) {
                        Ok(binding) => binding,
                        Err(error) => {
                            emit_chat_output(&format!("Cannot bind provider selection: {}", providers::sanitize_api_error(&error.to_string())));
                            continue;
                        }
                    };
                    provider_binding = next_binding;
                    // (a) the legacy path handle
                    provider = Arc::clone(&new_provider_arc);
                    // (b) Redux driver slot
                    #[cfg(feature = "terminal-tui")]
                    if let Some(slot) = provider_slot.as_ref() {
                        slot.set(Arc::clone(&new_provider_arc));
                    }
                    // (c) the runtime provider / model names
                    let model_changed = candidate_model != current_model_owned;
                    if model_changed {
                        current_model_owned = candidate_model.clone();
                        #[cfg(feature = "terminal-tui")]
                        if let Some(slot) = model_slot.as_ref() {
                            slot.set(Arc::from(candidate_model.as_str()));
                        }
                    }
                    current_provider_owned = new_provider.clone();
                    #[cfg(feature = "terminal-tui")]
                    {
                        let mut mirror = chat_mirror.lock();
                        mirror.provider = new_provider.clone();
                        if model_changed {
                            mirror.model = candidate_model.clone();
                        }
                    }
                    // (d) the session ledger: dispatch ProviderChanged so the reducer updates
                    // session.provider (and session.model when needed), making the status bar / UI
                    // snapshot reflect the new provider immediately. All three places (the legacy
                    // provider handle, the Redux provider_slot and the session ledger) stay
                    // consistent this way. When switching providers also changes the model, both go
                    // into the same action (no separate ModelChanged needed).
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::ProviderChanged {
                            provider: new_provider.clone(),
                            model: model_changed.then(|| candidate_model.clone()),
                        },
                        "chat.provider_changed",
                    );
                    let model_note = if model_changed {
                        format!(" (model set to {candidate_model})")
                    } else {
                        String::new()
                    };
                    emit_chat_output(&format!(
                        "Switched provider to {}{model_note}. Applies from the next turn.",
                        providers::redact_endpoint_userinfo(&new_provider)
                    ));
                }
                Err(e) => {
                    emit_chat_output(&format!(
                        "Cannot switch to provider '{}': {}",
                        providers::redact_endpoint_userinfo(&new_provider),
                        providers::redact_endpoint_userinfo(&e.to_string())
                    ));
                }
            }
            continue;
        }

        // BUG-07: the model name effective for this iteration (borrowed from the mutable owned
        // value). The `/model` interception above already handled its case and called `continue`, so
        // the shadowed `model_name` here is always the newest value and covers every later use of
        // `model_name` (system prompt / fabric / snapshot).
        let model_name: &str = current_model_owned.as_str();

        // BUG-04: `!cmd` bang mode — run the rest of the line directly as a
        // shell command (matching the footer hint "! for bash") instead of
        // sending it to the LLM. Output is shown inline; the LLM is not
        // involved. A bare `!` is ignored. The shell tool already applies the
        // direct host execution + workspace cwd, so bang commands share the same host FS view
        // as file_write (see BUG-02).
        if let Some(bang_cmd) = user_input.strip_prefix('!') {
            let bang_cmd = bang_cmd.trim();
            if bang_cmd.is_empty() {
                emit_chat_output("Usage: !<shell command>  (runs directly in the workspace)");
                continue;
            }
            let shell_tool = tools_registry.iter().find(|t| t.supports_name("shell"));
            match shell_tool {
                Some(tool) => {
                    let args = serde_json::json!({ "command": bang_cmd });
                    match tool.execute_named("shell", args).await {
                        Ok(result) => {
                            let mut out = String::new();
                            if !result.output.is_empty() {
                                out.push_str(&result.output);
                            }
                            if let Some(err) = result.error.as_ref().filter(|e| !e.is_empty()) {
                                if !out.is_empty() {
                                    out.push('\n');
                                }
                                out.push_str(err);
                            }
                            if out.is_empty() {
                                out = if result.success {
                                    "(no output)".to_string()
                                } else {
                                    "(command failed with no output)".to_string()
                                };
                            }
                            emit_chat_output(&out);
                        }
                        Err(e) => emit_chat_output(&format!("Shell error: {e}")),
                    }
                }
                None => emit_chat_output("Shell tool is not available in this session."),
            }
            continue;
        }

        // `/new` is a real session boundary: save the current durable state,
        // create a fresh session identity, and atomically replace every
        // session-scoped UI/model ledger. It must not share `/clear` semantics,
        // otherwise the old title, turns, and token usage survive indefinitely.
        if user_input == "/new" {
            match start_new_chat_session(
                mem.as_ref(),
                ChatSwitchCtx {
                    chat_session: &mut chat_session,
                    chat_session_key: &mut chat_session_key,
                    fabric_turn_seq: &mut fabric_turn_seq,
                    history: &mut history,
                    approval_router: approval_router.as_ref(),
                    pending_chat_rewind: &mut pending_chat_rewind,
                    pending_diff_apply: &mut pending_diff_apply,
                    chat_sessions: &mut chat_sessions,
                    ignored_session_events: &mut ignored_session_events,
                    session_rings: &mut session_rings,
                    reported_sessions: &mut reported_sessions,
                    announced_started_sessions: &mut announced_started_sessions,
                    last_sessions_summary: &mut last_sessions_summary,
                    last_sessions_entries: &mut last_sessions_entries,
                    attached_follow: &mut attached_follow,
                    attached_follow_seq: &mut attached_follow_seq,
                    chat_dispatcher: &chat_dispatcher,
                    redraw_handle: sessions_redraw_handle.as_ref(),
                    config: &config,
                    provider_name,
                    model_name,
                    skills: &skills,
                    native_tools,
                    #[cfg(feature = "terminal-tui")]
                    chat_mirror: &chat_mirror,
                },
            )
            .await
            {
                Ok(message) => emit_chat_output(&message),
                Err(e) => emit_chat_output(&e.to_string()),
            }
            continue;
        }

        // Handle /clear separately (needs mutable history). This intentionally
        // keeps the current session identity and accounting ledger.
        if user_input == "/clear" {
            history.clear();
            // S2-C Step 4: dual-write HistoryCleared to the reducer. The reducer's semantics are
            // "drain every non-system message, keep system" — legacy first clears and may then push a
            // system message (only when !skill_rag.enabled); both end states are "system only" (or
            // empty when skill_rag.enabled). During the dual-write period the two paths reach the same
            // end state but differ in between:
            //   - legacy: clear() empties history → may push system
            //   - reducer: HistoryCleared keeps the existing system (it does not rebuild it)
            // On the real production path legacy later pushes a newly built system message (which
            // overwrites the skill list of the old system), while the reducer's system is still the
            // previous one. This S2-C stage does not correct that — legacy is still the real context
            // source for the LLM and the reducer is the observation ledger.
            if !config.skill_rag.available() {
                let cleared_system = build_runtime_system_prompt(&config, model_name, &skills, native_tools);
                history.push(ChatMessage::system(cleared_system.clone()));
                // S2-C Step 4 (Codex P0 correction): use SetLeadingSystemPrompt rather than
                // RecordSystemMessage. The reducer's HistoryCleared means "drain non-system, keep
                // system" — the previous system message is still there; using RecordSystemMessage
                // (append) here would produce a duplicate system message and accumulate more over
                // time. SetLeadingSystemPrompt is an upsert: it replaces the existing leading system
                // message or pushes into an empty history, which matches the end state of the legacy
                // `clear + push` (at most 1 system message).
                let _ = chat_dispatcher.dispatch_or_log(
                    crate::chat::action::Action::SetLeadingSystemPrompt {
                        content: cleared_system,
                    },
                    "chat.system_prompt_after_clear",
                );
            }
            let cleared = commands::handle_clear(mem.as_ref(), Some(&chat_session.id)).await;
            let msg = commands::format_clear_feedback(cleared);
            #[cfg(feature = "terminal-tui")]
            {
                let _ = chat_dispatcher.dispatch_or_log(
                    crate::chat::action::Action::HistoryClearedWithNotice { notice: msg.clone() },
                    "chat.history_cleared_with_notice",
                );
                if let Some(tx) = redraw_tx_for_main.as_ref() {
                    let _ = tx.try_send(());
                } else {
                    print_fallback_chat_output(&msg);
                }
            }
            #[cfg(not(feature = "terminal-tui"))]
            {
                print_fallback_chat_output(&msg);
            }
            continue;
        }

        // `/compact` mutates the real LLM context history before the next turn.
        // Prefer the provider-backed summary patch used by the Redux driver; if
        // the summary call fails or times out, fall back to deterministic trim.
        if matches!(user_input.as_str(), "/compact") {
            let compact_context = crate::router::resolve_effective_compaction_config(
                &config.agent.compaction,
                provider_name,
                model_name,
                &config.router,
                &config.model_routes,
            );
            if manual_compact_below_trigger_threshold(&history, &compact_context.config) {
                emit_chat_output(&format_nothing_to_compact_feedback(&history, &compact_context.config));
                continue;
            }
            emit_chat_output("Compacting conversation...");
            let mut compact_source_history = Vec::with_capacity(
                chat_session
                    .turns
                    .len()
                    .saturating_add(usize::from(!history.is_empty())),
            );
            if let Some(system) = history.first().filter(|message| message.role == "system") {
                compact_source_history.push(system.clone());
            }
            compact_source_history.extend(session_turns_to_history(&chat_session));
            let system_count = usize::from(history.first().is_some_and(|m| m.role == "system"));
            let turns_before = history.len().saturating_sub(system_count);
            let tokens_before = estimate_chat_history_tokens(&history);
            if let Some(patch) = build_chat_compaction_patch(
                &history,
                &compact_source_history,
                provider.as_ref(),
                model_name,
                &compact_context.config,
                None,
                "chat_manual_compact",
            )
            .await
            {
                apply_chat_compaction_patch_and_sync(
                    &mut history,
                    Some(&compact_source_history),
                    patch,
                    &compact_context.config,
                    crate::chat::action::CompactReason::Manual,
                    &chat_dispatcher,
                );
            } else {
                compact_chat_history(&mut history);
                let _ = chat_dispatcher.dispatch_or_log(
                    crate::chat::action::Action::HistoryCompacted {
                        reason: crate::chat::action::CompactReason::Manual,
                    },
                    "chat.history_compacted_manual",
                );
            }
            #[cfg(feature = "terminal-tui")]
            refresh_context_budget_for_tui(
                &history,
                &compact_context.config,
                redraw_tx_for_main.is_some(),
                &chat_mirror,
                &chat_dispatcher,
            );

            let msg =
                format_compact_feedback_after_history(turns_before, tokens_before, &history, &compact_context.config);
            emit_chat_output(&msg);
            continue;
        }

        // Dispatch other slash commands
        {
            let cmd_ctx = commands::CommandContext {
                model_name,
                provider_name,
                chat_session: &chat_session,
                tools_registry: &tools_registry,
                mem: mem.as_ref(),
            };
            match commands::dispatch(&user_input, &cmd_ctx).await {
                commands::CommandResult::Handled => continue,
                commands::CommandResult::HandledWithOutput(text) => {
                    emit_chat_output(&text);
                    continue;
                }
                commands::CommandResult::Quit => break,
                commands::CommandResult::SetMode(mode) => {
                    // Pure skips the legacy chat_session.set_mode; in legacy mode
                    // run_turn still reads it.
                    let _ = chat_dispatcher
                        .dispatch_or_log(crate::chat::action::Action::ModeChanged(mode), "chat.mode_changed");
                    #[cfg(feature = "terminal-tui")]
                    {
                        chat_mirror.lock().chat_mode = mode;
                    }
                    #[cfg(feature = "terminal-tui")]
                    let legacy_session_mode_writes_enabled = false; // S4-B: Pure single source
                    #[cfg(not(feature = "terminal-tui"))]
                    let legacy_session_mode_writes_enabled = true;
                    if legacy_session_mode_writes_enabled {
                        chat_session.set_mode(mode);
                    }
                    let msg = match mode {
                        commands::ChatMode::Plan => {
                            "Switched to plan mode (read-only tools only — write/shell/git_commit will be simulated)"
                        }
                        commands::ChatMode::Edit => "Switched to edit mode (default — write tools enabled)",
                        commands::ChatMode::Auto => "Switched to auto mode (does not override [autonomy])",
                    };
                    emit_chat_output(msg);
                    continue;
                }
                commands::CommandResult::ResumeAction(action) => {
                    match action {
                        commands::ResumeCommand::List => match saved_chat_sessions(mem.as_ref()).await {
                            Ok(sessions) => {
                                #[cfg(feature = "terminal-tui")]
                                if sessions_redraw_handle.is_some() {
                                    let entries = sessions
                                        .iter()
                                        .map(|session| {
                                            crate::chat::session::SavedSessionPickerEntry::from_session(
                                                session,
                                                &chat_session.id,
                                            )
                                        })
                                        .collect::<Vec<_>>();
                                    {
                                        let mut mirror = chat_mirror.lock();
                                        mirror.saved_sessions_cache = entries.clone();
                                        mirror.saved_session_picker =
                                            Some(crate::chat::session::SavedSessionPickerState::new(entries.clone()));
                                    }
                                    let _ = chat_dispatcher.dispatch_or_log(
                                        crate::chat::action::Action::SavedSessionPickerOpened { entries },
                                        "chat.saved_session_picker_opened_resume",
                                    );
                                    let sources = {
                                        let mirror = chat_mirror.lock();
                                        crate::chat::action::Action::SlashMenuSourcesUpdated {
                                            saved_sessions: mirror.saved_sessions_cache.clone(),
                                            provider_model_catalog: mirror.provider_model_catalog.clone(),
                                        }
                                    };
                                    let _ = chat_dispatcher.dispatch_or_log(sources, "chat.slash_menu_sources_resume");
                                    if let Some(tx) = sessions_redraw_handle.as_ref() {
                                        let _ = tx.try_send(());
                                    }
                                } else {
                                    emit_chat_output(&format_saved_chat_sessions(&sessions));
                                }
                                #[cfg(not(feature = "terminal-tui"))]
                                emit_chat_output(&format_saved_chat_sessions(&sessions));
                            }
                            Err(e) => emit_chat_output(&format!("Failed to list saved chat sessions: {e}")),
                        },
                        commands::ResumeCommand::Id(id) => {
                            match resume_saved_session_by_id(
                                mem.as_ref(),
                                &id,
                                ChatSwitchCtx {
                                    chat_session: &mut chat_session,
                                    chat_session_key: &mut chat_session_key,
                                    fabric_turn_seq: &mut fabric_turn_seq,
                                    history: &mut history,
                                    approval_router: approval_router.as_ref(),
                                    pending_chat_rewind: &mut pending_chat_rewind,
                                    pending_diff_apply: &mut pending_diff_apply,
                                    chat_sessions: &mut chat_sessions,
                                    ignored_session_events: &mut ignored_session_events,
                                    session_rings: &mut session_rings,
                                    reported_sessions: &mut reported_sessions,
                                    announced_started_sessions: &mut announced_started_sessions,
                                    last_sessions_summary: &mut last_sessions_summary,
                                    last_sessions_entries: &mut last_sessions_entries,
                                    attached_follow: &mut attached_follow,
                                    attached_follow_seq: &mut attached_follow_seq,
                                    chat_dispatcher: &chat_dispatcher,
                                    redraw_handle: sessions_redraw_handle.as_ref(),
                                    config: &config,
                                    provider_name,
                                    model_name,
                                    skills: &skills,
                                    native_tools,
                                    #[cfg(feature = "terminal-tui")]
                                    chat_mirror: &chat_mirror,
                                },
                            )
                            .await
                            {
                                Ok(message) => emit_chat_output(&message),
                                Err(e) => emit_chat_output(&e.to_string()),
                            }
                        }
                        commands::ResumeCommand::Last => {
                            let current_child_summaries = chat_sessions
                                .snapshot()
                                .await
                                .iter()
                                .map(|view| {
                                    crate::chat::sessions::PersistedSessionSummary::from_view(view, String::new())
                                })
                                .collect::<Vec<_>>();
                            let mut current_to_save = chat_session.clone();
                            for summary in &current_child_summaries {
                                current_to_save.record_background_session(summary.clone());
                            }

                            if let Err(e) = save_session(mem.as_ref(), &current_to_save).await {
                                emit_chat_output(&format!(
                                    "Resume aborted: failed to save current session before switching: {e}"
                                ));
                                continue;
                            }
                            for summary in &current_child_summaries {
                                let _ = chat_dispatcher.dispatch_or_log(
                                    crate::chat::action::Action::BackgroundSessionRecorded {
                                        summary: summary.clone(),
                                    },
                                    "chat.resume_record_child_summary_before_switch",
                                );
                            }

                            let loaded = match load_latest_session_with_message_events(
                                mem.as_ref(),
                                memory_fabric.workspace_id(),
                                &config.cost,
                            )
                            .await
                            {
                                Ok(Some(session)) => Some(session),
                                Ok(None) => {
                                    emit_chat_output("No saved chat sessions to resume.");
                                    None
                                }
                                Err(e) => {
                                    emit_chat_output(&format!(
                                        "Resume aborted: failed to load saved chat session: {e}"
                                    ));
                                    None
                                }
                            };
                            let Some(loaded_session) = loaded else {
                                continue;
                            };

                            apply_chat_session_switch(
                                ChatSwitchCtx {
                                    chat_session: &mut chat_session,
                                    chat_session_key: &mut chat_session_key,
                                    fabric_turn_seq: &mut fabric_turn_seq,
                                    history: &mut history,
                                    approval_router: approval_router.as_ref(),
                                    pending_chat_rewind: &mut pending_chat_rewind,
                                    pending_diff_apply: &mut pending_diff_apply,
                                    chat_sessions: &mut chat_sessions,
                                    ignored_session_events: &mut ignored_session_events,
                                    session_rings: &mut session_rings,
                                    reported_sessions: &mut reported_sessions,
                                    announced_started_sessions: &mut announced_started_sessions,
                                    last_sessions_summary: &mut last_sessions_summary,
                                    last_sessions_entries: &mut last_sessions_entries,
                                    attached_follow: &mut attached_follow,
                                    attached_follow_seq: &mut attached_follow_seq,
                                    chat_dispatcher: &chat_dispatcher,
                                    redraw_handle: sessions_redraw_handle.as_ref(),
                                    config: &config,
                                    provider_name,
                                    model_name,
                                    skills: &skills,
                                    native_tools,
                                    #[cfg(feature = "terminal-tui")]
                                    chat_mirror: &chat_mirror,
                                },
                                loaded_session,
                            )
                            .await;

                            let title = if chat_session.title.is_empty() {
                                "(untitled)"
                            } else {
                                chat_session.title.as_str()
                            };
                            emit_chat_output(&format!(
                                "Resumed saved chat session {} ({title}, {} turns).",
                                chat_session.id,
                                chat_session.turn_count()
                            ));
                        }
                    }
                    continue;
                }
                commands::CommandResult::HistoryAction(action) => {
                    match action {
                        commands::HistoryCommand::BranchList => {
                            emit_chat_output(&format_turn_boundaries(&chat_session));
                        }
                        commands::HistoryCommand::Branch(raw) => {
                            let keep_turns = match parse_turn_boundary(&raw, chat_session.turn_count(), "branch") {
                                Ok(value) => value,
                                Err(msg) => {
                                    emit_chat_output(&msg);
                                    continue;
                                }
                            };
                            let current_child_summaries = chat_sessions
                                .snapshot()
                                .await
                                .iter()
                                .map(|view| {
                                    crate::chat::sessions::PersistedSessionSummary::from_view(view, String::new())
                                })
                                .collect::<Vec<_>>();
                            let mut current_to_save = chat_session.clone();
                            for summary in current_child_summaries {
                                current_to_save.record_background_session(summary);
                            }
                            if let Err(e) = save_session(mem.as_ref(), &current_to_save).await {
                                emit_chat_output(&format!(
                                    "Branch aborted: failed to save current session before forking: {e}"
                                ));
                                continue;
                            }
                            let branch =
                                branched_chat_session_from(&current_to_save, keep_turns, provider_name, model_name);
                            let branch_id = branch.id.clone();
                            let branch_turns = branch.turn_count();
                            if let Err(e) = save_session(mem.as_ref(), &branch).await {
                                emit_chat_output(&format!("Branch aborted: failed to save new branch session: {e}"));
                                continue;
                            }
                            apply_chat_session_switch(
                                ChatSwitchCtx {
                                    chat_session: &mut chat_session,
                                    chat_session_key: &mut chat_session_key,
                                    fabric_turn_seq: &mut fabric_turn_seq,
                                    history: &mut history,
                                    approval_router: approval_router.as_ref(),
                                    pending_chat_rewind: &mut pending_chat_rewind,
                                    pending_diff_apply: &mut pending_diff_apply,
                                    chat_sessions: &mut chat_sessions,
                                    ignored_session_events: &mut ignored_session_events,
                                    session_rings: &mut session_rings,
                                    reported_sessions: &mut reported_sessions,
                                    announced_started_sessions: &mut announced_started_sessions,
                                    last_sessions_summary: &mut last_sessions_summary,
                                    last_sessions_entries: &mut last_sessions_entries,
                                    attached_follow: &mut attached_follow,
                                    attached_follow_seq: &mut attached_follow_seq,
                                    chat_dispatcher: &chat_dispatcher,
                                    redraw_handle: sessions_redraw_handle.as_ref(),
                                    config: &config,
                                    provider_name,
                                    model_name,
                                    skills: &skills,
                                    native_tools,
                                    #[cfg(feature = "terminal-tui")]
                                    chat_mirror: &chat_mirror,
                                },
                                branch,
                            )
                            .await;
                            emit_chat_output(&format!(
                                "Created branch {branch_id} from the first {branch_turns} turns and switched to it."
                            ));
                        }
                        commands::HistoryCommand::Rewind(raw) => {
                            let keep_turns = match parse_turn_boundary(&raw, chat_session.turn_count(), "rewind") {
                                Ok(value) => value,
                                Err(msg) => {
                                    emit_chat_output(&msg);
                                    continue;
                                }
                            };
                            if keep_turns == chat_session.turn_count() {
                                emit_chat_output(&format!(
                                    "Rewind skipped: session already has exactly {keep_turns} turns."
                                ));
                                continue;
                            }
                            if approval_in_progress(approval_router.as_ref(), &pending_chat_rewind, &pending_diff_apply)
                            {
                                emit_chat_output(approval_already_pending_message());
                                continue;
                            }
                            if sessions_redraw_handle.is_none() {
                                emit_chat_output(
                                    "Rewind requires interactive confirmation; unavailable in this mode. Current session unchanged.",
                                );
                                continue;
                            }
                            #[cfg(not(feature = "terminal-tui"))]
                            {
                                emit_chat_output(
                                    "Rewind requires interactive confirmation; unavailable in this mode. Current session unchanged.",
                                );
                                continue;
                            }
                            #[cfg(feature = "terminal-tui")]
                            {
                                let Some(router) = approval_router.as_ref() else {
                                    emit_chat_output(
                                        "Rewind requires interactive confirmation; approval router unavailable. Current session unchanged.",
                                    );
                                    continue;
                                };
                                let target_session = rewound_chat_session_from(&chat_session, keep_turns);
                                let (approval_tx, approval_rx) = tokio::sync::oneshot::channel::<bool>();
                                let tool_id = format!("chat_rewind:{}", uuid::Uuid::new_v4());
                                if !router.register(tool_id.clone(), approval_tx) {
                                    emit_chat_output(approval_already_pending_message());
                                    continue;
                                }
                                let args = serde_json::json!({
                                    "session_id": chat_session.id,
                                    "from_turns": chat_session.turn_count(),
                                    "to_turns": keep_turns,
                                    "drops_child_summaries": keep_turns < chat_session.turn_count(),
                                })
                                .to_string();
                                let dispatch_result = chat_dispatcher.dispatch_or_log(
                                    crate::chat::action::Action::ToolApprovalRequested {
                                        task_id: None,
                                        tool_id: tool_id.clone(),
                                        name: "rewind_chat_session".to_string(),
                                        args,
                                    },
                                    "chat.rewind_approval_requested",
                                );
                                if dispatch_result != crate::chat::dispatcher::DispatchResult::Sent {
                                    let _ = router.resolve(&tool_id, false);
                                    emit_chat_output("Rewind approval could not be shown; current session unchanged.");
                                    continue;
                                }
                                pending_chat_rewind = Some(PendingChatRewind {
                                    tool_id,
                                    target_session,
                                    approval_rx,
                                });
                                emit_chat_output(&format!(
                                    "Confirm rewind to {keep_turns} turns in the approval prompt."
                                ));
                            }
                        }
                    }
                    continue;
                }
                commands::CommandResult::ApplyAction(action) => {
                    let latest_index = match action {
                        commands::ApplyCommand::Latest => 1,
                        commands::ApplyCommand::Index(index) => index,
                    };
                    if approval_in_progress(approval_router.as_ref(), &pending_chat_rewind, &pending_diff_apply) {
                        emit_chat_output(approval_already_pending_message());
                        continue;
                    }
                    let Some(diff) = diff_apply::latest_fenced_diff(&chat_session.turns, latest_index) else {
                        emit_chat_output("No applicable fenced diff block found in this conversation.");
                        continue;
                    };
                    let plan = match diff_apply::parse_unified_diff(&diff) {
                        Ok(plan) => plan,
                        Err(error) => {
                            emit_chat_output(&format!("Diff apply rejected: {error}. Workspace unchanged."));
                            continue;
                        }
                    };
                    #[cfg(not(feature = "terminal-tui"))]
                    let _ = &plan;
                    if sessions_redraw_handle.is_none() {
                        emit_chat_output(
                            "Diff apply requires interactive TUI approval; unavailable in this mode. Workspace unchanged.",
                        );
                        continue;
                    }
                    #[cfg(not(feature = "terminal-tui"))]
                    {
                        emit_chat_output(
                            "Diff apply requires interactive TUI approval; unavailable in this build. Workspace unchanged.",
                        );
                        continue;
                    }
                    #[cfg(feature = "terminal-tui")]
                    {
                        match request_diff_apply_approval(plan, true, approval_router.as_ref(), &chat_dispatcher) {
                            Ok(pending) => {
                                let summary = pending.plan.summary();
                                pending_diff_apply = Some(pending);
                                emit_chat_output(&format!("Confirm diff apply in the approval prompt.\n{summary}"));
                            }
                            Err(message) => emit_chat_output(&message),
                        }
                    }
                    #[cfg(feature = "terminal-tui")]
                    continue;
                }
                commands::CommandResult::SessionAction(action) => {
                    use crate::chat::sessions::SessionCommand;
                    // Shared parsing must not turn mailbox/model input into
                    // local operator authority for a child or approval action.
                    if action.requires_local_operator() && !is_local_operator_input(&msg) {
                        emit_chat_output("This session command requires local terminal operator input.");
                        if let Some(guard) = assignment_guard.as_mut() {
                            guard.failed("Assigned task cannot issue local operator session commands.");
                        }
                        continue;
                    }
                    match action {
                        SessionCommand::Bg { task } => {
                            // Spawn a background agent via sessions_spawn, passing
                            // the *current* provider/model (read from the main-loop
                            // strings, which `/provider` and `/model` keep in sync)
                            // so a hot switch is honoured (plan §C.0 blocker 3).
                            match tools_registry.iter().find(|t| t.supports_name("sessions_spawn")) {
                                Some(tool) => {
                                    let command = provider_binding.as_ref()
                                        .ok_or_else(|| anyhow::anyhow!("chat provider binding is missing"))
                                        .and_then(|binding| decision_source::ChatOperatorSpawn::capture(
                                            &msg, &task, binding, &ctx.config_generation, &provider,
                                            &current_provider_owned, &current_model_owned,
                                        ));
                                    let command = match command {
                                        Ok(command) => command,
                                        Err(error) => {
                                            emit_chat_output(&format!("Cannot admit background command: {error}"));
                                            continue;
                                        }
                                    };
                                    let (mut args, operator_source) = match command.admit(Arc::clone(&mem), &chat_session_key, Arc::clone(&ctx.security), Arc::clone(&turn_decisions)).await {
                                        Ok(admitted) => admitted,
                                        Err(error) => { emit_chat_output(&format!("Cannot record background command source: {error}")); continue; }
                                    };
                                    // Spawning is a Medium-risk side effect; under
                                    // supervised autonomy the gate requires a grant
                                    // bound to `sessions_spawn:spawn`. The operator
                                    // typed `/bg`, so issue the matching grant here
                                    // (same op name the gate authorizes), mirroring
                                    // `/kill` and `/steer` and how the agent loop
                                    // grants after operator approval.
                                    let grant = crate::security::policy::ApprovalGrant::for_resource_operation(
                                        "sessions_spawn",
                                        "sessions_spawn:spawn",
                                        "chat-operator",
                                        None,
                                    );
                                    match serde_json::to_value(&grant) {
                                        Ok(grant_value) => {
                                            if let Some(obj) = args.as_object_mut() {
                                                obj.insert(
                                                    crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.to_string(),
                                                    grant_value,
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                error = %e,
                                                "Failed to serialize spawn approval grant; proceeding without it"
                                            );
                                        }
                                    }
                                    match operator_source.scope(tool.execute_named("sessions_spawn", args)).await {
                                        Ok(result) => {
                                            let out = if result.output.is_empty() {
                                                result
                                                    .error
                                                    .filter(|e| !e.is_empty())
                                                    .unwrap_or_else(|| "(no output)".to_string())
                                            } else {
                                                result.output
                                            };
                                            emit_chat_output(&out);
                                        }
                                        Err(e) => emit_chat_output(&format!("Failed to start background agent: {e}")),
                                    }
                                }
                                None => emit_chat_output("Background sessions are not available in this session."),
                            }
                            continue;
                        }
                        SessionCommand::Sessions => {
                            if let Some(output) = handle_local_session_command(
                                &action,
                                &mut chat_sessions,
                                &session_rings,
                                &mut reaped_log_archive,
                                &reap_policy,
                                &tools_registry,
                            )
                            .await
                            {
                                emit_chat_output(&output);
                            }
                            continue;
                        }
                        SessionCommand::Transcript => {
                            attached_follow = None;
                            attached_follow_seq = None;
                            #[cfg(feature = "terminal-tui")]
                            {
                                open_transcript_view(
                                    &chat_mirror,
                                    &chat_dispatcher,
                                    sessions_redraw_handle.as_ref(),
                                    snapshot_rx_for_tui.as_ref(),
                                );
                            }
                            #[cfg(not(feature = "terminal-tui"))]
                            emit_chat_output("Transcript viewer is only available in the terminal TUI.");
                            continue;
                        }
                        SessionCommand::Diff { cached } => {
                            attached_follow = None;
                            attached_follow_seq = None;
                            let source = collect_workspace_diff(&config.workspace_dir, cached).await;
                            #[cfg(feature = "terminal-tui")]
                            {
                                if sessions_redraw_handle.is_some() {
                                    open_diff_view(
                                        &chat_mirror,
                                        &chat_dispatcher,
                                        sessions_redraw_handle.as_ref(),
                                        source,
                                    );
                                } else {
                                    emit_chat_output(&source.to_plain_text());
                                }
                            }
                            #[cfg(not(feature = "terminal-tui"))]
                            emit_chat_output(&source.to_plain_text());
                            continue;
                        }
                        SessionCommand::Kill { seq: _ } => {
                            if let Some(output) = handle_local_session_command(
                                &action,
                                &mut chat_sessions,
                                &session_rings,
                                &mut reaped_log_archive,
                                &reap_policy,
                                &tools_registry,
                            )
                            .await
                            {
                                emit_chat_output(&output);
                            }
                            continue;
                        }
                        SessionCommand::Steer { seq, message } => {
                            // v5: steer only applies to agent sessions (it appends
                            // an instruction to a running sub-agent's steer
                            // channel). Shells and PTYs have no steer channel —
                            // resolving their seq would yield a non-agent id that
                            // the sessions_spawn tool can't address, producing a
                            // cryptic "run not found". Guard with a clear message
                            // up front, mirroring `/kill`'s kind dispatch.
                            match chat_sessions.kind_for_seq(seq).await {
                                Ok(kind) => {
                                    if let Some(msg) =
                                        crate::chat::sessions::command::steer_unsupported_message(kind, seq)
                                    {
                                        emit_chat_output(&msg);
                                        continue;
                                    }
                                    // Agent: fall through to the steer delegation.
                                }
                                Err(e) => {
                                    emit_chat_output(&format!("Steer failed: {e}"));
                                    continue;
                                }
                            }
                            // Resolve `#N` -> run UUID, then delegate to the
                            // sessions_spawn tool's `steer` action so the shared
                            // semantics apply uniformly (Low-risk side-effect gate
                            // op `sessions_spawn:steer:<run_id>`, running-status
                            // check, steer_tx delivery). Mirrors `/kill`.
                            let run_id = match chat_sessions.resolve_run_id(seq).await {
                                Ok(id) => id,
                                Err(e) => {
                                    emit_chat_output(&format!("Steer failed: {e}"));
                                    continue;
                                }
                            };
                            match tools_registry.iter().find(|t| t.supports_name("sessions_spawn")) {
                                Some(tool) => {
                                    let operation_name = format!("sessions_spawn:steer:{run_id}");
                                    let grant = crate::security::policy::ApprovalGrant::for_resource_operation(
                                        "sessions_spawn",
                                        &operation_name,
                                        "chat-operator",
                                        None,
                                    );
                                    let mut args = serde_json::json!({
                                        "action": "steer",
                                        "run_id": run_id,
                                        "message": message,
                                    });
                                    match serde_json::to_value(&grant) {
                                        Ok(grant_value) => {
                                            if let Some(obj) = args.as_object_mut() {
                                                obj.insert(
                                                    crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.to_string(),
                                                    grant_value,
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                error = %e,
                                                "Failed to serialize steer approval grant; proceeding without it"
                                            );
                                        }
                                    }
                                    match tool.execute_named("sessions_spawn", args).await {
                                        Ok(result) => {
                                            let out = if result.output.is_empty() {
                                                result
                                                    .error
                                                    .filter(|e| !e.is_empty())
                                                    .unwrap_or_else(|| "(no output)".to_string())
                                            } else {
                                                result.output
                                            };
                                            emit_chat_output(&out);
                                        }
                                        Err(e) => emit_chat_output(&format!("Steer failed: {e}")),
                                    }
                                }
                                None => emit_chat_output("Background sessions are not available in this session."),
                            }
                            continue;
                        }
                        SessionCommand::Attach { seq } => {
                            // P2 `/attach` focuses the child session viewport.
                            // Main history receives only a breadcrumb; retained
                            // and live child output render inside ActiveSessionView.
                            //
                            // v5: PTY sessions are interactive terminal handoffs,
                            // not line-streamed output, so a read-only follow makes
                            // no sense for them. The Ctrl+G switcher routes Enter
                            // through this same `/attach <seq>` path for every kind,
                            // so guard PTYs here with a clear redirect rather than
                            // silently starting an empty follow. (A live PTY can be
                            // re-entered with `/pty`; an exited one is terminal.)
                            #[cfg(feature = "terminal-tui")]
                            if matches!(
                                chat_sessions.kind_for_seq(seq).await,
                                Ok(crate::chat::sessions::model::ManagedKind::Pty)
                            ) {
                                // v3b: a live PTY can be RE-ATTACHED — the detach
                                // path keeps the child running, so `/attach #N`
                                // (and the Ctrl+G switcher's synthetic /attach)
                                // hands the terminal back to it. An exited PTY is
                                // terminal and cannot be attached.
                                let pty = chat_sessions.pty_for_seq_public(seq);
                                match pty {
                                    Some(session) if session.is_attachable() => {
                                        reattach_pty(
                                            &session,
                                            seq,
                                            &pty_handoff,
                                            sessions_redraw_handle.as_ref(),
                                            &emit_chat_output,
                                        )
                                        .await;
                                    }
                                    _ => {
                                        emit_chat_output(&format!(
                                            "Interactive PTY session #{seq} has exited — nothing to attach to. \
                                             Start a new one with /pty <command>."
                                        ));
                                    }
                                }
                                // Restore the prompt/focus the switcher may have set
                                // optimistically (it pointed at this seq before the
                                // handoff), so the chat prompt is not left targeting
                                // the detached PTY for steering.
                                let prev_focus = crate::chat::sessions::focus::rollback_focus(attached_follow_seq);
                                let _ = chat_dispatcher.dispatch_or_log(
                                    crate::chat::action::Action::SessionFocusChanged { focus: prev_focus },
                                    "chat.session_focus_attach_pty_done",
                                );
                                {
                                    let mut mirror = chat_mirror.lock();
                                    mirror.focus = prev_focus;
                                    mirror.active_session_view = None;
                                }
                                let _ = chat_dispatcher.dispatch_or_log(
                                    crate::chat::action::Action::ActiveSessionViewUpdated { view: None },
                                    "chat.active_session_view_attach_pty_done",
                                );
                                if let Some(tx) = sessions_redraw_handle.as_ref() {
                                    let _ = tx.try_send(());
                                }
                                continue;
                            }
                            const ATTACH_TAIL_LINES: usize = 20;
                            match chat_sessions.resolve_run_id(seq).await {
                                Ok(run_id) => {
                                    let sid = crate::chat::sessions::id::SessionId::from_run_id(&run_id);
                                    let was_following_before_attach = attached_follow.is_some();
                                    let is_terminal = chat_sessions.is_terminal_for_seq(seq).await.unwrap_or(false);
                                    let views = chat_sessions.snapshot().await;
                                    let view_meta = views.iter().find(|view| view.seq == seq);
                                    let tail_lines = match chat_sessions.tail(seq, ATTACH_TAIL_LINES).await {
                                        Ok(lines) => lines
                                            .into_iter()
                                            .map(|line| format!("[{}] {}", line.role, line.content))
                                            .collect::<Vec<_>>(),
                                        Err(e) => {
                                            emit_chat_output(&format!("Attach tail failed: {e}"));
                                            Vec::new()
                                        }
                                    };
                                    let (ring_lines, truncated) = session_rings.get(&sid).map_or_else(
                                        || (Vec::new(), false),
                                        |ring| {
                                            let lines = if is_terminal {
                                                Vec::new()
                                            } else {
                                                ring.recent_lines(crate::chat::sessions::event::DEFAULT_RING_CAPACITY)
                                            };
                                            (lines, ring.is_truncated())
                                        },
                                    );
                                    #[cfg(not(feature = "terminal-tui"))]
                                    let _ = (
                                        &was_following_before_attach,
                                        &view_meta,
                                        &tail_lines,
                                        &ring_lines,
                                        truncated,
                                    );
                                    #[cfg(feature = "terminal-tui")]
                                    let active_projection = build_active_session_attach_projection(
                                        seq, view_meta, tail_lines, ring_lines, truncated,
                                    );
                                    attached_follow = Some(sid);
                                    attached_follow_seq = Some(seq);
                                    // v1.1b: route plain input to this session as
                                    // a steer and reflect the target in the prompt
                                    // (colour+glyph). Update both the render
                                    // snapshot (Action) and the key thread's
                                    // mirror (read by `resolve_esc`). When the
                                    // attach was triggered from the switcher the
                                    // key thread already set this optimistically;
                                    // re-affirming it here is idempotent and also
                                    // covers the typed `/attach N` path.
                                    let focus = crate::chat::sessions::FocusTarget::Session { seq };
                                    let _ = chat_dispatcher.dispatch_or_log(
                                        crate::chat::action::Action::SessionFocusChanged { focus },
                                        "chat.session_focus_attach",
                                    );
                                    #[cfg(feature = "terminal-tui")]
                                    {
                                        {
                                            let mut mirror = chat_mirror.lock();
                                            mirror.focus = focus;
                                            mirror.active_session_view = Some(active_projection.view.clone());
                                        }
                                        let _ = chat_dispatcher.dispatch_or_log(
                                            crate::chat::action::Action::ActiveSessionViewUpdated {
                                                view: Some(active_projection.view.clone()),
                                            },
                                            "chat.active_session_view_attach",
                                        );
                                        if let Some(tx) = sessions_redraw_handle.as_ref() {
                                            let _ = tx.try_send(());
                                        }
                                    }
                                    #[cfg(feature = "terminal-tui")]
                                    if let Some(breadcrumb) = attach_breadcrumb_for_transition(
                                        was_following_before_attach,
                                        &active_projection,
                                    ) {
                                        emit_chat_output(breadcrumb);
                                    }
                                    #[cfg(not(feature = "terminal-tui"))]
                                    emit_chat_output(&format!(
                                        "Attached session #{seq} (input routes as steer). Type /detach to stop."
                                    ));
                                }
                                Err(e) => {
                                    // P0 race fix: the switcher key thread may have
                                    // optimistically pointed the prompt + Esc
                                    // judgment at `seq` before this attach ran. The
                                    // attach failed (seq no longer resolves / the
                                    // session is gone), so `attached_follow` is
                                    // unchanged — restore the prompt to the *actual*
                                    // current target so perception cannot diverge
                                    // from routing. (A typed `/attach N` that fails
                                    // has no optimistic set, but restoring the same
                                    // unchanged focus is an idempotent no-op there.)
                                    let prev_focus = crate::chat::sessions::focus::rollback_focus(attached_follow_seq);
                                    let _ = chat_dispatcher.dispatch_or_log(
                                        crate::chat::action::Action::SessionFocusChanged { focus: prev_focus },
                                        "chat.session_focus_attach_rollback",
                                    );
                                    #[cfg(feature = "terminal-tui")]
                                    {
                                        {
                                            let mut mirror = chat_mirror.lock();
                                            mirror.focus = prev_focus;
                                            mirror.active_session_view = None;
                                        }
                                        let _ = chat_dispatcher.dispatch_or_log(
                                            crate::chat::action::Action::ActiveSessionViewUpdated { view: None },
                                            "chat.active_session_view_attach_rollback",
                                        );
                                        if let Some(tx) = sessions_redraw_handle.as_ref() {
                                            let _ = tx.try_send(());
                                        }
                                    }
                                    emit_chat_output(&format!("Attach failed: {e}"));
                                }
                            }
                            continue;
                        }
                        SessionCommand::Detach => {
                            let was_following = attached_follow.take().is_some();
                            attached_follow_seq = None;
                            if was_following {
                                // v1.1b: reset input routing back to main and clear
                                // the prompt target indicator (snapshot + mirror).
                                let focus = crate::chat::sessions::FocusTarget::Main;
                                let _ = chat_dispatcher.dispatch_or_log(
                                    crate::chat::action::Action::SessionFocusChanged { focus },
                                    "chat.session_focus_detach",
                                );
                                #[cfg(feature = "terminal-tui")]
                                {
                                    {
                                        let mut mirror = chat_mirror.lock();
                                        mirror.focus = focus;
                                        mirror.active_session_view = None;
                                    }
                                    let _ = chat_dispatcher.dispatch_or_log(
                                        crate::chat::action::Action::ActiveSessionViewUpdated { view: None },
                                        "chat.active_session_view_detach",
                                    );
                                    if let Some(tx) = sessions_redraw_handle.as_ref() {
                                        let _ = tx.try_send(());
                                    }
                                }
                                emit_chat_output("Detached. Input routes to main chat again.");
                            } else {
                                emit_chat_output("Not currently following any session.");
                            }
                            continue;
                        }
                        SessionCommand::Shell { command } => {
                            // v2: run a non-interactive command in the background.
                            // Runs directly in the configured workspace with the
                            // inherited host environment and the v1.1 event bridge.
                            match crate::chat::sessions::shell::spawn_shell(
                                &command,
                                &config.workspace_dir,
                                &shell_event_sink,
                            ) {
                                Ok(session) => {
                                    let seq = chat_sessions.add_shell(session);
                                    emit_chat_output(&format!("Started background shell #{seq}: {command}"));
                                }
                                Err(e) => {
                                    emit_chat_output(&format!("Failed to start background shell: {e}"));
                                }
                            }
                            continue;
                        }
                        SessionCommand::Logs { seq: _ } => {
                            if let Some(output) = handle_local_session_command(
                                &action,
                                &mut chat_sessions,
                                &session_rings,
                                &mut reaped_log_archive,
                                &reap_policy,
                                &tools_registry,
                            )
                            .await
                            {
                                emit_chat_output(&output);
                            }
                            continue;
                        }
                        SessionCommand::Pty { command } => {
                            // v3a: interactive PTY shell with a full terminal
                            // handoff. The chat ratatui render loop is suspended
                            // and the real terminal is wired straight to the PTY
                            // for the duration; Ctrl-] detaches, Ctrl-C/Ctrl-D
                            // pass through. Restoration is guaranteed by the RAII
                            // `PtyHandoffGuard` regardless of how the passthrough
                            // ends (detach, child exit, error, or panic).
                            #[cfg(feature = "terminal-tui")]
                            {
                                handle_pty_command(
                                    &command,
                                    &config.workspace_dir,
                                    &mut chat_sessions,
                                    &pty_handoff,
                                    sessions_redraw_handle.as_ref(),
                                    &emit_chat_output,
                                )
                                .await;
                            }
                            #[cfg(not(feature = "terminal-tui"))]
                            {
                                let _ = &command;
                                emit_chat_output("Interactive PTY sessions require the terminal UI.");
                            }
                            continue;
                        }
                        SessionCommand::ResumeDecision { decision_id } => {
                            if !config.causal_tree.enabled {
                                emit_chat_output("Decision resume is unavailable while the decision plane is disabled.");
                                continue;
                            }
                            // Only this explicit command selects resume mode. The
                            // fresh source event records the exact canonical request.
                            user_input = serde_json::json!({ "decision_id": decision_id.hyphenated().to_string() }).to_string();
                            resume_decision = true;
                        }
                        SessionCommand::InspectDecision { decision_id } => {
                            if !config.causal_tree.enabled {
                                emit_chat_output("Decision inspection is unavailable while the decision plane is disabled.");
                                continue;
                            }
                            let workspace = ctx.config_generation.effective.workspace_dir.to_string_lossy();
                            let envelope = chat_runtime_envelope(&workspace, &chat_session_key);
                            let id = crate::causal_tree::ids::DecisionId::parse(&decision_id.hyphenated().to_string())?;
                            let inspected = turn_decisions.inspect_pending_decision_approval(
                                &envelope.resolved_owner_id(), &envelope.session_key, id,
                            ).await.and_then(|view| serde_json::to_string_pretty(&view).map_err(Into::into));
                            match inspected {
                                Ok(view) => {
                                    match chat_sessions.register_decision_seq(&chat_session_key, id) {
                                        Ok(seq) => emit_chat_output(&format!("Pending decision #{seq}: {id}. Review with /approve {seq} or /deny {seq}.")),
                                        Err(_) => emit_chat_output("A display number is unavailable; use the explicit decision UUID."),
                                    }
                                    emit_chat_output("Recorded approval details (arguments may be redacted; output may be truncated):");
                                    emit_chat_output(&sanitize::truncate_large_output(&view));
                                }
                                Err(_) => emit_chat_output("No inspectable pending decision is available for this chat."),
                            }
                            continue;
                        }
                        SessionCommand::RevokeDecision { decision_id } => {
                            if !config.causal_tree.enabled {
                                emit_chat_output("Decision revocation is unavailable while the decision plane is disabled.");
                                continue;
                            }
                            let workspace = ctx.config_generation.effective.workspace_dir.to_string_lossy();
                            let envelope = chat_runtime_envelope(&workspace, &chat_session_key);
                            let id = crate::causal_tree::ids::DecisionId::parse(&decision_id.hyphenated().to_string())?;
                            match turn_decisions.revoke_pending_decision_approval(
                                &envelope.resolved_owner_id(), &envelope.session_key, id,
                            ).await {
                                Ok(_) => emit_chat_output("Decision grant revocation recorded. This does not undo an operation that already ran."),
                                Err(_) => emit_chat_output("Decision revocation could not be confirmed for this chat. No automatic retry was made."),
                            }
                            continue;
                        }
                        SessionCommand::ReviewDecision { decision_id, approved } => {
                            if !config.causal_tree.enabled {
                                emit_chat_output("Decision review is unavailable while the decision plane is disabled.");
                                continue;
                            }
                            let workspace = ctx.config_generation.effective.workspace_dir.to_string_lossy();
                            let envelope = chat_runtime_envelope(&workspace, &chat_session_key);
                            let id = crate::causal_tree::ids::DecisionId::parse(&decision_id.hyphenated().to_string())?;
                            match turn_decisions.review_pending_decision_approval(
                                &envelope.resolved_owner_id(), &envelope.session_key, id, approved,
                            ).await {
                                Ok(_) => emit_chat_output(if approved {
                                    "Decision approval recorded. The operation has not run; explicit resume is required."
                                } else {
                                    "Decision rejected. The operation has not run."
                                }),
                                Err(_) => emit_chat_output(
                                    "Decision review could not be confirmed for this chat. No automatic retry was made."
                                ),
                            }
                            continue;
                        }
                        SessionCommand::Approve { seq } | SessionCommand::Deny { seq } => {
                            // NeedsInput: deliver an approval decision to a
                            // background sub-agent suspended on the supervised
                            // approval gate. `/approve` injects a runtime grant
                            // (Grant) so the gated tool can pass the gate; `/deny`
                            // reports the tool as denied to the sub-agent.
                            let approve = matches!(action, SessionCommand::Approve { .. });
                            match chat_sessions.decision_for_seq(&chat_session_key, seq) {
                                Ok(Some(id)) => {
                                    if !config.causal_tree.enabled {
                                        emit_chat_output("Decision review is unavailable while the decision plane is disabled.");
                                        continue;
                                    }
                                    let workspace = ctx.config_generation.effective.workspace_dir.to_string_lossy();
                                    let envelope = chat_runtime_envelope(&workspace, &chat_session_key);
                                    match turn_decisions.review_pending_decision_approval(
                                        &envelope.resolved_owner_id(), &envelope.session_key, id, approve,
                                    ).await {
                                        Ok(_) => emit_chat_output(if approve {
                                            "Decision approval recorded. The operation has not run; explicit resume is required."
                                        } else { "Decision rejected. The operation has not run." }),
                                        Err(_) => emit_chat_output("Decision review could not be confirmed for this chat. No automatic retry was made."),
                                    }
                                    continue;
                                }
                                Err(_) => {
                                    emit_chat_output("No decision alias is available in this chat.");
                                    continue;
                                }
                                Ok(None) => {}
                            }
                            let run_id = match chat_sessions.resolve_run_id(seq).await {
                                Ok(id) => id,
                                Err(e) => {
                                    emit_chat_output(&format!(
                                        "{} failed: {e}",
                                        if approve { "Approve" } else { "Deny" }
                                    ));
                                    continue;
                                }
                            };
                            let decision = if approve {
                                crate::agent::turn_contracts::ApprovalDecision::Grant
                            } else {
                                crate::agent::turn_contracts::ApprovalDecision::Deny
                            };
                            if pending_approvals.resolve(&run_id, decision) {
                                emit_chat_output(&format!(
                                    "{} session #{seq}.",
                                    if approve { "Approved" } else { "Denied" }
                                ));
                            } else {
                                emit_chat_output(&format!(
                                    "Session #{seq} is not awaiting approval (it may have resumed, \
                                     timed out, completed, or was killed)."
                                ));
                            }
                            continue;
                        }
                        SessionCommand::DaemonSessions
                        | SessionCommand::DaemonKill { .. }
                        | SessionCommand::DaemonSteer { .. } => {
                            // a5: the daemon's work registry lives in another
                            // process, so this is a control-API call, not a
                            // registry read. The listing's local fallback is
                            // built here, on the loop where `chat_sessions` is
                            // reachable, and handed to the request to show if
                            // the daemon turns out to be unreachable.
                            let local_fallback = if matches!(action, SessionCommand::DaemonSessions) {
                                handle_local_session_command(
                                    &SessionCommand::Sessions,
                                    &mut chat_sessions,
                                    &session_rings,
                                    &mut reaped_log_archive,
                                    &reap_policy,
                                    &tools_registry,
                                )
                                .await
                            } else {
                                None
                            };
                            if let Some(notice) = start_daemon_session_command(
                                &action,
                                local_fallback,
                                &chat_sessions.daemon_requests(),
                                DaemonControl {
                                    config: &config,
                                    dispatcher: &chat_dispatcher,
                                    redraw_tx: redraw_tx_for_main.as_ref(),
                                },
                            ) {
                                emit_chat_output(&notice);
                            }
                            continue;
                        }
                    }
                }
                commands::CommandResult::NotACommand => {
                    // v1.1b input routing (head footgun: input-target ambiguity).
                    // When a child session is attached, plain text + Enter is
                    // routed as a *steer* to that session instead of starting a
                    // main-chat turn. The prompt's colour+glyph indicator already
                    // shows the target, and `/detach` (or Esc) returns to main.
                    // We never auto-switch focus — only an explicit /attach or the
                    // switcher changes the routing target.
                    if let Some(sid) = attached_follow.clone() {
                        if !is_local_operator_input(&msg) {
                            emit_chat_output("Steering an attached session requires local terminal operator input.");
                            if let Some(guard) = assignment_guard.as_mut() {
                                guard.failed("Assigned task cannot steer the operator's attached session.");
                            }
                            continue;
                        }
                        let run_id = sid.as_str().to_string();
                        match tools_registry.iter().find(|t| t.supports_name("sessions_spawn")) {
                            Some(tool) => {
                                // Same Low-risk steer path as `/steer`: delegate to
                                // the sessions_spawn tool with the matching grant so
                                // the shared side-effect gate + running-status check
                                // + steer_tx delivery all apply uniformly.
                                let operation_name = format!("sessions_spawn:steer:{run_id}");
                                let grant = crate::security::policy::ApprovalGrant::for_resource_operation(
                                    "sessions_spawn",
                                    &operation_name,
                                    "chat-operator",
                                    None,
                                );
                                let mut args = serde_json::json!({
                                    "action": "steer",
                                    "run_id": run_id,
                                    "message": user_input,
                                });
                                match serde_json::to_value(&grant) {
                                    Ok(grant_value) => {
                                        if let Some(obj) = args.as_object_mut() {
                                            obj.insert(
                                                crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.to_string(),
                                                grant_value,
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            "Failed to serialize steer approval grant; proceeding without it"
                                        );
                                    }
                                }
                                match tool.execute_named("sessions_spawn", args).await {
                                    Ok(result) => {
                                        let out = if result.output.is_empty() {
                                            result
                                                .error
                                                .filter(|e| !e.is_empty())
                                                .unwrap_or_else(|| "(steered)".to_string())
                                        } else {
                                            result.output
                                        };
                                        emit_chat_output(&out);
                                    }
                                    Err(e) => emit_chat_output(&format!("Steer failed: {e}")),
                                }
                            }
                            None => emit_chat_output("Background sessions are not available in this session."),
                        }
                        continue;
                    }
                }
            }
        }

        // This is the concrete TerminalChannel: send_draft always returns
        // Some(id), including when its UI receiver has closed. Capacity can
        // therefore be selected before creating the durable source. The local
        // worker registry is inserted into only by this main loop; awaits below
        // cannot consume another slot. Explicit approval resume stays foreground.
        #[cfg(feature = "terminal-tui")]
        let reducer_driver_turn_active = !resume_decision;
        #[cfg(feature = "terminal-tui")]
        let provider_worker_kind = if reducer_driver_turn_active {
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached
        } else {
            crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited
        };
        #[cfg(not(feature = "terminal-tui"))]
        let provider_worker_kind = crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited;
        let provider_admission_max = match provider_worker_kind {
            crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited => 1,
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached => max_concurrent_visible_turns,
        };
        let provider_admission =
            provider_turn_visible_admission(&provider_turn_workers, provider_worker_kind, provider_admission_max);
        if !provider_admission.can_start_visible {
            tracing::warn!(
                active_workers = provider_admission.active_workers,
                foreground_active = provider_admission.foreground_active,
                detached_active = provider_admission.detached_active,
                effective_max_visible_turns = provider_admission.effective_max_visible_turns,
                target_kind = ?provider_worker_kind,
                "visible provider turn capacity unavailable before source admission; queueing input"
            );
            // No durable input or decision exists yet. Keep the original
            // assignment and queue identity for its first actual admission.
            if let Some(guard) = assignment_guard.as_mut() {
                guard.disarm();
            }
            requeue_post_route_admission_rejected_input(
                &mut input_backlog,
                &mut defer_visible_input_pop_once,
                DequeuedInputMessage {
                    priority: input_priority,
                    turn_task_id: provider_queue_task_id,
                    msg,
                },
            );
            publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
            continue;
        }
        fabric_turn_seq += 1;
        // D8-2: one run_id per turn, generated at the turn entry and reused by
        // every run_id consumer within this loop iteration (user event, route
        // scope, assistant event). No parent_run_id is set (turns are not a spawn
        // lineage; session relation is via chat_session_key).
        let turn_run_id = uuid::Uuid::new_v4().to_string();
        let current_provider_binding = provider_binding.as_ref()
            .ok_or_else(|| anyhow::anyhow!("chat provider binding is missing"))?;
        current_provider_binding.verify(&ctx.config_generation, &provider, provider_name, model_name)?;
        let cancellation = turn_signal.new_turn_cancellation();
        let mut input_scope = chat_message_event_scope(
            memory_fabric.workspace_id(), &chat_session_key, &turn_run_id, provider_name, model_name,
        );
        input_scope.config_generation_id = Some(ctx.config_generation.id.0);
        input_scope.config_source_revision = Some(ctx.config_generation.source_revision.fingerprint_sha256.clone());
        // The TUI mirror is updated synchronously by /mode; its legacy session
        // deliberately stops tracking mode when the reducer owns UI state.
        #[cfg(feature = "terminal-tui")]
        let admitted_chat_mode = chat_mirror.lock().chat_mode;
        #[cfg(not(feature = "terminal-tui"))]
        let admitted_chat_mode = chat_session.mode;
        let has_conversation_history = history.iter().any(|message| message.role != "system");
        let (chat_user_event, admitted_binding, mut admission_guard) = current_provider_binding.record_guarded(
            memory_fabric.clone(), (input_scope, &chat_session_key, decision_source::ChatAdmissionMode::Model(admitted_chat_mode), has_conversation_history), &user_input,
            format!("chat:{}:{turn_run_id}:{fabric_turn_seq}:user", chat_session.id),
            child_cleanup.register(cancellation.clone())?,
        ).await?;
        let admission = if resume_decision {
            admitted_binding.decide_resume(&turn_decisions, &cancellation).await
        } else {
            admitted_binding.decide(&turn_decisions, &cancellation).await
        };
        if let Err(error) = admission {
            if let Err(record_error) = admission_guard.finish_failure(
                cancellation.is_cancelled(), &format!("decision admission failed: {error}"),
            ).await {
                return Err(error.context(format!("Chat admission failure persistence is unconfirmed: {record_error}")));
            }
            return Err(error);
        }
        admission_guard.handoff_terminal()?;
        let chat_turn_binding = Some(Arc::clone(&admitted_binding));
        if let Some(id) = chat_turn_binding.as_ref().and_then(|binding| binding.pending_decision_id()) {
            match chat_sessions.register_decision_seq(&chat_session_key, id) {
                Ok(seq) => emit_chat_output(&format!("Pending decision #{seq}: {id}. Inspect with /approval --decision {id}; review with /approve {seq} or /deny {seq}.")),
                Err(_) => emit_chat_output("A display number is unavailable; use the explicit decision UUID."),
            }
        }
        let fixed_reply = admitted_binding.fixed_reply()?;

        let collection_turn = admitted_binding.is_approval_collection()?;
        let direct_decision_turn = resume_decision || fixed_reply.is_some() || collection_turn;
        let file_mention_enrichment = if direct_decision_turn {
            FileMentionEnrichment { prompt: user_input.clone(), visible_note: None }
        } else {
            enrich_file_mentions_for_prompt(&user_input, tools_registry.as_ref()).await
        };
        if let Some(note) = file_mention_enrichment.visible_note.as_deref() {
            emit_chat_output(note);
        }
        let user_input_for_prompt = file_mention_enrichment.prompt;

        // Auto-save user message to memory
        if !direct_decision_turn && config.memory.should_auto_promote_user_message(&user_input) {
            let user_key = autosave_memory_key("user_msg");
            let safe_user_input = sanitize_chat_semantic_memory_content(&user_input);
            let _ = memory_fabric
                .record_semantic_memory_from_event(
                    &user_key,
                    &safe_user_input,
                    MemoryCategory::Conversation,
                    None,
                    chat_user_event.as_ref().map(|event| event.event_id.as_str()),
                    None,
                    None,
                )
                .await;
        }

        // Inject memory context
        let runtime_envelope = admitted_binding.envelope().clone();
        let memory_runtime = crate::agent::turn_request::ToolLoopMemory::new(
            &mem,
            &config.workspace_dir,
            Some(
                DocumentIngestRuntime::from_envelope(mem.clone(), &runtime_envelope)
                    .with_source_message_event_id(chat_user_event.as_ref().map(|event| event.event_id.clone())),
            ),
        )
        .with_event_fabric(memory_fabric.clone())
        .with_request_event_scope(
            runtime_envelope.message_scope().with_recipient(format!("{provider_name}/{model_name}")),
        );
        // The TUI handoff retains the original source input, before enrichment.
        #[cfg(feature = "terminal-tui")]
        let routing_input = admitted_binding.routing_input().to_owned();
        let mem_context = if direct_decision_turn {
            crate::agent::context_recall::RecalledMemoryContext { preamble: String::new(), ids: Vec::new() }
        } else {
            admitted_binding.recalled_context()?
        };
        let context = mem_context.preamble.clone();
        let enriched = if context.is_empty() {
            user_input_for_prompt.clone()
        } else {
            format!("{context}{user_input_for_prompt}")
        };

        // Build system prompt with skill selection
        let selected_skills = if direct_decision_turn {
            Vec::new()
        } else {
            admitted_binding.prompt_skills()?
        };
        // Publish only this turn's selected metadata. Previous turns do not
        // grant continued Skill exposure; the prompt builder orders entries.
        let system_prompt = build_runtime_system_prompt(&config, model_name, &selected_skills, native_tools);
        let persisted_history_for_turn = persisted_history_for_current_turn(&chat_session, &system_prompt, &user_input);
        upsert_leading_system_prompt(&mut history, system_prompt.clone());
        // S2-C Step 4: dual-write SetLeadingSystemPrompt to the reducer — byte-for-byte aligned with
        // the legacy `if empty { push } else { first_mut = ... }` (the reducer takes the same
        // branches internally). It runs on every turn, and expressing it as an append would pile up
        // system messages.
        let _ = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::SetLeadingSystemPrompt {
                content: system_prompt.clone(),
            },
            "chat.system_prompt_per_turn",
        );
        let history_len_before_user_turn = history.len();
        let history_user_message = crate::agent::turn_contracts::inbound_user_message(&enriched);
        // The provider-side copy is read only by the TUI reducer driver.
        #[cfg(feature = "terminal-tui")]
        let history_for_provider = {
            let mut history_for_provider = history.clone();
            history_for_provider.push(history_user_message.clone());
            history_for_provider
        };

        // ── Set active recipient/channel on tools (for proactive messaging) ──
        for tool in tools_registry.iter() {
            if tool.name() == "message_send" {
                continue;
            }
            tool.set_active_recipient("user").await;
            tool.set_active_channel(Arc::clone(&terminal) as Arc<dyn Channel>).await;
        }

        // ── Streaming pipeline setup ─────────────────────────────
        //
        // The delta channel carries ONLY visible assistant text — never the
        // model's reasoning/thinking content. Reasoning is separated upstream
        // at the provider parsing layer (see `parse_native_response` /
        // `parse_sse_line` in providers/{anthropic,openai,ollama,compatible}.rs)
        // and travels back via `ProviderChatResponse.reasoning_content`. The
        // tool-call loop persists reasoning into conversation history through
        // `build_native_assistant_history`; the live stream below renders text
        // only, so the user never sees the model's internal monologue.
        let (delta_tx, delta_rx) = mpsc::channel::<String>(DELTA_CHANNEL_CAPACITY);

        // Start a streaming draft on the terminal
        let draft_id = match terminal.send_draft(&SendMessage::new("", "user")).await {
            Ok(id) => id,
            Err(e) => {
                tracing::debug!("Failed to start draft: {e}");
                None
            }
        };

        #[cfg(feature = "terminal-tui")]
        anyhow::ensure!(
            !reducer_driver_turn_active || draft_id.is_some(),
            "admitted detached chat turn requires its terminal draft; execution was not requeued"
        );

        // Step 5b dual write: announce that a new LLM inference turn starts (only when a draft
        // exists). In shadow mode the reducer sets stream.draft + control.generating=true, with no
        // external side effects (business Effects are no-ops).
        if let Some(ref d_id) = draft_id {
            let _ = chat_dispatcher.dispatch_or_log(
                crate::chat::action::Action::TurnStarted {
                    draft_id: d_id.clone(),
                    cancel: cancellation.clone(),
                },
                "chat.turn_started",
            );
        }

        // Spawn background task: accumulate deltas → channel.update_draft()
        // Follows the exact same pattern as process_channel_message in channels/mod.rs.
        //
        // P1-6 — Monotonic draft version protocol (Step 3 update). The
        // sender-side counter still stamps each accumulated snapshot with a
        // strictly monotonic `u64` (kept for `update_draft` downstream
        // consumers and the inline-redraw protocol). The receiver-side stale
        // check formerly performed here by `DraftVersionTracker.accept()` has
        // been DELETED — its protection is now owned end-to-end by the
        // Redux-style reducer in `chat::state` via `StreamState::draft.version`
        // (see `ChatState::reduce_stream_chunk_received`).
        //
        // Why this is safe to remove:
        //   1. `delta_rx` is a single-task tokio mpsc; FIFO is guaranteed at
        //      the runtime layer (no parallel accumulators).
        //   2. The counter is incremented atomically inside the same task,
        //      producing a strictly monotonic sequence by construction. The
        //      old tracker call was over-defence ("unreachable here" per the
        //      original comment).
        //   3. The reducer's `StreamChunkReceived` arm now enforces
        //      strict-monotonic version + draft_id matching + finalize-state
        //      check as the single source of truth. Once Step 5 makes the
        //      reducer the renderer source, this task disappears entirely.
        //
        // Terminal TUI now relies on the reducer for the visible stream; the
        // non-TUI fallback still forwards full accumulated drafts to the
        // channel implementation.
        let draft_updater = if let Some(ref d_id) = draft_id {
            #[cfg(not(feature = "terminal-tui"))]
            let channel: Arc<TerminalChannel> = Arc::clone(&terminal);
            #[cfg(not(feature = "terminal-tui"))]
            let reply_target = "user".to_string();
            #[cfg(not(feature = "terminal-tui"))]
            let draft_id_owned = d_id.clone();
            let mut rx = delta_rx;
            let version_counter = Arc::new(DraftVersionCounter::new());
            // Step 5b dual write: send every delta through the coalescer as
            // Action::StreamChunkReceived. When the bounded(2048) action_tx is full the coalescer
            // merges deltas, so nothing grows without bound.
            let coalescer_sender = chat_dispatcher.sender();
            let coalescer_draft_id = d_id.clone();
            Some(tokio::spawn(async move {
                #[cfg(not(feature = "terminal-tui"))]
                let mut accumulated = String::new();
                let mut coalescer = dispatcher::StreamChunkCoalescer::new(coalescer_sender);
                while let Some(delta) = rx.recv().await {
                    // Counter still ticks for downstream consumers (UiActor's
                    // inline-redraw protocol uses it). No tracker.accept() —
                    // see comment block above.
                    let version = version_counter.next();
                    #[cfg(not(feature = "terminal-tui"))]
                    {
                        accumulated.push_str(&delta);
                        if let Err(e) = channel.update_draft(&reply_target, &draft_id_owned, &accumulated).await {
                            tracing::debug!("Draft update failed: {e}");
                        }
                    }
                    // Step 5b shadow: forward the **incremental** delta into
                    // the reducer via the coalescer. The reducer accumulates
                    // its own `draft.accumulated` mirror — feeding the full
                    // `accumulated` string here would cause double-accumulation.
                    // Backpressure or close are silently tolerated (legacy
                    // path remains the renderer source; shadow path observes).
                    let _ = coalescer.try_send_chunk(coalescer_draft_id.clone(), delta, version);
                }
                // Stream ended — flush pending coalescer state, counter goes
                // out of scope; reducer-side version state is cleared on
                // StreamCompleted/Failed/Cancelled (dispatched from the chat::run main loop).
                let _ = coalescer.flush();
            }))
        } else {
            // No draft — consume delta_rx so the sender doesn't block
            let mut rx = delta_rx;
            Some(tokio::spawn(async move { while rx.recv().await.is_some() {} }))
        };

        #[cfg(not(feature = "terminal-tui"))]
        {
            *active_cancel.lock() = Some(cancellation.clone());
        }

        // ── Tool event forwarding (visual feedback in terminal) ──
        //
        // P2-7: in addition to the existing notify_tool_* calls (which feed
        // the legacy UiActor renderer in `channels/terminal.rs`), we also
        // mirror every tool event into a `TuiState` instance behind a
        // `parking_lot::Mutex`. The ratatui renderer in `chat/tui.rs` reads
        // from this mirror; full renderer wiring lands in P2-12.
        let (tool_event_tx, mut tool_event_rx) = mpsc::channel::<ToolCallNotification>(TOOL_EVENT_CHANNEL_CAPACITY);
        let terminal_for_tools = Arc::clone(&terminal);
        // P3-3: every producer in this turn now shares the chat-scoped
        // `chat_mirror` (created once at the top of `run`). The previous
        // per-turn `tui_mirror` instance is gone — keeping a per-turn alias
        // here so downstream code that already says `tui_mirror.lock()` keeps
        // compiling, but the underlying `Arc` is the same one the render
        // task and the input task hold.
        #[cfg(feature = "terminal-tui")]
        let tui_mirror: Arc<parking_lot::Mutex<tui::TuiState>> = Arc::clone(&chat_mirror);
        // P0-5 fix: tool start/finish events now have a single mirror path.
        // The UiActor in `channels/terminal.rs::handle_event_tui` is the sole
        // writer of tool cards into `TuiState` (it sanitises name/args via
        // `sanitize_terminal_output` and calls `notify_redraw()` on the same
        // 1-slot channel as `redraw_tx_for_main`). The previous double-mirror
        // here pushed a second card and could reorder Running/Done with the
        // UiActor path under load — the forwarder now just relays the event
        // to the UiActor and lets that path own the mirror mutation.
        let tool_event_forwarder = tokio::spawn(async move {
            while let Some(notif) = tool_event_rx.recv().await {
                match notif {
                    ToolCallNotification::Started { name, args_summary } => {
                        terminal_for_tools.notify_tool_started(&name, &args_summary).await;
                    }
                    ToolCallNotification::Finished {
                        name,
                        success,
                        duration_ms,
                    } => {
                        terminal_for_tools
                            .notify_tool_finished(&name, success, duration_ms)
                            .await;
                    }
                    ToolCallNotification::Progress { iteration } => {
                        terminal_for_tools.notify_progress(iteration).await;
                    }
                }
            }
        });
        // Log a trace stat so the mirror is observably wired (also keeps the
        // `tui_mirror` binding from being flagged as unused when the renderer
        // wiring lands in P2-12).
        #[cfg(feature = "terminal-tui")]
        tracing::trace!(
            tracked_tool_cards = tui_mirror.lock().last_tool_result_index().map(|i| i + 1).unwrap_or(0),
            "tui_mirror initialized"
        );

        // ── Unified tool authorization (permission-model Phase 1) ──
        // Tool access is governed solely by `[autonomy]` via
        // `SecurityPolicy::decide`; the former PolicyPipeline is removed.
        let scope_owner_id = runtime_envelope.resolved_owner_id();
        let scope_ctx = ScopeContext {
            policy: &security,
            sender: "user",
            channel: "terminal",
            chat_type: "private",
            chat_id: "terminal:user",
            session_key: Some(&runtime_envelope.session_key),
            owner_id: Some(&scope_owner_id),
            owner_isolated: runtime_envelope.owner_isolated,
            topic_id: runtime_envelope.topic_id.as_deref(),
            task_id: runtime_envelope.resolved_task_id(),
            source_message_event_id: runtime_envelope.source_message_event_id.as_deref(),
            config_generation_id: runtime_envelope.config_generation_id,
            config_source_revision: runtime_envelope.config_source_revision.as_deref(),
        };

        let mut effective_compaction = crate::router::resolve_effective_compaction_config(
            &config.agent.compaction,
            provider_name,
            model_name,
            &config.router,
            &config.model_routes,
        );
        effective_compaction.config = admitted_binding.main_compaction()
            .ok_or_else(|| anyhow::anyhow!("chat compaction requires an admitted decision"))?.clone();
        crate::router::context::trace_effective_compaction_resolution(&effective_compaction);
        #[cfg(feature = "terminal-tui")]
        refresh_context_budget_for_tui(
            &history_for_provider,
            &effective_compaction.config,
            redraw_tx_for_main.is_some(),
            &chat_mirror,
            &chat_dispatcher,
        );

        let route_decision = RouteDecision::from_model_routes_for_context(
            provider_name,
            model_name,
            &config.model_routes,
            runtime_envelope.resolved_owner_id(),
            chat_session_key.clone(),
            chat_user_event.as_ref().map(|event| event.event_id.clone()),
            "chat",
            (user_input.chars().count() / 4 + 1).min(u32::MAX as usize) as u32,
            true,
            false,
        );
        let route_scope = route_event_scope(
            "chat",
            Some(runtime_envelope.resolved_owner_id()),
            Some(chat_session_key.clone()),
            Some(turn_run_id.clone()),
            Some("local-user".to_string()),
            Some(format!(
                "{}/{}",
                route_decision.selected.provider, route_decision.selected.model
            )),
        );
        if let Err(e) = record_route_decision_event(&memory_fabric, route_scope.clone(), &route_decision).await {
            tracing::warn!(error = %e, "Failed to append router.route_decision message event");
        }
        let provider_started_at = chrono::Utc::now();

        // The shared turn engine owns context-overflow recovery. Chat projects
        // its final outcome and does not start a second compaction/replay loop.
        //
        // There is deliberately no wall-clock budget around the turn. A chat turn
        // ends when it finishes, when the user cancels it, or when the stall
        // detector in `crate::agent::idle` finds it has stopped making progress.
        #[cfg(feature = "terminal-tui")]
        let history_len_before_tools;

        // S2-A refinement: split the coarse `Failed` variant so the Redux
        // dispatch path can distinguish user-driven cancellation from real
        // errors. The legacy renderer still treats every non-Success as a
        // failure (continue), but the reducer now sees the correct semantic
        // (`StreamCancelled` vs `StreamFailed { err, retryable }`).
        enum TurnOutcome {
            // FIX-P0-30/31: carry the loop's provider-attribution trace so the
            // success path can record the *real* serving model/attempts instead
            // of the routed `decision.selected.model`.
            Success(String),
            /// User-initiated cancel (Ctrl+C) or `is_tool_loop_cancelled` from
            /// the inner loop. Reducer side maps to `StreamCancelled`.
            Cancelled,
            /// Genuine failure (timeout / context-overflow exhausted / other
            /// error). Carries error string + retryable hint for the reducer's
            /// `StreamFailed` payload (mirrors `dispatcher::TurnOutcomeKind::Failed`).
            FailedWithError {
                err: String,
                retryable: bool,
            },
        }

        // The terminal TUI always routes a turn to the reducer driver; without
        // the TUI the shared agent loop runs it (the driver branch is cfg'd out).
        #[cfg(feature = "terminal-tui")]
        tracing::info!(
            redux_mode = ?top_redux_mode,
            // Registry roots before routing, refresh and alias expansion;
            // the wire count is the compiled-request event's
            // `tool_snapshot.count`.
            registry_root_tools = tools_registry.len(),
            route = "ReduxDriver",
            "chat::run turn route decision"
        );
        let provider_history_len_before_assistant;
        #[cfg(feature = "terminal-tui")]
        {
            if reducer_driver_turn_active {
                provider_history_len_before_assistant = history_for_provider.len();
            } else {
                history.push(history_user_message.clone());
                provider_history_len_before_assistant = history.len();
                let _ = chat_dispatcher.dispatch_or_log(
                    crate::chat::action::Action::RecordUserTurn(user_input.clone()),
                    "chat.record_user_turn",
                );
            }
        }
        #[cfg(not(feature = "terminal-tui"))]
        {
            history.push(history_user_message.clone());
            provider_history_len_before_assistant = history.len();
            let _ = chat_dispatcher.dispatch_or_log(
                crate::chat::action::Action::RecordUserTurn(user_input.clone()),
                "chat.record_user_turn",
            );
        }

        let provider_turn_task_id = start_provider_turn_task(
            &mut turn_scheduler,
            provider_queue_task_id,
            &user_input,
            history_len_before_user_turn,
        );
        anyhow::ensure!(
            !chat_turn_binding.as_ref().is_some_and(|binding| binding.is_decided()) || provider_turn_task_id.is_some(),
            "decision-owned Chat turn could not acquire a scheduler task"
        );
        register_provider_history_commit_task(&mut history_commit_coordinator, &turn_scheduler, provider_turn_task_id);
        record_provider_turn_completion_context(
            &mut provider_turn_completion_contexts,
            provider_turn_task_id,
            provider_history_len_before_assistant,
        );
        register_provider_turn_worker(
            &mut provider_turn_workers,
            &turn_scheduler,
            provider_turn_task_id,
            provider_worker_kind,
        );
        publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
        publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);

        // Foreground fixed closes have no older visible worker to wait for.
        // Keep the same task-scoped persistence acknowledgement as detached
        // replies, including builds without the terminal TUI.
        if provider_worker_kind == crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited
            && let Some(binding) = chat_turn_binding.as_ref()
            && (resume_decision || binding.fixed_reply()?.is_some())
        {
            anyhow::ensure!(history_commit_coordinator.pending_tasks() == 1,
                "foreground Chat close has an earlier uncommitted turn");
            let task_id = provider_turn_task_id.context("Chat close has no scheduler task")?;
            drop(delta_tx);
            drop(tool_event_tx);
            if let Some(handle) = draft_updater { let _ = handle.await; }
            let _ = tool_event_forwarder.await;
            let close_draft_id = draft_id.clone().unwrap_or_else(|| format!("decision-close:{turn_run_id}"));
            if draft_id.is_none() {
                anyhow::ensure!(matches!(chat_dispatcher.dispatch_or_log(
                    crate::chat::action::Action::TurnStarted {
                        draft_id: close_draft_id.clone(), cancel: cancellation.clone(),
                    }, "chat.fixed_close_started"), dispatcher::DispatchResult::Sent),
                    "Chat close draft could not be registered");
            }
            let commit_len = history.len().saturating_add(1);
            let (reply, succeeded) = if let Some(reply) = binding.fixed_reply()? {
                binding.close(&memory_fabric, commit_len, provider_started_at, &cancellation).await?;
                (reply, true)
            } else {
                binding.execute_resume_and_finalize(&memory_fabric, commit_len, provider_started_at, &cancellation).await?
            };
            dispatch_ordered_provider_turn_commit_and_wait(
                &chat_dispatcher, &turn_signal, task_id, &close_draft_id,
                &user_input, &reply, "", false, false,
            ).await.map_err(anyhow::Error::msg)?;
            history.push(ChatMessage::assistant(reply.to_owned()));
            chat_session.add_user_turn(&user_input);
            chat_session.add_assistant_turn(&reply, Vec::new());
            gate_completed_provider_turn_finalization(
                &mut turn_scheduler, &mut history_commit_coordinator, &mut provider_turn_workers,
                Some(task_id), commit_len, reply.chars().count(), reply.chars().count(),
                &crate::llm::route_decision::TokenUsage::default(), "chat decision closed",
            );
            take_provider_turn_completion_history_len(&mut provider_turn_completion_contexts,
                Some(task_id), commit_len);
            if let Some(id) = draft_id.as_ref() {
                terminal.finalize_draft("user", id, &reply).await?;
            } else {
                terminal.send(&SendMessage::new(&reply, "user")).await?;
            }
            if let Some(guard) = assignment_guard.as_mut() {
                if succeeded { guard.completed(&reply); } else { guard.failed(&reply); }
            }
            #[cfg(not(feature = "terminal-tui"))]
            { *active_cancel.lock() = None; }
            publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
            publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
            continue;
        }

        // ── Redux driver switched-over path (Step 5a-4) ────────────
        //
        // Entered only when routing selects ReduxDriver. dispatch Action::StartLLMTurn →
        // EffectExecutor::execute_real(Effect::StartTurn) → spawn drive_start_turn_stream
        // streaming driver → post StreamChunkReceived / Completed / Failed / Cancelled back through
        // action_tx → the dispatcher task reduces them and calls turn_signal.record_and_notify →
        // this code awaits the outcome.
        //
        // This branch does **not** call run_turn; the old path does not run at all:
        //   * no duplicate hooks (the old path's hooks.emit does not run; only the reducer's
        //     NotifyHook(Error) writes)
        //   * no duplicate history writes (the reducer writes once through RecordAssistantTurn)
        //   * round 2 hang defence: shutdown.cancelled() as a fallback arm of tokio::select!
        //
        #[cfg(feature = "terminal-tui")]
        if reducer_driver_turn_active && let Some(d_id) = draft_id.clone() {
            // Protocol: register the keyed turn before dispatch, then obtain the
            // waiter. The legacy single-slot signal remains as fallback while
            // P6J migrates toward detached provider workers.
            if let Some(id) = provider_turn_task_id {
                turn_signal.register_turn(id, d_id.clone());
                let _ = turn_signal.consume_turn_outcome(id);
                let _ = turn_signal.consume_turn_usage(id);
            }
            if let Some(id) = provider_turn_task_id {
                spawn_provider_turn_completion_waiter(
                    turn_signal.clone(),
                    id,
                    provider_completion_tx.clone(),
                    shutdown.clone(),
                );
            }
            let mut notify_fut: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>> =
                provider_turn_task_id
                    .is_none()
                    .then(|| Box::pin(turn_signal.notified()) as _);
            // Consume any leftover outcome before dispatching, so what we read belongs to this turn.
            let _ = turn_signal.consume_outcome();

            // S2.5 P1-A: handle dispatch_result in explicit branches (a failed StartLLMTurn must
            // fall through, otherwise notify_fut hangs forever); dispatch_or_log already meters and
            // warns, so no extra tracing is needed.
            // D8-4 (redux path real fix): seed the turn-root spawn execution
            // context for this turn and hand it to the driver via StartLLMTurn →
            // Effect::StartTurn. This is the redux mirror of the legacy
            // `SPAWN_EXECUTION_CONTEXT.scope(seed_turn_context(turn_run_id, ..))`
            // wrapper applied below at the legacy tool-loop path's `run_turn` call
            // — the redux path `continue`s before reaching it, so the seed must
            // travel with the effect. Same `turn_run_id` + `chat_session_key`
            // source as the legacy path (single source of truth) so a sub-agent
            // spawned inside this turn inherits `parent_run_id = turn_run_id`.
            let mut redux_turn_spawn_ctx = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
                turn_run_id.clone(),
                chat_session_key.clone(),
            );
            redux_turn_spawn_ctx.owner_id = Some(runtime_envelope.resolved_owner_id());
            redux_turn_spawn_ctx.topic_id = runtime_envelope.topic_id.clone();
            redux_turn_spawn_ctx.source_message_event_id = runtime_envelope.source_message_event_id.clone();
            let redux_turn_message_send_ctx = crate::tools::message_send::MessageSendExecutionContext::new(
                Some("user".to_string()),
                Arc::clone(&terminal) as Arc<dyn Channel>,
            );
            let provider_turn_sequence =
                provider_turn_task_id.and_then(|id| turn_scheduler.task(id).map(|task| task.sequence));
            let dispatch_result = chat_dispatcher.dispatch_or_log(
                crate::chat::action::Action::StartLLMTurn {
                    provider_turn_task_id,
                    provider_turn_sequence,
                    draft_id: d_id.clone(),
                    history: history_for_provider.clone(),
                    compaction_guard_history: Some(persisted_history_for_turn.clone()),
                    compaction_config: Some(effective_compaction.config.clone()),
                    cancel: cancellation.clone(),
                    turn_spawn_ctx: Some(redux_turn_spawn_ctx),
                    turn_message_send_ctx: Some(redux_turn_message_send_ctx),
                    // Route on what the user typed. `history_for_provider`
                    // already carries the memory preamble and the
                    // `[Recent shared workspace events]` block, and routing on
                    // that let injected text widen the published tool surface.
                    routing_input: Some(routing_input.clone()),
                    turn_binding: chat_turn_binding.clone(),
                },
                "chat.start_llm_turn",
            );
            // Codex P1: the dispatch may be Backpressured / ChannelClosed. Either failure means the
            // dispatcher task will not produce a turn outcome, so chat::run must immediately treat it
            // as Failed and fall through to cleanup, otherwise notify_fut is never fired.
            if !matches!(dispatch_result, dispatcher::DispatchResult::Sent) {
                tracing::warn!(
                    result = ?dispatch_result,
                    "Redux driver: StartLLMTurn dispatch failed; aborting turn"
                );
                // S2-B Step 3: send StreamCancelled synchronously so the reducer clears
                // active_cancel; the old field is only a fallback under Off/Both (symmetric with the
                // guard at the register site).
                if let Some(ref d_id) = draft_id {
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::StreamCancelled { draft_id: d_id.clone() },
                        "chat.stream_cancelled_dispatch_failed",
                    );
                }
                #[cfg(not(feature = "terminal-tui"))]
                {
                    *active_cancel.lock() = None;
                }
                drop(delta_tx);
                drop(tool_event_tx);
                if let Some(handle) = draft_updater {
                    let _ = handle.await;
                }
                let _ = tool_event_forwarder.await;
                if let Some(ref id) = draft_id {
                    let _ = terminal.cancel_draft("user", id).await;
                }
                enqueue_provider_turn_finalizer_event(
                    &mut provider_turn_finalizer_events,
                    provider_turn_task_id,
                    ProviderTurnTerminalPlan::Failed {
                        err: "redux driver dispatch failed".to_string(),
                        history_commit_len: take_provider_turn_completion_history_len(
                            &mut provider_turn_completion_contexts,
                            provider_turn_task_id,
                            history.len(),
                        ),
                        summary: "redux driver dispatch failed".to_string(),
                    },
                );
                eprintln!("\nError: redux driver dispatch failed\n");
                continue;
            }

            if let Some(task_id) = provider_turn_task_id {
                if per_turn_contexts
                    .insert(
                        task_id,
                        PerTurnContext {
                            cancellation: cancellation.clone(),
                            turn_binding: chat_turn_binding.clone(),
                            fixed_reply: fixed_reply.clone(),
                            task_id,
                            draft_id: d_id.clone(),
                            delta_tx: Some(delta_tx),
                            tool_event_tx: Some(tool_event_tx),
                            draft_updater,
                            tool_event_forwarder: Some(tool_event_forwarder),
                            user_input: user_input.clone(),
                            turn_run_id: turn_run_id.clone(),
                            route_scope: route_scope.clone(),
                            route_decision: route_decision.clone(),
                            provider_started_at,
                            provider_name: provider_name.to_string(),
                            model_name: model_name.to_string(),
                            history_len_before_user_turn,
                            history_user_message: history_user_message.clone(),
                            assignment: assignment_guard.take(),
                        },
                    )
                    .is_some()
                {
                    tracing::warn!(
                        task_id = task_id.get(),
                        "replaced pending Redux turn context before completion"
                    );
                }
                continue;
            }

            // Shutdown preemption guard against a round 2 hang.
            let mut turn_input_open = true;
            let mut provider_completion_event: Option<ProviderTurnCompletionEvent> =
                provider_turn_task_id.and_then(|id| pending_provider_completion_events.remove(&id));
            if provider_completion_event.is_none() {
                loop {
                    tokio::select! {
                        () = async {
                            if let Some(fut) = notify_fut.as_mut() {
                                fut.await;
                            } else {
                                std::future::pending::<()>().await;
                            }
                        } => break,
                        completion = provider_completion_rx.recv(), if provider_turn_task_id.is_some() => {
                            let Some(completion) = completion else {
                                break;
                            };
                            // the completion of the current turn; `None`: retained as pending
                            if let Some(completion) = route_provider_completion_event_and_publish(
                                &mut provider_turn_lifecycle_rx,
                                &mut provider_turn_lifecycle_events_open,
                                &mut provider_turn_workers,
                                &mut pending_provider_completion_events,
                                provider_turn_task_id,
                                completion,
                                &chat_dispatcher,
                            ) {
                                provider_completion_event = Some(completion);
                                break;
                            }
                        }
                        lifecycle = provider_turn_lifecycle_rx.recv(), if provider_turn_lifecycle_events_open => {
                            let Some(lifecycle) = lifecycle else {
                                provider_turn_lifecycle_events_open = false;
                                continue;
                            };
                            record_provider_turn_lifecycle_event(
                                &mut provider_turn_workers,
                                lifecycle,
                            );
                            publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                        }
                        () = shutdown.cancelled() => {
                            tracing::debug!("Redux driver: shutdown.cancelled before turn complete");
                            break;
                        }
                        msg = input_rx.recv(), if turn_input_open => {
                            let Some(msg) = msg else {
                                turn_input_open = false;
                                continue;
                            };
                            let daemon_control = DaemonControl {
                                config: &config,
                                dispatcher: &chat_dispatcher,
                                redraw_tx: redraw_tx_for_main.as_ref(),
                            };
                            process_active_turn_input_batch(
                                msg,
                                &mut emit_chat_output,
                                &mut input_rx,
                                &mut input_backlog,
                                &mut turn_scheduler,
                                &mut provider_turn_workers,
                                provider_turn_task_id,
                                &chat_dispatcher,
                                &chat_session,
                                &mut chat_sessions,
                                &session_rings,
                                &mut reaped_log_archive,
                                &reap_policy,
                                &tools_registry,
                                daemon_control,
                            )
                            .await;
                            publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                        }
                    }
                }
            }

            let resolved_completion =
                resolve_provider_turn_completion(&turn_signal, provider_turn_task_id, provider_completion_event);
            let completion_history_len = take_provider_turn_completion_history_len(
                &mut provider_turn_completion_contexts,
                provider_turn_task_id,
                history.len(),
            );
            let terminal_plan = provider_turn_terminal_plan_from_completion(
                resolved_completion,
                completion_history_len,
                &tools_registry,
            );
            enqueue_provider_turn_finalizer_event(
                &mut provider_turn_finalizer_events,
                provider_turn_task_id,
                terminal_plan.clone(),
            );
            let finalizer_result = drain_provider_turn_finalizer_events_and_publish(
                &mut turn_scheduler,
                &mut history_commit_coordinator,
                &mut provider_turn_workers,
                &mut provider_turn_finalizer_events,
                &chat_dispatcher,
            )
            .into_iter()
            .rev()
            .find(|result| result.task_id == provider_turn_task_id)
            .unwrap_or(ProviderTurnFinalizerResult {
                task_id: provider_turn_task_id,
                terminal_status: "unknown",
                finalized: false,
            });

            // Finalize streaming (aligned with the legacy teardown): drop the senders so the
            // background tasks wind down.
            //
            // S2-B Step 3: on the driver path the reducer already cleared
            // `state.control.active_cancel` when it received StreamCompleted/Failed/Cancelled; the
            // legacy Arc is only a fallback under Off/Both.
            #[cfg(not(feature = "terminal-tui"))]
            {
                *active_cancel.lock() = None;
            }
            drop(delta_tx);
            drop(tool_event_tx);
            if let Some(handle) = draft_updater {
                let _ = handle.await;
            }
            let _ = tool_event_forwarder.await;

            match terminal_plan {
                ProviderTurnTerminalPlan::Completed {
                    final_text,
                    recorded_response,
                    empty_response,
                    usage: redux_tokens_used,
                    history_commit_len,
                    ..
                } => {
                    if empty_response {
                        let provider_outcome = if let Some(binding) = chat_turn_binding.as_ref() {
                            binding.provider_outcome(&route_decision, provider_started_at)?
                        } else { ProviderExecutionOutcome::success_for_decision_with_usage(
                            &route_decision, provider_started_at, redux_tokens_used.clone(),
                        ) };
                        if !finalizer_result.finalized {
                            if let Err(e) = terminal.cancel_draft("user", &d_id).await {
                                tracing::debug!(error = %e, "Redux driver: cancel empty-response draft failed");
                            }
                            publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                            publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                            continue;
                        }
                        if let Err(e) = terminal.cancel_draft("user", &d_id).await {
                            tracing::debug!(error = %e, "Redux driver: cancel empty-response draft failed");
                        }
                        let terminal_usage = match finalize_chat_success_turn(
                            chat_turn_binding.as_deref(),
                            &memory_fabric,
                            &chat_session_key,
                            &turn_run_id,
                            provider_name,
                            model_name,
                            route_scope.clone(),
                            provider_outcome.clone(),
                            None,
                            history_commit_len,
                            "redux driver completed with empty response",
                            &config.cost,
                            &config.workspace_dir,
                        )
                        .await
                        {
                            Ok(receipt) => receipt.usage_settlement,
                            Err(error) => {
                                return Err(error);
                            }
                        };
                        chat_session.add_user_turn(&user_input);
                        if let Some(record) =
                            terminal_usage.and_then(|record| chat_session.record_usage_settlement(record))
                        {
                            record_provider_turn_usage(&mut turn_scheduler, provider_turn_task_id, &record);
                            #[cfg(feature = "terminal-tui")]
                            {
                                chat_mirror.lock().token_usage_summary = chat_session.token_usage_summary();
                            }
                            let _ = chat_dispatcher.dispatch_or_log(
                                crate::chat::action::Action::ProviderUsageRecorded {
                                    task_id: provider_turn_task_id,
                                    usage_kind: crate::chat::action::ProviderUsageRecordKind::FinalAggregate,
                                    record,
                                },
                                "chat.provider_usage_recorded_empty_response",
                            );
                            #[cfg(feature = "terminal-tui")]
                            if let Some(tx) = redraw_tx_for_main.as_ref() {
                                let _ = tx.try_send(());
                            }
                        }
                        surface_turn_elapsed_message(
                            &chat_dispatcher,
                            sessions_redraw_handle.as_ref(),
                            "completed",
                            provider_outcome.started_at,
                            provider_outcome.finished_at,
                        );
                        publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                        publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                        continue;
                    }
                    let provider_outcome = if let Some(binding) = chat_turn_binding.as_ref() {
                        binding.provider_outcome(&route_decision, provider_started_at)?
                    } else { ProviderExecutionOutcome::success_for_decision_with_usage(
                        &route_decision, provider_started_at, redux_tokens_used.clone(),
                    ) };
                    if !finalizer_result.finalized {
                        if let Err(e) = terminal.cancel_draft("user", &d_id).await {
                            tracing::debug!(error = %e, "Redux driver: cancel commit-gated draft failed");
                        }
                        publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                        publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                        continue;
                    }
                    // 1) Write the final text accumulated by the driver's stream back into the LLM
                    //    history (aligned with the legacy tail
                    //    `history.push(ChatMessage::assistant(...))`).
                    history.push(ChatMessage::assistant(final_text.clone()));
                    // 2) finalize_draft: deliver the text to the terminal channel so the user sees
                    //    it (the driver path does not go through the delta_tx → draft_updater chain,
                    //    it finalizes directly).
                    if let Err(e) = terminal.finalize_draft("user", &d_id, &final_text).await {
                        tracing::warn!(error = %e, "Redux driver: finalize_draft failed");
                    }
                    let terminal_usage = match finalize_chat_success_turn(
                        chat_turn_binding.as_deref(),
                        &memory_fabric,
                        &chat_session_key,
                        &turn_run_id,
                        provider_name,
                        model_name,
                        route_scope.clone(),
                        provider_outcome.clone(),
                        Some(recorded_response.clone()),
                        history_commit_len,
                        "redux driver completed",
                        &config.cost,
                        &config.workspace_dir,
                    )
                    .await
                    {
                        Ok(receipt) => receipt.usage_settlement,
                        Err(error) => {
                            return Err(error);
                        }
                    };
                    let attempts_count = u8::try_from(provider_outcome.attempts.len()).unwrap_or(u8::MAX);
                    crate::runtime::control_ladder::append_provider_outcome_trace(
                        std::path::Path::new(&config.workspace_dir),
                        &provider_outcome.decision_id,
                        &provider_outcome.final_provider,
                        &provider_outcome.final_model,
                        attempts_count,
                        "success",
                    );
                    // On the driver path RecordAssistantTurn is already sent by dispatcher.rs
                    // (fixB B5).
                    // BUG-06 / BUG-08 round-2 fix: the real TUI drives turns through
                    // this ReduxDriver branch, which `continue`s at the end of the
                    // block and therefore NEVER reaches the legacy tool-loop
                    // `chat_session.add_*_turn` at the bottom of the loop body. The
                    // round-1 fix populated only that legacy path, so interactive
                    // `/export` / `/cost` (which read `ctx.chat_session.turns`) still
                    // saw an empty session. Mirror the live turn into the in-memory
                    // `chat_session` here as well. The reducer remains the single *persistence*
                    // source (it dispatched RecordAssistantTurn + Effect::SaveSession),
                    // so this only backs the slash commands and never double-writes.
                    chat_session.add_user_turn(&user_input);
                    chat_session.add_assistant_turn(&recorded_response, Vec::new());
                    if let Some(record) = terminal_usage.and_then(|record| chat_session.record_usage_settlement(record))
                    {
                        record_provider_turn_usage(&mut turn_scheduler, provider_turn_task_id, &record);
                        #[cfg(feature = "terminal-tui")]
                        {
                            chat_mirror.lock().token_usage_summary = chat_session.token_usage_summary();
                        }
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::ProviderUsageRecorded {
                                task_id: provider_turn_task_id,
                                usage_kind: crate::chat::action::ProviderUsageRecordKind::FinalAggregate,
                                record,
                            },
                            "chat.provider_usage_recorded",
                        );
                        #[cfg(feature = "terminal-tui")]
                        if let Some(tx) = redraw_tx_for_main.as_ref() {
                            let _ = tx.try_send(());
                        }
                    }
                    surface_turn_elapsed_message(
                        &chat_dispatcher,
                        sessions_redraw_handle.as_ref(),
                        "completed",
                        provider_outcome.started_at,
                        provider_outcome.finished_at,
                    );
                    publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                    publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                    let _ = final_text;
                }
                ProviderTurnTerminalPlan::Failed { err, .. } => {
                    if let Err(error) = finalize_chat_non_success_turn(
                        chat_turn_binding.as_deref(),
                        &memory_fabric,
                        &turn_run_id,
                        route_scope.clone(),
                        crate::agent::terminal::TurnTerminalStatus::Failed,
                        None,
                        provider_started_at,
                        &format!("redux driver failed: {err}"),
                        &config.cost,
                        &config.workspace_dir,
                    )
                    .await
                    {
                        if chat_turn_binding.as_ref().is_some_and(|binding| binding.is_decided()) { return Err(error); }
                        tracing::warn!(error = %error, "Failed to commit shared failed Chat terminal event");
                    }
                    // The reducer's NotifyHook(Error) has already been sent; do not hooks.emit here
                    // again, to avoid duplicates.
                    #[cfg(feature = "terminal-tui")]
                    let interactive_tui_active = redraw_tx_for_main.is_some();
                    #[cfg(not(feature = "terminal-tui"))]
                    let interactive_tui_active = false;

                    if !interactive_tui_active && let Some(ref id) = draft_id {
                        let _ = terminal.cancel_draft("user", id).await;
                    }
                    if !interactive_tui_active {
                        eprintln!("\nError: {err}\n");
                    }
                    if plain_mode {
                        plain_mode_turn_failed = true;
                    }
                    surface_turn_elapsed_message(
                        &chat_dispatcher,
                        sessions_redraw_handle.as_ref(),
                        "failed",
                        provider_started_at,
                        chrono::Utc::now(),
                    );
                    publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                    publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                }
                ProviderTurnTerminalPlan::Cancelled { .. } => {
                    if let Err(error) = finalize_chat_non_success_turn(
                        chat_turn_binding.as_deref(),
                        &memory_fabric,
                        &turn_run_id,
                        route_scope.clone(),
                        crate::agent::terminal::TurnTerminalStatus::Cancelled,
                        None,
                        provider_started_at,
                        "redux driver cancelled",
                        &config.cost,
                        &config.workspace_dir,
                    )
                    .await
                    {
                        if chat_turn_binding.as_ref().is_some_and(|binding| binding.is_decided()) { return Err(error); }
                        tracing::warn!(error = %error, "Failed to commit shared cancelled Chat terminal event");
                    }
                    if let Some(ref id) = draft_id {
                        let _ = terminal.cancel_draft("user", id).await;
                    }
                    rollback_cancelled_turn_history(&mut history, history_len_before_user_turn);
                    publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                    publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                }
            }

            continue;
        }

        let preflight_budget = crate::agent::context_budget::plan_context_budget(
            &history,
            &effective_compaction.config,
            crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
        );
        if preflight_budget.over_hard_limit && !collection_turn {
            let audit_source_history = original_legacy_chat_compaction_audit_source(&persisted_history_for_turn);
            let summary_projection = bounded_legacy_chat_compaction_audit_source(&persisted_history_for_turn);
            let token_metadata = legacy_compaction_token_metadata(&history, &persisted_history_for_turn);
            let system_count = usize::from(history.first().is_some_and(|m| m.role == "system"));
            let turns_before = history.len().saturating_sub(system_count);
            let tokens_before = estimate_chat_history_tokens(&history);
            if let Some(patch) = build_chat_compaction_patch(
                &history,
                &persisted_history_for_turn,
                provider.as_ref(),
                model_name,
                &effective_compaction.config,
                memory_runtime.ingest(),
                "chat_preflight",
            )
            .await
            {
                apply_chat_compaction_patch_and_sync(
                    &mut history,
                    Some(&persisted_history_for_turn),
                    patch,
                    &effective_compaction.config,
                    crate::chat::action::CompactReason::ContextOverflow,
                    &chat_dispatcher,
                );
            } else {
                compact_chat_history(&mut history);
                let _ = chat_dispatcher.dispatch_or_log(
                    crate::chat::action::Action::HistoryCompacted {
                        reason: crate::chat::action::CompactReason::ContextOverflow,
                    },
                    "chat.history_compacted_preflight",
                );
            }
            persist_legacy_chat_compaction_audit(
                mem.as_ref(),
                &runtime_envelope,
                &audit_source_history,
                &summary_projection,
                token_metadata,
                "chat_preflight",
            )
            .await;
            #[cfg(feature = "terminal-tui")]
            refresh_context_budget_for_tui(
                &history,
                &effective_compaction.config,
                redraw_tx_for_main.is_some(),
                &chat_mirror,
                &chat_dispatcher,
            );
            let compact_msg = format_compact_feedback_after_history(
                turns_before,
                tokens_before,
                &history,
                &effective_compaction.config,
            );
            surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &compact_msg);
            tracing::warn!(
                before_used_tokens = preflight_budget.used_tokens,
                hard_limit = preflight_budget.available_input_tokens,
                "legacy chat context budget preflight compacted before provider call"
            );
        }

        // D8-4: seed a turn-root spawn execution context so a sub-agent spawned
        // directly from this chat turn inherits parent_run_id = the per-turn
        // run_id. spawn_depth starts at 0 and is_turn_root keeps the first child's
        // reported depth at 0 (a turn is not itself a spawn). The chat session key is the
        // turn's spawn session scope.
        let turn_spawn_ctx = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
            turn_run_id.clone(),
            chat_session_key.clone(),
        );
        let turn_message_send_ctx = crate::tools::message_send::MessageSendExecutionContext::new(
            Some("user".to_string()),
            Arc::clone(&terminal) as Arc<dyn Channel>,
        );

        // Keep the large trace inline outside the small outcome discriminator.
        let (turn_outcome, successful_trace) = {
            #[cfg(feature = "terminal-tui")]
            {
                history_len_before_tools = history.len();
            }

            let result = crate::tools::message_send::MESSAGE_SEND_EXECUTION_CONTEXT
                .scope(
                    turn_message_send_ctx.clone(),
                    crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT.scope(turn_spawn_ctx.clone(), async {
                        run_turn(
                            &mut history,
                            crate::agent::turn_request::TurnRequest {
                                channel_name: "terminal",
                                scope: Some(&scope_ctx),
                                mode: admitted_binding.admitted_model_mode()?,
                                silent: false,
                                expose_stay_silent: false,
                                multimodal: &config.multimodal,
                                schedule: crate::agent::turn_request::TurnSchedule {
                                    read_only_tool_concurrency_window: config.agent.read_only_tool_concurrency_window,
                                    priority_scheduling_enabled: config.agent.priority_scheduling_enabled,
                                    low_priority_tool_names: config.agent.low_priority_tools.clone(),
                                },
                                compaction: Some(&effective_compaction.config),
                                decision: admitted_binding.decision_routing()
                                    .ok_or_else(|| anyhow::anyhow!("chat runtime turn requires an admitted decision"))?,
                            },
                            crate::agent::turn_request::TurnServices {
                                provider: provider.as_ref(),
                                provider_name,
                                model: model_name,
                                temperature,
                                tools_registry: Arc::clone(&tools_registry),
                                observer: observer.as_ref(),
                                hooks: &hooks,
                                approval: Some(Arc::clone(&approval_manager)),
                                approval_resolver: None,
                                runtime_adapter: chat_turn_binding.as_ref().map(|binding| crate::agent::turn_contracts::ToolLoopRuntimeAdapter {
                                    provider_trace: binding.retained_provider_trace(),
                                    events: None, stream_provider: false, tool_execution_service: None,
                                    tool_execution_context: crate::tools::ToolExecutionContext::new(binding.envelope().clone(), "private")
                                        .with_chat_id("terminal:user"),
                                    allowed_tool_names: None,
                                }),
                                memory: memory_runtime.clone(),
                            },
                            crate::agent::turn_request::TurnDelivery {
                                cancellation: Some(cancellation.clone()),
                                on_delta: Some(delta_tx.clone()),
                                on_tool_call: Some(tool_event_tx.clone()),
                            },
                        )
                        .await
                        .map(|(outcome, trace)| (outcome.into_text(), trace))
                    }),
                )
                .await;

            match result {
                // ── Success ───────────────────────────────────────
                Ok((resp, trace)) => (TurnOutcome::Success(resp), Some(trace)),
                // ── Cancelled (Ctrl+C) ────────────────────────────
                Err(ref e) if is_tool_loop_cancelled(e) || cancellation.is_cancelled() => {
                    if let Some(ref d_id) = draft_id {
                        let _ = terminal.cancel_draft("user", d_id).await;
                    }
                    // S2-A: user-driven cancel — distinguished from real
                    // failures so the reducer emits `StreamCancelled` (no
                    // Error hook fan-out) instead of `StreamFailed`.
                    (TurnOutcome::Cancelled, None)
                }
                // ── Other errors ──────────────────────────────────
                Err(e) => {
                    if let Some(ref d_id) = draft_id {
                        let _ = terminal.cancel_draft("user", d_id).await;
                    }
                    let err_text = e.to_string();
                    eprintln!("\nError: {err_text}\n");
                    if plain_mode {
                        plain_mode_turn_failed = true;
                    }
                    // Phase E (5a-4): same dual_write_guard reasoning as the timeout branch.
                    if !dual_write_guard.is_active() {
                        hooks
                            .emit(HookEvent::Error, payload_error("chat-turn", &err_text))
                            .await;
                    }
                    // S2-A: generic provider/loop error — retryable hint is
                    // false (the caller already chose to surface and continue;
                    // retry policy is owned by upstream once tooling lands).
                    (
                        TurnOutcome::FailedWithError {
                            err: err_text,
                            retryable: false,
                        },
                        None,
                    )
                }
            }
        };

        // ── Finalize streaming ────────────────────────────────────
        // Deregister this turn's cancellation token.
        //
        // S2-B Step 3: on the legacy path the reducer also clears `state.control.active_cancel` when
        // Stream{Completed,Cancelled,Failed} is dispatched (lines 1886-1911 below); the legacy Arc is
        // only a fallback in Off/Both mode (the external Ctrl+C handler reads it).
        #[cfg(not(feature = "terminal-tui"))]
        {
            *active_cancel.lock() = None;
        }

        // Drop our channel senders so background tasks receive channel close
        drop(delta_tx);
        drop(tool_event_tx);
        if let Some(handle) = draft_updater {
            let _ = handle.await;
        }
        let _ = tool_event_forwarder.await;

        // Step 5b dual write: dispatch the matching stream-termination Action for the turn outcome.
        // Only when draft_id exists can the reducer match stream.draft and clean it up; otherwise this
        // is a no-op.
        //
        // S2-A: split the previous single-pronged "Failed → StreamCancelled"
        // fallback. Order is **critical**: cancellation is detected up the
        // stack via `is_tool_loop_cancelled` / `cancellation.is_cancelled()`
        // and surfaces as `TurnOutcome::Cancelled`. Real errors (timeout /
        // context overflow / provider error) surface as `FailedWithError` and
        // map to `StreamFailed { err, retryable }` so the reducer emits the
        // `NotifyHook(Error) + LogTrace + RequestRedraw` effect chain.
        //
        // T3-3-fixA P0-1: the StreamCompleted of the Success branch was moved down to be dispatched
        // after RecordAssistantTurn, so that when the reducer builds the SaveSession snapshot
        // session.turns already contains this turn's assistant message. Cancelled / FailedWithError
        // write no assistant turn, so their dispatch position is unchanged.
        match &turn_outcome {
            TurnOutcome::Success(..) => {}
            TurnOutcome::Cancelled => {
                if let Some(ref d_id) = draft_id {
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::StreamCancelled { draft_id: d_id.clone() },
                        "chat.stream_cancelled",
                    );
                }
                if let Err(error) = finalize_chat_non_success_turn(
                    chat_turn_binding.as_deref(),
                    &memory_fabric,
                    &turn_run_id,
                    route_scope.clone(),
                    crate::agent::terminal::TurnTerminalStatus::Cancelled,
                    None,
                    provider_started_at,
                    "legacy tool loop cancelled",
                    &config.cost,
                    &config.workspace_dir,
                )
                .await
                {
                    if chat_turn_binding.as_ref().is_some_and(|binding| binding.is_decided()) { return Err(error); }
                    tracing::warn!(error = %error, "Failed to commit shared cancelled Chat terminal event");
                }
            }
            TurnOutcome::FailedWithError { err, retryable } => {
                if let Some(ref d_id) = draft_id {
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::StreamFailed {
                            draft_id: d_id.clone(),
                            err: err.clone(),
                            retryable: *retryable,
                        },
                        "chat.stream_failed",
                    );
                }
                // The terminal records the loop failure. Decided turns retain
                // only observed provider attempts in their owning trace.
                if let Err(error) = finalize_chat_non_success_turn(
                    chat_turn_binding.as_deref(),
                    &memory_fabric,
                    &turn_run_id,
                    route_scope.clone(),
                    crate::agent::terminal::TurnTerminalStatus::Failed,
                    None,
                    provider_started_at,
                    &format!("legacy tool loop failed: {err}"),
                    &config.cost,
                    &config.workspace_dir,
                )
                .await
                {
                    if chat_turn_binding.as_ref().is_some_and(|binding| binding.is_decided()) { return Err(error); }
                    tracing::warn!(error = %error, "Failed to commit shared failed Chat terminal event");
                }
                surface_turn_elapsed_message(
                    &chat_dispatcher,
                    sessions_redraw_handle.as_ref(),
                    "failed",
                    provider_started_at,
                    chrono::Utc::now(),
                );
            }
        }

        // If the turn failed or was cancelled, skip response processing
        let (response, turn_trace) = match turn_outcome {
            TurnOutcome::Success(resp) => (resp, successful_trace.context("successful Chat outcome has no serving trace")?),
            TurnOutcome::Cancelled => {
                rollback_cancelled_turn_history(&mut history, history_len_before_user_turn);
                gate_cancelled_provider_turn_finalization(
                    &mut turn_scheduler,
                    &mut history_commit_coordinator,
                    &mut provider_turn_workers,
                    provider_turn_task_id,
                    "legacy tool loop cancelled",
                );
                publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                continue;
            }
            TurnOutcome::FailedWithError { err, .. } => {
                gate_failed_provider_turn_finalization(
                    &mut turn_scheduler,
                    &mut history_commit_coordinator,
                    &mut provider_turn_workers,
                    provider_turn_task_id,
                    history.len(),
                    format!("legacy tool loop failed: {err}"),
                );
                publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                continue;
            }
        };
        if let Some(binding) = chat_turn_binding.as_ref() {
            binding.capture_trace(&turn_trace)?;
        }
        // FIX-P0-30/31: build the provider outcome from the loop's real
        // attribution trace. When the trace carries the actual serving
        // provider/model + attempts (the `ReliableProvider` path), use them so a
        // retry/fallback is recorded as `FallbackSuccess` and `final_model`
        // reflects what truly executed. Fall back to the routed
        // `decision.selected.{provider,model}` only when no trace is available
        // (e.g. a provider whose `chat_traced` default produced a synthetic
        // attribution).
        let provider_outcome = {
            let has_trace = turn_trace.final_model.is_some() && !turn_trace.attempts.is_empty();
            if has_trace {
                let final_provider = turn_trace
                    .final_provider
                    .unwrap_or_else(|| route_decision.selected.provider.clone());
                let final_model = turn_trace
                    .final_model
                    .unwrap_or_else(|| route_decision.selected.model.clone());
                ProviderExecutionOutcome::from_trace_with_usage(
                    &route_decision,
                    turn_trace.attempts,
                    final_provider,
                    final_model,
                    provider_started_at,
                    chrono::Utc::now(),
                    // FIX #2: a fallback on any earlier (tool-call) turn must
                    // surface as FallbackSuccess even when the final turn is clean.
                    turn_trace.any_turn_had_fallback,
                    turn_trace.tokens_used,
                )
            } else {
                ProviderExecutionOutcome::success_for_decision_with_usage(
                    &route_decision,
                    provider_started_at,
                    turn_trace.tokens_used,
                )
            }
        };
        surface_turn_elapsed_message(
            &chat_dispatcher,
            sessions_redraw_handle.as_ref(),
            "completed",
            provider_outcome.started_at,
            provider_outcome.finished_at,
        );
        // d04 §10 G7: emit a control-ladder trace carrying the structured
        // decision_id / final_provider / final_model / attempts_count so a
        // `decision_id` join links the routing decision to the provider that
        // actually served the request. Best-effort (failures logged, not fatal).
        {
            let status_label = match &provider_outcome.status {
                crate::llm::route_decision::ExecutionStatus::Success => "success",
                crate::llm::route_decision::ExecutionStatus::FallbackSuccess => "fallback_success",
                crate::llm::route_decision::ExecutionStatus::AllFailed { .. } => "all_failed",
            };
            let attempts_count = u8::try_from(provider_outcome.attempts.len()).unwrap_or(u8::MAX);
            crate::runtime::control_ladder::append_provider_outcome_trace(
                std::path::Path::new(&config.workspace_dir),
                &provider_outcome.decision_id,
                &provider_outcome.final_provider,
                &provider_outcome.final_model,
                attempts_count,
                status_label,
            );
        }

        // ── P2-12: Mirror reasoning content into the TUI as a folded card. ──
        //
        // Reasoning is separated upstream at the provider layer (P0-2) and
        // persisted into the assistant-history JSON via
        // `build_native_assistant_history`. We scan the slice of history
        // produced during this turn for `reasoning_content` fields, aggregate
        // them, and push a single folded `Reasoning` card to the TUI mirror.
        // Empty buffers are skipped by `push_reasoning`. This does NOT touch
        // the visible delta stream — the user-facing assistant text remains
        // the only thing rendered in the streaming draft.
        #[cfg(feature = "terminal-tui")]
        {
            let turn_slice = history.get(history_len_before_tools..).unwrap_or(&[]);
            let aggregated = collect_reasoning_from_history_slice(turn_slice);
            if !aggregated.is_empty() {
                // S4-B: the mirror push_reasoning is deleted; the reducer
                // (reduce_stream_completed) is the single source that pushes the Reasoning card.
                if let Some(tx) = redraw_tx_for_main.as_ref() {
                    let _ = tx.try_send(());
                }
            }
        }

        increment_recalled_useful_counts(mem.as_ref(), &mem_context.ids).await;

        // ── Sanitize response: strip tool-call XML/JSON artifacts ──
        let response = sanitize_channel_response(&response, &tools_registry);

        if crate::agent::response_guard::is_empty_assistant_response(&response, false) {
            let terminal_usage = match finalize_chat_success_turn(
                chat_turn_binding.as_deref(),
                &memory_fabric,
                &chat_session_key,
                &turn_run_id,
                provider_name,
                model_name,
                route_scope.clone(),
                provider_outcome.clone(),
                None,
                history.len(),
                "legacy tool loop completed with empty response",
                &config.cost,
                &config.workspace_dir,
            )
            .await
            {
                Ok(receipt) => receipt.usage_settlement,
                Err(error) => {
                    return Err(error);
                }
            };
            if let Some(ref d_id) = draft_id {
                let _ = chat_dispatcher.dispatch_or_log(
                    crate::chat::action::Action::StreamCompleted {
                        draft_id: d_id.clone(),
                        final_text: String::new(),
                        reasoning: String::new(),
                    },
                    "chat.stream_completed_empty_response",
                );
                if let Err(e) = terminal.cancel_draft("user", d_id).await {
                    tracing::debug!(error = %e, "cancel empty-response draft failed");
                }
            }
            surface_session_message(
                &chat_dispatcher,
                sessions_redraw_handle.as_ref(),
                crate::agent::response_guard::EMPTY_ASSISTANT_RESPONSE_MESSAGE,
            );

            chat_session.add_user_turn(&user_input);
            if let Some(record) = terminal_usage.and_then(|record| chat_session.record_usage_settlement(record)) {
                record_provider_turn_usage(&mut turn_scheduler, provider_turn_task_id, &record);
                #[cfg(feature = "terminal-tui")]
                {
                    chat_mirror.lock().token_usage_summary = chat_session.token_usage_summary();
                }
                let _ = chat_dispatcher.dispatch_or_log(
                    crate::chat::action::Action::ProviderUsageRecorded {
                        task_id: provider_turn_task_id,
                        usage_kind: crate::chat::action::ProviderUsageRecordKind::FinalAggregate,
                        record,
                    },
                    "chat.provider_usage_recorded_empty_response",
                );
                #[cfg(feature = "terminal-tui")]
                if let Some(tx) = redraw_tx_for_main.as_ref() {
                    let _ = tx.try_send(());
                }
            }
            if !dual_write_guard.is_active() {
                if let Err(e) = save_session(mem.as_ref(), &chat_session).await {
                    tracing::warn!("Failed to persist session after empty assistant response: {e}");
                }
                observer.record_event(&ObserverEvent::TurnComplete);
                hooks
                    .emit(
                        HookEvent::TurnComplete,
                        serde_json::json!({
                            "mode": "chat",
                            "response_chars": 0,
                        }),
                    )
                    .await;
            } else {
                observer.record_event(&ObserverEvent::TurnComplete);
            }
            mark_provider_turn_completed(
                &mut turn_scheduler,
                &mut history_commit_coordinator,
                &mut provider_turn_workers,
                provider_turn_task_id,
                history.len(),
                "legacy tool loop completed with empty response",
            );
            publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
            publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
            continue;
        }

        let terminal_usage = match finalize_chat_success_turn(
            chat_turn_binding.as_deref(),
            &memory_fabric,
            &chat_session_key,
            &turn_run_id,
            provider_name,
            model_name,
            route_scope.clone(),
            provider_outcome.clone(),
            Some(response.clone()),
            history.len().saturating_add(1),
            "legacy tool loop completed",
            &config.cost,
            &config.workspace_dir,
        )
        .await
        {
            Ok(receipt) => receipt.usage_settlement,
            Err(error) => {
                return Err(error);
            }
        };

        // Persist only the user-facing assistant response. Tool results already
        // live in structured history entries; adding a synthetic summary here
        // can teach the model to emit that internal marker as a final answer.
        history.push(ChatMessage::assistant(&response));

        // S2-B Step 4: RecordAssistantTurn(response) is dispatched at the same point as the legacy
        // `history.push(ChatMessage::assistant(...))` — so the reducer's session.history is
        // byte-for-byte aligned with the legacy history. The old dispatch at line 2055 below used
        // sanitized_response, whose content differs from history.push — from S2-B Step 4 on it is
        // dispatched here with response instead, and the old dispatch below is deleted.
        let _ = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::RecordAssistantTurn {
                task_id: provider_turn_task_id,
                content: response.clone(),
            },
            "chat.record_assistant_turn",
        );

        // T3-3-fixA P0-1: StreamCompleted must be dispatched after RecordAssistantTurn, because the
        // reducer's reduce_stream_completed emits Effect::SaveSession(snapshot) and session.turns must
        // already contain this turn's assistant message by then — otherwise SaveSession persists a
        // stale snapshot. final_text matches the response persisted above, keeping the reducer's
        // conversation_lines, history records and the UI aligned.
        if let Some(ref d_id) = draft_id {
            let _ = chat_dispatcher.dispatch_or_log(
                crate::chat::action::Action::StreamCompleted {
                    draft_id: d_id.clone(),
                    final_text: response.clone(),
                    reasoning: String::new(),
                },
                "chat.stream_completed",
            );
        }

        // ── Extract and display media markers (images, documents, etc.) ──
        let (clean_response, media_items) = extract_outgoing_media(&response);
        for (media_type, path) in &media_items {
            if media_type == "IMAGE" && std::path::Path::new(path).exists() {
                if let Err(e) = terminal_proto::display_image(path) {
                    tracing::debug!("Image display failed: {e}");
                    println!("  [image: {path}]");
                }
            } else {
                println!("  [{media_type}: {path}]");
            }
        }
        // Use the cleaned response (media markers removed) for display
        let display_response = if media_items.is_empty() {
            &response
        } else {
            &clean_response
        };

        // Finalize the streaming draft with the full response.
        //
        // Idempotency contract (P1-4):
        //   When a draft exists, the streamed deltas have already painted the
        //   full response on screen via `update_draft`. `finalize_draft` is
        //   only a structural close (locking in the final text for non-TTY
        //   channels such as Telegram). On failure we therefore MUST NOT
        //   re-send the whole message — that would duplicate output that the
        //   user has already seen via the live stream. We log a warning and
        //   carry on; the assistant turn is already in `history`, so the
        //   conversation state remains consistent.
        //
        //   The "no draft" branch is the genuine first-send path (drafts were
        //   never created, e.g. `send_draft` failed earlier and returned
        //   `None`), so a normal `send` is correct and not a duplicate.
        if let Some(ref d_id) = draft_id {
            if let Err(e) = terminal.finalize_draft("user", d_id, display_response).await {
                if should_resend_on_finalize_failure(true) {
                    let rendered = render_response(display_response);
                    let _ = terminal.send(&SendMessage::new(rendered, "user")).await;
                } else {
                    tracing::warn!(
                        error = %e,
                        "finalize_draft failed; suppressing resend to preserve idempotency (user already saw streamed content)"
                    );
                }
            }
        } else {
            // No draft was created — send as a complete message with highlighting.
            let rendered = render_response(display_response);
            let _ = terminal.send(&SendMessage::new(rendered, "user")).await;
        }

        // ── Record turn in session + persist ───────────────────
        // S2-B Step 4: RecordUserTurn / RecordAssistantTurn were already dispatched above (at the
        // same points as enriched / response); the legacy `chat_session.add_*_turn` here is kept in
        // `Off` / `Both` / `Redux` mode, because `chat_session` is still the real persistence source
        // for `save_session(mem, &chat_session)`.
        //
        // T3-3-c wrap-up: **Pure mode skips the legacy add_*_turn** — the reducer's
        // `RecordUserTurn` / `RecordAssistantTurn` + `Effect::SaveSession` take over single-source
        // persistence, and the `save_session(...)` below is also suppressed by `dual_write_guard`.
        // That closes the last dual-write remnant kept from the S2-D/E stage.
        // BUG-06 / BUG-08 fix: always keep the in-memory `chat_session.turns`
        // populated so interactive `/cost` and `/export` (which read
        // `ctx.chat_session.turns`) reflect the live conversation. In Pure mode
        // the reducer owns *persistence* (its `build_session_snapshot` +
        // `Effect::SaveSession`), and the legacy `save_session(&chat_session)`
        // below is independently suppressed by `dual_write_guard`. Populating the
        // in-memory turns therefore does NOT cause double-persistence — it only
        // backs the slash commands that read from `chat_session`.
        chat_session.add_user_turn(&user_input);
        chat_session.add_assistant_turn(&response, Vec::new());
        if let Some(record) = terminal_usage.and_then(|record| chat_session.record_usage_settlement(record)) {
            record_provider_turn_usage(&mut turn_scheduler, provider_turn_task_id, &record);
            #[cfg(feature = "terminal-tui")]
            {
                chat_mirror.lock().token_usage_summary = chat_session.token_usage_summary();
            }
            let _ = chat_dispatcher.dispatch_or_log(
                crate::chat::action::Action::ProviderUsageRecorded {
                    task_id: provider_turn_task_id,
                    usage_kind: crate::chat::action::ProviderUsageRecordKind::FinalAggregate,
                    record,
                },
                "chat.provider_usage_recorded",
            );
            #[cfg(feature = "terminal-tui")]
            if let Some(tx) = redraw_tx_for_main.as_ref() {
                let _ = tx.try_send(());
            }
        }

        // P0-1 fix: in Both/Redux mode the old path is gated by dual_write_guard.
        // The Redux reducer's SaveSession effect already raised the guard inside execute_real, and
        // when the guard is active the old path skips save_session + hooks.emit(TurnComplete), which
        // prevents double hook/webhook firing (hooks/webhooks are not idempotent and really would
        // fire twice).
        // In Off mode the guard is always false and the old path writes alone as usual.
        // The guard is turn-level (not effect-level): once Redux has executed either
        // SaveSession or NotifyHook during a turn, the guard is active and every later write of the
        // old path is suppressed.
        if !dual_write_guard.is_active() {
            if let Err(e) = save_session(mem.as_ref(), &chat_session).await {
                tracing::warn!("Failed to persist session: {e}");
            }

            observer.record_event(&ObserverEvent::TurnComplete);
            hooks
                .emit(
                    HookEvent::TurnComplete,
                    serde_json::json!({
                        "mode": "chat",
                        "response_chars": response.chars().count(),
                    }),
                )
                .await;
        } else {
            // Guard active: Redux path already handled save_session + hooks.emit.
            // The old path skips this, to avoid duplicate writes/notifications.
            tracing::debug!(
                "P0-1: dual_write_guard active — legacy path skipping save_session + hooks.emit(TurnComplete)"
            );
            // observer.record_event is still called (the observer is only a local counter, with no
            // external side effects).
            observer.record_event(&ObserverEvent::TurnComplete);
        }
        mark_provider_turn_completed(
            &mut turn_scheduler,
            &mut history_commit_coordinator,
            &mut provider_turn_workers,
            provider_turn_task_id,
            history.len(),
            "legacy tool loop completed",
        );
        publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
        publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
        // The reply this turn produced *is* the answer to the assignment, so it
        // is what goes back. Reaching here at all is what distinguishes a turn
        // that answered from every other way the body can end.
        if let Some(guard) = assignment_guard.as_mut() {
            guard.completed(&response);
        }
    }

    // ── Child-session shutdown on exit (Phase C) ─────────────────
    // Snapshot every child session still tracked at exit, persist summaries for
    // live sessions as interrupted, then terminate and clear every child
    // registry through the single session owner. This covers running agents,
    // NeedsInput agents, background shells, and interactive PTYs uniformly.
    let _shutdown_report =
        shutdown_child_sessions_for_exit(&mut chat_sessions, &mut chat_session, &chat_dispatcher).await;
    session_rings.clear();
    withdraw_assignment_enrolment(
        assignment_enrolment.as_ref(),
        &chat_dispatcher,
        sessions_redraw_handle.as_ref(),
    )
    .await;

    // Step 5b: dispatcher task graceful shutdown.
    //
    // 1. shutdown.cancel() makes every spawned signal handler / TUI loop exit on its own and release
    //    the chat_dispatcher sender clone it holds (otherwise action_rx would never close naturally
    //    and dispatcher_handle.await would hang).
    // 2. drop(chat_dispatcher) releases the sender held by the main path.
    // 3. dispatcher_handle.await finishes the teardown — the shutdown.cancelled() arm of the select!
    //    fires immediately and the dispatcher exits. The RUNTIME_SHUTDOWN_TIMEOUT (2s) at
    //    main.rs:866 remains as the fallback (do not change it).
    shutdown.cancel();
    drop(chat_dispatcher);
    match dispatcher_handle.await {
        Ok(stats) => tracing::info!(
            actions = stats.actions_seen,
            effects = stats.effects_seen,
            "redux dispatcher shutdown clean"
        ),
        Err(e) => tracing::warn!(error = %e, "redux dispatcher join failed"),
    }

    // Restore terminal state after the TUI loop has observed shutdown, so no
    // late redraw can print the footer after the shell prompt.
    #[cfg(feature = "terminal-tui")]
    if terminal_guard.is_some() {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(std::io::stderr(), crossterm::cursor::Show);
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(b"\r\n");
        let _ = stdout.flush();
    }

    // T3-3-fixA P0-2: Pure guard for the exit-time save_session.
    //
    // In Pure mode chat_session.add_*_turn is skipped by the guard at line 2185, so chat_session.turns
    // lags behind the SessionState maintained by the reducer. Saving unconditionally on exit would
    // overwrite the latest snapshot the reducer already persisted with a stale one. The guard
    // expression is kept structurally identical to the one at line 2185.
    #[cfg(feature = "terminal-tui")]
    let legacy_exit_save_enabled = false; // S4-B: Pure single source
    #[cfg(not(feature = "terminal-tui"))]
    let legacy_exit_save_enabled = true;
    if legacy_exit_save_enabled {
        // Final session save before exit
        if let Err(e) = save_session(mem.as_ref(), &chat_session).await {
            tracing::warn!("Failed to persist session on exit: {e}");
        }
    } else {
        tracing::debug!("Pure mode: skip legacy exit save_session (reducer owns persistence)");
    }

    info!("Chat session ended");
    if plain_mode_turn_failed {
        anyhow::bail!("one or more chat turns failed");
    }
    Ok(())
    }).await;
    turn_decisions
        .shutdown(std::time::Instant::now() + Duration::from_secs(2))
        .await;
    chat_result
}

// ── Response rendering ───────────────────────────────────────────────────

/// Idempotency policy for the `finalize_draft` failure path.
///
/// Returns `true` if the caller should re-send the full response as a fresh
/// message when `finalize_draft` returns `Err`. Today this is always `false`
/// when a draft was active: streamed deltas have already painted the full
/// text on screen via `update_draft`, so resending would duplicate the
/// assistant turn (this was the P1-4 regression).
///
/// `had_active_draft = true` means a `send_draft` previously succeeded and
/// the user has seen the streamed output. The function is intentionally a
/// pure decision so it can be unit-tested without spinning up a channel.
const fn should_resend_on_finalize_failure(had_active_draft: bool) -> bool {
    // If a draft was active, the user already saw the streamed response —
    // resending would duplicate it. The "no draft" path is handled by the
    // caller directly (it is the genuine first send, not a fallback).
    !had_active_draft
}

/// Apply markdown highlighting to a response (when terminal-tui feature is active).
/// Falls back to plain formatting with newline wrapping.
fn render_response(response: &str) -> String {
    #[cfg(feature = "terminal-tui")]
    {
        format!("\n{}\n", renderer::render_markdown_with_highlighting(response))
    }
    #[cfg(not(feature = "terminal-tui"))]
    {
        format!("\n{response}\n")
    }
}

// ── P3-3: ratatui render task ────────────────────────────────────────────

/// Spawn the blocking `ratatui::Terminal` render task.
///
/// Owns a `Terminal<CrosstermBackend<Stdout>>` for the duration of the chat
/// session and redraws the four-area layout (status / output / input /
/// footer) on demand. Demand is signalled by any producer that mutated the
/// shared `Arc<Mutex<TuiState>>`; the wakeup channel is a `tokio::sync::mpsc`
/// of capacity 1 used as a coalescer (multiple `try_send(())` calls collapse
/// into a single deferred redraw).
///
/// S4-A Commit 4: RenderSource — in Pure mode the snapshot is read from the `watch::Receiver`; in
/// Off/Both/Redux mode the TuiState is read from the mirror lock.
///
/// The render hot path uses the [`Self::with_view`] closure to obtain a `&dyn BottomChromeView`
/// uniformly, so the two paths do not duplicate code.
#[cfg(feature = "terminal-tui")]
pub(crate) enum RenderSource {
    Mirror(Arc<parking_lot::Mutex<tui::TuiState>>),
    Snapshot(tokio::sync::watch::Receiver<Arc<crate::chat::state::UiSnapshot>>),
}

#[cfg(feature = "terminal-tui")]
fn should_log_tui_key_event(key: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};

    !matches!(key.code, KeyCode::Char(_)) || key.modifiers != KeyModifiers::NONE
}

#[cfg(feature = "terminal-tui")]
fn is_plain_character_key(key: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};

    matches!(key.code, KeyCode::Char(_)) && key.modifiers == KeyModifiers::NONE
}

#[cfg(feature = "terminal-tui")]
fn plain_character_from_key(key: &crossterm::event::KeyEvent) -> Option<char> {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) && key.modifiers == KeyModifiers::NONE {
        if let KeyCode::Char(ch) = key.code {
            return Some(ch);
        }
    }
    None
}

#[cfg(feature = "terminal-tui")]
fn drain_plain_character_burst(
    text: &mut String,
    pending_events: &mut VecDeque<crossterm::event::Event>,
) -> Result<()> {
    use crossterm::event::Event;

    const MAX_BURST_BYTES: usize = 256 * 1024;

    while text.len() < MAX_BURST_BYTES && crossterm::event::poll(Duration::ZERO)? {
        let next = crossterm::event::read()?;
        if let Event::Key(key) = &next {
            if let Some(ch) = plain_character_from_key(key) {
                text.push(ch);
                continue;
            }
        }
        pending_events.push_back(next);
        break;
    }

    Ok(())
}

#[cfg(feature = "terminal-tui")]
impl RenderSource {
    pub(crate) fn with_view<R>(&self, f: impl FnOnce(&dyn tui::BottomChromeView) -> R) -> R {
        match self {
            Self::Mirror(arc) => {
                let guard = arc.lock();
                f(&*guard)
            }
            Self::Snapshot(rx) => {
                let snap_arc = rx.borrow();
                f(&**snap_arc)
            }
        }
    }
}

#[cfg(feature = "terminal-tui")]
fn sync_key_mirror_observation_state(render_source: &RenderSource, mirror: &Arc<parking_lot::Mutex<tui::TuiState>>) {
    let RenderSource::Snapshot(rx) = render_source else {
        return;
    };
    let snapshot = rx.borrow();
    let status = snapshot.provider_worker_status.clone();
    let conversation_lines = snapshot.conversation_lines.as_ref().clone();
    let streaming = snapshot.streaming.clone();
    let visible_streaming_drafts = Arc::clone(&snapshot.visible_streaming_drafts);
    drop(snapshot);
    let mut guard = mirror.lock();
    guard.provider_worker_status = status.clone();
    guard.conversation_lines = conversation_lines;
    guard.streaming = streaming;
    guard.visible_streaming_drafts = visible_streaming_drafts;
    if let crate::chat::sessions::FocusTarget::Worker { sequence } = guard.focus {
        let previous_view = guard
            .active_session_view
            .as_ref()
            .filter(|view| view.kind == crate::chat::action::PROVIDER_WORKER_VIEW_KIND && view.seq == sequence);
        let io_lines = tui::provider_worker_io_lines_for_streaming_draft(
            &guard.conversation_lines,
            guard.streaming_draft_for_worker(sequence),
            12,
        );
        guard.active_session_view = Some(
            crate::chat::action::build_provider_worker_active_view_with_io_preserving_scroll(
                &status,
                sequence,
                previous_view,
                io_lines,
            ),
        );
    }
}

#[cfg(feature = "terminal-tui")]
type ChatTerminal = ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>;

#[cfg(feature = "terminal-tui")]
fn new_fullscreen_terminal() -> Result<ChatTerminal> {
    let stdout = std::io::stdout();
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    ratatui::Terminal::new(backend).map_err(|e| anyhow::anyhow!("ratatui Terminal::new failed: {e}"))
}

/// Runs inside `tokio::task::spawn_blocking` because `terminal.draw()`
/// performs synchronous I/O and `mpsc::Receiver::blocking_recv()` blocks the
/// caller. Returning a `JoinHandle` lets the caller observe panics if
/// desired (the chat loop currently fires-and-forgets — terminal restoration
/// is owned by `TerminalGuard::Drop`).
///
/// Lock policy: the render path takes the mirror lock for as briefly as
/// possible (the borrow is dropped before the next iteration parks).
/// Producers hold the same lock only across short, non-blocking mutations,
/// so the renderer never starves.
/// Spawn the unified ratatui TUI loop on a dedicated blocking thread.
///
/// **Single-thread architecture (P3 rearch).** The previous design split
/// rendering, input reading, and a periodic heartbeat across three separate
/// `spawn_blocking` tasks, each of which held its own raw `std::io::stdout()`
/// handle. crossterm's internal ANSI queries (DA1/DSR, used during key
/// dispatch and bracketed-paste decoding) wrote bytes to the same stdout
/// that ratatui's buffer flush was writing to — so characters made it into
/// ratatui's internal buffer but were partially overwritten on the wire,
/// producing the historic "I typed but nothing appears" bug.
///
/// This single loop is what every reference implementation does
/// (`ratatui/examples/user_input.rs`, OpenAI codex-rs, atuin, yazi, helix,
/// zellij): **one thread owns the Terminal + stdout**, reads events
/// directly, and redraws between events. There is no second stdout writer
/// and no producer/consumer split that can starve the renderer.
///
/// Wakeup sources:
///   * Each `crossterm::event::poll(50 ms)` returns either a real event
///     (keys / resize / paste / focus / mouse) or a timeout — and on every
///     iteration we redraw, so an in-flight LLM stream pushing deltas into
///     `mirror` shows up within ~50 ms even with no keypress.
///   * `redraw_rx` lets the UiActor wake us immediately when a streaming
///     event arrives; we drain it (coalesce) and let the next loop top
///     redraw.
///   * `shutdown` cancels the loop on `Ctrl+D` (empty buffer), double
///     `Ctrl+C`, or SIGTERM.
///
/// The function intentionally does NOT touch raw-mode / alt-screen state —
/// that is owned by [`TerminalGuard`] in `run()`. If `Terminal::new` fails
/// the task logs and exits; the guard's Drop still restores the terminal.
#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
fn spawn_tui_unified_loop(
    input_tx: mpsc::Sender<crate::channels::traits::ChannelMessage>,
    control_tx: mpsc::Sender<ChatControlEvent>,
    mirror: Arc<parking_lot::Mutex<tui::TuiState>>,
    redraw_rx: mpsc::Receiver<()>,
    redraw_tx: mpsc::Sender<()>,
    shutdown: CancellationToken,
    last_ctrlc_ms: Arc<AtomicU64>,
    chat_dispatcher: dispatcher::ChatDispatcher,
    snapshot_rx: Option<tokio::sync::watch::Receiver<Arc<crate::chat::state::UiSnapshot>>>,
    handoff: Arc<crate::chat::sessions::pty::HandoffControl>,
    workspace_dir: std::path::PathBuf,
    security: Arc<crate::security::SecurityPolicy>,
) {
    // Dedicated OS thread, not the tokio blocking pool: this loop only ends at
    // shutdown, so parking it in the pool would permanently retire one of the
    // `max_blocking_threads` slots that every SQLite write, subprocess reap and
    // config reload competes for. The loop needs no tokio context — it drives
    // crossterm synchronously and hands work off via `blocking_send`, which is
    // in fact only legal *outside* a runtime context.
    let thread_result = crate::runtime::blocking::spawn_detached_thread("prx-tui-render", move || {
        let result = run_tui_unified_loop(
            input_tx,
            control_tx,
            mirror,
            redraw_rx,
            redraw_tx,
            &shutdown,
            last_ctrlc_ms,
            &chat_dispatcher,
            snapshot_rx,
            &handoff,
            workspace_dir,
            security,
        );
        if let Err(e) = result {
            tracing::error!("TUI unified loop error: {e}");
        }
    });
    if let Err(error) = thread_result {
        tracing::error!("failed to start TUI render thread: {error}");
    }
}

#[cfg(feature = "terminal-tui")]
fn refresh_at_path_candidates_for_tui(
    mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: &mpsc::Sender<()>,
    workspace_dir: &std::path::Path,
    security: &crate::security::SecurityPolicy,
) {
    let candidates = {
        let guard = mirror.lock();
        collect_at_path_candidates(&guard.input, workspace_dir, security)
    };
    {
        let mut guard = mirror.lock();
        guard.update_at_path_candidates(candidates.clone());
    }
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::AtPathCandidatesUpdated { candidates },
        "chat.at_path_candidates_updated",
    );
    let _ = redraw_tx.try_send(());
}

#[cfg(feature = "terminal-tui")]
fn collect_at_path_candidates(
    input: &tui::TuiInput,
    workspace_dir: &std::path::Path,
    security: &crate::security::SecurityPolicy,
) -> Vec<tui::AtPathCandidate> {
    const MAX_AT_PATH_CANDIDATES: usize = 50;

    let Some(filter) = input.at_path_filter_at_cursor() else {
        return Vec::new();
    };
    if filter.starts_with('/') || filter.starts_with('~') || !security.is_path_allowed(&filter) {
        return Vec::new();
    }
    let normalized = filter.trim_start_matches("./");
    let (base_rel, needle) = if normalized.ends_with('/') {
        (normalized.trim_end_matches('/'), "")
    } else if let Some((base, leaf)) = normalized.rsplit_once('/') {
        (base, leaf)
    } else {
        ("", normalized)
    };
    let base_for_policy = if base_rel.is_empty() { "." } else { base_rel };
    if !security.is_path_allowed(base_for_policy) {
        return Vec::new();
    }
    let base_abs = if base_rel.is_empty() {
        workspace_dir.to_path_buf()
    } else {
        workspace_dir.join(base_rel)
    };
    let Ok(base_resolved) = base_abs.canonicalize() else {
        return Vec::new();
    };
    if !security.is_resolved_path_allowed(&base_resolved) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&base_resolved) else {
        return Vec::new();
    };
    let needle = needle.to_ascii_lowercase();
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_name) = entry.file_name().into_string() else {
            continue;
        };
        if file_name == "." || file_name == ".." {
            continue;
        }
        let rel = if base_rel.is_empty() {
            file_name.clone()
        } else {
            format!("{base_rel}/{file_name}")
        };
        let file_name_lower = file_name.to_ascii_lowercase();
        let rel_lower = rel.to_ascii_lowercase();
        if !needle.is_empty()
            && !file_name_lower.contains(&needle)
            && !rel_lower.contains(&needle)
            && !tui::fuzzy_path_match(&rel_lower, &needle)
        {
            continue;
        }
        if !security.is_path_allowed(&rel) {
            continue;
        }
        let Ok(resolved) = entry.path().canonicalize() else {
            continue;
        };
        if !security.is_resolved_path_allowed(&resolved) {
            continue;
        }
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        candidates.push(tui::AtPathCandidate {
            path: if is_dir { format!("{rel}/") } else { rel },
            is_dir,
        });
    }
    candidates.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.path.cmp(&b.path)));
    candidates.truncate(MAX_AT_PATH_CANDIDATES);
    candidates
}

/// Send a synthetic slash command from the TUI key thread to the async main
/// loop, reusing the same `input_tx` channel as real user submissions (v1.1b).
///
/// Used for switcher Enter (`/attach <seq>`) and Esc-detach (`/detach`) so the
/// attach/detach logic stays in the single async owner (`attached_follow` lives
/// in the main loop) rather than being duplicated in the synchronous key thread.
/// Returns `Err(())` if the receiver has been dropped (chat tearing down).
#[cfg(feature = "terminal-tui")]
fn send_synthetic_command(
    input_tx: &mpsc::Sender<crate::channels::traits::ChannelMessage>,
    command: &str,
) -> Result<(), ()> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let msg = crate::channels::traits::ChannelMessage {
        id: uuid::Uuid::new_v4().to_string(),
        sender: SYNTHETIC_UI_COMMAND_SENDER.to_string(),
        reply_target: "user".to_string(),
        content: command.to_string(),
        channel: "terminal".to_string(),
        timestamp,
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    };
    input_tx.blocking_send(msg).map_err(|_| ())
}

fn is_local_operator_input(msg: &crate::channels::traits::ChannelMessage) -> bool {
    msg.channel == "terminal" && msg.sender == "user"
}

fn is_synthetic_ui_command(msg: &crate::channels::traits::ChannelMessage) -> bool {
    msg.sender == SYNTHETIC_UI_COMMAND_SENDER && msg.channel == "terminal"
}

fn classify_input_priority(msg: &crate::channels::traits::ChannelMessage) -> (InputQueuePriority, String) {
    if is_synthetic_ui_command(msg) {
        return (InputQueuePriority::Control, msg.content.clone());
    }
    input_priority_from_text(&msg.content)
}

fn input_priority_from_text(input: &str) -> (InputQueuePriority, String) {
    let trimmed = input.trim();
    for prefix in ["/now ", "/priority ", "!! "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return (InputQueuePriority::Priority, rest.to_string());
            }
        }
    }
    (InputQueuePriority::Normal, input.to_string())
}

fn enqueue_input_message_and_return_priority(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    mut msg: crate::channels::traits::ChannelMessage,
) -> InputQueuePriority {
    let (priority, content) = classify_input_priority(&msg);
    msg.content = content;
    backlog.push_back(QueuedInputMessage {
        priority,
        turn_task_id: None,
        msg,
    });
    priority
}

const fn turn_priority_from_input(priority: InputQueuePriority) -> crate::chat::turn_scheduler::TurnPriority {
    match priority {
        InputQueuePriority::Normal => crate::chat::turn_scheduler::TurnPriority::Normal,
        InputQueuePriority::Priority => crate::chat::turn_scheduler::TurnPriority::Priority,
        InputQueuePriority::Control => crate::chat::turn_scheduler::TurnPriority::Control,
    }
}

fn enqueue_input_message_and_return_priority_with_scheduler(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    mut msg: crate::channels::traits::ChannelMessage,
    history_base_len: usize,
) -> InputQueuePriority {
    let (priority, content) = classify_input_priority(&msg);
    msg.content = content;
    let turn_task_id = scheduler.enqueue(
        msg.content.clone(),
        turn_priority_from_input(priority),
        history_base_len,
    );
    backlog.push_back(QueuedInputMessage {
        priority,
        turn_task_id: Some(turn_task_id),
        msg,
    });
    priority
}

fn enqueue_input_message_with_scheduler(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    msg: crate::channels::traits::ChannelMessage,
    history_base_len: usize,
) {
    let _ = enqueue_input_message_and_return_priority_with_scheduler(backlog, scheduler, msg, history_base_len);
}

fn enqueue_input_message(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    msg: crate::channels::traits::ChannelMessage,
) {
    let _ = enqueue_input_message_and_return_priority(backlog, msg);
}

fn pop_next_queued_input_message(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
) -> Option<QueuedInputMessage> {
    let best_idx = next_queued_input_index(backlog)?;
    backlog.remove(best_idx)
}

fn next_queued_input_index(backlog: &std::collections::VecDeque<QueuedInputMessage>) -> Option<usize> {
    let mut best_idx = None;
    let mut best_priority = InputQueuePriority::Normal;
    for (idx, queued) in backlog.iter().enumerate() {
        if best_idx.is_none() || queued.priority > best_priority {
            best_idx = Some(idx);
            best_priority = queued.priority;
        }
    }
    best_idx
}

fn pop_next_input_task_with_scheduler(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
) -> Option<DequeuedInputMessage> {
    let queued = pop_next_queued_input_message(backlog)?;
    if let Some(id) = queued.turn_task_id
        && let Err(error) = scheduler.mark_dispatched_to_chat_loop(id)
    {
        tracing::warn!(
            task_id = id.get(),
            error = ?error,
            "TurnScheduler chat-loop dequeue mirror failed"
        );
    }
    Some(DequeuedInputMessage {
        priority: queued.priority,
        turn_task_id: queued.turn_task_id,
        msg: queued.msg,
    })
}

fn pop_next_visible_input_task_with_scheduler(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    workers: &crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    target_kind: crate::chat::turn_worker::ProviderTurnWorkerKind,
    max_concurrent_visible_turns: usize,
) -> Option<DequeuedInputMessage> {
    if !provider_turn_visible_admission(workers, target_kind, max_concurrent_visible_turns).can_start_visible {
        return None;
    }
    if workers.snapshot().iter().any(|worker| !worker.state.is_terminal())
        && next_queued_input_index(backlog)
            .and_then(|index| backlog.get(index))
            .is_some_and(|queued| is_persistence_dependent_command(&queued.msg.content))
    {
        // Provider concurrency may admit another ordinary user turn, but
        // commands that inspect, mutate, switch, export, or terminate the
        // foreground session must observe the preceding turn's durable state.
        // Leave the command at the queue head until the ordered
        // StreamCompleted -> SaveSession acknowledgement finalizes the worker.
        return None;
    }
    pop_next_input_task_with_scheduler(backlog, scheduler)
}

fn is_persistence_dependent_command(input: &str) -> bool {
    let (_priority, content) = input_priority_from_text(input);
    let command = content.trim();
    is_chat_quit_command(command)
        || command == "/apply"
        || command == "/cost"
        || command == "/export"
        || command == "/new"
        || command == "/clear"
        || command == "/resume"
        || command.starts_with("/resume ")
        || command == "/branch"
        || command.starts_with("/branch ")
        || command == "/rewind"
        || command.starts_with("/rewind ")
}

fn requeue_post_route_admission_rejected_input(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    defer_visible_input_pop_once: &mut bool,
    input: DequeuedInputMessage,
) {
    backlog.push_front(QueuedInputMessage {
        priority: input.priority,
        turn_task_id: input.turn_task_id,
        msg: input.msg,
    });
    *defer_visible_input_pop_once = true;
}

fn publish_main_queue_status(
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    scheduler: &crate::chat::turn_scheduler::TurnScheduler,
) {
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::MainQueueStatusUpdated {
            status: scheduler.status().main_queue_status(),
        },
        "chat.main_queue_status_updated",
    );
}

fn provider_worker_status(
    workers: &crate::chat::turn_worker::ProviderTurnWorkerRegistry,
) -> crate::chat::action::ProviderWorkerStatus {
    use crate::chat::action::{ProviderWorkerRowKind, ProviderWorkerRowState, ProviderWorkerStatusRow};

    let mut status = crate::chat::action::ProviderWorkerStatus::default();
    for worker in workers.snapshot() {
        if worker.finalized_payload_ready {
            status.finalized_payloads = status.finalized_payloads.saturating_add(1);
            status.finalized_total_tokens = status
                .finalized_total_tokens
                .saturating_add(worker.finalized_total_tokens.unwrap_or_default());
        }
        let row_state = match worker.state {
            crate::chat::turn_worker::ProviderTurnWorkerState::Running => ProviderWorkerRowState::Running,
            crate::chat::turn_worker::ProviderTurnWorkerState::Cancelling => ProviderWorkerRowState::Cancelling,
            crate::chat::turn_worker::ProviderTurnWorkerState::AwaitingCommit(_) => {
                ProviderWorkerRowState::AwaitingCommit
            }
            crate::chat::turn_worker::ProviderTurnWorkerState::Committed => ProviderWorkerRowState::Committed,
            crate::chat::turn_worker::ProviderTurnWorkerState::Cancelled => ProviderWorkerRowState::Cancelled,
            crate::chat::turn_worker::ProviderTurnWorkerState::Failed => ProviderWorkerRowState::Failed,
        };
        if !worker.state.is_terminal() || worker.finalized_payload_ready {
            let row_kind = match worker.kind {
                crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited => {
                    ProviderWorkerRowKind::ForegroundAwaited
                }
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached => ProviderWorkerRowKind::Detached,
            };
            status.rows.push(ProviderWorkerStatusRow {
                task_id: worker.task_id.get(),
                sequence: worker.sequence,
                kind: row_kind,
                state: row_state,
                started_at_ms: worker.started_at_ms,
                finalized_total_tokens: worker.finalized_total_tokens,
                completion_ready: worker.completion_ready,
                recent_tool_call: None,
            });
        }
        match worker.state {
            crate::chat::turn_worker::ProviderTurnWorkerState::Running => {
                status.running = status.running.saturating_add(1);
                status.oldest_started_at_ms = Some(
                    status
                        .oldest_started_at_ms
                        .map_or(worker.started_at_ms, |current| current.min(worker.started_at_ms)),
                );
            }
            crate::chat::turn_worker::ProviderTurnWorkerState::Cancelling => {
                status.cancelling = status.cancelling.saturating_add(1);
                status.oldest_started_at_ms = Some(
                    status
                        .oldest_started_at_ms
                        .map_or(worker.started_at_ms, |current| current.min(worker.started_at_ms)),
                );
            }
            crate::chat::turn_worker::ProviderTurnWorkerState::AwaitingCommit(_) => {
                status.awaiting_commit = status.awaiting_commit.saturating_add(1);
            }
            crate::chat::turn_worker::ProviderTurnWorkerState::Committed
            | crate::chat::turn_worker::ProviderTurnWorkerState::Cancelled
            | crate::chat::turn_worker::ProviderTurnWorkerState::Failed => {}
        }
    }
    status
}

fn publish_provider_worker_status(
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    workers: &crate::chat::turn_worker::ProviderTurnWorkerRegistry,
) {
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ProviderWorkerStatusUpdated {
            status: provider_worker_status(workers),
        },
        "chat.provider_worker_status_updated",
    );
}

fn provider_turn_visible_admission(
    workers: &crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    target_kind: crate::chat::turn_worker::ProviderTurnWorkerKind,
    max_concurrent_visible_turns: usize,
) -> ProviderTurnVisibleAdmission {
    let effective_max_visible_turns = max_concurrent_visible_turns.max(1);
    let active_rows: Vec<_> = workers
        .snapshot()
        .into_iter()
        .filter(|worker| {
            matches!(
                worker.state,
                crate::chat::turn_worker::ProviderTurnWorkerState::Running
                    | crate::chat::turn_worker::ProviderTurnWorkerState::Cancelling
                    | crate::chat::turn_worker::ProviderTurnWorkerState::AwaitingCommit(_)
            )
        })
        .collect();
    let active_workers = active_rows.len();
    let foreground_active = active_rows
        .iter()
        .filter(|worker| worker.kind == crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited)
        .count();
    let detached_active = active_rows
        .iter()
        .filter(|worker| worker.kind == crate::chat::turn_worker::ProviderTurnWorkerKind::Detached)
        .count();
    let can_start_visible = match target_kind {
        crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited => active_workers == 0,
        crate::chat::turn_worker::ProviderTurnWorkerKind::Detached => {
            foreground_active == 0 && detached_active < effective_max_visible_turns
        }
    };
    ProviderTurnVisibleAdmission {
        active_workers,
        foreground_active,
        detached_active,
        effective_max_visible_turns,
        can_start_visible,
    }
}

fn drain_provider_turn_lifecycle_events(
    rx: &mut mpsc::Receiver<dispatcher::ProviderTurnLifecycleEvent>,
    events_open: &mut bool,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
) -> bool {
    if !*events_open {
        return false;
    }

    let mut drained = false;
    loop {
        match rx.try_recv() {
            Ok(event) => {
                drained = true;
                record_provider_turn_lifecycle_event(workers, event);
            }
            Err(mpsc::error::TryRecvError::Empty) => return drained,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                *events_open = false;
                return drained;
            }
        }
    }
}

fn record_provider_turn_lifecycle_event(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    event: dispatcher::ProviderTurnLifecycleEvent,
) {
    let (task_id, lease_id, kind, result) = match event {
        dispatcher::ProviderTurnLifecycleEvent::HandleAttached {
            task_id,
            lease_id,
            abort_handle,
        } => {
            let result = workers.attach_execution_handle(task_id, lease_id, abort_handle);
            (task_id, lease_id, "handle_attached", result)
        }
        dispatcher::ProviderTurnLifecycleEvent::Started { task_id, lease_id } => {
            let result = workers.record_execution_started(task_id, lease_id);
            (task_id, lease_id, "started", result)
        }
        dispatcher::ProviderTurnLifecycleEvent::Exited { task_id, lease_id } => {
            let result = workers.record_execution_exited(task_id, lease_id);
            (task_id, lease_id, "exited", result)
        }
    };
    if let Err(error) = result {
        tracing::warn!(
            task_id = task_id.get(),
            lease_id,
            kind,
            error = ?error,
            "ProviderTurnWorkerRegistry provider execution lifecycle record failed"
        );
    }
}

fn record_provider_turn_completion_ready(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    task_id: crate::chat::turn_scheduler::TurnTaskId,
) {
    if let Err(error) = workers.record_completion_ready(task_id) {
        tracing::warn!(
            task_id = task_id.get(),
            error = ?error,
            "ProviderTurnWorkerRegistry provider completion-ready gate failed"
        );
    }
}

fn route_provider_completion_event(
    lifecycle_rx: &mut mpsc::Receiver<dispatcher::ProviderTurnLifecycleEvent>,
    lifecycle_events_open: &mut bool,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    pending: &mut std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, ProviderTurnCompletionEvent>,
    current_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    completion: ProviderTurnCompletionEvent,
) -> Option<ProviderTurnCompletionEvent> {
    if drain_provider_turn_lifecycle_events(lifecycle_rx, lifecycle_events_open, workers) {
        tracing::trace!(
            task_id = completion.task_id.get(),
            "drained provider lifecycle events before routing completion"
        );
    }
    let task_id = completion.task_id;
    record_provider_turn_completion_ready(workers, task_id);
    if Some(task_id) == current_task_id {
        return Some(completion);
    }
    if pending.insert(task_id, completion).is_some() {
        tracing::warn!(
            task_id = task_id.get(),
            "replaced duplicate pending provider completion event"
        );
    }
    None
}

fn route_provider_completion_event_and_publish(
    lifecycle_rx: &mut mpsc::Receiver<dispatcher::ProviderTurnLifecycleEvent>,
    lifecycle_events_open: &mut bool,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    pending: &mut std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, ProviderTurnCompletionEvent>,
    current_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    completion: ProviderTurnCompletionEvent,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
) -> Option<ProviderTurnCompletionEvent> {
    let route = route_provider_completion_event(
        lifecycle_rx,
        lifecycle_events_open,
        workers,
        pending,
        current_task_id,
        completion,
    );
    publish_provider_worker_status(chat_dispatcher, workers);
    route
}

fn record_provider_turn_completion_context(
    contexts: &mut std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, ProviderTurnCompletionContext>,
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    history_len_before_assistant: usize,
) {
    if let Some(task_id) = task_id
        && contexts
            .insert(
                task_id,
                ProviderTurnCompletionContext {
                    history_len_before_assistant,
                },
            )
            .is_some()
    {
        tracing::warn!(
            task_id = task_id.get(),
            "replaced duplicate provider turn completion context"
        );
    }
}

fn take_provider_turn_completion_history_len(
    contexts: &mut std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, ProviderTurnCompletionContext>,
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    fallback_history_len: usize,
) -> usize {
    task_id
        .and_then(|task_id| contexts.remove(&task_id))
        .map_or(fallback_history_len, |context| context.history_len_before_assistant)
}

fn provider_turn_finalized_payload_from_usage(
    history_commit_len: usize,
    final_text_chars: usize,
    recorded_response_chars: usize,
    usage: &crate::llm::route_decision::TokenUsage,
) -> crate::chat::turn_worker::ProviderTurnFinalizedPayload {
    let prompt_tokens = usage.prompt_tokens.map_or(0, u64::from);
    let completion_tokens = usage.completion_tokens.map_or(0, u64::from);
    let total_tokens = usage
        .total_tokens
        .map_or_else(|| prompt_tokens.saturating_add(completion_tokens), u64::from);
    crate::chat::turn_worker::ProviderTurnFinalizedPayload {
        history_commit_len,
        final_text_chars,
        recorded_response_chars,
        total_tokens,
        prompt_tokens,
        completion_tokens,
    }
}

fn record_provider_turn_finalized_payload(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    payload: crate::chat::turn_worker::ProviderTurnFinalizedPayload,
) -> bool {
    let Some(id) = id else {
        tracing::warn!("ProviderTurnWorkerRegistry missing provider task for finalized payload");
        return false;
    };
    if let Err(error) = workers.record_finalized_payload(id, payload) {
        tracing::warn!(
            task_id = id.get(),
            error = ?error,
            "ProviderTurnWorkerRegistry provider finalized payload record failed"
        );
        return false;
    }
    true
}

fn gate_completed_provider_turn_finalization(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    history_commit_len: usize,
    final_text_chars: usize,
    recorded_response_chars: usize,
    usage: &crate::llm::route_decision::TokenUsage,
    summary: &'static str,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    let finalized_payload = provider_turn_finalized_payload_from_usage(
        history_commit_len,
        final_text_chars,
        recorded_response_chars,
        usage,
    );
    if !record_provider_turn_finalized_payload(workers, id, finalized_payload) {
        return Vec::new();
    }
    mark_provider_turn_completed(scheduler, coordinator, workers, id, history_commit_len, summary)
}

fn gate_failed_provider_turn_finalization(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    history_commit_len: usize,
    summary: impl Into<String>,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    mark_provider_turn_failed(scheduler, coordinator, workers, id, history_commit_len, summary)
}

fn gate_cancelled_provider_turn_finalization(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    summary: &'static str,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    mark_provider_turn_cancelled(scheduler, coordinator, workers, id, summary)
}

#[allow(clippy::option_if_let_else)]
#[cfg(feature = "terminal-tui")]
fn resolve_provider_turn_completion(
    turn_signal: &dispatcher::TurnCompletionSignal,
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    completion_event: Option<ProviderTurnCompletionEvent>,
) -> ResolvedProviderTurnCompletion {
    let outcome = if let Some(completion) = completion_event.as_ref() {
        completion.outcome.clone().or_else(|| turn_signal.consume_outcome())
    } else if let Some(id) = task_id {
        turn_signal
            .consume_turn_outcome(id)
            .or_else(|| turn_signal.consume_outcome())
    } else {
        turn_signal.consume_outcome()
    };
    let usage = if let Some(completion) = completion_event {
        if completion.usage.has_any_tokens() {
            completion.usage
        } else {
            turn_signal.consume_usage()
        }
    } else if let Some(id) = task_id {
        let keyed_usage = turn_signal.consume_turn_usage(id);
        if keyed_usage.has_any_tokens() {
            keyed_usage
        } else {
            turn_signal.consume_usage()
        }
    } else {
        turn_signal.consume_usage()
    };
    if let Some(id) = task_id {
        turn_signal.unregister_turn(id);
    }
    ResolvedProviderTurnCompletion { outcome, usage }
}

#[cfg(feature = "terminal-tui")]
fn provider_turn_terminal_plan_from_completion(
    resolved: ResolvedProviderTurnCompletion,
    history_len: usize,
    tools_registry: &[Box<dyn Tool>],
) -> ProviderTurnTerminalPlan {
    match resolved.outcome {
        Some(dispatcher::TurnOutcomeKind::Completed { final_text, reasoning }) => {
            let mut usage = resolved.usage;
            if !usage.has_any_tokens() {
                let accumulator = crate::llm::route_decision::ProviderUsageAccumulator::new();
                usage = accumulator.finish_or_estimate_completion_chars(final_text.chars().count());
            }
            if crate::agent::response_guard::is_empty_assistant_response(&final_text, false) {
                ProviderTurnTerminalPlan::Completed {
                    final_text,
                    reasoning: String::new(),
                    recorded_response: String::new(),
                    empty_response: true,
                    usage,
                    history_commit_len: history_len,
                    final_text_chars: 0,
                    recorded_response_chars: 0,
                    summary: "redux driver completed with empty response",
                }
            } else {
                let recorded_response = sanitize_channel_response(&final_text, tools_registry);
                let final_text_chars = final_text.chars().count();
                let recorded_response_chars = recorded_response.chars().count();
                ProviderTurnTerminalPlan::Completed {
                    final_text,
                    reasoning,
                    final_text_chars,
                    recorded_response_chars,
                    recorded_response,
                    empty_response: false,
                    usage,
                    history_commit_len: history_len.saturating_add(1),
                    summary: "redux driver completed",
                }
            }
        }
        Some(dispatcher::TurnOutcomeKind::Failed { err, retryable: _ }) => ProviderTurnTerminalPlan::Failed {
            summary: format!("redux driver failed: {err}"),
            err,
            history_commit_len: history_len,
        },
        Some(dispatcher::TurnOutcomeKind::Cancelled) | None => ProviderTurnTerminalPlan::Cancelled {
            summary: "redux driver cancelled",
        },
    }
}

#[cfg(feature = "terminal-tui")]
fn gate_provider_turn_terminal_finalization(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    gate: ProviderTurnTerminalGate<'_>,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    match gate {
        ProviderTurnTerminalGate::Completed {
            history_commit_len,
            final_text_chars,
            recorded_response_chars,
            usage,
            summary,
        } => gate_completed_provider_turn_finalization(
            scheduler,
            coordinator,
            workers,
            id,
            history_commit_len,
            final_text_chars,
            recorded_response_chars,
            usage,
            summary,
        ),
        ProviderTurnTerminalGate::Failed {
            history_commit_len,
            summary,
        } => gate_failed_provider_turn_finalization(scheduler, coordinator, workers, id, history_commit_len, summary),
        ProviderTurnTerminalGate::Cancelled { summary } => {
            gate_cancelled_provider_turn_finalization(scheduler, coordinator, workers, id, summary)
        }
    }
}

#[cfg(feature = "terminal-tui")]
fn finalize_provider_turn_from_event(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    event: ProviderTurnFinalizerEvent,
) -> Vec<ProviderTurnFinalizerResult> {
    let task_id = event.task_id;
    let decisions = match event.plan {
        ProviderTurnTerminalPlan::Completed {
            usage,
            history_commit_len,
            final_text_chars,
            recorded_response_chars,
            summary,
            ..
        } => gate_provider_turn_terminal_finalization(
            scheduler,
            coordinator,
            workers,
            task_id,
            ProviderTurnTerminalGate::Completed {
                history_commit_len,
                final_text_chars,
                recorded_response_chars,
                usage: &usage,
                summary,
            },
        ),
        ProviderTurnTerminalPlan::Failed {
            history_commit_len,
            summary,
            ..
        } => gate_provider_turn_terminal_finalization(
            scheduler,
            coordinator,
            workers,
            task_id,
            ProviderTurnTerminalGate::Failed {
                history_commit_len,
                summary,
            },
        ),
        ProviderTurnTerminalPlan::Cancelled { summary } => gate_provider_turn_terminal_finalization(
            scheduler,
            coordinator,
            workers,
            task_id,
            ProviderTurnTerminalGate::Cancelled { summary },
        ),
    };
    if decisions.is_empty() {
        return vec![ProviderTurnFinalizerResult {
            task_id,
            terminal_status: "unknown",
            finalized: false,
        }];
    }
    decisions
        .iter()
        .map(provider_turn_finalizer_result_from_commit_decision)
        .collect()
}

#[cfg(feature = "terminal-tui")]
const fn provider_turn_finalizer_result_from_commit_decision(
    decision: &crate::chat::history_commit::HistoryCommitDecision,
) -> ProviderTurnFinalizerResult {
    match decision {
        crate::chat::history_commit::HistoryCommitDecision::Commit { task_id, .. } => ProviderTurnFinalizerResult {
            task_id: Some(*task_id),
            terminal_status: "completed",
            finalized: true,
        },
        crate::chat::history_commit::HistoryCommitDecision::Skip { task_id, status, .. } => {
            ProviderTurnFinalizerResult {
                task_id: Some(*task_id),
                terminal_status: match status {
                    crate::chat::history_commit::HistoryCommitStatus::Completed => "completed",
                    crate::chat::history_commit::HistoryCommitStatus::Cancelled => "cancelled",
                    crate::chat::history_commit::HistoryCommitStatus::Failed => "failed",
                },
                finalized: true,
            }
        }
    }
}

#[cfg(feature = "terminal-tui")]
fn enqueue_provider_turn_finalizer_event(
    queue: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    plan: ProviderTurnTerminalPlan,
) {
    queue.push_back(ProviderTurnFinalizerEvent { task_id, plan });
}

fn dispatch_ordered_provider_turn_commit(
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    task_id: crate::chat::turn_scheduler::TurnTaskId,
    draft_id: &str,
    user_input: &str,
    final_text: &str,
    reasoning: &str,
    empty_response: bool,
    record_user: bool,
) -> std::result::Result<(), String> {
    if record_user {
        let result = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::RecordUserTurn(user_input.to_string()),
            "chat.ordered_record_user_turn",
        );
        if !matches!(result, dispatcher::DispatchResult::Sent) {
            return Err(format!("ordered history dispatch failed: {result:?}"));
        }
    }
    if !empty_response {
        let result = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::RecordAssistantTurn {
                task_id: Some(task_id),
                content: final_text.to_string(),
            },
            "chat.ordered_record_assistant_turn",
        );
        if !matches!(result, dispatcher::DispatchResult::Sent) {
            return Err(format!("ordered history dispatch failed: {result:?}"));
        }
    }
    let result = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::StreamCompleted {
            draft_id: draft_id.to_string(),
            final_text: final_text.to_string(),
            reasoning: reasoning.to_string(),
        },
        "chat.ordered_stream_completed",
    );
    if !matches!(result, dispatcher::DispatchResult::Sent) {
        return Err(format!("ordered history dispatch failed: {result:?}"));
    }
    Ok(())
}

async fn dispatch_ordered_provider_turn_commit_and_wait(
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    turn_signal: &dispatcher::TurnCompletionSignal,
    task_id: crate::chat::turn_scheduler::TurnTaskId,
    draft_id: &str,
    user_input: &str,
    final_text: &str,
    reasoning: &str,
    empty_response: bool,
    record_user: bool,
) -> std::result::Result<(), String> {
    // The first task-scoped notification means the provider produced its
    // terminal payload. The ordered StreamCompleted below is a distinct phase:
    // the dispatcher only emits its second notification after every reducer
    // effect, including SaveSession, has completed. Arm the waiter before
    // dispatch so the latch-less Notify cannot be missed.
    // Provider completion consumption unregisters the first-phase slot. Open
    // a fresh slot for the same task/draft so the ordered reducer commit has
    // its own acknowledgement lifecycle.
    turn_signal.register_turn(task_id, draft_id.to_string());
    let Some(persistence_committed) = turn_signal.notified_for(task_id) else {
        tracing::error!(task_id = task_id.get(), "missing task-scoped persistence waiter");
        turn_signal.fail_history_commit("missing task-scoped persistence waiter");
        return Err("missing task-scoped persistence waiter".to_string());
    };
    if let Err(error) = dispatch_ordered_provider_turn_commit(
        chat_dispatcher,
        task_id,
        draft_id,
        user_input,
        final_text,
        reasoning,
        empty_response,
        record_user,
    ) {
        turn_signal.unregister_turn(task_id);
        turn_signal.fail_history_commit(error.clone());
        return Err(error);
    }
    let result = match tokio::time::timeout(Duration::from_secs(30), persistence_committed).await {
        Ok(()) => {
            // Consume the second terminal outcome so a later waiter cannot
            // mistake this persistence acknowledgement for a provider result.
            let outcome = turn_signal.consume_turn_outcome(task_id);
            turn_signal.unregister_turn(task_id);
            match outcome {
                Some(dispatcher::TurnOutcomeKind::Completed { .. }) => Ok(()),
                Some(dispatcher::TurnOutcomeKind::Failed { err, .. }) => Err(err),
                Some(dispatcher::TurnOutcomeKind::Cancelled) => {
                    Err("ordered session persistence was cancelled".to_string())
                }
                None => Err("ordered session persistence acknowledged without an outcome".to_string()),
            }
        }
        Err(_) => {
            tracing::error!(
                task_id = task_id.get(),
                "timed out waiting for ordered session persistence"
            );
            turn_signal.unregister_turn(task_id);
            Err("timed out waiting for ordered session persistence".to_string())
        }
    };
    if let Err(error) = &result {
        turn_signal.fail_history_commit(error.clone());
    }
    result
}

#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
async fn commit_completed_provider_turn(
    pending_commit: PendingOrderedProviderTurnCommit,
    history: &mut Vec<ChatMessage>,
    turn_scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    provider_turn_workers: &crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    chat_mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    terminal: &Arc<TerminalChannel>,
    memory_fabric: &MemoryFabric,
    chat_session_key: &str,
    chat_session: &mut session::ChatSession,
    config: &Config,
    sessions_redraw_handle: Option<&mpsc::Sender<()>>,
    redraw_tx_for_main: Option<&mpsc::Sender<()>>,
    turn_signal: &dispatcher::TurnCompletionSignal,
) -> bool {
    let mut pending = pending_commit.context;
    let ProviderTurnTerminalPlan::Completed {
        final_text,
        reasoning,
        recorded_response,
        empty_response,
        usage: redux_tokens_used,
        history_commit_len,
        ..
    } = pending_commit.terminal_plan
    else {
        tracing::warn!(
            task_id = pending.task_id.get(),
            "ordered provider turn commit received non-completed terminal plan"
        );
        return false;
    };

    let provider_turn_task_id = Some(pending.task_id);
    // Provider trace capture seals execution authority before this ordered
    // finalizer runs. Re-reading the live disposition here would therefore
    // reject every successful provider-backed turn. Use the immutable
    // pre-execution classification carried with the turn instead.
    let fixed_reply = pending.fixed_reply.take();
    let (provider_outcome, terminal_usage) = if let Some(reply) = fixed_reply {
        if empty_response || final_text != reply || recorded_response != reply || !reasoning.is_empty() {
            tracing::error!("Chat fixed reply differs from its decision");
            return false;
        }
        let Some(binding) = pending.turn_binding.as_ref() else {
            return false;
        };
        match binding
            .close(
                memory_fabric,
                history_commit_len,
                pending.provider_started_at,
                &pending.cancellation,
            )
            .await
        {
            Ok(receipt) => (None, receipt.usage_settlement),
            Err(error) => {
                tracing::error!(error = %error, "Chat fixed terminal commit failed");
                return false;
            }
        }
    } else {
        let provider_outcome = if let Some(binding) = pending.turn_binding.as_ref() {
            match binding.provider_outcome(&pending.route_decision, pending.provider_started_at) {
                Ok(outcome) => outcome,
                Err(error) => {
                    tracing::error!(error = %error, "Chat completion has no authenticated provider trace");
                    return false;
                }
            }
        } else {
            ProviderExecutionOutcome::success_for_decision_with_usage(
                &pending.route_decision,
                pending.provider_started_at,
                redux_tokens_used.clone(),
            )
        };
        let terminal_usage = match finalize_chat_success_turn(
            pending.turn_binding.as_deref(),
            memory_fabric,
            chat_session_key,
            &pending.turn_run_id,
            &pending.provider_name,
            &pending.model_name,
            pending.route_scope.clone(),
            provider_outcome.clone(),
            (!empty_response).then(|| recorded_response.clone()),
            history_commit_len,
            if empty_response {
                "redux driver completed with empty response"
            } else {
                "redux driver completed"
            },
            &config.cost,
            &config.workspace_dir,
        )
        .await
        {
            Ok(receipt) => receipt.usage_settlement,
            Err(error) => {
                tracing::error!(error = %error, "Chat terminal persistence is unconfirmed");
                return false;
            }
        };
        (Some(provider_outcome), terminal_usage)
    };
    let finished_at = provider_outcome
        .as_ref()
        .map_or_else(chrono::Utc::now, |outcome| outcome.finished_at);
    if empty_response {
        if let Err(e) = terminal.cancel_draft("user", &pending.draft_id).await {
            tracing::debug!(error = %e, "Redux driver: cancel empty-response draft failed");
        }
        let persistence_result = dispatch_ordered_provider_turn_commit_and_wait(
            chat_dispatcher,
            turn_signal,
            pending.task_id,
            &pending.draft_id,
            &pending.user_input,
            &final_text,
            &reasoning,
            empty_response,
            true,
        )
        .await;
        if let Err(error) = persistence_result {
            surface_session_message(
                chat_dispatcher,
                sessions_redraw_handle,
                &format!("Session persistence failed; dependent commands remain blocked: {error}"),
            );
            return false;
        }
        if let Some(guard) = pending.assignment.as_mut() {
            guard.failed("The turn finished without reply text.");
        }
        history.push(pending.history_user_message.clone());
        chat_session.add_user_turn(&pending.user_input);
        if let Some(record) = terminal_usage.and_then(|record| chat_session.record_usage_settlement(record)) {
            record_provider_turn_usage(turn_scheduler, provider_turn_task_id, &record);
            chat_mirror.lock().token_usage_summary = chat_session.token_usage_summary();
            let _ = chat_dispatcher.dispatch_or_log(
                crate::chat::action::Action::ProviderUsageRecorded {
                    task_id: provider_turn_task_id,
                    usage_kind: crate::chat::action::ProviderUsageRecordKind::FinalAggregate,
                    record,
                },
                "chat.provider_usage_recorded_empty_response",
            );
            if let Some(tx) = redraw_tx_for_main {
                let _ = tx.try_send(());
            }
        }
        surface_turn_elapsed_message(
            chat_dispatcher,
            sessions_redraw_handle,
            "completed",
            pending.provider_started_at,
            finished_at,
        );
        publish_main_queue_status(chat_dispatcher, turn_scheduler);
        publish_provider_worker_status(chat_dispatcher, provider_turn_workers);
        return true;
    }

    let persistence_result = dispatch_ordered_provider_turn_commit_and_wait(
        chat_dispatcher,
        turn_signal,
        pending.task_id,
        &pending.draft_id,
        &pending.user_input,
        &final_text,
        &reasoning,
        empty_response,
        true,
    )
    .await;
    if let Err(error) = persistence_result {
        surface_session_message(
            chat_dispatcher,
            sessions_redraw_handle,
            &format!("Session persistence failed; dependent commands remain blocked: {error}"),
        );
        return false;
    }
    history.push(pending.history_user_message.clone());
    history.push(ChatMessage::assistant(final_text.clone()));
    if let Err(e) = terminal.finalize_draft("user", &pending.draft_id, &final_text).await {
        tracing::warn!(error = %e, "Redux driver: finalize_draft failed");
    }
    if let Some(provider_outcome) = provider_outcome.as_ref() {
        let attempts_count = u8::try_from(provider_outcome.attempts.len()).unwrap_or(u8::MAX);
        crate::runtime::control_ladder::append_provider_outcome_trace(
            std::path::Path::new(&config.workspace_dir),
            &provider_outcome.decision_id,
            &provider_outcome.final_provider,
            &provider_outcome.final_model,
            attempts_count,
            "success",
        );
    }
    chat_session.add_user_turn(&pending.user_input);
    chat_session.add_assistant_turn(&recorded_response, Vec::new());
    if let Some(guard) = pending.assignment.as_mut() {
        guard.completed(&recorded_response);
    }
    if let Some(record) = terminal_usage.and_then(|record| chat_session.record_usage_settlement(record)) {
        record_provider_turn_usage(turn_scheduler, provider_turn_task_id, &record);
        chat_mirror.lock().token_usage_summary = chat_session.token_usage_summary();
        let _ = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::ProviderUsageRecorded {
                task_id: provider_turn_task_id,
                usage_kind: crate::chat::action::ProviderUsageRecordKind::FinalAggregate,
                record,
            },
            "chat.provider_usage_recorded",
        );
        if let Some(tx) = redraw_tx_for_main {
            let _ = tx.try_send(());
        }
    }
    surface_turn_elapsed_message(
        chat_dispatcher,
        sessions_redraw_handle,
        "completed",
        pending.provider_started_at,
        finished_at,
    );
    publish_main_queue_status(chat_dispatcher, turn_scheduler);
    publish_provider_worker_status(chat_dispatcher, provider_turn_workers);
    true
}

#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
async fn apply_ready_ordered_provider_turn_commits(
    results: Vec<ProviderTurnFinalizerResult>,
    pending_ordered_provider_turn_commits: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        PendingOrderedProviderTurnCommit,
    >,
    history: &mut Vec<ChatMessage>,
    turn_scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    provider_turn_workers: &crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    chat_mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    terminal: &Arc<TerminalChannel>,
    memory_fabric: &MemoryFabric,
    chat_session_key: &str,
    chat_session: &mut session::ChatSession,
    config: &Config,
    sessions_redraw_handle: Option<&mpsc::Sender<()>>,
    redraw_tx_for_main: Option<&mpsc::Sender<()>>,
    turn_signal: &dispatcher::TurnCompletionSignal,
    plain_mode: bool,
    plain_mode_turn_failed: &mut bool,
) -> bool {
    let mut applied = false;
    for result in results {
        if turn_signal.ensure_history_commits_healthy().is_err() {
            break;
        }
        if !result.finalized || result.terminal_status != "completed" {
            continue;
        }
        let Some(task_id) = result.task_id else {
            continue;
        };
        let Some(pending_commit) = pending_ordered_provider_turn_commits.remove(&task_id) else {
            tracing::debug!(
                task_id = task_id.get(),
                "ordered provider turn commit ready before local payload was registered"
            );
            continue;
        };
        let committed = commit_completed_provider_turn(
            pending_commit,
            history,
            turn_scheduler,
            provider_turn_workers,
            chat_mirror,
            chat_dispatcher,
            terminal,
            memory_fabric,
            chat_session_key,
            chat_session,
            config,
            sessions_redraw_handle,
            redraw_tx_for_main,
            turn_signal,
        )
        .await;
        if !committed {
            turn_signal.fail_history_commit(format!("ordered turn {} was not committed", task_id.get()));
            if plain_mode {
                *plain_mode_turn_failed = true;
            }
            break;
        }
        applied = true;
    }
    applied
}

#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
async fn finalize_per_turn_context(
    mut pending: PerTurnContext,
    completion_event: Option<ProviderTurnCompletionEvent>,
    pending_ordered_provider_turn_commits: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        PendingOrderedProviderTurnCommit,
    >,
    turn_signal: &dispatcher::TurnCompletionSignal,
    provider_turn_completion_contexts: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        ProviderTurnCompletionContext,
    >,
    history: &mut Vec<ChatMessage>,
    tools_registry: &[Box<dyn Tool>],
    provider_turn_finalizer_events: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
    turn_scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    history_commit_coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    provider_turn_workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    chat_mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    terminal: &Arc<TerminalChannel>,
    memory_fabric: &MemoryFabric,
    chat_session_key: &str,
    chat_session: &mut session::ChatSession,
    config: &Config,
    sessions_redraw_handle: Option<&mpsc::Sender<()>>,
    redraw_tx_for_main: Option<&mpsc::Sender<()>>,
    plain_mode: bool,
    plain_mode_turn_failed: &mut bool,
) {
    let provider_turn_task_id = Some(pending.task_id);
    let resolved_completion = resolve_provider_turn_completion(turn_signal, provider_turn_task_id, completion_event);
    let completion_history_len = take_provider_turn_completion_history_len(
        provider_turn_completion_contexts,
        provider_turn_task_id,
        history.len(),
    );
    let terminal_plan =
        provider_turn_terminal_plan_from_completion(resolved_completion, completion_history_len, tools_registry);
    tracing::debug!(
        task_id = pending.task_id.get(),
        draft_id = %pending.draft_id,
        turn_run_id = %pending.turn_run_id,
        provider_name = %crate::providers::redact_endpoint_userinfo(&pending.provider_name),
        model_name = %pending.model_name,
        history_len_before_user_turn = pending.history_len_before_user_turn,
        "finalizing provider turn context"
    );
    enqueue_provider_turn_finalizer_event(
        provider_turn_finalizer_events,
        provider_turn_task_id,
        terminal_plan.clone(),
    );
    let finalizer_results = drain_provider_turn_finalizer_events_and_publish(
        turn_scheduler,
        history_commit_coordinator,
        provider_turn_workers,
        provider_turn_finalizer_events,
        chat_dispatcher,
    );

    drop(pending.delta_tx.take());
    drop(pending.tool_event_tx.take());
    if let Some(handle) = pending.draft_updater.take() {
        let _ = handle.await;
    }
    if let Some(handle) = pending.tool_event_forwarder.take() {
        let _ = handle.await;
    }

    match terminal_plan {
        plan @ ProviderTurnTerminalPlan::Completed { .. } => {
            let task_id = pending.task_id;
            if pending_ordered_provider_turn_commits
                .insert(
                    task_id,
                    PendingOrderedProviderTurnCommit {
                        context: pending,
                        terminal_plan: plan,
                    },
                )
                .is_some()
            {
                tracing::warn!(
                    task_id = task_id.get(),
                    "replaced pending ordered provider turn commit payload"
                );
            }
        }
        ProviderTurnTerminalPlan::Failed { err, .. } => {
            if let Some(guard) = pending.assignment.as_mut() {
                guard.failed(&format!("The turn this chat ran for the assignment failed: {err}"));
            }
            if let Err(error) = finalize_chat_non_success_turn(
                pending.turn_binding.as_deref(),
                memory_fabric,
                &pending.turn_run_id,
                pending.route_scope.clone(),
                crate::agent::terminal::TurnTerminalStatus::Failed,
                None,
                pending.provider_started_at,
                &format!("redux driver failed: {err}"),
                &config.cost,
                &config.workspace_dir,
            )
            .await
            {
                if pending
                    .turn_binding
                    .as_ref()
                    .is_some_and(|binding| binding.is_decided())
                {
                    *plain_mode_turn_failed = true;
                }
                tracing::warn!(error = %error, "Failed to commit ordered failed Chat terminal event");
            }
            let interactive_tui_active = redraw_tx_for_main.is_some();
            if !interactive_tui_active {
                let _ = terminal.cancel_draft("user", &pending.draft_id).await;
                eprintln!("\nError: {err}\n");
            }
            if plain_mode {
                *plain_mode_turn_failed = true;
            }
            surface_turn_elapsed_message(
                chat_dispatcher,
                sessions_redraw_handle,
                "failed",
                pending.provider_started_at,
                chrono::Utc::now(),
            );
            publish_main_queue_status(chat_dispatcher, turn_scheduler);
            publish_provider_worker_status(chat_dispatcher, provider_turn_workers);
        }
        ProviderTurnTerminalPlan::Cancelled { .. } => {
            // Deliberately not marked: the guard's own default already says the
            // turn ended before it answered, which is exactly what happened —
            // and `cancelled` is the daemon's verdict, not a client's to claim.
            if let Err(error) = finalize_chat_non_success_turn(
                pending.turn_binding.as_deref(),
                memory_fabric,
                &pending.turn_run_id,
                pending.route_scope.clone(),
                crate::agent::terminal::TurnTerminalStatus::Cancelled,
                None,
                pending.provider_started_at,
                "redux driver cancelled",
                &config.cost,
                &config.workspace_dir,
            )
            .await
            {
                if pending
                    .turn_binding
                    .as_ref()
                    .is_some_and(|binding| binding.is_decided())
                {
                    *plain_mode_turn_failed = true;
                }
                tracing::warn!(error = %error, "Failed to commit ordered cancelled Chat terminal event");
            }
            let _ = terminal.cancel_draft("user", &pending.draft_id).await;
            rollback_cancelled_turn_history(history, pending.history_len_before_user_turn);
            publish_main_queue_status(chat_dispatcher, turn_scheduler);
            publish_provider_worker_status(chat_dispatcher, provider_turn_workers);
        }
    }
    let _ = apply_ready_ordered_provider_turn_commits(
        finalizer_results,
        pending_ordered_provider_turn_commits,
        history,
        turn_scheduler,
        provider_turn_workers,
        chat_mirror,
        chat_dispatcher,
        terminal,
        memory_fabric,
        chat_session_key,
        chat_session,
        config,
        sessions_redraw_handle,
        redraw_tx_for_main,
        turn_signal,
        plain_mode,
        plain_mode_turn_failed,
    )
    .await;
}

#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
async fn finalize_ready_per_turn_contexts(
    per_turn_contexts: &mut std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, PerTurnContext>,
    pending_ordered_provider_turn_commits: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        PendingOrderedProviderTurnCommit,
    >,
    pending_provider_completion_events: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        ProviderTurnCompletionEvent,
    >,
    turn_signal: &dispatcher::TurnCompletionSignal,
    provider_turn_completion_contexts: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        ProviderTurnCompletionContext,
    >,
    history: &mut Vec<ChatMessage>,
    tools_registry: &[Box<dyn Tool>],
    provider_turn_finalizer_events: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
    turn_scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    history_commit_coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    provider_turn_workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    chat_mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    terminal: &Arc<TerminalChannel>,
    memory_fabric: &MemoryFabric,
    chat_session_key: &str,
    chat_session: &mut session::ChatSession,
    config: &Config,
    sessions_redraw_handle: Option<&mpsc::Sender<()>>,
    redraw_tx_for_main: Option<&mpsc::Sender<()>>,
    plain_mode: bool,
    plain_mode_turn_failed: &mut bool,
) -> bool {
    let ready_task_ids: Vec<_> = per_turn_contexts
        .keys()
        .copied()
        .filter(|task_id| pending_provider_completion_events.contains_key(task_id))
        .collect();
    if ready_task_ids.is_empty() {
        return false;
    }
    for task_id in ready_task_ids {
        if turn_signal.ensure_history_commits_healthy().is_err() {
            break;
        }
        let Some(pending) = per_turn_contexts.remove(&task_id) else {
            continue;
        };
        let completion = pending_provider_completion_events.remove(&task_id);
        finalize_per_turn_context(
            pending,
            completion,
            pending_ordered_provider_turn_commits,
            turn_signal,
            provider_turn_completion_contexts,
            history,
            tools_registry,
            provider_turn_finalizer_events,
            turn_scheduler,
            history_commit_coordinator,
            provider_turn_workers,
            chat_mirror,
            chat_dispatcher,
            terminal,
            memory_fabric,
            chat_session_key,
            chat_session,
            config,
            sessions_redraw_handle,
            redraw_tx_for_main,
            plain_mode,
            plain_mode_turn_failed,
        )
        .await;
    }
    true
}

#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
async fn finalize_all_per_turn_contexts_as_cancelled(
    per_turn_contexts: &mut std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, PerTurnContext>,
    pending_ordered_provider_turn_commits: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        PendingOrderedProviderTurnCommit,
    >,
    turn_signal: &dispatcher::TurnCompletionSignal,
    provider_turn_completion_contexts: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        ProviderTurnCompletionContext,
    >,
    history: &mut Vec<ChatMessage>,
    tools_registry: &[Box<dyn Tool>],
    provider_turn_finalizer_events: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
    turn_scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    history_commit_coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    provider_turn_workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    chat_mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    terminal: &Arc<TerminalChannel>,
    memory_fabric: &MemoryFabric,
    chat_session_key: &str,
    chat_session: &mut session::ChatSession,
    config: &Config,
    sessions_redraw_handle: Option<&mpsc::Sender<()>>,
    redraw_tx_for_main: Option<&mpsc::Sender<()>>,
    plain_mode: bool,
    plain_mode_turn_failed: &mut bool,
) {
    let pending_turns: Vec<_> = per_turn_contexts.drain().map(|(_, pending)| pending).collect();
    for pending in pending_turns {
        finalize_per_turn_context(
            pending,
            None,
            pending_ordered_provider_turn_commits,
            turn_signal,
            provider_turn_completion_contexts,
            history,
            tools_registry,
            provider_turn_finalizer_events,
            turn_scheduler,
            history_commit_coordinator,
            provider_turn_workers,
            chat_mirror,
            chat_dispatcher,
            terminal,
            memory_fabric,
            chat_session_key,
            chat_session,
            config,
            sessions_redraw_handle,
            redraw_tx_for_main,
            plain_mode,
            plain_mode_turn_failed,
        )
        .await;
    }
}

#[cfg(feature = "terminal-tui")]
fn drain_provider_turn_finalizer_events(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    queue: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
) -> Vec<ProviderTurnFinalizerResult> {
    let mut results = Vec::new();
    while let Some(event) = queue.pop_front() {
        results.extend(finalize_provider_turn_from_event(
            scheduler,
            coordinator,
            workers,
            event,
        ));
    }
    results
}

#[cfg(feature = "terminal-tui")]
fn drain_provider_turn_finalizer_events_and_publish(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    queue: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
) -> Vec<ProviderTurnFinalizerResult> {
    let results = drain_provider_turn_finalizer_events(scheduler, coordinator, workers, queue);
    if !results.is_empty() {
        publish_main_queue_status(chat_dispatcher, scheduler);
        publish_provider_worker_status(chat_dispatcher, workers);
    }
    results
}

fn start_provider_turn_task(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    existing_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    input: &str,
    history_base_len: usize,
) -> Option<crate::chat::turn_scheduler::TurnTaskId> {
    let id = existing_task_id.unwrap_or_else(|| {
        scheduler.enqueue(
            input.to_string(),
            crate::chat::turn_scheduler::TurnPriority::Normal,
            history_base_len,
        )
    });
    if let Err(error) = scheduler.start_task(id) {
        tracing::warn!(
            task_id = id.get(),
            reused_queued_task = existing_task_id.is_some(),
            error = ?error,
            "TurnScheduler provider task start failed"
        );
        return None;
    }
    Some(id)
}

fn register_provider_history_commit_task(
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    scheduler: &crate::chat::turn_scheduler::TurnScheduler,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
) {
    if let Some(id) = id
        && let Some(task) = scheduler.task(id)
        && let Err(error) = coordinator.register_task(task)
    {
        tracing::warn!(
            task_id = id.get(),
            sequence = task.sequence,
            error = ?error,
            "HistoryCommitCoordinator provider task registration failed"
        );
    }
}

fn register_provider_turn_worker(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    scheduler: &crate::chat::turn_scheduler::TurnScheduler,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    kind: crate::chat::turn_worker::ProviderTurnWorkerKind,
) {
    if let Some(id) = id
        && let Some(task) = scheduler.task(id)
        && let Err(error) = workers.start_from_task(task, kind)
    {
        tracing::warn!(
            task_id = id.get(),
            sequence = task.sequence,
            kind = ?kind,
            error = ?error,
            "ProviderTurnWorkerRegistry provider worker registration failed"
        );
    }
}

#[cfg(feature = "terminal-tui")]
fn spawn_provider_turn_completion_waiter(
    turn_signal: dispatcher::TurnCompletionSignal,
    task_id: crate::chat::turn_scheduler::TurnTaskId,
    tx: mpsc::Sender<ProviderTurnCompletionEvent>,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let notified = turn_signal.notified_for(task_id);
        match notified {
            Some(notified) => {
                tokio::select! {
                    () = notified => {}
                    () = shutdown.cancelled() => {}
                }
            }
            None => {
                tokio::select! {
                    () = turn_signal.notified() => {}
                    () = shutdown.cancelled() => {}
                }
            }
        }
        let event = ProviderTurnCompletionEvent {
            task_id,
            outcome: turn_signal.consume_turn_outcome(task_id),
            usage: turn_signal.consume_turn_usage(task_id),
        };
        if tx.send(event).await.is_err() {
            tracing::debug!(
                task_id = task_id.get(),
                "provider turn completion waiter could not deliver event"
            );
        }
    });
}

fn request_provider_turn_cancel(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    reason: &'static str,
) {
    if let Some(id) = id {
        if let Err(error) = scheduler.request_cancel(id) {
            tracing::warn!(
                task_id = id.get(),
                reason,
                error = ?error,
                "TurnScheduler provider task cancel request failed"
            );
        }
        if let Err(error) = workers.request_cancel(id) {
            tracing::warn!(
                task_id = id.get(),
                reason,
                error = ?error,
                "ProviderTurnWorkerRegistry provider worker cancel request failed"
            );
        }
    }
}

fn record_provider_turn_usage(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    record: &crate::chat::session::MainSessionTokenUsageRecord,
) {
    if let Some(id) = id
        && let Err(error) = scheduler.record_usage(id, record)
    {
        tracing::warn!(
            task_id = id.get(),
            error = ?error,
            "TurnScheduler provider usage ledger record failed"
        );
    }
}

fn mark_provider_turn_completed(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    history_commit_len: usize,
    summary: &'static str,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    if let Some(id) = id {
        match scheduler.mark_completed(id, history_commit_len, summary) {
            Ok(()) => {
                if record_provider_worker_completed(workers, id) {
                    return record_provider_history_commit_outcome(coordinator, workers, scheduler, id);
                }
            }
            Err(error) => {
                tracing::warn!(
                    task_id = id.get(),
                    error = ?error,
                    "TurnScheduler provider task completion failed"
                );
            }
        }
    }
    Vec::new()
}

fn mark_provider_turn_failed(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    history_commit_len: usize,
    summary: impl Into<String>,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    if let Some(id) = id {
        match scheduler.mark_failed(id, history_commit_len, summary) {
            Ok(()) => {
                if record_provider_worker_failed(workers, id) {
                    return record_provider_history_commit_outcome(coordinator, workers, scheduler, id);
                }
            }
            Err(error) => {
                tracing::warn!(
                    task_id = id.get(),
                    error = ?error,
                    "TurnScheduler provider task failure failed"
                );
            }
        }
    }
    Vec::new()
}

fn mark_provider_turn_cancelled(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    summary: &'static str,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    if let Some(id) = id {
        match scheduler.mark_cancelled(id, summary) {
            Ok(()) => {
                if record_provider_worker_cancelled(workers, id) {
                    return record_provider_history_commit_outcome(coordinator, workers, scheduler, id);
                }
            }
            Err(error) => {
                tracing::warn!(
                    task_id = id.get(),
                    error = ?error,
                    "TurnScheduler provider task cancellation failed"
                );
            }
        }
    }
    Vec::new()
}

fn record_provider_worker_completed(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: crate::chat::turn_scheduler::TurnTaskId,
) -> bool {
    if let Err(error) = workers.record_completed(id) {
        tracing::warn!(
            task_id = id.get(),
            error = ?error,
            "ProviderTurnWorkerRegistry provider worker completion failed"
        );
        return false;
    }
    true
}

fn record_provider_worker_failed(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: crate::chat::turn_scheduler::TurnTaskId,
) -> bool {
    if let Err(error) = workers.record_failed(id) {
        tracing::warn!(
            task_id = id.get(),
            error = ?error,
            "ProviderTurnWorkerRegistry provider worker failure failed"
        );
        return false;
    }
    true
}

fn record_provider_worker_cancelled(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: crate::chat::turn_scheduler::TurnTaskId,
) -> bool {
    if let Err(error) = workers.record_cancelled(id) {
        tracing::warn!(
            task_id = id.get(),
            error = ?error,
            "ProviderTurnWorkerRegistry provider worker cancellation failed"
        );
        return false;
    }
    true
}

fn record_provider_history_commit_outcome(
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    scheduler: &crate::chat::turn_scheduler::TurnScheduler,
    id: crate::chat::turn_scheduler::TurnTaskId,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    let Some(task) = scheduler.task(id) else {
        tracing::warn!(
            task_id = id.get(),
            "HistoryCommitCoordinator missing scheduler task for provider outcome"
        );
        return Vec::new();
    };
    let Some(outcome) = crate::chat::history_commit::HistoryCommitOutcome::from_terminal_task(task) else {
        tracing::warn!(
            task_id = id.get(),
            state = ?task.state,
            "HistoryCommitCoordinator ignored non-terminal provider outcome"
        );
        return Vec::new();
    };
    if let Err(error) = coordinator.record_outcome(outcome) {
        tracing::warn!(
            task_id = id.get(),
            sequence = task.sequence,
            error = ?error,
            "HistoryCommitCoordinator provider outcome record failed"
        );
        return Vec::new();
    }
    let mut decisions = Vec::new();
    for decision in coordinator.drain_ready() {
        if let Err(error) = workers.apply_commit_decision(&decision) {
            tracing::warn!(
                task_id = id.get(),
                decision = ?decision,
                error = ?error,
                "ProviderTurnWorkerRegistry provider worker commit decision failed"
            );
        }
        tracing::trace!(
            task_id = id.get(),
            decision = ?decision,
            pending_tasks = coordinator.pending_tasks(),
            pending_outcomes = coordinator.pending_outcomes(),
            "HistoryCommitCoordinator provider outcome became ready"
        );
        decisions.push(decision);
    }
    decisions
}

fn is_queue_command(input: &str) -> bool {
    matches!(input.trim(), "/queue" | "/queue status")
}

#[cfg(feature = "terminal-tui")]
fn is_cost_command(input: &str) -> bool {
    input.trim() == "/cost"
}

fn is_workers_command(input: &str) -> bool {
    matches!(input.trim(), "/workers" | "/workers status")
        || !matches!(
            parse_workers_cancel_command(input),
            ProviderWorkerCancelCommand::NotCancel
        )
}

#[cfg(feature = "terminal-tui")]
fn is_active_turn_local_command(input: &str) -> bool {
    let (_priority, content) = input_priority_from_text(input);
    is_queue_command(&content)
        || is_cost_command(&content)
        || is_workers_command(&content)
        || local_session_command(&content).is_some()
}

#[cfg(feature = "terminal-tui")]
fn format_active_turn_local_notice(input: &str) -> String {
    let (_priority, content) = input_priority_from_text(input);
    let collapsed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview = truncate_chars_for_queued_notice(&collapsed, 160);
    format!("local > {preview}")
}

#[cfg(feature = "terminal-tui")]
async fn active_turn_local_command_output(
    msg: &crate::channels::traits::ChannelMessage,
    backlog: &std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &crate::chat::turn_scheduler::TurnScheduler,
    chat_session: &session::ChatSession,
    provider_turn_workers: &crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    chat_sessions: &mut crate::chat::sessions::ChatSessionsHandle,
    session_rings: &std::collections::HashMap<crate::chat::sessions::id::SessionId, crate::chat::sessions::SessionRing>,
    reaped_log_archive: &mut ReapedSessionLogArchive,
    reap_policy: &crate::chat::sessions::runtime::ReapPolicy,
    tools_registry: &[Box<dyn Tool>],
    daemon: DaemonControl<'_>,
) -> Option<String> {
    let (_priority, content) = classify_input_priority(msg);
    if is_queue_command(&content) {
        return Some(format_input_backlog_report(backlog, scheduler, 8));
    }
    if is_cost_command(&content) {
        return Some(commands::format_cost_feedback(chat_session));
    }
    if is_workers_command(&content) {
        return Some(format_provider_worker_report(&provider_worker_status(
            provider_turn_workers,
        )));
    }
    if let Some(action) = local_session_command(&content) {
        if action.requires_local_operator() && !is_local_operator_input(msg) {
            return Some("This session command requires local terminal operator input.".to_owned());
        }
        if is_daemon_session_command(&action) {
            let local_fallback = if matches!(action, crate::chat::sessions::SessionCommand::DaemonSessions) {
                handle_local_session_command(
                    &crate::chat::sessions::SessionCommand::Sessions,
                    chat_sessions,
                    session_rings,
                    reaped_log_archive,
                    reap_policy,
                    tools_registry,
                )
                .await
            } else {
                None
            };
            return start_daemon_session_command(&action, local_fallback, &chat_sessions.daemon_requests(), daemon);
        }
        return handle_local_session_command(
            &action,
            chat_sessions,
            session_rings,
            reaped_log_archive,
            reap_policy,
            tools_registry,
        )
        .await;
    }
    None
}

#[allow(clippy::too_many_arguments)]
#[cfg(feature = "terminal-tui")]
async fn process_active_turn_input_message(
    msg: crate::channels::traits::ChannelMessage,
    emit_chat_output: &mut impl FnMut(&str),
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    provider_turn_workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    chat_session: &session::ChatSession,
    chat_sessions: &mut crate::chat::sessions::ChatSessionsHandle,
    session_rings: &std::collections::HashMap<crate::chat::sessions::id::SessionId, crate::chat::sessions::SessionRing>,
    reaped_log_archive: &mut ReapedSessionLogArchive,
    reap_policy: &crate::chat::sessions::runtime::ReapPolicy,
    tools_registry: &[Box<dyn Tool>],
    daemon: DaemonControl<'_>,
) {
    if let Some((output, signal_cancel)) =
        active_turn_workers_cancel_output(&msg, scheduler, provider_turn_workers, provider_turn_task_id)
    {
        emit_chat_output(&output);
        publish_provider_worker_status(chat_dispatcher, provider_turn_workers);
        dispatch_provider_worker_cancel_signal(chat_dispatcher, signal_cancel);
        return;
    }
    if let Some(output) = active_turn_local_command_output(
        &msg,
        backlog,
        scheduler,
        chat_session,
        provider_turn_workers,
        chat_sessions,
        session_rings,
        reaped_log_archive,
        reap_policy,
        tools_registry,
        daemon,
    )
    .await
    {
        emit_chat_output(&output);
        return;
    }
    // An assignment dispatched as `interrupt` ends the turn in flight first,
    // through the very same local path Ctrl+C uses. Two things this is not:
    // it is not a timeout — nothing measured a clock, somebody asked for it —
    // and it is not the daemon acting, which only recorded the disposition and
    // put the item at the head of its queue. `steer` deliberately does *not*
    // land here: it jumps the queue and leaves the running turn alone.
    if let Some(delivery) = crate::chat::sessions::assignment::delivered(&msg)
        && delivery.disposition.ends_the_turn_in_flight()
    {
        request_provider_turn_cancel(
            scheduler,
            provider_turn_workers,
            provider_turn_task_id,
            "daemon assignment interrupt",
        );
        dispatch_provider_worker_cancel_signal(chat_dispatcher, Some(ProviderWorkerCancelSignal::CancelRequested));
        publish_provider_worker_status(chat_dispatcher, provider_turn_workers);
    }
    // Everything else about an assignment is ordinary input: `queue` lands at
    // the back of the backlog, `steer` and `interrupt` at the front, because
    // their message carries the same `/now ` prefix a person types to do that.
    let _priority =
        enqueue_input_message_and_return_priority_with_scheduler(backlog, scheduler, msg, chat_session.turns.len());
}

#[allow(clippy::too_many_arguments)]
#[cfg(feature = "terminal-tui")]
async fn process_active_turn_input_batch(
    msg: crate::channels::traits::ChannelMessage,
    emit_chat_output: &mut impl FnMut(&str),
    input_rx: &mut mpsc::Receiver<crate::channels::traits::ChannelMessage>,
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    provider_turn_workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    chat_session: &session::ChatSession,
    chat_sessions: &mut crate::chat::sessions::ChatSessionsHandle,
    session_rings: &std::collections::HashMap<crate::chat::sessions::id::SessionId, crate::chat::sessions::SessionRing>,
    reaped_log_archive: &mut ReapedSessionLogArchive,
    reap_policy: &crate::chat::sessions::runtime::ReapPolicy,
    tools_registry: &[Box<dyn Tool>],
    daemon: DaemonControl<'_>,
) {
    process_active_turn_input_message(
        msg,
        emit_chat_output,
        backlog,
        scheduler,
        provider_turn_workers,
        provider_turn_task_id,
        chat_dispatcher,
        chat_session,
        chat_sessions,
        session_rings,
        reaped_log_archive,
        reap_policy,
        tools_registry,
        daemon,
    )
    .await;
    while let Ok(msg) = input_rx.try_recv() {
        process_active_turn_input_message(
            msg,
            emit_chat_output,
            backlog,
            scheduler,
            provider_turn_workers,
            provider_turn_task_id,
            chat_dispatcher,
            chat_session,
            chat_sessions,
            session_rings,
            reaped_log_archive,
            reap_policy,
            tools_registry,
            daemon,
        )
        .await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProviderWorkerCancelCommand {
    NotCancel,
    Invalid(String),
    Cancel { sequence: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderWorkerCancelSignal {
    CancelRequested,
    CancelProviderTurn {
        task_id: crate::chat::turn_scheduler::TurnTaskId,
    },
}

fn parse_workers_cancel_command(input: &str) -> ProviderWorkerCancelCommand {
    let mut parts = input.split_whitespace();
    if parts.next() != Some("/workers") {
        return ProviderWorkerCancelCommand::NotCancel;
    }
    let Some(verb) = parts.next() else {
        return ProviderWorkerCancelCommand::NotCancel;
    };
    if !matches!(verb, "cancel" | "stop") {
        return ProviderWorkerCancelCommand::NotCancel;
    }
    let Some(target) = parts.next() else {
        return ProviderWorkerCancelCommand::Invalid("missing worker id".to_string());
    };
    if parts.next().is_some() {
        return ProviderWorkerCancelCommand::Invalid("expected exactly one worker id".to_string());
    }
    match parse_provider_worker_sequence(target) {
        Ok(sequence) => ProviderWorkerCancelCommand::Cancel { sequence },
        Err(error) => ProviderWorkerCancelCommand::Invalid(error),
    }
}

fn parse_provider_worker_sequence(target: &str) -> Result<u64, String> {
    let raw = target
        .strip_prefix("w#")
        .or_else(|| target.strip_prefix("W#"))
        .or_else(|| target.strip_prefix('#'))
        .unwrap_or(target);
    if raw.is_empty() {
        return Err("empty worker id".to_string());
    }
    let sequence = raw
        .parse::<u64>()
        .map_err(|_| format!("invalid worker id {target:?}"))?;
    if sequence == 0 {
        return Err("worker id must be greater than zero".to_string());
    }
    Ok(sequence)
}

#[cfg(feature = "terminal-tui")]
fn active_turn_workers_cancel_output(
    msg: &crate::channels::traits::ChannelMessage,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
) -> Option<(String, Option<ProviderWorkerCancelSignal>)> {
    let (_priority, content) = classify_input_priority(msg);
    provider_workers_cancel_output_for_input(msg, &content, scheduler, workers, provider_turn_task_id)
}

fn provider_workers_cancel_output_for_input(
    source: &crate::channels::traits::ChannelMessage,
    input: &str,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
) -> Option<(String, Option<ProviderWorkerCancelSignal>)> {
    match parse_workers_cancel_command(input) {
        ProviderWorkerCancelCommand::NotCancel => None,
        ProviderWorkerCancelCommand::Invalid(error) => Some((
            format!("Workers cancel failed: {error}.\nUsage: /workers cancel w#N"),
            None,
        )),
        ProviderWorkerCancelCommand::Cancel { sequence } => {
            if !is_local_operator_input(source) {
                return Some((
                    "Cancelling a provider worker requires local terminal operator input.".to_owned(),
                    None,
                ));
            }
            let Some(worker) = workers
                .snapshot()
                .into_iter()
                .find(|worker| worker.sequence == sequence)
            else {
                return Some((
                    format!("Workers cancel failed: provider worker w#{sequence} is not retained."),
                    None,
                ));
            };
            if !matches!(
                worker.state,
                crate::chat::turn_worker::ProviderTurnWorkerState::Running
                    | crate::chat::turn_worker::ProviderTurnWorkerState::Cancelling
            ) {
                return Some((
                    format!(
                        "Workers cancel ignored: provider worker w#{sequence} is already {}.",
                        provider_turn_worker_state_label(worker.state)
                    ),
                    None,
                ));
            }
            request_provider_turn_cancel(
                scheduler,
                workers,
                Some(worker.task_id),
                "workers cancel command during active turn",
            );
            let state = workers
                .worker(worker.task_id)
                .map(|worker| provider_turn_worker_state_label(worker.state))
                .unwrap_or("unknown");
            Some((
                format!(
                    "Requested cancellation for provider worker w#{sequence} task={} kind={} state={state}.",
                    worker.task_id.get(),
                    provider_turn_worker_kind_label(worker.kind),
                ),
                Some(
                    if provider_turn_task_id == Some(worker.task_id)
                        && worker.kind == crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited
                    {
                        ProviderWorkerCancelSignal::CancelRequested
                    } else {
                        ProviderWorkerCancelSignal::CancelProviderTurn {
                            task_id: worker.task_id,
                        }
                    },
                ),
            ))
        }
    }
}

fn dispatch_provider_worker_cancel_signal(
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    signal_cancel: Option<ProviderWorkerCancelSignal>,
) {
    let Some(signal) = signal_cancel else {
        return;
    };
    let action = match signal {
        ProviderWorkerCancelSignal::CancelRequested => crate::chat::action::Action::CancelRequested,
        ProviderWorkerCancelSignal::CancelProviderTurn { task_id } => {
            crate::chat::action::Action::CancelProviderTurn { task_id }
        }
    };
    let _ = chat_dispatcher.dispatch_or_log(action, "chat.workers_cancel_provider_turn");
}

const fn provider_turn_worker_kind_label(kind: crate::chat::turn_worker::ProviderTurnWorkerKind) -> &'static str {
    match kind {
        crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited => "foreground_awaited",
        crate::chat::turn_worker::ProviderTurnWorkerKind::Detached => "detached",
    }
}

const fn provider_turn_worker_state_label(state: crate::chat::turn_worker::ProviderTurnWorkerState) -> &'static str {
    match state {
        crate::chat::turn_worker::ProviderTurnWorkerState::Running => "running",
        crate::chat::turn_worker::ProviderTurnWorkerState::Cancelling => "cancelling",
        crate::chat::turn_worker::ProviderTurnWorkerState::AwaitingCommit(_) => "awaiting_commit",
        crate::chat::turn_worker::ProviderTurnWorkerState::Committed => "committed",
        crate::chat::turn_worker::ProviderTurnWorkerState::Cancelled => "cancelled",
        crate::chat::turn_worker::ProviderTurnWorkerState::Failed => "failed",
    }
}

fn format_provider_worker_report(status: &crate::chat::action::ProviderWorkerStatus) -> String {
    if status.running == 0
        && status.cancelling == 0
        && status.awaiting_commit == 0
        && status.finalized_payloads == 0
        && status.rows.is_empty()
    {
        return "No main provider workers are active.".to_string();
    }

    let mut lines = vec![format!(
        "Main provider workers: {} running, {} cancelling, {} awaiting commit, {} finalized payloads, {} finalized tokens.",
        status.running,
        status.cancelling,
        status.awaiting_commit,
        status.finalized_payloads,
        format_worker_tokens_compact(status.finalized_total_tokens),
    )];
    let active_rows: Vec<_> = status.rows.iter().filter(|row| row.is_active()).collect();
    if active_rows.is_empty() {
        lines.push("No active worker rows are currently retained.".to_string());
        return lines.join("\n");
    }
    for row in active_rows {
        lines.push(format_provider_worker_report_row(row));
    }
    lines.join("\n")
}

fn format_provider_worker_report_row(row: &crate::chat::action::ProviderWorkerStatusRow) -> String {
    use crate::chat::action::ProviderWorkerRowState;

    let state = match row.state {
        ProviderWorkerRowState::Running => "running",
        ProviderWorkerRowState::Cancelling => "cancelling",
        ProviderWorkerRowState::AwaitingCommit => "awaiting_commit",
        ProviderWorkerRowState::Committed => "committed",
        ProviderWorkerRowState::Cancelled => "cancelled",
        ProviderWorkerRowState::Failed => "failed",
    };
    let elapsed = format_provider_worker_elapsed(row.started_at_ms);
    let mut line = format!(
        "- w#{} task={} kind={} state={} completion={} elapsed={elapsed}",
        row.sequence,
        row.task_id,
        crate::chat::action::provider_worker_row_kind_label(row.kind),
        state,
        if row.completion_ready { "ready" } else { "pending" },
    );
    if let Some(tokens) = row.finalized_total_tokens.filter(|tokens| *tokens > 0) {
        line.push_str(" tokens=");
        line.push_str(&format_worker_tokens_compact(tokens));
    }
    line
}

fn format_provider_worker_elapsed(started_at_ms: i64) -> String {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let elapsed_ms = now_ms.saturating_sub(started_at_ms).max(0);
    let elapsed_secs = u64::try_from(elapsed_ms / 1000).unwrap_or_default();
    crate::chat::sessions::model::format_elapsed_compact(elapsed_secs)
}

fn format_worker_tokens_compact(tokens: u64) -> String {
    if tokens >= 1_000 {
        let whole = tokens / 1_000;
        let decimal = (tokens % 1_000) / 100;
        if decimal == 0 {
            format!("{whole}k")
        } else {
            format!("{whole}.{decimal}k")
        }
    } else {
        tokens.to_string()
    }
}

/// Session commands that stay answerable while a provider turn is in flight.
///
/// The three `Daemon*` variants are here for the same reason the local ones
/// are, only more so: a `--daemon` command exists to look at, or end, work in
/// another process, and the moment an operator needs that is precisely while a
/// long turn is running here. Leaving them out queued them behind the very turn
/// they were reached for. They cost the loop nothing — each one hands its
/// request to a task and returns the acceptance line (see
/// [`start_daemon_session_command`]).
#[cfg(feature = "terminal-tui")]
fn local_session_command(input: &str) -> Option<crate::chat::sessions::SessionCommand> {
    let action = crate::chat::sessions::parse_session_command(input)?;
    match action {
        crate::chat::sessions::SessionCommand::Sessions
        | crate::chat::sessions::SessionCommand::Logs { .. }
        | crate::chat::sessions::SessionCommand::Kill { .. } => Some(action),
        _ if is_daemon_session_command(&action) => Some(action),
        _ => None,
    }
}

fn format_input_backlog_report(
    backlog: &std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &crate::chat::turn_scheduler::TurnScheduler,
    max_preview: usize,
) -> String {
    let status = scheduler.status();
    let mut lines = vec![format!(
        "Main queue: {} queued ({} priority), {} running.",
        status.queued, status.priority_queued, status.running
    )];
    if backlog.is_empty() {
        lines.push("Queue is empty.".to_string());
        return lines.join("\n");
    }
    for (idx, queued) in backlog.iter().take(max_preview).enumerate() {
        let label = match queued.priority {
            InputQueuePriority::Normal => "normal",
            InputQueuePriority::Priority => "priority",
            InputQueuePriority::Control => "control",
        };
        let preview = truncate_chars_for_queued_notice(
            &queued.msg.content.split_whitespace().collect::<Vec<_>>().join(" "),
            120,
        );
        lines.push(format!("{}. [{}] {}", idx + 1, label, preview));
    }
    let hidden = backlog.len().saturating_sub(max_preview);
    if hidden > 0 {
        lines.push(format!("... {hidden} more queued."));
    }
    lines.join("\n")
}

fn drain_available_input_messages(
    input_rx: &mut mpsc::Receiver<crate::channels::traits::ChannelMessage>,
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    mut scheduler: Option<&mut crate::chat::turn_scheduler::TurnScheduler>,
    history_base_len: usize,
) {
    while let Ok(msg) = input_rx.try_recv() {
        if let Some(scheduler) = scheduler.as_deref_mut() {
            enqueue_input_message_with_scheduler(backlog, scheduler, msg, history_base_len);
        } else {
            enqueue_input_message(backlog, msg);
        }
    }
}

#[cfg(feature = "terminal-tui")]
fn format_queued_input_notice(input: &str, priority: InputQueuePriority) -> String {
    let (_, text) = input_priority_from_text(input);
    let label = match priority {
        InputQueuePriority::Normal => "queued",
        InputQueuePriority::Priority => "priority queued",
        InputQueuePriority::Control => "control queued",
    };
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview = truncate_chars_for_queued_notice(&collapsed, 160);
    format!("{label} > {preview}")
}

fn truncate_chars_for_queued_notice(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = input.chars().take(keep).collect();
    out.push('…');
    out
}

fn rollback_cancelled_turn_history(history: &mut Vec<ChatMessage>, len_before_user_turn: usize) {
    history.truncate(len_before_user_turn);
}

#[cfg(feature = "terminal-tui")]
trait ExternalEditorTerminalMode {
    fn suspend_for_editor(&self);
    fn restore_after_editor(&self);
}

#[cfg(feature = "terminal-tui")]
struct CrosstermExternalEditorTerminalMode;

#[cfg(feature = "terminal-tui")]
fn write_external_editor_suspend_sequences(out: &mut dyn std::io::Write) {
    crate::chat::sessions::pty::write_chat_alt_screen_leave_for_handoff(
        out,
        CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.load(std::sync::atomic::Ordering::Acquire),
    );
}

#[cfg(feature = "terminal-tui")]
fn write_external_editor_restore_sequences(out: &mut dyn std::io::Write) {
    crate::chat::sessions::pty::write_handoff_terminal_restore(
        out,
        CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.load(std::sync::atomic::Ordering::Acquire),
        CHAT_MOUSE_CAPTURE_ACTIVE.load(std::sync::atomic::Ordering::Acquire),
    );
}

#[cfg(feature = "terminal-tui")]
impl ExternalEditorTerminalMode for CrosstermExternalEditorTerminalMode {
    fn suspend_for_editor(&self) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
        let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
        let mut out = std::io::stdout();
        write_external_editor_suspend_sequences(&mut out);
        let _ = crossterm::terminal::disable_raw_mode();
    }

    fn restore_after_editor(&self) {
        let mut out = std::io::stdout();
        write_external_editor_restore_sequences(&mut out);
        let _ = crossterm::terminal::enable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
        let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
    }
}

#[cfg(feature = "terminal-tui")]
struct ExternalEditorTerminalGuard<'a> {
    terminal: &'a dyn ExternalEditorTerminalMode,
    active: bool,
}

#[cfg(feature = "terminal-tui")]
impl<'a> ExternalEditorTerminalGuard<'a> {
    fn new(terminal: &'a dyn ExternalEditorTerminalMode) -> Self {
        terminal.suspend_for_editor();
        Self { terminal, active: true }
    }
}

#[cfg(feature = "terminal-tui")]
impl Drop for ExternalEditorTerminalGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            self.terminal.restore_after_editor();
            self.active = false;
        }
    }
}

#[cfg(feature = "terminal-tui")]
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExternalEditorResult {
    Edited(String),
    Unchanged(String),
}

#[cfg(feature = "terminal-tui")]
fn resolve_external_editor() -> Option<String> {
    std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("EDITOR").ok().filter(|value| !value.trim().is_empty()))
}

#[cfg(feature = "terminal-tui")]
fn run_external_editor_command(editor: &str, path: &std::path::Path) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .arg("/C")
            .arg(format!("{editor} \"{}\"", path.display()))
            .status()
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("sh")
            .arg("-c")
            .arg("exec $PRX_EXTERNAL_EDITOR \"$1\"")
            .arg("prx-editor")
            .arg(path)
            .env("PRX_EXTERNAL_EDITOR", editor)
            .status()
    }
}

#[cfg(feature = "terminal-tui")]
fn edit_text_with_external_editor(
    initial: &str,
    editor: Option<String>,
    terminal: &dyn ExternalEditorTerminalMode,
) -> ExternalEditorResult {
    edit_text_with_external_editor_with_runner(initial, editor, terminal, run_external_editor_command)
}

#[cfg(feature = "terminal-tui")]
fn edit_text_with_external_editor_with_runner(
    initial: &str,
    editor: Option<String>,
    terminal: &dyn ExternalEditorTerminalMode,
    run_editor: impl FnOnce(&str, &std::path::Path) -> std::io::Result<std::process::ExitStatus>,
) -> ExternalEditorResult {
    let Some(editor) = editor else {
        return ExternalEditorResult::Unchanged("External editor unavailable: set VISUAL or EDITOR.".to_string());
    };
    let mut file = match tempfile::NamedTempFile::new() {
        Ok(file) => file,
        Err(err) => {
            return ExternalEditorResult::Unchanged(format!("External editor unavailable: temp file failed ({err})."));
        }
    };
    if let Err(err) = file.write_all(initial.as_bytes()) {
        return ExternalEditorResult::Unchanged(format!(
            "External editor unavailable: temp file write failed ({err})."
        ));
    }
    if let Err(err) = file.flush() {
        return ExternalEditorResult::Unchanged(format!(
            "External editor unavailable: temp file flush failed ({err})."
        ));
    }
    let path = file.path().to_path_buf();
    {
        let _terminal_guard = ExternalEditorTerminalGuard::new(terminal);
        let status = match run_editor(&editor, &path) {
            Ok(status) => status,
            Err(err) => {
                return ExternalEditorResult::Unchanged(format!("External editor failed to start: {err}."));
            }
        };
        if !status.success() {
            return ExternalEditorResult::Unchanged(format!("External editor exited with status {status}."));
        }
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => ExternalEditorResult::Edited(text),
        Err(err) => ExternalEditorResult::Unchanged(format!("External editor read failed: {err}.")),
    }
}

#[cfg(feature = "terminal-tui")]
fn attach_command_for_seq(seq: u64) -> String {
    format!("/attach {seq}")
}

#[cfg(feature = "terminal-tui")]
const TRANSCRIPT_COMMAND: &str = "/transcript";

/// Optimistically apply an input-routing focus change from the synchronous TUI
/// key thread, keeping the three authorities that decide "where the next
/// submittable input goes" consistent at the *same instant* the synthetic
/// `/attach` / `/detach` is enqueued (v1.1b review P0 — close the attach/detach
/// TOCTOU race):
///
/// 1. **`mirror.focus`** — read by [`tui::dispatch_global_key`]'s `resolve_esc`
///    in this same key thread, so the very next Esc judgment matches.
/// 2. **`Action::SessionFocusChanged`** — drives the reducer-owned `UiSnapshot`
///    the prompt indicator (colour+glyph) is rendered from, so the prompt the
///    user sees matches before they can type the next character.
/// 3. The caller then sends the synthetic command on the **same FIFO
///    `input_tx`** as real submissions, so the actual main-loop routing of any
///    immediately-following input lands on the same target the prompt shows.
///
/// The main loop remains the sole owner of the authoritative `attached_follow`
/// and rolls this optimistic focus back if the attach ultimately fails.
#[cfg(feature = "terminal-tui")]
fn apply_optimistic_focus(
    mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: &mpsc::Sender<()>,
    focus: crate::chat::sessions::FocusTarget,
) {
    mirror.lock().focus = focus;
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SessionFocusChanged { focus },
        "chat.optimistic_focus",
    );
    // Nudge the renderer so the prompt repaints with the new target without
    // waiting for the idle poll.
    let _ = redraw_tx.try_send(());
}

#[cfg(feature = "terminal-tui")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveSessionAttachProjection {
    view: crate::chat::sessions::ActiveSessionView,
    breadcrumb: String,
}

#[cfg(feature = "terminal-tui")]
const fn attach_breadcrumb_for_transition(
    was_following: bool,
    projection: &ActiveSessionAttachProjection,
) -> Option<&str> {
    if was_following {
        None
    } else {
        Some(projection.breadcrumb.as_str())
    }
}

#[cfg(feature = "terminal-tui")]
fn build_active_session_attach_projection(
    seq: u64,
    meta: Option<&crate::chat::sessions::model::ManagedSessionView>,
    tail_lines: Vec<String>,
    ring_lines: Vec<String>,
    truncated: bool,
) -> ActiveSessionAttachProjection {
    ActiveSessionAttachProjection {
        view: build_active_session_view(seq, meta, tail_lines, ring_lines, truncated, 0),
        breadcrumb: format!(
            "Attached session #{seq} (child viewport; input routes as steer). Type /detach or press Esc to stop."
        ),
    }
}

#[cfg(feature = "terminal-tui")]
fn build_active_session_view(
    seq: u64,
    meta: Option<&crate::chat::sessions::model::ManagedSessionView>,
    tail_lines: Vec<String>,
    ring_lines: Vec<String>,
    truncated: bool,
    scroll_offset: usize,
) -> crate::chat::sessions::ActiveSessionView {
    let (kind, title) = meta.map_or_else(
        || ("session".to_string(), String::new()),
        |view| (view.kind.as_str().to_string(), view.title.clone()),
    );
    let mut lines = Vec::with_capacity(tail_lines.len().saturating_add(ring_lines.len()));
    lines.extend(tail_lines);
    lines.extend(ring_lines);
    crate::chat::sessions::ActiveSessionView {
        seq,
        kind,
        title,
        lines,
        truncated,
        scroll_offset,
    }
    .clamped_for_height(usize::from(tui::ACTIVE_SESSION_VIEW_DESIRED_ROWS))
}

#[cfg(feature = "terminal-tui")]
fn active_session_view_from_ring(
    mut current: crate::chat::sessions::ActiveSessionView,
    ring: &crate::chat::sessions::SessionRing,
) -> crate::chat::sessions::ActiveSessionView {
    current.lines = ring.recent_lines(crate::chat::sessions::event::DEFAULT_RING_CAPACITY);
    current.truncated = ring.is_truncated();
    current.clamped_for_height(usize::from(tui::ACTIVE_SESSION_VIEW_DESIRED_ROWS))
}

#[cfg(feature = "terminal-tui")]
fn refresh_attached_session_view_from_ring(
    mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
    _sid: &crate::chat::sessions::id::SessionId,
    ring: &crate::chat::sessions::SessionRing,
) {
    let current = mirror.lock().active_session_view.clone();
    let Some(mut view) = current else {
        return;
    };
    view = active_session_view_from_ring(view, ring);
    mirror.lock().active_session_view = Some(view.clone());
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ActiveSessionViewUpdated { view: Some(view) },
        "chat.active_session_view_live",
    );
    if let Some(tx) = redraw_tx {
        let _ = tx.try_send(());
    }
}

#[cfg(feature = "terminal-tui")]
fn scroll_active_session_view(
    mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: &mpsc::Sender<()>,
    lines: usize,
    up: bool,
) {
    let Some(current) = mirror.lock().active_session_view.clone() else {
        return;
    };
    let visible_rows = usize::from(tui::ACTIVE_SESSION_VIEW_DESIRED_ROWS);
    let view = if up {
        current.scrolled_up(lines, visible_rows)
    } else {
        current.scrolled_down(lines).clamped_for_height(visible_rows)
    };
    mirror.lock().active_session_view = Some(view.clone());
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ActiveSessionViewUpdated { view: Some(view) },
        "chat.active_session_view_scroll",
    );
    let _ = redraw_tx.try_send(());
}

const DIFF_MAX_BYTES: usize = 256 * 1024;
const DIFF_MAX_LINES: usize = 2_000;
const DIFF_ERROR_MAX_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffViewSource {
    title: String,
    lines: Vec<String>,
    truncated: bool,
}

impl DiffViewSource {
    fn to_plain_text(&self) -> String {
        self.lines.join("\n")
    }
}

fn diff_command_args(cached: bool) -> Vec<&'static str> {
    let mut args = vec!["diff", "--no-ext-diff", "--no-textconv", "--no-color", "--unified=3"];
    if cached {
        args.push("--cached");
    }
    args
}

fn truncate_utf8_lossy_bytes(bytes: &[u8], max_bytes: usize) -> String {
    let capped = if bytes.len() <= max_bytes {
        bytes
    } else {
        let mut end = max_bytes.min(bytes.len());
        while end > 0
            && bytes
                .get(..end)
                .and_then(|candidate| std::str::from_utf8(candidate).ok())
                .is_none()
        {
            end = end.saturating_sub(1);
        }
        bytes.get(..end).map_or(&[] as &[u8], |candidate| candidate)
    };
    String::from_utf8_lossy(capped).to_string()
}

fn bounded_diff_lines(raw: &str, max_bytes: usize, max_lines: usize) -> (Vec<String>, bool) {
    let mut truncated = raw.len() > max_bytes;
    let capped = if truncated {
        let mut end = max_bytes;
        while end > 0 && !raw.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        raw.get(..end).map_or("", |candidate| candidate)
    } else {
        raw
    };
    let mut lines: Vec<String> = capped.lines().take(max_lines).map(str::to_string).collect();
    if capped.lines().nth(max_lines).is_some() {
        truncated = true;
    }
    if truncated {
        lines.push("[output truncated]".to_string());
    }
    (lines, truncated)
}

fn git_diff_error_line(stderr: &[u8], stdout: &[u8]) -> String {
    let message = if stderr.is_empty() { stdout } else { stderr };
    let text = truncate_utf8_lossy_bytes(message, DIFF_ERROR_MAX_BYTES);
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .map_or("git diff failed", |line| line);
    format!("diff unavailable: {first}")
}

struct BoundedDiffOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

async fn run_git_diff_bounded(workspace_dir: &std::path::Path, cached: bool) -> Result<BoundedDiffOutput, String> {
    use tokio::io::AsyncReadExt as _;

    let args = diff_command_args(cached);
    // The hardened git launch (GIT-ENV): the workspace's `.git/config` must
    // not run a program (fsmonitor, filters, ...) with PRX's environment.
    let mut command =
        crate::security::git_launch::git_command(crate::security::git_launch::GitScope::Repository(workspace_dir))
            .await
            .map_err(|err| format!("diff unavailable: {err}"))?;
    let mut child = command
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| format!("diff unavailable: {err}"))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "diff unavailable: failed to capture git stdout".to_string())?
        .take(u64::try_from(DIFF_MAX_BYTES.saturating_add(1)).unwrap_or(u64::MAX));
    let mut stdout_bytes = Vec::new();
    stdout
        .read_to_end(&mut stdout_bytes)
        .await
        .map_err(|err| format!("diff unavailable: failed to read git stdout ({err})"))?;
    let stdout_truncated = stdout_bytes.len() > DIFF_MAX_BYTES;
    if stdout_truncated {
        stdout_bytes.truncate(DIFF_MAX_BYTES);
        let _ = child.start_kill();
    }

    let mut stderr_bytes = Vec::new();
    if let Some(stderr) = child.stderr.take() {
        let mut stderr = stderr.take(u64::try_from(DIFF_ERROR_MAX_BYTES).unwrap_or(u64::MAX));
        let _ = stderr.read_to_end(&mut stderr_bytes).await;
    }

    let status = match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(err)) => return Err(format!("diff unavailable: git wait failed ({err})")),
        Err(_) => {
            let _ = child.start_kill();
            return Err("diff unavailable: git diff timed out".to_string());
        }
    };

    Ok(BoundedDiffOutput {
        success: status.success() || stdout_truncated,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
        truncated: stdout_truncated,
    })
}

async fn collect_workspace_diff(workspace_dir: &std::path::Path, cached: bool) -> DiffViewSource {
    let title = if cached { "staged diff" } else { "workspace diff" }.to_string();
    match run_git_diff_bounded(workspace_dir, cached).await {
        Ok(output) if output.success => {
            let text = truncate_utf8_lossy_bytes(&output.stdout, DIFF_MAX_BYTES);
            let (mut lines, line_truncated) = bounded_diff_lines(&text, DIFF_MAX_BYTES, DIFF_MAX_LINES);
            if lines.is_empty() {
                lines.push("(no workspace diff)".to_string());
            }
            DiffViewSource {
                title,
                lines,
                truncated: output.truncated || line_truncated,
            }
        }
        Ok(output) => DiffViewSource {
            title,
            lines: vec![git_diff_error_line(&output.stderr, &output.stdout)],
            truncated: false,
        },
        Err(err) => DiffViewSource {
            title,
            lines: vec![format!("diff unavailable: {err}")],
            truncated: false,
        },
    }
}

#[cfg(feature = "terminal-tui")]
fn open_transcript_view(
    mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
    snapshot_rx: Option<&tokio::sync::watch::Receiver<Arc<crate::chat::state::UiSnapshot>>>,
) {
    let snapshot_source = snapshot_rx.and_then(|rx| {
        let snapshot = rx.borrow();
        if snapshot.conversation_lines.is_empty() {
            None
        } else {
            Some((
                snapshot.session_title.to_string(),
                snapshot.conversation_lines.as_ref().clone(),
            ))
        }
    });
    let (view, focus) = {
        let mut guard = mirror.lock();
        let previous_offset = guard
            .active_session_view
            .as_ref()
            .filter(|view| view.kind == crate::chat::sessions::model::ManagedKind::Transcript.as_str())
            .map_or(0, |view| view.scroll_offset);
        let view = if let Some((session_title, conversation_lines)) = snapshot_source {
            tui::build_transcript_view(&session_title, &conversation_lines, previous_offset)
        } else {
            tui::build_transcript_view(&guard.session_title, &guard.conversation_lines, previous_offset)
        };
        let focus = crate::chat::sessions::FocusTarget::Transcript;
        guard.focus = focus;
        guard.active_session_view = Some(view.clone());
        (view, focus)
    };
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SessionFocusChanged { focus },
        "chat.transcript_focus_open",
    );
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ActiveSessionViewUpdated { view: Some(view) },
        "chat.transcript_view_open",
    );
    if let Some(tx) = redraw_tx {
        let _ = tx.try_send(());
    }
}

#[cfg(feature = "terminal-tui")]
fn close_transcript_view(
    mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: &mpsc::Sender<()>,
) {
    {
        let mut guard = mirror.lock();
        if !matches!(guard.focus, crate::chat::sessions::FocusTarget::Transcript)
            && guard
                .active_session_view
                .as_ref()
                .is_none_or(|view| view.kind != crate::chat::sessions::model::ManagedKind::Transcript.as_str())
        {
            return;
        }
        guard.focus = crate::chat::sessions::FocusTarget::Main;
        guard.active_session_view = None;
    }
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SessionFocusChanged {
            focus: crate::chat::sessions::FocusTarget::Main,
        },
        "chat.transcript_focus_close",
    );
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ActiveSessionViewUpdated { view: None },
        "chat.transcript_view_close",
    );
    let _ = redraw_tx.try_send(());
}

#[cfg(feature = "terminal-tui")]
fn open_diff_view(
    mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
    source: DiffViewSource,
) {
    let (view, focus) = {
        let mut guard = mirror.lock();
        let previous_offset = guard
            .active_session_view
            .as_ref()
            .filter(|view| view.kind == crate::chat::sessions::model::ManagedKind::Diff.as_str())
            .map_or(0, |view| view.scroll_offset);
        let view = tui::build_diff_view(&source.title, source.lines, source.truncated, previous_offset);
        let focus = crate::chat::sessions::FocusTarget::Diff;
        guard.focus = focus;
        guard.active_session_view = Some(view.clone());
        (view, focus)
    };
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SessionFocusChanged { focus },
        "chat.diff_focus_open",
    );
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ActiveSessionViewUpdated { view: Some(view) },
        "chat.diff_view_open",
    );
    if let Some(tx) = redraw_tx {
        let _ = tx.try_send(());
    }
}

#[cfg(feature = "terminal-tui")]
fn close_diff_view(
    mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: &mpsc::Sender<()>,
) {
    {
        let mut guard = mirror.lock();
        if !matches!(guard.focus, crate::chat::sessions::FocusTarget::Diff)
            && guard
                .active_session_view
                .as_ref()
                .is_none_or(|view| view.kind != crate::chat::sessions::model::ManagedKind::Diff.as_str())
        {
            return;
        }
        guard.focus = crate::chat::sessions::FocusTarget::Main;
        guard.active_session_view = None;
    }
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SessionFocusChanged {
            focus: crate::chat::sessions::FocusTarget::Main,
        },
        "chat.diff_focus_close",
    );
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ActiveSessionViewUpdated { view: None },
        "chat.diff_view_close",
    );
    let _ = redraw_tx.try_send(());
}

#[cfg(feature = "terminal-tui")]
fn open_provider_worker_view(
    mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
    sequence: u64,
) {
    let (view, focus) = {
        let mut guard = mirror.lock();
        let previous_view = guard
            .active_session_view
            .as_ref()
            .filter(|view| view.kind == crate::chat::action::PROVIDER_WORKER_VIEW_KIND && view.seq == sequence);
        let io_lines = tui::provider_worker_io_lines_for_streaming_draft(
            &guard.conversation_lines,
            guard.streaming_draft_for_worker(sequence),
            12,
        );
        let view = crate::chat::action::build_provider_worker_active_view_with_io_preserving_scroll(
            &guard.provider_worker_status,
            sequence,
            previous_view,
            io_lines,
        );
        let focus = crate::chat::sessions::FocusTarget::Worker { sequence };
        guard.focus = focus;
        guard.active_session_view = Some(view.clone());
        (view, focus)
    };
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SwitcherClosed,
        "chat.switcher_closed_worker_view",
    );
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SessionFocusChanged { focus },
        "chat.provider_worker_focus_open",
    );
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ActiveSessionViewUpdated { view: Some(view) },
        "chat.provider_worker_view_open",
    );
    if let Some(tx) = redraw_tx {
        let _ = tx.try_send(());
    }
}

#[cfg(feature = "terminal-tui")]
fn close_provider_worker_view(
    mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: &mpsc::Sender<()>,
) {
    {
        let mut guard = mirror.lock();
        if !matches!(guard.focus, crate::chat::sessions::FocusTarget::Worker { .. })
            && guard
                .active_session_view
                .as_ref()
                .is_none_or(|view| view.kind != crate::chat::action::PROVIDER_WORKER_VIEW_KIND)
        {
            return;
        }
        guard.focus = crate::chat::sessions::FocusTarget::Main;
        guard.active_session_view = None;
    }
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SessionFocusChanged {
            focus: crate::chat::sessions::FocusTarget::Main,
        },
        "chat.provider_worker_focus_close",
    );
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ActiveSessionViewUpdated { view: None },
        "chat.provider_worker_view_close",
    );
    let _ = redraw_tx.try_send(());
}

/// Handle `/pty <command>` (v3a): spawn an interactive PTY shell and hand the
/// terminal over to it for the duration of an attach.
///
/// Flow:
/// 1. Spawn the PTY session (security-gated, hardened env) at the host
///    terminal's current size.
/// 2. Register it so `/sessions` / `/kill` can see / terminate it.
/// 3. Acquire a [`PtyHandoffGuard`] — this pauses the chat render loop and
///    blocks until it has parked, so we can take over `stdin`/`stdout` without a
///    keystroke-stealing race. The guard's `Drop` restores the chat TUI on
///    **every** exit path.
/// 4. Run the byte passthrough on blocking threads until detach (`Ctrl-]`) or
///    child exit.
/// 5. The guard drops here, restoring the chat TUI (resume render loop + force a
///    full redraw to wipe PTY residue).
#[cfg(feature = "terminal-tui")]
async fn handle_pty_command(
    command: &str,
    workspace_dir: &std::path::Path,
    chat_sessions: &mut crate::chat::sessions::ChatSessionsHandle,
    handoff: &Arc<crate::chat::sessions::pty::HandoffControl>,
    redraw_handle: Option<&mpsc::Sender<()>>,
    emit_chat_output: &impl Fn(&str),
) {
    use crate::chat::sessions::pty::PtyShellSession;
    use portable_pty::PtySize;

    // Host terminal size → PTY winsize (fall back to a sane 80x24).
    let size = crossterm::terminal::size().map_or(
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        },
        |(cols, rows)| PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        },
    );

    // v3b: enforce the live-PTY cap before spawning. Detached PTYs stay alive
    // (each holds a drain thread + ring + fds), so refuse a new one past the limit
    // with a hint to `/kill` an existing session.
    if chat_sessions.live_pty_count() >= crate::chat::sessions::pty::MAX_LIVE_PTYS {
        emit_chat_output(&format!(
            "Too many live PTY sessions (limit {}). Detach and /kill one before opening another.",
            crate::chat::sessions::pty::MAX_LIVE_PTYS
        ));
        return;
    }

    let session = match PtyShellSession::spawn(command, workspace_dir, size) {
        Ok(session) => session,
        Err(e) => {
            emit_chat_output(&format!("Failed to start interactive PTY session: {e}"));
            return;
        }
    };
    let seq = chat_sessions.add_pty(session.clone());
    emit_chat_output(&format!(
        "Interactive PTY session #{seq}: {command} — Ctrl-] to detach (Ctrl-C/Ctrl-D pass through to the shell)."
    ));

    // First `/pty` goes straight through the unified re-attach entry point, so the
    // spawn path and a later `/attach` share exactly one passthrough code path.
    reattach_pty(&session, seq, handoff, redraw_handle, emit_chat_output).await;
}

/// Re-attach (or first-attach) to a live PTY session: hand the terminal to it,
/// replay recent context, drive the stdin↔PTY passthrough, and on detach restore
/// the chat TUI **without killing the PTY** (v3b).
///
/// This is the single entry point for both `/pty <cmd>` (right after spawn) and
/// `/attach <seq>` of an already-live PTY. It reuses every v3a safety mechanism:
///
/// - [`PtyHandoffGuard::acquire`] pauses the render loop and refuses the attach
///   (returning the terminal untouched) if the loop never acks the pause; and
/// - the guard's `Drop` restores terminal modes + forces a full redraw on every
///   exit path (detach, child exit, error, panic).
///
/// Unlike v3a, the persistent drain reader and the writer live in the session's
/// runtime and survive detach: this function only *borrows* them for the attach.
/// Detach flips the sink off `stdout` (under the sink lock, so no byte races the
/// render loop's resume) and leaves the child running.
#[cfg(feature = "terminal-tui")]
async fn reattach_pty(
    session: &crate::chat::sessions::pty::PtyShellSession,
    seq: u64,
    handoff: &Arc<crate::chat::sessions::pty::HandoffControl>,
    redraw_handle: Option<&mpsc::Sender<()>>,
    emit_chat_output: &impl Fn(&str),
) {
    use crate::chat::sessions::pty::PtyHandoffGuard;

    // Build a redraw nudge for the guard so the chat TUI repaints the instant we
    // resume (rather than waiting out the render loop's idle poll).
    let redraw_nudge: Option<Box<dyn Fn() + Send>> = redraw_handle.cloned().map(|tx| {
        let f: Box<dyn Fn() + Send> = Box::new(move || {
            let _ = tx.try_send(());
        });
        f
    });

    let handoff = Arc::clone(handoff);
    let session_for_passthrough = session.clone();
    let outcome = crate::runtime::blocking::spawn_blocking(move || {
        // Acquire the handoff guard: pause the render loop and wait for its ack. If
        // the ack times out we do NOT proceed (running while the render loop might
        // still touch the terminal would corrupt the screen). `acquire` un-pauses
        // the render loop itself on timeout, so we just report the abort.
        let Some(_guard) = PtyHandoffGuard::acquire(
            handoff,
            redraw_nudge,
            CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.load(std::sync::atomic::Ordering::Acquire),
            CHAT_MOUSE_CAPTURE_ACTIVE.load(std::sync::atomic::Ordering::Acquire),
        ) else {
            return PtyOutcome::AttachAborted;
        };
        PtyOutcome::Exited(run_pty_attach(&session_for_passthrough))
        // `_guard` drops here → terminal modes restored, render loop resumes +
        // full redraw forced. The PTY child stays alive (no kill on detach).
    })
    .await;

    match outcome {
        Ok(PtyOutcome::Exited(PtyExit::Detached)) => {
            emit_chat_output(&format!(
                "Detached from PTY session #{seq} (still running — /attach #{seq} to return, /kill #{seq} to stop)."
            ));
        }
        Ok(PtyOutcome::Exited(PtyExit::ChildExited)) => {
            // The child exited; reap the drain thread so it does not linger.
            session.reap_reader();
            emit_chat_output(&format!("Interactive PTY session #{seq} exited."));
        }
        Ok(PtyOutcome::AttachAborted) => {
            // The render loop never acked the pause; we refused the handoff to
            // avoid two threads fighting over the terminal. The PTY is untouched
            // and still attachable later.
            tracing::warn!(seq, "PTY attach aborted: render loop did not park in time");
            emit_chat_output(&format!(
                "Could not enter PTY session #{seq}: the chat renderer did not pause in time (terminal unchanged)."
            ));
        }
        Err(e) => {
            // The passthrough task panicked; the guard's Drop still ran during
            // unwind, so the terminal is restored. The session detaches defensively
            // (it stays alive for a later attempt). Surface the fault.
            session.detach();
            tracing::error!(error = %e, seq, "PTY passthrough task panicked");
            emit_chat_output(&format!("PTY session #{seq} ended unexpectedly; terminal restored."));
        }
    }
}

/// The result of an attempted `/pty` attach: either the passthrough ran and
/// ended ([`PtyExit`]), or the handoff was refused because the render loop never
/// acknowledged the pause (so the terminal was left untouched).
#[cfg(feature = "terminal-tui")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PtyOutcome {
    /// The passthrough ran and ended this way.
    Exited(PtyExit),
    /// The handoff was aborted before takeover (render loop did not park).
    AttachAborted,
}

/// How an interactive PTY passthrough ended.
#[cfg(feature = "terminal-tui")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PtyExit {
    /// The user pressed `Ctrl-]`.
    Detached,
    /// The PTY child process exited on its own.
    ChildExited,
}

/// Drive the raw terminal ⇄ PTY byte passthrough for an attach until detach or
/// child exit (v3b).
///
/// Runs on a `spawn_blocking` thread with the chat render loop parked (the
/// [`PtyHandoffGuard`] owned by the caller guarantees that). Unlike v3a it does
/// **not** spawn a per-attach reader or own the writer: the session's persistent
/// drain reader is already running, and this function only:
///
/// 1. syncs the PTY + emulator to the current host size, clears the screen, and
///    renders the in-process emulator's current screen so the user sees the exact
///    on-screen state (re-attach restore, v3b-b — correct for vim/htop, not just a
///    raw byte replay);
/// 2. marks the sink attached so the drain reader mirrors live PTY output to
///    `stdout` while we are here;
/// 3. nudges the child with a `SIGWINCH` size jitter as a secondary safeguard so a
///    program tracking its own size also re-flows (v3b-b);
/// 4. reads raw `stdin`, classifying each byte
///    ([`crate::chat::sessions::pty::classify_input_byte`]) — `Ctrl-]` detaches,
///    everything else (incl. `Ctrl-C`/`Ctrl-D`) is forwarded to the PTY child —
///    and rechecks the child-done flag each tick so a child that exits while the
///    user is idle ends the attach promptly.
///
/// On **every** exit path (detach, child exit, error, panic) the local RAII
/// `AttachScope` detaches the sink — under the sink lock, so the drain reader is
/// guaranteed to have stopped writing `stdout` before the `PtyHandoffGuard`
/// resumes the chat render loop (the v3a invariant). The child is **not** killed:
/// the PTY stays alive for a later re-attach. Never panics: I/O errors end the
/// attach and the guard still restores the terminal.
#[cfg(feature = "terminal-tui")]
fn run_pty_attach(session: &crate::chat::sessions::pty::PtyShellSession) -> PtyExit {
    use crate::chat::sessions::pty::{InputByte, classify_input_byte};
    use std::io::Write as _;

    // Current host geometry (for resize-forward seed + redraw nudge). Fall back to
    // a sane 80x24 if crossterm cannot read it.
    let host_size = || {
        crossterm::terminal::size().map_or(
            portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            |(cols, rows)| portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
    };

    // RAII: detach the sink on EVERY exit path (incl. panic unwind) so the drain
    // reader stops mirroring to `stdout` before the chat render loop resumes.
    struct AttachScope<'a> {
        session: &'a crate::chat::sessions::pty::PtyShellSession,
    }
    impl Drop for AttachScope<'_> {
        fn drop(&mut self) {
            // Under the sink lock: after this returns the reader will not write
            // `stdout` again until re-attached (v3a invariant). Does NOT kill the
            // child — the PTY survives detach for re-attach.
            self.session.detach();
        }
    }
    let _scope = AttachScope { session };

    // 0. Sync the PTY + emulator to the CURRENT host geometry BEFORE rendering the
    //    restore. The host may have been resized while this PTY was detached; if we
    //    rendered the emulator's old-size screen it would be offset / wrapped wrong.
    //    `resize` updates the emulator grid and the PTY master together (v3b-b), so
    //    the subsequent `attach()` redraw is laid out for the real terminal. Cheap,
    //    synchronous, non-fatal.
    if let Err(e) = session.resize(host_size()) {
        tracing::debug!(error = %e, "PTY resize-to-host before re-attach redraw failed");
    }

    // 1. Clear the screen + home the cursor, then attach. `attach()` flips the sink
    //    to `attached` AND renders the emulator's CURRENT screen to `stdout`
    //    atomically under the sink lock (v3b-b): because the drain reader's
    //    live-mirror `write()` (which also feeds the emulator) contends for that
    //    same lock, the screen-restore escape codes can never interleave with live
    //    bytes — the restore completes first, then live bytes follow in order.
    //    Rendering from the emulator (`state_formatted`) rather than replaying the
    //    raw ring means full-screen programs (vim/htop) re-appear correct on the
    //    first frame instead of being scrambled by spliced-in cursor sequences.
    {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[2J\x1b[H");
        let _ = out.flush();
    }
    session.attach();

    // 3. Secondary safeguard (v3b-b): the emulator already restored the picture, but
    //    nudge the child with a SIGWINCH size jitter so a program tracking its own
    //    size also re-flows. Harmless to streaming programs.
    session.nudge_redraw(host_size());

    // ── This thread: stdin → PTY writer, watching for detach + child exit ─────
    //
    // The session's drain reader owns child-done detection; we borrow its flag so
    // the stdin loop ends promptly when the child exits while the user is idle.
    let child_done = session.child_done_flag();

    // v5: forward host terminal resizes to the PTY. The chat render loop is parked
    // during the handoff, so crossterm `Resize` events go unconsumed; instead we
    // poll the host size each loop tick (≤100 ms) and push a `resize` to the PTY
    // master whenever it changes, so full-screen curses programs (vim, htop, …)
    // re-flow. `resize` is cheap, synchronous and non-panicking. We seed
    // `last_size` from the geometry we just nudged to so the first real change is
    // detected.
    let mut last_size: Option<(u16, u16)> = crossterm::terminal::size().ok();
    let on_tick = || {
        if let Ok((cols, rows)) = crossterm::terminal::size() {
            if last_size != Some((cols, rows)) {
                last_size = Some((cols, rows));
                if let Err(e) = session.resize(portable_pty::PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                }) {
                    tracing::warn!(error = %e, cols, rows, "PTY resize forward failed");
                }
            }
        }
    };
    let result = pty_stdin_loop(&child_done, on_tick, |byte| {
        match classify_input_byte(byte) {
            InputByte::Detach => Ok(true), // stop the loop (detach)
            InputByte::Forward => {
                // Forward to the session writer. If the writer is gone the child
                // has exited / been reaped — treat as child exit (stop the loop)
                // rather than erroring, so detach stays clean.
                if session.write_input(byte).is_err() {
                    return Ok(true);
                }
                Ok(false)
            }
        }
    });

    // `_scope` drops here (detach sink) regardless of how the loop ended. Map any
    // stdin-loop error to a child-exit-equivalent terminal outcome — the PTY is
    // left alive and re-attachable; the guard restores the terminal.
    match result {
        Ok(exit) => exit,
        Err(e) => {
            tracing::warn!(error = %e, "PTY attach stdin loop ended with error; detaching");
            PtyExit::Detached
        }
    }
}

/// Read raw `stdin` byte-by-byte, invoking `on_byte` for each, until `on_byte`
/// returns `Ok(true)` (detach) or `child_done` is observed (child exit).
///
/// Uses `libc::poll` on fd 0 with a 100 ms timeout so a child that exits while
/// the user is idle ends the passthrough promptly (a blocking `read` alone would
/// hang until the next keystroke). On non-Unix targets, where this `poll` is
/// unavailable, it falls back to a plain blocking read (the child-exit-while-idle
/// case is handled when the reader thread closes `stdin`'s peer; documented
/// platform limitation, plan §v3 risk 3).
///
/// `on_byte` returns `Ok(true)` to stop (detach), `Ok(false)` to continue, or an
/// `Err` to abort the passthrough (surfaced to the caller; the guard still
/// restores the terminal).
#[cfg(feature = "terminal-tui")]
#[allow(unsafe_code)]
fn pty_stdin_loop(
    child_done: &Arc<std::sync::atomic::AtomicBool>,
    mut on_tick: impl FnMut(),
    mut on_byte: impl FnMut(u8) -> Result<bool>,
) -> Result<PtyExit> {
    use std::io::Read as _;

    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 1024];

    loop {
        if child_done.load(Ordering::Acquire) {
            return Ok(PtyExit::ChildExited);
        }

        // Per-iteration housekeeping that must run even while the user is idle
        // (the poll below wakes at least every 100 ms). Used to forward host
        // terminal resizes to the PTY so curses programs re-flow. The render
        // loop is parked during the handoff, so crossterm `Resize` events are
        // not being consumed elsewhere — polling the size here is the
        // self-contained way to track it.
        on_tick();

        // Wait (bounded) for stdin to be readable so we can re-check child exit.
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            let mut pfd = libc::pollfd {
                fd: stdin.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `poll` reads/writes exactly the one `pollfd` we pass
            // (`nfds = 1`); the pointer is to a live local, valid for the call.
            // It dereferences nothing else and has no memory-safety
            // preconditions. A 100 ms timeout bounds the wait.
            let rc = unsafe { libc::poll(&raw mut pfd, 1, 100) };
            if rc <= 0 {
                // Timeout (0) or EINTR (<0): loop to re-check `child_done`.
                continue;
            }
            if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 && pfd.revents & libc::POLLIN == 0 {
                // stdin hung up with no data: treat as child-exit-equivalent.
                return Ok(PtyExit::ChildExited);
            }
        }

        let n = match stdin.read(&mut buf) {
            Ok(0) => return Ok(PtyExit::ChildExited), // stdin EOF
            Ok(n) => n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(anyhow::anyhow!("PTY stdin read failed: {e}")),
        };
        for &byte in buf.get(..n).unwrap_or(&buf) {
            if on_byte(byte)? {
                return Ok(PtyExit::Detached);
            }
        }
    }
}

/// Inner body of [`spawn_tui_unified_loop`].
///
/// **Fullscreen architecture.** ratatui owns the alternate screen and redraws
/// the transcript pane plus pinned bottom chrome as one full frame. Native
/// terminal scrollback is intentionally not used; `/export` is the durable
/// transcript escape hatch.
#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
fn run_tui_unified_loop(
    input_tx: mpsc::Sender<crate::channels::traits::ChannelMessage>,
    control_tx: mpsc::Sender<ChatControlEvent>,
    mirror: Arc<parking_lot::Mutex<tui::TuiState>>,
    mut redraw_rx: mpsc::Receiver<()>,
    redraw_tx: mpsc::Sender<()>,
    shutdown: &CancellationToken,
    last_ctrlc_ms: Arc<AtomicU64>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    snapshot_rx: Option<tokio::sync::watch::Receiver<Arc<crate::chat::state::UiSnapshot>>>,
    handoff: &Arc<crate::chat::sessions::pty::HandoffControl>,
    workspace_dir: std::path::PathBuf,
    security: Arc<crate::security::SecurityPolicy>,
) -> Result<()> {
    use crate::channels::traits::ChannelMessage;
    use crate::chat::action::Action;
    use crossterm::event::{Event, KeyEventKind};

    let render_source = snapshot_rx.map_or_else(
        || RenderSource::Mirror(Arc::clone(&mirror)),
        |rx| {
            tracing::info!("S4-A Commit 4: run_tui_unified_loop using RenderSource::Snapshot");
            RenderSource::Snapshot(rx)
        },
    );

    let mut terminal = new_fullscreen_terminal()?;
    let mut fullscreen_scroll = tui::FullscreenTranscriptScroll::default();
    let mut transcript_selection: Option<tui::TranscriptSelection> = None;
    let mut last_directional_switch_at: Option<Instant> = None;

    terminal
        .draw(|f| {
            render_source.with_view(|view| {
                tui::render_fullscreen_chat_with_selection(
                    f,
                    view,
                    &mut fullscreen_scroll,
                    transcript_selection.as_ref(),
                );
            });
        })
        .map_err(|e| anyhow::anyhow!("initial TUI draw failed: {e}"))?;

    let mut skip_next_draw = false;
    let mut deferred_redraw_requested = false;
    let mut pending_events = VecDeque::new();

    // 150 ms only while an on-screen animation is active. Idle mode uses a long
    // poll so a completed/empty TUI does not keep a fixed redraw/tick cadence.
    let active_animation_poll = Duration::from_millis(150);
    let idle_poll = Duration::from_secs(1);

    loop {
        if shutdown.is_cancelled() {
            let _ = terminal.draw(|frame| {
                let area = frame.area();
                frame.render_widget(ratatui::widgets::Clear, area);
            });
            return Ok(());
        }

        // ── 0. PTY terminal handoff (v3a) ─────────────────────────────
        // While an interactive PTY session is attached, the chat owns NONE
        // of the terminal: the main loop has handed raw stdin/stdout to the
        // PTY passthrough. We must not touch `crossterm` (poll/read) or
        // `terminal.draw` here, or we would corrupt the
        // PTY's full-screen output and steal its keystrokes. We park,
        // acknowledge the park (so the handoff can deterministically know
        // we are out of the way before it takes stdin), and re-check shortly.
        if handoff.is_paused() {
            handoff.ack_paused();
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        // Just resumed from a handoff: the PTY scribbled over the whole screen,
        // so force a full clear + repaint to wipe its residue before resuming
        // fullscreen rendering.
        if handoff.take_force_redraw() {
            if let Err(e) = terminal.clear() {
                tracing::warn!(error = %e, "post-PTY terminal clear failed");
            }
            skip_next_draw = false;
            deferred_redraw_requested = true;
        }

        // ── 1. Drain coalesced redraw wakeups, then redraw fullscreen frame ─
        let mut redraw_requested = false;
        while redraw_rx.try_recv().is_ok() {
            redraw_requested = true;
        }
        redraw_requested |= deferred_redraw_requested;
        deferred_redraw_requested = false;
        let periodic_redraw_active = render_source.with_view(|view| tui::periodic_redraw_active_for_view(view))
            || mirror.lock().periodic_redraw_active();
        if skip_next_draw && !redraw_requested {
            skip_next_draw = false;
            deferred_redraw_requested = true;
        } else if redraw_requested || periodic_redraw_active {
            if let Err(e) = terminal.draw(|f| {
                render_source.with_view(|view| {
                    tui::render_fullscreen_chat_with_selection(
                        f,
                        view,
                        &mut fullscreen_scroll,
                        transcript_selection.as_ref(),
                    );
                });
            }) {
                tracing::warn!(error = %e, "TUI draw failed");
            }
        }

        // ── 2. Wait for the next input event, with a 50 ms floor ──────
        let ev = if let Some(ev) = pending_events.pop_front() {
            ev
        } else {
            let poll = if periodic_redraw_active {
                active_animation_poll
            } else {
                idle_poll
            };
            if !crossterm::event::poll(poll)? {
                continue;
            }
            crossterm::event::read()?
        };
        // [DIAG] log structural events so we can observe paste/resize/control
        // behavior without turning large plain-text input into a log flood.
        match &ev {
            crossterm::event::Event::Key(k) if should_log_tui_key_event(k) => {
                tracing::info!(
                    event_type = "Key",
                    code = ?k.code,
                    modifiers = ?k.modifiers,
                    kind = ?k.kind,
                    "tui_input_event"
                );
            }
            crossterm::event::Event::Key(_) => {}
            crossterm::event::Event::Paste(s) => {
                tracing::info!(
                    event_type = "Paste",
                    chars_count = s.chars().count(),
                    bytes_count = s.len(),
                    first_8_chars = %s.chars().take(8).collect::<String>(),
                    "tui_input_event"
                );
            }
            crossterm::event::Event::Resize(w, h) => {
                tracing::info!(event_type = "Resize", w, h, "tui_input_event");
            }
            other => {
                tracing::info!(event_type = "Other", debug = ?other, "tui_input_event");
            }
        }
        match ev {
            Event::Key(key) => {
                // Skip key-release events: terminals with
                // KeyboardEnhancement flags (Kitty et al.) fire both Press
                // and Release for one physical keystroke. Only Press /
                // Repeat are authoritative input.
                if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    continue;
                }
                if let Some(ch) = plain_character_from_key(&key) {
                    let mut text = String::new();
                    text.push(ch);
                    drain_plain_character_burst(&mut text, &mut pending_events)?;
                    if text.len() > 1 {
                        let approval_active = {
                            let mirror_guard = mirror.lock();
                            mirror_guard.pending_tool_approval.is_some()
                                || matches!(mirror_guard.focus, crate::chat::sessions::FocusTarget::Approval)
                        };
                        if approval_active {
                            let _ = redraw_tx.try_send(());
                            skip_next_draw = true;
                            continue;
                        }
                        let _ =
                            chat_dispatcher.dispatch_or_log(Action::PasteReceived(text.clone()), "chat.tui_key_burst");
                        mirror.lock().input.paste(&text);
                        refresh_at_path_candidates_for_tui(
                            &mirror,
                            chat_dispatcher,
                            &redraw_tx,
                            &workspace_dir,
                            &security,
                        );
                        skip_next_draw = true;
                        continue;
                    }
                }
                if is_plain_character_key(&key) {
                    let mut mirror_guard = mirror.lock();
                    if mirror_guard.input.byte_len() >= tui::INPUT_MAX_BYTES {
                        mirror_guard.input.truncated = true;
                        skip_next_draw = true;
                        continue;
                    }
                }
                let _ = chat_dispatcher.dispatch_or_log(Action::KeyPressed(key), "chat.tui_key_pressed");

                sync_key_mirror_observation_state(&render_source, &mirror);
                let switcher_open_before_dispatch = mirror.lock().switcher.is_some();
                let mut dispatch = tui::dispatch_global_key(key, &mut mirror.lock());
                dispatch = debounce_directional_switch_dispatch(
                    key,
                    dispatch,
                    &mut last_directional_switch_at,
                    Instant::now(),
                );
                if !(key.code == crossterm::event::KeyCode::Esc
                    && key.modifiers == crossterm::event::KeyModifiers::NONE)
                {
                    refresh_at_path_candidates_for_tui(&mirror, chat_dispatcher, &redraw_tx, &workspace_dir, &security);
                }
                // C1 fix: any consumed keystroke may have mutated visible
                // state — typing in the input box, Tab folding a card,
                // Ctrl+R reverse-searching history, Esc clearing the buffer,
                // history navigation. Nudge the loop so the change paints
                // on the next iteration rather than waiting for the next
                // crossterm event (worst case 50 ms idle poll). cap=1 +
                // try_send coalesces, so this is cheap on key floods.
                if matches!(dispatch, tui::KeyDispatch::Ignored)
                    || (is_plain_character_key(&key) && matches!(dispatch, tui::KeyDispatch::Consumed))
                {
                    skip_next_draw = true;
                } else {
                    let _ = redraw_tx.try_send(());
                }
                match dispatch {
                    tui::KeyDispatch::Submitted(text) => {
                        if switcher_open_before_dispatch {
                            let _ = chat_dispatcher.dispatch_or_log(
                                crate::chat::action::Action::SwitcherClosed,
                                "chat.switcher_closed_submit",
                            );
                        }
                        let trimmed = text.trim().to_string();
                        if trimmed.is_empty() {
                            continue;
                        }
                        // `/exit` uses the same input queue as every other
                        // submitted command. The async owner can therefore
                        // hold it behind an in-flight turn's persistence
                        // acknowledgement instead of cancelling the dispatcher
                        // from this key thread and losing the final snapshot.
                        let (input_priority, _) = input_priority_from_text(&trimmed);
                        let activity_active = render_source
                            .with_view(|view| tui::execution_activity_active_for_view(view))
                            || mirror.lock().execution_activity_active();
                        if activity_active {
                            let notice = if is_active_turn_local_command(&trimmed) {
                                format_active_turn_local_notice(&trimmed)
                            } else {
                                format_queued_input_notice(&trimmed, input_priority)
                            };
                            let _ = chat_dispatcher.dispatch_or_log(
                                crate::chat::action::Action::SystemMessageAdded { text: notice },
                                "chat.input_queued_during_activity",
                            );
                            let _ = redraw_tx.try_send(());
                        }
                        let timestamp = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let msg = ChannelMessage {
                            id: uuid::Uuid::new_v4().to_string(),
                            sender: "user".to_string(),
                            reply_target: "user".to_string(),
                            content: trimmed,
                            channel: "terminal".to_string(),
                            timestamp,
                            thread_ts: None,
                            chat_kind: crate::channels::traits::ChatKind::Dm,
                            chat_title: None,
                            sender_display: None,
                            mentioned_uuids: vec![],
                            mentioned: false,
                            is_group_hint: false,
                            sender_is_bot: false,
                        };
                        if input_tx.blocking_send(msg).is_err() {
                            // Receiver dropped — chat::run is tearing down.
                            return Ok(());
                        }
                    }
                    tui::KeyDispatch::Exit => {
                        // Ctrl+D on empty buffer → graceful shutdown.
                        shutdown.cancel();
                        let _ = terminal.draw(|frame| {
                            let area = frame.area();
                            frame.render_widget(ratatui::widgets::Clear, area);
                        });
                        return Ok(());
                    }
                    tui::KeyDispatch::InterruptTurn => {
                        // Raw mode swallows kernel-delivered SIGINT, so we
                        // replicate the persistent ctrl_c() handler's
                        // double-press semantics directly:
                        //   * Two presses within DOUBLE_CTRLC_WINDOW_MS → exit.
                        //   * Otherwise cancel the in-flight turn (if any).
                        let now = now_ms();
                        let prev = last_ctrlc_ms.swap(now, Ordering::Relaxed);
                        if now.saturating_sub(prev) < DOUBLE_CTRLC_WINDOW_MS {
                            // S2-B Step 3: double press — also dispatch ShutdownRequested so the
                            // reducer really emits CancelToken/Quit (Off/Both mode still falls back
                            // to shutdown.cancel()).
                            let _ = chat_dispatcher.dispatch_or_log(
                                crate::chat::action::Action::ShutdownRequested,
                                "chat.shutdown_tui_double_ctrlc",
                            );
                            shutdown.cancel();
                            let _ = terminal.draw(|frame| {
                                let area = frame.area();
                                frame.render_widget(ratatui::widgets::Clear, area);
                            });
                            return Ok(());
                        }
                        // Single Ctrl+C is handled by the reducer path
                        // (Action::CancelRequested -> Effect::CancelToken).
                        if mirror.lock().clear_pending_tool_approval() {
                            let _ = redraw_tx.try_send(());
                        }
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::CancelRequested,
                            "chat.cancel_tui_single_ctrlc",
                        );
                    }
                    tui::KeyDispatch::SwitcherOpened { entries } => {
                        // v1.1b: mirror the just-opened switcher into the render
                        // snapshot (the mirror was already mutated in place).
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::SwitcherOpened { entries },
                            "chat.switcher_opened",
                        );
                    }
                    tui::KeyDispatch::SwitcherMoved { selected } => {
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::SwitcherMoved { selected },
                            "chat.switcher_moved",
                        );
                    }
                    tui::KeyDispatch::SwitcherClosed => {
                        let _ = chat_dispatcher
                            .dispatch_or_log(crate::chat::action::Action::SwitcherClosed, "chat.switcher_closed");
                    }
                    tui::KeyDispatch::SavedSessionPickerMoved { selected } => {
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::SavedSessionPickerMoved { selected },
                            "chat.saved_session_picker_moved",
                        );
                    }
                    tui::KeyDispatch::SavedSessionPickerClosed => {
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::SavedSessionPickerClosed,
                            "chat.saved_session_picker_closed",
                        );
                    }
                    tui::KeyDispatch::ResumeSavedSession { id } => {
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::SavedSessionPickerClosed,
                            "chat.saved_session_picker_closed_resume",
                        );
                        if control_tx
                            .blocking_send(ChatControlEvent::ResumeSavedSession { id })
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    tui::KeyDispatch::AttachSession { seq } => {
                        // Close the switcher in the snapshot, then route a
                        // synthetic `/attach <seq>` through the same input channel
                        // user submissions use, so the async main loop performs
                        // the attach via its existing handler (single owner of
                        // `attached_follow`; no async work in the key thread).
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::SwitcherClosed,
                            "chat.switcher_closed_attach",
                        );
                        // P0 race fix: optimistically point the prompt + Esc
                        // judgment at the new target *before* enqueuing the
                        // synthetic command, so any input the user types
                        // immediately afterwards is perceived to go where FIFO
                        // ordering will actually route it. The main loop rolls
                        // this back if the attach fails.
                        apply_optimistic_focus(
                            &mirror,
                            chat_dispatcher,
                            &redraw_tx,
                            crate::chat::sessions::focus::optimistic_focus(
                                crate::chat::sessions::focus::RoutingIntent::Attach { seq },
                            ),
                        );
                        let command = attach_command_for_seq(seq);
                        if send_synthetic_command(&input_tx, &command).is_err() {
                            return Ok(());
                        }
                    }
                    tui::KeyDispatch::SwitchSession { seq } => {
                        // P3: directional child-session switching reuses the
                        // exact same attach owner as Ctrl+G Enter. The key
                        // thread only applies optimistic focus and queues
                        // `/attach N`; the async main loop remains authoritative.
                        apply_optimistic_focus(
                            &mirror,
                            chat_dispatcher,
                            &redraw_tx,
                            crate::chat::sessions::focus::optimistic_focus(
                                crate::chat::sessions::focus::RoutingIntent::Attach { seq },
                            ),
                        );
                        let command = attach_command_for_seq(seq);
                        if send_synthetic_command(&input_tx, &command).is_err() {
                            return Ok(());
                        }
                    }
                    tui::KeyDispatch::OpenTranscriptViewer => {
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::SwitcherClosed,
                            "chat.switcher_closed_transcript",
                        );
                        if send_synthetic_command(&input_tx, TRANSCRIPT_COMMAND).is_err() {
                            return Ok(());
                        }
                    }
                    tui::KeyDispatch::CloseTranscriptViewer => {
                        close_transcript_view(&mirror, chat_dispatcher, &redraw_tx);
                    }
                    tui::KeyDispatch::OpenProviderWorkerView { sequence } => {
                        open_provider_worker_view(&mirror, chat_dispatcher, Some(&redraw_tx), sequence);
                    }
                    tui::KeyDispatch::CloseProviderWorkerView => {
                        close_provider_worker_view(&mirror, chat_dispatcher, &redraw_tx);
                    }
                    tui::KeyDispatch::CloseDiffViewer => {
                        close_diff_view(&mirror, chat_dispatcher, &redraw_tx);
                    }
                    tui::KeyDispatch::ExternalEditorRequested => {
                        let initial = mirror.lock().input.text();
                        let terminal_mode = CrosstermExternalEditorTerminalMode;
                        match edit_text_with_external_editor(&initial, resolve_external_editor(), &terminal_mode) {
                            ExternalEditorResult::Edited(text) => {
                                if let Err(e) = terminal.clear() {
                                    tracing::warn!(error = %e, "post-editor fullscreen terminal clear failed");
                                }
                                {
                                    let mut guard = mirror.lock();
                                    guard.input.set_text(&text);
                                    guard.input.clear_navigation_state();
                                }
                                let _ = chat_dispatcher.dispatch_or_log(
                                    crate::chat::action::Action::InputReplaced(text),
                                    "chat.external_editor_input_replaced",
                                );
                                refresh_at_path_candidates_for_tui(
                                    &mirror,
                                    chat_dispatcher,
                                    &redraw_tx,
                                    &workspace_dir,
                                    &security,
                                );
                                let _ = redraw_tx.try_send(());
                            }
                            ExternalEditorResult::Unchanged(reason) => {
                                if let Err(e) = terminal.clear() {
                                    tracing::warn!(error = %e, "post-editor fullscreen terminal clear failed");
                                }
                                surface_session_message(chat_dispatcher, Some(&redraw_tx), &reason);
                            }
                        }
                    }
                    tui::KeyDispatch::ToolApprovalDecision { tool_id, approved } => {
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::ToolApprovalReceived { tool_id, approved },
                            "chat.tool_approval_decision",
                        );
                        let _ = redraw_tx.try_send(());
                    }
                    tui::KeyDispatch::ModeChanged(mode) => {
                        let _ = chat_dispatcher
                            .dispatch_or_log(crate::chat::action::Action::ModeChanged(mode), "chat.mode_changed_key");
                        let _ = redraw_tx.try_send(());
                    }
                    tui::KeyDispatch::RequestDetach => {
                        // Esc on empty input while a session is focused → route a
                        // synthetic `/detach` so the main loop clears
                        // `attached_follow` + focus via its existing handler.
                        // P0 race fix: optimistically reset routing to main first
                        // so the prompt + next-input perception match the FIFO
                        // detach that is about to be processed. Detach never
                        // fails (it is a local clear), so no rollback is needed.
                        apply_optimistic_focus(
                            &mirror,
                            chat_dispatcher,
                            &redraw_tx,
                            crate::chat::sessions::focus::optimistic_focus(
                                crate::chat::sessions::focus::RoutingIntent::Detach,
                            ),
                        );
                        if send_synthetic_command(&input_tx, "/detach").is_err() {
                            return Ok(());
                        }
                    }
                    tui::KeyDispatch::ScrollTranscriptUp => {
                        fullscreen_scroll.line_up();
                        let _ = redraw_tx.try_send(());
                    }
                    tui::KeyDispatch::ScrollTranscriptDown => {
                        fullscreen_scroll.line_down();
                        let _ = redraw_tx.try_send(());
                    }
                    tui::KeyDispatch::PageTranscriptUp => {
                        let total_height = terminal.size().map(|s| s.height).unwrap_or(24);
                        let page_rows =
                            render_source.with_view(|view| tui::fullscreen_transcript_page_rows(view, total_height));
                        fullscreen_scroll.page_up(page_rows);
                        let _ = redraw_tx.try_send(());
                    }
                    tui::KeyDispatch::PageTranscriptDown => {
                        let total_height = terminal.size().map(|s| s.height).unwrap_or(24);
                        let page_rows =
                            render_source.with_view(|view| tui::fullscreen_transcript_page_rows(view, total_height));
                        fullscreen_scroll.page_down(page_rows);
                        let _ = redraw_tx.try_send(());
                    }
                    tui::KeyDispatch::TranscriptHome => {
                        fullscreen_scroll.jump_top();
                        let _ = redraw_tx.try_send(());
                    }
                    tui::KeyDispatch::TranscriptEnd => {
                        fullscreen_scroll.jump_bottom();
                        let _ = redraw_tx.try_send(());
                    }
                    tui::KeyDispatch::ScrollSessionUp => {
                        scroll_active_session_view(&mirror, chat_dispatcher, &redraw_tx, 1, true);
                    }
                    tui::KeyDispatch::ScrollSessionDown => {
                        scroll_active_session_view(&mirror, chat_dispatcher, &redraw_tx, 1, false);
                    }
                    tui::KeyDispatch::PageSessionUp => {
                        scroll_active_session_view(
                            &mirror,
                            chat_dispatcher,
                            &redraw_tx,
                            usize::from(tui::ACTIVE_SESSION_VIEW_DESIRED_ROWS),
                            true,
                        );
                    }
                    tui::KeyDispatch::PageSessionDown => {
                        scroll_active_session_view(
                            &mirror,
                            chat_dispatcher,
                            &redraw_tx,
                            usize::from(tui::ACTIVE_SESSION_VIEW_DESIRED_ROWS),
                            false,
                        );
                    }
                    tui::KeyDispatch::SessionHome => {
                        scroll_active_session_view(&mirror, chat_dispatcher, &redraw_tx, usize::MAX, true);
                    }
                    tui::KeyDispatch::SessionEnd => {
                        scroll_active_session_view(&mirror, chat_dispatcher, &redraw_tx, usize::MAX, false);
                    }
                    tui::KeyDispatch::Cancelled => {
                        let _ = chat_dispatcher
                            .dispatch_or_log(crate::chat::action::Action::CancelRequested, "chat.cancel_tui_esc");
                    }
                    tui::KeyDispatch::Consumed | tui::KeyDispatch::Ignored => {}
                }
            }
            Event::Paste(text) => {
                // P3 rearch: bracketed-paste mode (enabled in
                // `TerminalGuard::enter`) is what makes CJK IME input
                // *and* multi-line clipboard paste actually work. Without
                // it, IME commit strings are shredded into per-byte
                // KeyEvents with random modifier bits that
                // `dispatch_global_key` filters out.
                let approval_active = {
                    let mirror_guard = mirror.lock();
                    mirror_guard.pending_tool_approval.is_some()
                        || matches!(mirror_guard.focus, crate::chat::sessions::FocusTarget::Approval)
                };
                if approval_active {
                    let _ = redraw_tx.try_send(());
                    continue;
                }
                let _ = chat_dispatcher.dispatch_or_log(Action::PasteReceived(text.clone()), "chat.tui_paste");
                mirror.lock().input.paste(&text);
                refresh_at_path_candidates_for_tui(&mirror, chat_dispatcher, &redraw_tx, &workspace_dir, &security);
                // Paste mutates `input.lines` directly so the chrome must
                // repaint; without this kick the next redraw is gated on
                // the 50 ms poll.
                let _ = redraw_tx.try_send(());
            }
            Event::Resize(w, h) => {
                let _ = chat_dispatcher.dispatch_or_log(Action::TerminalResized { w, h }, "chat.tui_resize");
                // crossterm forwards the new size to ratatui automatically on
                // the next `draw()` call. Nudge the loop so redraw happens
                // immediately rather than waiting up to 50 ms for the next poll.
                let _ = redraw_tx.try_send(());
            }
            Event::Mouse(mouse) => {
                if apply_fullscreen_mouse_scroll(mouse.kind, &mut fullscreen_scroll) {
                    transcript_selection = None;
                    let _ = redraw_tx.try_send(());
                } else if let Ok(size) = terminal.size() {
                    let point = {
                        render_source.with_view(|view| {
                            tui::transcript_render_row_at_point(
                                view,
                                &fullscreen_scroll,
                                size.width,
                                size.height,
                                mouse.column,
                                mouse.row,
                            )
                        })
                    };
                    match mouse.kind {
                        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                            if let Some((row, column)) = point {
                                transcript_selection = Some(tui::TranscriptSelection::new(row, column));
                                let _ = redraw_tx.try_send(());
                            }
                        }
                        crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                            if let (Some(selection), Some((row, column))) = (transcript_selection.as_mut(), point) {
                                selection.update(row, column);
                                let _ = redraw_tx.try_send(());
                            }
                        }
                        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                            if let Some(mut selection) = transcript_selection.take() {
                                if let Some((row, column)) = point {
                                    selection.update(row, column);
                                }
                                if selection.moved() {
                                    copy_transcript_selection(
                                        &render_source,
                                        selection,
                                        size.width,
                                        chat_dispatcher,
                                        &redraw_tx,
                                    );
                                } else {
                                    let toggled = {
                                        let mut guard = mirror.lock();
                                        tui::toggle_reasoning_at_fullscreen_point(
                                            &mut guard,
                                            &fullscreen_scroll,
                                            size.width,
                                            size.height,
                                            mouse.column,
                                            mouse.row,
                                        )
                                    };
                                    if toggled {
                                        let _ = redraw_tx.try_send(());
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {
                // Focus / other events — ignore for now.
            }
        }
    }
}

// ── Session persistence helpers ──────────────────────────────────────────

/// Save a session to the Memory backend.
async fn save_session(mem: &dyn Memory, session: &session::ChatSession) -> Result<()> {
    let sanitized = sanitize::sanitize_session_content(session);
    let json = sanitized.to_json().map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
    mem.store(&session.memory_key(), &json, MemoryCategory::Conversation, None)
        .await
        .map_err(|e| anyhow::anyhow!("store: {e}"))?;
    Ok(())
}

/// Load a session by ID (exact key lookup, not similarity search).
///
/// Returns `Ok(None)` only when there is genuinely no entry under the key.
/// A storage error (D10/C1) or a corrupt stored blob (D10/C2) is propagated as
/// `Err` rather than collapsed into "not found", so callers can fail fast
/// instead of silently starting a fresh session that buries the real context.
async fn load_session_by_id(mem: &dyn Memory, id: &str) -> Result<Option<session::ChatSession>> {
    let key = format!("{}:{}", session::SESSION_MEMORY_PREFIX, id);
    // C1: propagate storage Err; Ok(None) is the only genuine "no such session".
    let Some(entry) = mem
        .get(&key)
        .await
        .map_err(|e| anyhow::anyhow!("failed to load session '{id}': {e}"))?
    else {
        return Ok(None);
    };
    // C2: a corrupt stored blob is data corruption, not absence — surface it.
    let session = session::ChatSession::from_json(&entry.content)
        .map_err(|e| anyhow::anyhow!("session '{id}' stored entry is corrupt: {e}"))?;
    // C3 (id consistency): the embedded id must match the requested id.  If
    // they differ the stored blob was written under the wrong key (or the key
    // was tampered with).  Resuming it would silently continue with the wrong
    // session and subsequent saves would land under the embedded id, burying
    // the entry that was stored under `id`.
    if session.id != id {
        return Err(anyhow::anyhow!(
            "session '{}' stored entry is corrupt: embedded id '{}' disagrees with requested id; \
             refusing to start a fresh session that would bury it",
            id,
            session.id
        ));
    }
    Ok(Some(session))
}

const CHAT_MESSAGE_EVENT_REPLAY_WINDOW: usize = 500;

#[derive(Debug)]
struct ChatMessageEventProjection {
    session: session::ChatSession,
    turns_from_events: bool,
    usage_from_events: bool,
}

/// Read the backend's session-key-filtered replay window. The query hard-filters
/// canonical + legacy keys before applying its bound, so unrelated workspace
/// traffic cannot evict this chat's events. A longer/incomplete replay simply
/// fails the exact parity test and retains the blob compatibility snapshot.
async fn load_chat_session_message_event_window(
    mem: &dyn Memory,
    workspace_id: &str,
    session_id: &str,
) -> Result<Vec<crate::memory::MessageEvent>> {
    let legacy_session_key = format!("chat:{session_id}");
    let principal = chat_runtime_envelope(workspace_id, &legacy_session_key).memory_principal();
    let mut events = mem
        .load_recent_session_context(crate::memory::SessionContextQuery {
            principal,
            since_event_id: None,
            limit: CHAT_MESSAGE_EVENT_REPLAY_WINDOW,
            include_roles: Vec::new(),
        })
        .await
        .map_err(|error| anyhow::anyhow!("failed to read MessageEvent replay window: {error}"))?;
    events.sort_by_key(|event| event.id);
    Ok(events)
}

fn apply_chat_message_event_projection(
    mut snapshot: session::ChatSession,
    events: &[crate::memory::MessageEvent],
    cost_config: &crate::config::schema::CostConfig,
) -> ChatMessageEventProjection {
    let message_events = events
        .iter()
        .filter(|event| {
            event.source == "chat"
                && event.event_type == "message.created"
                && matches!(event.role.as_str(), "user" | "assistant")
        })
        .collect::<Vec<_>>();
    let snapshot_turn_indices = snapshot
        .turns
        .iter()
        .enumerate()
        .filter_map(|(index, turn)| matches!(turn.role.as_str(), "user" | "assistant").then_some(index))
        .collect::<Vec<_>>();
    let turns_from_events = !message_events.is_empty()
        && message_events.len() == snapshot_turn_indices.len()
        && message_events.iter().zip(&snapshot_turn_indices).all(|(event, index)| {
            snapshot
                .turns
                .get(*index)
                .is_some_and(|turn| event.role == turn.role && event.content == turn.content)
        });
    if turns_from_events {
        for (event, index) in message_events.iter().zip(snapshot_turn_indices) {
            // MessageEvent owns replay role/content. Blob-only timestamp and
            // tool-call summaries remain compatibility metadata until the event
            // contract can reproduce them as well.
            if let Some(turn) = snapshot.turns.get_mut(index) {
                turn.role.clone_from(&event.role);
                turn.content.clone_from(&event.content);
            }
        }
    }

    let final_outcome_events = events
        .iter()
        .filter(|event| event.source == "chat" && event.event_type == "provider.final_outcome")
        .collect::<Vec<_>>();
    let mut malformed_outcome = false;
    let mut projected_usage = Vec::new();
    for event in &final_outcome_events {
        let Some(payload) = event.raw_payload_json.as_deref() else {
            malformed_outcome = true;
            break;
        };
        let outcome = match serde_json::from_str::<ProviderExecutionOutcome>(payload) {
            Ok(outcome) => outcome,
            Err(_) => {
                malformed_outcome = true;
                break;
            }
        };
        if let Some(record) =
            crate::llm::route_decision::MeteredTokenUsageRecord::from_provider_outcome(&outcome, cost_config)
        {
            projected_usage.push(record);
        }
    }
    let usage_from_events = !final_outcome_events.is_empty()
        && !malformed_outcome
        && projected_usage.len() == snapshot.token_usage_records.len()
        && projected_usage
            .iter()
            .zip(&snapshot.token_usage_records)
            .all(|(projected, persisted)| {
                projected.provider == persisted.provider
                    && projected.model == persisted.model
                    && projected.prompt_tokens == persisted.prompt_tokens
                    && projected.completion_tokens == persisted.completion_tokens
                    && projected.total_tokens == persisted.total_tokens
                    && projected.cache_creation_input_tokens == persisted.cache_creation_input_tokens
                    && projected.cache_read_input_tokens == persisted.cache_read_input_tokens
                    && projected.source == persisted.source
                    && projected.cost_usd == persisted.cost_usd
            });
    if usage_from_events {
        for (projected, persisted) in projected_usage.iter_mut().zip(&snapshot.token_usage_records) {
            projected.settlement_id.clone_from(&persisted.settlement_id);
        }
        snapshot.token_usage_records = projected_usage;
    }

    ChatMessageEventProjection {
        session: snapshot,
        turns_from_events,
        usage_from_events,
    }
}

/// Project resume/export/cost state from MessageEvent only after exact parity
/// with the persisted blob. Event read failures and partial/mismatched replay
/// retain the blob as the compatibility snapshot.
async fn project_chat_session_from_message_events(
    mem: &dyn Memory,
    workspace_id: &str,
    snapshot: session::ChatSession,
    cost_config: &crate::config::schema::CostConfig,
) -> ChatMessageEventProjection {
    let session_id = snapshot.id.clone();
    let events = match load_chat_session_message_event_window(mem, workspace_id, &session_id).await {
        Ok(events) => events,
        Err(error) => {
            tracing::warn!(
                session_id,
                error = %error,
                "MessageEvent projection unavailable; retaining chat blob compatibility snapshot"
            );
            return ChatMessageEventProjection {
                session: snapshot,
                turns_from_events: false,
                usage_from_events: false,
            };
        }
    };
    let projection = apply_chat_message_event_projection(snapshot, &events, cost_config);
    tracing::debug!(
        session_id,
        turns_from_events = projection.turns_from_events,
        usage_from_events = projection.usage_from_events,
        "evaluated chat MessageEvent projection parity"
    );
    projection
}

async fn load_session_by_id_with_message_events(
    mem: &dyn Memory,
    workspace_id: &str,
    id: &str,
    cost_config: &crate::config::schema::CostConfig,
) -> Result<Option<session::ChatSession>> {
    let Some(snapshot) = load_session_by_id(mem, id).await? else {
        return Ok(None);
    };
    Ok(Some(
        project_chat_session_from_message_events(mem, workspace_id, snapshot, cost_config)
            .await
            .session,
    ))
}

/// Load the most recent session.
///
/// Returns `Ok(None)` when no saved session exists. A storage error (D10/C3) or
/// a corrupt entry under an exact session key (D10) is propagated as `Err`,
/// never silently degraded to "no session".
async fn load_latest_session(mem: &dyn Memory) -> Result<Option<session::ChatSession>> {
    let entries = mem
        .list(Some(&MemoryCategory::Conversation), None)
        .await
        .map_err(|e| anyhow::anyhow!("failed to list saved sessions: {e}"))?;
    // Find entries with the session prefix, parse (corrupt exact entry -> Err) and sort by updated_at.
    let mut sessions: Vec<session::ChatSession> = collect_valid_sessions(&entries)?;
    sort_saved_sessions_newest_first(&mut sessions);
    Ok(sessions.into_iter().next())
}

async fn load_latest_session_with_message_events(
    mem: &dyn Memory,
    workspace_id: &str,
    cost_config: &crate::config::schema::CostConfig,
) -> Result<Option<session::ChatSession>> {
    let Some(snapshot) = load_latest_session(mem).await? else {
        return Ok(None);
    };
    Ok(Some(
        project_chat_session_from_message_events(mem, workspace_id, snapshot, cost_config)
            .await
            .session,
    ))
}

/// Parse session entries, distinguishing corruption from non-session entries.
///
/// A corrupt blob under an exact `chat_session:{id}` key is treated as data
/// corruption and returned as `Err` (rather than silently dropped, which would
/// misreport a damaged session as "no session"). Entries that are not session
/// entries at all (wrong prefix / blank id) are skipped.
fn collect_valid_sessions(entries: &[crate::memory::MemoryEntry]) -> Result<Vec<session::ChatSession>> {
    let mut sessions = Vec::new();
    for entry in entries {
        if let Some(session) = valid_session_entry(entry)? {
            sessions.push(session);
        }
    }
    Ok(sessions)
}

fn bind_session_to_runtime_provider_model(session: &mut session::ChatSession, provider_name: &str, model_name: &str) {
    session.provider = provider_name.to_string();
    session.model = model_name.to_string();
}

/// Validate and parse a candidate session entry.
///
/// * `Ok(None)` — the entry is not a chat-session entry (wrong prefix or blank
///   id); skip it.
/// * `Ok(Some(session))` — a valid session keyed by its own id.
/// * `Err(..)` — the entry *is* keyed as a chat session (exact `chat_session:{id}`)
///   but its blob is corrupt or its embedded id disagrees with the key. This is
///   data corruption and must not be silently filtered out (D10): a damaged
///   session must surface as an error, not be misreported as "no session".
fn valid_session_entry(entry: &crate::memory::MemoryEntry) -> Result<Option<session::ChatSession>> {
    let Some(rest) = entry.key.strip_prefix(session::SESSION_MEMORY_PREFIX) else {
        return Ok(None);
    };
    let Some(id_from_key) = rest.strip_prefix(':') else {
        return Ok(None);
    };
    if id_from_key.trim().is_empty() {
        return Ok(None);
    }
    let session = session::ChatSession::from_json(&entry.content)
        .map_err(|e| anyhow::anyhow!("saved session '{id_from_key}' stored entry is corrupt: {e}"))?;
    if session.id == id_from_key {
        Ok(Some(session))
    } else {
        Err(anyhow::anyhow!(
            "saved session entry key '{}' disagrees with stored id '{}'",
            entry.key,
            session.id
        ))
    }
}

fn session_turns_to_history(session: &session::ChatSession) -> Vec<ChatMessage> {
    session
        .turns
        .iter()
        .filter(|turn| turn.role == "user" || turn.role == "assistant")
        // A stored user turn re-enters a history here: one saved before
        // inbound text was neutralised could still open with a reserved marker.
        .map(|turn| {
            crate::agent::turn_contracts::stored_turn_message(
                turn.role.clone(),
                turn.content.clone(),
            )
        })
        .collect()
}

fn persisted_history_for_current_turn(
    session: &session::ChatSession,
    system_prompt: &str,
    user_input: &str,
) -> Vec<ChatMessage> {
    let mut history = Vec::with_capacity(session.turns.len().saturating_add(2));
    history.push(ChatMessage::system(system_prompt.to_string()));
    history.extend(session_turns_to_history(session));
    history.push(crate::agent::turn_contracts::inbound_user_message(user_input));
    history
}

fn upsert_leading_system_prompt(history: &mut Vec<ChatMessage>, system_prompt: String) {
    let system_message = ChatMessage::system(system_prompt);
    match history.first_mut() {
        None => history.push(system_message),
        Some(first) if first.role == "system" => *first = system_message,
        // Skill-RAG sessions are rebuilt from durable user/assistant turns
        // without a leading system message. Insert in that case; replacing
        // index 0 would silently discard the oldest durable user turn and
        // shift context-switch patch indices away from their audit source.
        Some(_) => history.insert(0, system_message),
    }
}

fn history_for_session_with_system(
    session: &session::ChatSession,
    config: &Config,
    model_name: &str,
    skills: &[crate::skills::Skill],
    native_tools: bool,
) -> Vec<ChatMessage> {
    let resumed_history = session_turns_to_history(session);
    if config.skill_rag.available() {
        return resumed_history;
    }
    let mut history = vec![ChatMessage::system(build_runtime_system_prompt(
        config,
        model_name,
        skills,
        native_tools,
    ))];
    history.extend(resumed_history);
    history
}

fn format_saved_chat_sessions(sessions: &[session::ChatSession]) -> String {
    if sessions.is_empty() {
        return "No saved chat sessions.".to_string();
    }

    let mut out = String::from("Saved chat sessions:\n");
    for session in sessions {
        let title = if session.title.is_empty() {
            "(untitled)"
        } else {
            session.title.as_str()
        };
        out.push_str(&format!(
            "  {} | {} | {} turns | {}\n",
            session.id,
            title,
            session.turn_count(),
            session.updated_at.format("%Y-%m-%d %H:%M")
        ));
    }
    out.push_str("\nResume with: /resume <ID> or /resume last");
    out
}

async fn saved_chat_sessions(mem: &dyn Memory) -> Result<Vec<session::ChatSession>> {
    let entries = mem
        .list(Some(&MemoryCategory::Conversation), None)
        .await
        .map_err(|e| anyhow::anyhow!("failed to list saved chat sessions: {e}"))?;
    let mut sessions = collect_valid_sessions(&entries)?;
    sort_saved_sessions_newest_first(&mut sessions);
    Ok(sessions)
}

fn sort_saved_sessions_newest_first(sessions: &mut [session::ChatSession]) {
    sessions.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| b.id.cmp(&a.id))
    });
}

fn parse_turn_boundary(raw: &str, turn_count: usize, command: &str) -> std::result::Result<usize, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("Usage: /{command} <N> where N is between 0 and {turn_count}."));
    }
    if trimmed.split_whitespace().count() != 1 {
        return Err(format!("Usage: /{command} <N> where N is between 0 and {turn_count}."));
    }
    let boundary = trimmed
        .parse::<usize>()
        .map_err(|_| format!("Invalid turn boundary '{trimmed}'. Use a number between 0 and {turn_count}."))?;
    if boundary > turn_count {
        return Err(format!(
            "Turn boundary {boundary} is out of range. This session has {turn_count} turns."
        ));
    }
    Ok(boundary)
}

fn background_summaries_for_turn_boundary(
    session: &session::ChatSession,
    keep_turns: usize,
) -> Vec<crate::chat::sessions::PersistedSessionSummary> {
    if keep_turns == session.turn_count() {
        session.background_sessions.clone()
    } else {
        Vec::new()
    }
}

fn branched_chat_session_from(
    current: &session::ChatSession,
    keep_turns: usize,
    provider_name: &str,
    model_name: &str,
) -> session::ChatSession {
    let mut branch = session::ChatSession::new(provider_name, model_name);
    branch.id = safe_branch_session_id();
    branch.title = if current.title.is_empty() {
        format!("Branch at {keep_turns} turns")
    } else {
        format!("{} (branch at {keep_turns})", current.title)
    };
    branch.turns = current.turns.iter().take(keep_turns).cloned().collect();
    branch.background_sessions = background_summaries_for_turn_boundary(current, keep_turns);
    branch.updated_at = chrono::Utc::now();
    branch
}

fn safe_branch_session_id() -> String {
    let uuid = uuid::Uuid::new_v4();
    let mut id = String::with_capacity("branch-".len() + 32);
    id.push_str("branch-");
    for byte in uuid.as_bytes() {
        id.push(char::from(b'a' + (byte >> 4)));
        id.push(char::from(b'a' + (byte & 0x0f)));
    }
    id
}

#[cfg(feature = "terminal-tui")]
fn rewound_chat_session_from(current: &session::ChatSession, keep_turns: usize) -> session::ChatSession {
    let mut rewound = current.clone();
    rewound.turns.truncate(keep_turns);
    rewound.background_sessions = background_summaries_for_turn_boundary(current, keep_turns);
    rewound.updated_at = chrono::Utc::now();
    rewound
}

fn format_turn_boundaries(session: &session::ChatSession) -> String {
    let mut out = format!(
        "Turn boundaries for session {} ({} turns):\n",
        session.id,
        session.turn_count()
    );
    out.push_str("  0 | empty branch\n");
    for (idx, turn) in session.turns.iter().enumerate() {
        let boundary = idx.saturating_add(1);
        let preview = truncate_with_ellipsis(&turn.content.replace('\n', " "), 72);
        out.push_str(&format!("  {boundary} | {} | {}\n", turn.role, preview.trim()));
    }
    out.push_str("\nUse /branch <N> to fork, or /rewind <N> to trim this session.");
    out
}

struct ChatSwitchCtx<'a> {
    chat_session: &'a mut session::ChatSession,
    chat_session_key: &'a mut String,
    fabric_turn_seq: &'a mut u64,
    history: &'a mut Vec<ChatMessage>,
    approval_router: Option<&'a Arc<dispatcher::ApprovalRouter>>,
    pending_chat_rewind: &'a mut Option<PendingChatRewind>,
    pending_diff_apply: &'a mut Option<PendingDiffApply>,
    chat_sessions: &'a mut crate::chat::sessions::ChatSessionsHandle,
    ignored_session_events: &'a mut std::collections::HashSet<crate::chat::sessions::id::SessionId>,
    session_rings:
        &'a mut std::collections::HashMap<crate::chat::sessions::id::SessionId, crate::chat::sessions::SessionRing>,
    reported_sessions: &'a mut std::collections::HashSet<String>,
    announced_started_sessions: &'a mut std::collections::HashSet<String>,
    last_sessions_summary: &'a mut String,
    last_sessions_entries: &'a mut Vec<crate::chat::sessions::SwitcherEntry>,
    attached_follow: &'a mut Option<crate::chat::sessions::id::SessionId>,
    attached_follow_seq: &'a mut Option<u64>,
    chat_dispatcher: &'a dispatcher::ChatDispatcher,
    redraw_handle: Option<&'a mpsc::Sender<()>>,
    config: &'a Config,
    provider_name: &'a str,
    model_name: &'a str,
    skills: &'a [crate::skills::Skill],
    native_tools: bool,
    #[cfg(feature = "terminal-tui")]
    chat_mirror: &'a Arc<parking_lot::Mutex<tui::TuiState>>,
}

struct PendingChatRewind {
    tool_id: String,
    target_session: session::ChatSession,
    approval_rx: tokio::sync::oneshot::Receiver<bool>,
}

enum RewindApprovalOutcome {
    Apply,
    Cancelled(String),
}

fn resolve_rewind_approval(
    tool_id: &str,
    approval: std::result::Result<bool, tokio::sync::oneshot::error::RecvError>,
) -> RewindApprovalOutcome {
    match approval {
        Ok(true) => RewindApprovalOutcome::Apply,
        Ok(false) => RewindApprovalOutcome::Cancelled("Rewind cancelled; current session unchanged.".to_string()),
        Err(_) => RewindApprovalOutcome::Cancelled(format!(
            "Rewind cancelled; approval channel closed for {tool_id} and current session is unchanged."
        )),
    }
}

struct PendingDiffApply {
    tool_id: String,
    plan: diff_apply::DiffApplyPlan,
    approval_rx: tokio::sync::oneshot::Receiver<bool>,
}

fn approval_in_progress(
    approval_router: Option<&Arc<dispatcher::ApprovalRouter>>,
    pending_chat_rewind: &Option<PendingChatRewind>,
    pending_diff_apply: &Option<PendingDiffApply>,
) -> bool {
    pending_chat_rewind.is_some()
        || pending_diff_apply.is_some()
        || approval_router.is_some_and(|router| router.has_pending())
}

const fn approval_already_pending_message() -> &'static str {
    "Another approval is already pending; approve or cancel it first."
}

fn clear_pending_approvals_for_session_switch(ctx: &mut ChatSwitchCtx<'_>) -> usize {
    let mut cleared_ids = Vec::new();
    if let Some(router) = ctx.approval_router {
        for tool_id in router.resolve_all(false) {
            cleared_ids.push(tool_id);
        }
    }
    if let Some(pending) = ctx.pending_chat_rewind.take() {
        if !cleared_ids.iter().any(|existing| existing == &pending.tool_id) {
            cleared_ids.push(pending.tool_id);
        }
    }
    if let Some(pending) = ctx.pending_diff_apply.take() {
        if !cleared_ids.iter().any(|existing| existing == &pending.tool_id) {
            cleared_ids.push(pending.tool_id);
        }
    }
    for tool_id in &cleared_ids {
        let _ = ctx.chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::ToolApprovalReceived {
                tool_id: tool_id.clone(),
                approved: false,
            },
            "chat.session_switch_fail_closed_approval",
        );
    }
    let _ = ctx.chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ToolApprovalCleared,
        "chat.session_switch_clear_approval_view",
    );
    #[cfg(feature = "terminal-tui")]
    {
        let mut mirror = ctx.chat_mirror.lock();
        mirror.pending_tool_approval = None;
        if matches!(mirror.focus, crate::chat::sessions::FocusTarget::Approval) {
            mirror.focus = crate::chat::sessions::FocusTarget::Main;
        }
    }
    cleared_ids.len()
}

#[cfg(feature = "terminal-tui")]
fn request_diff_apply_approval(
    plan: diff_apply::DiffApplyPlan,
    interactive_tui: bool,
    approval_router: Option<&Arc<dispatcher::ApprovalRouter>>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
) -> Result<PendingDiffApply, String> {
    if !interactive_tui {
        return Err(
            "Diff apply requires interactive TUI approval; unavailable in this mode. Workspace unchanged.".to_string(),
        );
    }
    let Some(router) = approval_router else {
        return Err(
            "Diff apply requires interactive TUI approval; approval router unavailable. Workspace unchanged."
                .to_string(),
        );
    };
    let (approval_tx, approval_rx) = tokio::sync::oneshot::channel::<bool>();
    let tool_id = format!("diff_apply:{}", uuid::Uuid::new_v4());
    if !router.register(tool_id.clone(), approval_tx) {
        return Err(approval_already_pending_message().to_string());
    }
    let args = plan.approval_args_json();
    let dispatch_result = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ToolApprovalRequested {
            task_id: None,
            tool_id: tool_id.clone(),
            name: "apply_fenced_diff".to_string(),
            args,
        },
        "chat.diff_apply_approval_requested",
    );
    if dispatch_result != crate::chat::dispatcher::DispatchResult::Sent {
        let _ = router.resolve(&tool_id, false);
        return Err("Diff apply approval could not be shown; workspace unchanged.".to_string());
    }
    Ok(PendingDiffApply {
        tool_id,
        plan,
        approval_rx,
    })
}

/// Out-of-band control requests from the TUI key handler to the chat loop.
/// Only the TUI sends them, so without `terminal-tui` the channel carries
/// nothing and the enum has no variants.
#[derive(Debug)]
enum ChatControlEvent {
    #[cfg(feature = "terminal-tui")]
    ResumeSavedSession { id: String },
}

async fn resume_saved_session_by_id(mem: &dyn Memory, target_id: &str, ctx: ChatSwitchCtx<'_>) -> Result<String> {
    let current_child_summaries = ctx
        .chat_sessions
        .snapshot()
        .await
        .iter()
        .map(|view| crate::chat::sessions::PersistedSessionSummary::from_view(view, String::new()))
        .collect::<Vec<_>>();
    let mut current_to_save = ctx.chat_session.clone();
    for summary in &current_child_summaries {
        current_to_save.record_background_session(summary.clone());
    }

    if let Err(e) = save_session(mem, &current_to_save).await {
        anyhow::bail!("Resume aborted: failed to save current session before switching: {e}");
    }
    for summary in &current_child_summaries {
        let _ = ctx.chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::BackgroundSessionRecorded {
                summary: summary.clone(),
            },
            "chat.resume_record_child_summary_before_switch",
        );
    }

    let workspace_id = ctx.config.workspace_dir.to_string_lossy();
    let loaded_session =
        match load_session_by_id_with_message_events(mem, workspace_id.as_ref(), target_id, &ctx.config.cost).await {
            Ok(Some(session)) => session,
            Ok(None) => anyhow::bail!("Saved chat session '{target_id}' not found."),
            Err(e) => {
                anyhow::bail!("Resume aborted: failed to load saved chat session '{target_id}': {e}")
            }
        };
    let loaded_id = loaded_session.id.clone();
    let loaded_turns = loaded_session.turn_count();
    let loaded_title = if loaded_session.title.is_empty() {
        "(untitled)".to_string()
    } else {
        loaded_session.title.clone()
    };

    apply_chat_session_switch(ctx, loaded_session).await;
    Ok(format!(
        "Resumed saved chat session {loaded_id} ({loaded_title}, {loaded_turns} turns)."
    ))
}

async fn start_new_chat_session(mem: &dyn Memory, ctx: ChatSwitchCtx<'_>) -> Result<String> {
    let current_child_summaries = ctx
        .chat_sessions
        .snapshot()
        .await
        .iter()
        .map(|view| crate::chat::sessions::PersistedSessionSummary::from_view(view, String::new()))
        .collect::<Vec<_>>();
    let mut current_to_save = ctx.chat_session.clone();
    for summary in &current_child_summaries {
        current_to_save.record_background_session(summary.clone());
    }

    if let Err(e) = save_session(mem, &current_to_save).await {
        anyhow::bail!("New session aborted: failed to save current session: {e}");
    }
    for summary in &current_child_summaries {
        let _ = ctx.chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::BackgroundSessionRecorded {
                summary: summary.clone(),
            },
            "chat.new_record_child_summary_before_switch",
        );
    }

    #[cfg(feature = "terminal-tui")]
    let current_mode = ctx.chat_mirror.lock().chat_mode;
    #[cfg(not(feature = "terminal-tui"))]
    let current_mode = ctx.chat_session.mode;

    let previous_id = current_to_save.id.clone();
    let new_session = fresh_chat_session(ctx.provider_name, ctx.model_name, current_mode);
    let new_id = new_session.id.clone();
    if let Err(e) = save_session(mem, &new_session).await {
        anyhow::bail!("New session aborted: failed to persist new session: {e}");
    }

    apply_chat_session_switch(ctx, new_session).await;
    Ok(format!(
        "Started new chat session {new_id}. Previous session {previous_id} was saved."
    ))
}

fn fresh_chat_session(provider_name: &str, model_name: &str, mode: commands::ChatMode) -> session::ChatSession {
    let mut session = session::ChatSession::new(provider_name, model_name);
    session.set_mode(mode);
    session
}

async fn apply_chat_session_switch(mut ctx: ChatSwitchCtx<'_>, mut loaded_session: session::ChatSession) {
    let cleared_approvals = clear_pending_approvals_for_session_switch(&mut ctx);
    if cleared_approvals > 0 {
        tracing::warn!(
            cleared_approvals,
            "session switch resolved pending approvals fail-closed before swapping state"
        );
    }
    let (_detached_summaries, ignored_ids) = ctx.chat_sessions.detach_for_chat_session_switch().await;
    ctx.ignored_session_events.extend(ignored_ids);
    ctx.session_rings.clear();
    ctx.reported_sessions.clear();
    ctx.announced_started_sessions.clear();
    ctx.last_sessions_summary.clear();
    ctx.last_sessions_entries.clear();
    *ctx.attached_follow = None;
    *ctx.attached_follow_seq = None;

    bind_session_to_runtime_provider_model(&mut loaded_session, ctx.provider_name, ctx.model_name);
    *ctx.chat_session = loaded_session;
    *ctx.chat_session_key = format!("chat:{}", ctx.chat_session.id);
    *ctx.fabric_turn_seq = ctx
        .chat_session
        .turns
        .iter()
        .filter(|turn| turn.role == "user")
        .fold(0_u64, |acc, _| acc.saturating_add(1));
    *ctx.history = history_for_session_with_system(
        ctx.chat_session,
        ctx.config,
        ctx.model_name,
        ctx.skills,
        ctx.native_tools,
    );

    let _ = ctx.chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SessionLoaded(ctx.chat_session.clone()),
        "chat.session_switch_loaded",
    );
    let _ = ctx.chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SessionFocusChanged {
            focus: crate::chat::sessions::FocusTarget::Main,
        },
        "chat.session_switch_focus_main",
    );
    let _ = ctx.chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ActiveSessionViewUpdated { view: None },
        "chat.session_switch_clear_active_session_view",
    );
    let _ = ctx.chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SessionsStatusUpdated { summary: String::new() },
        "chat.session_switch_clear_sessions_status",
    );
    let _ = ctx.chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SessionsEntriesUpdated { entries: Vec::new() },
        "chat.session_switch_clear_sessions_entries",
    );
    let _ = ctx.chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SwitcherClosed,
        "chat.session_switch_switcher_closed",
    );
    let _ = ctx.chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SavedSessionPickerClosed,
        "chat.session_switch_saved_session_picker_closed",
    );

    #[cfg(feature = "terminal-tui")]
    {
        let mut mirror = ctx.chat_mirror.lock();
        mirror.session_title = ctx.chat_session.title.clone();
        mirror.turn_count = ctx.chat_session.turn_count();
        mirror.chat_mode = ctx.chat_session.mode;
        mirror.autonomy_level = ctx.config.autonomy.level;
        mirror.conversation_lines = conversation_lines_for_resumed_session(ctx.chat_session);
        mirror.streaming = None;
        mirror.sessions_status.clear();
        mirror.sessions_cache.clear();
        mirror.active_session_view = None;
        mirror.pending_tool_approval = None;
        mirror.context_used_tokens = None;
        mirror.context_window_tokens = None;
        mirror.token_usage_summary = ctx.chat_session.token_usage_summary();
        mirror.external_editor_prefix_armed = false;
        mirror.input.clear_navigation_state();
        mirror.focus = crate::chat::sessions::FocusTarget::Main;
        mirror.switcher = None;
        mirror.saved_session_picker = None;
    }
    if let Some(tx) = ctx.redraw_handle {
        let _ = tx.try_send(());
    }
}

fn should_ignore_session_event_after_chat_resume(
    ignored_session_events: &std::collections::HashSet<crate::chat::sessions::id::SessionId>,
    event: &crate::chat::sessions::SessionEvent,
) -> bool {
    ignored_session_events.contains(event.session_id())
}

#[cfg(feature = "terminal-tui")]
fn conversation_lines_for_resumed_session(session: &session::ChatSession) -> Vec<tui::ConversationLine> {
    session
        .turns
        .iter()
        .filter_map(|turn| match turn.role.as_str() {
            "user" => Some(tui::ConversationLine::User {
                content: turn.content.clone(),
            }),
            "assistant" => Some(tui::ConversationLine::Assistant {
                content: turn.content.clone(),
            }),
            "system" => Some(tui::ConversationLine::System {
                content: turn.content.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// List all saved sessions.
async fn list_saved_sessions(mem: &dyn Memory) -> Result<()> {
    let entries = mem
        .list(Some(&MemoryCategory::Conversation), None)
        .await
        .map_err(|e| anyhow::anyhow!("failed to list saved sessions: {e}"))?;
    // C4: surface storage Err instead of unwrap_or_default printing a misleading
    // "No saved sessions"; corrupt exact entries propagate as Err (D10).
    let mut sessions: Vec<session::ChatSession> = collect_valid_sessions(&entries)?;
    sort_saved_sessions_newest_first(&mut sessions);

    if sessions.is_empty() {
        println!("No saved sessions.");
        return Ok(());
    }

    println!("Saved sessions:\n");
    for s in &sessions {
        let title = if s.title.is_empty() { "(untitled)" } else { &s.title };
        // B2 (v4 review): print the FULL session id. The previous 8-char
        // truncation could not be passed back to `--session`, which requires
        // the complete UUID — copy/pasting a listed id now resumes correctly.
        println!(
            "  {} | {} | {} turns | {}",
            s.id,
            title,
            s.turn_count(),
            s.updated_at.format("%Y-%m-%d %H:%M")
        );
    }
    println!("\nResume with: prx chat --session <ID>");
    Ok(())
}

// ─── S4-A Commit 4: RenderSource enum dual paths ─────────────────────────────

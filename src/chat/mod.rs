//! `prx chat` entry point — rich terminal interactive chat.
//!
//! Wires up the full agent pipeline (memory, tools, providers, security, hooks,
//! observability) and uses [`TerminalChannel`] for streaming I/O through the
//! event-driven UI Actor.
//!
//! ## S4-A 渲染源切换（已完成 2026-05-16）
//!
//! Pure 模式下 ratatui 渲染源从 `chat_mirror: Arc<Mutex<TuiState>>` 切换到
//! `tokio::sync::watch::Receiver<Arc<state::UiSnapshot>>`. dispatcher 在
//! reducer 返回 `ui_dirty=true` 后构造新 [`state::UiSnapshot`] 并 send_if_modified
//! 推送给 watch；`run_tui_unified_loop` 通过 [`RenderSource::Snapshot`] 从
//! receiver borrow 当前 snapshot，绕过 chat_mirror 锁。
//!
//! Off/Both/Redux 模式保留既有 `chat_mirror` 路径 ([`RenderSource::Mirror`])，
//! 让灰度切换可控。
//!
//! S4-A 已完成 commit: 327395d / 84ec8f1 / 0bb93bb / 55a2421 / 8d53140 /
//! ae3a9af / ae47ddd + 本 commit (Commit 7 docs)。S4-B 计划:
//! 删 chat_mirror 字段 + 所有 mirror 路径调用，参见任务文档附录 D
//! (`/opt/worker/task/prx/prx-remaining-plan-2026-05-15.md`)。
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
//! routing, security, hooks — via [`crate::agent::loop_::run_tool_call_loop`],
//! [`crate::agent::loop_::build_context_with_shared_events_and_scope`], and the
//! same `RuntimeEnvelope`-derived [`MemoryPrincipal`]/`MessageEventScope`
//! (see [`chat_runtime_envelope`]). Only the presentation/streaming stack stays
//! independent; the runtime semantics are shared with every other mode.
// Chat module: println!/eprintln! are intentional user-facing output (banners, status, errors).
#![allow(clippy::print_stdout, clippy::print_stderr)]

pub mod action;
pub mod commands;
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

use crate::agent::loop_::{
    DocumentIngestRuntime, ScopeContext, ToolCallNotification, ToolConcurrencyGovernanceConfig,
    apply_compaction_patch_exact, build_configurable_compaction_patch_with_source_history,
    build_context_with_shared_events_and_scope, build_runtime_system_prompt, increment_recalled_useful_counts,
    is_tool_loop_cancelled, measure_history_tokens, run_tool_call_loop_traced, select_prompt_skills,
};
use crate::approval::ApprovalManager;
use crate::channels::traits::extract_outgoing_media;
use crate::channels::{
    Channel, SendMessage, TerminalChannel, extract_tool_context_summary, is_context_window_overflow_error,
    sanitize_channel_response,
};
use crate::chat::terminal_proto::DraftVersionCounter;
use crate::config::Config;
use crate::hooks::{HookEvent, HookManager, payload_error};
use crate::llm::route_decision::{
    ProviderExecutionOutcome, RouteDecision, record_provider_outcome_events as record_raw_provider_outcome_events,
    record_route_decision_event as record_raw_route_decision_event, route_event_scope,
};
use crate::memory::{
    self, CompactionRunInput, Memory, MemoryCategory, MemoryFabric, MemoryPrincipal, MemoryStoreMetadata,
    MemoryVisibility, MessageEventScope,
};
use crate::observability::ObserverEvent;
use crate::providers::{self, ChatMessage, Provider};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::tools::Tool;
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
#[cfg(feature = "terminal-tui")]
use std::collections::VecDeque;
use std::io::{IsTerminal as _, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
#[cfg(feature = "terminal-tui")]
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Window (ms) for double Ctrl+C to trigger exit.
const DOUBLE_CTRLC_WINDOW_MS: u64 = 500;

/// Max retries on context window overflow (compact history + retry).
const MAX_CONTEXT_OVERFLOW_RETRIES: usize = 2;

/// Keep last N non-system messages during history compaction.
const COMPACT_KEEP_MESSAGES: usize = 8;

/// Per-message character limit during compaction.
const COMPACT_CONTENT_CHARS: usize = 320;

/// Total character budget for compacted history (excluding system prompt).
const COMPACT_TOTAL_CHARS: usize = 2400;

/// Capacity for the user-input mpsc channel.
const INPUT_CHANNEL_CAPACITY: usize = 16;
const CHAT_CONTROL_CHANNEL_CAPACITY: usize = 8;
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
    outcome: Option<dispatcher::TurnOutcomeKind>,
    usage: crate::llm::route_decision::TokenUsage,
}

#[derive(Debug)]
enum ProviderTurnCompletionRoute {
    Current(ProviderTurnCompletionEvent),
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProviderTurnCompletionContext {
    history_len_before_assistant: usize,
}

#[cfg(feature = "terminal-tui")]
struct PerTurnContext {
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
}

#[cfg(feature = "terminal-tui")]
struct PendingOrderedProviderTurnCommit {
    context: PerTurnContext,
    terminal_plan: ProviderTurnTerminalPlan,
}

#[derive(Debug)]
struct ResolvedProviderTurnCompletion {
    outcome: Option<dispatcher::TurnOutcomeKind>,
    usage: crate::llm::route_decision::TokenUsage,
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
struct ProviderTurnFinalizerEvent {
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    plan: ProviderTurnTerminalPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Minimum base timeout (seconds) for per-turn timeout budget.
const TIMEOUT_MIN_BASE_SECS: u64 = 30;

/// Bounded grace period for reducer-owned persistence to finish before exit.
const EXIT_PERSISTENCE_DRAIN_GRACE_MS: u64 = 250;

/// Extra idle settle window after the persistence guard is inactive.
const EXIT_PERSISTENCE_IDLE_SETTLE_MS: u64 = 50;

/// Maximum multiplier applied to the base timeout (caps iterations-based scaling).
const TIMEOUT_MAX_SCALE_FACTOR: u64 = 4;

pub(crate) fn turn_timeout_budget(message_timeout_secs: u64, max_tool_iterations: usize) -> Duration {
    let base = message_timeout_secs.max(TIMEOUT_MIN_BASE_SECS);
    let scale = (max_tool_iterations.max(1) as u64).min(TIMEOUT_MAX_SCALE_FACTOR);
    Duration::from_secs(base.saturating_mul(scale))
}

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

fn mouse_capture_disabled_by_env(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

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

fn manual_compact_below_trigger_threshold(
    history: &[ChatMessage],
    compaction_config: &crate::config::AgentCompactionConfig,
) -> bool {
    !crate::agent::loop_::plan_context_budget(
        history,
        compaction_config,
        crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD,
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
struct TuiContextBudgetStatus {
    used_context_tokens: usize,
    max_context_tokens: usize,
}

fn context_budget_status_for_tui(
    history: &[ChatMessage],
    compaction_config: &crate::config::AgentCompactionConfig,
    terminal_tui_enabled: bool,
) -> Option<TuiContextBudgetStatus> {
    if !terminal_tui_enabled {
        return None;
    }
    let budget = crate::agent::loop_::plan_context_budget(
        history,
        compaction_config,
        crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD,
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

async fn build_chat_compaction_patch_with_timeout(
    budget_history: &[ChatMessage],
    source_history: &[ChatMessage],
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &str,
    timeout_duration: Duration,
) -> Option<crate::agent::loop_::CompactionPatch> {
    match tokio::time::timeout(
        timeout_duration,
        build_configurable_compaction_patch_with_source_history(
            budget_history,
            source_history,
            provider,
            model,
            config,
            audit,
            trigger,
        ),
    )
    .await
    {
        Ok(Ok(patch)) => patch,
        Ok(Err(error)) => {
            tracing::warn!(%error, trigger, "chat summary compaction failed; falling back to deterministic trim");
            None
        }
        Err(_) => {
            tracing::warn!(
                ?timeout_duration,
                trigger,
                "chat summary compaction timed out; falling back to deterministic trim"
            );
            None
        }
    }
}

fn apply_chat_compaction_patch_and_sync(
    history: &mut Vec<ChatMessage>,
    patch_source_history: Option<&[ChatMessage]>,
    patch: crate::agent::loop_::CompactionPatch,
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
    let budget =
        crate::agent::loop_::plan_context_budget(history, config, crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD);
    if budget.over_hard_limit {
        let trimmed = crate::agent::loop_::trim_history_to_context_budget_preserving_compaction_replacement_with_floor(
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
    let provenance = match crate::agent::loop_::resolve_compaction_event_provenance(
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
    let source_event_ids_json = provenance
        .as_ref()
        .and_then(|provenance| serde_json::to_string(&provenance.source_event_ids).ok());
    let source_event_range_json = provenance
        .as_ref()
        .and_then(|provenance| serde_json::to_string(&provenance.covered_range).ok());
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
            "fidelity_status": "accepted_legacy_deterministic",
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
            source_document_refs_json: None,
            fidelity_status: "accepted_legacy_deterministic".to_string(),
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

fn print_fallback_chat_output(text: &str) {
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
            Some(format_managed_sessions_list(&views))
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
                    Ok(()) => format!("Killed interactive PTY session #{seq} (process group terminated)."),
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

#[cfg(test)]
mod session_cleanup_tests {
    use super::*;
    use crate::chat::sessions::id::SessionId;
    use crate::chat::sessions::model::ManagedKind;
    use crate::chat::sessions::runtime::{ReapPolicy, ReapedSession};

    fn reaped(seq: u64, id: &SessionId, now: chrono::DateTime<chrono::Utc>) -> ReapedSession {
        ReapedSession {
            id: id.clone(),
            seq,
            kind: ManagedKind::Agent,
            summary: crate::chat::sessions::PersistedSessionSummary {
                id: id.as_str().to_string(),
                seq,
                kind: "agent".to_string(),
                origin: "user".to_string(),
                status: "completed".to_string(),
                title: "archived task".to_string(),
                summary: "persisted compact summary".to_string(),
                token_usage_records: Vec::new(),
                created_at: now - chrono::Duration::minutes(20),
            },
            terminal_at: now - chrono::Duration::minutes(20),
            reaped_at: now,
        }
    }

    #[test]
    fn logs_archive_returns_full_in_window_then_reaped_notice() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-05T12:00:00Z")
            .expect("test timestamp")
            .with_timezone(&chrono::Utc);
        let id = SessionId::from_run_id("archived-run");
        let mut ring = crate::chat::sessions::SessionRing::with_capacity(10);
        ring.push("full retained output".to_string());
        let mut rings = std::collections::HashMap::from([(id.clone(), ring)]);
        let mut policy = ReapPolicy::default();
        policy.archive_keep_last = 0;
        let mut archive = ReapedSessionLogArchive::default();

        archive.archive_reaped(&[reaped(7, &id, now)], &mut rings, &policy, now);

        let live_window = archive
            .logs_message(7, &policy, now + chrono::Duration::minutes(5))
            .expect("archive hit in window");
        assert!(live_window.contains("Session #7 logs"));
        assert!(live_window.contains("full retained output"));

        let expired = archive
            .logs_message(7, &policy, now + chrono::Duration::minutes(11))
            .expect("compact reaped notice after archive expiry");
        assert!(expired.contains("was reaped"));
        assert!(expired.contains("persisted summary"));
        assert!(expired.contains("persisted compact summary"));
        assert!(
            !expired.contains("full retained output"),
            "expired archive should not dump old full logs"
        );
    }

    #[test]
    fn quit_and_exit_are_shutdown_commands() {
        assert!(is_chat_quit_command("/quit"));
        assert!(is_chat_quit_command("/exit"));
        assert!(!is_chat_quit_command("/quit now"));
        assert!(!is_chat_quit_command(" /quit"));
    }

    #[tokio::test]
    async fn exit_shutdown_helper_calls_shutdown_all_and_records_interrupted_summary() {
        let run = crate::tools::sessions_spawn::SubAgentRun {
            id: "quit-live-agent".to_string(),
            task: "still running".to_string(),
            owner_id: None,
            topic_id: None,
            source_message_event_id: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
            status: crate::tools::sessions_spawn::SubAgentStatus::Running,
            recipient: None,
            channel_name: None,
            abort_handle: None,
            process_control: None,
            history: std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())),
            steer_tx: None,
            parent_run_id: None,
            session_scope_key: String::new(),
            spawn_depth: 0,
            token_usage_records: Vec::new(),
        };
        let runs = std::sync::Arc::new(tokio::sync::RwLock::new(vec![run]));
        let mut chat_sessions = crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::clone(&runs));
        let mut chat_session = session::ChatSession::new("provider", "model");
        let (dispatcher, _rx) = dispatcher::ChatDispatcher::new();

        let report = shutdown_child_sessions_for_exit(&mut chat_sessions, &mut chat_session, &dispatcher).await;

        assert_eq!(report.summaries.len(), 1);
        assert!(
            runs.read().await.is_empty(),
            "/quit exit helper must clear live agent registry"
        );
        assert_eq!(chat_session.background_sessions.len(), 1);
        assert_eq!(
            chat_session
                .background_sessions
                .first()
                .map(|summary| summary.status.as_str()),
            Some(crate::chat::sessions::model::STATUS_INTERRUPTED)
        );
    }
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

#[cfg(all(test, feature = "terminal-tui"))]
mod transcript_selection_tests {
    use super::slice_display_columns;

    #[test]
    fn slice_display_columns_uses_terminal_width() {
        assert_eq!(slice_display_columns("○ line-1", 2, 6), "line");
        assert_eq!(slice_display_columns("中文 line", 0, 4), "中文");
    }
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

#[cfg_attr(not(feature = "terminal-tui"), allow(dead_code))]
fn surface_active_turn_message(
    dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
    text: &str,
) {
    surface_session_message(dispatcher, redraw_tx, text);
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

#[cfg_attr(not(feature = "terminal-tui"), allow(dead_code))]
fn format_turn_elapsed_message(
    status: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    finished_at: chrono::DateTime<chrono::Utc>,
) -> String {
    let elapsed = crate::chat::sessions::model::format_elapsed_compact(
        crate::chat::sessions::model::elapsed_seconds_between(started_at, finished_at),
    );
    format!("turn {status} {elapsed}")
}

#[cfg_attr(not(feature = "terminal-tui"), allow(unused_variables))]
fn turn_elapsed_message_for_surface(
    redraw_tx: Option<&mpsc::Sender<()>>,
    status: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    finished_at: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    redraw_tx?;
    Some(format_turn_elapsed_message(status, started_at, finished_at))
}

#[cfg_attr(not(feature = "terminal-tui"), allow(unused_variables))]
fn surface_turn_elapsed_message(
    dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: Option<&mpsc::Sender<()>>,
    status: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    finished_at: chrono::DateTime<chrono::Utc>,
) {
    #[cfg(feature = "terminal-tui")]
    {
        let Some(text) = turn_elapsed_message_for_surface(redraw_tx, status, started_at, finished_at) else {
            return;
        };
        surface_session_message(dispatcher, redraw_tx, &text);
    }
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

#[cfg(test)]
mod fallback_chat_output_tests {
    use super::*;

    #[test]
    fn fallback_chat_output_preserves_lf_for_piped_stdout() {
        assert_eq!(format_fallback_chat_output_for("a\nb", false), "a\nb\n");
    }

    #[test]
    fn fallback_chat_output_uses_crlf_for_terminal_stdout() {
        assert_eq!(format_fallback_chat_output_for("a\nb", true), "a\r\nb\r\n");
    }
}

#[cfg(test)]
mod runtime_display_tests {
    use super::*;
    use crate::chat::sessions::id::SessionId;
    use crate::chat::sessions::model::{ManagedKind, ManagedSessionView, ManagedStatus, SessionOrigin};
    use crate::chat::sessions::runtime::FinishedSession;

    fn ts(value: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(value)
            .expect("test timestamp")
            .with_timezone(&chrono::Utc)
    }

    fn usage_record(
        source: crate::llm::route_decision::TokenUsageSource,
    ) -> crate::llm::route_decision::MeteredTokenUsageRecord {
        crate::llm::route_decision::MeteredTokenUsageRecord {
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            prompt_tokens: 8_000,
            completion_tokens: 4_300,
            total_tokens: 12_300,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            source,
            cost_usd: Some(0.0042),
        }
    }

    #[cfg(feature = "terminal-tui")]
    fn switcher_entry(seq: u64) -> crate::chat::sessions::SwitcherEntry {
        crate::chat::sessions::SwitcherEntry {
            seq,
            kind: ManagedKind::Agent.as_str(),
            origin: SessionOrigin::User.as_str(),
            status: ManagedStatus::Running.as_str(),
            title: format!("session {seq}"),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
        }
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn sessions_tick_helper_refreshes_session_cache() {
        let mut mirror = tui::TuiState::new("provider", "model");

        refresh_sessions_cache(&mut mirror, vec![switcher_entry(1), switcher_entry(2)]);

        assert_eq!(mirror.sessions_cache.len(), 2);
        assert_eq!(mirror.sessions_cache.first().map(|entry| entry.seq), Some(1));
        assert_eq!(mirror.sessions_cache.get(1).map(|entry| entry.seq), Some(2));
    }

    #[test]
    fn sessions_list_rows_include_elapsed() {
        let view = ManagedSessionView {
            id: SessionId::from_run_id("run-elapsed"),
            seq: 9,
            kind: ManagedKind::Agent,
            origin: SessionOrigin::Model,
            title: "index workspace".to_string(),
            status: ManagedStatus::Running,
            created_at: ts("2026-07-04T12:00:00Z"),
            updated_at: ts("2026-07-04T12:00:03Z"),
            token_usage_records: Vec::new(),
        };

        let out = format_managed_sessions_list(&[view]);

        assert!(out.contains("#9 agent model running 3s index workspace"), "{out}");
    }

    #[test]
    fn started_session_announcement_promotes_model_agent_to_child_session() {
        let view = ManagedSessionView {
            id: SessionId::from_run_id("run-started"),
            seq: 2,
            kind: ManagedKind::Agent,
            origin: SessionOrigin::Model,
            title: "audit queued input".to_string(),
            status: ManagedStatus::Running,
            created_at: ts("2026-07-04T12:00:00Z"),
            updated_at: ts("2026-07-04T12:00:03Z"),
            token_usage_records: Vec::new(),
        };

        assert_eq!(
            format_observed_session_announcement(&view),
            "[agent #2 model running 3s] started: audit queued input"
        );
    }

    #[test]
    fn observed_session_announcement_covers_fast_completed_model_agent() {
        let view = ManagedSessionView {
            id: SessionId::from_run_id("run-fast"),
            seq: 3,
            kind: ManagedKind::Agent,
            origin: SessionOrigin::Model,
            title: "fast reply".to_string(),
            status: ManagedStatus::Completed,
            created_at: ts("2026-07-04T12:00:00Z"),
            updated_at: ts("2026-07-04T12:00:01Z"),
            token_usage_records: Vec::new(),
        };

        assert_eq!(
            format_observed_session_announcement(&view),
            "[agent #3 model completed 1s] observed: fast reply"
        );
    }

    #[test]
    fn sessions_list_rows_include_reported_usage() {
        let view = ManagedSessionView {
            id: SessionId::from_run_id("run-metered"),
            seq: 9,
            kind: ManagedKind::Agent,
            origin: SessionOrigin::Model,
            title: "index workspace".to_string(),
            status: ManagedStatus::Completed,
            created_at: ts("2026-07-04T12:00:00Z"),
            updated_at: ts("2026-07-04T12:00:03Z"),
            token_usage_records: vec![usage_record(crate::llm::route_decision::TokenUsageSource::Reported)],
        };

        let out = format_managed_sessions_list(&[view]);

        assert!(
            out.contains("#9 agent model completed 3s 12.3k tok | $0.0042 index workspace"),
            "{out}"
        );
    }

    #[test]
    fn sessions_list_marks_estimated_usage() {
        let mut view = ManagedSessionView {
            id: SessionId::from_run_id("run-estimated"),
            seq: 10,
            kind: ManagedKind::Agent,
            origin: SessionOrigin::User,
            title: "summarize".to_string(),
            status: ManagedStatus::Completed,
            created_at: ts("2026-07-04T12:00:00Z"),
            updated_at: ts("2026-07-04T12:00:03Z"),
            token_usage_records: vec![usage_record(crate::llm::route_decision::TokenUsageSource::Estimated)],
        };
        if let Some(record) = view.token_usage_records.first_mut() {
            record.cost_usd = None;
        }

        let out = format_managed_sessions_list(&[view]);

        assert!(
            out.contains("#10 agent user completed 3s ~12.3k tok | cost unknown summarize"),
            "{out}"
        );
    }

    #[test]
    fn copy_command_selects_latest_and_numbered_assistant_turns() {
        let mut session = session::ChatSession::new("provider", "model");
        session.add_user_turn("question");
        session.add_assistant_turn("first **markdown**", Vec::new());
        session.add_user_turn("again");
        session.add_assistant_turn("second `raw`", Vec::new());

        let latest = select_copy_content(&session, "/copy").expect("latest copy");
        assert_eq!(latest.content, "second `raw`");
        assert_eq!(latest.ordinal, 1);
        assert!(!latest.truncated);

        let latest_named = select_copy_content(&session, "/copy latest").expect("named latest copy");
        assert_eq!(latest_named.content, "second `raw`");
        assert_eq!(latest_named.ordinal, 1);

        let previous = select_copy_content(&session, "/copy 2").expect("previous copy");
        assert_eq!(previous.content, "first **markdown**");
        assert_eq!(previous.ordinal, 2);
    }

    #[test]
    fn copy_command_validates_args_and_truncates_for_osc52() {
        let mut session = session::ChatSession::new("provider", "model");
        assert_eq!(
            select_copy_content(&session, "/copy").expect_err("no assistant"),
            "No assistant response to copy."
        );
        assert_eq!(
            select_copy_content(&session, "/copy nope").expect_err("usage"),
            "Usage: /copy [latest|N]"
        );

        let oversized = format!("{}界", "x".repeat(COPY_OSC52_MAX_BYTES));
        session.add_assistant_turn(&oversized, Vec::new());
        let selected = select_copy_content(&session, "/copy").expect("copy oversized");
        assert!(selected.truncated);
        assert!(selected.content.len() <= COPY_OSC52_MAX_BYTES);
        assert!(selected.content.is_char_boundary(selected.content.len()));
        assert!(
            !selected.content.ends_with('界'),
            "truncate at UTF-8 boundary before wide char"
        );
    }

    #[test]
    fn mouse_capture_disable_env_parser_is_explicit() {
        for enabled in ["1", "true", "TRUE", "yes", "on", " on "] {
            assert!(mouse_capture_disabled_by_env(Some(enabled)), "{enabled:?}");
        }
        for disabled in [None, Some(""), Some("0"), Some("false"), Some("off"), Some("no")] {
            assert!(!mouse_capture_disabled_by_env(disabled), "{disabled:?}");
        }
        assert!(mouse_capture_enabled_by_env(None));
        assert!(mouse_capture_enabled_by_env(Some("0")));
        assert!(!mouse_capture_enabled_by_env(Some("true")));
    }

    #[test]
    fn completion_announcement_includes_final_elapsed() {
        let fin = FinishedSession {
            seq: 4,
            run_id: "run-finished".to_string(),
            kind: ManagedKind::Agent,
            origin: SessionOrigin::User,
            status: ManagedStatus::Completed,
            summary: "done".to_string(),
            created_at: ts("2026-07-04T12:00:00Z"),
            updated_at: ts("2026-07-04T12:01:03Z"),
            token_usage_records: Vec::new(),
        };

        assert_eq!(format_finished_session_announcement(&fin), "[agent #4 ✓ 1m03s] done");
    }

    #[test]
    fn completion_announcement_includes_elapsed_and_tokens() {
        let fin = FinishedSession {
            seq: 4,
            run_id: "run-finished".to_string(),
            kind: ManagedKind::Agent,
            origin: SessionOrigin::User,
            status: ManagedStatus::Completed,
            summary: "done".to_string(),
            created_at: ts("2026-07-04T12:00:00Z"),
            updated_at: ts("2026-07-04T12:01:03Z"),
            token_usage_records: vec![usage_record(crate::llm::route_decision::TokenUsageSource::Reported)],
        };

        assert_eq!(
            format_finished_session_announcement(&fin),
            "[agent #4 ✓ 1m03s · 12.3k tok | $0.0042] done"
        );
    }

    #[test]
    fn agent_completion_announcement_uses_only_first_useful_line() {
        let fin = FinishedSession {
            seq: 4,
            run_id: "run-finished".to_string(),
            kind: ManagedKind::Agent,
            origin: SessionOrigin::User,
            status: ManagedStatus::Completed,
            summary: "\n```text\nfirst useful line\nfull output remains in child session".to_string(),
            created_at: ts("2026-07-04T12:00:00Z"),
            updated_at: ts("2026-07-04T12:01:03Z"),
            token_usage_records: Vec::new(),
        };

        assert_eq!(
            format_finished_session_announcement(&fin),
            "[agent #4 ✓ 1m03s] first useful line"
        );
        assert!(fin.summary.contains("full output remains in child session"));
    }

    #[test]
    fn agent_completion_announcement_clamps_summary_to_120_chars() {
        let fin = FinishedSession {
            seq: 4,
            run_id: "run-finished".to_string(),
            kind: ManagedKind::Agent,
            origin: SessionOrigin::User,
            status: ManagedStatus::Completed,
            summary: "x".repeat(CHILD_COMPLETION_SUMMARY_MAX_CHARS + 20),
            created_at: ts("2026-07-04T12:00:00Z"),
            updated_at: ts("2026-07-04T12:01:03Z"),
            token_usage_records: Vec::new(),
        };

        let out = format_finished_session_announcement(&fin);
        let summary = out.split_once("] ").map_or("", |(_, summary)| summary);

        assert_eq!(summary.chars().count(), CHILD_COMPLETION_SUMMARY_MAX_CHARS);
        assert!(summary.ends_with('…'), "{out}");
    }

    #[test]
    fn failed_completion_announcement_uses_cross_and_compact_reason() {
        let fin = FinishedSession {
            seq: 4,
            run_id: "run-finished".to_string(),
            kind: ManagedKind::Agent,
            origin: SessionOrigin::User,
            status: ManagedStatus::Failed,
            summary: "provider failed\nstack trace should stay in child logs".to_string(),
            created_at: ts("2026-07-04T12:00:00Z"),
            updated_at: ts("2026-07-04T12:01:03Z"),
            token_usage_records: Vec::new(),
        };

        assert_eq!(
            format_finished_session_announcement(&fin),
            "[agent #4 ✗ 1m03s] provider failed"
        );
    }

    #[test]
    fn shell_completion_announcement_stays_compact_without_tokens() {
        let fin = FinishedSession {
            seq: 5,
            run_id: "run-shell".to_string(),
            kind: ManagedKind::Shell,
            origin: SessionOrigin::User,
            status: ManagedStatus::Completed,
            summary: "exit 0\nstdout body".to_string(),
            created_at: ts("2026-07-04T12:00:00Z"),
            updated_at: ts("2026-07-04T12:00:03Z"),
            token_usage_records: vec![usage_record(crate::llm::route_decision::TokenUsageSource::Reported)],
        };

        let out = format_finished_session_announcement(&fin);

        assert_eq!(out, "[shell #5 ✓ 3s] exit 0");
        assert!(!out.contains("tok"), "{out}");
    }

    #[test]
    fn session_logs_render_full_retained_output() {
        let lines = vec![
            "first retained line".to_string(),
            "second retained line\nthird retained line".to_string(),
        ];

        let out = format_session_logs(4, &lines, true);

        assert!(out.contains("Session #4 logs (last 2 lines):"));
        assert!(out.contains("[output truncated]"));
        assert!(out.contains("first retained line"));
        assert!(out.contains("second retained line"));
        assert!(out.contains("third retained line"));
    }

    #[test]
    fn main_turn_elapsed_message_uses_compact_runtime() {
        assert_eq!(
            format_turn_elapsed_message("completed", ts("2026-07-04T12:00:00Z"), ts("2026-07-04T12:00:03Z")),
            "turn completed 3s"
        );
    }

    #[test]
    fn plain_mode_suppresses_turn_elapsed_chrome() {
        assert_eq!(
            turn_elapsed_message_for_surface(
                None,
                "completed",
                ts("2026-07-04T12:00:00Z"),
                ts("2026-07-04T12:00:03Z")
            ),
            None,
            "plain/fallback path must not surface turn completed chrome"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn surface_turn_elapsed_message_dispatches_system_message_and_redraw() {
        let (dispatcher, mut action_rx) = dispatcher::ChatDispatcher::new();
        let (redraw_tx, mut redraw_rx) = mpsc::channel(1);

        surface_turn_elapsed_message(
            &dispatcher,
            Some(&redraw_tx),
            "completed",
            ts("2026-07-04T12:00:00Z"),
            ts("2026-07-04T12:00:03Z"),
        );

        assert!(redraw_rx.try_recv().is_ok(), "surface path should request redraw");
        let action = action_rx
            .try_recv()
            .expect("surface path should dispatch system message");
        assert!(
            matches!(
                action,
                crate::chat::action::Action::SystemMessageAdded { ref text } if text == "turn completed 3s"
            ),
            "unexpected action: {action:?}"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn p4a_active_turn_message_uses_reducer_redraw_path() {
        let (dispatcher, mut action_rx) = dispatcher::ChatDispatcher::new();
        let (redraw_tx, mut redraw_rx) = mpsc::channel(1);

        surface_active_turn_message(&dispatcher, Some(&redraw_tx), "Main input queue: 0 queued, 0 priority.");

        assert!(
            redraw_rx.try_recv().is_ok(),
            "active-turn local output should request redraw"
        );
        let action = action_rx
            .try_recv()
            .expect("active-turn local output should dispatch through reducer");
        assert!(
            matches!(
                action,
                crate::chat::action::Action::SystemMessageAdded { ref text }
                    if text == "Main input queue: 0 queued, 0 priority."
            ),
            "unexpected action: {action:?}"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn p4a_resume_saved_session_is_deferred_while_provider_turn_pending() {
        let mut deferred = std::collections::VecDeque::new();

        assert!(defer_resume_saved_session_if_provider_turn_pending(
            1,
            &mut deferred,
            "session-a".to_string()
        ));
        assert_eq!(deferred.pop_front().as_deref(), Some("session-a"));

        assert!(!defer_resume_saved_session_if_provider_turn_pending(
            0,
            &mut deferred,
            "session-b".to_string()
        ));
        assert!(
            deferred.is_empty(),
            "no pending turn means resume should be handled immediately"
        );
    }
}

#[cfg(test)]
mod compact_command_tests {
    //! Bug #1: `/compact` manual compaction reuses `compact_chat_history`. These
    //! tests pin the routine the slash command drives so the user-visible turn /
    //! token delta is meaningful.
    use super::*;

    fn long_user(seq: usize) -> ChatMessage {
        // Each turn is well above COMPACT_CONTENT_CHARS so truncation + drop both fire.
        ChatMessage::user(format!("turn-{seq}-{}", "x".repeat(COMPACT_CONTENT_CHARS * 2)))
    }

    #[test]
    fn compact_drops_old_turns_beyond_keep_window() {
        let mut history = vec![ChatMessage::system("sys")];
        for i in 0..(COMPACT_KEEP_MESSAGES + 6) {
            history.push(long_user(i));
        }
        let turns_before = history.len() - 1;
        let tokens_before = estimate_chat_history_tokens(&history);

        compact_chat_history(&mut history);

        let turns_after = history.len() - 1;
        let tokens_after = estimate_chat_history_tokens(&history);

        // System prompt always preserved at index 0.
        assert_eq!(history.first().map(|m| m.role.as_str()), Some("system"));
        // Manual /compact must actually shrink an over-long history.
        assert!(turns_after < turns_before, "compact should drop old turns");
        assert!(tokens_after < tokens_before, "compact should reduce token estimate");
        // Never exceeds the keep window after compaction.
        assert!(turns_after <= COMPACT_KEEP_MESSAGES, "must respect keep window");
    }

    #[test]
    fn compact_is_noop_for_short_history() {
        let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("hi")];
        let before = estimate_chat_history_tokens(&history);
        compact_chat_history(&mut history);
        let after = estimate_chat_history_tokens(&history);
        // A 1-turn history is already compact — nothing to drop.
        assert_eq!(history.len(), 2);
        assert_eq!(before, after);
    }

    #[test]
    fn compact_command_reports_window_and_reclaim_delta() {
        let text = format_compact_feedback(20, 12, 10_000, 4_000, 1_000_000);

        assert!(text.contains("20 -> 12 turns"), "turn delta missing: {text}");
        assert!(text.contains("~10000 -> ~4000 tokens"), "token delta missing: {text}");
        assert!(text.contains("1M window"), "model window missing: {text}");
        assert!(
            text.contains("reclaimed ~6000 tokens (60%)"),
            "reclaim delta missing: {text}"
        );
    }

    #[test]
    fn compact_command_below_trigger_threshold_reports_noop() {
        let config = crate::config::AgentCompactionConfig {
            reserve_tokens: 10,
            max_context_tokens: 10_000,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let history = vec![ChatMessage::system("sys"), ChatMessage::user("short prompt")];

        assert!(
            manual_compact_below_trigger_threshold(&history, &config),
            "short history should not fall through to deterministic /compact trimming"
        );
        let feedback = format_nothing_to_compact_feedback(&history, &config);
        assert!(feedback.contains("Nothing to compact"), "{feedback}");
        assert!(feedback.contains("1 turns"), "{feedback}");
    }

    #[test]
    fn legacy_preflight_compaction_feedback_emits_system_message() {
        let config = crate::config::AgentCompactionConfig {
            reserve_tokens: 10,
            max_context_tokens: 10_000,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let mut history = vec![ChatMessage::system("sys")];
        for i in 0..(COMPACT_KEEP_MESSAGES + 4) {
            history.push(long_user(i));
        }
        let system_count = usize::from(history.first().is_some_and(|message| message.role == "system"));
        let turns_before = history.len().saturating_sub(system_count);
        let tokens_before = estimate_chat_history_tokens(&history);
        compact_chat_history(&mut history);
        let text = format_compact_feedback_after_history(turns_before, tokens_before, &history, &config);
        let (dispatcher, mut action_rx) = dispatcher::ChatDispatcher::new();

        surface_session_message(&dispatcher, None, &text);

        let action = action_rx
            .try_recv()
            .expect("legacy preflight feedback should dispatch a system message");
        assert!(
            matches!(action, crate::chat::action::Action::SystemMessageAdded { text: ref actual } if actual == &text),
            "unexpected feedback action: {action:?}"
        );
    }

    struct SlowSummaryProvider;

    #[async_trait::async_trait]
    impl Provider for SlowSummaryProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok("## Decisions\nslow summary".to_string())
        }
    }

    struct ImmediateSummaryProvider;

    #[async_trait::async_trait]
    impl Provider for ImmediateSummaryProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(
                "## Decisions\n- PERSISTED_GUARD_SUMMARY\n## Open TODOs\n- continue\n## Critical Context\n- persisted source"
                    .to_string(),
            )
        }
    }

    #[tokio::test]
    async fn configurable_summary_compaction_timeout_returns_none_for_fallback() {
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Safeguard,
            reserve_tokens: 0,
            keep_recent_messages: 1,
            memory_flush: false,
            max_context_tokens: 16,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("older ".repeat(400)),
            ChatMessage::assistant("middle ".repeat(400)),
            ChatMessage::user("recent ".repeat(40)),
        ];

        let patch = build_chat_compaction_patch_with_timeout(
            &history,
            &history,
            &SlowSummaryProvider,
            "slow-model",
            &config,
            None,
            "test_timeout",
            Duration::from_millis(1),
        )
        .await;

        assert!(
            patch.is_none(),
            "timeout must let caller fall back to deterministic trim"
        );
    }

    #[tokio::test]
    async fn legacy_compaction_with_tool_message_uses_persisted_guard_for_reducer() {
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Safeguard,
            reserve_tokens: 0,
            keep_recent_messages: 1,
            memory_flush: false,
            max_context_tokens: 400,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let system_message = ChatMessage::system("sys");
        let old_user_message = ChatMessage::user(format!("old persisted user {}", "u ".repeat(220)));
        let old_assistant_message = ChatMessage::assistant(format!("old persisted assistant {}", "a ".repeat(220)));
        let latest_user_message = ChatMessage::user("latest visible question");
        let persisted_history = vec![
            system_message.clone(),
            old_user_message.clone(),
            old_assistant_message.clone(),
            latest_user_message,
        ];
        let enriched_history = vec![
            system_message,
            old_user_message,
            old_assistant_message,
            ChatMessage::assistant(format!("[tool:shell]\n{}", "tool output ".repeat(320))),
            ChatMessage::user("[Memory context]\nlatest visible question"),
        ];

        let patch = build_chat_compaction_patch_with_timeout(
            &enriched_history,
            &persisted_history,
            &ImmediateSummaryProvider,
            "summary-model",
            &config,
            None,
            "test_persisted_guard",
            Duration::from_secs(1),
        )
        .await
        .expect("budget-overflowing enriched history should produce a persisted-source patch");

        assert!(
            crate::agent::loop_::compaction_patch_guard_matches(&persisted_history, &patch.guard),
            "patch guard must match the persisted reducer mirror"
        );
        assert!(
            !crate::agent::loop_::compaction_patch_guard_matches(&enriched_history, &patch.guard),
            "pre-fix enriched guard source would miss once tool/internal messages exist"
        );

        let mut legacy_history = enriched_history;
        let (dispatcher, mut action_rx) = dispatcher::ChatDispatcher::new();
        apply_chat_compaction_patch_and_sync(
            &mut legacy_history,
            Some(&persisted_history),
            patch,
            &config,
            crate::chat::action::CompactReason::ContextOverflow,
            &dispatcher,
        );
        let action = action_rx
            .try_recv()
            .expect("compaction helper should dispatch reducer patch");

        let mut reducer_state = crate::chat::state::ChatState::new(
            std::sync::Arc::from("provider"),
            std::sync::Arc::from("model"),
            tokio_util::sync::CancellationToken::new(),
        );
        reducer_state.session.history = persisted_history;
        let _ = reducer_state.reduce(action);

        let legacy_pairs: Vec<_> = legacy_history
            .iter()
            .map(|message| (message.role.as_str(), message.content.as_str()))
            .collect();
        let reducer_pairs: Vec<_> = reducer_state
            .session
            .history
            .iter()
            .map(|message| (message.role.as_str(), message.content.as_str()))
            .collect();
        assert_eq!(
            legacy_pairs, reducer_pairs,
            "legacy driver and reducer mirror must converge on the persisted-source compaction patch"
        );
        assert!(
            legacy_history
                .iter()
                .any(|message| message.content.contains("PERSISTED_GUARD_SUMMARY")),
            "provider summary should survive the reducer guard"
        );
        assert!(
            legacy_history
                .iter()
                .all(|message| !message.content.contains("[tool:shell]")),
            "persisted-source compaction must not keep enriched-only tool/internal messages"
        );
    }
}

/// Chat 输入路径的运行模式.
///
/// v0.4.1 清理后，terminal TUI 只支持 reducer/driver 单路由。旧的 Off/Both/Redux
/// 灰度模式已在 v0.4.0 验收后退役；`PRX_CHAT_REDUX` 仍会被读取一次用于告警，
/// 但不会再改变运行路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReduxMode {
    Pure,
}

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

    #[must_use]
    pub(crate) const fn is_pure(self) -> bool {
        true
    }
}

/// chat::run 主循环 LLM turn 路由结果.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Off / TUI 关闭场景下未必引用所有 variant
pub(crate) enum TurnRoute {
    /// Non-TUI fallback still uses the shared agent loop.
    LegacyToolLoop,
    /// Terminal TUI uses reducer/driver single route.
    ReduxDriver,
}

#[cfg(feature = "terminal-tui")]
#[must_use]
pub(crate) const fn route_turn(_mode: ReduxMode) -> TurnRoute {
    TurnRoute::ReduxDriver
}

#[cfg(not(feature = "terminal-tui"))]
#[must_use]
#[allow(dead_code)]
pub(crate) const fn route_turn(_mode: ()) -> TurnRoute {
    TurnRoute::LegacyToolLoop
}

/// P1-2: Both 模式下的累计差异计数器.
///
/// 每次 `log_redux_key_diff` 检测到旧路径 dispatch 与新路径 Effect 存在语义差异时 += 1.
/// 测试可通过 [`redux_diff_count`] 查询该值，断言双写期行为一致（期望 0）。
#[cfg(feature = "terminal-tui")]
static REDUX_DIFF_COUNT: AtomicU64 = AtomicU64::new(0);

/// 查询 Both 模式下累计的对账差异次数（供测试断言用）.
#[cfg(feature = "terminal-tui")]
#[allow(dead_code)]
pub fn redux_diff_count() -> u64 {
    REDUX_DIFF_COUNT.load(Ordering::Relaxed)
}

/// 重置对账差异计数器（测试间隔离用）.
#[cfg(feature = "terminal-tui")]
#[allow(dead_code)]
pub fn reset_redux_diff_count() {
    REDUX_DIFF_COUNT.store(0, Ordering::Relaxed);
}

/// Both 模式下记录旧路径 dispatch 与 reducer Effect 列表的差异（tracing::debug + 计数器）.
///
/// 用于 Step 2 双写期对账：若关键控制流（Quit / Submit）在两侧产生不同输出，
/// 该日志能在 PTY 测试日志里高亮出来，同时 `REDUX_DIFF_COUNT` += 1 供测试断言。
/// Step 5 删除旧路径后移除。
///
/// P2-5: 补充字段级比对——检测 Quit 语义差异（旧路径 Exit vs 新路径 Quit）和
/// Submitted 语义差异（旧路径 Submitted vs 新路径含 LogTrace）。
#[cfg(feature = "terminal-tui")]
#[allow(dead_code)]
fn log_redux_key_diff(old: &tui::KeyDispatch, new_effects: &[state::Effect]) {
    use state::Effect;
    let old_kind = match old {
        tui::KeyDispatch::Submitted(_) => "Submitted",
        tui::KeyDispatch::Exit => "Exit",
        tui::KeyDispatch::InterruptTurn => "InterruptTurn",
        tui::KeyDispatch::Cancelled => "Cancelled",
        tui::KeyDispatch::Consumed => "Consumed",
        tui::KeyDispatch::Ignored => "Ignored",
        // v1.1b switcher/focus control flow — treated as consumed-equivalent for
        // the legacy redux-diff comparison (they neither submit nor quit).
        tui::KeyDispatch::SwitcherOpened { .. } => "SwitcherOpened",
        tui::KeyDispatch::SwitcherMoved { .. } => "SwitcherMoved",
        tui::KeyDispatch::SwitcherClosed => "SwitcherClosed",
        tui::KeyDispatch::SavedSessionPickerMoved { .. } => "SavedSessionPickerMoved",
        tui::KeyDispatch::SavedSessionPickerClosed => "SavedSessionPickerClosed",
        tui::KeyDispatch::ResumeSavedSession { .. } => "ResumeSavedSession",
        tui::KeyDispatch::AttachSession { .. } => "AttachSession",
        tui::KeyDispatch::RequestDetach => "RequestDetach",
        tui::KeyDispatch::ScrollTranscriptUp => "ScrollTranscriptUp",
        tui::KeyDispatch::ScrollTranscriptDown => "ScrollTranscriptDown",
        tui::KeyDispatch::PageTranscriptUp => "PageTranscriptUp",
        tui::KeyDispatch::PageTranscriptDown => "PageTranscriptDown",
        tui::KeyDispatch::TranscriptHome => "TranscriptHome",
        tui::KeyDispatch::TranscriptEnd => "TranscriptEnd",
        tui::KeyDispatch::ScrollSessionUp => "ScrollSessionUp",
        tui::KeyDispatch::ScrollSessionDown => "ScrollSessionDown",
        tui::KeyDispatch::PageSessionUp => "PageSessionUp",
        tui::KeyDispatch::PageSessionDown => "PageSessionDown",
        tui::KeyDispatch::SessionHome => "SessionHome",
        tui::KeyDispatch::SessionEnd => "SessionEnd",
        tui::KeyDispatch::SwitchSession { .. } => "SwitchSession",
        tui::KeyDispatch::OpenTranscriptViewer => "OpenTranscriptViewer",
        tui::KeyDispatch::CloseTranscriptViewer => "CloseTranscriptViewer",
        tui::KeyDispatch::OpenProviderWorkerView { .. } => "OpenProviderWorkerView",
        tui::KeyDispatch::CloseProviderWorkerView => "CloseProviderWorkerView",
        tui::KeyDispatch::CloseDiffViewer => "CloseDiffViewer",
        tui::KeyDispatch::ExternalEditorRequested => "ExternalEditorRequested",
        tui::KeyDispatch::ToolApprovalDecision { .. } => "ToolApprovalDecision",
        tui::KeyDispatch::ModeChanged(_) => "ModeChanged",
    };
    let new_kinds: Vec<&'static str> = new_effects
        .iter()
        .map(|e| match e {
            Effect::Quit => "Quit",
            Effect::RequestRedraw => "RequestRedraw",
            Effect::LogTrace { .. } => "LogTrace",
            Effect::StartTurn { .. } => "StartTurn",
            Effect::SaveSession(_) => "SaveSession",
            Effect::SendDraftFinalize { .. } => "SendDraftFinalize",
            Effect::CancelDraft(_) => "CancelDraft",
            Effect::CancelToken(_) => "CancelToken",
            Effect::EmitChannelMessage(_) => "EmitChannelMessage",
            Effect::PersistToMemory { .. } => "PersistToMemory",
            Effect::NotifyHook { .. } => "NotifyHook",
            Effect::DisplayMedia { .. } => "DisplayMedia",
            Effect::AutoTitleSession(_) => "AutoTitleSession",
            Effect::RequestApproval { .. } => "RequestApproval",
            Effect::ResolveApproval { .. } => "ResolveApproval",
        })
        .collect();

    // P1-2 + P2-5: 字段级语义差异检测——比对关键控制流分类是否一致.
    // 差异定义：
    //   1. 旧路径 Exit（Ctrl+D 空 buffer）≠ 新路径无 Quit
    //   2. 旧路径无 Exit，但新路径有 Quit（reducer 检测到双 Ctrl+C 或 Ctrl+D）
    //   3. 旧路径 Submitted，但新路径无 LogTrace（InputSubmitted 路径未触发）
    let new_has_quit = new_effects.iter().any(|e| matches!(e, Effect::Quit));
    let new_has_log_trace = new_effects.iter().any(|e| matches!(e, Effect::LogTrace { .. }));

    let is_diff = match old {
        tui::KeyDispatch::Exit => !new_has_quit,
        tui::KeyDispatch::Submitted(_) => !new_has_log_trace,
        // Ctrl+C → InterruptTurn in old path; new path either returns [] or Quit.
        // 只在新路径意外产生 Quit（但旧路径没有 Exit 语义）时记为差异.
        tui::KeyDispatch::InterruptTurn
        | tui::KeyDispatch::Cancelled
        | tui::KeyDispatch::Consumed
        | tui::KeyDispatch::Ignored
        | tui::KeyDispatch::SwitcherOpened { .. }
        | tui::KeyDispatch::SwitcherMoved { .. }
        | tui::KeyDispatch::SwitcherClosed
        | tui::KeyDispatch::SavedSessionPickerMoved { .. }
        | tui::KeyDispatch::SavedSessionPickerClosed
        | tui::KeyDispatch::ResumeSavedSession { .. }
        | tui::KeyDispatch::AttachSession { .. }
        | tui::KeyDispatch::RequestDetach
        | tui::KeyDispatch::ScrollTranscriptUp
        | tui::KeyDispatch::ScrollTranscriptDown
        | tui::KeyDispatch::PageTranscriptUp
        | tui::KeyDispatch::PageTranscriptDown
        | tui::KeyDispatch::TranscriptHome
        | tui::KeyDispatch::TranscriptEnd
        | tui::KeyDispatch::ScrollSessionUp
        | tui::KeyDispatch::ScrollSessionDown
        | tui::KeyDispatch::PageSessionUp
        | tui::KeyDispatch::PageSessionDown
        | tui::KeyDispatch::SessionHome
        | tui::KeyDispatch::SessionEnd
        | tui::KeyDispatch::SwitchSession { .. }
        | tui::KeyDispatch::OpenTranscriptViewer
        | tui::KeyDispatch::CloseTranscriptViewer
        | tui::KeyDispatch::OpenProviderWorkerView { .. }
        | tui::KeyDispatch::CloseProviderWorkerView
        | tui::KeyDispatch::CloseDiffViewer
        | tui::KeyDispatch::ExternalEditorRequested
        | tui::KeyDispatch::ToolApprovalDecision { .. }
        | tui::KeyDispatch::ModeChanged(_) => new_has_quit,
    };

    if is_diff {
        REDUX_DIFF_COUNT.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            old_dispatch = old_kind,
            new_effects = ?new_kinds,
            diff_count = REDUX_DIFF_COUNT.load(Ordering::Relaxed),
            "redux:both SEMANTIC DIFF detected"
        );
    } else {
        tracing::debug!(
            old_dispatch = old_kind,
            new_effects = ?new_kinds,
            "redux:both ok"
        );
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
    chat_session_key: &str,
    chat_run_id: &str,
    provider_name: &str,
    model_name: &str,
) -> MessageEventScope {
    let chat_session_id = chat_session_id_from_key(chat_session_key);
    RuntimeEnvelope::chat_canonical("workspace", chat_session_id, MemoryVisibility::Workspace)
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

fn chat_runtime_write_context(envelope: &RuntimeEnvelope) -> crate::memory::principal::MemoryWriteContext {
    envelope.memory_write_context("private")
}

fn chat_runtime_principal(envelope: &RuntimeEnvelope) -> MemoryPrincipal {
    envelope.memory_principal()
}

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
    memory_fabric
        .record_inbound_user_message(
            chat_message_event_scope(chat_session_key, chat_run_id, provider_name, model_name),
            sanitize::sanitize_for_persistence(user_input),
            Some(format!("chat:{}:{chat_run_id}:{turn_seq}:user", chat_session.id)),
            None,
        )
        .await
}

async fn record_chat_assistant_message_event(
    memory_fabric: &MemoryFabric,
    chat_session_key: &str,
    chat_run_id: &str,
    provider_name: &str,
    model_name: &str,
    response: &str,
) -> anyhow::Result<crate::memory::MessageEvent> {
    memory_fabric
        .record_assistant_message(
            chat_message_event_scope(chat_session_key, chat_run_id, provider_name, model_name)
                .with_sender(format!("{provider_name}/{model_name}"))
                .with_recipient("local-user"),
            sanitize::sanitize_for_persistence(response),
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

async fn record_provider_outcome_events(
    fabric: &MemoryFabric,
    scope: MessageEventScope,
    outcome: &ProviderExecutionOutcome,
) -> anyhow::Result<()> {
    let sanitized = sanitize::sanitize_json_structure(outcome)?;
    record_raw_provider_outcome_events(fabric, scope, &sanitized).await
}

fn sanitize_chat_semantic_memory_content(content: &str) -> String {
    sanitize::sanitize_for_persistence(content)
}

/// Aggregate the model's reasoning/thinking content from the turn's history
/// slice. The agent loop encodes assistant turns that carried reasoning as a
/// JSON object containing `{"reasoning_content": "..."}` (see
/// `build_native_assistant_history` in `agent/loop_.rs`). We pull those out
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
    fn enable_raw_mode(&mut self) -> std::io::Result<()>;
    fn disable_raw_mode(&mut self) -> std::io::Result<()>;
    fn supports_keyboard_enhancement(&mut self) -> std::io::Result<bool>;
    fn push_keyboard_enhancement_flags(&mut self) -> std::io::Result<()>;
    fn pop_keyboard_enhancement_flags(&mut self) -> std::io::Result<()>;
    fn enable_bracketed_paste(&mut self) -> std::io::Result<()>;
    fn disable_bracketed_paste(&mut self) -> std::io::Result<()>;
    fn enable_mouse_capture(&mut self) -> std::io::Result<()>;
    fn disable_mouse_capture(&mut self) -> std::io::Result<()>;
    fn enter_alternate_screen(&mut self) -> std::io::Result<()>;
    fn leave_alternate_screen(&mut self) -> std::io::Result<()>;
    fn show_cursor(&mut self) -> std::io::Result<()>;
}

struct CrosstermTerminalModeOps;

impl TerminalModeOps for CrosstermTerminalModeOps {
    fn enable_raw_mode(&mut self) -> std::io::Result<()> {
        crossterm::terminal::enable_raw_mode()
    }

    fn disable_raw_mode(&mut self) -> std::io::Result<()> {
        crossterm::terminal::disable_raw_mode()
    }

    fn supports_keyboard_enhancement(&mut self) -> std::io::Result<bool> {
        crossterm::terminal::supports_keyboard_enhancement()
    }

    fn push_keyboard_enhancement_flags(&mut self) -> std::io::Result<()> {
        let flags = crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | crossterm::event::KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            | crossterm::event::KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            | crossterm::event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES;
        crossterm::execute!(std::io::stdout(), crossterm::event::PushKeyboardEnhancementFlags(flags)).map(|_| ())
    }

    fn pop_keyboard_enhancement_flags(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::PopKeyboardEnhancementFlags).map(|_| ())
    }

    fn enable_bracketed_paste(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste).map(|_| ())
    }

    fn disable_bracketed_paste(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste).map(|_| ())
    }

    fn enable_mouse_capture(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture).map(|_| ())
    }

    fn disable_mouse_capture(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture).map(|_| ())
    }

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
fn enter_terminal_state_with_ops(ops: &mut impl TerminalModeOps) -> std::io::Result<TerminalGuardState> {
    let mouse_enabled = mouse_capture_enabled_by_env(std::env::var("PRX_TUI_DISABLE_MOUSE").ok().as_deref());
    enter_terminal_state_with_ops_inner(ops, mouse_enabled)
}

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

/// RAII guard owning the `tracing_appender` non-blocking worker. When dropped
/// it:
///   1. swaps the global tracing writer back to stderr (best-effort), so
///      any post-chat logs (e.g. shutdown errors) remain visible;
///   2. drops `_worker_guard`, which flushes and joins the appender thread.
///
/// Held for the lifetime of `chat::run`. If construction fails we keep the
/// existing stderr writer — no panics, no silent data loss.
pub(crate) struct TracingChatGuard {
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
        // _worker_guard drops next → tracing-appender flushes pending lines.
    }
}

/// Compute `~/.openprx/` (or `$HOME/.openprx/`) for the chat log directory.
fn resolve_chat_log_dir() -> Result<std::path::PathBuf> {
    if let Some(dirs) = directories::UserDirs::new() {
        return Ok(dirs.home_dir().join(".openprx"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(std::path::PathBuf::from(home).join(".openprx"));
    }
    anyhow::bail!("cannot determine home directory for chat.log")
}

/// Redirect the global `tracing` writer to `~/.openprx/chat.log` so
/// `INFO`/`WARN`/`ERROR` lines never collide with the ratatui TUI.
///
/// Returns a guard that MUST be held for the lifetime of the chat session.
/// On any failure (no HOME, dir not creatable, file not openable, no global
/// reload handle) returns `Err` without panicking — callers fall back to the
/// existing stderr writer.
pub(crate) fn setup_chat_tracing_to_file() -> Result<TracingChatGuard> {
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

    Ok(TracingChatGuard {
        _worker_guard: worker_guard,
    })
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
    let _tracing_guard: Option<TracingChatGuard> = match setup_chat_tracing_to_file() {
        Ok(g) => Some(g),
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
    // hooks are not part of AppContext — keep the local construction unchanged.
    let hooks = Arc::new(HookManager::new(config.workspace_dir.clone()));

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
    let provider_name = provider_override
        .as_deref()
        .or(config.default_provider.as_deref())
        .unwrap_or("openrouter");

    let model_name = model_override
        .as_deref()
        .or(config.default_model.as_deref())
        .unwrap_or("anthropic/claude-sonnet-4");
    providers::validate_provider_model(provider_name, model_name)?;

    let provider_runtime_options = providers::provider_runtime_options_from_config(&config);

    let provider: Arc<dyn Provider> = Arc::from(providers::create_routed_provider_with_options(
        provider_name,
        config.api_key.as_deref(),
        config.api_url.as_deref(),
        &config.reliability,
        &config.model_routes,
        model_name,
        &provider_runtime_options,
    )?);

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
    let skill_embedder = memory::create_embedder_from_config(&config, config.api_key.as_deref());
    let mut skills = crate::skills::load_skills_with_config(&config.workspace_dir, &config);
    if config.skill_rag.enabled {
        crate::skills::hydrate_skill_embeddings(&mut skills, skill_embedder.as_ref()).await?;
    }

    // ── Tool descriptions for system prompt ──────────────────────
    let tool_descs: Vec<(&str, &str)> = vec![
        ("shell", "Execute terminal commands."),
        ("file_read", "Read file contents."),
        ("file_write", "Write file contents."),
        ("memory_store", "Save to memory."),
        ("memory_recall", "Search memory."),
        ("memory_forget", "Delete a memory entry."),
        (
            "chat_schedule",
            "Schedule a future message back into the current chat main session for dispatcher self-wake observation.",
        ),
    ];
    let native_tools = provider.supports_native_tools();

    // ── Approval manager ─────────────────────────────────────────
    let approval_manager = ApprovalManager::from_config(&config.autonomy);

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
        config.agents.clone(),
        config.api_key.clone(),
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
    .with_shared_memory(Arc::clone(&mem))
    .with_event_recording(config.memory.event_recording_config())
    .with_event_sink(session_event_sink.into_spawn_sink())
    .with_approval_resolver_factory(approval_resolver_factory);
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
        security.clone(),
        managed_shell_registry,
        shell_event_sink.clone(),
    )));
    base_tools_vec.push(Box::new(crate::chat::scheduled_input::ScheduledInputTool::new(
        scheduled_input_handle.clone(),
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
    if spawn_tools_handle.set(Arc::clone(&tools_registry)).is_err() {
        tracing::warn!(
            "sessions_spawn tools registry was already initialized; spawned sub-agents may have an incomplete tool set"
        );
    }

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
    let banner = format!(
        "prx {} \u{00B7} {provider_name}/{model_name}",
        env!("CARGO_PKG_VERSION")
    );

    // ── Conversation history ─────────────────────────────────────
    let mut history = history_for_session_with_system(
        &chat_session,
        &config,
        model_name,
        &tool_descs,
        &skills,
        native_tools,
        &tools_registry,
    );

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
    let (control_tx, mut control_rx) = mpsc::channel(CHAT_CONTROL_CHANNEL_CAPACITY);
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
    // 全局 dispatcher channel + EffectExecutor + ChatState 在此构造。
    // EffectExecutor 模式由 `PRX_CHAT_REDUX` env 决定：
    //   - Off (默认)：shadow 模式，业务 Effect 全部 no-op；旧路径单写
    //   - Both：real 模式，业务 Effect 真执行 + 旧路径仍跑；dual_write_guard
    //     在 reducer 持久化 effect 后置位，旧路径检查 guard 跳过对应写
    //   - Redux：与 Both 行为相同（5a-1 阶段不删旧路径；5a-3 才真正删旧路径）
    //
    // bounded(2048)：覆盖典型 chat session 的 Action 流（用户输入 + 流式 chunk +
    // 工具事件 + 信号），同时在反压时通过 [`StreamChunkCoalescer`] 合并 delta，
    // 避免无界增长导致 OOM。
    let (chat_dispatcher, chat_action_rx) = dispatcher::ChatDispatcher::new();
    let mut dispatcher_shadow_state =
        state::ChatState::new(Arc::from(provider_name), Arc::from(model_name), shutdown.clone());
    if chat_session.turn_count() > 0 {
        let _ = dispatcher_shadow_state.reduce(crate::chat::action::Action::SessionLoaded(chat_session.clone()));
    }
    #[cfg(feature = "terminal-tui")]
    {
        dispatcher_shadow_state.ui.chat_mode = chat_session.mode;
        dispatcher_shadow_state.ui.autonomy_level = config.autonomy.level;
        dispatcher_shadow_state.ui.provider_model_catalog = tui::slash_provider_model_catalog_from_config(&config);
        dispatcher_shadow_state.ui.saved_sessions_cache = initial_saved_session_entries.clone();
    }

    // 共享 dual-write guard（在 Both/Redux 模式下被 EffectExecutor 置位；旧路径
    // 检查 guard 决定是否跳过持久化。即使 Off 模式也构造，旧路径检查总是 false 零开销。
    // P0-1 fix: 去掉 allow(unused_variables)，guard 在旧路径 turn 结束时被读取，
    // 两种 feature 配置下都确保真正使用）
    let dual_write_guard = dispatcher::RuntimeDualWriteGuard::new();

    // 入口统一读 PRX_CHAT_REDUX，函数体内复用此值避免多点解析环境变量
    // S4-B: Pure 是唯一支持的运行路径；非 Pure 值 warning 后强制升级
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

    let (provider_turn_lifecycle_tx, mut provider_turn_lifecycle_rx) =
        mpsc::unbounded_channel::<dispatcher::ProviderTurnLifecycleEvent>();
    #[cfg(not(feature = "terminal-tui"))]
    let _ = &provider_turn_lifecycle_tx;

    // 根据 redux mode 选择 EffectExecutor 模式（TUI feature only）
    #[cfg(feature = "terminal-tui")]
    let effect_executor = {
        let mode = top_redux_mode;
        let deps = dispatcher::EffectDeps {
            provider: Arc::clone(&provider),
            memory: Arc::clone(&mem),
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
            max_tool_iterations: config.agent.max_tool_iterations,
            turn_timeout_budget: Some(turn_timeout_budget(
                config.channels_config.message_timeout_secs,
                config.agent.max_tool_iterations,
            )),
            approval_router: Arc::new(dispatcher::ApprovalRouter::new()),
            approval_manager: Some(Arc::new(ApprovalManager::from_config(&config.autonomy))),
        };
        tracing::info!(mode = ?mode, "chat EffectExecutor in Pure real-deps mode");
        dispatcher::EffectExecutor::new_with_deps(deps)
    };
    #[cfg(not(feature = "terminal-tui"))]
    let effect_executor = dispatcher::EffectExecutor::new_shadow();

    // P0-2 fix: 提前获取 redraw_slot Arc，用于在 TUI 初始化完成后后注入 redraw_tx。
    // EffectExecutor 被 spawn_dispatcher_task_with_executor 消费，但 Arc 在 spawn
    // 前复制出来，spawn 后仍可通过此 Arc 填入真实 sender，让 RequestRedraw 真执行。
    #[cfg(feature = "terminal-tui")]
    let executor_redraw_slot = effect_executor.redraw_handle();

    // BUG-07: 提前取出 model 热替换 slot 句柄（同 redraw_slot 的思路：spawn 前 clone
    // 出来，spawn 后仍可通过此句柄在 `/model <name>` 时替换 model，使后续 turn 的
    // drive_start_turn_stream 读到新值）。shadow 模式无 deps → None。
    #[cfg(feature = "terminal-tui")]
    let model_slot = effect_executor.model_handle();

    // Bug #3: provider 热替换 slot 句柄（同 model_slot 思路）。spawn 前 clone 出来，
    // `/provider <name>` 时把重建出的新 provider 句柄 set 进去，使后续 turn 的
    // Redux driver（drive_start_turn_stream）读到新 provider。shadow 模式无 deps → None。
    #[cfg(feature = "terminal-tui")]
    let provider_slot = effect_executor.provider_handle();

    #[cfg(feature = "terminal-tui")]
    let approval_router = effect_executor.approval_router();
    #[cfg(not(feature = "terminal-tui"))]
    let approval_router: Option<Arc<dispatcher::ApprovalRouter>> = None;

    // Step 5a-4: TurnCompletionSignal — Redux driver 切闸路径用此 signal 在
    // chat::run 主循环里 await turn 完成。dispatcher task 消费 terminal action
    // (StreamCompleted/Failed/Cancelled) 后 notify_waiters，唤醒等待。
    // Off / legacy 路径不读 signal，构造成本极低（Arc<Notify>）。
    let turn_signal = dispatcher::TurnCompletionSignal::new();

    // S4-A Commit 3: 构造 watch::channel<Arc<UiSnapshot>>，dispatcher
    // 在 ui_dirty=true 时推送新 snapshot；TUI render and child views consume
    // this reducer-owned snapshot as the primary UI source, with chat_mirror kept
    // only for synchronous key-thread compatibility and fallback.
    //
    // rx 在 Commit 4 接入 run_tui_unified_loop；本 commit 仅 trace 观察推送频率，
    // rx 保留为 `Option` 留给 spawn_tui_unified_loop 使用。
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
    // Commit 4: snapshot_rx_for_tui 传给 run_tui_unified_loop（见 TUI 分支 spawn_tui_unified_loop 调用）.

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
                    // S4-B: 删除 chat_mirror 旁路写，Pure 模式下 reducer 单源接管 banner
                    // S2-C Step 3: 双写到 Redux UI 镜像。Off/Both/Redux 下 chat_mirror
                    // 仍是 TUI 渲染源（本 dispatch 仅供 Redux 路径维护一致的 UI 账本
                    // + 测试断言）；Pure 模式下这是 reducer 单源唯一入口.
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::SystemMessageAdded { text: banner.clone() },
                        "chat.banner",
                    );

                    // The redraw channel exists solely so the UiActor and
                    // background tasks can wake the unified loop on
                    // streaming events. cap=1 + try_send is the coalesce
                    // idiom: bursts collapse into a single deferred redraw.
                    let (redraw_tx, redraw_rx) = mpsc::channel::<()>(1);

                    // P0-2 fix: 将 redraw_tx 后注入 EffectExecutor 的 redraw_slot。
                    // EffectExecutor 已被 dispatcher task 消费，但通过提前保存的
                    // executor_redraw_slot Arc 可跨越 spawn 边界填入真实 sender，
                    // 从而让 RequestRedraw effect 真正触发重绘。
                    *executor_redraw_slot.lock() = Some(redraw_tx.clone());
                    tracing::debug!("P0-2: redraw_tx injected into EffectExecutor redraw_slot");

                    // S4-B: 删除 TuiStateMirrorSink 路径，Pure 模式统一用 SnapshotDispatcherSink
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
                    // S4-A Commit 4: 把 snapshot_rx 传给 unified loop，让其从
                    // watch::Receiver borrow reducer-owned snapshot 替代
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
    // Step 5b 双写：每次 Ctrl+C 在旧路径 cancel/shutdown 之外，同步 try_dispatch
    // `CancelRequested` / `ShutdownRequested` 给 dispatcher（shadow 模式下仅入 reducer
    // + log，不参与真实控制流）。try_send 满或 closed 都不影响旧路径兜底。
    //
    // shutdown 触发时 handler 也需要退出，避免持有 dispatcher sender 阻塞
    // dispatcher task 退出（drop(chat_dispatcher) + 此 handler 内的 clone 同时
    // drop，channel 才能真正关闭，dispatcher_handle.await 才能返回）。
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
                    // Step 5b shadow: 同步投递 ShutdownRequested.
                    let _ = dispatcher_for_signal.dispatch_or_log(
                        crate::chat::action::Action::ShutdownRequested,
                        "chat.shutdown_double_ctrlc",
                    );
                    shutdown_signal.cancel();
                    break;
                }

                // Single Ctrl+C → cancel active generation if any
                // Step 5b shadow: 同步投递 CancelRequested 给 reducer 观察。
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
    // Step 5b 双写：投递 ShutdownRequested 给 dispatcher（shadow 观察），同时
    // 调用 shutdown.cancel() 兜底（旧路径退出协议保留）。
    // shutdown 触发时此任务也要主动退出，避免持有 sender clone 阻塞 dispatcher
    // task 关闭。
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
                            // 主路径已 shutdown，无需再触发；退出释放 sender clone。
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

    // BUG-07: 当前生效的 model 名（owned，可变）。`/model <name>` 在线切换时改写
    // 此值；每轮循环顶部把它借为 `model_name: &str` 供后续 turn 使用（system prompt /
    // fabric 事件 / snapshot）。初值与启动期解析出的 `model_name` 一致。
    let mut current_model_owned: String = model_name.to_string();

    // Bug #3: 当前生效的 provider 名（owned，可变）。`/provider <name>` 在线切换时
    // 改写此值；每轮循环顶部借为 `provider_name: &str`，覆盖后续 turn 的 provider 使用点
    // （system prompt / fabric 事件 / snapshot / legacy run_tool_call_loop）。初值与启动期
    // 解析出的 `provider_name` 一致。
    let mut current_provider_owned: String = provider_name.to_string();
    // Bug #3: 启动期 primary provider 名（owned，不可变）。`/provider` 切换时据此
    // 判断是否切回原 primary（决定是否复用 `config.api_key`/`config.api_url`）。
    let original_provider_name: String = provider_name.to_string();
    // Bug #3: provider 句柄（legacy 路径 run_tool_call_loop 直接 `provider.as_ref()`）。
    // `/provider <name>` 时用新 provider 重建并替换此 Arc，同步 set 进 provider_slot（Redux 路径）。
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
    if !chat_session.background_sessions.is_empty() {
        let recap = format_reloaded_background_sessions(&chat_session.background_sessions);
        surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &recap);
    }

    // ── Main message loop ────────────────────────────────────────
    //
    // The inner `loop` lets a timer tick do background-session work (summary
    // reflow + status-line refresh) without producing a message or ending the
    // outer loop: it only `break`s with a real input message (or `None` on
    // shutdown). On a tick we poll the registry and surface results via the
    // dispatcher (single source, reaches both render paths), then keep waiting.
    while let Some(input) = loop {
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
                        tool_descs: &tool_descs,
                        skills: &skills,
                        native_tools,
                        tools_registry: &tools_registry,
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
                }
                break None
            },
            maybe_control = control_rx.recv(), if control_events_open => {
                let Some(control) = maybe_control else {
                    control_events_open = false;
                    continue;
                };
                match control {
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
                    }
                }
                continue;
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
                            tool_descs: &tool_descs,
                            skills: &skills,
                            native_tools,
                            tools_registry: &tools_registry,
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
        let provider_queue_task_id = input.turn_task_id;
        let input_priority = input.priority;
        let msg = input.msg;
        let user_input = msg.content.clone();
        let synthetic_ui_command = is_synthetic_ui_command(&msg);

        // Bug #3: 本轮生效的 provider 名（借自可变 owned 值）。`/provider <name>`
        // 拦截会改写 `current_provider_owned` + `provider` Arc，下一轮迭代此 shadow
        // 即指向新 provider 名，覆盖后续所有 `provider_name` 使用点（含 `/model`
        // 校验 / system prompt / fabric / legacy run_tool_call_loop）。
        let provider_name: &str = current_provider_owned.as_str();

        // Step 5b 双写：每条用户输入入 dispatcher（shadow 观察 reducer）。
        // InputSubmitted 仅记 UI/LogTrace；RecordUserTurn 真写 history + session.turns，
        // 必须在 mem_context 注入后才 dispatch（用 `enriched` 与 legacy `history.push`
        // 字节级对齐 — 见 S2-B Step 4 risk notes）.
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
            // S4-B: 删除 legacy mirror push，reducer 单源 UserMessageEchoed
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
            // S4-B: 删除 mirror 旁路写，reducer 单源 SystemMessageAdded
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

        // BUG-07: `/model <name>` 在线切换 model（同 provider 换 model）。
        //
        // 在 commands::dispatch 之前拦截，因为真正生效需要 (a) 改写主循环
        // `current_model_owned`（影响后续 turn 的 system prompt / 事件记录），
        // (b) 写 EffectDeps 的热替换 slot（影响 dispatcher 子任务下一 turn 实际请求
        // 的 model），(c) dispatch ModelChanged 让 reducer 更新 session.model →
        // status bar 立即反映。bare `/model`（无参）仍交给 dispatch 显示当前 model。
        if let Some(raw) = user_input.strip_prefix("/model ") {
            let new_model = raw.trim();
            if new_model.is_empty() {
                emit_chat_output("Usage: /model <name>");
                continue;
            }
            match providers::validate_provider_model(provider_name, new_model) {
                Ok(()) => {
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
                        "Switched model to {new_model} (provider {provider_name}). Applies from the next turn."
                    ));
                }
                Err(e) => {
                    emit_chat_output(&format!("Cannot switch to '{new_model}': {e}"));
                }
            }
            continue;
        }

        // Bug #3: `/provider <name> [model]` — 会话内热切换 provider。
        //
        // 与 `/model` 同样在 commands::dispatch 之前拦截，因为生效需要在主循环侧
        // 重建 provider 实例并改写多处运行时状态：
        //   (a) 用新 provider 的 auth/base/protocol 重建 `Arc<dyn Provider>`，替换
        //       legacy `run_tool_call_loop` 直接持有的 `provider` 句柄；
        //   (b) `set()` 进 `provider_slot`（Redux driver 子任务下一 turn 读到新 provider）；
        //   (c) 改写 `current_provider_owned`（影响后续 turn 的 system prompt / 事件 /
        //       snapshot），并校验当前 model 对新 provider 有效（无效则要求随命令带上
        //       一个兼容 model：`/provider <name> <model>`）。
        // 凭据解析：切到非启动 primary 的 provider 时传 `api_key=None`/`api_url=None`，
        // 让 provider 自行从 auth profile / 环境解析其凭据（沿用启动期 `config.api_key`
        // 只对原 primary 有意义）；切回原 primary 时复用 `config.api_key`/`config.api_url`。
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
            // 决定切换后生效的 model：优先用命令显式给的；否则沿用当前 model（若兼容）。
            let candidate_model = requested_model.unwrap_or_else(|| current_model_owned.clone());
            if let Err(e) = providers::validate_provider_model(&new_provider, &candidate_model) {
                emit_chat_output(&format!(
                    "Cannot switch to provider '{new_provider}': model '{candidate_model}' is incompatible ({e}). \
Retry with a compatible model: /provider {new_provider} <model>"
                ));
                continue;
            }
            // 切到非原 primary 的 provider 时，不沿用 primary 的显式凭据/URL，让新 provider
            // 自行解析（避免把 A provider 的 key 错喂给 B provider）。
            let is_original_primary = new_provider.eq_ignore_ascii_case(original_provider_name.as_str());
            let switch_api_key = if is_original_primary {
                config.api_key.as_deref()
            } else {
                None
            };
            let switch_api_url = if is_original_primary {
                config.api_url.as_deref()
            } else {
                None
            };
            match providers::create_routed_provider_with_options(
                &new_provider,
                switch_api_key,
                switch_api_url,
                &config.reliability,
                &config.model_routes,
                &candidate_model,
                &provider_runtime_options,
            ) {
                Ok(built) => {
                    let new_provider_arc: Arc<dyn Provider> = Arc::from(built);
                    // (a) legacy 路径句柄
                    provider = Arc::clone(&new_provider_arc);
                    // (b) Redux driver slot
                    #[cfg(feature = "terminal-tui")]
                    if let Some(slot) = provider_slot.as_ref() {
                        slot.set(Arc::clone(&new_provider_arc));
                    }
                    // (c) 运行时 provider / model 名
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
                    // (d) session 账本：dispatch ProviderChanged，reducer 更新
                    // session.provider（必要时连带 session.model），使 status bar /
                    // UI snapshot 实时反映新 provider。三处（legacy provider 句柄、
                    // Redux provider_slot、session 账本）由此保持一致。换 provider 时
                    // model 若同时变了，一并放进同一个 action（无需单独 ModelChanged）。
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
                        "Switched provider to {new_provider}{model_note}. Applies from the next turn."
                    ));
                }
                Err(e) => {
                    emit_chat_output(&format!("Cannot switch to provider '{new_provider}': {e}"));
                }
            }
            continue;
        }

        // BUG-07: 本轮生效的 model 名（借自可变 owned 值）。`/model` 拦截已在上方
        // 处理并 `continue`，故此处 shadow 后的 `model_name` 一定是最新值，覆盖
        // 后续所有 `model_name` 使用点（system prompt / fabric / snapshot）。
        let model_name: &str = current_model_owned.as_str();

        // BUG-04: `!cmd` bang mode — run the rest of the line directly as a
        // shell command (matching the footer hint "! for bash") instead of
        // sending it to the LLM. Output is shown inline; the LLM is not
        // involved. A bare `!` is ignored. The shell tool already applies the
        // sandbox + workspace cwd, so bang commands share the same host FS view
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

        // Handle /clear separately (needs mutable history)
        if matches!(user_input.as_str(), "/clear" | "/new") {
            history.clear();
            // S2-C Step 4: 双写 HistoryCleared 到 reducer。reducer 的语义是
            // "drain 所有非 system + 保留 system"——legacy 是先 clear 再可能 push
            // system（仅当 !skill_rag.enabled），最终态都是 "system only"（或空，
            // 当 skill_rag.enabled 时）。双写期两路径终态一致，但中间状态不同：
            //   - legacy: clear() 把 history 清空 → 可能 push system
            //   - reducer: HistoryCleared 保留已有 system（不重新构造）
            // 实际生产路径 legacy 后续会 push 新构造的 system（覆盖旧 system 的
            // skill 列表），reducer 这边的 system 仍是上一轮的。本 S2-C 阶段
            // 不做修正——legacy 仍是 LLM 真上下文源，reducer 是观察账本。
            if !config.skill_rag.enabled {
                let cleared_system = build_runtime_system_prompt(
                    &config,
                    model_name,
                    &tool_descs,
                    &skills,
                    native_tools,
                    &tools_registry,
                );
                history.push(ChatMessage::system(cleared_system.clone()));
                // S2-C Step 4 (Codex P0 修正): 用 SetLeadingSystemPrompt 而非
                // RecordSystemMessage。reducer 的 HistoryCleared 是 "drain 非 system
                // 保留 system" — 之前的 system 仍在；若此处用 RecordSystemMessage
                // (append) 会产生重复 system，长期累计多条。SetLeadingSystemPrompt
                // 是 upsert：替换已有首位 system 或 push 到空 history，与 legacy
                // `clear + push` 终态等价（≤ 1 条 system）。
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
            if let Some(patch) = build_chat_compaction_patch_with_timeout(
                &history,
                &compact_source_history,
                provider.as_ref(),
                model_name,
                &compact_context.config,
                None,
                "chat_manual_compact",
                Duration::from_secs(crate::agent::loop_::COMPACTION_TIMEOUT_SECS),
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
                    // Pure 跳过 legacy chat_session.set_mode；legacy 模式下 run_tool_call_loop 仍读
                    let _ = chat_dispatcher
                        .dispatch_or_log(crate::chat::action::Action::ModeChanged(mode), "chat.mode_changed");
                    #[cfg(feature = "terminal-tui")]
                    {
                        chat_mirror.lock().chat_mode = mode;
                    }
                    #[cfg(feature = "terminal-tui")]
                    let legacy_session_mode_writes_enabled = false; // S4-B: Pure 单源
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
                                    tool_descs: &tool_descs,
                                    skills: &skills,
                                    native_tools,
                                    tools_registry: &tools_registry,
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
                                    tool_descs: &tool_descs,
                                    skills: &skills,
                                    native_tools,
                                    tools_registry: &tools_registry,
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
                                    tool_descs: &tool_descs,
                                    skills: &skills,
                                    native_tools,
                                    tools_registry: &tools_registry,
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
                    match action {
                        SessionCommand::Bg { task } => {
                            // Spawn a background agent via sessions_spawn, passing
                            // the *current* provider/model (read from the main-loop
                            // strings, which `/provider` and `/model` keep in sync)
                            // so a hot switch is honoured (plan §C.0 blocker 3).
                            match tools_registry.iter().find(|t| t.supports_name("sessions_spawn")) {
                                Some(tool) => {
                                    let mut args = serde_json::json!({
                                        "task": task,
                                        "provider": current_provider_owned,
                                        "model": current_model_owned,
                                    });
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
                            // Reuses the shell tool's SideEffectGate (high-risk
                            // commands still blocked), workspace cwd, hardened env,
                            // and the v1.1 event bridge for live `/attach`/`/logs`.
                            match crate::chat::sessions::shell::spawn_shell(&command, &security, &shell_event_sink) {
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
                                    &security,
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
                        SessionCommand::Approve { seq } | SessionCommand::Deny { seq } => {
                            // NeedsInput: deliver an approval decision to a
                            // background sub-agent suspended on the supervised
                            // approval gate. `/approve` injects a runtime grant
                            // (Grant) so the gated tool can pass the gate; `/deny`
                            // reports the tool as denied to the sub-agent.
                            let approve = matches!(action, SessionCommand::Approve { .. });
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
                                crate::agent::loop_::ApprovalDecision::Grant
                            } else {
                                crate::agent::loop_::ApprovalDecision::Deny
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

        let file_mention_enrichment = enrich_file_mentions_for_prompt(&user_input, tools_registry.as_ref()).await;
        if let Some(note) = file_mention_enrichment.visible_note.as_deref() {
            emit_chat_output(note);
        }
        let user_input_for_prompt = file_mention_enrichment.prompt;

        fabric_turn_seq += 1;
        // D8-2: one run_id per turn, generated at the turn entry and reused by
        // every run_id consumer within this loop iteration (user event, route
        // scope, assistant event). No parent_run_id is set (turns are not a spawn
        // lineage; session relation is via chat_session_key).
        let turn_run_id = uuid::Uuid::new_v4().to_string();
        let chat_user_event = match record_chat_user_message_event(
            &memory_fabric,
            &chat_session,
            &chat_session_key,
            &turn_run_id,
            provider_name,
            model_name,
            fabric_turn_seq,
            &user_input,
        )
        .await
        {
            Ok(event) => Some(event),
            Err(e) => {
                tracing::warn!(error = %e, "Failed to append chat user message event");
                None
            }
        };

        // Auto-save user message to memory
        if config.memory.should_auto_promote_user_message(&user_input) {
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
        let runtime_envelope = chat_runtime_envelope(memory_fabric.workspace_id(), &chat_session_key);
        let document_ingest = Some(
            DocumentIngestRuntime::from_envelope(mem.clone(), &runtime_envelope)
                .with_source_message_event_id(chat_user_event.as_ref().map(|event| event.event_id.clone())),
        );
        let semantic_scope = chat_runtime_write_context(&runtime_envelope);
        let mem_context = build_context_with_shared_events_and_scope(
            mem.as_ref(),
            chat_runtime_principal(&runtime_envelope),
            &user_input,
            config.memory.min_relevance_score,
            Some(&semantic_scope),
        )
        .await;
        let context = mem_context.preamble.clone();
        let enriched = if context.is_empty() {
            user_input_for_prompt.clone()
        } else {
            format!("{context}{user_input_for_prompt}")
        };

        // Build system prompt with skill selection
        let selected_skills = select_prompt_skills(&user_input, &skills, &config, skill_embedder.as_ref()).await;
        let system_prompt = build_runtime_system_prompt(
            &config,
            model_name,
            &tool_descs,
            &selected_skills,
            native_tools,
            &tools_registry,
        );
        let persisted_history_for_turn = persisted_history_for_current_turn(&chat_session, &system_prompt, &user_input);
        if history.is_empty() {
            history.push(ChatMessage::system(system_prompt.clone()));
        } else if let Some(first) = history.first_mut() {
            *first = ChatMessage::system(system_prompt.clone());
        }
        // S2-C Step 4: 双写 SetLeadingSystemPrompt 到 reducer — 与 legacy
        // `if empty { push } else { first_mut = ... }` 字节级语义对齐（reducer
        // 内部走同样分支）。每轮 turn 都会跑，append 表达会让 system 堆积。
        let _ = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::SetLeadingSystemPrompt {
                content: system_prompt.clone(),
            },
            "chat.system_prompt_per_turn",
        );
        let history_len_before_user_turn = history.len();
        let history_user_message = ChatMessage::user(&enriched);
        let mut history_for_provider = history.clone();
        history_for_provider.push(history_user_message.clone());

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
        let cancellation = CancellationToken::new();
        let (delta_tx, delta_rx) = mpsc::channel::<String>(DELTA_CHANNEL_CAPACITY);

        // Start a streaming draft on the terminal
        let draft_id = match terminal.send_draft(&SendMessage::new("", "user")).await {
            Ok(id) => id,
            Err(e) => {
                tracing::debug!("Failed to start draft: {e}");
                None
            }
        };

        // Step 5b 双写：宣告新一轮 LLM 推理开始（仅在 draft 存在时）。
        // shadow 模式下 reducer 设置 stream.draft + control.generating=true；
        // 无外部副作用（业务 Effect no-op）。
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
            // Step 5b 双写：把每个 delta 通过 coalescer 投递成 Action::StreamChunkReceived。
            // bounded(2048) action_tx 满时由 coalescer 合并 delta，避免无界增长。
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
                // StreamCompleted/Failed/Cancelled (投递在 chat::run 主循环里完成).
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
                    ToolCallNotification::Progress {
                        iteration,
                        max_iterations,
                    } => {
                        terminal_for_tools.notify_progress(iteration, max_iterations).await;
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
            owner_id: Some(&scope_owner_id),
            topic_id: runtime_envelope.topic_id.as_deref(),
            task_id: runtime_envelope.resolved_task_id(),
            source_message_event_id: runtime_envelope.source_message_event_id.as_deref(),
        };

        // ── Timeout budget ───────────────────────────────────────
        let timeout_budget = turn_timeout_budget(
            config.channels_config.message_timeout_secs,
            config.agent.max_tool_iterations,
        );

        let effective_compaction = crate::router::resolve_effective_compaction_config(
            &config.agent.compaction,
            provider_name,
            model_name,
            &config.router,
            &config.model_routes,
        );
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

        // ── Retry loop (context overflow recovery + timeout retry) ──
        //
        // Mirrors the retry strategy in channels/mod.rs process_channel_message:
        //  - Context overflow: compact history, retry up to MAX_CONTEXT_OVERFLOW_RETRIES
        //  - Timeout: sleep 2s, retry once
        let mut context_overflow_retries = 0usize;
        let mut timeout_retries = 0usize;
        let mut history_len_before_tools;

        // S2-A refinement: split the coarse `Failed` variant so the Redux
        // dispatch path can distinguish user-driven cancellation from real
        // errors. The legacy renderer still treats every non-Success as a
        // failure (continue), but the reducer now sees the correct semantic
        // (`StreamCancelled` vs `StreamFailed { err, retryable }`).
        enum TurnOutcome {
            // FIX-P0-30/31: carry the loop's provider-attribution trace so the
            // success path can record the *real* serving model/attempts instead
            // of the routed `decision.selected.model`.
            Success(String, crate::agent::loop_::ToolLoopTrace),
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

        // 路由 Redux driver vs Legacy tool loop，决策矩阵见 route_turn
        #[cfg(feature = "terminal-tui")]
        let turn_route = {
            let mode = top_redux_mode;
            let route = route_turn(mode);
            tracing::info!(
                redux_mode = ?mode,
                tools_count = tools_registry.len(),
                route = ?route,
                "chat::run turn route decision"
            );
            route
        };
        #[cfg(feature = "terminal-tui")]
        let reducer_driver_turn_active = matches!(turn_route, TurnRoute::ReduxDriver) && draft_id.is_some();
        #[cfg(feature = "terminal-tui")]
        let provider_worker_kind = if reducer_driver_turn_active {
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached
        } else {
            crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited
        };
        // 非 TUI feature 下 turn_route 不参与控制流（driver 分支被 cfg 屏蔽），
        // 仅作变量保留以让两条 feature 配置下 chat::run 共享同一路由契约。
        #[cfg(not(feature = "terminal-tui"))]
        let _ = TurnRoute::LegacyToolLoop;
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
                "visible provider turn admission rejected after route decision; requeueing input"
            );
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

        // ── Redux Driver 切闸路径（Step 5a-4） ─────────────────────
        //
        // 仅在路由命中 ReduxDriver 时进入。dispatch Action::StartLLMTurn →
        // EffectExecutor::execute_real(Effect::StartTurn) → spawn drive_start_turn_stream
        // 流式驱动 → 通过 action_tx 回投 StreamChunkReceived / Completed / Failed /
        // Cancelled → dispatcher task reduce 后 turn_signal.record_and_notify →
        // 此处 await 拿 outcome。
        //
        // 此分支**不调** run_tool_call_loop，旧路径完全不跑：
        //   * 无 hook 双发（旧路径 hooks.emit 不执行；reducer 内 NotifyHook(Error) 独写）
        //   * 无 history 双写（reducer 通过 RecordAssistantTurn 单写）
        //   * round 2 hang 防御：tokio::select! 上 shutdown.cancelled() 兜底
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
            // 在 dispatch 前消费旧 outcome 残留以确保读到的是本轮的。
            let _ = turn_signal.consume_outcome();

            // S2.5 P1-A: 显式分支处理 dispatch_result（StartLLMTurn 失败必须 fall-through
            // 否则 notify_fut 永挂）；dispatch_or_log 同时埋点 + warn，无需重复 tracing.
            // D8-4 (redux path real fix): seed the turn-root spawn execution
            // context for this turn and hand it to the driver via StartLLMTurn →
            // Effect::StartTurn. This is the redux mirror of the legacy
            // `SPAWN_EXECUTION_CONTEXT.scope(seed_turn_context(turn_run_id, ..))`
            // wrapper applied below at the legacy `run_tool_call_loop_traced` call
            // — the redux path `continue`s before reaching it, so the seed must
            // travel with the effect. Same `turn_run_id` + `chat_session_key`
            // source as the legacy path (single source of truth) so a sub-agent
            // spawned inside this turn inherits `parent_run_id = turn_run_id`.
            let redux_turn_spawn_ctx = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
                turn_run_id.clone(),
                chat_session_key.clone(),
            );
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
                },
                "chat.start_llm_turn",
            );
            // Codex P1：dispatch 可能 Backpressured / ChannelClosed。任一失败
            // 都意味着 dispatcher task 不会产生 turn outcome，chat::run 必须立即
            // 视为 Failed 并 fall-through 到 cleanup，否则 notify_fut 永远不被 fire。
            if !matches!(dispatch_result, dispatcher::DispatchResult::Sent) {
                tracing::warn!(
                    result = ?dispatch_result,
                    "Redux driver: StartLLMTurn dispatch failed; aborting turn"
                );
                // S2-B Step 3: 同步发 StreamCancelled 让 reducer 清 active_cancel，
                // 旧字段仅在 Off/Both 兜底（与 register 处的守卫对称）。
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

            // shutdown 抢占保护防 round 2 hang。
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
                            match route_provider_completion_event_and_publish(
                                &mut provider_turn_lifecycle_rx,
                                &mut provider_turn_lifecycle_events_open,
                                &mut provider_turn_workers,
                                &mut pending_provider_completion_events,
                                provider_turn_task_id,
                                completion,
                                &chat_dispatcher,
                            ) {
                                ProviderTurnCompletionRoute::Current(completion) => {
                                    provider_completion_event = Some(completion);
                                    break;
                                }
                                ProviderTurnCompletionRoute::Pending => {}
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

            // Finalize streaming（与 legacy 收尾对齐）：drop senders 让后台任务收口.
            //
            // S2-B Step 3: driver 路径下 reducer 在收到 StreamCompleted/Failed/Cancelled
            // 时已经清掉 `state.control.active_cancel`；legacy Arc 仅在 Off/Both 兜底.
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
                    ..
                } => {
                    if empty_response {
                        let provider_outcome = ProviderExecutionOutcome::success_for_decision_with_usage(
                            &route_decision,
                            provider_started_at,
                            redux_tokens_used.clone(),
                        );
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
                        if let Err(e) =
                            record_provider_outcome_events(&memory_fabric, route_scope.clone(), &provider_outcome).await
                        {
                            tracing::warn!(
                                error = %e,
                                "Failed to append provider.final_outcome message event for empty Redux driver turn"
                            );
                        }
                        chat_session.add_user_turn(&user_input);
                        if let Some(record) = chat_session.record_provider_usage(&provider_outcome, &config.cost) {
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
                    let provider_outcome = ProviderExecutionOutcome::success_for_decision_with_usage(
                        &route_decision,
                        provider_started_at,
                        redux_tokens_used.clone(),
                    );
                    if !finalizer_result.finalized {
                        if let Err(e) = terminal.cancel_draft("user", &d_id).await {
                            tracing::debug!(error = %e, "Redux driver: cancel commit-gated draft failed");
                        }
                        publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                        publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                        continue;
                    }
                    // 1) 把 driver 流式累计的最终文本写回 LLM history（与 legacy 行尾
                    //    `history.push(ChatMessage::assistant(...))` 对齐）。
                    history.push(ChatMessage::assistant(final_text.clone()));
                    // 2) finalize_draft：把文本投递给 terminal channel 让用户可见
                    //    （driver 路径不走 delta_tx → draft_updater 链路，直接最终化）。
                    if let Err(e) = terminal.finalize_draft("user", &d_id, &final_text).await {
                        tracing::warn!(error = %e, "Redux driver: finalize_draft failed");
                    }
                    if let Err(e) = record_chat_assistant_message_event(
                        &memory_fabric,
                        &chat_session_key,
                        &turn_run_id,
                        provider_name,
                        model_name,
                        &recorded_response,
                    )
                    .await
                    {
                        tracing::warn!(error = %e, "Failed to append Redux driver chat assistant message event");
                    }
                    if let Err(e) =
                        record_provider_outcome_events(&memory_fabric, route_scope.clone(), &provider_outcome).await
                    {
                        tracing::warn!(
                            error = %e,
                            "Failed to append provider.final_outcome message event for Redux driver turn"
                        );
                    }
                    let attempts_count = u8::try_from(provider_outcome.attempts.len()).unwrap_or(u8::MAX);
                    crate::runtime::control_ladder::append_provider_outcome_trace(
                        std::path::Path::new(&config.workspace_dir),
                        &provider_outcome.decision_id,
                        &provider_outcome.final_provider,
                        &provider_outcome.final_model,
                        attempts_count,
                        "success",
                    );
                    // driver 路径 RecordAssistantTurn 已由 dispatcher.rs send（fixB B5）
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
                    if let Some(record) = chat_session.record_provider_usage(&provider_outcome, &config.cost) {
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
                    // reducer NotifyHook(Error) 已发；这里不再 hooks.emit 避免双发.
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

        let preflight_budget = crate::agent::loop_::plan_context_budget(
            &history,
            &effective_compaction.config,
            crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD,
        );
        if preflight_budget.over_hard_limit {
            let system_count = usize::from(history.first().is_some_and(|m| m.role == "system"));
            let turns_before = history.len().saturating_sub(system_count);
            let tokens_before = estimate_chat_history_tokens(&history);
            if let Some(patch) = build_chat_compaction_patch_with_timeout(
                &history,
                &persisted_history_for_turn,
                provider.as_ref(),
                model_name,
                &effective_compaction.config,
                document_ingest.as_ref(),
                "chat_preflight",
                Duration::from_secs(crate::agent::loop_::COMPACTION_TIMEOUT_SECS),
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
        // depth at 0 (no max_spawn_depth tightening). The chat session key is the
        // turn's spawn session scope.
        let turn_spawn_ctx = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
            turn_run_id.clone(),
            chat_session_key.clone(),
        );
        let turn_message_send_ctx = crate::tools::message_send::MessageSendExecutionContext::new(
            Some("user".to_string()),
            Arc::clone(&terminal) as Arc<dyn Channel>,
        );

        let turn_outcome = loop {
            history_len_before_tools = history.len();

            let result = tokio::time::timeout(
                timeout_budget,
                crate::tools::message_send::MESSAGE_SEND_EXECUTION_CONTEXT.scope(
                    turn_message_send_ctx.clone(),
                    crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT.scope(
                        turn_spawn_ctx.clone(),
                        run_tool_call_loop_traced(
                            provider.as_ref(),
                            &mut history,
                            &tools_registry,
                            observer.as_ref(),
                            &hooks,
                            provider_name,
                            model_name,
                            temperature,
                            false,
                            Some(&approval_manager),
                            "terminal",
                            &config.multimodal,
                            config.agent.max_tool_iterations,
                            config.agent.parallel_tools,
                            config.agent.read_only_tool_concurrency_window,
                            config.agent.read_only_tool_timeout_secs,
                            config.agent.priority_scheduling_enabled,
                            config.agent.low_priority_tools.clone(),
                            ToolConcurrencyGovernanceConfig {
                                kill_switch_force_serial: config.agent.concurrency_kill_switch_force_serial,
                                rollout_stage: config.agent.concurrency_rollout_stage.clone(),
                                rollout_sample_percent: config.agent.concurrency_rollout_sample_percent,
                                rollout_channels: config.agent.concurrency_rollout_channels.clone(),
                                auto_rollback_enabled: config.agent.concurrency_auto_rollback_enabled,
                                rollback_timeout_rate_threshold: config
                                    .agent
                                    .concurrency_rollback_timeout_rate_threshold,
                                rollback_cancel_rate_threshold: config.agent.concurrency_rollback_cancel_rate_threshold,
                                rollback_error_rate_threshold: config.agent.concurrency_rollback_error_rate_threshold,
                            },
                            Some(&effective_compaction.config),
                            Some(cancellation.clone()),
                            Some(delta_tx.clone()),
                            Some(&scope_ctx),
                            Some(tool_event_tx.clone()),
                            Some(&config.tool_tiering),
                            document_ingest.clone(),
                            chat_session.mode,
                        ),
                    ),
                ),
            )
            .await;

            match result {
                // ── Timeout ───────────────────────────────────────
                Err(_elapsed) => {
                    if timeout_retries < 1 {
                        timeout_retries += 1;
                        tracing::warn!("LLM timeout, retrying (attempt {timeout_retries}/1)");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    // Exhausted timeout retries
                    cancellation.cancel();
                    if let Some(ref d_id) = draft_id {
                        let _ = terminal.cancel_draft("user", d_id).await;
                    }
                    eprintln!("\nError: operation timed out\n");
                    if plain_mode {
                        plain_mode_turn_failed = true;
                    }
                    // Phase E (5a-4): dual_write_guard 守卫防止与 reducer 的 NotifyHook(Error)
                    // 在 Both / Redux 双写期产生双发（reducer 通过 Effect::NotifyHook 已发）。
                    // Off 模式 guard 永远 false → 行为不变（旧路径单发）。
                    if !dual_write_guard.is_active() {
                        hooks
                            .emit(HookEvent::Error, payload_error("chat-turn", "timeout"))
                            .await;
                    }
                    // S2-A: timeout exhausted is a non-retryable hard failure.
                    break TurnOutcome::FailedWithError {
                        err: "timeout".to_string(),
                        retryable: false,
                    };
                }
                // ── Success ───────────────────────────────────────
                Ok(Ok((resp, trace))) => break TurnOutcome::Success(resp, trace),
                // ── Cancelled (Ctrl+C) ────────────────────────────
                Ok(Err(ref e)) if is_tool_loop_cancelled(e) || cancellation.is_cancelled() => {
                    if let Some(ref d_id) = draft_id {
                        let _ = terminal.cancel_draft("user", d_id).await;
                    }
                    // S2-A: user-driven cancel — distinguished from real
                    // failures so the reducer emits `StreamCancelled` (no
                    // Error hook fan-out) instead of `StreamFailed`.
                    break TurnOutcome::Cancelled;
                }
                // ── Context window overflow → compact + retry ─────
                Ok(Err(ref e)) if is_context_window_overflow_error(e) => {
                    let audit_source_history =
                        original_legacy_chat_compaction_audit_source(&persisted_history_for_turn);
                    let summary_projection = bounded_legacy_chat_compaction_audit_source(&persisted_history_for_turn);
                    let token_metadata = legacy_compaction_token_metadata(&history, &persisted_history_for_turn);
                    let turns_before = history
                        .len()
                        .saturating_sub(usize::from(history.first().is_some_and(|m| m.role == "system")));
                    let tokens_before = estimate_chat_history_tokens(&history);
                    if let Some(patch) = build_chat_compaction_patch_with_timeout(
                        &history,
                        &persisted_history_for_turn,
                        provider.as_ref(),
                        model_name,
                        &effective_compaction.config,
                        document_ingest.as_ref(),
                        "chat_context_overflow",
                        Duration::from_secs(crate::agent::loop_::COMPACTION_TIMEOUT_SECS),
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
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::HistoryCompacted {
                                reason: crate::chat::action::CompactReason::ContextOverflow,
                            },
                            "chat.history_compacted_overflow",
                        );
                        compact_chat_history(&mut history);
                    }
                    persist_legacy_chat_compaction_audit(
                        mem.as_ref(),
                        &runtime_envelope,
                        &audit_source_history,
                        &summary_projection,
                        token_metadata,
                        "chat_context_overflow",
                    )
                    .await;
                    let turns_after = history
                        .len()
                        .saturating_sub(usize::from(history.first().is_some_and(|m| m.role == "system")));
                    let tokens_after = estimate_chat_history_tokens(&history);
                    #[cfg(feature = "terminal-tui")]
                    refresh_context_budget_for_tui(
                        &history,
                        &effective_compaction.config,
                        redraw_tx_for_main.is_some(),
                        &chat_mirror,
                        &chat_dispatcher,
                    );
                    let compact_msg = format_compact_feedback(
                        turns_before,
                        turns_after,
                        tokens_before,
                        tokens_after,
                        effective_compaction.config.max_context_tokens,
                    );
                    surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &compact_msg);
                    let compacted_chars: usize = history.iter().map(|m| m.content.chars().count()).sum();
                    tracing::warn!(
                        retries = context_overflow_retries,
                        compacted_chars,
                        "Context window overflow, history compacted"
                    );

                    if context_overflow_retries < MAX_CONTEXT_OVERFLOW_RETRIES {
                        context_overflow_retries += 1;
                        continue;
                    }
                    // Exhausted overflow retries
                    if let Some(ref d_id) = draft_id {
                        let _ = terminal.cancel_draft("user", d_id).await;
                    }
                    eprintln!(
                        "\nError: context window exceeded after {} compaction retries\n",
                        MAX_CONTEXT_OVERFLOW_RETRIES
                    );
                    if plain_mode {
                        plain_mode_turn_failed = true;
                    }
                    // Phase E (5a-4): dual_write_guard 守卫见 timeout 分支同理.
                    if !dual_write_guard.is_active() {
                        hooks
                            .emit(
                                HookEvent::Error,
                                payload_error("chat-turn", "context-overflow-exhausted"),
                            )
                            .await;
                    }
                    // S2-A: compaction retries exhausted — non-retryable.
                    break TurnOutcome::FailedWithError {
                        err: "context-overflow-exhausted".to_string(),
                        retryable: false,
                    };
                }
                // ── Other errors ──────────────────────────────────
                Ok(Err(e)) => {
                    if let Some(ref d_id) = draft_id {
                        let _ = terminal.cancel_draft("user", d_id).await;
                    }
                    let err_text = e.to_string();
                    eprintln!("\nError: {err_text}\n");
                    if plain_mode {
                        plain_mode_turn_failed = true;
                    }
                    // Phase E (5a-4): dual_write_guard 守卫见 timeout 分支同理.
                    if !dual_write_guard.is_active() {
                        hooks
                            .emit(HookEvent::Error, payload_error("chat-turn", &err_text))
                            .await;
                    }
                    // S2-A: generic provider/loop error — retryable hint is
                    // false (the caller already chose to surface and continue;
                    // retry policy is owned by upstream once tooling lands).
                    break TurnOutcome::FailedWithError {
                        err: err_text,
                        retryable: false,
                    };
                }
            }
        };

        // ── Finalize streaming ────────────────────────────────────
        // Deregister this turn's cancellation token.
        //
        // S2-B Step 3: legacy 路径下 reducer 在 Stream{Completed,Cancelled,Failed}
        // dispatch (下方 1886-1911) 时也清 `state.control.active_cancel`；legacy
        // Arc 仅在 Off/Both 模式兜底（外部 Ctrl+C handler 读这个）。
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

        // Step 5b 双写：根据 turn 结果投递相应的流式结束 Action。
        // 当 draft_id 存在时 reducer 才能匹配 stream.draft 并清理；否则 no-op.
        //
        // S2-A: split the previous single-pronged "Failed → StreamCancelled"
        // fallback. Order is **critical**: cancellation is detected up the
        // stack via `is_tool_loop_cancelled` / `cancellation.is_cancelled()`
        // and surfaces as `TurnOutcome::Cancelled`. Real errors (timeout /
        // context overflow / provider error) surface as `FailedWithError` and
        // map to `StreamFailed { err, retryable }` so the reducer emits the
        // `NotifyHook(Error) + LogTrace + RequestRedraw` effect chain.
        //
        // T3-3-fixA P0-1: Success 分支的 StreamCompleted 已下移到 RecordAssistantTurn
        // 之后 dispatch，确保 reducer 构造 SaveSession 快照时 session.turns 已含当轮
        // assistant。Cancelled / FailedWithError 不写 assistant turn，dispatch 位置不变。
        match &turn_outcome {
            TurnOutcome::Success(..) => {}
            TurnOutcome::Cancelled => {
                if let Some(ref d_id) = draft_id {
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::StreamCancelled { draft_id: d_id.clone() },
                        "chat.stream_cancelled",
                    );
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
                // FIX-P1-15 (#27): the success path below records a
                // `ProviderExecutionOutcome` + control-ladder trace, but a
                // failed turn (timeout / context-overflow-exhausted / provider
                // error) used to `continue` without emitting any provider
                // outcome, leaving the routing/provider timeline blind to
                // failed turns. Record a `failed_for_decision` outcome here so
                // the `decision_id` join still has a `provider.final_outcome` /
                // control-ladder trace for the failed attempt. Cancellation is
                // a user-driven abort, not a provider failure, so the
                // `Cancelled` arm above intentionally records nothing.
                let failure = anyhow::anyhow!("{err}");
                let failed_outcome =
                    ProviderExecutionOutcome::failed_for_decision(&route_decision, provider_started_at, &failure);
                if let Err(e) =
                    record_provider_outcome_events(&memory_fabric, route_scope.clone(), &failed_outcome).await
                {
                    tracing::warn!(error = %e, "Failed to append provider.final_outcome message event for failed turn");
                }
                surface_turn_elapsed_message(
                    &chat_dispatcher,
                    sessions_redraw_handle.as_ref(),
                    "failed",
                    failed_outcome.started_at,
                    failed_outcome.finished_at,
                );
                let attempts_count = u8::try_from(failed_outcome.attempts.len()).unwrap_or(u8::MAX);
                crate::runtime::control_ladder::append_provider_outcome_trace(
                    std::path::Path::new(&config.workspace_dir),
                    &failed_outcome.decision_id,
                    &failed_outcome.final_provider,
                    &failed_outcome.final_model,
                    attempts_count,
                    "all_failed",
                );
            }
        }

        // If the turn failed or was cancelled, skip response processing
        let (response, turn_trace) = match turn_outcome {
            TurnOutcome::Success(resp, trace) => (resp, trace),
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
        if let Err(e) = record_provider_outcome_events(&memory_fabric, route_scope.clone(), &provider_outcome).await {
            tracing::warn!(error = %e, "Failed to append provider.final_outcome message event");
        }
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
                // S4-B: 删除 mirror push_reasoning，reducer (reduce_stream_completed) 单源 push Reasoning card
                if let Some(tx) = redraw_tx_for_main.as_ref() {
                    let _ = tx.try_send(());
                }
            }
        }

        increment_recalled_useful_counts(mem.as_ref(), &mem_context.ids).await;

        // ── Sanitize response: strip tool-call XML/JSON artifacts ──
        let response = sanitize_channel_response(&response, &tools_registry);

        if crate::agent::loop_::is_empty_assistant_response(&response, false) {
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
                crate::agent::loop_::EMPTY_ASSISTANT_RESPONSE_MESSAGE,
            );

            chat_session.add_user_turn(&user_input);
            if let Some(record) = chat_session.record_provider_usage(&provider_outcome, &config.cost) {
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

        if let Err(e) = record_chat_assistant_message_event(
            &memory_fabric,
            &chat_session_key,
            &turn_run_id,
            provider_name,
            model_name,
            &response,
        )
        .await
        {
            tracing::warn!(error = %e, "Failed to append chat assistant message event");
        }

        // ── Extract tool context summary for LLM awareness on next turn ──
        let tool_summary = extract_tool_context_summary(&history, history_len_before_tools);
        // Always persist the assistant response to history. When tools were
        // invoked, prepend the summary so the LLM retains awareness.
        let history_response = if tool_summary.is_empty() {
            response.clone()
        } else {
            format!("{tool_summary}\n{response}")
        };
        history.push(ChatMessage::assistant(&history_response));

        // S2-B Step 4: dispatch RecordAssistantTurn(history_response) 在与 legacy
        // `history.push(ChatMessage::assistant(...))` 同一点 — reducer 的
        // session.history 与 legacy history 字节级对齐。下方 line 2055 处的
        // 旧 dispatch 用 sanitized_response，与 history.push 内容不同 — S2-B Step 4
        // 起改在此处 dispatch 用 history_response，下方旧 dispatch 删除。
        let _ = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::RecordAssistantTurn {
                task_id: provider_turn_task_id,
                content: history_response.clone(),
            },
            "chat.record_assistant_turn",
        );

        // T3-3-fixA P0-1: StreamCompleted 必须在 RecordAssistantTurn 之后 dispatch，
        // reducer 的 reduce_stream_completed 会 emit Effect::SaveSession(snapshot)，
        // 此时 session.turns 已含当轮 assistant —— 否则 SaveSession 落盘旧快照。
        // final_text 用 response (UI 展示文案)，与上方 history_response (含 tool_summary
        // 前缀供 history 写入) 语义不同：reducer 的 conversation_lines 与 UI 对齐.
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
        // S2-B Step 4: RecordUserTurn / RecordAssistantTurn 已经在上面（enriched /
        // history_response 同点）dispatch；这里 legacy `chat_session.add_*_turn` 在
        // `Off` / `Both` / `Redux` 模式下保留，因为 `chat_session` 仍是
        // `save_session(mem, &chat_session)` 的真实持久化源。
        //
        // T3-3-c 收官：**Pure 模式跳过 legacy add_*_turn** —— reducer 的
        // `RecordUserTurn` / `RecordAssistantTurn` + `Effect::SaveSession` 接管
        // 单源持久化，下方 `save_session(...)` 也由 `dual_write_guard` 抑制。
        // 这关闭了 S2-D/E 阶段保留的最后一处双写残留。
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
        if let Some(record) = chat_session.record_provider_usage(&provider_outcome, &config.cost) {
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

        // P0-1 fix: 旧路径在 Both/Redux 模式下受 dual_write_guard 守卫。
        // Redux reducer 的 SaveSession effect 已在 execute_real 中置位 guard，
        // 若 guard 已激活则旧路径跳过 save_session + hooks.emit(TurnComplete)，
        // 防止 hooks/webhook 双触发（hooks/webhook 不幂等，真会双发）。
        // Off 模式下 guard 永远 false，旧路径如常单写。
        // 选 turn-level（而非 effect-level）：整个 turn 期间只要 Redux 执行了
        // SaveSession/NotifyHook 之一，guard 即 active，旧路径的所有后续写都被抑制。
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
            // 旧路径跳过，避免双写/双发。
            tracing::debug!(
                "P0-1: dual_write_guard active — legacy path skipping save_session + hooks.emit(TurnComplete)"
            );
            // observer.record_event 仍然调用（observer 只是本地计数，无外部副作用）
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
    }

    // ── Child-session shutdown on exit (Phase C) ─────────────────
    // Snapshot every child session still tracked at exit, persist summaries for
    // live sessions as interrupted, then terminate and clear every child
    // registry through the single session owner. This covers running agents,
    // NeedsInput agents, background shells, and interactive PTYs uniformly.
    let _shutdown_report =
        shutdown_child_sessions_for_exit(&mut chat_sessions, &mut chat_session, &chat_dispatcher).await;
    session_rings.clear();

    // Give the reducer-owned turn persistence path a bounded chance to finish
    // before shutdown cancellation drains the dispatcher. This closes the
    // piped-stdin race where a successful response followed immediately by
    // `/exit` could print to the user but miss `chat_session:*` persistence.
    let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(EXIT_PERSISTENCE_DRAIN_GRACE_MS);
    while dual_write_guard.is_active() && tokio::time::Instant::now() < drain_deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(EXIT_PERSISTENCE_IDLE_SETTLE_MS)).await;

    // Step 5b: dispatcher task graceful shutdown.
    //
    // 1. shutdown.cancel() 让所有 spawn 出去的信号 handler / TUI loop 主动退出，
    //    释放它们持有的 chat_dispatcher sender clone（否则 action_rx 永远不会
    //    自然 close，dispatcher_handle.await 会 hang）。
    // 2. drop(chat_dispatcher) 释放主路径持有的 sender。
    // 3. dispatcher_handle.await 收尾——select! 中 shutdown.cancelled() 分支立即
    //    触发，dispatcher 退出。main.rs:866 的 RUNTIME_SHUTDOWN_TIMEOUT (2s)
    //    仍兜底（不可改）。
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

    // T3-3-fixA P0-2: 退出 save_session Pure 守卫.
    //
    // Pure 模式下 chat_session.add_*_turn 被 line 2185 守卫跳过，chat_session.turns
    // 滞后于 reducer 维护的 SessionState。无条件退出 save 会用旧快照覆盖 reducer
    // 已落盘的最新 snapshot。守卫表达式与 line 2185 同形结构保持一致.
    #[cfg(feature = "terminal-tui")]
    let legacy_exit_save_enabled = false; // S4-B: Pure 单源
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
/// S4-A Commit 4: RenderSource — Pure 模式从 `watch::Receiver` 读 snapshot；
/// Off/Both/Redux 模式从 mirror 锁读 TuiState。
///
/// 渲染 hot path 通过 [`Self::with_view`] 闭包统一拿 `&dyn BottomChromeView`，
/// 避免两条路径重复代码。
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
    tokio::task::spawn_blocking(move || {
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

fn pop_next_input_message(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
) -> Option<crate::channels::traits::ChannelMessage> {
    pop_next_queued_input_message(backlog).map(|queued| queued.msg)
}

fn pop_next_queued_input_message(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
) -> Option<QueuedInputMessage> {
    let mut best_idx = None;
    let mut best_priority = InputQueuePriority::Normal;
    for (idx, queued) in backlog.iter().enumerate() {
        if best_idx.is_none() || queued.priority > best_priority {
            best_idx = Some(idx);
            best_priority = queued.priority;
        }
    }
    let best_idx = best_idx?;
    backlog.remove(best_idx)
}

fn pop_next_input_message_with_scheduler(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
) -> Option<crate::channels::traits::ChannelMessage> {
    let queued = pop_next_queued_input_message(backlog)?;
    if let Some(id) = queued.turn_task_id
        && let Err(error) = scheduler.mark_legacy_dispatched(id)
    {
        tracing::warn!(
            task_id = id.get(),
            error = ?error,
            "TurnScheduler legacy dispatch mirror failed"
        );
    }
    Some(queued.msg)
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
    pop_next_input_task_with_scheduler(backlog, scheduler)
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

fn input_backlog_status(
    backlog: &std::collections::VecDeque<QueuedInputMessage>,
) -> crate::chat::action::MainQueueStatus {
    crate::chat::action::MainQueueStatus {
        queued: backlog.len(),
        priority: backlog
            .iter()
            .filter(|queued| queued.priority == InputQueuePriority::Priority)
            .count(),
    }
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
    rx: &mut mpsc::UnboundedReceiver<dispatcher::ProviderTurnLifecycleEvent>,
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
    lifecycle_rx: &mut mpsc::UnboundedReceiver<dispatcher::ProviderTurnLifecycleEvent>,
    lifecycle_events_open: &mut bool,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    pending: &mut std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, ProviderTurnCompletionEvent>,
    current_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    completion: ProviderTurnCompletionEvent,
) -> ProviderTurnCompletionRoute {
    if drain_provider_turn_lifecycle_events(lifecycle_rx, lifecycle_events_open, workers) {
        tracing::trace!(
            task_id = completion.task_id.get(),
            "drained provider lifecycle events before routing completion"
        );
    }
    let task_id = completion.task_id;
    record_provider_turn_completion_ready(workers, task_id);
    if Some(task_id) == current_task_id {
        return ProviderTurnCompletionRoute::Current(completion);
    }
    if pending.insert(task_id, completion).is_some() {
        tracing::warn!(
            task_id = task_id.get(),
            "replaced duplicate pending provider completion event"
        );
    }
    ProviderTurnCompletionRoute::Pending
}

fn route_provider_completion_event_and_publish(
    lifecycle_rx: &mut mpsc::UnboundedReceiver<dispatcher::ProviderTurnLifecycleEvent>,
    lifecycle_events_open: &mut bool,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    pending: &mut std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, ProviderTurnCompletionEvent>,
    current_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    completion: ProviderTurnCompletionEvent,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
) -> ProviderTurnCompletionRoute {
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
            if crate::agent::loop_::is_empty_assistant_response(&final_text, false) {
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

fn enqueue_provider_turn_finalizer_event(
    queue: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    plan: ProviderTurnTerminalPlan,
) {
    queue.push_back(ProviderTurnFinalizerEvent { task_id, plan });
}

#[cfg(feature = "terminal-tui")]
fn dispatch_ordered_provider_turn_commit(
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    task_id: crate::chat::turn_scheduler::TurnTaskId,
    draft_id: &str,
    user_input: &str,
    final_text: &str,
    reasoning: &str,
    empty_response: bool,
) {
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::RecordUserTurn(user_input.to_string()),
        "chat.ordered_record_user_turn",
    );
    if !empty_response {
        let _ = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::RecordAssistantTurn {
                task_id: Some(task_id),
                content: final_text.to_string(),
            },
            "chat.ordered_record_assistant_turn",
        );
    }
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::StreamCompleted {
            draft_id: draft_id.to_string(),
            final_text: final_text.to_string(),
            reasoning: reasoning.to_string(),
        },
        "chat.ordered_stream_completed",
    );
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
) {
    let pending = pending_commit.context;
    let ProviderTurnTerminalPlan::Completed {
        final_text,
        reasoning,
        recorded_response,
        empty_response,
        usage: redux_tokens_used,
        ..
    } = pending_commit.terminal_plan
    else {
        tracing::warn!(
            task_id = pending.task_id.get(),
            "ordered provider turn commit received non-completed terminal plan"
        );
        return;
    };

    let provider_turn_task_id = Some(pending.task_id);
    let provider_outcome = ProviderExecutionOutcome::success_for_decision_with_usage(
        &pending.route_decision,
        pending.provider_started_at,
        redux_tokens_used.clone(),
    );
    history.push(pending.history_user_message.clone());
    if empty_response {
        if let Err(e) = terminal.cancel_draft("user", &pending.draft_id).await {
            tracing::debug!(error = %e, "Redux driver: cancel empty-response draft failed");
        }
        dispatch_ordered_provider_turn_commit(
            chat_dispatcher,
            pending.task_id,
            &pending.draft_id,
            &pending.user_input,
            &final_text,
            &reasoning,
            empty_response,
        );
        if let Err(e) =
            record_provider_outcome_events(memory_fabric, pending.route_scope.clone(), &provider_outcome).await
        {
            tracing::warn!(
                error = %e,
                "Failed to append provider.final_outcome message event for empty Redux driver turn"
            );
        }
        chat_session.add_user_turn(&pending.user_input);
        if let Some(record) = chat_session.record_provider_usage(&provider_outcome, &config.cost) {
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
            provider_outcome.started_at,
            provider_outcome.finished_at,
        );
        publish_main_queue_status(chat_dispatcher, turn_scheduler);
        publish_provider_worker_status(chat_dispatcher, provider_turn_workers);
        return;
    }

    history.push(ChatMessage::assistant(final_text.clone()));
    if let Err(e) = terminal.finalize_draft("user", &pending.draft_id, &final_text).await {
        tracing::warn!(error = %e, "Redux driver: finalize_draft failed");
    }
    dispatch_ordered_provider_turn_commit(
        chat_dispatcher,
        pending.task_id,
        &pending.draft_id,
        &pending.user_input,
        &final_text,
        &reasoning,
        empty_response,
    );
    if let Err(e) = record_chat_assistant_message_event(
        memory_fabric,
        chat_session_key,
        &pending.turn_run_id,
        &pending.provider_name,
        &pending.model_name,
        &recorded_response,
    )
    .await
    {
        tracing::warn!(error = %e, "Failed to append Redux driver chat assistant message event");
    }
    if let Err(e) = record_provider_outcome_events(memory_fabric, pending.route_scope.clone(), &provider_outcome).await
    {
        tracing::warn!(
            error = %e,
            "Failed to append provider.final_outcome message event for Redux driver turn"
        );
    }
    let attempts_count = u8::try_from(provider_outcome.attempts.len()).unwrap_or(u8::MAX);
    crate::runtime::control_ladder::append_provider_outcome_trace(
        std::path::Path::new(&config.workspace_dir),
        &provider_outcome.decision_id,
        &provider_outcome.final_provider,
        &provider_outcome.final_model,
        attempts_count,
        "success",
    );
    chat_session.add_user_turn(&pending.user_input);
    chat_session.add_assistant_turn(&recorded_response, Vec::new());
    if let Some(record) = chat_session.record_provider_usage(&provider_outcome, &config.cost) {
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
        provider_outcome.started_at,
        provider_outcome.finished_at,
    );
    publish_main_queue_status(chat_dispatcher, turn_scheduler);
    publish_provider_worker_status(chat_dispatcher, provider_turn_workers);
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
) -> bool {
    let mut applied = false;
    for result in results {
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
        commit_completed_provider_turn(
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
        )
        .await;
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
        provider_name = %pending.provider_name,
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

fn is_active_turn_local_command(input: &str) -> bool {
    let (_priority, content) = input_priority_from_text(input);
    is_queue_command(&content)
        || is_cost_command(&content)
        || is_workers_command(&content)
        || local_session_command(&content).is_some()
}

fn format_active_turn_local_notice(input: &str) -> String {
    let (_priority, content) = input_priority_from_text(input);
    let collapsed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview = truncate_chars_for_queued_notice(&collapsed, 160);
    format!("local > {preview}")
}

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
    )
    .await
    {
        emit_chat_output(&output);
        return;
    }
    let _priority =
        enqueue_input_message_and_return_priority_with_scheduler(backlog, scheduler, msg, chat_session.turns.len());
}

#[allow(clippy::too_many_arguments)]
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

fn active_turn_workers_cancel_output(
    msg: &crate::channels::traits::ChannelMessage,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
) -> Option<(String, Option<ProviderWorkerCancelSignal>)> {
    let (_priority, content) = classify_input_priority(msg);
    provider_workers_cancel_output_for_input(&content, scheduler, workers, provider_turn_task_id)
}

fn provider_workers_cancel_output_for_input(
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

fn local_session_command(input: &str) -> Option<crate::chat::sessions::SessionCommand> {
    let action = crate::chat::sessions::parse_session_command(input)?;
    match action {
        crate::chat::sessions::SessionCommand::Sessions
        | crate::chat::sessions::SessionCommand::Logs { .. }
        | crate::chat::sessions::SessionCommand::Kill { .. } => Some(action),
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
    let mut args = vec!["diff", "--no-ext-diff", "--no-color", "--unified=3"];
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
    let mut child = tokio::process::Command::new("git")
        .args(&args)
        .current_dir(workspace_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| format!("diff unavailable: {err}"))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "diff unavailable: failed to capture git stdout".to_string())?
        .take(u64::try_from(DIFF_MAX_BYTES.saturating_add(1)).map_or(u64::MAX, |value| value));
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
        let mut stderr = stderr.take(u64::try_from(DIFF_ERROR_MAX_BYTES).map_or(u64::MAX, |value| value));
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
    security: &Arc<crate::security::SecurityPolicy>,
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

    let session = match PtyShellSession::spawn(command, security, size) {
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
    let outcome = tokio::task::spawn_blocking(move || {
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
    let idle_poll = Duration::from_millis(1_000);

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
                        // Exit commands should not wait behind the async input
                        // consumer or an in-flight provider turn. Handle them
                        // at the TUI submit boundary and let the outer chat
                        // loop perform its normal child-session cleanup.
                        if is_chat_quit_command(&trimmed) {
                            let _ = chat_dispatcher.dispatch_or_log(Action::ForceQuit, "chat.tui_quit_submitted");
                            shutdown.cancel();
                            let _ = terminal.draw(|frame| {
                                let area = frame.area();
                                frame.render_widget(ratatui::widgets::Clear, area);
                            });
                            return Ok(());
                        }
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
                            // S2-B Step 3: 双击 — 同时 dispatch ShutdownRequested
                            // 让 reducer 真发 CancelToken/Quit (Off/Both 模式仍 fallback
                            // 到 shutdown.cancel()).
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
            let turn = &snapshot.turns[*index];
            event.role == turn.role && event.content == turn.content
        });
    if turns_from_events {
        for (event, index) in message_events.iter().zip(snapshot_turn_indices) {
            // MessageEvent owns replay role/content. Blob-only timestamp and
            // tool-call summaries remain compatibility metadata until the event
            // contract can reproduce them as well.
            snapshot.turns[index].role.clone_from(&event.role);
            snapshot.turns[index].content.clone_from(&event.content);
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
    let usage_from_events =
        !final_outcome_events.is_empty() && !malformed_outcome && projected_usage == snapshot.token_usage_records;
    if usage_from_events {
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
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
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
        .map(|turn| ChatMessage {
            role: turn.role.clone(),
            content: turn.content.clone(),
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
    history.push(ChatMessage::user(user_input.to_string()));
    history
}

fn history_for_session_with_system(
    session: &session::ChatSession,
    config: &Config,
    model_name: &str,
    tool_descs: &[(&str, &str)],
    skills: &[crate::skills::Skill],
    native_tools: bool,
    tools_registry: &[Box<dyn Tool>],
) -> Vec<ChatMessage> {
    let resumed_history = session_turns_to_history(session);
    if config.skill_rag.enabled {
        return resumed_history;
    }
    let mut history = vec![ChatMessage::system(build_runtime_system_prompt(
        config,
        model_name,
        tool_descs,
        skills,
        native_tools,
        tools_registry,
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
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(sessions)
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
    tool_descs: &'a [(&'a str, &'a str)],
    skills: &'a [crate::skills::Skill],
    native_tools: bool,
    tools_registry: &'a [Box<dyn Tool>],
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

#[derive(Debug)]
enum ChatControlEvent {
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
            Err(e) => anyhow::bail!("Resume aborted: failed to load saved chat session '{target_id}': {e}"),
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
        ctx.tool_descs,
        ctx.skills,
        ctx.native_tools,
        ctx.tools_registry,
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

#[cfg(test)]
mod session_load_error_semantics_tests {
    //! D10/C1-C4: chat session load paths must distinguish `None` (no such
    //! session -> start fresh) from `Err` (storage failure / corrupt blob ->
    //! propagate, fail fast). A `FailingMemory` mock injects storage errors so
    //! we can assert load helpers return `Err`, never silently degrade to `None`.
    use super::*;
    use crate::memory::MemoryEntry;
    use async_trait::async_trait;

    /// Minimal `Memory` whose `get`/`list` fail; everything else is inert.
    struct FailingMemory;

    #[async_trait]
    impl Memory for FailingMemory {
        fn name(&self) -> &str {
            "failing"
        }
        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }
        async fn recall(&self, _query: &str, _limit: usize, _session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
            Ok(vec![])
        }
        async fn get(&self, _key: &str) -> Result<Option<MemoryEntry>> {
            Err(anyhow::anyhow!("injected storage failure on get"))
        }
        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> Result<Vec<MemoryEntry>> {
            Err(anyhow::anyhow!("injected storage failure on list"))
        }
        async fn forget(&self, _key: &str) -> Result<bool> {
            Ok(false)
        }
        async fn count(&self) -> Result<usize> {
            Ok(0)
        }
        async fn health_check(&self) -> bool {
            false
        }
    }

    /// `Memory` returning a fixed set of entries from `list` and exact `get`.
    struct StaticMemory {
        entries: Vec<MemoryEntry>,
    }

    #[async_trait]
    impl Memory for StaticMemory {
        fn name(&self) -> &str {
            "static"
        }
        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }
        async fn recall(&self, _query: &str, _limit: usize, _session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
            Ok(vec![])
        }
        async fn get(&self, key: &str) -> Result<Option<MemoryEntry>> {
            Ok(self.entries.iter().find(|e| e.key == key).cloned())
        }
        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> Result<Vec<MemoryEntry>> {
            Ok(self.entries.clone())
        }
        async fn forget(&self, _key: &str) -> Result<bool> {
            Ok(false)
        }
        async fn count(&self) -> Result<usize> {
            Ok(self.entries.len())
        }
        async fn health_check(&self) -> bool {
            true
        }
    }

    fn entry(key: &str, content: &str) -> MemoryEntry {
        MemoryEntry {
            id: key.to_string(),
            key: key.to_string(),
            content: content.to_string(),
            category: MemoryCategory::Conversation,
            timestamp: "now".to_string(),
            session_id: None,
            score: None,
            tags: None,
            access_count: None,
            useful_count: None,
            source: None,
            source_confidence: None,
            verification_status: None,
            lifecycle_state: None,
            compressed_from: None,
        }
    }

    fn session_entry(id: &str) -> MemoryEntry {
        let mut s = session::ChatSession::new("p", "m");
        s.id = id.to_string();
        let json = s.to_json().expect("test: serialize session");
        entry(&format!("{}:{}", session::SESSION_MEMORY_PREFIX, id), &json)
    }

    // C1: storage Err on get must propagate, not collapse to Ok(None).
    #[tokio::test]
    async fn load_session_by_id_propagates_storage_error() {
        let mem = FailingMemory;
        let result = load_session_by_id(&mem, "abc").await;
        assert!(result.is_err(), "storage error must surface as Err, not Ok(None)");
    }

    // Ok(None): genuine absence still maps to a fresh-session path.
    #[tokio::test]
    async fn load_session_by_id_missing_returns_ok_none() {
        let mem = StaticMemory { entries: vec![] };
        let result = load_session_by_id(&mem, "missing").await;
        assert!(matches!(result, Ok(None)), "absent session must be Ok(None)");
    }

    // C2: corrupt blob under an exact session key is data corruption -> Err.
    #[tokio::test]
    async fn load_session_by_id_corrupt_blob_is_error() {
        let mem = StaticMemory {
            entries: vec![entry(
                &format!("{}:bad", session::SESSION_MEMORY_PREFIX),
                "{not valid json",
            )],
        };
        let result = load_session_by_id(&mem, "bad").await;
        assert!(result.is_err(), "corrupt stored blob must be Err, not Ok(None)");
    }

    // C3 (id consistency): embedded id that disagrees with the requested id must be Err.
    // If `chat_session:<id>` stores a blob whose `id` field is a different value, resuming
    // it would silently continue with the wrong session (D10 review finding).
    #[tokio::test]
    async fn load_session_by_id_embedded_id_mismatch_is_error() {
        // Build a valid session blob whose embedded id is "other-id".
        let mut s = session::ChatSession::new("p", "m");
        s.id = "other-id".to_string();
        let json = s.to_json().expect("test: serialize session");
        // Store it under the key for "requested-id" — key/embedded-id disagree.
        let mem = StaticMemory {
            entries: vec![entry(
                &format!("{}:requested-id", session::SESSION_MEMORY_PREFIX),
                &json,
            )],
        };
        let result = load_session_by_id(&mem, "requested-id").await;
        assert!(
            result.is_err(),
            "embedded id mismatch must be Err, not Ok(Some(wrong_session))"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("refusing") && msg.contains("bury"),
            "error message should mention 'refusing' and 'bury': {msg}"
        );
    }

    // Happy path: valid stored session round-trips.
    #[tokio::test]
    async fn load_session_by_id_valid_round_trips() {
        let mem = StaticMemory {
            entries: vec![session_entry("good")],
        };
        let result = load_session_by_id(&mem, "good").await.expect("test: load");
        assert_eq!(result.map(|s| s.id), Some("good".to_string()));
    }

    // C3: storage Err on list must propagate from load_latest_session.
    #[tokio::test]
    async fn load_latest_session_propagates_storage_error() {
        let mem = FailingMemory;
        assert!(
            load_latest_session(&mem).await.is_err(),
            "list error must surface as Err"
        );
    }

    #[tokio::test]
    async fn load_latest_session_empty_returns_ok_none() {
        let mem = StaticMemory { entries: vec![] };
        assert!(matches!(load_latest_session(&mem).await, Ok(None)));
    }

    // Corrupt exact session entry must not be silently filtered out of latest/list.
    #[tokio::test]
    async fn load_latest_session_corrupt_entry_is_error() {
        let mem = StaticMemory {
            entries: vec![entry(&format!("{}:rotten", session::SESSION_MEMORY_PREFIX), "{corrupt")],
        };
        assert!(
            load_latest_session(&mem).await.is_err(),
            "corrupt session entry must surface as Err, not be dropped as 'no session'"
        );
    }

    // C4: list_saved_sessions must propagate the storage error (no unwrap_or_default).
    #[tokio::test]
    async fn list_saved_sessions_propagates_storage_error() {
        let mem = FailingMemory;
        assert!(
            list_saved_sessions(&mem).await.is_err(),
            "list error must surface, not print 'No saved sessions'"
        );
    }

    // Non-session entries (wrong prefix) are skipped, not errored.
    #[test]
    fn valid_session_entry_skips_non_session_entries() {
        let e = entry("unrelated:key", "whatever");
        assert!(matches!(valid_session_entry(&e), Ok(None)));
    }

    // Key/id disagreement on an exact session entry is corruption -> Err.
    #[test]
    fn valid_session_entry_key_id_mismatch_is_error() {
        let mut s = session::ChatSession::new("p", "m");
        s.id = "embedded-id".to_string();
        let json = s.to_json().expect("test: serialize");
        let e = entry(&format!("{}:key-id", session::SESSION_MEMORY_PREFIX), &json);
        assert!(valid_session_entry(&e).is_err());
    }
}

#[cfg(test)]
mod session_runtime_binding_tests {
    use super::*;
    use crate::memory::SqliteMemory;
    use tempfile::TempDir;

    #[test]
    fn resumed_session_uses_runtime_provider_model_for_current_chat() {
        let mut session = session::ChatSession::new("old-provider", "old-model");
        session.title = "resumed".to_string();
        session.add_user_turn("hello");

        bind_session_to_runtime_provider_model(&mut session, "moonshot", "kimi-k2.5");

        assert_eq!(session.provider, "moonshot");
        assert_eq!(session.model, "kimi-k2.5");
        assert_eq!(session.title, "resumed");
        assert_eq!(session.turn_count(), 1);
    }

    #[tokio::test]
    async fn chat_entrypoint_records_user_and_assistant_message_events() {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), tmp.path().to_string_lossy());
        let session = session::ChatSession::new("mock", "mock-model");
        let session_key = format!("chat:{}", session.id);
        let run_id = "chat-run-test";

        let user_event = record_chat_user_message_event(
            &fabric,
            &session,
            &session_key,
            run_id,
            "mock",
            "mock-model",
            1,
            "hello from chat",
        )
        .await
        .unwrap();
        record_chat_assistant_message_event(
            &fabric,
            &session_key,
            run_id,
            "mock",
            "mock-model",
            "hello from assistant",
        )
        .await
        .unwrap();

        // D4 C6: the durable session_key is now the stable canonical derived from
        // the session id (NOT legacy chat:{id}, NOT {provider}/{model}). Recall by
        // the canonical key — exactly what chat_runtime_envelope reads — returns
        // both events.
        let canonical_key = RuntimeEnvelope::chat_canonical_session_key(&session.id.to_string());
        let events = memory
            .list_message_events_since(
                &MemoryPrincipal {
                    workspace_id: tmp.path().to_string_lossy().to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: Some(canonical_key.clone()),
                    channel: Some("terminal".to_string()),
                    sender: Some("local-user".to_string()),
                    owner_id: None,
                    legacy_session_key: None,
                },
                0,
                10,
            )
            .await
            .unwrap();

        assert_eq!(events.len(), 2);
        // The durable session_key truly changed to canonical (not just recipient).
        assert_eq!(
            events.first().and_then(|e| e.session_key.as_deref()),
            Some(canonical_key.as_str())
        );
        assert_ne!(
            events.first().and_then(|e| e.session_key.as_deref()),
            Some(session_key.as_str())
        );
        let user_recorded = events.first();
        let assistant_recorded = events.get(1);
        assert_eq!(user_recorded.map(|event| event.source.as_str()), Some("chat"));
        assert_eq!(user_recorded.map(|event| event.role.as_str()), Some("user"));
        assert_eq!(
            user_recorded.map(|event| event.content.as_str()),
            Some("hello from chat")
        );
        assert_eq!(
            user_recorded.map(|event| event.event_id.as_str()),
            Some(user_event.event_id.as_str())
        );
        assert_eq!(assistant_recorded.map(|event| event.role.as_str()), Some("assistant"));
        assert_eq!(
            assistant_recorded.and_then(|event| event.sender.as_deref()),
            Some("mock/mock-model")
        );
        assert_eq!(
            assistant_recorded.and_then(|event| event.recipient.as_deref()),
            Some("local-user")
        );

        // Write/read durable canonical strict equality (C6 / R6): the read
        // envelope's principal session_key equals the persisted event session_key.
        let read_envelope = chat_runtime_envelope(&tmp.path().to_string_lossy(), &session_key);
        assert_eq!(
            read_envelope.memory_principal().session_key.as_deref(),
            Some(canonical_key.as_str())
        );
        assert_eq!(
            read_envelope.message_scope().session_key.as_deref(),
            Some(canonical_key.as_str())
        );
        // legacy chat:{id} carried for read-merge.
        assert_eq!(
            read_envelope.memory_principal().legacy_session_key.as_deref(),
            Some(session_key.as_str())
        );

        // Read-merge proof on the session-key-filtered path. load_recent_session_context
        // applies a hard `session_key` filter for every visibility, so it is the
        // path where the canonical migration + legacy union actually matters. A
        // pre-cutover legacy event must recall via read-merge under the canonical key.
        memory
            .append_message_event(crate::memory::MessageEventInput {
                event_id: None,
                idempotency_key: None,
                workspace_id: tmp.path().to_string_lossy().to_string(),
                owner_id: None,
                source: "chat".into(),
                channel: Some("terminal".to_string()),
                session_key: Some(session_key.clone()),
                parent_session_key: None,
                run_id: None,
                parent_run_id: None,
                agent_id: None,
                persona_id: None,
                sender: Some("local-user".to_string()),
                recipient: None,
                role: "user".to_string(),
                event_type: "message.created".to_string(),
                subject: None,
                goal_id: None,
                causation_event_id: None,
                correlation_id: None,
                attempt_id: None,
                lease_epoch: None,
                content: "legacy pre-cutover turn".to_string(),
                raw_payload_json: None,
                visibility: crate::memory::MemoryVisibility::Workspace,
            })
            .await
            .unwrap();
        let merged = memory
            .load_recent_session_context(crate::memory::SessionContextQuery {
                principal: read_envelope.memory_principal(),
                since_event_id: None,
                limit: 20,
                include_roles: vec!["user".to_string(), "assistant".to_string()],
            })
            .await
            .unwrap();
        assert!(
            merged.iter().any(|event| event.content == "legacy pre-cutover turn"),
            "legacy history must read-merge under canonical key on the session-filtered path"
        );
        assert!(merged.iter().any(|event| event.content == "hello from chat"));

        // Without the legacy key, the same session-filtered path sees only the
        // canonical history (single-key degradation), not the legacy turn.
        let canonical_only = crate::memory::MemoryPrincipal {
            legacy_session_key: None,
            ..read_envelope.memory_principal()
        };
        let single = memory
            .load_recent_session_context(crate::memory::SessionContextQuery {
                principal: canonical_only,
                since_event_id: None,
                limit: 20,
                include_roles: vec!["user".to_string(), "assistant".to_string()],
            })
            .await
            .unwrap();
        assert!(single.iter().any(|event| event.content == "hello from chat"));
        assert!(
            !single.iter().any(|event| event.content == "legacy pre-cutover turn"),
            "single-key path must NOT see legacy history"
        );
    }
}

#[cfg(test)]
mod chat_message_event_projection_tests {
    use super::*;
    use crate::llm::route_decision::TokenUsage;
    use crate::memory::SqliteMemory;
    use tempfile::TempDir;

    #[tokio::test]
    async fn equivalent_events_drive_resume_export_and_cost_without_output_drift() {
        let tmp = TempDir::new().unwrap();
        let workspace_id = tmp.path().to_string_lossy().to_string();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), workspace_id.clone());
        let cost_config = crate::config::schema::CostConfig::default();

        let mut snapshot = session::ChatSession::new("anthropic", "claude-sonnet-4");
        snapshot.id = "event-projection-session".to_string();
        snapshot.add_user_turn("hello from the blob");
        snapshot.add_assistant_turn("hello from the event log", Vec::new());
        let decision = RouteDecision::single_candidate("anthropic", "claude-sonnet-4");
        let outcome = ProviderExecutionOutcome::success_for_decision_with_usage(
            &decision,
            chrono::Utc::now(),
            TokenUsage::reported(Some(120), Some(30), Some(150)),
        );
        snapshot.record_provider_usage(&outcome, &cost_config).unwrap();
        save_session(memory.as_ref(), &snapshot).await.unwrap();

        let session_key = format!("chat:{}", snapshot.id);
        record_chat_user_message_event(
            &fabric,
            &snapshot,
            &session_key,
            "turn-1",
            "anthropic",
            "claude-sonnet-4",
            1,
            "hello from the blob",
        )
        .await
        .unwrap();
        record_provider_outcome_events(
            &fabric,
            route_event_scope(
                "chat",
                None,
                Some(session_key.clone()),
                Some("turn-1".to_string()),
                Some("local-user".to_string()),
                Some("anthropic/claude-sonnet-4".to_string()),
            ),
            &outcome,
        )
        .await
        .unwrap();
        record_chat_assistant_message_event(
            &fabric,
            &session_key,
            "turn-1",
            "anthropic",
            "claude-sonnet-4",
            "hello from the event log",
        )
        .await
        .unwrap();

        // Workspace-visible noise from another chat must never enter this projection.
        let noise = session::ChatSession::new("other", "model");
        record_chat_user_message_event(
            &fabric,
            &noise,
            &format!("chat:{}", noise.id),
            "noise-turn",
            "other",
            "model",
            1,
            "unrelated workspace event",
        )
        .await
        .unwrap();

        let before_json = snapshot.to_json().unwrap();
        let before_cost = commands::format_cost_feedback(&snapshot);
        let projected =
            project_chat_session_from_message_events(memory.as_ref(), &workspace_id, snapshot.clone(), &cost_config)
                .await;

        assert!(projected.turns_from_events, "equivalent turns must use MessageEvent");
        assert!(
            projected.usage_from_events,
            "equivalent usage must use provider outcome events"
        );
        assert_eq!(
            projected.session.to_json().unwrap(),
            before_json,
            "export projection drifted"
        );
        assert_eq!(commands::format_cost_feedback(&projected.session), before_cost);

        let resumed =
            load_session_by_id_with_message_events(memory.as_ref(), &workspace_id, &snapshot.id, &cost_config)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(resumed.to_json().unwrap(), before_json, "resume projection drifted");
    }

    #[tokio::test]
    async fn non_equivalent_events_keep_the_blob_compatibility_snapshot() {
        let tmp = TempDir::new().unwrap();
        let workspace_id = tmp.path().to_string_lossy().to_string();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), workspace_id.clone());
        let cost_config = crate::config::schema::CostConfig::default();
        let mut snapshot = session::ChatSession::new("provider", "model");
        snapshot.id = "event-fallback-session".to_string();
        snapshot.add_user_turn("blob is authoritative until parity");
        let before = snapshot.to_json().unwrap();

        record_chat_user_message_event(
            &fabric,
            &snapshot,
            &format!("chat:{}", snapshot.id),
            "turn-1",
            "provider",
            "model",
            1,
            "different event content",
        )
        .await
        .unwrap();

        let projected =
            project_chat_session_from_message_events(memory.as_ref(), &workspace_id, snapshot, &cost_config).await;
        assert!(!projected.turns_from_events);
        assert!(!projected.usage_from_events);
        assert_eq!(projected.session.to_json().unwrap(), before);
    }

    #[tokio::test]
    async fn unsupported_event_reader_keeps_the_blob_compatibility_snapshot() {
        let mut snapshot = session::ChatSession::new("provider", "model");
        snapshot.id = "blob-only-session".to_string();
        snapshot.add_user_turn("backend has no event log");
        let before = snapshot.to_json().unwrap();

        let projected = project_chat_session_from_message_events(
            &crate::memory::NoneMemory::new(),
            "workspace",
            snapshot,
            &crate::config::schema::CostConfig::default(),
        )
        .await;

        assert!(!projected.turns_from_events);
        assert!(!projected.usage_from_events);
        assert_eq!(projected.session.to_json().unwrap(), before);
    }
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
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

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

#[cfg(test)]
mod legacy_chat_compaction_audit_tests {
    use super::*;
    use crate::memory::SqliteMemory;
    use tempfile::TempDir;

    #[test]
    fn legacy_chat_compaction_audit_source_is_bounded() {
        let mut source_history = vec![ChatMessage::system("system rules ".repeat(200))];
        for idx in 0..20 {
            source_history.push(ChatMessage::user(format!("turn {idx} {}", "payload ".repeat(200))));
        }

        let bounded = bounded_legacy_chat_compaction_audit_source(&source_history);
        assert_eq!(bounded.first().map(|msg| msg.role.as_str()), Some("system"));
        assert!(
            bounded.len() <= COMPACT_KEEP_MESSAGES + 1,
            "audit projection should not retain every historical turn"
        );
        assert!(
            bounded
                .iter()
                .all(|msg| msg.content.chars().count() <= COMPACT_CONTENT_CHARS + 3),
            "audit projection should truncate each retained message"
        );
    }

    #[test]
    fn legacy_chat_compaction_original_audit_source_tracks_untruncated_losses() {
        let mut source_history = vec![ChatMessage::system("system rules")];
        for idx in 0..12 {
            source_history.push(ChatMessage::user(format!(
                "evicted-original-{idx} {}",
                "payload ".repeat(8)
            )));
        }

        let audit_source = original_legacy_chat_compaction_audit_source(&source_history);

        assert_eq!(
            audit_source.len(),
            4,
            "12 non-system messages with keep window 8 must audit the 4 original evicted turns"
        );
        assert_eq!(
            audit_source
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            source_history
                .iter()
                .skip(1)
                .take(4)
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            "audit source must retain the original untruncated evicted turns in order"
        );
    }

    #[tokio::test]
    async fn legacy_chat_compaction_persists_run_and_summary_memory() {
        let tmp = TempDir::new().unwrap();
        let mem: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let envelope = RuntimeEnvelope::chat("workspace-a", "session-a");
        let mut source_history = vec![ChatMessage::system("system rules")];
        for idx in 0..12 {
            source_history.push(ChatMessage::user(format!(
                "evicted-source-{idx} /tmp/source-a owner lineage {}",
                "payload ".repeat(8)
            )));
        }
        let audit_source = original_legacy_chat_compaction_audit_source(&source_history);
        let summary_projection = bounded_legacy_chat_compaction_audit_source(&source_history);
        let fabric = MemoryFabric::new(mem.clone(), "workspace-a");
        let mut expected_source_event_ids = Vec::new();
        for (index, message) in audit_source.iter().enumerate() {
            let event = fabric
                .record_inbound_user_message(
                    envelope.message_scope(),
                    message.content.clone(),
                    Some(format!("legacy-compaction-source-{index}")),
                    None,
                )
                .await
                .unwrap();
            expected_source_event_ids.push(event.event_id);
        }
        let expected_source_tokens = estimate_chat_history_tokens(&audit_source);
        let provider_history = {
            let mut history = source_history.clone();
            history.push(ChatMessage::user(format!(
                "visible @file.txt\n\n[Attached file context from @path mentions]\n{}",
                "hidden enrichment ".repeat(20)
            )));
            history
        };
        let token_metadata = legacy_compaction_token_metadata(&provider_history, &source_history);

        persist_legacy_chat_compaction_audit(
            mem.as_ref(),
            &envelope,
            &audit_source,
            &summary_projection,
            token_metadata,
            "chat_context_overflow",
        )
        .await;

        let conn = rusqlite::Connection::open(tmp.path().join("memory").join("brain.db")).unwrap();
        let (
            summary_memory_key,
            mode,
            fidelity_status,
            source_message_count,
            source_token_estimate,
            source_event_ids_json,
            source_event_range_json,
            source_document_refs_json,
            payload_json,
        ): (String, String, String, i64, i64, String, String, Option<String>, String) = conn
            .query_row(
                "SELECT summary_memory_key, mode, fidelity_status, source_message_count,
                        source_token_estimate, source_event_ids_json,
                        source_event_range_json, source_document_refs_json, payload_json
                 FROM compaction_runs
                 WHERE workspace_id = 'workspace-a'
                 ORDER BY id DESC
                 LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                    ))
                },
            )
            .unwrap();
        assert!(summary_memory_key.starts_with("compaction_summary_"));
        assert_eq!(mode, "legacy_chat_overflow");
        assert_eq!(fidelity_status, "accepted_legacy_deterministic");
        assert_eq!(source_message_count, 4);
        assert_eq!(source_token_estimate, expected_source_tokens as i64);
        let source_event_ids: Vec<String> = serde_json::from_str(&source_event_ids_json).unwrap();
        assert_eq!(source_event_ids, expected_source_event_ids);
        let covered_range: crate::memory::CompactionSourceEventRange =
            serde_json::from_str(&source_event_range_json).unwrap();
        assert_eq!(covered_range.first_event_id, expected_source_event_ids[0]);
        assert_eq!(covered_range.last_event_id, expected_source_event_ids[3]);
        assert_eq!(covered_range.source_event_count, 4);
        assert!(source_document_refs_json.is_none());
        let payload: serde_json::Value = serde_json::from_str(&payload_json).unwrap();
        assert_eq!(
            payload
                .get("provider_token_estimate")
                .and_then(serde_json::Value::as_u64),
            Some(token_metadata.provider_token_estimate as u64)
        );
        assert_eq!(
            payload
                .get("persisted_token_estimate")
                .and_then(serde_json::Value::as_u64),
            Some(token_metadata.persisted_token_estimate as u64)
        );
        assert!(
            payload
                .get("enrichment_token_delta")
                .and_then(serde_json::Value::as_i64)
                .is_some_and(|delta| delta > 0),
            "legacy audit metadata must expose non-empty provider enrichment overhead"
        );

        let (summary_event_id, summary_causation, summary_payload): (String, String, String) = conn
            .query_row(
                "SELECT event_id, causation_event_id, raw_payload_json
                 FROM message_events
                 WHERE event_type = 'compaction.summary.created'
                 ORDER BY id DESC
                 LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(summary_causation, expected_source_event_ids[3]);
        let summary_payload: serde_json::Value = serde_json::from_str(&summary_payload).unwrap();
        assert_eq!(
            summary_payload
                .get("covered_event_range")
                .and_then(|range| range.get("last_event_id"))
                .and_then(serde_json::Value::as_str),
            Some(expected_source_event_ids[3].as_str())
        );
        assert_eq!(
            payload
                .get("summary_message_event_id")
                .and_then(serde_json::Value::as_str),
            Some(summary_event_id.as_str())
        );

        let stored_summary_count: i64 = conn
            .query_row(
                "SELECT COUNT(*)
                 FROM memories
                 WHERE key = ?1
                   AND source = 'legacy_chat_compaction_summary'
                   AND session_id = ?2",
                [&summary_memory_key, &envelope.session_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_summary_count, 1);
    }

    /// D8-2: chat run_id is per-turn. Two turns within one session must produce
    /// two distinct, non-empty run_ids on their message events, and neither turn
    /// may set parent_run_id (session relation lives in the session key, not the
    /// run lineage).
    #[tokio::test]
    async fn chat_per_turn_run_ids_are_distinct_and_have_no_parent() {
        let tmp = TempDir::new().unwrap();
        let mem: Arc<dyn crate::memory::Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let fabric = MemoryFabric::new(mem, "workspace-d8".to_string()).with_event_recording(
            crate::memory::MemoryEventRecording {
                enabled: true,
                record_user_messages: true,
                record_assistant_messages: true,
                ..crate::memory::MemoryEventRecording::default()
            },
        );
        let session = session::ChatSession::new("test-provider", "test-model");
        let session_key = format!("chat:{}", session.id);

        // Two turns, each with its own freshly generated run_id (mirroring the
        // per-turn generation at the turn-loop entry).
        let turn1_run_id = uuid::Uuid::new_v4().to_string();
        let user1 = record_chat_user_message_event(
            &fabric,
            &session,
            &session_key,
            &turn1_run_id,
            "test-provider",
            "test-model",
            1,
            "first turn",
        )
        .await
        .unwrap();
        let asst1 = record_chat_assistant_message_event(
            &fabric,
            &session_key,
            &turn1_run_id,
            "test-provider",
            "test-model",
            "first reply",
        )
        .await
        .unwrap();

        let turn2_run_id = uuid::Uuid::new_v4().to_string();
        let user2 = record_chat_user_message_event(
            &fabric,
            &session,
            &session_key,
            &turn2_run_id,
            "test-provider",
            "test-model",
            2,
            "second turn",
        )
        .await
        .unwrap();

        assert_eq!(user1.run_id.as_deref(), Some(turn1_run_id.as_str()));
        assert_eq!(asst1.run_id.as_deref(), Some(turn1_run_id.as_str()));
        assert_eq!(user2.run_id.as_deref(), Some(turn2_run_id.as_str()));
        assert_ne!(turn1_run_id, turn2_run_id, "each turn must get a distinct run_id");
        assert!(!turn1_run_id.is_empty() && !turn2_run_id.is_empty());
        assert!(
            user1.parent_run_id.is_none() && asst1.parent_run_id.is_none() && user2.parent_run_id.is_none(),
            "chat turns must not set parent_run_id (session relation is via session_key)"
        );
    }

    #[tokio::test]
    async fn save_and_message_event_boundaries_redact_aws_keys() {
        let secret = "AKIAABCDEFGHIJKLMNOP";
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), tmp.path().to_string_lossy());
        let mut session = session::ChatSession::new("mock", "model");
        session.title = format!("标题 {secret}");
        session.add_user_turn(&format!("用户 你好 {secret}"));
        session.add_assistant_turn(&format!("助手 مرحبا {secret}"), Vec::new());

        save_session(memory.as_ref(), &session).await.unwrap();
        let stored = memory.get(&session.memory_key()).await.unwrap().unwrap();
        assert!(!stored.content.contains(secret), "stored Memory content leaked AWS key");
        let recalled = memory.recall(secret, 10, None).await.unwrap();
        assert!(recalled.iter().all(|entry| !entry.content.contains(secret)));

        let session_key = format!("chat:{}", session.id);
        let user_event = record_chat_user_message_event(
            &fabric,
            &session,
            &session_key,
            "run-secret",
            "mock",
            "model",
            1,
            &format!("用户 {secret} 你好"),
        )
        .await
        .unwrap();
        let assistant_event = record_chat_assistant_message_event(
            &fabric,
            &session_key,
            "run-secret",
            "mock",
            "model",
            &format!("助手 {secret} 🌍"),
        )
        .await
        .unwrap();
        assert!(!user_event.content.contains(secret));
        assert!(!assistant_event.content.contains(secret));
        assert!(user_event.content.contains("你好"));
        assert!(assistant_event.content.contains('🌍'));

        let semantic_key = "chat_auto_promote_secret";
        let safe_semantic = sanitize_chat_semantic_memory_content(&format!("promote {secret} Unicode 你好"));
        fabric
            .record_semantic_memory_from_event(
                semantic_key,
                &safe_semantic,
                MemoryCategory::Conversation,
                None,
                Some(&user_event.event_id),
                None,
                None,
            )
            .await
            .unwrap();
        let semantic = memory.get(semantic_key).await.unwrap().unwrap();
        assert!(!semantic.content.contains(secret));
        assert!(semantic.content.contains("你好"));
        let recalled = memory.recall(secret, 10, None).await.unwrap();
        assert!(recalled.iter().all(|entry| !entry.content.contains(secret)));
    }

    #[tokio::test]
    async fn chat_route_payloads_remain_valid_json_and_redact_nested_secrets() {
        let secret = "AKIAABCDEFGHIJKLMNOP";
        let bearer = "Authorization: Bearer abcdefghijklmnop";
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), tmp.path().to_string_lossy());
        let session_key = "chat-route-sanitize".to_string();
        let scope = route_event_scope(
            "chat",
            None,
            Some(session_key.clone()),
            Some("route-run".to_string()),
            Some("local-user".to_string()),
            None,
        );
        let mut decision = RouteDecision::single_candidate("provider", "model");
        decision.user_hint = Some(format!("hint {secret}"));
        decision
            .filtered_out
            .push(crate::llm::route_decision::RouteFilterReason {
                provider: "fallback".to_string(),
                model: "model".to_string(),
                reason: "auth".to_string(),
                detail: Some(bearer.to_string()),
            });
        let route_event = record_route_decision_event(&fabric, scope.clone(), &decision)
            .await
            .unwrap();
        let route_payload = route_event.raw_payload_json.as_deref().unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(route_payload).is_ok());
        assert!(!route_payload.contains(secret));
        assert!(!route_payload.contains("abcdefghijklmnop"));

        let error = anyhow::anyhow!("provider failed: {bearer}; aws={secret}");
        let outcome = ProviderExecutionOutcome::failed_for_decision(&decision, chrono::Utc::now(), &error);
        record_provider_outcome_events(&fabric, scope, &outcome).await.unwrap();
        let events = memory
            .list_message_events_since(
                &MemoryPrincipal {
                    workspace_id: tmp.path().to_string_lossy().to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: Some(session_key),
                    channel: Some("runtime".to_string()),
                    sender: Some("local-user".to_string()),
                    owner_id: None,
                    legacy_session_key: None,
                },
                0,
                20,
            )
            .await
            .unwrap();
        assert!(!events.is_empty());
        for payload in events.iter().filter_map(|event| event.raw_payload_json.as_deref()) {
            assert!(serde_json::from_str::<serde_json::Value>(payload).is_ok());
            assert!(!payload.contains(secret));
            assert!(!payload.contains("abcdefghijklmnop"));
        }
    }
}

#[cfg(test)]
mod file_mention_tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use crate::tools::FileReadTool;
    use crate::tools::traits::{ToolCategory, ToolResult, ToolTier};
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockReadTool {
        calls: Arc<AtomicUsize>,
        max_bytes_seen: Arc<Mutex<Vec<Option<u64>>>>,
    }

    #[async_trait]
    impl Tool for MockReadTool {
        fn name(&self) -> &str {
            "file_read"
        }

        fn description(&self) -> &str {
            "mock file read"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            })
        }

        async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.max_bytes_seen
                .lock()
                .push(args.get("max_bytes").and_then(serde_json::Value::as_u64));
            let path = args.get("path").and_then(serde_json::Value::as_str).unwrap_or_default();
            Ok(ToolResult {
                success: true,
                output: format!("content for {path}"),
                error: None,
            })
        }

        fn tier(&self) -> ToolTier {
            ToolTier::Core
        }

        fn categories(&self) -> &'static [ToolCategory] {
            &[ToolCategory::FileSystem]
        }
    }

    fn file_read_registry(workspace: &std::path::Path, acl_enabled: bool) -> Vec<Box<dyn Tool>> {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace.to_path_buf(),
            max_actions_per_hour: 100,
            ..SecurityPolicy::default()
        });
        vec![Box::new(FileReadTool::new(security, acl_enabled))]
    }

    #[test]
    fn file_mentions_parse_multiple_cjk_and_ignore_email_bare_quote() {
        let mentions = extract_file_mentions(
            "read @src/lib.rs and @./README.md 邮件 a@example.com bare @ quoted @\"two words.txt\" @目录/文件.rs",
        );

        assert_eq!(
            mentions,
            vec![
                FileMention {
                    token: "@src/lib.rs".to_string(),
                    path: "src/lib.rs".to_string(),
                },
                FileMention {
                    token: "@./README.md".to_string(),
                    path: "./README.md".to_string(),
                },
                FileMention {
                    token: "@目录/文件.rs".to_string(),
                    path: "目录/文件.rs".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn file_mention_success_uses_file_read_tool_and_preserves_original_text() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("note.txt"), "hello from mention").expect("write note");
        let registry = file_read_registry(temp.path(), false);

        let enriched = enrich_file_mentions_for_prompt("please inspect @note.txt", &registry).await;

        assert!(enriched.prompt.starts_with("please inspect @note.txt"));
        assert!(enriched.prompt.contains("hello from mention"));
        assert!(enriched.visible_note.is_none());
    }

    #[tokio::test]
    async fn file_mention_security_negatives_are_soft_visible_and_generic() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(temp.path().join("memory")).expect("create memory");
        std::fs::write(temp.path().join("MEMORY.md"), "protected memory").expect("write memory");

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = tempfile::tempdir().expect("outside tempdir");
            std::fs::write(outside.path().join("secret.txt"), "outside secret").expect("write outside");
            symlink(outside.path().join("secret.txt"), temp.path().join("escape.txt")).expect("symlink escape");

            let registry = file_read_registry(temp.path(), true);
            let enriched = enrich_file_mentions_for_prompt(
                "check @../../etc/passwd @/etc/passwd @escape.txt @MEMORY.md @missing.txt",
                &registry,
            )
            .await;

            assert!(enriched.prompt.starts_with("check @../../etc/passwd"));
            assert!(!enriched.prompt.contains("outside secret"));
            assert!(!enriched.prompt.contains("protected memory"));
            let note = enriched.visible_note.expect("visible failure notes");
            assert!(note.contains("@../../etc/passwd: unavailable (blocked by policy)"));
            assert!(note.contains("@/etc/passwd: unavailable (blocked by policy)"));
            assert!(note.contains("@escape.txt: unavailable (blocked by policy)"));
            assert!(note.contains("@MEMORY.md: unavailable (blocked by policy)"));
            assert!(note.contains("@missing.txt: unavailable (missing or inaccessible)"));
        }
    }

    #[tokio::test]
    async fn file_mention_directory_is_rejected_softly() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(temp.path().join("dir")).expect("create dir");
        let registry = file_read_registry(temp.path(), false);

        let enriched = enrich_file_mentions_for_prompt("inspect @dir", &registry).await;

        let note = enriched.visible_note.expect("visible directory note");
        assert!(note.contains("@dir: unavailable"));
        assert!(enriched.prompt.starts_with("inspect @dir"));
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn at_path_candidates_are_relative_sorted_and_security_filtered() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(temp.path().join("src")).expect("create src");
        std::fs::write(temp.path().join("src").join("main.rs"), "fn main() {}\n").expect("write main");
        std::fs::write(temp.path().join("setup.rs"), "// setup\n").expect("write setup");

        #[cfg(unix)]
        let outside = tempfile::tempdir().expect("outside");
        #[cfg(unix)]
        {
            std::fs::write(outside.path().join("secret.rs"), "secret\n").expect("write outside");
            std::os::unix::fs::symlink(outside.path().join("secret.rs"), temp.path().join("secret.rs"))
                .expect("symlink");
        }

        let policy = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: temp.path().to_path_buf(),
            forbidden_paths: Vec::new(),
            ..SecurityPolicy::default()
        };
        let mut input = tui::TuiInput::new();
        input.set_text("@s");

        let candidates = collect_at_path_candidates(&input, temp.path(), &policy);

        let paths = candidates
            .iter()
            .map(|candidate| candidate.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths.first().copied(), Some("src/"), "directories sort first");
        assert!(paths.contains(&"setup.rs"));
        assert!(
            !paths.contains(&"secret.rs"),
            "symlink escaping workspace must be filtered by resolved-path policy"
        );

        input.set_text("@../");
        assert!(
            collect_at_path_candidates(&input, temp.path(), &policy).is_empty(),
            "traversal filter is blocked before directory enumeration"
        );
    }

    #[tokio::test]
    async fn file_mention_caps_file_count_and_bytes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let max_bytes_seen = Arc::new(Mutex::new(Vec::new()));
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(MockReadTool {
            calls: Arc::clone(&calls),
            max_bytes_seen: Arc::clone(&max_bytes_seen),
        })];

        let enriched = enrich_file_mentions_for_prompt("x @a @b @c @d @e @f", &registry).await;

        assert_eq!(calls.load(Ordering::SeqCst), FILE_MENTION_MAX_FILES);
        assert_eq!(
            max_bytes_seen.lock().as_slice(),
            &[Some(FILE_MENTION_MAX_BYTES as u64); FILE_MENTION_MAX_FILES],
            "@path expansion must pass a file_read byte cap before prompt enrichment"
        );
        assert!(enriched.prompt.contains("content for a"));
        assert!(!enriched.prompt.contains("content for f"));
        assert!(enriched.prompt.contains("1 file mention(s) skipped"));
        assert!(
            enriched
                .visible_note
                .expect("visible max-files note")
                .contains("skipped")
        );
    }

    #[tokio::test]
    async fn file_mention_truncates_utf8_on_char_boundary() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("big.txt"), "你".repeat(30_000)).expect("write big");
        let registry = file_read_registry(temp.path(), false);

        let enriched = enrich_file_mentions_for_prompt("read @big.txt", &registry).await;

        assert!(enriched.prompt.contains("[content truncated: 64 KiB limit]"));
        let attached_content = enriched
            .prompt
            .split("Path: big.txt\n\n")
            .nth(1)
            .expect("attached content")
            .split("\n[content truncated: 64 KiB limit]")
            .next()
            .expect("truncated content");
        assert!(
            attached_content.len() <= FILE_MENTION_MAX_BYTES,
            "attached content must be byte-capped, got {} bytes",
            attached_content.len()
        );
        assert!(attached_content.is_char_boundary(attached_content.len()));
        assert!(
            enriched
                .visible_note
                .expect("visible truncation note")
                .contains("@big.txt: content truncated to 64 KiB")
        );
    }
}

#[cfg(test)]
mod finalize_draft_fallback_tests {
    //! Tests for the P1-4 idempotency contract: when `finalize_draft` fails
    //! for an active draft, the chat loop must NOT re-send the full response
    //! (the streamed deltas already delivered it). The "no draft" path is a
    //! genuine first-send and is still allowed to call `send`.
    use super::*;
    use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Pure decision test: with an active draft, finalize failure must NOT trigger
    /// a resend (would duplicate what the user already saw via stream).
    #[test]
    fn finalize_failure_with_active_draft_does_not_resend() {
        assert!(
            !should_resend_on_finalize_failure(true),
            "active-draft finalize failure must be idempotent (no resend)"
        );
    }

    /// Pure decision test: with no active draft, the caller still uses `send`
    /// directly — that path is the first send, not a fallback resend.
    /// `should_resend_on_finalize_failure(false)` returning `true` simply
    /// documents that the "no draft" branch's normal send is allowed.
    #[test]
    fn no_active_draft_path_allows_send() {
        assert!(
            should_resend_on_finalize_failure(false),
            "no-draft path must allow the normal send (this is a first send, not a duplicate)"
        );
    }

    /// Mock channel that lets us script `finalize_draft` to fail and counts
    /// every method call so we can assert no fallback resend occurs.
    struct MockChannel {
        finalize_should_fail: bool,
        send_calls: AtomicUsize,
        finalize_calls: AtomicUsize,
    }

    impl MockChannel {
        fn new(finalize_should_fail: bool) -> Self {
            Self {
                finalize_should_fail,
                send_calls: AtomicUsize::new(0),
                finalize_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Channel for MockChannel {
        fn name(&self) -> &str {
            "mock"
        }

        async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
            self.send_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
            Ok(())
        }

        fn supports_draft_updates(&self) -> bool {
            true
        }

        async fn send_draft(&self, _message: &SendMessage) -> anyhow::Result<Option<String>> {
            Ok(Some("draft-1".to_string()))
        }

        async fn finalize_draft(&self, _recipient: &str, _message_id: &str, _text: &str) -> anyhow::Result<()> {
            self.finalize_calls.fetch_add(1, Ordering::SeqCst);
            if self.finalize_should_fail {
                Err(anyhow::anyhow!("simulated finalize failure"))
            } else {
                Ok(())
            }
        }
    }

    /// Replicates the production fallback control flow against a mock channel.
    /// On finalize failure with an active draft, `send` must NOT be invoked.
    async fn run_finalize_path(channel: Arc<MockChannel>, draft_id: Option<String>, display_response: &str) {
        if let Some(ref d_id) = draft_id {
            if let Err(e) = channel.finalize_draft("user", d_id, display_response).await {
                if should_resend_on_finalize_failure(true) {
                    let _ = channel.send(&SendMessage::new(display_response, "user")).await;
                } else {
                    tracing::warn!(error = %e, "finalize_draft failed; suppressing resend");
                }
            }
        } else {
            // First-send path (no draft was ever created)
            let _ = channel.send(&SendMessage::new(display_response, "user")).await;
        }
    }

    #[tokio::test]
    async fn finalize_failure_with_active_draft_suppresses_send() {
        let ch = Arc::new(MockChannel::new(true));
        run_finalize_path(Arc::clone(&ch), Some("draft-1".to_string()), "hello world").await;

        assert_eq!(ch.finalize_calls.load(Ordering::SeqCst), 1, "finalize must run once");
        assert_eq!(
            ch.send_calls.load(Ordering::SeqCst),
            0,
            "finalize failure must NOT trigger a resend when a draft was active"
        );
    }

    #[tokio::test]
    async fn finalize_success_does_not_resend() {
        let ch = Arc::new(MockChannel::new(false));
        run_finalize_path(Arc::clone(&ch), Some("draft-1".to_string()), "hello world").await;

        assert_eq!(ch.finalize_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            ch.send_calls.load(Ordering::SeqCst),
            0,
            "successful finalize must not produce any extra send"
        );
    }

    #[tokio::test]
    async fn no_draft_path_sends_once() {
        let ch = Arc::new(MockChannel::new(false));
        run_finalize_path(Arc::clone(&ch), None, "hello world").await;

        assert_eq!(
            ch.finalize_calls.load(Ordering::SeqCst),
            0,
            "no-draft path must not call finalize"
        );
        assert_eq!(
            ch.send_calls.load(Ordering::SeqCst),
            1,
            "no-draft path must send the response exactly once"
        );
    }

    /// Regression guard: two consecutive turns where finalize fails on both
    /// must produce zero extra `send` calls (the previous buggy behavior
    /// resulted in 2 duplicate messages).
    #[tokio::test]
    async fn repeated_finalize_failures_never_resend() {
        let ch = Arc::new(MockChannel::new(true));
        run_finalize_path(Arc::clone(&ch), Some("draft-1".to_string()), "turn 1").await;
        run_finalize_path(Arc::clone(&ch), Some("draft-2".to_string()), "turn 2").await;

        assert_eq!(ch.finalize_calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            ch.send_calls.load(Ordering::SeqCst),
            0,
            "repeated finalize failures must remain idempotent (no resends)"
        );
    }
}

#[cfg(all(test, feature = "terminal-tui"))]
mod reasoning_extraction_tests {
    //! Tests for `collect_reasoning_from_history_slice` — the P2-12 helper that
    //! pulls reasoning content from the agent loop's history JSON and feeds it
    //! to the TUI's foldable reasoning card.
    use super::*;

    fn assistant_json(content: Option<&str>, reasoning: Option<&str>) -> ChatMessage {
        let mut obj = serde_json::json!({"content": content, "tool_calls": []});
        if let Some(rc) = reasoning {
            if let Some(map) = obj.as_object_mut() {
                map.insert(
                    "reasoning_content".to_string(),
                    serde_json::Value::String(rc.to_string()),
                );
            }
        }
        ChatMessage::assistant(obj.to_string())
    }

    #[test]
    fn empty_slice_returns_empty_string() {
        assert_eq!(collect_reasoning_from_history_slice(&[]), "");
    }

    #[test]
    fn plain_assistant_text_is_skipped() {
        let history = vec![ChatMessage::assistant("Just a plain answer.")];
        assert_eq!(collect_reasoning_from_history_slice(&history), "");
    }

    #[test]
    fn extracts_single_reasoning_block() {
        let history = vec![assistant_json(Some("ok"), Some("Step 1: think.\nStep 2: act."))];
        let agg = collect_reasoning_from_history_slice(&history);
        assert_eq!(agg, "Step 1: think.\nStep 2: act.");
    }

    #[test]
    fn aggregates_multiple_reasoning_blocks_with_blank_line_separator() {
        let history = vec![
            assistant_json(Some("a"), Some("first thought")),
            ChatMessage::user("user follow-up"),
            assistant_json(Some("b"), Some("second thought")),
        ];
        let agg = collect_reasoning_from_history_slice(&history);
        assert_eq!(agg, "first thought\n\nsecond thought");
    }

    #[test]
    fn whitespace_only_reasoning_dropped() {
        let history = vec![
            assistant_json(Some("a"), Some("   \n\t  ")),
            assistant_json(Some("b"), Some("real reasoning")),
        ];
        let agg = collect_reasoning_from_history_slice(&history);
        assert_eq!(agg, "real reasoning");
    }

    #[test]
    fn malformed_json_is_safely_skipped() {
        // Pre-filter passes (contains "reasoning_content") but JSON parse fails.
        let history = vec![ChatMessage::assistant(
            "broken json with reasoning_content but not valid".to_string(),
        )];
        // Must not panic and must not surface anything.
        assert_eq!(collect_reasoning_from_history_slice(&history), "");
    }

    #[test]
    fn non_assistant_role_ignored_even_with_reasoning_content() {
        // A user message that happens to contain the literal "reasoning_content"
        // must never leak into the reasoning card.
        let history = vec![ChatMessage::user(
            "{\"reasoning_content\":\"shouldn't appear\"}".to_string(),
        )];
        assert_eq!(collect_reasoning_from_history_slice(&history), "");
    }
}

#[cfg(test)]
mod terminal_guard_tests {
    //! Tests for P3-2: `TerminalGuard` RAII + strengthened panic hook.
    //!
    //! These tests intentionally do NOT call `TerminalGuard::enter()` because
    //! the test harness is not connected to a real TTY — `enable_raw_mode`
    //! would fail on most CI runners. Instead we exercise:
    //!
    //!   * `leave()` idempotency on a guard constructed in the "inactive"
    //!     state (no crossterm calls issued).
    //!   * Concurrent `leave()` from multiple threads (Drop + manual).
    //!   * `restore_terminal_state()` does not panic when invoked outside
    //!     raw mode (the panic-hook fast path).
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct FakeTerminalModeOps {
        calls: Vec<&'static str>,
        fail_enable_bracketed_paste: bool,
        fail_enable_mouse_capture: bool,
        keyboard_enhancement_supported: bool,
        fail_push_keyboard_enhancement: bool,
    }

    impl TerminalModeOps for FakeTerminalModeOps {
        fn enable_raw_mode(&mut self) -> std::io::Result<()> {
            self.calls.push("enable_raw_mode");
            Ok(())
        }

        fn disable_raw_mode(&mut self) -> std::io::Result<()> {
            self.calls.push("disable_raw_mode");
            Ok(())
        }

        fn supports_keyboard_enhancement(&mut self) -> std::io::Result<bool> {
            self.calls.push("supports_keyboard_enhancement");
            Ok(self.keyboard_enhancement_supported)
        }

        fn push_keyboard_enhancement_flags(&mut self) -> std::io::Result<()> {
            self.calls.push("push_keyboard_enhancement_flags");
            if self.fail_push_keyboard_enhancement {
                Err(std::io::Error::other("push keyboard enhancement failed"))
            } else {
                Ok(())
            }
        }

        fn pop_keyboard_enhancement_flags(&mut self) -> std::io::Result<()> {
            self.calls.push("pop_keyboard_enhancement_flags");
            Ok(())
        }

        fn enable_bracketed_paste(&mut self) -> std::io::Result<()> {
            self.calls.push("enable_bracketed_paste");
            if self.fail_enable_bracketed_paste {
                Err(std::io::Error::other("enable bracketed paste failed"))
            } else {
                Ok(())
            }
        }

        fn disable_bracketed_paste(&mut self) -> std::io::Result<()> {
            self.calls.push("disable_bracketed_paste");
            Ok(())
        }

        fn enable_mouse_capture(&mut self) -> std::io::Result<()> {
            self.calls.push("enable_mouse_capture");
            if self.fail_enable_mouse_capture {
                Err(std::io::Error::other("enable mouse capture failed"))
            } else {
                Ok(())
            }
        }

        fn disable_mouse_capture(&mut self) -> std::io::Result<()> {
            self.calls.push("disable_mouse_capture");
            Ok(())
        }

        fn enter_alternate_screen(&mut self) -> std::io::Result<()> {
            self.calls.push("enter_alternate_screen");
            Ok(())
        }

        fn leave_alternate_screen(&mut self) -> std::io::Result<()> {
            self.calls.push("leave_alternate_screen");
            Ok(())
        }

        fn show_cursor(&mut self) -> std::io::Result<()> {
            self.calls.push("show_cursor");
            Ok(())
        }
    }

    /// Build a `TerminalGuard` in the inactive state (no real terminal
    /// mutation), suitable for unit-testing the bookkeeping.
    fn inactive_guard() -> TerminalGuard {
        TerminalGuard {
            raw_mode_active: AtomicBool::new(false),
            bracketed_paste_active: AtomicBool::new(false),
            keyboard_enhancement_active: AtomicBool::new(false),
            mouse_capture_active: AtomicBool::new(false),
            alternate_screen_active: AtomicBool::new(false),
        }
    }

    /// Build a "fake-active" guard whose flags are set but no real terminal
    /// state was touched. `leave()` will issue crossterm calls but they
    /// no-op safely on a non-TTY test harness (raw mode was never on, so
    /// `disable_raw_mode` is a cheap kernel call returning Ok or Err — both
    /// are swallowed).
    fn fake_active_guard() -> TerminalGuard {
        TerminalGuard {
            raw_mode_active: AtomicBool::new(true),
            bracketed_paste_active: AtomicBool::new(true),
            keyboard_enhancement_active: AtomicBool::new(true),
            mouse_capture_active: AtomicBool::new(true),
            alternate_screen_active: AtomicBool::new(true),
        }
    }

    #[test]
    fn leave_is_idempotent_on_inactive_guard() {
        let guard = inactive_guard();
        // Multiple calls must not panic and must not flip the flags.
        guard.leave();
        guard.leave();
        guard.leave();
        assert!(!guard.raw_mode_active.load(Ordering::Acquire));
        assert!(!guard.bracketed_paste_active.load(Ordering::Acquire));
        assert!(!guard.keyboard_enhancement_active.load(Ordering::Acquire));
        assert!(!guard.mouse_capture_active.load(Ordering::Acquire));
        assert!(!guard.alternate_screen_active.load(Ordering::Acquire));
    }

    #[test]
    fn leave_flips_flags_exactly_once() {
        let guard = fake_active_guard();
        assert!(guard.raw_mode_active.load(Ordering::Acquire));
        assert!(guard.bracketed_paste_active.load(Ordering::Acquire));
        assert!(guard.mouse_capture_active.load(Ordering::Acquire));
        assert!(guard.alternate_screen_active.load(Ordering::Acquire));
        guard.leave();
        assert!(!guard.raw_mode_active.load(Ordering::Acquire));
        assert!(!guard.bracketed_paste_active.load(Ordering::Acquire));
        assert!(!guard.keyboard_enhancement_active.load(Ordering::Acquire));
        assert!(!guard.mouse_capture_active.load(Ordering::Acquire));
        assert!(!guard.alternate_screen_active.load(Ordering::Acquire));
        // Second leave is a no-op (CAS fails, no crossterm calls).
        guard.leave();
        assert!(!guard.raw_mode_active.load(Ordering::Acquire));
        assert!(!guard.keyboard_enhancement_active.load(Ordering::Acquire));
        assert!(!guard.bracketed_paste_active.load(Ordering::Acquire));
        assert!(!guard.mouse_capture_active.load(Ordering::Acquire));
        assert!(!guard.alternate_screen_active.load(Ordering::Acquire));
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn plain_mode_disables_terminal_tui_even_for_tty() {
        assert!(
            !should_enable_terminal_tui(true, true, Some("1")),
            "--plain must bypass TUI even when PRX_TUI asks for TUI"
        );
        assert!(
            should_enable_terminal_tui(false, true, Some("1")),
            "TTY + PRX_TUI=1 should enable TUI when not plain"
        );
        assert!(
            !should_enable_terminal_tui(false, true, Some("0")),
            "PRX_TUI=0 remains an explicit TUI opt-out"
        );
        assert!(
            !should_enable_terminal_tui(false, false, Some("1")),
            "non-TTY stdin must not enter TUI"
        );
    }

    #[test]
    fn fullscreen_terminal_lifecycle_enters_and_leaves_alternate_screen_in_order() {
        let mut ops = FakeTerminalModeOps::default();
        let state = enter_terminal_state_with_ops(&mut ops).unwrap();
        assert!(
            state.mouse_capture_active,
            "fullscreen enables transcript mouse scrolling and drag selection by default"
        );
        leave_terminal_state_with_ops(&mut ops, state);

        assert_eq!(
            ops.calls,
            vec![
                "enable_raw_mode",
                "enter_alternate_screen",
                "enable_mouse_capture",
                "supports_keyboard_enhancement",
                "enable_bracketed_paste",
                "disable_bracketed_paste",
                "show_cursor",
                "disable_mouse_capture",
                "leave_alternate_screen",
                "disable_raw_mode"
            ]
        );
    }

    #[test]
    fn fullscreen_terminal_lifecycle_can_disable_mouse_capture() {
        let mut ops = FakeTerminalModeOps::default();
        let state = enter_terminal_state_with_ops_inner(&mut ops, false).unwrap();
        assert!(!state.mouse_capture_active);
        leave_terminal_state_with_ops(&mut ops, state);

        assert_eq!(
            ops.calls,
            vec![
                "enable_raw_mode",
                "enter_alternate_screen",
                "supports_keyboard_enhancement",
                "enable_bracketed_paste",
                "disable_bracketed_paste",
                "show_cursor",
                "leave_alternate_screen",
                "disable_raw_mode"
            ]
        );
    }

    #[test]
    fn fullscreen_enter_rolls_back_alternate_screen_when_mouse_capture_fails() {
        let mut ops = FakeTerminalModeOps {
            fail_enable_mouse_capture: true,
            ..FakeTerminalModeOps::default()
        };

        let err = enter_terminal_state_with_ops_inner(&mut ops, true).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::Other);
        assert_eq!(
            ops.calls,
            vec![
                "enable_raw_mode",
                "enter_alternate_screen",
                "enable_mouse_capture",
                "leave_alternate_screen",
                "disable_raw_mode"
            ]
        );
    }

    #[test]
    fn fullscreen_enter_rolls_back_alternate_screen_when_bracketed_paste_fails() {
        let mut ops = FakeTerminalModeOps {
            fail_enable_bracketed_paste: true,
            ..FakeTerminalModeOps::default()
        };

        let err = enter_terminal_state_with_ops(&mut ops).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::Other);
        assert_eq!(
            ops.calls,
            vec![
                "enable_raw_mode",
                "enter_alternate_screen",
                "enable_mouse_capture",
                "supports_keyboard_enhancement",
                "enable_bracketed_paste",
                "disable_mouse_capture",
                "leave_alternate_screen",
                "disable_raw_mode"
            ]
        );
    }

    #[test]
    fn fullscreen_enter_rolls_back_when_keyboard_enhancement_push_fails() {
        let mut ops = FakeTerminalModeOps {
            keyboard_enhancement_supported: true,
            fail_push_keyboard_enhancement: true,
            ..FakeTerminalModeOps::default()
        };

        let err = enter_terminal_state_with_ops(&mut ops).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::Other);
        assert!(!CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.load(Ordering::Acquire));
        assert!(
            !CHAT_FULLSCREEN_ACTIVE.load(Ordering::Acquire),
            "failed keyboard-enhancement push must roll back fullscreen active flag"
        );
        assert_eq!(
            ops.calls,
            vec![
                "enable_raw_mode",
                "enter_alternate_screen",
                "enable_mouse_capture",
                "supports_keyboard_enhancement",
                "push_keyboard_enhancement_flags",
                "disable_mouse_capture",
                "leave_alternate_screen",
                "disable_raw_mode"
            ]
        );
    }

    #[test]
    fn fullscreen_terminal_lifecycle_pushes_and_pops_keyboard_enhancement_when_supported() {
        let mut ops = FakeTerminalModeOps {
            keyboard_enhancement_supported: true,
            ..FakeTerminalModeOps::default()
        };

        let state = enter_terminal_state_with_ops(&mut ops).unwrap();
        assert!(state.keyboard_enhancement_active);
        leave_terminal_state_with_ops(&mut ops, state);

        assert_eq!(
            ops.calls,
            vec![
                "enable_raw_mode",
                "enter_alternate_screen",
                "enable_mouse_capture",
                "supports_keyboard_enhancement",
                "push_keyboard_enhancement_flags",
                "enable_bracketed_paste",
                "disable_bracketed_paste",
                "show_cursor",
                "pop_keyboard_enhancement_flags",
                "disable_mouse_capture",
                "leave_alternate_screen",
                "disable_raw_mode"
            ]
        );
    }

    #[test]
    fn panic_restore_leaves_alternate_screen_only_when_fullscreen_active() {
        let mut inline_ops = FakeTerminalModeOps::default();
        restore_terminal_state_with_ops(&mut inline_ops, false);
        assert_eq!(
            inline_ops.calls,
            vec!["disable_bracketed_paste", "show_cursor", "disable_raw_mode"]
        );

        let mut fullscreen_ops = FakeTerminalModeOps::default();
        restore_terminal_state_with_ops(&mut fullscreen_ops, true);
        assert_eq!(
            fullscreen_ops.calls,
            vec![
                "disable_bracketed_paste",
                "show_cursor",
                "leave_alternate_screen",
                "disable_raw_mode"
            ]
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn mouse_wheel_scrolls_fullscreen_transcript_by_three_rows() {
        let mut scroll = tui::FullscreenTranscriptScroll::default();

        assert!(apply_fullscreen_mouse_scroll(
            crossterm::event::MouseEventKind::ScrollUp,
            &mut scroll
        ));
        assert_eq!(scroll.offset_from_bottom, MOUSE_WHEEL_TRANSCRIPT_ROWS);

        assert!(apply_fullscreen_mouse_scroll(
            crossterm::event::MouseEventKind::ScrollDown,
            &mut scroll
        ));
        assert_eq!(scroll.offset_from_bottom, 0);

        assert!(!apply_fullscreen_mouse_scroll(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            &mut scroll
        ));
        assert_eq!(scroll.offset_from_bottom, 0);
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn plain_mode_does_not_emit_context_budget_chrome() {
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Aggressive,
            reserve_tokens: 5,
            keep_recent_messages: 2,
            memory_flush: false,
            max_context_tokens: 120,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("context pressure ".repeat(400)),
        ];
        let budget =
            crate::agent::loop_::plan_context_budget(&history, &config, crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD);
        assert!(budget.used_tokens > 0, "fixture must produce context usage");
        let terminal_tui_enabled = should_enable_terminal_tui(true, true, Some("1"));
        assert!(
            context_budget_status_for_tui(&history, &config, terminal_tui_enabled).is_none(),
            "--plain must not emit context budget chrome"
        );
        let status = context_budget_status_for_tui(&history, &config, true).expect("TUI context budget status");
        assert_eq!(status.used_context_tokens, budget.used_tokens);
        assert_eq!(status.max_context_tokens, budget.max_context_tokens);
    }

    #[test]
    fn drop_after_manual_leave_is_safe() {
        // Drop must not double-restore — the AtomicBool CAS in leave()
        // ensures the cleanup runs at most once across leave() + drop().
        let guard = fake_active_guard();
        guard.leave();
        // Implicit drop here — must be a no-op (no panic, no extra calls).
    }

    #[test]
    fn concurrent_leave_from_multiple_threads_is_safe() {
        // Simulate two threads racing to clean up the same guard
        // (e.g. manual `leave()` on the main thread + Drop on a panicking
        // background thread). Only one should win the CAS for each flag.
        let guard = Arc::new(fake_active_guard());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let g = Arc::clone(&guard);
            handles.push(std::thread::spawn(move || {
                g.leave();
            }));
        }
        for h in handles {
            h.join().expect("test: worker thread should not panic");
        }
        // After the race both flags must be cleared exactly once.
        assert!(!guard.raw_mode_active.load(Ordering::Acquire));
        assert!(!guard.bracketed_paste_active.load(Ordering::Acquire));
        assert!(!guard.mouse_capture_active.load(Ordering::Acquire));
        assert!(!guard.alternate_screen_active.load(Ordering::Acquire));
    }

    #[test]
    fn restore_terminal_state_is_safe_outside_raw_mode() {
        // The panic hook calls this from arbitrary terminal states. It must
        // never panic, even when raw mode was never enabled and we are not
        // inside an alternate screen.
        restore_terminal_state();
        // Calling twice in a row must also be safe (idempotent at the
        // crossterm level — both calls swallow errors).
        restore_terminal_state();
    }

    /// P3-6: the Resize branch in `run_tui_input_loop` only does
    /// `redraw_tx.try_send(()).ok();`. This test pins the coalescing
    /// contract of the cap=1 channel: multiple sends in a row must not
    /// block, they collapse into a single pending wakeup, and the receive
    /// side observes exactly one redraw signal until the next try_send
    /// after the recv. If this contract ever changes, the Resize handler
    /// would silently start blocking the input loop — the test fails
    /// loudly instead.
    #[cfg(feature = "terminal-tui")]
    #[test]
    fn resize_redraw_signal_coalesces() {
        use tokio::sync::mpsc;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test: runtime builds");
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel::<()>(1);
            // Burst of resize-equivalent signals: first one fills the
            // buffer, the rest must fail with Full (non-blocking, no
            // panic). This mirrors a user dragging the window border.
            for _ in 0..16 {
                let _ = tx.try_send(());
            }
            // Exactly one wakeup observable.
            rx.recv().await.expect("test: receives first wakeup");
            // Channel must be drained now.
            assert!(rx.try_recv().is_err(), "expected coalescing to drain to one signal");
            // After drain, the channel must accept a fresh send again.
            tx.try_send(()).expect("test: send after drain succeeds");
            rx.recv().await.expect("test: receives second wakeup");
        });
    }

    #[test]
    fn install_chat_panic_hook_is_idempotent() {
        // Second + later calls must be no-ops (OnceLock-guarded). This test
        // verifies the function does not panic when called repeatedly; the
        // OnceLock state is process-wide so we cannot meaningfully assert
        // which call performed the install — but the absence of panics +
        // unbounded hook nesting is the contract.
        install_chat_panic_hook();
        install_chat_panic_hook();
        install_chat_panic_hook();
    }
}

#[cfg(test)]
mod tracing_redirect_tests {
    //! P3-1: tests for `setup_chat_tracing_to_file_in` and `TracingChatGuard`.
    //!
    //! The reload handle (`crate::CHAT_TRACING_RELOAD`) is a process-wide
    //! `OnceLock` that `main()` initializes only for the `chat` subcommand,
    //! so under `cargo test` it's empty. Happy-path tests cope with both
    //! conditions: a successful redirect yields a guard, an absent reload
    //! handle yields a clean `Err` (and no panic). What we strictly assert
    //! is the I/O contract — directory creation, append-mode open,
    //! non-panicking failure modes — which is what governs whether
    //! `chat::run` can rely on this in production.
    use super::*;

    fn unique_tmpdir(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        base.join(format!("prx-p3-1-{tag}-{pid}-{nanos}"))
    }

    #[test]
    fn setup_creates_directory_and_file() {
        let dir = unique_tmpdir("create");
        let log = dir.join("chat.log");
        assert!(!dir.exists(), "precondition: tmp dir must not exist");
        let result = setup_chat_tracing_to_file_in(&dir);
        // Directory + file must be created regardless of whether the global
        // reload handle is wired up (we open the file *before* reloading).
        assert!(dir.is_dir(), "chat log dir should exist after setup");
        assert!(log.is_file(), "chat.log should exist after setup");
        // Result is Ok only when CHAT_TRACING_RELOAD is initialized (chat
        // subcommand path in main). Either outcome is non-panicking.
        match result {
            Ok(_guard) => { /* worker guard drops here → flush */ }
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("reload handle not initialized"),
                    "unexpected error variant: {msg}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_appends_does_not_truncate_existing_log() {
        let dir = unique_tmpdir("append");
        std::fs::create_dir_all(&dir).expect("test: create_dir_all must succeed");
        let log = dir.join("chat.log");
        std::fs::write(&log, b"pre-existing\n").expect("test: seed write must succeed");

        // Run setup; on Ok the guard drops immediately (flushing nothing
        // since nothing was logged); on Err we just confirm file is intact.
        let _ = setup_chat_tracing_to_file_in(&dir);

        let contents = std::fs::read_to_string(&log).expect("test: read log");
        assert!(
            contents.starts_with("pre-existing"),
            "OpenOptions::append must not truncate existing chat.log; got: {contents:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_returns_err_when_path_is_not_a_directory() {
        // Point `dir` at an existing file → create_dir_all should fail with
        // ENOTDIR / "Not a directory". Must return Err, must not panic.
        let parent = unique_tmpdir("notdir-parent");
        std::fs::create_dir_all(&parent).expect("test: create parent");
        let file_as_dir = parent.join("not-a-dir");
        std::fs::write(&file_as_dir, b"x").expect("test: seed file");

        let result = setup_chat_tracing_to_file_in(&file_as_dir);
        assert!(
            result.is_err(),
            "expected Err when target path is a regular file, got Ok"
        );
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn resolve_chat_log_dir_yields_an_absolute_path() {
        // Should succeed via either UserDirs or HOME; on container runners
        // without HOME the function bails — accept either outcome but never
        // panic.
        match resolve_chat_log_dir() {
            Ok(p) => {
                assert!(p.ends_with(".openprx"), "must point at ~/.openprx, got {p:?}");
                assert!(p.is_absolute(), "chat log dir must be absolute, got {p:?}");
            }
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("cannot determine home directory"),
                    "unexpected error: {msg}"
                );
            }
        }
    }
}

// ─── v0.4.1: Pure-only chat route contract ──────────────────────────────────

#[cfg(test)]
#[cfg(feature = "terminal-tui")]
mod redux_mode_tests {
    use super::*;

    #[test]
    fn mode_is_pure_only() {
        let mode = ReduxMode::Pure;
        assert!(mode.is_pure());
    }

    #[test]
    fn route_pure_always_driver() {
        assert!(matches!(route_turn(ReduxMode::Pure), TurnRoute::ReduxDriver));
    }
}

// ─── S4-A Commit 4: RenderSource enum 双路径 ──────────────────────────────────

#[cfg(test)]
#[cfg(feature = "terminal-tui")]
mod s4_a_4 {
    use super::*;
    use crate::chat::state::{ChatState, UiSnapshot};
    use crate::chat::tui::{ConversationLine, TuiState};
    use std::sync::Arc;
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    fn build_state_with_lines() -> ChatState {
        let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
        state.ui.conversation_lines.push(ConversationLine::User {
            content: "a".to_string(),
        });
        state.ui.conversation_lines.push(ConversationLine::Assistant {
            content: "b".to_string(),
        });
        state
    }

    /// RenderSource enum dispatch：mirror & snapshot 两种构造方式各自正确分支.
    #[test]
    fn s4_a_4_render_source_enum_dispatch() {
        // Mirror 路径.
        let tui = TuiState::new("p", "m");
        let mirror = Arc::new(parking_lot::Mutex::new(tui));
        let src_mirror = RenderSource::Mirror(Arc::clone(&mirror));
        src_mirror.with_view(|view| {
            assert_eq!(view.provider(), "p");
            assert_eq!(view.model(), "m");
        });

        // Snapshot 路径.
        let snap = Arc::new(UiSnapshot::initial(Arc::from("ps"), Arc::from("ms")));
        let (_tx, rx) = watch::channel(snap);
        let src_snap = RenderSource::Snapshot(rx);
        src_snap.with_view(|view| {
            assert_eq!(view.provider(), "ps");
            assert_eq!(view.model(), "ms");
        });
    }

    #[test]
    fn snapshot_render_source_syncs_provider_worker_status_into_key_mirror() {
        let mirror = Arc::new(parking_lot::Mutex::new(TuiState::new("p", "m")));
        let mut state = ChatState::new(Arc::from("ps"), Arc::from("ms"), CancellationToken::new());
        state.ui.provider_worker_status = crate::chat::action::ProviderWorkerStatus {
            running: 1,
            cancelling: 0,
            awaiting_commit: 0,
            finalized_payloads: 0,
            finalized_total_tokens: 0,
            oldest_started_at_ms: Some(chrono::Utc::now().timestamp_millis()),
            rows: vec![crate::chat::action::ProviderWorkerStatusRow {
                task_id: 42,
                sequence: 7,
                kind: crate::chat::action::ProviderWorkerRowKind::ForegroundAwaited,
                state: crate::chat::action::ProviderWorkerRowState::Running,
                started_at_ms: chrono::Utc::now().timestamp_millis(),
                finalized_total_tokens: None,
                completion_ready: false,
                recent_tool_call: None,
            }],
        };
        let snap = Arc::new(state.build_ui_snapshot(1));
        let (_tx, rx) = watch::channel(snap);
        let src = RenderSource::Snapshot(rx);

        sync_key_mirror_observation_state(&src, &mirror);

        let status = mirror.lock().provider_worker_status.clone();
        assert_eq!(status.running, 1);
        assert_eq!(status.rows.first().map(|row| row.sequence), Some(7));
    }

    #[test]
    fn phase2_snapshot_mirror_sync_uses_focused_worker_draft_not_primary() {
        let mirror = Arc::new(parking_lot::Mutex::new(TuiState::new("p", "m")));
        {
            let mut guard = mirror.lock();
            guard.focus = crate::chat::sessions::FocusTarget::Worker { sequence: 20 };
        }
        let mut state = ChatState::new(Arc::from("ps"), Arc::from("ms"), CancellationToken::new());
        state.ui.provider_worker_status = crate::chat::action::ProviderWorkerStatus {
            running: 2,
            cancelling: 0,
            awaiting_commit: 0,
            finalized_payloads: 0,
            finalized_total_tokens: 0,
            oldest_started_at_ms: Some(0),
            rows: vec![
                crate::chat::action::ProviderWorkerStatusRow {
                    task_id: 10,
                    sequence: 10,
                    kind: crate::chat::action::ProviderWorkerRowKind::Detached,
                    state: crate::chat::action::ProviderWorkerRowState::Running,
                    started_at_ms: 0,
                    finalized_total_tokens: None,
                    completion_ready: false,
                    recent_tool_call: None,
                },
                crate::chat::action::ProviderWorkerStatusRow {
                    task_id: 20,
                    sequence: 20,
                    kind: crate::chat::action::ProviderWorkerRowKind::Detached,
                    state: crate::chat::action::ProviderWorkerRowState::Running,
                    started_at_ms: 0,
                    finalized_total_tokens: None,
                    completion_ready: false,
                    recent_tool_call: None,
                },
            ],
        };
        let _ = state.reduce(crate::chat::action::Action::StartLLMTurn {
            provider_turn_task_id: None,
            provider_turn_sequence: Some(10),
            draft_id: "draft-a".to_string(),
            history: vec![crate::providers::ChatMessage::user("first")],
            compaction_guard_history: None,
            compaction_config: None,
            cancel: CancellationToken::new(),
            turn_spawn_ctx: None,
            turn_message_send_ctx: None,
        });
        let _ = state.reduce(crate::chat::action::Action::StartLLMTurn {
            provider_turn_task_id: None,
            provider_turn_sequence: Some(20),
            draft_id: "draft-b".to_string(),
            history: vec![crate::providers::ChatMessage::user("second")],
            compaction_guard_history: None,
            compaction_config: None,
            cancel: CancellationToken::new(),
            turn_spawn_ctx: None,
            turn_message_send_ctx: None,
        });
        let _ = state.reduce(crate::chat::action::Action::StreamChunkReceived {
            draft_id: "draft-a".to_string(),
            delta: "A primary live".to_string(),
            version: 1,
        });
        let _ = state.reduce(crate::chat::action::Action::StreamChunkReceived {
            draft_id: "draft-b".to_string(),
            delta: "B focused live".to_string(),
            version: 1,
        });
        let snap = Arc::new(state.build_ui_snapshot(1));
        assert_eq!(
            snap.streaming.as_ref().map(|draft| draft.draft_id.as_str()),
            Some("draft-a")
        );
        assert_eq!(
            snap.streaming_draft_for_worker(20)
                .map(|draft| draft.accumulated.as_str()),
            Some("B focused live")
        );
        let (_tx, rx) = watch::channel(snap);
        let src = RenderSource::Snapshot(rx);

        sync_key_mirror_observation_state(&src, &mirror);

        let guard = mirror.lock();
        assert_eq!(
            guard
                .streaming_draft_for_worker(20)
                .map(|draft| draft.accumulated.as_str()),
            Some("B focused live")
        );
        let view = guard.active_session_view.as_ref().expect("focused worker view");
        let text = view.lines.join("\n");
        assert!(text.contains("assistant streaming: B focused live"), "{text}");
        assert!(!text.contains("A primary live"), "{text}");
        drop(guard);

        let (dispatcher, mut action_rx) = crate::chat::dispatcher::ChatDispatcher::new();
        open_provider_worker_view(&mirror, &dispatcher, None, 20);
        match action_rx.try_recv().expect("switcher close action") {
            crate::chat::action::Action::SwitcherClosed => {}
            other => panic!("expected SwitcherClosed, got {other:?}"),
        }
        match action_rx.try_recv().expect("focus action") {
            crate::chat::action::Action::SessionFocusChanged { focus } => {
                assert_eq!(focus, crate::chat::sessions::FocusTarget::Worker { sequence: 20 });
            }
            other => panic!("expected SessionFocusChanged, got {other:?}"),
        }
        match action_rx.try_recv().expect("active worker view action") {
            crate::chat::action::Action::ActiveSessionViewUpdated { view } => {
                let text = view.expect("worker view").lines.join("\n");
                assert!(text.contains("assistant streaming: B focused live"), "{text}");
                assert!(!text.contains("A primary live"), "{text}");
            }
            other => panic!("expected ActiveSessionViewUpdated, got {other:?}"),
        }
    }

    #[test]
    fn plain_character_keys_are_not_logged_as_tui_input_events() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        assert!(!super::should_log_tui_key_event(&KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::NONE,
        )));
        assert!(super::should_log_tui_key_event(&KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        assert!(super::should_log_tui_key_event(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
    }

    #[test]
    fn plain_character_key_detection_is_limited_to_unmodified_chars() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        assert!(super::is_plain_character_key(&KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::NONE,
        )));
        assert!(!super::is_plain_character_key(&KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        assert!(!super::is_plain_character_key(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
    }

    #[test]
    fn plain_character_key_char_excludes_control_and_release_events() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

        assert_eq!(
            super::plain_character_from_key(&KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            Some('p')
        );
        assert_eq!(
            super::plain_character_from_key(&KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            None
        );

        let release = KeyEvent {
            code: KeyCode::Char('p'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert_eq!(super::plain_character_from_key(&release), None);
    }

    /// 验证 snapshot 路径在 watch 推送新值后 with_view 看到新内容.
    #[tokio::test]
    async fn s4_a_4_unified_loop_redraw_on_snapshot_change() {
        let mut state = build_state_with_lines();
        let snap0 = Arc::new(state.build_ui_snapshot(1));
        let (tx, rx) = watch::channel(Arc::clone(&snap0));
        let src = RenderSource::Snapshot(rx);

        // 初始：2 行.
        src.with_view(|view| assert_eq!(view.conversation_lines().len(), 2));

        // 推送新 snapshot：3 行（直接 push 不走 reduce，需手动清缓存让 build_ui_snapshot 重建 Arc）
        let mut state2 = state;
        state2.ui.conversation_lines.push(ConversationLine::System {
            content: "c".to_string(),
        });
        // 直接绕过 reduce 写 lines 后必须清缓存（S4-A Commit B 引入的 Arc 共享缓存）
        let _ = state2.reduce_tracked(crate::chat::action::Action::RedrawRequested);
        // RedrawRequested 标 dirty=true 会清 cached_lines_arc，下次 build 重建 Arc 反映新 push
        let snap1 = Arc::new(state2.build_ui_snapshot(2));
        tx.send(snap1).expect("send snap1");

        // 新视图应看到 3 行.
        src.with_view(|view| {
            assert_eq!(view.conversation_lines().len(), 3, "watch 推送后应看到新行");
        });
    }
}

#[cfg(test)]
mod wave8_routing_failure_trace_tests {
    use crate::llm::route_decision::{ExecutionStatus, ProviderExecutionOutcome, RouteDecision};

    /// Contract relied upon by the chat failure path (FIX-P1-15 / #27) and the
    /// channels failure path (#21): a failed turn must produce a
    /// `ProviderExecutionOutcome` that (a) carries the same `decision_id` as the
    /// originating `RouteDecision` so the timeline join still works, (b) reports
    /// an `AllFailed` execution status, and (c) records exactly one failed
    /// attempt. Both failure paths build this outcome via `failed_for_decision`
    /// before recording `provider.final_outcome` events.
    #[test]
    fn failed_for_decision_outcome_is_recordable_for_failed_turns() {
        let decision = RouteDecision::single_candidate("test-provider", "test-model");
        let started_at = chrono::Utc::now();
        let error = anyhow::anyhow!("simulated provider timeout");

        let outcome = ProviderExecutionOutcome::failed_for_decision(&decision, started_at, &error);

        assert_eq!(
            outcome.decision_id, decision.decision_id,
            "failed outcome must preserve the route decision_id for timeline joins"
        );
        assert!(
            matches!(outcome.status, ExecutionStatus::AllFailed { .. }),
            "a failed turn must surface as ExecutionStatus::AllFailed"
        );
        assert_eq!(
            outcome.attempts.len(),
            1,
            "failed_for_decision should record exactly one failed attempt"
        );
        assert_eq!(outcome.final_provider, "test-provider");
        assert_eq!(outcome.final_model, "test-model");
    }
}

#[cfg(test)]
#[cfg(feature = "terminal-tui")]
mod p2_iss005_tests {
    use super::*;
    use crate::chat::sessions::id::SessionId;
    use crate::chat::sessions::model::{ManagedKind, ManagedSessionView, ManagedStatus, SessionOrigin};

    fn session_view(seq: u64) -> ManagedSessionView {
        ManagedSessionView {
            id: SessionId::from_run_id(&format!("run-{seq}")),
            seq,
            kind: ManagedKind::Agent,
            origin: SessionOrigin::User,
            title: "inspect build output".to_string(),
            status: ManagedStatus::Running,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
        }
    }

    #[test]
    fn attach_projection_keeps_replay_content_out_of_main_history() {
        let tail_line = "[assistant] historical answer that must stay in viewport".to_string();
        let ring_line = "live delta that must stay in viewport".to_string();
        let meta = session_view(7);

        let projection = build_active_session_attach_projection(
            7,
            Some(&meta),
            vec![tail_line.clone()],
            vec![ring_line.clone()],
            true,
        );

        assert_eq!(projection.view.seq, 7);
        assert_eq!(projection.view.kind, "agent");
        assert_eq!(projection.view.title, "inspect build output");
        assert!(projection.view.lines.contains(&tail_line));
        assert!(projection.view.lines.contains(&ring_line));
        assert!(projection.view.truncated);

        assert_eq!(
            projection.breadcrumb.lines().count(),
            1,
            "attach writes exactly one main-history breadcrumb"
        );
        assert!(projection.breadcrumb.contains("Attached session #7"));
        assert!(
            !projection.breadcrumb.contains("historical answer") && !projection.breadcrumb.contains("live delta"),
            "main-history breadcrumb must not replay child output: {}",
            projection.breadcrumb
        );
    }

    #[test]
    fn active_session_live_refresh_preserves_nonzero_scroll_offset() {
        let mut ring = crate::chat::sessions::SessionRing::with_capacity(16);
        for i in 0..12 {
            ring.push(format!("line {i}"));
        }
        let current = crate::chat::sessions::ActiveSessionView {
            seq: 3,
            kind: "shell".to_string(),
            title: "watch logs".to_string(),
            lines: vec!["old line".to_string()],
            truncated: false,
            scroll_offset: 2,
        };

        ring.push("new live line".to_string());
        let refreshed = active_session_view_from_ring(current, &ring);

        assert_eq!(
            refreshed.scroll_offset, 2,
            "reviewing older child output must not be yanked back to follow-tail"
        );
        assert!(refreshed.lines.iter().any(|line| line == "new live line"));
    }
}

#[cfg(test)]
mod p6c2_diff_tests {
    use super::*;

    #[test]
    fn diff_command_args_include_no_ext_diff_and_cached_flag() {
        let workspace = diff_command_args(false);
        assert_eq!(workspace, vec!["diff", "--no-ext-diff", "--no-color", "--unified=3"]);
        assert!(
            workspace.contains(&"--no-ext-diff"),
            "git diff viewer must disable diff.external"
        );

        let cached = diff_command_args(true);
        assert!(cached.contains(&"--no-ext-diff"));
        assert!(cached.contains(&"--cached"));
    }

    #[test]
    fn bounded_diff_lines_caps_bytes_lines_and_marks_truncated() {
        let raw = (0..12).map(|i| format!("+line {i}")).collect::<Vec<_>>().join("\n");
        let (lines, truncated) = bounded_diff_lines(&raw, raw.len(), 5);
        assert!(truncated);
        assert_eq!(lines.len(), 6, "5 retained lines plus truncation marker");
        assert_eq!(lines.last().expect("marker"), "[output truncated]");

        let (wide_lines, wide_truncated) = bounded_diff_lines("+你好世界", 5, 20);
        assert!(wide_truncated);
        let first = wide_lines.first().expect("line");
        assert!(
            first.len() <= 5,
            "byte cap must be enforced before line bounding, got {} bytes",
            first.len()
        );
        assert!(first.is_char_boundary(first.len()), "byte cap must not split utf-8");
    }

    #[test]
    fn git_diff_error_line_is_single_bounded_line() {
        let line = git_diff_error_line(b"fatal: not a git repository\nsecond line", b"");
        assert_eq!(line, "diff unavailable: fatal: not a git repository");
    }

    #[tokio::test]
    async fn collect_workspace_diff_git_failure_is_soft() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let source = collect_workspace_diff(temp.path(), false).await;
        assert_eq!(source.title, "workspace diff");
        assert_eq!(source.lines.len(), 1);
        assert!(
            source
                .lines
                .first()
                .is_some_and(|line| line.starts_with("diff unavailable:"))
        );
        assert!(!source.truncated);
    }
}

#[cfg(all(test, feature = "terminal-tui"))]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod iss_019_diff_apply_tests {
    use super::*;
    use crate::security::SecurityPolicy;
    use crate::security::policy::AutonomyLevel;
    use std::sync::Arc;

    fn plan() -> diff_apply::DiffApplyPlan {
        diff_apply::parse_unified_diff("--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n").expect("valid plan")
    }

    fn policy(workspace: &std::path::Path) -> SecurityPolicy {
        let autonomy = crate::config::AutonomyConfig {
            level: AutonomyLevel::Full,
            ..crate::config::AutonomyConfig::default()
        };
        SecurityPolicy::from_config(&autonomy, workspace)
    }

    #[test]
    fn plain_apply_ignores_openprx_approval_override_and_leaves_file_unchanged() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let exe = std::env::current_exe().expect("current test exe");
        let status = std::process::Command::new(exe)
            .arg("--exact")
            .arg("chat::iss_019_diff_apply_tests::plain_apply_no_bypass_child")
            .arg("--nocapture")
            .env("OPENPRX_APPROVAL_OVERRIDE", "allow")
            .env("ISS019_CHILD_WORKSPACE", temp.path())
            .status()
            .expect("run child no-bypass test");
        assert!(status.success(), "child no-bypass test failed: {status}");
    }

    #[test]
    fn plain_apply_no_bypass_child() {
        let Some(workspace) = std::env::var_os("ISS019_CHILD_WORKSPACE") else {
            return;
        };
        assert_eq!(
            std::env::var("OPENPRX_APPROVAL_OVERRIDE").as_deref(),
            Ok("allow"),
            "child test must exercise the env-override condition"
        );
        let workspace = std::path::PathBuf::from(workspace);
        std::fs::write(workspace.join("a.txt"), "old\n").expect("seed");
        let (dispatcher, _rx) = dispatcher::ChatDispatcher::new();
        let router = Arc::new(dispatcher::ApprovalRouter::new());

        let result = request_diff_apply_approval(plan(), false, Some(&router), &dispatcher);

        assert!(
            result.is_err(),
            "plain mode must fail closed before approval registration"
        );
        assert_eq!(std::fs::read_to_string(workspace.join("a.txt")).unwrap(), "old\n");
    }

    #[test]
    fn missing_router_fails_closed_before_pending_apply() {
        let (dispatcher, _rx) = dispatcher::ChatDispatcher::new();
        let result = request_diff_apply_approval(plan(), true, None, &dispatcher);
        assert!(result.is_err());
    }

    #[test]
    fn channel_drop_resolves_false_and_returns_no_pending_apply() {
        let (dispatcher, rx) = dispatcher::ChatDispatcher::new();
        drop(rx);
        let router = Arc::new(dispatcher::ApprovalRouter::new());
        let result = request_diff_apply_approval(plan(), true, Some(&router), &dispatcher);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn tui_approval_true_is_required_before_apply_writes() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(temp.path().join("a.txt"), "old\n").expect("seed");
        let (dispatcher, _rx) = dispatcher::ChatDispatcher::new();
        let router = Arc::new(dispatcher::ApprovalRouter::new());

        let pending =
            request_diff_apply_approval(plan(), true, Some(&router), &dispatcher).expect("approval requested");
        assert_eq!(std::fs::read_to_string(temp.path().join("a.txt")).unwrap(), "old\n");
        assert!(router.resolve(&pending.tool_id, true));
        assert!(pending.approval_rx.await.expect("approval rx"));

        let message = diff_apply::execute_plan(&pending.plan, &policy(temp.path()))
            .await
            .expect("apply");
        assert_eq!(message, "Applied fenced diff to 1 file.");
        assert_eq!(std::fs::read_to_string(temp.path().join("a.txt")).unwrap(), "new\n");
    }

    #[tokio::test]
    async fn tui_approval_false_leaves_workspace_unchanged() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(temp.path().join("a.txt"), "old\n").expect("seed");
        let (dispatcher, _rx) = dispatcher::ChatDispatcher::new();
        let router = Arc::new(dispatcher::ApprovalRouter::new());

        let pending =
            request_diff_apply_approval(plan(), true, Some(&router), &dispatcher).expect("approval requested");
        assert!(router.resolve(&pending.tool_id, false));
        assert!(!pending.approval_rx.await.expect("approval rx"));

        assert_eq!(std::fs::read_to_string(temp.path().join("a.txt")).unwrap(), "old\n");
    }
}

#[cfg(test)]
#[cfg(feature = "terminal-tui")]
mod p6b2_external_editor_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FakeTerminalMode {
        suspended: Arc<AtomicUsize>,
        restored: Arc<AtomicUsize>,
    }

    impl ExternalEditorTerminalMode for FakeTerminalMode {
        fn suspend_for_editor(&self) {
            self.suspended.fetch_add(1, Ordering::SeqCst);
        }

        fn restore_after_editor(&self) {
            self.restored.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn shell_editor_script(body: &str) -> (tempfile::NamedTempFile, String) {
        let mut script = tempfile::NamedTempFile::new().expect("test: script temp file");
        script.write_all(body.as_bytes()).expect("test: write editor script");
        script.flush().expect("test: flush editor script");
        let command = format!("sh {}", script.path().display());
        (script, command)
    }

    #[test]
    fn external_editor_success_rewrites_draft_and_restores_terminal() {
        let (_script, editor) = shell_editor_script("printf 'edited draft' > \"$1\"\n");
        let terminal = FakeTerminalMode::default();
        let result = edit_text_with_external_editor("old draft", Some(editor), &terminal);
        assert_eq!(result, ExternalEditorResult::Edited("edited draft".to_string()));
        assert_eq!(terminal.suspended.load(Ordering::SeqCst), 1);
        assert_eq!(terminal.restored.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn external_editor_nonzero_exit_keeps_draft_unchanged_and_restores_terminal() {
        let (_script, editor) = shell_editor_script("exit 7\n");
        let terminal = FakeTerminalMode::default();
        let result = edit_text_with_external_editor("old draft", Some(editor), &terminal);
        match result {
            ExternalEditorResult::Unchanged(reason) => {
                assert!(
                    reason.contains("exited with status"),
                    "reason should mention nonzero exit: {reason}"
                );
            }
            other => panic!("expected unchanged result, got {other:?}"),
        }
        assert_eq!(terminal.suspended.load(Ordering::SeqCst), 1);
        assert_eq!(terminal.restored.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn external_editor_spawn_failure_keeps_draft_unchanged_and_restores_terminal() {
        let terminal = FakeTerminalMode::default();
        let result = edit_text_with_external_editor_with_runner(
            "old draft",
            Some("unused-editor".to_string()),
            &terminal,
            |_editor, _path| Err(std::io::Error::new(std::io::ErrorKind::NotFound, "spawn failed")),
        );
        match result {
            ExternalEditorResult::Unchanged(reason) => {
                assert!(
                    reason.contains("failed to start"),
                    "reason should mention spawn failure: {reason}"
                );
            }
            other => panic!("expected unchanged result, got {other:?}"),
        }
        assert_eq!(terminal.suspended.load(Ordering::SeqCst), 1);
        assert_eq!(
            terminal.restored.load(Ordering::SeqCst),
            1,
            "spawn failure must still restore terminal mode"
        );
    }

    #[test]
    fn external_editor_missing_env_keeps_draft_unchanged_without_terminal_handoff() {
        let terminal = FakeTerminalMode::default();
        let result = edit_text_with_external_editor("old draft", None, &terminal);
        match result {
            ExternalEditorResult::Unchanged(reason) => {
                assert!(reason.contains("VISUAL") && reason.contains("EDITOR"));
            }
            other => panic!("expected unchanged result, got {other:?}"),
        }
        assert_eq!(terminal.suspended.load(Ordering::SeqCst), 0);
        assert_eq!(terminal.restored.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn external_editor_fullscreen_suspend_leaves_alt_and_restore_reenters() {
        CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.store(false, Ordering::SeqCst);
        let previous_mouse_capture = CHAT_MOUSE_CAPTURE_ACTIVE.swap(true, Ordering::SeqCst);
        let mut suspend = Vec::new();
        write_external_editor_suspend_sequences(&mut suspend);
        let suspend = String::from_utf8(suspend).expect("test: suspend escape bytes are utf-8");
        assert!(
            suspend.contains("\x1b[?1049l") && suspend.contains("\x1b[?47l"),
            "fullscreen editor suspend must leave chat alt-screen: {suspend:?}"
        );

        let mut restore = Vec::new();
        write_external_editor_restore_sequences(&mut restore);
        let restore = String::from_utf8(restore).expect("test: restore escape bytes are utf-8");
        CHAT_MOUSE_CAPTURE_ACTIVE.store(previous_mouse_capture, Ordering::SeqCst);
        assert!(
            restore.contains("\x1b[?1049l") && restore.contains("\x1b[?47l"),
            "fullscreen editor restore must reset any child/editor alt-screen first: {restore:?}"
        );
        let alt_enter = restore
            .find("\x1b[?1049h")
            .expect("test: fullscreen editor restore must re-enter chat alt-screen");
        let mouse_enable = restore
            .find("\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h")
            .expect("test: fullscreen editor restore must re-enable chat mouse capture");
        assert!(
            alt_enter < mouse_enable,
            "fullscreen editor restore must re-enable chat mouse capture after chat alt-screen: {restore:?}"
        );
    }

    #[test]
    fn external_editor_handoff_pops_and_repushes_keyboard_flags_when_active() {
        let previous = CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.swap(true, Ordering::SeqCst);

        let mut suspend = Vec::new();
        write_external_editor_suspend_sequences(&mut suspend);
        let suspend = String::from_utf8(suspend).expect("test: suspend escape bytes are utf-8");
        assert!(
            suspend.starts_with("\x1b[<1u"),
            "active keyboard enhancement must be popped before editor handoff: {suspend:?}"
        );

        let mut restore = Vec::new();
        write_external_editor_restore_sequences(&mut restore);
        let restore = String::from_utf8(restore).expect("test: restore escape bytes are utf-8");
        assert!(
            restore.ends_with("\x1b[>15u"),
            "active keyboard enhancement must be re-pushed after editor handoff: {restore:?}"
        );

        CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.store(previous, Ordering::SeqCst);
    }

    #[test]
    fn directional_switch_debounce_suppresses_rapid_attach_dispatches() {
        let key =
            crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Right, crossterm::event::KeyModifiers::NONE);
        let start = Instant::now();
        let mut last = None;

        assert_eq!(
            debounce_directional_switch_dispatch(key, tui::KeyDispatch::AttachSession { seq: 1 }, &mut last, start,),
            tui::KeyDispatch::AttachSession { seq: 1 }
        );
        assert_eq!(
            debounce_directional_switch_dispatch(
                key,
                tui::KeyDispatch::SwitchSession { seq: 2 },
                &mut last,
                start + Duration::from_millis(20),
            ),
            tui::KeyDispatch::Consumed,
            "rapid Left/Right repeats must not enqueue another synthetic attach"
        );
        assert_eq!(
            debounce_directional_switch_dispatch(
                key,
                tui::KeyDispatch::SwitchSession { seq: 2 },
                &mut last,
                start + Duration::from_millis(120),
            ),
            tui::KeyDispatch::SwitchSession { seq: 2 }
        );
    }
}

#[cfg(test)]
#[cfg(feature = "terminal-tui")]
#[allow(clippy::indexing_slicing)]
mod p3_directional_switch_tests {
    use super::*;
    use crate::chat::sessions::id::SessionId;
    use crate::chat::sessions::model::{ManagedKind, ManagedSessionView, ManagedStatus, SessionOrigin};

    fn session_view(seq: u64) -> ManagedSessionView {
        ManagedSessionView {
            id: SessionId::from_run_id(&format!("p3-run-{seq}")),
            seq,
            kind: ManagedKind::Agent,
            origin: SessionOrigin::User,
            title: format!("session {seq}"),
            status: ManagedStatus::Running,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
        }
    }

    #[test]
    fn directional_switch_focus_and_synthetic_attach_seq_match() {
        let seq = 42;
        let focus =
            crate::chat::sessions::focus::optimistic_focus(crate::chat::sessions::focus::RoutingIntent::Attach { seq });

        assert_eq!(focus, crate::chat::sessions::FocusTarget::Session { seq });
        assert_eq!(attach_command_for_seq(seq), "/attach 42");
    }

    #[test]
    fn synthetic_ui_command_is_identified_for_hidden_echo_path() {
        let mut msg = crate::channels::traits::ChannelMessage {
            id: "synthetic".to_string(),
            sender: SYNTHETIC_UI_COMMAND_SENDER.to_string(),
            reply_target: "user".to_string(),
            content: "/attach 42".to_string(),
            channel: "terminal".to_string(),
            timestamp: 0,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        };

        assert!(is_synthetic_ui_command(&msg));

        msg.sender = "user".to_string();
        assert!(!is_synthetic_ui_command(&msg));
    }

    fn input_msg(content: &str) -> crate::channels::traits::ChannelMessage {
        crate::channels::traits::ChannelMessage {
            id: uuid::Uuid::new_v4().to_string(),
            sender: "user".to_string(),
            reply_target: "user".to_string(),
            content: content.to_string(),
            channel: "terminal".to_string(),
            timestamp: 0,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        }
    }

    #[test]
    fn priority_input_prefix_is_stripped_for_execution() {
        let (priority, content) = input_priority_from_text("  /now   inspect queue  ");
        assert_eq!(priority, InputQueuePriority::Priority);
        assert_eq!(content, "inspect queue");

        let (priority, content) = input_priority_from_text("!! urgent");
        assert_eq!(priority, InputQueuePriority::Priority);
        assert_eq!(content, "urgent");
    }

    #[test]
    fn active_turn_enqueue_reports_priority_and_strips_prefix() {
        let mut backlog = std::collections::VecDeque::new();

        let priority = enqueue_input_message_and_return_priority(&mut backlog, input_msg("/now inspect status"));

        assert_eq!(priority, InputQueuePriority::Priority);
        let msg = pop_next_input_message(&mut backlog).expect("queued priority message");
        assert_eq!(msg.content, "inspect status");
    }

    #[test]
    fn input_backlog_status_counts_priority_messages() {
        let mut backlog = std::collections::VecDeque::new();
        enqueue_input_message(&mut backlog, input_msg("normal one"));
        enqueue_input_message(&mut backlog, input_msg("/priority urgent two"));
        enqueue_input_message(&mut backlog, input_msg("normal three"));

        let status = input_backlog_status(&backlog);

        assert_eq!(status.queued, 3);
        assert_eq!(status.priority, 1);
    }

    #[test]
    fn scheduler_mirror_projects_same_queue_status_as_backlog() {
        let mut backlog = std::collections::VecDeque::new();
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();

        enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("normal one"), 5);
        enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("/priority urgent two"), 5);

        assert_eq!(input_backlog_status(&backlog), scheduler.status().main_queue_status());
        assert_eq!(scheduler.status().main_queue_status().queued, 2);
        assert_eq!(scheduler.status().main_queue_status().priority, 1);
    }

    #[test]
    fn scheduler_mirror_does_not_change_priority_pop_order() {
        let mut backlog = std::collections::VecDeque::new();
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();

        enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("normal one"), 7);
        enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("/priority urgent two"), 7);
        enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("normal three"), 7);

        let first = pop_next_input_message_with_scheduler(&mut backlog, &mut scheduler).expect("priority first");
        let second = pop_next_input_message_with_scheduler(&mut backlog, &mut scheduler).expect("normal one second");
        let third = pop_next_input_message_with_scheduler(&mut backlog, &mut scheduler).expect("normal three third");

        assert_eq!(first.content, "urgent two");
        assert_eq!(second.content, "normal one");
        assert_eq!(third.content, "normal three");
        assert_eq!(
            scheduler.status().main_queue_status(),
            crate::chat::action::MainQueueStatus::default()
        );
    }

    #[test]
    fn dequeued_input_task_id_is_reused_for_provider_start() {
        let mut backlog = std::collections::VecDeque::new();
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();

        enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("normal provider turn"), 7);
        let expected_id = backlog
            .front()
            .and_then(|queued| queued.turn_task_id)
            .expect("queued task id");

        let dequeued = pop_next_input_task_with_scheduler(&mut backlog, &mut scheduler).expect("dequeued input");

        assert_eq!(dequeued.turn_task_id, Some(expected_id));
        assert_eq!(
            scheduler.task(expected_id).unwrap().state,
            crate::chat::turn_scheduler::TurnTaskState::Dispatched
        );
        assert_eq!(
            scheduler.status().main_queue_status(),
            crate::chat::action::MainQueueStatus::default()
        );

        let provider_id = start_provider_turn_task(&mut scheduler, dequeued.turn_task_id, &dequeued.msg.content, 7)
            .expect("provider task id");

        assert_eq!(provider_id, expected_id);
        assert_eq!(
            scheduler.task(provider_id).unwrap().state,
            crate::chat::turn_scheduler::TurnTaskState::Running
        );
    }

    #[test]
    fn queue_report_summarizes_backlog_with_preview() {
        let mut backlog = std::collections::VecDeque::new();
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("normal one"), 3);
        enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("/priority urgent two"), 3);
        let running = scheduler.enqueue("running turn", crate::chat::turn_scheduler::TurnPriority::Normal, 3);
        scheduler.start_task(running).expect("running turn starts");

        let report = format_input_backlog_report(&backlog, &scheduler, 8);

        assert!(
            report.contains("Main queue: 2 queued (1 priority), 1 running."),
            "{report}"
        );
        assert!(report.contains("1. [normal] normal one"), "{report}");
        assert!(report.contains("2. [priority] urgent two"), "{report}");
    }

    #[tokio::test]
    async fn active_turn_queue_command_is_handled_without_enqueue() {
        let mut backlog = std::collections::VecDeque::new();
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("normal one"), 3);
        let running = scheduler.enqueue("active turn", crate::chat::turn_scheduler::TurnPriority::Normal, 3);
        scheduler.start_task(running).expect("active turn starts");
        let msg = input_msg("/now /queue");
        let chat_session = session::ChatSession::new("p", "m");
        let provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        let mut chat_sessions =
            crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())));
        let session_rings = std::collections::HashMap::new();
        let mut reaped_log_archive = ReapedSessionLogArchive::default();
        let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();

        let output = active_turn_local_command_output(
            &msg,
            &backlog,
            &scheduler,
            &chat_session,
            &provider_turn_workers,
            &mut chat_sessions,
            &session_rings,
            &mut reaped_log_archive,
            &reap_policy,
            &tools_registry,
        )
        .await
        .expect("queue command output");

        assert!(
            output.contains("Main queue: 1 queued (0 priority), 1 running."),
            "{output}"
        );
        assert_eq!(backlog.len(), 1, "read-only queue command must not mutate backlog");
    }

    #[tokio::test]
    async fn active_turn_cost_command_is_handled_without_enqueue() {
        let mut backlog = std::collections::VecDeque::new();
        enqueue_input_message(&mut backlog, input_msg("normal one"));
        let scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let msg = input_msg("/now /cost");
        let chat_session = session::ChatSession::new("p", "m");
        let provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        let mut chat_sessions =
            crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())));
        let session_rings = std::collections::HashMap::new();
        let mut reaped_log_archive = ReapedSessionLogArchive::default();
        let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();

        let output = active_turn_local_command_output(
            &msg,
            &backlog,
            &scheduler,
            &chat_session,
            &provider_turn_workers,
            &mut chat_sessions,
            &session_rings,
            &mut reaped_log_archive,
            &reap_policy,
            &tools_registry,
        )
        .await
        .expect("cost command output");

        assert!(output.contains("Session cost:"), "{output}");
        assert!(output.contains("Turns:"), "{output}");
        assert_eq!(backlog.len(), 1, "read-only cost command must not mutate backlog");
    }

    #[tokio::test]
    async fn active_turn_workers_command_is_handled_without_enqueue() {
        let mut backlog = std::collections::VecDeque::new();
        enqueue_input_message(&mut backlog, input_msg("normal one"));
        let msg = input_msg("/now /workers");
        let chat_session = session::ChatSession::new("p", "m");
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("long worker", crate::chat::turn_scheduler::TurnPriority::Normal, 3);
        scheduler.start_task(task_id).expect("task starts");
        let mut provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        provider_turn_workers
            .start_from_task(
                scheduler.task(task_id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
            )
            .expect("worker starts");
        let mut chat_sessions =
            crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())));
        let session_rings = std::collections::HashMap::new();
        let mut reaped_log_archive = ReapedSessionLogArchive::default();
        let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();

        let output = active_turn_local_command_output(
            &msg,
            &backlog,
            &scheduler,
            &chat_session,
            &provider_turn_workers,
            &mut chat_sessions,
            &session_rings,
            &mut reaped_log_archive,
            &reap_policy,
            &tools_registry,
        )
        .await
        .expect("workers command output");

        assert!(output.contains("Main provider workers: 1 running"), "{output}");
        assert!(output.contains("w#1 task="), "{output}");
        assert!(output.contains("kind=foreground_awaited"), "{output}");
        assert!(output.contains("state=running"), "{output}");
        assert_eq!(backlog.len(), 1, "read-only workers command must not mutate backlog");
    }

    #[test]
    fn workers_cancel_command_parser_accepts_worker_sequence_forms() {
        assert_eq!(
            parse_workers_cancel_command("/workers cancel w#7"),
            ProviderWorkerCancelCommand::Cancel { sequence: 7 }
        );
        assert_eq!(
            parse_workers_cancel_command("/workers stop #8"),
            ProviderWorkerCancelCommand::Cancel { sequence: 8 }
        );
        assert_eq!(
            parse_workers_cancel_command("/workers cancel 9"),
            ProviderWorkerCancelCommand::Cancel { sequence: 9 }
        );
        assert!(matches!(
            parse_workers_cancel_command("/workers cancel nope"),
            ProviderWorkerCancelCommand::Invalid(_)
        ));
        assert!(matches!(
            parse_workers_cancel_command("/workers status"),
            ProviderWorkerCancelCommand::NotCancel
        ));
    }

    #[tokio::test]
    async fn active_turn_workers_cancel_command_marks_current_worker_cancelling_without_enqueue() {
        let msg = input_msg("/now /workers cancel w#1");
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("long worker", crate::chat::turn_scheduler::TurnPriority::Normal, 3);
        scheduler.start_task(task_id).expect("task starts");
        let mut provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        provider_turn_workers
            .start_from_task(
                scheduler.task(task_id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
            )
            .expect("worker starts");

        let (output, signal_cancel) =
            active_turn_workers_cancel_output(&msg, &mut scheduler, &mut provider_turn_workers, Some(task_id))
                .expect("workers cancel output");

        assert_eq!(
            signal_cancel,
            Some(ProviderWorkerCancelSignal::CancelRequested),
            "current foreground worker should signal the active turn"
        );
        assert!(
            output.contains("Requested cancellation for provider worker w#1"),
            "{output}"
        );
        assert!(output.contains("state=cancelling"), "{output}");
        assert_eq!(
            scheduler.task(task_id).unwrap().state,
            crate::chat::turn_scheduler::TurnTaskState::Cancelling
        );
        assert_eq!(
            provider_turn_workers.worker(task_id).unwrap().state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Cancelling
        );
    }

    #[tokio::test]
    async fn workers_cancel_command_targets_detached_worker_without_cancelling_peer() {
        let msg = input_msg("/workers cancel w#2");
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let first = scheduler.enqueue("first detached", crate::chat::turn_scheduler::TurnPriority::Normal, 3);
        let second = scheduler.enqueue("second detached", crate::chat::turn_scheduler::TurnPriority::Normal, 3);
        scheduler.start_task(first).expect("first task starts");
        scheduler.start_task(second).expect("second task starts");
        let mut provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        provider_turn_workers
            .start_from_task(
                scheduler.task(first).expect("first task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("first worker starts");
        provider_turn_workers
            .start_from_task(
                scheduler.task(second).expect("second task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("second worker starts");

        let (output, signal_cancel) =
            active_turn_workers_cancel_output(&msg, &mut scheduler, &mut provider_turn_workers, Some(first))
                .expect("workers cancel output");

        assert!(
            output.contains("Requested cancellation for provider worker w#2"),
            "{output}"
        );
        assert!(output.contains("kind=detached"), "{output}");
        assert_eq!(
            signal_cancel,
            Some(ProviderWorkerCancelSignal::CancelProviderTurn { task_id: second }),
            "detached worker cancellation must target the requested task id"
        );
        assert_eq!(
            scheduler.task(first).expect("first task").state,
            crate::chat::turn_scheduler::TurnTaskState::Running,
            "peer detached task must stay running"
        );
        assert_eq!(
            scheduler.task(second).expect("second task").state,
            crate::chat::turn_scheduler::TurnTaskState::Cancelling,
            "requested detached task should enter cancelling"
        );
        assert_eq!(
            provider_turn_workers.worker(first).expect("first worker").state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Running,
            "peer detached worker must stay running"
        );
        assert_eq!(
            provider_turn_workers.worker(second).expect("second worker").state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Cancelling,
            "requested detached worker should enter cancelling"
        );
    }

    #[test]
    fn workers_cancel_command_targets_detached_worker_from_outer_loop() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let first = scheduler.enqueue("first detached", crate::chat::turn_scheduler::TurnPriority::Normal, 3);
        let second = scheduler.enqueue("second detached", crate::chat::turn_scheduler::TurnPriority::Normal, 3);
        scheduler.start_task(first).expect("first task starts");
        scheduler.start_task(second).expect("second task starts");
        let mut provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        provider_turn_workers
            .start_from_task(
                scheduler.task(first).expect("first task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("first worker starts");
        provider_turn_workers
            .start_from_task(
                scheduler.task(second).expect("second task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("second worker starts");

        let (output, signal_cancel) = provider_workers_cancel_output_for_input(
            "/workers cancel w#2",
            &mut scheduler,
            &mut provider_turn_workers,
            None,
        )
        .expect("workers cancel output");

        assert!(
            output.contains("Requested cancellation for provider worker w#2"),
            "{output}"
        );
        assert_eq!(
            signal_cancel,
            Some(ProviderWorkerCancelSignal::CancelProviderTurn { task_id: second }),
            "outer-loop worker cancel must target the requested detached task"
        );
        assert_eq!(
            scheduler.task(first).expect("first task").state,
            crate::chat::turn_scheduler::TurnTaskState::Running,
            "peer detached task must stay running"
        );
        assert_eq!(
            scheduler.task(second).expect("second task").state,
            crate::chat::turn_scheduler::TurnTaskState::Cancelling,
            "requested detached task should enter cancelling"
        );
    }

    #[tokio::test]
    async fn p4a_active_turn_event_pump_preserves_cancel_local_and_enqueue_order() {
        let first_msg = input_msg("/workers cancel w#1");
        let (tx, mut input_rx) = mpsc::channel(4);
        tx.send(input_msg("/queue")).await.expect("send queue command");
        tx.send(input_msg("follow up turn")).await.expect("send follow-up turn");
        drop(tx);
        let mut outputs = Vec::<String>::new();
        let mut emit = |text: &str| outputs.push(text.to_string());
        let mut backlog = std::collections::VecDeque::new();
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue(
            "active provider turn",
            crate::chat::turn_scheduler::TurnPriority::Normal,
            3,
        );
        scheduler.start_task(task_id).expect("task starts");
        let mut provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        provider_turn_workers
            .start_from_task(
                scheduler.task(task_id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
            )
            .expect("worker starts");
        let (chat_dispatcher, _action_rx) = dispatcher::ChatDispatcher::new();
        let chat_session = session::ChatSession::new("p", "m");
        let mut chat_sessions =
            crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())));
        let session_rings = std::collections::HashMap::new();
        let mut reaped_log_archive = ReapedSessionLogArchive::default();
        let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();

        process_active_turn_input_batch(
            first_msg,
            &mut emit,
            &mut input_rx,
            &mut backlog,
            &mut scheduler,
            &mut provider_turn_workers,
            Some(task_id),
            &chat_dispatcher,
            &chat_session,
            &mut chat_sessions,
            &session_rings,
            &mut reaped_log_archive,
            &reap_policy,
            &tools_registry,
        )
        .await;

        assert_eq!(outputs.len(), 2, "cancel and /queue should emit local output");
        assert!(
            outputs[0].contains("Requested cancellation for provider worker w#1"),
            "{}",
            outputs[0]
        );
        assert!(
            outputs[1].contains("Main queue: 0 queued (0 priority), 0 running."),
            "{}",
            outputs[1]
        );
        assert_eq!(backlog.len(), 1, "ordinary input after local commands remains queued");
        assert_eq!(backlog.front().expect("queued follow-up").msg.content, "follow up turn");
        assert_eq!(
            scheduler.task(task_id).unwrap().state,
            crate::chat::turn_scheduler::TurnTaskState::Cancelling
        );
        assert_eq!(scheduler.status().main_queue_status().queued, 1);
    }

    #[tokio::test]
    async fn p4a_active_turn_quit_is_queued_for_delayed_exit() {
        let mut outputs = Vec::<String>::new();
        let mut emit = |text: &str| outputs.push(text.to_string());
        let mut backlog = std::collections::VecDeque::new();
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue(
            "active provider turn",
            crate::chat::turn_scheduler::TurnPriority::Normal,
            3,
        );
        scheduler.start_task(task_id).expect("task starts");
        let mut provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        provider_turn_workers
            .start_from_task(
                scheduler.task(task_id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
            )
            .expect("worker starts");
        let (chat_dispatcher, _action_rx) = dispatcher::ChatDispatcher::new();
        let chat_session = session::ChatSession::new("p", "m");
        let mut chat_sessions =
            crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())));
        let session_rings = std::collections::HashMap::new();
        let mut reaped_log_archive = ReapedSessionLogArchive::default();
        let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();

        process_active_turn_input_message(
            input_msg("/exit"),
            &mut emit,
            &mut backlog,
            &mut scheduler,
            &mut provider_turn_workers,
            Some(task_id),
            &chat_dispatcher,
            &chat_session,
            &mut chat_sessions,
            &session_rings,
            &mut reaped_log_archive,
            &reap_policy,
            &tools_registry,
        )
        .await;

        assert!(outputs.is_empty(), "/exit during an active turn is not a local command");
        assert_eq!(backlog.len(), 1, "/exit should wait behind the active turn");
        assert_eq!(backlog.front().expect("queued exit").msg.content, "/exit");
        assert_eq!(scheduler.status().main_queue_status().queued, 1);
        assert_eq!(
            scheduler.task(task_id).unwrap().state,
            crate::chat::turn_scheduler::TurnTaskState::Running,
            "delayed exit must not cancel or complete the active provider turn"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn p4a_input_close_keeps_event_pump_alive_until_pending_turn_finishes() {
        assert!(
            should_continue_event_pump_after_input_closed(1),
            "closed input should not terminate the event pump while a detached Redux turn is pending"
        );
        assert!(
            !should_continue_event_pump_after_input_closed(0),
            "with no pending Redux turn, closed input can end the outer loop"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn p4a_deferred_resume_waits_for_queued_visible_input_from_current_session() {
        let mut backlog = std::collections::VecDeque::new();
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        let mut deferred_resume = std::collections::VecDeque::from(["session-b".to_string()]);
        let session_a_history_len = 4;

        enqueue_input_message_with_scheduler(
            &mut backlog,
            &mut scheduler,
            input_msg("queued for session A"),
            session_a_history_len,
        );

        assert!(
            !should_drain_deferred_resume_after_visible_inputs(0, &backlog, &workers),
            "deferred resume must not run before queued visible input is dequeued"
        );
        assert_eq!(
            deferred_resume.front().map(String::as_str),
            Some("session-b"),
            "resume request remains deferred while session-A input is pending"
        );

        let next = pop_next_visible_input_task_with_scheduler(
            &mut backlog,
            &mut scheduler,
            &workers,
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            1,
        )
        .expect("queued input");
        assert_eq!(next.msg.content, "queued for session A");
        let task_id = next.turn_task_id.expect("scheduler task id");
        let task = scheduler.task(task_id).expect("queued task snapshot");
        assert_eq!(
            task.history_base_len, session_a_history_len,
            "queued input keeps the pre-resume session-A history boundary"
        );
        assert_eq!(
            task.state,
            crate::chat::turn_scheduler::TurnTaskState::Dispatched,
            "queued input is dispatched before deferred resume can drain"
        );

        assert!(should_drain_deferred_resume_after_visible_inputs(0, &backlog, &workers));
        assert_eq!(deferred_resume.pop_front().as_deref(), Some("session-b"));
    }

    #[test]
    fn workers_report_lists_only_active_worker_rows() {
        let status = crate::chat::action::ProviderWorkerStatus {
            running: 1,
            cancelling: 0,
            awaiting_commit: 0,
            finalized_payloads: 1,
            finalized_total_tokens: 1_250,
            oldest_started_at_ms: Some(chrono::Utc::now().timestamp_millis().saturating_sub(1_000)),
            rows: vec![
                crate::chat::action::ProviderWorkerStatusRow {
                    task_id: 1,
                    sequence: 1,
                    kind: crate::chat::action::ProviderWorkerRowKind::ForegroundAwaited,
                    state: crate::chat::action::ProviderWorkerRowState::Running,
                    started_at_ms: chrono::Utc::now().timestamp_millis().saturating_sub(1_000),
                    finalized_total_tokens: None,
                    completion_ready: false,
                    recent_tool_call: None,
                },
                crate::chat::action::ProviderWorkerStatusRow {
                    task_id: 2,
                    sequence: 2,
                    kind: crate::chat::action::ProviderWorkerRowKind::Detached,
                    state: crate::chat::action::ProviderWorkerRowState::Committed,
                    started_at_ms: chrono::Utc::now().timestamp_millis().saturating_sub(9_000),
                    finalized_total_tokens: Some(1_250),
                    completion_ready: true,
                    recent_tool_call: None,
                },
            ],
        };

        let report = format_provider_worker_report(&status);

        assert!(
            report.contains("1 finalized payloads, 1.2k finalized tokens"),
            "report should retain finalized aggregate: {report}"
        );
        assert!(report.contains("- w#1 task=1"), "active row should be listed: {report}");
        assert!(
            report.contains("completion=pending"),
            "active row should expose completion readiness: {report}"
        );
        assert!(
            !report.contains("- w#2 task=2"),
            "completed row should not stay in the active worker list: {report}"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn non_current_provider_completion_is_retained_and_marks_ready() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let first = scheduler.enqueue("first", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
        let second = scheduler.enqueue("second", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
        scheduler.start_task(first).expect("first starts");
        scheduler.start_task(second).expect("second starts");

        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(first).expect("first task"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("first worker starts");
        workers
            .start_from_task(
                scheduler.task(second).expect("second task"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("second worker starts");
        workers
            .record_execution_started(second, 42)
            .expect("second worker execution starts");

        let (_tx, mut lifecycle_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut lifecycle_open = true;
        let mut pending = std::collections::HashMap::new();
        let completion = ProviderTurnCompletionEvent {
            task_id: second,
            outcome: Some(dispatcher::TurnOutcomeKind::Completed {
                final_text: "second done".to_string(),
                reasoning: String::new(),
            }),
            usage: crate::llm::route_decision::TokenUsage {
                total_tokens: Some(42),
                ..Default::default()
            },
        };

        let route = route_provider_completion_event(
            &mut lifecycle_rx,
            &mut lifecycle_open,
            &mut workers,
            &mut pending,
            Some(first),
            completion,
        );

        assert!(matches!(route, ProviderTurnCompletionRoute::Pending));
        assert!(
            pending.contains_key(&second),
            "non-current completion should be retained"
        );
        let rows = workers.snapshot();
        let second_row = rows
            .iter()
            .find(|row| row.task_id == second)
            .expect("second worker snapshot");
        assert!(
            second_row.completion_ready,
            "retained completion should still mark the worker completion-ready"
        );
        assert!(
            !rows
                .iter()
                .find(|row| row.task_id == first)
                .expect("first worker snapshot")
                .completion_ready,
            "uncompleted worker should remain pending"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn current_provider_completion_is_routed_without_retaining_pending() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("current", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
        scheduler.start_task(task_id).expect("task starts");

        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(task_id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("worker starts");
        workers
            .record_execution_started(task_id, 43)
            .expect("worker execution starts");

        let (_tx, mut lifecycle_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut lifecycle_open = true;
        let mut pending = std::collections::HashMap::new();
        let completion = ProviderTurnCompletionEvent {
            task_id,
            outcome: Some(dispatcher::TurnOutcomeKind::Completed {
                final_text: "current done".to_string(),
                reasoning: String::new(),
            }),
            usage: crate::llm::route_decision::TokenUsage {
                total_tokens: Some(43),
                ..Default::default()
            },
        };

        let route = route_provider_completion_event(
            &mut lifecycle_rx,
            &mut lifecycle_open,
            &mut workers,
            &mut pending,
            Some(task_id),
            completion,
        );

        match route {
            ProviderTurnCompletionRoute::Current(completion) => {
                assert_eq!(completion.task_id, task_id);
                assert_eq!(completion.usage.total_tokens, Some(43));
            }
            ProviderTurnCompletionRoute::Pending => panic!("current completion must not be retained as pending"),
        }
        assert!(pending.is_empty(), "current completion should bypass pending map");
        assert!(
            workers
                .snapshot()
                .into_iter()
                .find(|row| row.task_id == task_id)
                .expect("worker snapshot")
                .completion_ready,
            "current completion should still mark the worker completion-ready"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn p4a_outer_completion_route_retains_event_for_pending_redux_finalizer() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("redux pending", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
        scheduler.start_task(task_id).expect("task starts");
        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(task_id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("worker starts");
        workers
            .record_execution_started(task_id, 60)
            .expect("worker execution starts");
        let (_tx, mut lifecycle_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut lifecycle_open = true;
        let mut pending = std::collections::HashMap::new();
        let (chat_dispatcher, _action_rx) = dispatcher::ChatDispatcher::new();
        let completion = ProviderTurnCompletionEvent {
            task_id,
            outcome: Some(dispatcher::TurnOutcomeKind::Completed {
                final_text: "done".to_string(),
                reasoning: String::new(),
            }),
            usage: crate::llm::route_decision::TokenUsage {
                total_tokens: Some(9),
                ..Default::default()
            },
        };

        let route = route_provider_completion_event_and_publish(
            &mut lifecycle_rx,
            &mut lifecycle_open,
            &mut workers,
            &mut pending,
            None,
            completion,
            &chat_dispatcher,
        );

        assert!(matches!(route, ProviderTurnCompletionRoute::Pending));
        let retained = pending.get(&task_id).expect("outer completion retained by task id");
        assert_eq!(retained.usage.total_tokens, Some(9));
        assert!(
            workers
                .snapshot()
                .into_iter()
                .find(|row| row.task_id == task_id)
                .expect("worker snapshot")
                .completion_ready,
            "outer route still marks worker completion-ready before finalizer"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn p4a_completion_event_and_finalizer_chain_commits_ready_turn() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue(
            "redux pending finalizer",
            crate::chat::turn_scheduler::TurnPriority::Normal,
            1,
        );
        scheduler.start_task(task_id).expect("task starts");
        let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
        coordinator
            .register_task(scheduler.task(task_id).expect("task snapshot"))
            .expect("history task registers");
        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(task_id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("worker starts");
        workers
            .record_execution_started(task_id, 64)
            .expect("worker execution starts");
        let (_tx, mut lifecycle_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut lifecycle_open = true;
        let mut pending = std::collections::HashMap::new();
        let (chat_dispatcher, _action_rx) = dispatcher::ChatDispatcher::new();

        let route = route_provider_completion_event_and_publish(
            &mut lifecycle_rx,
            &mut lifecycle_open,
            &mut workers,
            &mut pending,
            None,
            ProviderTurnCompletionEvent {
                task_id,
                outcome: Some(dispatcher::TurnOutcomeKind::Completed {
                    final_text: "ready finalizer".to_string(),
                    reasoning: String::new(),
                }),
                usage: crate::llm::route_decision::TokenUsage {
                    total_tokens: Some(64),
                    ..Default::default()
                },
            },
            &chat_dispatcher,
        );
        assert!(matches!(route, ProviderTurnCompletionRoute::Pending));

        let completion = pending
            .remove(&task_id)
            .expect("ready completion retained for finalizer");
        let tools: Vec<Box<dyn Tool>> = Vec::new();
        let plan = provider_turn_terminal_plan_from_completion(
            ResolvedProviderTurnCompletion {
                outcome: completion.outcome,
                usage: completion.usage,
            },
            1,
            &tools,
        );
        let mut queue = std::collections::VecDeque::new();
        enqueue_provider_turn_finalizer_event(&mut queue, Some(task_id), plan);

        let results = drain_provider_turn_finalizer_events_and_publish(
            &mut scheduler,
            &mut coordinator,
            &mut workers,
            &mut queue,
            &chat_dispatcher,
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].terminal_status, "completed");
        assert!(results[0].finalized);
        assert_eq!(
            scheduler.task(task_id).unwrap().state,
            crate::chat::turn_scheduler::TurnTaskState::Completed
        );
        let row = workers
            .snapshot()
            .into_iter()
            .find(|row| row.task_id == task_id)
            .expect("worker snapshot retained");
        assert_eq!(row.finalized_total_tokens, Some(64));
        assert!(matches!(
            row.state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Committed
        ));
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn p4b1_ready_signal_is_noop_until_ordered_commit_dispatches_save() {
        use crate::chat::action::Action;
        use crate::chat::state::{ChatState, Effect};
        use std::sync::Arc;

        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("ordered commit", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        scheduler.start_task(task_id).expect("task starts");
        let sequence = scheduler.task(task_id).expect("task snapshot").sequence;
        let mut state = ChatState::new(Arc::from("provider"), Arc::from("model"), CancellationToken::new());
        let _ = state.reduce(Action::StartLLMTurn {
            provider_turn_task_id: Some(task_id),
            provider_turn_sequence: Some(sequence),
            draft_id: "draft-p4b1".to_string(),
            history: Vec::new(),
            compaction_guard_history: None,
            compaction_config: None,
            cancel: CancellationToken::new(),
            turn_spawn_ctx: None,
            turn_message_send_ctx: None,
        });

        let ready_effects = state.reduce(Action::ProviderTurnReadyForCommit {
            draft_id: "draft-p4b1".to_string(),
            final_text: "answer".to_string(),
            reasoning: "reason".to_string(),
        });
        assert!(ready_effects.is_empty(), "ready signal must not emit SaveSession");
        assert!(
            state.stream.primary_streaming_draft().is_some(),
            "ready signal must leave reducer draft open until ordered commit"
        );

        let (dispatcher, mut rx) = dispatcher::ChatDispatcher::new();
        dispatch_ordered_provider_turn_commit(
            &dispatcher,
            task_id,
            "draft-p4b1",
            "question",
            "answer",
            "reason",
            false,
        );

        let user = rx.try_recv().expect("ordered RecordUserTurn action");
        match user {
            Action::RecordUserTurn(content) => {
                assert_eq!(content, "question");
                let effects = state.reduce(Action::RecordUserTurn(content));
                assert!(
                    !effects.iter().any(|effect| matches!(effect, Effect::SaveSession(_))),
                    "RecordUserTurn alone must not save before terminal commit"
                );
            }
            other => panic!("expected ordered RecordUserTurn, got {other:?}"),
        }

        let record = rx.try_recv().expect("ordered RecordAssistantTurn action");
        match record {
            Action::RecordAssistantTurn { task_id: seen, content } => {
                assert_eq!(seen, Some(task_id));
                assert_eq!(content, "answer");
                let effects = state.reduce(Action::RecordAssistantTurn { task_id: seen, content });
                assert!(
                    !effects.iter().any(|effect| matches!(effect, Effect::SaveSession(_))),
                    "RecordAssistantTurn alone must not save before terminal commit"
                );
            }
            other => panic!("expected ordered RecordAssistantTurn, got {other:?}"),
        }

        let terminal = rx.try_recv().expect("ordered StreamCompleted action");
        match terminal {
            Action::StreamCompleted {
                draft_id,
                final_text,
                reasoning,
            } => {
                assert_eq!(draft_id, "draft-p4b1");
                assert_eq!(final_text, "answer");
                assert_eq!(reasoning, "reason");
                let effects = state.reduce(Action::StreamCompleted {
                    draft_id,
                    final_text,
                    reasoning,
                });
                assert!(
                    effects.iter().any(|effect| matches!(effect, Effect::SaveSession(_))),
                    "SaveSession must be emitted only by ordered StreamCompleted"
                );
            }
            other => panic!("expected ordered StreamCompleted, got {other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "ordered commit helper should emit exactly record + terminal actions"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn p4b1_failed_and_cancelled_skip_do_not_emit_persistence_actions() {
        use crate::chat::action::Action;

        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let failed = scheduler.enqueue("failed", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        let cancelled = scheduler.enqueue("cancelled", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        scheduler.start_task(failed).expect("failed task starts");
        scheduler.start_task(cancelled).expect("cancelled task starts");
        scheduler.request_cancel(cancelled).expect("cancel request accepted");
        let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
        coordinator
            .register_task(scheduler.task(failed).expect("failed task snapshot"))
            .expect("failed task registers");
        coordinator
            .register_task(scheduler.task(cancelled).expect("cancelled task snapshot"))
            .expect("cancelled task registers");
        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        for task_id in [failed, cancelled] {
            workers
                .start_from_task(
                    scheduler.task(task_id).expect("task snapshot"),
                    crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
                )
                .expect("worker starts");
            workers
                .record_execution_started(task_id, 70)
                .expect("worker execution starts");
            workers
                .record_completion_ready(task_id)
                .expect("worker completion ready");
        }
        workers
            .request_cancel(cancelled)
            .expect("worker cancellation requested");
        let mut queue = std::collections::VecDeque::new();
        enqueue_provider_turn_finalizer_event(
            &mut queue,
            Some(failed),
            ProviderTurnTerminalPlan::Failed {
                err: "failed".to_string(),
                history_commit_len: 0,
                summary: "test failed skip".to_string(),
            },
        );
        enqueue_provider_turn_finalizer_event(
            &mut queue,
            Some(cancelled),
            ProviderTurnTerminalPlan::Cancelled {
                summary: "test cancelled skip",
            },
        );
        let (dispatcher, mut rx) = dispatcher::ChatDispatcher::new();

        let results = drain_provider_turn_finalizer_events_and_publish(
            &mut scheduler,
            &mut coordinator,
            &mut workers,
            &mut queue,
            &dispatcher,
        );

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].terminal_status, "failed");
        assert_eq!(results[1].terminal_status, "cancelled");
        assert!(results.iter().all(|result| result.finalized));
        while let Ok(action) = rx.try_recv() {
            assert!(
                !matches!(
                    action,
                    Action::RecordAssistantTurn { .. } | Action::StreamCompleted { .. }
                ),
                "skip decisions must not emit assistant/session persistence actions: {action:?}"
            );
        }
    }

    #[test]
    fn provider_turn_visible_admission_blocks_while_provider_worker_active() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let running = scheduler.enqueue("running", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        let committed = scheduler.enqueue("committed", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        scheduler.start_task(running).expect("running starts");
        scheduler.start_task(committed).expect("committed starts");

        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        assert!(
            provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 1,)
                .can_start_visible
        );

        workers
            .start_from_task(
                scheduler.task(running).expect("running task"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("running worker starts");
        let admission =
            provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 1);
        assert_eq!(admission.active_workers, 1);
        assert!(!admission.can_start_visible);

        workers
            .record_execution_started(running, 44)
            .expect("running execution starts");
        workers
            .record_completion_ready(running)
            .expect("running completion ready");
        workers
            .record_finalized_payload(
                running,
                crate::chat::turn_worker::ProviderTurnFinalizedPayload {
                    history_commit_len: 1,
                    final_text_chars: 1,
                    recorded_response_chars: 1,
                    total_tokens: 1,
                    prompt_tokens: 0,
                    completion_tokens: 1,
                },
            )
            .expect("running payload finalized");
        workers.record_completed(running).expect("running worker completed");
        let admission =
            provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 1);
        assert_eq!(admission.active_workers, 1);
        assert!(
            !admission.can_start_visible,
            "awaiting commit still owns the visible draft boundary"
        );

        workers
            .start_from_task(
                scheduler.task(committed).expect("committed task"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("committed worker starts");
        workers
            .record_execution_started(committed, 45)
            .expect("committed execution starts");
        workers
            .record_completion_ready(committed)
            .expect("committed completion ready");
        workers
            .record_finalized_payload(
                committed,
                crate::chat::turn_worker::ProviderTurnFinalizedPayload {
                    history_commit_len: 1,
                    final_text_chars: 1,
                    recorded_response_chars: 1,
                    total_tokens: 1,
                    prompt_tokens: 0,
                    completion_tokens: 1,
                },
            )
            .expect("committed payload finalized");
        workers.record_completed(committed).expect("committed worker completed");
        let decision = crate::chat::history_commit::HistoryCommitDecision::Commit {
            task_id: running,
            sequence: scheduler.task(running).expect("running task").sequence,
            history_commit_len: 1,
            summary: "running committed".to_string(),
        };
        workers
            .apply_commit_decision(&decision)
            .expect("running commit applies");
        let admission =
            provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 1);
        assert_eq!(admission.active_workers, 1);
        assert!(
            !admission.can_start_visible,
            "second awaiting commit worker still blocks"
        );

        let decision = crate::chat::history_commit::HistoryCommitDecision::Commit {
            task_id: committed,
            sequence: scheduler.task(committed).expect("committed task").sequence,
            history_commit_len: 1,
            summary: "committed committed".to_string(),
        };
        workers
            .apply_commit_decision(&decision)
            .expect("committed commit applies");
        let admission =
            provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 1);
        assert_eq!(admission.active_workers, 0);
        assert!(admission.can_start_visible);
    }

    #[test]
    fn provider_turn_visible_admission_allows_detached_until_configured_limit() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let first = scheduler.enqueue("first", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        let second = scheduler.enqueue("second", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        scheduler.start_task(first).expect("first starts");
        scheduler.start_task(second).expect("second starts");

        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(first).expect("first task"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("first worker starts");
        let admission =
            provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 2);
        assert_eq!(admission.detached_active, 1);
        assert!(admission.can_start_visible, "one detached turn leaves one N=2 slot");

        workers
            .start_from_task(
                scheduler.task(second).expect("second task"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("second worker starts");
        let admission =
            provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 2);
        assert_eq!(admission.detached_active, 2);
        assert!(
            !admission.can_start_visible,
            "two detached turns exhaust configured N=2 admission"
        );
    }

    #[test]
    fn provider_turn_visible_admission_legacy_foreground_is_exclusive() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let legacy = scheduler.enqueue("legacy", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        scheduler.start_task(legacy).expect("legacy starts");

        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(legacy).expect("legacy task"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
            )
            .expect("legacy worker starts");

        let detached =
            provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 2);
        assert_eq!(detached.foreground_active, 1);
        assert!(!detached.can_start_visible, "legacy active blocks detached turns");

        let foreground = provider_turn_visible_admission(
            &workers,
            crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
            2,
        );
        assert!(
            !foreground.can_start_visible,
            "legacy active blocks another legacy turn"
        );
    }

    #[test]
    fn visible_input_pop_preserves_queue_while_provider_worker_active() {
        let mut backlog = std::collections::VecDeque::new();
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let active = scheduler.enqueue("active", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        scheduler.start_task(active).expect("active task starts");

        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(active).expect("active task"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("active worker starts");
        enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("queued while active"), 7);
        let queued_task = backlog
            .front()
            .and_then(|queued| queued.turn_task_id)
            .expect("queued task id");

        let popped = pop_next_visible_input_task_with_scheduler(
            &mut backlog,
            &mut scheduler,
            &workers,
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            1,
        );

        assert!(
            popped.is_none(),
            "active provider worker must hold visible input admission"
        );
        assert_eq!(backlog.len(), 1, "queued input must remain queued");
        assert_eq!(
            scheduler.task(queued_task).expect("queued task").state,
            crate::chat::turn_scheduler::TurnTaskState::Queued,
            "blocked visible input must not be marked dispatched"
        );
        assert_eq!(scheduler.status().main_queue_status().queued, 1);
    }

    #[test]
    fn visible_input_pop_prefers_priority_when_detached_slot_is_available() {
        let mut backlog = std::collections::VecDeque::new();
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let active = scheduler.enqueue("active", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        scheduler.start_task(active).expect("active task starts");

        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(active).expect("active task"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("active worker starts");
        enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("normal queued first"), 7);
        enqueue_input_message_with_scheduler(
            &mut backlog,
            &mut scheduler,
            input_msg("/priority urgent queued second"),
            7,
        );

        let popped = pop_next_visible_input_task_with_scheduler(
            &mut backlog,
            &mut scheduler,
            &workers,
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            2,
        )
        .expect("second detached slot should be available");

        assert_eq!(
            popped.msg.content, "urgent queued second",
            "priority input must dispatch before older normal input when a visible slot opens"
        );
        let popped_task = popped.turn_task_id.expect("popped task id");
        assert_eq!(
            scheduler.task(popped_task).expect("priority task").priority,
            crate::chat::turn_scheduler::TurnPriority::Priority
        );
        assert_eq!(
            backlog.front().map(|queued| queued.msg.content.as_str()),
            Some("normal queued first"),
            "normal input remains queued after priority dispatch"
        );
    }

    #[test]
    fn post_route_requeue_defers_next_visible_pop_once_under_detached_capacity() {
        let mut backlog = std::collections::VecDeque::new();
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let active = scheduler.enqueue("active detached", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        scheduler.start_task(active).expect("active task starts");

        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(active).expect("active task"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("active worker starts");
        enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("legacy fallback"), 7);
        let queued_task = backlog
            .front()
            .and_then(|queued| queued.turn_task_id)
            .expect("queued task id");

        assert!(
            provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 2,)
                .can_start_visible,
            "ordinary detached pop admission would otherwise immediately re-pop"
        );
        let mut defer_visible_input_pop_once = true;

        assert!(
            consume_deferred_visible_input_pop(&mut defer_visible_input_pop_once),
            "post-route requeue should force one event-pump wait"
        );
        assert_eq!(backlog.len(), 1, "deferred pop leaves requeued input in place");
        assert_eq!(
            scheduler.task(queued_task).expect("queued task").state,
            crate::chat::turn_scheduler::TurnTaskState::Queued,
            "deferred pop must not repeatedly mark the requeued task dispatched"
        );
        assert!(
            !consume_deferred_visible_input_pop(&mut defer_visible_input_pop_once),
            "defer gate is one-shot"
        );

        let popped = pop_next_visible_input_task_with_scheduler(
            &mut backlog,
            &mut scheduler,
            &workers,
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            2,
        )
        .expect("input can pop after one event-pump wait");
        assert_eq!(popped.turn_task_id, Some(queued_task));
        assert_eq!(
            scheduler.task(queued_task).expect("queued task").state,
            crate::chat::turn_scheduler::TurnTaskState::Dispatched
        );
    }

    #[test]
    fn post_route_admission_rejection_requeues_input_and_defers_next_pop() {
        let mut backlog = std::collections::VecDeque::new();
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let active = scheduler.enqueue("active detached", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        scheduler.start_task(active).expect("active task starts");

        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(active).expect("active task"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("active worker starts");
        enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("legacy after draft failure"), 7);

        let popped = pop_next_visible_input_task_with_scheduler(
            &mut backlog,
            &mut scheduler,
            &workers,
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            2,
        )
        .expect("detached admission allows second visible turn before route is known");
        let task_id = popped.turn_task_id.expect("popped task id");
        assert_eq!(
            scheduler.task(task_id).expect("popped task").state,
            crate::chat::turn_scheduler::TurnTaskState::Dispatched,
            "chat-loop pop marks the task dispatched before post-route admission runs"
        );
        assert!(
            !provider_turn_visible_admission(
                &workers,
                crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
                1
            )
            .can_start_visible,
            "legacy-routed turn is rejected while another detached worker is active"
        );

        let mut defer_visible_input_pop_once = false;
        requeue_post_route_admission_rejected_input(&mut backlog, &mut defer_visible_input_pop_once, popped);

        assert_eq!(backlog.len(), 1, "post-route rejection must requeue the popped input");
        assert_eq!(
            backlog.front().and_then(|queued| queued.turn_task_id),
            Some(task_id),
            "requeued input keeps the original scheduler task id"
        );
        assert!(
            provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 2)
                .can_start_visible,
            "without the defer gate, the next pump pass would immediately re-pop under detached capacity"
        );
        assert!(
            consume_deferred_visible_input_pop(&mut defer_visible_input_pop_once),
            "post-route rejection must force one event-pump wait before the same input can pop again"
        );
        assert_eq!(
            backlog.len(),
            1,
            "deferred pump pass leaves the requeued input in place"
        );
        assert!(
            !consume_deferred_visible_input_pop(&mut defer_visible_input_pop_once),
            "defer gate is one-shot after the event-pump wait"
        );
    }

    #[test]
    fn provider_turn_completion_context_supplies_stable_history_boundary_once() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("context", crate::chat::turn_scheduler::TurnPriority::Normal, 9);
        let mut contexts = std::collections::HashMap::new();

        record_provider_turn_completion_context(&mut contexts, Some(task_id), 12);

        assert_eq!(
            take_provider_turn_completion_history_len(&mut contexts, Some(task_id), 99),
            12
        );
        assert_eq!(
            take_provider_turn_completion_history_len(&mut contexts, Some(task_id), 99),
            99,
            "completion context is single-use"
        );
        assert_eq!(
            take_provider_turn_completion_history_len(&mut contexts, None, 7),
            7,
            "legacy turns without task id use the local fallback"
        );
    }

    #[test]
    fn provider_terminal_plan_uses_completion_context_history_boundary() {
        let tools: Vec<Box<dyn Tool>> = Vec::new();
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("context plan", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
        let mut contexts = std::collections::HashMap::new();
        record_provider_turn_completion_context(&mut contexts, Some(task_id), 4);
        let history_len = take_provider_turn_completion_history_len(&mut contexts, Some(task_id), 99);

        let plan = provider_turn_terminal_plan_from_completion(
            ResolvedProviderTurnCompletion {
                outcome: Some(dispatcher::TurnOutcomeKind::Completed {
                    final_text: "context done".to_string(),
                    reasoning: String::new(),
                }),
                usage: crate::llm::route_decision::TokenUsage::default(),
            },
            history_len,
            &tools,
        );

        match plan {
            ProviderTurnTerminalPlan::Completed { history_commit_len, .. } => assert_eq!(history_commit_len, 5),
            other => panic!("unexpected terminal plan: {other:?}"),
        }
    }

    #[test]
    fn completed_provider_turn_finalization_gate_records_payload_and_commit() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("complete me", crate::chat::turn_scheduler::TurnPriority::Normal, 2);
        scheduler.start_task(task_id).expect("task starts");
        let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
        coordinator
            .register_task(scheduler.task(task_id).expect("task snapshot"))
            .expect("history task registers");
        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(task_id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("worker starts");
        workers.record_execution_started(task_id, 7).expect("execution starts");
        workers.record_completion_ready(task_id).expect("completion ready");
        let usage = crate::llm::route_decision::TokenUsage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: Some(15),
            ..Default::default()
        };

        let decisions = gate_completed_provider_turn_finalization(
            &mut scheduler,
            &mut coordinator,
            &mut workers,
            Some(task_id),
            3,
            12,
            10,
            &usage,
            "test completed",
        );

        assert_eq!(decisions.len(), 1, "completion gate should release one ready decision");
        assert_eq!(
            scheduler.task(task_id).unwrap().state,
            crate::chat::turn_scheduler::TurnTaskState::Completed
        );
        let row = workers
            .snapshot()
            .into_iter()
            .find(|row| row.task_id == task_id)
            .expect("worker snapshot retained");
        assert_eq!(row.finalized_total_tokens, Some(15));
        assert!(matches!(
            row.state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Committed
        ));
    }

    #[test]
    fn failed_provider_turn_finalization_gate_records_skip_decision() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("fail me", crate::chat::turn_scheduler::TurnPriority::Normal, 2);
        scheduler.start_task(task_id).expect("task starts");
        let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
        coordinator
            .register_task(scheduler.task(task_id).expect("task snapshot"))
            .expect("history task registers");
        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(task_id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("worker starts");
        workers.record_execution_started(task_id, 8).expect("execution starts");
        workers.record_completion_ready(task_id).expect("completion ready");

        let decisions = gate_failed_provider_turn_finalization(
            &mut scheduler,
            &mut coordinator,
            &mut workers,
            Some(task_id),
            2,
            "test failed",
        );

        assert_eq!(decisions.len(), 1, "failure gate should release one skip decision");
        assert_eq!(
            scheduler.task(task_id).unwrap().state,
            crate::chat::turn_scheduler::TurnTaskState::Failed
        );
        let row = workers
            .snapshot()
            .into_iter()
            .find(|row| row.task_id == task_id)
            .expect("worker snapshot retained");
        assert!(matches!(
            row.state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Failed
        ));
    }

    #[test]
    fn cancelled_provider_turn_finalization_gate_records_skip_decision() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("cancel me", crate::chat::turn_scheduler::TurnPriority::Normal, 2);
        scheduler.start_task(task_id).expect("task starts");
        scheduler.request_cancel(task_id).expect("task cancellation requested");
        let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
        coordinator
            .register_task(scheduler.task(task_id).expect("task snapshot"))
            .expect("history task registers");
        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(task_id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("worker starts");
        workers.record_execution_started(task_id, 9).expect("execution starts");
        workers.request_cancel(task_id).expect("worker cancellation requested");
        workers.record_completion_ready(task_id).expect("completion ready");

        let decisions = gate_cancelled_provider_turn_finalization(
            &mut scheduler,
            &mut coordinator,
            &mut workers,
            Some(task_id),
            "test cancelled",
        );

        assert_eq!(decisions.len(), 1, "cancellation gate should release one skip decision");
        assert_eq!(
            scheduler.task(task_id).unwrap().state,
            crate::chat::turn_scheduler::TurnTaskState::Cancelled
        );
        let row = workers
            .snapshot()
            .into_iter()
            .find(|row| row.task_id == task_id)
            .expect("worker snapshot retained");
        assert!(matches!(
            row.state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Cancelled
        ));
    }

    #[test]
    fn provider_completion_resolution_prefers_completion_event() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("event", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        let signal = dispatcher::TurnCompletionSignal::new();
        signal.register_turn(task_id, "draft-event");
        signal.record_and_notify(dispatcher::TurnOutcomeKind::Cancelled);
        signal.record_usage(crate::llm::route_decision::TokenUsage {
            total_tokens: Some(99),
            ..Default::default()
        });
        let event = ProviderTurnCompletionEvent {
            task_id,
            outcome: Some(dispatcher::TurnOutcomeKind::Completed {
                final_text: "event wins".to_string(),
                reasoning: String::new(),
            }),
            usage: crate::llm::route_decision::TokenUsage {
                total_tokens: Some(12),
                ..Default::default()
            },
        };

        let resolved = resolve_provider_turn_completion(&signal, Some(task_id), Some(event));

        match resolved.outcome {
            Some(dispatcher::TurnOutcomeKind::Completed { final_text, .. }) => {
                assert_eq!(final_text, "event wins");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert_eq!(resolved.usage.total_tokens, Some(12));
        assert!(signal.notified_for(task_id).is_none(), "task should unregister");
    }

    #[test]
    fn provider_completion_resolution_uses_legacy_usage_when_event_usage_empty() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue(
            "event empty usage",
            crate::chat::turn_scheduler::TurnPriority::Normal,
            0,
        );
        let signal = dispatcher::TurnCompletionSignal::new();
        signal.record_usage(crate::llm::route_decision::TokenUsage {
            total_tokens: Some(21),
            ..Default::default()
        });
        let event = ProviderTurnCompletionEvent {
            task_id,
            outcome: Some(dispatcher::TurnOutcomeKind::Cancelled),
            usage: crate::llm::route_decision::TokenUsage::default(),
        };

        let resolved = resolve_provider_turn_completion(&signal, Some(task_id), Some(event));

        assert!(matches!(resolved.outcome, Some(dispatcher::TurnOutcomeKind::Cancelled)));
        assert_eq!(resolved.usage.total_tokens, Some(21));
    }

    #[test]
    fn provider_completion_resolution_uses_keyed_signal_without_event() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("keyed", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        let signal = dispatcher::TurnCompletionSignal::new();
        signal.register_turn(task_id, "draft-keyed");
        assert!(signal.record_usage_for_draft(
            "draft-keyed",
            crate::llm::route_decision::TokenUsage {
                total_tokens: Some(34),
                ..Default::default()
            }
        ));
        assert!(signal.record_and_notify_for_draft(
            "draft-keyed",
            dispatcher::TurnOutcomeKind::Failed {
                err: "keyed failed".to_string(),
                retryable: false,
            },
        ));

        let resolved = resolve_provider_turn_completion(&signal, Some(task_id), None);

        match resolved.outcome {
            Some(dispatcher::TurnOutcomeKind::Failed { err, retryable }) => {
                assert_eq!(err, "keyed failed");
                assert!(!retryable);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert_eq!(resolved.usage.total_tokens, Some(34));
        assert!(signal.notified_for(task_id).is_none(), "task should unregister");
    }

    #[test]
    fn provider_turn_terminal_gate_dispatches_completed_finalization() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("unified gate", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
        scheduler.start_task(task_id).expect("task starts");
        let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
        coordinator
            .register_task(scheduler.task(task_id).expect("task snapshot"))
            .expect("history task registers");
        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(task_id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("worker starts");
        workers.record_execution_started(task_id, 55).expect("execution starts");
        workers.record_completion_ready(task_id).expect("completion ready");
        let usage = crate::llm::route_decision::TokenUsage {
            total_tokens: Some(55),
            ..Default::default()
        };

        let decisions = gate_provider_turn_terminal_finalization(
            &mut scheduler,
            &mut coordinator,
            &mut workers,
            Some(task_id),
            ProviderTurnTerminalGate::Completed {
                history_commit_len: 2,
                final_text_chars: 11,
                recorded_response_chars: 10,
                usage: &usage,
                summary: "test unified completed",
            },
        );

        assert_eq!(
            decisions.len(),
            1,
            "unified terminal gate should finalize completed turn"
        );
        let row = workers
            .snapshot()
            .into_iter()
            .find(|row| row.task_id == task_id)
            .expect("worker snapshot retained");
        assert_eq!(row.finalized_total_tokens, Some(55));
        assert!(matches!(
            row.state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Committed
        ));
    }

    #[test]
    fn provider_turn_finalizer_event_commits_completed_plan() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue(
            "finalizer completed",
            crate::chat::turn_scheduler::TurnPriority::Normal,
            1,
        );
        scheduler.start_task(task_id).expect("task starts");
        let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
        coordinator
            .register_task(scheduler.task(task_id).expect("task snapshot"))
            .expect("history task registers");
        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(task_id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("worker starts");
        workers.record_execution_started(task_id, 56).expect("execution starts");
        workers.record_completion_ready(task_id).expect("completion ready");

        let result = finalize_provider_turn_from_event(
            &mut scheduler,
            &mut coordinator,
            &mut workers,
            ProviderTurnFinalizerEvent {
                task_id: Some(task_id),
                plan: ProviderTurnTerminalPlan::Completed {
                    final_text: "done".to_string(),
                    reasoning: String::new(),
                    recorded_response: "done".to_string(),
                    empty_response: false,
                    usage: crate::llm::route_decision::TokenUsage {
                        total_tokens: Some(56),
                        ..Default::default()
                    },
                    history_commit_len: 2,
                    final_text_chars: 4,
                    recorded_response_chars: 4,
                    summary: "test finalizer completed",
                },
            },
        );

        assert_eq!(
            result,
            vec![ProviderTurnFinalizerResult {
                task_id: Some(task_id),
                terminal_status: "completed",
                finalized: true,
            }]
        );
        let row = workers
            .snapshot()
            .into_iter()
            .find(|row| row.task_id == task_id)
            .expect("worker snapshot retained");
        assert_eq!(row.finalized_total_tokens, Some(56));
        assert!(matches!(
            row.state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Committed
        ));
    }

    #[test]
    fn provider_turn_finalizer_event_closes_failed_and_cancelled_plans() {
        let mut failed_scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let failed_id =
            failed_scheduler.enqueue("finalizer failed", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
        failed_scheduler.start_task(failed_id).expect("failed task starts");
        let mut failed_coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
        failed_coordinator
            .register_task(failed_scheduler.task(failed_id).expect("failed task snapshot"))
            .expect("failed history task registers");
        let mut failed_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        failed_workers
            .start_from_task(
                failed_scheduler.task(failed_id).expect("failed task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("failed worker starts");
        failed_workers
            .record_execution_started(failed_id, 57)
            .expect("failed execution starts");
        failed_workers
            .record_completion_ready(failed_id)
            .expect("failed completion ready");

        let failed_result = finalize_provider_turn_from_event(
            &mut failed_scheduler,
            &mut failed_coordinator,
            &mut failed_workers,
            ProviderTurnFinalizerEvent {
                task_id: Some(failed_id),
                plan: ProviderTurnTerminalPlan::Failed {
                    err: "failed".to_string(),
                    history_commit_len: 1,
                    summary: "test finalizer failed".to_string(),
                },
            },
        );

        assert_eq!(failed_result.len(), 1);
        assert_eq!(failed_result[0].terminal_status, "failed");
        assert!(failed_result[0].finalized);
        assert!(matches!(
            failed_workers
                .snapshot()
                .into_iter()
                .find(|row| row.task_id == failed_id)
                .expect("failed worker snapshot")
                .state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Failed
        ));

        let mut cancelled_scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let cancelled_id = cancelled_scheduler.enqueue(
            "finalizer cancelled",
            crate::chat::turn_scheduler::TurnPriority::Normal,
            1,
        );
        cancelled_scheduler
            .start_task(cancelled_id)
            .expect("cancelled task starts");
        cancelled_scheduler
            .request_cancel(cancelled_id)
            .expect("cancelled task requested");
        let mut cancelled_coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
        cancelled_coordinator
            .register_task(cancelled_scheduler.task(cancelled_id).expect("cancelled task snapshot"))
            .expect("cancelled history task registers");
        let mut cancelled_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        cancelled_workers
            .start_from_task(
                cancelled_scheduler.task(cancelled_id).expect("cancelled task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("cancelled worker starts");
        cancelled_workers
            .record_execution_started(cancelled_id, 58)
            .expect("cancelled execution starts");
        cancelled_workers
            .request_cancel(cancelled_id)
            .expect("cancelled worker requested");
        cancelled_workers
            .record_completion_ready(cancelled_id)
            .expect("cancelled completion ready");

        let cancelled_result = finalize_provider_turn_from_event(
            &mut cancelled_scheduler,
            &mut cancelled_coordinator,
            &mut cancelled_workers,
            ProviderTurnFinalizerEvent {
                task_id: Some(cancelled_id),
                plan: ProviderTurnTerminalPlan::Cancelled {
                    summary: "test finalizer cancelled",
                },
            },
        );

        assert_eq!(cancelled_result.len(), 1);
        assert_eq!(cancelled_result[0].terminal_status, "cancelled");
        assert!(cancelled_result[0].finalized);
        assert!(matches!(
            cancelled_workers
                .snapshot()
                .into_iter()
                .find(|row| row.task_id == cancelled_id)
                .expect("cancelled worker snapshot")
                .state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Cancelled
        ));
    }

    #[test]
    fn provider_turn_finalizer_queue_drains_fifo_and_unlocks_commit_order() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let first = scheduler.enqueue("first failed", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
        let second = scheduler.enqueue("second completed", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
        scheduler.start_task(first).expect("first starts");
        scheduler.start_task(second).expect("second starts");

        let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
        coordinator
            .register_task(scheduler.task(first).expect("first task"))
            .expect("first history task registers");
        coordinator
            .register_task(scheduler.task(second).expect("second task"))
            .expect("second history task registers");

        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        for (id, lease) in [(first, 61), (second, 62)] {
            workers
                .start_from_task(
                    scheduler.task(id).expect("task snapshot"),
                    crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
                )
                .expect("worker starts");
            workers.record_execution_started(id, lease).expect("execution starts");
            workers.record_completion_ready(id).expect("completion ready");
        }

        let mut queue = std::collections::VecDeque::new();
        enqueue_provider_turn_finalizer_event(
            &mut queue,
            Some(first),
            ProviderTurnTerminalPlan::Failed {
                err: "first failed".to_string(),
                history_commit_len: 1,
                summary: "test first failed".to_string(),
            },
        );
        enqueue_provider_turn_finalizer_event(
            &mut queue,
            Some(second),
            ProviderTurnTerminalPlan::Completed {
                final_text: "second done".to_string(),
                reasoning: String::new(),
                recorded_response: "second done".to_string(),
                empty_response: false,
                usage: crate::llm::route_decision::TokenUsage {
                    total_tokens: Some(62),
                    ..Default::default()
                },
                history_commit_len: 2,
                final_text_chars: 11,
                recorded_response_chars: 11,
                summary: "test second completed",
            },
        );

        let results = drain_provider_turn_finalizer_events(&mut scheduler, &mut coordinator, &mut workers, &mut queue);

        assert!(queue.is_empty());
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].task_id, Some(first));
        assert_eq!(results[0].terminal_status, "failed");
        assert!(results[0].finalized);
        assert_eq!(results[1].task_id, Some(second));
        assert_eq!(results[1].terminal_status, "completed");
        assert!(results[1].finalized);
        let rows = workers.snapshot();
        assert!(matches!(
            rows.iter().find(|row| row.task_id == first).expect("first row").state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Failed
        ));
        let second_row = rows.iter().find(|row| row.task_id == second).expect("second row");
        assert_eq!(second_row.finalized_total_tokens, Some(62));
        assert!(matches!(
            second_row.state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Committed
        ));
    }

    #[test]
    fn provider_turn_finalizer_delays_later_completed_turn_until_earlier_ready() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let first = scheduler.enqueue("first slow", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
        let second = scheduler.enqueue("second fast", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
        scheduler.start_task(first).expect("first starts");
        scheduler.start_task(second).expect("second starts");
        let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
        coordinator
            .register_task(scheduler.task(first).expect("first task"))
            .expect("first history task registers");
        coordinator
            .register_task(scheduler.task(second).expect("second task"))
            .expect("second history task registers");
        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        for (id, lease) in [(first, 81), (second, 82)] {
            workers
                .start_from_task(
                    scheduler.task(id).expect("task snapshot"),
                    crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
                )
                .expect("worker starts");
            workers.record_execution_started(id, lease).expect("execution starts");
            workers.record_completion_ready(id).expect("completion ready");
        }

        let second_results = finalize_provider_turn_from_event(
            &mut scheduler,
            &mut coordinator,
            &mut workers,
            ProviderTurnFinalizerEvent {
                task_id: Some(second),
                plan: ProviderTurnTerminalPlan::Completed {
                    final_text: "second answer".to_string(),
                    reasoning: String::new(),
                    recorded_response: "second answer".to_string(),
                    empty_response: false,
                    usage: crate::llm::route_decision::TokenUsage {
                        total_tokens: Some(82),
                        ..Default::default()
                    },
                    history_commit_len: 3,
                    final_text_chars: 13,
                    recorded_response_chars: 13,
                    summary: "test second completed",
                },
            },
        );
        assert_eq!(
            second_results,
            vec![ProviderTurnFinalizerResult {
                task_id: Some(second),
                terminal_status: "unknown",
                finalized: false,
            }],
            "later completion must wait for the earlier sequence"
        );
        assert!(matches!(
            workers.worker(second).expect("second worker").state,
            crate::chat::turn_worker::ProviderTurnWorkerState::AwaitingCommit(_)
        ));

        let first_results = finalize_provider_turn_from_event(
            &mut scheduler,
            &mut coordinator,
            &mut workers,
            ProviderTurnFinalizerEvent {
                task_id: Some(first),
                plan: ProviderTurnTerminalPlan::Completed {
                    final_text: "first answer".to_string(),
                    reasoning: String::new(),
                    recorded_response: "first answer".to_string(),
                    empty_response: false,
                    usage: crate::llm::route_decision::TokenUsage {
                        total_tokens: Some(81),
                        ..Default::default()
                    },
                    history_commit_len: 3,
                    final_text_chars: 12,
                    recorded_response_chars: 12,
                    summary: "test first completed",
                },
            },
        );

        assert_eq!(
            first_results.iter().map(|result| result.task_id).collect::<Vec<_>>(),
            vec![Some(first), Some(second)],
            "earlier completion should release both ready decisions in sequence"
        );
        assert!(first_results.iter().all(|result| result.finalized));
        assert!(matches!(
            workers.worker(first).expect("first worker").state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Committed
        ));
        assert!(matches!(
            workers.worker(second).expect("second worker").state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Committed
        ));
    }

    #[test]
    fn provider_turn_finalizer_releases_later_completed_turn_when_earlier_cancelled() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let first = scheduler.enqueue("first cancelled", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
        let second = scheduler.enqueue("second held", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
        scheduler.start_task(first).expect("first starts");
        scheduler.request_cancel(first).expect("first cancellation requested");
        scheduler.start_task(second).expect("second starts");
        let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
        coordinator
            .register_task(scheduler.task(first).expect("first task"))
            .expect("first history task registers");
        coordinator
            .register_task(scheduler.task(second).expect("second task"))
            .expect("second history task registers");
        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(first).expect("first task"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("first worker starts");
        workers
            .record_execution_started(first, 91)
            .expect("first execution starts");
        workers
            .request_cancel(first)
            .expect("first worker cancellation requested");
        workers.record_completion_ready(first).expect("first completion ready");
        workers
            .start_from_task(
                scheduler.task(second).expect("second task"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("second worker starts");
        workers
            .record_execution_started(second, 92)
            .expect("second execution starts");
        workers
            .record_completion_ready(second)
            .expect("second completion ready");

        let second_results = finalize_provider_turn_from_event(
            &mut scheduler,
            &mut coordinator,
            &mut workers,
            ProviderTurnFinalizerEvent {
                task_id: Some(second),
                plan: ProviderTurnTerminalPlan::Completed {
                    final_text: "second after cancel".to_string(),
                    reasoning: String::new(),
                    recorded_response: "second after cancel".to_string(),
                    empty_response: false,
                    usage: crate::llm::route_decision::TokenUsage {
                        total_tokens: Some(92),
                        ..Default::default()
                    },
                    history_commit_len: 3,
                    final_text_chars: 19,
                    recorded_response_chars: 19,
                    summary: "test second completed after cancel",
                },
            },
        );
        assert_eq!(
            second_results,
            vec![ProviderTurnFinalizerResult {
                task_id: Some(second),
                terminal_status: "unknown",
                finalized: false,
            }],
            "later completion must remain held until earlier cancellation is ordered"
        );

        let first_results = finalize_provider_turn_from_event(
            &mut scheduler,
            &mut coordinator,
            &mut workers,
            ProviderTurnFinalizerEvent {
                task_id: Some(first),
                plan: ProviderTurnTerminalPlan::Cancelled {
                    summary: "test first cancelled",
                },
            },
        );

        assert_eq!(
            first_results
                .iter()
                .map(|result| (result.task_id, result.terminal_status))
                .collect::<Vec<_>>(),
            vec![(Some(first), "cancelled"), (Some(second), "completed")],
            "earlier cancellation should skip first and then release held later commit"
        );
        assert!(first_results.iter().all(|result| result.finalized));
        assert!(matches!(
            workers.worker(first).expect("first worker").state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Cancelled
        ));
        assert!(matches!(
            workers.worker(second).expect("second worker").state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Committed
        ));
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn provider_turn_finalizer_n3_out_of_order_releases_ordered_persistence_actions() {
        use crate::chat::action::Action;

        fn completed_plan(final_text: &str, total_tokens: u32, history_commit_len: usize) -> ProviderTurnTerminalPlan {
            ProviderTurnTerminalPlan::Completed {
                final_text: final_text.to_string(),
                reasoning: String::new(),
                recorded_response: final_text.to_string(),
                empty_response: false,
                usage: crate::llm::route_decision::TokenUsage {
                    total_tokens: Some(total_tokens),
                    ..Default::default()
                },
                history_commit_len,
                final_text_chars: final_text.chars().count(),
                recorded_response_chars: final_text.chars().count(),
                summary: "test n3 completed",
            }
        }

        fn pending_commit_for(
            task_id: crate::chat::turn_scheduler::TurnTaskId,
            draft_id: &str,
            user_input: &str,
            plan: ProviderTurnTerminalPlan,
        ) -> PendingOrderedProviderTurnCommit {
            PendingOrderedProviderTurnCommit {
                context: PerTurnContext {
                    task_id,
                    draft_id: draft_id.to_string(),
                    delta_tx: None,
                    tool_event_tx: None,
                    draft_updater: None,
                    tool_event_forwarder: None,
                    user_input: user_input.to_string(),
                    turn_run_id: format!("test-run-{}", task_id.get()),
                    route_scope: crate::memory::MessageEventScope::new(
                        "chat",
                        crate::memory::MemoryVisibility::Workspace,
                    ),
                    route_decision: RouteDecision::single_candidate("mock", "mock"),
                    provider_started_at: chrono::Utc::now(),
                    provider_name: "mock".to_string(),
                    model_name: "mock".to_string(),
                    history_len_before_user_turn: 0,
                    history_user_message: ChatMessage::user(user_input.to_string()),
                },
                terminal_plan: plan,
            }
        }

        fn dispatch_ready_for_test(
            results: Vec<ProviderTurnFinalizerResult>,
            pending: &mut std::collections::HashMap<
                crate::chat::turn_scheduler::TurnTaskId,
                PendingOrderedProviderTurnCommit,
            >,
            dispatcher: &dispatcher::ChatDispatcher,
        ) {
            for result in results {
                if !result.finalized || result.terminal_status != "completed" {
                    continue;
                }
                let task_id = result.task_id.expect("finalized result has task id");
                let pending_commit = pending.remove(&task_id).expect("pending commit payload");
                let ProviderTurnTerminalPlan::Completed {
                    final_text,
                    reasoning,
                    empty_response,
                    ..
                } = pending_commit.terminal_plan
                else {
                    panic!("expected completed terminal plan");
                };
                dispatch_ordered_provider_turn_commit(
                    dispatcher,
                    task_id,
                    &pending_commit.context.draft_id,
                    &pending_commit.context.user_input,
                    &final_text,
                    &reasoning,
                    empty_response,
                );
            }
        }

        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let first = scheduler.enqueue("alpha prompt", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        let second = scheduler.enqueue("bravo prompt", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        let third = scheduler.enqueue("charlie prompt", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        for task_id in [first, second, third] {
            scheduler.start_task(task_id).expect("task starts");
        }

        let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
        for task_id in [first, second, third] {
            coordinator
                .register_task(scheduler.task(task_id).expect("task snapshot"))
                .expect("history task registers");
        }

        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        for (task_id, lease) in [(first, 101), (second, 102), (third, 103)] {
            workers
                .start_from_task(
                    scheduler.task(task_id).expect("task snapshot"),
                    crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
                )
                .expect("worker starts");
            workers
                .record_execution_started(task_id, lease)
                .expect("execution starts");
            workers.record_completion_ready(task_id).expect("completion ready");
        }

        let first_plan = completed_plan("alpha answer", 11, 2);
        let second_plan = completed_plan("bravo answer", 12, 4);
        let third_plan = completed_plan("charlie answer", 13, 6);
        let mut pending_ordered_provider_turn_commits = std::collections::HashMap::from([
            (
                first,
                pending_commit_for(first, "draft-alpha", "alpha prompt", first_plan.clone()),
            ),
            (
                second,
                pending_commit_for(second, "draft-bravo", "bravo prompt", second_plan.clone()),
            ),
            (
                third,
                pending_commit_for(third, "draft-charlie", "charlie prompt", third_plan.clone()),
            ),
        ]);
        let (dispatcher, mut rx) = dispatcher::ChatDispatcher::new();

        let third_results = finalize_provider_turn_from_event(
            &mut scheduler,
            &mut coordinator,
            &mut workers,
            ProviderTurnFinalizerEvent {
                task_id: Some(third),
                plan: third_plan,
            },
        );
        assert_eq!(
            third_results,
            vec![ProviderTurnFinalizerResult {
                task_id: Some(third),
                terminal_status: "unknown",
                finalized: false,
            }],
            "third completion must stay held until first and second are terminal"
        );
        dispatch_ready_for_test(third_results, &mut pending_ordered_provider_turn_commits, &dispatcher);
        assert!(
            rx.try_recv().is_err(),
            "held third turn must emit no persistence actions"
        );
        assert!(matches!(
            workers.worker(third).expect("third worker").state,
            crate::chat::turn_worker::ProviderTurnWorkerState::AwaitingCommit(_)
        ));

        let second_results = finalize_provider_turn_from_event(
            &mut scheduler,
            &mut coordinator,
            &mut workers,
            ProviderTurnFinalizerEvent {
                task_id: Some(second),
                plan: second_plan,
            },
        );
        assert_eq!(
            second_results,
            vec![ProviderTurnFinalizerResult {
                task_id: Some(second),
                terminal_status: "unknown",
                finalized: false,
            }],
            "second completion must stay held until first is terminal"
        );
        dispatch_ready_for_test(second_results, &mut pending_ordered_provider_turn_commits, &dispatcher);
        assert!(
            rx.try_recv().is_err(),
            "held second turn must emit no persistence actions"
        );

        let first_results = finalize_provider_turn_from_event(
            &mut scheduler,
            &mut coordinator,
            &mut workers,
            ProviderTurnFinalizerEvent {
                task_id: Some(first),
                plan: first_plan,
            },
        );
        assert_eq!(
            first_results
                .iter()
                .map(|result| (result.task_id, result.terminal_status, result.finalized))
                .collect::<Vec<_>>(),
            vec![
                (Some(first), "completed", true),
                (Some(second), "completed", true),
                (Some(third), "completed", true),
            ],
            "first completion must release all held completed turns in dispatch order"
        );
        dispatch_ready_for_test(first_results, &mut pending_ordered_provider_turn_commits, &dispatcher);

        for (expected_user, expected_assistant, expected_draft, expected_task) in [
            ("alpha prompt", "alpha answer", "draft-alpha", first),
            ("bravo prompt", "bravo answer", "draft-bravo", second),
            ("charlie prompt", "charlie answer", "draft-charlie", third),
        ] {
            match rx.try_recv().expect("ordered user action") {
                Action::RecordUserTurn(content) => assert_eq!(content, expected_user),
                other => panic!("expected RecordUserTurn, got {other:?}"),
            }
            match rx.try_recv().expect("ordered assistant action") {
                Action::RecordAssistantTurn { task_id, content } => {
                    assert_eq!(task_id, Some(expected_task));
                    assert_eq!(content, expected_assistant);
                }
                other => panic!("expected RecordAssistantTurn, got {other:?}"),
            }
            match rx.try_recv().expect("ordered stream completed action") {
                Action::StreamCompleted {
                    draft_id,
                    final_text,
                    reasoning,
                } => {
                    assert_eq!(draft_id, expected_draft);
                    assert_eq!(final_text, expected_assistant);
                    assert!(reasoning.is_empty());
                }
                other => panic!("expected StreamCompleted, got {other:?}"),
            }
        }
        assert!(
            rx.try_recv().is_err(),
            "ordered dispatch should emit exactly three action triplets"
        );
        assert!(pending_ordered_provider_turn_commits.is_empty());
        assert_eq!(coordinator.pending_tasks(), 0);
        assert_eq!(coordinator.pending_outcomes(), 0);
        for task_id in [first, second, third] {
            assert!(matches!(
                workers.worker(task_id).expect("worker").state,
                crate::chat::turn_worker::ProviderTurnWorkerState::Committed
            ));
        }
    }

    #[tokio::test]
    async fn provider_turn_finalizer_drain_helper_publishes_queue_and_worker_status() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue(
            "publish finalizer",
            crate::chat::turn_scheduler::TurnPriority::Normal,
            1,
        );
        scheduler.start_task(task_id).expect("task starts");
        let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
        coordinator
            .register_task(scheduler.task(task_id).expect("task snapshot"))
            .expect("history task registers");
        let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        workers
            .start_from_task(
                scheduler.task(task_id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("worker starts");
        workers.record_execution_started(task_id, 63).expect("execution starts");
        workers.record_completion_ready(task_id).expect("completion ready");
        let mut queue = std::collections::VecDeque::new();
        enqueue_provider_turn_finalizer_event(
            &mut queue,
            Some(task_id),
            ProviderTurnTerminalPlan::Completed {
                final_text: "published".to_string(),
                reasoning: String::new(),
                recorded_response: "published".to_string(),
                empty_response: false,
                usage: crate::llm::route_decision::TokenUsage {
                    total_tokens: Some(63),
                    ..Default::default()
                },
                history_commit_len: 2,
                final_text_chars: 9,
                recorded_response_chars: 9,
                summary: "test finalizer published",
            },
        );
        let (dispatcher, mut rx) = dispatcher::ChatDispatcher::new();

        let results = drain_provider_turn_finalizer_events_and_publish(
            &mut scheduler,
            &mut coordinator,
            &mut workers,
            &mut queue,
            &dispatcher,
        );

        assert_eq!(results.len(), 1);
        assert!(results[0].finalized);
        assert!(queue.is_empty());
        let first = rx.recv().await.expect("main queue status action");
        let second = rx.recv().await.expect("provider worker status action");
        assert!(matches!(
            first,
            crate::chat::action::Action::MainQueueStatusUpdated { .. }
        ));
        match second {
            crate::chat::action::Action::ProviderWorkerStatusUpdated { status } => {
                assert_eq!(status.finalized_payloads, 1);
                assert_eq!(status.finalized_total_tokens, 63);
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn provider_terminal_plan_completed_non_empty_builds_gate_fields() {
        let tools: Vec<Box<dyn Tool>> = Vec::new();
        let resolved = ResolvedProviderTurnCompletion {
            outcome: Some(dispatcher::TurnOutcomeKind::Completed {
                final_text: "P7J terminal plan".to_string(),
                reasoning: "kept reasoning".to_string(),
            }),
            usage: crate::llm::route_decision::TokenUsage {
                total_tokens: Some(77),
                ..Default::default()
            },
        };

        let plan = provider_turn_terminal_plan_from_completion(resolved, 4, &tools);

        match plan {
            ProviderTurnTerminalPlan::Completed {
                final_text,
                recorded_response,
                reasoning,
                empty_response,
                usage,
                history_commit_len,
                final_text_chars,
                recorded_response_chars,
                summary,
            } => {
                assert_eq!(final_text, "P7J terminal plan");
                assert_eq!(recorded_response, "P7J terminal plan");
                assert_eq!(reasoning, "kept reasoning");
                assert!(!empty_response);
                assert_eq!(usage.total_tokens, Some(77));
                assert_eq!(history_commit_len, 5);
                assert_eq!(final_text_chars, "P7J terminal plan".chars().count());
                assert_eq!(recorded_response_chars, "P7J terminal plan".chars().count());
                assert_eq!(summary, "redux driver completed");
            }
            other => panic!("unexpected terminal plan: {other:?}"),
        }
    }

    #[test]
    fn provider_terminal_plan_empty_failed_and_cancelled_keep_boundaries() {
        let tools: Vec<Box<dyn Tool>> = Vec::new();
        let empty = provider_turn_terminal_plan_from_completion(
            ResolvedProviderTurnCompletion {
                outcome: Some(dispatcher::TurnOutcomeKind::Completed {
                    final_text: "   ".to_string(),
                    reasoning: "hidden reasoning".to_string(),
                }),
                usage: crate::llm::route_decision::TokenUsage::default(),
            },
            6,
            &tools,
        );
        match empty {
            ProviderTurnTerminalPlan::Completed {
                empty_response,
                history_commit_len,
                final_text_chars,
                recorded_response_chars,
                summary,
                ..
            } => {
                assert!(empty_response);
                assert_eq!(history_commit_len, 6);
                assert_eq!(final_text_chars, 0);
                assert_eq!(recorded_response_chars, 0);
                assert_eq!(summary, "redux driver completed with empty response");
            }
            other => panic!("unexpected empty terminal plan: {other:?}"),
        }

        let failed = provider_turn_terminal_plan_from_completion(
            ResolvedProviderTurnCompletion {
                outcome: Some(dispatcher::TurnOutcomeKind::Failed {
                    err: "provider failed".to_string(),
                    retryable: false,
                }),
                usage: crate::llm::route_decision::TokenUsage::default(),
            },
            7,
            &tools,
        );
        match failed {
            ProviderTurnTerminalPlan::Failed {
                err,
                history_commit_len,
                summary,
            } => {
                assert_eq!(err, "provider failed");
                assert_eq!(history_commit_len, 7);
                assert_eq!(summary, "redux driver failed: provider failed");
            }
            other => panic!("unexpected failed terminal plan: {other:?}"),
        }

        let cancelled = provider_turn_terminal_plan_from_completion(
            ResolvedProviderTurnCompletion {
                outcome: None,
                usage: crate::llm::route_decision::TokenUsage::default(),
            },
            8,
            &tools,
        );
        match cancelled {
            ProviderTurnTerminalPlan::Cancelled { summary } => {
                assert_eq!(summary, "redux driver cancelled");
            }
            other => panic!("unexpected cancelled terminal plan: {other:?}"),
        }
    }

    #[tokio::test]
    async fn active_turn_sessions_command_is_handled_without_enqueue() {
        let mut backlog = std::collections::VecDeque::new();
        enqueue_input_message(&mut backlog, input_msg("normal one"));
        let scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let msg = input_msg("/sessions");
        let chat_session = session::ChatSession::new("p", "m");
        let provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        let mut chat_sessions =
            crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())));
        let session_rings = std::collections::HashMap::new();
        let mut reaped_log_archive = ReapedSessionLogArchive::default();
        let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();

        let output = active_turn_local_command_output(
            &msg,
            &backlog,
            &scheduler,
            &chat_session,
            &provider_turn_workers,
            &mut chat_sessions,
            &session_rings,
            &mut reaped_log_archive,
            &reap_policy,
            &tools_registry,
        )
        .await
        .expect("sessions command output");

        assert_eq!(output, "No child TUI sessions.");
        assert_eq!(backlog.len(), 1, "read-only sessions command must not mutate backlog");
    }

    #[tokio::test]
    async fn active_turn_logs_command_is_handled_without_enqueue() {
        let mut backlog = std::collections::VecDeque::new();
        enqueue_input_message(&mut backlog, input_msg("normal one"));
        let scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let msg = input_msg("/logs #99");
        let chat_session = session::ChatSession::new("p", "m");
        let provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        let mut chat_sessions =
            crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())));
        let session_rings = std::collections::HashMap::new();
        let mut reaped_log_archive = ReapedSessionLogArchive::default();
        let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();

        let output = active_turn_local_command_output(
            &msg,
            &backlog,
            &scheduler,
            &chat_session,
            &provider_turn_workers,
            &mut chat_sessions,
            &session_rings,
            &mut reaped_log_archive,
            &reap_policy,
            &tools_registry,
        )
        .await
        .expect("logs command output");

        assert!(output.contains("Logs failed: no session #99"), "{output}");
        assert_eq!(backlog.len(), 1, "read-only logs command must not mutate backlog");
    }

    #[tokio::test]
    async fn active_turn_kill_command_is_handled_without_enqueue() {
        let mut backlog = std::collections::VecDeque::new();
        enqueue_input_message(&mut backlog, input_msg("normal one"));
        let scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let msg = input_msg("/kill #99");
        let chat_session = session::ChatSession::new("p", "m");
        let provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        let mut chat_sessions =
            crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())));
        let session_rings = std::collections::HashMap::new();
        let mut reaped_log_archive = ReapedSessionLogArchive::default();
        let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();

        let output = active_turn_local_command_output(
            &msg,
            &backlog,
            &scheduler,
            &chat_session,
            &provider_turn_workers,
            &mut chat_sessions,
            &session_rings,
            &mut reaped_log_archive,
            &reap_policy,
            &tools_registry,
        )
        .await
        .expect("kill command output");

        assert!(output.contains("Kill failed: no session #99"), "{output}");
        assert_eq!(backlog.len(), 1, "explicit kill command must not mutate backlog");
    }

    #[test]
    fn queued_input_scheduler_prefers_priority_over_older_normal() {
        let mut backlog = std::collections::VecDeque::new();
        enqueue_input_message(&mut backlog, input_msg("normal one"));
        enqueue_input_message(&mut backlog, input_msg("/priority urgent two"));
        enqueue_input_message(&mut backlog, input_msg("normal three"));

        let first = pop_next_input_message(&mut backlog).expect("priority first");
        let second = pop_next_input_message(&mut backlog).expect("normal one second");
        let third = pop_next_input_message(&mut backlog).expect("normal three third");

        assert_eq!(first.content, "urgent two");
        assert_eq!(second.content, "normal one");
        assert_eq!(third.content, "normal three");
    }

    #[test]
    fn cancelled_turn_rolls_back_legacy_history_user_message() {
        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user("previous question"),
            ChatMessage::assistant("previous answer"),
        ];
        let len_before = history.len();
        history.push(ChatMessage::user("cancelled long task"));

        rollback_cancelled_turn_history(&mut history, len_before);

        assert_eq!(history.len(), len_before);
        assert!(
            history.iter().all(|message| message.content != "cancelled long task"),
            "cancelled turn must not leak into the next queued prompt"
        );
    }

    #[test]
    fn p4a_shutdown_cancelled_pending_turn_rolls_back_to_user_boundary() {
        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user("previous question"),
            ChatMessage::assistant("previous answer"),
        ];
        let len_before_user_turn = history.len();
        history.push(ChatMessage::user("pending provider turn"));
        history.push(ChatMessage::assistant("partial assistant text"));

        rollback_cancelled_turn_history(&mut history, len_before_user_turn);

        assert_eq!(history.len(), len_before_user_turn);
        assert!(
            history.iter().all(
                |message| message.content != "pending provider turn" && message.content != "partial assistant text"
            ),
            "shutdown cancellation must remove both the queued user turn and partial assistant output"
        );
    }

    #[test]
    fn queued_input_notice_is_single_line_and_bounded() {
        let notice = format_queued_input_notice("  first line\nsecond\tline  ", InputQueuePriority::Normal);
        assert_eq!(notice, "queued > first line second line");

        let long = format_queued_input_notice(&"x".repeat(220), InputQueuePriority::Normal);
        assert!(long.starts_with("queued > "));
        assert!(long.ends_with('…'));
        assert!(long.chars().count() <= "queued > ".chars().count() + 160);

        let priority = format_queued_input_notice("/now urgent", InputQueuePriority::Priority);
        assert_eq!(priority, "priority queued > urgent");
    }

    #[test]
    fn active_turn_local_notice_strips_priority_prefix() {
        assert!(is_active_turn_local_command("/now /cost"));
        assert!(is_active_turn_local_command("/workers"));
        assert!(is_active_turn_local_command("/workers cancel w#1"));
        assert!(is_active_turn_local_command("/sessions"));
        assert!(is_active_turn_local_command("/now /logs #1"));
        assert!(is_active_turn_local_command("/priority /kill 2"));
        assert!(!is_active_turn_local_command("/bg create child"));
        assert_eq!(format_active_turn_local_notice("/now /cost"), "local > /cost");
        assert_eq!(
            format_active_turn_local_notice("/priority /queue status"),
            "local > /queue status"
        );
        assert_eq!(
            format_active_turn_local_notice("/priority /workers cancel #2"),
            "local > /workers cancel #2"
        );
    }

    #[test]
    fn session_to_session_switch_suppresses_attach_breadcrumb_spam() {
        let meta = session_view(1);
        let projection =
            build_active_session_attach_projection(1, Some(&meta), vec!["tail".into()], vec!["ring".into()], false);

        assert!(
            attach_breadcrumb_for_transition(false, &projection).is_some(),
            "Main->Session entry keeps one breadcrumb"
        );

        let switches = [
            build_active_session_attach_projection(2, Some(&session_view(2)), Vec::new(), Vec::new(), false),
            build_active_session_attach_projection(3, Some(&session_view(3)), Vec::new(), Vec::new(), false),
            build_active_session_attach_projection(1, Some(&session_view(1)), Vec::new(), Vec::new(), false),
        ];
        let emitted = switches
            .iter()
            .filter_map(|projection| attach_breadcrumb_for_transition(true, projection))
            .count();
        assert_eq!(
            emitted, 0,
            "Session->Session directional cycling must not append extra main-history breadcrumbs"
        );
    }
}

#[cfg(test)]
mod p7a_resume_tests {
    use super::*;
    use crate::chat::sessions::SessionEvent;
    use crate::chat::sessions::id::SessionId;
    use std::collections::HashSet;

    #[test]
    fn resumed_chat_session_ignores_late_events_from_detached_child_sessions() {
        let old_id = SessionId::from_run_id("old-child");
        let new_id = SessionId::from_run_id("new-child");
        let ignored_session_events = HashSet::from([old_id.clone()]);

        let old_event = SessionEvent::Delta {
            id: old_id,
            text: "must not enter resumed chat session".to_string(),
        };
        let new_event = SessionEvent::Delta {
            id: new_id,
            text: "valid new child output".to_string(),
        };

        assert!(
            should_ignore_session_event_after_chat_resume(&ignored_session_events, &old_event),
            "late events from detached child sessions must be dropped before ring/history routing"
        );
        assert!(
            !should_ignore_session_event_after_chat_resume(&ignored_session_events, &new_event),
            "events from new child sessions must still route normally after resume"
        );
    }
}

#[cfg(test)]
mod p7b_branch_rewind_tests {
    use super::*;

    fn turn(role: &str, content: &str) -> session::ChatTurn {
        session::ChatTurn {
            role: role.to_string(),
            content: content.to_string(),
            timestamp: chrono::Utc::now(),
            tool_calls: Vec::new(),
        }
    }

    fn child_summary(id: &str) -> crate::chat::sessions::PersistedSessionSummary {
        crate::chat::sessions::PersistedSessionSummary {
            id: id.to_string(),
            seq: 1,
            kind: "agent".to_string(),
            origin: "test".to_string(),
            status: "done".to_string(),
            title: "child".to_string(),
            summary: "summary".to_string(),
            token_usage_records: Vec::new(),
            created_at: chrono::Utc::now(),
        }
    }

    fn sample_session() -> session::ChatSession {
        let mut s = session::ChatSession::new("kimi-code", "kimi2.6");
        s.title = "source".to_string();
        s.turns = vec![
            turn("user", "first"),
            turn("assistant", "second"),
            turn("user", "third"),
        ];
        s.background_sessions = vec![child_summary("child-1")];
        s
    }

    #[test]
    fn branch_rewind_turn_boundary_parser_rejects_bad_and_out_of_range() {
        assert_eq!(parse_turn_boundary("0", 3, "branch"), Ok(0));
        assert_eq!(parse_turn_boundary("3", 3, "rewind"), Ok(3));
        assert!(parse_turn_boundary("", 3, "rewind").is_err());
        assert!(parse_turn_boundary("2 extra", 3, "branch").is_err());
        assert!(parse_turn_boundary("nan", 3, "branch").is_err());
        assert!(parse_turn_boundary("4", 3, "rewind").is_err());
    }

    #[test]
    fn branch_and_rewind_prefixes_are_exact_and_ordered() {
        let source = sample_session();
        let branch = branched_chat_session_from(&source, 2, "kimi-code", "kimi2.6");
        let rewound = rewound_chat_session_from(&source, 2);

        assert_ne!(branch.id, source.id, "branch must fork a new saved session id");
        assert!(branch.id.starts_with("branch-"));
        assert!(
            branch.id.chars().all(|ch| ch == '-' || ch.is_ascii_lowercase()),
            "branch id must stay memory-safety friendly: {}",
            branch.id
        );
        assert_eq!(branch.turns.len(), 2);
        assert_eq!(rewound.id, source.id, "rewind trims current session in place");
        assert_eq!(rewound.turns.len(), 2);
        for (idx, source_turn) in source.turns.iter().take(2).enumerate() {
            let branch_turn = branch.turns.get(idx).expect("branch retains the requested prefix");
            let rewound_turn = rewound.turns.get(idx).expect("rewind retains the requested prefix");
            assert_eq!(branch_turn.role, source_turn.role);
            assert_eq!(branch_turn.content, source_turn.content);
            assert_eq!(rewound_turn.role, source_turn.role);
            assert_eq!(rewound_turn.content, source_turn.content);
        }
    }

    #[test]
    fn child_summaries_survive_only_at_full_length_boundary() {
        let source = sample_session();
        let full_branch = branched_chat_session_from(&source, source.turn_count(), "kimi-code", "kimi2.6");
        let short_branch = branched_chat_session_from(&source, 2, "kimi-code", "kimi2.6");
        let full_rewind = rewound_chat_session_from(&source, source.turn_count());
        let short_rewind = rewound_chat_session_from(&source, 2);

        assert_eq!(full_branch.background_sessions.len(), 1);
        assert!(short_branch.background_sessions.is_empty());
        assert_eq!(full_rewind.background_sessions.len(), 1);
        assert!(short_rewind.background_sessions.is_empty());
    }

    fn assert_same_turns(left: &session::ChatSession, right: &session::ChatSession) {
        assert_eq!(left.turns.len(), right.turns.len());
        for (left_turn, right_turn) in left.turns.iter().zip(right.turns.iter()) {
            assert_eq!(left_turn.role, right_turn.role);
            assert_eq!(left_turn.content, right_turn.content);
        }
    }

    #[tokio::test]
    async fn rewind_denied_approval_does_not_apply_trimmed_session() {
        let source = sample_session();
        let target = rewound_chat_session_from(&source, 1);
        let (tx, rx) = tokio::sync::oneshot::channel();
        tx.send(false).expect("send deny");
        let outcome = resolve_rewind_approval("chat_rewind:test-deny", rx.await);

        let current = if matches!(outcome, RewindApprovalOutcome::Apply) {
            target
        } else {
            source.clone()
        };

        assert_eq!(current.turns.len(), 3, "denied rewind must leave session unchanged");
        assert_same_turns(&current, &source);
        match outcome {
            RewindApprovalOutcome::Cancelled(message) => {
                assert!(
                    message.contains("unchanged"),
                    "message should be fail-closed: {message}"
                );
            }
            RewindApprovalOutcome::Apply => panic!("deny must not enter apply arm"),
        }
    }

    #[tokio::test]
    async fn rewind_dropped_approval_channel_does_not_apply_trimmed_session() {
        let source = sample_session();
        let target = rewound_chat_session_from(&source, 1);
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        drop(tx);
        let outcome = resolve_rewind_approval("chat_rewind:test-drop", rx.await);

        let current = if matches!(outcome, RewindApprovalOutcome::Apply) {
            target
        } else {
            source.clone()
        };

        assert_eq!(current.turns.len(), 3, "dropped approval must leave session unchanged");
        assert_same_turns(&current, &source);
        match outcome {
            RewindApprovalOutcome::Cancelled(message) => {
                assert!(
                    message.contains("chat_rewind:test-drop") && message.contains("unchanged"),
                    "message should include tool id and unchanged state: {message}"
                );
            }
            RewindApprovalOutcome::Apply => panic!("dropped channel must not enter apply arm"),
        }
    }
}

#[cfg(all(test, feature = "terminal-tui"))]
mod regfix_approval_switch_tests {
    use super::*;
    use crate::chat::action::Action;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    #[tokio::test]
    async fn session_switch_fail_closes_pending_approvals_and_resets_mirror() {
        let (chat_dispatcher, mut action_rx) = dispatcher::ChatDispatcher::new();
        let approval_router = Arc::new(dispatcher::ApprovalRouter::new());
        let (router_tx, mut router_rx) = tokio::sync::oneshot::channel::<bool>();
        assert!(approval_router.register("tool-live".to_string(), router_tx));

        let (_rewind_tx, rewind_rx) = tokio::sync::oneshot::channel::<bool>();
        let (_apply_tx, apply_rx) = tokio::sync::oneshot::channel::<bool>();
        let mut pending_chat_rewind = Some(PendingChatRewind {
            tool_id: "rewind-live".to_string(),
            target_session: session::ChatSession::new("p", "m"),
            approval_rx: rewind_rx,
        });
        let plan = diff_apply::parse_unified_diff(
            "diff --git a/foo.txt b/foo.txt\n--- a/foo.txt\n+++ b/foo.txt\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .expect("valid diff fixture");
        let mut pending_diff_apply = Some(PendingDiffApply {
            tool_id: "diff-live".to_string(),
            plan,
            approval_rx: apply_rx,
        });

        let mut chat_session = session::ChatSession::new("p", "m");
        chat_session.id = "current-session".to_string();
        chat_session.add_user_turn("current");
        let mut loaded_session = session::ChatSession::new("p", "m");
        loaded_session.id = "loaded-session".to_string();
        loaded_session.title = "Loaded".to_string();
        loaded_session.add_user_turn("hello");
        loaded_session.add_assistant_turn("hi", vec![]);
        let mut chat_session_key = "chat:current-session".to_string();
        let mut fabric_turn_seq = 9;
        let mut history = vec![ChatMessage::user("old history")];
        let active_runs = Arc::new(RwLock::new(Vec::<crate::tools::sessions_spawn::SubAgentRun>::new()));
        let mut chat_sessions = crate::chat::sessions::ChatSessionsHandle::new(active_runs);
        let mut ignored_session_events = std::collections::HashSet::new();
        let mut session_rings = std::collections::HashMap::new();
        let mut reported_sessions = std::collections::HashSet::new();
        let mut announced_started_sessions = std::collections::HashSet::from(["stale-started".to_string()]);
        let mut last_sessions_summary = "stale summary".to_string();
        let mut last_sessions_entries = vec![crate::chat::sessions::SwitcherEntry {
            seq: 1,
            kind: "agent",
            origin: "model",
            status: "running",
            title: "stale".to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
        }];
        let mut attached_follow = None;
        let mut attached_follow_seq = Some(7);
        let config = Config::default();
        let tool_descs: Vec<(&str, &str)> = Vec::new();
        let skills: Vec<crate::skills::Skill> = Vec::new();
        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
        let chat_mirror = Arc::new(parking_lot::Mutex::new(tui::TuiState::new("p", "m")));
        {
            let mut mirror = chat_mirror.lock();
            mirror.pending_tool_approval = Some(crate::chat::sessions::PendingToolApprovalView {
                task_id: None,
                tool_id: "tool-live".to_string(),
                name: "shell".to_string(),
                args: "{}".to_string(),
                selected_approval: false,
            });
            mirror.context_used_tokens = Some(2_500);
            mirror.context_window_tokens = Some(10_000_000);
            mirror.external_editor_prefix_armed = true;
            mirror.input.set_text("draft");
            assert!(mirror.input.begin_or_cycle_reverse_search());
            mirror.focus = crate::chat::sessions::FocusTarget::Approval;
        }

        apply_chat_session_switch(
            ChatSwitchCtx {
                chat_session: &mut chat_session,
                chat_session_key: &mut chat_session_key,
                fabric_turn_seq: &mut fabric_turn_seq,
                history: &mut history,
                approval_router: Some(&approval_router),
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
                redraw_handle: None,
                config: &config,
                provider_name: "p",
                model_name: "m",
                tool_descs: &tool_descs,
                skills: &skills,
                native_tools: false,
                tools_registry: &tools_registry,
                chat_mirror: &chat_mirror,
            },
            loaded_session,
        )
        .await;

        assert_eq!(router_rx.try_recv(), Ok(false), "router approval must be denied");
        assert!(pending_chat_rewind.is_none());
        assert!(pending_diff_apply.is_none());
        assert!(!approval_router.has_pending());
        assert_eq!(chat_session.id, "loaded-session");
        assert_eq!(chat_session_key, "chat:loaded-session");
        assert_eq!(fabric_turn_seq, 1);
        assert!(announced_started_sessions.is_empty());
        assert!(last_sessions_summary.is_empty());
        assert!(last_sessions_entries.is_empty());
        assert!(attached_follow.is_none());
        assert!(attached_follow_seq.is_none());
        {
            let mirror = chat_mirror.lock();
            assert!(mirror.pending_tool_approval.is_none());
            assert_eq!(mirror.context_used_tokens, None);
            assert_eq!(mirror.context_window_tokens, None);
            assert!(!mirror.external_editor_prefix_armed);
            assert!(!mirror.input.is_reverse_search_active());
            assert_eq!(mirror.focus, crate::chat::sessions::FocusTarget::Main);
            assert_eq!(mirror.turn_count, 2);
        }

        let mut received = Vec::new();
        let mut saw_clear = false;
        while let Ok(action) = action_rx.try_recv() {
            match action {
                Action::ToolApprovalReceived { tool_id, approved } => received.push((tool_id, approved)),
                Action::ToolApprovalCleared => saw_clear = true,
                _ => {}
            }
        }
        assert!(received.contains(&("tool-live".to_string(), false)));
        assert!(received.contains(&("rewind-live".to_string(), false)));
        assert!(received.contains(&("diff-live".to_string(), false)));
        assert!(saw_clear, "switch must clear reducer approval display");
    }

    #[test]
    fn approval_in_progress_covers_router_rewind_and_diff_apply() {
        let router = Arc::new(dispatcher::ApprovalRouter::new());
        let (tx, _rx) = tokio::sync::oneshot::channel::<bool>();
        assert!(router.register("tool-live".to_string(), tx));
        assert!(approval_in_progress(Some(&router), &None, &None));

        let (_tx, rewind_rx) = tokio::sync::oneshot::channel::<bool>();
        let pending_rewind = Some(PendingChatRewind {
            tool_id: "rewind-live".to_string(),
            target_session: session::ChatSession::new("p", "m"),
            approval_rx: rewind_rx,
        });
        assert!(approval_in_progress(None, &pending_rewind, &None));
    }
}

#[cfg(all(test, feature = "terminal-tui"))]
mod p6b1_transcript_tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn open_transcript_view_sets_read_only_focus_without_session_seq() {
        let mirror = Arc::new(parking_lot::Mutex::new(tui::TuiState::new("p", "m")));
        {
            let mut guard = mirror.lock();
            guard.session_title = "main chat".to_string();
            guard.conversation_lines.push(tui::ConversationLine::User {
                content: "hello".to_string(),
            });
        }
        let (dispatcher, mut action_rx) = crate::chat::dispatcher::ChatDispatcher::new();
        let (redraw_tx, mut redraw_rx) = mpsc::channel(1);

        open_transcript_view(&mirror, &dispatcher, Some(&redraw_tx), None);

        {
            let guard = mirror.lock();
            assert_eq!(guard.focus, crate::chat::sessions::FocusTarget::Transcript);
            assert_eq!(
                guard.focus.session_seq(),
                None,
                "transcript focus must not become a steerable attached session"
            );
            assert!(
                guard
                    .active_session_view
                    .as_ref()
                    .is_some_and(|view| view.kind == crate::chat::sessions::model::ManagedKind::Transcript.as_str())
            );
        }
        match action_rx.try_recv().expect("focus action") {
            crate::chat::action::Action::SessionFocusChanged { focus } => {
                assert_eq!(focus, crate::chat::sessions::FocusTarget::Transcript);
                assert_eq!(focus.session_seq(), None);
            }
            other => panic!("expected SessionFocusChanged, got {other:?}"),
        }
        match action_rx.try_recv().expect("active view action") {
            crate::chat::action::Action::ActiveSessionViewUpdated { view } => {
                let view = view.expect("transcript view");
                assert_eq!(
                    view.kind,
                    crate::chat::sessions::model::ManagedKind::Transcript.as_str()
                );
            }
            other => panic!("expected ActiveSessionViewUpdated, got {other:?}"),
        }
        assert!(redraw_rx.try_recv().is_ok(), "open should request redraw");
    }

    #[test]
    fn open_transcript_view_prefers_redux_snapshot_over_empty_mirror() {
        let mirror = Arc::new(parking_lot::Mutex::new(tui::TuiState::new("p", "m")));
        {
            let guard = mirror.lock();
            assert!(
                guard.conversation_lines.is_empty(),
                "test must model the real Redux-only shape: mirror transcript is empty"
            );
        }

        let mut state = crate::chat::state::ChatState::new(
            Arc::from("p"),
            Arc::from("m"),
            tokio_util::sync::CancellationToken::new(),
        );
        state.session.title = "redux chat".to_string();
        state.ui.conversation_lines.push(tui::ConversationLine::User {
            content: "redux-only user".to_string(),
        });
        state.ui.conversation_lines.push(tui::ConversationLine::ToolResult {
            tool_name: "shell".to_string(),
            args_preview: "echo ok".to_string(),
            args_full: "echo ok".to_string(),
            status: tui::ToolStatus::Done,
            elapsed_ms: Some(12),
            result: Some("ok".to_string()),
            folded: false,
        });
        let snapshot = Arc::new(state.build_ui_snapshot(1));
        let (_snapshot_tx, snapshot_rx) = tokio::sync::watch::channel(snapshot);

        let (dispatcher, mut action_rx) = crate::chat::dispatcher::ChatDispatcher::new();

        open_transcript_view(&mirror, &dispatcher, None, Some(&snapshot_rx));

        let view = {
            let guard = mirror.lock();
            guard.active_session_view.clone().expect("transcript view")
        };
        assert_eq!(view.title, "redux chat");
        assert!(
            view.lines.iter().any(|line| line.contains("redux-only user")),
            "transcript must render Redux-only user line: {:?}",
            view.lines
        );
        assert!(
            view.lines.iter().any(|line| line.contains("tool shell")),
            "transcript must render Redux-only tool output line: {:?}",
            view.lines
        );
        assert!(
            view.lines.iter().all(|line| !line.contains("(transcript is empty)")),
            "snapshot-backed transcript must not fall back to empty mirror: {:?}",
            view.lines
        );

        match action_rx.try_recv().expect("focus action") {
            crate::chat::action::Action::SessionFocusChanged { focus } => {
                assert_eq!(focus, crate::chat::sessions::FocusTarget::Transcript);
            }
            other => panic!("expected SessionFocusChanged, got {other:?}"),
        }
        match action_rx.try_recv().expect("active view action") {
            crate::chat::action::Action::ActiveSessionViewUpdated { view } => {
                let view = view.expect("transcript view action");
                assert!(
                    view.lines.iter().any(|line| line.contains("redux-only user")),
                    "dispatched view must carry Redux-only transcript: {:?}",
                    view.lines
                );
            }
            other => panic!("expected ActiveSessionViewUpdated, got {other:?}"),
        }
    }

    #[test]
    fn close_transcript_view_returns_main_focus_and_clears_view() {
        let mirror = Arc::new(parking_lot::Mutex::new(tui::TuiState::new("p", "m")));
        {
            let mut guard = mirror.lock();
            guard.focus = crate::chat::sessions::FocusTarget::Transcript;
            guard.active_session_view = Some(tui::build_transcript_view("", &[], 0));
        }
        let (dispatcher, mut action_rx) = crate::chat::dispatcher::ChatDispatcher::new();
        let (redraw_tx, mut redraw_rx) = mpsc::channel(1);

        close_transcript_view(&mirror, &dispatcher, &redraw_tx);

        {
            let guard = mirror.lock();
            assert_eq!(guard.focus, crate::chat::sessions::FocusTarget::Main);
            assert!(guard.active_session_view.is_none());
        }
        match action_rx.try_recv().expect("focus action") {
            crate::chat::action::Action::SessionFocusChanged { focus } => {
                assert_eq!(focus, crate::chat::sessions::FocusTarget::Main);
            }
            other => panic!("expected SessionFocusChanged, got {other:?}"),
        }
        match action_rx.try_recv().expect("active view action") {
            crate::chat::action::Action::ActiveSessionViewUpdated { view } => {
                assert!(view.is_none());
            }
            other => panic!("expected ActiveSessionViewUpdated, got {other:?}"),
        }
        assert!(redraw_rx.try_recv().is_ok(), "close should request redraw");
    }
}

#[cfg(test)]
mod v4_reload_recap_tests {
    use super::format_reloaded_background_sessions;
    use crate::chat::sessions::PersistedSessionSummary;
    use crate::llm::route_decision::{MeteredTokenUsageRecord, TokenUsageSource};

    fn summary(id: &str, status: &str, title: &str, body: &str) -> PersistedSessionSummary {
        PersistedSessionSummary {
            id: id.to_string(),
            seq: 2,
            kind: "agent".to_string(),
            origin: "user".to_string(),
            status: status.to_string(),
            title: title.to_string(),
            summary: body.to_string(),
            token_usage_records: Vec::new(),
            created_at: chrono::Utc::now(),
        }
    }

    fn usage_record() -> MeteredTokenUsageRecord {
        MeteredTokenUsageRecord {
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            source: TokenUsageSource::Reported,
            cost_usd: Some(0.0004),
        }
    }

    #[test]
    fn recap_has_not_resumed_header_and_one_line_per_session() {
        let sessions = vec![
            summary("a", "completed", "build report", "report ready"),
            summary("b", "interrupted", "long crawl", ""),
        ];
        let out = format_reloaded_background_sessions(&sessions);
        assert!(out.contains("not resumed"), "header must signal nothing was revived");
        assert!(out.contains("completed"));
        assert!(out.contains("build report"));
        assert!(out.contains("report ready"));
        // Interrupted (was-running) session shows as terminal, not running.
        assert!(out.contains("interrupted"));
        assert!(!out.contains("running"));
        // One header line + two session lines.
        assert_eq!(out.lines().count(), 3);
    }

    #[test]
    fn recap_omits_empty_summary_body() {
        let sessions = vec![summary("a", "cancelled", "task", "")];
        let out = format_reloaded_background_sessions(&sessions);
        // No trailing ": " when there is no summary body.
        assert!(out.contains("cancelled — task"));
        assert!(!out.contains("task: "));
        assert!(!out.contains("0 tok"), "missing usage must not render as zero: {out}");
    }

    #[test]
    fn recap_displays_persisted_usage_when_present() {
        let mut session = summary("a", "completed", "metered task", "done");
        session.token_usage_records = vec![usage_record()];

        let out = format_reloaded_background_sessions(&[session]);

        assert!(
            out.contains("completed 150 tok | $0.0004 — metered task: done"),
            "{out}"
        );
    }

    #[test]
    fn recap_tags_model_origin_and_leaves_user_untagged() {
        // Bug-V5-2: the persisted origin must be visible on reload so the
        // operator can tell which sessions the model started for itself.
        let mut user = summary("a", "completed", "user task", "done");
        user.origin = "user".to_string();
        user.seq = 1;
        let mut model = summary("b", "completed", "model task", "done");
        model.origin = "model".to_string();
        model.seq = 2;
        let out = format_reloaded_background_sessions(&[user, model]);
        assert!(
            out.contains("#1 completed — user task"),
            "user line stays untagged: {out}"
        );
        assert!(
            out.contains("#2 [model] completed — model task"),
            "model line is tagged: {out}"
        );
    }
}

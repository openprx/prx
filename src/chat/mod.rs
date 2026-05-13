//! `prx chat` entry point — rich terminal interactive chat.
//!
//! Wires up the full agent pipeline (memory, tools, providers, security, hooks,
//! observability) and uses [`TerminalChannel`] for streaming I/O through the
//! event-driven UI Actor.
// Chat module: println!/eprintln! are intentional user-facing output (banners, status, errors).
#![allow(clippy::print_stdout, clippy::print_stderr)]

pub mod commands;
pub mod sanitize;
pub mod session;
pub mod terminal_proto;

#[cfg(feature = "terminal-tui")]
pub mod renderer;
#[cfg(feature = "terminal-tui")]
pub mod tui;

use crate::agent::loop_::{
    ScopeContext, ToolCallNotification, ToolConcurrencyGovernanceConfig, build_context, build_runtime_system_prompt,
    increment_recalled_useful_counts, is_tool_loop_cancelled, run_tool_call_loop, select_prompt_skills,
};
use crate::approval::ApprovalManager;
use crate::channels::traits::extract_outgoing_media;
use crate::channels::{
    Channel, SendMessage, TerminalChannel, extract_tool_context_summary, is_context_window_overflow_error,
    sanitize_channel_response,
};
use crate::chat::terminal_proto::{DraftVersionCounter, DraftVersionTracker};
use crate::config::Config;
use crate::hooks::{HookEvent, HookManager, payload_error};
use crate::memory::{self, Memory, MemoryCategory};
use crate::observability::{self, Observer, ObserverEvent};
use crate::providers::{self, ChatMessage, Provider};
use crate::runtime;
use crate::security::PolicyPipeline;
use crate::security::SecurityPolicy;
use crate::tools;
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Minimum user-message length for auto-save to memory.
const AUTOSAVE_MIN_MESSAGE_CHARS: usize = 10;

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

/// Capacity for the streaming delta (partial response) mpsc channel.
const DELTA_CHANNEL_CAPACITY: usize = 64;

/// Capacity for the tool-call notification mpsc channel (visual feedback).
const TOOL_EVENT_CHANNEL_CAPACITY: usize = 32;

/// Minimum base timeout (seconds) for per-turn timeout budget.
const TIMEOUT_MIN_BASE_SECS: u64 = 30;

/// Maximum multiplier applied to the base timeout (caps iterations-based scaling).
const TIMEOUT_MAX_SCALE_FACTOR: u64 = 4;

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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn autosave_memory_key(prefix: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{prefix}:{ts}")
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

// ── P3-2: TerminalGuard RAII + strengthened panic hook ──────────────────────

/// Best-effort terminal restoration used by both [`TerminalGuard`] (on Drop /
/// manual `leave`) and the chat panic hook installed via
/// [`install_chat_panic_hook`].
///
/// Sequence (reverse of entry):
///   1. Show cursor
///   2. `LeaveAlternateScreen`
///   3. `disable_raw_mode`
///
/// Every step swallows its error: by the time this runs we are already on the
/// cleanup path (Drop or panic unwind) and there is no caller left to surface
/// the failure to. Errors are silently dropped — logging is intentionally
/// avoided to keep this callable from a panic hook without re-entering the
/// tracing machinery.
fn restore_terminal_state() {
    // 1. Show cursor + leave alternate screen, written to stdout (matches
    //    where we entered it). stderr is also valid but we keep parity with
    //    `TerminalGuard::enter` which writes to stdout.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::cursor::Show,
        crossterm::terminal::LeaveAlternateScreen,
    );
    // 2. Disable raw mode last so any escape sequences emitted above are
    //    interpreted by the terminal as expected.
    let _ = crossterm::terminal::disable_raw_mode();
}

/// RAII guard for the chat TUI terminal state.
///
/// Owns the entry side-effects (`enable_raw_mode` + `EnterAlternateScreen` +
/// hide cursor) and guarantees they are reversed exactly once on Drop —
/// whether by normal return, `?` early-exit, or panic unwinding. The
/// strengthened panic hook in [`install_chat_panic_hook`] provides
/// defence-in-depth for non-unwind aborts and for panics that happen before a
/// guard exists.
///
/// `enter()` is *transactional*: if either step (raw mode → alternate screen)
/// fails, any already-applied step is rolled back before returning `Err`, so a
/// failed enter never leaves the terminal in a half-modified state.
///
/// Note: this type is defined ahead of the P3-3 ratatui draw loop wiring. It
/// is intentionally **not** invoked from [`run`] yet — see the inline comment
/// in `run` for the integration point.
pub struct TerminalGuard {
    /// True while raw mode is currently enabled by *this* guard.
    raw_mode_active: std::sync::atomic::AtomicBool,
    /// True while we are currently inside the alternate screen + hidden
    /// cursor state owned by *this* guard.
    alt_screen_active: std::sync::atomic::AtomicBool,
}

impl TerminalGuard {
    /// Enter raw mode + alternate screen + hide cursor.
    ///
    /// Transactional: on partial failure (e.g. raw mode succeeded but
    /// alternate screen failed) the partially-applied state is rolled back
    /// before returning `Err`, so callers never need to clean up after a
    /// failed `enter`.
    pub fn enter() -> Result<Self> {
        use std::sync::atomic::AtomicBool;

        // Step 1: raw mode.
        crossterm::terminal::enable_raw_mode()
            .map_err(|e| anyhow::anyhow!("failed to enable raw mode for chat TUI: {e}"))?;

        // Step 2: alternate screen + hide cursor. If this fails, roll back
        // step 1 before propagating the error.
        if let Err(e) = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::cursor::Hide,
        ) {
            // Best-effort rollback — already on error path, ignore failure.
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(anyhow::anyhow!("failed to enter alternate screen for chat TUI: {e}"));
        }

        Ok(Self {
            raw_mode_active: AtomicBool::new(true),
            alt_screen_active: AtomicBool::new(true),
        })
    }

    /// Manual early teardown (e.g. before spawning a child process that
    /// needs a clean terminal). Idempotent — subsequent calls (including the
    /// Drop hook) are a no-op.
    ///
    /// Uses two CAS operations so concurrent `leave()` / `drop()` from
    /// different threads is safe: only the first caller to flip the flag
    /// actually issues the crossterm calls.
    pub fn leave(&self) {
        use std::sync::atomic::Ordering;

        // Order mirrors the reverse of entry: cursor + alt screen first,
        // raw mode last.
        if self
            .alt_screen_active
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::cursor::Show,
                crossterm::terminal::LeaveAlternateScreen,
            );
        }
        if self
            .raw_mode_active
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.leave();
    }
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

    // ── Wire up subsystems (same as agent::run) ──────────────────
    let base_observer = observability::create_observer(&config.observability);
    let observer: Arc<dyn Observer> = Arc::from(base_observer);
    let hooks = HookManager::new(config.workspace_dir.clone());
    let runtime: Arc<dyn runtime::RuntimeAdapter> = Arc::from(runtime::create_runtime(&config.runtime)?);
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));

    // ── Memory ───────────────────────────────────────────────────
    let mem: Arc<dyn Memory> = Arc::from(memory::create_memory_with_storage_and_routes_with_acl(
        &config.memory,
        &config.embedding_routes,
        Some(&config.storage.provider.config),
        &config.workspace_dir,
        config.api_key.as_deref(),
        &config.identity_bindings,
        &config.user_policies,
    )?);
    info!(backend = mem.name(), "Memory initialized");

    // ── List sessions (early return) ─────────────────────────────
    if list_sessions {
        return list_saved_sessions(mem.as_ref()).await;
    }

    // ── Tools ────────────────────────────────────────────────────
    let (composio_key, composio_entity_id) = if config.composio.enabled {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    };
    let tools_registry = tools::all_tools_with_runtime(
        Arc::new(config.clone()),
        &security,
        runtime,
        mem.clone(),
        composio_key,
        composio_entity_id,
        &config.browser,
        &config.http_request,
        &config.workspace_dir,
        &config.agents,
        config.api_key.as_deref(),
        &config,
    );

    // ── Resolve provider ─────────────────────────────────────────
    let provider_name = provider_override
        .as_deref()
        .or(config.default_provider.as_deref())
        .unwrap_or("openrouter");

    let model_name = model_override
        .as_deref()
        .or(config.default_model.as_deref())
        .unwrap_or("anthropic/claude-sonnet-4");

    let provider_runtime_options = providers::ProviderRuntimeOptions {
        auth_profile_override: None,
        openprx_dir: config.config_path.parent().map(std::path::PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        codex_auth_json_path: Some(config.auth.codex_auth_json_path.clone()),
        codex_auth_json_auto_import: config.auth.codex_auth_json_auto_import,
        reasoning_enabled: config.runtime.reasoning_enabled,
        codex_stream_idle_timeout_secs: config.runtime.codex_stream_idle_timeout_secs,
        codex_reasoning_effort: config.runtime.codex_reasoning_effort.clone(),
    };

    let provider: Box<dyn Provider> = providers::create_routed_provider_with_options(
        provider_name,
        config.api_key.as_deref(),
        config.api_url.as_deref(),
        &config.reliability,
        &config.model_routes,
        model_name,
        &provider_runtime_options,
    )?;

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
    ];
    let native_tools = provider.supports_native_tools();

    // ── Approval manager ─────────────────────────────────────────
    let approval_manager = ApprovalManager::from_config(&config.autonomy);

    // ── Create TerminalChannel (Arc-wrapped for sharing with streaming tasks) ──
    let terminal: Arc<TerminalChannel> = Arc::new(TerminalChannel::new(plain_mode));

    // ── Session: resume or create new ───────────────────────────
    let mut chat_session = match session_id.as_deref() {
        Some("last") => match load_latest_session(mem.as_ref()).await {
            Some(s) => {
                info!(id = %s.id, title = %s.title, turns = s.turn_count(), "Resumed session");
                s
            }
            None => {
                info!("No previous session found, starting new");
                session::ChatSession::new(provider_name, model_name)
            }
        },
        Some(id) => match load_session_by_id(mem.as_ref(), id).await {
            Some(s) => {
                info!(id = %s.id, title = %s.title, turns = s.turn_count(), "Resumed session");
                s
            }
            None => {
                eprintln!("Session '{id}' not found, starting new session.");
                session::ChatSession::new(provider_name, model_name)
            }
        },
        None => session::ChatSession::new(provider_name, model_name),
    };

    // ── Print banner ─────────────────────────────────────────────
    if chat_session.turn_count() > 0 {
        println!(
            "PRX Chat — {provider_name}/{model_name} — session: {} ({} turns)",
            chat_session.title,
            chat_session.turn_count()
        );
    } else {
        println!("PRX Chat — {provider_name}/{model_name}");
    }
    println!("Type /help for commands, /quit to exit.\n");

    // ── Conversation history ─────────────────────────────────────
    let mut history = if config.skill_rag.enabled {
        Vec::new()
    } else {
        vec![ChatMessage::system(build_runtime_system_prompt(
            &config,
            model_name,
            &tool_descs,
            &skills,
            native_tools,
            &tools_registry,
        ))]
    };

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
    let chat_mirror: Arc<parking_lot::Mutex<tui::TuiState>> =
        Arc::new(parking_lot::Mutex::new(tui::TuiState::new(provider_name, model_name)));

    // ── Input channel ────────────────────────────────────────────
    let (input_tx, mut input_rx) = mpsc::channel(INPUT_CHANNEL_CAPACITY);

    // ── Graceful shutdown signal ─────────────────────────────────
    // Instead of std::process::exit(), all signal handlers use this token to
    // break the main loop gracefully, allowing final session save + teardown.
    // Created up here (earlier than before) so the TUI input task can also
    // observe shutdown and exit its blocking poll cleanly.
    let shutdown = CancellationToken::new();

    // ── Ctrl+C shared state ─────────────────────────────────────
    // Tracks the timestamp (ms) of the last Ctrl+C press for double-press detection.
    // Lifted above the input loop so the TUI dispatcher can fold its own
    // Ctrl+C presses into the same double-press → exit semantics.
    let last_ctrlc_ms = Arc::new(AtomicU64::new(0));
    // The active cancellation token for the current generation turn (if any).
    let active_cancel: Arc<parking_lot::Mutex<Option<CancellationToken>>> = Arc::new(parking_lot::Mutex::new(None));

    // Spawn the appropriate input loop:
    //   - feature `terminal-tui` + TTY stdin + `PRX_TUI=1` → ratatui/crossterm
    //     KeyEvent loop driving `dispatch_global_key` against the shared
    //     `chat_mirror`, plus a `spawn_render_task` that owns the
    //     `ratatui::Terminal` and redraws on demand.
    //   - otherwise → legacy reedline + BufRead fallback via TerminalChannel.
    //
    // `_terminal_guard` is bound to this function's stack so its Drop runs at
    // chat::run exit (panic-safe via `install_chat_panic_hook` above). The
    // legacy path leaves `_terminal_guard = None`, so no entry side-effects
    // are applied.
    #[cfg(feature = "terminal-tui")]
    let _terminal_guard: Option<TerminalGuard> = {
        use std::io::IsTerminal as _;
        let tui_enabled = std::env::var("PRX_TUI").as_deref() == Ok("1") && std::io::stdin().is_terminal();
        if tui_enabled {
            // Order matters: `TerminalGuard::enter()` flips raw mode + alt
            // screen FIRST, then we create the ratatui `Terminal` (which the
            // render task takes ownership of via `spawn_blocking`). On enter
            // failure we fall back to the legacy reedline path so the user
            // is never left without a prompt.
            match TerminalGuard::enter() {
                Ok(guard) => {
                    // mpsc capacity = 1 + try_send is the coalesce idiom: many
                    // producers calling `try_send(())` while a draw is in
                    // flight all collapse into a single deferred redraw, so
                    // the render task never falls behind.
                    let (redraw_tx, redraw_rx) = mpsc::channel::<()>(1);
                    let _render_handle = spawn_render_task(Arc::clone(&chat_mirror), redraw_rx, shutdown.clone());
                    spawn_redraw_tick_task(redraw_tx.clone(), shutdown.clone());
                    spawn_tui_input_task(
                        input_tx,
                        Arc::clone(&chat_mirror),
                        shutdown.clone(),
                        Arc::clone(&last_ctrlc_ms),
                        Arc::clone(&active_cancel),
                        redraw_tx,
                    );
                    Some(guard)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "TerminalGuard::enter failed; falling back to reedline input");
                    let terminal_for_listen = TerminalChannel::new(plain_mode);
                    tokio::spawn(async move {
                        if let Err(e) = terminal_for_listen.listen(input_tx).await {
                            tracing::error!("Terminal input loop error: {e}");
                        }
                    });
                    None
                }
            }
        } else {
            // Default path (TTY without PRX_TUI=1, or pipe/heredoc) — keep the
            // legacy reedline + BufRead fallback via TerminalChannel.
            let terminal_for_listen = TerminalChannel::new(plain_mode);
            tokio::spawn(async move {
                if let Err(e) = terminal_for_listen.listen(input_tx).await {
                    tracing::error!("Terminal input loop error: {e}");
                }
            });
            None
        }
    };
    #[cfg(not(feature = "terminal-tui"))]
    {
        let terminal_for_listen = TerminalChannel::new(plain_mode);
        tokio::spawn(async move {
            if let Err(e) = terminal_for_listen.listen(input_tx).await {
                tracing::error!("Terminal input loop error: {e}");
            }
        });
    }

    // Persistent Ctrl+C handler: runs for the entire chat session.
    // - If a generation is active: cancel it (first press) or exit (double press).
    // - If idle (no generation): exit on double press.
    {
        let last_ctrlc = Arc::clone(&last_ctrlc_ms);
        let cancel_ref = Arc::clone(&active_cancel);
        let shutdown_signal = shutdown.clone();
        tokio::spawn(async move {
            loop {
                if tokio::signal::ctrl_c().await.is_err() {
                    break;
                }
                let now = now_ms();
                let prev = last_ctrlc.swap(now, Ordering::Relaxed);

                if now.saturating_sub(prev) < DOUBLE_CTRLC_WINDOW_MS {
                    // Double Ctrl+C → graceful shutdown
                    eprintln!("\nExiting...");
                    shutdown_signal.cancel();
                    break;
                }

                // Single Ctrl+C → cancel active generation if any
                if let Some(token) = cancel_ref.lock().as_ref() {
                    token.cancel();
                }
            }
        });
    }

    // SIGTERM handler: signal graceful shutdown.
    #[cfg(unix)]
    {
        let sigterm_result = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        match sigterm_result {
            Ok(mut sigterm) => {
                let shutdown_signal = shutdown.clone();
                tokio::spawn(async move {
                    sigterm.recv().await;
                    shutdown_signal.cancel();
                });
            }
            Err(e) => {
                tracing::warn!("Failed to register SIGTERM handler: {e}");
            }
        }
    }

    // ── Main message loop ────────────────────────────────────────
    while let Some(msg) = tokio::select! {
        msg = input_rx.recv() => msg,
        _ = shutdown.cancelled() => None,
    } {
        let user_input = msg.content.clone();

        // Handle /quit and /exit immediately
        if matches!(user_input.as_str(), "/quit" | "/exit") {
            break;
        }

        // Handle /clear separately (needs mutable history)
        if matches!(user_input.as_str(), "/clear" | "/new") {
            println!("Clearing conversation (core memories preserved)...");
            history.clear();
            if !config.skill_rag.enabled {
                history.push(ChatMessage::system(build_runtime_system_prompt(
                    &config,
                    model_name,
                    &tool_descs,
                    &skills,
                    native_tools,
                    &tools_registry,
                )));
            }
            let cleared = commands::handle_clear(mem.as_ref(), Some(&chat_session.id)).await;
            if cleared > 0 {
                println!("Conversation cleared ({cleared} memory entries removed).\n");
            } else {
                println!("Conversation cleared.\n");
            }
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
                commands::CommandResult::Quit => break,
                commands::CommandResult::SetMode(mode) => {
                    chat_session.set_mode(mode);
                    match mode {
                        commands::ChatMode::Plan => println!(
                            "✓ Switched to plan mode (read-only tools only — write/shell/git_commit will be simulated)\n"
                        ),
                        commands::ChatMode::Edit => {
                            println!("✓ Switched to edit mode (default — write tools enabled)\n");
                        }
                        commands::ChatMode::Auto => {
                            println!("✓ Switched to auto mode (all tools, no approval prompts)\n");
                        }
                    }
                    continue;
                }
                commands::CommandResult::NotACommand => {}
            }
        }

        // Auto-save user message to memory
        if config.memory.auto_save
            && user_input.chars().count() >= AUTOSAVE_MIN_MESSAGE_CHARS
            && memory::should_autosave_content(&user_input)
        {
            let user_key = autosave_memory_key("user_msg");
            let _ = mem
                .store(&user_key, &user_input, MemoryCategory::Conversation, None)
                .await;
        }

        // Inject memory context
        let mem_context = build_context(mem.as_ref(), &user_input, config.memory.min_relevance_score).await;
        let context = mem_context.preamble.clone();
        let enriched = if context.is_empty() {
            user_input.clone()
        } else {
            format!("{context}{user_input}")
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
        if history.is_empty() {
            history.push(ChatMessage::system(system_prompt));
        } else if let Some(first) = history.first_mut() {
            *first = ChatMessage::system(system_prompt);
        }
        history.push(ChatMessage::user(&enriched));

        // ── Set active recipient/channel on tools (for proactive messaging) ──
        for tool in &tools_registry {
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

        // Spawn background task: accumulate deltas → channel.update_draft()
        // Follows the exact same pattern as process_channel_message in channels/mod.rs.
        //
        // P1-6 — Monotonic draft version protocol. Even though the delta mpsc itself
        // is FIFO, the accumulated text is forwarded over additional channels inside
        // the channel implementation (TerminalChannel → UiActor) where a late or
        // duplicated message could otherwise visually rewind rendered text. The
        // sender-side counter stamps each accumulated snapshot with a strictly
        // monotonic `u64`; the receiver-side tracker drops any non-increasing
        // arrival before issuing the `update_draft` call.
        let draft_updater = if let Some(ref d_id) = draft_id {
            let channel: Arc<TerminalChannel> = Arc::clone(&terminal);
            let reply_target = "user".to_string();
            let draft_id_owned = d_id.clone();
            let mut rx = delta_rx;
            let version_counter = Arc::new(DraftVersionCounter::new());
            let version_tracker = Arc::new(DraftVersionTracker::new());
            Some(tokio::spawn(async move {
                let mut accumulated = String::new();
                while let Some(delta) = rx.recv().await {
                    accumulated.push_str(&delta);
                    let version = version_counter.next();
                    if !version_tracker.accept(&draft_id_owned, version) {
                        // Stale snapshot — drop to prevent visual rewind. In practice
                        // unreachable here because counter is single-task monotonic,
                        // but the guard is cheap and defends against future
                        // re-architecting (e.g. parallel accumulator tasks).
                        tracing::trace!(
                            draft_id = %draft_id_owned,
                            version,
                            "dropping stale draft delta"
                        );
                        continue;
                    }
                    if let Err(e) = channel.update_draft(&reply_target, &draft_id_owned, &accumulated).await {
                        tracing::debug!("Draft update failed: {e}");
                    }
                }
                // Stream ended — release per-draft version state.
                version_tracker.clear(&draft_id_owned);
            }))
        } else {
            // No draft — consume delta_rx so the sender doesn't block
            let mut rx = delta_rx;
            Some(tokio::spawn(async move { while rx.recv().await.is_some() {} }))
        };

        // Register this turn's cancellation token so the Ctrl+C handler can cancel it.
        *active_cancel.lock() = Some(cancellation.clone());

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
        #[cfg(feature = "terminal-tui")]
        let tui_mirror_for_tools = Arc::clone(&tui_mirror);
        let tool_event_forwarder = tokio::spawn(async move {
            while let Some(notif) = tool_event_rx.recv().await {
                match notif {
                    ToolCallNotification::Started { name, args_summary } => {
                        #[cfg(feature = "terminal-tui")]
                        tui_mirror_for_tools
                            .lock()
                            .push_tool_result_started(&name, &args_summary);
                        terminal_for_tools.notify_tool_started(&name, &args_summary).await;
                    }
                    ToolCallNotification::Finished {
                        name,
                        success,
                        duration_ms,
                    } => {
                        #[cfg(feature = "terminal-tui")]
                        tui_mirror_for_tools
                            .lock()
                            .mark_last_tool_result_finished(&name, success, duration_ms, None);
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

        // ── Policy Pipeline for tool access control ──────────────
        let policy_pipeline = PolicyPipeline::from_config(&config);
        let scope_ctx = ScopeContext {
            policy: &security,
            sender: "user",
            channel: "terminal",
            chat_type: "private",
            chat_id: "terminal:user",
            policy_pipeline: Some(&policy_pipeline),
        };

        // ── Timeout budget ───────────────────────────────────────
        let timeout_budget = {
            let base = config.channels_config.message_timeout_secs.max(TIMEOUT_MIN_BASE_SECS);
            let scale = (config.agent.max_tool_iterations.max(1) as u64).min(TIMEOUT_MAX_SCALE_FACTOR);
            Duration::from_secs(base.saturating_mul(scale))
        };

        // ── Retry loop (context overflow recovery + timeout retry) ──
        //
        // Mirrors the retry strategy in channels/mod.rs process_channel_message:
        //  - Context overflow: compact history, retry up to MAX_CONTEXT_OVERFLOW_RETRIES
        //  - Timeout: sleep 2s, retry once
        let mut context_overflow_retries = 0usize;
        let mut timeout_retries = 0usize;
        let mut history_len_before_tools;

        enum TurnOutcome {
            Success(String),
            Failed,
        }

        let turn_outcome = loop {
            history_len_before_tools = history.len();

            let result = tokio::time::timeout(
                timeout_budget,
                run_tool_call_loop(
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
                        rollback_timeout_rate_threshold: config.agent.concurrency_rollback_timeout_rate_threshold,
                        rollback_cancel_rate_threshold: config.agent.concurrency_rollback_cancel_rate_threshold,
                        rollback_error_rate_threshold: config.agent.concurrency_rollback_error_rate_threshold,
                    },
                    Some(&config.agent.compaction),
                    Some(cancellation.clone()),
                    Some(delta_tx.clone()),
                    Some(&scope_ctx),
                    Some(tool_event_tx.clone()),
                    Some(&config.tool_tiering),
                    chat_session.mode,
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
                    hooks
                        .emit(HookEvent::Error, payload_error("chat-turn", "timeout"))
                        .await;
                    break TurnOutcome::Failed;
                }
                // ── Success ───────────────────────────────────────
                Ok(Ok(resp)) => break TurnOutcome::Success(resp),
                // ── Cancelled (Ctrl+C) ────────────────────────────
                Ok(Err(ref e)) if is_tool_loop_cancelled(e) || cancellation.is_cancelled() => {
                    if let Some(ref d_id) = draft_id {
                        let _ = terminal.cancel_draft("user", d_id).await;
                    }
                    break TurnOutcome::Failed;
                }
                // ── Context window overflow → compact + retry ─────
                Ok(Err(ref e)) if is_context_window_overflow_error(e) => {
                    compact_chat_history(&mut history);
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
                    hooks
                        .emit(
                            HookEvent::Error,
                            payload_error("chat-turn", "context-overflow-exhausted"),
                        )
                        .await;
                    break TurnOutcome::Failed;
                }
                // ── Other errors ──────────────────────────────────
                Ok(Err(e)) => {
                    if let Some(ref d_id) = draft_id {
                        let _ = terminal.cancel_draft("user", d_id).await;
                    }
                    eprintln!("\nError: {e}\n");
                    hooks
                        .emit(HookEvent::Error, payload_error("chat-turn", &e.to_string()))
                        .await;
                    break TurnOutcome::Failed;
                }
            }
        };

        // ── Finalize streaming ────────────────────────────────────
        // Deregister this turn's cancellation token
        *active_cancel.lock() = None;

        // Drop our channel senders so background tasks receive channel close
        drop(delta_tx);
        drop(tool_event_tx);
        if let Some(handle) = draft_updater {
            let _ = handle.await;
        }
        let _ = tool_event_forwarder.await;

        // If the turn failed (timeout/cancel/error), skip response processing
        let response = match turn_outcome {
            TurnOutcome::Success(resp) => resp,
            TurnOutcome::Failed => continue,
        };

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
                tui_mirror.lock().push_reasoning(&aggregated);
            }
        }

        increment_recalled_useful_counts(mem.as_ref(), &mem_context.ids).await;

        // ── Sanitize response: strip tool-call XML/JSON artifacts ──
        let response = sanitize_channel_response(&response, &tools_registry);

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
        // Sanitize content before persistence (redact secrets, truncate large outputs)
        let sanitized_input = sanitize::sanitize_for_persistence(&user_input);
        let sanitized_response = sanitize::sanitize_for_persistence(&response);
        chat_session.add_user_turn(&sanitized_input);
        chat_session.add_assistant_turn(&sanitized_response, Vec::new());
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
    }

    // ── Graceful teardown: restore terminal state ────────────────
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(
        std::io::stderr(),
        crossterm::cursor::Show,
        crossterm::terminal::LeaveAlternateScreen
    );

    // Final session save before exit
    if let Err(e) = save_session(mem.as_ref(), &chat_session).await {
        tracing::warn!("Failed to persist session on exit: {e}");
    }

    info!("Chat session ended");
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
#[cfg(feature = "terminal-tui")]
fn spawn_render_task(
    mirror: Arc<parking_lot::Mutex<tui::TuiState>>,
    mut redraw_rx: mpsc::Receiver<()>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let stdout = std::io::stdout();
        let backend = ratatui::backend::CrosstermBackend::new(stdout);
        let mut terminal = match ratatui::Terminal::new(backend) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("ratatui Terminal::new failed: {e}");
                return;
            }
        };

        // Paint once at startup so the layout appears even before the user
        // hits a key. Skipping a frame on error is fine — the next redraw
        // signal will retry.
        if let Err(e) = terminal.draw(|f| tui::render(f, &mut mirror.lock())) {
            tracing::warn!(error = %e, "initial TUI draw failed");
        }

        loop {
            if shutdown.is_cancelled() {
                return;
            }
            // Block until the next redraw signal (or all senders are dropped,
            // which happens on shutdown). `None` from `blocking_recv` means
            // every clone of `redraw_tx` has been dropped — that is the
            // normal exit path on `chat::run` teardown.
            match redraw_rx.blocking_recv() {
                Some(()) => {}
                None => return,
            }
            // Drain any additional pending wakeups so a burst of producer
            // notifications turns into a single draw call (coalesce).
            while redraw_rx.try_recv().is_ok() {}

            if shutdown.is_cancelled() {
                return;
            }
            if let Err(e) = terminal.draw(|f| tui::render(f, &mut mirror.lock())) {
                tracing::warn!(error = %e, "TUI draw failed (will retry on next signal)");
            }
        }
    })
}

/// Spawn a 250 ms heartbeat that nudges the render task even when no input
/// or tool event arrived in that window. Useful for cursor blink, time-based
/// status updates, and (once P3-5 lands) idle stream redraws.
///
/// Cancels itself when `shutdown` fires. Send failures are silently ignored
/// because the receiver is allowed to disappear before this task does.
#[cfg(feature = "terminal-tui")]
fn spawn_redraw_tick_task(redraw_tx: mpsc::Sender<()>, shutdown: CancellationToken) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        // Skip the first immediate tick; the initial draw already painted.
        interval.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = interval.tick() => {
                    if redraw_tx.try_send(()).is_err() {
                        // Either the channel is full (coalesced — fine) or
                        // closed (render task gone — also fine, we will exit
                        // on the next shutdown tick).
                    }
                }
            }
        }
    });
}

// ── TUI input loop (P2-Integration) ──────────────────────────────────────

/// Spawn the crossterm `KeyEvent` input loop and route each event through
/// [`tui::dispatch_global_key`].
///
/// Lives in a dedicated `spawn_blocking` task because `crossterm::event::read`
/// blocks the calling thread. On every loop iteration we:
///   1. Poll with a short timeout so the loop can observe shutdown.
///   2. Read a single event.
///   3. Dispatch keys; submissions are forwarded over the shared
///      `mpsc::Sender<ChannelMessage>` (the same channel `TerminalChannel::listen`
///      would use, so the rest of `run()` is oblivious to which path produced
///      the message).
///   4. Poke the render task via `redraw_tx` so the visible state catches up
///      to the just-dispatched keystroke.
///
/// Raw mode + alternate screen are now owned by [`TerminalGuard`] (entered
/// in `run()` on the TUI path), so this function does NOT touch terminal
/// state — that avoids a race between the manual disable here and the
/// guard's Drop on `chat::run` exit.
#[cfg(feature = "terminal-tui")]
fn spawn_tui_input_task(
    input_tx: mpsc::Sender<crate::channels::traits::ChannelMessage>,
    mirror: Arc<parking_lot::Mutex<tui::TuiState>>,
    shutdown: CancellationToken,
    last_ctrlc_ms: Arc<AtomicU64>,
    active_cancel: Arc<parking_lot::Mutex<Option<CancellationToken>>>,
    redraw_tx: mpsc::Sender<()>,
) {
    tokio::task::spawn_blocking(move || {
        // Raw mode is owned by `TerminalGuard` (entered in `run()` on the TUI
        // path). We intentionally do NOT call `enable_raw_mode()` here —
        // doing so would flip the flag twice and the manual teardown at
        // `run()` exit would race the guard's Drop. The legacy fallback
        // path (no guard) never reaches this function.
        let result = run_tui_input_loop(
            &input_tx,
            &mirror,
            &shutdown,
            &last_ctrlc_ms,
            &active_cancel,
            &redraw_tx,
        );
        // Cleanup is owned by `TerminalGuard::leave()` — keep the panic hook
        // as defence in depth. If the loop returns an error, log it but do
        // NOT disable raw mode here (the guard handles it).
        if let Err(e) = result {
            tracing::error!("TUI input loop error: {e}");
        }
    });
}

/// Inner loop body for [`spawn_tui_input_task`].
///
/// Polls `crossterm::event` with a short timeout so it stays responsive to
/// the shutdown token, then routes every key press through
/// [`tui::dispatch_global_key`] and forwards submissions over the same
/// `ChannelMessage` channel the legacy reedline path uses. `Ctrl+C` folds
/// into the existing double-press handler by mutating the shared
/// `last_ctrlc_ms` + `active_cancel` state — keeping behaviour identical to
/// the `tokio::signal::ctrl_c()` branch that runs in parallel.
#[cfg(feature = "terminal-tui")]
fn run_tui_input_loop(
    input_tx: &mpsc::Sender<crate::channels::traits::ChannelMessage>,
    mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    shutdown: &CancellationToken,
    last_ctrlc_ms: &Arc<AtomicU64>,
    active_cancel: &Arc<parking_lot::Mutex<Option<CancellationToken>>>,
    redraw_tx: &mpsc::Sender<()>,
) -> Result<()> {
    use crate::channels::traits::ChannelMessage;
    use crossterm::event::{Event, KeyEventKind};

    let poll = Duration::from_millis(100);
    loop {
        if shutdown.is_cancelled() {
            return Ok(());
        }
        if !crossterm::event::poll(poll)? {
            continue;
        }
        let ev = crossterm::event::read()?;
        let Event::Key(key) = ev else { continue };
        // Skip key-release events: on terminals with KeyboardEnhancement
        // flags enabled (Kitty et al.), a single physical press fires both
        // Press and Release. Only Press / Repeat are authoritative input.
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        let dispatch = tui::dispatch_global_key(key, &mut mirror.lock());
        // Coalesce-style redraw signal: try_send into a cap=1 channel. If a
        // redraw is already pending, the send fails silently (Full) and the
        // pending one will pick up the new state. If the receiver is gone
        // (Closed) the render task has shut down — that is the normal exit
        // path, so we ignore the error too.
        let _ = redraw_tx.try_send(());
        match dispatch {
            tui::KeyDispatch::Submitted(text) => {
                let trimmed = text.trim().to_string();
                if trimmed.is_empty() {
                    continue;
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
                    mentioned_uuids: vec![],
                };
                if input_tx.blocking_send(msg).is_err() {
                    return Ok(());
                }
            }
            tui::KeyDispatch::Exit => {
                // Ctrl+D on empty buffer → graceful shutdown of the whole chat.
                shutdown.cancel();
                return Ok(());
            }
            tui::KeyDispatch::InterruptTurn => {
                // Raw mode swallows the kernel-delivered SIGINT, so we replicate
                // the persistent ctrl_c() handler's logic directly here:
                //   * Two presses within DOUBLE_CTRLC_WINDOW_MS → exit.
                //   * Otherwise cancel the in-flight turn (if any).
                let now = now_ms();
                let prev = last_ctrlc_ms.swap(now, Ordering::Relaxed);
                if now.saturating_sub(prev) < DOUBLE_CTRLC_WINDOW_MS {
                    shutdown.cancel();
                    return Ok(());
                }
                if let Some(token) = active_cancel.lock().as_ref() {
                    token.cancel();
                }
            }
            tui::KeyDispatch::Scroll(dir) => {
                let mut guard = mirror.lock();
                match dir {
                    tui::ScrollDir::Up => guard.scroll_up(3),
                    tui::ScrollDir::Down => guard.scroll_down(3),
                }
            }
            tui::KeyDispatch::Cancelled | tui::KeyDispatch::Consumed => {}
        }
    }
}

// ── Session persistence helpers ──────────────────────────────────────────

/// Save a session to the Memory backend.
async fn save_session(mem: &dyn Memory, session: &session::ChatSession) -> Result<()> {
    let json = session.to_json().map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
    mem.store(&session.memory_key(), &json, MemoryCategory::Conversation, None)
        .await
        .map_err(|e| anyhow::anyhow!("store: {e}"))?;
    Ok(())
}

/// Load a session by ID (exact key lookup, not similarity search).
async fn load_session_by_id(mem: &dyn Memory, id: &str) -> Option<session::ChatSession> {
    let key = format!("{}:{}", session::SESSION_MEMORY_PREFIX, id);
    let entry = mem.get(&key).await.ok()??;
    session::ChatSession::from_json(&entry.content).ok()
}

/// Load the most recent session.
async fn load_latest_session(mem: &dyn Memory) -> Option<session::ChatSession> {
    let entries = mem.list(Some(&MemoryCategory::Conversation), None).await.ok()?;
    // Find entries with the session prefix, parse and sort by updated_at
    let mut sessions: Vec<session::ChatSession> = entries
        .iter()
        .filter(|e| e.key.starts_with(session::SESSION_MEMORY_PREFIX))
        .filter_map(|e| session::ChatSession::from_json(&e.content).ok())
        .collect();
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions.into_iter().next()
}

/// List all saved sessions.
async fn list_saved_sessions(mem: &dyn Memory) -> Result<()> {
    let entries = mem
        .list(Some(&MemoryCategory::Conversation), None)
        .await
        .unwrap_or_default();
    let mut sessions: Vec<session::ChatSession> = entries
        .iter()
        .filter(|e| e.key.starts_with(session::SESSION_MEMORY_PREFIX))
        .filter_map(|e| session::ChatSession::from_json(&e.content).ok())
        .collect();
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    if sessions.is_empty() {
        println!("No saved sessions.");
        return Ok(());
    }

    println!("Saved sessions:\n");
    for s in &sessions {
        let title = if s.title.is_empty() { "(untitled)" } else { &s.title };
        println!(
            "  {} | {} | {} turns | {}",
            &s.id[..8.min(s.id.len())],
            title,
            s.turn_count(),
            s.updated_at.format("%Y-%m-%d %H:%M")
        );
    }
    println!("\nResume with: prx chat --session <ID>");
    Ok(())
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

    /// Build a `TerminalGuard` in the inactive state (no real terminal
    /// mutation), suitable for unit-testing the bookkeeping.
    fn inactive_guard() -> TerminalGuard {
        TerminalGuard {
            raw_mode_active: AtomicBool::new(false),
            alt_screen_active: AtomicBool::new(false),
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
            alt_screen_active: AtomicBool::new(true),
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
        assert!(!guard.alt_screen_active.load(Ordering::Acquire));
    }

    #[test]
    fn leave_flips_flags_exactly_once() {
        let guard = fake_active_guard();
        assert!(guard.raw_mode_active.load(Ordering::Acquire));
        assert!(guard.alt_screen_active.load(Ordering::Acquire));
        guard.leave();
        assert!(!guard.raw_mode_active.load(Ordering::Acquire));
        assert!(!guard.alt_screen_active.load(Ordering::Acquire));
        // Second leave is a no-op (CAS fails, no crossterm calls).
        guard.leave();
        assert!(!guard.raw_mode_active.load(Ordering::Acquire));
        assert!(!guard.alt_screen_active.load(Ordering::Acquire));
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
        assert!(!guard.alt_screen_active.load(Ordering::Acquire));
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

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
    // ── Panic hook: restore terminal state on crash ────────────
    // Must be sync-only (no async in panic hooks). Preserves the original
    // hook so backtraces still print.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Best-effort terminal restoration — never panic inside the hook
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stderr(),
            crossterm::cursor::Show,
            crossterm::terminal::LeaveAlternateScreen
        );
        original_hook(info);
    }));

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
    //   - feature `terminal-tui` + TTY stdin → ratatui/crossterm KeyEvent loop
    //     driving `dispatch_global_key` against a session-scoped mirror.
    //   - otherwise → legacy reedline + BufRead fallback via TerminalChannel.
    #[cfg(feature = "terminal-tui")]
    {
        use std::io::IsTerminal as _;
        // TUI mode is opt-in via PRX_TUI=1 until the ratatui rendering loop is
        // wired up. The current spawn_tui_input_task only enables raw mode
        // without an alternate screen or a frame draw loop, so terminal output
        // collides with tracing and the user's keystrokes are not echoed.
        // See P3 for the full renderer integration.
        let tui_enabled = std::env::var("PRX_TUI").as_deref() == Ok("1") && std::io::stdin().is_terminal();
        if tui_enabled {
            let mirror_for_input: Arc<parking_lot::Mutex<tui::TuiState>> =
                Arc::new(parking_lot::Mutex::new(tui::TuiState::new(provider_name, model_name)));
            spawn_tui_input_task(
                input_tx,
                mirror_for_input,
                shutdown.clone(),
                Arc::clone(&last_ctrlc_ms),
                Arc::clone(&active_cancel),
            );
        } else {
            // Default path (TTY without PRX_TUI=1, or pipe/heredoc) — keep the
            // legacy reedline + BufRead fallback via TerminalChannel.
            let terminal_for_listen = TerminalChannel::new(plain_mode);
            tokio::spawn(async move {
                if let Err(e) = terminal_for_listen.listen(input_tx).await {
                    tracing::error!("Terminal input loop error: {e}");
                }
            });
        }
    }
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
        #[cfg(feature = "terminal-tui")]
        let tui_mirror: Arc<parking_lot::Mutex<tui::TuiState>> =
            Arc::new(parking_lot::Mutex::new(tui::TuiState::new(provider_name, model_name)));
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
///
/// Raw mode is enabled for the duration of the loop and unconditionally
/// disabled on exit (via the existing panic hook + an explicit cleanup
/// branch). We do not enter the alternate screen — the existing `UiActor`
/// renderer in `channels/terminal.rs` keeps writing to the normal buffer, so
/// keeping the same screen avoids fighting with it.
#[cfg(feature = "terminal-tui")]
fn spawn_tui_input_task(
    input_tx: mpsc::Sender<crate::channels::traits::ChannelMessage>,
    mirror: Arc<parking_lot::Mutex<tui::TuiState>>,
    shutdown: CancellationToken,
    last_ctrlc_ms: Arc<AtomicU64>,
    active_cancel: Arc<parking_lot::Mutex<Option<CancellationToken>>>,
) {
    tokio::task::spawn_blocking(move || {
        if let Err(e) = crossterm::terminal::enable_raw_mode() {
            tracing::error!("failed to enable raw mode for TUI input: {e}");
            return;
        }
        let result = run_tui_input_loop(&input_tx, &mirror, &shutdown, &last_ctrlc_ms, &active_cancel);
        // Cleanup raw mode regardless of success — keeps the terminal usable
        // after `prx chat` returns. The panic hook in `run()` also disables
        // raw mode as a defence-in-depth measure.
        let _ = crossterm::terminal::disable_raw_mode();
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

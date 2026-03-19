//! `prx chat` entry point — rich terminal interactive chat.
//!
//! Wires up the full agent pipeline (memory, tools, providers, security, hooks,
//! observability) and uses [`TerminalChannel`] for streaming I/O through the
//! event-driven UI Actor.

pub mod commands;
pub mod sanitize;
pub mod session;
pub mod terminal_proto;

#[cfg(feature = "terminal-tui")]
pub mod renderer;
#[cfg(feature = "terminal-tui")]
pub mod tui;

use crate::agent::loop_::{
    build_context, build_runtime_system_prompt, increment_recalled_useful_counts,
    is_tool_loop_cancelled, run_tool_call_loop, select_prompt_skills, ScopeContext,
    ToolCallNotification, ToolConcurrencyGovernanceConfig,
};
use crate::approval::ApprovalManager;
use crate::channels::traits::extract_outgoing_media;
use crate::channels::{
    extract_tool_context_summary, is_context_window_overflow_error, sanitize_channel_response,
    Channel, SendMessage, TerminalChannel,
};
use crate::security::PolicyPipeline;
use crate::config::Config;
use crate::hooks::{payload_error, HookEvent, HookManager};
use crate::memory::{self, Memory, MemoryCategory};
use crate::observability::{self, Observer, ObserverEvent};
use crate::providers::{self, ChatMessage, Provider};
use crate::runtime;
use crate::security::SecurityPolicy;
use crate::tools;
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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
    let has_system = history
        .first()
        .map(|m| m.role == "system")
        .unwrap_or(false);
    let start = if has_system { 1 } else { 0 };

    // Keep only the last COMPACT_KEEP_MESSAGES conversation turns
    let turn_count = history.len() - start;
    if turn_count > COMPACT_KEEP_MESSAGES {
        let drain_end = start + turn_count - COMPACT_KEEP_MESSAGES;
        history.drain(start..drain_end);
    }

    // Truncate individual messages
    for msg in &mut history[start..] {
        if msg.content.chars().count() > COMPACT_CONTENT_CHARS {
            msg.content = truncate_with_ellipsis(&msg.content, COMPACT_CONTENT_CHARS);
        }
    }

    // Enforce total character budget (drop oldest turns first)
    while history[start..]
        .iter()
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
    let runtime: Arc<dyn runtime::RuntimeAdapter> =
        Arc::from(runtime::create_runtime(&config.runtime)?);
    let security = Arc::new(SecurityPolicy::from_config(
        &config.autonomy,
        &config.workspace_dir,
    ));

    // ── Memory ───────────────────────────────────────────────────
    let mem: Arc<dyn Memory> =
        Arc::from(memory::create_memory_with_storage_and_routes_with_acl(
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
        Some("last") => {
            match load_latest_session(mem.as_ref()).await {
                Some(s) => {
                    info!(id = %s.id, title = %s.title, turns = s.turn_count(), "Resumed session");
                    s
                }
                None => {
                    info!("No previous session found, starting new");
                    session::ChatSession::new(provider_name, model_name)
                }
            }
        }
        Some(id) => {
            match load_session_by_id(mem.as_ref(), id).await {
                Some(s) => {
                    info!(id = %s.id, title = %s.title, turns = s.turn_count(), "Resumed session");
                    s
                }
                None => {
                    eprintln!("Session '{id}' not found, starting new session.");
                    session::ChatSession::new(provider_name, model_name)
                }
            }
        }
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

    // Spawn input loop on a separate TerminalChannel (input only, no UI actor)
    let terminal_for_listen = TerminalChannel::new(plain_mode);
    tokio::spawn(async move {
        if let Err(e) = terminal_for_listen.listen(input_tx).await {
            tracing::error!("Terminal input loop error: {e}");
        }
    });

    // ── Graceful shutdown signal ─────────────────────────────────
    // Instead of std::process::exit(), all signal handlers use this token to
    // break the main loop gracefully, allowing final session save + teardown.
    let shutdown = CancellationToken::new();

    // ── Ctrl+C shared state ─────────────────────────────────────
    // Tracks the timestamp (ms) of the last Ctrl+C press for double-press detection.
    let last_ctrlc_ms = Arc::new(AtomicU64::new(0));
    // The active cancellation token for the current generation turn (if any).
    let active_cancel: Arc<parking_lot::Mutex<Option<CancellationToken>>> =
        Arc::new(parking_lot::Mutex::new(None));

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
        let sigterm_result =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
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
        let mem_context =
            build_context(mem.as_ref(), &user_input, config.memory.min_relevance_score).await;
        let context = mem_context.preamble.clone();
        let enriched = if context.is_empty() {
            user_input.clone()
        } else {
            format!("{context}{user_input}")
        };

        // Build system prompt with skill selection
        let selected_skills =
            select_prompt_skills(&user_input, &skills, &config, skill_embedder.as_ref()).await;
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
        } else {
            history[0] = ChatMessage::system(system_prompt);
        }
        history.push(ChatMessage::user(&enriched));

        // ── Set active recipient/channel on tools (for proactive messaging) ──
        for tool in &tools_registry {
            tool.set_active_recipient("user").await;
            tool.set_active_channel(Arc::clone(&terminal) as Arc<dyn Channel>)
                .await;
        }

        // ── Streaming pipeline setup ─────────────────────────────
        let cancellation = CancellationToken::new();
        let (delta_tx, delta_rx) = mpsc::channel::<String>(DELTA_CHANNEL_CAPACITY);

        // Start a streaming draft on the terminal
        let draft_id = match terminal
            .send_draft(&SendMessage::new("", "user"))
            .await
        {
            Ok(id) => id,
            Err(e) => {
                tracing::debug!("Failed to start draft: {e}");
                None
            }
        };

        // Spawn background task: accumulate deltas → channel.update_draft()
        // Follows the exact same pattern as process_channel_message in channels/mod.rs
        let draft_updater = if let Some(ref d_id) = draft_id {
            let channel: Arc<TerminalChannel> = Arc::clone(&terminal);
            let reply_target = "user".to_string();
            let draft_id_owned = d_id.clone();
            let mut rx = delta_rx;
            Some(tokio::spawn(async move {
                let mut accumulated = String::new();
                while let Some(delta) = rx.recv().await {
                    accumulated.push_str(&delta);
                    if let Err(e) = channel
                        .update_draft(&reply_target, &draft_id_owned, &accumulated)
                        .await
                    {
                        tracing::debug!("Draft update failed: {e}");
                    }
                }
            }))
        } else {
            // No draft — consume delta_rx so the sender doesn't block
            let mut rx = delta_rx;
            Some(tokio::spawn(async move {
                while rx.recv().await.is_some() {}
            }))
        };

        // Register this turn's cancellation token so the Ctrl+C handler can cancel it.
        *active_cancel.lock() = Some(cancellation.clone());

        // ── Tool event forwarding (visual feedback in terminal) ──
        let (tool_event_tx, mut tool_event_rx) =
            mpsc::channel::<ToolCallNotification>(TOOL_EVENT_CHANNEL_CAPACITY);
        let terminal_for_tools = Arc::clone(&terminal);
        let tool_event_forwarder = tokio::spawn(async move {
            while let Some(notif) = tool_event_rx.recv().await {
                match notif {
                    ToolCallNotification::Started { name, args_summary } => {
                        terminal_for_tools
                            .notify_tool_started(&name, &args_summary)
                            .await;
                    }
                    ToolCallNotification::Finished { name, success } => {
                        terminal_for_tools
                            .notify_tool_finished(&name, success)
                            .await;
                    }
                }
            }
        });

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
                        kill_switch_force_serial: config
                            .agent
                            .concurrency_kill_switch_force_serial,
                        rollout_stage: config.agent.concurrency_rollout_stage.clone(),
                        rollout_sample_percent: config
                            .agent
                            .concurrency_rollout_sample_percent,
                        rollout_channels: config
                            .agent
                            .concurrency_rollout_channels
                            .clone(),
                        auto_rollback_enabled: config
                            .agent
                            .concurrency_auto_rollback_enabled,
                        rollback_timeout_rate_threshold: config
                            .agent
                            .concurrency_rollback_timeout_rate_threshold,
                        rollback_cancel_rate_threshold: config
                            .agent
                            .concurrency_rollback_cancel_rate_threshold,
                        rollback_error_rate_threshold: config
                            .agent
                            .concurrency_rollback_error_rate_threshold,
                    },
                    Some(&config.agent.compaction),
                    Some(cancellation.clone()),
                    Some(delta_tx.clone()),
                    Some(&scope_ctx),
                    Some(tool_event_tx.clone()),
                ),
            )
            .await;

            match result {
                // ── Timeout ───────────────────────────────────────
                Err(_elapsed) => {
                    if timeout_retries < 1 {
                        timeout_retries += 1;
                        tracing::warn!(
                            "LLM timeout, retrying (attempt {timeout_retries}/1)"
                        );
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
                        .emit(
                            HookEvent::Error,
                            payload_error("chat-turn", "timeout"),
                        )
                        .await;
                    break TurnOutcome::Failed;
                }
                // ── Success ───────────────────────────────────────
                Ok(Ok(resp)) => break TurnOutcome::Success(resp),
                // ── Cancelled (Ctrl+C) ────────────────────────────
                Ok(Err(ref e))
                    if is_tool_loop_cancelled(e) || cancellation.is_cancelled() =>
                {
                    if let Some(ref d_id) = draft_id {
                        let _ = terminal.cancel_draft("user", d_id).await;
                    }
                    break TurnOutcome::Failed;
                }
                // ── Context window overflow → compact + retry ─────
                Ok(Err(ref e)) if is_context_window_overflow_error(e) => {
                    compact_chat_history(&mut history);
                    let compacted_chars: usize = history
                        .iter()
                        .map(|m| m.content.chars().count())
                        .sum();
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
                        .emit(
                            HookEvent::Error,
                            payload_error("chat-turn", &e.to_string()),
                        )
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

        increment_recalled_useful_counts(mem.as_ref(), &mem_context.ids).await;

        // ── Sanitize response: strip tool-call XML/JSON artifacts ──
        let response = sanitize_channel_response(&response, &tools_registry);

        // ── Extract tool context summary for LLM awareness on next turn ──
        let tool_summary =
            extract_tool_context_summary(&history, history_len_before_tools);
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

        // Finalize the streaming draft with the full response
        if let Some(ref d_id) = draft_id {
            if let Err(e) = terminal
                .finalize_draft("user", d_id, display_response)
                .await
            {
                tracing::warn!("Failed to finalize draft: {e}");
                let rendered = render_response(display_response);
                let _ = terminal
                    .send(&SendMessage::new(rendered, "user"))
                    .await;
            }
        } else {
            // No draft was created — send as a complete message with highlighting
            let rendered = render_response(display_response);
            let _ = terminal
                .send(&SendMessage::new(rendered, "user"))
                .await;
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

/// Apply markdown highlighting to a response (when terminal-tui feature is active).
/// Falls back to plain formatting with newline wrapping.
fn render_response(response: &str) -> String {
    #[cfg(feature = "terminal-tui")]
    {
        format!(
            "\n{}\n",
            renderer::render_markdown_with_highlighting(response)
        )
    }
    #[cfg(not(feature = "terminal-tui"))]
    {
        format!("\n{response}\n")
    }
}

// ── Session persistence helpers ──────────────────────────────────────────

/// Save a session to the Memory backend.
async fn save_session(mem: &dyn Memory, session: &session::ChatSession) -> Result<()> {
    let json = session.to_json().map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
    mem.store(
        &session.memory_key(),
        &json,
        MemoryCategory::Conversation,
        None,
    )
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
    let entries = mem
        .list(Some(&MemoryCategory::Conversation), None)
        .await
        .ok()?;
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
        let title = if s.title.is_empty() {
            "(untitled)"
        } else {
            &s.title
        };
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


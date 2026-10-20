//! The `prx chat` entry point.

use super::DELTA_CHANNEL_CAPACITY;
#[cfg(feature = "terminal-tui")]
use super::active_turn::process_active_turn_input_batch;
use super::active_turn::{
    dispatch_provider_worker_cancel_signal, format_provider_worker_report, is_queue_command, is_workers_command,
    provider_workers_cancel_output_for_input, rollback_cancelled_turn_history,
};
use super::bootstrap::{chat_model_switch, chat_provider_chain, initial_chat_dispatcher_state, startup_banner};
#[cfg(feature = "terminal-tui")]
use super::chat_events::{ReduxMode, collect_reasoning_from_history_slice};
use super::chat_events::{
    autosave_memory_key, chat_message_event_scope, chat_runtime_envelope, finalize_chat_non_success_turn,
    finalize_chat_success_turn, record_route_decision_event, sanitize_chat_semantic_memory_content,
};
#[cfg(feature = "terminal-tui")]
use super::compaction::refresh_context_budget_for_tui;
use super::compaction::{
    apply_chat_compaction_patch_and_sync, bounded_legacy_chat_compaction_audit_source, build_chat_compaction_patch,
    chat_embedding_upgrade_notice, compact_chat_history, estimate_chat_history_tokens,
    format_compact_feedback_after_history, format_nothing_to_compact_feedback, legacy_compaction_token_metadata,
    manual_compact_below_trigger_threshold, original_legacy_chat_compaction_audit_source,
    persist_legacy_chat_compaction_audit,
};
use super::copy_select::{copy_success_message, is_copy_command, select_copy_content};
use super::file_mention::{FileMentionEnrichment, enrich_file_mentions_for_prompt};
use super::input_queue::{
    DequeuedInputMessage, QueuedInputMessage, drain_available_input_messages, enqueue_input_message_with_scheduler,
    format_input_backlog_report, is_local_operator_input, is_synthetic_ui_command,
    pop_next_visible_input_task_with_scheduler, publish_main_queue_status, requeue_post_route_admission_rejected_input,
};
use super::output::{print_fallback_chat_output, render_response, should_resend_on_finalize_failure};
use super::provider_turns::{
    PROVIDER_TURN_LIFECYCLE_CHANNEL_CAPACITY, ProviderTurnCompletionContext, ProviderTurnCompletionEvent,
    dispatch_ordered_provider_turn_commit_and_wait, gate_cancelled_provider_turn_finalization,
    gate_completed_provider_turn_finalization, gate_failed_provider_turn_finalization, mark_provider_turn_completed,
    provider_turn_visible_admission, provider_worker_status, publish_provider_worker_status,
    record_provider_turn_completion_context, record_provider_turn_lifecycle_event, record_provider_turn_usage,
    register_provider_history_commit_task, register_provider_turn_worker, route_provider_completion_event_and_publish,
    start_provider_turn_task, take_provider_turn_completion_history_len,
};
#[cfg(feature = "terminal-tui")]
use super::provider_turns::{
    PendingOrderedProviderTurnCommit, PerTurnContext, ProviderTurnFinalizerEvent, ProviderTurnFinalizerResult,
    ProviderTurnTerminalPlan, apply_ready_ordered_provider_turn_commits,
    drain_provider_turn_finalizer_events_and_publish, enqueue_provider_turn_finalizer_event,
    finalize_all_per_turn_contexts_as_cancelled, finalize_ready_per_turn_contexts,
    provider_turn_terminal_plan_from_completion, resolve_provider_turn_completion,
    spawn_provider_turn_completion_waiter,
};
#[cfg(feature = "terminal-tui")]
use super::pty_attach::{handle_pty_command, reattach_pty};
use super::session_glue::{
    DaemonControl, ReapedSessionLogArchive, assignment_turn_guard, compact_child_completion_summary,
    consume_deferred_visible_input_pop, format_finished_session_announcement, format_observed_session_announcement,
    format_reloaded_background_sessions, handle_local_session_command, is_chat_quit_command, now_ms,
    shutdown_child_sessions_for_exit, start_assignment_enrolment, start_daemon_session_command,
    surface_session_message, surface_turn_elapsed_message, withdraw_assignment_enrolment,
};
#[cfg(feature = "terminal-tui")]
use super::session_glue::{
    defer_resume_saved_session_if_provider_turn_pending, refresh_sessions_cache,
    should_continue_event_pump_after_input_closed, should_drain_deferred_resume_after_visible_inputs,
    surface_active_turn_message,
};
#[cfg(feature = "terminal-tui")]
use super::session_store::rewound_chat_session_from;
use super::session_store::{
    bind_session_to_runtime_provider_model, branched_chat_session_from, format_saved_chat_sessions,
    format_turn_boundaries, history_for_session_with_system, list_saved_sessions,
    load_latest_session_with_message_events, load_session_by_id_with_message_events, parse_turn_boundary,
    persisted_history_for_current_turn, save_session, saved_chat_sessions, session_turns_to_history,
    upsert_leading_system_prompt,
};
#[cfg(feature = "terminal-tui")]
use super::session_switch::request_diff_apply_approval;
use super::session_switch::{
    ChatControlEvent, ChatSwitchCtx, PendingChatRewind, PendingDiffApply, RewindApprovalOutcome,
    apply_chat_session_switch, approval_already_pending_message, approval_in_progress, resolve_rewind_approval,
    resume_saved_session_by_id, should_ignore_session_event_after_chat_resume, start_new_chat_session,
};
#[cfg(feature = "terminal-tui")]
use super::terminal::TerminalGuard;
#[cfg(feature = "terminal-tui")]
use super::terminal::select_chat_tui;
use super::terminal::{TracingChatGuard, ensure_chat_tracing_to_file, install_chat_panic_hook};
#[cfg(feature = "terminal-tui")]
use super::tui;
#[cfg(feature = "terminal-tui")]
use super::tui_loop::spawn_tui_unified_loop;
use super::views::collect_workspace_diff;
#[cfg(feature = "terminal-tui")]
use super::views::{
    attach_breadcrumb_for_transition, build_active_session_attach_projection, open_diff_view, open_transcript_view,
    refresh_attached_session_view_from_ring,
};
use super::{
    CHAT_CONTROL_CHANNEL_CAPACITY, DOUBLE_CTRLC_WINDOW_MS, INPUT_CHANNEL_CAPACITY, TOOL_EVENT_CHANNEL_CAPACITY,
    commands, decision_source, diff_apply, dispatcher, sanitize, session, terminal_proto,
};
use crate::agent::context_recall::increment_recalled_useful_counts;
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::runtime_prompt::build_runtime_system_prompt;
use crate::agent::turn_contracts::ScopeContext;
use crate::agent::turn_contracts::ToolCallNotification;
use crate::agent::turn_contracts::is_tool_loop_cancelled;
use crate::agent::turn_engine::run_turn;
use crate::approval::ApprovalManager;
use crate::channels::Channel;
use crate::channels::SendMessage;
use crate::channels::TerminalChannel;
use crate::channels::sanitize_channel_response;
use crate::channels::traits::extract_outgoing_media;
use crate::chat::terminal_proto::DraftVersionCounter;
use crate::config::Config;
use crate::hooks::HookEvent;
use crate::hooks::payload_error;
use crate::llm::route_decision::ProviderExecutionOutcome;
use crate::llm::route_decision::RouteDecision;
use crate::llm::route_decision::route_event_scope;
use crate::memory::Memory;
use crate::memory::MemoryCategory;
use crate::memory::MemoryFabric;
use crate::observability::ObserverEvent;
use crate::providers;
use crate::providers::ChatMessage;
use crate::providers::Provider;
use crate::tools::Tool;
use anyhow::Context as _;
use anyhow::Result;
use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

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

    let mut provider_runtime_options = providers::provider_runtime_options_from_config(&config);

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
        provider_name,
        model_name,
        temperature,
        security.clone(),
        config.workspace_dir.clone(),
        config.multimodal.clone(),
        config.agents_with_references().into_owned(),
        // The spawn fallback is paired with `provider_name` (the `-p` override
        // when one is given), so it must be the credential released to that
        // provider, never the configured default's root key.
        startup_connection.api_key.map(str::to_string),
        provider_runtime_options.clone(),
        config.sessions_spawn.clone(),
        Arc::clone(&active_runs),
    )
    .with_cost_config(config.cost.clone())
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
                let active_task_id = per_turn_contexts.keys().next().copied();
                #[cfg(feature = "terminal-tui")]
                if let Some(active_task_id) = active_task_id {
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
        if user_input == "/model save-default" {
            let saved = super::model_commands::save_default(&config, provider_name, &current_model_owned, &provider_runtime_options).await;
            match saved {
                Ok(()) => emit_chat_output("Saved the current model and level as the default."),
                Err(error) => emit_chat_output(&format!("Cannot save model default: {}", providers::sanitize_api_error(&error.to_string()))),
            }
            continue;
        }
        let model_command = user_input.strip_prefix("/model ");
        let level_command = user_input.strip_prefix("/level ");
        if model_command.is_some() || level_command.is_some() {
            let (new_model, candidate_options) = match super::model_commands::selection(&user_input, &current_model_owned, provider_name, &provider_runtime_options) {
                Ok(selection) => selection,
                Err(error) => { emit_chat_output(&format!("{error:#}")); continue; }
            };
            let new_model = new_model.as_str();
            let rebuilt = chat_model_switch(&config, provider_name, new_model).and_then(|()| {
                chat_provider_chain(
                    &config,
                    provider_name,
                    configured_provider_name,
                    new_model,
                    &candidate_options,
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
                    provider_runtime_options = candidate_options;
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
            let compact_context = provider_binding
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("chat provider binding is missing"))?
                .effective_compaction();
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

        let mut effective_compaction = admitted_binding.effective_compaction()?;
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

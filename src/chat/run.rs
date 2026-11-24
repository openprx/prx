//! The `prx chat` entry point.

use super::DELTA_CHANNEL_CAPACITY;
#[cfg(feature = "terminal-tui")]
use super::active_turn::process_active_turn_input_batch;
use super::active_turn::{
    dispatch_provider_worker_cancel_signal, format_provider_worker_report, is_queue_command, is_workers_command,
    provider_workers_cancel_output_for_input,
};
use super::bootstrap::{chat_model_switch, chat_provider_chain, initial_chat_dispatcher_state, startup_banner};
#[cfg(feature = "terminal-tui")]
use super::chat_events::{ReduxMode, collect_reasoning_from_history_slice};
use super::chat_events::{
    chat_message_event_scope, finalize_chat_non_success_turn, finalize_chat_success_turn, record_route_decision_event,
    sanitize_chat_semantic_memory_content,
};
#[cfg(feature = "terminal-tui")]
use super::compaction::refresh_context_budget_for_tui;
use super::compaction::{
    apply_chat_compaction_patch_and_sync, bounded_legacy_chat_compaction_audit_source, build_chat_compaction_patch,
    chat_embedding_upgrade_notice, legacy_compaction_token_metadata, original_legacy_chat_compaction_audit_source,
    persist_legacy_chat_compaction_audit,
};
use super::copy_select::{copy_success_message, is_copy_command, select_copy_content};
use super::file_mention::enrich_file_mentions_for_prompt;
use super::input_queue::{
    DequeuedInputMessage, QueuedInputMessage, drain_available_input_messages, enqueue_input_message_with_scheduler,
    format_input_backlog_report, is_local_operator_input, is_synthetic_ui_command,
    pop_next_visible_input_task_with_scheduler, publish_main_queue_status, requeue_post_route_admission_rejected_input,
};
use super::output::{print_fallback_chat_output, render_response, should_resend_on_finalize_failure};
use super::plain_queue::{lines_queued_since, settle_queued_lines};
use super::provider_turns::{
    OrderedCommitTerminal, PROVIDER_TURN_LIFECYCLE_CHANNEL_CAPACITY, ProviderTurnCompletionContext,
    ProviderTurnCompletionEvent, acknowledged_session_barrier, dispatch_final_usage,
    dispatch_ordered_provider_turn_commit_and_wait, finish_inline_provider_turn,
    gate_cancelled_provider_turn_finalization, gate_failed_provider_turn_finalization, inline_provider_turn_payload,
    mark_provider_turn_completed, provider_turn_visible_admission, provider_view_after_settled_abort,
    provider_worker_status, publish_provider_worker_status, record_provider_turn_completion_context,
    record_provider_turn_lifecycle_event, record_user_turn_at_admission, register_provider_history_commit_task,
    register_provider_turn_worker, route_provider_completion_event_and_publish, settle_aborted_turn,
    start_provider_turn_task,
};
#[cfg(feature = "terminal-tui")]
use super::provider_turns::{
    PendingOrderedProviderTurnCommit, PerTurnContext, ProviderTurnFinalizerEvent, ProviderTurnFinalizerResult,
    ProviderTurnTerminalPlan, apply_ready_ordered_provider_turn_commits, committed_reply_message,
    drain_provider_turn_finalizer_events_and_publish, enqueue_provider_turn_finalizer_event,
    finalize_all_per_turn_contexts_as_cancelled, finalize_ready_per_turn_contexts,
    provider_turn_terminal_plan_from_completion, resolve_provider_turn_completion,
    spawn_provider_turn_completion_waiter, take_provider_turn_completion_history_len,
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
    format_turn_boundaries, leading_system_prompt_for_session, list_saved_sessions,
    load_latest_session_with_message_events, load_session_by_id_with_message_events, parse_turn_boundary,
    persisted_history_for_current_turn, provider_view_for_session, save_session, saved_chat_sessions,
    session_turns_to_history, upsert_leading_system_prompt,
};
#[cfg(feature = "terminal-tui")]
use super::session_switch::request_diff_apply_approval;
use super::session_switch::{
    ChatControlEvent, ChatSwitchCtx, PendingChatRewind, PendingDiffApply, RewindApprovalOutcome,
    apply_chat_session_switch, approval_already_pending_message, approval_in_progress, await_metadata_change,
    record_child_summaries_awaited, resolve_rewind_approval, resume_saved_session_by_id,
    should_ignore_session_event_after_chat_resume, start_new_chat_session,
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
    binding, commands, diff_apply, dispatcher, session, terminal_proto,
};
use crate::agent::context_recall::increment_recalled_useful_counts;
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::runtime_prompt::build_chat_system_prompt;
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

mod entry;
mod one_shot;
mod startup;
pub use entry::{run, run_controlled};
pub use one_shot::{ChatMessageRequest, ONE_SHOT_REFUSAL_EXIT_CODE, OneShotRefused, one_shot_exit_code, run_message};
#[cfg(test)]
pub(in crate::chat) use startup::resume_session;

/// Run the interactive chat session with rich terminal UI.
#[allow(clippy::too_many_lines)]
async fn run_internal(
    config: Config,
    provider_override: Option<String>,
    model_override: Option<String>,
    temperature: f64,
    plain_mode: bool,
    session_id: Option<String>,
    list_sessions: bool,
    shutdown: CancellationToken,
    terminal_producer: Option<crate::runtime::h2_entry::TerminalInputProducer>,
    one_shot: Option<one_shot::OneShotTurn>,
) -> Result<Option<one_shot::OneShotReply>> {
    // A one-shot keeps tracing on stderr (`CliCommand::ChatMessage`), not chat.log.
    let _tracing_guard = if one_shot.is_none() {
        startup::chat_tracing_guard()
    } else {
        None
    };
    // An unsaved one-shot writes no chat session row (AR-R2).
    let persist_sessions = one_shot.as_ref().is_none_or(one_shot::OneShotTurn::persists);

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
    let ctx = entry::build_context(config, list_sessions, terminal_producer).await?;

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
    let (mem, memory_fabric) = startup::chat_memory(&ctx, &config)?;

    // ── List sessions (early return) ─────────────────────────────
    // Early-exit before tools/provider/TUI are needed (MemoryOnly profile never
    // built them), preserving the original `--list-sessions` semantics.
    if list_sessions {
        return list_saved_sessions(mem.as_ref()).await.map(|()| None);
    }

    // ── Tools ────────────────────────────────────────────────────
    // The Interactive profile built the base tool registry inside the bootstrap
    // (security + runtime + memory all ready) and handed it to chat as an *owned*
    // `Vec` in `base_tools` (so chat can append its session tools after the
    // provider + TerminalChannel exist). Take it exactly once; we wrap it in
    // `Arc` ourselves once the chat session tools are appended (see "Chat session
    // runtime" below).
    let base_tools_vec: Vec<Box<dyn Tool>> = ctx
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
        binding::ChatProviderBinding::capture(&ctx.config_generation, &provider, provider_name, model_name)?;

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

    let (
        mut session_event_rx,
        shell_event_sink,
        mut chat_sessions,
        pending_approvals,
        scheduled_input_handle,
        tools_registry,
    ) = startup::session_tools(
        &config,
        &ctx.config_manager,
        provider_name,
        model_name,
        temperature,
        &security,
        &hooks,
        &terminal,
        &mem,
        startup_connection,
        &provider_runtime_options,
        base_tools_vec,
        ctx.controlled_entry.as_ref(),
    )?;
    // The process cost outbox projector (P2-R5), cancelled with `shutdown`.
    crate::runtime::cost_recovery::warn_legacy_files_once(&config.workspace_dir);
    if one_shot.is_some() {
        // The process may exit before the background projector's first pass
        // finishes, so a one-shot awaits one pass before its turn (AR-R2).
        crate::runtime::cost_recovery::CostOutboxProjector::run_once(
            Arc::clone(&mem),
            config.workspace_dir.clone(),
            config.cost.clone(),
        )
        .await;
    }
    let cost_projector = crate::runtime::cost_recovery::CostOutboxProjector::start(
        Arc::clone(&mem),
        config.workspace_dir.clone(),
        config.cost.clone(),
        &shutdown,
    );

    let mut turn_service = ctx
        .publish_turn_runtimes(
            Arc::clone(&tools_registry),
            crate::tools::capability::turn_state::TurnSurfaceKind::Interactive,
        )
        .await?;
    // Keep the publisher alive through every chat task and drain it even when
    // session setup or an ingress operation returns an error.
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(
        crate::runtime::turn_cleanup::current_tracker().as_ref(),
        CancellationToken::new(),
    )?;
    let chat_result = cleanup
        .run(async {
            let child_cleanup =
                crate::runtime::turn_cleanup::current_tracker().context("chat cleanup owner missing")?;
            child_cleanup.retain_service(Arc::clone(&turn_service));

            // v3a: coordination handle for interactive PTY terminal handoff. Shared with
            // the unified TUI render loop (which parks while a PTY is attached) and the
            // main loop's `/pty` handler (which pauses/resumes it around the passthrough).
            // Only the TUI path performs the handoff; the non-TUI fallback has no render
            // loop to suspend.
            #[cfg(feature = "terminal-tui")]
            let pty_handoff = Arc::new(crate::chat::sessions::pty::HandoffControl::new());

            // Pre-load construction: the only mutable session copy, consumed by the
            // reducer seed below. Afterwards the run loop reads the acknowledged session.
            let mut initial_session = startup::resume_session(
                session_id.as_deref(),
                one_shot.is_some(),
                &mem,
                &memory_fabric,
                &config,
                provider_name,
                model_name,
            )
            .await?;
            bind_session_to_runtime_provider_model(&mut initial_session, provider_name, model_name);
            let (mut chat_session_key, mut fabric_turn_seq, banner, mut history) = startup::initial_conversation(
                &initial_session,
                &config,
                provider_name,
                model_name,
                &skills,
                native_tools,
            );
            super::message_inbox::pin_epoch(&config, &chat_session_key).await?;

            #[cfg(feature = "terminal-tui")]
            let (initial_saved_session_entries, chat_mirror) =
                startup::initial_tui_mirror(&mem, &initial_session, &config, provider_name, model_name).await;

            // A resumed plain session shows its recent history on a terminal stdout.
            let resume_recap = (plain_mode
                && session_id.is_some()
                && one_shot.is_none()
                && std::io::IsTerminal::is_terminal(&std::io::stdout()))
            .then(|| super::resume_recap::format_resume_recap(&initial_session))
            .flatten();
            #[cfg(feature = "terminal-tui")]
            let ui_startup = startup::ChatStartupUi {
                banner: &banner,
                session_id: &initial_session.id,
                initial_saved_session_entries: &initial_saved_session_entries,
                resume_recap: resume_recap.as_deref(),
            };

            // ── Input channel ────────────────────────────────────────────
            let (input_tx, mut input_rx) = crate::runtime::h2_entry::ChatInputSender::channel(INPUT_CHANNEL_CAPACITY);
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

            // The reducer seed opens with the startup provider view's system prompt.
            let startup_system_prompt = history
                .first()
                .filter(|message| message.role == "system")
                .map(|message| message.content.clone());
            let (chat_dispatcher, chat_action_rx, dispatcher_shadow_state) = startup::initial_dispatcher(
                provider_name,
                model_name,
                &shutdown,
                &initial_session,
                startup_system_prompt,
                #[cfg(feature = "terminal-tui")]
                &config,
                #[cfg(feature = "terminal-tui")]
                &ui_startup,
            );

            // Shared dual-write guard (raised by the EffectExecutor in Both/Redux mode; the old path checks
            // the guard to decide whether to skip persistence. It is built even in Off mode, where the old
            // path's check is always false at zero cost.
            // P0-1 fix: dropped allow(unused_variables) — the guard is read at the end of an old-path turn,
            // so it is genuinely used under both feature configurations.)
            let dual_write_guard = dispatcher::RuntimeDualWriteGuard::new();
            // The run loop's only view of the session: the reducer's acknowledged state.
            let mut chat_session = chat_dispatcher.session_reader();

            // PRX_CHAT_REDUX is read once at the entry point and reused inside the function body, so the env
            // var is not parsed in several places.
            // S4-B: Pure is the only supported run path; non-Pure values are warned about and forced up.
            #[cfg(feature = "terminal-tui")]
            let top_redux_mode: ReduxMode = { ReduxMode::from_env() };
            // The plain REPL runs one turn of the session at a time; later lines queue.
            // Typed lines (terminal stdin) are acknowledged and bounded; piped input
            // keeps every line.
            let plain_typed_queue = plain_mode && std::io::IsTerminal::is_terminal(&std::io::stdin());
            #[cfg(feature = "terminal-tui")]
            let max_concurrent_visible_turns = if plain_mode {
                1
            } else {
                config.chat.max_concurrent_visible_turns.max(1)
            };
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

            // The last request's tool-surface reserve: `/compact`, compaction
            // feedback and the context footer budget with it, as the preflight does.
            let tool_surface_reserve = crate::agent::context_budget::ToolSurfaceReserve::default();

            // Pick the EffectExecutor mode from the redux mode (TUI feature only)
            #[cfg(feature = "terminal-tui")]
            let effect_executor = {
                let mode: ReduxMode = top_redux_mode;
                let deps = dispatcher::EffectDeps {
                    provider: Arc::clone(&provider),
                    memory_event_recording: config.memory.event_recording_config(),
                    channel: Arc::clone(&terminal) as Arc<dyn crate::channels::Channel>,
                    hooks: Arc::clone(&hooks),
                    observer: Arc::clone(&observer),
                    persistence: dispatcher::PersistenceDeps {
                        memory: Arc::clone(&mem),
                        action_tx: chat_dispatcher.sender(),
                        dual_write_guard: dual_write_guard.clone(),
                    },
                    provider_turn_lifecycle_tx: Some(provider_turn_lifecycle_tx.clone()),
                    redraw_tx: None,
                    tui_mirror: Some(Arc::clone(&chat_mirror)),
                    shutdown: shutdown.clone(),
                    model: dispatcher::ModelSlot::new(Arc::from(model_name)),
                    temperature,
                    tools_registry: Some(Arc::clone(&tools_registry)),
                    approval_router: Arc::new(dispatcher::ApprovalRouter::new()),
                    tool_security_policy: Arc::clone(&security),
                    tool_tiering: config.tool_tiering.clone(),
                    tool_surface_reserve: tool_surface_reserve.clone(),
                };
                tracing::info!(mode = ?mode, "chat EffectExecutor in Pure real-deps mode");
                let executor = dispatcher::EffectExecutor::new_with_deps(deps)
                    .with_cleanup_owner(Arc::clone(&child_cleanup))
                    .with_session_publisher(chat_dispatcher.session_publisher());
                if persist_sessions {
                    executor
                } else {
                    executor.without_session_rows()
                }
            };
            // Non-TUI: the persistence effects run for real (save, publish, ack); the
            // tool loop performs provider turns, hooks and output itself.
            #[cfg(not(feature = "terminal-tui"))]
            let effect_executor = {
                let executor = dispatcher::EffectExecutor::new_persistence_only(dispatcher::PersistenceDeps {
                    memory: Arc::clone(&mem),
                    action_tx: chat_dispatcher.sender(),
                    dual_write_guard: dual_write_guard.clone(),
                })
                .with_session_publisher(chat_dispatcher.session_publisher());
                if persist_sessions {
                    executor
                } else {
                    executor.without_session_rows()
                }
            };

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

            #[cfg(feature = "terminal-tui")]
            let (snapshot_tx_for_dispatcher, snapshot_rx_for_tui) =
                startup::initial_snapshot_channel(provider_name, model_name, &initial_session, &config, top_redux_mode);

            // The session authority outlives `shutdown`: shutdown cancels turns, and
            // the exit sequence still settles them (and records the final usage and
            // child exit summaries) through the dispatcher. It stops only in
            // `finish_chat_session`, after the last of those commits; the guard stops
            // it on an early error return as well, so no dispatcher task outlives this
            // scope.
            let dispatcher_stop = CancellationToken::new();
            let _dispatcher_stop_on_exit = dispatcher_stop.clone().drop_guard();

            #[cfg(feature = "terminal-tui")]
            let dispatcher_handle = dispatcher::spawn_dispatcher_task_full(
                dispatcher_shadow_state,
                chat_action_rx,
                dispatcher_stop.clone(),
                effect_executor,
                Some(turn_signal.clone()),
                snapshot_tx_for_dispatcher,
            );
            #[cfg(not(feature = "terminal-tui"))]
            let dispatcher_handle = dispatcher::spawn_dispatcher_task_with_signal(
                dispatcher_shadow_state,
                chat_action_rx,
                dispatcher_stop.clone(),
                effect_executor,
                Some(turn_signal.clone()),
            );

            // Created before `terminal_input` shows the banner, so a press from then on is kept for the handler below.
            let interrupts = startup::interrupt_listener()?;

            // ── Ctrl+C shared state ─────────────────────────────────────
            // Tracks the timestamp (ms) of the last Ctrl+C press for double-press detection.
            // Lifted above the input loop so the TUI dispatcher can fold its own
            // Ctrl+C presses into the same double-press → exit semantics.
            let last_ctrlc_ms = Arc::new(AtomicU64::new(0));
            // Non-TUI fallback still needs a shared cancellation slot for SIGINT.
            #[cfg(not(feature = "terminal-tui"))]
            let active_cancel: Arc<parking_lot::Mutex<Option<CancellationToken>>> =
                Arc::new(parking_lot::Mutex::new(None));
            // Fires when the line front end's stdin reaches EOF.
            let stdin_closed = CancellationToken::new();
            // A one-shot reads no terminal input: its one message is queued below.
            #[cfg(feature = "terminal-tui")]
            let (terminal_guard, redraw_tx_for_main) = if one_shot.is_some() {
                drop(input_tx);
                (None, None)
            } else {
                startup::terminal_input(
                    plain_mode,
                    entry::take_producer(&ctx),
                    &ui_startup,
                    &terminal,
                    input_tx,
                    &control_tx,
                    &chat_mirror,
                    &shutdown,
                    &last_ctrlc_ms,
                    &chat_dispatcher,
                    &executor_redraw_slot,
                    &snapshot_rx_for_tui,
                    &pty_handoff,
                    &config,
                    &security,
                    &stdin_closed,
                )
                .await
            };
            #[cfg(feature = "terminal-tui")]
            let _operator_notices = startup::tui_operator_notices(&chat_dispatcher, redraw_tx_for_main.as_ref());
            #[cfg(not(feature = "terminal-tui"))]
            let redraw_tx_for_main: Option<mpsc::Sender<()>> = None;
            #[cfg(not(feature = "terminal-tui"))]
            if one_shot.is_some() {
                drop(input_tx);
            } else {
                println!("{banner}");
                eprintln!(
                    "{}",
                    super::session_glue::current_chat_session_line(&chat_session.session().id)
                );
                if let Some(recap) = resume_recap.as_deref() {
                    println!("{recap}");
                }
                startup::spawn_line_input_listener(plain_mode, input_tx, stdin_closed.clone());
            }

            let mut plain_mode_turn_failed = false;

            startup::ctrlc_handler(
                interrupts,
                &last_ctrlc_ms,
                &shutdown,
                &chat_dispatcher,
                #[cfg(feature = "terminal-tui")]
                &chat_mirror,
                #[cfg(feature = "terminal-tui")]
                &redraw_tx_for_main,
                #[cfg(not(feature = "terminal-tui"))]
                &active_cancel,
            );

            #[cfg(unix)]
            startup::sigterm_handler(&shutdown, &chat_dispatcher);

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
            // The acknowledged compaction epoch the provider view was last brought up
            // to at a turn's admission (see the admission barrier below).
            let mut provider_view_compaction_epoch = chat_session.acked().compaction_epoch;
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
            let mut per_turn_contexts: std::collections::HashMap<
                crate::chat::turn_scheduler::TurnTaskId,
                PerTurnContext,
            > = std::collections::HashMap::new();
            #[cfg(feature = "terminal-tui")]
            let mut pending_ordered_provider_turn_commits: std::collections::HashMap<
                crate::chat::turn_scheduler::TurnTaskId,
                PendingOrderedProviderTurnCommit,
            > = std::collections::HashMap::new();
            #[cfg(feature = "terminal-tui")]
            let mut deferred_resume_saved_session_ids: std::collections::VecDeque<String> =
                std::collections::VecDeque::new();
            let mut attached_follow: Option<crate::chat::sessions::id::SessionId> = None;
            // Display sequence `#N` of the currently-followed session, kept in lock-step
            // with `attached_follow`. Used purely to reconstruct the *previous* focus
            // target when an optimistic attach must be rolled back (v1.1b review P0): on
            // attach failure the key thread has already pointed the prompt at the new
            // seq, so the main loop restores `Main` (when None) or `Session { seq }`.
            let mut attached_follow_seq: Option<u64> = None;
            let mut child_focus = super::child_focus::ChildFocus::default();
            let mut message_inbox = super::message_inbox::ChatInbox::open(&config, &chat_session_key)?;
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
            let mut input_events_open = one_shot.is_none();
            // The one-shot message, queued literally (D4): no priority prefix or
            // command is interpreted. The input channel stays closed, so the loop
            // ends once this turn resolves.
            // The one-shot succeeds when its turn's ordered commit is acknowledged.
            let mut one_shot_task_id: Option<crate::chat::turn_scheduler::TurnTaskId> = None;
            let mut one_shot_binding: Option<Arc<binding::ChatTurnBinding>> = None;
            if let Some(turn) = one_shot.as_ref() {
                turn.enqueue(
                    &mut input_backlog,
                    &mut turn_scheduler,
                    chat_session.session().turns.len(),
                );
            }

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

            let loaded_background_sessions = chat_session.session();
            if !loaded_background_sessions.background_sessions.is_empty() {
                let recap = format_reloaded_background_sessions(&loaded_background_sessions.background_sessions);
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
            // An unsaved one-shot session cannot be assigned work.
            let assignment_enrolment = if one_shot.is_none() {
                start_assignment_enrolment(
                    &config,
                    &chat_session_key,
                    &chat_sessions,
                    &assignment_input_tx,
                    &chat_dispatcher,
                    sessions_redraw_handle.as_ref(),
                )
            } else {
                None
            };

            super::child_focus::ChildFocusContext {
                memory: &mem,
                generation: &ctx.config_generation,
                parent: &chat_session_key,
                security: &ctx.security,
                service: &turn_service,
                tools: &tools_registry,
                sessions: &chat_sessions,
                approvals: &pending_approvals,
            }
            .restore()
            .await?;

            // ── Main message loop ────────────────────────────────────────
            //
            // The inner `loop` lets a timer tick do background-session work (summary
            // reflow + status-line refresh) without producing a message or ending the
            // outer loop: it only `break`s with a real input message (or `None` on
            // shutdown). On a tick we poll the registry and surface results via the
            // dispatcher (single source, reaches both render paths), then keep waiting.
            let mut session_erased = false;
            'chat: while let Some(input) = include!("run/event_pump.rs") {
                turn_signal.ensure_history_commits_healthy()?;
                let provider_queue_task_id = input.turn_task_id;
                let input_priority = input.priority;
                let terminal_origin = input.terminal_origin;
                let msg = input.msg;
                if let Err(error) =
                    entry::validate_input(ctx.controlled_entry.is_some(), terminal_origin.is_some(), &msg.content)
                {
                    surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &error.to_string());
                    continue;
                }
                if let Some(target) = attached_follow
                    .as_ref()
                    .filter(|_| super::is_local_operator_input(&msg) && !msg.sender_is_bot)
                {
                    match child_focus
                        .submit(
                            target.as_str(),
                            msg.clone(),
                            &super::child_focus::ChildFocusContext {
                                memory: &mem,
                                generation: &ctx.config_generation,
                                parent: &chat_session_key,
                                security: &ctx.security,
                                service: &turn_service,
                                tools: &tools_registry,
                                sessions: &chat_sessions,
                                approvals: &pending_approvals,
                            },
                        )
                        .await
                    {
                        Ok(Some(output)) => {
                            surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &output);
                            continue;
                        }
                        Err(error) => {
                            surface_session_message(
                                &chat_dispatcher,
                                sessions_redraw_handle.as_ref(),
                                &error.to_string(),
                            );
                            continue;
                        }
                        Ok(None) => {}
                    }
                }
                if msg.id == "mailbox-wake" && msg.sender == "session-message" && msg.sender_is_bot {
                    if let Some(inbox) = message_inbox.as_mut() {
                        inbox.wake();
                    }
                    continue;
                }
                if one_shot.is_none()
                    && super::is_local_operator_input(&msg)
                    && !msg.sender_is_bot
                    && msg.content.starts_with("/msg ")
                {
                    let result = super::message_inbox::send_command(
                        &config,
                        &mem,
                        &chat_session_key,
                        attached_follow.as_ref().map(|id| id.as_str()),
                        &msg,
                        &mut chat_sessions,
                    )
                    .await;
                    match result {
                        Ok(output) => {
                            surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &output)
                        }
                        Err(error) => surface_session_message(
                            &chat_dispatcher,
                            sessions_redraw_handle.as_ref(),
                            &error.to_string(),
                        ),
                    }
                    continue;
                }
                let mut mailbox_input = if let Some(inbox) = message_inbox.as_mut() {
                    match inbox.take_input(&msg) {
                        Ok(input) => input,
                        Err(error) => {
                            surface_session_message(
                                &chat_dispatcher,
                                sessions_redraw_handle.as_ref(),
                                &error.to_string(),
                            );
                            continue;
                        }
                    }
                } else {
                    None
                };
                let user_input = msg.content.clone();
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

                let () = include!("run/commands.rs");

                let model_name = current_model_owned.as_str();
                include!("run/provider_turn.rs");
            }

            // ── Child-session shutdown on exit (Phase C) ─────────────────
            // Snapshot every child session still tracked at exit, persist summaries for
            // live sessions as interrupted, then terminate and clear every child
            // registry through the single session owner. This covers running agents,
            // NeedsInput agents, background shells, and interactive PTYs uniformly.
            // A lost exit summary does not stop the cleanup below; it is returned
            // once teardown has finished.
            let exit_summaries = shutdown_child_sessions_for_exit(&mut chat_sessions, &chat_dispatcher, &turn_signal)
                .await
                .map(drop);
            session_rings.clear();
            withdraw_assignment_enrolment(
                assignment_enrolment.as_ref(),
                &chat_dispatcher,
                sessions_redraw_handle.as_ref(),
            )
            .await;

            // The one-shot outcome is read before teardown; its failure is reported
            // as that outcome, not as the interactive "turns failed" summary.
            let one_shot_reply = one_shot.as_ref().map(|_| {
                one_shot::outcome(
                    one_shot_binding.as_deref(),
                    plain_mode_turn_failed,
                    one_shot_task_id.is_some_and(|task_id| turn_signal.history_commit_acknowledged(task_id)),
                    // The committed turns' stored text, never the provider view (which
                    // renders each reply with its tool evidence).
                    &session_turns_to_history(&chat_dispatcher.session_reader().acked().session),
                )
            });
            let finished = startup::finish_chat_session(
                &shutdown,
                &dispatcher_stop,
                chat_dispatcher,
                dispatcher_handle,
                #[cfg(feature = "terminal-tui")]
                &terminal_guard,
                plain_mode_turn_failed && one_shot.is_none(),
            )
            .await;
            // The dispatcher is drained now: a save that failed during exit,
            // including one only seen while draining, has latched by this point, and
            // `/exit` or EOF must not report success over it.
            if session_erased {
                return Err(super::message_inbox::SessionErased.into());
            }
            exit_summaries?;
            turn_signal.ensure_history_commits_healthy()?;
            finished?;
            one_shot_reply.transpose()
        })
        .await;
    turn_service
        .shutdown(std::time::Instant::now() + Duration::from_secs(2))
        .await;
    hooks.drain_for_shutdown().await;
    cost_projector.stop().await;
    chat_result
}

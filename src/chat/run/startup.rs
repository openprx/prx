//! Mechanical startup and teardown helpers for the interactive chat entry point.
use super::{
    Arc, AtomicU64, Channel, Config, DOUBLE_CTRLC_WINDOW_MS, Memory, Ordering, Result, TerminalChannel, Tool,
    TracingChatGuard, dispatcher, ensure_chat_tracing_to_file, history_for_session_with_system,
    initial_chat_dispatcher_state, load_latest_session_with_message_events, load_session_by_id_with_message_events,
    mpsc, now_ms, providers, save_session, session, startup_banner,
};
#[cfg(feature = "terminal-tui")]
use super::{
    ChatControlEvent, Duration, TerminalGuard, saved_chat_sessions, select_chat_tui, spawn_tui_unified_loop, tui,
};
#[cfg(feature = "terminal-tui")]
use std::io::Write as _;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[cfg(feature = "terminal-tui")]
pub(super) struct ChatStartupUi<'a> {
    pub(super) banner: &'a String,
    pub(super) initial_saved_session_entries: &'a Vec<crate::chat::session::SavedSessionPickerEntry>,
}
pub(super) fn chat_tracing_guard() -> Option<TracingChatGuard> {
    // ── P3-1: Redirect tracing to ~/.openprx/chat.log ────────────────────
    // Returned to run and held for the rest of the session. On failure (no HOME, log dir
    // unwritable, etc.) we fall back to the stderr writer that `main` set up
    // and emit a warning. Never panics.
    match ensure_chat_tracing_to_file() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(error = %e, "P3-1: keeping tracing on stderr (chat.log unavailable)");
            None
        }
    }
}

/// Bind the bootstrap-owned memory backend to Chat's event-recording scope.
pub(super) fn chat_memory(
    ctx: &crate::runtime::bootstrap::AppContext,
    config: &Config,
) -> Result<(Arc<dyn Memory>, super::MemoryFabric)> {
    let memory = ctx
        .memory
        .clone()
        .ok_or_else(|| anyhow::anyhow!("bootstrap did not build a memory backend for chat"))?;
    let fabric = super::MemoryFabric::new(memory.clone(), config.workspace_dir.to_string_lossy())
        .with_event_recording(config.memory.event_recording_config());
    Ok((memory, fabric))
}

pub(super) fn session_tools(
    config: &Config,
    provider_name: &str,
    model_name: &str,
    temperature: f64,
    security: &Arc<crate::security::SecurityPolicy>,
    hooks: &Arc<crate::hooks::HookManager>,
    terminal: &Arc<TerminalChannel>,
    mem: &Arc<dyn Memory>,
    startup_connection: providers::ProviderConnection<'_>,
    provider_runtime_options: &providers::ProviderRuntimeOptions,
    mut base_tools_vec: Vec<Box<dyn Tool>>,
    controlled: Option<&Arc<crate::runtime::h2_entry::ControlledH2Profile>>,
) -> Result<(
    mpsc::Receiver<crate::chat::sessions::SessionEvent>,
    crate::chat::sessions::SessionEventSink,
    crate::chat::sessions::ChatSessionsHandle,
    crate::chat::sessions::PendingApprovals,
    crate::chat::scheduled_input::ScheduledInputHandle,
    Arc<Vec<Box<dyn Tool>>>,
)> {
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
    let (session_event_sink, session_event_rx) = crate::chat::sessions::SessionEventSink::channel();
    // Keep a clone for background `/shell` sessions (v2): they stream stdout/
    // stderr through the same event bridge / per-session drainer that agents use,
    // so live `/attach` and `/logs` work uniformly across both kinds. The other
    // clone is consumed by the spawn tool's `into_spawn_sink` below.
    let shell_event_sink = session_event_sink.clone();
    // Chat-side handle over the same single-source registries for `/sessions`,
    // `/kill`, and model-callable managed sessions. Build it before freezing the
    // tool registry so the LLM receives a handle to the same shell registry that
    // the TUI and slash commands render.
    let chat_sessions = crate::chat::sessions::ChatSessionsHandle::new(Arc::clone(&active_runs));
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
        Arc::clone(terminal) as Arc<dyn Channel>,
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
    .with_parent_hooks(Arc::clone(hooks))
    .with_cost_config(config.cost.clone())
    .with_configured_providers(&providers::ConfiguredProviders::from_config(config))
    .with_model_catalog(providers::selection::catalog_for(config))
    .with_hint_config(config)
    .with_fallback_root_connection(startup_connection.released)
    .with_shared_memory(Arc::clone(mem))
    .with_event_recording(config.memory.event_recording_config())
    .with_event_sink(session_event_sink.into_spawn_sink())
    .with_approval_resolver_factory(approval_resolver_factory)
    .with_process_recovery(controlled.is_none() && config.causal_tree.enabled)?;
    let spawn_tools_handle = spawn_tool.tools_handle();
    let scheduled_input_handle = crate::chat::scheduled_input::ScheduledInputHandle::default();

    if controlled.is_none() {
        // Sibling tools share the same single-source registry (only the v1a four;
        // `subagents`/`sessions_history` are intentionally not registered in chat —
        // see plan §C.0 blocker 4).
        base_tools_vec.push(Box::new(
            crate::tools::SessionsListTool::new(Arc::clone(&active_runs))
                .with_shared_memory(Arc::clone(mem), sessions_workspace_id.clone()),
        ));
        base_tools_vec.push(Box::new(
            crate::tools::SessionsSendTool::with_security(Arc::clone(&active_runs), security.clone())
                .with_shared_memory(Arc::clone(mem))
                .with_event_recording(config.memory.event_recording_config()),
        ));
        base_tools_vec.push(Box::new(
            crate::tools::SessionStatusTool::new(
                Arc::clone(&active_runs),
                provider_name,
                model_name,
                vec![terminal.name().to_string()],
            )
            .with_shared_memory(Arc::clone(mem), sessions_workspace_id),
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
            config,
            security.clone(),
        )));
    }

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

    Ok((
        session_event_rx,
        shell_event_sink,
        chat_sessions,
        pending_approvals,
        scheduled_input_handle,
        tools_registry,
    ))
}

pub(super) async fn resume_session(
    session_id: Option<&str>,
    mem: &Arc<dyn Memory>,
    memory_fabric: &super::MemoryFabric,
    config: &Config,
    provider_name: &str,
    model_name: &str,
) -> Result<session::ChatSession> {
    // ── Session: resume or create new ───────────────────────────
    Ok(match session_id {
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
    })
}

pub(super) fn initial_conversation(
    chat_session: &session::ChatSession,
    config: &Config,
    provider_name: &str,
    model_name: &str,
    skills: &[crate::skills::Skill],
    native_tools: bool,
) -> (String, u64, String, Vec<super::ChatMessage>) {
    // D8-2: run_id is per-turn, not per-session. It is generated inside the turn
    // loop (see `turn_run_id` below) so each user/assistant exchange gets a fresh
    // run_id. The session identity is carried by `chat_session_key`, never by
    // run_id, and turns deliberately set no parent_run_id (that field is reserved
    // for the spawn execution lineage, not for relating turns within a session).
    let chat_session_key = format!("chat:{}", chat_session.id);
    let fabric_turn_seq: u64 = 0;

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
    let history = history_for_session_with_system(chat_session, config, model_name, skills, native_tools);

    (chat_session_key, fabric_turn_seq, banner, history)
}

#[cfg(feature = "terminal-tui")]
pub(super) async fn initial_tui_mirror(
    mem: &Arc<dyn Memory>,
    chat_session: &session::ChatSession,
    config: &Config,
    provider_name: &str,
    model_name: &str,
) -> (
    Vec<crate::chat::session::SavedSessionPickerEntry>,
    Arc<parking_lot::Mutex<tui::TuiState>>,
) {
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

    let chat_mirror: Arc<parking_lot::Mutex<tui::TuiState>> = {
        let mut state = tui::TuiState::new(provider_name, model_name);
        state.chat_mode = chat_session.mode;
        state.autonomy_level = config.autonomy.level;
        state.token_usage_summary = chat_session.token_usage_summary();
        state.provider_model_catalog = tui::slash_provider_model_catalog_from_config(config);
        state.saved_sessions_cache = initial_saved_session_entries.clone();
        Arc::new(parking_lot::Mutex::new(state))
    };

    (initial_saved_session_entries, chat_mirror)
}

pub(super) fn initial_dispatcher(
    provider_name: &str,
    model_name: &str,
    shutdown: &CancellationToken,
    chat_session: &session::ChatSession,
    #[cfg(feature = "terminal-tui")] config: &Config,
    #[cfg(feature = "terminal-tui")] ui_startup: &ChatStartupUi<'_>,
) -> (
    dispatcher::ChatDispatcher,
    mpsc::Receiver<crate::chat::action::Action>,
    crate::chat::state::ChatState,
) {
    #[cfg(feature = "terminal-tui")]
    let initial_saved_session_entries = ui_startup.initial_saved_session_entries;
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
        initial_chat_dispatcher_state(provider_name, model_name, shutdown.clone(), chat_session);
    // The TUI fields exist only with `terminal-tui`; without it the state is
    // used as built (no `mut` binding, so feature-less builds do not warn).
    #[cfg(feature = "terminal-tui")]
    let dispatcher_shadow_state = {
        let mut state = dispatcher_shadow_state;
        state.ui.chat_mode = chat_session.mode;
        state.ui.autonomy_level = config.autonomy.level;
        state.ui.provider_model_catalog = tui::slash_provider_model_catalog_from_config(config);
        state.ui.saved_sessions_cache = initial_saved_session_entries.clone();
        state
    };

    (chat_dispatcher, chat_action_rx, dispatcher_shadow_state)
}

#[cfg(feature = "terminal-tui")]
pub(super) fn initial_snapshot_channel(
    provider_name: &str,
    model_name: &str,
    chat_session: &session::ChatSession,
    config: &Config,
    top_redux_mode: super::ReduxMode,
) -> (
    Option<tokio::sync::watch::Sender<Arc<crate::chat::state::UiSnapshot>>>,
    Option<tokio::sync::watch::Receiver<Arc<crate::chat::state::UiSnapshot>>>,
) {
    // S4-A Commit 3: build the watch::channel<Arc<UiSnapshot>>; the dispatcher pushes a new snapshot
    // whenever ui_dirty=true. TUI render and child views consume
    // this reducer-owned snapshot as the primary UI source, with chat_mirror kept
    // only for synchronous key-thread compatibility and fallback.
    //
    // rx is wired into run_tui_unified_loop in Commit 4; this commit only traces the push rate, and
    // rx is kept as an `Option` for spawn_tui_unified_loop to use.
    let mut initial =
        crate::chat::state::UiSnapshot::initial(std::sync::Arc::from(provider_name), std::sync::Arc::from(model_name));
    initial.chat_mode = chat_session.mode;
    initial.autonomy_level = config.autonomy.level;
    initial.token_usage_summary = chat_session.token_usage_summary();
    let initial = std::sync::Arc::new(initial);
    let (tx, rx) = tokio::sync::watch::channel(initial);
    tracing::info!(mode = ?top_redux_mode, "snapshot_tx constructed for Pure chat mode");
    (Some(tx), Some(rx))
    // Commit 4: snapshot_rx_for_tui is passed to run_tui_unified_loop (see the spawn_tui_unified_loop
    // call in the TUI branch).
}

#[cfg(feature = "terminal-tui")]
pub(super) async fn terminal_input(
    plain_mode: bool,
    mut producer: Option<crate::runtime::h2_entry::TerminalInputProducer>,
    ui_startup: &ChatStartupUi<'_>,
    terminal: &Arc<TerminalChannel>,
    input_tx: crate::runtime::h2_entry::ChatInputSender,
    control_tx: &mpsc::Sender<ChatControlEvent>,
    chat_mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    shutdown: &CancellationToken,
    last_ctrlc_ms: &Arc<AtomicU64>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    executor_redraw_slot: &Arc<parking_lot::Mutex<Option<mpsc::Sender<()>>>>,
    snapshot_rx_for_tui: &Option<tokio::sync::watch::Receiver<Arc<crate::chat::state::UiSnapshot>>>,
    pty_handoff: &Arc<crate::chat::sessions::pty::HandoffControl>,
    config: &Config,
    security: &Arc<crate::security::SecurityPolicy>,
) -> (Option<TerminalGuard>, Option<mpsc::Sender<()>>) {
    let banner = ui_startup.banner;

    // Spawn the appropriate input loop:
    //   - feature `terminal-tui` + TTY stdin + (PRX_TUI != "0") → ratatui/crossterm
    //     KeyEvent loop driving `dispatch_global_key` against the shared
    //     `chat_mirror`, plus a `spawn_render_task` that owns the
    //     `ratatui::Terminal` and redraws on demand.
    //   - otherwise → legacy reedline + BufRead fallback via TerminalChannel.
    //
    // `terminal_guard` is returned to run so its Drop runs at
    // chat::run exit (panic-safe via `install_chat_panic_hook` above). The
    // legacy path leaves `terminal_guard = None`, so no entry side-effects
    // are applied.
    // `redraw_tx_for_main` is `Some(sender)` only on the TUI path; the main
    // loop uses it to nudge the renderer after mutating `chat_mirror` (e.g.
    // echoing the user's submitted input so the conversation pane reflects
    // it immediately rather than waiting for the next async event).
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
                    producer.take(),
                    control_tx.clone(),
                    Arc::clone(chat_mirror),
                    redraw_rx,
                    redraw_tx_loop,
                    shutdown.clone(),
                    Arc::clone(last_ctrlc_ms),
                    chat_dispatcher.clone(),
                    snapshot_rx_for_tui.clone(),
                    Arc::clone(pty_handoff),
                    config.workspace_dir.clone(),
                    Arc::clone(security),
                );
                (Some(guard), Some(redraw_tx_main))
            }
            Err(e) => {
                if let Some(producer) = producer.take() {
                    producer.profile().revoke();
                    shutdown.cancel();
                    tracing::error!("controlled terminal reader unavailable: {e}");
                    return (None, None);
                }
                tracing::warn!(error = %e, "TerminalGuard::enter failed; falling back to reedline input");
                // On guard failure the banner has not been printed yet
                // and we are about to use the legacy non-TUI path, so
                // print it the old way.
                println!("{banner}");
                let terminal_for_listen = TerminalChannel::new(plain_mode);
                tokio::spawn(async move {
                    if let Err(e) = terminal_for_listen
                        .listen_input_sink(crate::runtime::h2_entry::TerminalInputSink::ordinary(input_tx))
                        .await
                    {
                        tracing::error!("Terminal input loop error: {e}");
                    }
                });
                (None, None)
            }
        }
    } else {
        if let Some(producer) = producer.take() {
            producer.profile().revoke();
            shutdown.cancel();
            return (None, None);
        }
        // Fallback path (PRX_TUI=0 opt-out, or non-TTY pipe/heredoc) — keep
        // the legacy reedline + BufRead fallback via TerminalChannel and
        // print the banner the old way for parity with the previous
        // behaviour.
        println!("{banner}");
        let terminal_for_listen = TerminalChannel::new(plain_mode);
        tokio::spawn(async move {
            if let Err(e) = terminal_for_listen
                .listen_input_sink(crate::runtime::h2_entry::TerminalInputSink::ordinary(input_tx))
                .await
            {
                tracing::error!("Terminal input loop error: {e}");
            }
        });
        (None, None)
    }
}

pub(super) fn ctrlc_handler(
    last_ctrlc_ms: &Arc<AtomicU64>,
    shutdown: &CancellationToken,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    #[cfg(feature = "terminal-tui")] chat_mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    #[cfg(feature = "terminal-tui")] redraw_tx_for_main: &Option<mpsc::Sender<()>>,
    #[cfg(not(feature = "terminal-tui"))] active_cancel: &Arc<parking_lot::Mutex<Option<CancellationToken>>>,
) {
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
        let last_ctrlc = Arc::clone(last_ctrlc_ms);
        #[cfg(not(feature = "terminal-tui"))]
        let cancel_ref = Arc::clone(active_cancel);
        let shutdown_signal = shutdown.clone();
        let dispatcher_for_signal = chat_dispatcher.clone();
        #[cfg(feature = "terminal-tui")]
        let mirror_for_signal = Arc::clone(chat_mirror);
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
}

#[cfg(unix)]
pub(super) fn sigterm_handler(shutdown: &CancellationToken, chat_dispatcher: &dispatcher::ChatDispatcher) {
    // SIGTERM handler: signal graceful shutdown.
    //
    // Step 5b dual write: dispatch ShutdownRequested to the dispatcher (shadow observation) and also
    // call shutdown.cancel() as the fallback (the old path's exit protocol is kept).
    // This task must exit on its own when shutdown fires, so it does not hold a sender clone and
    // block the dispatcher task from closing.
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
}

pub(super) async fn finish_chat_session(
    shutdown: &CancellationToken,
    chat_dispatcher: dispatcher::ChatDispatcher,
    dispatcher_handle: tokio::task::JoinHandle<dispatcher::DispatcherStats>,
    #[cfg(feature = "terminal-tui")] terminal_guard: &Option<TerminalGuard>,
    mem: &Arc<dyn Memory>,
    chat_session: &session::ChatSession,
    plain_mode_turn_failed: bool,
) -> Result<()> {
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
        if let Err(e) = save_session(mem.as_ref(), chat_session).await {
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

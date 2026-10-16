//! Fullscreen TUI render source and the unified TUI event loop.

#[cfg(feature = "terminal-tui")]
use super::active_turn::{format_active_turn_local_notice, is_active_turn_local_command};
#[cfg(feature = "terminal-tui")]
use super::copy_select::{apply_fullscreen_mouse_scroll, copy_transcript_selection};
#[cfg(feature = "terminal-tui")]
use super::external_editor::{
    CrosstermExternalEditorTerminalMode, ExternalEditorResult, edit_text_with_external_editor, resolve_external_editor,
};
#[cfg(feature = "terminal-tui")]
use super::input_queue::format_queued_input_notice;
#[cfg(feature = "terminal-tui")]
use super::input_queue::input_priority_from_text;
#[cfg(feature = "terminal-tui")]
use super::session_glue::{now_ms, surface_session_message};
#[cfg(feature = "terminal-tui")]
use super::session_switch::ChatControlEvent;
#[cfg(feature = "terminal-tui")]
use super::tui;
#[cfg(feature = "terminal-tui")]
use super::views::{
    TRANSCRIPT_COMMAND, apply_optimistic_focus, attach_command_for_seq, close_diff_view, close_provider_worker_view,
    close_transcript_view, open_provider_worker_view, scroll_active_session_view,
};
#[cfg(feature = "terminal-tui")]
use super::{DOUBLE_CTRLC_WINDOW_MS, SYNTHETIC_UI_COMMAND_SENDER, dispatcher};
#[cfg(feature = "terminal-tui")]
use anyhow::Result;
#[cfg(feature = "terminal-tui")]
use std::collections::VecDeque;
#[cfg(feature = "terminal-tui")]
use std::sync::Arc;
#[cfg(feature = "terminal-tui")]
use std::sync::atomic::AtomicU64;
#[cfg(feature = "terminal-tui")]
use std::sync::atomic::Ordering;
#[cfg(feature = "terminal-tui")]
use std::time::Duration;
#[cfg(feature = "terminal-tui")]
use std::time::Instant;
#[cfg(feature = "terminal-tui")]
use tokio::sync::mpsc;
#[cfg(feature = "terminal-tui")]
use tokio_util::sync::CancellationToken;

#[cfg(feature = "terminal-tui")]
const DIRECTIONAL_SWITCH_DEBOUNCE: Duration = Duration::from_millis(100);

#[cfg(feature = "terminal-tui")]
pub(super) fn debounce_directional_switch_dispatch(
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
pub(super) fn should_log_tui_key_event(key: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};

    !matches!(key.code, KeyCode::Char(_)) || key.modifiers != KeyModifiers::NONE
}

#[cfg(feature = "terminal-tui")]
pub(super) fn is_plain_character_key(key: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};

    matches!(key.code, KeyCode::Char(_)) && key.modifiers == KeyModifiers::NONE
}

#[cfg(feature = "terminal-tui")]
pub(super) fn plain_character_from_key(key: &crossterm::event::KeyEvent) -> Option<char> {
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
pub(super) fn sync_key_mirror_observation_state(
    render_source: &RenderSource,
    mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
) {
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
pub(super) fn spawn_tui_unified_loop(
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
pub(super) fn collect_at_path_candidates(
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

/// Inner body of [`spawn_tui_unified_loop`].
///
/// **Fullscreen architecture.** ratatui owns the alternate screen and redraws
/// the transcript pane plus pinned bottom chrome as one full frame. Native
/// terminal scrollback is intentionally not used; `/export` is the durable
/// transcript escape hatch.
#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
pub(super) fn run_tui_unified_loop(
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

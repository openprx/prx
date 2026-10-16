//! Transcript, diff and provider-worker views and attached-session projection.

use super::dispatcher;
#[cfg(feature = "terminal-tui")]
use super::tui;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

#[cfg(feature = "terminal-tui")]
pub(super) fn attach_command_for_seq(seq: u64) -> String {
    format!("/attach {seq}")
}

#[cfg(feature = "terminal-tui")]
pub(super) const TRANSCRIPT_COMMAND: &str = "/transcript";

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
pub(super) fn apply_optimistic_focus(
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
pub(super) struct ActiveSessionAttachProjection {
    pub(super) view: crate::chat::sessions::ActiveSessionView,
    pub(super) breadcrumb: String,
}

#[cfg(feature = "terminal-tui")]
pub(super) const fn attach_breadcrumb_for_transition(
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
pub(super) fn build_active_session_attach_projection(
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
pub(super) fn active_session_view_from_ring(
    mut current: crate::chat::sessions::ActiveSessionView,
    ring: &crate::chat::sessions::SessionRing,
) -> crate::chat::sessions::ActiveSessionView {
    current.lines = ring.recent_lines(crate::chat::sessions::event::DEFAULT_RING_CAPACITY);
    current.truncated = ring.is_truncated();
    current.clamped_for_height(usize::from(tui::ACTIVE_SESSION_VIEW_DESIRED_ROWS))
}

#[cfg(feature = "terminal-tui")]
pub(super) fn refresh_attached_session_view_from_ring(
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
pub(super) fn scroll_active_session_view(
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
pub(super) struct DiffViewSource {
    pub(super) title: String,
    pub(super) lines: Vec<String>,
    pub(super) truncated: bool,
}

impl DiffViewSource {
    pub(super) fn to_plain_text(&self) -> String {
        self.lines.join("\n")
    }
}

pub(super) fn diff_command_args(cached: bool) -> Vec<&'static str> {
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

pub(super) fn bounded_diff_lines(raw: &str, max_bytes: usize, max_lines: usize) -> (Vec<String>, bool) {
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

pub(super) fn git_diff_error_line(stderr: &[u8], stdout: &[u8]) -> String {
    let message = if stderr.is_empty() { stdout } else { stderr };
    let text = truncate_utf8_lossy_bytes(message, DIFF_ERROR_MAX_BYTES);
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .map_or("git diff failed", |line| line);
    format!("diff unavailable: {first}")
}

pub(super) struct BoundedDiffOutput {
    pub(super) success: bool,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    pub(super) truncated: bool,
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

pub(super) async fn collect_workspace_diff(workspace_dir: &std::path::Path, cached: bool) -> DiffViewSource {
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
pub(super) fn open_transcript_view(
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
pub(super) fn close_transcript_view(
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
pub(super) fn open_diff_view(
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
pub(super) fn close_diff_view(
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
pub(super) fn open_provider_worker_view(
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
pub(super) fn close_provider_worker_view(
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

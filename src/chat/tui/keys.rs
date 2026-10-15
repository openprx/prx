use super::TuiState;
use super::render_chrome::cycle_chat_mode;
use super::slash_menu::{SlashMenuSources, dispatch_slash_menu_key_with_sources};
use super::views::{
    focus_active_entry_seq, is_provider_worker_switcher_entry, provider_worker_sequence_from_switcher_seq,
    provider_worker_switcher_entries, switcher_entries_with_transcript_and_workers,
};
use crate::agent::turn_contracts::ChatMode;
use crate::chat::action::ProviderWorkerStatus;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Outcome of [`TuiInput::handle_key`].
///
/// Designed so the surrounding event loop can react with a single match
/// without inspecting `TuiInput` internals.
///
/// PageUp / PageDown fall through as [`InputOutcome::Unhandled`] here so the
/// outer fullscreen event loop can decide whether transcript scrolling or a
/// focused child view owns them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputOutcome {
    /// Key was consumed; no externally observable change beyond the buffer.
    Consumed,
    /// Key was ignored because it would not change the buffer.
    Ignored,
    /// User pressed Enter on a non-empty buffer. The full multi-line text is
    /// returned, the buffer has been cleared, and history advanced.
    Submitted(String),
    /// User requested cancellation (Esc). Buffer was cleared if non-empty.
    Cancelled,
    /// Key was not handled by the input subsystem; caller should fall through
    /// to higher-level shortcuts (`Tab`, `Ctrl+C`, `Ctrl+D`, etc.).
    Unhandled,
}

/// Top-level dispatch outcome for a single [`KeyEvent`] when it is fed into
/// the chat event loop via [`dispatch_global_key`].
///
/// This is a pure-function projection over the global shortcut table layered
/// on top of [`TuiState::handle_input_key`]:
///
/// - `Tab` — toggles the most recent tool-result card.
/// - `Ctrl+R` — reverse-searches submitted input history.
/// - `Ctrl+X Ctrl+E` — opens the current draft in an external editor.
/// - `Ctrl+C` — interrupt the current turn (caller cancels in-flight work)
/// - `Ctrl+D` — EOF when the input buffer is logically empty
/// - everything else — forwarded to the input box; submissions surface as
///   `Submitted(text)`.
///
/// Keeping the dispatch separate from the actual I/O loop lets us unit-test
/// the keybindings without spinning up a terminal.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyDispatch {
    /// Global shortcut handled or input-only change. The event loop should
    /// re-render but otherwise continue waiting for input.
    Consumed,
    /// Event was intentionally ignored and does not need a redraw.
    Ignored,
    /// User pressed Enter on a non-empty buffer. The full multi-line text is
    /// the value to deliver to the agent pipeline.
    Submitted(String),
    /// User requested cancellation of the current input (Esc).
    Cancelled,
    /// User requested cancellation of the in-flight LLM turn (`Ctrl+C`).
    /// The caller is responsible for firing the `CancellationToken` and
    /// keeping the input loop running so a new prompt can be entered.
    InterruptTurn,
    /// EOF (`Ctrl+D` on an empty buffer) — the event loop should exit.
    Exit,
    /// v1.1b: the Ctrl+G switcher overlay was opened over the supplied session
    /// snapshot. The key loop dispatches `Action::SwitcherOpened` so the render
    /// snapshot reflects it (the mirror was already mutated in place).
    SwitcherOpened {
        entries: Vec<crate::chat::sessions::SwitcherEntry>,
    },
    /// v1.1b: the switcher highlight moved to row `selected`. The key loop
    /// dispatches `Action::SwitcherMoved`.
    SwitcherMoved { selected: usize },
    /// v1.1b: the switcher overlay was closed (Esc / Ctrl+G toggle / after
    /// attach). The key loop dispatches `Action::SwitcherClosed`.
    SwitcherClosed,
    /// P7c: saved chat-session picker moved.
    SavedSessionPickerMoved { selected: usize },
    /// P7c: saved chat-session picker closed.
    SavedSessionPickerClosed,
    /// P7c: resume the selected saved chat session through the main-loop
    /// control channel, not by synthetic slash-command text.
    ResumeSavedSession { id: String },
    /// v1.1b: attach to the given display sequence `#N` (switcher Enter). The
    /// key loop sends a synthetic `/attach <seq>` through the input channel so
    /// the async main loop performs the attach via its existing handler.
    AttachSession { seq: u64 },
    /// v1.1b: detach the focused child session (Esc on empty input while a
    /// session is focused). The key loop sends a synthetic `/detach` through the
    /// input channel so the async main loop performs the detach.
    RequestDetach,
    /// Scroll the main fullscreen transcript one line up from tail.
    ScrollTranscriptUp,
    /// Scroll the main fullscreen transcript one line down toward tail.
    ScrollTranscriptDown,
    /// Page the main fullscreen transcript up from tail.
    PageTranscriptUp,
    /// Page the main fullscreen transcript down toward tail.
    PageTranscriptDown,
    /// Jump the main fullscreen transcript to the oldest visible content.
    TranscriptHome,
    /// Jump the main fullscreen transcript to the tail.
    TranscriptEnd,
    /// P2: scroll the focused child-session viewport one line up from tail.
    ScrollSessionUp,
    /// P2: scroll the focused child-session viewport one line down toward tail.
    ScrollSessionDown,
    /// P2: page the focused child-session viewport up from tail.
    PageSessionUp,
    /// P2: page the focused child-session viewport down toward tail.
    PageSessionDown,
    /// P2: jump the focused child-session viewport to the oldest retained line.
    SessionHome,
    /// P2: jump the focused child-session viewport to the tail.
    SessionEnd,
    /// P3: switch to an adjacent live child session through the single `/attach`
    /// owner path.
    SwitchSession { seq: u64 },
    /// P6b1: open the read-only transcript child TUI.
    OpenTranscriptViewer,
    /// P6b1: close the read-only transcript child TUI.
    CloseTranscriptViewer,
    /// P6Y: open the read-only main provider worker detail TUI.
    OpenProviderWorkerView { sequence: u64 },
    /// P6Y: close the read-only main provider worker detail TUI.
    CloseProviderWorkerView,
    /// P6c2: close the read-only diff child TUI.
    CloseDiffViewer,
    /// P6b2: open the current draft in an external editor.
    ExternalEditorRequested,
    /// P6c1: resolve the foreground tool approval prompt.
    ToolApprovalDecision { tool_id: String, approved: bool },
    /// P8: cycle the in-session chat mode via Shift+Tab.
    ModeChanged(ChatMode),
}

pub(super) const MAIN_SESSION_SELECTION_SEQ: u64 = 0;

pub(super) fn bottom_list_selection_index(
    entries: &[crate::chat::sessions::SwitcherEntry],
    focus: crate::chat::sessions::FocusTarget,
) -> usize {
    focus_active_entry_seq(focus)
        .and_then(|seq| entries.iter().position(|entry| entry.seq == seq))
        .map_or(0, |idx| idx.saturating_add(1))
}

pub(super) fn bottom_list_seq_at(entries: &[crate::chat::sessions::SwitcherEntry], idx: usize) -> Option<u64> {
    if idx == 0 {
        Some(MAIN_SESSION_SELECTION_SEQ)
    } else {
        entries.get(idx.saturating_sub(1)).map(|entry| entry.seq)
    }
}

pub(super) fn move_bottom_list_selection(
    entries: &[crate::chat::sessions::SwitcherEntry],
    focus: crate::chat::sessions::FocusTarget,
    direction: crate::chat::sessions::SessionDirection,
) -> Option<u64> {
    if entries.is_empty() {
        return None;
    }
    let total = entries.len().saturating_add(1);
    let current = bottom_list_selection_index(entries, focus).min(total.saturating_sub(1));
    let target_idx = match direction {
        crate::chat::sessions::SessionDirection::Previous => {
            current.checked_sub(1).unwrap_or_else(|| total.saturating_sub(1))
        }
        crate::chat::sessions::SessionDirection::Next => {
            let next = current.saturating_add(1);
            if next >= total { 0 } else { next }
        }
    };
    bottom_list_seq_at(entries, target_idx)
}

pub(super) fn bottom_chrome_session_entries(
    entries: &[crate::chat::sessions::SwitcherEntry],
    focus: crate::chat::sessions::FocusTarget,
) -> Vec<crate::chat::sessions::SwitcherEntry> {
    let active_seq = focus_active_entry_seq(focus);
    entries
        .iter()
        .filter(|entry| entry.is_transcript() || !entry.is_terminal() || active_seq == Some(entry.seq))
        .cloned()
        .collect()
}

pub(super) fn bottom_chrome_session_entries_with_workers(
    entries: &[crate::chat::sessions::SwitcherEntry],
    workers: ProviderWorkerStatus,
    focus: crate::chat::sessions::FocusTarget,
) -> Vec<crate::chat::sessions::SwitcherEntry> {
    // A single provider worker is the ordinary main-chat turn, not a child
    // session. Rendering it below the input duplicated the transcript's
    // Working/tool feedback and leaked internal details such as `run npx ...`
    // into the composer chrome. Surface worker rows here only when there is
    // real concurrent turn activity, or while the user is explicitly viewing
    // a worker. `/workers` and Ctrl+G keep the full diagnostic inventory.
    let active_workers = workers.rows.iter().filter(|row| row.is_active()).count();
    let mut out = if active_workers > 1 || focus.worker_sequence().is_some() {
        provider_worker_switcher_entries(workers, focus)
    } else {
        Vec::new()
    };
    out.extend(bottom_chrome_session_entries(entries, focus));
    out
}

pub(crate) fn selected_strip_entry<'a>(
    entries: &'a [crate::chat::sessions::SwitcherEntry],
    selected: Option<u64>,
) -> Option<&'a crate::chat::sessions::SwitcherEntry> {
    let seq = selected?;
    entries.iter().find(|entry| entry.seq == seq)
}

pub(super) fn dispatch_bottom_entry_activation(
    entry: &crate::chat::sessions::SwitcherEntry,
    current_focus: crate::chat::sessions::FocusTarget,
) -> KeyDispatch {
    if is_provider_worker_switcher_entry(entry)
        && let Some(sequence) = provider_worker_sequence_from_switcher_seq(entry.seq)
    {
        return KeyDispatch::OpenProviderWorkerView { sequence };
    }
    if current_focus.session_seq() == Some(entry.seq) {
        return KeyDispatch::Consumed;
    }
    if matches!(current_focus, crate::chat::sessions::FocusTarget::Main) {
        KeyDispatch::AttachSession { seq: entry.seq }
    } else {
        KeyDispatch::SwitchSession { seq: entry.seq }
    }
}

pub(super) fn dispatch_main_bottom_switch(
    state: &TuiState,
    direction: crate::chat::sessions::SessionDirection,
) -> KeyDispatch {
    let bottom_entries = bottom_chrome_session_entries_with_workers(
        &state.sessions_cache,
        state.provider_worker_status.clone(),
        state.focus,
    );
    let Some(selected) = move_bottom_list_selection(&bottom_entries, state.focus, direction) else {
        return KeyDispatch::Consumed;
    };
    if selected == MAIN_SESSION_SELECTION_SEQ {
        return KeyDispatch::Consumed;
    }
    selected_strip_entry(&bottom_entries, Some(selected)).map_or(KeyDispatch::Consumed, |entry| {
        dispatch_bottom_entry_activation(entry, state.focus)
    })
}

pub(super) fn dispatch_child_bottom_switch(
    state: &TuiState,
    direction: crate::chat::sessions::SessionDirection,
) -> KeyDispatch {
    let bottom_entries = bottom_chrome_session_entries_with_workers(
        &state.sessions_cache,
        state.provider_worker_status.clone(),
        state.focus,
    );
    let Some(selected) = move_bottom_list_selection(&bottom_entries, state.focus, direction) else {
        return KeyDispatch::Consumed;
    };
    if selected == MAIN_SESSION_SELECTION_SEQ {
        return match state.focus {
            crate::chat::sessions::FocusTarget::Worker { .. } => KeyDispatch::CloseProviderWorkerView,
            crate::chat::sessions::FocusTarget::Transcript => KeyDispatch::CloseTranscriptViewer,
            crate::chat::sessions::FocusTarget::Diff => KeyDispatch::CloseDiffViewer,
            crate::chat::sessions::FocusTarget::Session { .. } => KeyDispatch::RequestDetach,
            crate::chat::sessions::FocusTarget::Main | crate::chat::sessions::FocusTarget::Approval => {
                KeyDispatch::Consumed
            }
        };
    }
    selected_strip_entry(&bottom_entries, Some(selected)).map_or(KeyDispatch::Consumed, |entry| {
        dispatch_bottom_entry_activation(entry, state.focus)
    })
}

/// Resolve a [`KeyEvent`] against the global shortcut table layered above the
/// input box. See [`KeyDispatch`] for the priority order. The function is
/// pure: it consumes a mutable reference to [`TuiState`] only to forward the
/// key into the input buffer and to flip fold flags on tool / reasoning cards.
///
/// Pure on its own, no I/O — kept here so unit tests can exercise the binding
/// table without touching crossterm / ratatui terminals.
pub fn dispatch_global_key(key: KeyEvent, state: &mut TuiState) -> KeyDispatch {
    // [DIAG] trace every key that enters the dispatch function so we can
    // correlate raw events (tui_input_event) with what the handler sees.
    tracing::debug!(
        code = ?key.code,
        modifiers = ?key.modifiers,
        kind = ?key.kind,
        input_lines_before = state.input.lines.len(),
        input_first_line_chars = state.input.lines.first().map(|s| s.chars().count()).unwrap_or(0),
        "dispatch_global_key_entry"
    );
    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
        state.clear_pending_tool_approval();
        return KeyDispatch::InterruptTurn;
    }
    if key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL && state.input.is_empty() {
        return KeyDispatch::Exit;
    }
    // P7c: the saved chat-session picker has top overlay priority. It is
    // distinct from the child-TUI Ctrl+G switcher and captures all keys while
    // open so navigation cannot leak into input history or child switching.
    if state.saved_session_picker.is_some() {
        return dispatch_saved_session_picker_key(key, state);
    }
    if state.slash_menu.is_some() {
        let sources = SlashMenuSources {
            live_sessions: &state.sessions_cache,
            saved_sessions: &state.saved_sessions_cache,
            provider_model_catalog: &state.provider_model_catalog,
            at_path_candidates: &state.at_path_candidates,
            current_provider: &state.provider,
        };
        return dispatch_slash_menu_key_with_sources(&mut state.input, &mut state.slash_menu, key, sources);
    }
    // v1.1b: when the Ctrl+G switcher overlay is open it captures navigation /
    // selection keys before anything else (Tab, input box, etc.). Handled in a
    // dedicated resolver so the open-state key table is self-contained.
    if state.switcher.is_some() {
        return dispatch_switcher_key(key, state);
    }
    if let Some(pending) = state.pending_tool_approval.clone()
        && matches!(state.focus, crate::chat::sessions::FocusTarget::Approval)
    {
        if key.modifiers == KeyModifiers::NONE {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    state.pending_tool_approval = None;
                    state.focus = crate::chat::sessions::FocusTarget::Main;
                    return KeyDispatch::ToolApprovalDecision {
                        tool_id: pending.tool_id,
                        approved: true,
                    };
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    state.pending_tool_approval = None;
                    state.focus = crate::chat::sessions::FocusTarget::Main;
                    return KeyDispatch::ToolApprovalDecision {
                        tool_id: pending.tool_id,
                        approved: false,
                    };
                }
                KeyCode::Left | KeyCode::Up => {
                    if let Some(pending) = state.pending_tool_approval.as_mut() {
                        pending.selected_approval = false;
                    }
                    return KeyDispatch::Consumed;
                }
                KeyCode::Right | KeyCode::Down => {
                    if let Some(pending) = state.pending_tool_approval.as_mut() {
                        pending.selected_approval = true;
                    }
                    return KeyDispatch::Consumed;
                }
                KeyCode::Enter => {
                    state.pending_tool_approval = None;
                    state.focus = crate::chat::sessions::FocusTarget::Main;
                    return KeyDispatch::ToolApprovalDecision {
                        tool_id: pending.tool_id,
                        approved: pending.selected_approval,
                    };
                }
                _ => return KeyDispatch::Consumed,
            }
        }
        return KeyDispatch::Consumed;
    }
    if state.external_editor_prefix_armed {
        state.external_editor_prefix_armed = false;
        if key.code == KeyCode::Char('e') && key.modifiers == KeyModifiers::CONTROL {
            return KeyDispatch::ExternalEditorRequested;
        }
        return KeyDispatch::Consumed;
    }
    if key.code == KeyCode::BackTab
        && (key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT)
        && state.input.is_empty()
    {
        let mode = cycle_chat_mode(state.chat_mode);
        state.chat_mode = mode;
        return KeyDispatch::ModeChanged(mode);
    }
    // v1.1b/P1: Ctrl+G opens the PRX sessions switcher over the cached session
    // list. This intentionally diverges from Claude Code's external-editor
    // Ctrl+G binding until the P3/P6b parity keybinding pass; keep the footer
    // hint discoverable so the temporary divergence is not hidden.
    // Never falls through to the input box. Opening an empty switcher is still
    // valid — it shows the "no child TUI sessions" hint with an Esc to close.
    if key.code == KeyCode::Char('g') && key.modifiers == KeyModifiers::CONTROL {
        let entries = switcher_entries_with_transcript_and_workers(
            &state.sessions_cache,
            state.provider_worker_status.clone(),
            state.focus,
        );
        state.switcher = Some(crate::chat::sessions::SwitcherState::new(entries.clone()));
        return KeyDispatch::SwitcherOpened { entries };
    }
    if key.modifiers == KeyModifiers::NONE && matches!(state.focus, crate::chat::sessions::FocusTarget::Main) {
        match key.code {
            KeyCode::PageUp => return KeyDispatch::PageTranscriptUp,
            KeyCode::PageDown => return KeyDispatch::PageTranscriptDown,
            KeyCode::Home => return KeyDispatch::TranscriptHome,
            KeyCode::End => return KeyDispatch::TranscriptEnd,
            _ => {}
        }
    }
    if key.modifiers == KeyModifiers::NONE && state.focus.is_child_view() {
        match key.code {
            KeyCode::PageUp => return KeyDispatch::PageSessionUp,
            KeyCode::PageDown => return KeyDispatch::PageSessionDown,
            KeyCode::Home => return KeyDispatch::SessionHome,
            KeyCode::End => return KeyDispatch::SessionEnd,
            _ => {}
        }
    }
    if state.input.is_empty()
        && key.modifiers == KeyModifiers::NONE
        && matches!(state.focus, crate::chat::sessions::FocusTarget::Main)
    {
        match key.code {
            KeyCode::Up => return KeyDispatch::ScrollTranscriptUp,
            KeyCode::Down => return KeyDispatch::ScrollTranscriptDown,
            KeyCode::Left if key.kind == KeyEventKind::Repeat => return KeyDispatch::Consumed,
            KeyCode::Right if key.kind == KeyEventKind::Repeat => return KeyDispatch::Consumed,
            KeyCode::Left => {
                return dispatch_main_bottom_switch(state, crate::chat::sessions::SessionDirection::Previous);
            }
            KeyCode::Right => return dispatch_main_bottom_switch(state, crate::chat::sessions::SessionDirection::Next),
            _ => {}
        }
    }
    if key.code == KeyCode::Char('o') && key.modifiers == KeyModifiers::CONTROL {
        return KeyDispatch::OpenTranscriptViewer;
    }
    if key.code == KeyCode::Char('x') && key.modifiers == KeyModifiers::CONTROL {
        state.external_editor_prefix_armed = true;
        return KeyDispatch::Consumed;
    }
    // Tab → toggle the most recent visible tool-result card. Reasoning is
    // retained for the verbose transcript but intentionally hidden from the
    // primary conversation, so it must not intercept this shortcut.
    if key.code == KeyCode::Tab && key.modifiers == KeyModifiers::NONE && state.input.is_empty() {
        let _ = state.toggle_last_foldable_card();
        return KeyDispatch::Consumed;
    }
    // Ctrl+R → reverse-search submitted input history. Never falls through
    // to child steering, transcript scrolling or the input box.
    if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
        let _ = state.input.begin_or_cycle_reverse_search();
        return KeyDispatch::Consumed;
    }
    // Ctrl+D → EOF when the input buffer is empty; otherwise treat as a
    // forward-delete (delegated to the input box via Delete equivalence).
    if key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL {
        if state.input.is_empty() {
            return KeyDispatch::Exit;
        }
        // Non-empty: forward as a normal Delete keystroke so users can still
        // use Ctrl+D as forward-delete inside the buffer.
        let synthetic = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
        let _ = state.handle_input_key(synthetic);
        return KeyDispatch::Consumed;
    }
    if state.focus.is_child_view() && state.input.is_empty() && key.modifiers == KeyModifiers::NONE {
        let direction = match key.code {
            KeyCode::Left => Some(crate::chat::sessions::SessionDirection::Previous),
            KeyCode::Right => Some(crate::chat::sessions::SessionDirection::Next),
            _ => None,
        };
        if let Some(direction) = direction {
            if key.kind == KeyEventKind::Repeat {
                return KeyDispatch::Consumed;
            }
            return dispatch_child_bottom_switch(state, direction);
        }
    }
    if state.focus.is_child_view() && state.input.is_empty() && key.modifiers == KeyModifiers::NONE {
        match key.code {
            KeyCode::Up => return KeyDispatch::ScrollSessionUp,
            KeyCode::Down => return KeyDispatch::ScrollSessionDown,
            _ => {}
        }
    }
    // v1.1b: context-aware Esc. The switcher-open case is already handled above
    // (the early return). Here, with no switcher open, `resolve_esc` decides
    // between the established "non-empty clears input" muscle memory and the new
    // "empty + session focused → detach" behaviour, never weakening the former.
    if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
        use crate::chat::sessions::focus::{EscAction, resolve_esc};
        match resolve_esc(state.input.is_empty(), state.focus, false, state.streaming.is_some()) {
            EscAction::CancelGenerating => return KeyDispatch::InterruptTurn,
            EscAction::ClearInput => {
                // Preserve existing behaviour: clear the buffer, signal cancel.
                let _ = state.handle_input_key(key);
                return KeyDispatch::Cancelled;
            }
            EscAction::RequestDetach => return KeyDispatch::RequestDetach,
            EscAction::CloseTranscript => return KeyDispatch::CloseTranscriptViewer,
            EscAction::DenyApproval => {
                state.pending_tool_approval = None;
                state.focus = crate::chat::sessions::FocusTarget::Main;
                return KeyDispatch::Cancelled;
            }
            EscAction::CloseDiff => return KeyDispatch::CloseDiffViewer,
            EscAction::CloseWorker => return KeyDispatch::CloseProviderWorkerView,
            EscAction::Cancel => {
                // Empty buffer + main focus → unchanged legacy cancel semantics.
                let _ = state.handle_input_key(key);
                return KeyDispatch::Cancelled;
            }
            // Unreachable here (switcher_open=false), but keep the match total.
            EscAction::CloseSwitcher => return KeyDispatch::Cancelled,
        }
    }
    if matches!(
        state.focus,
        crate::chat::sessions::FocusTarget::Transcript
            | crate::chat::sessions::FocusTarget::Diff
            | crate::chat::sessions::FocusTarget::Worker { .. }
    ) && key.code == KeyCode::Enter
        && key.modifiers == KeyModifiers::NONE
    {
        return KeyDispatch::Consumed;
    }
    // All other keys → input box.
    match state.handle_input_key(key) {
        InputOutcome::Submitted(text) => KeyDispatch::Submitted(text),
        InputOutcome::Cancelled => KeyDispatch::Cancelled,
        InputOutcome::Consumed | InputOutcome::Unhandled => KeyDispatch::Consumed,
        InputOutcome::Ignored => KeyDispatch::Ignored,
    }
}

/// Resolve a key while the Ctrl+G session switcher overlay is open (v1.1b).
///
/// The overlay captures navigation + selection keys; everything else is a
/// no-op `Consumed` so stray keystrokes do not leak into the input box or fire
/// global shortcuts while the popup has focus. Mutates the mirror switcher in
/// place and returns the matching [`KeyDispatch`] so the key loop can mirror the
/// change into the render snapshot.
pub(super) fn dispatch_switcher_key(key: KeyEvent, state: &mut TuiState) -> KeyDispatch {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // Up / Ctrl+P → previous; Down / Ctrl+N → next.
    let up = key.code == KeyCode::Up || (ctrl && key.code == KeyCode::Char('p'));
    let down = key.code == KeyCode::Down || (ctrl && key.code == KeyCode::Char('n'));
    if up || down {
        let Some(switcher) = state.switcher.as_mut() else {
            return KeyDispatch::Consumed;
        };
        if up {
            switcher.select_prev();
        } else {
            switcher.select_next();
        }
        return KeyDispatch::SwitcherMoved {
            selected: switcher.selected,
        };
    }
    // Enter → attach the highlighted session, then close. Empty list → just close.
    if key.code == KeyCode::Enter {
        let selected = state.switcher.as_ref().and_then(|s| s.selected_entry().cloned());
        state.switcher = None;
        return selected.map_or(KeyDispatch::SwitcherClosed, |entry| {
            if entry.is_transcript() {
                return KeyDispatch::OpenTranscriptViewer;
            }
            if is_provider_worker_switcher_entry(&entry) {
                return provider_worker_sequence_from_switcher_seq(entry.seq)
                    .map_or(KeyDispatch::SwitcherClosed, |sequence| {
                        KeyDispatch::OpenProviderWorkerView { sequence }
                    });
            }
            // The key loop closes the snapshot switcher *and* sends the synthetic
            // `/attach`, so signal the attach here; the close rides along.
            KeyDispatch::AttachSession { seq: entry.seq }
        });
    }
    // Esc → close the switcher (resolve_esc gives CloseSwitcher when open).
    // Ctrl+G → toggle closed. Both just close.
    if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('g')) {
        state.switcher = None;
        return KeyDispatch::SwitcherClosed;
    }
    // Any other key is swallowed while the overlay has focus.
    KeyDispatch::Consumed
}

/// Resolve a key while the saved chat-session picker overlay is open (P7c).
pub(super) fn dispatch_saved_session_picker_key(key: KeyEvent, state: &mut TuiState) -> KeyDispatch {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let up = key.code == KeyCode::Up || (ctrl && key.code == KeyCode::Char('p'));
    let down = key.code == KeyCode::Down || (ctrl && key.code == KeyCode::Char('n'));
    if up || down {
        let Some(picker) = state.saved_session_picker.as_mut() else {
            return KeyDispatch::Consumed;
        };
        if up {
            picker.select_prev();
        } else {
            picker.select_next();
        }
        picker.clamp_selected();
        return KeyDispatch::SavedSessionPickerMoved {
            selected: picker.selected,
        };
    }
    if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE {
        let selected = state.saved_session_picker.as_mut().and_then(|picker| {
            picker.clamp_selected();
            picker.selected_entry().cloned()
        });
        state.saved_session_picker = None;
        return selected.map_or(KeyDispatch::SavedSessionPickerClosed, |entry| {
            KeyDispatch::ResumeSavedSession { id: entry.id }
        });
    }
    if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
        state.saved_session_picker = None;
        return KeyDispatch::SavedSessionPickerClosed;
    }
    KeyDispatch::Consumed
}

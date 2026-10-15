//! Reducer arms for status updates, session switcher, saved-session picker and slash menu sources.

use super::{ChatState, Effect};
use crate::chat::action::{MainQueueStatus, ProviderWorkerStatus};
use crate::chat::slash_types::AtPathCandidate;

impl ChatState {
    /// `Action::SystemMessageAdded` — append one system message to the Redux UI mirror.
    ///
    /// S2-C Step 2: dual-writes with legacy `chat_mirror.lock().push_system_message(text)`.
    /// The reducer keeps its own ConversationLine::System inside `ui.conversation_lines` so the Redux
    /// path has its own UI ledger; the visible TUI is still rendered by `chat_mirror`, and this reducer
    /// does not replace the mirror (mod.rs still writes chat_mirror unconditionally).
    ///
    /// Without the terminal-tui feature it only emits RequestRedraw (the String placeholder carries no
    /// semantics), symmetric with the other TUI-only push functions (user/assistant).
    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_system_message_added(&mut self, text: String) -> Vec<Effect> {
        self.ui
            .conversation_lines
            .push(crate::chat::tui::ConversationLine::System { content: text });
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(super) fn reduce_system_message_added(&mut self, _text: String) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// `Action::UserMessageEchoed` — visual echo of the user's submission in Pure mode
    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_user_message_echoed(&mut self, text: String) -> Vec<Effect> {
        self.ui
            .conversation_lines
            .push(crate::chat::tui::ConversationLine::User { content: text });
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(super) fn reduce_user_message_echoed(&mut self, _text: String) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// `Action::SessionsStatusUpdated` — replace the persistent background-session
    /// status line (v1b). The main loop already dedups (only dispatches when the
    /// summary changed), but we still no-op an identical write so a stray
    /// duplicate cannot mark the UI dirty for nothing.
    pub(super) fn reduce_sessions_status_updated(&mut self, summary: String) -> Vec<Effect> {
        if self.ui.sessions_status == summary {
            return Vec::new();
        }
        self.ui.sessions_status = summary;
        vec![Effect::RequestRedraw]
    }

    /// `Action::SessionsEntriesUpdated` — replace the structured child-session
    /// entries that back the P1 bottom strip. Identical snapshots are no-ops so
    /// the 1s session poll cannot churn redraws when nothing changed.
    pub(super) fn reduce_sessions_entries_updated(
        &mut self,
        entries: Vec<crate::chat::sessions::SwitcherEntry>,
    ) -> Vec<Effect> {
        if self.ui.sessions_entries == entries {
            return Vec::new();
        }
        self.ui.sessions_entries = entries;
        vec![Effect::RequestRedraw]
    }

    pub(super) fn reduce_main_queue_status_updated(&mut self, status: MainQueueStatus) -> Vec<Effect> {
        if self.ui.main_queue_status == status {
            return Vec::new();
        }
        self.ui.main_queue_status = status;
        vec![Effect::RequestRedraw]
    }

    pub(super) fn reduce_provider_worker_status_updated(&mut self, status: ProviderWorkerStatus) -> Vec<Effect> {
        #[cfg(feature = "terminal-tui")]
        let status = {
            let mut status = status;
            if let Some(summary) = crate::chat::tui::latest_provider_worker_tool_summary_from_conversation(
                &self.ui.conversation_lines,
                self.ui.ascii_fallback,
            ) {
                for row in &mut status.rows {
                    if row.is_active() {
                        row.recent_tool_call = Some(summary.clone());
                    }
                }
            }
            status
        };
        if self.ui.provider_worker_status == status {
            return Vec::new();
        }
        let worker_view = self.ui.focus.worker_sequence().map(|sequence| {
            let previous_view = self
                .ui
                .active_session_view
                .as_ref()
                .filter(|view| view.kind == crate::chat::action::PROVIDER_WORKER_VIEW_KIND && view.seq == sequence);
            #[cfg(feature = "terminal-tui")]
            let io_lines = crate::chat::tui::provider_worker_io_lines_for_streaming_draft(
                &self.ui.conversation_lines,
                self.stream.streaming_draft_for_worker(sequence),
                12,
            );
            #[cfg(not(feature = "terminal-tui"))]
            let io_lines = Vec::new();
            crate::chat::action::build_provider_worker_active_view_with_io_preserving_scroll(
                &status,
                sequence,
                previous_view,
                io_lines,
            )
        });
        self.ui.provider_worker_status = status;
        if let Some(view) = worker_view {
            self.ui.active_session_view = Some(view);
        }
        vec![Effect::RequestRedraw]
    }

    #[cfg(feature = "terminal-tui")]
    pub(super) fn refresh_provider_worker_view_if_focused(&mut self) {
        let Some(sequence) = self.ui.focus.worker_sequence() else {
            return;
        };
        let previous_view = self
            .ui
            .active_session_view
            .as_ref()
            .filter(|view| view.kind == crate::chat::action::PROVIDER_WORKER_VIEW_KIND && view.seq == sequence);
        let io_lines = crate::chat::tui::provider_worker_io_lines_for_streaming_draft(
            &self.ui.conversation_lines,
            self.stream.streaming_draft_for_worker(sequence),
            12,
        );
        self.ui.active_session_view = Some(
            crate::chat::action::build_provider_worker_active_view_with_io_preserving_scroll(
                &self.ui.provider_worker_status,
                sequence,
                previous_view,
                io_lines,
            ),
        );
    }

    #[cfg(not(feature = "terminal-tui"))]
    pub(super) const fn refresh_provider_worker_view_if_focused(&self) {}

    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_slash_menu_sources_updated(
        &mut self,
        saved_sessions: Vec<crate::chat::session::SavedSessionPickerEntry>,
        provider_model_catalog: Vec<crate::chat::slash_types::SlashProviderModelCatalog>,
    ) -> Vec<Effect> {
        if self.ui.saved_sessions_cache == saved_sessions && self.ui.provider_model_catalog == provider_model_catalog {
            return Vec::new();
        }
        self.ui.saved_sessions_cache = saved_sessions;
        self.ui.provider_model_catalog = provider_model_catalog;
        if self.ui.slash_menu.is_some() {
            let sources = Self::slash_menu_sources_from(
                &self.ui.sessions_entries,
                &self.ui.saved_sessions_cache,
                &self.ui.provider_model_catalog,
                &self.ui.at_path_candidates,
                self.session.provider.as_ref(),
            );
            crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
        }
        vec![Effect::RequestRedraw]
    }

    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_at_path_candidates_updated(&mut self, candidates: Vec<AtPathCandidate>) -> Vec<Effect> {
        if self.ui.at_path_candidates == candidates {
            return Vec::new();
        }
        self.ui.at_path_candidates = candidates;
        let sources = Self::slash_menu_sources_from(
            &self.ui.sessions_entries,
            &self.ui.saved_sessions_cache,
            &self.ui.provider_model_catalog,
            &self.ui.at_path_candidates,
            self.session.provider.as_ref(),
        );
        crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    pub(super) fn reduce_at_path_candidates_updated(&self, _candidates: Vec<AtPathCandidate>) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    pub(super) fn reduce_slash_menu_sources_updated(
        &self,
        _saved_sessions: Vec<crate::chat::session::SavedSessionPickerEntry>,
        _provider_model_catalog: Vec<crate::chat::slash_types::SlashProviderModelCatalog>,
    ) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// `Action::SessionFocusChanged` (v1.1b) — record the current input-routing
    /// target so the snapshot prompt indicator (colour+glyph) reflects it.
    /// Idempotent: an unchanged focus is a no-op (no needless redraw).
    pub(super) fn reduce_session_focus_changed(&mut self, focus: crate::chat::sessions::FocusTarget) -> Vec<Effect> {
        if self.ui.focus == focus {
            return Vec::new();
        }
        self.ui.focus = focus;
        vec![Effect::RequestRedraw]
    }

    /// `Action::SwitcherOpened` (v1.1b) — open the Ctrl+G switcher overlay over
    /// the supplied session snapshot, highlighting the first row.
    pub(super) fn reduce_switcher_opened(&mut self, entries: Vec<crate::chat::sessions::SwitcherEntry>) -> Vec<Effect> {
        self.ui.saved_session_picker = None;
        self.ui.slash_menu = None;
        self.ui.switcher = Some(crate::chat::sessions::SwitcherState::new(entries));
        vec![Effect::RequestRedraw]
    }

    /// `Action::SwitcherMoved` (v1.1b) — update the highlighted row. The index is
    /// clamped to a valid row by the key thread; we clamp again defensively so a
    /// stale snapshot can never index out of range. No-op (no redraw) when the
    /// switcher is closed or the selection is unchanged.
    pub(super) fn reduce_switcher_moved(&mut self, selected: usize) -> Vec<Effect> {
        let Some(switcher) = self.ui.switcher.as_mut() else {
            return Vec::new();
        };
        let clamped = if switcher.entries.is_empty() {
            0
        } else {
            selected.min(switcher.entries.len().saturating_sub(1))
        };
        if switcher.selected == clamped {
            return Vec::new();
        }
        switcher.selected = clamped;
        vec![Effect::RequestRedraw]
    }

    /// `Action::SwitcherClosed` (v1.1b) — close the switcher overlay. No-op (no
    /// redraw) when already closed.
    pub(super) fn reduce_switcher_closed(&mut self) -> Vec<Effect> {
        if self.ui.switcher.is_none() {
            return Vec::new();
        }
        self.ui.switcher = None;
        vec![Effect::RequestRedraw]
    }

    /// `Action::SavedSessionPickerOpened` (P7c) — open the saved chat-session
    /// history picker, separate from the child-TUI Ctrl+G switcher.
    pub(super) fn reduce_saved_session_picker_opened(
        &mut self,
        entries: Vec<crate::chat::session::SavedSessionPickerEntry>,
    ) -> Vec<Effect> {
        self.ui.switcher = None;
        self.ui.slash_menu = None;
        self.ui.saved_session_picker = Some(crate::chat::session::SavedSessionPickerState::new(entries));
        vec![Effect::RequestRedraw]
    }

    pub(super) fn reduce_saved_session_picker_moved(&mut self, selected: usize) -> Vec<Effect> {
        let Some(picker) = self.ui.saved_session_picker.as_mut() else {
            return Vec::new();
        };
        let clamped = if picker.entries.is_empty() {
            0
        } else {
            selected.min(picker.entries.len().saturating_sub(1))
        };
        if picker.selected == clamped {
            return Vec::new();
        }
        picker.selected = clamped;
        vec![Effect::RequestRedraw]
    }

    pub(super) fn reduce_saved_session_picker_closed(&mut self) -> Vec<Effect> {
        if self.ui.saved_session_picker.is_none() {
            return Vec::new();
        }
        self.ui.saved_session_picker = None;
        vec![Effect::RequestRedraw]
    }

    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_saved_session_picker_key_pressed(&mut self, key: crossterm::event::KeyEvent) -> Vec<Effect> {
        use crossterm::event::{KeyCode, KeyModifiers};

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let up = key.code == KeyCode::Up || (ctrl && key.code == KeyCode::Char('p'));
        let down = key.code == KeyCode::Down || (ctrl && key.code == KeyCode::Char('n'));
        if up || down {
            let Some(picker) = self.ui.saved_session_picker.as_mut() else {
                return Vec::new();
            };
            if up {
                picker.select_prev();
            } else {
                picker.select_next();
            }
            picker.clamp_selected();
            return vec![Effect::RequestRedraw];
        }
        if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE {
            return self.reduce_saved_session_picker_closed();
        }
        if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
            return self.reduce_saved_session_picker_closed();
        }
        Vec::new()
    }
}

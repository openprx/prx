//! Reducer arms for keyboard, paste and input-buffer actions.

#[cfg(feature = "terminal-tui")]
use super::DOUBLE_CTRLC_WINDOW_MS;
use super::{ChatState, Effect, ToolTaskKey};
use crate::chat::action::HistoryDir;

impl ChatState {
    // ── Input-path helpers (Step 2) ────────────────────────────────────────────

    /// Handle `KeyPressed`: dispatch the key to the input buffer / global shortcuts / exit semantics.
    ///
    /// This is essentially the reducer version of `tui::dispatch_global_key`, but it acts on
    /// `UiState.input` instead of `TuiState`. Returns a list of Effects (typically just RequestRedraw).
    /// Actually posting the user message to the channel or firing cancel is still done by the main loop.
    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_key_pressed(&mut self, key: crossterm::event::KeyEvent, now_ms: u64) -> Vec<Effect> {
        use crossterm::event::{KeyCode, KeyModifiers};

        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            let prev = self.ui.last_ctrlc_ms;
            self.ui.last_ctrlc_ms = now_ms;
            if prev != 0 && now_ms.saturating_sub(prev) < DOUBLE_CTRLC_WINDOW_MS {
                return vec![Effect::Quit];
            }
            return self.reduce_cancel_requested();
        }
        if key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL && self.ui.input.is_empty() {
            return vec![Effect::Quit];
        }
        if self.ui.saved_session_picker.is_some() {
            return self.reduce_saved_session_picker_key_pressed(key);
        }
        if self.ui.switcher.is_some() {
            if key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return self.reduce_switcher_closed();
            }
            return vec![Effect::RequestRedraw];
        }
        if self.ui.pending_tool_approval.is_some()
            || matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval)
        {
            return self.reduce_approval_key_pressed(key);
        }

        if self.ui.slash_menu.is_some() {
            let sources = Self::slash_menu_sources_from(
                &self.ui.sessions_entries,
                &self.ui.saved_sessions_cache,
                &self.ui.provider_model_catalog,
                &self.ui.at_path_candidates,
                self.session.provider.as_ref(),
            );
            let dispatch = crate::chat::tui::dispatch_slash_menu_key_with_sources(
                &mut self.ui.input,
                &mut self.ui.slash_menu,
                key,
                sources,
            );
            return match dispatch {
                crate::chat::tui::KeyDispatch::Submitted(text) => self.reduce_input_submitted(text),
                crate::chat::tui::KeyDispatch::Cancelled => self.reduce_input_cancelled(),
                crate::chat::tui::KeyDispatch::Ignored => Vec::new(),
                _ => vec![Effect::RequestRedraw],
            };
        }

        if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE && self.control.generating {
            return self.reduce_cancel_requested();
        }

        // Tab → fold/unfold the most recent visible ToolResult card. Reasoning only lives in the
        // verbose transcript and must not intercept Tab in the main conversation.
        if key.code == KeyCode::Tab && key.modifiers == KeyModifiers::NONE && self.ui.input.is_empty() {
            return self.reduce_foldable_card_toggled();
        }
        // Ctrl+R → reverse-search submitted input history. Tab is the sole
        // fold binding after P6b2.
        if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
            let _ = self.ui.input.begin_or_cycle_reverse_search();
            return vec![Effect::RequestRedraw];
        }
        // Ctrl+L → request a redraw; actually clearing the host terminal is up to the effect executor
        if key.code == KeyCode::Char('l') && key.modifiers == KeyModifiers::CONTROL {
            return vec![Effect::RequestRedraw];
        }
        // Ctrl+D → exit on an empty buffer / forward-delete otherwise (delegated to handle_key)
        if key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL {
            // Non-empty buffer: forward as Delete
            let synthetic = crossterm::event::KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
            let _ = self.ui.input.handle_key(synthetic);
            let sources = Self::slash_menu_sources_from(
                &self.ui.sessions_entries,
                &self.ui.saved_sessions_cache,
                &self.ui.provider_model_catalog,
                &self.ui.at_path_candidates,
                self.session.provider.as_ref(),
            );
            crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
            return vec![Effect::RequestRedraw];
        }
        // Other keys → forward to the input buffer, then re-enter with the Action from InputOutcome
        match self.ui.input.handle_key(key) {
            crate::chat::tui::InputOutcome::Submitted(text) => {
                self.ui.slash_menu = None;
                // Re-enter through reduce_with_now to keep a single handling path
                self.reduce_input_submitted(text)
            }
            crate::chat::tui::InputOutcome::Cancelled => {
                self.ui.slash_menu = None;
                self.reduce_input_cancelled()
            }
            crate::chat::tui::InputOutcome::Consumed | crate::chat::tui::InputOutcome::Unhandled => {
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
            crate::chat::tui::InputOutcome::Ignored => Vec::new(),
        }
    }

    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_approval_key_pressed(&mut self, key: crossterm::event::KeyEvent) -> Vec<Effect> {
        use crossterm::event::{KeyCode, KeyModifiers};
        let Some(pending) = self.ui.pending_tool_approval.clone() else {
            if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
                if matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
                    self.ui.focus = crate::chat::sessions::FocusTarget::Main;
                }
                return vec![Effect::RequestRedraw];
            }
            return vec![Effect::RequestRedraw];
        };
        if key.modifiers != KeyModifiers::NONE {
            return vec![Effect::RequestRedraw];
        }
        let approved = match key.code {
            KeyCode::Char('y' | 'Y') => Some(true),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(false),
            KeyCode::Enter => Some(pending.selected_approval),
            KeyCode::Left | KeyCode::Up => {
                if let Some(pending) = self.ui.pending_tool_approval.as_mut() {
                    pending.selected_approval = false;
                }
                return vec![Effect::RequestRedraw];
            }
            KeyCode::Right | KeyCode::Down => {
                if let Some(pending) = self.ui.pending_tool_approval.as_mut() {
                    pending.selected_approval = true;
                }
                return vec![Effect::RequestRedraw];
            }
            _ => None,
        };
        let Some(approved) = approved else {
            return vec![Effect::RequestRedraw];
        };
        self.ui.pending_tool_approval = None;
        if matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
            self.ui.focus = crate::chat::sessions::FocusTarget::Main;
        }
        vec![
            Effect::ResolveApproval {
                tool_id: pending.tool_id,
                approved,
            },
            Effect::RequestRedraw,
        ]
    }

    /// Placeholder without the terminal-tui feature (KeyEvent only exists when crossterm is there)
    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut, clippy::missing_const_for_fn)]
    pub(super) fn reduce_key_pressed(&mut self, _key: crossterm::event::KeyEvent, _now_ms: u64) -> Vec<Effect> {
        let _ = &self.ui;
        vec![]
    }

    /// Handle a bracketed paste: insert the text into the input buffer.
    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_paste_received(&mut self, text: &str) -> Vec<Effect> {
        if self.ui.pending_tool_approval.is_some()
            || matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval)
        {
            return vec![Effect::RequestRedraw];
        }
        self.ui.input.paste(text);
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
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(super) fn reduce_paste_received(&mut self, _text: &str) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// Handle a user submission — UI-side bookkeeping only (turn_count + last_submitted).
    ///
    /// Step 2 does not trigger the LLM (Step 3 adds `Effect::StartTurn`).
    /// `LogTrace` is used for dual-write reconciliation.
    pub(super) fn reduce_input_submitted(&mut self, text: String) -> Vec<Effect> {
        self.ui.slash_menu = None;
        self.ui.turn_count = self.ui.turn_count.saturating_add(1);
        let log_msg = format!("input_submitted len={}", text.chars().count());
        self.ui.last_submitted = Some(text);
        self.ui.input.clear();
        // Legacy main-turn entry only clears the Primary tool bucket; keyed
        // worker buckets must survive until their own terminal event.
        self.control.clear_tool_buffer(ToolTaskKey::Primary);
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: log_msg,
            },
            Effect::RequestRedraw,
        ]
    }

    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_input_replaced(&mut self, text: &str) -> Vec<Effect> {
        self.ui.input.set_text(text);
        self.ui.input.clear_navigation_state();
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
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(super) fn reduce_input_replaced(&mut self, _text: &str) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// Handle Up/Down history navigation.
    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_history_navigated(&mut self, dir: HistoryDir) -> Vec<Effect> {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = match dir {
            HistoryDir::Up => KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            HistoryDir::Down => KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        };
        let _ = self.ui.input.handle_key(key);
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
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(super) fn reduce_history_navigated(&mut self, _dir: HistoryDir) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// Handle Esc — clear the input buffer.
    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_input_cancelled(&mut self) -> Vec<Effect> {
        self.ui.slash_menu = None;
        self.ui.input.clear();
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(super) fn reduce_input_cancelled(&mut self) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// Handle Tab — fold/unfold the most recent visible ToolResult card.
    ///
    /// Reasoning data still lives in `conversation_lines` for the verbose transcript, but the main
    /// conversation no longer renders it, so it does not take part in Tab folding either.
    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_foldable_card_toggled(&mut self) -> Vec<Effect> {
        use crate::chat::tui::ConversationLine;
        let mut toggled = false;
        for line in self.ui.conversation_lines.iter_mut().rev() {
            if let ConversationLine::ToolResult { folded, .. } = line {
                *folded = !*folded;
                toggled = true;
                break;
            }
        }
        // A fold toggle must still mark the conversation as changed so the
        // snapshot/repaint path observes the new fold state. Only bump when a
        // card was toggled to avoid spurious redraw work on no-op key presses.
        if toggled {
            self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        }
        vec![Effect::RequestRedraw]
    }

    /// Handle Tab — fold/unfold the most recent ToolResult.
    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_tool_card_fold_toggled(&mut self) -> Vec<Effect> {
        use crate::chat::tui::ConversationLine;
        let mut toggled = false;
        for line in self.ui.conversation_lines.iter_mut().rev() {
            if let ConversationLine::ToolResult { folded, .. } = line {
                *folded = !*folded;
                toggled = true;
                break;
            }
        }
        // BUG-01 round-2 fix: re-emit scrollback so the new fold state is visible
        // (see `reduce_foldable_card_toggled` for the full rationale).
        if toggled {
            self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        }
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(super) fn reduce_tool_card_fold_toggled(&mut self) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// Handle Ctrl+R — fold/unfold the most recent Reasoning.
    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_reasoning_fold_toggled(&mut self) -> Vec<Effect> {
        use crate::chat::tui::ConversationLine;
        let mut toggled = false;
        for line in self.ui.conversation_lines.iter_mut().rev() {
            if let ConversationLine::Reasoning { folded, .. } = line {
                *folded = !*folded;
                toggled = true;
                break;
            }
        }
        // BUG-01 round-2 fix: re-emit scrollback so the new fold state is visible
        // (see `reduce_foldable_card_toggled` for the full rationale).
        if toggled {
            self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        }
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(super) fn reduce_reasoning_fold_toggled(&mut self) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }
}

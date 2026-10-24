//! Reducer arms for tool cards, tool approvals and turn cancellation.

use super::{ChatState, Effect, ToolInvocationKey, ToolTaskKey};

impl ChatState {
    #[cfg(feature = "terminal-tui")]
    pub(super) fn remove_pending_tool_cards(&mut self, key: ToolTaskKey) {
        use crate::chat::tui::{ConversationLine, ToolStatus};

        let mut indices = self
            .control
            .tool_buffers
            .get_mut(&key)
            .map(|buffer| std::mem::take(&mut buffer.pending_tool_cards))
            .unwrap_or_default();
        indices.sort_unstable_by(|a, b| b.cmp(a));
        indices.dedup();
        for idx in indices {
            if matches!(
                self.ui.conversation_lines.get(idx),
                Some(ConversationLine::ToolResult {
                    status: ToolStatus::Running,
                    ..
                })
            ) {
                self.ui.conversation_lines.remove(idx);
            }
        }
    }

    #[cfg(not(feature = "terminal-tui"))]
    pub(super) fn remove_pending_tool_cards(&mut self, key: ToolTaskKey) {
        if let Some(buffer) = self.control.tool_buffers.get_mut(&key) {
            buffer.pending_tool_cards.clear();
        }
    }

    #[cfg(feature = "terminal-tui")]
    pub(super) fn finalize_pending_tool_cards(
        &mut self,
        key: ToolTaskKey,
        cancelled: bool,
        fallback_result: Option<&'static str>,
    ) {
        use crate::chat::tui::{ConversationLine, ToolStatus};

        let Some(buffer) = self.control.tool_buffers.get_mut(&key) else {
            return;
        };
        for idx in buffer.pending_tool_cards.drain(..) {
            let Some(ConversationLine::ToolResult {
                status,
                result,
                elapsed_ms,
                ..
            }) = self.ui.conversation_lines.get_mut(idx)
            else {
                continue;
            };
            if *status != ToolStatus::Running {
                continue;
            }
            *status = if cancelled {
                ToolStatus::Cancelled
            } else {
                ToolStatus::Error
            };
            if result.is_none()
                && let Some(fallback_result) = fallback_result
            {
                *result = Some(fallback_result.to_string());
            }
            if elapsed_ms.is_none() {
                *elapsed_ms = Some(0);
            }
        }
    }

    #[cfg(not(feature = "terminal-tui"))]
    pub(super) fn finalize_pending_tool_cards(
        &mut self,
        key: ToolTaskKey,
        _cancelled: bool,
        _fallback_result: Option<&'static str>,
    ) {
        if let Some(buffer) = self.control.tool_buffers.get_mut(&key) {
            buffer.pending_tool_cards.clear();
        }
    }

    /// `Action::ToolStarted` — append a ToolResult card in Running state + record its index.
    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_tool_started(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        _sequence: Option<u64>,
        tool_call_id: Option<String>,
        name: String,
        args: String,
    ) -> Vec<Effect> {
        use crate::chat::tui::{
            ARGS_PREVIEW_ELLIPSIS, ARGS_PREVIEW_MAX_CHARS, ConversationLine, ToolStatus, build_tool_args_preview,
            tool_result_defaults_expanded,
        };
        let args_preview = build_tool_args_preview(&name, &args, ARGS_PREVIEW_MAX_CHARS, ARGS_PREVIEW_ELLIPSIS);
        let folded = !tool_result_defaults_expanded(&name);
        let tool_key = ToolTaskKey::from_task_id(task_id);
        let invocation_key = ToolInvocationKey::new(tool_call_id, &name);
        self.control
            .tool_buffer_mut(tool_key)
            .tool_args
            .insert(invocation_key, args_preview.clone());
        self.ui.conversation_lines.push(ConversationLine::ToolResult {
            tool_name: name,
            args_preview,
            args_full: args,
            result: None,
            status: ToolStatus::Running,
            elapsed_ms: None,
            folded,
        });
        let idx = self.ui.conversation_lines.len().saturating_sub(1);
        self.control.tool_buffer_mut(tool_key).pending_tool_cards.push(idx);
        self.refresh_provider_worker_view_if_focused();
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    pub(super) fn reduce_tool_started(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        _sequence: Option<u64>,
        tool_call_id: Option<String>,
        name: String,
        args: String,
    ) -> Vec<Effect> {
        let args_preview = if args.chars().count() > 80 {
            let prefix: String = args.chars().take(80).collect();
            format!("{prefix}…")
        } else {
            args.clone()
        };
        let tool_key = ToolTaskKey::from_task_id(task_id);
        let invocation_key = ToolInvocationKey::new(tool_call_id, &name);
        self.control
            .tool_buffer_mut(tool_key)
            .tool_args
            .insert(invocation_key, args_preview);
        self.ui.conversation_lines.push(format!("tool_started:{name}:{args}"));
        let idx = self.ui.conversation_lines.len().saturating_sub(1);
        self.control.tool_buffer_mut(tool_key).pending_tool_cards.push(idx);
        vec![Effect::RequestRedraw]
    }

    /// `Action::ToolFinished` — update the matching Running card → Done/Error.
    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_tool_finished(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        sequence: Option<u64>,
        tool_call_id: Option<String>,
        name: String,
        success: bool,
        duration_ms: u64,
        result: Option<String>,
    ) -> Vec<Effect> {
        use crate::chat::session::ToolCallSummary;
        use crate::chat::tui::{ConversationLine, ToolStatus};
        let tool_key = ToolTaskKey::from_task_id(task_id);
        let sequence = sequence.or_else(|| self.sequence_for_tool_key(tool_key));
        let invocation_key = ToolInvocationKey::new(tool_call_id, &name);
        let buffer = self.control.tool_buffer_mut(tool_key);
        let args_preview = buffer.tool_args.remove(&invocation_key).unwrap_or_default();
        buffer.tool_calls.push(ToolCallSummary {
            name: name.clone(),
            args_preview,
            success,
            task_id: task_id.map(crate::chat::turn_scheduler::TurnTaskId::get),
            sequence,
        });
        // Step 1: scan pending_tool_cards backwards for the latest Running card with a matching name
        // (it only borrows conversation_lines and holds no mut reference, avoiding a move conflict)
        let target_pos = self.control.tool_buffers.get(&tool_key).and_then(|buffer| {
            buffer
                .pending_tool_cards
                .iter()
                .enumerate()
                .rev()
                .find_map(|(pos, &idx)| match self.ui.conversation_lines.get(idx) {
                    Some(ConversationLine::ToolResult { tool_name, status, .. })
                        if tool_name == &name && *status == ToolStatus::Running =>
                    {
                        Some((pos, idx))
                    }
                    _ => None,
                })
        });
        // Step 2: once the target is found, apply the mut update + remove it from pending
        if let Some((pending_pos, line_idx)) = target_pos {
            if let Some(ConversationLine::ToolResult {
                status,
                elapsed_ms,
                result: result_slot,
                ..
            }) = self.ui.conversation_lines.get_mut(line_idx)
            {
                *status = if success { ToolStatus::Done } else { ToolStatus::Error };
                *elapsed_ms = Some(duration_ms);
                *result_slot = result;
            }
            if let Some(buffer) = self.control.tool_buffers.get_mut(&tool_key) {
                buffer.pending_tool_cards.remove(pending_pos);
            }
        }
        self.refresh_provider_worker_view_if_focused();
        vec![
            Effect::RequestRedraw,
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_finished name={name} success={success} duration_ms={duration_ms}"),
            },
        ]
    }

    #[cfg(not(feature = "terminal-tui"))]
    pub(super) fn reduce_tool_finished(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        sequence: Option<u64>,
        tool_call_id: Option<String>,
        name: String,
        success: bool,
        duration_ms: u64,
        _result: Option<String>,
    ) -> Vec<Effect> {
        use crate::chat::session::ToolCallSummary;
        let tool_key = ToolTaskKey::from_task_id(task_id);
        let sequence = sequence.or_else(|| self.sequence_for_tool_key(tool_key));
        let invocation_key = ToolInvocationKey::new(tool_call_id, &name);
        let buffer = self.control.tool_buffer_mut(tool_key);
        let args_preview = buffer.tool_args.remove(&invocation_key).unwrap_or_default();
        buffer.tool_calls.push(ToolCallSummary {
            name: name.clone(),
            args_preview,
            success,
            task_id: task_id.map(crate::chat::turn_scheduler::TurnTaskId::get),
            sequence,
        });
        // Placeholder feature: only record + pop the last pending index
        if let Some(buffer) = self.control.tool_buffers.get_mut(&tool_key)
            && !buffer.pending_tool_cards.is_empty()
        {
            buffer.pending_tool_cards.pop();
        }
        vec![
            Effect::RequestRedraw,
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_finished name={name} success={success} duration_ms={duration_ms}"),
            },
        ]
    }

    /// `Action::ToolProgress` — progress notification (RequestRedraw + LogTrace only).
    ///
    /// The UI does not render the progress field separately yet; the Action is kept for future
    /// extension + hook firing. It does not mutate UI state (the signature still takes `&self` while
    /// the reducer entry passes `&mut`; `&self` here silences clippy::needless-pass-by-ref-mut).
    pub(super) fn reduce_tool_progress(&mut self, iteration: usize) -> Vec<Effect> {
        self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_progress {iteration}"),
            },
            Effect::RequestRedraw,
        ]
    }

    /// **S3 T3-1**: `Action::ToolApprovalRequested` — records the foreground
    /// approval view and asks the EffectExecutor to surface it.
    ///
    /// In supervised autonomy mode the driver sends this Action **before** ToolStarted so the reducer
    /// can forward the request to EffectExecutor / UI; the driver itself waits on a oneshot rx (the
    /// dispatcher relays `ToolApprovalReceived` into the driver's receiving channel).
    /// reducer only owns display state. The driver/router remains the single
    /// approval owner and execution gate.
    pub(super) fn reduce_tool_approval_requested(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        tool_id: String,
        name: String,
        args: String,
    ) -> Vec<Effect> {
        self.ui.pending_tool_approval = Some(crate::chat::sessions::PendingToolApprovalView {
            task_id,
            tool_id: tool_id.clone(),
            name: name.clone(),
            args: args.clone(),
            selected_approval: false,
        });
        self.ui.focus = crate::chat::sessions::FocusTarget::Approval;
        self.ui.switcher = None;
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_approval_requested tool_id={tool_id} name={name}"),
            },
            Effect::RequestApproval {
                task_id,
                tool_id,
                name,
                args,
            },
        ]
    }

    /// **S3 T3-1**: `Action::ToolApprovalReceived` — clear the display prompt
    /// after a human/non-TUI decision. The dispatcher resolves the router after
    /// this reducer step.
    pub(super) fn reduce_tool_approval_received(&mut self, tool_id: &str, approved: bool) -> Vec<Effect> {
        if self
            .ui
            .pending_tool_approval
            .as_ref()
            .is_some_and(|view| view.tool_id == tool_id)
        {
            self.ui.pending_tool_approval = None;
            if matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
                self.ui.focus = crate::chat::sessions::FocusTarget::Main;
            }
        }
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_approval_received tool_id={tool_id} approved={approved}"),
            },
            Effect::RequestRedraw,
        ]
    }

    pub(super) fn reduce_tool_approval_cleared(&mut self) -> Vec<Effect> {
        self.ui.pending_tool_approval = None;
        if matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
            self.ui.focus = crate::chat::sessions::FocusTarget::Main;
        }
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: "tool_approval_cleared".to_string(),
            },
            Effect::RequestRedraw,
        ]
    }

    // ── Step 4 helpers (exit + session) ────────────────────────────────────────

    /// `Action::CancelRequested` — a single Ctrl+C; cancel the current streaming turn if there is one.
    ///
    /// - if generating == false → no active turn, return vec![] (no-op)
    /// - if generating == true  → clear stream.draft + control state and return
    ///   [CancelToken(tok)?, CancelDraft(id), LogTrace, RequestRedraw]
    ///
    /// S2-B Step 2: added [`Effect::CancelToken`] — as soon as the reducer performs
    /// `active_cancel.take()` it hands the token to EffectExecutor, which really calls
    /// `token.cancel()`. That closes the earlier window of "UI cancelled but the underlying LLM stream
    /// keeps running" (a reducer clearing state without cancelling the token was the S2-B Codex risk).
    pub(super) fn reduce_cancel_requested(&mut self) -> Vec<Effect> {
        if !self.control.generating {
            // cancelling while idle is meaningless — no-op
            return vec![];
        }
        let Some((tool_key, draft_id)) = self.primary_cancel_target() else {
            return vec![];
        };
        self.cancel_task(tool_key, draft_id, "turn cancelled by cancel request")
    }

    pub(super) fn primary_cancel_target(&self) -> Option<(ToolTaskKey, String)> {
        self.stream
            .primary_draft()
            .map(|turn| (ToolTaskKey::from_task_id(turn.task_id), turn.draft.draft_id.clone()))
    }

    pub(super) fn reduce_cancel_provider_turn(
        &mut self,
        task_id: crate::chat::turn_scheduler::TurnTaskId,
    ) -> Vec<Effect> {
        let Some(draft_id) = self
            .stream
            .visible_drafts
            .iter()
            .find(|turn| turn.task_id == Some(task_id))
            .map(|turn| turn.draft.draft_id.clone())
        else {
            return vec![];
        };
        self.cancel_task(
            ToolTaskKey::Task(task_id),
            draft_id,
            "turn cancelled by provider worker cancel request",
        )
    }

    pub(super) fn clear_target_pending_approval(
        &mut self,
        tool_key: ToolTaskKey,
    ) -> Option<crate::chat::sessions::PendingToolApprovalView> {
        let should_clear = self
            .ui
            .pending_tool_approval
            .as_ref()
            .is_some_and(|pending| ToolTaskKey::from_task_id(pending.task_id) == tool_key);
        if !should_clear {
            return None;
        }
        let pending = self.ui.pending_tool_approval.take();
        if matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
            self.ui.focus = crate::chat::sessions::FocusTarget::Main;
        }
        pending
    }

    pub(super) fn cancel_task(&mut self, tool_key: ToolTaskKey, draft_id: String, reason: &'static str) -> Vec<Effect> {
        let cancel_opt = self.control.take_turn_cancel(tool_key);
        let _ = self.stream.remove_visible_draft(&draft_id);
        self.finalize_pending_tool_cards(tool_key, true, Some(reason));
        self.control.clear_tool_buffer(tool_key);
        let target_pending_approval = self.clear_target_pending_approval(tool_key);
        let no_visible_drafts = !self.stream.has_visible_drafts();
        let no_task_cancels = !self.control.has_task_turn_cancels();
        let global_pending_approval = if no_visible_drafts && no_task_cancels {
            self.control.active_cancel = None;
            self.control.generating = false;
            let pending = self.ui.pending_tool_approval.take();
            if pending.is_some() && matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
                self.ui.focus = crate::chat::sessions::FocusTarget::Main;
            }
            pending
        } else {
            None
        };

        let mut effects = Vec::new();
        // Emit CancelToken first to really trigger the underlying cancel, then CancelDraft for the UI.
        if let Some(token) = cancel_opt {
            effects.push(Effect::CancelToken(token));
        }
        for pending in target_pending_approval.into_iter().chain(global_pending_approval) {
            effects.push(Effect::ResolveApproval {
                tool_id: pending.tool_id,
                approved: false,
            });
        }
        effects.push(Effect::CancelDraft(draft_id));
        effects.push(Effect::LogTrace {
            level: tracing::Level::INFO,
            msg: "Turn cancelled by CancelRequested".to_string(),
        });
        effects.push(Effect::RequestRedraw);
        effects
    }

    /// `Action::ShutdownRequested` — double Ctrl+C / SIGTERM; exit gracefully.
    ///
    /// If generation is in progress, cancel the current draft + token too, then return [Quit].
    /// When the main loop sees `Effect::Quit` it calls `shutdown.cancel()` (the CancellationToken lives
    /// in the main-loop shell, not in the reducer; to be confirmed once Step 5 wires it all up).
    ///
    /// S2-B Step 2: same as [`Self::reduce_cancel_requested`] — while a streaming turn is still alive
    /// we must emit `Effect::CancelToken` so EffectExecutor really calls token.cancel(), otherwise the
    /// underlying LLM stream never receives the cancel signal.
    pub(super) fn reduce_shutdown_requested(&mut self) -> Vec<Effect> {
        let (draft_id_opt, cancel_tokens) = if self.control.generating {
            let id = Self::take_draft_id(&self.stream);
            let tokens = self.control.drain_turn_cancels();
            self.stream.clear_visible_drafts();
            self.control.generating = false;
            (id, tokens)
        } else {
            (None, Vec::new())
        };

        let mut effects = Vec::new();
        for token in cancel_tokens {
            effects.push(Effect::CancelToken(token));
        }
        if let Some(draft_id) = draft_id_opt {
            effects.push(Effect::CancelDraft(draft_id));
        }
        effects.push(Effect::Quit);
        effects
    }
}

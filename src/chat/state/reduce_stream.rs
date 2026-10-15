//! Reducer arms for the streaming turn lifecycle (start, chunks, completion, failure, cancel).

use super::{ChatState, Effect, StreamingDraft, StreamingTurnDraft, ToolTaskKey, apply_reasoning_progress};
use crate::hooks::HookEvent;
use crate::providers::ChatMessage;
use crate::util::truncate_with_ellipsis;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

impl ChatState {
    // ── Streaming / tool helpers (Step 3) ───────────────────────────────────
    //
    // P3-5 moved the whole version mechanism down here: `StreamState::draft` inside the reducer is
    // the single guard. The strict-monotonic version comparison lives in
    // `reduce_stream_chunk_received`, with these rules:
    //   1. no draft (already finalized) → drop
    //   2. draft_id does not match (stale across turns) → drop
    //   3. version <= the current draft.version → drop (equal included, strict-monotonic)
    //   4. otherwise: accumulate the delta + bump version + RequestRedraw
    //
    // The old `DraftVersionTracker` (HashMap-based, Mutex-guarded) was over-defensive and has been
    // removed from the `chat::mod::draft_updater` task since Step 3 (a single-threaded mpsc is
    // naturally FIFO and the counter alone guarantees monotonicity). With the reducer in charge
    // there is exactly one version mechanism left, so no dual-write race remains.

    pub(super) fn visible_draft_sequence(
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        sequence: Option<u64>,
    ) -> u64 {
        sequence
            .or_else(|| task_id.map(crate::chat::turn_scheduler::TurnTaskId::get))
            .unwrap_or(0)
    }

    pub(super) fn prompt_preview_from_history(history: &[ChatMessage]) -> String {
        history
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map_or_else(String::new, |message| truncate_with_ellipsis(&message.content, 96))
    }

    pub(super) fn insert_visible_streaming_draft(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        sequence: Option<u64>,
        draft_id: String,
        prompt_preview: String,
    ) {
        self.stream.insert_visible_draft(StreamingTurnDraft {
            task_id,
            sequence: Self::visible_draft_sequence(task_id, sequence),
            prompt_preview,
            started_at_ms: chrono::Utc::now().timestamp_millis(),
            draft: StreamingDraft::new(draft_id),
        });
    }

    #[must_use]
    pub(super) fn sequence_for_tool_key(&self, key: ToolTaskKey) -> Option<u64> {
        let ToolTaskKey::Task(task_id) = key else {
            return None;
        };
        self.stream
            .visible_drafts
            .iter()
            .find(|draft| draft.task_id == Some(task_id))
            .map(|draft| draft.sequence)
    }

    /// `Action::TurnStarted` — initialise the streaming draft + register the cancellation token.
    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_turn_started(&mut self, draft_id: String, cancel: CancellationToken) -> Vec<Effect> {
        self.insert_visible_streaming_draft(None, None, draft_id.clone(), String::new());
        self.stream.started_at_ms = Some(chrono::Utc::now().timestamp_millis());
        self.stream.last_duration_ms = None;
        self.control.clear_tool_buffer(ToolTaskKey::Primary);
        self.control.register_turn_cancel(ToolTaskKey::Primary, cancel);
        self.control.generating = true;
        self.control.context_degrade_notified = false;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("turn_started draft_id={draft_id}"),
            },
            Effect::RequestRedraw,
        ]
    }

    #[cfg(not(feature = "terminal-tui"))]
    pub(super) fn reduce_turn_started(&mut self, draft_id: String, cancel: CancellationToken) -> Vec<Effect> {
        self.insert_visible_streaming_draft(None, None, draft_id.clone(), String::new());
        self.control.clear_tool_buffer(ToolTaskKey::Primary);
        self.control.register_turn_cancel(ToolTaskKey::Primary, cancel);
        self.control.generating = true;
        self.control.context_degrade_notified = false;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("turn_started draft_id={draft_id}"),
            },
            Effect::RequestRedraw,
        ]
    }

    /// Step 5a-3 Phase A — `Action::StartLLMTurn`: start a streaming LLM turn.
    ///
    /// Behaviour:
    /// 1. state changes match [`Self::reduce_turn_started`] (init draft, register cancel, set generating)
    /// 2. **additionally** emits `Effect::StartTurn { draft_id, history, cancel }`, which makes
    ///    EffectExecutor spawn a subtask calling `provider.stream_chat_with_history` in real-deps mode
    ///
    /// The key difference from `TurnStarted`: it carries a history snapshot so the reducer can drive a
    /// real LLM stream. In Phase A the old chat::run main loop has not switched over; this Action is
    /// only for the Phase B+ main loop or unit tests of the reducer → Effect → EffectExecutor loop.
    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_start_llm_turn(
        &mut self,
        provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        provider_turn_sequence: Option<u64>,
        draft_id: String,
        history: Vec<crate::providers::ChatMessage>,
        compaction_guard_history: Option<Vec<crate::providers::ChatMessage>>,
        compaction_config: Option<crate::config::AgentCompactionConfig>,
        cancel: CancellationToken,
        turn_spawn_ctx: Option<crate::tools::sessions_spawn::SpawnExecutionContext>,
        turn_message_send_ctx: Option<crate::tools::message_send::MessageSendExecutionContext>,
        routing_input: Option<String>,
        turn_binding: Option<Arc<crate::chat::decision_source::ChatTurnBinding>>,
    ) -> Vec<Effect> {
        self.insert_visible_streaming_draft(
            provider_turn_task_id,
            provider_turn_sequence,
            draft_id.clone(),
            Self::prompt_preview_from_history(&history),
        );
        self.stream.started_at_ms = Some(chrono::Utc::now().timestamp_millis());
        self.stream.last_duration_ms = None;
        self.control
            .clear_tool_buffer(ToolTaskKey::from_task_id(provider_turn_task_id));
        self.control
            .register_turn_cancel(ToolTaskKey::from_task_id(provider_turn_task_id), cancel.clone());
        self.control.generating = true;
        self.control.context_degrade_notified = false;
        // BUG-09: capture the current chat mode so the driver can enforce plan
        // mode's read-only contract on write/shell/git tools.
        let chat_mode = self.session.mode;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("start_llm_turn draft_id={draft_id} history_len={}", history.len()),
            },
            Effect::StartTurn {
                provider_turn_task_id,
                draft_id,
                history,
                compaction_guard_history: compaction_guard_history.or_else(|| Some(self.session.history.clone())),
                compaction_config,
                cancel,
                chat_mode,
                turn_spawn_ctx,
                turn_message_send_ctx,
                routing_input,
                turn_binding,
            },
            Effect::RequestRedraw,
        ]
    }

    #[cfg(not(feature = "terminal-tui"))]
    pub(super) fn reduce_start_llm_turn(
        &mut self,
        provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        provider_turn_sequence: Option<u64>,
        draft_id: String,
        history: Vec<crate::providers::ChatMessage>,
        compaction_guard_history: Option<Vec<crate::providers::ChatMessage>>,
        compaction_config: Option<crate::config::AgentCompactionConfig>,
        cancel: CancellationToken,
        turn_spawn_ctx: Option<crate::tools::sessions_spawn::SpawnExecutionContext>,
        turn_message_send_ctx: Option<crate::tools::message_send::MessageSendExecutionContext>,
        routing_input: Option<String>,
        turn_binding: Option<Arc<crate::chat::decision_source::ChatTurnBinding>>,
    ) -> Vec<Effect> {
        self.insert_visible_streaming_draft(
            provider_turn_task_id,
            provider_turn_sequence,
            draft_id.clone(),
            Self::prompt_preview_from_history(&history),
        );
        self.control
            .clear_tool_buffer(ToolTaskKey::from_task_id(provider_turn_task_id));
        self.control
            .register_turn_cancel(ToolTaskKey::from_task_id(provider_turn_task_id), cancel.clone());
        self.control.generating = true;
        self.control.context_degrade_notified = false;
        // BUG-09: capture the current chat mode so the driver can enforce plan
        // mode's read-only contract on write/shell/git tools.
        let chat_mode = self.session.mode;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("start_llm_turn draft_id={draft_id} history_len={}", history.len()),
            },
            Effect::StartTurn {
                provider_turn_task_id,
                draft_id,
                history,
                compaction_guard_history: compaction_guard_history.or_else(|| Some(self.session.history.clone())),
                compaction_config,
                cancel,
                chat_mode,
                turn_spawn_ctx,
                turn_message_send_ctx,
                routing_input,
                turn_binding,
            },
            Effect::RequestRedraw,
        ]
    }

    /// `Action::StreamChunkReceived` — version guard + delta accumulation.
    ///
    /// Return value:
    /// - accepted → `[RequestRedraw]`
    /// - dropped  → `[]` (silently; callers can tell by comparing draft.version before and after)
    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_stream_chunk_received(&mut self, draft_id: &str, delta: &str, version: u64) -> Vec<Effect> {
        let Some(turn) = self.stream.visible_draft_mut(draft_id) else {
            // already finalized — the chunk is stale, drop it
            return vec![];
        };
        let draft = &mut turn.draft;
        if version <= draft.version {
            // strictly monotonic: equal or smaller means out-of-order/duplicate, drop it
            return vec![];
        }
        draft.accumulated.push_str(delta);
        draft.version = version;
        self.refresh_provider_worker_view_if_focused();
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    pub(super) fn reduce_stream_chunk_received(&mut self, draft_id: &str, delta: &str, version: u64) -> Vec<Effect> {
        let Some(turn) = self.stream.visible_draft_mut(draft_id) else {
            return vec![];
        };
        let draft = &mut turn.draft;
        if version <= draft.version {
            return vec![];
        }
        draft.accumulated.push_str(delta);
        draft.version = version;
        vec![Effect::RequestRedraw]
    }

    /// `Action::StreamReasoningReceived` — version guard + accumulated thinking progress.
    ///
    /// Shares the strict-monotonic rule with [`Self::reduce_stream_chunk_received`] (no draft / older
    /// version → silently dropped), because the versions of both delta kinds come from the same
    /// per-turn counter.
    ///
    /// Single source of truth: only the **character count** and a bounded tail (for the one-line live
    /// preview) are accumulated here; the reasoning body stays owned by the streaming driver and comes
    /// back in one piece in `Action::StreamCompleted` — the reducer never keeps a second full copy.
    pub(super) fn reduce_stream_reasoning_received(
        &mut self,
        draft_id: &str,
        delta: &str,
        version: u64,
    ) -> Vec<Effect> {
        let Some(turn) = self.stream.visible_draft_mut(draft_id) else {
            // already finalized — the delta is stale, drop it
            return vec![];
        };
        let draft = &mut turn.draft;
        if version <= draft.version {
            // strictly monotonic: equal or smaller means out-of-order/duplicate, drop it
            return vec![];
        }
        draft.version = version;
        apply_reasoning_progress(draft, delta);
        self.refresh_provider_worker_view_if_focused();
        vec![Effect::RequestRedraw]
    }

    /// `Action::StreamCompleted` — clear draft + push assistant message + notify hooks + **persist**.
    ///
    /// T3-3-c: [`Effect::SaveSession`] is appended at the end of the Effect list so the reducer writes a
    /// session snapshot whenever a turn completes; this pulls the persistence that legacy
    /// `chat_session.add_*_turn` + `save_session(...)` did in Pure mode into the reducer as one source.
    ///
    /// Effect ordering guarantee (the executor consumes the Vec in order):
    ///
    /// - `[0]` NotifyHook(TurnComplete) — webhooks / observers learn the turn finished first
    /// - `[1]` SaveSession(snapshot)    — persist turns (dual_write_guard prevents double writes)
    /// - `[2]` RequestRedraw            — the UI refresh comes last
    ///
    /// **Important**: the snapshot uses the reducer's own `session.turns` (written by
    /// `RecordAssistantTurn`), not the legacy `chat_session` copy. Pure mode keeps both in sync and
    /// skips the legacy copy via the T3-3-c guard; Off / Both / Redux modes rely on dual_write_guard.
    #[cfg(feature = "terminal-tui")]
    pub(super) fn reduce_stream_completed(
        &mut self,
        draft_id: &str,
        final_text: String,
        reasoning: String,
    ) -> Vec<Effect> {
        use crate::chat::tui::ConversationLine;
        let Some(removed_draft) = self.stream.remove_visible_draft(draft_id) else {
            return vec![];
        };
        let tool_key = ToolTaskKey::from_task_id(removed_draft.task_id);
        self.control.remove_turn_cancel(tool_key);
        let no_visible_drafts = !self.stream.has_visible_drafts();
        self.stream.last_duration_ms = Some(crate::chat::tui::turn_elapsed_ms(removed_draft.started_at_ms));
        if no_visible_drafts {
            self.stream.started_at_ms = None;
        }
        self.remove_pending_tool_cards(tool_key);
        if no_visible_drafts && !self.control.has_task_turn_cancels() {
            self.control.active_cancel = None;
            self.control.generating = false;
        }
        if !final_text.is_empty() {
            self.ui.conversation_lines.push(ConversationLine::Assistant {
                content: final_text.clone(),
            });
        }
        if !reasoning.trim().is_empty() {
            let char_count = reasoning.chars().count();
            self.ui.conversation_lines.push(ConversationLine::Reasoning {
                content: reasoning,
                char_count,
                folded: true,
            });
        }
        self.refresh_provider_worker_view_if_focused();
        let chars = final_text.chars().count();
        let effects = vec![
            Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                payload: serde_json::json!({
                    "mode": "chat",
                    "response_chars": chars,
                }),
            },
            Effect::SaveSession(self.build_session_snapshot()),
            Effect::RequestRedraw,
        ];
        // Fallback for drivers that miss RecordAssistantTurn: only the completed
        // task's buffer is discarded.
        self.control.clear_tool_buffer(tool_key);
        effects
    }

    #[cfg(not(feature = "terminal-tui"))]
    pub(super) fn reduce_stream_completed(
        &mut self,
        draft_id: &str,
        final_text: String,
        reasoning: String,
    ) -> Vec<Effect> {
        let Some(removed_draft) = self.stream.remove_visible_draft(draft_id) else {
            return vec![];
        };
        let tool_key = ToolTaskKey::from_task_id(removed_draft.task_id);
        self.control.remove_turn_cancel(tool_key);
        let no_visible_drafts = !self.stream.has_visible_drafts();
        self.remove_pending_tool_cards(tool_key);
        if no_visible_drafts && !self.control.has_task_turn_cancels() {
            self.control.active_cancel = None;
            self.control.generating = false;
        }
        if !final_text.is_empty() {
            self.ui.conversation_lines.push(final_text.clone());
        }
        if !reasoning.trim().is_empty() {
            self.ui.conversation_lines.push(reasoning);
        }
        let chars = final_text.chars().count();
        let effects = vec![
            Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                payload: serde_json::json!({
                    "mode": "chat",
                    "response_chars": chars,
                }),
            },
            Effect::SaveSession(self.build_session_snapshot()),
            Effect::RequestRedraw,
        ];
        self.control.clear_tool_buffer(tool_key);
        effects
    }

    /// `Action::StreamFailed` — clear draft + LogTrace + NotifyHook(Error).
    ///
    /// Phase F: semantics match the old path's `hooks.emit(HookEvent::Error, payload_error(...))` in the
    /// chat::run main loop — a failed turn must fire the Error hook, otherwise external audits and
    /// webhooks miss it. The hook always fires, because "this turn failed" is a definite public event.
    ///
    /// `retryable` is a **diagnostic field** and triggers no automatic resend: retries belong to the provider
    /// layer (backoff / `Retry-After` / failover), and by this point the turn's tool side effects may have landed.
    /// It feeds the trace log and the `HookEvent::Error` payload so audits can tell transient from hard failures.
    pub(super) fn reduce_stream_failed(&mut self, draft_id: &str, err: String, retryable: bool) -> Vec<Effect> {
        let Some(removed_draft) = self.stream.remove_visible_draft(draft_id) else {
            return vec![];
        };
        let tool_key = ToolTaskKey::from_task_id(removed_draft.task_id);
        self.control.remove_turn_cancel(tool_key);
        let no_visible_drafts = !self.stream.has_visible_drafts();
        if no_visible_drafts {
            self.stream.last_duration_ms = self.stream.started_at_ms.take().map(|started_at_ms| {
                let elapsed = chrono::Utc::now()
                    .timestamp_millis()
                    .saturating_sub(started_at_ms)
                    .max(0);
                u64::try_from(elapsed).unwrap_or(u64::MAX)
            });
        }
        let orphan_user_removed = if no_visible_drafts {
            self.rollback_trailing_answerless_user_turn()
        } else {
            false
        };
        self.finalize_pending_tool_cards(tool_key, false, Some("turn failed before tool finish event"));
        if no_visible_drafts && !self.control.has_task_turn_cancels() {
            self.control.active_cancel = None;
            self.control.generating = false;
        }
        self.control.clear_tool_buffer(tool_key);
        #[cfg(feature = "terminal-tui")]
        self.ui
            .conversation_lines
            .push(crate::chat::tui::ConversationLine::System {
                content: format!("Error: {err}"),
            });
        vec![
            Effect::LogTrace {
                level: tracing::Level::WARN,
                msg: format!(
                    "stream_failed draft_id={draft_id} retryable={retryable} orphan_user_removed={orphan_user_removed} err={err}"
                ),
            },
            Effect::NotifyHook {
                event: HookEvent::Error,
                payload: serde_json::json!({
                    "component": "chat-turn",
                    "message": err,
                    "retryable": retryable,
                    "draft_id": draft_id,
                }),
            },
            Effect::RequestRedraw,
        ]
    }

    /// `Action::StreamCancelled` — the user cancelled explicitly; only clear the draft.
    pub(super) fn reduce_stream_cancelled(&mut self, draft_id: &str) -> Vec<Effect> {
        let Some(removed_draft) = self.stream.remove_visible_draft(draft_id) else {
            return vec![];
        };
        let tool_key = ToolTaskKey::from_task_id(removed_draft.task_id);
        self.control.remove_turn_cancel(tool_key);
        let no_visible_drafts = !self.stream.has_visible_drafts();
        if no_visible_drafts {
            self.stream.last_duration_ms = self.stream.started_at_ms.take().map(|started_at_ms| {
                let elapsed = chrono::Utc::now()
                    .timestamp_millis()
                    .saturating_sub(started_at_ms)
                    .max(0);
                u64::try_from(elapsed).unwrap_or(u64::MAX)
            });
        }
        self.finalize_pending_tool_cards(tool_key, false, Some("turn cancelled before tool finish event"));
        if no_visible_drafts {
            self.rollback_trailing_answerless_user_turn();
        }
        if no_visible_drafts && !self.control.has_task_turn_cancels() {
            self.control.active_cancel = None;
            self.control.generating = false;
        }
        self.control.clear_tool_buffer(tool_key);
        vec![Effect::RequestRedraw]
    }

    pub(super) fn rollback_trailing_answerless_user_turn(&mut self) -> bool {
        let Some(last_turn) = self.session.turns.last() else {
            return false;
        };
        if last_turn.role != "user" {
            return false;
        }
        let content = last_turn.content.clone();
        let title_from_user = crate::chat::session::truncate_title(&content);
        self.session.turns.pop();
        if self
            .session
            .history
            .last()
            .is_some_and(|message| message.role == "user" && message.content == content)
        {
            self.session.history.pop();
        }
        if self.session.turns.is_empty() && self.session.title == title_from_user {
            self.session.title.clear();
        }
        true
    }

    /// **S3 T3-1**: `Action::StreamRetryAttempt` — a network retry attempt; trace + redraw only.
    ///
    /// Does not mutate state (the driver keeps its own attempt count); the UI can show a "retrying" hint.
    pub(super) fn reduce_stream_retry_attempt(&self, attempt: u8, reason: &str) -> Vec<Effect> {
        let _ = &self.ui;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("stream_retry_attempt #{attempt} reason={reason}"),
            },
            Effect::RequestRedraw,
        ]
    }
}

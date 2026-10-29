//! Reducer arms for session load/save/switch, turn recording and history compaction.

use super::{
    ChatState, Effect, ToolTaskKey, compact_history_in_place, conversation_lines_from_turns,
    durable_turns_from_compacted_history,
};
use crate::chat::action::{CompactReason, ProviderUsageRecordKind};
use crate::chat::session::{ChatSession, MainSessionTokenUsageRecord, MainSessionTokenUsageSummary};
use crate::providers::ChatMessage;
use std::sync::Arc;

impl ChatState {
    /// `Action::SessionLoaded(ChatSession)` — restore a persisted session into SessionState.
    ///
    /// Every field is replaced (id/title/provider/model/mode/turns); history is rebuilt from turns by
    /// the main loop when SessionLoaded arrives (wired up in Step 5).
    pub(super) fn reduce_session_loaded(&mut self, loaded: ChatSession) -> Vec<Effect> {
        let id = loaded.id.clone();
        if self.control.generating {
            return vec![Effect::LogTrace {
                level: tracing::Level::WARN,
                msg: format!("SessionLoaded rejected while generating: {id}"),
            }];
        }
        self.session.id = loaded.id;
        self.session.title = loaded.title;
        self.session.provider = Arc::from(loaded.provider.as_str());
        self.session.model = Arc::from(loaded.model.as_str());
        self.session.mode = loaded.mode;
        self.ui.chat_mode = loaded.mode;
        self.session.turns = loaded.turns;
        self.session.token_usage_records = loaded.token_usage_records;
        self.ui.token_usage_summary = MainSessionTokenUsageSummary::from_records(&self.session.token_usage_records);
        // v4: restore persisted background-session summaries (display only —
        // the live processes are gone and are never revived). Carrying them in
        // SessionState means the next save_session snapshot re-persists them, so
        // they survive across multiple reload cycles.
        self.session.background_sessions = loaded.background_sessions;
        // S4-B T4-B-6: keep the original session's created_at so a later save_session cannot overwrite it
        self.session.created_at = Some(loaded.created_at);
        // rebuild history from turns (only user/assistant roles enter the LLM context)
        self.session.history = self
            .session
            .turns
            .iter()
            .filter(|t| t.role == "user" || t.role == "assistant")
            // stored user turns are neutralised like fresh input
            .map(|t| crate::agent::turn_contracts::stored_turn_message(t.role.clone(), t.content.clone()))
            .collect();
        // A session saved before the delivery guard existed still holds
        // fabricated `[Used tools: ...]` claims in its assistant turns. This is
        // the replay seam that hands those turns back to the model, so strip
        // them from the context copy here, in both grains. The persisted turns
        // and the rendered transcript keep the bytes that were actually
        // produced; only what the model is told it once did is corrected.
        crate::agent::response_guard::strip_fabricated_tool_markers_from_assistant_turns(&mut self.session.history);
        self.ui.conversation_lines = conversation_lines_from_turns(&self.session.turns);
        self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        #[cfg(feature = "terminal-tui")]
        self.ui.input.clear_navigation_state();
        #[cfg(not(feature = "terminal-tui"))]
        self.ui.input.clear();
        self.ui.turn_count = self.session.turns.len();
        self.ui.active_session_view = None;
        self.ui.pending_tool_approval = None;
        self.ui.context_used_tokens = None;
        self.ui.context_window_tokens = None;
        self.ui.focus = crate::chat::sessions::FocusTarget::Main;
        self.ui.sessions_status.clear();
        self.ui.sessions_entries.clear();
        self.ui.switcher = None;
        self.ui.slash_menu = None;
        self.ui.saved_session_picker = None;
        self.stream.clear_visible_drafts();
        self.control.clear_all_tool_buffers();
        self.control.turn_cancels.clear();
        self.control.final_usage_tasks_recorded.clear();
        self.control.generating = false;
        self.control.active_cancel = None;
        vec![
            Effect::RequestRedraw,
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("Session loaded: {id}"),
            },
        ]
    }

    /// A save acknowledgement may initialize an empty identity, but cannot
    /// switch an established session after a newer SessionLoaded action.
    pub(super) fn reduce_session_saved(&mut self, id: String) -> Vec<Effect> {
        if self.session.id.is_empty() {
            self.session.id = id.clone();
        }
        vec![Effect::LogTrace {
            level: tracing::Level::INFO,
            msg: if self.session.id == id {
                format!("Session saved: {id}")
            } else {
                format!("Inactive session save acknowledged: {id}")
            },
        }]
    }

    /// `Action::SessionSwitched { id }` — request a switch to another session.
    ///
    /// Design: a two-step async flow. The reducer only produces effects[0] = SaveSession(current).
    /// After the main loop performs the save it spawns the async load and dispatches `SessionLoaded(new)`.
    /// The interruption window (a crash before the save succeeds) is handled by the main loop, not here.
    ///
    /// effects order (exact):
    ///   [0] SaveSession(current_snapshot)
    ///   [1] LogTrace
    ///   [2] RequestRedraw
    pub(super) fn reduce_session_switched(&self, id: String) -> Vec<Effect> {
        vec![
            Effect::SaveSession(self.build_session_snapshot()),
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("Session switching to: {id}"),
            },
            Effect::RequestRedraw,
        ]
    }

    /// T3-3-c: build a [`ChatSession`] snapshot from `SessionState` for [`Effect::SaveSession`].
    ///
    /// `SessionState` holds no `created_at` / `updated_at` timestamps (chronological metadata belongs to
    /// the `ChatSession` persistence layer), so the snapshot fills them with the current time — that both
    /// distinguishes `updated_at` between saves and lets `load_latest_session` pick the newest session by
    /// `updated_at`. `schema_version` always uses the `SCHEMA_VERSION` constant.
    ///
    /// It lives in its own fn so `reduce_session_switched` / `reduce_stream_completed` and others share a
    /// single construction path and cannot miss a field.
    pub(super) fn build_session_snapshot(&self) -> ChatSession {
        let now = chrono::Utc::now();
        let snapshot = ChatSession {
            id: self.session.id.clone(),
            schema_version: crate::chat::session::SCHEMA_VERSION,
            title: self.session.title.clone(),
            provider: self.session.provider.as_ref().to_owned(),
            model: self.session.model.as_ref().to_owned(),
            // S4-B T4-B-6: strict created_at semantics — take SessionState.created_at (set on the first
            // RecordUserTurn) and fall back to now, so an existing creation time is never overwritten
            created_at: self.session.created_at.unwrap_or(now),
            updated_at: now,
            turns: self.session.turns.clone(),
            background_sessions: self.session.background_sessions.clone(),
            token_usage_records: self.session.token_usage_records.clone(),
            mode: self.session.mode,
        };
        crate::chat::sanitize::sanitize_session_content(&snapshot)
    }

    /// `Action::RecordUserTurn(text)` — persist a user turn into the session record and LLM history.
    ///
    /// Matches `session.add_user_turn` semantics:
    /// - `updated_at` is set by the effect executor when it builds the `SaveSession` snapshot
    /// - on the first user turn, if title is empty, set_title runs automatically (first 50 chars, as
    ///   ChatSession does); tool_calls stays empty, tool sync is done by `ToolStarted`/`ToolFinished`
    pub(super) fn reduce_record_user_turn(&mut self, content: String) -> Vec<Effect> {
        // A user turn must not read as the loop's own bookkeeping later on.
        let content = match crate::agent::turn_contracts::neutralize_reserved_markers(&content) {
            std::borrow::Cow::Borrowed(_) => content,
            std::borrow::Cow::Owned(escaped) => escaped,
        };
        let now = chrono::Utc::now();
        // S4-B T4-B-6: lazily initialise created_at on the first RecordUserTurn
        if self.session.created_at.is_none() {
            self.session.created_at = Some(now);
        }
        self.session.turns.push(crate::chat::session::ChatTurn {
            role: "user".to_string(),
            content: content.clone(),
            timestamp: now,
            tool_calls: Vec::new(),
        });
        // On the first user turn with an empty title, set it automatically (as session.add_user_turn does)
        if self.session.title.is_empty() {
            self.session.title = crate::chat::session::truncate_title(&content);
        }
        self.session
            .history
            .push(crate::agent::turn_contracts::inbound_user_message(&content));
        vec![Effect::LogTrace {
            level: tracing::Level::DEBUG,
            msg: format!("RecordUserTurn len={}", self.session.turns.len()),
        }]
    }

    /// `Action::RecordAssistantTurn` — persist an assistant turn into the session record and LLM history.
    ///
    /// Matches `session.add_assistant_turn` semantics:
    /// - `updated_at` is set by the effect executor when it builds the `SaveSession` snapshot
    /// - P3a: tool_calls come from the matching task bucket. Legacy callers use
    ///   the Primary bucket, so main transcript behavior stays unchanged.
    pub(super) fn reduce_record_assistant_turn(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        content: String,
    ) -> Vec<Effect> {
        let tool_calls = self.control.take_tool_calls(ToolTaskKey::from_task_id(task_id));
        self.session.turns.push(crate::chat::session::ChatTurn {
            role: "assistant".to_string(),
            content: content.clone(),
            timestamp: chrono::Utc::now(),
            tool_calls,
        });
        self.session.history.push(ChatMessage::assistant(content));
        vec![Effect::LogTrace {
            level: tracing::Level::DEBUG,
            msg: format!("RecordAssistantTurn len={}", self.session.turns.len()),
        }]
    }

    /// `Action::RecordSystemMessage` — append one system message to the LLM context history.
    ///
    /// S2-C Step 2: matches legacy `history.push(ChatMessage::system(content))`.
    /// Difference from [`Self::reduce_set_leading_system_prompt`]:
    /// - this function always appends (typical case: rebuilding the system prompt after `/clear` — clear
    ///   already emptied history, so pushing at the end is also first and append equals replace)
    /// - `SetLeadingSystemPrompt` upserts (empty → push, non-empty → replace history[0])
    ///
    /// session.turns is untouched (a system message is not a user/assistant turn, only LLM context config).
    pub(super) fn reduce_record_system_message(&mut self, content: String) -> Vec<Effect> {
        self.session.history.push(ChatMessage::system(content));
        vec![Effect::LogTrace {
            level: tracing::Level::DEBUG,
            msg: format!("RecordSystemMessage history_len={}", self.session.history.len()),
        }]
    }

    /// `Action::SetLeadingSystemPrompt` — set/replace the leading system prompt.
    ///
    /// S2-C Step 2: byte-for-byte aligned with the chat::mod main loop's `if history.is_empty() { push }
    /// else { first_mut = system }`. It runs on every turn (the system prompt is rebuilt after technique
    /// selection), so expressing it as an append would pile up more and more system messages.
    ///
    /// Behaviour:
    /// - history empty → push one system message
    /// - history non-empty, first entry is system → replace `history[0]` (same as legacy `*first = ...`)
    /// - history non-empty, first entry is **not** system → insert system at the front. This keeps
    ///   resumed user/assistant turns intact when a loaded session rebuilds history
    ///   without a runtime system prompt.
    pub(super) fn reduce_set_leading_system_prompt(&mut self, content: String) -> Vec<Effect> {
        if self.session.history.is_empty() {
            self.session.history.push(ChatMessage::system(content));
        } else if self
            .session
            .history
            .first()
            .is_some_and(|message| message.role != "system")
        {
            self.session.history.insert(0, ChatMessage::system(content));
        } else if let Some(first) = self.session.history.first_mut() {
            *first = ChatMessage::system(content);
        }
        vec![Effect::LogTrace {
            level: tracing::Level::DEBUG,
            msg: format!("SetLeadingSystemPrompt history_len={}", self.session.history.len()),
        }]
    }

    /// `Action::ActiveSessionViewUpdated` — replace/clear the focused child
    /// viewport render snapshot.
    pub(super) fn reduce_active_session_view_updated(
        &mut self,
        view: Option<crate::chat::sessions::ActiveSessionView>,
    ) -> Vec<Effect> {
        if self.ui.active_session_view == view {
            return Vec::new();
        }
        self.ui.active_session_view = view;
        vec![Effect::RequestRedraw]
    }

    pub(super) fn reduce_context_window_updated(
        &mut self,
        used_context_tokens: Option<usize>,
        max_context_tokens: Option<usize>,
    ) -> Vec<Effect> {
        if self.ui.context_used_tokens == used_context_tokens && self.ui.context_window_tokens == max_context_tokens {
            return Vec::new();
        }
        self.ui.context_used_tokens = used_context_tokens;
        self.ui.context_window_tokens = max_context_tokens;
        vec![Effect::RequestRedraw]
    }

    pub(super) fn reduce_provider_usage_recorded(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        usage_kind: ProviderUsageRecordKind,
        record: MainSessionTokenUsageRecord,
    ) -> Vec<Effect> {
        if !self.control.should_record_provider_usage(task_id, usage_kind) {
            return Vec::new();
        }
        self.session.token_usage_records.push(record);
        self.ui.token_usage_summary = MainSessionTokenUsageSummary::from_records(&self.session.token_usage_records);
        vec![
            Effect::SaveSession(self.build_session_snapshot()),
            Effect::RequestRedraw,
        ]
    }

    /// `Action::BackgroundSessionRecorded` (v4) — upsert a background-session
    /// summary into `session.background_sessions` and **immediately emit
    /// `Effect::SaveSession`** so the summary is durably persisted to the memory
    /// backend (the only write path; `dispatcher.rs` `Effect::SaveSession`).
    ///
    /// Why emit SaveSession here (P0, v4 review): under `terminal-tui` (now the
    /// default) the legacy exit-save path is disabled (`mod.rs`
    /// `legacy_exit_save_enabled=false`), and no other action snapshots after a
    /// child session reaches a terminal state. Without this effect the
    /// summary lived only in memory and was lost on exit → reload recap broke.
    /// Emitting SaveSession **after** the upsert guarantees the snapshot
    /// (`build_session_snapshot`, which clones `self.session.background_sessions`)
    /// already contains this record, eliminating the prior race where a snapshot
    /// taken before the action could miss it.
    ///
    /// Dedup is by session id: a later record for the same id replaces the
    /// earlier one (e.g. an `interrupted` entry written at exit, or a terminal
    /// summary superseding a placeholder). This records **summary only** — it
    /// never spawns or revives a process / sub-agent / PTY.
    ///
    /// No save storm: a child session reaching a terminal state is a
    /// low-frequency event, and an unchanged re-record short-circuits to
    /// `Vec::new()` before emitting any effect. `Effect::SaveSession` is a pure
    /// persistence sink (it never dispatches a new action), so there is no
    /// SaveSession → BackgroundSessionRecorded feedback loop.
    pub(super) fn reduce_background_session_recorded(
        &mut self,
        summary: crate::chat::sessions::PersistedSessionSummary,
    ) -> Vec<Effect> {
        if let Some(existing) = self.session.background_sessions.iter_mut().find(|s| s.id == summary.id) {
            if *existing == summary {
                return Vec::new();
            }
            *existing = summary;
        } else {
            self.session.background_sessions.push(summary);
        }
        // Persist the updated snapshot now (state already mutated above).
        vec![Effect::SaveSession(self.build_session_snapshot())]
    }

    /// `Action::HistoryCleared` — clear the LLM context history (keeping the system prompt) + clear UI.
    ///
    /// session.turns is not cleared (the persisted record is irreversible); only the LLM context (so the
    /// next request carries no history) and the TUI conversation_lines display are reset.
    pub(super) fn reduce_history_cleared(&mut self) -> Vec<Effect> {
        // Defensively keep every system message (usually just one, but scan all in case it is not first)
        let system_msgs: Vec<_> = self.session.history.drain(..).filter(|m| m.role == "system").collect();
        // history was emptied by drain(..), so re-insert the system messages
        self.session.history.extend(system_msgs);
        // Note: the current input buffer is left alone; InputCancelled handles that separately
        self.ui.conversation_lines.clear();
        self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        vec![
            Effect::RequestRedraw,
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: "History cleared".to_string(),
            },
        ]
    }

    pub(super) fn reduce_history_cleared_with_notice(&mut self, notice: String) -> Vec<Effect> {
        let mut effects = self.reduce_history_cleared();
        #[cfg(feature = "terminal-tui")]
        {
            self.ui
                .conversation_lines
                .push(crate::chat::tui::ConversationLine::System { content: notice });
        }
        #[cfg(not(feature = "terminal-tui"))]
        {
            let _ = notice;
        }
        effects.push(Effect::RequestRedraw);
        effects
    }

    /// `Action::HistoryCompacted` — compact the LLM context history.
    ///
    /// The algorithm matches `chat::mod::compact_chat_history` exactly (during dual-write both paths must
    /// produce byte-identical results):
    /// 1. return immediately when `history.len() <= 1` (nothing to compact).
    /// 2. keep the system prompt (if the first entry has role==system).
    /// 3. keep only the last [`COMPACT_KEEP_MESSAGES`] non-system messages (drain the older ones).
    /// 4. truncate a single message with an ellipsis when it exceeds [`COMPACT_CONTENT_CHARS`] chars.
    /// 5. drop oldest turns FIFO when the total budget exceeds [`COMPACT_TOTAL_CHARS`].
    pub(super) fn reduce_history_compacted(&mut self, reason: CompactReason) -> Vec<Effect> {
        let history = &mut self.session.history;
        if history.len() <= 1 {
            return vec![Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("HistoryCompacted noop reason={reason:?} len={}", history.len()),
            }];
        }
        compact_history_in_place(history);

        let final_chars: usize = history.iter().map(|m| m.content.chars().count()).sum();
        vec![Effect::LogTrace {
            level: tracing::Level::INFO,
            msg: format!(
                "HistoryCompacted reason={reason:?} len={} chars={final_chars}",
                history.len()
            ),
        }]
    }

    pub(super) fn reduce_history_compaction_patch_applied(
        &mut self,
        reason: CompactReason,
        patch: crate::agent::turn_contracts::CompactionPatch,
        compaction_config: &crate::config::AgentCompactionConfig,
    ) -> Vec<Effect> {
        let history = &mut self.session.history;
        if crate::agent::compaction::compaction_patch_guard_matches(history, &patch.guard) {
            crate::agent::compaction::apply_compaction_patch_exact(history, &patch);
            let budget = crate::agent::context_budget::plan_context_budget(
                history,
                compaction_config,
                crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
            );
            let trim_fallback = if budget.over_hard_limit {
                crate::agent::context_budget::trim_history_to_context_budget_preserving_compaction_replacement_with_floor(
                    history,
                    compaction_config,
                    patch.replacement.len(),
                )
            } else {
                false
            };
            self.session.turns = durable_turns_from_compacted_history(history);
            self.ui.conversation_lines = conversation_lines_from_turns(&self.session.turns);
            self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
            self.ui.turn_count = self.session.turns.len();

            return vec![
                Effect::LogTrace {
                    level: tracing::Level::INFO,
                    msg: format!(
                        "HistoryCompactionPatchApplied reason={reason:?} start={} end={} replacement={} append_after={} trim_fallback={} len={}",
                        patch.range_start,
                        patch.range_end,
                        patch.replacement.len(),
                        patch.append_after.len(),
                        trim_fallback,
                        history.len()
                    ),
                },
                Effect::SaveSession(self.build_session_snapshot()),
                Effect::RequestRedraw,
            ];
        }

        let before_len = history.len();
        let trimmed = crate::agent::context_budget::trim_history_to_context_budget(history, compaction_config);
        vec![Effect::LogTrace {
            level: tracing::Level::WARN,
            msg: format!(
                "HistoryCompactionPatch guard mismatch reason={reason:?}; stale patch ignored; trim_fallback={trimmed} before_len={before_len} after_len={}",
                history.len()
            ),
        }]
    }

    /// `Action::HistoryCompactionDegraded` — one user-visible line per turn when
    /// a rollover fell back to a lossy trim.
    ///
    /// Losing the oldest context is worth exactly one line: the turn still
    /// answers, so an error would be wrong, but saying nothing hides the fact
    /// that the messages are gone for good. Repeats inside the same turn are
    /// collapsed to trace level.
    pub(super) fn reduce_history_compaction_degraded(
        &mut self,
        reason: CompactReason,
        dropped_messages: usize,
    ) -> Vec<Effect> {
        if self.control.context_degrade_notified {
            return vec![Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!(
                    "HistoryCompactionDegraded suppressed (already surfaced this turn) reason={reason:?} dropped={dropped_messages}"
                ),
            }];
        }
        self.control.context_degrade_notified = true;
        let text = crate::chat::format_context_degraded_notice(dropped_messages);
        #[cfg(feature = "terminal-tui")]
        self.ui
            .conversation_lines
            .push(crate::chat::tui::ConversationLine::System { content: text.clone() });
        vec![
            Effect::LogTrace {
                level: tracing::Level::WARN,
                msg: format!("HistoryCompactionDegraded reason={reason:?} dropped={dropped_messages}"),
            },
            Effect::SurfaceNotice { text },
        ]
    }
}

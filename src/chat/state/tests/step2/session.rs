#![cfg(test)]

use super::*;

// ─── Step 4 unit tests (exit + session paths) ──────────────────────────────

/// Step4-1: CancelRequested with generating=false → no-op (vec![])
#[test]
fn test_redux_cancel_requested_no_active_turn_noop() {
    let mut state = s();
    assert!(!state.control.generating, "precondition: not generating");
    let effects = state.reduce(Action::CancelRequested);
    assert!(
        effects.is_empty(),
        "CancelRequested while not generating must return vec![]"
    );
}

/// Step4-2: CancelRequested with generating=true and a draft → clears draft + CancelDraft effect
#[test]
fn test_redux_cancel_requested_with_active_turn_clears_state() {
    let mut state = s();
    // start a streaming round
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    assert!(state.control.generating);
    assert!(state.stream.primary_streaming_draft().is_some());

    let effects = state.reduce(Action::CancelRequested);

    // the state must be cleared
    assert!(!state.control.generating, "generating must be cleared to false");
    assert!(
        state.stream.primary_streaming_draft().is_none(),
        "the draft must be cleared"
    );
    assert!(state.control.active_cancel.is_none(), "active_cancel must be cleared");
    // effects must contain CancelDraft + LogTrace + RequestRedraw
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CancelDraft(id) if id == "d1")),
        "must contain CancelDraft(d1)"
    );
    assert!(has_log_trace(&effects), "must contain LogTrace");
    assert!(has_request_redraw(&effects), "must contain RequestRedraw");
}

/// Step4-3: ShutdownRequested (idle) → vec![Quit]
#[test]
fn test_redux_shutdown_requested_returns_quit() {
    let mut state = s();
    let effects = state.reduce(Action::ShutdownRequested);
    assert!(has_quit(&effects), "ShutdownRequested must return a Quit effect");
    // no CancelDraft while idle
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::CancelDraft(_))),
        "an idle shutdown must not contain CancelDraft"
    );
}

/// Step4-4: ShutdownRequested while streaming → Quit + CancelDraft
#[test]
fn test_redux_shutdown_during_streaming_cancels_draft() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d2".to_string(),
        cancel: CancellationToken::new(),
    });
    assert!(state.control.generating);

    let effects = state.reduce(Action::ShutdownRequested);

    assert!(!state.control.generating, "generating must be cleared");
    assert!(
        state.stream.primary_streaming_draft().is_none(),
        "the draft must be cleared"
    );
    // S2-B Step 2: the effect order became [CancelToken, CancelDraft, Quit].
    // CancelToken comes first (really cancels the underlying turn), CancelDraft follows (syncs the
    // channel UI), and Quit is last (the shell calls shutdown.cancel()).
    assert!(
        effects.len() >= 3,
        "a streaming ShutdownRequested must emit at least 3 effects"
    );
    assert!(
        matches!(effects.first(), Some(Effect::CancelToken(_))),
        "effects[0] must be CancelToken, got: {:?}",
        effects.first()
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CancelDraft(id) if id == "d2")),
        "effects must contain CancelDraft(d2)"
    );
    assert!(
        matches!(effects.last(), Some(Effect::Quit)),
        "effects.last() must be Quit, got: {:?}",
        effects.last()
    );
}

/// Step4-5: SessionLoaded replaces every session field
#[test]
fn test_redux_session_loaded_replaces_session_state() {
    use crate::chat::session::ChatSession;
    let mut state = s();
    // put something into history to confirm it gets replaced
    state.session.history.push(crate::providers::ChatMessage::user("old"));

    let mut loaded = ChatSession::new("prov2", "model2");
    loaded.id = "sess-abc".to_string();
    loaded.title = "My Session".to_string();
    loaded.add_user_turn("hello");
    loaded.add_assistant_turn("hi", vec![]);

    let effects = state.reduce(Action::SessionLoaded(loaded));

    assert_eq!(state.session.id, "sess-abc");
    assert_eq!(state.session.title, "My Session");
    assert_eq!(&*state.session.provider, "prov2");
    assert_eq!(&*state.session.model, "model2");
    assert_eq!(state.session.turns.len(), 2, "2 turns");
    // history rebuilt from turns: user + assistant
    assert_eq!(
        state.session.history.len(),
        2,
        "history must be rebuilt from turns (user+assistant)"
    );
    assert_eq!(
        state.ui.conversation_lines.len(),
        2,
        "UI conversation_lines must be rebuilt from the restored turns"
    );
    assert!(has_request_redraw(&effects), "must contain RequestRedraw");
    assert!(has_log_trace(&effects), "must contain LogTrace");
}

/// v4: BackgroundSessionRecorded upserts into session.background_sessions
/// (dedup by id) and emits SaveSession so the summary is persisted.
#[test]
fn test_redux_background_session_recorded_upserts() {
    let mut state = s();
    let e1 = state.reduce(Action::BackgroundSessionRecorded {
        summary: bg_summary("run-1", "running"),
    });
    // P0 (v4 review): a record that changed state must emit SaveSession
    // (the only durable write path). It must NOT redraw — a background
    // summary write is invisible to the live conversation surface.
    assert_eq!(e1.len(), 1, "exactly one effect: SaveSession");
    assert!(
        matches!(e1.first(), Some(Effect::SaveSession(_))),
        "changed record must emit SaveSession, got {e1:?}"
    );
    assert_eq!(state.session.background_sessions.len(), 1);

    // Same id again with a terminal status replaces, does not duplicate,
    // and still emits a fresh SaveSession (state changed). Reuse the same
    // value for the no-op check below (bg_summary stamps a fresh
    // created_at per call, which would otherwise count as a change).
    let completed = bg_summary("run-1", "completed");
    let e2 = state.reduce(Action::BackgroundSessionRecorded {
        summary: completed.clone(),
    });
    assert!(matches!(e2.first(), Some(Effect::SaveSession(_))));
    assert_eq!(state.session.background_sessions.len(), 1);
    assert_eq!(
        state.session.background_sessions.first().map(|s| s.status.as_str()),
        Some("completed")
    );

    // An identical re-record is a no-op: no state change, no effect, no
    // save storm.
    let e_dup = state.reduce(Action::BackgroundSessionRecorded { summary: completed });
    assert!(
        e_dup.is_empty(),
        "unchanged re-record must short-circuit with no SaveSession (no save storm)"
    );

    // A different id appends and saves.
    let e3 = state.reduce(Action::BackgroundSessionRecorded {
        summary: bg_summary("run-2", "failed"),
    });
    assert!(matches!(e3.first(), Some(Effect::SaveSession(_))));
    assert_eq!(state.session.background_sessions.len(), 2);
}

/// B1 (P0, v4 review): dispatching BackgroundSessionRecorded must emit a
/// SaveSession whose snapshot ALREADY contains the just-recorded summary.
/// This is the regression guard: previously the reducer returned no
/// effect, so the terminal-summary never reached the memory backend
/// (legacy exit-save is disabled under terminal-tui), breaking reload
/// recap. Round-trip: capture the SaveSession snapshot → reload it into a
/// fresh state → the background summary is present.
#[test]
fn test_redux_background_session_recorded_emits_savesession_with_summary() {
    let mut state = s();
    let effects = state.reduce(Action::BackgroundSessionRecorded {
        summary: bg_summary("run-42", "completed"),
    });
    let snapshot = effects
        .iter()
        .find_map(|e| match e {
            Effect::SaveSession(session) => Some(session.clone()),
            _ => None,
        })
        .expect("BackgroundSessionRecorded must emit Effect::SaveSession");
    // The emitted snapshot must already carry the recorded summary —
    // proving the save happens AFTER the upsert (no race where the
    // snapshot predates the state mutation).
    assert_eq!(
        snapshot.background_sessions.len(),
        1,
        "SaveSession snapshot must contain the just-recorded child session"
    );
    let recorded = snapshot
        .background_sessions
        .first()
        .expect("snapshot child session present");
    assert_eq!(recorded.id, "run-42");
    assert_eq!(recorded.status, "completed");

    // Round-trip: reloading that snapshot into a fresh state restores it,
    // confirming the persisted blob is sufficient for reload recap.
    let mut reloaded = s();
    let _ = reloaded.reduce(Action::SessionLoaded(snapshot));
    assert_eq!(reloaded.session.background_sessions.len(), 1);
    assert_eq!(
        reloaded.session.background_sessions.first().map(|s| s.id.as_str()),
        Some("run-42")
    );
}

/// v4: a recorded child session must survive a save→load round trip
/// through the reducer (snapshot persists it, SessionLoaded restores it),
/// and a still-running session is never restored as a live one.
#[test]
fn test_redux_background_sessions_survive_snapshot_and_reload() {
    let mut state = s();
    let _ = state.reduce(Action::BackgroundSessionRecorded {
        summary: bg_summary("run-1", "completed"),
    });
    // An interrupted entry stands in for "was running at last exit".
    let _ = state.reduce(Action::BackgroundSessionRecorded {
        summary: bg_summary("run-2", crate::chat::sessions::model::STATUS_INTERRUPTED),
    });

    // The snapshot the SaveSession effect would persist must carry them.
    let snapshot = state.build_session_snapshot();
    assert_eq!(snapshot.background_sessions.len(), 2);

    // Reloading that snapshot into a fresh state restores the summaries.
    let mut fresh = s();
    let _ = fresh.reduce(Action::SessionLoaded(snapshot));
    assert_eq!(fresh.session.background_sessions.len(), 2);
    // None of the restored entries is a live status — reload never
    // resurrects a running process.
    for bg in &fresh.session.background_sessions {
        assert_ne!(bg.status, "running");
        assert_ne!(bg.status, "needs-input");
    }
    let statuses: Vec<&str> = fresh
        .session
        .background_sessions
        .iter()
        .map(|s| s.status.as_str())
        .collect();
    assert!(statuses.contains(&"completed"));
    assert!(statuses.contains(&crate::chat::sessions::model::STATUS_INTERRUPTED));
}

/// Step4-5b: SessionLoaded with a system prompt — user/assistant are kept in history, the
/// system turn does not enter the LLM history (role=system turns are filtered out)
#[test]
fn test_redux_session_loaded_only_user_assistant_in_history() {
    use crate::chat::session::{ChatSession, ChatTurn};
    let mut state = s();
    let mut loaded = ChatSession::new("p", "m");
    loaded.turns.push(ChatTurn {
        role: "system".to_string(),
        content: "You are helpful".to_string(),
        timestamp: chrono::Utc::now(),
        tool_calls: vec![],
    });
    loaded.add_user_turn("q");
    let _ = state.reduce(Action::SessionLoaded(loaded));
    // history holds only user, no system (SessionLoaded does not add system to history)
    assert_eq!(
        state.session.history.len(),
        1,
        "only user turns enter history (role=system is filtered out)"
    );
    assert_eq!(
        state.session.history.first().map(|m| m.role.as_str()),
        Some("user"),
        "history[0] must have the user role"
    );
}

/// A session saved before the delivery guard existed still holds
/// fabricated `[Used tools: ...]` claims — in both grains. Resuming it
/// is a replay seam: the context copy handed back to the model must not
/// re-teach it the shape, while the persisted turns (and therefore the
/// rendered transcript) keep what was actually produced, and a user turn
/// quoting the marker is never rewritten.
///
/// MUTATION GUARD: drop the strip in `reduce_session_loaded` and the
/// assistant-history assertions fail with the marker still in context.
#[test]
fn test_redux_session_loaded_strips_fabricated_tool_markers_from_assistant_history() {
    use crate::chat::session::ChatSession;
    let mut state = s();
    let mut loaded = ChatSession::new("p", "m");
    loaded.add_user_turn("you wrote [Used tools: shell] last time");
    loaded.add_assistant_turn("Fixture checked. [Used tools: shell]", vec![]);
    loaded.add_user_turn("and now?");
    loaded.add_assistant_turn("Nothing new.\n[Used tools: shell]", vec![]);

    let _ = state.reduce(Action::SessionLoaded(loaded));

    let history: Vec<&str> = state.session.history.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        history,
        vec![
            "you wrote [Used tools: shell] last time",
            "Fixture checked.",
            "and now?",
            "Nothing new."
        ],
        "assistant turns are cleaned in both grains; the user turn is real content"
    );
    assert_eq!(
        state.session.turns.get(1).map(|t| t.content.as_str()),
        Some("Fixture checked. [Used tools: shell]"),
        "the persisted turn keeps the bytes the model actually produced"
    );
}

/// Step4-6: SessionSaved updates session.id
#[test]
fn test_redux_session_saved_updates_id() {
    let mut state = s();
    state.session.id = String::new(); // simulate not having an id yet
    let effects = state.reduce(Action::SessionSaved {
        id: "new-id-123".to_string(),
    });
    assert_eq!(state.session.id, "new-id-123");
    assert!(has_log_trace(&effects), "must contain LogTrace");
}

/// Step4-6b: SessionSaved with the same id — unchanged (idempotent)
#[test]
fn test_redux_session_saved_same_id_idempotent() {
    let mut state = s();
    state.session.id = "already-set".to_string();
    let effects = state.reduce(Action::SessionSaved {
        id: "already-set".to_string(),
    });
    assert_eq!(state.session.id, "already-set");
    assert!(has_log_trace(&effects));
}

/// Step4-7: SessionSwitched → SaveSession + LogTrace + RequestRedraw
#[test]
fn test_redux_session_switched_saves_current_then_logs() {
    let mut state = s();
    state.session.id = "cur-session".to_string();
    state.session.title = "Current".to_string();
    let effects = state.reduce(Action::SessionSwitched {
        id: "new-session".to_string(),
    });
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::SaveSession(sess) if sess.id == "cur-session")),
        "the current session must be saved first"
    );
    assert!(has_log_trace(&effects), "must contain LogTrace");
    assert!(has_request_redraw(&effects), "must contain RequestRedraw");
}

/// P2-C: SessionSwitched effects[0] is exactly SaveSession (save first in the two-step flow)
#[test]
fn test_redux_session_switched_emits_save_first() {
    let mut state = s();
    state.session.id = "session-x".to_string();
    let effects = state.reduce(Action::SessionSwitched {
        id: "session-y".to_string(),
    });
    assert!(!effects.is_empty(), "SessionSwitched must emit at least 1 effect");
    assert!(
        matches!(effects.first(), Some(Effect::SaveSession(sess)) if sess.id == "session-x"),
        "effects[0] must be SaveSession(current), got: {:?}",
        effects.first()
    );
}

/// Step4-8: RecordUserTurn → session.turns + history grow, updated_at refreshed, title set
#[test]
fn test_redux_record_user_turn_grows_history() {
    let mut state = s();
    assert_eq!(state.session.turns.len(), 0);
    assert_eq!(state.session.history.len(), 0);
    assert!(state.session.title.is_empty(), "the initial title is empty");
    let effects = state.reduce(Action::RecordUserTurn("what is Rust?".to_string()));
    assert_eq!(state.session.turns.len(), 1, "turns grew");
    assert_eq!(state.session.history.len(), 1, "history grew");
    assert_eq!(
        state.session.turns.first().map(|t| t.role.as_str()),
        Some("user"),
        "turns[0] role"
    );
    assert_eq!(
        state.session.history.first().map(|m| m.role.as_str()),
        Some("user"),
        "history[0] role"
    );
    assert_eq!(
        state.session.history.first().map(|m| m.content.as_str()),
        Some("what is Rust?"),
        "history[0] content"
    );
    // the first user turn sets the title automatically
    assert_eq!(
        state.session.title, "what is Rust?",
        "the first user turn must set_title"
    );
    assert!(has_log_trace(&effects));
}

/// Step4-8b: a second RecordUserTurn must not overwrite an existing title
#[test]
fn test_redux_record_user_turn_no_overwrite_existing_title() {
    let mut state = s();
    state.session.title = "My Chat".to_string();
    let _ = state.reduce(Action::RecordUserTurn("second question".to_string()));
    assert_eq!(
        state.session.title, "My Chat",
        "an existing title must not be overwritten"
    );
}

/// Step4-9: RecordAssistantTurn → session.turns + history grow, updated_at refreshed
#[test]
fn test_redux_record_assistant_turn_grows_history() {
    let mut state = s();
    let _ = state.reduce(Action::RecordUserTurn("hello".to_string()));
    let effects = state.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: "Rust is fast.".to_string(),
    });
    assert_eq!(state.session.turns.len(), 2, "turns grew to 2");
    assert_eq!(state.session.history.len(), 2, "history grew to 2");
    assert_eq!(
        state.session.turns.last().map(|t| t.role.as_str()),
        Some("assistant"),
        "turns.last() role"
    );
    assert_eq!(
        state.session.history.last().map(|m| m.role.as_str()),
        Some("assistant"),
        "history.last() role"
    );
    assert_eq!(
        state.session.history.last().map(|m| m.content.as_str()),
        Some("Rust is fast."),
        "history.last() content"
    );
    assert!(has_log_trace(&effects));
}

/// Step4-10: HistoryCleared — keeps the system prompt, clears user/assistant
#[test]
fn test_redux_history_cleared_keeps_system_prompt() {
    let mut state = s();
    // build system + user + assistant
    state
        .session
        .history
        .push(crate::providers::ChatMessage::system("Be helpful"));
    state.session.history.push(crate::providers::ChatMessage::user("hi"));
    state
        .session
        .history
        .push(crate::providers::ChatMessage::assistant("hello!"));
    state
        .ui
        .conversation_lines
        .push(crate::chat::tui::ConversationLine::User {
            content: "hi".to_string(),
        });
    assert_eq!(state.session.history.len(), 3);

    let effects = state.reduce(Action::HistoryCleared);

    // history keeps only the system prompt
    assert_eq!(
        state.session.history.len(),
        1,
        "only the system prompt may remain after clearing"
    );
    assert_eq!(
        state.session.history.first().map(|m| m.role.as_str()),
        Some("system"),
        "the kept history[0] must be system"
    );
    // conversation_lines is cleared
    assert!(
        state.ui.conversation_lines.is_empty(),
        "UI conversation_lines must be cleared"
    );
    assert!(has_request_redraw(&effects));
    assert!(has_log_trace(&effects));
}

/// Step4-10b: HistoryCleared without a system prompt — clears everything
#[test]
fn test_redux_history_cleared_no_system_prompt_clears_all() {
    let mut state = s();
    state.session.history.push(crate::providers::ChatMessage::user("q"));
    state
        .session
        .history
        .push(crate::providers::ChatMessage::assistant("a"));
    let effects = state.reduce(Action::HistoryCleared);
    assert!(
        state.session.history.is_empty(),
        "without a system prompt everything is cleared"
    );
    assert!(has_request_redraw(&effects));
}

/// P2-D: HistoryCleared with system not first — it is still kept (defensive full scan)
#[test]
fn test_redux_history_cleared_preserves_system_in_middle() {
    let mut state = s();
    // deliberately put system in the middle (unusual order, handled defensively)
    state.session.history.push(crate::providers::ChatMessage::user("q1"));
    state
        .session
        .history
        .push(crate::providers::ChatMessage::system("Be helpful"));
    state
        .session
        .history
        .push(crate::providers::ChatMessage::assistant("a1"));
    assert_eq!(state.session.history.len(), 3);

    let _effects = state.reduce(Action::HistoryCleared);

    // the system message must be kept, user/assistant cleared
    assert_eq!(state.session.history.len(), 1, "only 1 system message may remain");
    assert_eq!(
        state.session.history.first().map(|m| m.role.as_str()),
        Some("system"),
        "the kept message must be the system one"
    );
}

/// Step4-11: the full double Ctrl+C flow (including the Effect sequence)
///
/// t=100: KeyPressed(Ctrl+C) → single press, last_ctrlc_ms=100, no Quit
/// t=300: KeyPressed(Ctrl+C) → double press (<500ms) → Quit effect
#[test]
fn test_redux_double_ctrl_c_flow_e2e() {
    let mut state = s();
    let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

    // first Ctrl+C at t=100
    let effects1 = state.reduce_with_now(Action::KeyPressed(key.clone()), 100);
    assert!(!has_quit(&effects1), "the first Ctrl+C must not Quit");
    assert_eq!(state.ui.last_ctrlc_ms, 100, "records the window timestamp");

    // second Ctrl+C at t=300 (within 100ms)
    let effects2 = state.reduce_with_now(Action::KeyPressed(key), 300);
    assert!(
        has_quit(&effects2),
        "a Ctrl+C double press within 300ms must produce Quit"
    );

    // check that Effect::Quit is in the result
    let has_quit_effect = effects2.iter().any(|e| matches!(e, Effect::Quit));
    assert!(has_quit_effect, "effects2 must contain Effect::Quit");
}

/// A turn that degrades on every tool iteration owes the user one line,
/// not one per iteration — and the next turn is a new fact, so the latch
/// has to re-arm.
#[test]
fn context_degradation_notice_is_surfaced_once_per_turn() {
    use crate::chat::action::CompactReason;
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "draft-1".to_string(),
        cancel: CancellationToken::new(),
    });

    let first = state.reduce(Action::HistoryCompactionDegraded {
        reason: CompactReason::ContextOverflow,
        dropped_messages: 7,
    });
    let notices = first
        .iter()
        .filter_map(|effect| match effect {
            Effect::SurfaceNotice { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        notices.len(),
        1,
        "the first degradation must surface exactly one notice"
    );
    let notice = notices.first().cloned().unwrap_or_default();
    assert!(
        notice.contains("lossy") && notice.contains('7'),
        "the notice must say the trim was lossy and how much it dropped: {notice}"
    );
    assert!(
        !notice.contains('\n'),
        "the notice must stay a single line for plain mode: {notice:?}"
    );

    let second = state.reduce(Action::HistoryCompactionDegraded {
        reason: CompactReason::ContextOverflow,
        dropped_messages: 4,
    });
    assert!(
        !second
            .iter()
            .any(|effect| matches!(effect, Effect::SurfaceNotice { .. })),
        "a second degradation inside the same turn must not repeat the notice"
    );

    #[cfg(feature = "terminal-tui")]
    {
        let lines = state
            .ui
            .conversation_lines
            .iter()
            .filter(|line| {
                matches!(line, crate::chat::tui::ConversationLine::System { content }
                            if content.contains("lossy"))
            })
            .count();
        assert_eq!(lines, 1, "the transcript must carry the notice exactly once");
    }

    let _ = state.reduce(Action::TurnStarted {
        draft_id: "draft-2".to_string(),
        cancel: CancellationToken::new(),
    });
    let next_turn = state.reduce(Action::HistoryCompactionDegraded {
        reason: CompactReason::ContextOverflow,
        dropped_messages: 2,
    });
    assert!(
        next_turn
            .iter()
            .any(|effect| matches!(effect, Effect::SurfaceNotice { .. })),
        "a new turn that degrades is a new fact and must be surfaced again"
    );
}

/// S2-B Step 1: HistoryCompacted algorithm baseline — keep system + truncate each + cap the total
#[test]
fn test_redux_history_compacted_basic_algorithm() {
    use crate::chat::action::CompactReason;
    let mut state = s();
    // build 1 system + 20 long user/assistant messages
    state
        .session
        .history
        .push(crate::providers::ChatMessage::system("system prompt - keep me"));
    for i in 0..20 {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        let content = "x".repeat(500); // over COMPACT_CONTENT_CHARS=320
        state.session.history.push(crate::providers::ChatMessage {
            role: role.to_string(),
            content: format!("{content} #{i}"),
        });
    }
    let before_len = state.session.history.len();
    assert_eq!(before_len, 21);

    let effects = state.reduce(Action::HistoryCompacted {
        reason: CompactReason::ContextOverflow,
    });

    // the system prompt must stay in first position
    assert_eq!(
        state.session.history.first().map(|m| m.role.as_str()),
        Some("system"),
        "system must still be first after compaction"
    );
    // the total count must be <= 1 system + COMPACT_KEEP_MESSAGES
    assert!(
        state.session.history.len() <= 1 + super::COMPACT_KEEP_MESSAGES,
        "after compaction the non-system count must be <= COMPACT_KEEP_MESSAGES, got {}",
        state.session.history.len()
    );
    // each non-system message must have <= COMPACT_CONTENT_CHARS chars (+3 for the "...")
    for m in state.session.history.iter().skip(1) {
        assert!(
            m.content.chars().count() <= super::COMPACT_CONTENT_CHARS + 3,
            "non-system msg should be truncated, got {} chars",
            m.content.chars().count()
        );
    }
    // the total budget (non-system) must be <= COMPACT_TOTAL_CHARS
    let non_system_chars: usize = state
        .session
        .history
        .iter()
        .skip(1)
        .map(|m| m.content.chars().count())
        .sum();
    assert!(
        non_system_chars <= super::COMPACT_TOTAL_CHARS,
        "non-system total chars {non_system_chars} > budget {}",
        super::COMPACT_TOTAL_CHARS
    );
    // LogTrace must be emitted
    assert!(has_log_trace(&effects), "HistoryCompacted must emit LogTrace");
}

/// S2-B Step 1: HistoryCompacted is a no-op when len<=1
#[test]
fn test_redux_history_compacted_noop_when_short() {
    use crate::chat::action::CompactReason;
    let mut state = s();
    // only 1 system message → nothing to compact
    state
        .session
        .history
        .push(crate::providers::ChatMessage::system("only system"));
    let effects = state.reduce(Action::HistoryCompacted {
        reason: CompactReason::Manual,
    });
    assert_eq!(state.session.history.len(), 1, "unchanged when len<=1");
    assert!(has_log_trace(&effects), "a no-op still emits LogTrace(DEBUG)");
}

#[test]
fn redux_compaction_patch_applies_exactly_and_matches_driver_history() {
    use crate::chat::action::CompactReason;
    let mut state = s();
    state.session.history = vec![
        crate::providers::ChatMessage::system("sys"),
        crate::providers::ChatMessage::user("old user"),
        crate::providers::ChatMessage::assistant("old assistant"),
        crate::providers::ChatMessage::user("recent user"),
    ];
    let mut driver_history = state.session.history.clone();
    let guard = crate::agent::compaction::compaction_patch_guard_for(&driver_history, 1, 3).expect("guard");
    let patch = crate::agent::turn_contracts::CompactionPatch {
        range_start: 1,
        range_end: 3,
        replacement: vec![crate::providers::ChatMessage::assistant(
            "[Context compacted at test. Summary: PROVIDER_SUMMARY_MARKER]",
        )],
        append_after: vec![crate::providers::ChatMessage::user(
            "[Post-compaction context refresh]\nre-read",
        )],
        guard,
    };
    let config = crate::config::AgentCompactionConfig {
        max_context_tokens: 10_000,
        reserve_tokens: 10,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };

    crate::agent::compaction::apply_compaction_patch_exact(&mut driver_history, &patch);
    let effects = state.reduce(Action::HistoryCompactionPatchApplied {
        reason: CompactReason::ContextOverflow,
        patch,
        compaction_config: config,
    });

    assert_eq!(
        messages_as_pairs(&state.session.history),
        messages_as_pairs(&driver_history),
        "GP-6: reducer history must exactly match driver history after patch"
    );
    assert!(
        state
            .session
            .history
            .iter()
            .any(|message| message.content.contains("PROVIDER_SUMMARY_MARKER")),
        "provider summary marker must be present"
    );
    assert!(has_log_trace(&effects));
}

#[test]
fn compaction_patch_refresh_position_parity_between_legacy_and_redux() {
    use crate::chat::action::CompactReason;
    let current_question = "What should ISS-037 answer now?";
    let mut state = s();
    state.session.history = vec![
        crate::providers::ChatMessage::system("sys"),
        crate::providers::ChatMessage::user("old user"),
        crate::providers::ChatMessage::assistant("old assistant"),
        crate::providers::ChatMessage::user(current_question),
    ];
    let mut legacy_history = state.session.history.clone();
    let guard = crate::agent::compaction::compaction_patch_guard_for(&legacy_history, 1, 3).expect("guard");
    let patch = crate::agent::turn_contracts::CompactionPatch {
        range_start: 1,
        range_end: 3,
        replacement: vec![crate::providers::ChatMessage::assistant(
            "[Context compacted at test. Summary: ISS-037 parity summary]",
        )],
        append_after: vec![crate::providers::ChatMessage::user(
            "[Post-compaction context refresh]\nre-read",
        )],
        guard,
    };
    let config = crate::config::AgentCompactionConfig {
        max_context_tokens: 10_000,
        reserve_tokens: 10,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };

    crate::agent::compaction::apply_compaction_patch_exact(&mut legacy_history, &patch);
    let _ = state.reduce(Action::HistoryCompactionPatchApplied {
        reason: CompactReason::ContextOverflow,
        patch,
        compaction_config: config,
    });

    assert_eq!(
        messages_as_pairs(&state.session.history),
        messages_as_pairs(&legacy_history),
        "GP-6: legacy and Redux histories must match exactly after the shared patch primitive"
    );
    let refresh_index = state
        .session
        .history
        .iter()
        .position(|message| message.content.starts_with("[Post-compaction context refresh]"))
        .expect("refresh marker should be present");
    let summary_index = state
        .session
        .history
        .iter()
        .position(|message| message.content.contains("ISS-037 parity summary"))
        .expect("summary marker should be present");
    let question_index = state
        .session
        .history
        .iter()
        .position(|message| message.content == current_question)
        .expect("current question should be present");
    assert!(
        summary_index < refresh_index && refresh_index < question_index,
        "refresh marker must sit after the summary and before the real current question"
    );
    assert_eq!(
        state.session.history.last().map(|message| message.content.as_str()),
        Some(current_question),
        "real current user question must remain the trailing provider-bound user message"
    );
}

#[test]
fn post_compaction_refresh_not_persisted_as_session_turn() {
    use crate::chat::action::CompactReason;
    let current_question = "Persist this as the real user turn";
    let assistant_reply = "assistant reply bound to the real user turn";
    let mut state = s();
    state.session.history = vec![
        crate::providers::ChatMessage::system("sys"),
        crate::providers::ChatMessage::user("old user"),
        crate::providers::ChatMessage::assistant("old assistant"),
    ];
    let _ = state.reduce(Action::RecordUserTurn(current_question.to_string()));
    let guard = crate::agent::compaction::compaction_patch_guard_for(&state.session.history, 1, 3).expect("guard");
    let patch = crate::agent::turn_contracts::CompactionPatch {
        range_start: 1,
        range_end: 3,
        replacement: vec![crate::providers::ChatMessage::assistant(
            "[Context compacted at test. Summary: persisted-turn summary]",
        )],
        append_after: vec![crate::providers::ChatMessage::user(
            "[Post-compaction context refresh]\nre-read",
        )],
        guard,
    };
    let config = crate::config::AgentCompactionConfig {
        max_context_tokens: 10_000,
        reserve_tokens: 10,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };

    let _ = state.reduce(Action::HistoryCompactionPatchApplied {
        reason: CompactReason::ContextOverflow,
        patch,
        compaction_config: config,
    });
    let _ = state.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: assistant_reply.to_string(),
    });

    assert_eq!(
        state.session.turns.len(),
        3,
        "the compaction summary, real user, and assistant turns are persisted"
    );
    let [summary_turn, user_turn, assistant_turn] = state.session.turns.as_slice() else {
        panic!("expected summary, real user turn, and assistant turn");
    };
    assert_eq!(summary_turn.role, "assistant");
    assert!(summary_turn.content.contains("persisted-turn summary"));
    assert_eq!(user_turn.role, "user");
    assert_eq!(user_turn.content, current_question);
    assert_eq!(assistant_turn.role, "assistant");
    assert_eq!(assistant_turn.content, assistant_reply);
    assert!(
        state
            .session
            .turns
            .iter()
            .all(|turn| !turn.content.starts_with("[Post-compaction context refresh]")),
        "refresh marker must remain a history context marker, not a persisted user turn"
    );

    let snapshot = state.build_session_snapshot();
    let mut reloaded = s();
    let _ = reloaded.reduce(Action::SessionLoaded(snapshot));
    assert_eq!(
        messages_as_pairs(&reloaded.session.history),
        vec![
            (
                "assistant".to_string(),
                "[Context compacted at test. Summary: persisted-turn summary]".to_string()
            ),
            ("user".to_string(), current_question.to_string()),
            ("assistant".to_string(), assistant_reply.to_string()),
        ],
        "resume must rebuild the compacted durable turn shape"
    );
}

#[test]
fn redux_compaction_patch_guard_mismatch_falls_back_without_stale_patch() {
    use crate::chat::action::CompactReason;
    let mut state = s();
    let original = vec![
        crate::providers::ChatMessage::system("sys"),
        crate::providers::ChatMessage::user(format!("old user {}", "x ".repeat(180))),
        crate::providers::ChatMessage::assistant(format!("old assistant {}", "y ".repeat(180))),
        crate::providers::ChatMessage::user(format!("recent {}", "z ".repeat(180))),
    ];
    let guard = crate::agent::compaction::compaction_patch_guard_for(&original, 1, 3).expect("guard");
    state.session.history = original;
    state
        .session
        .history
        .push(crate::providers::ChatMessage::user("mutation before reducer"));
    let patch = crate::agent::turn_contracts::CompactionPatch {
        range_start: 1,
        range_end: 3,
        replacement: vec![crate::providers::ChatMessage::assistant(
            "[Context compacted at test. Summary: STALE_PROVIDER_SUMMARY]",
        )],
        append_after: vec![crate::providers::ChatMessage::user("stale refresh")],
        guard,
    };
    let config = crate::config::AgentCompactionConfig {
        max_context_tokens: 90,
        reserve_tokens: 10,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };

    let effects = state.reduce(Action::HistoryCompactionPatchApplied {
        reason: CompactReason::ContextOverflow,
        patch,
        compaction_config: config.clone(),
    });

    assert!(
        state
            .session
            .history
            .iter()
            .all(|message| !message.content.contains("STALE_PROVIDER_SUMMARY")),
        "guard mismatch must not apply stale provider patch"
    );
    assert!(
        crate::agent::context_budget::plan_context_budget(
            &state.session.history,
            &config,
            crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD
        )
        .used_tokens
            <= 80,
        "fallback trim must bring history under literal hard limit 80"
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::LogTrace {
            level: tracing::Level::WARN,
            msg
        } if msg.contains("guard mismatch")
    )));
}

/// Step4-12: CancelRequested twice — the second one is a no-op (generating=false)
#[test]
fn test_redux_cancel_requested_twice_second_noop() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    // first cancel
    let effects1 = state.reduce(Action::CancelRequested);
    assert!(!effects1.is_empty(), "the first cancel must produce effects");
    // second cancel — generating is already false → no-op
    let effects2 = state.reduce(Action::CancelRequested);
    assert!(
        effects2.is_empty(),
        "a second CancelRequested(generating=false) must be a no-op"
    );
}

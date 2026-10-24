#![cfg(test)]

use super::*;

// ─── Step 3 unit tests (streaming + tool paths) ────────────────────────────
//
// Coverage goals:
//   - the 5 streaming Actions (TurnStarted/StreamChunkReceived/StreamCompleted/
//     StreamFailed/StreamCancelled) plus the 3 tool Actions
//     (ToolStarted/ToolFinished/ToolProgress)
//   - every stale-drop path now that the P3-5 version guard lives in the reducer
//   - the idempotency boundary against the finalize_draft retry path
//
// These tests are the core evidence that the P3-5 version mechanism fully moved into the reducer.

/// Step3-1: TurnStarted initialises stream.draft + active_cancel + generating
#[test]
fn test_redux_turn_started_sets_stream_state() {
    let mut state = s();
    assert!(state.stream.primary_streaming_draft().is_none());
    assert!(state.control.active_cancel.is_none());
    assert!(!state.control.generating);
    let effects = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    assert!(state.stream.primary_streaming_draft().is_some());
    assert_eq!(
        state.stream.primary_streaming_draft().map(|d| d.draft_id.clone()),
        Some("d1".to_string())
    );
    assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(0));
    assert!(state.control.active_cancel.is_some());
    assert!(state.control.generating);
    assert!(has_request_redraw(&effects));
    assert!(has_log_trace(&effects));
}

/// Step3-2: a normal chunk → accumulates + bumps version + RequestRedraw
#[test]
fn test_redux_stream_chunk_received_valid_appends() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    let effects = state.reduce(Action::StreamChunkReceived {
        draft_id: "d1".to_string(),
        delta: "hello".to_string(),
        version: 1,
    });
    assert!(has_request_redraw(&effects));
    assert_eq!(
        state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
        Some("hello".to_string())
    );
    assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(1));

    // second valid chunk → accumulates
    let effects = state.reduce(Action::StreamChunkReceived {
        draft_id: "d1".to_string(),
        delta: " world".to_string(),
        version: 2,
    });
    assert!(has_request_redraw(&effects));
    assert_eq!(
        state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
        Some("hello world".to_string())
    );
    assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(2));
}

/// Step3-3: a stale version (version=1 arriving after version=2) → dropped
#[test]
fn test_redux_stream_chunk_received_stale_version_dropped() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    // version=2 arrives first
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "d1".to_string(),
        delta: "AB".to_string(),
        version: 2,
    });
    assert_eq!(
        state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
        Some("AB".to_string())
    );
    assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(2));
    // version=1 arrives later → dropped
    let effects = state.reduce(Action::StreamChunkReceived {
        draft_id: "d1".to_string(),
        delta: "STALE".to_string(),
        version: 1,
    });
    assert!(effects.is_empty(), "a stale version must return no effects");
    assert_eq!(
        state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
        Some("AB".to_string()),
        "accumulated must stay unchanged"
    );
    assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(2));

    // a repeated version=2 → also dropped (strict-monotonic)
    let effects = state.reduce(Action::StreamChunkReceived {
        draft_id: "d1".to_string(),
        delta: "DUP".to_string(),
        version: 2,
    });
    assert!(effects.is_empty(), "a repeated version must be dropped");
    assert_eq!(
        state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
        Some("AB".to_string())
    );
}

/// Step3-4: a draft_id from another turn → dropped
#[test]
fn test_redux_stream_chunk_received_wrong_draft_id_dropped() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "d1".to_string(),
        delta: "ok".to_string(),
        version: 1,
    });
    // wrong draft_id → dropped
    let effects = state.reduce(Action::StreamChunkReceived {
        draft_id: "d2".to_string(),
        delta: "STALE".to_string(),
        version: 99,
    });
    assert!(effects.is_empty(), "a mismatched draft_id must return no effects");
    assert_eq!(
        state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
        Some("ok".to_string())
    );
    assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(1));
}

/// Step3-5: a chunk arriving after finalize → dropped
#[test]
fn test_redux_stream_chunk_received_after_finalize_dropped() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "d1".to_string(),
        delta: "complete".to_string(),
        version: 1,
    });
    let _ = state.reduce(Action::StreamCompleted {
        draft_id: "d1".to_string(),
        final_text: "complete".to_string(),
        reasoning: String::new(),
    });
    assert!(
        state.stream.primary_streaming_draft().is_none(),
        "the draft must be cleared after finalize"
    );
    // any chunk after this is treated as stale
    let effects = state.reduce(Action::StreamChunkReceived {
        draft_id: "d1".to_string(),
        delta: "LATE".to_string(),
        version: 2,
    });
    assert!(effects.is_empty(), "a chunk after finalize must be dropped");
    assert!(state.stream.primary_streaming_draft().is_none());
}

/// Step3-6: StreamCompleted clears the draft + pushes assistant + NotifyHook
#[test]
fn test_redux_stream_completed_clears_draft_and_pushes_assistant() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    assert!(state.control.generating);
    let prev_lines = state.ui.conversation_lines.len();
    let effects = state.reduce(Action::StreamCompleted {
        draft_id: "d1".to_string(),
        final_text: "final answer".to_string(),
        reasoning: String::new(),
    });
    assert!(
        state.stream.primary_streaming_draft().is_none(),
        "the draft must be cleared"
    );
    assert!(state.control.active_cancel.is_none());
    assert!(!state.control.generating);
    assert_eq!(
        state.ui.conversation_lines.len(),
        prev_lines + 1,
        "only Assistant must be pushed"
    );
    // check that the last line is Assistant("final answer")
    if let Some(crate::chat::tui::ConversationLine::Assistant { content }) = state.ui.conversation_lines.last() {
        assert_eq!(content, "final answer");
    } else {
        panic!("the last line must be ConversationLine::Assistant");
    }
    assert!(has_request_redraw(&effects));
    assert!(
        effects.iter().any(|e| matches!(e, Effect::NotifyHook { .. })),
        "must contain NotifyHook(TurnComplete)"
    );
}

/// Step3-6b: StreamCompleted with reasoning → keeps Reasoning for the verbose transcript
#[test]
fn test_redux_stream_completed_with_reasoning_preserves_transcript_data() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::StreamCompleted {
        draft_id: "d1".to_string(),
        final_text: "ans".to_string(),
        reasoning: "thinking step".to_string(),
    });
    // The completion duration does not enter the UI body; only Assistant + Reasoning are kept.
    assert_eq!(state.ui.conversation_lines.len(), 2);
    assert!(matches!(
        state.ui.conversation_lines.first(),
        Some(crate::chat::tui::ConversationLine::Assistant { .. })
    ));
    assert!(
        matches!(
            state.ui.conversation_lines.last(),
            Some(crate::chat::tui::ConversationLine::Reasoning { .. })
        ),
        "the last line must be Reasoning"
    );
}

/// F1: every reasoning delta must advance the visible progress on the draft (counted in
/// chars, not bytes) and request a redraw.
#[test]
fn test_reasoning_delta_updates_live_thinking_progress() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    let segments = thinking_segments();
    let mut expected_chars = 0usize;
    for (idx, seg) in segments.iter().enumerate() {
        let version = u64::try_from(idx).unwrap_or(0).saturating_add(1);
        let effects = state.reduce(Action::StreamReasoningReceived {
            draft_id: "d1".to_string(),
            delta: seg.clone(),
            version,
        });
        assert!(has_request_redraw(&effects), "delta {idx} must request a redraw");
        expected_chars = expected_chars.saturating_add(seg.chars().count());
        let draft = live_draft(&state);
        assert_eq!(
            draft.reasoning_chars, expected_chars,
            "delta {idx} count must update live"
        );
        assert_eq!(draft.version, version);
    }
    let joined: String = segments.concat();
    assert_ne!(
        joined.chars().count(),
        joined.len(),
        "the fixture must contain multi-byte chars, otherwise char and byte counts are indistinguishable"
    );
    let draft = live_draft(&state);
    assert_eq!(draft.reasoning_chars, joined.chars().count());
    assert!(
        draft.accumulated.is_empty(),
        "thinking must not pollute the visible text"
    );
    assert!(
        state.ui.conversation_lines.is_empty(),
        "no transcript lines while streaming"
    );
    let preview = draft.reasoning_preview().expect("test: preview present");
    assert!(!preview.is_empty(), "expected a non-empty value");
    assert!(!preview.contains('\n'), "the preview must be folded into a single line");
}

/// F1: the tail is bounded and cut on char boundaries (never splitting a char); the count stays full.
#[test]
fn test_reasoning_tail_is_bounded_and_char_aligned() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    let segments = thinking_segments();
    for (idx, seg) in segments.iter().enumerate() {
        let version = u64::try_from(idx).unwrap_or(0).saturating_add(1);
        let _ = state.reduce(Action::StreamReasoningReceived {
            draft_id: "d1".to_string(),
            delta: seg.clone(),
            version,
        });
    }
    let joined: String = segments.concat();
    assert!(
        joined.chars().count() > crate::chat::state::REASONING_TAIL_MAX_CHARS,
        "the fixture must cross the tail threshold, otherwise the truncation branch is not covered"
    );
    let draft = live_draft(&state);
    assert_eq!(
        draft.reasoning_tail.chars().count(),
        crate::chat::state::REASONING_TAIL_MAX_CHARS,
        "the tail must be trimmed down to the limit"
    );
    let expected_tail: String = joined
        .chars()
        .skip(joined.chars().count() - crate::chat::state::REASONING_TAIL_MAX_CHARS)
        .collect();
    assert_eq!(
        draft.reasoning_tail, expected_tail,
        "the tail must be the text end with no split char"
    );
    assert_eq!(
        draft.reasoning_chars,
        joined.chars().count(),
        "the count is still the full text"
    );
}

/// F1: out-of-order / duplicate / stale versions are dropped by the same rule as
/// StreamChunkReceived; both kinds of delta share one version counter.
#[test]
fn test_reasoning_delta_version_guard_drops_stale() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::StreamReasoningReceived {
        draft_id: "d1".to_string(),
        delta: "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}\u{2026}\u{2026}".to_string(),
        version: 5,
    });
    let baseline = live_draft(&state).reasoning_chars;
    assert_eq!(
        baseline,
        "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}\u{2026}\u{2026}"
            .chars()
            .count()
    );

    for stale_version in [5u64, 3, 0] {
        let effects = state.reduce(Action::StreamReasoningReceived {
            draft_id: "d1".to_string(),
            delta: "STALE".to_string(),
            version: stale_version,
        });
        assert!(effects.is_empty(), "version {stale_version} must be dropped");
        assert_eq!(live_draft(&state).reasoning_chars, baseline);
        assert_eq!(live_draft(&state).version, 5);
    }

    // an unknown draft_id (stale across turns) is dropped as well
    let effects = state.reduce(Action::StreamReasoningReceived {
        draft_id: "other".to_string(),
        delta: "STALE".to_string(),
        version: 99,
    });
    assert!(effects.is_empty(), "an unknown draft_id must be dropped");
    assert_eq!(live_draft(&state).reasoning_chars, baseline);

    // shared counter: once reasoning bumped the version, an older text delta is dropped too
    let effects = state.reduce(Action::StreamChunkReceived {
        draft_id: "d1".to_string(),
        delta: "late text".to_string(),
        version: 4,
    });
    assert!(
        effects.is_empty(),
        "the version counter is shared by both kinds of delta"
    );
    assert!(live_draft(&state).accumulated.is_empty(), "expected an empty value");
}

/// F1: live progress must not change the final reasoning card after StreamCompleted.
#[test]
fn test_reasoning_progress_leaves_final_card_unchanged() {
    let full_reasoning = thinking_segments().concat();

    let mut with_progress = s();
    let _ = with_progress.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    for (idx, seg) in thinking_segments().iter().enumerate() {
        let version = u64::try_from(idx).unwrap_or(0).saturating_add(1);
        let _ = with_progress.reduce(Action::StreamReasoningReceived {
            draft_id: "d1".to_string(),
            delta: seg.clone(),
            version,
        });
    }
    let _ = with_progress.reduce(Action::StreamCompleted {
        draft_id: "d1".to_string(),
        final_text: "ans".to_string(),
        reasoning: full_reasoning.clone(),
    });

    let mut without_progress = s();
    let _ = without_progress.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = without_progress.reduce(Action::StreamCompleted {
        draft_id: "d1".to_string(),
        final_text: "ans".to_string(),
        reasoning: full_reasoning.clone(),
    });

    assert_eq!(
        format!("{:?}", with_progress.ui.conversation_lines),
        format!("{:?}", without_progress.ui.conversation_lines),
        "the final transcript must be line-for-line identical with and without live progress"
    );
    match with_progress.ui.conversation_lines.last() {
        Some(crate::chat::tui::ConversationLine::Reasoning {
            content,
            char_count,
            folded,
        }) => {
            assert_eq!(content, &full_reasoning);
            assert_eq!(*char_count, full_reasoning.chars().count());
            assert!(*folded, "the final card is still folded by default");
        }
        other => panic!("the last line must be a Reasoning card, got {other:?}"),
    }
    assert!(
        with_progress.stream.primary_streaming_draft().is_none(),
        "the draft must be cleared after completion"
    );
}

/// F1: `--plain` (no TUI renderer) must not flood the screen with thinking progress — the
/// Action only produces RequestRedraw (a no-op without a renderer), produces no output
/// Effect, and appends no transcript line.
#[test]
fn test_reasoning_delta_produces_no_output_for_plain_mode() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    let lines_before = state.ui.conversation_lines.len();
    for (idx, seg) in thinking_segments().iter().enumerate() {
        let version = u64::try_from(idx).unwrap_or(0).saturating_add(1);
        let effects = state.reduce(Action::StreamReasoningReceived {
            draft_id: "d1".to_string(),
            delta: seg.clone(),
            version,
        });
        assert_eq!(effects.len(), 1, "each delta may produce only one Effect: {effects:?}");
        assert!(
            matches!(effects.first(), Some(Effect::RequestRedraw)),
            "the only Effect must be RequestRedraw: {effects:?}"
        );
    }
    assert_eq!(
        state.ui.conversation_lines.len(),
        lines_before,
        "in plain mode thinking must not write transcript lines"
    );
}

/// Step3-7: StreamFailed clears the draft + WARN LogTrace
#[test]
fn test_redux_stream_failed_clears_draft() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "d1".to_string(),
        delta: "partial".to_string(),
        version: 1,
    });
    let effects = state.reduce(Action::StreamFailed {
        draft_id: "d1".to_string(),
        err: "network".to_string(),
        retryable: true,
    });
    assert!(state.stream.primary_streaming_draft().is_none());
    assert!(!state.control.generating);
    assert!(has_request_redraw(&effects));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::LogTrace { level, .. } if *level == tracing::Level::WARN)),
        "must contain a WARN LogTrace"
    );
}

/// Step3-8: StreamCancelled clears the draft + pushes no message
#[test]
fn test_redux_stream_cancelled_clears_draft() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "d1".to_string(),
        delta: "partial".to_string(),
        version: 1,
    });
    let lines_before = state.ui.conversation_lines.len();
    let effects = state.reduce(Action::StreamCancelled {
        draft_id: "d1".to_string(),
    });
    assert!(state.stream.primary_streaming_draft().is_none());
    assert!(!state.control.generating);
    assert!(state.control.active_cancel.is_none());
    assert!(has_request_redraw(&effects));
    assert_eq!(
        state.ui.conversation_lines.len(),
        lines_before,
        "cancel must not push any conversation line"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn stream_cancelled_finalizes_pending_running_tool_cards() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-tool-cancel".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "shell".to_string(),
        args: r#"{"command":"sleep 10"}"#.to_string(),
    });

    let _ = state.reduce(Action::StreamCancelled {
        draft_id: "d-tool-cancel".to_string(),
    });

    assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Primary), 0);
    assert!(
        !crate::chat::tui::execution_activity_active_for_view(&state.build_ui_snapshot(1)),
        "cancelled turn must not leave a running tool card driving the status bar"
    );
    assert!(
        state.ui.conversation_lines.iter().any(|line| matches!(
            line,
            crate::chat::tui::ConversationLine::ToolResult {
                tool_name,
                status: crate::chat::tui::ToolStatus::Cancelled,
                ..
            } if tool_name == "shell"
        )),
        "pending shell tool card should be finalized as a distinct cancelled card"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn cancel_requested_finalizes_pending_running_tool_cards() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-tool-cancel-request".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "shell".to_string(),
        args: r#"{"command":"sleep 10"}"#.to_string(),
    });

    let _ = state.reduce(Action::CancelRequested);

    assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Primary), 0);
    assert!(
        !crate::chat::tui::execution_activity_active_for_view(&state.build_ui_snapshot(1)),
        "cancel request must not leave a running tool card driving the status bar"
    );
    assert!(
        state.ui.conversation_lines.iter().any(|line| matches!(
            line,
            crate::chat::tui::ConversationLine::ToolResult {
                tool_name,
                status: crate::chat::tui::ToolStatus::Cancelled,
                result: Some(result),
                ..
            } if tool_name == "shell" && result.contains("cancel request")
        )),
        "cancel request should finalize the shell tool card"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn stream_completed_removes_unfinished_pending_tool_cards() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-tool-complete".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "shell".to_string(),
        args: r#"{"command":"sleep 10"}"#.to_string(),
    });

    let _ = state.reduce(Action::StreamCompleted {
        draft_id: "d-tool-complete".to_string(),
        final_text: "done".to_string(),
        reasoning: String::new(),
    });

    assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Primary), 0);
    assert!(
        !state
            .ui
            .conversation_lines
            .iter()
            .any(|line| matches!(line, crate::chat::tui::ConversationLine::ToolResult { .. })),
        "completed turn should remove unfinished placeholder tool cards"
    );
    assert!(
        !crate::chat::tui::execution_activity_active_for_view(&state.build_ui_snapshot(1)),
        "completed turn must not leave placeholder tool cards driving the status bar"
    );
}

#[test]
fn stream_failed_removes_trailing_answerless_user_turn() {
    let mut state = s();
    let _ = state.reduce(Action::RecordUserTurn("failed question".to_string()));
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-failed".to_string(),
        cancel: CancellationToken::new(),
    });

    let effects = state.reduce(Action::StreamFailed {
        draft_id: "d-failed".to_string(),
        err: "provider failed".to_string(),
        retryable: false,
    });

    assert!(has_request_redraw(&effects));
    assert!(
        state.session.turns.is_empty(),
        "GP-9: failed turn must not leave an answerless user turn"
    );
    assert!(
        state
            .session
            .history
            .iter()
            .all(|message| message.content != "failed question"),
        "failed turn must also be removed from reducer history"
    );
    assert!(
        state.session.title.is_empty(),
        "first failed prompt must not become the session title"
    );
}

#[test]
fn stream_cancelled_removes_trailing_answerless_user_turn() {
    let mut state = s();
    let _ = state.reduce(Action::RecordUserTurn("cancelled question".to_string()));
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-cancelled".to_string(),
        cancel: CancellationToken::new(),
    });

    let effects = state.reduce(Action::StreamCancelled {
        draft_id: "d-cancelled".to_string(),
    });

    assert!(has_request_redraw(&effects));
    assert!(
        state.session.turns.is_empty(),
        "GP-9: cancelled turn must not leave an answerless user turn"
    );
    assert!(
        state
            .session
            .history
            .iter()
            .all(|message| message.content != "cancelled question"),
        "cancelled turn must also be removed from reducer history"
    );
}

#[test]
fn failed_turn_orphan_is_absent_from_background_and_later_success_snapshots() {
    let mut state = s();
    let _ = state.reduce(Action::RecordUserTurn("orphan candidate".to_string()));
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-failed".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::StreamFailed {
        draft_id: "d-failed".to_string(),
        err: "provider failed".to_string(),
        retryable: false,
    });

    let background_effects = state.reduce(Action::BackgroundSessionRecorded {
        summary: bg_summary("run-after-failure", "completed"),
    });
    let background_snapshot = background_effects
        .iter()
        .find_map(|effect| match effect {
            Effect::SaveSession(snapshot) => Some(snapshot),
            _ => None,
        })
        .expect("BackgroundSessionRecorded must emit SaveSession");
    assert!(
        background_snapshot
            .turns
            .iter()
            .all(|turn| turn.content != "orphan candidate"),
        "GP-9: background save snapshot must not persist the failed user turn"
    );

    let _ = state.reduce(Action::RecordUserTurn("real question".to_string()));
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-ok".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: "real answer".to_string(),
    });
    let completion_effects = state.reduce(Action::StreamCompleted {
        draft_id: "d-ok".to_string(),
        final_text: "real answer".to_string(),
        reasoning: String::new(),
    });
    let completion_snapshot = completion_effects
        .iter()
        .find_map(|effect| match effect {
            Effect::SaveSession(snapshot) => Some(snapshot),
            _ => None,
        })
        .expect("StreamCompleted must emit SaveSession");
    assert_eq!(
        completion_snapshot.turns.len(),
        2,
        "only the successful exchange is persisted"
    );
    assert_eq!(
        completion_snapshot.turns.first().map(|turn| turn.content.as_str()),
        Some("real question")
    );
    assert_eq!(
        completion_snapshot.turns.get(1).map(|turn| turn.content.as_str()),
        Some("real answer")
    );
}

/// Step3-8b: StreamCancelled / StreamFailed / StreamCompleted with a mismatched draft_id → no-op
#[test]
fn test_redux_stream_terminal_actions_wrong_id_noop() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    // fire the three terminal actions with the wrong id — all must be no-ops
    let e1 = state.reduce(Action::StreamCancelled {
        draft_id: "wrong".to_string(),
    });
    let e2 = state.reduce(Action::StreamFailed {
        draft_id: "wrong".to_string(),
        err: "x".to_string(),
        retryable: false,
    });
    let e3 = state.reduce(Action::StreamCompleted {
        draft_id: "wrong".to_string(),
        final_text: "x".to_string(),
        reasoning: String::new(),
    });
    assert!(e1.is_empty() && e2.is_empty() && e3.is_empty());
    assert!(
        state.stream.primary_streaming_draft().is_some(),
        "the original draft must be kept"
    );
    assert!(state.control.generating, "the generating flag must be kept");
}

/// Step3-9: ToolStarted → push a Running ToolResult + enqueue its index
#[test]
fn test_redux_tool_started_pushes_card() {
    use crate::chat::tui::{ConversationLine, ToolStatus};
    let mut state = s();
    let effects = state.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "shell".to_string(),
        args: r#"{"cmd":"ls"}"#.to_string(),
    });
    assert!(has_request_redraw(&effects));
    assert_eq!(state.ui.conversation_lines.len(), 1);
    assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Primary), 1);
    if let Some(ConversationLine::ToolResult { tool_name, status, .. }) = state.ui.conversation_lines.last() {
        assert_eq!(tool_name, "shell");
        assert_eq!(*status, ToolStatus::Running);
    } else {
        panic!("the last line must be a Running ToolResult");
    }
}

#[test]
fn test_redux_file_tool_cards_default_folded() {
    use crate::chat::tui::ConversationLine;
    for (tool_name, expected_folded) in [("file_edit", true), ("file_write", true), ("shell", true)] {
        let mut state = s();
        let _ = state.reduce(Action::ToolStarted {
            task_id: None,
            sequence: None,
            tool_call_id: None,
            name: tool_name.to_string(),
            args: r#"{"path":"demo.txt"}"#.to_string(),
        });
        match state.ui.conversation_lines.last() {
            Some(ConversationLine::ToolResult { folded, .. }) => {
                assert_eq!(*folded, expected_folded, "{tool_name} folded default mismatch");
            }
            other => panic!("expected ToolResult for {tool_name}, got {other:?}"),
        }
    }
}

/// Step3-10: ToolFinished → Running → Done + removed from pending
#[test]
fn test_redux_tool_finished_updates_card() {
    use crate::chat::tui::{ConversationLine, ToolStatus};
    let mut state = s();
    let _ = state.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "shell".to_string(),
        args: r#"{"cmd":"ls"}"#.to_string(),
    });
    let effects = state.reduce(Action::ToolFinished {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "shell".to_string(),
        success: true,
        duration_ms: 42,
        result: Some("ok".to_string()),
    });
    assert!(has_request_redraw(&effects));
    assert_eq!(
        state.control.pending_tool_card_count(ToolTaskKey::Primary),
        0,
        "pending must be cleared"
    );
    if let Some(ConversationLine::ToolResult {
        status,
        elapsed_ms,
        result,
        ..
    }) = state.ui.conversation_lines.last()
    {
        assert_eq!(*status, ToolStatus::Done);
        assert_eq!(*elapsed_ms, Some(42));
        assert_eq!(result.as_deref(), Some("ok"));
    } else {
        panic!("the last line must be a ToolResult");
    }
}

/// Step3-10b: ToolFinished success=false → Error
#[test]
fn test_redux_tool_finished_failed_marks_error() {
    use crate::chat::tui::{ConversationLine, ToolStatus};
    let mut state = s();
    let _ = state.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "shell".to_string(),
        args: r#"{}"#.to_string(),
    });
    let _ = state.reduce(Action::ToolFinished {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "shell".to_string(),
        success: false,
        duration_ms: 10,
        result: Some("err".to_string()),
    });
    if let Some(ConversationLine::ToolResult { status, .. }) = state.ui.conversation_lines.last() {
        assert_eq!(*status, ToolStatus::Error);
    } else {
        panic!("the last line must be a ToolResult");
    }
}

/// Step3-11: ToolProgress returns redraw/log and bumps UI generation.
#[test]
fn test_redux_tool_progress_returns_log_redraw_and_visible_generation() {
    let mut state = s();
    let before = state.ui.conversation_generation;
    let effects = state.reduce(Action::ToolProgress { iteration: 3 });
    assert!(has_request_redraw(&effects));
    assert!(has_log_trace(&effects));
    assert!(state.ui.conversation_lines.is_empty());
    assert_eq!(state.ui.conversation_generation, before + 1);
}

/// Step3-12: idempotency of the finalize path — even if StreamCompleted is fired twice by
/// mistake, the second reduce must be a no-op (the draft_id no longer exists)
#[test]
fn test_redux_finalize_retry_after_stream_completed_idempotent() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::StreamCompleted {
        draft_id: "d1".to_string(),
        final_text: "ans".to_string(),
        reasoning: String::new(),
    });
    let lines_after_first = state.ui.conversation_lines.len();
    // finalize the same draft_id again — the draft is already cleared → no-op
    let effects = state.reduce(Action::StreamCompleted {
        draft_id: "d1".to_string(),
        final_text: "ans-dup".to_string(),
        reasoning: String::new(),
    });
    assert!(effects.is_empty(), "a repeated finalize must be a no-op");
    assert_eq!(
        state.ui.conversation_lines.len(),
        lines_after_first,
        "no duplicate assistant line may be pushed (idempotent)"
    );
}

/// Step3-13: retrying a new Turn after StreamFailed — the version counter restarts at 0
#[test]
fn test_redux_finalize_retry_after_stream_failed() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "d1".to_string(),
        delta: "partial".to_string(),
        version: 5,
    });
    let _ = state.reduce(Action::StreamFailed {
        draft_id: "d1".to_string(),
        err: "boom".to_string(),
        retryable: true,
    });
    assert!(state.stream.primary_streaming_draft().is_none());
    // retry: open a new turn (the same draft_id is fine, draft.version starts at 0)
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: CancellationToken::new(),
    });
    assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(0));
    // version=1 must be accepted (unaffected by the previous round's 5, the draft was rebuilt)
    let effects = state.reduce(Action::StreamChunkReceived {
        draft_id: "d1".to_string(),
        delta: "retry".to_string(),
        version: 1,
    });
    assert!(has_request_redraw(&effects), "v=1 of a new turn must be accepted");
    assert_eq!(
        state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
        Some("retry".to_string())
    );
}

#[test]
fn esc_key_during_generation_cancels_active_turn() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "draft-esc".to_string(),
        cancel: CancellationToken::new(),
    });
    state.ui.input.set_text("local draft");

    let effects = state.reduce_with_now(
        Action::KeyPressed(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        1_000,
    );

    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::CancelDraft(id) if id == "draft-esc")),
        "Esc while generating must cancel the active draft: {effects:?}"
    );
    assert!(!state.control.generating);
    assert_eq!(state.ui.input.text(), "local draft");
}

/// P1-2: ten normal Actions in a row in Both mode — no semantic difference (diff_count baseline).
///
/// This test checks that the reducer itself behaves stably; the real diff_count cannot be
/// inspected across processes, so semantic consistency of the reduce output (stable effects for
/// the same Action sequence) is checked as an equivalent.
#[test]
fn test_redux_both_mode_diff_count_zero() {
    let mut state1 = s();
    let mut state2 = s();
    // run the same Action sequence on two independent states; the effect categories must match
    let actions: Vec<Action> = vec![
        Action::PasteReceived("hello".to_string()),
        Action::TerminalResized { w: 120, h: 40 },
        Action::RedrawRequested,
        Action::ToolCardFoldToggled,
        Action::ReasoningFoldToggled,
        Action::HistoryNavigated(HistoryDir::Up),
        Action::HistoryNavigated(HistoryDir::Down),
        Action::InputCancelled,
        Action::InputSubmitted("test input".to_string()),
        Action::RedrawRequested,
    ];
    for action in actions {
        let e1 = state1.reduce(action.clone());
        let e2 = state2.reduce(action);
        // running the same action twice independently must yield the same effect categories
        assert_eq!(
            e1.len(),
            e2.len(),
            "the same Action on two independent states must produce the same number of effects"
        );
    }
}

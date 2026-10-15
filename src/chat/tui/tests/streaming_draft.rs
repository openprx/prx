#![cfg(test)]

use super::*;

// ── P3-5: streaming-draft API tests ────────────────────────────────

#[test]
fn stream_start_creates_empty_draft_at_version_zero() {
    let mut state = TuiState::new("p", "m");
    assert!(state.streaming.is_none(), "fresh state has no draft");
    let v = state.start_stream("draft-1");
    assert_eq!(v, 0, "initial version is 0");
    let draft = state
        .streaming
        .as_ref()
        .expect("test: streaming slot populated after start");
    assert_eq!(draft.draft_id, "draft-1");
    assert_eq!(draft.accumulated, "");
    assert_eq!(draft.version, 0);
}

#[test]
fn stream_update_replaces_text_when_version_advances() {
    let mut state = TuiState::new("p", "m");
    state.start_stream("d1");
    assert!(state.update_stream("d1", "Hel", 1));
    assert_eq!(
        state.streaming.as_ref().expect("test: streaming present").accumulated,
        "Hel"
    );
    assert!(state.update_stream("d1", "Hello", 2));
    assert_eq!(
        state.streaming.as_ref().expect("test: streaming present").accumulated,
        "Hello"
    );
    assert_eq!(state.streaming.as_ref().expect("test: streaming present").version, 2);
}

#[test]
fn stream_update_rejects_stale_version() {
    let mut state = TuiState::new("p", "m");
    state.start_stream("d1");
    assert!(state.update_stream("d1", "Hel", 5));
    // Stale: seq 3 < 5 is dropped.
    assert!(!state.update_stream("d1", "old", 3));
    // And the buffer is unchanged.
    assert_eq!(
        state.streaming.as_ref().expect("test: streaming present").accumulated,
        "Hel"
    );
    // Same version also rejected (must strictly advance).
    assert!(!state.update_stream("d1", "still old", 5));
    assert_eq!(
        state.streaming.as_ref().expect("test: streaming present").accumulated,
        "Hel"
    );
}

#[test]
fn stream_update_rejects_mismatched_draft_id() {
    let mut state = TuiState::new("p", "m");
    state.start_stream("draft-A");
    assert!(state.update_stream("draft-A", "alpha", 1));
    // Wrong draft id — silent no-op, must NOT corrupt the active draft.
    assert!(!state.update_stream("draft-B", "beta", 99));
    assert_eq!(
        state.streaming.as_ref().expect("test: streaming present").accumulated,
        "alpha"
    );
    assert_eq!(
        state.streaming.as_ref().expect("test: streaming present").draft_id,
        "draft-A"
    );
}

#[test]
fn stream_update_without_active_draft_is_noop() {
    let mut state = TuiState::new("p", "m");
    assert!(!state.update_stream("nope", "lost", 1));
    assert!(state.streaming.is_none());
}

#[test]
fn stream_finalize_lifts_into_conversation_and_clears() {
    let mut state = TuiState::new("p", "m");
    state.start_stream("d1");
    let _ = state.update_stream("d1", "answer body", 1);
    let len_before = state.conversation_lines.len();
    state.finalize_stream("d1", "answer body");
    assert!(state.streaming.is_none(), "streaming slot cleared after finalize");
    assert_eq!(
        state.conversation_lines.len(),
        len_before + 1,
        "only assistant line pushed"
    );
    let rendered = transcript_plain_rows(&state, 80).join("\n");
    assert!(
        !rendered.contains("Worked for"),
        "completion timing must stay out of the transcript: {rendered}"
    );
    assert!(
        !rendered.to_ascii_lowercase().contains("turn completed"),
        "completion status must stay out of the transcript: {rendered}"
    );
    let last = state
        .conversation_lines
        .last()
        .expect("test: at least one conversation line");
    match last {
        ConversationLine::Assistant { content } => {
            assert_eq!(content, "answer body");
        }
        other => panic!("test: expected Assistant, got {other:?}"),
    }
}

#[test]
fn stream_finalize_empty_clears_slot_without_pushing() {
    let mut state = TuiState::new("p", "m");
    state.start_stream("d1");
    let _ = state.update_stream("d1", "partial", 1);
    let len_before = state.conversation_lines.len();
    state.finalize_stream("d1", "");
    assert!(state.streaming.is_none(), "slot cleared even on empty finalise");
    assert_eq!(state.conversation_lines.len(), len_before, "no line pushed");
}

#[test]
fn stream_finalize_mismatched_draft_is_noop() {
    let mut state = TuiState::new("p", "m");
    state.start_stream("d1");
    let _ = state.update_stream("d1", "buf", 1);
    let len_before = state.conversation_lines.len();
    state.finalize_stream("OTHER", "should-not-land");
    assert!(state.streaming.is_some(), "active draft preserved");
    assert_eq!(state.conversation_lines.len(), len_before, "no line pushed");
}

#[test]
fn stream_cancel_clears_slot_without_pushing() {
    let mut state = TuiState::new("p", "m");
    state.start_stream("d1");
    let _ = state.update_stream("d1", "buf", 1);
    let len_before = state.conversation_lines.len();
    state.cancel_stream("d1");
    assert!(state.streaming.is_none(), "slot cleared after cancel");
    assert_eq!(state.conversation_lines.len(), len_before, "no line pushed on cancel");
}

#[test]
fn stream_cancel_mismatched_draft_is_noop() {
    let mut state = TuiState::new("p", "m");
    state.start_stream("d1");
    state.cancel_stream("WRONG");
    assert!(state.streaming.is_some(), "active draft survives wrong-id cancel");
}

#[test]
fn stream_start_replaces_previous_inflight_draft() {
    // Defensive: if the channel layer fails to finalise the previous draft
    // before starting a new one, dropping the stale buffer is safer than
    // interleaving deltas across turns.
    let mut state = TuiState::new("p", "m");
    state.start_stream("d1");
    let _ = state.update_stream("d1", "stale buffer", 4);
    state.start_stream("d2");
    let draft = state.streaming.as_ref().expect("test: new streaming slot present");
    assert_eq!(draft.draft_id, "d2");
    assert_eq!(draft.accumulated, "");
    assert_eq!(draft.version, 0);
}

#[test]
fn streaming_transient_renders_after_history_in_fullscreen_lines() {
    // Drive `render_conversation_line` over both finalized history and the
    // staged transient line. The streaming block must contain the in-flight
    // tokens and end with the cursor glyph.
    let mut state = TuiState::new("p", "m");
    state.push_assistant_message("first turn done");
    state.start_stream("d-live");
    assert!(state.update_stream("d-live", "in-flight tokens", 1));

    // Simulate the render path's two-stage line build.
    let transient: Option<ConversationLine> = state.streaming.as_ref().map(|s| ConversationLine::StreamingAssistant {
        content: s.accumulated.clone(),
    });
    let mut lines: Vec<Line<'_>> = Vec::new();
    for conv_line in &state.conversation_lines {
        render_conversation_line(&mut lines, conv_line, false);
    }
    let history_end = lines.len();
    if let Some(t) = transient.as_ref() {
        render_conversation_line(&mut lines, t, false);
    }
    assert!(lines.len() > history_end, "streaming block contributed >=1 line");
    // The streaming body must contain the in-flight tokens AND end with
    // the streaming cursor glyph — confirms it routed through the
    // StreamingAssistant variant rather than the finalized Assistant one.
    // With the Claude-Code-style header removed, the body now lands at
    // `history_end` itself.
    let streaming_body: String = lines
        .get(history_end)
        .expect("test: body line present")
        .iter()
        .map(ratatui::text::Span::to_string)
        .collect();
    assert!(
        streaming_body.contains("in-flight tokens"),
        "expected streaming body, got {streaming_body:?}"
    );
    assert!(
        streaming_body.ends_with('\u{258C}'),
        "streaming body ends with ▌ cursor"
    );
}

#[test]
fn streaming_assistant_variant_renders_with_cursor() {
    let line = ConversationLine::StreamingAssistant {
        content: "partial response".to_string(),
    };
    let mut sink: Vec<Line<'_>> = Vec::new();
    render_conversation_line(&mut sink, &line, false);
    // Claude Code style: no `PRX:` header → body + trailing blank only.
    assert_eq!(sink.len(), 2, "streaming assistant renders body+blank");
    // Body must contain the content and end with the unicode cursor glyph.
    let body_text: String = sink
        .first()
        .expect("test: body line present")
        .iter()
        .map(ratatui::text::Span::to_string)
        .collect();
    assert!(body_text.contains("partial response"), "body has content");
    assert!(body_text.ends_with('\u{258C}'), "body ends with ▌ cursor");

    // ASCII fallback uses '_' instead of '▌'.
    let mut sink2: Vec<Line<'_>> = Vec::new();
    render_conversation_line(&mut sink2, &line, true);
    let body2: String = sink2
        .first()
        .expect("test: body line present")
        .iter()
        .map(ratatui::text::Span::to_string)
        .collect();
    assert!(body2.ends_with('_'), "ASCII fallback ends with '_'");
}

#[test]
fn assistant_marker_is_filled_neutral_and_separated_from_tool_output() {
    let mut sink: Vec<Line<'_>> = vec![Line::from("● Ran shell(command=\"pwd\")")];
    let line = ConversationLine::Assistant {
        content: "done".to_string(),
    };
    render_conversation_line(&mut sink, &line, false);

    assert_eq!(
        sink.get(1).map(line_to_plain).as_deref(),
        Some(""),
        "tool and assistant blocks need a blank row"
    );
    assert!(sink.get(2).is_some_and(|row| line_to_plain(row).starts_with("● done")));
    let marker_style = sink.get(2).and_then(|row| row.spans.first()).map(|span| span.style);
    assert_eq!(marker_style.and_then(|style| style.fg), Some(Color::DarkGray));
    assert!(marker_style.is_some_and(|style| !style.add_modifier.contains(Modifier::BOLD)));
}

#[test]
fn streaming_assistant_empty_content_still_renders() {
    let line = ConversationLine::StreamingAssistant { content: String::new() };
    let mut sink: Vec<Line<'_>> = Vec::new();
    render_conversation_line(&mut sink, &line, false);
    // Empty stream still produces 1 cursor row + 1 blank separator.
    assert_eq!(sink.len(), 2, "empty stream still produces body+blank");
}

#[test]
fn conversation_line_variants() {
    let user = ConversationLine::User {
        content: "test".to_string(),
    };
    assert!(matches!(user, ConversationLine::User { .. }));

    let tool = ConversationLine::Tool {
        name: "shell".to_string(),
        success: true,
    };
    assert!(matches!(tool, ConversationLine::Tool { .. }));
    assert!(!tool.is_tool_result());

    let tr = ConversationLine::ToolResult {
        tool_name: "shell".to_string(),
        args_preview: "ls".to_string(),
        args_full: "{\"command\":\"ls\"}".to_string(),
        result: None,
        status: ToolStatus::Running,
        elapsed_ms: None,
        folded: true,
    };
    assert!(tr.is_tool_result());
}

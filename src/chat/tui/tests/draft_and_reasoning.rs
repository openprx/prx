#![cfg(test)]

use super::*;

#[test]
fn draft_buffer_set_draft_seeds_lines() {
    let buf = DraftLineBuffer::new();
    buf.set_draft("d1", "a\nb\nc", 1).expect("test: seed v1");
    assert_eq!(buf.snapshot("d1"), Some(lines(&["a", "b", "c"])));
    assert_eq!(buf.tracked_count(), 1);
}

#[test]
fn draft_buffer_replace_middle_line() {
    let buf = DraftLineBuffer::new();
    buf.set_draft("d1", "a\nb\nc\nd", 1).expect("test: seed");
    // Replace just line 2 (index 2 = "c") with two new lines.
    buf.replace_lines_sync("d1", 2, 1, "X\nY", 2).expect("test: splice");
    assert_eq!(buf.snapshot("d1"), Some(lines(&["a", "b", "X", "Y", "d"])));
}

#[test]
fn draft_buffer_insert_with_zero_count() {
    let buf = DraftLineBuffer::new();
    buf.set_draft("d1", "a\nb\nc", 1).expect("test: seed");
    // count=0 → pure insertion before line at index 1.
    buf.replace_lines_sync("d1", 1, 0, "INS", 2).expect("test: insert");
    assert_eq!(buf.snapshot("d1"), Some(lines(&["a", "INS", "b", "c"])));
}

#[test]
fn draft_buffer_out_of_bounds_is_safe_error() {
    let buf = DraftLineBuffer::new();
    buf.set_draft("d1", "a\nb", 1).expect("test: seed");
    let err = buf
        .replace_lines_sync("d1", 5, 1, "X", 2)
        .expect_err("test: must reject");
    assert!(matches!(err, LineProtocolError::RangeOutOfBounds { .. }));
    // Buffer unchanged after the failed splice.
    assert_eq!(buf.snapshot("d1"), Some(lines(&["a", "b"])));
}

#[test]
fn draft_buffer_unknown_draft_rejected() {
    let buf = DraftLineBuffer::new();
    let err = buf
        .replace_lines_sync("ghost", 0, 1, "X", 1)
        .expect_err("test: unknown draft");
    assert!(matches!(err, LineProtocolError::UnknownDraft(ref id) if id == "ghost"));
}

#[test]
fn draft_buffer_stale_version_rejected() {
    let buf = DraftLineBuffer::new();
    buf.set_draft("d1", "a\nb\nc", 5).expect("test: seed v5");
    // Lower version must be dropped — buffer untouched.
    let err = buf.replace_lines_sync("d1", 1, 1, "X", 3).expect_err("test: stale");
    match err {
        LineProtocolError::StaleVersion { got, current } => {
            assert_eq!(got, 3);
            assert_eq!(current, 5);
        }
        other => panic!("test: expected StaleVersion, got {other:?}"),
    }
    assert_eq!(buf.snapshot("d1"), Some(lines(&["a", "b", "c"])));
    // Exact duplicate also rejected.
    assert!(buf.replace_lines_sync("d1", 1, 1, "X", 5).is_err());
    // Newer version accepted and applied.
    buf.replace_lines_sync("d1", 1, 1, "X", 6).expect("test: v6");
    assert_eq!(buf.snapshot("d1"), Some(lines(&["a", "X", "c"])));
}

#[test]
fn draft_buffer_set_then_replace_mixed_flow() {
    // Mirrors a real producer that alternates between full snapshot
    // (P1-6 `update_draft`) and fine-grained edits (P2-11).
    let buf = DraftLineBuffer::new();
    buf.set_draft("d1", "row1\nrow2\nrow3", 1).expect("test: v1");
    buf.replace_lines_sync("d1", 1, 1, "row2-edited", 2).expect("test: v2");
    assert_eq!(buf.snapshot("d1"), Some(lines(&["row1", "row2-edited", "row3"])));
    // A later full snapshot at higher version overwrites everything.
    buf.set_draft("d1", "a\nb", 3).expect("test: v3 full");
    assert_eq!(buf.snapshot("d1"), Some(lines(&["a", "b"])));
}

#[test]
fn draft_buffer_finalize_clears_state() {
    let buf = DraftLineBuffer::new();
    buf.set_draft("d1", "a\nb", 1).expect("test: seed");
    assert_eq!(buf.tracked_count(), 1);
    buf.finalize("d1");
    assert_eq!(buf.tracked_count(), 0);
    assert_eq!(buf.snapshot("d1"), None);
    // After finalize, low versions are fresh again.
    buf.set_draft("d1", "fresh", 1).expect("test: reseed");
    assert_eq!(buf.snapshot("d1"), Some(lines(&["fresh"])));
}

#[tokio::test]
async fn draft_buffer_implements_trait_for_async_callers() {
    let buf = DraftLineBuffer::new();
    buf.set_draft("d1", "a\nb\nc", 1).expect("test: seed");
    let ch: &dyn InlineDraftProtocol = &buf;
    ch.replace_lines("assistant", "d1", 1, 1, "MID", 2)
        .await
        .expect("test: trait call");
    assert_eq!(buf.snapshot("d1"), Some(lines(&["a", "MID", "c"])));
}

#[test]
fn reasoning_variant_construction_caches_char_count_and_defaults_folded() {
    let r = ConversationLine::Reasoning {
        content: "abc".to_string(),
        char_count: 3,
        folded: true,
    };
    assert!(r.is_reasoning());
    assert!(!r.is_tool_result());
    match r {
        ConversationLine::Reasoning {
            content,
            char_count,
            folded,
        } => {
            assert_eq!(content, "abc");
            assert_eq!(char_count, 3);
            assert!(folded);
        }
        other => panic!("test: expected Reasoning, got {other:?}"),
    }
}

#[test]
fn push_reasoning_drops_empty_and_whitespace_only_inputs() {
    let mut state = TuiState::new("p", "m");
    assert!(!state.push_reasoning(""));
    assert!(!state.push_reasoning("   \n\t"));
    assert!(state.last_reasoning_index().is_none());
    assert!(state.conversation_lines.is_empty());

    // Real reasoning is accepted.
    assert!(state.push_reasoning("  let me think...  "));
    let idx = state.last_reasoning_index().expect("test: reasoning exists");
    match state.conversation_lines.get(idx).expect("test: idx valid") {
        ConversationLine::Reasoning {
            content,
            char_count,
            folded,
        } => {
            // Trim is applied before storage.
            assert_eq!(content, "let me think...");
            assert_eq!(*char_count, "let me think...".chars().count());
            assert!(*folded, "default state is folded");
        }
        other => panic!("test: expected Reasoning, got {other:?}"),
    }
}

#[test]
fn toggle_last_reasoning_folded_flips_and_targets_last() {
    let mut state = TuiState::new("p", "m");
    // No card yet → None.
    assert_eq!(state.toggle_last_reasoning_folded(), None);

    state.push_reasoning("first thought");
    state.push_reasoning("second thought");
    // Default folded=true → toggling last (second) → false.
    assert_eq!(state.toggle_last_reasoning_folded(), Some(false));

    // First card untouched.
    let first_idx = state
        .conversation_lines
        .iter()
        .position(ConversationLine::is_reasoning)
        .expect("test: first idx");
    match state.conversation_lines.get(first_idx).expect("test: first idx valid") {
        ConversationLine::Reasoning { folded, .. } => assert!(*folded, "first card untouched"),
        other => panic!("test: expected Reasoning, got {other:?}"),
    }

    // Second toggle of last → back to folded.
    assert_eq!(state.toggle_last_reasoning_folded(), Some(true));
}

#[test]
fn render_folded_reasoning_card_renders_single_summary_line() {
    let mut lines: Vec<Line<'_>> = Vec::new();
    let card = ConversationLine::Reasoning {
        content: "Step 1: analyze the input\nStep 2: reason about it".to_string(),
        char_count: 45,
        folded: true,
    };
    render_conversation_line(&mut lines, &card, false);
    assert_eq!(lines.len(), 1, "folded card is exactly one line");
    let rendered = line_text(lines.first().expect("test: first line"));
    // Folded summary: `▸ Thinking (N tokens)` without noisy key-hint prose.
    assert!(rendered.starts_with("\u{25B8} "), "uses ▸ folded icon: {rendered}");
    assert!(rendered.contains("Thinking"), "shows Thinking label: {rendered}");
    assert!(rendered.contains("tokens"), "shows token count: {rendered}");
    assert!(
        !rendered.contains("press Tab"),
        "folded header trims noisy Tab prose: {rendered}"
    );
    // Folded summary must NOT leak the body text.
    assert!(!rendered.contains("Step 1"), "body hidden when folded: {rendered}");
}

#[test]
fn render_expanded_reasoning_card_indents_body_two_spaces() {
    let mut lines: Vec<Line<'_>> = Vec::new();
    let body = "Let me reason step by step.\nFirst, I need to check the input.\nThen, decide the next action.";
    let card = ConversationLine::Reasoning {
        content: body.to_string(),
        char_count: body.chars().count(),
        folded: false,
    };
    render_conversation_line(&mut lines, &card, false);
    // 1 header + 3 body rows = 4 lines.
    assert_eq!(lines.len(), 4, "header + 3 body rows: {}", lines.len());
    let header = line_text(lines.first().expect("test: header"));
    assert!(
        header.starts_with("\u{25BE} "),
        "expanded header shows ▾ icon: {header}"
    );
    assert!(
        header.contains("Thinking"),
        "expanded header still says Thinking: {header}"
    );
    assert!(
        header.contains("tokens"),
        "expanded header carries token count: {header}"
    );
    assert!(
        !header.contains("press Tab to expand"),
        "expanded header drops the expand hint: {header}"
    );
    assert!(
        header.contains("Tab to collapse"),
        "expanded header shows Tab collapse hint: {header}"
    );
    assert!(
        !header.contains("Ctrl+R"),
        "expanded header must not advertise Ctrl+R as fold after P6b2: {header}"
    );
    // Each body row begins with "  " indent.
    for (idx, original) in body.lines().enumerate() {
        let rendered = line_text(lines.get(idx + 1).expect("test: body line"));
        assert_eq!(rendered, format!("  {original}"), "body row {idx} indent");
    }
}

#[test]
fn render_reasoning_card_ascii_fallback_uses_plain_glyphs() {
    let mut lines: Vec<Line<'_>> = Vec::new();
    let card = ConversationLine::Reasoning {
        content: "thinking...".to_string(),
        char_count: 11,
        folded: true,
    };
    render_conversation_line(&mut lines, &card, true);
    let rendered = line_text(lines.first().expect("test: first line"));
    // ASCII folded glyph is `>`.
    assert!(rendered.starts_with("> "), "ASCII folded icon >: {rendered}");
    assert!(
        rendered.contains("Thinking ("),
        "ASCII keeps Thinking label: {rendered}"
    );
    assert!(
        !rendered.contains("press Tab"),
        "ASCII folded header trims noisy Tab prose: {rendered}"
    );
    assert!(!rendered.contains("\u{25B8}"), "no ▸ in ASCII mode: {rendered}");

    // Expanded ASCII uses `v` chevron.
    let mut lines2: Vec<Line<'_>> = Vec::new();
    let expanded = ConversationLine::Reasoning {
        content: "x".to_string(),
        char_count: 1,
        folded: false,
    };
    render_conversation_line(&mut lines2, &expanded, true);
    let header = line_text(lines2.first().expect("test: expanded header"));
    assert!(header.starts_with("v Thinking ("), "ASCII expanded header: {header}");
    assert!(!header.contains("\u{25BE}"), "no ▾ in ASCII mode: {header}");
}

#[test]
fn primary_transcript_hides_reasoning_and_completion_duration_and_click_cannot_toggle() {
    let mut state = TuiState::new("p", "m");
    state.conversation_lines.push(ConversationLine::Assistant {
        content: "final answer".to_string(),
    });
    assert!(state.push_reasoning("first step\nsecond step"));
    let scroll = FullscreenTranscriptScroll::default();

    let primary = transcript_plain_rows(&state, 80).join("\n");
    assert!(
        !primary.contains("Worked for"),
        "primary omits completion timing: {primary}"
    );
    assert!(
        !primary.contains("turn completed"),
        "primary omits completion status: {primary}"
    );
    assert!(primary.contains("final answer"), "primary keeps the answer: {primary}");
    assert!(
        !primary.contains("Thinking"),
        "primary transcript hides reasoning: {primary}"
    );
    assert!(
        !primary.contains("first step"),
        "primary transcript hides reasoning body: {primary}"
    );
    assert!(
        !toggle_reasoning_at_fullscreen_point(&mut state, &scroll, 80, 24, 0, 0),
        "hidden reasoning has no clickable row"
    );
    match state.conversation_lines.last() {
        Some(ConversationLine::Reasoning { folded, .. }) => assert!(*folded, "hidden record remains unchanged"),
        other => panic!("test: expected Reasoning, got {other:?}"),
    }

    let verbose = build_transcript_view("demo", &state.conversation_lines, 0);
    assert!(
        verbose.lines.iter().any(|line| line.contains("first step")),
        "verbose transcript retains reasoning: {:?}",
        verbose.lines
    );
}

#[test]
fn mouse_click_outside_transcript_does_not_toggle_reasoning() {
    let mut state = TuiState::new("p", "m");
    assert!(state.push_reasoning("first step"));
    let scroll = FullscreenTranscriptScroll::default();

    assert!(
        !toggle_reasoning_at_fullscreen_point(&mut state, &scroll, 80, 6, 0, 5),
        "bottom chrome row is outside the transcript"
    );
    match state.conversation_lines.last() {
        Some(ConversationLine::Reasoning { folded, .. }) => assert!(*folded, "outside click leaves default fold"),
        other => panic!("test: expected Reasoning, got {other:?}"),
    }
}

#[test]
fn reasoning_card_glyphs_table() {
    // S1-A: triangle glyphs — `▸` folded / `▾` expanded — with `>` / `v`
    // as ASCII fallbacks. The fold state DOES change the leading icon so
    // the user can see at a glance whether a card is currently expanded.
    assert_eq!(reasoning_card_glyphs(false), ("\u{25B8}", "\u{25BE}"));
    assert_eq!(reasoning_card_glyphs(true), (">", "v"));
}

#[test]
fn estimate_reasoning_tokens_table() {
    // Empty → 0 (folded card never reaches this because push_reasoning
    // drops empty bodies, but the function should still be sane).
    assert_eq!(estimate_reasoning_tokens(0), 0);
    // Anything non-empty rounds up to at least 1 token.
    assert_eq!(estimate_reasoning_tokens(1), 1);
    assert_eq!(estimate_reasoning_tokens(3), 1);
    assert_eq!(estimate_reasoning_tokens(4), 1);
    assert_eq!(estimate_reasoning_tokens(5), 2);
    assert_eq!(estimate_reasoning_tokens(400), 100);
}

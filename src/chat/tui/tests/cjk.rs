#![cfg(test)]

use super::*;

// ── CJK / wide-char rendering regression tests ───────────────────────
//
// These tests guard the shared conversation-line renderer against phantom
// spaces in wide CJK text. Each CJK character occupies 2 columns; ratatui
// stores the glyph in cell[x] and resets cell[x+1] to `Cell::EMPTY`
// (whose `symbol()` returns `" "`). Buffer diffing must reconstruct the
// original text without inter-character spaces.

#[test]
#[cfg(feature = "terminal-tui")]
fn cjk_buffer_diff_omits_continuation_cells() {
    // Build a buffer containing a wide-char assistant message, then take
    // its diff against an empty buffer. The diff updates must not contain
    // continuation-cell spaces between consecutive CJK characters.
    let line = ConversationLine::Assistant {
        content: "\u{ac00}\u{ac01}\u{ac02}\u{ac03}\u{ac04}\u{ac05}\u{ac06}\u{ac07}PRX".to_string(),
    };
    let area = Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 2,
    };
    let empty = Buffer::empty(area);
    let mut filled = Buffer::empty(area);
    render_conversation_line_to_buffer(&mut filled, &line, false);

    // Collect symbols from the diff.
    let diff = empty.diff(&filled);
    let mut row0_symbols = String::new();
    for (_x, y, cell) in &diff {
        if *y == 0 {
            // Only gather non-space content on row 0 for readability.
            // Trailing spaces that come from the padding at the end of
            // the line are acceptable and are trimmed below.
            row0_symbols.push_str(cell.symbol());
        }
    }
    let trimmed = row0_symbols.trim_end();
    // The diff must faithfully reconstruct the text (no phantom spaces).
    assert!(
        trimmed.contains("\u{ac00}\u{ac01}\u{ac02}\u{ac03}\u{ac04}\u{ac05}\u{ac06}\u{ac07}PRX"),
        "diff path should emit CJK chars without inter-character spaces, got {trimmed:?}"
    );
    assert!(
        !trimmed.contains("\u{ac00} \u{ac01}") && !trimmed.contains("\u{ac01} \u{ac02}"),
        "phantom spaces detected in diff output: {trimmed:?}"
    );
}

#[test]
#[cfg(feature = "terminal-tui")]
fn cjk_streaming_buffer_diff_omits_continuation_cells() {
    // StreamingAssistant appends a block-cursor glyph (▌). Verify that
    // CJK content before the cursor is also contiguous in the diff path.
    let line = ConversationLine::StreamingAssistant {
        content: "\u{ac00}\u{ac01}\u{ac02}\u{ac03}".to_string(),
    };
    let area = Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 2,
    };
    let empty = Buffer::empty(area);
    let mut filled = Buffer::empty(area);
    render_conversation_line_to_buffer(&mut filled, &line, false);

    let diff = empty.diff(&filled);
    let mut row0 = String::new();
    for (_x, y, cell) in &diff {
        if *y == 0 {
            row0.push_str(cell.symbol());
        }
    }
    let trimmed = row0.trim_end();
    // Assistant text now has an actor marker prefix; the CJK payload before
    // the cursor must still be contiguous in the diff output.
    assert!(
        trimmed.contains("\u{ac00}\u{ac01}\u{ac02}\u{ac03}"),
        "streaming CJK diff should be contiguous, got {trimmed:?}"
    );
    assert!(
        !trimmed.contains("\u{ac00} \u{ac01}") && !trimmed.contains("\u{ac01} \u{ac02}"),
        "phantom spaces detected in streaming diff output: {trimmed:?}"
    );
}

#[test]
#[cfg(feature = "terminal-tui")]
fn cjk_user_message_diff_omits_continuation_cells() {
    // User message is prefixed with `> ` in a styled Span. Verify the
    // full row (including prefix) is contiguous in the diff path.
    let line = ConversationLine::User {
        content: "\u{ac00}\u{ac01}\u{ac02}\u{ac03}".to_string(),
    };
    let area = Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 2,
    };
    let empty = Buffer::empty(area);
    let mut filled = Buffer::empty(area);
    render_conversation_line_to_buffer(&mut filled, &line, false);

    let diff = empty.diff(&filled);
    let mut row0 = String::new();
    for (_x, y, cell) in &diff {
        if *y == 0 {
            row0.push_str(cell.symbol());
        }
    }
    let trimmed = row0.trim_end();
    // Row 0: "> " prefix followed by the four wide chars.
    assert!(
        trimmed.starts_with("> \u{ac00}\u{ac01}\u{ac02}\u{ac03}"),
        "user CJK diff should be '> ' followed by the four wide chars, got {trimmed:?}"
    );
    assert!(
        !trimmed.contains("\u{ac00} \u{ac01}"),
        "phantom spaces detected in user CJK diff: {trimmed:?}"
    );
}

/// USERINFO-2: the status bar and the session list show the provider on
/// every frame; a `custom:` URL may carry `user:password@`.
#[test]
fn the_status_bar_names_the_provider_without_url_userinfo() {
    let state = TuiState::new("custom:http://u:SECRET-fixture-pw@127.0.0.1:1/v1", "fixture-model");
    for width in [72, 120, 220] {
        let line = render_status_bar_text(&state, width);
        assert!(!line.contains("SECRET-fixture-pw"), "{line}");
    }
    let line = render_status_bar_text(&state, 220);
    assert!(line.contains("custom:http://[redacted]@127.0.0.1:1/v1"), "{line}");
}

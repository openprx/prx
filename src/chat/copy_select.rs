//! Copy-to-clipboard selection and transcript mouse selection helpers.

use super::file_mention::truncate_utf8_to_byte_cap;
use super::session_glue::surface_session_message;
#[cfg(feature = "terminal-tui")]
use super::tui;
use super::tui_loop::RenderSource;
use super::{dispatcher, session, terminal_proto};
use anyhow::Result;
use tokio::sync::mpsc;

pub(super) const COPY_OSC52_MAX_BYTES: usize = 74 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CopySelection {
    pub(super) content: String,
    pub(super) ordinal: usize,
    pub(super) truncated: bool,
}

pub(super) fn is_copy_command(input: &str) -> bool {
    input == "/copy" || input.starts_with("/copy ")
}

pub(super) fn select_copy_content(session: &session::ChatSession, input: &str) -> Result<CopySelection, String> {
    let raw = input.strip_prefix("/copy").unwrap_or_default().trim();
    let ordinal = if raw.is_empty() || raw.eq_ignore_ascii_case("latest") {
        1
    } else {
        raw.parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| "Usage: /copy [latest|N]".to_string())?
    };
    let Some(turn) = session
        .turns
        .iter()
        .rev()
        .filter(|turn| turn.role == "assistant")
        .nth(ordinal - 1)
    else {
        return Err("No assistant response to copy.".to_string());
    };
    let (content, truncated) = truncate_utf8_to_byte_cap(&turn.content, COPY_OSC52_MAX_BYTES);
    Ok(CopySelection {
        content,
        ordinal,
        truncated,
    })
}

pub(super) fn copy_success_message(selection: &CopySelection) -> String {
    let suffix = if selection.truncated {
        " (truncated to 74 KiB for OSC 52)"
    } else {
        ""
    };
    if selection.ordinal == 1 {
        format!("Copied latest assistant response to clipboard{suffix}.")
    } else {
        format!(
            "Copied assistant response #{} from the end to clipboard{suffix}.",
            selection.ordinal
        )
    }
}

#[cfg(feature = "terminal-tui")]
pub(super) const MOUSE_WHEEL_TRANSCRIPT_ROWS: usize = 3;

#[cfg(feature = "terminal-tui")]
pub(super) fn apply_fullscreen_mouse_scroll(
    kind: crossterm::event::MouseEventKind,
    fullscreen_scroll: &mut tui::FullscreenTranscriptScroll,
) -> bool {
    match kind {
        crossterm::event::MouseEventKind::ScrollUp => {
            fullscreen_scroll.page_up(MOUSE_WHEEL_TRANSCRIPT_ROWS);
            true
        }
        crossterm::event::MouseEventKind::ScrollDown => {
            fullscreen_scroll.page_down(MOUSE_WHEEL_TRANSCRIPT_ROWS);
            true
        }
        _ => false,
    }
}

#[cfg(feature = "terminal-tui")]
pub(super) fn copy_transcript_selection(
    render_source: &RenderSource,
    selection: tui::TranscriptSelection,
    width: u16,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    redraw_tx: &mpsc::Sender<()>,
) {
    if !selection.moved() {
        return;
    }
    let ((start_row, start_column), (end_row, end_column)) = selection.ordered_points();
    let content = render_source.with_view(|view| {
        let rows = tui::transcript_plain_rows(view, width);
        if rows.is_empty() || start_row >= rows.len() {
            return String::new();
        }
        let end_row = end_row.min(rows.len().saturating_sub(1));
        (start_row..=end_row)
            .map(|row| {
                let text = rows.get(row).map(String::as_str).unwrap_or("");
                let width = unicode_width::UnicodeWidthStr::width(text);
                let (start, end) = if start_row == end_row {
                    (start_column, end_column.saturating_add(1).min(width))
                } else if row == start_row {
                    (start_column, width)
                } else if row == end_row {
                    (0, end_column.saturating_add(1).min(width))
                } else {
                    (0, width)
                };
                slice_display_columns(text, start, end)
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    if content.is_empty() {
        return;
    }
    let (content, truncated) = truncate_utf8_to_byte_cap(&content, COPY_OSC52_MAX_BYTES);
    if content.trim().is_empty() {
        return;
    }
    match terminal_proto::copy_to_clipboard(&content) {
        Ok(()) => {
            let suffix = if truncated { " (truncated to 74 KiB)" } else { "" };
            surface_session_message(
                chat_dispatcher,
                Some(redraw_tx),
                &format!("Copied selected transcript rows to clipboard{suffix}."),
            );
        }
        Err(error) => surface_session_message(chat_dispatcher, Some(redraw_tx), &format!("Copy failed: {error}")),
    }
}

#[cfg(feature = "terminal-tui")]
pub(super) fn slice_display_columns(input: &str, start: usize, end: usize) -> String {
    if start >= end {
        return String::new();
    }
    let mut output = String::new();
    let mut column = 0usize;
    for ch in input.chars() {
        let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        let next_column = column.saturating_add(char_width);
        if next_column > start && column < end {
            output.push(ch);
        }
        column = next_column;
        if column >= end {
            break;
        }
    }
    output
}

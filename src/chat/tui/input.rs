use super::keys::InputOutcome;
use super::slash_menu::SlashCursorContext;
use super::{
    INPUT_HISTORY_CAPACITY, INPUT_MAX_BYTES, PASTE_CHIP_SENTINEL_END, PASTE_CHIP_SENTINEL_START,
    PASTE_FOLD_BYTE_THRESHOLD, PASTE_FOLD_LINE_THRESHOLD,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Multi-line text input with history navigation.
///
/// Storage is a `Vec<String>` where each element is one logical line **without**
/// the trailing newline. The cursor is a `(line_index, byte_offset)` pair;
/// `byte_offset` always lies on a UTF-8 char boundary because all mutations go
/// through the dedicated helpers below.
///
/// History is a FIFO ring capped at [`INPUT_HISTORY_CAPACITY`]. `history_pos`
/// is `None` while the user is editing a fresh buffer; once they navigate up
/// it becomes `Some(index)` pointing into `history`.
#[derive(Debug, Clone)]
pub struct TuiInput {
    /// Each element is a logical line (no trailing '\n').
    pub lines: Vec<String>,
    /// Cursor: (line_index, byte_offset_into_line).
    pub cursor: (usize, usize),
    /// Submitted history, oldest at index 0.
    pub history: Vec<String>,
    /// Position when navigating history; `None` = editing fresh input.
    pub history_pos: Option<usize>,
    /// Snapshot of the in-flight buffer saved when entering history nav, so we
    /// can restore it when the user scrolls past the end of history.
    pub(super) pending_draft: Option<InputDraftSnapshot>,
    /// Original payloads hidden behind folded paste chips in `lines`.
    pub(super) paste_chips: Vec<PasteChip>,
    /// Monotonic display id for folded paste chips in this draft.
    pub(super) next_paste_chip_id: usize,
    /// True when text was ignored because the input reached INPUT_MAX_BYTES.
    pub truncated: bool,
    /// Active reverse history search state (`Ctrl+R`), if any.
    pub(super) reverse_search: Option<ReverseSearchState>,
}

/// Ephemeral reverse-search state for the input history ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReverseSearchState {
    /// Draft buffer before the search started; restored on Esc.
    pub(super) saved_draft: InputDraftSnapshot,
    /// User-entered incremental search query.
    pub query: String,
    /// Currently selected history entry.
    pub match_pos: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PasteChip {
    pub(super) token: String,
    pub(super) placeholder: String,
    pub(super) content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InputDraftSnapshot {
    pub(super) lines: Vec<String>,
    pub(super) paste_chips: Vec<PasteChip>,
    pub(super) next_paste_chip_id: usize,
    pub(super) truncated: bool,
}

impl Default for TuiInput {
    fn default() -> Self {
        Self::new()
    }
}

impl TuiInput {
    /// Create a fresh, empty input buffer.
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: (0, 0),
            history: Vec::new(),
            history_pos: None,
            pending_draft: None,
            paste_chips: Vec::new(),
            next_paste_chip_id: 1,
            truncated: false,
            reverse_search: None,
        }
    }

    /// Joined buffer contents (lines separated by '\n').
    pub fn text(&self) -> String {
        self.expand_paste_chips(&self.lines.join("\n"))
    }

    pub(super) fn expand_paste_chips(&self, stored: &str) -> String {
        if self.paste_chips.is_empty() {
            return stored.to_string();
        }
        let mut expanded = stored.to_string();
        for chip in &self.paste_chips {
            expanded = expanded.replace(&chip.token, &chip.content);
        }
        expanded
    }

    pub(super) fn paste_chip_token(id: usize) -> String {
        format!("{PASTE_CHIP_SENTINEL_START}paste:{id}{PASTE_CHIP_SENTINEL_END}")
    }

    pub(super) fn chip_at_offset<'a>(&'a self, line: &str, offset: usize) -> Option<&'a PasteChip> {
        let tail = line.get(offset..)?;
        self.paste_chips.iter().find(|chip| tail.starts_with(&chip.token))
    }

    pub(super) fn display_line(&self, line: &str) -> String {
        if self.paste_chips.is_empty() {
            return line.to_string();
        }
        let mut out = String::new();
        let mut offset = 0usize;
        while offset < line.len() {
            if let Some(chip) = self.chip_at_offset(line, offset) {
                out.push_str(&chip.placeholder);
                offset = offset.saturating_add(chip.token.len());
                continue;
            }
            let Some(ch) = line.get(offset..).and_then(|tail| tail.chars().next()) else {
                break;
            };
            out.push(ch);
            offset = offset.saturating_add(ch.len_utf8());
        }
        out
    }

    pub(super) fn display_lines(&self) -> Vec<String> {
        self.lines.iter().map(|line| self.display_line(line)).collect()
    }

    pub(super) fn display_cursor_offset(&self, line_idx: usize, storage_cursor: usize) -> usize {
        let Some(line) = self.lines.get(line_idx) else {
            return 0;
        };
        let cursor = storage_cursor.min(line.len());
        let mut storage_offset = 0usize;
        let mut display_offset = 0usize;
        while storage_offset < line.len() {
            if cursor <= storage_offset {
                return display_offset;
            }
            if let Some(chip) = self.chip_at_offset(line, storage_offset) {
                let chip_end = storage_offset.saturating_add(chip.token.len());
                if cursor < chip_end {
                    return display_offset;
                }
                storage_offset = chip_end;
                display_offset = display_offset.saturating_add(chip.placeholder.len());
                continue;
            }
            let Some(ch) = line.get(storage_offset..).and_then(|tail| tail.chars().next()) else {
                break;
            };
            storage_offset = storage_offset.saturating_add(ch.len_utf8());
            display_offset = display_offset.saturating_add(ch.len_utf8());
        }
        display_offset
    }

    pub(super) fn chip_range_matching<F>(&self, line: &str, mut matches: F) -> Option<(usize, usize)>
    where
        F: FnMut(usize, usize) -> bool,
    {
        for chip in &self.paste_chips {
            for (start, _) in line.match_indices(&chip.token) {
                let end = start.saturating_add(chip.token.len());
                if matches(start, end) {
                    return Some((start, end));
                }
            }
        }
        None
    }

    pub(super) fn chip_range_containing_cursor(&self, line: &str, offset: usize) -> Option<(usize, usize)> {
        self.chip_range_matching(line, |start, end| start < offset && offset < end)
    }

    pub(super) fn chip_range_before_or_containing_cursor(&self, line: &str, offset: usize) -> Option<(usize, usize)> {
        self.chip_range_matching(line, |start, end| start < offset && offset <= end)
    }

    pub(super) fn chip_range_at_or_containing_cursor(&self, line: &str, offset: usize) -> Option<(usize, usize)> {
        self.chip_range_matching(line, |start, end| start <= offset && offset < end)
    }

    pub(super) fn remove_chip_range(&mut self, line_idx: usize, start: usize, end: usize) {
        let removed = self
            .lines
            .get(line_idx)
            .and_then(|line| line.get(start..end))
            .map(str::to_string);
        if let Some(line) = self.lines.get_mut(line_idx) {
            line.replace_range(start..end, "");
            self.cursor = (line_idx, start.min(line.len()));
        }
        if let Some(token) = removed {
            self.paste_chips.retain(|chip| chip.token != token);
        }
    }

    pub(super) fn snap_cursor_after_chip(&mut self) {
        let (line_idx, offset) = self.cursor;
        let Some(line) = self.lines.get(line_idx) else {
            return;
        };
        if let Some((_start, end)) = self.chip_range_containing_cursor(line, offset) {
            self.cursor = (line_idx, end);
        }
    }

    pub(super) fn draft_snapshot(&self) -> InputDraftSnapshot {
        InputDraftSnapshot {
            lines: self.lines.clone(),
            paste_chips: self.paste_chips.clone(),
            next_paste_chip_id: self.next_paste_chip_id,
            truncated: self.truncated,
        }
    }

    pub(super) fn restore_draft_snapshot(&mut self, snapshot: InputDraftSnapshot) {
        self.lines = if snapshot.lines.is_empty() {
            vec![String::new()]
        } else {
            snapshot.lines
        };
        self.paste_chips = snapshot.paste_chips;
        self.next_paste_chip_id = snapshot.next_paste_chip_id.max(1);
        self.truncated = snapshot.truncated;
        let last_line_idx = self.lines.len().saturating_sub(1);
        let last_len = self.lines.get(last_line_idx).map_or(0, String::len);
        self.cursor = (last_line_idx, last_len);
    }

    /// True when the buffer is logically empty (single empty line).
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines.first().is_none_or(String::is_empty)
    }

    /// Filter text for a leading slash command when the cursor is inside the
    /// command token. Returns `None` once the cursor moves into arguments.
    pub fn slash_command_filter_at_cursor(&self) -> Option<String> {
        match self.slash_cursor_context() {
            Some(SlashCursorContext::Command { filter }) => Some(filter),
            _ => None,
        }
    }

    /// Filter text after a word-start `@` token at the cursor, used by the
    /// TUI loop to source file candidates through the file-read security gate.
    pub(crate) fn at_path_filter_at_cursor(&self) -> Option<String> {
        match self.at_path_cursor_context() {
            Some(SlashCursorContext::AtPath { filter }) => Some(filter),
            _ => None,
        }
    }

    pub(super) fn completion_cursor_context(&self) -> Option<SlashCursorContext> {
        self.slash_cursor_context().or_else(|| self.at_path_cursor_context())
    }

    pub(super) fn slash_cursor_context(&self) -> Option<SlashCursorContext> {
        let (line_idx, cursor_offset) = self.cursor;
        if line_idx != 0 {
            return None;
        }
        let line = self.lines.get(line_idx)?;
        if !line.starts_with('/') {
            return None;
        }
        let cursor = cursor_offset.min(line.len());
        let command_end = line.find(char::is_whitespace).unwrap_or(line.len());
        if cursor <= command_end {
            return line.get(1..cursor).map(|filter| SlashCursorContext::Command {
                filter: filter.to_string(),
            });
        }
        let command = line.get(..command_end)?.to_string();
        let Some(args_start) = line
            .get(command_end..)?
            .find(|ch: char| !ch.is_whitespace())
            .map(|offset| command_end.saturating_add(offset))
        else {
            return Some(SlashCursorContext::Argument {
                command,
                arg_index: 0,
                filter: String::new(),
                previous_args: Vec::new(),
            });
        };
        if cursor < args_start {
            return None;
        }
        let before_cursor = line.get(args_start..cursor)?;
        let mut parts = before_cursor.split_whitespace().map(str::to_string).collect::<Vec<_>>();
        let cursor_after_whitespace = before_cursor.chars().last().is_some_and(char::is_whitespace);
        let (arg_index, filter, previous_args) = if cursor_after_whitespace {
            (parts.len(), String::new(), parts)
        } else if let Some(filter) = parts.pop() {
            (parts.len(), filter, parts)
        } else {
            (0, String::new(), Vec::new())
        };
        Some(SlashCursorContext::Argument {
            command,
            arg_index,
            filter,
            previous_args,
        })
    }

    pub(super) fn at_path_cursor_context(&self) -> Option<SlashCursorContext> {
        let (line_idx, cursor_offset) = self.cursor;
        let line = self.lines.get(line_idx)?;
        let cursor = cursor_offset.min(line.len());
        let mut token_start = 0;
        for (offset, ch) in line.get(..cursor)?.char_indices() {
            if ch.is_whitespace() {
                token_start = offset.saturating_add(ch.len_utf8());
            }
        }
        let token = line.get(token_start..cursor)?;
        if !token.starts_with('@') {
            return None;
        }
        let before = line.get(..token_start).unwrap_or_default();
        if before.chars().last().is_some_and(|ch| !ch.is_whitespace()) {
            return None;
        }
        Some(SlashCursorContext::AtPath {
            filter: token.get(1..).unwrap_or_default().to_string(),
        })
    }

    /// Replace the current leading slash-command token with `command`, leaving a
    /// trailing space so the operator can immediately type arguments.
    pub(super) fn slash_command_suffix_is_empty(&self) -> bool {
        let (line_idx, _cursor_offset) = self.cursor;
        let Some(line) = self.lines.get(line_idx) else {
            return false;
        };
        if !line.starts_with('/') {
            return false;
        }
        let token_end = line.find(char::is_whitespace).unwrap_or(line.len());
        line.get(token_end..).unwrap_or_default().trim().is_empty()
    }

    pub(super) fn replace_slash_command_token(&mut self, command: &str, append_space: bool) {
        let (line_idx, _cursor_offset) = self.cursor;
        let Some(line) = self.lines.get_mut(line_idx) else {
            return;
        };
        if !line.starts_with('/') {
            return;
        }
        let token_end = line.find(char::is_whitespace).unwrap_or(line.len());
        let suffix = line.get(token_end..).unwrap_or_default().trim_start();
        let replacement = if suffix.is_empty() {
            if append_space {
                format!("{command} ")
            } else {
                command.to_string()
            }
        } else {
            format!("{command} {suffix}")
        };
        *line = replacement;
        let cursor = if append_space {
            command.len().saturating_add(1)
        } else {
            command.len()
        };
        self.cursor = (line_idx, cursor.min(line.len()));
        self.history_pos = None;
        self.pending_draft = None;
        self.reverse_search = None;
    }

    pub(super) fn replace_slash_argument_token(&mut self, value: &str, append_space: bool) {
        let (line_idx, cursor_offset) = self.cursor;
        let Some(line) = self.lines.get_mut(line_idx) else {
            return;
        };
        if !line.starts_with('/') {
            return;
        }
        let cursor = cursor_offset.min(line.len());
        let command_end = line.find(char::is_whitespace).unwrap_or(line.len());
        if cursor <= command_end {
            return;
        }
        let Some(args_offset) = line
            .get(command_end..)
            .and_then(|tail| tail.find(|ch: char| !ch.is_whitespace()))
        else {
            let insertion = if append_space {
                format!("{value} ")
            } else {
                value.to_string()
            };
            line.insert_str(cursor, &insertion);
            self.cursor = (line_idx, cursor.saturating_add(insertion.len()).min(line.len()));
            self.history_pos = None;
            self.pending_draft = None;
            self.reverse_search = None;
            return;
        };
        let args_start = command_end.saturating_add(args_offset);
        if cursor < args_start {
            return;
        }

        let mut token_start = args_start;
        for (offset, ch) in line.get(args_start..cursor).unwrap_or_default().char_indices() {
            if ch.is_whitespace() {
                token_start = args_start.saturating_add(offset).saturating_add(ch.len_utf8());
            }
        }
        let token_end = line
            .get(cursor..)
            .and_then(|tail| tail.find(char::is_whitespace))
            .map_or(line.len(), |offset| cursor.saturating_add(offset));
        let suffix = line.get(token_end..).unwrap_or_default().to_string();
        let insertion = if append_space {
            format!("{value} ")
        } else {
            value.to_string()
        };
        line.replace_range(token_start..token_end, &insertion);
        self.cursor = (line_idx, token_start.saturating_add(insertion.len()).min(line.len()));
        if !suffix.is_empty() && !line.ends_with(&suffix) {
            line.push_str(&suffix);
        }
        self.history_pos = None;
        self.pending_draft = None;
        self.reverse_search = None;
    }

    pub(super) fn replace_at_path_token(&mut self, value: &str, append_space: bool) {
        let (line_idx, cursor_offset) = self.cursor;
        let Some(line) = self.lines.get_mut(line_idx) else {
            return;
        };
        let cursor = cursor_offset.min(line.len());
        let mut token_start = 0;
        for (offset, ch) in line.get(..cursor).unwrap_or_default().char_indices() {
            if ch.is_whitespace() {
                token_start = offset.saturating_add(ch.len_utf8());
            }
        }
        let Some(token) = line.get(token_start..cursor) else {
            return;
        };
        if !token.starts_with('@') {
            return;
        }
        let token_end = line
            .get(cursor..)
            .and_then(|tail| tail.find(char::is_whitespace))
            .map_or(line.len(), |offset| cursor.saturating_add(offset));
        let insertion = if append_space {
            format!("{value} ")
        } else {
            value.to_string()
        };
        line.replace_range(token_start..token_end, &insertion);
        self.cursor = (line_idx, token_start.saturating_add(insertion.len()).min(line.len()));
        self.history_pos = None;
        self.pending_draft = None;
        self.reverse_search = None;
    }

    /// Current draft size in bytes, counting newline separators between rows.
    pub fn byte_len(&self) -> usize {
        self.text().len()
    }

    /// True if the user is currently editing a single logical line — used to
    /// decide whether `↑/↓` should navigate history or move the cursor.
    pub const fn is_single_line(&self) -> bool {
        self.lines.len() <= 1
    }

    /// Replace the entire buffer (used by history navigation and paste).
    pub fn set_text(&mut self, text: &str) {
        // Strip a trailing '\n' so single-line history doesn't grow a blank
        // second row when restored.
        let trimmed = text.strip_suffix('\n').unwrap_or(text);
        self.lines = if trimmed.is_empty() {
            vec![String::new()]
        } else {
            trimmed.split('\n').map(str::to_owned).collect()
        };
        let last_line_idx = self.lines.len().saturating_sub(1);
        let last_len = self.lines.get(last_line_idx).map_or(0, String::len);
        self.cursor = (last_line_idx, last_len);
        self.paste_chips.clear();
        self.next_paste_chip_id = 1;
        self.truncated = false;
    }

    /// Clear navigation/search state after an external text replacement.
    pub fn clear_navigation_state(&mut self) {
        self.reverse_search = None;
        self.history_pos = None;
        self.pending_draft = None;
    }

    /// Clear the buffer back to a single empty line.
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor = (0, 0);
        self.history_pos = None;
        self.pending_draft = None;
        self.paste_chips.clear();
        self.next_paste_chip_id = 1;
        self.truncated = false;
        self.reverse_search = None;
    }

    /// Insert a single grapheme (`ch`) at the cursor.
    pub(super) fn insert_char(&mut self, ch: char) -> bool {
        self.snap_cursor_after_chip();
        if self.byte_len().saturating_add(ch.len_utf8()) > INPUT_MAX_BYTES {
            self.truncated = true;
            return false;
        }
        let (li, off) = self.cursor;
        if let Some(line) = self.lines.get_mut(li) {
            self.history_pos = None;
            self.pending_draft = None;
            // `off` is always at a char boundary because we only ever advance
            // by `ch.len_utf8()` from prior inserts and via `floor_char_boundary`.
            let clamped = off.min(line.len());
            line.insert(clamped, ch);
            self.cursor = (li, clamped + ch.len_utf8());
            if self.byte_len() >= INPUT_MAX_BYTES {
                self.truncated = true;
            }
            return true;
        }
        false
    }

    /// Insert a literal string at the cursor. Newlines split into rows.
    pub(super) fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.snap_cursor_after_chip();
        self.history_pos = None;
        self.pending_draft = None;
        let remaining = INPUT_MAX_BYTES.saturating_sub(self.byte_len());
        let text = if text.len() > remaining {
            self.truncated = true;
            clamp_str_to_byte_len(text, remaining)
        } else {
            text
        };
        if text.is_empty() {
            return;
        }
        // Split by '\n' explicitly so a trailing newline produces an empty row.
        let mut parts = text.split('\n');
        if let Some(first) = parts.next() {
            // Insert `first` at the cursor on the current line.
            let (li, off) = self.cursor;
            if let Some(line) = self.lines.get_mut(li) {
                let clamped = off.min(line.len());
                let suffix: String = line[clamped..].to_string();
                line.truncate(clamped);
                line.push_str(first);
                let mut new_cursor = (li, line.len());

                // Any remaining parts become new lines below the current one.
                for (insert_at, part) in (li + 1..).zip(parts) {
                    self.lines.insert(insert_at, part.to_string());
                    new_cursor = (insert_at, self.lines.get(insert_at).map_or(0, String::len));
                }

                // Append the original suffix to whatever ended up as the
                // cursor's line.
                if let Some(last_line) = self.lines.get_mut(new_cursor.0) {
                    last_line.push_str(&suffix);
                }
                self.cursor = new_cursor;
            }
        }
    }

    pub(super) fn insert_chip_token(&mut self, token: &str) {
        let (li, off) = self.cursor;
        if let Some(line) = self.lines.get_mut(li) {
            self.history_pos = None;
            self.pending_draft = None;
            let clamped = off.min(line.len());
            line.insert_str(clamped, token);
            self.cursor = (li, clamped.saturating_add(token.len()));
        }
    }

    pub(super) fn insert_folded_paste(&mut self, text: &str) {
        let remaining = INPUT_MAX_BYTES.saturating_sub(self.byte_len());
        if remaining == 0 {
            self.truncated = true;
            return;
        }
        let content = if text.len() > remaining {
            self.truncated = true;
            clamp_str_to_byte_len(text, remaining).to_string()
        } else {
            text.to_string()
        };
        if content.is_empty() {
            return;
        }
        let id = self.next_paste_chip_id;
        self.next_paste_chip_id = self.next_paste_chip_id.saturating_add(1);
        let line_count = pasted_line_count(&content);
        let token = Self::paste_chip_token(id);
        let placeholder = format!("[Pasted text #{id}: {line_count} lines]");
        self.paste_chips.push(PasteChip {
            token: token.clone(),
            placeholder,
            content,
        });
        self.insert_chip_token(&token);
    }

    /// Split the current line at the cursor (`Shift+Enter`).
    pub(super) fn insert_newline(&mut self) {
        self.snap_cursor_after_chip();
        if self.byte_len().saturating_add(1) > INPUT_MAX_BYTES {
            self.truncated = true;
            return;
        }
        self.history_pos = None;
        self.pending_draft = None;
        let (li, off) = self.cursor;
        if let Some(line) = self.lines.get_mut(li) {
            let clamped = off.min(line.len());
            let tail: String = line.split_off(clamped);
            self.lines.insert(li + 1, tail);
            self.cursor = (li + 1, 0);
        }
    }

    pub(super) fn consume_backslash_line_continuation(&mut self) -> bool {
        let (li, off) = self.cursor;
        let Some(line) = self.lines.get_mut(li) else {
            return false;
        };
        if off != line.len() || !line.ends_with('\\') {
            return false;
        }
        line.pop();
        self.cursor = (li, line.len());
        self.insert_newline();
        true
    }

    /// Delete the character before the cursor; join with previous line if at
    /// column 0.
    pub(super) fn backspace(&mut self) {
        self.history_pos = None;
        self.pending_draft = None;
        let (li, off) = self.cursor;
        if off > 0 {
            if let Some(line) = self.lines.get(li)
                && let Some((start, end)) = self.chip_range_before_or_containing_cursor(line, off)
            {
                self.remove_chip_range(li, start, end);
                if self.byte_len() < INPUT_MAX_BYTES {
                    self.truncated = false;
                }
                return;
            }
            if let Some(line) = self.lines.get_mut(li) {
                let new_off = floor_char_boundary(line, off.saturating_sub(1));
                line.replace_range(new_off..off, "");
                self.cursor = (li, new_off);
            }
        } else if li > 0 {
            // Merge current line into previous.
            let current = self.lines.remove(li);
            let prev_idx = li - 1;
            if let Some(prev) = self.lines.get_mut(prev_idx) {
                let new_off = prev.len();
                prev.push_str(&current);
                self.cursor = (prev_idx, new_off);
            }
        }
        if self.byte_len() < INPUT_MAX_BYTES {
            self.truncated = false;
        }
    }

    /// Delete the character at the cursor; join next line if at end of line.
    pub(super) fn delete_forward(&mut self) {
        self.history_pos = None;
        self.pending_draft = None;
        let (li, off) = self.cursor;
        let line_len = self.lines.get(li).map_or(0, String::len);
        if off < line_len {
            if let Some(line) = self.lines.get(li)
                && let Some((start, end)) = self.chip_range_at_or_containing_cursor(line, off)
            {
                self.remove_chip_range(li, start, end);
                if self.byte_len() < INPUT_MAX_BYTES {
                    self.truncated = false;
                }
                return;
            }
            if let Some(line) = self.lines.get_mut(li) {
                // Find end of the char starting at `off`.
                let mut end = off + 1;
                while end < line.len() && !line.is_char_boundary(end) {
                    end += 1;
                }
                line.replace_range(off..end, "");
            }
        } else if li + 1 < self.lines.len() {
            // Join next line into current.
            let next = self.lines.remove(li + 1);
            if let Some(line) = self.lines.get_mut(li) {
                line.push_str(&next);
            }
        }
        if self.byte_len() < INPUT_MAX_BYTES {
            self.truncated = false;
        }
    }

    /// Move cursor one char left, possibly to previous line's end.
    pub(super) fn move_left(&mut self) {
        let (li, off) = self.cursor;
        if off > 0 {
            if let Some(line) = self.lines.get(li) {
                if let Some((start, _end)) = self.chip_range_before_or_containing_cursor(line, off) {
                    self.cursor = (li, start);
                    return;
                }
                let new_off = floor_char_boundary(line, off.saturating_sub(1));
                self.cursor = (li, new_off);
            }
        } else if li > 0 {
            let prev_len = self.lines.get(li - 1).map_or(0, String::len);
            self.cursor = (li - 1, prev_len);
        }
    }

    /// Move cursor one char right, possibly to next line's start.
    pub(super) fn move_right(&mut self) {
        let (li, off) = self.cursor;
        let line_len = self.lines.get(li).map_or(0, String::len);
        if off < line_len {
            if let Some(line) = self.lines.get(li) {
                if let Some((_start, end)) = self.chip_range_at_or_containing_cursor(line, off) {
                    self.cursor = (li, end);
                    return;
                }
                let mut new_off = off + 1;
                while new_off < line.len() && !line.is_char_boundary(new_off) {
                    new_off += 1;
                }
                self.cursor = (li, new_off);
            }
        } else if li + 1 < self.lines.len() {
            self.cursor = (li + 1, 0);
        }
    }

    /// Move cursor to start of current line (Home / Ctrl+A).
    pub(super) const fn move_line_start(&mut self) {
        self.cursor.1 = 0;
    }

    /// Move cursor to end of current line (End / Ctrl+E).
    pub(super) fn move_line_end(&mut self) {
        let line_len = self.lines.get(self.cursor.0).map_or(0, String::len);
        self.cursor.1 = line_len;
    }

    /// Move cursor up one row when multi-line; keep byte offset clamped.
    pub(super) fn move_cursor_up(&mut self) -> bool {
        let (li, off) = self.cursor;
        if li == 0 {
            return false;
        }
        let new_li = li - 1;
        let new_line_len = self.lines.get(new_li).map_or(0, String::len);
        let target_off = off.min(new_line_len);
        let safe_off = self
            .lines
            .get(new_li)
            .map_or(target_off, |line| floor_char_boundary(line, target_off));
        self.cursor = (new_li, safe_off);
        self.snap_cursor_after_chip();
        true
    }

    /// Move cursor down one row when multi-line.
    pub(super) fn move_cursor_down(&mut self) -> bool {
        let (li, off) = self.cursor;
        if li + 1 >= self.lines.len() {
            return false;
        }
        let new_li = li + 1;
        let new_line_len = self.lines.get(new_li).map_or(0, String::len);
        let target_off = off.min(new_line_len);
        let safe_off = self
            .lines
            .get(new_li)
            .map_or(target_off, |line| floor_char_boundary(line, target_off));
        self.cursor = (new_li, safe_off);
        self.snap_cursor_after_chip();
        true
    }

    /// Delete from start of current line up to cursor (`Ctrl+U`).
    pub(super) fn delete_to_line_start(&mut self) {
        self.snap_cursor_after_chip();
        self.history_pos = None;
        self.pending_draft = None;
        let (li, off) = self.cursor;
        if let Some(line) = self.lines.get_mut(li) {
            line.replace_range(0..off.min(line.len()), "");
            self.cursor = (li, 0);
            if self.byte_len() < INPUT_MAX_BYTES {
                self.truncated = false;
            }
        }
    }

    /// Push a finalized entry onto the history ring (dedups consecutive dupes).
    pub(super) fn record_history(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if self.history.last() == Some(&text) {
            return;
        }
        if self.history.len() >= INPUT_HISTORY_CAPACITY {
            self.history.remove(0);
        }
        self.history.push(text);
    }

    /// True while `Ctrl+R` reverse history search is active.
    pub const fn is_reverse_search_active(&self) -> bool {
        self.reverse_search.is_some()
    }

    /// Human-readable reverse-search status for the input box title.
    pub fn reverse_search_title(&self) -> Option<String> {
        let search = self.reverse_search.as_ref()?;
        let query = truncate_input_title(&search.query, 36);
        let status = if search.match_pos.is_some() {
            "match"
        } else {
            "no match"
        };
        Some(format!(" reverse-search: {query} ({status}) "))
    }

    /// Start reverse-search, or cycle to the next older match when already active.
    pub fn begin_or_cycle_reverse_search(&mut self) -> bool {
        if self.reverse_search.is_none() {
            self.reverse_search = Some(ReverseSearchState {
                saved_draft: self.draft_snapshot(),
                query: String::new(),
                match_pos: None,
            });
        }
        self.reverse_search_cycle_older();
        true
    }

    pub(super) fn reverse_search_cycle_older(&mut self) {
        let Some(search) = self.reverse_search.as_ref() else {
            return;
        };
        let before = search.match_pos.unwrap_or(self.history.len());
        let query = search.query.clone();
        let matched = self.find_reverse_history_match(&query, before);
        if let Some(search) = self.reverse_search.as_mut() {
            search.match_pos = matched;
        }
        self.apply_reverse_search_match();
    }

    pub(super) fn reverse_search_query_changed(&mut self) {
        let Some(search) = self.reverse_search.as_ref() else {
            return;
        };
        let query = search.query.clone();
        let matched = self.find_reverse_history_match(&query, self.history.len());
        if let Some(search) = self.reverse_search.as_mut() {
            search.match_pos = matched;
        }
        self.apply_reverse_search_match();
    }

    pub(super) fn find_reverse_history_match(&self, query: &str, before: usize) -> Option<usize> {
        if self.history.is_empty() {
            return None;
        }
        let mut idx = before.min(self.history.len());
        while idx > 0 {
            idx -= 1;
            let Some(entry) = self.history.get(idx) else {
                continue;
            };
            if query.is_empty() || entry.contains(query) {
                return Some(idx);
            }
        }
        None
    }

    pub(super) fn apply_reverse_search_match(&mut self) {
        let Some(search) = self.reverse_search.as_ref() else {
            return;
        };
        if let Some(pos) = search.match_pos
            && let Some(entry) = self.history.get(pos)
        {
            let entry = entry.clone();
            self.set_text(&entry);
        } else {
            self.restore_draft_snapshot(search.saved_draft.clone());
        }
    }

    pub(super) fn cancel_reverse_search(&mut self) {
        if let Some(search) = self.reverse_search.take() {
            self.restore_draft_snapshot(search.saved_draft);
        }
    }

    pub(super) fn accept_reverse_search(&mut self) {
        self.reverse_search = None;
        self.history_pos = None;
        self.pending_draft = None;
    }

    pub(super) fn handle_reverse_search_key(&mut self, key: KeyEvent) -> InputOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('r') if ctrl => {
                self.reverse_search_cycle_older();
                InputOutcome::Consumed
            }
            KeyCode::Char(ch) if !ctrl => {
                if let Some(search) = self.reverse_search.as_mut() {
                    search.query.push(ch);
                }
                self.reverse_search_query_changed();
                InputOutcome::Consumed
            }
            KeyCode::Backspace => {
                if let Some(search) = self.reverse_search.as_mut() {
                    search.query.pop();
                }
                self.reverse_search_query_changed();
                InputOutcome::Consumed
            }
            KeyCode::Enter => {
                self.accept_reverse_search();
                InputOutcome::Consumed
            }
            KeyCode::Esc => {
                self.cancel_reverse_search();
                InputOutcome::Cancelled
            }
            _ => InputOutcome::Consumed,
        }
    }

    /// Navigate to the previous (older) entry. Saves the in-flight draft on
    /// first call so it can be restored later.
    pub(super) fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let next_pos = match self.history_pos {
            None => {
                self.pending_draft = Some(self.draft_snapshot());
                self.history.len().saturating_sub(1)
            }
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.history_pos = Some(next_pos);
        if let Some(entry) = self.history.get(next_pos) {
            let entry_owned = entry.clone();
            self.set_text(&entry_owned);
        }
        true
    }

    /// Navigate to the next (newer) entry, or back to the pending draft.
    pub(super) fn history_next(&mut self) -> bool {
        let Some(pos) = self.history_pos else {
            return false;
        };
        let next_pos = pos + 1;
        if next_pos >= self.history.len() {
            // Past the most recent entry → restore in-flight draft (if any).
            self.history_pos = None;
            if let Some(draft) = self.pending_draft.take() {
                self.restore_draft_snapshot(draft);
            } else {
                self.lines = vec![String::new()];
                self.paste_chips.clear();
                self.next_paste_chip_id = 1;
                self.truncated = false;
                self.cursor = (0, 0);
            }
        } else {
            self.history_pos = Some(next_pos);
            if let Some(entry) = self.history.get(next_pos) {
                let entry_owned = entry.clone();
                self.set_text(&entry_owned);
            }
        }
        true
    }

    /// Process a single key event. See [`InputOutcome`] for return semantics.
    pub fn handle_key(&mut self, key: KeyEvent) -> InputOutcome {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if self.reverse_search.is_some() {
            return self.handle_reverse_search_key(key);
        }

        if ctrl && !alt {
            match key.code {
                KeyCode::Char('p' | 'P') => {
                    return if self.history_prev() {
                        InputOutcome::Consumed
                    } else {
                        InputOutcome::Unhandled
                    };
                }
                KeyCode::Char('n' | 'N') => {
                    return if self.history_next() {
                        InputOutcome::Consumed
                    } else {
                        InputOutcome::Unhandled
                    };
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Enter => {
                if shift || alt {
                    self.insert_newline();
                    return InputOutcome::Consumed;
                }
                if self.consume_backslash_line_continuation() {
                    return InputOutcome::Consumed;
                }
                if self.is_empty() {
                    return InputOutcome::Consumed;
                }
                let text = self.text();
                self.record_history(text.clone());
                self.clear();
                InputOutcome::Submitted(text)
            }
            KeyCode::Char('u') if ctrl => {
                self.delete_to_line_start();
                InputOutcome::Consumed
            }
            KeyCode::Char('a') if ctrl => {
                self.move_line_start();
                InputOutcome::Consumed
            }
            KeyCode::Char('e') if ctrl => {
                self.move_line_end();
                InputOutcome::Consumed
            }
            KeyCode::Char('j') if ctrl => {
                // Common terminal alternative for "newline without submit".
                self.insert_newline();
                InputOutcome::Consumed
            }
            KeyCode::Char(ch) if !ctrl => {
                if self.insert_char(ch) {
                    InputOutcome::Consumed
                } else {
                    InputOutcome::Ignored
                }
            }
            KeyCode::Tab => {
                if self.insert_char('\t') {
                    InputOutcome::Consumed
                } else {
                    InputOutcome::Ignored
                }
            }
            KeyCode::Backspace => {
                self.backspace();
                InputOutcome::Consumed
            }
            KeyCode::Delete => {
                self.delete_forward();
                InputOutcome::Consumed
            }
            KeyCode::Left => {
                self.move_left();
                InputOutcome::Consumed
            }
            KeyCode::Right => {
                self.move_right();
                InputOutcome::Consumed
            }
            KeyCode::Home => {
                self.move_line_start();
                InputOutcome::Consumed
            }
            KeyCode::End => {
                self.move_line_end();
                InputOutcome::Consumed
            }
            KeyCode::Up => {
                if !self.is_single_line() && self.move_cursor_up() {
                    InputOutcome::Consumed
                } else {
                    InputOutcome::Unhandled
                }
            }
            KeyCode::Down => {
                if !self.is_single_line() && self.move_cursor_down() {
                    InputOutcome::Consumed
                } else {
                    InputOutcome::Unhandled
                }
            }
            // Fullscreen transcript scrolling is owned by the outer event loop,
            // not the input widget.
            KeyCode::PageUp | KeyCode::PageDown => InputOutcome::Unhandled,
            KeyCode::Esc => {
                if !self.is_empty() {
                    self.clear();
                }
                InputOutcome::Cancelled
            }
            _ => InputOutcome::Unhandled,
        }
    }

    /// Append pasted text verbatim. Newlines split into rows.
    pub fn paste(&mut self, text: &str) {
        if should_fold_paste(text) {
            self.insert_folded_paste(text);
        } else {
            self.insert_str(text);
        }
    }
}

pub(super) fn pasted_line_count(text: &str) -> usize {
    if text.is_empty() { 0 } else { text.split('\n').count() }
}

pub(super) fn should_fold_paste(text: &str) -> bool {
    pasted_line_count(text) > PASTE_FOLD_LINE_THRESHOLD || text.len() > PASTE_FOLD_BYTE_THRESHOLD
}

pub(super) fn clamp_str_to_byte_len(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub(super) fn truncate_input_title(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in input.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

/// Round `idx` down to the nearest UTF-8 char boundary in `s`. Saturates at 0.
pub(super) const fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    let max = s.len();
    if idx > max {
        idx = max;
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

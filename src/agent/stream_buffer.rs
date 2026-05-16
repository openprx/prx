//! Semantic-boundary stream buffer for relaying LLM streaming text to channels.
//!
//! The agent loop streams LLM output to a `mpsc::Sender<String>` so that the
//! channel layer can progressively update a draft message. Previously the loop
//! split deltas on whitespace into ~80-character chunks, which routinely cut
//! `<tool_call>...</tool_call>` XML blocks or JSON objects in half. The first
//! half rendered to the user, the second half arrived later and produced
//! garbled output.
//!
//! `StreamBoundaryBuffer` solves this by accumulating raw deltas and only
//! flushing prefix slices that end at a *safe* semantic boundary:
//!
//!   - we are NOT inside an unclosed `<tag>` (XML/HTML angle bracket pair)
//!   - we are NOT inside an unclosed JSON `{...}` object
//!   - we are NOT inside a JSON string literal (`"..."`, with `\\` / `\"` escapes)
//!   - we are NOT inside an unclosed JSON `[...]` array
//!
//! Within a "safe" zone we still want to stream progressively, so we flush on:
//!   - newline characters (`\n`), or
//!   - when the buffered safe prefix grows past `MIN_FLUSH_CHARS`.
//!
//! On stream end, `flush_all()` returns any remaining buffered content
//! unconditionally — even if it is syntactically unclosed (the upstream LLM
//! emitted malformed output; passing it through is better than silently
//! dropping it).
//!
//! The state machine is intentionally minimal: it does not parse XML attributes,
//! CDATA, comments, or full JSON. It tracks just enough to avoid splitting
//! obvious atomic blocks.

use std::mem;

/// Soft target for flushing safe-zone prefixes. We do not split atomic blocks,
/// but inside plain text we still want to stream chunks of roughly this size
/// so the UI sees progressive updates rather than one giant final blob.
const MIN_FLUSH_CHARS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    /// Plain text, outside any structured block.
    Text,
    /// Inside `<...>` (XML tag, until matching `>`).
    XmlTag,
    /// Inside JSON structure (object or array). `depth` tracks nesting.
    JsonStruct { depth: u32 },
    /// Inside JSON string literal `"..."`. `prev_backslash` handles `\"`.
    JsonString { depth: u32, prev_backslash: bool },
}

/// Accumulates streaming text and emits chunks that end at semantic boundaries.
#[derive(Debug)]
pub(crate) struct StreamBoundaryBuffer {
    /// All accumulated unflushed bytes.
    buf: String,
    /// Byte index in `buf` up to which we have scanned the state machine.
    scanned: usize,
    /// Byte index of the last position that is a confirmed "safe flush" point
    /// (end of a top-level boundary inside `buf`).
    safe_end: usize,
    /// Current scan state at position `scanned`.
    state: ScanState,
}

impl StreamBoundaryBuffer {
    pub(crate) const fn new() -> Self {
        Self {
            buf: String::new(),
            scanned: 0,
            safe_end: 0,
            state: ScanState::Text,
        }
    }

    /// Append a raw delta from the LLM and return any chunk(s) that are now
    /// safe to forward downstream. Returns `None` when nothing is safe yet.
    #[tracing::instrument(level = "trace", skip_all, fields(delta_len = delta.len()))]
    pub(crate) fn push(&mut self, delta: &str) -> Option<String> {
        if delta.is_empty() {
            return None;
        }
        // S2.5 T2.5-2: chunk 计数指标（每个 non-empty delta 计 1）.
        crate::observability::chat_metrics::inc_stream_chunk();
        self.buf.push_str(delta);
        self.advance_scan();
        self.take_flushable()
    }

    /// Drain everything that remains, regardless of state. Used at stream end.
    pub(crate) fn flush_all(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let out = mem::take(&mut self.buf);
        self.scanned = 0;
        self.safe_end = 0;
        self.state = ScanState::Text;
        Some(out)
    }

    /// Advance the state machine over any unscanned bytes, updating
    /// `safe_end` whenever we return to plain `Text` state.
    fn advance_scan(&mut self) {
        let bytes = self.buf.as_bytes();
        while let Some(&b) = bytes.get(self.scanned) {
            match self.state {
                ScanState::Text => match b {
                    b'<' => {
                        self.state = ScanState::XmlTag;
                    }
                    b'{' => {
                        self.state = ScanState::JsonStruct { depth: 1 };
                    }
                    b'[' => {
                        self.state = ScanState::JsonStruct { depth: 1 };
                    }
                    _ => {
                        // Stay in Text — boundary candidate at next-byte position.
                        // We mark safe_end *after* this byte (inclusive of newline etc.).
                        self.safe_end = self.scanned + 1;
                    }
                },
                ScanState::XmlTag => {
                    if b == b'>' {
                        self.state = ScanState::Text;
                        self.safe_end = self.scanned + 1;
                    }
                }
                ScanState::JsonStruct { depth } => match b {
                    b'"' => {
                        self.state = ScanState::JsonString {
                            depth,
                            prev_backslash: false,
                        };
                    }
                    b'{' | b'[' => {
                        self.state = ScanState::JsonStruct { depth: depth + 1 };
                    }
                    b'}' | b']' => {
                        let new_depth = depth.saturating_sub(1);
                        if new_depth == 0 {
                            self.state = ScanState::Text;
                            self.safe_end = self.scanned + 1;
                        } else {
                            self.state = ScanState::JsonStruct { depth: new_depth };
                        }
                    }
                    _ => {}
                },
                ScanState::JsonString { depth, prev_backslash } => {
                    if prev_backslash {
                        // Current byte is escaped (e.g., `\"`, `\\`, `\n`); consume.
                        self.state = ScanState::JsonString {
                            depth,
                            prev_backslash: false,
                        };
                    } else if b == b'\\' {
                        self.state = ScanState::JsonString {
                            depth,
                            prev_backslash: true,
                        };
                    } else if b == b'"' {
                        self.state = ScanState::JsonStruct { depth };
                    }
                }
            }
            self.scanned += 1;
        }
    }

    /// Return the prefix that can be safely flushed, if any. The prefix always
    /// ends on a UTF-8 boundary because `safe_end` only advances past whole
    /// scanned bytes that were ASCII boundary markers (`>`, `}`, `]`, or any
    /// non-special byte in `Text` state — non-ASCII continuation bytes in
    /// `Text` state are also valid flush positions since multi-byte UTF-8
    /// scalars are never split mid-sequence by this byte-level scanner).
    ///
    /// Flushes when either:
    ///   - `safe_end` already contains a newline (interactive streaming), or
    ///   - `safe_end` >= `MIN_FLUSH_CHARS` (progressive chunks for plain text).
    fn take_flushable(&mut self) -> Option<String> {
        if self.safe_end == 0 {
            return None;
        }
        let prefix = self.buf.as_bytes().get(..self.safe_end)?;
        let has_newline = prefix.contains(&b'\n');
        if !has_newline && self.safe_end < MIN_FLUSH_CHARS {
            return None;
        }
        // Verify UTF-8 boundary; if not aligned, retreat to the last char boundary.
        let cut = utf8_char_boundary(&self.buf, self.safe_end);
        if cut == 0 {
            return None;
        }
        let flushed = self.buf.get(..cut)?.to_string();
        // Drain flushed bytes from the buffer and reset indices.
        self.buf.drain(..cut);
        self.scanned = self.scanned.saturating_sub(cut);
        self.safe_end = self.safe_end.saturating_sub(cut);
        Some(flushed)
    }
}

/// Walk backwards from `pos` to the nearest valid UTF-8 char boundary in `s`.
/// `pos` is a byte index, `0 <= pos <= s.len()`. Returns the largest `cut`
/// with `cut <= pos` such that `s.is_char_boundary(cut)`.
fn utf8_char_boundary(s: &str, pos: usize) -> usize {
    let mut cut = pos.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::iter_with_drain
)]
mod tests {
    use super::*;

    fn collect_all(deltas: Vec<&str>) -> Vec<String> {
        let mut buf = StreamBoundaryBuffer::new();
        let mut out = Vec::new();
        for d in deltas {
            if let Some(s) = buf.push(d) {
                out.push(s);
            }
        }
        if let Some(s) = buf.flush_all() {
            out.push(s);
        }
        out
    }

    #[test]
    fn plain_text_flushes_on_newline() {
        // Newline within first ~64 chars should trigger immediate flush.
        let chunks = collect_all(vec!["hello world\n", "second line\n"]);
        let joined: String = chunks.iter().flat_map(|s| s.chars()).collect();
        assert_eq!(joined, "hello world\nsecond line\n");
        // First flush happens at newline of first delta.
        assert!(chunks[0].ends_with('\n'));
    }

    #[test]
    fn short_plain_text_buffers_until_min_or_end() {
        // Short text without newline: nothing flushed until flush_all.
        let mut buf = StreamBoundaryBuffer::new();
        let mid = buf.push("hi");
        assert!(mid.is_none(), "short prefix should not flush early");
        let end = buf.flush_all().unwrap();
        assert_eq!(end, "hi");
    }

    #[test]
    fn complete_tool_call_emits_as_one_block() {
        let chunks = collect_all(vec!["<tool_call>shell ls</tool_call>\n"]);
        let joined: String = chunks.concat();
        assert_eq!(joined, "<tool_call>shell ls</tool_call>\n");
        // Whatever the chunk boundary, no chunk should split inside `<...>`.
        for c in &chunks {
            let opens = c.matches('<').count();
            let closes = c.matches('>').count();
            assert_eq!(opens, closes, "chunk has unbalanced XML brackets: {c:?}");
        }
    }

    #[test]
    fn fragmented_tool_call_buffers_until_close() {
        // Split `<tool_call>do stuff</tool_call>` into many tiny deltas
        // that never let the scanner exit XmlTag state mid-flush.
        let deltas = vec!["<to", "ol_", "call", ">do ", "stuff</", "tool_call>"];
        let chunks = collect_all(deltas);
        let joined: String = chunks.concat();
        assert_eq!(joined, "<tool_call>do stuff</tool_call>");
        // Each chunk must have balanced angle brackets.
        for c in &chunks {
            let opens = c.matches('<').count();
            let closes = c.matches('>').count();
            assert_eq!(opens, closes, "chunk split inside <...>: {c:?}");
        }
    }

    #[test]
    fn fragmented_json_buffers_until_brace_closes() {
        let deltas = vec![r#"{"name": "#, r#""ls", "#, r#""args": [1, 2]"#, "}"];
        let chunks = collect_all(deltas);
        let joined: String = chunks.concat();
        assert_eq!(joined, r#"{"name": "ls", "args": [1, 2]}"#);
        // No chunk should leave unbalanced braces.
        for c in &chunks {
            let lo = c.matches('{').count();
            let lc = c.matches('}').count();
            assert_eq!(lo, lc, "chunk split inside {{...}}: {c:?}");
        }
    }

    #[test]
    fn string_literal_braces_do_not_affect_depth() {
        // The `{` and `<` inside the JSON string must NOT increment depth/start an XML tag.
        let deltas = vec![r#"{"text": "a { b < c"}"#];
        let chunks = collect_all(deltas);
        let joined: String = chunks.concat();
        assert_eq!(joined, r#"{"text": "a { b < c"}"#);
    }

    #[test]
    fn escaped_quote_in_string_handled() {
        // `\"` inside a JSON string must not close the string prematurely.
        let deltas = vec![r#"{"q": "she said \"hi\" "}"#];
        let chunks = collect_all(deltas);
        let joined: String = chunks.concat();
        assert_eq!(joined, r#"{"q": "she said \"hi\" "}"#);
    }

    #[test]
    fn escaped_backslash_in_string_handled() {
        // `\\` should consume a single backslash, then the following `"` closes the string.
        let deltas = vec![r#"{"p": "a\\"}"#];
        let chunks = collect_all(deltas);
        let joined: String = chunks.concat();
        assert_eq!(joined, r#"{"p": "a\\"}"#);
    }

    #[test]
    fn flush_all_emits_residual_unclosed_content() {
        // If the stream ends with an unclosed `<tool_call>`, we still emit it
        // (better than dropping silently).
        let mut buf = StreamBoundaryBuffer::new();
        assert!(buf.push("<tool_call>partial").is_none());
        let residue = buf.flush_all().unwrap();
        assert_eq!(residue, "<tool_call>partial");
    }

    #[test]
    fn long_plain_text_flushes_progressively() {
        // A long line without newline still flushes once it crosses MIN_FLUSH_CHARS.
        let long = "a".repeat(200);
        let mut buf = StreamBoundaryBuffer::new();
        let out = buf.push(&long).expect("should flush after MIN_FLUSH_CHARS");
        assert!(out.len() >= MIN_FLUSH_CHARS);
        let rest: String = buf.flush_all().unwrap_or_default();
        assert_eq!(out.len() + rest.len(), 200);
    }

    #[test]
    fn multibyte_utf8_not_split_mid_codepoint() {
        // Chinese characters are 3-byte UTF-8. Feed enough to cross MIN_FLUSH_CHARS.
        // Each `中` = 3 bytes; 30 of them = 90 bytes.
        let s: String = "中".repeat(30);
        let mut buf = StreamBoundaryBuffer::new();
        let out = buf.push(&s);
        // Whatever flushes must be valid UTF-8 (String::from guarantees) and
        // when concatenated with the residue must equal the input.
        let first = out.unwrap_or_default();
        let rest = buf.flush_all().unwrap_or_default();
        assert_eq!(format!("{first}{rest}"), s);
    }

    #[test]
    fn nested_json_objects_only_flush_at_outer_close() {
        let deltas = vec![r#"{"outer": {"inner": 1}"#, "}"];
        let chunks = collect_all(deltas);
        // Final concatenation = full object.
        let joined: String = chunks.concat();
        assert_eq!(joined, r#"{"outer": {"inner": 1}}"#);
        // No intermediate chunk should split the outer object.
        for c in &chunks {
            let lo = c.matches('{').count();
            let lc = c.matches('}').count();
            assert_eq!(lo, lc);
        }
    }

    #[test]
    fn text_then_tool_call_then_text() {
        // Mixed content: leading text, then a tool call, then trailing text.
        let deltas = vec!["Sure, let me try.\n", "<tool_call>x</tool_call>", "\nDone.\n"];
        let chunks = collect_all(deltas);
        let joined: String = chunks.concat();
        assert_eq!(joined, "Sure, let me try.\n<tool_call>x</tool_call>\nDone.\n");
        for c in &chunks {
            let opens = c.matches('<').count();
            let closes = c.matches('>').count();
            assert_eq!(opens, closes);
        }
    }
}

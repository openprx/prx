//! Incremental UTF-8 decoder for chunked HTTP byte streams.
//!
//! HTTP chunk boundaries have nothing to do with UTF-8 character boundaries: a
//! 3-byte CJK character routinely arrives as 2 bytes in one chunk and 1 byte in
//! the next, and 8 KiB read buffers make a split land on a character boundary
//! remarkably often. Decoding each chunk on its own therefore reports a
//! perfectly healthy stream as corrupt and kills the whole turn.
//!
//! [`Utf8StreamDecoder`] keeps the trailing bytes of an incomplete sequence and
//! completes them from the next chunk. The distinction that makes this safe is
//! [`std::str::Utf8Error::error_len`]:
//!
//! * `None` — the input simply *ends* inside a well-formed prefix. More bytes
//!   are needed; this is not an error.
//! * `Some(n)` — the bytes are genuinely malformed (stray continuation byte,
//!   overlong encoding, surrogate half, out-of-range scalar). This *is* an
//!   error and is still surfaced.
//!
//! Two further rules keep untrusted input bounded:
//!
//! * At most [`MAX_PENDING_BYTES`] (3) bytes are ever carried between chunks —
//!   the longest possible prefix of a 4-byte sequence. Anything larger is
//!   malformed input, not an incomplete character, and is rejected.
//! * [`Utf8StreamDecoder::finish`] must be called when the stream ends. Bytes
//!   still pending at that point are a real truncation and are reported rather
//!   than silently dropped.

use super::traits::StreamError;
use std::fmt;

/// Longest valid UTF-8 encoding of a single Unicode scalar value.
const MAX_UTF8_SEQUENCE_LEN: usize = 4;

/// Upper bound on the bytes carried across a chunk boundary.
///
/// A sequence is at most 4 bytes long, so at most 3 of them can be buffered
/// while still waiting for the rest. This is the hard cap that keeps a
/// hostile or broken peer from growing the carry buffer without limit.
pub const MAX_PENDING_BYTES: usize = MAX_UTF8_SEQUENCE_LEN - 1;

/// Failure modes of [`Utf8StreamDecoder`].
///
/// Both variants are genuine data errors. An *incomplete* trailing sequence in
/// the middle of a stream is deliberately not represented here: it is normal
/// chunking and is handled by buffering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Utf8StreamError {
    /// Malformed bytes that can never become a valid sequence.
    InvalidSequence {
        /// Absolute byte offset of the first malformed byte within the stream.
        offset: usize,
        /// Number of bytes the decoder rejected.
        len: usize,
    },
    /// The stream ended while a multi-byte sequence was still incomplete.
    TruncatedAtEnd {
        /// Absolute byte offset where the unfinished sequence began.
        offset: usize,
        /// Number of trailing bytes that never formed a character.
        pending: usize,
    },
}

impl fmt::Display for Utf8StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSequence { offset, len } => {
                write!(f, "invalid utf-8 byte sequence of length {len} from index {offset}")
            }
            Self::TruncatedAtEnd { offset, pending } => write!(
                f,
                "stream ended with an incomplete utf-8 byte sequence ({pending} trailing byte(s)) from index {offset}"
            ),
        }
    }
}

impl std::error::Error for Utf8StreamError {}

/// Stateful UTF-8 decoder that survives arbitrary chunk boundaries.
///
/// Feed every chunk to [`push`](Self::push) and call [`finish`](Self::finish)
/// exactly once when the stream ends.
///
/// The decoder never allocates: decoded text is appended straight into the
/// caller's buffer, and the only state is a fixed 4-byte array.
#[derive(Debug, Clone, Default)]
pub struct Utf8StreamDecoder {
    /// Prefix of a sequence whose remaining bytes have not arrived yet.
    pending: [u8; MAX_UTF8_SEQUENCE_LEN],
    /// Live length of `pending`; never exceeds [`MAX_PENDING_BYTES`] between calls.
    pending_len: usize,
    /// Absolute offset of the next byte to be decoded, for error reporting.
    stream_pos: usize,
}

impl Utf8StreamDecoder {
    /// Create a decoder positioned at the start of a stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of bytes currently held back waiting for their continuation.
    ///
    /// Always in `0..=MAX_PENDING_BYTES` between calls.
    pub const fn pending_len(&self) -> usize {
        self.pending_len
    }

    /// Absolute number of bytes decoded and emitted so far.
    pub const fn stream_pos(&self) -> usize {
        self.stream_pos
    }

    /// Decode `chunk` and append the resulting text to `out`.
    ///
    /// A trailing incomplete sequence is retained for the next call instead of
    /// being reported as an error. Genuinely malformed bytes return
    /// [`Utf8StreamError::InvalidSequence`]; everything decoded before the bad
    /// bytes has already been appended to `out`.
    pub fn push(&mut self, chunk: &[u8], out: &mut String) -> Result<(), Utf8StreamError> {
        let rest = self.complete_pending(chunk, out)?;
        self.decode_rest(rest, out)
    }

    /// Decode `chunk`, substituting [`char::REPLACEMENT_CHARACTER`] for each
    /// malformed sequence instead of stopping, and return how many
    /// substitutions were made.
    ///
    /// This is for consumers that must keep parsing the *rest* of the chunk
    /// after a bad byte — a framed protocol such as SSE, where abandoning the
    /// chunk would tear a record in half and lose every well-formed event that
    /// followed. A split character is still carried across the boundary
    /// exactly as in [`push`](Self::push); only genuinely malformed bytes are
    /// replaced. A non-zero return value means data was corrupt and the caller
    /// is expected to say so — the marker in `out` keeps the damage visible in
    /// the payload as well, so a silently shortened message is impossible.
    ///
    /// Recovery resynchronises on the next byte that can start a character, so
    /// a malformed run spanning a chunk boundary may yield more than one
    /// marker where a single-pass decoder would emit one. The count is a
    /// corruption signal, not a byte-exact damage measure.
    pub fn push_lossy(&mut self, chunk: &[u8], out: &mut String) -> usize {
        let mut rest = chunk;
        let mut replacements = 0usize;
        loop {
            let pos_before = self.stream_pos;
            let carried = self.pending_len;
            let Err(err) = self.push(rest, out) else {
                return replacements;
            };
            let Utf8StreamError::InvalidSequence { len, .. } = err else {
                // `push` never ends a stream, so it never truncates one.
                return replacements;
            };
            out.push(char::REPLACEMENT_CHARACTER);
            replacements = replacements.saturating_add(1);

            let decoded = self.stream_pos.saturating_sub(pos_before);
            // `decoded == 0` with a non-empty carry means the malformed unit
            // started in the bytes held over from an earlier chunk, which
            // `push` has already discarded: `rest` must be re-examined from
            // the top rather than skipped into. The now-empty carry is what
            // guarantees the retry cannot take this branch twice.
            let skip = if carried > 0 && decoded == 0 {
                0
            } else {
                // Everything `push` decoded came from `rest` except the bytes
                // it inherited as carry; the malformed unit follows.
                decoded.saturating_sub(carried).saturating_add(len).max(1)
            };
            let Some(tail) = rest.get(skip..) else {
                // A skip past the end means the whole remainder was rejected.
                return replacements;
            };
            rest = tail;
        }
    }

    /// Signal end of stream.
    ///
    /// Returns [`Utf8StreamError::TruncatedAtEnd`] when bytes are still
    /// pending: the peer stopped mid-character, which is real truncation and
    /// must not be swallowed.
    pub const fn finish(&mut self) -> Result<(), Utf8StreamError> {
        if self.pending_len == 0 {
            return Ok(());
        }
        let pending = self.pending_len;
        self.pending_len = 0;
        Err(Utf8StreamError::TruncatedAtEnd {
            offset: self.stream_pos,
            pending,
        })
    }

    /// Consume bytes from `chunk` until the carried-over sequence completes.
    ///
    /// Returns the unconsumed remainder of `chunk`.
    fn complete_pending<'a>(&mut self, chunk: &'a [u8], out: &mut String) -> Result<&'a [u8], Utf8StreamError> {
        let mut rest = chunk;
        while self.pending_len > 0 {
            let Some((&byte, tail)) = rest.split_first() else {
                // Chunk exhausted while still incomplete: keep waiting.
                return Ok(rest);
            };
            let Some(slot) = self.pending.get_mut(self.pending_len) else {
                // Unreachable while the loop below keeps `pending_len < 4`,
                // but bounded input deserves a bounded failure, not a panic.
                return Err(self.reject(MAX_UTF8_SEQUENCE_LEN));
            };
            *slot = byte;
            self.pending_len += 1;
            rest = tail;

            let Some(seq) = self.pending.get(..self.pending_len) else {
                return Err(self.reject(self.pending_len));
            };
            match std::str::from_utf8(seq) {
                Ok(text) => {
                    out.push_str(text);
                    self.stream_pos += self.pending_len;
                    self.pending_len = 0;
                }
                // Still a well-formed prefix and there is room for more bytes.
                Err(err) if err.error_len().is_none() && self.pending_len < MAX_UTF8_SEQUENCE_LEN => {}
                // Malformed, or four bytes that still failed to form a
                // character — neither can be fixed by waiting.
                Err(err) => {
                    let len = err.error_len().unwrap_or(self.pending_len);
                    return Err(self.reject(len));
                }
            }
        }
        Ok(rest)
    }

    /// Decode a chunk remainder that starts on a character boundary.
    fn decode_rest(&mut self, rest: &[u8], out: &mut String) -> Result<(), Utf8StreamError> {
        let err = match std::str::from_utf8(rest) {
            Ok(text) => {
                out.push_str(text);
                self.stream_pos += rest.len();
                return Ok(());
            }
            Err(err) => err,
        };

        let valid_up_to = err.valid_up_to();
        let Some((valid, tail)) = rest.split_at_checked(valid_up_to) else {
            return Err(self.reject(rest.len()));
        };
        match std::str::from_utf8(valid) {
            Ok(text) => out.push_str(text),
            // `valid_up_to` guarantees this half decodes; refuse rather than panic.
            Err(_) => return Err(self.reject(valid_up_to)),
        }
        self.stream_pos += valid_up_to;

        match err.error_len() {
            // Genuinely malformed bytes: surface them.
            Some(len) => Err(self.reject(len)),
            // Truncated tail: carry it into the next chunk, within the cap.
            None => {
                if tail.len() > MAX_PENDING_BYTES {
                    return Err(self.reject(tail.len()));
                }
                let Some(slot) = self.pending.get_mut(..tail.len()) else {
                    return Err(self.reject(tail.len()));
                };
                slot.copy_from_slice(tail);
                self.pending_len = tail.len();
                Ok(())
            }
        }
    }

    /// Drop any carried state and build an `InvalidSequence` at the current offset.
    const fn reject(&mut self, len: usize) -> Utf8StreamError {
        self.pending_len = 0;
        Utf8StreamError::InvalidSequence {
            offset: self.stream_pos,
            len,
        }
    }
}

/// Provider-facing adapter that reports decode failures as [`StreamError`].
///
/// Every LLM provider drives its SSE body through one of these so the
/// byte-to-character layer behaves identically everywhere; the SSE *record*
/// accumulation (`\n` / `\n\n` framing) stays where it already lives, one
/// layer above.
#[derive(Debug, Clone)]
pub struct SseTextDecoder {
    inner: Utf8StreamDecoder,
    /// Provider name used in error messages, e.g. `"Anthropic"`.
    label: &'static str,
}

impl SseTextDecoder {
    /// Create a decoder that labels its errors with `label`.
    pub fn new(label: &'static str) -> Self {
        Self {
            inner: Utf8StreamDecoder::new(),
            label,
        }
    }

    /// Append the decoded text of one HTTP chunk to `out`.
    ///
    /// A character split across this and the next chunk is buffered, not
    /// reported: chunk boundaries are not character boundaries.
    pub fn push(&mut self, bytes: &[u8], out: &mut String) -> Result<(), StreamError> {
        self.inner.push(bytes, out).map_err(|e| self.to_stream_error(&e))
    }

    /// Assert the stream ended on a character boundary.
    pub fn finish(&mut self) -> Result<(), StreamError> {
        self.inner.finish().map_err(|e| self.to_stream_error(&e))
    }

    /// Bytes currently held back waiting for their continuation.
    pub const fn pending_len(&self) -> usize {
        self.inner.pending_len()
    }

    fn to_stream_error(&self, err: &Utf8StreamError) -> StreamError {
        StreamError::InvalidSse(format!("non-utf8 byte in {} SSE: {err}", self.label))
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    /// Decode `chunks` in order and return the concatenated text.
    fn decode_all(chunks: &[&[u8]]) -> Result<String, Utf8StreamError> {
        let mut decoder = Utf8StreamDecoder::new();
        let mut out = String::new();
        for chunk in chunks {
            decoder.push(chunk, &mut out)?;
        }
        decoder.finish()?;
        Ok(out)
    }

    #[test]
    fn ascii_passes_through_unchanged() {
        assert_eq!(decode_all(&[b"hello ", b"world"]).unwrap(), "hello world");
    }

    #[test]
    fn empty_chunks_are_ignored() {
        assert_eq!(decode_all(&[b"", b"ab", b"", b"", b"cd", b""]).unwrap(), "abcd");
    }

    #[test]
    fn three_byte_character_split_two_plus_one() {
        let bytes = "中".as_bytes();
        assert_eq!(bytes.len(), 3);
        assert_eq!(decode_all(&[&bytes[..2], &bytes[2..]]).unwrap(), "中");
    }

    #[test]
    fn three_byte_character_split_one_plus_two() {
        let bytes = "文".as_bytes();
        assert_eq!(decode_all(&[&bytes[..1], &bytes[1..]]).unwrap(), "文");
    }

    #[test]
    fn four_byte_character_split_byte_by_byte() {
        let bytes = "𝄞".as_bytes();
        assert_eq!(bytes.len(), 4);
        let chunks: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(decode_all(&chunks).unwrap(), "𝄞");
    }

    #[test]
    fn consecutive_incomplete_chunks_accumulate() {
        // Three separate chunks each carrying one byte of the same character,
        // with empty chunks interleaved.
        let bytes = "汉".as_bytes();
        assert_eq!(
            decode_all(&[&bytes[..1], b"", &bytes[1..2], b"", &bytes[2..]]).unwrap(),
            "汉"
        );
    }

    #[test]
    fn chunk_ending_exactly_on_a_character_boundary_carries_nothing() {
        let mut decoder = Utf8StreamDecoder::new();
        let mut out = String::new();
        decoder.push("中文".as_bytes(), &mut out).unwrap();
        assert_eq!(decoder.pending_len(), 0);
        assert_eq!(out, "中文");
        decoder.finish().unwrap();
    }

    #[test]
    fn pending_never_exceeds_three_bytes() {
        let bytes = "𝄞".as_bytes();
        let mut decoder = Utf8StreamDecoder::new();
        let mut out = String::new();
        for take in 1..bytes.len() {
            let mut fresh = Utf8StreamDecoder::new();
            let mut sink = String::new();
            fresh.push(&bytes[..take], &mut sink).unwrap();
            assert!(fresh.pending_len() <= MAX_PENDING_BYTES, "carry must stay bounded");
        }
        decoder.push(&bytes[..3], &mut out).unwrap();
        assert_eq!(decoder.pending_len(), 3);
    }

    #[test]
    fn text_before_a_split_is_emitted_immediately() {
        let mut decoder = Utf8StreamDecoder::new();
        let mut out = String::new();
        let mut first = b"prefix ".to_vec();
        first.extend_from_slice(&"中".as_bytes()[..2]);
        decoder.push(&first, &mut out).unwrap();
        assert_eq!(out, "prefix ", "valid prefix must not be held back");
        decoder.push(&"中".as_bytes()[2..], &mut out).unwrap();
        assert_eq!(out, "prefix 中");
    }

    // --- genuinely invalid input must still be rejected ------------------

    #[test]
    fn lone_continuation_byte_is_rejected() {
        let err = decode_all(&[b"ok", &[0x80], b"more"]).unwrap_err();
        assert!(
            matches!(err, Utf8StreamError::InvalidSequence { .. }),
            "stray continuation byte must be an error, got {err:?}"
        );
    }

    #[test]
    fn overlong_encoding_is_rejected() {
        // 0xC0 0xAF is an overlong encoding of '/'.
        let err = decode_all(&[&[0xC0, 0xAF]]).unwrap_err();
        assert!(matches!(err, Utf8StreamError::InvalidSequence { .. }), "got {err:?}");
    }

    #[test]
    fn surrogate_half_is_rejected() {
        // 0xED 0xA0 0x80 is CESU-8 for U+D800, illegal in UTF-8.
        let err = decode_all(&[&[0xED, 0xA0, 0x80]]).unwrap_err();
        assert!(matches!(err, Utf8StreamError::InvalidSequence { .. }), "got {err:?}");
    }

    #[test]
    fn out_of_range_scalar_is_rejected() {
        // 0xF5.. encodes beyond U+10FFFF.
        let err = decode_all(&[&[0xF5, 0x80, 0x80, 0x80]]).unwrap_err();
        assert!(matches!(err, Utf8StreamError::InvalidSequence { .. }), "got {err:?}");
    }

    #[test]
    fn invalid_continuation_split_across_chunks_is_rejected() {
        // A valid lead byte followed, in the *next* chunk, by a byte that
        // cannot continue it. Buffering must not turn this into silence.
        let err = decode_all(&[&[0xE4], &[0x41]]).unwrap_err();
        assert!(matches!(err, Utf8StreamError::InvalidSequence { .. }), "got {err:?}");
    }

    #[test]
    fn five_byte_lead_is_rejected_not_buffered() {
        // 0xF8 is not a legal lead byte at all: it must never be carried.
        let mut decoder = Utf8StreamDecoder::new();
        let mut out = String::new();
        let err = decoder.push(&[0xF8, 0x80], &mut out).unwrap_err();
        assert!(matches!(err, Utf8StreamError::InvalidSequence { .. }), "got {err:?}");
        assert_eq!(decoder.pending_len(), 0, "rejected input must not stay buffered");
    }

    #[test]
    fn valid_text_before_invalid_bytes_is_still_emitted() {
        let mut decoder = Utf8StreamDecoder::new();
        let mut out = String::new();
        let mut chunk = "good 中文".as_bytes().to_vec();
        chunk.push(0x80);
        let err = decoder.push(&chunk, &mut out).unwrap_err();
        assert!(matches!(err, Utf8StreamError::InvalidSequence { .. }));
        assert_eq!(out, "good 中文");
    }

    #[test]
    fn invalid_sequence_offset_points_at_the_bad_byte() {
        let mut decoder = Utf8StreamDecoder::new();
        let mut out = String::new();
        let mut chunk = b"abcd".to_vec();
        chunk.push(0x80);
        let err = decoder.push(&chunk, &mut out).unwrap_err();
        match err {
            Utf8StreamError::InvalidSequence { offset, .. } => assert_eq!(offset, 4),
            other => panic!("expected InvalidSequence, got {other:?}"),
        }
    }

    // --- end-of-stream truncation ---------------------------------------

    #[test]
    fn stream_ending_mid_character_is_reported() {
        let bytes = "中".as_bytes();
        let err = decode_all(&[&bytes[..2]]).unwrap_err();
        match err {
            Utf8StreamError::TruncatedAtEnd { pending, .. } => assert_eq!(pending, 2),
            other => panic!("expected TruncatedAtEnd, got {other:?}"),
        }
    }

    #[test]
    fn truncation_is_not_silently_swallowed_after_valid_text() {
        let mut decoder = Utf8StreamDecoder::new();
        let mut out = String::new();
        let mut chunk = b"hello ".to_vec();
        chunk.extend_from_slice(&"界".as_bytes()[..1]);
        decoder.push(&chunk, &mut out).unwrap();
        assert_eq!(out, "hello ");
        assert!(decoder.finish().is_err(), "trailing partial byte must be reported");
    }

    #[test]
    fn finish_on_a_clean_stream_succeeds_and_is_idempotent() {
        let mut decoder = Utf8StreamDecoder::new();
        let mut out = String::new();
        decoder.push(b"done", &mut out).unwrap();
        assert!(decoder.finish().is_ok());
        assert!(decoder.finish().is_ok());
    }

    // --- the production scenario ----------------------------------------

    #[test]
    fn eight_kib_boundary_split_of_chinese_text_decodes_intact() {
        // Reproduces the shipped failure: a long Chinese answer read through an
        // 8 KiB buffer, where byte 8192 lands inside a 3-byte character.
        const BUF: usize = 8192;
        let text: String = "长回答内容测试汉字流式解码".repeat(2000);
        let bytes = text.as_bytes();
        assert!(bytes.len() > BUF * 2);
        // Confirm the boundary really cuts a character in half.
        assert!(
            !text.is_char_boundary(BUF),
            "test setup: byte {BUF} must fall inside a character"
        );

        let chunks: Vec<&[u8]> = bytes.chunks(BUF).collect();
        assert_eq!(decode_all(&chunks).unwrap(), text);
    }

    #[test]
    fn production_failure_offsets_decode_intact() {
        // The offsets recorded in production logs: 1748 / 1911 / 5654 / 8190.
        for split in [1748_usize, 1911, 5654, 8190] {
            // All characters below are 3 bytes wide, so an ASCII pad of 1 byte
            // is enough to make the recorded offset land *inside* a character,
            // exactly as it did upstream.
            let pad = if split % 3 == 0 { 1 } else { 0 };
            let text: String = format!("{}{}", "x".repeat(pad), "流式响应中文内容".repeat(2000));
            assert!(
                !text.is_char_boundary(split),
                "test setup: byte {split} must fall inside a character"
            );
            let bytes = text.as_bytes();
            let decoded = decode_all(&[&bytes[..split], &bytes[split..]]).unwrap();
            assert_eq!(decoded, text, "split at {split} must round-trip");
        }
    }

    #[test]
    fn every_single_byte_split_of_mixed_text_round_trips() {
        // Exhaustive: mixed ASCII / CJK / emoji cut at every possible offset.
        let text = "a中b文c𝄞d😀e漢字";
        let bytes = text.as_bytes();
        for split in 0..=bytes.len() {
            let decoded = decode_all(&[&bytes[..split], &bytes[split..]])
                .unwrap_or_else(|e| panic!("split at {split} failed: {e}"));
            assert_eq!(decoded, text, "split at {split}");
        }
    }

    // --- lossy recovery for framed consumers ----------------------------

    /// Decode `chunks` with `push_lossy` and return the text plus the total
    /// number of replacement characters substituted.
    fn decode_all_lossy(chunks: &[&[u8]]) -> (String, usize) {
        let mut decoder = Utf8StreamDecoder::new();
        let mut out = String::new();
        let mut replaced = 0;
        for chunk in chunks {
            replaced += decoder.push_lossy(chunk, &mut out);
        }
        (out, replaced)
    }

    #[test]
    fn lossy_carries_split_characters_without_replacing_anything() {
        let bytes = "中文内容".as_bytes();
        for split in 0..=bytes.len() {
            let (text, replaced) = decode_all_lossy(&[&bytes[..split], &bytes[split..]]);
            assert_eq!(text, "中文内容", "split at {split}");
            assert_eq!(replaced, 0, "a split character is not corruption (split at {split})");
        }
    }

    #[test]
    fn lossy_replaces_only_the_bad_bytes_and_keeps_the_rest_of_the_chunk() {
        // The whole point: a bad byte must not take the surrounding framing
        // with it. Everything before *and after* it still has to arrive.
        let mut chunk = b"before".to_vec();
        chunk.push(0x80);
        chunk.extend_from_slice("after中文".as_bytes());
        let (text, replaced) = decode_all_lossy(&[&chunk]);
        assert_eq!(text, "before\u{fffd}after中文");
        assert_eq!(replaced, 1, "corruption must be counted, not hidden");
    }

    #[test]
    fn lossy_reports_a_bad_byte_that_only_shows_up_after_a_boundary() {
        // A valid lead byte carried over, then a byte that cannot continue it.
        // Buffering must not turn this into silence.
        let (text, replaced) = decode_all_lossy(&[b"ok", &[0xE4], &[0x41], b"end"]);
        assert!(replaced >= 1, "cross-chunk corruption must be reported");
        assert!(
            text.contains('\u{fffd}'),
            "damage must be visible in the payload: {text:?}"
        );
        assert!(text.starts_with("ok"), "text before the damage must survive: {text:?}");
        assert!(text.ends_with("Aend"), "text after the damage must survive: {text:?}");
    }

    #[test]
    fn lossy_never_shortens_a_message_silently() {
        // Every byte of legitimate text on both sides of the damage is kept, so
        // a reader can never see a plausible-but-shorter message.
        let mut chunk = "流式".as_bytes().to_vec();
        chunk.extend_from_slice(&[0xC0, 0xAF]); // overlong '/'
        chunk.extend_from_slice("解码".as_bytes());
        let (text, replaced) = decode_all_lossy(&[&chunk]);
        assert!(replaced >= 1);
        assert!(text.contains("流式") && text.contains("解码"), "got {text:?}");
    }

    #[test]
    fn lossy_terminates_on_input_that_is_entirely_garbage() {
        // Guards the recovery loop: no byte pattern may make it spin.
        let garbage: Vec<u8> = (0x80u8..=0xFF).collect();
        let (text, replaced) = decode_all_lossy(&[&garbage]);
        assert!(replaced > 0);
        assert!(text.chars().all(|c| c == char::REPLACEMENT_CHARACTER), "got {text:?}");
    }

    #[test]
    fn lossy_terminates_when_every_byte_arrives_alone() {
        let mut mixed = "a中b".as_bytes().to_vec();
        mixed.push(0xF8);
        mixed.extend_from_slice("文c".as_bytes());
        let chunks: Vec<&[u8]> = mixed.chunks(1).collect();
        let (text, replaced) = decode_all_lossy(&chunks);
        assert!(replaced >= 1);
        assert!(text.starts_with("a中b"), "got {text:?}");
        assert!(text.ends_with("文c"), "got {text:?}");
    }

    #[test]
    fn lossy_still_reports_a_truncated_tail_at_end_of_stream() {
        let mut decoder = Utf8StreamDecoder::new();
        let mut out = String::new();
        let bytes = "界".as_bytes();
        assert_eq!(decoder.push_lossy(&bytes[..2], &mut out), 0);
        assert!(
            decoder.finish().is_err(),
            "a tail lost at end of stream must be reported"
        );
    }

    #[test]
    fn byte_at_a_time_delivery_round_trips() {
        let text = "网络分片不该破坏解码 😀 ok";
        let bytes = text.as_bytes();
        let chunks: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(decode_all(&chunks).unwrap(), text);
    }
}

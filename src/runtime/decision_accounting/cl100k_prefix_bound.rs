//! Fast conservative cl100k bound for latency-bounded accounting.
//!
//! The bundled ranks are the complete prefix 0..4095 of the cl100k merge
//! table shipped by `tiktoken-rs` 0.5.9. Running BPE with a complete rank
//! prefix can only leave more tokens than continuing with the remaining
//! ranks, so the result is an upper bound on the full tokenizer. The scanner
//! reproduces cl100k's regex pieces for ASCII input. Non-ASCII input declines
//! the fast path and retains the full tokenizer fallback.

use std::{collections::HashMap, sync::LazyLock};

use base64::Engine as _;

const PREFIX: &str = include_str!("cl100k_prefix.tiktoken");
const PREFIX_LEN: usize = 4096;
/// Below this size the existing byte/heuristic bound is cheaper to evaluate;
/// inconclusive small inputs retain their exact-tokenizer path.
pub(super) const MIN_INPUT_BYTES: usize = 2 * 1024;

fn prefix_ranks() -> Option<&'static HashMap<Vec<u8>, usize>> {
    static RANKS: LazyLock<Option<HashMap<Vec<u8>, usize>>> = LazyLock::new(|| {
        let mut ranks = HashMap::with_capacity(PREFIX_LEN);
        for (expected, line) in PREFIX.lines().enumerate() {
            let (encoded, written_rank) = line.split_once(' ')?;
            let rank = written_rank.parse::<usize>().ok()?;
            if rank != expected {
                return None;
            }
            let token = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
            if ranks.insert(token, rank).is_some() {
                return None;
            }
        }
        if ranks.len() != PREFIX_LEN || !(u8::MIN..=u8::MAX).all(|byte| ranks.contains_key([byte].as_slice())) {
            return None;
        }
        Some(ranks)
    });
    RANKS.as_ref()
}

fn pair_rank(
    piece: &[u8],
    parts: &[(usize, usize)],
    index: usize,
    skip: usize,
    ranks: &HashMap<Vec<u8>, usize>,
) -> Option<usize> {
    let start = parts.get(index)?.0;
    let end = parts.get(index.checked_add(skip)?.checked_add(2)?)?.0;
    ranks.get(piece.get(start..end)?).copied()
}

fn prefix_piece_tokens(piece: &[u8], ranks: &HashMap<Vec<u8>, usize>) -> usize {
    if piece.is_empty() {
        return 0;
    }
    if ranks.contains_key(piece) {
        return 1;
    }
    let mut parts: Vec<_> = (0..=piece.len()).map(|offset| (offset, usize::MAX)).collect();
    for index in 0..parts.len().saturating_sub(2) {
        let rank = pair_rank(piece, &parts, index, 0, ranks).unwrap_or(usize::MAX);
        if let Some(part) = parts.get_mut(index) {
            part.1 = rank;
        }
    }
    while let Some((index, rank)) = parts
        .iter()
        .take(parts.len().saturating_sub(1))
        .enumerate()
        .map(|(index, (_, rank))| (index, *rank))
        .min_by_key(|(_, rank)| *rank)
    {
        if rank == usize::MAX {
            break;
        }
        let merged_rank = pair_rank(piece, &parts, index, 1, ranks).unwrap_or(usize::MAX);
        if let Some(part) = parts.get_mut(index) {
            part.1 = merged_rank;
        }
        if index > 0 {
            let prior_rank = pair_rank(piece, &parts, index - 1, 1, ranks).unwrap_or(usize::MAX);
            if let Some(part) = parts.get_mut(index - 1) {
                part.1 = prior_rank;
            }
        }
        parts.remove(index + 1);
    }
    parts.len().saturating_sub(1)
}

fn contraction_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start).copied()? != b'\'' {
        return None;
    }
    for suffix in ["s", "t", "re", "ve", "m", "ll", "d"] {
        let end = start.checked_add(1 + suffix.len())?;
        let candidate = bytes.get(start + 1..end)?;
        if candidate.eq_ignore_ascii_case(suffix.as_bytes()) {
            return Some(end);
        }
    }
    None
}

fn ascii_piece_end(bytes: &[u8], start: usize) -> Option<usize> {
    let current = bytes.get(start).copied()?;
    if let Some(end) = contraction_end(bytes, start) {
        return Some(end);
    }

    let letter_start = if current.is_ascii_alphabetic() {
        Some(start)
    } else if current != b'\r'
        && current != b'\n'
        && !current.is_ascii_alphanumeric()
        && bytes.get(start + 1).is_some_and(u8::is_ascii_alphabetic)
    {
        Some(start + 1)
    } else {
        None
    };
    if let Some(letter_start) = letter_start {
        let mut end = letter_start;
        while bytes.get(end).is_some_and(u8::is_ascii_alphabetic) {
            end += 1;
        }
        return Some(end);
    }

    if current.is_ascii_digit() {
        let mut end = start;
        while end < start.saturating_add(3) && bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        return Some(end);
    }

    let punctuation_start = if current == b' '
        && bytes
            .get(start + 1)
            .is_some_and(|next| !next.is_ascii_whitespace() && !next.is_ascii_alphanumeric())
    {
        Some(start + 1)
    } else if !current.is_ascii_whitespace() && !current.is_ascii_alphanumeric() {
        Some(start)
    } else {
        None
    };
    if let Some(punctuation_start) = punctuation_start {
        let mut end = punctuation_start;
        while bytes
            .get(end)
            .is_some_and(|byte| !byte.is_ascii_whitespace() && !byte.is_ascii_alphanumeric())
        {
            end += 1;
        }
        while bytes.get(end).is_some_and(|byte| matches!(byte, b'\r' | b'\n')) {
            end += 1;
        }
        return Some(end);
    }

    if current.is_ascii_whitespace() {
        let mut end = start;
        let mut last_newline_end = None;
        while let Some(byte) = bytes.get(end).copied().filter(u8::is_ascii_whitespace) {
            end += 1;
            if matches!(byte, b'\r' | b'\n') {
                last_newline_end = Some(end);
            }
        }
        if let Some(newline_end) = last_newline_end {
            return Some(newline_end);
        }
        if end == bytes.len() || end == start + 1 {
            return Some(end);
        }
        return Some(end - 1);
    }

    Some(start + 1)
}

/// Return a fast upper bound on full cl100k tokens for ASCII text.
pub(super) fn upper_bound(text: &str) -> Option<usize> {
    if !text.is_ascii() {
        return None;
    }
    let ranks = prefix_ranks()?;
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut tokens = 0usize;
    while start < bytes.len() {
        let end = ascii_piece_end(bytes, start)?;
        if end <= start || end > bytes.len() {
            return None;
        }
        tokens = tokens.checked_add(prefix_piece_tokens(bytes.get(start..end)?, ranks))?;
        start = end;
    }
    Some(tokens)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::upper_bound;

    fn assert_bounds(samples: impl IntoIterator<Item = String>) {
        static ENCODER: std::sync::LazyLock<tiktoken_rs::CoreBPE> =
            std::sync::LazyLock::new(|| tiktoken_rs::cl100k_base().expect("test cl100k encoder"));
        for sample in samples {
            let exact = ENCODER.encode_with_special_tokens(&sample).len();
            let upper = upper_bound(&sample).expect("ASCII prefix bound");
            assert!(
                exact <= upper,
                "prefix bound under-counted {sample:?}: exact={exact} upper={upper}"
            );
        }
    }

    #[test]
    fn prefix_bpe_bounds_curated_cl100k_shapes() {
        assert_bounds(
            [
                "",
                "a",
                "hello world",
                "tiktoken",
                "don't we're I'LL",
                "{\"state\":{\"candidates\":[{\"action\":\"use_tool\",\"target\":\"native_tool\"}]}}",
                "spaces before text and punctuation: [] {} == !=\nnext line\r\n",
                "<|endoftext|>",
                "1234567890",
            ]
            .into_iter()
            .map(str::to_owned),
        );
    }

    #[test]
    fn prefix_bpe_bounds_deterministic_ascii_corpus() {
        let alphabet = b" abcXYZ019_-'{}[],:.\n\r\t";
        let mut state = 0x5eed_u64;
        let mut samples = Vec::new();
        for length in 0..256usize {
            let mut sample = String::with_capacity(length);
            for _ in 0..length {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                sample.push(char::from(alphabet[(state as usize) % alphabet.len()]));
            }
            samples.push(sample);
        }
        assert_bounds(samples);
    }

    #[test]
    fn prefix_bpe_declines_non_ascii_input() {
        assert_eq!(upper_bound("PRX λ"), None);
    }
}

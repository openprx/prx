// Shared test-code recognition for the source scanners.
//
// Included with `include!` by `tests/architecture_boundaries/` and by the
// audit-wiring gate in `src/runtime/bootstrap.rs`, so both apply exactly one
// rule for "which text is production code". Not a test target of its own
// (cargo only builds `tests/*.rs`). Items only: an included file may carry no
// inner attributes or module docs.
//
// The rule, in one place:
// - a file whose first line of code is exactly `#![cfg(test)]` is test code
//   (`is_test_only_file`);
// - otherwise every item, field, parameter, argument, match arm or statement
//   that carries `#[cfg(test)]` is test code, and its extent is computed on
//   text whose comments and literals are blanked, so no bracket inside a
//   string, char literal or comment can move it (`cfg_test_ranges`);
// - the stripped text keeps every character position and line of the input:
//   test code is replaced by spaces, so function signatures and the closers
//   of enclosing constructs always survive.

pub(crate) const fn is_ident_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

/// Replace every comment, string literal and character literal with spaces.
///
/// Line structure and character offsets are preserved, so the result can still
/// be split into lines and searched for `fn` without a comment or a doc example
/// that merely *mentions* `.execute(` being mistaken for a call site.
pub(crate) fn blank_comments_and_literals(source: &str) -> String {
    memoized(MemoKind::Blanked, source, blank_comments_and_literals_uncached)
}

fn blank_comments_and_literals_uncached(source: &str) -> String {
    let characters: Vec<char> = source.chars().collect();
    let mut output = characters.clone();
    let length = characters.len();
    let blank = |output: &mut [char], from: usize, to: usize| {
        let end = to.min(length);
        if from >= end {
            return;
        }
        for slot in &mut output[from..end] {
            if *slot != '\n' {
                *slot = ' ';
            }
        }
    };
    let mut index = 0usize;
    while index < length {
        let character = characters[index];
        if character == '/' && index + 1 < length && characters[index + 1] == '/' {
            let mut end = index;
            while end < length && characters[end] != '\n' {
                end += 1;
            }
            blank(&mut output, index, end);
            index = end;
            continue;
        }
        if character == '/' && index + 1 < length && characters[index + 1] == '*' {
            let mut depth = 0usize;
            let mut end = index;
            while end < length {
                if characters[end] == '/' && end + 1 < length && characters[end + 1] == '*' {
                    depth += 1;
                    end += 2;
                    continue;
                }
                if characters[end] == '*' && end + 1 < length && characters[end + 1] == '/' {
                    depth = depth.saturating_sub(1);
                    end += 2;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                end += 1;
            }
            blank(&mut output, index, end);
            index = end;
            continue;
        }
        if character == 'r' || character == 'b' || character == 'c' {
            // Raw strings of every kind: `r"..."`, `r#"..."#`, `br"..."`,
            // `br#"..."#`, `cr"..."`, `cr#"..."#`. The literal is raw when an
            // `r` is in its prefix, whatever letter comes first, so a raw byte
            // or C string ending in a backslash (`br"C:\"`) ends at its quote.
            let mut cursor = index;
            while cursor < length && cursor - index < 2 && matches!(characters[cursor], 'r' | 'b' | 'c') {
                cursor += 1;
            }
            let mut hashes = 0usize;
            while cursor + hashes < length && characters[cursor + hashes] == '#' {
                hashes += 1;
            }
            let quote = cursor + hashes;
            let prefix_is_raw = characters[index..cursor].contains(&'r');
            let raw = cursor > index && quote < length && characters[quote] == '"' && prefix_is_raw;
            let leading_ident = index > 0 && is_ident_char(characters[index - 1]);
            if raw && !leading_ident {
                let mut end = quote + 1;
                loop {
                    if end >= length {
                        break;
                    }
                    if characters[end] == '"' {
                        let mut closing = 0usize;
                        while closing < hashes && end + 1 + closing < length && characters[end + 1 + closing] == '#' {
                            closing += 1;
                        }
                        if closing == hashes {
                            end += 1 + hashes;
                            break;
                        }
                    }
                    end += 1;
                }
                blank(&mut output, index, end);
                index = end;
                continue;
            }
        }
        if character == '"' {
            let mut end = index + 1;
            while end < length {
                if characters[end] == '\\' {
                    end += 2;
                    continue;
                }
                if characters[end] == '"' {
                    end += 1;
                    break;
                }
                end += 1;
            }
            blank(&mut output, index, end);
            index = end;
            continue;
        }
        if character == '\'' {
            // A character literal, not a lifetime: `'a'`, `'\n'` or `'\''`.
            let escaped = index + 1 < length && characters[index + 1] == '\\';
            let mut end = index + 1;
            if escaped {
                // Skip the backslash and the escaped character itself (which
                // may be a quote), then find the closing quote.
                end += 2;
                while end < length && characters[end] != '\'' && characters[end] != '\n' && end - index < 12 {
                    end += 1;
                }
            } else {
                end = index + 2;
            }
            if end < length && characters[end] == '\'' {
                blank(&mut output, index, end + 1);
                index = end + 1;
                continue;
            }
        }
        index += 1;
    }
    output.into_iter().collect()
}
/// Is this whole file test code?
///
/// True when the file's first line of code (blank lines and comments,
/// including `//!` module docs and nested block comments, skipped) is exactly
/// `#![cfg(test)]`. That inner attribute is enforced by the compiler: outside
/// `cfg(test)` the module the file defines is empty. It is a marker the file
/// carries, not a path or directory exemption. Any other first line, a marker
/// after the first item, or a composite predicate such as
/// `#![cfg(any(test, feature = "x"))]` leaves the file production code.
pub(crate) fn is_test_only_file(source: &str) -> bool {
    blank_comments_and_literals(source)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        == Some("#![cfg(test)]")
}

/// One `#[cfg(test)]`-attributed piece of source: character range
/// `[start, end)` of the input, attribute included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CfgTestRange {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// How the construct after a `#[cfg(test)]` attribute ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CfgTestShape {
    /// `fn`, `mod`, `impl`, `struct`, `enum`, `trait`, `union`,
    /// `macro_rules!`, `extern`: at `;` or where its top-level block closes.
    Item,
    /// `const`, `static`, `type`, `use`, `let`: at `;` only.
    Semicolon,
    /// A field, parameter, argument, match arm, expression or macro call: at a
    /// top-level `,` or `;`, or where a top-level block closes and nothing
    /// continues the expression.
    Expression,
}

pub(crate) fn masked_word_at(masked: &[char], from: usize) -> (String, usize) {
    let mut index = from;
    while index < masked.len() && masked[index].is_whitespace() {
        index += 1;
    }
    let start = index;
    while index < masked.len() && is_ident_char(masked[index]) {
        index += 1;
    }
    (masked[start..index].iter().collect(), index)
}

pub(crate) const fn close_of_group(masked: &[char], open: usize) -> usize {
    let mut depth = 0usize;
    let mut index = open;
    while index < masked.len() {
        match masked[index] {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return index + 1;
                }
            }
            _ => {}
        }
        index += 1;
    }
    masked.len()
}

fn cfg_test_shape(masked: &[char], from: usize) -> CfgTestShape {
    let (mut word, mut after) = masked_word_at(masked, from);
    if word == "pub" {
        let mut index = after;
        while index < masked.len() && masked[index].is_whitespace() {
            index += 1;
        }
        if index < masked.len() && masked[index] == '(' {
            after = close_of_group(masked, index);
        }
        (word, after) = masked_word_at(masked, after);
    }
    let (next, _) = masked_word_at(masked, after);
    match word.as_str() {
        "fn" | "mod" | "impl" | "struct" | "enum" | "trait" | "union" | "macro_rules" | "extern" => CfgTestShape::Item,
        "async" | "unsafe" if matches!(next.as_str(), "fn" | "impl" | "trait" | "extern") => CfgTestShape::Item,
        "const" if matches!(next.as_str(), "fn" | "unsafe" | "async" | "extern") => CfgTestShape::Item,
        // `const { .. }` is an inline const block: an expression, not an item.
        "const" if next.is_empty() => CfgTestShape::Expression,
        "const" | "static" | "type" | "use" | "let" => CfgTestShape::Semicolon,
        _ => CfgTestShape::Expression,
    }
}

/// End (exclusive) of the construct that starts at `from`, on masked text.
///
/// A closer that belongs to the enclosing construct (depth would drop below
/// zero) ends the item *before* it, so the enclosing `}` / `)` / `]` is never
/// part of the test code.
fn cfg_test_item_end(masked: &[char], from: usize, shape: CfgTestShape) -> usize {
    let mut depth = 0usize;
    let mut block = false;
    let mut index = from;
    while index < masked.len() {
        let character = masked[index];
        match character {
            '(' | '[' => depth += 1,
            '{' => {
                if depth == 0 {
                    block = true;
                }
                depth += 1;
            }
            ')' | ']' | '}' => {
                if depth == 0 {
                    return index;
                }
                depth -= 1;
                if character == '}' && depth == 0 && block && shape != CfgTestShape::Semicolon {
                    if shape == CfgTestShape::Item {
                        return index + 1;
                    }
                    let mut peek = index + 1;
                    while peek < masked.len() && masked[peek].is_whitespace() {
                        peek += 1;
                    }
                    if peek < masked.len() && matches!(masked[peek], ',' | ';') {
                        return peek + 1;
                    }
                    let (word, _) = masked_word_at(masked, peek);
                    // `.method()`, `.field`, `.await` and `?` continue the
                    // expression; `..` / `..=` begin a range pattern of the
                    // next match arm and do not.
                    let method_or_field = peek + 1 < masked.len()
                        && masked[peek] == '.'
                        && (is_ident_char(masked[peek + 1]) || masked[peek + 1].is_whitespace());
                    let continues = method_or_field
                        || peek < masked.len() && masked[peek] == '?'
                        || matches!(word.as_str(), "else" | "as");
                    if !continues {
                        return index + 1;
                    }
                }
            }
            ';' if depth == 0 => return index + 1,
            ',' if depth == 0 && shape == CfgTestShape::Expression => return index + 1,
            _ => {}
        }
        index += 1;
    }
    masked.len()
}

/// Every `#[cfg(test)]`-attributed construct of `source`, as character ranges
/// of `source`. Attributes are found and extents computed on
/// `blank_comments_and_literals(source)`, so an attribute spelled inside a
/// comment or string is not one, and no bracket inside a literal or comment
/// can stretch or cut an extent.
pub(crate) fn cfg_test_ranges(source: &str) -> Vec<CfgTestRange> {
    const ATTRIBUTE: [char; 12] = ['#', '[', 'c', 'f', 'g', '(', 't', 'e', 's', 't', ')', ']'];
    let masked: Vec<char> = blank_comments_and_literals(source).chars().collect();
    let mut ranges = Vec::new();
    let mut index = 0usize;
    while index + ATTRIBUTE.len() <= masked.len() {
        if masked[index..index + ATTRIBUTE.len()] != ATTRIBUTE {
            index += 1;
            continue;
        }
        let start = index;
        let mut cursor = index + ATTRIBUTE.len();
        loop {
            while cursor < masked.len() && masked[cursor].is_whitespace() {
                cursor += 1;
            }
            if cursor + 1 < masked.len() && masked[cursor] == '#' && masked[cursor + 1] == '[' {
                cursor = close_of_group(&masked, cursor + 1);
            } else {
                break;
            }
        }
        let shape = cfg_test_shape(&masked, cursor);
        let end = cfg_test_item_end(&masked, cursor, shape).max(cursor);
        ranges.push(CfgTestRange { start, end });
        index = end.max(start + 1);
    }
    ranges
}

/// `source` with the given character ranges replaced by spaces (newlines kept).
pub(crate) fn blank_ranges(source: &str, ranges: &[CfgTestRange]) -> String {
    let mut next = ranges.iter().peekable();
    let mut current: Option<CfgTestRange> = None;
    source
        .chars()
        .enumerate()
        .map(|(offset, character)| {
            while current.is_none_or(|range| offset >= range.end) {
                match next.peek() {
                    Some(range) if range.start <= offset => current = next.next().copied(),
                    _ => {
                        current = None;
                        break;
                    }
                }
            }
            match current {
                Some(range) if offset >= range.start && offset < range.end && character != '\n' => ' ',
                _ => character,
            }
        })
        .collect()
}

/// The production text of `source`: every `#[cfg(test)]` construct blanked
/// (line structure and positions kept), or nothing for a test-only file.
/// Works on raw and on already-blanked text alike.
pub(crate) fn strip_cfg_test_items(source: &str) -> String {
    memoized(MemoKind::Stripped, source, |source| {
        if is_test_only_file(source) {
            return String::new();
        }
        blank_ranges(source, &cfg_test_ranges(source))
    })
}

/// Which pure function a memo entry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum MemoKind {
    Blanked,
    Stripped,
}

/// Results of the two pure text functions above, shared by every thread of
/// the process: every tree scanner, and every scenario of every
/// planted-violation probe, strips the same disk files again. An entry is
/// found by a hash of the input and returned only when the stored input is
/// byte-identical, so a memo hit is always the exact result.
type StripMemo = std::collections::HashMap<(MemoKind, u64), Vec<(std::sync::Arc<str>, std::sync::Arc<str>)>>;

static STRIP_MEMO: std::sync::LazyLock<parking_lot::Mutex<StripMemo>> = std::sync::LazyLock::new(Default::default);

fn memoized(kind: MemoKind, input: &str, compute: impl FnOnce(&str) -> String) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    input.hash(&mut hasher);
    let key = (kind, hasher.finish());
    let hit = STRIP_MEMO.lock().get(&key).and_then(|entries| {
        entries
            .iter()
            .find(|(stored, _)| stored.as_ref() == input)
            .map(|(_, output)| output.to_string())
    });
    if let Some(output) = hit {
        return output;
    }
    let output = compute(input);
    STRIP_MEMO
        .lock()
        .entry(key)
        .or_default()
        .push((std::sync::Arc::from(input), std::sync::Arc::from(output.as_str())));
    output
}

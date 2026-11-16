//! I-LOG: no production log line in the session paths formats a value that
//! can carry session, turn, tool, key or backend-error text (PR #207 RCA).
//!
//! Every argument of every `tracing` event or span macro, and every reducer
//! `Effect::LogTrace` message, in `src/chat`, `src/agent`, `src/memory` and
//! `src/session_worker` (and the runtime terminal commit, see
//! [`LOG_SCAN_ROOTS`]) must be typed metadata. The scanner is an allowlist of
//! argument forms, not a blocklist of spellings: an argument that is none of
//! the forms below is a violation, whatever it is called.
//!
//! Permitted forms (after an optional `%`, `?`, `&`, `*` or `!`):
//! - a literal (number, boolean or string literal);
//! - a call to a payload-free helper ([`PAYLOAD_FREE_HELPERS`]): errors and
//!   free text are logged as their class (type, byte length, digest) only;
//! - a value whose last name is a metadata name ([`METADATA_NAMES`],
//!   [`METADATA_SUFFIXES`], [`METADATA_PREFIXES`]): an id, a length or count,
//!   a digest, an index, a duration, a kind, a flag; a call whose callee has
//!   such a name, or is a numeric accessor ([`NUMERIC_ACCESSORS`]), is one too;
//! - an identifier the enclosing function annotates with a scalar or reviewed
//!   fieldless type ([`TYPED_BINDINGS`]): the compiler then proves the type.
//!
//! The format string's inline captures (`{name}`, `{name:?}`) are judged the
//! same way by name; its positional arguments are arguments like any other.
//! User-visible messages (`bail!`, `format!` outside `LogTrace`, notices) are
//! not log lines and are not scanned.

use super::helpers::{
    assert_inventory, blank_comments_and_literals, enclosing_function, is_test_code_file, read_source, relative_path,
    rust_source_files, strip_cfg_test_items, test_module_files,
};

/// The session paths, plus the runtime terminal commit and cleanup every chat
/// turn ends in (where the terminal-write sentinel found a raw error).
const LOG_SCAN_ROOTS: &[&str] = &[
    "src/agent/",
    "src/chat/",
    "src/memory/",
    "src/session_worker/",
    "src/runtime/turn_terminal.rs",
    "src/runtime/turn_cleanup.rs",
];

const LOG_MACROS: &[&str] = &[
    "trace!(",
    "debug!(",
    "info!(",
    "warn!(",
    "error!(",
    "trace_span!(",
    "debug_span!(",
    "info_span!(",
    "warn_span!(",
    "error_span!(",
];

/// Helpers whose result is payload-free by construction, with the reason.
const PAYLOAD_FREE_HELPERS: &[(&str, &str)] = &[
    (
        "error_class",
        "agent::diagnostics: an error's type, byte length and digest, never its text",
    ),
    (
        "text_class",
        "agent::diagnostics: free text's byte length and digest, never the text",
    ),
    (
        "redact_endpoint_userinfo",
        "a configured provider or model label with URL userinfo removed; the label scanner pins it",
    ),
    (
        "redact_provider_text",
        "a configured provider or model label through the one redactor",
    ),
];

/// Names that denote typed metadata wherever they appear as the last name of
/// a logged value (compared case-insensitively).
const METADATA_NAMES: &[&str] = &[
    "id",
    "ids",
    "len",
    "count",
    "seq",
    "sequence",
    "index",
    "idx",
    "kind",
    "generation",
    "revision",
    "rev",
    "epoch",
    "attempt",
    "attempts",
    "tokens",
    "mode",
    "cols",
    "rows",
    "pid",
    "pgid",
    "hits",
    "misses",
    "chars",
    "bytes",
    "sha256",
];

const METADATA_SUFFIXES: &[&str] = &[
    "_id",
    "_ids",
    "_len",
    "_count",
    "_size",
    "_bytes",
    "_chars",
    "_tokens",
    "_total",
    "_sha256",
    "_digest",
    "_hash",
    "_index",
    "_seq",
    "_rev",
    "_revision",
    "_epoch",
    "_generation",
    "_attempt",
    "_attempts",
    "_ms",
    "_secs",
    "_kind",
    "_class",
    "_mode",
    "_limit",
    "_threshold",
    "_events",
    "_rows",
    "_files",
    "_topics",
    "_archives",
    "_seen",
    "_hits",
    "_misses",
    "_tasks",
    "_outcomes",
    "_workers",
    "_turns",
    "_pages",
    "_dimensions",
    "_rc",
    "_active",
    "_changed",
];

const METADATA_PREFIXES: &[&str] = &["is_", "has_"];

/// Accessors that return a number or a kind whatever their receiver is.
const NUMERIC_ACCESSORS: &[&str] = &[
    "len",
    "count",
    "get",
    "kind",
    "as_secs",
    "as_millis",
    "as_micros",
    "as_secs_f64",
];

/// Types an enclosing function may annotate a logged identifier with, with
/// the reason each one carries no free text.
const TYPED_BINDINGS: &[(&str, &str)] = &[
    ("&'static str", "a compile-time label"),
    ("bool", "a flag"),
    ("u8", "a number"),
    ("u16", "a number"),
    ("u32", "a number"),
    ("u64", "a number"),
    ("usize", "a number"),
    ("i32", "a number"),
    ("i64", "a number"),
    ("f64", "a number"),
    ("Option<u64>", "an optional number"),
    ("Option<usize>", "an optional number"),
    ("CompactReason", "fieldless enum (chat::action)"),
    ("KeyModifiers", "crossterm modifier bitflags"),
    ("crossterm::event::KeyModifiers", "crossterm modifier bitflags"),
    (
        "Option<&KeyCode>",
        "a named key only: key_log_class maps every character key to None",
    ),
    (
        "Option<&crossterm::event::KeyCode>",
        "a named key only: key_log_class maps every character key to None",
    ),
];

/// Reviewed log sites whose argument the scanner cannot prove, with the
/// reason. A new entry is an architecture review event.
const REVIEWED_LOG_SITES: &[(&str, &str)] = &[(
    "src/chat/dispatcher.rs::emit_trace `msg`",
    "the reducer's LogTrace sink: every `Effect::LogTrace` message is a `format!` this scanner checks at its construction",
)];

const fn is_ident_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

fn find_chars(haystack: &[char], needle: &str, from: usize) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&start| haystack[start..start + needle.len()] == needle[..])
}

fn text(chars: &[char]) -> String {
    chars.iter().collect()
}

fn normalized(chars: &[char]) -> String {
    text(chars).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The argument ranges of the macro or `format!` call whose `(` is at
/// `open` in the masked production text: split on depth-1 commas, with
/// turbofish angle brackets counted as nesting.
fn argument_ranges(masked: &[char], open: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut depth = 0usize;
    let mut angle = 0usize;
    let mut start = open + 1;
    let mut index = open;
    while index < masked.len() {
        let character = masked[index];
        match character {
            '(' | '[' | '{' => depth += 1,
            ':' if masked.get(index + 1) == Some(&':') && masked.get(index + 2) == Some(&'<') => {
                angle += 1;
                index += 3;
                continue;
            }
            '>' if angle > 0 => angle -= 1,
            ',' if depth == 1 && angle == 0 => {
                ranges.push((start, index));
                start = index + 1;
            }
            ')' | ']' | '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    ranges.push((start, index));
                    break;
                }
            }
            _ => {}
        }
        index += 1;
    }
    ranges
}

fn is_metadata_name(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    METADATA_NAMES.contains(&name.as_str())
        || METADATA_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
        || METADATA_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// Whether `function_text` annotates `name` with one of [`TYPED_BINDINGS`]
/// (`name: Type` as a parameter or a `let`).
fn is_annotated_typed(name: &str, function_text: &str) -> bool {
    let bytes = function_text.as_bytes();
    function_text.match_indices(name).any(|(at, _)| {
        let before_ok = at == 0 || !(is_ident_char(char::from(bytes[at - 1])) || bytes[at - 1] == b'.');
        if !before_ok {
            return false;
        }
        let rest = function_text[at + name.len()..].trim_start();
        let Some(rest) = rest.strip_prefix(':') else {
            return false;
        };
        if rest.starts_with(':') {
            return false;
        }
        let rest = rest.trim_start();
        TYPED_BINDINGS.iter().any(|(type_name, _)| {
            rest.strip_prefix(type_name)
                .is_some_and(|after| !after.starts_with(is_ident_char))
        })
    })
}

fn strip_value_tail(value: &str) -> &str {
    let mut value = value.trim();
    loop {
        let before = value;
        for tail in [
            ".as_deref()",
            ".as_str()",
            ".clone()",
            ".copied()",
            ".unwrap_or_default()",
        ] {
            if let Some(stripped) = value.strip_suffix(tail) {
                value = stripped.trim_end();
            }
        }
        if let Some(open) = value.rfind(".unwrap_or(")
            && value.ends_with(')')
        {
            let inner = value[open + ".unwrap_or(".len()..value.len() - 1].trim();
            if inner.is_empty() || inner == "0" || inner == "false" {
                value = value[..open].trim_end();
            }
        }
        if value == before {
            return value;
        }
    }
}

fn is_path(value: &str) -> bool {
    !value.is_empty()
        && value.split(['.']).all(|part| {
            part.split("::")
                .all(|segment| !segment.is_empty() && segment.chars().all(is_ident_char))
        })
}

/// Whether one logged value (the masked text, literals blanked) is typed
/// metadata.
fn value_is_metadata(masked_value: &str, function_text: &str) -> bool {
    let mut value = masked_value.split_whitespace().collect::<Vec<_>>().join(" ");
    while let Some(stripped) = value.strip_prefix(['%', '?', '&', '*', '!']) {
        value = stripped.trim_start().to_string();
    }
    // A literal, also one turned into a `String` (`"text".to_string()`).
    let literal_conversion = [".to_string()", ".to_owned()", ".into()"].contains(&value.as_str());
    if value.is_empty() || literal_conversion || value == "true" || value == "false" {
        return true;
    }
    if value.starts_with(|character: char| character.is_ascii_digit() || character == '-')
        && value
            .chars()
            .all(|character| is_ident_char(character) || character == '.' || character == '-')
    {
        return true;
    }
    let value = strip_value_tail(&value);
    if value.ends_with(')') {
        let mut depth = 0usize;
        let mut open = None;
        for (at, character) in value.char_indices().rev() {
            match character {
                ')' => depth += 1,
                '(' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        open = Some(at);
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Some(open) = open {
            let mut callee_end = value[..open].trim_end();
            if callee_end.ends_with('>')
                && let Some(turbofish) = callee_end.rfind("::<")
            {
                callee_end = &callee_end[..turbofish];
            }
            let callee: String = callee_end
                .chars()
                .rev()
                .take_while(|character| is_ident_char(*character))
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if PAYLOAD_FREE_HELPERS.iter().any(|(helper, _)| *helper == callee)
                || NUMERIC_ACCESSORS.contains(&callee.as_str())
                || is_metadata_name(&callee)
            {
                return true;
            }
        }
        return false;
    }
    if is_path(value) {
        let last = value.rsplit(['.', ':']).next().unwrap_or(value);
        if is_metadata_name(last) {
            return true;
        }
        if value.chars().all(is_ident_char) && is_annotated_typed(value, function_text) {
            return true;
        }
    }
    false
}

/// The inline captures of a format string's raw text (`{name}`, `{name:?}`),
/// by name; positional placeholders are skipped.
fn format_captures(raw_literal: &str) -> Vec<String> {
    let characters: Vec<char> = raw_literal.chars().collect();
    let mut captures = Vec::new();
    let mut index = 0usize;
    while index < characters.len() {
        if characters[index] == '{' {
            if characters.get(index + 1) == Some(&'{') {
                index += 2;
                continue;
            }
            let close = (index + 1..characters.len()).find(|&at| characters[at] == '}');
            if let Some(close) = close {
                let inner = text(&characters[index + 1..close]);
                let name = inner.split(':').next().unwrap_or("").trim().to_string();
                if !name.is_empty() && !name.chars().all(|character| character.is_ascii_digit()) {
                    captures.push(name);
                }
                index = close + 1;
                continue;
            }
        } else if characters[index] == '}' && characters.get(index + 1) == Some(&'}') {
            index += 2;
            continue;
        }
        index += 1;
    }
    captures
}

/// The non-metadata arguments of one call whose `(` is at `open`.
fn call_violations(raw: &[char], masked: &[char], open: usize) -> Vec<(usize, String)> {
    let function_start = (0..open)
        .rev()
        .find(|&at| masked[at..].starts_with(&['f', 'n', ' ']) && (at == 0 || !is_ident_char(masked[at - 1])))
        .unwrap_or(0);
    let function_text = text(&raw[function_start..open]);
    let mut violations = Vec::new();
    let mut format_seen = false;
    for (start, end) in argument_ranges(masked, open) {
        let masked_segment = &masked[start..end];
        let raw_segment = text(&raw[start..end]);
        let shown = normalized(masked_segment);
        if shown.is_empty() {
            let raw_trimmed = raw_segment.trim();
            if raw_trimmed.is_empty() || format_seen {
                continue;
            }
            format_seen = true;
            for capture in format_captures(raw_trimmed) {
                if !(is_metadata_name(&capture) || is_annotated_typed(&capture, &function_text)) {
                    violations.push((start, format!("{{{capture}}}")));
                }
            }
            continue;
        }
        let label_end = shown.find(':');
        if let Some(colon) = label_end
            && !shown[colon + 1..].starts_with(':')
            && ["target", "parent", "name"].contains(&shown[..colon].trim())
        {
            continue;
        }
        let assignment = shown.char_indices().find(|&(at, character)| {
            character == '=' && !shown[at + 1..].starts_with('=') && !shown[..at].ends_with(['=', '!', '<', '>'])
        });
        let value = match assignment {
            Some((at, _))
                if shown[..at]
                    .trim()
                    .chars()
                    .all(|character| is_ident_char(character) || character == '.' || character == ' ') =>
            {
                shown[at + 1..].trim().to_string()
            }
            _ => shown.clone(),
        };
        if !value_is_metadata(&value, &function_text) {
            violations.push((start, shown));
        }
    }
    violations
}

/// The non-metadata log arguments of one production file.
fn payload_capable_log_arguments(relative: &str, raw_source: &str) -> Vec<String> {
    let production = strip_cfg_test_items(&blank_comments_and_literals(raw_source));
    let raw: Vec<char> = raw_source.chars().collect();
    let masked: Vec<char> = production.chars().collect();
    if raw.len() != masked.len() {
        return Vec::new();
    }
    let lines: Vec<&str> = production.lines().collect();
    let mut found: Vec<(usize, String)> = Vec::new();
    for macro_name in LOG_MACROS {
        let mut from = 0usize;
        while let Some(at) = find_chars(&masked, macro_name, from) {
            from = at + macro_name.len();
            if at > 0 && is_ident_char(masked[at - 1]) {
                continue;
            }
            found.extend(call_violations(&raw, &masked, at + macro_name.len() - 1));
        }
    }
    let mut from = 0usize;
    while let Some(at) = find_chars(&masked, "Effect::LogTrace {", from) {
        from = at + 1;
        let open = at + "Effect::LogTrace ".len();
        let Some(&(_, close)) = argument_ranges(&masked, open).last() else {
            continue;
        };
        let Some(msg) = find_chars(&masked[..close], "msg:", open) else {
            continue;
        };
        let value_start = msg + "msg:".len();
        let value = normalized(&masked[value_start..close]);
        let value = value.trim_end_matches(',').trim();
        if value.starts_with("format!(") {
            let format_open =
                find_chars(&masked, "format!(", value_start).map_or(value_start, |at| at + "format".len() + 1);
            found.extend(call_violations(&raw, &masked, format_open));
        } else if !value_is_metadata(value, "") {
            found.push((value_start, format!("msg = {value}")));
        }
    }
    found.sort();
    found
        .into_iter()
        .map(|(offset, argument)| {
            let line_index = masked[..offset].iter().filter(|character| **character == '\n').count();
            format!(
                "{relative}::{} `{argument}`",
                enclosing_function(&lines, line_index.min(lines.len().saturating_sub(1)))
            )
        })
        .collect()
}

/// I-LOG: every log argument in the session paths is typed metadata.
///
/// MUTATION GUARD: put `error = %error` back into any `tracing` call in
/// `src/chat/provider_turns.rs` (r6-4) and the inventory gains it.
#[test]
pub(super) fn log_lines_carry_only_typed_metadata() {
    let modules = test_module_files();
    let mut actual = std::collections::BTreeSet::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !LOG_SCAN_ROOTS.iter().any(|root| relative.starts_with(root)) {
            continue;
        }
        let raw = read_source(&path);
        if is_test_code_file(&relative, &raw, &modules) {
            continue;
        }
        actual.extend(payload_capable_log_arguments(&relative, &raw));
    }
    let reviewed: Vec<&str> = REVIEWED_LOG_SITES.iter().map(|(site, _)| *site).collect();
    assert_inventory(
        "log arguments that can carry free-form text (use agent::diagnostics::error_class / text_class)",
        actual.into_iter().collect(),
        &reviewed,
    );
}

/// The argument classifier behind `log_lines_carry_only_typed_metadata`
/// rejects every free-form form and accepts every typed one.
#[test]
fn payload_capable_log_arguments_sees_every_form() {
    let source = r#"fn f(error: anyhow::Error, reason: &'static str, name: &str) {
    tracing::warn!(error = %error, "a");
    tracing::warn!(%error, "b");
    tracing::warn!(error = ?err, "c");
    tracing::warn!(?e, "d");
    tracing::warn!("e {e}");
    tracing::warn!("f {err@DEBUG_SPEC}");
    tracing::warn!("g {}", error);
    tracing::warn!(detail = error.to_string(), "h");
    tracing::warn!(tool = %name, "i");
    tracing::warn!(name, "j");
    tracing::warn!(detail = "static".to_string(), "literal");
    tracing::warn!("k {name}");
    tracing::warn!(
        error = %crate::agent::diagnostics::error_class(&error),
        tool = %crate::agent::diagnostics::text_class(name),
        reason,
        task_id = id.get(),
        turns_count = s.turn_count(),
        history_len = history.len(),
        is_terminal,
        kind = ?mode,
        sha = digest_sha256.as_deref().unwrap_or("none"),
        "ok {reason} {}", history.len()
    );
}
fn g() -> Vec<Effect> {
    vec![Effect::LogTrace { level: tracing::Level::DEBUG, msg: format!("x {name}") }]
}
"#;
    // The debug spec is spliced in so the fixture is not itself a format string.
    let source = source.replace("@DEBUG_SPEC", concat!(":", "?"));
    let found = payload_capable_log_arguments("src/chat/x.rs", &source);
    assert_eq!(
        found,
        vec![
            "src/chat/x.rs::f `error = %error`".to_string(),
            "src/chat/x.rs::f `%error`".to_string(),
            "src/chat/x.rs::f `error = ?err`".to_string(),
            "src/chat/x.rs::f `?e`".to_string(),
            "src/chat/x.rs::f `{e}`".to_string(),
            "src/chat/x.rs::f `{err}`".to_string(),
            "src/chat/x.rs::f `error`".to_string(),
            "src/chat/x.rs::f `detail = error.to_string()`".to_string(),
            "src/chat/x.rs::f `tool = %name`".to_string(),
            "src/chat/x.rs::f `name`".to_string(),
            "src/chat/x.rs::f `{name}`".to_string(),
            "src/chat/x.rs::g `{name}`".to_string(),
        ],
        "each free-form form must be reported once, and no typed form may be"
    );
}

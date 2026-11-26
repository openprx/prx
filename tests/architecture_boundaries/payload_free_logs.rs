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
//! Permitted forms (after an optional `%`, `?`, `&`, `*` or `!`). A name is
//! never evidence: `tool_kind`, `session_id` or `x_len` pass only when the
//! declarations say what they are.
//! - a literal (number, boolean or string literal), or a cast to a scalar;
//! - a call to a payload-free helper ([`PAYLOAD_FREE_HELPERS`]): errors and
//!   free text are logged as their class (type, byte length, digest) only;
//! - configuration metadata built through a typed constructor
//!   ([`PLAIN_LABEL_CONSTRUCTORS`]): a registered tool name (`ToolName`), a
//!   provider kind or model id through the provider-label redactor
//!   (`ProviderLabel::redacted`);
//! - a value whose declared type is typed ([`TypeIndex`] reads every
//!   production struct field, method and function return type, constant,
//!   fieldless enum and struct of typed fields in `src/`): a scalar
//!   ([`SCALAR_TYPES`], which has no `char`: a character is text), a reviewed
//!   type ([`REVIEWED_TYPES`]), a typed-by-construction type, or a reference,
//!   `Option` or tuple of them. An identifier's type comes from its binding
//!   in scope: a parameter or `let` annotation, a typed initializer, a
//!   destructured digest helper ([`DIGEST_HELPERS`]), a struct or annotated
//!   tuple pattern, or `Some(x)`/`Ok(x)` of a typed initializer. A binding
//!   with none of these (a closure or match binding) is not typed. A field's
//!   type is its owner's declaration (`self` is the enclosing `impl`); a
//!   call's type is the receiver type's method, or every declaration of the
//!   name (so `backend_kind()` is the `MemoryBackendKind` it is declared as);
//! - an accessor on a proven receiver only: `len`/`is_empty`/`count` on a
//!   std collection, string or iterator, or a production type whose own
//!   method is declared typed; `get` on a collection of typed elements, a
//!   `Cell` or `NonZero*` of a number, or a type whose `get` is declared
//!   typed; `Duration` accessors on a duration; a fully qualified call
//!   (`TurnTaskId::get(id)`, `Vec::len(&turns)`), whose receiver the
//!   compiler proves. `args.get("password")` on a JSON value is rejected.
//!
//! The format string's inline captures (`{name}`, `{name:?}`) are judged the
//! same way by name; its positional arguments are arguments like any other.
//! User-visible messages (`bail!`, `format!` outside `LogTrace`, notices) are
//! not log lines and are not scanned.

use std::collections::{BTreeMap, BTreeSet};

use super::helpers::{
    assert_inventory, blank_comments_and_literals, enclosing_function, is_test_code_file, production_source_files,
    read_source, relative_path, strip_cfg_test_items, test_module_files,
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

/// Typed constructors of configuration metadata that is logged in plain,
/// matched on the callee path, with the reason.
const PLAIN_LABEL_CONSTRUCTORS: &[(&str, &str)] = &[
    (
        "ToolName::spec",
        "agent::diagnostics: a registered tool's name, from the spec the registry advertises",
    ),
    (
        "ToolName::descriptor",
        "agent::diagnostics: a resolved tool descriptor's public name",
    ),
    (
        "ProviderLabel::redacted",
        "agent::diagnostics: a provider kind or model id through the provider-label redactor",
    ),
];

/// Scalar types: a number, a flag, a duration or a compile-time label.
const SCALAR_TYPES: &[&str] = &[
    "bool",
    "u8",
    "u16",
    "u32",
    "u64",
    "u128",
    "usize",
    "i8",
    "i16",
    "i32",
    "i64",
    "i128",
    "isize",
    "f32",
    "f64",
    "Duration",
    "NonZeroU32",
    "NonZeroU64",
    "NonZeroUsize",
];

/// Types the scanner cannot classify from their declaration (it reads
/// fieldless enums and structs of typed fields on its own), each reviewed to
/// carry no free text, with the reason.
const REVIEWED_TYPES: &[(&str, &str)] = &[
    (
        "ProviderLabel",
        "agent::diagnostics: a provider kind or model id built only through the provider-label redactor",
    ),
    (
        "ToolName",
        "agent::diagnostics: a registered tool name built only from a tool spec or descriptor",
    ),
    (
        "LogClass",
        "agent::diagnostics: an error or text's static kind, byte length and digest",
    ),
    ("KeyModifiers", "crossterm modifier bitflags"),
    ("KeyEventKind", "crossterm press, repeat or release"),
    ("Level", "a tracing level"),
    (
        "KeyLogClass",
        "a key's loggable class (chat::tui::keys): `Char` and `Modifier` carry no payload and the named variant holds only a static name",
    ),
];

/// Helpers whose returned tuple is a length and a digest, never text: every
/// name a `let` destructures from one of them is typed.
const DIGEST_HELPERS: &[(&str, &str)] = &[
    (
        "error_log_digest",
        "agent::diagnostics: an error's byte length and SHA-256 prefix",
    ),
    (
        "tool_parse_log_digest",
        "agent::diagnostics: free text's byte length and SHA-256 prefix",
    ),
];

/// Accessors whose value is a count on any std collection, string or
/// iterator; allowed only on a receiver proven to be one.
const COUNT_ACCESSORS: &[&str] = &["len", "is_empty", "count", "capacity"];

/// std predicates of `Option` and `Result`: a flag whatever the receiver is
/// (unless a production declaration of the same name says otherwise).
const FLAG_PREDICATES: &[&str] = &["is_some", "is_none", "is_ok", "is_err", "is_panic", "is_cancelled"];

/// `Duration` accessors; allowed only on a receiver proven to be a duration.
const DURATION_ACCESSORS: &[&str] = &[
    "as_secs",
    "as_millis",
    "as_micros",
    "as_nanos",
    "as_secs_f32",
    "as_secs_f64",
    "subsec_millis",
];

/// Calls whose value is a `Duration` whatever the receiver is.
const DURATION_SOURCES: &[&str] = &["elapsed", "duration_since", "saturating_duration_since"];

/// Integer arithmetic that keeps its receiver's type.
const NUMERIC_METHODS: &[&str] = &[
    "saturating_add",
    "saturating_sub",
    "saturating_mul",
    "wrapping_add",
    "wrapping_sub",
    "checked_add",
    "checked_sub",
    "min",
    "max",
    "abs",
    "pow",
    "clamp",
];

/// std iterator constructors and adapters: a call ending in one of them is
/// an iterator, whose `count()` is a number.
const ITERATOR_CALLS: &[&str] = &[
    "iter",
    "iter_mut",
    "into_iter",
    "chars",
    "bytes",
    "lines",
    "keys",
    "values",
    "split_whitespace",
    "filter",
    "filter_map",
    "map",
    "flat_map",
    "enumerate",
    "take",
    "skip",
    "rev",
    "take_while",
    "skip_while",
    "zip",
    "chain",
];

/// Collections whose `len()`/`is_empty()` is a count, with the generic
/// argument that is their element (the value for a map).
const COLLECTION_TYPES: &[(&str, usize)] = &[
    ("Vec", 0),
    ("VecDeque", 0),
    ("HashSet", 0),
    ("BTreeSet", 0),
    ("IndexSet", 0),
    ("HashMap", 1),
    ("BTreeMap", 1),
    ("IndexMap", 1),
];

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

fn normalized_str(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The index of the bracket that closes the one at `open` (any of `(`, `[`,
/// `{`, `<`), counting every bracket kind; `->` is not a closing angle.
fn matching_close(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut index = open;
    while index < chars.len() {
        match chars[index] {
            '(' | '[' | '{' | '<' => depth += 1,
            '>' if index > 0 && chars[index - 1] == '-' => {}
            ')' | ']' | '}' | '>' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// `text` split on top-level commas (outside every bracket kind).
fn split_top_level(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, character) in chars.iter().enumerate() {
        match character {
            '(' | '[' | '{' | '<' => depth += 1,
            '>' if index > 0 && chars[index - 1] == '-' => {}
            ')' | ']' | '}' | '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(chars[start..index].iter().collect());
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(chars[start..].iter().collect());
    parts
}

const fn skip_whitespace(chars: &[char], mut index: usize) -> usize {
    while index < chars.len() && chars[index].is_whitespace() {
        index += 1;
    }
    index
}

fn ident_at(chars: &[char], from: usize) -> (String, usize) {
    let mut end = from;
    while end < chars.len() && is_ident_char(chars[end]) {
        end += 1;
    }
    (text(&chars[from..end]), end)
}

/// Whether `word` stands as a whole identifier at `at`.
fn is_word_at(chars: &[char], at: usize, word: &str) -> bool {
    let word: Vec<char> = word.chars().collect();
    chars.get(at..at + word.len()) == Some(&word[..])
        && (at == 0 || !is_ident_char(chars[at - 1]))
        && chars
            .get(at + word.len())
            .is_none_or(|character| !is_ident_char(*character))
}

/// The type that starts at `start` (an annotation after its `:`), up to the
/// first `,`, `)`, `=`, `;`, `{` or `|` outside its own brackets.
fn take_type(chars: &[char], start: usize) -> String {
    let mut depth = 0usize;
    let mut end = start;
    while end < chars.len() {
        match chars[end] {
            '(' | '[' | '<' => depth += 1,
            '>' if end > 0 && chars[end - 1] == '-' => {}
            ')' | ']' | '>' if depth > 0 => depth -= 1,
            ',' | ')' | '=' | ';' | '{' | '|' if depth == 0 => break,
            _ => {}
        }
        end += 1;
    }
    normalized(&chars[start..end])
}

/// Every position where `word` stands as a whole identifier.
fn word_positions(chars: &[char], word: &str) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut from = 0usize;
    while let Some(at) = find_chars(chars, word, from) {
        from = at + 1;
        let before_ok = at == 0 || !is_ident_char(chars[at - 1]);
        let after_ok = chars
            .get(at + word.len())
            .is_none_or(|character| !is_ident_char(*character));
        if before_ok && after_ok {
            positions.push(at);
        }
    }
    positions
}

/// A declaration fragment without its leading attributes and visibility.
fn strip_attributes_and_visibility(fragment: &str) -> String {
    let mut rest = fragment.trim();
    loop {
        if rest.starts_with("#[") {
            let chars: Vec<char> = rest.chars().collect();
            let Some(close) = matching_close(&chars, 1) else {
                return String::new();
            };
            rest = rest[chars[..=close]
                .iter()
                .map(|character| character.len_utf8())
                .sum::<usize>()..]
                .trim_start();
            continue;
        }
        if let Some(after) = rest.strip_prefix("pub")
            && !after.starts_with(is_ident_char)
        {
            let after = after.trim_start();
            if after.starts_with('(') {
                let chars: Vec<char> = after.chars().collect();
                let Some(close) = matching_close(&chars, 0) else {
                    return String::new();
                };
                rest = after[chars[..=close]
                    .iter()
                    .map(|character| character.len_utf8())
                    .sum::<usize>()..]
                    .trim_start();
            } else {
                rest = after;
            }
            continue;
        }
        return rest.to_string();
    }
}

/// Declarations the scanner reads type evidence from, over every production
/// source in `src/`: struct fields, function return types, constants, and
/// the types that are typed by construction (fieldless enums, and structs
/// whose every field is typed).
#[derive(Debug, Default)]
pub(super) struct TypeIndex {
    fields: BTreeMap<String, Vec<String>>,
    returns: BTreeMap<String, Vec<String>>,
    /// Return types of the functions that take `self`, by name: the only
    /// candidates for a method call `value.name()` on an unknown receiver.
    receiver_returns: BTreeMap<String, Vec<String>>,
    /// Field types of one production struct, by `(struct name, field name)`.
    struct_fields: BTreeMap<(String, String), Vec<String>>,
    /// Return types of the methods of one production type (`impl` and
    /// `trait` blocks), by `(type name, method name)`.
    methods: BTreeMap<(String, String), Vec<String>>,
    consts: BTreeMap<String, Vec<String>>,
    typed_names: BTreeSet<String>,
}

impl TypeIndex {
    /// The index of masked production texts (comments and literals blanked,
    /// `#[cfg(test)]` items removed).
    pub(super) fn of_masked_sources<'a>(sources: impl IntoIterator<Item = &'a str>) -> Self {
        let mut index = Self::default();
        let mut struct_fields: BTreeMap<String, Vec<Vec<String>>> = BTreeMap::new();
        let mut fieldless_enums: BTreeSet<String> = BTreeSet::new();
        let mut other_enums: BTreeSet<String> = BTreeSet::new();
        for source in sources {
            let chars: Vec<char> = source.chars().collect();
            index.read_structs(&chars, &mut struct_fields);
            read_enums(&chars, &mut fieldless_enums, &mut other_enums);
            index.read_functions(&chars);
            index.read_consts(&chars);
        }
        index.typed_names = fieldless_enums.difference(&other_enums).cloned().collect();
        // Structs of typed fields, to a fixed point (a struct may hold another).
        loop {
            let newly: Vec<String> = struct_fields
                .iter()
                .filter(|(name, _)| !index.typed_names.contains(*name))
                .filter(|(_, definitions)| {
                    definitions
                        .iter()
                        .all(|fields| fields.iter().all(|field| index.is_typed_type(field)))
                })
                .map(|(name, _)| name.clone())
                .collect();
            if newly.is_empty() {
                break;
            }
            index.typed_names.extend(newly);
        }
        index
    }

    fn read_structs(&mut self, chars: &[char], struct_fields: &mut BTreeMap<String, Vec<Vec<String>>>) {
        for at in word_positions(chars, "struct") {
            let name_start = skip_whitespace(chars, at + "struct".len());
            let (name, mut cursor) = ident_at(chars, name_start);
            if name.is_empty() {
                continue;
            }
            cursor = skip_whitespace(chars, cursor);
            if chars.get(cursor) == Some(&'<') {
                let Some(close) = matching_close(chars, cursor) else {
                    continue;
                };
                cursor = skip_whitespace(chars, close + 1);
            }
            let mut field_types = Vec::new();
            match chars.get(cursor) {
                Some('(') => {
                    let Some(close) = matching_close(chars, cursor) else {
                        continue;
                    };
                    for field in split_top_level(&text(&chars[cursor + 1..close])) {
                        let field = strip_attributes_and_visibility(&field);
                        if !field.is_empty() {
                            field_types.push(normalized_str(&field));
                        }
                    }
                }
                Some('{') => {
                    let Some(close) = matching_close(chars, cursor) else {
                        continue;
                    };
                    for field in split_top_level(&text(&chars[cursor + 1..close])) {
                        let field = strip_attributes_and_visibility(&field);
                        let Some((field_name, field_type)) = field.split_once(':') else {
                            continue;
                        };
                        let field_type = normalized_str(field_type);
                        self.fields
                            .entry(field_name.trim().to_string())
                            .or_default()
                            .push(field_type.clone());
                        self.struct_fields
                            .entry((name.clone(), field_name.trim().to_string()))
                            .or_default()
                            .push(field_type.clone());
                        field_types.push(field_type);
                    }
                }
                Some(';') => {}
                _ => continue,
            }
            struct_fields.entry(name).or_default().push(field_types);
        }
    }

    fn read_functions(&mut self, chars: &[char]) {
        let owners = impl_blocks(chars);
        for at in word_positions(chars, "fn") {
            let name_start = skip_whitespace(chars, at + 2);
            let (name, mut cursor) = ident_at(chars, name_start);
            if name.is_empty() {
                continue;
            }
            cursor = skip_whitespace(chars, cursor);
            if chars.get(cursor) == Some(&'<') {
                let Some(close) = matching_close(chars, cursor) else {
                    continue;
                };
                cursor = skip_whitespace(chars, close + 1);
            }
            if chars.get(cursor) != Some(&'(') {
                continue;
            }
            let Some(close) = matching_close(chars, cursor) else {
                continue;
            };
            let takes_self = takes_self(&text(&chars[cursor + 1..close]));
            cursor = skip_whitespace(chars, close + 1);
            let return_type = if chars.get(cursor) == Some(&'-') && chars.get(cursor + 1) == Some(&'>') {
                let start = cursor + 2;
                let mut end = start;
                let mut depth = 0usize;
                while end < chars.len() {
                    match chars[end] {
                        '(' | '[' | '<' => depth += 1,
                        '>' if chars[end - 1] == '-' => {}
                        ')' | ']' | '>' => depth = depth.saturating_sub(1),
                        '{' | ';' if depth == 0 => break,
                        'w' if depth == 0 && is_word_at(chars, end, "where") => break,
                        _ => {}
                    }
                    end += 1;
                }
                normalized(&chars[start..end])
            } else {
                "()".to_string()
            };
            if let Some((owner, _, _)) = owners
                .iter()
                .filter(|(_, start, end)| *start < at && at < *end)
                .min_by_key(|(_, start, end)| end - start)
            {
                self.methods
                    .entry((owner.clone(), name.clone()))
                    .or_default()
                    .push(return_type.clone());
            }
            if takes_self {
                self.receiver_returns
                    .entry(name.clone())
                    .or_default()
                    .push(return_type.clone());
            }
            self.returns.entry(name).or_default().push(return_type);
        }
    }

    /// Every declared return type of a call to `callee`. A method call
    /// (`value.callee()`) cannot reach an associated function without `self`.
    fn call_returns(&self, callee: &str, method_call: bool) -> Option<&Vec<String>> {
        if method_call {
            self.receiver_returns.get(callee)
        } else {
            self.returns.get(callee)
        }
    }

    /// The declared return types of `method` on the production type a
    /// receiver is declared as (empty when the type declares no such method).
    fn method_returns(&self, receiver_type: &str, method: &str) -> Vec<String> {
        let mut declared = normalized_str(receiver_type);
        while let Some(rest) = declared.strip_prefix('&') {
            let rest = rest.trim_start();
            declared = strip_lifetime(rest.strip_prefix("mut ").unwrap_or(rest)).to_string();
        }
        self.methods
            .get(&(type_name(&declared).to_string(), method.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    fn read_consts(&mut self, chars: &[char]) {
        for keyword in ["const", "static"] {
            for at in word_positions(chars, keyword) {
                let mut cursor = skip_whitespace(chars, at + keyword.len());
                let (mut name, mut end) = ident_at(chars, cursor);
                if name == "mut" {
                    cursor = skip_whitespace(chars, end);
                    (name, end) = ident_at(chars, cursor);
                }
                if name.is_empty() || name == "fn" || name == "_" {
                    continue;
                }
                let colon = skip_whitespace(chars, end);
                if chars.get(colon) != Some(&':') || chars.get(colon + 1) == Some(&':') {
                    continue;
                }
                let Some(equals) = (colon + 1..chars.len()).find(|&index| chars[index] == '=' || chars[index] == ';')
                else {
                    continue;
                };
                self.consts
                    .entry(name)
                    .or_default()
                    .push(normalized(&chars[colon + 1..equals]));
            }
        }
    }

    /// Whether a declared type carries no free text: a scalar, a reviewed or
    /// typed-by-construction type, or a reference, `Option` or tuple of them.
    fn is_typed_type(&self, raw: &str) -> bool {
        let declared = normalized_str(raw);
        let declared = declared.trim();
        if declared == "&'static str" || declared == "()" {
            return true;
        }
        if let Some(rest) = declared.strip_prefix('&') {
            let rest = rest.trim_start();
            let rest = rest.strip_prefix("mut ").unwrap_or(rest);
            let rest = strip_lifetime(rest);
            return rest != "str" && self.is_typed_type(rest);
        }
        if let Some(arguments) = generic_arguments(declared, "Option") {
            return arguments.len() == 1 && self.is_typed_type(&arguments[0]);
        }
        if declared.starts_with('(') && declared.ends_with(')') {
            return split_top_level(&declared[1..declared.len() - 1])
                .iter()
                .all(|part| part.trim().is_empty() || self.is_typed_type(part));
        }
        let name = type_name(declared);
        SCALAR_TYPES.contains(&name)
            || REVIEWED_TYPES.iter().any(|(reviewed, _)| *reviewed == name)
            || self.typed_names.contains(name)
    }
}

/// Every `impl` and `trait` block: the type (or trait) it belongs to and the
/// character range of its body.
fn impl_blocks(chars: &[char]) -> Vec<(String, usize, usize)> {
    let mut blocks = Vec::new();
    for keyword in ["impl", "trait"] {
        for at in word_positions(chars, keyword) {
            let mut cursor = skip_whitespace(chars, at + keyword.len());
            if chars.get(cursor) == Some(&'<') {
                let Some(close) = matching_close(chars, cursor) else {
                    continue;
                };
                cursor = close + 1;
            }
            let mut depth = 0usize;
            let mut open = None;
            let mut index = cursor;
            while index < chars.len() {
                match chars[index] {
                    '(' | '[' | '<' => depth += 1,
                    '>' if chars[index - 1] == '-' => {}
                    ')' | ']' | '>' => depth = depth.saturating_sub(1),
                    '{' if depth == 0 => {
                        open = Some(index);
                        break;
                    }
                    ';' if depth == 0 => break,
                    _ => {}
                }
                index += 1;
            }
            let Some(open) = open else {
                continue;
            };
            let Some(close) = matching_close(chars, open) else {
                continue;
            };
            let header = normalized(&chars[cursor..open]);
            let header = header.split(" where ").next().unwrap_or(&header);
            let target = header.rsplit(" for ").next().unwrap_or(header);
            let target = target.split(':').next().unwrap_or(target).trim();
            let name = type_name(target.trim_start_matches(['&', ' '])).to_string();
            if !name.is_empty() {
                blocks.push((name, open, close));
            }
        }
    }
    blocks
}

fn read_enums(chars: &[char], fieldless: &mut BTreeSet<String>, other: &mut BTreeSet<String>) {
    for at in word_positions(chars, "enum") {
        let name_start = skip_whitespace(chars, at + "enum".len());
        let (name, mut cursor) = ident_at(chars, name_start);
        if name.is_empty() {
            continue;
        }
        cursor = skip_whitespace(chars, cursor);
        if chars.get(cursor) == Some(&'<') {
            other.insert(name);
            continue;
        }
        if chars.get(cursor) != Some(&'{') {
            continue;
        }
        let Some(close) = matching_close(chars, cursor) else {
            continue;
        };
        let variants: Vec<String> = split_top_level(&text(&chars[cursor + 1..close]))
            .iter()
            .map(|variant| strip_attributes_and_visibility(variant))
            .filter(|variant| !variant.is_empty())
            .collect();
        if !variants.is_empty() && variants.iter().all(|variant| !variant.contains(['(', '{'])) {
            fieldless.insert(name);
        } else {
            other.insert(name);
        }
    }
}

/// `T` of a declared `Option<T>` or `Result<T, E>` (references removed).
fn unwrap_option_or_result(declared: &str) -> Option<String> {
    let declared = strip_references(declared);
    generic_arguments(declared, "Option")
        .or_else(|| generic_arguments(declared, "Result"))
        .and_then(|arguments| arguments.first().map(|argument| argument.trim().to_string()))
}

/// A type without its leading references and lifetimes.
fn strip_references(declared: &str) -> &str {
    let mut rest = declared.trim();
    while let Some(after) = rest.strip_prefix('&') {
        let after = after.trim_start();
        rest = strip_lifetime(after.strip_prefix("mut ").unwrap_or(after));
    }
    rest
}

/// Whether a parameter list starts with a `self` receiver (`self`,
/// `mut self`, `&self`, `&'a mut self`, `self: Arc<Self>`).
fn takes_self(parameters: &str) -> bool {
    let first = parameters.trim_start();
    let first = first
        .strip_prefix('&')
        .map_or(first, |rest| strip_lifetime(rest.trim_start()));
    let first = first.strip_prefix("mut ").map_or(first, str::trim_start);
    first
        .strip_prefix("self")
        .is_some_and(|rest| !rest.starts_with(is_ident_char))
}

fn strip_lifetime(declared: &str) -> &str {
    declared.strip_prefix('\'').map_or(declared, |after| {
        after
            .find(|character: char| !is_ident_char(character))
            .map_or("", |end| after[end..].trim_start())
    })
}

/// The last path segment of a type, without its generic arguments.
fn type_name(declared: &str) -> &str {
    let base = declared.split('<').next().unwrap_or(declared).trim();
    base.rsplit("::").next().unwrap_or(base).trim()
}

/// The generic arguments of `declared` when its type name is `name`.
fn generic_arguments(declared: &str, name: &str) -> Option<Vec<String>> {
    if type_name(declared) != name {
        return None;
    }
    let open = declared.find('<')?;
    let inner = declared.get(open + 1..declared.len().checked_sub(1)?)?;
    declared.ends_with('>').then(|| split_top_level(inner))
}

/// The element type of a collection, string or slice (`str` for a string,
/// whose `get` returns text).
fn collection_element(declared: &str) -> Option<String> {
    let declared = normalized_str(declared);
    let mut rest = declared.trim();
    loop {
        if let Some(after) = rest.strip_prefix('&') {
            let after = after.trim_start();
            rest = strip_lifetime(after.strip_prefix("mut ").unwrap_or(after));
            continue;
        }
        let wrapped = ["Box", "Arc", "Rc"]
            .iter()
            .find_map(|wrapper| generic_arguments(rest, wrapper).filter(|arguments| arguments.len() == 1));
        match wrapped {
            Some(arguments) => {
                let inner = arguments[0].trim().to_string();
                return collection_element(&inner);
            }
            None => break,
        }
    }
    if rest == "str" || type_name(rest) == "String" {
        return Some("str".to_string());
    }
    if rest.starts_with('[') && rest.ends_with(']') {
        let inner = &rest[1..rest.len() - 1];
        return Some(inner.split(';').next().unwrap_or(inner).trim().to_string());
    }
    COLLECTION_TYPES.iter().find_map(|(name, element)| {
        generic_arguments(rest, name)
            .and_then(|arguments| arguments.get(*element).map(|element| element.trim().to_string()))
    })
}

/// The function a logged value sits in: its masked text up to the log call,
/// and its signature (up to the body's opening brace).
struct Scope<'a> {
    function: &'a str,
    signature: &'a str,
    /// The type of `self`: the `impl` block the log call sits in.
    self_type: Option<&'a str>,
    index: &'a TypeIndex,
}

impl Scope<'_> {
    /// The declared type of `name` in a destructuring `let` pattern: a
    /// struct pattern (`Type { name, .. }`, the struct's field type) or an
    /// annotated tuple (`(a, name): (A, B)`, the type at its position).
    fn pattern_binding_type(&self, pattern: &str, name: &str) -> Option<String> {
        let pattern = pattern.trim();
        if let Some((bound, declared)) = pattern.split_once(':')
            && let Some(bound) = bound.trim().strip_prefix('(').and_then(|rest| rest.strip_suffix(')'))
        {
            let declared = declared.trim();
            let declared = declared.strip_prefix('(')?.strip_suffix(')')?;
            let names = split_top_level(bound);
            let types = split_top_level(declared);
            let position = names
                .iter()
                .position(|bound| bound.trim().trim_start_matches("mut ").trim() == name)?;
            return types.get(position).map(|declared| normalized_str(declared));
        }
        let open = pattern.find('{')?;
        let owner = type_name(pattern[..open].trim()).to_string();
        let fields = pattern[open + 1..].trim_end().strip_suffix('}')?;
        split_top_level(fields)
            .iter()
            .any(|field| field.trim().trim_start_matches("mut ").trim() == name)
            .then(|| self.index.struct_fields.get(&(owner, name.to_string())))
            .flatten()
            .and_then(|declared| (declared.len() == 1).then(|| declared[0].clone()))
    }

    /// The types `name` is declared with as a parameter of this function.
    fn parameter_types(&self, name: &str) -> Vec<String> {
        let mut found = Vec::new();
        let signature: Vec<char> = self.signature.chars().collect();
        for at in word_positions(&signature, name) {
            let colon = skip_whitespace(&signature, at + name.chars().count());
            if signature.get(colon) == Some(&':') && signature.get(colon + 1) != Some(&':') {
                found.push(take_type(&signature, colon + 1));
            }
        }
        found
    }

    /// Every `let <pattern> = <init>;` in this function, as text.
    fn let_bindings(&self) -> Vec<(String, String)> {
        let chars: Vec<char> = self.function.chars().collect();
        let mut bindings = Vec::new();
        for at in word_positions(&chars, "let") {
            let start = at + 3;
            let mut depth = 0usize;
            let mut equals = None;
            let mut index = start;
            while index < chars.len() {
                match chars[index] {
                    '(' | '[' | '{' | '<' => depth += 1,
                    ')' | ']' | '}' | '>' => depth = depth.saturating_sub(1),
                    '=' if depth == 0
                        && chars.get(index + 1) != Some(&'=')
                        && chars.get(index + 1) != Some(&'>')
                        && !matches!(chars.get(index.wrapping_sub(1)), Some('=' | '!' | '<' | '>')) =>
                    {
                        equals = Some(index);
                        break;
                    }
                    ';' => break,
                    _ => {}
                }
                index += 1;
            }
            let Some(equals) = equals else {
                continue;
            };
            // A conditional `let` (`if let`, `while let`, a `&&` chain) ends
            // at its block or the next condition; a `let .. else` at `else`.
            let before = text(&chars[..at]);
            let before = before.trim_end();
            let conditional = before.ends_with("&&")
                || [" if", " while", "(if", "(while"]
                    .iter()
                    .any(|keyword| before.ends_with(keyword))
                || before == "if"
                || before == "while";
            let mut end = equals + 1;
            let mut depth = 0usize;
            while end < chars.len() {
                match chars[end] {
                    '{' if depth == 0 && conditional => break,
                    '&' if depth == 0 && conditional && chars.get(end + 1) == Some(&'&') => break,
                    'e' if depth == 0 && is_word_at(&chars, end, "else") => break,
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' => depth = depth.saturating_sub(1),
                    ';' if depth == 0 => break,
                    _ => {}
                }
                end += 1;
            }
            bindings.push((normalized(&chars[start..equals]), normalized(&chars[equals + 1..end])));
        }
        bindings
    }

    /// The `let` that binds `name` last before the log call, if any (the
    /// binding in scope there, by shadowing).
    fn last_let(&self, name: &str) -> Option<(String, String)> {
        self.let_bindings().into_iter().rev().find(|(pattern, _)| {
            let pattern_chars: Vec<char> = pattern.chars().collect();
            let bound = pattern.split(':').next().unwrap_or(pattern);
            let bound_chars: Vec<char> = bound.chars().collect();
            !word_positions(&pattern_chars, name).is_empty() && !word_positions(&bound_chars, name).is_empty()
        })
    }

    /// Whether the identifier `name` is typed in this function. Its binding
    /// in scope (the last `let`, else the parameter) gives the evidence: a
    /// typed annotation, a typed initializer, or a destructured digest
    /// helper. A binding with no evidence (a closure or pattern binding, an
    /// untyped `let`) is not typed, whatever its name. A name with no
    /// binding in the function is typed only as a typed constant.
    fn identifier_is_typed(&self, name: &str, depth: usize) -> bool {
        if let Some((pattern, init)) = self.last_let(name) {
            if DIGEST_HELPERS
                .iter()
                .any(|(helper, _)| call_name(&init).is_some_and(|callee| callee == *helper))
            {
                return true;
            }
            if pattern.trim_start_matches("mut ").trim() == name {
                return depth < 4 && self.value_is_typed(&init, depth + 1);
            }
            let declared = self.let_binding_types(&pattern, &init, name, depth);
            return !declared.is_empty() && declared.iter().all(|declared| self.index.is_typed_type(declared));
        }
        let parameters = self.parameter_types(name);
        if !parameters.is_empty() {
            return parameters.iter().all(|declared| self.index.is_typed_type(declared));
        }
        self.index
            .consts
            .get(name)
            .is_some_and(|declared| declared.iter().all(|declared| self.index.is_typed_type(declared)))
    }

    /// The declared types `name` takes from `let <pattern> = <init>`: its
    /// annotation, its field or tuple position in a destructuring pattern, or
    /// the value inside `Some(name)` / `Ok(name)` of a typed initializer.
    fn let_binding_types(&self, pattern: &str, init: &str, name: &str, depth: usize) -> Vec<String> {
        if let Some(declared) = self.pattern_binding_type(pattern, name) {
            return vec![declared];
        }
        if let Some((bound, declared)) = pattern.split_once(':') {
            return if bound.trim().trim_start_matches("mut ").trim() == name {
                vec![normalized_str(declared)]
            } else {
                Vec::new()
            };
        }
        if pattern.trim_start_matches("mut ").trim() == name {
            return if depth < 4 {
                self.declared_types_at(init, depth + 1)
            } else {
                Vec::new()
            };
        }
        let wrapped = ["Some", "Ok"].iter().any(|wrapper| {
            pattern
                .strip_prefix(wrapper)
                .and_then(|rest| rest.trim().strip_prefix('('))
                .and_then(|rest| rest.strip_suffix(')'))
                .is_some_and(|inner| inner.trim().trim_start_matches("mut ").trim() == name)
        });
        if !wrapped || depth >= 4 {
            return Vec::new();
        }
        let outer = self.declared_types_at(init, depth + 1);
        let inner: Vec<String> = outer
            .iter()
            .filter_map(|declared| unwrap_option_or_result(declared))
            .collect();
        if inner.len() == outer.len() { inner } else { Vec::new() }
    }

    /// The declared types of an expression: an identifier's annotations, a
    /// field's declarations, a call's declared return types.
    fn declared_types(&self, expression: &str) -> Vec<String> {
        self.declared_types_at(expression, 0)
    }

    fn declared_types_at(&self, expression: &str, depth: usize) -> Vec<String> {
        let expression = strip_value_tail(expression.trim());
        if expression.chars().all(is_ident_char) {
            if depth >= 4 {
                return Vec::new();
            }
            return match self.last_let(expression) {
                Some((pattern, init)) => self.let_binding_types(&pattern, &init, expression, depth),
                None => self.parameter_types(expression),
            };
        }
        if let Some(callee) = call_name(expression) {
            // A method of a receiver whose type is known: that type's own
            // declaration; otherwise every declaration of the name.
            let receiver = method_receiver(expression);
            if let Some(receiver) = receiver
                && depth < 4
            {
                let receiver_types = self.declared_types_at(receiver, depth + 1);
                let mut declared = Vec::new();
                for receiver_type in &receiver_types {
                    let returns = self.index.method_returns(receiver_type, callee);
                    if returns.is_empty() {
                        declared.clear();
                        break;
                    }
                    declared.extend(returns);
                }
                if !declared.is_empty() {
                    return declared;
                }
            }
            return self
                .index
                .call_returns(callee, receiver.is_some())
                .cloned()
                .unwrap_or_default();
        }
        if is_path(expression)
            && let Some((owner, field)) = expression.rsplit_once('.')
        {
            // The owner's own declaration of the field when the owner's type
            // is known, otherwise every declaration of a field of that name.
            let owner_types = if owner == "self" {
                self.self_type.map(|name| vec![name.to_string()]).unwrap_or_default()
            } else if depth < 4 {
                self.declared_types_at(owner, depth + 1)
            } else {
                Vec::new()
            };
            let mut declared = Vec::new();
            for owner_type in &owner_types {
                let owner_name = type_name(strip_references(owner_type)).to_string();
                match self.index.struct_fields.get(&(owner_name, field.to_string())) {
                    Some(field_types) => declared.extend(field_types.iter().cloned()),
                    None => return self.index.fields.get(field).cloned().unwrap_or_default(),
                }
            }
            if declared.is_empty() {
                return self.index.fields.get(field).cloned().unwrap_or_default();
            }
            return declared;
        }
        Vec::new()
    }

    /// Whether `receiver.accessor()` is a count: the receiver is a std
    /// collection, string or iterator, or a production type whose own
    /// `accessor` method is declared typed.
    fn count_is_typed(&self, receiver: &str, accessor: &str) -> bool {
        if call_name(receiver).is_some_and(|callee| ITERATOR_CALLS.contains(&callee)) {
            return true;
        }
        let declared = self.declared_types(receiver);
        !declared.is_empty()
            && declared
                .iter()
                .all(|declared| collection_element(declared).is_some() || self.typed_method(declared, accessor))
    }

    /// Whether the production type `declared` has a method `method`, and
    /// every declaration of it returns a typed value.
    fn typed_method(&self, declared: &str, method: &str) -> bool {
        let returns = self.index.method_returns(declared, method);
        !returns.is_empty() && returns.iter().all(|returned| self.index.is_typed_type(returned))
    }

    /// Whether `receiver.get(..)` is a number: a collection of typed
    /// elements, or a `Cell`/`NonZero*` of a number.
    fn get_is_typed(&self, receiver: &str) -> bool {
        let declared = self.declared_types(receiver);
        !declared.is_empty()
            && declared.iter().all(|declared| {
                let cell = generic_arguments(declared.trim_start_matches('&').trim(), "Cell")
                    .and_then(|arguments| arguments.first().cloned());
                let non_zero = type_name(declared).starts_with("NonZero");
                non_zero
                    || self.typed_method(declared, "get")
                    || cell.is_some_and(|inner| self.index.is_typed_type(&inner))
                    || collection_element(declared)
                        .is_some_and(|element| element != "str" && self.index.is_typed_type(&element))
            })
    }

    /// Whether one logged value (masked text) is typed metadata.
    fn value_is_typed(&self, masked_value: &str, depth: usize) -> bool {
        let mut value = normalized_str(masked_value);
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
        if let Some((_, cast)) = value.rsplit_once(" as ")
            && SCALAR_TYPES.contains(&cast.trim())
        {
            return true;
        }
        let value = strip_value_tail(&value);
        if let Some(callee) = call_name(value) {
            return self.call_is_typed(value, callee, depth);
        }
        if !is_path(value) {
            return false;
        }
        if value.chars().all(is_ident_char) {
            return self.identifier_is_typed(value, depth);
        }
        if value.contains('.') {
            let declared = self.declared_types(value);
            return !declared.is_empty() && declared.iter().all(|declared| self.index.is_typed_type(declared));
        }
        // `Type::VARIANT` of a typed enum, or a typed constant.
        let mut segments = value.rsplit("::");
        let last = segments.next().unwrap_or(value);
        let owner = segments.next().unwrap_or("");
        self.index.typed_names.contains(owner)
            || self
                .index
                .consts
                .get(last)
                .is_some_and(|declared| declared.iter().all(|declared| self.index.is_typed_type(declared)))
    }

    fn call_is_typed(&self, value: &str, callee: &str, depth: usize) -> bool {
        let callee_path = callee_path(value);
        let plain_label = PLAIN_LABEL_CONSTRUCTORS.iter().any(|(constructor, _)| {
            callee_path
                .strip_suffix(constructor)
                .is_some_and(|before| before.is_empty() || before.ends_with("::"))
        });
        if plain_label || PAYLOAD_FREE_HELPERS.iter().any(|(helper, _)| *helper == callee) {
            return true;
        }
        // A fully qualified call (`Type::method(receiver)`): the compiler
        // proves the receiver is a `Type`, the declaration gives the result.
        let owner = callee_path
            .strip_suffix(callee)
            .and_then(|before| before.strip_suffix("::"))
            .map(|before| before.rsplit("::").next().unwrap_or(before));
        if let Some(owner) =
            owner.filter(|owner| *owner == "str" || owner.starts_with(|character: char| character.is_ascii_uppercase()))
        {
            let std_count = COUNT_ACCESSORS.contains(&callee)
                && (COLLECTION_TYPES.iter().any(|(name, _)| *name == owner) || owner == "String" || owner == "str");
            let std_duration = DURATION_ACCESSORS.contains(&callee) && owner == "Duration";
            if std_count || std_duration || self.typed_method(owner, callee) {
                return true;
            }
        }
        let receiver = method_receiver(value);
        if COUNT_ACCESSORS.contains(&callee) {
            return receiver.is_some_and(|receiver| self.count_is_typed(receiver, callee));
        }
        if DURATION_ACCESSORS.contains(&callee) {
            return receiver.is_some_and(|receiver| {
                call_name(receiver).is_some_and(|inner| DURATION_SOURCES.contains(&inner)) || {
                    let declared = self.declared_types(receiver);
                    !declared.is_empty() && declared.iter().all(|declared| type_name(declared) == "Duration")
                }
            });
        }
        if callee == "get" {
            return receiver.is_some_and(|receiver| self.get_is_typed(receiver));
        }
        if FLAG_PREDICATES.contains(&callee) {
            return self
                .index
                .call_returns(callee, receiver.is_some())
                .is_none_or(|declared| declared.iter().all(|declared| self.index.is_typed_type(declared)));
        }
        if NUMERIC_METHODS.contains(&callee) {
            return depth < 4 && receiver.is_some_and(|receiver| self.value_is_typed(receiver, depth + 1));
        }
        if let Some(receiver) = receiver {
            let declared = self.declared_types(receiver);
            if !declared.is_empty() && declared.iter().all(|declared| self.typed_method(declared, callee)) {
                return true;
            }
        }
        self.index
            .call_returns(callee, receiver.is_some())
            .is_some_and(|declared| declared.iter().all(|declared| self.index.is_typed_type(declared)))
    }
}

/// The name of the function or method a value calls last, if it is a call.
fn call_name(value: &str) -> Option<&str> {
    let open = call_open(value)?;
    let mut callee_end = value[..open].trim_end();
    if callee_end.ends_with('>')
        && let Some(turbofish) = callee_end.rfind("::<")
    {
        callee_end = &callee_end[..turbofish];
    }
    let start = callee_end
        .char_indices()
        .rev()
        .take_while(|(_, character)| is_ident_char(*character))
        .last()
        .map(|(at, _)| at)?;
    Some(&callee_end[start..])
}

/// The byte index of the `(` that opens a value's last call.
fn call_open(value: &str) -> Option<usize> {
    let value = value.trim_end();
    if !value.ends_with(')') {
        return None;
    }
    let mut depth = 0usize;
    for (at, character) in value.char_indices().rev() {
        match character {
            ')' => depth += 1,
            '(' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

/// The whole callee path of a value's last call, whitespace removed.
fn callee_path(value: &str) -> String {
    call_open(value)
        .map(|open| value[..open].split_whitespace().collect())
        .unwrap_or_default()
}

/// The receiver of a method call (`receiver.method(..)`), if it is one.
fn method_receiver(value: &str) -> Option<&str> {
    let open = call_open(value)?;
    let callee = call_name(value)?;
    let before = value[..open].trim_end();
    let before = before.strip_suffix(callee)?.trim_end();
    before.strip_suffix('.').map(str::trim_end)
}

/// Reviewed log sites whose argument the scanner cannot prove, with the
/// reason. A new entry is an architecture review event.
const REVIEWED_LOG_SITES: &[(&str, &str)] = &[
    (
        "src/chat/dispatcher.rs::emit_trace `msg`",
        "the reducer's LogTrace sink: every `Effect::LogTrace` message is a `format!` this scanner checks at its construction",
    ),
    (
        "src/agent/prompt.rs::build `prompt_sha256 = %compiled.sha256`",
        "a hex SHA-256 digest of the compiled prompt (`prompt_sha256`)",
    ),
    (
        "src/agent/prompt_cache_metrics.rs::record_round_trip `system_prompt_sha256 = round.system_prompt_sha256`",
        "a hex SHA-256 digest of the system prompt (`prompt_sha256`)",
    ),
    (
        "src/agent/prompt_cache_metrics.rs::record_round_trip `tools_sha256 = round.tools_sha256`",
        "a hex SHA-256 digest of the tool schema",
    ),
    (
        "src/agent/turn_engine.rs::run_turn_unguarded `decision_brief_sha256 = brief_hash`",
        "a hex SHA-256 digest of the decision brief",
    ),
    (
        "src/agent/turn_engine.rs::run_turn_unguarded `system_prompt_sha256 = system_prompt_sha256.as_deref()`",
        "a hex SHA-256 digest of the final system prompt (`prompt_sha256`)",
    ),
    (
        "src/agent/turn_engine.rs::run_turn_unguarded `system_prompt_sha256 = system_prompt_sha256.as_deref().unwrap_or( )`",
        "a hex SHA-256 digest of the final system prompt (`prompt_sha256`)",
    ),
    (
        "src/agent/turn_engine.rs::run_turn_unguarded `tool_snapshot_sha256 = %tool_snapshot.sha256`",
        "a hex SHA-256 digest of the advertised tool snapshot",
    ),
];

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
fn call_violations(
    raw: &[char],
    masked: &[char],
    open: usize,
    owners: &[(String, usize, usize)],
    index: &TypeIndex,
) -> Vec<(usize, String)> {
    let function_start = (0..open)
        .rev()
        .find(|&at| masked[at..].starts_with(&['f', 'n', ' ']) && (at == 0 || !is_ident_char(masked[at - 1])))
        .unwrap_or(0);
    let function = text(&masked[function_start..open]);
    let signature_end = {
        let mut depth = 0usize;
        (function_start..open)
            .find(|&at| match masked[at] {
                '(' => {
                    depth += 1;
                    false
                }
                ')' => {
                    depth = depth.saturating_sub(1);
                    false
                }
                '{' => depth == 0,
                _ => false,
            })
            .unwrap_or(open)
    };
    let signature = text(&masked[function_start..signature_end]);
    let self_type = owners
        .iter()
        .filter(|(_, start, end)| *start < open && open < *end)
        .min_by_key(|(_, start, end)| end - start)
        .map(|(owner, _, _)| owner.as_str());
    let scope = Scope {
        function: &function,
        signature: &signature,
        self_type,
        index,
    };
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
                if !scope.identifier_is_typed(&capture, 0) {
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
        if !scope.value_is_typed(&value, 0) {
            violations.push((start, shown));
        }
    }
    violations
}

/// The non-metadata log arguments of one production file.
fn payload_capable_log_arguments(relative: &str, raw_source: &str, index: &TypeIndex) -> Vec<String> {
    let production = strip_cfg_test_items(&blank_comments_and_literals(raw_source));
    let raw: Vec<char> = raw_source.chars().collect();
    let masked: Vec<char> = production.chars().collect();
    if raw.len() != masked.len() {
        return Vec::new();
    }
    let lines: Vec<&str> = production.lines().collect();
    let owners = impl_blocks(&masked);
    let mut found: Vec<(usize, String)> = Vec::new();
    for macro_name in LOG_MACROS {
        let mut from = 0usize;
        while let Some(at) = find_chars(&masked, macro_name, from) {
            from = at + macro_name.len();
            if at > 0 && is_ident_char(masked[at - 1]) {
                continue;
            }
            found.extend(call_violations(
                &raw,
                &masked,
                at + macro_name.len() - 1,
                &owners,
                index,
            ));
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
            found.extend(call_violations(&raw, &masked, format_open, &owners, index));
        } else if !(Scope {
            function: "",
            signature: "",
            self_type: None,
            index,
        })
        .value_is_typed(value, 0)
        {
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
    let production: Vec<(String, String)> = production_source_files()
        .iter()
        .map(|path| (relative_path(path), read_source(path)))
        .filter(|(relative, raw)| !is_test_code_file(relative, raw, &modules))
        .collect();
    let masked: Vec<String> = production
        .iter()
        .map(|(_, raw)| strip_cfg_test_items(&blank_comments_and_literals(raw)))
        .collect();
    let index = TypeIndex::of_masked_sources(masked.iter().map(String::as_str));
    let mut actual = BTreeSet::new();
    for (relative, raw) in &production {
        if LOG_SCAN_ROOTS.iter().any(|root| relative.starts_with(root)) {
            actual.extend(payload_capable_log_arguments(relative, raw, &index));
        }
    }
    let reviewed: Vec<&str> = REVIEWED_LOG_SITES.iter().map(|(site, _)| *site).collect();
    assert_inventory(
        "log arguments that can carry free-form text (use agent::diagnostics::error_class / text_class)",
        actual.into_iter().collect(),
        &reviewed,
    );
}

/// The argument classifier behind `log_lines_carry_only_typed_metadata`
/// rejects every free-form form and accepts every typed one: names are never
/// evidence, declarations are.
#[test]
fn payload_capable_log_arguments_sees_every_form() {
    let source = r#"pub enum MemoryBackendKind { Sqlite, Unknown }
pub enum CompactReason { ContextOverflow, Manual }
pub struct Session { turns: Vec<String> }
impl Session {
    pub fn turn_count(&self) -> usize { self.turns.len() }
}
impl Runtime {
    pub fn backend_kind(&self) -> MemoryBackendKind { MemoryBackendKind::Unknown }
}
fn f(error: anyhow::Error, reason: &'static str, name: &str, backend: MemoryBackendKind, s: &Session, history: &[ChatMessage], is_terminal: bool, ids: &[u64], args: &serde_json::Value, tool_kind: &str, task_id: u64) {
    let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
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
    tracing::warn!(tool = %ToolName(name), "l");
    tracing::warn!(tool = %crate::agent::diagnostics::spec(name), "m");
    tracing::warn!(model = %model_label, "n");
    tracing::warn!(payload = ?args.get("password"), "p");
    tracing::warn!(tool_kind, "q");
    tracing::warn!(session_id = %session_id, "r");
    tracing::warn!(count = items.len(), "s");
    tracing::warn!(task_id = id.get(), kind = ?mode, sha = digest_sha256.as_deref().unwrap_or("none"), "t");
    tracing::warn!(
        error = %crate::agent::diagnostics::error_class(&error),
        tool = %crate::agent::diagnostics::text_class(name),
        reason,
        task_id,
        first_id = ids.get(0),
        turns_count = s.turn_count(),
        history_len = history.len(),
        rounds = history.iter().filter(|message| message.role == "tool").count(),
        is_terminal,
        error_len,
        error_sha256,
        elapsed_ms = started.elapsed().as_millis(),
        compact = ?CompactReason::Manual,
        tool = %crate::agent::diagnostics::ToolName::spec(spec),
        tool = %ToolName::descriptor(&request.descriptor),
        provider = %ProviderLabel::redacted(round.provider),
        backend_kind = ?runtime.memory.backend_kind(),
        backend_kind = ?backend,
        "ok {reason} {}", history.len()
    );
}
fn h(spec: &ToolSpec) {
    let provider: ProviderLabel = ProviderLabel::redacted(round.provider);
    tracing::debug!(%provider, "o");
}
fn g() -> Vec<Effect> {
    vec![Effect::LogTrace { level: tracing::Level::DEBUG, msg: format!("x {name}") }]
}
pub struct TurnTask { pub sequence: u64, pub input: String }
pub struct TurnScheduler { tasks: Vec<TurnTask> }
impl TurnScheduler {
    pub fn task(&self, id: u64) -> Option<&TurnTask> { self.tasks.first() }
    fn log(&self) {
        tracing::debug!(tasks = self.tasks.len(), "w");
    }
}
fn k(scheduler: &TurnScheduler, id: u64) {
    let Some(task) = scheduler.task(id) else {
        return;
    };
    tracing::warn!(task_seq = task.sequence, "u");
    tracing::warn!(input = %task.input, "v");
}
"#;
    // The debug spec is spliced in so the fixture is not itself a format string.
    let source = source.replace("@DEBUG_SPEC", concat!(":", "?"));
    let masked = strip_cfg_test_items(&blank_comments_and_literals(&source));
    let index = TypeIndex::of_masked_sources([masked.as_str()]);
    let found = payload_capable_log_arguments("src/chat/x.rs", &source, &index);
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
            "src/chat/x.rs::f `tool = %ToolName(name)`".to_string(),
            "src/chat/x.rs::f `tool = %crate::agent::diagnostics::spec(name)`".to_string(),
            "src/chat/x.rs::f `model = %model_label`".to_string(),
            "src/chat/x.rs::f `payload = ?args.get( )`".to_string(),
            "src/chat/x.rs::f `tool_kind`".to_string(),
            "src/chat/x.rs::f `session_id = %session_id`".to_string(),
            "src/chat/x.rs::f `count = items.len()`".to_string(),
            "src/chat/x.rs::f `task_id = id.get()`".to_string(),
            "src/chat/x.rs::f `kind = ?mode`".to_string(),
            "src/chat/x.rs::f `sha = digest_sha256.as_deref().unwrap_or( )`".to_string(),
            "src/chat/x.rs::g `{name}`".to_string(),
            "src/chat/x.rs::k `input = %task.input`".to_string(),
        ],
        "each free-form form must be reported once, and no typed form may be"
    );
}

/// A `char` is text, and a `KeyCode` can hold one: neither is logged. A named
/// key reaches a log only as the `KeyLogClass` that `key_log_class` returns.
#[test]
fn character_and_key_code_forms_are_judged_by_their_declared_types() {
    let source = r#"impl KeyLogClass { pub(crate) const fn label(self) -> &'static str { "char" } }
pub(crate) fn key_log_class(code: KeyCode) -> KeyLogClass { KeyLogClass::Char }
fn keys(key: &KeyEvent, typed: char) {
    let key_class = key_log_class(key.code);
    tracing::debug!(key_class = key_class.label(), "accepted");
    tracing::warn!(typed = %typed, "rejected");
    let named_key: Option<&KeyCode> = Some(&key.code);
    tracing::warn!(named_key = ?named_key, "rejected");
}
"#;
    let masked = strip_cfg_test_items(&blank_comments_and_literals(source));
    let index = TypeIndex::of_masked_sources([masked.as_str()]);
    let found = payload_capable_log_arguments("src/chat/x.rs", source, &index);
    assert_eq!(
        found,
        vec![
            "src/chat/x.rs::keys `typed = %typed`".to_string(),
            "src/chat/x.rs::keys `named_key = ?named_key`".to_string(),
        ],
        "a char and a KeyCode must be reported, and a KeyLogClass must not"
    );
}

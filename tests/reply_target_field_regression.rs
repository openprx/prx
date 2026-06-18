//! Regression guard for `ChannelMessage` field naming consistency.
//!
//! This test prevents accidental reintroduction of the removed `reply_to` field
//! in Rust source code where `reply_target` must be used.
#![allow(clippy::panic, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

const SCAN_PATHS: &[&str] = &["src", "examples"];

/// The base field token whose legacy use we forbid. We scan for this token and
/// then decide per-occurrence whether it is a real `reply_to` field access vs a
/// longer, legitimate identifier (`reply_to_sender_jid`, `reply_to_id`, …).
const BASE_TOKEN: &str = "reply_to";

/// Returns true when the line contains a *complete* legacy `reply_to` field
/// reference, distinguishing it from longer, unrelated identifiers such as the
/// wacli webhook payload's `reply_to_sender_jid` / `reply_to_id` /
/// `reply_to_display`, which mirror the official wacli `ParsedMessage` and must
/// not be flagged.
///
/// A `reply_to` occurrence is a violation when it is NOT part of a longer
/// identifier (the next char is not `[A-Za-z0-9_]`) AND it appears in one of the
/// two banned syntactic forms:
///   * field access:      `.reply_to`           (preceded by `.`)
///   * field definition/  `reply_to:` / `reply_to :`  (followed by optional
///     struct literal:     whitespace then `:`)
fn line_violates(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut start = 0;
    while let Some(rel) = line[start..].find(BASE_TOKEN) {
        let token_start = start + rel;
        let after = token_start + BASE_TOKEN.len();

        // Skip when `reply_to` is just the prefix of a longer identifier
        // (e.g. `reply_to_sender_jid`): the next byte continues the identifier.
        let next_is_ident_char = bytes
            .get(after)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
        if !next_is_ident_char {
            // Form (a): `.reply_to` — immediately preceded by a `.`.
            let preceded_by_dot = token_start > 0 && bytes.get(token_start - 1) == Some(&b'.');

            // Form (b): `reply_to` followed by optional whitespace then `:`
            // (covers `reply_to:` and the legal-Rust `reply_to :`).
            let followed_by_colon = {
                let mut idx = after;
                while bytes.get(idx).is_some_and(u8::is_ascii_whitespace) {
                    idx += 1;
                }
                bytes.get(idx) == Some(&b':')
            };

            if preceded_by_dot || followed_by_colon {
                return true;
            }
        }
        start = after;
    }
    false
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|err| panic!("Failed to read directory {}: {err}", dir.display()));

    for entry in entries {
        let entry = entry.unwrap_or_else(|err| panic!("Failed to read entry in {}: {err}", dir.display()));
        let path = entry.path();

        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn source_does_not_use_legacy_reply_to_field() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut rust_files = Vec::new();

    for relative in SCAN_PATHS {
        collect_rs_files(&root.join(relative), &mut rust_files);
    }

    rust_files.sort();

    let mut violations = Vec::new();

    for file_path in rust_files {
        let content = fs::read_to_string(&file_path)
            .unwrap_or_else(|err| panic!("Failed to read source file {}: {err}", file_path.display()));

        for (line_idx, line) in content.lines().enumerate() {
            if line_violates(line) {
                let rel = file_path.strip_prefix(root).unwrap_or(&file_path).display().to_string();
                violations.push(format!(
                    "{rel}:{} contains forbidden legacy `reply_to` field: {}",
                    line_idx + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Found legacy `reply_to` field usage:\n{}",
        violations.join("\n")
    );
}

#[cfg(test)]
mod line_violates_tests {
    use super::line_violates;

    #[test]
    fn flags_legacy_field_access_and_definitions() {
        // Field access form.
        assert!(line_violates("let x = msg.reply_to;"));
        // Struct-literal / definition form, no space.
        assert!(line_violates("reply_to: Some(id),"));
        // Legal-Rust form with whitespace before the colon (the P2-5 gap).
        assert!(line_violates("reply_to : value,"));
        assert!(line_violates("    reply_to  :  some_value,"));
    }

    #[test]
    fn ignores_longer_legitimate_identifiers() {
        // wacli webhook payload fields — must NOT be flagged.
        assert!(!line_violates("let j = parsed.reply_to_sender_jid;"));
        assert!(!line_violates("reply_to_sender_jid: jid,"));
        assert!(!line_violates("reply_to_id: Some(1),"));
        assert!(!line_violates("self.reply_to_display.clone()"));
    }

    #[test]
    fn ignores_unrelated_lines() {
        assert!(!line_violates("let reply_target = Some(id);"));
        assert!(!line_violates("// reply target lives in reply_target now"));
        // Bare token not in a banned syntactic form (no leading `.`, no trailing `:`).
        assert!(!line_violates("let reply_to = 1;"));
    }
}

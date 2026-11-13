//! Bounded, redacted summaries of runtime artifacts and the one text
//! redactor they share with the decision-state projection (moved from
//! `causal_tree::projection`, R15). Every text goes through [`redact_text`]:
//! credentials, secret-store values and private URLs are replaced, control
//! characters and long base64 runs are sanitized, and the result is cut at a
//! character boundary.

use std::borrow::Cow;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

const ARTIFACT_SUMMARY_CHARS: usize = 256;
/// The longest run of base64-alphabet characters a projected string keeps
/// (below the protocol's 1 KiB binary-run limit).
const MAX_BASE64_RUN: usize = 512;
const REDACTED: &str = "[REDACTED]";
pub(crate) const PRIVATE_URL: &str = "[private-url]";
pub(crate) const BINARY: &str = "[binary]";

/// Prepare a runtime artifact from its complete bounded source text. The
/// normal projection rechecks the result and accounts for these operations.
pub(crate) fn bounded_artifact_summary(text: &str) -> (String, bool, bool) {
    let (redacted, was_redacted) = redact_text(text);
    let (summary, was_truncated) = cut_chars(redacted, ARTIFACT_SUMMARY_CHARS);
    (summary, was_redacted, was_truncated)
}

pub(crate) fn cut_bytes(text: String, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let end = (0..=max).rev().find(|index| text.is_char_boundary(*index)).unwrap_or(0);
    (text.get(..end).unwrap_or_default().to_string(), true)
}

pub(crate) fn cut_chars(text: String, max: usize) -> (String, bool) {
    if text.chars().count() <= max {
        return (text, false);
    }
    (text.chars().take(max).collect(), true)
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

static SECRET_STORE_VALUE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:enc2?:[0-9a-f]{8,}|profile:[A-Za-z0-9._-]+)").ok());

/// A URL as far as white space, `<`, `>`, a quote or a backtick; punctuation
/// that ends a sentence or a bracket is split off by [`url_tail_start`].
static URL: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"(?i)\b(?:[a-z][a-z0-9+.-]*://[^\s<>"'`]+|file:[^\s<>"'`]+)"#).ok());

/// Replace every match of `pattern` with `replacement`. A pattern that failed
/// to compile (impossible for these constants; `regex` is deterministic)
/// replaces everything, so a broken pattern can never let a secret through.
fn replace_all<'t>(pattern: &LazyLock<Option<Regex>>, text: &'t str, replacement: &str) -> Cow<'t, str> {
    pattern.as_ref().map_or_else(
        || Cow::Owned(REDACTED.to_string()),
        |regex| regex.replace_all(text, replacement),
    )
}

/// `text` with every credential, secret-store value and private URL
/// replaced, then sanitized to the `StructuredValue` text rules, and whether
/// anything was redacted.
pub(crate) fn redact_text(text: &str) -> (String, bool) {
    let (text, mut changed) = redact_json_document(text).map_or((Cow::Borrowed(text), false), |document| {
        (Cow::Owned(document.0), document.1)
    });
    let scrubbed = crate::security::redaction::redact_secrets(&text);
    changed |= scrubbed != *text;
    let mut text = scrubbed;
    let replaced = match replace_all(&SECRET_STORE_VALUE, &text, REDACTED) {
        Cow::Owned(replaced) => Some(replaced),
        Cow::Borrowed(_) => None,
    };
    if let Some(replaced) = replaced {
        changed |= replaced != text;
        text = replaced;
    }
    let (text, private) = redact_private_urls(&text);
    (sanitize(&text), changed || private)
}

/// `text` with every private URL replaced by [`PRIVATE_URL`], keeping any
/// punctuation after it, and whether one was replaced.
fn redact_private_urls(text: &str) -> (String, bool) {
    let Some(pattern) = URL.as_ref() else {
        return (REDACTED.to_string(), true);
    };
    let mut changed = false;
    let replaced = pattern.replace_all(text, |captures: &regex::Captures<'_>| {
        let found = captures.get(0).map_or("", |found| found.as_str());
        let (url, tail) = found.split_at(url_tail_start(found));
        if is_private_url(url) {
            changed = true;
            format!("{PRIVATE_URL}{tail}")
        } else {
            found.to_string()
        }
    });
    (replaced.into_owned(), changed)
}

/// Where the trailing punctuation of a matched URL starts: closing brackets,
/// quotes and sentence punctuation at its end are not part of it, except a
/// `]` that closes a bracketed IPv6 host.
fn url_tail_start(found: &str) -> usize {
    let mut end = found
        .trim_end_matches(|character| {
            matches!(
                character,
                ')' | ']' | '}' | '>' | ',' | ';' | '.' | '!' | '?' | ':' | '"' | '\''
            )
        })
        .len();
    let kept = found.get(..end).unwrap_or(found);
    let rest = found.get(end..).unwrap_or_default();
    if kept.matches('[').count() > kept.matches(']').count() && rest.starts_with(']') {
        end += 1;
    }
    end
}

/// When `text` is a JSON object or array: the document with its sensitive
/// keys redacted, re-serialized compactly, and whether a value changed.
fn redact_json_document(text: &str) -> Option<(String, bool)> {
    let trimmed = text.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    let document: Value = serde_json::from_str(trimmed).ok()?;
    let mut redacted = document.clone();
    crate::security::redaction::redact_json_value(&mut redacted);
    let changed = redacted != document;
    Some((redacted.to_string(), changed))
}

/// Host suffixes that only resolve on a local network.
const LOCAL_SUFFIXES: [&str; 6] = [
    ".localhost",
    ".local",
    ".internal",
    ".lan",
    ".home.arpa",
    ".localdomain",
];

/// Whether `url` names a private or local host (by its literal host only;
/// nothing is resolved) or is a `file:` URL.
pub(crate) fn is_private_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("file:") {
        return true;
    }
    let Some((_, rest)) = lower.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#', '\\']).next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = host_port.strip_prefix('[').map_or_else(
        || {
            let end = host_port
                .find(|character: char| !(character.is_alphanumeric() || matches!(character, '.' | '-' | '_' | '%')))
                .unwrap_or(host_port.len());
            host_port.get(..end).unwrap_or_default()
        },
        |bracketed| bracketed.split(']').next().unwrap_or_default(),
    );
    let host = host.trim_end_matches('.');
    if host.is_empty() || host == "localhost" || LOCAL_SUFFIXES.iter().any(|suffix| host.ends_with(suffix)) {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => is_private_v4(v4),
        Ok(IpAddr::V6(v6)) => {
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || v6.to_ipv4_mapped().is_some_and(is_private_v4)
        }
        Err(_) => parse_loose_ipv4(host).is_some_and(is_private_v4),
    }
}

/// Loopback, RFC 1918, link-local, CGNAT (`100.64.0.0/10`, where tailnet
/// hosts live), "this network", unspecified and broadcast addresses.
const fn is_private_v4(v4: Ipv4Addr) -> bool {
    let octets = v4.octets();
    v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || octets[0] == 0
        || (octets[0] == 100 && (octets[1] & 0xC0) == 64)
}

/// The IPv4 address of a host written in one of the short numeric forms a
/// resolver also accepts: one to four parts separated by `.`, each decimal,
/// `0x` hex or `0` octal, the last part filling the remaining bytes
/// (`2130706433`, `0x7f.1`, `127.1`).
fn parse_loose_ipv4(host: &str) -> Option<Ipv4Addr> {
    let count = host.split('.').count();
    if !(1..=4).contains(&count) {
        return None;
    }
    let mut address: u32 = 0;
    for (index, part) in host.split('.').enumerate() {
        let value = parse_inet_number(part)?;
        let remaining = 4 - index;
        if index + 1 < count {
            let byte = u8::try_from(value).ok()?;
            address |= u32::from(byte) << (8 * (remaining - 1));
        } else {
            let limit = 1_u64
                .checked_shl(u32::try_from(8 * remaining).ok()?)
                .unwrap_or(u64::MAX);
            if value >= limit {
                return None;
            }
            address |= u32::try_from(value).ok()?;
        }
    }
    Some(Ipv4Addr::from(address))
}

/// One part of a short numeric IPv4 host: decimal, `0x` hex or `0` octal.
fn parse_inet_number(part: &str) -> Option<u64> {
    let (digits, radix) = if let Some(hex) = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
        (hex, 16)
    } else if part.len() > 1 && part.starts_with('0') {
        (part.get(1..)?, 8)
    } else {
        (part, 10)
    };
    if digits.is_empty() {
        return None;
    }
    u64::from_str_radix(digits, radix)
        .ok()
        .filter(|value| u32::try_from(*value).is_ok())
}

/// Control characters (other than tab and line breaks) and U+FFFD become
/// spaces, and base64-alphabet runs longer than [`MAX_BASE64_RUN`] become
/// [`BINARY`].
fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.chars().count() > MAX_BASE64_RUN {
            out.push_str(BINARY);
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for character in text.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '+' | '/' | '=' | '-' | '_') {
            run.push(character);
            continue;
        }
        flush(&mut run, &mut out);
        let control = character < ' ' && !matches!(character, '\t' | '\n' | '\r');
        if control || character == '\u{FFFD}' {
            out.push(' ');
        } else {
            out.push(character);
        }
    }
    flush(&mut run, &mut out);
    out
}

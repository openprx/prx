//! The one secret redactor (D-054).
//!
//! Every secret PRX persists, logs or shows is detected here: replies,
//! provider errors, turn summaries, message and task payloads, session files,
//! provider error strings, audit records and tool diagnostics. Detection is
//! the union of the pattern sets that chat's persistence sanitizer, the
//! provider error scrubber, the audit log and the tool diagnostics each kept
//! before they were merged (D-054 amendment 3), plus a few formats none of
//! them knew:
//!
//! - credentials in a URL's userinfo (`scheme://user:password@host`), through
//!   [`crate::providers::redact_endpoint_userinfo`] (`scheme://[redacted]@host`);
//! - `name = value` / `name: value` credentials, bare, single- or
//!   double-quoted, the name kept, at any non-empty length, when the name is a
//!   credential name ([`is_credential_key`]): a credential word or suffix
//!   (`token`, `mytoken`, `PGPASSWORD`, `clientsecret`, `apikey`, ...) or any
//!   name ending in `key` (`api_key`, `hmac_key`, `masterkey`, ...) unless its
//!   stem is on the non-secret list (`sort_key`, `primary_key`, `public_key`,
//!   `cache_key`, `idempotency_key`, ...); `tokenizer`, `secretary` and
//!   `author` are not credential names either, and a `session_key` value is a
//!   secret only when it looks like one (D-054 amendment 4). The bare names
//!   `key` and `auth` count only as JSON object keys or in `key=` /
//!   `--key=` form, not as `key:` in prose or YAML;
//! - the credential after any `Authorization:` / `Proxy-Authorization:`
//!   scheme (`Bearer`, `Basic`, `Bot`, `SSWS`, `ApiKey`, `AWS4-HMAC-SHA256`,
//!   ...) and after a bare `Bearer`, at any length;
//! - keyless token formats, each with its minimum length (`sk-learn`, a
//!   5-character body, is not a key): `sk-`, `sk-proj-`, `sk-ant-`, `xoxb-`,
//!   `xoxp-`, `ghp_`, `gho_`, `ghu_`, `github_pat_`, `glpat-`, Stripe
//!   `sk_live_` / `rk_live_` / `sk_test_` / `rk_test_`, Google `AIza...`,
//!   AWS access key ids (`AKIA` / `ASIA`), JWTs (`eyJ...` in three segments),
//!   PEM private-key blocks and Telegram bot tokens (`<digits>:<35 chars>`,
//!   also inside a `.../bot<token>/...` URL).
//!
//! [`redact_secrets`] replaces each secret with [`REDACTED`];
//! [`redact_secrets_with`] lets a caller choose how a detected secret is
//! shown (a presentation wrapper, e.g. a diagnostic preview keeping a short
//! prefix). Neither ever truncates: a store that caps what it keeps does so
//! itself, after redaction.

use regex::Regex;
use std::ops::Range;
use std::sync::LazyLock;

/// What a redacted secret is replaced with.
pub const REDACTED: &str = "[REDACTED]";

/// Any `name = value` / `name: value` pair: an opening quote of the name is
/// capture 1, the name capture 2, the separator capture 3, and the value the
/// first non-empty of capture 4 (double-quoted), 5 (single-quoted) or 6
/// (bare). Only a pair whose name is a credential name is a secret.
static KEYED_PAIR: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r#"(["']?)([A-Za-z0-9_.-]+)["']?\s*([:=])\s*(?:"([^"]+)"|'([^']+)'|([^\s"',;{}]+))"#).ok()
});

/// The credential after an `Authorization:` / `Proxy-Authorization:` scheme
/// word, whatever the scheme: capture 1, up to the end of the header value.
static AUTHORIZATION_HEADER: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:proxy-)?authorization["']?\s*[:=]\s*["']?[A-Za-z][A-Za-z0-9_-]*[ \t]+([^\r\n"']+)"#).ok()
});

/// The token after a bare `Bearer`, at any length: capture 1.
static BARE_BEARER: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"(?i)(?:^|[^a-z0-9])bearer\s+([^\s"',;]+)"#).ok());

/// Keyless token formats with a fixed shape: the whole match is the secret.
static KEYLESS_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // AWS access key ids, long-lived and temporary
        r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
        // Stripe secret and restricted keys
        r"\b(?:sk|rk)_(?:live|test)_[0-9A-Za-z]{10,}",
        // Google API keys
        r"\bAIza[0-9A-Za-z_-]{35}",
        // JWTs: base64url header, payload and signature
        r"\beyJ[0-9A-Za-z_-]{8,}\.eyJ[0-9A-Za-z_-]{8,}\.[0-9A-Za-z_-]{8,}",
        // PEM private-key blocks, whole
        r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----[\s\S]*?-----END (?:[A-Z0-9]+ )*PRIVATE KEY-----",
    ]
    .iter()
    // every pattern is a literal pinned by a test of its own
    .filter_map(|pattern| Regex::new(pattern).ok())
    .collect()
});

/// Telegram bot tokens, `<bot id>:<35 characters>`: capture 1. The boundary is
/// checked by hand so a token inside `.../bot<token>/...` counts.
static TELEGRAM_BOT_TOKEN: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"([0-9]{6,12}:[0-9A-Za-z_-]{35})").ok());

/// Authentication scheme words: an `Authorization:` value that is only one of
/// these names the scheme, not a credential.
const AUTH_SCHEMES: [&str; 8] = [
    "bearer",
    "basic",
    "digest",
    "token",
    "bot",
    "negotiate",
    "ssws",
    "apikey",
];

/// Words that make a key a credential name when they are one of its segments.
const CREDENTIAL_SEGMENTS: [&str; 14] = [
    "token",
    "secret",
    "secrets",
    "password",
    "passwords",
    "passwd",
    "pwd",
    "passphrase",
    "credential",
    "credentials",
    "authorization",
    "apikey",
    "bearer",
    "cookie",
];

/// Endings that make a run-together segment a credential name, as the
/// pre-program detectors matched them unanchored: `mytoken`, `authtoken`,
/// `PGPASSWORD`, `clientsecret`, `x_apikey`, `authheader`. `tokenizer`,
/// `secretary` and `author` do not end with one.
const CREDENTIAL_SUFFIXES: [&str; 9] = [
    "token",
    "secret",
    "password",
    "passwd",
    "pwd",
    "apikey",
    "credential",
    "credentials",
    "authheader",
];

/// Stems before `key` that name a non-secret key (D-054 amendment 4): every
/// other name ending in `key`, as a segment (`hmac_key`, `openai_key`) or run
/// together (`masterkey`, `monkey`), is a credential name.
const NON_SECRET_KEY_STEMS: [&str; 28] = [
    "sort",
    "primary",
    "foreign",
    "partition",
    "cache",
    "idempotency",
    "public",
    "dedup",
    "dedupe",
    "lookup",
    "index",
    "hash",
    "row",
    "map",
    "group",
    "shard",
    "routing",
    "cursor",
    "sequence",
    "order",
    "unique",
    "composite",
    "natural",
    "surrogate",
    "object",
    "bucket",
    "message",
    "conversation",
];

/// The stem whose key is a credential only when its value looks like one
/// ([`looks_like_a_secret`]): a PRX session key (`telegram_sender-1`,
/// `delegate:telegram:chat-1:alice`) is an identifier, a session secret is not.
const SESSION_KEY_STEM: &str = "session";

/// How a name ending in `key` classifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyName {
    Credential,
    CredentialIfSecretShaped,
    Plain,
}

/// Classify the stem before a `key` (a segment's prefix or the segment before).
fn key_stem_class(stem: &str) -> KeyName {
    if stem == SESSION_KEY_STEM {
        KeyName::CredentialIfSecretShaped
    } else if NON_SECRET_KEY_STEMS.contains(&stem) {
        KeyName::Plain
    } else {
        KeyName::Credential
    }
}

/// Whether a value has the shape of a secret rather than an identifier: 24+
/// characters of a token alphabet (letters, digits, `+`, `/`, `=`, `-`), with
/// both letters and digits, and not a canonical UUID.
fn looks_like_a_secret(value: &str) -> bool {
    let is_uuid = value.len() == 36
        && value.char_indices().all(|(index, c)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
    value.len() >= 24
        && !is_uuid
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-'))
        && value.chars().any(|c| c.is_ascii_digit())
        && value.chars().any(|c| c.is_ascii_alphabetic())
}

/// A key's segments, lower-cased: split on `_`, `-`, `.` and camelCase
/// (`authToken` -> `auth`, `token`; `APIKey` -> `api`, `key`).
fn key_segments(key: &str) -> Vec<String> {
    let mut segments = Vec::new();
    for part in key.split(['_', '-', '.']).filter(|part| !part.is_empty()) {
        let chars: Vec<char> = part.chars().collect();
        let mut current = String::new();
        for (index, c) in chars.iter().enumerate() {
            let previous = index.checked_sub(1).and_then(|i| chars.get(i));
            let next = chars.get(index + 1);
            let boundary = c.is_ascii_uppercase()
                && previous.is_some_and(|p| {
                    p.is_ascii_lowercase()
                        || p.is_ascii_digit()
                        || (p.is_ascii_uppercase() && next.is_some_and(char::is_ascii_lowercase))
                });
            if boundary && !current.is_empty() {
                segments.push(std::mem::take(&mut current));
            }
            current.push(c.to_ascii_lowercase());
        }
        if !current.is_empty() {
            segments.push(current);
        }
    }
    segments
}

/// Whether a segment names a credential on its own: a credential word or
/// ending, or a run-together `…auth` (`oauth`, the audit log's `auth`
/// suffix) other than the bare `auth` itself.
fn is_credential_segment(segment: &str) -> bool {
    CREDENTIAL_SEGMENTS.contains(&segment)
        || CREDENTIAL_SUFFIXES.iter().any(|suffix| segment.ends_with(suffix))
        || segment.strip_suffix("auth").is_some_and(|stem| !stem.is_empty())
}

/// How `key` classifies by its words, endings and `…key` compounds (the bare
/// names `key` and `auth` are decided by where they appear).
fn classify_name(key: &str) -> KeyName {
    let segments = key_segments(key);
    let by_words = segments.iter().any(|segment| is_credential_segment(segment))
        || segments
            .windows(2)
            .any(|pair| matches!(pair, [left, right] if left == "auth" && right == "header"))
        // `x-auth`, `basic_auth`, `proxy_auth`: an `auth` segment after another
        || (segments.len() > 1 && segments.last().is_some_and(|last| last == "auth"));
    if by_words {
        return KeyName::Credential;
    }
    // `…key` (D-054 amendment 4): a run-together segment (`masterkey`) or a
    // `key` segment after another (`hmac_key`), classified by its stem; the
    // strongest classification among them wins
    let mut class = KeyName::Plain;
    for (index, segment) in segments.iter().enumerate() {
        let stem = if segment == "key" {
            index
                .checked_sub(1)
                .and_then(|previous| segments.get(previous))
                .map(String::as_str)
        } else {
            segment.strip_suffix("key").filter(|stem| !stem.is_empty())
        };
        match stem.map(key_stem_class) {
            Some(KeyName::Credential) => return KeyName::Credential,
            Some(KeyName::CredentialIfSecretShaped) => class = KeyName::CredentialIfSecretShaped,
            Some(KeyName::Plain) | None => {}
        }
    }
    class
}

/// Whether `key` names a credential by its words, compounds and endings.
fn names_a_credential(key: &str) -> bool {
    classify_name(key) == KeyName::Credential
}

/// Whether the whole key is the bare name `key` or `auth`.
fn is_bare_key_or_auth(key: &str) -> bool {
    matches!(key_segments(key).as_slice(), [only] if only == "key" || only == "auth")
}

/// Whether `key`, as a JSON object key, names a credential (D-054 amendments
/// 3 and 4): a credential word or ending, a `…key` name whose stem is not on
/// the non-secret list, or the bare `key` / `auth`. A `session…key` is decided
/// by its value ([`is_credential_value`]).
#[must_use]
pub fn is_credential_key(key: &str) -> bool {
    names_a_credential(key) || is_bare_key_or_auth(key)
}

/// Whether the value under `key` is a secret: always under a credential
/// name, and under a `session…key` only when it looks like one.
fn is_credential_value(key: &str, value: &str) -> bool {
    match classify_name(key) {
        KeyName::Credential => true,
        KeyName::CredentialIfSecretShaped => looks_like_a_secret(value),
        KeyName::Plain => is_bare_key_or_auth(key),
    }
}

/// Token formats recognised by their prefix, with the shortest body each
/// needs; the whole token is the secret. Longer prefixes come first so the
/// most specific one names the match. The minimums are the union of what the
/// pre-program detectors caught (the provider scrubber took any length:
/// `sk-abcdef`, `xoxb-12345`) and what the ruling keeps out: `sk-learn`
/// (a 5-character body) is a package name, not a key.
const TOKEN_PREFIXES: [(&str, usize); 10] = [
    ("github_pat_", 6),
    ("sk-proj-", 6),
    ("sk-ant-", 6),
    ("glpat-", 6),
    ("xoxb-", 4),
    ("xoxp-", 4),
    ("ghp_", 6),
    ("gho_", 6),
    ("ghu_", 6),
    ("sk-", 6),
];

const fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':')
}

/// Whether `text[at..]` starts a token rather than continuing a word, so
/// `task-sk-1` or `desk-top` do not lose their tail.
fn starts_token(text: &str, at: usize) -> bool {
    text[..at]
        .chars()
        .next_back()
        .is_none_or(|previous| !(previous.is_ascii_alphanumeric() || matches!(previous, '_' | '-')))
}

fn prefixed_token_spans(text: &str, spans: &mut Vec<Range<usize>>) {
    // The quick check reads the text once per prefix, up to the first prefix
    // found.
    if !TOKEN_PREFIXES.iter().any(|(prefix, _)| {
        #[cfg(test)]
        record_redaction_work(text.len());
        text.contains(prefix)
    }) {
        return;
    }
    let mut at = 0usize;
    while at < text.len() {
        #[cfg(test)]
        record_redaction_work(TOKEN_PREFIXES.len());
        let matched = TOKEN_PREFIXES
            .iter()
            .find(|(prefix, _)| text[at..].starts_with(*prefix) && starts_token(text, at));
        if let Some((prefix, min_body)) = matched {
            let body = at + prefix.len();
            let end = text[body..]
                .char_indices()
                .find(|(_, c)| !is_token_char(*c))
                .map_or(text.len(), |(offset, _)| body + offset);
            #[cfg(test)]
            record_redaction_work(end - body);
            if end - body >= *min_body {
                spans.push(at..end);
                at = end;
                continue;
            }
        }
        at += text[at..].chars().next().map_or(1, char::len_utf8);
    }
}

fn keyed_pair_spans(text: &str, spans: &mut Vec<Range<usize>>) {
    let Some(pair) = KEYED_PAIR.as_ref() else {
        return;
    };
    // One pass of the pattern over the text (`regex` matches in linear time).
    #[cfg(test)]
    record_redaction_work(text.len());
    for captures in pair.captures_iter(text) {
        // Each match is read again: its groups, the name's segments (twice
        // at most: the header check and the classification) and the value.
        #[cfg(test)]
        record_redaction_work(
            captures.get(0).map_or(0, |whole| whole.len()) + captures.get(2).map_or(0, |name| 2 * name.len()),
        );
        let (Some(name), Some(separator), Some(value)) = (
            captures.get(2),
            captures.get(3),
            (4..=6).find_map(|group| captures.get(group)),
        ) else {
            continue;
        };
        let quoted_name = captures.get(1).is_some_and(|quote| !quote.is_empty());
        // `Authorization: <scheme> <credential>`: the header rule owns it
        let before_scheme = text[value.end()..].strip_prefix([' ', '\t']).is_some_and(|rest| {
            let after_blanks = rest.trim_start_matches([' ', '\t']);
            #[cfg(test)]
            record_redaction_work(1 + rest.len() - after_blanks.len());
            after_blanks.starts_with(|c: char| !c.is_whitespace())
        });
        let names = name.as_str();
        let is_header =
            names.eq_ignore_ascii_case("authorization") || names.eq_ignore_ascii_case("proxy-authorization");
        // an `Authorization:` that names only its scheme holds no credential
        if is_header
            && (before_scheme
                || AUTH_SCHEMES
                    .iter()
                    .any(|scheme| value.as_str().eq_ignore_ascii_case(scheme)))
        {
            continue;
        }
        // bare `key` / `auth`: a JSON object key or `key=` / `--key=`, never
        // `key:` in prose or YAML
        let credential = if is_bare_key_or_auth(names) {
            quoted_name || separator.as_str() == "="
        } else {
            is_credential_value(names, value.as_str())
        };
        if credential {
            spans.push(value.range());
        }
    }
}

/// The byte ranges of every secret in `text` (values only, never their keys),
/// sorted and merged where they overlap. URL userinfo is not included: it is
/// rewritten before detection by [`redact_secrets_with`].
#[must_use]
pub fn find_secrets(text: &str) -> Vec<Range<usize>> {
    // Every stage records the work it does in the test-only counter: one
    // text-length pass per pattern (`regex` matches in linear time) plus the
    // bytes each match is read again, and the spans sorted and merged.
    let mut spans = Vec::new();
    keyed_pair_spans(text, &mut spans);
    for pattern in [AUTHORIZATION_HEADER.as_ref(), BARE_BEARER.as_ref()]
        .into_iter()
        .flatten()
    {
        #[cfg(test)]
        record_redaction_work(text.len());
        for credential in pattern.captures_iter(text).filter_map(|captures| captures.get(1)) {
            #[cfg(test)]
            record_redaction_work(credential.len());
            let trimmed = credential.as_str().trim_end();
            spans.push(credential.start()..credential.start() + trimmed.len());
        }
    }
    for pattern in KEYLESS_PATTERNS.iter() {
        #[cfg(test)]
        record_redaction_work(text.len());
        spans.extend(pattern.find_iter(text).map(|found| found.range()));
    }
    if let Some(telegram) = TELEGRAM_BOT_TOKEN.as_ref() {
        #[cfg(test)]
        record_redaction_work(text.len());
        for token in telegram.captures_iter(text).filter_map(|captures| captures.get(1)) {
            #[cfg(test)]
            record_redaction_work(token.len() + 2);
            let before = text[..token.start()].chars().next_back();
            let after = text[token.end()..].chars().next();
            if !before.is_some_and(|c| c.is_ascii_digit())
                && !after.is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
            {
                spans.push(token.range());
            }
        }
    }
    prefixed_token_spans(text, &mut spans);
    // Sorting and merging: proportional to the number of spans (times its
    // logarithm for the sort, counted as the bit length).
    #[cfg(test)]
    record_redaction_work(spans.len() * (usize::try_from(usize::BITS - spans.len().leading_zeros()).unwrap_or(0) + 1));
    spans.retain(|span| !span.is_empty());
    spans.sort_by_key(|span| (span.start, std::cmp::Reverse(span.end)));
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

// Bytes the redactor examined on this thread: a deterministic work count, so
// a test can prove a redaction linear, or bounded, without a clock (R-C2BUk
// K-5). Every scan records what it reads, and the output pass what it copies.
#[cfg(test)]
thread_local! {
    static REDACTION_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_redaction_work(bytes: usize) {
    REDACTION_WORK.with(|work| work.set(work.get().saturating_add(bytes)));
}

/// The bytes the redactor examines while `run` runs on this thread.
#[cfg(test)]
pub(crate) fn redaction_work<R>(run: impl FnOnce() -> R) -> usize {
    REDACTION_WORK.with(|work| work.set(0));
    std::hint::black_box(run());
    REDACTION_WORK.with(std::cell::Cell::get)
}

/// Redact every secret in `text`, showing each detected secret as `present`
/// returns it. URL userinfo is always shown as `[redacted]`.
#[must_use]
pub fn redact_secrets_with(text: &str, present: impl Fn(&str) -> String) -> String {
    let text = crate::providers::redact_endpoint_userinfo(text);
    let spans = find_secrets(&text);
    if spans.is_empty() {
        return text.into_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = 0usize;
    for span in spans {
        let presented = present(&text[span.clone()]);
        // What the output pass copies: the gap, the secret it presents and
        // the presentation.
        #[cfg(test)]
        record_redaction_work(span.end - rest + presented.len());
        out.push_str(&text[rest..span.start]);
        out.push_str(&presented);
        rest = span.end;
    }
    #[cfg(test)]
    record_redaction_work(text.len() - rest);
    out.push_str(&text[rest..]);
    out
}

/// What a redacted URL query value, valueless query parameter or fragment
/// reads as.
pub const REDACTED_URL_PART: &str = "[redacted]";

/// Characters that end a URL in free text.
const fn ends_url(character: char) -> bool {
    character.is_whitespace() || matches!(character, '\'' | '"' | '`' | '<' | '>' | '(' | ')' | ',')
}

/// Redact the parts of every URL in `text` that can carry a credential and
/// that no parameter name reliably announces (D-088 C-6): URL userinfo
/// (`[redacted]@`), the value of every query parameter whatever its name
/// (`?sig=[redacted]&region=[redacted]`), a query parameter without a value
/// (`?[redacted]`), and the fragment (`#[redacted]`). The scheme, host and
/// path are kept, so the endpoint stays recognisable. A URL is a run of
/// text without whitespace, quotes, backticks, angle brackets, parentheses
/// or commas that contains `://`. Idempotent: redacted text passes through
/// unchanged. This is the one URL redactor for plans, history, Gateway
/// bodies, `GET /api/config` and CLI listings.
#[must_use]
pub fn redact_url_components(text: &str) -> std::borrow::Cow<'_, str> {
    let userinfo_free = crate::providers::redact_endpoint_userinfo(text);
    let source = userinfo_free.as_ref();
    let mut out = String::new();
    let mut changed = false;
    let mut rest = source;
    while !rest.is_empty() {
        let token_len = rest.find(ends_url).unwrap_or(rest.len());
        let token_len = if token_len == 0 {
            rest.chars().next().map_or(rest.len(), char::len_utf8)
        } else {
            token_len
        };
        let (token, tail) = rest.split_at(token_len);
        match redact_url_token(token) {
            Some(redacted) => {
                changed |= redacted != token;
                out.push_str(&redacted);
            }
            None => out.push_str(token),
        }
        rest = tail;
    }
    if changed {
        std::borrow::Cow::Owned(out)
    } else {
        userinfo_free
    }
}

/// The query and fragment of one URL token redacted; `None` when the token
/// is not a URL.
fn redact_url_token(token: &str) -> Option<String> {
    let authority = token.find("://")? + 3;
    let after = token.get(authority..)?;
    let cut = after.find(['?', '#'])?;
    let (head, parts) = token.split_at(authority + cut);
    let (query, fragment) = parts.strip_prefix('?').map_or_else(
        || (None, parts.strip_prefix('#')),
        |query| {
            query
                .split_once('#')
                .map_or((Some(query), None), |(query, fragment)| (Some(query), Some(fragment)))
        },
    );
    let mut out = head.to_string();
    if let Some(query) = query {
        out.push('?');
        let redacted = query
            .split('&')
            .map(|parameter| match parameter.split_once('=') {
                Some((name, _)) if is_plain_parameter_name(name) => format!("{name}={REDACTED_URL_PART}"),
                Some(_) => format!("{REDACTED_URL_PART}={REDACTED_URL_PART}"),
                None if parameter.is_empty() => String::new(),
                None => REDACTED_URL_PART.to_string(),
            })
            .collect::<Vec<_>>()
            .join("&");
        out.push_str(&redacted);
    }
    if let Some(fragment) = fragment {
        out.push('#');
        if !fragment.is_empty() {
            out.push_str(REDACTED_URL_PART);
        }
    }
    Some(out)
}

/// Whether a query parameter name reads like a name rather than a value,
/// so it is shown (`sig`, `api_key`, `region`): 1 to 32 characters, a
/// letter first, then letters, digits, `_` or `.`, at most three digits.
/// Anything else (`sk-live-x`, a long token, a percent-encoded name) is
/// redacted with the value (D-095 N-2).
fn is_plain_parameter_name(name: &str) -> bool {
    let digits = name.chars().filter(char::is_ascii_digit).count();
    (1..=32).contains(&name.len())
        && name.chars().next().is_some_and(|first| first.is_ascii_alphabetic())
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '.'))
        && digits <= 3
}

/// [`redact_url_components`] for a value that is one URL as a whole (a
/// configured provider or endpoint, a URL-keyed table key), not free text:
/// the whole value is the URL, so a query value holding `,`, `(`, `)` or a
/// quote is redacted to its end instead of being cut there (D-095 N-2).
/// Values without `://` are returned with only their userinfo redacted.
#[must_use]
pub fn redact_url_value(value: &str) -> std::borrow::Cow<'_, str> {
    let userinfo_free = crate::providers::redact_endpoint_userinfo(value);
    match redact_url_token(&userinfo_free) {
        Some(redacted) if redacted != userinfo_free.as_ref() => std::borrow::Cow::Owned(redacted),
        _ => userinfo_free,
    }
}

/// Redact what a provider spelling, or a message that names one, may
/// carry: the secrets this module recognises, then every URL's userinfo,
/// query values, valueless parameters and fragment
/// ([`redact_url_components`]). The one rendering for provider text on
/// every surface: refusals, statuses, reports, listings and logs (D-085
/// F-10, D-088 C-6, D-095 N-1).
///
/// A secret recognised inside a URL (a token in its path) reads as
/// [`REDACTED_URL_PART`], the one URL mask, so a redacted URL sent back as
/// a value is recognised by [`holds_redacted_url_part`] and refused at the
/// write boundary instead of being stored (D-099 7c).
#[must_use]
pub fn redact_provider_text(text: &str) -> String {
    let secrets_free = redact_secrets(text);
    let url_masked = url_tokens_with_url_mask(&secrets_free);
    redact_url_components(&url_masked).into_owned()
}

/// `text` with [`REDACTED`] replaced by [`REDACTED_URL_PART`] inside every
/// URL token (a run of text holding `://`, delimited as
/// [`redact_url_components`] delimits it).
fn url_tokens_with_url_mask(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains(REDACTED) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let token_len = rest.find(ends_url).unwrap_or(rest.len());
        let token_len = if token_len == 0 {
            rest.chars().next().map_or(rest.len(), char::len_utf8)
        } else {
            token_len
        };
        let (token, tail) = rest.split_at(token_len);
        if token.contains("://") {
            out.push_str(&token.replace(REDACTED, REDACTED_URL_PART));
        } else {
            out.push_str(token);
        }
        rest = tail;
    }
    std::borrow::Cow::Owned(out)
}

/// Whether `text` holds a URL part a redactor replaced (a redacted query
/// value, valueless parameter, fragment or path secret, or the userinfo
/// placeholder), or the free-text mask [`REDACTED`] inside a URL: a value
/// shown redacted, never a real one, which the write boundary refuses to
/// store (D-099 7c).
#[must_use]
pub fn holds_redacted_url_part(text: &str) -> bool {
    text.contains(REDACTED_URL_PART) || (text.contains("://") && text.contains(REDACTED))
}

/// Redact every secret this module recognises in `text`, and nothing else.
#[must_use]
pub fn redact_secrets(text: &str) -> String {
    redact_secrets_with(text, |_| REDACTED.to_string())
}

/// Redact every string in a JSON value, and replace the value of every
/// credential-named key, whatever its type (D-054 amendment 3). `null` stays
/// `null`: it holds no secret. Never truncates.
pub fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object {
                let secret = match child {
                    serde_json::Value::Null => false,
                    serde_json::Value::String(text) => is_credential_value(key, text),
                    _ => is_credential_key(key),
                };
                if secret {
                    *child = serde_json::Value::String(REDACTED.to_string());
                } else {
                    redact_json_value(child);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_json_value),
        serde_json::Value::String(text) => *text = redact_secrets(text),
        _ => {}
    }
}

/// [`redact_json_value`] applied to a serializable value, keeping its type.
///
/// # Errors
/// When `value` does not round-trip through JSON, which includes a
/// credential-named field of a non-string type: redaction replaces its value.
pub fn redact_json_structure<T>(value: &T) -> anyhow::Result<T>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let mut json = serde_json::to_value(value)?;
    redact_json_value(&mut json);
    Ok(serde_json::from_value(json)?)
}

#[cfg(test)]
mod tests {
    /// RED before PMM-H (D-099 7c): a secret inside a URL path was shown as
    /// `[REDACTED]`, which `holds_redacted_url_part` did not recognise.
    #[test]
    fn a_secret_in_a_url_path_reads_as_the_url_mask() {
        let token = "sk-proj-abcdefghijklmnopqrstuvwxyz0123";
        let shown = redact_provider_text(&format!("custom:https://llm.example/v1/{token}/chat"));
        assert!(!shown.contains(token), "{shown}");
        assert!(
            shown.contains(REDACTED_URL_PART) && !shown.contains(REDACTED),
            "{shown}"
        );
        assert!(holds_redacted_url_part(&shown), "{shown}");
        assert!(holds_redacted_url_part("https://h/v1/[REDACTED]/x"));
        assert!(!holds_redacted_url_part("[REDACTED] in free text"));
        // Outside a URL the free-text mask stays.
        let free_text = redact_provider_text(&format!("key {token}"));
        assert!(
            free_text.contains(REDACTED) && !free_text.contains(token),
            "{free_text}"
        );
    }

    /// D-088 C-6: every query value (any parameter name), a valueless
    /// parameter and the fragment are redacted; the endpoint stays readable.
    #[test]
    fn url_components_are_redacted_whatever_the_parameter_name() {
        let cases = [
            (
                "custom:https://h.example/v1?sig=abc&region=eu",
                "custom:https://h.example/v1?sig=[redacted]&region=[redacted]",
            ),
            (
                "https://h.example/mcp?sk-bare-token",
                "https://h.example/mcp?[redacted]",
            ),
            ("https://h.example/mcp#frag-token", "https://h.example/mcp#[redacted]"),
            (
                "custom:http://user:pw@h.example/v1?key%3Dv=1#x",
                "custom:http://[redacted]@h.example/v1?[redacted]=[redacted]#[redacted]",
            ),
            (
                "provider 'custom:https://h/v1?t=1' refused, see https://docs.example/a?b=c.",
                "provider 'custom:https://h/v1?t=[redacted]' refused, see https://docs.example/a?b=[redacted]",
            ),
            ("openai", "openai"),
            ("what? no url here", "what? no url here"),
        ];
        for (input, expected) in cases {
            let redacted = redact_url_components(input);
            assert!(
                redacted.starts_with(expected) || redacted == expected,
                "{input} -> {redacted}"
            );
            assert_eq!(redact_url_components(&redacted), redacted, "idempotent: {redacted}");
        }
        // D-095 N-2: a name that reads like a token is redacted with its
        // value; a structured value is redacted to its end.
        assert_eq!(
            redact_url_components("https://h/v1?sk-live-abc=1&region=eu"),
            "https://h/v1?[redacted]=[redacted]&region=[redacted]"
        );
        assert_eq!(
            redact_url_value("custom:https://h/v1?sig=a,b(c)'d"),
            "custom:https://h/v1?sig=[redacted]"
        );
        assert_eq!(redact_url_value("openai"), "openai");
        assert!(holds_redacted_url_part("https://h/v1?x=[redacted]"));
        assert!(holds_redacted_url_part("http://[redacted]@h/v1"));
        assert!(!holds_redacted_url_part("https://h/v1?x=1"));
    }

    use super::*;

    fn assert_redacts(input: &str, secret: &str) {
        let output = redact_secrets(input);
        assert!(!output.contains(secret), "{input:?} kept {secret:?}: {output:?}");
        assert!(output.contains(REDACTED), "{input:?} -> {output:?}");
    }

    #[test]
    fn url_userinfo_is_redacted() {
        assert_eq!(
            redact_secrets("POST http://alice:hunter2pw@10.0.0.1/v1 failed"),
            "POST http://[redacted]@10.0.0.1/v1 failed"
        );
    }

    #[test]
    fn keyed_api_key_keeps_its_key() {
        let output = redact_secrets("api_key: sk1234567890abcdef");
        assert_eq!(output, "api_key: [REDACTED]");
    }

    #[test]
    fn keyed_api_secret_auth_and_access_tokens_are_redacted() {
        assert_redacts("api-secret=abcdefgh12345678", "abcdefgh12345678");
        assert_redacts("auth_token = zyxwvuts98765432", "zyxwvuts98765432");
        assert_redacts("ACCESS-TOKEN:qwertyuiop123456", "qwertyuiop123456");
        assert_redacts("bearer=mnbvcxzlkj654321", "mnbvcxzlkj654321");
    }

    #[test]
    fn keyed_passwords_are_redacted() {
        assert_redacts("password=hunter22", "hunter22");
        assert_redacts("passwd: s3cr3t!", "s3cr3t!");
        assert_redacts("PWD=abcd", "abcd");
    }

    #[test]
    fn json_password_fields_are_redacted() {
        assert_redacts(r#"{"password": "open sesame"}"#, "open sesame");
    }

    #[test]
    fn authorization_bearer_headers_keep_the_scheme() {
        assert_eq!(
            redact_secrets("Authorization: Bearer abc.def.ghi"),
            "Authorization: Bearer [REDACTED]"
        );
    }

    #[test]
    fn every_token_prefix_is_redacted() {
        for token in [
            "sk-abc123456789012345678901",
            "sk-proj-1234567890abcdefghijklmnop",
            "sk-ant-api03-abcdefghijklmnopqrst",
            "xoxb-1234-5678-abcdef",
            "xoxp-1234-5678-abcdef",
            "ghp_abc123def456",
            "gho_1234567890abcdef",
            "ghu_sessiontoken123",
            "github_pat_11AABBC_xyzzy789",
            "glpat-abcdefghij0123456789",
        ] {
            assert_eq!(
                redact_secrets(&format!("failed with {token} here")),
                "failed with [REDACTED] here"
            );
        }
    }

    /// The audit log's and the tool diagnostics' key vocabulary (REDACT-2).
    #[test]
    fn keyed_generic_credential_names_are_redacted() {
        for (input, secret) in [
            ("deploy --token=yyysecret", "yyysecret"),
            ("secret: s3cr3tvalue", "s3cr3tvalue"),
            ("key=ABCDEF0123456789", "ABCDEF0123456789"),
            ("auth=abcdefgh1234", "abcdefgh1234"),
            ("credentials: 'hunter2hunter2'", "hunter2hunter2"),
            ("user_key=\"0123456789ab\"", "0123456789ab"),
            ("private-key: aabbccddeeff", "aabbccddeeff"),
            ("refresh_token=rt_0123456789", "rt_0123456789"),
            ("client_secret: cs-abcdefghij", "cs-abcdefghij"),
        ] {
            assert_redacts(input, secret);
        }
    }

    #[test]
    fn quoted_values_keep_their_quotes() {
        assert_eq!(redact_secrets(r#"password="secret123456""#), r#"password="[REDACTED]""#);
        assert_eq!(redact_secrets("token='abcdefghijkl'"), "token='[REDACTED]'");
        assert_eq!(
            redact_secrets(r#"{"api_key": "sk1234567890ab"}"#),
            r#"{"api_key": "[REDACTED]"}"#
        );
    }

    #[test]
    fn bare_bearer_tokens_are_redacted() {
        assert_eq!(
            redact_secrets("curl -H 'Bearer abcdefghijklmnop' x"),
            "curl -H 'Bearer [REDACTED]' x"
        );
    }

    /// D-054 amendment 2: a value under a credential key is a secret at any
    /// non-empty length.
    #[test]
    fn short_keyed_secrets_are_redacted() {
        assert_eq!(redact_secrets(r#"api_key="short""#), r#"api_key="[REDACTED]""#);
        assert_eq!(redact_secrets("token: abc"), "token: [REDACTED]");
        assert_eq!(redact_secrets("deploy --token=y"), "deploy --token=[REDACTED]");
        assert_eq!(redact_secrets("PWD=x"), "PWD=[REDACTED]");
        assert_eq!(redact_secrets("bearer abc"), "bearer [REDACTED]");
        assert_eq!(redact_secrets("authToken: t1"), "authToken: [REDACTED]");
    }

    /// D-054 amendment 2: key names match by whole segments.
    #[test]
    fn credential_key_names_match_by_whole_segments() {
        for key in [
            "token",
            "access_token",
            "authToken",
            "client_secret",
            "API_KEY",
            "APIKey",
            "x-api-key",
            "private.key",
            "passwordHash",
            "auth",
            "key",
        ] {
            assert!(is_credential_key(key), "{key} is a credential name");
        }
        for key in [
            "tokenizer",
            "secretary",
            "keyboard",
            "author",
            "tokens_used",
            "sort_key",
        ] {
            assert!(!is_credential_key(key), "{key} is not a credential name");
        }
    }

    #[test]
    fn values_under_other_names_are_kept() {
        for text in [
            "tokenizer: cl100k",
            "secretary=alice",
            "the keyboard: fine",
            "Authorization: Bearer",
        ] {
            assert_eq!(redact_secrets(text), text);
        }
    }

    /// D-054 amendment 4 (R-LOOPD5 R-1): a name ending in `key`, as a segment
    /// or run together, is a credential unless its stem is on the non-secret
    /// list; a `session_key` value is a secret only when it looks like one.
    #[test]
    fn every_key_name_is_a_credential_unless_its_stem_is_not_secret() {
        for name in [
            "hmac_key",
            "jwt_key",
            "app_key",
            "consumer_key",
            "deploy_key",
            "license_key",
            "webhook_key",
            "openai_key",
            "hmacKey",
            "licensekey",
            "monkey",
        ] {
            let text = format!("{name}=abcdefgh12345678");
            assert_eq!(redact_secrets(&text), format!("{name}=[REDACTED]"), "{name} in text");
            let mut json = serde_json::json!({ name: "abcdefgh12345678" });
            redact_json_value(&mut json);
            assert_eq!(
                json.get(name).and_then(serde_json::Value::as_str),
                Some(REDACTED),
                "{name} in JSON"
            );
        }
        for name in [
            "sort_key",
            "primary_key",
            "public_key",
            "cache_key",
            "idempotency_key",
            "primarykey",
            "partition_key",
        ] {
            let text = format!("{name}=abcdefgh12345678");
            assert_eq!(redact_secrets(&text), text, "{name} in text");
            let mut json = serde_json::json!({ name: "abcdefgh12345678" });
            redact_json_value(&mut json);
            assert_eq!(
                json.get(name).and_then(serde_json::Value::as_str),
                Some("abcdefgh12345678"),
                "{name} in JSON"
            );
        }
        // a session key is an identifier, unless its value looks like a secret
        for kept in [
            "session_key=telegram_sender-1",
            "session_key=delegate:telegram:chat-1:alice",
            r#"{"session_key": "0b8c7f6e-2d4a-4c1b-9e3f-5a6b7c8d9e0f"}"#,
        ] {
            assert_eq!(redact_secrets(kept), kept);
        }
        assert_eq!(
            redact_secrets("session_key=Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MA=="),
            "session_key=[REDACTED]"
        );
    }

    /// D-054 amendment 3 (R-LOOPD5 H-1): every credential name a pre-program
    /// detector recognised (chat's persistence sanitizer, the audit log, the
    /// tool diagnostics and their JSON key sets, at `9b3d2a90`) is still one,
    /// run-together forms included. The audit log's unanchored `…key` suffix
    /// is kept by D-054 amendment 4 except for non-secret stems (`sort_key`);
    /// see `every_key_name_is_a_credential_unless_its_stem_is_not_secret`.
    const PRE_PROGRAM_CREDENTIAL_NAMES: &[&str] = &[
        // chat: api[_-]?key|api[_-]?secret|auth[_-]?token|access[_-]?token|bearer, password|passwd|pwd (unanchored)
        "api_key",
        "api-key",
        "apikey",
        "API_KEY",
        "x_api_key",
        "api_secret",
        "apisecret",
        "APISECRET",
        "auth_token",
        "authtoken",
        "AUTHTOKEN",
        "authToken",
        "access_token",
        "accesstoken",
        "ACCESSTOKEN",
        "bearer",
        "password",
        "passwd",
        "pwd",
        "PGPASSWORD",
        "dbpassword",
        "mypassword",
        "password_hash",
        // audit: (token|key|secret|password|passwd|api[_-]?key|auth) as a suffix
        "token",
        "mytoken",
        "apitoken",
        "githubtoken",
        "csrftoken",
        "secret",
        "clientsecret",
        "secretkey",
        "SECRETKEY",
        "auth_key",
        "authKey",
        "ssh_key",
        "sshkey",
        "encryption_key",
        "masterkey",
        "x-auth",
        "oauth",
        "basic_auth",
        // diagnostics: token|api[_-]?key|password|secret|user[_-]?key|bearer|credential
        "user_key",
        "userkey",
        "credential",
        "credentials",
        "client_secret",
        "secret_key",
        // JSON key sets (chat substrings, diagnostics exact names)
        "refresh_token",
        "refreshtoken",
        "private_key",
        "privatekey",
        "signing_key",
        "signingkey",
        "auth_header",
        "authheader",
        "authorization",
        "github_token_value",
    ];

    #[test]
    fn every_pre_program_credential_name_is_still_caught() {
        for name in PRE_PROGRAM_CREDENTIAL_NAMES {
            let text = format!("{name}=abcdefgh12345678");
            assert_eq!(redact_secrets(&text), format!("{name}=[REDACTED]"), "{name} in text");
            let mut json = serde_json::json!({ *name: "abcdefgh12345678" });
            redact_json_value(&mut json);
            assert_eq!(
                json.get(*name).and_then(serde_json::Value::as_str),
                Some(REDACTED),
                "{name} as a JSON key"
            );
        }
        // the bare names in the forms that keep them (JSON keys, `=`)
        for text in [
            "key=abcdefgh1234",
            "--key=abcdefgh1234",
            "auth=abcdefgh1234",
            r#"{"key": "abcdefgh1234"}"#,
        ] {
            assert!(!redact_secrets(text).contains("abcdefgh1234"), "{text}");
        }
    }

    #[test]
    fn credential_json_values_of_any_type_are_replaced() {
        let mut value = serde_json::json!({
            "password": 12_345_678,
            "credentials": { "user": "a", "pass": "b" },
            "token": ["t1", "t2"],
            "secret": true,
            "api_key": null,
            "max_tokens": 4096,
            "tokenizer": "cl100k"
        });
        redact_json_value(&mut value);
        for key in ["password", "credentials", "token", "secret"] {
            assert_eq!(
                value.get(key).and_then(serde_json::Value::as_str),
                Some(REDACTED),
                "{key}"
            );
        }
        assert!(
            value.get("api_key").is_some_and(serde_json::Value::is_null),
            "null holds no secret"
        );
        assert_eq!(value.get("max_tokens").and_then(serde_json::Value::as_u64), Some(4096));
        assert_eq!(
            value.get("tokenizer").and_then(serde_json::Value::as_str),
            Some("cl100k")
        );
    }

    /// Typed round-trips through the redactor keep working: none of these
    /// structures has a credential-named non-string field.
    #[test]
    fn provider_outcomes_and_route_decisions_round_trip() {
        let decision = crate::llm::route_decision::RouteDecision::single_candidate("provider", "model");
        let outcome = crate::llm::route_decision::ProviderExecutionOutcome::failed_for_decision(
            &decision,
            chrono::Utc::now(),
            &anyhow::anyhow!("rejected key AKIAIOSFODNN7EXAMPLE"),
        );
        let redacted = redact_json_structure(&outcome).expect("the outcome round-trips");
        assert!(!format!("{redacted:?}").contains("AKIAIOSFODNN7EXAMPLE"));
        redact_json_structure(&decision).expect("the route decision round-trips");
    }

    /// R-LOOPD5 H-2: bare `key` / `auth` in prose or YAML, and words that only
    /// look like a keyless token, are kept.
    #[test]
    fn prose_yaml_and_token_lookalikes_are_kept() {
        for text in [
            "key: value",
            "the primary key: id",
            "auth: required",
            "sk-learn is a library",
            "pip install sk-learn",
            "primary_key=user_id",
        ] {
            assert_eq!(redact_secrets(text), text);
        }
    }

    /// R-LOOPD5 H-3: the credential after any `Authorization` scheme.
    #[test]
    fn every_authorization_scheme_hides_its_credential() {
        for (text, secret) in [
            ("Authorization: Bot MTIzNDU2Nzg5.abc.def", "MTIzNDU2Nzg5.abc.def"),
            ("Authorization: SSWS 00abcdefghij", "00abcdefghij"),
            ("authorization: ApiKey x1", "x1"),
            ("Proxy-Authorization: Basic dXNlcjpwYXNz", "dXNlcjpwYXNz"),
            (r#"{"Authorization": "Negotiate YIIabc"}"#, "YIIabc"),
            (
                "Authorization: AWS4-HMAC-SHA256 Credential=AKID/2026/s3/aws4_request, Signature=abcdef0123",
                "Signature=abcdef0123",
            ),
        ] {
            let redacted = redact_secrets(text);
            assert!(!redacted.contains(secret), "{text} -> {redacted}");
            assert!(redacted.contains(REDACTED), "{text} -> {redacted}");
        }
        assert_eq!(
            redact_secrets("Authorization: Bearer x"),
            "Authorization: Bearer [REDACTED]"
        );
    }

    /// R-LOOPD5 H-4: formats none of the pre-program detectors knew.
    #[test]
    fn stripe_google_jwt_pem_and_telegram_secrets_are_redacted() {
        for (text, secret) in [
            (
                "stripe sk_live_4eC39HqLyjWDarjtT1zdp7dc",
                "sk_live_4eC39HqLyjWDarjtT1zdp7dc",
            ),
            ("restricted rk_live_51H0abcdefghijkl", "rk_live_51H0abcdefghijkl"),
            (
                "maps AIzaSyA1234567890abcdefghijklmnopqrstuv",
                "AIzaSyA1234567890abcdefghijklmnopqrstuv",
            ),
            (
                "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
                "dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
            ),
            (
                "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEAuHidden\n-----END RSA PRIVATE KEY-----",
                "MIIEowIBAAKCAQEAuHidden",
            ),
            (
                "bot token 123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsawA",
                "AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsawA",
            ),
            (
                "error sending request for url (https://api.telegram.org/bot123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsawA/sendMessage)",
                "AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsawA",
            ),
            ("temporary ASIAIOSFODNN7EXAMPLE", "ASIAIOSFODNN7EXAMPLE"),
        ] {
            let redacted = redact_secrets(text);
            assert!(!redacted.contains(secret), "{text} -> {redacted}");
            assert!(redacted.contains(REDACTED), "{text} -> {redacted}");
        }
    }

    #[test]
    fn find_secrets_returns_merged_value_spans() {
        let text = "API_KEY=sk-1234567890abcdef ok";
        let spans = find_secrets(text);
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert_eq!(
            spans.first().and_then(|span| text.get(span.clone())),
            Some("sk-1234567890abcdef")
        );
    }

    #[test]
    fn a_presentation_wrapper_chooses_how_a_secret_is_shown() {
        let shown = redact_secrets_with("token: 1234567890 and AKIAIOSFODNN7EXAMPLE", |secret| {
            format!("{}*{REDACTED}", secret.get(..4).unwrap_or_default())
        });
        assert_eq!(shown, "token: 1234*[REDACTED] and AKIA*[REDACTED]");
    }

    #[test]
    fn aws_access_key_ids_are_redacted() {
        assert_redacts("key AKIAIOSFODNN7EXAMPLE rejected", "AKIAIOSFODNN7EXAMPLE");
    }

    #[test]
    fn text_without_secrets_is_unchanged() {
        for text in [
            "simple upstream timeout",
            "the desk-top task-sk-1 stays",
            "a password prompt",
            "github_pat_",
        ] {
            assert_eq!(redact_secrets(text), text);
        }
    }

    #[test]
    fn redaction_never_truncates() {
        let long = format!("{} AKIAIOSFODNN7EXAMPLE", "x".repeat(64 * 1024));
        let output = redact_secrets(&long);
        assert!(output.starts_with(&"x".repeat(64 * 1024)));
        assert!(output.ends_with(REDACTED));
    }

    #[test]
    fn json_values_are_redacted_by_content_and_by_key() {
        let mut value = serde_json::json!({
            "error": "rejected key AKIAIOSFODNN7EXAMPLE",
            "nested": [{ "api_key": "short", "apikey": "a", "credentials": "b", "signing_key": "c", "tokenizer": "cl100k" }],
            "count": 3
        });
        redact_json_value(&mut value);
        assert_eq!(
            value.pointer("/error").and_then(serde_json::Value::as_str),
            Some("rejected key [REDACTED]")
        );
        assert_eq!(
            value.pointer("/nested/0/api_key").and_then(serde_json::Value::as_str),
            Some(REDACTED)
        );
        for key in ["apikey", "credentials", "signing_key"] {
            assert_eq!(
                value
                    .pointer(&format!("/nested/0/{key}"))
                    .and_then(serde_json::Value::as_str),
                Some(REDACTED),
                "{key}"
            );
        }
        assert_eq!(
            value.pointer("/nested/0/tokenizer").and_then(serde_json::Value::as_str),
            Some("cl100k"),
            "a tokenizer is not a credential"
        );
        assert_eq!(value.pointer("/count").and_then(serde_json::Value::as_u64), Some(3));
    }

    /// R-C2BUk K-5 guard: the one redactor is linear on repeated short token
    /// prefixes and on dense real tokens. The provider scrubber it replaced
    /// rebuilt the text per match (`replace_range`), about 1.5 s for 1 MB of
    /// `sk-a `. Measured as the bytes it examines, not by a clock: the work
    /// per input byte on a 1 MB text may exceed that on a 100 KB text by at
    /// most 10% (a quadratic scan grows it tenfold).
    #[test]
    fn redaction_work_is_linear_on_dense_token_prefixes() {
        for unit in ["sk-a ", "sk-abcdef123 ", "xoxb-1 sk-proj-abcdefgh "] {
            let small = unit.repeat(100 * 1024 / unit.len());
            let large = unit.repeat(1024 * 1024 / unit.len());
            let small_work = redaction_work(|| redact_secrets(&small));
            let large_work = redaction_work(|| redact_secrets(&large));
            assert!(
                large_work * small.len() * 10 <= small_work * large.len() * 11,
                "{unit:?}: {} bytes cost {small_work}, {} bytes cost {large_work}",
                small.len(),
                large.len()
            );
        }
    }
}

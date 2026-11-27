//! Credential-name classification and the value shapes it falls back on.
//!
//! [`classify_name`] decides from a pair's name alone whether its value is
//! a secret ([`KeyName::Credential`]), never one ([`KeyName::Plain`]), or one
//! only when the value has a credential's shape: [`looks_like_a_secret`] for
//! `session…key` names and pagination, cursor and count names,
//! [`looks_like_a_key_value`] for the bare name `key`.

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
/// together (`masterkey`, `authkey`), is a credential name.
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

/// English words that end in `key` without naming one: the only run-together
/// `…key` segments that are not credential names. Every other run-together
/// `…key` (`authkey`, `secretkey`, `signingkey`, ...) is one (fail closed).
const KEY_ENDING_WORDS: [&str; 8] = [
    "monkey", "hotkey", "turkey", "donkey", "whiskey", "hockey", "jockey", "turnkey",
];

/// Segments that, next to a `token` or `cursor` word, make a pagination-looking
/// name a credential name again (`sync_auth_token`, `page_access_token`).
const CREDENTIAL_CONTEXT_WORDS: [&str; 8] = ["auth", "access", "refresh", "id", "api", "session", "csrf", "xsrf"];

/// Segments that, next to a `token` word, name a pagination token, a count or
/// a budget rather than a credential (`next_page_token`, `token_count`).
const PAGINATION_WORDS: [&str; 7] = ["next", "page", "continuation", "sync", "count", "limit", "budget"];

/// The stem whose key is a credential only when its value looks like one
/// ([`looks_like_a_secret`]): a PRX session key (`telegram_sender-1`,
/// `delegate:telegram:chat-1:alice`) is an identifier, a session secret is not.
const SESSION_KEY_STEM: &str = "session";

/// How a name classifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyName {
    Credential,
    CredentialIfSecretShaped,
    /// The bare name `key`: a credential only when its value looks like a key
    /// ([`looks_like_a_key_value`]).
    CredentialIfKeyShaped,
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

/// Whether a value is a canonical UUID (`8-4-4-4-12` hex digits).
fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.char_indices().all(|(index, c)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// Whether a value has the shape of a secret rather than an identifier: 24+
/// characters of a token alphabet (letters, digits, `+`, `/`, `=`, `-`), with
/// both letters and digits, and not a canonical UUID.
pub(super) fn looks_like_a_secret(value: &str) -> bool {
    value.len() >= 24
        && !is_uuid(value)
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-'))
        && value.chars().any(|c| c.is_ascii_digit())
        && value.chars().any(|c| c.is_ascii_alphabetic())
}

/// The shortest value under the bare name `key` that can be a credential.
pub(super) const KEY_VALUE_MIN_LEN: usize = 16;

/// The shortest run of letters and digits a key value must hold.
pub(super) const KEY_VALUE_MIN_RUN: usize = 8;

/// Whether a byte belongs to the alphabet of a value under the bare `key`.
pub(super) const fn is_key_value_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'_' | b'.' | b'-')
}

/// Whether the value under the bare name `key` looks like a credential: 16+
/// characters of `[A-Za-z0-9+/=_.-]`, not starting with `/` or `.` (a path),
/// holding a run of 8+ letters and digits with both a letter and a digit, and
/// not a canonical UUID (a list or record key). `ABCDEF0123456789`,
/// `SG.<id>.<secret>` and `hf_<secret>` are keys; `PRX-58`, `photos/a.jpg`,
/// `user_profile_v2` and `sort` are not.
pub(super) fn looks_like_a_key_value(value: &str) -> bool {
    if value.len() < KEY_VALUE_MIN_LEN
        || value.starts_with(['/', '.'])
        || is_uuid(value)
        || !value.bytes().all(is_key_value_byte)
    {
        return false;
    }
    let mut run = 0usize;
    let mut letter = false;
    let mut digit = false;
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() {
            run += 1;
            letter |= byte.is_ascii_alphabetic();
            digit |= byte.is_ascii_digit();
            if run >= KEY_VALUE_MIN_RUN && letter && digit {
                return true;
            }
        } else {
            run = 0;
            letter = false;
            digit = false;
        }
    }
    false
}

/// A key's segments, lower-cased: split on `_`, `-`, `.`, `:` and camelCase
/// (`authToken` -> `auth`, `token`; `APIKey` -> `api`, `key`).
fn key_segments(key: &str) -> Vec<String> {
    let mut segments = Vec::new();
    for part in key.split(['_', '-', '.', ':']).filter(|part| !part.is_empty()) {
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

/// How the `…key` compounds of a name classify (D-054 amendment 4): a `key`
/// word after another segment (`hmac_key`, `APIKey`) or a run-together
/// `…key` segment (`masterkey`, `authkey`), each by its stem; the strongest
/// classification among them wins. A run-together English word (`monkey`,
/// `hotkey`) is not a compound.
fn key_compound_class(segments: &[String]) -> KeyName {
    let mut class = KeyName::Plain;
    for (index, segment) in segments.iter().enumerate() {
        let stem = if segment == "key" {
            index
                .checked_sub(1)
                .and_then(|previous| segments.get(previous))
                .map(String::as_str)
        } else if KEY_ENDING_WORDS.contains(&segment.as_str()) {
            None
        } else {
            segment.strip_suffix("key").filter(|stem| !stem.is_empty())
        };
        match stem.map(key_stem_class) {
            Some(KeyName::Credential) => return KeyName::Credential,
            Some(KeyName::CredentialIfSecretShaped) => class = KeyName::CredentialIfSecretShaped,
            Some(KeyName::CredentialIfKeyShaped | KeyName::Plain) | None => {}
        }
    }
    class
}

/// How `key` classifies by its words, endings and `…key` compounds (the bare
/// names `key` and `auth` are decided by where they appear).
pub(super) fn classify_name(key: &str) -> KeyName {
    let segments = key_segments(key);
    if matches!(segments.as_slice(), [only] if only == "key") {
        return KeyName::CredentialIfKeyShaped;
    }
    let compounds = key_compound_class(&segments);
    // Pagination tokens, count and budget names and cursors carry no
    // credential unless the value looks like one (CE-64 follow-up), and only
    // while nothing else in the name is a credential word or compound:
    // `sync_auth_token`, `cursor_secret` and `CURSOR_API_KEY` stay credential
    // names. The `token` a pagination word downgrades is not counted.
    let pagination_token = segments.iter().any(|segment| segment == "token")
        && segments
            .iter()
            .any(|segment| PAGINATION_WORDS.contains(&segment.as_str()));
    let cursor = segments.iter().any(|segment| segment == "cursor");
    if (pagination_token || cursor) && compounds != KeyName::Credential {
        let other_credential_word = segments.iter().any(|segment| {
            !(pagination_token && segment == "token")
                && (is_credential_segment(segment) || CREDENTIAL_CONTEXT_WORDS.contains(&segment.as_str()))
        });
        if !other_credential_word {
            return KeyName::CredentialIfSecretShaped;
        }
    }
    let by_words = segments.iter().any(|segment| is_credential_segment(segment))
        || segments
            .windows(2)
            .any(|pair| matches!(pair, [left, right] if left == "auth" && right == "header"))
        // `x-auth`, `basic_auth`, `proxy_auth`: an `auth` segment after another
        || (segments.len() > 1 && segments.last().is_some_and(|last| last == "auth"));
    if by_words {
        return KeyName::Credential;
    }
    compounds
}

/// Whether `key` names a credential by its words, compounds and endings.
pub(super) fn names_a_credential(key: &str) -> bool {
    classify_name(key) == KeyName::Credential
}

/// Whether the whole key is the bare name `key` or `auth`.
pub(super) fn is_bare_key_or_auth(key: &str) -> bool {
    matches!(key_segments(key).as_slice(), [only] if only == "key" || only == "auth")
}

/// Whether the whole key is the bare name `auth`.
pub(super) fn is_bare_auth(key: &str) -> bool {
    matches!(key_segments(key).as_slice(), [only] if only == "auth")
}

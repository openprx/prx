use regex::Regex;
use std::ops::Range;
use std::sync::LazyLock;

use super::names::{
    KEY_VALUE_MIN_LEN, KEY_VALUE_MIN_RUN, KeyName, classify_name, is_bare_key_or_auth, is_key_value_byte,
};
#[cfg(test)]
use super::record_redaction_work;
use super::{
    AUTH_SCHEMES, KEYED_PAIR, is_call, is_credential_value, is_type_annotation, looks_like_a_key_value,
    looks_like_a_secret,
};

/// The offset of the first `"` in `tail` that no backslash escapes: a
/// backslash takes the byte after it with it.
fn closing_double_quote(tail: &[u8]) -> Option<usize> {
    let mut at = 0;
    while let Some(&byte) = tail.get(at) {
        match byte {
            b'"' => return Some(at),
            b'\\' => at += 2,
            _ => at += 1,
        }
    }
    None
}

/// Opens an escaped JSON pair, `\"name\":\"`, as found in a JSON fragment
/// that sits inside a JSON string or other text: capture 1 is the name.
static ESCAPED_PAIR: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"\\"([A-Za-z0-9_.-]+)\\"\s*:\s*\\""#).ok());

/// Where the value of an escaped pair that starts at `start` ends: the
/// backslash of the first `\"` (one backslash, then the quote). Three
/// backslashes before a quote are an escaped quote inside the value
/// (`\\\"`) and stay in it. A value with no single-backslash close ends at
/// the first quote run instead. `None` when no quote follows at all.
fn escaped_value_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes().get(start..)?;
    let mut at = 0;
    let mut first_run = None;
    while let Some(&byte) = bytes.get(at) {
        if byte != b'\\' {
            at += 1;
            continue;
        }
        let run = bytes.get(at..)?.iter().take_while(|&&b| b == b'\\').count();
        if bytes.get(at + run) == Some(&b'"') {
            if run == 1 {
                return Some(start + at);
            }
            first_run.get_or_insert(start + at);
            at += run + 1;
        } else {
            at += run;
        }
    }
    first_run
}

/// The values of credential pairs whose quotes are escaped, as in
/// `config {\"password\":\"hunter22\"} ok`, masked exactly as the unescaped
/// JSON pair is: the name decides ([`is_credential_value`]), the name is kept.
pub(super) fn escaped_pair_spans(text: &str, spans: &mut Vec<Range<usize>>) {
    #[cfg(test)]
    record_redaction_work(text.len());
    if !text.contains("\\\"") {
        return;
    }
    let Some(pair) = ESCAPED_PAIR.as_ref() else {
        return;
    };
    #[cfg(test)]
    record_redaction_work(text.len());
    let mut covered_until = 0;
    for captures in pair.captures_iter(text) {
        let (Some(header), Some(name)) = (captures.get(0), captures.get(1)) else {
            continue;
        };
        if header.start() < covered_until {
            continue;
        }
        let Some(end) = escaped_value_end(text, header.end()) else {
            continue;
        };
        #[cfg(test)]
        record_redaction_work(end - header.end() + 1);
        let Some(value) = text.get(header.end()..end) else {
            continue;
        };
        if !value.is_empty() && is_credential_value(name.as_str(), value) {
            covered_until = end;
            spans.push(header.end()..end);
        }
    }
}

/// Cached value boundaries and token-shape summaries. Header positions only
/// advance, so each quote/bare boundary is searched once per disjoint region.
#[derive(Default)]
struct PairValueScan {
    end: usize,
    shape: Option<(usize, SecretShape)>,
}

/// What a value's bytes, read once up to its end, say about every suffix of
/// it: a suffix starting at or after `valid_from` holds only token bytes.
struct SecretShape {
    valid_from: usize,
    last_letter: Option<usize>,
    last_digit: Option<usize>,
    /// The same for the alphabet of a bare `key` value.
    key_valid_from: usize,
    /// Where the last run of letters and digits long enough for a key, with
    /// both a letter and a digit, starts.
    last_key_run: Option<usize>,
}

impl PairValueScan {
    fn value(&mut self, text: &str, start: usize, quote: Option<char>) -> Option<Range<usize>> {
        let body = start + usize::from(quote.is_some());
        if self.end <= start {
            let tail = text.get(body..)?;
            // A double-quoted value steps over a backslash-escaped quote
            // (`"a b\"c"`); one with no closing quote that way ends at the
            // first quote, as before.
            let offset = if quote == Some('"') {
                closing_double_quote(tail.as_bytes()).or_else(|| tail.find('"'))
            } else {
                tail.find(|character: char| {
                    quote.map_or_else(
                        || character.is_whitespace() || matches!(character, '"' | '\'' | ',' | ';' | '{' | '}'),
                        |quote| character == quote,
                    )
                })
            };
            self.end = offset.map_or(text.len(), |offset| body + offset);
            self.shape = None;
            #[cfg(test)]
            record_redaction_work(self.end - body + usize::from(offset.is_some()));
        }
        if quote.is_some() && self.end == text.len() {
            return None;
        }
        (body < self.end).then_some(body..self.end)
    }

    /// The shape of the value ending at `value.end`, read once per end.
    fn shape(&mut self, text: &str, value: &Range<usize>) -> Option<&SecretShape> {
        if self.shape.as_ref().is_none_or(|(end, _)| *end != value.end) {
            let source = text.get(value.clone())?;
            let mut shape = SecretShape {
                valid_from: value.start,
                last_letter: None,
                last_digit: None,
                key_valid_from: value.start,
                last_key_run: None,
            };
            let (mut run_start, mut run_letter, mut run_digit) = (value.start, false, false);
            for (offset, byte) in source.bytes().enumerate() {
                let position = value.start + offset;
                if !byte.is_ascii_alphanumeric() && !matches!(byte, b'+' | b'/' | b'=' | b'-') {
                    shape.valid_from = position + 1;
                }
                if !is_key_value_byte(byte) {
                    shape.key_valid_from = position + 1;
                }
                if byte.is_ascii_alphabetic() {
                    shape.last_letter = Some(position);
                }
                if byte.is_ascii_digit() {
                    shape.last_digit = Some(position);
                }
                if byte.is_ascii_alphanumeric() {
                    run_letter |= byte.is_ascii_alphabetic();
                    run_digit |= byte.is_ascii_digit();
                    if run_letter && run_digit && position + 1 - run_start >= KEY_VALUE_MIN_RUN {
                        shape.last_key_run = Some(run_start);
                    }
                } else {
                    (run_start, run_letter, run_digit) = (position + 1, false, false);
                }
            }
            #[cfg(test)]
            record_redaction_work(source.len());
            self.shape = Some((value.end, shape));
        }
        self.shape.as_ref().map(|(_, shape)| shape)
    }

    fn secret_shaped(&mut self, text: &str, value: &Range<usize>) -> bool {
        if value.len() < 24 {
            return false;
        }
        // UUID recognition reads at most 36 bytes, independently of nesting.
        if value.len() == 36 {
            #[cfg(test)]
            record_redaction_work(4 * value.len());
            return text.get(value.clone()).is_some_and(looks_like_a_secret);
        }
        self.shape(text, value).is_some_and(|shape| {
            shape.valid_from <= value.start
                && shape.last_letter.is_some_and(|last| last >= value.start)
                && shape.last_digit.is_some_and(|last| last >= value.start)
        })
    }

    /// [`looks_like_a_key_value`] on `value`, reading its bytes once per end.
    /// A value starts after a separator, space or quote, so every run of
    /// letters and digits that starts inside it is a whole run.
    fn key_shaped(&mut self, text: &str, value: &Range<usize>) -> bool {
        if value.len() < KEY_VALUE_MIN_LEN || text.get(value.start..).is_none_or(|tail| tail.starts_with(['/', '.'])) {
            return false;
        }
        // UUID recognition reads at most 36 bytes, independently of nesting.
        if value.len() == 36 {
            #[cfg(test)]
            record_redaction_work(4 * value.len());
            return text.get(value.clone()).is_some_and(looks_like_a_key_value);
        }
        self.shape(text, value).is_some_and(|shape| {
            shape.key_valid_from <= value.start && shape.last_key_run.is_some_and(|run| run >= value.start)
        })
    }
}

/// The first position at or after a query position where a byte predicate
/// holds, cached: a query inside the last searched gap reuses its answer, so
/// forward queries read each byte about once.
#[derive(Default)]
struct NextByte {
    from: usize,
    found: usize,
}

impl NextByte {
    fn find(&mut self, text: &str, at: usize, stop: impl Fn(u8) -> bool) -> usize {
        if self.from <= at && at <= self.found {
            return self.found;
        }
        let tail = text.as_bytes().get(at..).unwrap_or_default();
        let offset = tail.iter().position(|byte| stop(*byte)).unwrap_or(tail.len());
        #[cfg(test)]
        record_redaction_work(offset + 1);
        self.from = at;
        self.found = at + offset;
        self.found
    }
}

/// Where a bare value that stops at a quote (`String::from("abc123")`) ends
/// once it takes the quoted literal with it: after the closing quote on the
/// same line and whatever follows up to the next bare delimiter (whitespace,
/// `,`, `;`, `{`, `}`), so `token = String::from("abc")` is masked whole and
/// masking twice changes nothing. `None` when the quote does not close on
/// its line. A quote directly followed by whitespace, `,`, `;`, `{`, `}`,
/// `)`, `]`, a backslash or the end of the text closes an enclosing string
/// (the closing `'` of a shell argument, `'api_key: x' 'next'`); it does not
/// open a literal, so the bare value keeps its own end.
#[derive(Default)]
struct QuotedTail {
    double: NextByte,
    single: NextByte,
    delimiter: NextByte,
}

impl QuotedTail {
    fn end(&mut self, text: &str, quote_at: usize) -> Option<usize> {
        let quote = *text.as_bytes().get(quote_at)?;
        let closes = match quote {
            b'"' => &mut self.double,
            b'\'' => &mut self.single,
            _ => return None,
        };
        if text.as_bytes().get(quote_at + 1).is_none_or(|&after| {
            after.is_ascii_whitespace() || matches!(after, b',' | b';' | b'{' | b'}' | b')' | b']' | b'\\')
        }) {
            return None;
        }
        let close = closes.find(text, quote_at + 1, |byte| {
            byte == quote || matches!(byte, b'\n' | b'\r')
        });
        if text.as_bytes().get(close) != Some(&quote) {
            return None;
        }
        Some(self.delimiter.find(text, close + 1, |byte| {
            byte.is_ascii_whitespace() || matches!(byte, b',' | b';' | b'{' | b'}')
        }))
    }
}

pub(super) struct UrlPart {
    pub(super) range: Range<usize>,
    pub(super) value_start: Option<usize>,
}

pub(super) fn keyed_pair_spans(text: &str, url_parts: &[UrlPart], spans: &mut Vec<Range<usize>>) {
    let Some(pair) = KEYED_PAIR.as_ref() else {
        return;
    };
    #[cfg(test)]
    record_redaction_work(text.len());
    let mut values: [PairValueScan; 3] = std::array::from_fn(|_| PairValueScan::default());
    let mut quoted_tail = QuotedTail::default();
    let mut covered_until = 0;
    let mut url_part = 0;
    // Only the header is consumed. In `failed: token = value` the next
    // header remains visible even when whitespace splits the inner pair.
    for captures in pair.captures_iter(text) {
        let (Some(header), Some(name), Some(separator)) = (captures.get(0), captures.get(2), captures.get(3)) else {
            continue;
        };
        #[cfg(test)]
        record_redaction_work(header.len() + 2 * name.len());
        let names = name.as_str();
        let quoted_name = captures.get(1).is_some_and(|quote| !quote.is_empty());
        let bare_key = is_bare_key_or_auth(names);
        let class = classify_name(names);
        if (bare_key && !quoted_name && separator.as_str() != "=") || (!bare_key && class == KeyName::Plain) {
            continue;
        }
        let (kind, quote) = match text.get(header.end()..).and_then(|tail| tail.chars().next()) {
            Some('"') => (1, Some('"')),
            Some('\'') => (2, Some('\'')),
            Some(_) => (0, None),
            None => continue,
        };
        let Some(scan) = values.get_mut(kind) else {
            continue;
        };
        let Some(mut value) = scan.value(text, header.end(), quote) else {
            continue;
        };
        while url_parts
            .get(url_part)
            .is_some_and(|part| part.range.end <= name.start())
        {
            url_part += 1;
        }
        // A URL query value ends before the next parameter. Keep later names
        // intact so the provider renderer can still mask every URL component.
        if let Some(part) = url_parts.get(url_part)
            && part.range.start <= name.start()
        {
            // The URL pass classifies the complete decoded parameter name.
            // A partial regex name inside `public%5Fkey` must not override it.
            if part.value_start.is_some_and(|start| name.start() < start) && header.end() <= part.range.end {
                continue;
            }
            if header.end() < part.range.end {
                value.end = value.end.min(part.range.end);
            }
        }
        // The value as scanned, before any quoted tail: a shape check reads
        // only this, so a tail of quotes cannot hide a secret-shaped value.
        let scanned = value.clone();
        // A bare value that stops at a quote (`String::from("abc123")`) takes
        // the quoted literal with it, so the literal is masked too.
        if kind == 0
            && let Some(end) = quoted_tail.end(text, value.end)
        {
            value.end = end;
        }
        if value.is_empty() {
            continue;
        }
        let Some(source) = text.get(value.clone()) else {
            continue;
        };
        let is_header =
            names.eq_ignore_ascii_case("authorization") || names.eq_ignore_ascii_case("proxy-authorization");
        if is_header {
            let scheme_word = source.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                && source
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
            let before_credential = scheme_word
                && text.get(value.end..).is_some_and(|tail| {
                    let after = tail.trim_start_matches([' ', '\t']);
                    #[cfg(test)]
                    record_redaction_work(tail.len() - after.len() + 1);
                    after.len() < tail.len() && after.starts_with(|character: char| !character.is_whitespace())
                });
            if before_credential || AUTH_SCHEMES.iter().any(|scheme| source.eq_ignore_ascii_case(scheme)) {
                continue;
            }
        }
        // A bare code shape is not a credential: `token = compute()` and
        // `pub token: String` keep their code (CE-64). Quoted values stay masked.
        // A quote right after the scanned value closes an enclosing argument
        // (`'password=hunter(two)'`), so that value is a secret, not code.
        let quote_follows = text
            .as_bytes()
            .get(scanned.end)
            .is_some_and(|byte| matches!(byte, b'\'' | b'"'));
        let code_shape = kind == 0
            && !quote_follows
            && (is_call(source) || (separator.as_str() == ":" && is_type_annotation(source)));
        // The bare `key` is classified by its value; the bare `auth` is a
        // credential name on its own (the only Plain name that gets here).
        let credential = !code_shape
            && match class {
                KeyName::Credential => true,
                KeyName::CredentialIfSecretShaped => scan.secret_shaped(text, &scanned),
                KeyName::CredentialIfKeyShaped => scan.key_shaped(text, &scanned),
                KeyName::Plain => bare_key,
            };
        if credential && value.end > covered_until {
            covered_until = value.end;
            spans.push(value);
        }
    }
}

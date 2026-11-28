use regex::Regex;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::LazyLock;

use super::names::{
    KEY_VALUE_MIN_LEN, KEY_VALUE_MIN_RUN, KeyName, classify_name, is_bare_key_or_auth, is_key_value_byte,
    names_a_password_or_secret,
};
#[cfg(test)]
use super::record_redaction_work;
use super::{
    AUTH_SCHEMES, KEYED_PAIR, is_call, is_credential_value, is_type_annotation, looks_like_a_key_value,
    looks_like_a_secret,
};

/// Whether the unescaped quote at `quote_at` in `tail` opens the next field
/// instead of closing the value: `name="` (the name after whitespace or a
/// separator, an `=` and optional blanks before the quote) or `"name":` (the
/// quote followed by a name, a quote and a colon). The second field of the
/// result is the number of bytes read.
fn opens_next_field(tail: &[u8], quote_at: usize) -> (bool, usize) {
    let is_name = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-');
    let starts_name = |at: usize| {
        at.checked_sub(1)
            .and_then(|before| tail.get(before))
            .is_some_and(|&byte| byte.is_ascii_whitespace() || matches!(byte, b',' | b';' | b'{' | b'('))
    };
    let mut read = 1;
    // `name="`
    let mut at = quote_at;
    while at > 0 && matches!(tail.get(at - 1), Some(b' ' | b'\t')) {
        at -= 1;
        read += 1;
    }
    if at > 0 && tail.get(at - 1) == Some(&b'=') {
        let name_end = at - 1;
        let mut name_start = name_end;
        while name_start > 0 && tail.get(name_start - 1).copied().is_some_and(is_name) {
            name_start -= 1;
            read += 1;
        }
        if name_start < name_end && starts_name(name_start) {
            return (true, read);
        }
    }
    // `"name":`
    let name_start = quote_at + 1;
    let mut at = name_start;
    while tail.get(at).copied().is_some_and(is_name) {
        at += 1;
        read += 1;
    }
    if at > name_start && tail.get(at) == Some(&b'"') {
        let mut after = at + 1;
        while matches!(tail.get(after), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            after += 1;
            read += 1;
        }
        if tail.get(after) == Some(&b':') {
            return (true, read);
        }
    }
    (false, read)
}

/// The offset of the first `"` in `tail` that no backslash escapes (a
/// backslash takes the byte after it with it) and the number of bytes read.
/// When that quote opens the next field (`token="C:\temp\" password="x"`) the
/// value ended at the last escaped quote before it, so the name is not eaten.
fn closing_double_quote(tail: &[u8]) -> (Option<usize>, usize) {
    let mut at = 0;
    let mut last_escaped_quote = None;
    let mut read = 0;
    while let Some(&byte) = tail.get(at) {
        match byte {
            b'"' => {
                if let Some(escaped) = last_escaped_quote {
                    let (opens, extra) = opens_next_field(tail, at);
                    read += extra;
                    if opens {
                        return (Some(escaped), read + at + 1);
                    }
                }
                return (Some(at), read + at + 1);
            }
            b'\\' => {
                if tail.get(at + 1) == Some(&b'"') {
                    last_escaped_quote = Some(at + 1);
                }
                at += 2;
            }
            _ => at += 1,
        }
    }
    (None, read + tail.len())
}

/// Opens an escaped pair, `\"name\":\"` or `\'name\':\'`, as found in a
/// fragment that sits inside a string or other text. The runs of backslashes
/// are checked in code (the regex crate has no backreferences): captures are
/// 1 the opening quote, 2 the name, 3 the run before the name's closing
/// quote, 4 that quote, 5 the run before the value's opening quote, 6 that
/// quote.
static ESCAPED_PAIR: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"(["'])([A-Za-z0-9_.-]+)(\\+)(["'])\s*:\s*(\\+)(["'])"#).ok());

/// The most distinct unusual run lengths one text may use for escaped pairs.
/// A nesting level k encodes a quote as 2^k - 1 backslashes; the lengths 1,
/// 3, 7, 15 and 31 and the any-run fallback are never capped, so a text
/// built from many other lengths cannot switch masking off for the rest. A
/// capped pair is masked through the fallback instead of being skipped.
const MAX_ESCAPE_RUNS: usize = 32;

/// Whether a pair run length is a realistic nesting level (1, 3, 7, 15, 31)
/// or the any-run fallback (0): such keys are exempt from the cap.
const fn is_standard_run(pair_run: usize) -> bool {
    pair_run == 0 || (pair_run <= 31 && (pair_run + 1).is_power_of_two())
}

/// The first run of backslashes followed by a quote, at or after a query
/// position, cached: forward queries inside the last answered gap reuse it.
#[derive(Default)]
struct RunSearch {
    from: usize,
    found: Option<usize>,
}

/// Where escaped values end, one cached search per quote character and
/// run length.
#[derive(Default)]
struct EscapedEnds {
    searches: HashMap<(u8, usize), Option<RunSearch>>,
    /// Distinct non-standard keys in `searches`, counted against the cap.
    unusual: usize,
}

impl EscapedEnds {
    /// Where a value ends: the first run of backslashes before `quote` at or
    /// after `at` that closes a value whose pair uses `pair_run` backslashes
    /// (the value ends before the last `pair_run` of them): at nesting
    /// level k the pair uses `pair_run = 2^k - 1` backslashes and an escaped
    /// backslash in the value is `2 * (pair_run + 1)` of them, so the run's
    /// length is `pair_run` modulo `2 * pair_run + 2`; a value may end in
    /// backslashes. `pair_run == 0` takes any run and ends the value at the
    /// start of the run, as the unescaped scan does.
    fn find(&mut self, bytes: &[u8], at: usize, quote: u8, pair_run: usize) -> Option<usize> {
        if !self.searches.contains_key(&(quote, pair_run)) && !is_standard_run(pair_run) {
            if self.unusual >= MAX_ESCAPE_RUNS {
                return None;
            }
            self.unusual += 1;
        }
        let slot = self.searches.entry((quote, pair_run)).or_default();
        if let Some(search) = slot
            && search.from <= at
            && search.found.is_none_or(|found| at <= found)
        {
            return search.found;
        }
        let mut position = at;
        let mut found = None;
        while let Some(&byte) = bytes.get(position) {
            if byte != b'\\' {
                position += 1;
                continue;
            }
            let run = bytes
                .get(position..)
                .map_or(0, |rest| rest.iter().take_while(|&&b| b == b'\\').count());
            if bytes.get(position + run) == Some(&quote) && (pair_run == 0 || run % (2 * pair_run + 2) == pair_run) {
                // The value's own trailing backslashes belong to the value.
                found = Some(if pair_run == 0 {
                    position
                } else {
                    position + run - pair_run
                });
                break;
            }
            position += run + usize::from(bytes.get(position + run) == Some(&quote));
        }
        #[cfg(test)]
        record_redaction_work(found.unwrap_or(bytes.len()) - at.min(bytes.len()) + 1);
        *slot = Some(RunSearch { from: at, found });
        found
    }
}

/// Where the value of an escaped pair that starts at `start` ends: the first
/// run that closes it, else the first quote run of any length. `None` when
/// no quote follows at all.
fn escaped_value_end(ends: &mut EscapedEnds, text: &str, start: usize, quote: u8, pair_run: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    ends.find(bytes, start, quote, pair_run)
        .or_else(|| ends.find(bytes, start, quote, 0))
}

/// The values of credential pairs whose quotes are escaped, as in
/// `config {\"password\":\"hunter22\"} ok` (also at deeper nesting, with
/// 3, 7, ... backslashes, an even equal run such as 2 where a shell command
/// escaped the quotes of an already escaped text, and with `'`), masked exactly as the unescaped
/// pair is: the name decides ([`is_credential_value`]), the name is kept.
pub(super) fn escaped_pair_spans(text: &str, spans: &mut Vec<Range<usize>>) {
    #[cfg(test)]
    record_redaction_work(text.len());
    if !text.contains("\\\"") && !text.contains("\\'") {
        return;
    }
    let Some(pair) = ESCAPED_PAIR.as_ref() else {
        return;
    };
    #[cfg(test)]
    record_redaction_work(text.len());
    let bytes = text.as_bytes();
    let mut ends = EscapedEnds::default();
    let mut covered_until = 0;
    for captures in pair.captures_iter(text) {
        let (Some(header), Some(name), Some(open), Some(close), Some(value_open)) = (
            captures.get(0),
            captures.get(2),
            captures.get(1),
            captures.get(3),
            captures.get(5),
        ) else {
            continue;
        };
        if header.start() < covered_until {
            continue;
        }
        let run = close.len();
        if value_open.len() != run {
            continue;
        }
        let (Some(quote), Some(closing), Some(value_quote)) = (
            captures.get(4).and_then(|quote| quote.as_str().bytes().next()),
            open.as_str().bytes().next(),
            captures.get(6).and_then(|quote| quote.as_str().bytes().next()),
        ) else {
            continue;
        };
        if quote != closing || quote != value_quote {
            continue;
        }
        // The run before the opening quote is the pair's own, or longer when
        // an escaped backslash precedes it.
        let lead = bytes
            .get(..header.start())
            .map_or(0, |before| before.iter().rev().take_while(|&&b| b == b'\\').count());
        #[cfg(test)]
        record_redaction_work(lead + 1);
        if lead < run {
            continue;
        }
        let Some(end) = escaped_value_end(&mut ends, text, header.end(), quote, run) else {
            continue;
        };
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
            let (offset, _read) = if quote == Some('"') {
                let (closing, read) = closing_double_quote(tail.as_bytes());
                (closing.or_else(|| tail.find('"')), read)
            } else {
                let found = tail.find(|character: char| {
                    quote.map_or_else(
                        || character.is_whitespace() || matches!(character, '"' | '\'' | ',' | ';' | '{' | '}'),
                        |quote| character == quote,
                    )
                });
                (found, 0)
            };
            self.end = offset.map_or(text.len(), |offset| body + offset);
            self.shape = None;
            #[cfg(test)]
            record_redaction_work(_read.max(self.end - body + usize::from(offset.is_some())));
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
        // A password or secret name gets no call-shape exemption: a bare
        // `password=hunter(two)` is a secret, and `password = getpass()` in a
        // listing is masked too (accepted over-mask); `password: String` keeps
        // its type annotation.
        let quote_follows = text
            .as_bytes()
            .get(scanned.end)
            .is_some_and(|byte| matches!(byte, b'\'' | b'"'));
        let code_shape = kind == 0
            && !quote_follows
            && ((is_call(source) && !names_a_password_or_secret(names))
                || (separator.as_str() == ":" && is_type_annotation(source)));
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

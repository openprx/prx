//! Redaction of endpoint userinfo and URL secrets in provider labels.

use super::split_byo_provider;

/// A provider string as it is safe to echo back.
///
/// A bring-your-own endpoint is named by its URL, and a URL may carry
/// userinfo, which is a credential. The scheme, host and port are what an
/// operator needs to recognise the endpoint; the userinfo must not reach
/// terminal output or an error chain.
///
/// The input may be a bare label or free text that quotes labels (an error
/// chain, a failover summary): every URL authority in it that carries
/// userinfo is rewritten to `scheme://[redacted]@host`. The rewrite is
/// idempotent, so text redacted once can pass through here again unchanged.
///
/// The authority is delimited the way the WHATWG URL parser behind
/// `reqwest::Url` delimits it, so whatever the transport would send as
/// userinfo is what is redacted: a special scheme's separator may be written
/// with any number of `/` or `\`; the authority ends at the first `/`, `?`
/// or `#` (and `\` for a special scheme); the userinfo ends at the last `@`
/// inside it, but never reaches into a later URL that carries its own
/// userinfo. Quotes, spaces, backticks, angle brackets and non-ASCII
/// characters do not end it: the parser accepts them in a password. A `//`
/// authority that does not parse (a password holding a literal `/`) is
/// redacted conservatively, through the last `@` before the next URL that
/// carries its own userinfo. No input can make this function panic: every
/// slice goes through `get` at a char boundary. It runs in time linear in
/// the text (see [`TextIndex`]), so an adversarial upstream body cannot
/// stall the caller.
///
/// `docs/providers.md` lists what the tests prove and the known limits.
#[must_use]
pub fn redact_endpoint_userinfo(provider: &str) -> std::borrow::Cow<'_, str> {
    if !provider.contains('@') {
        return std::borrow::Cow::Borrowed(provider);
    }
    // First pass: a bare label is decided by the parser itself, so the two
    // can never disagree about whether the transport would send userinfo.
    // It is only a first pass: text that merely *begins* with a URL also
    // parses as one URL (the rest becomes its path), so the free-text scan
    // always runs over the result and redacts every later userinfo. The scan
    // leaves `[redacted]@` alone, so running it again is safe.
    match redact_parsed_label(provider) {
        Some(std::borrow::Cow::Owned(first)) => match redact_free_text(&first) {
            std::borrow::Cow::Owned(both) => std::borrow::Cow::Owned(both),
            std::borrow::Cow::Borrowed(_) => std::borrow::Cow::Owned(first),
        },
        Some(std::borrow::Cow::Borrowed(_)) | None => redact_free_text(provider),
    }
}

/// Redact the userinfo of every URL authority in free text (see
/// [`redact_endpoint_userinfo`]).
///
/// Every index below is a char boundary: it is either a position `find` or
/// `rfind` returned for an ASCII character (`:`, `@`, a separator or a
/// terminator), or a position advanced past a whole character with
/// `len_utf8`. Slices go through `get`, so even a broken invariant ends the
/// scan instead of panicking.
///
/// The cost is linear in the text (R-C2BUj J-1): the positions of `@` and of
/// every authority terminator are indexed once ([`TextIndex`]), so an
/// authority's end, whether it holds an `@`, and the next credentialed URL
/// are each a binary search, never a rescan of the rest of the text.
pub(super) fn redact_free_text(provider: &str) -> std::borrow::Cow<'_, str> {
    let index = TextIndex::new(provider);
    let mut output = String::new();
    let mut copied = 0;
    let mut search_from = 0;
    while let Some(found) = next_url_authority(provider, search_from) {
        let authority_end = index.authority_end(found.authority_start, found.special);
        let authority_len = authority_end - found.authority_start;
        // The userinfo ends at the last `@` of the authority, but never
        // reaches into a later URL that carries its own userinfo: a
        // non-special authority does not end at `\`, so without this bound it
        // could swallow `ws:\\x@h` on one pass but not on the next. The bound
        // can only matter when the authority holds an `@` at all.
        let userinfo_end = if index.has_at(found.authority_start, authority_end) {
            let bound = found.authority_start
                + authority_len.min(index.userinfo_bound_after(found.authority_start, found.authority_start));
            index.last_at(found.authority_start, bound)
        } else if found.separators >= 2 {
            // The conservative fallback is for a `//` authority that a
            // literal `/` in the password cut short; a spelling with fewer
            // separators is matched only where the parser itself reads it as
            // an authority.
            conservative_userinfo_end(provider, &index, &found, authority_end)
        } else {
            None
        };
        let Some(at) = userinfo_end else {
            search_from = found.authority_start;
            continue;
        };
        let userinfo = provider.get(found.authority_start..at).unwrap_or_default();
        // Already redacted, or the placeholder a pasteable declaration prints.
        if !matches!(userinfo, REDACTED_USERINFO | USERINFO_PLACEHOLDER) {
            output.push_str(provider.get(copied..found.authority_start).unwrap_or_default());
            output.push_str(REDACTED_USERINFO);
            copied = at;
        }
        search_from = at + 1;
    }
    if copied == 0 {
        return std::borrow::Cow::Borrowed(provider);
    }
    output.push_str(provider.get(copied..).unwrap_or_default());
    std::borrow::Cow::Owned(output)
}

// Bytes the userinfo redactor examined on this thread: a deterministic work
// count, so a test can prove the scan linear without a clock (R-C2BUk K-4).
// Every scan step records what it reads; a test resets it, redacts, and
// compares the count across input sizes.
#[cfg(test)]
thread_local! {
    static USERINFO_SCAN_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn record_userinfo_scan_work(bytes: usize) {
    USERINFO_SCAN_WORK.with(|work| work.set(work.get().saturating_add(bytes)));
}

/// The bytes [`redact_endpoint_userinfo`] examines for `text`, counted on this
/// thread.
#[cfg(test)]
pub(crate) fn userinfo_scan_work(text: &str) -> usize {
    USERINFO_SCAN_WORK.with(|work| work.set(0));
    std::hint::black_box(redact_endpoint_userinfo(text).len());
    USERINFO_SCAN_WORK.with(std::cell::Cell::get)
}

/// The positions the scan asks about, found once per text, so every question
/// is a binary search: where an authority ends, whether a span holds an `@`,
/// the last `@` in a span, and the next URL whose own authority carries an
/// `@`.
pub(super) struct TextIndex<'a> {
    pub(super) text: &'a str,
    /// Positions of every `@`.
    pub(super) ats: Vec<usize>,
    /// Positions of `/`, `\`, `?` and `#`: where a special authority ends.
    pub(super) special_ends: Vec<usize>,
    /// Positions of `/`, `?` and `#`: where a non-special authority ends
    /// (the parser keeps `\` in a non-special userinfo).
    pub(super) plain_ends: Vec<usize>,
    /// Every URL written with at least two separators whose authority holds
    /// an `@`, as `(colon, scheme start)`, in text order. Built lazily.
    pub(super) credentialed: std::cell::OnceCell<Vec<(usize, usize)>>,
    pub(super) text_len: usize,
}

impl<'a> TextIndex<'a> {
    pub(super) fn new(text: &'a str) -> Self {
        #[cfg(test)]
        record_userinfo_scan_work(text.len());
        let mut ats = Vec::new();
        let mut special_ends = Vec::new();
        let mut plain_ends = Vec::new();
        for (position, byte) in text.bytes().enumerate() {
            match byte {
                b'@' => ats.push(position),
                b'\\' => special_ends.push(position),
                b'/' | b'?' | b'#' => {
                    special_ends.push(position);
                    plain_ends.push(position);
                }
                _ => {}
            }
        }
        Self {
            text,
            ats,
            special_ends,
            plain_ends,
            credentialed: std::cell::OnceCell::new(),
            text_len: text.len(),
        }
    }

    /// Every URL written with at least two separators whose own authority
    /// holds an `@`, as `(colon, scheme start)`, from one pass over the text.
    pub(super) fn scan_credentialed(&self) -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        let mut from = 0;
        while let Some(url) = next_url_authority(self.text, from) {
            if url.separators >= 2
                && self.has_at(
                    url.authority_start,
                    self.authority_end(url.authority_start, url.special),
                )
            {
                found.push((url.colon, url.scheme_start));
            }
            from = url.authority_start;
        }
        found
    }

    /// Where the authority starting at `start` ends: the first `/`, `?` or
    /// `#` at or after it (and `\` for a special scheme), or the end.
    pub(super) fn authority_end(&self, start: usize, special: bool) -> usize {
        let ends = if special { &self.special_ends } else { &self.plain_ends };
        let index = ends.partition_point(|&position| position < start);
        ends.get(index).copied().unwrap_or(self.text_len)
    }

    /// Whether `[start, end)` holds an `@`.
    pub(super) fn has_at(&self, start: usize, end: usize) -> bool {
        let index = self.ats.partition_point(|&position| position < start);
        self.ats.get(index).is_some_and(|&position| position < end)
    }

    /// The last `@` in `[start, end)`.
    pub(super) fn last_at(&self, start: usize, end: usize) -> Option<usize> {
        let index = self.ats.partition_point(|&position| position < end);
        index
            .checked_sub(1)
            .and_then(|last| self.ats.get(last).copied())
            .filter(|&position| position >= start)
    }

    /// The offset from `authority_start` of the first URL whose colon is at
    /// or after `from`, written with at least two separators, whose own
    /// authority holds an `@`; or the rest of the text. A userinfo never
    /// reaches into such a URL: that `@` belongs to it.
    ///
    /// Only a `//` (or, for a special scheme, `\\`) URL counts: a special
    /// scheme word with a colon and no separators (`k9-wss:Q`) inside a
    /// password is part of that password to the parser (R-C2BUj J-2).
    pub(super) fn userinfo_bound_after(&self, authority_start: usize, from: usize) -> usize {
        let whole = self.text_len.saturating_sub(authority_start);
        let credentialed = self.credentialed.get_or_init(|| self.scan_credentialed());
        let index = credentialed.partition_point(|&(colon, _)| colon < from);
        credentialed.get(index).map_or(whole, |&(_, scheme_start)| {
            scheme_start.max(from).saturating_sub(authority_start).min(whole)
        })
    }
}

/// A redacted provider label as it can be pasted into a declaration: the
/// `[redacted]` userinfo is replaced by `<user>:<password>` placeholders, and
/// the second value says whether that happened, so the caller can tell the
/// operator to fill them in. A declaration matches by provider identity,
/// which keeps userinfo, so a pasted `[redacted]` would match nothing (and
/// the configuration API reads it as a mask).
#[must_use]
pub fn pasteable_provider_label(label: &str) -> (std::borrow::Cow<'_, str>, bool) {
    // After whatever separator the label was spelled with (`//`, `\\`, `/`,
    // none): the marker is what the redactor leaves in place of userinfo.
    const MARKER: &str = "[redacted]@";
    if label.contains(MARKER) {
        (
            std::borrow::Cow::Owned(label.replace(MARKER, "<user>:<password>@")),
            true,
        )
    } else {
        (std::borrow::Cow::Borrowed(label), false)
    }
}

/// The marker a redacted userinfo leaves behind, without its `@`.
pub(super) const REDACTED_USERINFO: &str = "[redacted]";

/// The userinfo placeholder of [`pasteable_provider_label`]; it names no
/// credential, so it is printed as is.
pub(super) const USERINFO_PLACEHOLDER: &str = "<user>:<password>";

/// Schemes the URL parser treats as special: after the colon it accepts any
/// number of `/` or `\`, none included, before the authority.
pub(super) const SPECIAL_URL_SCHEMES: [&str; 5] = ["http", "https", "ws", "wss", "ftp"];

/// Whether `c` is one of the characters the URL parser removes from its whole
/// input before parsing (ASCII tab, LF, CR).
pub(super) const fn is_url_ignored(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r')
}

/// The scanner's view of one URL.
pub(super) struct UrlAuthority {
    pub(super) scheme_start: usize,
    pub(super) colon: usize,
    pub(super) authority_start: usize,
    pub(super) separators: usize,
    pub(super) special: bool,
}

/// How far back from a colon the scheme search looks. A special scheme has
/// at most five characters; a longer non-special scheme is still found, only
/// reported with a shorter scheme. Without this cap, a long run of scheme
/// characters (`1a1a…:`, `a\ta\t…:`) makes every colon cost the whole run
/// (R-C2BUj J-1).
pub(super) const MAX_SCHEME_SPAN: usize = 64;

/// The next URL authority at or after `from`.
///
/// The scheme is `[A-Za-z][A-Za-z0-9+.-]*` before a colon; tab, LF and CR
/// anywhere in the scheme or the separator are skipped, as the parser skips
/// them. A special scheme (`http`, `https`, `ws`, `wss`, `ftp`) takes any
/// number of `/` or `\` after the colon, none included, and whatever follows
/// (whitespace too: the parser reads `http: alice:pw@h` as userinfo, so prose
/// such as `https: see a@b` is redacted, the safe direction). Any other
/// scheme needs `//`: the parser gives it an authority only after two `/`,
/// and keeps a `\` as part of that authority.
///
/// Candidate scheme starts, tried in order: the widest run of scheme
/// characters (plus tab, LF, CR) before the colon, at most
/// [`MAX_SCHEME_SPAN`] bytes back, as the parser reads `ht\ntp://`; then each
/// start just after a tab, LF or CR inside it (a URL at the start of a line
/// or a cell); then each letter that follows a digit, `+`, `-` or `.` (a
/// scheme glued to `502`, `1.`, `--`). Nothing before `from` is ever part of
/// a candidate.
pub(super) fn next_url_authority(text: &str, from: usize) -> Option<UrlAuthority> {
    let mut index = from;
    while index < text.len() {
        let colon = index + text.get(index..)?.find(':')?;
        #[cfg(test)]
        record_userinfo_scan_work(colon + 1 - index);
        index = colon + 1;
        let rest = text.get(colon + 1..)?;
        // Special schemes read any mix of `/` and `\`; others only `/`.
        let special_run = rest
            .find(|c: char| !matches!(c, '/' | '\\') && !is_url_ignored(c))
            .unwrap_or(rest.len());
        let special_separators = rest
            .get(..special_run)?
            .chars()
            .filter(|c| matches!(c, '/' | '\\'))
            .count();
        let plain_run = rest
            .find(|c: char| c != '/' && !is_url_ignored(c))
            .unwrap_or(rest.len());
        let plain_separators = rest.get(..plain_run)?.chars().filter(|c| *c == '/').count();
        #[cfg(test)]
        record_userinfo_scan_work(special_run + plain_run + 2);
        let before = text.get(from..colon)?;
        // Advance past the whole character the search stopped at: it may be
        // multibyte (`é:`, `€:`, NBSP, East-Asian text), and one byte past its
        // start is not a char boundary (R-C2BUi I-1).
        let widest = from
            + before
                .char_indices()
                .rev()
                .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.') || is_url_ignored(*c)))
                .map_or(0, |(position, c)| position + c.len_utf8());
        #[cfg(test)]
        record_userinfo_scan_work(colon - widest);
        // Every character between `widest` and the colon is ASCII, so any
        // byte offset in that span is a char boundary.
        let widest = widest.max(colon.saturating_sub(MAX_SCHEME_SPAN));
        let span = text.get(widest..colon)?;
        #[cfg(test)]
        record_userinfo_scan_work(span.len());
        let mut previous: Option<char> = None;
        let mut inner = Vec::new();
        for (offset, c) in span.char_indices() {
            if is_url_ignored(c) {
                inner.push(widest + offset + c.len_utf8());
            }
            if c.is_ascii_alphabetic() && previous.is_some_and(|p| p.is_ascii_digit() || matches!(p, '+' | '-' | '.')) {
                inner.push(widest + offset);
            }
            if !is_url_ignored(c) {
                previous = Some(c);
            }
        }
        for scheme_start in std::iter::once(widest).chain(inner) {
            #[cfg(test)]
            record_userinfo_scan_work(colon - scheme_start);
            let scheme: String = text
                .get(scheme_start..colon)?
                .chars()
                .filter(|c| !is_url_ignored(*c))
                .collect();
            if !scheme.chars().next().is_some_and(|first| first.is_ascii_alphabetic()) {
                continue;
            }
            let special = SPECIAL_URL_SCHEMES.iter().any(|name| scheme.eq_ignore_ascii_case(name));
            let (run, separators) = if special {
                (special_run, special_separators)
            } else {
                (plain_run, plain_separators)
            };
            if separators >= 2 || special {
                return Some(UrlAuthority {
                    scheme_start,
                    colon,
                    authority_start: colon + 1 + run,
                    separators,
                    special,
                });
            }
        }
        // Every candidate read as a non-special scheme without two `/`. A
        // special scheme glued to a preceding letter (`xhttps:\\u:pw@h`,
        // `completionshTTp\nS:\\u:pw@h`) is such a longer word, for which `\`
        // is no separator, but the URL it ends is what a reader copies (and
        // what the parser reads once the glued prefix is gone). When the
        // separators include two `/` or `\`, that trailing special scheme is
        // taken as the start (R-C2BUk K-1).
        if special_separators >= 2
            && let Some(scheme_start) = glued_special_scheme_start(span, widest)
        {
            return Some(UrlAuthority {
                scheme_start,
                colon,
                authority_start: colon + 1 + special_run,
                separators: special_separators,
                special: true,
            });
        }
    }
    None
}

/// Where a special scheme glued to the end of `span` starts, as a byte offset
/// into the text (`span` starts at `span_start`): `https` in `xhttps`, and in
/// `hTTp\nS`, since tab, LF and CR inside a scheme are skipped, as the parser
/// skips them. The longest special name wins (`wss` over `ss`). `None` when
/// the span does not end with one.
pub(super) fn glued_special_scheme_start(span: &str, span_start: usize) -> Option<usize> {
    /// The longest special scheme name.
    const MAX_SPECIAL_SCHEME_LEN: usize = 5;
    #[cfg(test)]
    record_userinfo_scan_work(span.len());
    let mut letters = 0usize;
    let mut start = None;
    for (offset, c) in span.char_indices().rev() {
        if is_url_ignored(c) {
            continue;
        }
        letters += 1;
        if letters > MAX_SPECIAL_SCHEME_LEN {
            break;
        }
        let name: String = span.get(offset..)?.chars().filter(|c| !is_url_ignored(*c)).collect();
        if SPECIAL_URL_SCHEMES
            .iter()
            .any(|special| name.eq_ignore_ascii_case(special))
        {
            start = Some(span_start + offset);
        }
    }
    start
}

/// For a bare provider label (optionally prefixed `custom:` or
/// `anthropic-custom:`) that the URL parser reads with a user name or a
/// password, the label with that userinfo redacted, measured on the raw
/// string: after the scheme colon and every separator (`/`, `\`, tab, LF,
/// CR), through the last `@` before the first `/`, `?` or `#` (and `\` for a
/// special scheme). `None` when the input is not such a label.
pub(super) fn redact_parsed_label(label: &str) -> Option<std::borrow::Cow<'_, str>> {
    let trimmed = label.trim_end();
    let url_start = split_byo_provider(trimmed).map_or_else(
        || trimmed.len() - trimmed.trim_start().len(),
        |(_, raw)| trimmed.len() - raw.len(),
    );
    let raw = trimmed.get(url_start..)?;
    #[cfg(test)]
    record_userinfo_scan_work(raw.len());
    let parsed = reqwest::Url::parse(raw).ok()?;
    if parsed.username().is_empty() && parsed.password().is_none() {
        return None;
    }
    let special = SPECIAL_URL_SCHEMES.contains(&parsed.scheme());
    let colon = raw.find(':')?;
    let after = raw.get(colon + 1..)?;
    let run = after
        .find(|c: char| !(if special { matches!(c, '/' | '\\') } else { c == '/' }) && !is_url_ignored(c))
        .unwrap_or(after.len());
    let authority_start = url_start + colon + 1 + run;
    let index = TextIndex::new(label);
    // Bounded like the free-text scan, so the two passes agree.
    let authority_end = index.authority_end(authority_start, special);
    let bound = authority_start
        + (authority_end - authority_start).min(index.userinfo_bound_after(authority_start, authority_start));
    let at = index.last_at(authority_start, bound)?;
    if matches!(
        label.get(authority_start..at)?,
        REDACTED_USERINFO | USERINFO_PLACEHOLDER
    ) {
        return Some(std::borrow::Cow::Borrowed(label));
    }
    Some(std::borrow::Cow::Owned(format!(
        "{}{REDACTED_USERINFO}{}",
        label.get(..authority_start)?,
        label.get(at..)?
    )))
}

/// An authority longer than this is not handed to the URL parser by the
/// conservative fallback: no real host is that long, and parsing a long
/// authority for every URL of a crafted text would cost quadratic time. It is
/// treated as unparseable, which only ever redacts more.
pub(super) const MAX_PARSED_AUTHORITY: usize = 1024;

/// For an authority with no `@` of its own: when `scheme://authority` does not
/// parse, the password may hold a literal `/` that ended the authority early,
/// so everything through the last `@` before the next URL that carries its
/// own userinfo (or the end of the text) is treated as userinfo. Tab, LF and
/// CR do not end it, since the parser ignores them. `None` when the authority
/// parses, or there is no such `@`.
pub(super) fn conservative_userinfo_end(
    text: &str,
    index: &TextIndex<'_>,
    found: &UrlAuthority,
    authority_end: usize,
) -> Option<usize> {
    // No `@` after the authority: nothing to find, and nothing to parse.
    index.last_at(authority_end, text.len())?;
    #[cfg(test)]
    if authority_end - found.authority_start <= MAX_PARSED_AUTHORITY {
        record_userinfo_scan_work(authority_end - found.scheme_start);
    }
    if authority_end - found.authority_start <= MAX_PARSED_AUTHORITY
        && reqwest::Url::parse(text.get(found.scheme_start..authority_end)?).is_ok()
    {
        return None;
    }
    // Stop at the next URL whose own authority holds an `@`: that `@` belongs
    // to it (R-C2BUi I-3). A URL nested in this password (`a:b://c@h`) starts
    // before this authority ends and is not searched for.
    let bound = found.authority_start + index.userinfo_bound_after(found.authority_start, authority_end);
    index.last_at(found.authority_start, bound)
}

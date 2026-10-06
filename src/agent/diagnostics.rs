pub(super) const TOOL_PARSE_LOG_PREVIEW_CHARS: usize = 200;

/// How much of a secret a tool-output preview keeps: detection is the one
/// redactor's, this is only how a detected secret is shown.
const CREDENTIAL_PREVIEW_PREFIX_CHARS: usize = 4;

/// The shortest secret a preview shows a prefix of: a shorter one would give
/// away half of itself or more, so it is shown as `*[REDACTED]` alone.
const CREDENTIAL_PREVIEW_MIN_CHARS: usize = 8;

/// Scrub credentials from tool output to prevent accidental exfiltration.
///
/// Every secret the one redactor ([`crate::security::redaction`]) detects is
/// shown as its first few characters and `*[REDACTED]`, so a reader can tell
/// which credential it was; a secret shorter than
/// [`CREDENTIAL_PREVIEW_MIN_CHARS`] is shown as `*[REDACTED]` alone. This is a
/// presentation wrapper only.
pub(super) fn scrub_credentials(input: &str) -> String {
    crate::security::redaction::redact_secrets_with(input, |secret| {
        let prefix: String = if secret.chars().count() >= CREDENTIAL_PREVIEW_MIN_CHARS {
            secret.chars().take(CREDENTIAL_PREVIEW_PREFIX_CHARS).collect()
        } else {
            String::new()
        };
        format!("{prefix}*{}", crate::security::redaction::REDACTED)
    })
}

/// How much of a tool-call parse failure's text is redacted for its preview.
/// The preview keeps [`TOOL_PARSE_LOG_PREVIEW_CHARS`] characters; a secret
/// that starts within them and is detected by its start (a token prefix, a
/// credential name, an `Authorization` value) is found in this prefix even
/// when cut at its end, so redacting more would only cost time on a large
/// model response (R-C2BUk K-5). The one format detected by its end, a PEM
/// private-key block, is handled apart (see
/// [`unterminated_private_key_start`]).
const TOOL_PARSE_LOG_SCRUBBED_BYTES: usize = 16 * 1024;

/// The start of a PEM private-key header (`-----BEGIN … PRIVATE KEY-----`)
/// in `text`. The redactor replaces a whole block, BEGIN to END, so a header
/// still present after redaction is one whose END lies beyond the redacted
/// prefix (D-069 L-4).
fn unterminated_private_key_start(text: &str) -> Option<usize> {
    const BEGIN: &str = "-----BEGIN ";
    const DASHES: &str = "-----";
    let mut from = 0usize;
    while let Some(offset) = text.get(from..).and_then(|rest| rest.find(BEGIN)) {
        let at = from + offset;
        let label_start = at + BEGIN.len();
        let label = text
            .get(label_start..)
            .and_then(|rest| rest.find(DASHES).and_then(|end| rest.get(..end)));
        if label.is_some_and(|label| {
            label.ends_with("PRIVATE KEY")
                && label
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == ' ')
        }) {
            return Some(at);
        }
        from = label_start;
    }
    None
}

/// Redact and bound a tool-call parse failure's preview for the log.
///
/// Only a bounded prefix is redacted, then the preview is cut from it: the
/// prefix is far longer than the preview, so a secret that starts inside the
/// preview is still detected whole. A PEM private-key block whose END lies
/// beyond the prefix is redacted from its BEGIN to the cut.
pub(super) fn sanitize_tool_parse_log_preview(input: &str) -> String {
    let mut prefix_end = input.len().min(TOOL_PARSE_LOG_SCRUBBED_BYTES);
    while !input.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }
    let mut redacted = crate::security::redaction::redact_secrets(input.get(..prefix_end).unwrap_or_default());
    if let Some(at) = unterminated_private_key_start(&redacted) {
        redacted.truncate(at);
        redacted.push_str(crate::security::redaction::REDACTED);
    }
    if redacted.chars().count() <= TOOL_PARSE_LOG_PREVIEW_CHARS {
        return redacted;
    }
    redacted.chars().take(TOOL_PARSE_LOG_PREVIEW_CHARS).collect()
}

/// Tool arguments for the log: every credential-named value replaced and
/// every string redacted by the one redactor.
pub(crate) fn redact_sensitive_json_keys(value: &serde_json::Value) -> serde_json::Value {
    let mut redacted = value.clone();
    crate::security::redaction::redact_json_value(&mut redacted);
    redacted
}

#[path = "turn_tests/diagnostics.rs"]
mod tests;

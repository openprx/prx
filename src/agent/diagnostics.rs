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

/// Payload-free identity of free text for the log (a malformed tool-call
/// body, a provider error, a model-chosen reason): its byte length and the
/// first 16 hex digits of its SHA-256. The text itself (model output, an
/// upstream error that may echo user or tool content) is never logged.
pub(crate) fn tool_parse_log_digest(input: &str) -> (usize, String) {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(input.as_bytes());
    let hex = digest.iter().take(8).fold(String::with_capacity(16), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
        out
    });
    (input.len(), hex)
}

/// Payload-free identity of an error for the log: the length and digest of
/// its full display chain ([`tool_parse_log_digest`]). Memory, provider and
/// storage errors can quote the content they refused (a SQLite trigger's
/// message, an upstream echo), so the text is never logged.
pub(crate) fn error_log_digest(error: &(impl std::fmt::Display + ?Sized)) -> (usize, String) {
    tool_parse_log_digest(&format!("{error:#}"))
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

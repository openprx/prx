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

/// The one way production code logs an error or a free-form string: its
/// type, byte length and SHA-256 prefix, never its text.
///
/// Errors from memory, providers, storage and tools can quote what they
/// refused (a SQLite trigger message, an upstream echo of user or tool
/// content), and names, paths and keys can be user data, so a log line
/// carries only this class. `Display` prints `kind=<type> len=<bytes>
/// sha256=<16 hex>`; the `architecture_boundaries` log scanner accepts a
/// tracing field built by [`error_class`] or [`text_class`] and rejects any
/// other way of formatting an error or a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogClass {
    kind: &'static str,
    len: usize,
    sha256: String,
}

impl std::fmt::Display for LogClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "kind={} len={} sha256={}", self.kind, self.len, self.sha256)
    }
}

/// [`LogClass`] of an error: its static type name, and the length and digest
/// of its full display chain ([`error_log_digest`]).
pub(crate) fn error_class<E: std::fmt::Display + ?Sized>(error: &E) -> LogClass {
    let (len, sha256) = error_log_digest(error);
    LogClass {
        kind: std::any::type_name::<E>(),
        len,
        sha256,
    }
}

/// [`LogClass`] of free text (a name, a path, a key, a label): kind `text`,
/// its byte length and digest.
pub(crate) fn text_class(text: &str) -> LogClass {
    let (len, sha256) = tool_parse_log_digest(text);
    LogClass {
        kind: "text",
        len,
        sha256,
    }
}

/// A registered tool's name, logged in plain.
///
/// A tool name is configuration metadata (the registry or a plugin manifest
/// chose it), not session payload, so it needs no digest. It can only be
/// built from a registry type ([`crate::tools::ToolSpec`],
/// [`crate::tools::execution::ToolDescriptor`]); a
/// bare `&str`, such as a model-written call name, cannot become one and is
/// logged through [`text_class`]. The `architecture_boundaries` log scanner
/// accepts these constructors and an identifier typed `ToolName`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolName<'a>(&'a str);

impl<'a> ToolName<'a> {
    /// The name a tool spec advertises to the model.
    pub(crate) fn spec(spec: &'a crate::tools::ToolSpec) -> Self {
        Self(&spec.name)
    }

    /// The public name of a resolved tool descriptor.
    pub(crate) fn descriptor(descriptor: &'a crate::tools::execution::ToolDescriptor) -> Self {
        Self(&descriptor.public_name)
    }
}

impl std::fmt::Display for ToolName<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// A configured provider kind or model id, logged in plain after the one
/// provider-label redactor ([`crate::security::redaction::redact_provider_text`])
/// removed any secret or URL credential it carried. It can only be built
/// through that redactor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderLabel(String);

impl ProviderLabel {
    /// `label` through the provider-label redactor.
    pub(crate) fn redacted(label: &str) -> Self {
        Self(crate::security::redaction::redact_provider_text(label))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
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

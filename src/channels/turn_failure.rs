//! Classification and user-facing notices for failed channel turns.

use super::providers;

/// Number of hex digits in the short failure code shown to the user.
const TURN_ERROR_ID_LEN: usize = 8;

/// Short, user-quotable code identifying one failed turn.
///
/// It is derived deterministically from the turn's run id, which is the value
/// the failure is logged under (`turn_run_id`) and the id the durable terminal
/// event is committed with. A user who quotes the code therefore pins an
/// operator to exactly one turn instead of "somewhere in the last few hours".
/// The run id is a UUIDv4, so its leading hex digits carry the same
/// randomness; a run id that is not hex-shaped (not produced today, but the
/// field is a plain `String`) is hashed instead so the function stays total.
pub(crate) fn turn_error_id(turn_run_id: &str) -> String {
    let hex: String = turn_run_id
        .chars()
        .filter(char::is_ascii_hexdigit)
        .map(|c| c.to_ascii_lowercase())
        .take(TURN_ERROR_ID_LEN)
        .collect();
    if hex.len() == TURN_ERROR_ID_LEN {
        return hex;
    }
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(turn_run_id.as_bytes());
    digest
        .iter()
        .take(TURN_ERROR_ID_LEN / 2)
        .fold(String::with_capacity(TURN_ERROR_ID_LEN), |mut acc, byte| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{byte:02x}");
            acc
        })
}

/// What went wrong on a channel turn, at the coarse granularity a chat user can
/// actually act on.
///
/// Every variant maps to a fixed sentence. Error text is never interpolated
/// into what the user sees, so no upstream message can carry a filesystem path,
/// a credential, an internal hostname or a stack out into the chat — the worst
/// a misclassification can do is give slightly wrong advice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnFailureKind {
    /// An image or file the conversation refers to could not be loaded.
    Attachment,
    /// The conversation no longer fits in the model's context window.
    ContextTooLong,
    /// Upstream asked us to slow down; waiting helps.
    RateLimited,
    /// Upstream refused on billing/entitlement grounds; waiting does not help.
    QuotaExhausted,
    /// We cannot authenticate to the model service.
    ServiceCredentials,
    /// The model service answered, but is failing or overloaded.
    ServiceUnavailable,
    /// The model service could not be reached at all.
    Network,
    /// The model service cannot accept some of the tool definitions this
    /// assistant is configured to offer it, so the request was not sent.
    /// Retrying cannot help; the configuration has to change.
    ToolSchemaRefused,
    /// Nothing matched. Deliberately says as little as possible.
    Unclassified,
}

impl TurnFailureKind {
    /// Stable machine token logged beside the full error, so a reported error
    /// id can be cross-checked against the class the classifier picked.
    pub(crate) const fn log_label(self) -> &'static str {
        match self {
            Self::Attachment => "attachment",
            Self::ContextTooLong => "context_too_long",
            Self::RateLimited => "rate_limited",
            Self::QuotaExhausted => "quota_exhausted",
            Self::ServiceCredentials => "service_credentials",
            Self::ServiceUnavailable => "service_unavailable",
            Self::Network => "network",
            Self::ToolSchemaRefused => "tool_schema_refused",
            Self::Unclassified => "unclassified",
        }
    }

    /// The user-visible sentence for this class.
    ///
    /// Every arm is a compile-time constant written for a non-engineer and says
    /// what the reader can do next. Nothing derived from the error is allowed
    /// in here.
    pub(crate) const fn advice(self) -> &'static str {
        match self {
            Self::Attachment => {
                "⚠️ I could not open an image or file that this conversation refers to — it looks like it has expired or been removed. Please send it again, or start a new conversation to clear it."
            }
            Self::ContextTooLong => {
                "⚠️ This conversation has grown too long for me to work through in one go. Please start a new conversation, or ask a shorter question."
            }
            Self::RateLimited => {
                "⚠️ There are too many requests going through right now, so I was asked to slow down. Please wait a minute and send your message again."
            }
            Self::QuotaExhausted => {
                "⚠️ My AI service account has run out of allowance, so I cannot answer right now. Trying again will not help — please let an administrator know."
            }
            Self::ServiceCredentials => {
                "⚠️ I cannot sign in to the AI service, so I cannot answer right now. An administrator needs to fix this — please pass on the error ID below."
            }
            Self::ServiceUnavailable => {
                "⚠️ The AI service is having trouble at the moment. Please try again in a few minutes."
            }
            Self::Network => "⚠️ I could not reach the AI service — the connection failed. Please try again.",
            Self::ToolSchemaRefused => {
                "⚠️ The AI service I am set up to use cannot accept some of the tools I am configured to offer it, so I cannot answer. This is a configuration problem that sending the message again will not fix — please pass the error ID below to an administrator."
            }
            Self::Unclassified => "⚠️ Something went wrong. Please try again later.",
        }
    }
}

/// Build the user-visible failure notice: fixed advice plus the code that ties
/// this message to one log record.
pub(crate) fn turn_failure_notice(kind: TurnFailureKind, error_id: &str) -> String {
    format!("{}\n(Error ID: {error_id})", kind.advice())
}

/// Classify a failed turn by scanning the whole `anyhow` cause chain.
///
/// The chain, not just the outermost message: a multimodal read failure or an
/// upstream 429 routinely arrives wrapped in a generic context line, and
/// matching only the head would classify all of those as unknown.
pub(crate) fn classify_turn_failure(err: &anyhow::Error) -> TurnFailureKind {
    // Typed first: a tool-schema refusal (or a failover chain that ended in
    // one) is a configuration problem, whatever words its tool names contain.
    // After a boundary that carried it as text, a cause that *is* a rendered
    // refusal still counts; upstream text that only quotes one does not
    // (SP-INT2 a51).
    if providers::tool_schema_refused_tools(err).is_some()
        || err
            .chain()
            .any(|cause| providers::is_tool_schema_refusal_text(&cause.to_string()))
    {
        return TurnFailureKind::ToolSchemaRefused;
    }
    if crate::recovery::overflow::is_context_overflow_error(err) {
        return TurnFailureKind::ContextTooLong;
    }
    let mut chain_text = String::new();
    for cause in err.chain() {
        use std::fmt::Write as _;
        let _ = writeln!(chain_text, "{cause}");
    }
    chain_text.make_ascii_lowercase();
    classify_turn_failure_text(&chain_text)
}

/// Classifier core, split out so it can be exercised on literal error text.
///
/// `lower` must already be lowercased. The hint strings were taken from the
/// error shapes this branch has actually produced in production plus the
/// `Display` impls that can reach it (`MultimodalError`, `StreamError`, the
/// per-provider `"{provider} API error ({status}): …"` renderings and the
/// reliability layer's aggregate). Order is deliberate: narrow, unambiguous
/// markers first, and a billing refusal is recognised before the generic 429
/// wording it arrives with. Anything matching nothing stays `Unclassified` —
/// saying too little is recoverable, telling a user the wrong thing is not.
fn classify_turn_failure_text(lower: &str) -> TurnFailureKind {
    /// Emitted only by the multimodal / media-artifact preparation stage. The
    /// production incident this classifier exists for lands on the first entry.
    const ATTACHMENT_HINTS: [&str; 8] = [
        "failed to read local image",
        "failed to download remote image",
        "invalid multimodal image marker",
        "multimodal remote image",
        "multimodal image",
        "image url targets a private/local address",
        "media artifact i/o failed",
        "media source is not a regular readable file",
    ];
    /// Billing / entitlement refusals: retrying cannot clear these.
    const QUOTA_HINTS: [&str; 9] = [
        "insufficient balance",
        "insufficient_balance",
        "insufficient quota",
        "insufficient_quota",
        "quota exhausted",
        "exceeded your current quota",
        "credit balance is too low",
        "plan does not include",
        "billing",
    ];
    /// Authentication and credential-configuration failures. `reqwest` renders
    /// a status as `401 Unauthorized`, so the reason phrase is the reliable
    /// marker; a bare `401` is not matched, because it also occurs inside
    /// request ids and token counts.
    const CREDENTIAL_HINTS: [&str; 12] = [
        "unauthorized",
        "403 forbidden",
        "http 403",
        "invalid authentication",
        "invalid_authentication",
        "authentication_error",
        "api key appears to be invalid",
        "invalid api key",
        "incorrect api key",
        "invalid_api_key",
        "api key not set",
        "oauth refresh failed",
    ];
    /// Explicit 5xx statuses. Checked before the rate-limit hints because the
    /// streaming layer reports a 503 through a variant whose `Display` still
    /// begins with "Rate limited".
    const SERVER_STATUS_HINTS: [&str; 9] = [
        "500 internal server error",
        "502 bad gateway",
        "503 service unavailable",
        "504 gateway timeout",
        "http 500",
        "http 502",
        "http 503",
        "http 504",
        "http 529",
    ];
    /// Throttling that clears on its own.
    const RATE_LIMIT_HINTS: [&str; 6] = [
        "too many requests",
        "http 429",
        "rate limited",
        "rate limit",
        "rate_limit",
        "slow down",
    ];
    /// Upstream capacity problems stated in prose rather than as a status
    /// (Anthropic `overloaded_error`, Kimi `engine_overloaded_error`). Checked
    /// after the rate-limit hints so a 429 that merely explains itself as
    /// "overloaded" still gets the "wait a minute" advice.
    const OVERLOAD_TEXT_HINTS: [&str; 6] = [
        "overloaded",
        "service unavailable",
        "bad gateway",
        "gateway timeout",
        "internal server error",
        "temporarily unavailable",
    ];
    /// Transport-level failures: nothing upstream was reached at all.
    const NETWORK_HINTS: [&str; 12] = [
        "error sending request",
        "error decoding response body",
        "network retries exhausted",
        "connection refused",
        "connection reset",
        "connection closed",
        "timed out",
        "timeout",
        "dns error",
        "failed to lookup address",
        "network is unreachable",
        "broken pipe",
    ];

    let hit = |hints: &[&str]| hints.iter().any(|hint| lower.contains(hint));

    // A tool-schema refusal is recognised by `classify_turn_failure` before
    // this text is built, and never by a marker anywhere in it: upstream
    // bodies are part of this text.
    if hit(&ATTACHMENT_HINTS) {
        TurnFailureKind::Attachment
    } else if hit(&QUOTA_HINTS) {
        TurnFailureKind::QuotaExhausted
    } else if hit(&CREDENTIAL_HINTS) {
        TurnFailureKind::ServiceCredentials
    } else if hit(&SERVER_STATUS_HINTS) {
        TurnFailureKind::ServiceUnavailable
    } else if hit(&RATE_LIMIT_HINTS) {
        TurnFailureKind::RateLimited
    } else if hit(&OVERLOAD_TEXT_HINTS) {
        TurnFailureKind::ServiceUnavailable
    } else if hit(&NETWORK_HINTS) {
        TurnFailureKind::Network
    } else {
        TurnFailureKind::Unclassified
    }
}

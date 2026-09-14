//! S2.5 T2.5-1: the chat module's three error layers plus one top-level
//! aggregate.
//!
//! `providers::traits::StreamError` is deliberately left alone (its five
//! variants stay as they are, so none of the providers break).
//! `TransportError` bridges from it through `From<StreamError>`, and
//! `TransportError::is_retryable` is the method form of what used to be
//! `dispatcher.rs::stream_error_is_retryable` (kept there as a thin wrapper).

use thiserror::Error;

use crate::providers::traits::StreamError;

/// Provider semantic errors: business-level failures reported by the upstream
/// LLM service.
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider returned semantic error: {0}")]
    Semantic(String),

    #[error("provider rejected request: {reason}")]
    Rejected { reason: String },

    #[error("provider context window exceeded: {0}")]
    ContextOverflow(String),
}

/// Transport errors: network / IO / SSE / JSON-parsing failures.
///
/// Bridged from `providers::traits::StreamError` through `From<StreamError>`;
/// `is_retryable` is the same rule as `dispatcher.rs::stream_error_is_retryable`.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("transport HTTP error: {0}")]
    Http(String),

    #[error("transport IO error: {0}")]
    Io(String),

    #[error("transport JSON parse error: {0}")]
    Json(String),

    #[error("transport SSE format invalid: {0}")]
    InvalidSse(String),

    #[error("transport provider message: {0}")]
    ProviderMessage(String),

    /// Upstream rate-limit (429) or temporary unavailability (503).
    ///
    /// FIX-P0-33: retryable, since the reliability layer honors the carried
    /// `Retry-After` hint and retries the same provider/model.
    #[error("transport rate limited (HTTP {status}): {message}")]
    RateLimited {
        status: u16,
        retry_after_ms: Option<u64>,
        message: String,
    },
}

impl TransportError {
    /// Whether this error is worth retrying.
    ///
    /// Same rule as `dispatcher.rs::stream_error_is_retryable`: Http / Io count
    /// as transient and are retryable.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Http(_) | Self::Io(_) | Self::RateLimited { .. })
    }
}

impl From<StreamError> for TransportError {
    fn from(err: StreamError) -> Self {
        match err {
            StreamError::Http(e) => Self::Http(e.to_string()),
            StreamError::Io(e) => Self::Io(e.to_string()),
            StreamError::Json(e) => Self::Json(e.to_string()),
            StreamError::InvalidSse(msg) => Self::InvalidSse(msg),
            StreamError::Provider(msg) => Self::ProviderMessage(msg),
            StreamError::RateLimited {
                status,
                retry_after_ms,
                message,
            } => Self::RateLimited {
                status,
                retry_after_ms,
                message,
            },
        }
    }
}

/// UI-layer errors: terminal rendering, input parsing, quarantine display.
#[derive(Debug, Error)]
pub enum UiError {
    #[error("ui render failed: {0}")]
    Render(String),

    #[error("ui input invalid: {0}")]
    Input(String),

    #[error("ui terminal unavailable: {0}")]
    Terminal(String),
}

/// The top-level chat error: the three layers plus IO / anyhow in one type.
#[derive(Debug, Error)]
pub enum ChatError {
    #[error(transparent)]
    Provider(#[from] ProviderError),

    #[error(transparent)]
    Transport(#[from] TransportError),

    #[error(transparent)]
    Ui(#[from] UiError),

    #[error("chat session error: {0}")]
    Session(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

// Note: `From<ChatError> for anyhow::Error` is deliberately not implemented.
// anyhow already provides a blanket impl for every
// `E: std::error::Error + Send + Sync + 'static`, and the `thiserror::Error`
// derive makes ChatError satisfy that, so `?` bridges straight into
// `anyhow::Result` at the call site. To pull the inner `anyhow::Error` back out
// of `ChatError::Other`, use the `into_anyhow` helper rather than a `From`
// impl, which would collide with the blanket one (E0119).
impl ChatError {
    /// Convert a ChatError into an `anyhow::Error`: `Other` passes through
    /// unchanged, every other variant is wrapped as a trait object.
    #[must_use]
    pub fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Other(inner) => inner,
            other => anyhow::Error::new(other),
        }
    }
}

impl From<StreamError> for ChatError {
    fn from(err: StreamError) -> Self {
        Self::Transport(err.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s2_5_t2_5_1_provider_error_display_format() {
        let semantic = ProviderError::Semantic("rate limit".to_string());
        assert_eq!(format!("{semantic}"), "provider returned semantic error: rate limit");

        let rejected = ProviderError::Rejected {
            reason: "policy".to_string(),
        };
        assert_eq!(format!("{rejected}"), "provider rejected request: policy");

        let overflow = ProviderError::ContextOverflow("maximum context length".to_string());
        assert_eq!(
            format!("{overflow}"),
            "provider context window exceeded: maximum context length"
        );
    }

    #[test]
    fn s2_5_t2_5_1_transport_from_stream_error() {
        let io_err = StreamError::Io(std::io::Error::other("disk full"));
        let transport: TransportError = io_err.into();
        assert!(matches!(transport, TransportError::Io(_)));

        let sse_err = StreamError::InvalidSse("bad frame".to_string());
        let transport: TransportError = sse_err.into();
        match transport {
            TransportError::InvalidSse(msg) => assert_eq!(msg, "bad frame"),
            other => panic!("expected InvalidSse, got {other:?}"),
        }

        let provider_err = StreamError::Provider("oops".to_string());
        let transport: TransportError = provider_err.into();
        match transport {
            TransportError::ProviderMessage(msg) => assert_eq!(msg, "oops"),
            other => panic!("expected ProviderMessage, got {other:?}"),
        }

        let json_err =
            StreamError::Json(serde_json::from_str::<serde_json::Value>("notjson").expect_err("test: invalid json"));
        let transport: TransportError = json_err.into();
        assert!(matches!(transport, TransportError::Json(_)));
    }

    #[test]
    fn s2_5_t2_5_1_transport_is_retryable_matches_legacy() {
        // is_retryable follows the same rule as
        // dispatcher.rs::stream_error_is_retryable: Http / Io are retryable,
        // the other three are not.
        let http = TransportError::Http("conn reset".to_string());
        assert!(http.is_retryable());

        let io = TransportError::Io("eof".to_string());
        assert!(io.is_retryable());

        let json = TransportError::Json("bad".to_string());
        assert!(!json.is_retryable());

        let sse = TransportError::InvalidSse("frame".to_string());
        assert!(!sse.is_retryable());

        let provider = TransportError::ProviderMessage("rate limit".to_string());
        assert!(!provider.is_retryable());
    }

    #[test]
    fn s2_5_t2_5_1_ui_error_kind_complete() {
        let render = UiError::Render("frame buffer overflow".to_string());
        assert_eq!(format!("{render}"), "ui render failed: frame buffer overflow");

        let input = UiError::Input("invalid utf-8".to_string());
        assert_eq!(format!("{input}"), "ui input invalid: invalid utf-8");

        let terminal = UiError::Terminal("tty closed".to_string());
        assert_eq!(format!("{terminal}"), "ui terminal unavailable: tty closed");
    }

    #[test]
    fn s2_5_t2_5_1_chat_error_aggregate_from() {
        // All three of Provider / Transport / Ui bridge into ChatError with `?`.
        let provider_chat: ChatError = ProviderError::Semantic("x".to_string()).into();
        assert!(matches!(provider_chat, ChatError::Provider(_)));

        let transport_chat: ChatError = TransportError::Http("y".to_string()).into();
        assert!(matches!(transport_chat, ChatError::Transport(_)));

        let ui_chat: ChatError = UiError::Render("z".to_string()).into();
        assert!(matches!(ui_chat, ChatError::Ui(_)));

        // StreamError reaches ChatError through TransportError.
        let stream_chat: ChatError = StreamError::Io(std::io::Error::other("eof")).into();
        match stream_chat {
            ChatError::Transport(TransportError::Io(_)) => {}
            other => panic!("expected ChatError::Transport(Io), got {other:?}"),
        }

        // ChatError reaches anyhow::Error through into_anyhow; the Other arm
        // passes through instead of being wrapped twice.
        let anyhow_err = ChatError::Other(anyhow::anyhow!("plain")).into_anyhow();
        assert_eq!(anyhow_err.to_string(), "plain");

        let wrapped = ChatError::Session("missing id".to_string()).into_anyhow();
        assert!(wrapped.to_string().contains("missing id"));

        // anyhow's blanket From<ChatError> must carry a `?` bridge; map_err
        // spells the conversion out explicitly.
        let typed: Result<(), ChatError> = Err(ChatError::Session("bridge".to_string()));
        let bridged: anyhow::Result<()> = typed.map_err(Into::into);
        assert!(bridged.is_err());
        assert!(format!("{:?}", bridged.expect_err("test: should be err")).contains("bridge"));
    }
}

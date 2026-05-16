//! S2.5 T2.5-1: chat 模块三层错误分层 + 顶层聚合。
//!
//! 不重写 `providers::traits::StreamError`（保留 5 变体不动，避免破坏 4 个 provider）。
//! `TransportError` 通过 `From<StreamError>` 桥接，`TransportError::is_retryable`
//! 关联方法迁出 `dispatcher.rs::stream_error_is_retryable`（原 fn 保留为 thin wrapper）。

use thiserror::Error;

use crate::providers::traits::StreamError;

/// Provider 语义错误：由上游 LLM 服务返回的业务级错误。
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider returned semantic error: {0}")]
    Semantic(String),

    #[error("provider rejected request: {reason}")]
    Rejected { reason: String },

    #[error("provider context window exceeded: {0}")]
    ContextOverflow(String),
}

/// Transport 错误：网络/IO/SSE/JSON 解析等传输层故障。
///
/// 通过 `From<StreamError>` 从 `providers::traits::StreamError` 桥接，
/// 关联方法 `is_retryable` 与 `dispatcher.rs::stream_error_is_retryable` 同源。
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
}

impl TransportError {
    /// 判断错误是否值得重试。
    ///
    /// 与 `dispatcher.rs::stream_error_is_retryable` 同源（Http/Io 视为瞬时故障 retryable）。
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Http(_) | Self::Io(_))
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
        }
    }
}

/// UI 层错误：终端渲染、输入解析、隔离区呈现等。
#[derive(Debug, Error)]
pub enum UiError {
    #[error("ui render failed: {0}")]
    Render(String),

    #[error("ui input invalid: {0}")]
    Input(String),

    #[error("ui terminal unavailable: {0}")]
    Terminal(String),
}

/// 顶层 chat 聚合错误：将三层 + IO/Anyhow 统一封装。
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

// 注：不为 ChatError 显式 impl From<_> for anyhow::Error —— anyhow 已为所有
// `E: std::error::Error + Send + Sync + 'static` 提供 blanket 实现，ChatError
// 通过 thiserror::Error derive 自动满足约束，调用点 `?` 直接桥接到
// `anyhow::Result`。如需直接抽出 ChatError::Other 的内部 anyhow::Error，
// 使用 helper `into_anyhow` 而非 `From`，避免与 blanket 冲突 (E0119)。
impl ChatError {
    /// 将 ChatError 转为 `anyhow::Error`，Other 变体透传，其他变体经 trait object 包装。
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
        // 关联方法 is_retryable 与 dispatcher.rs::stream_error_is_retryable 同源:
        // Http / Io → retryable，其余三类 → non-retryable。
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
        // Provider/Transport/Ui 三类都能 ? 桥接到 ChatError。
        let provider_chat: ChatError = ProviderError::Semantic("x".to_string()).into();
        assert!(matches!(provider_chat, ChatError::Provider(_)));

        let transport_chat: ChatError = TransportError::Http("y".to_string()).into();
        assert!(matches!(transport_chat, ChatError::Transport(_)));

        let ui_chat: ChatError = UiError::Render("z".to_string()).into();
        assert!(matches!(ui_chat, ChatError::Ui(_)));

        // StreamError → ChatError 经 TransportError 桥接.
        let stream_chat: ChatError = StreamError::Io(std::io::Error::other("eof")).into();
        match stream_chat {
            ChatError::Transport(TransportError::Io(_)) => {}
            other => panic!("expected ChatError::Transport(Io), got {other:?}"),
        }

        // ChatError → anyhow::Error 经 helper into_anyhow（Other 分支透传不双重包装）.
        let anyhow_err = ChatError::Other(anyhow::anyhow!("plain")).into_anyhow();
        assert_eq!(anyhow_err.to_string(), "plain");

        let wrapped = ChatError::Session("missing id".to_string()).into_anyhow();
        assert!(wrapped.to_string().contains("missing id"));

        // 验证 anyhow blanket From<ChatError> 通过 ? 桥接的能力 — 显式 map_err 到 anyhow::Error。
        let typed: Result<(), ChatError> = Err(ChatError::Session("bridge".to_string()));
        let bridged: anyhow::Result<()> = typed.map_err(Into::into);
        assert!(bridged.is_err());
        assert!(format!("{:?}", bridged.expect_err("test: should be err")).contains("bridge"));
    }
}

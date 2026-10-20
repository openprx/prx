use super::{Deserialize, JsonSchema, Serialize};

// ── Gateway security ─────────────────────────────────────────────

/// Gateway server configuration (`[gateway]` section).
///
/// Controls the HTTP gateway for webhook and pairing endpoints.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct GatewayConfig {
    /// Gateway port (default: 16830)
    #[serde(default = "default_gateway_port")]
    pub port: u16,
    /// Gateway host (default: 127.0.0.1)
    #[serde(default = "default_gateway_host")]
    pub host: String,
    /// Require pairing before accepting requests (default: true)
    #[serde(default = "default_true")]
    pub require_pairing: bool,
    /// Allow binding to non-localhost without a tunnel (default: false)
    #[serde(default)]
    pub allow_public_bind: bool,
    /// Paired bearer tokens (managed automatically, not user-edited)
    #[serde(default)]
    pub paired_tokens: Vec<String>,

    /// Max `/pair` requests per minute per client key.
    #[serde(default = "default_pair_rate_limit")]
    pub pair_rate_limit_per_minute: u32,

    /// Max `/webhook` requests per minute per client key.
    #[serde(default = "default_webhook_rate_limit")]
    pub webhook_rate_limit_per_minute: u32,

    /// Max `/api/*` requests per minute per authenticated token.
    #[serde(default = "default_api_rate_limit")]
    pub api_rate_limit_per_minute: u32,

    /// Trust headers that a reverse proxy in front of the gateway asserts: the
    /// client IP (`X-Forwarded-For`, `X-Real-IP`) for rate limiting, and the
    /// caller identity of MCP tool calls (`X-Client-Cert-Subject`,
    /// `X-Forwarded-Client-Cert`, `X-Spiffe-Id`, `X-Spiffe-Svid`,
    /// `X-Client-Cert-Pem`). Disabled by default. Enable only when every request
    /// reaches the gateway through a proxy that removes or overwrites all of
    /// these headers; otherwise any client can claim any identity.
    #[serde(default)]
    pub trust_forwarded_headers: bool,

    /// Maximum distinct client keys tracked by gateway rate limiter maps.
    #[serde(default = "default_gateway_rate_limit_max_keys")]
    pub rate_limit_max_keys: usize,

    /// TTL for webhook idempotency keys.
    #[serde(default = "default_idempotency_ttl_secs")]
    pub idempotency_ttl_secs: u64,

    /// Maximum distinct idempotency keys retained in memory.
    #[serde(default = "default_gateway_idempotency_max_keys")]
    pub idempotency_max_keys: usize,

    /// How long an already-accepted platform webhook delivery (`/whatsapp`,
    /// `/linq`, `/nextcloud-talk`) is remembered so a platform redelivery of the
    /// same payload is answered instead of re-run.
    ///
    /// Sized against the sender, not the agent: Meta retries a WhatsApp delivery
    /// for up to 7 days and Linq for ~25 minutes, so a window shorter than the
    /// platform's retry schedule lets a late retry start the turn a second time.
    #[serde(default = "default_channel_webhook_dedup_ttl_secs")]
    pub channel_webhook_dedup_ttl_secs: u64,
}

pub(super) const fn default_gateway_port() -> u16 {
    16830
}

pub(super) fn default_gateway_host() -> String {
    "127.0.0.1".into()
}

pub(super) const fn default_pair_rate_limit() -> u32 {
    10
}

pub(super) const fn default_webhook_rate_limit() -> u32 {
    60
}

pub(super) const fn default_api_rate_limit() -> u32 {
    60
}

pub(super) const fn default_idempotency_ttl_secs() -> u64 {
    300
}

pub(super) const fn default_gateway_rate_limit_max_keys() -> usize {
    10_000
}

pub(super) const fn default_gateway_idempotency_max_keys() -> usize {
    10_000
}

pub(super) const fn default_channel_webhook_dedup_ttl_secs() -> u64 {
    86_400
}

pub(super) const fn default_true() -> bool {
    true
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            port: default_gateway_port(),
            host: default_gateway_host(),
            require_pairing: true,
            allow_public_bind: false,
            paired_tokens: Vec::new(),
            pair_rate_limit_per_minute: default_pair_rate_limit(),
            webhook_rate_limit_per_minute: default_webhook_rate_limit(),
            api_rate_limit_per_minute: default_api_rate_limit(),
            trust_forwarded_headers: false,
            rate_limit_max_keys: default_gateway_rate_limit_max_keys(),
            idempotency_ttl_secs: default_idempotency_ttl_secs(),
            idempotency_max_keys: default_gateway_idempotency_max_keys(),
            channel_webhook_dedup_ttl_secs: default_channel_webhook_dedup_ttl_secs(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct MemoryWebhookConfig {
    /// Socket address for the receiver (e.g. "0.0.0.0:16899").
    #[serde(default = "default_memory_webhook_bind")]
    pub bind: String,
    /// Required bearer token for incoming webhook events.
    #[serde(default)]
    pub token: Option<String>,
    /// Optional HMAC-SHA256 signing secret for `X-Webhook-Signature` verification.
    #[serde(default)]
    pub signing_secret: Option<String>,
}

pub(super) fn default_memory_webhook_bind() -> String {
    "0.0.0.0:16899".to_string()
}

impl Default for MemoryWebhookConfig {
    fn default() -> Self {
        Self {
            bind: default_memory_webhook_bind(),
            token: None,
            signing_secret: None,
        }
    }
}

impl MemoryWebhookConfig {
    pub fn configured(&self) -> bool {
        self.token.as_deref().is_some_and(|value| !value.trim().is_empty())
    }
}

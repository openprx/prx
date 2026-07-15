//! Axum-based HTTP gateway with proper HTTP/1.1 compliance, body limits, and timeouts.
//!
//! This module replaces the raw TCP implementation with axum for:
//! - Proper HTTP/1.1 parsing and compliance
//! - Content-Length validation (handled by hyper)
//! - Request body size limits (64KB max)
//! - Request timeouts (configurable) to prevent slow-loris attacks
//! - Header sanitization (handled by axum/hyper)

#![allow(clippy::print_stdout, clippy::print_stderr)]

mod api;
mod compat;
mod ui;

use crate::agent::loop_::{
    DocumentIngestRuntime, ToolConcurrencyGovernanceConfig, build_context_with_shared_events_and_scope,
    run_tool_call_loop_traced,
};
use crate::channels::{Channel, LinqChannel, NextcloudTalkChannel, SendMessage, SignalChannel, WhatsAppChannel};
use crate::config::Config;
use crate::hooks::HookManager;
use crate::memory::{self, Memory, MemoryCategory, MemoryFabric, MemoryVisibility};
use crate::observability::NoopObserver;
use crate::providers::{self, ChatMessage, Provider, ProviderCapabilityError};
use crate::runtime;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::security::SideEffectGate;
use crate::security::pairing::{PairingGuard, constant_time_eq, is_public_bind};
use crate::security::policy::ResourceRiskLevel;
use crate::tools::{self, McpTool, Tool};
use crate::util::truncate_with_ellipsis;
use anyhow::{Context, Result};
use axum::{
    Router,
    body::Bytes,
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Json},
    routing::{get, post},
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use uuid::Uuid;

/// Maximum request body size (64KB) — prevents memory exhaustion
pub const MAX_BODY_SIZE: usize = 65_536;
/// Larger request body limit for `/api` routes to support media uploads
/// (`10 * 20MB` files plus multipart overhead).
pub const MAX_API_BODY_SIZE: usize = (10 * 20 * 1024 * 1024) + (1024 * 1024);
/// Sliding window used by gateway rate limiting.
pub const RATE_LIMIT_WINDOW_SECS: u64 = 60;
/// Fallback max distinct client keys tracked in gateway rate limiter.
pub const RATE_LIMIT_MAX_KEYS_DEFAULT: usize = 10_000;
/// Fallback max distinct idempotency keys retained in gateway memory.
pub const IDEMPOTENCY_MAX_KEYS_DEFAULT: usize = 10_000;
/// Maximum accepted idempotency-key header length.
const IDEMPOTENCY_MAX_KEY_BYTES: usize = 256;
/// Maximum replay payload retained for one successful request.
const IDEMPOTENCY_MAX_REPLAY_BYTES: usize = 1024 * 1024;
/// Process-wide payload budget reserved by in-flight and successful requests.
const IDEMPOTENCY_REPLAY_BUDGET_BYTES: usize = 32 * 1024 * 1024;
/// Upper bound on how long the gateway waits for in-flight requests to drain
/// after a shutdown signal before forcing exit (D5/D9 step 3). Gateway-local on
/// purpose: `main.rs`'s private `RUNTIME_SHUTDOWN_TIMEOUT` is unreachable from
/// the lib crate.
const GATEWAY_GRACEFUL_TIMEOUT: Duration = Duration::from_secs(30);

fn webhook_memory_key(idempotency_digest: Option<&str>) -> String {
    idempotency_digest.map_or_else(
        || format!("webhook_msg_{}", Uuid::new_v4()),
        |digest| format!("webhook_msg_{digest}"),
    )
}

fn whatsapp_memory_key(msg: &crate::channels::traits::ChannelMessage) -> String {
    format!("whatsapp_{}_{}", msg.sender, msg.id)
}

fn linq_memory_key(msg: &crate::channels::traits::ChannelMessage) -> String {
    format!("linq_{}_{}", msg.sender, msg.id)
}

fn nextcloud_talk_memory_key(msg: &crate::channels::traits::ChannelMessage) -> String {
    format!("nextcloud_talk_{}_{}", msg.sender, msg.id)
}

fn is_group_reply_target(reply_target: &str) -> bool {
    reply_target.contains("group:") || reply_target.contains("@g.us")
}

fn should_autosave_gateway_message(reply_target: Option<&str>, content: &str) -> bool {
    if !memory::should_autosave_content(content) {
        return false;
    }
    !reply_target.is_some_and(is_group_reply_target)
}

#[derive(Debug, Clone)]
struct GatewayFabricContext {
    channel: String,
    session_key: String,
    sender: String,
    recipient: String,
    idempotency_key: Option<String>,
}

impl GatewayFabricContext {
    fn generic_webhook(reply_target: Option<&str>, idempotency_key: Option<String>) -> Self {
        let target = reply_target.unwrap_or("webhook-client");
        Self {
            channel: "webhook".to_string(),
            session_key: format!("gateway:webhook:{target}"),
            sender: target.to_string(),
            recipient: "prx".to_string(),
            idempotency_key,
        }
    }

    fn channel_message(msg: &crate::channels::traits::ChannelMessage) -> Self {
        Self {
            channel: msg.channel.clone(),
            session_key: format!("gateway:{}:{}", msg.channel, msg.sender),
            sender: msg.sender.clone(),
            recipient: msg.reply_target.clone(),
            idempotency_key: Some(format!("gateway:{}:{}", msg.channel, msg.id)),
        }
    }
}

pub(super) fn hash_webhook_secret(value: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(value.as_bytes());
    hex::encode(digest)
}

fn verify_webhook_hmac_signature(secret: &str, body: &[u8], signature_header: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let signature_hex = signature_header
        .trim()
        .strip_prefix("sha256=")
        .unwrap_or_else(|| signature_header.trim());
    let Ok(provided) = hex::decode(signature_hex) else {
        return false;
    };

    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&provided).is_ok()
}

/// How often the rate limiter sweeps stale IP entries from its map.
const RATE_LIMITER_SWEEP_INTERVAL_SECS: u64 = 300; // 5 minutes

#[derive(Debug)]
struct SlidingWindowRateLimiter {
    limit_per_window: u32,
    window: Duration,
    max_keys: usize,
    requests: Mutex<(HashMap<String, Vec<Instant>>, Instant)>,
}

impl SlidingWindowRateLimiter {
    fn new(limit_per_window: u32, window: Duration, max_keys: usize) -> Self {
        Self {
            limit_per_window,
            window,
            max_keys: max_keys.max(1),
            requests: Mutex::new((HashMap::new(), Instant::now())),
        }
    }

    fn prune_stale(requests: &mut HashMap<String, Vec<Instant>>, cutoff: Instant) {
        requests.retain(|_, timestamps| {
            timestamps.retain(|t| *t > cutoff);
            !timestamps.is_empty()
        });
    }

    fn allow(&self, key: &str) -> bool {
        if self.limit_per_window == 0 {
            return true;
        }

        let now = Instant::now();
        let cutoff = now.checked_sub(self.window).unwrap_or(now);

        let mut guard = self.requests.lock();
        let (requests, last_sweep) = &mut *guard;

        // Periodic sweep: remove keys with no recent requests
        if last_sweep.elapsed() >= Duration::from_secs(RATE_LIMITER_SWEEP_INTERVAL_SECS) {
            Self::prune_stale(requests, cutoff);
            *last_sweep = now;
        }

        if !requests.contains_key(key) && requests.len() >= self.max_keys {
            // Opportunistic stale cleanup before eviction under cardinality pressure.
            Self::prune_stale(requests, cutoff);
            *last_sweep = now;

            if requests.len() >= self.max_keys {
                // Evict the key with the fewest recent requests.  FIFO (oldest)
                // eviction lets an attacker cycle through IPs and reset legitimate
                // users' rate-limit counters.  Evicting the least-active key is
                // harder to weaponize.
                let evict_key = requests
                    .iter()
                    .min_by_key(|(_, timestamps)| timestamps.len())
                    .map(|(k, _)| k.clone());
                if let Some(evict_key) = evict_key {
                    requests.remove(&evict_key);
                }
            }
        }

        let entry = requests.entry(key.to_owned()).or_default();
        entry.retain(|instant| *instant > cutoff);

        if entry.len() >= self.limit_per_window as usize {
            return false;
        }

        entry.push(now);
        true
    }
}

#[derive(Debug)]
pub struct GatewayRateLimiter {
    pair: SlidingWindowRateLimiter,
    webhook: SlidingWindowRateLimiter,
    webhook_credential: SlidingWindowRateLimiter,
    api: SlidingWindowRateLimiter,
}

impl GatewayRateLimiter {
    pub fn new(pair_per_minute: u32, webhook_per_minute: u32, api_per_minute: u32, max_keys: usize) -> Self {
        let window = Duration::from_secs(RATE_LIMIT_WINDOW_SECS);
        Self {
            pair: SlidingWindowRateLimiter::new(pair_per_minute, window, max_keys),
            webhook: SlidingWindowRateLimiter::new(webhook_per_minute, window, max_keys),
            webhook_credential: SlidingWindowRateLimiter::new(webhook_per_minute, window, max_keys),
            api: SlidingWindowRateLimiter::new(api_per_minute, window, max_keys),
        }
    }

    fn allow_pair(&self, key: &str) -> bool {
        self.pair.allow(key)
    }

    pub fn allow_webhook(&self, key: &str) -> bool {
        self.webhook.allow(key)
    }

    fn allow_webhook_credential(&self, key: &str) -> bool {
        self.webhook_credential.allow(key)
    }

    pub(super) fn allow_api(&self, key: &str) -> bool {
        self.api.allow(key)
    }
}

#[derive(Debug)]
pub struct IdempotencyStore {
    ttl: Duration,
    max_keys: usize,
    state: Mutex<IdempotencyStoreState>,
}

#[derive(Debug, Default)]
struct IdempotencyStoreState {
    entries: HashMap<String, IdempotencyEntry>,
    next_generation: u64,
    payload_bytes: usize,
}

#[derive(Debug)]
enum IdempotencyEntry {
    Processing {
        generation: u64,
        request_fingerprint: [u8; 32],
        reserved_bytes: usize,
    },
    Succeeded {
        request_fingerprint: [u8; 32],
        completed_at: Instant,
        response_id: Uuid,
        result_hash: String,
        replay: Option<IdempotencyReplay>,
    },
    Failed {
        request_fingerprint: [u8; 32],
        failed_at: Instant,
        retry_eligible: bool,
    },
}

impl IdempotencyEntry {
    const fn request_fingerprint(&self) -> &[u8; 32] {
        match self {
            Self::Processing {
                request_fingerprint, ..
            }
            | Self::Succeeded {
                request_fingerprint, ..
            }
            | Self::Failed {
                request_fingerprint, ..
            } => request_fingerprint,
        }
    }
}

#[derive(Debug, Clone)]
struct IdempotencyReplay {
    response_id: Uuid,
    response: Arc<str>,
    model: Arc<str>,
}

impl IdempotencyReplay {
    fn payload_bytes(&self) -> usize {
        self.response.len().saturating_add(self.model.len())
    }

    fn json_body(&self) -> serde_json::Value {
        serde_json::json!({
            "status": "duplicate",
            "idempotent": true,
            "response": self.response.as_ref(),
            "model": self.model.as_ref(),
            "response_id": self.response_id.to_string(),
        })
    }
}

#[derive(Debug)]
enum IdempotencyClaimOutcome {
    Acquired(IdempotencyClaim),
    Processing,
    Replay(IdempotencyReplay),
    ReplayUnavailable { response_id: Uuid, result_hash: String },
    RequestConflict,
    RetryUnavailable,
    AtCapacity,
}

#[derive(Debug)]
struct IdempotencyClaim {
    store: Arc<IdempotencyStore>,
    key_digest: String,
    generation: u64,
    armed: bool,
}

impl IdempotencyClaim {
    fn succeed(mut self, replay: IdempotencyReplay, result_hash: String) -> bool {
        let transitioned = self
            .store
            .complete_if_owner(&self.key_digest, self.generation, replay, result_hash);
        if transitioned {
            self.armed = false;
        }
        transitioned
    }

    fn fail(mut self, retry_eligible: bool) -> bool {
        let transitioned = self
            .store
            .fail_if_owner(&self.key_digest, self.generation, retry_eligible);
        if transitioned {
            self.armed = false;
        }
        transitioned
    }
}

impl Drop for IdempotencyClaim {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.store.fail_if_owner(&self.key_digest, self.generation, true);
        }
    }
}

impl IdempotencyStore {
    pub fn new(ttl: Duration, max_keys: usize) -> Self {
        Self {
            ttl,
            max_keys: max_keys.max(1),
            state: Mutex::new(IdempotencyStoreState::default()),
        }
    }

    fn prune_expired_terminal(&self, state: &mut IdempotencyStoreState, now: Instant) {
        let expired = state
            .entries
            .iter()
            .filter_map(|(key, entry)| {
                let terminal_at = match entry {
                    IdempotencyEntry::Processing { .. } => return None,
                    IdempotencyEntry::Succeeded { completed_at, .. } => *completed_at,
                    IdempotencyEntry::Failed { failed_at, .. } => *failed_at,
                };
                (now.saturating_duration_since(terminal_at) >= self.ttl).then(|| key.clone())
            })
            .collect::<Vec<_>>();

        for key in expired {
            if let Some(IdempotencyEntry::Succeeded {
                replay: Some(replay), ..
            }) = state.entries.remove(&key)
            {
                state.payload_bytes = state.payload_bytes.saturating_sub(replay.payload_bytes());
            }
        }
    }

    fn reserve_processing(
        self: &Arc<Self>,
        state: &mut IdempotencyStoreState,
        key_digest: String,
        request_fingerprint: [u8; 32],
    ) -> IdempotencyClaimOutcome {
        let Some(next_payload_bytes) = state.payload_bytes.checked_add(IDEMPOTENCY_MAX_REPLAY_BYTES) else {
            return IdempotencyClaimOutcome::AtCapacity;
        };
        if next_payload_bytes > IDEMPOTENCY_REPLAY_BUDGET_BYTES {
            return IdempotencyClaimOutcome::AtCapacity;
        }
        let Some(generation) = state.next_generation.checked_add(1) else {
            return IdempotencyClaimOutcome::AtCapacity;
        };
        state.next_generation = generation;
        state.payload_bytes = next_payload_bytes;
        state.entries.insert(
            key_digest.clone(),
            IdempotencyEntry::Processing {
                generation,
                request_fingerprint,
                reserved_bytes: IDEMPOTENCY_MAX_REPLAY_BYTES,
            },
        );
        IdempotencyClaimOutcome::Acquired(IdempotencyClaim {
            store: Arc::clone(self),
            key_digest,
            generation,
            armed: true,
        })
    }

    fn claim(self: &Arc<Self>, key_digest: String, request_fingerprint: [u8; 32]) -> IdempotencyClaimOutcome {
        let now = Instant::now();
        let mut state = self.state.lock();
        self.prune_expired_terminal(&mut state, now);

        enum Existing {
            Processing,
            Replay(IdempotencyReplay),
            ReplayUnavailable(Uuid, String),
            Retry,
            RetryUnavailable,
            RequestConflict,
        }
        let existing = state.entries.get(&key_digest).map(|entry| {
            if entry.request_fingerprint() != &request_fingerprint {
                return Existing::RequestConflict;
            }
            match entry {
                IdempotencyEntry::Processing { .. } => Existing::Processing,
                IdempotencyEntry::Succeeded {
                    response_id,
                    result_hash,
                    replay,
                    ..
                } => replay.as_ref().map_or_else(
                    || Existing::ReplayUnavailable(*response_id, result_hash.clone()),
                    |snapshot| Existing::Replay(snapshot.clone()),
                ),
                IdempotencyEntry::Failed { retry_eligible, .. } => {
                    if *retry_eligible {
                        Existing::Retry
                    } else {
                        Existing::RetryUnavailable
                    }
                }
            }
        });

        match existing {
            Some(Existing::Processing) => IdempotencyClaimOutcome::Processing,
            Some(Existing::Replay(replay)) => IdempotencyClaimOutcome::Replay(replay),
            Some(Existing::ReplayUnavailable(response_id, result_hash)) => IdempotencyClaimOutcome::ReplayUnavailable {
                response_id,
                result_hash,
            },
            Some(Existing::RetryUnavailable) => IdempotencyClaimOutcome::RetryUnavailable,
            Some(Existing::RequestConflict) => IdempotencyClaimOutcome::RequestConflict,
            Some(Existing::Retry) => self.reserve_processing(&mut state, key_digest, request_fingerprint),
            None => {
                if state.entries.len() >= self.max_keys {
                    return IdempotencyClaimOutcome::AtCapacity;
                }
                self.reserve_processing(&mut state, key_digest, request_fingerprint)
            }
        }
    }

    fn complete_if_owner(
        &self,
        key_digest: &str,
        generation: u64,
        replay: IdempotencyReplay,
        result_hash: String,
    ) -> bool {
        let mut state = self.state.lock();
        let Some(IdempotencyEntry::Processing {
            generation: current_generation,
            request_fingerprint,
            reserved_bytes,
            ..
        }) = state.entries.get(key_digest)
        else {
            return false;
        };
        if *current_generation != generation {
            return false;
        }

        let request_fingerprint = *request_fingerprint;
        let reserved_bytes = *reserved_bytes;
        let replay_bytes = replay.payload_bytes();
        let response_id = replay.response_id;
        state.payload_bytes = state.payload_bytes.saturating_sub(reserved_bytes);
        let replay = if replay_bytes <= IDEMPOTENCY_MAX_REPLAY_BYTES {
            state.payload_bytes = state.payload_bytes.saturating_add(replay_bytes);
            Some(replay)
        } else {
            None
        };
        state.entries.insert(
            key_digest.to_string(),
            IdempotencyEntry::Succeeded {
                request_fingerprint,
                completed_at: Instant::now(),
                response_id,
                result_hash,
                replay,
            },
        );
        true
    }

    fn fail_if_owner(&self, key_digest: &str, generation: u64, retry_eligible: bool) -> bool {
        let mut state = self.state.lock();
        let Some(IdempotencyEntry::Processing {
            generation: current_generation,
            request_fingerprint,
            reserved_bytes,
            ..
        }) = state.entries.get(key_digest)
        else {
            return false;
        };
        if *current_generation != generation {
            return false;
        }

        let request_fingerprint = *request_fingerprint;
        let reserved_bytes = *reserved_bytes;
        state.payload_bytes = state.payload_bytes.saturating_sub(reserved_bytes);
        state.entries.insert(
            key_digest.to_string(),
            IdempotencyEntry::Failed {
                request_fingerprint,
                failed_at: Instant::now(),
                retry_eligible,
            },
        );
        true
    }
}

fn webhook_request_fingerprint(body: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(body).into()
}

fn webhook_idempotency_digest(scope: &str, raw_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(scope.as_bytes());
    hasher.update([0]);
    hasher.update(raw_key.as_bytes());
    hex::encode(hasher.finalize())
}

fn webhook_result_hash(response: &str, model: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(response.as_bytes());
    hasher.update([0]);
    hasher.update(model.as_bytes());
    hex::encode(hasher.finalize())
}

fn parse_client_ip(value: &str) -> Option<IpAddr> {
    let value = value.trim().trim_matches('"').trim();
    if value.is_empty() {
        return None;
    }

    if let Ok(ip) = value.parse::<IpAddr>() {
        return Some(ip);
    }

    if let Ok(addr) = value.parse::<SocketAddr>() {
        return Some(addr.ip());
    }

    let value = value.trim_matches(['[', ']']);
    value.parse::<IpAddr>().ok()
}

fn forwarded_client_ip(headers: &HeaderMap) -> Option<IpAddr> {
    // Prefer X-Real-IP (single trusted proxy) over X-Forwarded-For
    if let Some(real_ip) = headers
        .get("X-Real-IP")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_client_ip)
    {
        return Some(real_ip);
    }

    // RFC 7239: use the LAST (rightmost) IP in X-Forwarded-For, which is the
    // one added by the closest trusted proxy.  Using the first IP is spoofable
    // by the client and can bypass rate limiting.
    if let Some(xff) = headers.get("X-Forwarded-For").and_then(|v| v.to_str().ok()) {
        for candidate in xff.rsplit(',') {
            if let Some(ip) = parse_client_ip(candidate) {
                return Some(ip);
            }
        }
    }

    None
}

pub(super) fn client_key_from_request(
    peer_addr: Option<SocketAddr>,
    headers: &HeaderMap,
    trust_forwarded_headers: bool,
) -> String {
    if trust_forwarded_headers {
        if let Some(ip) = forwarded_client_ip(headers) {
            return ip.to_string();
        }
    }

    peer_addr
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn normalize_max_keys(configured: usize, fallback: usize) -> usize {
    if configured == 0 { fallback.max(1) } else { configured }
}

fn authorize_gateway_resource_mutation(
    state: &AppState,
    operation_name: &str,
    risk: ResourceRiskLevel,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    // D2: authorization reads the hot SharedConfig (D) snapshot, NOT the cached
    // `state.config` Mutex (C), so a config reload (autonomy / security.audit change)
    // takes effect at this decision point without a restart. Logic is otherwise
    // identical to the prior C-based path: same `build_security_policy` construction
    // (FIX-P1-31 audit-config wiring preserved), same gate, same allow/deny result.
    let config = state.shared_config.load_full();
    let policy = crate::runtime::bootstrap::build_security_policy(&config);
    SideEffectGate::new(&policy)
        .authorize_resource_operation("gateway", operation_name, risk, None)
        .map(|_| ())
        .map_err(|error| (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": error}))))
}

fn gateway_channel_webhook_operation(channel: &str, action: &str) -> String {
    format!("gateway:channel_webhook:{channel}:{action}")
}

/// Shared state for all axum handlers
#[derive(Clone)]
pub struct AppState {
    /// Cached config (C) — a NON-authoritative, NON-authorization snapshot.
    ///
    /// D2 invariant: **no security / authorization decision may read `config`.**
    /// All allow/deny gating reads [`shared_config`](Self::shared_config) (D) via
    /// `load_full()` so config reloads take effect without a restart. `config` is
    /// kept only for display/persist paths (serving the current config to the Web
    /// Console, resolving `workspace_dir` for upload paths, persisting paired
    /// tokens, etc.). Every config mutation route still dual-writes C **and** D
    /// (holding this Mutex while it `.store()`s D) so the two stay consistent for
    /// those non-security consumers; see `api/config.rs`.
    pub config: Arc<Mutex<Config>>,
    /// ArcSwap-backed hot config (D) — the SINGLE source of truth for every gateway
    /// authorization decision (P1.5 / P3-1, D2). Updated by the daemon file-watch
    /// hot-reload manager, the `ConfigReloadTool`, and the `/api/config/reload`
    /// route; read lock-free via `load_full()` at each authz point.
    pub shared_config: crate::config::SharedConfig,
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub temperature: f64,
    pub mem: Arc<dyn Memory>,
    pub auto_save: bool,
    /// Tools available to the agent loop (shell, file I/O, memory, etc.)
    pub tools_registry: Arc<Vec<Box<dyn Tool>>>,
    /// Shared reference to the MCP tool for runtime introspection (discovered tools, etc.).
    pub mcp_tool: Option<Arc<McpTool>>,
    /// Hook manager for lifecycle events.
    pub hooks: Arc<HookManager>,
    /// SHA-256 hash of `webhook.token` for `X-Webhook-Token` auth.
    pub webhook_token_hash: Option<Arc<str>>,
    /// HMAC signing secret for `X-Webhook-Signature` verification.
    pub webhook_signing_secret: Option<Arc<str>>,
    pub pairing: Arc<PairingGuard>,
    pub trust_forwarded_headers: bool,
    pub rate_limiter: Arc<GatewayRateLimiter>,
    pub idempotency_store: Arc<IdempotencyStore>,
    pub whatsapp: Option<Arc<WhatsAppChannel>>,
    pub signal: Option<Arc<SignalChannel>>,
    /// `WhatsApp` app secret for webhook signature verification (`X-Hub-Signature-256`)
    pub whatsapp_app_secret: Option<Arc<str>>,
    pub linq: Option<Arc<LinqChannel>>,
    /// Linq webhook signing secret for signature verification
    pub linq_signing_secret: Option<Arc<str>>,
    pub nextcloud_talk: Option<Arc<NextcloudTalkChannel>>,
    /// Nextcloud Talk webhook secret for signature verification
    pub nextcloud_talk_webhook_secret: Option<Arc<str>>,
    /// Observability backend for metrics scraping
    pub observer: Arc<dyn crate::observability::Observer>,
    /// Gateway boot instant used for uptime in Web Console status API.
    pub start_time: Instant,
    /// Actual bound gateway port (supports dynamic port assignment).
    pub gateway_port: u16,
    /// Web Console log stream broadcast channel.
    pub logs_broadcast_tx: broadcast::Sender<String>,
    /// WASM plugin manager (optional, enabled with `--features wasm-plugins`).
    #[cfg(feature = "wasm-plugins")]
    pub plugin_manager: Option<Arc<crate::plugins::PluginManager>>,
    /// WASM middleware chain for message pipeline interception.
    #[cfg(feature = "wasm-plugins")]
    pub wasm_middleware: Option<Arc<crate::plugins::capabilities::middleware::MiddlewareChain>>,
    /// WASM hook executor for lifecycle event observation.
    #[cfg(feature = "wasm-plugins")]
    pub wasm_hook_executor: Option<Arc<crate::plugins::capabilities::hook::WasmHookExecutor>>,
    /// WASM cron manager for scheduled plugin tasks.
    #[cfg(feature = "wasm-plugins")]
    pub wasm_cron_manager: Option<Arc<crate::plugins::capabilities::cron::WasmCronManager>>,
    /// Shared event bus for inter-plugin communication.
    #[cfg(feature = "wasm-plugins")]
    pub event_bus: Option<Arc<crate::plugins::event_bus::EventBus>>,
}

/// Run the HTTP gateway using axum with proper HTTP/1.1 compliance.
///
/// `shared_config` is the single SharedConfig snapshot the gateway uses for ALL
/// authorization decisions (D2). When the gateway runs under the daemon, the
/// daemon owns this handle and also wires it to the file-watch hot-reload manager,
/// so config.toml edits become visible at every gateway authz point. When invoked
/// directly via `prx gateway` (no daemon), `None` is passed and the gateway builds
/// its own fallback handle — in that mode only the in-gateway ConfigReloadTool / the
/// `/api/config/reload` route can update it (no file watcher).
#[allow(clippy::too_many_lines)]
pub async fn run_gateway(
    host: &str,
    port: u16,
    config: Config,
    shared_config: Option<crate::config::SharedConfig>,
    shutdown: CancellationToken,
) -> Result<()> {
    let start_time = Instant::now();
    crate::health::register_component(
        "gateway",
        "gateway",
        true,
        Duration::from_secs(60),
        crate::health::ComponentState::Starting,
    );
    // ── Security: refuse public bind without tunnel or explicit opt-in ──
    if is_public_bind(host) && config.tunnel.provider == "none" && !config.gateway.allow_public_bind {
        anyhow::bail!(
            "🛑 Refusing to bind to {host} — gateway would be exposed to the internet.\n\
             Fix: use --host 127.0.0.1 (default), configure a tunnel, or set\n\
             [gateway] allow_public_bind = true in config.toml (NOT recommended)."
        );
    }
    // C: cached config for display/persist only (see `AppState::config`). It is
    // NOT read by any authorization path (those read `shared_config`, D). On the
    // reload-only paths (daemon file-watch + ConfigReloadTool / `/api/config/reload`)
    // C intentionally lags D: only the explicit `/api/config` POST/PUT routes
    // dual-write C+D. The lag is display-only and never affects allow/deny.
    let config_state = Arc::new(Mutex::new(config.clone()));

    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let actual_port = listener.local_addr()?.port();
    let display_addr = format!("{host}:{actual_port}");

    let provider_runtime_options = providers::provider_runtime_options_from_config(&config);
    let provider: Arc<dyn Provider> = Arc::from(providers::create_resilient_provider_with_options(
        config.default_provider.as_deref().unwrap_or("openrouter"),
        config.api_key.as_deref(),
        config.api_url.as_deref(),
        &config.reliability,
        &provider_runtime_options,
    )?);
    let model = config
        .default_model
        .clone()
        .unwrap_or_else(|| "anthropic/claude-sonnet-4".into());
    let temperature = config.default_temperature;
    let mem: Arc<dyn Memory> = Arc::from(memory::create_memory_with_storage(
        &config.memory,
        Some(&config.storage.provider.config),
        &config.workspace_dir,
        config.api_key.as_deref(),
    )?);
    let runtime: Arc<dyn runtime::RuntimeAdapter> = Arc::from(runtime::create_runtime(&config.runtime)?);
    // FIX-P1-31: honour the configured `security.audit` block on the gate audit path.
    // Built via the shared `build_security_policy` helper so this site cannot drift
    // from (or forget) the audit-config wiring — byte-for-byte identical to the
    // former local from_config + audit-config construction.
    //
    // D2 SCOPE / restart-only boundary: this `security` is a STARTUP snapshot baked
    // into every tool instance constructed below (each tool carries its own
    // `SideEffectGate`). It is therefore **restart-only** — a config reload does NOT
    // re-arm the security gate already embedded in a live tool. Tool-execution
    // authorization is per the user-ruled restart-only scope and is intentionally NOT
    // hot in this increment. What IS hot (reads the SharedConfig D at decision time):
    // gateway resource-mutation routes (`authorize_gateway_resource_mutation`), the
    // `/api/*` config-mutation gates (`authorize_resource_mutation`), per-session /
    // console runtime-turn policies, and the `/api/config/reload` route's own gate.
    // The cron / webhook / evolution supervisors likewise use a restart-only security
    // snapshot (their hot-reload is out of this increment's scope).
    let security = crate::runtime::bootstrap::build_security_policy(&config);

    let (composio_key, composio_entity_id) = if config.composio.enabled {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    };

    // Build the base tool list (mutable so we can append channel-aware tools below)
    let tools_result = tools::all_tools_with_runtime_ext(
        Arc::new(config.clone()),
        &security,
        runtime,
        Arc::clone(&mem),
        composio_key,
        composio_entity_id,
        &config.browser,
        &config.http_request,
        &config.workspace_dir,
        &config.agents,
        config.api_key.as_deref(),
        &config,
    );
    let mut tools_list = tools_result.tools;
    let mcp_tool = tools_result.mcp_tool;
    let hooks = Arc::new(HookManager::new(config.workspace_dir.clone()));
    // Generic /webhook auth can require a standalone token and/or HMAC signature.
    let webhook_token_hash: Option<Arc<str>> = config.webhook.token.as_ref().and_then(|raw_token| {
        let trimmed_token = raw_token.trim();
        (!trimmed_token.is_empty()).then(|| Arc::<str>::from(hash_webhook_secret(trimmed_token)))
    });
    let webhook_signing_secret: Option<Arc<str>> = config.channels_config.webhook.as_ref().and_then(|webhook| {
        webhook.secret.as_ref().and_then(|raw_secret| {
            let trimmed_secret = raw_secret.trim();
            (!trimmed_secret.is_empty()).then(|| Arc::<str>::from(trimmed_secret.to_string()))
        })
    });

    // WhatsApp channel (if configured)
    let whatsapp_channel: Option<Arc<WhatsAppChannel>> = config
        .channels_config
        .whatsapp
        .as_ref()
        .filter(|wa| wa.is_cloud_config())
        .map(|wa| {
            Arc::new(WhatsAppChannel::new(
                wa.access_token.clone().unwrap_or_default(),
                wa.phone_number_id.clone().unwrap_or_default(),
                wa.verify_token.clone().unwrap_or_default(),
                wa.allowed_numbers.clone(),
            ))
        });
    let signal_media_config = config.media.clone();
    let signal_channel: Option<Arc<SignalChannel>> = config.channels_config.signal.as_ref().map(|sg| {
        Arc::new(SignalChannel::new_with_mode(
            sg.effective_http_url(),
            sg.account.clone(),
            sg.group_id.clone(),
            sg.allowed_from.clone(),
            sg.ignore_attachments,
            sg.ignore_stories,
            signal_media_config.clone(),
            sg.is_native_mode(),
            sg.data_dir.clone(),
            sg.storm_protection.clone(),
        ))
    });

    // Register message_send tool backed by Signal when the channel is configured.
    // For other channels (WhatsApp, Linq, etc.) we register a generic sender without
    // reaction support so the tool is still available for text/file messages.
    if let Some(ref sc) = signal_channel {
        let msg_send_tool = tools::MessageSendTool::new_signal(sc.clone(), security.clone());
        tools_list.push(Box::new(msg_send_tool));
    }

    // Linq channel (if configured). Built here (ahead of the spawn tool) so it can
    // join the sessions_spawn per-turn announce/kill routing registry below.
    let linq_channel: Option<Arc<LinqChannel>> = config.channels_config.linq.as_ref().map(|lq| {
        Arc::new(LinqChannel::new(
            lq.api_token.clone(),
            lq.from_phone.clone(),
            lq.allowed_senders.clone(),
        ))
    });

    // Nextcloud Talk channel (if configured). Built here (ahead of the spawn tool)
    // so it can join the sessions_spawn per-turn announce/kill routing registry.
    let nextcloud_talk_channel: Option<Arc<NextcloudTalkChannel>> =
        config.channels_config.nextcloud_talk.as_ref().map(|nc| {
            Arc::new(NextcloudTalkChannel::new(
                nc.base_url.clone(),
                nc.app_token.clone(),
                nc.allowed_users.clone(),
            ))
        });

    // Register sessions_spawn tool backed by Signal (if configured) so the LLM can
    // fire off async sub-agent tasks that announce their results when complete.
    // Keep the OnceLock handle so we can inject the full tools_registry post-wrap.
    let spawn_tools_handle = if let Some(ref sc) = signal_channel {
        let provider_name = config.default_provider.as_deref().unwrap_or("openrouter").to_string();
        // Per-turn announce/kill routing registry: every configured gateway channel
        // keyed by name. A webhook turn stamps its originating channel into the
        // tool's per-turn scope; sessions_spawn binds that name to each run and
        // resolves the channel object from here at announce/kill time, so a result
        // is never mis-routed to the wrong channel under concurrent webhooks.
        let mut spawn_channels_by_name: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        spawn_channels_by_name.insert(sc.name().to_string(), sc.clone() as Arc<dyn Channel>);
        if let Some(ref wa) = whatsapp_channel {
            spawn_channels_by_name.insert(wa.name().to_string(), wa.clone() as Arc<dyn Channel>);
        }
        if let Some(ref lq) = linq_channel {
            spawn_channels_by_name.insert(lq.name().to_string(), lq.clone() as Arc<dyn Channel>);
        }
        if let Some(ref nc) = nextcloud_talk_channel {
            spawn_channels_by_name.insert(nc.name().to_string(), nc.clone() as Arc<dyn Channel>);
        }
        let spawn_tool = tools::SessionsSpawnTool::new(
            sc.clone() as Arc<dyn Channel>,
            Arc::clone(&provider),
            provider_name,
            model.clone(),
            temperature,
            security.clone(),
            config.workspace_dir.clone(),
            config.multimodal.clone(),
            config.agent.compaction.clone(),
            config.agents.clone(),
            config.api_key.clone(),
            provider_runtime_options.clone(),
            config.sessions_spawn.clone(),
        )
        .with_compaction_resolver(crate::router::CompactionResolver::new(
            config.agent.compaction.clone(),
            config.router.clone(),
            config.model_routes.clone(),
        ))
        .with_channels(Arc::new(spawn_channels_by_name))
        .with_shared_memory(Arc::clone(&mem))
        .with_event_recording(config.memory.event_recording_config());
        let handle = spawn_tool.tools_handle();
        tools_list.push(Box::new(spawn_tool));
        Some(handle)
    } else {
        None
    };

    // D2: the SharedConfig (D) is the single source of truth for all gateway
    // authorization decisions. Use the daemon-injected handle when present (so the
    // daemon's file-watch hot-reload is observed here); otherwise build a fallback
    // for the standalone `prx gateway` path. The ConfigReloadTool and the
    // `/api/config/reload` route both `.store()` into THIS handle, and every authz
    // helper reads from it, so a reload is immediately visible to allow/deny.
    let shared_config_for_reload =
        shared_config.unwrap_or_else(|| Arc::new(arc_swap::ArcSwap::from_pointee(config.clone())));
    tools_list.push(Box::new(tools::ConfigReloadTool::with_security(
        Arc::clone(&shared_config_for_reload),
        security.clone(),
    )));

    // ── Register WASM plugin tools and create plugin manager (if feature enabled) ──
    #[cfg(feature = "wasm-plugins")]
    let (wasm_plugin_manager, wasm_mw_chain, wasm_hook_exec, wasm_cron_mgr, wasm_event_bus) = {
        let pm = crate::plugins::init_plugin_manager(&config.workspace_dir).await;
        let mut mw = None;
        let mut he = None;
        let mut cm = None;
        // Always create an event bus when the wasm-plugins feature is active.
        let bus = Arc::new(crate::plugins::event_bus::EventBus::new());
        if let Some(ref pm) = pm {
            // Tool adapters
            let wasm_tools = pm
                .create_tool_adapters_with_memory(Some(Arc::clone(&mem)), Some(Arc::clone(&bus)))
                .await;
            if !wasm_tools.is_empty() {
                tracing::info!(
                    count = wasm_tools.len(),
                    "registering WASM plugin tools in tools_registry"
                );
                tools_list.extend(wasm_tools);
            }
            // Middleware chain
            let chain = pm.create_middleware_chain(Some(Arc::clone(&bus))).await;
            if !chain.is_empty() {
                tracing::info!(count = chain.len(), "WASM middleware chain ready");
                mw = Some(Arc::new(chain));
            }
            // Hook executor
            let executor = pm.create_hook_executor(Some(Arc::clone(&bus))).await;
            if !executor.is_empty() {
                tracing::info!("WASM hook executor ready");
                he = Some(Arc::new(executor));
            }
            // Cron manager
            let cron = pm.create_cron_manager(Some(Arc::clone(&bus))).await;
            if !cron.is_empty() {
                tracing::info!(count = cron.jobs().len(), "WASM cron manager ready");
                cm = Some(Arc::new(cron));
            }
        }
        tracing::debug!("WASM event bus ready");
        (pm, mw, he, cm, Some(bus))
    };

    let tools_registry = Arc::new(tools_list);

    // Inject the tools registry into sessions_spawn so sub-agents can use tools.
    if let Some(handle) = spawn_tools_handle {
        handle.set(Arc::clone(&tools_registry)).ok();
    }

    // WhatsApp app secret for webhook signature verification (from config)
    let whatsapp_app_secret: Option<Arc<str>> = config
        .channels_config
        .whatsapp
        .as_ref()
        .and_then(|wa| {
            wa.app_secret
                .as_deref()
                .map(str::trim)
                .filter(|secret| !secret.is_empty())
                .map(ToOwned::to_owned)
        })
        .map(Arc::from);

    // Linq signing secret for webhook signature verification (from config).
    // (The `linq_channel` object itself is built earlier, ahead of the spawn tool.)
    let linq_signing_secret: Option<Arc<str>> = config
        .channels_config
        .linq
        .as_ref()
        .and_then(|lq| {
            lq.signing_secret
                .as_deref()
                .map(str::trim)
                .filter(|secret| !secret.is_empty())
                .map(ToOwned::to_owned)
        })
        .map(Arc::from);

    // Nextcloud Talk webhook secret for signature verification (from config).
    // (The `nextcloud_talk_channel` object is built earlier, ahead of the spawn tool.)
    let nextcloud_talk_webhook_secret: Option<Arc<str>> = config
        .channels_config
        .nextcloud_talk
        .as_ref()
        .and_then(|nc| {
            nc.webhook_secret
                .as_deref()
                .map(str::trim)
                .filter(|secret| !secret.is_empty())
                .map(ToOwned::to_owned)
        })
        .map(Arc::from);

    // ── Pairing guard ──────────────────────────────────────
    let pairing = Arc::new(PairingGuard::new(
        config.gateway.require_pairing,
        &config.gateway.paired_tokens,
    ));
    let rate_limit_max_keys = normalize_max_keys(config.gateway.rate_limit_max_keys, RATE_LIMIT_MAX_KEYS_DEFAULT);
    let rate_limiter = Arc::new(GatewayRateLimiter::new(
        config.gateway.pair_rate_limit_per_minute,
        config.gateway.webhook_rate_limit_per_minute,
        config.gateway.api_rate_limit_per_minute,
        rate_limit_max_keys,
    ));
    let idempotency_max_keys = normalize_max_keys(config.gateway.idempotency_max_keys, IDEMPOTENCY_MAX_KEYS_DEFAULT);
    let idempotency_store = Arc::new(IdempotencyStore::new(
        Duration::from_secs(config.gateway.idempotency_ttl_secs.max(1)),
        idempotency_max_keys,
    ));

    // ── Tunnel ────────────────────────────────────────────────
    let tunnel = crate::tunnel::create_tunnel(&config.tunnel)?;
    let mut tunnel_url: Option<String> = None;

    if let Some(ref tun) = tunnel {
        println!("🔗 Starting {} tunnel...", tun.name());
        match tun.start(host, actual_port).await {
            Ok(url) => {
                println!("🌐 Tunnel active: {url}");
                tunnel_url = Some(url);
            }
            Err(e) => {
                println!("⚠️  Tunnel failed to start: {e}");
                println!("   Falling back to local-only mode.");
            }
        }
    }

    println!("🦀 OpenPRX Gateway listening on http://{display_addr}");
    if let Some(ref url) = tunnel_url {
        println!("  🌐 Public URL: {url}");
    }
    println!("  POST /pair      — pair a new client (X-Pairing-Code header)");
    println!("  POST /webhook   — {{\"message\": \"your prompt\"}}");
    if whatsapp_channel.is_some() {
        println!("  GET  /whatsapp  — Meta webhook verification");
        println!("  POST /whatsapp  — WhatsApp message webhook");
    }
    if linq_channel.is_some() {
        println!("  POST /linq      — Linq message webhook (iMessage/RCS/SMS)");
    }
    if nextcloud_talk_channel.is_some() {
        println!("  POST /nextcloud-talk — Nextcloud Talk bot webhook");
    }
    println!("  GET  /health    — health check");
    println!("  GET  /metrics   — Prometheus metrics");
    if let Some(code) = pairing.pairing_code() {
        println!();
        println!("  🔐 PAIRING REQUIRED — use this one-time code:");
        println!("     ┌──────────────┐");
        println!("     │  {code}  │");
        println!("     └──────────────┘");
        println!("     Send: POST /pair with header X-Pairing-Code: {code}");
    } else if pairing.require_pairing() {
        println!("  🔒 Pairing: ACTIVE (bearer token required)");
    } else {
        println!("  ⚠️  Pairing: DISABLED (all requests accepted)");
    }
    println!("  Press Ctrl+C to stop.\n");

    // Build shared state
    let observer: Arc<dyn crate::observability::Observer> =
        Arc::from(crate::observability::create_observer(&config.observability));
    let (logs_broadcast_tx, _) = broadcast::channel(1024);

    let state = AppState {
        config: config_state,
        shared_config: Arc::clone(&shared_config_for_reload),
        provider,
        model,
        temperature,
        mem,
        auto_save: config.memory.auto_save && config.memory.semantic.auto_promote_user_messages,
        tools_registry,
        mcp_tool,
        hooks,
        webhook_token_hash,
        webhook_signing_secret,
        pairing,
        trust_forwarded_headers: config.gateway.trust_forwarded_headers,
        rate_limiter,
        idempotency_store,
        whatsapp: whatsapp_channel,
        signal: signal_channel,
        whatsapp_app_secret,
        linq: linq_channel,
        linq_signing_secret,
        nextcloud_talk: nextcloud_talk_channel,
        nextcloud_talk_webhook_secret,
        observer,
        start_time,
        gateway_port: actual_port,
        logs_broadcast_tx,
        #[cfg(feature = "wasm-plugins")]
        plugin_manager: wasm_plugin_manager,
        #[cfg(feature = "wasm-plugins")]
        wasm_middleware: wasm_mw_chain,
        #[cfg(feature = "wasm-plugins")]
        wasm_hook_executor: wasm_hook_exec,
        #[cfg(feature = "wasm-plugins")]
        wasm_cron_manager: wasm_cron_mgr,
        #[cfg(feature = "wasm-plugins")]
        event_bus: wasm_event_bus,
    };

    // Inject WASM hook executor into HookManager so .emit() triggers WASM hooks too.
    #[cfg(feature = "wasm-plugins")]
    if let Some(ref exec) = state.wasm_hook_executor {
        state.hooks.set_wasm_executor(Arc::clone(exec)).await;
    }

    // Inject event bus into HookManager so lifecycle events bridge to inter-plugin topics.
    #[cfg(feature = "wasm-plugins")]
    if let Some(ref bus) = state.event_bus {
        state.hooks.set_event_bus(Arc::clone(bus)).await;
    }

    let limited_public_routes = Router::new()
        .route("/pair", post(handle_pair))
        .route("/webhook", post(handle_webhook))
        .route("/whatsapp", get(handle_whatsapp_verify))
        .route("/whatsapp", post(handle_whatsapp_message))
        .route("/linq", post(handle_linq_webhook))
        .route("/nextcloud-talk", post(handle_nextcloud_talk_webhook))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_SIZE));

    let api_routes = api::router(state.clone()).layer(RequestBodyLimitLayer::new(MAX_API_BODY_SIZE));

    // Build router with middleware
    let app = Router::new()
        .route("/health", get(handle_health))
        .route("/metrics", get(handle_metrics))
        .route(
            "/mcp/v1/initialize",
            get(compat::mcp_initialize).post(compat::mcp_initialize),
        )
        .route(
            "/mcp/v1/list_servers",
            get(compat::mcp_list_servers).post(compat::mcp_list_servers),
        )
        .route(
            "/mcp/v1/tools/list",
            get(compat::mcp_tools_list).post(compat::mcp_tools_list),
        )
        .route("/mcp/v1/tools/call", post(compat::mcp_tools_call))
        .route("/a2a/v1/identity", get(compat::a2a_identity).post(compat::a2a_identity))
        .route("/a2a/v1/discover", get(compat::a2a_discover).post(compat::a2a_discover))
        .route("/.well-known/agent.json", get(compat::well_known_agent_json))
        .route("/a2a/v1/.well-known/jwks.json", get(compat::a2a_jwks))
        .merge(limited_public_routes)
        .nest("/api", api_routes)
        .merge(ui::router())
        .with_state(state)
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(config.gateway.request_timeout_secs.max(1)),
        ));

    // The listener is bound and every route/state dependency is now constructed.
    // This explicit acknowledgement, rather than supervisor task survival, is the
    // gateway readiness boundary.
    crate::health::mark_component_ok("gateway");

    // Run the server with graceful shutdown (D5/D9 step 3, DEV-02). On root token
    // cancellation axum stops accepting and drains in-flight requests. The drain
    // bound must only start counting *after* shutdown is requested — never while
    // the server is idle — otherwise the gateway would self-exit after the timeout
    // on a quiet listener. We therefore arm the bounded timer inside a select! arm
    // gated on `shutdown.cancelled()`, so the timeout window opens only post-shutdown.
    let drain_deadline = shutdown.clone();
    let serve_fut = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .into_future();
    tokio::pin!(serve_fut);
    tokio::select! {
        res = &mut serve_fut => res?,
        _ = async {
            drain_deadline.cancelled().await;
            tokio::time::sleep(GATEWAY_GRACEFUL_TIMEOUT).await;
        } => {
            tracing::warn!(
                timeout_secs = GATEWAY_GRACEFUL_TIMEOUT.as_secs(),
                "gateway graceful shutdown timed out; forcing exit"
            );
        }
    }

    crate::health::mark_component_stopping("gateway");
    crate::health::mark_component_stopped("gateway");
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
// AXUM HANDLERS
// ══════════════════════════════════════════════════════════════════════════════

/// GET /health — always public (no secrets leaked)
async fn handle_health(State(state): State<AppState>) -> impl IntoResponse {
    // A successfully dispatched health request is also an explicit freshness
    // acknowledgement from the gateway owner.
    crate::health::mark_component_ok("gateway");
    let runtime = crate::health::snapshot();
    health_response(state.pairing.is_paired(), runtime)
}

fn health_response(paired: bool, runtime: crate::health::HealthSnapshot) -> (StatusCode, Json<serde_json::Value>) {
    let readiness = crate::health::readiness_from_snapshot(&runtime);
    let status = if readiness.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = serde_json::json!({
        "status": readiness.status,
        "paired": paired,
        "readiness": readiness,
        "runtime": runtime,
    });
    (status, Json(body))
}

/// Prometheus content type for text exposition format.
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// GET /metrics — Prometheus text exposition format
async fn handle_metrics(State(state): State<AppState>) -> impl IntoResponse {
    // 合并 PrometheusObserver 主 registry + chat_metrics 独立 registry (S2.5 P1-A).
    // chat 4 个 counter 独立于 observer 物理隔离，必须显式合并才能被 scrape 到。
    let chat_reg = crate::observability::chat_metrics::chat_registry();
    let body = state
        .observer
        .as_ref()
        .as_any()
        .downcast_ref::<crate::observability::PrometheusObserver>()
        .map_or_else(
            || {
                // observer 非 Prometheus 时，chat 指标仍需暴露。
                format!(
                    "# Prometheus backend not enabled. Set [observability] backend = \"prometheus\" in config.\n{}",
                    crate::observability::prometheus::encode_registries(&[chat_reg])
                )
            },
            |prom| prom.encode_with_extras(&[chat_reg]),
        );

    (StatusCode::OK, [(header::CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)], body)
}

/// POST /pair — exchange one-time code for bearer token
#[axum::debug_handler]
async fn handle_pair(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let rate_key = client_key_from_request(Some(peer_addr), &headers, state.trust_forwarded_headers);
    if !state.rate_limiter.allow_pair(&rate_key) {
        tracing::warn!("/pair rate limit exceeded");
        let err = serde_json::json!({
            "error": "Too many pairing requests. Please retry later.",
            "retry_after": RATE_LIMIT_WINDOW_SECS,
        });
        return (StatusCode::TOO_MANY_REQUESTS, Json(err));
    }

    let code = headers
        .get("X-Pairing-Code")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if let Err(error) = authorize_gateway_resource_mutation(&state, "gateway:pair", ResourceRiskLevel::Low) {
        return error;
    }

    match state.pairing.try_pair(code, &rate_key).await {
        Ok(Some(token)) => {
            tracing::info!("🔐 New client paired successfully");
            if let Err(err) = persist_pairing_tokens(state.config.clone(), &state.shared_config, &state.pairing).await {
                tracing::error!("🔐 Pairing succeeded but token persistence failed: {err:#}");
                let body = serde_json::json!({
                    "paired": true,
                    "persisted": false,
                    "token": token,
                    "message": "Paired for this process, but failed to persist token to config.toml. Check config path and write permissions.",
                });
                return (StatusCode::OK, Json(body));
            }

            let body = serde_json::json!({
                "paired": true,
                "persisted": true,
                "token": token,
                "message": "Save this token — use it as Authorization: Bearer <token>"
            });
            (StatusCode::OK, Json(body))
        }
        Ok(None) => {
            tracing::warn!("🔐 Pairing attempt with invalid code");
            let err = serde_json::json!({"error": "Invalid pairing code"});
            (StatusCode::FORBIDDEN, Json(err))
        }
        Err(lockout_secs) => {
            tracing::warn!("🔐 Pairing locked out — too many failed attempts ({lockout_secs}s remaining)");
            let err = serde_json::json!({
                "error": format!("Too many failed attempts. Try again in {lockout_secs}s."),
                "retry_after": lockout_secs
            });
            (StatusCode::TOO_MANY_REQUESTS, Json(err))
        }
    }
}

async fn persist_pairing_tokens(
    config: Arc<Mutex<Config>>,
    shared_config: &crate::config::SharedConfig,
    pairing: &PairingGuard,
) -> Result<()> {
    let paired_tokens = pairing.tokens();
    // D2 (correctness): base the persisted config on the hot SharedConfig (D)
    // snapshot, NOT the cached C Mutex. C lags D on the reload-only paths; if we
    // cloned a stale C, saved it, and re-stored it, we would write hot-reloaded
    // fields back to their old values on disk AND clobber D. Cloning the D snapshot
    // guarantees we only add the pairing tokens on top of the current authoritative
    // config.
    let mut updated_cfg = (*shared_config.load_full()).clone();
    updated_cfg.gateway.paired_tokens = paired_tokens;
    updated_cfg
        .save()
        .await
        .context("Failed to persist paired tokens to config.toml")?;

    // Publish to D and re-sync C to it (hold the Mutex across the `.store()` so the
    // two stores stay observably consistent), preserving the C == D invariant.
    {
        let mut guard = config.lock();
        *guard = updated_cfg.clone();
        shared_config.store(Arc::new(updated_cfg));
    }
    Ok(())
}

async fn run_gateway_chat_with_multimodal(
    state: &AppState,
    provider_label: &str,
    message: &str,
    fabric_ctx: &GatewayFabricContext,
) -> anyhow::Result<String> {
    let workspace_id = state.config.lock().workspace_dir.to_string_lossy().to_string();
    let event_recording = state.config.lock().memory.event_recording_config();
    let fabric = MemoryFabric::new(state.mem.clone(), workspace_id.clone()).with_event_recording(event_recording);
    let run_id = Uuid::new_v4().to_string();
    // D4 C4: migrate the gateway fabric durable session_key to the recipient-aware
    // canonical (`gateway:{channel}:{sender}:{recipient}`) while carrying the
    // pre-cutover legacy key (`gateway:webhook:{target}` / `gateway:{ch}:{sender}`)
    // for read-merge, so existing legacy history stays visible under the new key.
    let legacy_session_key = fabric_ctx.session_key.clone();
    let canonical_session_key = RuntimeEnvelope::gateway(
        workspace_id.clone(),
        legacy_session_key.clone(),
        fabric_ctx.channel.clone(),
        fabric_ctx.sender.clone(),
        fabric_ctx.recipient.clone(),
        MemoryVisibility::Session,
    )
    .canonical_session_key();
    let runtime_envelope = RuntimeEnvelope::gateway(
        workspace_id,
        canonical_session_key,
        fabric_ctx.channel.clone(),
        fabric_ctx.sender.clone(),
        fabric_ctx.recipient.clone(),
        // FIX-P1-04: webhook-driven gateway turns are session-scoped by default.
        // Persisting them as workspace-visible would leak per-session conversation
        // content to the whole workspace; Session keeps it bound to this session.
        MemoryVisibility::Session,
    )
    .with_run_id(run_id)
    .with_legacy_session_key(legacy_session_key);
    let base_scope = runtime_envelope.message_scope();
    authorize_gateway_resource_mutation(state, "gateway:webhook:message_event:user", ResourceRiskLevel::Low).map_err(
        |(_, body)| {
            let error = body
                .0
                .get("error")
                .and_then(|value| value.as_str())
                .unwrap_or("gateway resource mutation denied")
                .to_string();
            anyhow::anyhow!(error)
        },
    )?;
    if let Err(error) = fabric
        .record_inbound_user_message(
            base_scope.clone(),
            message.to_string(),
            fabric_ctx.idempotency_key.clone(),
            None,
        )
        .await
    {
        tracing::warn!(
            channel = %fabric_ctx.channel,
            session_key = %fabric_ctx.session_key,
            "Failed to append gateway user message event: {error}"
        );
    }

    let min_relevance_score = state.config.lock().memory.min_relevance_score;
    let semantic_scope = runtime_envelope.memory_write_context(if fabric_ctx.channel == "webhook" {
        "webhook"
    } else {
        "private"
    });
    let mem_context = build_context_with_shared_events_and_scope(
        state.mem.as_ref(),
        runtime_envelope.memory_principal(),
        message,
        min_relevance_score,
        Some(&semantic_scope),
    )
    .await;
    let enriched_message = if mem_context.preamble.is_empty() {
        message.to_string()
    } else {
        format!("{}{}", mem_context.preamble, message)
    };

    let user_messages = vec![ChatMessage::user(&enriched_message)];
    let image_marker_count = crate::multimodal::count_image_markers(&user_messages);
    if image_marker_count > 0 && !state.provider.supports_vision() {
        return Err(ProviderCapabilityError {
            provider: provider_label.to_string(),
            capability: "vision".to_string(),
            message: format!(
                "received {image_marker_count} image marker(s), but this provider does not support vision input"
            ),
        }
        .into());
    }

    // Build system prompt with native_tools flag so the prompt instructs the
    // LLM to use tools rather than emit XML tags.
    let (config_snapshot, multimodal_config, max_tool_iterations) = {
        let config_guard = state.config.lock();
        (
            config_guard.clone(),
            config_guard.multimodal.clone(),
            config_guard.agent.max_tool_iterations,
        )
    };
    let native_tools = state.provider.supports_native_tools();
    let skill_embedder =
        crate::memory::create_embedder_from_config(&config_snapshot, config_snapshot.api_key.as_deref());
    let skills = crate::skills::load_skills_with_embeddings(
        &config_snapshot.workspace_dir,
        &config_snapshot,
        skill_embedder.as_ref(),
    )
    .await?;
    let selected_skills = if config_snapshot.skill_rag.enabled {
        crate::skills::select_skills_by_relevance(
            message,
            &skills,
            config_snapshot.skill_rag.top_k,
            skill_embedder.as_ref(),
        )
        .await
    } else {
        skills.clone()
    };
    let system_prompt = {
        let tool_descs: Vec<(&str, &str)> = vec![
            ("shell", "Execute terminal commands"),
            ("file_read", "Read file contents"),
            ("file_write", "Write file contents"),
            ("memory_store", "Save to memory"),
            ("memory_recall", "Search memory"),
            ("memory_forget", "Delete a memory entry"),
        ];
        crate::channels::build_system_prompt_with_mode(
            &config_snapshot.workspace_dir,
            &state.model,
            &tool_descs,
            &selected_skills,
            Some(&config_snapshot.identity),
            None,
            native_tools,
        )
    };

    let mut history = Vec::with_capacity(2 + user_messages.len());
    history.push(ChatMessage::system(system_prompt));
    history.extend(user_messages);

    let noop_observer = NoopObserver;

    // P1-a: route the webhook agent loop through the unified `SecurityPolicy::decide`
    // gate instead of leaving `scope_ctx = None` (which short-circuits to `Allow`
    // and bypasses both the scope ACL and the autonomy level entirely).
    //
    // Webhook turns are Bearer-token authenticated (`handle_webhook` rejects
    // unauthenticated requests), so the request is a trusted principal and we can
    // construct a `ScopeContext` from the gateway identity. This makes every tool
    // call honour the configured autonomy level: under `supervised`, side-effecting
    // tools resolve to `Ask`, and with no `ApprovalManager` wired on this path the
    // tool loop fail-closes (deny) — only genuinely read-only tools run. Under
    // `full` autonomy the behaviour is unchanged (everything allowed).
    let gateway_policy =
        crate::security::SecurityPolicy::from_config(&config_snapshot.autonomy, &config_snapshot.workspace_dir);
    let gateway_scope_ctx = crate::agent::loop_::ScopeContext {
        policy: &gateway_policy,
        sender: fabric_ctx.sender.as_str(),
        channel: fabric_ctx.channel.as_str(),
        // Gateway/webhook turns are 1:1 (no group semantics); use a stable
        // chat_type so scope rules can target the gateway surface.
        chat_type: "gateway",
        chat_id: fabric_ctx.session_key.as_str(),
        owner_id: None,
        topic_id: None,
        task_id: None,
        source_message_event_id: None,
    };

    let provider_started_at = chrono::Utc::now();
    let route_decision = crate::llm::route_decision::RouteDecision::single_candidate_for_context(
        provider_label,
        state.model.clone(),
        runtime_envelope.resolved_owner_id(),
        runtime_envelope.session_key.clone(),
        runtime_envelope.source_message_event_id.clone(),
        None,
        "gateway_webhook",
        u32::try_from(message.chars().count() / 4).unwrap_or(u32::MAX),
        !state.tools_registry.is_empty(),
        false,
    );
    let loop_result = run_tool_call_loop_traced(
        state.provider.as_ref(),
        &mut history,
        Arc::clone(&state.tools_registry),
        &noop_observer,
        state.hooks.as_ref(),
        provider_label,
        &state.model,
        state.temperature,
        true, // silent
        None, // no approval manager
        "webhook",
        &multimodal_config,
        max_tool_iterations,
        config_snapshot.agent.parallel_tools,
        config_snapshot.agent.read_only_tool_concurrency_window,
        config_snapshot.agent.read_only_tool_timeout_secs,
        config_snapshot.agent.priority_scheduling_enabled,
        config_snapshot.agent.low_priority_tools.clone(),
        ToolConcurrencyGovernanceConfig {
            kill_switch_force_serial: config_snapshot.agent.concurrency_kill_switch_force_serial,
            rollout_stage: config_snapshot.agent.concurrency_rollout_stage.clone(),
            rollout_sample_percent: config_snapshot.agent.concurrency_rollout_sample_percent,
            rollout_channels: config_snapshot.agent.concurrency_rollout_channels.clone(),
            auto_rollback_enabled: config_snapshot.agent.concurrency_auto_rollback_enabled,
            rollback_timeout_rate_threshold: config_snapshot.agent.concurrency_rollback_timeout_rate_threshold,
            rollback_cancel_rate_threshold: config_snapshot.agent.concurrency_rollback_cancel_rate_threshold,
            rollback_error_rate_threshold: config_snapshot.agent.concurrency_rollback_error_rate_threshold,
        },
        None,
        None,                     // no cancellation token
        None,                     // no streaming delta sender
        Some(&gateway_scope_ctx), // P1-a: gateway turns now route through decide()
        None,                     // no tool call notifications
        Some(&config_snapshot.tool_tiering),
        Some(DocumentIngestRuntime::from_envelope(
            state.mem.clone(),
            &runtime_envelope,
        )),
        crate::agent::loop_::ChatMode::default(),
    )
    .await;
    let (response, trace) = match loop_result {
        Ok(result) => result,
        Err(error) => {
            let provider_outcome = crate::llm::route_decision::ProviderExecutionOutcome::failed_for_decision(
                &route_decision,
                provider_started_at,
                &error,
            );
            let terminal_id = runtime_envelope
                .run_id
                .clone()
                .unwrap_or_else(|| provider_outcome.decision_id.clone());
            if let Err(finalize_error) = crate::agent::terminal::finalize_turn(
                &fabric,
                crate::agent::terminal::TurnTerminalCommit {
                    terminal_id,
                    scope: runtime_envelope.message_scope(),
                    status: crate::agent::terminal::TurnTerminalStatus::Failed,
                    history: None,
                    history_scope: None,
                    provider_outcome: Some(provider_outcome),
                    telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                        summary: error.to_string(),
                        started_at: provider_started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
                },
                &config_snapshot.cost,
            )
            .await
            {
                tracing::warn!(error = %finalize_error, "Failed to commit failed gateway terminal event");
            }
            return Err(error);
        }
    };

    authorize_gateway_resource_mutation(state, "gateway:webhook:message_event:assistant", ResourceRiskLevel::Low)
        .map_err(|(_, body)| {
            let error = body
                .0
                .get("error")
                .and_then(|value| value.as_str())
                .unwrap_or("gateway resource mutation denied")
                .to_string();
            anyhow::anyhow!(error)
        })?;
    let provider_outcome =
        crate::agent::terminal::provider_outcome_from_trace(&route_decision, provider_started_at, trace);
    let terminal_id = runtime_envelope
        .run_id
        .clone()
        .unwrap_or_else(|| provider_outcome.decision_id.clone());
    if let Err(error) = crate::agent::terminal::finalize_turn(
        &fabric,
        crate::agent::terminal::TurnTerminalCommit {
            terminal_id,
            scope: runtime_envelope.message_scope(),
            status: crate::agent::terminal::TurnTerminalStatus::Completed,
            history: Some(crate::agent::terminal::TurnHistoryProjection {
                assistant_content: response.clone(),
                history_commit_len: history.len(),
            }),
            history_scope: Some(
                base_scope
                    .clone()
                    .with_sender(format!("{provider_label}/{}", state.model))
                    .with_recipient(fabric_ctx.sender.clone()),
            ),
            provider_outcome: Some(provider_outcome),
            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                summary: "gateway webhook completed".to_string(),
                started_at: provider_started_at,
                finished_at: chrono::Utc::now(),
            },
            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
        },
        &config_snapshot.cost,
    )
    .await
    {
        tracing::warn!(
            channel = %fabric_ctx.channel,
            session_key = %fabric_ctx.session_key,
            "Failed to commit shared gateway terminal event: {error}"
        );
        if let Err(fallback_error) = fabric
            .record_assistant_message(
                base_scope
                    .with_sender(format!("{provider_label}/{}", state.model))
                    .with_recipient(fabric_ctx.sender.clone()),
                response.clone(),
            )
            .await
        {
            tracing::warn!(error = %fallback_error, "Gateway assistant fallback projection also failed");
        }
    }
    Ok(response)
}

/// Webhook request body
#[derive(serde::Deserialize)]
pub struct WebhookBody {
    pub message: String,
    #[serde(default)]
    pub reply_target: Option<String>,
}

/// POST /webhook — main webhook endpoint
async fn handle_webhook(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let bearer_token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    let webhook_token_header = headers
        .get("X-Webhook-Token")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let webhook_signature_header = headers
        .get("X-Webhook-Signature")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let rate_key = client_key_from_request(Some(peer_addr), &headers, state.trust_forwarded_headers);
    if !state.rate_limiter.allow_webhook(&rate_key) {
        tracing::warn!("/webhook rate limit exceeded");
        let err = serde_json::json!({
            "error": "Too many webhook requests. Please retry later.",
            "retry_after": RATE_LIMIT_WINDOW_SECS,
        });
        return (StatusCode::TOO_MANY_REQUESTS, Json(err));
    }

    // ── Bearer token auth (pairing) ──
    if state.pairing.require_pairing() {
        if !state.pairing.is_authenticated(bearer_token) {
            tracing::warn!("Webhook: rejected — not paired / invalid bearer token");
            let err = serde_json::json!({
                "error": "Unauthorized — pair first via POST /pair, then send Authorization: Bearer <token>"
            });
            return (StatusCode::UNAUTHORIZED, Json(err));
        }
    }

    // ── Standalone webhook token auth (optional, additional layer) ──
    if let Some(ref token_hash) = state.webhook_token_hash {
        let header_hash = webhook_token_header.map(hash_webhook_secret);
        match header_hash {
            Some(val) if constant_time_eq(&val, token_hash.as_ref()) => {}
            _ => {
                tracing::warn!("Webhook: rejected request — invalid or missing X-Webhook-Token");
                let err = serde_json::json!({"error": "Unauthorized — invalid or missing X-Webhook-Token header"});
                return (StatusCode::UNAUTHORIZED, Json(err));
            }
        }
    }

    // ── Webhook HMAC auth (optional, additional layer) ──
    if let Some(ref signing_secret) = state.webhook_signing_secret {
        match webhook_signature_header {
            Some(signature) if verify_webhook_hmac_signature(signing_secret, &body, signature) => {}
            _ => {
                tracing::warn!("Webhook: rejected request — invalid or missing X-Webhook-Signature");
                let err = serde_json::json!({"error": "Unauthorized — invalid or missing X-Webhook-Signature header"});
                return (StatusCode::UNAUTHORIZED, Json(err));
            }
        }
    }

    // Additional credential-based limiter reduces distributed IP rotation bypass.
    let credential_rate_key = if !bearer_token.is_empty() {
        format!("bearer:{}", hash_webhook_secret(bearer_token))
    } else if let Some(token) = webhook_token_header {
        format!("token:{}", hash_webhook_secret(token))
    } else if let Some(signature) = webhook_signature_header {
        format!("signature:{}", hash_webhook_secret(signature))
    } else {
        "public".to_string()
    };
    let idempotency_scope = if state.pairing.require_pairing() {
        format!("bearer:{}", hash_webhook_secret(bearer_token))
    } else if let (Some(_), Some(token)) = (&state.webhook_token_hash, webhook_token_header) {
        format!("token:{}", hash_webhook_secret(token))
    } else if let Some(signing_secret) = state.webhook_signing_secret.as_deref() {
        format!("signing-secret:{}", hash_webhook_secret(signing_secret))
    } else {
        "public".to_string()
    };
    if !state.rate_limiter.allow_webhook_credential(&credential_rate_key) {
        tracing::warn!("/webhook credential rate limit exceeded");
        let err = serde_json::json!({
            "error": "Too many webhook requests. Please retry later.",
            "retry_after": RATE_LIMIT_WINDOW_SECS,
        });
        return (StatusCode::TOO_MANY_REQUESTS, Json(err));
    }

    // ── Parse body ──
    let webhook_body: WebhookBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("Webhook JSON parse error: {e}");
            let err = serde_json::json!({
                "error": "Invalid JSON body. Expected: {\"message\": \"...\"}"
            });
            return (StatusCode::BAD_REQUEST, Json(err));
        }
    };

    // ── Idempotency (optional) ──
    let request_idempotency_key = match headers.get("X-Idempotency-Key") {
        None => None,
        Some(value) => {
            let Ok(value) = value.to_str() else {
                let err = serde_json::json!({"error": "X-Idempotency-Key must be valid UTF-8"});
                return (StatusCode::BAD_REQUEST, Json(err));
            };
            let value = value.trim();
            if value.is_empty() {
                None
            } else if value.len() > IDEMPOTENCY_MAX_KEY_BYTES {
                let err = serde_json::json!({
                    "error": "X-Idempotency-Key exceeds the 256-byte limit"
                });
                return (StatusCode::BAD_REQUEST, Json(err));
            } else {
                Some(value.to_string())
            }
        }
    };
    let mut idempotency_digest = None;
    let mut idempotency_claim = None;
    if let Some(raw_key) = request_idempotency_key.as_deref() {
        if let Err(error) =
            authorize_gateway_resource_mutation(&state, "gateway:webhook:idempotency", ResourceRiskLevel::Low)
        {
            return error;
        }
        let digest = webhook_idempotency_digest(&idempotency_scope, raw_key);
        let fingerprint = webhook_request_fingerprint(&body);
        match state.idempotency_store.claim(digest.clone(), fingerprint) {
            IdempotencyClaimOutcome::Acquired(claim) => {
                idempotency_digest = Some(digest);
                idempotency_claim = Some(claim);
            }
            IdempotencyClaimOutcome::Processing => {
                let body = serde_json::json!({
                    "status": "processing",
                    "idempotent": true,
                    "message": "A request with this idempotency key is still processing"
                });
                return (StatusCode::CONFLICT, Json(body));
            }
            IdempotencyClaimOutcome::Replay(replay) => {
                return (StatusCode::OK, Json(replay.json_body()));
            }
            IdempotencyClaimOutcome::ReplayUnavailable {
                response_id,
                result_hash,
            } => {
                let body = serde_json::json!({
                    "status": "replay_unavailable",
                    "idempotent": true,
                    "response_id": response_id.to_string(),
                    "result_hash": result_hash,
                    "message": "The original request succeeded, but its response is too large to replay"
                });
                return (StatusCode::CONFLICT, Json(body));
            }
            IdempotencyClaimOutcome::RequestConflict => {
                let body = serde_json::json!({
                    "status": "request_conflict",
                    "idempotent": true,
                    "message": "This idempotency key was used with a different request body"
                });
                return (StatusCode::CONFLICT, Json(body));
            }
            IdempotencyClaimOutcome::RetryUnavailable => {
                let body = serde_json::json!({
                    "status": "retry_unavailable",
                    "idempotent": true,
                    "message": "This failed request is not eligible for retry"
                });
                return (StatusCode::CONFLICT, Json(body));
            }
            IdempotencyClaimOutcome::AtCapacity => {
                let body = serde_json::json!({
                    "error": "Idempotency capacity is temporarily exhausted",
                    "retryable": true
                });
                return (StatusCode::SERVICE_UNAVAILABLE, Json(body));
            }
        }
    }

    let message = &webhook_body.message;

    if state.auto_save && should_autosave_gateway_message(webhook_body.reply_target.as_deref(), message) {
        if let Err(error) =
            authorize_gateway_resource_mutation(&state, "gateway:webhook:autosave", ResourceRiskLevel::Low)
        {
            return error;
        }
        let key = webhook_memory_key(idempotency_digest.as_deref());
        let _ = state.mem.store(&key, message, MemoryCategory::Conversation, None).await;
    }

    let provider_label = state
        .config
        .lock()
        .default_provider
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let model_label = state.model.clone();
    let started_at = Instant::now();

    state
        .observer
        .record_event(&crate::observability::ObserverEvent::AgentStart {
            provider: provider_label.clone(),
            model: model_label.clone(),
        });
    state
        .observer
        .record_event(&crate::observability::ObserverEvent::LlmRequest {
            provider: provider_label.clone(),
            model: model_label.clone(),
            messages_count: 1,
        });

    let fabric_ctx = GatewayFabricContext::generic_webhook(
        webhook_body.reply_target.as_deref(),
        idempotency_digest
            .as_deref()
            .map(|digest| format!("gateway:webhook:{digest}")),
    );
    match run_gateway_chat_with_multimodal(&state, &provider_label, message, &fabric_ctx).await {
        Ok(response) => {
            let response_id = Uuid::new_v4();
            let result_hash = webhook_result_hash(&response, &model_label);
            if let Some(claim) = idempotency_claim.take() {
                let replay = IdempotencyReplay {
                    response_id,
                    response: Arc::from(response.as_str()),
                    model: Arc::from(model_label.as_str()),
                };
                if !claim.succeed(replay, result_hash) {
                    tracing::error!("Webhook idempotency ownership was lost before success commit");
                    let err = serde_json::json!({
                        "error": "Request completed but its idempotency result could not be committed"
                    });
                    return (StatusCode::SERVICE_UNAVAILABLE, Json(err));
                }
            }
            let duration = started_at.elapsed();
            state
                .observer
                .record_event(&crate::observability::ObserverEvent::LlmResponse {
                    provider: provider_label.clone(),
                    model: model_label.clone(),
                    duration,
                    success: true,
                    error_message: None,
                });
            state
                .observer
                .record_metric(&crate::observability::traits::ObserverMetric::RequestLatency(duration));
            state
                .observer
                .record_event(&crate::observability::ObserverEvent::AgentEnd {
                    provider: provider_label,
                    model: model_label,
                    duration,
                    tokens_used: None,
                    cost_usd: None,
                });

            let body = serde_json::json!({
                "response": response,
                "model": state.model,
                "response_id": response_id.to_string(),
            });
            (StatusCode::OK, Json(body))
        }
        Err(e) => {
            if let Some(claim) = idempotency_claim.take() {
                let _ = claim.fail(true);
            }
            let duration = started_at.elapsed();
            let sanitized = providers::sanitize_api_error(&e.to_string());

            state
                .observer
                .record_event(&crate::observability::ObserverEvent::LlmResponse {
                    provider: provider_label.clone(),
                    model: model_label.clone(),
                    duration,
                    success: false,
                    error_message: Some(sanitized.clone()),
                });
            state
                .observer
                .record_metric(&crate::observability::traits::ObserverMetric::RequestLatency(duration));
            state
                .observer
                .record_event(&crate::observability::ObserverEvent::Error {
                    component: "gateway".to_string(),
                    message: sanitized.clone(),
                });
            state
                .observer
                .record_event(&crate::observability::ObserverEvent::AgentEnd {
                    provider: provider_label,
                    model: model_label,
                    duration,
                    tokens_used: None,
                    cost_usd: None,
                });

            tracing::error!("Webhook provider error: {}", sanitized);
            let err = serde_json::json!({"error": "LLM request failed"});
            (StatusCode::INTERNAL_SERVER_ERROR, Json(err))
        }
    }
}

/// `WhatsApp` verification query params
#[derive(serde::Deserialize)]
pub struct WhatsAppVerifyQuery {
    #[serde(rename = "hub.mode")]
    pub mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    pub verify_token: Option<String>,
    #[serde(rename = "hub.challenge")]
    pub challenge: Option<String>,
}

/// GET /whatsapp — Meta webhook verification
async fn handle_whatsapp_verify(
    State(state): State<AppState>,
    Query(params): Query<WhatsAppVerifyQuery>,
) -> impl IntoResponse {
    let Some(ref wa) = state.whatsapp else {
        return (StatusCode::NOT_FOUND, "WhatsApp not configured".to_string());
    };

    // Verify the token matches (constant-time comparison to prevent timing attacks)
    let token_matches = params
        .verify_token
        .as_deref()
        .is_some_and(|t| constant_time_eq(t, wa.verify_token()));
    if params.mode.as_deref() == Some("subscribe") && token_matches {
        if let Some(ch) = params.challenge {
            tracing::info!("WhatsApp webhook verified successfully");
            return (StatusCode::OK, ch);
        }
        return (StatusCode::BAD_REQUEST, "Missing hub.challenge".to_string());
    }

    tracing::warn!("WhatsApp webhook verification failed — token mismatch");
    (StatusCode::FORBIDDEN, "Forbidden".to_string())
}

/// Verify `WhatsApp` webhook signature (`X-Hub-Signature-256`).
///
/// Returns true if the signature is valid, false otherwise.
/// See: <https://developers.facebook.com/docs/graph-api/webhooks/getting-started#verification-requests>
pub fn verify_whatsapp_signature(app_secret: &str, body: &[u8], signature_header: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    // Signature format: "sha256=<hex_signature>"
    let Some(hex_sig) = signature_header.strip_prefix("sha256=") else {
        return false;
    };

    // Decode hex signature
    let Ok(expected) = hex::decode(hex_sig) else {
        return false;
    };

    // Compute HMAC-SHA256
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(app_secret.as_bytes()) else {
        return false;
    };
    mac.update(body);

    // Constant-time comparison
    mac.verify_slice(&expected).is_ok()
}

/// POST /whatsapp — incoming message webhook
async fn handle_whatsapp_message(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    let Some(ref wa) = state.whatsapp else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "WhatsApp not configured"})),
        );
    };

    // ── Security: Verify X-Hub-Signature-256 — reject if signing secret not configured ──
    let app_secret = match &state.whatsapp_app_secret {
        Some(secret) => secret,
        None => {
            tracing::error!("WhatsApp webhook received but signing secret not configured — rejecting");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Webhook signing secret not configured"})),
            );
        }
    };
    {
        let signature = headers
            .get("X-Hub-Signature-256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if !verify_whatsapp_signature(app_secret, &body, signature) {
            tracing::warn!(
                "WhatsApp webhook signature verification failed (signature: {})",
                if signature.is_empty() { "missing" } else { "invalid" }
            );
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Invalid signature"})),
            );
        }
    }

    // Parse JSON body
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid JSON payload"})),
        );
    };

    // Parse messages from the webhook payload
    let messages = wa.parse_webhook_payload(&payload);

    if messages.is_empty() {
        // Acknowledge the webhook even if no messages (could be status updates)
        return (StatusCode::OK, Json(serde_json::json!({"status": "ok"})));
    }

    // Process each message
    let provider_label = state
        .config
        .lock()
        .default_provider
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    for msg in &messages {
        tracing::info!(
            "WhatsApp message from {}: {}",
            msg.sender,
            truncate_with_ellipsis(&msg.content, 50)
        );

        // Auto-save to memory
        if state.auto_save && should_autosave_gateway_message(Some(&msg.reply_target), &msg.content) {
            if let Err(error) = authorize_gateway_resource_mutation(
                &state,
                &gateway_channel_webhook_operation("whatsapp", "autosave"),
                ResourceRiskLevel::Low,
            ) {
                return error;
            }
            let key = whatsapp_memory_key(msg);
            let _ = state
                .mem
                .store(&key, &msg.content, MemoryCategory::Conversation, None)
                .await;
        }

        let fabric_ctx = GatewayFabricContext::channel_message(msg);
        match run_gateway_chat_with_multimodal(&state, &provider_label, &msg.content, &fabric_ctx).await {
            Ok(response) => {
                // Send reply via WhatsApp
                if let Err(error) = authorize_gateway_resource_mutation(
                    &state,
                    &gateway_channel_webhook_operation("whatsapp", "send"),
                    ResourceRiskLevel::Low,
                ) {
                    return error;
                }
                if let Err(e) = wa.send(&SendMessage::new(response, &msg.reply_target)).await {
                    tracing::error!("Failed to send WhatsApp reply: {e}");
                }
            }
            Err(e) => {
                tracing::error!("LLM error for WhatsApp message: {e:#}");
                if let Err(error) = authorize_gateway_resource_mutation(
                    &state,
                    &gateway_channel_webhook_operation("whatsapp", "send"),
                    ResourceRiskLevel::Low,
                ) {
                    return error;
                }
                let _ = wa
                    .send(&SendMessage::new(
                        "Sorry, I couldn't process your message right now.",
                        &msg.reply_target,
                    ))
                    .await;
            }
        }
    }

    // Acknowledge the webhook
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
}

/// POST /linq — incoming message webhook (iMessage/RCS/SMS via Linq)
async fn handle_linq_webhook(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    let Some(ref linq) = state.linq else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Linq not configured"})),
        );
    };

    let body_str = String::from_utf8_lossy(&body);

    // ── Security: Verify X-Webhook-Signature — reject if signing secret not configured ──
    let signing_secret = match &state.linq_signing_secret {
        Some(secret) => secret,
        None => {
            tracing::error!("Linq webhook received but signing secret not configured — rejecting");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Webhook signing secret not configured"})),
            );
        }
    };
    {
        let timestamp = headers
            .get("X-Webhook-Timestamp")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let signature = headers
            .get("X-Webhook-Signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if !crate::channels::linq::verify_linq_signature(signing_secret, &body_str, timestamp, signature) {
            tracing::warn!(
                "Linq webhook signature verification failed (signature: {})",
                if signature.is_empty() { "missing" } else { "invalid" }
            );
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Invalid signature"})),
            );
        }
    }

    // Parse JSON body
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid JSON payload"})),
        );
    };

    // Parse messages from the webhook payload
    let messages = linq.parse_webhook_payload(&payload);

    if messages.is_empty() {
        // Acknowledge the webhook even if no messages (could be status/delivery events)
        return (StatusCode::OK, Json(serde_json::json!({"status": "ok"})));
    }

    // Process each message
    let provider_label = state
        .config
        .lock()
        .default_provider
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    for msg in &messages {
        tracing::info!(
            "Linq message from {}: {}",
            msg.sender,
            truncate_with_ellipsis(&msg.content, 50)
        );

        // Auto-save to memory
        if state.auto_save && should_autosave_gateway_message(Some(&msg.reply_target), &msg.content) {
            if let Err(error) = authorize_gateway_resource_mutation(
                &state,
                &gateway_channel_webhook_operation("linq", "autosave"),
                ResourceRiskLevel::Low,
            ) {
                return error;
            }
            let key = linq_memory_key(msg);
            let _ = state
                .mem
                .store(&key, &msg.content, MemoryCategory::Conversation, None)
                .await;
        }

        // Call the LLM
        let fabric_ctx = GatewayFabricContext::channel_message(msg);
        match run_gateway_chat_with_multimodal(&state, &provider_label, &msg.content, &fabric_ctx).await {
            Ok(response) => {
                // Send reply via Linq
                if let Err(error) = authorize_gateway_resource_mutation(
                    &state,
                    &gateway_channel_webhook_operation("linq", "send"),
                    ResourceRiskLevel::Low,
                ) {
                    return error;
                }
                if let Err(e) = linq.send(&SendMessage::new(response, &msg.reply_target)).await {
                    tracing::error!("Failed to send Linq reply: {e}");
                }
            }
            Err(e) => {
                tracing::error!("LLM error for Linq message: {e:#}");
                if let Err(error) = authorize_gateway_resource_mutation(
                    &state,
                    &gateway_channel_webhook_operation("linq", "send"),
                    ResourceRiskLevel::Low,
                ) {
                    return error;
                }
                let _ = linq
                    .send(&SendMessage::new(
                        "Sorry, I couldn't process your message right now.",
                        &msg.reply_target,
                    ))
                    .await;
            }
        }
    }

    // Acknowledge the webhook
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
}

/// POST /nextcloud-talk — incoming message webhook (Nextcloud Talk bot API)
async fn handle_nextcloud_talk_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let Some(ref nextcloud_talk) = state.nextcloud_talk else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Nextcloud Talk not configured"})),
        );
    };

    let body_str = String::from_utf8_lossy(&body);

    // ── Security: Verify Nextcloud Talk HMAC signature — reject if secret not configured ──
    let webhook_secret = match &state.nextcloud_talk_webhook_secret {
        Some(secret) => secret,
        None => {
            tracing::error!("Nextcloud Talk webhook received but signing secret not configured — rejecting");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Webhook signing secret not configured"})),
            );
        }
    };
    {
        let random = headers
            .get("X-Nextcloud-Talk-Random")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let signature = headers
            .get("X-Nextcloud-Talk-Signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if !crate::channels::nextcloud_talk::verify_nextcloud_talk_signature(
            webhook_secret,
            random,
            &body_str,
            signature,
        ) {
            tracing::warn!(
                "Nextcloud Talk webhook signature verification failed (signature: {})",
                if signature.is_empty() { "missing" } else { "invalid" }
            );
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Invalid signature"})),
            );
        }
    }

    // Parse JSON body
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid JSON payload"})),
        );
    };

    // Parse messages from webhook payload
    let messages = nextcloud_talk.parse_webhook_payload(&payload);
    if messages.is_empty() {
        // Acknowledge webhook even if payload does not contain actionable user messages.
        return (StatusCode::OK, Json(serde_json::json!({"status": "ok"})));
    }

    let provider_label = state
        .config
        .lock()
        .default_provider
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    for msg in &messages {
        tracing::info!(
            "Nextcloud Talk message from {}: {}",
            msg.sender,
            truncate_with_ellipsis(&msg.content, 50)
        );

        if state.auto_save && should_autosave_gateway_message(Some(&msg.reply_target), &msg.content) {
            if let Err(error) = authorize_gateway_resource_mutation(
                &state,
                &gateway_channel_webhook_operation("nextcloud_talk", "autosave"),
                ResourceRiskLevel::Low,
            ) {
                return error;
            }
            let key = nextcloud_talk_memory_key(msg);
            let _ = state
                .mem
                .store(&key, &msg.content, MemoryCategory::Conversation, None)
                .await;
        }

        let fabric_ctx = GatewayFabricContext::channel_message(msg);
        match run_gateway_chat_with_multimodal(&state, &provider_label, &msg.content, &fabric_ctx).await {
            Ok(response) => {
                if let Err(error) = authorize_gateway_resource_mutation(
                    &state,
                    &gateway_channel_webhook_operation("nextcloud_talk", "send"),
                    ResourceRiskLevel::Low,
                ) {
                    return error;
                }
                if let Err(e) = nextcloud_talk
                    .send(&SendMessage::new(response, &msg.reply_target))
                    .await
                {
                    tracing::error!("Failed to send Nextcloud Talk reply: {e}");
                }
            }
            Err(e) => {
                tracing::error!("LLM error for Nextcloud Talk message: {e:#}");
                if let Err(error) = authorize_gateway_resource_mutation(
                    &state,
                    &gateway_channel_webhook_operation("nextcloud_talk", "send"),
                    ResourceRiskLevel::Low,
                ) {
                    return error;
                }
                let _ = nextcloud_talk
                    .send(&SendMessage::new(
                        "Sorry, I couldn't process your message right now.",
                        &msg.reply_target,
                    ))
                    .await;
            }
        }
    }

    (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::needless_collect,
        clippy::unreadable_literal
    )]
    use super::*;
    use crate::channels::traits::ChannelMessage;
    use crate::memory::{Memory, MemoryCategory, MemoryEntry, MemoryPrincipal, SqliteMemory};
    use crate::providers::Provider;
    use async_trait::async_trait;
    use axum::http::HeaderValue;
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    /// Generate a random hex secret at runtime to avoid hard-coded cryptographic values.
    fn generate_test_secret() -> String {
        let bytes: [u8; 32] = rand::random();
        hex::encode(bytes)
    }

    #[test]
    fn security_body_limit_is_64kb() {
        assert_eq!(MAX_BODY_SIZE, 65_536);
    }

    #[test]
    fn security_api_body_limit_supports_media_batch_uploads() {
        assert!(MAX_API_BODY_SIZE >= (10 * 20 * 1024 * 1024));
    }

    #[test]
    fn security_timeout_uses_gateway_config_default() {
        assert_eq!(crate::config::GatewayConfig::default().request_timeout_secs, 60);
    }

    #[test]
    fn security_timeout_config_allows_override() {
        let cfg = crate::config::GatewayConfig {
            request_timeout_secs: 12,
            ..crate::config::GatewayConfig::default()
        };
        assert_eq!(cfg.request_timeout_secs, 12);
    }

    #[test]
    fn webhook_body_requires_message_field() {
        let valid = r#"{"message": "hello"}"#;
        let parsed: Result<WebhookBody, _> = serde_json::from_str(valid);
        assert!(parsed.is_ok());
        assert_eq!(parsed.unwrap().message, "hello");

        let missing = r#"{"other": "field"}"#;
        let parsed: Result<WebhookBody, _> = serde_json::from_str(missing);
        assert!(parsed.is_err());
    }

    #[test]
    fn whatsapp_query_fields_are_optional() {
        let q = WhatsAppVerifyQuery {
            mode: None,
            verify_token: None,
            challenge: None,
        };
        assert!(q.mode.is_none());
    }

    #[test]
    fn app_state_is_clone() {
        fn assert_clone<T: Clone>() {}
        assert_clone::<AppState>();
    }

    /// Build a minimal `AppState` whose cached C and hot D both start from `config`,
    /// for exercising the gateway authorization helpers in isolation.
    fn authz_test_state(config: Config) -> AppState {
        AppState {
            config: Arc::new(Mutex::new(config.clone())),
            shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(config)),
            provider: Arc::new(MockProvider::default()),
            model: "test-model".into(),
            temperature: 0.0,
            mem: Arc::new(MockMemory),
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: None,
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(crate::observability::NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_manager: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_middleware: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_hook_executor: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_cron_manager: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        }
    }

    /// D2 / T1 (core mutation point): `authorize_gateway_resource_mutation` reads the
    /// hot SharedConfig (D). Publishing a ReadOnly config to D ONLY (C left stale)
    /// flips a previously-allowed core gateway mutation to denied — no restart, and
    /// proving the decision no longer depends on C.
    #[test]
    fn gateway_core_authz_reads_hot_shared_config_after_reload() {
        use crate::security::policy::ResourceRiskLevel;

        let state = authz_test_state(Config::default());
        assert!(
            authorize_gateway_resource_mutation(&state, "gateway:pair", ResourceRiskLevel::Low).is_ok(),
            "default autonomous policy should allow a low-risk core gateway mutation"
        );

        let read_only = Config {
            autonomy: crate::config::AutonomyConfig {
                level: crate::security::policy::AutonomyLevel::ReadOnly,
                ..crate::config::AutonomyConfig::default()
            },
            ..Config::default()
        };
        // Publish to D only; C stays at the permissive default on purpose.
        state.shared_config.store(Arc::new(read_only));

        let denied = authorize_gateway_resource_mutation(&state, "gateway:pair", ResourceRiskLevel::Low)
            .expect_err("ReadOnly published to D must deny the core gateway mutation");
        assert_eq!(denied.0, StatusCode::FORBIDDEN);
        // C is unchanged — the deny came purely from reading D.
        assert_eq!(
            state.config.lock().autonomy.level,
            crate::security::policy::AutonomyLevel::default()
        );
    }

    #[tokio::test]
    async fn metrics_endpoint_returns_hint_when_prometheus_is_disabled() {
        let state = AppState {
            config: Arc::new(Mutex::new(Config::default())),
            shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(Config::default())),
            provider: Arc::new(MockProvider::default()),
            model: "test-model".into(),
            temperature: 0.0,
            mem: Arc::new(MockMemory),
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: None,
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(crate::observability::NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_manager: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_middleware: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_hook_executor: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_cron_manager: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        };

        let response = handle_metrics(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(PROMETHEUS_CONTENT_TYPE)
        );

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("Prometheus backend not enabled"));
    }

    #[tokio::test]
    async fn metrics_endpoint_renders_prometheus_output() {
        let prom = Arc::new(crate::observability::PrometheusObserver::try_new().unwrap());
        crate::observability::Observer::record_event(
            prom.as_ref(),
            &crate::observability::ObserverEvent::HeartbeatTick,
        );

        let observer: Arc<dyn crate::observability::Observer> = prom;
        let state = AppState {
            config: Arc::new(Mutex::new(Config::default())),
            shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(Config::default())),
            provider: Arc::new(MockProvider::default()),
            model: "test-model".into(),
            temperature: 0.0,
            mem: Arc::new(MockMemory),
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: None,
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer,
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_manager: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_middleware: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_hook_executor: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_cron_manager: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        };

        let response = handle_metrics(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("prx_heartbeat_ticks_total 1"));
    }

    #[test]
    fn gateway_rate_limiter_blocks_after_limit() {
        let limiter = GatewayRateLimiter::new(2, 2, 2, 100);
        assert!(limiter.allow_pair("127.0.0.1"));
        assert!(limiter.allow_pair("127.0.0.1"));
        assert!(!limiter.allow_pair("127.0.0.1"));
    }

    #[test]
    fn gateway_webhook_credential_limiter_blocks_after_limit() {
        let limiter = GatewayRateLimiter::new(2, 2, 2, 100);
        assert!(limiter.allow_webhook_credential("bearer:token-a"));
        assert!(limiter.allow_webhook_credential("bearer:token-a"));
        assert!(!limiter.allow_webhook_credential("bearer:token-a"));
    }

    #[test]
    fn gateway_api_limiter_blocks_after_limit() {
        let limiter = GatewayRateLimiter::new(2, 2, 2, 100);
        assert!(limiter.allow_api("token:abc"));
        assert!(limiter.allow_api("token:abc"));
        assert!(!limiter.allow_api("token:abc"));
    }

    #[test]
    fn rate_limiter_sweep_removes_stale_entries() {
        let limiter = SlidingWindowRateLimiter::new(10, Duration::from_secs(60), 100);
        // Add entries for multiple IPs
        assert!(limiter.allow("ip-1"));
        assert!(limiter.allow("ip-2"));
        assert!(limiter.allow("ip-3"));

        {
            let guard = limiter.requests.lock();
            assert_eq!(guard.0.len(), 3);
        }

        // Force a sweep by backdating last_sweep
        {
            let mut guard = limiter.requests.lock();
            guard.1 = Instant::now()
                .checked_sub(Duration::from_secs(RATE_LIMITER_SWEEP_INTERVAL_SECS + 1))
                .unwrap();
            // Clear timestamps for ip-2 and ip-3 to simulate stale entries
            guard.0.get_mut("ip-2").unwrap().clear();
            guard.0.get_mut("ip-3").unwrap().clear();
        }

        // Next allow() call should trigger sweep and remove stale entries
        assert!(limiter.allow("ip-1"));

        {
            let guard = limiter.requests.lock();
            assert_eq!(guard.0.len(), 1, "Stale entries should have been swept");
            assert!(guard.0.contains_key("ip-1"));
        }
    }

    #[test]
    fn rate_limiter_zero_limit_always_allows() {
        let limiter = SlidingWindowRateLimiter::new(0, Duration::from_secs(60), 10);
        for _ in 0..100 {
            assert!(limiter.allow("any-key"));
        }
    }

    fn test_fingerprint(value: u8) -> [u8; 32] {
        [value; 32]
    }

    fn acquire_idempotency_claim(store: &Arc<IdempotencyStore>, key: &str, fingerprint: [u8; 32]) -> IdempotencyClaim {
        match store.claim(key.to_string(), fingerprint) {
            IdempotencyClaimOutcome::Acquired(claim) => claim,
            outcome => panic!("expected acquired idempotency claim, got {outcome:?}"),
        }
    }

    fn test_idempotency_replay(response: &str) -> IdempotencyReplay {
        IdempotencyReplay {
            response_id: Uuid::new_v4(),
            response: Arc::from(response),
            model: Arc::from("test-model"),
        }
    }

    #[test]
    fn idempotency_store_reports_processing_then_replays_success() {
        let store = Arc::new(IdempotencyStore::new(Duration::from_secs(30), 10));
        let claim = acquire_idempotency_claim(&store, "req-1", test_fingerprint(1));
        assert!(matches!(
            store.claim("req-1".to_string(), test_fingerprint(1)),
            IdempotencyClaimOutcome::Processing
        ));

        let replay = test_idempotency_replay("ok");
        let response_id = replay.response_id;
        assert!(claim.succeed(replay, "result-hash".to_string()));
        assert!(matches!(
            store.claim("req-1".to_string(), test_fingerprint(1)),
            IdempotencyClaimOutcome::Replay(snapshot) if snapshot.response_id == response_id
        ));
    }

    #[test]
    fn rate_limiter_bounded_cardinality_evicts_least_active_key() {
        let limiter = SlidingWindowRateLimiter::new(5, Duration::from_secs(60), 2);
        // ip-1 gets 2 requests, ip-2 gets 1 — ip-2 is least active
        assert!(limiter.allow("ip-1"));
        assert!(limiter.allow("ip-1"));
        assert!(limiter.allow("ip-2"));
        // ip-3 triggers eviction — ip-2 (1 request) is evicted over ip-1 (2 requests)
        assert!(limiter.allow("ip-3"));

        let guard = limiter.requests.lock();
        assert_eq!(guard.0.len(), 2);
        assert!(guard.0.contains_key("ip-1"), "ip-1 (most active) must survive");
        assert!(guard.0.contains_key("ip-3"), "ip-3 (just inserted) must be present");
    }

    #[test]
    fn idempotency_store_never_evicts_live_or_unexpired_entries() {
        let store = Arc::new(IdempotencyStore::new(Duration::from_secs(300), 2));
        let first = acquire_idempotency_claim(&store, "k1", test_fingerprint(1));
        let second = acquire_idempotency_claim(&store, "k2", test_fingerprint(2));

        assert!(matches!(
            store.claim("k3".to_string(), test_fingerprint(3)),
            IdempotencyClaimOutcome::AtCapacity
        ));
        assert!(first.succeed(test_idempotency_replay("first"), "first-hash".to_string()));
        assert!(second.succeed(test_idempotency_replay("second"), "second-hash".to_string()));
        assert!(matches!(
            store.claim("k3".to_string(), test_fingerprint(3)),
            IdempotencyClaimOutcome::AtCapacity
        ));
    }

    #[test]
    fn client_key_defaults_to_peer_addr_when_untrusted_proxy_mode() {
        let peer = SocketAddr::from(([10, 0, 0, 5], 3000));
        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Forwarded-For",
            HeaderValue::from_static("198.51.100.10, 203.0.113.11"),
        );

        let key = client_key_from_request(Some(peer), &headers, false);
        assert_eq!(key, "10.0.0.5");
    }

    #[test]
    fn client_key_uses_forwarded_ip_only_in_trusted_proxy_mode() {
        let peer = SocketAddr::from(([10, 0, 0, 5], 3000));
        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Forwarded-For",
            HeaderValue::from_static("198.51.100.10, 203.0.113.11"),
        );

        let key = client_key_from_request(Some(peer), &headers, true);
        // RFC 7239: use the LAST (rightmost) IP — it's the one added by the
        // closest trusted proxy and cannot be spoofed by the client.
        assert_eq!(key, "203.0.113.11");
    }

    #[test]
    fn client_key_falls_back_to_peer_when_forwarded_header_invalid() {
        let peer = SocketAddr::from(([10, 0, 0, 5], 3000));
        let mut headers = HeaderMap::new();
        headers.insert("X-Forwarded-For", HeaderValue::from_static("garbage-value"));

        let key = client_key_from_request(Some(peer), &headers, true);
        assert_eq!(key, "10.0.0.5");
    }

    #[test]
    fn normalize_max_keys_uses_fallback_for_zero() {
        assert_eq!(normalize_max_keys(0, 10_000), 10_000);
        assert_eq!(normalize_max_keys(0, 0), 1);
    }

    #[test]
    fn normalize_max_keys_preserves_nonzero_values() {
        assert_eq!(normalize_max_keys(2_048, 10_000), 2_048);
        assert_eq!(normalize_max_keys(1, 10_000), 1);
    }

    /// D2 / 修3 regression: `persist_pairing_tokens` must base the persisted/published
    /// config on the HOT SharedConfig (D) snapshot, NOT the cached C Mutex. On the
    /// reload-only paths C lags D; if persist cloned a stale C, saved it, and re-stored
    /// it, every field a prior hot-reload changed would be silently reverted on disk AND
    /// in D — only the pairing token would survive.
    ///
    /// Hardening (Codex review): we seed C with a STALE value and D with a DIFFERENT HOT
    /// value of an observable, round-tripping field (`default_temperature`: C=0.0 stale,
    /// D=0.42 hot — simulating a reload C has not yet observed), then assert the hot value
    /// SURVIVES persistence. If the base were C, the saved file (and D) would revert to
    /// 0.0; this assertion fails in that case, so the test genuinely guards the merge base.
    #[tokio::test]
    async fn persist_pairing_tokens_uses_hot_config_base_and_writes_tokens() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        let workspace_path = temp.path().join("workspace");

        // C (cached Mutex): STALE base with the old, pre-reload temperature.
        let mut cached_config = Config::default();
        cached_config.config_path = config_path.clone();
        cached_config.workspace_dir = workspace_path.clone();
        cached_config.default_temperature = 0.0;
        // Persist this stale config to disk first, mirroring a real running daemon whose
        // on-disk file predates the hot reload.
        cached_config.save().await.unwrap();

        // D (hot SharedConfig): the HOT base with a DIFFERENT temperature, as if a prior
        // `config_reload` published 0.42 into D while C still holds the old 0.0.
        let mut hot_config = Config::default();
        hot_config.config_path = config_path.clone();
        hot_config.workspace_dir = workspace_path;
        hot_config.default_temperature = 0.42;

        let guard = PairingGuard::new(true, &[]);
        let code = guard.pairing_code().unwrap();
        let token = guard.try_pair(&code, "test_client").await.unwrap().unwrap();
        assert!(guard.is_authenticated(&token));

        let cached = Arc::new(Mutex::new(cached_config));
        let shared: crate::config::SharedConfig = Arc::new(arc_swap::ArcSwap::from_pointee(hot_config));
        persist_pairing_tokens(cached.clone(), &shared, &guard).await.unwrap();

        let saved = tokio::fs::read_to_string(config_path).await.unwrap();
        let parsed: Config = toml::from_str(&saved).unwrap();

        // (a) The HOT field from D survived: the persisted file keeps 0.42, NOT the stale
        // 0.0 from C. A regression that used C as the base would write 0.0 here.
        assert!(
            (parsed.default_temperature - 0.42).abs() < 1e-9,
            "persist base must be D: hot default_temperature must survive on disk, got {}",
            parsed.default_temperature
        );

        // (b) The pairing token was written.
        assert_eq!(parsed.gateway.paired_tokens.len(), 1);
        let persisted = &parsed.gateway.paired_tokens[0];
        assert_eq!(persisted.len(), 64);
        assert!(persisted.chars().all(|c| c.is_ascii_hexdigit()));

        // (c) C == D after persist (sync invariant restored), and both carry the HOT
        // temperature and the new token — not the stale C value.
        let in_memory = cached.lock();
        assert!(
            (in_memory.default_temperature - 0.42).abs() < 1e-9,
            "C must be re-synced to the hot value 0.42, got {}",
            in_memory.default_temperature
        );
        assert_eq!(in_memory.gateway.paired_tokens.len(), 1);
        assert_eq!(&in_memory.gateway.paired_tokens[0], persisted);
        drop(in_memory);

        let hot = shared.load_full();
        assert!(
            (hot.default_temperature - 0.42).abs() < 1e-9,
            "D must retain the hot value 0.42 after persist, got {}",
            hot.default_temperature
        );
        assert_eq!(hot.gateway.paired_tokens.len(), 1);
        assert_eq!(&hot.gateway.paired_tokens[0], persisted);
    }

    #[test]
    fn webhook_memory_key_is_unique() {
        let key1 = webhook_memory_key(None);
        let key2 = webhook_memory_key(None);

        assert!(key1.starts_with("webhook_msg_"));
        assert!(key2.starts_with("webhook_msg_"));
        assert_ne!(key1, key2);
    }

    #[test]
    fn webhook_memory_key_is_stable_for_idempotent_retry() {
        let key1 = webhook_memory_key(Some("digest"));
        let key2 = webhook_memory_key(Some("digest"));

        assert_eq!(key1, key2);
        assert_eq!(key1, "webhook_msg_digest");
    }

    #[test]
    fn webhook_idempotency_digest_is_scoped_and_hides_raw_key() {
        let first_scope = webhook_idempotency_digest("scope-a", "sensitive-external-key");
        let second_scope = webhook_idempotency_digest("scope-b", "sensitive-external-key");

        assert_eq!(first_scope.len(), 64);
        assert!(first_scope.chars().all(|character| character.is_ascii_hexdigit()));
        assert!(!first_scope.contains("sensitive-external-key"));
        assert_ne!(first_scope, second_scope);
    }

    #[test]
    fn whatsapp_memory_key_includes_sender_and_message_id() {
        let msg = ChannelMessage {
            id: "wamid-123".into(),
            sender: "+1234567890".into(),
            reply_target: "+1234567890".into(),
            content: "hello".into(),
            channel: "whatsapp".into(),
            timestamp: 1,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        };

        let key = whatsapp_memory_key(&msg);
        assert_eq!(key, "whatsapp_+1234567890_wamid-123");
    }

    #[derive(Default)]
    struct MockMemory;

    #[async_trait]
    impl Memory for MockMemory {
        fn name(&self) -> &str {
            "mock"
        }

        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct MockProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("ok".into())
        }
    }

    #[derive(Default)]
    struct FailFirstProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for FailFirstProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                anyhow::bail!("injected provider failure");
            }
            Ok("retry-ok".into())
        }
    }

    #[derive(Default)]
    struct BlockFirstProvider {
        calls: AtomicUsize,
        first_started: Notify,
        release_first: Notify,
    }

    #[async_trait]
    impl Provider for BlockFirstProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.first_started.notify_one();
                self.release_first.notified().await;
            }
            Ok("released".into())
        }
    }

    #[derive(Default)]
    struct OversizeResponseProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for OversizeResponseProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("x".repeat(IDEMPOTENCY_MAX_REPLAY_BYTES + 1))
        }
    }

    #[derive(Default)]
    struct TrackingMemory {
        keys: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Memory for TrackingMemory {
        fn name(&self) -> &str {
            "tracking"
        }

        async fn store(
            &self,
            key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            self.keys.lock().push(key.to_string());
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            let size = self.keys.lock().len();
            Ok(size)
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    fn test_connect_info() -> ConnectInfo<SocketAddr> {
        ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 30_300)))
    }

    fn webhook_test_state(provider: Arc<dyn Provider>) -> AppState {
        AppState {
            config: Arc::new(Mutex::new(Config::default())),
            shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(Config::default())),
            provider,
            model: "test-model".into(),
            temperature: 0.0,
            mem: Arc::new(MockMemory),
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: None,
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(crate::observability::NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_manager: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_middleware: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_hook_executor: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_cron_manager: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        }
    }

    #[tokio::test]
    async fn webhook_idempotency_skips_duplicate_provider_calls() {
        let provider_impl = Arc::new(MockProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let state = webhook_test_state(provider);

        let mut headers = HeaderMap::new();
        headers.insert("X-Idempotency-Key", HeaderValue::from_static("abc-123"));

        let first = handle_webhook(
            State(state.clone()),
            test_connect_info(),
            headers.clone(),
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();
        assert_eq!(first.status(), StatusCode::OK);
        let first_payload = first.into_body().collect().await.unwrap().to_bytes();
        let first_json: serde_json::Value = serde_json::from_slice(&first_payload).unwrap();
        let response_id = first_json["response_id"]
            .as_str()
            .expect("successful idempotent request must expose a stable response identity")
            .to_string();

        let second = handle_webhook(
            State(state),
            test_connect_info(),
            headers,
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();
        assert_eq!(second.status(), StatusCode::OK);

        let payload = second.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(parsed["status"], "duplicate");
        assert_eq!(parsed["idempotent"], true);
        assert_eq!(parsed["response"], "ok");
        assert_eq!(parsed["model"], "test-model");
        assert_eq!(parsed["response_id"], response_id);
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn webhook_idempotency_failed_attempt_can_retry() {
        let provider_impl = Arc::new(FailFirstProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let state = webhook_test_state(provider);
        let mut headers = HeaderMap::new();
        headers.insert("X-Idempotency-Key", HeaderValue::from_static("retry-after-error"));

        let first = handle_webhook(
            State(state.clone()),
            test_connect_info(),
            headers.clone(),
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();
        assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let retry = handle_webhook(
            State(state),
            test_connect_info(),
            headers,
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();
        assert_eq!(retry.status(), StatusCode::OK);
        let payload = retry.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(parsed["response"], "retry-ok");
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn webhook_idempotency_retry_reuses_autosave_key() {
        let provider_impl = Arc::new(FailFirstProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let tracking_impl = Arc::new(TrackingMemory::default());
        let mut state = webhook_test_state(provider);
        state.mem = tracking_impl.clone();
        state.auto_save = true;
        let mut headers = HeaderMap::new();
        headers.insert("X-Idempotency-Key", HeaderValue::from_static("stable-autosave"));

        let first = handle_webhook(
            State(state.clone()),
            test_connect_info(),
            headers.clone(),
            Bytes::from_static(br#"{"message":"remember this stable autosave payload across the retry attempt"}"#),
        )
        .await
        .into_response();
        assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let retry = handle_webhook(
            State(state),
            test_connect_info(),
            headers,
            Bytes::from_static(br#"{"message":"remember this stable autosave payload across the retry attempt"}"#),
        )
        .await
        .into_response();
        assert_eq!(retry.status(), StatusCode::OK);

        let keys = tracking_impl.keys.lock();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], keys[1]);
        assert!(keys[0].starts_with("webhook_msg_"));
        assert!(!keys[0].contains("stable-autosave"));
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn webhook_idempotency_cancelled_attempt_can_retry() {
        let provider_impl = Arc::new(BlockFirstProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let state = webhook_test_state(provider);
        let mut headers = HeaderMap::new();
        headers.insert("X-Idempotency-Key", HeaderValue::from_static("retry-after-cancel"));

        let started = provider_impl.first_started.notified();
        let first_state = state.clone();
        let first_headers = headers.clone();
        let first = tokio::spawn(async move {
            handle_webhook(
                State(first_state),
                test_connect_info(),
                first_headers,
                Bytes::from_static(br#"{"message":"hello"}"#),
            )
            .await
            .into_response()
        });
        started.await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        let retry = handle_webhook(
            State(state),
            test_connect_info(),
            headers,
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();
        assert_eq!(retry.status(), StatusCode::OK);
        let payload = retry.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(parsed["response"], "released");
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn webhook_idempotency_concurrent_attempt_reports_processing() {
        let provider_impl = Arc::new(BlockFirstProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let state = webhook_test_state(provider);
        let mut headers = HeaderMap::new();
        headers.insert("X-Idempotency-Key", HeaderValue::from_static("concurrent-key"));

        let started = provider_impl.first_started.notified();
        let first_state = state.clone();
        let first_headers = headers.clone();
        let first = tokio::spawn(async move {
            handle_webhook(
                State(first_state),
                test_connect_info(),
                first_headers,
                Bytes::from_static(br#"{"message":"hello"}"#),
            )
            .await
            .into_response()
        });
        started.await;

        let concurrent = handle_webhook(
            State(state.clone()),
            test_connect_info(),
            headers.clone(),
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();
        let concurrent_status = concurrent.status();
        let payload = concurrent.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();

        provider_impl.release_first.notify_one();
        let first_response = first.await.unwrap();
        assert_eq!(first_response.status(), StatusCode::OK);
        assert_eq!(concurrent_status, StatusCode::CONFLICT);
        assert_eq!(parsed["status"], "processing");
        assert_eq!(parsed["idempotent"], true);
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn webhook_idempotency_full_capacity_returns_service_unavailable() {
        let provider_impl = Arc::new(BlockFirstProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let mut state = webhook_test_state(provider);
        state.idempotency_store = Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1));

        let mut first_headers = HeaderMap::new();
        first_headers.insert("X-Idempotency-Key", HeaderValue::from_static("first-key"));
        let started = provider_impl.first_started.notified();
        let first_state = state.clone();
        let first = tokio::spawn(async move {
            handle_webhook(
                State(first_state),
                test_connect_info(),
                first_headers,
                Bytes::from_static(br#"{"message":"first"}"#),
            )
            .await
            .into_response()
        });
        started.await;

        let mut second_headers = HeaderMap::new();
        second_headers.insert("X-Idempotency-Key", HeaderValue::from_static("second-key"));
        let at_capacity = handle_webhook(
            State(state),
            test_connect_info(),
            second_headers,
            Bytes::from_static(br#"{"message":"second"}"#),
        )
        .await
        .into_response();
        assert_eq!(at_capacity.status(), StatusCode::SERVICE_UNAVAILABLE);
        let payload = at_capacity.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(parsed["retryable"], true);

        provider_impl.release_first.notify_one();
        assert_eq!(first.await.unwrap().status(), StatusCode::OK);
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn webhook_idempotency_same_key_different_body_conflicts() {
        let provider_impl = Arc::new(MockProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let state = webhook_test_state(provider);
        let mut headers = HeaderMap::new();
        headers.insert("X-Idempotency-Key", HeaderValue::from_static("body-bound-key"));

        let first = handle_webhook(
            State(state.clone()),
            test_connect_info(),
            headers.clone(),
            Bytes::from_static(br#"{"message":"first"}"#),
        )
        .await
        .into_response();
        assert_eq!(first.status(), StatusCode::OK);

        let conflict = handle_webhook(
            State(state),
            test_connect_info(),
            headers,
            Bytes::from_static(br#"{"message":"second"}"#),
        )
        .await
        .into_response();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        let payload = conflict.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(parsed["status"], "request_conflict");
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn webhook_idempotency_rejects_oversize_key() {
        let provider_impl = Arc::new(MockProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let state = webhook_test_state(provider);
        let mut headers = HeaderMap::new();
        let oversized_key = vec![b'x'; IDEMPOTENCY_MAX_KEY_BYTES + 1];
        headers.insert("X-Idempotency-Key", HeaderValue::from_bytes(&oversized_key).unwrap());

        let response = handle_webhook(
            State(state),
            test_connect_info(),
            headers,
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn webhook_idempotency_oversize_success_is_not_reexecuted() {
        let provider_impl = Arc::new(OversizeResponseProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let state = webhook_test_state(provider);
        let mut headers = HeaderMap::new();
        headers.insert("X-Idempotency-Key", HeaderValue::from_static("oversize-result"));

        let first = handle_webhook(
            State(state.clone()),
            test_connect_info(),
            headers.clone(),
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();
        assert_eq!(first.status(), StatusCode::OK);

        let duplicate = handle_webhook(
            State(state),
            test_connect_info(),
            headers,
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();
        assert_eq!(duplicate.status(), StatusCode::CONFLICT);
        let payload = duplicate.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(parsed["status"], "replay_unavailable");
        assert!(parsed["response_id"].as_str().is_some());
        assert!(parsed["result_hash"].as_str().is_some());
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn webhook_records_gateway_message_events() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = tmp.path().to_path_buf();
        let provider_impl = Arc::new(MockProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());

        let state = AppState {
            config: Arc::new(Mutex::new(config.clone())),
            shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(config)),
            provider,
            model: "test-model".into(),
            temperature: 0.0,
            mem: Arc::clone(&memory),
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            hooks: Arc::new(HookManager::new(tmp.path().to_path_buf())),
            webhook_token_hash: None,
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(crate::observability::NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_manager: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_middleware: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_hook_executor: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_cron_manager: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        };

        let mut headers = HeaderMap::new();
        headers.insert("X-Idempotency-Key", HeaderValue::from_static("gateway-event-1"));
        let response = handle_webhook(
            State(state),
            test_connect_info(),
            headers,
            Bytes::from_static(br#"{"message":"hello gateway","reply_target":"client-a"}"#),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        // D4 C4: the durable session_key is now the recipient-aware canonical
        // (`gateway:webhook:client-a:prx`). Recalling by the canonical key (what
        // the production envelope reads) returns the request, shared terminal
        // projections, and provider telemetry for the turn.
        let canonical_principal = MemoryPrincipal {
            workspace_id: tmp.path().to_string_lossy().to_string(),
            agent_id: None,
            persona_id: None,
            session_key: Some("gateway:webhook:client-a:prx".to_string()),
            channel: Some("webhook".to_string()),
            sender: Some("client-a".to_string()),
            owner_id: None,
            legacy_session_key: None,
        };
        let events = memory
            .list_message_events_since(&canonical_principal, 0, 10)
            .await
            .unwrap();

        let user_event = events
            .iter()
            .find(|event| event.role == "user")
            .expect("gateway user event");
        let assistant_event = events
            .iter()
            .find(|event| event.role == "assistant")
            .expect("gateway assistant projection");
        assert_eq!(user_event.source, "gateway");
        assert_eq!(user_event.channel.as_deref(), Some("webhook"));
        assert_eq!(user_event.content, "hello gateway");
        assert_eq!(user_event.session_key.as_deref(), Some("gateway:webhook:client-a:prx"));
        let expected_digest = webhook_idempotency_digest("public", "gateway-event-1");
        assert_eq!(
            user_event.idempotency_key.as_deref(),
            Some(format!("gateway:webhook:{expected_digest}").as_str())
        );
        assert!(
            !user_event
                .idempotency_key
                .as_deref()
                .expect("idempotency digest")
                .contains("gateway-event-1")
        );
        assert_eq!(assistant_event.content, "ok");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "provider.final_outcome")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "turn.finalized")
                .count(),
            1
        );
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);

        // A principal still keyed on the legacy session_key only would NOT see the
        // canonical events (single-key), but a read-merge principal carrying the
        // legacy key as `legacy_session_key` recalls them (D4 union).
        let legacy_only = MemoryPrincipal {
            session_key: Some("gateway:webhook:client-a".to_string()),
            ..canonical_principal.clone()
        };
        assert!(
            memory
                .list_message_events_since(&legacy_only, 0, 10)
                .await
                .unwrap()
                .is_empty()
        );
        let read_merge = MemoryPrincipal {
            session_key: Some("gateway:webhook:client-a:prx".to_string()),
            legacy_session_key: Some("gateway:webhook:client-a".to_string()),
            ..canonical_principal
        };
        assert_eq!(
            memory
                .list_message_events_since(&read_merge, 0, 10)
                .await
                .unwrap()
                .len(),
            5
        );
    }

    #[tokio::test]
    #[ignore = "known failure — webhook autosave key dedup needs fix"]
    async fn webhook_autosave_stores_distinct_keys_per_request() {
        let provider_impl = Arc::new(MockProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();

        let tracking_impl = Arc::new(TrackingMemory::default());
        let memory: Arc<dyn Memory> = tracking_impl.clone();

        let state = AppState {
            config: Arc::new(Mutex::new(Config::default())),
            shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(Config::default())),
            provider,
            model: "test-model".into(),
            temperature: 0.0,
            mem: memory,
            auto_save: true,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: None,
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(crate::observability::NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_manager: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_middleware: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_hook_executor: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_cron_manager: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        };

        let headers = HeaderMap::new();

        let first = handle_webhook(
            State(state.clone()),
            test_connect_info(),
            headers.clone(),
            Bytes::from_static(br#"{"message":"hello one"}"#),
        )
        .await
        .into_response();
        assert_eq!(first.status(), StatusCode::OK);

        let second = handle_webhook(
            State(state),
            test_connect_info(),
            headers,
            Bytes::from_static(br#"{"message":"hello two"}"#),
        )
        .await
        .into_response();
        assert_eq!(second.status(), StatusCode::OK);

        let keys = tracking_impl.keys.lock().clone();
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0], keys[1]);
        assert!(keys[0].starts_with("webhook_msg_"));
        assert!(keys[1].starts_with("webhook_msg_"));
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn webhook_autosave_skips_group_reply_target() {
        let provider_impl = Arc::new(MockProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();

        let tracking_impl = Arc::new(TrackingMemory::default());
        let memory: Arc<dyn Memory> = tracking_impl.clone();

        let state = AppState {
            config: Arc::new(Mutex::new(Config::default())),
            shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(Config::default())),
            provider,
            model: "test-model".into(),
            temperature: 0.0,
            mem: memory,
            auto_save: true,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: None,
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(crate::observability::NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_manager: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_middleware: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_hook_executor: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_cron_manager: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        };

        let response = handle_webhook(
            State(state),
            test_connect_info(),
            HeaderMap::new(),
            Bytes::from_static(br#"{"message":"hello group","reply_target":"group:team-1"}"#),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(tracking_impl.keys.lock().is_empty());
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn webhook_token_hash_is_deterministic_and_nonempty() {
        let secret_a = generate_test_secret();
        let secret_b = generate_test_secret();
        let one = hash_webhook_secret(&secret_a);
        let two = hash_webhook_secret(&secret_a);
        let other = hash_webhook_secret(&secret_b);

        assert_eq!(one, two);
        assert_ne!(one, other);
        assert_eq!(one.len(), 64);
    }

    #[tokio::test]
    async fn webhook_token_hash_rejects_missing_header() {
        let provider_impl = Arc::new(MockProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let memory: Arc<dyn Memory> = Arc::new(MockMemory);
        let secret = generate_test_secret();

        let state = AppState {
            config: Arc::new(Mutex::new(Config::default())),
            shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(Config::default())),
            provider,
            model: "test-model".into(),
            temperature: 0.0,
            mem: memory,
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: Some(Arc::from(hash_webhook_secret(&secret))),
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(crate::observability::NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_manager: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_middleware: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_hook_executor: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_cron_manager: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        };

        let response = handle_webhook(
            State(state),
            test_connect_info(),
            HeaderMap::new(),
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn webhook_token_hash_rejects_invalid_header() {
        let provider_impl = Arc::new(MockProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let memory: Arc<dyn Memory> = Arc::new(MockMemory);
        let valid_secret = generate_test_secret();
        let wrong_secret = generate_test_secret();

        let state = AppState {
            config: Arc::new(Mutex::new(Config::default())),
            shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(Config::default())),
            provider,
            model: "test-model".into(),
            temperature: 0.0,
            mem: memory,
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: Some(Arc::from(hash_webhook_secret(&valid_secret))),
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(crate::observability::NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_manager: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_middleware: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_hook_executor: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_cron_manager: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        };

        let mut headers = HeaderMap::new();
        headers.insert("X-Webhook-Token", HeaderValue::from_str(&wrong_secret).unwrap());

        let response = handle_webhook(
            State(state),
            test_connect_info(),
            headers,
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn webhook_token_hash_accepts_valid_header() {
        let provider_impl = Arc::new(MockProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let memory: Arc<dyn Memory> = Arc::new(MockMemory);
        let secret = generate_test_secret();

        let state = AppState {
            config: Arc::new(Mutex::new(Config::default())),
            shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(Config::default())),
            provider,
            model: "test-model".into(),
            temperature: 0.0,
            mem: memory,
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: Some(Arc::from(hash_webhook_secret(&secret))),
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(crate::observability::NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_manager: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_middleware: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_hook_executor: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_cron_manager: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        };

        let mut headers = HeaderMap::new();
        headers.insert("X-Webhook-Token", HeaderValue::from_str(&secret).unwrap());

        let response = handle_webhook(
            State(state),
            test_connect_info(),
            headers,
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn webhook_signature_rejects_missing_header() {
        let provider_impl = Arc::new(MockProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let memory: Arc<dyn Memory> = Arc::new(MockMemory);
        let secret = generate_test_secret();

        let state = AppState {
            config: Arc::new(Mutex::new(Config::default())),
            shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(Config::default())),
            provider,
            model: "test-model".into(),
            temperature: 0.0,
            mem: memory,
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: None,
            webhook_signing_secret: Some(Arc::from(secret)),
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(crate::observability::NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_manager: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_middleware: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_hook_executor: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_cron_manager: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        };

        let response = handle_webhook(
            State(state),
            test_connect_info(),
            HeaderMap::new(),
            Bytes::from_static(br#"{"message":"hello"}"#),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn webhook_signature_accepts_valid_hmac() {
        let provider_impl = Arc::new(MockProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let memory: Arc<dyn Memory> = Arc::new(MockMemory);
        let secret = generate_test_secret();
        let body = br#"{"message":"hello"}"#;

        let state = AppState {
            config: Arc::new(Mutex::new(Config::default())),
            shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(Config::default())),
            provider,
            model: "test-model".into(),
            temperature: 0.0,
            mem: memory,
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: None,
            webhook_signing_secret: Some(Arc::from(secret.clone())),
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(crate::observability::NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_manager: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_middleware: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_hook_executor: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_cron_manager: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        };

        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Webhook-Signature",
            HeaderValue::from_str(&format!("sha256={}", compute_whatsapp_signature_hex(&secret, body))).unwrap(),
        );

        let response = handle_webhook(State(state), test_connect_info(), headers, Bytes::from_static(body))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
    }

    fn compute_nextcloud_signature_hex(secret: &str, random: &str, body: &str) -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let payload = format!("{random}{body}");
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    #[tokio::test]
    async fn nextcloud_talk_webhook_returns_not_found_when_not_configured() {
        let provider: Arc<dyn Provider> = Arc::new(MockProvider::default());
        let memory: Arc<dyn Memory> = Arc::new(MockMemory);

        let state = AppState {
            config: Arc::new(Mutex::new(Config::default())),
            shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(Config::default())),
            provider,
            model: "test-model".into(),
            temperature: 0.0,
            mem: memory,
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: None,
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(crate::observability::NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_manager: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_middleware: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_hook_executor: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_cron_manager: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        };

        let response = handle_nextcloud_talk_webhook(
            State(state),
            HeaderMap::new(),
            Bytes::from_static(br#"{"type":"message"}"#),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn nextcloud_talk_webhook_rejects_invalid_signature() {
        let provider_impl = Arc::new(MockProvider::default());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let memory: Arc<dyn Memory> = Arc::new(MockMemory);

        let channel = Arc::new(NextcloudTalkChannel::new(
            "https://cloud.example.com".into(),
            "app-token".into(),
            vec!["*".into()],
        ));

        let secret = "nextcloud-test-secret";
        let random = "seed-value";
        let body = r#"{"type":"message","object":{"token":"room-token"},"message":{"actorType":"users","actorId":"user_a","message":"hello"}}"#;
        let _valid_signature = compute_nextcloud_signature_hex(secret, random, body);
        let invalid_signature = "deadbeef";

        let state = AppState {
            config: Arc::new(Mutex::new(Config::default())),
            shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(Config::default())),
            provider,
            model: "test-model".into(),
            temperature: 0.0,
            mem: memory,
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: None,
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: Some(channel),
            nextcloud_talk_webhook_secret: Some(Arc::from(secret)),
            observer: Arc::new(crate::observability::NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_manager: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_middleware: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_hook_executor: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_cron_manager: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        };

        let mut headers = HeaderMap::new();
        headers.insert("X-Nextcloud-Talk-Random", HeaderValue::from_str(random).unwrap());
        headers.insert(
            "X-Nextcloud-Talk-Signature",
            HeaderValue::from_str(invalid_signature).unwrap(),
        );

        let response = handle_nextcloud_talk_webhook(State(state), headers, Bytes::from(body))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 0);
    }

    // ══════════════════════════════════════════════════════════
    // WhatsApp Signature Verification Tests (CWE-345 Prevention)
    // ══════════════════════════════════════════════════════════

    fn compute_whatsapp_signature_hex(secret: &str, body: &[u8]) -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    fn compute_whatsapp_signature_header(secret: &str, body: &[u8]) -> String {
        format!("sha256={}", compute_whatsapp_signature_hex(secret, body))
    }

    // D4 C4: gateway fabric durable session_key migrates to the recipient-aware
    // canonical while the legacy key is carried for read-merge.
    #[test]
    fn d4_gateway_fabric_durable_key_is_canonical_with_legacy_read_merge() {
        // webhook ingress
        let webhook = GatewayFabricContext::generic_webhook(Some("client-42"), None);
        assert_eq!(webhook.session_key, "gateway:webhook:client-42"); // legacy form
        let canonical = RuntimeEnvelope::gateway(
            "ws",
            webhook.session_key.clone(),
            webhook.channel.clone(),
            webhook.sender.clone(),
            webhook.recipient.clone(),
            MemoryVisibility::Session,
        )
        .canonical_session_key();
        // canonical is recipient-aware: gateway:{channel}:{sender}:{recipient}
        assert_eq!(canonical, "gateway:webhook:client-42:prx");
        assert_ne!(canonical, webhook.session_key);

        // The migrated envelope writes/reads the canonical durable key and carries
        // the legacy key for read-merge.
        let envelope = RuntimeEnvelope::gateway(
            "ws",
            canonical.clone(),
            webhook.channel.clone(),
            webhook.sender.clone(),
            webhook.recipient.clone(),
            MemoryVisibility::Session,
        )
        .with_legacy_session_key(webhook.session_key);
        assert_eq!(
            envelope.message_scope().session_key.as_deref(),
            Some(canonical.as_str())
        );
        let principal = envelope.memory_principal();
        assert_eq!(principal.session_key.as_deref(), Some(canonical.as_str()));
        assert_eq!(
            principal.legacy_session_key.as_deref(),
            Some("gateway:webhook:client-42")
        );
        assert_eq!(
            principal.session_key_candidates(),
            vec![
                "gateway:webhook:client-42:prx".to_string(),
                "gateway:webhook:client-42".to_string()
            ]
        );
    }

    #[test]
    fn whatsapp_signature_valid() {
        let app_secret = generate_test_secret();
        let body = b"test body content";

        let signature_header = compute_whatsapp_signature_header(&app_secret, body);

        assert!(verify_whatsapp_signature(&app_secret, body, &signature_header));
    }

    #[test]
    fn whatsapp_signature_invalid_wrong_secret() {
        let app_secret = generate_test_secret();
        let wrong_secret = generate_test_secret();
        let body = b"test body content";

        let signature_header = compute_whatsapp_signature_header(&wrong_secret, body);

        assert!(!verify_whatsapp_signature(&app_secret, body, &signature_header));
    }

    #[test]
    fn whatsapp_signature_invalid_wrong_body() {
        let app_secret = generate_test_secret();
        let original_body = b"original body";
        let tampered_body = b"tampered body";

        let signature_header = compute_whatsapp_signature_header(&app_secret, original_body);

        // Verify with tampered body should fail
        assert!(!verify_whatsapp_signature(
            &app_secret,
            tampered_body,
            &signature_header
        ));
    }

    #[test]
    fn whatsapp_signature_missing_prefix() {
        let app_secret = generate_test_secret();
        let body = b"test body";

        // Signature without "sha256=" prefix
        let signature_header = "abc123def456";

        assert!(!verify_whatsapp_signature(&app_secret, body, signature_header));
    }

    #[test]
    fn whatsapp_signature_empty_header() {
        let app_secret = generate_test_secret();
        let body = b"test body";

        assert!(!verify_whatsapp_signature(&app_secret, body, ""));
    }

    #[test]
    fn whatsapp_signature_invalid_hex() {
        let app_secret = generate_test_secret();
        let body = b"test body";

        // Invalid hex characters
        let signature_header = "sha256=not_valid_hex_zzz";

        assert!(!verify_whatsapp_signature(&app_secret, body, signature_header));
    }

    #[test]
    fn whatsapp_signature_empty_body() {
        let app_secret = generate_test_secret();
        let body = b"";

        let signature_header = compute_whatsapp_signature_header(&app_secret, body);

        assert!(verify_whatsapp_signature(&app_secret, body, &signature_header));
    }

    #[test]
    fn whatsapp_signature_unicode_body() {
        let app_secret = generate_test_secret();
        let body = "Hello 🦀 World".as_bytes();

        let signature_header = compute_whatsapp_signature_header(&app_secret, body);

        assert!(verify_whatsapp_signature(&app_secret, body, &signature_header));
    }

    #[test]
    fn whatsapp_signature_json_payload() {
        let app_secret = generate_test_secret();
        let body =
            br#"{"entry":[{"changes":[{"value":{"messages":[{"from":"1234567890","text":{"body":"Hello"}}]}}]}]}"#;

        let signature_header = compute_whatsapp_signature_header(&app_secret, body);

        assert!(verify_whatsapp_signature(&app_secret, body, &signature_header));
    }

    #[test]
    fn whatsapp_signature_case_sensitive_prefix() {
        let app_secret = generate_test_secret();
        let body = b"test body";

        let hex_sig = compute_whatsapp_signature_hex(&app_secret, body);

        // Wrong case prefix should fail
        let wrong_prefix = format!("SHA256={hex_sig}");
        assert!(!verify_whatsapp_signature(&app_secret, body, &wrong_prefix));

        // Correct prefix should pass
        let correct_prefix = format!("sha256={hex_sig}");
        assert!(verify_whatsapp_signature(&app_secret, body, &correct_prefix));
    }

    #[test]
    fn whatsapp_signature_truncated_hex() {
        let app_secret = generate_test_secret();
        let body = b"test body";

        let hex_sig = compute_whatsapp_signature_hex(&app_secret, body);
        let truncated = &hex_sig[..32]; // Only half the signature
        let signature_header = format!("sha256={truncated}");

        assert!(!verify_whatsapp_signature(&app_secret, body, &signature_header));
    }

    #[test]
    fn whatsapp_signature_extra_bytes() {
        let app_secret = generate_test_secret();
        let body = b"test body";

        let hex_sig = compute_whatsapp_signature_hex(&app_secret, body);
        let extended = format!("{hex_sig}deadbeef");
        let signature_header = format!("sha256={extended}");

        assert!(!verify_whatsapp_signature(&app_secret, body, &signature_header));
    }

    // ══════════════════════════════════════════════════════════
    // IdempotencyStore Edge-Case Tests
    // ══════════════════════════════════════════════════════════

    #[test]
    fn idempotency_store_allows_different_keys() {
        let store = Arc::new(IdempotencyStore::new(Duration::from_secs(60), 100));
        let claims = ["key-a", "key-b", "key-c", "key-d"]
            .into_iter()
            .enumerate()
            .map(|(index, key)| acquire_idempotency_claim(&store, key, test_fingerprint(index as u8)))
            .collect::<Vec<_>>();

        assert_eq!(claims.len(), 4);
    }

    #[test]
    fn idempotency_store_max_keys_clamped_to_one() {
        let store = Arc::new(IdempotencyStore::new(Duration::from_secs(60), 0));
        let _claim = acquire_idempotency_claim(&store, "only-key", test_fingerprint(1));
        assert!(matches!(
            store.claim("second-key".to_string(), test_fingerprint(2)),
            IdempotencyClaimOutcome::AtCapacity
        ));
    }

    #[test]
    fn idempotency_store_same_key_different_body_conflicts() {
        let store = Arc::new(IdempotencyStore::new(Duration::from_secs(300), 100));
        let _claim = acquire_idempotency_claim(&store, "rapid", test_fingerprint(1));
        assert!(matches!(
            store.claim("rapid".to_string(), test_fingerprint(2)),
            IdempotencyClaimOutcome::RequestConflict
        ));
    }

    #[test]
    fn idempotency_store_processing_survives_ttl_and_failed_retry_expires() {
        let store = Arc::new(IdempotencyStore::new(Duration::from_millis(1), 100));
        let claim = acquire_idempotency_claim(&store, "ttl-key", test_fingerprint(1));
        std::thread::sleep(Duration::from_millis(10));
        assert!(matches!(
            store.claim("ttl-key".to_string(), test_fingerprint(1)),
            IdempotencyClaimOutcome::Processing
        ));

        assert!(claim.fail(true));
        std::thread::sleep(Duration::from_millis(10));
        let _retry = acquire_idempotency_claim(&store, "ttl-key", test_fingerprint(1));
    }

    #[test]
    fn idempotency_store_terminal_ttl_starts_at_completion() {
        let store = Arc::new(IdempotencyStore::new(Duration::from_millis(20), 1));
        let claim = acquire_idempotency_claim(&store, "old-key", test_fingerprint(1));
        std::thread::sleep(Duration::from_millis(15));
        assert!(claim.succeed(test_idempotency_replay("ok"), "hash".to_string()));
        std::thread::sleep(Duration::from_millis(10));
        assert!(matches!(
            store.claim("old-key".to_string(), test_fingerprint(1)),
            IdempotencyClaimOutcome::Replay(_)
        ));
        std::thread::sleep(Duration::from_millis(15));
        let _retry = acquire_idempotency_claim(&store, "old-key", test_fingerprint(1));
    }

    #[test]
    fn idempotency_store_stale_generation_cannot_finish_new_attempt() {
        let store = Arc::new(IdempotencyStore::new(Duration::from_secs(60), 1));
        let first = acquire_idempotency_claim(&store, "key", test_fingerprint(1));
        let first_generation = first.generation;
        assert!(first.fail(true));
        let second = acquire_idempotency_claim(&store, "key", test_fingerprint(1));

        assert!(!store.complete_if_owner(
            "key",
            first_generation,
            test_idempotency_replay("stale"),
            "stale-hash".to_string(),
        ));
        assert!(matches!(
            store.claim("key".to_string(), test_fingerprint(1)),
            IdempotencyClaimOutcome::Processing
        ));
        assert!(second.succeed(test_idempotency_replay("fresh"), "fresh-hash".to_string()));
    }

    #[test]
    fn idempotency_store_payload_budget_is_released_after_failure() {
        let store = Arc::new(IdempotencyStore::new(Duration::from_secs(60), 100));
        let mut claims = (0..(IDEMPOTENCY_REPLAY_BUDGET_BYTES / IDEMPOTENCY_MAX_REPLAY_BYTES))
            .map(|index| acquire_idempotency_claim(&store, &format!("key-{index}"), test_fingerprint(index as u8)))
            .collect::<Vec<_>>();
        assert!(matches!(
            store.claim("over-budget".to_string(), test_fingerprint(250)),
            IdempotencyClaimOutcome::AtCapacity
        ));

        assert!(claims.pop().expect("one claim").fail(true));
        let _replacement = acquire_idempotency_claim(&store, "replacement", test_fingerprint(251));
    }

    #[test]
    fn idempotency_store_oversize_success_keeps_non_reexecuting_tombstone() {
        let store = Arc::new(IdempotencyStore::new(Duration::from_secs(60), 1));
        let claim = acquire_idempotency_claim(&store, "oversize", test_fingerprint(1));
        let replay = IdempotencyReplay {
            response_id: Uuid::new_v4(),
            response: Arc::from("x".repeat(IDEMPOTENCY_MAX_REPLAY_BYTES + 1)),
            model: Arc::from("test-model"),
        };
        let response_id = replay.response_id;
        assert!(claim.succeed(replay, "oversize-hash".to_string()));

        assert!(matches!(
            store.claim("oversize".to_string(), test_fingerprint(1)),
            IdempotencyClaimOutcome::ReplayUnavailable {
                response_id: id,
                result_hash,
            } if id == response_id && result_hash == "oversize-hash"
        ));
    }

    /// S2.5 P1-A: /metrics 端点在 PrometheusObserver 模式下包含 chat counter 指标.
    #[tokio::test]
    async fn s2_5_p1c_metrics_endpoint_exposes_chat_counters() {
        let prom = Arc::new(crate::observability::PrometheusObserver::try_new().unwrap());
        crate::observability::Observer::record_event(
            prom.as_ref(),
            &crate::observability::ObserverEvent::HeartbeatTick,
        );
        // 递增 4 个 chat counter（使用唯一 label 避免与其他测试累计值干扰）
        crate::observability::chat_metrics::inc_action("s2_5_p1c_test_kind");
        crate::observability::chat_metrics::inc_effect("s2_5_p1c_test_effect");
        crate::observability::chat_metrics::inc_stream_chunk();
        crate::observability::chat_metrics::inc_dispatch_drop("s2_5_p1c_test_drop");

        let observer: Arc<dyn crate::observability::Observer> = prom;
        let state = AppState {
            config: Arc::new(Mutex::new(Config::default())),
            shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(Config::default())),
            provider: Arc::new(MockProvider::default()),
            model: "test-model".into(),
            temperature: 0.0,
            mem: Arc::new(MockMemory),
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: None,
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer,
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_manager: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_middleware: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_hook_executor: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_cron_manager: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        };

        let response = handle_metrics(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        // PrometheusObserver 自身指标仍在
        assert!(
            text.contains("prx_heartbeat_ticks_total"),
            "observer metrics must be present"
        );
        // chat 4 个 counter 名都要出现
        assert!(text.contains("prx_chat_actions_total"), "chat actions counter missing");
        assert!(text.contains("prx_chat_effects_total"), "chat effects counter missing");
        assert!(
            text.contains("prx_chat_stream_chunks_total"),
            "chat stream chunks counter missing"
        );
        assert!(
            text.contains("prx_chat_dispatch_drops_total"),
            "chat dispatch drops counter missing"
        );
    }

    /// S2.5 P1-A: /metrics 端点在 NoopObserver 模式下仍暴露 chat counter 指标.
    #[tokio::test]
    async fn s2_5_p1c_metrics_endpoint_exposes_chat_when_observer_noop() {
        // 触发 LazyLock 初始化，确保 4 个 counter 注册到 CHAT_REGISTRY
        crate::observability::chat_metrics::inc_action("s2_5_p1c_noop_kind");
        crate::observability::chat_metrics::inc_effect("s2_5_p1c_noop_effect");
        crate::observability::chat_metrics::inc_stream_chunk();
        crate::observability::chat_metrics::inc_dispatch_drop("s2_5_p1c_noop_drop");

        let state = AppState {
            config: Arc::new(Mutex::new(Config::default())),
            shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(Config::default())),
            provider: Arc::new(MockProvider::default()),
            model: "test-model".into(),
            temperature: 0.0,
            mem: Arc::new(MockMemory),
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: None,
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(crate::observability::NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_manager: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_middleware: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_hook_executor: None,
            #[cfg(feature = "wasm-plugins")]
            wasm_cron_manager: None,
            #[cfg(feature = "wasm-plugins")]
            event_bus: None,
        };

        let response = handle_metrics(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        // noop observer 降级路径：仍含 hint 前缀
        assert!(
            text.contains("Prometheus backend not enabled"),
            "hint prefix must be present"
        );
        // chat counter 名也要出现（即使 noop observer）
        assert!(
            text.contains("prx_chat_actions_total"),
            "chat actions counter missing with noop observer"
        );
        assert!(
            text.contains("prx_chat_effects_total"),
            "chat effects counter missing with noop observer"
        );
        assert!(
            text.contains("prx_chat_stream_chunks_total"),
            "chat stream chunks counter missing with noop observer"
        );
        assert!(
            text.contains("prx_chat_dispatch_drops_total"),
            "chat dispatch drops counter missing with noop observer"
        );
    }

    #[test]
    fn readiness_http_is_non_success_without_ready_required_components() {
        let runtime = crate::health::HealthSnapshot {
            pid: 1,
            updated_at: chrono::Utc::now().to_rfc3339(),
            uptime_seconds: 1,
            components: std::collections::BTreeMap::new(),
        };

        let (status, Json(body)) = health_response(false, runtime);

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "not_ready");
    }
}

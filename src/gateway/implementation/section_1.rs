

#[cfg(test)]
pub(crate) use api::{drive_disabled_engine_console_entry, drive_search_console_entry};


pub use h2_control::run_gateway_controlled;





/// The reserved issuer component of every built-in (non-bearer) MCP origin.
///
/// Re-exported so `Config::validate` can refuse a `mcp_server.jwt_issuer` that
/// would collide with one of them inside the `:`-composed caller identity.
pub use compat::FIXED_EXTERNAL_ISSUERS;




use turn_failure::TurnFailureClass;
use turn_readiness::{GatewayTurnState, TurnNotReady, TurnNotReadyReason, TurnRuntimeStandby};

use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::turn_engine::run_turn;
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
use arc_swap::ArcSwap;
use axum::{
    Router,
    body::Bytes,
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Json},
    routing::{get, post},
};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tower_http::limit::RequestBodyLimitLayer;
use uuid::Uuid;

/// The gateway's own announce channel for `sessions_spawn`.
///
/// `sessions_spawn` needs one `Arc<dyn Channel>` as the fallback destination for
/// a run's completion announcement (and for a kill receipt). Until now the
/// gateway only had one when Signal happened to be configured, so on a host with
/// no Signal account the tool was never registered at all and a webhook turn
/// could not spawn anything.
///
/// The gateway is a request/response surface: it owns no conversation to push an
/// unsolicited message into, and inventing one would mean a new outbound
/// transport. So this channel deliberately accepts the announcement and drops the
/// outbound leg, recording it in the log instead. Nothing is lost: every run is
/// already recorded in the shared memory fabric by the tool itself, so the result
/// is collected the way a gateway caller collects it anyway — `sessions_spawn`'s
/// own `join`, `sessions_list` / `sessions_history`, or `GET /api/runtime/tasks`.
///
/// `listen` refuses: this is an outbound sink, never an inbound listener (the
/// 2026-08-26 stage-c constraint — no new listeners, no new transports).
#[derive(Debug, Clone, Copy)]
struct GatewayAnnounceChannel;

#[async_trait::async_trait]
impl Channel for GatewayAnnounceChannel {
    fn name(&self) -> &str {
        "gateway"
    }

    async fn send(&self, message: &SendMessage) -> Result<()> {
        tracing::info!(
            recipient = %message.recipient,
            body_len = message.content.len(),
            "gateway sessions_spawn announcement has no outbound channel; \
             the run result stays retrievable via join / sessions_list"
        );
        Ok(())
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<crate::channels::traits::ChannelMessage>) -> Result<()> {
        anyhow::bail!("gateway announce channel does not accept inbound messages")
    }
}

/// Pick the channel `sessions_spawn` announces on for a gateway process.
///
/// Signal keeps priority whenever it is configured, so a deployment that has one
/// behaves exactly as before. Without it the gateway falls back to its own
/// [`GatewayAnnounceChannel`] rather than skipping the tool: registration of
/// `sessions_spawn` must not depend on an unrelated IM channel being present.
fn gateway_spawn_announce_channel(signal: Option<Arc<dyn Channel>>) -> Arc<dyn Channel> {
    signal.unwrap_or_else(|| Arc::new(GatewayAnnounceChannel))
}

/// Maximum request body size (64KB) — prevents memory exhaustion
pub const MAX_BODY_SIZE: usize = 65_536;
/// Larger request body limit for `/api` routes to support media uploads
/// (`10 * 20MB` files plus multipart overhead).
pub const MAX_API_BODY_SIZE: usize = (10 * 20 * 1024 * 1024) + (1024 * 1024);
/// Sliding window used by gateway rate limiting.
pub const RATE_LIMIT_WINDOW_SECS: u64 = 60;

/// The coded `429 rate_limited` of the `/pair` and `/webhook` limits: the shared
/// `{"error":{"code","message","retry_after_ms"}}` body plus a `Retry-After`
/// header in whole seconds.
fn rate_limited_reply(message: &str, retry_after_secs: u64) -> axum::response::Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({
            "error": {
                "code": "rate_limited",
                "message": message,
                "retry_after_ms": retry_after_secs.saturating_mul(1000),
            }
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, header::HeaderValue::from(retry_after_secs));
    response
}
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
/// Refresh required gateway liveness independently of inbound traffic. This is
/// deliberately shorter than the daemon's 60-second core-health TTL.
const GATEWAY_HEALTH_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);

async fn run_health_heartbeat(component: &'static str, interval: Duration, shutdown: CancellationToken) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ticker.tick() => crate::health::touch_component(component),
            () = shutdown.cancelled() => break,
        }
    }
}

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
    /// The authenticated webhook credential the request presented
    /// (`bearer:{sha256}` for a paired token, `token:{sha256}` for the
    /// standalone webhook token, `signing-secret:{sha256}` for HMAC, `public`
    /// when the gateway requires none). `None` for platform channel messages,
    /// whose sender the platform authenticates.
    credential: Option<String>,
}

impl GatewayFabricContext {
    /// `reply_target` is chosen by the client, so it never decides the owner
    /// or the conversation on its own: the turn owner is the credential with
    /// `reply_target` as a sub-namespace under it
    /// ([`Self::webhook_owner_id`], R-MCPB F-2), and so is the conversation
    /// session key, `gateway:webhook:{credential}:{reply_target}` (D-025).
    fn generic_webhook(reply_target: Option<&str>, credential: &str, idempotency_key: Option<String>) -> Self {
        let target = reply_target.unwrap_or("webhook-client");
        Self {
            channel: "webhook".to_string(),
            session_key: format!("gateway:webhook:{credential}:{target}"),
            sender: target.to_string(),
            recipient: "prx".to_string(),
            idempotency_key,
            credential: Some(credential.to_string()),
        }
    }

    /// The owner a webhook turn acts for:
    /// `owner:{workspace}:webhook:{credential}:{reply_target}`. Two different
    /// credentials never compose the same owner, whatever `reply_target` they
    /// send, so one token cannot reach another token's sub-agent runs or
    /// owner-scoped rows. Holders of one credential remain one trust domain,
    /// separated only by the `reply_target` they assert. `None` for a platform
    /// channel message, which keeps its envelope-derived owner.
    fn webhook_owner_id(&self, workspace_id: &str) -> Option<String> {
        let credential = self.credential.as_deref()?;
        Some(
            crate::memory::principal::OwnerPrincipal::new(
                workspace_id,
                self.channel.as_str(),
                format!("{credential}:{}", self.sender),
                self.session_key.as_str(),
                vec![crate::memory::principal::Role::Anonymous],
            )
            .owner_id,
        )
    }

    fn channel_message(msg: &crate::channels::traits::ChannelMessage) -> Self {
        Self {
            channel: msg.channel.clone(),
            session_key: format!("gateway:{}:{}", msg.channel, msg.sender),
            sender: msg.sender.clone(),
            recipient: msg.reply_target.clone(),
            idempotency_key: Some(format!("gateway:{}:{}", msg.channel, msg.id)),
            credential: None,
        }
    }
}

/// Domain separator of the webhook owner digest (R-MCPBfg N-4). Bumping it
/// re-keys every webhook owner and conversation once.
const WEBHOOK_OWNER_DIGEST_DOMAIN: &str = "prx-webhook-owner-v1";

/// Hex digits of the webhook owner digest kept in owner ids and session keys:
/// 128 bits.
const WEBHOOK_OWNER_DIGEST_HEX: usize = 32;

/// The credential part of a `/webhook` owner id and session key:
/// `{kind}:{digest}`, where the digest is SHA-256 over
/// `prx-webhook-owner-v1 \0 kind \0 secret`, truncated to 128 bits.
///
/// Domain-separated on purpose. The plain SHA-256 of a paired token is the
/// at-rest verifier in `gateway.paired_tokens`, and the plain digest of an
/// operator-chosen `X-Webhook-Token` or signing secret is an offline-guessable
/// verifier; neither belongs in message events, run tags, session keys and
/// logs. The value only has to be stable per credential and distinct across
/// credentials.
fn webhook_owner_credential(kind: &str, secret: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(WEBHOOK_OWNER_DIGEST_DOMAIN.as_bytes());
    hasher.update([0u8]);
    hasher.update(kind.as_bytes());
    hasher.update([0u8]);
    hasher.update(secret.as_bytes());
    let digest = hex::encode(hasher.finalize());
    let digest = digest.get(..WEBHOOK_OWNER_DIGEST_HEX).unwrap_or(digest.as_str());
    format!("{kind}:{digest}")
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
    /// The budget, re-readable at runtime so a configuration reload can adopt a
    /// new one **without** discarding the windows already recorded. Rebuilding
    /// the limiter instead would hand every caller a fresh window, which is
    /// precisely the moment a throttled caller must not be allowed to burst.
    limit_per_window: AtomicU32,
    window: Duration,
    max_keys: usize,
    requests: Mutex<(HashMap<String, Vec<Instant>>, Instant)>,
}

impl SlidingWindowRateLimiter {
    fn new(limit_per_window: u32, window: Duration, max_keys: usize) -> Self {
        Self {
            limit_per_window: AtomicU32::new(limit_per_window),
            window,
            max_keys: max_keys.max(1),
            requests: Mutex::new((HashMap::new(), Instant::now())),
        }
    }

    /// The budget this limiter currently enforces.
    fn limit_per_window(&self) -> u32 {
        self.limit_per_window.load(Ordering::Relaxed)
    }

    /// Adopt a new budget, keeping every recorded window.
    fn set_limit_per_window(&self, limit_per_window: u32) {
        self.limit_per_window.store(limit_per_window, Ordering::Relaxed);
    }

    fn prune_stale(requests: &mut HashMap<String, Vec<Instant>>, cutoff: Instant) {
        requests.retain(|_, timestamps| {
            timestamps.retain(|t| *t > cutoff);
            !timestamps.is_empty()
        });
    }

    fn allow(&self, key: &str) -> bool {
        let limit_per_window = self.limit_per_window();
        if limit_per_window == 0 {
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

        if entry.len() >= limit_per_window as usize {
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
    /// Whether this attempt reached work that can produce observable side
    /// effects — memory writes, tool calls, outbound messages.
    ///
    /// This is the only thing that decides whether a redelivery of the same key
    /// is allowed to run again. Before dispatch a failed attempt is provably a
    /// no-op, so re-running it is free. After dispatch nothing here knows how
    /// far the attempt got, so re-running it could write the file, send the
    /// message, or call the external API a second time. "Not known to be safe"
    /// is therefore treated as "not retryable", never the other way round.
    dispatched: bool,
}

impl IdempotencyClaim {
    /// Record that the attempt is about to start doing observable work.
    ///
    /// Call this immediately before the first side effect, not at claim
    /// acquisition: everything up to that point is what makes a retry safe.
    const fn mark_dispatched(&mut self) {
        self.dispatched = true;
    }

    /// Whether a redelivery of this key may re-execute the request.
    const fn retry_eligible(&self) -> bool {
        !self.dispatched
    }

    fn succeed(mut self, replay: IdempotencyReplay, result_hash: String) -> bool {
        let transitioned = self
            .store
            .complete_if_owner(&self.key_digest, self.generation, replay, result_hash);
        if transitioned {
            self.armed = false;
        }
        transitioned
    }

    fn succeed_unavailable(mut self, response_id: Uuid, result_hash: String) -> bool {
        let transitioned =
            self.store
                .complete_result_if_owner(&self.key_digest, self.generation, response_id, result_hash, None);
        if transitioned {
            self.armed = false;
        }
        transitioned
    }

    fn fail(mut self) -> bool {
        let retry_eligible = self.retry_eligible();
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
    /// A claim that reaches `Drop` still armed was abandoned without anyone
    /// deciding how the attempt ended: the task was cancelled or killed, a
    /// caller took an early return path, or a panic unwound through it.
    ///
    /// This used to release the key as `retry_eligible = true`, which turned
    /// every abandoned attempt into an invitation to run the whole request
    /// again — while the original may still have been mid-flight, and after it
    /// may already have written half its side effects. Abandonment carries no
    /// information about how far the work got, so it resolves the same way any
    /// other unknown does: dispatched work is not retryable.
    fn drop(&mut self) {
        if self.armed {
            let retry_eligible = self.retry_eligible();
            let _ = self
                .store
                .fail_if_owner(&self.key_digest, self.generation, retry_eligible);
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
            dispatched: false,
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
        let response_id = replay.response_id;
        let replay = (replay.payload_bytes() <= IDEMPOTENCY_MAX_REPLAY_BYTES).then_some(replay);
        self.complete_result_if_owner(key_digest, generation, response_id, result_hash, replay)
    }

    fn complete_result_if_owner(
        &self,
        key_digest: &str,
        generation: u64,
        response_id: Uuid,
        result_hash: String,
        replay: Option<IdempotencyReplay>,
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
        state.payload_bytes = state.payload_bytes.saturating_sub(reserved_bytes);
        state.payload_bytes = state
            .payload_bytes
            .saturating_add(replay.as_ref().map_or(0, IdempotencyReplay::payload_bytes));
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

/// Read an idempotency-key request header under a shared length cap.
///
/// `/webhook` (`X-Idempotency-Key`) and `POST /mcp/v1/tools/call`
/// (`Idempotency-Key`) both accept a caller-supplied key, so both cap it the
/// same way: a non-UTF-8 or over-long value is a client error rather than
/// something silently truncated into a different key, and a blank value is
/// treated as "no key" rather than as the empty key.
fn idempotency_key_from_header(headers: &HeaderMap, header_name: &str) -> Result<Option<String>, String> {
    let Some(value) = headers.get(header_name) else {
        return Ok(None);
    };
    let Ok(value) = value.to_str() else {
        return Err(format!("{header_name} must be valid UTF-8"));
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > IDEMPOTENCY_MAX_KEY_BYTES {
        return Err(format!(
            "{header_name} exceeds the {IDEMPOTENCY_MAX_KEY_BYTES}-byte limit"
        ));
    }
    Ok(Some(value.to_string()))
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
    crate::agent::terminal::oversized_reply::result_hash(response, model)
}

fn replay_unavailable_body(response_id: Uuid, result_hash: &str) -> serde_json::Value {
    serde_json::json!({
        "status": "replay_unavailable", "idempotent": true,
        "response_id": response_id.to_string(), "result_hash": result_hash,
        "message": "The original request succeeded, but its response is too large to replay"
    })
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
    let config = state.config.load_full();
    authorize_gateway_config_resource_mutation(&config, operation_name, risk)
}

fn authorize_gateway_config_resource_mutation(
    config: &Config,
    operation_name: &str,
    risk: ResourceRiskLevel,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
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
    pub rf_streams: Arc<live_frames::StreamHub>,
    pub rf_approvals: Arc<run_approvals::ApprovalHub>,
    /// Sole configuration generation owner used by every gateway path.
    pub config: crate::config::SharedConfig,
    /// Test fixtures' provider, served by `pin_turn_runtime` when a test
    /// state has no turn runtime; every production turn takes its provider
    /// from [`turn_runtime`](Self::turn_runtime).
    #[cfg(test)]
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub temperature: f64,
    pub mem: Arc<dyn Memory>,
    pub auto_save: bool,
    /// Test fixtures' registry, served by `pin_turn_runtime` when a test
    /// state has no turn runtime. Production state never holds one: every
    /// handler pins [`turn_runtime`](Self::turn_runtime), and a registry kept
    /// here would retain the startup generation, and its MCP children, for
    /// the gateway's lifetime (D-098 F-1).
    #[cfg(test)]
    pub tools_registry: Arc<Vec<Box<dyn Tool>>>,
    /// Test fixtures' MCP tool (see [`tools_registry`](Self::tools_registry)).
    #[cfg(test)]
    pub mcp_tool: Option<Arc<McpTool>>,
    /// Atomically rebuilt provider/model/tools bundle for newly admitted turns.
    pub turn_runtime: Option<Arc<GatewayTurnRuntimeOwner>>,
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
    /// Sole process-level owner of the WASM registry and every derived adapter.
    #[cfg(feature = "wasm-plugins")]
    pub plugin_runtime: Option<Arc<crate::plugins::PluginRuntime>>,
}

pub struct TurnRuntimeGeneration {
    provider_binding: Option<Arc<turn_source::GatewayProviderBinding>>,
    publication_slot: Arc<turn_source::publication_slot::GatewayPublicationSlot>,
    pub config_generation: Arc<crate::config::ConfigGeneration>,
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub temperature: f64,
    pub tools_registry: Arc<Vec<Box<dyn Tool>>>,
    pub mcp_tool: Option<Arc<McpTool>>,
}

struct ConfigGenerationMessageAuditSink {
    memory: Arc<dyn Memory>,
    workspace_id: String,
    runtime_handle: tokio::runtime::Handle,
}

impl crate::config::ConfigGenerationAuditSink for ConfigGenerationMessageAuditSink {
    fn record(&self, event: crate::config::ConfigGenerationAuditEvent) {
        let memory = Arc::clone(&self.memory);
        let workspace_id = self.workspace_id.clone();
        self.runtime_handle.spawn(async move {
            let source_revision = event
                .source_revision
                .as_ref()
                .map(|revision| revision.fingerprint_sha256.clone());
            let raw_payload_json = serde_json::to_string(&event).ok();
            let input = crate::memory::MessageEventInput {
                agent_partition: None,
                event_id: None,
                idempotency_key: None,
                workspace_id,
                owner_id: Some("system".to_string()),
                source: crate::memory::MessageEventSource::Other("config".to_string()),
                channel: Some("daemon".to_string()),
                session_key: Some("config-generation".to_string()),
                parent_session_key: None,
                run_id: event.generation_id.map(|generation| format!("config:{}", generation.0)),
                parent_run_id: None,
                agent_id: Some("config-generation-manager".to_string()),
                persona_id: None,
                sender: Some("system".to_string()),
                recipient: Some("runtime".to_string()),
                role: "event".to_string(),
                event_type: event.event_type.clone(),
                subject: None,
                goal_id: None,
                causation_event_id: None,
                correlation_id: None,
                attempt_id: None,
                lease_epoch: None,
                config_generation_id: event.generation_id.map(|generation| generation.0),
                config_source_revision: source_revision,
                content: event.payload.to_string(),
                raw_payload_json,
                visibility: MemoryVisibility::Workspace,
            };
            if let Err(error) = memory.append_message_event(input).await {
                tracing::warn!(
                    event_type = %event.event_type,
                    "failed to append config generation audit event: {error}"
                );
            }
        });
    }
}

pub struct GatewayTurnRuntimeOwner {
    cleanup: Arc<turn_source::CleanupTracker>,
    current: Arc<ArcSwap<GatewayTurnState>>,
    config: crate::config::SharedConfig,
    memory: Arc<dyn Memory>,
    signal: Option<Arc<SignalChannel>>,
    whatsapp: Option<Arc<WhatsAppChannel>>,
    linq: Option<Arc<LinqChannel>>,
    nextcloud_talk: Option<Arc<NextcloudTalkChannel>>,
    hooks: Arc<HookManager>,
    agents: agent_lifecycle::AgentLifecycle,
    #[cfg(feature = "wasm-plugins")]
    plugin_runtime: Option<Arc<crate::plugins::PluginRuntime>>,
}

impl GatewayTurnRuntimeOwner {
    fn new(
        current: impl Into<GatewayTurnState>,
        config: crate::config::SharedConfig,
        memory: Arc<dyn Memory>,
        signal: Option<Arc<SignalChannel>>,
        whatsapp: Option<Arc<WhatsAppChannel>>,
        linq: Option<Arc<LinqChannel>>,
        nextcloud_talk: Option<Arc<NextcloudTalkChannel>>,
        hooks: Arc<HookManager>,
        #[cfg(feature = "wasm-plugins")] plugin_runtime: Option<Arc<crate::plugins::PluginRuntime>>,
    ) -> Self {
        // Startup sweep, before any target is published: a failure latches
        // every targeted resolve until a later reconcile succeeds.
        let agents = agent_lifecycle::AgentLifecycle::default();
        agents.reconcile(memory.as_ref(), &config.pin().effective, true);
        let current = current.into();
        Self {
            cleanup: Arc::clone(&current.publication_slot().cleanup),
            current: Arc::new(ArcSwap::from_pointee(current)),
            config,
            memory,
            signal,
            whatsapp,
            linq,
            nextcloud_talk,
            hooks,
            agents,
            #[cfg(feature = "wasm-plugins")]
            plugin_runtime,
        }
    }

    fn pin_state(&self) -> Arc<GatewayTurnState> {
        self.current.load_full()
    }

    /// Test fixtures' ready generation.
    #[cfg(test)]
    fn pin(&self) -> Arc<TurnRuntimeGeneration> {
        self.current
            .load()
            .ready()
            .expect("test: the gateway turn runtime is ready")
    }

    const fn agent_lifecycle(&self) -> &agent_lifecycle::AgentLifecycle {
        &self.agents
    }

    /// Whether the active generation was launched with a controlled H2 profile.
    ///
    /// The lifecycle experiment HTTP adapter uses this to refuse when the
    /// process has no profile. It does not mint an invocation.
    pub(crate) fn has_controlled_profile(&self) -> bool {
        self.current.load().publication_slot().controlled.is_some()
    }

    /// The runtime of `generation`. A ready runtime rejects a generation
    /// whose default provider has no usable credential (the previous
    /// generation stays active); a runtime standing by takes it and keeps
    /// standing by, so the agent and configuration routes keep applying.
    fn build(&self, generation: Arc<crate::config::ConfigGeneration>) -> anyhow::Result<GatewayTurnState> {
        let previous = self.current.load_full();
        if let Some(profile) = &previous.publication_slot().controlled {
            profile.revoke();
            anyhow::bail!("controlled Gateway generation cannot be rebuilt");
        }
        let chat = build_gateway_turn_provider(
            &generation.effective,
            &providers::provider_runtime_options_from_config(&generation.effective),
        );
        let chat = match (chat, previous.as_ref()) {
            (Ok(chat), _) => Ok(chat),
            (Err(error), GatewayTurnState::NotReady(_)) => {
                let reason = TurnNotReadyReason::of_provider_error(&error).ok_or(error)?;
                tracing::warn!(
                    reason = reason.code(),
                    "Gateway turn runtime stays not ready after a configuration change"
                );
                Err(reason)
            }
            (Err(error), GatewayTurnState::Ready(_)) => return Err(error),
        };
        let mut runtime = build_gateway_turn_state(
            generation,
            chat,
            Arc::clone(&self.config),
            Arc::clone(&self.memory),
            self.signal.clone(),
            self.whatsapp.clone(),
            self.linq.clone(),
            self.nextcloud_talk.clone(),
            Arc::clone(&self.hooks),
            #[cfg(feature = "wasm-plugins")]
            self.plugin_runtime.clone(),
        )?;
        let slot = runtime
            .publication_slot_mut()
            .ok_or_else(|| anyhow::anyhow!("new gateway turn runtime was unexpectedly shared"))?;
        Arc::get_mut(slot)
            .ok_or_else(|| anyhow::anyhow!("new gateway decision slot was unexpectedly shared"))?
            .cleanup = Arc::clone(&self.cleanup);
        Ok(runtime)
    }

    /// Retry a runtime standing by against the active generation. Storing a
    /// provider credential changes no configuration field, so the reload
    /// that follows publishes no generation and prepares no participant;
    /// the reload route calls this so that reload still makes the runtime
    /// ready. A runtime that is ready, or that another reload replaced in
    /// the meantime, is left as it is. `Ok(true)` when this call made the
    /// runtime ready.
    pub(crate) fn retry_standby(&self) -> anyhow::Result<bool> {
        let previous = self.current.load_full();
        if previous.ready().is_ok() {
            return Ok(false);
        }
        let next = self.build(self.config.pin())?;
        if next.ready().is_err() {
            return Ok(false);
        }
        let next = Arc::new(next);
        let swapped = self.current.compare_and_swap(&previous, Arc::clone(&next));
        if !Arc::ptr_eq(&swapped, &previous) {
            return Ok(false);
        }
        retire_replaced_mcp_servers(previous.mcp_tool(), next.mcp_tool());
        tracing::info!("Gateway turn runtime is ready: the default provider credential resolved");
        Ok(true)
    }

    fn synchronize_to_active(&self) -> anyhow::Result<()> {
        let active = self.config.pin();
        if self.current.load().config_generation().id == active.id {
            return Ok(());
        }
        self.current.store(Arc::new(self.build(active)?));
        Ok(())
    }
}

/// Start the Gateway's turn runtime from its startup generation: the owner
/// that rebuilds it on every configuration change, registered with the
/// manager and synchronized to the active generation. The returned owner is
/// the startup generation's only holder, so the first configuration change
/// releases it and ends its stdio MCP children, and a change of a server's
/// definition stops the process that ran the old one (D-098 F-1).
#[allow(clippy::too_many_arguments)]
fn start_gateway_turn_runtime(
    startup: impl Into<GatewayTurnState>,
    shared: &crate::config::SharedConfig,
    memory: Arc<dyn Memory>,
    signal: Option<Arc<SignalChannel>>,
    whatsapp: Option<Arc<WhatsAppChannel>>,
    linq: Option<Arc<LinqChannel>>,
    nextcloud_talk: Option<Arc<NextcloudTalkChannel>>,
    hooks: Arc<HookManager>,
    #[cfg(feature = "wasm-plugins")] plugin_runtime: Option<Arc<crate::plugins::PluginRuntime>>,
) -> anyhow::Result<Arc<GatewayTurnRuntimeOwner>> {
    let owner = Arc::new(GatewayTurnRuntimeOwner::new(
        startup,
        Arc::clone(shared),
        memory,
        signal,
        whatsapp,
        linq,
        nextcloud_talk,
        hooks,
        #[cfg(feature = "wasm-plugins")]
        plugin_runtime,
    ));
    let participant: Arc<dyn crate::config::ConfigGenerationParticipant> = owner.clone();
    shared.register_participant(&participant);
    owner.synchronize_to_active()?;
    Ok(owner)
}

struct PreparedGatewayTurnRuntime {
    current: Arc<ArcSwap<GatewayTurnState>>,
    previous: Arc<GatewayTurnState>,
    generation: GatewayTurnState,
    agents: agent_lifecycle::AgentReconcile,
}

impl crate::config::PreparedConfigGeneration for PreparedGatewayTurnRuntime {
    fn commit(&mut self) -> anyhow::Result<()> {
        // First, so a failed retire rejects the reload before it is published.
        self.agents.commit()?;
        self.current.store(Arc::new(self.generation.clone()));
        if self.previous.ready().is_err() && self.generation.ready().is_ok() {
            tracing::info!("Gateway turn runtime is ready: the default provider credential resolved");
        }
        retire_replaced_mcp_servers(self.previous.mcp_tool(), self.generation.mcp_tool());
        Ok(())
    }

    fn rollback(&mut self) {
        self.current.store(Arc::clone(&self.previous));
        self.agents.rollback();
    }

    fn finalize(&mut self) {
        self.agents
            .finalize(&self.generation.config_generation().source_revision.fingerprint_sha256);
    }
}

/// The turn runtime of `generation` when no rebuild-and-swap field changed:
/// the previous provider, tool surface and MCP tool (the same instances, so
/// no server is respawned) with a fresh decision slot, so the next
/// publication, and the process bindings it seals, come from `generation`.
/// A controlled profile is sealed against the digest of its whole
/// configuration and cannot follow a new generation, so the previous state
/// is kept as it was.
fn generation_only_swap(previous: &GatewayTurnState, generation: Arc<crate::config::ConfigGeneration>) -> GatewayTurnState {
    if previous.publication_slot().controlled.is_some() {
        tracing::warn!(
            "Controlled gateway profile keeps its configuration generation: the change is not followed by the turn runtime"
        );
        return previous.clone();
    }
    match previous {
        GatewayTurnState::Ready(runtime) => GatewayTurnState::from(TurnRuntimeGeneration {
            provider_binding: runtime.provider_binding.clone(),
            publication_slot: Arc::new(runtime.publication_slot.republished()),
            config_generation: generation,
            provider: Arc::clone(&runtime.provider),
            model: runtime.model.clone(),
            temperature: runtime.temperature,
            tools_registry: Arc::clone(&runtime.tools_registry),
            mcp_tool: runtime.mcp_tool.clone(),
        }),
        GatewayTurnState::NotReady(standby) => GatewayTurnState::NotReady(TurnRuntimeStandby {
            reason: standby.reason,
            publication_slot: Arc::new(standby.publication_slot.republished()),
            config_generation: generation,
            tools_registry: Arc::clone(&standby.tools_registry),
            mcp_tool: standby.mcp_tool.clone(),
        }),
    }
}

/// When a new generation replaces the MCP tool, stop the old tool's
/// servers that the new one no longer runs as they were launched (a
/// narrowed `mcp.workspace_servers`, a removed or disabled server, a changed
/// command, arguments, environment or working directory), so the process
/// running the old definition ends now instead of when the old tool is
/// dropped (D-095 M-2, D-097 M-3). Servers both generations run alike keep
/// their sessions until the old tool is dropped, which happens when the
/// last turn pinned to the old generation ends, so such a turn keeps
/// working. The stop runs on the runtime; without one it is logged and left
/// to the tool's drop.
fn retire_replaced_mcp_servers(previous: Option<&Arc<McpTool>>, next: Option<&Arc<McpTool>>) {
    let Some(previous) = previous.cloned() else {
        return;
    };
    if next.is_some_and(|next| Arc::ptr_eq(next, &previous)) {
        return;
    }
    let keep = next
        .map(|next| previous.servers_launched_alike(next))
        .unwrap_or_default();
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                let stopped = previous.retire_servers_except(&keep).await;
                if stopped > 0 {
                    tracing::info!(
                        stopped,
                        "Stopped MCP servers the new configuration no longer runs as launched"
                    );
                }
            });
        }
        Err(error) => tracing::warn!(
            error = %error,
            "No async runtime to stop the MCP servers the new configuration no longer runs"
        ),
    }
}

impl crate::config::ConfigGenerationParticipant for GatewayTurnRuntimeOwner {
    fn name(&self) -> &'static str {
        "gateway_turn_runtime"
    }

    fn supports_rebuild_field(&self, _field: &str) -> bool {
        true
    }

    /// Every published generation reaches the turn runtime: a process child
    /// is sealed against the generation of the turn that spawns it, so a
    /// restart-only or controlled-restart change (the adopted pairing tokens)
    /// must replace it too.
    fn prepares_for_field(&self, _field: &str) -> bool {
        true
    }

    fn prepare(
        &self,
        generation: Arc<crate::config::ConfigGeneration>,
        changed_fields: &[String],
    ) -> anyhow::Result<Box<dyn crate::config::PreparedConfigGeneration>> {
        let previous = self.current.load_full();
        let agents = agent_lifecycle::AgentReconcile::prepare(
            &self.agents,
            &self.memory,
            &previous.config_generation().effective,
            &generation.effective,
        );
        let runtime = if changed_fields.iter().all(|field| {
            crate::config::generation::is_process_restart_only(field)
                || crate::config::generation::is_controlled_restart(field)
        }) {
            generation_only_swap(&previous, generation)
        } else {
            self.build(generation)?
        };
        Ok(Box::new(PreparedGatewayTurnRuntime {
            current: Arc::clone(&self.current),
            previous,
            generation: runtime,
            agents,
        }))
    }
}

impl AppState {
    /// The current turn runtime, ready or standing by. Routes that read the
    /// tool surface or the configuration take this; routes that start or
    /// continue a turn take [`admit_turn_runtime`](Self::admit_turn_runtime).
    fn pin_turn_state(&self) -> Arc<GatewayTurnState> {
        if let Some(runtime) = &self.turn_runtime {
            return runtime.pin_state();
        }
        #[cfg(not(test))]
        {
            tracing::error!("production gateway state is missing its TurnRuntimeGeneration; aborting");
            std::process::abort();
        }
        #[cfg(test)]
        {
            Arc::new(GatewayTurnState::Ready(self.test_turn_runtime()))
        }
    }

    /// The admission check of every turn entry point: the ready generation
    /// the turn runs on, or the typed refusal it answers with.
    fn admit_turn_runtime(&self) -> Result<Arc<TurnRuntimeGeneration>, TurnNotReady> {
        self.pin_turn_state().ready()
    }

    /// Test fixtures' ready generation.
    #[cfg(test)]
    fn pin_turn_runtime(&self) -> Arc<TurnRuntimeGeneration> {
        self.turn_runtime
            .as_ref()
            .map_or_else(|| self.test_turn_runtime(), |runtime| runtime.pin())
    }

    #[cfg(test)]
    fn test_turn_runtime(&self) -> Arc<TurnRuntimeGeneration> {
        {
            let mut generation = TurnRuntimeGeneration {
                provider_binding: None,
                publication_slot: Arc::new(turn_source::publication_slot::GatewayPublicationSlot::default()),
                config_generation: self.config.pin(),
                provider: Arc::clone(&self.provider),
                model: self.model.clone(),
                temperature: self.temperature,
                tools_registry: Arc::clone(&self.tools_registry),
                mcp_tool: self.mcp_tool.clone(),
            };
            let config = &generation.config_generation.effective;
            if let Some(provider_name) = config.default_provider.as_deref() {
                let catalog = crate::providers::selection::catalog_for(config);
                if let Ok(resolved) = crate::providers::selection::check_with(
                    config,
                    &catalog,
                    provider_name,
                    &generation.model,
                    crate::providers::selection::credential_for(config, provider_name),
                    &crate::providers::catalog::Requirement::chat(),
                    crate::providers::catalog::UnknownPolicy::Invocation,
                ) {
                    generation.provider_binding = Some(Arc::new(turn_source::GatewayProviderBinding::new(
                        Arc::clone(&generation.provider),
                        resolved,
                    )));
                    let security = Arc::new(crate::security::SecurityPolicy::from_config(
                        &config.autonomy,
                        &config.workspace_dir,
                    ));
                    generation.publication_slot =
                        Arc::new(turn_source::publication_slot::GatewayPublicationSlot::new(security, 0));
                }
            }
            Arc::new(generation)
        }
    }
}

/// Build the gateway's per-turn channel routing registry and register the
/// channel-aware tools that complete its tool surface.
///
/// The gateway has two launch paths — [`build_gateway_turn_runtime`] (under the
/// daemon, and again on every hot reload) and [`run_gateway`] (`prx gateway`,
/// no daemon) — and each used to carry its own symmetric copy of this block.
/// Only the daemon copy had tests, so a change to one path silently gave the two
/// launch modes different tool surfaces. There is one body now, and
/// `both_gateway_launch_paths_publish_the_same_tool_surface` asserts the two
/// callers end up with the same tool names.
///
/// `message_send` is registered only when Signal is configured; `sessions_spawn`,
/// `sessions_list`, `sessions_history` and `config_reload` are registered on
/// every gateway. The two session read tools share `sessions_spawn`'s own
/// active-run registry (`SessionsSpawnTool::active_runs_arc`) and the gateway's
/// memory, so they read back the runs this gateway actually started rather than
/// a registry of their own; they are also the default-exposed MCP capabilities
/// `POST /mcp/v1/tools/call` resolves against this list (V02-R7). Returns the
/// routing registry (the caller publishes it for
/// `POST /api/channels/{name}/send`) and the `sessions_spawn` tools handle,
/// which the caller must fill with the final registry once it is sealed.
#[allow(clippy::too_many_arguments)]
fn register_gateway_turn_tools(
    tools_list: &mut Vec<Box<dyn Tool>>,
    config: &Config,
    shared_config: &crate::config::SharedConfig,
    security: &Arc<crate::security::SecurityPolicy>,
    hooks: &Arc<HookManager>,
    _model: &str,
    memory: &Arc<dyn Memory>,
    provider_runtime_options: &providers::ProviderRuntimeOptions,
    signal: Option<&Arc<SignalChannel>>,
    whatsapp: Option<&Arc<WhatsAppChannel>>,
    linq: Option<&Arc<LinqChannel>>,
    nextcloud_talk: Option<&Arc<NextcloudTalkChannel>>,
) -> anyhow::Result<(Arc<HashMap<String, Arc<dyn Channel>>>, Arc<tools::ParentToolRegistry>)> {
    // One routing registry, keyed by `Channel::name`, shared by `message_send`
    // (destination of an explicit `channel` argument) and `sessions_spawn`
    // (announce/kill routing) so both address exactly the same channel set.
    if shared_config.ensure_runtime_writable().is_err() {
        return Ok((Arc::new(HashMap::new()), Arc::new(tools::ParentToolRegistry::default())));
    }
    let mut registry: HashMap<String, Arc<dyn Channel>> = HashMap::new();
    if let Some(channel) = signal {
        registry.insert(channel.name().to_string(), Arc::clone(channel) as Arc<dyn Channel>);
    }
    if let Some(channel) = whatsapp {
        registry.insert(channel.name().to_string(), Arc::clone(channel) as Arc<dyn Channel>);
    }
    if let Some(channel) = linq {
        registry.insert(channel.name().to_string(), Arc::clone(channel) as Arc<dyn Channel>);
    }
    if let Some(channel) = nextcloud_talk {
        registry.insert(channel.name().to_string(), Arc::clone(channel) as Arc<dyn Channel>);
    }
    let channels_registry = Arc::new(registry);

    // `message_send` is backed by Signal when the channel is configured. Other
    // channels are addressed through the routing registry above.
    if let Some(signal_channel) = signal {
        tools_list.push(Box::new(
            tools::MessageSendTool::new_signal(Arc::clone(signal_channel), Arc::clone(security))
                .with_channels(Arc::clone(&channels_registry)),
        ));
    }

    // `sessions_spawn` is registered on every gateway, with or without Signal.
    // The channel argument is only the fallback announce destination, so a
    // missing IM channel is no reason to withhold the tool from a webhook turn.
    //
    // The per-turn announce/kill routing registry is the one built above: a
    // webhook turn stamps its originating channel into the tool's per-turn
    // scope, and the tool resolves the channel object from here at announce /
    // kill time, so a result is never mis-routed under concurrent webhooks.
    let spawn_tools_handle = {
        let announce_channel =
            gateway_spawn_announce_channel(signal.map(|channel| Arc::clone(channel) as Arc<dyn Channel>));
        let provider_name = providers::selection::default_provider(config).to_string();
        let spawn_tool = tools::SessionsSpawnTool::new(
            announce_channel,
            provider_name,
            providers::selection::default_model(config).to_string(),
            config.default_temperature,
            Arc::clone(security),
            config.workspace_dir.clone(),
            config.multimodal.clone(),
            config.agents_with_references().into_owned(),
            config.api_key.clone(),
            provider_runtime_options.clone(),
            config.sessions_spawn.clone(),
        )
        .with_configured_providers(&crate::providers::ConfiguredProviders::from_config(config))
        .with_model_catalog(providers::selection::catalog_for(config))
        .with_hint_config(config)
        .with_parent_hooks(Arc::clone(hooks))
        .with_channels(Arc::clone(&channels_registry))
        .with_shared_memory(Arc::clone(memory))
        .with_event_recording(config.memory.event_recording_config())
        .with_process_recovery(true)?;
        let handle = spawn_tool.tools_handle();

        // `sessions_list` / `sessions_history` read back what `sessions_spawn`
        // started, so they are bound to that tool's own run registry — the same
        // `Arc` it writes to — and to the gateway's memory, exactly as the
        // channel server wires them. A registry of their own would answer with
        // an empty list instead of a 404, which is a worse failure than the one
        // this registration fixes. Both are default-exposed MCP capabilities
        // (`default_mcp_server_exposed_tools`), and without this registration
        // `POST /mcp/v1/tools/call` answered 404 for a name
        // `GET /mcp/v1/tools/list` advertises (V02-R7). Pushed before
        // `spawn_tool` is moved into the list below.
        let active_runs = spawn_tool.active_runs_arc();
        let workspace_id = config.workspace_dir.to_string_lossy().to_string();
        tools_list.push(Box::new(
            tools::SessionsListTool::new(active_runs.clone())
                .with_shared_memory(Arc::clone(memory), workspace_id.clone()),
        ));
        tools_list.push(Box::new(
            tools::SessionsSendTool::with_security(Arc::clone(&active_runs), Arc::clone(security))
                .with_messaging(config)
                .with_shared_memory(Arc::clone(memory))
                .with_event_recording(config.memory.event_recording_config()),
        ));
        tools_list.push(Box::new(
            tools::SessionStatusTool::new(
                Arc::clone(&active_runs),
                providers::selection::default_provider(config),
                providers::selection::default_model(config),
                channels_registry.keys().cloned().collect(),
            )
            .with_shared_memory(Arc::clone(memory), workspace_id.clone()),
        ));
        tools_list.push(Box::new(
            tools::SessionsHistoryTool::new(active_runs).with_shared_memory(Arc::clone(memory), workspace_id),
        ));

        tools_list.push(Box::new(spawn_tool));
        handle
    };

    // The ConfigReloadTool and `/api/config/reload` route both submit candidates
    // to the same manager; no gateway path can publish configuration directly.
    tools_list.push(Box::new(tools::ConfigReloadTool::with_security(
        shared_config.clone(),
        Arc::clone(security),
    )));

    Ok((channels_registry, spawn_tools_handle))
}

/// A ready runtime of `generation`, or the error its default provider failed
/// with (test fixtures; production builds through [`build_gateway_turn_state`]).
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn build_gateway_turn_runtime(
    generation: Arc<crate::config::ConfigGeneration>,
    shared_config: crate::config::SharedConfig,
    memory: Arc<dyn Memory>,
    signal: Option<Arc<SignalChannel>>,
    whatsapp: Option<Arc<WhatsAppChannel>>,
    linq: Option<Arc<LinqChannel>>,
    nextcloud_talk: Option<Arc<NextcloudTalkChannel>>,
    hooks: Arc<HookManager>,
    #[cfg(feature = "wasm-plugins")] plugin_runtime: Option<Arc<crate::plugins::PluginRuntime>>,
) -> anyhow::Result<TurnRuntimeGeneration> {
    let config = generation.effective.as_ref();
    let chat = build_gateway_turn_provider(config, &providers::provider_runtime_options_from_config(config))?;
    let state = build_gateway_turn_state(
        generation,
        Ok(chat),
        shared_config,
        memory,
        signal,
        whatsapp,
        linq,
        nextcloud_talk,
        hooks,
        #[cfg(feature = "wasm-plugins")]
        plugin_runtime,
    )?;
    match state {
        GatewayTurnState::Ready(generation) => {
            Arc::try_unwrap(generation).map_err(|_| anyhow::anyhow!("new gateway turn runtime was unexpectedly shared"))
        }
        GatewayTurnState::NotReady(standby) => Err(TurnNotReady(standby.reason).into()),
    }
}

/// The runtime of `generation` around its already resolved default chat
/// connection, or a standby with the same tool surface when `chat` names
/// why there is none. The caller resolves the default before anything is
/// built, so a generation whose default model cannot chat is rejected first.
#[allow(clippy::too_many_arguments)]
fn build_gateway_turn_state(
    generation: Arc<crate::config::ConfigGeneration>,
    chat: Result<GatewayTurnChat, TurnNotReadyReason>,
    shared_config: crate::config::SharedConfig,
    memory: Arc<dyn Memory>,
    signal: Option<Arc<SignalChannel>>,
    whatsapp: Option<Arc<WhatsAppChannel>>,
    linq: Option<Arc<LinqChannel>>,
    nextcloud_talk: Option<Arc<NextcloudTalkChannel>>,
    hooks: Arc<HookManager>,
    #[cfg(feature = "wasm-plugins")] plugin_runtime: Option<Arc<crate::plugins::PluginRuntime>>,
) -> anyhow::Result<GatewayTurnState> {
    let config = generation.effective.as_ref();
    let provider_runtime_options = providers::provider_runtime_options_from_config(config);
    let security = crate::runtime::bootstrap::build_security_policy(config);
    let runtime: Arc<dyn runtime::RuntimeAdapter> = Arc::from(runtime::create_runtime(&config.runtime)?);
    let (composio_key, composio_entity_id) = if config.composio.configured() {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    };
    let extensions = hooks.control_tool_arcs(Arc::clone(&security));
    #[cfg(feature = "wasm-plugins")]
    let extensions = {
        let mut extensions = extensions;
        if let Some(runtime) = &plugin_runtime {
            extensions.extend(runtime.control_tool_arcs(Arc::clone(&security)));
        }
        extensions
    };
    let tools_result = tools::all_tools_with_runtime_ext_and_extensions(
        Arc::clone(&generation.effective),
        Arc::clone(&shared_config),
        &security,
        runtime,
        Arc::clone(&memory),
        composio_key,
        composio_entity_id,
        &config.browser,
        &config.http_request,
        &config.workspace_dir,
        &config.agents_with_references(),
        config.api_key.as_deref(),
        config,
        extensions,
        Some(Arc::clone(&hooks)),
    );
    let mut tools_list = tools_result.tools;
    let mcp_tool = tools_result.mcp_tool;

    let (_channels_registry, spawn_tools_handle) = register_gateway_turn_tools(
        &mut tools_list,
        config,
        &shared_config,
        &security,
        &hooks,
        chat.as_ref().map_or("", |(_, model, _)| model.as_str()),
        &memory,
        &provider_runtime_options,
        signal.as_ref(),
        whatsapp.as_ref(),
        linq.as_ref(),
        nextcloud_talk.as_ref(),
    )?;
    let tools_registry = Arc::new(tools_list);
    let _ = spawn_tools_handle.set(&tools_registry);

    let publication_slot = Arc::new(turn_source::publication_slot::GatewayPublicationSlot::new(
        Arc::clone(&security),
        {
            #[cfg(feature = "wasm-plugins")]
            {
                plugin_runtime.as_ref().map_or(0, |runtime| runtime.generation_id())
            }
            #[cfg(not(feature = "wasm-plugins"))]
            {
                0
            }
        },
    ));
    Ok(turn_state_of(
        chat,
        publication_slot,
        generation,
        tools_registry,
        mcp_tool,
    ))
}

/// The default chat connection of a ready generation: provider, model and
/// the binding that proves they are the resolved selection.
type GatewayTurnChat = (
    Arc<dyn Provider>,
    String,
    Option<Arc<turn_source::GatewayProviderBinding>>,
);

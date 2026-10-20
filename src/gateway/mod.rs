//! Axum-based HTTP gateway with proper HTTP/1.1 compliance and body limits.
//!
//! This module replaces the raw TCP implementation with axum for:
//! - Proper HTTP/1.1 parsing and compliance
//! - Content-Length validation (handled by hyper)
//! - Request body size limits (64KB max)
//! - Header sanitization (handled by axum/hyper)
//!
//! There is deliberately **no request timeout**. A gateway request can start an
//! agent turn, and an agent turn has no legitimate upper bound — research runs,
//! builds, and multi-hour tool chains are ordinary work here, not pathology. A
//! wall-clock cap on the outer request could only ever cut such a turn off
//! mid-flight, and it did: the former 60s `TimeoutLayer` sat *below* every inner
//! bound it was supposed to contain (single LLM call, compaction, sub-agent), so
//! it fired first and made failover and retry unreachable dead configuration.
//!
//! Work that can run long is therefore submitted as a detached job (see
//! [`jobs`]) rather than executed inside the request future, which decouples its
//! lifetime from the connection: a closed tab or a proxy hang-up no longer
//! destroys a turn halfway through its side effects. Visibility and termination
//! move to the operator instead of the clock — `prx tasks list` /
//! `prx tasks kill`, `GET /api/runtime/tasks`, and `GET /api/jobs`.

#![allow(clippy::print_stdout, clippy::print_stderr)]

mod api;
mod compat;
mod decision_cleanup;
mod decision_runtime;
pub(crate) mod decision_source;
mod jobs;

/// The reserved issuer component of every built-in (non-bearer) MCP origin.
///
/// Re-exported so `Config::validate` can refuse a `mcp_server.jwt_issuer` that
/// would collide with one of them inside the `:`-composed caller identity.
pub use compat::FIXED_EXTERNAL_ISSUERS;

mod ui;
mod webhook_dedup;

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
    /// Sole configuration generation owner used by every gateway path.
    pub config: crate::config::SharedConfig,
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
    provider_binding: Option<Arc<decision_source::GatewayProviderBinding>>,
    decision_slot: Arc<decision_runtime::GatewayDecisionSlot>,
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
    cleanup: Arc<decision_cleanup::CleanupTracker>,
    current: Arc<ArcSwap<TurnRuntimeGeneration>>,
    config: crate::config::SharedConfig,
    memory: Arc<dyn Memory>,
    signal: Option<Arc<SignalChannel>>,
    whatsapp: Option<Arc<WhatsAppChannel>>,
    linq: Option<Arc<LinqChannel>>,
    nextcloud_talk: Option<Arc<NextcloudTalkChannel>>,
    hooks: Arc<HookManager>,
    #[cfg(feature = "wasm-plugins")]
    plugin_runtime: Option<Arc<crate::plugins::PluginRuntime>>,
}

impl GatewayTurnRuntimeOwner {
    fn new(
        current: TurnRuntimeGeneration,
        config: crate::config::SharedConfig,
        memory: Arc<dyn Memory>,
        signal: Option<Arc<SignalChannel>>,
        whatsapp: Option<Arc<WhatsAppChannel>>,
        linq: Option<Arc<LinqChannel>>,
        nextcloud_talk: Option<Arc<NextcloudTalkChannel>>,
        hooks: Arc<HookManager>,
        #[cfg(feature = "wasm-plugins")] plugin_runtime: Option<Arc<crate::plugins::PluginRuntime>>,
    ) -> Self {
        Self {
            cleanup: Arc::clone(&current.decision_slot.cleanup),
            current: Arc::new(ArcSwap::from_pointee(current)),
            config,
            memory,
            signal,
            whatsapp,
            linq,
            nextcloud_talk,
            hooks,
            #[cfg(feature = "wasm-plugins")]
            plugin_runtime,
        }
    }

    fn pin(&self) -> Arc<TurnRuntimeGeneration> {
        self.current.load_full()
    }

    fn build(&self, generation: Arc<crate::config::ConfigGeneration>) -> anyhow::Result<TurnRuntimeGeneration> {
        let mut runtime = build_gateway_turn_runtime(
            generation,
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
        Arc::get_mut(&mut runtime.decision_slot)
            .ok_or_else(|| anyhow::anyhow!("new gateway decision slot was unexpectedly shared"))?
            .cleanup = Arc::clone(&self.cleanup);
        Ok(runtime)
    }

    fn synchronize_to_active(&self) -> anyhow::Result<()> {
        let active = self.config.pin();
        if self.current.load().config_generation.id == active.id {
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
    startup: TurnRuntimeGeneration,
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
    current: Arc<ArcSwap<TurnRuntimeGeneration>>,
    previous: Arc<TurnRuntimeGeneration>,
    generation: TurnRuntimeGeneration,
}

impl crate::config::PreparedConfigGeneration for PreparedGatewayTurnRuntime {
    fn commit(&mut self) -> anyhow::Result<()> {
        self.current.store(Arc::new(TurnRuntimeGeneration {
            provider_binding: self.generation.provider_binding.clone(),
            decision_slot: Arc::clone(&self.generation.decision_slot),
            config_generation: Arc::clone(&self.generation.config_generation),
            provider: Arc::clone(&self.generation.provider),
            model: self.generation.model.clone(),
            temperature: self.generation.temperature,
            tools_registry: Arc::clone(&self.generation.tools_registry),
            mcp_tool: self.generation.mcp_tool.clone(),
        }));
        retire_replaced_mcp_servers(self.previous.mcp_tool.as_ref(), self.generation.mcp_tool.as_ref());
        Ok(())
    }

    fn rollback(&mut self) {
        self.current.store(Arc::clone(&self.previous));
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

    fn prepares_for_field(&self, field: &str) -> bool {
        !crate::config::generation::is_process_restart_only(field)
            && !crate::config::generation::is_controlled_restart(field)
    }

    fn prepare(
        &self,
        generation: Arc<crate::config::ConfigGeneration>,
        _changed_fields: &[String],
    ) -> anyhow::Result<Box<dyn crate::config::PreparedConfigGeneration>> {
        let runtime = self.build(generation)?;
        Ok(Box::new(PreparedGatewayTurnRuntime {
            current: Arc::clone(&self.current),
            previous: self.current.load_full(),
            generation: runtime,
        }))
    }
}

impl AppState {
    fn pin_turn_runtime(&self) -> Arc<TurnRuntimeGeneration> {
        if let Some(runtime) = &self.turn_runtime {
            return runtime.pin();
        }
        #[cfg(not(test))]
        {
            tracing::error!("production gateway state is missing its TurnRuntimeGeneration; aborting");
            std::process::abort();
        }
        #[cfg(test)]
        Arc::new(TurnRuntimeGeneration {
            provider_binding: None,
            decision_slot: Arc::new(decision_runtime::GatewayDecisionSlot::default()),
            config_generation: self.config.pin(),
            provider: Arc::clone(&self.provider),
            model: self.model.clone(),
            temperature: self.temperature,
            tools_registry: Arc::clone(&self.tools_registry),
            mcp_tool: self.mcp_tool.clone(),
        })
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
        .with_channels(Arc::clone(&channels_registry))
        .with_shared_memory(Arc::clone(memory))
        .with_event_recording(config.memory.event_recording_config())
        .with_process_recovery(config.causal_tree.enabled)?;
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
    // The default is resolved before anything is built: a generation whose
    // default model cannot chat is rejected here, so the participant refuses
    // it and the previous generation stays active.
    let provider_runtime_options = providers::provider_runtime_options_from_config(config);
    let (provider, model, provider_binding) = build_gateway_turn_provider(config, &provider_runtime_options)?;
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
    #[allow(unused_mut)]
    let mut extensions = hooks.control_tool_arcs(Arc::clone(&security));
    #[cfg(feature = "wasm-plugins")]
    if let Some(runtime) = &plugin_runtime {
        extensions.extend(runtime.control_tool_arcs(Arc::clone(&security)));
    }
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
    );
    let mut tools_list = tools_result.tools;
    let mcp_tool = tools_result.mcp_tool;

    let (_channels_registry, spawn_tools_handle) = register_gateway_turn_tools(
        &mut tools_list,
        config,
        &shared_config,
        &security,
        &model,
        &memory,
        &provider_runtime_options,
        signal.as_ref(),
        whatsapp.as_ref(),
        linq.as_ref(),
        nextcloud_talk.as_ref(),
    )?;
    let tools_registry = Arc::new(tools_list);
    let _ = spawn_tools_handle.set(&tools_registry);

    let temperature = config.default_temperature;
    Ok(TurnRuntimeGeneration {
        provider_binding,
        decision_slot: Arc::new(decision_runtime::GatewayDecisionSlot::new(Arc::clone(&security), {
            #[cfg(feature = "wasm-plugins")]
            {
                plugin_runtime.as_ref().map_or(0, |runtime| runtime.generation_id())
            }
            #[cfg(not(feature = "wasm-plugins"))]
            {
                0
            }
        })),
        config_generation: generation,
        provider,
        model,
        temperature,
        tools_registry,
        mcp_tool,
    })
}

/// Build the pinned generation's default chat connection. The selected
/// provider, model and credential are resolved as one value before the exact
/// connection is constructed, including when the default names a route hint.
pub(crate) fn build_gateway_provider_selection(
    config: &Config,
    options: &providers::ProviderRuntimeOptions,
) -> anyhow::Result<(Arc<dyn Provider>, providers::catalog::ResolvedModel)> {
    let resolved = providers::selection::check_default(config, providers::catalog::UnknownPolicy::Invocation)?;
    let catalog = providers::selection::catalog_for(config);
    let provider: Arc<dyn Provider> = Arc::from(providers::create_routed_provider_for(
        &resolved, &catalog, config, options,
    )?);
    Ok((provider, resolved))
}

fn build_gateway_turn_provider(
    config: &Config,
    options: &providers::ProviderRuntimeOptions,
) -> anyhow::Result<(
    Arc<dyn Provider>,
    String,
    Option<Arc<decision_source::GatewayProviderBinding>>,
)> {
    let (provider, resolved) = build_gateway_provider_selection(config, options)?;
    let model = resolved.model().to_owned();
    let binding = Arc::new(decision_source::GatewayProviderBinding::new(
        Arc::clone(&provider),
        resolved,
    ));
    Ok((provider, model, Some(binding)))
}

/// Run the HTTP gateway using axum with proper HTTP/1.1 compliance.
///
/// `shared_config` is the single ConfigGeneration owner the gateway uses for all
/// authorization decisions. When the gateway runs under the daemon, the
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
        Duration::from_mins(1),
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
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let actual_port = listener.local_addr()?.port();
    let display_addr = format!("{host}:{actual_port}");

    // The chat-session mailbox is durable, and this is the one startup path
    // both launch modes share (`prx gateway`, and the daemon's supervised
    // gateway), so it is the only place the store has to be attached. Attaching
    // replays what the previous process left queued, and is idempotent: the
    // daemon's supervisor restarts the gateway inside one process, and a replay
    // never duplicates a session that is still live in memory.
    if let Some(path) = runtime::chat_sessions::attach_workspace_store(&config.workspace_dir) {
        tracing::debug!(path = %path.display(), "chat session mailbox is durable");
    }

    #[cfg(feature = "wasm-plugins")]
    let wasm_early_runtime = if config
        .default_provider
        .as_deref()
        .is_some_and(|provider| provider.starts_with("wasm:"))
        || memory::effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config))
            .starts_with("wasm:")
    {
        crate::plugins::init_plugin_runtime(&config, None).await
    } else {
        None
    };
    let provider_runtime_options = providers::provider_runtime_options_from_config(&config);
    let (provider, model, provider_binding) = build_gateway_turn_provider(&config, &provider_runtime_options)?;
    let temperature = config.default_temperature;
    let shared_config_for_reload = shared_config.unwrap_or_else(|| crate::config::new_shared(config.clone()));
    let mem: Arc<dyn Memory> = Arc::from(memory::create_memory_with_storage(
        &config.memory,
        Some(&config.storage.provider.config),
        &config.workspace_dir,
        memory::EmbeddingRootCredential::from_config(&config),
    )?);
    let runtime: Arc<dyn runtime::RuntimeAdapter> = Arc::from(runtime::create_runtime(&config.runtime)?);
    // FIX-P1-31: honour the configured `security.audit` block on the gate audit path.
    // Built via the shared `build_security_policy` helper so this site cannot drift
    // from (or forget) the audit-config wiring — byte-for-byte identical to the
    // former local from_config + audit-config construction.
    //
    // This policy bootstraps static gateway gates and the initial tool registry.
    // Gateway turn tools are rebuilt from one candidate ConfigGeneration and swapped
    // atomically by GatewayTurnRuntimeOwner. Resource-mutation routes independently
    // pin the manager's active generation at each authorization decision.
    let security = crate::runtime::bootstrap::build_security_policy(&config);
    // Workspace command hooks follow the live `security.workspace_hooks`
    // opt-in (HOOKS-ENV).
    let hooks =
        Arc::new(HookManager::new(config.workspace_dir.clone()).with_config(Arc::clone(&shared_config_for_reload)));
    // Kept for the shutdown drain of queued hook events.
    let hooks_for_shutdown = Arc::clone(&hooks);
    #[cfg(feature = "wasm-plugins")]
    let wasm_plugin_runtime = if let Some(runtime) = wasm_early_runtime {
        runtime
            .attach_memory(Arc::clone(&mem))
            .await
            .map_err(|error| anyhow::anyhow!("failed to attach WASM memory backend in gateway: {error}"))?;
        Some(runtime)
    } else {
        crate::plugins::init_plugin_runtime(&config, Some(Arc::clone(&mem))).await
    };

    let (composio_key, composio_entity_id) = if config.composio.configured() {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    };

    // Build the base tool list (mutable so we can append channel-aware tools below)
    #[allow(unused_mut)]
    let mut extensions = hooks.control_tool_arcs(Arc::clone(&security));
    #[cfg(feature = "wasm-plugins")]
    if let Some(runtime) = &wasm_plugin_runtime {
        extensions.extend(runtime.control_tool_arcs(Arc::clone(&security)));
    }
    let tools_result = tools::all_tools_with_runtime_ext_and_extensions(
        Arc::new(config.clone()),
        Arc::clone(&shared_config_for_reload),
        &security,
        runtime,
        Arc::clone(&mem),
        composio_key,
        composio_entity_id,
        &config.browser,
        &config.http_request,
        &config.workspace_dir,
        &config.agents_with_references(),
        config.api_key.as_deref(),
        &config,
        extensions,
    );
    let mut tools_list = tools_result.tools;
    let mcp_tool = tools_result.mcp_tool;
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
        Arc::new(
            SignalChannel::new_with_mode(
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
            )
            .with_artifact_owner(crate::media::MediaArtifactOwner::for_workspace(&config.workspace_dir)),
        )
    });

    // Linq channel (if configured). Built here (ahead of message_send and the spawn
    // tool) so it can join the shared per-turn channel routing registry below.
    let linq_channel: Option<Arc<LinqChannel>> = config.channels_config.linq.as_ref().map(|lq| {
        Arc::new(LinqChannel::new(
            lq.api_token.clone(),
            lq.from_phone.clone(),
            lq.allowed_senders.clone(),
        ))
    });

    // Nextcloud Talk channel (if configured). Built here (ahead of message_send and
    // the spawn tool) so it can join the shared per-turn channel routing registry.
    let nextcloud_talk_channel: Option<Arc<NextcloudTalkChannel>> =
        config.channels_config.nextcloud_talk.as_ref().map(|nc| {
            Arc::new(NextcloudTalkChannel::new(
                nc.base_url.clone(),
                nc.app_token.clone(),
                nc.allowed_users.clone(),
            ))
        });

    let (channels_registry, spawn_tools_handle) = register_gateway_turn_tools(
        &mut tools_list,
        &config,
        &shared_config_for_reload,
        &security,
        &model,
        &mem,
        &provider_runtime_options,
        signal_channel.as_ref(),
        whatsapp_channel.as_ref(),
        linq_channel.as_ref(),
        nextcloud_talk_channel.as_ref(),
    )?;
    // Publish the routing registry process-wide for as long as this gateway
    // runs, so `POST /api/channels/{name}/send` can address the channels the
    // gateway itself constructed. The channels supervisor publishes its own
    // (larger) registry alongside this one; the two are merged at lookup time.
    let _outbound_publication = crate::channels::outbound_registry::publish(Arc::clone(&channels_registry));
    #[cfg(feature = "wasm-plugins")]
    if let Some(runtime) = &wasm_plugin_runtime {
        tracing::debug!(generation = runtime.generation_id(), "WASM plugin runtime ready");
    }

    let tools_registry = Arc::new(tools_list);

    // Inject the tools registry into sessions_spawn so sub-agents can use tools.
    let _ = spawn_tools_handle.set(&tools_registry);

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
    // The startup registry and MCP tool move into the turn runtime: nothing
    // else keeps them, so the first configuration change releases them and
    // their stdio MCP children (D-098 F-1).
    let turn_runtime_owner = start_gateway_turn_runtime(
        TurnRuntimeGeneration {
            provider_binding,
            decision_slot: Arc::new(decision_runtime::GatewayDecisionSlot::new(Arc::clone(&security), {
                #[cfg(feature = "wasm-plugins")]
                {
                    wasm_plugin_runtime
                        .as_ref()
                        .map_or(0, |runtime| runtime.generation_id())
                }
                #[cfg(not(feature = "wasm-plugins"))]
                {
                    0
                }
            })),
            config_generation: shared_config_for_reload.pin(),
            provider: Arc::clone(&provider),
            model: model.clone(),
            temperature,
            tools_registry,
            mcp_tool,
        },
        &shared_config_for_reload,
        Arc::clone(&mem),
        signal_channel.clone(),
        whatsapp_channel.clone(),
        linq_channel.clone(),
        nextcloud_talk_channel.clone(),
        Arc::clone(&hooks),
        #[cfg(feature = "wasm-plugins")]
        wasm_plugin_runtime.clone(),
    )?;
    shared_config_for_reload.register_audit_sink(Arc::new(ConfigGenerationMessageAuditSink {
        memory: Arc::clone(&mem),
        workspace_id: config.workspace_dir.to_string_lossy().to_string(),
        runtime_handle: tokio::runtime::Handle::current(),
    }));

    let state = AppState {
        config: Arc::clone(&shared_config_for_reload),
        provider,
        model,
        temperature,
        mem,
        auto_save: config.memory.auto_save && config.memory.semantic.auto_promote_user_messages,
        // Test fixtures only; this state serves every turn from `turn_runtime`.
        #[cfg(test)]
        tools_registry: Arc::new(Vec::new()),
        #[cfg(test)]
        mcp_tool: None,
        turn_runtime: Some(Arc::clone(&turn_runtime_owner)),
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
        plugin_runtime: wasm_plugin_runtime,
    };

    // HookManager resolves hooks and event delivery from this same generation owner.
    #[cfg(feature = "wasm-plugins")]
    if let Some(ref runtime) = state.plugin_runtime {
        state.hooks.set_plugin_runtime(Arc::clone(runtime)).await;
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
        .with_state(state);

    // The listener is bound and every route/state dependency is now constructed.
    // This explicit acknowledgement, rather than supervisor task survival, is the
    // gateway readiness boundary.
    crate::health::mark_component_ok("gateway");

    // A quiet listener is still a live listener. Refresh gateway health from an
    // owner-controlled heartbeat rather than relying on `/health` requests to
    // keep the required component fresh.
    let health_shutdown = CancellationToken::new();
    let health_heartbeat = tokio::spawn(run_health_heartbeat(
        "gateway",
        GATEWAY_HEALTH_HEARTBEAT_INTERVAL,
        health_shutdown.clone(),
    ));

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
    let serve_result = tokio::select! {
        res = &mut serve_fut => res,
        _ = async {
            drain_deadline.cancelled().await;
            tokio::time::sleep(GATEWAY_GRACEFUL_TIMEOUT).await;
        } => {
            tracing::warn!(
                timeout_secs = GATEWAY_GRACEFUL_TIMEOUT.as_secs(),
                "gateway graceful shutdown timed out; forcing exit"
            );
            Ok(())
        }
    };

    let decision_cleanup_result = turn_runtime_owner
        .cleanup
        .drain(tokio::time::Instant::now() + Duration::from_secs(15))
        .await;
    if !hooks_for_shutdown.drain(crate::hooks::HOOK_DRAIN_BOUND).await {
        tracing::warn!(
            bound_secs = crate::hooks::HOOK_DRAIN_BOUND.as_secs(),
            "hook events were still queued at shutdown; the rest are abandoned"
        );
    }
    health_shutdown.cancel();
    let _ = health_heartbeat.await;
    serve_result?;
    decision_cleanup_result?;

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
    // Merge the PrometheusObserver's main registry with the separate
    // chat_metrics registry (S2.5 P1-A). The four chat counters live in a
    // registry that is physically isolated from the observer, so they have to
    // be merged explicitly or a scrape never sees them.
    let chat_reg = crate::observability::chat_metrics::chat_registry();
    let body = state
        .observer
        .as_ref()
        .as_any()
        .downcast_ref::<crate::observability::PrometheusObserver>()
        .map_or_else(
            || {
                // The chat metrics still have to be exposed when the observer is not Prometheus.
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
            if let Err(err) = persist_pairing_tokens(&state.config, &state.pairing).await {
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

/// Add what pairing learned to the configured list, **without rewriting the
/// entries an operator authored**.
///
/// Persistence used to assign `pairing.tokens()` over the whole list, which
/// treated the in-memory set as authoritative over the file. It is not:
/// `gateway.paired_tokens` is a restart-only field, so between startup and the
/// next restart the file can legitimately hold entries the running guard never
/// loaded, and it can hold them in a *spelling* the guard cannot reproduce.
/// Both were destroyed silently.
///
/// The spelling matters because [`PairingGuard`] stores only hashes: a
/// plaintext entry is hashed on load, so `tokens()` returns a hash for it and
/// the assignment wrote that hash back in its place. The token stays valid as a
/// bearer either way — but chat's `/sessions --daemon` takes its credential out
/// of this list and deliberately skips hashes (a stored hash, re-hashed by the
/// gateway on receipt, authenticates as nothing), so the entry it was reading
/// would have become useless to it with no edit by anyone and nothing in the
/// file to show what changed.
///
/// That particular sequence turns out not to be reachable through `/pair`:
/// a pairing code is minted only when the token set is *empty*, so a config
/// that already carries a plaintext entry can never complete a pairing. What is
/// reachable is the same destruction one step earlier — an operator adds an
/// entry by hand while the gateway is up and a still-live code is later
/// redeemed, and the entry is dropped from disk outright rather than merely
/// rehashed.
///
/// So nothing is rewritten or removed here. Every non-empty configured entry is
/// kept, in order and as written, and a live hash is appended only when no
/// configured entry already names that token. Revocation is not a thing this
/// gateway has, so there is nothing this would wrongly resurrect.
///
/// This never puts a *generated* token into the file in plaintext: `/pair`
/// hands that to the client once and persists only its hash, unchanged. The
/// only plaintext preserved is plaintext the operator wrote themselves.
///
/// MUTATION GUARD: assign `pairing.tokens()` wholesale again and
/// `a_configured_token_survives_a_later_pairing` fails.
fn merge_paired_tokens(configured: &[String], live: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = Vec::with_capacity(configured.len().saturating_add(live.len()));
    let mut covered: HashSet<String> = HashSet::new();
    for entry in configured {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Compared by hash, kept by spelling: a plaintext entry and the hash
        // pairing holds for it are the same credential, and the operator's
        // spelling is the one that stays.
        let hash = if crate::security::pairing::is_token_hash(trimmed) {
            trimmed.to_string()
        } else {
            crate::security::pairing::hash_token(trimmed)
        };
        if covered.insert(hash) {
            merged.push(trimmed.to_string());
        }
    }
    for token in live {
        if covered.insert(token.clone()) {
            merged.push(token.clone());
        }
    }
    merged
}

async fn persist_pairing_tokens(config: &crate::config::SharedConfig, pairing: &PairingGuard) -> Result<()> {
    let paired_tokens = pairing.tokens();
    let mut updated_cfg = (*config.desired().config).clone();
    updated_cfg.gateway.paired_tokens = merge_paired_tokens(&updated_cfg.gateway.paired_tokens, &paired_tokens);
    updated_cfg
        .save()
        .await
        .context("Failed to persist paired tokens to config.toml")?;
    let manager = Arc::clone(config);
    crate::runtime::blocking::spawn_blocking(move || {
        manager.reload_from_disk(crate::config::ConfigReloadTrigger::PairingPersistence)
    })
    .await
    .context("Pairing config reload worker failed")??;
    Ok(())
}

/// Build the turn-root spawn context for one gateway/webhook turn.
///
/// Two things travel down from here into every sub-agent this turn starts:
///
/// * `run_id` — the turn's own id becomes the child's `parent_run_id`, so a
///   webhook-launched run is attached to the turn that asked for it instead of
///   floating as a root.
/// * `channel_excluded_tools` — the names the turn's own surface hides. The
///   gateway is not an IM channel and has no `channel_exclude` list: it publishes
///   `tool_tiering` as configured, not `for_channel_surface()`. So the boundary a
///   child inherits here is exactly `always_exclude`: the child's surface is the
///   gateway's own surface minus the names the operator hid from the gateway, and
///   `spawn` is not a way to get one of them back. `always_include` is untouched,
///   exactly as on the channel path.
fn gateway_turn_spawn_context(
    run_id: String,
    fabric_ctx: &GatewayFabricContext,
    tiering: &crate::config::ToolTieringConfig,
) -> crate::tools::sessions_spawn::SpawnExecutionContext {
    crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
        run_id,
        format!(
            "{}:{}:{}",
            fabric_ctx.channel, fabric_ctx.session_key, fabric_ctx.sender
        ),
    )
    .with_channel_excluded_tools(tiering.always_exclude.clone())
}

/// Run `inner` as the webhook turn itself: seeded with this turn's spawn
/// boundary, so anything it spawns inherits the gateway's own surface.
///
/// This is the production seam. `gateway_turn_spawn_context` only builds the
/// context; nothing enforced that the value actually reached the tool loop, so
/// emptying the list at the call site (or shadowing the binding) left every
/// test green while `sessions_spawn` handed a child the tools the operator hid
/// from the gateway. Binding construction and scoping together here gives that
/// wiring one place to be tested, and
/// `tests/architecture_boundaries.rs::the_gateway_webhook_turn_seeds_its_spawn_boundary`
/// pins that `run_gateway_chat_with_multimodal` still goes through it.
async fn within_gateway_turn_spawn_context<F>(
    run_id: String,
    fabric_ctx: &GatewayFabricContext,
    tiering: &crate::config::ToolTieringConfig,
    inner: F,
) -> F::Output
where
    F: std::future::Future,
{
    let turn_spawn_ctx = gateway_turn_spawn_context(run_id, fabric_ctx, tiering);
    crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
        .scope(turn_spawn_ctx, inner)
        .await
}

async fn run_gateway_chat_with_multimodal(
    state: &AppState,
    provider_label: &str,
    message: &str,
    fabric_ctx: &GatewayFabricContext,
) -> anyhow::Result<String> {
    let turn_runtime = state.pin_turn_runtime();
    turn_runtime
        .provider_binding
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("gateway provider binding is missing"))?
        .verify(&turn_runtime)?;
    let provider_label = turn_runtime
        .provider_binding
        .as_ref()
        .map_or(provider_label, |binding| binding.provider_name());
    let config_generation = Arc::clone(&turn_runtime.config_generation);
    let config_snapshot = Arc::clone(&config_generation.effective);
    let workspace_id = config_generation.effective.workspace_dir.to_string_lossy().to_string();
    let event_recording = config_generation.effective.memory.event_recording_config();
    let fabric = MemoryFabric::new(state.mem.clone(), workspace_id.clone()).with_event_recording(event_recording);
    let run_id = Uuid::new_v4().to_string();
    // D4 C4: migrate the gateway fabric durable session_key to the recipient-aware
    // canonical (`gateway:{channel}:{sender}:{recipient}`) while carrying the
    // pre-cutover legacy key (`gateway:webhook:{target}` / `gateway:{ch}:{sender}`)
    // for read-merge, so existing legacy history stays visible under the new key.
    let webhook_owner_id = fabric_ctx.webhook_owner_id(&workspace_id);
    // D-025: a `/webhook` turn's durable key is its credential-qualified
    // `gateway:webhook:{credential}:{reply_target}` itself, with no legacy key:
    // history under the old `gateway:webhook:{reply_target}` key cannot be
    // attributed to a credential, so it is neither migrated nor read-merged, and
    // continuity restarts once. Platform channel messages keep the canonical
    // key and their legacy read-merge.
    let (canonical_session_key, legacy_session_key) = if fabric_ctx.credential.is_some() {
        (fabric_ctx.session_key.clone(), None)
    } else {
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
        (canonical_session_key, Some(legacy_session_key))
    };
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
    .with_config_generation(&config_generation);
    let runtime_envelope = match legacy_session_key {
        Some(legacy_session_key) => runtime_envelope.with_legacy_session_key(legacy_session_key),
        None => runtime_envelope,
    };
    let runtime_envelope = match webhook_owner_id.as_deref() {
        Some(owner_id) => runtime_envelope.with_owner_id(owner_id),
        None => runtime_envelope,
    };
    let base_scope = runtime_envelope.message_scope();
    authorize_gateway_config_resource_mutation(
        &config_snapshot,
        "gateway:webhook:message_event:user",
        ResourceRiskLevel::Low,
    )
    .map_err(|(_, body)| {
        let error = body
            .0
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or("gateway resource mutation denied")
            .to_string();
        anyhow::anyhow!(error)
    })?;
    let cancellation = jobs::current_cancellation().map_or_else(CancellationToken::new, |token| token.child_token());
    let _turn_cancel_guard = cancellation.clone().drop_guard();
    let pending_input = decision_source::GatewayPendingInput {
        generation: Arc::clone(&config_generation),
        fabric: fabric.clone(),
        envelope: runtime_envelope,
        ticket: turn_runtime.decision_slot.cleanup.register(cancellation.clone())?,
        acknowledgement: jobs::register_terminal_cleanup(),
    };
    let (source_receipt, gateway_user_event, mut admission_guard) = pending_input
        .record(message.to_owned(), fabric_ctx.idempotency_key.clone())
        .await?;
    let runtime_envelope = source_receipt.envelope().clone();
    let gateway_proof = source_receipt.bind(&turn_runtime, fabric_ctx)?;
    // A spawned webhook job cannot inherit the caller's cleanup task-local.
    // Prediction and later work must retain the same concrete cleanup owner.
    let child_cleanup = match crate::runtime::turn_cleanup::CleanupScope::new(
        Some(&turn_runtime.decision_slot.cleanup),
        cancellation.clone(),
    ) {
        Ok(scope) => scope.with_acknowledgement(jobs::register_terminal_cleanup()),
        Err(error) => {
            if let Err(record_error) = admission_guard
                .finish_failure(
                    cancellation.is_cancelled(),
                    &format!("decision cleanup admission failed: {error}"),
                )
                .await
            {
                return Err(error.context(format!(
                    "Gateway admission failure persistence is unconfirmed: {record_error}"
                )));
            }
            return Err(error);
        }
    };
    let admission = child_cleanup
        .scope_admission(Box::pin(async {
            let service = turn_runtime.decision_slot.publish(&turn_runtime, state).await?;
            let context = service
                .decide_agent_initial(
                    &gateway_proof,
                    crate::runtime::turn_decision::AgentTurnKind::SingleShot,
                    &cancellation,
                )
                .await?;
            Ok::<_, anyhow::Error>((service, context))
        }))
        .await;
    let (service, context) = match admission {
        Ok(admitted) => admitted,
        Err(error) => {
            // Admission is no longer polled. Settle its owned accounting work
            // before acknowledging the persisted input's failure.
            let admission_error = format!("{error:#}");
            let error = match child_cleanup.run(Box::pin(async { Err::<(), _>(error) })).await {
                Err(error) if error.is::<crate::runtime::turn_cleanup::CleanupIncomplete>() => {
                    let context = format!(
                        "decision admission failed: {admission_error}; Gateway admission cleanup is unconfirmed: {error:#}"
                    );
                    error.context(context)
                }
                Err(error) => error,
                Ok(()) => anyhow::anyhow!(admission_error),
            };
            if let Err(record_error) = admission_guard
                .finish_failure(
                    cancellation.is_cancelled(),
                    &format!("decision admission failed: {error}"),
                )
                .await
            {
                return Err(error.context(format!(
                    "Gateway admission failure persistence is unconfirmed: {record_error}"
                )));
            }
            return Err(error);
        }
    };
    let (cleanup_ticket, admission_acknowledgement) = admission_guard.into_terminal_parts()?;
    let settlement = service.terminal_settlement_ref(Arc::clone(&context))?;
    let terminal_guard = decision_runtime::GatewayTerminalGuard::new(
        cleanup_ticket,
        Arc::clone(service),
        Arc::clone(&context),
        fabric.clone(),
        Arc::clone(&config_snapshot),
        runtime_envelope.clone(),
        admission_acknowledgement,
    )?;
    let turn_result: anyhow::Result<String> = child_cleanup
        .run(async {
            anyhow::ensure!(
                !matches!(
                    context.disposition()?,
                    crate::runtime::turn_decision::TurnDisposition::CollectApprovalArguments
                ),
                "ingress has no approval collection adapter"
            );
            if let crate::runtime::turn_decision::TurnDisposition::Close { reply } = context.disposition()? {
                let ports = decision_runtime::GatewayDecisionClose {
                    resource: "gateway:webhook:message_event:assistant",
                    history_commit_len: 2,
                    terminal_guard: &terminal_guard,
                    service,
                    config: &config_snapshot,
                    envelope: &runtime_envelope,
                    started_at: chrono::Utc::now(),
                };
                service
                    .close_agent_directive_turn(&gateway_proof, Arc::clone(&context), &ports, &cancellation)
                    .await?;
                return Ok(reply.into_owned());
            }
            let selected_context = crate::agent::memory_loader::format_selected_context(None, context.retrieved()?)?;
            let semantic_scope = runtime_envelope.memory_write_context(if fabric_ctx.channel == "webhook" {
                "webhook"
            } else {
                "private"
            });
            let recalled_context = crate::agent::context_recall::build_context_with_shared_events_and_scope(
                state.mem.as_ref(),
                runtime_envelope.memory_principal(),
                message,
                config_snapshot.memory.min_relevance_score,
                Some(&semantic_scope),
            )
            .await;
            let mem_context = crate::agent::context_recall::RecalledMemoryContext {
                preamble: format!("{selected_context}{}", recalled_context.preamble),
                ids: recalled_context.ids,
            };
            let enriched_message = if mem_context.preamble.is_empty() {
                message.to_string()
            } else {
                format!("{}{}", mem_context.preamble, message)
            };

            let user_messages = vec![crate::agent::turn_contracts::inbound_user_message(&enriched_message)];
            let image_marker_count = crate::multimodal::count_image_markers(&user_messages);
            let adapter_capabilities = turn_runtime.provider.capabilities_for(
                &turn_runtime.model,
                crate::providers::traits::ProviderRequestMode::NonStreaming,
            );
            let mode_capabilities = providers::selection::catalog_for(&config_snapshot)
                .effective_provider_capabilities(provider_label, &turn_runtime.model, adapter_capabilities);
            if image_marker_count > 0 && !mode_capabilities.vision {
                return Err(ProviderCapabilityError {
                    provider: providers::redact_endpoint_userinfo(provider_label).into_owned(),
                    capability: "vision".to_string(),
                    message: format!(
                        "received {image_marker_count} image marker(s), but this provider does not support vision input"
                    ),
                }
                .into());
            }

            // Build system prompt with native_tools flag so the prompt instructs the
            // LLM to use tools rather than emit XML tags.
            let multimodal_config = config_snapshot.multimodal.clone();
            let native_tools = mode_capabilities.native_tool_calling;
            let selected_skills = service.agent_prompt_skills(&gateway_proof, &context)?;
            // The runtime prompt wrapper owns the whole prompt policy — the
            // `skill_read` gate on the skill list and the `[agent] compact_context`
            // bootstrap cap — so the webhook builds its prompt exactly as CLI, chat and
            // Console sessions do. It used to call the canonical builder directly with
            // a hardcoded `bootstrap_max_chars = None` (E3, webhook half).
            let system_prompt = crate::agent::runtime_prompt::build_runtime_system_prompt(
                &config_snapshot,
                &turn_runtime.model,
                &selected_skills,
                native_tools,
            );

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
            let gateway_scope_ctx = crate::agent::turn_contracts::ScopeContext {
                policy: &gateway_policy,
                sender: fabric_ctx.sender.as_str(),
                channel: fabric_ctx.channel.as_str(),
                // Gateway/webhook turns are 1:1 (no group semantics); use a stable
                // chat_type so scope rules can target the gateway surface.
                chat_type: "gateway",
                chat_id: fabric_ctx.session_key.as_str(),
                session_key: Some(runtime_envelope.session_key.as_str()),
                // R-MCPB F-2: the credential-bound owner, so run views and owner-scoped
                // rows follow the authenticated caller rather than its `reply_target`.
                owner_id: webhook_owner_id.as_deref(),
                owner_isolated: runtime_envelope.owner_isolated,
                topic_id: None,
                task_id: None,
                source_message_event_id: None,
                config_generation_id: runtime_envelope.config_generation_id,
                config_source_revision: runtime_envelope.config_source_revision.as_deref(),
            };

            // OQ-23: the webhook turn runs the same bounded-request contract as every
            // other execution surface — pre-turn flush/paging, the preflight before
            // each provider call (with the published tool schemas reserved) and the
            // reserved mid-turn trim. The window is the *effective* one, which applies
            // the operator/discovery/catalog limits of the model this turn actually serves, as the
            // CLI and TUI do.
            let mut effective_compaction = service.effective_compaction(provider_label, &turn_runtime.model);
            effective_compaction.config = context.main_compaction().clone();
            crate::router::context::trace_effective_compaction_resolution(&effective_compaction);

            let provider_started_at = chrono::Utc::now();
            let route_decision = crate::llm::route_decision::RouteDecision::single_candidate_for_context(
                provider_label,
                turn_runtime.model.clone(),
                runtime_envelope.resolved_owner_id(),
                runtime_envelope.session_key.clone(),
                runtime_envelope.source_message_event_id.clone(),
                None,
                "gateway_webhook",
                u32::try_from(message.chars().count() / 4).unwrap_or(u32::MAX),
                !turn_runtime.tools_registry.is_empty(),
                false,
            );
            // R3: name this turn's real origin before any tool runs. The gateway owns no
            // channel of its own — `message_send` falls back to whichever handle the
            // daemon registered — so without this every webhook turn's outbound scope
            // reads as that handle's channel and a rule written for the surface the
            // message actually came from can never match. Reply routing is untouched;
            // only rule matching changes.
            //
            // G2: the same turn also seeds the spawn execution context. A sub-agent the
            // webhook turn starts inherits (a) this turn's run_id as its parent, and (b)
            // the gateway surface's own hidden tool names. The gateway is not an IM
            // channel, so it has no `channel_exclude` list of its own: it publishes
            // `config_snapshot.tool_tiering` rather than `for_channel_surface()`. The
            // boundary a child must therefore inherit is exactly `always_exclude` — the
            // gateway's own tool set minus what the operator hid from it — so a spawn can
            // never be a way to get back a tool the gateway itself may not call.
            let loop_result = within_gateway_turn_spawn_context(
                runtime_envelope
                    .run_id
                    .clone()
                    .unwrap_or_else(|| Uuid::new_v4().to_string()),
                fabric_ctx,
                &config_snapshot.tool_tiering,
                crate::tools::message_send::with_outbound_origin_channel(fabric_ctx.channel.as_str(), async {
                    run_turn(
                        &mut history,
                        crate::agent::turn_request::TurnRequest {
                            // no approval manager
                            channel_name: "webhook",
                            // no streaming delta sender
                            scope: Some(&gateway_scope_ctx),
                            mode: crate::agent::turn_contracts::ChatMode::default(),
                            silent: true,
                            expose_stay_silent: false,
                            multimodal: &multimodal_config,
                            schedule: crate::agent::turn_request::TurnSchedule {
                                read_only_tool_concurrency_window: config_snapshot
                                    .agent
                                    .read_only_tool_concurrency_window,
                                priority_scheduling_enabled: config_snapshot.agent.priority_scheduling_enabled,
                                low_priority_tool_names: config_snapshot.agent.low_priority_tools.clone(),
                            },
                            compaction: Some(&effective_compaction.config),
                            // no tool call notifications
                            decision: crate::agent::turn_request::TurnDecision {
                                service,
                                context: &context,
                                proof: &gateway_proof,
                            },
                        },
                        crate::agent::turn_request::TurnServices {
                            provider: turn_runtime.provider.as_ref(),
                            provider_name: provider_label,
                            model: &turn_runtime.model,
                            temperature: turn_runtime.temperature,
                            tools_registry: Arc::clone(&turn_runtime.tools_registry),
                            observer: &noop_observer,
                            hooks: state.hooks.as_ref(),
                            // silent
                            approval: None,
                            approval_resolver: None,
                            runtime_adapter: Some({
                                crate::agent::turn_contracts::ToolLoopRuntimeAdapter {
                                    provider_trace: None,
                                    events: None,
                                    stream_provider: false,
                                    tool_execution_service: None,
                                    tool_execution_context: crate::tools::ToolExecutionContext::new(
                                        runtime_envelope.clone(),
                                        if fabric_ctx.channel == "webhook" {
                                            "webhook"
                                        } else {
                                            "private"
                                        },
                                    )
                                    .with_chat_id(&runtime_envelope.session_key),
                                    allowed_tool_names: None,
                                }
                            }),
                            memory: crate::agent::turn_request::ToolLoopMemory::new(
                                &state.mem,
                                &config_snapshot.workspace_dir,
                                Some(DocumentIngestRuntime::from_envelope(
                                    state.mem.clone(),
                                    &runtime_envelope,
                                )),
                            )
                            .with_event_fabric(
                                MemoryFabric::new(state.mem.clone(), runtime_envelope.workspace_id.clone())
                                    .with_event_recording(config_snapshot.memory.event_recording_config()),
                            )
                            .with_request_event_scope({
                                let mut scope = base_scope.clone();
                                scope.causation_event_id =
                                    gateway_user_event.as_ref().map(|event| event.event_id.clone());
                                scope
                            }),
                        },
                        crate::agent::turn_request::TurnDelivery {
                            cancellation: Some(cancellation.clone()),
                            // no cancellation token
                            on_delta: None,
                            // P1-a: gateway turns now route through decide()
                            on_tool_call: None,
                        },
                    )
                    .await
                    .map(|(outcome, trace)| (outcome.into_text(), trace))
                }),
            )
            .await;
            let (response, trace) = match loop_result {
                Ok(result) => result,
                Err(error) => {
                    let cancelled =
                        cancellation.is_cancelled() || crate::agent::turn_contracts::is_tool_loop_cancelled(&error);
                    let terminal_id = runtime_envelope
                        .run_id
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("gateway source run id is missing"))?;
                    if let Err(finalize_error) = decision_runtime::commit_terminal(
                        Some(&terminal_guard),
                        &fabric,
                        crate::agent::terminal::TurnTerminalDraft {
                            decision: Some(settlement.clone()),
                            terminal_id,
                            scope: runtime_envelope.message_scope(),
                            status: if cancelled {
                                crate::agent::terminal::TurnTerminalStatus::Cancelled
                            } else {
                                crate::agent::terminal::TurnTerminalStatus::Failed
                            },
                            history: None,
                            history_scope: None,
                            provider_outcome: None,
                            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                                summary: error.to_string(),
                                started_at: provider_started_at,
                                finished_at: chrono::Utc::now(),
                            },
                            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
                        },
                        &config_snapshot,
                    )
                    .await
                    {
                        return Err(finalize_error);
                    }
                    return Err(error);
                }
            };
            anyhow::ensure!(
                trace.execution_trace.as_ref() == Some(&context.terminal_execution_trace()),
                "gateway returned execution trace differs from its owning decision"
            );

            authorize_gateway_config_resource_mutation(
                &config_snapshot,
                "gateway:webhook:message_event:assistant",
                ResourceRiskLevel::Low,
            )
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
            if let Err(error) = decision_runtime::commit_terminal(
                Some(&terminal_guard),
                &fabric,
                crate::agent::terminal::TurnTerminalDraft {
                    decision: Some(settlement.clone()),
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
                            .with_sender(format!("{provider_label}/{}", turn_runtime.model))
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
                &config_snapshot,
            )
            .await
            {
                return Err(error);
            }
            Ok(response)
        })
        .await;
    if let Err(error) = &turn_result {
        terminal_guard
            .finish_failure(error, cancellation.is_cancelled())
            .await?;
    }
    turn_result
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
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let async_mode = jobs::wants_async(uri.query(), &headers);
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
    // The authenticated credential this request presented, in two forms: the
    // idempotency scope (process-local; hashed again into every stored
    // idempotency key) and the owner credential of the webhook turn
    // (`GatewayFabricContext::webhook_owner_id`, R-MCPB F-2), whose
    // domain-separated digest is what reaches owner ids, session keys and logs
    // (R-MCPBfg N-4). Both follow the same precedence.
    let owner_credential = if state.pairing.require_pairing() {
        webhook_owner_credential("bearer", bearer_token)
    } else if let (Some(_), Some(token)) = (&state.webhook_token_hash, webhook_token_header) {
        webhook_owner_credential("token", token)
    } else if let Some(signing_secret) = state.webhook_signing_secret.as_deref() {
        webhook_owner_credential("signing-secret", signing_secret)
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
    let request_idempotency_key = match idempotency_key_from_header(&headers, "X-Idempotency-Key") {
        Ok(key) => key,
        Err(message) => {
            let err = serde_json::json!({ "error": message });
            return (StatusCode::BAD_REQUEST, Json(err));
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

    // From here on the request may run for as long as the agent turn behind it
    // needs, so it becomes a detached job rather than staying inside this
    // handler's future. Two things follow from that. The turn survives a client
    // that hangs up — a webhook sender's read timeout no longer destroys work
    // that had already started writing memory and calling tools. And the
    // idempotency claim travels *with* the work instead of with the connection,
    // so a dropped connection can no longer release the claim underneath a run
    // that is still going.
    let job = jobs::submit(
        jobs::KIND_WEBHOOK,
        format!("gateway:webhook:{}", fabric_channel_label(&webhook_body)),
        run_webhook_job(
            state,
            webhook_body,
            owner_credential,
            idempotency_digest,
            idempotency_claim,
        ),
    );

    if async_mode {
        return (StatusCode::ACCEPTED, Json(job.accepted_body()));
    }
    job.wait().await.into_parts()
}

/// Label for the registry row of a webhook job, so `prx tasks list` shows which
/// delivery target the run belongs to rather than a bare route name.
fn fabric_channel_label(body: &WebhookBody) -> &str {
    body.reply_target.as_deref().unwrap_or("anonymous")
}

/// The unbounded half of `POST /webhook`, run as a job.
///
/// Owns the idempotency claim for the whole run: the claim is only resolved by
/// code that knows how the run ended, never by the request future going away.
fn run_webhook_job(
    state: AppState,
    webhook_body: WebhookBody,
    credential: String,
    idempotency_digest: Option<String>,
    idempotency_claim: Option<IdempotencyClaim>,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<jobs::JobOutput, String>> + Send>> {
    // Keep owned webhook work out of the generic job submitter's future.
    // Construction does not poll it; the same idempotency claim moves with it.
    Box::pin(run_webhook_job_work(
        state,
        webhook_body,
        credential,
        idempotency_digest,
        idempotency_claim,
    ))
}

async fn run_webhook_job_work(
    state: AppState,
    webhook_body: WebhookBody,
    credential: String,
    idempotency_digest: Option<String>,
    idempotency_claim: Option<IdempotencyClaim>,
) -> Result<jobs::JobOutput, String> {
    let mut idempotency_claim = idempotency_claim;
    let message = webhook_body.message.as_str();

    if state.auto_save && should_autosave_gateway_message(webhook_body.reply_target.as_deref(), message) {
        if let Err((status, body)) =
            authorize_gateway_resource_mutation(&state, "gateway:webhook:autosave", ResourceRiskLevel::Low)
        {
            return Ok(jobs::JobOutput::new(status, body.0));
        }
        // First observable side effect of the attempt: past this point a
        // redelivery of the same key would duplicate it.
        if let Some(claim) = idempotency_claim.as_mut() {
            claim.mark_dispatched();
        }
        let key = webhook_memory_key(idempotency_digest.as_deref());
        let _ = state.mem.store(&key, message, MemoryCategory::Conversation, None).await;
    }

    let provider_label = state
        .config
        .load_full()
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
        &credential,
        idempotency_digest
            .as_deref()
            .map(|digest| format!("gateway:webhook:{digest}")),
    );
    // The turn records the inbound message and may call tools, so from here the
    // attempt is not replayable by re-execution — only by the stored result.
    if let Some(claim) = idempotency_claim.as_mut() {
        claim.mark_dispatched();
    }
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
                    return Ok(jobs::JobOutput::new(StatusCode::SERVICE_UNAVAILABLE, err));
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
            Ok(jobs::JobOutput::new(StatusCode::OK, body))
        }
        Err(e) => {
            if let Some(claim) = idempotency_claim.take() {
                let _ = claim.fail();
            }
            if e.is::<crate::runtime::ingress_recovery::AlreadyAccepted>() {
                return Ok(jobs::JobOutput::new(
                    StatusCode::CONFLICT,
                    serde_json::json!({"error":"input_already_accepted","status":"unconfirmed","message":"Execution was not repeated; the previously accepted input has an unconfirmed outcome."}),
                ));
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

            if let Some(tools) = providers::tool_schema_refused_tools(&e) {
                // A configuration problem, not a transient one: the whole
                // refusal (every tool, the configuration to add) is logged
                // unclipped, and the authenticated caller gets a stable code
                // with the refused tools.
                tracing::error!(
                    error = %crate::security::redaction::redact_secrets(&format!("{e:#}")),
                    "Webhook turn refused before transport: the provider cannot accept some offered tool schemas"
                );
                let err = serde_json::json!({
                    "error": "tool_schema_refused",
                    "tools": tools,
                    "message": "The configured provider cannot accept some of the tool schemas this request \
                                offered, so nothing was sent. The gateway log names the configuration to change.",
                });
                return Ok(jobs::JobOutput::new(StatusCode::INTERNAL_SERVER_ERROR, err));
            }
            tracing::error!("Webhook provider error: {}", sanitized);
            let err = serde_json::json!({"error": "LLM request failed"});
            Ok(jobs::JobOutput::new(StatusCode::INTERNAL_SERVER_ERROR, err))
        }
    }
}

/// Registry label for one accepted channel-webhook delivery.
///
/// Named the way every other registry row names a correspondent: the channel in
/// the clear, the chat only as [`crate::security::op_id::ref_for_channel_recipient`]'s
/// fingerprint. `reply_target` on this route is a platform address — a Telegram
/// chat id, a WhatsApp JID — and this label is published verbatim by
/// `GET /api/runtime/tasks`, so writing it out would leak through the operator
/// plane the very address the send path takes care to fingerprint. Sharing that
/// one function is also what keeps the digest here equal to the digest an
/// outbound send to the same chat produces, so the two rows stay correlatable.
///
/// MUTATION GUARD: interpolate `reply_target` directly and
/// `a_channel_webhook_job_label_fingerprints_the_chat` fails.
fn channel_webhook_job_label(channel_key: &str, first: Option<&crate::channels::traits::ChannelMessage>) -> String {
    first.map_or_else(
        || format!("gateway:{channel_key}"),
        |msg| {
            format!(
                "gateway:{channel_key}:{}",
                crate::security::op_id::ref_for_channel_recipient(channel_key, &msg.reply_target)
            )
        },
    )
}

/// Shared tail of the three platform-pushed channel webhooks.
///
/// These routes differ from `POST /webhook` in one decisive way: the sender is a
/// platform with a response deadline, not a client that chose to wait. Linq
/// documents "Response timeout: 10 seconds" and retries "up to 10 times over ~25
/// minutes"; Meta retries a WhatsApp delivery "with decreasing frequency until
/// the request succeeds, for up to 7 days". An agent turn has no upper bound and
/// routinely exceeds any of those, so awaiting the turn inside the handler — as
/// these three did — guaranteed the platform would give up, mark the delivery
/// failed, and redeliver, running the whole turn again for every retry.
///
/// The ack therefore reports that the work was *accepted*, not that it finished:
/// the turns run as a detached job, visible in `prx tasks list` and killable with
/// `prx tasks kill`, and the reply reaches the user through the channel's own
/// outbound API exactly as before. The delivery is claimed before submission, so
/// a retry that races the accepted run is answered as a duplicate instead of
/// starting a second copy of it.
fn accept_channel_webhook_delivery(
    state: &AppState,
    channel: Arc<dyn Channel>,
    channel_key: &'static str,
    body: &Bytes,
    messages: Vec<crate::channels::traits::ChannelMessage>,
    memory_key: fn(&crate::channels::traits::ChannelMessage) -> String,
) -> (StatusCode, Json<serde_json::Value>) {
    let dedup_ttl = Duration::from_secs(state.config.load_full().gateway.channel_webhook_dedup_ttl_secs.max(1));
    let claim = match webhook_dedup::claim(webhook_dedup::delivery_key(channel_key, body), dedup_ttl) {
        webhook_dedup::DeliveryOutcome::Fresh(claim) => claim,
        webhook_dedup::DeliveryOutcome::Duplicate => {
            tracing::info!("{channel_key} webhook: redelivery of an already accepted payload — not run again");
            return (
                StatusCode::OK,
                Json(serde_json::json!({ "status": "duplicate", "idempotent": true })),
            );
        }
    };

    let label = channel_webhook_job_label(channel_key, messages.first());
    let job = jobs::submit(
        jobs::KIND_CHANNEL_WEBHOOK,
        label,
        run_channel_webhook_job(state.clone(), channel, channel_key, messages, memory_key, claim),
    );

    // Every platform on this route treats any 2xx as delivered; 200 is the one
    // status all three accept without special handling.
    (StatusCode::OK, Json(job.accepted_body()))
}

/// The unbounded half of a platform channel webhook, run as a job.
///
/// Owns the delivery claim for the whole run, so the claim is resolved by code
/// that knows whether the turn reached a side effect — never by the request
/// future going away.
fn run_channel_webhook_job(
    state: AppState,
    channel: Arc<dyn Channel>,
    channel_key: &'static str,
    messages: Vec<crate::channels::traits::ChannelMessage>,
    memory_key: fn(&crate::channels::traits::ChannelMessage) -> String,
    claim: webhook_dedup::DeliveryClaim,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<jobs::JobOutput, String>> + Send>> {
    // Keep owned channel work out of the generic job submitter's future.
    // Construction does not poll it; the same delivery claim moves with it.
    Box::pin(run_channel_webhook_job_work(
        state,
        channel,
        channel_key,
        messages,
        memory_key,
        claim,
    ))
}

async fn run_channel_webhook_job_work(
    state: AppState,
    channel: Arc<dyn Channel>,
    channel_key: &'static str,
    messages: Vec<crate::channels::traits::ChannelMessage>,
    memory_key: fn(&crate::channels::traits::ChannelMessage) -> String,
    mut claim: webhook_dedup::DeliveryClaim,
) -> Result<jobs::JobOutput, String> {
    let provider_label = state
        .config
        .load_full()
        .default_provider
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    for msg in &messages {
        tracing::info!(
            "{channel_key} message from {}: {}",
            msg.sender,
            truncate_with_ellipsis(&msg.content, 50)
        );

        // Auto-save to memory
        if state.auto_save && should_autosave_gateway_message(Some(&msg.reply_target), &msg.content) {
            if let Err((status, body)) = authorize_gateway_resource_mutation(
                &state,
                &gateway_channel_webhook_operation(channel_key, "autosave"),
                ResourceRiskLevel::Low,
            ) {
                return Ok(jobs::JobOutput::new(status, body.0));
            }
            // First observable side effect: past this point a redelivery would
            // duplicate it, so the claim must outlive the run.
            claim.mark_dispatched();
            let key = memory_key(msg);
            let _ = state
                .mem
                .store(&key, &msg.content, MemoryCategory::Conversation, None)
                .await;
        }

        let fabric_ctx = GatewayFabricContext::channel_message(msg);
        // The turn records the inbound message and may call tools, so from here
        // the delivery is not replayable by re-execution.
        claim.mark_dispatched();
        match run_gateway_chat_with_multimodal(&state, &provider_label, &msg.content, &fabric_ctx).await {
            Ok(response) => {
                if let Err((status, body)) = authorize_gateway_resource_mutation(
                    &state,
                    &gateway_channel_webhook_operation(channel_key, "send"),
                    ResourceRiskLevel::Low,
                ) {
                    return Ok(jobs::JobOutput::new(status, body.0));
                }
                if let Err(e) = channel.send(&SendMessage::new(response, &msg.reply_target)).await {
                    tracing::error!("Failed to send {channel_key} reply: {e}");
                }
            }
            Err(e) => {
                tracing::error!("LLM error for {channel_key} message: {e:#}");
                if let Err((status, body)) = authorize_gateway_resource_mutation(
                    &state,
                    &gateway_channel_webhook_operation(channel_key, "send"),
                    ResourceRiskLevel::Low,
                ) {
                    return Ok(jobs::JobOutput::new(status, body.0));
                }
                let _ = channel
                    .send(&SendMessage::new(
                        if e.is::<crate::runtime::ingress_recovery::AlreadyAccepted>() {"This input was already accepted. Execution was not repeated; its outcome remains unconfirmed."} else {"Sorry, I couldn't process your message right now."},
                        &msg.reply_target,
                    ))
                    .await;
            }
        }
    }

    Ok(jobs::JobOutput::new(
        StatusCode::OK,
        serde_json::json!({ "status": "ok", "messages": messages.len() }),
    ))
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

    // Webhook-delivered channels never pass through the channel dispatch loop,
    // so their inbound activity is recorded here instead (see
    // `channels::activity`). Reporting only — nothing acts on it.
    crate::channels::activity::record_inbound(wa.name());

    let channel: Arc<dyn Channel> = Arc::clone(wa) as Arc<dyn Channel>;
    accept_channel_webhook_delivery(&state, channel, "whatsapp", &body, messages, whatsapp_memory_key)
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

    // Webhook-delivered channels never pass through the channel dispatch loop,
    // so their inbound activity is recorded here instead (see
    // `channels::activity`). Reporting only — nothing acts on it.
    crate::channels::activity::record_inbound(linq.name());

    let channel: Arc<dyn Channel> = Arc::clone(linq) as Arc<dyn Channel>;
    accept_channel_webhook_delivery(&state, channel, "linq", &body, messages, linq_memory_key)
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

    // Webhook-delivered channels never pass through the channel dispatch loop,
    // so their inbound activity is recorded here instead (see
    // `channels::activity`). Reporting only — nothing acts on it.
    crate::channels::activity::record_inbound(nextcloud_talk.name());

    let channel: Arc<dyn Channel> = Arc::clone(nextcloud_talk) as Arc<dyn Channel>;
    accept_channel_webhook_delivery(
        &state,
        channel,
        "nextcloud_talk",
        &body,
        messages,
        nextcloud_talk_memory_key,
    )
}

#[cfg(test)]
mod platform_test_transport;

#[cfg(test)]
mod tests;

//! Process-global cache and fetch of remote JWKS documents for MCP bearer
//! JWT verification.

use jsonwebtoken::jwk::JwkSet;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

/// Maximum time to wait for a remote JWKS fetch before giving up (fail-closed).
const JWKS_FETCH_TIMEOUT_SECS: u64 = 5;

/// How long a failed fetch for a URI is remembered. While it holds, no new
/// fetch is attempted for that URI; verification runs against whatever the
/// cache holds (fail-closed when stale or empty).
const JWKS_FETCH_BACKOFF: Duration = Duration::from_secs(30);

/// A fetched remote JWKS document together with the instant it was retrieved.
#[derive(Clone)]
pub(super) struct CachedJwks {
    pub(super) jwks: Arc<JwkSet>,
    pub(super) fetched_at: Instant,
}

impl CachedJwks {
    fn is_fresh(&self, ttl: Duration) -> bool {
        self.fetched_at.elapsed() < ttl
    }
}

/// Process-global cache of remote JWKS documents, keyed by endpoint URI. Reads
/// are lock-light; refreshes happen on the async pre-warm path so the
/// synchronous verification path never performs network I/O.
pub(super) static REMOTE_JWKS_CACHE: LazyLock<RwLock<HashMap<String, CachedJwks>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Instant of the last failed fetch per endpoint URI. Cleared by a successful
/// fetch. Never held across an `.await`.
static JWKS_FETCH_FAILURES: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Single flight: at most one remote JWKS fetch runs per process at a time.
static JWKS_FETCH_FLIGHT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn cache_is_fresh(uri: &str, ttl: Duration) -> bool {
    REMOTE_JWKS_CACHE.read().get(uri).is_some_and(|e| e.is_fresh(ttl))
}

fn in_fetch_backoff(uri: &str) -> bool {
    JWKS_FETCH_FAILURES
        .lock()
        .get(uri)
        .is_some_and(|failed_at| failed_at.elapsed() < JWKS_FETCH_BACKOFF)
}

fn record_fetch_failure(uri: &str) {
    JWKS_FETCH_FAILURES.lock().insert(uri.to_string(), Instant::now());
}

/// Records a fetch failure when the request future holding the fetch lock is
/// dropped before the fetch completes (for example a client disconnect), so a
/// hanging endpoint still starts the back-off. Disarmed on every completed
/// outcome. Takes only the failure-map lock and logs nothing.
struct FetchCancelGuard<'a> {
    uri: &'a str,
    armed: bool,
}

impl Drop for FetchCancelGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            record_fetch_failure(self.uri);
        }
    }
}

/// Return the cached JWKS for `uri` if a fresh (within `ttl_secs`) copy exists.
///
/// Synchronous and never performs network I/O — refreshing is the job of
/// [`prewarm_remote_jwks`]. Returns `None` when no entry exists or the cached
/// copy is stale, which the verification path translates into a fail-closed
/// rejection.
pub(super) fn cached_remote_jwks(uri: &str, ttl_secs: u64) -> Option<Arc<JwkSet>> {
    let ttl = Duration::from_secs(ttl_secs.max(1));
    let guard = REMOTE_JWKS_CACHE.read();
    let entry = guard.get(uri)?;
    if entry.is_fresh(ttl) {
        Some(Arc::clone(&entry.jwks))
    } else {
        None
    }
}

/// Ensure the remote JWKS cache for `config` holds a fresh document before the
/// synchronous verification path runs. No-op unless a remote `jwt_jwks_uri` is
/// configured and no inline `jwt_jwks` overrides it.
///
/// The fetch is async, so the runtime is never stalled; concurrent requests
/// share one fetch and a failed fetch backs off per URI. On fetch failure the existing cache entry (if any) is left in
/// place — a still-fresh entry keeps working, a stale/absent one results in a
/// fail-closed rejection at verification time.
pub(super) async fn prewarm_remote_jwks(config: &crate::config::McpServerRuntimeConfig) {
    if config.jwt_jwks.as_deref().is_some_and(|v| !v.trim().is_empty()) {
        // Inline JWKS wins; no remote fetch needed.
        return;
    }
    let raw_uri = match config.jwt_jwks_uri.as_deref().filter(|v| !v.trim().is_empty()) {
        Some(v) => v,
        None => return,
    };
    // Enforce HTTPS-only before issuing any network request (fail-closed).
    // normalize_jwks_uri also trims whitespace so the returned slice is the
    // canonical cache key — identical to what resolve_decoding_key uses.
    let uri = match super::normalize_jwks_uri(raw_uri) {
        Ok(u) => u.to_string(),
        Err(_) => {
            tracing::warn!(
                uri = %raw_uri.trim(),
                "jwt_jwks_uri does not use https:// — remote JWKS fetch refused (fail-closed)"
            );
            return;
        }
    };
    let ttl = Duration::from_secs(config.jwt_jwks_cache_ttl_secs.max(1));

    // Fast path: a fresh entry already satisfies this request, and a recent
    // failure means the endpoint is not asked again yet.
    if cache_is_fresh(&uri, ttl) || in_fetch_backoff(&uri) {
        return;
    }

    // Single flight: requests that queued behind an in-progress fetch re-check
    // freshness and the back-off once they hold the lock, so a burst produces
    // one fetch. The lock covers only the fetch and the cache update.
    let _flight = JWKS_FETCH_FLIGHT.lock().await;
    if cache_is_fresh(&uri, ttl) || in_fetch_backoff(&uri) {
        return;
    }

    let mut guard = FetchCancelGuard { uri: &uri, armed: true };
    let outcome = fetch_remote_jwks(&uri).await;
    guard.armed = false;
    match outcome {
        Ok(jwks) => {
            REMOTE_JWKS_CACHE.write().insert(
                uri.clone(),
                CachedJwks {
                    jwks: Arc::new(jwks),
                    fetched_at: Instant::now(),
                },
            );
            JWKS_FETCH_FAILURES.lock().remove(&uri);
        }
        Err(error) => {
            record_fetch_failure(&uri);
            tracing::warn!(error = %error, "remote jwks refresh failed; relying on existing cache (fail-closed if stale)");
        }
    }
}

/// Fetch + parse a remote JWKS document.
///
/// Deliberately async rather than `reqwest::blocking` inside `spawn_blocking`:
/// a blocking HTTP call holds one of the process's finite blocking-pool threads
/// for the whole round trip (up to `JWKS_FETCH_TIMEOUT_SECS`), while the async
/// client holds none. With unbounded concurrent gateway traffic, a slow or
/// unreachable JWKS endpoint could otherwise drain the pool on its own.
/// The bounded timeout is retained so a hung endpoint cannot stall the caller.
async fn fetch_remote_jwks(uri: &str) -> Result<JwkSet, anyhow::Error> {
    use anyhow::Context as _;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(JWKS_FETCH_TIMEOUT_SECS))
        .build()
        .context("failed to build jwks http client")?;
    let response = client
        .get(uri)
        .send()
        .await
        .context("jwks endpoint request failed")?
        .error_for_status()
        .context("jwks endpoint returned an error status")?;
    let body = response.text().await.context("failed to read jwks response body")?;
    serde_json::from_str::<JwkSet>(&body).context("jwks endpoint returned invalid json")
}

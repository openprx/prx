//! Process-global cache and fetch of remote JWKS documents for MCP bearer
//! JWT verification.

use jsonwebtoken::jwk::JwkSet;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

/// Maximum time to wait for a remote JWKS fetch before giving up (fail-closed).
const JWKS_FETCH_TIMEOUT_SECS: u64 = 5;

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
/// Runs the blocking HTTP fetch on a blocking thread so the async runtime is
/// never stalled. On fetch failure the existing cache entry (if any) is left in
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

    // Fast path: a fresh entry already satisfies this request.
    if REMOTE_JWKS_CACHE.read().get(&uri).is_some_and(|e| e.is_fresh(ttl)) {
        return;
    }

    match fetch_remote_jwks(&uri).await {
        Ok(jwks) => {
            REMOTE_JWKS_CACHE.write().insert(
                uri,
                CachedJwks {
                    jwks: Arc::new(jwks),
                    fetched_at: Instant::now(),
                },
            );
        }
        Err(error) => {
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

#![cfg(test)]

use super::*;

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
    let limiter = SlidingWindowRateLimiter::new(10, Duration::from_mins(1), 100);
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
    let limiter = SlidingWindowRateLimiter::new(0, Duration::from_mins(1), 10);
    for _ in 0..100 {
        assert!(limiter.allow("any-key"));
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
    let limiter = SlidingWindowRateLimiter::new(5, Duration::from_mins(1), 2);
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
    let store = Arc::new(IdempotencyStore::new(Duration::from_mins(5), 2));
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

/// D2 / fix 3 regression: `persist_pairing_tokens` must base the persisted/published
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

    let shared = crate::config::new_shared(hot_config);
    persist_pairing_tokens(&shared, &guard).await.unwrap();

    // `paired_tokens` is encrypted at rest; read it the way the loader does.
    let saved = tokio::fs::read_to_string(&config_path).await.unwrap();
    assert!(
        !saved.contains(token.as_str()),
        "the generated token must never reach the file"
    );
    let parsed = Config::load_from_path(&config_path, temp.path().join("workspace")).unwrap();

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

    let hot = shared.load_full();
    assert!(
        (hot.default_temperature - 0.42).abs() < 1e-9,
        "D must retain the hot value 0.42 after persist, got {}",
        hot.default_temperature
    );
    // CONSOLE-OBS-3: the running guard already holds the token it issued,
    // so the pairing reload adopts the persisted token list into the
    // active generation (and only that: other `gateway` fields stay
    // restart-only, see `config::generation`).
    assert_eq!(
        hot.gateway.paired_tokens, parsed.gateway.paired_tokens,
        "the pairing reload publishes the token the guard already accepts"
    );
    let desired = shared.desired();
    assert_eq!(desired.config.gateway.paired_tokens.len(), 1);
    assert_eq!(&desired.config.gateway.paired_tokens[0], persisted);
}

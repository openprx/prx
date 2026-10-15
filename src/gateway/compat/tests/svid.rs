#![cfg(test)]

use super::*;

// -- SPIFFE SVID X.509 verification -----------------------------------

const TEST_CA_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDEzCCAfugAwIBAgIUDhSoIjROzqwbTnjzWsE0ruuM+OEwDQYJKoZIhvcNAQEL\n\
BQAwGTEXMBUGA1UEAwwOdGVzdC1zcGlmZmUtY2EwHhcNMjYwNTMxMjA0NzExWhcN\n\
MzYwNTI4MjA0NzExWjAZMRcwFQYDVQQDDA50ZXN0LXNwaWZmZS1jYTCCASIwDQYJ\n\
KoZIhvcNAQEBBQADggEPADCCAQoCggEBAJzwAHQX7gZCZq5xwYhEPXr8tn2hWWtj\n\
M6dq3KjJ+44FklT4dGO9EC7WvzJlfPokppuUYLvuRPT/Hsdcru4lL4CzYbECN7Ci\n\
aLxN+5rHKIrt25O53bu2878TsJufX1pQqusGx4hJNOQIOq9cjfbOdCJ9HKzI0E/x\n\
uGsieEfm5HSeh1h8ZVT3j6I1yvR69A/k/a/p1yon19qx+iQ4Xgf5Fa7ntKlX9xdt\n\
pKWsjRQEmpb6aeUcfcyyIzJmm9EceJaE5okAr+8B2O9ds1v3PxzKtAHaXsqjpFGz\n\
ANme7GfMf00qgIB2NH9zC+c2btE8Au3oRYXkmwr5Dd6EWAXvMAou2lkCAwEAAaNT\n\
MFEwHQYDVR0OBBYEFJU1LDUF0KECh/i1zlQ3FIhSYU0GMB8GA1UdIwQYMBaAFJU1\n\
LDUF0KECh/i1zlQ3FIhSYU0GMA8GA1UdEwEB/wQFMAMBAf8wDQYJKoZIhvcNAQEL\n\
BQADggEBAHUurEvLru/d2gYIEtwBoBxEbgzct45k+hASehG1kF4B5Mbl+ZEqI5UN\n\
5gvtlRQSqmbkO1VabYwk9SLK6uD9zUb08KiKJJQedrMjxs/g0kbVHdYolQneY51V\n\
4cMDYOi1/AIfonK+I6G97DgouRGWMRtg0dmmqj2KwF4O3F4AWlfgSFUJfrAk/BN+\n\
D1UJLeOuPwnWYJmwgJTYkMi1gzWHSCwjMw8/K1B/gK5CVjfWc5Qq5RpmditgtZPi\n\
/yOiuWQsBixthIxfCqspj5mRoGu8ZOTiQSzfbKQpbwQZtKYjkVqOt+EcdxD3Vo5m\n\
rfMSOTLgmwS0fvgZn1UiBJm/hiCMyno=\n\
-----END CERTIFICATE-----\n";

const TEST_LEAF_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDLjCCAhagAwIBAgIUcfUQUBXgovzvmrcNl4cwB5pVH6EwDQYJKoZIhvcNAQEL\n\
BQAwGTEXMBUGA1UEAwwOdGVzdC1zcGlmZmUtY2EwHhcNMjYwNTMxMjA0NzExWhcN\n\
MzYwNTI4MjA0NzExWjATMREwDwYDVQQDDAh3b3JrbG9hZDCCASIwDQYJKoZIhvcN\n\
AQEBBQADggEPADCCAQoCggEBAL496gM4R3YYIquFKEKzJS9hXpWrStLVVmiCGBxK\n\
keGCpZ1C80myFo1GV+QKRP5bNOryBkPQNmUhCMfe/pD0CN4pdXRLliEoy6bX/Qnv\n\
osQlrRanINTPOkM/CAw462pOHuu8MSRODrLmd96udRacez6nhrgfm9wWxLCeB4qn\n\
zhtT+utm1VFkZhthF+M5x/xZDYLVdZbft6Aj+feRyTXoYpnYfwD7an+UgBWAAuA3\n\
AP9uue3GaGbQcgKB9WOIoRxTlkcFATWbO/07mNXv/OM+2/mI3rCtNTU/Iyk/5MyS\n\
3kzaGnj631pP/5v+EZAWPxD5lgW7bfQP05lgug14L2RAFdECAwEAAaN0MHIwMAYD\n\
VR0RBCkwJ4Ylc3BpZmZlOi8vdHJ1c3RlZC5leGFtcGxlL2FnZW50L3dvcmtlcjAd\n\
BgNVHQ4EFgQUfZIt57LabHcu+3ZwUvH3kQgjK9UwHwYDVR0jBBgwFoAUlTUsNQXQ\n\
oQKH+LXOVDcUiFJhTQYwDQYJKoZIhvcNAQELBQADggEBAECx5q+dY8WANqkWT9aL\n\
llsPhDcSegsWNR9EAos1b0RZFgsbBnXJd/p4WFyp4R2IKmNS83kQMI3h8KaElrrq\n\
IKbVir8bTB2w3KJJ+ZZ2ZBwDyQ86t/lYl7O6VsfZyRiu/nwXUddx2wYLmHSNqTRj\n\
yoYOfb4yDo5Ezi4ttGXBmcZkBcbpB5zsM74CgA7puPp6trgCYRohE44DlLHGZN2c\n\
dkMK/xsOTz1kOowyahZDADPgATW+SoXQ5tnR4gkunfPdowUyuqOkWohgy+WmdUM8\n\
HkRJBc3+5CQr6Za4zjfNTMrMMTE8LHLp08uZGdOkaMMhAftF1IR+UMWRdJ3wkCzm\n\
KIo=\n\
-----END CERTIFICATE-----\n";

// A leaf carrying the same SPIFFE SAN but signed by an UNTRUSTED CA.
const TEST_ROGUE_LEAF_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDJTCCAg2gAwIBAgIUE9uuBu6Z8BKxQdYG7hR/1Lx+WZgwDQYJKoZIhvcNAQEL\n\
BQAwEzERMA8GA1UEAwwIcm9ndWUtY2EwHhcNMjYwNTMxMjA0NzExWhcNMzYwNTI4\n\
MjA0NzExWjAQMQ4wDAYDVQQDDAVyb2d1ZTCCASIwDQYJKoZIhvcNAQEBBQADggEP\n\
ADCCAQoCggEBANK5BOkp7KvPV1aBSCSL9u68QiWdFPdypNx12Y/EmHyTJwkRQ8Cz\n\
/DcT4Txsab0tzerwdPptfkIqX8hk/1YTsC4jsOAL9kxsIWNEsO8n8sClMgNnOaW7\n\
tcsjJ3GeJFFsI/pz6X9cbTEcI20v+migBrKtvppmZlvoAbNAxfz78i2HZVM9IGgR\n\
Tsulpbwq1uegZbkeZJGsNXjbQdfhXW6ykKBq69AEfpimBubVZcH7IOrko23Z4/SN\n\
BDgfJRWbbdbqQy7PGxHjqVPl1+gGO8MSWDjK3Ht9+VZKVIu80T1Xm2BWoEcLKJVc\n\
psoJX0LtIZRCzFsOIlQfzeWmh1IZJVekn4UCAwEAAaN0MHIwMAYDVR0RBCkwJ4Yl\n\
c3BpZmZlOi8vdHJ1c3RlZC5leGFtcGxlL2FnZW50L3dvcmtlcjAdBgNVHQ4EFgQU\n\
VtrnJ0c5c9373mu7BT7qLjG1sjIwHwYDVR0jBBgwFoAUKwgA2ADDQPrrz0HSPZ0G\n\
5Ncvp3QwDQYJKoZIhvcNAQELBQADggEBAF4FtdodA+1JXPEQQHppu/AcvoYevMB9\n\
49WRnv8plcKbsUxaNh8ji5aWrq1NZ2JTGbs7NFD63t1L9lMY3wlXH8jGmgyx+XgQ\n\
p7+XY0xJ3sljbxmjxym5HNn6L2uOQV77zOwumAsjcTm2YtUTMoBqDxFDQB50nin9\n\
+jK6Y34CtczVEFzLVajfEXJNoYjqPw2C02YAfuFs4S0WALQsWG27sC/UI4amXYuJ\n\
f/+LwM1E2HpwH8ZFVfFQr0acepQEMs9gkymArUuqdWObmM5v2xAw1WY8rH6UX6Cm\n\
2nXfrUbRSIN6SXEctFoy2APvsTy5T3UpzNTHCRW+B81QBEy/cJ4yDrU=\n\
-----END CERTIFICATE-----\n";

fn svid_config() -> crate::config::A2aConfig {
    crate::config::A2aConfig {
        spiffe_trust_bundle_pem: Some(TEST_CA_PEM.to_string()),
        trusted_trust_domains: vec!["trusted.example".to_string()],
        ..Default::default()
    }
}

/// HTTP header values cannot carry raw newlines; percent-encode the PEM the
/// way a front proxy would when transporting an SVID via a header.
fn header_safe_pem(pem: &str) -> String {
    pem.replace('\n', "%0A")
}

#[test]
fn parse_trust_domain_extracts_authority() {
    assert_eq!(
        parse_spiffe_trust_domain("spiffe://trusted.example/agent/x").unwrap(),
        "trusted.example"
    );
    assert!(parse_spiffe_trust_domain("https://not-spiffe/x").is_err());
    assert!(parse_spiffe_trust_domain("spiffe:///empty-authority").is_err());
}

#[test]
fn valid_svid_chains_to_trust_bundle() {
    let result = verify_svid(TEST_LEAF_PEM, &svid_config()).expect("test: valid svid");
    assert_eq!(result.spiffe_id, "spiffe://trusted.example/agent/worker");
    assert_eq!(result.trust_domain, "trusted.example");
    assert!(result.trusted);
    assert_eq!(result.cert_fingerprint.len(), 64);
}

#[test]
fn svid_with_untrusted_chain_is_rejected() {
    // Same SPIFFE SAN, but signed by a CA not in our trust bundle.
    let err = verify_svid(TEST_ROGUE_LEAF_PEM, &svid_config()).expect_err("test: untrusted chain");
    assert_eq!(err, SvidError::UntrustedChain);
}

#[test]
fn svid_without_trust_bundle_is_rejected() {
    let mut config = svid_config();
    config.spiffe_trust_bundle_pem = None;
    let err = verify_svid(TEST_LEAF_PEM, &config).expect_err("test: no bundle");
    assert_eq!(err, SvidError::NoTrustBundle);
}

#[test]
fn svid_with_untrusted_domain_is_rejected() {
    let mut config = svid_config();
    config.trusted_trust_domains = vec!["other.example".to_string()];
    let err = verify_svid(TEST_LEAF_PEM, &config).expect_err("test: untrusted domain");
    assert_eq!(err, SvidError::UntrustedDomain("trusted.example".to_string()));
}

#[test]
fn svid_empty_domain_list_accepts_any_chained_domain() {
    let mut config = svid_config();
    config.trusted_trust_domains.clear();
    let result = verify_svid(TEST_LEAF_PEM, &config).expect("test: empty domain list");
    assert!(result.trusted);
}

#[test]
fn garbage_svid_pem_does_not_panic() {
    let err = verify_svid("not a certificate", &svid_config()).expect_err("test: garbage");
    assert!(matches!(err, SvidError::ParseError(_)));
}

#[test]
fn strict_mode_rejects_svid_failure() {
    let mut config = crate::config::Config::default();
    config.gateway.trust_forwarded_headers = true;
    config.a2a = svid_config();
    config.a2a.spiffe_strict_validation = true;
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-spiffe-id",
        HeaderValue::from_static("spiffe://trusted.example/agent/worker"),
    );
    // Rogue SVID -> strict mode must reject (no identity derived).
    let mut value = HeaderValue::from_str(&header_safe_pem(TEST_ROGUE_LEAF_PEM)).expect("test: header value");
    value.set_sensitive(true);
    headers.insert("x-spiffe-svid", value);
    assert!(derive_external_agent_identity(&headers, &config).is_none());
}

#[test]
fn verified_svid_overrides_asserted_header() {
    let mut config = crate::config::Config::default();
    config.gateway.trust_forwarded_headers = true;
    config.a2a = svid_config();
    let mut headers = HeaderMap::new();
    // Caller asserts a DIFFERENT spiffe id in the header...
    headers.insert(
        "x-spiffe-id",
        HeaderValue::from_static("spiffe://attacker.example/agent/evil"),
    );
    // ...but presents a valid SVID. The SVID SAN must win.
    let value = HeaderValue::from_str(&header_safe_pem(TEST_LEAF_PEM)).expect("test: header value");
    headers.insert("x-spiffe-svid", value);
    let identity = derive_external_agent_identity(&headers, &config).expect("test: svid identity");
    assert_eq!(identity.external_subject, "spiffe://trusted.example/agent/worker");
}

#[test]
fn non_strict_mode_falls_back_to_header_on_svid_failure() {
    let mut config = crate::config::Config::default();
    config.gateway.trust_forwarded_headers = true;
    config.a2a = svid_config();
    config.a2a.spiffe_strict_validation = false;
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-spiffe-id",
        HeaderValue::from_static("spiffe://trusted.example/agent/worker"),
    );
    let value = HeaderValue::from_str(&header_safe_pem(TEST_ROGUE_LEAF_PEM)).expect("test: header value");
    headers.insert("x-spiffe-svid", value);
    // Non-strict: rogue SVID fails, but header-asserted id is still honored.
    let identity = derive_external_agent_identity(&headers, &config).expect("test: fallback identity");
    assert_eq!(identity.external_subject, "spiffe://trusted.example/agent/worker");
}

// ── MCP-HDR-ID (OQ-13 / OQ-14): proxy-asserted identity needs a trusted proxy ──

/// `x-client-cert-subject` / `x-forwarded-client-cert` are plain request
/// headers. Without `gateway.trust_forwarded_headers` nothing guarantees a
/// proxy wrote them, so they must not mint an `mtls` identity.
#[test]
fn untrusted_mtls_subject_header_mints_no_identity() {
    let config = crate::config::Config::default();
    assert!(
        !config.gateway.trust_forwarded_headers,
        "sanity: the flag is off by default"
    );
    for name in ["x-client-cert-subject", "x-forwarded-client-cert"] {
        let mut headers = HeaderMap::new();
        headers.insert(name, HeaderValue::from_static("CN=victim"));
        assert_eq!(
            derive_external_agent_identity(&headers, &config),
            None,
            "an untrusted {name} header must not mint an identity"
        );
    }
}

#[test]
fn untrusted_spiffe_header_mints_no_identity() {
    let config = crate::config::Config::default();
    let mut headers = HeaderMap::new();
    headers.insert("x-spiffe-id", HeaderValue::from_static("spiffe://issuer/agent/a"));
    assert_eq!(derive_external_agent_identity(&headers, &config), None);
}

/// Q-1 (D-019): a certificate in a header proves no possession of its key,
/// so even an SVID that verifies against the trust bundle is a replayable
/// assertion and is gated like the other forwarded identities.
#[test]
fn untrusted_svid_header_mints_no_identity_even_when_it_verifies() {
    let mut config = crate::config::Config::default();
    config.a2a = svid_config();
    for svid_header in ["x-spiffe-svid", "x-client-cert-pem"] {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-spiffe-id",
            HeaderValue::from_static("spiffe://trusted.example/agent/worker"),
        );
        let value = HeaderValue::from_str(&header_safe_pem(TEST_LEAF_PEM)).expect("test: header value");
        headers.insert(svid_header, value);
        assert!(
            verify_svid(TEST_LEAF_PEM, &config.a2a).is_ok(),
            "sanity: the presented SVID verifies"
        );
        assert_eq!(
            derive_external_agent_identity(&headers, &config),
            None,
            "a verified SVID in {svid_header} must not mint an identity without a trusted proxy"
        );
    }
}

#[test]
fn untrusted_identity_header_does_not_preempt_a_valid_bearer() {
    let mut config = crate::config::Config::default();
    config.mcp_server = pem_config();
    let token = sign_rs256(
        &TestClaims {
            iss: "https://issuer.example".to_string(),
            sub: "attacker".to_string(),
            aud: Some("prx-mcp".to_string()),
            exp: future_exp(),
        },
        None,
    );
    for (name, value) in [
        ("x-client-cert-subject", "CN=victim"),
        ("x-forwarded-client-cert", "CN=victim"),
        ("x-spiffe-id", "spiffe://issuer/agent/victim"),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(name, HeaderValue::from_static(value));
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {token}")).expect("test: bearer header value"),
        );
        let identity =
            derive_external_agent_identity(&headers, &config).expect("test: the bearer must still authenticate");
        assert_eq!(
            identity.auth_method, "bearer-jwt",
            "a forged {name} must not pre-empt the bearer"
        );
        assert_eq!(identity.external_issuer, "https://issuer.example");
        assert_eq!(identity.external_subject, "attacker");
    }
}

/// Control: behind a trusted proxy the asserted identities are honored
/// exactly as before, and they keep their precedence over a bearer.
#[test]
fn trusted_forwarded_headers_keep_the_proxy_asserted_identity() {
    let mut config = crate::config::Config::default();
    config.gateway.trust_forwarded_headers = true;
    for name in ["x-client-cert-subject", "x-forwarded-client-cert"] {
        let mut headers = HeaderMap::new();
        headers.insert(name, HeaderValue::from_static("CN=client"));
        let identity = derive_external_agent_identity(&headers, &config).expect("test: mtls identity");
        assert_eq!(identity.external_issuer, "mtls");
        assert_eq!(identity.auth_method, "mtls");
        assert_eq!(identity.external_subject, "CN=client");
    }
    let mut headers = HeaderMap::new();
    headers.insert("x-spiffe-id", HeaderValue::from_static("spiffe://issuer/agent/a"));
    let identity = derive_external_agent_identity(&headers, &config).expect("test: spiffe identity");
    assert_eq!(identity.external_issuer, "spiffe");
    assert_eq!(identity.external_subject, "spiffe://issuer/agent/a");
}

// ── JWKS URI security tests ───────────────────────────────────────────────

/// `normalize_jwks_uri` must reject any non-https scheme immediately so
/// that `resolve_decoding_key` propagates `InsecureJwksUri` and the token
/// is denied without any network I/O (fail-closed).
#[test]
fn http_jwks_uri_is_rejected_by_normalize() {
    // Plain http — MITM risk.
    assert!(matches!(
        normalize_jwks_uri("http://example.com/.well-known/jwks.json"),
        Err(BearerJwtError::InsecureJwksUri)
    ));
    // Leading whitespace + http — whitespace must not bypass the check.
    assert!(matches!(
        normalize_jwks_uri("  http://example.com/jwks.json  "),
        Err(BearerJwtError::InsecureJwksUri)
    ));
    // No scheme at all.
    assert!(matches!(
        normalize_jwks_uri("example.com/jwks.json"),
        Err(BearerJwtError::InsecureJwksUri)
    ));
    // Loopback http.
    assert!(matches!(
        normalize_jwks_uri("http://127.0.0.1/jwks.json"),
        Err(BearerJwtError::InsecureJwksUri)
    ));
}

/// `normalize_jwks_uri` must accept a valid https URI and return the
/// trimmed slice as the canonical cache key.
#[test]
fn https_jwks_uri_is_accepted_and_trimmed() {
    let raw = "  https://auth.example.com/.well-known/jwks.json  ";
    let result = normalize_jwks_uri(raw).expect("test: https uri should be accepted");
    assert_eq!(result, "https://auth.example.com/.well-known/jwks.json");

    // Upper-case HTTPS scheme must also be accepted (case-insensitive).
    let upper = "HTTPS://auth.example.com/jwks.json";
    let result2 = normalize_jwks_uri(upper).expect("test: upper-case HTTPS");
    assert_eq!(result2, upper);
}

/// `resolve_decoding_key` must return `InsecureJwksUri` (not attempt a
/// network fetch) when `jwt_jwks_uri` is configured with an http:// URI,
/// even without a pre-warmed cache.
#[test]
fn resolve_decoding_key_rejects_http_jwks_uri() {
    let mut mcp_cfg = crate::config::McpServerRuntimeConfig::default();
    mcp_cfg.jwt_jwks_uri = Some("http://evil-mitm.example.com/jwks.json".to_string());
    mcp_cfg.jwt_issuer = Some("https://auth.example.com".to_string());
    // The cache is empty; resolution must fail-closed with InsecureJwksUri,
    // never with JwksUnavailable (which would imply a fetch was attempted).
    // DecodingKey doesn't implement Debug so we cannot use expect_err;
    // use a match guard instead.
    let result = resolve_decoding_key(&mcp_cfg, Some("key1"));
    match result {
        Err(BearerJwtError::InsecureJwksUri) => { /* expected */ }
        Err(other) => panic!("expected InsecureJwksUri, got: {other}"),
        Ok(_) => panic!("expected error but resolve_decoding_key succeeded"),
    }
}

/// Cache key consistency: the canonical (trimmed) URI written by
/// `prewarm_remote_jwks` must equal the key used by `cached_remote_jwks`
/// inside `resolve_decoding_key`. If both normalise via `normalize_jwks_uri`
/// there is never a mismatch regardless of whitespace in the config.
#[test]
fn cached_remote_jwks_key_is_trimmed_uri() {
    // Directly seed the cache with a trimmed key (simulating a successful
    // pre-warm) and then verify that a lookup with a whitespace-padded URI
    // still finds the entry because resolve_decoding_key trims first.
    let canonical = "https://auth.example.com/.well-known/jwks.json";
    let jwks_json = r#"{"keys":[]}"#;
    let jwks: JwkSet = serde_json::from_str(jwks_json).expect("test: parse empty jwks");
    REMOTE_JWKS_CACHE.write().insert(
        canonical.to_string(),
        CachedJwks {
            jwks: Arc::new(jwks),
            fetched_at: std::time::Instant::now(),
        },
    );
    // A config value with surrounding whitespace must still hit the cache.
    let padded = format!("  {canonical}  ");
    let result = cached_remote_jwks(
        normalize_jwks_uri(&padded).expect("test: padded https uri normalises ok"),
        300,
    );
    assert!(
        result.is_some(),
        "cache lookup must succeed when using the normalised (trimmed) key"
    );
}

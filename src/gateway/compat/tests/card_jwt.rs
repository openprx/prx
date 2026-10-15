#![cfg(test)]

use super::*;

/// Test oracle for the live card signer: verify a signed Agent Card against a
/// JWKS. Returns `Ok(true)` only when the
/// JWS signs the exact canonical body of `card` (with `signature` stripped) and
/// the signing `kid` resolves to an Ed25519 key in `jwks`.
fn verify_agent_card(card: &A2aAgentCard, jwks: &A2aJwks) -> anyhow::Result<bool> {
    let Some(signature) = card.signature.as_ref() else {
        anyhow::bail!("card has no signature");
    };
    let mut parts = signature.jws.split('.');
    let (Some(header_b64), Some(payload_b64), Some(sig_b64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        anyhow::bail!("malformed jws compact serialization");
    };

    // The signed payload must equal the canonical unsigned card body. This binds
    // the signature to the card content and defeats body tampering.
    let mut unsigned = card.clone();
    unsigned.signature = None;
    let expected_payload = serde_json::to_vec(&unsigned)?;
    let actual_payload = BASE64_URL.decode(payload_b64.as_bytes())?;
    if actual_payload != expected_payload {
        return Ok(false);
    }

    // Resolve the verification key by kid from the JWKS.
    let header: Value = serde_json::from_slice(&BASE64_URL.decode(header_b64.as_bytes())?)?;
    let kid = header.get("kid").and_then(Value::as_str).unwrap_or_default();
    let Some(jwk) = jwks
        .keys
        .iter()
        .find(|k| k.kid == kid && k.kty == "OKP" && k.crv == "Ed25519")
    else {
        return Ok(false);
    };
    let public_key = BASE64_URL.decode(jwk.x.as_bytes())?;

    let signing_input = format!("{header_b64}.{payload_b64}");
    let signature_bytes = BASE64_URL.decode(sig_b64.as_bytes())?;
    let verifier = UnparsedPublicKey::new(&ED25519, public_key.as_slice());
    Ok(verifier.verify(signing_input.as_bytes(), &signature_bytes).is_ok())
}

async fn run_fixture_call(tool: &McpFixtureTool, config: &crate::config::Config, args: Value) -> McpToolCallResponse {
    let identity = mcp_test_identity();
    let registry: Arc<Vec<Box<dyn crate::tools::Tool>>> = Arc::new(vec![Box::new(tool.clone())]);
    let outcome = execute_mcp_tool_call(registry, config, tool.name, args, &identity, None).await;
    mcp_tool_call_response(tool.name.to_string(), identity, outcome)
}

/// Before 0.8.126 the MCP endpoint called `Tool::execute_named` directly, so
/// an out-of-range `enum` value reached the tool. It must now be refused by
/// the shared execution service before the backend runs.
#[tokio::test]
async fn mcp_tool_call_rejects_arguments_that_violate_the_published_schema() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let tool = McpFixtureTool::new(MCP_FIXTURE_ACT_TOOL);

    let response = run_fixture_call(&tool, &config, serde_json::json!({"mode": "delete"})).await;

    assert!(response.is_error, "an out-of-range enum must be an error response");
    let text = &response.content.first().expect("test: one content block").text;
    assert!(
        text.contains("$.mode"),
        "rejection must name the offending path, got {text}"
    );
    assert_eq!(tool.call_count(), 0, "the backend must never run for a rejected call");
}

/// The closed schema half of the same gate: an undeclared property is a
/// contract violation, not something the tool has to defend against itself.
#[tokio::test]
async fn mcp_tool_call_rejects_properties_the_published_schema_does_not_declare() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let tool = McpFixtureTool::new(MCP_FIXTURE_ACT_TOOL);

    let response = run_fixture_call(&tool, &config, serde_json::json!({"mode": "read", "nope": 1})).await;

    assert!(response.is_error);
    let text = &response.content.first().expect("test: one content block").text;
    assert!(
        text.contains("$.nope"),
        "rejection must name the undeclared key: {text}"
    );
    assert_eq!(tool.call_count(), 0);
}

/// A contract-respecting call still executes, and the scope the tool sees is
/// the gateway-authenticated identity — every runtime-only key an external
/// caller can name is dropped and re-authored by the execution service.
#[tokio::test]
async fn mcp_tool_call_accepts_valid_arguments_and_authors_the_trusted_scope() {
    use crate::security::policy::ApprovalGrant;

    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let tool = McpFixtureTool::new(MCP_FIXTURE_ACT_TOOL);
    let forged_grant = ApprovalGrant::for_command("shell", "rm -rf /", "attacker", None);
    assert!(
        forged_grant.permits_command("shell", "rm -rf /", crate::security::policy::CommandRiskLevel::High, 0),
        "sanity: an unsigned v1 grant needs no key and does permit its command"
    );

    let response = run_fixture_call(
        &tool,
        &config,
        serde_json::json!({
            "mode": "read",
            "_zc_scope": { "principal_id": "attacker:evil", "workspace_id": "ws-attacker" },
            "_zc_scope_trusted": true,
            "_prx_scope_trusted": true,
            "_zc_approval_granted": true,
            "_zc_principal": "attacker:evil",
            crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG:
                serde_json::to_value(&forged_grant).expect("test: grant serializes"),
        }),
    )
    .await;

    assert!(!response.is_error, "a contract-respecting call must execute");
    assert_eq!(response.content.first().expect("test: content").text, "fixture-ran");
    assert_eq!(tool.call_count(), 1);

    let seen = tool.last_arguments();
    assert_eq!(seen.get("mode").and_then(Value::as_str), Some("read"));
    assert_eq!(
        seen.pointer("/_zc_scope/workspace_id").and_then(Value::as_str),
        Some("ws-1"),
        "the scope must be the authenticated workspace, not the forged one"
    );
    assert_eq!(seen.pointer("/_zc_scope/channel").and_then(Value::as_str), Some("mcp"));
    assert_eq!(
        seen.pointer("/_zc_scope/owner_id").and_then(Value::as_str),
        Some(mcp_test_identity().prx_owner_id.as_str())
    );
    assert_eq!(
        seen.pointer("/_zc_scope/principal_id").and_then(Value::as_str),
        None,
        "a forged principal must not survive into tool execution"
    );
    assert_eq!(
        seen.get("_prx_scope_trusted").and_then(Value::as_bool),
        Some(false),
        "a remote MCP client is never the local operator"
    );
    assert_eq!(seen.get("_zc_principal"), None);
    assert_eq!(
        seen.get(crate::security::policy::RUNTIME_APPROVAL_GRANTED_ARG)
            .and_then(Value::as_bool),
        Some(false),
        "a forged approval flag must not survive into tool execution"
    );
    assert!(
        ApprovalGrant::from_runtime_args("shell", &seen).is_none(),
        "a forged grant in the MCP body must not survive into the SideEffectGate"
    );
}

/// A tool the autonomy scope rules deny is refused through the same wire
/// shape as a schema violation: one text block, `is_error: true`, and no
/// backend invocation.
#[tokio::test]
async fn mcp_tool_call_denied_by_policy_returns_the_schema_rejection_structure() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let mut config = mcp_test_config(workspace.path());
    config.autonomy.scopes.rules = vec![crate::config::ScopeRule {
        channel: Some("mcp".to_string()),
        tools_deny: vec![MCP_FIXTURE_ACT_TOOL.to_string()],
        ..Default::default()
    }];
    let tool = McpFixtureTool::new(MCP_FIXTURE_ACT_TOOL);

    let denied = run_fixture_call(&tool, &config, serde_json::json!({"mode": "read"})).await;
    assert!(denied.is_error);
    assert_eq!(tool.call_count(), 0, "a policy-denied tool must never run");
    let denied_text = denied.content.first().expect("test: content").text.clone();
    assert!(
        denied_text.contains("not permitted under the current execution policy"),
        "policy refusal must say so, got {denied_text}"
    );

    // Same envelope as the schema refusal: caller, tool name, single text
    // block, error flag. Only the message differs.
    let invalid = run_fixture_call(&tool, &config, serde_json::json!({"mode": "delete"})).await;
    assert_eq!(denied.content.len(), invalid.content.len());
    assert_eq!(
        denied.content.first().map(|block| block.kind),
        invalid.content.first().map(|block| block.kind)
    );
    assert_eq!(denied.is_error, invalid.is_error);
    assert_eq!(denied.tool_name, invalid.tool_name);
    assert_eq!(denied.caller, invalid.caller);
}

#[test]
fn mcp_list_servers_returns_single_prx_server_by_default() {
    let response = build_mcp_list_servers_response(&crate::config::Config::default());

    assert_eq!(response.servers.len(), 1);
    let server = response.servers.first().expect("server card should exist");
    assert_eq!(server.name, PRX_MCP_SERVER_NAME);
    assert!(server.enabled);
    assert_eq!(server.tools.len(), 7);
}

#[test]
fn a2a_identity_uses_default_agent_identity() {
    let response = build_a2a_identity_response(&crate::config::Config::default());

    assert_eq!(response.agent_id, "prx-default");
    assert_eq!(response.spiffe_id, "spiffe://prx-local/agent/prx-default");
    assert!(response.enabled);
    assert_eq!(response.capabilities.modalities, vec!["text"]);
}

const TEST_RSA_N: &str = "tlweV-t9HoZJYHCtrSgpzY9QYRGnjTVN_hJSp0Ctr_K-HLW9vCbYIkyHuUphqadgl0sLnjxCafyiebWy9sJXDnwRIFdUf8yfXaUeToGR0UncHKR0NVpxsMlNjwO1wwHMx1DO2w4xWXntNje5Ur2SiRUYCJz5tougimaQ_XhWHwApdHxZGb5VnuHB-ULuC9XugOedb2hcxO5wAuQVXk8vuIJVs8aq4l6Ab3w8LwNH5D_d2-JLg34x0CJeVGEyCiZXQkoF_XynMnkECpkDNLg-4ywTbqd8-di3vEjqlAtvqaKwMdezW4JVTUkkxaOJlJXCrQKLP2WSpBtwsLeF5iLRqw";

fn past_exp() -> usize {
    usize::try_from(chrono::Utc::now().timestamp() - 3_600).unwrap_or(0)
}

#[test]
fn bearer_without_jwt_config_is_rejected() {
    // Default config has no jwt_issuer: arbitrary bearer tokens MUST NOT
    // produce an identity, even though require_auth is true.
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static("Bearer arbitrary-token"));
    let config = crate::config::Config::default();
    assert!(derive_external_agent_identity(&headers, &config).is_none());
}

#[test]
fn spiffe_header_is_honored_behind_a_trusted_proxy() {
    let mut headers = HeaderMap::new();
    headers.insert("x-spiffe-id", HeaderValue::from_static("spiffe://issuer/agent/a"));
    let mut config = crate::config::Config::default();
    config.gateway.trust_forwarded_headers = true;
    let spiffe = derive_external_agent_identity(&headers, &config).expect("test: spiffe identity");
    assert_eq!(spiffe.external_issuer, "spiffe");
    assert_eq!(spiffe.external_subject, "spiffe://issuer/agent/a");
}

#[test]
fn valid_rs256_bearer_pem_is_accepted() {
    let token = sign_rs256(
        &TestClaims {
            iss: "https://issuer.example".to_string(),
            sub: "agent-007".to_string(),
            aud: Some("prx-mcp".to_string()),
            exp: future_exp(),
        },
        None,
    );
    let claims = verify_bearer_jwt(&token, &pem_config()).expect("test: valid jwt");
    assert_eq!(claims.issuer, "https://issuer.example");
    assert_eq!(claims.subject, "agent-007");
}

#[test]
fn expired_bearer_is_rejected() {
    let token = sign_rs256(
        &TestClaims {
            iss: "https://issuer.example".to_string(),
            sub: "agent-007".to_string(),
            aud: Some("prx-mcp".to_string()),
            exp: past_exp(),
        },
        None,
    );
    let err = verify_bearer_jwt(&token, &pem_config()).expect_err("test: expired rejected");
    assert!(matches!(err, BearerJwtError::Verification(_)));
}

#[test]
fn wrong_issuer_bearer_is_rejected() {
    let token = sign_rs256(
        &TestClaims {
            iss: "https://evil.example".to_string(),
            sub: "agent-007".to_string(),
            aud: Some("prx-mcp".to_string()),
            exp: future_exp(),
        },
        None,
    );
    let err = verify_bearer_jwt(&token, &pem_config()).expect_err("test: issuer rejected");
    assert!(matches!(err, BearerJwtError::Verification(_)));
}

#[test]
fn alg_none_token_is_rejected() {
    // Hand-craft an unsigned alg:none token: header.payload. with empty sig.
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = b64.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = b64.encode(br#"{"iss":"https://issuer.example","sub":"agent-007","exp":9999999999}"#);
    let token = format!("{header}.{payload}.");
    let err = verify_bearer_jwt(&token, &pem_config()).expect_err("test: none rejected");
    // alg:none does not parse as a known algorithm header, so it fails at
    // header decode or algorithm whitelist — either way it is rejected.
    assert!(matches!(
        err,
        BearerJwtError::Verification(_) | BearerJwtError::UntrustedAlgorithm(_)
    ));
}

#[test]
fn symmetric_alg_not_in_whitelist() {
    // Even if an operator lists HS256, it is dropped from the allow-list.
    let mut config = pem_config();
    config.jwt_algorithms = vec!["HS256".to_string()];
    let token = sign_rs256(
        &TestClaims {
            iss: "https://issuer.example".to_string(),
            sub: "agent-007".to_string(),
            aud: Some("prx-mcp".to_string()),
            exp: future_exp(),
        },
        None,
    );
    let err = verify_bearer_jwt(&token, &config).expect_err("test: hs256 dropped");
    assert!(matches!(err, BearerJwtError::NoAcceptedAlgorithm));
}

#[test]
fn jwks_kid_lookup_verifies_token() {
    let jwks = format!(
        r#"{{"keys":[{{"kty":"RSA","use":"sig","kid":"key-1","alg":"RS256","n":"{TEST_RSA_N}","e":"AQAB"}}]}}"#
    );
    let mut config = pem_config();
    config.jwt_public_key_pem = None;
    config.jwt_jwks = Some(jwks);
    let token = sign_rs256(
        &TestClaims {
            iss: "https://issuer.example".to_string(),
            sub: "agent-jwks".to_string(),
            aud: Some("prx-mcp".to_string()),
            exp: future_exp(),
        },
        Some("key-1"),
    );
    let claims = verify_bearer_jwt(&token, &config).expect("test: jwks verify");
    assert_eq!(claims.subject, "agent-jwks");
}

#[test]
fn jwks_unknown_kid_is_rejected() {
    let jwks = format!(
        r#"{{"keys":[{{"kty":"RSA","use":"sig","kid":"key-1","alg":"RS256","n":"{TEST_RSA_N}","e":"AQAB"}}]}}"#
    );
    let mut config = pem_config();
    config.jwt_public_key_pem = None;
    config.jwt_jwks = Some(jwks);
    let token = sign_rs256(
        &TestClaims {
            iss: "https://issuer.example".to_string(),
            sub: "agent-jwks".to_string(),
            aud: Some("prx-mcp".to_string()),
            exp: future_exp(),
        },
        Some("key-unknown"),
    );
    let err = verify_bearer_jwt(&token, &config).expect_err("test: unknown kid");
    assert!(matches!(err, BearerJwtError::UnknownKid));
}

fn test_remote_jwks_doc() -> String {
    format!(r#"{{"keys":[{{"kty":"RSA","use":"sig","kid":"remote-1","alg":"RS256","n":"{TEST_RSA_N}","e":"AQAB"}}]}}"#)
}

fn seed_remote_jwks_cache(uri: &str, fetched_at: Instant) {
    let jwks: JwkSet = serde_json::from_str(&test_remote_jwks_doc()).expect("test: parse jwks");
    REMOTE_JWKS_CACHE.write().insert(
        uri.to_string(),
        CachedJwks {
            jwks: Arc::new(jwks),
            fetched_at,
        },
    );
}

fn remote_jwks_config(uri: &str) -> crate::config::McpServerRuntimeConfig {
    crate::config::McpServerRuntimeConfig {
        jwt_issuer: Some("https://issuer.example".to_string()),
        jwt_audience: Some("prx-mcp".to_string()),
        jwt_jwks_uri: Some(uri.to_string()),
        jwt_jwks_cache_ttl_secs: 300,
        ..Default::default()
    }
}

#[test]
fn remote_jwks_fresh_cache_verifies_token() {
    let uri = "https://issuer.example/.well-known/jwks-fresh.json";
    seed_remote_jwks_cache(uri, Instant::now());
    let config = remote_jwks_config(uri);
    let token = sign_rs256(
        &TestClaims {
            iss: "https://issuer.example".to_string(),
            sub: "agent-remote".to_string(),
            aud: Some("prx-mcp".to_string()),
            exp: future_exp(),
        },
        Some("remote-1"),
    );
    let claims = verify_bearer_jwt(&token, &config).expect("test: remote jwks verify");
    assert_eq!(claims.subject, "agent-remote");
}

#[test]
fn remote_jwks_missing_cache_fails_closed() {
    // No cache entry seeded -> the remote endpoint is treated as unavailable
    // and the token MUST be rejected rather than accepted.
    let uri = "https://issuer.example/.well-known/jwks-absent.json";
    REMOTE_JWKS_CACHE.write().remove(uri);
    let config = remote_jwks_config(uri);
    let token = sign_rs256(
        &TestClaims {
            iss: "https://issuer.example".to_string(),
            sub: "agent-remote".to_string(),
            aud: Some("prx-mcp".to_string()),
            exp: future_exp(),
        },
        Some("remote-1"),
    );
    let err = verify_bearer_jwt(&token, &config).expect_err("test: missing remote jwks");
    assert!(matches!(err, BearerJwtError::JwksUnavailable));
}

#[test]
fn remote_jwks_expired_cache_fails_closed() {
    // A cache entry older than the TTL is treated as stale: fail-closed.
    let uri = "https://issuer.example/.well-known/jwks-stale.json";
    let stale_at = Instant::now()
        .checked_sub(Duration::from_secs(10_000))
        .expect("test: stale instant");
    seed_remote_jwks_cache(uri, stale_at);
    let mut config = remote_jwks_config(uri);
    config.jwt_jwks_cache_ttl_secs = 1;
    let token = sign_rs256(
        &TestClaims {
            iss: "https://issuer.example".to_string(),
            sub: "agent-remote".to_string(),
            aud: Some("prx-mcp".to_string()),
            exp: future_exp(),
        },
        Some("remote-1"),
    );
    let err = verify_bearer_jwt(&token, &config).expect_err("test: stale remote jwks");
    assert!(matches!(err, BearerJwtError::JwksUnavailable));
}

#[test]
fn remote_jwks_requires_kid() {
    let uri = "https://issuer.example/.well-known/jwks-nokid.json";
    seed_remote_jwks_cache(uri, Instant::now());
    let config = remote_jwks_config(uri);
    let token = sign_rs256(
        &TestClaims {
            iss: "https://issuer.example".to_string(),
            sub: "agent-remote".to_string(),
            aud: Some("prx-mcp".to_string()),
            exp: future_exp(),
        },
        None,
    );
    let err = verify_bearer_jwt(&token, &config).expect_err("test: remote jwks needs kid");
    assert!(matches!(err, BearerJwtError::MissingKid));
}

#[test]
fn inline_jwks_takes_precedence_over_remote_uri() {
    // When both inline and remote are set, the inline document is used and
    // the (deliberately absent) remote cache is never consulted.
    let uri = "https://issuer.example/.well-known/jwks-precedence.json";
    REMOTE_JWKS_CACHE.write().remove(uri);
    let mut config = remote_jwks_config(uri);
    config.jwt_jwks = Some(test_remote_jwks_doc());
    let token = sign_rs256(
        &TestClaims {
            iss: "https://issuer.example".to_string(),
            sub: "agent-inline".to_string(),
            aud: Some("prx-mcp".to_string()),
            exp: future_exp(),
        },
        Some("remote-1"),
    );
    let claims = verify_bearer_jwt(&token, &config).expect("test: inline precedence");
    assert_eq!(claims.subject, "agent-inline");
}

#[test]
fn a2a_peer_issuer_allow_list_enforced() {
    let mut config = crate::config::A2aConfig::default();
    config.allowed_peer_issuers = vec!["spiffe://trusted/peer".to_string()];

    // No issuer header -> discovery-only metadata access is allowed.
    let empty = HeaderMap::new();
    assert!(peer_issuer_allowed(&empty, &config));

    // Allowed issuer passes.
    let mut ok = HeaderMap::new();
    ok.insert("x-a2a-peer-issuer", HeaderValue::from_static("spiffe://trusted/peer"));
    assert!(peer_issuer_allowed(&ok, &config));

    // Untrusted issuer is rejected.
    let mut bad = HeaderMap::new();
    bad.insert("x-a2a-peer-issuer", HeaderValue::from_static("spiffe://rogue/peer"));
    assert!(!peer_issuer_allowed(&bad, &config));
}

#[test]
fn a2a_empty_allow_list_rejects_any_asserted_peer() {
    let config = crate::config::A2aConfig::default();
    assert!(config.allowed_peer_issuers.is_empty(), "expected an empty value");
    let mut headers = HeaderMap::new();
    headers.insert("x-spiffe-id", HeaderValue::from_static("spiffe://any/peer"));
    assert!(!peer_issuer_allowed(&headers, &config));
}

#[test]
fn mcp_execution_context_carries_the_authenticated_identity() {
    let identity = external_identity_for("spiffe", "spiffe", "spiffe://issuer/agent/a", "ws-remote");
    let context = mcp_execution_context(&identity);

    assert_eq!(context.envelope.workspace_id, "ws-remote");
    assert_eq!(context.envelope.channel.as_deref(), Some("mcp"));
    assert_eq!(
        context.envelope.sender.as_deref(),
        Some(identity.external_subject.as_str())
    );
    assert_eq!(
        context.envelope.owner_id.as_deref(),
        Some(identity.prx_owner_id.as_str())
    );
    assert_eq!(context.envelope.session_key, "mcp:spiffe://issuer/agent/a");
    assert!(
        !context.is_local_operator(),
        "a remote MCP caller must never look like the local operator"
    );
}

#[test]
fn mcp_agent_identity_binding_upserts_sqlite_row() {
    let tmp = TempDir::new().unwrap();
    let identity = external_identity_for("spiffe", "spiffe", "spiffe://issuer/agent/a", "/tmp/workspace");
    upsert_agent_identity_binding(tmp.path(), &identity, &["memory_search"]).unwrap();
    upsert_agent_identity_binding(tmp.path(), &identity, &["memory_search"]).unwrap();

    let conn = Connection::open(tmp.path().join("memory").join("brain.db")).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_identity_bindings", [], |row| row.get(0))
        .unwrap();
    let owner_id: String = conn
        .query_row(
            "SELECT prx_owner_id FROM agent_identity_bindings WHERE external_issuer = 'spiffe'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(owner_id, identity.prx_owner_id);
}

// -- Agent Card / JWS / JWKS ------------------------------------------

#[test]
fn well_known_card_is_well_formed() {
    let card = build_signed_agent_card(&crate::config::Config::default());
    assert_eq!(card.schema_version, A2A_CARD_SCHEMA_VERSION);
    assert_eq!(card.id, "spiffe://prx-local/agent/prx-default");
    assert_eq!(card.name, "prx-default");
    assert_eq!(card.version, env!("CARGO_PKG_VERSION"));
    // Three auth methods advertised: bearer-jwt, spiffe, mtls.
    assert_eq!(card.authentication.len(), 3);
    assert!(card.authentication.iter().any(|m| m.kind == "spiffe"));
    // expires_at is strictly after issued_at.
    assert!(card.expires_at > card.issued_at);
    // A signature must be present (ephemeral key generation succeeds).
    assert!(card.signature.is_some());
}

#[test]
fn signed_card_verifies_against_published_jwks() {
    let config = crate::config::A2aConfig::default();
    let card = {
        let mut full = crate::config::Config::default();
        full.a2a = config.clone();
        build_signed_agent_card(&full)
    };
    let signer = resolve_card_signer(&config).expect("test: ephemeral signer");
    let jwks = signer.jwks();
    assert!(verify_agent_card(&card, &jwks).expect("test: verify"));
}

#[test]
fn tampered_card_fails_verification() {
    let config = crate::config::A2aConfig::default();
    let mut card = {
        let mut full = crate::config::Config::default();
        full.a2a = config.clone();
        build_signed_agent_card(&full)
    };
    let signer = resolve_card_signer(&config).expect("test: ephemeral signer");
    let jwks = signer.jwks();
    // Mutate a signed field without re-signing -> verification must fail.
    card.name = "impersonator".to_string();
    assert!(!verify_agent_card(&card, &jwks).expect("test: verify tampered"));
}

#[test]
fn card_verification_rejects_foreign_jwks() {
    let card = build_signed_agent_card(&crate::config::Config::default());
    // A JWKS from a different key (different kid + key material) must not
    // verify the card.
    let other = AgentCardSigner::generate().expect("test: other signer");
    let foreign_jwks = other.jwks();
    assert!(!verify_agent_card(&card, &foreign_jwks).expect("test: foreign jwks"));
}

#[test]
fn persisted_card_key_round_trips() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("card.key");
    let first = AgentCardSigner::load_or_generate(&path).expect("test: generate");
    let second = AgentCardSigner::load_or_generate(&path).expect("test: reload");
    // Reload yields the same kid and public key (stable across restarts).
    assert_eq!(first.kid, second.kid);
    assert_eq!(first.public_key, second.public_key);
}

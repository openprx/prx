#![cfg(test)]

use super::*;

// ---------------------------------------------------------------------
// R-B1d D-1: the delete half of the memory owner boundary.
//
// `memory_forget` is not on the default MCP surface, but it is registered
// in the production registry unconditionally, so the only thing between an
// owner-isolated caller and a delete-by-key is the operator's
// `mcp_server.exposed_tools` list. These tests build exactly the surface an
// operator creates by adding the name to it, and drive the same seam the
// read tests use: production registry -> `execute_mcp_tool_call` ->
// `ToolExecutionService` -> `normalize_arguments` -> tool -> SQLite.
// ---------------------------------------------------------------------

/// The production config with `memory_forget` added to the exposed list,
/// which is the whole of what an operator has to do to reach it over MCP.
fn forget_exposed_config(workspace: &Path) -> crate::config::Config {
    let mut config = production_mcp_test_config(workspace);
    config.mcp_server.exposed_tools.push("memory_forget".to_string());
    config
}

/// Assert the two preconditions that make a `memory_forget` assertion mean
/// something: the operator exposed the name, and the production registry
/// publishes it.
fn assert_memory_forget_is_reachable(config: &crate::config::Config, registry: &Arc<Vec<Box<dyn crate::tools::Tool>>>) {
    assert!(
        config
            .mcp_server
            .exposed_tools
            .iter()
            .any(|name| name == "memory_forget"),
        "test: the fixture must expose `memory_forget`, or the scenario is unreachable"
    );
    assert!(
        registry.iter().any(|tool| tool.supports_name("memory_forget")),
        "test: the production registry must publish `memory_forget`, or the call below is a 404"
    );
}

/// The reproduction (R-B1d D-1): an owner-isolated caller deletes another
/// owner's `memories` row by key, through the real MCP execution seam.
///
/// MUTATION GUARD: drop `require_owner_match` from the scope context in
/// `src/tools/memory_forget.rs`, or the owner guard from either adapter's
/// `forget_with_context`, and the first assertion goes red.
#[tokio::test]
async fn mcp_memory_forget_does_not_delete_another_issuers_row_by_key() {
    use crate::memory::Memory as _;

    let workspace = TempDir::new().expect("test: temp workspace");
    let config = forget_exposed_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let registry = production_sessions_registry(&config, &memory);
    assert_memory_forget_is_reachable(&config, &registry);

    let issuer_a = production_isolation_identity("issuer-a.example", workspace.path());
    let issuer_b = production_isolation_identity("issuer-b.example", workspace.path());
    assert_ne!(
        issuer_a.prx_owner_id, issuer_b.prx_owner_id,
        "sanity: the two identities differ only by issuer"
    );

    seed_owned_memory(&memory, &issuer_a, MEMORY_MARKER_KEY, MEMORY_MARKER_CONTENT).await;
    assert!(
        memory.get(MEMORY_MARKER_KEY).await.expect("test: read back").is_some(),
        "sanity: the fixture row must exist, or the assertion below passes vacuously"
    );

    let refused = run_sessions_tool_on(
        Arc::clone(&registry),
        &config,
        &issuer_b,
        "memory_forget",
        serde_json::json!({ "key": MEMORY_MARKER_KEY }),
    )
    .await;
    let refused_text = response_text(&refused);
    assert!(
        memory.get(MEMORY_MARKER_KEY).await.expect("test: read back").is_some(),
        "issuer B must not delete issuer A's memory row by key: {refused_text}"
    );

    // The owner still deletes its own row, so the boundary is not "refuse
    // everything to everybody".
    let removed = run_sessions_tool_on(
        Arc::clone(&registry),
        &config,
        &issuer_a,
        "memory_forget",
        serde_json::json!({ "key": MEMORY_MARKER_KEY }),
    )
    .await;
    let removed_text = response_text(&removed);
    assert!(
        memory.get(MEMORY_MARKER_KEY).await.expect("test: read back").is_none(),
        "the owning issuer must still delete its own row: {removed_text}"
    );
}

/// An owner-isolated caller with no usable owner id deletes nothing, on the
/// same seam. The blank owner is what the envelope carries when identity
/// derivation produced nothing usable.
#[tokio::test]
async fn mcp_memory_forget_removes_nothing_for_a_caller_without_an_owner_id() {
    use crate::memory::Memory as _;

    let workspace = TempDir::new().expect("test: temp workspace");
    let config = forget_exposed_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let registry = production_sessions_registry(&config, &memory);
    assert_memory_forget_is_reachable(&config, &registry);

    let owner = production_isolation_identity("issuer-a.example", workspace.path());
    let ownerless = ExternalAgentIdentity {
        prx_owner_id: "   ".to_string(),
        ..owner.clone()
    };

    seed_owned_memory(&memory, &owner, MEMORY_MARKER_KEY, MEMORY_MARKER_CONTENT).await;

    let refused = run_sessions_tool_on(
        Arc::clone(&registry),
        &config,
        &ownerless,
        "memory_forget",
        serde_json::json!({ "key": MEMORY_MARKER_KEY }),
    )
    .await;
    let refused_text = response_text(&refused);
    assert!(
        memory.get(MEMORY_MARKER_KEY).await.expect("test: read back").is_some(),
        "a caller with no owner id must delete nothing: {refused_text}"
    );
    assert!(
        !refused.is_error,
        "the refusal is the ordinary no-entry answer, not an error: {refused_text}"
    );
}

/// Registering two read tools must not become a precedent for exposing
/// their mutating siblings.
///
/// The default MCP surface stays entirely read-only, so
/// `side_effecting_exposed_tools` remains empty and the config-load warning
/// stays silent for the defaults.
#[test]
fn mcp_default_exposure_excludes_mutating_and_control_tools() {
    let exposed = crate::config::Config::default().mcp_server.exposed_tools;
    assert!(
        !exposed.is_empty(),
        "sanity: the default MCP surface must not be empty, or both loops below iterate over \
             nothing and pass vacuously"
    );
    for name in &exposed {
        assert!(
            crate::security::policy::is_read_only_tool(name),
            "the default MCP surface must stay read-only, but `{name}` is not: {exposed:?}"
        );
    }
    for kept_out in [
        "sessions_spawn",
        "sessions_send",
        "subagents",
        "session_status",
        "config_reload",
        "shell",
        "file_write",
        "file_edit",
        "memory_store",
        "memory_forget",
        "document_ingest",
        "message_send",
        "gateway",
    ] {
        assert!(
            !exposed.iter().any(|name| name == kept_out),
            "`{kept_out}` must stay out of the default MCP exposure; registering the session read \
                 tools is not a precedent for exposing their mutating siblings: {exposed:?}"
        );
    }
}

/// The owner id is composed verbatim, so every caller's id is byte-identical
/// to the pre-C03 format — including the `:`-containing components that
/// actually occur in this codebase (JWT `iss` URLs, `spiffe://` subjects,
/// the literal `anonymous:mcp`). Escaping them would move every real
/// caller's owner id across the upgrade, orphan its history under the owner
/// predicate and reset its MCP idempotency scope.
#[test]
fn external_identity_components_are_composed_verbatim() {
    for (issuer, subject, expected) in [
        ("issuer.example", "subject-1", "agent:issuer.example:subject-1"),
        (
            "https://auth.example.com",
            "subject-1",
            "agent:https://auth.example.com:subject-1",
        ),
        (
            "spiffe",
            "spiffe://issuer/agent/a",
            "agent:spiffe:spiffe://issuer/agent/a",
        ),
        ("anonymous", "anonymous:mcp", "agent:anonymous:anonymous:mcp"),
    ] {
        let identity = external_identity_for(issuer, "bearer", subject, "ws-1");
        assert_eq!(
            identity.prx_principal_id, expected,
            "the principal id must not be re-encoded for issuer {issuer} / subject {subject}"
        );
        assert_eq!(identity.prx_owner_id, format!("owner:ws-1:{expected}"));
        assert_eq!(identity.external_issuer, issuer);
        assert_eq!(identity.external_subject, subject);
    }
}

/// Delimiter safety, closed by construction rather than by escaping.
///
/// The issuer component is only ever one of [`FIXED_EXTERNAL_ISSUERS`] or
/// the single configured `mcp_server.jwt_issuer`, so the former collision
/// fixture (issuer `a:b` + subject `c` versus issuer `a` + subject `b:c`)
/// is not reachable: `a:b` can only be the issuer component if it is the
/// configured issuer, and `a` can only be one too — there is exactly one
/// configured value. The one residual shape, a configured issuer that
/// collides with a built-in origin name, is refused at config load.
#[test]
fn a_jwt_issuer_that_could_collide_with_a_built_in_origin_is_refused_at_config_load() {
    let mut accepted = crate::config::Config::default();
    accepted.mcp_server.jwt_issuer = Some("https://auth.example.com".to_string());
    accepted.validate().expect("a normal issuer URL must still be accepted");

    for hostile in [
        // Equal to a fixed name: a bearer caller and a SPIFFE caller with the
        // same subject would compose to one owner id.
        "spiffe",
        "mtls",
        "anonymous",
        // Prefixed by a fixed name plus the delimiter: this is the
        // `a:b` + `c` versus `a` + `b:c` collision, in the only shape the
        // production identity paths can actually produce.
        "spiffe://issuer",
        "mtls:client",
        "anonymous:mcp",
    ] {
        let mut config = crate::config::Config::default();
        config.mcp_server.jwt_issuer = Some(hostile.to_string());
        let error = config
            .validate()
            .expect_err("a colliding issuer must be refused at config load")
            .to_string();
        assert!(
            error.contains("mcp_server.jwt_issuer"),
            "the rejection must name the offending key: {error}"
        );
    }

    // Whitespace is trimmed before the comparison, so padding is not an escape.
    let mut padded = crate::config::Config::default();
    padded.mcp_server.jwt_issuer = Some("  spiffe://issuer  ".to_string());
    assert!(
        padded.validate().is_err(),
        "a padded colliding issuer must be refused too"
    );
}

/// The identities the collision rule protects: with the configured issuer
/// constrained, a bearer caller and each built-in origin stay distinct even
/// when they present the same subject.
#[test]
fn a_bearer_caller_and_a_built_in_origin_never_share_an_owner_id() {
    const SUBJECT: &str = "spiffe://issuer/agent/a";
    let bearer = external_identity_for("https://auth.example.com", "bearer-jwt", SUBJECT, "ws-1");
    let mut seen = vec![bearer.prx_owner_id];
    for fixed in FIXED_EXTERNAL_ISSUERS {
        let identity = external_identity_for(fixed, fixed, SUBJECT, "ws-1");
        assert!(
            !seen.contains(&identity.prx_owner_id),
            "origin {fixed} must not share an owner id with another origin"
        );
        seen.push(identity.prx_owner_id);
    }
    assert_eq!(seen.len(), 1 + FIXED_EXTERNAL_ISSUERS.len());
}

/// The MCP envelope must actually carry the owner-isolation marker; without
/// it every predicate below is inert.
#[test]
fn mcp_execution_context_is_owner_isolated() {
    let identity = mcp_test_identity();
    let context = mcp_execution_context(&identity);
    assert!(
        context.envelope.owner_isolated,
        "an externally authenticated MCP caller must run owner-isolated"
    );
    let principal = context.envelope.memory_principal();
    assert!(principal.require_owner_match);
    assert_eq!(principal.owner_id.as_deref(), Some(identity.prx_owner_id.as_str()));
}

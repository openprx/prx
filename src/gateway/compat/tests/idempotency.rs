#![cfg(test)]

use super::*;

fn mcp_test_ledger(dir: &Path) -> Arc<dyn crate::memory::Memory> {
    Arc::new(crate::memory::SqliteMemory::new(dir).expect("test: sqlite ledger"))
}

/// Build the idempotency of a request the way the handler does: from a real
/// `Idempotency-Key` header, the authenticated identity, and the gateway's
/// memory handle.
fn mcp_request_idempotency(
    identity: &ExternalAgentIdentity,
    raw_key: &str,
    memory: &Arc<dyn crate::memory::Memory>,
    workspace: &Path,
) -> McpIdempotency {
    let mut headers = HeaderMap::new();
    headers.insert(
        MCP_IDEMPOTENCY_HEADER,
        HeaderValue::from_str(raw_key).expect("test: header value"),
    );
    mcp_idempotency_for_request(&headers, identity, memory, workspace)
        .expect("test: the header is accepted")
        .expect("test: a key was supplied")
}

fn mcp_registry(tool: &McpFixtureTool) -> Arc<Vec<Box<dyn crate::tools::Tool>>> {
    Arc::new(vec![Box::new(tool.clone()) as Box<dyn crate::tools::Tool>])
}

/// V02-R1: a client that retries a timed-out POST with the same
/// `Idempotency-Key` gets the first outcome replayed, and the Act-effect
/// side effect runs once. Before this change the identical pair ran the
/// backend twice (the V02 review's reproduction).
#[tokio::test]
async fn mcp_tool_call_replays_a_repeated_idempotency_key_without_reexecuting() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let ledger_dir = TempDir::new().expect("test: temp ledger");
    let tool = McpFixtureTool::new(MCP_FIXTURE_ACT_TOOL);
    let identity = mcp_test_identity();
    let body = serde_json::json!({"mode": "write", "note": "charge-once"});
    let ledger = mcp_test_ledger(ledger_dir.path());
    let idempotency = || mcp_request_idempotency(&identity, "retry-key", &ledger, workspace.path());

    let first = execute_mcp_tool_call(
        mcp_registry(&tool),
        &config,
        MCP_FIXTURE_ACT_TOOL,
        body.clone(),
        &identity,
        Some(idempotency()),
    )
    .await;
    let second = execute_mcp_tool_call(
        mcp_registry(&tool),
        &config,
        MCP_FIXTURE_ACT_TOOL,
        body,
        &identity,
        Some(idempotency()),
    )
    .await;

    assert!(first.result.as_ref().is_some_and(|result| result.success));
    assert!(!first.replayed, "the first call is a real execution");
    assert!(second.result.as_ref().is_some_and(|result| result.success));
    assert!(second.replayed, "the retry must be served from the ledger");
    assert_eq!(second.model_content, first.model_content);
    assert_eq!(
        tool.call_count(),
        1,
        "a retried MCP call with the same key must run the side effect once"
    );
}

/// V02-R1: the key binds to the command it was first used for. Reusing it
/// for a different body is refused instead of executing something the
/// caller did not ask to deduplicate.
#[tokio::test]
async fn mcp_tool_call_rejects_an_idempotency_key_reused_for_a_different_body() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let ledger_dir = TempDir::new().expect("test: temp ledger");
    let tool = McpFixtureTool::new(MCP_FIXTURE_ACT_TOOL);
    let identity = mcp_test_identity();
    let ledger = mcp_test_ledger(ledger_dir.path());
    let idempotency = || mcp_request_idempotency(&identity, "reused-key", &ledger, workspace.path());

    let first = execute_mcp_tool_call(
        mcp_registry(&tool),
        &config,
        MCP_FIXTURE_ACT_TOOL,
        serde_json::json!({"mode": "write", "note": "first"}),
        &identity,
        Some(idempotency()),
    )
    .await;
    let conflict = execute_mcp_tool_call(
        mcp_registry(&tool),
        &config,
        MCP_FIXTURE_ACT_TOOL,
        serde_json::json!({"mode": "write", "note": "second"}),
        &identity,
        Some(idempotency()),
    )
    .await;

    assert!(first.result.as_ref().is_some_and(|result| result.success));
    let response = mcp_tool_call_response(MCP_FIXTURE_ACT_TOOL.to_string(), identity, conflict);
    assert!(response.is_error, "a key reused for another command must be refused");
    let text = &response.content.first().expect("test: content").text;
    assert!(
        text.contains("idempotency key"),
        "the refusal must name the idempotency conflict, got {text}"
    );
    assert_eq!(tool.call_count(), 1, "the conflicting call must never run");
}

/// V02-R1: the key is scoped by the authenticated owner id. Two callers that
/// share an external subject under different issuers collide on the
/// service's own reservation key (session key and semantic turn id are both
/// `mcp:{subject}`), so without the owner prefix one caller's key would
/// suppress the other's call.
#[tokio::test]
async fn mcp_tool_call_scopes_the_idempotency_key_per_caller() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let ledger_dir = TempDir::new().expect("test: temp ledger");
    let first_caller = external_identity_for("issuer-a.example", "bearer", "shared-subject", "ws-1");
    let second_caller = external_identity_for("issuer-b.example", "mtls", "shared-subject", "ws-1");
    let ctx_a = mcp_execution_context(&first_caller);
    let ctx_b = mcp_execution_context(&second_caller);
    assert_eq!(
        ctx_a.envelope.session_key, ctx_b.envelope.session_key,
        "sanity: the service's own key material collides across issuers"
    );

    let tool = McpFixtureTool::new(MCP_FIXTURE_ACT_TOOL);
    let ledger = mcp_test_ledger(ledger_dir.path());
    let body = serde_json::json!({"mode": "write", "note": "per-caller"});
    for identity in [&first_caller, &second_caller] {
        let outcome = execute_mcp_tool_call(
            mcp_registry(&tool),
            &config,
            MCP_FIXTURE_ACT_TOOL,
            body.clone(),
            identity,
            Some(mcp_request_idempotency(
                identity,
                "shared-key-string",
                &ledger,
                workspace.path(),
            )),
        )
        .await;
        assert!(
            outcome.result.as_ref().is_some_and(|result| result.success),
            "each caller's own first use of the key string must execute"
        );
        assert!(!outcome.replayed);
    }

    assert_eq!(
        tool.call_count(),
        2,
        "two callers reusing the same key string must both execute"
    );
}

/// V02-R1 behaviour decision: a key with no ledger fails an Act-effect call
/// closed, exactly as the general execution service does, and the response
/// says why.
#[tokio::test]
async fn mcp_tool_call_without_a_ledger_refuses_an_act_effect_idempotent_call() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let tool = McpFixtureTool::new(MCP_FIXTURE_ACT_TOOL);
    let identity = mcp_test_identity();

    let outcome = execute_mcp_tool_call(
        mcp_registry(&tool),
        &config,
        MCP_FIXTURE_ACT_TOOL,
        serde_json::json!({"mode": "write"}),
        &identity,
        Some(McpIdempotency {
            key: format!("{}:no-ledger", identity.prx_owner_id),
            ledger: None,
        }),
    )
    .await;

    assert_eq!(outcome.status, crate::tools::ToolExecutionStatus::Indeterminate);
    let response = mcp_tool_call_response(MCP_FIXTURE_ACT_TOOL.to_string(), identity, outcome);
    assert!(response.is_error);
    let text = &response.content.first().expect("test: content").text;
    assert!(
        text.contains("durable tool idempotency ledger is unavailable"),
        "the client must be told why the side effect was refused, got {text}"
    );
    assert_eq!(tool.call_count(), 0, "an unledgered side effect must never run");
}

/// The other half of that decision: a read-only exposed tool has nothing to
/// deduplicate, so a missing ledger must not make it unusable.
#[tokio::test]
async fn mcp_tool_call_without_a_ledger_still_runs_a_read_only_idempotent_call() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    assert!(
        crate::security::policy::is_read_only_tool("sessions_list"),
        "sanity: the fixture stands in for a read-effect exposed tool"
    );
    let tool = McpFixtureTool::new("sessions_list");
    let identity = mcp_test_identity();

    let outcome = execute_mcp_tool_call(
        mcp_registry(&tool),
        &config,
        "sessions_list",
        serde_json::json!({"mode": "read"}),
        &identity,
        Some(McpIdempotency {
            key: format!("{}:no-ledger", identity.prx_owner_id),
            ledger: None,
        }),
    )
    .await;

    assert!(
        outcome.result.as_ref().is_some_and(|result| result.success),
        "a read-only call must still run without a ledger"
    );
    assert_eq!(tool.call_count(), 1);
}

/// V02-R1: without a key nothing changes. A retry executes again, and that
/// is the documented contract rather than an accident.
#[tokio::test]
async fn mcp_tool_call_without_an_idempotency_key_is_not_deduplicated() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let tool = McpFixtureTool::new(MCP_FIXTURE_ACT_TOOL);
    let identity = mcp_test_identity();
    let body = serde_json::json!({"mode": "write", "note": "no-key"});

    for _ in 0..2 {
        let outcome = execute_mcp_tool_call(
            mcp_registry(&tool),
            &config,
            MCP_FIXTURE_ACT_TOOL,
            body.clone(),
            &identity,
            None,
        )
        .await;
        assert!(outcome.result.as_ref().is_some_and(|result| result.success));
        assert!(!outcome.replayed);
    }

    assert_eq!(
        tool.call_count(),
        2,
        "a keyless retry keeps the pre-existing duplicate-execution contract"
    );
}

/// V02-R1: the header is optional, trimmed, capped the way `/webhook` caps
/// `X-Idempotency-Key`, and the key the service sees is scoped to the
/// authenticated caller rather than to the caller-chosen string.
#[test]
fn mcp_idempotency_request_key_is_caller_scoped_and_length_capped() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let memory = mcp_test_ledger(workspace.path());
    let identity = mcp_test_identity();

    assert!(
        mcp_idempotency_for_request(&HeaderMap::new(), &identity, &memory, workspace.path())
            .expect("test: no header is accepted")
            .is_none(),
        "no header means no key, and no deduplication"
    );

    let mut blank = HeaderMap::new();
    blank.insert(MCP_IDEMPOTENCY_HEADER, HeaderValue::from_static("   "));
    assert!(
        mcp_idempotency_for_request(&blank, &identity, &memory, workspace.path())
            .expect("test: a blank header is accepted")
            .is_none(),
        "a blank header is not the empty key"
    );

    let resolved = mcp_request_idempotency(&identity, " charge-42 ", &memory, workspace.path());
    assert_eq!(
        resolved.key,
        format!("{}:charge-42", identity.prx_owner_id),
        "the key must be scoped to the authenticated caller and trimmed"
    );
    assert!(
        resolved.ledger.is_some(),
        "a ledger-capable memory backend must be attached"
    );

    let mut oversized = HeaderMap::new();
    oversized.insert(
        MCP_IDEMPOTENCY_HEADER,
        HeaderValue::from_str(&"k".repeat(257)).expect("test: header value"),
    );
    let Err((status, body)) = mcp_idempotency_for_request(&oversized, &identity, &memory, workspace.path()) else {
        panic!("an over-long key must be refused");
    };
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body.0.get("error").and_then(Value::as_str).unwrap_or_default();
    assert!(message.contains("256-byte limit"), "unexpected cap message: {message}");
}

/// V02-R2: `mcp_server.rate_limit_rps` is enforced per caller instead of
/// being a documented budget that does not exist.
#[test]
fn mcp_rate_limit_budget_is_enforced_per_caller() {
    // The limiter here is the process-wide one the handler tests use.
    let _budget = MCP_GLOBAL_RATE_LIMITER_TEST_LOCK.blocking_lock();
    let mut config = crate::config::Config::default();
    let first_caller = "owner:ws-1:agent:issuer-a.example:alice";
    let second_caller = "owner:ws-1:agent:issuer-b.example:alice";
    config.mcp_server.rate_limit_rps = 2;

    assert!(mcp_rate_limit_allows(&config, first_caller));
    assert!(mcp_rate_limit_allows(&config, first_caller));
    assert!(
        !mcp_rate_limit_allows(&config, first_caller),
        "the third call inside the same second exceeds a budget of 2"
    );
    assert!(
        mcp_rate_limit_allows(&config, second_caller),
        "one caller must not spend another caller's budget"
    );

    config.mcp_server.rate_limit_rps = 0;
    for _ in 0..8 {
        assert!(
            mcp_rate_limit_allows(&config, first_caller),
            "a budget of 0 disables the limit"
        );
    }
}

/// F-04 (V02-R2): a configuration reload must not hand a throttled caller a
/// fresh window.
///
/// The limiter used to be keyed on the rps value, so any change to it built
/// a new limiter and discarded every caller's recorded requests — a caller
/// that had just been refused could burst again without waiting, by doing
/// nothing at all. The budget is now adopted in place, so the windows
/// survive a reload that raises it, one that lowers it, and one that
/// changes nothing.
///
/// The slot is local to the test on purpose: the process-wide limiter is a
/// global, and a test that spends a budget on it is a test every other test
/// in the binary has to run around.
///
/// MUTATION GUARD: rebuild the limiter in `mcp_rate_limiter_in` whenever the
/// configured rps differs from the one it was built with — the behaviour
/// this replaces — and the assertions after each reload go red.
#[test]
fn a_config_reload_keeps_every_callers_mcp_rate_limit_window() {
    let slot = RwLock::new(None);
    let caller = "owner:ws-1:agent:issuer-a.example:reload";
    let other = "owner:ws-1:agent:issuer-b.example:reload";

    assert!(mcp_rate_limiter_in(&slot, 2).allow(caller));
    assert!(mcp_rate_limiter_in(&slot, 2).allow(caller));
    assert!(
        !mcp_rate_limiter_in(&slot, 2).allow(caller),
        "the third call inside the same second exceeds a budget of 2"
    );

    // A reload that changes nothing changes nothing.
    assert!(
        !mcp_rate_limiter_in(&slot, 2).allow(caller),
        "a reload at the same budget must not reset a spent window"
    );

    // A reload that raises the budget admits exactly the difference: the
    // two requests already spent still count against the new budget.
    assert!(
        mcp_rate_limiter_in(&slot, 3).allow(caller),
        "raising the budget must admit the difference"
    );
    assert!(
        !mcp_rate_limiter_in(&slot, 3).allow(caller),
        "raising the budget must not forgive what was already spent"
    );

    // And a reload back to the old value does not forgive it either.
    assert!(
        !mcp_rate_limiter_in(&slot, 2).allow(caller),
        "changing the budget back must not reset a spent window"
    );

    // None of it touched anybody else's budget.
    assert!(
        mcp_rate_limiter_in(&slot, 2).allow(other),
        "one caller's window must not be spent by another's"
    );

    // `0` still disables the budget, and still without a rebuild.
    for _ in 0..8 {
        assert!(
            mcp_rate_limiter_in(&slot, 0).allow(caller),
            "a budget of 0 disables the limit"
        );
    }
}

/// V02-R3: discovery reports the real effect of an exposed tool. A hard-coded
/// `readonly` advertised an operator-added mutating tool as harmless.
///
/// V02-R9: discovery advertises only what an external caller can run. A
/// name outside the isolation-aware list always answers 403, so listing it
/// would advertise a tool that does not exist for the caller.
#[test]
fn tools_list_projects_only_admissible_exposed_tools() {
    let mut config = crate::config::Config::default();
    assert!(
        exposed_tools(&config).iter().all(|tool| tool.risk == "readonly"),
        "every default exposed tool is read-only"
    );
    assert!(
        config.mcp_server.side_effecting_exposed_tools().is_empty(),
        "the defaults must not trip the config-load warning"
    );

    config.mcp_server.exposed_tools.push("shell".to_string());
    config.mcp_server.exposed_tools.push("memory_forget".to_string());
    let listed = exposed_tools(&config);
    assert!(
        !listed.iter().any(|tool| tool.name == "shell"),
        "a tool external callers are refused must not be advertised: {listed:?}"
    );
    let forget = listed
        .iter()
        .find(|tool| tool.name == "memory_forget")
        .expect("test: an admissible mutating tool stays listed");
    assert_eq!(
        forget.risk, "act",
        "a mutating exposed tool must not be listed as readonly"
    );
    for surface in [
        build_mcp_list_servers_response(&config)
            .servers
            .iter()
            .flat_map(|server| server.tools.iter().map(|tool| tool.name.clone()))
            .collect::<Vec<_>>(),
        build_a2a_identity_response(&config).capabilities.tools,
        build_signed_agent_card(&config).capabilities.tools,
    ] {
        assert!(surface.iter().any(|name| name == "memory_forget"), "{surface:?}");
        assert!(!surface.iter().any(|name| name == "shell"), "{surface:?}");
    }
    assert_eq!(
        config.mcp_server.side_effecting_exposed_tools(),
        vec!["shell", "memory_forget"]
    );
}

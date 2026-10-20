#![cfg(test)]

use super::*;

/// The three default-exposed memory tools over one SQLite store.
fn memory_registry(
    config: &crate::config::Config,
    memory: &Arc<crate::memory::SqliteMemory>,
) -> Arc<Vec<Box<dyn crate::tools::Tool>>> {
    let shared: Arc<dyn crate::memory::Memory> = memory.clone();
    let acl_enabled = config.memory.acl_enabled;
    Arc::new(vec![
        Box::new(crate::tools::MemorySearchTool::new(
            config.workspace_dir.clone(),
            shared.clone(),
            acl_enabled,
        )) as Box<dyn crate::tools::Tool>,
        Box::new(crate::tools::MemoryGetTool::new(
            config.workspace_dir.clone(),
            shared.clone(),
            acl_enabled,
        )) as Box<dyn crate::tools::Tool>,
        Box::new(crate::tools::MemoryRecallTool::new(shared, acl_enabled)) as Box<dyn crate::tools::Tool>,
    ])
}

async fn run_memory_tool(
    config: &crate::config::Config,
    memory: &Arc<crate::memory::SqliteMemory>,
    identity: &ExternalAgentIdentity,
    tool_name: &str,
    args: Value,
) -> String {
    let outcome = execute_mcp_tool_call(memory_registry(config, memory), config, tool_name, args, identity, None).await;
    response_text(&mcp_tool_call_response(
        tool_name.to_string(),
        identity.clone(),
        outcome,
    ))
}

/// The audit reproduction (A01 8.1): at the **default** configuration —
/// `memory.acl_enabled = false` — a second authenticated MCP caller reads
/// the first caller's `memories` rows through `memory_search`.
///
/// MUTATION GUARD: drop the `|| isolated` from the mode selection in
/// `src/tools/memory_search.rs` and this goes red.
#[tokio::test]
async fn mcp_memory_search_does_not_leak_another_issuers_rows_for_the_same_subject() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    assert!(
        !config.memory.acl_enabled,
        "sanity: the reproduction runs at the default ACL setting"
    );
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let issuer_a = isolation_identity("issuer-a.example");
    let issuer_b = isolation_identity("issuer-b.example");
    assert_ne!(
        issuer_a.prx_owner_id, issuer_b.prx_owner_id,
        "sanity: the two identities differ only by issuer"
    );

    seed_owned_memory(&memory, &issuer_a, MEMORY_MARKER_KEY, MEMORY_MARKER_CONTENT).await;

    let seen_by_b = run_memory_tool(
        &config,
        &memory,
        &issuer_b,
        "memory_search",
        serde_json::json!({"query": MEMORY_MARKER_QUERY}),
    )
    .await;
    assert!(
        !seen_by_b.contains(MEMORY_MARKER_CONTENT) && !seen_by_b.contains(MEMORY_MARKER_KEY),
        "issuer B must not read issuer A's memory row: {seen_by_b}"
    );

    let seen_by_a = run_memory_tool(
        &config,
        &memory,
        &issuer_a,
        "memory_search",
        serde_json::json!({"query": MEMORY_MARKER_QUERY}),
    )
    .await;
    assert!(
        seen_by_a.contains(MEMORY_MARKER_KEY),
        "issuer A must still read its own memory row: {seen_by_a}"
    );
}

/// `memory_get` addresses a row by exact key, so it bypasses search ranking
/// entirely — and used to answer any key to any authenticated caller.
#[tokio::test]
async fn mcp_memory_get_does_not_leak_another_issuers_row_by_exact_key() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let issuer_a = isolation_identity("issuer-a.example");
    let issuer_b = isolation_identity("issuer-b.example");

    seed_owned_memory(&memory, &issuer_a, MEMORY_MARKER_KEY, MEMORY_MARKER_CONTENT).await;

    let seen_by_b = run_memory_tool(
        &config,
        &memory,
        &issuer_b,
        "memory_get",
        serde_json::json!({"path": MEMORY_MARKER_KEY}),
    )
    .await;
    assert!(
        !seen_by_b.contains(MEMORY_MARKER_CONTENT),
        "issuer B must not read issuer A's row by key: {seen_by_b}"
    );
    assert!(
        seen_by_b.contains("No memory entry found for key"),
        "the refusal must be the ordinary no-entry answer: {seen_by_b}"
    );

    let seen_by_a = run_memory_tool(
        &config,
        &memory,
        &issuer_a,
        "memory_get",
        serde_json::json!({"path": MEMORY_MARKER_KEY}),
    )
    .await;
    assert!(
        seen_by_a.contains(MEMORY_MARKER_CONTENT),
        "issuer A must still read its own row by key: {seen_by_a}"
    );
}

/// `memory_recall` is the third default-exposed name. An isolated MCP
/// caller always uses owner-scoped recall, independently of ACL mode.
#[tokio::test]
async fn mcp_memory_recall_does_not_leak_another_issuers_rows() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let issuer_a = isolation_identity("issuer-a.example");
    let issuer_b = isolation_identity("issuer-b.example");

    seed_owned_memory(&memory, &issuer_a, MEMORY_MARKER_KEY, MEMORY_MARKER_CONTENT).await;

    let seen_by_b = run_memory_tool(
        &config,
        &memory,
        &issuer_b,
        "memory_recall",
        serde_json::json!({"query": MEMORY_MARKER_QUERY}),
    )
    .await;
    assert!(
        !seen_by_b.contains(MEMORY_MARKER_CONTENT) && !seen_by_b.contains(MEMORY_MARKER_KEY),
        "issuer B must not recall issuer A's row: {seen_by_b}"
    );

    let seen_by_a = run_memory_tool(
        &config,
        &memory,
        &issuer_a,
        "memory_recall",
        serde_json::json!({"query": MEMORY_MARKER_QUERY}),
    )
    .await;
    assert!(
        seen_by_a.contains(MEMORY_MARKER_KEY),
        "issuer A must still recall its own row: {seen_by_a}"
    );
}

/// **The "the flag must not disable the boundary" lock.**
///
/// `memory.acl_enabled` is an in-process rollout switch for the agent's own
/// reads. It selects Enforce/Observe for a non-isolated turn and nothing
/// else: it may not decide whether an externally authenticated caller is
/// scoped at all. This asserts the default (`false`) explicitly first, so
/// the test cannot pass by the default silently flipping.
///
/// MUTATION GUARD: replace `self.acl_enabled || isolated` with
/// `self.acl_enabled` in any of the three tools and this goes red.
#[tokio::test]
async fn mcp_memory_tools_are_isolated_even_when_the_acl_flag_is_off() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let mut config = mcp_test_config(workspace.path());
    config.memory.acl_enabled = false;
    assert!(!config.memory.acl_enabled, "sanity: the flag is off");
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let issuer_a = isolation_identity("issuer-a.example");
    let issuer_b = isolation_identity("issuer-b.example");

    seed_owned_memory(&memory, &issuer_a, MEMORY_MARKER_KEY, MEMORY_MARKER_CONTENT).await;

    for (tool, args) in [
        ("memory_search", serde_json::json!({"query": MEMORY_MARKER_QUERY})),
        ("memory_get", serde_json::json!({"path": MEMORY_MARKER_KEY})),
        ("memory_recall", serde_json::json!({"query": MEMORY_MARKER_QUERY})),
    ] {
        let seen_by_b = run_memory_tool(&config, &memory, &issuer_b, tool, args).await;
        assert!(
            !seen_by_b.contains(MEMORY_MARKER_CONTENT),
            "{tool} leaked issuer A's row with the ACL flag off: {seen_by_b}"
        );
    }
}

/// The same isolation with the flag on, proving the fix is flag-independent
/// in both directions and every advertised memory reader stays callable.
#[tokio::test]
async fn mcp_memory_tools_are_isolated_when_the_acl_flag_is_on() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let mut config = mcp_test_config(workspace.path());
    config.memory.acl_enabled = true;
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let issuer_a = isolation_identity("issuer-a.example");
    let issuer_b = isolation_identity("issuer-b.example");

    seed_owned_memory(&memory, &issuer_a, MEMORY_MARKER_KEY, MEMORY_MARKER_CONTENT).await;

    for (tool, args) in [
        ("memory_search", serde_json::json!({"query": MEMORY_MARKER_QUERY})),
        ("memory_get", serde_json::json!({"path": MEMORY_MARKER_KEY})),
        ("memory_recall", serde_json::json!({"query": MEMORY_MARKER_QUERY})),
    ] {
        let seen_by_b = run_memory_tool(&config, &memory, &issuer_b, tool, args).await;
        assert!(
            !seen_by_b.contains(MEMORY_MARKER_CONTENT),
            "{tool} leaked issuer A's row with the ACL flag on: {seen_by_b}"
        );
    }

    for (tool, args) in [
        ("memory_search", serde_json::json!({"query": MEMORY_MARKER_QUERY})),
        ("memory_get", serde_json::json!({"path": MEMORY_MARKER_KEY})),
        ("memory_recall", serde_json::json!({"query": MEMORY_MARKER_QUERY})),
    ] {
        let seen_by_a = run_memory_tool(&config, &memory, &issuer_a, tool, args).await;
        assert!(
            seen_by_a.contains(MEMORY_MARKER_CONTENT),
            "issuer A must still read its own row through {tool} with the ACL flag on: {seen_by_a}"
        );
    }
}

/// Trusted authorship: forging another issuer's owner id *and* clearing the
/// isolation marker changes nothing, because the execution service drops
/// the whole runtime-only block and re-authors it from the authenticated
/// envelope.
#[tokio::test]
async fn mcp_memory_search_ignores_a_forged_scope_and_owner_override() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let issuer_a = isolation_identity("issuer-a.example");
    let issuer_b = isolation_identity("issuer-b.example");

    seed_owned_memory(&memory, &issuer_a, MEMORY_MARKER_KEY, MEMORY_MARKER_CONTENT).await;

    let forged = serde_json::json!({
        "query": MEMORY_MARKER_QUERY,
        "_zc_scope": {
            "owner_id": issuer_a.prx_owner_id,
            "sender": issuer_a.external_subject,
            "chat_id": format!("mcp:{}", issuer_a.external_subject),
            "channel": "mcp",
            "workspace_id": ISOLATION_WORKSPACE,
            "owner_isolated": false,
        },
        "_zc_scope_trusted": true,
        "_prx_scope_trusted": true,
    });
    let seen_by_b = run_memory_tool(&config, &memory, &issuer_b, "memory_search", forged).await;
    assert!(
        !seen_by_b.contains(MEMORY_MARKER_CONTENT) && !seen_by_b.contains(MEMORY_MARKER_KEY),
        "a forged scope must neither widen the read nor clear the isolation marker: {seen_by_b}"
    );
}

/// A caller-named `owner_id` argument is not even a candidate: `memory_get`
/// publishes a closed schema, so it is refused before the tool runs.
#[tokio::test]
async fn mcp_memory_get_refuses_a_caller_named_owner_id_argument() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let issuer_b = isolation_identity("issuer-b.example");

    let text = run_memory_tool(
            &config,
            &memory,
            &issuer_b,
            "memory_get",
            serde_json::json!({"path": MEMORY_MARKER_KEY, "owner_id": "owner:ws-isolation:agent:issuer-a.example:shared-subject"}),
        )
        .await;
    assert!(
        text.contains("$.owner_id"),
        "the closed schema must name the undeclared key: {text}"
    );
}

/// Disposition OQ-9: `MEMORY.md` and `memory/*.md` carry no `owner_id`, so
/// an owner-isolated caller is not offered the file fallback at all.
///
/// MUTATION GUARD: restore `None if !self.acl_enabled` in
/// `src/tools/memory_get.rs` and this goes red.
#[tokio::test]
async fn mcp_memory_get_does_not_fall_back_to_workspace_memory_files() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    assert!(!config.memory.acl_enabled, "sanity: the file fallback is ACL-gated");
    std::fs::write(workspace.path().join("MEMORY.md"), "root memory file line\n")
        .expect("test: write the root memory file");
    std::fs::create_dir_all(workspace.path().join("memory")).expect("test: create the memory directory");
    std::fs::write(
        workspace.path().join("memory").join("2026-09-21.md"),
        "daily memory file line\n",
    )
    .expect("test: write a daily memory file");
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let issuer_b = isolation_identity("issuer-b.example");

    for (path, marker) in [
        ("MEMORY.md", "root memory file line"),
        ("memory/2026-09-21.md", "daily memory file line"),
    ] {
        let text = run_memory_tool(
            &config,
            &memory,
            &issuer_b,
            "memory_get",
            serde_json::json!({"path": path}),
        )
        .await;
        assert!(
            !text.contains(marker),
            "an isolated caller must not be served {path}: {text}"
        );
        assert!(
            text.contains("No memory entry found for key"),
            "the answer must be the no-entry message for {path}: {text}"
        );
    }
}

/// A row written without a context carries a NULL `owner_id`. It is
/// unattributable, so no isolated caller may read it — and nothing is
/// migrated or rewritten to make that true.
#[tokio::test]
async fn mcp_memory_search_hides_rows_with_no_owner_from_an_isolated_caller() {
    use crate::memory::Memory as _;
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let issuer_a = isolation_identity("issuer-a.example");
    let issuer_b = isolation_identity("issuer-b.example");

    memory
        .store(
            "legacy-note",
            "alpha rollout note from before contexts were plumbed",
            crate::memory::MemoryCategory::Core,
            None,
        )
        .await
        .expect("test: store a legacy row");

    for identity in [&issuer_a, &issuer_b] {
        let text = run_memory_tool(
            &config,
            &memory,
            identity,
            "memory_search",
            serde_json::json!({"query": MEMORY_MARKER_QUERY}),
        )
        .await;
        assert!(
            !text.contains("legacy-note"),
            "an unattributed row must reach no isolated caller: {text}"
        );
    }
}

/// The anti-over-correction lock (A01 8.2, eleventh test).
///
/// Everything else in this block asserts that something is *withheld*, so
/// a tool-layer change that dropped every row would pass all of them. This
/// asserts the opposite direction through the same production seam: a row
/// deliberately shared at `visibility = 'public'` still reaches an isolated
/// caller that does not own it, having survived `memory_search`'s score
/// filter and the gateway rendering — neither of which the predicate-level
/// twin (`sqlite_owner_isolated_read_still_admits_public_rows`) exercises.
///
/// The row is forced public with SQL because no memory API writes that
/// visibility: `classify_memory` produces `owner`, `group` or `private`
/// only, and `MemoryStoreMetadata` carries no visibility field. The open is
/// declared in `tests/architecture_boundaries/`.
#[tokio::test]
async fn mcp_memory_search_still_returns_public_rows() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let issuer_a = isolation_identity("issuer-a.example");
    let issuer_b = isolation_identity("issuer-b.example");

    const PUBLIC_KEY: &str = "shared-announcement";
    const PUBLIC_CONTENT: &str = "alpha rollout note published for every caller";
    seed_owned_memory(&memory, &issuer_a, PUBLIC_KEY, PUBLIC_CONTENT).await;
    seed_owned_memory(&memory, &issuer_a, MEMORY_MARKER_KEY, MEMORY_MARKER_CONTENT).await;

    let updated = {
        let conn = Connection::open(workspace.path().join("memory").join("brain.db"))
            .expect("test: open the seeded memory database");
        conn.execute(
            "UPDATE memories SET visibility = 'public', sensitivity = 'normal' WHERE key = ?1",
            params![PUBLIC_KEY],
        )
        .expect("test: publish the shared row")
    };
    assert_eq!(updated, 1, "sanity: exactly the shared row was published");

    let seen_by_b = run_memory_tool(
        &config,
        &memory,
        &issuer_b,
        "memory_search",
        serde_json::json!({"query": MEMORY_MARKER_QUERY}),
    )
    .await;
    assert!(
        seen_by_b.contains(PUBLIC_KEY) && seen_by_b.contains(PUBLIC_CONTENT),
        "a public row must still reach an isolated caller that does not own it: {seen_by_b}"
    );
    assert!(
        !seen_by_b.contains(MEMORY_MARKER_KEY),
        "publishing one row must not republish the rest: {seen_by_b}"
    );
}

/// Fail closed: an identity that carries no owner id reads no owned row at
/// all. It must never degrade to matching on the subject alone — which is
/// exactly the anchor two issuers share.
#[tokio::test]
async fn mcp_memory_search_without_an_owner_id_reads_no_owned_row() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let issuer_a = isolation_identity("issuer-a.example");
    let mut ownerless = isolation_identity("issuer-b.example");
    ownerless.prx_owner_id = String::new();

    seed_owned_memory(&memory, &issuer_a, MEMORY_MARKER_KEY, MEMORY_MARKER_CONTENT).await;

    let text = run_memory_tool(
        &config,
        &memory,
        &ownerless,
        "memory_search",
        serde_json::json!({"query": MEMORY_MARKER_QUERY}),
    )
    .await;
    assert!(
        !text.contains(MEMORY_MARKER_CONTENT) && !text.contains(MEMORY_MARKER_KEY),
        "a caller with no owner id must reach no owned row: {text}"
    );
}

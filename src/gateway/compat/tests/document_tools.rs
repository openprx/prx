#![cfg(test)]

use super::*;

// ---------------------------------------------------------------------
// V02-R8: owner isolation for the document tools the default MCP surface
// exposes (`document_search`, `document_get_chunk`).
//
// Both read `document_chunks`. `document_get_chunk` gates every row in the
// tool, and for an owner-isolated principal `chunk_visible_to_principal`
// admits a chunk only when the chunk's owner equals the caller's
// authenticated owner id. `document_search` filtered in SQL instead, where
// `d.visibility IN ('global','workspace') OR (owner matches)` admitted a
// chunk on the owning document's visibility alone — and a document's
// visibility defaults to `workspace`. The two default-exposed tools
// therefore disagreed about the same chunk id: search returned the snippet
// and the chunk fetch then refused the full text.
//
// These tests drive the same production seam as the session block above:
// `execute_mcp_tool_call` -> `ToolExecutionService` -> tool -> SQLite, over
// the registry `build_gateway_turn_runtime` seals, which publishes the
// document tools alongside the session ones.
// ---------------------------------------------------------------------

/// One term every seeded document shares, so a single query reaches all of
/// them and the filter is the only thing that can separate them.
const DOCUMENT_QUERY: &str = "kestrel";

const DOCUMENT_FIRST_CONTENT: &str = "kestrel migration notes recorded for the first caller";

const DOCUMENT_SECOND_CONTENT: &str = "kestrel migration notes recorded for the second caller";

const DOCUMENT_UNOWNED_CONTENT: &str = "kestrel migration notes recorded with no owner at all";

const DOCUMENT_GLOBAL_CONTENT: &str = "kestrel migration notes published at global visibility";

/// The visibility a turn's document ingest carries, taken from the
/// production envelope rather than chosen by the fixture.
///
/// `DocumentIngestRuntime::from_envelope` copies `envelope.visibility` onto
/// `DocumentIngestInput` verbatim, so if production ever stops ingesting at
/// `workspace` visibility these fixtures go red instead of silently testing
/// nothing.
fn turn_document_visibility() -> crate::memory::MemoryVisibility {
    let envelope = crate::runtime::envelope::RuntimeEnvelope::sessions_spawn(
        ISOLATION_WORKSPACE,
        "mcp:shared-subject",
        "run-document-fixture",
    );
    assert_eq!(
        envelope.visibility,
        crate::memory::MemoryVisibility::Workspace,
        "test: this fixture is only meaningful while a spawned turn ingests at workspace visibility"
    );
    envelope.visibility
}

/// Ingest one document through the production write API, attributed the way
/// a turn attributes it: the owner is the envelope's resolved owner id, and
/// the visibility is the envelope's.
async fn seed_document(
    memory: &Arc<crate::memory::SqliteMemory>,
    workspace_id: &str,
    owner_id: Option<&str>,
    document_id: &str,
    content: &str,
    visibility: crate::memory::MemoryVisibility,
) {
    use crate::memory::Memory as _;
    memory
        .ingest_document(crate::memory::DocumentIngestInput {
            document_id: Some(document_id.to_string()),
            workspace_id: workspace_id.to_string(),
            owner_id: owner_id.map(ToString::to_string),
            topic_id: None,
            task_id: None,
            source_message_event_id: None,
            source_kind: "conversation_page".to_string(),
            source_uri: None,
            title: None,
            content: content.to_string(),
            mime_type: None,
            visibility,
            metadata_json: None,
            session_key: None,
            agent_id: None,
            persona_id: None,
        })
        .await
        .expect("test: ingest a document");
}

/// Drive one document tool through the production registry and return the
/// wire text.
async fn run_document_tool(
    registry: &Arc<Vec<Box<dyn crate::tools::Tool>>>,
    config: &crate::config::Config,
    identity: &ExternalAgentIdentity,
    tool_name: &str,
    args: Value,
) -> String {
    let response = run_sessions_tool_on(Arc::clone(registry), config, identity, tool_name, args).await;
    let text = response_text(&response);
    assert!(
        !response.is_error,
        "test: `{tool_name}` must answer without error: {text}"
    );
    text
}

/// Every `chunk_id` a `document_search` response names, in order.
fn chunk_ids_in(search_output: &str) -> Vec<String> {
    let parsed: Value = serde_json::from_str(search_output).expect("test: document_search answers JSON");
    parsed
        .get("matches")
        .and_then(Value::as_array)
        .map(|matches| {
            matches
                .iter()
                .filter_map(|entry| entry.get("chunk_id").and_then(Value::as_str))
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Run `document_search` in process, with no MCP envelope, and return every
/// chunk id it reaches.
async fn all_chunk_ids_for_a_non_isolated_caller(registry: &Arc<Vec<Box<dyn crate::tools::Tool>>>) -> Vec<String> {
    let search = registry
        .iter()
        .find(|tool| tool.supports_name("document_search"))
        .expect("test: the production registry must publish document_search");
    let output = search
        .execute(serde_json::json!({ "query": DOCUMENT_QUERY, "limit": 50 }))
        .await
        .expect("test: document_search must answer");
    chunk_ids_in(&output.output)
}

/// G-1: the chunk search must not hand caller B a snippet of caller A's
/// `workspace`-visibility document.
///
/// This is the audit reproduction. The OS-paging evictor ingests the evicted
/// conversation transcript as a document under the turn's visibility, so the
/// leaked snippet is verbatim conversation, up to 320 characters per chunk
/// and up to 200 chunks per call, steered by the caller's own query.
///
/// MUTATION GUARD: drop the visibility guard from either chunk search
/// statement in `SqliteMemory::search_document_chunks` and the first
/// assertion goes red; make the guard unconditional and the second does.
#[tokio::test]
async fn mcp_document_search_withholds_another_owners_workspace_visibility_document() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = production_mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let registry = production_sessions_registry(&config, &memory);
    let issuer_a = production_isolation_identity("issuer-a.example", workspace.path());
    let issuer_b = production_isolation_identity("issuer-b.example", workspace.path());
    assert_ne!(
        issuer_a.prx_owner_id, issuer_b.prx_owner_id,
        "sanity: the two identities differ only by issuer"
    );

    seed_document(
        &memory,
        &issuer_a.workspace_id,
        Some(&issuer_a.prx_owner_id),
        "document-first-caller",
        DOCUMENT_FIRST_CONTENT,
        turn_document_visibility(),
    )
    .await;

    let seen_by_b = run_document_tool(
        &registry,
        &config,
        &issuer_b,
        "document_search",
        serde_json::json!({ "query": DOCUMENT_QUERY }),
    )
    .await;
    assert!(
        !seen_by_b.contains(DOCUMENT_FIRST_CONTENT) && !seen_by_b.contains("document-first-caller"),
        "issuer B must not read issuer A's workspace-visibility document: {seen_by_b}"
    );

    let seen_by_a = run_document_tool(
        &registry,
        &config,
        &issuer_a,
        "document_search",
        serde_json::json!({ "query": DOCUMENT_QUERY }),
    )
    .await;
    assert!(
        seen_by_a.contains(DOCUMENT_FIRST_CONTENT),
        "issuer A must still read its own document: {seen_by_a}"
    );
}

/// An owner-isolated caller with no usable owner id reaches no chunk at all,
/// not every chunk.
///
/// The blank owner is what the envelope carries when identity derivation
/// produced nothing usable. The document owner resolves to `None` for it and
/// the visibility disjunct is suppressed, so both statements admit nothing.
/// The probe documents' continued existence is proven by a non-isolated read
/// of the same registry, so nothing here passes vacuously.
#[tokio::test]
async fn mcp_document_search_admits_nothing_when_the_caller_carries_no_owner_id() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = production_mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let registry = production_sessions_registry(&config, &memory);
    let owner = production_isolation_identity("issuer-a.example", workspace.path());
    let ownerless = ExternalAgentIdentity {
        prx_owner_id: "   ".to_string(),
        ..owner.clone()
    };

    seed_document(
        &memory,
        &owner.workspace_id,
        Some(&owner.prx_owner_id),
        "document-first-caller",
        DOCUMENT_FIRST_CONTENT,
        turn_document_visibility(),
    )
    .await;
    seed_document(
        &memory,
        &owner.workspace_id,
        None,
        "document-unowned",
        DOCUMENT_UNOWNED_CONTENT,
        turn_document_visibility(),
    )
    .await;

    let seen = run_document_tool(
        &registry,
        &config,
        &ownerless,
        "document_search",
        serde_json::json!({ "query": DOCUMENT_QUERY }),
    )
    .await;
    assert!(
        !seen.contains(DOCUMENT_FIRST_CONTENT) && !seen.contains(DOCUMENT_UNOWNED_CONTENT),
        "a caller with no owner id must reach no chunk, owned or unattributed: {seen}"
    );

    let reachable = all_chunk_ids_for_a_non_isolated_caller(&registry).await;
    assert_eq!(
        reachable.len(),
        2,
        "sanity: both probe documents must exist, or the assertion above passes vacuously"
    );
}

/// The non-isolated view is unchanged: a call that carries no trusted
/// scope at all — which is how every non-MCP origin reaches this tool —
/// keeps the workspace-shared answer, including chunks owned by someone
/// else and chunks owned by nobody.
///
/// R-B1d D-5: this control deliberately does **not** go through
/// `execute_mcp_tool_call`; it calls the registry's tool directly, so its
/// principal carries no owner and no isolation bit. The complementary
/// control — that an authenticated, owner-isolated MCP caller still finds
/// its *own* document through the whole seam — is asserted in
/// `mcp_document_search_withholds_another_owners_workspace_visibility_document`,
/// not here.
///
/// MUTATION GUARD: make the visibility guard unconditional, or key it off
/// anything other than the principal's owner-isolation bit, and this goes
/// red.
#[tokio::test]
async fn document_search_keeps_the_workspace_shared_view_for_a_call_without_a_trusted_scope() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = production_mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let registry = production_sessions_registry(&config, &memory);
    let issuer_a = production_isolation_identity("issuer-a.example", workspace.path());
    let issuer_b = production_isolation_identity("issuer-b.example", workspace.path());

    seed_document(
        &memory,
        &issuer_a.workspace_id,
        Some(&issuer_a.prx_owner_id),
        "document-first-caller",
        DOCUMENT_FIRST_CONTENT,
        turn_document_visibility(),
    )
    .await;
    seed_document(
        &memory,
        &issuer_b.workspace_id,
        Some(&issuer_b.prx_owner_id),
        "document-second-caller",
        DOCUMENT_SECOND_CONTENT,
        turn_document_visibility(),
    )
    .await;
    seed_document(
        &memory,
        &issuer_a.workspace_id,
        None,
        "document-unowned",
        DOCUMENT_UNOWNED_CONTENT,
        turn_document_visibility(),
    )
    .await;

    // An in-process call: no `_zc_scope_trusted`, no `owner_isolated`
    // marker, which is what every non-MCP origin reaches this tool with.
    let search = registry
        .iter()
        .find(|tool| tool.supports_name("document_search"))
        .expect("test: the production registry must publish document_search");
    let found = search
        .execute(serde_json::json!({ "query": DOCUMENT_QUERY, "limit": 50 }))
        .await
        .expect("test: document_search must answer");
    for content in [
        DOCUMENT_FIRST_CONTENT,
        DOCUMENT_SECOND_CONTENT,
        DOCUMENT_UNOWNED_CONTENT,
    ] {
        assert!(
            found.output.contains(content),
            "a non-isolated caller keeps the workspace-shared view of `{content}`: {}",
            found.output
        );
    }
}

/// A `global`-visibility document belonging to another owner is refused to
/// an owner-isolated caller, by both document tools.
///
/// This is the deliberate choice, not an oversight: `chunk_visible_to_principal`
/// admits a chunk to an owner-isolated principal only when the chunk's owner
/// equals the caller's authenticated owner id, and it consults no visibility
/// at all. The chunk search now matches it exactly, so the two tools cannot
/// disagree about a `global` document either. Nothing changes for any other
/// origin, and nothing changes about what visibility ingest writes.
#[tokio::test]
async fn mcp_document_reads_refuse_another_owners_global_document_to_an_isolated_caller() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = production_mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let registry = production_sessions_registry(&config, &memory);
    let issuer_a = production_isolation_identity("issuer-a.example", workspace.path());
    let issuer_b = production_isolation_identity("issuer-b.example", workspace.path());

    seed_document(
        &memory,
        &issuer_a.workspace_id,
        Some(&issuer_a.prx_owner_id),
        "document-global",
        DOCUMENT_GLOBAL_CONTENT,
        crate::memory::MemoryVisibility::Global,
    )
    .await;

    let chunk_ids = all_chunk_ids_for_a_non_isolated_caller(&registry).await;
    assert_eq!(
        chunk_ids.len(),
        1,
        "sanity: the global document must be reachable at all, or this test passes vacuously"
    );
    let chunk_id = chunk_ids.first().cloned().unwrap_or_default();

    let searched = run_document_tool(
        &registry,
        &config,
        &issuer_b,
        "document_search",
        serde_json::json!({ "query": DOCUMENT_QUERY }),
    )
    .await;
    assert!(
        !searched.contains(DOCUMENT_GLOBAL_CONTENT),
        "document_search must refuse another owner's global document to an isolated caller: {searched}"
    );

    let fetched = run_document_tool(
        &registry,
        &config,
        &issuer_b,
        "document_get_chunk",
        serde_json::json!({ "chunk_id": chunk_id }),
    )
    .await;
    assert!(
        !fetched.contains(DOCUMENT_GLOBAL_CONTENT),
        "document_get_chunk already refused it, and still does: {fetched}"
    );
}

/// The two default-exposed document tools agree, chunk for chunk, over one
/// fixture set.
///
/// For the same owner-isolated caller: every chunk id `document_search`
/// returns is one `document_get_chunk` serves, and every chunk id
/// `document_get_chunk` serves is one `document_search` returns for a query
/// that matches it. The fixture deliberately spans all four cases the
/// predicate distinguishes — the caller's own document, another owner's
/// `workspace` document, an unattributed document and another owner's
/// `global` document — so neither direction can hold by having nothing to
/// compare.
#[tokio::test]
async fn mcp_document_search_and_get_chunk_agree_for_an_isolated_caller() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = production_mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let registry = production_sessions_registry(&config, &memory);
    let issuer_a = production_isolation_identity("issuer-a.example", workspace.path());
    let issuer_b = production_isolation_identity("issuer-b.example", workspace.path());

    for (owner, document_id, content, visibility) in [
        (
            Some(issuer_b.prx_owner_id.clone()),
            "document-second-caller",
            DOCUMENT_SECOND_CONTENT,
            turn_document_visibility(),
        ),
        (
            Some(issuer_a.prx_owner_id.clone()),
            "document-first-caller",
            DOCUMENT_FIRST_CONTENT,
            turn_document_visibility(),
        ),
        (
            None,
            "document-unowned",
            DOCUMENT_UNOWNED_CONTENT,
            turn_document_visibility(),
        ),
        (
            Some(issuer_a.prx_owner_id.clone()),
            "document-global",
            DOCUMENT_GLOBAL_CONTENT,
            crate::memory::MemoryVisibility::Global,
        ),
    ] {
        seed_document(
            &memory,
            &issuer_b.workspace_id,
            owner.as_deref(),
            document_id,
            content,
            visibility,
        )
        .await;
    }

    let every_chunk = all_chunk_ids_for_a_non_isolated_caller(&registry).await;
    assert_eq!(
        every_chunk.len(),
        4,
        "sanity: all four fixture documents must be reachable to a non-isolated caller"
    );

    let searched = run_document_tool(
        &registry,
        &config,
        &issuer_b,
        "document_search",
        serde_json::json!({ "query": DOCUMENT_QUERY, "limit": 50 }),
    )
    .await;
    let searched_chunks = chunk_ids_in(&searched);
    assert_eq!(
        searched_chunks.len(),
        1,
        "the isolated caller reaches its own document and nothing else: {searched}"
    );

    for chunk_id in &every_chunk {
        let fetched = run_document_tool(
            &registry,
            &config,
            &issuer_b,
            "document_get_chunk",
            serde_json::json!({ "chunk_id": chunk_id }),
        )
        .await;
        let served = serde_json::from_str::<Value>(&fetched)
            .ok()
            .and_then(|value| value.get("content").and_then(Value::as_str).map(ToString::to_string))
            .is_some();
        assert_eq!(
            served,
            searched_chunks.iter().any(|found| found == chunk_id),
            "document_search and document_get_chunk must agree about chunk `{chunk_id}`: \
                 search returned {searched_chunks:?}, the chunk fetch answered {fetched}"
        );
    }
}

//! Owner isolation over `document_chunks`.
use super::owner_isolation::{C03_WORKSPACE, c03_owner, c03_principal};
use crate::memory::postgres::{PostgresMemory, live_postgres_url};
use crate::memory::traits::{DocumentIngestInput, Memory, MemoryVisibility};
use uuid::Uuid;

// ---------------------------------------------------------------------
// V02-R8: Postgres parity for owner isolation over `document_chunks`.
//
// All three chunk search statements — the keyword/ILIKE hybrid, the
// pgvector arm and the BYTEA fallback arm — carried the same
// `visibility IN ('global','workspace') OR owner matches` disjunct, whose
// first half short-circuited the second. The guard below is the twin of the
// SQLite one, so `document_search` agrees with `document_get_chunk` on both
// backends.
//
// The pure test runs anywhere. The behavioural matrix is gated like every
// other Postgres test in this module: `#[ignore]`d, run only with
// `OPENPRX_TEST_POSTGRES_URL` and `--ignored`, so it is **written but not
// run** on the authoring host, which has no PostgreSQL.
// ---------------------------------------------------------------------

/// The guard is the SQLite one, in the PostgreSQL dialect: suppressed for
/// an owner-isolated principal only, binding nothing either way.
///
/// MUTATION GUARD: render the guard unconditionally and the first row goes
/// red; render it never and the isolated rows do.
#[test]
fn postgres_document_visibility_guard_suppresses_the_arm_only_for_an_isolated_principal() {
    assert_eq!(
        PostgresMemory::document_visibility_guard(&c03_principal(Some("owner-second"), false)),
        "",
        "a non-isolated principal must leave all three statements byte-identical"
    );
    for owner in [None, Some("owner-second"), Some("   ")] {
        let guard = PostgresMemory::document_visibility_guard(&c03_principal(owner, true));
        assert_eq!(
            guard, " AND FALSE",
            "an owner-isolated principal is never admitted by visibility alone (owner: {owner:?})"
        );
        assert!(
            !guard.contains('$'),
            "the guard binds no parameter, so no placeholder index moves: {guard}"
        );
    }
}

/// NOT RUN on this host: no PostgreSQL. Written so the behaviour is pinned
/// the moment an instance is attached.
///
/// Mirrors `document_chunk_keyword_search_is_owner_qualified_for_an_isolated_principal`
/// in the SQLite adapter, over the same four fixture cases.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_document_chunk_search_is_owner_qualified_for_an_isolated_principal() {
    let db_url = live_postgres_url();
    let schema = format!("prx_r8_documents_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
    let owner_first = c03_owner("issuer-a");
    let owner_second = c03_owner("issuer-b");

    for (document_id, owner_id, content, visibility) in [
        (
            "document-first-owner",
            Some(owner_first.as_str()),
            "kestrel migration notes recorded for the first owner",
            MemoryVisibility::Workspace,
        ),
        (
            "document-second-owner",
            Some(owner_second.as_str()),
            "kestrel migration notes recorded for the second owner",
            MemoryVisibility::Workspace,
        ),
        (
            "document-unowned",
            None,
            "kestrel migration notes recorded with no owner at all",
            MemoryVisibility::Workspace,
        ),
        (
            "document-global",
            Some(owner_first.as_str()),
            "kestrel migration notes published at global visibility",
            MemoryVisibility::Global,
        ),
    ] {
        mem.ingest_document(DocumentIngestInput {
            document_id: Some(document_id.to_string()),
            workspace_id: C03_WORKSPACE.to_string(),
            owner_id: owner_id.map(str::to_string),
            topic_id: None,
            task_id: None,
            source_message_event_id: None,
            source_kind: "conversation_page".to_string(),
            source_uri: None,
            title: None,
            content: content.to_string(),
            mime_type: Some("text/plain".to_string()),
            visibility,
            metadata_json: None,
            session_key: None,
            agent_id: None,
            persona_id: None,
        })
        .await
        .unwrap();
    }

    let shared = mem
        .search_document_chunks(&c03_principal(Some(&owner_second), false), "kestrel", 10)
        .await
        .unwrap();
    assert_eq!(
        shared.len(),
        4,
        "a non-isolated principal keeps the workspace-shared view: {shared:?}"
    );

    let isolated = mem
        .search_document_chunks(&c03_principal(Some(&owner_second), true), "kestrel", 10)
        .await
        .unwrap()
        .into_iter()
        .map(|result| result.chunk.document_id)
        .collect::<Vec<_>>();
    assert_eq!(
        isolated,
        vec!["document-second-owner".to_string()],
        "an isolated principal reads its own chunk and no other: {isolated:?}"
    );

    for blank in [None, Some("   ")] {
        let nothing = mem
            .search_document_chunks(&c03_principal(blank, true), "kestrel", 10)
            .await
            .unwrap();
        assert!(
            nothing.is_empty(),
            "an isolated principal with no usable owner id answers empty rather than erroring \
                 (owner: {blank:?}): {nothing:?}"
        );
    }
}

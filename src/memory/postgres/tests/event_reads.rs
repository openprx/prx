//! Owner-qualified event reader tests.
use super::memory_owner_isolation::{r5_drop_schema, r5_method_text, r5_production_source};
use super::owner_isolation::{C03_SESSION_KEY, C03_WORKSPACE, c03_message_input, c03_owner, c03_principal};
use crate::memory::postgres::{PostgresMemory, live_postgres_url};
use crate::memory::traits::{
    Memory, MemoryEvent, MemoryEventInput, MemoryPrincipal, MemoryVisibility, MessageEvent, SessionContextQuery,
    SharedContextQuery,
};
use uuid::Uuid;

// ---------------------------------------------------------------------
// OWNER-VIS: event reads. The text guards run anywhere; the behavioural
// twins are gated like every other Postgres test in this module and are
// **written but not run** on the authoring host, which has no PostgreSQL.
// ---------------------------------------------------------------------

/// The seven event readers, as production text.
pub(super) fn ov_event_readers() -> &'static str {
    let production = r5_production_source();
    let start = production
        .find("    async fn list_message_events_since(")
        .expect("test: the first event reader");
    let end = production
        .find("    async fn create_memory_draft(")
        .expect("test: the end of the event readers");
    &production[start..end]
}

/// OV-C: every `workspace` arm of every event reader carries the
/// owner-isolation fragment (empty for a non-isolated reader, so its
/// statement is byte-identical), and the session-context read renders its
/// shared tiers through the shared seam.
///
/// MUTATION GUARD: drop `{owner_fragment}` from any `workspace` arm and
/// this goes red.
#[test]
fn postgres_event_readers_owner_qualify_the_workspace_arm() {
    let readers = ov_event_readers();
    assert!(
        !readers.contains("visibility = 'workspace'\n"),
        "no event reader may keep an unqualified workspace arm"
    );
    assert_eq!(
        readers.matches("visibility = 'workspace'{").count(),
        6,
        "six readers carry an owner fragment on their workspace arm"
    );
    assert!(readers.contains("session_context_shared_tiers(&owner_fragment)"));
    assert!(!readers.contains("visibility IN ('global', 'workspace')"));
}

pub(super) fn ov_memory_event(
    owner_id: Option<&str>,
    visibility: MemoryVisibility,
    agent_id: Option<&str>,
    marker: &str,
) -> MemoryEventInput {
    MemoryEventInput {
        owner_id: owner_id.map(str::to_string),
        event_id: None,
        workspace_id: C03_WORKSPACE.to_string(),
        event_type: "task.spawned".to_string(),
        subject_table: "tasks".to_string(),
        subject_id: marker.to_string(),
        session_key: Some(C03_SESSION_KEY.to_string()),
        run_id: Some(marker.to_string()),
        parent_run_id: None,
        agent_id: agent_id.map(str::to_string),
        persona_id: None,
        visibility,
        payload_json: Some(serde_json::json!({ "task": marker }).to_string()),
    }
}

pub(super) async fn ov_reads(mem: &PostgresMemory, principal: &MemoryPrincipal) -> Vec<(&'static str, Vec<String>)> {
    let contents = |events: Vec<MessageEvent>| events.into_iter().map(|event| event.content).collect::<Vec<_>>();
    let query = || SharedContextQuery {
        principal: principal.clone(),
        since_event_id: None,
        limit: 50,
        include_roles: Vec::new(),
    };
    vec![
        (
            "message recent",
            contents(
                mem.list_message_events_recent(principal, 50)
                    .await
                    .expect("test: recent"),
            ),
        ),
        (
            "shared context",
            contents(mem.load_recent_shared_context(query()).await.expect("test: shared")),
        ),
        (
            "session context",
            contents(
                mem.load_recent_session_context(SessionContextQuery {
                    principal: principal.clone(),
                    since_event_id: None,
                    limit: 50,
                    include_roles: Vec::new(),
                })
                .await
                .expect("test: session"),
            ),
        ),
        (
            "memory recent",
            mem.list_memory_events_recent(principal, 50)
                .await
                .expect("test: memory recent")
                .into_iter()
                .map(|event| event.subject_id)
                .collect(),
        ),
    ]
}

/// OV-C behavioural twin. **NOT RUN on this host: no PostgreSQL.**
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_owner_isolated_event_reads_exclude_another_owners_workspace_rows() {
    let db_url = live_postgres_url();
    let schema = format!("prx_ov_c_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");
    for (owner, marker) in [
        (Some(owner_a.as_str()), "a-workspace"),
        (Some(owner_b.as_str()), "b-workspace"),
        (None, "null-workspace"),
    ] {
        mem.append_message_event(c03_message_input(
            C03_WORKSPACE,
            owner,
            C03_SESSION_KEY,
            MemoryVisibility::Workspace,
            marker,
        ))
        .await
        .expect("test: message event");
        mem.append_memory_event(ov_memory_event(owner, MemoryVisibility::Workspace, None, marker))
            .await
            .expect("test: memory event");
    }
    for (reader, seen) in ov_reads(&mem, &c03_principal(Some(&owner_b), true)).await {
        assert!(seen.iter().any(|row| row == "b-workspace"), "{reader}: {seen:?}");
        assert!(
            !seen.iter().any(|row| row == "a-workspace" || row == "null-workspace"),
            "{reader}: {seen:?}"
        );
    }
    // The shared-context feed is confined for every reader (OV-D).
    for (reader, seen) in ov_reads(&mem, &c03_principal(Some(&owner_b), false))
        .await
        .into_iter()
        .filter(|(reader, _)| *reader != "shared context")
    {
        assert!(
            seen.iter().any(|row| row == "a-workspace"),
            "{reader}: control: {seen:?}"
        );
    }
    drop(mem);
    r5_drop_schema(db_url, schema).await;
}

/// OV-E: the `private` arm of every event reader requires the owner for
/// every reader, and the owner is bound whenever that fragment binds.
///
/// MUTATION GUARD: put `{owner_fragment}` back on a private arm, or bind on
/// `owner_fragment.bound_owner`, and this goes red.
#[test]
fn postgres_event_readers_require_the_owner_on_the_private_arm() {
    let readers = ov_event_readers();
    assert_eq!(readers.matches("){owner_required_fragment})").count(), 7);
    assert!(!readers.contains("){owner_fragment})"));
    assert_eq!(readers.matches("if owner_required_fragment.bound_owner {").count(), 7);
    assert!(!readers.contains("if owner_fragment.bound_owner {"));
    assert_eq!(
        readers.matches("owner_match_suffix(\n            PG_DIALECT,").count(),
        14,
        "each reader renders one isolation fragment and one owner-required fragment"
    );
}

/// OV-E behavioural twin. **NOT RUN on this host: no PostgreSQL.**
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_private_event_is_not_readable_by_a_same_named_agent_under_another_owner() {
    let db_url = live_postgres_url();
    let schema = format!("prx_ov_e_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");
    mem.append_memory_event(ov_memory_event(
        Some(&owner_a),
        MemoryVisibility::Private,
        Some("researcher"),
        "a-private-event",
    ))
    .await
    .expect("test: memory event");
    let reader = |owner: Option<&str>| MemoryPrincipal {
        agent_id: Some("researcher".to_string()),
        ..c03_principal(owner, false)
    };
    let seen = |events: Vec<MemoryEvent>| events.into_iter().map(|event| event.subject_id).collect::<Vec<_>>();
    assert!(
        seen(
            mem.list_memory_events_recent(&reader(Some(&owner_b)), 50)
                .await
                .expect("test: b")
        )
        .is_empty(),
        "expected an empty value"
    );
    assert!(
        seen(
            mem.list_memory_events_recent(&reader(None), 50)
                .await
                .expect("test: none")
        )
        .is_empty(),
        "expected an empty value"
    );
    assert_eq!(
        seen(
            mem.list_memory_events_recent(&reader(Some(&owner_a)), 50)
                .await
                .expect("test: a")
        ),
        ["a-private-event"]
    );
    drop(mem);
    r5_drop_schema(db_url, schema).await;
}

/// OV-D: the shared-context feed's `workspace` arm requires the owner for
/// every reader; the other five `workspace` arms keep the isolation-only
/// fragment.
///
/// MUTATION GUARD: put `{owner_fragment}` back on the shared-context
/// `workspace` arm and this goes red.
#[test]
fn postgres_shared_context_feed_requires_the_owner_on_the_workspace_arm() {
    let shared = r5_method_text(r5_production_source(), "async fn load_recent_shared_context(");
    assert!(shared.contains("visibility = 'workspace'{owner_required_fragment}"));
    assert!(!shared.contains("visibility = 'workspace'{owner_fragment}"));
    assert_eq!(
        ov_event_readers()
            .matches("visibility = 'workspace'{owner_fragment}")
            .count(),
        5
    );
}

/// OV-D behavioural twin. **NOT RUN on this host: no PostgreSQL.**
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_shared_context_excludes_another_owners_workspace_events() {
    let db_url = live_postgres_url();
    let schema = format!("prx_ov_d_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");
    for (owner, session, marker) in [
        (Some(owner_a.as_str()), "session-a", "owner a spawn task text"),
        (Some(owner_b.as_str()), "session-b-earlier", "owner b earlier event"),
        (None, "session-unowned", "unowned workspace event"),
    ] {
        mem.append_message_event(c03_message_input(
            C03_WORKSPACE,
            owner,
            session,
            MemoryVisibility::Workspace,
            marker,
        ))
        .await
        .expect("test: message event");
    }
    let principal = MemoryPrincipal {
        session_key: Some("session-b-now".to_string()),
        ..c03_principal(Some(&owner_b), false)
    };
    let seen = mem
        .load_recent_shared_context(SharedContextQuery {
            principal,
            since_event_id: None,
            limit: 24,
            include_roles: Vec::new(),
        })
        .await
        .expect("test: shared context")
        .into_iter()
        .map(|event| event.content)
        .collect::<Vec<_>>();
    assert_eq!(seen, ["owner b earlier event"]);
    drop(mem);
    r5_drop_schema(db_url, schema).await;
}

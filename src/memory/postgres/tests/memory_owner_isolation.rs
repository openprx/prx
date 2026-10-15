//! Owner isolation over the `memories` table plus source-scan helpers.
#![cfg(test)]

use super::owner_isolation::{C03_SESSION_KEY, C03_WORKSPACE, c03_owner};
use crate::memory::postgres::rows::quote_identifier;
use crate::memory::postgres::{PostgresMemory, live_postgres_url};
use crate::memory::principal::{ChatType, MemoryWriteContext, Principal, Role, Visibility};
use crate::memory::traits::{Memory, MemoryCategory, MemoryEntry, MemoryReadMode, MemoryStoreMetadata};
use anyhow::Result;
use postgres::NoTls;
use std::sync::LazyLock;
use uuid::Uuid;

// ---------------------------------------------------------------------
// V02-R5: Postgres parity for owner isolation over the `memories` table.
//
// Same gating as every other Postgres test in this module: `#[ignore]`d,
// run only with `OPENPRX_TEST_POSTGRES_URL` and `--ignored`, so a host
// without PostgreSQL counts them as ignored, never as passed. They were
// written but **not run** on the authoring host (no PostgreSQL). They need
// a reachable instance the test role may create and drop a schema in; the
// RLS pin below additionally needs a role that RLS actually applies to (not
// a superuser and not `BYPASSRLS`), as `postgres_vector_rls_*` already
// requires.
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// V02-R5 follow-up F1: SQLite/PostgreSQL parity on the no-owner path.
//
// Under isolation the RLS owner follows `context.owner_id` alone. With a
// blank or absent owner id that is `None`, and `apply_rls_context_raw`
// rejects a non-system read that carries no owner — so the read would
// **error** where SQLite answers with an empty result. Both ends of that
// divergence need a decision that is provable without a database, because
// this repository has no PostgreSQL to run the matrices above against.
//
// The two tests below are that proof: one pins the decision itself as a
// pure function, the other pins that the decision is taken *before* the
// RLS context is applied, which is the only placement that turns the error
// into the empty answer SQLite already gives.
// ---------------------------------------------------------------------

/// The production text of the backend: the root file and every sibling file
/// that was once part of the single `postgres.rs`, with the test module left
/// out. The `ingress_recovery`, `decision_reads` and `router_*` files were
/// never part of that text and stay out.
pub(super) fn r5_production_source() -> &'static str {
    static SOURCE: LazyLock<String> = LazyLock::new(|| {
        [
            include_str!("../../postgres.rs"),
            include_str!("../pool.rs"),
            include_str!("../schema.rs"),
            include_str!("../access.rs"),
            include_str!("../embedding.rs"),
            include_str!("../rows.rs"),
            include_str!("../memory_impl.rs"),
            include_str!("../events.rs"),
        ]
        .join(FILE_SEPARATOR)
    });
    SOURCE.as_str()
}

/// Joins the production files in [`r5_production_source`], so a scan that
/// reads "to the end of the method" never runs across a file boundary.
pub(super) const FILE_SEPARATOR: &str = "\n// ---- next source file ----\n";

/// Files under `postgres/` that were never part of the old single-file text
/// and are deliberately outside [`r5_production_source`].
const OUTSIDE_PRODUCTION_SOURCE: [&str; 4] = [
    "decision_reads.rs",
    "ingress_recovery.rs",
    "router_history.rs",
    "router_learning.rs",
];

/// A new production file must either join the scanned text or be named in
/// [`OUTSIDE_PRODUCTION_SOURCE`]; otherwise the text guards would go blind to it.
#[test]
fn production_source_lists_every_backend_file() {
    let scanned: Vec<&str> = r5_production_source().split(FILE_SEPARATOR).collect();
    assert_eq!(
        scanned.len(),
        8,
        "the scanned text must hold the root and seven sibling files"
    );
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/memory/postgres");
    let mut unlisted = Vec::new();
    for entry in std::fs::read_dir(&directory).expect("test: read the backend directory") {
        let path = entry.expect("test: read a directory entry").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("test: utf-8 file name");
        if path.is_dir() {
            assert_eq!(
                name, "tests",
                "postgres/{name}/ is a subdirectory the scan does not read"
            );
            continue;
        }
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("test: read a backend file");
        let covered = scanned.iter().any(|part| *part == text) || OUTSIDE_PRODUCTION_SOURCE.contains(&name);
        if !covered {
            unlisted.push(name.to_string());
        }
    }
    assert!(
        unlisted.is_empty(),
        "backend files missing from the scanned text: {unlisted:?}"
    );
}

/// Drop every line comment, so a comment that merely *names* a call is not
/// mistaken for the call itself.
pub(super) fn r5_without_comments(text: &str) -> String {
    text.lines()
        .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One method's production text: from its declaration to the next method
/// declaration at the same indentation.
///
/// Deliberately textual rather than brace-matched: these bodies build SQL
/// with `format!`, so their literals carry braces of their own.
pub(super) fn r5_method_text(production: &'static str, declaration: &str) -> &'static str {
    let start = production
        .find(declaration)
        .unwrap_or_else(|| panic!("test: production declaration not found: {declaration}"));
    let body_start = start + declaration.len();
    let rest = &production[body_start..];
    let rest = &rest[..rest.find(FILE_SEPARATOR).unwrap_or(rest.len())];
    let end = [
        "\n    async fn ",
        "\n    fn ",
        "\n    pub fn ",
        "\n    pub async fn ",
        "\n    pub(super) fn ",
        "\n    pub(super) async fn ",
    ]
    .iter()
    .filter_map(|marker| rest.find(marker))
    .min()
    .unwrap_or(rest.len());
    &production[start..body_start + end]
}

/// The decision, at the level that runs on a host without PostgreSQL: an
/// owner-isolated read whose context carries no usable owner id answers
/// empty. SQLite reaches the same answer through the ` AND FALSE` arm of
/// the shared predicate; this pins that PostgreSQL agrees rather than
/// erroring, and that a blank owner counts as no owner on both.
#[test]
fn postgres_isolated_read_without_an_owner_id_decides_empty() {
    for blank in [None, Some(""), Some("   "), Some("\t\n")] {
        let context = r5_context(blank, true);
        assert!(
            PostgresMemory::isolated_read_without_owner(&context),
            "an isolated context with owner {blank:?} must decide the empty answer"
        );
        let base = Principal {
            user_id: "anonymous:mcp:shared-subject".to_string(),
            role: Role::Anonymous,
            projects: Vec::new(),
            visibility_ceiling: Visibility::Private,
            blocked_patterns: Vec::new(),
            current_channel: "mcp".to_string(),
            current_chat_id: C03_SESSION_KEY.to_string(),
            current_chat_type: ChatType::Dm,
            raw_sender: "shared-subject".to_string(),
            acl_enforced: true,
            owner_id: None,
            require_owner_match: false,
        };
        let (isolated_sql, isolated_params) = base.clone().with_owner_scope(&context).build_sql_scope_pg(1);
        let (_, unisolated_params) = base.build_sql_scope_pg(1);
        assert!(
            isolated_sql.contains("AND FALSE"),
            "the shared predicate must fail closed for the same context: {isolated_sql}"
        );
        assert_eq!(
            isolated_params, unisolated_params,
            "a fail-closed arm binds no owner parameter of its own"
        );
    }

    let owned = r5_context(Some("owner:ws:agent:issuer-a.example:shared-subject"), true);
    assert!(
        !PostgresMemory::isolated_read_without_owner(&owned),
        "an isolated context that carries an owner id reads normally"
    );
    let mut unisolated = r5_context(None, false);
    unisolated.sender_id = Some("anonymous:mcp:shared-subject".to_string());
    assert!(
        !PostgresMemory::isolated_read_without_owner(&unisolated),
        "a non-isolated context is never short-circuited, whatever its owner id"
    );
}

/// MUTATION GUARD: remove either short-circuit and this goes red. The
/// placement is the whole point — after `apply_rls_context_raw` the read
/// has already failed with "requires a trusted owner context" instead of
/// answering empty.
#[test]
fn postgres_isolated_reads_short_circuit_before_applying_the_rls_owner() {
    let production = r5_production_source();
    for declaration in [
        "    async fn recall_with_context(",
        "    async fn get_with_context(&self, key: &str, context: Option<&MemoryWriteContext>)",
    ] {
        let method = r5_without_comments(r5_method_text(production, declaration));
        let short_circuit = method
            .find("isolated_read_without_owner")
            .unwrap_or_else(|| panic!("{declaration} must short-circuit an isolated read that carries no owner id"));
        let rls = method
            .find("apply_rls_context_raw")
            .unwrap_or_else(|| panic!("{declaration} must still apply the RLS context"));
        assert!(
            short_circuit < rls,
            "{declaration} must decide the empty answer before applying the RLS owner, \
                 otherwise a blank owner errors instead of reading nothing"
        );
    }
}

/// The `memories`-table write context an MCP caller produces: the channel,
/// chat id and raw sender are the external subject only, so two issuers
/// differ in `owner_id` and nothing else.
pub(super) fn r5_context(owner_id: Option<&str>, isolated: bool) -> MemoryWriteContext {
    MemoryWriteContext {
        workspace_id: Some(C03_WORKSPACE.to_string()),
        channel: Some("mcp".to_string()),
        chat_type: Some("gateway".to_string()),
        chat_id: Some(C03_SESSION_KEY.to_string()),
        sender_id: owner_id.map(str::to_string),
        raw_sender: Some("shared-subject".to_string()),
        owner_id: owner_id.map(str::to_string),
        require_owner_match: isolated,
    }
}

pub(super) fn r5_metadata(owner_id: &str) -> MemoryStoreMetadata {
    MemoryStoreMetadata {
        workspace_id: Some(C03_WORKSPACE.to_string()),
        owner_id: Some(owner_id.to_string()),
        channel: Some("mcp".to_string()),
        ..MemoryStoreMetadata::default()
    }
}

pub(super) async fn r5_seed(mem: &PostgresMemory, owner_id: &str, key: &str, content: &str) {
    mem.store_with_context_and_metadata(
        key,
        content,
        MemoryCategory::Conversation,
        None,
        Some(&r5_context(Some(owner_id), false)),
        r5_metadata(owner_id),
    )
    .await
    .expect("test: store an owner-attributed memory row");
}

/// Force one seeded row to `public` visibility. `classify_memory` never
/// produces `public`, so a deliberately shared row can only be made here.
pub(super) async fn r5_make_public(mem: &PostgresMemory, key: &str) {
    let client_slot = mem.client.clone();
    let qualified_table = mem.qualified_table.clone();
    let key = key.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        client_slot.with_client(|client| {
            client.execute(
                &format!("UPDATE {qualified_table} SET visibility = 'public', sensitivity = 'normal' WHERE key = $1"),
                &[&key],
            )?;
            Ok(())
        })
    })
    .await
    .expect("test: publish task")
    .expect("test: mark the row public");
}

pub(super) async fn r5_drop_schema(db_url: String, schema: String) {
    tokio::task::spawn_blocking(move || {
        let mut client = db_url
            .parse::<postgres::Config>()
            .expect("test: parse the postgres url")
            .connect(NoTls)
            .expect("test: connect to postgres");
        client
            .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_identifier(&schema)))
            .expect("test: drop the test schema");
    })
    .await
    .expect("test: schema drop task");
}

/// R-INT2A F1, the offline half: the write path a plugin's `memory.store`
/// takes (`store_with_metadata`, carrying only the plugin's channel)
/// stays system-scoped here. It computes the embedding as the system and
/// never applies a caller's RLS context, which would demand a workspace
/// and owner a plugin does not have.
///
/// MUTATION GUARD: route `store_with_metadata` through
/// `store_with_context_and_metadata` or `apply_rls_context_raw` and this
/// goes red.
#[test]
fn postgres_metadata_writes_stay_system_scoped_for_plugin_rows() {
    let production = r5_production_source();
    let declaration = "    async fn store_with_metadata(";
    let method = r5_without_comments(r5_method_text(production, declaration));
    assert!(
        method.contains("embedding_metadata_for_category(&category, content, None, None, true)"),
        "{declaration} must compute its embedding with system scope"
    );
    for caller_scoped in [
        "apply_rls_context_raw",
        "store_with_context_and_metadata",
        "resolve_principal",
    ] {
        assert!(
            !method.contains(caller_scoped),
            "{declaration} must not take the caller-scoped path ({caller_scoped})"
        );
    }
}

/// R-INT2A F1, the live half: a plugin's `memory.store` succeeds on
/// PostgreSQL and the row is readable. **NOT RUN on this host: no
/// PostgreSQL.**
#[cfg(feature = "wasm-plugins")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_plugin_memory_store_succeeds() {
    let db_url = live_postgres_url();
    let schema = format!("prx_int2af_plugin_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
    let key = crate::plugins::capabilities::common::store_plugin_memory(&mem, "sample-plugin", "plugin text", "core")
        .await
        .expect("test: a plugin write succeeds on PostgreSQL");
    let stored = mem.get(&key).await.expect("test: read back");
    assert_eq!(stored.map(|entry| entry.content).as_deref(), Some("plugin text"));
    drop(mem);
    r5_drop_schema(db_url, schema).await;
}

/// The direct analogue of the C03 session/private matrix, over `memories`.
///
/// Under a NOSUPERUSER/NOBYPASSRLS role RLS applies to every read here, so
/// issuer B sees issuer A's `public` row only through the read-only
/// `memories_public_read` policy (D-028); before it, RLS hid that row and
/// the `r5-a-public` assertion failed on such a role. Under a superuser the
/// RLS half is bypassed and only the SQL predicate is exercised.
/// **NOT RUN on this host: no PostgreSQL.**
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_owner_isolated_memory_recall_matrix_for_the_same_subject() {
    let db_url = live_postgres_url();
    let schema = format!("prx_r5_recall_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");

    r5_seed(&mem, &owner_a, "r5-a", "shared keyword first issuer note").await;
    r5_seed(&mem, &owner_b, "r5-b", "shared keyword second issuer note").await;
    r5_seed(&mem, &owner_a, "r5-a-public", "shared keyword announcement").await;
    mem.store(
        "r5-legacy",
        "shared keyword unattributed note",
        MemoryCategory::Conversation,
        None,
    )
    .await
    .expect("test: store a legacy row with no owner");
    r5_make_public(&mem, "r5-a-public").await;

    let keys = |entries: Vec<MemoryEntry>| {
        entries
            .into_iter()
            .map(|entry| entry.key)
            .collect::<std::collections::HashSet<_>>()
    };

    let seen_by_b = keys(
        mem.recall_with_context("shared keyword", 50, None, Some(&r5_context(Some(&owner_b), true)))
            .await
            .expect("test: isolated recall as issuer B"),
    );
    assert!(!seen_by_b.contains("r5-a"), "{seen_by_b:?}");
    assert!(!seen_by_b.contains("r5-legacy"), "{seen_by_b:?}");
    assert!(seen_by_b.contains("r5-b"), "{seen_by_b:?}");
    assert!(seen_by_b.contains("r5-a-public"), "{seen_by_b:?}");

    let seen_by_a = keys(
        mem.recall_with_context("shared keyword", 50, None, Some(&r5_context(Some(&owner_a), true)))
            .await
            .expect("test: isolated recall as issuer A"),
    );
    assert!(seen_by_a.contains("r5-a"), "{seen_by_a:?}");
    assert!(!seen_by_a.contains("r5-b"), "{seen_by_a:?}");

    // Observe must not reopen the boundary (the backend mode override).
    let observed = keys(
        mem.recall_with_context_mode(
            "shared keyword",
            50,
            None,
            Some(&r5_context(Some(&owner_b), true)),
            MemoryReadMode::Observe,
        )
        .await
        .expect("test: observe-mode recall under isolation"),
    );
    assert!(!observed.contains("r5-a"), "{observed:?}");

    drop(mem);
    r5_drop_schema(db_url, schema).await;
}

/// The same matrix for the exact-key read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_owner_isolated_memory_get_matrix_for_the_same_subject() {
    let db_url = live_postgres_url();
    let schema = format!("prx_r5_get_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");

    r5_seed(&mem, &owner_a, "r5-a", "shared keyword first issuer note").await;

    assert!(
        mem.get_with_context("r5-a", Some(&r5_context(Some(&owner_b), true)))
            .await
            .expect("test: isolated keyed read as issuer B")
            .is_none(),
        "an exact key must not cross the owner boundary"
    );
    assert!(
        mem.get_with_context("r5-a", Some(&r5_context(Some(&owner_a), true)))
            .await
            .expect("test: isolated keyed read as issuer A")
            .is_some(),
        "the owning caller must still read its own row"
    );
    assert!(
        mem.get_with_context_mode("r5-a", Some(&r5_context(Some(&owner_b), true)), MemoryReadMode::Observe)
            .await
            .expect("test: observe-mode keyed read under isolation")
            .is_none(),
        "Observe must not reopen the owner boundary"
    );
    // F1 parity: this is an empty answer, not an error. The isolated
    // no-owner case is decided before the RLS context is applied, so
    // PostgreSQL returns `Ok(None)` exactly as SQLite returns no rows.
    assert!(
        mem.get_with_context("r5-a", Some(&r5_context(None, true)))
            .await
            .expect("test: isolated keyed read with no owner id")
            .is_none(),
        "an isolated caller with no owner id must read nothing owned"
    );
    assert!(
        mem.recall_with_context("shared keyword", 50, None, Some(&r5_context(None, true)))
            .await
            .expect("test: isolated recall with no owner id")
            .is_empty(),
        "the recall half of the same no-owner case must also answer empty"
    );

    drop(mem);
    r5_drop_schema(db_url, schema).await;
}

/// The RLS owner an isolated read sets must be the same string the SQL
/// predicate gates on. Guards the `recall_with_context` /
/// `get_with_context` `rls_owner` change: if the two drifted apart, a read
/// whose `sender_id` differs from its `owner_id` would silently fall back
/// to the RLS policy alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_owner_isolated_rls_owner_matches_the_predicate_owner() {
    let db_url = live_postgres_url();
    let schema = format!("prx_r5_rls_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
    let owner_a = c03_owner("issuer-a");

    r5_seed(&mem, &owner_a, "r5-a", "shared keyword first issuer note").await;

    // `sender_id` deliberately disagrees with `owner_id`: the RLS owner and
    // the predicate owner must both follow `owner_id` under isolation.
    let mut divergent = r5_context(Some(&owner_a), true);
    divergent.sender_id = Some("some-other-anchor".to_string());
    assert!(
        mem.get_with_context("r5-a", Some(&divergent))
            .await
            .expect("test: isolated keyed read with a divergent sender anchor")
            .is_some(),
        "the RLS owner must follow owner_id, not sender_id, under isolation"
    );

    drop(mem);
    r5_drop_schema(db_url, schema).await;
}

/// R-B1d D-1, the delete half. The twin of
/// `sqlite_owner_isolated_forget_excludes_another_owners_row_by_key` and
/// its public-row case, over the one `DELETE` statement this adapter runs.
///
/// **NOT RUN on a host without PostgreSQL:** it is `#[ignore]`d there, so
/// the runner counts it as ignored rather than printing a vacuous `ok`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_owner_isolated_forget_is_owner_qualified() {
    let db_url = live_postgres_url();
    let schema = format!("prx_d1_forget_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");

    r5_seed(&mem, &owner_a, "d1-a", "shared keyword first issuer note").await;
    assert!(
        mem.get_with_context("d1-a", Some(&r5_context(Some(&owner_a), true)))
            .await
            .expect("test: read back the fixture")
            .is_some(),
        "sanity: the fixture row must exist, or every assertion below passes vacuously"
    );

    assert!(
        !mem.forget_with_context("d1-a", Some(&r5_context(Some(&owner_b), true)))
            .await
            .expect("test: isolated forget as issuer B"),
        "an isolated caller must not delete another owner's row by key"
    );
    // A blank owner is decided before any statement or RLS call runs, so
    // this is `Ok(false)` and not the "requires a trusted owner context"
    // error `apply_rls_context_raw` would raise.
    for blank in [None, Some("")] {
        assert!(
            !mem.forget_with_context("d1-a", Some(&r5_context(blank, true)))
                .await
                .expect("test: isolated forget with no owner id"),
            "an isolated caller with owner {blank:?} must delete nothing"
        );
    }
    // The public arm the shared visibility predicate leaves open for reads.
    r5_make_public(&mem, "d1-a").await;
    assert!(
        !mem.forget_with_context("d1-a", Some(&r5_context(Some(&owner_b), true)))
            .await
            .expect("test: isolated forget of a public row as issuer B"),
        "a public row is shared for reading, but it is still another owner's row to delete"
    );

    // Positive control: the owner still deletes its own row.
    assert!(
        mem.forget_with_context("d1-a", Some(&r5_context(Some(&owner_a), true)))
            .await
            .expect("test: isolated forget as issuer A"),
        "the owning caller must still delete its own row"
    );
    assert!(
        mem.get_with_context("d1-a", Some(&r5_context(Some(&owner_a), true)))
            .await
            .expect("test: read back after the delete")
            .is_none(),
        "the row must really be gone"
    );

    drop(mem);
    r5_drop_schema(db_url, schema).await;
}

/// The placement, at the level that runs on a host without PostgreSQL: the
/// isolated no-owner delete is decided before the RLS context is applied,
/// exactly as the two keyed reads decide it.
///
/// MUTATION GUARD: move the short-circuit below `apply_rls_context_raw`,
/// or let it fall through instead of returning `Ok(false)`, and this goes
/// red.
#[test]
fn postgres_isolated_forget_short_circuits_before_applying_the_rls_owner() {
    let production = r5_production_source();
    let declaration = "    async fn forget_with_context(";
    let method = r5_without_comments(r5_method_text(production, declaration));
    let short_circuit = method
        .find("isolated_read_without_owner")
        .unwrap_or_else(|| panic!("{declaration} must short-circuit an isolated delete with no owner id"));
    let rls = method
        .find("apply_rls_context_raw")
        .unwrap_or_else(|| panic!("{declaration} must still apply the RLS context"));
    assert!(
        short_circuit < rls,
        "{declaration} must decide the empty answer before applying the RLS owner, \
             otherwise a blank owner errors instead of deleting nothing"
    );
    // R-B1g F-6: the call being first is not enough; the branch must
    // answer. Falling through would still apply the RLS owner.
    assert!(
        method
            .get(short_circuit..rls)
            .is_some_and(|between| between.contains("return Ok(false)")),
        "{declaration} must return `Ok(false)` from the short-circuit, not fall through to the delete"
    );
}

/// The guard itself, runnable without a database: its decision, its
/// placeholder, and that a non-isolated delete renders no fragment at all
/// so its statement and bind list stay byte-identical.
#[test]
fn postgres_forget_owner_guard_binds_only_for_an_isolated_caller_with_an_owner() {
    let owner_a = c03_owner("issuer-a");

    let mut unisolated = r5_context(Some(&owner_a), false);
    unisolated.sender_id = Some(owner_a.clone());
    let guard = PostgresMemory::forget_owner_guard(&unisolated, 9);
    assert_eq!(guard.sql, "", "a non-isolated delete must render no fragment at all");
    assert!(!guard.bound_owner);
    assert_eq!(PostgresMemory::forget_owner_anchor(&unisolated), None);

    let isolated = r5_context(Some(&owner_a), true);
    let guard = PostgresMemory::forget_owner_guard(&isolated, 9);
    assert_eq!(guard.sql, " AND owner_id = $9");
    assert!(guard.bound_owner, "one marker, one parameter");
    assert_eq!(
        PostgresMemory::forget_owner_anchor(&isolated).as_deref(),
        Some(owner_a.as_str())
    );

    // The blank case never reaches the guard — the short-circuit above
    // answers first — but the seam must still fail closed if it ever did.
    for blank in [None, Some(""), Some("   ")] {
        let context = r5_context(blank, true);
        assert!(PostgresMemory::isolated_read_without_owner(&context));
        let guard = PostgresMemory::forget_owner_guard(&context, 9);
        assert_eq!(guard.sql, " AND FALSE", "owner {blank:?} must fail closed");
        assert!(!guard.bound_owner, "a fail-closed guard binds nothing");
    }
}

/// Pin what the unscoped compatibility reads do with **no** RLS context.
///
/// `recall()` and `get()` never call `apply_rls_context_raw`, so their
/// result depends on whether RLS applies to the connected role: an
/// unprivileged role sees nothing, a superuser / `BYPASSRLS` role sees
/// everything. This records that split as a stated contract instead of an
/// accident, and asserts the one thing that must hold either way: the
/// *scoped* read is unaffected by it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_unscoped_recall_behaviour_under_rls_is_pinned() {
    let db_url = live_postgres_url();
    let schema = format!("prx_r5_unscoped_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");

    r5_seed(&mem, &owner_a, "r5-a", "shared keyword first issuer note").await;

    let unscoped = mem
        .recall("shared keyword", 50, None)
        .await
        .expect("test: unscoped recall");
    let bypasses_rls = unscoped.iter().any(|entry| entry.key == "r5-a");

    let scoped = mem
        .recall_with_context("shared keyword", 50, None, Some(&r5_context(Some(&owner_b), true)))
        .await
        .expect("test: isolated recall as issuer B");
    assert!(
        !scoped.iter().any(|entry| entry.key == "r5-a"),
        "the scoped read must isolate regardless of whether the connected role \
             bypasses RLS (observed bypass: {bypasses_rls})"
    );

    drop(mem);
    r5_drop_schema(db_url, schema).await;
}

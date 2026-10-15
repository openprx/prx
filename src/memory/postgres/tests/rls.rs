//! Row-level-security policy tests, live and offline.
#![cfg(test)]

use super::CountingEmbedding;
use super::memory_owner_isolation::{FILE_SEPARATOR, r5_drop_schema, r5_production_source, r5_seed, r5_without_comments};
use super::owner_isolation::{C03_WORKSPACE, c03_owner};
use crate::memory::postgres::pool::{PostgresConnectionPool, PostgresSessionContext};
use crate::memory::postgres::rows::quote_identifier;
use crate::memory::postgres::schema::{MEMORIES_PUBLIC_READ_POLICY, PolicyRow, check_policy_inventory};
use crate::memory::postgres::{PostgresMemory, live_postgres_url};
use crate::memory::principal::MemoryWriteContext;
use crate::memory::traits::{Memory, MemoryCategory, MemoryStoreMetadata};
use anyhow::Result;
use postgres::NoTls;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_vector_rls_denies_missing_and_cross_owner_context_from_env() {
    let db_url = live_postgres_url();
    let schema = format!("prx_vector_rls_test_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::with_embedder(
        &db_url,
        &schema,
        "memories",
        Some(5),
        Arc::new(CountingEmbedding {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        1.0,
        0.0,
    )
    .unwrap();

    let context = |workspace: &str, owner: &str| MemoryWriteContext {
        workspace_id: Some(workspace.to_string()),
        channel: Some("test".to_string()),
        chat_type: Some("private".to_string()),
        chat_id: Some(format!("chat-{owner}")),
        sender_id: Some(owner.to_string()),
        raw_sender: Some(owner.to_string()),
        owner_id: None,
        require_owner_match: false,
    };
    let metadata = |workspace: &str, owner: &str| MemoryStoreMetadata {
        workspace_id: Some(workspace.to_string()),
        owner_id: Some(owner.to_string()),
        ..MemoryStoreMetadata::default()
    };
    let owner_a = context("workspace-a", "owner-a");
    let owner_b = context("workspace-a", "owner-b");
    let other_workspace = context("workspace-b", "owner-a");

    mem.store_with_context_and_metadata(
        "vector-owner-a",
        "isolation-token shared",
        MemoryCategory::Core,
        None,
        Some(&owner_a),
        metadata("workspace-a", "owner-a"),
    )
    .await
    .unwrap();
    mem.store_with_context_and_metadata(
        "vector-owner-b",
        "isolation-token shared",
        MemoryCategory::Core,
        None,
        Some(&owner_b),
        metadata("workspace-a", "owner-b"),
    )
    .await
    .unwrap();

    let visible_a = mem
        .recall_with_context("isolation-token", 10, None, Some(&owner_a))
        .await
        .unwrap();
    assert_eq!(visible_a.len(), 1);
    assert_eq!(visible_a.first().unwrap().key, "vector-owner-a");
    let visible_b = mem
        .recall_with_context("isolation-token", 10, None, Some(&owner_b))
        .await
        .unwrap();
    assert_eq!(visible_b.len(), 1);
    assert_eq!(visible_b.first().unwrap().key, "vector-owner-b");
    assert!(
        mem.recall_with_context("isolation-token", 10, None, Some(&other_workspace))
            .await
            .unwrap()
            .is_empty()
    );

    let missing_workspace = MemoryWriteContext {
        workspace_id: None,
        ..owner_a.clone()
    };
    let missing_error = mem
        .recall_with_context("isolation-token", 10, None, Some(&missing_workspace))
        .await
        .unwrap_err();
    assert!(missing_error.to_string().contains("trusted workspace context"));

    let cache_table = mem.qualified_embedding_cache_table.clone();
    let memories_table = mem.qualified_table.clone();
    let chunks_table = mem.qualified_document_chunks_table.clone();
    let scoped_cache_rows = tokio::task::spawn_blocking({
        let client = Arc::clone(&mem.client);
        move || {
            client.with_client(|client| {
                let count: i64 = client
                    .query_one(
                        &format!(
                            "SELECT COUNT(*) FROM {cache_table} \
                                 WHERE workspace_id = 'workspace-a' \
                                 AND owner_id IN ('owner-a', 'owner-b')"
                        ),
                        &[],
                    )?
                    .get(0);
                for table in [&memories_table, &chunks_table, &cache_table] {
                    let row = client.query_one(
                        "SELECT c.relrowsecurity, c.relforcerowsecurity \
                             FROM pg_class c WHERE c.oid = to_regclass($1::text)",
                        &[table],
                    )?;
                    assert!(row.get::<_, bool>(0));
                    assert!(row.get::<_, bool>(1));
                }
                Ok(count)
            })
        }
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        scoped_cache_rows, 2,
        "identical model/cache dimensions remain owner-scoped"
    );
    let evidence = PostgresMemory::verify_vector_isolation(&db_url, &schema, "memories", Some(5)).unwrap();
    assert!(evidence.starts_with("postgres:migration:21:sha256:"));
    assert!(evidence.contains(":policy-sha256:"));

    drop(mem);
    tokio::task::spawn_blocking(move || {
        let mut client = db_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
        client
            .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_identifier(&schema)))
            .unwrap();
    })
    .await
    .unwrap();
}

/// The security boundary that separates trusted runtime storage from
/// callers that ingest externally-originated data, verified against a live
/// PostgreSQL instance under a **non-superuser** role.
///
/// A superuser (or `BYPASSRLS`) role short-circuits every policy, so this
/// test asserts up front that the configured role is neither; running it as
/// `postgres` would pass vacuously and prove nothing.
///
/// Three properties, in the order they matter:
/// 1. the restricted context is denied a write to the RLS-protected
///    `memories` table -- the pre-pool boundary, reproduced;
/// 2. the system context on the *same pool* is allowed that write -- so
///    property 1 is a real difference, not a broken fixture;
/// 3. a connection released by a restricted checkout comes back with its
///    bypass intact for the next system checkout -- the contexts cannot
///    contaminate each other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_pool_session_contexts_are_isolated_from_env() {
    let db_url = live_postgres_url();
    let schema = format!("prx_ctx_{}", Uuid::new_v4().simple());

    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
    let qualified = format!("{}.{}", quote_identifier(&schema), quote_identifier("memories"));

    // A pool of exactly one connection, so "the next checkout" is
    // necessarily the very same backend rather than a fresh one that would
    // hide a leak behind a clean session.
    let pool = Arc::new(PostgresConnectionPool::new(db_url.clone(), Some(5), 1));

    let insert_sql = format!(
        "INSERT INTO {qualified} (id, key, content, category, created_at, updated_at, \
                                      workspace_id, owner_id, visibility) \
             VALUES ($1, $2, 'ctx probe', 'conversation', NOW(), NOW(), $3, $4, 'owner')"
    );

    let probe_pool = Arc::clone(&pool);
    let probe_sql = insert_sql.clone();
    let qualified_probe = qualified.clone();
    tokio::task::spawn_blocking(move || {
        // Guard: an RLS-exempt role makes every assertion below meaningless.
        let (is_super, bypasses): (bool, bool) = probe_pool
            .with_client_in(PostgresSessionContext::System, |client| {
                let row = client.query_one(
                    "SELECT rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user",
                    &[],
                )?;
                Ok((row.get(0), row.get(1)))
            })
            .unwrap();
        assert!(
            !is_super && !bypasses,
            "this test only proves anything under a NOSUPERUSER/NOBYPASSRLS role; \
                 point OPENPRX_TEST_POSTGRES_URL at an unprivileged role"
        );

        // 1. Restricted context: RLS applies, no owner context is bound, so
        //    both policy branches are false and the write is rejected.
        let restricted_backend: i32 = {
            let mut checkout = probe_pool.checkout_in(PostgresSessionContext::Restricted).unwrap();
            let client = checkout.client_mut().unwrap();

            let bypass: String = client
                .query_one("SELECT current_setting('prx.rls_bypass', true)", &[])
                .unwrap()
                .get(0);
            assert_eq!(bypass, "off", "restricted checkout must not carry an RLS bypass");

            let denied = client.execute(
                &probe_sql,
                &[
                    &Uuid::new_v4().to_string(),
                    &format!("restricted:{}", Uuid::new_v4()),
                    &"ws-probe".to_string(),
                    &"owner-probe".to_string(),
                ],
            );
            let error = denied.expect_err("restricted context must be denied by row-level security");
            // `postgres::Error` renders as a bare "db error", so the
            // assertion has to reach into the `DbError`. SQLSTATE 42501
            // alone is not enough: it also covers a plain `permission
            // denied for table`, which would let a mis-granted fixture
            // masquerade as a policy denial. The message pins it to RLS.
            let db_error = error
                .as_db_error()
                .unwrap_or_else(|| panic!("expected a PostgreSQL error, got: {error}"));
            assert_eq!(
                db_error.code(),
                &postgres::error::SqlState::INSUFFICIENT_PRIVILEGE,
                "expected SQLSTATE 42501, got: {}",
                db_error.message()
            );
            assert!(
                db_error.message().contains("row-level security"),
                "expected a row-level security denial, got: {}",
                db_error.message()
            );

            client
                .query_one("SELECT pg_backend_pid()", &[])
                .unwrap()
                .get::<_, i32>(0)
        };

        // 2. System context on the same pool: the write succeeds, proving
        //    the denial above is the context and not the fixture.
        //
        // 3. The connection is the one the restricted checkout just
        //    released, and its bypass is back -- no contamination in
        //    either direction.
        let system_backend: i32 = probe_pool
            .with_client_in(PostgresSessionContext::System, |client| {
                let bypass: String = client
                    .query_one("SELECT current_setting('prx.rls_bypass', true)", &[])?
                    .get(0);
                assert_eq!(
                    bypass, "on",
                    "a restricted checkout must not strip the bypass from the recycled connection"
                );
                client.execute(
                    &probe_sql,
                    &[
                        &Uuid::new_v4().to_string(),
                        &"system:probe".to_string(),
                        &"ws-probe".to_string(),
                        &"owner-probe".to_string(),
                    ],
                )?;
                let rows: i64 = client
                    .query_one(&format!("SELECT COUNT(*) FROM {qualified_probe}"), &[])?
                    .get(0);
                assert_eq!(rows, 1, "only the system write landed");
                Ok(client.query_one("SELECT pg_backend_pid()", &[])?.get::<_, i32>(0))
            })
            .unwrap();

        assert_eq!(
            restricted_backend, system_backend,
            "the cross-contamination check is only meaningful on a reused backend"
        );

        // Alternating contexts on that same single connection stay stable
        // rather than degrading after the first hand-back.
        for _ in 0..3 {
            let mut restricted = probe_pool.checkout_in(PostgresSessionContext::Restricted).unwrap();
            let value: String = restricted
                .client_mut()
                .unwrap()
                .query_one("SELECT current_setting('prx.rls_bypass', true)", &[])
                .unwrap()
                .get(0);
            assert_eq!(value, "off");
            drop(restricted);

            let value: String = probe_pool
                .with_client_in(PostgresSessionContext::System, |client| {
                    Ok(client
                        .query_one("SELECT current_setting('prx.rls_bypass', true)", &[])?
                        .get::<_, String>(0))
                })
                .unwrap();
            assert_eq!(value, "on");
        }
    })
    .await
    .unwrap();

    drop(mem);
    drop(pool);
    tokio::task::spawn_blocking(move || {
        let mut client = db_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
        client
            .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_identifier(&schema)))
            .unwrap();
    })
    .await
    .unwrap();
}

// ---------------------------------------------------------------------
// OQ-8 / D-019 (B2): the vector RLS policies close their shared
// `global`/`workspace` read arm to owner-isolated callers, so the policy
// floor matches the shared SQL predicate instead of being wider than it.
//
// The policy text, the transaction flag and the call-site bits are pinned
// offline below. The behavioural twin needs a live PostgreSQL and a role
// RLS applies to; it is **written but not run** on the authoring host.
// ---------------------------------------------------------------------

pub(super) fn b2_collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Split one policy DDL into its `USING` and `WITH CHECK` halves.
pub(super) fn b2_policy_halves(sql: &str) -> (String, String) {
    let sql = b2_collapse_whitespace(sql);
    let (using, check) = sql
        .split_once("WITH CHECK")
        .expect("test: the policy DDL has a WITH CHECK clause");
    (using.to_string(), check.to_string())
}

pub(super) const B2_ISOLATION_GATE: &str = "COALESCE(current_setting('prx.owner_isolated', true), '') <> 'on'";

#[test]
fn vector_rls_shared_read_arm_is_closed_to_owner_isolated_callers() {
    for policy in ["memories_owner_isolation", "document_chunks_owner_isolation"] {
        let (using, check) = b2_policy_halves(&PostgresMemory::vector_rls_policy_sql("s.t", policy, true));
        assert!(
            using.contains(&format!(
                "OR (visibility IN ('global', 'workspace') AND {B2_ISOLATION_GATE}))"
            )),
            "{policy}: the shared read arm must be gated on the caller not being \
                 owner-isolated: {using}"
        );
        assert!(
            using.contains("(owner_id = current_setting('prx.current_owner', true) OR"),
            "{policy}: the caller's own rows stay readable: {using}"
        );
        for required in ["prx.current_workspace", "prx.current_owner", "prx.rls_bypass"] {
            assert!(
                using.contains(required) && check.contains(required),
                "{policy}: {required}"
            );
        }
        assert!(
            !check.contains("visibility") && !check.contains("prx.owner_isolated"),
            "{policy}: writes stay owner exact and independent of the flag: {check}"
        );
        assert!(
            check.contains("AND owner_id = current_setting('prx.current_owner', true)"),
            "{policy}: {check}"
        );
    }
}

#[test]
fn vector_rls_embedding_cache_policy_has_no_shared_arm() {
    let (using, check) = b2_policy_halves(&PostgresMemory::vector_rls_policy_sql(
        "s.t",
        "embedding_cache_owner_isolation",
        false,
    ));
    for half in [&using, &check] {
        assert!(!half.contains("visibility"), "{half}");
        assert!(!half.contains("prx.owner_isolated"), "{half}");
        assert!(
            half.contains("owner_id = current_setting('prx.current_owner', true)"),
            "{half}"
        );
    }
}

/// `apply_rls_context_raw` must set the flag the policy reads, always
/// (both values), transaction-local, and only on the non-system path.
#[test]
fn apply_rls_context_raw_sets_the_owner_isolation_flag() {
    let source = r5_production_source();
    let start = source
        .find("fn apply_rls_context_raw<C: GenericClient>(")
        .expect("test: apply_rls_context_raw exists");
    let body = &source[start..];
    let body = body.split(FILE_SEPARATOR).next().unwrap_or(body);
    let body = &body[..body.find("\n    }\n").expect("test: apply_rls_context_raw ends")];
    let bypass_return = body.find("return Ok(());").expect("test: system early return");
    let flag = body
        .find("set_config('prx.owner_isolated', $2, true)")
        .expect("test: the owner isolation flag is set transaction-local");
    assert!(flag > bypass_return, "the flag is set on the non-system path");
    assert!(body.contains("if owner_isolated { \"on\" } else { \"off\" }"), "{body}");
}

/// Every production call site passes the caller's isolation bit. A literal
/// `false` is only acceptable where the policy has no shared arm (the
/// embedding cache); ingest passes `true` because all its statements are
/// exact on the document's own owner.
#[test]
fn apply_rls_context_raw_call_sites_pass_the_callers_isolation_bit() {
    let source = r5_without_comments(r5_production_source());
    let mut last_args = Vec::new();
    for (offset, _) in source.match_indices("Self::apply_rls_context_raw(") {
        let call = &source[offset..];
        let call = &call[..call.find(")?;").expect("test: call ends with )?;")];
        let args = b2_collapse_whitespace(&call["Self::apply_rls_context_raw(".len()..]);
        let last = args
            .trim_end_matches(',')
            .rsplit(',')
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        last_args.push(last);
    }
    let count = |value: &str| last_args.iter().filter(|arg| arg.as_str() == value).count();
    assert_eq!(count("false"), 2, "only the two embedding-cache calls: {last_args:?}");
    assert_eq!(count("true"), 1, "only ingest: {last_args:?}");
    assert!(
        last_args
            .iter()
            .all(|arg| ["false", "true", "rls_owner_isolated", "owner_isolated"].contains(&arg.as_str())),
        "{last_args:?}"
    );
    assert!(count("rls_owner_isolated") >= 7, "{last_args:?}");
}

#[test]
fn vector_rls_owner_isolated_reads_is_a_recorded_migration() {
    let registry = PostgresMemory::memory_schema_migration_registry();
    assert!(
        registry.windows(2).all(|pair| matches!(pair, [a, b] if a.0 < b.0)),
        "migration versions stay strictly increasing"
    );
    let (_, name, descriptor) = registry
        .iter()
        .find(|(version, _, _)| *version == 23)
        .expect("test: OQ-8 migration 23 is registered");
    assert_eq!(*name, "vector_rls_owner_isolated_reads");
    assert_eq!(
        *descriptor,
        "memories + document_chunks POLICY *_owner_isolation USING: shared global/workspace read arm additionally requires COALESCE(current_setting('prx.owner_isolated', true), '') <> 'on'; prx.owner_isolated set transaction-local from require_owner_match; WITH CHECK and embedding_cache unchanged",
        "migration 23's recorded text must not change (checksum mismatch bails at startup)"
    );
    let (_, _, a04) = registry
        .iter()
        .find(|(version, _, _)| *version == 21)
        .expect("test: A04 migration 21 is registered");
    assert!(
        a04.starts_with("memories + document_chunks + embedding_cache scoped by workspace_id/owner_id"),
        "migration 21's recorded text must not change (checksum mismatch bails at startup)"
    );
}

// ---------------------------------------------------------------------
// D-028 (B2-f): `public` memory rows are readable across owners through a
// separate, read-only `FOR SELECT` policy; the owner-isolation policy that
// also governs writes never names `public`. Pinned offline; the live
// behaviour is covered by the recall matrix above, NOT RUN on this host.
// ---------------------------------------------------------------------

#[test]
fn memories_public_read_policy_is_read_only_and_workspace_scoped() {
    let sql = b2_collapse_whitespace(&PostgresMemory::memories_public_read_policy_sql(
        "s.memories",
        MEMORIES_PUBLIC_READ_POLICY,
    ));
    assert!(
        sql.contains("CREATE POLICY memories_public_read ON s.memories FOR SELECT USING ("),
        "the public grant must be a FOR SELECT policy on memories: {sql}"
    );
    assert!(
        !sql.contains("WITH CHECK"),
        "a read-only policy has no WITH CHECK: {sql}"
    );
    for command in ["FOR ALL", "FOR UPDATE", "FOR DELETE", "FOR INSERT", "AS RESTRICTIVE"] {
        assert!(!sql.contains(command), "{command}: {sql}");
    }
    for required in [
        "NULLIF(current_setting('prx.current_workspace', true), '') IS NOT NULL",
        "NULLIF(current_setting('prx.current_owner', true), '') IS NOT NULL",
        "workspace_id = current_setting('prx.current_workspace', true)",
        "visibility = 'public'",
    ] {
        assert!(sql.contains(required), "{required}: {sql}");
    }
    assert!(
        !sql.contains("prx.owner_isolated") && !sql.contains("prx.rls_bypass") && !sql.contains("owner_id"),
        "public reads are the same for every caller and are not an owner grant: {sql}"
    );
}

#[test]
fn owner_isolation_policies_never_grant_public_rows() {
    for (policy, shared) in [
        ("memories_owner_isolation", true),
        ("document_chunks_owner_isolation", true),
        ("embedding_cache_owner_isolation", false),
    ] {
        let sql = PostgresMemory::vector_rls_policy_sql("s.t", policy, shared);
        assert!(
            !sql.contains("'public'"),
            "{policy}: the policy that also governs writes must not name `public`: {sql}"
        );
    }
}

/// `init_vector_rls` installs the public grant exactly once, on the
/// memories table, inside the same transaction as the isolation policies.
#[test]
fn init_vector_rls_installs_the_public_read_policy_on_memories_only() {
    let source = r5_production_source();
    let start = source
        .find("fn init_vector_rls(")
        .expect("test: init_vector_rls exists");
    let body = &source[start..];
    let body = body.split(FILE_SEPARATOR).next().unwrap_or(body);
    let body = b2_collapse_whitespace(&body[..body.find("\n    }\n").expect("test: init_vector_rls ends")]);
    assert_eq!(body.matches("memories_public_read_policy_sql(").count(), 1, "{body}");
    assert!(
            body.contains(
                "tx.batch_execute(&Self::memories_public_read_policy_sql( qualified_memories_table, MEMORIES_PUBLIC_READ_POLICY, ))?; tx.commit()?;"
            ),
            "{body}"
        );
}

/// Migration 24's descriptor is checksummed into every database that
/// recorded it, so any edit to the text bails at startup there. It is
/// pinned verbatim and looked up by version, so appending migration 25
/// leaves this test alone (R-B2 F-6).
#[test]
fn memories_public_read_policy_is_a_recorded_migration() {
    let (_, name, descriptor) = PostgresMemory::memory_schema_migration_registry()
        .iter()
        .find(|(version, _, _)| *version == 24)
        .expect("test: migration 24 is registered");
    assert_eq!(*name, "memories_public_read_policy");
    assert_eq!(
        *descriptor,
        "memories POLICY memories_public_read FOR SELECT USING: transaction-local workspace and owner present AND workspace_id = prx.current_workspace AND visibility = 'public'; permissive, read only (UPDATE/DELETE/INSERT still need memories_owner_isolation); independent of prx.owner_isolated; document_chunks and embedding_cache unchanged"
    );
}

/// R-B2 F-5, the offline half: the evidence accepts exactly the policy
/// set `init_vector_rls` installs and refuses a stray policy, a missing
/// one, and a restrictive or role-limited variant.
#[test]
fn policy_inventory_is_exact_permissive_and_public() {
    let row = |name: &str, permissive: &str, roles: &[&str]| PolicyRow {
        name: name.to_string(),
        permissive: permissive.to_string(),
        roles: roles.iter().map(ToString::to_string).collect(),
    };
    let expected = ["memories_owner_isolation", MEMORIES_PUBLIC_READ_POLICY];
    let installed = vec![
        row("memories_owner_isolation", "PERMISSIVE", &["public"]),
        row(MEMORIES_PUBLIC_READ_POLICY, "PERMISSIVE", &["public"]),
    ];
    check_policy_inventory("memories", &installed, &expected).expect("test: the installed set passes");

    let mut stray = installed;
    stray.push(row("dba_convenience_read", "PERMISSIVE", &["public"]));
    assert!(
        check_policy_inventory("memories", &stray, &expected).is_err(),
        "a stray policy"
    );

    let missing = vec![row("memories_owner_isolation", "PERMISSIVE", &["public"])];
    assert!(
        check_policy_inventory("memories", &missing, &expected).is_err(),
        "a missing policy"
    );

    let restrictive = vec![
        row("memories_owner_isolation", "PERMISSIVE", &["public"]),
        row(MEMORIES_PUBLIC_READ_POLICY, "RESTRICTIVE", &["public"]),
    ];
    assert!(
        check_policy_inventory("memories", &restrictive, &expected).is_err(),
        "a restrictive variant"
    );

    let role_limited = vec![
        row("memories_owner_isolation", "PERMISSIVE", &["public"]),
        row(MEMORIES_PUBLIC_READ_POLICY, "PERMISSIVE", &["reporting"]),
    ];
    assert!(
        check_policy_inventory("memories", &role_limited, &expected).is_err(),
        "a role-limited variant"
    );
}

pub(super) async fn b2_make_workspace(mem: &PostgresMemory, key: &str) {
    let client_slot = mem.client.clone();
    let qualified_table = mem.qualified_table.clone();
    let key = key.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        client_slot.with_client(|client| {
            client.execute(
                &format!("UPDATE {qualified_table} SET visibility = 'workspace' WHERE key = $1"),
                &[&key],
            )?;
            Ok(())
        })
    })
    .await
    .expect("test: visibility task")
    .expect("test: mark the row workspace-visible");
}

/// OQ-8 behavioural twin: under a role RLS applies to, a raw statement
/// with no SQL owner predicate still sees another owner's `workspace` row
/// for a non-isolated context, and does not for an isolated one.
/// **NOT RUN on this host: no PostgreSQL.**
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_vector_rls_hides_other_owners_workspace_rows_from_isolated_callers() {
    let db_url = live_postgres_url();
    let schema = format!("prx_b2_rls_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");
    r5_seed(&mem, &owner_a, "b2-a", "b2 workspace note").await;
    b2_make_workspace(&mem, "b2-a").await;

    let qualified_table = mem.qualified_table.clone();
    let url = db_url.clone();
    let (shared, isolated) = tokio::task::spawn_blocking(move || -> Result<(i64, i64)> {
        let mut client = url.parse::<postgres::Config>()?.connect(NoTls)?;
        let row = client.query_one(
            "SELECT rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user",
            &[],
        )?;
        let (is_super, bypasses): (bool, bool) = (row.get(0), row.get(1));
        anyhow::ensure!(
            !is_super && !bypasses,
            "this test only proves anything under a NOSUPERUSER/NOBYPASSRLS role"
        );
        let count_stmt = format!("SELECT COUNT(*) FROM {qualified_table} WHERE key = 'b2-a'");
        let mut probe = |isolated: bool| -> Result<i64> {
            let mut tx = client.transaction()?;
            PostgresMemory::apply_rls_context_raw(&mut tx, false, Some(C03_WORKSPACE), Some(&owner_b), isolated)?;
            let count: i64 = tx.query_one(&count_stmt, &[])?.get(0);
            tx.commit()?;
            Ok(count)
        };
        Ok((probe(false)?, probe(true)?))
    })
    .await
    .expect("test: rls probe task")
    .expect("test: rls probe");
    assert_eq!(shared, 1, "a non-isolated context keeps the tenant-wide workspace read");
    assert_eq!(
        isolated, 0,
        "an owner-isolated context must not see another owner's workspace row"
    );

    drop(mem);
    r5_drop_schema(db_url, schema).await;
}

//! The owner-isolated `memory_store` upsert.
#![cfg(test)]

use super::memory_owner_isolation::{r5_context, r5_drop_schema, r5_method_text, r5_production_source};
use super::owner_isolation::c03_owner;
use crate::memory::postgres::rows::quote_identifier;
use crate::memory::postgres::{PostgresMemory, live_postgres_url};
use crate::memory::traits::{Memory, MemoryCategory};
use uuid::Uuid;

// ---------------------------------------------------------------------
// V02-R9: the owner-isolated `memory_store` upsert.
//
// The pure test runs anywhere. The behavioural twin is gated like every
// other Postgres test in this module: `#[ignore]`d, run only with
// `OPENPRX_TEST_POSTGRES_URL` and `--ignored`, so it is **written but not
// run** on the authoring host, which has no PostgreSQL.
// ---------------------------------------------------------------------

/// The scoped upsert carries the shared conflict guard, refuses a write
/// the guard suppressed, and maps an RLS rejection of the existing row to
/// the same refusal; the context-free store path is untouched.
///
/// MUTATION GUARD: drop `{owner_guard}`, the zero-rows check or the
/// `42501` mapping and this goes red.
#[test]
fn postgres_scoped_store_upsert_carries_the_owner_conflict_guard_and_maps_both_refusals() {
    let production = r5_production_source();
    let scoped = r5_method_text(production, "async fn store_with_context_and_metadata(");
    for needle in [
        "isolated_store_owner(Some(&context))?",
        "policy_version = EXCLUDED.policy_version{owner_guard}",
        "store_conflict_guard(PG_DIALECT, &qualified_table, isolated_owner.is_some())",
        "SqlState::INSUFFICIENT_PRIVILEGE",
        "if isolated_owner.is_some() && written == 0 {",
    ] {
        assert!(
            scoped.contains(needle),
            "store_with_context_and_metadata lost `{needle}`"
        );
    }
    let plain = r5_method_text(production, "async fn store_with_metadata(");
    assert!(
        !plain.contains("owner_guard"),
        "a write without a context is never owner-isolated and keeps its historical statement"
    );
}

/// SP-MCP test 10. **NOT RUN on this host: no PostgreSQL.**
///
/// An owner-isolated caller that names another owner's key is refused and
/// changes nothing, for a `workspace`-visible existing row (admitted by the
/// RLS `USING` clause, so only the conflict guard stops it) and for a
/// non-shared one (where RLS may reject first; either way the caller sees
/// the one refusal). Its own key still updates. Needs a role RLS applies to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_owner_isolated_store_refuses_a_cross_owner_upsert() {
    let db_url = live_postgres_url();
    let schema = format!("prx_r9_store_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
    let qualified = format!("{}.{}", quote_identifier(&schema), quote_identifier("memories"));
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");

    for (key, visibility) in [("r9-shared-key", "workspace"), ("r9-private-key", "private")] {
        mem.store_with_context(
            key,
            "first owner note",
            MemoryCategory::Core,
            None,
            Some(&r5_context(Some(&owner_a), true)),
        )
        .await
        .expect("test: the first owner stores its key");
        let update = format!("UPDATE {qualified} SET visibility = $1 WHERE key = $2");
        mem.client
            .with_client(|client| {
                client.execute(&update, &[&visibility, &key])?;
                Ok(())
            })
            .expect("test: set the seeded row's visibility");

        let refused = mem
            .store_with_context(
                key,
                "second owner overwrite",
                MemoryCategory::Core,
                None,
                Some(&r5_context(Some(&owner_b), true)),
            )
            .await
            .expect_err("test: a cross-owner write must fail");
        assert_eq!(
            refused.to_string(),
            format!("key '{key}' is not available to this caller"),
            "{visibility}: one refusal, whichever layer stopped the write"
        );

        let select = format!("SELECT content, owner_id FROM {qualified} WHERE key = $1");
        let (content, owner): (String, Option<String>) = mem
            .client
            .with_client(|client| {
                let row = client.query_one(&select, &[&key])?;
                Ok((row.get(0), row.get(1)))
            })
            .expect("test: read the seeded row back");
        assert_eq!(content, "first owner note", "{visibility}: content untouched");
        assert_eq!(
            owner.as_deref(),
            Some(owner_a.as_str()),
            "{visibility}: owner untouched"
        );
    }

    mem.store_with_context(
        "r9-shared-key",
        "first owner update",
        MemoryCategory::Core,
        None,
        Some(&r5_context(Some(&owner_a), true)),
    )
    .await
    .expect("test: the owner still updates its own key");

    drop(mem);
    r5_drop_schema(db_url, schema).await;
}

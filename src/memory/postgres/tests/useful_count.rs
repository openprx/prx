//! `increment_useful_count` against a live PostgreSQL.
#![cfg(test)]

use super::memory_owner_isolation::r5_drop_schema;
use crate::memory::postgres::{PostgresMemory, live_postgres_url};
use crate::memory::traits::{Memory, MemoryCategory};
use uuid::Uuid;

/// Each call adds exactly one to the stored count, as the SQLite twin does,
/// and an unknown id changes nothing and is not an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_increment_useful_count_updates_the_row() {
    let db_url = live_postgres_url();
    let schema = format!("prx_useful_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");

    mem.store("useful_key", "useful fact", MemoryCategory::Core, None)
        .await
        .expect("test: store the row");
    let entry = mem
        .get("useful_key")
        .await
        .expect("test: read the row")
        .expect("test: the row exists");
    assert_eq!(entry.useful_count, Some(0));

    mem.increment_useful_count(&entry.id)
        .await
        .expect("test: first increment");
    mem.increment_useful_count(&entry.id)
        .await
        .expect("test: second increment");
    mem.increment_useful_count("no-such-id")
        .await
        .expect("test: an unknown id is not an error");

    let updated = mem
        .get("useful_key")
        .await
        .expect("test: read the row again")
        .expect("test: the row still exists");
    assert_eq!(updated.useful_count, Some(2));

    drop(mem);
    r5_drop_schema(db_url, schema).await;
}

//! Phase 4: Cross-module integration tests — concurrent agent turns with shared memory.
//!
//! Validates that multiple parallel agent invocations can safely read/write
//! to the same SQLite memory backend without data loss or corruption.

use openprx::memory::sqlite::SqliteMemory;
use openprx::memory::traits::{Memory, MemoryCategory};
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// Concurrent writes from independent "agents" sharing the same memory
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn parallel_stores_no_data_loss() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mem = Arc::new(SqliteMemory::new(tmp.path()).unwrap());

    let mut handles = Vec::new();
    for i in 0..10 {
        let mem = mem.clone();
        handles.push(tokio::spawn(async move {
            let key = format!("agent-{i}");
            let content = format!("result from agent {i}");
            mem.store(&key, &content, MemoryCategory::Core, None).await.unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let count = mem.count().await.unwrap();
    assert_eq!(count, 10, "all 10 concurrent stores must survive");
}

#[tokio::test]
async fn parallel_stores_then_recall_all() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mem = Arc::new(SqliteMemory::new(tmp.path()).unwrap());

    let mut handles = Vec::new();
    for i in 0..5 {
        let mem = mem.clone();
        handles.push(tokio::spawn(async move {
            mem.store(
                &format!("topic-{i}"),
                &format!("Rust concurrency pattern #{i}"),
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let results = mem.recall("Rust concurrency", 10, None).await.unwrap();
    assert!(!results.is_empty(), "recall should find concurrently-stored entries");
}

// ─────────────────────────────────────────────────────────────────────────────
// Session-scoped isolation: parallel agents in different sessions
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn session_scoped_parallel_stores_isolated() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mem = Arc::new(SqliteMemory::new(tmp.path()).unwrap());

    let mem1 = mem.clone();
    let mem2 = mem.clone();

    let h1 = tokio::spawn(async move {
        for i in 0..5 {
            mem1.store(
                &format!("session-a-{i}"),
                &format!("a-data-{i}"),
                MemoryCategory::Core,
                Some("session-a"),
            )
            .await
            .unwrap();
        }
    });

    let h2 = tokio::spawn(async move {
        for i in 0..5 {
            mem2.store(
                &format!("session-b-{i}"),
                &format!("b-data-{i}"),
                MemoryCategory::Core,
                Some("session-b"),
            )
            .await
            .unwrap();
        }
    });

    h1.await.unwrap();
    h2.await.unwrap();

    let total = mem.count().await.unwrap();
    assert_eq!(total, 10, "both sessions should have stored 5 entries each");

    // Session-scoped list should show only that session's entries
    let a_entries = mem.list(Some(&MemoryCategory::Core), Some("session-a")).await.unwrap();
    assert_eq!(a_entries.len(), 5, "session-a should have exactly 5 entries");

    let b_entries = mem.list(Some(&MemoryCategory::Core), Some("session-b")).await.unwrap();
    assert_eq!(b_entries.len(), 5, "session-b should have exactly 5 entries");
}

// ─────────────────────────────────────────────────────────────────────────────
// Concurrent upserts (same key from multiple "agents")
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn concurrent_upserts_on_same_key_converge() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mem = Arc::new(SqliteMemory::new(tmp.path()).unwrap());

    let mut handles = Vec::new();
    for i in 0..10 {
        let mem = mem.clone();
        handles.push(tokio::spawn(async move {
            mem.store("shared-key", &format!("value-{i}"), MemoryCategory::Core, None)
                .await
                .unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    // Should still be exactly 1 entry (upsert semantics)
    let count = mem.count().await.unwrap();
    assert_eq!(count, 1, "concurrent upserts on same key should deduplicate");

    // Content should be one of the values (last writer wins)
    let entry = mem.get("shared-key").await.unwrap().unwrap();
    assert!(
        entry.content.starts_with("value-"),
        "content should be one of the written values"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Concurrent reads while writing
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn reads_during_writes_do_not_panic() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mem = Arc::new(SqliteMemory::new(tmp.path()).unwrap());

    // Pre-populate
    for i in 0..5 {
        mem.store(
            &format!("pre-{i}"),
            &format!("pre-data-{i}"),
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
    }

    let mem_writer = mem.clone();
    let mem_reader = mem.clone();

    let writer = tokio::spawn(async move {
        for i in 0..10 {
            mem_writer
                .store(
                    &format!("new-{i}"),
                    &format!("new-data-{i}"),
                    MemoryCategory::Core,
                    None,
                )
                .await
                .unwrap();
        }
    });

    let reader = tokio::spawn(async move {
        for _ in 0..10 {
            let _ = mem_reader.recall("data", 5, None).await;
            let _ = mem_reader.count().await;
        }
    });

    writer.await.unwrap();
    reader.await.unwrap();

    let final_count = mem.count().await.unwrap();
    assert_eq!(final_count, 15, "5 pre + 10 new entries");
}

// ─────────────────────────────────────────────────────────────────────────────
// Store → forget → store cycle
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn store_forget_store_cycle_consistent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();

    mem.store("ephemeral", "first", MemoryCategory::Core, None)
        .await
        .unwrap();
    assert_eq!(mem.count().await.unwrap(), 1);

    let forgotten = mem.forget("ephemeral").await.unwrap();
    assert!(forgotten, "forget should return true for existing key");
    assert_eq!(mem.count().await.unwrap(), 0);

    mem.store("ephemeral", "second", MemoryCategory::Core, None)
        .await
        .unwrap();
    let entry = mem.get("ephemeral").await.unwrap().unwrap();
    assert_eq!(entry.content, "second");
}

// ─────────────────────────────────────────────────────────────────────────────
// Cross-category: store in different categories, recall respects category
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cross_category_isolation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();

    mem.store("fact-1", "Rust is fast", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("note-1", "meeting at 3pm", MemoryCategory::Daily, None)
        .await
        .unwrap();
    mem.store("chat-1", "user said hello", MemoryCategory::Conversation, None)
        .await
        .unwrap();

    let core = mem.list(Some(&MemoryCategory::Core), None).await.unwrap();
    assert_eq!(core.len(), 1);

    let daily = mem.list(Some(&MemoryCategory::Daily), None).await.unwrap();
    assert_eq!(daily.len(), 1);

    let convo = mem.list(Some(&MemoryCategory::Conversation), None).await.unwrap();
    assert_eq!(convo.len(), 1);

    // Total should be 3
    assert_eq!(mem.count().await.unwrap(), 3);
}

#![cfg(test)]
use super::*;

#[tokio::test]
async fn sqlite_owner_isolated_recall_excludes_another_owners_rows() {
    let (_tmp, mem) = temp_sqlite();
    seed_isolated_row(&mem, ISOLATED_OWNER_A, "isolated-a", "shared keyword first issuer note").await;
    seed_isolated_row(
        &mem,
        ISOLATED_OWNER_B,
        "isolated-b",
        "shared keyword second issuer note",
    )
    .await;

    let seen_by_b = mem
        .recall_with_context(
            "shared keyword",
            10,
            None,
            Some(&isolated_mcp_context(Some(ISOLATED_OWNER_B))),
        )
        .await
        .expect("test: isolated recall");
    let keys = seen_by_b.iter().map(|entry| entry.key.as_str()).collect::<Vec<_>>();
    assert!(keys.contains(&"isolated-b"), "{keys:?}");
    assert!(!keys.contains(&"isolated-a"), "{keys:?}");
}

#[tokio::test]
async fn sqlite_owner_isolated_get_excludes_another_owners_row_by_key() {
    let (_tmp, mem) = temp_sqlite();
    seed_isolated_row(&mem, ISOLATED_OWNER_A, "isolated-a", "shared keyword first issuer note").await;

    assert!(
        mem.get_with_context("isolated-a", Some(&isolated_mcp_context(Some(ISOLATED_OWNER_B))))
            .await
            .expect("test: isolated keyed read")
            .is_none(),
        "an exact key must not cross the owner boundary"
    );
    assert!(
        mem.get_with_context("isolated-a", Some(&isolated_mcp_context(Some(ISOLATED_OWNER_A))))
            .await
            .expect("test: own keyed read")
            .is_some(),
        "the owning caller must still read its own row"
    );
}

/// The backend override: an isolated context is enforced even when the
/// caller asks for the unrestricted compatibility result.
#[tokio::test]
async fn sqlite_owner_isolated_read_ignores_the_observe_mode_request() {
    let (_tmp, mem) = temp_sqlite();
    seed_isolated_row(&mem, ISOLATED_OWNER_A, "isolated-a", "shared keyword first issuer note").await;
    let context_b = isolated_mcp_context(Some(ISOLATED_OWNER_B));

    let observed = mem
        .recall_with_context_mode("shared keyword", 10, None, Some(&context_b), MemoryReadMode::Observe)
        .await
        .expect("test: observe-mode search under isolation");
    assert!(
        !observed.iter().any(|entry| entry.key == "isolated-a"),
        "Observe must not reopen the owner boundary: {:?}",
        observed.iter().map(|e| e.key.as_str()).collect::<Vec<_>>()
    );

    assert!(
        mem.get_with_context_mode("isolated-a", Some(&context_b), MemoryReadMode::Observe)
            .await
            .expect("test: observe-mode keyed read under isolation")
            .is_none(),
        "Observe must not reopen the owner boundary for a keyed read"
    );
}

/// The Observe-mode topic projection runs as the backend's own unrestricted
/// principal. An isolated caller must be handed its own principal instead,
/// or the projection becomes a second way around the boundary.
#[tokio::test]
async fn sqlite_owner_isolated_topic_projection_uses_the_caller_principal() {
    let (_tmp, mem) = temp_sqlite();
    seed_isolated_row(
        &mem,
        ISOLATED_OWNER_A,
        "isolated-a",
        "openpr migration phase checkpoint for the first issuer",
    )
    .await;
    {
        let conn = mem.pool.write();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO topics (id, title, project, status, created_at, updated_at)
             VALUES ('topic-isolated', 'openpr migration phase', 'openpr', 'open', ?1, ?1)",
            params![&now],
        )
        .expect("test: insert a topic");
        conn.execute(
            "UPDATE memories SET topic_id = 'topic-isolated' WHERE key = 'isolated-a'",
            [],
        )
        .expect("test: link the row to the topic");
    }

    let observed = mem
        .recall_with_context_mode(
            "openpr migration phase",
            10,
            None,
            Some(&isolated_mcp_context(Some(ISOLATED_OWNER_B))),
            MemoryReadMode::Observe,
        )
        .await
        .expect("test: topic-projected search under isolation");
    assert!(
        !observed.iter().any(|entry| entry.key == "isolated-a"),
        "the topic projection must not bypass the owner boundary: {:?}",
        observed.iter().map(|e| e.key.as_str()).collect::<Vec<_>>()
    );
}

/// Legacy rows carry a NULL `owner_id`. They are unattributable, so they
/// reach no isolated caller — and nothing is migrated to make that true.
#[tokio::test]
async fn sqlite_owner_isolated_read_excludes_null_owner_rows() {
    let (_tmp, mem) = temp_sqlite();
    seed_isolated_row(&mem, ISOLATED_OWNER_A, "isolated-a", "shared keyword first issuer note").await;
    mem.pool
        .write()
        .execute("UPDATE memories SET owner_id = NULL WHERE key = 'isolated-a'", [])
        .expect("test: strip the owner attribution");

    for owner in [ISOLATED_OWNER_A, ISOLATED_OWNER_B] {
        let seen = mem
            .recall_with_context("shared keyword", 10, None, Some(&isolated_mcp_context(Some(owner))))
            .await
            .expect("test: isolated recall");
        assert!(
            !seen.iter().any(|entry| entry.key == "isolated-a"),
            "an unattributed row must reach no isolated caller ({owner})"
        );
    }
}

/// Fail closed: an isolated caller with no owner id reads no owned row, and
/// never degrades to matching on the channel/sender triple two issuers
/// share.
#[tokio::test]
async fn sqlite_owner_isolated_read_without_an_owner_id_reads_nothing_owned() {
    let (_tmp, mem) = temp_sqlite();
    seed_isolated_row(&mem, ISOLATED_OWNER_A, "isolated-a", "shared keyword first issuer note").await;

    let seen = mem
        .recall_with_context("shared keyword", 10, None, Some(&isolated_mcp_context(None)))
        .await
        .expect("test: isolated recall with no owner id");
    assert!(seen.is_empty(), "{seen:?}");
}

/// The anti-over-correction lock: a deliberately `public` row stays
/// readable across the owner boundary.
#[tokio::test]
async fn sqlite_owner_isolated_read_still_admits_public_rows() {
    let (_tmp, mem) = temp_sqlite();
    seed_isolated_row(&mem, ISOLATED_OWNER_A, "isolated-public", "shared keyword announcement").await;
    mem.pool
        .write()
        .execute(
            "UPDATE memories SET visibility = 'public', sensitivity = 'normal' WHERE key = 'isolated-public'",
            [],
        )
        .expect("test: mark the row public");

    let seen_by_b = mem
        .recall_with_context(
            "shared keyword",
            10,
            None,
            Some(&isolated_mcp_context(Some(ISOLATED_OWNER_B))),
        )
        .await
        .expect("test: isolated recall");
    assert!(
        seen_by_b.iter().any(|entry| entry.key == "isolated-public"),
        "a public row must stay shared under isolation: {:?}",
        seen_by_b.iter().map(|e| e.key.as_str()).collect::<Vec<_>>()
    );
}

/// The byte-for-byte lock: a non-isolated context renders and behaves
/// exactly as before, over the very same fixture shape.
#[tokio::test]
async fn sqlite_non_isolated_recall_is_unchanged() {
    let (_tmp, mem) = temp_sqlite();
    seed_isolated_row(&mem, ISOLATED_OWNER_A, "isolated-a", "shared keyword first issuer note").await;

    let mut context_b = isolated_mcp_context(Some(ISOLATED_OWNER_B));
    context_b.require_owner_match = false;
    let seen = mem
        .recall_with_context("shared keyword", 10, None, Some(&context_b))
        .await
        .expect("test: non-isolated recall");
    assert!(
        seen.iter().any(|entry| entry.key == "isolated-a"),
        "without the isolation bit the historical channel/sender scope still applies: {:?}",
        seen.iter().map(|e| e.key.as_str()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn sqlite_owner_isolated_forget_excludes_another_owners_row_by_key() {
    let (_tmp, mem) = temp_sqlite();
    seed_isolated_row(&mem, ISOLATED_OWNER_A, "isolated-a", "first issuer note").await;

    let removed = mem
        .forget_with_context("isolated-a", Some(&isolated_mcp_context(Some(ISOLATED_OWNER_B))))
        .await
        .expect("test: isolated forget");
    assert!(!removed, "an isolated caller must not delete another owner's row");
    assert!(
        mem.get("isolated-a").await.expect("test: read back").is_some(),
        "the row must still be there"
    );

    // Positive control: the owner itself still deletes it, so the refusal
    // above is a boundary and not a broken statement.
    let removed_by_owner = mem
        .forget_with_context("isolated-a", Some(&isolated_mcp_context(Some(ISOLATED_OWNER_A))))
        .await
        .expect("test: isolated forget by the owner");
    assert!(removed_by_owner, "the owning caller must still delete its own row");
    assert!(mem.get("isolated-a").await.expect("test: read back").is_none());
}

/// The case the shared visibility predicate does **not** cover: a `public`
/// row belonging to another owner. Reading it is shared by design; deleting
/// it is not.
///
/// MUTATION GUARD: remove the top-level owner guard from
/// `forget_with_context` and this goes red while every other test here
/// stays green.
#[tokio::test]
async fn sqlite_owner_isolated_forget_excludes_another_owners_public_row() {
    let (_tmp, mem) = temp_sqlite();
    seed_isolated_row(&mem, ISOLATED_OWNER_A, "isolated-public", "first issuer public note").await;
    make_public(&mem, "isolated-public");

    let removed = mem
        .forget_with_context("isolated-public", Some(&isolated_mcp_context(Some(ISOLATED_OWNER_B))))
        .await
        .expect("test: isolated forget of a public row");
    assert!(
        !removed,
        "a public row is shared for reading, but it is still another owner's row to delete"
    );
    assert!(mem.get("isolated-public").await.expect("test: read back").is_some());
}

#[tokio::test]
async fn sqlite_owner_isolated_forget_without_an_owner_id_removes_nothing() {
    let (_tmp, mem) = temp_sqlite();
    seed_isolated_row(&mem, ISOLATED_OWNER_A, "isolated-a", "first issuer note").await;

    for blank in [None, Some(""), Some("   ")] {
        let removed = mem
            .forget_with_context("isolated-a", Some(&isolated_mcp_context(blank)))
            .await
            .expect("test: isolated forget with no owner id");
        assert!(!removed, "an isolated caller with owner {blank:?} must delete nothing");
    }
    assert!(mem.get("isolated-a").await.expect("test: read back").is_some());
}

/// An unattributed (`NULL`-owner) row is not deletable by an isolated
/// caller: `owner_id = ?N` is never true for `NULL`.
#[tokio::test]
async fn sqlite_owner_isolated_forget_excludes_a_row_with_no_owner() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("unattributed", "legacy note", MemoryCategory::Conversation, None)
        .await
        .expect("test: store a legacy row");

    let removed = mem
        .forget_with_context("unattributed", Some(&isolated_mcp_context(Some(ISOLATED_OWNER_A))))
        .await
        .expect("test: isolated forget of an unattributed row");
    assert!(!removed, "an unattributed row cannot be proved to be the caller's");
    assert!(mem.get("unattributed").await.expect("test: read back").is_some());
}

/// The anti-over-correction control: without the isolation bit the delete
/// behaves exactly as it always did, including for a row the caller does
/// not own but can see through the historical channel/sender scope.
#[tokio::test]
async fn sqlite_non_isolated_forget_is_unchanged() {
    let (_tmp, mem) = temp_sqlite();
    seed_isolated_row(&mem, ISOLATED_OWNER_A, "isolated-a", "first issuer note").await;

    let mut context_b = isolated_mcp_context(Some(ISOLATED_OWNER_B));
    context_b.require_owner_match = false;
    let removed = mem
        .forget_with_context("isolated-a", Some(&context_b))
        .await
        .expect("test: non-isolated forget");
    assert!(
        removed,
        "without the isolation bit the historical channel/sender scope still applies"
    );
    assert!(mem.get("isolated-a").await.expect("test: read back").is_none());
}

/// The guard itself: its decision, its placeholder and that a non-isolated
/// caller's statement is byte-identical because the fragment is empty.
#[test]
fn sqlite_forget_owner_guard_binds_only_for_an_isolated_caller_with_an_owner() {
    let mut unisolated = isolated_mcp_context(Some(ISOLATED_OWNER_A));
    unisolated.require_owner_match = false;
    let guard = SqliteMemory::forget_owner_guard(&unisolated, 7);
    assert_eq!(guard.sql, "", "a non-isolated delete must render no fragment at all");
    assert!(!guard.bound_owner);
    assert_eq!(SqliteMemory::forget_owner_anchor(&unisolated), None);

    for blank in [None, Some(""), Some("   ")] {
        let context = isolated_mcp_context(blank);
        let guard = SqliteMemory::forget_owner_guard(&context, 7);
        assert_eq!(guard.sql, " AND FALSE", "owner {blank:?} must fail closed");
        assert!(!guard.bound_owner, "a fail-closed guard binds nothing");
    }

    let isolated = isolated_mcp_context(Some(ISOLATED_OWNER_A));
    let guard = SqliteMemory::forget_owner_guard(&isolated, 7);
    assert_eq!(guard.sql, " AND owner_id = ?7");
    assert!(guard.bound_owner, "one marker, one parameter");
    assert_eq!(
        SqliteMemory::forget_owner_anchor(&isolated).as_deref(),
        Some(ISOLATED_OWNER_A)
    );
}

// ── Edge cases: schema idempotency ───────────────────────────

#[tokio::test]
async fn schema_idempotent_reopen() {
    let tmp = TempDir::new().unwrap();
    {
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        mem.store("k1", "v1", MemoryCategory::Core, None).await.unwrap();
    }
    // Open again — init_schema runs again on existing DB
    let mem2 = SqliteMemory::new(tmp.path()).unwrap();
    let entry = mem2.get("k1").await.unwrap();
    assert!(entry.is_some());
    assert_eq!(entry.unwrap().content, "v1");
    // Store more data — should work fine
    mem2.store("k2", "v2", MemoryCategory::Daily, None).await.unwrap();
    assert_eq!(mem2.count().await.unwrap(), 2);
}

#[tokio::test]
async fn schema_triple_open() {
    let tmp = TempDir::new().unwrap();
    let _m1 = SqliteMemory::new(tmp.path()).unwrap();
    let _m2 = SqliteMemory::new(tmp.path()).unwrap();
    let m3 = SqliteMemory::new(tmp.path()).unwrap();
    assert!(m3.health_check().await);
}

// ── Edge cases: forget + FTS5 consistency ────────────────────

#[tokio::test]
async fn forget_then_recall_no_ghost_results() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("ghost", "phantom memory content", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.forget("ghost").await.unwrap();
    let results = mem.recall("phantom memory", 10, None).await.unwrap();
    assert!(results.is_empty(), "Deleted memory should not appear in recall");
}

#[tokio::test]
async fn forget_and_re_store_same_key() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("cycle", "version 1", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.forget("cycle").await.unwrap();
    mem.store("cycle", "version 2", MemoryCategory::Core, None)
        .await
        .unwrap();
    let entry = mem.get("cycle").await.unwrap().unwrap();
    assert_eq!(entry.content, "version 2");
    assert_eq!(mem.count().await.unwrap(), 1);
}

// ── Edge cases: reindex ──────────────────────────────────────

#[tokio::test]
async fn reindex_empty_db() {
    let (_tmp, mem) = temp_sqlite();
    let count = mem.reindex().await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn reindex_twice_is_safe() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("r1", "reindex data", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.reindex().await.unwrap();
    let count = mem.reindex().await.unwrap();
    assert_eq!(count, 0); // Noop embedder → nothing to re-embed
    // Data should still be intact
    let results = mem.recall("reindex", 10, None).await.unwrap();
    assert_eq!(results.len(), 1);
}

// ── Edge cases: content_hash ─────────────────────────────────

#[test]
fn content_hash_empty_string() {
    let h = SqliteMemory::content_hash("");
    assert!(!h.is_empty(), "expected a non-empty value");
    assert_eq!(h.len(), 32); // 128-bit = 16 bytes = 32 hex chars
}

#[test]
fn content_hash_unicode() {
    let h1 = SqliteMemory::content_hash("🦀");
    let h2 = SqliteMemory::content_hash("🦀");
    assert_eq!(h1, h2);
    let h3 = SqliteMemory::content_hash("🚀");
    assert_ne!(h1, h3);
}

#[test]
fn content_hash_long_input() {
    let long = "a".repeat(1_000_000);
    let h = SqliteMemory::content_hash(&long);
    assert_eq!(h.len(), 32); // 128-bit = 16 bytes = 32 hex chars
}

// ── Edge cases: category helpers ─────────────────────────────

#[test]
fn category_roundtrip_custom_with_spaces() {
    let cat = MemoryCategory::Custom("my custom category".into());
    let s = SqliteMemory::category_to_str(&cat);
    assert_eq!(s, "my custom category");
    let back = SqliteMemory::str_to_category(&s);
    assert_eq!(back, cat);
}

#[test]
fn category_roundtrip_empty_custom() {
    let cat = MemoryCategory::Custom(String::new());
    let s = SqliteMemory::category_to_str(&cat);
    assert_eq!(s, "");
    let back = SqliteMemory::str_to_category(&s);
    assert_eq!(back, MemoryCategory::Custom(String::new()));
}

// ── Edge cases: list ─────────────────────────────────────────

#[tokio::test]
async fn list_custom_category() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("c1", "custom1", MemoryCategory::Custom("project".into()), None)
        .await
        .unwrap();
    mem.store("c2", "custom2", MemoryCategory::Custom("project".into()), None)
        .await
        .unwrap();
    mem.store("c3", "other", MemoryCategory::Core, None).await.unwrap();

    let project = mem
        .list(Some(&MemoryCategory::Custom("project".into())), None)
        .await
        .unwrap();
    assert_eq!(project.len(), 2);
}

#[tokio::test]
async fn list_empty_db() {
    let (_tmp, mem) = temp_sqlite();
    let all = mem.list(None, None).await.unwrap();
    assert!(all.is_empty());
}

// ── Session isolation ─────────────────────────────────────────

#[tokio::test]
async fn store_and_recall_with_session_id() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("k1", "session A fact", MemoryCategory::Core, Some("sess-a"))
        .await
        .unwrap();
    mem.store("k2", "session B fact", MemoryCategory::Core, Some("sess-b"))
        .await
        .unwrap();
    mem.store("k3", "no session fact", MemoryCategory::Core, None)
        .await
        .unwrap();

    // Recall with session-a filter returns only session-a entry
    let results = mem.recall("fact", 10, Some("sess-a")).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].key, "k1");
    assert_eq!(results[0].session_id.as_deref(), Some("sess-a"));
}

#[tokio::test]
async fn recall_no_session_filter_returns_all() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("k1", "alpha fact", MemoryCategory::Core, Some("sess-a"))
        .await
        .unwrap();
    mem.store("k2", "beta fact", MemoryCategory::Core, Some("sess-b"))
        .await
        .unwrap();
    mem.store("k3", "gamma fact", MemoryCategory::Core, None).await.unwrap();

    // Recall without session filter returns all matching entries
    let results = mem.recall("fact", 10, None).await.unwrap();
    assert_eq!(results.len(), 3);
}

#[tokio::test]
async fn cross_session_recall_isolation() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("secret", "session A secret data", MemoryCategory::Core, Some("sess-a"))
        .await
        .unwrap();

    // Session B cannot see session A data
    let results = mem.recall("secret", 10, Some("sess-b")).await.unwrap();
    assert!(results.is_empty());

    // Session A can see its own data
    let results = mem.recall("secret", 10, Some("sess-a")).await.unwrap();
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn list_with_session_filter() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("k1", "a1", MemoryCategory::Core, Some("sess-a"))
        .await
        .unwrap();
    mem.store("k2", "a2", MemoryCategory::Conversation, Some("sess-a"))
        .await
        .unwrap();
    mem.store("k3", "b1", MemoryCategory::Core, Some("sess-b"))
        .await
        .unwrap();
    mem.store("k4", "none1", MemoryCategory::Core, None).await.unwrap();

    // List with session-a filter
    let results = mem.list(None, Some("sess-a")).await.unwrap();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|e| e.session_id.as_deref() == Some("sess-a")));

    // List with session-a + category filter
    let results = mem.list(Some(&MemoryCategory::Core), Some("sess-a")).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].key, "k1");
}

#[tokio::test]
async fn schema_migration_idempotent_on_reopen() {
    let tmp = TempDir::new().unwrap();

    // First open: creates schema + migration
    {
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        mem.store("k1", "before reopen", MemoryCategory::Core, Some("sess-x"))
            .await
            .unwrap();
    }

    // Second open: migration runs again but is idempotent
    {
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        let results = mem.recall("reopen", 10, Some("sess-x")).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "k1");
        assert_eq!(results[0].session_id.as_deref(), Some("sess-x"));
    }
}

// ── §4.1 Concurrent write contention tests ──────────────

#[tokio::test]
async fn sqlite_concurrent_writes_no_data_loss() {
    let (_tmp, mem) = temp_sqlite();
    let mem = std::sync::Arc::new(mem);

    let mut handles = Vec::new();
    for i in 0..10 {
        let mem = std::sync::Arc::clone(&mem);
        handles.push(tokio::spawn(async move {
            mem.store(
                &format!("concurrent_key_{i}"),
                &format!("value_{i}"),
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let count = mem.count().await.unwrap();
    assert_eq!(count, 10, "all 10 concurrent writes must succeed without data loss");
}

#[tokio::test]
async fn sqlite_concurrent_read_write_no_panic() {
    let (_tmp, mem) = temp_sqlite();
    let mem = std::sync::Arc::new(mem);

    // Pre-populate
    mem.store("shared_key", "initial", MemoryCategory::Core, None)
        .await
        .unwrap();

    let mut handles = Vec::new();

    // Concurrent reads
    for _ in 0..5 {
        let mem = std::sync::Arc::clone(&mem);
        handles.push(tokio::spawn(async move {
            let _ = mem.get("shared_key").await.unwrap();
        }));
    }

    // Concurrent writes
    for i in 0..5 {
        let mem = std::sync::Arc::clone(&mem);
        handles.push(tokio::spawn(async move {
            mem.store(&format!("key_{i}"), &format!("val_{i}"), MemoryCategory::Core, None)
                .await
                .unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    // Should have 6 total entries (1 pre-existing + 5 new)
    assert_eq!(mem.count().await.unwrap(), 6);
}

// ── §4.2 Reindex / corruption recovery tests ────────────

#[tokio::test]
async fn sqlite_reindex_preserves_data() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "Rust is fast", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("b", "Python is interpreted", MemoryCategory::Core, None)
        .await
        .unwrap();

    mem.reindex().await.unwrap();

    let count = mem.count().await.unwrap();
    assert_eq!(count, 2, "reindex must preserve all entries");

    let entry = mem.get("a").await.unwrap();
    assert!(entry.is_some());
    assert_eq!(entry.unwrap().content, "Rust is fast");
}

#[tokio::test]
async fn sqlite_reindex_idempotent() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("x", "test data", MemoryCategory::Core, None).await.unwrap();

    // Multiple reindex calls should be safe
    mem.reindex().await.unwrap();
    mem.reindex().await.unwrap();
    mem.reindex().await.unwrap();

    assert_eq!(mem.count().await.unwrap(), 1);
}

#[tokio::test]
async fn append_conversation_turn_persists_session_and_turns() {
    let (_tmp, mem) = temp_sqlite();

    mem.append_conversation_turn(
        "signal_alice",
        "signal",
        "alice",
        "user",
        "first user message",
        Some("2026-03-05T00:00:00Z"),
        Some("msg-1"),
        None,
    )
    .await
    .unwrap();
    mem.append_conversation_turn(
        "signal_alice",
        "signal",
        "alice",
        "assistant",
        "first assistant message",
        Some("2026-03-05T00:00:01Z"),
        None,
        None,
    )
    .await
    .unwrap();

    let sessions = mem.list_conversation_sessions(50, 0, None).await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_key, "signal_alice");
    assert_eq!(sessions[0].message_count, 2);
    assert_eq!(sessions[0].created_at, "2026-03-05T00:00:00Z");
    assert_eq!(sessions[0].updated_at, "2026-03-05T00:00:01Z");
    assert_eq!(sessions[0].last_message_preview, "first assistant message");

    let principal = test_conversation_principal("signal_alice", Some("legacy:signal_alice"));
    let turns = mem
        .list_conversation_turns(&principal, "signal_alice", 50, 0)
        .await
        .unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].role, "user");
    assert_eq!(turns[0].timestamp, "2026-03-05T00:00:00Z");
    assert_eq!(turns[0].message_id.as_deref(), Some("msg-1"));
    assert_eq!(turns[1].role, "assistant");
    assert_eq!(turns[1].timestamp, "2026-03-05T00:00:01Z");
}

#[tokio::test]
async fn conversation_turns_phase1_owner_columns_exist() {
    let (_tmp, mem) = temp_sqlite();
    let conn = mem.pool.write();

    let session_cols: Vec<String> = conn
        .prepare("PRAGMA table_info(sessions)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    let turn_cols: Vec<String> = conn
        .prepare("PRAGMA table_info(conversation_turns)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .map(|row| row.unwrap())
        .collect();

    assert!(session_cols.contains(&"owner_id".to_string()));
    assert!(turn_cols.contains(&"owner_id".to_string()));
}

#[tokio::test]
async fn append_conversation_turn_stores_owner_id() {
    let (_tmp, mem) = temp_sqlite();

    mem.append_conversation_turn(
        "signal_owned",
        "signal",
        "alice",
        "user",
        "owned message",
        Some("2026-03-05T00:00:00Z"),
        Some("msg-owned"),
        Some("owner:alice"),
    )
    .await
    .unwrap();

    let conn = mem.pool.write();
    let session_owner: Option<String> = conn
        .query_row(
            "SELECT owner_id FROM sessions WHERE session_key = 'signal_owned'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let turn_owner: Option<String> = conn
        .query_row(
            "SELECT owner_id FROM conversation_turns WHERE session_key = 'signal_owned'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(session_owner.as_deref(), Some("owner:alice"));
    assert_eq!(turn_owner.as_deref(), Some("owner:alice"));
}

#[tokio::test]
async fn conversation_turns_owner_acl_filters_same_session_key() {
    let (_tmp, mem) = temp_sqlite();

    mem.append_conversation_turn(
        "shared-session",
        "signal",
        "alice",
        "user",
        "alice private turn",
        Some("2026-03-05T00:00:00Z"),
        Some("msg-a"),
        Some("owner:alice"),
    )
    .await
    .unwrap();
    mem.append_conversation_turn(
        "shared-session",
        "signal",
        "bob",
        "user",
        "bob private turn",
        Some("2026-03-05T00:00:01Z"),
        Some("msg-b"),
        Some("owner:bob"),
    )
    .await
    .unwrap();

    let alice = test_conversation_principal("shared-session", Some("owner:alice"));
    let bob = test_conversation_principal("shared-session", Some("owner:bob"));
    let mallory = test_conversation_principal("shared-session", Some("owner:mallory"));

    let alice_turns = mem
        .list_conversation_turns(&alice, "shared-session", 50, 0)
        .await
        .unwrap();
    let bob_turns = mem
        .list_conversation_turns(&bob, "shared-session", 50, 0)
        .await
        .unwrap();
    let mallory_turns = mem
        .list_conversation_turns(&mallory, "shared-session", 50, 0)
        .await
        .unwrap();

    assert_eq!(alice_turns.len(), 1);
    assert_eq!(alice_turns[0].content, "alice private turn");
    assert_eq!(bob_turns.len(), 1);
    assert_eq!(bob_turns[0].content, "bob private turn");
    assert!(mallory_turns.is_empty());
}

#[tokio::test]
async fn load_recent_conversation_histories_limits_per_session() {
    let (_tmp, mem) = temp_sqlite();
    for idx in 0..4 {
        mem.append_conversation_turn(
            "telegram_bob",
            "telegram",
            "bob",
            "user",
            &format!("turn-{idx}"),
            Some(&format!("2026-03-05T00:00:0{idx}Z")),
            None,
            None,
        )
        .await
        .unwrap();
    }

    let principal = test_conversation_principal("telegram_bob", Some("legacy:telegram_bob"));
    let histories = mem
        .load_recent_conversation_histories(&principal, 2, MAX_HYDRATED_SESSIONS)
        .await
        .unwrap();
    let bob_history = histories.get("telegram_bob").unwrap();
    assert_eq!(bob_history.len(), 2);
    assert_eq!(bob_history[0].content, "turn-2");
    assert_eq!(bob_history[1].content, "turn-3");
}

#[tokio::test]
async fn list_conversation_turns_returns_latest_window_chronologically() {
    let (_tmp, mem) = temp_sqlite();
    for idx in 0..4 {
        mem.append_conversation_turn(
            "signal_latest_window",
            "signal",
            "tester",
            "user",
            &format!("turn-{idx}"),
            Some(&format!("2026-03-05T00:00:0{idx}Z")),
            None,
            None,
        )
        .await
        .unwrap();
    }

    let principal = test_conversation_principal("signal_latest_window", Some("legacy:signal_latest_window"));
    let turns = mem
        .list_conversation_turns(&principal, "signal_latest_window", 2, 0)
        .await
        .unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].content, "turn-2");
    assert_eq!(turns[1].content, "turn-3");
}

#[tokio::test]
async fn load_recent_conversation_histories_limits_sessions_by_updated_at() {
    let (_tmp, mem) = temp_sqlite();
    mem.append_conversation_turn(
        "session_a",
        "signal",
        "tester",
        "user",
        "turn-a",
        Some("2026-03-05T00:00:00Z"),
        None,
        None,
    )
    .await
    .unwrap();
    mem.append_conversation_turn(
        "session_b",
        "signal",
        "tester",
        "user",
        "turn-b",
        Some("2026-03-05T00:00:01Z"),
        None,
        None,
    )
    .await
    .unwrap();
    mem.append_conversation_turn(
        "session_c",
        "signal",
        "tester",
        "user",
        "turn-c",
        Some("2026-03-05T00:00:02Z"),
        None,
        None,
    )
    .await
    .unwrap();

    let principal = test_conversation_principal("*", Some("system:*"));
    let histories = mem.load_recent_conversation_histories(&principal, 2, 2).await.unwrap();
    assert_eq!(histories.len(), 2);
    assert!(histories.contains_key("session_b"));
    assert!(histories.contains_key("session_c"));
    assert!(!histories.contains_key("session_a"));
}

#[tokio::test]
async fn store_rejects_reserved_self_namespace_without_self_system_session() {
    let (_tmp, mem) = temp_sqlite();

    let error = mem
        .store("self/guarded", "blocked", MemoryCategory::Core, None)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("reserved memory namespace"));
    assert!(mem.get("self/guarded").await.unwrap().is_none());

    mem.store(
        "self/guarded",
        "allowed",
        MemoryCategory::Core,
        Some(crate::self_system::SELF_SYSTEM_SESSION_ID),
    )
    .await
    .unwrap();

    let entry = mem.get("self/guarded").await.unwrap().unwrap();
    assert_eq!(entry.content, "allowed");
    assert_eq!(entry.session_id.as_deref(), Some("self_system"));
}

#[tokio::test]
async fn store_rejects_reserved_router_namespace_without_self_system_session() {
    let (_tmp, mem) = temp_sqlite();

    let error = mem
        .store("router/elo/test", "blocked", MemoryCategory::Core, None)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("reserved memory namespace"));
    assert!(mem.get("router/elo/test").await.unwrap().is_none());
}

#[tokio::test]
async fn g1_schema_migrations_recorded_on_init() {
    let (_tmp, mem) = temp_sqlite();
    let conn = mem.pool.write();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_schema_migrations", [], |row| row.get(0))
        .expect("count migrations");
    assert_eq!(count, SqliteMemory::memory_schema_migration_registry().len() as i64);
}

#[tokio::test]
async fn g1_schema_migration_checksum_mismatch_bails() {
    let (_tmp, mem) = temp_sqlite();
    {
        let conn = mem.pool.write();
        conn.execute(
            "UPDATE memory_schema_migrations SET checksum = 'tampered' WHERE version = 1",
            [],
        )
        .expect("tamper");
        let result = SqliteMemory::run_memory_schema_migrations(&conn);
        assert!(result.is_err(), "checksum mismatch must fail-fast");
        assert!(
            result.unwrap_err().to_string().contains("checksum mismatch"),
            "error should mention checksum mismatch"
        );
    }
}

#[tokio::test]
async fn g1_schema_migrations_idempotent_no_duplicate_rows() {
    let (_tmp, mem) = temp_sqlite();
    {
        let conn = mem.pool.write();
        // Re-running must be a no-op and never duplicate rows.
        SqliteMemory::run_memory_schema_migrations(&conn).expect("rerun ok");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_schema_migrations", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, SqliteMemory::memory_schema_migration_registry().len() as i64);
    }
}

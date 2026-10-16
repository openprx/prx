#![cfg(test)]
use super::*;

#[tokio::test]
async fn pool_runs_in_wal_mode_and_readers_reject_writes() {
    let (_tmp, mem) = temp_sqlite_with_read_pool(2);

    let writer_mode: String = mem
        .pool
        .write()
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("test: writer journal_mode");
    assert_eq!(
        writer_mode.to_lowercase(),
        "wal",
        "the reader pool is only sound under WAL journalling"
    );

    let reader = mem.pool.read().expect("test: reader checkout");
    let conn = reader.connection().expect("test: reader connection");
    let reader_mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("test: reader journal_mode");
    assert_eq!(reader_mode.to_lowercase(), "wal");

    let error = conn
        .execute("DELETE FROM memories", [])
        .expect_err("test: a reader must refuse to mutate the database");
    assert!(
        error.to_string().to_lowercase().contains("readonly"),
        "misrouted writes must fail loudly on a reader, got: {error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pooled_reads_are_actually_concurrent() {
    const READERS: usize = 6;
    let (_tmp, mem) = temp_sqlite_with_read_pool(READERS);
    mem.store("pool-read-key", "pool read content", MemoryCategory::Core, None)
        .await
        .expect("test: seed row");

    let (acquired_tx, acquired_rx) = mpsc::channel::<i64>();
    let mut releases = Vec::with_capacity(READERS);
    let mut handles = Vec::with_capacity(READERS);
    for _ in 0..READERS {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        releases.push(release_tx);
        let pool = Arc::clone(&mem.pool);
        let acquired = acquired_tx.clone();
        handles.push(thread::spawn(move || {
            let reader = pool.read().expect("test: reader checkout");
            let count: i64 = reader
                .connection()
                .expect("test: reader connection")
                .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
                .expect("test: count query");
            acquired.send(count).expect("test: report acquisition");
            // Keep holding the reader: if reads were serialized behind one
            // connection the main thread could never collect every signal.
            let _ = release_rx.recv();
        }));
    }
    drop(acquired_tx);

    for _ in 0..READERS {
        let count = acquired_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("test: all readers must be held at the same time");
        assert_eq!(count, 1);
    }
    let stats = mem.pool_stats();
    assert_eq!(stats.max_readers, READERS);
    assert_eq!(
        stats.readers_in_use, READERS,
        "every reader is checked out simultaneously"
    );
    assert_eq!(stats.saturated_reader_acquisitions, 0, "nobody had to queue");

    for release in releases {
        let _ = release.send(());
    }
    for handle in handles {
        handle.join().expect("test: reader thread");
    }
    let stats = mem.pool_stats();
    assert_eq!(stats.readers_in_use, 0);
    assert_eq!(stats.readers_idle, READERS, "readers are recycled, not discarded");
    assert_eq!(stats.reader_discards, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saturated_read_pool_queues_instead_of_failing() {
    let (_tmp, mem) = temp_sqlite_with_read_pool(1);
    let held = mem.pool.read().expect("test: hold the only reader");

    let pool = Arc::clone(&mem.pool);
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let waiter = thread::spawn(move || {
        let reader = pool.read().expect("test: a queued checkout must not fail");
        reader.connection().expect("test: queued reader connection");
        done_tx.send(()).expect("test: report completion");
    });

    assert!(
        done_rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "a saturated pool must make the caller wait"
    );
    assert_eq!(mem.pool_stats().reader_waiters, 1, "the waiting caller is observable");

    drop(held);
    done_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("test: the queued checkout completes once a reader is returned");
    waiter.join().expect("test: waiter thread");

    let stats = mem.pool_stats();
    assert_eq!(stats.max_readers, 1);
    assert!(stats.saturated_reader_acquisitions >= 1, "saturation is counted");
    assert!(
        stats.total_reader_wait >= Duration::from_millis(300),
        "the wait is measured, not bounded: {:?}",
        stats.total_reader_wait
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_and_reads_never_report_busy() {
    const WRITERS: usize = 16;
    let (_tmp, mem) = temp_sqlite_with_read_pool(8);
    let mem = Arc::new(mem);

    let mut writes = Vec::with_capacity(WRITERS);
    let mut reads = Vec::with_capacity(WRITERS);
    for index in 0..WRITERS {
        let writer = Arc::clone(&mem);
        writes.push(tokio::spawn(async move {
            writer
                .store(
                    &format!("busy-key-{index}"),
                    &format!("busy content {index}"),
                    MemoryCategory::Core,
                    None,
                )
                .await
        }));
        let reader = Arc::clone(&mem);
        reads.push(tokio::spawn(async move { reader.count().await }));
    }

    for task in writes {
        let result = task.await.expect("test: writer task");
        assert!(result.is_ok(), "concurrent write failed: {:?}", result.err());
    }
    for task in reads {
        let result = task.await.expect("test: reader task");
        assert!(result.is_ok(), "concurrent read failed: {:?}", result.err());
    }

    assert_eq!(mem.count().await.expect("test: final count"), WRITERS);
    let stats = mem.pool_stats();
    assert!(stats.write_acquisitions >= u64::try_from(WRITERS).unwrap_or(u64::MAX));
    assert_eq!(stats.reader_discards, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uncommitted_writes_stay_invisible_to_pooled_readers() {
    let (_tmp, mem) = temp_sqlite_with_read_pool(2);
    let count_iso = |mem: &SqliteMemory| -> i64 {
        let reader = mem.pool.read().expect("test: reader checkout");
        reader
            .connection()
            .expect("test: reader connection")
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE key = ?1",
                params!["isolation-key"],
                |row| row.get(0),
            )
            .expect("test: isolation count")
    };

    let mut writer = mem.pool.write();
    let tx = writer.transaction().expect("test: begin transaction");
    let now = Utc::now().to_rfc3339();
    tx.execute(
        "INSERT INTO memories (id, key, content, category, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params!["isolation-id", "isolation-key", "isolation content", "core", now],
    )
    .expect("test: insert inside transaction");

    assert_eq!(
        count_iso(&mem),
        0,
        "a pooled reader is a separate connection and must not see an uncommitted write"
    );

    tx.commit().expect("test: commit");
    drop(writer);

    assert_eq!(count_iso(&mem), 1, "a committed write is visible to pooled readers");
}

#[tokio::test]
async fn reader_left_in_a_transaction_is_reset_before_reuse() {
    let (_tmp, mem) = temp_sqlite_with_read_pool(1);
    {
        let reader = mem.pool.read().expect("test: reader checkout");
        reader
            .connection()
            .expect("test: reader connection")
            .execute_batch("BEGIN")
            .expect("test: open a read transaction");
    }
    let reader = mem.pool.read().expect("test: reuse the only reader");
    assert!(
        reader.connection().expect("test: reader connection").is_autocommit(),
        "a recycled reader must not carry an open transaction into the next caller"
    );
    assert_eq!(
        mem.pool_stats().reader_discards,
        0,
        "the reader was reset, not thrown away"
    );
}

/// The candidate cap is a real recall trade-off, not a formality: past it an
/// old memory is never scored no matter how relevant it is. The fixture puts
/// the only good match on the far side of a cap of 3 out of 10 embedded
/// rows, then raises the cap and gets it back.
#[tokio::test]
async fn the_vector_candidate_cap_bounds_recall_and_raising_it_restores_the_old_memory() {
    let tmp = TempDir::new().expect("test: tempdir");
    let mem = local_embedding_memory(&tmp);

    mem.store(
        "oldest_relevant",
        "release deployment pipeline failure",
        MemoryCategory::Core,
        None,
    )
    .await
    .expect("test: store the oldest memory");
    for index in 0..9 {
        mem.store(
            &format!("filler_{index}"),
            &format!("unrelated gardening note number {index}"),
            MemoryCategory::Core,
            None,
        )
        .await
        .expect("test: store filler");
    }

    let query = "release deployment pipeline failure";
    let capped = vector_candidates(&mem, query, 5, 3);
    assert!(
        !capped.iter().any(|id| memory_key_for_id(&mem, id) == "oldest_relevant"),
        "a cap of 3 over 10 embedded rows must not reach the oldest memory"
    );

    let widened = vector_candidates(&mem, query, 5, 100);
    assert_eq!(
        widened.first().map(|id| memory_key_for_id(&mem, id)).as_deref(),
        Some("oldest_relevant"),
        "raising [memory] vector_candidate_cap must bring the old memory back"
    );
}

/// `updated_at` is `Local::now().to_rfc3339()`, and SQLite orders it as text.
/// Across a DST transition the later instant sorts *earlier* as a string, so
/// ordering the candidate window by `updated_at` truncates the wrong rows.
/// This fixture writes exactly that pair of timestamps and asserts the
/// window still follows the update sequence: the rows written last are the
/// candidates whatever their timestamp text says.
#[tokio::test]
async fn the_candidate_window_follows_the_update_sequence_not_local_timestamp_text() {
    let tmp = TempDir::new().expect("test: tempdir");
    let mem = local_embedding_memory(&tmp);

    for index in 0..4 {
        mem.store(
            &format!("note_{index}"),
            "release deployment pipeline failure",
            MemoryCategory::Core,
            None,
        )
        .await
        .expect("test: store note");
    }

    {
        let conn = mem.pool.write();
        // 01:30-04:00 is 05:30Z; 01:15-05:00 is 06:15Z. The rows written
        // *later* (note_2, note_3) therefore carry the timestamp that sorts
        // *first* as text, which is exactly what a DST end does to this
        // column.
        for (key, updated_at) in [
            ("note_0", "2026-11-01T01:30:00-04:00"),
            ("note_1", "2026-11-01T01:31:00-04:00"),
            ("note_2", "2026-11-01T01:15:00-05:00"),
            ("note_3", "2026-11-01T01:16:00-05:00"),
        ] {
            conn.execute(
                "UPDATE memories SET updated_at = ?1 WHERE key = ?2",
                params![updated_at, key],
            )
            .expect("test: rewrite updated_at");
        }
    }

    let window: Vec<String> = vector_candidates(&mem, "release deployment pipeline failure", 2, 2)
        .iter()
        .map(|id| memory_key_for_id(&mem, id))
        .collect();
    let mut sorted = window;
    sorted.sort();
    assert_eq!(
        sorted,
        vec!["note_2".to_string(), "note_3".to_string()],
        "the two most recently written rows must be the candidates; ordering by updated_at text \
         would have picked note_0/note_1, whose local-offset strings sort later"
    );
}

/// Both optional filters bind their own placeholder. When `category` and
/// `session_id` were supplied together the third placeholder was reused, so
/// the candidate limit was bound where the session filter belonged.
#[tokio::test]
async fn category_and_session_filters_bind_distinct_placeholders() {
    let tmp = TempDir::new().expect("test: tempdir");
    let mem = local_embedding_memory(&tmp);

    mem.store(
        "wanted",
        "release deployment pipeline failure",
        MemoryCategory::Core,
        Some("session-a"),
    )
    .await
    .expect("test: store wanted");
    mem.store(
        "wrong_session",
        "release deployment pipeline failure",
        MemoryCategory::Core,
        Some("session-b"),
    )
    .await
    .expect("test: store wrong session");
    mem.store(
        "wrong_category",
        "release deployment pipeline failure",
        MemoryCategory::Daily,
        Some("session-a"),
    )
    .await
    .expect("test: store wrong category");

    let embedding = local_query_embedding("release deployment pipeline failure");
    let conn = mem.pool.write();
    let matched = SqliteMemory::vector_search(
        &conn,
        &embedding,
        "local",
        "prx-local-hash-v1",
        384,
        10,
        Some("core"),
        Some("session-a"),
        100,
    )
    .expect("test: filtered vector search");
    drop(conn);

    let keys: Vec<String> = matched.iter().map(|(id, _)| memory_key_for_id(&mem, id)).collect();
    assert_eq!(
        keys,
        vec!["wanted".to_string()],
        "both filters must apply; a collided placeholder drops or widens one of them"
    );
}

/// The candidate window must be an index scan, not a sort: the index leads
/// with the three embedding-identity columns every vector query filters on
/// and ends with `update_seq`, so a backward scan already yields the
/// newest-first order the window wants. Ordering on a column the index does
/// not carry would make every recall materialise and sort the whole
/// matching set, which is exactly what the cap exists to avoid — so the
/// plan is asserted for the unfiltered form and for the fully scoped form.
#[tokio::test]
async fn the_candidate_window_is_served_by_an_index_without_a_sort() {
    let tmp = TempDir::new().expect("test: tempdir");
    let mem = local_embedding_memory(&tmp);
    mem.store("seed", "release deployment", MemoryCategory::Core, Some("session-a"))
        .await
        .expect("test: store seed");

    let conn = mem.pool.write();
    // Every row of the plan, not just the first: the sort SQLite adds when
    // the index cannot supply the order is reported on its own line, so a
    // single-row read would call an index scan plus a temp B-tree "clean".
    let plan_of = |category, session_id| -> String {
        let query = vector_candidate_query("local", "prx-local-hash-v1", 384, 10, category, session_id, 10);
        let explain_sql = format!("EXPLAIN QUERY PLAN {}", query.sql);
        let mut stmt = conn
            .prepare(&explain_sql)
            .expect("test: prepare production candidate query plan");
        let params: Vec<&dyn rusqlite::types::ToSql> = query.params.iter().map(AsRef::as_ref).collect();
        let rows = stmt
            .query_map(params.as_slice(), |row| row.get::<_, String>(3))
            .expect("test: explain query plan");
        rows.collect::<Result<Vec<_>, _>>()
            .expect("test: collect query plan")
            .join(" | ")
    };
    let plans = [plan_of(None, None), plan_of(Some("core"), Some("session-a"))];
    for plan in plans {
        assert!(
            plan.contains("idx_memories_embedding_candidates_seq"),
            "the candidate window must use the embedding-identity index, got: {plan}"
        );
        assert!(
            !plan.to_ascii_uppercase().contains("TEMP B-TREE"),
            "the candidate window must not sort, got: {plan}"
        );
    }

    // The trigger runs on every memory write, so its MAX lookup must be an
    // index descent rather than a scan of the whole table.
    let seq_plan: String = {
        let mut stmt = conn
            .prepare("EXPLAIN QUERY PLAN SELECT COALESCE(MAX(update_seq), 0) + 1 FROM memories")
            .expect("test: prepare update sequence plan");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(3))
            .expect("test: explain update sequence plan");
        rows.collect::<Result<Vec<_>, _>>()
            .expect("test: collect update sequence plan")
            .join(" | ")
    };
    assert!(
        seq_plan.contains("idx_memories_update_seq"),
        "the update-sequence allocation must use its index, got: {seq_plan}"
    );
}

/// The window is bounded, so the question an upsert raises is not whether an
/// old key still exists but whether refreshing it puts the key back in the
/// window. Ordering by `rowid` answered "no": `ON CONFLICT(key) DO UPDATE`
/// rewrites content and embedding in place and never moves the row, so the
/// oldest key could be given the only embedding that matches the query one
/// second ago and still never be scored. The fixture refreshes through the
/// production `store()` path, with the cap genuinely biting first. `limit`
/// is held at the cap because the effective window is `max(limit, cap)`.
#[tokio::test]
async fn refreshing_the_oldest_key_returns_it_to_the_candidate_window() {
    const CAP: usize = 3;
    const FILLERS: usize = 9;

    let tmp = TempDir::new().expect("test: tempdir");
    let mem = local_embedding_memory(&tmp);
    let query = "release deployment pipeline failure";

    mem.store(
        "oldest",
        "unrelated gardening note about compost",
        MemoryCategory::Core,
        None,
    )
    .await
    .expect("test: store the oldest key");
    for index in 0..FILLERS {
        mem.store(
            &format!("filler_{index}"),
            &format!("unrelated gardening note number {index}"),
            MemoryCategory::Core,
            None,
        )
        .await
        .expect("test: store filler");
    }

    // Guard the fixture itself: a cap that does not exclude the oldest key
    // proves nothing about what refreshing it changes.
    let before: Vec<String> = vector_candidates(&mem, query, CAP, CAP)
        .iter()
        .map(|id| memory_key_for_id(&mem, id))
        .collect();
    assert!(
        !before.contains(&"oldest".to_string()),
        "fixture must start with the oldest key outside a cap of {CAP} over {} rows, got: {before:?}",
        FILLERS + 1
    );

    // A real upsert: same key, new content, new embedding, same rowid.
    mem.store("oldest", query, MemoryCategory::Core, None)
        .await
        .expect("test: refresh the oldest key");
    let rowid: i64 = {
        let conn = mem.pool.write();
        conn.query_row("SELECT rowid FROM memories WHERE key = 'oldest'", [], |row| row.get(0))
            .expect("test: read the refreshed rowid")
    };
    assert_eq!(rowid, 1, "the upsert must reuse the original row, not append a new one");

    let after: Vec<String> = vector_candidates(&mem, query, CAP, CAP)
        .iter()
        .map(|id| memory_key_for_id(&mem, id))
        .collect();
    assert_eq!(
        after.first().map(String::as_str),
        Some("oldest"),
        "a key refreshed with the only relevant embedding must be scored again, got: {after:?}"
    );
}

/// Update recency must be an integer sequence, not a timestamp: the four
/// rewrites below disagree on purpose. `note_0` carries the text that sorts
/// last, `note_2` the latest real instant, `note_3` the highest `rowid`, and
/// `note_1` — earliest by text and by instant — is simply the row written
/// last. Only a monotonic update sequence picks `note_1`.
#[tokio::test]
async fn the_candidate_window_ignores_utc_offsets_and_dst_in_updated_at() {
    let tmp = TempDir::new().expect("test: tempdir");
    let mem = local_embedding_memory(&tmp);
    let query = "release deployment pipeline failure";

    for index in 0..4 {
        mem.store(&format!("note_{index}"), query, MemoryCategory::Core, None)
            .await
            .expect("test: store note");
    }

    {
        let conn = mem.pool.write();
        for (key, updated_at) in [
            // Sorts last as text; +14:00 makes its instant an early one.
            ("note_0", "2026-12-31T23:59:59+14:00"),
            // The latest instant of the four (2026-12-31T10:00:00Z).
            ("note_2", "2026-12-30T23:00:00-11:00"),
            // The ambiguous local hour a DST end repeats.
            ("note_3", "2026-11-01T01:30:00-04:00"),
            // Earliest by text and by instant, but written last.
            ("note_1", "2026-01-01T00:00:00+00:00"),
        ] {
            conn.execute(
                "UPDATE memories SET updated_at = ?1 WHERE key = ?2",
                params![updated_at, key],
            )
            .expect("test: rewrite updated_at");
        }
    }

    let window: Vec<String> = vector_candidates(&mem, query, 1, 1)
        .iter()
        .map(|id| memory_key_for_id(&mem, id))
        .collect();
    assert_eq!(
        window,
        vec!["note_1".to_string()],
        "a window of one must hold the row written last; updated_at text would pick note_0, its \
         UTC instant note_2, and insertion order note_3"
    );
}

/// Recency decides which rows survive the cap, never which rows qualify:
/// the embedding identity, category and session filters are applied before
/// the window, so a newer row belonging to another embedding model,
/// dimension count, category or session must not consume a candidate slot.
#[tokio::test]
async fn the_candidate_window_keeps_its_scopes_when_newer_rows_are_out_of_scope() {
    let tmp = TempDir::new().expect("test: tempdir");
    let mem = local_embedding_memory(&tmp);
    let query = "release deployment pipeline failure";

    mem.store("wanted", query, MemoryCategory::Core, Some("session-a"))
        .await
        .expect("test: store wanted");
    mem.store("wrong_session", query, MemoryCategory::Core, Some("session-b"))
        .await
        .expect("test: store wrong session");
    mem.store("wrong_category", query, MemoryCategory::Daily, Some("session-a"))
        .await
        .expect("test: store wrong category");

    let embedding = local_query_embedding(query);
    {
        let conn = mem.pool.write();
        for (key, provider, model, dimensions) in [
            ("wrong_provider", "remote", "prx-local-hash-v1", 384_i64),
            ("wrong_model", "local", "some-other-model", 384_i64),
            ("wrong_dimensions", "local", "prx-local-hash-v1", 512_i64),
        ] {
            conn.execute(
                "INSERT INTO memories (
                    id, key, content, category, embedding, embedding_provider,
                    embedding_model, embedding_dimensions, created_at, updated_at, session_id
                 )
                 VALUES (?1, ?1, ?2, 'core', ?3, ?4, ?5, ?6, '2026-01-01T00:00:00+00:00', \
                         '2026-01-01T00:00:00+00:00', 'session-a')",
                params![
                    key,
                    query,
                    vector::vec_to_bytes(&embedding),
                    provider,
                    model,
                    dimensions
                ],
            )
            .expect("test: insert an out-of-scope row");
        }
    }

    // Every out-of-scope row was written after `wanted`, so an unscoped
    // window of two would hold none of the rows this query may score.
    let conn = mem.pool.write();
    let matched = SqliteMemory::vector_search(
        &conn,
        &embedding,
        "local",
        "prx-local-hash-v1",
        384,
        10,
        Some("core"),
        Some("session-a"),
        2,
    )
    .expect("test: scoped vector search");
    drop(conn);

    let keys: Vec<String> = matched.iter().map(|(id, _)| memory_key_for_id(&mem, id)).collect();
    assert_eq!(
        keys,
        vec!["wanted".to_string()],
        "provider, model, dimension, category and session scoping must survive the recency window"
    );
}

/// A store written before the update sequence existed must migrate on open:
/// the column is added, every existing row keeps its content and takes its
/// insertion order as its sequence, the superseded index is retired, and
/// the triggers that maintain the sequence from then on are installed.
#[tokio::test]
async fn opening_a_pre_update_sequence_store_migrates_it_without_data_loss() {
    const LEGACY_ROWS: usize = 4;

    let tmp = TempDir::new().expect("test: tempdir");
    {
        let mem = local_embedding_memory(&tmp);
        for index in 0..LEGACY_ROWS {
            mem.store(
                &format!("legacy_{index}"),
                &format!("legacy note number {index}"),
                MemoryCategory::Core,
                None,
            )
            .await
            .expect("test: store legacy row");
        }

        let conn = mem.pool.write();
        // Rebuild the 0.8.126 shape: no sequence column, no triggers, and
        // the candidate index that could only order by rowid.
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS memories_update_seq_ai;
             DROP TRIGGER IF EXISTS memories_update_seq_au;
             DROP INDEX IF EXISTS idx_memories_update_seq;
             DROP INDEX IF EXISTS idx_memories_embedding_candidates_seq;
             ALTER TABLE memories DROP COLUMN update_seq;
             CREATE INDEX IF NOT EXISTS idx_memories_embedding_candidates
                 ON memories(embedding_provider, embedding_model, embedding_dimensions)
                 WHERE embedding IS NOT NULL;",
        )
        .expect("test: rebuild the pre-migration schema");
        assert!(
            conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'update_seq'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("test: probe the fixture column")
                == 0,
            "the fixture must start without the update sequence column"
        );
    }

    let reopened = local_embedding_memory(&tmp);
    let conn = reopened.pool.write();

    let rows: Vec<(String, String, i64, i64)> = {
        let mut stmt = conn
            .prepare("SELECT key, content, rowid, update_seq FROM memories ORDER BY rowid")
            .expect("test: prepare migrated read");
        let mapped = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .expect("test: read migrated rows");
        mapped
            .collect::<Result<Vec<_>, _>>()
            .expect("test: collect migrated rows")
    };
    assert_eq!(rows.len(), LEGACY_ROWS, "migration must not lose or duplicate rows");
    for (index, (key, content, rowid, update_seq)) in rows.iter().enumerate() {
        assert_eq!(key, &format!("legacy_{index}"), "row {index} lost its key");
        assert_eq!(
            content,
            &format!("legacy note number {index}"),
            "row {index} lost its content"
        );
        assert_eq!(
            update_seq, rowid,
            "a backfilled row must keep its insertion order as its sequence"
        );
    }

    let object_names: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master \
                 WHERE type IN ('index', 'trigger') AND tbl_name = 'memories' ORDER BY name",
            )
            .expect("test: prepare schema read");
        let mapped = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("test: read schema objects");
        mapped
            .collect::<Result<Vec<_>, _>>()
            .expect("test: collect schema objects")
    };
    for expected in [
        "idx_memories_embedding_candidates_seq",
        "idx_memories_update_seq",
        "memories_au_fts",
        "memories_update_seq_ai",
        "memories_update_seq_au",
    ] {
        assert!(
            object_names.iter().any(|name| name == expected),
            "migration must create {expected}, got: {object_names:?}"
        );
    }
    assert!(
        !object_names
            .iter()
            .any(|name| name == "idx_memories_embedding_candidates"),
        "migration must retire the rowid-ordered index, got: {object_names:?}"
    );
    assert!(
        !object_names.iter().any(|name| name == "memories_au"),
        "migration must retire the unguarded FTS update trigger, got: {object_names:?}"
    );
    drop(conn);

    // The sequence continues above the backfill, and a refreshed legacy key
    // is recalled again on the migrated store.
    let query = "release deployment pipeline failure";
    reopened
        .store("legacy_0", query, MemoryCategory::Core, None)
        .await
        .expect("test: refresh a migrated key");
    let refreshed_seq: i64 = {
        let conn = reopened.pool.write();
        conn.query_row("SELECT update_seq FROM memories WHERE key = 'legacy_0'", [], |row| {
            row.get(0)
        })
        .expect("test: read the refreshed sequence")
    };
    assert!(
        refreshed_seq > i64::try_from(LEGACY_ROWS).unwrap_or(i64::MAX),
        "a write after migration must continue above every backfilled sequence, got {refreshed_seq}"
    );
    let window: Vec<String> = vector_candidates(&reopened, query, 1, 1)
        .iter()
        .map(|id| memory_key_for_id(&reopened, id))
        .collect();
    assert_eq!(
        window,
        vec!["legacy_0".to_string()],
        "the refreshed legacy key must lead the migrated candidate window"
    );
}

/// The update sequence is maintained by a trigger that writes back to
/// `memories`, and `memories_fts` is an FTS5 *external content* table whose
/// index is only as correct as the triggers that feed it. An unguarded FTS
/// update trigger fires for that write-back and issues a 'delete' for an
/// index entry that may not exist yet, which FTS5 reports as a corrupt
/// virtual table on the very next statement. This asserts the invariant
/// directly with FTS5's own integrity check, across an insert, a real
/// content upsert and a metadata-only update.
#[tokio::test]
async fn maintaining_the_update_sequence_keeps_the_fts_index_intact() {
    let tmp = TempDir::new().expect("test: tempdir");
    let mem = local_embedding_memory(&tmp);
    let integrity = |mem: &SqliteMemory, stage: &str| {
        let conn = mem.pool.write();
        conn.execute_batch("INSERT INTO memories_fts(memories_fts) VALUES('integrity-check');")
            .unwrap_or_else(|error| panic!("test: FTS index corrupt after {stage}: {error}"));
    };

    mem.store(
        "note",
        "release deployment pipeline failure",
        MemoryCategory::Core,
        None,
    )
    .await
    .expect("test: store note");
    integrity(&mem, "insert");

    mem.store("note", "compost heap turning schedule", MemoryCategory::Core, None)
        .await
        .expect("test: refresh note");
    integrity(&mem, "content upsert");

    {
        let conn = mem.pool.write();
        conn.execute(
            "UPDATE memories SET useful_count = useful_count + 1 WHERE key = 'note'",
            [],
        )
        .expect("test: metadata-only update");
    }
    integrity(&mem, "metadata update");

    // The guard must not cost the index its updates: the new content is
    // searchable and the replaced content is gone.
    let hits = mem.recall("compost", 5, None).await.expect("test: recall new content");
    assert!(
        hits.iter().any(|entry| entry.key == "note"),
        "the refreshed content must be searchable, got: {:?}",
        hits.iter().map(|entry| entry.key.as_str()).collect::<Vec<_>>()
    );
    let conn = mem.pool.write();
    let stale: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH 'deployment'",
            [],
            |row| row.get(0),
        )
        .expect("test: search the replaced content");
    assert_eq!(stale, 0, "the replaced content must no longer be indexed");
}

/// Opening an existing store must retire the 0.8.124 index, whose leading
/// column was the local-timestamp text this no longer orders by.
#[tokio::test]
async fn opening_an_existing_store_retires_the_timestamp_ordered_index() {
    let tmp = TempDir::new().expect("test: tempdir");
    {
        let mem = local_embedding_memory(&tmp);
        mem.store("seed", "release deployment", MemoryCategory::Core, None)
            .await
            .expect("test: store seed");
        let conn = mem.pool.write();
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_memories_embedding_recency \
             ON memories(updated_at DESC) WHERE embedding IS NOT NULL;",
        )
        .expect("test: recreate the legacy index");
        let legacy: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_memories_embedding_recency'",
                [],
                |row| row.get(0),
            )
            .expect("test: count legacy index");
        assert_eq!(legacy, 1, "test fixture must start with the legacy index present");
    }

    let reopened = local_embedding_memory(&tmp);
    let conn = reopened.pool.write();
    let legacy: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_memories_embedding_recency'",
            [],
            |row| row.get(0),
        )
        .expect("test: count legacy index after reopen");
    assert_eq!(legacy, 0, "the timestamp-ordered index must be dropped on open");
}

#[test]
fn the_configured_candidate_cap_is_authoritative() {
    assert_eq!(vector_candidate_cap(5, 2_000), 2_000);
    assert_eq!(vector_candidate_cap(5, 10), 10, "an operator may lower the cap");
    // Never below what the caller asked for: scoring fewer candidates than
    // results would return a short list for no reason.
    assert_eq!(vector_candidate_cap(50, 10), 50);
    assert_eq!(
        crate::config::MemoryConfig::default().vector_candidate_cap,
        VECTOR_CANDIDATE_CAP,
        "the shipped default must be the documented one"
    );
}

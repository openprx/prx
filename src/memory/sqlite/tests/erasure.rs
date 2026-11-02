//! Transactional erasure, retry, scope and plaintext/ciphertext controls.
use super::*;
use crate::memory::encrypted_scope::EncryptedScope;
use crate::memory::erasure::ErasureOrigin;
use crate::security::ramflux_aead::AccountCrypto;

const CANARY: &str = "erasurepositivecontrolcanaryuniquetokenoverthirtyletters";

fn bytes_contain(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|window| window == needle)
}

async fn populated(encryption: bool) -> (TempDir, Arc<SqliteMemory>, Arc<dyn Memory>, ErasureOrigin) {
    let (dir, memory) = temp_sqlite();
    let memory = Arc::new(memory);
    let crypto = Arc::new(AccountCrypto::new(
        dir.path().join("secrets"),
        &dir.path().join("ramflux"),
        encryption,
    ));
    crypto.unlock("account").unwrap();
    let origin = ErasureOrigin::new("account", "conversation", "m1");
    let scope = EncryptedScope::new(memory.clone(), crypto.clone(), "account", "account/conversation");
    let view = scope.with_erasure_origin(origin.clone()).unwrap();
    view.upsert_chat_profile_metadata("ramflux", "conversation", "dm", Some("session metadata title"))
        .await
        .unwrap();
    assert_eq!(
        view.get_chat_profile("ramflux", "conversation")
            .await
            .unwrap()
            .unwrap()
            .title
            .as_deref(),
        Some("session metadata title")
    );
    view.store("ramflux_sender_m1", CANARY, MemoryCategory::Conversation, None)
        .await
        .unwrap();
    view.append_conversation_turn(
        "session",
        "ramflux",
        "sender",
        "assistant",
        CANARY,
        Some("2026-11-01T01:00:00Z"),
        Some("assistant-1"),
        None,
    )
    .await
    .unwrap();
    let mut input = message_input(
        "ws",
        CANARY,
        MemoryVisibility::Workspace,
        None,
        Some("session"),
        Some("sender"),
    );
    input.idempotency_key = Some("channel:ramflux:m1".to_owned());
    input.raw_payload_json = Some(serde_json::json!({"text":CANARY}).to_string());
    view.append_message_event(input).await.unwrap();
    let mut event = memory_event_input(
        "ws",
        "memory.created",
        MemoryVisibility::Workspace,
        None,
        Some("session"),
    );
    event.payload_json = Some(serde_json::json!({"text":CANARY}).to_string());
    view.append_memory_event(event).await.unwrap();
    let mut draft = g1_draft_input(None, "worker", "draft1", "draft-key");
    draft.content = CANARY.to_owned();
    draft.payload_json = Some(CANARY.to_owned());
    view.create_memory_draft(draft).await.unwrap();
    let blob = crypto
        .seal("account", "conversation", "object", CANARY.as_bytes())
        .unwrap();
    memory
        .erasure_store()
        .unwrap()
        .put_blob(&origin, "media", "object", &blob)
        .unwrap();
    (dir, memory, view, origin)
}

/// Query every retention table, including FTS shadows and metadata tables.
fn assert_no_canary_in_any_table(memory: &SqliteMemory) {
    let reader = memory.pool.read().unwrap();
    let conn = reader.connection().unwrap();
    let mut names = conn
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
        .unwrap();
    let names = names
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    for table in names {
        let quoted = table.replace('"', "\"\"");
        let mut query = conn.prepare(&format!("SELECT * FROM \"{quoted}\"")).unwrap();
        let columns = query.column_count();
        let mut rows = query.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            for index in 0..columns {
                match row.get_ref(index).unwrap() {
                    rusqlite::types::ValueRef::Text(bytes) | rusqlite::types::ValueRef::Blob(bytes) => {
                        assert!(!bytes_contain(bytes, CANARY.as_bytes()), "retained canary in {table}")
                    }
                    _ => {}
                }
            }
        }
    }
}

#[tokio::test]
async fn indexed_erasure_covers_every_derived_write_with_plaintext_positive_control() {
    for encryption in [false, true] {
        let (dir, memory, view, origin) = populated(encryption).await;
        assert!(
            view.list(None, None)
                .await
                .unwrap()
                .iter()
                .any(|entry| entry.content.contains(CANARY))
        );
        memory.checkpoint_wal().await.unwrap();
        let before = std::fs::read(dir.path().join("memory/brain.db")).unwrap();
        assert_eq!(
            bytes_contain(&before, CANARY.as_bytes()),
            !encryption,
            "raw-byte positive control"
        );
        let backend = memory.erasure_store().unwrap();
        let indexed: i64 = memory
            .pool
            .read()
            .unwrap()
            .connection()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM ramflux_erase_index WHERE account=?1 AND conversation=?2 AND message=?3",
                params![origin.account, origin.conversation, origin.message],
                |row| row.get(0),
            )
            .unwrap();
        assert!(indexed >= 6, "derived writes were not indexed: {indexed}");
        let owned_rows = {
            let reader = memory.pool.read().unwrap();
            let mut stmt=reader.connection().unwrap().prepare("SELECT table_name,record_rowid FROM ramflux_erase_index WHERE account=?1 AND conversation=?2 AND message=?3").unwrap();
            stmt.query_map(params![origin.account, origin.conversation, origin.message], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        };
        for (table, rowid) in &owned_rows {
            let count: i64 = memory
                .pool
                .read()
                .unwrap()
                .connection()
                .unwrap()
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE rowid=?1"),
                    [rowid],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "positive control for {table}");
        }
        let ciphertext_before = if encryption {
            let ciphertext: String = memory
                .pool
                .read()
                .unwrap()
                .connection()
                .unwrap()
                .query_row(
                    "SELECT content FROM memories WHERE key=?1",
                    [view.persisted_key("ramflux_sender_m1")],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!ciphertext.contains(CANARY));
            assert!(
                bytes_contain(&before, ciphertext.as_bytes()),
                "ciphertext positive control must be present before erase"
            );
            Some(ciphertext)
        } else {
            None
        };
        backend.begin_delete(&origin).unwrap();
        // A write finishing after deletion began must still join this origin.
        view.store("late-autosave", CANARY, MemoryCategory::Conversation, None)
            .await
            .unwrap();
        backend.erase_rows(&origin).unwrap();
        assert!(backend.is_deleted(&origin).unwrap());
        assert!(
            view.store("after-fence", CANARY, MemoryCategory::Core, None)
                .await
                .is_err()
        );
        memory.checkpoint_wal().await.unwrap();
        backend.finish_delete(&origin).unwrap();
        memory.checkpoint_wal().await.unwrap();
        assert!(view.list(None, None).await.unwrap().is_empty());
        for (table, rowid) in &owned_rows {
            let count: i64 = memory
                .pool
                .read()
                .unwrap()
                .connection()
                .unwrap()
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE rowid=?1"),
                    [rowid],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "retained owned row in {table}");
        }
        assert_no_canary_in_any_table(&memory);
        let after = std::fs::read(dir.path().join("memory/brain.db")).unwrap();
        assert!(!bytes_contain(&after, CANARY.as_bytes()));
        if let Some(ciphertext) = ciphertext_before {
            assert!(
                !bytes_contain(&after, ciphertext.as_bytes()),
                "deleted ciphertext survived in freed pages"
            );
        }
        // Tombstones survive idempotency forget and reopening the database.
        memory.forget_message_idempotency("channel:ramflux:m1").await.unwrap();
        let reopened = SqliteMemory::new(dir.path()).unwrap();
        assert!(reopened.erasure_store().unwrap().is_deleted(&origin).unwrap());
    }
}

#[tokio::test]
async fn index_and_data_rollback_together_and_foreign_prefixes_survive() {
    let (_dir, memory) = temp_sqlite();
    let origin = ErasureOrigin::new("a", "conv", "m1");
    let view = memory.pool.with_origin(origin.clone());
    {
        let mut conn = view.write();
        let tx = conn.transaction().unwrap();
        tx.execute("INSERT INTO ramflux_blobs(account,conversation,kind,object_key,bytes) VALUES('a','b','message','rollback',X'01')",[]).unwrap();
        let count: i64 = tx
            .query_row("SELECT count(*) FROM ramflux_erase_index", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        tx.rollback().unwrap();
    }
    let count: i64 = memory
        .pool
        .read()
        .unwrap()
        .connection()
        .unwrap()
        .query_row("SELECT count(*) FROM ramflux_erase_index", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
    for (id, key, body) in [
        ("m1", "channel:ramflux:m1", "erase"),
        ("m10", "channel:ramflux:m10", "keep"),
    ] {
        let own = ErasureOrigin::new("a", "conv", id);
        let scoped = memory.with_erasure_origin(own).unwrap();
        let mut event = message_input("ws", body, MemoryVisibility::Workspace, None, None, None);
        event.idempotency_key = Some(key.to_owned());
        scoped.append_message_event(event).await.unwrap();
    }
    let mut unrelated = message_input(
        "ws",
        "literal channel:ramflux:m1 in unrelated text",
        MemoryVisibility::Workspace,
        None,
        None,
        None,
    );
    unrelated.idempotency_key = Some("foreign".to_owned());
    memory.append_message_event(unrelated).await.unwrap();
    memory.forget_message_idempotency("channel:ramflux:m1").await.unwrap();
    assert!(
        memory
            .find_message_event_by_idempotency_key("ws", "channel:ramflux:m10")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        memory
            .find_message_event_by_idempotency_key("ws", "foreign")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn checkpoint_failure_keeps_the_index_for_retry() {
    let (_dir, memory, _view, origin) = populated(false).await;
    let backend = memory.erasure_store().unwrap();
    let reader = memory.pool.read().unwrap();
    reader.connection().unwrap().execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .connection()
        .unwrap()
        .query_row("SELECT count(*) FROM memories", [], |row| row.get(0))
        .unwrap();
    backend.begin_delete(&origin).unwrap();
    backend.erase_rows(&origin).unwrap();
    assert!(memory.checkpoint_wal().await.is_err());
    assert!(
        backend
            .origins(&origin.account, Some(&origin.conversation))
            .unwrap()
            .contains(&origin)
    );
    reader.connection().unwrap().execute_batch("ROLLBACK").unwrap();
    drop(reader);
    backend.erase_rows(&origin).unwrap();
    memory.checkpoint_wal().await.unwrap();
    backend.finish_delete(&origin).unwrap();
    assert!(
        !backend
            .origins(&origin.account, Some(&origin.conversation))
            .unwrap()
            .contains(&origin)
    );
}

#[tokio::test]
async fn later_recovery_evidence_keeps_the_original_inputs_erase_origin() {
    let (_dir, memory) = temp_sqlite();
    let old = ErasureOrigin::new("account", "conv", "old");
    let new = ErasureOrigin::new("account", "conv", "new");
    let first = memory.with_erasure_origin(old.clone()).unwrap();
    let later = memory.with_erasure_origin(new).unwrap();
    let input = first
        .append_message_event(message_input(
            "ws",
            CANARY,
            MemoryVisibility::Workspace,
            None,
            None,
            None,
        ))
        .await
        .unwrap();
    let mut evidence = message_input("ws", CANARY, MemoryVisibility::Workspace, None, None, None);
    evidence.causation_event_id = Some(input.event_id.clone());
    evidence.event_type = "turn.ingress.interrupted".to_owned();
    evidence.role = "event".to_owned();
    let evidence = later.append_message_event(evidence).await.unwrap();
    later
        .store("keep", "different input must survive", MemoryCategory::Core, None)
        .await
        .unwrap();
    let backend = memory.erasure_store().unwrap();
    backend.begin_delete(&old).unwrap();
    backend.erase_rows(&old).unwrap();
    let conn = memory.pool.read().unwrap();
    for id in [&input.event_id, &evidence.event_id] {
        let count: i64 = conn
            .connection()
            .unwrap()
            .query_row("SELECT count(*) FROM message_events WHERE event_id=?1", [id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
        let count: i64 = conn
            .connection()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM memory_events WHERE subject_table='message_events' AND subject_id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
    assert!(memory.get("keep").await.unwrap().is_some());
}

#[tokio::test]
async fn encrypted_decision_retrieval_returns_own_plaintext_and_no_foreign_rows() {
    let (_dir, memory, view, _origin) = populated(true).await;
    memory
        .store(
            "foreign",
            "erasurepositivecontrolcanaryuniquetokenoverthirtyletters foreign body",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
    let hits = view
        .retrieve_decision_memory(
            CANARY,
            5,
            &MemoryWriteContext::default(),
            None,
            std::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_ne!(hits, Vec::new());
    assert!(
        hits.iter()
            .any(|hit| hit.key == "ramflux_sender_m1" && hit.content == CANARY)
    );
    assert!(
        hits.iter()
            .all(|hit| hit.key != "foreign" && !hit.content.contains("foreign body"))
    );
    assert!(
        memory.get("foreign").await.unwrap().is_some(),
        "foreign positive control must exist"
    );
}

#[tokio::test]
async fn an_unscoped_insert_reusing_a_forgotten_rowid_never_inherits_erase_ownership() {
    let (_dir, memory) = temp_sqlite();
    let origin = ErasureOrigin::new("account", "conv", "message");
    let scoped = memory.with_erasure_origin(origin.clone()).unwrap();
    scoped
        .store("scoped", "old message body", MemoryCategory::Core, None)
        .await
        .unwrap();
    let old_row: i64 = memory
        .pool
        .read()
        .unwrap()
        .connection()
        .unwrap()
        .query_row("SELECT rowid FROM memories WHERE key='scoped'", [], |row| row.get(0))
        .unwrap();
    scoped.forget("scoped").await.unwrap();
    memory
        .store("foreign", "unrelated later workspace body", MemoryCategory::Core, None)
        .await
        .unwrap();
    let new_row: i64 = memory
        .pool
        .read()
        .unwrap()
        .connection()
        .unwrap()
        .query_row("SELECT rowid FROM memories WHERE key='foreign'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(old_row, new_row, "test must exercise actual rowid reuse");
    let backend = memory.erasure_store().unwrap();
    backend.begin_delete(&origin).unwrap();
    backend.erase_rows(&origin).unwrap();
    memory.checkpoint_wal().await.unwrap();
    backend.finish_delete(&origin).unwrap();
    assert_eq!(
        memory.get("foreign").await.unwrap().unwrap().content,
        "unrelated later workspace body"
    );
}

#[tokio::test]
async fn a_causal_parent_in_another_conversation_never_transfers_erase_ownership() {
    let (_dir, memory) = temp_sqlite();
    let first_origin = ErasureOrigin::new("account", "first", "same-id");
    let second_origin = ErasureOrigin::new("account", "second", "same-id");
    let first = memory.with_erasure_origin(first_origin.clone()).unwrap();
    let second = memory.with_erasure_origin(second_origin.clone()).unwrap();
    let mut input = message_input(
        "ws",
        "first conversation input",
        MemoryVisibility::Workspace,
        None,
        None,
        None,
    );
    input.run_id = Some("first-run".to_owned());
    let event = first.append_message_event(input).await.unwrap();
    let mut next = message_input(
        "ws",
        "independent second conversation input",
        MemoryVisibility::Workspace,
        None,
        None,
        None,
    );
    next.run_id = Some("second-run".to_owned());
    next.causation_event_id = Some(event.event_id);
    let retained = second.append_message_event(next).await.unwrap();
    let backend = memory.erasure_store().unwrap();
    assert_eq!(backend.run_ids(&first_origin).unwrap(), vec!["first-run".to_owned()]);
    backend.begin_delete(&first_origin).unwrap();
    backend.erase_rows(&first_origin).unwrap();
    memory.checkpoint_wal().await.unwrap();
    backend.finish_delete(&first_origin).unwrap();
    // The raw row and its independent erase origin survive the first delete.
    let exists: bool = memory
        .pool
        .read()
        .unwrap()
        .connection()
        .unwrap()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM message_events WHERE event_id=?1)",
            [&retained.event_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(exists);
    assert_eq!(backend.run_ids(&second_origin).unwrap(), vec!["second-run".to_owned()]);
}

#[tokio::test]
async fn later_trash_and_draft_merge_copies_keep_the_original_erase_origin() {
    let (_dir, memory) = temp_sqlite();
    let original = ErasureOrigin::new("account", "conv", "original");
    let first = memory.with_erasure_origin(original.clone()).unwrap();
    let later = memory
        .with_erasure_origin(ErasureOrigin::new("account", "conv", "later"))
        .unwrap();
    first
        .store("original-note", CANARY, MemoryCategory::Core, None)
        .await
        .unwrap();
    assert!(
        later
            .move_to_trash("original-note", "retention copy", 7)
            .await
            .unwrap()
            .is_some()
    );
    assert!(later.forget("original-note").await.unwrap());
    let mut draft = g1_draft_input(Some("owner"), "worker", "copied-draft", "draft-note");
    draft.content = CANARY.to_owned();
    first.create_memory_draft(draft).await.unwrap();
    assert!(
        later
            .merge_memory_draft(&g1_owner_principal("owner"), "copied-draft")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(later.get("draft-note").await.unwrap().unwrap().content, CANARY);
    let backend = memory.erasure_store().unwrap();
    backend.begin_delete(&original).unwrap();
    backend.erase_rows(&original).unwrap();
    memory.checkpoint_wal().await.unwrap();
    backend.finish_delete(&original).unwrap();
    assert_no_canary_in_any_table(&memory);
}

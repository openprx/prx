#![cfg(test)]

use super::*;
use crate::memory::Memory;
use std::sync::{Arc, Barrier};
use tempfile::TempDir;

fn store() -> anyhow::Result<(TempDir, SqliteMemory)> {
    // Recovery authority rejects group-writable ancestors; a build harness's
    // shared TMPDIR is not necessarily suitable for these private fixtures.
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("fixture HOME is unavailable"))?;
    let directory = tempfile::Builder::new()
        .prefix(".prx-recovery-test-")
        .tempdir_in(home)?;
    let path = directory.path().canonicalize()?.join("memory/brain.db");
    let memory = SqliteMemory::new_with_path(path)?;
    Ok((directory, memory))
}
fn chain() -> RecoveryChain {
    RecoveryChain {
        workspace_id: "prx_workspace".into(),
        owner_id: "prx_owner".into(),
        source: "cli".into(),
        run_id: "prx_run".into(),
    }
}
fn seed(memory: &SqliteMemory, id: &str, role: &str) -> anyhow::Result<RecoveryEvidence> {
    memory.pool.write().execute("INSERT INTO message_events(event_id,workspace_id,owner_id,source,run_id,role,event_type,content,created_at,updated_at) VALUES(?1,'prx_workspace','prx_owner','cli','prx_run',?2,'message.created','original bytes','0','0')", params![id,role])?;
    memory.recovery_event_evidence(&chain(), id)
}
fn pending(memory: &SqliteMemory) -> anyhow::Result<RecoveryPublication> {
    Ok(RecoveryPublication {
        chain: chain(),
        input: Some(seed(memory, "input", "user")?),
        terminal: None,
        facts: b"verified capsule, generation, lifetime, transport".to_vec(),
    })
}
fn key_path(memory: &SqliteMemory, id: &str) -> anyhow::Result<std::path::PathBuf> {
    Ok(memory
        .db_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("fixture repository has no memory directory"))?
        .join(".recovery_keys")
        .join(format!("{id}.key")))
}

#[test]
#[serial_test::serial(recovery_authority)]
fn recovery_mac_bytes_are_unchanged() -> anyhow::Result<()> {
    let (_dir, memory) = store()?;
    let bytes = [7u8; 32];
    let id = KeyDirectory::key_id(&bytes);
    let parent = memory
        .db_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("fixture repository has no memory directory"))?;
    KeyDirectory::open(parent, true)?.install(&id, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(key_path(&memory, &id)?)?.permissions().mode() & 0o777,
            0o600
        );
    }
    let capsule = memory.recovery_mac(Some(&id), RecoveryMacDomain::CapsuleV3, b"fixed")?;
    assert_eq!(
        hex::encode(capsule.tag),
        "0ca4d25441b7413aadada92b23e64df08779c56954cf93872abcdae19a14bebe"
    );
    assert_eq!(capsule.key_id, id);
    let digest = memory.recovery_mac(Some(&id), RecoveryMacDomain::InputDigestV3, b"fixed")?;
    assert_eq!(
        hex::encode(digest.tag),
        "decaf66f760e0cf400435fa4a1e3235c281dd616b496baccf32400dd7e9b05f6"
    );
    assert_eq!(digest.key_id, id);
    Ok(())
}

#[test]
#[serial_test::serial(recovery_authority)]
fn recovery_key_create_race_is_durable() {
    let (_dir, memory) = store().unwrap();
    assert!(memory.recovery_repository().is_some());
    assert!(!memory.db_path.parent().unwrap().join(".recovery_keys").exists());
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::with_capacity(2);
    for _ in 0..2 {
        let memory = SqliteMemory::new_with_path(memory.db_path.clone()).unwrap();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            memory.activate_recovery_authority().unwrap()
        }));
    }
    barrier.wait();
    let ids = handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>();
    assert_eq!(ids.first(), ids.get(1));
    let id = ids.first().unwrap();
    let before = std::fs::read(key_path(&memory, id).unwrap()).unwrap();
    assert_eq!(before.len(), 32);
    let keys = KeyDirectory::open(memory.db_path.parent().unwrap(), false).unwrap();
    // A second exclusive create must refuse without touching existing bytes.
    let bytes: [u8; 32] = before.clone().try_into().unwrap();
    assert!(keys.install(id, &bytes).is_err());
    assert_eq!(before, std::fs::read(key_path(&memory, id).unwrap()).unwrap());
    let reopened = SqliteMemory::new_with_path(memory.db_path.clone()).unwrap();
    assert_eq!(*id, reopened.activate_recovery_authority().unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(key_path(&memory, id).unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(key_path(&memory, id).unwrap().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[test]
#[serial_test::serial(recovery_authority)]
fn recovery_key_rejects_unsafe_paths() {
    let (_dir, memory) = store().unwrap();
    let id = memory.activate_recovery_authority().unwrap();
    let path = key_path(&memory, &id).unwrap();
    std::fs::write(&path, b"malformed").unwrap();
    assert!(memory.activate_recovery_authority().is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        std::fs::remove_file(&path).unwrap();
        let target = path.with_extension("target");
        std::fs::write(&target, [1u8; 32]).unwrap();
        symlink(&target, &path).unwrap();
        assert!(memory.activate_recovery_authority().is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, [1u8; 32]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(memory.activate_recovery_authority().is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
        let keys = path.parent().unwrap();
        let moved = keys.with_file_name("retained_keys");
        std::fs::rename(keys, &moved).unwrap();
        symlink(&moved, keys).unwrap();
        assert!(memory.activate_recovery_authority().is_err());
        let alias = keys.with_file_name("memory_alias");
        symlink(memory.db_path.parent().unwrap(), &alias).unwrap();
        assert!(KeyDirectory::open(&alias, false).is_err());
        let (other_directory, other) = store().unwrap();
        let parent = other.db_path.parent().unwrap();
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(other.activate_recovery_authority().is_err());
        assert!(!parent.join(".recovery_keys").exists());
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(other_directory.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(other.activate_recovery_authority().is_err());
        assert!(!parent.join(".recovery_keys").exists());
    }
}

#[test]
#[serial_test::serial(recovery_authority)]
fn recovery_key_loss_never_replaces_authority() {
    let (_dir, memory) = store().unwrap();
    let id = memory.activate_recovery_authority().unwrap();
    let publication = pending(&memory).unwrap();
    memory.record_recovery_pending(&publication).unwrap();
    let path = key_path(&memory, &id).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(memory.activate_recovery_authority().is_err());
    assert!(memory.rotate_recovery_authority().is_err());
    assert!(memory.read_recovery_receipt(&chain()).is_err());
    assert!(!path.exists());
    std::fs::write(&path, [0u8; 31]).unwrap();
    assert!(memory.activate_recovery_authority().is_err());
    assert_eq!(std::fs::read(&path).unwrap().len(), 31);
    std::fs::write(&path, [0u8; 32]).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert!(memory.activate_recovery_authority().is_err());
    memory
        .pool
        .write()
        .execute("DELETE FROM recovery_authority", [])
        .unwrap();
    assert!(memory.activate_recovery_authority().is_err());
}

#[test]
#[serial_test::serial(recovery_authority)]
fn recovery_rotation_preserves_old_receipts() {
    let (_dir, memory) = store().unwrap();
    let old = memory.activate_recovery_authority().unwrap();
    let mut publication = pending(&memory).unwrap();
    let pending_receipt = memory.record_recovery_pending(&publication).unwrap();
    let new = memory.rotate_recovery_authority().unwrap();
    assert_ne!(old, new);
    assert_eq!(
        memory.read_recovery_receipt(&chain()).unwrap().unwrap(),
        pending_receipt
    );
    publication.terminal = Some(seed(&memory, "terminal", "assistant").unwrap());
    publication.facts = b"verified final status and usage".to_vec();
    let settled = memory.settle_recovery(&publication).unwrap();
    assert_eq!(settled.key_id, new);
    let reopened = SqliteMemory::new_with_path(memory.db_path.clone()).unwrap();
    assert_eq!(reopened.read_recovery_receipt(&chain()).unwrap().unwrap(), settled);
    memory
        .pool
        .write()
        .execute("UPDATE message_events SET content='changed' WHERE event_id='input'", [])
        .unwrap();
    assert!(memory.read_recovery_receipt(&chain()).is_err());
    assert!(key_path(&memory, &old).unwrap().exists());
    assert!(key_path(&memory, &new).unwrap().exists());
}

#[test]
#[serial_test::serial(recovery_authority)]
fn recovery_schema_upgrade_is_atomic() {
    let (_dir, memory) = store().unwrap();
    {
        let writer = memory.pool.write();
        let tx = writer.unchecked_transaction().unwrap();
        schema(&tx).unwrap();
        tx.rollback().unwrap();
        let exists: bool = writer
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='recovery_authority')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!exists);
    }
    memory
        .pool
        .write()
        .execute_batch("CREATE VIEW recovery_attestations AS SELECT 1")
        .unwrap();
    assert!(memory.activate_recovery_authority().is_err());
    let exists: bool = memory
        .pool
        .write()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='recovery_authority')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!exists, "failed activation must roll back all new schema");
    memory
        .pool
        .write()
        .execute_batch("DROP VIEW recovery_attestations")
        .unwrap();
    memory.activate_recovery_authority().unwrap();
    let publication = pending(&memory).unwrap();
    let receipt = memory.record_recovery_pending(&publication).unwrap();
    assert_eq!(receipt, memory.record_recovery_pending(&publication).unwrap());
    let mut changed = publication.clone();
    changed.facts.push(1);
    assert!(memory.record_recovery_pending(&changed).is_err());
    let mut foreign = publication;
    foreign.chain.owner_id = "foreign".into();
    assert!(memory.record_recovery_pending(&foreign).is_err());
    memory
        .pool
        .write()
        .execute("DELETE FROM message_events WHERE event_id='input'", [])
        .unwrap();
    assert!(memory.read_recovery_receipt(&chain()).unwrap().is_none());
    let writer = memory.pool.write();
    let count: i64 = writer
        .query_row("SELECT count(*) FROM recovery_attestations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    writer
        .execute("UPDATE recovery_authority SET version=2", [])
        .unwrap_err();
}

#[test]
#[serial_test::serial(recovery_authority)]
fn recovery_settlement_race_has_one_receipt() {
    let (_dir, memory) = store().unwrap();
    memory.activate_recovery_authority().unwrap();
    let mut publication = pending(&memory).unwrap();
    let initial = memory.record_recovery_pending(&publication).unwrap();
    let mut relabeled = initial.clone();
    relabeled.state = "settled".into();
    let keys = KeyDirectory::open(memory.db_path.parent().unwrap(), false).unwrap();
    let error = verify(&memory.pool.write(), &keys, &publication.chain, &relabeled)
        .err()
        .unwrap();
    assert!(error.to_string().contains("invalid recovery authentication"));
    publication.terminal = Some(seed(&memory, "terminal", "assistant").unwrap());
    publication.facts = b"verified final/claim/usage/lineage; projection pending".to_vec();
    let barrier = Arc::new(Barrier::new(3));
    // Distinct repositories/pools exercise SQLite's cross-connection writer arbitration.
    let mut handles = Vec::with_capacity(2);
    for _ in 0..2 {
        let memory = SqliteMemory::new_with_path(memory.db_path.clone()).unwrap();
        let publication = publication.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            memory.settle_recovery(&publication).unwrap()
        }));
    }
    barrier.wait();
    let receipts = handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>();
    let winner = receipts.first().unwrap();
    assert_eq!(Some(winner), receipts.get(1));
    assert_ne!(initial.mac, winner.mac);
    assert_eq!(*winner, memory.settle_recovery(&publication).unwrap());
    let mut conflicting = publication;
    conflicting.facts.push(1);
    assert!(memory.settle_recovery(&conflicting).is_err());
    let writer = memory.pool.write();
    assert_eq!(
        writer
            .query_row(
                "SELECT count(*) FROM recovery_attestations WHERE state='settled'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert!(
        writer
            .execute("UPDATE recovery_attestations SET facts=x'00'", [])
            .is_err()
    );
    assert_eq!(
        writer
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name LIKE '%outbox%' AND name LIKE '%recovery%'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    drop(writer);
    assert_eq!(*winner, memory.read_recovery_receipt(&chain()).unwrap().unwrap());
    assert!(!memory.db_path.parent().unwrap().join("cost.jsonl").exists());
}

#[test]
#[serial_test::serial(recovery_authority)]
fn recovery_key_fds_are_cloexec() {
    let (_dir, memory) = store().unwrap();
    // Test-only checks inspect F_GETFD immediately after every production open,
    // including transient ancestor, install and load descriptors.
    let id = memory.activate_recovery_authority().unwrap();
    let keys = KeyDirectory::open(memory.db_path.parent().unwrap(), false).unwrap();
    keys.load(&id).unwrap();
    memory.rotate_recovery_authority().unwrap();
}

#[test]
#[serial_test::serial(recovery_authority)]
fn recovery_max_size_facts_still_settle() {
    let (_dir, memory) = store().unwrap();
    memory.activate_recovery_authority().unwrap();
    // Exercise worst-case JSON escaping for all six bounded identity/event IDs.
    let identity = "\u{1}".repeat(1024);
    let scope = RecoveryChain {
        workspace_id: identity.clone(),
        owner_id: identity.clone(),
        source: identity.clone(),
        run_id: identity,
    };
    let input_id = "\u{2}".repeat(1024);
    let terminal_id = "\u{3}".repeat(1024);
    for (id, role) in [(&input_id, "user"), (&terminal_id, "assistant")] {
        memory.pool.write().execute(
            "INSERT INTO message_events(event_id,workspace_id,owner_id,source,run_id,role,event_type,content,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,'message.created','original bytes','0','0')",
            params![id, scope.workspace_id, scope.owner_id, scope.source, scope.run_id, role],
        ).unwrap();
    }
    let input = memory.recovery_event_evidence(&scope, &input_id).unwrap();
    let mut publication = RecoveryPublication {
        chain: scope,
        input: Some(input),
        terminal: None,
        facts: vec![255; MAX_FACTS + 1],
    };
    let error = memory.record_recovery_pending(&publication).unwrap_err();
    assert_eq!(error.downcast_ref::<RecoveryFactsTooLarge>().unwrap().limit, MAX_FACTS);
    assert!(memory.read_recovery_receipt(&publication.chain).unwrap().is_none());
    publication.facts.pop();
    memory.record_recovery_pending(&publication).unwrap();
    publication.terminal = Some(
        memory
            .recovery_event_evidence(&publication.chain, &terminal_id)
            .unwrap(),
    );
    publication.facts.push(255);
    let error = memory.settle_recovery(&publication).unwrap_err();
    assert!(error.downcast_ref::<RecoveryFactsTooLarge>().is_some());
    assert_eq!(
        memory.read_recovery_receipt(&publication.chain).unwrap().unwrap().state,
        "pending"
    );
    publication.facts.pop();
    let settled = memory.settle_recovery(&publication).unwrap();
    assert!(settled.facts.len() <= MAX_SERIALIZED_FACTS);
    let facts: BoundFacts = serde_json::from_slice(&settled.facts).unwrap();
    assert_eq!(facts.pending, vec![255; MAX_FACTS]);
    assert_eq!(facts.completion, vec![255; MAX_FACTS]);
    assert_eq!(
        memory.read_recovery_receipt(&publication.chain).unwrap().unwrap(),
        settled
    );
}

#[tokio::test]
#[serial_test::serial(recovery_authority)]
async fn recovery_unavailable_key_is_shared_without_retry() -> anyhow::Result<()> {
    use crate::runtime::ingress_recovery::IngressRecorder;
    // Process-wide callsite interest and set_default capture; sibling tests in
    // this file also emit the shared unavailable-key warning.
    let _logs = crate::config::secret_paths::fixture::LOG_CALLSITE_LOCK.lock().await;
    let (dir, memory) = store()?;
    let id = memory.activate_recovery_authority()?;
    let key_file = memory
        .db_path
        .parent()
        .context("memory directory")?
        .join(".recovery_keys")
        .join(format!("{id}.key"));
    // Reopen so activation observes corruption on its first attempt.
    std::fs::write(&key_file, [0u8; 32])?;
    drop(memory);
    let memory = Arc::new(SqliteMemory::new(dir.path())?);
    #[derive(Clone)]
    struct Logs(Arc<parking_lot::Mutex<Vec<u8>>>);
    impl std::io::Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let logs = Logs(Arc::new(parking_lot::Mutex::new(Vec::new())));
    let sink = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .finish();
    // The first forwarded activation establishes the failure; preparations must
    // share this same result, including its one warning, without reading again.
    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        anyhow::ensure!(
            memory
                .recovery_repository()
                .context("repository")?
                .activate_recovery_authority()
                .is_err(),
            "corrupt key accepted"
        );
        Ok::<_, anyhow::Error>(())
    })?;
    let mut config = crate::config::Config::default();
    config.workspace_dir = dir.path().to_owned();
    config.causal_tree.enabled = true;
    for _ in 0..2 {
        let tracker = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
        let ticket = tracker.register(tokio_util::sync::CancellationToken::new())?;
        let result = IngressRecorder::prepare(memory.clone(), &config, &ticket).await;
        ticket.finish(true);
        anyhow::ensure!(
            matches!(result, Ok(None)),
            "corrupt authority did not yield a recovery-unavailable preparation"
        );
    }
    anyhow::ensure!(
        memory
            .recovery_activation_attempts
            .load(std::sync::atomic::Ordering::SeqCst)
            == 1,
        "activation retried"
    );
    let output = String::from_utf8(logs.0.lock().clone())?;
    anyhow::ensure!(
        output
            .matches("recovery authority unavailable; activation will not be retried")
            .count()
            == 1,
        "warning not emitted exactly once"
    );
    anyhow::ensure!(
        std::fs::read_dir(key_file.parent().context("key directory")?)?.count() == 1,
        "additional key created"
    );
    anyhow::ensure!(std::fs::read(&key_file)? == [0u8; 32], "corrupt key was replaced");
    anyhow::ensure!(
        !dir.path().join("self/causal_tree/.decision_key").exists(),
        "legacy authority created"
    );
    // Cell: a v3 capsule whose key is retired. Startup and the next append still
    // succeed; the capsule stays pending (no marker, no replay, no interruption
    // or reconciliation evidence); the shared warning appears exactly once.
    let (cell_dir, cell_store) = store()?;
    let workspace = cell_dir.path().to_owned();
    let db_path = cell_store.db_path.clone();
    let mut cell_config = crate::config::Config::default();
    cell_config.workspace_dir = workspace.clone();
    cell_config.causal_tree.enabled = true;
    let cell_input = |run: &str, key: &str| -> anyhow::Result<crate::memory::MessageEventInput> {
        Ok(serde_json::from_value(serde_json::json!({
            "workspace_id": workspace.to_string_lossy(), "owner_id": "prx_user",
            "source": "agent", "channel": "agent_api", "session_key": "prx_session",
            "run_id": run, "idempotency_key": key, "role": "user",
            "event_type": "message.created", "content": "recovery fixture", "visibility": "session"
        }))?)
    };
    let prepare = |memory: Arc<dyn crate::memory::Memory>| {
        let config = cell_config.clone();
        async move {
            let tracker = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
            let ticket = tracker.register(tokio_util::sync::CancellationToken::new())?;
            let result = IngressRecorder::prepare(memory, &config, &ticket).await;
            ticket.finish(result.is_ok());
            result?.context("recorder unavailable")
        }
    };
    let first_memory = Arc::new(cell_store);
    let first_recorder = prepare(first_memory.clone()).await?;
    let first = first_recorder
        .append(cell_input("prx-cell-1", "prx-cell-input-1")?)
        .await?;
    // Retire the capsule's key: rotate to a new active generation, then remove
    // the old file. The active authority stays usable.
    let retired = first_memory.activate_recovery_authority()?;
    first_memory.rotate_recovery_authority()?;
    // This cell is a database written before AUTH-02-1b: a capsule with no
    // attestation (an attestation would keep its key referenced, and activation
    // refuses a missing referenced key by design).
    first_memory.pool.write().execute_batch(
        "DROP TRIGGER recovery_transport_tombstone;
         DROP TABLE recovery_transport_index;
         DROP TABLE recovery_tombstones;
         DELETE FROM recovery_attestations;",
    )?;
    std::fs::remove_file(
        db_path
            .parent()
            .context("memory directory")?
            .join(".recovery_keys")
            .join(format!("{retired}.key")),
    )?;
    drop(first_recorder);
    drop(first_memory);
    let second_memory = Arc::new(SqliteMemory::new_with_path(db_path.clone())?);
    #[derive(Clone)]
    struct CellLogs(Arc<parking_lot::Mutex<Vec<u8>>>);
    impl std::io::Write for CellLogs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let cell_logs = CellLogs(Arc::new(parking_lot::Mutex::new(Vec::new())));
    let sink = cell_logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();
    let second_recorder = prepare(second_memory.clone()).await?;
    let second = second_recorder
        .append(cell_input("prx-cell-2", "prx-cell-input-2")?)
        .await?;
    anyhow::ensure!(second.id > 0, "chat append after a retired capsule key did not persist");
    let decided: i64 = second_memory.pool.write().query_row(
        "SELECT count(*) FROM message_events WHERE idempotency_key LIKE ?1",
        params![format!("ingress:{}:%", first.event_id)],
        |row| row.get(0),
    )?;
    anyhow::ensure!(decided == 0, "a retired capsule produced marker or outcome evidence");
    let warnings = String::from_utf8(cell_logs.0.lock().clone())?
        .matches("recovery authority unavailable; activation will not be retried")
        .count();
    anyhow::ensure!(warnings == 1, "shared warning emitted {warnings} times");
    Ok(())
}

#[test]
#[serial_test::serial(recovery_authority)]
fn recovery_activation_failure_is_attempted_once_under_contention() -> anyhow::Result<()> {
    let (dir, memory) = store()?;
    let id = memory.activate_recovery_authority()?;
    // Corrupt the active key: every activation attempt must fail as unavailable.
    std::fs::write(key_path(&memory, &id)?, [0u8; 32])?;
    let before = memory
        .recovery_activation_attempts
        .load(std::sync::atomic::Ordering::SeqCst);
    let memory = Arc::new(memory);
    let callers = 8;
    let barrier = Arc::new(Barrier::new(callers));
    let handles = (0..callers)
        .map(|_| {
            let memory = Arc::clone(&memory);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                memory
                    .activate_recovery_authority()
                    .map(|_| ())
                    .map_err(|error| crate::memory::recovery_authority::is_key_unavailable(&error))
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        let outcome = handle
            .join()
            .map_err(|_| anyhow::anyhow!("activation thread panicked"))?;
        anyhow::ensure!(outcome == Err(true), "queued caller was not refused as key-unavailable");
    }
    anyhow::ensure!(
        memory
            .recovery_activation_attempts
            .load(std::sync::atomic::Ordering::SeqCst)
            == before + 1,
        "activation attempted more than once under contention"
    );
    drop(dir);
    Ok(())
}

#[test]
#[serial_test::serial(recovery_authority)]
fn recovery_key_gates_do_not_grow_with_distinct_ids() -> anyhow::Result<()> {
    let (dir, memory) = store()?;
    let before = memory.recovery_key_gates.len();
    anyhow::ensure!(
        before == super::RECOVERY_KEY_GATES,
        "gate set is not the fixed stripe count"
    );
    let mut used = std::collections::HashSet::new();
    for n in 0..5000 {
        let id = format!("capsule-key-{n}");
        let stripe = super::recovery_key_gate_index(&id);
        anyhow::ensure!(stripe < before, "stripe index escaped the fixed gate set");
        anyhow::ensure!(
            stripe == super::recovery_key_gate_index(&id),
            "one id must always map to one gate"
        );
        used.insert(stripe);
    }
    anyhow::ensure!(used.len() > 1, "distinct ids never spread across stripes");
    anyhow::ensure!(memory.recovery_key_gates.len() == before, "gate set grew");
    drop(dir);
    Ok(())
}

#[test]
fn message_event_digest_is_stable_across_the_ma_scope_alter() -> anyhow::Result<()> {
    let (_dir, memory) = store()?;
    let evidence = seed(&memory, "stable", "user")?;
    // Rebuild the pre-ALTER row shape: the same table without `ma_scope_id`.
    let legacy = Connection::open_in_memory()?;
    legacy.execute(
        "ATTACH DATABASE ?1 AS live",
        [memory.db_path.to_string_lossy().as_ref()],
    )?;
    let columns: Vec<String> = {
        let conn = memory.pool.write();
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info('message_events') WHERE name <> 'ma_scope_id' ORDER BY cid")?;
        stmt.query_map([], |row| row.get(0))?.collect::<Result<_, _>>()?
    };
    legacy.execute_batch(&format!(
        "CREATE TABLE message_events AS SELECT {} FROM live.message_events",
        columns.join(", ")
    ))?;
    let before = event(&legacy, &chain(), "stable")?;
    assert_eq!(
        before, evidence,
        "an unstamped scope column must not change the digest input"
    );
    // A stamped row must still differ, so the scope cannot be rebound silently.
    let rev = "a".repeat(64);
    let (inc, scope) = (
        "00000000-0000-4000-8000-0000000000a0",
        "00000000-0000-4000-8000-0000000000b1",
    );
    {
        let conn = memory.pool.write();
        conn.execute(
            "INSERT INTO agent_incarnations VALUES ('prx_workspace','prx_owner','m',?1,'active',?2)",
            params![inc, rev],
        )?;
        conn.execute(
            "INSERT INTO agent_scopes VALUES (?1,'prx_workspace','prx_owner','sk','m',?2,1)",
            params![scope, inc],
        )?;
        conn.execute(
            "UPDATE message_events SET ma_scope_id=?1 WHERE event_id='stable'",
            [scope],
        )?;
    }
    let after = memory.recovery_event_evidence(&chain(), "stable")?;
    assert_ne!(after, evidence, "a stamped scope must change the digest input");
    Ok(())
}

fn console_chain(run: &str) -> RecoveryChain {
    RecoveryChain {
        workspace_id: "prx_workspace".into(),
        owner_id: "prx_owner".into(),
        source: "console".into(),
        run_id: run.into(),
    }
}
fn console_seed(memory: &SqliteMemory, id: &str, run: &str, role: &str) -> anyhow::Result<RecoveryEvidence> {
    memory.pool.write().execute("INSERT INTO message_events(event_id,workspace_id,owner_id,source,run_id,session_key,role,event_type,content,created_at,updated_at) VALUES(?1,'prx_workspace','prx_owner','console',?2,'sk',?3,'message.created','console bytes','0','0')", params![id,run,role])?;
    memory.recovery_event_evidence(&console_chain(run), id)
}
fn scope_of(memory: &SqliteMemory, id: &str) -> Option<String> {
    memory
        .pool
        .write()
        .query_row("SELECT ma_scope_id FROM message_events WHERE event_id=?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
}
fn set_scope(memory: &SqliteMemory, id: &str, scope: Option<&str>) {
    memory
        .pool
        .write()
        .execute(
            "UPDATE message_events SET ma_scope_id=?2 WHERE event_id=?1",
            params![id, scope],
        )
        .unwrap();
}

#[test]
fn main_attribution_rebinds_pending_and_settled_console_receipts() -> anyhow::Result<()> {
    let (_dir, memory) = store()?;
    memory.activate_recovery_authority()?;
    let path = memory.db_path.clone();
    let publish = |run: &str, input: &str, terminal: Option<&str>| -> anyhow::Result<RecoveryReceipt> {
        let mut publication = RecoveryPublication {
            chain: console_chain(run),
            input: Some(console_seed(&memory, input, run, "user")?),
            terminal: None,
            facts: b"pending console facts".to_vec(),
        };
        let pending = memory.record_recovery_pending(&publication)?;
        let receipt = if let Some(terminal) = terminal {
            publication.terminal = Some(console_seed(&memory, terminal, run, "assistant")?);
            publication.facts = b"settled console facts".to_vec();
            memory.settle_recovery(&publication)?
        } else {
            pending
        };
        Ok(receipt)
    };
    let pending_before = publish("run-pending", "p-in", None)?;
    let settled_before = publish("run-settled", "s-in", Some("s-out"))?;
    publish("run-tampered", "t-in", None)?;
    // Legacy history changed after attestation: this receipt must not be rebound.
    memory
        .pool
        .write()
        .execute("UPDATE message_events SET content='changed' WHERE event_id='t-in'", [])?;
    drop(memory);

    // Normal reopen runs the Main attribution migration.
    let memory = SqliteMemory::new_with_path(path)?;
    let scope = scope_of(&memory, "p-in").expect("verified console event is attributed to Main");
    for id in ["s-in", "s-out"] {
        assert_eq!(scope_of(&memory, id).as_deref(), Some(scope.as_str()));
    }
    assert_eq!(
        scope_of(&memory, "t-in"),
        None,
        "an unverifiable receipt keeps its event unmigrated"
    );
    let pending = memory
        .read_recovery_receipt(&console_chain("run-pending"))?
        .expect("pending receipt");
    let settled = memory
        .read_recovery_receipt(&console_chain("run-settled"))?
        .expect("settled receipt");
    assert_eq!((pending.state.as_str(), settled.state.as_str()), ("pending", "settled"));
    assert_ne!(pending.mac, pending_before.mac);
    assert_ne!(settled.mac, settled_before.mac);
    assert!(memory.read_recovery_receipt(&console_chain("run-tampered")).is_err());

    // A later change of the scope, including clearing it, is detected.
    let other = "00000000-0000-4000-8000-0000000000c3";
    let incarnation: String = memory.pool.write().query_row(
        "SELECT incarnation FROM agent_scopes WHERE scope_id=?1",
        [&scope],
        |r| r.get(0),
    )?;
    memory.pool.write().execute(
        "INSERT INTO agent_scopes VALUES (?1,'prx_workspace','prx_owner','sk2','m',?2,1)",
        params![other, incarnation],
    )?;
    for changed in [None, Some(other)] {
        set_scope(&memory, "p-in", changed);
        assert!(memory.read_recovery_receipt(&console_chain("run-pending")).is_err());
        set_scope(&memory, "p-in", Some(&scope));
        assert!(memory.read_recovery_receipt(&console_chain("run-pending")).is_ok());
    }
    Ok(())
}

/// A fresh owner-only case directory; the bundle inside it does not exist yet.
fn bundle_target(root: &std::path::Path, name: &str) -> anyhow::Result<std::path::PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let case = root.join(name);
    std::fs::DirBuilder::new().mode(0o700).create(&case)?;
    Ok(case.join("bundle").join("target.db"))
}

fn parent_of(path: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    Ok(path
        .parent()
        .context("fixture path has no parent directory")?
        .to_path_buf())
}

fn key_bytes(keys: &std::path::Path) -> anyhow::Result<std::collections::BTreeMap<String, Vec<u8>>> {
    let mut found = std::collections::BTreeMap::new();
    for entry in std::fs::read_dir(keys)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        found.insert(
            name.strip_suffix(".key").unwrap_or(&name).to_owned(),
            std::fs::read(entry.path())?,
        );
    }
    Ok(found)
}

fn directory_listing(directory: &std::path::Path) -> anyhow::Result<std::collections::BTreeSet<String>> {
    let mut names = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(directory)? {
        names.insert(entry?.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

fn mode_of(path: &std::path::Path) -> anyhow::Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(std::fs::metadata(path)?.permissions().mode() & 0o777)
}

fn refusal<T>(result: anyhow::Result<T>, what: &str) -> anyhow::Result<anyhow::Error> {
    match result {
        Ok(_) => anyhow::bail!("{what}: expected a refusal"),
        Err(error) => Ok(error),
    }
}

/// `path` as a path relative to the current directory (read only, never changed).
fn relative_to_cwd(path: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let cwd = std::env::current_dir()?;
    let from = cwd.components().collect::<Vec<_>>();
    let to = path.components().collect::<Vec<_>>();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut relative = std::path::PathBuf::new();
    for _ in common..from.len() {
        relative.push("..");
    }
    for component in to.iter().skip(common) {
        relative.push(component);
    }
    Ok(relative)
}

/// Every file and directory below `root` with its bytes.
fn tree(root: &std::path::Path) -> anyhow::Result<std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>> {
    let mut found = std::collections::BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        found.insert(directory.clone(), Vec::new());
        for entry in std::fs::read_dir(&directory)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.insert(path.clone(), std::fs::read(&path)?);
            }
        }
    }
    Ok(found)
}

/// An attempt directory is refused by the key directory, the constructor and
/// activation alike, and none of them changes its listing or any file byte.
fn assert_refused_untouched(attempt: &std::path::Path) -> anyhow::Result<()> {
    let before = tree(attempt)?;
    refusal(KeyDirectory::open(attempt, false), "opening an attempt directory")?;
    refusal(
        SqliteMemory::new_with_path(attempt.join("target.db")),
        "constructing in an attempt directory",
    )?;
    assert!(
        tree(attempt)? == before,
        "{}: listing and bytes unchanged",
        attempt.display()
    );
    Ok(())
}

/// The tag of a persisted capsule row: its recorded key id and input digest.
fn persisted_capsule_tag(memory: &SqliteMemory, input_event_id: &str) -> anyhow::Result<RecoveryTag> {
    let json: String = memory.pool.write().query_row(
        "SELECT capsule_json FROM ingress_recovery WHERE input_event_id=?1",
        [input_event_id],
        |r| r.get(0),
    )?;
    let capsule: serde_json::Value = serde_json::from_str(&json)?;
    let text = |name: &str| -> anyhow::Result<String> {
        capsule
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .with_context(|| format!("persisted capsule has no {name}"))
    };
    Ok(RecoveryTag {
        key_id: text("key_id")?,
        tag: hex::decode(text("input_digest")?)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("capsule digest is not 32 bytes"))?,
    })
}

fn count_rows(memory: &SqliteMemory, table: &str) -> anyhow::Result<i64> {
    Ok(memory
        .pool
        .write()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
}

#[tokio::test]
async fn recovery_backup_includes_every_referenced_key_generation() -> anyhow::Result<()> {
    use super::super::recovery_backup::{Step, backup_bundle, with_fault};
    use std::collections::BTreeSet;
    use std::os::unix::fs::DirBuilderExt;

    let (dir, memory) = store()?;
    let root = dir.path().canonicalize()?;
    let memory_dir = parent_of(&memory.db_path)?;
    let source_keys = memory_dir.join(".recovery_keys");

    // K0 accepts chain A, which is then erased: K0 is left referenced by its
    // tombstone alone.
    let k0 = memory.activate_recovery_authority()?;
    let a = attestation::fixture_as(&memory, "_a").await;
    attestation::append(&memory, &a, a.attestation.clone()).await?;
    erase_chain(&memory, &a, false)?;
    assert_eq!(
        count_rows(&memory, "recovery_tombstones")?,
        1,
        "erasing A left its tombstone"
    );
    // K1 accepts chain B (attestation and capsule).
    let k1 = memory.rotate_recovery_authority()?;
    let b = attestation::fixture_as(&memory, "_b").await;
    assert_eq!(b.capsule.key_id.as_deref(), Some(k1.as_str()));
    attestation::append(&memory, &b, b.attestation.clone()).await?;
    // Kc accepts chain C; erasing it and its tombstone leaves Kc referenced by
    // C's valid v3 capsule alone. K2 is only the active key.
    let kc = memory.rotate_recovery_authority()?;
    let c = attestation::fixture_as(&memory, "_c").await;
    assert_eq!(c.capsule.key_id.as_deref(), Some(kc.as_str()));
    attestation::append(&memory, &c, c.attestation.clone()).await?;
    erase_chain(&memory, &c, true)?;
    memory
        .pool
        .write()
        .execute("DELETE FROM recovery_tombstones WHERE key_id=?1", [&kc])?;
    let k2 = memory.rotate_recovery_authority()?;
    // Kc is referenced by C's real capsule row and by nothing else.
    {
        let reader = memory.pool.write();
        let references = |sql: &str| -> anyhow::Result<i64> { Ok(reader.query_row(sql, [&kc], |r| r.get(0))?) };
        for table in [
            "recovery_attestations",
            "recovery_transport_index",
            "recovery_tombstones",
        ] {
            assert_eq!(
                references(&format!("SELECT count(*) FROM {table} WHERE key_id=?1"))?,
                0,
                "{table}"
            );
        }
        assert_eq!(
            references("SELECT count(*) FROM recovery_authority WHERE active_key_id=?1")?,
            0
        );
        assert_eq!(
            references("SELECT count(*) FROM ingress_recovery WHERE json_extract(capsule_json,'$.key_id')=?1")?,
            1,
            "Kc is referenced only by the persisted capsule"
        );
    }
    let expected: BTreeSet<String> = [&k0, &k1, &k2, &kc].into_iter().cloned().collect();
    assert_eq!(expected.len(), 4);
    let source = key_bytes(&source_keys)?;
    assert_eq!(source.keys().cloned().collect::<BTreeSet<_>>(), expected);
    let chain_b = RecoveryChain {
        workspace_id: b.capsule.scope.workspace_id.clone(),
        owner_id: "prx_owner".into(),
        source: "agent".into(),
        run_id: b.capsule.run_id.clone(),
    };

    // (i) The published bundle carries all four generations, in an owner-only
    // directory even when the destination was pre-created with default
    // permissions, and restores.
    let target = bundle_target(&root, "complete")?;
    let bundle = parent_of(&target)?;
    std::fs::create_dir(&bundle)?;
    std::fs::set_permissions(&bundle, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    memory.backup_database_to(&target).await?;
    assert_eq!(mode_of(&bundle)?, 0o700, "published directory itself is owner-only");
    let keys = bundle.join(".recovery_keys");
    assert_eq!(mode_of(&keys)?, 0o700);
    for id in &expected {
        assert_eq!(mode_of(&keys.join(format!("{id}.key")))?, 0o600);
    }
    assert!(key_bytes(&keys)? == source, "key listing and bytes equal the source");
    assert_eq!(
        directory_listing(&bundle)?,
        [".recovery_keys".to_owned(), "target.db".to_owned()]
            .into_iter()
            .collect()
    );
    let restored = SqliteMemory::new_with_path(target.clone())?;
    assert_eq!(restored.activate_recovery_authority()?, k2);
    assert!(restored.read_recovery_receipt(&chain_b)?.is_some());
    // The persisted capsule row survives the backup and verifies under Kc.
    let capsule_tag = persisted_capsule_tag(&restored, &c.capsule.input_event_id)?;
    assert_eq!(capsule_tag.key_id, kc);
    restored.verify_recovery_mac(&capsule_tag, RecoveryMacDomain::InputDigestV3, b"input")?;
    assert!(matches!(
        stable_key_fence(&restored.pool.write(), Some(bundle.as_path()), &a.input)?,
        Fence::Tombstoned
    ));
    drop(restored);

    // (v) The migration path: a live WAL row, the notes and every key generation
    // are published together; a fault before publication leaves only refused attempts.
    seed(&memory, "live-wal-row", "user")?;
    std::fs::write(root.join("MEMORY.md"), b"core notes")?;
    std::fs::write(memory_dir.join("2026-01-01.md"), b"daily notes")?;
    let migrations = memory_dir.join("migrations");
    let crashed = with_fault(
        {
            let leftover = root.join("migration-leftover");
            move |step, at| {
                if step == Step::Staged {
                    std::fs::rename(at, &leftover)?;
                    anyhow::bail!("simulated crash");
                }
                Ok(())
            }
        },
        || crate::migration::backup_target_memory(&root),
    );
    refusal(crashed, "migration backup crash")?;
    assert!(
        directory_listing(&migrations)?.is_empty(),
        "nothing published by the crash"
    );
    assert_refused_untouched(&root.join("migration-leftover"))?;
    let migrated = crate::migration::backup_target_memory(&root)?.context("the migration backup was created")?;
    assert_eq!(mode_of(&migrated)?, 0o700);
    assert!(
        key_bytes(&migrated.join(".recovery_keys"))? == source,
        "migration bundle carries every key"
    );
    assert_eq!(std::fs::read(migrated.join("MEMORY.md"))?, b"core notes");
    assert_eq!(
        std::fs::read(migrated.join("daily").join("2026-01-01.md"))?,
        b"daily notes"
    );
    let restored = SqliteMemory::new_with_path(migrated.join("brain.db"))?;
    let live_rows: i64 = restored.pool.write().query_row(
        "SELECT count(*) FROM message_events WHERE event_id='live-wal-row'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(live_rows, 1, "the uncheckpointed row is in the bundle");
    assert_eq!(restored.activate_recovery_authority()?, k2);
    drop(restored);

    // (ii) A restore missing any referenced generation fails closed, never
    // regenerates, and leaves the bundle mode and every remaining key byte alone.
    for (index, missing) in [k1.clone(), k0.clone(), kc.clone()].into_iter().enumerate() {
        let target = bundle_target(&root, &format!("missing-{index}"))?;
        let bundle = parent_of(&target)?;
        memory.backup_database_to(&target).await?;
        let keys = bundle.join(".recovery_keys");
        std::fs::remove_file(keys.join(format!("{missing}.key")))?;
        let mut remaining = source.clone();
        remaining.remove(&missing);
        let restored = SqliteMemory::new_with_path(target)?;
        let error = if missing == kc {
            let capsule_tag = persisted_capsule_tag(&restored, &c.capsule.input_event_id)?;
            refusal(
                restored.verify_recovery_mac(&capsule_tag, RecoveryMacDomain::InputDigestV3, b"input"),
                "capsule verify without its key",
            )?
        } else {
            refusal(
                restored.activate_recovery_authority(),
                "activation without a referenced key",
            )?
        };
        assert!(crate::memory::recovery_authority::is_key_unavailable(&error));
        assert!(
            key_bytes(&keys)? == remaining,
            "a restore never regenerates or alters a key"
        );
        assert_eq!(mode_of(&bundle)?, 0o700);
        assert_eq!(mode_of(&keys)?, 0o700);
    }

    // (iii) A source missing a referenced key refuses the backup and publishes nothing.
    let lost = source_keys.join(format!("{k1}.key"));
    std::fs::remove_file(&lost)?;
    let target = bundle_target(&root, "source-missing")?;
    let case = parent_of(&parent_of(&target)?)?;
    refusal(
        memory.backup_database_to(&target).await,
        "backup without a referenced key",
    )?;
    assert!(
        directory_listing(&case)?.is_empty(),
        "nothing published, staging removed"
    );
    let lost_bytes: [u8; 32] = source.get(&k1).context("source bytes of K1")?.as_slice().try_into()?;
    KeyDirectory::open(&memory_dir, false)?.install(&k1, &lost_bytes)?;
    assert!(key_bytes(&source_keys)? == source, "K1 was reinstalled unchanged");

    // (iv) A never-activated database backs up with no key directory.
    let (fresh_dir, fresh) = store()?;
    let target = bundle_target(&fresh_dir.path().canonicalize()?, "fresh")?;
    fresh.backup_database_to(&target).await?;
    assert_eq!(
        directory_listing(&parent_of(&target)?)?,
        std::collections::BTreeSet::from(["target.db".to_owned()])
    );
    assert_eq!(mode_of(&parent_of(&target)?)?, 0o700);

    // (vi) A failure after each side effect, including after publication began,
    // publishes nothing and removes only this attempt's files.
    for (index, failing) in [
        Step::Install,
        Step::AfterInstall,
        Step::Staged,
        Step::Renamed,
        Step::Published,
    ]
    .into_iter()
    .enumerate()
    {
        let target = bundle_target(&root, &format!("fails-{index}"))?;
        let case = parent_of(&parent_of(&target)?)?;
        let result = with_fault(
            move |step, _| {
                anyhow::ensure!(step != failing, "injected failure");
                Ok(())
            },
            || backup_bundle(&memory.pool.write(), &target),
        );
        refusal(result, "injected backup failure")?;
        assert!(directory_listing(&case)?.is_empty(), "{failing:?}: nothing left behind");
        assert!(key_bytes(&source_keys)? == source, "{failing:?}: source keys untouched");
    }

    // (vii) A crash at each step leaves a directory that is neither restorable
    // nor a fresh authority, and no key is created or altered.
    for (index, crash) in [Step::AfterInstall, Step::Staged, Step::Renamed]
        .into_iter()
        .enumerate()
    {
        let target = bundle_target(&root, &format!("crash-{index}"))?;
        let case = parent_of(&parent_of(&target)?)?;
        let leftover = case.join("leftover");
        let moved = leftover.clone();
        let crashed = with_fault(
            move |step, at| {
                if step == crash {
                    std::fs::rename(at, &moved)?;
                    anyhow::bail!("simulated crash");
                }
                Ok(())
            },
            || backup_bundle(&memory.pool.write(), &target),
        );
        refusal(crashed, "simulated crash")?;
        assert!(!target.exists(), "{crash:?}: nothing was published");
        assert_refused_untouched(&leftover)?;
        assert_eq!(mode_of(&leftover)?, 0o700);
        // Only a completed publication restores.
        std::fs::remove_dir_all(&leftover)?;
        memory.backup_database_to(&target).await?;
        let restored = SqliteMemory::new_with_path(target)?;
        assert_eq!(restored.activate_recovery_authority()?, k2);
    }
    // Attempt directories: refused by name (including ASCII-case aliases) when
    // empty or markerless keys-only, and by marker when moved to a plain name.
    let shapes = [
        (".backup-incomplete-pre-marker", false, false),
        (".BACKUP-INCOMPLETE-alias-empty", false, false),
        (".backup-incomplete-keys-only", true, false),
        (".Backup-Incomplete-alias-keys", true, false),
        ("renamed-empty", false, true),
        ("renamed-keys", true, true),
    ];
    for (name, with_keys, with_marker) in shapes {
        let case = parent_of(&parent_of(&bundle_target(&root, &format!("shape-{name}"))?)?)?;
        let leftover = case.join(name);
        std::fs::DirBuilder::new().mode(0o700).create(&leftover)?;
        if with_marker {
            std::fs::write(leftover.join(".backup-incomplete"), b"")?;
        }
        if with_keys {
            let keys = leftover.join(".recovery_keys");
            std::fs::DirBuilder::new().mode(0o700).create(&keys)?;
            for (id, bytes) in &source {
                let file = keys.join(format!("{id}.key"));
                std::fs::write(&file, bytes)?;
                std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
            }
        }
        assert_refused_untouched(&leftover)?;
        // Below the attempt, absolute or relative to the working directory:
        // constructor and backup destination refuse without creating anything.
        let before = tree(&case)?;
        let descendant = leftover.join("inner").join("target.db");
        refusal(
            SqliteMemory::new_with_path(descendant.clone()),
            "constructing below an attempt",
        )?;
        refusal(
            memory.backup_database_to(&descendant).await,
            "backing up below an attempt",
        )?;
        refusal(
            SqliteMemory::new_with_path(relative_to_cwd(&leftover.join("target.db"))?),
            "constructing through a relative path",
        )?;
        refusal(
            SqliteMemory::new_with_path(relative_to_cwd(&descendant)?),
            "constructing below an attempt through a relative path",
        )?;
        assert!(tree(&case)? == before, "{name}: listing and bytes unchanged");
    }
    // An already-open handle whose directory becomes an attempt refuses activation.
    let (_open_dir, open) = store()?;
    let open_memory = parent_of(&open.db_path)?;
    std::fs::write(open_memory.join(".backup-incomplete"), b"")?;
    let without_database = |directory: &std::path::Path| -> anyhow::Result<_> {
        let mut found = tree(directory)?;
        found.retain(|path, _| {
            !path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("brain.db"))
        });
        Ok(found)
    };
    let before = without_database(&open_memory)?;
    refusal(
        open.activate_recovery_authority(),
        "activating an open handle in an attempt",
    )?;
    assert!(
        without_database(&open_memory)? == before,
        "open handle: listing and bytes unchanged"
    );
    assert!(!open_memory.join(".recovery_keys").exists());
    // A symlink alias of a reserved or moved-with-marker attempt, and `..`, are
    // refused by constructor and backup destination with nothing changed.
    {
        let case = parent_of(&parent_of(&bundle_target(&root, "symlinks")?)?)?;
        let reserved = case.join(".backup-incomplete-sym");
        let moved = case.join("moved");
        for directory in [&reserved, &moved] {
            std::fs::DirBuilder::new().mode(0o700).create(directory)?;
        }
        std::fs::write(moved.join(".backup-incomplete"), b"")?;
        let alias_reserved = case.join("alias-reserved");
        let alias_moved = case.join("alias-moved");
        std::os::unix::fs::symlink(&reserved, &alias_reserved)?;
        std::os::unix::fs::symlink(&moved, &alias_moved)?;
        let before = tree(&case)?;
        for destination in [
            alias_reserved.join("inner").join("target.db"),
            alias_moved.join("inner").join("target.db"),
            alias_moved.join("..").join("moved").join("inner").join("target.db"),
            case.join("plain").join("..").join("target.db"),
        ] {
            refusal(
                SqliteMemory::new_with_path(destination.clone()),
                "constructing through an alias",
            )?;
            refusal(
                memory.backup_database_to(&destination).await,
                "backing up through an alias",
            )?;
        }
        assert!(tree(&case)? == before, "aliases: listing and bytes unchanged");
        // A benign symlink to an ordinary directory is admitted, and the store
        // lands in the physical directory.
        let ordinary = case.join("ordinary");
        std::fs::DirBuilder::new().mode(0o700).create(&ordinary)?;
        std::os::unix::fs::symlink(&ordinary, case.join("alias-ordinary"))?;
        let benign = SqliteMemory::new_with_path(case.join("alias-ordinary").join("target.db"))?;
        assert!(ordinary.join("target.db").is_file());
        assert_eq!(parent_of(&benign.db_path)?, ordinary.canonicalize()?);
    }
    // A reserved final basename and a reserved ancestor are rejected with no side effect.
    for (index, reserved) in [".backup-incomplete-x/target.db", ".BACKUP-INCOMPLETE-y/inner/target.db"]
        .into_iter()
        .enumerate()
    {
        let case = parent_of(&parent_of(&bundle_target(&root, &format!("reserved-{index}"))?)?)?;
        refusal(
            memory.backup_database_to(&case.join(reserved)).await,
            "backup to a reserved destination",
        )?;
        assert!(directory_listing(&case)?.is_empty(), "{reserved}: no side effect");
    }
    // The quarantine durability barrier and an interrupted cleanup keep what
    // remains refused: intact at the barrier, keys-only after a partial delete.
    for (index, stage) in [Step::Barrier, Step::Cleanup].into_iter().enumerate() {
        let target = bundle_target(&root, &format!("quarantine-{index}"))?;
        let case = parent_of(&parent_of(&target)?)?;
        let result = with_fault(
            move |step, at| {
                anyhow::ensure!(step != Step::Published, "publication failed");
                if step == stage {
                    if stage == Step::Cleanup {
                        std::fs::remove_file(at.join("target.db"))?;
                    }
                    anyhow::bail!("quarantine interrupted");
                }
                Ok(())
            },
            || backup_bundle(&memory.pool.write(), &target),
        );
        refusal(result, "failed publication")?;
        let names = directory_listing(&case)?;
        assert_eq!(names.len(), 1, "{stage:?}: exactly the quarantined directory remains");
        let quarantined = case.join(names.iter().next().context("quarantined directory")?);
        assert!(!target.exists());
        assert!(quarantined.join(".recovery_keys").is_dir());
        assert_eq!(quarantined.join("target.db").exists(), stage == Step::Barrier);
        assert_refused_untouched(&quarantined)?;
    }
    // A bundle directory that does not exist yet is created owner-only by the rename.
    let nested = root.join("nested-parent").join("deeper");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(root.join("nested-parent"))?;
    memory.backup_database_to(&nested.join("target.db")).await?;
    assert_eq!(mode_of(&nested)?, 0o700);
    Ok(())
}

/// Production-shaped erasure of one accepted chain: the transport row goes
/// first so the tombstone trigger records the stable key under its key id.
fn erase_chain(memory: &SqliteMemory, f: &attestation::Fixture, keep_capsule: bool) -> anyhow::Result<()> {
    let writer = memory.pool.write();
    if !keep_capsule {
        writer.execute(
            "DELETE FROM ingress_recovery WHERE input_event_id=?1",
            [&f.capsule.input_event_id],
        )?;
    }
    writer.execute(
        "DELETE FROM recovery_transport_index WHERE run_id=?1",
        [&f.capsule.run_id],
    )?;
    writer.execute("DELETE FROM recovery_attestations WHERE run_id=?1", [&f.capsule.run_id])?;
    Ok(())
}

mod attestation {
    use super::*;
    use crate::memory::ingress_recovery::{IngressAppendOutcome, IngressLifetime, IngressRecoveryScope};
    use crate::memory::recovery_authority::RecoveryMacDomain;
    use crate::memory::{MessageEventInput, MessageEventSource};

    pub(super) struct Fixture {
        pub(super) input: MessageEventInput,
        pub(super) capsule: IngressCapsule,
        pub(super) lifetime: Arc<dyn IngressLifetime>,
        pub(super) facts: PendingFacts,
        pub(super) attestation: IngressAttestation,
    }

    fn count(memory: &SqliteMemory, table: &str) -> i64 {
        memory
            .pool
            .write()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    async fn fixture(memory: &SqliteMemory) -> Fixture {
        fixture_as(memory, "").await
    }

    /// A fixture whose input, stable key and run carry `tag`, so several
    /// chains can coexist in one store.
    pub(super) async fn fixture_as(memory: &SqliteMemory, tag: &str) -> Fixture {
        let key_id = memory.activate_recovery_authority().unwrap();
        let workspace = memory
            .db_path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let lifetime = memory
            .acquire_ingress_lifetime(&workspace, &uuid::Uuid::new_v4().to_string(), deadline)
            .await
            .unwrap();
        let input_key = format!("prx_input_key{tag}");
        let input_key = input_key.as_str();
        let digest = memory
            .recovery_mac(Some(&key_id), RecoveryMacDomain::InputDigestV3, b"input")
            .unwrap();
        let capsule = IngressCapsule {
            version: 3,
            key_id: Some(key_id.clone()),
            backend_namespace: lifetime.namespace().to_owned(),
            scope: IngressRecoveryScope {
                workspace_id: workspace.clone(),
                owner_id: "prx_owner".into(),
                source: MessageEventSource::Agent,
            },
            lifetime: lifetime.token().to_owned(),
            input_event_id: format!("prx_input_event{tag}"),
            input_key: input_key.into(),
            run_id: format!("prx_run{tag}"),
            input_digest: hex::encode(digest.tag),
            authentication: "0".repeat(64),
        };
        let input: MessageEventInput = serde_json::from_value(serde_json::json!({
            "event_id": capsule.input_event_id, "workspace_id": workspace, "owner_id": "prx_owner",
            "source": "agent", "channel": "agent_api", "session_key": "prx_session",
            "run_id": capsule.run_id, "idempotency_key": input_key, "role": "user",
            "event_type": "message.created", "content": "attested", "visibility": "session",
            "config_generation_id": 7,
            "raw_payload_json": serde_json::json!({
                "prx_ingress_capsule": capsule, "original_payload_json": null
            }).to_string()
        }))
        .unwrap();
        let transport = memory
            .recovery_mac(
                Some(&key_id),
                RecoveryMacDomain::TransportV3,
                &crate::memory::ingress_recovery::transport_preimage(&input, None),
            )
            .unwrap();
        let key_tag = memory
            .recovery_mac(Some(&key_id), RecoveryMacDomain::InputKeyV3, input_key.as_bytes())
            .unwrap();
        use base64::Engine as _;
        let facts = PendingFacts {
            v: 1,
            namespace: capsule.backend_namespace.clone(),
            workspace_id: workspace.clone(),
            owner_id: "prx_owner".into(),
            source: "agent".into(),
            channel: Some("agent_api".into()),
            session_key: Some("prx_session".into()),
            surface: "agent_loop".into(),
            generation: 7,
            lifetime: capsule.lifetime.clone(),
            input_event_id: capsule.input_event_id.clone(),
            input_key: input_key.into(),
            capsule_version: 3,
            capsule: base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&capsule).unwrap()),
            input_digest: capsule.input_digest.clone(),
            transport_digest: hex::encode(transport.tag),
            input_key_digest: hex::encode(key_tag.tag),
            key_id,
        };
        let attestation = IngressAttestation::new(serde_json::to_vec(&facts).unwrap(), transport, key_tag);
        Fixture {
            input,
            capsule,
            lifetime,
            facts,
            attestation,
        }
    }

    pub(super) async fn append(
        memory: &SqliteMemory,
        f: &Fixture,
        attestation: IngressAttestation,
    ) -> anyhow::Result<IngressAppendOutcome> {
        memory
            .append_ingress_input(
                f.input.clone(),
                f.capsule.clone(),
                f.lifetime.clone(),
                std::time::Instant::now() + std::time::Duration::from_secs(30),
                Some(attestation),
            )
            .await
    }

    fn mutated(f: &Fixture, edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>)) -> IngressAttestation {
        let mut value = serde_json::to_value(&f.facts).unwrap();
        edit(value.as_object_mut().unwrap());
        let mut attestation = f.attestation.clone();
        attestation.facts = serde_json::to_vec(&value).unwrap();
        attestation
    }

    async fn append_input(
        memory: &dyn crate::memory::Memory,
        f: &Fixture,
        input: MessageEventInput,
        attestation: IngressAttestation,
    ) -> anyhow::Result<IngressAppendOutcome> {
        memory
            .append_ingress_input(
                input,
                f.capsule.clone(),
                f.lifetime.clone(),
                std::time::Instant::now() + std::time::Duration::from_secs(30),
                Some(attestation),
            )
            .await
    }

    #[tokio::test]
    async fn recovery_transport_substitution_is_refused() {
        for backend in ["sqlite", "encrypted"] {
            let (dir, memory) = store().unwrap();
            let f = fixture(&memory).await;
            let target: Arc<dyn crate::memory::Memory> = if backend == "encrypted" {
                let workspace = std::path::PathBuf::from(&f.capsule.scope.workspace_id);
                let crypto = Arc::new(crate::security::ramflux_aead::AccountCrypto::new(
                    &workspace, &workspace, true,
                ));
                crypto.unlock("prx_account").unwrap();
                Arc::new(crate::memory::encrypted_scope::EncryptedScope::new(
                    Arc::new(memory.clone()),
                    crypto,
                    "prx_account",
                    "prx_scope",
                ))
            } else {
                Arc::new(memory.clone())
            };
            let mismatch = Some(&IngressAttestationError::Mismatch("transport_digest"));
            let assert_nothing_written = || {
                for table in ["message_events", "recovery_attestations", "recovery_transport_index"] {
                    assert_eq!(count(&memory, table), 0, "{backend}: {table} kept a refused write");
                }
            };

            // (i) A coherent tag and facts over an unrelated transport.
            let unrelated = memory
                .recovery_mac(
                    f.capsule.key_id.as_deref(),
                    RecoveryMacDomain::TransportV3,
                    b"transport",
                )
                .unwrap();
            let mut substituted = mutated(&f, |m| {
                m.insert(
                    "transport_digest".into(),
                    serde_json::Value::String(hex::encode(unrelated.tag)),
                );
            });
            substituted.transport = unrelated;
            let error = append_input(target.as_ref(), &f, f.input.clone(), substituted)
                .await
                .err()
                .unwrap();
            assert_eq!(
                error.downcast_ref::<IngressAttestationError>(),
                mismatch,
                "{backend}: {error:#}"
            );
            assert_nothing_written();

            // (ii) The real attestation, replayed with different content.
            let mut other = f.input.clone();
            other.content = "substituted content".to_owned();
            let error = append_input(target.as_ref(), &f, other, f.attestation.clone())
                .await
                .err()
                .unwrap();
            assert_eq!(
                error.downcast_ref::<IngressAttestationError>(),
                mismatch,
                "{backend}: {error:#}"
            );
            assert_nothing_written();

            // (iii) The genuine pair commits, and the stored tag verifies against A.
            assert!(matches!(
                append_input(target.as_ref(), &f, f.input.clone(), f.attestation.clone())
                    .await
                    .unwrap(),
                IngressAppendOutcome::Inserted(_)
            ));
            memory
                .verify_recovery_mac(
                    &f.attestation.transport,
                    RecoveryMacDomain::TransportV3,
                    &crate::memory::ingress_recovery::transport_preimage(&f.input, None),
                )
                .unwrap();
            let stored: String = memory
                .pool
                .write()
                .query_row("SELECT content FROM message_events", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                stored != "attested",
                backend == "encrypted",
                "{backend}: stored content"
            );
            drop(dir);
        }
    }

    #[tokio::test]
    async fn recovery_wrong_owner_or_workspace_is_refused() {
        let (dir, memory) = store().unwrap();
        let f = fixture(&memory).await;
        let other = uuid::Uuid::new_v4().to_string();
        let text = |v: &str| serde_json::Value::String(v.to_owned());
        let mut cells: Vec<(&'static str, IngressAttestation)> = Vec::new();
        let mut set = |name: &'static str, value: serde_json::Value| {
            cells.push((
                name,
                mutated(&f, |m| {
                    m.insert(name.into(), value);
                }),
            ));
        };
        set("v", 2.into());
        set("namespace", text(&other));
        set("workspace_id", text("prx_other_workspace"));
        set("owner_id", text("prx_other_owner"));
        set("source", text("cli"));
        set("channel", text("terminal"));
        set("session_key", text("prx_other_session"));
        set("surface", text("cron"));
        set("generation", 8.into());
        set("lifetime", text(&other));
        set("input_event_id", text("prx_other_event"));
        set("input_key", text("prx_other_key"));
        // Legacy format: the capsule version, and the key id that version implies.
        set("capsule_version", 2.into());
        set("capsule", text("AAAA"));
        set("input_digest", text(&"1".repeat(64)));
        set("transport_digest", text(&"2".repeat(64)));
        set("input_key_digest", text(&"3".repeat(64)));
        set("key_id", text(&other));
        for (name, attestation) in cells {
            let error = append(&memory, &f, attestation).await.err().unwrap();
            assert_eq!(
                error.downcast_ref::<IngressAttestationError>(),
                Some(&IngressAttestationError::Mismatch(name)),
                "cell {name}: {error:#}"
            );
        }
        // A forged tag next to honest facts: the stable-key tag must verify.
        let mut forged = f.attestation.clone();
        forged.input_key.tag = [9; 32];
        let facts = mutated(&f, |m| {
            m.insert(
                "input_key_digest".into(),
                serde_json::Value::String(hex::encode([9u8; 32])),
            );
        });
        forged.facts = facts.facts;
        assert_eq!(
            append(&memory, &f, forged)
                .await
                .err()
                .unwrap()
                .downcast_ref::<IngressAttestationError>(),
            Some(&IngressAttestationError::Mismatch("input_key_digest"))
        );
        let unknown = mutated(&f, |m| {
            m.insert("extra".into(), 1.into());
        });
        assert_eq!(
            append(&memory, &f, unknown)
                .await
                .err()
                .unwrap()
                .downcast_ref::<IngressAttestationError>(),
            Some(&IngressAttestationError::UnknownKey)
        );
        let missing = mutated(&f, |m| {
            m.remove("surface");
        });
        assert_eq!(
            append(&memory, &f, missing)
                .await
                .err()
                .unwrap()
                .downcast_ref::<IngressAttestationError>(),
            Some(&IngressAttestationError::Malformed)
        );
        // Every refusal rolled the whole transaction back.
        for table in [
            "message_events",
            "recovery_attestations",
            "recovery_transport_index",
            "ingress_recovery",
        ] {
            assert_eq!(count(&memory, table), 0, "{table} kept a refused write");
        }
        // The honest attestation commits all of it together, and survives a reopen.
        assert!(matches!(
            append(&memory, &f, f.attestation.clone()).await.unwrap(),
            IngressAppendOutcome::Inserted(_)
        ));
        let reopened = SqliteMemory::new_with_path(memory.db_path.clone()).unwrap();
        for table in ["message_events", "recovery_attestations", "recovery_transport_index"] {
            assert_eq!(count(&reopened, table), 1, "{table}");
        }
        let chain = RecoveryChain {
            workspace_id: f.capsule.scope.workspace_id.clone(),
            owner_id: "prx_owner".into(),
            source: "agent".into(),
            run_id: "prx_run".into(),
        };
        let receipt = reopened.read_recovery_receipt(&chain).unwrap().unwrap();
        assert_eq!(receipt.state, "pending");
        assert_eq!(receipt.pending_facts().unwrap(), f.attestation.facts);
        reopened
            .verify_recovery_mac(
                &f.attestation.transport,
                RecoveryMacDomain::TransportV3,
                &crate::memory::ingress_recovery::transport_preimage(&f.input, None),
            )
            .unwrap();
        assert!(
            reopened
                .verify_recovery_mac(&f.attestation.transport, RecoveryMacDomain::TransportV3, b"other")
                .is_err()
        );
        drop(dir);
    }
}

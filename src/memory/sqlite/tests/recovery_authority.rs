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
async fn recovery_unavailable_key_is_shared_without_retry() -> anyhow::Result<()> {
    use crate::runtime::ingress_recovery::IngressRecorder;
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
        anyhow::ensure!(result.is_err(), "corrupt authority made recorder available");
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

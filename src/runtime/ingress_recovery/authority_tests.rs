#![cfg(test)]
use super::*;
use crate::memory::SqliteMemory;
use crate::memory::recovery_authority::{RecoveryRepository, RecoveryTag};

fn directory() -> anyhow::Result<tempfile::TempDir> {
    let home = std::env::var_os("HOME").context("fixture HOME unavailable")?;
    Ok(tempfile::Builder::new().prefix(".prx-auth2-").tempdir_in(home)?)
}

fn input(workspace: &std::path::Path, run: &str) -> anyhow::Result<MessageEventInput> {
    Ok(serde_json::from_value(serde_json::json!({
        "workspace_id": workspace.to_string_lossy(), "owner_id": "prx_user",
        "source": "agent", "channel": "agent_api", "session_key": "prx_session",
        "run_id": run, "idempotency_key": "prx_input", "role": "user",
        "event_type": "message.created", "content": "recovery fixture", "visibility": "session"
    }))?)
}

async fn recorder(memory: Arc<dyn Memory>, workspace: &std::path::Path) -> anyhow::Result<Arc<IngressRecorder>> {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_owned();
    config.causal_tree.enabled = true;
    let owner = Arc::new(super::super::turn_cleanup::CleanupTracker::default());
    let ticket = owner.register(tokio_util::sync::CancellationToken::new())?;
    let result = IngressRecorder::prepare(memory, &config, &ticket)
        .await?
        .context("recorder unavailable");
    ticket.finish(result.is_ok());
    result
}

#[tokio::test]
async fn recovery_legacy_mac_compatibility_is_exact() -> anyhow::Result<()> {
    let dir = directory()?;
    let workspace = dir.path().canonicalize()?;
    DecisionKey::load_or_create(&workspace)?;
    std::fs::write(workspace.join("self/causal_tree/.decision_key"), [42u8; 32])?;
    let legacy = DecisionKey::load_existing(&workspace)?;
    let memory = Arc::new(SqliteMemory::new(&workspace)?);
    let rec = recorder(memory.clone(), &workspace).await?;
    let event = rec.append(input(&workspace, "prx_run")?).await?;
    let original = capsule_from_event(&event)?;
    let normalized = rec.verify(&original, &event)?;
    // Immutable pre-AUTH-02 grammar, with a fixed fixture key.
    for (version, golden, quarantine) in [
        (
            1,
            r#"{"version":1,"backend_namespace":"00000000-0000-0000-0000-000000000000","scope":{"workspace_id":"prx_workspace","owner_id":"prx_user","source":"agent"},"lifetime":"00000000-0000-0000-0000-000000000001","input_event_id":"prx_event","input_key":"prx_input","run_id":"prx_run","input_digest":"0000000000000000000000000000000000000000000000000000000000000000","authentication":"0fc00e1561caad90ab5f91d52c4348d55aa4d8e67f5e760a6903311ebb2acc39"}"#,
            "a6a94412c809806126d8edbe3ac7bde9ef726b900085f18f1b9cc5b3e333bfe9",
        ),
        (
            2,
            r#"{"version":2,"backend_namespace":"00000000-0000-0000-0000-000000000000","scope":{"workspace_id":"prx_workspace","owner_id":"prx_user","source":"agent"},"lifetime":"00000000-0000-0000-0000-000000000001","input_event_id":"prx_event","input_key":"prx_input","run_id":"prx_run","input_digest":"0000000000000000000000000000000000000000000000000000000000000000","authentication":"a3ec43dd93684f3912a1728a0040f8a4c38a403057322ceb1440e23fc3b2a4c4"}"#,
            "61baa487cb41b022cc597d0b56bd7af576d50d9c387d252a509b9ffa816a2a70",
        ),
    ] {
        let decoded = IngressCapsule::decode(golden)?;
        anyhow::ensure!(
            serde_json::to_string(&decoded)? == golden,
            "legacy golden bytes changed"
        );
        let mut unsigned = decoded.clone();
        unsigned.authentication.clear();
        let bytes = serde_json::to_vec(&unsigned)?;
        anyhow::ensure!(
            rec.verify_slot(
                &decoded,
                RecoveryMacDomain::CapsuleV3,
                &[&bytes],
                &bytes,
                &hex::decode(&decoded.authentication)?
            )?,
            "legacy golden MAC changed"
        );
        anyhow::ensure!(
            legacy.verify_hmac(
                KeyDomain::IngressRecovery,
                &[b"quarantine-key-v1", golden.as_bytes()],
                &hex::decode(quarantine)?
            ),
            "legacy golden quarantine MAC changed"
        );
        let mut capsule = original.clone();
        capsule.version = version;
        capsule.key_id = None;
        capsule.authentication.clear();
        capsule.input_digest = if version == 1 {
            digest(&normalized)?
        } else {
            hex::encode(legacy.hmac(
                KeyDomain::IngressRecovery,
                &[b"input-digest-v2", digest(&normalized)?.as_bytes()],
            ))
        };
        let bytes = serde_json::to_vec(&capsule)?;
        anyhow::ensure!(
            !String::from_utf8(bytes.clone())?.contains("key_id"),
            "legacy serialization changed"
        );
        let tag = legacy.hmac(KeyDomain::IngressRecovery, &[&bytes]);
        capsule.authentication = hex::encode(tag);
        anyhow::ensure!(
            legacy.verify_hmac(
                KeyDomain::IngressRecovery,
                &[&bytes],
                &hex::decode(rec.sign(&capsule)?)?
            ),
            "legacy capsule preimage changed"
        );
        let quarantine_key = rec.authenticated_quarantine_key(&capsule)?;
        let (prefix, quarantine_tag) = quarantine_key.rsplit_once(':').context("quarantine key grammar")?;
        anyhow::ensure!(
            prefix == IngressRecorder::quarantine_key(&capsule),
            "legacy quarantine key changed"
        );
        anyhow::ensure!(
            legacy.verify_hmac(
                KeyDomain::IngressRecovery,
                &[b"quarantine-key-v1", &serde_json::to_vec(&capsule)?],
                &hex::decode(quarantine_tag)?
            ),
            "legacy quarantine grammar changed"
        );
        let mut legacy_event = event.clone();
        let mut payload: serde_json::Value =
            serde_json::from_str(legacy_event.raw_payload_json.as_deref().context("payload")?)?;
        *payload.get_mut("prx_ingress_capsule").context("capsule")? = serde_json::to_value(&capsule)?;
        legacy_event.raw_payload_json = Some(payload.to_string());
        rec.verify(&capsule, &legacy_event)?;
        if version == 1 {
            let crypto = Arc::new(crate::security::ramflux_aead::AccountCrypto::new(
                &workspace, &workspace, true,
            ));
            crypto.unlock("prx_account")?;
            let protected: Arc<dyn Memory> = Arc::new(crate::memory::encrypted_scope::EncryptedScope::new(
                memory.clone(),
                crypto,
                "prx_account",
                "prx_scope",
            ));
            let protected_rec = recorder(protected, &workspace).await?;
            let refused = protected_rec.verify(&capsule, &legacy_event);
            anyhow::ensure!(
                refused
                    .as_ref()
                    .err()
                    .is_some_and(|e| e.to_string().contains("protected scope")),
                "protected scope accepted v1"
            );
        }
        let mut flipped = tag;
        *flipped.first_mut().context("empty MAC")? ^= 1;
        anyhow::ensure!(
            !rec.verify_slot(&capsule, RecoveryMacDomain::CapsuleV3, &[&bytes], &bytes, &flipped)?,
            "changed legacy tag accepted"
        );
        let mut flipped_bytes = bytes.clone();
        *flipped_bytes.first_mut().context("empty preimage")? ^= 1;
        anyhow::ensure!(
            !rec.verify_slot(
                &capsule,
                RecoveryMacDomain::CapsuleV3,
                &[&flipped_bytes],
                &flipped_bytes,
                &tag
            )?,
            "changed preimage accepted"
        );
        let repository_tag = memory.recovery_mac(None, RecoveryMacDomain::CapsuleV3, &bytes)?;
        anyhow::ensure!(
            !legacy.verify_hmac(KeyDomain::IngressRecovery, &[&bytes], &repository_tag.tag),
            "v3 accepted as legacy"
        );
        let reverse = RecoveryTag {
            key_id: repository_tag.key_id,
            tag,
        };
        anyhow::ensure!(
            memory
                .verify_recovery_mac(&reverse, RecoveryMacDomain::CapsuleV3, &bytes)
                .is_err(),
            "legacy accepted as v3"
        );
    }
    let old_id = original.key_id.clone().context("missing v3 key")?;
    let new_id = memory.rotate_recovery_authority()?;
    anyhow::ensure!(new_id != old_id, "rotation did not change id");
    rec.verify(&original, &event)?;
    let mut next = input(&workspace, "prx_next")?;
    next.idempotency_key = Some("prx_next_input".into());
    let next = recorder(memory.clone(), &workspace).await?.append(next).await?;
    anyhow::ensure!(
        capsule_from_event(&next)?.key_id.as_deref() == Some(new_id.as_str()),
        "same handle cached stale id"
    );
    for invalid in [
        "PRX-INVALID".to_owned(),
        "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA".to_owned(),
    ] {
        let mut capsule = original.clone();
        capsule.key_id = Some(invalid);
        anyhow::ensure!(capsule.validate_bounds().is_err(), "invalid id accepted");
    }
    drop(rec);
    drop(memory);
    let reopened = Arc::new(SqliteMemory::new(&workspace)?);
    let reopened_rec = recorder(reopened, &workspace).await?;
    reopened_rec.verify(&original, &event)?;
    let mut legacy_capsule = original.clone();
    legacy_capsule.version = 2;
    legacy_capsule.key_id = None;
    std::fs::remove_file(workspace.join("self/causal_tree/.decision_key"))?;
    let refused = reopened_rec.sign(&legacy_capsule);
    anyhow::ensure!(
        refused
            .as_ref()
            .err()
            .is_some_and(|e| e.is::<authority::LegacyUnconfirmed>()),
        "missing legacy key did not fail unconfirmed"
    );
    let marker = reopened_rec
        .record_quarantine(&legacy_capsule.scope, &legacy_capsule, "legacy_unconfirmed")
        .await;
    anyhow::ensure!(
        marker
            .as_ref()
            .err()
            .is_some_and(|e| e.is::<authority::LegacyUnconfirmed>()),
        "missing legacy authority wrote a marker"
    );
    anyhow::ensure!(
        reopened_rec
            .memory
            .find_owned_message_event_by_idempotency_key(
                &legacy_capsule.scope.workspace_id,
                &legacy_capsule.scope.owner_id,
                &IngressRecorder::quarantine_key(&legacy_capsule)
            )
            .await?
            .is_none(),
        "unconfirmed legacy marker exists"
    );
    anyhow::ensure!(
        !workspace.join("self/causal_tree/.decision_key").exists(),
        "legacy key regenerated"
    );
    Ok(())
}

#[tokio::test]
async fn recovery_wrapped_backends_keep_duplicate_suppression() -> anyhow::Result<()> {
    for backend in ["sqlite", "lucid", "encrypted", "postgres_class"] {
        let dir = directory()?;
        let workspace = dir.path().canonicalize()?;
        let memory = backend_memory(backend, &workspace)?;
        let mut source = input(&workspace, "prx_run")?;
        let event = recorder(memory.clone(), &workspace)
            .await?
            .append(source.clone())
            .await?;
        anyhow::ensure!(
            capsule_from_event(&event)?.version == if backend == "postgres_class" { 2 } else { 3 },
            "wrapper lost repository capability: {backend}"
        );
        source.run_id = Some("prx_retry".into());
        let retry = recorder(memory, &workspace).await?.append(source.clone()).await;
        anyhow::ensure!(
            retry.as_ref().err().is_some_and(|e| e.is::<AlreadyAccepted>()),
            "duplicate suppression failed: {backend}"
        );
        let reopened = backend_memory(backend, &workspace)?;
        let reopened_rec = recorder(reopened, &workspace).await?;
        reopened_rec.verify(&capsule_from_event(&event)?, &event)?;
        let retried = reopened_rec.append(source).await;
        anyhow::ensure!(
            retried.as_ref().err().is_some_and(|e| e.is::<AlreadyAccepted>()),
            "reopen lost duplicate suppression: {backend}"
        );
    }
    Ok(())
}

/// Durable ledger with no repository capability, as on PostgreSQL.
struct LegacyLedger(SqliteMemory);
#[async_trait::async_trait]
impl Memory for LegacyLedger {
    fn name(&self) -> &str {
        "postgres"
    }
    async fn store(
        &self,
        key: &str,
        content: &str,
        category: crate::memory::MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.0.store(key, content, category, session_id).await
    }
    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        self.0.recall(query, limit, session_id).await
    }
    async fn get(&self, key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
        self.0.get(key).await
    }
    async fn list(
        &self,
        category: Option<&crate::memory::MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        self.0.list(category, session_id).await
    }
    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        self.0.forget(key).await
    }
    async fn count(&self) -> anyhow::Result<usize> {
        self.0.count().await
    }
    async fn health_check(&self) -> bool {
        self.0.health_check().await
    }
    async fn acquire_ingress_lifetime(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Arc<dyn crate::memory::ingress_recovery::IngressLifetime>> {
        self.0.acquire_ingress_lifetime(workspace_id, token, deadline).await
    }
    async fn append_ingress_input(
        &self,
        input: MessageEventInput,
        capsule: crate::memory::ingress_recovery::IngressCapsule,
        lifetime: Arc<dyn crate::memory::ingress_recovery::IngressLifetime>,
        deadline: std::time::Instant,
        attestation: Option<crate::memory::ingress_recovery::IngressAttestation>,
    ) -> anyhow::Result<crate::memory::ingress_recovery::IngressAppendOutcome> {
        // Like PostgreSQL: no repository, so an attestation is refused, not dropped.
        crate::memory::ingress_recovery::refuse_attestation(attestation.as_ref())?;
        self.0
            .append_ingress_input(input, capsule, lifetime, deadline, attestation)
            .await
    }
    async fn list_ingress_recovery_candidates(
        &self,
        scope: &crate::memory::ingress_recovery::IngressRecoveryScope,
        current: Arc<dyn crate::memory::ingress_recovery::IngressLifetime>,
        phase: crate::memory::ingress_recovery::IngressRecoveryScan,
        after_id: i64,
        limit: usize,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<crate::memory::ingress_recovery::IngressRecoveryCandidate>> {
        self.0
            .list_ingress_recovery_candidates(scope, current, phase, after_id, limit, deadline)
            .await
    }
    async fn read_ingress_event(
        &self,
        scope: &crate::memory::ingress_recovery::IngressRecoveryScope,
        lookup: &crate::memory::ingress_recovery::IngressEventLookup,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.0.read_ingress_event(scope, lookup, deadline).await
    }
    async fn find_owned_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        owner_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.0
            .find_owned_message_event_by_idempotency_key(workspace_id, owner_id, idempotency_key)
            .await
    }
    async fn claim_ingress_recovery(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Option<crate::memory::ingress_recovery::IngressRecoveryClaim>> {
        self.0.claim_ingress_recovery(workspace_id, token, deadline).await
    }
    async fn commit_ingress_recovery(
        &self,
        commit: crate::memory::ingress_recovery::IngressRecoveryCommit,
    ) -> anyhow::Result<MessageEvent> {
        self.0.commit_ingress_recovery(commit).await
    }
}

fn backend_memory(backend: &str, workspace: &std::path::Path) -> anyhow::Result<Arc<dyn Memory>> {
    let local = SqliteMemory::new(workspace)?;
    let memory: Arc<dyn Memory> = match backend {
        "lucid" => Arc::new(crate::memory::lucid::LucidMemory::new(
            workspace,
            local,
            &crate::config::MemoryConfig::default(),
        )),
        "encrypted" => {
            let crypto = Arc::new(crate::security::ramflux_aead::AccountCrypto::new(
                workspace, workspace, true,
            ));
            crypto.unlock("prx_account")?;
            Arc::new(crate::memory::encrypted_scope::EncryptedScope::new(
                Arc::new(local),
                crypto,
                "prx_account",
                "prx_scope",
            ))
        }
        "postgres_class" => Arc::new(LegacyLedger(local)),
        _ => Arc::new(local),
    };
    Ok(memory)
}

/// How one production admission is shaped.
#[derive(Clone, Copy)]
struct Admission<'a> {
    key: &'a str,
    owner: &'a str,
    /// `causal_tree.enabled`; off means no recorder exists.
    plane: bool,
    /// The best-effort issuer path that gateway, scheduled, CLI and channel share.
    best_effort: bool,
}

const ADMISSION: Admission<'static> = Admission {
    key: "prx_stable_key",
    owner: "prx_user",
    plane: true,
    best_effort: false,
};

/// Production entry: fabric -> `with_ingress_recovery` -> `append`. The spy counts
/// what an entry point would go on to dispatch: any admission that did not fail.
async fn admit_as(
    memory: &Arc<dyn Memory>,
    workspace: &std::path::Path,
    content: &str,
    dispatched: &std::sync::atomic::AtomicUsize,
    how: Admission<'_>,
) -> (anyhow::Result<Option<MessageEvent>>, bool) {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_owned();
    config.causal_tree.enabled = how.plane;
    let config = Arc::new(config);
    let owner = Arc::new(super::super::turn_cleanup::CleanupTracker::default());
    let ticket = owner
        .register(tokio_util::sync::CancellationToken::new())
        .expect("ticket");
    let fabric = crate::memory::fabric::MemoryFabric::new(Arc::clone(memory), workspace.to_string_lossy())
        .with_event_recording(config.memory.event_recording_config())
        .with_ingress_recovery(&config, &ticket)
        .await
        .expect("fabric");
    let scope = crate::memory::fabric::MessageEventScope::new(
        crate::memory::MessageEventSource::Agent,
        MemoryVisibility::Session,
    )
    .with_channel("agent_api")
    .with_owner_id(how.owner)
    .with_session_key("prx_session")
    .with_run_id(uuid::Uuid::new_v4().to_string());
    let result = if how.best_effort {
        fabric
            .record_inbound_user_message_best_effort(scope, content, Some(how.key.to_owned()), "gateway")
            .await
    } else {
        fabric
            .record_inbound_user_message(scope, content, Some(how.key.to_owned()), None)
            .await
            .map(Some)
    };
    if result.is_ok() {
        dispatched.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    let duplicate = fabric.ingress_already_accepted();
    ticket.finish(true);
    (result, duplicate)
}

async fn admit(
    memory: &Arc<dyn Memory>,
    workspace: &std::path::Path,
    content: &str,
    dispatched: &std::sync::atomic::AtomicUsize,
) -> (anyhow::Result<MessageEvent>, bool) {
    let (result, duplicate) = admit_as(memory, workspace, content, dispatched, ADMISSION).await;
    (
        result.and_then(|event| event.context("admission was not recorded")),
        duplicate,
    )
}

fn db(workspace: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(workspace.join("memory").join("brain.db")).expect("brain.db")
}

fn table_count(writer: &rusqlite::Connection, table: &str) -> i64 {
    writer
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .expect("count")
}

fn assert_absent_everywhere(writer: &rusqlite::Connection, needles: &[Vec<u8>]) {
    let names: Vec<String> = writer
        .prepare("SELECT name FROM sqlite_schema WHERE type='table'")
        .expect("tables")
        .query_map([], |r| r.get(0))
        .expect("names")
        .collect::<rusqlite::Result<_>>()
        .expect("names");
    for table in names {
        let quoted = table.replace('"', "\"\"");
        let mut query = writer.prepare(&format!("SELECT * FROM \"{quoted}\"")).expect("scan");
        let columns = query.column_count();
        let mut rows = query.query([]).expect("rows");
        while let Some(row) = rows.next().expect("row") {
            for index in 0..columns {
                if let rusqlite::types::ValueRef::Text(bytes) | rusqlite::types::ValueRef::Blob(bytes) =
                    row.get_ref(index).expect("cell")
                {
                    for needle in needles {
                        assert!(
                            !bytes.windows(needle.len()).any(|window| window == needle.as_slice()),
                            "a transport digest survived in {table}"
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn recovery_duplicate_never_reexecutes_tools() -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = directory()?;
    let workspace = dir.path().canonicalize()?;
    let sqlite = SqliteMemory::new(&workspace)?;
    let origin = crate::memory::erasure::ErasureOrigin::new("prx_account", "prx_conversation", "prx_message");
    let view = sqlite.with_erasure_origin(origin.clone())?;
    let dispatched = AtomicUsize::new(0);
    let conn = db(&workspace);

    let (first, duplicate) = admit(&view, &workspace, "original content", &dispatched).await;
    first?;
    anyhow::ensure!(!duplicate && dispatched.load(Ordering::SeqCst) == 1, "first admission");

    // A matching retry (fresh run id, same stable content) is an AlreadyAccepted.
    let (retry, duplicate) = admit(&view, &workspace, "original content", &dispatched).await;
    anyhow::ensure!(
        retry.as_ref().err().is_some_and(|e| e.is::<AlreadyAccepted>()) && duplicate,
        "matching retry must be AlreadyAccepted"
    );
    // A conflicting retry is a typed IngressConflict.
    let (conflict, duplicate) = admit(&view, &workspace, "different content", &dispatched).await;
    anyhow::ensure!(
        conflict.as_ref().err().is_some_and(|e| e.is::<IngressConflict>()) && duplicate,
        "conflicting retry must be IngressConflict"
    );
    anyhow::ensure!(dispatched.load(Ordering::SeqCst) == 1, "a retry dispatched");
    anyhow::ensure!(table_count(&conn, "recovery_transport_index") == 1, "index row missing");

    let (transport, key_id): (Vec<u8>, String) = conn.query_row(
        "SELECT transport_digest,key_id FROM recovery_transport_index",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    anyhow::ensure!(
        table_count(&conn, "recovery_tombstones") == 0,
        "tombstone before erasure"
    );

    // Production erasure of the message that carried the input.
    let store = sqlite.erasure_store()?;
    store.begin_delete(&origin)?;
    store.erase_rows(&origin)?;
    anyhow::ensure!(table_count(&conn, "message_events") == 0, "input survived erasure");
    anyhow::ensure!(
        table_count(&conn, "recovery_transport_index") == 0,
        "index row survived"
    );
    anyhow::ensure!(table_count(&conn, "recovery_attestations") == 0, "attestation survived");
    anyhow::ensure!(table_count(&conn, "recovery_tombstones") == 1, "tombstone missing");
    let (tombstone_key,): (String,) =
        conn.query_row("SELECT key_id FROM recovery_tombstones", [], |r| Ok((r.get(0)?,)))?;
    anyhow::ensure!(tombstone_key == key_id, "tombstone key id");
    assert_absent_everywhere(&conn, &[transport.clone(), hex::encode(&transport).into_bytes()]);

    // Even different content is the spent key: AlreadyAccepted, nothing executes.
    for content in ["original content", "entirely different content"] {
        let (after, duplicate) = admit(&view, &workspace, content, &dispatched).await;
        anyhow::ensure!(
            after.as_ref().err().is_some_and(|e| e.is::<AlreadyAccepted>()) && duplicate,
            "tombstoned retry must be AlreadyAccepted"
        );
    }
    anyhow::ensure!(table_count(&conn, "message_events") == 0, "a tombstoned retry inserted");

    // The tombstone was written under a key that is no longer active.
    let rotated = sqlite.rotate_recovery_authority()?;
    anyhow::ensure!(rotated != key_id, "rotation kept the key");
    let (after, duplicate) = admit(&view, &workspace, "rotated content", &dispatched).await;
    anyhow::ensure!(
        after.as_ref().err().is_some_and(|e| e.is::<AlreadyAccepted>()) && duplicate,
        "rotated-out tombstone must still match"
    );
    anyhow::ensure!(dispatched.load(Ordering::SeqCst) == 1, "a retry dispatched");

    // A PostgreSQL-class backend refuses an attestation instead of dropping it.
    let ledger = LegacyLedger(SqliteMemory::new(&workspace)?);
    let lifetime = ledger
        .acquire_ingress_lifetime(
            &workspace.to_string_lossy(),
            &uuid::Uuid::new_v4().to_string(),
            Instant::now() + Duration::from_secs(30),
        )
        .await?;
    let capsule = IngressCapsule {
        version: 3,
        key_id: Some(rotated.clone()),
        backend_namespace: lifetime.namespace().to_owned(),
        scope: IngressRecoveryScope {
            workspace_id: workspace.to_string_lossy().into_owned(),
            owner_id: "prx_user".into(),
            source: crate::memory::MessageEventSource::Agent,
        },
        lifetime: lifetime.token().to_owned(),
        input_event_id: "prx_event".into(),
        input_key: "prx_stable_key".into(),
        run_id: "prx_run".into(),
        input_digest: "0".repeat(64),
        authentication: "0".repeat(64),
    };
    let tag = RecoveryTag {
        key_id: rotated,
        tag: [0; 32],
    };
    let refused = ledger
        .append_ingress_input(
            input(&workspace, "prx_run")?,
            capsule,
            lifetime,
            Instant::now() + Duration::from_secs(30),
            Some(crate::memory::ingress_recovery::IngressAttestation::new(
                Vec::new(),
                tag.clone(),
                tag,
            )),
        )
        .await
        .err()
        .context("attestation was accepted")?;
    anyhow::ensure!(
        refused.downcast_ref::<crate::memory::ingress_recovery::IngressAttestationError>()
            == Some(&crate::memory::ingress_recovery::IngressAttestationError::Unsupported),
        "untyped refusal: {refused:#}"
    );
    Ok(())
}

fn keys_dir(workspace: &std::path::Path) -> std::path::PathBuf {
    workspace.join("memory").join(".recovery_keys")
}

/// Move every recovery key aside (K1 lost).
fn stash_keys(workspace: &std::path::Path) -> anyhow::Result<()> {
    let stash = workspace.join("stashed-keys");
    std::fs::create_dir_all(&stash)?;
    for entry in std::fs::read_dir(keys_dir(workspace))? {
        let entry = entry?;
        std::fs::rename(entry.path(), stash.join(entry.file_name()))?;
    }
    Ok(())
}

/// Put the stashed keys back byte-for-byte (same inode, mode and mtime).
fn restore_keys(workspace: &std::path::Path) -> anyhow::Result<()> {
    let stash = workspace.join("stashed-keys");
    for entry in std::fs::read_dir(&stash)? {
        let entry = entry?;
        std::fs::rename(entry.path(), keys_dir(workspace).join(entry.file_name()))?;
    }
    Ok(())
}

fn origin(message: &str) -> crate::memory::erasure::ErasureOrigin {
    crate::memory::erasure::ErasureOrigin::new("prx_account", "prx_conversation", message)
}

/// Production erasure of every row written through a view carrying `origin`.
fn erase_origin(memory: &Arc<dyn Memory>, origin: &crate::memory::erasure::ErasureOrigin) -> anyhow::Result<()> {
    let store = memory.erasure_store()?;
    store.begin_delete(origin)?;
    store.erase_rows(origin)?;
    Ok(())
}

fn is_unavailable(result: &anyhow::Result<Option<MessageEvent>>) -> bool {
    result
        .as_ref()
        .err()
        .is_some_and(|e| e.is::<crate::memory::ingress_recovery::StableKeyFenceUnavailable>())
}

fn is_already_accepted(result: &anyhow::Result<Option<MessageEvent>>) -> bool {
    result.as_ref().err().is_some_and(|e| e.is::<AlreadyAccepted>())
}

#[tokio::test]
async fn recovery_missing_key_with_attestation_never_readmits() -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = directory()?;
    let workspace = dir.path().canonicalize()?;
    let dispatched = AtomicUsize::new(0);
    let accepted = |key| Admission { key, ..ADMISSION };
    let base: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace)?);
    for key in ["prx_key_a", "prx_key_b"] {
        let view = base.with_erasure_origin(origin(key))?;
        let (first, _) = admit_as(&view, &workspace, "original", &dispatched, accepted(key)).await;
        first?;
    }
    anyhow::ensure!(dispatched.load(Ordering::SeqCst) == 2, "first admissions");
    anyhow::ensure!(
        table_count(&db(&workspace), "recovery_attestations") == 2,
        "inputs were not attested"
    );
    let rows = table_count(&db(&workspace), "message_events");

    // (i) K1 is lost and the process restarts: the live row still refuses the key.
    stash_keys(&workspace)?;
    let restarted: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace)?);
    let (retry, duplicate) = admit_as(&restarted, &workspace, "changed", &dispatched, accepted("prx_key_a")).await;
    anyhow::ensure!(is_already_accepted(&retry) && duplicate, "same-key retry: {retry:?}");
    anyhow::ensure!(dispatched.load(Ordering::SeqCst) == 2, "a retry dispatched");
    anyhow::ensure!(
        table_count(&db(&workspace), "message_events") == rows,
        "row count changed"
    );

    // The scope now also holds a tombstone under K1.
    restore_keys(&workspace)?;
    erase_origin(&base, &origin("prx_key_b"))?;
    anyhow::ensure!(
        table_count(&db(&workspace), "recovery_tombstones") == 1,
        "tombstone missing"
    );
    let rows = table_count(&db(&workspace), "message_events");

    // (ii) A new key in that scope is refused while K1 is lost.
    stash_keys(&workspace)?;
    let restarted: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace)?);
    let (fresh, _) = admit_as(&restarted, &workspace, "new", &dispatched, accepted("prx_key_c")).await;
    anyhow::ensure!(is_unavailable(&fresh), "a new key was not fenced: {fresh:?}");
    anyhow::ensure!(dispatched.load(Ordering::SeqCst) == 2, "a fenced input dispatched");
    anyhow::ensure!(
        table_count(&db(&workspace), "message_events") == rows,
        "a fenced input inserted"
    );
    // Another owner's scope has no tombstone: the blast radius is bounded.
    let other = Admission {
        owner: "prx_other",
        ..accepted("prx_key_c")
    };
    let (admitted, _) = admit_as(&restarted, &workspace, "new", &dispatched, other).await;
    anyhow::ensure!(admitted.is_ok(), "another owner's scope was refused: {admitted:?}");

    // (iii) The erased key under another owner is a different scope: admitted.
    let (admitted, _) = admit_as(
        &restarted,
        &workspace,
        "original",
        &dispatched,
        Admission {
            owner: "prx_other",
            ..accepted("prx_key_b")
        },
    )
    .await;
    anyhow::ensure!(admitted.is_ok(), "the erased key under another owner: {admitted:?}");
    anyhow::ensure!(dispatched.load(Ordering::SeqCst) == 4, "dispatch count");
    Ok(())
}

#[tokio::test]
async fn recovery_missing_key_after_erasure_never_readmits() -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for backend in ["sqlite", "encrypted"] {
        let dir = directory()?;
        let workspace = dir.path().canonicalize()?;
        let dispatched = AtomicUsize::new(0);
        let base = backend_memory(backend, &workspace)?;
        let first = base.with_erasure_origin(origin("prx_message"))?;
        let (accepted, _) = admit_as(&first, &workspace, "original", &dispatched, ADMISSION).await;
        accepted?;
        // Production erasure of the message that carried the input.
        erase_origin(&base, &origin("prx_message"))?;
        anyhow::ensure!(
            table_count(&db(&workspace), "recovery_tombstones") == 1,
            "{backend}: tombstone"
        );
        anyhow::ensure!(
            table_count(&db(&workspace), "message_events") == 0,
            "{backend}: row survived"
        );
        drop((first, base));

        // K1 is lost and the process restarts (no recorder): different content is fenced.
        stash_keys(&workspace)?;
        let restarted = backend_memory(backend, &workspace)?;
        let (fenced, _) = admit_as(&restarted, &workspace, "different", &dispatched, ADMISSION).await;
        anyhow::ensure!(
            is_unavailable(&fenced),
            "{backend}: missing K1 was not fenced: {fenced:?}"
        );
        anyhow::ensure!(dispatched.load(Ordering::SeqCst) == 1, "{backend}: dispatch");
        anyhow::ensure!(
            table_count(&db(&workspace), "message_events") == 0,
            "{backend}: fresh row"
        );
        drop(restarted);

        // K1 is back, the plane is off (plain path, no recorder).
        restore_keys(&workspace)?;
        let plain = backend_memory(backend, &workspace)?;
        let off = Admission {
            plane: false,
            ..ADMISSION
        };
        let (retry, duplicate) = admit_as(&plain, &workspace, "different", &dispatched, off).await;
        anyhow::ensure!(
            is_already_accepted(&retry) && duplicate,
            "{backend}: plain retry: {retry:?}"
        );
        anyhow::ensure!(dispatched.load(Ordering::SeqCst) == 1, "{backend}: dispatch");
        anyhow::ensure!(table_count(&db(&workspace), "message_events") == 0, "{backend}: count");

        if backend == "sqlite" {
            // A batch refuses the tombstoned input and keeps its other items.
            let mut spent = input(&workspace, "prx_batch")?;
            spent.idempotency_key = Some(ADMISSION.key.to_owned());
            let mut unrelated = input(&workspace, "prx_batch")?;
            unrelated.role = "assistant".to_owned();
            unrelated.idempotency_key = Some("prx_unrelated".to_owned());
            let results = plain.append_message_events(vec![spent, unrelated]).await;
            anyhow::ensure!(
                results
                    .first()
                    .and_then(|r| r.as_ref().err())
                    .is_some_and(|e| e.is::<crate::memory::ingress_recovery::StableKeyTombstoned>()),
                "batch item 0 was not fenced"
            );
            anyhow::ensure!(results.get(1).is_some_and(Result::is_ok), "batch item 1 failed");
            anyhow::ensure!(table_count(&db(&workspace), "message_events") == 1, "batch rows");
        }
    }
    Ok(())
}

#[tokio::test]
async fn recovery_plane_off_tombstone_fence() -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = directory()?;
    let workspace = dir.path().canonicalize()?;
    let dispatched = AtomicUsize::new(0);
    let base: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace)?);
    let view = base.with_erasure_origin(origin("prx_message"))?;
    let (first, _) = admit_as(&view, &workspace, "original", &dispatched, ADMISSION).await;
    first?;
    erase_origin(&base, &origin("prx_message"))?;
    anyhow::ensure!(table_count(&db(&workspace), "recovery_tombstones") == 1, "tombstone");
    drop((view, base));

    let snapshot = |workspace: &std::path::Path| -> anyhow::Result<Vec<(String, Vec<u8>, std::time::SystemTime)>> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(keys_dir(workspace))? {
            let entry = entry?;
            files.push((
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path())?,
                entry.metadata()?.modified()?,
            ));
        }
        files.sort();
        Ok(files)
    };
    let before = snapshot(&workspace)?;

    // (i) Keys present, plane off: the erased key is still spent, through the
    // best-effort path that every issuer shares.
    let off = Admission {
        plane: false,
        best_effort: true,
        ..ADMISSION
    };
    let restarted: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace)?);
    for content in ["original", "different"] {
        let (retry, duplicate) = admit_as(&restarted, &workspace, content, &dispatched, off).await;
        anyhow::ensure!(is_already_accepted(&retry) && duplicate, "plane-off retry: {retry:?}");
    }
    anyhow::ensure!(dispatched.load(Ordering::SeqCst) == 1, "a retry dispatched");
    anyhow::ensure!(snapshot(&workspace)? == before, "the fence touched the recovery keys");
    let decisions = workspace.join("self/causal_tree");
    anyhow::ensure!(
        !decisions.join(".decision_key").exists() && !decisions.join("decisions.db").exists(),
        "the fence created decision state"
    );

    // (ii) K1 is lost: a typed refusal, never an unrecorded receipt.
    drop(restarted);
    stash_keys(&workspace)?;
    let restarted: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace)?);
    let (refused, _) = admit_as(&restarted, &workspace, "different", &dispatched, off).await;
    anyhow::ensure!(is_unavailable(&refused), "missing K1 plane-off: {refused:?}");
    anyhow::ensure!(dispatched.load(Ordering::SeqCst) == 1, "a fenced input dispatched");
    anyhow::ensure!(std::fs::read_dir(keys_dir(&workspace))?.count() == 0, "key dir changed");

    // (iii) A database that never activated recovery: admitted, nothing created.
    let fresh_dir = directory()?;
    let fresh = fresh_dir.path().canonicalize()?;
    let never: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&fresh)?);
    let (admitted, _) = admit_as(&never, &fresh, "new", &dispatched, off).await;
    anyhow::ensure!(
        admitted.is_ok_and(|event| event.is_some()),
        "a never-activated store refused"
    );
    let tables: i64 = db(&fresh).query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name='recovery_tombstones'",
        [],
        |r| r.get(0),
    )?;
    anyhow::ensure!(tables == 0, "the fence created the tombstone table");
    anyhow::ensure!(!keys_dir(&fresh).exists(), "the fence created .recovery_keys");
    Ok(())
}

#[tokio::test]
async fn recovery_v3_missing_attestation_is_refused() -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = directory()?;
    let workspace = dir.path().canonicalize()?;
    let dispatched = AtomicUsize::new(0);
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace)?);
    let (first, _) = admit_as(&memory, &workspace, "original", &dispatched, ADMISSION).await;
    first?;
    anyhow::ensure!(
        table_count(&db(&workspace), "recovery_attestations") == 1,
        "attestation"
    );
    let conn = db(&workspace);
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    conn.execute("DELETE FROM recovery_attestations", [])?;
    anyhow::ensure!(
        table_count(&conn, "recovery_tombstones") == 1,
        "the trigger wrote no tombstone"
    );
    conn.execute("DELETE FROM recovery_tombstones", [])?;
    anyhow::ensure!(table_count(&conn, "message_events") == 1, "the accepted row is gone");

    let (retry, duplicate) = admit_as(&memory, &workspace, "original", &dispatched, ADMISSION).await;
    let missing = retry.as_ref().err().and_then(|e| {
        e.chain()
            .find_map(|cause| cause.downcast_ref::<crate::memory::ingress_recovery::IngressAttestationError>())
    });
    anyhow::ensure!(
        missing == Some(&crate::memory::ingress_recovery::IngressAttestationError::Missing),
        "retry without an attestation: {retry:?}"
    );
    anyhow::ensure!(duplicate, "the duplicate flag was not set");
    anyhow::ensure!(dispatched.load(Ordering::SeqCst) == 1, "a retry dispatched");
    Ok(())
}

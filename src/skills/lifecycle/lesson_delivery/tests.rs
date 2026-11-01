//! Backend integrity/retention tests; fixture pins are not adoption authority.
use super::*;
use crate::skills::lifecycle::RevisionId;
use std::sync::Arc;
use std::time::Duration;

fn payload(store: &SkillLifecycleStore, key: &DecisionKey, attempt: &str) -> Result<SealedDelivery> {
    let revision = RevisionId::parse(&format!("r1-{}", "a".repeat(64)))?;
    Ok(SealedDelivery {
        payload: Payload {
            schema_version: 1,
            delivery_id: delivery_id(key, store.owner_scope(), "fixture-turn", attempt),
            owner_scope: store.owner_scope().clone(),
            skill_id: SkillId::parse("fixture-skill")?,
            state: DeliveryState::ProvenUnsent,
            turn_id: "fixture-turn".into(),
            attempt_id: attempt.into(),
            physical_binding_sha256: "b".repeat(64),
            snapshot_sha256: "c".repeat(64),
            block_sha256: "d".repeat(64),
            final_context_sha256: "e".repeat(64),
            body_binding_sha256: "f".repeat(64),
            experiment: ExperimentRunBinding {
                experiment_sha256: "1".repeat(64),
                stage: "fixture-editor".into(),
                input_sha256: "2".repeat(64),
            },
            model: ModelBinding {
                provider_label: "fixture-provider".into(),
                model: "fixture-model".into(),
                temperature_milli: 100,
            },
            refs: vec![LessonRef {
                adoption_id: "fixture-adoption".into(),
                adoption_sha256: "3".repeat(64),
                publication_tx: "fixture-publication".into(),
                adopted_revision: revision,
                adopted_record_sha256: "4".repeat(64),
            }],
            created_at: Utc::now(),
        },
        seal: String::new(),
    })
}

fn limits(records: usize, bytes: u64) -> DeliveryRetentionLimits {
    DeliveryRetentionLimits {
        max_records: records,
        max_decoded_bytes: bytes,
        deadline: Instant::now() + Duration::from_secs(5),
    }
}

#[test]
fn delivery_states_and_seals_keep_distinct_attempts_and_retention_refs() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let store = SkillLifecycleStore::open(workspace.path(), OwnerScope::new("fixture-owner")?)?;
    let key = DecisionKey::load_or_create(workspace.path())?;
    let mut first = payload(&store, &key, "fixture-attempt-1")?;
    let mut second = payload(&store, &key, "fixture-attempt-2")?;
    assert_ne!(first.payload.delivery_id, second.payload.delivery_id);
    let dir = store.root().join(DIRECTORY);
    let first_path = dir.join(format!("{}.json", first.payload.delivery_id));
    let second_path = dir.join(format!("{}.json", second.payload.delivery_id));
    {
        let _store = store.lock_store(true, Duration::from_secs(1))?;
        let _skill = store.lock_skill(&first.payload.skill_id, Duration::from_secs(1))?;
        ensure_dir_synced(store.root(), &dir)?;
        let skill = first.payload.skill_id.clone();
        write_confirmed(&store, &skill, &first_path, &mut first, &key)?;
        let saved: SealedDelivery = serde_json::from_slice(&read_bytes(&first_path, MAX_RECORD_BYTES)?)?;
        assert!(matches!(saved.payload.state, DeliveryState::ProvenUnsent));
        first.payload.state = DeliveryState::PossibleHandoffUnknown;
        write_confirmed(&store, &skill, &first_path, &mut first, &key)?;
        write_confirmed(&store, &skill, &second_path, &mut second, &key)?;
    }
    let exclusive = store.lock_store(false, Duration::from_secs(1))?;
    let refs = store.delivery_retention_refs_locked(&exclusive, limits(2, 1024 * 1024))?;
    assert_eq!(refs.examined_records, 2);
    assert_eq!(refs.refs, first.payload.refs);
    assert_eq!(
        refs.decoded_bytes,
        fs::metadata(&first_path)?.len() + fs::metadata(&second_path)?.len()
    );
    assert!(
        store
            .delivery_retention_refs_locked(&exclusive, limits(1, 1024 * 1024))
            .is_err()
    );
    assert!(store.delivery_retention_refs_locked(&exclusive, limits(2, 1)).is_err());
    let expired = DeliveryRetentionLimits {
        deadline: Instant::now(),
        ..limits(2, 1024 * 1024)
    };
    assert!(store.delivery_retention_refs_locked(&exclusive, expired).is_err());
    Ok(())
}

#[test]
fn tampered_or_corrupt_delivery_aborts_the_whole_retention_read() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let store = SkillLifecycleStore::open(workspace.path(), OwnerScope::new("fixture-owner")?)?;
    let key = DecisionKey::load_or_create(workspace.path())?;
    let mut record = payload(&store, &key, "fixture-attempt")?;
    let dir = store.root().join(DIRECTORY);
    let path = dir.join(format!("{}.json", record.payload.delivery_id));
    {
        let _store = store.lock_store(true, Duration::from_secs(1))?;
        let _skill = store.lock_skill(&record.payload.skill_id, Duration::from_secs(1))?;
        ensure_dir_synced(store.root(), &dir)?;
        let skill = record.payload.skill_id.clone();
        write_confirmed(&store, &skill, &path, &mut record, &key)?;
        // Modify a bound field without its key-authenticated seal.
        record.payload.body_binding_sha256 = "0".repeat(64);
        store.write_atomic(&skill, &path, &record_bytes(&record, &path)?)?;
    }
    {
        let exclusive = store.lock_store(false, Duration::from_secs(1))?;
        assert!(
            store
                .delivery_retention_refs_locked(&exclusive, limits(1, 1024 * 1024))
                .is_err()
        );
    }
    {
        let _store = store.lock_store(true, Duration::from_secs(1))?;
        let _skill = store.lock_skill(&record.payload.skill_id, Duration::from_secs(1))?;
        store.write_atomic(&record.payload.skill_id, &path, b"corrupt")?;
    }
    let exclusive = store.lock_store(false, Duration::from_secs(1))?;
    assert!(
        store
            .delivery_retention_refs_locked(&exclusive, limits(1, 1024 * 1024))
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn advancing_the_manual_clock_expires_the_retention_deadline() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let store = SkillLifecycleStore::open(workspace.path(), OwnerScope::new("fixture-owner")?)?;
    let clock = crate::causal_tree::logical_clock::ManualClock::new();
    crate::causal_tree::logical_clock::scope(Arc::clone(&clock), async move {
        let exclusive = store.lock_store(false, Duration::from_secs(1))?;
        let deadline = crate::causal_tree::logical_clock::now() + Duration::from_millis(500);
        let open = DeliveryRetentionLimits {
            max_records: 1,
            max_decoded_bytes: 1024,
            deadline,
        };
        store.delivery_retention_refs_locked(&exclusive, open)?;
        clock.advance(Duration::from_millis(501));
        let expired = DeliveryRetentionLimits {
            max_records: 1,
            max_decoded_bytes: 1024,
            deadline,
        };
        let Err(error) = store.delivery_retention_refs_locked(&exclusive, expired) else {
            panic!("test: logical deadline");
        };
        assert!(
            error.to_string().contains("delivery retention work deadline exhausted"),
            "test: refusal must name the retention deadline, got {error}"
        );
        Ok(())
    })
    .await
}

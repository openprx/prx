//! Default/negative contracts use no fabricated adoption or prepared authority.
use super::*;
use crate::skills::lifecycle::adoption::{LessonCompatibility, LessonLimits, LessonQuery};
use sha2::{Digest, Sha256};

#[test]
fn lesson_none_preserves_pre_extension_operation_bytes_and_hash() -> Result<()> {
    let operation = ExperimentOperation::DevelopmentProposal {
        role: DevelopmentRole::General,
        patterns: vec![],
        probe_stages: vec!["baseline".into()],
        parent_selection: None,
        lesson_context_sha256: None,
        max_input_bytes: 65536,
        max_output_bytes: 16384,
        timeout_ms: 120000,
    };
    let legacy = br#"{"operation":"development_proposal","role":"general","patterns":[],"probe_stages":["baseline"],"max_input_bytes":65536,"max_output_bytes":16384,"timeout_ms":120000}"#;
    assert_eq!(serde_json::to_vec(&operation)?, legacy);
    let mut hash = Sha256::new();
    hash.update(b"prx-instruction-experiment-input-v1\0");
    hash.update(legacy);
    assert_eq!(operation.digest()?, hex::encode(hash.finalize()));
    let spec = test_specification();
    assert!(serde_json::to_value(&spec.pins)?.get("lesson_snapshot").is_none());
    let mut json = serde_json::to_value(&operation)?;
    json["lesson_context_sha256"] = serde_json::Value::Null;
    let explicit: ExperimentOperation = serde_json::from_value(json)?;
    assert_eq!(explicit, operation);
    Ok(())
}

#[tokio::test]
async fn no_snapshot_exits_before_runtime_profile_operation_or_store_io() -> Result<()> {
    let f = fixture();
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let service = service(&f, &suite(&f));
        let plan = test_specification().freeze()?;
        let root = f.store.root().to_path_buf();
        let detached = root.with_file_name("fixture-detached-store");
        std::fs::rename(&root, &detached)?;
        let result = service
            .prepare_lesson_context(
                &plan,
                "not-an-admitted-stage",
                &ExperimentOperation::Verify {
                    evaluation_stage: "missing".into(),
                },
                "non-ASCII fixture: \u{00e9}",
            )
            .await;
        let absent = !root.exists();
        std::fs::rename(&detached, &root)?;
        assert!(result?.is_none());
        assert!(absent, "None must not recreate the absent store or inspect a profile");
        assert_eq!(server.count.load(Ordering::SeqCst), 0);
        assert!(!config.workspace_dir.join("state/costs.reservations.jsonl").exists());
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[test]
fn root_identity_ignores_ids_splits_paths_but_not_actual_task_content() -> Result<()> {
    let f = fixture();
    let original = suite(&f);
    let mut renamed = original.suite().clone();
    for (number, case) in renamed.cases.iter_mut().enumerate() {
        case.case_id = format!("renamed-{number}");
        case.split = match case.split {
            EvalSplit::Development => EvalSplit::Validation,
            EvalSplit::Validation => EvalSplit::Holdout,
            EvalSplit::Holdout => EvalSplit::Development,
        };
    }
    assert_eq!(original.lesson_root_digests(), renamed.freeze()?.lesson_root_digests());
    let mut source = original.suite().clone();
    for case in &mut source.cases {
        case.fixtures = vec![crate::skills::lifecycle::FixtureFile {
            path: "fixture.txt".into(),
            content: String::new(),
        }];
    }
    let frozen = source.clone().freeze()?;
    for case in &mut source.cases {
        case.fixtures[0].path = "renamed.txt".into();
    }
    assert_eq!(
        frozen.lesson_root_digests(),
        source.clone().freeze()?.lesson_root_digests()
    );
    for case in &mut source.cases {
        case.prompt = format!("A different release-preflight task: {}", case.prompt);
    }
    let fresh = source.freeze()?.lesson_root_digests();
    assert!(
        frozen.lesson_root_digests().iter().all(|root| !fresh.contains(root)),
        "empty/common fixture bodies must not create a root shared by different real tasks"
    );
    Ok(())
}

#[test]
fn readonly_cost_snapshot_does_not_create_or_migrate_old_storage() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let scopes = ["a".repeat(64)];
    assert!(crate::cost::tracker::physical_scopes_read_only(temp.path(), &scopes)?.is_none());
    assert!(!temp.path().join("state").exists());
    let legacy = temp.path().join(".openprx");
    std::fs::create_dir(&legacy)?;
    let bytes = b"legacy evidence is opaque and must remain untouched";
    std::fs::write(legacy.join("costs.db"), bytes)?;
    assert!(crate::cost::tracker::physical_scopes_read_only(temp.path(), &scopes)?.is_none());
    assert_eq!(std::fs::read(legacy.join("costs.db"))?, bytes);
    assert!(!temp.path().join("state").exists());
    assert!(
        crate::cost::tracker::physical_scopes_read_only(temp.path(), &[scopes[0].clone(), scopes[0].clone()]).is_err()
    );
    assert!(crate::cost::tracker::physical_scopes_read_only(temp.path(), &["invalid".into()]).is_err());
    Ok(())
}

#[test]
fn empty_query_rejects_owner_and_caps_without_inventing_eligible_lessons() -> Result<()> {
    let f = fixture();
    let now = Utc::now();
    let query = LessonQuery {
        owner_scope: f.store.owner_scope().clone(),
        task_family: "deploy".into(),
        skill_id: f.pin.skill_id.clone(),
        baseline_revision: f.pin.revision_id.clone(),
        baseline_record_sha256: f.pin.record_digest.clone(),
        compatibility: LessonCompatibility {
            model_sha256: "a".repeat(64),
            policy_sha256: "b".repeat(64),
            source_profile_sha256: "c".repeat(64),
            tokenizer_profile_sha256: "d".repeat(64),
        },
        excluded_roots: suite(&f).lesson_root_digests(),
        limits: LessonLimits::default(),
        planned_at: now,
        expires_at: now + chrono::Duration::minutes(5),
    };
    let snapshot = f.store.query_adopted_lessons(&query)?;
    assert!(snapshot.entries.is_empty());
    assert!(crate::skills::lifecycle::adoption::format_lesson_block(&snapshot, "general-proposal")?.is_empty());
    let mut foreign = query.clone();
    foreign.owner_scope = crate::skills::lifecycle::OwnerScope::new("fixture-foreign-owner")?;
    assert!(f.store.query_adopted_lessons(&foreign).is_err());
    let mut oversized = query.clone();
    oversized.limits.examined += 1;
    assert!(f.store.query_adopted_lessons(&oversized).is_err());
    let mut expired = query;
    expired.expires_at = expired.planned_at;
    assert!(f.store.query_adopted_lessons(&expired).is_err());
    assert!(
        !f.store.root().join("adoptions").exists(),
        "read-only empty query must not create a library"
    );
    Ok(())
}

#[tokio::test]
async fn verified_fixture_profile_is_exact_owner_model_endpoint_and_guard_scoped() -> Result<()> {
    let f = fixture();
    let other = fixture();
    let server = Server::start(false).await?;
    let result = async {
        let binding = ModelBinding {
            provider_label: format!("custom:{}", server.url),
            model: "experiment-loopback".into(),
            temperature_milli: 100,
        };
        let policy = "a".repeat(64);
        let source = "b".repeat(64);
        assert!(
            f.store
                .lesson_profile_compatibility(&binding, &policy, &source)
                .is_err()
        );
        let guard = f.store.register_lesson_fixture_profile(&binding)?;
        let expected = f.store.lesson_profile_compatibility(&binding, &policy, &source)?;
        assert_eq!(expected.model_sha256, binding.digest());
        assert!(
            other
                .store
                .lesson_profile_compatibility(&binding, &policy, &source)
                .is_err()
        );
        let mut wrong = binding.clone();
        wrong.model = "unregistered-model".into();
        assert!(f.store.lesson_profile_compatibility(&wrong, &policy, &source).is_err());
        let mut sampling = binding.clone();
        sampling.temperature_milli = 0;
        assert!(
            f.store
                .lesson_profile_compatibility(&sampling, &policy, &source)
                .is_err()
        );
        let mut endpoint = binding.clone();
        endpoint.provider_label = "custom:http://127.0.0.1:1/v1".into();
        assert!(
            f.store
                .lesson_profile_compatibility(&endpoint, &policy, &source)
                .is_err()
        );
        drop(guard);
        assert!(
            f.store
                .lesson_profile_compatibility(&binding, &policy, &source)
                .is_err()
        );
        assert_eq!(server.count.load(Ordering::SeqCst), 0);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

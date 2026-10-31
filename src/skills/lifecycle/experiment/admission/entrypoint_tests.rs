//! The original owner authenticates envelopes; data never installs live scope.
use super::*;
#[tokio::test]
async fn original_owner_rejects_stripped_changed_and_copied_controlled_admission() {
    crate::runtime::h2_entry::test_recorded_terminal_scope(|config| async move {
        let owner = config.skills_lifecycle.owner().unwrap().unwrap();
        let store = SkillLifecycleStore::open_existing(&config.workspace_dir, owner.clone())
            .unwrap()
            .unwrap();
        let mut spec = super::super::tests::specification();
        spec.owner_scope = owner;
        spec.created_at = chrono::Utc::now();
        spec.expires_at = spec.created_at + chrono::Duration::seconds(60);
        let plan = store.freeze_experiment_admission(spec, &config).unwrap();
        assert!(plan.spec().id.as_str().starts_with("h2c-"));
        store.verify_experiment_admission(&plan).unwrap();
        let envelope = plan.spec().controlled_admission.as_ref().unwrap();
        let current = crate::runtime::h2_entry::current_run_continuation().unwrap();
        let bound = current.for_admitted_plan(&plan, &envelope.pin, &config).unwrap();
        // An authentic MAC alone cannot reserve a copied root without live scope.
        crate::runtime::h2_entry::retain_run_continuation(None, async {
            assert!(store.admit_experiment(&plan, chrono::Utc::now()).is_err());
            assert!(
                !store.root().join("experiments").exists(),
                "missing live source cannot create an experiment directory"
            );
        })
        .await;
        bound
            .sync_scope(|| store.admit_experiment(&plan, chrono::Utc::now()))
            .unwrap();
        let mutations: [fn(&mut ExperimentSpec); 4] = [
            |s: &mut ExperimentSpec| s.controlled_admission = None,
            |s: &mut ExperimentSpec| s.controlled_admission.as_mut().unwrap().owner_mac = "0".repeat(64),
            |s: &mut ExperimentSpec| s.controlled_admission.as_mut().unwrap().pin.source.boot = "other-boot".into(),
            |s: &mut ExperimentSpec| s.id = ExperimentId::parse("legacy-copied-controlled").unwrap(),
        ];
        for mutate in mutations {
            let mut changed = plan.spec().clone();
            mutate(&mut changed);
            let changed = changed.freeze().unwrap();
            assert!(store.verify_experiment_admission(&changed).is_err());
        }
        let mut old_boot = envelope.pin.clone();
        old_boot.source.boot = "old-boot".into();
        assert!(current.for_admitted_plan(&plan, &old_boot, &config).is_err());
    })
    .await;
}

#[tokio::test]
async fn invalid_owner_mac_and_operation_map_never_consume_declared_root_ceilings() {
    crate::runtime::h2_entry::test_recorded_terminal_scope(|config| async move {
        let owner = config.skills_lifecycle.owner().unwrap().unwrap();
        let store = SkillLifecycleStore::open_existing(&config.workspace_dir, owner.clone())
            .unwrap()
            .unwrap();
        let mut spec = super::super::tests::specification();
        spec.owner_scope = owner;
        spec.created_at = chrono::Utc::now();
        spec.expires_at = spec.created_at + chrono::Duration::seconds(60);
        let plan = store.freeze_experiment_admission(spec, &config).unwrap();
        let envelope = plan.spec().controlled_admission.as_ref().unwrap();
        let source = crate::runtime::h2_entry::current_run_continuation().unwrap();
        let bound = source.for_admitted_plan(&plan, &envelope.pin, &config).unwrap();
        let mut bad = plan.spec().clone();
        bad.controlled_admission.as_mut().unwrap().owner_mac = "0".repeat(64);
        let bad = bad.freeze().unwrap();
        assert!(
            bound
                .sync_scope(|| store.admit_experiment(&bad, chrono::Utc::now()))
                .is_err()
        );
        assert_eq!(crate::runtime::h2_entry::test_scope_root_claims(), 0);
        let request = super::super::InstructionExperimentPlan {
            spec: plan.spec().clone(),
            operations: Default::default(),
        };
        // The real adapter must reach business validation with a valid once grant,
        // then reject the missing operation map without reserving a root first.
        let error = crate::tools::SkillsManageTool::test_controlled_admit(request, &config)
            .await
            .unwrap_err();
        assert!(
            !error.to_string().contains("grant"),
            "rejection must reach plan validation: {error}"
        );
        assert_eq!(crate::runtime::h2_entry::test_scope_root_claims(), 0);
        bound
            .sync_scope(|| store.admit_experiment(&plan, chrono::Utc::now()))
            .unwrap();
        assert_eq!(crate::runtime::h2_entry::test_scope_root_claims(), 1);
        bound
            .sync_scope(|| store.admit_experiment(&plan, chrono::Utc::now()))
            .unwrap();
        assert_eq!(crate::runtime::h2_entry::test_scope_root_claims(), 1);
        let store = std::sync::Arc::new(store);
        let plan = std::sync::Arc::new(plan);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let store = store.clone();
                let plan = plan.clone();
                let bound = bound.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    bound
                        .sync_scope(|| store.admit_experiment(&plan, chrono::Utc::now()))
                        .is_ok()
                })
            })
            .collect();
        barrier.wait();
        assert!(threads.into_iter().all(|thread| thread.join().unwrap()));
        assert_eq!(crate::runtime::h2_entry::test_scope_root_claims(), 1);
    })
    .await;
}

#[tokio::test]
async fn conflicting_authentic_spec_does_not_consume_capacity_or_replace_the_admitted_root() {
    crate::runtime::h2_entry::test_recorded_terminal_scope(|config| async move {
        let owner = config.skills_lifecycle.owner().unwrap().unwrap();
        let store = SkillLifecycleStore::open_existing(&config.workspace_dir, owner.clone())
            .unwrap()
            .unwrap();
        let mut spec = super::super::tests::specification();
        spec.owner_scope = owner;
        spec.created_at = chrono::Utc::now();
        spec.expires_at = spec.created_at + chrono::Duration::seconds(60);
        let mut alternate = spec.clone();
        alternate.resources.calls += 1;
        let plan = store.freeze_experiment_admission(spec, &config).unwrap();
        let conflict = store.freeze_experiment_admission(alternate, &config).unwrap();
        assert_eq!(
            plan.spec().id,
            conflict.spec().id,
            "same source/skill/business ID keeps original conflict semantics"
        );
        assert_ne!(plan.digest(), conflict.digest());
        store.verify_experiment_admission(&plan).unwrap();
        store.verify_experiment_admission(&conflict).unwrap();
        let source = crate::runtime::h2_entry::current_run_continuation().unwrap();
        let bound = source
            .for_admitted_plan(&plan, &plan.spec().controlled_admission.as_ref().unwrap().pin, &config)
            .unwrap();
        let original = bound
            .sync_scope(|| store.admit_experiment(&plan, chrono::Utc::now()))
            .unwrap();
        assert_eq!(crate::runtime::h2_entry::test_scope_root_claims(), 1);
        let conflicting = source
            .for_admitted_plan(
                &conflict,
                &conflict.spec().controlled_admission.as_ref().unwrap().pin,
                &config,
            )
            .unwrap();
        assert!(matches!(
            conflicting.sync_scope(|| store.admit_experiment(&conflict, chrono::Utc::now())),
            Err(ExperimentError::Conflict)
        ));
        assert_eq!(
            crate::runtime::h2_entry::test_scope_root_claims(),
            1,
            "a rejected real-MAC conflict cannot reserve a second root"
        );
        let replay = bound
            .sync_scope(|| store.admit_experiment(&plan, chrono::Utc::now()))
            .unwrap();
        assert_eq!(original.plan.digest(), replay.plan.digest());
        assert_eq!(original.head_sha256, replay.head_sha256);
        assert_eq!(crate::runtime::h2_entry::test_scope_root_claims(), 1);
    })
    .await;
}

#[tokio::test]
async fn controlled_identity_separates_actual_skills_without_changing_legacy_ids() {
    crate::runtime::h2_entry::test_recorded_terminal_scope(|config| async move {
        let owner = config.skills_lifecycle.owner().unwrap().unwrap();
        let store = SkillLifecycleStore::open_existing(&config.workspace_dir, owner.clone())
            .unwrap()
            .unwrap();
        let mut spec = super::super::tests::specification();
        spec.owner_scope = owner;
        spec.created_at = chrono::Utc::now();
        spec.expires_at = spec.created_at + chrono::Duration::seconds(60);
        let mut other = spec.clone();
        other.pins.skill_id = crate::skills::lifecycle::SkillId::parse("second-real-skill-identity").unwrap();
        let first = store.freeze_experiment_admission(spec.clone(), &config).unwrap();
        let second = store.freeze_experiment_admission(other.clone(), &config).unwrap();
        assert_ne!(
            first.spec().id,
            second.spec().id,
            "different skill locks cannot address the same controlled experiment directory"
        );
        store.verify_experiment_admission(&first).unwrap();
        store.verify_experiment_admission(&second).unwrap();
        assert_eq!(
            crate::runtime::h2_entry::test_scope_root_claims(),
            0,
            "planning identities grants no capacity"
        );
        crate::runtime::h2_entry::retain_run_continuation(None, async {
            let original = spec.clone().freeze().unwrap();
            let legacy = store.freeze_experiment_admission(spec, &config).unwrap();
            assert_eq!(legacy.spec().id, original.spec().id);
            assert_eq!(legacy.digest(), original.digest());
            let other_original = other.clone().freeze().unwrap();
            let other_legacy = store.freeze_experiment_admission(other, &config).unwrap();
            assert_eq!(other_legacy.spec().id, other_original.spec().id);
            assert_eq!(other_legacy.digest(), other_original.digest());
        })
        .await;
    })
    .await;
}

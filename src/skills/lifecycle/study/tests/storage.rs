use super::*;
use crate::skills::lifecycle::SkillLifecycleStore;

fn private_file(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
fn metadata_store(plan: &StudyPlan) -> Result<(tempfile::TempDir, SkillLifecycleStore)> {
    let workspace = tempfile::tempdir()?;
    let store = SkillLifecycleStore::open(workspace.path(), plan.manifest.owner_scope.clone())?;
    crate::decision::DecisionKey::load_or_create(workspace.path())?;
    Ok((workspace, store))
}

#[test]
fn caller_numbers_without_an_actual_stage_cannot_be_owner_sealed_as_trial_evidence() -> Result<()> {
    let (plan, baseline, _) = plan_fixture(2, 1)?;
    let (_workspace, store) = metadata_store(&plan)?;
    store.write_study_plan(&plan)?;
    let frozen = plan.experiment.spec.clone().freeze()?;
    let journal = crate::skills::lifecycle::experiment::ExperimentJournal {
        plan: frozen.clone(),
        entries: Vec::new(),
        state: crate::skills::lifecycle::experiment::ExperimentState::new(&frozen),
        head_sha256: frozen.digest().into(),
    };
    let pair = plan.manifest.pairs.first().context("pair")?;
    let (mut receipt, _) = observed_receipt(&plan, &baseline, pair, StudyArm::Baseline, Some(b"VERIFIED\n"))?;
    receipt.success = true;
    receipt.quality_ppm = 1_000_000;
    let config = crate::config::Config::default();
    assert!(store.write_study_trial(&journal, &receipt, &config).is_err());
    assert!(
        store
            .optional_study_trial(&plan.manifest.id, &receipt.binding.stage)?
            .is_none()
    );
    Ok(())
}

#[test]
fn altered_owner_metadata_and_unmetered_padding_do_not_survive_a_valid_mac_reference() -> Result<()> {
    let (plan, _, _) = plan_fixture(2, 1)?;
    let (_workspace, store) = metadata_store(&plan)?;
    store.write_study_plan(&plan)?;
    let path = store
        .root()
        .join("studies")
        .join(plan.manifest.id.as_str())
        .join("plan.json");
    let original = std::fs::read(&path)?;
    let mut changed: serde_json::Value = serde_json::from_slice(&original)?;
    *changed
        .get_mut("value")
        .and_then(|value| value.get_mut("manifest"))
        .and_then(|value| value.get_mut("owner_scope"))
        .context("owner field")? = serde_json::json!("PRXOtherOperator");
    private_file(&path, &serde_json::to_vec(&changed)?)?;
    assert!(
        store
            .read_study_plan(&plan.manifest.id, Some(&plan.manifest.sha256()?))
            .is_err()
    );
    private_file(&path, &original)?;
    assert!(
        store
            .read_study_plan(&plan.manifest.id, Some(&plan.manifest.sha256()?))
            .is_ok()
    );
    let mut padded = original.clone();
    padded.push(b' ');
    private_file(&path, &padded)?;
    assert!(
        store
            .read_study_plan(&plan.manifest.id, Some(&plan.manifest.sha256()?))
            .is_err(),
        "retained encoded bytes cannot grow outside the original accounting by preserving payload/MAC"
    );
    Ok(())
}

#[test]
fn same_identity_inspects_but_relabelled_duplicate_sample_is_not_a_fresh_holdout() -> Result<()> {
    let (plan, _, _) = plan_fixture(2, 1)?;
    let (_workspace, store) = metadata_store(&plan)?;
    store.write_study_plan(&plan)?;
    let before = std::fs::read(
        store
            .root()
            .join("studies")
            .join(plan.manifest.id.as_str())
            .join("plan.json"),
    )?;
    store.write_study_plan(&plan)?;
    assert_eq!(
        before,
        std::fs::read(
            store
                .root()
                .join("studies")
                .join(plan.manifest.id.as_str())
                .join("plan.json")
        )?
    );
    let mut relabelled = plan.clone();
    relabelled.manifest.id = ExperimentId::parse("renamed-study-fixture")?;
    relabelled.experiment.spec.id = relabelled.manifest.id.clone();
    for root in &mut relabelled.manifest.roots {
        root.root_task_id.push_str("-renamed");
        for case in &mut root.cases {
            let tools = relabelled
                .manifest
                .case_tools
                .remove(&case.case_id)
                .context("case tool ceiling")?;
            case.case_id.push_str("-renamed");
            relabelled.manifest.case_tools.insert(case.case_id.clone(), tools);
        }
    }
    rebind_plan(&mut relabelled)?;
    assert!(
        relabelled.validate().is_ok(),
        "all caller-controlled labels/digests are internally consistent"
    );
    assert!(store.write_study_plan(&relabelled).is_err());
    Ok(())
}

#[test]
fn actual_encoded_plan_metadata_needs_headroom_beyond_all_stage_allocations() -> Result<()> {
    let (mut plan, _, _) = plan_fixture(2, 1)?;
    let reserved = plan
        .experiment
        .spec
        .stages
        .iter()
        .try_fold(ExperimentResources::default(), |total, stage| {
            total.checked_add(stage.allocation)
        })?;
    plan.manifest.resources = reserved;
    rebind_plan(&mut plan)?;
    let (_workspace, store) = metadata_store(&plan)?;
    let error = store
        .write_study_plan(&plan)
        .expect_err("encoded plan/MAC bytes are real retained output, not a free side channel");
    assert!(error.to_string().contains("headroom"));
    assert!(
        !store
            .root()
            .join("studies")
            .join(plan.manifest.id.as_str())
            .join("plan.json")
            .exists()
    );
    Ok(())
}

#[test]
fn unknown_prior_history_is_not_skipped_to_admit_a_fresh_sample() -> Result<()> {
    let (plan, _, _) = plan_fixture(2, 1)?;
    let (_workspace, store) = metadata_store(&plan)?;
    store.write_study_plan(&plan)?;
    let orphan = store.root().join("studies/unresolved-prior");
    std::fs::create_dir(&orphan)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&orphan, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut fresh = plan.clone();
    fresh.manifest.id = ExperimentId::parse("fresh-study")?;
    for root in &mut fresh.manifest.roots {
        root.root_task_id.push_str("-fresh");
        for case in &mut root.cases {
            case.prompt.push_str(" A genuinely different fixture source.");
        }
    }
    rebind_plan(&mut fresh)?;
    assert!(store.write_study_plan(&fresh).is_err());
    assert!(!store.root().join("studies/fresh-study/plan.json").exists());
    Ok(())
}

#[test]
fn retained_actual_artifact_bytes_are_checked_against_size_and_content() -> Result<()> {
    let (plan, _, _) = plan_fixture(2, 1)?;
    let (_workspace, store) = metadata_store(&plan)?;
    store.write_study_plan(&plan)?;
    let stage = plan.manifest.pairs.first().context("pair")?.stage_id(StudyArm::H2);
    let sha = hash(b"VERIFIED\n");
    let path = store
        .root()
        .join("studies")
        .join(plan.manifest.id.as_str())
        .join(format!("effect-{stage}-{sha}.bin"));
    private_file(&path, b"VERIFIED\n")?;
    assert_eq!(
        store.read_study_artifact(&plan.manifest.id, &stage, &sha, 9)?,
        b"VERIFIED\n"
    );
    private_file(&path, b"TAMPERED\n")?;
    assert!(store.read_study_artifact(&plan.manifest.id, &stage, &sha, 9).is_err());
    Ok(())
}

#[test]
fn historical_physical_query_does_not_create_zero_ledger_or_migrate_raw_legacy_data() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let scope = hash(b"scope");
    assert!(crate::cost::tracker::CostTracker::physical_scope_usage_read_only(workspace.path(), &scope).is_err());
    assert!(!workspace.path().join("state").exists());
    let legacy_dir = workspace.path().join(".openprx");
    std::fs::create_dir(&legacy_dir)?;
    let legacy = legacy_dir.join("costs.db");
    private_file(&legacy, b"retained legacy fixture bytes")?;
    let before = std::fs::read(&legacy)?;
    let error = crate::cost::tracker::CostTracker::physical_scope_usage_read_only(workspace.path(), &scope)
        .expect_err("legacy evidence must need original migration owner");
    assert!(error.to_string().contains("migration"));
    assert_eq!(std::fs::read(&legacy)?, before);
    assert!(!workspace.path().join("state").exists());
    Ok(())
}

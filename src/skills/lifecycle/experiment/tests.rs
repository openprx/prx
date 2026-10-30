#![cfg(test)]
//! Contract tests use declared immutable inputs, not provider or execution mocks.
use super::*;
use crate::skills::lifecycle::{EvalSplit, ModelBinding, OwnerScope, RevisionId, SkillId};
use chrono::{Duration, Utc};

pub(crate) fn specification() -> ExperimentSpec {
    let created_at = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let resources = ExperimentResources {
        calls: 8,
        tokens: 8192,
        micro_usd: 100,
        tool_calls: 16,
        output_bytes: 8192,
        scratch_bytes: 65536,
    };
    ExperimentSpec {
        schema_version: EXPERIMENT_SCHEMA_VERSION,
        id: ExperimentId::parse("prx-experiment").unwrap(),
        owner_scope: OwnerScope::new("owner:prx:terminal:local-user").unwrap(),
        pins: ExperimentPins {
            skill_id: SkillId::parse("prx-skill").unwrap(),
            baseline_revision: RevisionId::parse(&format!("r1-{}", "1".repeat(64))).unwrap(),
            baseline_record_digest: "2".repeat(64),
            baseline_generation: 1,
            binary_sha256: "3".repeat(64),
            config_sha256: "4".repeat(64),
            policy_sha256: "5".repeat(64),
            catalog_sha256: "6".repeat(64),
            environment_sha256: "7".repeat(64),
            edit_scope_sha256: "8".repeat(64),
            model: ModelBinding {
                provider_label: "prx-provider".into(),
                model: "prx-model".into(),
                temperature_milli: 100,
            },
            suite_digest: "a".repeat(64),
            development_split_digest: "b".repeat(64),
            verification_split: EvalSplit::Validation,
            verification_split_digest: "c".repeat(64),
            holdout_split_digest: None,
        },
        resources,
        max_concurrent: 1,
        elapsed_ms: 60_000,
        created_at,
        expires_at: created_at + Duration::seconds(60),
        stages: vec![
            ExperimentStage {
                id: "proposal".into(),
                kind: ExperimentStageKind::ModelProposal,
                dependencies: vec![],
                input_sha256: "c".repeat(64),
                allocation: resources,
            },
            ExperimentStage {
                id: "verify".into(),
                kind: ExperimentStageKind::IndependentVerification,
                dependencies: vec!["proposal".into()],
                input_sha256: "d".repeat(64),
                allocation: resources,
            },
        ],
    }
}

#[test]
fn no_holdout_pin_preserves_pre_extension_frozen_spec_hash() {
    let spec = specification();
    assert!(
        serde_json::to_value(&spec)
            .unwrap()
            .get("pins")
            .unwrap()
            .get("holdout_split_digest")
            .is_none()
    );
    // Compact pre-extension specification, including its original field order.
    assert_eq!(
        spec.freeze().unwrap().digest(),
        "4e07350e4c7f3d91aee38dc5b5a1c3a015de01e1dc4a9293e412089b334a4cb0"
    );
}

#[test]
fn frozen_plan_roundtrips_and_pins_every_resource_and_input() {
    let spec = specification();
    let frozen = spec.clone().freeze().unwrap();
    let restored: ExperimentSpec = serde_json::from_slice(&serde_json::to_vec(&spec).unwrap()).unwrap();
    assert_eq!(restored.freeze().unwrap().digest(), frozen.digest());
    let mut changed = spec.clone();
    changed.resources.tokens += 1;
    assert_ne!(changed.freeze().unwrap().digest(), frozen.digest());
    let mut changed = spec.clone();
    changed.pins.model.model = "prx-other-model".into();
    assert_ne!(changed.freeze().unwrap().digest(), frozen.digest());
    let mut changed = spec.clone();
    changed.stages.last_mut().unwrap().input_sha256 = "e".repeat(64);
    assert_ne!(changed.freeze().unwrap().digest(), frozen.digest());
    assert_eq!(frozen.spec(), &spec);
}

#[test]
fn stage_graph_rejects_cycles_unknown_dependencies_and_duplicate_nodes() {
    for dependencies in [vec!["verify".into()], vec!["missing".into()]] {
        let mut spec = specification();
        spec.stages.first_mut().unwrap().dependencies = dependencies;
        assert!(spec.freeze().is_err());
    }
    let mut spec = specification();
    spec.stages.last_mut().unwrap().dependencies.push("proposal".into());
    assert!(spec.freeze().is_err());
    let mut spec = specification();
    spec.stages.push(spec.stages.first().unwrap().clone());
    assert!(spec.freeze().is_err());
}

#[test]
fn expired_plan_or_invalid_resource_and_verifier_identity_cannot_admit_work() {
    let spec = specification();
    let frozen = spec.clone().freeze().unwrap();
    assert!(frozen.ensure_live(spec.created_at).is_ok());
    assert!(frozen.ensure_live(spec.expires_at).is_err());
    assert!(frozen.ensure_live(spec.created_at - Duration::milliseconds(1)).is_err());
    let mut changed = spec.clone();
    changed.expires_at += Duration::milliseconds(1);
    assert!(changed.freeze().is_err());
    let mut changed = spec.clone();
    changed.stages.first_mut().unwrap().allocation.calls += 1;
    assert!(changed.freeze().is_err());
    let mut changed = spec.clone();
    changed.pins.verification_split_digest = changed.pins.development_split_digest.clone();
    assert!(changed.freeze().is_err());
    let mut changed = spec;
    changed.max_concurrent = 0;
    assert!(changed.freeze().is_err());
}

#[test]
fn resource_addition_refuses_overflow_instead_of_refreshing_allowance() {
    let maximum = ExperimentResources {
        tokens: u64::MAX,
        ..ExperimentResources::default()
    };
    assert!(
        maximum
            .checked_add(ExperimentResources {
                tokens: 1,
                ..ExperimentResources::default()
            })
            .is_err()
    );
    let resources = specification().resources;
    assert_eq!(
        resources.checked_add(ExperimentResources::default()).unwrap(),
        resources
    );
    assert!(!maximum.within(resources));
}

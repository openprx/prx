//! Representative wire-size DTO, not an adoption/proposal or executed cost
//! claim. No fake history is persisted; the actual private serializer is used.
use super::*;
use crate::skills::lifecycle::adoption::proof::{AdoptionRootCostEvidence, AdoptionStageCostEvidence};
use crate::skills::lifecycle::experiment::ExperimentResources;
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn typical_172_attempt_cost_dto_roundtrips_losslessly_with_record_headroom() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let store = SkillLifecycleStore::open(temp.path(), OwnerScope::new("cost-wire-fixture-owner")?)?;
    DecisionKey::load_or_create(temp.path())?;
    let mut serial = 0_u128;
    let mut stage = |name: String, calls: u64, prefix: &str, tool_calls: u64| {
        let mut attempts = BTreeMap::new();
        if calls > 0 {
            let suffixes = (0..calls)
                .map(|_| {
                    serial += 1;
                    uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0000_u128 | serial).to_string()
                })
                .collect();
            attempts.insert(prefix.to_owned(), suffixes);
        }
        AdoptionStageCostEvidence {
            physical_scope_id: digest(name.as_bytes()),
            stage: name,
            usage: [
                calls,
                calls * 150,
                calls * 7,
                tool_calls,
                if calls > 0 { 1024 } else { 128 },
                if calls > 0 { 8192 } else { 256 },
            ],
            attempts,
            model_indices: if calls > 0 { vec![0] } else { vec![] },
        }
    };
    let mut stages = Vec::new();
    // 7 x 3 development runs + 6 validation + 6 holdout, 5 physical attempts
    // per run, plus six editors and one judge: 172 admitted physical IDs.
    for number in 0..7 {
        stages.push(stage(format!("development-probe-{number}"), 15, "decision:", 6));
    }
    stages.push(stage("final-evaluation".into(), 30, "decision:", 12));
    stages.push(stage("final-holdout".into(), 30, "decision:", 12));
    for number in 0..6 {
        stages.push(stage(
            format!("development-editor-{number}"),
            1,
            "evolution_proposer:",
            0,
        ));
    }
    stages.push(stage("gate-review".into(), 1, "evolution_judge:", 0));
    for name in [
        "select-round-1",
        "select-round-2",
        "verify-validation",
        "verify-holdout",
    ] {
        stages.push(stage(name.into(), 0, "", 0));
    }
    let usage = stages.iter().try_fold(ExperimentResources::default(), |sum, stage| {
        sum.checked_add(stage.resources())
    })?;
    assert_eq!(usage.calls, 172);
    let body = AdoptionRootCostEvidence {
        schema_version: 1,
        owner_scope: store.owner_scope().clone(),
        experiment_id: "representative-wire-dto".into(),
        experiment_sha256: "a".repeat(64),
        experiment_head_sha256: "b".repeat(64),
        usage,
        models: vec!["qwen38-27b-splash".into()],
        stages,
    };
    body.validate()?;
    let dto = serde_json::json!({"kind":"cost-wire-dto-only","root_cost_evidence":body,
        "uninterpreted_fixture_metadata_reserve":"m".repeat(3*1024)});
    let not_written = store.root().join("cost-wire-dto-not-persisted.json");
    let wire = store.adoption_sealed_bytes(&dto, &not_written, b"prx-test-cost-dto-v1")?;
    assert!(
        u64::try_from(wire.len())? <= RECORD_CAP,
        "typical lossless cost metadata plus 3KiB headroom must fit the unchanged 16KiB wire cap; bytes={}",
        wire.len()
    );
    let (decoded, _): (serde_json::Value, _) = store.read_adoption_sealed(&wire, b"prx-test-cost-dto-v1")?;
    let decoded: AdoptionRootCostEvidence = serde_json::from_value(
        decoded
            .get("root_cost_evidence")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("actual wire omitted the root cost body"))?,
    )?;
    assert_eq!(decoded, body);
    let mut ids = BTreeSet::new();
    for stage in &decoded.stages {
        for (prefix, suffixes) in &stage.attempts {
            for suffix in suffixes {
                uuid::Uuid::parse_str(suffix)?;
                assert!(ids.insert(format!("{prefix}{suffix}")));
            }
        }
    }
    assert_eq!(ids.len(), 172);
    assert_eq!(decoded.usage, usage);
    assert!(!not_written.exists());
    assert!(
        !store.root().join("adoptions").exists(),
        "DTO encoding must not create an adoption/index"
    );
    Ok(())
}

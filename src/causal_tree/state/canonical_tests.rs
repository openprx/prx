#![cfg(test)]
#![allow(clippy::expect_used)] // All constructions are private typed fixtures.

use super::*;
use crate::causal_tree::branch::{
    CandidateSpec, CapabilityId, CapabilityRef, CapabilityTarget, CausalActionKind, CostEstimate, Ns8,
    SchemaFingerprint, SideEffectClass,
};

fn canonical_fixture(key: &DecisionKey, size: u8) -> CausalState {
    let mut state = fixtures::state(key, "PRX evidence \"quoted\" \\ newline\n\u{2603}\u{0000}\t\r\u{001f}");
    let mut list = Vec::new();
    for position in 0..size {
        let approval = match position % 3 {
            0 => ApprovalRequirement::None,
            1 => ApprovalRequirement::Required {
                op_ids: vec![OpId::new("prx_op_b"), OpId::new("prx_op_a"), OpId::new("prx_op_b")],
            },
            _ => ApprovalRequirement::Satisfied {
                grant_id: Arc::from("prx_grant"),
            },
        };
        let refs = (0..2)
            .map(|reference| CapabilityRef {
                id: CapabilityId::new(
                    CapabilityTarget::NativeTool,
                    "native",
                    &format!("prx_cap_{position}_{reference}"),
                )
                .expect("neutral component capability id"),
                fingerprint: SchemaFingerprint([position + reference; 32]),
                generation: CatalogGeneration([position; 32]),
            })
            .collect();
        list.push(DerivedCandidate {
            candidate: CausalCandidate::new(
                CausalActionKind::UseTool,
                CapabilityTarget::NativeTool,
                CandidateSpec {
                    namespace: Some(Ns8::of(key, &format!("prx_component_{position}"))),
                    required_capabilities: refs,
                    side_effect: match position % 6 {
                        0 => SideEffectClass::ReadOnly,
                        1 => SideEffectClass::LocalWrite,
                        2 => SideEffectClass::ProcessExec,
                        3 => SideEffectClass::ExternalSend,
                        4 => SideEffectClass::CredentialOrConfig,
                        _ => SideEffectClass::Destructive,
                    },
                    approval,
                    estimated_cost: match position % 3 {
                        0 => None,
                        1 => Some(CostEstimate {
                            tokens: 0,
                            micro_usd: 0,
                        }),
                        _ => Some(CostEstimate {
                            tokens: u32::MAX,
                            micro_usd: u64::MAX,
                        }),
                    },
                    estimated_latency_ms: match position % 3 {
                        0 => None,
                        1 => Some(0),
                        _ => Some(u32::MAX),
                    },
                },
            ),
            summary: Arc::from("PRX borrowed summary \" \u{2603}"),
            exact_mention: position % 2 == 0,
            available: position % 2 != 0,
        });
    }
    state.derived = DerivedState::new(list).expect("distinct typed component candidate ids");
    state
}

fn assert_oracle(state: &CausalState, key: &DecisionKey) {
    for record in [false, true] {
        let expected = legacy_canonical(state, record);
        assert_eq!(state.canonical(record), expected, "exact canonical bytes");
        let domain: &[u8] = if record { b"record" } else { b"material" };
        let digest = hex::encode(key.hmac(KeyDomain::StateFingerprint, &[domain, expected.as_bytes()]));
        assert_eq!(
            if record {
                state.record_fingerprint(key)
            } else {
                state.material_fingerprint(key)
            },
            digest
        );
    }
}

#[test]
fn canonical_bytes_and_keyed_digests_match_frozen_owned_oracle() {
    let key = fixtures::key();
    let other_key = fixtures::key();
    for size in [0, 1, 13] {
        let state = canonical_fixture(&key, size);
        assert_oracle(&state, &other_key);
        assert_ne!(state.material_fingerprint(&key), state.material_fingerprint(&other_key));
        assert_ne!(state.material_fingerprint(&key), state.record_fingerprint(&key));
        assert_oracle(&state, &key);
        let mut empty = state.clone();
        let mut trusted = empty.trusted.parts().clone();
        trusted.owner_id = None;
        trusted.session_key = None;
        trusted.channel = None;
        trusted.run_id = None;
        trusted.task_id = None;
        trusted.source_event_id = None;
        trusted.causation_event_id = None;
        trusted.exclusions.clear();
        trusted.constraints.clear();
        trusted.approval_grants.clear();
        trusted.budgets = TurnBudgets::default();
        trusted.delivery = DeliveryConstraints::default();
        empty.trusted = TrustedState::from_runtime(trusted);
        empty.observed = ObservedState::from_runtime_records(Vec::new(), Vec::new(), 0);
        assert_oracle(&empty, &key);
        for mode in [
            SideEffectMode::ReadOnly,
            SideEffectMode::ApprovalRequired,
            SideEffectMode::GuardedWrite,
        ] {
            let mut changed = state.clone();
            let mut trusted = changed.trusted.parts().clone();
            trusted.side_effect_mode = mode;
            trusted.approval_pending = true;
            trusted.approval_grants = vec![
                Arc::from("prx_grant_b"),
                Arc::from("prx_grant_a"),
                Arc::from("prx_grant_b"),
            ];
            trusted
                .exclusions
                .extend([Arc::from("prx_exclusion\n\u{2603}"), Arc::from("shell")]);
            trusted
                .constraints
                .push(ConstraintCode::new("no_destructive_filesystem"));
            trusted.task_id = Some(Arc::from("prx_task"));
            trusted.budgets = TurnBudgets {
                tokens: u64::MAX,
                latency_ms: u64::MAX,
                micro_usd: u64::MAX,
            };
            changed.trusted = TrustedState::from_runtime(trusted);
            changed.lineage = HopLineage::After {
                hop: NonZeroU8::new(4).expect("nonzero hop"),
                prior_decision_id: Uuid::from_u128(7),
            };
            changed.built_at_ms = u64::MAX;
            changed.observed = ObservedState::from_runtime_records(
                vec![
                    fixtures::step(ObservedStepKind::Retrieve, ObservedStepStatus::Failed),
                    fixtures::step(ObservedStepKind::Tool, ObservedStepStatus::Denied),
                ],
                [
                    0,
                    FRESH_ARTIFACT_MS - 1,
                    FRESH_ARTIFACT_MS,
                    FRESH_ARTIFACT_MS + 1,
                    STALE_ARTIFACT_MS - 1,
                    STALE_ARTIFACT_MS,
                    STALE_ARTIFACT_MS + 1,
                    u64::MAX,
                ]
                .into_iter()
                .map(|age| fixtures::artifact("PRX summary\n\u{2603}", age))
                .collect(),
                u32::MAX,
            );
            changed.routing_input = RawRoutingInput::new("x".repeat(16384));
            changed.goal = RawRoutingInput::new("prx_distinct_goal\u{0000}\t\u{001f}".to_owned());
            assert_oracle(&changed, &key);
            let mut reordered = changed.clone();
            let mut trusted = reordered.trusted.parts().clone();
            trusted.exclusions.reverse();
            trusted.constraints.reverse();
            trusted.approval_grants.reverse();
            reordered.trusted = TrustedState::from_runtime(trusted);
            let mut candidates = changed.derived.candidates().to_vec();
            candidates.reverse();
            reordered.derived = DerivedState::new(candidates).expect("same distinct candidates");
            assert_oracle(&reordered, &key);
            assert_eq!(reordered.canonical(false), changed.canonical(false));
            assert_eq!(reordered.canonical(true), changed.canonical(true));
        }
    }
}

#[test]
#[allow(clippy::print_stdout)] // Explicit diagnostic output is the regression evidence.
fn canonical_allocations_are_less_than_half_of_owned_oracle() {
    let key = fixtures::key();
    let mut all_bounded = true;
    for size in [0, 1, 13] {
        let state = canonical_fixture(&key, size);
        for record in [false, true] {
            for warm in [false, true] {
                if warm {
                    drop(state.canonical(record));
                }
                let (expected, original) =
                    crate::decision::builtin::allocation_counter::count(|| legacy_canonical(&state, record));
                let (actual, current) = crate::decision::builtin::allocation_counter::count(|| state.canonical(record));
                assert_eq!(actual, expected);
                println!(
                    "PRX_CANONICAL_ALLOCATION size={size} record={record} warm={warm} original={original} current={current}"
                );
                all_bounded &= current.checked_mul(2).is_some_and(|doubled| doubled < original);
            }
        }
    }
    assert!(
        all_bounded,
        "canonical serialization must use fewer than half the frozen owned oracle allocation attempts"
    );
}

#[test]
fn canonical_empty_state_matches_independent_lexical_golden() {
    let key = fixtures::key();
    let mut state = canonical_fixture(&key, 0);
    let mut trusted = state.trusted.parts().clone();
    trusted.workspace_id = Arc::from("prx_workspace");
    trusted.owner_id = None;
    trusted.session_key = None;
    trusted.run_id = None;
    trusted.task_id = None;
    trusted.channel = None;
    trusted.source_event_id = None;
    trusted.causation_event_id = None;
    trusted.exclusions.clear();
    trusted.constraints.clear();
    trusted.approval_grants.clear();
    trusted.approval_pending = false;
    trusted.budgets = TurnBudgets::default();
    trusted.delivery = DeliveryConstraints::default();
    trusted.catalog_generation = CatalogGeneration([0; 32]);
    trusted.policy_version = Arc::from("prx_policy");
    trusted.resolved_model = Arc::from("prx_model");
    state.trusted = TrustedState::from_runtime(trusted);
    state.observed = ObservedState::from_runtime_records(Vec::new(), Vec::new(), 0);
    state.routing_input = RawRoutingInput::new("prx_input".to_owned());
    state.goal = RawRoutingInput::new("prx_goal".to_owned());
    state.built_at_ms = 0;
    let material = concat!(
        "{\"derived\":[],\"goal\":\"prx_goal\",\"observed\":{\"artifacts\":[],\"completed_steps\":[],\"retry_count\":0},",
        "\"routing_input\":\"prx_input\",\"trusted\":{\"approval_grants\":[],\"approval_pending\":false,\"budgets\":[0,0,0],",
        "\"catalog_generation\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"channel\":null,",
        "\"constraints\":[],\"delivery\":[false,false,false],\"exclusions\":[],\"owner_id\":null,\"policy_version\":\"prx_policy\",",
        "\"resolved_model\":\"prx_model\",\"session_key\":null,\"side_effect_mode\":\"ReadOnly\",\"surface\":\"interactive\",\"workspace_id\":\"prx_workspace\"}}"
    );
    let record = material.replacen(
        "\"routing_input\":",
        concat!(
            "\"record\":{\"artifact_ages_ms\":[],\"artifact_event_ids\":[],\"built_at_ms\":0,\"causation_event_id\":null,\"hop\":0,",
            "\"prior_decision_id\":null,\"run_id\":null,\"source_event_id\":null,\"step_event_ids\":[],\"task_id\":null},\"routing_input\":"
        ),
        1,
    );
    assert_eq!(state.canonical(false), material);
    assert_eq!(state.canonical(true), record);
    assert_oracle(&state, &key);
}

#[test]
fn canonical_preserves_observation_and_capability_sequence_order() {
    let key = fixtures::key();
    let mut state = canonical_fixture(&key, 6);
    let candidate = state
        .derived
        .candidates()
        .first()
        .expect("component candidate")
        .candidate
        .candidate_id()
        .clone();
    let mut steps = Vec::new();
    for kind in [
        ObservedStepKind::Tool,
        ObservedStepKind::Provider,
        ObservedStepKind::Retrieve,
        ObservedStepKind::Discover,
    ] {
        for status in [
            ObservedStepStatus::Ok,
            ObservedStepStatus::Failed,
            ObservedStepStatus::Denied,
            ObservedStepStatus::Cancelled,
        ] {
            steps.push(ObservedStep::recorded_by_runtime(
                kind,
                status,
                Some(candidate.clone()),
                Some(Arc::from("prx_step")),
            ));
            steps.push(ObservedStep::recorded_by_runtime(kind, status, None, None));
        }
    }
    let mut artifacts = Vec::new();
    for kind in [
        ObservedArtifactKind::Memory,
        ObservedArtifactKind::Document,
        ObservedArtifactKind::Session,
        ObservedArtifactKind::Discovery,
    ] {
        let mut artifact = fixtures::artifact("PRX summary\u{0000}\t\r\u{001f}", STALE_ARTIFACT_MS);
        artifact.kind = kind;
        artifacts.push(artifact.clone());
        artifact.source_event_id = None;
        artifacts.push(artifact);
    }
    state.observed = ObservedState::from_runtime_records(steps, artifacts, 7);
    assert_oracle(&state, &key);
    let mut reversed_steps = state.clone();
    reversed_steps.observed.completed_steps.reverse();
    assert_oracle(&reversed_steps, &key);
    assert_ne!(state.canonical(false), reversed_steps.canonical(false));
    let mut reversed_artifacts = state.clone();
    reversed_artifacts.observed.artifacts.reverse();
    assert_oracle(&reversed_artifacts, &key);
    assert_ne!(state.canonical(false), reversed_artifacts.canonical(false));
    let mut reversed_caps = state.clone();
    let changed = state
        .derived
        .candidates()
        .iter()
        .map(|derived| {
            let candidate = &derived.candidate;
            let mut refs = candidate.required_capabilities().to_vec();
            refs.reverse();
            DerivedCandidate {
                candidate: CausalCandidate::new(
                    candidate.action(),
                    candidate.target(),
                    CandidateSpec {
                        namespace: candidate.candidate_id().namespace(),
                        required_capabilities: refs,
                        side_effect: candidate.side_effect(),
                        approval: candidate.approval().clone(),
                        estimated_cost: candidate.estimated_cost(),
                        estimated_latency_ms: candidate.estimated_latency_ms(),
                    },
                ),
                ..derived.clone()
            }
        })
        .collect();
    reversed_caps.derived = DerivedState::new(changed).expect("same distinct candidate ids");
    assert_oracle(&reversed_caps, &key);
    assert_ne!(state.canonical(false), reversed_caps.canonical(false));
    let mut record_only = state.clone();
    record_only.built_at_ms += 1;
    record_only
        .observed
        .completed_steps
        .iter_mut()
        .for_each(|step| step.event_id = None);
    record_only
        .observed
        .artifacts
        .iter_mut()
        .for_each(|artifact| artifact.source_event_id = None);
    assert_oracle(&record_only, &key);
    assert_eq!(state.canonical(false), record_only.canonical(false));
    assert_ne!(state.canonical(true), record_only.canonical(true));
}

fn legacy_canonical(state: &CausalState, record: bool) -> String {
    let trusted = state.trusted.parts();
    let text = |value: &Option<Arc<str>>| value.as_deref().map(str::to_string);
    let mut value = json!({
        "trusted": {
            "workspace_id": trusted.workspace_id.as_ref(),
            "owner_id": text(&trusted.owner_id),
            "session_key": text(&trusted.session_key),
            "channel": text(&trusted.channel),
            "surface": trusted.surface.as_str(),
            "side_effect_mode": format!("{:?}", trusted.side_effect_mode),
            "exclusions": legacy_sorted_set(trusted.exclusions.iter().map(AsRef::as_ref)),
            "constraints": legacy_sorted_set(trusted.constraints.iter().map(|code| code.as_str())),
            "approval_pending": trusted.approval_pending,
            "approval_grants": legacy_sorted_set(trusted.approval_grants.iter().map(AsRef::as_ref)),
            "budgets": [trusted.budgets.tokens, trusted.budgets.latency_ms, trusted.budgets.micro_usd],
            "delivery": [trusted.delivery.interactive, trusted.delivery.can_ask_question, trusted.delivery.approval_channel],
            "catalog_generation": trusted.catalog_generation.to_hex(),
            "policy_version": trusted.policy_version.as_ref(),
            "resolved_model": trusted.resolved_model.as_ref(),
        },
        "observed": {
            "completed_steps": state.observed.completed_steps().iter().map(|step| json!([
                step.kind().as_str(),
                step.status().as_str(),
                step.candidate().map(CandidateId::as_str),
            ])).collect::<Vec<_>>(),
            "artifacts": state.observed.artifacts().iter().map(|artifact| json!([
                artifact.kind.as_str(),
                artifact_age_class(artifact.age_ms),
                artifact.summary,
            ])).collect::<Vec<_>>(),
            "retry_count": state.observed.retry_count(),
        },
        "derived": state.derived_sorted().map(|derived| {
            let candidate = &derived.candidate;
            let approval = match candidate.approval() {
                ApprovalRequirement::None => json!(["none"]),
                ApprovalRequirement::Required { op_ids } => {
                    json!(["required", legacy_sorted_set(op_ids.iter().map(OpId::as_str))])
                }
                ApprovalRequirement::Satisfied { grant_id } => json!(["satisfied", grant_id.as_ref()]),
            };
            json!({
                "id": candidate.candidate_id().as_str(),
                "side_effect": candidate.side_effect().as_str(),
                "approval": approval,
                "capabilities": candidate.required_capabilities().iter().map(|capability| json!([
                    capability.id.as_str(),
                    hex::encode(capability.fingerprint.0),
                    capability.generation.to_hex(),
                ])).collect::<Vec<_>>(),
                "cost": candidate.estimated_cost().map(|cost| [u64::from(cost.tokens), cost.micro_usd]),
                "latency_ms": candidate.estimated_latency_ms(),
                "summary": derived.summary.as_ref(),
                "exact_mention": derived.exact_mention,
                "available": derived.available,
            })
        }).collect::<Vec<_>>(),
        "routing_input": state.routing_input.as_str(),
        "goal": state.goal.as_str(),
    });
    if let (true, Some(object)) = (record, value.as_object_mut()) {
        let record = json!({
            "run_id": text(&trusted.run_id),
            "task_id": text(&trusted.task_id),
            "source_event_id": text(&trusted.source_event_id),
            "causation_event_id": text(&trusted.causation_event_id),
            "hop": state.lineage.hop(),
            "prior_decision_id": state.lineage.prior_decision_id().map(|id| id.to_string()),
            "step_event_ids": state.observed.completed_steps().iter().map(|step| text(step.event_id())).collect::<Vec<_>>(),
            "artifact_ages_ms": state.observed.artifacts().iter().map(|artifact| artifact.age_ms).collect::<Vec<_>>(),
            "artifact_event_ids": state.observed.artifacts().iter().map(|artifact| text(&artifact.source_event_id)).collect::<Vec<_>>(),
            "built_at_ms": state.built_at_ms,
        });
        object.insert("record".to_string(), record);
    }
    value.to_string()
}

fn legacy_sorted_set<'a>(items: impl Iterator<Item = &'a str>) -> Vec<String> {
    items
        .collect::<BTreeSet<&str>>()
        .into_iter()
        .map(str::to_string)
        .collect()
}

#[test]
fn canonical_distinguishes_mixed_delivery_empty_optionals_and_maximum_hop() {
    let key = fixtures::key();
    let state = canonical_fixture(&key, 3);
    for flags in 0_u8..8 {
        let mut changed = state.clone();
        let mut trusted = changed.trusted.parts().clone();
        trusted.delivery = DeliveryConstraints {
            interactive: flags & 1 != 0,
            can_ask_question: flags & 2 != 0,
            approval_channel: flags & 4 != 0,
        };
        trusted.owner_id = Some(Arc::from(""));
        trusted.session_key = Some(Arc::from(""));
        trusted.channel = Some(Arc::from(""));
        trusted.run_id = Some(Arc::from(""));
        trusted.task_id = Some(Arc::from(""));
        trusted.source_event_id = Some(Arc::from(""));
        trusted.causation_event_id = Some(Arc::from(""));
        changed.trusted = TrustedState::from_runtime(trusted);
        changed.lineage = HopLineage::After {
            hop: NonZeroU8::new(u8::MAX).expect("maximum nonzero hop"),
            prior_decision_id: Uuid::from_u128(u128::MAX),
        };
        assert_oracle(&changed, &key);
        let mut absent = changed.clone();
        let mut trusted = absent.trusted.parts().clone();
        trusted.owner_id = None;
        absent.trusted = TrustedState::from_runtime(trusted);
        assert_oracle(&absent, &key);
        assert_ne!(changed.canonical(false), absent.canonical(false));
    }
}

#[test]
fn model_admission_canonical_commits_only_digest_and_none_keeps_legacy_bytes() {
    use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
    use crate::tools::capability::model_policy::ModelAdmission;
    let key = fixtures::key();
    let catalog =
        CapabilityCatalog::build(&[], &CatalogSources::default(), &SurfacePolicy::default()).expect("empty catalog");
    let view = catalog.view(&CatalogScope::default());
    let policy = ModelAdmission::fresh(&[], &view, "prx-model", &[], "prx-policy", &key);
    let mut state = canonical_fixture(&key, 3);
    assert_oracle(&state, &key);
    let mut parts = state.trusted.parts().clone();
    parts.model_admission = Some(Arc::clone(&policy));
    state.trusted = TrustedState::from_runtime(parts);
    for record in [false, true] {
        let mut expected: serde_json::Value =
            serde_json::from_str(&legacy_canonical(&state, record)).expect("old scalar canonical oracle");
        expected
            .get_mut("trusted")
            .expect("trusted object")
            .as_object_mut()
            .expect("trusted map")
            .insert(
                "model_admission".into(),
                serde_json::to_value(policy.digest()).expect("fixed digest"),
            );
        assert_eq!(
            state.canonical(record),
            serde_json::to_string(&expected).expect("oracle canonical JSON")
        );
    }
    let mut parts = state.trusted.parts().clone();
    parts.model_admission = None;
    state.trusted = TrustedState::from_runtime(parts);
    assert_oracle(&state, &key);
}

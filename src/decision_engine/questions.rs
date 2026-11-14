//! Deterministic questions over the admitted causal candidate set.
//!
//! The compiler reads only the redacted state projection. Question ids,
//! options and instructions come from typed actions, targets and fixed text.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::cost::decision_attempt::error::{DecisionError, RequestRule};
use crate::decision_engine::candidate::{CandidateId, CausalCandidate};
use crate::decision_engine::candidate_generator::CandidateSet;
use crate::decision_engine::prediction::{
    CandidateQuestion, DecisionQuestion, DecisionRequest, GlobalQuestion, OmissionReason, OptionId, ProjectionAudit,
    QuestionId, RequestLimits, RequestParts, StructuredValue,
};
use crate::decision_engine::projection::{ProjectionError, redacted_projection, shrink_oldest_artifact_summary};
use crate::decision_engine::state::CausalState;
use crate::security::runtime_key::DecisionKey;
use crate::tools::capability::contract::{ApprovalRequirement, CausalActionKind};

/// Inputs bound by the runtime to this decision hop.
pub(crate) struct CompilerPolicy<'a> {
    pub key: &'a DecisionKey,
    pub model: Option<&'a str>,
    pub expires_at: Instant,
    pub limits: RequestLimits,
}

/// The sealed request and local projection facts the service records with it.
pub(crate) struct CompiledQuestionRequest {
    request: DecisionRequest,
}

impl CompiledQuestionRequest {
    pub(crate) const fn request(&self) -> &DecisionRequest {
        &self.request
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CompileError {
    #[error("state fingerprint serialization failed")]
    Fingerprint(#[source] serde_json::Error),
    #[error("a NoLegalPath has no decision request")]
    NoLegalPath,
    #[error("state candidates differ from the admitted candidate set")]
    CandidateMismatch,
    #[error("decision request exceeds its byte budget after artifact shrinking")]
    Budget,
    #[error(transparent)]
    Projection(#[from] ProjectionError),
    #[error(transparent)]
    Protocol(#[from] DecisionError),
}

pub(crate) struct QuestionCompiler;

impl QuestionCompiler {
    pub(crate) fn compile(
        state: &CausalState,
        candidate_set: &CandidateSet,
        policy: &CompilerPolicy<'_>,
    ) -> Result<CompiledQuestionRequest, CompileError> {
        let CandidateSet::Legal {
            candidates,
            exact_mentions,
        } = candidate_set
        else {
            return Err(CompileError::NoLegalPath);
        };
        let derived: BTreeMap<_, _> = state
            .derived
            .candidates()
            .iter()
            .map(|item| (item.candidate.candidate_id(), item))
            .collect();
        if candidates.len() != derived.len()
            || candidates.iter().any(|candidate| {
                derived.get(candidate.candidate_id()).is_none_or(|item| {
                    item.candidate != *candidate
                        || item.exact_mention != exact_mentions.contains(candidate.candidate_id())
                        || !item.available
                })
            })
        {
            return Err(CompileError::CandidateMismatch);
        }

        let (projection, mut projection_manifest) = redacted_projection(state)?;
        let projected_ids = projection
            .as_json()
            .get("candidates")
            .and_then(Value::as_array)
            .ok_or(CompileError::CandidateMismatch)?;
        if projected_ids.len() != candidates.len() {
            return Err(CompileError::CandidateMismatch);
        }
        let positions: BTreeMap<_, _> = projected_ids
            .iter()
            .enumerate()
            .filter_map(|(index, item)| item.get("id").and_then(Value::as_str).map(|id| (id.to_string(), index)))
            .collect();
        if positions.len() != candidates.len() {
            return Err(CompileError::CandidateMismatch);
        }

        let nonterminal: Vec<_> = candidates
            .iter()
            .filter(|candidate| !is_fixed_terminal(candidate.action()))
            .collect();
        let mut questions = BTreeMap::new();
        let mut omitted = Vec::new();
        if nonterminal.len() == 1 {
            omitted.push((
                QuestionId::global(GlobalQuestion::NextAction),
                OmissionReason::SingleLegalCandidate,
            ));
        } else {
            let mut actions = BTreeSet::new();
            for candidate in candidates {
                actions.insert(candidate.action().as_str());
            }
            let mut criteria = BTreeMap::new();
            for action in actions {
                criteria.insert(OptionId::code(action), fixed_text(action)?);
            }
            criteria.insert(OptionId::code("other"), fixed_text("Other action")?);
            questions.insert(
                QuestionId::global(GlobalQuestion::NextAction),
                DecisionQuestion::Choice {
                    instructions: fixed_text("Choose the best legal next action.")?,
                    criteria,
                },
            );
        }

        let targets: BTreeSet<_> = candidates.iter().map(|candidate| candidate.target().as_str()).collect();
        if nonterminal.len() == 1 || targets.len() == 1 {
            omitted.push((
                QuestionId::global(GlobalQuestion::CapabilityTarget),
                if nonterminal.len() == 1 {
                    OmissionReason::SingleLegalCandidate
                } else {
                    OmissionReason::SingleTarget
                },
            ));
        } else {
            let mut criteria = BTreeMap::new();
            for target in targets {
                criteria.insert(OptionId::code(target), fixed_text(target)?);
            }
            criteria.insert(OptionId::code("none"), fixed_text("No capability")?);
            questions.insert(
                QuestionId::global(GlobalQuestion::CapabilityTarget),
                DecisionQuestion::Choice {
                    instructions: fixed_text("Choose the capability family needed by the next action.")?,
                    criteria,
                },
            );
        }
        questions.insert(
            QuestionId::global(GlobalQuestion::EvidenceSufficient),
            DecisionQuestion::Noul {
                instructions: fixed_text("Is the observed evidence sufficient for the next action?")?,
                criteria: None,
            },
        );
        let has_artifacts = projection
            .as_json()
            .get("artifacts")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty());
        if has_artifacts {
            questions.insert(
                QuestionId::global(GlobalQuestion::NeedsFreshState),
                DecisionQuestion::Noul {
                    instructions: fixed_text("Could the observed artifacts be stale?")?,
                    criteria: None,
                },
            );
        } else {
            omitted.push((
                QuestionId::global(GlobalQuestion::NeedsFreshState),
                OmissionReason::NoArtifacts,
            ));
        }
        questions.insert(
            QuestionId::global(GlobalQuestion::SemanticRisk),
            DecisionQuestion::Score {
                instructions: fixed_text("Rate the risk of the next action.")?,
                levels: score_levels()?,
            },
        );
        omitted.push((
            QuestionId::global(GlobalQuestion::ExpectedGain),
            OmissionReason::UnusedPrediction,
        ));

        for candidate in candidates {
            let id = candidate.candidate_id();
            let index = *positions.get(id.as_str()).ok_or(CompileError::CandidateMismatch)?;
            let path = format!("$.candidates[{index}]");
            let key = id.key();
            // Fixed terminal selection is code-owned; scoring/extraction never
            // consumes these candidate predictions. Keep the omission manifest
            // rather than paying for four unused answers per terminal.
            if is_fixed_terminal(candidate.action()) {
                for kind in [
                    CandidateQuestion::Applicable,
                    CandidateQuestion::Risk,
                    CandidateQuestion::Success,
                    CandidateQuestion::Gain,
                ] {
                    omitted.push((
                        QuestionId::candidate(&key, kind),
                        OmissionReason::DeterministicCandidate,
                    ));
                }
                continue;
            }
            questions.insert(
                QuestionId::candidate(&key, CandidateQuestion::Applicable),
                DecisionQuestion::Noul {
                    instructions: fixed_text(&format!("Does {path} apply to this state?"))?,
                    criteria: None,
                },
            );
            questions.insert(
                QuestionId::candidate(&key, CandidateQuestion::Risk),
                DecisionQuestion::Score {
                    instructions: fixed_text(&format!("Rate the risk of {path}."))?,
                    levels: score_levels()?,
                },
            );
            questions.insert(
                QuestionId::candidate(&key, CandidateQuestion::Success),
                DecisionQuestion::Noul {
                    instructions: fixed_text(&format!("Would {path} succeed?"))?,
                    criteria: None,
                },
            );
            questions.insert(
                QuestionId::candidate(&key, CandidateQuestion::Gain),
                DecisionQuestion::Score {
                    instructions: fixed_text(&format!("Rate the task gain from {path}."))?,
                    levels: score_levels()?,
                },
            );
        }
        omitted.sort_by(|left, right| left.0.cmp(&right.0));

        let candidate_set_hash = hash_candidate_set(candidates, exact_mentions);
        let trusted = state.trusted.parts();
        let mut parts = RequestParts {
            state: projection,
            questions,
            model: policy.model.map(str::to_string),
            state_fingerprint: state
                .material_fingerprint(policy.key)
                .map_err(CompileError::Fingerprint)?,
            candidate_set_hash,
            policy_version: Arc::clone(&trusted.policy_version),
            catalog_generation: trusted.catalog_generation.to_hex(),
            omitted_questions: omitted,
            approval_pending: trusted.approval_pending,
        };
        loop {
            let projection_audit = ProjectionAudit {
                truncated_fields: projection_manifest
                    .truncated
                    .iter()
                    .map(|field| (*field).to_string())
                    .collect(),
                redacted_texts: projection_manifest.redacted_texts,
            };
            match DecisionRequest::seal_recovering_parts(
                parts,
                projection_audit,
                policy.key,
                policy.expires_at,
                &policy.limits,
            ) {
                Ok(request) => {
                    return Ok(CompiledQuestionRequest { request });
                }
                Err((DecisionError::InvalidRequest(RequestRule::RequestTooLarge), recovered)) => {
                    parts = *recovered;
                    parts.state = shrink_oldest_artifact_summary(&parts.state)?.ok_or(CompileError::Budget)?;
                    projection_manifest.truncated.push("artifacts.summary");
                }
                Err((error, _)) => return Err(CompileError::Protocol(error)),
            }
        }
    }
}

pub(crate) fn hash_candidate_set(candidates: &[CausalCandidate], exact_mentions: &BTreeSet<CandidateId>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"prx.cdp.candidates.v1\0");
    let mut ordered: Vec<_> = candidates.iter().collect();
    ordered.sort_by_key(|candidate| candidate.candidate_id());
    hasher.update(b"candidates\0");
    hasher.update(ordered.len().to_be_bytes());
    for candidate in ordered {
        hash_candidate(&mut hasher, candidate);
    }
    hasher.update(b"exact_mentions\0");
    hasher.update(exact_mentions.len().to_be_bytes());
    for id in exact_mentions {
        hash_text(&mut hasher, id.as_str());
    }
    // The retired approval scopes keep their always-empty encoding, so the
    // hash bytes equal the former advisory case.
    hasher.update(b"approval_scopes\0");
    hasher.update(0usize.to_be_bytes());
    hex::encode(hasher.finalize())
}

fn hash_candidate(hasher: &mut Sha256, candidate: &CausalCandidate) {
    hash_text(hasher, candidate.candidate_id().as_str());
    hash_text(hasher, candidate.side_effect().as_str());
    hash_text(hasher, candidate.approval().code());
    match candidate.approval() {
        ApprovalRequirement::None => {}
        ApprovalRequirement::Required { op_ids } => {
            let mut ordered = op_ids.iter().collect::<Vec<_>>();
            ordered.sort();
            hasher.update(ordered.len().to_be_bytes());
            for op_id in ordered {
                hash_text(hasher, op_id.as_str());
            }
        }
    }
    let mut refs = candidate.required_capabilities().iter().collect::<Vec<_>>();
    refs.sort();
    hasher.update(refs.len().to_be_bytes());
    for reference in refs {
        hash_text(hasher, reference.id.as_str());
        hasher.update(reference.fingerprint.0);
        hasher.update(reference.generation.0);
    }
    match candidate.estimated_cost() {
        Some(cost) => {
            hasher.update([1]);
            hasher.update(cost.tokens.to_be_bytes());
            hasher.update(cost.micro_usd.to_be_bytes());
        }
        None => hasher.update([0]),
    }
    match candidate.estimated_latency_ms() {
        Some(latency) => {
            hasher.update([1]);
            hasher.update(latency.to_be_bytes());
        }
        None => hasher.update([0]),
    }
}

fn hash_text(hasher: &mut Sha256, text: &str) {
    hasher.update(text.len().to_be_bytes());
    hasher.update(text.as_bytes());
}

const fn is_fixed_terminal(action: CausalActionKind) -> bool {
    matches!(action, CausalActionKind::Clarify | CausalActionKind::Refuse)
}

fn fixed_text(text: &str) -> Result<StructuredValue, DecisionError> {
    StructuredValue::try_from(Value::String(text.to_string()))
}

fn score_levels() -> Result<Vec<StructuredValue>, DecisionError> {
    ["very low", "low", "moderate", "high", "very high"]
        .into_iter()
        .map(fixed_text)
        .collect()
}

#[cfg(test)]
mod tests {
    mod ownership;
    use super::*;
    static EMPTY_DISCOVERED_REFS: std::sync::LazyLock<crate::decision_engine::candidate_generator::DiscoveredRefs> =
        std::sync::LazyLock::new(crate::decision_engine::candidate_generator::DiscoveredRefs::empty);
    use crate::decision_engine::candidate::CausalCandidate;
    use crate::decision_engine::candidate_generator::{CandidatePolicy, CausalCandidateGenerator};
    use crate::decision_engine::cost_model::CapabilityCostModel;
    use crate::decision_engine::prediction::wire::SystemOneCodec;
    use crate::decision_engine::state::{DerivedCandidate, DerivedState, RawRoutingInput, fixtures};
    use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
    use crate::tools::capability::contract::{CapabilityRef, CapabilityTarget};
    use crate::tools::capability::turn_state::{SideEffectMode, TrustedState};
    use crate::tools::mcp::McpCapabilitySpec;
    use crate::tools::traits::ToolSpec;
    use std::time::Duration;

    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/decision/state_v1/check_state_v1.rs"
    ));

    fn legal(state: &CausalState) -> CandidateSet {
        let candidates: Vec<CausalCandidate> = state
            .derived
            .candidates()
            .iter()
            .map(|derived| derived.candidate.clone())
            .collect();
        CandidateSet::Legal {
            candidates,
            exact_mentions: BTreeSet::new(),
        }
    }

    fn fixture_cost_for_view(
        view: &crate::tools::capability::catalog::CatalogView<'_>,
        mut cost: CapabilityCostModel,
    ) -> CapabilityCostModel {
        for entry in view.entries() {
            if let Some(estimate) = cost.estimates.get(&entry.family) {
                cost.per_capability.insert(
                    CapabilityRef {
                        id: entry.id.clone(),
                        fingerprint: entry.fingerprint,
                        generation: entry.generation,
                    },
                    *estimate,
                );
            }
        }
        cost
    }

    fn bind_generated(state: &mut CausalState, set: &CandidateSet, catalog: &CapabilityCatalog) {
        let CandidateSet::Legal {
            candidates,
            exact_mentions,
            ..
        } = set
        else {
            panic!("expected legal candidates");
        };
        let view = catalog.view(&CatalogScope::default());
        state.derived = DerivedState::new(
            candidates
                .iter()
                .map(|candidate| DerivedCandidate {
                    candidate: candidate.clone(),
                    summary: Arc::from(
                        candidate
                            .required_capabilities()
                            .first()
                            .and_then(|reference| view.entries().iter().find(|entry| entry.id == reference.id))
                            .map_or("Fixed terminal", |entry| entry.summary.as_str()),
                    ),
                    exact_mention: exact_mentions.contains(candidate.candidate_id()),
                    available: true,
                })
                .collect(),
        )
        .unwrap();
    }

    #[test]
    fn compiler_uses_projected_order_and_typed_ids() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "unrelated request");
        let mut derived = state.derived.candidates().to_vec();
        derived.reverse();
        state.derived = DerivedState::new(derived).unwrap();
        let set = legal(&state);
        let policy = CompilerPolicy {
            key: &key,
            model: Some("anthropic:claude"),
            expires_at: Instant::now() + Duration::from_secs(30),
            limits: RequestLimits::default(),
        };
        let first = QuestionCompiler::compile(&state, &set, &policy).unwrap();
        let second = QuestionCompiler::compile(&state, &set, &policy).unwrap();
        assert_eq!(
            state_v1_violations(first.request.state().as_json()),
            Vec::<String>::new()
        );
        assert_eq!(first.request.state(), second.request.state());
        assert_eq!(
            SystemOneCodec::new(&first.request, &key)
                .unwrap()
                .encode_request()
                .unwrap(),
            SystemOneCodec::new(&second.request, &key)
                .unwrap()
                .encode_request()
                .unwrap(),
        );
        assert_eq!(first.request.questions(), second.request.questions());
        assert_eq!(
            first.request.manifest().request_fingerprint(),
            second.request.manifest().request_fingerprint()
        );
        assert_ne!(first.request.request_id(), second.request.request_id());
        assert!(
            first.request.manifest().projection_audit().truncated_fields.is_empty(),
            "expected an empty value"
        );
        let projected = first.request.state().as_json()["candidates"].as_array().unwrap();
        for (index, item) in projected.iter().enumerate() {
            let id = item["id"].as_str().unwrap();
            let question_id = format!("candidate.{id}.applicable");
            let question = first
                .request
                .questions()
                .iter()
                .find(|(key, _)| key.as_str() == question_id)
                .unwrap()
                .1;
            assert_eq!(
                question.instructions().as_json().as_str(),
                Some(format!("Does $.candidates[{index}] apply to this state?").as_str()),
            );
        }
        state.routing_input = RawRoutingInput::new("malicious.next_action candidate.fake.risk".to_string());
        let changed = QuestionCompiler::compile(&state, &set, &policy).unwrap();
        assert_eq!(
            first.request.questions().keys().collect::<Vec<_>>(),
            changed.request.questions().keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn compiler_rejects_state_candidate_desync_and_no_legal_path() {
        let key = fixtures::key();
        let state = fixtures::state(&key, "neutral request");
        let policy = CompilerPolicy {
            key: &key,
            model: None,
            expires_at: Instant::now() + Duration::from_secs(30),
            limits: RequestLimits::default(),
        };
        let mut set = legal(&state);
        if let CandidateSet::Legal { candidates, .. } = &mut set {
            candidates.pop();
        }
        assert!(matches!(
            QuestionCompiler::compile(&state, &set, &policy),
            Err(CompileError::CandidateMismatch)
        ));
        let no_legal = CandidateSet::NoLegalPath;
        assert!(matches!(
            QuestionCompiler::compile(&state, &no_legal, &policy),
            Err(CompileError::NoLegalPath)
        ));
    }

    #[test]
    fn decision_request_bytes_stay_constant_as_mcp_inventory_grows() {
        let key = fixtures::key();
        let mut wire_lengths = Vec::new();
        for extras in [0, 8, 64, 512] {
            let mut sources = CatalogSources::default();
            for index in 0..=extras {
                let tool = if index == 0 {
                    "aa_seed".to_string()
                } else {
                    format!("zz_extra_{index}")
                };
                sources.mcp.push(McpCapabilitySpec {
                    server: "prx_server".into(),
                    tool: tool.clone(),
                    state_digest: [1; 32],
                    spec: ToolSpec {
                        name: format!("mcp__prx_server__{tool}"),
                        description: if index == 0 { "Fixed compact summary".into() } else { "Extra inventory entry".into() },
                        parameters: serde_json::json!({"type":"object","properties":{"PRX_SCHEMA_SENTINEL":{"type":"string"}}}),
                    },
                });
            }
            let catalog = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).unwrap();
            let view = catalog.view(&CatalogScope::default());
            let mut state = fixtures::state(&key, "unrelated request");
            let mut trusted = fixtures::trusted();
            trusted.owner_id = Some(Arc::from("prx_user"));
            trusted.catalog_generation = *catalog.generation();
            trusted.channel = None;
            trusted.side_effect_mode = SideEffectMode::GuardedWrite;
            state.trusted = TrustedState::from_runtime(trusted);
            let cost = CapabilityCostModel {
                per_capability: BTreeMap::new(),
                estimates: BTreeMap::from([
                    (
                        CapabilityTarget::None,
                        (
                            crate::tools::capability::contract::CostEstimate {
                                tokens: 1,
                                micro_usd: 0,
                            },
                            1,
                        ),
                    ),
                    (
                        CapabilityTarget::Mcp,
                        (
                            crate::tools::capability::contract::CostEstimate {
                                tokens: 1,
                                micro_usd: 0,
                            },
                            1,
                        ),
                    ),
                ]),
            };
            let cost = fixture_cost_for_view(&view, cost);
            let candidate_policy = CandidatePolicy {
                key: &key,
                max_candidates: 12,
                can_deliver_text: true,
                may_delegate: false,
                delegation_target: None,
                cost_model: &cost,
                discovered_refs: &EMPTY_DISCOVERED_REFS,
            };
            let admitted = CausalCandidateGenerator::generate(&state, &view, &candidate_policy).unwrap();
            let seed = view
                .entries()
                .iter()
                .find(|entry| entry.public_name == "mcp__prx_server__aa_seed")
                .expect("fixed MCP seed exists");
            assert!(
                matches!(&admitted, CandidateSet::Legal { candidates, .. } if candidates.iter().any(|candidate| candidate.required_capabilities().iter().any(|reference| reference.id == seed.id))),
                "fixed MCP seed must survive admission at inventory size {extras}"
            );
            bind_generated(&mut state, &admitted, &catalog);
            let compiler_policy = CompilerPolicy {
                key: &key,
                model: Some("anthropic:claude"),
                expires_at: Instant::now() + Duration::from_secs(30),
                limits: RequestLimits::default(),
            };
            let compiled = QuestionCompiler::compile(&state, &admitted, &compiler_policy).unwrap();
            let body = SystemOneCodec::new(&compiled.request, &key)
                .unwrap()
                .encode_request()
                .unwrap();
            let body = String::from_utf8(body).unwrap();
            assert!(!body.contains("PRX_SCHEMA_SENTINEL"));
            assert!(!body.contains("zz_extra_"));
            assert!(body.contains("Fixed compact summary"));
            assert!(
                state_v1_violations(compiled.request.state().as_json()).is_empty(),
                "expected an empty value"
            );
            wire_lengths.push(body.len());
        }
        let first = wire_lengths
            .first()
            .copied()
            .expect("four inventory sizes were measured");
        assert_eq!(wire_lengths, vec![first; 4]);
    }

    #[test]
    fn budget_shrinker_uses_artifact_age_with_stable_ties() {
        fn summary(value: &StructuredValue, index: usize) -> Option<&str> {
            value
                .as_json()
                .get("artifacts")?
                .as_array()?
                .get(index)?
                .get("summary")?
                .as_str()
        }
        let projection = StructuredValue::try_from(serde_json::json!({
            "artifacts": [
                {"age_ms": 3, "summary": "newer"},
                {"age_ms": 9, "summary": "older-a"},
                {"age_ms": 9, "summary": "older-b"}
            ]
        }))
        .unwrap();
        let first = shrink_oldest_artifact_summary(&projection).unwrap().unwrap();
        assert_eq!(summary(&first, 1), Some(""));
        assert_eq!(summary(&first, 2), Some("older-b"));
        let second = shrink_oldest_artifact_summary(&first).unwrap().unwrap();
        assert_eq!(summary(&second, 2), Some(""));
        assert_eq!(summary(&second, 0), Some("newer"));
    }

    #[test]
    fn candidate_hash_separates_mentions_and_keeps_its_pre_change_bytes() {
        let key = fixtures::key();
        let working = fixtures::candidate(&key, CausalActionKind::UseTool, CapabilityTarget::Mcp, None).candidate;
        let id = working.candidate_id().clone();
        let candidates = vec![working];
        assert_ne!(
            hash_candidate_set(&candidates, &BTreeSet::from([id])),
            hash_candidate_set(&candidates, &BTreeSet::new())
        );
        // One fixed refusal, mentioned exactly. The literal was computed from
        // the encoding with an empty approval-scope map, so the retired scope
        // field must keep its empty bytes.
        let refusal = CausalCandidate::new(
            CausalActionKind::Refuse,
            CapabilityTarget::None,
            crate::decision_engine::candidate::CandidateSpec {
                namespace: None,
                required_capabilities: Vec::new(),
                side_effect: crate::tools::capability::contract::SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: Some(crate::tools::capability::contract::CostEstimate {
                    tokens: 0,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(0),
            },
        );
        let refusal_id = refusal.candidate_id().clone();
        assert_eq!(refusal_id.as_str(), "refuse-none");
        assert_eq!(
            hash_candidate_set(&[refusal], &BTreeSet::from([refusal_id])),
            "7488d8d9c99dba9564d503160078244051053a52df671fa962625ff6a7739abb"
        );
    }

    #[test]
    fn one_working_candidate_forces_choice_and_omits_staleness_without_artifacts() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let working = fixtures::candidate(&key, CausalActionKind::UseTool, CapabilityTarget::Mcp, None);
        let refusal = fixtures::candidate(&key, CausalActionKind::Refuse, CapabilityTarget::None, None);
        state.derived = DerivedState::new(vec![working, refusal]).unwrap();
        state.observed = crate::decision_engine::state::ObservedState::default();
        let set = legal(&state);
        let policy = CompilerPolicy {
            key: &key,
            model: None,
            expires_at: Instant::now() + Duration::from_secs(30),
            limits: RequestLimits::default(),
        };
        let compiled = QuestionCompiler::compile(&state, &set, &policy).unwrap();
        assert!(
            !compiled
                .request
                .questions()
                .contains_key(&QuestionId::global(GlobalQuestion::NextAction))
        );
        assert!(
            !compiled
                .request
                .questions()
                .contains_key(&QuestionId::global(GlobalQuestion::NeedsFreshState))
        );
        assert_eq!(
            compiled
                .request
                .manifest()
                .omitted_questions()
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "candidate.refuse-none.applicable",
                "candidate.refuse-none.gain",
                "candidate.refuse-none.risk",
                "candidate.refuse-none.success",
                "capability_target",
                "expected_gain",
                "needs_fresh_state",
                "next_action",
            ],
        );
        assert_eq!(
            compiled
                .request
                .manifest()
                .omitted_questions()
                .iter()
                .find(|(id, _)| id.as_str() == "capability_target")
                .map(|(_, reason)| *reason),
            Some(OmissionReason::SingleLegalCandidate),
        );
    }
}

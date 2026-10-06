//! Deterministic questions over the admitted causal candidate set.
//!
//! The compiler reads only the redacted state projection. Question ids,
//! options and instructions come from typed actions, targets and fixed text.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::branch::{ApprovalRequirement, CandidateId, CausalActionKind, CausalCandidate};
use super::candidate_generator::{CandidateSet, PendingApproval};
use super::projection::{ProjectionError, redacted_projection, shrink_oldest_artifact_summary};
use super::state::CausalState;
use crate::decision::{
    CandidateQuestion, DecisionError, DecisionKey, DecisionQuestion, DecisionRequest, GlobalQuestion, OmissionReason,
    OptionId, ProjectionAudit, QuestionId, RequestLimits, RequestParts, RequestRule, StructuredValue,
};

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
    /// Values that a selector reads as probability 1 when a Choice question
    /// was omitted because there was only one possible value.
    forced_choices: BTreeMap<GlobalQuestion, OptionId>,
}

impl CompiledQuestionRequest {
    pub(crate) const fn request(&self) -> &DecisionRequest {
        &self.request
    }

    pub(crate) fn forced_choice(&self, question: GlobalQuestion) -> Option<&OptionId> {
        self.forced_choices.get(&question)
    }

    /// Check the sidecar against the sealed omission manifest and the exact
    /// admitted set before extraction treats an omitted Choice as certain.
    pub(crate) fn forced_choices_match(&self, candidates: &[CausalCandidate]) -> bool {
        let working: Vec<_> = candidates
            .iter()
            .filter(|candidate| !is_fixed_terminal(candidate.action()))
            .collect();
        let targets: BTreeSet<_> = candidates.iter().map(CausalCandidate::target).collect();
        let action = working.first().filter(|_| working.len() == 1).map(|candidate| {
            (
                OptionId::code(candidate.action().as_str()),
                OmissionReason::SingleLegalCandidate,
            )
        });
        let target = if working.len() == 1 {
            working.first().map(|candidate| {
                (
                    OptionId::code(candidate.target().as_str()),
                    OmissionReason::SingleLegalCandidate,
                )
            })
        } else if targets.len() == 1 {
            targets
                .first()
                .map(|target| (OptionId::code(target.as_str()), OmissionReason::SingleTarget))
        } else {
            None
        };
        [
            (GlobalQuestion::NextAction, action),
            (GlobalQuestion::CapabilityTarget, target),
        ]
        .into_iter()
        .all(|(question, expected)| {
            let id = QuestionId::global(question);
            let omitted = self
                .request
                .manifest()
                .omitted_questions()
                .iter()
                .find(|(found, _)| *found == id);
            match expected {
                Some((option, reason)) => {
                    self.forced_choices.get(&question) == Some(&option)
                        && omitted.is_some_and(|(_, actual)| *actual == reason)
                        && !self.request.questions().contains_key(&id)
                }
                None => {
                    !self.forced_choices.contains_key(&question)
                        && omitted.is_none()
                        && self.request.questions().contains_key(&id)
                }
            }
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CompileError {
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
            approval_scopes,
            ..
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
        let mut forced_choices = BTreeMap::new();
        if nonterminal.len() == 1 {
            omitted.push((
                QuestionId::global(GlobalQuestion::NextAction),
                OmissionReason::SingleLegalCandidate,
            ));
            if let Some(candidate) = nonterminal.first() {
                forced_choices.insert(GlobalQuestion::NextAction, OptionId::code(candidate.action().as_str()));
            }
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
            if let Some(candidate) = nonterminal.first() {
                forced_choices.insert(
                    GlobalQuestion::CapabilityTarget,
                    OptionId::code(candidate.target().as_str()),
                );
            } else if let Some(target) = targets.first() {
                forced_choices.insert(GlobalQuestion::CapabilityTarget, OptionId::code(target));
            }
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
        questions.insert(
            QuestionId::global(GlobalQuestion::ExpectedGain),
            DecisionQuestion::Score {
                instructions: fixed_text("Rate the expected task gain.")?,
                levels: score_levels()?,
            },
        );

        for candidate in candidates {
            let id = candidate.candidate_id();
            let index = *positions.get(id.as_str()).ok_or(CompileError::CandidateMismatch)?;
            let path = format!("$.candidates[{index}]");
            let key = id.key();
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
            if matches!(candidate.action(), CausalActionKind::Refuse | CausalActionKind::Clarify) {
                for kind in [CandidateQuestion::Success, CandidateQuestion::Gain] {
                    omitted.push((
                        QuestionId::candidate(&key, kind),
                        OmissionReason::DeterministicCandidate,
                    ));
                }
            } else {
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
        }
        omitted.sort_by(|left, right| left.0.cmp(&right.0));

        let candidate_set_hash = hash_candidate_set(candidates, exact_mentions, approval_scopes);
        let trusted = state.trusted.parts();
        let mut parts = RequestParts {
            state: projection,
            questions,
            model: policy.model.map(str::to_string),
            state_fingerprint: state.material_fingerprint(policy.key),
            candidate_set_hash,
            policy_version: Arc::clone(&trusted.policy_version),
            catalog_generation: trusted.catalog_generation.to_hex(),
            omitted_questions: omitted,
            approval_pending: trusted.approval_pending || !approval_scopes.is_empty(),
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
                    return Ok(CompiledQuestionRequest {
                        request,
                        forced_choices,
                    });
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

pub(crate) fn hash_candidate_set(
    candidates: &[CausalCandidate],
    exact_mentions: &BTreeSet<CandidateId>,
    approval_scopes: &BTreeMap<CandidateId, Vec<PendingApproval>>,
) -> String {
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
    hasher.update(b"approval_scopes\0");
    hasher.update(approval_scopes.len().to_be_bytes());
    for (id, pending) in approval_scopes {
        hash_text(&mut hasher, id.as_str());
        hasher.update(pending.len().to_be_bytes());
        for scope in pending {
            hash_candidate(&mut hasher, &scope.candidate);
            let mut op_ids: Vec<_> = scope.op_ids.iter().collect();
            op_ids.sort();
            hasher.update(op_ids.len().to_be_bytes());
            for op_id in op_ids {
                hash_text(&mut hasher, op_id.as_str());
            }
        }
    }
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
        ApprovalRequirement::Satisfied { grant_id } => hash_text(hasher, grant_id),
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
    matches!(
        action,
        CausalActionKind::Clarify | CausalActionKind::RequestApproval | CausalActionKind::Refuse
    )
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
    static EMPTY_DISCOVERED_REFS: std::sync::LazyLock<crate::causal_tree::candidate_generator::DiscoveredRefs> =
        std::sync::LazyLock::new(crate::causal_tree::candidate_generator::DiscoveredRefs::empty);
    use crate::causal_tree::branch::{CapabilityRef, CapabilityTarget, CausalCandidate};
    use crate::causal_tree::candidate_generator::{CandidatePolicy, CapabilityCostModel, CausalCandidateGenerator};
    use crate::causal_tree::gates::GateInputs;
    use crate::causal_tree::gates::GateReport;
    use crate::causal_tree::state::{
        DerivedCandidate, DerivedState, RawRoutingInput, SideEffectMode, TrustedState, fixtures,
    };
    use crate::decision::wire::SystemOneCodec;
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
    use crate::tools::mcp::McpCapabilitySpec;
    use crate::tools::traits::{Tool, ToolResult, ToolSpec};
    use async_trait::async_trait;
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
            truncated: Vec::new(),
            discoverable_families: Default::default(),
            approval_scopes: BTreeMap::new(),
            report: GateReport {
                outcomes: BTreeMap::new(),
                gate_set_hash: [0; 32],
            },
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

    struct ReadProbe(&'static str);

    #[async_trait]
    impl Tool for ReadProbe {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "Read project evidence"
        }
        fn parameters_schema(&self) -> Value {
            serde_json::json!({"type":"object","properties":{}})
        }
        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
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
        assert!(first.request.manifest().projection_audit().truncated_fields.is_empty());
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
        let no_legal = CandidateSet::NoLegalPath {
            reason: super::super::candidate_generator::NoLegalReason::NoLegalCandidate,
            terminals: Vec::new(),
            approval_only: Vec::new(),
            report: GateReport {
                outcomes: BTreeMap::new(),
                gate_set_hash: [0; 32],
            },
        };
        assert!(matches!(
            QuestionCompiler::compile(&state, &no_legal, &policy),
            Err(CompileError::NoLegalPath)
        ));
    }

    #[test]
    fn hard_gated_candidate_is_absent_from_compiled_request() {
        let key = fixtures::key();
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(ReadProbe("prx_read")), Box::new(ReadProbe("prx_search"))];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let view = catalog.view(&CatalogScope::default());
        let mut state = fixtures::state(&key, "use prx_read");
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
                        super::super::branch::CostEstimate {
                            tokens: 1,
                            micro_usd: 0,
                        },
                        1,
                    ),
                ),
                (
                    CapabilityTarget::NativeTool,
                    (
                        super::super::branch::CostEstimate {
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
        let mut security = SecurityPolicy::default();
        security.autonomy = AutonomyLevel::Full;
        let calls_seen = BTreeSet::new();
        let calls = BTreeMap::new();
        let gates = GateInputs {
            policy: &security,
            owner_id: "prx_user",
            sender: "prx_user",
            policy_channel: "terminal",
            chat_type: "dm",
            model_ref: "anthropic:claude",
            model_can_chat: true,
            model_can_call_tools: true,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &calls_seen,
            retry_limit: Some(2),
        };
        let first = CausalCandidateGenerator::generate(&state, &view, &candidate_policy, &gates).unwrap();
        let CandidateSet::Legal {
            candidates,
            exact_mentions,
            ..
        } = &first
        else {
            panic!("expected legal candidates");
        };
        let gated_id = candidates
            .iter()
            .find(|candidate| {
                exact_mentions.contains(candidate.candidate_id())
                    && candidate.required_capabilities().first().is_some_and(|reference| {
                        view.entries()
                            .iter()
                            .any(|entry| entry.id == reference.id && entry.public_name == "prx_read")
                    })
            })
            .expect("read capability was admitted before exclusion")
            .candidate_id()
            .as_str()
            .to_string();
        let mut trusted = state.trusted.parts().clone();
        trusted.exclusions.push("prx_read".into());
        state.trusted = TrustedState::from_runtime(trusted);
        let admitted = CausalCandidateGenerator::generate(&state, &view, &candidate_policy, &gates).unwrap();
        assert!(
            matches!(&admitted, CandidateSet::Legal { candidates, .. } if candidates.iter().all(|candidate| candidate.candidate_id().as_str() != gated_id))
        );
        bind_generated(&mut state, &admitted, &catalog);
        let compiler_policy = CompilerPolicy {
            key: &key,
            model: Some("anthropic:claude"),
            expires_at: Instant::now() + Duration::from_secs(30),
            limits: RequestLimits::default(),
        };
        let compiled = QuestionCompiler::compile(&state, &admitted, &compiler_policy).unwrap();
        let encoded = SystemOneCodec::new(&compiled.request, &key)
            .unwrap()
            .encode_request()
            .unwrap();
        let encoded = String::from_utf8(encoded).unwrap();
        assert!(!encoded.contains(&gated_id));
        assert!(encoded.contains("use prx_read"));
        assert!(
            compiled
                .request
                .questions()
                .keys()
                .all(|id| !id.as_str().contains(&gated_id))
        );
        assert!(state_v1_violations(compiled.request.state().as_json()).is_empty());
        assert!(
            matches!(&admitted, CandidateSet::Legal { candidates, .. } if candidates.iter().any(|candidate| candidate.required_capabilities().first().is_some_and(|reference| view.entries().iter().any(|entry| entry.id == reference.id && entry.public_name == "prx_search"))))
        );
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
                            super::super::branch::CostEstimate {
                                tokens: 1,
                                micro_usd: 0,
                            },
                            1,
                        ),
                    ),
                    (
                        CapabilityTarget::Mcp,
                        (
                            super::super::branch::CostEstimate {
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
            let mut security = SecurityPolicy::default();
            security.autonomy = AutonomyLevel::Full;
            let calls_seen = BTreeSet::new();
            let calls = BTreeMap::new();
            let gates = GateInputs {
                policy: &security,
                owner_id: "prx_user",
                sender: "prx_user",
                policy_channel: "terminal",
                chat_type: "dm",
                model_ref: "anthropic:claude",
                model_can_chat: true,
                model_can_call_tools: true,
                decision_endpoint_ready: true,
                child_scope: None,
                verified_approval_calls: BTreeMap::new(),
                call_fingerprints: &calls,
                failed_call_counts: BTreeMap::new(),
                replayed_non_idempotent: &calls_seen,
                retry_limit: Some(2),
            };
            let admitted = CausalCandidateGenerator::generate(&state, &view, &candidate_policy, &gates).unwrap();
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
            assert!(state_v1_violations(compiled.request.state().as_json()).is_empty());
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
    fn candidate_hash_separates_mentions_from_approval_scope() {
        let key = fixtures::key();
        let candidate = fixtures::candidate(&key, CausalActionKind::UseTool, CapabilityTarget::Mcp, None).candidate;
        let id = candidate.candidate_id().clone();
        let candidates = vec![candidate.clone()];
        let mentioned = hash_candidate_set(&candidates, &BTreeSet::from([id.clone()]), &BTreeMap::new());
        let empty_scope = hash_candidate_set(
            &candidates,
            &BTreeSet::new(),
            &BTreeMap::from([(id.clone(), Vec::new())]),
        );
        assert_ne!(mentioned, empty_scope);
        let scope_a = PendingApproval {
            candidate: candidate.clone(),
            op_ids: vec![super::super::branch::OpId::new("mcp:call:a")],
        };
        let scope_b = PendingApproval {
            candidate,
            op_ids: vec![super::super::branch::OpId::new("mcp:call:b")],
        };
        assert_ne!(
            hash_candidate_set(
                &candidates,
                &BTreeSet::new(),
                &BTreeMap::from([(id.clone(), vec![scope_a])])
            ),
            hash_candidate_set(&candidates, &BTreeSet::new(), &BTreeMap::from([(id, vec![scope_b])])),
        );
    }

    #[test]
    fn byte_budget_shrinks_summaries_without_changing_questions_or_candidates() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let artifacts = (0..8)
            .map(|index| fixtures::artifact(&format!("artifact-{index}-{}", "a".repeat(220)), index * 10))
            .collect();
        state.observed = crate::causal_tree::state::ObservedState::from_runtime_records(Vec::new(), artifacts, 0);
        let set = legal(&state);
        let mut policy = CompilerPolicy {
            key: &key,
            model: None,
            expires_at: Instant::now() + Duration::from_secs(30),
            limits: RequestLimits::default(),
        };
        let baseline = QuestionCompiler::compile(&state, &set, &policy).unwrap();
        let mut shrunk = None;
        for bytes in (2_000..=8_000).step_by(256) {
            policy.limits.max_request_bytes = bytes;
            if let Ok(compiled) = QuestionCompiler::compile(&state, &set, &policy) {
                if !compiled
                    .request
                    .manifest()
                    .projection_audit()
                    .truncated_fields
                    .is_empty()
                {
                    shrunk = Some(compiled);
                    break;
                }
            }
        }
        let shrunk = shrunk.expect("a bounded request fits after artifact shrinking");
        assert_eq!(baseline.request.questions(), shrunk.request.questions());
        assert_eq!(
            baseline.request.state().as_json()["candidates"],
            shrunk.request.state().as_json()["candidates"],
        );
        assert_ne!(
            baseline.request.manifest().request_fingerprint(),
            shrunk.request.manifest().request_fingerprint()
        );
        policy.limits.max_request_bytes = 64;
        assert!(matches!(
            QuestionCompiler::compile(&state, &set, &policy),
            Err(CompileError::Budget)
        ));
    }

    #[test]
    fn one_working_candidate_forces_choice_and_omits_staleness_without_artifacts() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let working = fixtures::candidate(&key, CausalActionKind::UseTool, CapabilityTarget::Mcp, None);
        let refusal = fixtures::candidate(&key, CausalActionKind::Refuse, CapabilityTarget::None, None);
        state.derived = DerivedState::new(vec![working, refusal]).unwrap();
        state.observed = crate::causal_tree::state::ObservedState::default();
        let set = legal(&state);
        let policy = CompilerPolicy {
            key: &key,
            model: None,
            expires_at: Instant::now() + Duration::from_secs(30),
            limits: RequestLimits::default(),
        };
        let mut compiled = QuestionCompiler::compile(&state, &set, &policy).unwrap();
        assert_eq!(
            compiled.forced_choices.get(&GlobalQuestion::NextAction),
            Some(&OptionId::code("use_tool"))
        );
        assert_eq!(
            compiled.forced_choices.get(&GlobalQuestion::CapabilityTarget),
            Some(&OptionId::code("mcp"))
        );
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
                "candidate.refuse-none.gain",
                "candidate.refuse-none.success",
                "capability_target",
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
        let CandidateSet::Legal { candidates, .. } = &set else {
            panic!("expected legal candidates");
        };
        assert!(compiled.forced_choices_match(candidates));
        compiled
            .forced_choices
            .insert(GlobalQuestion::NextAction, OptionId::code("refuse"));
        assert!(!compiled.forced_choices_match(candidates));
    }
}

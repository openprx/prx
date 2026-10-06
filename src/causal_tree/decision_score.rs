//! Versioned, pure scoring for admitted decision candidates.
//!
//! This module is independent of the `llm-router`-only legacy scorer. The
//! decision service consumes these scores after the post-rehearsal gates.

use std::collections::BTreeMap;

use super::branch::{CandidateId, CapabilityTarget, CausalActionKind, SideEffectClass};
use super::candidate_generator::CandidateSet;
use super::config::ScoringConfig;
use super::questions::{CompiledQuestionRequest, hash_candidate_set};
use crate::decision::{
    CandidateQuestion, DecisionAnswer, GlobalQuestion, OmissionReason, OptionId, QuestionId, ScoreAnswer,
    ValidatedDecision,
};

pub(crate) const SCORER_VERSION: &str = "scorer-v1";
const VERSION: &str = SCORER_VERSION;

/// Prediction features extracted from a validated decision response.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CandidatePrediction {
    pub choice_probability: f64,
    pub applicability_probability: f64,
    pub success_probability: f64,
    pub expected_gain: f64,
    pub gain_uncertainty: f64,
    pub semantic_risk: f64,
    pub risk_uncertainty: f64,
}

/// Global abstention evidence and normalized per-candidate features. Choice
/// confidence is deliberately separate from each candidate's probability.
#[derive(Debug, Clone)]
pub(crate) struct PredictionEvidence {
    pub candidates: BTreeMap<CandidateId, CandidatePrediction>,
    pub action_confidence: f64,
    pub target_confidence: f64,
    pub target_probabilities: BTreeMap<CapabilityTarget, f64>,
    pub chosen_action: String,
    pub chosen_target: String,
    pub evidence_sufficient: f64,
    pub needs_fresh_state: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ExtractionError {
    #[error("decision response belongs to another request")]
    RequestMismatch,
    #[error("candidate bodies differ from the sealed request")]
    CandidateMismatch,
    #[error("there is no legal candidate set to score")]
    NoLegalPath,
    #[error("validated prediction lacks a required decision answer")]
    MissingAnswer,
    #[error("validated prediction has an unexpected primitive")]
    WrongPrimitive,
    #[error("candidate set has duplicate ids")]
    DuplicateCandidate,
}

impl PredictionEvidence {
    pub(crate) fn extract(
        decision: &ValidatedDecision,
        compiled: &CompiledQuestionRequest,
        candidate_set: &CandidateSet,
    ) -> Result<Self, ExtractionError> {
        if decision.request_id() != compiled.request().request_id() {
            return Err(ExtractionError::RequestMismatch);
        }
        let CandidateSet::Legal {
            candidates,
            exact_mentions,
            approval_scopes,
            ..
        } = candidate_set
        else {
            return Err(ExtractionError::NoLegalPath);
        };
        if hash_candidate_set(candidates, exact_mentions, approval_scopes)
            != compiled.request().manifest().candidate_set_hash()
        {
            return Err(ExtractionError::CandidateMismatch);
        }
        if !compiled.forced_choices_match(candidates) {
            return Err(ExtractionError::CandidateMismatch);
        }
        let (action_probs, action_confidence, chosen_action) = choice(decision, compiled, GlobalQuestion::NextAction)?;
        let (target_probs, target_confidence, chosen_target) =
            choice(decision, compiled, GlobalQuestion::CapabilityTarget)?;
        let target_probabilities = CapabilityTarget::ALL
            .into_iter()
            .map(|target| {
                let probability = target_probs
                    .get(&OptionId::code(target.as_str()))
                    .copied()
                    .unwrap_or(0.0);
                (target, probability)
            })
            .collect();
        let evidence_sufficient = noul(decision, QuestionId::global(GlobalQuestion::EvidenceSufficient))?;
        // The compiler deliberately omits this question when there are no
        // artifacts. Such a state has no stale observed artifact.
        let freshness_id = QuestionId::global(GlobalQuestion::NeedsFreshState);
        let needs_fresh_state = match decision.answer(&freshness_id) {
            Some(DecisionAnswer::Noul(answer)) => answer.probability_true.get(),
            Some(_) => return Err(ExtractionError::WrongPrimitive),
            None if compiled
                .request()
                .manifest()
                .omitted_questions()
                .contains(&(freshness_id, OmissionReason::NoArtifacts)) =>
            {
                0.0
            }
            None => return Err(ExtractionError::MissingAnswer),
        };
        let global_risk = score_answer(decision, QuestionId::global(GlobalQuestion::SemanticRisk))?.0;
        let mut raw = BTreeMap::new();
        for candidate in candidates.iter().filter(|candidate| !is_terminal(candidate.action())) {
            let action = OptionId::code(candidate.action().as_str());
            let target = OptionId::code(candidate.target().as_str());
            let action_probability = action_probs.get(&action).copied().unwrap_or(0.0);
            let target_probability = target_probs.get(&target).copied().unwrap_or(0.0);
            if raw
                .insert(
                    candidate.candidate_id().clone(),
                    action_probability * target_probability,
                )
                .is_some()
            {
                return Err(ExtractionError::DuplicateCandidate);
            }
        }
        let total: f64 = raw.values().sum();
        let mut extracted = BTreeMap::new();
        for candidate in candidates.iter().filter(|candidate| !is_terminal(candidate.action())) {
            let key = candidate.candidate_id().key();
            let applicable = noul(decision, QuestionId::candidate(&key, CandidateQuestion::Applicable))?;
            let success = noul(decision, QuestionId::candidate(&key, CandidateQuestion::Success))?;
            let (gain, gain_uncertainty) =
                score_answer(decision, QuestionId::candidate(&key, CandidateQuestion::Gain))?;
            let (candidate_risk, risk_uncertainty) =
                score_answer(decision, QuestionId::candidate(&key, CandidateQuestion::Risk))?;
            let risk_floor = risk_floor(candidate.side_effect());
            let raw_probability = raw.get(candidate.candidate_id()).copied().unwrap_or(0.0);
            extracted.insert(
                candidate.candidate_id().clone(),
                CandidatePrediction {
                    choice_probability: if total > 0.0 { raw_probability / total } else { 0.0 },
                    applicability_probability: applicable,
                    success_probability: success,
                    expected_gain: gain,
                    gain_uncertainty,
                    semantic_risk: candidate_risk.max(global_risk).max(risk_floor),
                    risk_uncertainty,
                },
            );
        }
        Ok(Self {
            candidates: extracted,
            action_confidence,
            target_confidence,
            target_probabilities,
            chosen_action,
            chosen_target,
            evidence_sufficient,
            needs_fresh_state,
        })
    }
}

const fn is_terminal(action: CausalActionKind) -> bool {
    matches!(
        action,
        CausalActionKind::Clarify | CausalActionKind::RequestApproval | CausalActionKind::Refuse
    )
}

const fn risk_floor(effect: SideEffectClass) -> f64 {
    match effect {
        SideEffectClass::ReadOnly => 0.0,
        SideEffectClass::LocalWrite => 0.3,
        SideEffectClass::ProcessExec => 0.4,
        SideEffectClass::ExternalSend => 0.5,
        SideEffectClass::CredentialOrConfig => 0.6,
        SideEffectClass::Destructive => 0.7,
    }
}

fn choice(
    decision: &ValidatedDecision,
    compiled: &CompiledQuestionRequest,
    question: GlobalQuestion,
) -> Result<(BTreeMap<OptionId, f64>, f64, String), ExtractionError> {
    if let Some(forced) = compiled.forced_choice(question) {
        return Ok((
            BTreeMap::from([(forced.clone(), 1.0)]),
            1.0,
            forced.as_str().to_string(),
        ));
    }
    match decision.answer(&QuestionId::global(question)) {
        Some(DecisionAnswer::Choice(answer)) => Ok((
            answer
                .probabilities
                .iter()
                .map(|(id, probability)| (id.clone(), probability.get()))
                .collect(),
            answer.confidence.get(),
            answer.chosen.as_str().to_string(),
        )),
        Some(_) => Err(ExtractionError::WrongPrimitive),
        None => Err(ExtractionError::MissingAnswer),
    }
}

fn noul(decision: &ValidatedDecision, id: QuestionId) -> Result<f64, ExtractionError> {
    match decision.answer(&id) {
        Some(DecisionAnswer::Noul(answer)) => Ok(answer.probability_true.get()),
        Some(_) => Err(ExtractionError::WrongPrimitive),
        None => Err(ExtractionError::MissingAnswer),
    }
}

fn score_answer(decision: &ValidatedDecision, id: QuestionId) -> Result<(f64, f64), ExtractionError> {
    match decision.answer(&id) {
        Some(DecisionAnswer::Score(answer)) => Ok(normalized_score(answer)),
        Some(_) => Err(ExtractionError::WrongPrimitive),
        None => Err(ExtractionError::MissingAnswer),
    }
}

fn normalized_score(answer: &ScoreAnswer) -> (f64, f64) {
    let max_level = answer.distribution.len().saturating_sub(1) as f64;
    if max_level == 0.0 {
        return (0.0, 0.0);
    }
    let expected = (answer.expected / max_level).clamp(0.0, 1.0);
    let variance: f64 = answer
        .distribution
        .iter()
        .enumerate()
        .map(|(index, probability)| probability.get() * ((index as f64) - answer.expected).powi(2))
        .sum();
    let uncertainty = (variance.sqrt() / (max_level / 2.0)).clamp(0.0, 1.0);
    (expected, uncertainty)
}

/// The scorer's immutable policy snapshot. Positive weights sum to one;
/// penalties are independently bounded so each input stays interpretable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ScorerWeights {
    pub version: &'static str,
    pub w_choice: f64,
    pub w_applicable: f64,
    pub w_success: f64,
    pub w_gain: f64,
    pub w_risk: f64,
    pub w_uncertainty: f64,
    pub w_cost: f64,
    pub w_latency: f64,
    pub w_rehearsal: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ScoreError {
    #[error("unsupported scorer version")]
    Version,
    #[error("invalid scorer weight budget")]
    Weights,
    #[error("non-finite prediction or execution estimate")]
    NonFinite,
}

impl ScorerWeights {
    pub(crate) fn effective(config: &ScoringConfig) -> Result<Self, ScoreError> {
        if config.version.as_deref().is_some_and(|version| version != VERSION) {
            return Err(ScoreError::Version);
        }
        let overrides = &config.weights;
        let weights = Self {
            version: VERSION,
            w_choice: overrides.w_choice.unwrap_or(0.30),
            w_applicable: overrides.w_applicable.unwrap_or(0.15),
            w_success: overrides.w_success.unwrap_or(0.30),
            w_gain: overrides.w_gain.unwrap_or(0.25),
            w_risk: overrides.w_risk.unwrap_or(0.35),
            w_uncertainty: overrides.w_uncertainty.unwrap_or(0.15),
            w_cost: overrides.w_cost.unwrap_or(0.10),
            w_latency: overrides.w_latency.unwrap_or(0.10),
            w_rehearsal: overrides.w_rehearsal.unwrap_or(0.10),
        };
        let values = [
            weights.w_choice,
            weights.w_applicable,
            weights.w_success,
            weights.w_gain,
            weights.w_risk,
            weights.w_uncertainty,
            weights.w_cost,
            weights.w_latency,
            weights.w_rehearsal,
        ];
        let positive = weights.w_choice + weights.w_applicable + weights.w_success + weights.w_gain;
        if values
            .iter()
            .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
            || (positive - 1.0).abs() > 1e-6
            || weights.w_risk == 0.0
        {
            return Err(ScoreError::Weights);
        }
        Ok(weights)
    }
}

/// Trusted costs and bounded read-only rehearsal evidence, normalized by
/// the scorer against this turn's remaining budgets.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScoreInputs {
    pub estimated_micro_usd: u64,
    pub remaining_micro_usd: u64,
    pub estimated_latency_ms: u64,
    pub remaining_latency_ms: u64,
    pub rehearsal_delta: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CandidateScore {
    pub semantic_utility: f64,
    pub execution_utility: f64,
}

pub(crate) struct CausalBranchScorer;

impl CausalBranchScorer {
    pub(crate) fn score(
        prediction: CandidatePrediction,
        inputs: ScoreInputs,
        weights: &ScorerWeights,
    ) -> Result<CandidateScore, ScoreError> {
        let features = [
            prediction.choice_probability,
            prediction.applicability_probability,
            prediction.success_probability,
            prediction.expected_gain,
            prediction.gain_uncertainty,
            prediction.semantic_risk,
            prediction.risk_uncertainty,
            inputs.rehearsal_delta,
        ];
        if features.iter().any(|value| !value.is_finite()) {
            return Err(ScoreError::NonFinite);
        }
        let unit = |value: f64| value.clamp(0.0, 1.0);
        let uncertainty = f64::midpoint(unit(prediction.gain_uncertainty), unit(prediction.risk_uncertainty));
        let mut semantic = 0.0_f64;
        for (weight, feature) in [
            (weights.w_choice, prediction.choice_probability),
            (weights.w_applicable, prediction.applicability_probability),
            (weights.w_success, prediction.success_probability),
            (weights.w_gain, prediction.expected_gain),
            (-weights.w_risk, prediction.semantic_risk),
            (-weights.w_uncertainty, uncertainty),
        ] {
            semantic = weight.mul_add(unit(feature), semantic);
        }
        let cost = normalized_ratio(inputs.estimated_micro_usd, inputs.remaining_micro_usd);
        let latency = normalized_ratio(inputs.estimated_latency_ms, inputs.remaining_latency_ms);
        let mut execution = semantic;
        for (weight, feature) in [
            (-weights.w_cost, cost),
            (-weights.w_latency, latency),
            (weights.w_rehearsal, inputs.rehearsal_delta.clamp(-1.0, 1.0)),
        ] {
            execution = weight.mul_add(feature, execution);
        }
        if !semantic.is_finite() || !execution.is_finite() {
            return Err(ScoreError::NonFinite);
        }
        Ok(CandidateScore {
            semantic_utility: semantic.clamp(-1.0, 1.0),
            execution_utility: execution.clamp(-1.0, 1.0),
        })
    }
}

fn normalized_ratio(estimate: u64, remaining: u64) -> f64 {
    if remaining == 0 {
        f64::from(estimate != 0)
    } else {
        (estimate as f64 / remaining as f64).min(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::causal_tree::branch::{CandidateSpec, CausalCandidate};
    use crate::causal_tree::candidate_generator::CandidateSet;
    use crate::causal_tree::gates::GateReport;
    use crate::causal_tree::questions::{CompilerPolicy, QuestionCompiler};
    use crate::causal_tree::state::fixtures;
    use crate::decision::Probability;
    use crate::decision::builtin::BuiltinDecisionBackend;
    use crate::decision::{DecisionResponseValidator, ModelVersionPolicy, RequestLimits, ValidationContext};
    use proptest::prelude::*;
    use std::time::{Duration, Instant};

    fn prediction() -> CandidatePrediction {
        CandidatePrediction {
            choice_probability: 0.7,
            applicability_probability: 0.6,
            success_probability: 0.8,
            expected_gain: 0.5,
            gain_uncertainty: 0.2,
            semantic_risk: 0.3,
            risk_uncertainty: 0.4,
        }
    }

    fn inputs() -> ScoreInputs {
        ScoreInputs {
            estimated_micro_usd: 25,
            remaining_micro_usd: 100,
            estimated_latency_ms: 20,
            remaining_latency_ms: 100,
            rehearsal_delta: 0.0,
        }
    }

    #[test]
    fn scorer_defaults_have_valid_positive_budget() {
        let weights = ScorerWeights::effective(&ScoringConfig::default());
        assert!(weights.is_ok());
        assert_eq!(weights.ok().map(|value| value.version), Some(VERSION));
    }

    #[test]
    fn risk_and_uncertainty_only_lower_execution_utility() {
        let weights = ScorerWeights::effective(&ScoringConfig::default()).unwrap_or_else(|error| panic!("{error}"));
        let baseline =
            CausalBranchScorer::score(prediction(), inputs(), &weights).unwrap_or_else(|error| panic!("{error}"));
        let mut higher = prediction();
        higher.semantic_risk += 0.2;
        higher.gain_uncertainty += 0.2;
        let changed = CausalBranchScorer::score(higher, inputs(), &weights).unwrap_or_else(|error| panic!("{error}"));
        assert!(changed.execution_utility <= baseline.execution_utility);
    }

    #[test]
    fn each_risk_and_uncertainty_coordinate_lowers_an_interior_score() {
        let weights = ScorerWeights::effective(&ScoringConfig::default()).expect("valid default weights");
        let original = CandidatePrediction {
            choice_probability: 0.5,
            applicability_probability: 0.5,
            success_probability: 0.5,
            expected_gain: 0.5,
            semantic_risk: 0.2,
            gain_uncertainty: 0.2,
            risk_uncertainty: 0.2,
        };
        let baseline = CausalBranchScorer::score(original, inputs(), &weights).expect("finite baseline");
        assert!(-1.0 < baseline.execution_utility && baseline.execution_utility < 1.0);
        for raised in [
            CandidatePrediction {
                semantic_risk: 0.4,
                ..original
            },
            CandidatePrediction {
                gain_uncertainty: 0.4,
                ..original
            },
            CandidatePrediction {
                risk_uncertainty: 0.4,
                ..original
            },
        ] {
            let changed = CausalBranchScorer::score(raised, inputs(), &weights).expect("finite changed score");
            assert!(-1.0 < changed.execution_utility && changed.execution_utility < 1.0);
            assert!(changed.execution_utility < baseline.execution_utility);
        }
    }

    #[test]
    fn scorer_rejects_nan_and_invalid_weight_budget() {
        let mut config = ScoringConfig::default();
        config.weights.w_choice = Some(0.9);
        assert_eq!(ScorerWeights::effective(&config), Err(ScoreError::Weights));
        let weights = ScorerWeights::effective(&ScoringConfig::default()).unwrap_or_else(|error| panic!("{error}"));
        let mut invalid = prediction();
        invalid.expected_gain = f64::NAN;
        assert_eq!(
            CausalBranchScorer::score(invalid, inputs(), &weights),
            Err(ScoreError::NonFinite)
        );
    }

    #[test]
    fn score_expected_and_distribution_uncertainty_are_normalized() {
        let probability = |value| Probability::new(value).unwrap_or_else(|error| panic!("{error}"));
        let answer = ScoreAnswer {
            distribution: vec![probability(0.5), probability(0.0), probability(0.5)],
            expected: 1.0,
            legend: Vec::new(),
            confidence: probability(0.4),
        };
        let (gain, uncertainty) = normalized_score(&answer);
        assert_eq!(gain, 0.5);
        assert_eq!(uncertainty, 1.0);
    }

    #[test]
    fn trusted_risk_floor_covers_every_effect_class() {
        for (effect, expected) in [
            (SideEffectClass::ReadOnly, 0.0),
            (SideEffectClass::LocalWrite, 0.3),
            (SideEffectClass::ProcessExec, 0.4),
            (SideEffectClass::ExternalSend, 0.5),
            (SideEffectClass::CredentialOrConfig, 0.6),
            (SideEffectClass::Destructive, 0.7),
        ] {
            assert_eq!(risk_floor(effect), expected);
        }
    }

    #[test]
    fn validated_choice_confidence_is_kept_out_of_candidate_features() {
        let key = fixtures::key();
        let state = fixtures::state(&key, "Find the project note");
        let candidates: Vec<_> = state
            .derived
            .candidates()
            .iter()
            .map(|item| item.candidate.clone())
            .collect();
        let set = CandidateSet::Legal {
            candidates: candidates.clone(),
            exact_mentions: Default::default(),
            truncated: Vec::new(),
            discoverable_families: Default::default(),
            approval_scopes: Default::default(),
            report: GateReport {
                outcomes: Default::default(),
                gate_set_hash: [0; 32],
            },
        };
        let compiled = QuestionCompiler::compile(
            &state,
            &set,
            &CompilerPolicy {
                key: &key,
                model: None,
                expires_at: Instant::now() + Duration::from_secs(30),
                limits: RequestLimits::default(),
            },
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let response = BuiltinDecisionBackend::answer(compiled.request()).unwrap_or_else(|error| panic!("{error}"));
        let mut changed_response = response.clone();
        match changed_response
            .answers
            .get_mut(&QuestionId::global(GlobalQuestion::NextAction))
        {
            Some(DecisionAnswer::Choice(choice)) => {
                choice.confidence = Probability::new(0.01).unwrap_or_else(|error| panic!("{error}"));
            }
            _ => panic!("expected action choice"),
        }
        let context = ValidationContext {
            received_at: Instant::now(),
            max_response_bytes: 256 * 1024,
            version_policy: ModelVersionPolicy::Unpinned,
            local_backend: BuiltinDecisionBackend::identity(),
        };
        let validated = DecisionResponseValidator::validate(compiled.request(), response, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        let evidence =
            PredictionEvidence::extract(&validated, &compiled, &set).unwrap_or_else(|error| panic!("{error}"));
        let changed_validated = DecisionResponseValidator::validate(compiled.request(), changed_response, &context)
            .unwrap_or_else(|error| panic!("{error}"));
        let changed_evidence =
            PredictionEvidence::extract(&changed_validated, &compiled, &set).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(evidence.candidates, changed_evidence.candidates);
        assert_eq!(changed_evidence.action_confidence, 0.01);
        let total: f64 = evidence.candidates.values().map(|item| item.choice_probability).sum();
        assert!((total - 1.0).abs() < 1e-10);
        assert!(evidence.action_confidence.is_finite());
        assert!(evidence.target_confidence.is_finite());
        assert!(
            evidence
                .candidates
                .values()
                .all(|item| (0.0..=1.0).contains(&item.choice_probability))
        );
        assert!(evidence.candidates.values().all(|item| item.semantic_risk >= 0.0));

        let another_request = QuestionCompiler::compile(
            &state,
            &set,
            &CompilerPolicy {
                key: &key,
                model: None,
                expires_at: Instant::now() + Duration::from_secs(30),
                limits: RequestLimits::default(),
            },
        )
        .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(
            PredictionEvidence::extract(&validated, &another_request, &set),
            Err(ExtractionError::RequestMismatch)
        ));

        let mut altered_candidates = candidates;
        let first = altered_candidates
            .first()
            .unwrap_or_else(|| panic!("expected a candidate"));
        let altered = CausalCandidate::new(
            first.action(),
            first.target(),
            CandidateSpec {
                namespace: None,
                required_capabilities: first.required_capabilities().to_vec(),
                side_effect: SideEffectClass::LocalWrite,
                approval: first.approval().clone(),
                estimated_cost: first.estimated_cost(),
                estimated_latency_ms: first.estimated_latency_ms(),
            },
        );
        assert_eq!(altered.candidate_id(), first.candidate_id());
        if let Some(first) = altered_candidates.first_mut() {
            *first = altered;
        }
        let altered_set = CandidateSet::Legal {
            candidates: altered_candidates,
            exact_mentions: Default::default(),
            truncated: Vec::new(),
            discoverable_families: Default::default(),
            approval_scopes: Default::default(),
            report: GateReport {
                outcomes: Default::default(),
                gate_set_hash: [0; 32],
            },
        };
        assert!(matches!(
            PredictionEvidence::extract(&validated, &compiled, &altered_set),
            Err(ExtractionError::CandidateMismatch)
        ));
    }

    proptest! {
        #[test]
        fn risk_and_uncertainty_are_score_monotone(
            choice in 0.0_f64..=1.0,
            applicable in 0.0_f64..=1.0,
            success in 0.0_f64..=1.0,
            gain in 0.0_f64..=1.0,
            risk in 0.0_f64..=1.0,
            gain_uncertainty in 0.0_f64..=1.0,
            risk_uncertainty in 0.0_f64..=1.0,
            increment in 0.0_f64..=1.0,
        ) {
            let weights = ScorerWeights::effective(&ScoringConfig::default())
                .unwrap_or_else(|error| panic!("{error}"));
            let baseline_prediction = CandidatePrediction {
                choice_probability: choice,
                applicability_probability: applicable,
                success_probability: success,
                expected_gain: gain,
                semantic_risk: risk,
                gain_uncertainty,
                risk_uncertainty,
            };
            let baseline = CausalBranchScorer::score(baseline_prediction, inputs(), &weights)
                .unwrap_or_else(|error| panic!("{error}"));
            for value in [baseline.semantic_utility, baseline.execution_utility] {
                prop_assert!(value.is_finite());
                prop_assert!((-1.0..=1.0).contains(&value));
            }
            // V3.06 holds every other input fixed. Joint changes could hide
            // an incorrectly positive penalty behind another negative one.
            for raised in [
                CandidatePrediction {
                    semantic_risk: (risk + increment).min(1.0),
                    ..baseline_prediction
                },
                CandidatePrediction {
                    gain_uncertainty: (gain_uncertainty + increment).min(1.0),
                    ..baseline_prediction
                },
                CandidatePrediction {
                    risk_uncertainty: (risk_uncertainty + increment).min(1.0),
                    ..baseline_prediction
                },
                // Keep the original simultaneous-change regression as well.
                CandidatePrediction {
                    semantic_risk: (risk + increment).min(1.0),
                    gain_uncertainty: (gain_uncertainty + increment).min(1.0),
                    risk_uncertainty: (risk_uncertainty + increment).min(1.0),
                    ..baseline_prediction
                },
            ] {
                let changed = CausalBranchScorer::score(raised, inputs(), &weights)
                    .unwrap_or_else(|error| panic!("{error}"));
                for value in [changed.semantic_utility, changed.execution_utility] {
                    prop_assert!(value.is_finite());
                    prop_assert!((-1.0..=1.0).contains(&value));
                }
                prop_assert!(changed.execution_utility <= baseline.execution_utility + f64::EPSILON);
            }
        }
    }
}

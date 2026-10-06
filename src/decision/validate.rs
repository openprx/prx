//! The decision response validator (CDP section 8, CDP-PLAN 1.4).
//!
//! [`DecisionResponseValidator::validate`] checks a decoded response against
//! the sealed request it answers and the local context of the attempt. The
//! rules run in a fixed order and the first failure rejects the whole
//! response. Nothing is repaired: a [`ValidatedDecision`] holds exactly the
//! values it was given (AG-22), and it can only be built here (AG-23).
//!
//! Tolerances follow D-042 C-1 as amended by D-057 M-2. System One rounds
//! every probability to two decimals on its own, without renormalizing, so a
//! distribution whose values are all on hundredths may miss 1 by the exact
//! worst-case rounding error `n × 0.005` (uncapped); any other distribution
//! must sum to 1 within [`PROBABILITY_SUM_TOLERANCE`]. The Score expectation
//! is the mean level index, and it is compared with the distribution mean
//! within the matching rounding bound.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use super::error::{DecisionError, ValidationRule};
use sha2::{Digest as _, Sha256};

use super::protocol::{
    BackendIdentity, DecisionAnswer, DecisionQuestion, DecisionRequest, DecisionResponse, DecisionUsage,
    MAX_CLAIM_CHARS, Probability, QuestionId, StructuredValue, is_identity_text, write_canonical,
};

/// The sum tolerance for a distribution not on hundredths.
pub const PROBABILITY_SUM_TOLERANCE: f64 = 1e-3;
/// Half of the two-decimal wire step: the largest rounding error of one value.
pub const WIRE_ROUNDING_ERROR: f64 = 0.005;
/// How far below the largest probability the chosen option may be (a tie
/// after rounding is valid; DIFFS D-07).
pub const CHOSEN_MODAL_TOLERANCE: f64 = 1e-6;
/// The longest provider request id, in bytes.
pub const MAX_PROVIDER_REQUEST_ID_BYTES: usize = 128;

/// Whether `id` is a usable provider request id: at most
/// [`MAX_PROVIDER_REQUEST_ID_BYTES`] of `[A-Za-z0-9_-]` (DIFFS D-15), which
/// also satisfies the identity character rules.
fn is_provider_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_PROVIDER_REQUEST_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Absorbs binary floating-point error in sums of decimal wire values.
const FLOAT_SLACK: f64 = 1e-9;

/// How the claimed model must relate to the configured one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelVersionPolicy {
    /// Any model claim is accepted.
    Unpinned,
    /// The claim must equal this exact model snapshot (D-043 C-8).
    ExactPinned(Arc<str>),
}

/// The local facts of one attempt.
#[derive(Debug, Clone)]
pub struct ValidationContext {
    /// When the complete response was received.
    pub received_at: Instant,
    /// The largest acceptable response body.
    pub max_response_bytes: u32,
    /// The model version policy of the attempted backend.
    pub version_policy: ModelVersionPolicy,
    /// The configured identity of the attempted backend: the audit truth.
    pub local_backend: BackendIdentity,
}

/// A response that satisfied every invariant. Built only by
/// [`DecisionResponseValidator::validate`]; its values are bit-identical to
/// the response it was built from.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedDecision {
    answers: Arc<BTreeMap<QuestionId, DecisionAnswer>>,
    identity: BackendIdentity,
    claimed_backend: String,
    claimed_model: String,
    usage: DecisionUsage,
    provider_request_id: Option<String>,
    request_id: String,
    response_sha256: Option<[u8; 32]>,
}

impl ValidatedDecision {
    /// The answer to question `id`.
    #[must_use]
    pub fn answer(&self, id: &QuestionId) -> Option<&DecisionAnswer> {
        self.answers.get(id)
    }

    /// The configured identity of the backend that answered (audit truth).
    #[must_use]
    pub const fn identity(&self) -> &BackendIdentity {
        &self.identity
    }

    /// The backend name the response claimed; never a label or audit truth.
    #[must_use]
    pub fn claimed_backend(&self) -> &str {
        &self.claimed_backend
    }

    /// The model the response claimed.
    #[must_use]
    pub fn claimed_model(&self) -> &str {
        &self.claimed_model
    }

    /// Usage accounting.
    #[must_use]
    pub const fn usage(&self) -> &DecisionUsage {
        &self.usage
    }

    /// The provider's diagnostic request id.
    #[must_use]
    pub fn provider_request_id(&self) -> Option<&str> {
        self.provider_request_id.as_deref()
    }

    /// The id of the request this decision answers.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// SHA-256 of the raw response body; `None` for a backend without one.
    #[must_use]
    pub const fn response_sha256(&self) -> Option<[u8; 32]> {
        self.response_sha256
    }

    /// This decision served again from the cache for the request
    /// `request_id` (D-079 L-9): the answers and the backend identity are
    /// the cached ones; the request id is the current request's, and the
    /// usage and provider request id are cleared, since no backend was
    /// called for it.
    #[must_use]
    pub fn served_from_cache(&self, request_id: &str) -> Self {
        Self {
            usage: DecisionUsage::default(),
            provider_request_id: None,
            request_id: request_id.to_string(),
            ..self.clone()
        }
    }
}

/// The one validator of decision responses.
#[derive(Debug, Clone, Copy)]
pub struct DecisionResponseValidator;

impl DecisionResponseValidator {
    /// Validate `response` against `request` (and its manifest) and `ctx`.
    ///
    /// Rules, in order: identity claims present, bounded and free of control
    /// and bidirectional characters; not past the manifest deadline; within
    /// the size limit; exactly one answer per question; the question's
    /// primitive; Choice options inside the criteria; a probability for every
    /// offered option; distributions summing to 1 within tolerance; the chosen
    /// option modal; the Score legend and distribution matching the levels;
    /// the Score expectation in range and equal to the mean; the pinned model
    /// version; and a wire body not already validated against this request
    /// (duplicate delivery). Every probability is range-checked by
    /// [`Probability`] itself.
    ///
    /// A provider request id that is not a bounded `[A-Za-z0-9_-]` token is
    /// diagnostic only: it is dropped with a warning, and the decision stands
    /// (D-057 L-8).
    pub fn validate(
        request: &DecisionRequest,
        response: DecisionResponse,
        ctx: &ValidationContext,
    ) -> Result<ValidatedDecision, DecisionError> {
        let reject = |rule| Err(DecisionError::InvalidResponse(rule));
        let manifest = request.manifest();

        for claim in [&response.backend, &response.resolved_model] {
            if claim.trim().is_empty() {
                return reject(ValidationRule::IdentityMissing);
            }
            if claim.chars().count() > MAX_CLAIM_CHARS {
                return reject(ValidationRule::IdentityTooLong);
            }
            if !is_identity_text(claim) {
                return reject(ValidationRule::IdentityCharacters);
            }
        }
        if ctx.received_at > manifest.expires_at() {
            return reject(ValidationRule::Expired);
        }
        if response.raw_bytes > ctx.max_response_bytes {
            return reject(ValidationRule::Oversize);
        }
        let questions = request.questions();
        if questions.keys().any(|id| !response.answers.contains_key(id)) {
            return reject(ValidationRule::AnswerMissing);
        }
        if response.answers.keys().any(|id| !questions.contains_key(id)) {
            return reject(ValidationRule::AnswerExtra);
        }

        let pairs: Vec<(&DecisionQuestion, &DecisionAnswer)> = questions
            .iter()
            .filter_map(|(id, question)| response.answers.get(id).map(|answer| (question, answer)))
            .collect();
        for rule in ORDERED_ANSWER_RULES {
            if pairs
                .iter()
                .any(|(question, answer)| !satisfies(rule, question, answer))
            {
                return reject(rule);
            }
        }

        if let ModelVersionPolicy::ExactPinned(pinned) = &ctx.version_policy {
            if response.resolved_model != pinned.as_ref() {
                return Err(DecisionError::ModelVersionMismatch);
            }
        }

        // Last, so that only a response that passed every other rule is
        // recorded: the same decoded content from a wire backend is accepted
        // once per sealed request, whatever its encoding (D-057 amendment 2,
        // N-2). Typed responses (no wire body: builtin, mock) are not
        // tracked, so the builtin shadow never collides with a wire answer.
        if response.raw_bytes > 0 && !request.record_delivery(delivery_digest(&response)) {
            return reject(ValidationRule::DuplicateDelivery);
        }

        let DecisionResponse {
            backend,
            resolved_model,
            answers,
            usage,
            provider_request_id,
            raw_bytes,
            response_sha256,
            ..
        } = response;
        let provider_request_id = provider_request_id.filter(|id| {
            let usable = is_provider_request_id(id);
            if !usable {
                tracing::warn!(
                    rule = "provider_request_id_malformed",
                    request_id = %manifest.request_id(),
                    "decision response carried a malformed provider request id; it was dropped"
                );
            }
            usable
        });
        Ok(ValidatedDecision {
            answers: Arc::new(answers),
            identity: ctx.local_backend.clone(),
            claimed_backend: backend,
            claimed_model: resolved_model,
            usage,
            provider_request_id,
            request_id: manifest.request_id().to_string(),
            response_sha256: (raw_bytes > 0).then_some(response_sha256),
        })
    }
}

/// The delivery digest of a response: SHA-256 over a canonical encoding of
/// what the response decides, namely the claimed backend and model and every
/// answer with its exact values (probability bits, the legend as canonical
/// JSON). Formatting, key order, unknown fields, usage and the provider
/// request id do not change it, so one logical response re-encoded is still
/// one delivery.
fn delivery_digest(response: &DecisionResponse) -> [u8; 32] {
    fn part(hasher: &mut Sha256, bytes: &[u8]) {
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    fn number(hasher: &mut Sha256, value: f64) {
        hasher.update(value.to_bits().to_be_bytes());
    }
    let mut hasher = Sha256::new();
    hasher.update(b"prx.decision.delivery.v1\0");
    part(&mut hasher, response.backend.as_bytes());
    part(&mut hasher, response.resolved_model.as_bytes());
    for (id, answer) in &response.answers {
        part(&mut hasher, id.as_str().as_bytes());
        match answer {
            DecisionAnswer::Choice(choice) => {
                part(&mut hasher, b"choice");
                part(&mut hasher, choice.chosen.as_str().as_bytes());
                for (option, probability) in &choice.probabilities {
                    part(&mut hasher, option.as_str().as_bytes());
                    number(&mut hasher, probability.get());
                }
                number(&mut hasher, choice.confidence.get());
            }
            DecisionAnswer::Score(score) => {
                part(&mut hasher, b"score");
                for probability in &score.distribution {
                    number(&mut hasher, probability.get());
                }
                number(&mut hasher, score.expected);
                for level in &score.legend {
                    let mut canonical = String::new();
                    write_canonical(level.as_json(), &mut canonical);
                    part(&mut hasher, canonical.as_bytes());
                }
                number(&mut hasher, score.confidence.get());
            }
            DecisionAnswer::Noul(noul) => {
                part(&mut hasher, b"noul");
                number(&mut hasher, noul.probability_true.get());
            }
        }
    }
    hasher.finalize().into()
}

/// The per-answer rules, in check order.
const ORDERED_ANSWER_RULES: [ValidationRule; 7] = [
    ValidationRule::WrongPrimitive,
    ValidationRule::UnknownOption,
    ValidationRule::ProbabilityMissing,
    ValidationRule::ProbabilitySum,
    ValidationRule::ChosenNotModal,
    ValidationRule::LegendMismatch,
    ValidationRule::ExpectedOutOfRange,
];

/// Whether one answer satisfies `rule` for its question. A rule that does
/// not concern the pair's primitive is satisfied; so is every rule once the
/// primitives differ, because `WrongPrimitive` runs first.
fn satisfies(rule: ValidationRule, question: &DecisionQuestion, answer: &DecisionAnswer) -> bool {
    match (question, answer) {
        (DecisionQuestion::Choice { criteria, .. }, DecisionAnswer::Choice(choice)) => match rule {
            ValidationRule::UnknownOption => {
                criteria.contains_key(&choice.chosen)
                    && choice.probabilities.keys().all(|option| criteria.contains_key(option))
            }
            ValidationRule::ProbabilityMissing => {
                criteria.keys().all(|option| choice.probabilities.contains_key(option))
            }
            ValidationRule::ProbabilitySum => sums_to_one(choice.probabilities.values().copied()),
            ValidationRule::ChosenNotModal => {
                let chosen = choice.probabilities.get(&choice.chosen).map_or(f64::NAN, |p| p.get());
                let highest = choice.probabilities.values().map(|p| p.get()).fold(0.0_f64, f64::max);
                chosen >= highest - CHOSEN_MODAL_TOLERANCE
            }
            _ => true,
        },
        (DecisionQuestion::Score { levels, .. }, DecisionAnswer::Score(score)) => match rule {
            ValidationRule::ProbabilitySum => sums_to_one(score.distribution.iter().copied()),
            ValidationRule::LegendMismatch => {
                score.distribution.len() == levels.len()
                    && score.legend.len() == levels.len()
                    && score
                        .legend
                        .iter()
                        .zip(levels)
                        .all(|(echoed, level)| same_json(echoed, level))
            }
            ValidationRule::ExpectedOutOfRange => expectation_matches(&score.distribution, score.expected),
            _ => true,
        },
        (DecisionQuestion::Noul { .. }, DecisionAnswer::Noul(_)) => true,
        _ => rule != ValidationRule::WrongPrimitive,
    }
}

/// Whether every value is a whole number of hundredths.
fn on_hundredths(value: f64) -> bool {
    let scaled = value * 100.0;
    (scaled - scaled.round()).abs() <= 1e-6
}

/// Whether a distribution sums to 1 as closely as its encoding allows.
///
/// A distribution not on hundredths must sum to 1 within
/// [`PROBABILITY_SUM_TOLERANCE`]. A distribution on hundredths was rounded
/// value by value (each value moves by at most [`WIRE_ROUNDING_ERROR`]), and
/// a value rounded to 0 can only have been rounded down. So its sum lies in
/// `[1 − 0.005·n, 1 + 0.005·nnz]`, where `nnz` counts the nonzero values
/// (D-057 amendment 2, N-1). The all-zero distribution of 200 or more
/// options is inside this bound; selector streams treat it as abstention.
fn sums_to_one(values: impl Iterator<Item = Probability> + Clone) -> bool {
    let count = values.clone().count();
    let nonzero = values.clone().filter(|value| value.get() > 0.0).count();
    let sum: f64 = values.clone().map(Probability::get).sum();
    let (below, above) = if values.map(Probability::get).all(on_hundredths) {
        (count as f64 * WIRE_ROUNDING_ERROR, nonzero as f64 * WIRE_ROUNDING_ERROR)
    } else {
        (PROBABILITY_SUM_TOLERANCE, PROBABILITY_SUM_TOLERANCE)
    };
    sum >= 1.0 - below - FLOAT_SLACK && sum <= 1.0 + above + FLOAT_SLACK
}

/// Whether `expected` lies in `[0, n-1]` and equals the mean level index of
/// `distribution` within the rounding bound: `0.005` for the rounded
/// expectation itself, plus `Σ |i − mean| × 0.005` when the distribution is on
/// hundredths (each rounded `p_i` moves the mean by at most that much).
fn expectation_matches(distribution: &[Probability], expected: f64) -> bool {
    let highest = distribution.len().saturating_sub(1) as f64;
    if !(expected.is_finite() && (0.0..=highest).contains(&expected)) {
        return false;
    }
    let mass: f64 = distribution.iter().map(|p| p.get()).sum();
    if mass <= 0.0 {
        return false;
    }
    let mean = distribution
        .iter()
        .enumerate()
        .map(|(index, p)| index as f64 * p.get())
        .sum::<f64>()
        / mass;
    let rounding = if distribution.iter().all(|p| on_hundredths(p.get())) {
        distribution
            .iter()
            .enumerate()
            .map(|(index, _)| (index as f64 - mean).abs() * WIRE_ROUNDING_ERROR)
            .sum::<f64>()
    } else {
        0.0
    };
    (expected - mean).abs() <= rounding + WIRE_ROUNDING_ERROR + FLOAT_SLACK
}

/// JSON equality with numbers compared by value (`1` equals `1.0`) and object
/// keys compared as sets.
fn same_json(left: &StructuredValue, right: &StructuredValue) -> bool {
    json_equal(left.as_json(), right.as_json())
}

fn json_equal(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => match (a.as_i64(), b.as_i64(), a.as_u64(), b.as_u64()) {
            (Some(x), Some(y), _, _) => x == y,
            (_, _, Some(x), Some(y)) => x == y,
            _ => a.as_f64().is_some_and(|x| b.as_f64().is_some_and(|y| x == y)),
        },
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| json_equal(x, y)),
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len() && a.iter().all(|(key, x)| b.get(key).is_some_and(|y| json_equal(x, y)))
        }
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod sharing;

    fn probabilities(values: &[f64]) -> Vec<Probability> {
        values
            .iter()
            .map(|value| Probability::new(*value).expect("test: probability"))
            .collect()
    }

    #[test]
    fn the_rounding_aware_sum_tolerance_matches_the_recorded_captures() {
        assert!(sums_to_one(probabilities(&[0.01, 0.23, 0.67, 0.08]).into_iter()));
        assert!(!sums_to_one(probabilities(&[0.01, 0.20, 0.67, 0.08]).into_iter()));
        assert!(!sums_to_one(probabilities(&[0.3, 0.6]).into_iter()));
        assert!(sums_to_one(probabilities(&[0.3333, 0.6667]).into_iter()));
        assert!(!sums_to_one(probabilities(&[0.3333, 0.6682]).into_iter()));
        // D-057 M-2: ten options rounded independently (R-CDP01 counterexamples).
        let mut low = vec![0.96];
        low.extend([0.0; 9]);
        assert!(sums_to_one(probabilities(&low).into_iter()));
        let mut high = vec![0.86];
        high.extend([0.02; 9]);
        assert!(sums_to_one(probabilities(&high).into_iter()));
        let mut beyond = vec![0.84];
        beyond.extend([0.0; 9]);
        assert!(!sums_to_one(probabilities(&beyond).into_iter()));
        // D-057 amendment 2 (N-1): above 1 only the nonzero values can have
        // been rounded up.
        let mut two_high = vec![0.60, 0.55];
        two_high.extend([0.0; 38]);
        assert!(!sums_to_one(probabilities(&two_high).into_iter()));
        let mut two_ones = vec![1.0, 1.0];
        two_ones.extend([0.0; 253]);
        assert!(!sums_to_one(probabilities(&two_ones).into_iter()));
        assert!(sums_to_one(probabilities(&[0.0; 255]).into_iter()));
        let mut one_up = vec![0.51, 0.5];
        one_up.extend([0.0; 38]);
        assert!(sums_to_one(probabilities(&one_up).into_iter()));
        let mut two_up = vec![0.52, 0.5];
        two_up.extend([0.0; 38]);
        assert!(!sums_to_one(probabilities(&two_up).into_iter()));
    }

    #[test]
    fn the_expectation_is_the_mean_level_index() {
        let rounded = probabilities(&[0.13, 0.76, 0.11]);
        assert!(expectation_matches(&rounded, 0.97));
        assert!(!expectation_matches(&rounded, 1.05));
        assert!(!expectation_matches(&rounded, -0.01));
        assert!(!expectation_matches(&rounded, f64::NAN));
        let exact = probabilities(&[0.255, 0.49, 0.255]);
        assert!(expectation_matches(&exact, 1.004));
        assert!(!expectation_matches(&exact, 1.006));
        assert!(!expectation_matches(&probabilities(&[0.0, 0.0]), 0.0));
    }

    #[test]
    fn json_equality_compares_numbers_by_value_and_ignores_key_order() {
        let value = |json| StructuredValue::try_from(json).expect("test: structured");
        assert!(same_json(
            &value(serde_json::json!({"a": [1, {"b": 2.0}], "c": "x"})),
            &value(serde_json::json!({"c": "x", "a": [1.0, {"b": 2}]}))
        ));
        assert!(!same_json(
            &value(serde_json::json!({"a": 1})),
            &value(serde_json::json!({"a": 1, "b": 1}))
        ));
        assert!(!same_json(
            &value(serde_json::json!(["x"])),
            &value(serde_json::json!("x"))
        ));
    }
}

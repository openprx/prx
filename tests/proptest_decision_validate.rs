//! Property corpus for the decision response validator (SP-CDP-C V1.06, the
//! G-09 corpus: `PROPTEST_CASES=10000 cargo test --test
//! proptest_decision_validate`).
//!
//! Properties:
//! - a validated value is bit-identical to the value received: no repair and
//!   no renormalization (AG-22);
//! - a distribution not on hundredths passes at `1 ± tol/2` and fails at
//!   `1 ± 2·tol`; a distribution on hundredths gets the uncapped rounding
//!   bound `[1 − 0.005·n, 1 + 0.005·nnz]` (D-057 M-2 and amendment 2) and
//!   nothing more;
//! - any true distribution over up to 255 options, with zeros, ones and ties,
//!   rounded to two decimals value by value, validates; so do tied and
//!   one-hot distributions;
//! - a repeated key at any depth voids the response, and salted wire ids are
//!   64-bit, distinct and bound to the request;
//! - NaN, infinities and every value outside `[0, 1]` are refused for
//!   probabilities, Noul answers and confidences, on the wire and in typed
//!   form;
//! - arbitrary mutations of a recorded response body never panic, and are
//!   either rejected as a whole or validate to exactly the decoded values.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used
)]

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use openprx::decision::protocol::MAX_CHOICE_OPTIONS;
use openprx::decision::validate::{PROBABILITY_SUM_TOLERANCE, WIRE_ROUNDING_ERROR};
use openprx::decision::wire::{ResponseEnvelope, SystemOneCodec};
use openprx::decision::{
    BackendIdentity, BackendKind, CandidateKey, CandidateQuestion, ChoiceAnswer, DecisionAnswer, DecisionError,
    DecisionKey, DecisionQuestion, DecisionRequest, DecisionResponse, DecisionResponseValidator, DecisionUsage,
    GlobalQuestion, ModelVersionPolicy, NoulAnswer, OptionId, Probability, QuestionId, RequestLimits, RequestParts,
    ScoreAnswer, StructuredValue, ValidationContext, ValidationRule,
};
use proptest::prelude::*;
use serde_json::json;

/// Option counts for the continuous-weight properties.
const MAX_OPTIONS: usize = 12;
/// Option counts for the shifted two-decimal properties: large enough that
/// the uncapped bound `n × 0.005` exceeds the old 0.02 cap, small enough that
/// a shift by the bound stays a probability.
const MAX_SHIFTED_OPTIONS: usize = 40;

struct Fixture {
    _dir: tempfile::TempDir,
    key: Arc<DecisionKey>,
    /// One Choice question with `n` options, for every `n` in
    /// `2..=MAX_CHOICE_OPTIONS`.
    choices: Vec<DecisionRequest>,
    /// One Score question with `n` levels, for every `n` in 2..=10.
    scores: Vec<DecisionRequest>,
    /// One Noul question.
    noul: DecisionRequest,
}

fn structured(value: serde_json::Value) -> StructuredValue {
    StructuredValue::try_from(value).expect("test: structured")
}

fn option(index: usize) -> OptionId {
    OptionId::candidate(&CandidateKey::from_parts(
        "use_tool",
        "mcp",
        Some(u32::try_from(index).unwrap().to_be_bytes()),
    ))
}

fn seal(key: &DecisionKey, id: GlobalQuestion, question: DecisionQuestion) -> DecisionRequest {
    DecisionRequest::seal(
        RequestParts {
            state: structured(json!({"goal": "property"})),
            questions: BTreeMap::from([(QuestionId::global(id), question)]),
            model: Some("jev-1.13.0".to_string()),
            state_fingerprint: "state".to_string(),
            candidate_set_hash: "candidates".to_string(),
            policy_version: Arc::from("policy"),
            catalog_generation: "catalog".to_string(),
            omitted_questions: Vec::new(),
            approval_pending: false,
        },
        key,
        Instant::now() + Duration::from_hours(24),
        &RequestLimits::default(),
    )
    .expect("test: seal")
}

static FIXTURE: LazyLock<Fixture> = LazyLock::new(|| {
    let dir = tempfile::tempdir().expect("test: tempdir");
    let key = DecisionKey::load_or_create(dir.path()).expect("test: key");
    let choices = (2..=MAX_CHOICE_OPTIONS)
        .map(|n| {
            let question = DecisionQuestion::Choice {
                instructions: structured(json!("Which?")),
                criteria: (0..n)
                    .map(|i| (option(i), structured(json!(format!("option {i}")))))
                    .collect(),
            };
            seal(&key, GlobalQuestion::NextAction, question)
        })
        .collect();
    let scores = (2..=10)
        .map(|n| {
            let question = DecisionQuestion::Score {
                instructions: structured(json!("How much?")),
                levels: (0..n).map(|i| structured(json!(format!("level {i}")))).collect(),
            };
            seal(&key, GlobalQuestion::SemanticRisk, question)
        })
        .collect();
    let noul = seal(
        &key,
        GlobalQuestion::EvidenceSufficient,
        DecisionQuestion::Noul {
            instructions: structured(json!("Is it?")),
            criteria: None,
        },
    );
    Fixture {
        _dir: dir,
        key,
        choices,
        scores,
        noul,
    }
});

fn context() -> ValidationContext {
    ValidationContext {
        received_at: Instant::now(),
        max_response_bytes: 256 * 1024,
        version_policy: ModelVersionPolicy::Unpinned,
        local_backend: BackendIdentity {
            kind: BackendKind::Builtin,
            provider: None,
            configured_model: Arc::from("builtin-v1"),
        },
    }
}

fn response(answers: BTreeMap<QuestionId, DecisionAnswer>) -> DecisionResponse {
    DecisionResponse {
        backend: "builtin".to_string(),
        resolved_model: "builtin-v1".to_string(),
        answers,
        usage: DecisionUsage::default(),
        provider_request_id: None,
        elapsed_ms: 0,
        raw_bytes: 0,
        response_sha256: [0; 32],
    }
}

fn on_hundredths(value: f64) -> bool {
    let scaled = value * 100.0;
    (scaled - scaled.round()).abs() <= 1e-6
}

/// Positive weights normalized to sum 1, then the first entry shifted by
/// `offset`: a distribution whose sum is `1 + offset` (up to rounding).
fn distribution(weights: &[f64], offset: f64) -> Vec<f64> {
    let total: f64 = weights.iter().sum();
    let mut values: Vec<f64> = weights.iter().map(|weight| weight / total).collect();
    values[0] += offset;
    values
}

fn choice_answer(values: &[f64], confidence: f64) -> Result<DecisionAnswer, DecisionError> {
    let modal = values.iter().enumerate().fold(
        0,
        |best, (index, value)| if *value > values[best] { index } else { best },
    );
    Ok(DecisionAnswer::Choice(ChoiceAnswer {
        chosen: option(modal),
        probabilities: values
            .iter()
            .enumerate()
            .map(|(index, value)| Probability::new(*value).map(|p| (option(index), p)))
            .collect::<Result<_, _>>()?,
        confidence: Probability::new(confidence)?,
    }))
}

fn validate_choice(values: &[f64], confidence: f64) -> Result<(), DecisionError> {
    let request = &FIXTURE.choices[values.len() - 2];
    let answer = choice_answer(values, confidence)?;
    let id = QuestionId::global(GlobalQuestion::NextAction);
    let decision = DecisionResponseValidator::validate(
        request,
        response(BTreeMap::from([(id.clone(), answer.clone())])),
        &context(),
    )?;
    assert_eq!(decision.answer(&id), Some(&answer), "validated values equal the input");
    Ok(())
}

fn score_answer(values: &[f64], shift: f64) -> Result<DecisionAnswer, DecisionError> {
    let mass: f64 = values.iter().sum();
    let mean = values.iter().enumerate().map(|(i, p)| i as f64 * p).sum::<f64>() / mass;
    Ok(DecisionAnswer::Score(ScoreAnswer {
        distribution: values
            .iter()
            .map(|value| Probability::new(*value))
            .collect::<Result<_, _>>()?,
        expected: mean + shift,
        legend: (0..values.len())
            .map(|i| structured(json!(format!("level {i}"))))
            .collect(),
        confidence: Probability::new(0.5)?,
    }))
}

fn validate_score(values: &[f64], shift: f64) -> Result<(), DecisionError> {
    let request = &FIXTURE.scores[values.len() - 2];
    let answer = score_answer(values, shift)?;
    let id = QuestionId::global(GlobalQuestion::SemanticRisk);
    let decision = DecisionResponseValidator::validate(
        request,
        response(BTreeMap::from([(id.clone(), answer.clone())])),
        &context(),
    )?;
    let Some(DecisionAnswer::Score(validated)) = decision.answer(&id) else {
        panic!("score answer missing")
    };
    let DecisionAnswer::Score(input) = &answer else {
        unreachable!()
    };
    assert_eq!(validated.expected.to_bits(), input.expected.to_bits());
    for (left, right) in validated.distribution.iter().zip(&input.distribution) {
        assert_eq!(left.get().to_bits(), right.get().to_bits());
    }
    Ok(())
}

fn weights(max: usize) -> impl Strategy<Value = Vec<f64>> {
    prop::collection::vec(0.05_f64..1.0, 2..=max)
}

/// A distribution on hundredths: `n` non-negative multiples of 0.01 summing
/// to exactly 100 hundredths.
fn hundredths(max: usize) -> impl Strategy<Value = Vec<u32>> {
    prop::collection::vec(1_u32..60, 2..=max).prop_map(|parts| {
        let total: u32 = parts.iter().sum();
        let mut scaled: Vec<u32> = parts.iter().map(|part| part * 100 / total).collect();
        let short = 100 - scaled.iter().sum::<u32>();
        scaled[0] += short;
        scaled
    })
}

/// Non-negative weights over up to `max` options with zeros, ones and ties
/// likely, and a positive total.
fn true_distribution(max: usize) -> impl Strategy<Value = Vec<f64>> {
    let weight = prop_oneof![
        4 => 0.0_f64..1.0,
        2 => Just(0.0),
        1 => Just(1.0),
        1 => Just(0.5),
    ];
    prop::collection::vec(weight, 2..=max).prop_map(|mut weights| {
        if weights.iter().all(|weight| *weight == 0.0) {
            weights[0] = 1.0;
        }
        weights
    })
}

/// Two-decimal values (in hundredths, 0..=100) over up to `max` options,
/// mostly zero, so sums land on both sides of both bounds.
fn sparse_hundredths(max: usize) -> impl Strategy<Value = Vec<u32>> {
    let part = prop_oneof![
        6 => Just(0_u32),
        2 => 0_u32..=5,
        2 => 0_u32..=100,
    ];
    prop::collection::vec(part, 2..=max)
}

fn out_of_range() -> impl Strategy<Value = f64> {
    prop_oneof![
        Just(f64::NAN),
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
        Just(-0.0001),
        Just(1.0001),
        -1e9_f64..-1e-12,
        (1.0_f64 + 1e-12)..1e9,
    ]
}

proptest! {
    /// Every value that validates is the exact value received.
    #[test]
    fn validated_values_equal_input_bits(raw in weights(MAX_OPTIONS), confidence in 0.0_f64..=1.0) {
        let values = distribution(&raw, 0.0);
        prop_assert_eq!(validate_choice(&values, confidence), Ok(()));
    }

    /// A distribution not on hundredths passes within half the tolerance and
    /// fails at twice the tolerance, above and below.
    #[test]
    fn probabilities_sum_within_tolerance(raw in weights(MAX_OPTIONS), above in any::<bool>()) {
        let sign = if above { 1.0 } else { -1.0 };
        let inside = distribution(&raw, sign * PROBABILITY_SUM_TOLERANCE / 2.0);
        let outside = distribution(&raw, sign * PROBABILITY_SUM_TOLERANCE * 2.0);
        prop_assume!(!inside.iter().all(|value| on_hundredths(*value)));
        prop_assume!(!outside.iter().all(|value| on_hundredths(*value)));
        prop_assume!(outside.iter().chain(&inside).all(|value| (0.0..=1.0).contains(value)));
        prop_assert_eq!(validate_choice(&inside, 0.5), Ok(()));
        prop_assert_eq!(
            validate_choice(&outside, 0.5),
            Err(DecisionError::InvalidResponse(ValidationRule::ProbabilitySum))
        );
    }

    /// A distribution on hundredths may miss 1 by the rounding bound and no
    /// more.
    #[test]
    fn rounded_probabilities_get_the_rounding_bound(
        parts in hundredths(MAX_SHIFTED_OPTIONS),
        above in any::<bool>(),
    ) {
        let n = parts.len();
        // Below 1 every value may have been rounded down; above 1 only the
        // nonzero ones can have been rounded up (D-057 amendment 2, N-1). The
        // shift touches the largest value, which stays nonzero going up.
        let nonzero = parts.iter().filter(|part| **part > 0).count();
        let counted = if above { nonzero } else { n };
        let bound_hundredths = (counted as f64 * WIRE_ROUNDING_ERROR * 100.0).floor() as u32;
        let shifted = |delta: u32| -> Option<Vec<f64>> {
            let mut shifted = parts.clone();
            let largest = (0..n).max_by_key(|index| shifted[*index])?;
            if above {
                shifted[largest] += delta;
            } else {
                shifted[largest] = shifted[largest].checked_sub(delta)?;
            }
            Some(shifted.iter().map(|part| f64::from(*part) / 100.0).collect())
        };
        // A shift that leaves [0, 1] is not a distribution; it is skipped
        // rather than rejected, so wide counts never exhaust the reject budget.
        let in_range = |values: &[f64]| values.iter().all(|value| (0.0..=1.0).contains(value));
        if let Some(inside) = shifted(bound_hundredths).filter(|values| in_range(values)) {
            prop_assert_eq!(validate_choice(&inside, 0.5), Ok(()));
        }
        if let Some(outside) = shifted(bound_hundredths + 1).filter(|values| in_range(values)) {
            prop_assert_eq!(
                validate_choice(&outside, 0.5),
                Err(DecisionError::InvalidResponse(ValidationRule::ProbabilitySum))
            );
        }
    }

    /// D-057 M-2 / I-3: any true distribution over up to 255 options, with
    /// zeros, ones and ties, rounded to two decimals value by value as System
    /// One does, validates bit for bit.
    #[test]
    fn rounded_true_distributions_validate(raw in true_distribution(MAX_CHOICE_OPTIONS), confidence in 0.0_f64..=1.0) {
        let total: f64 = raw.iter().sum();
        let rounded: Vec<f64> = raw.iter().map(|weight| (weight / total * 100.0).round() / 100.0).collect();
        prop_assert_eq!(validate_choice(&rounded, confidence), Ok(()));
    }

    /// D-057 amendment 2 (N-1): a two-decimal distribution is accepted
    /// exactly when its sum is one rounding can produce,
    /// `1 − 0.005·n ≤ Σ ≤ 1 + 0.005·nnz`; the upper side never grows with
    /// zeros.
    #[test]
    fn rounded_sums_are_accepted_exactly_within_the_feasible_bounds(parts in sparse_hundredths(MAX_CHOICE_OPTIONS)) {
        let n = i64::try_from(parts.len()).unwrap();
        let nonzero = i64::try_from(parts.iter().filter(|part| **part > 0).count()).unwrap();
        let total: i64 = parts.iter().map(|part| i64::from(*part)).sum();
        // In hundredths: 1 − 0.005·n ≤ total/100 ⇔ 200 − n ≤ 2·total, and
        // total/100 ≤ 1 + 0.005·nnz ⇔ 2·total ≤ 200 + nnz.
        let feasible = 200 - n <= 2 * total && 2 * total <= 200 + nonzero;
        let values: Vec<f64> = parts.iter().map(|part| f64::from(*part) / 100.0).collect();
        let result = validate_choice(&values, 0.5);
        if feasible {
            prop_assert_eq!(result, Ok(()));
        } else {
            prop_assert_eq!(result, Err(DecisionError::InvalidResponse(ValidationRule::ProbabilitySum)));
        }
    }

    /// I-3: ties and one-hot distributions without rounding.
    #[test]
    fn tied_and_one_hot_distributions_validate(n in 2_usize..=MAX_CHOICE_OPTIONS, hot in any::<prop::sample::Index>()) {
        let tied = vec![1.0 / n as f64; n];
        prop_assert_eq!(validate_choice(&tied, 0.0), Ok(()));
        let mut one_hot = vec![0.0; n];
        one_hot[hot.index(n)] = 1.0;
        prop_assert_eq!(validate_choice(&one_hot, 1.0), Ok(()));
    }

    /// I-3: a repeated key at any depth of a legend level voids the response.
    #[test]
    fn a_repeated_key_at_any_depth_is_refused(depth in 0_usize..12) {
        let (request, key) = (&FIXTURE.scores[0], &FIXTURE.key);
        let codec = SystemOneCodec::new(request, key).expect("test: codec");
        let wire: serde_json::Value = serde_json::from_slice(&codec.encode_request().unwrap()).unwrap();
        let wire_id = wire["questions"].as_object().unwrap().keys().next().unwrap().clone();
        let level = format!("{}{{\"k\":1,\"k\":2}}{}", "{\"a\":".repeat(depth), "}".repeat(depth));
        let body = format!(
            r#"{{"model":"jev-1.13.0","answers":{{"{wire_id}":{{"type":"score","score":0.5,"confidence":0.5,"legend":{{"0":"level 0","1":{level}}},"probabilities":{{"0":0.5,"1":0.5}}}}}}}}"#
        );
        let envelope = ResponseEnvelope { backend: "compatible_http", provider_request_id: None, elapsed_ms: 1 };
        prop_assert_eq!(
            codec.decode_response(body.as_bytes(), envelope).map(|_| ()),
            Err(DecisionError::InvalidResponse(ValidationRule::AnswerDuplicated))
        );
    }

    /// I-3: salted wire ids are 64-bit, distinct, never the PRX ids, and move
    /// with every binding of the request.
    #[test]
    fn salted_wire_ids_are_distinct_and_bound_to_the_request(n in 2_usize..=MAX_OPTIONS, other in "[a-z]{1,12}") {
        let key = &FIXTURE.key;
        let wire_ids = |state_fingerprint: &str| {
            let questions: BTreeMap<QuestionId, DecisionQuestion> = (0..n)
                .map(|index| {
                    let candidate = CandidateKey::from_parts(
                        "use_tool",
                        "mcp",
                        Some(u32::try_from(index).unwrap().to_be_bytes()),
                    );
                    (
                        QuestionId::candidate(&candidate, CandidateQuestion::Success),
                        DecisionQuestion::Noul { instructions: structured(json!("Will it work?")), criteria: None },
                    )
                })
                .collect();
            let request = DecisionRequest::seal(
                RequestParts {
                    state: structured(json!({"goal": "salt"})),
                    questions,
                    model: Some("jev-1.13.0".to_string()),
                    state_fingerprint: state_fingerprint.to_string(),
                    candidate_set_hash: "candidates".to_string(),
                    policy_version: Arc::from("policy"),
                    catalog_generation: "catalog".to_string(),
                    omitted_questions: Vec::new(),
                    approval_pending: false,
                },
                key,
                Instant::now() + Duration::from_mins(1),
                &RequestLimits::default(),
            )
            .expect("test: seal");
            let body = SystemOneCodec::new(&request, key).unwrap().encode_request().unwrap();
            let wire: serde_json::Value = serde_json::from_slice(&body).unwrap();
            wire["questions"].as_object().unwrap().keys().cloned().collect::<Vec<_>>()
        };
        let first = wire_ids("state");
        prop_assert_eq!(first.len(), n);
        for id in &first {
            prop_assert!(id.len() == 18 && id.starts_with("q-") && !id.contains("candidate"), "{}", id);
        }
        let moved = wire_ids(&format!("state-{other}"));
        prop_assert!(first.iter().all(|id| !moved.contains(id)));
    }

    /// A Score expectation equal to the mean validates bit for bit; one far
    /// from it is refused.
    #[test]
    fn score_expectation_is_the_mean_level(raw in weights(10), far in 0.05_f64..3.0, above in any::<bool>()) {
        let values = distribution(&raw, 0.0);
        prop_assume!(!values.iter().all(|value| on_hundredths(*value)));
        prop_assert_eq!(validate_score(&values, 0.0), Ok(()));
        let shift = if above { far } else { -far };
        prop_assert_eq!(
            validate_score(&values, shift),
            Err(DecisionError::InvalidResponse(ValidationRule::ExpectedOutOfRange))
        );
    }

    /// Nothing outside `[0, 1]` becomes a probability, and every value inside
    /// it keeps its bits.
    #[test]
    fn non_probabilities_are_refused(bad in out_of_range(), good in 0.0_f64..=1.0) {
        prop_assert_eq!(
            Probability::new(bad),
            Err(DecisionError::InvalidResponse(ValidationRule::ProbabilityRange))
        );
        prop_assert_eq!(Probability::new(good).map(|p| p.get().to_bits()), Ok(good.to_bits()));
    }

    /// On the wire, a Noul answer or a confidence outside `[0, 1]` rejects the
    /// whole response; non-finite tokens are not JSON and are malformed.
    #[test]
    fn wire_noul_and_confidence_outside_the_unit_interval_are_refused(
        bad in prop_oneof![-1e6_f64..-1e-9, (1.0_f64 + 1e-9)..1e6],
        in_noul in any::<bool>(),
    ) {
        let envelope = ResponseEnvelope { backend: "compatible_http", provider_request_id: None, elapsed_ms: 1 };
        let request = if in_noul { &FIXTURE.noul } else { &FIXTURE.choices[0] };
        let codec = SystemOneCodec::new(request, &FIXTURE.key).expect("test: codec");
        let wire: serde_json::Value = serde_json::from_slice(&codec.encode_request().unwrap()).unwrap();
        let wire_id = wire["questions"].as_object().unwrap().keys().next().unwrap().clone();
        let answer = if in_noul {
            json!({"type": "noul", "noul": bad})
        } else {
            json!({"type": "choice", "choice": option(0).as_str(), "confidence": bad,
                   "probabilities": {option(0).as_str(): 0.6, option(1).as_str(): 0.4}})
        };
        let body = json!({"model": "jev-1.13.0", "answers": {wire_id: answer}}).to_string();
        prop_assert_eq!(
            codec.decode_response(body.as_bytes(), envelope).map(|_| ()),
            Err(DecisionError::InvalidResponse(ValidationRule::ProbabilityRange))
        );
        let codec = SystemOneCodec::new(&FIXTURE.choices[0], &FIXTURE.key).expect("test: codec");
        let wire: serde_json::Value = serde_json::from_slice(&codec.encode_request().unwrap()).unwrap();
        let wire_id = wire["questions"].as_object().unwrap().keys().next().unwrap().clone();
        for token in ["NaN", "Infinity", "-Infinity", "1e400", "null", "\"0.5\""] {
            let body = format!(
                r#"{{"model":"jev-1.13.0","answers":{{"{wire_id}":{{"type":"choice","choice":"{}","confidence":{token},"probabilities":{{"{}":0.6,"{}":0.4}}}}}}}}"#,
                option(0).as_str(), option(0).as_str(), option(1).as_str()
            );
            prop_assert_eq!(
                codec.decode_response(body.as_bytes(), envelope).map(|_| ()),
                Err(DecisionError::InvalidResponse(ValidationRule::Malformed)),
                "{}", token
            );
        }
    }

    /// Z corpus: any mutation of a recorded body (numbers replaced, keys
    /// dropped or duplicated, bytes flipped or truncated) never panics; it is
    /// rejected as a whole or validates to exactly the decoded values.
    #[test]
    fn mutated_recorded_bodies_are_rejected_whole_or_kept_exactly(
        mutation in 0_usize..6,
        at in any::<prop::sample::Index>(),
        replacement in -2.0_f64..2.0,
        byte in any::<u8>(),
    ) {
        let (request, key) = (&FIXTURE.scores[1], &FIXTURE.key);
        let codec = SystemOneCodec::new(request, key).expect("test: codec");
        let wire: serde_json::Value = serde_json::from_slice(&codec.encode_request().unwrap()).unwrap();
        let wire_id = wire["questions"].as_object().unwrap().keys().next().unwrap().clone();
        let base = format!(
            r#"{{"model":"jev-1.13.0","answers":{{"{wire_id}":{{"type":"score","score":0.97,"confidence":0.64,"legend":{{"0":"level 0","1":"level 1","2":"level 2"}},"probabilities":{{"0":0.13,"1":0.76,"2":0.11}}}}}},"usage":{{"input_tokens":471,"output_tokens":71}}}}"#
        );
        let mutated: Vec<u8> = match mutation {
            0 => {
                let numbers: Vec<(usize, usize)> = base
                    .match_indices(|c: char| c.is_ascii_digit())
                    .map(|(index, _)| (index, index + 1))
                    .collect();
                let (start, _) = numbers[at.index(numbers.len())];
                let end = base[start..].find(|c: char| !(c.is_ascii_digit() || c == '.')).map_or(base.len(), |o| start + o);
                format!("{}{replacement}{}", &base[..start], &base[end..]).into_bytes()
            }
            1 => {
                let cut = at.index(base.len());
                base.as_bytes()[..cut].to_vec()
            }
            2 => {
                let mut bytes = base.into_bytes();
                let index = at.index(bytes.len());
                bytes[index] = byte;
                bytes
            }
            3 => base.replacen("\"confidence\":0.64", "\"confidence\":0.64,\"confidence\":0.64", 1).into_bytes(),
            4 => {
                // A key renamed, or a container type swapped for an array.
                let keys = ["\"type\"", "\"score\"", "\"legend\"", "\"probabilities\"", "\"answers\"", "\"model\""];
                base.replacen(keys[at.index(keys.len())], "\"renamed\"", 1).into_bytes()
            }
            _ => {
                let objects = ["\"legend\":{", "\"probabilities\":{", "\"answers\":{"];
                let chosen = objects[at.index(objects.len())];
                let array = chosen.replace('{', "[");
                base.replacen(chosen, &array, 1).into_bytes()
            }
        };
        let envelope = ResponseEnvelope { backend: "compatible_http", provider_request_id: None, elapsed_ms: 1 };
        match codec.decode_response(&mutated, envelope) {
            Err(error) => prop_assert!(matches!(error, DecisionError::InvalidResponse(_)), "{error:?}"),
            Ok(decoded) => {
                let kept = decoded.clone();
                match DecisionResponseValidator::validate(request, decoded, &context()) {
                    Ok(decision) => {
                        for (id, answer) in &kept.answers {
                            prop_assert_eq!(decision.answer(id), Some(answer));
                        }
                    }
                    Err(error) => prop_assert!(matches!(error, DecisionError::InvalidResponse(_)), "{error:?}"),
                }
            }
        }
    }

    /// A typed Noul answer keeps its bits through validation.
    #[test]
    fn noul_answers_keep_their_bits(value in 0.0_f64..=1.0) {
        let request = &FIXTURE.noul;
        let id = QuestionId::global(GlobalQuestion::EvidenceSufficient);
        let answer = DecisionAnswer::Noul(NoulAnswer { probability_true: Probability::new(value).unwrap() });
        let decision = DecisionResponseValidator::validate(
            request,
            response(BTreeMap::from([(id.clone(), answer.clone())])),
            &context(),
        ).expect("test: noul validates");
        prop_assert_eq!(decision.answer(&id), Some(&answer));
    }
}

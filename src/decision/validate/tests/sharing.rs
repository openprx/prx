#![cfg(test)]
#![allow(clippy::expect_used)] // Sealed fixture failures retain explicit context.

use super::*;
use crate::decision::builtin::BuiltinDecisionBackend;
use crate::decision::key::DecisionKey;
use crate::decision::protocol::{
    CandidateKey, CandidateQuestion, GlobalQuestion, OptionId, RequestLimits, RequestParts,
};
use std::time::Duration;

fn structured(value: serde_json::Value) -> StructuredValue {
    StructuredValue::try_from(value).expect("valid sharing fixture value")
}

fn request(candidates: u32, key: &DecisionKey) -> DecisionRequest {
    let text = || structured(serde_json::json!({"question":"Evaluate the scoped candidate"}));
    let levels = || {
        (0..5)
            .map(|level| structured(serde_json::json!({"level":level,"description":"PRX bounded level"})))
            .collect()
    };
    let mut questions = BTreeMap::from([
        (
            QuestionId::global(GlobalQuestion::NextAction),
            DecisionQuestion::Choice {
                instructions: text(),
                criteria: BTreeMap::from([(OptionId::code("answer"), text()), (OptionId::code("clarify"), text())]),
            },
        ),
        (
            QuestionId::global(GlobalQuestion::EvidenceSufficient),
            DecisionQuestion::Noul {
                instructions: text(),
                criteria: None,
            },
        ),
        (
            QuestionId::global(GlobalQuestion::SemanticRisk),
            DecisionQuestion::Score {
                instructions: text(),
                levels: levels(),
            },
        ),
    ]);
    for index in 0..candidates {
        let candidate = CandidateKey::from_parts("use_tool", "native_tool", Some(index.to_be_bytes()));
        for primitive in CandidateQuestion::ALL {
            let question = match primitive {
                CandidateQuestion::Applicable | CandidateQuestion::Success => DecisionQuestion::Noul {
                    instructions: text(),
                    criteria: None,
                },
                CandidateQuestion::Gain | CandidateQuestion::Risk => DecisionQuestion::Score {
                    instructions: text(),
                    levels: levels(),
                },
            };
            questions.insert(QuestionId::candidate(&candidate, primitive), question);
        }
    }
    DecisionRequest::seal(
        RequestParts {
            state: structured(serde_json::json!({"goal":"Inspect local evidence"})),
            questions,
            model: Some("prx-fixture-model".into()),
            state_fingerprint: "prx-state".into(),
            candidate_set_hash: "prx-candidates".into(),
            policy_version: Arc::from("prx-policy"),
            catalog_generation: "prx-catalog".into(),
            omitted_questions: Vec::new(),
            approval_pending: false,
        },
        key,
        Instant::now() + Duration::from_secs(30),
        &RequestLimits::default(),
    )
    .expect("sealed sharing fixture")
}

fn validate_builtin(request: &DecisionRequest) -> ValidatedDecision {
    let mut response = BuiltinDecisionBackend::answer(request).expect("actual builtin answer");
    // Fixed diagnostic accounting exercises clone/cache metadata independently
    // of answer count. It is fixture data, not builtin billing evidence.
    response.usage = DecisionUsage {
        input_tokens: Some(11),
        output_tokens: Some(7),
        cost_micro_usd: Some(3),
    };
    response.provider_request_id = Some("prx_fixture_request".into());
    DecisionResponseValidator::validate(
        request,
        response,
        &ValidationContext {
            received_at: Instant::now(),
            max_response_bytes: 262_144,
            version_policy: ModelVersionPolicy::Unpinned,
            local_backend: BuiltinDecisionBackend::identity(),
        },
    )
    .expect("validated actual builtin answers")
}

#[test]
#[allow(clippy::print_stdout)] // Retain raw component allocation evidence.
fn validated_clone_allocations_do_not_grow_with_answer_count() {
    let dir = tempfile::tempdir().expect("valid sharing fixture");
    let key = DecisionKey::load_or_create(dir.path()).expect("valid sharing fixture");
    let mut measurements = Vec::new();
    for candidates in [1, 12] {
        let request = request(candidates, &key);
        assert_eq!(
            request.questions().len(),
            usize::try_from(4 * candidates + 3).expect("bounded fixture question count")
        );
        let decision = validate_builtin(&request);
        let cache = crate::decision::cache::DecisionCache::new();
        let cache_key = crate::decision::cache::CacheKey::new(&request, decision.identity(), 1);
        let now = Instant::now();
        cache.put(cache_key, decision.clone(), now);
        let (cache_hit, hit_count) = crate::decision::builtin::allocation_counter::count(|| {
            cache.get(&cache_key, now).expect("primed decision cache")
        });
        assert_eq!(cache_hit, decision);
        let (clone, clone_count) = crate::decision::builtin::allocation_counter::count(|| decision.clone());
        let (cached, cache_count) =
            crate::decision::builtin::allocation_counter::count(|| decision.served_from_cache("prx_current_request"));
        assert_eq!(clone, decision);
        for id in request.questions().keys() {
            assert_eq!(cached.answer(id), decision.answer(id));
        }
        std::hint::black_box((&clone, &cached));
        println!(
            "PRX_ANSWER_CLONE candidates={candidates} questions={} clone_attempts={clone_count} cache_relabel_attempts={cache_count} cache_get_attempts={hit_count}",
            request.questions().len()
        );
        measurements.push((clone_count, cache_count, hit_count));
    }
    assert_eq!(
        measurements.first(),
        measurements.get(1),
        "Clone and cache relabel allocation must be answer-count independent"
    );
}

#[test]
fn validated_clones_preserve_values_metadata_and_survive_source_drop() {
    let dir = tempfile::tempdir().expect("valid sharing fixture");
    let key = DecisionKey::load_or_create(dir.path()).expect("valid sharing fixture");
    let request = request(12, &key);
    let decision = validate_builtin(&request);
    let independently_validated = validate_builtin(&request);
    assert_eq!(
        decision, independently_validated,
        "Equality compares independently validated values"
    );
    let clone = decision.clone();
    let cached = decision.served_from_cache("prx_current_request");
    assert_eq!(clone, decision);
    assert_eq!(cached.request_id(), "prx_current_request");
    assert_eq!(*cached.usage(), DecisionUsage::default());
    assert_eq!(cached.provider_request_id(), None);
    assert_eq!(cached.identity(), decision.identity());
    assert_eq!(cached.claimed_backend(), decision.claimed_backend());
    assert_eq!(cached.claimed_model(), decision.claimed_model());
    assert_eq!(cached.response_sha256(), decision.response_sha256());
    assert_eq!(decision.provider_request_id(), Some("prx_fixture_request"));
    assert_eq!(decision.usage().input_tokens, Some(11));
    assert_eq!(decision.request_id(), request.request_id());
    let ids: Vec<_> = request.questions().keys().cloned().collect();
    drop(decision);
    drop(request);
    drop(key);
    for id in &ids {
        assert_eq!(clone.answer(id), independently_validated.answer(id));
        assert_eq!(cached.answer(id), independently_validated.answer(id));
    }
}

#[test]
fn cache_relabel_preserves_real_decoded_response_digest() {
    use crate::decision::wire::salted_question_ids;
    use crate::decision::wire::{ResponseEnvelope, SystemOneCodec};
    let dir = tempfile::tempdir().expect("valid sharing fixture");
    let key = DecisionKey::load_or_create(dir.path()).expect("valid sharing fixture");
    let mut request_parts = RequestParts {
        state: structured(serde_json::json!({"goal":"Inspect local evidence"})),
        questions: BTreeMap::from([(
            QuestionId::global(GlobalQuestion::EvidenceSufficient),
            DecisionQuestion::Noul {
                instructions: structured(serde_json::json!("Evidence sufficient?")),
                criteria: None,
            },
        )]),
        model: Some("prx-fixture-model".into()),
        state_fingerprint: "prx-state".into(),
        candidate_set_hash: "prx-candidates".into(),
        policy_version: Arc::from("prx-policy"),
        catalog_generation: "prx-catalog".into(),
        omitted_questions: Vec::new(),
        approval_pending: false,
    };
    let request = DecisionRequest::seal(
        request_parts.clone(),
        &key,
        Instant::now() + Duration::from_secs(30),
        &RequestLimits::default(),
    )
    .expect("valid sharing fixture");
    // A codec-only owned fixture, with a declared compatible identity. It
    // does not send a request or prove service/backend admission.
    let decode = |request: &DecisionRequest| {
        let wire_id = salted_question_ids(
            &key,
            request.manifest().request_fingerprint(),
            request.questions().keys(),
        )
        .expect("valid sharing fixture")
        .pop()
        .expect("valid sharing fixture");
        let body = serde_json::to_vec(&serde_json::json!({"model":"prx-fixture-model", "answers":{wire_id:{"type":"noul","noul":0.75}}, "usage":{"input_tokens":11,"output_tokens":7}})).expect("valid sharing fixture");
        let codec = SystemOneCodec::new(request, &key).expect("valid sharing fixture");
        let response = codec
            .decode_response(
                &body,
                ResponseEnvelope {
                    backend: "prx_fixture",
                    provider_request_id: Some("prx_fixture_request"),
                    elapsed_ms: 0,
                },
            )
            .expect("valid sharing fixture");
        let expected_digest: [u8; 32] = sha2::Sha256::digest(&body).into();
        assert_eq!(response.response_sha256, expected_digest);
        assert_eq!(
            usize::try_from(response.raw_bytes).expect("bounded wire fixture size"),
            body.len()
        );
        response
    };
    let response = decode(&request);
    let digest = response.response_sha256;
    let context = ValidationContext {
        received_at: Instant::now(),
        max_response_bytes: 262_144,
        version_policy: ModelVersionPolicy::ExactPinned(Arc::from("prx-fixture-model")),
        local_backend: BackendIdentity {
            kind: crate::decision::protocol::BackendKind::CompatibleHttp,
            provider: None,
            configured_model: Arc::from("prx-fixture-model"),
        },
    };
    let decision =
        DecisionResponseValidator::validate(&request, response.clone(), &context).expect("valid sharing fixture");
    let cached = decision.served_from_cache("prx_current_request");
    assert_eq!(decision.response_sha256(), Some(digest));
    assert_eq!(cached.response_sha256(), Some(digest));
    assert_eq!(
        cached.answer(&QuestionId::global(GlobalQuestion::EvidenceSufficient)),
        decision.answer(&QuestionId::global(GlobalQuestion::EvidenceSufficient))
    );
    assert_eq!(*cached.usage(), DecisionUsage::default());
    assert_eq!(cached.provider_request_id(), None);
    assert_eq!(decision.provider_request_id(), Some("prx_fixture_request"));
    assert!(matches!(
        DecisionResponseValidator::validate(&request, response.clone(), &context),
        Err(DecisionError::InvalidResponse(ValidationRule::DuplicateDelivery))
    ));
    // Fresh request has its own delivery authority; sharing does not reset
    // the original request's duplicate-delivery ledger.
    request_parts.state_fingerprint = "prx-fresh-state".into();
    let fresh = DecisionRequest::seal(
        request_parts,
        &key,
        Instant::now() + Duration::from_secs(30),
        &RequestLimits::default(),
    )
    .expect("valid sharing fixture");
    let fresh_response = decode(&fresh);
    let fresh_context = ValidationContext {
        received_at: Instant::now(),
        ..context
    };
    assert!(DecisionResponseValidator::validate(&fresh, fresh_response, &fresh_context).is_ok());
    assert!(matches!(
        DecisionResponseValidator::validate(&request, response, &fresh_context),
        Err(DecisionError::InvalidResponse(ValidationRule::DuplicateDelivery))
    ));
}

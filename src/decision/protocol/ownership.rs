#![cfg(test)]
//! Literal pre-optimization sealing order; never call the revised helpers.
use super::*;

pub(crate) fn original_seal(
    parts: RequestParts,
    projection_audit: ProjectionAudit,
    key: &DecisionKey,
    expires_at: Instant,
    limits: &RequestLimits,
) -> Result<DecisionRequest, DecisionError> {
    let refuse = |rule| DecisionError::InvalidRequest(rule);
    if !parts.state.is_object() {
        return Err(refuse(RequestRule::StateShape));
    }
    parts.state.check(&limits.structured)?;
    if has_duplicate_candidate_ids(parts.state.as_json()) {
        return Err(refuse(RequestRule::DuplicateCandidateId));
    }
    if parts.questions.is_empty() {
        return Err(refuse(RequestRule::NoQuestions));
    }
    for (id, question) in &parts.questions {
        if !id.is_well_formed() {
            return Err(refuse(RequestRule::IdentifierGrammar));
        }
        check_question(question, &limits.structured)?;
    }
    if let Some(model) = &parts.model {
        if !is_identity_text(model) {
            return Err(refuse(RequestRule::ModelInvalid));
        }
    }
    for (id, _) in &parts.omitted_questions {
        if !id.is_well_formed() {
            return Err(refuse(RequestRule::IdentifierGrammar));
        }
        if parts.questions.contains_key(id) {
            return Err(refuse(RequestRule::OmittedQuestionAsked));
        }
    }

    let content = canonical_content(&parts);
    if content.len() > limits.max_request_bytes as usize {
        return Err(refuse(RequestRule::RequestTooLarge));
    }
    let mut hasher = Sha256::new();
    hasher.update(b"prx.decision.request.v1\0");
    hasher.update(content.as_bytes());
    hasher.update(b"\0");
    hasher.update(canonical_bindings(&parts, &projection_audit).as_bytes());
    let request_fingerprint: [u8; 32] = hasher.finalize().into();
    crate::decision::wire::salted_question_ids(key, &request_fingerprint, parts.questions.keys())?;

    let request_id = uuid::Uuid::now_v7().to_string();
    let RequestParts {
        state,
        questions,
        model,
        state_fingerprint,
        candidate_set_hash,
        policy_version,
        catalog_generation,
        omitted_questions,
        approval_pending,
    } = parts;
    Ok(DecisionRequest {
        protocol_version: PROTOCOL_VERSION,
        request_id: request_id.clone(),
        state,
        questions,
        model,
        structured_limits: limits.structured,
        manifest: DecisionRequestManifest {
            request_id,
            request_fingerprint,
            state_fingerprint,
            candidate_set_hash,
            policy_version,
            catalog_generation,
            omitted_questions,
            projection_audit,
            cacheable: !approval_pending,
            expires_at,
        },
        delivered: Mutex::new(BTreeSet::new()),
    })
}

fn parts() -> RequestParts {
    RequestParts {
        state: StructuredValue::try_from(serde_json::json!({"project":"prx"})).unwrap(),
        questions: BTreeMap::from([(
            QuestionId::global(GlobalQuestion::EvidenceSufficient),
            DecisionQuestion::Noul {
                instructions: text("Sufficient evidence?"),
                criteria: None,
            },
        )]),
        model: Some("prx_model".into()),
        state_fingerprint: "state".into(),
        candidate_set_hash: "candidates".into(),
        policy_version: Arc::from("policy"),
        catalog_generation: "generation".into(),
        omitted_questions: Vec::new(),
        approval_pending: false,
    }
}
fn text(value: &str) -> StructuredValue {
    StructuredValue::try_from(serde_json::json!(value)).unwrap()
}
fn invalid_id() -> QuestionId {
    QuestionId::candidate(
        &CandidateKey::from_parts("Invalid Action", "mcp", None),
        CandidateQuestion::Gain,
    )
}

pub(crate) fn assert_same_request(a: &DecisionRequest, b: &DecisionRequest, key: &DecisionKey) {
    assert_eq!(a.state(), b.state());
    assert_eq!(a.questions(), b.questions());
    assert_eq!(a.model(), b.model());
    assert_eq!(a.structured_limits(), b.structured_limits());
    assert_eq!(a.protocol_version(), b.protocol_version());
    let x = a.manifest();
    let y = b.manifest();
    assert_eq!(x.request_fingerprint(), y.request_fingerprint());
    assert_eq!(x.state_fingerprint(), y.state_fingerprint());
    assert_eq!(x.candidate_set_hash(), y.candidate_set_hash());
    assert_eq!(x.policy_version(), y.policy_version());
    assert_eq!(x.catalog_generation(), y.catalog_generation());
    assert_eq!(x.omitted_questions(), y.omitted_questions());
    assert_eq!(x.projection_audit(), y.projection_audit());
    assert_eq!(x.cacheable(), y.cacheable());
    assert_eq!(x.expires_at(), y.expires_at());
    let aid = uuid::Uuid::parse_str(a.request_id()).unwrap();
    let bid = uuid::Uuid::parse_str(b.request_id()).unwrap();
    assert_eq!(a.request_id(), a.manifest().request_id());
    assert_eq!(b.request_id(), b.manifest().request_id());
    assert_eq!(aid.get_version_num(), 7);
    assert_eq!(bid.get_version_num(), 7);
    assert_ne!(aid, bid);
    assert_eq!(
        crate::decision::wire::SystemOneCodec::new(a, key)
            .unwrap()
            .encode_request(),
        crate::decision::wire::SystemOneCodec::new(b, key)
            .unwrap()
            .encode_request()
    );
}

#[test]
fn public_seal_preserves_original_order_and_byte_boundary() {
    let key = crate::causal_tree::state::fixtures::key();
    let expiry = Instant::now();
    let mut cases = Vec::new();
    let mut add = |p, default_limits, rule| cases.push((p, default_limits, Some(DecisionError::InvalidRequest(rule))));
    let default_limits = RequestLimits::default();
    let mut case_parts = parts();
    case_parts.state = text("nonobject");
    case_parts.questions.clear();
    add(case_parts, default_limits, RequestRule::StateShape);
    let mut case_parts = parts();
    case_parts.state = StructuredValue::try_from(serde_json::json!({"a":[["deep"]]})).unwrap();
    case_parts.questions.clear();
    let mut tight = default_limits;
    tight.structured.max_depth = 1;
    add(case_parts, tight, RequestRule::StructuredDepth);
    let mut case_parts = parts();
    case_parts.state =
        StructuredValue::try_from(serde_json::json!({"candidates":[{"id":"same"},{"id":"same"}]})).unwrap();
    case_parts.questions.clear();
    add(case_parts, default_limits, RequestRule::DuplicateCandidateId);
    let mut case_parts = parts();
    case_parts.questions.clear();
    let mut tight = default_limits;
    tight.structured.max_nodes = 1;
    add(case_parts, tight, RequestRule::StructuredNodes);
    let mut case_parts = parts();
    case_parts.state = StructuredValue::try_from(serde_json::json!({"project":"long neutral value"})).unwrap();
    case_parts.questions.clear();
    let mut tight = default_limits;
    tight.structured.max_string_bytes = 8;
    add(case_parts, tight, RequestRule::StructuredStringLength);
    let mut case_parts = parts();
    case_parts.questions.clear();
    case_parts.model = Some("".into());
    add(case_parts, default_limits, RequestRule::NoQuestions);
    let mut case_parts = parts();
    let question = case_parts.questions.pop_first().unwrap().1;
    case_parts.questions.insert(invalid_id(), question);
    add(case_parts, default_limits, RequestRule::IdentifierGrammar);
    let id = QuestionId::global(GlobalQuestion::EvidenceSufficient);
    let mut case_parts = parts();
    case_parts.questions.insert(
        id.clone(),
        DecisionQuestion::Choice {
            instructions: text("instructions longer than eight"),
            criteria: BTreeMap::new(),
        },
    );
    let mut tight = default_limits;
    tight.structured.max_string_bytes = 8;
    add(case_parts, tight, RequestRule::StructuredStringLength);
    for (count, rule) in [(1, RequestRule::MinChoiceOptions), (256, RequestRule::MaxChoiceOptions)] {
        let mut case_parts = parts();
        case_parts.questions.insert(
            id.clone(),
            DecisionQuestion::Choice {
                instructions: text("choice"),
                criteria: (0..count)
                    .map(|i| {
                        (
                            OptionId::candidate(&CandidateKey::from_parts("answer", "mcp", Some([0, 0, 0, i as u8]))),
                            text("value"),
                        )
                    })
                    .collect(),
            },
        );
        add(case_parts, default_limits, rule);
    }
    let mut case_parts = parts();
    case_parts.questions.insert(
        id.clone(),
        DecisionQuestion::Choice {
            instructions: text("choice"),
            criteria: BTreeMap::from([
                (OptionId::code("Invalid Option"), text("value")),
                (OptionId::code("valid"), text("value")),
            ]),
        },
    );
    add(case_parts, default_limits, RequestRule::IdentifierGrammar);
    for (count, rule) in [(1, RequestRule::MinScoreLevels), (11, RequestRule::MaxScoreLevels)] {
        let mut case_parts = parts();
        case_parts.questions.insert(
            id.clone(),
            DecisionQuestion::Score {
                instructions: text("score"),
                levels: vec![text("level"); count],
            },
        );
        add(case_parts, default_limits, rule);
    }
    let mut case_parts = parts();
    case_parts.questions.insert(
        id.clone(),
        DecisionQuestion::Noul {
            instructions: text("noul"),
            criteria: Some(NoulCriteria {
                when_true: None,
                when_false: None,
            }),
        },
    );
    add(case_parts, default_limits, RequestRule::NoulCriteriaShape);
    let mut tight = default_limits;
    tight.structured.max_depth = 2;
    tight.structured.max_string_bytes = 8;
    for (when_true, when_false, rule) in [
        (
            text("long true outcome"),
            StructuredValue::try_from(serde_json::json!([["deep"]])).unwrap(),
            RequestRule::StructuredStringLength,
        ),
        (
            StructuredValue::try_from(serde_json::json!([["deep"]])).unwrap(),
            text("long false outcome"),
            RequestRule::StructuredDepth,
        ),
    ] {
        let mut case_parts = parts();
        case_parts.questions.insert(
            id.clone(),
            DecisionQuestion::Noul {
                instructions: text("noul"),
                criteria: Some(NoulCriteria {
                    when_true: Some(when_true),
                    when_false: Some(when_false),
                }),
            },
        );
        add(case_parts, tight, rule);
    }
    let mut case_parts = parts();
    case_parts.questions.insert(
        QuestionId::global(GlobalQuestion::CapabilityTarget),
        DecisionQuestion::Score {
            instructions: text("score"),
            levels: vec![text("level")],
        },
    );
    case_parts.questions.insert(
        id.clone(),
        DecisionQuestion::Noul {
            instructions: text("long later instruction"),
            criteria: None,
        },
    );
    let mut tight = default_limits;
    tight.structured.max_string_bytes = 8;
    add(case_parts, tight, RequestRule::MinScoreLevels);
    let mut case_parts = parts();
    case_parts.model = Some("".into());
    case_parts
        .omitted_questions
        .push((invalid_id(), OmissionReason::NoArtifacts));
    let mut tiny = default_limits;
    tiny.max_request_bytes = 1;
    add(case_parts, tiny, RequestRule::ModelInvalid);
    let mut case_parts = parts();
    case_parts
        .omitted_questions
        .push((invalid_id(), OmissionReason::NoArtifacts));
    add(case_parts, tiny, RequestRule::IdentifierGrammar);
    let mut case_parts = parts();
    case_parts
        .omitted_questions
        .push((id.clone(), OmissionReason::NoArtifacts));
    case_parts
        .omitted_questions
        .push((invalid_id(), OmissionReason::NoArtifacts));
    add(case_parts, tiny, RequestRule::OmittedQuestionAsked);
    let mut case_parts = parts();
    case_parts
        .omitted_questions
        .push((invalid_id(), OmissionReason::NoArtifacts));
    case_parts.omitted_questions.push((id, OmissionReason::NoArtifacts));
    add(case_parts, tiny, RequestRule::IdentifierGrammar);
    let size = canonical_content(&parts()).len() as u32;
    let mut minus = default_limits;
    minus.max_request_bytes = size - 1;
    add(parts(), minus, RequestRule::RequestTooLarge);
    let mut exact = default_limits;
    exact.max_request_bytes = size;
    cases.push((parts(), exact, None));
    let mut case_parts = parts();
    case_parts.model = None;
    cases.push((case_parts, default_limits, None));
    let mut case_parts = parts();
    case_parts.approval_pending = true;
    let omitted = QuestionId::global(GlobalQuestion::NeedsFreshState);
    case_parts.omitted_questions = vec![
        (omitted.clone(), OmissionReason::NoArtifacts),
        (omitted, OmissionReason::NoArtifacts),
    ];
    cases.push((case_parts, default_limits, None));
    for (index, (p, limits, expected)) in cases.into_iter().enumerate() {
        let audit = ProjectionAudit {
            truncated_fields: vec!["artifacts.summary".into(); 3],
            redacted_texts: 2,
        };
        let original = original_seal(p.clone(), audit.clone(), &key, expiry, &limits);
        let recovered_input = p.clone();
        let before = parts_snapshot(&recovered_input, &audit);
        let pointers = heap_pointers(&recovered_input);
        let recovering = DecisionRequest::seal_recovering_parts(recovered_input, audit.clone(), &key, expiry, &limits);
        match (&original, recovering) {
            (Err(expected), Err((error, recovered))) => {
                assert_eq!(expected, &error);
                assert_eq!(before, parts_snapshot(&recovered, &audit));
                assert_eq!(pointers, heap_pointers(&recovered));
            }
            (Ok(original), Ok(recovered)) => assert_same_request(original, &recovered, &key),
            _ => panic!("recovering order differs case{index}"),
        }
        let current = DecisionRequest::seal_with_projection_audit(p, audit, &key, expiry, &limits);
        match (original, current, expected) {
            (Err(a), Err(b), Some(expected)) => {
                assert_eq!(a, expected, "case{index}");
                assert_eq!(a, b, "case{index}");
            }
            (Ok(a), Ok(b), None) => assert_same_request(&a, &b, &key),
            (a, b, e) => panic!("case{index} unexpected {} / {} expected {e:?}", a.is_ok(), b.is_ok()),
        }
    }
}

fn parts_snapshot(parts: &RequestParts, audit: &ProjectionAudit) -> (String, String, bool) {
    (
        canonical_content(parts),
        canonical_bindings(parts, audit),
        parts.approval_pending,
    )
}
fn heap_pointers(parts: &RequestParts) -> [Option<usize>; 7] {
    [
        parts.model.as_ref().map(|x| x.as_ptr() as usize),
        Some(parts.state_fingerprint.as_ptr() as usize),
        Some(parts.candidate_set_hash.as_ptr() as usize),
        Some(parts.catalog_generation.as_ptr() as usize),
        Some(Arc::as_ptr(&parts.policy_version).cast::<()>() as usize),
        parts
            .questions
            .values()
            .next()
            .and_then(|q| q.instructions().as_json().as_str())
            .map(|s| s.as_ptr() as usize),
        parts
            .state
            .as_json()
            .get("project")
            .and_then(serde_json::Value::as_str)
            .map(|s| s.as_ptr() as usize),
    ]
}

#[test]
fn recovering_parts_retries_exact_ownership_and_drops_failure_payload() {
    let key = crate::causal_tree::state::fixtures::key();
    let expires = Instant::now();
    let mut limits = RequestLimits::default();
    limits.max_request_bytes = 1;
    let input = parts();
    let weak = Arc::downgrade(&input.policy_version);
    let audit = ProjectionAudit {
        truncated_fields: vec!["artifacts.summary".into(); 2],
        redacted_texts: 3,
    };
    let expected = input.clone();
    let snapshot = parts_snapshot(&input, &audit);
    let pointers = heap_pointers(&input);
    let (error, recovered) = DecisionRequest::seal_recovering_parts(input, audit.clone(), &key, expires, &limits)
        .expect_err("original parts recover on size failure");
    assert_eq!(error, DecisionError::InvalidRequest(RequestRule::RequestTooLarge));
    assert_eq!(snapshot, parts_snapshot(&recovered, &audit));
    assert_eq!(pointers, heap_pointers(&recovered));
    assert_eq!(weak.strong_count(), 2);
    limits = RequestLimits::default();
    let actual = DecisionRequest::seal_recovering_parts(*recovered, audit.clone(), &key, expires, &limits).unwrap();
    let original = original_seal(expected, audit, &key, expires, &limits).unwrap();
    assert_same_request(&original, &actual, &key);
    drop(actual);
    drop(original);
    assert!(weak.upgrade().is_none());
    let failed = parts();
    let weak = Arc::downgrade(&failed.policy_version);
    limits.max_request_bytes = 1;
    let failure =
        DecisionRequest::seal_recovering_parts(failed, ProjectionAudit::default(), &key, expires, &limits).unwrap_err();
    assert_eq!(weak.strong_count(), 1);
    drop(failure);
    assert!(weak.upgrade().is_none());
}

#[test]
fn successful_recovery_and_public_seal_have_independent_delivery_ledgers() {
    use crate::decision::builtin::BuiltinDecisionBackend;
    use crate::decision::{DecisionResponseValidator, ModelVersionPolicy, ValidationContext};
    let key = crate::causal_tree::state::fixtures::key();
    let input = parts();
    let expires = Instant::now() + std::time::Duration::from_secs(30);
    let first = DecisionRequest::seal_recovering_parts(
        input.clone(),
        ProjectionAudit::default(),
        &key,
        expires,
        &RequestLimits::default(),
    )
    .unwrap();
    let second = DecisionRequest::seal(input, &key, expires, &RequestLimits::default()).unwrap();
    assert_same_request(&first, &second, &key);
    let context = ValidationContext {
        received_at: Instant::now(),
        max_response_bytes: 262_144,
        version_policy: ModelVersionPolicy::Unpinned,
        local_backend: BuiltinDecisionBackend::identity(),
    };
    // Typed builtin responses have no wire delivery and are intentionally
    // repeatable. They cannot populate or collide with the wire ledger.
    for request in [&first, &second] {
        let response = BuiltinDecisionBackend::answer(request).unwrap();
        assert_eq!(response.raw_bytes, 0);
        assert!(DecisionResponseValidator::validate(request, response.clone(), &context).is_ok());
        assert!(DecisionResponseValidator::validate(request, response, &context).is_ok());
    }
    // Codec-only fixture: actual encode/decode/validator, no remote backend
    // or service admission claim. Both requests have the same salted body.
    let context = ValidationContext {
        version_policy: ModelVersionPolicy::ExactPinned(Arc::from("prx_model")),
        local_backend: BackendIdentity {
            kind: BackendKind::CompatibleHttp,
            provider: None,
            configured_model: Arc::from("prx_model"),
        },
        ..context
    };
    for request in [&first, &second] {
        let wire_id = crate::decision::wire::salted_question_ids(
            &key,
            request.manifest().request_fingerprint(),
            request.questions().keys(),
        )
        .unwrap()
        .pop()
        .unwrap();
        let body = serde_json::to_vec(
            &serde_json::json!({"model":"prx_model","answers":{wire_id:{"type":"noul","noul":0.75}}}),
        )
        .unwrap();
        let codec = crate::decision::wire::SystemOneCodec::new(request, &key).unwrap();
        let response = codec
            .decode_response(
                &body,
                crate::decision::wire::ResponseEnvelope {
                    backend: "prx_fixture",
                    provider_request_id: None,
                    elapsed_ms: 0,
                },
            )
            .unwrap();
        assert!(response.raw_bytes > 0);
        let expected: [u8; 32] = Sha256::digest(&body).into();
        assert_eq!(response.response_sha256, expected);
        assert!(DecisionResponseValidator::validate(request, response.clone(), &context).is_ok());
        assert_eq!(
            DecisionResponseValidator::validate(request, response, &context),
            Err(DecisionError::InvalidResponse(ValidationRule::DuplicateDelivery))
        );
    }
}

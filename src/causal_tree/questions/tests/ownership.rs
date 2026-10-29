#![cfg(test)]
//! Original allocation/sealing implementation under the current question policy.
//! The 2026-10-29 policy omits all unused fixed-terminal candidate predictions.
use super::*;
use crate::causal_tree::branch::{CandidateSpec, SideEffectClass};
use crate::decision::builtin::allocation_counter::count;
use crate::decision::protocol::ownership::assert_same_request;

fn original_compile(
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

    let (mut projection, mut projection_manifest) = redacted_projection(state)?;
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

    let candidate_set_hash = hash_candidate_set(candidates, exact_mentions, approval_scopes);
    let trusted = state.trusted.parts();
    let mut parts = RequestParts {
        state: projection.clone(),
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
        parts.state = projection.clone();
        let projection_audit = ProjectionAudit {
            truncated_fields: projection_manifest
                .truncated
                .iter()
                .map(|field| (*field).to_string())
                .collect(),
            redacted_texts: projection_manifest.redacted_texts,
        };
        match crate::decision::protocol::ownership::original_seal(
            parts.clone(),
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
            Err(DecisionError::InvalidRequest(RequestRule::RequestTooLarge)) => {
                projection = shrink_oldest_artifact_summary(&projection)?.ok_or(CompileError::Budget)?;
                projection_manifest.truncated.push("artifacts.summary");
            }
            Err(error) => return Err(CompileError::Protocol(error)),
        }
    }
}
fn assert_same_compiled(a: &CompiledQuestionRequest, b: &CompiledQuestionRequest, key: &DecisionKey) {
    assert_eq!(a.forced_choices, b.forced_choices);
    assert_same_request(&a.request, &b.request, key);
}
fn fixture(key: &DecisionKey, count: usize, effect: SideEffectClass) -> CausalState {
    let mut state = fixtures::state(key, "neutral request");
    let mut candidates = Vec::new();
    for index in 0..count {
        let mut derived = fixtures::candidate(
            key,
            CausalActionKind::UseTool,
            CapabilityTarget::Mcp,
            Some(&format!("prx_namespace_{index}")),
        );
        derived.candidate = CausalCandidate::new(
            CausalActionKind::UseTool,
            CapabilityTarget::Mcp,
            CandidateSpec {
                namespace: derived.candidate.candidate_id().namespace(),
                required_capabilities: Vec::new(),
                side_effect: effect,
                approval: ApprovalRequirement::None,
                estimated_cost: derived.candidate.estimated_cost(),
                estimated_latency_ms: Some(200),
            },
        );
        candidates.push(derived);
    }
    candidates.push(fixtures::candidate(
        key,
        CausalActionKind::Refuse,
        CapabilityTarget::None,
        None,
    ));
    state.derived = DerivedState::new(candidates).unwrap();
    state
}

#[test]
#[allow(clippy::print_stdout)]
fn compiler_moves_parts_with_fixed_allocation_improvement() {
    // Prospective criterion: at least32 fewer Rust allocation attempts for
    // each equal-shape first/warm success. Setup/output/destruction excluded.
    let key = fixtures::key();
    let _ = uuid::Uuid::now_v7();
    let effects = [
        SideEffectClass::ReadOnly,
        SideEffectClass::LocalWrite,
        SideEffectClass::ProcessExec,
        SideEffectClass::ExternalSend,
        SideEffectClass::CredentialOrConfig,
        SideEffectClass::Destructive,
    ];
    // Prime shared UUID/canonical setup outside measurements. First/warm
    // below are first and second measured calls for each complete shape.
    let priming_state = fixture(&key, 1, SideEffectClass::ReadOnly);
    let priming_set = legal(&priming_state);
    let priming_policy = CompilerPolicy {
        key: &key,
        model: Some("prx_model"),
        expires_at: Instant::now() + Duration::from_secs(30),
        limits: RequestLimits::default(),
    };
    drop(original_compile(&priming_state, &priming_set, &priming_policy).unwrap());
    drop(QuestionCompiler::compile(&priming_state, &priming_set, &priming_policy).unwrap());
    let mut results = Vec::new();
    for effect in effects {
        for candidates in [1, 13, 31] {
            for model in [None, Some("prx_model")] {
                let state = fixture(&key, candidates, effect);
                let set = legal(&state);
                assert_eq!(state.derived.candidates().len(), candidates + 1);
                assert_eq!(
                    redacted_projection(&state).unwrap().0.as_json()["candidates"]
                        .as_array()
                        .unwrap()
                        .len(),
                    candidates + 1
                );
                let policy = CompilerPolicy {
                    key: &key,
                    model,
                    expires_at: Instant::now() + Duration::from_secs(30),
                    limits: RequestLimits::default(),
                };
                for pass in ["first", "warm"] {
                    let (old, old_count) = count(|| original_compile(&state, &set, &policy).unwrap());
                    let (new, new_count) = count(|| QuestionCompiler::compile(&state, &set, &policy).unwrap());
                    assert_same_compiled(&old, &new, &key);
                    println!(
                        "PRX_SEAL_ALLOC effect={} candidates={} model={} pass={} original={} current={}",
                        effect.as_str(),
                        candidates + 1,
                        model.is_some(),
                        pass,
                        old_count,
                        new_count
                    );
                    results.push((effect, candidates, model.is_some(), pass, old_count, new_count));
                }
            }
        }
    }
    for (effect, candidates, model, pass, old, new) in results {
        assert!(
            new + 32 <= old,
            "fixed improvement failed {effect:?}/{candidates}/{model}/{pass}: {old}->{new}"
        );
    }
}

#[test]
fn compiler_multiple_shrinks_budget_and_empty_summary_match_original() {
    let key = fixtures::key();
    let mut state = fixture(&key, 3, SideEffectClass::ReadOnly);
    let mut empty = fixtures::artifact("", 900);
    empty.summary_was_truncated = true;
    state.observed = crate::causal_tree::state::ObservedState::from_runtime_records(
        Vec::new(),
        vec![
            empty,
            fixtures::artifact(&"neutral evidence ".repeat(15), 1000),
            fixtures::artifact(&"project facts ".repeat(15), 1000),
            fixtures::artifact(&"release notes ".repeat(15), 2000),
        ],
        0,
    );
    let (initial_projection, initial_manifest) = redacted_projection(&state).unwrap();
    let initial_cuts = initial_manifest
        .truncated
        .iter()
        .filter(|p| **p == "artifacts.summary")
        .count();
    let set = legal(&state);
    let mut policy = CompilerPolicy {
        key: &key,
        model: Some("prx_model"),
        expires_at: Instant::now() + Duration::from_secs(30),
        limits: RequestLimits::default(),
    };
    let mut found = None;
    // Choose from the literal old oracle before comparing new behavior.
    for bytes in (1000..=8000).step_by(32) {
        policy.limits.max_request_bytes = bytes;
        if let Ok(old) = original_compile(&state, &set, &policy) {
            let cuts = old
                .request
                .manifest()
                .projection_audit()
                .truncated_fields
                .iter()
                .filter(|p| p.as_str() == "artifacts.summary")
                .count();
            if cuts >= initial_cuts + 2 {
                found = Some((bytes, old));
                break;
            }
        }
    }
    let (bytes, old) = found.expect("at least two actual retry shrinks plus existing truncation");
    policy.limits.max_request_bytes = bytes;
    let initial_artifacts = initial_projection.as_json()["artifacts"].as_array().unwrap();
    let final_artifacts = old.request.state().as_json()["artifacts"].as_array().unwrap();
    let cleared = initial_artifacts
        .iter()
        .zip(final_artifacts)
        .filter(|(a, b)| a["summary"].as_str().is_some_and(|s| !s.is_empty()) && b["summary"] == "")
        .count();
    assert!(cleared >= 2, "at least two nonempty summaries actually shrink");
    let new = QuestionCompiler::compile(&state, &set, &policy).unwrap();
    assert_same_compiled(&old, &new, &key);
    policy.limits.max_request_bytes = 64;
    assert!(matches!(
        original_compile(&state, &set, &policy),
        Err(CompileError::Budget)
    ));
    assert!(matches!(
        QuestionCompiler::compile(&state, &set, &policy),
        Err(CompileError::Budget)
    ));
    policy.limits = RequestLimits::default();
    policy.model = Some("");
    assert!(matches!(
        original_compile(&state, &set, &policy),
        Err(CompileError::Protocol(DecisionError::InvalidRequest(
            RequestRule::ModelInvalid
        )))
    ));
    assert!(matches!(
        QuestionCompiler::compile(&state, &set, &policy),
        Err(CompileError::Protocol(DecisionError::InvalidRequest(
            RequestRule::ModelInvalid
        )))
    ));
}

#[test]
fn compiler_mention_approval_bindings_and_forced_choices_match_original() {
    use crate::causal_tree::branch::OpId;
    let key = fixtures::key();
    let mut state = fixture(&key, 1, SideEffectClass::LocalWrite);
    let mut derived = state.derived.candidates().to_vec();
    let mentioned = derived.first_mut().expect("nonempty component candidates");
    mentioned.exact_mention = true;
    let candidate = mentioned.candidate.clone();
    let id = candidate.candidate_id().clone();
    state.derived = DerivedState::new(derived).unwrap();
    let mut trusted = state.trusted.parts().clone();
    trusted.approval_pending = true;
    state.trusted = TrustedState::from_runtime(trusted);
    let mut set = legal(&state);
    let CandidateSet::Legal {
        exact_mentions,
        approval_scopes,
        ..
    } = &mut set
    else {
        panic!("legal component fixture")
    };
    exact_mentions.insert(id.clone());
    approval_scopes.insert(
        id,
        vec![PendingApproval {
            candidate,
            op_ids: vec![OpId::new("prx_write_b"), OpId::new("prx_write_a")],
        }],
    );
    let policy = CompilerPolicy {
        key: &key,
        model: Some("prx_model"),
        expires_at: Instant::now() + Duration::from_secs(30),
        limits: RequestLimits::default(),
    };
    let original = original_compile(&state, &set, &policy).unwrap();
    let current = QuestionCompiler::compile(&state, &set, &policy).unwrap();
    assert!(!original.request.manifest().cacheable());
    assert!(!original.forced_choices.is_empty());
    assert_same_compiled(&original, &current, &key);
    let neutral = fixture(&key, 1, SideEffectClass::LocalWrite);
    let neutral_set = legal(&neutral);
    let neutral = original_compile(&neutral, &neutral_set, &policy).unwrap();
    assert_ne!(
        original.request.manifest().candidate_set_hash(),
        neutral.request.manifest().candidate_set_hash()
    );
}

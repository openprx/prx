#![cfg(test)]
//! Local checks precede typed normalization. Run only outside measured windows.
//! Reconstructed request fields are component evidence, not retained wire data.

#![allow(clippy::expect_used, clippy::panic)] // Invalid owned fixtures fail immediately.

use super::*;
use crate::causal_tree::branch::{CandidateId, CapabilityTarget};
use crate::causal_tree::decision_engine::{EngineDecision, FallbackCause};
use crate::causal_tree::gates::GateOutcome;
use crate::causal_tree::outcome::DecisionRecordBuilder;
use crate::causal_tree::questions::{CompilerPolicy, QuestionCompiler};
use crate::decision::factory::ServedBy;
use crate::decision::wire::SystemOneCodec;
use crate::decision::{CandidateQuestion, DecisionQuestion, GlobalQuestion, QuestionId, RequestLimits};

// Every local ID has one canonical identity. Namespace presence distinguishes
// an exact mention from a family fallback even if both use the same full ref.
struct CanonicalIds {
    candidates: BTreeMap<String, String>,
    questions: BTreeMap<String, String>,
}

impl CanonicalIds {
    fn new(
        hop: &EngineDecision,
        catalog: &crate::tools::capability::catalog::CatalogView<'_>,
        key: &crate::decision::DecisionKey,
    ) -> Self {
        let mut stable = BTreeMap::new();
        let mut local_seen = BTreeSet::new();
        for candidate in candidates(&hop.candidates) {
            let id = candidate.candidate_id();
            assert!(local_seen.insert(id.as_str()));
            let mut seen_refs = BTreeSet::new();
            let mut refs: Vec<_> = candidate.required_capabilities().iter().map(|reference| {
                assert!(seen_refs.insert(reference.id.clone()),"full refs are unique");
                assert_eq!(reference.generation, catalog.generation());
                assert!(catalog.contains(&reference.id));
                let entry = catalog.entry_any(&reference.id).expect("visible full ref");
                assert_eq!(reference.fingerprint, entry.fingerprint);
                assert!(!entry.public_name.starts_with("prx_perf_"));
                if let Some(namespace) = id.namespace() { assert_eq!(namespace, Ns8::of(key, &entry.namespace)); }
                json!({"id":reference.id.as_str(),"fingerprint":format!("{:?}",reference.fingerprint),"namespace":entry.namespace})
            }).collect();
            refs.sort_by_key(Value::to_string);
            assert!(id.namespace().is_none() || !refs.is_empty());
            let semantic_namespace: Vec<_> = if id.namespace().is_some() {
                refs.iter()
                    .map(|r| r.get("namespace").expect("semantic namespace").clone())
                    .collect()
            } else {
                Vec::new()
            };
            let identity = json!({"action":candidate.action().as_str(),"target":candidate.target().as_str(),
                "has_namespace":id.namespace().is_some(),"namespaces":semantic_namespace,"refs":refs})
            .to_string();
            assert!(
                stable.insert(identity, id.clone()).is_none(),
                "canonical candidate identity must be bijective"
            );
        }
        let mut result = Self {
            candidates: BTreeMap::new(),
            questions: BTreeMap::new(),
        };
        for (index, id) in stable.into_values().enumerate() {
            let canonical = format!("candidate_{index}");
            assert!(
                result
                    .candidates
                    .insert(id.as_str().to_owned(), canonical.clone())
                    .is_none()
            );
            for kind in CandidateQuestion::ALL {
                assert!(
                    result
                        .questions
                        .insert(
                            QuestionId::candidate(&id.key(), kind).as_str().to_owned(),
                            format!("{canonical}.{}", kind.as_str())
                        )
                        .is_none()
                );
            }
        }
        for kind in GlobalQuestion::ALL {
            let id = QuestionId::global(kind).as_str().to_owned();
            assert!(result.questions.insert(id.clone(), id).is_none());
        }
        result
    }
    fn candidate(&self, id: &str) -> &str {
        self.candidates.get(id).expect("known local candidate")
    }
    fn question(&self, id: &str) -> &str {
        self.questions.get(id).expect("known local question")
    }
    fn candidate_map(&self, value: &Value) -> Value {
        Value::Object(
            value
                .as_object()
                .expect("candidate map")
                .iter()
                .map(|(id, v)| (self.candidate(id).to_owned(), v.clone()))
                .collect(),
        )
    }
    fn question_map(&self, value: &Value) -> Value {
        Value::Object(
            value
                .as_object()
                .expect("question map")
                .iter()
                .map(|(id, v)| (self.question(id).to_owned(), v.clone()))
                .collect(),
        )
    }
    fn projection(&self, value: &Value) -> Value {
        let mut value = value.clone();
        for candidate in value
            .get_mut("candidates")
            .and_then(Value::as_array_mut)
            .expect("projected candidates")
        {
            let id = candidate
                .get("id")
                .and_then(Value::as_str)
                .expect("projected candidate id");
            let canonical = self.candidate(id).to_owned();
            candidate
                .as_object_mut()
                .expect("candidate object")
                .insert("id".to_owned(), json!(canonical));
        }
        assert!(
            value
                .get("failed_tool_fingerprints")
                .and_then(Value::as_array)
                .expect("failure facts")
                .is_empty(),
            "these fixtures have no failed call producer; do not normalize away failure evidence"
        );
        value
    }
}

fn answer_value(answer: &crate::decision::DecisionAnswer) -> Value {
    match answer {
        crate::decision::DecisionAnswer::Choice(value) => {
            json!({"kind":"choice","chosen":value.chosen.as_str(),
            "probabilities":value.probabilities.iter().map(|(option,p)|(option.as_str().to_owned(),p.get())).collect::<BTreeMap<_,_>>(),"confidence":value.confidence.get()})
        }
        crate::decision::DecisionAnswer::Score(value) => {
            json!({"kind":"score","distribution":value.distribution.iter().map(|p|p.get()).collect::<Vec<_>>(),
            "expected":value.expected,"confidence":value.confidence.get()})
        }
        crate::decision::DecisionAnswer::Noul(value) => {
            json!({"kind":"noul","probability_true":value.probability_true.get()})
        }
    }
}

fn question_body(question: &DecisionQuestion) -> Value {
    match question {
        DecisionQuestion::Choice { instructions, criteria } => {
            json!({"type":"choice","instructions":instructions.as_json(),
            "criteria":criteria.iter().map(|(id,description)|(id.as_str().to_owned(),description.as_json().clone())).collect::<BTreeMap<_,_>>()})
        }
        DecisionQuestion::Score { instructions, levels } => {
            json!({"type":"score","instructions":instructions.as_json(),
            "levels":levels.iter().map(crate::decision::StructuredValue::as_json).collect::<Vec<_>>()})
        }
        DecisionQuestion::Noul { instructions, criteria } => {
            json!({"type":"noul","instructions":instructions.as_json(),
            "criteria":criteria.as_ref().map(|c|json!({"true":c.when_true.as_ref().map(crate::decision::StructuredValue::as_json),
                "false":c.when_false.as_ref().map(crate::decision::StructuredValue::as_json)}))})
        }
    }
}

fn component_request(service: &TurnDecisionService, hop: &EngineDecision, ids: &CanonicalIds, model: &str) -> Value {
    if !hop.request_compiled {
        assert!(hop.audit_trace.is_none());
        return Value::Null;
    }
    let trace = hop.audit_trace.as_ref().expect("actual successful prediction trace");
    let compiled = QuestionCompiler::compile(
        &hop.state,
        &hop.candidates,
        &CompilerPolicy {
            key: &service.state_key,
            model: Some(model),
            expires_at: Instant::now() + Duration::from_secs(5),
            limits: RequestLimits {
                max_request_bytes: service.generation.effective.causal_tree.backend.max_request_bytes,
                ..RequestLimits::default()
            },
        },
    )
    .expect("component reconstruction from actual successful hop");
    let request = compiled.request();
    if let CandidateSet::Legal {
        candidates,
        exact_mentions,
        approval_scopes,
        ..
    } = &hop.candidates
    {
        assert_eq!(
            request.manifest().candidate_set_hash(),
            crate::causal_tree::questions::hash_candidate_set(candidates, exact_mentions, approval_scopes)
        );
    } else {
        panic!("compiled hop must be Legal");
    }
    assert_eq!(request.manifest().state_fingerprint(), trace.state_fingerprint);
    assert_eq!(
        trace.state_fingerprint,
        hop.state.material_fingerprint(&service.state_key)
    );
    assert_eq!(request.manifest().policy_version().as_ref(), trace.policy_version);
    assert_eq!(request.manifest().catalog_generation(), trace.catalog_generation);
    assert_eq!(
        request.questions().keys().map(|id| id.as_str()).collect::<Vec<_>>(),
        trace.answers.keys().map(String::as_str).collect::<Vec<_>>()
    );
    assert_eq!(
        request
            .manifest()
            .omitted_questions()
            .iter()
            .map(|(id, reason)| (id.as_str().to_owned(), reason.as_str()))
            .collect::<BTreeMap<_, _>>(),
        trace.omitted_questions
    );
    let forced: BTreeMap<_, _> = [GlobalQuestion::NextAction, GlobalQuestion::CapabilityTarget]
        .into_iter()
        .filter_map(|q| {
            compiled
                .forced_choice(q)
                .map(|option| (q.as_str().to_owned(), option.as_str().to_owned()))
        })
        .collect();
    assert_eq!(forced, trace.forced_choices);
    assert!(compiled.forced_choices_match(candidates(&hop.candidates)));
    let retain = service
        .generation
        .effective
        .causal_tree
        .store
        .record_redacted_projection;
    assert_eq!(trace.redacted_projection.is_some(), retain);
    if let Some(projection) = &trace.redacted_projection {
        assert_eq!(
            serde_json::from_str::<Value>(projection).expect("actual opt-in projection"),
            *request.state().as_json()
        );
    }
    let response =
        crate::decision::builtin::BuiltinDecisionBackend::answer(request).expect("actual component builtin response");
    let validation = crate::decision::ValidationContext {
        received_at: Instant::now(),
        max_response_bytes: service.generation.effective.causal_tree.backend.max_response_bytes,
        version_policy: crate::decision::validate::ModelVersionPolicy::ExactPinned(
            crate::decision::builtin::BuiltinDecisionBackend::identity().configured_model,
        ),
        local_backend: crate::decision::builtin::BuiltinDecisionBackend::identity(),
    };
    let component = crate::decision::DecisionResponseValidator::validate(request, response, &validation)
        .expect("actual component response validation");
    let original = &hop.prediction.as_ref().expect("original prediction").served;
    let originals: BTreeMap<_, _> = request
        .questions()
        .keys()
        .map(|id| {
            (
                id.as_str().to_owned(),
                answer_value(original.answer(id).expect("original answer")),
            )
        })
        .collect();
    let trace_values: BTreeMap<_, _> = trace
        .answers
        .iter()
        .map(|(id, answer)| {
            use crate::causal_tree::decision_engine::PredictionAuditAnswer;
            let value = match answer {
                PredictionAuditAnswer::Choice {
                    chosen,
                    probabilities,
                    confidence,
                } => json!({"kind":"choice","chosen":chosen,"probabilities":probabilities,"confidence":confidence}),
                PredictionAuditAnswer::Score {
                    distribution,
                    expected,
                    confidence,
                } => json!({"kind":"score","distribution":distribution,"expected":expected,"confidence":confidence}),
                PredictionAuditAnswer::Noul { probability_true } => {
                    json!({"kind":"noul","probability_true":probability_true})
                }
            };
            (id.clone(), value)
        })
        .collect();
    assert_eq!(
        trace_values, originals,
        "actual trace numeric values match original served answers"
    );
    for id in request.questions().keys() {
        assert_eq!(
            answer_value(component.answer(id).expect("component answer")),
            answer_value(
                hop.prediction
                    .as_ref()
                    .expect("actual original prediction")
                    .served
                    .answer(id)
                    .expect("original answer")
            )
        );
    }
    let wire = SystemOneCodec::new(request, &service.state_key)
        .expect("component codec")
        .encode_request()
        .expect("component wire encoding");
    let wire_json: Value = serde_json::from_slice(&wire).expect("component wire shape");
    let keys: BTreeSet<_> = wire_json
        .as_object()
        .expect("wire object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, BTreeSet::from(["model", "questions", "state"]));
    assert_eq!(wire_json.get("state"), Some(request.state().as_json()));
    assert_eq!(
        wire_json
            .get("questions")
            .and_then(Value::as_object)
            .expect("wire questions")
            .len(),
        request.questions().len()
    );
    let audit = request.manifest().projection_audit();
    json!({"evidence":"component reconstruction; original sealed question bodies/cuts/wire bytes not retained",
        "model":request.model(),"projection":ids.projection(request.state().as_json()),
        "questions":request.questions().iter().map(|(id,q)|(ids.question(id.as_str()).to_owned(),question_body(q))).collect::<BTreeMap<_,_>>(),
        "omissions":request.manifest().omitted_questions().iter().map(|(id,r)|(ids.question(id.as_str()).to_owned(),r.as_str())).collect::<BTreeMap<_,_>>(),
        "forced_choices":forced,"truncated_fields":audit.truncated_fields,"redacted_texts":audit.redacted_texts,
        "wire_bytes":wire.len(),"original_projection_matched":retain})
}

// Schema-v1 record checksum list/order, identical to store::RECORD_HASH_COLUMNS.
// This is an integrity checksum; the source's material fingerprint is keyed.
const CHECKSUM_COLUMNS: [&str; 40] = [
    "decision_id",
    "created_at_ms",
    "workspace_id",
    "owner_id",
    "session_key",
    "run_id",
    "task_id",
    "source_event_id",
    "causation_id",
    "channel",
    "enabled_snapshot",
    "state_fingerprint",
    "redacted_projection",
    "protocol_version",
    "policy_version",
    "catalog_generation",
    "scorer_version",
    "backend_kind",
    "backend_locality",
    "configured_model",
    "claimed_model",
    "provider_request_id",
    "candidates_json",
    "gates_json",
    "distributions_json",
    "builtin_shadow_json",
    "features_json",
    "selected_candidate_id",
    "directive_kind",
    "fallback_order_json",
    "abstention_reason",
    "propensity",
    "explored",
    "latency_ms",
    "usage_json",
    "attempts_json",
    "error_class",
    "cache_hit",
    "response_sha256",
    "legacy_v1",
];

fn assert_record_checksum(connection: &rusqlite::Connection, workspace: &str, owner: &str, decision: &str) {
    use rusqlite::types::Value as SqlValue;
    use sha2::{Digest as _, Sha256};
    let query = format!(
        "SELECT {},record_hash FROM decisions WHERE workspace_id=?1 AND owner_id=?2 AND decision_id=?3",
        CHECKSUM_COLUMNS.join(",")
    );
    let (values, stored): (Vec<SqlValue>, String) = connection
        .query_row(&query, rusqlite::params![workspace, owner, decision], |row| {
            Ok((
                (0..CHECKSUM_COLUMNS.len())
                    .map(|index| row.get(index))
                    .collect::<rusqlite::Result<Vec<SqlValue>>>()?,
                row.get(CHECKSUM_COLUMNS.len())?,
            ))
        })
        .expect("exact owner-scoped checksum row");
    let mut hasher = Sha256::new();
    hasher.update(b"prx.decision.record_hash.v1\0");
    for (name, value) in CHECKSUM_COLUMNS.iter().zip(values) {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        match value {
            SqlValue::Null => hasher.update(b"n"),
            SqlValue::Integer(number) => {
                hasher.update(b"i");
                hasher.update(number.to_be_bytes());
            }
            SqlValue::Real(number) => {
                hasher.update(b"r");
                hasher.update(number.to_bits().to_be_bytes());
            }
            SqlValue::Text(text) => {
                hasher.update(b"t");
                hasher.update(u64::try_from(text.len()).expect("fixture text length").to_be_bytes());
                hasher.update(text.as_bytes());
            }
            SqlValue::Blob(bytes) => {
                hasher.update(b"b");
                hasher.update(u64::try_from(bytes.len()).expect("fixture blob length").to_be_bytes());
                hasher.update(bytes);
            }
        }
    }
    assert_eq!(hex::encode(hasher.finalize()), stored, "versioned record checksum");
}

pub(super) fn capture(service: &TurnDecisionService, proof: &AgentTurnProof, context: &TurnDecisionContext) -> Value {
    let catalog = service.catalog.view(&CatalogScope::default());
    let model = service.resolve_agent_model(proof).expect("actual pinned model");
    let model_ref = format!("{}:{}", model.provider_label(), model.model());
    let exclusions = effective_agent_exclusions(
        &service.generation.effective,
        &service.registry,
        &catalog,
        model.model(),
        &service.policy_version,
        &service.state_key,
    );
    let retained_projection = service
        .generation
        .effective
        .causal_tree
        .store
        .record_redacted_projection;
    let expected = DecisionRecordBuilder::build_enabled_chain(context, &service.state_key, true, retained_projection)
        .expect("real context record conversion");
    let connection =
        rusqlite::Connection::open_with_flags(service.store.path(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("isolated read-only witness");
    // This fixture owns exactly the default Agent surface plus its direct names.
    let fixture_surface = SurfacePolicy {
        allowed_tools: None,
        direct_names: catalog
            .iter_entries()
            .filter(|entry| entry.exposure == crate::tools::capability::catalog::Exposure::Direct)
            .map(|entry| entry.public_name.clone())
            .collect(),
        ..SurfacePolicy::default()
    };
    assert_eq!(
        pinned_policy_version(
            &service.generation.effective,
            &service.security,
            &fixture_surface,
            &service.state_key
        )
        .expect("local pinned fixture policy"),
        service.policy_version
    );
    assert_eq!(expected.len(), context.hop_ids().len());
    let mut unique_rows = BTreeSet::new();
    let mut hops = Vec::new();
    let mut raw_bindings = Vec::new();
    for (index, (hop, record)) in context.audit_hops().zip(&expected).enumerate() {
        assert_eq!(context.hop_ids().get(index), Some(&hop.decision_id));
        validate_agent_retrieval_provenance(
            proof,
            hop.state.trusted.parts(),
            &model_ref,
            &service.policy_version,
            &exclusions.names,
            exclusions.model_admission.as_deref(),
        )
        .expect("source-owned hop provenance");
        assert_eq!(hop.state.trusted.parts().catalog_generation, catalog.generation());
        let ids = CanonicalIds::new(hop, &catalog, &service.state_key);
        let admitted = candidates(&hop.candidates);
        let initial = match &hop.candidates {
            CandidateSet::Legal { report, .. } | CandidateSet::NoLegalPath { report, .. } => report,
        };
        match &hop.candidates {
            CandidateSet::Legal {
                truncated,
                approval_scopes,
                ..
            } => {
                assert!(
                    truncated.is_empty() && approval_scopes.is_empty(),
                    "authority-free untruncated fixture"
                );
            }
            CandidateSet::NoLegalPath { approval_only, .. } => assert!(approval_only.is_empty()),
        }
        assert!(hop.exploration_propensities.is_empty(), "these fixtures never explore");
        for candidate in admitted {
            assert_eq!(
                candidate.approval(),
                &crate::causal_tree::branch::ApprovalRequirement::None
            );
        }
        raw_bindings.push(json!({"index":index,"decision_id":hop.decision_id.to_string(),
            "catalog_generation":catalog.generation().to_hex(),"state_fingerprint":hop.state.material_fingerprint(&service.state_key),
            "policy_version":service.policy_version.as_ref(),
            "refs":admitted.iter().map(|c|json!({"candidate_id":c.candidate_id().as_str(),
                "refs":c.required_capabilities().iter().map(|r|json!({"id":r.id.as_str(),"generation":r.generation.to_hex(),
                    "fingerprint":format!("{:?}",r.fingerprint)})).collect::<Vec<_>>()})).collect::<Vec<_>>() }));
        let selected = match &hop.selection {
            Selection::Commit { candidate_id, .. } | Selection::Recover { candidate_id, .. } => candidate_id.clone(),
            Selection::Abstain { action, .. } => CandidateId::new(*action, CapabilityTarget::None, None),
            Selection::RequestApproval { .. } | Selection::CollectApprovalArguments(_) => {
                panic!("no fabricated approval producer")
            }
        };
        assert!(admitted.iter().any(|c| c.candidate_id() == &selected));
        assert_eq!(initial.gate_set_hash, hop.post_rehearsal.gate_set_hash);
        assert_eq!(initial.gate_set_hash, hop.pre_commit.gate_set_hash);
        assert_eq!(initial.outcomes.get(&selected), Some(&GateOutcome::Passed));
        assert_eq!(hop.pre_commit.outcomes.get(&selected), Some(&GateOutcome::Passed));
        assert_eq!(hop.post_rehearsal.outcomes.get(&selected), Some(&GateOutcome::Passed));
        if let Some(prediction) = &hop.prediction {
            assert_eq!(
                prediction.served, prediction.builtin_shadow,
                "exact builtin-only shadow"
            );
            assert!(prediction.served_endpoint.is_none());
            assert_eq!(prediction.served.claimed_backend(), "builtin");
            assert_eq!(
                prediction.served.claimed_model(),
                prediction.served.identity().configured_model.as_ref()
            );
            assert!(!prediction.served.request_id().is_empty());
            assert_eq!(prediction.served.usage(), &crate::decision::DecisionUsage::default());
            assert!(prediction.served.provider_request_id().is_none() && prediction.served.response_sha256().is_none());
            assert!(hop.request_compiled && hop.scored_count > 0 && hop.audit_trace.is_some());
            assert_eq!(prediction.served_by, ServedBy::Builtin);
            assert_eq!(prediction.served_locality, crate::decision::BackendLocality::Builtin);
            assert_eq!(prediction.served.identity().kind, crate::decision::BackendKind::Builtin);
            assert_eq!(
                prediction.served.identity(),
                &crate::decision::builtin::BuiltinDecisionBackend::identity()
            );
            assert_eq!(prediction.reason, None);
            assert!(prediction.attempts.is_empty());
            assert_eq!(
                prediction.fallback_order,
                vec![crate::decision::builtin::BuiltinDecisionBackend::identity()]
            );
            assert!(hop.fallback_cause.is_none());
        } else {
            assert!(!hop.request_compiled && hop.scored_count == 0 && hop.audit_trace.is_none());
            assert_eq!(hop.fallback_cause, Some(FallbackCause::HopStopped));
        }
        let workspace = service.store.workspace_id().expect("actual store workspace");
        let row = service
            .store
            .reader()
            .get(
                &DecisionScope::Owner {
                    workspace_id: Arc::clone(&workspace),
                    owner_id: Arc::from(proof.owner_id()),
                },
                &hop.decision_id,
            )
            .expect("owner-scoped durable read")
            .expect("persisted actual hop");
        assert_eq!(row.decision_id, hop.decision_id);
        assert!(
            unique_rows.insert(row.decision_id.to_string()),
            "chain IDs must be unique"
        );
        assert_eq!(row.latency_ms, i64::try_from(record.latency_ms).expect("local latency"));
        assert_eq!(record.latency_ms, hop.elapsed_ms);
        assert_eq!(
            row.prior_decision_id,
            index.checked_sub(1).and_then(|n| context.hop_ids().get(n)).copied()
        );
        assert_eq!(row.session_key.as_deref(), Some(proof.session_key()));
        assert_eq!(row.run_id.as_deref(), Some(proof.run_id()));
        assert_eq!(row.policy_version, service.policy_version.as_ref());
        assert_eq!(row.candidates, record.candidates);
        assert_eq!(row.selected_candidate_id.as_deref(), Some(selected.as_str()));
        assert_eq!(row.directive_kind, selected.action().as_str());
        assert_eq!(row.created_at_ms, record.created_at_ms);
        assert_eq!(row.task_id, record.lineage.task_id.as_deref().map(str::to_owned));
        assert_eq!(row.channel, record.lineage.channel.as_deref().map(str::to_owned));
        assert_eq!(row.scorer_version, record.scorer_version);
        assert_eq!(row.backend_endpoint, record.backend_endpoint);
        assert_eq!(row.builtin_shadow, record.builtin_shadow);
        assert_eq!(row.fallback_order, record.fallback_order);
        assert_eq!(row.propensity, record.propensity);
        assert_eq!(row.explored, record.explored);
        assert_eq!(row.attempts, record.attempts);
        assert_eq!(row.backend_kind, record.backend_kind);
        assert_eq!(row.backend_locality, record.backend_locality);
        assert_eq!(row.configured_model, record.configured_model);
        assert_eq!(row.distributions, record.distributions);
        assert_eq!(row.abstention_reason, record.abstention_reason);
        assert!(row.enabled_snapshot && row.has_owner && !row.legacy_v1 && !row.cache_hit && row.error_class.is_none());
        let stored:(String,String,String,String,Option<String>,Option<String>)=connection.query_row(
            "SELECT state_fingerprint,catalog_generation,gates_json,features_json,redacted_projection,source_event_id FROM decisions WHERE workspace_id=?1 AND owner_id=?2 AND decision_id=?3",
            rusqlite::params![workspace.as_ref(),proof.owner_id(),hop.decision_id.to_string()],
            |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))).expect("exact scope read-only omitted-field witness");
        let extra: (Option<String>,u32,Option<String>,Option<String>,Option<String>,Option<String>) = connection.query_row(
            "SELECT causation_id,protocol_version,claimed_model,provider_request_id,usage_json,response_sha256 FROM decisions WHERE workspace_id=?1 AND owner_id=?2 AND decision_id=?3",
            rusqlite::params![workspace.as_ref(),proof.owner_id(),hop.decision_id.to_string()],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))).expect("additional exact owned fields");
        assert_eq!(extra.0.as_deref(), record.lineage.causation_id.as_deref());
        assert_eq!(extra.1, record.protocol_version);
        assert_eq!(extra.2, record.claimed_model);
        assert_eq!(extra.3, record.provider_request_id);
        assert_eq!(
            extra
                .4
                .as_deref()
                .map(|v| serde_json::from_str::<Value>(v).expect("usage JSON")),
            record.usage
        );
        assert_eq!(extra.5, record.response_sha256);
        assert_eq!(
            record.lineage.source_event_id,
            hop.state.trusted.parts().source_event_id
        );
        assert_eq!(
            record.lineage.causation_id,
            hop.state.trusted.parts().causation_event_id
        );
        let count: i64 = connection
            .query_row(
                "SELECT count(*) FROM decisions WHERE workspace_id=?1 AND owner_id=?2 AND decision_id=?3",
                rusqlite::params![workspace.as_ref(), proof.owner_id(), hop.decision_id.to_string()],
                |row| row.get(0),
            )
            .expect("exact chain row count");
        assert_eq!(count, 1);
        assert_record_checksum(
            &connection,
            workspace.as_ref(),
            proof.owner_id(),
            &hop.decision_id.to_string(),
        );
        assert_eq!(stored.0, hop.state.material_fingerprint(&service.state_key));
        assert_eq!(stored.1, catalog.generation().to_hex());
        assert_eq!(
            stored.2,
            serde_json::to_string(&record.gates).expect("exact gate serialization")
        );
        assert_eq!(
            stored.3,
            serde_json::to_string(&record.features).expect("exact numeric feature serialization")
        );
        assert_eq!(stored.4, record.redacted_projection);
        assert_eq!(stored.5.as_deref(), record.lineage.source_event_id.as_deref());
        let wrong_owner_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM decisions WHERE workspace_id=?1 AND owner_id=?2 AND decision_id=?3",
                rusqlite::params![workspace.as_ref(), "prx_other_owner", hop.decision_id.to_string()],
                |row| row.get(0),
            )
            .expect("negative exact scope");
        assert_eq!(wrong_owner_count, 0);
        let bodies:Vec<_>=admitted.iter().map(|candidate| {
            let id=candidate.candidate_id();
            let exact=match &hop.candidates { CandidateSet::Legal {exact_mentions,..}=>exact_mentions.contains(id),CandidateSet::NoLegalPath {..}=>false };
            let cost=candidate.estimated_cost().expect("actual bounded candidate estimate");
            let refs:Vec<_>=candidate.required_capabilities().iter().map(|reference| {
                let entry=catalog.entry_any(&reference.id).expect("already validated local ref");
                json!({"id":reference.id.as_str(),"fingerprint":format!("{:?}",reference.fingerprint),"namespace":entry.namespace})
            }).collect();
            json!({"id":ids.candidate(id.as_str()),"action":candidate.action().as_str(),"target":candidate.target().as_str(),
                "has_namespace":id.namespace().is_some(),"exact_mention":exact,"refs":refs,"effect":candidate.side_effect().as_str(),
                "approval":candidate.approval().code(),"tokens":cost.tokens,"micro_usd":cost.micro_usd,"latency_ms":candidate.estimated_latency_ms()})
        }).collect();
        let gates = Value::Object(
            record
                .gates
                .as_object()
                .expect("three actual gate passes")
                .iter()
                .map(|(pass, value)| (pass.clone(), ids.candidate_map(value)))
                .collect(),
        );
        let features = json!({"answers":ids.question_map(record.features.get("answers").expect("actual answers")),
            "omitted_questions":ids.question_map(record.features.get("omitted_questions").expect("actual omissions")),
            "forced_choices":record.features.get("forced_choices"),"scores":ids.candidate_map(record.features.get("scores").expect("actual scores")),
            "fallback_cause":record.features.get("fallback_cause"),"factory_reason":record.features.get("factory_reason")});
        let selection = match &hop.selection {
            Selection::Commit { propensity, .. } => {
                json!({"kind":"commit","id":ids.candidate(selected.as_str()),"propensity":propensity})
            }
            Selection::Recover { reason_code, .. } => {
                json!({"kind":"recover","id":ids.candidate(selected.as_str()),"reason":reason_code})
            }
            Selection::Abstain { action, reason_code } => {
                json!({"kind":"abstain","id":ids.candidate(selected.as_str()),"action":action.as_str(),"reason":reason_code})
            }
            Selection::RequestApproval { .. } | Selection::CollectApprovalArguments(_) => {
                panic!("no fabricated approval producer")
            }
        };
        let distributions = json!({"next_action":record.distributions.next_action,
            "capability_target":record.distributions.capability_target,
            "features":ids.question_map(&serde_json::to_value(&record.distributions.features).expect("feature distribution"))});
        hops.push(json!({"index":index,"candidate_count":admitted.len(),"bodies":bodies,"gates":gates,"features":features,
            "selection":selection,"gate_set_hash":hex::encode(initial.gate_set_hash),"request_compiled":hop.request_compiled,"scored_count":hop.scored_count,
            "backend_kind":row.backend_kind,"backend_locality":row.backend_locality,"configured_model":row.configured_model,
            "trusted_budgets":{"tokens":hop.state.trusted.parts().budgets.tokens,"latency_ms":hop.state.trusted.parts().budgets.latency_ms,
                "micro_usd":hop.state.trusted.parts().budgets.micro_usd},
            "distributions":distributions,"builtin_shadow":record.builtin_shadow,
            "protocol_version":record.protocol_version,"scorer_version":record.scorer_version,
            "truncated_candidates":[],"approval_scopes":{},"exploration_propensities":{},
            "durable_enabled":row.enabled_snapshot,"durable_directive":row.directive_kind,
            "component_request":component_request(service,hop,&ids,model.model())}));
    }
    assert!(
        context
            .audit_hops()
            .any(|hop| hop.request_compiled && hop.prediction.is_some() && hop.scored_count > 0)
    );
    let execution: Vec<_> = context
        .execution_refs()
        .iter()
        .map(|reference| {
            assert_eq!(reference.generation, catalog.generation());
            assert!(catalog.contains(&reference.id));
            let entry = catalog.entry_any(&reference.id).expect("actual retained execution ref");
            assert_eq!(reference.fingerprint, entry.fingerprint);
            json!({"id":reference.id.as_str(),"fingerprint":format!("{:?}",reference.fingerprint)})
        })
        .collect();
    let directive = match context.directive() {
        TurnExecutionDirective::AnswerDirectly => json!({"kind":"answer"}),
        TurnExecutionDirective::Execute { allowed_capability_ids } => {
            json!({"kind":"use_tool","capability_ids":allowed_capability_ids.iter().map(|id|id.as_str()).collect::<Vec<_>>()})
        }
        TurnExecutionDirective::Clarify { reason_code } => {
            json!({"kind":"clarify","reason":reason_code})
        }
        TurnExecutionDirective::Refuse { policy_code } => {
            json!({"kind":"refuse","reason":policy_code})
        }
        _ => panic!("unconsumed Read or fabricated approval/child context"),
    };
    assert_eq!(
        expected.last().expect("final record").directive_kind,
        directive.get("kind").and_then(Value::as_str).expect("directive kind")
    );
    assert_eq!(unique_rows.len(), expected.len());
    let workspace = service.store.workspace_id().expect("exact chain workspace");
    let mut query = connection
        .prepare(
            "SELECT decision_id FROM decisions WHERE workspace_id=?1 AND owner_id=?2 AND session_key=?3 AND run_id=?4",
        )
        .expect("exact run scoped set");
    let persisted: BTreeSet<String> = query
        .query_map(
            rusqlite::params![
                workspace.as_ref(),
                proof.owner_id(),
                proof.session_key(),
                proof.run_id()
            ],
            |row| row.get(0),
        )
        .expect("actual chain rows")
        .collect::<rusqlite::Result<_>>()
        .expect("chain ID set");
    assert_eq!(persisted, unique_rows, "no missing or extra run scoped decision rows");

    json!({"semantic":{"hops":hops,"directive":directive,"execution_refs":execution,
        "policy":service.generation.effective.causal_tree},
        "local_binding_evidence":{"hops":raw_bindings,"chain_rows":unique_rows.len(),
            "execution_refs":context.execution_refs().iter().map(|r|json!({"id":r.id.as_str(),"generation":r.generation.to_hex(),
                "fingerprint":format!("{:?}",r.fingerprint)})).collect::<Vec<_>>()}})
}

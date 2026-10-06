#![cfg(test)]
//! Original objects from one explicitly scoped observation run, never wire evidence.
//! Disabled during measured runs. Copies affect this run's timing; no clock is sampled here.

use super::*;
use crate::decision::{DecisionQuestion, DecisionRequest, DecisionRequestManifest};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::future::Future;

const MAX_EVENTS: usize = 512;
const MAX_HOPS: usize = 16;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_REQUEST_CAPTURE: usize = 512 * 1024;

tokio::task_local! {
    static ACTIVE: Arc<Mutex<Recorder>>;
}

#[derive(Clone, PartialEq, Eq)]
struct SourceIdentity {
    workspace: Arc<str>,
    owner: Option<Arc<str>>,
    session: Option<Arc<str>>,
    run: Option<Arc<str>>,
    source: Option<Arc<str>>,
}

impl SourceIdentity {
    fn of(state: &CausalState) -> Self {
        let parts = state.trusted.parts();
        Self {
            workspace: Arc::clone(&parts.workspace_id),
            owner: parts.owner_id.clone(),
            session: parts.session_key.clone(),
            run: parts.run_id.clone(),
            source: parts.source_event_id.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("original observation rejected: {self:?}")]
pub(crate) enum ObservationError {
    NestedScope,
    Bound,
    Source,
    Duplicate,
    Missing,
    Changed,
    Interrupted,
}

struct RequestObservation {
    decision: DecisionId,
    hop: u8,
    manifest: DecisionRequestManifest,
    payload: Value,
    digest: [u8; 32],
}

struct ScoreObservation {
    decision: DecisionId,
    hop: u8,
    candidate: CandidateId,
    payload: Value,
    digest: [u8; 32],
    result: CandidateScore,
    prediction: CandidatePrediction,
    inputs: ScoreInputs,
    weights: ScorerWeights,
}

/// Test-only owner. No public production DTO or authority is introduced.
pub(crate) struct Observation {
    label: &'static str,
    source: Option<SourceIdentity>,
    requests: Vec<RequestObservation>,
    scores: Vec<ScoreObservation>,
    failure: Option<ObservationError>,
}

struct Recorder {
    capture: Observation,
    events: usize,
    bytes: usize,
    event_limit: usize,
}

fn digest(value: &Value) -> Result<([u8; 32], usize), ObservationError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ObservationError::Changed)?;
    Ok((Sha256::digest(&bytes).into(), bytes.len()))
}

struct ByteBound(usize);
impl std::io::Write for ByteBound {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("observation size limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn bound_request(request: &DecisionRequest) -> Result<(), ObservationError> {
    // Fixed bound on copied value bodies plus a separate metadata/count cap.
    // The writer never allocates a serialization buffer or samples the clock.
    if request.structured_limits().max_depth > 16 {
        return Err(ObservationError::Bound);
    }
    let mut bound = ByteBound(MAX_REQUEST_CAPTURE);
    let mut value = |value: &Value| serde_json::to_writer(&mut bound, value).map_err(|_| ObservationError::Bound);
    value(request.state().as_json())?;
    if request.questions().len() > MAX_EVENTS {
        return Err(ObservationError::Bound);
    }
    for question in request.questions().values() {
        match question {
            DecisionQuestion::Choice { instructions, criteria } => {
                value(instructions.as_json())?;
                if criteria.len() > MAX_EVENTS {
                    return Err(ObservationError::Bound);
                }
                for item in criteria.values() {
                    value(item.as_json())?;
                }
            }
            DecisionQuestion::Score { instructions, levels } => {
                value(instructions.as_json())?;
                if levels.len() > MAX_EVENTS {
                    return Err(ObservationError::Bound);
                }
                for item in levels {
                    value(item.as_json())?;
                }
            }
            DecisionQuestion::Noul { instructions, criteria } => {
                value(instructions.as_json())?;
                if let Some(criteria) = criteria {
                    for item in [criteria.when_true.as_ref(), criteria.when_false.as_ref()]
                        .into_iter()
                        .flatten()
                    {
                        value(item.as_json())?;
                    }
                }
            }
        }
    }
    let manifest = request.manifest();
    if manifest.omitted_questions().len() > MAX_EVENTS
        || manifest.projection_audit().truncated_fields.len() > MAX_EVENTS
    {
        return Err(ObservationError::Bound);
    }
    let mut metadata = ByteBound(64 * 1024);
    for text in [
        Some(request.request_id()),
        request.model(),
        Some(manifest.state_fingerprint()),
        Some(manifest.candidate_set_hash()),
        Some(manifest.policy_version().as_ref()),
        Some(manifest.catalog_generation()),
    ]
    .into_iter()
    .flatten()
    .chain(request.questions().keys().map(crate::decision::QuestionId::as_str))
    .chain(manifest.omitted_questions().iter().map(|(id, _)| id.as_str()))
    .chain(manifest.projection_audit().truncated_fields.iter().map(String::as_str))
    {
        serde_json::to_writer(&mut metadata, text).map_err(|_| ObservationError::Bound)?;
    }
    for question in request.questions().values() {
        if let DecisionQuestion::Choice { criteria, .. } = question {
            for key in criteria.keys() {
                serde_json::to_writer(&mut metadata, key.as_str()).map_err(|_| ObservationError::Bound)?;
            }
        }
    }
    Ok(())
}

fn question(question: &DecisionQuestion) -> Value {
    match question {
        DecisionQuestion::Choice { instructions, criteria } => json!({
            "kind":"choice", "instructions":instructions.as_json(),
            "criteria":criteria.iter().map(|(key,value)|(key.as_str(),value.as_json())).collect::<BTreeMap<_,_>>()
        }),
        DecisionQuestion::Score { instructions, levels } => json!({
            "kind":"score", "instructions":instructions.as_json(),
            "levels":levels.iter().map(crate::decision::StructuredValue::as_json).collect::<Vec<_>>()
        }),
        DecisionQuestion::Noul { instructions, criteria } => json!({
            "kind":"noul", "instructions":instructions.as_json(),
            "criteria":criteria.as_ref().map(|value|json!({
                "true":value.when_true.as_ref().map(crate::decision::StructuredValue::as_json),
                "false":value.when_false.as_ref().map(crate::decision::StructuredValue::as_json)
            }))
        }),
    }
}

fn request_payload(request: &DecisionRequest) -> Value {
    let manifest = request.manifest();
    json!({
        "protocol_version":request.protocol_version(), "request_id":request.request_id(),
        "expiry_process_local_instant_debug":format!("{:?}", manifest.expires_at()),
        "request_fingerprint":hex::encode(manifest.request_fingerprint()),
        "state_fingerprint":manifest.state_fingerprint(), "candidate_set_hash":manifest.candidate_set_hash(),
        "policy_version":manifest.policy_version().as_ref(), "catalog_generation":manifest.catalog_generation(),
        "model":request.model(), "projection":request.state().as_json(),
        "questions":request.questions().iter().map(|(id,body)|(id.as_str(),question(body))).collect::<BTreeMap<_,_>>(),
        "omissions":manifest.omitted_questions().iter().map(|(id,reason)|(id.as_str(),reason.as_str())).collect::<BTreeMap<_,_>>(),
        "truncated_fields":manifest.projection_audit().truncated_fields,
        "redacted_texts":manifest.projection_audit().redacted_texts,
        "cacheable":manifest.cacheable()
    })
}

impl Recorder {
    fn begin(&mut self, state: &CausalState) -> Result<(), ObservationError> {
        if self.events >= self.event_limit {
            return Err(ObservationError::Bound);
        }
        let source = SourceIdentity::of(state);
        if [
            Some(source.workspace.as_ref()),
            source.owner.as_deref(),
            source.session.as_deref(),
            source.run.as_deref(),
            source.source.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|text| text.len() > 1024)
        {
            return Err(ObservationError::Bound);
        }
        if let Some(expected) = &self.capture.source {
            if expected != &source {
                return Err(ObservationError::Source);
            }
        } else {
            self.capture.source = Some(source);
        }
        Ok(())
    }

    fn reserve(&mut self, bytes: usize) -> Result<(), ObservationError> {
        if bytes > MAX_BYTES.saturating_sub(self.bytes) {
            return Err(ObservationError::Bound);
        }
        self.events += 1;
        self.bytes += bytes;
        Ok(())
    }
}

fn record(operation: impl FnOnce(&mut Recorder) -> Result<(), ObservationError>) {
    let _ = ACTIVE.try_with(|owner| {
        let mut recorder = owner.lock();
        if recorder.capture.failure.is_none() {
            // parking_lot does not poison. Keep an interrupted hook invalid even
            // if a surrounding fixture catches the panic and continues.
            recorder.capture.failure = Some(ObservationError::Interrupted);
            recorder.capture.failure = operation(&mut recorder).err();
        }
    });
}

pub(super) fn compiled(
    decision: DecisionId,
    state: &CausalState,
    compiled: &CompiledQuestionRequest,
    limits: RequestLimits,
) {
    record(|recorder| {
        recorder.begin(state)?;
        if recorder.capture.requests.len() >= MAX_HOPS {
            return Err(ObservationError::Bound);
        }
        if recorder
            .capture
            .requests
            .iter()
            .any(|request| request.decision == decision || request.hop == state.lineage.hop())
        {
            return Err(ObservationError::Duplicate);
        }
        let request = compiled.request();
        bound_request(request)?;
        let payload = json!({
            "request":request_payload(request), "max_request_bytes":limits.max_request_bytes,
            "structured_limits":{"max_depth":limits.structured.max_depth,
                "max_nodes":limits.structured.max_nodes,"max_string_bytes":limits.structured.max_string_bytes},
            "forced_choices":([GlobalQuestion::NextAction,GlobalQuestion::CapabilityTarget].into_iter()
                .filter_map(|kind|compiled.forced_choice(kind).map(|value|(kind.as_str(),value.as_str())))
                .collect::<BTreeMap<_,_>>())
        });
        let (digest, bytes) = digest(&payload)?;
        recorder.reserve(bytes)?;
        recorder.capture.requests.push(RequestObservation {
            decision,
            hop: state.lineage.hop(),
            manifest: request.manifest().clone(),
            payload,
            digest,
        });
        Ok(())
    });
}

pub(super) fn scored(
    decision: DecisionId,
    state: &CausalState,
    candidate: &CandidateId,
    values: (CandidatePrediction, ScoreInputs, ScorerWeights, CandidateScore),
    remaining: super::super::state::TurnBudgets,
) {
    record(|recorder| {
        recorder.begin(state)?;
        if !recorder
            .capture
            .requests
            .iter()
            .any(|request| request.decision == decision && request.hop == state.lineage.hop())
        {
            return Err(ObservationError::Missing);
        }
        if recorder
            .capture
            .scores
            .iter()
            .any(|score| score.decision == decision && score.candidate == *candidate)
        {
            return Err(ObservationError::Duplicate);
        }
        let (features, inputs, weights, result) = values;
        let payload = json!({
            "candidate":candidate.as_str(),
            "features_bits":([features.choice_probability,features.applicability_probability,features.success_probability,
                features.expected_gain,features.gain_uncertainty,features.semantic_risk,features.risk_uncertainty].map(f64::to_bits)),
            "estimated_micro_usd":inputs.estimated_micro_usd, "remaining_micro_usd":inputs.remaining_micro_usd,
            "estimated_latency_ms":inputs.estimated_latency_ms, "remaining_latency_ms":inputs.remaining_latency_ms,
            "rehearsal_bits":inputs.rehearsal_delta.to_bits(),
            "remaining_tokens":remaining.tokens, "remaining_budget_micro_usd":remaining.micro_usd,
            "remaining_budget_latency_ms":remaining.latency_ms,
            "scorer_version":weights.version,
            "weights_bits":([weights.w_choice,weights.w_applicable,weights.w_success,weights.w_gain,weights.w_risk,
                weights.w_uncertainty,weights.w_cost,weights.w_latency,weights.w_rehearsal].map(f64::to_bits)),
            "semantic_utility_bits":result.semantic_utility.to_bits(), "execution_utility_bits":result.execution_utility.to_bits()
        });
        let (digest, bytes) = digest(&payload)?;
        recorder.reserve(bytes)?;
        recorder.capture.scores.push(ScoreObservation {
            decision,
            hop: state.lineage.hop(),
            candidate: candidate.clone(),
            payload,
            digest,
            result,
            prediction: features,
            inputs,
            weights,
        });
        Ok(())
    });
}

/// Explicit async-task scope, safe across executor thread migration. Spawned
/// tasks do not inherit it: missing observations fail validation below.
/// The first actual hook latches source identity from its trusted engine state.
/// Callers cannot label a fabricated source as authority. The future can be an
/// actual TurnDecisionService admission; no preconstructed state is required.
/// Only owned neutral test fixtures may opt in; no runtime source is logged.
pub(crate) async fn capture<F: Future>(
    label: &'static str,
    future: F,
) -> Result<(F::Output, Observation), ObservationError> {
    capture_bounded(label, MAX_EVENTS, future).await
}

async fn capture_bounded<F: Future>(
    label: &'static str,
    event_limit: usize,
    future: F,
) -> Result<(F::Output, Observation), ObservationError> {
    if ACTIVE.try_with(|_| ()).is_ok() {
        return Err(ObservationError::NestedScope);
    }
    if event_limit > MAX_EVENTS || label.is_empty() || label.len() > 64 || !label.is_ascii() {
        return Err(ObservationError::Bound);
    }
    let owner = Arc::new(Mutex::new(Recorder {
        capture: Observation {
            label,
            source: None,
            requests: Vec::new(),
            scores: Vec::new(),
            failure: None,
        },
        events: 0,
        bytes: 0,
        event_limit,
    }));
    let output = ACTIVE.scope(Arc::clone(&owner), future).await;
    let recorder = Arc::try_unwrap(owner)
        .map_err(|_| ObservationError::Changed)?
        .into_inner();
    Ok((output, recorder.capture))
}

impl Observation {
    /// Actual successful engine outcomes are the independent inventory. Calling
    /// this with an empty or incomplete list never certifies a partial capture.
    pub(crate) fn checked_sidecar(&self, decisions: &[&EngineDecision]) -> Result<Value, ObservationError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let source = self.source.as_ref().ok_or(ObservationError::Missing)?;
        if decisions.is_empty() || decisions.len() > MAX_HOPS {
            return Err(ObservationError::Missing);
        }
        let expected: BTreeSet<_> = decisions
            .iter()
            .filter(|d| d.request_compiled)
            .map(|d| d.decision_id)
            .collect();
        let actual: BTreeSet<_> = self.requests.iter().map(|r| r.decision).collect();
        if expected != actual || expected.len() != self.requests.len() {
            return Err(ObservationError::Missing);
        }
        if decisions.iter().map(|d| d.decision_id).collect::<BTreeSet<_>>().len() != decisions.len() {
            return Err(ObservationError::Duplicate);
        }
        for decision in decisions {
            if &SourceIdentity::of(&decision.state) != source {
                return Err(ObservationError::Source);
            }
            if let Some(request) = self.requests.iter().find(|r| r.decision == decision.decision_id) {
                if request.hop != decision.state.lineage.hop()
                    || request.manifest.expires_at() != decision.deadline
                    || digest(&request.payload)?.0 != request.digest
                {
                    return Err(ObservationError::Changed);
                }
                let trace = decision.audit_trace.as_ref().ok_or(ObservationError::Missing)?;
                if trace.state_fingerprint != request.manifest.state_fingerprint()
                    || trace.catalog_generation != request.manifest.catalog_generation()
                    || trace.policy_version != request.manifest.policy_version().as_ref()
                    || request.payload.pointer("/request/omissions") != Some(&json!(trace.omitted_questions))
                    || request.payload.get("forced_choices") != Some(&json!(trace.forced_choices))
                    || decision
                        .prediction
                        .as_ref()
                        .is_none_or(|prediction| prediction.served.request_id() != request.manifest.request_id())
                {
                    return Err(ObservationError::Changed);
                }
            }
            let scores: Vec<_> = self
                .scores
                .iter()
                .filter(|score| score.decision == decision.decision_id)
                .collect();
            if scores.len() != decision.scored_count {
                return Err(ObservationError::Missing);
            }
            for score in scores {
                if score.hop != decision.state.lineage.hop()
                    || digest(&score.payload)?.0 != score.digest
                    || decision.scores.get(&score.candidate) != Some(&score.result)
                    || CausalBranchScorer::score(score.prediction, score.inputs, &score.weights)
                        .map_err(|_| ObservationError::Changed)?
                        != score.result
                {
                    return Err(ObservationError::Changed);
                }
            }
        }
        if self.scores.iter().any(|score| !actual.contains(&score.decision)) {
            return Err(ObservationError::Missing);
        }
        Ok(json!({
            "kind":"original-compiled-observation-v1", "run":self.label,
            "scope":"unmeasured original engine inputs; no HTTP wire or source-authority acceptance",
            "physical_wire":"not_observed_in_slice_a",
            "clock_provenance":{
                "kind":"process_local_monotonic_instant_debug",
                "meaning":"opaque original Instant Debug values; no epoch, wall-clock or cross-process claim",
                "request_expiry_equals_original_decision_deadline":true,
                "decisions":decisions.iter().filter(|decision|decision.request_compiled).map(|decision|json!({
                    "decision":decision.decision_id.to_string(),
                    "original_decision_deadline_debug":format!("{:?}",decision.deadline),
                    "expiry_minus_deadline_nanoseconds":0
                })).collect::<Vec<_>>()
            },
            "source":{"workspace":source.workspace.as_ref(),"owner":source.owner.as_deref(),
                "session":source.session.as_deref(),"run":source.run.as_deref(),"event":source.source.as_deref()},
            "requests":self.requests.iter().map(|r|json!({"decision":r.decision.to_string(),"hop":r.hop,"sha256":hex::encode(r.digest),"original":r.payload})).collect::<Vec<_>>(),
            "scores":self.scores.iter().map(|r|json!({"decision":r.decision.to_string(),"hop":r.hop,"sha256":hex::encode(r.digest),"actual":r.payload})).collect::<Vec<_>>()
        }))
    }
}

#[cfg(test)]
mod tests;

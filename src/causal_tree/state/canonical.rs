//! Private borrowed representation of the existing canonical state JSON.
//! Field order is the lexical order of the original serde_json object maps.

use serde::Serialize;

use super::{ApprovalRequirement, CandidateId, CausalState, OpId, SideEffectMode, artifact_age_class};

#[derive(Serialize)]
struct CanonicalState<'a> {
    derived: Vec<CanonicalCandidate<'a>>,
    goal: &'a str,
    has_conversation_history: bool,
    observed: CanonicalObserved<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    record: Option<CanonicalRecord<'a>>,
    routing_input: &'a str,
    trusted: CanonicalTrusted<'a>,
}

#[derive(Serialize)]
struct CanonicalTrusted<'a> {
    approval_grants: Vec<&'a str>,
    approval_pending: bool,
    budgets: [u64; 3],
    catalog_generation: String,
    channel: Option<&'a str>,
    constraints: Vec<&'a str>,
    delivery: [bool; 3],
    exclusions: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_admission: Option<&'a [u8; 32]>,
    owner_id: Option<&'a str>,
    policy_version: &'a str,
    resolved_model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    router_history: Option<&'a super::RouterHistoryReadBinding>,
    session_key: Option<&'a str>,
    side_effect_mode: &'static str,
    surface: &'a str,
    workspace_id: &'a str,
}

#[derive(Serialize)]
struct CanonicalObserved<'a> {
    artifacts: Vec<(&'a str, &'static str, &'a str)>,
    completed_steps: Vec<(&'a str, &'a str, Option<&'a str>)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exact_call_observations: Option<&'a super::exact_calls::ExactCallObservations>,
    retry_count: u32,
}

#[derive(Serialize)]
#[serde(untagged)]
enum CanonicalApproval<'a> {
    None([&'static str; 1]),
    Required(&'static str, Vec<&'a str>),
    Satisfied(&'static str, &'a str),
}

#[derive(Serialize)]
struct CanonicalCandidate<'a> {
    approval: CanonicalApproval<'a>,
    available: bool,
    capabilities: Vec<(&'a str, String, String)>,
    cost: Option<[u64; 2]>,
    exact_mention: bool,
    id: &'a str,
    latency_ms: Option<u32>,
    side_effect: &'a str,
    summary: &'a str,
}

#[derive(Serialize)]
struct CanonicalRecord<'a> {
    artifact_ages_ms: Vec<u64>,
    artifact_event_ids: Vec<Option<&'a str>>,
    built_at_ms: u64,
    causation_event_id: Option<&'a str>,
    hop: u8,
    prior_decision_id: Option<String>,
    run_id: Option<&'a str>,
    source_event_id: Option<&'a str>,
    step_event_ids: Vec<Option<&'a str>>,
    task_id: Option<&'a str>,
}

fn borrowed_set<'a>(items: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut values: Vec<_> = items.collect();
    values.sort_unstable();
    values.dedup();
    values
}

pub(super) fn serialize(state: &CausalState, record: bool) -> Result<String, serde_json::Error> {
    let trusted = state.trusted.parts();
    let canonical = CanonicalState {
        derived: state
            .derived_sorted()
            .map(|derived| {
                let candidate = &derived.candidate;
                CanonicalCandidate {
                    approval: match candidate.approval() {
                        ApprovalRequirement::None => CanonicalApproval::None(["none"]),
                        ApprovalRequirement::Required { op_ids } => {
                            CanonicalApproval::Required("required", borrowed_set(op_ids.iter().map(OpId::as_str)))
                        }
                        ApprovalRequirement::Satisfied { grant_id } => {
                            CanonicalApproval::Satisfied("satisfied", grant_id.as_ref())
                        }
                    },
                    available: derived.available,
                    capabilities: candidate
                        .required_capabilities()
                        .iter()
                        .map(|capability| {
                            (
                                capability.id.as_str(),
                                hex::encode(capability.fingerprint.0),
                                capability.generation.to_hex(),
                            )
                        })
                        .collect(),
                    cost: candidate
                        .estimated_cost()
                        .map(|cost| [u64::from(cost.tokens), cost.micro_usd]),
                    exact_mention: derived.exact_mention,
                    id: candidate.candidate_id().as_str(),
                    latency_ms: candidate.estimated_latency_ms(),
                    side_effect: candidate.side_effect().as_str(),
                    summary: derived.summary.as_ref(),
                }
            })
            .collect(),
        goal: state.goal.as_str(),
        has_conversation_history: state.has_conversation_history,
        observed: CanonicalObserved {
            artifacts: state
                .observed
                .artifacts()
                .iter()
                .map(|artifact| {
                    (
                        artifact.kind.as_str(),
                        artifact_age_class(artifact.age_ms),
                        artifact.summary.as_str(),
                    )
                })
                .collect(),
            completed_steps: state
                .observed
                .completed_steps()
                .iter()
                .map(|step| {
                    (
                        step.kind().as_str(),
                        step.status().as_str(),
                        step.candidate().map(CandidateId::as_str),
                    )
                })
                .collect(),
            exact_call_observations: state.observed.exact_call_observations().map(AsRef::as_ref),
            retry_count: state.observed.retry_count(),
        },
        record: record.then(|| CanonicalRecord {
            artifact_ages_ms: state
                .observed
                .artifacts()
                .iter()
                .map(|artifact| artifact.age_ms)
                .collect(),
            artifact_event_ids: state
                .observed
                .artifacts()
                .iter()
                .map(|artifact| artifact.source_event_id.as_deref())
                .collect(),
            built_at_ms: state.built_at_ms,
            causation_event_id: trusted.causation_event_id.as_deref(),
            hop: state.lineage.hop(),
            prior_decision_id: state.lineage.prior_decision_id().map(|id| id.to_string()),
            run_id: trusted.run_id.as_deref(),
            source_event_id: trusted.source_event_id.as_deref(),
            step_event_ids: state
                .observed
                .completed_steps()
                .iter()
                .map(|step| step.event_id().as_deref())
                .collect(),
            task_id: trusted.task_id.as_deref(),
        }),
        routing_input: state.routing_input.as_str(),
        trusted: CanonicalTrusted {
            approval_grants: borrowed_set(trusted.approval_grants.iter().map(AsRef::as_ref)),
            approval_pending: trusted.approval_pending,
            budgets: [
                trusted.budgets.tokens,
                trusted.budgets.latency_ms,
                trusted.budgets.micro_usd,
            ],
            catalog_generation: trusted.catalog_generation.to_hex(),
            channel: trusted.channel.as_deref(),
            constraints: borrowed_set(trusted.constraints.iter().map(|code| code.as_str())),
            delivery: [
                trusted.delivery.interactive,
                trusted.delivery.can_ask_question,
                trusted.delivery.approval_channel,
            ],
            exclusions: borrowed_set(trusted.exclusions.iter().map(AsRef::as_ref)),
            model_admission: trusted.model_admission.as_ref().map(|policy| policy.digest()),
            owner_id: trusted.owner_id.as_deref(),
            policy_version: trusted.policy_version.as_ref(),
            resolved_model: trusted.resolved_model.as_ref(),
            router_history: state.trusted.router_history(),
            session_key: trusted.session_key.as_deref(),
            side_effect_mode: match trusted.side_effect_mode {
                SideEffectMode::ReadOnly => "ReadOnly",
                SideEffectMode::ApprovalRequired => "ApprovalRequired",
                SideEffectMode::GuardedWrite => "GuardedWrite",
            },
            surface: trusted.surface.as_str(),
            workspace_id: trusted.workspace_id.as_ref(),
        },
    };
    serde_json::to_string(&canonical)
}

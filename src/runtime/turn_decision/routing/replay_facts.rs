//! Read-only facts issued after checking an actual consumed Router transition.
//! This type is not deserializable and confers no execution or storage authority.
use crate::causal_tree::ids::DecisionId;

pub(crate) struct RouterReadFacts {
    pub(crate) version: u8,
    pub(crate) readmission_succeeded: bool,
    pub(crate) failure_stage: Option<&'static str>,
    pub(crate) read_decision_id: DecisionId,
    pub(crate) read_state_fingerprint: String,
    pub(crate) selected_candidate_id: String,
    pub(crate) capability_id: String,
    pub(crate) capability_fingerprint: [u8; 32],
    pub(crate) catalog_generation: [u8; 32],
    pub(crate) domain_sha256: [u8; 32],
    pub(crate) query_sha256: [u8; 32],
    pub(crate) history_snapshot_sha256: Option<[u8; 32]>,
    pub(crate) next_decision_id: DecisionId,
    pub(crate) next_state_fingerprint: String,
    pub(crate) from_provider_binding: [u8; 32],
    pub(crate) from_model: String,
    pub(crate) to_provider_binding: [u8; 32],
    pub(crate) to_model: String,
    pub(crate) pmm_route_binding: Option<[u8; 32]>,
    pub(crate) original_budget: [u64; 3],
    pub(crate) remaining_budget: [u64; 3],
    pub(crate) source_sequence: i64,
    pub(super) _issued: (),
}

/// Same identity normalization as actual persisted provider outcomes. Only
/// keyed bytes enter the Router replay projection, never a private URL.
pub(crate) fn provider_binding(key: &crate::decision::DecisionKey, provider: &str) -> [u8; 32] {
    key.hmac(
        crate::decision::KeyDomain::StateFingerprint,
        &[
            b"router-readmission-provider-v1",
            crate::providers::redact_endpoint_userinfo(provider).as_bytes(),
        ],
    )
}

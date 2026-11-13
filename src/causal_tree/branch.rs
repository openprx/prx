use std::sync::Arc;

use crate::decision::CandidateKey;
use crate::security::runtime_key::{DecisionKey, KeyDomain};
use crate::tools::capability::contract::{
    ApprovalRequirement, CapabilityRef, CapabilityTarget, CausalActionKind, CostEstimate, SideEffectClass,
};

/// The namespace discriminator of a candidate: the first four bytes of
/// `HMAC(DecisionKey, StateFingerprint, "ns8" ‖ namespace)`, written as eight
/// hex digits.
///
/// It is keyed so that no namespace name can be recovered from it by a
/// dictionary (only bounded family summaries leave PRX, CDP section 11). It
/// is the candidate id suffix, and the diagnostic `failed_tool_fingerprints`
/// lists it for failed candidates; no backend matches failures by it, since
/// a failure marks only the candidate that failed (`failed_before`, D-061 M-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Ns8([u8; 4]);

impl Ns8 {
    /// The discriminator of `namespace` under the workspace key.
    pub(crate) fn of(key: &DecisionKey, namespace: &str) -> Self {
        let mac = key.hmac(KeyDomain::StateFingerprint, &[b"ns8", namespace.as_bytes()]);
        Self([mac[0], mac[1], mac[2], mac[3]])
    }

    /// The raw four bytes (the `ns8` part of a `CandidateKey`).
    pub(crate) const fn bytes(self) -> [u8; 4] {
        self.0
    }

    /// Eight lowercase hex digits.
    pub(crate) fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

/// A stable candidate id in the dash form `{action}-{target}[-{ns8}]`
/// (CDP-PLAN M17). There is no sequence counter: the same action, target and
/// namespace always give the same id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CandidateId {
    action: CausalActionKind,
    target: CapabilityTarget,
    namespace: Option<Ns8>,
    text: Arc<str>,
}

impl CandidateId {
    /// The id of `action` on `target`, optionally within one namespace.
    pub(crate) fn new(action: CausalActionKind, target: CapabilityTarget, namespace: Option<Ns8>) -> Self {
        let key = CandidateKey::from_parts(action.as_str(), target.as_str(), namespace.map(Ns8::bytes));
        Self {
            action,
            target,
            namespace,
            text: Arc::from(key.as_str()),
        }
    }

    /// The id as text.
    pub(crate) fn as_str(&self) -> &str {
        &self.text
    }

    /// The action the id names.
    pub(crate) const fn action(&self) -> CausalActionKind {
        self.action
    }

    /// The capability target the id names.
    pub(crate) const fn target(&self) -> CapabilityTarget {
        self.target
    }

    /// The namespace discriminator, if the candidate is namespace-specific.
    pub(crate) const fn namespace(&self) -> Option<Ns8> {
        self.namespace
    }

    /// The protocol key used in question and option ids.
    pub(crate) fn key(&self) -> CandidateKey {
        CandidateKey::from_parts(
            self.action.as_str(),
            self.target.as_str(),
            self.namespace.map(Ns8::bytes),
        )
    }
}

/// One executable combination of action and capability target (CDP 5.1).
///
/// Its fields are private and it has no setter: the only constructor is
/// [`CausalCandidate::new`], and the action and target are read from the
/// id, so a candidate's id always matches its action and target (R-CDP16
/// M-2; the state v1 rule "id matches its own action and target").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CausalCandidate {
    candidate_id: CandidateId,
    required_capabilities: Vec<CapabilityRef>,
    side_effect: SideEffectClass,
    approval: ApprovalRequirement,
    estimated_cost: Option<CostEstimate>,
    estimated_latency_ms: Option<u32>,
    decision_overhead_ms: Option<u32>,
}

/// Everything a candidate is built from besides its action and target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CandidateSpec {
    pub(crate) namespace: Option<Ns8>,
    pub(crate) required_capabilities: Vec<CapabilityRef>,
    pub(crate) side_effect: SideEffectClass,
    pub(crate) approval: ApprovalRequirement,
    /// `None` when the cost is unknown (never read as zero).
    pub(crate) estimated_cost: Option<CostEstimate>,
    /// `None` when the latency is unknown (never read as fast; R-CDP16
    /// round 2 N-4).
    pub(crate) estimated_latency_ms: Option<u32>,
}

impl CausalCandidate {
    /// The candidate for `action` on `target`; its id is derived from them
    /// and `spec.namespace`.
    pub(crate) fn new(action: CausalActionKind, target: CapabilityTarget, spec: CandidateSpec) -> Self {
        Self {
            candidate_id: CandidateId::new(action, target, spec.namespace),
            required_capabilities: spec.required_capabilities,
            side_effect: spec.side_effect,
            approval: spec.approval,
            estimated_cost: spec.estimated_cost,
            estimated_latency_ms: spec.estimated_latency_ms,
            decision_overhead_ms: spec.estimated_latency_ms,
        }
    }

    /// Set the extra latency this candidate adds to the decision itself, the
    /// only latency G6 compares with `extra_latency_budget_ms`. It defaults to
    /// the execution estimate; a tool or retrieval candidate adds none. `None`
    /// stays unknown and is rejected by G6.
    #[must_use]
    pub(crate) const fn with_decision_overhead_ms(mut self, overhead: Option<u32>) -> Self {
        self.decision_overhead_ms = overhead;
        self
    }

    /// The stable id.
    pub(crate) const fn candidate_id(&self) -> &CandidateId {
        &self.candidate_id
    }

    /// What the candidate does.
    pub(crate) const fn action(&self) -> CausalActionKind {
        self.candidate_id.action()
    }

    /// Which capability family it acts on.
    pub(crate) const fn target(&self) -> CapabilityTarget {
        self.candidate_id.target()
    }

    /// The capabilities an execution needs, pinned to their schemas.
    pub(crate) fn required_capabilities(&self) -> &[CapabilityRef] {
        &self.required_capabilities
    }

    /// The side-effect class.
    pub(crate) const fn side_effect(&self) -> SideEffectClass {
        self.side_effect
    }

    /// The approval an execution needs.
    pub(crate) const fn approval(&self) -> &ApprovalRequirement {
        &self.approval
    }

    /// The cost estimate, `None` when unknown.
    pub(crate) const fn estimated_cost(&self) -> Option<CostEstimate> {
        self.estimated_cost
    }

    /// The estimated latency of one execution, in milliseconds, `None`
    /// when unknown; the projection sends an unknown latency as the slowest
    /// bucket.
    pub(crate) const fn estimated_latency_ms(&self) -> Option<u32> {
        self.estimated_latency_ms
    }

    /// The extra latency one decision spends on this candidate, `None` when
    /// unknown. G6 reads this, never the execution estimate.
    pub(crate) const fn decision_overhead_ms(&self) -> Option<u32> {
        self.decision_overhead_ms
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod candidate_tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::tools::capability::contract::{CapabilityId, CapabilityIdError, NODE_NAME_CHARS, OpId};

    fn key() -> Arc<DecisionKey> {
        let dir = tempfile::tempdir().expect("test: tempdir");
        DecisionKey::load_or_create(dir.path()).expect("test: key")
    }

    fn spec(namespace: Option<Ns8>) -> CandidateSpec {
        CandidateSpec {
            namespace,
            required_capabilities: Vec::new(),
            side_effect: SideEffectClass::ReadOnly,
            approval: ApprovalRequirement::None,
            estimated_cost: None,
            estimated_latency_ms: None,
        }
    }

    /// The codes are the state v1 contract's (CDP-PLAN 1.3) and the
    /// builtin's; a drift turns this red. (The equality with CDP-09's v1
    /// decoder, M14, is pinned when that decoder lands.)
    #[test]
    fn codes_match_the_state_v1_contract() {
        let actions: Vec<&str> = CausalActionKind::ALL.iter().map(|action| action.as_str()).collect();
        assert_eq!(
            actions,
            [
                "answer",
                "retrieve",
                "discover",
                "use_tool",
                "delegate",
                "clarify",
                "request_approval",
                "refuse"
            ]
        );
        for action in CausalActionKind::ALL {
            assert!(crate::decision::builtin::features::Action::from_code(action.as_str()).is_some());
        }
        let targets: Vec<&str> = CapabilityTarget::ALL.iter().map(|target| target.as_str()).collect();
        assert_eq!(
            targets,
            [
                "none",
                "memory",
                "skill",
                "mcp",
                "native_tool",
                "wasm",
                "hook",
                "filesystem",
                "shell",
                "document",
                "web",
                "im",
                "session"
            ]
        );
        let effects = [
            SideEffectClass::ReadOnly,
            SideEffectClass::LocalWrite,
            SideEffectClass::ProcessExec,
            SideEffectClass::ExternalSend,
            SideEffectClass::CredentialOrConfig,
            SideEffectClass::Destructive,
        ];
        assert!(
            effects.windows(2).all(|pair| pair[0] < pair[1]),
            "ordered by risk floor"
        );
        let risks: Vec<f64> = effects
            .iter()
            .map(|effect| crate::decision::builtin::weights::side_effect_risk(effect.as_str()))
            .collect();
        assert!(
            risks.windows(2).all(|pair| pair[0] <= pair[1]),
            "the builtin reads the same order"
        );
    }

    /// Candidate ids use the dash form, are stable (no counter), match the
    /// protocol key, and fit its grammar for every action and target.
    #[test]
    fn candidate_ids_are_stable_dash_ids_that_match_the_protocol_key() {
        let key = key();
        let namespace = Ns8::of(&key, "github");
        let with = CausalCandidate::new(CausalActionKind::UseTool, CapabilityTarget::Mcp, spec(Some(namespace)));
        assert_eq!(
            with.candidate_id().as_str(),
            format!("use_tool-mcp-{}", namespace.to_hex())
        );
        assert_eq!(with.candidate_id().key().as_str(), with.candidate_id().as_str());
        assert_eq!(with.candidate_id().namespace(), Some(namespace));
        let again = CausalCandidate::new(CausalActionKind::UseTool, CapabilityTarget::Mcp, spec(Some(namespace)));
        assert_eq!(with.candidate_id(), again.candidate_id());
        let without = CausalCandidate::new(CausalActionKind::Answer, CapabilityTarget::None, spec(None));
        assert_eq!(without.candidate_id().as_str(), "answer-none");
        let mut all = BTreeSet::new();
        for action in CausalActionKind::ALL {
            for target in CapabilityTarget::ALL {
                let id = CandidateId::new(action, target, Some(namespace));
                let text = id.as_str();
                assert!(text.len() <= 47 && text.split('-').count() == 3, "{text}");
                assert!(all.insert(text.to_string()));
            }
        }
    }

    /// R-CDP16 M-2: a candidate's action and target are read from its id, so
    /// the id always matches them; there is no field to change afterwards.
    #[test]
    fn a_candidate_id_always_matches_its_action_and_target() {
        for action in CausalActionKind::ALL {
            for target in CapabilityTarget::ALL {
                let candidate = CausalCandidate::new(action, target, spec(None));
                assert_eq!(candidate.action(), action);
                assert_eq!(candidate.target(), target);
                assert_eq!(
                    candidate.candidate_id().as_str(),
                    format!("{}-{}", action.as_str(), target.as_str())
                );
            }
        }
        let unknown = CausalCandidate::new(CausalActionKind::UseTool, CapabilityTarget::Web, spec(None));
        assert_eq!(
            unknown.estimated_cost(),
            None,
            "an unknown cost stays unknown, not zero"
        );
        assert_eq!(
            unknown.estimated_latency_ms(),
            None,
            "an unknown latency stays unknown, not zero"
        );
    }

    /// The namespace discriminator is keyed: stable under one workspace key,
    /// different under another, and different per namespace.
    #[test]
    fn namespace_discriminators_are_keyed() {
        let (first, second) = (key(), key());
        assert_eq!(Ns8::of(&first, "github"), Ns8::of(&first, "github"));
        assert_ne!(Ns8::of(&first, "github"), Ns8::of(&first, "gitlab"));
        assert_ne!(Ns8::of(&first, "github"), Ns8::of(&second, "github"));
        assert_eq!(Ns8::of(&first, "github").to_hex().len(), 8);
    }

    /// R-CDP16 L-3: a part with `:` or an empty part is refused, so the ids of
    /// `("a:b", "c")` and `("a", "b:c")` can never collide.
    #[test]
    fn capability_ids_refuse_separators_and_empty_parts() {
        let id = CapabilityId::new(CapabilityTarget::Mcp, "github", "create_issue").expect("test: id");
        assert_eq!(id.as_str(), "mcp:github:create_issue");
        assert_eq!(
            CapabilityId::new(CapabilityTarget::Mcp, "a:b", "c"),
            Err(CapabilityIdError::SeparatorInPart)
        );
        assert_eq!(
            CapabilityId::new(CapabilityTarget::Mcp, "a", "b:c"),
            Err(CapabilityIdError::SeparatorInPart)
        );
        assert_eq!(
            CapabilityId::new(CapabilityTarget::Mcp, "", "c"),
            Err(CapabilityIdError::EmptyPart)
        );
        assert_eq!(
            CapabilityId::new(CapabilityTarget::Mcp, "a", ""),
            Err(CapabilityIdError::EmptyPart)
        );
    }

    /// D-066 N-2: a remote node's capability id is `shell:node:<id>-<h8>`,
    /// without a `:` from the node id, and distinct raw ids never share an
    /// id even when they sanitize alike.
    #[test]
    fn remote_node_capability_ids_are_sanitized_and_keyed() {
        let key = key();
        let id = CapabilityId::remote_node(&key, "Box:1").expect("test: node id");
        let text = id.as_str();
        assert!(text.starts_with("shell:node:box-1-"), "{text}");
        assert_eq!(text.split(':').count(), 3, "{text}");
        assert_eq!(text.len(), "shell:node:box-1-".len() + 8);
        let other = CapabilityId::remote_node(&key, "box-1").expect("test: node id");
        assert!(other.as_str().starts_with("shell:node:box-1-"));
        assert_ne!(id, other, "the keyed suffix separates ids that sanitize alike");
        assert_eq!(
            CapabilityId::remote_node(&key, "Box:1").expect("test: node id"),
            id,
            "stable"
        );
        let long = CapabilityId::remote_node(&key, &"n".repeat(100)).expect("test: node id");
        assert!(long.as_str().len() <= "shell:node:".len() + NODE_NAME_CHARS + 9);
        assert_eq!(CapabilityId::remote_node(&key, ""), Err(CapabilityIdError::EmptyPart));
    }

    #[test]
    fn approval_codes() {
        assert_eq!(ApprovalRequirement::None.code(), "none");
        assert_eq!(
            ApprovalRequirement::Required {
                op_ids: vec![OpId::new("fs:write:abc")]
            }
            .code(),
            "required"
        );
        assert_eq!(OpId::new("fs:write:abc").as_str(), "fs:write:abc");
    }
}

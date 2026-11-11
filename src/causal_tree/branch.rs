use std::sync::Arc;

use crate::decision::{CandidateKey, DecisionKey, KeyDomain};

// ---------------------------------------------------------------------------
// Action and capability-target candidate model (CDP 5.1; CDP-PLAN 1.9)
//
// Runtime code builds gated `CausalCandidate`s. Historical v1 labels are
// decoded only by `v1_compat`; they cannot become executable branches.
// ---------------------------------------------------------------------------

/// What a candidate does (CDP 5.1). The `as_str` codes are the state v1
/// contract codes (CDP-PLAN 1.3) and the builtin's `features::Action` codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum CausalActionKind {
    Answer,
    Retrieve,
    Discover,
    UseTool,
    Delegate,
    Clarify,
    RequestApproval,
    Refuse,
}

impl CausalActionKind {
    /// Every action kind, in declaration order.
    pub(crate) const ALL: [Self; 8] = [
        Self::Answer,
        Self::Retrieve,
        Self::Discover,
        Self::UseTool,
        Self::Delegate,
        Self::Clarify,
        Self::RequestApproval,
        Self::Refuse,
    ];

    /// The stable snake_case code.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Answer => "answer",
            Self::Retrieve => "retrieve",
            Self::Discover => "discover",
            Self::UseTool => "use_tool",
            Self::Delegate => "delegate",
            Self::Clarify => "clarify",
            Self::RequestApproval => "request_approval",
            Self::Refuse => "refuse",
        }
    }
}

/// Which capability family a candidate acts on (CDP 5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum CapabilityTarget {
    None,
    Memory,
    Skill,
    Mcp,
    NativeTool,
    Wasm,
    Hook,
    Filesystem,
    Shell,
    Document,
    Web,
    Im,
    Session,
}

impl CapabilityTarget {
    /// Every target, in declaration order.
    pub(crate) const ALL: [Self; 13] = [
        Self::None,
        Self::Memory,
        Self::Skill,
        Self::Mcp,
        Self::NativeTool,
        Self::Wasm,
        Self::Hook,
        Self::Filesystem,
        Self::Shell,
        Self::Document,
        Self::Web,
        Self::Im,
        Self::Session,
    ];

    /// The stable snake_case code.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Memory => "memory",
            Self::Skill => "skill",
            Self::Mcp => "mcp",
            Self::NativeTool => "native_tool",
            Self::Wasm => "wasm",
            Self::Hook => "hook",
            Self::Filesystem => "filesystem",
            Self::Shell => "shell",
            Self::Document => "document",
            Self::Web => "web",
            Self::Im => "im",
            Self::Session => "session",
        }
    }
}

/// The side effect a candidate may have, ordered by risk floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum SideEffectClass {
    ReadOnly,
    LocalWrite,
    ProcessExec,
    ExternalSend,
    CredentialOrConfig,
    Destructive,
}

impl SideEffectClass {
    /// The stable snake_case code.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::LocalWrite => "local_write",
            Self::ProcessExec => "process_exec",
            Self::ExternalSend => "external_send",
            Self::CredentialOrConfig => "credential_or_config",
            Self::Destructive => "destructive",
        }
    }
}

/// A security-policy operation id (`security::op_id::op_id`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct OpId(Arc<str>);

impl OpId {
    /// Wrap an op id produced by `security::op_id::op_id`.
    pub(crate) fn new(op_id: &str) -> Self {
        Self(Arc::from(op_id))
    }

    /// The op id as text.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether executing a candidate needs an authenticated approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApprovalRequirement {
    None,
    Required { op_ids: Vec<OpId> },
}

impl ApprovalRequirement {
    /// The state v1 code: `none` or `required`. Live grants are verified per
    /// call through `GateInputs`, never carried on the candidate.
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Required { .. } => "required",
        }
    }
}

/// A canonical capability id: `{family}:{namespace}:{public_name}`. Neither
/// the namespace nor the public name may be empty or contain `:`, so two
/// different pairs never give the same id: an MCP server `a` advertising a
/// tool named `b:c` cannot pose as tool `c` of a server `a:b`, and an
/// execution allowed by id cannot reach another capability (R-CDP16 L-3).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CapabilityId(Arc<str>);

/// Why a capability id cannot be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum CapabilityIdError {
    /// The namespace or the public name is empty.
    #[error("a capability id part is empty")]
    EmptyPart,
    /// The namespace or the public name contains the separator `:`.
    #[error("a capability id part contains ':'")]
    SeparatorInPart,
}

impl CapabilityId {
    /// The id of `public_name` in `namespace` of `family`.
    ///
    /// # Errors
    /// [`CapabilityIdError`] when either part is empty or contains `:`.
    pub(crate) fn new(family: CapabilityTarget, namespace: &str, public_name: &str) -> Result<Self, CapabilityIdError> {
        for part in [namespace, public_name] {
            if part.is_empty() {
                return Err(CapabilityIdError::EmptyPart);
            }
            if part.contains(':') {
                return Err(CapabilityIdError::SeparatorInPart);
            }
        }
        Ok(Self(Arc::from(format!(
            "{}:{namespace}:{public_name}",
            family.as_str()
        ))))
    }

    /// The id of the shell capability of a remote node (D-066 N-2): family
    /// `shell`, namespace `node`, and the name `<sanitized-id>-<h8>`. The
    /// sanitized id is the node id in lowercase with every character outside
    /// `[a-z0-9-]` turned into `-`, at most [`NODE_NAME_CHARS`] characters,
    /// so it never carries `:`; `h8` is the first four bytes of the keyed
    /// `HMAC(DecisionKey, StateFingerprint, "node" ‖ raw id)` as hex, so two
    /// ids that sanitize alike (`Box:1` and `box-1`) still get different
    /// capability ids.
    ///
    /// # Errors
    /// [`CapabilityIdError::EmptyPart`] when the node id is empty.
    pub(crate) fn remote_node(key: &DecisionKey, node_id: &str) -> Result<Self, CapabilityIdError> {
        if node_id.is_empty() {
            return Err(CapabilityIdError::EmptyPart);
        }
        let sanitized: String = node_id
            .chars()
            .take(NODE_NAME_CHARS)
            .map(|character| {
                let lower = character.to_ascii_lowercase();
                if lower.is_ascii_lowercase() || lower.is_ascii_digit() || lower == '-' {
                    lower
                } else {
                    '-'
                }
            })
            .collect();
        let mac = key.hmac(KeyDomain::StateFingerprint, &[b"node", node_id.as_bytes()]);
        let h8 = hex::encode([mac[0], mac[1], mac[2], mac[3]]);
        Self::new(CapabilityTarget::Shell, "node", &format!("{sanitized}-{h8}"))
    }

    /// The id as text.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The longest sanitized node id in a remote-node capability name.
pub(crate) const NODE_NAME_CHARS: usize = 32;

/// SHA-256 of a capability's canonical schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SchemaFingerprint(pub(crate) [u8; 32]);

/// The identity of one catalog generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CatalogGeneration(pub(crate) [u8; 32]);

impl CatalogGeneration {
    /// The generation as lowercase hex.
    pub(crate) fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

/// A capability pinned to its schema and catalog generation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CapabilityRef {
    pub(crate) id: CapabilityId,
    pub(crate) fingerprint: SchemaFingerprint,
    pub(crate) generation: CatalogGeneration,
}

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

/// The estimated cost of executing a candidate once (CDP 5.1): what the
/// execution itself uses, beyond the decision call that chose it, as the
/// catalog's cost model predicts it (R-CDP16 L-6). A candidate whose cost
/// is unknown carries no estimate at all (`None` in
/// [`CausalCandidate::estimated_cost`]); the projection sends it as the
/// highest cost bucket and CDP-18's budget gate treats it as over budget, so
/// a missing estimate is never free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CostEstimate {
    /// Model tokens of the execution, prompt and completion together.
    pub(crate) tokens: u32,
    /// Money of the execution, in millionths of a US dollar.
    pub(crate) micro_usd: u64,
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

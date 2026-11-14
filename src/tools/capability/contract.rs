//! Capability contract types shared by the capability catalog, execution,
//! cost estimation and the decision engine: action kinds, capability targets,
//! side-effect classes, approval requirements and pinned capability references.

use std::sync::Arc;

use crate::security::runtime_key::{DecisionKey, KeyDomain};

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

/// Test-support enumeration of every action kind; production code matches
/// the enum exhaustively instead.
#[cfg(test)]
pub(crate) mod causal_action_test_support {
    use super::CausalActionKind;

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
    }
}

impl CausalActionKind {
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

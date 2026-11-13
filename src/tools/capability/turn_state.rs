//! The neutral turn scope shared by execution, recovery and the decision
//! engine (R15, R52): surface kind, side-effect mode, budgets, delivery
//! constraints and the trusted parts of a turn, plus the exact-call and
//! model-admission helpers. Nothing here imports the decision engine.

use std::fmt;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::tools::capability::contract::CatalogGeneration;

pub(crate) mod exact_calls;
pub(crate) mod model_admission;

/// The model identifier the builtin reports.
pub const MODEL_VERSION: &str = "builtin-v4";

/// An artifact younger than this (ms) is fresh evidence.
pub const FRESH_ARTIFACT_MS: u64 = 5 * 60 * 1000;
/// An artifact older than this (ms) may be stale.
pub const STALE_ARTIFACT_MS: u64 = 60 * 60 * 1000;

/// The side-effect mode governing what the CTE pipeline is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum SideEffectMode {
    /// No writes, no tool execution — pure analysis.
    ReadOnly,
    /// Writes require explicit user approval.
    ApprovalRequired,
    /// Writes allowed under policy guard (first-version default is ReadOnly).
    GuardedWrite,
}

impl Default for SideEffectMode {
    fn default() -> Self {
        Self::ReadOnly
    }
}

// ---------------------------------------------------------------------------
// Trusted, observed, predicted and derived turn state (CDP 5.2 and 5.3;
// CDP-PLAN 1.9). This source-owned state replaces the legacy history snapshot.
//
// Nothing here prints private text: every type that holds user, memory,
// catalog or identity text has a `Debug` that shows lengths and counts only
// (D-057 M-5; R-CDP16 M-4).
// ---------------------------------------------------------------------------

/// The surface a turn arrives on: CDP-PLAN 1.12 `SurfaceClass`, one to one
/// (CDP-21 builds it from the turn's origin).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum TurnSurfaceKind {
    Interactive,
    AgentLoop,
    Cron,
    Xin,
    Server,
    Channel,
    Worker,
}

impl TurnSurfaceKind {
    /// The stable code.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::AgentLoop => "agent_loop",
            Self::Cron => "cron",
            Self::Xin => "xin",
            Self::Server => "server",
            Self::Channel => "channel",
            Self::Worker => "worker",
        }
    }
}

/// A constraint code that may leave PRX in the redacted state: a fixed PRX
/// code, never a tool, capability or user name (so no hidden catalog entry
/// can leak through it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ConstraintCode(&'static str);

impl ConstraintCode {
    /// A constraint named by a static code.
    pub(crate) const fn new(code: &'static str) -> Self {
        Self(code)
    }

    /// The code.
    pub(crate) const fn as_str(self) -> &'static str {
        self.0
    }
}

/// Remaining budgets of the turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TurnBudgets {
    pub(crate) tokens: u64,
    pub(crate) latency_ms: u64,
    pub(crate) micro_usd: u64,
}

/// What the turn may deliver.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DeliveryConstraints {
    pub(crate) interactive: bool,
    pub(crate) can_ask_question: bool,
    pub(crate) approval_channel: bool,
}

/// Every trusted input of a turn: produced by authenticated runtime code,
/// never by a decision backend (CDP 5.2).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct TrustedParts {
    pub(crate) workspace_id: Arc<str>,
    pub(crate) owner_id: Option<Arc<str>>,
    pub(crate) session_key: Option<Arc<str>>,
    pub(crate) run_id: Option<Arc<str>>,
    pub(crate) task_id: Option<Arc<str>>,
    pub(crate) channel: Option<Arc<str>>,
    pub(crate) source_event_id: Option<Arc<str>>,
    pub(crate) causation_event_id: Option<Arc<str>>,
    pub(crate) surface: TurnSurfaceKind,
    pub(crate) side_effect_mode: SideEffectMode,
    /// Explicit excluded capability names (global, channel, child manifest).
    /// Exact-model policy has a separate compact commitment below.
    pub(crate) exclusions: Vec<Arc<str>>,
    pub(crate) model_admission: Option<Arc<dyn model_admission::ModelAdmissionView>>,
    /// Explicit structural user restrictions, as codes.
    pub(crate) constraints: Vec<ConstraintCode>,
    pub(crate) approval_pending: bool,
    pub(crate) approval_grants: Vec<Arc<str>>,
    pub(crate) budgets: TurnBudgets,
    pub(crate) delivery: DeliveryConstraints,
    pub(crate) catalog_generation: CatalogGeneration,
    pub(crate) policy_version: Arc<str>,
    /// The PMM-resolved model reference of the turn (`{provider}:{model}`).
    pub(crate) resolved_model: Arc<str>,
    /// PMM provider label. Carried with [`Self::resolved_model_id`] so pricing
    /// hints do not parse [`Self::resolved_model`].
    pub(crate) resolved_provider: Arc<str>,
    /// PMM model id of the turn.
    pub(crate) resolved_model_id: Arc<str>,
}

impl TrustedParts {
    pub(crate) fn owner_isolated(&self) -> bool {
        self.constraints.iter().any(|code| code.as_str() == "owner_isolated")
    }

    pub(crate) fn excludes_entry(&self, entry: model_admission::ModelAdmissionEntry<'_>) -> bool {
        self.exclusions
            .iter()
            .any(|name| name.as_ref() == entry.public_name || name.as_ref() == entry.root)
            || self
                .model_admission
                .as_ref()
                .is_some_and(|policy| policy.excludes(entry))
    }
}

impl fmt::Debug for TrustedParts {
    /// Identities, exclusions and grants print as presence or counts only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set = |value: &Option<Arc<str>>| value.is_some();
        f.debug_struct("TrustedParts")
            .field("owner_id_set", &set(&self.owner_id))
            .field("session_key_set", &set(&self.session_key))
            .field("run_id_set", &set(&self.run_id))
            .field("task_id_set", &set(&self.task_id))
            .field("channel_set", &set(&self.channel))
            .field("surface", &self.surface)
            .field("side_effect_mode", &self.side_effect_mode)
            .field("exclusions", &self.exclusions.len())
            .field("constraints", &self.constraints)
            .field("approval_pending", &self.approval_pending)
            .field("approval_grants", &self.approval_grants.len())
            .field("budgets", &self.budgets)
            .field("delivery", &self.delivery)
            .finish_non_exhaustive()
    }
}

/// The trusted part of a turn state. Its fields are private and it has no
/// setter; the only constructor is [`TrustedState::from_runtime`], whose
/// single production caller CDP-21's guard AG-28 pins to the turn decision
/// service. No backend output type converts into it (V3.14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustedState(TrustedParts, Option<Arc<RouterHistoryReadBinding>>);

/// Trusted runtime purpose, included in canonical state only for the routing
/// phase. It grants no access until the corresponding candidate is selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RouterHistoryReadBinding {
    pub(crate) domain_sha256: [u8; 32],
    pub(crate) query_sha256: [u8; 32],
    pub(crate) version: u8,
}

impl TrustedState {
    /// The trusted state of a turn, from runtime-authenticated parts.
    pub(crate) const fn from_runtime(parts: TrustedParts) -> Self {
        Self(parts, None)
    }

    pub(crate) fn router_history(&self) -> Option<&RouterHistoryReadBinding> {
        self.1.as_deref()
    }

    /// The trusted parts, read-only.
    pub(crate) const fn parts(&self) -> &TrustedParts {
        &self.0
    }
}

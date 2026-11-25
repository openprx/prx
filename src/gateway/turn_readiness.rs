//! Whether the Gateway's turn runtime can run a turn. A Gateway whose default
//! provider has no usable credential still binds and serves every management
//! and read route; only the routes that start or continue a turn refuse, each
//! once at admission, until a reload installs a ready generation.
use super::{AppState, TurnRuntimeGeneration, turn_source};
use crate::tools::{McpTool, Tool};
use std::sync::Arc;

/// Why the turn runtime cannot run a turn. Closed: each reason is one wire
/// `reason` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnNotReadyReason {
    /// The default provider requires a credential and none resolved.
    MissingProviderCredential,
}

impl TurnNotReadyReason {
    /// The wire value of this reason.
    pub const fn code(self) -> &'static str {
        match self {
            Self::MissingProviderCredential => "missing_provider_credential",
        }
    }

    /// The reason a failed default-provider construction stands for; `None`
    /// for every other failure, which keeps failing the start or the reload.
    pub(super) fn of_provider_error(error: &anyhow::Error) -> Option<Self> {
        error
            .chain()
            .any(|cause| cause.is::<crate::providers::MissingProviderCredential>())
            .then_some(Self::MissingProviderCredential)
    }
}

/// The refusal a turn entry point gets while the runtime is not ready.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("the gateway turn runtime is not ready ({})", .0.code())]
pub struct TurnNotReady(pub TurnNotReadyReason);

/// A generation without a chat connection: the published tool surface and the
/// configuration it was built from, so the catalog, the MCP routes and every
/// management route keep answering.
#[derive(Clone)]
pub struct TurnRuntimeStandby {
    pub(super) reason: TurnNotReadyReason,
    pub(super) publication_slot: Arc<turn_source::publication_slot::GatewayPublicationSlot>,
    pub(super) config_generation: Arc<crate::config::ConfigGeneration>,
    pub(super) tools_registry: Arc<Vec<Box<dyn Tool>>>,
    pub(super) mcp_tool: Option<Arc<McpTool>>,
}

/// The Gateway's current turn runtime: ready to run turns, or standing by.
#[derive(Clone)]
pub enum GatewayTurnState {
    Ready(Arc<TurnRuntimeGeneration>),
    NotReady(TurnRuntimeStandby),
}

impl From<TurnRuntimeGeneration> for GatewayTurnState {
    fn from(generation: TurnRuntimeGeneration) -> Self {
        Self::Ready(Arc::new(generation))
    }
}

impl GatewayTurnState {
    /// The ready generation, or the refusal a turn entry point answers with.
    pub fn ready(&self) -> Result<Arc<TurnRuntimeGeneration>, TurnNotReady> {
        match self {
            Self::Ready(generation) => Ok(Arc::clone(generation)),
            Self::NotReady(standby) => Err(TurnNotReady(standby.reason)),
        }
    }

    pub fn config_generation(&self) -> &Arc<crate::config::ConfigGeneration> {
        match self {
            Self::Ready(generation) => &generation.config_generation,
            Self::NotReady(standby) => &standby.config_generation,
        }
    }

    pub(super) fn publication_slot(&self) -> &Arc<turn_source::publication_slot::GatewayPublicationSlot> {
        match self {
            Self::Ready(generation) => &generation.publication_slot,
            Self::NotReady(standby) => &standby.publication_slot,
        }
    }

    /// The slot of a state nothing else holds yet, to hand it the owner's
    /// cleanup tracker; `None` once the state is shared.
    pub(super) fn publication_slot_mut(
        &mut self,
    ) -> Option<&mut Arc<turn_source::publication_slot::GatewayPublicationSlot>> {
        match self {
            Self::Ready(generation) => Arc::get_mut(generation).map(|generation| &mut generation.publication_slot),
            Self::NotReady(standby) => Some(&mut standby.publication_slot),
        }
    }

    pub fn tools_registry(&self) -> &Arc<Vec<Box<dyn Tool>>> {
        match self {
            Self::Ready(generation) => &generation.tools_registry,
            Self::NotReady(standby) => &standby.tools_registry,
        }
    }

    pub fn mcp_tool(&self) -> Option<&Arc<McpTool>> {
        match self {
            Self::Ready(generation) => generation.mcp_tool.as_ref(),
            Self::NotReady(standby) => standby.mcp_tool.as_ref(),
        }
    }

    /// This state's published decision service (published again when the installed skills change).
    pub(super) async fn publish(
        &self,
        state: &AppState,
    ) -> anyhow::Result<Arc<crate::runtime::turn_publication::TurnRuntimeService>> {
        self.publication_slot()
            .publish_parts(self.config_generation(), self.tools_registry(), self.mcp_tool(), state)
            .await
    }
}

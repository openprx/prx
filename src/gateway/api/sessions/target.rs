//! Targeted agent selection for `POST /api/sessions/{id}/message`.
//!
//! The selector is resolved against one pinned config generation before any
//! admission, input recording or provider access. Every refusal here is a
//! stable `{"error":{"code":...}}` and has no side effects.

use crate::config::agent_name::AgentName;
use crate::config::{Config, DelegateAgentConfig};
use crate::gateway::TurnRuntimeGeneration;
use crate::gateway::decision_source::GatewayProviderBinding;
use crate::providers::Provider;
use crate::runtime::turn_publication::TargetedSurface;
use crate::security::AutonomyLevel;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Tools no target can reach until an audited inventory extension: they act
/// on the host's scheduler and background-task stores. The exclusion applies
/// to the published surface whatever the allowlist says, wildcard included.
const TARGET_EXCLUDED_TOOLS: [&str; 2] = ["cron", "xin"];

/// The `agent` field exactly as the request carried it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AgentField {
    /// The field was absent: Main.
    Omitted,
    /// An explicit JSON `null`: refused, as the only explicit Main encoding is `"main"`.
    Null,
    Text(String),
    /// Any other JSON type.
    NotText,
}

impl AgentField {
    /// Classify the parsed JSON value of a present `agent` key.
    pub(super) fn from_json(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::String(text) => Self::Text(text),
            _ => Self::NotText,
        }
    }
}

/// Who answers the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TurnTarget {
    Main,
    Named(AgentName),
}

impl TurnTarget {
    /// Apply the name grammar to the request field. No folding, no fallback.
    pub(super) fn from_field(field: AgentField) -> Result<Self, TargetError> {
        match field {
            AgentField::Omitted => Ok(Self::Main),
            AgentField::Null | AgentField::NotText => Err(TargetError::Invalid),
            AgentField::Text(text) => {
                let name = AgentName::parse(&text).map_err(|_| TargetError::Invalid)?;
                if name.is_main() {
                    Ok(Self::Main)
                } else {
                    Ok(Self::Named(name))
                }
            }
        }
    }
}

/// Why a target was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetError {
    /// 400: the field is null, not a string, or outside the name grammar.
    Invalid,
    /// 404: no such configured agent.
    Unknown,
    /// 409: the rollout switch is off, or the agent is disabled.
    Disabled,
    /// 400: the agentic allowlist is empty or mixes `*` with names.
    ToolsInvalid,
    /// 409: the memory backend keeps no agent incarnation registry.
    BackendUnsupported,
}

impl TargetError {
    const fn status(self) -> StatusCode {
        match self {
            Self::Invalid | Self::ToolsInvalid => StatusCode::BAD_REQUEST,
            Self::Unknown => StatusCode::NOT_FOUND,
            Self::Disabled | Self::BackendUnsupported => StatusCode::CONFLICT,
        }
    }

    pub(super) const fn code(self) -> &'static str {
        match self {
            Self::Invalid => "agent_invalid",
            Self::Unknown => "agent_unknown",
            Self::Disabled => "agent_disabled",
            Self::ToolsInvalid => "agent_tools_invalid",
            Self::BackendUnsupported => "agent_backend_unsupported",
        }
    }
}

impl IntoResponse for TargetError {
    fn into_response(self) -> Response {
        (
            self.status(),
            Json(serde_json::json!({ "error": { "code": self.code() } })),
        )
            .into_response()
    }
}

/// The tools a target may use, before host policy and the catalog narrow them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ToolAllowlist {
    /// A non-agentic target: no general execution tools.
    None,
    /// `["*"]`: inherit only the admitted host surface.
    Inherit,
    /// Explicit names, intersected with the host surface.
    Only(BTreeSet<String>),
}

impl ToolAllowlist {
    /// The published surface, with the target-wide exclusions applied.
    pub(super) fn surface(&self) -> TargetedSurface {
        let excluded_names: BTreeSet<String> = TARGET_EXCLUDED_TOOLS.iter().map(|name| (*name).to_owned()).collect();
        let allowed_tools = match self {
            Self::None => Some(BTreeSet::new()),
            Self::Inherit => None,
            Self::Only(names) => Some(names.difference(&excluded_names).cloned().collect()),
        };
        TargetedSurface {
            allowed_tools,
            excluded_names,
        }
    }
}

/// `min(agent, main)` with `ReadOnly < Supervised < Full`; an unset agent
/// level inherits Main's.
pub(super) fn effective_autonomy(agent: Option<AutonomyLevel>, main: AutonomyLevel) -> AutonomyLevel {
    agent.map_or(main, |level| level.min(main))
}

fn allowlist_for(agent: &DelegateAgentConfig) -> Result<ToolAllowlist, TargetError> {
    if !agent.agentic {
        return Ok(ToolAllowlist::None);
    }
    let names: BTreeSet<String> = agent
        .allowed_tools
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    if names.is_empty() || (names.contains("*") && names.len() != 1) {
        return Err(TargetError::ToolsInvalid);
    }
    if names.contains("*") {
        Ok(ToolAllowlist::Inherit)
    } else {
        Ok(ToolAllowlist::Only(names))
    }
}

/// A named target that passed resolution against one pinned config.
#[derive(Clone)]
pub(super) struct TargetPlan {
    pub(super) name: AgentName,
    pub(super) agent: DelegateAgentConfig,
    pub(super) autonomy: AutonomyLevel,
    pub(super) tools: ToolAllowlist,
}

impl std::fmt::Debug for TargetPlan {
    /// The agent config can hold an inline credential; it is never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TargetPlan")
            .field("name", &self.name)
            .field("autonomy", &self.autonomy)
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}

/// Resolve `target` against `config`. `Ok(None)` is Main.
///
/// Order: rollout switch, existence, enabled flag, allowlist. The switch
/// comes first so that, while it is off, an otherwise-valid named target is
/// always 409 and reveals nothing about which agents exist.
pub(super) fn resolve_target(config: &Config, target: &TurnTarget) -> Result<Option<TargetPlan>, TargetError> {
    let TurnTarget::Named(name) = target else {
        return Ok(None);
    };
    if !config.gateway.targeted_agents {
        return Err(TargetError::Disabled);
    }
    let agent = config.agents.get(name.as_str()).ok_or(TargetError::Unknown)?;
    if !agent.enabled {
        return Err(TargetError::Disabled);
    }
    let tools = allowlist_for(agent)?;
    Ok(Some(TargetPlan {
        name: name.clone(),
        autonomy: effective_autonomy(agent.autonomy, config.autonomy.level),
        agent: agent.clone(),
        tools,
    }))
}

/// What the turn needs beyond the host runtime.
pub(super) struct TargetedTurn {
    pub(super) name: AgentName,
    pub(super) system_prompt: String,
    pub(super) autonomy: AutonomyLevel,
}

/// The runtime a message turn runs on, and the target it serves when it is
/// not Main.
pub(super) struct SelectedTurn {
    pub(super) runtime: Arc<TurnRuntimeGeneration>,
    pub(super) targeted: Option<TargetedTurn>,
    /// The named agent's incarnation for this owner; `None` for Main.
    pub(super) incarnation: Option<String>,
}

/// A target that resolved but cannot be served right now.
#[derive(Debug)]
pub(super) struct TargetUnavailable(pub(super) String);

impl IntoResponse for TargetUnavailable {
    fn into_response(self) -> Response {
        tracing::error!(error = %self.0, "targeted agent runtime could not be built");
        super::json_error(StatusCode::INTERNAL_SERVER_ERROR, "Targeted agent is unavailable")
    }
}

fn target_provider(
    config: &Config,
    plan: &TargetPlan,
) -> anyhow::Result<(Arc<dyn Provider>, crate::providers::catalog::ResolvedModel)> {
    use crate::providers::credentials::{CredentialRef, LegacySlot};
    let agent = &plan.agent;
    let credential = match agent.credential_ref.as_deref() {
        Some(id) => CredentialRef::Profile { id: id.to_owned() },
        None if agent.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) => {
            CredentialRef::LegacyInline(LegacySlot::Agent(plan.name.as_str().to_owned()))
        }
        None => crate::providers::selection::credential_for(config, &agent.provider),
    };
    let catalog = crate::providers::selection::catalog_for(config);
    let resolved = crate::providers::selection::check_with(
        config,
        &catalog,
        &agent.provider,
        &agent.model,
        credential,
        &crate::providers::catalog::Requirement::chat(),
        crate::providers::catalog::UnknownPolicy::Invocation,
    )?;
    let options = crate::providers::provider_runtime_options_from_config(config);
    let provider: Arc<dyn Provider> = Arc::from(crate::providers::create_routed_provider_for(
        &resolved, &catalog, config, &options,
    )?);
    Ok((provider, resolved))
}

fn target_system_prompt(config: &Config, agent: &DelegateAgentConfig) -> String {
    let identity_dir = agent
        .identity_dir
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|directory| config.workspace_dir.join(directory));
    crate::identity::delegated::compose(identity_dir.as_deref(), agent.system_prompt.as_deref())
}

/// Derive the pinned runtime for a resolved target. It shares the generation,
/// registry and cleanup owner of `parent` and replaces the provider binding,
/// model, temperature and decision slot (capped security policy and the
/// target's own tool surface).
pub(super) fn prepare_target(
    parent: &TurnRuntimeGeneration,
    plan: &TargetPlan,
) -> Result<(Arc<TurnRuntimeGeneration>, TargetedTurn), TargetUnavailable> {
    let config = &parent.config_generation.effective;
    let (provider, resolved) = target_provider(config, plan)
        .map_err(|error| TargetUnavailable(crate::security::redaction::redact_secrets(&format!("{error:#}"))))?;
    let model = resolved.model().to_owned();
    let binding = Arc::new(GatewayProviderBinding::new(Arc::clone(&provider), resolved));
    let mut capped = (**config).clone();
    capped.autonomy.level = plan.autonomy;
    let security = crate::runtime::bootstrap::build_security_policy(&capped);
    let runtime = TurnRuntimeGeneration {
        provider_binding: Some(binding),
        decision_slot: Arc::new(parent.decision_slot.targeted(security, plan.tools.surface())),
        config_generation: Arc::clone(&parent.config_generation),
        provider,
        model,
        temperature: plan.agent.temperature.unwrap_or(0.7),
        tools_registry: Arc::clone(&parent.tools_registry),
        mcp_tool: parent.mcp_tool.clone(),
    };
    let turn = TargetedTurn {
        name: plan.name.clone(),
        system_prompt: target_system_prompt(config, &plan.agent),
        autonomy: plan.autonomy,
    };
    Ok((Arc::new(runtime), turn))
}

/// Resolve the target's incarnation for (workspace, owner, agent), minting it
/// at the first authorized resolve. This is the last step of selection: the
/// admission re-reads the active generation inside the registry transaction.
pub(super) fn resolve_incarnation(
    state: &crate::gateway::AppState,
    registry: &dyn crate::memory::agent_registry::AgentRegistry,
    runtime: &TurnRuntimeGeneration,
    owner_id: &str,
    plan: &TargetPlan,
) -> Result<String, super::TargetRefusal> {
    use crate::memory::agent_registry::AgentRefusal;
    let key = crate::gateway::agent_lifecycle::agent_key(plan.name.as_str());
    let revision = crate::gateway::agent_lifecycle::config_revision(&plan.agent)
        .map_err(|error| super::TargetRefusal::Unavailable(TargetUnavailable(error.to_string())))?;
    let lifecycle = state.turn_runtime.as_ref().map(|owner| owner.agent_lifecycle());
    let admit = || {
        if lifecycle.is_some_and(|lifecycle| lifecycle.blocks(&key)) {
            return Err(AgentRefusal::Disabled);
        }
        let active = state.config.pin();
        match active.effective.agents.get(plan.name.as_str()) {
            None => Err(AgentRefusal::Unknown),
            Some(agent) if !agent.enabled || !active.effective.gateway.targeted_agents => Err(AgentRefusal::Disabled),
            Some(_) => Ok(()),
        }
    };
    let workspace = runtime.config_generation.effective.workspace_dir.to_string_lossy();
    match registry.resolve_incarnation(&workspace, owner_id, &key, &revision, &admit) {
        Ok(Ok(incarnation)) => Ok(incarnation),
        Ok(Err(AgentRefusal::Unknown)) => Err(super::TargetRefusal::Selector(TargetError::Unknown)),
        Ok(Err(AgentRefusal::Disabled)) => Err(super::TargetRefusal::Selector(TargetError::Disabled)),
        Err(error) => Err(super::TargetRefusal::Unavailable(TargetUnavailable(format!(
            "agent registry: {error:#}"
        )))),
    }
}

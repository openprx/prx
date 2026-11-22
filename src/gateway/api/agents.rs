//! The rf contract's agent management routes (AC-1a1): named-agent CRUD,
//! Main's readback and the targeted-agents switch. Field names are a
//! contract with rf (`docs/rf-contract/capability-tokens.md`, token
//! `agent_crud_v1` and `agent_main_readback_v1`).
//!
//! - Every route is authorised by the owner bearer alone ([`super::capabilities::guard`]);
//!   no autonomy level is consulted, Main's included.
//! - Every error is the coded `{"error":{"code","message"}}` shape; a 409
//!   conflict adds `current_revision`, a 428 adds `revision`.
//! - Writes go through the one management commit
//!   ([`super::manage::commit_as_owner`]): the whole-config revision named in
//!   `If-Match`, history, activation and the high-risk confirmation.
//! - No body carries a credential: the agent's `api_key` and
//!   `credential_ref` are kept across a `PUT` and never returned, and provider
//!   text is redacted as on every model listing. Log lines carry no payload.
//! - The incarnation comes from the agent registry's own mint
//!   (`AgentRegistry::resolve_incarnation`, as a targeted turn mints it), at
//!   the first `PUT` or the first read, whatever the switch or `enabled` say.

use super::capabilities::{catalog_tool_names, coded_error_with, confirms_high_risk, method_not_allowed_allowing};
use super::manage::{self, Write, WriteQuery, looks_secret};
use super::{ApiPrincipal, AppState};
use crate::config::agent_name::AgentName;
use crate::config::manage::credentials::CredentialReferenceError;
use crate::config::manage::service::redact_provider_text;
use crate::config::manage::{
    AgentSpec, ConfigTransactionError, ConfigTransactions, ManageError, ManagedChange, NotAppliedOutcome,
};
use crate::config::{Config, DelegateAgentConfig};
use crate::gateway::agent_lifecycle::{agent_key, config_revision};
use crate::memory::agent_registry::{AgentRefusal, AgentRegistry, MAIN_AGENT_KEY, MAIN_REVISION_PREIMAGE};
use crate::security::AutonomyLevel;
use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::Arc;

/// The longest `prompt`, in bytes (the config schema sets none).
const PROMPT_MAX_BYTES: usize = 32_768;

const BODY_SHAPE: &str = "the body must be a JSON object with exactly provider, model, autonomy, tools, enabled and an \
                          optional prompt and limits; nothing was written";

/// A refused request: its status, closed code and message, and the one
/// extra field its code carries.
struct Refusal {
    status: StatusCode,
    code: &'static str,
    message: Cow<'static, str>,
    extra: Option<(&'static str, String)>,
}

impl Refusal {
    fn new(status: StatusCode, code: &'static str, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            extra: None,
        }
    }

    fn invalid_name() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "agent_invalid",
            "the agent name must match ^[a-z][a-z0-9_-]{0,63}$ and not be main or catalog here; nothing was written",
        )
    }

    fn field(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "field_invalid", message)
    }

    fn tools(message: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "agent_tools_invalid", message)
    }

    fn unknown() -> Self {
        Self::new(StatusCode::NOT_FOUND, "agent_unknown", "no such agent")
    }

    fn retired() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "agent_disabled",
            "this agent is retired until its purge; use a new name; nothing was written",
        )
    }

    fn storage(reason: &'static str) -> Self {
        tracing::warn!(reason, "agent route failed");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "storage_unavailable", reason)
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let extra = self.extra.as_ref().map(|(key, value)| (*key, value.as_str()));
        coded_error_with(self.status, self.code, &self.message, extra)
    }
}

type Outcome<T> = Result<T, Refusal>;

fn respond<T: Serialize>(outcome: Outcome<(StatusCode, T)>) -> Response {
    match outcome {
        Ok((status, body)) => (status, Json(body)).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

/// The map of every management refusal onto the closed codes. Messages are
/// the service's sentences with provider text redacted.
fn refusal_of(error: ManageError) -> Refusal {
    let message = redact_provider_text(&error.to_string());
    match error {
        ManageError::NotFound(_) => Refusal::unknown(),
        ManageError::Credential(CredentialReferenceError::Store(_)) => {
            Refusal::storage("the credential store could not be read")
        }
        ManageError::Invalid(_)
        | ManageError::AlreadyExists(_)
        | ManageError::Referenced { .. }
        | ManageError::Credential(_)
        | ManageError::Transaction(ConfigTransactionError::Invalid(_)) => Refusal::field(message),
        ManageError::Transaction(ConfigTransactionError::Conflict(conflict)) => Refusal {
            extra: Some(("current_revision", conflict.actual)),
            ..Refusal::new(
                StatusCode::CONFLICT,
                "revision_conflict",
                "the configuration changed since it was read; read it back and retry with current_revision; \
                 nothing was written",
            )
        },
        ManageError::Transaction(ConfigTransactionError::ConfirmationRequired { revision, .. }) => Refusal {
            extra: revision.map(|revision| ("revision", revision)),
            ..Refusal::new(StatusCode::PRECONDITION_REQUIRED, "confirmation_required", message)
        },
        ManageError::Transaction(ConfigTransactionError::NotApplied {
            outcome: NotAppliedOutcome::Restored | NotAppliedOutcome::NothingWritten,
            ..
        }) => {
            tracing::warn!("agent route: the configuration change was not applied");
            Refusal::new(StatusCode::UNPROCESSABLE_ENTITY, "not_applied", message)
        }
        ManageError::Transaction(ConfigTransactionError::NotApplied { .. } | ConfigTransactionError::Failed(_)) => {
            Refusal::storage("the configuration change could not be saved or activated")
        }
    }
}

/// [`refusal_of`], plus Main's level gate for a caller not authenticated by
/// a paired bearer: `403 read_only` at read-only autonomy, otherwise
/// `403 policy_denied` (the codes the skills lifecycle routes use).
fn owner_refusal_of(error: manage::OwnerCommitError) -> Refusal {
    match error {
        manage::OwnerCommitError::Manage(error) => refusal_of(error),
        manage::OwnerCommitError::MainLevel(message) => {
            let code = if message.contains("read-only") {
                "read_only"
            } else {
                "policy_denied"
            };
            Refusal::new(StatusCode::FORBIDDEN, code, redact_provider_text(&message))
        }
    }
}

/// The desired configuration and its whole-config revision, read together.
struct Snapshot {
    config: Arc<Config>,
    revision: String,
}

fn snapshot(state: &AppState) -> Outcome<Snapshot> {
    let desired = state.config.desired();
    let revision = ConfigTransactions::base_revision(&desired)
        .ok_or_else(|| Refusal::storage("the configuration was not loaded from disk, so it has no revision"))?;
    Ok(Snapshot {
        config: Arc::clone(&desired.config),
        revision,
    })
}

/// The agent registry of this workspace, for the requesting owner.
struct Registry<'a> {
    store: &'a dyn AgentRegistry,
    workspace: String,
    owner: &'a str,
}

fn registry<'a>(state: &'a AppState, principal: &'a ApiPrincipal) -> Outcome<Registry<'a>> {
    let store = state.mem.agent_registry().ok_or_else(|| {
        Refusal::new(
            StatusCode::CONFLICT,
            "agent_backend_unsupported",
            "the memory backend keeps no agent incarnation registry",
        )
    })?;
    Ok(Registry {
        store,
        workspace: state.config.load_full().workspace_dir.to_string_lossy().into_owned(),
        owner: &principal.owner_id,
    })
}

impl Registry<'_> {
    /// The live row of `key`: `(incarnation, retired)`.
    fn live(&self, key: &str) -> Outcome<Option<(String, bool)>> {
        self.store
            .live_incarnation(&self.workspace, self.owner, key)
            .map(|live| live.map(|live| (live.incarnation, live.retiring)))
            .map_err(|_| Refusal::storage("the agent registry could not be read"))
    }

    fn mint(
        &self,
        key: &str,
        revision: &str,
        admit: &dyn Fn() -> Result<(), AgentRefusal>,
    ) -> Outcome<Result<String, AgentRefusal>> {
        self.store
            .resolve_incarnation(&self.workspace, self.owner, key, revision, admit)
            .map_err(|_| Refusal::storage("the agent registry could not be written"))
    }
}

/// The incarnation of a configured named agent and whether it is retired,
/// minted at its first read. `None` when the agent left the configuration
/// meanwhile.
fn named_incarnation(
    state: &AppState,
    registry: &Registry<'_>,
    name: &AgentName,
    agent: &DelegateAgentConfig,
) -> Outcome<Option<(String, bool)>> {
    let key = agent_key(name.as_str());
    if let Some(live) = registry.live(&key)? {
        return Ok(Some(live));
    }
    let revision =
        config_revision(agent).map_err(|_| Refusal::storage("the agent's config revision could not be computed"))?;
    let lifecycle = state.turn_runtime.as_ref().map(|owner| owner.agent_lifecycle());
    let admit = || {
        if lifecycle.is_some_and(|lifecycle| lifecycle.blocks(&key)) {
            return Err(AgentRefusal::Disabled);
        }
        if state.config.desired().config.agents.contains_key(name.as_str()) {
            Ok(())
        } else {
            Err(AgentRefusal::Unknown)
        }
    };
    match registry.mint(&key, &revision, &admit)? {
        Ok(incarnation) => Ok(Some((incarnation, false))),
        Err(AgentRefusal::Unknown) => Ok(None),
        Err(AgentRefusal::Disabled) => match registry.live(&key)? {
            Some((incarnation, true)) => Ok(Some((incarnation, true))),
            _ => Err(Refusal::storage("the agent registry refused the incarnation")),
        },
    }
}

#[derive(Serialize)]
struct AgentConfigBody {
    approval_wait_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    limits: Option<crate::config::AgentLimits>,
    provider: String,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    autonomy: AutonomyLevel,
    tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
}

/// One agent's readback. Main's has no `config.enabled`, no `prompt` and no
/// `status`.
#[derive(Serialize)]
struct Readback {
    name: String,
    config: AgentConfigBody,
    revision: String,
    incarnation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
}

fn named_readback(
    name: &AgentName,
    agent: &DelegateAgentConfig,
    snapshot: &Snapshot,
    (incarnation, retired): (String, bool),
) -> Readback {
    let tools = if agent.agentic {
        agent.allowed_tools.clone()
    } else {
        Vec::new()
    };
    let status = if retired {
        "retired"
    } else if agent.enabled {
        "active"
    } else {
        "disabled"
    };
    Readback {
        name: name.as_str().to_owned(),
        config: AgentConfigBody {
            approval_wait_secs: agent.approval_wait_secs,
            limits: Some(agent.limits.clone()),
            provider: redact_provider_text(&agent.provider),
            model: agent.model.clone(),
            prompt: agent.system_prompt.clone(),
            // A hand-written agent without a level runs at Main's.
            autonomy: crate::security::agent_level::agent_level(agent.autonomy, snapshot.config.autonomy.level),
            tools,
            enabled: Some(agent.enabled),
        },
        revision: snapshot.revision.clone(),
        incarnation,
        status: Some(status),
    }
}

/// The readback of a configured named agent; `None` when it is not
/// configured.
fn read_named(
    state: &AppState,
    registry: &Registry<'_>,
    snapshot: &Snapshot,
    name: &AgentName,
) -> Outcome<Option<Readback>> {
    let Some(agent) = snapshot.config.agents.get(name.as_str()) else {
        return Ok(None);
    };
    Ok(named_incarnation(state, registry, name, agent)?.map(|live| named_readback(name, agent, snapshot, live)))
}

fn read_main(registry: &Registry<'_>, snapshot: &Snapshot) -> Outcome<Readback> {
    let revision = crate::memory::SqliteMemory::schema_migration_checksum(MAIN_REVISION_PREIMAGE);
    let incarnation = registry
        .mint(MAIN_AGENT_KEY, &revision, &|| Ok(()))?
        .map_err(|_| Refusal::storage("the agent registry refused Main's incarnation"))?;
    let config = &snapshot.config;
    Ok(Readback {
        name: crate::config::agent_name::MAIN_AGENT_NAME.to_owned(),
        config: AgentConfigBody {
            approval_wait_secs: config.agent.approval_wait_secs,
            limits: None,
            provider: redact_provider_text(crate::providers::selection::default_provider(config)),
            model: crate::providers::selection::default_model(config).to_owned(),
            prompt: None,
            autonomy: config.autonomy.level,
            tools: vec!["*".to_owned()],
            enabled: None,
        },
        revision: snapshot.revision.clone(),
        incarnation,
        status: None,
    })
}

/// Who a path segment names.
enum Target {
    Main,
    Named(AgentName),
}

fn target(raw: &str) -> Outcome<Target> {
    if looks_secret(raw) {
        return Err(Refusal::invalid_name());
    }
    let name = AgentName::parse(raw).map_err(|_| Refusal::invalid_name())?;
    if name.is_main() {
        Ok(Target::Main)
    } else if name.is_reserved() {
        Err(Refusal::invalid_name())
    } else {
        Ok(Target::Named(name))
    }
}

/// A name a write may take: never Main or a reserved name.
fn writable(raw: &str) -> Outcome<AgentName> {
    match target(raw)? {
        Target::Named(name) => Ok(name),
        Target::Main => Err(Refusal::invalid_name()),
    }
}

/// The write options: `If-Match` naming a config revision is required
/// (absent or `*` is refused), `?confirm_high_risk=true` only confirms.
fn write_of(uri: &Uri, headers: &HeaderMap) -> Outcome<Write> {
    let raw = headers
        .get(header::IF_MATCH)
        .ok_or_else(|| Refusal::field("If-Match is required: the config revision from GET /api/agents"))?;
    if raw.to_str().is_ok_and(|value| value.trim() == "*") {
        return Err(Refusal::field("If-Match must name the config revision; `*` is refused"));
    }
    Write::of(&WriteQuery::confirming(confirms_high_risk(uri)), headers)
        .map_err(|_| Refusal::field("If-Match must be a config revision (lowercase hex)"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutBody {
    #[serde(default = "crate::config::schema::default_approval_wait_secs")]
    approval_wait_secs: u64,
    #[serde(default)]
    limits: Option<crate::config::AgentLimits>,
    provider: String,
    model: String,
    autonomy: String,
    tools: Vec<String>,
    enabled: bool,
    #[serde(default)]
    prompt: Option<String>,
}

fn parse_spec(body: &[u8]) -> Outcome<AgentSpec> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| Refusal::field(BODY_SHAPE))?;
    let Some(object) = value.as_object() else {
        return Err(Refusal::field(BODY_SHAPE));
    };
    if object.get("prompt").is_some_and(serde_json::Value::is_null) {
        return Err(Refusal::field(
            "prompt must be a string, or absent for PRX's own composition; nothing was written",
        ));
    }
    let body: PutBody = serde_json::from_value(value).map_err(|_| Refusal::field(BODY_SHAPE))?;
    let autonomy = match body.autonomy.as_str() {
        "readonly" => AutonomyLevel::ReadOnly,
        "supervised" => AutonomyLevel::Supervised,
        "full" => AutonomyLevel::Full,
        _ => {
            return Err(Refusal::field(
                "autonomy must be readonly, supervised or full; nothing was written",
            ));
        }
    };
    if body
        .prompt
        .as_ref()
        .is_some_and(|prompt| prompt.len() > PROMPT_MAX_BYTES)
    {
        return Err(Refusal::field("prompt is longer than 32768 bytes; nothing was written"));
    }
    if !(1..=86400).contains(&body.approval_wait_secs) {
        return Err(Refusal::field("approval_wait_secs must be within 1..=86400"));
    }
    let limits = body.limits.unwrap_or_default();
    limits.validate().map_err(Refusal::field)?;
    Ok(AgentSpec {
        approval_wait_secs: body.approval_wait_secs,
        limits,
        provider: body.provider,
        model: body.model,
        prompt: body.prompt,
        autonomy,
        tools: body.tools,
        enabled: body.enabled,
    })
}

/// `["*"]`, `[]`, or distinct names from the catalog's `tools`.
async fn check_tools(state: &AppState, tools: &[String]) -> Outcome<()> {
    let mut seen = BTreeSet::new();
    if !tools.iter().all(|tool| seen.insert(tool.as_str())) {
        return Err(Refusal::tools("a tool is named twice; nothing was written"));
    }
    if seen.contains("*") {
        return if tools.len() == 1 {
            Ok(())
        } else {
            Err(Refusal::tools(
                "`*` cannot be mixed with tool names; nothing was written",
            ))
        };
    }
    if tools.is_empty() {
        return Ok(());
    }
    let catalog = catalog_tool_names(state)
        .await
        .ok_or_else(|| Refusal::storage("the tool catalog could not be published"))?;
    if tools.iter().all(|tool| catalog.binary_search(tool).is_ok()) {
        Ok(())
    } else {
        Err(Refusal::tools(
            "every tool must be one of the tools of GET /api/agents/catalog; nothing was written",
        ))
    }
}

#[derive(Serialize)]
struct ListBody {
    revision: String,
    agents: Vec<Readback>,
}

/// `GET /api/agents`: every configured named agent, retired ones included,
/// sorted by name; Main and untargetable names are not listed.
pub(super) async fn get_agents(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
) -> Response {
    respond(list(&state, &principal).map(|body| (StatusCode::OK, body)))
}

fn list(state: &AppState, principal: &ApiPrincipal) -> Outcome<ListBody> {
    let snapshot = snapshot(state)?;
    let registry = registry(state, principal)?;
    let mut names = snapshot
        .config
        .agents
        .keys()
        .filter_map(|name| AgentName::parse(name).ok())
        .filter(|name| !name.is_reserved())
        .collect::<Vec<_>>();
    names.sort();
    let mut agents = Vec::with_capacity(names.len());
    for name in &names {
        if let Some(readback) = read_named(state, &registry, &snapshot, name)? {
            agents.push(readback);
        }
    }
    Ok(ListBody {
        revision: snapshot.revision,
        agents,
    })
}

/// `GET /api/agents/{name}`: a named agent's readback, or Main's for `main`.
pub(super) async fn get_agent(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Path(raw): Path<String>,
) -> Response {
    respond(read_one(&state, &principal, &raw).map(|body| (StatusCode::OK, body)))
}

fn read_one(state: &AppState, principal: &ApiPrincipal, raw: &str) -> Outcome<Readback> {
    let target = target(raw)?;
    let snapshot = snapshot(state)?;
    let registry = registry(state, principal)?;
    match target {
        Target::Main => read_main(&registry, &snapshot),
        Target::Named(name) => read_named(state, &registry, &snapshot, &name)?.ok_or_else(Refusal::unknown),
    }
}

/// `PUT /api/agents/{name}`: create (201) or fully replace (200) a named
/// agent; the readback is the answer.
pub(super) async fn put_agent(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Path(raw): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    respond(put(&state, &principal, &raw, &uri, &headers, &body).await)
}

async fn put(
    state: &AppState,
    principal: &ApiPrincipal,
    raw: &str,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Outcome<(StatusCode, Readback)> {
    let name = writable(raw)?;
    let write = write_of(uri, headers)?;
    let spec = parse_spec(body)?;
    check_tools(state, &spec.tools).await?;
    let registry = registry(state, principal)?;
    if registry
        .live(&agent_key(name.as_str()))?
        .is_some_and(|(_, retired)| retired)
    {
        return Err(Refusal::retired());
    }
    let created = !state.config.desired().config.agents.contains_key(name.as_str());
    let change = ManagedChange::PutAgent {
        agent: name.as_str().to_owned(),
        spec,
    };
    manage::commit_as_owner(state, &change, &write, super::owner_authenticated(state, headers))
        .await
        .map_err(owner_refusal_of)?;
    let snapshot = snapshot(state)?;
    let readback = read_named(state, &registry, &snapshot, &name)?
        .ok_or_else(|| Refusal::storage("the agent left the configuration right after it was written"))?;
    let status = if created { StatusCode::CREATED } else { StatusCode::OK };
    Ok((status, readback))
}

#[derive(Serialize)]
struct RetiredBody {
    name: String,
    status: &'static str,
    revision: String,
}

/// `DELETE /api/agents/{name}`: retire a named agent (its config stays,
/// disabled, until its purge). Repeating it answers the same body.
pub(super) async fn delete_agent(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Path(raw): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    respond(delete(&state, &principal, &raw, &uri, &headers).await)
}

async fn delete(
    state: &AppState,
    principal: &ApiPrincipal,
    raw: &str,
    uri: &Uri,
    headers: &HeaderMap,
) -> Outcome<(StatusCode, RetiredBody)> {
    let name = writable(raw)?;
    let write = write_of(uri, headers)?;
    let registry = registry(state, principal)?;
    let key = agent_key(name.as_str());
    let answer = |revision| RetiredBody {
        name: name.as_str().to_owned(),
        status: "retired",
        revision,
    };
    let pause_jobs = || {
        crate::cron::agent_jobs::retire(&state.config.load_full(), &principal.owner_id, name.as_str())
            .map_err(|_| Refusal::storage("the agent jobs could not be paused"))
    };
    if registry.live(&key)?.is_some_and(|(_, retired)| retired) {
        pause_jobs()?;
        return Ok((StatusCode::OK, answer(snapshot(state)?.revision)));
    }
    if !state.config.desired().config.agents.contains_key(name.as_str()) {
        return Err(Refusal::unknown());
    }
    let change = ManagedChange::RetireAgent {
        agent: name.as_str().to_owned(),
    };
    manage::commit_as_owner(state, &change, &write, super::owner_authenticated(state, headers))
        .await
        .map_err(owner_refusal_of)?;
    let snapshot = snapshot(state)?;
    // This owner gets a row to retire even if it never read the agent.
    if let Some(agent) = snapshot.config.agents.get(name.as_str()) {
        named_incarnation(state, &registry, &name, agent)?;
    }
    registry
        .store
        .retire_removed(&registry.workspace, &BTreeSet::from([key]))
        .map_err(|_| Refusal::storage("the agent registry could not retire the agent"))?;
    pause_jobs()?;
    Ok((StatusCode::OK, answer(snapshot.revision)))
}

/// `PUT` or `DELETE /api/agents/catalog`: a reserved name.
pub(super) async fn reserved_name() -> Response {
    Refusal::invalid_name().into_response()
}

/// Any other method on `/api/agents/{name}`.
pub(super) async fn agent_method_not_allowed() -> Response {
    method_not_allowed_allowing("GET,HEAD,PUT,DELETE")
}

#[derive(Serialize)]
struct SwitchBody {
    revision: String,
    configured: bool,
    effective: bool,
    restart_required: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SwitchPut {
    enabled: bool,
}

/// What is configured, what this running daemon uses, and whether a
/// restart is still needed for the two to agree.
fn switch_body(state: &AppState) -> Outcome<SwitchBody> {
    let snapshot = snapshot(state)?;
    let configured = snapshot.config.gateway.targeted_agents;
    let effective = state.config.load_full().gateway.targeted_agents;
    Ok(SwitchBody {
        revision: snapshot.revision,
        configured,
        effective,
        restart_required: configured != effective,
    })
}

/// `GET /api/gateway/targeted-agents`.
pub(super) async fn get_targeted_agents(State(state): State<AppState>) -> Response {
    respond(switch_body(&state).map(|body| (StatusCode::OK, body)))
}

/// `PUT /api/gateway/targeted-agents`: set the switch; it takes effect at
/// the next daemon restart (`restart_required`).
pub(super) async fn put_targeted_agents(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    respond(put_switch(&state, &uri, &headers, &body).await)
}

async fn put_switch(
    state: &AppState,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Outcome<(StatusCode, SwitchBody)> {
    let write = write_of(uri, headers)?;
    let SwitchPut { enabled } = serde_json::from_slice(body)
        .map_err(|_| Refusal::field("the body must be {\"enabled\":true|false}; nothing was written"))?;
    manage::commit_as_owner(
        state,
        &ManagedChange::SetTargetedAgents { enabled },
        &write,
        super::owner_authenticated(state, headers),
    )
    .await
    .map_err(owner_refusal_of)?;
    Ok((StatusCode::OK, switch_body(state)?))
}

/// Any other method on `/api/gateway/targeted-agents`.
pub(super) async fn switch_method_not_allowed() -> Response {
    method_not_allowed_allowing("GET,HEAD,PUT")
}

/// Purge is an explicit owner destructive action. Remove only the named
/// configuration through the same CAS/activation engine as agent management.
pub(super) async fn purge_config(state: &AppState, name: &str, headers: &HeaderMap) -> Result<(), Box<Response>> {
    if !state.config.desired().config.agents.contains_key(name) {
        return Ok(());
    }
    let write = Write::of(&WriteQuery::confirming(true), headers)?;
    let change = ManagedChange::PurgeAgent { agent: name.to_owned() };
    manage::commit_as_owner(state, &change, &write, true)
        .await
        .map_err(|error| Box::new(owner_refusal_of(error).into_response()))?;
    Ok(())
}

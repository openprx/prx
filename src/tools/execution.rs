//! Canonical tool execution contract.
//!
//! This module owns the fixed application pipeline for native tools and MCP
//! aliases. Existing [`Tool`] implementations remain adapter-owned during the
//! migration; callers submit a typed command and runtime context instead of
//! resolving and invoking adapters directly.

use super::schema::ToolSchemaDialect;
use super::traits::{Tool, ToolCategory, ToolResult, ToolSpec, ToolTier, is_tool_cancelled_result};
use crate::capability::CapabilityAvailability;
use crate::memory::{Memory, MessageEventInput};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::security::SecurityPolicy;
use crate::security::policy::{RUNTIME_APPROVAL_GRANT_ARG, RUNTIME_APPROVAL_GRANTED_ARG, ToolDecision};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Instant;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Coarse side-effect classification used by the common policy stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    Read,
    Act,
}

/// Adapter family selected for a public tool name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolAdapterKind {
    Native,
    McpAlias,
    WasmPlugin,
    Skill,
}

/// Small raw execution port consumed by the application service.
///
/// Backends expose descriptors and invocation only; policy, approval, execution
/// preparation, auditing, and typed outcomes remain service-owned.
#[async_trait]
pub trait ToolBackend: Send + Sync {
    /// Read-only operation metadata; never an execution or approval permit.
    async fn resolve_approval_operation(
        &self,
        _public_name: &str,
        _arguments: &serde_json::Value,
    ) -> anyhow::Result<Option<(String, crate::acl::approval_grant::RiskLevel)>> {
        Ok(None)
    }
    /// Defaults closed for aliases, plugins and unaudited backends.
    fn supports_owner_filesystem(&self) -> bool {
        false
    }
    fn process_worker_compatible(&self) -> bool {
        true
    }
    fn execution_parameters_schema(&self) -> Option<Arc<serde_json::Value>> {
        None
    }
    fn root_name(&self) -> &str;
    fn specs(&self) -> Vec<ToolSpec>;
    fn catalog_specs(&self) -> Vec<ToolSpec> {
        self.specs()
    }
    fn immutable_catalog_specs(&self) -> Option<&[ToolSpec]> {
        None
    }
    fn shared_catalog_snapshot(&self) -> Option<super::SharedToolCatalogSnapshot> {
        None
    }
    fn catalog_binding_snapshot(&self) -> std::collections::BTreeMap<String, [u8; 32]> {
        std::collections::BTreeMap::new()
    }
    fn spec_for_name(&self, public_name: &str) -> Option<ToolSpec> {
        self.specs().into_iter().find(|spec| spec.name == public_name)
    }
    fn supports_name(&self, public_name: &str) -> bool;
    fn tier(&self) -> ToolTier;
    fn categories(&self) -> Vec<ToolCategory>;
    fn adapter_kind(&self, public_name: &str) -> ToolAdapterKind;
    fn availability(&self) -> CapabilityAvailability {
        CapabilityAvailability::ready(format!("executable backend '{}' is registered", self.root_name()))
    }

    fn catalog_availability_is_configured(&self) -> bool {
        let availability = self.availability();
        availability.level >= crate::capability::CapabilityAvailabilityLevel::Configured
            || availability.reason.trim().is_empty()
    }

    async fn invoke(
        &self,
        public_name: &str,
        arguments: serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<ToolResult>;

    async fn invoke_observed(
        &self,
        public_name: &str,
        arguments: serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<super::traits::ToolInvocationResult> {
        self.invoke(public_name, arguments, cancellation).await.map(Into::into)
    }
}

/// Compatibility adapter for the existing native and MCP [`Tool`] registry.
pub struct LegacyToolAdapter {
    tool: Arc<dyn Tool>,
}

/// Named adapter over a shared boxed registry. Chat Redux keeps this legacy
/// registry alive across provider turns, so migration must borrow it rather
/// than consume or duplicate stateful tool instances.
struct SharedRegistryToolAdapter {
    registry: Arc<Vec<Box<dyn Tool>>>,
    root_name: String,
}

impl SharedRegistryToolAdapter {
    fn tool(&self) -> Option<&dyn Tool> {
        self.registry
            .iter()
            .find(|tool| tool.name() == self.root_name)
            .map(Box::as_ref)
    }
}

#[async_trait]
impl ToolBackend for SharedRegistryToolAdapter {
    async fn invoke_observed(
        &self,
        public_name: &str,
        arguments: serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<super::traits::ToolInvocationResult> {
        self.tool()
            .ok_or_else(|| anyhow::anyhow!("shared tool is no longer registered"))?
            .execute_named_observed(public_name, arguments, cancellation)
            .await
    }

    fn execution_parameters_schema(&self) -> Option<Arc<serde_json::Value>> {
        self.tool()?.execution_parameters_schema()
    }
    fn supports_owner_filesystem(&self) -> bool {
        self.tool().is_some_and(Tool::supports_owner_filesystem)
    }
    fn process_worker_compatible(&self) -> bool {
        self.tool().is_some_and(Tool::process_worker_compatible)
    }
    async fn resolve_approval_operation(
        &self,
        public_name: &str,
        arguments: &serde_json::Value,
    ) -> anyhow::Result<Option<(String, crate::acl::approval_grant::RiskLevel)>> {
        let tool = self
            .tool()
            .ok_or_else(|| anyhow::anyhow!("approval tool unavailable"))?;
        anyhow::ensure!(public_name == tool.name(), "approval alias is unsupported");
        tool.resolve_approval_operation(arguments).await
    }
    fn root_name(&self) -> &str {
        &self.root_name
    }

    fn specs(&self) -> Vec<ToolSpec> {
        self.tool().map_or_else(Vec::new, Tool::specs)
    }

    fn catalog_specs(&self) -> Vec<ToolSpec> {
        self.tool().map_or_else(Vec::new, Tool::catalog_specs)
    }

    fn immutable_catalog_specs(&self) -> Option<&[ToolSpec]> {
        self.tool()?.immutable_catalog_specs()
    }

    fn shared_catalog_snapshot(&self) -> Option<super::SharedToolCatalogSnapshot> {
        self.tool()?.shared_catalog_snapshot()
    }

    fn catalog_binding_snapshot(&self) -> std::collections::BTreeMap<String, [u8; 32]> {
        self.tool()
            .map_or_else(std::collections::BTreeMap::new, Tool::catalog_binding_snapshot)
    }

    fn spec_for_name(&self, public_name: &str) -> Option<ToolSpec> {
        self.tool()?.spec_for_name(public_name)
    }

    fn supports_name(&self, public_name: &str) -> bool {
        self.tool().is_some_and(|tool| tool.supports_name(public_name))
    }

    fn tier(&self) -> ToolTier {
        self.tool().map_or(ToolTier::Standard, Tool::tier)
    }

    fn categories(&self) -> Vec<ToolCategory> {
        self.tool().map_or_else(Vec::new, |tool| tool.categories().to_vec())
    }

    fn adapter_kind(&self, public_name: &str) -> ToolAdapterKind {
        if self.root_name == "mcp_call" && public_name != self.root_name {
            ToolAdapterKind::McpAlias
        } else if self.root_name == "wasm_plugin_call" {
            ToolAdapterKind::WasmPlugin
        } else if self.root_name == "skill_execute" || self.root_name == "skill_read" && public_name != self.root_name {
            ToolAdapterKind::Skill
        } else {
            ToolAdapterKind::Native
        }
    }

    fn availability(&self) -> CapabilityAvailability {
        self.tool().map_or_else(
            || CapabilityAvailability::declared(format!("tool '{}' is no longer registered", self.root_name)),
            Tool::availability,
        )
    }

    fn catalog_availability_is_configured(&self) -> bool {
        self.tool().is_some_and(Tool::catalog_availability_is_configured)
    }

    async fn invoke(
        &self,
        public_name: &str,
        arguments: serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        let Some(tool) = self.tool() else {
            anyhow::bail!("shared tool '{}' is no longer registered", self.root_name);
        };
        tool.execute_named_with_cancellation(public_name, arguments, cancellation)
            .await
    }
}

impl LegacyToolAdapter {
    #[must_use]
    pub fn new(tool: Arc<dyn Tool>) -> Self {
        Self { tool }
    }
}

#[async_trait]
impl ToolBackend for LegacyToolAdapter {
    async fn invoke_observed(
        &self,
        public_name: &str,
        arguments: serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<super::traits::ToolInvocationResult> {
        self.tool
            .as_ref()
            .execute_named_observed(public_name, arguments, cancellation)
            .await
    }

    fn execution_parameters_schema(&self) -> Option<Arc<serde_json::Value>> {
        self.tool.execution_parameters_schema()
    }
    fn supports_owner_filesystem(&self) -> bool {
        self.tool.supports_owner_filesystem()
    }
    fn process_worker_compatible(&self) -> bool {
        self.tool.process_worker_compatible()
    }
    async fn resolve_approval_operation(
        &self,
        public_name: &str,
        arguments: &serde_json::Value,
    ) -> anyhow::Result<Option<(String, crate::acl::approval_grant::RiskLevel)>> {
        anyhow::ensure!(public_name == self.tool.name(), "approval alias is unsupported");
        self.tool.resolve_approval_operation(arguments).await
    }
    fn root_name(&self) -> &str {
        self.tool.name()
    }

    fn specs(&self) -> Vec<ToolSpec> {
        self.tool.specs()
    }

    fn catalog_specs(&self) -> Vec<ToolSpec> {
        self.tool.catalog_specs()
    }

    fn immutable_catalog_specs(&self) -> Option<&[ToolSpec]> {
        self.tool.immutable_catalog_specs()
    }

    fn shared_catalog_snapshot(&self) -> Option<super::SharedToolCatalogSnapshot> {
        self.tool.shared_catalog_snapshot()
    }

    fn catalog_binding_snapshot(&self) -> std::collections::BTreeMap<String, [u8; 32]> {
        self.tool.catalog_binding_snapshot()
    }

    fn spec_for_name(&self, public_name: &str) -> Option<ToolSpec> {
        self.tool.spec_for_name(public_name)
    }

    fn supports_name(&self, public_name: &str) -> bool {
        self.tool.supports_name(public_name)
    }

    fn tier(&self) -> ToolTier {
        self.tool.tier()
    }

    fn categories(&self) -> Vec<ToolCategory> {
        self.tool.categories().to_vec()
    }

    fn adapter_kind(&self, public_name: &str) -> ToolAdapterKind {
        if self.tool.name() == "mcp_call" && public_name != self.tool.name() {
            ToolAdapterKind::McpAlias
        } else if self.tool.name() == "wasm_plugin_call" {
            ToolAdapterKind::WasmPlugin
        } else if self.tool.name() == "skill_execute"
            || self.tool.name() == "skill_read" && public_name != self.tool.name()
        {
            ToolAdapterKind::Skill
        } else {
            ToolAdapterKind::Native
        }
    }

    fn availability(&self) -> CapabilityAvailability {
        self.tool.availability()
    }

    fn catalog_availability_is_configured(&self) -> bool {
        self.tool.catalog_availability_is_configured()
    }

    async fn invoke(
        &self,
        public_name: &str,
        arguments: serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        self.tool
            .execute_named_with_cancellation(public_name, arguments, cancellation)
            .await
    }
}

/// Immutable public descriptor captured before policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub public_name: String,
    pub backend_name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    /// Full private built-in contract; None means the public contract is complete.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::tools::schema::serialize_execution_parameters",
        deserialize_with = "crate::tools::schema::deserialize_execution_parameters"
    )]
    pub execution_parameters: Option<Arc<serde_json::Value>>,
    pub tier: ToolTier,
    pub categories: Vec<ToolCategory>,
    pub effect: ToolEffect,
    pub adapter: ToolAdapterKind,
    #[serde(default = "process_worker_compatible_by_default")]
    pub process_worker_compatible: bool,
    /// Evidence-backed runtime availability. Registered executable tools are
    /// `Ready`; `Healthy` is reserved for a positive runtime probe.
    pub availability: CapabilityAvailability,
}

impl ToolDescriptor {
    fn with_execution_parameters(mut self, schema: Option<Arc<serde_json::Value>>) -> Self {
        // A dynamic alias owns its own schema, never its backend's root contract.
        if self.public_name == self.backend_name {
            self.execution_parameters = schema;
        }
        self
    }
}

const fn process_worker_compatible_by_default() -> bool {
    true
}

/// Fresh executable spec for one parity pass. The backend identity is borrowed
/// from the live registry; published dynamic specs share one captured live view.
/// Legacy dynamic backends retain their owned-inventory fallback.
pub(crate) struct CurrentToolSpec<'a> {
    pub(crate) backend_name: &'a str,
    pub(crate) spec: CurrentToolSpecValue<'a>,
    pub(crate) configured: bool,
}

pub(crate) enum CurrentToolSpecValue<'a> {
    Legacy(std::borrow::Cow<'a, ToolSpec>),
    Shared {
        entry: Arc<super::SharedToolCatalogEntry>,
        snapshot: super::SharedToolCatalogSnapshot,
    },
}

impl std::ops::Deref for CurrentToolSpecValue<'_> {
    type Target = ToolSpec;
    fn deref(&self) -> &ToolSpec {
        match self {
            Self::Legacy(spec) => spec,
            Self::Shared { entry, .. } => entry.spec(),
        }
    }
}

/// A captured snapshot's missing binding is authoritative. Only legacy
/// backends may query a fresh binding map as fallback.
pub(crate) enum SharedBinding {
    Legacy,
    Snapshot(Option<[u8; 32]>),
}

impl SharedBinding {
    pub(crate) const fn is_snapshot(&self) -> bool {
        matches!(self, Self::Snapshot(_))
    }

    pub(crate) fn or_else_live(self, fallback: impl FnOnce() -> Option<[u8; 32]>) -> Option<[u8; 32]> {
        match self {
            Self::Legacy => fallback(),
            Self::Snapshot(binding) => binding,
        }
    }
}

impl CurrentToolSpecValue<'_> {
    pub(crate) fn fingerprint(&self) -> crate::causal_tree::branch::SchemaFingerprint {
        match self {
            Self::Shared { entry, .. } => crate::causal_tree::branch::SchemaFingerprint(entry.fingerprint()),
            Self::Legacy(spec) => {
                super::capability::fingerprint::tool_spec_fingerprint(&spec.name, &spec.description, &spec.parameters)
            }
        }
    }
    pub(crate) fn shared_named_binding(&self, name: &str) -> SharedBinding {
        match self {
            Self::Shared { snapshot, .. } => SharedBinding::Snapshot(snapshot.binding_digest(name)),
            Self::Legacy(_) => SharedBinding::Legacy,
        }
    }

    pub(crate) fn shared_binding_digest(&self) -> SharedBinding {
        match self {
            Self::Shared { entry, .. } => SharedBinding::Snapshot(entry.binding_digest()),
            Self::Legacy(_) => SharedBinding::Legacy,
        }
    }
}

fn current_tool_spec<'a>(backend: &'a dyn ToolBackend, spec: std::borrow::Cow<'a, ToolSpec>) -> CurrentToolSpec<'a> {
    // Match tool_descriptor's legacy empty-reason promotion.
    let configured = backend.catalog_availability_is_configured()
        && crate::tools::schema::SchemaCleanr::validate(&spec.parameters).is_ok();
    CurrentToolSpec {
        backend_name: backend.root_name(),
        spec: CurrentToolSpecValue::Legacy(spec),
        configured,
    }
}

/// One normalized execution request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExecutionCommand {
    pub operation_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub idempotency_key: Option<String>,
}

impl ToolExecutionCommand {
    #[must_use]
    pub fn new(name: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self {
            operation_id: Uuid::now_v7().to_string(),
            name: name.into(),
            arguments,
            idempotency_key: None,
        }
    }

    #[must_use]
    pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }

    #[must_use]
    pub fn with_operation_id(mut self, operation_id: impl Into<String>) -> Self {
        self.operation_id = operation_id.into();
        self
    }
}

/// Authenticated runtime facade available to every execution stage.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct H2ExecutionMetadata {
    controlled_case: Option<Arc<crate::runtime::h2_entry::ControlledCaseScope>>,
    origin: Option<Arc<crate::runtime::h2_entry::VerifiedEntryOrigin>>,
    review: Option<crate::runtime::h2_entry::LocalOwnerReview>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutionContext {
    h2_metadata: Option<Box<H2ExecutionMetadata>>,
    pub(crate) owner_filesystem: Option<Arc<super::owner_filesystem::OwnerFilesystem>>,
    pub(crate) capability_search: Option<super::capability::search_tool::CapabilitySearchPort>,
    pub(crate) ordinary_child_publication: Option<crate::runtime::turn_decision::OrdinaryChildPublication>,
    pub(crate) child_grant: Option<Arc<crate::runtime::child_source::IssuedChildGrant>>,
    pub(crate) decision_resume: Option<super::capability::execution_binding::DecisionResumePort>,
    pub(crate) decision_approval: Option<super::capability::execution_binding::DecisionApprovalPort>,
    pub(crate) execution_trace: Option<super::capability::execution_trace::ExecutionTrace>,
    /// None is retained for non-model service callers. A constrained model
    /// request carries even an empty map, so absence cannot widen its surface.
    pub(crate) execution_bindings:
        Option<Arc<std::collections::BTreeMap<String, super::capability::execution_binding::ExecutionBinding>>>,
    pub envelope: RuntimeEnvelope,
    pub chat_type: String,
    pub chat_id: String,
    pub semantic_turn_id: String,
    /// Which rewrite of the published tool schemas the model behind this turn
    /// was actually shown.
    ///
    /// Argument validation enforces *that* document, not the published one: a
    /// provider whose API rejects `additionalProperties` / `minLength` /
    /// `minimum` never showed those constraints to the model, so holding the
    /// model to them rejects calls it had no way to get right. Defaults to
    /// [`ToolSchemaDialect::Raw`] — the published document — which is correct
    /// for OpenAI and for every non-model entry point.
    pub schema_dialect: ToolSchemaDialect,
}

impl ToolExecutionContext {
    pub(crate) fn controlled_case(&self) -> Option<&Arc<crate::runtime::h2_entry::ControlledCaseScope>> {
        self.h2_metadata
            .as_ref()
            .and_then(|metadata| metadata.controlled_case.as_ref())
    }
    pub(crate) fn h2_origin(&self) -> Option<&Arc<crate::runtime::h2_entry::VerifiedEntryOrigin>> {
        self.h2_metadata.as_ref().and_then(|metadata| metadata.origin.as_ref())
    }
    pub(crate) fn h2_review(&self) -> Option<&crate::runtime::h2_entry::LocalOwnerReview> {
        self.h2_metadata.as_ref().and_then(|metadata| metadata.review.as_ref())
    }
    fn normalize_h2_metadata(&mut self) {
        if self.h2_metadata.as_ref().is_some_and(|metadata| {
            metadata.controlled_case.is_none() && metadata.origin.is_none() && metadata.review.is_none()
        }) {
            self.h2_metadata = None;
        }
    }
    pub(crate) fn set_controlled_case(&mut self, case: Arc<crate::runtime::h2_entry::ControlledCaseScope>) {
        self.h2_metadata.get_or_insert_with(Box::default).controlled_case = Some(case);
    }
    pub(crate) fn set_h2_origin(&mut self, origin: Option<Arc<crate::runtime::h2_entry::VerifiedEntryOrigin>>) {
        if self.h2_metadata.is_none() && origin.is_none() {
            return;
        }
        self.h2_metadata.get_or_insert_with(Box::default).origin = origin;
        self.normalize_h2_metadata();
    }
    pub(crate) fn set_h2_review(&mut self, review: Option<crate::runtime::h2_entry::LocalOwnerReview>) {
        if self.h2_metadata.is_none() && review.is_none() {
            return;
        }
        self.h2_metadata.get_or_insert_with(Box::default).review = review;
        self.normalize_h2_metadata();
    }
    #[must_use]
    pub fn new(envelope: RuntimeEnvelope, chat_type: impl Into<String>) -> Self {
        let chat_id = envelope.session_key.clone();
        let semantic_turn_id = envelope
            .source_message_event_id
            .clone()
            .or_else(|| envelope.run_id.clone())
            .or_else(|| envelope.task_id.clone())
            .unwrap_or_else(|| envelope.session_key.clone());
        Self {
            h2_metadata: None,
            owner_filesystem: None,
            capability_search: None,
            ordinary_child_publication: None,
            child_grant: None,
            decision_approval: None,
            decision_resume: None,
            execution_bindings: None,
            execution_trace: None,
            envelope,
            chat_type: chat_type.into(),
            chat_id,
            semantic_turn_id,
            schema_dialect: ToolSchemaDialect::Raw,
        }
    }

    /// Declare the schema rewrite the serving provider applied for this turn.
    #[must_use]
    pub const fn with_schema_dialect(mut self, schema_dialect: ToolSchemaDialect) -> Self {
        self.schema_dialect = schema_dialect;
        self
    }

    #[must_use]
    pub fn with_chat_id(mut self, chat_id: impl Into<String>) -> Self {
        self.chat_id = chat_id.into();
        self
    }

    #[must_use]
    pub fn with_semantic_turn_id(mut self, semantic_turn_id: impl Into<String>) -> Self {
        self.semantic_turn_id = semantic_turn_id.into();
        self
    }

    pub(super) fn sender(&self) -> &str {
        self.envelope.sender.as_deref().unwrap_or("unknown")
    }

    pub(super) fn channel(&self) -> &str {
        self.envelope.channel.as_deref().unwrap_or("unknown")
    }

    /// Whether this turn was initiated by the **local machine operator** — the
    /// human sitting at the PRX terminal UI — as opposed to a remote IM user, a
    /// gateway/webhook client, the console web UI, cron, or a spawned worker.
    ///
    /// This is the authority behind the runtime-injected `_prx_scope_trusted`
    /// marker that guards destructive `gateway` actions (`config.patch`,
    /// `restart`). It answers *who asked*, which at the default
    /// [`AutonomyLevel::Full`] is the only thing standing between a remote
    /// WhatsApp/Telegram message and a daemon restart: `SecurityPolicy::decide`
    /// returns `Allow` for every side-effecting tool at `Full`, so no approval
    /// is ever requested and `SideEffectGate::authorize_resource_operation`
    /// passes medium-risk operations unconditionally. The second gate only
    /// bites at `Supervised`; this one is what bites at the default.
    ///
    /// **Why the whole tuple and not `envelope.source`.** Every agent tool loop
    /// builds its envelope with [`RuntimeEnvelope::chat_terminal`] regardless of
    /// origin (see `agent_tool_execution_context` in `src/agent/execution_policy.rs`), so
    /// `source` is `Chat` even for a Telegram turn and carries no signal at all.
    /// The discriminating fields are the scope strings, and they are checked as
    /// a set so that no single one has to be trusted alone:
    ///
    /// | origin | channel | sender | chat_id | chat_type |
    /// |---|---|---|---|---|
    /// | local terminal UI | `terminal` | `user` | `terminal:user` | `private` |
    /// | IM channel | adapter name | remote user id | reply target | inferred |
    /// | gateway / webhook | `webhook` / adapter | client id | `gateway:…` | `gateway` |
    /// | console web UI | `console` | `console-user` | session key | `private` |
    ///
    /// The gateway surface cannot forge this even in principle: its `chat_id` is
    /// `GatewayFabricContext`'s `session_key`, which is always built with a
    /// literal `gateway:` prefix (`src/gateway/mod.rs:149,159`), so it can never
    /// equal `terminal:user`.
    ///
    /// **Fail-closed by construction.** The predicate is an allowlist of one
    /// exact tuple; anything unrecognised — a new runtime source, a renamed
    /// channel, a missing envelope field (`sender()`/`channel()` fall back to
    /// `"unknown"`) — lands on `false`, i.e. denied. It is never derived from
    /// caller input: the two production sites that produce this tuple hardcode
    /// every field (`src/chat/dispatcher.rs:2072-2090` and `src/chat/mod.rs:7142`).
    #[must_use]
    pub fn is_local_operator(&self) -> bool {
        self.envelope.source == crate::runtime::envelope::RuntimeSource::Chat
            && self.channel() == LOCAL_OPERATOR_CHANNEL
            && self.sender() == LOCAL_OPERATOR_SENDER
            && self.chat_id == LOCAL_OPERATOR_CHAT_ID
            && self.chat_type == LOCAL_OPERATOR_CHAT_TYPE
    }
}

/// The exact scope tuple the local terminal UI stamps on every turn it drives.
///
/// Kept as constants next to [`ToolExecutionContext::is_local_operator`] so the
/// allowlist is one grep away from the sites that mint it.
const LOCAL_OPERATOR_CHANNEL: &str = "terminal";
const LOCAL_OPERATOR_SENDER: &str = "user";
const LOCAL_OPERATOR_CHAT_ID: &str = "terminal:user";
const LOCAL_OPERATOR_CHAT_TYPE: &str = "private";

/// The exact scope tuple a `prx agent` CLI turn (single-shot or REPL) stamps
/// on every tool call: `agent_tool_execution_context(None, "cli")` in
/// `src/agent/execution_policy.rs`. Cron agent jobs and xin tasks enter through the same
/// `run_with_runtime_envelope`, but carry this tuple only when their
/// [`JobOrigin`] is [`JobOrigin::LocalOperator`]; every other job runs under
/// [`JobOrigin::scheduled_tool_context`] instead.
const LOCAL_CLI_CHANNEL: &str = "cli";
const LOCAL_CLI_SENDER: &str = "agent";
const LOCAL_CLI_CHAT_ID: &str = "agent:local";

/// Whether the service-authored scope in `args` comes from a **local-process
/// operator surface**: the terminal UI or the `prx agent` CLI (D-019 Q-5).
///
/// This is wider than [`ToolExecutionContext::is_local_operator`] on purpose
/// and must never replace it: that predicate (`_prx_scope_trusted`) guards
/// destructive `gateway` actions and answers "is a human at this terminal",
/// while this one answers "is this turn the operator's own process rather
/// than a network surface" — the question the workspace-wide sub-agent run
/// view is gated on. Every network surface (gateway, webhook, Web Console,
/// MCP, IM channels) and every sub-agent worker answers `false`.
///
/// Fail-closed like its sibling: both tuples are compared field for field
/// against the values the runtime itself authored into `_zc_scope`
/// (`normalize_arguments`), an owner-isolated scope never qualifies, and an
/// untrusted or missing scope is `false`. No network surface can mint either
/// tuple: gateway chat ids carry a literal `gateway:` prefix, IM chat ids are
/// the adapter's reply target, and the console's are session keys.
#[must_use]
pub fn scope_is_local_process_operator(args: &serde_json::Value) -> bool {
    let trusted = args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !trusted || scope_requires_owner_match(args) {
        return false;
    }
    if args
        .get("_prx_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return true;
    }
    let Some(scope) = args.get("_zc_scope").and_then(serde_json::Value::as_object) else {
        return false;
    };
    let field = |key: &str| scope.get(key).and_then(serde_json::Value::as_str);
    field("runtime_source") == Some(crate::runtime::envelope::RuntimeSource::Chat.as_str())
        && field("channel") == Some(LOCAL_CLI_CHANNEL)
        && field("sender") == Some(LOCAL_CLI_SENDER)
        && field("chat_id") == Some(LOCAL_CLI_CHAT_ID)
        && field("chat_type") == Some(LOCAL_OPERATOR_CHAT_TYPE)
}

/// Who created a persisted background job — a cron job, a xin task or a xin
/// goal — and therefore whose authority its agent runs execute under
/// (CRON-ORIGIN; D-016, D-019 Q-5, D-026).
///
/// A scheduled agent run has no live caller: the turn that created the job
/// ended long before. Without a recorded origin every run used to execute with
/// the `prx agent` CLI tuple, so a job scheduled from a channel, webhook or the
/// Web Console ran with the local operator's authority: the workspace-wide run
/// view and the `MEMORY.md` mirror. The origin is captured from the creating
/// turn's service-authored scope, persisted with the job, and never changed by
/// a later update, so a job can never gain authority after it was created.
///
/// Fail-closed: [`Self::LocalOperator`] is granted only when the creating turn
/// was itself the local-process operator
/// ([`scope_is_local_process_operator`]) or the job was written by a path that
/// only a local operator reaches (the `prx cron` / `prx xin` CLI, built-in and
/// `HEARTBEAT.md` system tasks). Anything else — a network surface, a
/// sub-agent, an untrusted scope, a row stored before origins existed, an
/// unreadable stored value — never runs with operator authority.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobOrigin {
    /// Created by the local operator. Runs keep the `prx agent` CLI tuple and
    /// with it the workspace-wide run view and the `MEMORY.md` mirror.
    LocalOperator,
    /// Created from a trusted, non-operator scope. Runs act as this principal.
    Principal {
        channel: String,
        sender: String,
        chat_type: String,
        #[serde(default)]
        owner_id: Option<String>,
        #[serde(default)]
        owner_isolated: bool,
    },
    /// No attributable creator: a row stored before origins were recorded, or
    /// a job created without a trusted scope. Runs act as the job itself — its
    /// stored owner, if any — never as the operator.
    #[default]
    Unattributed,
}

/// Channel segment of the per-job owner an ownerless scheduled job acts as
/// (R-CRON F-4). No channel adapter is named this, so the derived owner can
/// never equal a real principal's.
const UNATTRIBUTED_OWNER_CHANNEL: &str = "unattributed-job";

/// The value an absent origin field resolves to in [`JobOrigin::Principal`].
const JOB_ORIGIN_UNKNOWN: &str = "unknown";

impl JobOrigin {
    /// The origin a set of tool arguments proves for a job it creates.
    ///
    /// Read only from the service-authored `_zc_scope` (guarded by
    /// `_zc_scope_trusted`): a caller can neither claim the operator origin nor
    /// shed its own. `owner_id` is the owner the creating tool resolved for the
    /// job's lineage, so the origin and the job's `owner_id` column agree.
    #[must_use]
    pub fn from_trusted_args(args: &serde_json::Value, owner_id: Option<&str>) -> Self {
        let trusted = args
            .get("_zc_scope_trusted")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let Some(scope) = args
            .get("_zc_scope")
            .and_then(serde_json::Value::as_object)
            .filter(|_| trusted)
        else {
            return Self::Unattributed;
        };
        if scope_is_local_process_operator(args) {
            return Self::LocalOperator;
        }
        let field = |key: &str| {
            scope
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(JOB_ORIGIN_UNKNOWN)
                .to_string()
        };
        Self::Principal {
            channel: field("channel"),
            sender: field("sender"),
            chat_type: field("chat_type"),
            owner_id: owner_id
                .map(str::trim)
                .filter(|owner| !owner.is_empty())
                .map(str::to_string),
            owner_isolated: scope_requires_owner_match(args),
        }
    }

    /// Decode a stored origin column.
    ///
    /// `None` (a row written before the column existed) and an unreadable
    /// value both resolve to [`Self::Unattributed`]: an origin that cannot be
    /// proven is never the operator's.
    #[must_use]
    pub fn from_stored(raw: Option<&str>) -> Self {
        let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
            return Self::Unattributed;
        };
        serde_json::from_str(raw).unwrap_or_else(|error| {
            tracing::warn!(%error, "unreadable scheduled-job origin; running the job as unattributed");
            Self::Unattributed
        })
    }

    /// The column value [`Self::from_stored`] reads back.
    ///
    /// # Errors
    /// Only if serialization fails, which a plain enum of strings cannot do in
    /// practice; the error is propagated rather than stored as a guess.
    pub fn to_stored(&self) -> anyhow::Result<String> {
        serde_json::to_string(self).map_err(|error| anyhow::anyhow!("failed to encode job origin: {error}"))
    }

    /// Whether runs of this job keep the local operator's authority.
    #[must_use]
    pub const fn is_local_operator(&self) -> bool {
        matches!(self, Self::LocalOperator)
    }

    /// Whether the creator was an owner-isolated origin (an external MCP
    /// caller); its runs are then owner-isolated too.
    #[must_use]
    pub const fn is_owner_isolated(&self) -> bool {
        matches!(
            self,
            Self::Principal {
                owner_isolated: true,
                ..
            }
        )
    }

    /// The owner a principal origin recorded, if any.
    #[must_use]
    pub fn owner_id(&self) -> Option<&str> {
        match self {
            Self::Principal { owner_id, .. } => owner_id.as_deref(),
            Self::LocalOperator | Self::Unattributed => None,
        }
    }

    /// [`Self::scheduled_tool_context`] paired with the configured policy for
    /// it (see [`ScheduledToolScope::new`]); `None` for an operator job, which
    /// keeps the operator's CLI context and policy.
    #[must_use]
    pub fn scheduled_tool_scope(
        &self,
        run_envelope: RuntimeEnvelope,
        config: &crate::config::Config,
    ) -> Option<ScheduledToolScope> {
        self.scheduled_tool_context(run_envelope)
            .map(|context| ScheduledToolScope::new(context, config))
    }

    /// The tool execution context a scheduled run of this job must use, or
    /// `None` when it keeps the local operator's CLI context.
    ///
    /// `run_envelope` is the job's own run envelope (`RuntimeEnvelope::cron` /
    /// `RuntimeEnvelope::xin`, carrying the job's stored owner). The result
    /// keeps its `cron`/`xin` runtime source and its `cron:{id}` / `xin:{id}`
    /// session key as the chat id, so neither operator predicate —
    /// [`ToolExecutionContext::is_local_operator`] (source `Chat`, chat id
    /// `terminal:user`) nor [`scope_is_local_process_operator`] (source `Chat`,
    /// chat id `agent:local`) — can match it, whatever the recorded channel and
    /// sender are. A principal origin additionally acts as its creator: its
    /// channel, sender, chat type, owner and owner isolation.
    ///
    /// A job with no owner — neither recorded by its origin nor stored on the
    /// row — acts as an owner of its own, derived from its run envelope's
    /// `cron:{id}` / `xin:{id}` session key (R-CRON F-4), so ownerless jobs do
    /// not share one memory, run-view and job-management bucket.
    #[must_use]
    pub fn scheduled_tool_context(&self, run_envelope: RuntimeEnvelope) -> Option<ToolExecutionContext> {
        let run_envelope = if run_envelope.owner_id.is_none() {
            let owner_id = crate::memory::principal::OwnerPrincipal::new(
                run_envelope.workspace_id.clone(),
                UNATTRIBUTED_OWNER_CHANNEL,
                run_envelope.session_key.clone(),
                run_envelope.session_key.clone(),
                vec![crate::memory::principal::Role::Anonymous],
            )
            .owner_id;
            run_envelope.with_owner_id(owner_id)
        } else {
            run_envelope
        };
        match self {
            Self::LocalOperator => None,
            Self::Unattributed => Some(ToolExecutionContext::new(run_envelope, JOB_ORIGIN_UNKNOWN)),
            Self::Principal {
                channel,
                sender,
                chat_type,
                owner_id,
                owner_isolated,
            } => {
                let mut envelope = run_envelope.with_channel(channel.as_str()).with_sender(sender.as_str());
                if let Some(owner_id) = owner_id {
                    envelope = envelope.with_owner_id(owner_id.as_str());
                }
                if *owner_isolated {
                    envelope = envelope.with_owner_isolation();
                }
                Some(ToolExecutionContext::new(envelope, chat_type.as_str()))
            }
        }
    }
}

/// What a non-operator scheduled run's tool calls are decided with: the
/// creator's context and the policy that applies to it (CRON-ORIGIN, R-CRON
/// F-1).
#[derive(Clone)]
pub struct ScheduledToolScope {
    pub context: ToolExecutionContext,
    pub policy: Arc<SecurityPolicy>,
}

impl ScheduledToolScope {
    /// Pair `context` with the configured policy, so the creator's
    /// `[autonomy.scopes]` rules — keyed on the channel, sender and chat type
    /// the context carries — and the configured autonomy level apply to the
    /// run exactly as they applied to the creator's own turn.
    ///
    /// One difference is deliberate: a scheduled run is unattended, so nobody
    /// can answer an approval prompt. Under `supervised` a call that would
    /// ask is refused instead (the level is read as `read_only` for the
    /// effect decision), never approved on the creator's behalf and never put
    /// to whoever happens to hold the daemon's terminal.
    ///
    /// The policy comes from `build_security_policy`, the sanctioned
    /// constructor, so the configured audit block is wired exactly as on every
    /// other surface (BUG-D1-01).
    #[must_use]
    pub fn new(context: ToolExecutionContext, config: &crate::config::Config) -> Self {
        let mut policy = crate::runtime::bootstrap::build_security_policy(config);
        if policy.autonomy == crate::security::policy::AutonomyLevel::Supervised {
            // Freshly built and not yet shared, so this mutates in place.
            Arc::make_mut(&mut policy).autonomy = crate::security::policy::AutonomyLevel::ReadOnly;
        }
        Self { context, policy }
    }
}

impl std::fmt::Debug for ScheduledToolScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScheduledToolScope")
            .field("context", &self.context)
            .field("autonomy", &self.policy.autonomy)
            .finish_non_exhaustive()
    }
}

/// An immutable caller receipt issued only at the execution-service dispatch
/// boundary. Copying an envelope or decoding tool arguments cannot construct it.
#[derive(Clone)]
pub(crate) struct ChildCallContext {
    tool_name: &'static str,
    context: ToolExecutionContext,
}

impl ChildCallContext {
    pub(crate) fn verify_tool(&self, tool_name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(self.tool_name == tool_name, "child caller belongs to a different tool");
        Ok(())
    }

    pub(crate) const fn context(&self) -> &ToolExecutionContext {
        &self.context
    }
}

tokio::task_local! {
    // Installed only by the execution service after policy/preparation checks.
    // JSON arguments cannot supply this in-process child caller identity.
    static CHILD_CALL_CONTEXT: Option<ChildCallContext>;
}

pub(crate) fn current_child_call_context() -> Option<ChildCallContext> {
    CHILD_CALL_CONTEXT.try_with(Clone::clone).ok().flatten()
}

/// Validate an installed receipt without cloning its execution context. Direct
/// operator/legacy callers have no receipt and retain their existing admission.
pub(crate) fn verify_current_child_call(tool_name: &str) -> anyhow::Result<()> {
    CHILD_CALL_CONTEXT
        .try_with(|caller| caller.as_ref().map_or(Ok(()), |caller| caller.verify_tool(tool_name)))
        .unwrap_or(Ok(()))
}

tokio::task_local! {
    /// The non-operator tool scope of the scheduled run on this task, if any.
    static SCHEDULED_JOB_TOOL_SCOPE: Option<ScheduledToolScope>;
}

/// Run `future` with `scope` as the tool context and policy of every unscoped
/// agent tool loop inside it (see [`scheduled_job_tool_scope`]).
///
/// `None` leaves the loop on its historical CLI context and policy. Only the
/// scheduled entry (`run_with_runtime_envelope`) installs one, from the job's
/// [`JobOrigin`].
pub async fn with_scheduled_job_tool_scope<F: Future>(scope: Option<ScheduledToolScope>, future: F) -> F::Output {
    SCHEDULED_JOB_TOOL_SCOPE.scope(scope, future).await
}

/// The tool scope installed by [`with_scheduled_job_tool_scope`] for the
/// current task, if any. Task-locals do not cross `tokio::spawn`; sub-agents
/// spawned from a scheduled run take their scope from the service-authored
/// `_zc_scope` of the spawning call instead, which this context produced.
#[must_use]
pub fn scheduled_job_tool_scope() -> Option<ScheduledToolScope> {
    SCHEDULED_JOB_TOOL_SCOPE.try_with(Clone::clone).ok().flatten()
}

/// Typed policy decision used independently of the legacy policy enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionDecision {
    Allow,
    Ask,
    Deny,
}

/// Pure side-effect policy port.
pub trait EffectPolicy: Send + Sync {
    fn decide(&self, descriptor: &ToolDescriptor, context: &ToolExecutionContext) -> ToolExecutionDecision;
}

/// Adapter from the current authoritative [`SecurityPolicy`] decision point.
pub struct SecurityEffectPolicy {
    policy: Arc<SecurityPolicy>,
}

impl SecurityEffectPolicy {
    #[must_use]
    pub const fn new(policy: Arc<SecurityPolicy>) -> Self {
        Self { policy }
    }
}

impl EffectPolicy for SecurityEffectPolicy {
    fn decide(&self, descriptor: &ToolDescriptor, context: &ToolExecutionContext) -> ToolExecutionDecision {
        let policy_name = if descriptor.adapter == ToolAdapterKind::Skill {
            &descriptor.backend_name
        } else {
            &descriptor.public_name
        };
        decide_tool_execution(
            self.policy.as_ref(),
            policy_name,
            context.sender(),
            context.channel(),
            &context.chat_type,
        )
    }
}

/// Canonical autonomy/scope decision projection used by both the legacy Agent
/// loop and [`ToolExecutionService`]. Approval UI and execution are downstream;
/// neither entry point may reinterpret the policy result.
#[must_use]
pub fn decide_tool_execution(
    policy: &SecurityPolicy,
    tool_name: &str,
    sender: &str,
    channel: &str,
    chat_type: &str,
) -> ToolExecutionDecision {
    match policy.decide(tool_name, sender, channel, chat_type) {
        ToolDecision::Allow => ToolExecutionDecision::Allow,
        ToolDecision::Ask => ToolExecutionDecision::Ask,
        ToolDecision::Deny => ToolExecutionDecision::Deny,
    }
}

/// Request passed to a UI, queue, signed-grant, or fail-closed approval adapter.
#[derive(Debug, Clone)]
pub struct ToolApprovalRequest {
    pub command: ToolExecutionCommand,
    pub descriptor: ToolDescriptor,
    pub context: ToolExecutionContext,
    pub(crate) decision_call: Option<super::capability::execution_binding::VerifiedDecisionCall>,
}

/// Approval result. Runtime-only grant material is never accepted from the
/// original command arguments; only this trusted adapter may supply it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolApprovalDecision {
    Approved {
        runtime_approval_granted: bool,
        runtime_grant: Option<serde_json::Value>,
    },
    Denied {
        reason: String,
    },
    Cancelled {
        reason: String,
    },
}

#[async_trait]
pub trait ApprovalStrategy: Send + Sync {
    async fn resolve(&self, request: ToolApprovalRequest) -> ToolApprovalDecision;
}

/// Safe default for contexts without an interactive or persisted resolver.
#[derive(Debug, Default)]
pub struct DenyApprovalStrategy;

#[async_trait]
impl ApprovalStrategy for DenyApprovalStrategy {
    async fn resolve(&self, _request: ToolApprovalRequest) -> ToolApprovalDecision {
        ToolApprovalDecision::Denied {
            reason: "approval required but no approval resolver is available".to_string(),
        }
    }
}

/// Prepared execution boundary returned by the preparation stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExecutionPermit {
    pub strategy: String,
}

#[async_trait]
pub trait ToolExecutionPreparation: Send + Sync {
    async fn prepare(
        &self,
        descriptor: &ToolDescriptor,
        command: &ToolExecutionCommand,
        context: &ToolExecutionContext,
    ) -> Result<ToolExecutionPermit, String>;
}

/// Default preparation for native and MCP adapters. The service keeps readiness
/// as an explicit auditable stage without claiming that it provides isolation.
#[derive(Debug, Default)]
pub struct AdapterOwnedPreparation;

#[async_trait]
impl ToolExecutionPreparation for AdapterOwnedPreparation {
    async fn prepare(
        &self,
        _descriptor: &ToolDescriptor,
        _command: &ToolExecutionCommand,
        _context: &ToolExecutionContext,
    ) -> Result<ToolExecutionPermit, String> {
        Ok(ToolExecutionPermit {
            strategy: "adapter_owned".to_string(),
        })
    }
}

/// Terminal service outcome classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionStatus {
    Succeeded,
    Failed,
    Denied,
    ApprovalDenied,
    AwaitingApproval,
    PreparationDenied,
    UnknownTool,
    StaleCapability,
    InvalidArguments,
    Cancelled,
    IdempotencyConflict,
    Indeterminate,
}

/// Stable typed outcome shared by entrypoint-specific model/UI projections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionOutcome {
    pub operation_id: String,
    pub descriptor: Option<ToolDescriptor>,
    pub decision: Option<ToolExecutionDecision>,
    pub status: ToolExecutionStatus,
    pub model_content: String,
    pub result: Option<ToolResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<Box<super::traits::ProcessExecutionDetails>>,
    pub error: Option<String>,
    pub preparation: Option<ToolExecutionPermit>,
    pub duration_ms: u64,
    #[serde(default)]
    pub replayed: bool,
}

impl ToolExecutionOutcome {
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.status == ToolExecutionStatus::Succeeded
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolExecutionReservation {
    operation_id: String,
    #[serde(default, alias = "owner_token")]
    reservation_id: String,
    #[serde(default)]
    attempt: u32,
    capability: String,
    input_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolExecutionTerminalRecord {
    reservation: ToolExecutionReservation,
    outcome: ToolExecutionOutcome,
}

fn tool_execution_lock(
    workspace_id: &str,
    session_key: &str,
    semantic_turn_id: &str,
    idempotency_key: &str,
) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<AsyncMutex<()>>>>> = OnceLock::new();
    let lock_key = format!("{workspace_id}\0{session_key}\0{semantic_turn_id}\0{idempotency_key}");
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock();
    if let Some(lock) = locks.get(&lock_key).and_then(Weak::upgrade) {
        return lock;
    }
    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = Arc::new(AsyncMutex::new(()));
    locks.insert(lock_key, Arc::downgrade(&lock));
    lock
}

fn tool_execution_ledger_keys(context: &ToolExecutionContext, idempotency_key: &str) -> (String, String) {
    let scope = format!(
        "{}\0{}\0{}",
        context.envelope.session_key, context.semantic_turn_id, idempotency_key
    );
    let digest = format!("{:x}", Sha256::digest(scope.as_bytes()));
    (
        format!("tool-execution:{digest}:reservation"),
        format!("tool-execution:{digest}:terminal"),
    )
}

fn tool_execution_reservation_key(base_key: &str, attempt: u32) -> String {
    if attempt == 0 {
        base_key.to_string()
    } else {
        format!("{base_key}:retry:{attempt}")
    }
}

fn tool_execution_abandonment_key(base_key: &str, attempt: u32) -> String {
    format!("{}:abandoned", tool_execution_reservation_key(base_key, attempt))
}

/// Audit projection emitted exactly once for every service outcome, including
/// resolution, policy, approval, preparation, validation, and cancellation failures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExecutionAuditRecord {
    pub operation_id: String,
    pub capability: String,
    pub backend_name: Option<String>,
    pub adapter: Option<ToolAdapterKind>,
    pub effect: Option<ToolEffect>,
    pub decision: Option<ToolExecutionDecision>,
    pub status: ToolExecutionStatus,
    pub preparation_strategy: Option<String>,
    pub input_sha256: String,
    pub source: String,
    pub workspace_id: String,
    pub session_key: String,
    pub run_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub task_id: Option<String>,
    pub error: Option<String>,
    pub duration_ms: u64,
}

/// Audit is mandatory but observational: sinks cannot rewrite the result or
/// fail a completed side effect.
pub trait ToolExecutionAuditSink: Send + Sync {
    fn record(&self, record: ToolExecutionAuditRecord);
}

/// Structured tracing sink used when no durable integration-event writer has
/// yet been supplied by the entrypoint.
#[derive(Debug, Default)]
pub struct TracingToolExecutionAudit;

impl ToolExecutionAuditSink for TracingToolExecutionAudit {
    fn record(&self, record: ToolExecutionAuditRecord) {
        tracing::info!(
            target: "tool_execution_audit",
            operation_id = %record.operation_id,
            capability = %record.capability,
            backend = ?record.backend_name,
            adapter = ?record.adapter,
            effect = ?record.effect,
            decision = ?record.decision,
            status = ?record.status,
            preparation = ?record.preparation_strategy,
            source = %record.source,
            workspace_id = %record.workspace_id,
            session_key = %record.session_key,
            run_id = ?record.run_id,
            parent_run_id = ?record.parent_run_id,
            task_id = ?record.task_id,
            duration_ms = record.duration_ms,
            error = ?record.error,
            "tool execution audited"
        );
    }
}

struct ResolvedTool {
    shared_binding_digest: SharedBinding,
    backend: Arc<dyn ToolBackend>,
    descriptor: ToolDescriptor,
}

struct ToolExecutionLease {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    memory: Arc<dyn Memory>,
    reservation: ToolExecutionReservation,
    abandonment_key: String,
    terminal_key: String,
}

/// Immutable descriptor snapshot shared by discovery and execution.
///
/// The catalog is assembled from the exact finalized backend registry received
/// by an entry point. It never infers executable readiness from configuration.
#[derive(Debug, Clone)]
pub struct ToolCatalog {
    descriptors: Arc<[ToolDescriptor]>,
}

impl ToolCatalog {
    fn from_backends(backends: &[Arc<dyn ToolBackend>]) -> Self {
        let descriptors = backends
            .iter()
            .flat_map(|backend| {
                backend.specs().into_iter().map(|spec| {
                    tool_descriptor(
                        backend.root_name(),
                        spec,
                        backend.tier(),
                        backend.categories(),
                        |public_name| backend.adapter_kind(public_name),
                        backend.availability(),
                        backend.process_worker_compatible(),
                    )
                    .with_execution_parameters(backend.execution_parameters_schema())
                })
            })
            .collect();
        Self::from_descriptors(descriptors)
    }

    /// Snapshot a legacy boxed registry for user-facing discovery. This uses
    /// the same descriptor construction as [`ToolExecutionService`].
    #[must_use]
    pub fn from_boxed_registry(registry: &[Box<dyn Tool>]) -> Self {
        Self::from_tools(registry.iter().map(Box::as_ref))
    }

    /// Snapshot any selected set of legacy tools. Entry-point-specific tiering
    /// may choose a subset, but descriptor and availability semantics remain
    /// identical everywhere.
    #[must_use]
    pub fn from_tools<'a>(tools: impl IntoIterator<Item = &'a dyn Tool>) -> Self {
        let descriptors = tools
            .into_iter()
            .flat_map(|tool| {
                tool.specs().into_iter().map(|spec| {
                    let root_name = tool.name();
                    tool_descriptor(
                        root_name,
                        spec,
                        tool.tier(),
                        tool.categories().to_vec(),
                        |public_name| {
                            if root_name == "mcp_call" && public_name != root_name {
                                ToolAdapterKind::McpAlias
                            } else if root_name == "wasm_plugin_call" {
                                ToolAdapterKind::WasmPlugin
                            } else if root_name == "skill_execute" {
                                ToolAdapterKind::Skill
                            } else {
                                ToolAdapterKind::Native
                            }
                        },
                        tool.availability(),
                        tool.process_worker_compatible(),
                    )
                    .with_execution_parameters(tool.execution_parameters_schema())
                })
            })
            .collect();
        Self::from_descriptors(descriptors)
    }

    fn from_descriptors(descriptors: Vec<ToolDescriptor>) -> Self {
        // Preserve registry/spec order so adopting the catalog does not reorder
        // provider prompts. The first executable registration owns a duplicate
        // public name, matching execution resolution's first-match semantics.
        let mut unique = Vec::with_capacity(descriptors.len());
        for descriptor in descriptors {
            if !unique
                .iter()
                .any(|registered: &ToolDescriptor| registered.public_name == descriptor.public_name)
            {
                unique.push(descriptor);
            }
        }
        Self {
            descriptors: unique.into(),
        }
    }

    #[must_use]
    pub fn descriptors(&self) -> &[ToolDescriptor] {
        &self.descriptors
    }

    /// Provider-facing projection of the canonical descriptors.
    #[must_use]
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.descriptors
            .iter()
            .filter(|descriptor| {
                descriptor.availability.level >= crate::capability::CapabilityAvailabilityLevel::Configured
            })
            .map(|descriptor| ToolSpec {
                name: descriptor.public_name.clone(),
                description: descriptor.description.clone(),
                parameters: descriptor.parameters.clone(),
            })
            .collect()
    }

    #[must_use]
    pub fn descriptor(&self, public_name: &str) -> Option<&ToolDescriptor> {
        self.descriptors
            .iter()
            .find(|descriptor| descriptor.public_name == public_name)
    }
}

fn tool_descriptor(
    backend_name: &str,
    spec: ToolSpec,
    tier: ToolTier,
    categories: Vec<ToolCategory>,
    adapter_kind: impl FnOnce(&str) -> ToolAdapterKind,
    availability: CapabilityAvailability,
    process_worker_compatible: bool,
) -> ToolDescriptor {
    let adapter = adapter_kind(&spec.name);
    let effect = if backend_name == "skill_read" || crate::security::policy::is_read_only_tool(&spec.name) {
        ToolEffect::Read
    } else {
        ToolEffect::Act
    };
    let adapter_label = match adapter {
        ToolAdapterKind::Native => "native",
        ToolAdapterKind::McpAlias => "MCP alias",
        ToolAdapterKind::WasmPlugin => "WASM plugin",
        ToolAdapterKind::Skill => "skill",
    };
    let availability = match crate::tools::schema::SchemaCleanr::validate(&spec.parameters) {
        Ok(()) => availability,
        Err(error) => CapabilityAvailability::declared(format!(
            "invalid schema for {adapter_label} tool '{}': {error}",
            spec.name
        )),
    };
    ToolDescriptor {
        public_name: spec.name,
        backend_name: backend_name.to_string(),
        description: spec.description,
        parameters: spec.parameters,
        execution_parameters: None,
        tier,
        categories,
        effect,
        adapter,
        process_worker_compatible,
        availability: if availability.reason.trim().is_empty() {
            CapabilityAvailability::ready(format!(
                "executable {adapter_label} backend '{backend_name}' is registered"
            ))
        } else {
            availability
        },
    }
}

/// Mandatory application pipeline for native tools and MCP aliases.
pub struct ToolExecutionService {
    backends: Arc<[Arc<dyn ToolBackend>]>,
    catalog: ToolCatalog,
    policy: Arc<dyn EffectPolicy>,
    approval: Arc<dyn ApprovalStrategy>,
    preparation: Arc<dyn ToolExecutionPreparation>,
    audit: Arc<dyn ToolExecutionAuditSink>,
    idempotency_memory: Option<Arc<dyn Memory>>,
    #[cfg(test)]
    post_reservation_gate: Option<Arc<TestPostReservationGate>>,
}

#[cfg(test)]
struct TestPostReservationGate {
    armed: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl ToolExecutionService {
    #[must_use]
    pub fn new(
        tools: Vec<Arc<dyn Tool>>,
        policy: Arc<dyn EffectPolicy>,
        approval: Arc<dyn ApprovalStrategy>,
        preparation: Arc<dyn ToolExecutionPreparation>,
        audit: Arc<dyn ToolExecutionAuditSink>,
    ) -> Self {
        let backends = tools
            .into_iter()
            .map(|tool| Arc::new(LegacyToolAdapter::new(tool)) as Arc<dyn ToolBackend>)
            .collect();
        Self::from_backends(backends, policy, approval, preparation, audit)
    }

    /// Assemble the service from raw execution ports.
    #[must_use]
    pub fn from_backends(
        backends: Vec<Arc<dyn ToolBackend>>,
        policy: Arc<dyn EffectPolicy>,
        approval: Arc<dyn ApprovalStrategy>,
        preparation: Arc<dyn ToolExecutionPreparation>,
        audit: Arc<dyn ToolExecutionAuditSink>,
    ) -> Self {
        let catalog = ToolCatalog::from_backends(&backends);
        Self {
            backends: backends.into(),
            catalog,
            policy,
            approval,
            preparation,
            audit,
            idempotency_memory: None,
            #[cfg(test)]
            post_reservation_gate: None,
        }
    }

    /// Consume the current boxed registry without cloning tool state.
    #[must_use]
    pub fn from_boxed(
        tools: Vec<Box<dyn Tool>>,
        policy: Arc<dyn EffectPolicy>,
        approval: Arc<dyn ApprovalStrategy>,
        preparation: Arc<dyn ToolExecutionPreparation>,
        audit: Arc<dyn ToolExecutionAuditSink>,
    ) -> Self {
        let tools = tools.into_iter().map(Arc::<dyn Tool>::from).collect();
        Self::new(tools, policy, approval, preparation, audit)
    }

    /// Adapt a shared boxed registry without cloning or consuming stateful
    /// tools. This is the compatibility assembly used by Chat Redux.
    #[must_use]
    pub fn from_shared_boxed_registry(
        registry: Arc<Vec<Box<dyn Tool>>>,
        policy: Arc<dyn EffectPolicy>,
        approval: Arc<dyn ApprovalStrategy>,
        preparation: Arc<dyn ToolExecutionPreparation>,
        audit: Arc<dyn ToolExecutionAuditSink>,
    ) -> Self {
        let backends = registry
            .iter()
            .map(|tool| tool.name().to_string())
            .map(|root_name| {
                Arc::new(SharedRegistryToolAdapter {
                    registry: Arc::clone(&registry),
                    root_name,
                }) as Arc<dyn ToolBackend>
            })
            .collect();
        Self::from_backends(backends, policy, approval, preparation, audit)
    }

    /// Fail-closed convenience assembly for non-interactive callers.
    #[must_use]
    pub fn with_security_policy(tools: Vec<Box<dyn Tool>>, policy: Arc<SecurityPolicy>) -> Self {
        Self::from_boxed(
            tools,
            Arc::new(SecurityEffectPolicy::new(policy)),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        )
    }

    /// Attach the durable MessageEvent ledger used to reserve and replay
    /// idempotent tool executions.
    ///
    /// Callers resolve the ledger through `crate::memory::tool_execution_ledger`,
    /// which already guarantees a ledger-capable backend. The check here stays as
    /// a last line of defence: a backend without a durable event log would
    /// otherwise record reservations that never survive a crash.
    #[must_use]
    pub fn with_idempotency_memory(mut self, memory: Arc<dyn Memory>) -> Self {
        if crate::memory::serves_tool_execution_ledger(memory.name()) {
            self.idempotency_memory = Some(memory);
        } else {
            tracing::warn!(
                backend = memory.name(),
                "tool execution idempotency is unavailable for this memory backend"
            );
        }
        self
    }

    #[cfg(test)]
    fn with_post_reservation_gate(mut self, gate: Arc<TestPostReservationGate>) -> Self {
        self.post_reservation_gate = Some(gate);
        self
    }

    fn resolve(&self, public_name: &str) -> Option<ResolvedTool> {
        // Always inspect the current exact spec. A startup descriptor can be
        // stale after MCP, WASM or Skill discovery changes its schema, and a
        // live MCP catalog can be larger than the provider-facing spec window.
        let pinned_root = self
            .catalog
            .descriptor(public_name)
            .map(|entry| entry.backend_name.as_str());
        let mut resolved = None;
        for backend in self.backends.iter() {
            if pinned_root.is_some_and(|root| root != backend.root_name()) {
                continue;
            }
            if !backend.supports_name(public_name) {
                continue;
            }
            let (spec, shared_binding_digest) = if let Some(snapshot) = backend.shared_catalog_snapshot() {
                let Some(entry) = snapshot.entry(public_name) else {
                    continue;
                };
                (entry.spec().clone(), SharedBinding::Snapshot(entry.binding_digest()))
            } else {
                let Some(spec) = backend.spec_for_name(public_name) else {
                    continue;
                };
                (spec, SharedBinding::Legacy)
            };
            let descriptor = tool_descriptor(
                backend.root_name(),
                spec,
                backend.tier(),
                backend.categories(),
                |name| backend.adapter_kind(name),
                backend.availability(),
                backend.process_worker_compatible(),
            )
            .with_execution_parameters(backend.execution_parameters_schema());
            if resolved.is_some() {
                // A dynamic name with more than one live owner is ambiguous.
                return None;
            }
            resolved = Some(ResolvedTool {
                shared_binding_digest,
                backend: Arc::clone(backend),
                descriptor,
            });
        }
        resolved
    }

    /// Descriptor for the exact capability the execution service can run now.
    #[must_use]
    pub fn current_descriptor(&self, public_name: &str) -> Option<ToolDescriptor> {
        self.resolve(public_name).map(|resolved| resolved.descriptor)
    }

    pub(crate) fn current_descriptor_with_binding(
        &self,
        name: &str,
        require_binding: bool,
    ) -> Option<(ToolDescriptor, Option<[u8; 32]>)> {
        let resolved = self.resolve(name)?;
        let binding = resolved.shared_binding_digest.or_else_live(|| {
            require_binding
                .then(|| resolved.backend.catalog_binding_snapshot().get(name).copied())
                .flatten()
        });
        Some((resolved.descriptor, binding))
    }

    pub(crate) fn current_binding_digest(&self, root: &str, name: &str) -> Option<[u8; 32]> {
        let owner = self.catalog.descriptor(root)?.backend_name.as_str();
        let backend = self.backends.iter().find(|backend| backend.root_name() == owner)?;
        backend.shared_catalog_snapshot().map_or_else(
            || backend.catalog_binding_snapshot().get(name).copied(),
            |snapshot| snapshot.binding_digest(name),
        )
    }

    pub(crate) fn supports_owner_filesystem(&self, public_name: &str) -> bool {
        self.resolve(public_name).is_some_and(|resolved| {
            resolved.descriptor.adapter == ToolAdapterKind::Native
                && resolved.descriptor.backend_name == public_name
                && matches!(public_name, "file_read" | "file_write" | "file_edit")
                && resolved.backend.supports_owner_filesystem()
        })
    }

    pub(crate) async fn resolve_approval_operation(
        &self,
        public_name: &str,
        arguments: &serde_json::Value,
    ) -> anyhow::Result<(String, crate::acl::approval_grant::RiskLevel)> {
        self.resolve(public_name)
            .ok_or_else(|| anyhow::anyhow!("approval backend unavailable"))?
            .backend
            .resolve_approval_operation(public_name, arguments)
            .await?
            .ok_or_else(|| anyhow::anyhow!("approval operation cannot be resolved exactly"))
    }

    /// Full live inventory. A collision with a pinned startup name is
    /// reported by parity even if the service still routes to its first owner.
    #[must_use]
    pub fn current_descriptors(&self) -> Vec<ToolDescriptor> {
        self.backends
            .iter()
            .flat_map(|backend| {
                backend.catalog_specs().into_iter().map(|spec| {
                    tool_descriptor(
                        backend.root_name(),
                        spec,
                        backend.tier(),
                        backend.categories(),
                        |name| backend.adapter_kind(name),
                        backend.availability(),
                        backend.process_worker_compatible(),
                    )
                    .with_execution_parameters(backend.execution_parameters_schema())
                })
            })
            .collect()
    }

    /// Full fresh inventory for causal parity without building user-facing
    /// descriptors. A malformed schema has the same Declared status as the
    /// descriptor path and therefore cannot satisfy a pinned configured entry.
    #[must_use]
    pub(crate) fn current_specs_for_parity(&self) -> Vec<CurrentToolSpec<'_>> {
        // Capture each dynamic publication once before allocating the flat view.
        // Startup descriptors may contain only a dynamic backend's root.
        let snapshots: Vec<_> = self
            .backends
            .iter()
            .map(|backend| backend.shared_catalog_snapshot())
            .collect();
        let capacity = self.catalog.descriptors().len()
            + snapshots
                .iter()
                .flatten()
                .map(|snapshot| snapshot.entries().len())
                .sum::<usize>();
        let mut live = Vec::with_capacity(capacity);
        for (backend, snapshot) in self.backends.iter().zip(snapshots) {
            let backend = backend.as_ref();
            if let Some(snapshot) = snapshot {
                let configured = backend.catalog_availability_is_configured();
                for entry in snapshot.entries() {
                    live.push(CurrentToolSpec {
                        backend_name: backend.root_name(),
                        configured: configured && entry.schema_valid(),
                        spec: CurrentToolSpecValue::Shared {
                            entry: Arc::clone(entry),
                            snapshot: snapshot.clone(),
                        },
                    });
                }
            } else if let Some(specs) = backend.immutable_catalog_specs() {
                for spec in specs {
                    live.push(current_tool_spec(backend, std::borrow::Cow::Borrowed(spec)));
                }
            } else {
                for spec in backend.catalog_specs() {
                    live.push(current_tool_spec(backend, std::borrow::Cow::Owned(spec)));
                }
            }
        }
        live
    }

    #[must_use]
    pub fn current_binding_snapshot(&self, root: &str) -> Option<std::collections::BTreeMap<String, [u8; 32]>> {
        let owner = self.catalog.descriptor(root)?.backend_name.as_str();
        Some(
            self.backends
                .iter()
                .find(|backend| backend.root_name() == owner)?
                .catalog_binding_snapshot(),
        )
    }

    /// Return whether an exact public tool name can currently be resolved.
    ///
    /// This includes live dynamic aliases that were discovered after the
    /// immutable startup catalog was captured. Turn-level exposure guards use
    /// this distinction to reject a known-but-unexposed capability without
    /// changing the existing error for a genuinely unknown tool name.
    #[must_use]
    pub fn supports_public_name(&self, public_name: &str) -> bool {
        self.resolve(public_name).is_some()
    }

    /// Return the normal execution-policy denial for a known capability that
    /// was withheld from this turn. This performs no binding, approval,
    /// preparation, reservation, or backend work.
    #[must_use]
    pub fn unexposed_policy_denial(&self, public_name: &str, context: &ToolExecutionContext) -> Option<String> {
        let descriptor = self.resolve(public_name)?.descriptor;
        (self.policy.decide(&descriptor, context) == ToolExecutionDecision::Deny).then(|| {
            format!(
                "tool '{}' is not permitted under the current execution policy",
                descriptor.public_name
            )
        })
    }

    #[must_use]
    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.catalog.descriptors().to_vec()
    }

    #[must_use]
    pub const fn catalog(&self) -> &ToolCatalog {
        &self.catalog
    }

    async fn prepare_idempotent_execution(
        &self,
        command: &ToolExecutionCommand,
        context: &ToolExecutionContext,
        input_sha256: &str,
        descriptor: &ToolDescriptor,
        decision: ToolExecutionDecision,
        preparation: &ToolExecutionPermit,
        started: Instant,
    ) -> Result<Option<ToolExecutionLease>, Box<ToolExecutionOutcome>> {
        let Some(idempotency_key) = command.idempotency_key.as_deref() else {
            return Ok(None);
        };
        let Some(memory) = self.idempotency_memory.as_ref() else {
            if descriptor.effect == ToolEffect::Read {
                return Ok(None);
            }
            return Err(Box::new(self.finish(
                command,
                context,
                input_sha256.to_string(),
                Some(descriptor.clone()),
                Some(decision),
                ToolExecutionStatus::Indeterminate,
                "Error: durable tool idempotency ledger is unavailable; refusing side effect".to_string(),
                None,
                Some("durable tool idempotency ledger is unavailable".to_string()),
                Some(preparation.clone()),
                started,
            )));
        };
        let lock = tool_execution_lock(
            &context.envelope.workspace_id,
            &context.envelope.session_key,
            &context.semantic_turn_id,
            idempotency_key,
        );
        let guard = lock.lock_owned().await;
        let (base_reservation_key, terminal_key) = tool_execution_ledger_keys(context, idempotency_key);

        match memory
            .find_message_event_by_idempotency_key(&context.envelope.workspace_id, &terminal_key)
            .await
        {
            Ok(Some(event)) => {
                let terminal = event
                    .raw_payload_json
                    .as_deref()
                    .and_then(|payload| serde_json::from_str::<ToolExecutionTerminalRecord>(payload).ok());
                let Some(terminal) = terminal else {
                    return Err(Box::new(self.finish(
                        command,
                        context,
                        input_sha256.to_string(),
                        Some(descriptor.clone()),
                        Some(decision),
                        ToolExecutionStatus::Indeterminate,
                        "Error: persisted tool terminal record is unreadable; refusing replay".to_string(),
                        None,
                        Some("persisted tool terminal record is unreadable".to_string()),
                        Some(preparation.clone()),
                        started,
                    )));
                };
                if terminal.reservation.capability != command.name || terminal.reservation.input_sha256 != input_sha256
                {
                    return Err(Box::new(self.finish(
                        command,
                        context,
                        input_sha256.to_string(),
                        Some(descriptor.clone()),
                        Some(decision),
                        ToolExecutionStatus::IdempotencyConflict,
                        "Error: idempotency key was already used for a different tool command".to_string(),
                        None,
                        Some("idempotency key command fingerprint mismatch".to_string()),
                        Some(preparation.clone()),
                        started,
                    )));
                }
                return Err(Box::new(self.replay_outcome(
                    command,
                    context,
                    input_sha256,
                    terminal.outcome,
                )));
            }
            Ok(None) => {}
            Err(error) => {
                return Err(Box::new(self.finish(
                    command,
                    context,
                    input_sha256.to_string(),
                    Some(descriptor.clone()),
                    Some(decision),
                    ToolExecutionStatus::Indeterminate,
                    format!("Error: unable to inspect tool idempotency ledger: {error}"),
                    None,
                    Some(error.to_string()),
                    Some(preparation.clone()),
                    started,
                )));
            }
        }

        let mut attempt = 0_u32;
        loop {
            let reservation_key = tool_execution_reservation_key(&base_reservation_key, attempt);
            match memory
                .find_message_event_by_idempotency_key(&context.envelope.workspace_id, &reservation_key)
                .await
            {
                Ok(Some(event)) => {
                    let existing = event
                        .raw_payload_json
                        .as_deref()
                        .and_then(|payload| serde_json::from_str::<ToolExecutionReservation>(payload).ok());
                    let Some(existing) = existing else {
                        return Err(Box::new(
                            self.finish(
                                command,
                                context,
                                input_sha256.to_string(),
                                Some(descriptor.clone()),
                                Some(decision),
                                ToolExecutionStatus::Indeterminate,
                                "Error: persisted tool reservation is unreadable; refusing duplicate side effect"
                                    .to_string(),
                                None,
                                Some("persisted tool reservation is unreadable".to_string()),
                                Some(preparation.clone()),
                                started,
                            ),
                        ));
                    };
                    if existing.capability != command.name || existing.input_sha256 != input_sha256 {
                        return Err(Box::new(self.finish(
                            command,
                            context,
                            input_sha256.to_string(),
                            Some(descriptor.clone()),
                            Some(decision),
                            ToolExecutionStatus::IdempotencyConflict,
                            "Error: idempotency key was already used for a different tool command".to_string(),
                            None,
                            Some("idempotency key command fingerprint mismatch".to_string()),
                            Some(preparation.clone()),
                            started,
                        )));
                    }
                    let abandonment_key = tool_execution_abandonment_key(&base_reservation_key, attempt);
                    let abandoned = memory
                        .find_message_event_by_idempotency_key(&context.envelope.workspace_id, &abandonment_key)
                        .await
                        .map_err(|error| {
                            self.finish(
                                command,
                                context,
                                input_sha256.to_string(),
                                Some(descriptor.clone()),
                                Some(decision),
                                ToolExecutionStatus::Indeterminate,
                                format!("Error: unable to inspect tool idempotency abandonment: {error}"),
                                None,
                                Some(error.to_string()),
                                Some(preparation.clone()),
                                started,
                            )
                        })?;
                    let Some(abandoned) = abandoned else {
                        return Err(Box::new(self.finish(
                            command,
                            context,
                            input_sha256.to_string(),
                            Some(descriptor.clone()),
                            Some(decision),
                            ToolExecutionStatus::Indeterminate,
                            "Error: tool execution was reserved without a terminal outcome; refusing duplicate side effect"
                                .to_string(),
                            None,
                            Some(
                                "tool execution was reserved without a terminal outcome; refusing duplicate side effect"
                                    .to_string(),
                            ),
                            Some(preparation.clone()),
                            started,
                        )));
                    };
                    let abandoned = abandoned
                        .raw_payload_json
                        .as_deref()
                        .and_then(|payload| serde_json::from_str::<ToolExecutionReservation>(payload).ok());
                    if abandoned.as_ref().is_none_or(|abandoned| {
                        abandoned.reservation_id != existing.reservation_id
                            || abandoned.attempt != existing.attempt
                            || abandoned.capability != existing.capability
                            || abandoned.input_sha256 != existing.input_sha256
                    }) {
                        return Err(Box::new(
                            self.finish(
                                command,
                                context,
                                input_sha256.to_string(),
                                Some(descriptor.clone()),
                                Some(decision),
                                ToolExecutionStatus::Indeterminate,
                                "Error: persisted tool abandonment is unreadable or does not own the reservation"
                                    .to_string(),
                                None,
                                Some("persisted tool abandonment does not match reservation owner".to_string()),
                                Some(preparation.clone()),
                                started,
                            ),
                        ));
                    }
                    attempt = attempt.checked_add(1).ok_or_else(|| {
                        self.finish(
                            command,
                            context,
                            input_sha256.to_string(),
                            Some(descriptor.clone()),
                            Some(decision),
                            ToolExecutionStatus::Indeterminate,
                            "Error: tool idempotency retry generation overflow".to_string(),
                            None,
                            Some("tool idempotency retry generation overflow".to_string()),
                            Some(preparation.clone()),
                            started,
                        )
                    })?;
                }
                Ok(None) => {
                    let reservation = ToolExecutionReservation {
                        operation_id: command.operation_id.clone(),
                        reservation_id: Uuid::now_v7().to_string(),
                        attempt,
                        capability: command.name.clone(),
                        input_sha256: input_sha256.to_string(),
                    };
                    let payload = serde_json::to_string(&reservation).map_err(|error| {
                        self.finish(
                            command,
                            context,
                            input_sha256.to_string(),
                            Some(descriptor.clone()),
                            Some(decision),
                            ToolExecutionStatus::Indeterminate,
                            format!("Error: unable to encode tool idempotency reservation: {error}"),
                            None,
                            Some(error.to_string()),
                            Some(preparation.clone()),
                            started,
                        )
                    })?;
                    let event = memory
                        .append_message_event(tool_execution_event_input(
                            context,
                            &reservation_key,
                            "tool.execution.reserved",
                            format!(
                                "operation_id={} capability={} attempt={attempt}",
                                command.operation_id, command.name
                            ),
                            payload,
                        ))
                        .await
                        .map_err(|error| {
                            self.finish(
                                command,
                                context,
                                input_sha256.to_string(),
                                Some(descriptor.clone()),
                                Some(decision),
                                ToolExecutionStatus::Indeterminate,
                                format!("Error: unable to reserve idempotent tool execution: {error}"),
                                None,
                                Some(error.to_string()),
                                Some(preparation.clone()),
                                started,
                            )
                        })?;
                    // The reservation is a durable write the turn owed, and it
                    // sits between `ToolStart` and the backend's first poll
                    // with nothing else to show for it. Recorded now that it
                    // has come back, so a turn whose ledger is merely slow is
                    // not read as a turn that went silent — and so the hang
                    // report, when one is warranted, names where the turn
                    // actually was.
                    crate::agent::idle::beat(crate::agent::idle::ProgressKind::DurableWrite);
                    let recorded = event
                        .raw_payload_json
                        .as_deref()
                        .and_then(|payload| serde_json::from_str::<ToolExecutionReservation>(payload).ok());
                    if recorded
                        .as_ref()
                        .is_none_or(|recorded| recorded.reservation_id != reservation.reservation_id)
                    {
                        return Err(Box::new(self.finish(
                            command,
                            context,
                            input_sha256.to_string(),
                            Some(descriptor.clone()),
                            Some(decision),
                            ToolExecutionStatus::Indeterminate,
                            "Error: another runtime owns this idempotent tool execution".to_string(),
                            None,
                            Some("another runtime owns this idempotent tool execution".to_string()),
                            Some(preparation.clone()),
                            started,
                        )));
                    }
                    return Ok(Some(ToolExecutionLease {
                        _guard: guard,
                        memory: Arc::clone(memory),
                        abandonment_key: tool_execution_abandonment_key(&base_reservation_key, attempt),
                        reservation,
                        terminal_key,
                    }));
                }
                Err(error) => {
                    return Err(Box::new(self.finish(
                        command,
                        context,
                        input_sha256.to_string(),
                        Some(descriptor.clone()),
                        Some(decision),
                        ToolExecutionStatus::Indeterminate,
                        format!("Error: unable to inspect tool idempotency reservation: {error}"),
                        None,
                        Some(error.to_string()),
                        Some(preparation.clone()),
                        started,
                    )));
                }
            }
        }
    }

    async fn commit_idempotent_execution(
        &self,
        lease: Option<&ToolExecutionLease>,
        context: &ToolExecutionContext,
        outcome: &ToolExecutionOutcome,
    ) {
        let Some(lease) = lease else {
            return;
        };
        let terminal = ToolExecutionTerminalRecord {
            reservation: lease.reservation.clone(),
            outcome: outcome.clone(),
        };
        let payload = match serde_json::to_value(&terminal)
            .map(crate::agent::terminal::redact_persisted_json)
            .and_then(|value| serde_json::to_string(&value))
        {
            Ok(payload) => payload,
            Err(error) => {
                tracing::error!(error = %error, "failed to encode idempotent tool terminal outcome");
                return;
            }
        };
        let appended = lease
            .memory
            .append_message_event(tool_execution_event_input(
                context,
                &lease.terminal_key,
                "tool.execution.finalized",
                format!("operation_id={} status={:?}", outcome.operation_id, outcome.status),
                payload,
            ))
            .await;
        // Same reasoning as the reservation write above, at the other end of
        // the call: this is the last thing the turn does before the loop can
        // announce the result, and until now it was silent.
        crate::agent::idle::beat(crate::agent::idle::ProgressKind::DurableWrite);
        if let Err(error) = appended {
            tracing::error!(
                operation_id = %outcome.operation_id,
                error = %error,
                "tool side effect completed but terminal idempotency outcome was not persisted"
            );
        }
    }

    async fn abandon_idempotent_execution(
        &self,
        lease: Option<&ToolExecutionLease>,
        context: &ToolExecutionContext,
    ) -> Result<(), String> {
        let Some(lease) = lease else {
            return Ok(());
        };
        let payload = serde_json::to_string(&lease.reservation)
            .map_err(|error| format!("unable to encode tool idempotency abandonment: {error}"))?;
        let event = lease
            .memory
            .append_message_event(tool_execution_event_input(
                context,
                &lease.abandonment_key,
                "tool.execution.abandoned",
                format!(
                    "operation_id={} attempt={}",
                    lease.reservation.operation_id, lease.reservation.attempt
                ),
                payload,
            ))
            .await
            .map_err(|error| format!("unable to abandon tool idempotency reservation: {error}"))?;
        let recorded = event
            .raw_payload_json
            .as_deref()
            .and_then(|payload| serde_json::from_str::<ToolExecutionReservation>(payload).ok());
        if recorded.as_ref().is_none_or(|recorded| {
            recorded.reservation_id != lease.reservation.reservation_id
                || recorded.attempt != lease.reservation.attempt
                || recorded.capability != lease.reservation.capability
                || recorded.input_sha256 != lease.reservation.input_sha256
        }) {
            return Err("another runtime owns the tool idempotency abandonment".to_string());
        }
        Ok(())
    }

    fn replay_outcome(
        &self,
        command: &ToolExecutionCommand,
        context: &ToolExecutionContext,
        input_sha256: &str,
        mut outcome: ToolExecutionOutcome,
    ) -> ToolExecutionOutcome {
        outcome.operation_id = command.operation_id.clone();
        outcome.replayed = true;
        self.audit.record(ToolExecutionAuditRecord {
            operation_id: command.operation_id.clone(),
            capability: command.name.clone(),
            backend_name: outcome.descriptor.as_ref().map(|value| value.backend_name.clone()),
            adapter: outcome.descriptor.as_ref().map(|value| value.adapter),
            effect: outcome.descriptor.as_ref().map(|value| value.effect),
            decision: outcome.decision,
            status: outcome.status,
            preparation_strategy: outcome.preparation.as_ref().map(|value| value.strategy.clone()),
            input_sha256: input_sha256.to_string(),
            source: context.envelope.source.as_str().to_string(),
            workspace_id: context.envelope.workspace_id.clone(),
            session_key: context.envelope.session_key.clone(),
            run_id: context.envelope.run_id.clone(),
            parent_run_id: context.envelope.parent_run_id.clone(),
            task_id: context.envelope.task_id.clone(),
            error: outcome.error.clone(),
            duration_ms: 0,
        });
        outcome
    }

    /// Run the fixed descriptor -> effect -> policy -> approval -> preparation ->
    /// execute -> audit pipeline and return a typed terminal outcome.
    pub async fn execute(
        &self,
        command: ToolExecutionCommand,
        context: ToolExecutionContext,
        cancellation: Option<CancellationToken>,
    ) -> ToolExecutionOutcome {
        let pending = context.execution_trace.as_ref().map(|trace| {
            let binding = context
                .execution_bindings
                .as_ref()
                .and_then(|bindings| bindings.get(&command.name));
            trace.begin(
                &command.operation_id,
                binding.map(|binding| binding.capability_id()),
                binding.is_some_and(|binding| binding.is_selected()),
            )
        });
        // `execute_inner` owns the whole descriptor, policy, approval,
        // preparation and backend pipeline. Its state machine is large enough
        // that embedding it in each caller's async frame can exhaust a default
        // tokio worker stack when a tool starts another agent loop. Keep the
        // canonical pipeline on the heap so every caller carries only a pointer
        // while it is pending.
        let mut outcome = Box::pin(self.execute_inner(command, context, cancellation)).await;
        if let Some(stage) = crate::runtime::experiment_stage::current() {
            if let Err(error) = stage.account_output(outcome.model_content.len()) {
                outcome.status = ToolExecutionStatus::Failed;
                outcome.error = Some(error.to_string());
                outcome.model_content = error.to_string();
            }
        }
        if let Some(pending) = pending {
            pending.finish_with_error(outcome.status, outcome.error.as_deref());
        }
        outcome
    }

    async fn execute_inner(
        &self,
        mut command: ToolExecutionCommand,
        context: ToolExecutionContext,
        cancellation: Option<CancellationToken>,
    ) -> ToolExecutionOutcome {
        let started = Instant::now();
        // Every production tool call passes through here, which makes this the
        // one place that has to register for the runtime work registry to be
        // complete. The call gets its own kill handle, derived from the turn
        // token so turn-level cancellation still reaches it, but cancellable on
        // its own so an operator can end one stuck call without taking the
        // whole turn down with it.
        let cancellation = Some(
            cancellation
                .as_ref()
                .map_or_else(CancellationToken::new, CancellationToken::child_token),
        );
        let work = crate::runtime::registry::register_tool_call(
            &command.name,
            context.envelope.task_id.as_deref(),
            cancellation.clone(),
        );
        // A tool call starting is progress for the turn's idle detector, and
        // this is the one place every production tool call passes through.
        crate::agent::idle::beat(crate::agent::idle::ProgressKind::ToolStart);
        let input_sha256 = format!("{:x}", Sha256::digest(command.arguments.to_string().as_bytes()));
        let Some(resolved) = self.resolve(&command.name) else {
            let available = self
                .descriptors()
                .into_iter()
                .map(|descriptor| descriptor.public_name)
                .collect::<Vec<_>>();
            let error = format!(
                "unknown tool '{}'; available tools: {}",
                command.name,
                available.join(", ")
            );
            return self.finish(
                &command,
                &context,
                input_sha256,
                None,
                None,
                ToolExecutionStatus::UnknownTool,
                format!("Error: {error}"),
                None,
                Some(error),
                None,
                started,
            );
        };
        let descriptor = resolved.descriptor;
        if let Some(stage) = crate::runtime::experiment_stage::current() {
            if let Err(error) = stage.claim_tool(&descriptor.backend_name) {
                let error = error.to_string();
                return self.finish(
                    &command,
                    &context,
                    input_sha256,
                    None,
                    None,
                    ToolExecutionStatus::Failed,
                    error.clone(),
                    None,
                    Some(error),
                    None,
                    started,
                );
            }
        }

        if let Some(bindings) = &context.execution_bindings {
            let verified = bindings
                .get(&command.name)
                .filter(|_| context.execution_trace.as_ref().is_some_and(|trace| !trace.is_sealed()))
                .ok_or(super::capability::execution_binding::ToolExecutionError::StaleCapability)
                .and_then(|binding| binding.verify(self, &context.envelope, &descriptor));
            if let Err(error) = verified {
                let error = error.to_string();
                return self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    None,
                    ToolExecutionStatus::StaleCapability,
                    format!("Error: {error}"),
                    None,
                    Some(error),
                    None,
                    started,
                );
            }
        }
        // V02-R9 / OQ-21: an owner-isolated origin runs only tools that enforce
        // per-owner isolation. Keyed on the envelope rather than a channel name,
        // so every isolated origin inherits it, and decided before policy so no
        // autonomy level can widen it.
        let private_filesystem = context
            .owner_filesystem
            .as_ref()
            .filter(|scope| {
                (context.controlled_case().is_some()
                    || (context.envelope.owner_isolated
                        && context.envelope.source == crate::runtime::envelope::RuntimeSource::Console))
                    && (if context.controlled_case().is_some() {
                        scope.matches(&context.envelope.workspace_id, &context.envelope.resolved_owner_id())
                    } else {
                        context
                            .envelope
                            .owner_id
                            .as_deref()
                            .is_some_and(|owner| scope.matches(&context.envelope.workspace_id, owner))
                    })
                    && descriptor.adapter == ToolAdapterKind::Native
                    && descriptor.backend_name == descriptor.public_name
                    && matches!(
                        descriptor.public_name.as_str(),
                        "file_read" | "file_write" | "file_edit"
                    )
                    && resolved.backend.supports_owner_filesystem()
            })
            .cloned();
        if context.envelope.owner_isolated && !admits_owner_isolated_origin(&descriptor) && private_filesystem.is_none()
        {
            let error = owner_isolation_refusal(&descriptor.public_name);
            return self.finish(
                &command,
                &context,
                input_sha256,
                Some(descriptor),
                None,
                ToolExecutionStatus::Denied,
                format!("Error: {error}"),
                None,
                Some(error),
                None,
                started,
            );
        }
        // Structural validation runs before approval on purpose: a call that
        // cannot legally execute must never reach an approval prompt, a
        // reservation, or a backend. This is the one production gate for the
        // tool contract, and every entry point funnels through here — including
        // the gateway MCP server, which ran outside it until 0.8.126. The
        // contract enforced is the one the caller was shown: see
        // `ToolExecutionContext::schema_dialect`.
        let contract = enforce_command_contract(&mut command.arguments, &descriptor, context.schema_dialect);
        if let Err(error) = contract {
            return self.finish(
                &command,
                &context,
                input_sha256,
                Some(descriptor),
                None,
                ToolExecutionStatus::InvalidArguments,
                format!("Error: {error}"),
                None,
                Some(error),
                None,
                started,
            );
        }

        let h2_invocation = match async {
            use crate::runtime::h2_entry::{EntryAction, EntryInvocation};
            let requested = command.name == "skills_manage"
                && command
                    .arguments
                    .get("action")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|name| name.starts_with("lifecycle_experiment_"));
            if let Some(origin) = context.h2_origin() {
                origin.ensure_live()?;
                anyhow::ensure!(
                    origin.profile().permits_tool(&command.name),
                    "tool is outside the fixed controlled profile"
                );
                if command.name == "skills_manage" {
                    EntryAction::parse(
                        command
                            .arguments
                            .get("action")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default(),
                    )?;
                }
            }
            if !requested {
                return Ok::<_, anyhow::Error>(None);
            }
            let origin = context
                .h2_origin()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("experiment action has no actual controlled source"))?;
            let binding = context
                .execution_bindings
                .as_ref()
                .and_then(|bindings| bindings.get(&command.name))
                .ok_or_else(|| anyhow::anyhow!("experiment action has no exact execution binding"))?;
            let verified = binding
                .bind_call(
                    self,
                    &context.envelope,
                    &descriptor,
                    &command,
                    context.decision_approval.as_ref(),
                    context.schema_dialect,
                )
                .await?;
            // Only execution after the original approval can consume this port.
            // The initial pending call still needs to be recorded first.
            if context.decision_resume.is_none()
                && crate::security::approval_operation::requires_explicit_grant(&command.name, &command.arguments)
            {
                return Ok(None);
            }
            Ok(Some(EntryInvocation::from_verified(
                origin,
                &verified,
                context.h2_review().cloned(),
            )?))
        }
        .await
        {
            Ok(invocation) => invocation,
            Err(error) => {
                let error = error.to_string();
                return self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    None,
                    ToolExecutionStatus::Denied,
                    format!("Error: {error}"),
                    None,
                    Some(error),
                    None,
                    started,
                );
            }
        };

        let decision = self.policy.decide(&descriptor, &context);
        // A call the tool itself only accepts with a signed one-shot grant
        // needs a human approval at every autonomy level, also where policy
        // would allow it. Deny stays Deny.
        let decision = if decision != ToolExecutionDecision::Deny
            && (crate::security::approval_operation::requires_explicit_grant(
                &descriptor.public_name,
                &command.arguments,
            ) || context
                .execution_bindings
                .as_ref()
                .and_then(|bindings| bindings.get(&command.name))
                .is_some_and(|binding| binding.requires_approval()))
        {
            ToolExecutionDecision::Ask
        } else {
            decision
        };
        if decision == ToolExecutionDecision::Deny {
            let error = format!(
                "tool '{}' is not permitted under the current execution policy",
                descriptor.public_name
            );
            return self.finish(
                &command,
                &context,
                input_sha256,
                Some(descriptor),
                Some(decision),
                ToolExecutionStatus::Denied,
                format!("Error: {error}"),
                None,
                Some(error),
                None,
                started,
            );
        }

        let mut runtime_approval_granted = false;
        let mut runtime_grant = None;
        if let Some(resume) = &context.decision_resume {
            match resume
                .verify(&command, &context)
                .and_then(|grant| serde_json::to_value(grant).map_err(anyhow::Error::from))
            {
                Ok(grant) => {
                    runtime_approval_granted = true;
                    runtime_grant = Some(grant);
                }
                Err(error) => {
                    let error = error.to_string();
                    return self.finish(
                        &command,
                        &context,
                        input_sha256,
                        Some(descriptor),
                        Some(decision),
                        ToolExecutionStatus::ApprovalDenied,
                        format!("Error: {error}"),
                        None,
                        Some(error),
                        None,
                        started,
                    );
                }
            }
        }
        let approval_run = context.envelope.run_id.clone().unwrap_or_default();
        let approval_channel = context.envelope.channel.clone().unwrap_or_default();
        let channel_gate = crate::agent::channel_approval::lookup_for_call(&approval_run);
        let decision = if channel_gate.as_ref().is_some_and(|(_, force_ask)| *force_ask)
            && decision != ToolExecutionDecision::Deny
        {
            ToolExecutionDecision::Ask
        } else {
            decision
        };
        if decision == ToolExecutionDecision::Ask && context.decision_resume.is_none() && channel_gate.is_some() {
            let resolved = crate::agent::channel_approval::resolve_tool(
                &approval_run,
                &descriptor.public_name,
                &command.arguments,
                &approval_channel,
            )
            .await;
            if !matches!(resolved, Some(crate::agent::turn_contracts::ApprovalDecision::Grant)) {
                let reason = "approval denied".to_string();
                return self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    ToolExecutionStatus::ApprovalDenied,
                    format!("Denied: {reason}"),
                    None,
                    Some(reason),
                    None,
                    started,
                );
            }
            runtime_approval_granted = true;
        } else if decision == ToolExecutionDecision::Ask && context.decision_resume.is_none() {
            let decision_call = match async {
                let Some(bindings) = context.execution_bindings.as_ref() else {
                    return Ok(None);
                };
                let binding = bindings
                    .get(&command.name)
                    .ok_or_else(|| anyhow::anyhow!("approval execution binding missing"))?;
                binding
                    .bind_call(
                        self,
                        &context.envelope,
                        &descriptor,
                        &command,
                        context.decision_approval.as_ref(),
                        context.schema_dialect,
                    )
                    .await
                    .map(Some)
            }
            .await
            {
                Ok(call) => call,
                Err(error) => {
                    let error = error.to_string();
                    return self.finish(
                        &command,
                        &context,
                        input_sha256,
                        Some(descriptor),
                        Some(decision),
                        ToolExecutionStatus::StaleCapability,
                        format!("Error: {error}"),
                        None,
                        Some(error),
                        None,
                        started,
                    );
                }
            };
            if let Some(call) = &decision_call {
                let pending = match &context.decision_approval {
                    Some(port) => port.record(call).await,
                    None => Err(anyhow::anyhow!("decision approval persistence unavailable")),
                };
                let (status, reason) = match pending {
                    Ok(()) => (
                        ToolExecutionStatus::AwaitingApproval,
                        format!(
                            "Approval is required for decision {}. The operation has not run.",
                            call.binding().decision_id
                        ),
                    ),
                    Err(error) => (
                        ToolExecutionStatus::ApprovalDenied,
                        format!("decision approval unavailable: {error}"),
                    ),
                };
                return self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    status,
                    reason.clone(),
                    None,
                    (status != ToolExecutionStatus::AwaitingApproval).then_some(reason),
                    None,
                    started,
                );
            }
            match self
                .approval
                .resolve(ToolApprovalRequest {
                    command: command.clone(),
                    descriptor: descriptor.clone(),
                    context: context.clone(),
                    decision_call,
                })
                .await
            {
                ToolApprovalDecision::Approved {
                    runtime_approval_granted: approved,
                    runtime_grant: grant,
                } => {
                    runtime_approval_granted = approved;
                    runtime_grant = grant;
                }
                ToolApprovalDecision::Denied { reason } => {
                    return self.finish(
                        &command,
                        &context,
                        input_sha256,
                        Some(descriptor),
                        Some(decision),
                        ToolExecutionStatus::ApprovalDenied,
                        format!("Denied: {reason}"),
                        None,
                        Some(reason),
                        None,
                        started,
                    );
                }
                ToolApprovalDecision::Cancelled { reason } => {
                    return self.finish(
                        &command,
                        &context,
                        input_sha256,
                        Some(descriptor),
                        Some(decision),
                        ToolExecutionStatus::Cancelled,
                        format!("Error: {reason}"),
                        None,
                        Some(reason),
                        None,
                        started,
                    );
                }
            }
        }

        let preparation = match self.preparation.prepare(&descriptor, &command, &context).await {
            Ok(permit) => permit,
            Err(error) => {
                return self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    ToolExecutionStatus::PreparationDenied,
                    format!("Error: preparation denied tool execution: {error}"),
                    None,
                    Some(error),
                    None,
                    started,
                );
            }
        };

        let arguments = match normalize_arguments(&command.arguments, &context, runtime_approval_granted, runtime_grant)
        {
            Ok(arguments) => arguments,
            Err(error) => {
                return self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    ToolExecutionStatus::InvalidArguments,
                    format!("Error: {error}"),
                    None,
                    Some(error),
                    Some(preparation),
                    started,
                );
            }
        };
        if cancellation.as_ref().is_some_and(CancellationToken::is_cancelled) {
            return self.finish(
                &command,
                &context,
                input_sha256,
                Some(descriptor),
                Some(decision),
                ToolExecutionStatus::Cancelled,
                "Error: tool execution cancelled".to_string(),
                None,
                Some("tool execution cancelled before reservation".to_string()),
                Some(preparation),
                started,
            );
        }

        let exact_call = context
            .execution_trace
            .as_ref()
            .and_then(|trace| trace.begin_exact_call(&command, &context, &input_sha256));
        let idempotency_lease = match self
            .prepare_idempotent_execution(
                &command,
                &context,
                &input_sha256,
                &descriptor,
                decision,
                &preparation,
                started,
            )
            .await
        {
            Ok(lease) => {
                if let Some(observation) = &exact_call {
                    observation.reserved(lease.as_ref().map(|lease| lease.reservation.attempt));
                }
                lease
            }
            Err(outcome) => {
                if let Some(observation) = exact_call {
                    observation.refused(outcome.status, outcome.replayed);
                }
                return *outcome;
            }
        };
        #[cfg(test)]
        if let Some(gate) = self
            .post_reservation_gate
            .as_ref()
            .filter(|gate| gate.armed.swap(false, AtomicOrdering::AcqRel))
        {
            gate.entered.notify_one();
            gate.release.notified().await;
        }

        // A resume consumes approval only after preparation and the execution
        // reservation. Cancellation or an uncertain claim never restores it.
        let claimed_decision = if let Some(resume) = &context.decision_resume {
            let claim = async {
                if let Some(token) = cancellation.as_ref() {
                    tokio::select! {
                        biased;
                        () = token.cancelled() => anyhow::bail!("resume cancelled during final claim"),
                        result = resume.claim(&command, &context) => result,
                    }
                } else {
                    resume.claim(&command, &context).await
                }
            }
            .await;
            match claim {
                Ok(authority) => Some(authority),
                Err(error) => {
                    // The backend has not started, but the durable claim may
                    // have committed before a readback failure/cancellation.
                    let abandonment = self
                        .abandon_idempotent_execution(idempotency_lease.as_ref(), &context)
                        .await;
                    let error = abandonment.err().unwrap_or_else(|| error.to_string());
                    if let Some(observation) = exact_call {
                        observation.refused(ToolExecutionStatus::Indeterminate, false);
                    }
                    return self.finish(
                        &command,
                        &context,
                        input_sha256,
                        Some(descriptor),
                        Some(decision),
                        ToolExecutionStatus::Indeterminate,
                        format!("Error: {error}"),
                        None,
                        Some(error),
                        Some(preparation),
                        started,
                    );
                }
            }
        } else {
            None
        };

        // A scheduled claim can expire while approval, preparation or the
        // idempotency reservation awaits. Validate its live authority before
        // constructing/polling the backend future. The guard may read SQLite,
        // so it runs on the blocking pool, never inside the schema verifier.
        if let Some(authority_guard) = context.envelope.authority_guard.clone() {
            let validate = async {
                crate::runtime::blocking::spawn_blocking(move || authority_guard.validate())
                    .await
                    .is_ok_and(|result| result.unwrap_or(false))
            };
            let current = if let Some(token) = cancellation.as_ref() {
                tokio::select! {
                    biased;
                    () = token.cancelled() => false,
                    current = validate => current,
                }
            } else {
                validate.await
            };
            if !current {
                let cancelled = cancellation.as_ref().is_some_and(CancellationToken::is_cancelled);
                let status = if cancelled {
                    ToolExecutionStatus::Cancelled
                } else {
                    ToolExecutionStatus::StaleCapability
                };
                let error = if cancelled {
                    "tool execution cancelled during runtime authority validation"
                } else {
                    "runtime execution authority is expired or unavailable"
                };
                let abandonment = self
                    .abandon_idempotent_execution(idempotency_lease.as_ref(), &context)
                    .await;
                let (status, error) = match abandonment {
                    Ok(()) => (status, error.to_owned()),
                    Err(error) => (ToolExecutionStatus::Indeterminate, error),
                };
                if let Some(observation) = exact_call {
                    observation.complete(status);
                }
                return self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    status,
                    format!("Error: {error}"),
                    None,
                    Some(error),
                    Some(preparation),
                    started,
                );
            }
        }

        // Approval, preparation, reservation and claim may have awaited while the
        // live registry changed. Recheck before the backend future is polled.
        if let Some(bindings) = &context.execution_bindings {
            let verified = bindings
                .get(&command.name)
                .filter(|_| context.execution_trace.as_ref().is_some_and(|trace| !trace.is_sealed()))
                .ok_or(super::capability::execution_binding::ToolExecutionError::StaleCapability)
                .and_then(|binding| binding.verify(self, &context.envelope, &descriptor));
            if let Err(error) = verified {
                let abandonment = self
                    .abandon_idempotent_execution(idempotency_lease.as_ref(), &context)
                    .await;
                let (status, error) = match abandonment {
                    Ok(()) => (ToolExecutionStatus::StaleCapability, error.to_string()),
                    Err(error) => (ToolExecutionStatus::Indeterminate, error),
                };
                if let Some(observation) = exact_call {
                    observation.complete(status);
                }
                return self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    status,
                    format!("Error: {error}"),
                    None,
                    Some(error),
                    Some(preparation),
                    started,
                );
            }
        }

        // The resolved Arc is immutable for the in-flight call even if a
        // dynamic catalog refresh changes future descriptor snapshots.
        let backend = resolved.backend;
        let child_tool_name = match command.name.as_str() {
            "delegate" => Some("delegate"),
            "sessions_spawn" => Some("sessions_spawn"),
            _ => None,
        };
        let child_call_context = child_tool_name.map(|tool_name| ChildCallContext {
            tool_name,
            context: context.clone(),
        });
        // Scoped so any child process the tool spawns registers with this call
        // as its parent, which is what makes a cascading kill possible.
        let backend_cancellation = cancellation.clone();
        let backend_h2_invocation = h2_invocation.clone();
        let exact_call_ref = exact_call.as_ref();
        let public_name = command.name.as_str();
        let execution_parameters = descriptor.execution_parameters.clone();
        let schema_dialect = context.schema_dialect;
        let tool_future = crate::runtime::registry::scope_current(
            work.id(),
            crate::runtime::child_source::scope_outgoing_grant(
                context.child_grant.clone(),
                CHILD_CALL_CONTEXT.scope(
                    child_call_context,
                    super::capability::search_tool::with_search_authority(
                        if command.name == "capability_search" {
                            context.capability_search.clone()
                        } else {
                            None
                        },
                        super::capability::execution_binding::scope_dispatch_binding(
                            if backend.root_name() == "mcp_call" {
                                context
                                    .execution_bindings
                                    .as_ref()
                                    .and_then(|bindings| bindings.get(&command.name))
                            } else {
                                None
                            },
                            async move {
                                if let Some(observation) = exact_call_ref {
                                    observation.backend_started();
                                }
                                super::owner_filesystem_dispatch::scope(
                                    private_filesystem,
                                    crate::tools::schema::with_execution_contract(
                                        execution_parameters,
                                        schema_dialect,
                                        crate::runtime::h2_entry::scope_invocation(
                                            backend_h2_invocation.map(|invocation| {
                                                (invocation, backend_cancellation.clone().unwrap_or_default())
                                            }),
                                            backend.invoke_observed(public_name, arguments, backend_cancellation),
                                        ),
                                    ),
                                )
                                .await
                            },
                        ),
                    ),
                ),
            ),
        );
        let tool_future = async {
            if let Some(invocation) = &h2_invocation {
                invocation.ensure_live()?;
            }
            match claimed_decision {
                Some(authority) => {
                    // Check again on the backend future's first poll, after
                    // scheduling/cancellation may have closed the owning turn.
                    context
                        .decision_resume
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("resume adapter missing at dispatch"))?
                        .verify(&command, &context)?;
                    crate::security::decision_approval::with_claimed_decision(authority, tool_future).await
                }
                None => tool_future.await,
            }
        };
        let mut cancelled_before_invoke = false;
        let raw_result = if let (Some(token), Some(_)) = (cancellation.clone(), idempotency_lease.as_ref()) {
            // The outer cancellation race owns only the ReservedNotStarted
            // phase. The wrapper marks the handoff immediately before the
            // backend's first poll. Once that boundary is crossed we retain
            // ownership and drive the backend's cooperative cancellation to a
            // terminal result instead of dropping an indeterminate side
            // effect.
            let backend_started = Arc::new(AtomicBool::new(false));
            tokio::pin!(tool_future);
            let started_for_poll = Arc::clone(&backend_started);
            let tracked_future = std::future::poll_fn(move |context| {
                started_for_poll.store(true, AtomicOrdering::Release);
                tool_future.as_mut().poll(context)
            });
            tokio::pin!(tracked_future);
            tokio::select! {
                biased;
                () = token.cancelled() => {
                    if backend_started.load(AtomicOrdering::Acquire) {
                        Some(tracked_future.as_mut().await)
                    } else {
                        cancelled_before_invoke = true;
                        None
                    }
                },
                result = tracked_future.as_mut() => Some(result),
            }
        } else if let Some(token) = cancellation {
            tokio::select! {
                biased;
                () = token.cancelled() => None,
                result = tool_future => Some(result),
            }
        } else {
            Some(tool_future.await)
        };

        let abandonment_error = if cancelled_before_invoke {
            self.abandon_idempotent_execution(idempotency_lease.as_ref(), &context)
                .await
                .err()
        } else {
            None
        };
        let (raw_result, process) = match raw_result {
            Some(Ok(observed)) => (Some(Ok(observed.result)), observed.process),
            Some(Err(error)) => (Some(Err(error)), None),
            None => (None, None),
        };
        let mut outcome = match (raw_result, abandonment_error) {
            (None, Some(error)) => self.finish(
                &command,
                &context,
                input_sha256,
                Some(descriptor),
                Some(decision),
                ToolExecutionStatus::Indeterminate,
                format!("Error: {error}"),
                None,
                Some(error),
                Some(preparation),
                started,
            ),
            (None, None) => self.finish(
                &command,
                &context,
                input_sha256,
                Some(descriptor),
                Some(decision),
                ToolExecutionStatus::Cancelled,
                "Error: tool execution cancelled".to_string(),
                None,
                Some("tool execution cancelled".to_string()),
                Some(preparation),
                started,
            ),
            (Some(Ok(result)), _)
                if is_tool_cancelled_result(&result)
                    || process
                        .as_ref()
                        .is_some_and(|process| process.termination == super::traits::ProcessTermination::Cancelled) =>
            {
                self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    ToolExecutionStatus::Cancelled,
                    "Error: tool execution cancelled".to_string(),
                    Some(result),
                    Some("tool execution cancelled".to_string()),
                    Some(preparation),
                    started,
                )
            }
            (Some(Ok(result)), _) => {
                let (status, model_content, error) = if result.success {
                    let model_content = match result.error.as_deref().filter(|error| !error.is_empty()) {
                        Some(diagnostics) => format!("Output:\n{}\n\nDiagnostics:\n{diagnostics}", result.output),
                        None => result.output.clone(),
                    };
                    (ToolExecutionStatus::Succeeded, model_content, None)
                } else {
                    let error = process
                        .as_ref()
                        .map(|process| {
                            process
                                .diagnostic
                                .clone()
                                .unwrap_or_else(|| result.error.clone().unwrap_or_else(|| process.summary()))
                        })
                        .unwrap_or_else(|| result.error.clone().unwrap_or_else(|| result.output.clone()));
                    let hint = super::error_hints::recovery_hint(&command.name, &error);
                    let mut model_content = if hint.is_empty() {
                        format!("Error: {error}")
                    } else {
                        format!("Error: {error}\n{hint}")
                    };
                    // `error` is a separate diagnostic channel, not a replacement
                    // for output produced before failure. Keep the Error prefix
                    // used by existing history consumers and avoid duplicating
                    // output when it was the fallback error above.
                    if result.error.is_some() && !result.output.is_empty() {
                        model_content.push_str(&format!("\n\nOutput:\n{}", result.output));
                    }
                    (ToolExecutionStatus::Failed, model_content, Some(error))
                };
                self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    status,
                    model_content,
                    Some(result),
                    error,
                    Some(preparation),
                    started,
                )
            }
            (Some(Err(error)), _) => {
                // The trace alone classifies the full source chain; the outcome and
                // everything built from it keep the plain message.
                if let Some(trace) = context.execution_trace.as_ref() {
                    trace.note_error_detail(&command.operation_id, format!("{error:#}"));
                }
                let status = if error
                    .downcast_ref::<crate::tools::schema::ToolArgumentError>()
                    .is_some()
                {
                    ToolExecutionStatus::InvalidArguments
                } else {
                    ToolExecutionStatus::Failed
                };
                let error = error.to_string();
                let hint = super::error_hints::recovery_hint(&command.name, &error);
                let model_content = if hint.is_empty() {
                    format!("Error executing {}: {error}", command.name)
                } else {
                    format!("Error executing {}: {error}\n{hint}", command.name)
                };
                self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    status,
                    model_content,
                    None,
                    Some(error),
                    Some(preparation),
                    started,
                )
            }
        };
        outcome.process = process;
        if let (Some(process), Some(result)) = (&outcome.process, &outcome.result) {
            outcome.model_content = process.render(result);
            if !result.success {
                let hint =
                    super::error_hints::recovery_hint(&command.name, outcome.error.as_deref().unwrap_or_default());
                if !hint.is_empty() {
                    outcome.model_content.push('\n');
                    outcome.model_content.push_str(&hint);
                }
            }
        }
        if let Some(observation) = exact_call {
            observation.complete(outcome.status);
        }
        if !cancelled_before_invoke {
            self.commit_idempotent_execution(idempotency_lease.as_ref(), &context, &outcome)
                .await;
        }
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        &self,
        command: &ToolExecutionCommand,
        context: &ToolExecutionContext,
        input_sha256: String,
        descriptor: Option<ToolDescriptor>,
        decision: Option<ToolExecutionDecision>,
        status: ToolExecutionStatus,
        model_content: String,
        result: Option<ToolResult>,
        error: Option<String>,
        preparation: Option<ToolExecutionPermit>,
        started: Instant,
    ) -> ToolExecutionOutcome {
        // ... and a tool call producing its terminal outcome is progress too.
        crate::agent::idle::beat(crate::agent::idle::ProgressKind::ToolEnd);
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.audit.record(ToolExecutionAuditRecord {
            operation_id: command.operation_id.clone(),
            capability: command.name.clone(),
            backend_name: descriptor.as_ref().map(|value| value.backend_name.clone()),
            adapter: descriptor.as_ref().map(|value| value.adapter),
            effect: descriptor.as_ref().map(|value| value.effect),
            decision,
            status,
            preparation_strategy: preparation.as_ref().map(|value| value.strategy.clone()),
            input_sha256,
            source: context.envelope.source.as_str().to_string(),
            workspace_id: context.envelope.workspace_id.clone(),
            session_key: context.envelope.session_key.clone(),
            run_id: context.envelope.run_id.clone(),
            parent_run_id: context.envelope.parent_run_id.clone(),
            task_id: context.envelope.task_id.clone(),
            error: error.clone(),
            duration_ms,
        });
        ToolExecutionOutcome {
            operation_id: command.operation_id.clone(),
            descriptor,
            decision,
            status,
            model_content,
            result,
            process: None,
            error,
            preparation,
            duration_ms,
            replayed: false,
        }
    }
}

fn tool_execution_event_input(
    context: &ToolExecutionContext,
    idempotency_key: &str,
    event_type: &str,
    content: String,
    raw_payload_json: String,
) -> MessageEventInput {
    MessageEventInput {
        event_id: None,
        idempotency_key: Some(idempotency_key.to_string()),
        workspace_id: context.envelope.workspace_id.clone(),
        owner_id: context.envelope.owner_id.clone(),
        source: context.envelope.source.as_str().into(),
        channel: context.envelope.channel.clone(),
        session_key: Some(context.envelope.session_key.clone()),
        parent_session_key: context.envelope.legacy_session_key.clone(),
        run_id: context.envelope.run_id.clone(),
        parent_run_id: context.envelope.parent_run_id.clone(),
        agent_id: context.envelope.agent_id.clone(),
        persona_id: context.envelope.persona_id.clone(),
        sender: context.envelope.sender.clone(),
        recipient: context.envelope.recipient.clone(),
        role: "event".to_string(),
        event_type: event_type.to_string(),
        subject: None,
        goal_id: None,
        causation_event_id: context.envelope.source_message_event_id.clone(),
        correlation_id: context.envelope.run_id.clone(),
        attempt_id: context.envelope.attempt_id.clone(),
        lease_epoch: context.envelope.lease_epoch,
        config_generation_id: context.envelope.config_generation_id,
        config_source_revision: context.envelope.config_source_revision.clone(),
        content,
        raw_payload_json: Some(raw_payload_json),
        visibility: context.envelope.visibility.clone(),
    }
}

/// Tool-argument name prefixes reserved for the runtime.
///
/// Every argument under one of these prefixes is *runtime-only*: it is written
/// by the runtime immediately before a tool executes and it carries trust —
/// the caller scope (`_zc_scope`, `_zc_scope_trusted`, `_prx_scope_trusted`),
/// the approval grant (`_zc_approval_granted`, `_zc_approval_grant`) and the
/// principal binding (`_zc_principal`). None of it may ever survive from an
/// externally supplied payload (model output, an MCP gateway HTTP body, a
/// third-party tool result) into tool execution, and none of it may leak back
/// out to a third-party MCP server.
///
/// Stripping is by prefix on purpose. A per-key blocklist fails *open*: adding
/// a new `_zc_*` key and forgetting one of the injection sites silently accepts
/// a forged one. With a prefix the same mistake fails *closed* — the runtime
/// key simply goes missing, which surfaces immediately as a denied or
/// untrusted call instead of an authorization bypass.
pub const RUNTIME_ONLY_ARG_PREFIXES: [&str; 2] = ["_zc_", "_prx_"];

/// Tools that enforce per-owner isolation for an owner-isolated origin
/// (V02-R9 / OQ-21).
///
/// An owner-isolated context — today an externally authenticated MCP caller —
/// may execute only these. Stateful tools rebuild their principal from the service-authored
/// `_zc_scope` and applies [`scope_requires_owner_match`] (or the run registry's
/// owner scope) to every row it reads or deletes. Every other tool is refused:
/// autonomy approval is not consulted at `AutonomyLevel::Full`, so the operator's
/// `mcp_server.exposed_tools` list would otherwise be the only boundary between
/// an external caller and a mutating tool that does not scope its effect by
/// owner. A tool joins this list only once it is owner-aware or reads no private
/// runtime state. Web search is stateless; its provider key and quota are shared.
///
/// `session_status` and `sessions_send` are owner-aware for in-process turns
/// but the gateway does not register them, so they stay off the list until it
/// does: an exposed entry is then refused with 403 before any identity binding
/// is written, instead of passing this gate and answering 404 afterwards.
pub use crate::causal_tree::capability_policy::OWNER_ISOLATION_AWARE_TOOLS;

/// Whether `name` is on [`OWNER_ISOLATION_AWARE_TOOLS`].
#[must_use]
pub fn is_owner_isolation_aware_tool(name: &str) -> bool {
    OWNER_ISOLATION_AWARE_TOOLS.contains(&name)
}

/// Whether a resolved tool may run for an owner-isolated origin.
///
/// The public name must be on the list **and** be served by the backend of the
/// same name, so a dynamic MCP, WASM or skill alias that happens to publish a
/// listed name from another backend is refused: it is not the audited tool.
#[must_use]
pub fn admits_owner_isolated_origin(descriptor: &ToolDescriptor) -> bool {
    crate::causal_tree::capability_policy::admits_owner_isolated_identity(
        &descriptor.public_name,
        &descriptor.backend_name,
    )
}

/// The refusal given to an owner-isolated caller for a tool that does not
/// enforce per-owner isolation.
#[must_use]
pub fn owner_isolation_refusal(tool_name: &str) -> String {
    format!("tool '{tool_name}' is not available to an owner-isolated caller: it does not enforce per-owner isolation")
}

/// Key inside the service-authored `_zc_scope` that marks the turn's origin as
/// owner-isolated (C03 / V02-R4).
///
/// Written only when [`crate::runtime::envelope::RuntimeEnvelope::owner_isolated`]
/// is set, which today means an externally authenticated MCP caller. Tools that
/// rebuild a [`crate::memory::MemoryPrincipal`] from the scope read it back with
/// [`scope_requires_owner_match`].
pub const SCOPE_OWNER_ISOLATED_KEY: &str = "owner_isolated";

/// Read the owner-isolation marker back out of tool arguments.
///
/// Honoured only when `_zc_scope_trusted` is set, and that whole block is
/// re-authored by [`normalize_arguments`] from the authenticated execution
/// context after [`strip_runtime_only_args`] has dropped everything the caller
/// supplied — so a caller can neither forge the marker nor clear it.
#[must_use]
pub fn scope_requires_owner_match(args: &serde_json::Value) -> bool {
    args.get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        && args
            .get("_zc_scope")
            .and_then(serde_json::Value::as_object)
            .and_then(|scope| scope.get(SCOPE_OWNER_ISOLATED_KEY))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

/// Whether `key` names a runtime-only tool argument (see
/// [`RUNTIME_ONLY_ARG_PREFIXES`]).
#[must_use]
pub fn is_runtime_only_arg(key: &str) -> bool {
    RUNTIME_ONLY_ARG_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
}

/// Single source of truth for scrubbing runtime-only arguments off a tool
/// argument object.
///
/// Every site that injects trusted runtime state into tool arguments must call
/// this first, and every site that forwards arguments to an untrusted process
/// must call it last.
pub fn strip_runtime_only_args(root: &mut serde_json::Map<String, serde_json::Value>) {
    root.retain(|key, _| !is_runtime_only_arg(key));
}

/// Enforce a tool's published contract on caller-supplied arguments: validate
/// them, then make the document say what the contract means.
///
/// The descriptor's full `execution_parameters` (or public `parameters` when
/// no private contract exists) is the truth source: native,
/// skill, MCP alias and WASM plugin tools all reach this gate. Migrated built-ins
/// keep their original conditional rules here while publishing a flat schema.
/// `dialect` replays the provider's keyword lowering on the full contract:
/// Gemini strips `additionalProperties` and bounds using the same
/// [`crate::tools::schema::SchemaCleanr`] strategy as publication. Runtime-only
/// keys are removed first because the runtime authors them after validation and
/// they are intentionally absent from public schemas.
///
/// On success the arguments are normalized in place — see
/// [`crate::tools::schema::CompiledToolSchema::normalize_integral_numbers`].
/// That is why this takes `&mut`: enforcement is not only a verdict.
pub(crate) fn enforce_command_contract(
    arguments: &mut serde_json::Value,
    descriptor: &ToolDescriptor,
    dialect: ToolSchemaDialect,
) -> Result<(), String> {
    let compiled = crate::tools::schema::compiled_tool_schema_for_dialect(
        &descriptor.public_name,
        dialect,
        descriptor
            .execution_parameters
            .as_deref()
            .unwrap_or(&descriptor.parameters),
    );
    if !arguments.is_object() {
        return Err("tool arguments must be a JSON object".to_string());
    }
    let issues =
        crate::tools::schema::with_public_arguments(arguments, |public| compiled.validate_tool_arguments(public));
    if !issues.is_empty() {
        return Err(crate::tools::schema::describe_argument_validation_issues(
            &descriptor.public_name,
            &issues,
        ));
    }
    // The contract holds, so the document is now made to say what the contract
    // says it means: `integer` is a mathematical value in JSON Schema, and a
    // model that answers `200.0` for one is correct — but every executor reads
    // integers with `as_u64()` / `as_i64()`, which see `Float(200.0)` as absent
    // and fall back to a default. Coercing here, against the published schema,
    // is the one place that knows which fields were declared `integer`; doing
    // it in each executor would be ~20 copies of a rule none of them can state.
    compiled.normalize_integral_numbers(arguments);
    Ok(())
}

/// Validate effect arguments for a non-executing approval proposal.
///
/// The owning collection lease verifies source and capability authority. This
/// function returns only schema-normalized effect arguments, with no grant or
/// runtime scope injected. The normal executor authors those fields only after
/// approval; the pending-call digest binds its source separately.
pub(crate) fn normalize_collection_arguments(
    arguments: &serde_json::Value,
    descriptor: &ToolDescriptor,
    dialect: ToolSchemaDialect,
) -> Result<serde_json::Value, String> {
    let root = arguments
        .as_object()
        .ok_or_else(|| "collection arguments must be a JSON object".to_string())?;
    if root.keys().any(|key| is_runtime_only_arg(key)) {
        return Err("collection arguments contain reserved runtime fields".to_string());
    }
    let mut normalized = arguments.clone();
    enforce_command_contract(&mut normalized, descriptor, dialect)?;
    Ok(normalized)
}

fn normalize_arguments(
    arguments: &serde_json::Value,
    context: &ToolExecutionContext,
    runtime_approval_granted: bool,
    runtime_grant: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mut arguments = arguments.clone();
    let root = arguments
        .as_object_mut()
        .ok_or_else(|| "tool arguments must be a JSON object".to_string())?;

    // Structural validation against `descriptor.parameters` already ran in
    // `ToolExecutionService::execute`, before approval. Re-checking here would
    // be a second, divergent rule set.

    // Single source of truth: drop every runtime-only key an external caller
    // may have supplied before re-injecting the authoritative ones below.
    strip_runtime_only_args(root);

    let mut scope = serde_json::json!({
        "sender": context.sender(),
        "channel": context.channel(),
        "chat_type": context.chat_type,
        "chat_id": context.chat_id,
        "workspace_id": context.envelope.workspace_id,
        "session_key": context.envelope.session_key,
        "runtime_source": context.envelope.source.as_str(),
    });
    if let Some(scope) = scope.as_object_mut() {
        insert_optional(scope, "owner_id", context.envelope.owner_id.as_deref());
        // C03: only written when the origin is owner-isolated, so every other
        // turn's authored scope stays byte-identical to the pre-C03 one.
        if context.envelope.owner_isolated {
            scope.insert(SCOPE_OWNER_ISOLATED_KEY.to_string(), serde_json::Value::Bool(true));
        }
        insert_optional(scope, "topic_id", context.envelope.topic_id.as_deref());
        insert_optional(scope, "task_id", context.envelope.task_id.as_deref());
        insert_optional(scope, "run_id", context.envelope.run_id.as_deref());
        insert_optional(scope, "parent_run_id", context.envelope.parent_run_id.as_deref());
        insert_optional(
            scope,
            "source_message_event_id",
            context.envelope.source_message_event_id.as_deref(),
        );
        if let Some(config_generation_id) = context.envelope.config_generation_id {
            scope.insert(
                "config_generation_id".to_string(),
                serde_json::Value::Number(config_generation_id.into()),
            );
        }
        insert_optional(
            scope,
            "config_source_revision",
            context.envelope.config_source_revision.as_deref(),
        );
    }
    root.insert("_zc_scope".to_string(), scope);
    root.insert("_zc_scope_trusted".to_string(), serde_json::Value::Bool(true));
    // Local-operator origin marker (T18). Unlike `_zc_scope_trusted` — which
    // asserts "the `_zc_scope` block above was written by the runtime", and is
    // therefore always true here — this one asserts "*a human at this machine's
    // terminal* drove this turn". It is the guard on destructive `gateway`
    // actions. Authored here and only here: the caller-supplied value was
    // already dropped by `strip_runtime_only_args` above, so whatever the model
    // or an HTTP body asked for is irrelevant either way.
    root.insert(
        "_prx_scope_trusted".to_string(),
        serde_json::Value::Bool(context.is_local_operator()),
    );
    root.insert(
        RUNTIME_APPROVAL_GRANTED_ARG.to_string(),
        serde_json::Value::Bool(runtime_approval_granted),
    );
    if let Some(grant) = runtime_grant {
        root.insert(RUNTIME_APPROVAL_GRANT_ARG.to_string(), grant);
    }
    Ok(arguments)
}

fn insert_optional(map: &mut serde_json::Map<String, serde_json::Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        map.insert(key.to_string(), serde_json::Value::String(value.to_string()));
    }
}

#[cfg(test)]
mod tests;

/// T18 — local-operator origin marker (`_prx_scope_trusted`).
///
/// These tests pin the one property the destructive `gateway` guard rests on:
/// the marker is authored by the runtime from the turn's origin, and a caller
/// (model, IM user, HTTP body) can neither set it nor clear it.
#[cfg(test)]
mod local_operator_scope_tests;

#[cfg(test)]
mod shared_binding_state_tests;

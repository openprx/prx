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
    /// Read-only below-Full path precheck; see [`Tool::path_precheck`].
    async fn path_precheck(
        &self,
        _public_name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(), crate::security::PathRefusal> {
        Ok(())
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
    /// Absent-value normalisation of the raw arguments; runs before the contract check.
    fn normalize_arguments(&self, _arguments: &mut serde_json::Value) {}
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
    fn normalize_arguments(&self, arguments: &mut serde_json::Value) {
        if let Some(tool) = self.tool() {
            tool.normalize_arguments(arguments);
        }
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
    async fn path_precheck(
        &self,
        public_name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(), crate::security::PathRefusal> {
        // The root precheck reads the root's arguments; an alias is checked
        // by its backend at execute time.
        match self.tool() {
            Some(tool) if public_name == tool.name() => tool.path_precheck(arguments).await,
            _ => Ok(()),
        }
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
    fn normalize_arguments(&self, arguments: &mut serde_json::Value) {
        self.tool.normalize_arguments(arguments);
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
    async fn path_precheck(
        &self,
        public_name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(), crate::security::PathRefusal> {
        if public_name == self.tool.name() {
            self.tool.path_precheck(arguments).await
        } else {
            Ok(())
        }
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
    pub(crate) fn fingerprint(&self) -> crate::tools::capability::contract::SchemaFingerprint {
        match self {
            Self::Shared { entry, .. } => crate::tools::capability::contract::SchemaFingerprint(entry.fingerprint()),
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

/// Who constructed this execution context. Model turns fail closed without a
/// surface; every other origin keeps today's no-surface admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DispatchOrigin {
    ModelTurn,
    Operator,
    Job,
    #[default]
    Internal,
}

/// Authenticated runtime facade available to every execution stage.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct H2ExecutionMetadata {
    controlled_case: Option<Arc<crate::runtime::h2_entry::ControlledCaseScope>>,
    origin: Option<Arc<crate::runtime::h2_entry::VerifiedEntryOrigin>>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutionContext {
    pub(crate) live_turn: Option<crate::agent::live_frames::LiveTurnRef>,
    h2_metadata: Option<Box<H2ExecutionMetadata>>,
    pub(crate) dispatch_origin: DispatchOrigin,
    pub(crate) turn_surface: Option<Arc<admission::TurnSurface>>,
    pub(crate) approval_ledger: Option<crate::security::approval_ledger::strategy::ExecutionApprovalLedger>,
    pub(crate) owner_filesystem: Option<Arc<super::owner_filesystem::OwnerFilesystem>>,
    pub(crate) capability_search: Option<super::capability::search_tool::CapabilitySearchPort>,
    pub(crate) ordinary_child_publication: Option<crate::runtime::child_source::publication::OrdinaryChildPublication>,
    /// Incoming child authority of this turn.
    pub(crate) child_scope: Option<Arc<super::capability::child::VerifiedChildScope>>,
    /// Pinned child provenance; dispatch refuses either half without the other.
    pub(crate) child_pin: Option<Arc<super::capability::child::ChildDispatchPin>>,
    /// Live G6 tokens for the model turn's called tools; absent outside model turns.
    pub(crate) call_budget: Option<admission::CallTokenBudget>,
    /// The call context computed once when the turn was opened; tools read
    /// level, workspace and owner from it. Absent outside model turns.
    pub(crate) call: Option<Arc<crate::context::call::TurnCallContext>>,
    /// A call that needs approval ends denied before any ledger route or
    /// durable pending decision exists. Set for targeted agent turns, which
    /// have no resumable approval until MA-04.
    pub(crate) pending_approval_denied: bool,
    pub(crate) execution_trace: Option<super::capability::execution_trace::ExecutionTrace>,
    /// Admission-time pin for MCP schema-drift (gateway compat). Agent turns
    /// leave this empty; H2 uses `verify_admitted_call` against the service catalog.
    pub(crate) admitted_binding: Option<super::capability::execution_binding::ExecutionBinding>,
    pub(crate) execution_admission: Option<super::capability::execution_binding::ExecutionAdmission>,
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
    fn normalize_h2_metadata(&mut self) {
        if self
            .h2_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.controlled_case.is_none() && metadata.origin.is_none())
        {
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
            live_turn: None,
            h2_metadata: None,
            dispatch_origin: DispatchOrigin::Internal,
            turn_surface: None,
            approval_ledger: None,
            owner_filesystem: None,
            capability_search: None,
            ordinary_child_publication: None,
            child_scope: None,
            child_pin: None,
            call_budget: None,
            call: None,
            pending_approval_denied: false,
            admitted_binding: None,
            execution_admission: None,
            execution_trace: None,
            envelope,
            chat_type: chat_type.into(),
            chat_id,
            semantic_turn_id,
            schema_dialect: ToolSchemaDialect::Raw,
        }
    }

    /// Carries the authenticated incoming child authority from a child source
    /// proof. Child provenance without a verified scope and pin is refused here.
    pub(crate) fn with_incoming_child(
        mut self,
        proof: &dyn crate::runtime::turn_source::TurnSourceProof,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            proof.child_scope().is_some() && proof.child_pin().is_some(),
            "child provenance requires a verified scope and pinned catalog"
        );
        self.child_scope = proof.child_scope().cloned();
        self.child_pin = proof.child_pin().cloned();
        Ok(self)
    }

    /// Stamp the caller kind. Model-turn constructors set [`DispatchOrigin::ModelTurn`].
    #[must_use]
    pub(crate) const fn with_dispatch_origin(mut self, dispatch_origin: DispatchOrigin) -> Self {
        self.dispatch_origin = dispatch_origin;
        self
    }

    /// Deny every call that would otherwise wait for approval.
    #[must_use]
    pub(crate) const fn with_pending_approval_denied(mut self) -> Self {
        self.pending_approval_denied = true;
        self
    }

    pub(crate) fn with_approval_ledger(
        mut self,
        ledger: Option<crate::security::approval_ledger::strategy::ExecutionApprovalLedger>,
    ) -> Self {
        self.approval_ledger = ledger;
        self
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

    /// Whether the root principal of this call is the local operator: the
    /// terminal UI turn itself, any child of a terminal turn at any depth
    /// (children keep the root's `terminal` channel), a scheduled run the
    /// local operator created (the local CLI tuple), or a scheduled run with
    /// no recorded creator. Remote principals (IM channels, the console, the
    /// gateway and rf), their children and their jobs answer `false`, as does
    /// any owner-isolated scope. Runtime-authored; never caller input.
    #[must_use]
    pub fn is_local_root(&self) -> bool {
        if self.envelope.owner_isolated {
            return false;
        }
        self.is_local_operator()
            || self.channel() == LOCAL_OPERATOR_CHANNEL
            || (self.envelope.source == crate::runtime::envelope::RuntimeSource::Chat
                && self.channel() == LOCAL_CLI_CHANNEL
                && self.sender() == LOCAL_CLI_SENDER
                && self.chat_id == LOCAL_CLI_CHAT_ID
                && self.chat_type == LOCAL_OPERATOR_CHAT_TYPE)
            || (self.dispatch_origin == DispatchOrigin::Job && self.chat_type == JOB_ORIGIN_UNKNOWN)
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

/// The exact scope tuple an operator-created cron or xin run of the internal agent engine stamps
/// on every tool call: `agent_tool_execution_context(None, "cli")` in
/// `src/agent/execution_policy.rs`. Cron agent jobs and xin tasks enter through the same
/// `run_with_runtime_envelope`, but carry this tuple only when their
/// [`JobOrigin`] is [`JobOrigin::LocalOperator`]; every other job runs under
/// [`JobOrigin::scheduled_tool_context`] instead.
const LOCAL_CLI_CHANNEL: &str = "cli";
const LOCAL_CLI_SENDER: &str = "agent";
const LOCAL_CLI_CHAT_ID: &str = "agent:local";

/// Whether the service-authored scope in `args` comes from a **local-process
/// operator surface**: the terminal UI or the local-operator CLI tuple (D-019 Q-5).
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
/// the local-operator CLI tuple, so a job scheduled from a channel, webhook or the
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
    /// Created by the local operator. Runs keep the local-operator CLI tuple and
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

    /// [`Self::scheduled_tool_context`] paired with the configured policy at
    /// the run's `level` (see [`ScheduledToolScope::new`]); `None` for an
    /// operator job, which keeps the operator's CLI context and policy.
    #[must_use]
    pub fn scheduled_tool_scope(
        &self,
        run_envelope: RuntimeEnvelope,
        config: &crate::config::Config,
        level: crate::security::AutonomyLevel,
    ) -> Option<ScheduledToolScope> {
        self.scheduled_tool_context(run_envelope)
            .map(|context| ScheduledToolScope::new(context, config, level))
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
            Self::Unattributed => Some(
                ToolExecutionContext::new(run_envelope, JOB_ORIGIN_UNKNOWN).with_dispatch_origin(DispatchOrigin::Job),
            ),
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
                Some(ToolExecutionContext::new(envelope, chat_type.as_str()).with_dispatch_origin(DispatchOrigin::Job))
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
    /// Pair `context` with the configured policy at the run's `level`
    /// (`security::agent_level::scheduled_level`), so the creator's
    /// `[autonomy.scopes]` rules — keyed on the channel, sender and chat type
    /// the context carries — apply to the run exactly as they applied to the
    /// creator's own turn.
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
    pub fn new(
        context: ToolExecutionContext,
        config: &crate::config::Config,
        level: crate::security::policy::AutonomyLevel,
    ) -> Self {
        let mut policy = crate::runtime::bootstrap::build_security_policy(config);
        // Freshly built and not yet shared, so this mutates in place.
        Arc::make_mut(&mut policy).autonomy = if level == crate::security::policy::AutonomyLevel::Supervised {
            crate::security::policy::AutonomyLevel::ReadOnly
        } else {
            level
        };
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

    /// Re-scope this receipt onto a publication derived from the caller's own
    /// authenticated parent. Besides the dispatch boundary, this is the only
    /// constructor, and it refuses any object with a different parent.
    pub(crate) fn with_ordinary_publication(
        &self,
        derived: crate::runtime::child_source::publication::OrdinaryChildPublication,
    ) -> anyhow::Result<Self> {
        derived.verify_derived_from(&self.context)?;
        let mut context = self.context.clone();
        context.ordinary_child_publication = Some(derived);
        Ok(Self {
            tool_name: self.tool_name,
            context,
        })
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
    fn autonomy_level(&self) -> crate::security::AutonomyLevel;
    fn decide(&self, descriptor: &ToolDescriptor, context: &ToolExecutionContext) -> ToolExecutionDecision;
    /// Bind a bridge-approved call to the policy's trusted runtime scope.
    /// Policies without a grant issuer leave resource gates fail-closed.
    fn approved_call_grant(
        &self,
        _command: &ToolExecutionCommand,
        _context: &ToolExecutionContext,
    ) -> Option<serde_json::Value> {
        None
    }
    /// The decision at the call's effective level, which a child scope may
    /// set apart from the service's own level.
    fn decide_at(
        &self,
        descriptor: &ToolDescriptor,
        context: &ToolExecutionContext,
        _level: crate::security::AutonomyLevel,
    ) -> ToolExecutionDecision {
        self.decide(descriptor, context)
    }
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
    fn approved_call_grant(
        &self,
        command: &ToolExecutionCommand,
        context: &ToolExecutionContext,
    ) -> Option<serde_json::Value> {
        let scope =
            crate::agent::turn_contracts::ScopeContext::from_execution_context(&self.policy, context, &context.chat_id);
        let grant = crate::agent::execution_policy::runtime_approval_grant_for_call(
            &command.name,
            &command.arguments,
            Some(&scope),
        )?;
        serde_json::to_value(grant)
            .inspect_err(|_| tracing::warn!("bridge approval grant encoding failed"))
            .ok()
    }

    fn autonomy_level(&self) -> crate::security::AutonomyLevel {
        self.policy.autonomy
    }
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

    fn decide_at(
        &self,
        descriptor: &ToolDescriptor,
        context: &ToolExecutionContext,
        level: crate::security::AutonomyLevel,
    ) -> ToolExecutionDecision {
        if level == self.policy.autonomy {
            return self.decide(descriptor, context);
        }
        let policy_name = if descriptor.adapter == ToolAdapterKind::Skill {
            &descriptor.backend_name
        } else {
            &descriptor.public_name
        };
        let at_level = SecurityPolicy {
            autonomy: level,
            ..self.policy.as_ref().clone()
        };
        decide_tool_execution(
            &at_level,
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
}

/// Approval result. Runtime-only grant material is never accepted from the
/// original command arguments; only this trusted adapter may supply it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolApprovalDecision {
    Approved {
        runtime_approval_granted: bool,
        runtime_grant: Option<serde_json::Value>,
    },
    /// The call did not run. `cause` is the typed reason; `reason` is the
    /// display text the operator and the model read.
    Denied {
        cause: ApprovalDenialCause,
        reason: String,
    },
    Cancelled {
        reason: String,
    },
}

/// Why an approval-gated call was denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDenialCause {
    /// The operator answered no (`n`, `Esc`, `Enter` on deny, or any other
    /// plain answer).
    UserRejected,
    /// Plain mode: EOF, or the prompt stopped waiting.
    NoAnswer,
    /// The TUI had no approval surface, or the action channel closed.
    NoApprovalSurface,
    /// Plain or one-shot mode without a terminal and no allowing override.
    NoTerminal,
    /// Plain mode: the terminal was busy or its reader failed to start.
    PromptUnavailable,
    /// Another approval was already pending.
    ConcurrentPending,
    /// A session switch failed the approval closed.
    SessionSwitched,
    /// No resolver, or approval is not available for this agent.
    NoResolver,
    /// The channel approval gate denied the call.
    ChannelDenied,
    /// The approval ledger failed; its own error text is the reason.
    LedgerRefused,
}

/// Display text of a user rejection.
pub const APPROVAL_DENIED_USER_REJECTED: &str = "User rejected tool approval";
/// Display text when no approval answer was given.
pub const APPROVAL_DENIED_NO_ANSWER: &str = "No approval answer was given";
/// Display text when no approval prompt could be shown.
pub const APPROVAL_DENIED_NO_APPROVAL_SURFACE: &str = "No approval prompt could be shown, so the call was refused";
/// Display text when no terminal could ask for approval.
pub const APPROVAL_DENIED_NO_TERMINAL: &str = "No terminal was available to ask for approval";
/// Display text when the approval prompt was unavailable.
pub const APPROVAL_DENIED_PROMPT_UNAVAILABLE: &str = "The approval prompt was unavailable";
/// Display text when another approval was already waiting.
pub const APPROVAL_DENIED_CONCURRENT_PENDING: &str =
    "Another tool approval was already waiting, so this call was refused; retry it after that approval is answered";
/// Display text when the chat session was switched before approval.
pub const APPROVAL_DENIED_SESSION_SWITCHED: &str = "The chat session was switched before approval";
/// Display text when no approval resolver is available.
pub const APPROVAL_DENIED_NO_RESOLVER: &str = "approval required but no approval resolver is available";
/// Display text of a channel approval gate denial.
pub const APPROVAL_DENIED_CHANNEL: &str = "approval denied";

impl ApprovalDenialCause {
    /// The stable snake_case name recorded in the approval row.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserRejected => "user_rejected",
            Self::NoAnswer => "no_answer",
            Self::NoApprovalSurface => "no_approval_surface",
            Self::NoTerminal => "no_terminal",
            Self::PromptUnavailable => "prompt_unavailable",
            Self::ConcurrentPending => "concurrent_pending",
            Self::SessionSwitched => "session_switched",
            Self::NoResolver => "no_resolver",
            Self::ChannelDenied => "channel_denied",
            Self::LedgerRefused => "ledger_refused",
        }
    }

    /// The fixed display text of this cause. `LedgerRefused` has none: its
    /// text is the ledger's own error, carried in `Denied.reason`.
    #[must_use]
    pub const fn fixed_reason(self) -> Option<&'static str> {
        match self {
            Self::UserRejected => Some(APPROVAL_DENIED_USER_REJECTED),
            Self::NoAnswer => Some(APPROVAL_DENIED_NO_ANSWER),
            Self::NoApprovalSurface => Some(APPROVAL_DENIED_NO_APPROVAL_SURFACE),
            Self::NoTerminal => Some(APPROVAL_DENIED_NO_TERMINAL),
            Self::PromptUnavailable => Some(APPROVAL_DENIED_PROMPT_UNAVAILABLE),
            Self::ConcurrentPending => Some(APPROVAL_DENIED_CONCURRENT_PENDING),
            Self::SessionSwitched => Some(APPROVAL_DENIED_SESSION_SWITCHED),
            Self::NoResolver => Some(APPROVAL_DENIED_NO_RESOLVER),
            Self::ChannelDenied => Some(APPROVAL_DENIED_CHANNEL),
            Self::LedgerRefused => None,
        }
    }
}

/// How one approval request was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalVerdict {
    Approved,
    Denied,
    Cancelled,
}

impl ApprovalVerdict {
    /// The stable snake_case name recorded in the approval row.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The model-visible result of a call that was denied before it ran.
#[must_use]
pub fn approval_denied_model_content(reason: &str, tool: &str) -> String {
    format!("Denied: {reason}. The {tool} call did not run and produced no output.")
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
            cause: ApprovalDenialCause::NoResolver,
            reason: APPROVAL_DENIED_NO_RESOLVER.to_string(),
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
    /// The typed reason of an `ApprovalDenied` outcome; `None` for every
    /// other status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_denial_cause: Option<ApprovalDenialCause>,
    /// Typed pre-approval refusal, projected only by rf live frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal_cause: Option<ToolCallRefusalReason>,
    /// A `Denied` call refused by the execution policy or the autonomy level,
    /// as opposed to an admission, surface or budget refusal. Never stored.
    #[serde(skip)]
    pub(crate) permission_refused: bool,
}

impl ToolExecutionOutcome {
    const fn with_permission_refusal(mut self) -> Self {
        self.permission_refused = true;
        self
    }

    const fn with_refusal_cause(mut self, cause: ToolCallRefusalReason) -> Self {
        self.refusal_cause = Some(cause);
        self
    }

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

/// Digest of one tool call's session, semantic turn and idempotency key: the
/// scope shared by its execution-ledger and approval-event keys.
fn tool_execution_scope_digest(context: &ToolExecutionContext, idempotency_key: &str) -> String {
    let scope = format!(
        "{}\0{}\0{}",
        context.envelope.session_key, context.semantic_turn_id, idempotency_key
    );
    format!("{:x}", Sha256::digest(scope.as_bytes()))
}

fn tool_execution_ledger_keys(context: &ToolExecutionContext, idempotency_key: &str) -> (String, String) {
    let digest = tool_execution_scope_digest(context, idempotency_key);
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


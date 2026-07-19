//! Canonical tool execution contract.
//!
//! This module owns the fixed application pipeline for native tools and MCP
//! aliases. Existing [`Tool`] implementations remain adapter-owned during the
//! migration; callers submit a typed command and runtime context instead of
//! resolving and invoking adapters directly.

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
}

/// Small raw execution port consumed by the application service.
///
/// Backends expose descriptors and invocation only; policy, approval, execution
/// preparation, auditing, and typed outcomes remain service-owned.
#[async_trait]
pub trait ToolBackend: Send + Sync {
    fn root_name(&self) -> &str;
    fn specs(&self) -> Vec<ToolSpec>;
    fn supports_name(&self, public_name: &str) -> bool;
    fn tier(&self) -> ToolTier;
    fn categories(&self) -> Vec<ToolCategory>;
    fn adapter_kind(&self, public_name: &str) -> ToolAdapterKind;

    async fn invoke(
        &self,
        public_name: &str,
        arguments: serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<ToolResult>;
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
    fn root_name(&self) -> &str {
        &self.root_name
    }

    fn specs(&self) -> Vec<ToolSpec> {
        self.tool().map_or_else(Vec::new, Tool::specs)
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
        } else {
            ToolAdapterKind::Native
        }
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
    fn root_name(&self) -> &str {
        self.tool.name()
    }

    fn specs(&self) -> Vec<ToolSpec> {
        self.tool.specs()
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
        } else {
            ToolAdapterKind::Native
        }
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
    pub tier: ToolTier,
    pub categories: Vec<ToolCategory>,
    pub effect: ToolEffect,
    pub adapter: ToolAdapterKind,
    /// Evidence-backed runtime availability. Registered executable tools are
    /// `Ready`; `Healthy` is reserved for a positive runtime probe.
    pub availability: CapabilityAvailability,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutionContext {
    pub envelope: RuntimeEnvelope,
    pub chat_type: String,
    pub chat_id: String,
    pub semantic_turn_id: String,
}

impl ToolExecutionContext {
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
            envelope,
            chat_type: chat_type.into(),
            chat_id,
            semantic_turn_id,
        }
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

    fn sender(&self) -> &str {
        self.envelope.sender.as_deref().unwrap_or("unknown")
    }

    fn channel(&self) -> &str {
        self.envelope.channel.as_deref().unwrap_or("unknown")
    }
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
        decide_tool_execution(
            self.policy.as_ref(),
            &descriptor.public_name,
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
    PreparationDenied,
    UnknownTool,
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
    #[serde(default)]
    owner_token: String,
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
                    )
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
                            } else {
                                ToolAdapterKind::Native
                            }
                        },
                    )
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
) -> ToolDescriptor {
    let adapter = adapter_kind(&spec.name);
    let effect = if crate::security::policy::is_read_only_tool(&spec.name) {
        ToolEffect::Read
    } else {
        ToolEffect::Act
    };
    let adapter_label = match adapter {
        ToolAdapterKind::Native => "native",
        ToolAdapterKind::McpAlias => "MCP alias",
    };
    ToolDescriptor {
        public_name: spec.name,
        backend_name: backend_name.to_string(),
        description: spec.description,
        parameters: spec.parameters,
        tier,
        categories,
        effect,
        adapter,
        availability: CapabilityAvailability::ready(format!(
            "executable {adapter_label} backend '{backend_name}' is registered"
        )),
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
    #[must_use]
    pub fn with_idempotency_memory(mut self, memory: Arc<dyn Memory>) -> Self {
        if matches!(memory.name(), "sqlite" | "postgres" | "lucid") {
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
        let descriptor = self.catalog.descriptor(public_name)?.clone();
        let backend = self
            .backends
            .iter()
            .find(|backend| backend.root_name() == descriptor.backend_name && backend.supports_name(public_name))?
            .clone();
        Some(ResolvedTool { backend, descriptor })
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
    ) -> Result<Option<ToolExecutionLease>, ToolExecutionOutcome> {
        let Some(idempotency_key) = command.idempotency_key.as_deref() else {
            return Ok(None);
        };
        let Some(memory) = self.idempotency_memory.as_ref() else {
            if descriptor.effect == ToolEffect::Read {
                return Ok(None);
            }
            return Err(self.finish(
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
            ));
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
                    return Err(self.finish(
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
                    ));
                };
                if terminal.reservation.capability != command.name || terminal.reservation.input_sha256 != input_sha256
                {
                    return Err(self.finish(
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
                    ));
                }
                return Err(self.replay_outcome(command, context, input_sha256, terminal.outcome));
            }
            Ok(None) => {}
            Err(error) => {
                return Err(self.finish(
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
                ));
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
                        return Err(self.finish(
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
                        ));
                    };
                    if existing.capability != command.name || existing.input_sha256 != input_sha256 {
                        return Err(self.finish(
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
                        ));
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
                        return Err(self.finish(
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
                        ));
                    };
                    let abandoned = abandoned
                        .raw_payload_json
                        .as_deref()
                        .and_then(|payload| serde_json::from_str::<ToolExecutionReservation>(payload).ok());
                    if abandoned.as_ref().is_none_or(|abandoned| {
                        abandoned.owner_token != existing.owner_token
                            || abandoned.attempt != existing.attempt
                            || abandoned.capability != existing.capability
                            || abandoned.input_sha256 != existing.input_sha256
                    }) {
                        return Err(self.finish(
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
                        owner_token: Uuid::now_v7().to_string(),
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
                    let recorded = event
                        .raw_payload_json
                        .as_deref()
                        .and_then(|payload| serde_json::from_str::<ToolExecutionReservation>(payload).ok());
                    if recorded
                        .as_ref()
                        .is_none_or(|recorded| recorded.owner_token != reservation.owner_token)
                    {
                        return Err(self.finish(
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
                        ));
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
                    return Err(self.finish(
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
                    ));
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
        let payload = match serde_json::to_string(&terminal) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::error!(error = %error, "failed to encode idempotent tool terminal outcome");
                return;
            }
        };
        if let Err(error) = lease
            .memory
            .append_message_event(tool_execution_event_input(
                context,
                &lease.terminal_key,
                "tool.execution.finalized",
                format!("operation_id={} status={:?}", outcome.operation_id, outcome.status),
                payload,
            ))
            .await
        {
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
            recorded.owner_token != lease.reservation.owner_token
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
        let started = Instant::now();
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
        let decision = self.policy.decide(&descriptor, &context);
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
        if decision == ToolExecutionDecision::Ask {
            match self
                .approval
                .resolve(ToolApprovalRequest {
                    command: command.clone(),
                    descriptor: descriptor.clone(),
                    context: context.clone(),
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

        let arguments = match normalize_arguments(
            &command.arguments,
            &descriptor,
            &context,
            runtime_approval_granted,
            runtime_grant,
        ) {
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
            Ok(lease) => lease,
            Err(outcome) => return outcome,
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

        // The resolved Arc is immutable for the in-flight call even if a
        // dynamic catalog refresh changes future descriptor snapshots.
        let backend = resolved.backend;
        let tool_future = backend.invoke(&command.name, arguments, cancellation.clone());
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
        let outcome = match (raw_result, abandonment_error) {
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
            (Some(Ok(result)), _) if is_tool_cancelled_result(&result) => self.finish(
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
            ),
            (Some(Ok(result)), _) => {
                let (status, model_content, error) = if result.success {
                    (ToolExecutionStatus::Succeeded, result.output.clone(), None)
                } else {
                    let error = result.error.clone().unwrap_or_else(|| result.output.clone());
                    let hint = super::error_hints::recovery_hint(&command.name, &error);
                    let model_content = if hint.is_empty() {
                        format!("Error: {error}")
                    } else {
                        format!("Error: {error}\n{hint}")
                    };
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
                    ToolExecutionStatus::Failed,
                    model_content,
                    None,
                    Some(error),
                    Some(preparation),
                    started,
                )
            }
        };
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
        attempt_id: None,
        lease_epoch: None,
        config_generation_id: context.envelope.config_generation_id,
        config_source_revision: context.envelope.config_source_revision.clone(),
        content,
        raw_payload_json: Some(raw_payload_json),
        visibility: context.envelope.visibility.clone(),
    }
}

fn normalize_arguments(
    arguments: &serde_json::Value,
    descriptor: &ToolDescriptor,
    context: &ToolExecutionContext,
    runtime_approval_granted: bool,
    runtime_grant: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mut arguments = arguments.clone();
    let root = arguments
        .as_object_mut()
        .ok_or_else(|| "tool arguments must be a JSON object".to_string())?;

    if let Some(required) = descriptor
        .parameters
        .get("required")
        .and_then(serde_json::Value::as_array)
    {
        let missing = required
            .iter()
            .filter_map(serde_json::Value::as_str)
            .filter(|name| !root.contains_key(*name))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(format!("missing required tool parameters: {}", missing.join(", ")));
        }
    }

    root.remove("_zc_scope");
    root.remove("_zc_scope_trusted");
    root.remove("_prx_scope_trusted");
    root.remove(RUNTIME_APPROVAL_GRANTED_ARG);
    root.remove(RUNTIME_APPROVAL_GRANT_ARG);

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
    root.insert("_prx_scope_trusted".to_string(), serde_json::Value::Bool(false));
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
mod tests {
    use super::*;
    use crate::security::policy::AutonomyLevel;
    use crate::tools::traits::TOOL_EXECUTION_CANCELLED;
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct RecordingTool {
        root_name: &'static str,
        aliases: Vec<&'static str>,
        calls: Arc<AtomicUsize>,
        arguments: Arc<Mutex<Vec<serde_json::Value>>>,
        stages: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl Tool for RecordingTool {
        fn name(&self) -> &str {
            self.root_name
        }

        fn description(&self) -> &str {
            "recording tool"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object", "required":["value"]})
        }

        fn specs(&self) -> Vec<ToolSpec> {
            std::iter::once(self.root_name)
                .chain(self.aliases.iter().copied())
                .map(|name| ToolSpec {
                    name: name.to_string(),
                    description: format!("spec for {name}"),
                    parameters: self.parameters_schema(),
                })
                .collect()
        }

        fn supports_name(&self, name: &str) -> bool {
            name == self.root_name || self.aliases.contains(&name)
        }

        async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
            self.execute_named(self.root_name, args).await
        }

        async fn execute_named(&self, name: &str, args: serde_json::Value) -> anyhow::Result<ToolResult> {
            assert!(self.supports_name(name));
            self.stages.lock().push("execute");
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.arguments.lock().push(args);
            Ok(ToolResult {
                success: true,
                output: format!("executed:{name}"),
                error: None,
            })
        }
    }

    struct PollRecordingBackend {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ToolBackend for PollRecordingBackend {
        fn root_name(&self) -> &str {
            "native_write"
        }

        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "native_write".to_string(),
                description: "records whether invoke was polled".to_string(),
                parameters: serde_json::json!({"type":"object", "required":["value"]}),
            }]
        }

        fn supports_name(&self, public_name: &str) -> bool {
            public_name == "native_write"
        }

        fn tier(&self) -> ToolTier {
            ToolTier::Standard
        }

        fn categories(&self) -> Vec<ToolCategory> {
            Vec::new()
        }

        fn adapter_kind(&self, _public_name: &str) -> ToolAdapterKind {
            ToolAdapterKind::Native
        }

        async fn invoke(
            &self,
            _public_name: &str,
            _arguments: serde_json::Value,
            cancellation: Option<CancellationToken>,
        ) -> anyhow::Result<ToolResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if cancellation.as_ref().is_some_and(CancellationToken::is_cancelled) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(TOOL_EXECUTION_CANCELLED.to_string()),
                });
            }
            Ok(ToolResult {
                success: true,
                output: "executed".to_string(),
                error: None,
            })
        }
    }

    struct CancellationAwareBackend {
        calls: Arc<AtomicUsize>,
        started: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl ToolBackend for CancellationAwareBackend {
        fn root_name(&self) -> &str {
            "native_write"
        }

        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "native_write".to_string(),
                description: "waits for cooperative cancellation".to_string(),
                parameters: serde_json::json!({"type":"object", "required":["value"]}),
            }]
        }

        fn supports_name(&self, public_name: &str) -> bool {
            public_name == "native_write"
        }

        fn tier(&self) -> ToolTier {
            ToolTier::Standard
        }

        fn categories(&self) -> Vec<ToolCategory> {
            Vec::new()
        }

        fn adapter_kind(&self, _public_name: &str) -> ToolAdapterKind {
            ToolAdapterKind::Native
        }

        async fn invoke(
            &self,
            _public_name: &str,
            _arguments: serde_json::Value,
            cancellation: Option<CancellationToken>,
        ) -> anyhow::Result<ToolResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            let cancellation = cancellation.expect("test backend requires cancellation token");
            cancellation.cancelled().await;
            Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(TOOL_EXECUTION_CANCELLED.to_string()),
            })
        }
    }

    struct FixedPolicy {
        decision: ToolExecutionDecision,
        stages: Arc<Mutex<Vec<&'static str>>>,
    }

    impl EffectPolicy for FixedPolicy {
        fn decide(&self, _descriptor: &ToolDescriptor, _context: &ToolExecutionContext) -> ToolExecutionDecision {
            self.stages.lock().push("policy");
            self.decision
        }
    }

    struct FixedApproval {
        decision: ToolApprovalDecision,
        stages: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl ApprovalStrategy for FixedApproval {
        async fn resolve(&self, _request: ToolApprovalRequest) -> ToolApprovalDecision {
            self.stages.lock().push("approval");
            self.decision.clone()
        }
    }

    struct RecordingPreparation {
        stages: Arc<Mutex<Vec<&'static str>>>,
        allowed: bool,
    }

    #[async_trait]
    impl ToolExecutionPreparation for RecordingPreparation {
        async fn prepare(
            &self,
            _descriptor: &ToolDescriptor,
            _command: &ToolExecutionCommand,
            _context: &ToolExecutionContext,
        ) -> Result<ToolExecutionPermit, String> {
            self.stages.lock().push("preparation");
            self.allowed
                .then(|| ToolExecutionPermit {
                    strategy: "test".to_string(),
                })
                .ok_or_else(|| "blocked".to_string())
        }
    }

    struct RecordingAudit {
        records: Arc<Mutex<Vec<ToolExecutionAuditRecord>>>,
        stages: Arc<Mutex<Vec<&'static str>>>,
    }

    impl ToolExecutionAuditSink for RecordingAudit {
        fn record(&self, record: ToolExecutionAuditRecord) {
            self.stages.lock().push("audit");
            self.records.lock().push(record);
        }
    }

    struct Fixture {
        calls: Arc<AtomicUsize>,
        arguments: Arc<Mutex<Vec<serde_json::Value>>>,
        stages: Arc<Mutex<Vec<&'static str>>>,
        records: Arc<Mutex<Vec<ToolExecutionAuditRecord>>>,
    }

    impl Fixture {
        fn service(
            &self,
            root_name: &'static str,
            aliases: Vec<&'static str>,
            decision: ToolExecutionDecision,
            approval: ToolApprovalDecision,
            preparation_allowed: bool,
        ) -> ToolExecutionService {
            let tool = RecordingTool {
                root_name,
                aliases,
                calls: Arc::clone(&self.calls),
                arguments: Arc::clone(&self.arguments),
                stages: Arc::clone(&self.stages),
            };
            ToolExecutionService::new(
                vec![Arc::new(tool)],
                Arc::new(FixedPolicy {
                    decision,
                    stages: Arc::clone(&self.stages),
                }),
                Arc::new(FixedApproval {
                    decision: approval,
                    stages: Arc::clone(&self.stages),
                }),
                Arc::new(RecordingPreparation {
                    stages: Arc::clone(&self.stages),
                    allowed: preparation_allowed,
                }),
                Arc::new(RecordingAudit {
                    records: Arc::clone(&self.records),
                    stages: Arc::clone(&self.stages),
                }),
            )
        }
    }

    fn fixture() -> Fixture {
        Fixture {
            calls: Arc::new(AtomicUsize::new(0)),
            arguments: Arc::new(Mutex::new(Vec::new())),
            stages: Arc::new(Mutex::new(Vec::new())),
            records: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn context() -> ToolExecutionContext {
        ToolExecutionContext::new(
            RuntimeEnvelope::agent("workspace-a", "run-a")
                .with_channel("terminal")
                .with_sender("alice")
                .with_owner_id("owner-a")
                .with_task_id("task-a")
                .with_parent_run_id("run-parent"),
            "dm",
        )
        .with_chat_id("chat-a")
    }

    fn approved() -> ToolApprovalDecision {
        ToolApprovalDecision::Approved {
            runtime_approval_granted: true,
            runtime_grant: Some(serde_json::json!({"trusted":"grant"})),
        }
    }

    #[test]
    fn catalog_is_the_execution_snapshot_and_reports_registered_backends_ready() {
        let fixture = fixture();
        let service = fixture.service(
            "mcp_call",
            vec!["mcp__demo__echo"],
            ToolExecutionDecision::Allow,
            approved(),
            true,
        );

        assert_eq!(service.catalog().descriptors(), service.descriptors());
        let root = service.catalog().descriptor("mcp_call").expect("root descriptor");
        let alias = service
            .catalog()
            .descriptor("mcp__demo__echo")
            .expect("MCP alias descriptor");
        assert_eq!(
            root.availability.level,
            crate::capability::CapabilityAvailabilityLevel::Ready
        );
        assert_eq!(
            alias.availability.level,
            crate::capability::CapabilityAvailabilityLevel::Ready
        );
        assert_eq!(root.adapter, ToolAdapterKind::Native);
        assert_eq!(alias.adapter, ToolAdapterKind::McpAlias);
        assert!(alias.availability.reason.contains("backend 'mcp_call' is registered"));
    }

    #[tokio::test]
    async fn native_pipeline_is_ordered_and_injects_only_trusted_runtime_fields() {
        let fixture = fixture();
        let service = fixture.service("native_write", Vec::new(), ToolExecutionDecision::Ask, approved(), true);
        let outcome = service
            .execute(
                ToolExecutionCommand::new(
                    "native_write",
                    serde_json::json!({
                        "value":"ok",
                        "_zc_scope":{"sender":"attacker"},
                        "_zc_approval_grant":{"forged":true}
                    }),
                ),
                context(),
                None,
            )
            .await;

        assert_eq!(outcome.status, ToolExecutionStatus::Succeeded);
        assert_eq!(
            &*fixture.stages.lock(),
            &["policy", "approval", "preparation", "execute", "audit"]
        );
        let args = fixture.arguments.lock().first().cloned().unwrap();
        assert_eq!(
            args.pointer("/_zc_scope/sender").and_then(serde_json::Value::as_str),
            Some("alice")
        );
        assert_eq!(
            args.pointer("/_zc_scope/chat_id").and_then(serde_json::Value::as_str),
            Some("chat-a")
        );
        assert_eq!(
            args.pointer("/_zc_scope/task_id").and_then(serde_json::Value::as_str),
            Some("task-a")
        );
        assert_eq!(
            args.get("_zc_scope_trusted").and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            args.get(RUNTIME_APPROVAL_GRANT_ARG),
            Some(&serde_json::json!({"trusted":"grant"}))
        );
    }

    #[tokio::test]
    async fn idempotent_tool_execution_replays_persisted_terminal_without_reexecution() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
        let fixture = fixture();
        let first_service = fixture
            .service(
                "native_write",
                Vec::new(),
                ToolExecutionDecision::Allow,
                approved(),
                true,
            )
            .with_idempotency_memory(Arc::clone(&memory));
        let second_service = fixture
            .service(
                "native_write",
                Vec::new(),
                ToolExecutionDecision::Allow,
                approved(),
                true,
            )
            .with_idempotency_memory(memory);
        let command = || {
            ToolExecutionCommand::new("native_write", serde_json::json!({"value":"once"}))
                .with_operation_id("call-once")
                .with_idempotency_key("call-once")
        };

        let first = first_service.execute(command(), context(), None).await;
        let replay = second_service.execute(command(), context(), None).await;

        assert_eq!(first.status, ToolExecutionStatus::Succeeded);
        assert!(!first.replayed);
        assert_eq!(replay.status, ToolExecutionStatus::Succeeded);
        assert!(replay.replayed);
        assert_eq!(replay.model_content, first.model_content);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn same_provider_call_id_is_isolated_between_semantic_turns() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
        let fixture = fixture();
        let service = fixture
            .service(
                "native_write",
                Vec::new(),
                ToolExecutionDecision::Allow,
                approved(),
                true,
            )
            .with_idempotency_memory(memory);
        let command = || {
            ToolExecutionCommand::new("native_write", serde_json::json!({"value":"repeat"}))
                .with_idempotency_key("call_0")
        };

        let first = service
            .execute(command(), context().with_semantic_turn_id("turn-a"), None)
            .await;
        let second = service
            .execute(command(), context().with_semantic_turn_id("turn-b"), None)
            .await;

        assert_eq!(first.status, ToolExecutionStatus::Succeeded);
        assert_eq!(second.status, ToolExecutionStatus::Succeeded);
        assert!(!first.replayed);
        assert!(!second.replayed);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn idempotent_mutation_fails_closed_without_durable_ledger() {
        let fixture = fixture();
        let service = fixture.service(
            "native_write",
            Vec::new(),
            ToolExecutionDecision::Allow,
            approved(),
            true,
        );
        let command = || {
            ToolExecutionCommand::new("native_write", serde_json::json!({"value":"unsafe"}))
                .with_idempotency_key("call-without-ledger")
        };

        let first = service.execute(command(), context(), None).await;
        let second = service.execute(command(), context(), None).await;

        assert_eq!(first.status, ToolExecutionStatus::Indeterminate);
        assert_eq!(second.status, ToolExecutionStatus::Indeterminate);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancellation_before_reservation_allows_same_idempotency_key_to_retry() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
        let fixture = fixture();
        let service = fixture
            .service(
                "native_write",
                Vec::new(),
                ToolExecutionDecision::Allow,
                approved(),
                true,
            )
            .with_idempotency_memory(memory);
        let command = || {
            ToolExecutionCommand::new("native_write", serde_json::json!({"value":"after-cancel"}))
                .with_idempotency_key("cancel-before-reserve")
        };
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let cancelled = service.execute(command(), context(), Some(cancellation)).await;
        let retried = service.execute(command(), context(), None).await;

        assert_eq!(cancelled.status, ToolExecutionStatus::Cancelled);
        assert_eq!(retried.status, ToolExecutionStatus::Succeeded);
        assert!(!retried.replayed);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_after_reservation_abandons_before_backend_and_allows_retry() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
        let fixture = fixture();
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(TestPostReservationGate {
            armed: AtomicBool::new(true),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let service = Arc::new(
            ToolExecutionService::from_backends(
                vec![Arc::new(PollRecordingBackend {
                    calls: Arc::clone(&calls),
                })],
                Arc::new(FixedPolicy {
                    decision: ToolExecutionDecision::Allow,
                    stages: Arc::clone(&fixture.stages),
                }),
                Arc::new(FixedApproval {
                    decision: approved(),
                    stages: Arc::clone(&fixture.stages),
                }),
                Arc::new(RecordingPreparation {
                    stages: Arc::clone(&fixture.stages),
                    allowed: true,
                }),
                Arc::new(RecordingAudit {
                    records: Arc::clone(&fixture.records),
                    stages: Arc::clone(&fixture.stages),
                }),
            )
            .with_idempotency_memory(memory)
            .with_post_reservation_gate(Arc::clone(&gate)),
        );
        let command = || {
            ToolExecutionCommand::new("native_write", serde_json::json!({"value":"cancelled"}))
                .with_idempotency_key("cancel-after-reserve")
        };
        let cancellation = CancellationToken::new();
        let task = {
            let service = Arc::clone(&service);
            let cancellation = cancellation.clone();
            tokio::spawn(async move { service.execute(command(), context(), Some(cancellation)).await })
        };

        gate.entered.notified().await;
        cancellation.cancel();
        gate.release.notify_one();
        let cancelled = task.await.unwrap();
        let retried = service.execute(command(), context(), None).await;

        assert_eq!(cancelled.status, ToolExecutionStatus::Cancelled);
        assert!(!cancelled.replayed);
        assert_eq!(retried.status, ToolExecutionStatus::Succeeded);
        assert!(!retried.replayed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_after_backend_start_commits_one_terminal_and_never_reexecutes() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
        let fixture = fixture();
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let service = Arc::new(
            ToolExecutionService::from_backends(
                vec![Arc::new(CancellationAwareBackend {
                    calls: Arc::clone(&calls),
                    started: Arc::clone(&started),
                })],
                Arc::new(FixedPolicy {
                    decision: ToolExecutionDecision::Allow,
                    stages: Arc::clone(&fixture.stages),
                }),
                Arc::new(FixedApproval {
                    decision: approved(),
                    stages: Arc::clone(&fixture.stages),
                }),
                Arc::new(RecordingPreparation {
                    stages: Arc::clone(&fixture.stages),
                    allowed: true,
                }),
                Arc::new(RecordingAudit {
                    records: Arc::clone(&fixture.records),
                    stages: Arc::clone(&fixture.stages),
                }),
            )
            .with_idempotency_memory(memory),
        );
        let command = || {
            ToolExecutionCommand::new("native_write", serde_json::json!({"value":"cancelled"}))
                .with_idempotency_key("cancel-after-start")
        };
        let cancellation = CancellationToken::new();
        let task = {
            let service = Arc::clone(&service);
            let cancellation = cancellation.clone();
            tokio::spawn(async move { service.execute(command(), context(), Some(cancellation)).await })
        };

        started.notified().await;
        cancellation.cancel();
        let cancelled = task.await.unwrap();
        let replay = service.execute(command(), context(), None).await;

        assert_eq!(cancelled.status, ToolExecutionStatus::Cancelled);
        assert!(!cancelled.replayed);
        assert_eq!(replay.status, ToolExecutionStatus::Cancelled);
        assert!(replay.replayed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn idempotency_key_reuse_with_different_command_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
        let fixture = fixture();
        let service = fixture
            .service(
                "native_write",
                Vec::new(),
                ToolExecutionDecision::Allow,
                approved(),
                true,
            )
            .with_idempotency_memory(memory);

        let first = service
            .execute(
                ToolExecutionCommand::new("native_write", serde_json::json!({"value":"first"}))
                    .with_idempotency_key("reused-key"),
                context(),
                None,
            )
            .await;
        let conflict = service
            .execute(
                ToolExecutionCommand::new("native_write", serde_json::json!({"value":"second"}))
                    .with_idempotency_key("reused-key"),
                context(),
                None,
            )
            .await;

        assert_eq!(first.status, ToolExecutionStatus::Succeeded);
        assert_eq!(conflict.status, ToolExecutionStatus::IdempotencyConflict);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn orphaned_idempotency_reservation_refuses_duplicate_side_effect() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
        let fixture = fixture();
        let service = fixture
            .service(
                "native_write",
                Vec::new(),
                ToolExecutionDecision::Allow,
                approved(),
                true,
            )
            .with_idempotency_memory(Arc::clone(&memory));
        let command = ToolExecutionCommand::new("native_write", serde_json::json!({"value":"uncertain"}))
            .with_operation_id("recovery-call")
            .with_idempotency_key("recovery-key");
        let execution_context = context();
        let input_sha256 = format!("{:x}", Sha256::digest(command.arguments.to_string().as_bytes()));
        let (reservation_key, _) = tool_execution_ledger_keys(&execution_context, "recovery-key");
        let reservation = ToolExecutionReservation {
            operation_id: "crashed-owner".to_string(),
            owner_token: "crashed-owner-token".to_string(),
            attempt: 0,
            capability: command.name.clone(),
            input_sha256,
        };
        memory
            .append_message_event(tool_execution_event_input(
                &execution_context,
                &reservation_key,
                "tool.execution.reserved",
                "crashed owner".to_string(),
                serde_json::to_string(&reservation).unwrap(),
            ))
            .await
            .unwrap();

        let outcome = service.execute(command, execution_context, None).await;

        assert_eq!(outcome.status, ToolExecutionStatus::Indeterminate);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn policy_deny_prevents_approval_preparation_and_execution_but_audits() {
        let fixture = fixture();
        let service = fixture.service(
            "native_write",
            Vec::new(),
            ToolExecutionDecision::Deny,
            approved(),
            true,
        );
        let outcome = service
            .execute(
                ToolExecutionCommand::new("native_write", serde_json::json!({"value":"blocked"})),
                context(),
                None,
            )
            .await;

        assert_eq!(outcome.status, ToolExecutionStatus::Denied);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert_eq!(&*fixture.stages.lock(), &["policy", "audit"]);
        assert_eq!(fixture.records.lock().len(), 1);
    }

    #[tokio::test]
    async fn approval_denial_is_typed_and_fail_closed() {
        let fixture = fixture();
        let service = fixture.service(
            "native_write",
            Vec::new(),
            ToolExecutionDecision::Ask,
            ToolApprovalDecision::Denied {
                reason: "operator denied".to_string(),
            },
            true,
        );
        let outcome = service
            .execute(
                ToolExecutionCommand::new("native_write", serde_json::json!({"value":"blocked"})),
                context(),
                None,
            )
            .await;

        assert_eq!(outcome.status, ToolExecutionStatus::ApprovalDenied);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert_eq!(&*fixture.stages.lock(), &["policy", "approval", "audit"]);
    }

    #[tokio::test]
    async fn preparation_denial_prevents_adapter_execution() {
        let fixture = fixture();
        let service = fixture.service(
            "native_read",
            Vec::new(),
            ToolExecutionDecision::Allow,
            approved(),
            false,
        );
        let outcome = service
            .execute(
                ToolExecutionCommand::new("native_read", serde_json::json!({"value":"blocked"})),
                context(),
                None,
            )
            .await;

        assert_eq!(outcome.status, ToolExecutionStatus::PreparationDenied);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert_eq!(&*fixture.stages.lock(), &["policy", "preparation", "audit"]);
    }

    #[tokio::test]
    async fn mcp_alias_resolves_one_adapter_and_preserves_public_descriptor() {
        let fixture = fixture();
        let tool = RecordingTool {
            root_name: "mcp_call",
            aliases: vec!["mcp__docs__search"],
            calls: Arc::clone(&fixture.calls),
            arguments: Arc::clone(&fixture.arguments),
            stages: Arc::clone(&fixture.stages),
        };
        let service = ToolExecutionService::from_shared_boxed_registry(
            Arc::new(vec![Box::new(tool) as Box<dyn Tool>]),
            Arc::new(FixedPolicy {
                decision: ToolExecutionDecision::Allow,
                stages: Arc::clone(&fixture.stages),
            }),
            Arc::new(FixedApproval {
                decision: approved(),
                stages: Arc::clone(&fixture.stages),
            }),
            Arc::new(RecordingPreparation {
                stages: Arc::clone(&fixture.stages),
                allowed: true,
            }),
            Arc::new(RecordingAudit {
                records: Arc::clone(&fixture.records),
                stages: Arc::clone(&fixture.stages),
            }),
        );
        let outcome = service
            .execute(
                ToolExecutionCommand::new("mcp__docs__search", serde_json::json!({"value":"rust"})),
                context(),
                None,
            )
            .await;

        let descriptor = outcome.descriptor.unwrap();
        assert_eq!(outcome.status, ToolExecutionStatus::Succeeded);
        assert_eq!(descriptor.public_name, "mcp__docs__search");
        assert_eq!(descriptor.backend_name, "mcp_call");
        assert_eq!(descriptor.adapter, ToolAdapterKind::McpAlias);
        assert_eq!(descriptor.effect, ToolEffect::Act);
        assert_eq!(outcome.model_content, "executed:mcp__docs__search");
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalid_arguments_are_typed_and_adapter_is_not_called() {
        let fixture = fixture();
        let service = fixture.service(
            "native_read",
            Vec::new(),
            ToolExecutionDecision::Allow,
            approved(),
            true,
        );
        let outcome = service
            .execute(
                ToolExecutionCommand::new("native_read", serde_json::json!({})),
                context(),
                None,
            )
            .await;

        assert_eq!(outcome.status, ToolExecutionStatus::InvalidArguments);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert_eq!(&*fixture.stages.lock(), &["policy", "preparation", "audit"]);
    }

    #[tokio::test]
    async fn unknown_tool_is_typed_and_still_audited() {
        let fixture = fixture();
        let service = fixture.service(
            "native_read",
            Vec::new(),
            ToolExecutionDecision::Allow,
            approved(),
            true,
        );
        let outcome = service
            .execute(
                ToolExecutionCommand::new("missing", serde_json::json!({"value":"x"})),
                context(),
                None,
            )
            .await;

        assert_eq!(outcome.status, ToolExecutionStatus::UnknownTool);
        assert_eq!(&*fixture.stages.lock(), &["audit"]);
        assert_eq!(
            fixture.records.lock().first().unwrap().status,
            ToolExecutionStatus::UnknownTool
        );
    }

    #[test]
    fn security_policy_adapter_preserves_all_autonomy_modes() {
        let descriptor = |name: &str| ToolDescriptor {
            public_name: name.to_string(),
            backend_name: name.to_string(),
            description: name.to_string(),
            parameters: serde_json::json!({"type":"object"}),
            tier: ToolTier::Core,
            categories: Vec::new(),
            effect: if crate::security::policy::is_read_only_tool(name) {
                ToolEffect::Read
            } else {
                ToolEffect::Act
            },
            adapter: ToolAdapterKind::Native,
            availability: CapabilityAvailability::ready("test backend is registered"),
        };
        let policy = |autonomy| {
            SecurityEffectPolicy::new(Arc::new(SecurityPolicy {
                autonomy,
                ..SecurityPolicy::default()
            }))
        };

        let assert_projection = |autonomy, name, expected| {
            let policy = SecurityPolicy {
                autonomy,
                ..SecurityPolicy::default()
            };
            assert_eq!(
                decide_tool_execution(&policy, name, "alice", "terminal", "dm"),
                expected
            );
            assert_eq!(
                SecurityEffectPolicy::new(Arc::new(policy)).decide(&descriptor(name), &context()),
                expected
            );
        };

        assert_projection(AutonomyLevel::ReadOnly, "file_read", ToolExecutionDecision::Allow);
        assert_projection(AutonomyLevel::ReadOnly, "native_write", ToolExecutionDecision::Deny);
        assert_projection(AutonomyLevel::Supervised, "native_write", ToolExecutionDecision::Ask);
        assert_projection(AutonomyLevel::Full, "native_write", ToolExecutionDecision::Allow);

        assert_eq!(
            policy(AutonomyLevel::ReadOnly).decide(&descriptor("file_read"), &context()),
            ToolExecutionDecision::Allow
        );
        assert_eq!(
            policy(AutonomyLevel::ReadOnly).decide(&descriptor("native_write"), &context()),
            ToolExecutionDecision::Deny
        );
        assert_eq!(
            policy(AutonomyLevel::Supervised).decide(&descriptor("native_write"), &context()),
            ToolExecutionDecision::Ask
        );
        assert_eq!(
            policy(AutonomyLevel::Full).decide(&descriptor("native_write"), &context()),
            ToolExecutionDecision::Allow
        );
    }

    #[tokio::test]
    async fn default_full_policy_executes_ready_tool_without_prompt_and_keeps_typed_audit() {
        let fixture = fixture();
        let tool = RecordingTool {
            root_name: "native_write",
            aliases: Vec::new(),
            calls: Arc::clone(&fixture.calls),
            arguments: Arc::clone(&fixture.arguments),
            stages: Arc::clone(&fixture.stages),
        };
        let service = ToolExecutionService::new(
            vec![Arc::new(tool)],
            Arc::new(SecurityEffectPolicy::new(Arc::new(SecurityPolicy::default()))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(RecordingPreparation {
                stages: Arc::clone(&fixture.stages),
                allowed: true,
            }),
            Arc::new(RecordingAudit {
                records: Arc::clone(&fixture.records),
                stages: Arc::clone(&fixture.stages),
            }),
        );

        let outcome = service
            .execute(
                ToolExecutionCommand::new("native_write", serde_json::json!({"value":"ok"})),
                context(),
                None,
            )
            .await;

        assert_eq!(outcome.status, ToolExecutionStatus::Succeeded);
        assert_eq!(outcome.decision, Some(ToolExecutionDecision::Allow));
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        let records = fixture.records.lock();
        let record = records.first().expect("typed audit record");
        assert_eq!(record.decision, Some(ToolExecutionDecision::Allow));
        assert_eq!(record.status, ToolExecutionStatus::Succeeded);
        assert_eq!(record.effect, Some(ToolEffect::Act));
    }

    struct HangingTool;

    #[async_trait]
    impl Tool for HangingTool {
        fn name(&self) -> &str {
            "hanging_tool"
        }

        fn description(&self) -> &str {
            "waits until cancelled"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }

        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn cancellation_is_typed_and_audited_once() {
        let fixture = fixture();
        let service = ToolExecutionService::new(
            vec![Arc::new(HangingTool)],
            Arc::new(FixedPolicy {
                decision: ToolExecutionDecision::Allow,
                stages: Arc::clone(&fixture.stages),
            }),
            Arc::new(FixedApproval {
                decision: approved(),
                stages: Arc::clone(&fixture.stages),
            }),
            Arc::new(RecordingPreparation {
                stages: Arc::clone(&fixture.stages),
                allowed: true,
            }),
            Arc::new(RecordingAudit {
                records: Arc::clone(&fixture.records),
                stages: Arc::clone(&fixture.stages),
            }),
        );
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let outcome = service
            .execute(
                ToolExecutionCommand::new("hanging_tool", serde_json::json!({})),
                context(),
                Some(cancellation),
            )
            .await;

        assert_eq!(outcome.status, ToolExecutionStatus::Cancelled);
        assert_eq!(&*fixture.stages.lock(), &["policy", "preparation", "audit"]);
        assert_eq!(fixture.records.lock().len(), 1);
    }
}

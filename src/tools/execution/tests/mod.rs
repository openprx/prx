#![cfg(test)]
mod collection_arguments;
mod exact_call_observation;
use super::*;
use crate::security::policy::AutonomyLevel;
use crate::tools::traits::TOOL_EXECUTION_CANCELLED;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

mod composition;
mod contracts;
mod flat_contracts;
mod idempotency;
mod pipeline;

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
        serde_json::json!({
            "type": "object",
            "properties": {"value": {}},
            "required": ["value"]
        })
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

struct LateAliasTool {
    enabled: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for LateAliasTool {
    fn name(&self) -> &str {
        "mcp_call"
    }

    fn description(&self) -> &str {
        "late alias fixture"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object", "properties": {}})
    }

    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = vec![self.spec()];
        if self.enabled.load(AtomicOrdering::Acquire) {
            specs.push(ToolSpec {
                name: "mcp__late__navigate".to_string(),
                description: "runtime-discovered alias".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"url": {"type": "string"}},
                    "required": ["url"]
                }),
            });
        }
        specs
    }

    fn supports_name(&self, name: &str) -> bool {
        name == self.name() || (name == "mcp__late__navigate" && self.enabled.load(AtomicOrdering::Acquire))
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.execute_named(self.name(), args).await
    }

    async fn execute_named(&self, name: &str, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if !self.supports_name(name) {
            anyhow::bail!("unsupported fixture alias")
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
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

/// Records what the runtime work registry looked like from *inside* the
/// backend call, which is the only place that can prove the production
/// execute path registers a row and scopes it as the current work item.
struct RegistryObservingBackend {
    observed: Arc<Mutex<Option<(crate::runtime::registry::WorkId, String, String)>>>,
    started: Arc<tokio::sync::Notify>,
    wait_for_cancellation: bool,
}

#[async_trait]
impl ToolBackend for RegistryObservingBackend {
    fn root_name(&self) -> &str {
        "native_write"
    }

    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "native_write".to_string(),
            description: "observes the runtime work registry".to_string(),
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
        if let Some(id) = crate::runtime::registry::current_work_id() {
            if let Some(row) = crate::runtime::registry::snapshot(id) {
                *self.observed.lock() = Some((
                    id,
                    row.name.to_string(),
                    row.run_id.map(|run_id| run_id.to_string()).unwrap_or_default(),
                ));
            }
        }
        self.started.notify_one();
        if self.wait_for_cancellation {
            let cancellation = cancellation.expect("test: execute must supply a per-call token");
            cancellation.cancelled().await;
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

/// A tool whose published contract expresses every constraint through
/// composition, on values that are not objects: a scalar `allOf`, an array
/// `allOf` with its own item composition, and a scalar `if`/`then`/`else`.
///
/// Composition used to be compiled and then skipped for anything but an
/// object, so a contract shaped like this validated nothing at all. The
/// same fixture also keeps the document-root conditional and local-ref
/// paths on the production executor boundary.
struct CompositionTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CompositionTool {
    fn name(&self) -> &str {
        "composition_tool"
    }

    fn description(&self) -> &str {
        "composition fixture"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {
                    "allOf": [{"type": "integer", "minimum": 1}],
                    "description": "Page size."
                },
                "tags": {
                    "allOf": [
                        {"type": "array", "minItems": 1},
                        {"items": {"allOf": [{"type": "string", "minLength": 2}]}}
                    ],
                    "description": "Labels to attach."
                },
                "window": {
                    "type": "integer",
                    "if": {"minimum": 10},
                    "then": {"maximum": 20},
                    "else": {"minimum": 0},
                    "description": "Lookback window."
                },
                "profile": {
                    "allOf": [{
                        "type": "object",
                        "properties": {"name": {"type": "string", "minLength": 2}},
                        "required": ["name"]
                    }]
                },
                "action": {"type": "string", "enum": ["read", "write"]},
                "content": {"type": "string"},
                "target": {"$ref": "#/$defs/Target"}
            },
            "if": {
                "properties": {"action": {"const": "write"}},
                "required": ["action"]
            },
            "then": {"required": ["content"]},
            "else": {},
            "$defs": {
                "Target": {
                    "type": "object",
                    "properties": {"id": {"type": "string", "minLength": 3}},
                    "required": ["id"]
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.execute_named(self.name(), args).await
    }

    async fn execute_named(&self, _name: &str, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            success: true,
            output: "executed".to_string(),
            error: None,
        })
    }
}

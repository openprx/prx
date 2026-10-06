use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::turn_contracts::{
    ApprovalResolver, ChatMode, ScopeContext, ToolCallNotification, ToolLoopRuntimeAdapter,
};
use crate::approval::ApprovalManager;
use crate::hooks::HookManager;
use crate::memory::{Memory, MemoryFabric, MessageEventScope};
use crate::observability::Observer;
use crate::providers::Provider;
use crate::tools::Tool;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A neutral, crate-library-level sink that provisions the per-run event
/// streams for a background sub-agent (used by `tools::sessions_spawn`).
///
/// The actual wiring (middle channel + drainer + chat ring buffers) lives in the
/// binary crate (`chat::sessions::event`), but `sessions_spawn` is a *library*
/// module and must not depend on `chat`. So the binary constructs a
/// [`SpawnEventSink`] from a closure that, given a run id, creates the run's
/// `on_delta` / `on_tool_call` senders (and spawns its drainer); the library
/// only ever invokes that closure. This inverts the dependency without pulling
/// `chat` into the library crate.
#[derive(Clone)]
pub struct SpawnEventSink {
    #[allow(clippy::type_complexity)]
    factory: Arc<
        dyn Fn(
                &str,
            ) -> (
                tokio::sync::mpsc::Sender<String>,
                tokio::sync::mpsc::Sender<ToolCallNotification>,
            ) + Send
            + Sync,
    >,
}

impl SpawnEventSink {
    /// Build a sink from a per-run stream factory.
    ///
    /// `factory(run_id)` must return the `(on_delta, on_tool_call)` senders for
    /// that run and arrange for them to be drained (the chat side spawns the
    /// drainer there). It is called exactly once per spawned task-mode run.
    pub fn new<F>(factory: F) -> Self
    where
        F: Fn(
                &str,
            ) -> (
                tokio::sync::mpsc::Sender<String>,
                tokio::sync::mpsc::Sender<ToolCallNotification>,
            ) + Send
            + Sync
            + 'static,
    {
        Self {
            factory: Arc::new(factory),
        }
    }

    /// Provision the `(on_delta, on_tool_call)` senders for one run.
    #[must_use]
    pub fn streams_for(
        &self,
        run_id: &str,
    ) -> (
        tokio::sync::mpsc::Sender<String>,
        tokio::sync::mpsc::Sender<ToolCallNotification>,
    ) {
        (self.factory)(run_id)
    }
}

impl std::fmt::Debug for SpawnEventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpawnEventSink")
    }
}

/// A crate-library-level factory that mints a per-run [`ApprovalResolver`] for a
/// background sub-agent (used by `tools::sessions_spawn`).
///
/// Mirrors the [`SpawnEventSink`] inversion-of-control: `sessions_spawn` is a
/// *library* module and must not depend on `chat`, so the binary constructs the
/// resolver factory (capturing the chat-side event channel + decision registry)
/// and the library only invokes it. The factory is called at most once per
/// spawned task-mode run, with that run's id, so the returned resolver can tag
/// its NeedsInput event / registry update with the correct [`SessionId`].
///
/// **Behaviour-preserving for non-chat callers:** only the chat `/bg` path
/// (which already attaches a [`SpawnEventSink`]) attaches a factory. The
/// channels / gateway spawn paths attach `None`, so their background sub-agents
/// keep the historical auto-fail-on-gate behaviour (no human suspension).
#[derive(Clone)]
pub(crate) struct SpawnApprovalResolverFactory {
    #[allow(clippy::type_complexity)]
    factory: Arc<dyn Fn(&str) -> Arc<dyn ApprovalResolver> + Send + Sync>,
}

impl SpawnApprovalResolverFactory {
    /// Build a factory from a per-run resolver constructor (chat's
    /// `sessions::approval::build_resolver_factory`).
    pub(crate) fn new<F>(factory: F) -> Self
    where
        F: Fn(&str) -> Arc<dyn ApprovalResolver> + Send + Sync + 'static,
    {
        Self {
            factory: Arc::new(factory),
        }
    }

    /// Mint the [`ApprovalResolver`] for one run.
    #[must_use]
    pub(crate) fn resolver_for(&self, run_id: &str) -> Arc<dyn ApprovalResolver> {
        (self.factory)(run_id)
    }
}

impl std::fmt::Debug for SpawnApprovalResolverFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpawnApprovalResolverFactory")
    }
}

/// The memory handles and immutable turn input a tool loop needs.
///
/// `ledger` is the durable tool-execution idempotency ledger: without it every
/// side-effecting tool is refused, so it must be present on every entry point
/// that can run one. `ingest` is the optional document-ingest runtime used to
/// page oversized tool output into memory, and it needs a resolved scope that
/// some entry points simply do not have.
///
/// They used to travel as a single `Option<DocumentIngestRuntime>`, which tied
/// the ledger to whether document ingest happened to be configured. Any entry
/// point without an ingest scope silently lost every side-effecting tool.
#[derive(Clone, Default)]
pub(crate) struct ToolLoopMemory {
    pub(super) ledger: Option<Arc<dyn Memory>>,
    pub(super) ingest: Option<DocumentIngestRuntime>,
    /// Optional durable event sink for request-context provenance. Callers
    /// attach their already-configured fabric so recording honors the same
    /// event policy as user, assistant, tool, and terminal events.
    pub(super) event_fabric: Option<MemoryFabric>,
    /// Exact message-fabric lineage for entrypoints whose tool execution scope
    /// is synthetic (notably the local CLI compatibility paths).
    pub(super) request_event_scope: Option<MessageEventScope>,
}

impl ToolLoopMemory {
    /// Resolve the ledger for `memory` and pair it with an optional ingest runtime.
    pub(crate) fn new(memory: &Arc<dyn Memory>, workspace_dir: &Path, ingest: Option<DocumentIngestRuntime>) -> Self {
        Self {
            ledger: crate::memory::tool_execution_ledger(memory, workspace_dir),
            ingest,
            event_fabric: None,
            request_event_scope: None,
        }
    }

    /// Attach the entrypoint's configured event fabric. This deliberately
    /// travels with the other per-turn memory handles so every ingress reaches
    /// the same request-context persistence point inside the shared loop.
    pub(crate) fn with_event_fabric(mut self, event_fabric: MemoryFabric) -> Self {
        self.event_fabric = Some(event_fabric);
        self
    }

    /// Override the request event's lineage when it cannot be recovered from
    /// the tool execution envelope.
    pub(crate) fn with_request_event_scope(mut self, scope: MessageEventScope) -> Self {
        self.request_event_scope = Some(scope);
        self
    }

    /// The document-ingest runtime, when this entry point resolved a scope for it.
    pub(crate) const fn ingest(&self) -> Option<&DocumentIngestRuntime> {
        self.ingest.as_ref()
    }

    /// No ledger and no ingest runtime.
    ///
    /// Only for callers that supply their own [`ToolLoopRuntimeAdapter`], whose
    /// `ToolExecutionService` already carries a ledger.
    pub(crate) fn none() -> Self {
        Self::default()
    }
}

/// Source-owned admission required for every model turn, including disabled mode.
/// Only the owning service can validate the context against the ingress proof.
pub(crate) struct TurnDecision<'a> {
    pub(crate) service: &'a Arc<crate::runtime::turn_decision::TurnDecisionService>,
    pub(crate) context: &'a Arc<crate::runtime::turn_decision::TurnDecisionContext>,
    pub(crate) proof: &'a dyn crate::runtime::turn_source::TurnSourceProof,
}

/// Scheduling knobs for the tool calls of one turn.
pub(crate) struct TurnSchedule {
    pub(crate) read_only_tool_concurrency_window: usize,
    pub(crate) priority_scheduling_enabled: bool,
    pub(crate) low_priority_tool_names: Vec<String>,
}

/// What one turn is asked to do: its channel and scope, mode, delivery
/// flags, multimodal limits, scheduling, compaction and capability routing.
pub(crate) struct TurnRequest<'a> {
    pub(crate) channel_name: &'a str,
    pub(crate) scope: Option<&'a ScopeContext<'a>>,
    pub(crate) mode: ChatMode,
    /// Suppress the loop's own stdout echo of the final text.
    pub(crate) silent: bool,
    /// Publish the `stay_silent` tool, so the model may end the turn with no
    /// reply (smart group turns only).
    pub(crate) expose_stay_silent: bool,
    pub(crate) multimodal: &'a crate::config::MultimodalConfig,
    pub(crate) schedule: TurnSchedule,
    pub(crate) compaction: Option<&'a crate::config::AgentCompactionConfig>,
    pub(crate) decision: TurnDecision<'a>,
}

/// The services one turn runs on: the provider and its identity, the tool
/// registry, observability, approvals, the runtime adapter and the memory
/// handles.
pub(crate) struct TurnServices<'a> {
    pub(crate) provider: &'a dyn Provider,
    pub(crate) provider_name: &'a str,
    pub(crate) model: &'a str,
    pub(crate) temperature: f64,
    pub(crate) tools_registry: Arc<Vec<Box<dyn Tool>>>,
    pub(crate) observer: &'a dyn Observer,
    pub(crate) hooks: &'a HookManager,
    pub(crate) approval: Option<Arc<ApprovalManager>>,
    /// Awaited at the approval decision point when present; `None` keeps the
    /// synchronous approval behavior.
    pub(crate) approval_resolver: Option<Arc<dyn ApprovalResolver>>,
    /// A caller-owned execution runtime (the chat driver's); `None` builds the
    /// default one from the scope and channel.
    pub(crate) runtime_adapter: Option<ToolLoopRuntimeAdapter>,
    pub(crate) memory: ToolLoopMemory,
}

/// How one turn is delivered while it runs: cancellation, streamed text
/// deltas and tool-call notifications.
pub(crate) struct TurnDelivery {
    pub(crate) cancellation: Option<CancellationToken>,
    pub(crate) on_delta: Option<tokio::sync::mpsc::Sender<String>>,
    pub(crate) on_tool_call: Option<tokio::sync::mpsc::Sender<ToolCallNotification>>,
}

#[path = "turn_tests/turn_request.rs"]
mod tests;

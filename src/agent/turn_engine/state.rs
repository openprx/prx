//! The values one turn's rounds share: the inputs fixed for the whole turn
//! (`TurnInputs`) and the state the rounds carry forward and change
//! (`TurnState`).

use std::collections::HashMap;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::agent::turn_contracts::{ChatMode, ToolCallNotification, ToolLoopRuntimeAdapter, ToolLoopTrace};
use crate::llm::route_decision::ProviderUsageAccumulator;

/// Values fixed for the whole turn, read by every round.
pub(super) struct TurnInputs<'a> {
    pub(super) advice: Option<crate::decision_engine::IntentAnalysis>,
    pub(super) channel_name: &'a str,
    pub(super) chat_mode: ChatMode,
    pub(super) silent: bool,
    pub(super) expose_stay_silent: bool,
    pub(super) multimodal_config: &'a crate::config::MultimodalConfig,
    pub(super) compaction_config: Option<&'a crate::config::AgentCompactionConfig>,
    pub(super) service: &'a Arc<crate::runtime::turn_publication::TurnRuntimeService>,
    pub(super) context: &'a Arc<crate::runtime::turn_context::OpenedTurn>,
    pub(super) proof: &'a dyn crate::runtime::turn_source::TurnSourceProof,
    pub(super) provider: &'a dyn crate::providers::Provider,
    pub(super) provider_name: &'a str,
    pub(super) model: &'a str,
    pub(super) temperature: f64,
    pub(super) tools_registry: Arc<Vec<Box<dyn crate::tools::Tool>>>,
    pub(super) observer: &'a dyn crate::observability::Observer,
    pub(super) hooks: &'a crate::hooks::HookManager,
    pub(super) runtime_adapter: ToolLoopRuntimeAdapter,
    pub(super) document_ingest: Option<crate::agent::document_ingest::DocumentIngestRuntime>,
    pub(super) event_fabric: Option<crate::memory::MemoryFabric>,
    pub(super) request_event_scope: Option<crate::memory::MessageEventScope>,
    pub(super) tool_surface_reserve: Option<crate::agent::context_budget::ToolSurfaceReserve>,
    pub(super) cancellation_token: Option<CancellationToken>,
    pub(super) on_delta: Option<tokio::sync::mpsc::Sender<String>>,
    pub(super) on_tool_call: Option<tokio::sync::mpsc::Sender<ToolCallNotification>>,
    pub(super) usage_anchors: crate::context::budget::anchor::UsageAnchors,
    pub(super) parameter_capture: crate::providers::parameter_capture::Capture,
    pub(super) cache_series: crate::agent::prompt_cache_metrics::Series,
    pub(super) read_only_schedule: crate::agent::tool_scheduler::ReadOnlyToolScheduleConfig,
    pub(super) mode_capabilities: crate::providers::ProviderCapabilities,
    pub(super) inherited_exclusions: Vec<String>,
    pub(super) safety_surface: Arc<crate::tools::execution::admission::TurnSurface>,
}

/// The request of one round as it leaves for the provider, with the adapter
/// its tool calls run through.
pub(super) struct PreparedRound {
    pub(super) prepared_messages: crate::multimodal::PreparedMessages,
    pub(super) compiled_tool_context: crate::agent::turn_context::CompiledToolContext,
    pub(super) execution_adapter: ToolLoopRuntimeAdapter,
    pub(super) guarded_delivery: bool,
    pub(super) system_prompt_sha256: Option<String>,
    pub(super) system_prompt_chars: usize,
    pub(super) tool_surface: crate::agent::turn_context::RequestToolMeasurement,
}

/// Values the rounds of one turn carry forward and change.
pub(super) struct TurnState {
    /// The round counter; the first round is 1.
    pub(super) iteration: usize,
    pub(super) decision_brief: Option<String>,
    pub(super) decision_brief_sha256: Option<String>,
    /// The analysis is recorded once, with the brief the first send carries.
    pub(super) analysis_recorded: bool,
    /// Attribution of the final returning turn. Each provider call overwrites
    /// it with that turn's real serving provider/model and attempts.
    pub(super) last_turn_trace: ToolLoopTrace,
    pub(super) usage_accumulator: ProviderUsageAccumulator,
    /// Sticky cross-turn fallback flag, folded into the returned trace.
    pub(super) any_turn_had_fallback: bool,
    pub(super) consecutive_failures: HashMap<String, usize>,
    pub(super) unrecoverable_failures: HashMap<String, u8>,
    pub(super) tool_summary_retry_attempted: bool,
    pub(super) inject_tool_summary_retry_instruction: bool,
    /// Whether this turn ever really dispatched a tool batch. A response that
    /// claims tool use while this is still false is a fabrication.
    pub(super) executed_any_tool_call: bool,
    pub(super) overflow_recovery_passes: usize,
    pub(super) empty_response_retry_attempted: bool,
    pub(super) inject_empty_response_retry_instruction: bool,
    /// Memoized rendered prompt sections and fingerprint of the tool snapshot.
    pub(super) tool_snapshot_render_cache: crate::agent::turn_context::ToolSnapshotRenderCache,
    pub(super) request_tool_surface_cache: crate::agent::turn_context::RequestToolSurfaceCache,
}

impl TurnState {
    pub(super) fn new(
        decision_brief: Option<String>,
        request_tool_surface_cache: crate::agent::turn_context::RequestToolSurfaceCache,
    ) -> Self {
        let decision_brief_sha256 = decision_brief.as_deref().map(crate::agent::prompt::prompt_sha256);
        Self {
            iteration: 0,
            decision_brief,
            decision_brief_sha256,
            analysis_recorded: false,
            last_turn_trace: ToolLoopTrace::default(),
            usage_accumulator: ProviderUsageAccumulator::new(),
            any_turn_had_fallback: false,
            consecutive_failures: HashMap::new(),
            unrecoverable_failures: HashMap::new(),
            tool_summary_retry_attempted: false,
            inject_tool_summary_retry_instruction: false,
            executed_any_tool_call: false,
            overflow_recovery_passes: 0,
            empty_response_retry_attempted: false,
            inject_empty_response_retry_instruction: false,
            tool_snapshot_render_cache: crate::agent::turn_context::ToolSnapshotRenderCache::default(),
            request_tool_surface_cache,
        }
    }

    /// The trace a returning turn hands back: the cross-turn fallback flag and
    /// the usage total are folded in here, then the trace is taken. Every call
    /// is immediately followed by the turn's `return`.
    pub(super) fn returned_trace(&mut self, context: &crate::runtime::turn_context::OpenedTurn) -> ToolLoopTrace {
        self.last_turn_trace.any_turn_had_fallback = self.any_turn_had_fallback;
        self.last_turn_trace.tokens_used = self.usage_accumulator.finish();
        self.last_turn_trace.execution_trace = Some(context.terminal_execution_trace());
        std::mem::take(&mut self.last_turn_trace)
    }
}

//! Capability routing on the redux chat driver.
//!
//! Every other entry point (channels, gateway, console, session worker,
//! `sessions_spawn`, `delegate`) hands `[tool_tiering]` to the shared tool loop.
//! The redux chat driver passed `None`, which does not mean "default policy" —
//! it disables intent routing altogether and republishes the entire registry to
//! the provider on every turn. These tests pin the boundary by reading what the
//! provider was actually offered, so deleting the argument again turns them red.

use super::*;
use crate::providers::traits::{
    ChatMessage, ChatRequest, ChatTrace, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
};
use crate::security::SecurityPolicy;
use crate::tools::{Tool, ToolCategory, ToolResult, ToolTier};
use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use std::sync::Arc;

const TIERING_DRAFT: &str = "tool-tiering-draft";
const CORE_TOOL: &str = "tiering_core_probe";
const EXTENDED_TOOL: &str = "tiering_extended_probe";

/// Records the tool catalog the provider was offered for the turn.
struct CatalogRecordingProvider {
    offered: Mutex<Vec<Vec<String>>>,
}

impl CatalogRecordingProvider {
    fn new() -> Self {
        Self {
            offered: Mutex::new(Vec::new()),
        }
    }

    fn last_offer(&self) -> Vec<String> {
        self.offered.lock().last().cloned().unwrap_or_default()
    }
}

#[async_trait]
impl Provider for CatalogRecordingProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(String::new())
    }

    async fn chat_traced(
        &self,
        _request: ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<ChatTrace> {
        anyhow::bail!("tiering fixture drives the streaming path only")
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn stream_chat_with_history(
        &self,
        _messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
        options: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamChunk>> {
        self.offered.lock().push(
            options
                .tools
                .unwrap_or_default()
                .into_iter()
                .map(|spec| spec.name)
                .collect(),
        );
        stream::iter(vec![Ok(StreamChunk::delta("ok")), Ok(StreamChunk::final_chunk())]).boxed()
    }
}

struct TierProbeTool {
    name: &'static str,
    tier: ToolTier,
    categories: &'static [ToolCategory],
}

#[async_trait]
impl Tool for TierProbeTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "capability routing probe"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: String::new(),
            error: None,
        })
    }

    fn tier(&self) -> ToolTier {
        self.tier
    }

    fn categories(&self) -> &'static [ToolCategory] {
        self.categories
    }
}

fn probe_registry() -> Arc<Vec<Box<dyn Tool>>> {
    Arc::new(vec![
        Box::new(TierProbeTool {
            name: CORE_TOOL,
            tier: ToolTier::Core,
            categories: &[],
        }) as Box<dyn Tool>,
        Box::new(TierProbeTool {
            name: EXTENDED_TOOL,
            tier: ToolTier::Extended,
            categories: &[ToolCategory::Automation],
        }) as Box<dyn Tool>,
    ])
}

async fn offered_tools_for(user_message: &str, tiering: crate::config::ToolTieringConfig) -> Vec<String> {
    let provider = Arc::new(CatalogRecordingProvider::new());
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(128);
    let policy = Arc::new(SecurityPolicy::default());
    let context = chat_tool_execution_context(policy.as_ref(), None, None, TIERING_DRAFT);
    let ledger_dir = tempfile::TempDir::new().expect("tiering fixture ledger");
    let ledger: Arc<dyn crate::memory::Memory> =
        Arc::new(crate::memory::SqliteMemory::new(ledger_dir.path()).expect("tiering fixture sqlite"));
    let registry = probe_registry();
    let cancellation = CancellationToken::new();
    let service = Arc::new(chat_tool_execution_service(
        Arc::clone(&registry),
        Some(Arc::clone(&ledger)),
        Arc::clone(&policy),
        Arc::new(ApprovalRouter::new()),
        action_tx.clone(),
        cancellation.clone(),
        None,
    ));

    drive_start_turn_stream(
        None,
        Arc::clone(&provider) as Arc<dyn Provider>,
        vec![ChatMessage::system("tiering system"), ChatMessage::user(user_message)],
        vec![ChatMessage::system("tiering system"), ChatMessage::user(user_message)],
        "tiering-model".to_string(),
        0.0,
        None,
        cancellation,
        TIERING_DRAFT.to_string(),
        action_tx.clone(),
        Some(registry),
        Some(service),
        context,
        crate::memory::MemoryFabric::new(Arc::clone(&ledger), ledger_dir.path().to_string_lossy()),
        None,
        crate::agent::loop_::ChatMode::Edit,
        Arc::new(crate::observability::noop::NoopObserver),
        Arc::new(crate::hooks::HookManager::new(std::path::PathBuf::new())),
        tiering,
    )
    .await;

    drop(action_tx);
    while action_rx.recv().await.is_some() {}
    provider.last_offer()
}

/// MUTATION GUARD: pass `None` instead of `Some(&tool_tiering)` in
/// `drive_start_turn_stream` and the extended probe is offered again.
#[tokio::test]
async fn chat_driver_routes_capabilities_instead_of_publishing_the_whole_registry() {
    let offered = offered_tools_for(
        "please summarise the paragraph above",
        crate::config::ToolTieringConfig::default(),
    )
    .await;
    assert!(
        offered.iter().any(|name| name == CORE_TOOL),
        "core tools stay on every turn, got {offered:?}"
    );
    assert!(
        !offered.iter().any(|name| name == EXTENDED_TOOL),
        "an extended tool whose category was never mentioned must not be published, got {offered:?}"
    );
}

#[tokio::test]
async fn chat_driver_offers_an_extended_tool_once_its_category_is_named() {
    let offered = offered_tools_for(
        "spawn a sub-agent for this",
        crate::config::ToolTieringConfig::default(),
    )
    .await;
    assert!(
        offered.iter().any(|name| name == EXTENDED_TOOL),
        "'spawn' activates Automation, so the extended probe must be offered, got {offered:?}"
    );
}

#[tokio::test]
async fn chat_driver_honors_always_include_over_intent_routing() {
    let tiering = crate::config::ToolTieringConfig {
        always_include: vec![EXTENDED_TOOL.to_string()],
        ..crate::config::ToolTieringConfig::default()
    };
    let offered = offered_tools_for("please summarise the paragraph above", tiering).await;
    assert!(
        offered.iter().any(|name| name == EXTENDED_TOOL),
        "always_include must restore a tool intent routing dropped, got {offered:?}"
    );
}

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
const WEB_TOOL: &str = "tiering_web_probe";

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
        Box::new(TierProbeTool {
            name: WEB_TOOL,
            tier: ToolTier::Extended,
            categories: &[ToolCategory::WebBrowsing],
        }) as Box<dyn Tool>,
    ])
}

async fn offered_tools_for(user_message: &str, tiering: crate::config::ToolTieringConfig) -> Vec<String> {
    offered_tools_for_history(user_message, Some(user_message), tiering).await
}

/// Drive one redux turn whose provider history may differ from the text the
/// user actually typed — which is the live chat shape, because memory recall
/// and the shared-workspace event block are prepended before the turn starts.
async fn offered_tools_for_history(
    history_user_message: &str,
    routing_input: Option<&str>,
    tiering: crate::config::ToolTieringConfig,
) -> Vec<String> {
    offered_tools_for_surface(
        history_user_message,
        routing_input,
        // A fresh fixture session has nothing pinned yet, which is the
        // first-turn case where an unrouted turn still gets the whole registry.
        crate::tools::intent::SessionToolSurface {
            tiering,
            unrouted: crate::tools::intent::UnroutedToolPolicy::KeepPinnedExposure,
        },
    )
    .await
}

/// Drive one turn of a *continuing* chat session: the dispatcher folds the turn
/// into the session's cumulative exposure first, exactly as the live driver
/// does, and hands the resulting surface to the shared tool loop.
async fn offered_tools_for_session(
    exposure: &crate::tools::intent::SessionToolExposure,
    user_message: &str,
    base: &crate::config::ToolTieringConfig,
) -> Vec<String> {
    let surface = exposure.sticky_surface(base, probe_registry().as_ref(), user_message);
    let mut offered = offered_tools_for_surface(user_message, Some(user_message), surface).await;
    offered.sort();
    offered
}

async fn offered_tools_for_surface(
    history_user_message: &str,
    routing_input: Option<&str>,
    surface: crate::tools::intent::SessionToolSurface,
) -> Vec<String> {
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
        vec![
            ChatMessage::system("tiering system"),
            ChatMessage::user(history_user_message),
        ],
        vec![
            ChatMessage::system("tiering system"),
            ChatMessage::user(history_user_message),
        ],
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
        surface,
        routing_input.map(str::to_string),
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
        "search the archive for that paragraph",
        crate::config::ToolTieringConfig::default(),
    )
    .await;
    assert!(
        offered.iter().any(|name| name == CORE_TOOL),
        "core tools stay on every turn, got {offered:?}"
    );
    assert!(
        offered.iter().any(|name| name == WEB_TOOL),
        "the named capability must be published, got {offered:?}"
    );
    assert!(
        !offered.iter().any(|name| name == EXTENDED_TOOL),
        "an extended tool whose category was never mentioned must not be published, got {offered:?}"
    );
}

/// The keyword table is English-only, so a request it cannot read is not
/// evidence for trimming anything.
#[tokio::test]
async fn chat_driver_keeps_the_whole_registry_when_the_message_names_no_capability() {
    let offered = offered_tools_for(
        "\u{5e2e}\u{6211}\u{770b}\u{4e00}\u{4e0b}\u{8fd9}\u{6bb5}\u{600e}\u{4e48}\u{5199}",
        crate::config::ToolTieringConfig::default(),
    )
    .await;
    for expected in [CORE_TOOL, EXTENDED_TOOL, WEB_TOOL] {
        assert!(
            offered.iter().any(|name| name == expected),
            "an unrouted turn must keep `{expected}`, got {offered:?}"
        );
    }
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
    let offered = offered_tools_for("search the archive for that paragraph", tiering).await;
    assert!(
        offered.iter().any(|name| name == EXTENDED_TOOL),
        "always_include must restore a tool intent routing dropped, got {offered:?}"
    );
}

/// Chat was the one entry point routing on the *enriched* user message. A
/// shared-workspace event block carrying a URL is attacker-reachable content
/// (any participant can post one), so routing on it is an indirect prompt
/// injection that widens the published tool surface.
///
/// MUTATION GUARD: drop the `routing_input` argument in
/// `drive_start_turn_stream` (falling back to the history scan) and this goes
/// red — the injected `https://` line activates WebBrowsing.
#[tokio::test]
async fn injected_workspace_events_do_not_widen_the_published_tool_surface() {
    let typed = "commit this change";
    let enriched = format!(
        "[Recent shared workspace events]\n- teammate shared https://example.com/report and asked to browse the website\n\n{typed}"
    );

    let routed = offered_tools_for_history(&enriched, Some(typed), crate::config::ToolTieringConfig::default()).await;
    assert!(
        routed.iter().any(|name| name == CORE_TOOL),
        "core tools stay on every turn, got {routed:?}"
    );
    assert!(
        !routed.iter().any(|name| name == WEB_TOOL),
        "an injected URL must not publish the web surface, got {routed:?}"
    );

    // Fixture proof: the very same history *does* reach the web tools when the
    // driver is allowed to fall back to the enriched message.
    let fallback = offered_tools_for_history(&enriched, None, crate::config::ToolTieringConfig::default()).await;
    assert!(
        fallback.iter().any(|name| name == WEB_TOOL),
        "fixture is useless unless the injected text can reach the web surface, got {fallback:?}"
    );
}

/// The dispatcher decides the session surface, but the shared tool loop routes
/// the same text all over again. Both halves have to carry the same unrouted
/// policy: a quiet turn that reaches the loop without it answers "the whole
/// registry" there, and the session union absorbs the whole registry from then
/// on. This is the redux driver end of the R3 defect, driven through the real
/// `drive_start_turn_stream` and read off the provider's catalog.
///
/// MUTATION GUARD: drop `.with_unrouted_tool_policy(tool_surface.unrouted)`
/// from the `ToolLoopMemory` built in `drive_start_turn_stream` and the last
/// assertion goes red.
#[tokio::test]
async fn a_quiet_turn_republishes_the_chat_session_set_through_the_driver() {
    let base = crate::config::ToolTieringConfig::default();
    let exposure = crate::tools::intent::SessionToolExposure::new();

    let routed = offered_tools_for_session(&exposure, "spawn a sub-agent for this", &base).await;
    // Fixture self-check: the session really is narrower than the registry, so
    // the equality below cannot be satisfied by a set that never narrowed.
    assert!(
        routed.iter().any(|name| name == EXTENDED_TOOL),
        "the routed turn must reach the automation probe: {routed:?}"
    );
    assert!(
        !routed.iter().any(|name| name == WEB_TOOL),
        "the routed turn must not reach the web probe: {routed:?}"
    );

    let quiet = offered_tools_for_session(&exposure, "thanks, that is all", &base).await;
    assert_eq!(
        quiet, routed,
        "a turn the keyword table cannot read must republish the session set, not the registry"
    );
}

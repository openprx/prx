//! Decision-owned capability publication on the Redux chat driver.
//!
//! Registry tier and previous offers do not confer authority. These fixtures
//! record the actual provider schemas after durable source admission, and use
//! real search/append admission for tools absent from the initial selection.

#![cfg(test)]
use super::*;
use crate::providers::traits::{
    ChatMessage, ChatRequest, ChatTrace, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult, ToolCallChunk,
};
use crate::tools::{Tool, ToolCategory, ToolResult, ToolTier};
use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

const TIERING_DRAFT: &str = "tool-tiering-draft";
const CORE_TOOL: &str = "tiering_core_probe";
const EXTENDED_TOOL: &str = "tiering_extended_probe";
const WEB_TOOL: &str = "tiering_web_probe";
/// A permitted discovery control distinct from the excluded web probe.
const MEDIA_TOOL: &str = "tiering_media_probe";
const APPROVAL_TOOL: &str = "config_reload";
const SESSIONS_SPAWN_TOOL: &str = "sessions_spawn";

/// Records the tool catalog the provider was offered for the turn.
struct CatalogRecordingProvider {
    offered: Mutex<Vec<Vec<String>>>,
    discover: Option<&'static str>,
    call: Option<&'static str>,
}

impl CatalogRecordingProvider {
    fn new(discover: Option<&'static str>, call: Option<&'static str>) -> Self {
        Self {
            offered: Mutex::new(Vec::new()),
            discover,
            call,
        }
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

    async fn chat_traced(&self, request: ChatRequest<'_>, model: &str, _temperature: f64) -> anyhow::Result<ChatTrace> {
        super::assessment_fixture::trace(&request, model)
            .ok_or_else(|| anyhow::anyhow!("unexpected non-assessment request in streaming fixture"))
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
        let round = {
            let mut offered = self.offered.lock();
            offered.push(
                options
                    .tools
                    .unwrap_or_default()
                    .into_iter()
                    .map(|spec| spec.name)
                    .collect(),
            );
            offered.len()
        };
        if let Some(target) = self.call.filter(|_| round == 1) {
            return stream::iter(vec![
                Ok(StreamChunk::tool_call_chunk(vec![ToolCallChunk::new(
                    "tiering-direct-call",
                    target,
                    "{}",
                    0,
                )])),
                Ok(StreamChunk::final_chunk()),
            ])
            .boxed();
        }
        if let Some(target) = self.discover {
            let call = match round {
                1 => Some((
                    "capability_search",
                    serde_json::json!({"query": target, "limit": 1}).to_string(),
                )),
                2 => Some((target, "{}".to_owned())),
                _ => None,
            };
            if let Some((name, args)) = call {
                return stream::iter(vec![
                    Ok(StreamChunk::tool_call_chunk(vec![ToolCallChunk::new(
                        format!("tiering-call-{round}"),
                        name,
                        args,
                        0,
                    )])),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed();
            }
        }
        stream::iter(vec![Ok(StreamChunk::delta("ok")), Ok(StreamChunk::final_chunk())]).boxed()
    }
}

struct ApprovalProbeTool {
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for ApprovalProbeTool {
    fn name(&self) -> &str {
        APPROVAL_TOOL
    }

    fn description(&self) -> &str {
        "approval boundary probe"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.executions.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(ToolResult {
            success: true,
            output: "approved".to_owned(),
            error: None,
        })
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

fn probe_tools(include_sessions_spawn: bool) -> Vec<Box<dyn Tool>> {
    let mut tools = vec![
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
        Box::new(TierProbeTool {
            name: MEDIA_TOOL,
            tier: ToolTier::Extended,
            categories: &[ToolCategory::Media],
        }) as Box<dyn Tool>,
    ];
    if include_sessions_spawn {
        tools.push(Box::new(TierProbeTool {
            name: SESSIONS_SPAWN_TOOL,
            tier: ToolTier::Core,
            categories: &[ToolCategory::Automation],
        }) as Box<dyn Tool>);
    }
    tools
}

fn probe_registry() -> Arc<Vec<Box<dyn Tool>>> {
    crate::agent::test_admission::registry_with_search(probe_tools(false))
}

fn spawn_probe_registry() -> Arc<Vec<Box<dyn Tool>>> {
    crate::agent::test_admission::registry_with_search(probe_tools(true))
}

/// Each call records a new original input under the same fixture session. The
/// workspace can be reused to expose accidental persisted session-wide grants.
async fn offered_tools_for(
    workspace: &std::path::Path,
    source: (&str, &str),
    history_user_message: &str,
    tiering: crate::config::ToolTieringConfig,
    discover: Option<&'static str>,
) -> Vec<Vec<String>> {
    let provider = Arc::new(CatalogRecordingProvider::new(discover, None));
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(128);
    let cancellation = CancellationToken::new();
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_owned();
    config.causal_tree.enabled = false;
    // Publication is the subject of these fixtures. Leave room for decision
    // work plus the unchanged 500 ms Web family estimate so G6 measures a
    // genuine admitted capability instead of closing on fixture latency.
    config.causal_tree.policy.extra_latency_budget_ms = 1_000;
    config.tool_tiering = tiering;
    let admitted = super::turn_characterization_tests::admit_chat_fixture_with_config(
        provider.clone(),
        Some(probe_registry()),
        config,
        ("ollama", "tiering-model", None),
        source,
        action_tx.clone(),
        cancellation.clone(),
    )
    .await;
    let routing = admitted.binding.decision_routing().expect("tiering routing");
    let audit = routing
        .context
        .audit_hops()
        .map(|hop| {
            format!(
                "candidates={:?} selection={:?} prediction={:?} trace={:?} scores={:?} gates={:?}",
                hop.candidates, hop.selection, hop.prediction, hop.audit_trace, hop.scores, hop.pre_commit.outcomes
            )
        })
        .collect::<Vec<_>>();
    assert!(
        admitted.binding.fixed_reply().expect("tiering decision").is_none(),
        "tiering fixture closed before provider execution: {:?}; {audit:?}",
        routing.context.directive()
    );
    let history = vec![
        ChatMessage::system("tiering system"),
        ChatMessage::user(history_user_message),
    ];
    drive_start_turn_stream(
        Some(admitted.binding),
        None,
        provider.clone(),
        history.clone(),
        history,
        "tiering-model".to_owned(),
        0.0,
        None,
        cancellation,
        source.0.to_owned(),
        action_tx.clone(),
        Some(admitted.registry),
        Some(admitted.service),
        admitted.context,
        admitted.fabric,
        None,
        crate::agent::turn_contracts::ChatMode::Edit,
        Arc::new(crate::observability::noop::NoopObserver),
        Arc::new(crate::hooks::HookManager::new(std::path::PathBuf::new())),
    )
    .await;
    drop(action_tx);
    let mut completed = false;
    while let Some(action) = action_rx.recv().await {
        match action {
            Action::StreamFailed { err, .. } => panic!("admitted tiering turn failed: {err}"),
            Action::StreamCompleted { .. } => completed = true,
            _ => {}
        }
    }
    assert!(completed, "fixture must complete an actual provider turn");
    let offers = provider.offered.lock().clone();
    assert!(!offers.is_empty(), "provider must observe the publication");
    offers
}

struct PlainAnswerObservation {
    offers: Vec<Vec<String>>,
    has_child_manifest: bool,
}

async fn plain_answer_offers_for(
    workspace: &std::path::Path,
    draft: &str,
    tiering: crate::config::ToolTieringConfig,
    autonomy: crate::security::AutonomyLevel,
) -> PlainAnswerObservation {
    let provider = Arc::new(CatalogRecordingProvider::new(None, None));
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(128);
    let cancellation = CancellationToken::new();
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_owned();
    config.tool_tiering = tiering;
    config.autonomy.level = autonomy;
    let admitted = super::turn_characterization_tests::admit_chat_fixture_with_config(
        provider.clone(),
        Some(spawn_probe_registry()),
        config,
        ("ollama", "tiering-model", None),
        (draft, "hello"),
        action_tx.clone(),
        cancellation.clone(),
    )
    .await;
    let decision = admitted.binding.decision_routing().expect("plain turn decision");
    assert!(matches!(
        decision.context.directive(),
        crate::causal_tree::directive::TurnExecutionDirective::AnswerDirectly
    ));
    let has_child_manifest = decision.context.engine().outgoing_child.is_some();
    let history = vec![ChatMessage::system("tiering system"), ChatMessage::user("hello")];
    drive_start_turn_stream(
        Some(admitted.binding),
        None,
        provider.clone(),
        history.clone(),
        history,
        "tiering-model".to_owned(),
        0.0,
        None,
        cancellation,
        draft.to_owned(),
        action_tx.clone(),
        Some(admitted.registry),
        Some(admitted.service),
        admitted.context,
        admitted.fabric,
        None,
        crate::agent::turn_contracts::ChatMode::Edit,
        Arc::new(crate::observability::noop::NoopObserver),
        Arc::new(crate::hooks::HookManager::new(std::path::PathBuf::new())),
    )
    .await;
    drop(action_tx);
    let mut completed = false;
    while let Some(action) = action_rx.recv().await {
        match action {
            Action::StreamFailed { err, .. } => panic!("plain direct-answer turn failed: {err}"),
            Action::StreamCompleted { .. } => completed = true,
            _ => {}
        }
    }
    assert!(completed, "plain direct-answer fixture must complete");
    PlainAnswerObservation {
        offers: provider.offered.lock().clone(),
        has_child_manifest,
    }
}

fn contains(offer: &[String], name: &str) -> bool {
    offer.iter().any(|offered| offered == name)
}

#[tokio::test]
async fn plain_direct_answer_only_offers_unexcluded_direct_tools() {
    let workspace = tempfile::tempdir().unwrap();
    let policy = crate::config::ToolTieringConfig {
        always_include: vec![CORE_TOOL.to_owned(), WEB_TOOL.to_owned()],
        always_exclude: vec![WEB_TOOL.to_owned(), SESSIONS_SPAWN_TOOL.to_owned()],
        ..Default::default()
    };
    let observation = plain_answer_offers_for(
        workspace.path(),
        "plain-protective-surface",
        policy,
        crate::security::AutonomyLevel::Full,
    )
    .await;
    assert_eq!(observation.offers.len(), 1);
    let first = observation.offers.first().expect("plain answer provider offer");
    assert!(contains(first, CORE_TOOL), "configured Direct tool must be offered");
    assert!(
        !contains(first, WEB_TOOL),
        "always_exclude must outrank Direct publication"
    );
    for searchable in [EXTENDED_TOOL, MEDIA_TOOL] {
        assert!(
            !contains(first, searchable),
            "Searchable tool must not be offered: {searchable}"
        );
    }
}

#[tokio::test]
async fn plain_direct_answer_exclusion_hides_sessions_spawn() {
    let workspace = tempfile::tempdir().unwrap();
    let policy = crate::config::ToolTieringConfig {
        always_exclude: vec![SESSIONS_SPAWN_TOOL.to_owned()],
        ..Default::default()
    };
    let observation = plain_answer_offers_for(
        workspace.path(),
        "plain-excluded-spawn",
        policy,
        crate::security::AutonomyLevel::Full,
    )
    .await;
    assert!(
        observation
            .offers
            .iter()
            .all(|offer| !contains(offer, SESSIONS_SPAWN_TOOL)),
        "interactive publication must honor the sessions_spawn exclusion"
    );
}

#[tokio::test]
async fn plain_direct_answer_without_a_child_manifest_hides_sessions_spawn() {
    let workspace = tempfile::tempdir().unwrap();
    let observation = plain_answer_offers_for(
        workspace.path(),
        "plain-unsealed-spawn",
        Default::default(),
        crate::security::AutonomyLevel::ReadOnly,
    )
    .await;
    assert!(
        !observation.has_child_manifest,
        "read-only admission must not seal a child manifest"
    );
    assert!(
        observation
            .offers
            .iter()
            .all(|offer| !contains(offer, SESSIONS_SPAWN_TOOL)),
        "sessions_spawn must not be offered without this decision's sealed child manifest"
    );
}

#[tokio::test]
async fn plain_direct_answer_does_not_execute_a_direct_tool_without_approval() {
    let workspace = tempfile::tempdir().unwrap();
    let executions = Arc::new(AtomicUsize::new(0));
    let registry = crate::agent::test_admission::registry_with_search(vec![Box::new(ApprovalProbeTool {
        executions: Arc::clone(&executions),
    }) as Box<dyn Tool>]);
    let provider = Arc::new(CatalogRecordingProvider::new(None, Some(APPROVAL_TOOL)));
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(128);
    let cancellation = CancellationToken::new();
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.autonomy.level = crate::security::AutonomyLevel::Supervised;
    config.tool_tiering.always_include.push(APPROVAL_TOOL.to_owned());
    let admitted = super::turn_characterization_tests::admit_chat_fixture_with_config(
        provider.clone(),
        Some(registry),
        config,
        ("ollama", "tiering-model", None),
        ("plain-approval", "hello"),
        action_tx.clone(),
        cancellation.clone(),
    )
    .await;
    assert!(matches!(
        admitted
            .binding
            .decision_routing()
            .expect("plain approval decision")
            .context
            .directive(),
        crate::causal_tree::directive::TurnExecutionDirective::AnswerDirectly
    ));
    let history = vec![ChatMessage::system("tiering system"), ChatMessage::user("hello")];
    let driver = tokio::spawn(drive_start_turn_stream(
        Some(admitted.binding),
        None,
        provider.clone(),
        history.clone(),
        history,
        "tiering-model".to_owned(),
        0.0,
        None,
        cancellation,
        "plain-approval".to_owned(),
        action_tx.clone(),
        Some(admitted.registry),
        Some(admitted.service),
        admitted.context,
        admitted.fabric,
        None,
        crate::agent::turn_contracts::ChatMode::Edit,
        Arc::new(crate::observability::noop::NoopObserver),
        Arc::new(crate::hooks::HookManager::new(std::path::PathBuf::new())),
    ));

    let mut saw_approval_required = false;
    let mut saw_finished = false;
    let mut saw_completed = false;
    for _ in 0..64 {
        let action = tokio::time::timeout(std::time::Duration::from_secs(3), action_rx.recv())
            .await
            .expect("approval action within timeout")
            .expect("approval action channel remains open");
        match action {
            Action::ToolStarted { name, .. } if name == APPROVAL_TOOL => {
                panic!("unapproved tool reached execution preparation")
            }
            Action::ToolFinished {
                name, success, result, ..
            } if name == APPROVAL_TOOL => {
                assert!(!success, "unapproved probe must be rejected");
                let result = result.expect("approval-required result");
                assert!(
                    result.contains("Approval is required") && result.contains("has not run"),
                    "unexpected approval result: {result}"
                );
                assert_eq!(
                    executions.load(AtomicOrdering::SeqCst),
                    0,
                    "the backend must remain untouched while approval is pending"
                );
                saw_approval_required = true;
                saw_finished = true;
            }
            Action::StreamCompleted { .. } => {
                saw_completed = true;
                break;
            }
            Action::StreamFailed { err, .. } => panic!("plain approval turn failed: {err}"),
            _ => {}
        }
    }
    driver.await.expect("plain approval driver task");
    drop(action_tx);
    assert!(
        saw_approval_required && saw_finished && saw_completed,
        "approval_required={saw_approval_required} finished={saw_finished} completed={saw_completed}"
    );
    assert_eq!(executions.load(AtomicOrdering::SeqCst), 0);
    let offers = provider.offered.lock();
    assert!(contains(
        offers.first().expect("plain approval provider offer"),
        APPROVAL_TOOL
    ));
}

#[tokio::test]
async fn chat_driver_publishes_selected_capability_instead_of_registry_tiers() {
    let workspace = tempfile::tempdir().unwrap();
    let offers = offered_tools_for(
        workspace.path(),
        (TIERING_DRAFT, WEB_TOOL),
        WEB_TOOL,
        Default::default(),
        None,
    )
    .await;
    assert_eq!(offers.len(), 1);
    assert!(
        contains(offers.first().expect("test: expected provider offer"), WEB_TOOL),
        "selected web publication missing: {offers:?}"
    );
    assert!(contains(
        offers.first().expect("test: expected provider offer"),
        "capability_search"
    ));
    for unselected in [CORE_TOOL, EXTENDED_TOOL, MEDIA_TOOL] {
        assert!(
            !contains(offers.first().expect("test: expected provider offer"), unselected),
            "tier does not authorize {unselected}: {offers:?}"
        );
    }
}

#[tokio::test]
async fn chat_driver_does_not_publish_whole_registry_for_unrecognized_language() {
    let workspace = tempfile::tempdir().unwrap();
    let input = "ayúdame a revisar cómo quedó esto";
    let offers = offered_tools_for(
        workspace.path(),
        (TIERING_DRAFT, input),
        input,
        Default::default(),
        None,
    )
    .await;
    assert!(
        [CORE_TOOL, EXTENDED_TOOL, WEB_TOOL, MEDIA_TOOL]
            .iter()
            .any(|name| !contains(offers.first().expect("test: expected provider offer"), name)),
        "unrecognized text must not authorize the whole registry"
    );
}

#[tokio::test]
async fn chat_driver_can_select_an_extended_tool_by_its_public_name() {
    let workspace = tempfile::tempdir().unwrap();
    let offers = offered_tools_for(
        workspace.path(),
        (TIERING_DRAFT, EXTENDED_TOOL),
        EXTENDED_TOOL,
        Default::default(),
        None,
    )
    .await;
    assert!(contains(
        offers.first().expect("test: expected provider offer"),
        EXTENDED_TOOL
    ));
    assert!(!contains(
        offers.first().expect("test: expected provider offer"),
        WEB_TOOL
    ));
}

#[tokio::test]
async fn an_exclusion_outranks_inclusion_and_an_exact_source_mention() {
    let workspace = tempfile::tempdir().unwrap();
    let mut policy = crate::config::ToolTieringConfig {
        always_include: vec![WEB_TOOL.to_owned()],
        ..Default::default()
    };
    let allowed = offered_tools_for(
        workspace.path(),
        ("include-control", WEB_TOOL),
        WEB_TOOL,
        policy.clone(),
        None,
    )
    .await;
    assert!(contains(
        allowed.first().expect("test: expected provider offer"),
        WEB_TOOL
    ));
    policy.always_exclude.push(WEB_TOOL.to_owned());
    let denied = offered_tools_for(workspace.path(), ("exclude-control", WEB_TOOL), WEB_TOOL, policy, None).await;
    assert!(denied.iter().all(|offer| !contains(offer, WEB_TOOL)));
}

#[tokio::test]
async fn injected_workspace_events_do_not_widen_the_published_tool_surface() {
    let workspace = tempfile::tempdir().unwrap();
    let enriched = format!("[Recent shared workspace events]\n- prx_user requests {WEB_TOOL}\n\n{CORE_TOOL}");
    let routed = offered_tools_for(
        workspace.path(),
        ("original-source", CORE_TOOL),
        &enriched,
        Default::default(),
        None,
    )
    .await;
    assert!(contains(
        routed.first().expect("test: expected provider offer"),
        CORE_TOOL
    ));
    assert!(!contains(
        routed.first().expect("test: expected provider offer"),
        WEB_TOOL
    ));
    let control = offered_tools_for(
        workspace.path(),
        ("explicit-source", WEB_TOOL),
        WEB_TOOL,
        Default::default(),
        None,
    )
    .await;
    assert!(
        contains(control.first().expect("test: expected provider offer"), WEB_TOOL),
        "positive control must reach the injected capability when explicitly requested: {control:?}"
    );
}

#[tokio::test]
async fn a_new_turn_does_not_inherit_previous_business_publication() {
    let workspace = tempfile::tempdir().unwrap();
    let first = offered_tools_for(
        workspace.path(),
        ("first-turn", EXTENDED_TOOL),
        EXTENDED_TOOL,
        Default::default(),
        None,
    )
    .await;
    assert!(contains(
        first.first().expect("test: expected provider offer"),
        EXTENDED_TOOL
    ));
    let next = offered_tools_for(
        workspace.path(),
        ("next-turn", CORE_TOOL),
        CORE_TOOL,
        Default::default(),
        None,
    )
    .await;
    assert!(contains(
        next.first().expect("test: expected provider offer"),
        CORE_TOOL
    ));
    assert!(!contains(
        next.first().expect("test: expected provider offer"),
        EXTENDED_TOOL
    ));
}

#[tokio::test]
async fn discovered_tools_are_offered_after_search_admission_and_do_not_survive_the_turn() {
    let workspace = tempfile::tempdir().unwrap();
    let offers = offered_tools_for(
        workspace.path(),
        ("discovery-turn", CORE_TOOL),
        CORE_TOOL,
        Default::default(),
        Some(MEDIA_TOOL),
    )
    .await;
    assert_eq!(offers.len(), 3, "search, admitted execution, final reply");
    assert!(!contains(
        offers.first().expect("test: expected provider offer"),
        MEDIA_TOOL
    ));
    // FINDING 110.3: a search hit that passes the same admission gates is
    // declared on the next request (the search request itself is unchanged);
    // being declared is not an execution grant, which still runs every gate.
    assert!(
        contains(offers.get(1).expect("test: expected provider offer"), MEDIA_TOOL),
        "an admitted search hit is declared on the request after the search"
    );
    assert!(contains(
        offers.get(2).expect("test: expected provider offer"),
        MEDIA_TOOL
    ));
    let next = offered_tools_for(
        workspace.path(),
        ("after-discovery", CORE_TOOL),
        CORE_TOOL,
        Default::default(),
        None,
    )
    .await;
    assert!(!contains(
        next.first().expect("test: expected provider offer"),
        MEDIA_TOOL
    ));
}

#[tokio::test]
async fn discovery_cannot_append_an_excluded_tool() {
    let workspace = tempfile::tempdir().unwrap();
    let policy = crate::config::ToolTieringConfig {
        always_exclude: vec![WEB_TOOL.to_owned()],
        ..Default::default()
    };
    let control = offered_tools_for(
        workspace.path(),
        ("allowed-discovery", CORE_TOOL),
        CORE_TOOL,
        policy.clone(),
        Some(MEDIA_TOOL),
    )
    .await;
    assert_eq!(control.len(), 3);
    assert!(
        contains(control.get(2).expect("test: expected provider offer"), MEDIA_TOOL),
        "discovery must actually widen the allowed surface"
    );
    let denied = offered_tools_for(
        workspace.path(),
        ("denied-discovery", CORE_TOOL),
        CORE_TOOL,
        policy,
        Some(WEB_TOOL),
    )
    .await;
    assert_eq!(denied.len(), 3);
    assert!(denied.iter().all(|offer| !contains(offer, WEB_TOOL)));
}

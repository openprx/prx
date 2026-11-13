//! R29: child publications are admitted from the parent's pinned
//! `TurnRuntimeContext` and keep every refusal after the test releases its
//! handles on the parent's decision service.
//!
//! Ordinary children: a real admitted parent binds `sessions_spawn`; the
//! production binding captures the ordinary publication. Each cell then runs
//! with only the publication, the pinned runtime context, the terminal
//! binding and the production executor. Operator children: a real local `/bg`
//! admission captures the operator publication.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;

use crate::agent::test_admission::{AdmittedTurn, registry_with_search};
use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
use crate::security::AutonomyLevel;
use crate::tools::{ToolExecutionCommand, ToolExecutionContext, ToolExecutionStatus, ToolResult, ToolSpec};

const PROBE: &str = "prx_child_probe";

struct NoTransport;
#[async_trait::async_trait]
impl crate::providers::Provider for NoTransport {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("child publication fixture must not dispatch a provider")
    }
    async fn chat(
        &self,
        _: crate::providers::ChatRequest<'_>,
        _: &str,
        _: f64,
    ) -> anyhow::Result<crate::providers::ChatResponse> {
        anyhow::bail!("child publication fixture must not dispatch a provider")
    }
}

struct NoChannel;
#[async_trait::async_trait]
impl Channel for NoChannel {
    fn name(&self) -> &str {
        "prx_child_publication_fixture"
    }
    async fn send(&self, _: &SendMessage) -> anyhow::Result<()> {
        anyhow::bail!("child publication fixture must not announce")
    }
    async fn listen(&self, _: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        anyhow::bail!("child publication fixture has no listener")
    }
}

/// A registered parent tool outside every child manifest. Its published
/// description follows `drift`, so a cell can change the live executor after
/// the catalog was pinned.
struct ProbeTool {
    calls: Arc<AtomicUsize>,
    drift: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::tools::Tool for ProbeTool {
    fn name(&self) -> &str {
        PROBE
    }
    fn description(&self) -> &str {
        "child publication probe"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"value":{}},"required":["value"]})
    }
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: PROBE.into(),
            description: format!("child publication probe {}", self.drift.load(Ordering::SeqCst)),
            parameters: self.parameters_schema(),
        }]
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            success: true,
            output: "executed".into(),
            error: None,
        })
    }
}

fn channel_message(id: &str) -> ChannelMessage {
    ChannelMessage {
        id: id.to_owned(),
        sender: "prx_user".to_owned(),
        reply_target: "prx_channel".to_owned(),
        content: "sessions_spawn: explain two integers".to_owned(),
        channel: "wacli".to_owned(),
        timestamp: 0,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        thread_ts: None,
        mentioned_uuids: Vec::new(),
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    }
}

struct OrdinaryParent {
    admitted: AdmittedTurn,
    calls: Arc<AtomicUsize>,
    drift: Arc<AtomicUsize>,
}

async fn ordinary_parent(
    workspace: &std::path::Path,
    level: AutonomyLevel,
    memory: Arc<dyn crate::memory::Memory>,
    message: &str,
) -> OrdinaryParent {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_owned();
    config.config_path = workspace.join("config.toml");
    config.causal_tree.enabled = false;
    config.autonomy.level = level;
    let security = Arc::new(crate::security::SecurityPolicy::from_config(
        &config.autonomy,
        workspace,
    ));
    let spawn = crate::tools::sessions_spawn::SessionsSpawnTool::new(
        Arc::new(NoChannel),
        "ollama",
        "prx-local-model",
        0.0,
        security,
        workspace.to_owned(),
        config.multimodal.clone(),
        Default::default(),
        None,
        Default::default(),
        config.sessions_spawn.clone(),
    )
    .with_shared_memory(Arc::clone(&memory));
    let calls = Arc::new(AtomicUsize::new(0));
    let drift = Arc::new(AtomicUsize::new(0));
    let registry = registry_with_search(vec![
        Box::new(spawn),
        Box::new(ProbeTool {
            calls: Arc::clone(&calls),
            drift: Arc::clone(&drift),
        }),
    ]);
    let admitted = AdmittedTurn::ordinary_controlled_channel_with_memory(
        config,
        channel_message(message),
        registry,
        memory,
        ("ollama", "prx-local-model"),
    )
    .await;
    OrdinaryParent { admitted, calls, drift }
}

/// A child-origin dispatch context: the publication's sealed scope and the pin
/// derived from it, on the parent's owner and workspace.
fn child_dispatch(
    bound: &ToolExecutionContext,
    publication: &super::publication::OrdinaryChildPublication,
) -> ToolExecutionContext {
    let owner = bound.envelope.resolved_owner_id();
    let workspace = bound.envelope.workspace_id.clone();
    let mut context = ToolExecutionContext::new(bound.envelope.clone(), bound.chat_type.clone());
    context.child_scope = Some(Arc::clone(publication.scope()));
    context.child_pin = Some(publication.dispatch_pin(&owner, &workspace));
    context
}

async fn dispatch(
    execution: &crate::tools::ToolExecutionService,
    context: ToolExecutionContext,
) -> ToolExecutionStatus {
    execution
        .execute(
            ToolExecutionCommand::new(PROBE, serde_json::json!({"value": 1})),
            context,
            None,
        )
        .await
        .status
}

async fn ordinary_case(level: AutonomyLevel) -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path())?);
    let cleanup = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    let result = cleanup
        .scope(Box::pin(async {
            let parent = ordinary_parent(workspace.path(), level, Arc::clone(&memory), "prx-child-parent").await;
            let other = ordinary_parent(workspace.path(), level, Arc::clone(&memory), "prx-child-other").await;
            let (execution, bound) = parent.admitted.bind_selected("sessions_spawn")?;
            let publication = bound
                .ordinary_child_publication
                .clone()
                .context("ordinary parent binding captured no child publication")?;
            let decision = parent.admitted.decision();
            let runtime = Arc::clone(decision.context.runtime());
            let binding = decision.service.terminal_binding(decision.context)?;
            let link = decision.service.terminal_settlement_ref(Arc::clone(decision.context))?;
            let other_decision = other.admitted.decision();
            let foreign = other_decision.service.terminal_binding(other_decision.context)?;

            // Published from the pinned runtime context: the sealed manifest
            // and its parent authority are the ones the turn pinned at opening.
            let outgoing = runtime
                .outgoing_delegation
                .as_ref()
                .context("parent runtime context sealed no outgoing manifest")?;
            let (manifest, constraints) = publication.scope().gate_inputs();
            let pinned_parent: crate::tools::capability::child::ParentAuthorityId = runtime.decision_id.into();
            assert!(manifest == &outgoing.manifest, "manifest differs from the pinned one");
            assert!(constraints.allowed == outgoing.parent_allowed, "parent refs differ");
            assert!(
                constraints.parent_authority_id == pinned_parent,
                "parent authority differs"
            );
            assert!(
                constraints.catalog_generation == runtime.catalog_generation,
                "catalog differs"
            );
            assert_eq!(manifest.authority.level, level);
            assert_eq!(
                Some(manifest.authority.owner.as_str()),
                runtime.trusted.owner_id.as_deref()
            );

            // The test now holds no decision service: only the publication,
            // the pinned runtime context, the binding and the executor.
            drop(decision);
            drop(other_decision);
            let OrdinaryParent { admitted, calls, drift } = parent;
            drop(admitted);
            publication.verify_live()?;
            assert!(
                !publication.allows_tool(PROBE),
                "the probe is outside the sealed child manifest"
            );

            // Membership: Supervised refuses a tool outside the manifest with
            // no dispatch; Full has no manifest limit (R51) and executes once.
            let status = dispatch(&execution, child_dispatch(&bound, &publication)).await;
            if level == AutonomyLevel::Full {
                assert_eq!(status, ToolExecutionStatus::Succeeded);
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            } else {
                assert_ne!(status, ToolExecutionStatus::Succeeded);
                assert_eq!(calls.load(Ordering::SeqCst), 0);
            }
            let executed = calls.load(Ordering::SeqCst);

            // Terminal binding: the binding issued for this turn shares its
            // runtime context and decision link; another turn's binding is
            // refused, also after the service handles are gone.
            assert!(Arc::ptr_eq(binding.runtime(), &runtime));
            assert!(binding.same_turn(&binding.clone()));
            assert!(!binding.same_turn(&foreign));
            link.validate_binding(&binding)?;
            assert!(link.validate_binding(&foreign).is_err());

            // Recursive money: an exhausted parent allowance cannot fund a
            // strict child share, in Full as in Supervised; nothing dispatches.
            let allowance = &publication.parent_authority().allowance;
            allowance.money().store(0, Ordering::Release);
            assert!(allowance.carve_child(1).is_err());
            assert_eq!(calls.load(Ordering::SeqCst), executed);

            // Parity: a live executor that drifted from the pinned catalog is
            // refused in both levels, before and at dispatch.
            drift.fetch_add(1, Ordering::SeqCst);
            assert!(publication.verify_live().is_err());
            assert!(publication.verify_task_scope().is_err());
            let status = dispatch(&execution, child_dispatch(&bound, &publication)).await;
            assert_ne!(status, ToolExecutionStatus::Succeeded);
            assert_eq!(calls.load(Ordering::SeqCst), executed);
            drop(other);
            Ok(())
        }))
        .await;
    cleanup.drain((Instant::now() + Duration::from_secs(5)).into()).await?;
    result
}

async fn operator_case(level: AutonomyLevel) -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.config_path = workspace.path().join("config.toml");
    config.default_provider = Some("ollama".into());
    config.default_model = Some("prx-local-model".into());
    config.autonomy.level = level;
    config.providers.models.push(toml::from_str(
        "provider='ollama'\nmodel='prx-local-model'\noperations=['chat']\nnative_tools=true\n",
    )?);
    let generation = crate::config::new_shared(config.clone()).pin();
    let security = Arc::new(crate::security::SecurityPolicy::from_config(
        &config.autonomy,
        workspace.path(),
    ));
    let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path())?);
    let calls = Arc::new(AtomicUsize::new(0));
    let drift = Arc::new(AtomicUsize::new(0));
    let spawn = crate::tools::sessions_spawn::SessionsSpawnTool::new(
        Arc::new(NoChannel),
        "ollama",
        "prx-local-model",
        0.0,
        Arc::clone(&security),
        workspace.path().to_owned(),
        config.multimodal.clone(),
        Default::default(),
        None,
        Default::default(),
        config.sessions_spawn.clone(),
    )
    .with_shared_memory(Arc::clone(&memory));
    let registry: Arc<Vec<Box<dyn crate::tools::Tool>>> = Arc::new(vec![
        Box::new(spawn),
        Box::new(ProbeTool {
            calls: Arc::clone(&calls),
            drift: Arc::clone(&drift),
        }),
    ]);
    let provider: Arc<dyn crate::providers::Provider> = Arc::new(NoTransport);
    let cleanup = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    let result = cleanup
        .scope(Box::pin(async {
            let publication = crate::runtime::turn_publication::TurnDecisionPublication {
                generation: Arc::clone(&generation),
                registry,
                mcp_tool: None,
                security,
                observer: Arc::new(crate::observability::noop::NoopObserver),
                hooks: Arc::new(crate::hooks::HookManager::new(workspace.path().to_owned())),
                memory: Arc::clone(&memory),
                plugin_generation: 0,
            };
            let (_, source) =
                crate::chat::binding::tests::recorded_local_operator_spawn(publication, provider, "Add two integers.")
                    .await?;
            // The operator source retains its own publication; the test holds
            // no decision service. Admission holds before any change.
            source.verify_live()?;
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            // Parity drift of the live executor refuses the operator child in
            // Full as in Supervised, without any dispatch.
            drift.fetch_add(1, Ordering::SeqCst);
            assert!(source.verify_live().is_err());
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            Ok(())
        }))
        .await;
    cleanup.drain((Instant::now() + Duration::from_secs(5)).into()).await?;
    result
}

#[tokio::test]
async fn child_publication_survives_without_turn_decision_service() -> anyhow::Result<()> {
    for level in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
        ordinary_case(level)
            .await
            .with_context(|| format!("ordinary child, {level:?}"))?;
        operator_case(level)
            .await
            .with_context(|| format!("operator child, {level:?}"))?;
    }
    Ok(())
}

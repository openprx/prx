//! R29: child publications are admitted from the parent's pinned
//! `TurnRuntimeContext` and keep every refusal after the test releases its
//! handles on the parent's decision service.
//!
//! Ordinary children: a real admitted parent binds `sessions_spawn`; the
//! production binding captures the ordinary publication. Each cell then runs
//! with only the publication, the pinned runtime context, the terminal
//! binding and the production executor. Operator children: a real local `/bg`
//! admission captures the operator publication; the production task path
//! (`claim` → `issue_task` → `task_service`) builds the task service whose
//! membership, recursive-money and parity cells run per autonomy level.

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
    engine_enabled: bool,
) -> OrdinaryParent {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_owned();
    config.config_path = workspace.join("config.toml");
    config.causal_tree.enabled = false;
    config.decision_engine.enabled = engine_enabled;
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
    let mut envelope = bound.envelope.clone();
    // Child admission binds the envelope's own owner to the pin, never a
    // projection; the child envelope carries it explicitly.
    envelope.owner_id = Some(owner.clone());
    let mut context = ToolExecutionContext::new(envelope, bound.chat_type.clone());
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

async fn ordinary_case(level: AutonomyLevel, workspace: &tempfile::TempDir) -> anyhow::Result<()> {
    let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path())?);
    let cleanup = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    let result = cleanup
        .scope(Box::pin(async {
            let parent = ordinary_parent(workspace.path(), level, Arc::clone(&memory), "prx-child-parent", false).await;
            let other = ordinary_parent(workspace.path(), level, Arc::clone(&memory), "prx-child-other", false).await;
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

            // Published from the pinned runtime context: the ordinary
            // publication re-seals the pinned manifest's refs under its
            // run-scoped parent authority, inside the pinned parent refs.
            let outgoing = runtime
                .outgoing_delegation
                .as_ref()
                .context("parent runtime context sealed no outgoing manifest")?;
            let (manifest, constraints) = publication.scope().gate_inputs();
            assert!(
                manifest.allowed == outgoing.manifest.allowed,
                "manifest refs differ from the pinned ones"
            );
            assert!(constraints.allowed == outgoing.parent_allowed, "parent refs differ");
            assert!(
                manifest.authority.remaining_depth == outgoing.manifest.authority.remaining_depth,
                "manifest depth differs from the pinned one"
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
            // the pinned runtime context, the binding and the executor. The
            // borrowed `TurnDecision` views are no longer used, so moving the
            // admitted turn out below ends them.
            let OrdinaryParent { admitted, calls, drift } = parent;
            drop(admitted);
            publication.verify_live()?;
            assert!(
                !publication.allows_tool(PROBE),
                "the probe is outside the sealed child manifest"
            );

            // Membership: Supervised refuses a tool outside the manifest with
            // no dispatch; Full has no manifest limit (R51) and executes once.
            let outcome = execution
                .execute(
                    ToolExecutionCommand::new(PROBE, serde_json::json!({"value": 1})),
                    child_dispatch(&bound, &publication),
                    None,
                )
                .await;
            let status = outcome.status;
            if level == AutonomyLevel::Full {
                assert_eq!(status, ToolExecutionStatus::Succeeded, "{}", outcome.model_content);
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
            assert_eq!(allowance.carve_ordinary_child(1)?.0, 0);
            assert_eq!(calls.load(Ordering::SeqCst), executed);

            // Parity: a live executor that drifted from the pinned catalog is
            // refused in both levels, before and at dispatch. The task-scope
            // check covers the sealed manifest's refs only; the drifted probe
            // is outside the manifest, so that check alone stays valid and
            // the live-catalog check and dispatch are what refuse it.
            drift.fetch_add(1, Ordering::SeqCst);
            assert!(publication.verify_live().is_err());
            assert!(
                publication.verify_task_scope().is_ok(),
                "the sealed manifest itself did not drift"
            );
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

const OPERATOR_TASK: &str = "Add two integers.";

async fn operator_case(level: AutonomyLevel, workspace: &tempfile::TempDir) -> anyhow::Result<()> {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.config_path = workspace.path().join("config.toml");
    config.default_provider = Some("ollama".into());
    config.default_model = Some("prx-local-model".into());
    config.autonomy.level = level;
    // The probe is denied to the operator source, so below Full it is outside
    // the frozen operator capability set. Full has no ACL limit (R51).
    config.autonomy.scopes.rules.push(crate::config::ScopeRule {
        tools_deny: vec![PROBE.to_owned()],
        ..Default::default()
    });
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
                registry: Arc::clone(&registry),
                mcp_tool: None,
                security,
                observer: Arc::new(crate::observability::noop::NoopObserver),
                hooks: Arc::new(crate::hooks::HookManager::new(workspace.path().to_owned())),
                memory: Arc::clone(&memory),
                plugin_generation: 0,
            };
            let (args, source) =
                crate::chat::binding::tests::recorded_local_operator_spawn(publication, provider, OPERATOR_TASK)
                    .await?;
            // The operator source retains its own publication; the test holds
            // no decision service. Admission holds before any change.
            source.verify_live()?;
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            source
                .scope(operator_task_cells(level, args, &registry, &calls, &drift))
                .await
        }))
        .await;
    cleanup.drain((Instant::now() + Duration::from_secs(5)).into()).await?;
    result
}

/// The operator task service, built the way production task mode builds it
/// (`claim` → `issue_task` → `task_service`), and its first segment's
/// production execution context.
async fn operator_task_cells(
    level: AutonomyLevel,
    args: serde_json::Value,
    registry: &Arc<Vec<Box<dyn crate::tools::Tool>>>,
    calls: &AtomicUsize,
    drift: &AtomicUsize,
) -> anyhow::Result<()> {
    let claimed = crate::runtime::operator_spawn::claim(&args)?.context("operator source not in scope")?;
    let binding = Arc::new(super::ChildProviderBinding::build_operator(&claimed)?);
    let mut scope = crate::runtime::envelope::RuntimeEnvelope::sessions_spawn(
        claimed.fabric().workspace_id(),
        "prx_operator_session",
        "prx_child_operator_task",
    )
    .message_scope();
    claimed.bind_input_scope(&mut scope)?;
    let receipt = super::record_child_input(
        claimed.fabric(),
        scope.clone(),
        OPERATOR_TASK,
        Some("prx-child-operator-task-input".to_owned()),
        None,
    )
    .await?;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let (authority, _lifetime) =
        claimed.issue_task(binding, &scope, OPERATOR_TASK, receipt.as_deref(), &cancellation)?;
    let service = authority
        .publication()
        .task_service(Arc::clone(&authority), Arc::clone(registry))?;
    crate::runtime::turn_cleanup::current_tracker()
        .context("operator task cleanup owner missing")?
        .retain_child_service(Arc::clone(&service))?;
    let mut segments = authority.segments()?;
    let (proof, context) = segments.record(OPERATOR_TASK).await?;
    use crate::runtime::turn_source::TurnSourceProof as _;
    // Production admits the segment turn (which stamps the call budget) and
    // binds its published specs under the turn surface before any dispatch.
    let owner = crate::runtime::turn_cleanup::current_tracker().context("operator task cleanup owner missing")?;
    let turn = crate::runtime::child_turn::ChildSegmentTurn::admit(
        Arc::clone(&service),
        (Arc::clone(&proof), context),
        claimed.fabric().clone(),
        Arc::clone(&authority.generation().effective),
        &owner,
        &cancellation,
        None,
    )
    .await?;
    let routing = turn.routing();
    let specs = service.agent_execution_specs(proof.as_ref(), routing.context)?;
    let (executor, mut context) = service.bind_agent_execution(
        proof.as_ref(),
        routing.context,
        &turn.adapter().tool_execution_context,
        &specs.specs,
    )?;
    context.turn_surface = Some(Arc::new(crate::tools::surface_owner::build(
        crate::tools::surface_owner::SurfaceParts {
            execution: service.execution(),
            catalog: service.catalog(),
            publication_policy: service.publication_policy(),
            tiering: service.tool_tiering(),
            security: service.security(),
            pinned_model: proof.model_binding().pinned_model(),
            channel: service.channel_surface(),
            inherited_exclusions: &[],
            sender: proof.policy_sender(),
            channel_name: proof.policy_channel(),
            chat_type: proof.policy_chat_type(),
            tool_calls_allowed: service.model_can_call_tools(proof.as_ref()),
        },
    )));

    // Membership: below Full the source ACL keeps the probe outside the frozen
    // operator capability set, so Supervised publishes only that set and
    // refuses the probe with no dispatch. Full bypasses the frozen set (R51):
    // the task publishes under the parent surface and executes the probe once.
    let probe = service
        .catalog()
        .entry_for_public_name(PROBE)
        .context("probe missing from the task catalog")?
        .id
        .clone();
    let policy = service.publication_policy();
    let unscoped = service
        .catalog()
        .view(&crate::tools::capability::catalog::CatalogScope::default());
    let visible = unscoped.with_surface(policy).contains(&probe);
    let outcome = executor
        .execute(
            ToolExecutionCommand::new(PROBE, serde_json::json!({"value": 1})),
            context,
            None,
        )
        .await;
    let status = outcome.status;
    if level == AutonomyLevel::Full {
        assert!(
            policy.allowed_tools.is_none(),
            "a Full operator task is not limited to the frozen set"
        );
        assert!(!policy.excluded_names.contains(PROBE));
        assert!(visible, "Full publishes the probe");
        assert_eq!(status, ToolExecutionStatus::Succeeded, "{}", outcome.model_content);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    } else {
        assert!(
            policy
                .allowed_tools
                .as_ref()
                .is_some_and(|names| names.contains("sessions_spawn") && !names.contains(PROBE)),
            "Supervised publishes exactly the frozen set"
        );
        assert!(policy.excluded_names.contains(PROBE));
        assert!(!visible, "Supervised does not publish the probe");
        assert_ne!(status, ToolExecutionStatus::Succeeded, "{}", outcome.model_content);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    let executed = calls.load(Ordering::SeqCst);
    // The segment turn closes before the resource and parity cells change
    // its funding and executor.
    turn.fail(&anyhow::anyhow!("operator task cells close the segment"), false)
        .await?;

    // Recursive money is a resource bound, not a permission: an exhausted
    // task or parent allowance funds no child share in Full as in
    // Supervised; nothing dispatches.
    authority.allowance().money().store(0, Ordering::Release);
    assert_eq!(authority.allowance().carve_ordinary_child(1)?.0, 0);
    let parent = &authority.publication().parent_authority().allowance;
    parent.money().store(0, Ordering::Release);
    assert_eq!(parent.carve_ordinary_child(1)?.0, 0);
    assert_eq!(calls.load(Ordering::SeqCst), executed);

    // Parity: drift of the live executor refuses the operator source, the
    // task authority and a new task service in both levels, with no dispatch.
    drift.fetch_add(1, Ordering::SeqCst);
    assert!(claimed.verify_live().is_err());
    assert!(authority.verify().is_err());
    assert!(
        authority
            .publication()
            .task_service(Arc::clone(&authority), Arc::clone(registry))
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), executed);
    Ok(())
}

#[tokio::test]
async fn child_publication_survives_without_turn_decision_service() -> anyhow::Result<()> {
    for level in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
        ordinary_case(level, &tempfile::tempdir()?)
            .await
            .with_context(|| format!("ordinary child, {level:?}"))?;
        operator_case(level, &tempfile::tempdir()?)
            .await
            .with_context(|| format!("operator child, {level:?}"))?;
    }
    Ok(())
}

/// P2-R18: every child kind issues through the ordinary or operator
/// publication with no decision record. Each cell checks that no legacy
/// decision artifact (`.decision_key`, `decisions.db`, `journal.db`) exists
/// afterwards, whatever the decision-plane setting, and a pre-strip delegated
/// manifest no longer decodes.
#[tokio::test]
async fn every_child_kind_issues_without_decision_records() -> anyhow::Result<()> {
    fn assert_no_decision_records(workspace: &std::path::Path, cell: &str) {
        for relative in [
            "self/causal_tree/.decision_key",
            "self/causal_tree/decisions.db",
            "self/child_authority/journal.db",
        ] {
            assert!(
                std::fs::symlink_metadata(workspace.join(relative)).is_err(),
                "{cell}: {relative} must not exist"
            );
        }
    }
    for level in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
        // sessions_spawn and delegate children of an ordinary parent share
        // the ordinary child publication (task, process and batch launches
        // derive from it); operator children use the operator publication.
        let ordinary = tempfile::tempdir()?;
        ordinary_case(level, &ordinary)
            .await
            .with_context(|| format!("ordinary child, {level:?}"))?;
        assert_no_decision_records(ordinary.path(), "ordinary child");
        let operator = tempfile::tempdir()?;
        operator_case(level, &operator)
            .await
            .with_context(|| format!("operator child, {level:?}"))?;
        assert_no_decision_records(operator.path(), "operator child");
    }
    // A pre-strip delegated worker manifest carries retired fields and is
    // refused at decode (WorkerManifestUnsupported at the snapshot reader).
    let scratch = tempfile::tempdir()?;
    let manifest = crate::session_worker::runner::tests::base_manifest(scratch.path(), "capability");
    for retired in ["child_source", "capability_manifest"] {
        let mut value = serde_json::to_value(&manifest)?;
        value
            .as_object_mut()
            .context("worker manifest serializes as an object")?
            .insert(retired.to_owned(), serde_json::json!({}));
        assert!(
            serde_json::from_value::<crate::session_worker::protocol::WorkerManifest>(value).is_err(),
            "a manifest with `{retired}` must not decode"
        );
    }
    assert!(
        serde_json::from_value::<crate::session_worker::protocol::WorkerManifest>(serde_json::to_value(&manifest)?)
            .is_ok()
    );
    assert_no_decision_records(scratch.path(), "manifest decode");
    Ok(())
}

/// P1-05: an enabled engine builds the advisory driver of a child
/// publication (its analysis then runs on the child's own runtime context and
/// accounting scope); a disabled engine builds no factory (P1-I2).
#[tokio::test]
async fn child_publication_builds_the_advisory_driver_only_when_enabled() -> anyhow::Result<()> {
    for enabled in [false, true] {
        let workspace = tempfile::tempdir()?;
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path())?);
        let cleanup = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
        let result = cleanup
            .scope(Box::pin(async {
                let parent = ordinary_parent(
                    workspace.path(),
                    AutonomyLevel::Supervised,
                    Arc::clone(&memory),
                    "prx-child-advisory",
                    enabled,
                )
                .await;
                let (_execution, bound) = parent.admitted.bind_selected("sessions_spawn")?;
                let publication = bound
                    .ordinary_child_publication
                    .clone()
                    .context("ordinary parent binding captured no child publication")?;
                let child = publication.child_service(&bound, None)?;
                assert_eq!(
                    child.advisory().factory().is_some(),
                    enabled,
                    "child advisory factory follows the engine switch"
                );
                Ok::<(), anyhow::Error>(())
            }))
            .await;
        cleanup.drain((Instant::now() + Duration::from_secs(5)).into()).await?;
        result.with_context(|| format!("engine enabled: {enabled}"))?;
    }
    Ok(())
}

/// HMAC-SHA256 over `lp(label) ‖ Σ lp(part)`, computed with `ring` directly.
fn golden_state_fingerprint(key: &[u8], parts: &[&[u8]]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    let mut context = ring::hmac::Context::with_key(&key);
    for part in std::iter::once(b"prx.decision.state_fingerprint.v1".as_slice()).chain(parts.iter().copied()) {
        context.update(&(part.len() as u64).to_be_bytes());
        context.update(part);
    }
    hex::encode(context.sign().as_ref())
}

fn golden_hex(value: &str) -> Vec<u8> {
    hex::decode(value).expect("test: golden hex literal")
}

/// D§4.5: `K_child`, `K_batch`, the batch seal and the segment seal. Expected
/// values are independently computed literals, never production output.
#[test]
fn golden_child_seal_vectors() {
    use crate::security::runtime_key::{DecisionKey, KeyDomain};
    let transport: Vec<u8> = (0x20u8..=0x3f).collect();
    let transport = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &transport);

    const K_CHILD: &str = "27d4adfc09c6ddf85b638f9f1a9e0870988240161549824fd1e11da715cdec4f";
    const K_BATCH: &str = "e4184f14d8a0d42c0f17b4bd527834eb34450edbf753e106c3d0da296db78b01";
    const BATCH_SEAL: &str = "edbc083f3912ad65d4f089e6ca4ab409791c85d7caece2e62d32f6d94b42a0b3";
    const SEGMENT_SEAL: &str = "03ce00b207929043257d261dac5a88856098e4ed5c21d34dc9f5299794f6e69e";

    // The production key equals the literal K_child: identical HMACs.
    let child = DecisionKey::from_worker_channel(&transport, "ws-1", "run-1", "gen-1").expect("test: K_child");
    assert_eq!(
        hex::encode(child.hmac(KeyDomain::StateFingerprint, &[b"probe"])),
        golden_state_fingerprint(&golden_hex(K_CHILD), &[b"probe"])
    );
    let revision = "f".repeat(64);
    let batch_key = DecisionKey::from_worker_channel(&transport, "ws-1", "batch-1", &revision).expect("test: K_batch");
    assert_eq!(
        hex::encode(batch_key.hmac(KeyDomain::StateFingerprint, &[b"probe"])),
        golden_state_fingerprint(&golden_hex(K_BATCH), &[b"probe"])
    );

    let batch_seal = crate::tools::sessions_spawn::golden_plan_seal(
        &batch_key,
        &crate::tools::sessions_spawn::GoldenPlan {
            workspace: "ws-1",
            owner: "owner-1",
            batch: "batch-1",
            parent_run: "run-0",
            parent_session: "sess-0",
            config_generation: 1,
            config_revision: &revision,
            members: &[("run-1", "t")],
        },
    )
    .expect("test: batch seal");
    assert_eq!(hex::encode(batch_seal), BATCH_SEAL);

    let segment = crate::runtime::child_source::ordinary_process::journal::SegmentEvidence {
        version: 1,
        task_run_id: "run-1".into(),
        process_id: "proc-1".into(),
        index: 0,
        input: None,
        terminal: None,
        seal: [0; 32],
    };
    assert_eq!(
        segment.signing_bytes().expect("test: segment bytes"),
        br#"[1,"run-1","proc-1",0,null,null]"#.to_vec()
    );
    let seal = segment
        .seal_under(&child, "terminal", "scope-1")
        .expect("test: segment seal");
    assert_eq!(hex::encode(seal), SEGMENT_SEAL);
}

/// D§3.3: the constraints tuple (with and without the delegation suffix) and
/// the effective-config payload against hand-written byte literals; the seals
/// are recomputed with `ring` over those literals.
#[test]
fn golden_constraints_and_effective_config_payloads() {
    use crate::tools::capability::child::wire::{WorkerDelegationAuthority, WorkerParentConstraints};
    let mut wire = WorkerParentConstraints {
        decision_id: "00000000-0000-4000-8000-000000000001".into(),
        allowed: Vec::new(),
        owner: "owner-1".into(),
        workspace: "ws-1".into(),
        level: crate::security::AutonomyLevel::Supervised,
        max_cost_cents: 25,
        remaining_depth: 2,
        delegation: None,
        catalog_generation: [7; 32],
        seal: [0; 32],
    };
    let without = concat!(
        r#"["00000000-0000-4000-8000-000000000001",[],"owner-1","ws-1","supervised",25,2,"#,
        "[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7]]"
    );
    let payload = crate::tools::capability::child::worker_constraints_signing_payload(&wire)
        .map_err(|_| anyhow::anyhow!("constraints payload"))
        .expect("test: constraints payload");
    assert_eq!(payload, without.as_bytes());
    // The production key and helper seal the production payload exactly as
    // `ring` seals the literal under the literal K_child.
    let transport: Vec<u8> = (0x20u8..=0x3f).collect();
    let child = crate::security::runtime_key::DecisionKey::from_worker_channel(
        &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &transport),
        "ws-1",
        "run-1",
        "gen-1",
    )
    .expect("test: K_child");
    let k_child = golden_hex("27d4adfc09c6ddf85b638f9f1a9e0870988240161549824fd1e11da715cdec4f");
    assert_eq!(
        hex::encode(child.hmac(
            crate::security::runtime_key::KeyDomain::StateFingerprint,
            &[b"worker-parent-constraints-v1", &payload]
        )),
        golden_state_fingerprint(&k_child, &[b"worker-parent-constraints-v1", without.as_bytes()])
    );

    wire.delegation = Some(WorkerDelegationAuthority {
        requested: true,
        provider: "openai".into(),
        model: "gpt-x".into(),
    });
    let with = format!(
        "{without}\nrecursive-delegation-v1\n{}",
        r#"{"requested":true,"provider":"openai","model":"gpt-x"}"#
    );
    let payload = crate::tools::capability::child::worker_constraints_signing_payload(&wire)
        .map_err(|_| anyhow::anyhow!("constraints payload"))
        .expect("test: delegated constraints payload");
    assert_eq!(payload, with.as_bytes());
    assert_eq!(
        hex::encode(child.hmac(
            crate::security::runtime_key::KeyDomain::StateFingerprint,
            &[b"worker-parent-constraints-v1", &payload]
        )),
        golden_state_fingerprint(&k_child, &[b"worker-parent-constraints-v1", with.as_bytes()])
    );

    // Effective config: the metadata prefix is a literal; nested maps inserted
    // out of order serialize sorted.
    let mut config = crate::config::Config::default();
    config.workspace_dir = std::path::PathBuf::new();
    config.config_path = std::path::PathBuf::new();
    for model in ["zeta/model", "alpha/model"] {
        config.cost.prices.insert(
            model.to_owned(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 2.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
    }
    let binding = crate::config::generation::ProcessGenerationBinding {
        working_root: None,
        id: crate::config::generation::ConfigGenerationId(7),
        source_revision: crate::config::generation::ConfigSourceRevision {
            fingerprint_sha256: "f".repeat(64),
            disk_generation: None,
        },
        applied_at: chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .expect("test: timestamp")
            .with_timezone(&chrono::Utc),
        trigger: crate::config::generation::ConfigReloadTrigger::Startup,
        deferred_changes: Vec::new(),
        seal: [0; 32],
    };
    let payload = binding
        .signing_payload(&config)
        .expect("test: effective config payload");
    let prefix = format!(
        r#"[7,{{"disk_generation":null,"fingerprint_sha256":"{}"}},"2026-01-02T03:04:05Z","startup",[],"","",{{"#,
        "f".repeat(64)
    );
    assert!(
        payload.starts_with(prefix.as_bytes()),
        "effective-config metadata prefix differs"
    );
    let text = std::str::from_utf8(&payload).expect("test: utf-8 payload");
    let alpha = text.find(r#""alpha/model":{"#).expect("test: alpha price");
    let zeta = text.find(r#""zeta/model":{"#).expect("test: zeta price");
    assert!(
        alpha < zeta,
        "nested maps serialize sorted regardless of insertion order"
    );
    let value: serde_json::Value = serde_json::from_slice(&payload).expect("test: payload JSON");
    let mut resorted = value.clone();
    resorted.sort_all_objects();
    assert_eq!(
        serde_json::to_vec(&resorted).expect("test: resorted"),
        payload,
        "every object is sorted"
    );
    assert_eq!(
        hex::encode(child.hmac(
            crate::security::runtime_key::KeyDomain::StateFingerprint,
            &[b"process-effective-config-v1", &payload]
        )),
        golden_state_fingerprint(&k_child, &[b"process-effective-config-v1", &payload])
    );
}

use super::*;
use crate::tools::execution::admission::TurnSurface;

fn allow_service(fixture: &Fixture) -> ToolExecutionService {
    fixture.service("config_reload", vec![], ToolExecutionDecision::Allow, approved(), true)
}

fn surface_context(
    service: &ToolExecutionService,
    config: &crate::config::ToolTieringConfig,
    channel: bool,
    eligible: bool,
) -> ToolExecutionContext {
    let mut context = context();
    context.turn_surface = Some(Arc::new(TurnSurface::build(
        service,
        config,
        "prx_model",
        channel,
        |_| eligible,
    )));
    assert!(context.execution_bindings.is_none());
    context
}

async fn assert_refused(service: &ToolExecutionService, fixture: &Fixture, context: ToolExecutionContext, name: &str) {
    let outcome = service
        .execute(
            ToolExecutionCommand::new(name, serde_json::json!({"value":1})),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(&*fixture.stages.lock(), &["audit"]);
}

#[tokio::test]
async fn excluded_tool_called_by_model_is_refused() {
    let fixture = fixture();
    let service = allow_service(&fixture);
    let mut config = crate::config::ToolTieringConfig::default();
    config.always_exclude.push("config_reload".into());
    let context = surface_context(&service, &config, false, true);
    assert_refused(&service, &fixture, context, "config_reload").await;
}

#[tokio::test]
async fn model_allowlist_excluded_tool_is_refused() {
    let fixture = fixture();
    let service = allow_service(&fixture);
    let mut config = crate::config::ToolTieringConfig::default();
    config
        .model_allowlists
        .insert("prx_model".into(), vec!["project_tool".into()]);
    let context = surface_context(&service, &config, false, true);
    assert_refused(&service, &fixture, context, "config_reload").await;
}

fn channel_message() -> crate::channels::traits::ChannelMessage {
    crate::channels::traits::ChannelMessage {
        id: "prx-channel-surface".to_owned(),
        sender: "prx_user".to_owned(),
        reply_target: "prx_channel".to_owned(),
        content: "run the project tool".to_owned(),
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

/// The channel surface comes from the production turn service, whose catalog
/// entries carry `channel_eligible`, not from a caller-supplied closure.
#[tokio::test]
async fn channel_ineligible_tool_is_refused_on_channel_turn() {
    let workspace = tempfile::tempdir().expect("channel workspace");
    let fixture = fixture();
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.tool_tiering.channel_exclude.push("project_tool".into());
    let registry: Arc<Vec<Box<dyn crate::tools::Tool>>> = Arc::new(vec![Box::new(RecordingTool {
        root_name: "project_tool",
        aliases: vec![],
        calls: Arc::clone(&fixture.calls),
        arguments: Arc::clone(&fixture.arguments),
        stages: Arc::clone(&fixture.stages),
    }) as Box<dyn crate::tools::Tool>]);
    let admitted = crate::agent::test_admission::AdmittedTurn::with_channel(config, channel_message(), registry).await;
    let decision = admitted.decision();
    let surface = decision.service.turn_tool_surface(decision.proof, &[]);
    assert!(!surface.contains("project_tool"), "channel-excluded tool was published");
}

#[tokio::test]
async fn owner_isolated_origin_denies_non_isolated_tool() {
    let fixture = fixture();
    let service = allow_service(&fixture);
    let mut context = surface_context(&service, &crate::config::ToolTieringConfig::default(), false, true);
    context.envelope.owner_isolated = true;
    assert_refused(&service, &fixture, context, "config_reload").await;
}

struct FileSpy(RecordingTool);
#[async_trait]
impl Tool for FileSpy {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn description(&self) -> &str {
        self.0.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.0.parameters_schema()
    }
    fn supports_owner_filesystem(&self) -> bool {
        true
    }
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.0.execute(args).await
    }
}

#[tokio::test]
async fn owner_scope_mismatch_cannot_use_private_filesystem() {
    let fixture = fixture();
    let service = ToolExecutionService::new(
        vec![Arc::new(FileSpy(RecordingTool {
            root_name: "file_write",
            aliases: vec![],
            calls: Arc::clone(&fixture.calls),
            arguments: Arc::clone(&fixture.arguments),
            stages: Arc::clone(&fixture.stages),
        }))],
        Arc::new(FixedPolicy {
            decision: ToolExecutionDecision::Allow,
            stages: Arc::clone(&fixture.stages),
        }),
        Arc::new(FixedApproval {
            decision: approved(),
            stages: Arc::clone(&fixture.stages),
        }),
        Arc::new(RecordingPreparation {
            allowed: true,
            stages: Arc::clone(&fixture.stages),
        }),
        Arc::new(RecordingAudit {
            records: Arc::clone(&fixture.records),
            stages: Arc::clone(&fixture.stages),
        }),
    );
    let private = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    let scope = Arc::new(
        crate::tools::owner_filesystem::OwnerFilesystem::open(
            private.path(),
            "prx_workspace",
            "prx_owner_a",
            shared.path(),
        )
        .unwrap(),
    );
    for (workspace, owner) in [("prx_workspace", "prx_owner_b"), ("prx_other_workspace", "prx_owner_a")] {
        let mut context = surface_context(&service, &crate::config::ToolTieringConfig::default(), false, true);
        context.envelope = RuntimeEnvelope::console(workspace, "prx_session")
            .with_owner_id(owner)
            .with_owner_isolation();
        context.owner_filesystem = Some(Arc::clone(&scope));
        fixture.stages.lock().clear();
        assert_refused(&service, &fixture, context, "file_write").await;
    }
}

#[tokio::test]
async fn admitted_tool_reaches_backend_without_decision_context() {
    let fixture = fixture();
    let service = allow_service(&fixture);
    let context = surface_context(&service, &crate::config::ToolTieringConfig::default(), false, true);
    assert_eq!(
        service.execute(command(1), context, None).await.status,
        ToolExecutionStatus::Succeeded
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

struct MutableSurfaceTool {
    state: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for MutableSurfaceTool {
    fn name(&self) -> &str {
        "project_tool"
    }
    fn description(&self) -> &str {
        "mutable publication fixture"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        if self.state.load(Ordering::SeqCst) == 1 {
            serde_json::json!({"type":"object", "properties":{"changed":{"type":"string"}}})
        } else {
            serde_json::json!({"type":"object", "properties":{}})
        }
    }
    fn specs(&self) -> Vec<crate::tools::ToolSpec> {
        if self.state.load(Ordering::SeqCst) == 5 {
            return Vec::new();
        }
        let mut specs = vec![self.spec()];
        if self.state.load(Ordering::SeqCst) == 2 {
            let mut extra = self.spec();
            extra.name = "project_extra".into();
            specs.push(extra);
        }
        specs
    }
    fn catalog_binding_snapshot(&self) -> std::collections::BTreeMap<String, [u8; 32]> {
        if self.state.load(Ordering::SeqCst) == 4 {
            std::collections::BTreeMap::from([("project_tool".to_owned(), [7u8; 32])])
        } else {
            std::collections::BTreeMap::new()
        }
    }
    fn availability(&self) -> crate::capability::CapabilityAvailability {
        if self.state.load(Ordering::SeqCst) == 3 {
            crate::capability::CapabilityAvailability::declared("fixture unavailable")
        } else {
            crate::capability::CapabilityAvailability::configured("fixture configured")
        }
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            success: true,
            output: "ok".into(),
            error: None,
        })
    }
}
async fn assert_stale_after_change(state_value: usize) {
    assert_stale_between(0, state_value).await;
}
/// Publish at `build_state`, then admit at `admit_state`. Equal states keep the
/// registry generation stable, so only the per-tool checks can refuse the call.
async fn assert_stale_between(build_state: usize, admit_state: usize) {
    let fixture = fixture();
    let state = Arc::new(AtomicUsize::new(build_state));
    let service = ToolExecutionService::new(
        vec![Arc::new(MutableSurfaceTool {
            state: Arc::clone(&state),
            calls: Arc::clone(&fixture.calls),
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
            allowed: true,
            stages: Arc::clone(&fixture.stages),
        }),
        Arc::new(RecordingAudit {
            records: Arc::clone(&fixture.records),
            stages: Arc::clone(&fixture.stages),
        }),
    );
    let context = surface_context(&service, &crate::config::ToolTieringConfig::default(), false, true);
    state.store(admit_state, Ordering::SeqCst);
    let outcome = service
        .execute(
            ToolExecutionCommand::new("project_tool", serde_json::json!({})),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::StaleCapability);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(&*fixture.stages.lock(), &["audit"]);
}
#[tokio::test]
async fn registry_generation_change_mid_turn_is_stale() {
    assert_stale_after_change(2).await;
}
#[tokio::test]
async fn unavailable_tool_is_refused() {
    assert_stale_after_change(3).await;
}
#[tokio::test]
async fn schema_changed_after_publication_is_stale() {
    assert_stale_after_change(1).await;
}
/// Availability alone: declared at publication and at admission, so the
/// generation and fingerprint match and only the availability check refuses.
#[tokio::test]
async fn unavailable_tool_alone_is_refused() {
    assert_stale_between(3, 3).await;
}
/// Binding digest alone: the descriptor and schema are unchanged, only the
/// backend binding appears after publication.
#[tokio::test]
async fn binding_drift_after_publication_is_stale() {
    assert_stale_between(0, 4).await;
}
/// A tool that vanishes after publication has no current descriptor.
#[tokio::test]
async fn disappeared_tool_after_publication_is_stale() {
    assert_stale_between(0, 5).await;
}

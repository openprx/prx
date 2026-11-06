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
        &service.descriptors(),
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

#[tokio::test]
async fn channel_ineligible_tool_is_refused_on_channel_turn() {
    let fixture = fixture();
    let service = allow_service(&fixture);
    let context = surface_context(&service, &crate::config::ToolTieringConfig::default(), true, false);
    assert_refused(&service, &fixture, context, "config_reload").await;
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

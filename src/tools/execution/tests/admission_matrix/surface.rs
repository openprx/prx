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
    context.turn_surface = Some(Arc::new(TurnSurface::build_from(
        service,
        service.current_descriptors(),
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

/// Eligibility is applied once, in `TurnSurface::build_from`. An ineligible entry is
/// never published, so the surface does not contain it and dispatch is Denied.
#[tokio::test]
async fn channel_ineligible_entry_is_refused_on_channel_surface() {
    let fixture = fixture();
    let service = allow_service(&fixture);
    let context = surface_context(&service, &crate::config::ToolTieringConfig::default(), true, false);
    assert!(
        context
            .turn_surface
            .as_ref()
            .is_some_and(|surface| !surface.contains("config_reload")),
        "channel-ineligible entry was published"
    );
    assert_refused(&service, &fixture, context, "config_reload").await;
}

/// A name that no backend registers is not a surface violation: resolution
/// fails before any backend runs, so the registry's unknown-tool diagnostic
/// is preserved and nothing dispatches.
#[tokio::test]
async fn unregistered_unpublished_name_keeps_unknown_tool_diagnostic() {
    let fixture = fixture();
    let service = allow_service(&fixture);
    let context = surface_context(&service, &crate::config::ToolTieringConfig::default(), false, true);
    let outcome = service
        .execute(
            ToolExecutionCommand::new("invented_probe", serde_json::json!({"value":1})),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::UnknownTool);
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("unknown tool 'invented_probe'")),
        "unexpected diagnostic: {:?}",
        outcome.error
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

/// A registered tool that the surface never published stays Denied as
/// outside the turn surface. The unknown-name fall-through must not widen this.
#[tokio::test]
async fn registered_unpublished_tool_is_denied_outside_surface() {
    let fixture = fixture();
    let service = allow_service(&fixture);
    let context = surface_context(&service, &crate::config::ToolTieringConfig::default(), false, false);
    let outcome = service
        .execute(
            ToolExecutionCommand::new("config_reload", serde_json::json!({"value":1})),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("outside the turn surface")),
        "unexpected diagnostic: {:?}",
        outcome.error
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
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

/// `channel_exclude` is folded at publication. The excluded name is never
/// published, so dispatch is Denied with no backend call and no decision context.
#[tokio::test]
async fn channel_ineligible_tool_is_refused_on_channel_turn() {
    let fixture = fixture();
    let service = allow_service(&fixture);
    let mut config = crate::config::ToolTieringConfig::default();
    config.channel_exclude.push("config_reload".into());
    let context = surface_context(&service, &config, true, true);
    assert!(
        context
            .turn_surface
            .as_ref()
            .is_some_and(|surface| !surface.contains("config_reload")),
        "channel-excluded tool was published"
    );
    assert_refused(&service, &fixture, context, "config_reload").await;
}

/// The channel surface comes from the production surface owner, whose catalog
/// entries carry `channel_eligible`, not from a caller-supplied closure.
#[tokio::test]
async fn production_channel_surface_omits_channel_excluded_tool() {
    let fixture = fixture();
    let tools: Vec<Box<dyn crate::tools::Tool>> = vec![Box::new(RecordingTool {
        root_name: "project_tool",
        aliases: vec![],
        calls: Arc::clone(&fixture.calls),
        arguments: Arc::clone(&fixture.arguments),
        stages: Arc::clone(&fixture.stages),
    })];
    let publication_policy = crate::tools::capability::catalog::SurfacePolicy {
        channel_allowed: true,
        ..crate::tools::capability::catalog::SurfacePolicy::default()
    };
    let catalog = crate::tools::capability::catalog::CapabilityCatalog::build(
        &tools,
        &crate::tools::capability::catalog::CatalogSources::default(),
        &publication_policy,
    )
    .expect("catalog");
    let mut tiering = crate::config::ToolTieringConfig::default();
    tiering.channel_exclude.push("project_tool".into());
    let security = crate::security::SecurityPolicy::default();
    let service = crate::tools::ToolExecutionService::from_boxed(
        tools,
        Arc::new(crate::tools::SecurityEffectPolicy::new(Arc::new(security.clone()))),
        Arc::new(crate::tools::DenyApprovalStrategy),
        Arc::new(crate::tools::AdapterOwnedPreparation),
        Arc::new(crate::tools::TracingToolExecutionAudit),
    );
    let surface = crate::tools::surface_owner::build(crate::tools::surface_owner::SurfaceParts {
        execution: &service,
        catalog: &catalog,
        publication_policy: &publication_policy,
        tiering: &tiering,
        security: &security,
        pinned_model: "prx_model",
        channel: true,
        inherited_exclusions: &[],
        sender: "prx_user",
        channel_name: "wacli",
        chat_type: "dm",
        tool_calls_allowed: true,
    });
    assert!(!surface.contains("project_tool"), "channel-excluded tool was published");
}

#[tokio::test]
async fn model_turn_without_surface_is_denied() {
    let fixture = fixture();
    let service = allow_service(&fixture);
    let mut context = context();
    context.dispatch_origin = crate::tools::DispatchOrigin::ModelTurn;
    assert!(context.turn_surface.is_none());
    let outcome = service
        .execute(
            ToolExecutionCommand::new("config_reload", serde_json::json!({"value":1})),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("no turn surface")),
        "unexpected diagnostic: {:?}",
        outcome.error
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

/// Console resume replays a model tool call: ModelTurn plus a surface built
/// from the binding, registry, ACL, config and catalog. An excluded name is
/// Denied with no backend call.
#[tokio::test]
async fn console_resume_replays_model_turn_with_surface() {
    let fixture = fixture();
    let tools: Vec<Box<dyn crate::tools::Tool>> = vec![Box::new(RecordingTool {
        root_name: "project_tool",
        aliases: vec![],
        calls: Arc::clone(&fixture.calls),
        arguments: Arc::clone(&fixture.arguments),
        stages: Arc::clone(&fixture.stages),
    })];
    let publication_policy = crate::tools::capability::catalog::SurfacePolicy {
        channel_allowed: true,
        ..crate::tools::capability::catalog::SurfacePolicy::default()
    };
    let catalog = crate::tools::capability::catalog::CapabilityCatalog::build(
        &tools,
        &crate::tools::capability::catalog::CatalogSources::default(),
        &publication_policy,
    )
    .expect("catalog");
    let mut tiering = crate::config::ToolTieringConfig::default();
    tiering.channel_exclude.push("project_tool".into());
    let security = crate::security::SecurityPolicy::default();
    let service = crate::tools::ToolExecutionService::from_boxed(
        tools,
        Arc::new(crate::tools::SecurityEffectPolicy::new(Arc::new(security.clone()))),
        Arc::new(crate::tools::DenyApprovalStrategy),
        Arc::new(crate::tools::AdapterOwnedPreparation),
        Arc::new(crate::tools::TracingToolExecutionAudit),
    );
    let envelope = crate::runtime::envelope::RuntimeEnvelope::console("prx_workspace", "prx_session");
    let context = crate::tools::surface_owner::model_turn_context(
        envelope,
        "private",
        crate::tools::surface_owner::SurfaceParts {
            execution: &service,
            catalog: &catalog,
            publication_policy: &publication_policy,
            tiering: &tiering,
            security: &security,
            pinned_model: "prx_model",
            channel: true,
            inherited_exclusions: &[],
            sender: "prx_user",
            channel_name: "wacli",
            chat_type: "dm",
            tool_calls_allowed: true,
        },
    );
    assert_eq!(context.dispatch_origin, crate::tools::DispatchOrigin::ModelTurn);
    assert!(context.turn_surface.is_some());
    let outcome = service
        .execute(
            ToolExecutionCommand::new("project_tool", serde_json::json!({"value": 1})),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn operator_dispatch_without_surface_is_unchanged() {
    let fixture = fixture();
    let service = allow_service(&fixture);
    let mut context = context();
    context.dispatch_origin = crate::tools::DispatchOrigin::Operator;
    assert!(context.turn_surface.is_none());
    assert_eq!(
        service.execute(command(1), context, None).await.status,
        ToolExecutionStatus::Succeeded
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

/// Control: an empty `channel_exclude` publishes the tool on a channel turn
/// and dispatch reaches the backend without a decision context.
#[tokio::test]
async fn channel_turn_admits_tool_not_channel_excluded() {
    let fixture = fixture();
    let service = allow_service(&fixture);
    let mut config = crate::config::ToolTieringConfig::default();
    config.channel_exclude.clear();
    let context = surface_context(&service, &config, true, true);
    assert_eq!(
        service.execute(command(1), context, None).await.status,
        ToolExecutionStatus::Succeeded
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
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
fn mutable_service(fixture: &Fixture, state: &Arc<AtomicUsize>) -> ToolExecutionService {
    ToolExecutionService::new(
        vec![Arc::new(MutableSurfaceTool {
            state: Arc::clone(state),
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
    )
}

/// Two dispatches of one published turn. The backend owns its state and the
/// service is never told, so the second dispatch must see the drift.
#[tokio::test]
async fn backend_state_change_between_dispatches_is_stale() {
    let fixture = fixture();
    let state = Arc::new(AtomicUsize::new(0));
    let service = mutable_service(&fixture, &state);
    let context = surface_context(&service, &crate::config::ToolTieringConfig::default(), false, true);
    let call = || ToolExecutionCommand::new("project_tool", serde_json::json!({}));
    assert_eq!(
        service.execute(call(), context.clone(), None).await.status,
        ToolExecutionStatus::Succeeded
    );
    state.store(2, Ordering::SeqCst);
    assert_eq!(
        service.execute(call(), context, None).await.status,
        ToolExecutionStatus::StaleCapability
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

/// Control for the test above: with no backend change, the same surface is
/// reused across two dispatches and both succeed.
#[tokio::test]
async fn unchanged_backend_admits_repeated_dispatches() {
    let fixture = fixture();
    let state = Arc::new(AtomicUsize::new(0));
    let service = mutable_service(&fixture, &state);
    let context = surface_context(&service, &crate::config::ToolTieringConfig::default(), false, true);
    for _ in 0..2 {
        assert_eq!(
            service
                .execute(
                    ToolExecutionCommand::new("project_tool", serde_json::json!({})),
                    context.clone(),
                    None
                )
                .await
                .status,
            ToolExecutionStatus::Succeeded
        );
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
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

/// `admit` lets an unpublished, unregistered name through; a backend can
/// register it before `resolve`. The post-resolve recheck must then deny it.
#[test]
fn recheck_resolved_denies_registered_name_outside_surface() {
    let fixture = fixture();
    let service = allow_service(&fixture);
    let context = surface_context(&service, &crate::config::ToolTieringConfig::default(), true, false);
    assert!(
        context
            .turn_surface
            .as_ref()
            .is_some_and(|surface| !surface.contains("config_reload")),
        "fixture must leave config_reload unpublished"
    );
    assert!(service.current_descriptor("config_reload").is_some());
    let (status, message) = crate::tools::execution::admission::recheck_resolved(&context, "config_reload")
        .expect_err("a resolved name outside the surface must be denied");
    assert_eq!(status, ToolExecutionStatus::Denied);
    assert!(message.contains("outside the turn surface"), "{message}");
}

#[test]
fn recheck_resolved_passes_published_name() {
    let fixture = fixture();
    let service = allow_service(&fixture);
    let context = surface_context(&service, &crate::config::ToolTieringConfig::default(), false, true);
    assert!(
        context
            .turn_surface
            .as_ref()
            .is_some_and(|surface| surface.contains("config_reload"))
    );
    assert!(crate::tools::execution::admission::recheck_resolved(&context, "config_reload").is_ok());
}

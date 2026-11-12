use super::super::{FixedApproval, FixedPolicy, RecordingAudit, RecordingPreparation};
use super::*;
use crate::security::policy::AutonomyLevel;
use crate::tools::capability::execution_binding::ExecutionBinding;
use crate::tools::capability::execution_trace::ExecutionTrace;

fn autonomy_service(fixture: &Fixture, name: &'static str, autonomy: AutonomyLevel) -> ToolExecutionService {
    let mut service = fixture.service(name, vec![], ToolExecutionDecision::Ask, approved(), true);
    service.policy = Arc::new(SecurityEffectPolicy::new(Arc::new(SecurityPolicy {
        autonomy,
        ..SecurityPolicy::default()
    })));
    service.approval = Arc::new(DenyApprovalStrategy);
    service
}

fn issued_binding(fixture: &Fixture, name: &'static str, envelope: RuntimeEnvelope) -> ExecutionBinding {
    let tool = RecordingTool {
        root_name: name,
        aliases: vec![],
        calls: Arc::clone(&fixture.calls),
        arguments: Arc::clone(&fixture.arguments),
        stages: Arc::clone(&fixture.stages),
    };
    let catalog = Arc::new(
        CapabilityCatalog::build(
            &[Box::new(tool) as Box<dyn Tool>],
            &CatalogSources::default(),
            &SurfacePolicy::default(),
        )
        .expect("test: catalog for binding pin"),
    );
    let entry = catalog
        .entry_for_public_name(name)
        .expect("test: catalog entry for binding pin");
    ExecutionBinding::issued(
        crate::causal_tree::ids::DecisionId::new_runtime(),
        entry,
        Arc::clone(&catalog),
        envelope,
        true,
    )
}

/// Supervised still Asks for a discovered tool when no admission pin is present.
#[tokio::test]
async fn discovered_tool_still_asks_under_supervised() {
    let fixture = fixture();
    let service = autonomy_service(&fixture, "project_tool", AutonomyLevel::Supervised);
    let context = context();
    assert!(context.admitted_binding.is_none());
    let outcome = service
        .execute(
            ToolExecutionCommand::new("project_tool", serde_json::json!({"value": 1})),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.decision, Some(ToolExecutionDecision::Ask));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn experiment_action_without_h2_origin_refuses() {
    let fixture = fixture();
    let service = autonomy_service(&fixture, "skills_manage", AutonomyLevel::Full);
    let context = context();
    assert!(context.h2_origin().is_none());
    let outcome = service
        .execute(
            ToolExecutionCommand::new(
                "skills_manage",
                serde_json::json!({"value": 1, "action": "lifecycle_experiment_catalog"}),
            ),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert!(
        outcome
            .model_content
            .contains("experiment action has no actual controlled source"),
        "{}",
        outcome.model_content
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn experiment_action_with_admitted_descriptor_and_h2_origin_proceeds() {
    let (workspace, fabric, chat_binding, origin) =
        crate::runtime::h2_entry::recorded_terminal_approval_fixture().await;
    let workspace_id = origin.record().workspace.clone();
    let run_id = origin.record().run.clone();
    let session = origin.record().session.clone();
    let actor = origin.record().actor.clone();
    let event = origin.record().event.clone();
    let fixture = fixture();
    let service = autonomy_service(&fixture, "skills_manage", AutonomyLevel::Full);
    let mut envelope = RuntimeEnvelope::agent(&workspace_id, &run_id)
        .with_channel("terminal")
        .with_sender("local-user")
        .with_owner_id(&actor)
        .with_source_message_event_id(&event);
    envelope.session_key = session;
    let mut context = context();
    context.envelope = envelope;
    context.set_h2_origin(Some(origin));
    context.execution_admission = Some(issued_binding(&fixture, "skills_manage", context.envelope.clone()).admission());
    let outcome = service
        .execute(
            ToolExecutionCommand::new(
                "skills_manage",
                serde_json::json!({"value": 1, "action": "lifecycle_experiment_catalog"}),
            ),
            context,
            None,
        )
        .await;
    drop((workspace, fabric, chat_binding));
    assert_eq!(outcome.status, ToolExecutionStatus::Succeeded);
    assert_eq!(outcome.model_content, "executed:skills_manage");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

fn allow_service(fixture: &Fixture, name: &'static str) -> ToolExecutionService {
    fixture.service(name, vec![], ToolExecutionDecision::Allow, approved(), true)
}

/// An admission pin with no unsealed trace is Stale on the MCP consumer.
#[tokio::test]
async fn admitted_binding_without_trace_is_stale() {
    let fixture = fixture();
    let service = allow_service(&fixture, "project_tool");
    let mut context = context();
    context.admitted_binding = Some(issued_binding(&fixture, "project_tool", context.envelope.clone()));
    assert!(context.execution_trace.is_none());
    let outcome = service
        .execute(
            ToolExecutionCommand::new("project_tool", serde_json::json!({"value": 1})),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::StaleCapability);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

/// Sealing the live trace is the cancelled-resume window: dispatch must
/// refuse rather than skip the admitted-descriptor check.
#[tokio::test]
async fn sealed_execution_trace_with_admitted_binding_is_stale() {
    let fixture = fixture();
    let service = allow_service(&fixture, "project_tool");
    let mut context = context();
    context.admitted_binding = Some(issued_binding(&fixture, "project_tool", context.envelope.clone()));
    let trace = ExecutionTrace::default();
    let _ = trace.seal();
    context.execution_trace = Some(trace);
    let outcome = service
        .execute(
            ToolExecutionCommand::new("project_tool", serde_json::json!({"value": 1})),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::StaleCapability);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

#[derive(Clone)]
struct MutableSchemaTool {
    name: &'static str,
    schema_generation: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for MutableSchemaTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "mutable schema fixture"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        if self.schema_generation.load(Ordering::SeqCst) == 0 {
            serde_json::json!({"type":"object","properties":{"value":{},"action":{}},"required":["value"]})
        } else {
            serde_json::json!({"type":"object","properties":{"changed":{"type":"string"}}})
        }
    }
    fn specs(&self) -> Vec<ToolSpec> {
        vec![self.spec()]
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            success: true,
            output: "mutated".into(),
            error: None,
        })
    }
}

/// Fingerprint drift after the MCP pin is issued must Stale at tools::execution
/// (the MCP gateway cell is not this module's pin).
#[tokio::test]
async fn binding_fingerprint_drift_with_live_trace_is_stale() {
    let fixture = fixture();
    let schema_generation = Arc::new(AtomicUsize::new(0));
    let tool = MutableSchemaTool {
        name: "project_tool",
        schema_generation: Arc::clone(&schema_generation),
        calls: Arc::clone(&fixture.calls),
    };
    let catalog = Arc::new(
        CapabilityCatalog::build(
            &[Box::new(tool.clone()) as Box<dyn Tool>],
            &CatalogSources::default(),
            &SurfacePolicy::default(),
        )
        .expect("test: catalog for drift pin"),
    );
    let entry = catalog
        .entry_for_public_name("project_tool")
        .expect("test: catalog entry for drift pin");
    let service = ToolExecutionService::new(
        vec![Arc::new(tool)],
        Arc::new(FixedPolicy {
            decision: ToolExecutionDecision::Allow,
            stages: Arc::clone(&fixture.stages),
        }),
        Arc::new(FixedApproval {
            decision: approved(),
            stages: Arc::clone(&fixture.stages),
        }),
        Arc::new(RecordingPreparation {
            stages: Arc::clone(&fixture.stages),
            allowed: true,
        }),
        Arc::new(RecordingAudit {
            records: Arc::clone(&fixture.records),
            stages: Arc::clone(&fixture.stages),
        }),
    );
    let mut context = context();
    context.admitted_binding = Some(ExecutionBinding::issued(
        crate::causal_tree::ids::DecisionId::new_runtime(),
        entry,
        Arc::clone(&catalog),
        context.envelope.clone(),
        true,
    ));
    context.execution_trace = Some(ExecutionTrace::default());
    schema_generation.store(1, Ordering::SeqCst);
    let outcome = service
        .execute(
            ToolExecutionCommand::new("project_tool", serde_json::json!({"value": 1})),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::StaleCapability);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

/// Catalog snapshot at service construction versus a later live schema change.
#[tokio::test]
async fn experiment_action_with_stale_descriptor_refuses() {
    let (workspace, fabric, chat_binding, origin) =
        crate::runtime::h2_entry::recorded_terminal_approval_fixture().await;
    let workspace_id = origin.record().workspace.clone();
    let run_id = origin.record().run.clone();
    let session = origin.record().session.clone();
    let actor = origin.record().actor.clone();
    let event = origin.record().event.clone();
    let fixture = fixture();
    let schema_generation = Arc::new(AtomicUsize::new(0));
    let tool = MutableSchemaTool {
        name: "skills_manage",
        schema_generation: Arc::clone(&schema_generation),
        calls: Arc::clone(&fixture.calls),
    };
    let mut service = ToolExecutionService::new(
        vec![Arc::new(tool)],
        Arc::new(FixedPolicy {
            decision: ToolExecutionDecision::Allow,
            stages: Arc::clone(&fixture.stages),
        }),
        Arc::new(FixedApproval {
            decision: approved(),
            stages: Arc::clone(&fixture.stages),
        }),
        Arc::new(RecordingPreparation {
            stages: Arc::clone(&fixture.stages),
            allowed: true,
        }),
        Arc::new(RecordingAudit {
            records: Arc::clone(&fixture.records),
            stages: Arc::clone(&fixture.stages),
        }),
    );
    service.policy = Arc::new(SecurityEffectPolicy::new(Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        ..SecurityPolicy::default()
    })));
    service.approval = Arc::new(DenyApprovalStrategy);
    let mut envelope = RuntimeEnvelope::agent(&workspace_id, &run_id)
        .with_channel("terminal")
        .with_sender("local-user")
        .with_owner_id(&actor)
        .with_source_message_event_id(&event);
    envelope.session_key = session;
    let mut context = context();
    context.envelope = envelope;
    context.set_h2_origin(Some(origin));
    schema_generation.store(1, Ordering::SeqCst);
    let outcome = service
        .execute(
            ToolExecutionCommand::new(
                "skills_manage",
                serde_json::json!({"value": 1, "action": "lifecycle_experiment_catalog"}),
            ),
            context,
            None,
        )
        .await;
    drop((workspace, fabric, chat_binding));
    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert!(
        outcome
            .model_content
            .contains("experiment action has no admitted descriptor"),
        "{}",
        outcome.model_content
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn verify_admitted_call_capability_equals_catalog_entry() {
    let fixture = fixture();
    let service = allow_service(&fixture, "project_tool");
    let envelope = context().envelope.with_source_message_event_id("prx_source_event");
    let binding = issued_binding(&fixture, "project_tool", envelope.clone());
    let admission = binding.admission();
    let descriptor = service
        .current_descriptor("project_tool")
        .expect("test: real descriptor");
    let command = ToolExecutionCommand::new("project_tool", serde_json::json!({"value":1}));
    let verified = service
        .verify_admitted_call(&envelope, &admission, &descriptor, &command)
        .expect("test: admitted call");
    let entry = admission
        .catalog
        .entry_for_public_name("project_tool")
        .expect("test: admitted catalog entry");
    assert_eq!(
        verified.capability(),
        &crate::causal_tree::branch::CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation
        },
        "test: exact catalog identity and generation"
    );
    assert_eq!(
        verified.catalog_generation(),
        admission.catalog.generation().0,
        "test: actual catalog generation"
    );
    let mut changed_scope = envelope;
    changed_scope.run_id = Some("prx_foreign_run".into());
    assert!(
        service
            .verify_admitted_call(&changed_scope, &admission, &descriptor, &command)
            .is_err(),
        "test: admission scope cannot be substituted"
    );
}

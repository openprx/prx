mod called_tokens;
mod surface;
use super::*;
mod approval;
mod policy;
use crate::security::approval_ledger::{run_call::ApprovalLedgerPort, strategy::ExecutionApprovalLedger};
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogSources, SurfacePolicy};
use crate::tools::execution::admission::CallTokenBudget;

pub(crate) async fn ledger_context(path: &std::path::Path) -> ToolExecutionContext {
    crate::acl::approval_grant::WitnessKeyring::global_for_tests();
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(path).unwrap());
    let key = crate::decision::DecisionKey::load_or_create(path).unwrap();
    let fixture = fixture();
    let registry: Vec<Box<dyn Tool>> = [
        "file_write",
        "shell",
        "message_send",
        "config_reload",
        "mcp_call",
        "sessions_spawn",
        "project_tool",
    ]
    .into_iter()
    .map(|root_name| {
        Box::new(RecordingTool {
            root_name,
            aliases: vec![],
            calls: Arc::clone(&fixture.calls),
            arguments: Arc::clone(&fixture.arguments),
            stages: Arc::clone(&fixture.stages),
        }) as Box<dyn Tool>
    })
    .collect();
    let catalog =
        Arc::new(CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap());
    let mut context = context();
    context.envelope.run_id = Some(uuid::Uuid::new_v4().to_string());
    let source = crate::memory::MemoryFabric::new(Arc::clone(&memory), &context.envelope.workspace_id)
        .record_inbound_user_message(context.envelope.message_scope(), "project call", None, None)
        .await
        .unwrap();
    context.envelope.source_message_event_id = Some(source.event_id);
    context.approval_ledger = Some(ExecutionApprovalLedger {
        memory,
        state_key: key,
        workspace: context.envelope.workspace_id.clone(),
        catalog,
    });
    context
}
fn service(fixture: &Fixture) -> ToolExecutionService {
    fixture.service("config_reload", vec![], ToolExecutionDecision::Ask, approved(), true)
}
fn command(value: u64) -> ToolExecutionCommand {
    ToolExecutionCommand::new("config_reload", serde_json::json!({"value":value}))
}
async fn review(context: &ToolExecutionContext, approved: bool) {
    let ledger = context.approval_ledger.as_ref().unwrap();
    let port = ApprovalLedgerPort {
        memory: &ledger.memory,
        state_key: &ledger.state_key,
        workspace: &ledger.workspace,
        keyring: crate::acl::approval_grant::WitnessKeyring::global_for_tests(),
    };
    let pending = port
        .list_pending_calls(context.envelope.owner_id.as_deref().unwrap(), 0, 10)
        .await
        .unwrap();
    port.review(
        context.envelope.owner_id.as_deref().unwrap(),
        &context.envelope.session_key,
        pending.items.first().unwrap().authority(),
        approved,
    )
    .await
    .unwrap();
}
#[tokio::test]
async fn approved_resume_executes_once_and_second_claim_is_refused_after_restart() {
    let path = tempfile::tempdir().unwrap();
    let fixture = fixture();
    let service = service(&fixture);
    let mut context = ledger_context(path.path()).await;
    assert_eq!(
        service.execute(command(1), context.clone(), None).await.status,
        ToolExecutionStatus::AwaitingApproval
    );
    review(&context, true).await;
    assert_eq!(
        service.execute(command(1), context.clone(), None).await.status,
        ToolExecutionStatus::Succeeded
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    let mut ledger = context.approval_ledger.take().unwrap();
    drop(ledger.memory);
    ledger.memory = Arc::new(crate::memory::SqliteMemory::new(path.path()).unwrap());
    context.approval_ledger = Some(ledger);
    assert_eq!(
        service.execute(command(1), context, None).await.status,
        ToolExecutionStatus::ApprovalDenied
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn resume_with_changed_arguments_is_refused() {
    let path = tempfile::tempdir().unwrap();
    let fixture = fixture();
    let service = service(&fixture);
    let context = ledger_context(path.path()).await;
    assert_eq!(
        service.execute(command(1), context.clone(), None).await.status,
        ToolExecutionStatus::AwaitingApproval
    );
    review(&context, true).await;
    assert_eq!(
        service.execute(command(2), context, None).await.status,
        ToolExecutionStatus::AwaitingApproval
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn denied_review_resolve_returns_denied_without_backend_call() {
    let path = tempfile::tempdir().unwrap();
    let fixture = fixture();
    let service = service(&fixture);
    let context = ledger_context(path.path()).await;
    assert_eq!(
        service.execute(command(1), context.clone(), None).await.status,
        ToolExecutionStatus::AwaitingApproval
    );
    review(&context, false).await;
    let outcome = service.execute(command(1), context, None).await;
    assert_eq!(outcome.status, ToolExecutionStatus::ApprovalDenied);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn denied_and_expired_approvals_never_execute() {
    use crate::security::approval_ledger::keys::ApprovalAuthority;
    for expired in [false, true] {
        let path = tempfile::tempdir().unwrap();
        let fixture = fixture();
        let service = service(&fixture);
        let context = ledger_context(path.path()).await;
        assert_eq!(
            service.execute(command(1), context.clone(), None).await.status,
            ToolExecutionStatus::AwaitingApproval
        );
        if expired {
            let ledger = context.approval_ledger.as_ref().unwrap();
            let port = ApprovalLedgerPort {
                memory: &ledger.memory,
                state_key: &ledger.state_key,
                workspace: &ledger.workspace,
                keyring: crate::acl::approval_grant::WitnessKeyring::global_for_tests(),
            };
            let page = port
                .list_pending_calls(context.envelope.owner_id.as_deref().unwrap(), 0, 10)
                .await
                .unwrap();
            let pending = page.items.first().unwrap();
            let event = ledger
                .memory
                .find_message_event_by_idempotency_key(&ledger.workspace, &pending.authority().pending_key())
                .await
                .unwrap()
                .unwrap();
            let mut payload: serde_json::Value =
                serde_json::from_str(event.raw_payload_json.as_deref().unwrap()).unwrap();
            let record = payload.get_mut("record").unwrap();
            *record.get_mut("expires_at").unwrap() =
                serde_json::to_value(chrono::Utc::now() - chrono::Duration::seconds(1)).unwrap();
            let typed: crate::security::approval_ledger::run_call::PendingCall =
                serde_json::from_value(record.clone()).unwrap();
            let mac = ledger.state_key.hmac(
                crate::decision::KeyDomain::ApprovalState,
                &[b"call.pending", &serde_json::to_vec(&typed).unwrap()],
            );
            *payload.get_mut("mac").unwrap() = serde_json::to_value(mac).unwrap();
            crate::memory::SqliteMemory::new(path.path())
                .unwrap()
                .replace_event_payload_for_test(&event.event_id, &payload.to_string())
                .unwrap();
        } else {
            review(&context, false).await;
        }
        assert_eq!(
            service.execute(command(1), context, None).await.status,
            ToolExecutionStatus::ApprovalDenied
        );
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    }
}

struct ChannelResolver {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl crate::agent::turn_contracts::ApprovalResolver for ChannelResolver {
    async fn resolve(
        &self,
        _: &crate::approval::ApprovalRequest,
        _: &str,
    ) -> crate::agent::turn_contracts::ApprovalDecision {
        self.entered.notify_one();
        self.release.notified().await;
        crate::agent::turn_contracts::ApprovalDecision::Deny
    }
}
#[tokio::test]
async fn channel_force_ask_precedes_ledger_and_effective_full_skips_it() {
    let path = tempfile::tempdir().unwrap();
    let fixture = fixture();
    let context = ledger_context(path.path()).await;
    let run = context.envelope.run_id.as_deref().unwrap();
    let resolver = Arc::new(ChannelResolver {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    crate::agent::channel_approval::install(run, resolver.clone(), true, false);
    let service = fixture.service("config_reload", vec![], ToolExecutionDecision::Allow, approved(), true);
    let task_context = context.clone();
    let task = tokio::spawn(async move { service.execute(command(1), task_context, None).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), resolver.entered.notified())
        .await
        .unwrap();
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    let ledger = context.approval_ledger.as_ref().unwrap();
    let port = ApprovalLedgerPort {
        memory: &ledger.memory,
        state_key: &ledger.state_key,
        workspace: &ledger.workspace,
        keyring: crate::acl::approval_grant::WitnessKeyring::global_for_tests(),
    };
    assert!(
        port.list_pending_calls(context.envelope.owner_id.as_deref().unwrap(), 0, 10)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    resolver.release.notify_one();
    assert_eq!(task.await.unwrap().status, ToolExecutionStatus::ApprovalDenied);
    crate::agent::channel_approval::install(run, resolver, true, true);
    let service = fixture.service("config_reload", vec![], ToolExecutionDecision::Allow, approved(), true);
    let outcome = service.execute(command(1), context.clone(), None).await;
    crate::agent::channel_approval::clear(run);
    assert_eq!(outcome.status, ToolExecutionStatus::Succeeded);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert!(
        port.list_pending_calls(context.envelope.owner_id.as_deref().unwrap(), 0, 10)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn supervised_every_non_read_only_class_waits() {
    let path = tempfile::tempdir().unwrap();
    let context = ledger_context(path.path()).await;
    assert!(context.execution_bindings.is_none());
    let calls = [
        (
            "file_write",
            serde_json::json!({"value":1,"path":"project.txt","content":"project content"}),
        ),
        ("shell", serde_json::json!({"value":1,"command":"touch project.txt"})),
        (
            "message_send",
            serde_json::json!({"value":1,"channel":"prx_channel","target":"prx_target","message":"project message"}),
        ),
        ("config_reload", serde_json::json!({"value":1})),
        (
            "mcp_call",
            serde_json::json!({"value":1,"server":"prx_server","tool":"prx_action","arguments":{}}),
        ),
        ("sessions_spawn", serde_json::json!({"value":1,"task":"project task"})),
    ];
    for (name, args) in calls {
        let fixture = fixture();
        let mut service = fixture.service(name, vec![], ToolExecutionDecision::Ask, approved(), true);
        service.policy = Arc::new(SecurityEffectPolicy::new(Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        })));
        let result = service
            .execute(ToolExecutionCommand::new(name, args.clone()), context.clone(), None)
            .await;
        assert_eq!(
            result.status,
            ToolExecutionStatus::AwaitingApproval,
            "{name}: {}",
            result.model_content
        );
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn general_call_claims_supply_gate_compatible_signed_grants() {
    let path = tempfile::tempdir().unwrap();
    let context = ledger_context(path.path()).await;
    for (name, args, operation) in [
        (
            "shell",
            serde_json::json!({"value":1,"command":"touch project.txt"}),
            "touch project.txt",
        ),
        (
            "file_write",
            serde_json::json!({"value":1,"path":"project.txt"}),
            "file_write:write:project",
        ),
        (
            "message_send",
            serde_json::json!({"value":1,"message":"project"}),
            "message_send:send:project",
        ),
        (
            "mcp_call",
            serde_json::json!({"value":1,"server":"prx_server","tool":"prx_action"}),
            "mcp_call:call:prx_server:prx_action",
        ),
    ] {
        let fixture = fixture();
        let service = fixture.service(name, vec![], ToolExecutionDecision::Ask, approved(), true);
        assert_eq!(
            service
                .execute(ToolExecutionCommand::new(name, args.clone()), context.clone(), None)
                .await
                .status,
            ToolExecutionStatus::AwaitingApproval
        );
        review(&context, true).await;
        assert_eq!(
            service
                .execute(ToolExecutionCommand::new(name, args), context.clone(), None)
                .await
                .status,
            ToolExecutionStatus::Succeeded
        );
        let arguments = fixture.arguments.lock();
        let normalized = arguments.first().unwrap();
        let grant = crate::security::policy::ApprovalGrant::from_runtime_args(name, normalized).unwrap();
        let v2 = grant.v2.as_ref().unwrap();
        assert!(v2.decision_binding.is_none());
        assert_eq!(v2.max_uses, 1);
        assert!(!grant.permits_resource_operation(
            "prx_other_tool",
            operation,
            crate::security::policy::ResourceRiskLevel::Medium,
            chrono::Utc::now().timestamp()
        ));
        if name == "shell" {
            assert!(grant.permits_command(
                name,
                operation,
                crate::security::policy::CommandRiskLevel::Medium,
                chrono::Utc::now().timestamp()
            ));
            assert!(!grant.permits_command(
                name,
                "touch another.txt",
                crate::security::policy::CommandRiskLevel::Medium,
                chrono::Utc::now().timestamp()
            ));
        } else {
            assert!(grant.permits_resource_operation(
                name,
                operation,
                if name == "mcp_call" {
                    crate::security::policy::ResourceRiskLevel::High
                } else {
                    crate::security::policy::ResourceRiskLevel::Medium
                },
                chrono::Utc::now().timestamp()
            ));
        }
    }
}

/// Exercise ordinary approval with both answers and observe the real durable inbox.
pub(crate) async fn assert_existing_strategy(context: ToolExecutionContext, observer: ExecutionApprovalLedger) {
    for allow in [true, false] {
        let fixture = fixture();
        let answer = if allow {
            approved()
        } else {
            ToolApprovalDecision::Denied {
                reason: "project refusal".into(),
            }
        };
        let service = fixture.service("config_reload", vec![], ToolExecutionDecision::Ask, answer, true);
        let outcome = service.execute(command(1), context.clone(), None).await;
        assert_eq!(
            outcome.status,
            if allow {
                ToolExecutionStatus::Succeeded
            } else {
                ToolExecutionStatus::ApprovalDenied
            }
        );
        assert_eq!(fixture.calls.load(Ordering::SeqCst), usize::from(allow));
        assert_eq!(
            fixture
                .stages
                .lock()
                .iter()
                .filter(|stage| **stage == "approval")
                .count(),
            1
        );
    }
    let port = ApprovalLedgerPort {
        memory: &observer.memory,
        state_key: &observer.state_key,
        workspace: &observer.workspace,
        keyring: crate::acl::approval_grant::WitnessKeyring::global_for_tests(),
    };
    assert!(
        port.list_pending_calls(context.envelope.owner_id.as_deref().unwrap(), 0, 10)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn unmodelled_operation_uses_existing_strategy_without_pending() {
    let path = tempfile::tempdir().unwrap();
    let context = ledger_context(path.path()).await;
    assert!(
        crate::security::approval_operation::grant_op_for_call("project_tool", &serde_json::json!({"value":1}))
            .is_none()
    );
    for allow in [true, false] {
        let fixture = fixture();
        let answer = if allow {
            approved()
        } else {
            ToolApprovalDecision::Denied {
                reason: "project refusal".into(),
            }
        };
        let service = fixture.service("project_tool", vec![], ToolExecutionDecision::Ask, answer, true);
        let outcome = service
            .execute(
                ToolExecutionCommand::new("project_tool", serde_json::json!({"value":1})),
                context.clone(),
                None,
            )
            .await;
        assert_eq!(
            outcome.status,
            if allow {
                ToolExecutionStatus::Succeeded
            } else {
                ToolExecutionStatus::ApprovalDenied
            }
        );
        assert_eq!(fixture.calls.load(Ordering::SeqCst), usize::from(allow));
        assert!(fixture.stages.lock().contains(&"approval"));
    }
    let observer = context.approval_ledger.clone().unwrap();
    let mut ordinary = context;
    ordinary.approval_ledger = None;
    assert_existing_strategy(ordinary, observer).await;
}

#[tokio::test]
async fn pinned_mcp_alias_uses_catalog_operation_and_rejects_changed_live_target() {
    use crate::config::schema::{McpConfig, McpServerConfig};
    use crate::security::policy::{ApprovalGrant, ResourceRiskLevel, SideEffectGate};
    let path = tempfile::tempdir().unwrap();
    let mut context = ledger_context(path.path()).await;
    let fixture = fixture();
    let alias = "mcp__prx_server__nested__action";
    let tool = RecordingTool {
        root_name: "mcp_call",
        aliases: vec![alias],
        calls: Arc::clone(&fixture.calls),
        arguments: Arc::clone(&fixture.arguments),
        stages: Arc::clone(&fixture.stages),
    };
    let mut sources = CatalogSources::default();
    sources.mcp.push(crate::tools::mcp::McpCapabilitySpec {
        server: "prx_server__nested".into(),
        tool: "action".into(),
        state_digest: [7; 32],
        spec: ToolSpec {
            name: alias.into(),
            description: "project action".into(),
            parameters: tool.parameters_schema(),
        },
    });
    context.approval_ledger.as_mut().unwrap().catalog = Arc::new(
        CapabilityCatalog::build(&[Box::new(tool) as Box<dyn Tool>], &sources, &SurfacePolicy::default()).unwrap(),
    );
    let service = fixture.service("mcp_call", vec![alias], ToolExecutionDecision::Ask, approved(), true);
    let command = || ToolExecutionCommand::new(alias, serde_json::json!({"value":1}));
    assert_eq!(
        service.execute(command(), context.clone(), None).await.status,
        ToolExecutionStatus::AwaitingApproval
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    review(&context, true).await;
    assert_eq!(
        service.execute(command(), context, None).await.status,
        ToolExecutionStatus::Succeeded
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    let args = fixture.arguments.lock().first().unwrap().clone();
    let grant = ApprovalGrant::from_runtime_args(alias, &args).unwrap();
    let operation = "mcp_call:call:prx_server__nested:action";
    assert!(grant.permits_resource_operation(
        alias,
        operation,
        ResourceRiskLevel::High,
        chrono::Utc::now().timestamp()
    ));
    // The same public alias now resolves through a shorter configured server
    // name to a different root operation. The real MCP gate must reject it
    // before transport, without spending the valid original grant.
    let mut live = McpConfig::default();
    live.servers.insert(
        "prx_server".into(),
        McpServerConfig {
            tool_name_prefix: "mcp".into(),
            ..McpServerConfig::default()
        },
    );
    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    });
    let mcp = crate::tools::mcp::McpTool::new(Arc::clone(&security), live, path.path().to_owned());
    let refused = mcp.execute_named(alias, args).await.unwrap();
    assert!(!refused.success);
    assert!(
        refused
            .error
            .as_deref()
            .unwrap()
            .contains("requires runtime approval grant")
    );
    assert!(
        SideEffectGate::new(&security)
            .authorize_resource_operation(alias, operation, ResourceRiskLevel::High, Some(&grant))
            .is_ok()
    );
}

mod child_fixture {
    use super::*;
    use crate::causal_tree::branch::CapabilityRef;
    use crate::decision::DecisionKey;
    use crate::tools::capability::child::{
        AuthorityEnvelope, CapabilityConstraints, ChildCapabilityManifest, ChildDispatchPin, VerifiedChildScope,
    };

    /// A tool whose published description follows a shared counter, so a test
    /// can drift one registered capability after the catalog was pinned.
    struct DriftTool {
        state: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Tool for DriftTool {
        fn name(&self) -> &str {
            "file_write"
        }
        fn description(&self) -> &str {
            "drift tool"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{"value":{}},"required":["value"]})
        }
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "file_write".into(),
                description: format!("drift {}", self.state.load(Ordering::SeqCst)),
                parameters: self.parameters_schema(),
            }]
        }
        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult {
                success: true,
                output: "executed".into(),
                error: None,
            })
        }
    }

    pub(super) struct ChildCase {
        pub(super) fixture: Fixture,
        pub(super) service: ToolExecutionService,
        pub(super) drift: Arc<AtomicUsize>,
        pub(super) key: Arc<DecisionKey>,
        pub(super) owner: String,
        pub(super) catalog: Arc<CapabilityCatalog>,
        pub(super) scope: Arc<VerifiedChildScope>,
    }

    impl ChildCase {
        /// `members` names the registered tools the sealed manifest allows.
        pub(super) fn new(members: &[&str]) -> Self {
            Self::owned_by(members, "owner-a")
        }

        pub(super) fn owned_by(members: &[&str], owner: &str) -> Self {
            Self::build(members, owner, AutonomyLevel::Full, 2)
        }

        /// `depth` is the parent's remaining depth; the child holds one less.
        pub(super) fn build(members: &[&str], owner: &str, level: AutonomyLevel, depth: u8) -> Self {
            let fixture = fixture();
            let drift = Arc::new(AtomicUsize::new(0));
            let tools = |state: &Arc<AtomicUsize>| -> Vec<Box<dyn Tool>> {
                vec![
                    Box::new(RecordingTool {
                        root_name: "config_reload",
                        aliases: vec![],
                        calls: Arc::clone(&fixture.calls),
                        arguments: Arc::clone(&fixture.arguments),
                        stages: Arc::clone(&fixture.stages),
                    }),
                    Box::new(DriftTool {
                        state: Arc::clone(state),
                        calls: Arc::clone(&fixture.calls),
                    }),
                    // Held by the parent only, so every manifest is a strict subset.
                    Box::new(RecordingTool {
                        root_name: "http_request",
                        aliases: vec![],
                        calls: Arc::clone(&fixture.calls),
                        arguments: Arc::clone(&fixture.arguments),
                        stages: Arc::clone(&fixture.stages),
                    }),
                    Box::new(RecordingTool {
                        root_name: "shell",
                        aliases: vec![],
                        calls: Arc::clone(&fixture.calls),
                        arguments: Arc::clone(&fixture.arguments),
                        stages: Arc::clone(&fixture.stages),
                    }),
                    Box::new(RecordingTool {
                        root_name: "nodes",
                        aliases: vec![],
                        calls: Arc::clone(&fixture.calls),
                        arguments: Arc::clone(&fixture.arguments),
                        stages: Arc::clone(&fixture.stages),
                    }),
                    Box::new(RecordingTool {
                        root_name: "message_send",
                        aliases: vec![],
                        calls: Arc::clone(&fixture.calls),
                        arguments: Arc::clone(&fixture.arguments),
                        stages: Arc::clone(&fixture.stages),
                    }),
                    Box::new(RecordingTool {
                        root_name: "delegate",
                        aliases: vec![],
                        calls: Arc::clone(&fixture.calls),
                        arguments: Arc::clone(&fixture.arguments),
                        stages: Arc::clone(&fixture.stages),
                    }),
                    Box::new(RecordingTool {
                        root_name: "sessions_spawn",
                        aliases: vec![],
                        calls: Arc::clone(&fixture.calls),
                        arguments: Arc::clone(&fixture.arguments),
                        stages: Arc::clone(&fixture.stages),
                    }),
                ]
            };
            let catalog = Arc::new(
                CapabilityCatalog::build(&tools(&drift), &CatalogSources::default(), &SurfacePolicy::default())
                    .unwrap(),
            );
            let service = ToolExecutionService::new(
                tools(&drift).into_iter().map(Arc::from).collect(),
                Arc::new(FixedPolicy {
                    decision: ToolExecutionDecision::Allow,
                    stages: Arc::clone(&fixture.stages),
                }),
                Arc::new(FixedApproval {
                    decision: ToolApprovalDecision::Denied {
                        reason: "no approver".into(),
                    },
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
            let reference = |name: &str| {
                let entry = catalog.entry_for_public_name(name).unwrap();
                CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                }
            };
            let key = DecisionKey::ephemeral();
            let parent = CapabilityConstraints {
                parent_authority_id: crate::causal_tree::ids::DecisionId::legacy(&[7; 32]).into(),
                allowed: [
                    "config_reload",
                    "file_write",
                    "message_send",
                    "http_request",
                    "shell",
                    "nodes",
                    "delegate",
                    "sessions_spawn",
                ]
                .into_iter()
                .map(reference)
                .collect(),
                authority: AuthorityEnvelope {
                    owner: owner.into(),
                    workspace: "workspace-a".into(),
                    level,
                    max_cost_cents: 100,
                    remaining_depth: depth,
                    delegation: None,
                },
                catalog_generation: *catalog.generation(),
                key: Arc::clone(&key),
            };
            let requested: Vec<_> = members.iter().map(|name| reference(name)).collect();
            let manifest = ChildCapabilityManifest::derive_strict_subset(&parent, &requested).unwrap();
            let scope = Arc::new(VerifiedChildScope::new(manifest, &parent).unwrap());
            Self {
                fixture,
                service,
                drift,
                key,
                owner: owner.into(),
                catalog,
                scope,
            }
        }

        pub(super) fn context(&self, scope: bool, pin: bool) -> ToolExecutionContext {
            let mut context = context();
            context.envelope.workspace_id = "workspace-a".into();
            context.child_scope = scope.then(|| Arc::clone(&self.scope));
            context.child_pin = pin.then(|| {
                Arc::new(ChildDispatchPin {
                    catalog: Arc::clone(&self.catalog),
                    key: Arc::clone(&self.key),
                    owner: self.owner.clone(),
                    workspace: "workspace-a".into(),
                })
            });
            context
        }

        /// Dispatches without bindings, a surface or a decision context.
        pub(super) async fn call(&self, name: &str, context: ToolExecutionContext) -> ToolExecutionStatus {
            self.service
                .execute(
                    ToolExecutionCommand::new(name, serde_json::json!({"value":1})),
                    context,
                    None,
                )
                .await
                .status
        }
    }
}

#[tokio::test]
async fn child_call_outside_manifest_is_refused_without_decision_context() {
    let case = child_fixture::ChildCase::new(&["config_reload"]);
    assert_eq!(
        case.call("config_reload", case.context(true, true)).await,
        ToolExecutionStatus::Succeeded
    );
    assert_eq!(case.fixture.calls.load(Ordering::SeqCst), 1);
    // Registered, catalog-valid and parity-valid, but not a manifest member.
    assert_eq!(
        case.call("file_write", case.context(true, true)).await,
        ToolExecutionStatus::StaleCapability
    );
    assert_eq!(case.fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn child_call_with_parity_drift_is_refused_without_decision_context() {
    let case = child_fixture::ChildCase::new(&["config_reload", "file_write"]);
    assert_eq!(
        case.call("config_reload", case.context(true, true)).await,
        ToolExecutionStatus::Succeeded
    );
    assert_eq!(case.fixture.calls.load(Ordering::SeqCst), 1);
    // A different allowed capability drifts: the member itself is unchanged.
    case.drift.store(1, Ordering::SeqCst);
    assert_eq!(
        case.call("config_reload", case.context(true, true)).await,
        ToolExecutionStatus::StaleCapability
    );
    assert_eq!(case.fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn child_provenance_without_scope_is_stale() {
    let case = child_fixture::ChildCase::new(&["config_reload"]);
    // Provenance without scope, and scope without provenance, both refuse.
    for (scope, pin) in [(false, true), (true, false)] {
        assert_eq!(
            case.call("config_reload", case.context(scope, pin)).await,
            ToolExecutionStatus::StaleCapability
        );
    }
    // A foreign owner or a foreign runtime key never validates the scope.
    let mut foreign_owner = case.context(true, true);
    foreign_owner.envelope.owner_id = Some("owner-b".into());
    assert_eq!(
        case.call("config_reload", foreign_owner).await,
        ToolExecutionStatus::StaleCapability
    );
    let mut foreign_key = case.context(true, true);
    foreign_key.child_pin = Some(Arc::new(crate::tools::capability::child::ChildDispatchPin {
        catalog: Arc::clone(&case.catalog),
        key: crate::decision::DecisionKey::ephemeral(),
        owner: "owner-a".into(),
        workspace: "workspace-a".into(),
    }));
    assert_eq!(
        case.call("config_reload", foreign_key).await,
        ToolExecutionStatus::StaleCapability
    );
    // A foreign workspace and a catalog of another generation also refuse.
    let mut foreign_workspace = case.context(true, true);
    foreign_workspace.envelope.workspace_id = "workspace-b".into();
    assert_eq!(
        case.call("config_reload", foreign_workspace).await,
        ToolExecutionStatus::StaleCapability
    );
    let other_catalog = Arc::new(
        CapabilityCatalog::build(
            &[Box::new(RecordingTool {
                root_name: "config_reload",
                aliases: vec![],
                calls: Arc::clone(&case.fixture.calls),
                arguments: Arc::clone(&case.fixture.arguments),
                stages: Arc::clone(&case.fixture.stages),
            }) as Box<dyn Tool>],
            &CatalogSources::default(),
            &SurfacePolicy::default(),
        )
        .unwrap(),
    );
    assert_ne!(other_catalog.generation(), case.catalog.generation());
    let mut stale_generation = case.context(true, true);
    stale_generation.child_pin = Some(Arc::new(crate::tools::capability::child::ChildDispatchPin {
        catalog: other_catalog,
        key: Arc::clone(&case.key),
        owner: "owner-a".into(),
        workspace: "workspace-a".into(),
    }));
    assert_eq!(
        case.call("config_reload", stale_generation).await,
        ToolExecutionStatus::StaleCapability
    );
    assert_eq!(case.fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        case.call("config_reload", case.context(true, true)).await,
        ToolExecutionStatus::Succeeded
    );
    assert_eq!(case.fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn child_owner_is_never_derived_from_channel_or_sender() {
    let mut cleared = context();
    cleared.envelope.owner_id = None;
    let derived = cleared.envelope.resolved_owner_id();
    assert_ne!(derived, "owner-a");
    let case = child_fixture::ChildCase::owned_by(&["config_reload"], &derived);
    let mut context = case.context(true, true);
    context.envelope.owner_id = None;
    assert_eq!(
        case.call("config_reload", context).await,
        ToolExecutionStatus::StaleCapability
    );
    assert_eq!(case.fixture.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn readonly_child_denies_every_non_read_only_class() {
    // LocalWrite, ProcessExec, ExternalSend, CredentialOrConfig and Destructive.
    let names = ["file_write", "shell", "message_send", "config_reload", "nodes"];
    let case = child_fixture::ChildCase::build(&names, "owner-a", AutonomyLevel::ReadOnly, 2);
    for name in names {
        assert!(!crate::security::policy::is_read_only_tool(name), "{name}");
        assert_eq!(
            case.call(name, case.context(true, true)).await,
            ToolExecutionStatus::Denied,
            "{name}"
        );
    }
    assert_eq!(case.fixture.calls.load(Ordering::SeqCst), 0);
    assert!(!case.fixture.stages.lock().contains(&"approval"));
}

#[tokio::test]
async fn supervised_child_under_full_parent_asks() {
    let case = child_fixture::ChildCase::build(
        &["config_reload", "file_write"],
        "owner-a",
        AutonomyLevel::Supervised,
        2,
    );
    // The service policy allows every call, as a Full parent does.
    assert_eq!(
        case.call("config_reload", case.context(true, true)).await,
        ToolExecutionStatus::ApprovalDenied
    );
    assert_eq!(case.fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        case.fixture
            .stages
            .lock()
            .iter()
            .filter(|stage| **stage == "approval")
            .count(),
        1
    );
}

#[tokio::test]
async fn spawn_without_parent_authority_is_refused() {
    let case = child_fixture::ChildCase::build(
        &["config_reload", "delegate", "sessions_spawn"],
        "owner-a",
        AutonomyLevel::Full,
        2,
    );
    // Live depth but no outgoing grant or ordinary publication.
    for name in ["delegate", "sessions_spawn"] {
        assert_eq!(
            case.call(name, case.context(true, true)).await,
            ToolExecutionStatus::Denied,
            "{name}"
        );
    }
    assert_eq!(case.fixture.calls.load(Ordering::SeqCst), 0);
    // The runtime-owned local terminal operator context (the constructor the chat
    // dispatcher uses) carries no child scope, pin or grant and keeps its existing
    // admission: each launcher reaches the backend exactly once.
    let policy = crate::security::SecurityPolicy::default();
    let operator = crate::chat::dispatcher::chat_tool_execution_context(&policy, None, None, "operator-control");
    assert!(operator.is_local_operator());
    assert!(operator.child_scope.is_none() && operator.child_pin.is_none() && operator.child_grant.is_none());
    assert!(operator.execution_bindings.is_none() && operator.turn_surface.is_none());
    for (index, name) in ["delegate", "sessions_spawn"].into_iter().enumerate() {
        assert_eq!(
            case.call(name, operator.clone()).await,
            ToolExecutionStatus::Succeeded,
            "{name}"
        );
        assert_eq!(case.fixture.calls.load(Ordering::SeqCst), index + 1);
    }
}

/// Launcher cells that need a real issued outgoing grant. The caller supplies one
/// from a fixture that builds a decision-enabled parent; the grant is only checked
/// for presence at dispatch. No decision context, binding or surface is involved.
pub(crate) async fn launcher_authority_cells(grant: Arc<crate::runtime::child_source::IssuedChildGrant>) {
    let members = ["config_reload", "delegate", "sessions_spawn"];
    let launchers = ["delegate", "sessions_spawn"];
    let with_grant = |case: &child_fixture::ChildCase| {
        let mut context = case.context(true, true);
        context.child_grant = Some(Arc::clone(&grant));
        context
    };
    // Depth zero with valid outgoing authority: only depth can refuse.
    let exhausted = child_fixture::ChildCase::build(&members, "owner-a", AutonomyLevel::Full, 1);
    for name in launchers {
        assert_eq!(
            exhausted.call(name, with_grant(&exhausted)).await,
            ToolExecutionStatus::Denied,
            "{name} at depth zero"
        );
    }
    assert_eq!(exhausted.fixture.calls.load(Ordering::SeqCst), 0);
    // Live depth with valid authority: exactly one backend entry per launcher.
    let live = child_fixture::ChildCase::build(&members, "owner-a", AutonomyLevel::Full, 2);
    for (index, name) in launchers.into_iter().enumerate() {
        assert_eq!(
            live.call(name, with_grant(&live)).await,
            ToolExecutionStatus::Succeeded,
            "{name} with live depth"
        );
        assert_eq!(live.fixture.calls.load(Ordering::SeqCst), index + 1);
    }
    // The same admitted launchers under a ReadOnly child: policy Deny, no approval, no backend.
    let readonly = child_fixture::ChildCase::build(&members, "owner-a", AutonomyLevel::ReadOnly, 2);
    for name in launchers {
        assert_eq!(
            readonly.call(name, with_grant(&readonly)).await,
            ToolExecutionStatus::Denied,
            "{name} under a ReadOnly child"
        );
    }
    assert_eq!(readonly.fixture.calls.load(Ordering::SeqCst), 0);
    assert!(!readonly.fixture.stages.lock().contains(&"approval"));
}

/// The money ceiling is re-read at admission and again after the approval wait,
/// immediately before invocation. A funded control runs once. The changed case
/// reads funded at admission (the send-time view) and empty at the final
/// recheck, so the call is refused with 0 further backend calls.
#[tokio::test]
async fn child_budget_changed_after_send_is_refused_at_dispatch() {
    let case = child_fixture::ChildCase::new(&["config_reload"]);
    let command = || ToolExecutionCommand::new("config_reload", serde_json::json!({"value":1}));
    let funded = CallTokenBudget::new(|| 1000, |_| Some(500)).with_spend(|| Some(10_000), |_| Some(1000));
    let mut control = case.context(true, true);
    control.call_budget = Some(funded);
    let outcome = case.service.execute(command(), control, None).await;
    assert_eq!(outcome.status, ToolExecutionStatus::Succeeded);
    assert_eq!(case.fixture.calls.load(Ordering::SeqCst), 1);

    let reads = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&reads);
    let changed = CallTokenBudget::new(|| 1000, |_| Some(500)).with_spend(
        move || {
            Some(if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                10_000
            } else {
                0
            })
        },
        |_| Some(1000),
    );
    let mut context = case.context(true, true);
    context.call_budget = Some(changed);
    let outcome = case.service.execute(command(), context, None).await;
    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("tool 'config_reload' estimates 1000 micro-USD but 0 remain")),
        "unexpected diagnostic: {:?}",
        outcome.error
    );
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_eq!(case.fixture.calls.load(Ordering::SeqCst), 1);
}

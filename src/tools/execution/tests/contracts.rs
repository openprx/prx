#![cfg(test)]

use super::*;

#[tokio::test]
async fn approval_denial_is_typed_and_fail_closed() {
    let fixture = fixture();
    let service = fixture.service(
        "native_write",
        Vec::new(),
        ToolExecutionDecision::Ask,
        ToolApprovalDecision::Denied {
            reason: "operator denied".to_string(),
        },
        true,
    );
    let outcome = service
        .execute(
            ToolExecutionCommand::new("native_write", serde_json::json!({"value":"blocked"})),
            context(),
            None,
        )
        .await;

    assert_eq!(outcome.status, ToolExecutionStatus::ApprovalDenied);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(&*fixture.stages.lock(), &["policy", "approval", "audit"]);
}

#[tokio::test]
async fn preparation_denial_prevents_adapter_execution() {
    let fixture = fixture();
    let service = fixture.service(
        "native_read",
        Vec::new(),
        ToolExecutionDecision::Allow,
        approved(),
        false,
    );
    let outcome = service
        .execute(
            ToolExecutionCommand::new("native_read", serde_json::json!({"value":"blocked"})),
            context(),
            None,
        )
        .await;

    assert_eq!(outcome.status, ToolExecutionStatus::PreparationDenied);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(&*fixture.stages.lock(), &["policy", "preparation", "audit"]);
}

/// A tool whose contract declares integers, so the coercion has something
/// to act on and the recorded arguments show what the executor really got.
struct IntegerContractTool {
    calls: Arc<AtomicUsize>,
    arguments: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[async_trait]
impl Tool for IntegerContractTool {
    fn name(&self) -> &str {
        "paged_read"
    }

    fn description(&self) -> &str {
        "reads a page"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "from": {"type": "integer"},
                "window": {"type": "object", "properties": {"lines": {"type": "integer"}}},
                "marks": {"type": "array", "items": {"type": "integer"}}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.arguments.lock().push(args);
        Ok(ToolResult {
            success: true,
            output: "page".to_string(),
            error: None,
        })
    }
}

fn integer_contract_service(fixture: &Fixture) -> ToolExecutionService {
    ToolExecutionService::from_shared_boxed_registry(
        Arc::new(vec![Box::new(IntegerContractTool {
            calls: Arc::clone(&fixture.calls),
            arguments: Arc::clone(&fixture.arguments),
        }) as Box<dyn Tool>]),
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
    )
}

#[tokio::test]
async fn whole_numbers_written_as_floats_reach_the_executor_as_integers() {
    // The validator accepts `200.0` for an `integer` because JSON Schema
    // says so. Executors read integers with `as_u64()`, which answers
    // `None` for `Float(200.0)` and leaves them on their default — a wrong
    // answer the model never learns about. This is the one place that knows
    // which fields the contract declared `integer`.
    let fixture = fixture();
    let service = integer_contract_service(&fixture);
    let outcome = service
        .execute(
            ToolExecutionCommand::new(
                "paged_read",
                serde_json::json!({
                    "path": "notes.md",
                    "from": 200.0,
                    "window": {"lines": 12.0},
                    "marks": [3.0, 4.0]
                }),
            ),
            context(),
            None,
        )
        .await;

    assert_eq!(outcome.status, ToolExecutionStatus::Succeeded);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    let recorded = fixture.arguments.lock().first().cloned().expect("executor arguments");
    let integer_at = |pointer: &str| recorded.pointer(pointer).and_then(serde_json::Value::as_u64);
    assert_eq!(
        integer_at("/from"),
        Some(200),
        "the executor must read 200, not fall back to a default: {recorded}"
    );
    assert_eq!(integer_at("/window/lines"), Some(12), "{recorded}");
    assert_eq!(integer_at("/marks/0"), Some(3), "{recorded}");
    assert_eq!(integer_at("/marks/1"), Some(4), "{recorded}");
    assert_eq!(
        recorded.pointer("/path").and_then(serde_json::Value::as_str),
        Some("notes.md")
    );
}

#[tokio::test]
async fn a_fractional_value_for_an_integer_field_is_still_rejected_before_the_executor() {
    let fixture = fixture();
    let service = integer_contract_service(&fixture);
    let outcome = service
        .execute(
            ToolExecutionCommand::new("paged_read", serde_json::json!({"path": "notes.md", "from": 1.5})),
            context(),
            None,
        )
        .await;

    assert_eq!(outcome.status, ToolExecutionStatus::InvalidArguments);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert!(
        outcome.error.unwrap_or_default().contains("integer"),
        "the model has to be told which field it got wrong"
    );
}

#[tokio::test]
async fn mcp_alias_resolves_one_adapter_and_preserves_public_descriptor() {
    let fixture = fixture();
    let tool = RecordingTool {
        root_name: "mcp_call",
        aliases: vec!["mcp__docs__search"],
        calls: Arc::clone(&fixture.calls),
        arguments: Arc::clone(&fixture.arguments),
        stages: Arc::clone(&fixture.stages),
    };
    let service = ToolExecutionService::from_shared_boxed_registry(
        Arc::new(vec![Box::new(tool) as Box<dyn Tool>]),
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
    let outcome = service
        .execute(
            ToolExecutionCommand::new("mcp__docs__search", serde_json::json!({"value":"rust"})),
            context(),
            None,
        )
        .await;

    let descriptor = outcome.descriptor.unwrap();
    assert_eq!(outcome.status, ToolExecutionStatus::Succeeded);
    assert_eq!(descriptor.public_name, "mcp__docs__search");
    assert_eq!(descriptor.backend_name, "mcp_call");
    assert_eq!(descriptor.adapter, ToolAdapterKind::McpAlias);
    assert_eq!(descriptor.effect, ToolEffect::Act);
    assert_eq!(outcome.model_content, "executed:mcp__docs__search");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn runtime_discovered_alias_resolves_after_execution_catalog_snapshot() {
    let fixture = fixture();
    let enabled = Arc::new(AtomicBool::new(false));
    let service = ToolExecutionService::new(
        vec![Arc::new(LateAliasTool {
            enabled: Arc::clone(&enabled),
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
            stages: Arc::clone(&fixture.stages),
            allowed: true,
        }),
        Arc::new(RecordingAudit {
            records: Arc::clone(&fixture.records),
            stages: Arc::clone(&fixture.stages),
        }),
    );

    assert!(service.catalog().descriptor("mcp__late__navigate").is_none());
    enabled.store(true, AtomicOrdering::Release);

    let outcome = service
        .execute(
            ToolExecutionCommand::new("mcp__late__navigate", serde_json::json!({"url":"https://example.com"})),
            context(),
            None,
        )
        .await;

    let descriptor = outcome.descriptor.expect("dynamic alias descriptor");
    assert_eq!(outcome.status, ToolExecutionStatus::Succeeded);
    assert_eq!(descriptor.public_name, "mcp__late__navigate");
    assert_eq!(descriptor.backend_name, "mcp_call");
    assert_eq!(descriptor.adapter, ToolAdapterKind::McpAlias);
    assert_eq!(descriptor.effect, ToolEffect::Act);
    assert_eq!(outcome.model_content, "executed:mcp__late__navigate");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

/// A tool published with the full canonical contract surface: an action
/// enum, action-specific required fields, a typed field, a closed property
/// set, and a JSON Schema keyword this validator does not model.
struct ContractTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for ContractTool {
    fn name(&self) -> &str {
        "contract_tool"
    }

    fn description(&self) -> &str {
        "contract fixture"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        crate::tools::schema::with_default_action_requirements(
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["publish", "list"],
                        "description": "What to do."
                    },
                    "topic": {"type": "string", "description": "Topic to publish to."},
                    "limit": {"type": "integer", "minimum": 1, "description": "Page size."},
                    // `pattern` is deliberately unmodelled by the PRX
                    // validator: an unknown keyword must not reject a call.
                    "tag": {"type": "string", "pattern": "^[0-9]+$", "description": "Free-form tag."}
                }
            }),
            "action",
            "publish",
            &[crate::tools::schema::ActionRequirement {
                action: "publish",
                required: &["topic"],
            }],
        )
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.execute_named(self.name(), args).await
    }

    async fn execute_named(&self, _name: &str, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            success: true,
            output: "executed".to_string(),
            error: None,
        })
    }
}

fn contract_service(calls: &Arc<AtomicUsize>) -> ToolExecutionService {
    ToolExecutionService::new(
        vec![Arc::new(ContractTool {
            calls: Arc::clone(calls),
        })],
        Arc::new(FixedPolicy {
            decision: ToolExecutionDecision::Allow,
            stages: Arc::new(Mutex::new(Vec::new())),
        }),
        Arc::new(FixedApproval {
            decision: approved(),
            stages: Arc::new(Mutex::new(Vec::new())),
        }),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    )
}

async fn contract_outcome(arguments: serde_json::Value) -> (ToolExecutionOutcome, usize) {
    let calls = Arc::new(AtomicUsize::new(0));
    let service = contract_service(&calls);
    let outcome = service
        .execute(ToolExecutionCommand::new("contract_tool", arguments), context(), None)
        .await;
    let observed = calls.load(Ordering::SeqCst);
    (outcome, observed)
}

async fn contract_outcome_for_dialect(
    arguments: serde_json::Value,
    dialect: ToolSchemaDialect,
) -> (ToolExecutionOutcome, usize) {
    let calls = Arc::new(AtomicUsize::new(0));
    let service = contract_service(&calls);
    let outcome = service
        .execute(
            ToolExecutionCommand::new("contract_tool", arguments),
            context().with_schema_dialect(dialect),
            None,
        )
        .await;
    let observed = calls.load(Ordering::SeqCst);
    (outcome, observed)
}

/// Gemini's API rejects `additionalProperties` and `minimum`, so
/// `SchemaCleanr::clean_for_gemini` deletes them before the request goes
/// out. A model that was never shown those constraints cannot be held to
/// them: the same two calls must pass on the Gemini path and fail on the
/// OpenAI path, which is shown the published document verbatim.
#[tokio::test]
async fn gemini_calls_are_validated_against_the_document_gemini_was_shown() {
    // The published schema closes the property set; Gemini never saw that.
    let undeclared = serde_json::json!({"action": "list", "nope": 1});
    let (raw, raw_calls) = contract_outcome_for_dialect(undeclared.clone(), ToolSchemaDialect::Raw).await;
    assert_eq!(
        raw.status,
        ToolExecutionStatus::InvalidArguments,
        "the published contract closes the property set: {}",
        raw.model_content
    );
    assert_eq!(raw_calls, 0);

    let (gemini, gemini_calls) = contract_outcome_for_dialect(undeclared, ToolSchemaDialect::Gemini).await;
    assert_eq!(
        gemini.status,
        ToolExecutionStatus::Succeeded,
        "clean_for_gemini strips additionalProperties, so it must not be enforced: {}",
        gemini.model_content
    );
    assert_eq!(gemini_calls, 1);

    // Same story for a numeric bound.
    let below_minimum = serde_json::json!({"action": "list", "limit": 0});
    let (raw, _) = contract_outcome_for_dialect(below_minimum.clone(), ToolSchemaDialect::Raw).await;
    assert_eq!(raw.status, ToolExecutionStatus::InvalidArguments);
    assert!(raw.model_content.contains("$.limit"), "{}", raw.model_content);

    let (gemini, gemini_calls) = contract_outcome_for_dialect(below_minimum, ToolSchemaDialect::Gemini).await;
    assert_eq!(
        gemini.status,
        ToolExecutionStatus::Succeeded,
        "clean_for_gemini strips minimum: {}",
        gemini.model_content
    );
    assert_eq!(gemini_calls, 1);
}

/// The relaxation is exactly the cleaner's keyword list and nothing wider:
/// `enum`, `type`, `required` and the action-specific `if`/`then` clauses
/// survive `clean_for_gemini`, so they stay enforced on the Gemini path.
#[tokio::test]
async fn the_gemini_dialect_still_enforces_everything_gemini_was_shown() {
    for (arguments, expected_path) in [
        (serde_json::json!({"action": "delete_everything"}), "$.action"),
        (serde_json::json!({"action": "publish"}), "$.topic"),
        (serde_json::json!({"action": "list", "limit": "ten"}), "$.limit"),
    ] {
        let (outcome, calls) = contract_outcome_for_dialect(arguments.clone(), ToolSchemaDialect::Gemini).await;
        assert_eq!(
            outcome.status,
            ToolExecutionStatus::InvalidArguments,
            "{arguments} survives clean_for_gemini and must stay enforced, got {}",
            outcome.model_content
        );
        assert!(
            outcome.model_content.contains(expected_path),
            "expected {expected_path} in {}",
            outcome.model_content
        );
        assert_eq!(calls, 0);
    }
}

#[tokio::test]
async fn production_path_rejects_out_of_range_enum_before_the_backend_runs() {
    let (outcome, calls) = contract_outcome(serde_json::json!({"action": "delete_everything"})).await;
    assert_eq!(outcome.status, ToolExecutionStatus::InvalidArguments);
    assert_eq!(calls, 0, "a contract violation must not reach the executor");
    assert!(
        outcome.model_content.contains("$.action"),
        "the model needs the offending field name: {}",
        outcome.model_content
    );
    assert!(
        outcome.error.as_deref().is_some_and(|error| error.contains("$.action")),
        "the audit record must carry the same issue"
    );
}

#[tokio::test]
async fn production_path_rejects_missing_action_specific_required_field() {
    let (outcome, calls) = contract_outcome(serde_json::json!({"action": "publish"})).await;
    assert_eq!(outcome.status, ToolExecutionStatus::InvalidArguments);
    assert_eq!(calls, 0);
    assert!(
        outcome.model_content.contains("$.topic") && outcome.model_content.contains("is required"),
        "action-specific requirement not reported: {}",
        outcome.model_content
    );
}

#[tokio::test]
async fn production_path_rejects_wrong_argument_types_and_unknown_properties() {
    let (outcome, calls) = contract_outcome(serde_json::json!({"action": "list", "limit": "ten"})).await;
    assert_eq!(outcome.status, ToolExecutionStatus::InvalidArguments);
    assert_eq!(calls, 0);
    assert!(
        outcome.model_content.contains("$.limit"),
        "type error not reported: {}",
        outcome.model_content
    );

    let (outcome, calls) = contract_outcome(serde_json::json!({"action": "list", "nope": 1})).await;
    assert_eq!(outcome.status, ToolExecutionStatus::InvalidArguments);
    assert_eq!(calls, 0);
    assert!(
        outcome.model_content.contains("$.nope"),
        "closed property set not enforced: {}",
        outcome.model_content
    );
}

#[tokio::test]
async fn production_path_accepts_contract_respecting_calls() {
    let (outcome, calls) = contract_outcome(serde_json::json!({"action": "publish", "topic": "ops", "limit": 5})).await;
    assert_eq!(outcome.status, ToolExecutionStatus::Succeeded, "{:?}", outcome.error);
    assert_eq!(calls, 1);
}

#[tokio::test]
async fn production_path_passes_unmodelled_schema_keywords_through() {
    // `pattern` is not part of the validated subset. Fail-open on an
    // unknown keyword keeps dynamic MCP and WASM contracts callable.
    let (outcome, calls) = contract_outcome(serde_json::json!({"action": "list", "tag": "not-digits"})).await;
    assert_eq!(outcome.status, ToolExecutionStatus::Succeeded, "{:?}", outcome.error);
    assert_eq!(calls, 1);
}

#[tokio::test]
async fn production_path_applies_the_default_action_contract_when_action_is_omitted() {
    // The executor defaults to `publish`, so an omitted action stays legal
    // and is held to the `publish` contract rather than escaping it.
    let (outcome, calls) = contract_outcome(serde_json::json!({"topic": "ops"})).await;
    assert_eq!(outcome.status, ToolExecutionStatus::Succeeded, "{:?}", outcome.error);
    assert_eq!(calls, 1);

    let (outcome, calls) = contract_outcome(serde_json::json!({"limit": 3})).await;
    assert_eq!(outcome.status, ToolExecutionStatus::InvalidArguments);
    assert_eq!(calls, 0);
    assert!(
        outcome.model_content.contains("$.topic"),
        "the default action's requirements must bind: {}",
        outcome.model_content
    );

    let (outcome, calls) = contract_outcome(serde_json::json!({"action": "list", "limit": 0})).await;
    assert_eq!(outcome.status, ToolExecutionStatus::InvalidArguments);
    assert_eq!(calls, 0);
    assert!(outcome.model_content.contains("$.limit"), "{}", outcome.model_content);
}

#[tokio::test]
async fn production_path_validates_dynamic_alias_contracts_with_their_own_schema() {
    let enabled = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    let service = ToolExecutionService::new(
        vec![Arc::new(LateAliasTool {
            enabled: Arc::clone(&enabled),
            calls: Arc::clone(&calls),
        })],
        Arc::new(FixedPolicy {
            decision: ToolExecutionDecision::Allow,
            stages: Arc::new(Mutex::new(Vec::new())),
        }),
        Arc::new(FixedApproval {
            decision: approved(),
            stages: Arc::new(Mutex::new(Vec::new())),
        }),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    );

    let rejected = service
        .execute(
            ToolExecutionCommand::new("mcp__late__navigate", serde_json::json!({"url": 7})),
            context(),
            None,
        )
        .await;
    assert_eq!(rejected.status, ToolExecutionStatus::InvalidArguments);
    assert!(rejected.model_content.contains("$.url"), "{}", rejected.model_content);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let accepted = service
        .execute(
            ToolExecutionCommand::new(
                "mcp__late__navigate",
                serde_json::json!({"url": "https://example.test"}),
            ),
            context(),
            None,
        )
        .await;
    assert_eq!(accepted.status, ToolExecutionStatus::Succeeded, "{:?}", accepted.error);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn invalid_arguments_are_typed_and_adapter_is_not_called() {
    let fixture = fixture();
    let service = fixture.service(
        "native_read",
        Vec::new(),
        ToolExecutionDecision::Allow,
        approved(),
        true,
    );
    let outcome = service
        .execute(
            ToolExecutionCommand::new("native_read", serde_json::json!({})),
            context(),
            None,
        )
        .await;

    assert_eq!(outcome.status, ToolExecutionStatus::InvalidArguments);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    // Validation now precedes approval and preparation, so a structurally
    // invalid call never reaches either.
    assert_eq!(&*fixture.stages.lock(), &["policy", "audit"]);
}

#[tokio::test]
async fn unknown_tool_is_typed_and_still_audited() {
    let fixture = fixture();
    let service = fixture.service(
        "native_read",
        Vec::new(),
        ToolExecutionDecision::Allow,
        approved(),
        true,
    );
    let outcome = service
        .execute(
            ToolExecutionCommand::new("missing", serde_json::json!({"value":"x"})),
            context(),
            None,
        )
        .await;

    assert_eq!(outcome.status, ToolExecutionStatus::UnknownTool);
    assert_eq!(&*fixture.stages.lock(), &["audit"]);
    assert_eq!(
        fixture.records.lock().first().unwrap().status,
        ToolExecutionStatus::UnknownTool
    );
}

#[test]
fn security_policy_adapter_preserves_all_autonomy_modes() {
    let descriptor = |name: &str| ToolDescriptor {
        public_name: name.to_string(),
        backend_name: name.to_string(),
        description: name.to_string(),
        parameters: serde_json::json!({"type":"object"}),
        tier: ToolTier::Core,
        categories: Vec::new(),
        effect: if crate::security::policy::is_read_only_tool(name) {
            ToolEffect::Read
        } else {
            ToolEffect::Act
        },
        adapter: ToolAdapterKind::Native,
        process_worker_compatible: true,
        availability: CapabilityAvailability::ready("test backend is registered"),
    };
    let policy = |autonomy| {
        SecurityEffectPolicy::new(Arc::new(SecurityPolicy {
            autonomy,
            ..SecurityPolicy::default()
        }))
    };

    let assert_projection = |autonomy, name, expected| {
        let policy = SecurityPolicy {
            autonomy,
            ..SecurityPolicy::default()
        };
        assert_eq!(
            decide_tool_execution(&policy, name, "alice", "terminal", "dm"),
            expected
        );
        assert_eq!(
            SecurityEffectPolicy::new(Arc::new(policy)).decide(&descriptor(name), &context()),
            expected
        );
    };

    assert_projection(AutonomyLevel::ReadOnly, "file_read", ToolExecutionDecision::Allow);
    assert_projection(AutonomyLevel::ReadOnly, "native_write", ToolExecutionDecision::Deny);
    assert_projection(AutonomyLevel::Supervised, "native_write", ToolExecutionDecision::Ask);
    assert_projection(AutonomyLevel::Full, "native_write", ToolExecutionDecision::Allow);

    assert_eq!(
        policy(AutonomyLevel::ReadOnly).decide(&descriptor("file_read"), &context()),
        ToolExecutionDecision::Allow
    );
    assert_eq!(
        policy(AutonomyLevel::ReadOnly).decide(&descriptor("native_write"), &context()),
        ToolExecutionDecision::Deny
    );
    assert_eq!(
        policy(AutonomyLevel::Supervised).decide(&descriptor("native_write"), &context()),
        ToolExecutionDecision::Ask
    );
    assert_eq!(
        policy(AutonomyLevel::Full).decide(&descriptor("native_write"), &context()),
        ToolExecutionDecision::Allow
    );
}

#[tokio::test]
async fn default_full_policy_executes_ready_tool_without_prompt_and_keeps_typed_audit() {
    let fixture = fixture();
    let tool = RecordingTool {
        root_name: "native_write",
        aliases: Vec::new(),
        calls: Arc::clone(&fixture.calls),
        arguments: Arc::clone(&fixture.arguments),
        stages: Arc::clone(&fixture.stages),
    };
    let service = ToolExecutionService::new(
        vec![Arc::new(tool)],
        Arc::new(SecurityEffectPolicy::new(Arc::new(SecurityPolicy::default()))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(RecordingPreparation {
            stages: Arc::clone(&fixture.stages),
            allowed: true,
        }),
        Arc::new(RecordingAudit {
            records: Arc::clone(&fixture.records),
            stages: Arc::clone(&fixture.stages),
        }),
    );

    let outcome = service
        .execute(
            ToolExecutionCommand::new("native_write", serde_json::json!({"value":"ok"})),
            context(),
            None,
        )
        .await;

    assert_eq!(outcome.status, ToolExecutionStatus::Succeeded);
    assert_eq!(outcome.decision, Some(ToolExecutionDecision::Allow));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    let records = fixture.records.lock();
    let record = records.first().expect("typed audit record");
    assert_eq!(record.decision, Some(ToolExecutionDecision::Allow));
    assert_eq!(record.status, ToolExecutionStatus::Succeeded);
    assert_eq!(record.effect, Some(ToolEffect::Act));
}

struct HangingTool;

#[async_trait]
impl Tool for HangingTool {
    fn name(&self) -> &str {
        "hanging_tool"
    }

    fn description(&self) -> &str {
        "waits until cancelled"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancellation_is_typed_and_audited_once() {
    let fixture = fixture();
    let service = ToolExecutionService::new(
        vec![Arc::new(HangingTool)],
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
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let outcome = service
        .execute(
            ToolExecutionCommand::new("hanging_tool", serde_json::json!({})),
            context(),
            Some(cancellation),
        )
        .await;

    assert_eq!(outcome.status, ToolExecutionStatus::Cancelled);
    assert_eq!(&*fixture.stages.lock(), &["policy", "preparation", "audit"]);
    assert_eq!(fixture.records.lock().len(), 1);
}

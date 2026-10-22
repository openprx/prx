#![cfg(test)]

use super::*;

#[tokio::test]
async fn cron_missing_prompt_is_rejected_before_policy_approval_and_idempotency() {
    let temp = tempfile::tempdir().unwrap();
    let config = crate::config::Config {
        workspace_dir: temp.path().to_owned(),
        config_path: temp.path().join("config.toml"),
        ..crate::config::Config::default()
    };
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
    let fixture = fixture();
    let tool = Arc::new(crate::tools::CronTool::new(
        crate::config::new_shared(config.clone()),
        Arc::new(SecurityPolicy::default()),
    ));
    let service = ToolExecutionService::new(
        vec![tool],
        Arc::new(FixedPolicy {
            decision: ToolExecutionDecision::Ask,
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
    .with_idempotency_memory(Arc::clone(&memory));
    let ctx = context();
    let key = "invalid-cron-add";
    // Holding this lock proves the invalid call never attempts to acquire a lease.
    let lock = tool_execution_lock(
        &ctx.envelope.workspace_id,
        &ctx.envelope.session_key,
        &ctx.semantic_turn_id,
        key,
    );
    let _held = lock.lock().await;
    let (reserved, terminal) = tool_execution_ledger_keys(&ctx, key);
    for dialect in [ToolSchemaDialect::Raw, ToolSchemaDialect::NoRootCombinators] {
        fixture.stages.lock().clear();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            service.execute(
                ToolExecutionCommand::new(
                    "cron",
                    serde_json::json!({
                        "action":"add", "job_type":"agent", "schedule":{"kind":"cron","expr":"*/5 * * * *"}
                    }),
                )
                .with_idempotency_key(key),
                ctx.clone().with_schema_dialect(dialect),
                None,
            ),
        )
        .await
        .expect("invalid arguments must not wait for an idempotency lease");
        assert_eq!(outcome.status, ToolExecutionStatus::InvalidArguments);
        assert!(outcome.error.as_deref().unwrap().contains("required-field set"));
        assert_eq!(outcome.decision, None);
        assert!(outcome.preparation.is_none());
        assert_eq!(&*fixture.stages.lock(), &["audit"]);
        for key in [&reserved, &terminal] {
            assert!(
                memory
                    .find_message_event_by_idempotency_key(&ctx.envelope.workspace_id, key)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        assert!(crate::cron::list_jobs(&config).unwrap().is_empty());
    }
    // A complete read contract still reaches the existing Ask policy and approval.
    fixture.stages.lock().clear();
    let valid = service
        .execute(
            ToolExecutionCommand::new("cron", serde_json::json!({"action":"list"})),
            ctx,
            None,
        )
        .await;
    assert_eq!(valid.status, ToolExecutionStatus::Succeeded, "{:?}", valid.error);
    assert_eq!(&*fixture.stages.lock(), &["policy", "approval", "preparation", "audit"]);
}

struct LateArgumentError;

#[async_trait]
impl Tool for LateArgumentError {
    fn name(&self) -> &str {
        "late_contract"
    }
    fn description(&self) -> &str {
        "fixture for a backend-discovered argument error"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    async fn execute(&self, mut args: serde_json::Value) -> anyhow::Result<ToolResult> {
        crate::tools::schema::validate_execution_contract(
            self.name(),
            &serde_json::json!({"type":"object","required":["value"]}),
            &mut args,
        )?;
        Ok(ToolResult {
            success: true,
            output: String::new(),
            error: None,
        })
    }
}

#[tokio::test]
async fn backend_tool_argument_error_is_classified_as_invalid_arguments() {
    let fixture = fixture();
    let service = ToolExecutionService::new(
        vec![Arc::new(LateArgumentError)],
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
    let outcome = service
        .execute(
            ToolExecutionCommand::new("late_contract", serde_json::json!({})),
            context(),
            None,
        )
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::InvalidArguments);
    assert!(outcome.error.as_deref().unwrap().contains("$.value"));
}

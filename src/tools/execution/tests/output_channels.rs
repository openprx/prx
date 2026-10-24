#![cfg(test)]

use super::*;

#[tokio::test]
async fn shell_output_channels_survive_success_and_failure_at_the_service_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
    let service = shell_service(&temp, memory);
    for (exit, status) in [(0, ToolExecutionStatus::Succeeded), (7, ToolExecutionStatus::Failed)] {
        let command = ToolExecutionCommand::new(
            "shell",
            serde_json::json!({"command":format!(
                "printf 'PRX_STDOUT_EVIDENCE\\n'; printf 'PRX_STDERR_EVIDENCE\\n' >&2; exit {exit}"
            )}),
        )
        .with_operation_id(format!("channels-{exit}"))
        .with_idempotency_key(format!("channels-{exit}"));
        let outcome = service.execute(command.clone(), context(), None).await;
        assert_eq!(outcome.status, status);
        let result = outcome.result.as_ref().unwrap();
        assert_eq!(result.output, "PRX_STDOUT_EVIDENCE\n");
        assert_eq!(result.error.as_deref(), Some("PRX_STDERR_EVIDENCE\n"));
        assert!(
            outcome.model_content.contains("PRX_STDOUT_EVIDENCE"),
            "stdout missing from model content"
        );
        assert!(
            outcome.model_content.contains("PRX_STDERR_EVIDENCE"),
            "stderr missing from model content"
        );
        let replay = service.execute(command, context(), None).await;
        assert!(replay.replayed);
        assert_eq!(replay.status, status);
        assert_eq!(replay.model_content, outcome.model_content);
        assert_eq!(replay.result.unwrap().output, result.output);
    }
}

fn shell_service(temp: &tempfile::TempDir, memory: Arc<dyn Memory>) -> ToolExecutionService {
    let fixture = fixture();
    ToolExecutionService::new(
        vec![Arc::new(crate::tools::shell::ShellTool::new(
            Arc::new(crate::runtime::NativeRuntime::new()),
            temp.path().to_path_buf(),
        ))],
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
    .with_idempotency_memory(memory)
}

#[tokio::test]
async fn shell_single_channel_and_recovery_hint_contracts_remain_intact() {
    let temp = tempfile::tempdir().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
    let service = shell_service(&temp, memory);
    for (command, success, expected) in [
        ("printf stdout_only", true, "stdout_only"),
        ("true", true, ""),
        ("printf stdout_only; exit 7", false, "Error: stdout_only"),
        ("printf stderr_only >&2; exit 7", false, "Error: stderr_only"),
        (
            "printf warning_only >&2",
            true,
            "Output:\n\n\nDiagnostics:\nwarning_only",
        ),
    ] {
        let outcome = service
            .execute(
                ToolExecutionCommand::new("shell", serde_json::json!({"command":command})),
                context(),
                None,
            )
            .await;
        assert_eq!(outcome.succeeded(), success);
        assert_eq!(outcome.model_content, expected);
    }
    let outcome = service
        .execute(
            ToolExecutionCommand::new(
                "shell",
                serde_json::json!({"command":"printf partial_output; printf 'Permission denied' >&2; exit 7"}),
            ),
            context(),
            None,
        )
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::Failed);
    assert!(outcome.model_content.starts_with("Error: Permission denied"));
    assert!(outcome.model_content.contains("Hint:"));
    assert!(outcome.model_content.contains("partial_output"));
}

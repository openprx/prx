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
        assert_eq!(outcome.process.as_ref().unwrap().exit_code, Some(exit));
        assert_eq!(replay.process.as_ref().unwrap().exit_code, Some(exit));
        assert_eq!(replay.result.unwrap().output, result.output);
    }
}

fn shell_service(temp: &tempfile::TempDir, memory: Arc<dyn Memory>) -> ToolExecutionService {
    let fixture = fixture();
    ToolExecutionService::new(
        vec![Arc::from(crate::tools::ArcDelegatingTool::boxed(Arc::new(
            crate::tools::shell::ShellTool::new(
                Arc::new(crate::runtime::NativeRuntime::new()),
                temp.path().to_path_buf(),
            ),
        )))],
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
    for (command, exit, stdout, stderr) in [
        ("printf stdout_only", 0, "stdout_only", None),
        ("true", 0, "", None),
        ("printf stdout_only; exit 7", 7, "stdout_only", None),
        ("printf stderr_only >&2; exit 7", 7, "", Some("stderr_only")),
        ("printf warning_only >&2", 0, "", Some("warning_only")),
        ("exit 17", 17, "", None),
    ] {
        let outcome = service
            .execute(
                ToolExecutionCommand::new("shell", serde_json::json!({"command":command})),
                context(),
                None,
            )
            .await;
        assert_eq!(outcome.succeeded(), exit == 0);
        let process = outcome
            .process
            .as_ref()
            .expect("shell result must preserve process facts");
        assert!(process.started);
        assert!(process.output_complete);
        assert_eq!(process.exit_code, Some(exit));
        assert_eq!(process.signal, None);
        assert!(
            outcome
                .model_content
                .contains(&format!("Process exited with code {exit}"))
        );
        let raw = outcome.result.unwrap();
        assert_eq!(raw.output, stdout);
        assert_eq!(raw.error.as_deref(), stderr);
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
    assert!(outcome.model_content.contains("Process exited with code 7"));
    assert!(outcome.model_content.contains("Permission denied"));
    assert!(outcome.model_content.contains("Hint:"));
    assert!(outcome.model_content.contains("partial_output"));
}

#[tokio::test]
async fn cancelled_shell_retains_process_facts_and_partial_output_in_replay() {
    let temp = tempfile::tempdir().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
    let service = shell_service(&temp, memory);
    let token = CancellationToken::new();
    let command = ToolExecutionCommand::new(
        "shell",
        serde_json::json!({
            "command": "printf before_cancel; printf diagnostic_before_cancel >&2; touch ready; sleep 30"
        }),
    )
    .with_idempotency_key("cancel-partial")
    .with_operation_id("cancel-partial");
    let execute = service.execute(command.clone(), context(), Some(token.clone()));
    let cancel = async {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !temp.path().join("ready").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("shell must start before cancellation");
        token.cancel();
    };
    let (outcome, ()) = tokio::join!(execute, cancel);
    assert_eq!(outcome.status, ToolExecutionStatus::Cancelled);
    let process = outcome.process.as_ref().unwrap();
    assert!(process.started);
    assert!(process.output_complete);
    assert_eq!(process.termination, crate::tools::traits::ProcessTermination::Cancelled);
    assert_eq!(process.exit_code, None);
    let result = outcome.result.as_ref().unwrap();
    assert_eq!(result.output, "before_cancel");
    assert_eq!(result.error.as_deref(), Some("diagnostic_before_cancel"));
    assert!(outcome.model_content.contains("before_cancel"));
    assert!(outcome.model_content.contains("diagnostic_before_cancel"));
    let replay = service.execute(command, context(), None).await;
    assert!(replay.replayed);
    assert_eq!(replay.status, ToolExecutionStatus::Cancelled);
    assert_eq!(replay.model_content, outcome.model_content);
    assert!(replay.process.unwrap().started);
}

#[cfg(unix)]
#[tokio::test]
async fn shell_signal_is_not_reported_as_a_numeric_exit_code() {
    let temp = tempfile::tempdir().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
    let outcome = shell_service(&temp, memory)
        .execute(
            ToolExecutionCommand::new(
                "shell",
                serde_json::json!({"command":"printf before_signal; kill -TERM $$"}),
            ),
            context(),
            None,
        )
        .await;
    let process = outcome.process.unwrap();
    assert_eq!(outcome.status, ToolExecutionStatus::Failed);
    assert_eq!(process.exit_code, None);
    assert_eq!(process.signal, Some(libc::SIGTERM));
    assert!(outcome.model_content.contains("Process terminated by signal"));
    assert!(outcome.model_content.contains("before_signal"));
}

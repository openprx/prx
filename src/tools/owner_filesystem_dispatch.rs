//! Runtime-only private file routing. No serialized argument carries this scope.
use super::owner_filesystem::OwnerFilesystem;
#[cfg(all(test, unix))]
use super::traits::Tool;
use super::traits::ToolResult;
use crate::security::{SecurityPolicy, SideEffectGate};
use anyhow::{Context, ensure};
use std::future::Future;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

tokio::task_local! {
    static CURRENT_FILESYSTEM: Option<Arc<OwnerFilesystem>>;
}

pub(super) fn current() -> Option<Arc<OwnerFilesystem>> {
    CURRENT_FILESYSTEM.try_with(Clone::clone).ok().flatten()
}

pub(super) async fn scope<T>(filesystem: Option<Arc<OwnerFilesystem>>, future: impl Future<Output = T>) -> T {
    CURRENT_FILESYSTEM.scope(filesystem, future).await
}

/// Closed native operations below the service's authenticated owner scope.
/// These variants are not serialized capability or approval authority.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NativeFileOperation {
    Read,
    Write,
    Edit,
}

impl NativeFileOperation {
    const fn tool_name(self) -> &'static str {
        match self {
            Self::Read => "file_read",
            Self::Write => "file_write",
            Self::Edit => "file_edit",
        }
    }
}

/// Once a private effect starts, keep its blocking work owned until completion.
/// An ordinary unscoped invocation retains the Tool default cancellation policy.
pub(super) async fn supervise_file_operation<F>(
    make_operation: impl FnOnce() -> F,
    cancellation: Option<CancellationToken>,
) -> anyhow::Result<ToolResult>
where
    F: Future<Output = anyhow::Result<ToolResult>>,
{
    let cancelled = || ToolResult {
        success: false,
        output: String::new(),
        error: Some(super::traits::TOOL_EXECUTION_CANCELLED.to_owned()),
    };
    let Some(cancellation) = cancellation else {
        return make_operation().await;
    };
    if current().is_some() {
        if cancellation.is_cancelled() {
            return Ok(cancelled());
        }
        return make_operation().await;
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Ok(cancelled()),
        result = make_operation() => result,
    }
}

pub(super) async fn apply_file_operation(
    filesystem: Arc<OwnerFilesystem>,
    security: &SecurityPolicy,
    operation: NativeFileOperation,
    arguments: serde_json::Value,
) -> anyhow::Result<ToolResult> {
    let result = apply_file_operation_inner(filesystem, security, operation, arguments).await;
    Ok(match result {
        Ok(output) => ToolResult {
            success: true,
            output,
            error: None,
        },
        Err(error) => ToolResult {
            success: false,
            output: String::new(),
            error: Some(error.to_string()),
        },
    })
}

async fn apply_file_operation_inner(
    filesystem: Arc<OwnerFilesystem>,
    security: &SecurityPolicy,
    operation: NativeFileOperation,
    arguments: serde_json::Value,
) -> anyhow::Result<String> {
    let stage = crate::runtime::experiment_stage::current();
    let scratch = stage
        .as_ref()
        .map(|stage| stage.native_scratch_root(filesystem.namespace_root()))
        .transpose()?;
    let path = arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .context("missing file path")?;
    ensure!(security.is_path_allowed(path), "file path is denied by security policy");
    let tool = operation.tool_name();
    let target = if operation == NativeFileOperation::Write {
        filesystem.resolve(path)?
    } else {
        filesystem.resolve_existing(path)?
    };
    if operation != NativeFileOperation::Read {
        ensure!(security.can_act(), "action blocked: autonomy is read-only");
        let (operation, risk, _, _) = crate::security::approval_operation::file_operation(
            tool,
            target.namespace_root(),
            target.operation_target(),
        );
        let grant = crate::security::policy::ApprovalGrant::from_runtime_args(tool, &arguments);
        SideEffectGate::new(security)
            .authorize_resource_operation(tool, &operation, risk, grant.as_ref())
            .map_err(anyhow::Error::msg)?;
    }
    // Register before offloading; the ticket stays in the worker even if its
    // awaiting future is dropped. Terminal cleanup cannot report an early drain.
    let owner = crate::runtime::turn_cleanup::current_tracker().context("private file cleanup owner missing")?;
    let cancellation = CancellationToken::new();
    let ticket = owner.register(cancellation.clone())?;
    let write_owner = Arc::clone(&owner);
    let path = path.to_owned();
    crate::runtime::blocking::spawn_blocking(move || {
        let result = (|| -> anyhow::Result<String> {
            ensure!(
                !cancellation.is_cancelled(),
                "private file operation cancelled before start"
            );
            if let Some(stage) = &stage {
                stage.ensure_active()?;
            }
            match operation {
                NativeFileOperation::Read => {
                    const MAX_BYTES: u64 = 10 * 1024 * 1024;
                    let requested = arguments.get("max_bytes").and_then(serde_json::Value::as_u64);
                    let limit = requested.unwrap_or(MAX_BYTES).min(MAX_BYTES);
                    let mut bytes = filesystem.read(&target, limit + 1)?;
                    let truncated = u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit;
                    ensure!(requested.is_some() || !truncated, "file exceeds read size limit");
                    if truncated {
                        bytes.truncate(usize::try_from(limit).context("file limit exceeds platform capacity")?);
                        // Only trim an incomplete final UTF-8 code point. Interior
                        // invalid UTF-8 is an error, never silently discarded.
                        if let Err(error) = std::str::from_utf8(&bytes) {
                            ensure!(error.error_len().is_none(), "file is not UTF-8");
                            bytes.truncate(error.valid_up_to());
                        }
                    }
                    let mut output = String::from_utf8(bytes).context("file is not UTF-8")?;
                    if truncated {
                        output.push_str(&format!("\n[content truncated: {limit} byte limit]"));
                    }
                    Ok(output)
                }
                NativeFileOperation::Write => {
                    let content = arguments
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .context("missing file content")?;
                    let admission = match (&stage, &scratch) {
                        (Some(stage), Some(root)) => Some(stage.prepare_native_write_owned(
                            root,
                            target.operation_target(),
                            content.len(),
                            &write_owner,
                        )?),
                        (None, None) => None,
                        _ => anyhow::bail!("native write lost its original scratch owner"),
                    };
                    let result = (|| -> anyhow::Result<()> {
                        if let Some(admission) = &admission {
                            admission.check()?;
                        }
                        filesystem.write_authorized(&target, content.as_bytes())
                    })();
                    if let Some(admission) = admission {
                        admission.finish();
                    }
                    result?;
                    Ok(format!(
                        "Written {} bytes to {path} in your private file space",
                        content.len()
                    ))
                }
                NativeFileOperation::Edit => {
                    let old = arguments
                        .get("old_string")
                        .and_then(serde_json::Value::as_str)
                        .context("missing old_string")?;
                    let new = arguments
                        .get("new_string")
                        .and_then(serde_json::Value::as_str)
                        .context("missing new_string")?;
                    let all = arguments
                        .get("replace_all")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let count = match (&stage, &scratch) {
                        (Some(stage), Some(root)) => {
                            filesystem.edit_authorized_with_admission(&target, old, new, all, |bytes| {
                                stage
                                    .prepare_native_write_owned(root, target.operation_target(), bytes, &write_owner)
                                    .map(Some)
                            })?
                        }
                        (None, None) => filesystem.edit_authorized(&target, old, new, all)?,
                        _ => anyhow::bail!("native edit lost its original scratch owner"),
                    };
                    Ok(format!(
                        "Applied {count} replacements to {path} in your private file space"
                    ))
                }
            }
        })();
        // This records worker completion, including a settled ordinary error.
        // A panic instead drops an unfinished ticket and fails cleanup closed.
        ticket.finish(true);
        result
    })
    .await
    .context("private file worker did not complete")?
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    mod h2_controlled;
    use super::*;
    use crate::runtime::turn_cleanup::CleanupScope;
    use crate::security::AutonomyLevel;
    use crate::tools::{file_edit::FileEditTool, file_read::FileReadTool, file_write::FileWriteTool};
    use serde_json::json;
    use std::path::PathBuf;

    struct Fixture {
        _root: tempfile::TempDir,
        config: PathBuf,
        workspace: PathBuf,
    }

    impl Fixture {
        fn new() -> std::io::Result<Self> {
            let root = tempfile::tempdir()?;
            let config = root.path().join("config");
            let workspace = root.path().join("workspace");
            std::fs::create_dir(&config)?;
            std::fs::create_dir(&workspace)?;
            Ok(Self {
                _root: root,
                config,
                workspace,
            })
        }

        fn owner(&self, owner: &str) -> anyhow::Result<Arc<OwnerFilesystem>> {
            Ok(Arc::new(OwnerFilesystem::open(
                &self.config,
                "workspace_a",
                owner,
                &self.workspace,
            )?))
        }

        fn security(&self, autonomy: AutonomyLevel) -> Arc<SecurityPolicy> {
            Arc::new(SecurityPolicy {
                workspace_dir: self.workspace.clone(),
                autonomy,
                ..SecurityPolicy::default()
            })
        }
    }

    fn file_service(fixture: &Fixture) -> crate::tools::ToolExecutionService {
        let security = fixture.security(AutonomyLevel::Full);
        crate::tools::ToolExecutionService::with_security_policy(
            vec![
                Box::new(FileWriteTool::new(Arc::clone(&security))),
                Box::new(FileEditTool::new(Arc::clone(&security))),
                Box::new(FileReadTool::new(Arc::clone(&security), false)),
            ],
            security,
        )
    }

    fn private_context(filesystem: Arc<OwnerFilesystem>, owner: &str) -> crate::tools::ToolExecutionContext {
        let mut context = crate::tools::ToolExecutionContext::new(
            crate::runtime::envelope::RuntimeEnvelope::console("workspace_a", "prx_file_service")
                .with_owner_id(owner)
                .with_owner_isolation(),
            "private",
        );
        context.owner_filesystem = Some(filesystem);
        context
    }

    async fn service_result(
        service: &crate::tools::ToolExecutionService,
        context: crate::tools::ToolExecutionContext,
        name: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<ToolResult> {
        let outcome = service
            .execute(crate::tools::ToolExecutionCommand::new(name, args), context, None)
            .await;
        outcome.result.context("file service did not return a backend result")
    }

    async fn owned_call(
        filesystem: Arc<OwnerFilesystem>,
        tool: &dyn Tool,
        args: serde_json::Value,
    ) -> anyhow::Result<ToolResult> {
        CleanupScope::new(None, CancellationToken::new())?
            .run(scope(Some(filesystem), tool.execute(args)))
            .await
    }

    #[tokio::test]
    async fn native_file_tools_keep_same_logical_name_separate_across_owners() {
        let fixture = Fixture::new().unwrap();
        let owner_a = fixture.owner("owner_a").unwrap();
        let owner_b = fixture.owner("owner_b").unwrap();
        let service = file_service(&fixture);
        for (owner, owner_id, content) in [
            (Arc::clone(&owner_a), "owner_a", "alpha"),
            (Arc::clone(&owner_b), "owner_b", "beta"),
        ] {
            let result = CleanupScope::new(None, CancellationToken::new())
                .unwrap()
                .run(service_result(
                    &service,
                    private_context(owner, owner_id),
                    "file_write",
                    json!({"path":"notes.txt","content":content}),
                ))
                .await
                .unwrap();
            assert!(result.success, "{:?}", result.error);
        }
        let result = CleanupScope::new(None, CancellationToken::new())
            .unwrap()
            .run(service_result(
                &service,
                private_context(Arc::clone(&owner_a), "owner_a"),
                "file_edit",
                json!({"path":"notes.txt","old_string":"alpha","new_string":"updated"}),
            ))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        for (owner, owner_id, expected) in [(owner_a, "owner_a", "updated"), (owner_b, "owner_b", "beta")] {
            let result = CleanupScope::new(None, CancellationToken::new())
                .unwrap()
                .run(service_result(
                    &service,
                    private_context(owner, owner_id),
                    "file_read",
                    json!({"path":"notes.txt"}),
                ))
                .await
                .unwrap();
            assert!(result.success, "{:?}", result.error);
            assert_eq!(result.output, expected);
        }
        assert!(!fixture.workspace.join("notes.txt").exists());
    }

    #[tokio::test]
    async fn cancelled_private_write_creates_no_namespace() {
        let fixture = Fixture::new().unwrap();
        let filesystem = fixture.owner("owner_a").unwrap();
        let write = FileWriteTool::new(fixture.security(AutonomyLevel::Full));
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let result = CleanupScope::new(None, CancellationToken::new())
            .unwrap()
            .run(scope(
                Some(filesystem),
                write.execute_with_cancellation(json!({"path":"file.txt","content":"private"}), Some(cancelled)),
            ))
            .await
            .unwrap();
        assert!(!result.success);
        assert_eq!(
            result.error.as_deref(),
            Some(crate::tools::traits::TOOL_EXECUTION_CANCELLED)
        );
        assert!(!fixture.config.join("owner-files").exists());
        assert!(!fixture.workspace.join("file.txt").exists());
    }

    #[tokio::test]
    async fn private_scope_does_not_override_read_only_policy() {
        let fixture = Fixture::new().unwrap();
        let filesystem = fixture.owner("owner_a").unwrap();
        let write = FileWriteTool::new(fixture.security(AutonomyLevel::ReadOnly));
        let result = owned_call(filesystem, &write, json!({"path":"file.txt","content":"private"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().is_some_and(|error| error.contains("read-only")));
        assert!(!fixture.config.join("owner-files").exists());
    }

    #[tokio::test]
    async fn serialized_owner_fields_cannot_select_private_scope() {
        let fixture = Fixture::new().unwrap();
        let owner = fixture.owner("owner_a").unwrap();
        let service = file_service(&fixture);
        let context = crate::tools::ToolExecutionContext::new(
            crate::runtime::envelope::RuntimeEnvelope::console("workspace_a", "prx_file_service"),
            "private",
        );
        let result = CleanupScope::new(None, CancellationToken::new()).unwrap()
            .run(service_result(&service, context, "file_write", json!({"path":"file.txt","content":"shared","owner_filesystem":{"owner_id":"owner_a","root":owner.namespace_root()},"workspace_id":"workspace_a","owner_isolated":true})))
            .await.unwrap();
        assert!(result.success, "{:?}", result.error);
        assert_eq!(std::fs::read(fixture.workspace.join("file.txt")).unwrap(), b"shared");
        assert!(!fixture.config.join("owner-files").exists());
    }

    #[test]
    fn dropped_private_write_retains_cleanup_until_real_queued_worker_finishes() {
        use std::future::Future;
        use std::task::{Context, Waker};
        use std::time::Duration;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let fixture = Fixture::new().unwrap();
            let filesystem = fixture.owner("owner_a").unwrap();
            let service = file_service(&fixture);
            // Saturate this test's dedicated blocking pool; the real write must
            // queue behind a worker whose release is controlled by a channel.
            // Dropping the sender on panic also releases the blocker.
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
            });
            started_rx.await.unwrap();
            let (ack_tx, mut ack_rx) = tokio::sync::oneshot::channel();
            let cleanup = CleanupScope::new(None, CancellationToken::new())
                .unwrap()
                .with_acknowledgement(Some(ack_tx));
            let mut running = Box::pin(cleanup.run(service_result(
                &service,
                private_context(filesystem, "owner_a"),
                "file_write",
                json!({"path":"file.txt","content":"private"}),
            )));
            assert!(
                running
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            drop(running);
            tokio::task::yield_now().await;
            assert!(matches!(
                ack_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            assert!(!fixture.config.join("owner-files").exists());
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), ack_rx)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(!fixture.config.join("owner-files").exists());
            assert!(!fixture.workspace.join("file.txt").exists());
        });
    }
}

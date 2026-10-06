//! Runtime-only private file routing. No serialized argument carries this scope.
use super::owner_filesystem::OwnerFilesystem;
use super::traits::{Tool, ToolResult};
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

/// Once a private effect starts, keep its blocking work owned until completion.
/// An ordinary unscoped invocation retains the Tool default cancellation policy.
pub(super) async fn execute_with_cancellation(
    tool: &dyn Tool,
    args: serde_json::Value,
    cancellation: Option<CancellationToken>,
) -> anyhow::Result<ToolResult> {
    let cancelled = || ToolResult {
        success: false,
        output: String::new(),
        error: Some(super::traits::TOOL_EXECUTION_CANCELLED.to_owned()),
    };
    let Some(cancellation) = cancellation else {
        return tool.execute(args).await;
    };
    if current().is_some() {
        if cancellation.is_cancelled() {
            return Ok(cancelled());
        }
        return tool.execute(args).await;
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Ok(cancelled()),
        result = tool.execute(args) => result,
    }
}

pub(super) async fn execute(
    filesystem: Arc<OwnerFilesystem>,
    security: &SecurityPolicy,
    tool: &str,
    arguments: serde_json::Value,
) -> anyhow::Result<ToolResult> {
    let result = execute_inner(filesystem, security, tool, arguments).await;
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

async fn execute_inner(
    filesystem: Arc<OwnerFilesystem>,
    security: &SecurityPolicy,
    tool: &str,
    arguments: serde_json::Value,
) -> anyhow::Result<String> {
    let path = arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .context("missing file path")?;
    ensure!(security.is_path_allowed(path), "file path is denied by security policy");
    ensure!(
        matches!(tool, "file_read" | "file_write" | "file_edit"),
        "unsupported private file tool"
    );
    let target = if tool == "file_write" {
        filesystem.resolve(path)?
    } else {
        filesystem.resolve_existing(path)?
    };
    if tool != "file_read" {
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
    let tool = tool.to_owned();
    let path = path.to_owned();
    crate::runtime::blocking::spawn_blocking(move || {
        let result = (|| -> anyhow::Result<String> {
            ensure!(
                !cancellation.is_cancelled(),
                "private file operation cancelled before start"
            );
            match tool.as_str() {
                "file_read" => {
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
                "file_write" => {
                    let content = arguments
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .context("missing file content")?;
                    filesystem.write_authorized(&target, content.as_bytes())?;
                    Ok(format!(
                        "Written {} bytes to {path} in your private file space",
                        content.len()
                    ))
                }
                "file_edit" => {
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
                    let count = filesystem.edit_authorized(&target, old, new, all)?;
                    Ok(format!(
                        "Applied {count} replacements to {path} in your private file space"
                    ))
                }
                _ => anyhow::bail!("unsupported private file tool"),
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

#[cfg(all(test, unix))]
mod tests {
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
        let security = fixture.security(AutonomyLevel::Full);
        let write = FileWriteTool::new(Arc::clone(&security));
        let edit = FileEditTool::new(Arc::clone(&security));
        let read = FileReadTool::new(security, false);
        for (owner, content) in [(Arc::clone(&owner_a), "alpha"), (Arc::clone(&owner_b), "beta")] {
            let result = owned_call(owner, &write, json!({"path":"notes.txt","content":content}))
                .await
                .unwrap();
            assert!(result.success, "{:?}", result.error);
        }
        let result = owned_call(
            Arc::clone(&owner_a),
            &edit,
            json!({"path":"notes.txt","old_string":"alpha","new_string":"updated"}),
        )
        .await
        .unwrap();
        assert!(result.success, "{:?}", result.error);
        for (owner, expected) in [(owner_a, "updated"), (owner_b, "beta")] {
            let result = owned_call(owner, &read, json!({"path":"notes.txt"})).await.unwrap();
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
                execute_with_cancellation(&write, json!({"path":"file.txt","content":"private"}), Some(cancelled)),
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
        let write = FileWriteTool::new(fixture.security(AutonomyLevel::Full));
        let result = CleanupScope::new(None, CancellationToken::new()).unwrap()
            .run(scope(None, write.execute(json!({"path":"file.txt","content":"shared","owner_filesystem":{"owner_id":"owner_a","root":owner.namespace_root()},"workspace_id":"workspace_a","owner_isolated":true}))))
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
            let write = FileWriteTool::new(fixture.security(AutonomyLevel::Full));
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
            let mut running = Box::pin(cleanup.run(scope(
                Some(filesystem),
                write.execute(json!({"path":"file.txt","content":"private"})),
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

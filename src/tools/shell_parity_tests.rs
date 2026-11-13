#![cfg(test)]
//! Real native process/filesystem regressions for Full-mode parity.
use super::execution::*;
use super::traits::{Tool, ToolResult};
use super::{
    effective_autonomy, file_edit::FileEditTool, file_read::FileReadTool, file_write::FileWriteTool, shell::ShellTool,
};
use crate::runtime::NativeRuntime;
use crate::security::{AutonomyLevel, SecurityPolicy};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn shell(root: &Path) -> ShellTool {
    ShellTool::new(Arc::new(NativeRuntime::new()), root.to_path_buf())
}

async fn direct(tool: &dyn Tool, level: AutonomyLevel, args: Value) -> ToolResult {
    tokio::time::timeout(
        Duration::from_secs(15),
        effective_autonomy::scope(level, tool.execute(args)),
    )
    .await
    .expect("test: tool deadline")
    .expect("test: tool result")
}

async fn run_exact_environment_probe(mut command: tokio::process::Command) {
    use tokio::io::AsyncReadExt;
    const MAX_PROBE_OUTPUT: u64 = 64 * 1024;
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("test: isolated environment probe");
    let stdout = child.stdout.take().expect("test: probe stdout");
    let stderr = child.stderr.take().expect("test: probe stderr");
    let (status, out, err) = tokio::time::timeout(Duration::from_secs(25), async {
        tokio::join!(
            child.wait(),
            async {
                let mut bytes = Vec::new();
                stdout
                    .take(MAX_PROBE_OUTPUT + 1)
                    .read_to_end(&mut bytes)
                    .await
                    .expect("test: bounded probe stdout");
                assert!(bytes.len() as u64 <= MAX_PROBE_OUTPUT, "test: probe stdout cap");
                bytes
            },
            async {
                let mut bytes = Vec::new();
                stderr
                    .take(MAX_PROBE_OUTPUT + 1)
                    .read_to_end(&mut bytes)
                    .await
                    .expect("test: bounded probe stderr");
                assert!(bytes.len() as u64 <= MAX_PROBE_OUTPUT, "test: probe stderr cap");
                bytes
            }
        )
    })
    .await
    .expect("test: probe deadline");
    let out = String::from_utf8_lossy(&out);
    let err = String::from_utf8_lossy(&err);
    assert!(
        status.expect("test: probe status").success(),
        "test: environment probe failed: {out} {err}"
    );
    assert!(
        out.lines().any(|line| line == "running 1 test"),
        "test: exact probe must execute one test: {out}"
    );
    assert!(
        out.lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")),
        "test: exact probe must pass one test: {out}"
    );
}

#[tokio::test]
async fn full_shell_runs_git_and_repository_hook() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().expect("test: repository");
    let tool = shell(root.path());
    let init = direct(
        &tool,
        AutonomyLevel::Full,
        json!({"command":"git -c user.name=t -c user.email=t@t init"}),
    )
    .await;
    assert!(init.success, "test: git init: {init:?}");
    let hook = root.path().join(".git/hooks/post-commit");
    std::fs::write(&hook, "#!/bin/sh\nprintf hook > hook-marker\n").expect("test: hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).expect("test: executable hook");
    let result = direct(
        &tool,
        AutonomyLevel::Full,
        json!({"command":"git -c user.name=t -c user.email=t@t commit --allow-empty -m x"}),
    )
    .await;
    assert!(result.success, "test: ordinary git: {result:?}");
    assert_eq!(
        std::fs::read(root.path().join("hook-marker")).expect("test: hook marker"),
        b"hook",
        "test: hooks must execute"
    );
}

#[tokio::test]
async fn restricted_shell_refuses_git_before_spawn() {
    let root = tempfile::tempdir().expect("test: root");
    for level in [AutonomyLevel::Supervised, AutonomyLevel::ReadOnly] {
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            effective_autonomy::scope(
                level,
                shell(root.path()).execute_named_observed(
                    "shell",
                    json!({"command":"git -c user.name=t -c user.email=t@t commit --allow-empty -m x"}),
                    None,
                ),
            ),
        )
        .await
        .expect("test: deadline")
        .expect("test: result");
        assert!(!result.result.success, "test: restricted git denied");
        assert!(result.process.as_ref().is_some_and(|p| !p.started), "test: zero spawn");
        assert!(
            result
                .process
                .and_then(|p| p.diagnostic)
                .is_some_and(|e| e.contains("git")),
            "test: git refusal diagnostic"
        );
    }
}

#[tokio::test]
async fn native_file_confinement_uses_effective_level_and_rejects_parent_traversal() {
    let root = tempfile::tempdir().expect("test: workspace");
    let outside = tempfile::tempdir().expect("test: outside");
    let security = Arc::new(SecurityPolicy {
        workspace_dir: root.path().into(),
        autonomy: AutonomyLevel::Full,
        ..SecurityPolicy::default()
    });
    let write = FileWriteTool::new(security.clone());
    let edit = FileEditTool::new(security.clone());
    let read = FileReadTool::new(security, false);
    let path = outside.path().join("parity.txt");
    for level in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
        let result = direct(&write, level, json!({"path":path,"content":"alpha"})).await;
        assert_eq!(
            result.success,
            level == AutonomyLevel::Full,
            "test: confinement: {result:?}"
        );
        let result = direct(&write, level, json!({"path":"../escape.txt","content":"blocked"})).await;
        assert!(!result.success, "test: parent traversal denied: {level:?}");
    }
    let edited = direct(
        &edit,
        AutonomyLevel::Full,
        json!({"path":path,"old_string":"alpha","new_string":"beta"}),
    )
    .await;
    assert!(edited.success, "test: outside edit: {edited:?}");
    let read_result = direct(&read, AutonomyLevel::Full, json!({"path":path})).await;
    assert!(read_result.success, "test: outside read: {read_result:?}");
    assert_eq!(read_result.output, "beta", "test: physical file content");
    for tool in [&edit as &dyn Tool, &read as &dyn Tool] {
        let result = direct(
            tool,
            AutonomyLevel::Supervised,
            json!({"path":path,"old_string":"beta","new_string":"blocked"}),
        )
        .await;
        assert!(!result.success, "test: restricted outside access: {result:?}");
    }
}

#[tokio::test]
async fn full_shell_environment_keeps_proxy_and_strips_credentials() {
    const PROBE: &str = "PRX_FULL_ENV_TEST";
    if std::env::var_os(PROBE).is_none() {
        let mut command = tokio::process::Command::new(std::env::current_exe().expect("test: executable"));
        command
            .args([
                "--exact",
                "tools::shell_parity_tests::full_shell_environment_keeps_proxy_and_strips_credentials",
                "--nocapture",
            ])
            .env(PROBE, "1")
            .env("ANTHROPIC_API_KEY", "sentinel")
            .env("SESSION_WORKER_SECRET", "s2")
            .env("MINIMAX_OAUTH_TEST", "synthetic")
            .env("OPENPRX_TEST_SECRET", "synthetic")
            .env("http_proxy", "x")
            .kill_on_drop(true);
        run_exact_environment_probe(command).await;
        return;
    }
    let root = tempfile::tempdir().expect("test: workspace");
    for level in [AutonomyLevel::Full, AutonomyLevel::Supervised] {
        let result = direct(&shell(root.path()), level, json!({"command":"printf '%s|%s|%s|%s|%s' \"$http_proxy\" \"$ANTHROPIC_API_KEY\" \"$SESSION_WORKER_SECRET\" \"$MINIMAX_OAUTH_TEST\" \"$OPENPRX_TEST_SECRET\""})).await;
        assert!(result.success, "test: environment shell: {result:?}");
        assert_eq!(
            result.output,
            if level == AutonomyLevel::Full { "x||||" } else { "||||" },
            "test: inheritance and denylist"
        );
        let invalid = direct(
            &shell(root.path()).with_env_allowlist(vec!["*".into()]),
            level,
            json!({"command":"printf MUST_NOT_RUN"}),
        )
        .await;
        assert!(
            !invalid.success && invalid.output.is_empty(),
            "test: invalid allowlist rejected at every level"
        );
    }
}

struct Approve;
#[async_trait::async_trait]
impl ApprovalStrategy for Approve {
    async fn resolve(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
        let command = request
            .command
            .arguments
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("native-file");
        let grant = crate::security::policy::ApprovalGrant::for_command(
            request.descriptor.public_name,
            command,
            "PRXOperator",
            None,
        );
        ToolApprovalDecision::Approved {
            runtime_approval_granted: true,
            runtime_grant: Some(serde_json::to_value(grant).expect("test: approval grant")),
        }
    }
}

fn service(root: &Path, level: AutonomyLevel) -> ToolExecutionService {
    let policy = Arc::new(SecurityPolicy {
        autonomy: level,
        workspace_dir: root.into(),
        ..SecurityPolicy::default()
    });
    ToolExecutionService::from_boxed(
        vec![
            Box::new(shell(root)),
            Box::new(FileWriteTool::new(policy.clone())),
            Box::new(FileReadTool::new(policy.clone(), false)),
        ],
        Arc::new(SecurityEffectPolicy::new(policy)),
        Arc::new(Approve),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    )
}

fn context() -> ToolExecutionContext {
    ToolExecutionContext::new(
        crate::runtime::envelope::RuntimeEnvelope::agent("workspace-a", "parity-run")
            .with_owner_id("prx_user")
            .with_channel("terminal"),
        "dm",
    )
}

async fn dispatch(
    service: &ToolExecutionService,
    context: ToolExecutionContext,
    name: &str,
    args: Value,
) -> ToolExecutionOutcome {
    tokio::time::timeout(
        Duration::from_secs(15),
        service.execute(ToolExecutionCommand::new(name, args), context, None),
    )
    .await
    .expect("test: dispatch deadline")
}

#[tokio::test]
async fn supervised_approval_does_not_enable_git() {
    let root = tempfile::tempdir().expect("test: workspace");
    let outcome = dispatch(
        &service(root.path(), AutonomyLevel::Supervised),
        context(),
        "shell",
        json!({"command":"git status"}),
    )
    .await;
    assert_eq!(
        outcome.status,
        ToolExecutionStatus::Failed,
        "test: approved shell reaches refusal"
    );
    assert!(
        outcome.process.as_ref().is_some_and(|process| !process.started
            && process
                .diagnostic
                .as_deref()
                .is_some_and(|error| error.contains("refusing to run git through the shell"))),
        "test: GitRefused before spawn: {outcome:?}"
    );
}

#[tokio::test]
async fn full_parent_supervised_child_keeps_native_restrictions_after_approval() {
    use super::capability::catalog::{CapabilityCatalog, CatalogSources, SurfacePolicy};
    use super::capability::child::{
        AuthorityEnvelope, CapabilityConstraints, ChildCapabilityManifest, ChildDispatchPin, VerifiedChildScope,
    };
    use crate::tools::capability::contract::CapabilityRef;
    let root = tempfile::tempdir().expect("test: workspace");
    let outside = tempfile::tempdir().expect("test: outside");
    let parent_service = service(root.path(), AutonomyLevel::Full);
    let policy = Arc::new(SecurityPolicy {
        workspace_dir: root.path().into(),
        ..SecurityPolicy::default()
    });
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(shell(root.path())),
        Box::new(FileWriteTool::new(policy.clone())),
        Box::new(FileReadTool::new(policy, false)),
    ];
    let catalog = Arc::new(
        CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default()).expect("test: catalog"),
    );
    let reference = |name: &str| {
        let entry = catalog.entry_for_public_name(name).expect("test: catalog entry");
        CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        }
    };
    let key = crate::security::runtime_key::DecisionKey::ephemeral();
    let parent = CapabilityConstraints {
        parent_authority_id: crate::causal_tree::ids::DecisionId::legacy(&[7; 32]).into(),
        allowed: ["shell", "file_write", "file_read"]
            .into_iter()
            .map(reference)
            .collect(),
        authority: AuthorityEnvelope {
            owner: "prx_user".into(),
            workspace: "workspace-a".into(),
            level: AutonomyLevel::Full,
            max_cost_cents: 100,
            remaining_depth: 2,
            delegation: None,
        },
        catalog_generation: *catalog.generation(),
        key: Arc::clone(&key),
    };
    let mut narrowed = parent.clone();
    narrowed.authority.level = AutonomyLevel::Supervised;
    let manifest =
        ChildCapabilityManifest::derive_strict_subset(&narrowed, &[reference("shell"), reference("file_write")])
            .expect("test: narrowed manifest");
    let scope = Arc::new(VerifiedChildScope::new(manifest, &parent).expect("test: verified child"));
    let mut child = context();
    child.child_scope = Some(scope);
    child.child_pin = Some(Arc::new(ChildDispatchPin {
        catalog,
        key,
        owner: "prx_user".into(),
        workspace: "workspace-a".into(),
    }));
    let git = dispatch(&parent_service, child.clone(), "shell", json!({"command":"git status"})).await;
    assert_eq!(
        git.status,
        ToolExecutionStatus::Failed,
        "test: child approved shell reaches refusal"
    );
    assert!(
        git.process.as_ref().is_some_and(|process| !process.started
            && process
                .diagnostic
                .as_deref()
                .is_some_and(|error| error.contains("refusing to run git through the shell"))),
        "test: child GitRefused before spawn: {git:?}"
    );
    let path = outside.path().join("child.txt");
    let write = dispatch(
        &parent_service,
        child,
        "file_write",
        json!({"path":path,"content":"blocked"}),
    )
    .await
    .result
    .expect("test: child file reached backend");
    assert!(!write.success && !path.exists(), "test: child out-of-root file denied");
}

#[tokio::test]
async fn configured_workdir_is_shared_by_shell_files_and_git() {
    let workspace = tempfile::tempdir().expect("test: workspace");
    let workdir = tempfile::tempdir().expect("test: working root");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().into();
    config.config_path = workspace.path().join("config.toml");
    config.runtime.workdir = Some(workdir.path().canonicalize().expect("test: canonical root"));
    config.autonomy.level = AutonomyLevel::Full;
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: memory"));
    let tools = super::all_tools(
        Arc::new(config.clone()),
        &security,
        memory,
        None,
        None,
        &config.browser,
        &config.http_request,
        workspace.path(),
        &config.agents,
        None,
        &config,
    );
    let find = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.name() == name)
            .expect("test: registered tool")
            .as_ref()
    };
    let pwd = direct(find("shell"), AutonomyLevel::Full, json!({"command":"pwd"})).await;
    assert!(pwd.success, "test: pwd: {pwd:?}");
    assert_eq!(
        pwd.output.trim(),
        workdir
            .path()
            .canonicalize()
            .expect("test: canonical root")
            .to_string_lossy(),
        "test: shell working root"
    );
    let init = direct(find("shell"), AutonomyLevel::Full, json!({"command":"git init"})).await;
    assert!(init.success, "test: init: {init:?}");
    let write = direct(
        find("file_write"),
        AutonomyLevel::Full,
        json!({"path":"relative.txt","content":"parity"}),
    )
    .await;
    assert!(
        write.success && workdir.path().join("relative.txt").exists(),
        "test: native root: {write:?}"
    );
    let status = direct(
        find("git_operations"),
        AutonomyLevel::Full,
        json!({"operation":"status"}),
    )
    .await;
    assert!(
        status.success && status.output.contains("relative.txt"),
        "test: hardened git root: {status:?}"
    );
    assert!(
        !workspace.path().join("relative.txt").exists(),
        "test: security workspace stays separate"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn full_dispatcher_widens_native_files_and_environment() {
    let root = tempfile::tempdir().expect("test: workspace");
    let outside = tempfile::tempdir().expect("test: outside workspace");
    let service = service(root.path(), AutonomyLevel::Full);
    let target = outside.path().join("file");
    let result = dispatch(
        &service,
        context(),
        "file_write",
        json!({"path":target,"content":"written"}),
    )
    .await
    .result
    .expect("test: dispatched write");
    assert!(
        result.success && target.exists(),
        "test: Full dispatcher bypasses native confinement: {result:?}"
    );
    const PROBE: &str = "PRX_DISPATCH_ENV_TEST";
    if std::env::var_os(PROBE).is_none() {
        let mut command = tokio::process::Command::new(std::env::current_exe().expect("test: executable"));
        command
            .args([
                "--exact",
                "tools::shell_parity_tests::full_dispatcher_widens_native_files_and_environment",
            ])
            .env(PROBE, "1")
            .env("http_proxy", "prx-proxy")
            .env("ANTHROPIC_API_KEY", "prx-secret")
            .kill_on_drop(true);
        run_exact_environment_probe(command).await;
        return;
    }
    let result = dispatch(
        &service,
        context(),
        "shell",
        json!({"command":"printf '%s|%s' \"$http_proxy\" \"${ANTHROPIC_API_KEY-unset}\""}),
    )
    .await
    .result
    .expect("test: dispatched env");
    assert!(
        result.success && result.output.contains("prx-proxy|unset"),
        "test: dispatcher grants environment inheritance with secret hygiene: {result:?}"
    );
}

#[tokio::test]
async fn working_root_forbidden_file_matrix() {
    let workspace = tempfile::tempdir().expect("test: policy workspace");
    let root = tempfile::tempdir().expect("test: execution root");
    let forbidden = root.path().join("blocked");
    std::fs::create_dir(&forbidden).expect("test: forbidden directory");
    for level in [AutonomyLevel::Supervised, AutonomyLevel::ReadOnly] {
        let policy = Arc::new(SecurityPolicy {
            workspace_dir: workspace.path().into(),
            autonomy: level,
            workspace_only: false,
            forbidden_paths: vec![forbidden.to_string_lossy().into_owned()],
            ..SecurityPolicy::default()
        });
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(FileReadTool::new(policy.clone(), false).with_workdir(Some(root.path().into()))),
            Box::new(FileWriteTool::new(policy.clone()).with_workdir(Some(root.path().into()))),
            Box::new(FileEditTool::new(policy).with_workdir(Some(root.path().into()))),
        ];
        for tool in tools {
            for (path, blocked) in [("blocked/target", true), ("allowed", false)] {
                let target = root.path().join(path);
                std::fs::write(&target, "original").expect("test: target fixture");
                let mut args = json!({"path":path,"content":"changed","old_string":"original","new_string":"changed"});
                if tool.name() != "file_read" {
                    let operation = crate::security::approval_operation::file_operation(
                        tool.name(),
                        workspace.path(),
                        &target.canonicalize().expect("test: approval target"),
                    )
                    .0;
                    let grant = serde_json::to_value(crate::security::policy::ApprovalGrant::for_resource_operation(
                        tool.name(),
                        &operation,
                        "PRXOperator",
                        None,
                    ))
                    .expect("test: resource approval");
                    args.as_object_mut()
                        .expect("test: argument object")
                        .insert(crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.to_owned(), grant);
                }
                let result = direct(tool.as_ref(), level, args).await;
                let readonly_mutation = level == AutonomyLevel::ReadOnly && tool.name() != "file_read";
                if blocked || readonly_mutation {
                    assert!(
                        !result.success,
                        "test: {} {level:?} must refuse {path}: {result:?}",
                        tool.name()
                    );
                    let expected = if readonly_mutation {
                        "Action blocked: autonomy is read-only".to_owned()
                    } else {
                        format!("Path not allowed by security policy: {path}")
                    };
                    assert_eq!(
                        result.error.as_deref(),
                        Some(expected.as_str()),
                        "test: native typed refusal for {} {level:?}",
                        tool.name()
                    );
                    assert_eq!(
                        std::fs::read(&target).expect("test: unchanged fixture"),
                        b"original",
                        "test: refused {} leaves target unchanged",
                        tool.name()
                    );
                } else {
                    assert!(result.success, "test: allowed {} {level:?}: {result:?}", tool.name());
                }
            }
        }
    }
}

#[tokio::test]
async fn working_root_forbidden_shell_argument_is_refused() {
    let workspace = tempfile::tempdir().expect("test: workspace");
    let root = tempfile::tempdir().expect("test: root");
    let policy = Arc::new(SecurityPolicy {
        workspace_dir: workspace.path().into(),
        workspace_only: false,
        forbidden_paths: vec![root.path().join("blocked").to_string_lossy().into_owned()],
        ..SecurityPolicy::default()
    });
    let tool = shell(root.path()).with_path_policy(policy);
    for level in [AutonomyLevel::Supervised, AutonomyLevel::ReadOnly] {
        for command in ["cat blocked/target", "sh -c 'cat blocked/target'"] {
            let denied = direct(&tool, level, json!({"command":command})).await;
            assert_eq!(
                denied.error.as_deref(),
                Some("forbidden path argument: blocked/target"),
                "test: shell and nested shell checks use actual cwd"
            );
        }
        let allowed = direct(&tool, level, json!({"command":"printf allowed; test -n \"$PATH\""})).await;
        assert!(
            allowed.success,
            "test: allowed shell argument remains executable: {allowed:?}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn working_root_symlink_retains_existing_policy() {
    let workspace = tempfile::tempdir().expect("test: workspace");
    let root = tempfile::tempdir().expect("test: root");
    let target = root.path().join("forbidden");
    std::fs::write(&target, "fixture").expect("test: target");
    std::os::unix::fs::symlink(&target, workspace.path().join("link")).expect("test: symlink");
    for workspace_only in [true, false] {
        let policy = Arc::new(SecurityPolicy {
            workspace_dir: workspace.path().into(),
            workspace_only,
            forbidden_paths: vec![target.to_string_lossy().into_owned()],
            ..SecurityPolicy::default()
        });
        for explicit_root in [false, true] {
            let tool =
                FileReadTool::new(policy.clone(), false).with_workdir(explicit_root.then(|| workspace.path().into()));
            let result = direct(&tool, AutonomyLevel::ReadOnly, json!({"path":"link"})).await;
            assert_eq!(
                result.success, !workspace_only,
                "test: explicit root retains main's canonical containment behavior: {result:?}"
            );
        }
    }
}

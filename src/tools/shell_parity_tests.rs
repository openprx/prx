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
        let status = tokio::time::timeout(Duration::from_secs(25), command.status())
            .await
            .expect("test: probe deadline")
            .expect("test: probe exit");
        assert!(status.success(), "test: isolated environment probe");
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
    use crate::causal_tree::branch::CapabilityRef;
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
    let key = crate::decision::DecisionKey::ephemeral();
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

struct RebuildAutonomy;
struct PreparedAutonomy;
impl crate::config::ConfigGenerationParticipant for RebuildAutonomy {
    fn name(&self) -> &'static str {
        "parity-autonomy"
    }
    fn supports_rebuild_field(&self, field: &str) -> bool {
        field == "autonomy"
    }
    fn prepare(
        &self,
        _: Arc<crate::config::ConfigGeneration>,
        _: &[String],
    ) -> anyhow::Result<Box<dyn crate::config::PreparedConfigGeneration>> {
        Ok(Box::new(PreparedAutonomy))
    }
}
impl crate::config::PreparedConfigGeneration for PreparedAutonomy {
    fn commit(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn rollback(&mut self) {}
}

#[tokio::test]
#[serial_test::serial]
async fn generation_swap_changes_git_outcome_between_calls() {
    use crate::config::generation::{ConfigGenerationManager, ConfigReloadTrigger};
    let root = tempfile::tempdir().expect("test: workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = root.path().into();
    config.config_path = root.path().join("config.toml");
    config.autonomy.level = AutonomyLevel::Full;
    let manager = ConfigGenerationManager::new(config.clone());
    let participant: Arc<dyn crate::config::ConfigGenerationParticipant> = Arc::new(RebuildAutonomy);
    manager.register_participant(&participant);
    for level in [AutonomyLevel::Full, AutonomyLevel::Supervised] {
        config.autonomy.level = level;
        manager
            .apply_runtime_config(config.clone(), ConfigReloadTrigger::Test)
            .expect("test: generation swap");
        let current = manager.pin();
        let result = dispatch(
            &service(root.path(), current.effective.autonomy.level),
            context(),
            "shell",
            json!({"command":"git --version"}),
        )
        .await
        .result
        .expect("test: shell result");
        assert_eq!(
            result.success,
            level == AutonomyLevel::Full,
            "test: live generation controls next call"
        );
    }
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

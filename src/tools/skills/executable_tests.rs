#![cfg(test)]
//! SL11 at the tool boundary: `skill_execute` for lifecycle-managed skills runs
//! the pinned revision's script through the real shell adapter, with real
//! processes and real policy objects, and `skills_manage` publishes a reviewed
//! executable only under a grant bound to the complete bundle hash.
#![allow(clippy::indexing_slicing)]

use super::*;
use crate::security::AutonomyLevel;
use crate::skills::lifecycle::{
    Bundle, BundleEntry, LifecycleMode, OwnerScope, SkillId, SkillLifecycleStore, SkillsLifecycleService,
    SkillsLifecycleSettings, SourceKind, release_task_pins,
};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tempfile::TempDir;

const NAME: &str = "Report runner";
const SCRIPT_TOOL: &str = "[[tools]]\nname = \"run\"\ndescription = \"run it\"\nkind = \"script\"\ncommand = \"scripts/run.sh\"\n\n[tools.args]\nmessage = \"hello\"\nmarks = \"\"\n";

fn unique(tag: &str) -> String {
    format!("{tag}-{}", uuid::Uuid::new_v4().simple())
}

fn skill_md() -> String {
    format!("---\nname: {NAME}\ndescription: Runs the report\n---\n\nRun the report.\n")
}

fn skill_toml(tools: &str) -> String {
    format!("[skill]\nname = \"{NAME}\"\ndescription = \"Runs the report\"\nversion = \"1.0.0\"\n\n{tools}")
}

fn entries(script: &str, tools: &str) -> Vec<BundleEntry> {
    let file = |path: &str, bytes: &[u8], executable: bool| BundleEntry {
        path: path.into(),
        bytes: bytes.to_vec(),
        executable,
    };
    vec![
        file("SKILL.md", skill_md().as_bytes(), false),
        file("SKILL.toml", skill_toml(tools).as_bytes(), false),
        file("references/guide.md", b"# Guide\n", false),
        file("scripts/run.sh", script.as_bytes(), true),
    ]
}

fn bundle(script: &str) -> Bundle {
    Bundle::from_entries(entries(script, SCRIPT_TOOL)).expect("test: bundle")
}

struct Managed {
    tmp: TempDir,
    owner: String,
    store: SkillLifecycleStore,
    skill: SkillId,
}

fn managed(tag: &str) -> Managed {
    let tmp = TempDir::new().expect("test: tempdir");
    let owner = format!("owner:ws:cli:sl11-{}", unique(tag));
    let store =
        SkillLifecycleStore::open(tmp.path(), OwnerScope::new(&owner).expect("test: owner")).expect("test: store");
    Managed {
        tmp,
        owner,
        store,
        skill: SkillId::parse("report-runner").expect("test: skill id"),
    }
}

impl Managed {
    fn publish(&self, bundle: &Bundle, generation: u64) {
        crate::skills::lifecycle::test_support::publish_reviewed_executable(
            &self.store,
            &self.skill,
            bundle,
            generation,
        );
    }

    fn marks(&self) -> PathBuf {
        let dir = self.tmp.path().join("marks");
        std::fs::create_dir_all(&dir).expect("test: marks dir");
        dir
    }

    fn run_root(&self) -> PathBuf {
        self.tmp.path().join("self/skill-exec")
    }

    fn leftover_runs(&self) -> usize {
        std::fs::read_dir(self.run_root()).map_or(0, Iterator::count)
    }

    fn tool_with(&self, security: SecurityPolicy, config: Config) -> SkillExecuteTool {
        let runtime: Arc<dyn RuntimeAdapter> = Arc::new(crate::runtime::NativeRuntime::new());
        SkillExecuteTool::new(
            self.tmp.path().to_path_buf(),
            config,
            runtime,
            Arc::new(security),
            &crate::config::BrowserConfig::default(),
            &crate::config::HttpRequestConfig::default(),
        )
    }

    fn config(&self) -> Config {
        let mut config = Config::default();
        config.workspace_dir = self.tmp.path().to_path_buf();
        config.skills_lifecycle.owner_scope = Some(self.owner.clone());
        config.skills.open_skills_dir = Some(self.tmp.path().join("missing-open").to_string_lossy().to_string());
        config.skills.openclaw_skills_dir = Some(self.tmp.path().join("missing-claw").to_string_lossy().to_string());
        config
    }

    fn policy(&self, autonomy: AutonomyLevel) -> SecurityPolicy {
        SecurityPolicy {
            autonomy,
            workspace_dir: self.tmp.path().to_path_buf(),
            ..SecurityPolicy::default()
        }
    }

    fn tool(&self) -> SkillExecuteTool {
        self.tool_with(self.policy(AutonomyLevel::Full), self.config())
    }

    fn call(&self, task: &str, tool: &str, arguments: &serde_json::Value) -> serde_json::Value {
        json!({
            "skill": NAME,
            "tool": tool,
            "arguments": arguments,
            "_zc_scope_trusted": true,
            "_zc_scope": {
                "owner_id": self.owner,
                "task_id": task,
                "sender": "alice",
                "channel": "cli",
                "chat_type": "private"
            }
        })
    }

    fn release(&self, tasks: &[&str]) {
        let owner = OwnerScope::new(&self.owner).expect("test: owner");
        for task in tasks {
            release_task_pins(&owner, task);
        }
    }
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .map(|out| {
            let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
            !state.is_empty() && !state.starts_with('Z')
        })
        .unwrap_or(false)
}

fn recorded_pids(marks: &Path) -> Vec<String> {
    ["leader.pid", "child.pid"]
        .iter()
        .map(|name| {
            std::fs::read_to_string(marks.join(name))
                .expect("test: the script recorded its pid")
                .trim()
                .to_string()
        })
        .collect()
}

async fn assert_all_gone(pids: &[String]) {
    for pid in pids {
        let mut tries = 0;
        while alive(pid) && tries < 100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            tries += 1;
        }
        assert!(!alive(pid), "process {pid} outlived the call");
    }
}

const SPAWNING_SCRIPT: &str = "#!/bin/sh\necho $$ > \"$PRX_SKILL_ARG_MARKS/leader.pid\"\nsleep 300 &\necho $! > \"$PRX_SKILL_ARG_MARKS/child.pid\"\nwait\n";

// ---- the pin -------------------------------------------------------------------------

#[tokio::test]
async fn a_pinned_task_runs_a_s_script_after_b_is_published_and_a_new_task_runs_b() {
    let m = managed("pin");
    m.publish(&bundle("#!/bin/sh\necho \"A:$PRX_SKILL_ARG_MESSAGE\"\n"), 0);
    let tool = m.tool();
    let (task, later) = (unique("task"), unique("task"));
    let first = tool
        .execute(m.call(&task, "run", &json!({"message": "one"})))
        .await
        .expect("test: execute");
    assert!(first.success, "{:?}", first.error);
    assert_eq!(first.output.trim(), "A:one");

    m.publish(&bundle("#!/bin/sh\necho \"B:$PRX_SKILL_ARG_MESSAGE\"\n"), 1);
    let pinned = tool
        .execute(m.call(&task, "run", &json!({"message": "two"})))
        .await
        .expect("test: execute");
    assert_eq!(
        pinned.output.trim(),
        "A:two",
        "no read-to-execute switch to the newer script"
    );
    let fresh = tool
        .execute(m.call(&later, "run", &json!({"message": "three"})))
        .await
        .expect("test: execute");
    assert_eq!(fresh.output.trim(), "B:three");
    assert_eq!(m.leftover_runs(), 0);
    m.release(&[&task, &later]);
}

#[tokio::test]
async fn a_task_that_read_the_skill_executes_the_revision_it_read() {
    let m = managed("read-exec");
    m.publish(&bundle("#!/bin/sh\necho A\n"), 0);
    let read = SkillReadTool::new(m.tmp.path().to_path_buf(), m.config());
    let task = unique("task");
    let read_call = json!({
        "name": NAME,
        "_zc_scope_trusted": true,
        "_zc_scope": {"owner_id": m.owner, "task_id": task}
    });
    let read = read.execute(read_call).await.expect("test: read");
    assert!(read.success, "{:?}", read.error);
    m.publish(&bundle("#!/bin/sh\necho B\n"), 1);
    let ran = m
        .tool()
        .execute(m.call(&task, "run", &json!({})))
        .await
        .expect("test: execute");
    assert_eq!(ran.output.trim(), "A", "the read pinned A for execution too");
    m.release(&[&task]);
}

#[tokio::test]
async fn a_tampered_or_missing_script_is_refused_and_nothing_runs() {
    let m = managed("tamper");
    let marks = m.marks();
    let script = "#!/bin/sh\necho ran >> \"$PRX_SKILL_ARG_MARKS/ran\"\n";
    m.publish(&bundle(script), 0);
    let task = unique("task");
    let marks_arg = json!({"marks": marks.to_string_lossy()});
    let ok = m
        .tool()
        .execute(m.call(&task, "run", &marks_arg))
        .await
        .expect("test: execute");
    assert!(ok.success, "{:?}", ok.error);
    let ran = |m: &Managed| {
        std::fs::read_to_string(m.marks().join("ran"))
            .unwrap_or_default()
            .lines()
            .count()
    };
    assert_eq!(ran(&m), 1);

    let pin = crate::skills::lifecycle::task_pin(
        &OwnerScope::new(&m.owner).expect("test: owner"),
        &task,
        &m.skill,
        || Ok(None),
    )
    .expect("test: pin lookup")
    .expect("test: the task is pinned");
    let stored = m
        .tmp
        .path()
        .join("self/skill-lifecycle/revisions")
        .join(m.skill.as_str())
        .join(pin.revision_id.as_str())
        .join("bundle/scripts/run.sh");
    std::fs::set_permissions(&stored, std::fs::Permissions::from_mode(0o700)).expect("test: chmod");
    std::fs::write(&stored, "#!/bin/sh\necho EVIL >> \"$PRX_SKILL_ARG_MARKS/ran\"\n").expect("test: tamper");
    let refused = m
        .tool()
        .execute(m.call(&task, "run", &marks_arg))
        .await
        .expect_err("a tampered script must be refused");
    assert!(refused.to_string().contains("was refused"), "{refused}");
    std::fs::remove_file(&stored).expect("test: remove");
    assert!(m.tool().execute(m.call(&task, "run", &marks_arg)).await.is_err());
    assert_eq!(ran(&m), 1, "neither the tampered nor the missing script ran");
    assert_eq!(m.leftover_runs(), 0);
    m.release(&[&task]);
}

// ---- arguments, scope, policy ------------------------------------------------------------

#[tokio::test]
async fn arguments_outside_the_declared_schema_are_refused_before_anything_runs() {
    let m = managed("args");
    let marks = m.marks();
    m.publish(&bundle("#!/bin/sh\necho ran >> \"$PRX_SKILL_ARG_MARKS/ran\"\n"), 0);
    let task = unique("task");
    for bad in [
        json!({"marks": marks.to_string_lossy(), "extra": "x"}),
        json!({"marks": marks.to_string_lossy(), "message": {"a": 1}}),
        json!({"marks": marks.to_string_lossy(), "message": "x".repeat(5000)}),
        json!("a string"),
    ] {
        let refused = m.tool().execute(m.call(&task, "run", &bad)).await;
        assert!(refused.is_err(), "{bad}");
    }
    assert!(!marks.join("ran").exists());
    let ok = m
        .tool()
        .execute(m.call(&task, "run", &json!({"marks": marks.to_string_lossy()})))
        .await
        .expect("test: execute");
    assert!(ok.success, "{:?}", ok.error);
    m.release(&[&task]);
}

#[tokio::test]
async fn an_undeclared_or_ungranted_tool_is_refused() {
    let m = managed("scope");
    let marks = m.marks();
    m.publish(&bundle("#!/bin/sh\necho ran >> \"$PRX_SKILL_ARG_MARKS/ran\"\n"), 0);
    let task = unique("task");
    let args = json!({"marks": marks.to_string_lossy()});

    let undeclared = m.tool().execute(m.call(&task, "not-declared", &args)).await;
    assert!(undeclared.is_err());

    // The security policy denies the shell for this caller: the script's own
    // manifest cannot grant it back.
    let deny = SecurityPolicy {
        scope_rules: vec![crate::config::ScopeRule {
            tools_deny: vec!["shell".to_string()],
            ..crate::config::ScopeRule::default()
        }],
        ..m.policy(AutonomyLevel::Full)
    };
    let denied = m
        .tool_with(deny, m.config())
        .execute(m.call(&task, "run", &args))
        .await
        .expect_err("a denied shell must refuse the skill's script");
    assert!(denied.to_string().contains("is not granted"), "{denied}");

    // A read-only installation grants nothing that acts.
    let read_only = m
        .tool_with(m.policy(AutonomyLevel::ReadOnly), m.config())
        .execute(m.call(&task, "run", &args))
        .await;
    assert!(read_only.is_err());

    // A tool the run excludes from its surface is not granted either.
    let mut excluded = m.config();
    excluded.tool_tiering.always_exclude = vec!["shell".to_string()];
    let hidden = m
        .tool_with(m.policy(AutonomyLevel::Full), excluded)
        .execute(m.call(&task, "run", &args))
        .await;
    assert!(hidden.is_err());

    assert!(!marks.join("ran").exists(), "no refused call ran the script");
    assert_eq!(m.leftover_runs(), 0);
    m.release(&[&task]);
}

// ---- cleanup -------------------------------------------------------------------------------

#[tokio::test]
async fn a_long_running_script_is_killed_and_reaped_on_timeout() {
    let m = managed("timeout");
    let marks = m.marks();
    m.publish(&bundle(SPAWNING_SCRIPT), 0);
    let task = unique("task");
    let tool = m.tool().with_managed_timeout(Duration::from_secs(3));
    let started = std::time::Instant::now();
    let result = tool
        .execute(m.call(&task, "run", &json!({"marks": marks.to_string_lossy()})))
        .await
        .expect("test: a timeout is a failed result");
    assert!(!result.success);
    assert!(
        result.error.as_deref().unwrap_or_default().contains("timed out"),
        "{:?}",
        result.error
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_all_gone(&recorded_pids(&marks)).await;
    assert_eq!(m.leftover_runs(), 0, "the run directory is removed");
    m.release(&[&task]);
}

#[tokio::test]
async fn a_cancelled_call_kills_the_process_tree_and_removes_its_run_directory() {
    let m = managed("cancel");
    let marks = m.marks();
    m.publish(&bundle(SPAWNING_SCRIPT), 0);
    let task = unique("task");
    let tool = m.tool();
    let token = CancellationToken::new();
    let canceller = {
        let (marks, token) = (marks.clone(), token.clone());
        async move {
            for _ in 0..200 {
                if marks.join("child.pid").exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            token.cancel();
        }
    };
    let call = m.call(&task, "run", &json!({"marks": marks.to_string_lossy()}));
    let (result, ()) = tokio::join!(
        tool.execute_named_with_cancellation("skill_execute", call, Some(token.clone())),
        canceller
    );
    let result = result.expect("test: cancellation is a failed result");
    assert!(!result.success);
    assert_eq!(
        result.error.as_deref(),
        Some(crate::tools::traits::TOOL_EXECUTION_CANCELLED)
    );
    assert_all_gone(&recorded_pids(&marks)).await;
    assert_eq!(m.leftover_runs(), 0);
    m.release(&[&task]);
}

#[tokio::test]
async fn the_success_path_is_joined_nothing_the_script_started_outlives_it() {
    let m = managed("success");
    let marks = m.marks();
    // Exits at once and leaves a child holding the output pipes.
    m.publish(
        &bundle("#!/bin/sh\necho $$ > \"$PRX_SKILL_ARG_MARKS/leader.pid\"\nsleep 300 &\necho $! > \"$PRX_SKILL_ARG_MARKS/child.pid\"\necho done\n"),
        0,
    );
    let task = unique("task");
    let started = std::time::Instant::now();
    let result = m
        .tool()
        .execute(m.call(&task, "run", &json!({"marks": marks.to_string_lossy()})))
        .await
        .expect("test: execute");
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output.trim(), "done");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the call did not wait for the stray child"
    );
    assert_all_gone(&recorded_pids(&marks)).await;
    assert_eq!(m.leftover_runs(), 0);
    m.release(&[&task]);
}

// ---- legacy skills are unchanged ----------------------------------------------------------------

#[tokio::test]
async fn an_unmanaged_executable_skill_runs_exactly_as_before() {
    let tmp = TempDir::new().expect("test: tempdir");
    let skill_dir = tmp.path().join("skills/demo");
    std::fs::create_dir_all(skill_dir.join("scripts")).expect("test: dirs");
    std::fs::write(
        skill_dir.join("SKILL.toml"),
        "[skill]\nname='demo'\ndescription='demo skill'\n[[tools]]\nname='where'\ndescription='w'\nkind='script'\ncommand='scripts/where.sh'\n",
    )
    .expect("test: toml");
    let script = skill_dir.join("scripts/where.sh");
    std::fs::write(&script, "#!/bin/sh\npwd\necho \"$PRX_SKILL_ARG_ANYTHING\"\n").expect("test: script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("test: chmod");
    let mut config = Config::default();
    config.workspace_dir = tmp.path().to_path_buf();
    config.skills.open_skills_dir = Some(tmp.path().join("none-a").to_string_lossy().to_string());
    config.skills.openclaw_skills_dir = Some(tmp.path().join("none-b").to_string_lossy().to_string());
    // The owner has a lifecycle store, but this skill has no binding in it.
    let owner = format!("owner:ws:cli:sl11-{}", unique("legacy"));
    SkillLifecycleStore::open(tmp.path(), OwnerScope::new(&owner).expect("test: owner")).expect("test: store");
    let runtime: Arc<dyn RuntimeAdapter> = Arc::new(crate::runtime::NativeRuntime::new());
    let tool = SkillExecuteTool::new(
        tmp.path().to_path_buf(),
        config,
        runtime,
        Arc::new(SecurityPolicy::default()),
        &crate::config::BrowserConfig::default(),
        &crate::config::HttpRequestConfig::default(),
    );
    let result = tool
        .execute(json!({
            "skill": "demo",
            "tool": "where",
            // The legacy path takes arguments the manifest did not declare.
            "arguments": {"anything": "goes"},
            "_zc_scope_trusted": true,
            "_zc_scope": {"owner_id": owner, "task_id": unique("task")}
        }))
        .await
        .expect("test: execute");
    assert!(result.success, "{:?}", result.error);
    let mut lines = result.output.lines();
    let canonical = std::fs::canonicalize(&skill_dir).expect("test: canonical");
    assert_eq!(
        lines.next().map(Path::new),
        Some(canonical.as_path()),
        "it still runs in the skill directory"
    );
    assert_eq!(lines.next(), Some("goes"));
    assert!(
        !tmp.path().join("self/skill-exec").exists(),
        "the legacy path stages nothing"
    );
}

// ---- operator import through skills_manage -------------------------------------------------------

fn grant_for(operation: &str, owner: &str) -> serde_json::Value {
    use crate::acl::approval_grant::{ApprovalGrantV2, IssuerAuthority, RiskLevel, Subject, WitnessKeyring};
    let grant = ApprovalGrantV2::issue_one_shot(
        WitnessKeyring::global_for_tests(),
        Subject {
            agent_id: "prx:test:agent".to_string(),
            principal_id: "telegram:alice".to_string(),
            owner_id: owner.to_string(),
            workspace_id: "ws".to_string(),
            session_key: Some("session".to_string()),
        },
        IssuerAuthority::HumanReview,
        operation,
        RiskLevel::High,
    )
    .expect("test: grant");
    serde_json::to_value(ApprovalGrant::from_verified_v2("skills_manage", "test", grant)).expect("test: grant json")
}

fn with_grant(mut args: serde_json::Value, operation: &str, owner: &str) -> serde_json::Value {
    if let Some(object) = args.as_object_mut() {
        object.insert(
            crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.to_string(),
            grant_for(operation, owner),
        );
        object.insert("_zc_scope_trusted".to_string(), json!(true));
        object.insert(
            "_zc_scope".to_string(),
            json!({"principal_id": "telegram:alice", "owner_id": owner}),
        );
    }
    args
}

fn manage_tool(m: &Managed, mode: LifecycleMode, autonomy: AutonomyLevel) -> SkillsManageTool {
    let store =
        SkillLifecycleStore::open(m.tmp.path(), OwnerScope::new(&m.owner).expect("test: owner")).expect("test: store");
    let service = SkillsLifecycleService::new(
        Arc::new(store),
        SkillsLifecycleSettings {
            enabled: true,
            mode,
            ..SkillsLifecycleSettings::default()
        },
    )
    .expect("test: service");
    SkillsManageTool::new(m.tmp.path().to_path_buf(), m.config(), Arc::new(m.policy(autonomy)))
        .with_lifecycle(Arc::new(service))
}

fn write_source(m: &Managed, script: &str) -> (PathBuf, String) {
    let source = m.tmp.path().join("incoming/report-runner");
    let _ = std::fs::remove_dir_all(&source);
    for file in entries(script, SCRIPT_TOOL) {
        let target = source.join(&file.path);
        std::fs::create_dir_all(target.parent().expect("test: parent")).expect("test: dirs");
        std::fs::write(&target, &file.bytes).expect("test: file");
        let mode = if file.executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).expect("test: chmod");
    }
    let revision = Bundle::from_dir(&source)
        .expect("test: reviewed bundle")
        .revision_id()
        .to_string();
    (source, revision)
}

#[tokio::test]
async fn publishing_an_executable_needs_a_grant_bound_to_the_reviewed_bundle_hash() {
    for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
        let m = managed("grant-import");
        let tool = manage_tool(&m, LifecycleMode::Auto, autonomy);
        let (source, revision) = write_source(&m, "#!/bin/sh\necho A\n");
        let source_text = source.to_string_lossy().to_string();
        let args = json!({"action": "lifecycle_import_executable", "source": source_text, "revision_id": revision});
        let operation = format!("skills_manage:lifecycle_import_executable:{source_text}:{revision}");

        // No grant, a grant for another hash, and a grant for another action.
        let other = format!("r1-{}", "7".repeat(64));
        for operation in [
            None,
            Some(format!(
                "skills_manage:lifecycle_import_executable:{source_text}:{other}"
            )),
            Some(format!("skills_manage:lifecycle_publish:{source_text}:{revision}")),
        ] {
            let call = operation.map_or_else(
                || {
                    let mut call = args.clone();
                    call["_zc_scope_trusted"] = json!(true);
                    call["_zc_scope"] = json!({"owner_id": m.owner});
                    call
                },
                |operation| with_grant(args.clone(), &operation, &m.owner),
            );
            let refused = tool.execute(call).await.expect("test: execute");
            assert!(!refused.success, "{autonomy:?}");
            assert!(
                refused
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("explicit approval grant"),
                "{autonomy:?}: {:?}",
                refused.error
            );
        }
        assert!(
            SkillLifecycleStore::open(m.tmp.path(), OwnerScope::new(&m.owner).expect("test: owner"))
                .expect("test: store")
                .list_active_bindings()
                .expect("test: bindings")
                .is_empty(),
            "nothing was published without the bound grant"
        );

        let call = with_grant(args.clone(), &operation, &m.owner);
        let published = tool.execute(call.clone()).await.expect("test: execute");
        assert!(published.success, "{autonomy:?}: {:?}", published.error);
        assert!(published.output.contains(&revision), "{}", published.output);
        // The one-shot grant cannot be replayed.
        let replay = tool.execute(call).await.expect("test: execute");
        assert!(!replay.success, "{autonomy:?}: a spent grant must not publish again");
    }
}

#[tokio::test]
async fn an_executable_publication_is_refused_outside_auto_mode_even_with_the_grant() {
    let m = managed("shadow-import");
    let tool = manage_tool(&m, LifecycleMode::Shadow, AutonomyLevel::Full);
    let (source, revision) = write_source(&m, "#!/bin/sh\necho A\n");
    let source_text = source.to_string_lossy().to_string();
    let operation = format!("skills_manage:lifecycle_import_executable:{source_text}:{revision}");
    let result = tool
        .execute(with_grant(
            json!({"action": "lifecycle_import_executable", "source": source_text, "revision_id": revision}),
            &operation,
            &m.owner,
        ))
        .await
        .expect("test: execute");
    assert!(!result.success);
    assert!(
        result.error.as_deref().unwrap_or_default().contains("does not allow"),
        "{:?}",
        result.error
    );
}

/// The operation the runtime mints for the call is the one the tool checks.
#[test]
fn the_runtime_mints_the_import_operation_the_tool_checks() {
    use crate::security::approval_operation::{GrantOp, grant_op_for_call};
    let args = json!({"action": "lifecycle_import_executable", "source": " /x/y ", "revision_id": "r1-abc"});
    let (operation, risk) = match grant_op_for_call("skills_manage", &args) {
        Some(GrantOp::ResourceExact(operation, risk)) => Some((operation, risk)),
        _ => None,
    }
    .expect("test: an exact resource operation");
    assert_eq!(operation, "skills_manage:lifecycle_import_executable:/x/y:r1-abc");
    assert_eq!(risk, crate::acl::approval_grant::RiskLevel::High);
    assert!(
        grant_op_for_call(
            "skills_manage",
            &json!({"action": "lifecycle_import_executable", "source": "/x"})
        )
        .is_none()
    );
}

// ---- environment, fallback, review ---------------------------------------------------------------

#[tokio::test]
async fn a_managed_script_does_not_inherit_the_prx_environment() {
    let m = managed("env");
    m.publish(&bundle("#!/bin/sh\nenv\n"), 0);
    let task = unique("task");
    let result = m
        .tool()
        .execute(m.call(&task, "run", &json!({"message": "hi"})))
        .await
        .expect("test: execute");
    assert!(result.success, "{:?}", result.error);
    for line in result.output.lines() {
        let key = line.split('=').next().unwrap_or_default();
        assert!(
            matches!(
                key,
                "PATH" | "LANG" | "TZ" | "HOME" | "TMPDIR" | "PWD" | "OLDPWD" | "SHLVL" | "_"
            ) || key.starts_with("LC_")
                || key.starts_with("PRX_SKILL_ARG_"),
            "the script sees {key}"
        );
    }
    // Every other variable of the test process stays behind.
    let foreign: Vec<String> = std::env::vars_os()
        .filter_map(|(key, _)| key.into_string().ok())
        .filter(|key| {
            !(matches!(
                key.as_str(),
                "PATH" | "LANG" | "TZ" | "HOME" | "TMPDIR" | "PWD" | "OLDPWD" | "SHLVL" | "_"
            ) || key.starts_with("LC_"))
        })
        .collect();
    assert!(!foreign.is_empty(), "the test process has variables to leak");
    for key in &foreign {
        assert!(!result.output.contains(&format!("{key}=")), "{key} leaked");
    }
    assert!(result.output.contains("PRX_SKILL_ARG_MESSAGE=hi"), "{}", result.output);
    assert!(
        result.output.contains("HOME=") && result.output.contains(".tmp"),
        "{}",
        result.output
    );
    m.release(&[&task]);
}

#[tokio::test]
async fn a_managed_skill_never_falls_back_to_the_unpinned_legacy_path() {
    let m = managed("fallback");
    let marks = m.marks();
    m.publish(&bundle("#!/bin/sh\necho ran >> \"$PRX_SKILL_ARG_MARKS/ran\"\n"), 0);
    let args = json!({"marks": marks.to_string_lossy()});
    // No trusted scope at all: the call has no owner to pin for.
    let untrusted = m
        .tool()
        .execute(json!({"skill": NAME, "tool": "run", "arguments": args}))
        .await;
    assert!(untrusted.is_err(), "{untrusted:?}");
    // A trusted caller of another owner.
    let mut other = m.call(&unique("task"), "run", &args);
    other["_zc_scope"]["owner_id"] = json!("owner:ws:cli:someone-else");
    let foreign = m.tool().execute(other).await;
    assert!(foreign.is_err(), "{foreign:?}");
    assert!(
        foreign
            .map(|_| ())
            .unwrap_err()
            .to_string()
            .contains("trusted owner scope"),
    );
    assert!(!marks.join("ran").exists(), "the projection was never run unpinned");
}

#[tokio::test]
async fn an_unreviewed_executable_revision_is_refused_by_the_tool() {
    let m = managed("unreviewed");
    let marks = m.marks();
    crate::skills::lifecycle::test_support::publish_revision(
        &m.store,
        &m.skill,
        SourceKind::Operator,
        &bundle("#!/bin/sh\necho ran >> \"$PRX_SKILL_ARG_MARKS/ran\"\n"),
        0,
    );
    let task = unique("task");
    let refused = m
        .tool()
        .execute(m.call(&task, "run", &json!({"marks": marks.to_string_lossy()})))
        .await
        .expect_err("a revision no operator reviewed must not execute");
    assert!(refused.to_string().contains("lifecycle_import_executable"), "{refused}");
    assert!(!marks.join("ran").exists());
    m.release(&[&task]);
}

#[tokio::test]
async fn the_legacy_path_never_runs_a_projection_whatever_owner_scope_is_configured() {
    let m = managed("projection");
    let marks = m.marks();
    m.publish(&bundle("#!/bin/sh\necho ran >> \"$PRX_SKILL_ARG_MARKS/ran\"\n"), 0);
    let args = json!({"marks": marks.to_string_lossy()});
    let mut no_owner = m.config();
    no_owner.skills_lifecycle.owner_scope = None;
    let tool = m.tool_with(m.policy(AutonomyLevel::Full), no_owner.clone());

    // No configured owner, no trusted scope: the projection is not run.
    let bare = tool
        .execute(json!({"skill": NAME, "tool": "run", "arguments": args}))
        .await;
    let text = bare
        .map(|_| ())
        .expect_err("a projection must not run unpinned")
        .to_string();
    assert!(text.contains("projection"), "{text}");
    // A skill managed under another owner of the same workspace.
    let mut foreign = m.call(&unique("task"), "run", &args);
    foreign["_zc_scope"]["owner_id"] = json!("owner:ws:cli:someone-else");
    assert!(tool.execute(foreign).await.is_err());
    assert!(!marks.join("ran").exists(), "the projection never ran");

    // An invalid configured owner fails closed.
    let mut invalid = no_owner;
    invalid.skills_lifecycle.owner_scope = Some("not a valid owner".to_string());
    let closed = m
        .tool_with(m.policy(AutonomyLevel::Full), invalid)
        .execute(json!({"skill": NAME, "tool": "run", "arguments": args}))
        .await;
    assert!(closed.is_err());
    assert!(!marks.join("ran").exists());
}

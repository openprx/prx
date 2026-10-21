#![cfg(test)]
#![allow(clippy::indexing_slicing)]

//! SL11: reviewed executable bundles. Complete bundle identity (scripts,
//! references and execute mode), execution from the pinned revision with exact
//! hashes, argument validation, tool scope, the operator import with its
//! review, the proposer's inability to change a script, and the projection of
//! an executable bundle. Process cleanup is tested where the process runs, in
//! `tools::skills::executable_tests`.

use super::evaluate::{candidate_bundle, fixture_candidate_unwritten, skill as fixture_skill};
use super::gate::stage_draft;
use super::{entry, exec_entry, owner};
use crate::skills::lifecycle::test_support::publish_revision;
use crate::skills::lifecycle::{
    Bundle, BundleEntry, ExecutableError, ExecutableImport, ExecutionIdentity, LifecycleError, LifecycleMode,
    MigrationAction, NewRevision, OwnerScope, PrepareRequest, PreparedExecution, RevisionId, ServiceError, SkillId,
    SkillLifecycleStore, SkillsLifecycleService, SkillsLifecycleSettings, SourceKind, ToolGrant, apply_migration,
    declared_tools, equivalent_direct_tool, is_executable_bundle, plan_migration, prepare_execution, release_task_pins,
    review_executable_bundle, validate_arguments,
};
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

const RUN_TOOL: &str = "[[tools]]\nname = \"run\"\ndescription = \"run it\"\nkind = \"script\"\ncommand = \"scripts/run.sh\"\n\n[tools.args]\nmessage = \"hello\"\n";
const SHELL_TOOL: &str =
    "[[tools]]\nname = \"run\"\ndescription = \"run it\"\nkind = \"shell\"\ncommand = \"sh scripts/run.sh\"\n";
const NAME: &str = "Report runner";

fn skill_md() -> String {
    format!(
        "---\nname: {NAME}\ndescription: Runs the report\nrequired-tools: [filesystem:native:shell]\n---\n\nRun the report with the run tool.\n"
    )
}

fn skill_toml(tools: &str) -> String {
    format!("[skill]\nname = \"{NAME}\"\ndescription = \"Runs the report\"\nversion = \"1.0.0\"\n\n{tools}")
}

fn entries(script: &[u8], executable: bool, tools: &str) -> Vec<BundleEntry> {
    let script_entry = if executable {
        exec_entry("scripts/run.sh", script)
    } else {
        entry("scripts/run.sh", script)
    };
    vec![
        entry("SKILL.md", skill_md().as_bytes()),
        entry("SKILL.toml", skill_toml(tools).as_bytes()),
        entry("references/guide.md", b"# Guide\n"),
        script_entry,
    ]
}

fn bundle(script: &[u8]) -> Bundle {
    Bundle::from_entries(entries(script, true, RUN_TOOL)).expect("test: bundle")
}

fn unique(tag: &str) -> String {
    format!("{tag}-{}", uuid::Uuid::new_v4().simple())
}

struct Env {
    tmp: TempDir,
    owner: OwnerScope,
    store: SkillLifecycleStore,
    skill: SkillId,
}

fn env(tag: &str) -> Env {
    let tmp = TempDir::new().expect("test: tempdir");
    let owner = owner(&format!("owner:ws:cli:sl11-{}", unique(tag)));
    let store = SkillLifecycleStore::open(tmp.path(), owner.clone()).expect("test: store");
    Env {
        tmp,
        owner,
        store,
        skill: SkillId::parse("report-runner").expect("test: skill id"),
    }
}

impl Env {
    fn publish(&self, bundle: &Bundle, expected_generation: u64) -> RevisionId {
        publish_revision(
            &self.store,
            &self.skill,
            SourceKind::Operator,
            bundle,
            expected_generation,
        )
    }

    fn prepare(
        &self,
        task: Option<&str>,
        tool: &str,
        arguments: Option<&serde_json::Value>,
        grant: &dyn ToolGrant,
    ) -> Result<Option<PreparedExecution>, ExecutableError> {
        prepare_execution(&PrepareRequest {
            workspace_dir: self.tmp.path(),
            owner: &self.owner,
            skill_name: NAME,
            tool_name: tool,
            task_id: task,
            arguments,
            grant,
        })
    }

    fn run_root(&self) -> PathBuf {
        self.tmp.path().join("self/skill-exec")
    }

    fn leftover_runs(&self) -> usize {
        fs::read_dir(self.run_root()).map_or(0, Iterator::count)
    }
}

struct AllowAll;
impl ToolGrant for AllowAll {
    fn permits(&self, _tool: &str) -> Result<(), String> {
        Ok(())
    }
}

struct DenyTool(&'static str);
impl ToolGrant for DenyTool {
    fn permits(&self, tool: &str) -> Result<(), String> {
        if tool == self.0 {
            Err("denied by the test policy".to_string())
        } else {
            Ok(())
        }
    }
}

fn staged_script(prepared: &PreparedExecution) -> String {
    let root = prepared.root().expect("test: a staged run");
    fs::read_to_string(root.join("scripts/run.sh")).expect("test: staged script")
}

// ---- identity (spec section 6.1, matrix "Identity") ------------------------------

#[test]
fn script_reference_and_mode_changes_each_produce_a_new_revision_id() {
    let base = Bundle::from_entries(entries(b"#!/bin/sh\necho A\n", true, RUN_TOOL)).expect("test: base");
    let script = Bundle::from_entries(entries(b"#!/bin/sh\necho B\n", true, RUN_TOOL)).expect("test: script");
    let mode = Bundle::from_entries(entries(b"#!/bin/sh\necho A\n", false, RUN_TOOL)).expect("test: mode");
    let mut with_reference = entries(b"#!/bin/sh\necho A\n", true, RUN_TOOL);
    with_reference.push(entry("references/extra.md", b"more"));
    let reference = Bundle::from_entries(with_reference).expect("test: reference");
    let ids = [
        base.revision_id(),
        script.revision_id(),
        mode.revision_id(),
        reference.revision_id(),
    ];
    for (i, a) in ids.iter().enumerate() {
        for b in ids.iter().skip(i + 1) {
            assert_ne!(a, b);
        }
    }
    // The mode is the only difference between `base` and `mode`.
    assert_eq!(
        base.file("scripts/run.sh").map(|f| f.bytes()),
        mode.file("scripts/run.sh").map(|f| f.bytes())
    );
    assert_ne!(base.revision_id(), mode.revision_id());
}

// ---- the pin: no read-to-execute switch (spec section 9) ---------------------------

#[test]
fn a_task_pinned_to_a_executes_a_script_after_b_is_published() {
    let env = env("pin");
    let a = env.publish(&bundle(b"#!/bin/sh\necho A\n"), 0);
    let task = unique("task");

    let first = env
        .prepare(Some(&task), "run", None, &AllowAll)
        .expect("test: prepare")
        .expect("test: managed");
    assert_eq!(first.pin.revision_id, a);
    assert!(first.pinned);
    assert!(staged_script(&first).contains("echo A"));
    first.finish().expect("test: finish");

    let b = env.publish(&bundle(b"#!/bin/sh\necho B\n"), 1);
    assert_ne!(a, b);

    let again = env
        .prepare(Some(&task), "run", None, &AllowAll)
        .expect("test: prepare")
        .expect("test: managed");
    assert_eq!(again.pin.revision_id, a, "the pinned task still resolves A");
    assert!(staged_script(&again).contains("echo A"));
    again.finish().expect("test: finish");

    let other_task = unique("task");
    let other = env
        .prepare(Some(&other_task), "run", None, &AllowAll)
        .expect("test: prepare")
        .expect("test: managed");
    assert_eq!(other.pin.revision_id, b, "a new task gets the active revision");
    assert!(staged_script(&other).contains("echo B"));
    other.finish().expect("test: finish");

    // A call without a task is not pinned and follows the binding.
    let unpinned = env
        .prepare(None, "run", None, &AllowAll)
        .expect("test: prepare")
        .expect("test: managed");
    assert!(!unpinned.pinned);
    assert_eq!(unpinned.pin.revision_id, b);
    unpinned.finish().expect("test: finish");

    // A task that only read the skill is pinned by the read, and executes it.
    for task in [&task, &other_task] {
        release_task_pins(&env.owner, task);
    }
    assert_eq!(env.leftover_runs(), 0);
}

#[test]
fn a_tool_only_the_newer_manifest_declares_is_undeclared_for_a_pinned_task() {
    let env = env("undeclared");
    env.publish(&bundle(b"#!/bin/sh\necho A\n"), 0);
    let task = unique("task");
    env.prepare(Some(&task), "run", None, &AllowAll)
        .expect("test: pin A")
        .expect("test: managed")
        .finish()
        .expect("test: finish");

    let two_tools = format!(
        "{RUN_TOOL}\n[[tools]]\nname = \"extra\"\ndescription = \"x\"\nkind = \"script\"\ncommand = \"scripts/run.sh\"\n"
    );
    env.publish(
        &Bundle::from_entries(entries(b"#!/bin/sh\necho B\n", true, &two_tools)).expect("test: b"),
        1,
    );

    let refused = env.prepare(Some(&task), "extra", None, &AllowAll);
    assert!(
        matches!(refused, Err(ExecutableError::UndeclaredTool { .. })),
        "{refused:?}"
    );
    let missing = env.prepare(Some(&task), "nope", None, &AllowAll);
    assert!(matches!(missing, Err(ExecutableError::UndeclaredTool { .. })));
    // A task without the pin sees the new tool.
    let fresh = unique("task");
    assert!(env.prepare(Some(&fresh), "extra", None, &AllowAll).is_ok());
    for task in [&task, &fresh] {
        release_task_pins(&env.owner, task);
    }
    assert_eq!(env.leftover_runs(), 0);
}

#[test]
fn a_tampered_or_missing_script_is_refused() {
    let env = env("tamper");
    let a = env.publish(&bundle(b"#!/bin/sh\necho A\n"), 0);
    let task = unique("task");
    env.prepare(Some(&task), "run", None, &AllowAll)
        .expect("test: pin")
        .expect("test: managed")
        .finish()
        .expect("test: finish");

    let stored = env.store.bundle_dir(&env.skill, &a).join("scripts/run.sh");
    fs::set_permissions(&stored, fs::Permissions::from_mode(0o700)).expect("test: chmod");
    fs::write(&stored, b"#!/bin/sh\necho EVIL\n").expect("test: tamper");
    let tampered = env.prepare(Some(&task), "run", None, &AllowAll);
    assert!(
        matches!(
            tampered,
            Err(ExecutableError::Lifecycle(LifecycleError::Tampered { .. }))
        ),
        "{tampered:?}"
    );
    fs::remove_file(&stored).expect("test: remove");
    let missing = env.prepare(Some(&task), "run", None, &AllowAll);
    assert!(missing.is_err(), "a missing script fails explicitly");
    assert_eq!(env.leftover_runs(), 0, "a refused call stages nothing that stays");
    release_task_pins(&env.owner, &task);
}

#[test]
fn a_flipped_execute_mode_in_the_store_is_refused() {
    let env = env("mode-flip");
    let a = env.publish(&bundle(b"#!/bin/sh\necho A\n"), 0);
    let task = unique("task");
    let stored = env.store.bundle_dir(&env.skill, &a).join("scripts/run.sh");
    fs::set_permissions(&stored, fs::Permissions::from_mode(0o600)).expect("test: chmod");
    let refused = env.prepare(Some(&task), "run", None, &AllowAll);
    assert!(refused.is_err(), "{refused:?}");
    release_task_pins(&env.owner, &task);
}

// ---- argument validation -----------------------------------------------------------

#[test]
fn arguments_are_validated_against_the_declared_schema() {
    let tools = declared_tools(&bundle(b"#!/bin/sh\n")).expect("test: tools");
    let tool = tools.first().expect("test: run tool");
    let ok = validate_arguments(tool, Some(&json!({"message": "hi"}))).expect("test: ok");
    assert_eq!(ok.get("message").map(String::as_str), Some("hi"));
    let defaults = validate_arguments(tool, None).expect("test: defaults");
    assert_eq!(defaults.get("message").map(String::as_str), Some("hello"));
    let numeric = validate_arguments(tool, Some(&json!({"message": 7}))).expect("test: number");
    assert_eq!(numeric.get("message").map(String::as_str), Some("7"));

    for bad in [
        json!({"unknown": "x"}),
        json!({"message": {"nested": 1}}),
        json!({"message": ["a"]}),
        json!({"message": "x".repeat(5000)}),
        json!({"message": "a\u{0}b"}),
        json!("not an object"),
        json!(["message"]),
    ] {
        assert!(
            matches!(
                validate_arguments(tool, Some(&bad)),
                Err(ExecutableError::InvalidArguments(_))
            ),
            "{bad}"
        );
    }
    let env = env("args");
    env.publish(&bundle(b"#!/bin/sh\n"), 0);
    let refused = env.prepare(None, "run", Some(&json!({"unknown": "x"})), &AllowAll);
    assert!(matches!(refused, Err(ExecutableError::InvalidArguments(_))));
    assert_eq!(
        env.leftover_runs(),
        0,
        "invalid arguments are refused before anything is staged"
    );
}

// ---- tool scope: a skill cannot grant its own access -----------------------------

#[test]
fn a_tool_the_policy_does_not_grant_is_refused_whatever_the_manifest_declares() {
    let env = env("grant");
    env.publish(&bundle(b"#!/bin/sh\necho A\n"), 0);
    // The kind's direct tool (shell for a script) must be granted.
    let shell = env.prepare(None, "run", None, &DenyTool("shell"));
    assert!(
        matches!(&shell, Err(ExecutableError::NotGranted { tool, .. }) if tool == "shell"),
        "{shell:?}"
    );
    // The manifest declares `required-tools: [filesystem:native:shell]`; that is a
    // request, never a grant: the policy refusing the same tool refuses the call
    // (the declared capability maps to the tool `shell`).
    assert_eq!(equivalent_direct_tool("script"), "shell");
    assert_eq!(equivalent_direct_tool("http"), "http_request");
    assert_eq!(env.leftover_runs(), 0, "an ungranted call stages nothing");
}

#[test]
fn every_capability_a_revision_requires_must_be_granted() {
    let env = env("capability");
    let md = "---\nname: Report runner\ndescription: Runs the report\nrequired-tools: [native_tool:native:deploy_check]\n---\n\nBody.\n";
    let mut list = entries(b"#!/bin/sh\necho A\n", true, RUN_TOOL);
    list[0] = entry("SKILL.md", md.as_bytes());
    env.publish(&Bundle::from_entries(list).expect("test: bundle"), 0);
    let refused = env.prepare(None, "run", None, &DenyTool("deploy_check"));
    assert!(
        matches!(&refused, Err(ExecutableError::NotGranted { tool, .. }) if tool == "deploy_check"),
        "{refused:?}"
    );
    assert!(env.prepare(None, "run", None, &AllowAll).is_ok());
}

#[test]
fn an_http_tool_is_held_to_the_direct_http_tool_and_runs_no_local_file() {
    let env = env("http");
    let tools = "[[tools]]\nname = \"fetch\"\ndescription = \"f\"\nkind = \"http\"\ncommand = \"https://example.test/report\"\n";
    env.publish(
        &Bundle::from_entries(vec![
            entry("SKILL.md", skill_md().as_bytes()),
            entry("SKILL.toml", skill_toml(tools).as_bytes()),
        ])
        .expect("test: bundle"),
        0,
    );
    let refused = env.prepare(None, "fetch", None, &DenyTool("http_request"));
    assert!(
        matches!(&refused, Err(ExecutableError::NotGranted { tool, .. }) if tool == "http_request"),
        "{refused:?}"
    );
    let prepared = env
        .prepare(None, "fetch", None, &AllowAll)
        .expect("test: prepare")
        .expect("test: managed");
    assert!(prepared.root().is_none(), "an http tool stages no run directory");
    assert_eq!(env.leftover_runs(), 0);
}

// ---- execution identity (Harness2 b) -------------------------------------------------

#[test]
fn the_execution_key_covers_bundle_tool_settings_environment_and_policy() {
    let a = bundle(b"#!/bin/sh\necho A\n");
    let tools = declared_tools(&a).expect("test: tools");
    let tool = tools.first().expect("test: tool");
    let files = |script: &[u8]| {
        vec![crate::skills::lifecycle::FileDigest {
            path: "scripts/run.sh".to_string(),
            sha256: hex::encode(<sha2::Sha256 as sha2::Digest>::digest(script)),
        }]
    };
    let base_files = files(b"A");
    let key = |revision: &RevisionId,
               tool: &crate::skills::SkillTool,
               files: &[crate::skills::lifecycle::FileDigest],
               settings: &str,
               environment: &str,
               policy: &str| {
        ExecutionIdentity {
            revision_id: revision,
            tool,
            files,
            settings,
            environment,
            policy,
        }
        .digest()
    };
    let same = key(a.revision_id(), tool, &base_files, "s", "e", "p");
    assert_eq!(same, key(a.revision_id(), tool, &base_files, "s", "e", "p"));
    let b = bundle(b"#!/bin/sh\necho B\n");
    let mut other_tool = tool.clone();
    other_tool.command = "scripts/other.sh".to_string();
    let mut other_schema = tool.clone();
    other_schema.args.insert("extra".to_string(), String::new());
    let variants = [
        key(b.revision_id(), tool, &base_files, "s", "e", "p"),
        key(a.revision_id(), &other_tool, &base_files, "s", "e", "p"),
        key(a.revision_id(), &other_schema, &base_files, "s", "e", "p"),
        key(a.revision_id(), tool, &files(b"B"), "s", "e", "p"),
        key(a.revision_id(), tool, &base_files, "s2", "e", "p"),
        key(a.revision_id(), tool, &base_files, "s", "e2", "p"),
        key(a.revision_id(), tool, &base_files, "s", "e", "p2"),
    ];
    for variant in &variants {
        assert_ne!(variant, &same);
    }
}

// ---- the staged run ------------------------------------------------------------------

#[test]
fn a_staged_run_is_a_private_exact_copy_removed_by_finish_or_drop() {
    let env = env("stage");
    let b = bundle(b"#!/bin/sh\necho A\n");
    env.publish(&b, 0);
    let prepared = env
        .prepare(None, "run", None, &AllowAll)
        .expect("test: prepare")
        .expect("test: managed");
    let root = prepared.root().expect("test: root").to_path_buf();
    assert_eq!(
        Bundle::from_dir(&root).expect("test: staged bundle").revision_id(),
        b.revision_id(),
        "the staged tree hashes to the pinned revision, modes included"
    );
    let mode = |relative: &str| {
        fs::metadata(root.join(relative))
            .expect("test: meta")
            .permissions()
            .mode()
    };
    assert_ne!(mode("scripts/run.sh") & 0o100, 0);
    assert_eq!(mode("scripts/run.sh") & 0o077, 0, "private");
    assert_eq!(mode("references/guide.md") & 0o111, 0);
    assert_eq!(fs::metadata(&root).expect("test: meta").permissions().mode() & 0o077, 0);
    prepared.finish().expect("test: finish");
    assert!(!root.exists());

    let dropped = env
        .prepare(None, "run", None, &AllowAll)
        .expect("test: prepare")
        .expect("test: managed");
    let dropped_root = dropped.root().expect("test: root").to_path_buf();
    assert!(dropped_root.exists());
    drop(dropped);
    assert!(!dropped_root.exists(), "an abandoned call leaves nothing behind");
    assert_eq!(env.leftover_runs(), 0);
}

#[test]
fn a_stale_run_directory_of_a_dead_process_is_swept_by_the_next_preparation() {
    let env = env("sweep");
    env.publish(&bundle(b"#!/bin/sh\necho A\n"), 0);
    let stale = env.run_root().join("run-deadbeef");
    fs::create_dir_all(&stale).expect("test: stale dir");
    fs::write(stale.join("x"), b"x").expect("test: file");
    let old = std::time::SystemTime::now() - std::time::Duration::from_hours(48);
    fs::File::open(&stale)
        .and_then(|dir| dir.set_modified(old))
        .expect("test: age it");
    env.prepare(None, "run", None, &AllowAll)
        .expect("test: prepare")
        .expect("test: managed")
        .finish()
        .expect("test: finish");
    assert!(!stale.exists());
}

// ---- review of an executable bundle (Harness2 a) ---------------------------------------

fn review(list: Vec<BundleEntry>) -> Result<(), ExecutableError> {
    review_executable_bundle(&Bundle::from_entries(list).expect("test: bundle")).map(|_| ())
}

#[test]
fn the_review_refuses_malformed_or_unreferenced_executable_content() {
    assert!(review(entries(b"#!/bin/sh\n", true, RUN_TOOL)).is_ok());
    let report = review_executable_bundle(&bundle(b"#!/bin/sh\necho A\n")).expect("test: review");
    assert_eq!(report.tools, vec!["run".to_string()]);
    assert_eq!(report.executables.len(), 1);

    let tool = |name: &str, kind: &str, command: &str| {
        format!("[[tools]]\nname = \"{name}\"\ndescription = \"d\"\nkind = \"{kind}\"\ncommand = \"{command}\"\n")
    };
    // A script outside the bundle, absolute, missing, or not executable.
    for command in ["../escape.sh", "/bin/sh", "scripts/missing.sh", "scripts/../x"] {
        assert!(
            matches!(
                review(entries(b"#!/bin/sh\n", true, &tool("run", "script", command))),
                Err(ExecutableError::InvalidTool { .. })
            ),
            "{command}"
        );
    }
    assert!(matches!(
        review(entries(b"#!/bin/sh\n", false, RUN_TOOL)),
        Err(ExecutableError::InvalidTool { .. })
    ));
    // A shell tool plus an executable nothing runs.
    assert!(matches!(
        review(entries(b"#!/bin/sh\n", true, &tool("run", "shell", "echo hi"))),
        Err(ExecutableError::UnreferencedExecutable { .. })
    ));
    // Duplicate names (case-insensitive), unknown kinds, plain-text URLs, bad names.
    let twice = format!(
        "{}\n{}",
        tool("run", "script", "scripts/run.sh"),
        tool("RUN", "script", "scripts/run.sh")
    );
    assert!(matches!(
        review(entries(b"#!/bin/sh\n", true, &twice)),
        Err(ExecutableError::InvalidTool { .. })
    ));
    for bad in [
        tool("run", "wasm", "scripts/run.sh"),
        tool("run", "http", "ftp://example.test"),
        tool("bad name", "script", "scripts/run.sh"),
    ] {
        assert!(matches!(
            review(entries(b"#!/bin/sh\n", true, &bad)),
            Err(ExecutableError::InvalidTool { .. })
        ));
    }
    // Instruction-only bundles are not executable bundles.
    let plain = vec![entry("SKILL.md", skill_md().as_bytes())];
    assert!(!is_executable_bundle(
        &Bundle::from_entries(plain.clone()).expect("test: plain")
    ));
    assert!(matches!(review(plain), Err(ExecutableError::NotExecutable)));
    // The untrusted-origin marker cannot be reviewed away.
    let mut marked = entries(b"#!/bin/sh\n", true, RUN_TOOL);
    marked.push(entry(".openprx-untrusted-origin.json", b"{}"));
    assert!(matches!(review(marked), Err(ExecutableError::UntrustedOrigin)));
    // SKILL.toml and SKILL.md must agree.
    let mut conflict = entries(b"#!/bin/sh\n", true, RUN_TOOL);
    conflict[1] = entry(
        "SKILL.toml",
        skill_toml(RUN_TOOL).replace(NAME, "Other name").as_bytes(),
    );
    assert!(review(conflict).is_err());
}

// ---- operator import (spec: reviewed artifact, grant-bound at the tool) ---------------

fn write_tree(root: &Path, list: &[BundleEntry]) {
    for file in list {
        let target = root.join(&file.path);
        fs::create_dir_all(target.parent().expect("test: parent")).expect("test: dirs");
        fs::write(&target, &file.bytes).expect("test: file");
        let mode = if file.executable { 0o755 } else { 0o644 };
        fs::set_permissions(&target, fs::Permissions::from_mode(mode)).expect("test: chmod");
    }
}

fn service(env: &Env, mode: LifecycleMode) -> SkillsLifecycleService {
    let store = SkillLifecycleStore::open(env.tmp.path(), env.owner.clone()).expect("test: store");
    SkillsLifecycleService::new(
        Arc::new(store),
        SkillsLifecycleSettings {
            enabled: true,
            mode,
            ..SkillsLifecycleSettings::default()
        },
    )
    .expect("test: service")
}

fn import(env: &Env, svc: &SkillsLifecycleService, list: &[BundleEntry]) -> Result<ExecutableImport, ServiceError> {
    let source = env.tmp.path().join("incoming/report-runner");
    let _ = fs::remove_dir_all(&source);
    write_tree(&source, list);
    let reviewed = Bundle::from_dir(&source)
        .expect("test: reviewed bundle")
        .revision_id()
        .clone();
    svc.import_executable(&source, &reviewed)
}

#[test]
fn an_operator_import_publishes_a_reviewed_executable_and_revises_it_by_hash() {
    let env = env("import");
    let svc = service(&env, LifecycleMode::Auto);
    let first = import(&env, &svc, &entries(b"#!/bin/sh\necho A\n", true, SHELL_TOOL)).expect("test: import");
    assert_eq!(first.generation, 1);
    assert_eq!(first.replaced, None);
    let record = env
        .store
        .read_revision(&first.skill_id, &first.revision_id, None)
        .expect("test: revision")
        .revision;
    assert_eq!(record.source_kind, SourceKind::Operator);
    assert_eq!(record.review_receipt_ids.len(), 1);
    assert!(
        record
            .review_receipt_ids
            .iter()
            .all(|id| id.starts_with("executable-review-"))
    );
    assert_eq!(
        record.capability_requirements,
        vec!["filesystem:native:shell".to_string()]
    );

    // The projection carries the script with its exact mode (modes are in the hash).
    let projection = env.tmp.path().join("skills").join(first.skill_id.as_str());
    assert_eq!(
        Bundle::from_dir(&projection).expect("test: projection").revision_id(),
        &first.revision_id
    );
    let script_mode = fs::metadata(projection.join("scripts/run.sh"))
        .expect("test: meta")
        .permissions()
        .mode();
    assert_ne!(script_mode & 0o100, 0, "the projected script is executable");

    // A mode-only change is a new revision, published from the current generation.
    let revised = import(&env, &svc, &entries(b"#!/bin/sh\necho A\n", false, SHELL_TOOL)).expect("test: mode only");
    assert_ne!(revised.revision_id, first.revision_id);
    assert_eq!(revised.skill_id, first.skill_id);
    assert_eq!(revised.generation, 2);
    assert_eq!(revised.replaced, Some(first.revision_id));
    let projected = fs::metadata(projection.join("scripts/run.sh"))
        .expect("test: meta")
        .permissions()
        .mode();
    assert_eq!(projected & 0o111, 0, "the projection followed the mode change");

    // A directory that does not hash to the reviewed revision is refused.
    let source = env.tmp.path().join("incoming/report-runner");
    let other = RevisionId::parse(&format!("r1-{}", "0".repeat(64))).expect("test: id");
    let refused = svc.import_executable(&source, &other);
    assert!(
        matches!(
            refused,
            Err(ServiceError::Executable(ExecutableError::NotReviewed { .. }))
        ),
        "{refused:?}"
    );
    // An instruction-only directory is not an executable import.
    let plain = import(&env, &svc, &[entry("SKILL.md", skill_md().as_bytes())]);
    assert!(
        matches!(plain, Err(ServiceError::Executable(ExecutableError::NotExecutable))),
        "{plain:?}"
    );
}

#[test]
fn an_executable_import_follows_the_publication_gate_and_the_bundle_rules() {
    let env = env("import-gate");
    let list = entries(b"#!/bin/sh\necho A\n", true, RUN_TOOL);
    for mode in [LifecycleMode::DraftOnly, LifecycleMode::Shadow] {
        let svc = service(&env, mode);
        let refused = import(&env, &svc, &list);
        assert!(
            matches!(refused, Err(ServiceError::ModeForbids { .. })),
            "{mode}: {refused:?}"
        );
    }
    assert_eq!(env.store.list_active_bindings().expect("test: bindings").len(), 0);

    let svc = service(&env, LifecycleMode::Auto);
    // A symlink inside the directory, an oversized file and a non-UTF-8 name are
    // refused by the bundle rules before any review.
    let source = env.tmp.path().join("incoming/report-runner");
    write_tree(&source, &list);
    std::os::unix::fs::symlink("/etc/passwd", source.join("references/link")).expect("test: symlink");
    let reviewed = RevisionId::parse(&format!("r1-{}", "1".repeat(64))).expect("test: id");
    let linked = svc.import_executable(&source, &reviewed);
    assert!(
        matches!(linked, Err(ServiceError::Lifecycle(LifecycleError::Bundle(_)))),
        "{linked:?}"
    );
    fs::remove_file(source.join("references/link")).expect("test: unlink");
    fs::write(source.join("references/big.bin"), vec![0_u8; 1024 * 1024 + 1]).expect("test: big");
    assert!(matches!(
        svc.import_executable(&source, &reviewed),
        Err(ServiceError::Lifecycle(LifecycleError::Bundle(_)))
    ));
    fs::remove_file(source.join("references/big.bin")).expect("test: rm");
    assert_eq!(env.store.list_active_bindings().expect("test: bindings").len(), 0);
}

#[test]
fn migration_plans_a_legacy_executable_for_review_and_never_imports_it_unreviewed() {
    let env = env("migrate");
    let legacy = env.tmp.path().join("skills/report-runner");
    write_tree(&legacy, &entries(b"#!/bin/sh\necho legacy\n", true, RUN_TOOL));
    let plain = env.tmp.path().join("skills/plain");
    write_tree(
        &plain,
        &[entry(
            "SKILL.md",
            "---\nname: Plain\ndescription: Plain skill\n---\n\nBody.\n".as_bytes(),
        )],
    );
    let plan = plan_migration(env.tmp.path(), Some(&env.store)).expect("test: plan");
    let action = |dir: &str| {
        plan.entries
            .iter()
            .find(|entry| entry.directory == dir)
            .map(|entry| entry.action.clone())
            .expect("test: planned")
    };
    let reviewed = match action("report-runner") {
        MigrationAction::ExecutableNeedsReview { revision_id, .. } => Some(revision_id),
        _ => None,
    }
    .expect("test: a legacy executable needs review");
    assert!(matches!(action("plain"), MigrationAction::Import { .. }));

    let report = apply_migration(&env.store, env.tmp.path(), &plan);
    assert_eq!(report.imported.len(), 1, "only the instruction skill is imported");
    assert_eq!(report.failed.len(), 0);
    let legacy_id = SkillId::from_legacy_dir("report-runner").expect("test: id");
    assert!(env.store.active_binding(&legacy_id).expect("test: binding").is_none());

    // The operator reviews exactly that hash and imports it.
    let svc = service(&env, LifecycleMode::Auto);
    let imported = svc.import_executable(&legacy, &reviewed).expect("test: import");
    assert_eq!(imported.skill_id, legacy_id);
    assert_eq!(imported.revision_id, reviewed);
    assert_eq!(
        Bundle::from_dir(&legacy).expect("test: projection").revision_id(),
        &reviewed,
        "the legacy directory now is the projection of the revision, mode included"
    );
}

// ---- the proposer cannot change scripts, references or modes ----------------------------

#[test]
fn a_revision_of_an_executable_bundle_may_change_skill_md_text_only() {
    let (fx, _) = fixture_candidate_unwritten();
    let skill = fixture_skill();
    let md = |body: &str| {
        format!("---\nname: release-notes\ndescription: Writes release notes\n---\n\n{body}\n").into_bytes()
    };
    let toml = |tools: &str| {
        format!(
            "[skill]\nname = \"release-notes\"\ndescription = \"Writes release notes\"\nversion = \"1.0.0\"\n\n{tools}"
        )
        .into_bytes()
    };
    let run_tool = "[[tools]]\nname = \"run\"\ndescription = \"r\"\nkind = \"script\"\ncommand = \"scripts/run.sh\"\n";
    let list = |body: &str, script: &[u8], executable: bool, reference: &[u8], tools: &str, extra: bool| {
        let mut files = vec![
            BundleEntry {
                path: "SKILL.md".into(),
                bytes: md(body),
                executable: false,
            },
            BundleEntry {
                path: "SKILL.toml".into(),
                bytes: toml(tools),
                executable: false,
            },
            BundleEntry {
                path: "references/guide.md".into(),
                bytes: reference.to_vec(),
                executable: false,
            },
            BundleEntry {
                path: "scripts/run.sh".into(),
                bytes: script.to_vec(),
                executable,
            },
        ];
        if extra {
            files.push(BundleEntry {
                path: "scripts/extra.sh".into(),
                bytes: b"#!/bin/sh\n".to_vec(),
                executable: true,
            });
        }
        Bundle::from_entries(files).expect("test: bundle")
    };
    let write = |bundle: &Bundle, kind: SourceKind| {
        fx.store
            .write_revision(&NewRevision::new(skill.clone(), kind), bundle)
            .expect("test: write")
            .revision
            .revision_id
    };
    let base = write(
        &list("v1", b"#!/bin/sh\necho A\n", true, b"# G\n", run_tool, false),
        SourceKind::Operator,
    );
    let mut draft = stage_draft(
        &fx,
        "req-exec",
        &candidate_bundle("ANSWER=report-v2"),
        "ANSWER=report-v2",
    );
    draft.baseline_revision = Some(base);
    let mut check = |candidate: Bundle| {
        draft.candidate_revision = Some(write(&candidate, SourceKind::Learned));
        fx.store.check_capabilities(&draft, None)
    };
    // Only SKILL.md text differs: accepted.
    assert!(
        check(list(
            "v2 with new steps",
            b"#!/bin/sh\necho A\n",
            true,
            b"# G\n",
            run_tool,
            false
        ))
        .is_ok()
    );
    for (what, candidate) in [
        (
            "script bytes",
            list("v1", b"#!/bin/sh\necho EVIL\n", true, b"# G\n", run_tool, false),
        ),
        (
            "execute mode",
            list("v1", b"#!/bin/sh\necho A\n", false, b"# G\n", run_tool, false),
        ),
        (
            "reference bytes",
            list("v1", b"#!/bin/sh\necho A\n", true, b"# changed\n", run_tool, false),
        ),
        (
            "tool declaration",
            list("v1", b"#!/bin/sh\necho A\n", true, b"# G\n", "", false),
        ),
        (
            "an added script",
            list("v1", b"#!/bin/sh\necho A\n", true, b"# G\n", run_tool, true),
        ),
    ] {
        let refused = check(candidate);
        assert!(
            matches!(refused, Err(LifecycleError::CapabilityExpansion { .. })),
            "{what}: {refused:?}"
        );
    }
}

// ---- evaluation excludes non-replayable external effects (Harness2 d) --------------------

/// A frozen suite can offer a trial only read tools and, when declared,
/// workspace file writes. Anything that runs a skill script or reaches the
/// outside world (mail, payments through a connector, production writes, a
/// shell, HTTP, a node) cannot be required, so an executable revision under
/// evaluation never has its scripts run in a trial. No resettable simulator
/// exists, so none is admitted.
#[test]
fn a_trial_cannot_be_offered_a_tool_with_non_replayable_external_effects() {
    use crate::skills::lifecycle::{EvalSplit, SideEffects};
    let build = |tool: &str| {
        let mut suite = super::evaluate::suite_with(vec![
            super::evaluate::case("a", EvalSplit::Validation, "prompt one"),
            super::evaluate::case("b", EvalSplit::Validation, "prompt two"),
        ]);
        suite.required_tools = vec![tool.to_string()];
        suite.side_effects = SideEffects::WorkspaceWrite;
        suite.freeze()
    };
    for tool in [
        "skill_execute",
        "shell",
        "http_request",
        "message_send",
        "pushover",
        "composio",
        "nodes",
        "sessions_send",
        "mcp_call",
        "cron",
    ] {
        assert!(build(tool).is_err(), "{tool} must not be offered to a trial");
    }
    assert!(build("file_write").is_ok());
}

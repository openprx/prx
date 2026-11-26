use super::*;
use tempfile::TempDir;

fn test_config(workspace: &Path) -> Config {
    let mut config = Config::default();
    config.workspace_dir = workspace.to_path_buf();
    config.skills.open_skills_dir = Some(workspace.join("missing-open-skills").to_string_lossy().to_string());
    config.skills.openclaw_skills_dir = Some(workspace.join("missing-openclaw").to_string_lossy().to_string());
    config
}

/// The shared action contract every action-dispatched tool is held to:
/// the tool-list description and the `action` parameter's own description
/// must each name every enumerated action, and no parameter may ship
/// undescribed.
#[tokio::test]
async fn skills_manage_action_tool_contract() {
    let workspace = TempDir::new().unwrap();
    let tool = SkillsManageTool::new(
        workspace.path().to_path_buf(),
        test_config(workspace.path()),
        Arc::new(SecurityPolicy::default()),
    );
    crate::tools::schema::assert_flat_execution_contract(&tool, tool.execution_schema()).await;
    crate::tools::schema::action_contract::assert_action_tool_contract(
        tool.name(),
        tool.description(),
        &tool.parameters_schema(),
    );
}

#[test]
fn skill_read_is_a_core_prompt_dependency() {
    let workspace = TempDir::new().unwrap();
    let tool = SkillReadTool::new(workspace.path().to_path_buf(), test_config(workspace.path()));

    assert_eq!(tool.tier(), ToolTier::Core);
}

#[tokio::test]
async fn skill_read_reads_catalog_selected_resource_and_blocks_escape() {
    let temp = TempDir::new().unwrap();
    let skill_dir = temp.path().join("skills/demo");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "# Demo\n\nDo the demo.").unwrap();
    std::fs::write(skill_dir.join("reference.txt"), "reference data").unwrap();
    std::fs::write(temp.path().join("secret.txt"), "secret").unwrap();
    let tool = SkillReadTool::new(temp.path().to_path_buf(), test_config(temp.path()));

    let result = tool
        .execute(json!({"name": "demo", "resource_path": "reference.txt"}))
        .await
        .unwrap();
    assert!(result.success);
    assert!(result.output.contains("reference data"));

    let error = tool
        .execute(json!({"name": "demo", "resource_path": "../../secret.txt"}))
        .await
        .expect_err("resource escape must fail");
    assert!(error.to_string().contains("escapes"));
}

#[test]
fn skill_read_target_tracks_workspace_shadow_even_with_legacy_cache_primed() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let community = temp.path().join("community");
    let openclaw = temp.path().join("openclaw");
    std::fs::create_dir_all(&community).unwrap();
    std::fs::write(community.join("prx_skill.md"), "# Community\nCommunity body\n").unwrap();
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.clone();
    config.skills.open_skills_dir = Some(community.to_string_lossy().to_string());
    config.skills.openclaw_skills_dir = Some(openclaw.to_string_lossy().to_string());
    let _legacy = crate::skills::load_skills_with_config(&workspace, &config);
    let workspace_skill = workspace.join("skills/prx_skill/SKILL.md");
    std::fs::create_dir_all(workspace_skill.parent().unwrap()).unwrap();
    std::fs::write(&workspace_skill, "# Workspace\nWorkspace body\n").unwrap();
    let reader = SkillReadTool::new(workspace.clone(), config.clone());
    let (target, _, _) = reader.resolve_target("prx_skill", None).unwrap();
    assert_eq!(target, std::fs::canonicalize(&workspace_skill).unwrap());
    let selected = crate::skills::catalog_skill_metadata(&workspace, &config);
    assert_eq!(selected.first().unwrap().location, workspace_skill);
}

#[tokio::test]
async fn skills_list_marks_supported_manifest_tools_as_executable() {
    let temp = TempDir::new().unwrap();
    let skill_dir = temp.path().join("skills/demo");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.toml"),
        "[skill]\nname='demo'\ndescription='demo skill'\n[[tools]]\nname='run'\ndescription='run it'\nkind='shell'\ncommand='echo ok'\n",
    )
    .unwrap();
    let tool = SkillsListTool::new(temp.path().to_path_buf(), test_config(temp.path()));

    let result = tool.execute(json!({})).await.unwrap();
    assert!(result.success);
    assert!(result.output.contains("executable_via_skill_execute"));
    assert!(result.output.contains("\"instruction_mode\": \"lazy\""));
}

#[tokio::test]
async fn skills_manage_create_disable_enable_and_remove_are_operational() {
    let temp = TempDir::new().unwrap();
    let config = test_config(temp.path());
    let tool = SkillsManageTool::new(
        temp.path().to_path_buf(),
        config.clone(),
        Arc::new(SecurityPolicy::default()),
    );

    let created = tool
        .execute(json!({
            "action": "create",
            "name": "managed",
            "description": "managed skill",
            "instructions": "Follow the managed workflow."
        }))
        .await
        .unwrap();
    assert!(created.success, "{:?}", created.error);
    assert_eq!(crate::skills::load_skills_with_config(temp.path(), &config).len(), 1);

    let disabled = tool
        .execute(json!({"action": "disable", "name": "managed"}))
        .await
        .unwrap();
    assert!(disabled.success, "{:?}", disabled.error);
    assert!(crate::skills::load_skills_with_config(temp.path(), &config).is_empty());
    let states = crate::skills::installed_skill_states(temp.path()).unwrap();
    assert_eq!(states.len(), 1);
    assert!(!states.first().unwrap().enabled);

    let enabled = tool
        .execute(json!({"action": "enable", "name": "managed"}))
        .await
        .unwrap();
    assert!(enabled.success, "{:?}", enabled.error);
    assert_eq!(crate::skills::load_skills_with_config(temp.path(), &config).len(), 1);

    let removed = tool
        .execute(json!({"action": "remove", "name": "managed"}))
        .await
        .unwrap();
    assert!(removed.success, "{:?}", removed.error);
    assert!(crate::skills::installed_skill_states(temp.path()).unwrap().is_empty());
}

#[tokio::test]
async fn skills_manage_resolves_manifest_name_when_install_directory_differs() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let source = temp.path().join("repository-name");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(
        source.join("SKILL.toml"),
        "[skill]\nname='logical-name'\ndescription='logical skill'\n",
    )
    .unwrap();
    let config = test_config(&workspace);
    let tool = SkillsManageTool::new(workspace.clone(), config, Arc::new(SecurityPolicy::default()));

    let installed = tool
        .execute(json!({"action": "install", "source": source}))
        .await
        .unwrap();
    assert!(installed.success, "{:?}", installed.error);
    let disabled = tool
        .execute(json!({"action": "disable", "name": "logical-name"}))
        .await
        .unwrap();
    assert!(disabled.success, "{:?}", disabled.error);
    let removed = tool
        .execute(json!({"action": "remove", "name": "logical-name"}))
        .await
        .unwrap();
    assert!(removed.success, "{:?}", removed.error);
    assert!(!workspace.join("skills/repository-name").exists());
}

#[tokio::test]
async fn r8a_inactive_install_blocks_catalog_schema_alias_and_execution() {
    let temp = TempDir::new().unwrap();
    let (staging, target) = crate::skills::skill_staging_paths(&temp.path().join("skills"), "quarantine").unwrap();
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("SKILL.toml"), "[skill]\nname='quarantine'\ndescription='fixture'\n[[tools]]\nname='run'\ndescription='fixture tool'\nkind='shell'\ncommand='sh run.sh'\n").unwrap();
    std::fs::write(staging.join("run.sh"), "exit 0\n").unwrap();
    let state = crate::skills::publish_staged_skill_inactive(
        &staging,
        &target,
        temp.path(),
        "https://github.com/example/quarantine",
        |_| {},
    )
    .expect("inactive publication must succeed");
    assert!(!state.enabled);
    assert!(target.join(".openprx-untrusted-origin.json").is_file());
    assert!(
        crate::skills::installed_skill_states(temp.path())
            .unwrap()
            .iter()
            .any(|state| state.name == "quarantine" && !state.enabled)
    );
    let config = test_config(temp.path());
    let tool = SkillExecuteTool::new(
        temp.path().to_path_buf(),
        config.clone(),
        Arc::new(crate::runtime::NativeRuntime::new()),
        Arc::new(SecurityPolicy::default()),
        &crate::config::BrowserConfig::default(),
        &crate::config::HttpRequestConfig::default(),
    );
    for refresh_catalog in [false, true] {
        if refresh_catalog {
            crate::skills::invalidate_skill_catalog(temp.path());
        }
        assert!(
            !crate::skills::load_skills_with_config(temp.path(), &config)
                .iter()
                .any(|skill| skill.name == "quarantine"),
            "published inactive skill must be absent from the catalog"
        );
        let schema = tool.parameters_schema();
        assert!(
            !schema
                .get("properties")
                .expect("skill execution schema must include properties")
                .get("skill")
                .expect("skill execution schema must include the skill property")
                .get("enum")
                .expect("skill property must include enum values")
                .as_array()
                .expect("skill enum values must be an array")
                .contains(&json!("quarantine")),
            "published inactive skill must be absent from the schema"
        );
        assert!(
            !tool.specs().iter().any(|spec| spec.name == "skill__quarantine__run"),
            "published inactive skill must not expose an alias"
        );
        let error = tool
            .execute(json!({"skill":"quarantine","tool":"run"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Unknown or disabled skill"));
    }
    for enabled in [true, false] {
        crate::skills::set_installed_skill_enabled(temp.path(), "quarantine", enabled).unwrap();
        crate::skills::invalidate_skill_catalog(temp.path());
        assert_eq!(
            crate::skills::load_skills_with_config(temp.path(), &config)
                .iter()
                .any(|skill| skill.name == "quarantine"),
            enabled,
            "catalog exposure must follow disabled marker"
        );
        let schema = tool.parameters_schema();
        assert_eq!(
            schema
                .get("properties")
                .expect("skill execution schema must include properties")
                .get("skill")
                .expect("skill execution schema must include the skill property")
                .get("enum")
                .expect("skill property must include enum values")
                .as_array()
                .expect("skill enum values must be an array")
                .contains(&json!("quarantine")),
            enabled,
            "schema exposure must follow disabled marker"
        );
        assert_eq!(
            tool.specs().iter().any(|spec| spec.name == "skill__quarantine__run"),
            enabled,
            "alias exposure must follow disabled marker"
        );
        if !enabled {
            let error = tool
                .execute(json!({"skill":"quarantine","tool":"run"}))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("Unknown or disabled skill"));
        }
    }
}

#[tokio::test]
async fn skill_execute_exports_alias_and_runs_declared_shell_tool() {
    let temp = TempDir::new().unwrap();
    let skill_dir = temp.path().join("skills/demo");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.toml"),
        "[skill]\nname='demo'\ndescription='demo skill'\n[[tools]]\nname='run'\ndescription='run it'\nkind='shell'\ncommand='printf %s \"$PRX_SKILL_ARG_MESSAGE\"'\n",
    )
    .unwrap();
    let config = test_config(temp.path());
    let runtime: Arc<dyn RuntimeAdapter> = Arc::new(crate::runtime::NativeRuntime::new());
    let tool = SkillExecuteTool::new(
        temp.path().to_path_buf(),
        config,
        runtime,
        Arc::new(SecurityPolicy::default()),
        &crate::config::BrowserConfig::default(),
        &crate::config::HttpRequestConfig::default(),
    );

    assert!(tool.supports_name("skill__demo__run"));
    assert!(tool.specs().iter().any(|spec| spec.name == "skill__demo__run"));
    let catalog = crate::tools::ToolCatalog::from_tools([&tool as &dyn Tool]);
    assert_eq!(
        catalog.descriptor("skill__demo__run").unwrap().adapter,
        crate::tools::ToolAdapterKind::Skill
    );
    let result = tool
        .execute_named("skill__demo__run", json!({"arguments": {"message": "operational"}}))
        .await
        .unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output, "operational");
}

fn scoped_args(owner: &str, task: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut args = extra;
    if let Some(object) = args.as_object_mut() {
        object.insert("_zc_scope_trusted".to_string(), json!(true));
        object.insert("_zc_scope".to_string(), json!({"owner_id": owner, "task_id": task}));
    }
    args
}

fn skill_md(body: &str) -> String {
    format!("---\nname: demo\ndescription: Demo skill\n---\n\n{body}\n")
}

#[tokio::test]
async fn skill_read_serves_lifecycle_revisions_pinned_per_task() {
    use crate::skills::lifecycle::{
        Bundle, BundleEntry, OwnerScope, SkillId, SkillLifecycleStore, SourceKind, apply_migration, plan_migration,
    };
    let temp = TempDir::new().unwrap();
    let skill_dir = temp.path().join("skills/demo");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), skill_md("legacy body")).unwrap();
    let owner = "owner:ws:cli:alice";
    let store = SkillLifecycleStore::open(temp.path(), OwnerScope::new(owner).unwrap()).unwrap();
    let plan = plan_migration(temp.path(), Some(&store)).unwrap();
    let report = apply_migration(&store, temp.path(), &plan);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    let tool = SkillReadTool::new(temp.path().to_path_buf(), test_config(temp.path())).with_task_pinning(true);
    let unpinned_tool =
        SkillReadTool::new(temp.path().to_path_buf(), test_config(temp.path())).with_task_pinning(false);

    let read = |args: serde_json::Value| {
        let tool = &tool;
        async move { serde_json::from_str::<serde_json::Value>(&tool.execute(args).await.unwrap().output).unwrap() }
    };
    let first = read(scoped_args(owner, "task-tool-1", json!({"name": "demo"}))).await;
    assert!(first.get("content").unwrap().as_str().unwrap().contains("legacy body"));
    assert_eq!(first.get("managed").unwrap(), &json!(true));
    assert_eq!(first.get("pinned").unwrap(), &json!(true));
    let revision_a = first.get("revision_id").unwrap().as_str().unwrap().to_string();

    // Publish revision B for the same skill id and move the binding.
    let id = SkillId::from_legacy_dir("demo").unwrap();
    let bundle = Bundle::from_entries(vec![BundleEntry {
        path: "SKILL.md".into(),
        bytes: skill_md("revised body").into_bytes(),
        executable: false,
    }])
    .unwrap();
    crate::skills::lifecycle::test_support::publish_revision(&store, &id, SourceKind::Learned, &bundle, 1);

    let same_task = read(scoped_args(owner, "task-tool-1", json!({"name": "demo"}))).await;
    assert!(
        same_task
            .get("content")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("legacy body")
    );
    assert_eq!(same_task.get("revision_id").unwrap().as_str().unwrap(), revision_a);
    let new_task = read(scoped_args(owner, "task-tool-2", json!({"name": "demo"}))).await;
    assert!(
        new_task
            .get("content")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("revised body")
    );
    assert_eq!(new_task.get("generation").unwrap(), &json!(2));

    // Production default: no task pinning, so every read follows the binding.
    let unpinned: serde_json::Value = serde_json::from_str(
        &unpinned_tool
            .execute(scoped_args(owner, "task-tool-1", json!({"name": "demo"})))
            .await
            .unwrap()
            .output,
    )
    .unwrap();
    assert!(
        unpinned
            .get("content")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("revised body")
    );
    assert_eq!(unpinned.get("pinned").unwrap(), &json!(false));

    // Callers without a trusted owner, and other owners, read the legacy
    // projection. Publication refreshed it, so they now see the committed
    // revision's content, not the pre-publication bytes.
    let legacy = read(json!({"name": "demo"})).await;
    assert!(
        legacy
            .get("content")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("revised body")
    );
    assert!(legacy.get("managed").is_none());
    let other = read(scoped_args("owner:ws:cli:bob", "task-tool-3", json!({"name": "demo"}))).await;
    assert!(other.get("managed").is_none());

    // Escapes are still refused for managed skills.
    let escape = tool
        .execute(scoped_args(
            owner,
            "task-tool-1",
            json!({"name": "demo", "resource_path": "../../x"}),
        ))
        .await
        .expect_err("managed resource escape must fail");
    assert!(escape.to_string().contains("cannot be read"));
    let owner_scope = OwnerScope::new(owner).unwrap();
    for task in ["task-tool-1", "task-tool-2", "task-tool-3"] {
        crate::skills::lifecycle::release_task_pins(&owner_scope, task);
    }
}

#[test]
fn skill_read_wire_alias_preserves_valid_names_and_distinguishes_display_names() {
    assert_eq!(catalog_skill_read_name("docx"), "docx");
    let names = [
        "Deploy helper".to_string(),
        "Deploy.helper".into(),
        "Skill / report".into(),
        "\u{3b1}\u{3b2}".into(),
        "x".repeat(129),
    ];
    let mut aliases = std::collections::BTreeSet::new();
    for name in names {
        let alias = catalog_skill_read_name(&name);
        assert_eq!(catalog_skill_read_name(&name), alias);
        assert!(
            alias.len() <= 128
                && alias
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        );
        assert!(aliases.insert(alias));
    }
}

#[tokio::test]
async fn skill_read_wire_alias_keeps_the_bound_display_name_and_backend_identity() {
    let temp = TempDir::new().unwrap();
    let directory = temp.path().join("skills/demo");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("SKILL.md"),
        skill_md("kept display name").replace("name: demo", "name: Deploy helper"),
    )
    .unwrap();
    let config = test_config(temp.path());
    let metadata = crate::skills::catalog_skill_metadata(temp.path(), &config);
    let metadata = metadata.first().unwrap();
    assert_eq!(metadata.name, "Deploy helper");
    let spec = catalog_skill_read_spec(metadata);
    assert_ne!(spec.name, metadata.name);
    assert_eq!(
        spec.parameters.pointer("/properties/name/const"),
        Some(&json!("Deploy helper"))
    );
    let tool = SkillReadTool::new(temp.path().into(), config);
    assert!(tool.supports_name(&spec.name));
    assert_eq!(tool.spec_for_name(&spec.name).unwrap().name, spec.name);
    assert_eq!(
        tool.catalog_binding_snapshot().get(&spec.name),
        Some(&crate::skills::catalog_skill_binding_digest(metadata))
    );
    assert_eq!(
        tool.catalog_binding_snapshot().get(&metadata.name),
        Some(&crate::skills::catalog_skill_binding_digest(metadata))
    );
    let result = tool
        .execute_named(&spec.name, json!({"name":"Deploy helper"}))
        .await
        .unwrap();
    assert!(result.success);
    assert!(result.output.contains("kept display name"));
    assert!(
        tool.execute_named(&spec.name, json!({"name":"another skill"}))
            .await
            .is_err()
    );
    let sources = crate::tools::capability::catalog::CatalogSources {
        skills: vec![metadata.clone()],
        ..Default::default()
    };
    let registry: Vec<Box<dyn Tool>> = vec![Box::new(tool)];
    let catalog = crate::tools::capability::catalog::CapabilityCatalog::build(
        &registry,
        &sources,
        &crate::tools::capability::catalog::SurfacePolicy::default(),
    )
    .unwrap();
    let service = crate::tools::ToolExecutionService::with_security_policy(
        registry,
        Arc::new(crate::security::SecurityPolicy::default()),
    );
    assert!(catalog.check_parity(&service).is_ok());
}

#[tokio::test]
async fn skill_read_alias_schema_is_closed_and_paths_stay_confined() {
    let temp = TempDir::new().unwrap();
    let skill_dir = temp.path().join("skills/demo");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), skill_md("body")).unwrap();
    std::fs::write(temp.path().join("secret.txt"), "secret").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(temp.path().join("secret.txt"), skill_dir.join("link.txt")).unwrap();
    let tool = SkillReadTool::new(temp.path().to_path_buf(), test_config(temp.path()));

    let metadata = crate::skills::catalog_skill_metadata(temp.path(), &test_config(temp.path()));
    let spec = catalog_skill_read_spec(metadata.first().unwrap());
    assert_eq!(spec.parameters.get("additionalProperties").unwrap(), &json!(false));
    assert!(spec.parameters.pointer("/properties/resource_path").is_some());

    let junk = tool
        .execute_named("demo", json!({"name": "demo", "extra": 1}))
        .await
        .expect_err("unknown alias argument must fail");
    assert!(junk.to_string().contains("Unexpected skill read argument"));
    let read = tool
        .execute_named("demo", json!({"name": "demo", "resource_path": "SKILL.md"}))
        .await
        .unwrap();
    assert!(read.success);

    for bad in ["../../secret.txt", "/etc/passwd", "link.txt"] {
        let error = tool
            .execute_named("demo", json!({"name": "demo", "resource_path": bad}))
            .await
            .expect_err("resource must stay inside the skill directory");
        let text = error.to_string();
        assert!(text.contains("escapes") || text.contains("relative"), "{bad}: {text}");
    }
}

#[tokio::test]
async fn skill_read_records_every_trusted_scope_identity_on_its_receipt() {
    use crate::skills::lifecycle::{Correlation, OwnerScope, SkillLifecycleStore, apply_migration, plan_migration};
    let temp = TempDir::new().unwrap();
    let skill_dir = temp.path().join("skills/demo");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), skill_md("body")).unwrap();
    let owner = "owner:ws:cli:alice";
    let store = SkillLifecycleStore::open(temp.path(), OwnerScope::new(owner).unwrap()).unwrap();
    let plan = plan_migration(temp.path(), Some(&store)).unwrap();
    let report = apply_migration(&store, temp.path(), &plan);
    assert_eq!(report.failed, Vec::new());
    let tool = SkillReadTool::new(temp.path().to_path_buf(), test_config(temp.path()));

    // The scope exactly as the execution service authors it (task, run and parent).
    let args = json!({
        "name": "demo",
        "_zc_scope_trusted": true,
        "_zc_scope": {
            "owner_id": owner,
            "task_id": "task-corr",
            "run_id": "run-corr",
            "parent_run_id": "run-parent"
        }
    });
    let result = tool.execute(args).await.unwrap();
    assert!(result.success);

    // The experience is keyed by the run (a child is never keyed by the task).
    let record = store.read_experience("run-corr").unwrap().unwrap();
    let expected = Correlation {
        run_id: Some("run-corr".into()),
        task_id: Some("task-corr".into()),
        parent_run_id: Some("run-parent".into()),
        segment_id: None,
        child_id: None,
    };
    assert_eq!(record.correlation, expected);
    assert_eq!(
        record.hydration_receipts.first().map(|receipt| &receipt.correlation),
        Some(&expected)
    );
    crate::skills::lifecycle::release_task_pins(&OwnerScope::new(owner).unwrap(), "run-corr");
}

fn lifecycle_tool(workspace: &Path, mode: crate::skills::lifecycle::LifecycleMode) -> SkillsManageTool {
    lifecycle_tool_with(workspace, mode, Arc::new(SecurityPolicy::default()))
}

fn lifecycle_tool_with(
    workspace: &Path,
    mode: crate::skills::lifecycle::LifecycleMode,
    security: Arc<SecurityPolicy>,
) -> SkillsManageTool {
    use crate::skills::lifecycle::{
        OwnerScope, SkillLifecycleStore, SkillsLifecycleService, SkillsLifecycleSettings,
    };
    let store = SkillLifecycleStore::open(workspace, OwnerScope::new("owner:ws:cli:alice").unwrap()).unwrap();
    let settings = SkillsLifecycleSettings {
        enabled: true,
        mode,
        ..SkillsLifecycleSettings::default()
    };
    let service = SkillsLifecycleService::new(Arc::new(store), settings).unwrap();
    SkillsManageTool::new(workspace.to_path_buf(), test_config(workspace), security)
        .with_lifecycle(Arc::new(service))
}

fn offered_actions(tool: &SkillsManageTool) -> Vec<String> {
    tool.parameters_schema()
        .pointer("/properties/action/enum")
        .and_then(serde_json::Value::as_array)
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect()
}

/// The actions the schema offers are exactly the legacy ones, plus the
/// lifecycle ones when a service is attached, and never an operator-only
/// operation or a parameter that lets the caller choose the suite, the
/// policy or the mode.
#[test]
fn lifecycle_actions_offered_match_the_policy_owner_split() {
    let workspace = TempDir::new().unwrap();
    let legacy = [
        "create", "install", "update", "enable", "disable", "validate", "sync", "remove",
    ];
    let plain = SkillsManageTool::new(
        workspace.path().to_path_buf(),
        test_config(workspace.path()),
        Arc::new(SecurityPolicy::default()),
    );
    assert_eq!(offered_actions(&plain), legacy);
    crate::tools::schema::action_contract::assert_action_tool_contract(
        plain.name(),
        plain.description(),
        plain.execution_schema(),
    );

    let tool = lifecycle_tool(workspace.path(), crate::skills::lifecycle::LifecycleMode::Shadow);
    let configured = plain.with_lifecycle(Arc::clone(tool.lifecycle.as_ref().unwrap()));
    assert_eq!(
        configured.execution_schema(),
        tool.execution_schema(),
        "attaching lifecycle must invalidate the cached plain contract"
    );
    let mut expected: Vec<String> = legacy.iter().map(|a| (*a).to_string()).collect();
    expected.extend(LIFECYCLE_READ_ACTIONS.iter().map(|a| (*a).to_string()));
    expected.extend(LIFECYCLE_MUTATING_ACTIONS.iter().map(|a| (*a).to_string()));
    let offered = offered_actions(&tool);
    assert_eq!(offered, expected);
    for forbidden in LIFECYCLE_OPERATOR_ONLY {
        assert!(
            !offered.iter().any(|action| action.contains(forbidden)),
            "{forbidden} must never be offered"
        );
    }
    let schema = tool.execution_schema();
    crate::tools::schema::assert_execution_schema_golden("skills_manage_lifecycle", schema);
    crate::tools::schema::assert_published_schema_golden("skills_manage_lifecycle", &tool.parameters_schema());
    let properties = schema.get("properties").and_then(serde_json::Value::as_object).unwrap();
    for forbidden in [
        "suite",
        "suite_digest",
        "policy",
        "mode",
        "settings",
        "candidate",
        "bundle",
        "path",
    ] {
        assert!(
            !properties.contains_key(forbidden),
            "{forbidden} must not be a parameter"
        );
    }
    crate::tools::schema::action_contract::assert_action_tool_contract(tool.name(), tool.description(), schema);
}

#[tokio::test]
async fn lifecycle_actions_are_read_through_the_service_and_unknown_ones_are_refused() {
    let workspace = TempDir::new().unwrap();
    let tool = lifecycle_tool(workspace.path(), crate::skills::lifecycle::LifecycleMode::Shadow);

    let status = tool
        .execute(owned(json!({"action": "lifecycle_status"})))
        .await
        .unwrap();
    assert!(status.success, "{:?}", status.error);
    let status: serde_json::Value = serde_json::from_str(&status.output).unwrap();
    assert_eq!(status.get("mode").and_then(serde_json::Value::as_str), Some("shadow"));
    assert!(
        status
            .get("publication_block")
            .and_then(serde_json::Value::as_str)
            .is_some()
    );

    let listed = tool.execute(owned(json!({"action": "lifecycle_list"}))).await.unwrap();
    assert!(listed.success, "{:?}", listed.error);
    assert!(listed.output.contains("\"proposals\": []"));

    // Operator-only operations are not actions at all.
    for forbidden in [
        "write_suite",
        "begin_evaluation",
        "lifecycle_begin_evaluation",
        "lifecycle_write_suite",
    ] {
        assert!(tool.execute(json!({"action": forbidden})).await.is_err(), "{forbidden}");
    }

    // A mutation reaches the service and is refused by the mode.
    let publish = tool
        .execute(with_grant(
            json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL}),
            &format!("skills_manage:lifecycle_publish:{PROPOSAL}"),
        ))
        .await
        .unwrap();
    assert!(!publish.success);
    assert!(
        publish.error.unwrap().contains("does not allow"),
        "mode must refuse publication"
    );
    let rollback = tool
        .execute(owned(json!({
            "action": "lifecycle_rollback",
            "skill_id": "release-notes",
            "revision_id": format!("r1-{}", "0".repeat(64)),
            "expected_generation": 1
        })))
        .await
        .unwrap();
    assert!(!rollback.success);
}

#[tokio::test]
async fn lifecycle_actions_are_unavailable_without_a_service() {
    let workspace = TempDir::new().unwrap();
    let tool = SkillsManageTool::new(
        workspace.path().to_path_buf(),
        test_config(workspace.path()),
        Arc::new(SecurityPolicy::default()),
    );
    for action in LIFECYCLE_READ_ACTIONS.iter().chain(LIFECYCLE_MUTATING_ACTIONS.iter()) {
        assert!(
            tool.execute(json!({"action": action, "proposal_id": "x"}))
                .await
                .is_err(),
            "{action}"
        );
    }
}

/// A signed one-shot grant for `operation`, as the agent loop issues it.
fn grant_for(operation: &str) -> serde_json::Value {
    use crate::acl::approval_grant::{ApprovalGrantV2, IssuerAuthority, RiskLevel, Subject, WitnessKeyring};
    let risk = if operation.contains(":lifecycle_cancel:") {
        RiskLevel::Medium
    } else {
        RiskLevel::High
    };
    let grant = ApprovalGrantV2::issue_one_shot(
        WitnessKeyring::global_for_tests(),
        Subject {
            agent_id: "prx:test:agent".to_string(),
            principal_id: "telegram:alice".to_string(),
            owner_id: "owner:ws:cli:alice".to_string(),
            workspace_id: "ws".to_string(),
            session_key: Some("session".to_string()),
        },
        IssuerAuthority::HumanReview,
        operation,
        risk,
    )
    .unwrap();
    serde_json::to_value(crate::security::policy::ApprovalGrant::from_verified_v2(
        "skills_manage",
        "test",
        grant,
    ))
    .unwrap()
}

/// Attaches `grant` and the trusted caller principal it is bound to.
fn attach_grant(mut args: serde_json::Value, grant: serde_json::Value) -> serde_json::Value {
    if let Some(object) = args.as_object_mut() {
        object.insert(crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.to_string(), grant);
        object.insert("_zc_scope_trusted".to_string(), json!(true));
        object.insert(
            "_zc_scope".to_string(),
            json!({"principal_id": "telegram:alice", "owner_id": "owner:ws:cli:alice"}),
        );
    }
    args
}

/// A call from the configured owner, with no grant.
fn owned(mut args: serde_json::Value) -> serde_json::Value {
    if let Some(object) = args.as_object_mut() {
        object.insert("_zc_scope_trusted".to_string(), json!(true));
        object.insert("_zc_scope".to_string(), json!({"owner_id": "owner:ws:cli:alice"}));
    }
    args
}

fn with_grant(args: serde_json::Value, operation: &str) -> serde_json::Value {
    attach_grant(args, grant_for(operation))
}

const PROPOSAL: &str = "prop-00000000000000000000000000000000";
const OTHER_PROPOSAL: &str = "prop-11111111111111111111111111111111";

/// Supervised needs review; supplied grants bind the target at every
/// autonomy level. Full without a grant still respects lifecycle mode.
#[tokio::test]
async fn lifecycle_mutations_bind_supplied_grants_and_supervised_requires_review() {
    use crate::security::AutonomyLevel;
    let revision = format!("r1-{}", "0".repeat(64));
    let rollback_args = json!({"action": "lifecycle_rollback", "skill_id": "release-notes", "revision_id": revision, "expected_generation": 1});
    let rollback_op = format!("skills_manage:lifecycle_rollback:release-notes:{revision}:1");
    let publish_args = json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL});
    let cancel_args = json!({"action": "lifecycle_cancel", "proposal_id": PROPOSAL});
    for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
        let workspace = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy {
            autonomy,
            workspace_dir: workspace.path().to_path_buf(),
            ..SecurityPolicy::default()
        });
        let tool = lifecycle_tool_with(
            workspace.path(),
            crate::skills::lifecycle::LifecycleMode::Shadow,
            security,
        );

        // No grant: Supervised refuses review; Full reaches the Shadow mode gate.
        for args in [&publish_args, &rollback_args] {
            let result = tool.execute(owned(args.clone())).await.unwrap();
            assert!(!result.success);
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains(if autonomy == AutonomyLevel::Full {
                        "does not allow"
                    } else {
                        "explicit approval grant"
                    }),
                "{autonomy:?}: {:?}",
                result.error
            );
        }
        // A grant bound to another proposal or target is refused too.
        for (args, operation) in [
            (
                &publish_args,
                format!("skills_manage:lifecycle_publish:{OTHER_PROPOSAL}"),
            ),
            (&publish_args, "skills_manage:lifecycle_publish".to_string()),
            (
                &rollback_args,
                "skills_manage:lifecycle_rollback:release-notes:r1-other:1".to_string(),
            ),
            // The grant binds the generation the caller observed.
            (
                &rollback_args,
                format!("skills_manage:lifecycle_rollback:release-notes:{revision}:2"),
            ),
            (
                &rollback_args,
                format!("skills_manage:lifecycle_rollback:release-notes:{revision}"),
            ),
        ] {
            let result = tool.execute(with_grant(args.clone(), &operation)).await.unwrap();
            assert!(!result.success);
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("explicit approval grant"),
                "{autonomy:?} {operation}: {:?}",
                result.error
            );
        }
        // The bound grant reaches the service, which the mode then refuses.
        for (args, operation) in [
            (&publish_args, format!("skills_manage:lifecycle_publish:{PROPOSAL}")),
            (&rollback_args, rollback_op.clone()),
        ] {
            let result = tool.execute(with_grant(args.clone(), &operation)).await.unwrap();
            assert!(!result.success);
            assert!(
                result.error.as_deref().unwrap_or_default().contains("does not allow"),
                "{autonomy:?} {operation}: {:?}",
                result.error
            );
        }
    }

    // Supervised cancel: refused without a grant, accepted with the bound one.
    let workspace = TempDir::new().unwrap();
    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        workspace_dir: workspace.path().to_path_buf(),
        ..SecurityPolicy::default()
    });
    let tool = lifecycle_tool_with(
        workspace.path(),
        crate::skills::lifecycle::LifecycleMode::Shadow,
        security,
    );
    let refused = tool.execute(owned(cancel_args.clone())).await.unwrap();
    assert!(!refused.success);
    assert!(
        refused.error.as_deref().unwrap_or_default().contains("approval grant"),
        "{:?}",
        refused.error
    );
    let wrong = tool
        .execute(with_grant(
            cancel_args.clone(),
            &format!("skills_manage:lifecycle_cancel:{OTHER_PROPOSAL}"),
        ))
        .await
        .unwrap();
    assert!(
        wrong.error.as_deref().unwrap_or_default().contains("approval grant"),
        "{:?}",
        wrong.error
    );
    let reached = tool
        .execute(with_grant(
            cancel_args,
            &format!("skills_manage:lifecycle_cancel:{PROPOSAL}"),
        ))
        .await
        .unwrap();
    assert!(!reached.success);
    assert!(
        reached
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("no such proposal"),
        "the bound grant must reach the service: {:?}",
        reached.error
    );
}

/// What `grant_op_for_call` mints for `args`: the exact operation and
/// whether the grant is issued at high risk.
fn minted(args: &serde_json::Value) -> Option<(String, bool)> {
    use crate::acl::approval_grant::RiskLevel;
    use crate::security::approval_operation::{GrantOp, grant_op_for_call};
    match grant_op_for_call("skills_manage", args)? {
        GrantOp::ResourceExact(operation, risk) => Some((operation, risk == RiskLevel::High)),
        GrantOp::Command(_) | GrantOp::ResourceGlob(..) => None,
    }
}

fn supervised_tool(workspace: &Path, autonomy: crate::security::AutonomyLevel) -> SkillsManageTool {
    lifecycle_tool_with(
        workspace,
        crate::skills::lifecycle::LifecycleMode::Shadow,
        Arc::new(SecurityPolicy {
            autonomy,
            workspace_dir: workspace.to_path_buf(),
            ..SecurityPolicy::default()
        }),
    )
}

/// The operation the runtime mints for a call is exactly the one the tool
/// checks: a grant for the minted operation gets the call to the service,
/// in both gated autonomy levels.
#[tokio::test]
async fn the_runtime_minted_operations_are_the_ones_the_tool_checks() {
    use crate::security::AutonomyLevel;
    let revision = format!("r1-{}", "0".repeat(64));
    let cases = [
        (
            json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL}),
            true,
            "does not allow",
        ),
        (
            json!({"action": "lifecycle_cancel", "proposal_id": PROPOSAL}),
            false,
            "no such proposal",
        ),
        (
            json!({"action": "lifecycle_rollback", "skill_id": "release-notes", "revision_id": revision, "expected_generation": 7}),
            true,
            "does not allow",
        ),
    ];
    for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
        let workspace = TempDir::new().unwrap();
        let tool = supervised_tool(workspace.path(), autonomy);
        for (args, high, reaches) in &cases {
            let (operation, minted_high) = minted(args).expect("a lifecycle mutation gets a grant");
            assert_eq!(minted_high, *high, "{operation}");
            let result = tool.execute(with_grant(args.clone(), &operation)).await.unwrap();
            assert!(!result.success);
            assert!(
                result.error.as_deref().unwrap_or_default().contains(reaches),
                "{autonomy:?} {operation}: {:?}",
                result.error
            );
        }
    }
    let (rollback, _) = minted(&cases[2].0).unwrap();
    assert_eq!(
        rollback,
        format!("skills_manage:lifecycle_rollback:release-notes:{revision}:7")
    );
    // Reads, incomplete mutations and the other actions get no grant.
    for args in [
        json!({"action": "lifecycle_status"}),
        json!({"action": "lifecycle_list"}),
        json!({"action": "lifecycle_inspect", "proposal_id": PROPOSAL}),
        json!({"action": "lifecycle_publish"}),
        json!({"action": "lifecycle_rollback", "skill_id": "a", "revision_id": "b"}),
        json!({"action": "create", "name": "x"}),
        json!({"action": "lifecycle_force_close", "proposal_id": PROPOSAL}),
    ] {
        assert!(minted(&args).is_none(), "{args}");
    }
}

/// A rollback grant minted for one observed generation does not permit
/// another.
#[tokio::test]
async fn a_rollback_grant_is_bound_to_the_generation() {
    let workspace = TempDir::new().unwrap();
    let tool = supervised_tool(workspace.path(), crate::security::AutonomyLevel::Full);
    let revision = format!("r1-{}", "0".repeat(64));
    let args_at = |generation: u64| json!({"action": "lifecycle_rollback", "skill_id": "release-notes", "revision_id": revision, "expected_generation": generation});
    let (operation, _) = minted(&args_at(3)).unwrap();
    let wrong = tool.execute(with_grant(args_at(4), &operation)).await.unwrap();
    assert!(
        wrong
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("explicit approval grant"),
        "{:?}",
        wrong.error
    );
    let right = tool.execute(with_grant(args_at(3), &operation)).await.unwrap();
    assert!(
        right.error.as_deref().unwrap_or_default().contains("does not allow"),
        "{:?}",
        right.error
    );
}

/// A grant authorizes one publication or rollback: replaying it is refused
/// under supervised (the gate spends it) and under full (the tool does).
#[tokio::test]
async fn a_publication_grant_cannot_be_replayed_in_any_autonomy_level() {
    use crate::security::AutonomyLevel;
    for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
        let workspace = TempDir::new().unwrap();
        let tool = supervised_tool(workspace.path(), autonomy);
        let args = json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL});
        let (operation, _) = minted(&args).unwrap();
        let grant = grant_for(&operation);

        let first = tool.execute(attach_grant(args.clone(), grant.clone())).await.unwrap();
        assert!(
            first.error.as_deref().unwrap_or_default().contains("does not allow"),
            "{autonomy:?}: the first use reaches the service: {:?}",
            first.error
        );
        let replay = tool.execute(attach_grant(args.clone(), grant.clone())).await.unwrap();
        assert!(!replay.success);
        assert!(
            replay.error.as_deref().unwrap_or_default().contains("replay"),
            "{autonomy:?}: {:?}",
            replay.error
        );
        assert!(
            !replay.error.as_deref().unwrap_or_default().contains("does not allow"),
            "{autonomy:?}: a replay must not reach the service"
        );
        // A fresh grant works again.
        let fresh = tool.execute(with_grant(args, &operation)).await.unwrap();
        assert!(fresh.error.as_deref().unwrap_or_default().contains("does not allow"));
    }
}

/// Only a signed one-shot grant authorizes a publication or rollback: a v1
/// operation hash grant has no use budget and never expires.
#[tokio::test]
async fn a_hash_grant_does_not_authorize_a_publication() {
    let workspace = TempDir::new().unwrap();
    let tool = supervised_tool(workspace.path(), crate::security::AutonomyLevel::Full);
    let args = json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL});
    let (operation, _) = minted(&args).unwrap();
    let v1 = serde_json::to_value(crate::security::policy::ApprovalGrant::for_resource_operation(
        "skills_manage",
        &operation,
        "test",
        None,
    ))
    .unwrap();
    let result = tool.execute(attach_grant(args, v1)).await.unwrap();
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("explicit approval grant"),
        "{:?}",
        result.error
    );
}

/// A caller the runtime knows to be another owner never reaches the store,
/// and does not spend its grant trying.
#[tokio::test]
async fn another_owner_does_not_reach_the_lifecycle_store() {
    let workspace = TempDir::new().unwrap();
    let tool = supervised_tool(workspace.path(), crate::security::AutonomyLevel::Full);
    let foreign = |owner: &str| {
        let mut args = with_grant(
            json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL}),
            &format!("skills_manage:lifecycle_publish:{PROPOSAL}"),
        );
        if let Some(object) = args.as_object_mut() {
            object.insert(
                "_zc_scope".to_string(),
                json!({"principal_id": "telegram:alice", "owner_id": owner}),
            );
        }
        args
    };
    let denied = tool.execute(foreign("owner:ws:cli:bob")).await.unwrap();
    assert!(
        denied
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("another owner scope"),
        "{:?}",
        denied.error
    );
    let status = tool.execute(json!({"action": "lifecycle_status", "_zc_scope_trusted": true, "_zc_scope": {"owner_id": "owner:ws:cli:bob"}})).await.unwrap();
    assert!(!status.success);
    // The configured owner is served.
    let served = tool.execute(foreign("owner:ws:cli:alice")).await.unwrap();
    assert!(
        served.error.as_deref().unwrap_or_default().contains("does not allow"),
        "{:?}",
        served.error
    );
}

/// A disabled lifecycle builds the tool without any lifecycle action and
/// opens no store.
#[test]
fn a_disabled_lifecycle_offers_nothing_and_opens_no_store() {
    let workspace = TempDir::new().unwrap();
    let tool = SkillsManageTool::from_config(
        workspace.path().to_path_buf(),
        test_config(workspace.path()),
        Arc::new(SecurityPolicy::default()),
    );
    assert!(tool.lifecycle.is_none());
    assert!(
        offered_actions(&tool)
            .iter()
            .all(|action| !action.starts_with("lifecycle_"))
    );
    assert!(!workspace.path().join("self").exists(), "no store was opened");

    let mut config = test_config(workspace.path());
    config.skills_lifecycle.enabled = true;
    config.skills_lifecycle.owner_scope = Some("owner:ws:cli:alice".to_string());
    let enabled = SkillsManageTool::from_config(
        workspace.path().to_path_buf(),
        config,
        Arc::new(SecurityPolicy::default()),
    );
    assert!(enabled.lifecycle.is_some());
    assert!(
        offered_actions(&enabled)
            .iter()
            .any(|action| action == "lifecycle_status")
    );
}

/// A caller whose owner the runtime cannot vouch for reaches nothing, reads
/// included: no trusted scope, an incomplete scope, an owner-isolated
/// caller without an owner id of its own.
#[tokio::test]
async fn a_caller_with_an_unknown_owner_reaches_no_lifecycle_action() {
    let workspace = TempDir::new().unwrap();
    let tool = supervised_tool(workspace.path(), crate::security::AutonomyLevel::Full);
    let args = json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL});
    let (operation, _) = minted(&args).unwrap();
    let grant = grant_for(&operation);
    let calls = [
        // No scope at all.
        json!({"action": "lifecycle_status"}),
        json!({"action": "lifecycle_list"}),
        // A scope the runtime did not mark trusted.
        json!({"action": "lifecycle_status", "_zc_scope": {"owner_id": "owner:ws:cli:alice"}}),
        // Trusted but incomplete.
        json!({"action": "lifecycle_status", "_zc_scope_trusted": true, "_zc_scope": {"workspace_id": "ws"}}),
        // Owner-isolated with workspace, channel and sender but no owner id.
        json!({"action": "lifecycle_status", "_zc_scope_trusted": true, "_zc_scope": {"owner_isolated": true, "workspace_id": "ws", "channel": "cli", "sender": "alice"}}),
        json!({"action": "lifecycle_status", "_zc_scope_trusted": true, "_zc_scope": {"owner_isolated": true, "owner_id": "  ", "workspace_id": "ws", "channel": "cli", "sender": "alice"}}),
    ];
    for call in calls {
        let result = tool.execute(call.clone()).await.unwrap();
        assert!(!result.success, "{call}");
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("known owner scope"),
            "{call}: {:?}",
            result.error
        );
    }
    // A grant is not spent by an unknown caller: it still works for the owner.
    let mut unknown = args.clone();
    if let Some(object) = unknown.as_object_mut() {
        object.insert(
            crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.to_string(),
            grant.clone(),
        );
    }
    let refused = tool.execute(unknown).await.unwrap();
    assert!(
        refused
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("known owner scope")
    );
    let served = tool.execute(attach_grant(args, grant)).await.unwrap();
    assert!(
        served.error.as_deref().unwrap_or_default().contains("does not allow"),
        "{:?}",
        served.error
    );
    // An owner-isolated caller with its own owner id is served.
    let isolated = tool
        .execute(json!({"action": "lifecycle_status", "_zc_scope_trusted": true, "_zc_scope": {"owner_isolated": true, "owner_id": "owner:ws:cli:alice"}}))
        .await
        .unwrap();
    assert!(isolated.success, "{:?}", isolated.error);
}

#[tokio::test]
async fn isolated_lifecycle_callers_cannot_write_the_shared_workspace() {
    let workspace = TempDir::new().unwrap();
    let tool = supervised_tool(workspace.path(), crate::security::AutonomyLevel::Full);
    for action in [
        "create",
        "install",
        "update",
        "enable",
        "disable",
        "validate",
        "sync",
        "remove",
        "lifecycle_publish",
        "lifecycle_rollback",
        "lifecycle_import_executable",
    ] {
        let result = tool
            .execute(json!({"action": action, "name":"prx-fixture", "description":"Fixture", "instructions":"Fixture instructions", "source":"fixture", "proposal_id":"prop-fixture", "skill_id":"prx-fixture", "revision_id":"0".repeat(64), "expected_generation":1,
                "_zc_scope_trusted": true,
                "_zc_scope": {"owner_isolated": true, "owner_id": "owner:ws:cli:alice"}
            }))
            .await
            .unwrap();
        assert!(!result.success, "{action}");
        assert!(result.error.unwrap_or_default().contains("local operator"), "{action}");
    }
    let foreign = tool
        .execute(json!({"action":"lifecycle_list", "_zc_scope_trusted":true,
        "_zc_scope":{"owner_isolated":true,"owner_id":"owner:ws:cli:prx_other"}}))
        .await
        .unwrap();
    assert!(!foreign.success);
    assert!(foreign.error.unwrap_or_default().contains("another owner"));
    assert!(!workspace.path().join("skills").exists());
}

/// A call the gate refuses does not burn the approval.
#[tokio::test]
async fn a_refused_call_does_not_spend_its_grant() {
    use crate::security::AutonomyLevel;
    let workspace = TempDir::new().unwrap();
    // Read-only autonomy: the gate refuses before any grant is looked at.
    let denied = supervised_tool(workspace.path(), AutonomyLevel::ReadOnly);
    let allowed = supervised_tool(workspace.path(), AutonomyLevel::Full);
    let args = json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL});
    let (operation, _) = minted(&args).unwrap();
    let grant = grant_for(&operation);
    let first = denied.execute(attach_grant(args.clone(), grant.clone())).await.unwrap();
    assert!(!first.success);
    assert!(!first.error.as_deref().unwrap_or_default().contains("does not allow"));
    let then = allowed.execute(attach_grant(args, grant)).await.unwrap();
    assert!(
        then.error.as_deref().unwrap_or_default().contains("does not allow"),
        "the grant was not burned by the refused call: {:?}",
        then.error
    );
}

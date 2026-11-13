use super::*;
use crate::skills::lifecycle::{OwnerScope, SkillLifecycleStore, apply_migration, plan_migration, task_pin};

fn reader_fixture() -> (tempfile::TempDir, Config, Vec<crate::skills::SkillMetadata>) {
    let workspace = tempfile::tempdir().expect("test: child skill workspace");
    for name in ["prx_bound", "prx_parent_only"] {
        let directory = workspace.path().join("skills").join(name);
        std::fs::create_dir_all(&directory).expect("test: skill directory");
        std::fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Project skill\n---\n\nProject instructions\n"),
        )
        .expect("test: skill instructions");
        std::fs::write(directory.join("resource.txt"), "project resource").expect("test: skill resource");
    }
    let mut config = Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.skills.open_skills_dir = Some(workspace.path().join("missing-open").to_string_lossy().into_owned());
    config.skills.openclaw_skills_dir = Some(workspace.path().join("missing-openclaw").to_string_lossy().into_owned());
    let metadata = crate::skills::catalog_skill_metadata(workspace.path(), &config)
        .into_iter()
        .filter(|skill| skill.name == "prx_bound")
        .collect();
    (workspace, config, metadata)
}

fn worker_reader(config: Config, metadata: Vec<crate::skills::SkillMetadata>) -> SkillReadTool {
    use crate::tools::capability::catalog::{CapabilityCatalog, CatalogSources, SurfacePolicy};
    let registry: Vec<Box<dyn Tool>> = vec![Box::new(SkillReadTool::new(
        config.workspace_dir.clone(),
        config.clone(),
    ))];
    let catalog = CapabilityCatalog::build(
        &registry,
        &CatalogSources {
            skills: metadata,
            ..CatalogSources::default()
        },
        &SurfacePolicy::default(),
    )
    .expect("test: authenticated catalog shape");
    let entry = catalog
        .entry_for_public_name("prx_bound")
        .expect("test: signed child skill");
    let allowed = std::collections::BTreeSet::from([crate::tools::capability::contract::CapabilityRef {
        id: entry.id.clone(),
        fingerprint: entry.fingerprint,
        generation: entry.generation,
    }]);
    crate::session_worker::runner::worker_child_skill_reader(&config, &catalog, &allowed)
        .expect("test: production worker reader construction")
}

#[tokio::test]
async fn child_skill_read_refuses_unbound_parent_skill() {
    let (workspace, config, metadata) = reader_fixture();
    let tool = worker_reader(config, metadata);
    for arguments in [
        json!({"name":"prx_parent_only"}),
        json!({"name":"prx_parent_only","resource_path":"resource.txt"}),
    ] {
        let error = tool.execute(arguments).await.expect_err("test: unbound skill refused");
        assert_eq!(
            error.downcast_ref::<ChildSkillReadError>(),
            Some(&ChildSkillReadError::Unbound),
            "test: typed unbound denial"
        );
    }
    assert!(!tool.supports_name("prx_parent_only"), "test: unbound alias hidden");
    let owner = OwnerScope::new("prx_owner").expect("test: lifecycle owner");
    let store = SkillLifecycleStore::open(workspace.path(), owner.clone()).expect("test: lifecycle store");
    let plan = plan_migration(workspace.path(), Some(&store)).expect("test: migration plan");
    assert!(
        apply_migration(&store, workspace.path(), &plan).failed.is_empty(),
        "test: migrate skill"
    );
    let skill_id = crate::skills::lifecycle::SkillId::from_legacy_dir("prx_bound").expect("test: skill id");
    assert!(
        task_pin(&owner, "prx_child_task", &skill_id, || Ok(None))
            .expect("test: pin snapshot")
            .is_none(),
        "test: initially no task pin"
    );
    let read = tool.execute(json!({"name":"prx_bound", "_zc_scope_trusted":true, "_zc_scope":{"owner_id":"prx_owner", "task_id":"prx_child_task"}})).await.expect("test: bound read");
    let output: serde_json::Value = serde_json::from_str(&read.output).expect("test: read receipt");
    assert_eq!(
        output.get("pinned"),
        Some(&json!(false)),
        "test: child never pins lifecycle revisions"
    );
    assert!(
        task_pin(&owner, "prx_child_task", &skill_id, || Ok(None))
            .expect("test: pin snapshot")
            .is_none(),
        "test: parent pin store unchanged"
    );
}

#[tokio::test]
async fn child_skill_read_refuses_changed_frontmatter_digest() {
    let (workspace, config, metadata) = reader_fixture();
    let tool = worker_reader(config, metadata);
    std::fs::write(
        workspace.path().join("skills/prx_bound/SKILL.md"),
        "---\nname: prx_bound\ndescription: Changed metadata\n---\n\nProject instructions\n",
    )
    .expect("test: metadata changes after child issuance");
    let error = tool
        .execute(json!({"name":"prx_bound"}))
        .await
        .expect_err("test: stale metadata refused");
    assert_eq!(
        error.downcast_ref::<ChildSkillReadError>(),
        Some(&ChildSkillReadError::MetadataChanged),
        "test: typed stale metadata error"
    );
    assert!(!tool.supports_name("prx_bound"), "test: stale alias hidden");
}

use super::*;
#[test]
fn nested_case_registry_is_fixed_and_data_storage_never_overlaps_runtime_state() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let data = directory.path().join("DATA");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&data).unwrap();
    assert!(ControlledCaseScope::create(&state, &state, "owner_a").is_err());
    let case = ControlledCaseScope::create(&data, &state, "owner_a").unwrap();
    assert!(!case.data_root().starts_with(&state));
    assert!(!state.starts_with(case.data_root()));
    let mut config = Config::default();
    config.workspace_dir = state.clone();
    config.runtime.kind = "native".into();
    config.tunnel.provider = "none".into();
    config.plugins.enabled = false;
    config.memory.backend = "sqlite".into();
    config.sessions_spawn.allow_recursive_delegation = false;
    config.tool_tiering.always_include = vec!["file_read".into(), "file_write".into(), "file_edit".into()];
    let security = Arc::new(crate::security::SecurityPolicy {
        workspace_dir: state.clone(),
        ..Default::default()
    });
    let registry = case.registry(&config, &security).unwrap();
    let names: Vec<_> = registry.iter().map(|tool| tool.name()).collect();
    assert_eq!(
        names,
        vec![
            "skill_read",
            "capability_search",
            "file_read",
            "file_write",
            "file_edit"
        ]
    );
    config.tool_tiering.always_include.push("shell".into());
    assert!(case.registry(&config, &security).is_err());
    config.tool_tiering.always_include.pop();
    config.workspace_dir = data;
    assert!(case.registry(&config, &security).is_err());
}
#[test]
fn data_root_symlink_cannot_point_native_backend_at_internal_state() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let alias = directory.path().join("DATA");
    symlink(&state, &alias).unwrap();
    assert!(ControlledCaseScope::create(&alias, &state, "owner_a").is_err());
}

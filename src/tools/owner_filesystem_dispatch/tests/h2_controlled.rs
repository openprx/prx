//! Exercise the actual native backend, including direct execute, not schema hiding.
use super::*;
#[tokio::test]
async fn controlled_backend_requires_exact_runtime_filesystem_even_for_relative_paths() {
    let fixture = Fixture::new().unwrap();
    let filesystem = fixture.owner("owner_a").unwrap();
    let write = FileWriteTool::new_controlled(fixture.security(AutonomyLevel::Full), filesystem.clone());
    assert!(write.execute(json!({"path":"notes","content":"escape"})).await.is_err());
    let wrong = fixture.owner("owner_b").unwrap();
    assert!(
        owned_call(wrong, &write, json!({"path":"notes","content":"escape"}))
            .await
            .is_err()
    );
    let result = owned_call(
        filesystem.clone(),
        &write,
        json!({"path":"notes","content":"inside DATA"}),
    )
    .await
    .unwrap();
    assert!(result.success);
    assert!(!fixture.workspace.join("notes").exists());
    let read = FileReadTool::new_controlled(fixture.security(AutonomyLevel::Full), false, filesystem.clone());
    assert_eq!(
        owned_call(filesystem, &read, json!({"path":"notes"}))
            .await
            .unwrap()
            .output,
        "inside DATA"
    );
}
#[tokio::test]
async fn controlled_native_backend_cannot_read_keys_absolute_paths_or_symlink_directories() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new().unwrap();
    let filesystem = fixture.owner("owner_a").unwrap();
    let external = fixture.config.join("decision-key");
    std::fs::write(&external, b"secret-key").unwrap();
    std::fs::set_permissions(&external, std::fs::Permissions::from_mode(0o600)).unwrap();
    let seed = filesystem.resolve("seed").unwrap();
    filesystem.write_authorized(&seed, b"seed").unwrap();
    symlink(&fixture.config, filesystem.namespace_root().join("config-alias")).unwrap();
    let read = FileReadTool::new_controlled(fixture.security(AutonomyLevel::Full), false, filesystem.clone());
    for path in [
        external.to_string_lossy().into_owned(),
        "../decision-key".into(),
        "config-alias/decision-key".into(),
    ] {
        let result = owned_call(filesystem.clone(), &read, json!({"path":path})).await;
        assert!(result.is_err() || !result.unwrap().success);
    }
    let write = FileWriteTool::new_controlled(fixture.security(AutonomyLevel::Full), filesystem.clone());
    let result = owned_call(
        filesystem,
        &write,
        json!({"path":"config-alias/decision-key","content":"modified"}),
    )
    .await;
    assert!(result.is_err() || !result.unwrap().success);
    assert_eq!(std::fs::read(external).unwrap(), b"secret-key");
}

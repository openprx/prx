//! Provisioned case inputs must interoperate with the actual confined backend.
use super::*;
use crate::tools::owner_filesystem::OwnerFilesystem;
use std::os::unix::fs::PermissionsExt;

#[test]
fn private_case_provisioning_supports_native_effects_and_nested_fixtures() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let state = directory.path().join("runtime-state");
    let private = directory.path().join("case-data");
    create_private_dir_all(&state)?;
    create_private_dir_all(&private)?;
    let filesystem = OwnerFilesystem::open(&private, "case-workspace", "case-owner", &state)?;
    create_private_dir_all(filesystem.namespace_root())?;
    write_fixtures(
        filesystem.namespace_root(),
        &[FixtureFile {
            path: "nested/seed.txt".into(),
            content: "seed input\n".into(),
        }],
    )?;
    let seed = filesystem.resolve_existing("nested/seed.txt")?;
    assert_eq!(filesystem.read(&seed, 128)?, b"seed input\n");
    let artifact = filesystem.resolve("artifact.txt")?;
    filesystem.write_authorized(&artifact, b"PASS-TEMP\n")?;
    assert_eq!(
        filesystem.edit_authorized(&artifact, "PASS-TEMP", "VERIFIED", false)?,
        1
    );
    assert_eq!(filesystem.read(&artifact, 128)?, b"VERIFIED\n");
    assert!(!state.join("artifact.txt").exists());
    assert_eq!(
        fs::metadata(filesystem.namespace_root().join("nested/seed.txt"))?
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    Ok(())
}

#[test]
fn private_provisioning_does_not_repair_an_existing_nonprivate_namespace() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let state = directory.path().join("runtime-state");
    let private = directory.path().join("case-data");
    create_private_dir_all(&state)?;
    create_private_dir_all(&private)?;
    let filesystem = OwnerFilesystem::open(&private, "case-workspace", "case-owner", &state)?;
    create_private_dir_all(filesystem.namespace_root())?;
    fs::set_permissions(filesystem.namespace_root(), fs::Permissions::from_mode(0o755))?;
    create_private_dir_all(filesystem.namespace_root())?;
    assert_eq!(
        fs::metadata(filesystem.namespace_root())?.permissions().mode() & 0o777,
        0o755
    );
    assert!(filesystem.resolve("artifact.txt").is_err());
    Ok(())
}

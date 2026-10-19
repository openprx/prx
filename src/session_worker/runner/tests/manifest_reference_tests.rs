#![cfg(test)]

use super::base_manifest;
use crate::session_worker::protocol::WorkerManifest;
use crate::session_worker::runner::capability::{
    authenticate_worker_capability_with_key, compute_worker_capability_with_key, manifest_signing_payload,
};
use crate::session_worker::runner::parse_worker_manifest_line;
use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::session_worker::manifest_reference::{PreparedManifest, restore};
use std::os::unix::fs::PermissionsExt as _;

struct Fixture {
    workspace: tempfile::TempDir,
    key: ring::hmac::Key,
    manifest: WorkerManifest,
    capability: String,
    snapshot: PreparedManifest,
}

impl Fixture {
    fn new() -> Result<Self> {
        let workspace = tempfile::tempdir()?;
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"prx_manifest_reference_fixture");
        let mut manifest = base_manifest(workspace.path(), "");
        manifest.system_prompt = Some("prx reference roundtrip ".repeat(256));
        let capability =
            compute_worker_capability_with_key(&key, &manifest.run_id, 300, &manifest_signing_payload(&manifest)?);
        manifest.parent_capability = Some(capability.clone());
        let snapshot = PreparedManifest::prepare(&manifest, &key, 300)?;
        Ok(Self {
            workspace,
            key,
            manifest,
            capability,
            snapshot,
        })
    }

    fn value(&self) -> Result<serde_json::Value> {
        Ok(serde_json::from_str(self.snapshot.line())?)
    }

    fn directory(&self) -> Result<PathBuf> {
        let value = self.value()?;
        Ok(PathBuf::from(
            value
                .pointer("/worker_manifest_reference/body/directory")
                .and_then(serde_json::Value::as_str)
                .context("reference directory missing")?,
        ))
    }

    fn restore(&self) -> Result<WorkerManifest> {
        restore(self.value()?, &self.key, &self.capability, 300, 0)
    }
}

#[test]
fn reference_roundtrip_preserves_complete_manifest_and_original_capability() -> Result<()> {
    let mut fixture = Fixture::new()?;
    assert!(serde_json::to_vec(&fixture.manifest)?.len() > 3072);
    assert!(fixture.snapshot.line().len() < 3072);
    let restored = fixture.restore()?;
    assert_eq!(serde_json::to_vec(&restored)?, serde_json::to_vec(&fixture.manifest)?);
    let transport =
        authenticate_worker_capability_with_key(&restored, Some(&fixture.capability), 300, 0, fixture.key.clone())?;
    transport.verify_manifest(&restored)?;
    fixture.snapshot.close()?;
    Ok(())
}

#[test]
fn reference_tampering_is_rejected_before_snapshot_access() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut value = fixture.value()?;
    *value
        .pointer_mut("/worker_manifest_reference/body/directory")
        .context("reference directory missing")? = serde_json::json!("/prx_nonexistent_reference");
    let result = restore(value, &fixture.key, &fixture.capability, 300, 0);
    let error = result.err().context("tampered reference unexpectedly accepted")?;
    assert!(error.to_string().contains("signature is invalid"));
    assert!(!fixture.directory()?.join("claimed").exists());
    Ok(())
}

#[test]
fn reference_wrong_channel_and_expiry_do_not_consume_snapshot() -> Result<()> {
    let fixture = Fixture::new()?;
    assert!(restore(fixture.value()?, &fixture.key, "prx_other_capability", 300, 0).is_err());
    assert!(restore(fixture.value()?, &fixture.key, &fixture.capability, 301, 0).is_err());
    assert!(restore(fixture.value()?, &fixture.key, &fixture.capability, 300, 301).is_err());
    assert!(!fixture.directory()?.join("claimed").exists());
    fixture.restore()?;
    Ok(())
}

#[test]
fn reference_has_exactly_one_consumer_even_when_claims_race() -> Result<()> {
    let fixture = Fixture::new()?;
    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| fixture.restore().is_ok());
        let second = scope.spawn(|| fixture.restore().is_ok());
        (first.join(), second.join())
    });
    let first = first.map_err(|_| anyhow::anyhow!("first reference consumer panicked"))?;
    let second = second.map_err(|_| anyhow::anyhow!("second reference consumer panicked"))?;
    assert_ne!(first, second);
    assert!(fixture.restore().is_err());
    Ok(())
}

#[test]
fn reference_refuses_symlink_leaf_and_ancestor() -> Result<()> {
    for ancestor in [false, true] {
        let fixture = Fixture::new()?;
        let directory = fixture.directory()?;
        let original = if ancestor {
            directory.clone()
        } else {
            directory.join("manifest.json")
        };
        let moved = fixture.workspace.path().join("prx_moved_snapshot");
        std::fs::rename(&original, &moved)?;
        std::os::unix::fs::symlink(&moved, &original)?;
        assert!(fixture.restore().is_err());
        std::fs::remove_file(&original)?;
        std::fs::rename(moved, original)?;
    }
    Ok(())
}

#[test]
fn reference_refuses_writable_file_and_public_directory() -> Result<()> {
    for directory_case in [false, true] {
        let fixture = Fixture::new()?;
        let directory = fixture.directory()?;
        let path = if directory_case {
            directory
        } else {
            directory.join("manifest.json")
        };
        let mode = if directory_case { 0o755 } else { 0o600 };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
        assert!(fixture.restore().is_err());
        let original_mode = if directory_case { 0o700 } else { 0o400 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(original_mode))?;
    }
    Ok(())
}

#[test]
fn reference_refuses_same_length_content_tampering_and_hardlinks() -> Result<()> {
    for hardlink in [false, true] {
        let fixture = Fixture::new()?;
        let path = fixture.directory()?.join("manifest.json");
        if hardlink {
            std::fs::hard_link(&path, fixture.workspace.path().join("prx_extra_link"))?;
        } else {
            let mut bytes = std::fs::read(&path)?;
            *bytes.first_mut().context("snapshot empty")? = b'!';
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            std::fs::write(&path, bytes)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400))?;
        }
        assert!(fixture.restore().is_err());
    }
    Ok(())
}

#[test]
fn reference_lifetime_tracks_spawn_and_confirmed_cleanup() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = fixture.directory()?;
    drop(fixture.snapshot);
    assert!(!path.exists(), "pre-spawn snapshot must be removed");

    let mut fixture = Fixture::new()?;
    let path = fixture.directory()?;
    fixture.snapshot.mark_spawned();
    drop(fixture.snapshot);
    assert!(path.exists(), "unconfirmed child retains its snapshot");
    std::fs::remove_dir_all(path)?;

    let mut fixture = Fixture::new()?;
    let path = fixture.directory()?;
    fixture.snapshot.mark_spawned();
    fixture.restore()?;
    fixture.snapshot.close()?;
    assert!(!path.exists(), "confirmed cleanup removes snapshot");
    Ok(())
}

#[test]
fn legacy_inline_manifest_still_decodes_without_reference_environment() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let manifest = base_manifest(workspace.path(), "prx_inline_capability");
    let line = serde_json::to_string(&manifest)?;
    let decoded = parse_worker_manifest_line(&line)?;
    assert_eq!(serde_json::to_vec(&decoded)?, serde_json::to_vec(&manifest)?);
    let duplicate = line.replacen('{', r#"{"run_id":"prx_duplicate", "#, 1);
    assert!(parse_worker_manifest_line(&duplicate).is_err());
    Ok(())
}

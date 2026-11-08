#![cfg(test)]

//! Shared fixtures for the plane-off tombstone fence tests of every input issuer.
//!
//! The tombstone state is never written by hand: an input is admitted with the
//! decision plane on through a view that carries an erasure origin, then the
//! production erasure store erases that origin.
use crate::memory::Memory;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Recovery keys refuse group-writable ancestors, so a fixture lives under `$HOME`.
pub(crate) fn home_dir() -> tempfile::TempDir {
    let home = std::env::var_os("HOME").expect("test: HOME is unavailable");
    let home = Path::new(&home).canonicalize().expect("test: HOME does not resolve");
    tempfile::Builder::new()
        .prefix(".prx-auth2-")
        .tempdir_in(home)
        .expect("test: home tempdir")
}

pub(crate) fn erasure_origin() -> crate::memory::erasure::ErasureOrigin {
    crate::memory::erasure::ErasureOrigin::new("prx_account", "prx_conversation", "prx_message")
}

/// Production erasure of every row written through a view carrying [`erasure_origin`].
pub(crate) fn erase(base: &Arc<dyn Memory>) {
    let store = base.erasure_store().expect("test: erasure store");
    store.begin_delete(&erasure_origin()).expect("test: begin delete");
    store.erase_rows(&erasure_origin()).expect("test: erase rows");
}

pub(crate) fn keys_dir(workspace: &Path) -> PathBuf {
    workspace.join("memory").join(".recovery_keys")
}

/// Names, bytes and mtimes of every recovery key file, or `None` without the directory.
pub(crate) fn key_snapshot(workspace: &Path) -> Option<Vec<(String, Vec<u8>, std::time::SystemTime)>> {
    let entries = std::fs::read_dir(keys_dir(workspace)).ok()?;
    let mut files: Vec<_> = entries
        .map(|entry| {
            let entry = entry.expect("test: key entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).expect("test: key bytes"),
                entry
                    .metadata()
                    .expect("test: key metadata")
                    .modified()
                    .expect("test: mtime"),
            )
        })
        .collect();
    files.sort();
    Some(files)
}

/// Move every recovery key aside (the key is lost).
pub(crate) fn stash_keys(workspace: &Path) {
    let stash = workspace.join("stashed-keys");
    std::fs::create_dir_all(&stash).expect("test: stash dir");
    for entry in std::fs::read_dir(keys_dir(workspace)).expect("test: key dir") {
        let entry = entry.expect("test: key entry");
        std::fs::rename(entry.path(), stash.join(entry.file_name())).expect("test: stash key");
    }
}

pub(crate) use crate::memory::sqlite::fixture_readback::{row_count, table_exists};

pub(crate) fn is_already_accepted(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<super::AlreadyAccepted>())
}

pub(crate) fn is_fence_unavailable(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<crate::memory::ingress_recovery::StableKeyFenceUnavailable>())
}

/// Production fence-only append: no `with_ingress_recovery`, so AUTH-02-2a
/// activation never runs. The stable-key fence still executes inside the
/// backend commit.
pub(crate) async fn fence_only_record(
    memory: Arc<dyn Memory>,
    workspace: &Path,
    content: &str,
    key: &str,
) -> anyhow::Result<Option<crate::memory::MessageEvent>> {
    let fabric = crate::memory::MemoryFabric::new(memory, workspace.to_string_lossy());
    let scope = crate::memory::fabric::MessageEventScope::new(
        crate::memory::MessageEventSource::Agent,
        crate::memory::MemoryVisibility::Session,
    )
    .with_owner_id("prx_user")
    .with_run_id("prx-fence-only");
    fabric
        .record_inbound_user_message_best_effort(scope, content, Some(key.to_owned()), "fence")
        .await
}

pub(crate) fn assert_fence_created_nothing(workspace: &Path) {
    assert!(
        !table_exists(workspace, "recovery_tombstones"),
        "the fence created the tombstone table"
    );
    assert!(!keys_dir(workspace).exists(), "the fence created .recovery_keys");
}

pub(crate) fn assert_plane_off_no_decision_store(workspace: &Path) {
    assert!(
        !workspace.join("self/causal_tree/.decision_key").exists(),
        "plane-off created .decision_key"
    );
    assert!(
        !crate::causal_tree::store::db_path(workspace).exists(),
        "plane-off created decisions.db"
    );
}

pub(crate) fn assert_recovery_keys_owner_modes(workspace: &Path) {
    let dir = keys_dir(workspace);
    assert!(
        dir.exists(),
        "plane-off sqlite activation did not create .recovery_keys"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).expect("test: metadata").permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700, ".recovery_keys is not 0700");
        for entry in std::fs::read_dir(&dir).expect("test: keys dir") {
            let path = entry.expect("test: key entry").path();
            if path.is_file() {
                assert_eq!(mode(&path), 0o600, "recovery key is not 0600");
            }
        }
    }
}

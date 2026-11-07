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

//! Bounded walks over a fixture tree.

use std::io::Read as _;
use std::path::{Path, PathBuf};

/// Largest file the scan reads; a larger file fails the test.
pub const MAX_SCANNED_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Most regular files the scan visits; more fails the test.
pub const MAX_SCANNED_FILES: usize = 4096;

/// Every regular file under `root` (no symlink is followed), bounded by
/// `MAX_SCANNED_FILES`.
fn regular_files(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // SQLite and worker cleanup may remove a directory after it was
            // listed; a vanished directory holds nothing.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("test: read scanned directory {}: {error}", dir.display()),
        };
        for entry in entries {
            let entry = entry.expect("test: scanned directory entry");
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                files.push(entry.path());
                assert!(
                    files.len() <= MAX_SCANNED_FILES,
                    "scan of {} exceeded MAX_SCANNED_FILES",
                    root.display()
                );
            }
        }
    }
    files
}

/// No regular file under `root` contains `secret`, compared
/// case-insensitively because full-text indexes store lowercased tokens.
pub fn assert_nothing_under_persists_secret(root: &Path, secret: &str) {
    let needle = secret.to_ascii_lowercase();
    for file in regular_files(root) {
        let handle = match std::fs::File::open(&file) {
            Ok(handle) => handle,
            // SQLite may checkpoint and unlink a WAL or shared-memory sidecar
            // after enumeration. A vanished file cannot retain the secret.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("test: open scanned file {}: {error}", file.display()),
        };
        let mut bytes = Vec::new();
        handle
            .take(MAX_SCANNED_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .expect("test: read scanned file");
        assert!(
            u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= MAX_SCANNED_FILE_BYTES,
            "{} exceeded MAX_SCANNED_FILE_BYTES",
            file.display()
        );
        assert!(
            !String::from_utf8_lossy(&bytes).to_ascii_lowercase().contains(&needle),
            "{} persisted the secret",
            file.display()
        );
    }
}

/// No decision store artefact exists anywhere under `root`.
pub fn assert_no_decision_records(root: &Path) {
    for file in regular_files(root) {
        let name = file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        assert!(
            ![".decision_key", "decisions.db", "journal.db"].contains(&name.as_str()),
            "decision record {} exists",
            file.display()
        );
    }
}

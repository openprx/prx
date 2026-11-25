//! A cheap change stamp for the directories a skill catalog is read from.
//!
//! The process-level catalog snapshot is reused until this stamp changes, so a
//! skill installed, edited or removed from any process (a shell, a child, the
//! operator) is seen by the next read without a restart. The stamp reads file
//! metadata only; it never opens a skill file.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Skill files whose own modification time matters beside their directory's.
const SKILL_FILES: [&str; 2] = ["SKILL.md", "SKILL.toml"];

fn hash_metadata(path: &Path, hasher: &mut DefaultHasher) {
    path.hash(hasher);
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            metadata.len().hash(hasher);
            metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|age| age.as_nanos())
                .hash(hasher);
        }
        Err(_) => 0_u8.hash(hasher),
    }
}

fn hash_root(root: &Path, hasher: &mut DefaultHasher) {
    hash_metadata(root, hasher);
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut paths: Vec<_> = entries.filter_map(Result::ok).map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        hash_metadata(&path, hasher);
        if path.is_dir() {
            for file in SKILL_FILES {
                hash_metadata(&path.join(file), hasher);
            }
        }
    }
}

/// The stamp of `roots`: the directories themselves, their entries and each
/// entry's skill files.
pub(super) fn stamp_of(roots: &[&Path]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for root in roots {
        hash_root(root, &mut hasher);
    }
    hasher.finish()
}

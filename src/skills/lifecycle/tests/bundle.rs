#![cfg(test)]

use super::{basic_entries, entry, md};
use crate::skills::lifecycle::{
    Bundle, BundleEntry, BundleError, MAX_BUNDLE_COMPONENT_BYTES, MAX_BUNDLE_FILE_BYTES, MAX_BUNDLE_FILES,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

fn id_of(entries: Vec<BundleEntry>) -> String {
    Bundle::from_entries(entries)
        .expect("test: valid bundle")
        .revision_id()
        .to_string()
}

fn at(entries: &mut [BundleEntry], index: usize) -> &mut BundleEntry {
    entries.get_mut(index).expect("test: entry exists")
}

fn error_of(entries: Vec<BundleEntry>) -> BundleError {
    Bundle::from_entries(entries).expect_err("test: bundle must be rejected")
}

#[test]
fn canonical_format_matches_the_documented_encoding() {
    let content = b"hello";
    let mut hasher = Sha256::new();
    hasher.update(b"prx-skill-bundle\0");
    hasher.update(1u32.to_be_bytes());
    hasher.update(1u64.to_be_bytes());
    hasher.update(8u32.to_be_bytes());
    hasher.update(b"SKILL.md");
    hasher.update(5u64.to_be_bytes());
    hasher.update(Sha256::digest(content));
    hasher.update([0u8]);
    let expected = format!("r1-{}", hex::encode(hasher.finalize()));
    assert_eq!(id_of(vec![entry("SKILL.md", content)]), expected);
}

#[test]
fn bundle_ids_are_reproducible_across_runs_and_file_orders() {
    let first = id_of(basic_entries());
    assert_eq!(first, id_of(basic_entries()));
    let mut reversed = basic_entries();
    reversed.reverse();
    assert_eq!(first, id_of(reversed));
}

#[test]
fn any_content_path_or_mode_change_changes_the_id() {
    let base = id_of(basic_entries());
    let mutate = |f: fn(&mut Vec<BundleEntry>)| {
        let mut entries = basic_entries();
        f(&mut entries);
        id_of(entries)
    };
    let changed = [
        mutate(|e| at(e, 1).bytes.extend_from_slice(b"x")),
        mutate(|e| at(e, 1).bytes = b"# Guide\nkeep CRLF\n".to_vec()),
        mutate(|e| at(e, 2).bytes = b"#!/bin/sh\necho ho\n".to_vec()),
        mutate(|e| at(e, 2).executable = false),
        mutate(|e| at(e, 1).executable = true),
        mutate(|e| at(e, 1).path = PathBuf::from("references/other.md")),
        mutate(|e| e.push(entry("assets/extra.txt", b""))),
        mutate(|e| {
            e.pop();
        }),
    ];
    for (index, id) in changed.iter().enumerate() {
        assert_ne!(&base, id, "mutation {index} must change the bundle id");
    }
    let unique: std::collections::BTreeSet<_> = changed.iter().collect();
    assert_eq!(unique.len(), changed.len());
}

#[test]
fn directory_bundle_matches_entry_bundle_and_tracks_exec_mode() {
    let tmp = TempDir::new().expect("test: tempdir");
    let root = tmp.path().join("bundle");
    for item in basic_entries() {
        let target = root.join(&item.path);
        fs::create_dir_all(target.parent().expect("test: parent")).expect("test: mkdir");
        fs::write(&target, &item.bytes).expect("test: write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if item.executable { 0o755 } else { 0o644 };
            fs::set_permissions(&target, fs::Permissions::from_mode(mode)).expect("test: chmod");
        }
    }
    let from_dir = Bundle::from_dir(&root).expect("test: dir bundle");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(from_dir.revision_id().to_string(), id_of(basic_entries()));
        let script = root.join("scripts/run.sh");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).expect("test: chmod");
        let dropped = Bundle::from_dir(&root).expect("test: dir bundle");
        assert_ne!(dropped.revision_id(), from_dir.revision_id());
    }
}

#[test]
fn traversal_and_absolute_paths_are_rejected() {
    for path in ["../escape.md", "a/../../b.md", "a/../b.md", "./SKILL.md"] {
        let err = error_of(vec![entry("SKILL.md", b"x"), entry(path, b"x")]);
        assert!(
            matches!(err, BundleError::Traversal(_) | BundleError::InvalidPath { .. }),
            "{path}: {err}"
        );
    }
    let err = error_of(vec![entry("/etc/passwd", b"x")]);
    assert!(matches!(err, BundleError::AbsolutePath(_)), "{err}");
}

#[test]
fn duplicate_paths_are_rejected() {
    let err = error_of(vec![entry("SKILL.md", b"a"), entry("SKILL.md", b"b")]);
    assert!(matches!(err, BundleError::DuplicatePath(_)), "{err}");
}

#[test]
fn case_and_normalization_collisions_are_rejected() {
    let err = error_of(vec![entry("references/A.md", b"a"), entry("references/a.md", b"b")]);
    assert!(matches!(err, BundleError::CaseCollision { .. }), "{err}");
    let err = error_of(vec![entry("caf\u{e9}.md", b"a"), entry("CAF\u{c9}.md", b"b")]);
    assert!(matches!(err, BundleError::CaseCollision { .. }), "{err}");
    // A decomposed (NFD) spelling is not canonical and is refused outright.
    let err = error_of(vec![entry("cafe\u{301}.md", b"b")]);
    assert!(matches!(err, BundleError::InvalidPath { .. }), "{err}");
}

#[test]
fn file_and_directory_sharing_a_path_is_rejected() {
    let err = error_of(vec![entry("notes", b"a"), entry("notes/inner.md", b"b")]);
    assert!(matches!(err, BundleError::PathConflict { .. }), "{err}");
}

#[test]
fn reserved_and_unportable_names_are_rejected() {
    for path in [
        "CON.txt",
        "refs/aux",
        "refs/com1.md",
        "LPT9",
        "a\\b.md",
        "bad:name.md",
        "trailing.",
        "sp ",
        "q?.md",
    ] {
        let err = error_of(vec![entry(path, b"x")]);
        assert!(
            matches!(err, BundleError::ReservedName(_) | BundleError::InvalidPath { .. }),
            "{path}: {err}"
        );
    }
    let err = error_of(vec![entry("tab\tname.md", b"x")]);
    assert!(matches!(err, BundleError::InvalidPath { .. }), "{err}");
    let long = "n".repeat(MAX_BUNDLE_COMPONENT_BYTES + 1);
    let err = error_of(vec![entry(&long, b"x")]);
    assert!(matches!(err, BundleError::InvalidPath { .. }), "{err}");
}

#[cfg(unix)]
#[test]
fn invalid_utf8_path_is_rejected() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let bad = BundleEntry {
        path: PathBuf::from(OsStr::from_bytes(b"ref/\xff\xfe.md")),
        bytes: b"x".to_vec(),
        executable: false,
    };
    let err = error_of(vec![bad]);
    assert!(matches!(err, BundleError::InvalidUtf8Path(_)), "{err}");
}

#[test]
fn oversized_and_excessive_content_is_rejected() {
    let big = vec![0u8; usize::try_from(MAX_BUNDLE_FILE_BYTES).expect("test: fits") + 1];
    let err = error_of(vec![entry("SKILL.md", &big)]);
    assert!(matches!(err, BundleError::FileTooLarge { .. }), "{err}");

    let chunk = vec![0u8; usize::try_from(MAX_BUNDLE_FILE_BYTES).expect("test: fits")];
    let many: Vec<BundleEntry> = (0..9).map(|i| entry(&format!("f{i}.bin"), &chunk)).collect();
    let err = error_of(many);
    assert!(matches!(err, BundleError::BundleTooLarge), "{err}");

    let files: Vec<BundleEntry> = (0..=MAX_BUNDLE_FILES)
        .map(|i| entry(&format!("f{i}.md"), b"x"))
        .collect();
    let err = error_of(files);
    assert!(matches!(err, BundleError::TooManyFiles), "{err}");

    let err = error_of(Vec::new());
    assert!(matches!(err, BundleError::Empty), "{err}");
}

#[cfg(unix)]
mod unix_tree {
    use super::*;

    fn tree() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().expect("test: tempdir");
        let root = tmp.path().join("bundle");
        fs::create_dir_all(&root).expect("test: mkdir");
        fs::write(root.join("SKILL.md"), md("S", "d")).expect("test: write");
        (tmp, root)
    }

    fn rejected(root: &std::path::Path) -> BundleError {
        Bundle::from_dir(root).expect_err("test: tree must be rejected")
    }

    #[test]
    fn symlinks_are_rejected_even_when_they_point_inside() {
        let (tmp, root) = tree();
        std::os::unix::fs::symlink(root.join("SKILL.md"), root.join("alias.md")).expect("test: symlink");
        assert!(matches!(rejected(&root), BundleError::Symlink(_)));
        fs::remove_file(root.join("alias.md")).expect("test: rm");

        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).expect("test: mkdir");
        fs::write(outside.join("secret.txt"), "secret").expect("test: write");
        std::os::unix::fs::symlink(&outside, root.join("escape")).expect("test: symlink");
        assert!(matches!(rejected(&root), BundleError::Symlink(_)));
    }

    #[test]
    fn special_files_are_rejected() {
        let (_tmp, root) = tree();
        let _listener = std::os::unix::net::UnixListener::bind(root.join("sock")).expect("test: socket");
        assert!(matches!(rejected(&root), BundleError::UnsupportedFileType(_)));
    }

    #[test]
    fn empty_directories_and_non_directory_roots_are_rejected() {
        let (_tmp, root) = tree();
        fs::create_dir_all(root.join("empty")).expect("test: mkdir");
        assert!(matches!(rejected(&root), BundleError::EmptyDirectory(_)));
        assert!(matches!(
            rejected(&root.join("SKILL.md")),
            BundleError::UnsupportedFileType(_)
        ));
    }

    #[test]
    fn oversized_file_on_disk_is_rejected() {
        let (_tmp, root) = tree();
        let file = fs::File::create(root.join("huge.bin")).expect("test: create");
        file.set_len(MAX_BUNDLE_FILE_BYTES + 1).expect("test: set_len");
        assert!(matches!(rejected(&root), BundleError::FileTooLarge { .. }));
    }

    #[test]
    fn too_deep_trees_are_rejected() {
        let (_tmp, root) = tree();
        let deep = root.join("a/b/c/d/e/f/g/h/i");
        fs::create_dir_all(&deep).expect("test: mkdir");
        fs::write(deep.join("x.md"), "x").expect("test: write");
        assert!(matches!(rejected(&root), BundleError::InvalidPath { .. }));
    }
}

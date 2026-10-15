//! Canonical, versioned hashing of complete skill bundles.
//!
//! Canonical encoding, format version 1 (SHA-256 over the concatenation of):
//!
//! ```text
//! b"prx-skill-bundle\0"                       domain separator (17 bytes)
//! u32 big-endian                              format version (1)
//! u64 big-endian                              file count
//! per file, ordered by the UTF-8 path bytes ascending:
//!   u32 big-endian                            path length in bytes
//!   path bytes                                '/'-separated relative UTF-8 path
//!   u64 big-endian                            content length in bytes
//!   [u8; 32]                                  SHA-256 of the exact content bytes
//!   u8                                        1 when any execute bit is set, else 0
//! ```
//!
//! File contents are hashed byte for byte; Markdown and scripts are never
//! normalised. The revision id is `r1-` plus the lowercase hex digest.

use super::types::RevisionId;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

/// Version of the canonical encoding documented above.
pub const BUNDLE_FORMAT_VERSION: u32 = 1;
/// Maximum number of files in one bundle.
pub const MAX_BUNDLE_FILES: usize = 256;
/// Maximum size of one file.
pub const MAX_BUNDLE_FILE_BYTES: u64 = 1024 * 1024;
/// Maximum total size of all files in one bundle.
pub const MAX_BUNDLE_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
/// Maximum length of a relative path in bytes.
pub const MAX_BUNDLE_PATH_BYTES: usize = 240;
/// Maximum length of one path component in bytes.
pub const MAX_BUNDLE_COMPONENT_BYTES: usize = 100;
/// Maximum number of path components of a file.
pub const MAX_BUNDLE_DEPTH: usize = 8;

const DOMAIN: &[u8] = b"prx-skill-bundle\0";
const FORBIDDEN_NAME_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*', '\\'];
const RESERVED_DEVICE_NAMES: &[&str] = &["CON", "PRN", "AUX", "NUL"];

#[derive(Debug, Error)]
pub enum BundleError {
    #[error("bundle has no files")]
    Empty,
    #[error("bundle has more than {MAX_BUNDLE_FILES} files")]
    TooManyFiles,
    #[error("duplicate path {0:?}")]
    DuplicatePath(String),
    #[error("path traversal in {0:?}")]
    Traversal(String),
    #[error("absolute path {0:?}")]
    AbsolutePath(String),
    #[error("path is not valid UTF-8: {0:?}")]
    InvalidUtf8Path(String),
    #[error("invalid path {path:?}: {reason}")]
    InvalidPath { path: String, reason: &'static str },
    #[error("reserved platform name in {0:?}")]
    ReservedName(String),
    #[error("paths {first:?} and {second:?} collide on a case-insensitive or normalising filesystem")]
    CaseCollision { first: String, second: String },
    #[error("path {file:?} is used both as a file and as a directory")]
    PathConflict { file: String },
    #[error("symlink {0:?} is not allowed in a managed bundle")]
    Symlink(String),
    #[error("unsupported file type at {0:?}")]
    UnsupportedFileType(String),
    #[error("empty directory {0:?} cannot be part of a bundle")]
    EmptyDirectory(String),
    #[error("file {path:?} exceeds {MAX_BUNDLE_FILE_BYTES} bytes")]
    FileTooLarge { path: String },
    #[error("bundle exceeds {MAX_BUNDLE_TOTAL_BYTES} bytes in total")]
    BundleTooLarge,
    #[error("i/o error at {path:?}: {message}")]
    Io { path: String, message: String },
}

/// One file offered for bundling. The path is untrusted and is validated.
#[derive(Clone, Debug)]
pub struct BundleEntry {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub executable: bool,
}

/// One validated file of a bundle.
#[derive(Clone, Debug)]
pub struct BundleFile {
    path: String,
    bytes: Vec<u8>,
    sha256: [u8; 32],
    executable: bool,
}

impl BundleFile {
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    #[must_use]
    pub const fn is_executable(&self) -> bool {
        self.executable
    }
}

/// A validated, complete skill bundle with its content identity.
#[derive(Clone, Debug)]
pub struct Bundle {
    files: Vec<BundleFile>,
    revision_id: RevisionId,
}

impl Bundle {
    /// Validates entries and computes the canonical identity. Input order is
    /// irrelevant: files are sorted by path before hashing.
    pub fn from_entries(entries: Vec<BundleEntry>) -> Result<Self, BundleError> {
        if entries.is_empty() {
            return Err(BundleError::Empty);
        }
        if entries.len() > MAX_BUNDLE_FILES {
            return Err(BundleError::TooManyFiles);
        }
        let mut total: u64 = 0;
        let mut by_path: BTreeMap<String, BundleFile> = BTreeMap::new();
        let mut collision_keys: BTreeMap<String, String> = BTreeMap::new();
        for entry in entries {
            let path = normalize_relative_path(&entry.path)?;
            let size = u64::try_from(entry.bytes.len()).unwrap_or(u64::MAX);
            if size > MAX_BUNDLE_FILE_BYTES {
                return Err(BundleError::FileTooLarge { path });
            }
            total = total.saturating_add(size);
            if total > MAX_BUNDLE_TOTAL_BYTES {
                return Err(BundleError::BundleTooLarge);
            }
            if by_path.contains_key(&path) {
                return Err(BundleError::DuplicatePath(path));
            }
            let key = collision_key(&path);
            if let Some(first) = collision_keys.get(&key) {
                return Err(BundleError::CaseCollision {
                    first: first.clone(),
                    second: path,
                });
            }
            collision_keys.insert(key, path.clone());
            let sha256: [u8; 32] = Sha256::digest(&entry.bytes).into();
            by_path.insert(
                path.clone(),
                BundleFile {
                    path,
                    bytes: entry.bytes,
                    sha256,
                    executable: entry.executable,
                },
            );
        }
        reject_file_directory_conflicts(&by_path)?;
        let files: Vec<BundleFile> = by_path.into_values().collect();
        let revision_id = RevisionId::from_digest(&canonical_digest(&files));
        Ok(Self { files, revision_id })
    }

    /// Reads a directory tree into a bundle. Symlinks, special files, empty
    /// directories and oversized content are rejected without following
    /// links.
    pub fn from_dir(root: &Path) -> Result<Self, BundleError> {
        let meta = fs::symlink_metadata(root).map_err(|e| io_error(root, &e))?;
        if meta.file_type().is_symlink() {
            return Err(BundleError::Symlink(display(root)));
        }
        if !meta.is_dir() {
            return Err(BundleError::UnsupportedFileType(display(root)));
        }
        let mut entries = Vec::new();
        let mut total: u64 = 0;
        collect_dir(root, Path::new(""), 0, &mut entries, &mut total)?;
        Self::from_entries(entries)
    }

    #[must_use]
    pub const fn revision_id(&self) -> &RevisionId {
        &self.revision_id
    }

    /// Files in canonical (path byte) order.
    #[must_use]
    pub fn files(&self) -> &[BundleFile] {
        &self.files
    }

    #[must_use]
    pub fn file(&self, path: &str) -> Option<&BundleFile> {
        self.files.iter().find(|file| file.path == path)
    }
}

fn canonical_digest(files: &[BundleFile]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    hasher.update(BUNDLE_FORMAT_VERSION.to_be_bytes());
    hasher.update(u64::try_from(files.len()).unwrap_or(u64::MAX).to_be_bytes());
    for file in files {
        hasher.update(u32::try_from(file.path.len()).unwrap_or(u32::MAX).to_be_bytes());
        hasher.update(file.path.as_bytes());
        hasher.update(u64::try_from(file.bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(file.sha256);
        hasher.update([u8::from(file.executable)]);
    }
    hasher.finalize().into()
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn io_error(path: &Path, error: &std::io::Error) -> BundleError {
    BundleError::Io {
        path: display(path),
        message: error.to_string(),
    }
}

fn collision_key(path: &str) -> String {
    path.nfc().collect::<String>().to_lowercase()
}

fn reject_file_directory_conflicts(files: &BTreeMap<String, BundleFile>) -> Result<(), BundleError> {
    let file_keys: BTreeSet<String> = files.keys().map(|path| collision_key(path)).collect();
    for path in files.keys() {
        let mut prefix = String::new();
        let mut segments = path.split('/').peekable();
        while let Some(segment) = segments.next() {
            if segments.peek().is_none() {
                break;
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            if file_keys.contains(&collision_key(&prefix)) {
                return Err(BundleError::PathConflict { file: prefix });
            }
        }
    }
    Ok(())
}

/// Validates an untrusted relative path and returns its canonical form.
pub(super) fn normalize_relative_path(path: &Path) -> Result<String, BundleError> {
    let lossy = display(path);
    let raw = path
        .to_str()
        .ok_or_else(|| BundleError::InvalidUtf8Path(lossy.clone()))?;
    if raw.is_empty() {
        return Err(BundleError::InvalidPath {
            path: lossy,
            reason: "empty path",
        });
    }
    let mut names: Vec<&str> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                let name = name
                    .to_str()
                    .ok_or_else(|| BundleError::InvalidUtf8Path(lossy.clone()))?;
                validate_component(name, &lossy)?;
                names.push(name);
            }
            Component::ParentDir => return Err(BundleError::Traversal(lossy)),
            Component::RootDir | Component::Prefix(_) => return Err(BundleError::AbsolutePath(lossy)),
            Component::CurDir => {
                return Err(BundleError::InvalidPath {
                    path: lossy,
                    reason: "'.' segment",
                });
            }
        }
    }
    if names.len() > MAX_BUNDLE_DEPTH {
        return Err(BundleError::InvalidPath {
            path: lossy,
            reason: "path is too deep",
        });
    }
    let joined = names.join("/");
    if joined != raw {
        return Err(BundleError::InvalidPath {
            path: lossy,
            reason: "path is not in canonical form",
        });
    }
    if joined.len() > MAX_BUNDLE_PATH_BYTES {
        return Err(BundleError::InvalidPath {
            path: lossy,
            reason: "path is too long",
        });
    }
    Ok(joined)
}

fn validate_component(name: &str, whole: &str) -> Result<(), BundleError> {
    let invalid = |reason: &'static str| BundleError::InvalidPath {
        path: whole.to_string(),
        reason,
    };
    if name.len() > MAX_BUNDLE_COMPONENT_BYTES {
        return Err(invalid("path component is too long"));
    }
    if name
        .chars()
        .any(|c| c.is_control() || FORBIDDEN_NAME_CHARS.contains(&c))
    {
        return Err(invalid("path component has a control or platform-reserved character"));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(invalid("path component ends with a dot or space"));
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let numbered_device = ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix)
            .is_some_and(|n| matches!(n.len(), 1) && n != "0" && n.bytes().all(|b| b.is_ascii_digit()))
    });
    if RESERVED_DEVICE_NAMES.contains(&stem.as_str()) || numbered_device {
        return Err(BundleError::ReservedName(whole.to_string()));
    }
    Ok(())
}

fn collect_dir(
    dir: &Path,
    rel: &Path,
    depth: usize,
    entries: &mut Vec<BundleEntry>,
    total: &mut u64,
) -> Result<(), BundleError> {
    if depth > MAX_BUNDLE_DEPTH {
        return Err(BundleError::InvalidPath {
            path: display(rel),
            reason: "path is too deep",
        });
    }
    let mut children: Vec<(OsString, fs::FileType)> = Vec::new();
    for item in fs::read_dir(dir).map_err(|e| io_error(dir, &e))? {
        let item = item.map_err(|e| io_error(dir, &e))?;
        let file_type = item.file_type().map_err(|e| io_error(&item.path(), &e))?;
        children.push((item.file_name(), file_type));
        if children.len() > MAX_BUNDLE_FILES {
            return Err(BundleError::TooManyFiles);
        }
    }
    if children.is_empty() && depth > 0 {
        return Err(BundleError::EmptyDirectory(display(rel)));
    }
    children.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, file_type) in children {
        if name.to_str().is_none() {
            return Err(BundleError::InvalidUtf8Path(display(&rel.join(&name))));
        }
        let child_rel = rel.join(&name);
        let child = dir.join(&name);
        if file_type.is_symlink() {
            return Err(BundleError::Symlink(display(&child_rel)));
        } else if file_type.is_dir() {
            collect_dir(&child, &child_rel, depth + 1, entries, total)?;
        } else if file_type.is_file() {
            entries.push(read_file(&child, &child_rel, total)?);
            if entries.len() > MAX_BUNDLE_FILES {
                return Err(BundleError::TooManyFiles);
            }
        } else {
            return Err(BundleError::UnsupportedFileType(display(&child_rel)));
        }
    }
    Ok(())
}

fn read_file(path: &Path, rel: &Path, total: &mut u64) -> Result<BundleEntry, BundleError> {
    let rel_text = display(rel);
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Never follow a link swapped in after the directory scan, and never
        // block on a special file swapped in after it.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path).map_err(|e| io_error(path, &e))?;
    let meta = file.metadata().map_err(|e| io_error(path, &e))?;
    if !meta.is_file() {
        return Err(BundleError::UnsupportedFileType(rel_text));
    }
    if meta.len() > MAX_BUNDLE_FILE_BYTES {
        return Err(BundleError::FileTooLarge { path: rel_text });
    }
    *total = total.saturating_add(meta.len());
    if *total > MAX_BUNDLE_TOTAL_BYTES {
        return Err(BundleError::BundleTooLarge);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    (&mut file)
        .take(MAX_BUNDLE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error(path, &e))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_BUNDLE_FILE_BYTES {
        return Err(BundleError::FileTooLarge { path: rel_text });
    }
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    Ok(BundleEntry {
        path: rel.to_path_buf(),
        bytes,
        executable,
    })
}

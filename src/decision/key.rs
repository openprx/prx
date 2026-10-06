//! The per-workspace decision key (CDP-PLAN 1.2, mismatch M15).
//!
//! One 32-byte random key, stored at
//! `<workspace>/self/causal_tree/.decision_key` with mode `0600`, keys every
//! HMAC the decision plane computes: wire-id salting, state fingerprints, the
//! v1 feedback import and the exploration seed. Each use has its own
//! [`KeyDomain`], so a value computed for one purpose never equals a value for
//! another.

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hmac::digest::KeyInit;
use hmac::{Hmac, Mac as _};
use sha2::Sha256;

use super::error::DecisionError;

/// Length of the key in bytes.
const KEY_LEN: usize = 32;

/// The HMAC-SHA256 block size. A 32-byte key padded with zeros to one block
/// is, per RFC 2104, the same HMAC key as the 32 bytes themselves; the padded
/// form lets the fixed-size constructor, which cannot fail, be used.
const HMAC_BLOCK_LEN: usize = 64;

/// The key file, relative to the workspace directory.
const KEY_RELATIVE_PATH: [&str; 3] = ["self", "causal_tree", ".decision_key"];

/// What an HMAC is computed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyDomain {
    /// Private binding of a priced physical decision transport.
    AttemptAccounting,
    /// Question ids sent to an HTTP backend.
    WireSalt,
    /// Keyed fingerprints of the causal state.
    StateFingerprint,
    /// The seed of read-only exploration.
    ExplorationSeed,
    /// Private binding for a durable terminal decision claim.
    TerminalClaim,
    /// Authenticated pending approval metadata and original-input binding.
    ApprovalState,
    /// Ownership and content bindings for persisted evaluation reports.
    ReportOwnership,
    /// Durable ingress reception and interruption evidence.
    IngressRecovery,
}

impl KeyDomain {
    /// The domain-separation label mixed into every HMAC.
    const fn label(self) -> &'static [u8] {
        match self {
            Self::AttemptAccounting => b"prx.decision.attempt_accounting.v1",
            Self::WireSalt => b"prx.decision.wire_salt.v1",
            Self::StateFingerprint => b"prx.decision.state_fingerprint.v1",
            Self::ExplorationSeed => b"prx.decision.exploration_seed.v1",
            Self::TerminalClaim => b"prx.decision.terminal_claim.v1",
            Self::ApprovalState => b"prx.decision.approval_state.v1",
            Self::ReportOwnership => b"prx.decision.report_ownership.v1",
            Self::IngressRecovery => b"prx.decision.ingress_recovery.v1",
        }
    }
}

/// The workspace decision key. It never leaves the process: `Debug` redacts
/// it and there is no accessor for the bytes.
pub struct DecisionKey {
    block: [u8; HMAC_BLOCK_LEN],
}

impl fmt::Debug for DecisionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DecisionKey([REDACTED])")
    }
}

impl DecisionKey {
    /// A generation-local key for builtin decisions when the persistent
    /// workspace key must not be read or is unavailable. Its fingerprints
    /// and seals must never be persisted or handed to another process.
    pub(crate) fn ephemeral() -> Arc<Self> {
        Arc::new(Self::from_bytes(&rand::random::<[u8; KEY_LEN]>()))
    }

    /// Load existing authority without creating a key or repairing permissions.
    /// Missing or unsafe state cannot be replaced during worker admission.
    pub(crate) fn load_existing(workspace_dir: &Path) -> Result<Arc<Self>, DecisionError> {
        let path = key_path(workspace_dir);
        inspect_key_dirs(workspace_dir, false).map_err(|reason| unavailable(&path, &reason))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            let owner = effective_uid().map_err(|reason| unavailable(&path, &reason))?;
            for (dir, forbidden) in [
                (workspace_dir.join(KEY_RELATIVE_PATH[0]), 0o022),
                (
                    workspace_dir.join(KEY_RELATIVE_PATH[0]).join(KEY_RELATIVE_PATH[1]),
                    0o077,
                ),
            ] {
                let metadata = std::fs::symlink_metadata(&dir)
                    .map_err(|_| unavailable(&path, "existing decision key directory is missing"))?;
                if metadata.file_type().is_symlink()
                    || !metadata.is_dir()
                    || metadata.uid() != owner
                    || metadata.permissions().mode() & forbidden != 0
                {
                    return Err(unavailable(&path, "existing decision key directory is unsafe"));
                }
            }
        }
        let bytes = read_key_with_permissions(&path, false)
            .map_err(|reason| unavailable(&path, &reason))?
            .ok_or_else(|| unavailable(&path, "the existing decision key is missing"))?;
        Ok(Arc::new(Self::from_bytes(&bytes)))
    }

    /// Load the workspace key, creating it (32 random bytes, mode `0600`)
    /// when it does not exist yet.
    ///
    /// A key file that is a symlink, not a regular file, or not exactly 32
    /// bytes long is refused rather than replaced: replacing it would silently
    /// invalidate every fingerprint already recorded. A key file with wider
    /// permissions is narrowed back to `0600`, and the key directory to
    /// `0700`; a symlinked key directory, or a `self` directory writable by
    /// group or others, is refused.
    ///
    /// Key rotation is out of scope (the CDP text does not require it): a
    /// refused key leaves the plane unavailable until an operator restores or
    /// removes the file, and callers fall back to the builtin backend.
    ///
    /// Every failure is reported as [`DecisionError::Unavailable`] (the
    /// decision plane cannot run without its key); the cause is logged with
    /// the path but never with key material.
    pub fn load_or_create(workspace_dir: &Path) -> Result<Arc<Self>, DecisionError> {
        let path = key_path(workspace_dir);
        if let Err(reason) = check_key_dirs(workspace_dir) {
            return Err(unavailable(&path, &reason));
        }
        match read_key(&path) {
            Ok(Some(bytes)) => return Ok(Arc::new(Self::from_bytes(&bytes))),
            Ok(None) => {}
            Err(reason) => return Err(unavailable(&path, &reason)),
        }
        let bytes: [u8; KEY_LEN] = rand::random();
        match create_key(&path, &bytes) {
            Ok(()) => Ok(Arc::new(Self::from_bytes(&bytes))),
            Err(CreateError::AlreadyExists) => match read_key(&path) {
                // Another process created the key first; use its key.
                Ok(Some(existing)) => Ok(Arc::new(Self::from_bytes(&existing))),
                Ok(None) => Err(unavailable(&path, "the key file vanished after a concurrent create")),
                Err(reason) => Err(unavailable(&path, &reason)),
            },
            Err(CreateError::Io(reason)) => Err(unavailable(&path, &reason)),
        }
    }

    /// Derive a task-scoped ordinary-worker signing domain from the already
    /// resolved transport key. This does not read/create a workspace decision
    /// key, serialize key material, or authenticate an incoming manifest.
    /// Receivers must verify the outer worker capability before using it.
    pub(crate) fn from_worker_channel(
        transport_key: &ring::hmac::Key,
        workspace_id: &str,
        run_id: &str,
        config_generation: &str,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            !workspace_id.trim().is_empty() && !run_id.trim().is_empty() && !config_generation.trim().is_empty(),
            "ordinary worker signing identity is incomplete"
        );
        let mut context = ring::hmac::Context::with_key(transport_key);
        for part in [
            b"prx-ordinary-worker-source-key-v1".as_slice(),
            workspace_id.as_bytes(),
            run_id.as_bytes(),
            config_generation.as_bytes(),
        ] {
            context.update(&(part.len() as u64).to_be_bytes());
            context.update(part);
        }
        let tag = context.sign();
        let bytes: [u8; KEY_LEN] = tag
            .as_ref()
            .try_into()
            .map_err(|_| anyhow::anyhow!("ordinary worker signing key has invalid length"))?;
        Ok(Arc::new(Self::from_bytes(&bytes)))
    }

    fn from_bytes(bytes: &[u8; KEY_LEN]) -> Self {
        let mut block = [0_u8; HMAC_BLOCK_LEN];
        block[..KEY_LEN].copy_from_slice(bytes);
        Self { block }
    }

    /// HMAC-SHA256 over `parts` in `domain`. Each part is length-prefixed, so
    /// no two different part lists produce the same input.
    #[must_use]
    pub fn hmac(&self, domain: KeyDomain, parts: &[&[u8]]) -> [u8; 32] {
        self.mac(domain, parts).finalize().into_bytes().into()
    }

    /// Verify a durable binding using the MAC implementation's constant-time check.
    pub(crate) fn verify_hmac(&self, domain: KeyDomain, parts: &[&[u8]], tag: &[u8]) -> bool {
        self.mac(domain, parts).verify_slice(tag).is_ok()
    }

    fn mac(&self, domain: KeyDomain, parts: &[&[u8]]) -> Hmac<Sha256> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new(&self.block.into());
        let label = domain.label();
        mac.update(&(label.len() as u64).to_be_bytes());
        mac.update(label);
        for part in parts {
            mac.update(&(part.len() as u64).to_be_bytes());
            mac.update(part);
        }
        mac
    }
}

fn key_path(workspace_dir: &Path) -> PathBuf {
    KEY_RELATIVE_PATH
        .iter()
        .fold(workspace_dir.to_path_buf(), |path, part| path.join(part))
}

fn unavailable(path: &Path, reason: &str) -> DecisionError {
    tracing::error!(path = %path.display(), reason, "decision key is unavailable");
    DecisionError::Unavailable
}

/// The key bytes, `None` when no key file exists, or why the file is unusable.
///
/// The file is opened once without following a symlink (`O_NOFOLLOW` on
/// Unix), and every later check (regular file, exact length, permissions)
/// and the read itself use that open handle, so a path swapped after the
/// check is never read (D-057 L-6). The length is checked before reading,
/// and exactly [`KEY_LEN`] bytes are read.
fn read_key(path: &Path) -> Result<Option<[u8; KEY_LEN]>, String> {
    read_key_with_permissions(path, true)
}

fn read_key_with_permissions(path: &Path, fix: bool) -> Result<Option<[u8; KEY_LEN]>, String> {
    use std::io::Read as _;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // `O_NONBLOCK` keeps a FIFO (or another special file) planted at the
        // key path from blocking the open; the type is checked on the handle
        // right after (D-057 amendment 2, N-3). It has no effect on a
        // regular file.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot open the key file without following links: {error}")),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect the key file: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err("the key file is not a regular file".to_string());
    }
    if metadata.len() != KEY_LEN as u64 {
        return Err(format!(
            "the key file holds {} bytes, expected {KEY_LEN}",
            metadata.len()
        ));
    }
    if fix {
        restrict_file(&file, &metadata, path)?;
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            if metadata.permissions().mode() & 0o077 != 0 || metadata.uid() != effective_uid()? {
                return Err("existing decision key ownership or permissions are unsafe".to_owned());
            }
        }
    }
    let mut bytes = [0_u8; KEY_LEN];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("cannot read the key file: {error}"))?;
    Ok(Some(bytes))
}

/// Check the key's directories before the key is read or created:
/// `<workspace>/self` and `<workspace>/self/causal_tree` must be plain
/// directories (not symlinks) when they exist. `causal_tree` is narrowed to
/// `0700` (D-057 L-6). `self` is shared with other subsystems that create it
/// under the process umask, so (D-057 amendment 2, N-5) a `self` that is
/// group-writable is narrowed with a warning, and it is refused only when
/// another user owns it or it is world-writable: whoever can write it can
/// replace the key directory.
fn check_key_dirs(workspace_dir: &Path) -> Result<(), String> {
    inspect_key_dirs(workspace_dir, true)
}

/// [`check_key_dirs`]; with `fix` off, a directory that is too open is not
/// narrowed (a read-only caller changes nothing, D-093 N-6).
fn inspect_key_dirs(workspace_dir: &Path, fix: bool) -> Result<(), String> {
    let self_dir = workspace_dir.join(KEY_RELATIVE_PATH[0]);
    let key_dir = self_dir.join(KEY_RELATIVE_PATH[1]);
    for (dir, narrow) in [(self_dir, false), (key_dir, true)] {
        let metadata = match std::fs::symlink_metadata(&dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("cannot inspect {}: {error}", dir.display())),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!("{} is not a plain directory", dir.display()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = metadata.permissions().mode();
            if fix && narrow && mode & 0o077 != 0 {
                tracing::warn!(path = %dir.display(), "decision key directory was open to others; narrowing it to 0700");
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                    .map_err(|error| format!("cannot restrict {}: {error}", dir.display()))?;
            }
            if !narrow {
                use std::os::unix::fs::MetadataExt as _;
                if metadata.uid() != effective_uid()? {
                    return Err(format!("{} is owned by another user", dir.display()));
                }
                if mode & 0o002 != 0 {
                    return Err(format!("{} is writable by every user", dir.display()));
                }
                if fix && mode & 0o020 != 0 {
                    tracing::warn!(path = %dir.display(), "workspace self directory was group-writable; removing group write");
                    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode & 0o7757))
                        .map_err(|error| format!("cannot restrict {}: {error}", dir.display()))?;
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = narrow;
        }
    }
    Ok(())
}

/// The effective user id of this process. `std` exposes no `geteuid`, and
/// the crate forbids `unsafe`; a file this process creates is owned by its
/// effective user id, so the owner of an anonymous temporary file (unlinked
/// at creation, nothing left behind) is that id.
#[cfg(unix)]
fn effective_uid() -> Result<u32, String> {
    use std::os::unix::fs::MetadataExt as _;
    tempfile::tempfile()
        .and_then(|probe| probe.metadata())
        .map(|metadata| metadata.uid())
        .map_err(|error| format!("cannot determine the effective user: {error}"))
}

enum CreateError {
    AlreadyExists,
    Io(String),
}

/// Write the key to a private temporary file, then link it into place. The
/// link fails when a key already exists, so a concurrent creator never
/// overwrites the key another process is already using, and a crash never
/// leaves a truncated key behind.
fn create_key(path: &Path, bytes: &[u8; KEY_LEN]) -> Result<(), CreateError> {
    let parent = path
        .parent()
        .ok_or_else(|| CreateError::Io("the key path has no parent directory".to_string()))?;
    create_private_dir(parent).map_err(CreateError::Io)?;
    let temp = parent.join(format!(".decision_key.tmp-{}", uuid::Uuid::new_v4()));
    let written = write_private_file(&temp, bytes).and_then(|()| match std::fs::hard_link(&temp, path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(format!("cannot link the key file into place: {error}")),
    });
    if let Err(error) = std::fs::remove_file(&temp) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = %temp.display(), %error, "could not remove the temporary decision key file");
        }
    }
    match written {
        Ok(true) => sync_dir(parent).map_err(CreateError::Io),
        Ok(false) => Err(CreateError::AlreadyExists),
        Err(reason) => Err(CreateError::Io(reason)),
    }
}

/// Check the decision plane's directories (`self` and `self/causal_tree`,
/// as for the key) without creating or changing anything, not even a mode:
/// `Ok(false)` when they do not exist yet.
///
/// # Errors
///
/// Why a directory is unsafe (a symlink, not a directory, owned by another
/// user, writable by every user).
pub fn check_decision_dir(workspace_dir: &Path) -> Result<bool, String> {
    inspect_key_dirs(workspace_dir, false)?;
    Ok(workspace_dir
        .join(KEY_RELATIVE_PATH[0])
        .join(KEY_RELATIVE_PATH[1])
        .is_dir())
}

/// The decision plane's directory (`<workspace>/self/causal_tree`), created
/// `0700` when missing, after and again before use the same checks as the
/// key's directory: never a symlink, never another user's.
///
/// # Errors
///
/// Why the directory is unsafe or cannot be created.
pub fn prepare_decision_dir(workspace_dir: &Path) -> Result<PathBuf, String> {
    check_key_dirs(workspace_dir)?;
    let dir = workspace_dir.join(KEY_RELATIVE_PATH[0]).join(KEY_RELATIVE_PATH[1]);
    create_private_dir(&dir)?;
    check_key_dirs(workspace_dir)?;
    Ok(dir)
}

fn create_private_dir(dir: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// Narrow the key file to `0600` (a copy or restore of the workspace can
/// recreate it with the default umask).
fn restrict_file(file: &std::fs::File, metadata: &std::fs::Metadata, path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            tracing::warn!(path = %path.display(), "decision key file was readable by others; narrowing it to 0600");
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("cannot restrict the key file: {error}"))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (file, metadata, path);
    }
    Ok(())
}

fn sync_dir(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)
            .and_then(|handle| handle.sync_all())
            .map_err(|error| format!("cannot sync {}: {error}", dir.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_created_once_private_and_reused() {
        let workspace = tempfile::tempdir().expect("test: tempdir");
        let first = DecisionKey::load_or_create(workspace.path()).expect("test: create key");
        let second = DecisionKey::load_or_create(workspace.path()).expect("test: load key");
        let path = workspace.path().join("self/causal_tree/.decision_key");
        assert_eq!(std::fs::read(&path).expect("test: read key").len(), KEY_LEN);
        assert_eq!(
            first.hmac(KeyDomain::WireSalt, &[b"a"]),
            second.hmac(KeyDomain::WireSalt, &[b"a"])
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).expect("test: stat key").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dir_mode = std::fs::metadata(path.parent().expect("test: parent"))
                .expect("test: stat dir")
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700);
        }
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().expect("test: parent"))
            .expect("test: list dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "the temporary key file must be removed");
    }

    #[test]
    fn domains_and_part_boundaries_separate_every_hmac() {
        let workspace = tempfile::tempdir().expect("test: tempdir");
        let key = DecisionKey::load_or_create(workspace.path()).expect("test: key");
        let wire = key.hmac(KeyDomain::WireSalt, &[b"ab", b"c"]);
        assert_ne!(wire, key.hmac(KeyDomain::StateFingerprint, &[b"ab", b"c"]));
        assert_ne!(wire, key.hmac(KeyDomain::WireSalt, &[b"a", b"bc"]));
        assert_ne!(wire, key.hmac(KeyDomain::WireSalt, &[b"abc"]));
        let other = tempfile::tempdir().expect("test: tempdir");
        let other_key = DecisionKey::load_or_create(other.path()).expect("test: key");
        assert_ne!(wire, other_key.hmac(KeyDomain::WireSalt, &[b"ab", b"c"]));
    }

    #[test]
    fn the_padded_block_key_equals_the_variable_length_hmac() {
        let workspace = tempfile::tempdir().expect("test: tempdir");
        let key = DecisionKey::load_or_create(workspace.path()).expect("test: key");
        let bytes = std::fs::read(workspace.path().join("self/causal_tree/.decision_key")).expect("test: read");
        let mut reference = <Hmac<Sha256> as KeyInit>::new_from_slice(&bytes).expect("test: hmac key");
        let label = KeyDomain::StateFingerprint.label();
        reference.update(&(label.len() as u64).to_be_bytes());
        reference.update(label);
        reference.update(&3_u64.to_be_bytes());
        reference.update(b"xyz");
        let expected: [u8; 32] = reference.finalize().into_bytes().into();
        assert_eq!(key.hmac(KeyDomain::StateFingerprint, &[b"xyz"]), expected);
    }

    #[test]
    fn a_corrupt_or_symlinked_key_is_refused_not_replaced() {
        let workspace = tempfile::tempdir().expect("test: tempdir");
        let path = workspace.path().join("self/causal_tree/.decision_key");
        std::fs::create_dir_all(path.parent().expect("test: parent")).expect("test: mkdir");
        std::fs::write(&path, b"short").expect("test: write");
        assert_eq!(
            DecisionKey::load_or_create(workspace.path()).expect_err("test: short key"),
            DecisionError::Unavailable
        );
        assert_eq!(std::fs::read(&path).expect("test: read"), b"short");
        #[cfg(unix)]
        {
            let other = tempfile::tempdir().expect("test: tempdir");
            let link = other.path().join("self/causal_tree/.decision_key");
            std::fs::create_dir_all(link.parent().expect("test: parent")).expect("test: mkdir");
            std::os::unix::fs::symlink(&path, &link).expect("test: symlink");
            assert_eq!(
                DecisionKey::load_or_create(other.path()).expect_err("test: symlink"),
                DecisionError::Unavailable
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_widened_key_file_is_narrowed_back_to_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let workspace = tempfile::tempdir().expect("test: tempdir");
        DecisionKey::load_or_create(workspace.path()).expect("test: create");
        let path = workspace.path().join("self/causal_tree/.decision_key");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("test: chmod");
        DecisionKey::load_or_create(workspace.path()).expect("test: reload");
        let mode = std::fs::metadata(&path).expect("test: stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn the_key_directories_are_checked_before_the_key_is_read() {
        use std::os::unix::fs::PermissionsExt as _;
        let workspace = tempfile::tempdir().expect("test: tempdir");
        DecisionKey::load_or_create(workspace.path()).expect("test: create");
        let self_dir = workspace.path().join("self");
        let key_dir = self_dir.join("causal_tree");

        std::fs::set_permissions(&key_dir, std::fs::Permissions::from_mode(0o755)).expect("test: chmod");
        DecisionKey::load_or_create(workspace.path()).expect("test: narrowed");
        let mode = std::fs::metadata(&key_dir).expect("test: stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        // D-057 amendment 2 (N-5): a group-writable self/ owned by this user
        // (umask 002) is narrowed, not refused.
        std::fs::set_permissions(&self_dir, std::fs::Permissions::from_mode(0o775)).expect("test: chmod");
        DecisionKey::load_or_create(workspace.path()).expect("test: group-writable self is narrowed");
        let mode = std::fs::metadata(&self_dir).expect("test: stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o755);

        std::fs::set_permissions(&self_dir, std::fs::Permissions::from_mode(0o777)).expect("test: chmod");
        assert_eq!(
            DecisionKey::load_or_create(workspace.path()).expect_err("test: writable self"),
            DecisionError::Unavailable
        );
        std::fs::set_permissions(&self_dir, std::fs::Permissions::from_mode(0o700)).expect("test: chmod");

        let other = tempfile::tempdir().expect("test: tempdir");
        std::fs::create_dir(other.path().join("self")).expect("test: mkdir");
        std::fs::set_permissions(other.path().join("self"), std::fs::Permissions::from_mode(0o700))
            .expect("test: chmod");
        std::os::unix::fs::symlink(&key_dir, other.path().join("self/causal_tree")).expect("test: symlink");
        assert_eq!(
            DecisionKey::load_or_create(other.path()).expect_err("test: symlinked key dir"),
            DecisionError::Unavailable
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_well_formed_key_is_not_followed() {
        let elsewhere = tempfile::tempdir().expect("test: tempdir");
        let planted = elsewhere.path().join("planted_key");
        std::fs::write(&planted, [7_u8; KEY_LEN]).expect("test: write planted key");
        let workspace = tempfile::tempdir().expect("test: tempdir");
        let key_dir = workspace.path().join("self/causal_tree");
        create_private_dir(&key_dir).expect("test: key dir");
        std::os::unix::fs::symlink(&planted, key_dir.join(".decision_key")).expect("test: symlink");
        assert_eq!(
            DecisionKey::load_or_create(workspace.path()).expect_err("test: symlinked key"),
            DecisionError::Unavailable
        );
        assert_eq!(std::fs::read(&planted).expect("test: read"), [7_u8; KEY_LEN]);
    }

    #[test]
    fn an_oversized_key_file_is_refused_before_it_is_read() {
        let workspace = tempfile::tempdir().expect("test: tempdir");
        let path = workspace.path().join("self/causal_tree/.decision_key");
        std::fs::create_dir_all(path.parent().expect("test: parent")).expect("test: mkdir");
        std::fs::write(&path, vec![7_u8; 1024 * 1024]).expect("test: write");
        assert_eq!(
            DecisionKey::load_or_create(workspace.path()).expect_err("test: oversized key"),
            DecisionError::Unavailable
        );
        assert_eq!(std::fs::metadata(&path).expect("test: stat").len(), 1024 * 1024);
    }

    #[test]
    fn debug_never_shows_key_material() {
        let workspace = tempfile::tempdir().expect("test: tempdir");
        let key = DecisionKey::load_or_create(workspace.path()).expect("test: key");
        assert_eq!(format!("{key:?}"), "DecisionKey([REDACTED])");
    }

    #[test]
    fn ephemeral_keys_need_no_workspace_and_do_not_share_fingerprints() {
        let workspace = tempfile::tempdir().expect("test: tempdir");
        let first = DecisionKey::ephemeral();
        let repeated = Arc::clone(&first);
        let second = DecisionKey::ephemeral();
        let parts: &[&[u8]] = &[b"prx_turn"];
        let fingerprint = first.hmac(KeyDomain::StateFingerprint, parts);
        assert_eq!(fingerprint, repeated.hmac(KeyDomain::StateFingerprint, parts));
        assert_ne!(fingerprint, second.hmac(KeyDomain::StateFingerprint, parts));
        assert!(!workspace.path().join("self").exists());
    }
}

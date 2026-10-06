//! Audit logging for security events

use crate::config::AuditConfig;
use anyhow::Result;
use chrono::{DateTime, Utc};
use fs2::FileExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Weak};
use uuid::Uuid;

/// Audit event types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventType {
    CommandExecution,
    FileAccess,
    ConfigChange,
    AuthSuccess,
    AuthFailure,
    PolicyViolation,
    SecurityEvent,
    ToolGate,
}

/// Actor information (who performed the action)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub channel: String,
    pub user_id: Option<String>,
    pub username: Option<String>,
}

/// Action information (what was done)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub command: Option<String>,
    pub risk_level: Option<String>,
    pub approved: bool,
    pub allowed: bool,
}

/// Execution result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
}

/// Security context
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityContext {
    pub policy_violation: bool,
    pub rate_limit_remaining: Option<u32>,
}

/// Complete audit event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub timestamp: DateTime<Utc>,
    pub event_id: String,
    pub event_type: AuditEventType,
    pub actor: Option<Actor>,
    pub action: Option<Action>,
    pub result: Option<ExecutionResult>,
    pub security: SecurityContext,
}

impl AuditEvent {
    /// Create a new audit event
    pub fn new(event_type: AuditEventType) -> Self {
        Self {
            timestamp: Utc::now(),
            event_id: Uuid::new_v4().to_string(),
            event_type,
            actor: None,
            action: None,
            result: None,
            security: SecurityContext {
                policy_violation: false,
                rate_limit_remaining: None,
            },
        }
    }

    /// Set the actor
    pub fn with_actor(mut self, channel: String, user_id: Option<String>, username: Option<String>) -> Self {
        self.actor = Some(Actor {
            channel,
            user_id,
            username,
        });
        self
    }

    /// Set the action
    pub fn with_action(mut self, command: String, risk_level: String, approved: bool, allowed: bool) -> Self {
        self.action = Some(Action {
            command: Some(command),
            risk_level: Some(risk_level),
            approved,
            allowed,
        });
        self
    }

    /// Set the result
    pub fn with_result(
        mut self,
        success: bool,
        exit_code: Option<i32>,
        duration_ms: u64,
        error: Option<String>,
    ) -> Self {
        self.result = Some(ExecutionResult {
            success,
            exit_code,
            duration_ms: Some(duration_ms),
            error,
        });
        self
    }
}

type AuditPathLock = Arc<Mutex<()>>;

static AUDIT_PATH_LOCKS: LazyLock<Mutex<std::collections::HashMap<PathBuf, Weak<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

fn shared_audit_path_lock(path: &Path) -> AuditPathLock {
    let key = path
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .and_then(|parent| path.file_name().map(|name| parent.join(name)))
        .unwrap_or_else(|| path.to_path_buf());
    let mut locks = AUDIT_PATH_LOCKS.lock();
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

/// Audit logger
pub struct AuditLogger {
    /// The directory the audit log must stay inside.
    base: PathBuf,
    /// The configured log inside `base`, or `None` when the configured path
    /// could land outside it: every record is then skipped (D-055).
    target: Option<AuditTarget>,
    max_size_mb: u32,
}

/// A confined audit log path and the in-process lock for it.
struct AuditTarget {
    log_path: PathBuf,
    writer_lock: AuditPathLock,
}

/// Structured command execution details for audit logging.
#[derive(Debug, Clone)]
pub struct CommandExecutionLog<'a> {
    pub channel: &'a str,
    pub command: &'a str,
    pub risk_level: &'a str,
    pub approved: bool,
    pub allowed: bool,
    pub success: bool,
    pub duration_ms: u64,
}

/// Structured side-effect gate decision details for audit logging.
///
/// Carries the EU AI Act Art.12 traceability fields for every authorization
/// decision: subject (`principal_id`), operation (`tool_name` + `operation_name`),
/// decision (`allowed`), deny `error` reason, and the `grant_id` correlation
/// handle. The event `timestamp` is stamped by [`AuditEvent::new`].
#[derive(Debug, Clone)]
pub struct SideEffectDecisionLog<'a> {
    pub tool_name: &'a str,
    pub operation_name: &'a str,
    pub risk_level: &'a str,
    pub approved: bool,
    pub allowed: bool,
    pub error: Option<&'a str>,
    /// Trusted caller principal id (audit subject / actor). `None` when no
    /// principal context is available (e.g. background runners).
    pub principal_id: Option<&'a str>,
    /// Stable approval-grant identifier for audit correlation, when a grant was
    /// presented. `None` when the decision was made without any grant.
    pub grant_id: Option<&'a str>,
}

/// The audit log path `configured` names inside `base`, or `None` when it
/// could land outside it (D-052).
///
/// The check is lexical first, so it holds for a file that does not exist
/// yet: only plain names (and `.`) are allowed, never `..`, a root or a
/// drive prefix. A symlink among the existing directories on the way can
/// still point elsewhere, so the deepest one that exists is resolved and
/// must stay inside the resolved base. This is checked once, here; the
/// open in [`AuditLogger::log`] checks again (D-055), because a dangling
/// link or a later swap passes this check.
fn confined_log_path(base: &Path, configured: &str) -> Option<PathBuf> {
    let relative = Path::new(configured);
    let plain = relative.components().all(|component| {
        matches!(
            component,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    });
    if configured.trim().is_empty() || !plain {
        return None;
    }
    // On Unix every directory on the way is opened without following a
    // symlink, so a symlinked directory would refuse every record; it is
    // refused here, once, instead (B2f F-3).
    #[cfg(unix)]
    if symlinked_log_directory(base, configured).is_some() {
        return None;
    }
    let joined = base.join(relative);
    let mut existing = joined.as_path();
    while !existing.exists() && existing != base {
        existing = existing.parent()?;
    }
    if existing == base && !base.exists() {
        return Some(joined);
    }
    let resolved_base = base.canonicalize().ok()?;
    let resolved = existing.canonicalize().ok()?;
    resolved.starts_with(&resolved_base).then_some(joined)
}

/// The first directory on the way from `base` to the audit log `configured`
/// (a path relative to `base`) that exists and is a symbolic link, whether it
/// points inside the workspace or not. On Unix the log is written only
/// through real directories (every component is opened with `O_NOFOLLOW`),
/// so such a `log_path` can never be written: configuration load warns about
/// it (D-063 G-1) and the logger skips every record. Only plain components are looked at; a lexical
/// escape (`..`, an absolute path) is the business of [`confined_log_path`].
#[cfg(unix)]
pub(crate) fn symlinked_log_directory(base: &Path, configured: &str) -> Option<PathBuf> {
    let parent = Path::new(configured).parent()?;
    let mut current = base.to_path_buf();
    for component in parent.components() {
        match component {
            std::path::Component::Normal(name) => current.push(name),
            std::path::Component::CurDir => continue,
            _ => return None,
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Some(current),
            Ok(_) => {}
            // A directory that does not exist yet cannot be a link, and
            // nothing below it exists either.
            Err(_) => return None,
        }
    }
    None
}

/// The directory that holds the audit log, opened for one record.
///
/// On Unix it is a handle reached from the base directory one component at a
/// time, never following a symlink (INT-2B AUDIT-SIGN, D-055): every later
/// open, stat and rename is relative to that handle, so swapping any
/// directory on the way for a symlink after this point cannot send a record
/// elsewhere, and a symlink already there is refused. The log file and its
/// `.lock` sidecar are opened without following a symlink either.
#[cfg(unix)]
struct LogDir {
    dir: std::os::fd::OwnedFd,
}

/// Why the log directory could not be opened for a record.
enum LogDirError {
    /// A component on the way is a symlink (or not a directory), or the path
    /// leaves the base: the record is skipped as unconfined.
    Unconfined,
    /// An ordinary I/O failure (a missing directory, permissions).
    Io(std::io::Error),
}

#[cfg(unix)]
impl LogDir {
    /// Open the directory of `log_path` (inside `base`, as checked by
    /// [`confined_log_path`]) from a handle on `base`, one component at a
    /// time with `O_NOFOLLOW`.
    fn open(base: &Path, log_path: &Path) -> std::result::Result<Self, LogDirError> {
        use rustix::fs::{Mode, OFlags};
        let directory = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let relative = log_path.strip_prefix(base).map_err(|_| LogDirError::Unconfined)?;
        let components: Vec<&std::ffi::OsStr> = relative
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(name) => Some(Ok(name)),
                std::path::Component::CurDir => None,
                _ => Some(Err(())),
            })
            .collect::<std::result::Result<_, ()>>()
            .map_err(|()| LogDirError::Unconfined)?;
        let Some((_, parents)) = components.split_last() else {
            return Err(LogDirError::Unconfined);
        };
        // The base itself is the operator's workspace path and may be reached
        // through a symlink (for example `/var` on macOS); nothing below it is.
        let mut dir =
            rustix::fs::open(base, directory, Mode::empty()).map_err(|error| LogDirError::Io(error.into()))?;
        for name in parents {
            dir = rustix::fs::openat(&dir, *name, directory | OFlags::NOFOLLOW, Mode::empty()).map_err(|error| {
                if error == rustix::io::Errno::LOOP || error == rustix::io::Errno::NOTDIR {
                    LogDirError::Unconfined
                } else {
                    LogDirError::Io(error.into())
                }
            })?;
        }
        Ok(Self { dir })
    }

    /// Open `name` in this directory without following a symlink.
    ///
    /// The open is non-blocking, so a FIFO planted at the name cannot stall
    /// every audit writer, and the opened file must be a regular file with
    /// exactly one link: a hard link to a file elsewhere (which `O_NOFOLLOW`
    /// does not see) is refused like a symlink (B2f F-4). `O_NONBLOCK` has no
    /// effect on a regular file.
    fn open_file(&self, name: &std::ffi::OsStr, flags: rustix::fs::OFlags) -> std::io::Result<std::fs::File> {
        use rustix::fs::{FileType, Mode, OFlags};
        let mode = Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH;
        let fd = rustix::fs::openat(
            &self.dir,
            name,
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            mode,
        )
        .map_err(|error| {
            // A non-blocking write-only open of a FIFO with no reader (or of
            // some devices) fails with ENXIO: the name is not a regular file,
            // which is the same confinement refusal as below (D-063).
            if error == rustix::io::Errno::NXIO {
                std::io::Error::other(NotAnAuditFile)
            } else {
                error.into()
            }
        })?;
        let stat = rustix::fs::fstat(&fd)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink != 1 {
            return Err(std::io::Error::other(NotAnAuditFile));
        }
        Ok(std::fs::File::from(fd))
    }

    /// Open (creating) the `.lock` sidecar of `name` for the cross-process lock.
    fn open_lock(&self, name: &std::ffi::OsStr) -> std::io::Result<std::fs::File> {
        let mut lock_name = name.to_os_string();
        lock_name.push(".lock");
        self.open_file(&lock_name, rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CREATE)
    }

    /// Open (creating) the log `name` for appending.
    fn open_log(&self, name: &std::ffi::OsStr) -> std::io::Result<std::fs::File> {
        use rustix::fs::OFlags;
        self.open_file(name, OFlags::WRONLY | OFlags::APPEND | OFlags::CREATE)
    }

    /// The size of the regular file `name`, read without following a symlink
    /// (`None` when it is absent or not a regular file).
    fn regular_file_size(&self, name: &std::ffi::OsStr) -> Option<u64> {
        let stat = rustix::fs::statat(&self.dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW).ok()?;
        (rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::RegularFile)
            .then(|| u64::try_from(stat.st_size).unwrap_or_default())
    }

    /// Rename `from` to `to`, both in this directory (a rename moves a link
    /// itself, never its target).
    fn rename(&self, from: &std::ffi::OsStr, to: &std::ffi::OsStr) -> std::io::Result<()> {
        Ok(rustix::fs::renameat(&self.dir, from, &self.dir, to)?)
    }
}

/// The directory that holds the audit log, by path (non-Unix): the parent is
/// resolved again at open time and must still be inside the resolved base,
/// and the file name is joined to that resolved directory. Unlike the Unix
/// handle, this leaves a window between the check and the open.
#[cfg(not(unix))]
struct LogDir {
    dir: PathBuf,
}

#[cfg(not(unix))]
impl LogDir {
    fn open(base: &Path, log_path: &Path) -> std::result::Result<Self, LogDirError> {
        let Some(parent) = log_path.parent() else {
            return Err(LogDirError::Unconfined);
        };
        let resolved_base = base.canonicalize().map_err(LogDirError::Io)?;
        let resolved_parent = parent.canonicalize().map_err(LogDirError::Io)?;
        if resolved_parent.starts_with(&resolved_base) {
            Ok(Self { dir: resolved_parent })
        } else {
            Err(LogDirError::Unconfined)
        }
    }

    fn open_file(&self, name: &std::ffi::OsStr, options: &mut OpenOptions) -> std::io::Result<std::fs::File> {
        let path = self.dir.join(name);
        if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the audit file is a symbolic link",
            ));
        }
        options.open(path)
    }

    fn open_lock(&self, name: &std::ffi::OsStr) -> std::io::Result<std::fs::File> {
        let mut lock_name = name.to_os_string();
        lock_name.push(".lock");
        self.open_file(
            &lock_name,
            OpenOptions::new().create(true).truncate(false).read(true).write(true),
        )
    }

    fn open_log(&self, name: &std::ffi::OsStr) -> std::io::Result<std::fs::File> {
        self.open_file(name, OpenOptions::new().create(true).append(true))
    }

    fn regular_file_size(&self, name: &std::ffi::OsStr) -> Option<u64> {
        std::fs::symlink_metadata(self.dir.join(name))
            .ok()
            .filter(std::fs::Metadata::is_file)
            .map(|metadata| metadata.len())
    }

    fn rename(&self, from: &std::ffi::OsStr, to: &std::ffi::OsStr) -> std::io::Result<()> {
        std::fs::rename(self.dir.join(from), self.dir.join(to))
    }
}

/// The opened audit file is not a regular file with exactly one link (a
/// FIFO, a device, a hard link to a file elsewhere): the record is skipped.
#[cfg(unix)]
#[derive(Debug)]
struct NotAnAuditFile;

#[cfg(unix)]
impl std::fmt::Display for NotAnAuditFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the audit file is not a regular file with exactly one link")
    }
}

#[cfg(unix)]
impl std::error::Error for NotAnAuditFile {}

/// Whether an open was refused for confinement: the last component is a
/// symlink, or (Unix) the file is not a regular file with one link.
fn is_symlink_refusal(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::ELOOP) || error.get_ref().is_some_and(|inner| inner.is::<NotAnAuditFile>())
    }
    #[cfg(not(unix))]
    {
        error.kind() == std::io::ErrorKind::InvalidInput
    }
}

/// A test's action between the confinement check and the open.
#[cfg(test)]
type BeforeOpenHook = Box<dyn FnOnce()>;

#[cfg(test)]
thread_local! {
    static BEFORE_OPEN: std::cell::RefCell<Option<BeforeOpenHook>> = std::cell::RefCell::new(None);
}

/// Run (once) the action a test placed between the confinement check and
/// the open, to swap a directory in that window.
#[cfg(test)]
fn run_before_open_hook() {
    if let Some(hook) = BEFORE_OPEN.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}

impl AuditLogger {
    /// Create a new audit logger.
    ///
    /// Borrows the config (no clone): only `max_size_mb` is retained, plus
    /// the confined `log_path`. A `log_path` that could land outside
    /// `openprx_dir` is not replaced by another path: the logger then skips
    /// every record with an error (D-055).
    pub fn new(config: &AuditConfig, openprx_dir: PathBuf) -> Result<Self> {
        let target = confined_log_path(&openprx_dir, &config.log_path).map(|log_path| AuditTarget {
            writer_lock: shared_audit_path_lock(&log_path),
            log_path,
        });
        if target.is_none() {
            tracing::error!(
                path = %config.log_path,
                "audit log path must be a relative path inside the workspace through real directories \
                 (no symbolic link on the way); audit records are not written"
            );
        }
        Ok(Self {
            base: openprx_dir,
            target,
            max_size_mb: config.max_size_mb,
        })
    }

    /// The record is skipped: the audit log would leave the base directory.
    fn refuse(log_path: Option<&Path>) -> anyhow::Error {
        let shown = log_path.map_or_else(|| "(unconfined)".to_string(), |path| path.display().to_string());
        tracing::error!(
            path = %shown,
            "audit log path leaves the workspace or is a symbolic link; the record is skipped"
        );
        anyhow::anyhow!("audit record skipped: the audit log path is not confined to the workspace")
    }

    /// Log an event
    pub fn log(&self, event: &AuditEvent) -> Result<()> {
        let mut record = serde_json::to_vec(event)?;
        record.push(b'\n');

        // Rotation and append must share the same path-scoped critical section.
        // A complete JSONL record is prepared before locking and emitted with one
        // write_all call so independent AuditLogger instances cannot interleave.
        // The sidecar file lock extends that guarantee across daemon/chat/CLI
        // processes that share the same workspace audit path.
        let Some(target) = &self.target else {
            return Err(Self::refuse(None));
        };
        let _guard = target.writer_lock.lock();
        // Confinement is checked again at open time (D-055): the log's
        // directory is opened from the base without following a symlink, and
        // every file below is opened relative to that directory handle
        // (INT-2B AUDIT-SIGN), so no swap after this point can redirect it.
        let dir = match LogDir::open(&self.base, &target.log_path) {
            Ok(dir) => dir,
            Err(LogDirError::Unconfined) => return Err(Self::refuse(Some(&target.log_path))),
            Err(LogDirError::Io(error)) => return Err(error.into()),
        };
        let Some(name) = target.log_path.file_name() else {
            return Err(Self::refuse(Some(&target.log_path)));
        };
        #[cfg(test)]
        run_before_open_hook();
        let lock_file = dir.open_lock(name).map_err(|error| {
            if is_symlink_refusal(&error) {
                let mut lock_path = target.log_path.clone().into_os_string();
                lock_path.push(".lock");
                Self::refuse(Some(Path::new(&lock_path)))
            } else {
                error.into()
            }
        })?;
        lock_file.lock_exclusive()?;
        Self::rotate_if_needed(&dir, name, self.max_size_mb)?;
        let mut file = dir.open_log(name).map_err(|error| {
            if is_symlink_refusal(&error) {
                Self::refuse(Some(&target.log_path))
            } else {
                error.into()
            }
        })?;
        file.write_all(&record)?;
        file.sync_all()?;

        Ok(())
    }

    /// Log a command execution event.
    ///
    /// The command string is redacted to remove common secret patterns
    /// (tokens, passwords, API keys) before writing to the audit log.
    pub fn log_command_event(&self, entry: CommandExecutionLog<'_>) -> Result<()> {
        let redacted_command = crate::security::redaction::redact_secrets(entry.command);
        let event = AuditEvent::new(AuditEventType::CommandExecution)
            .with_actor(entry.channel.to_string(), None, None)
            .with_action(
                redacted_command,
                entry.risk_level.to_string(),
                entry.approved,
                entry.allowed,
            )
            .with_result(entry.success, None, entry.duration_ms, None);

        self.log(&event)
    }

    /// Log a SideEffectGate decision for command or resource operations.
    ///
    /// Records the full EU AI Act Art.12 field set: timestamp (stamped by
    /// [`AuditEvent::new`]), subject (`principal_id` as the actor `user_id`),
    /// operation (`tool_name:operation_name`), decision (`allowed`), deny reason
    /// (`error`), and the `grant_id` correlation handle (folded into the action
    /// string so it survives in the structured `command` field).
    pub fn log_side_effect_decision(&self, entry: SideEffectDecisionLog<'_>) -> Result<()> {
        // Redact secret-bearing values in the operation name before it lands in
        // the structured action field (the tool_name is a fixed enum and needs no
        // redaction; the operation_name can embed raw command args / credentials).
        let mut action = format!(
            "{}:{}",
            entry.tool_name,
            crate::security::redaction::redact_secrets(entry.operation_name)
        );
        if let Some(grant_id) = entry.grant_id {
            // Append the grant correlation handle so a single structured field
            // ties the decision back to the authorizing grant.
            action.push_str(" grant_id=");
            action.push_str(grant_id);
        }
        let event = AuditEvent::new(AuditEventType::ToolGate)
            .with_actor(
                "side_effect_gate".to_string(),
                entry.principal_id.map(ToString::to_string),
                None,
            )
            .with_action(action, entry.risk_level.to_string(), entry.approved, entry.allowed)
            // Redact secrets in the deny reason before it lands in the error field;
            // gate error strings can embed the raw command / operation_name.
            .with_result(entry.allowed, None, 0, entry.error.map(crate::security::redaction::redact_secrets));

        self.log(&event)
    }

    /// Backward-compatible helper to log a command execution event.
    #[allow(clippy::too_many_arguments)]
    pub fn log_command(
        &self,
        channel: &str,
        command: &str,
        risk_level: &str,
        approved: bool,
        allowed: bool,
        success: bool,
        duration_ms: u64,
    ) -> Result<()> {
        self.log_command_event(CommandExecutionLog {
            channel,
            command,
            risk_level,
            approved,
            allowed,
            success,
            duration_ms,
        })
    }

    /// Rotate the log if it exceeds the maximum size. The size is read
    /// without following a symlink: a planted link is refused by the open,
    /// never rotated.
    fn rotate_if_needed(dir: &LogDir, name: &std::ffi::OsStr, max_size_mb: u32) -> Result<()> {
        if let Some(size) = dir.regular_file_size(name)
            && size / (1024 * 1024) >= u64::from(max_size_mb)
        {
            Self::rotate(dir, name)?;
        }
        Ok(())
    }

    /// Rotate the log file inside its directory: `<name>.<i>.log` moves to
    /// `<name>.<i+1>.log` (up to 10) and the log becomes `<name>.1.log`.
    fn rotate(dir: &LogDir, name: &std::ffi::OsStr) -> Result<()> {
        let numbered = |index: u32| {
            let mut rotated = name.to_os_string();
            rotated.push(format!(".{index}.log"));
            rotated
        };
        for i in (1..10).rev() {
            // A missing older generation is the normal case.
            let _ = dir.rename(&numbered(i), &numbered(i + 1));
        }
        dir.rename(name, &numbered(1))?;
        Ok(())
    }
}

/// Best-effort production audit hook for SideEffectGate decisions.
///
/// Default/unit policies often use `.` as workspace; skip those to avoid writing
/// audit.log into the repo during tests. Runtime policies built from config have
/// an explicit workspace.
///
/// The caller threads the real `security.audit` configuration through `config`
/// (previously this hard-coded `AuditConfig::default()`, so user configuration
/// was ignored). Audit recording is always available and has no module switch.
pub fn record_side_effect_decision_best_effort(
    workspace_dir: &Path,
    config: &AuditConfig,
    entry: SideEffectDecisionLog<'_>,
) {
    if workspace_dir.as_os_str().is_empty() || workspace_dir == Path::new(".") {
        return;
    }
    let Ok(logger) = AuditLogger::new(config, workspace_dir.to_path_buf()) else {
        return;
    };
    if let Err(error) = logger.log_side_effect_decision(entry) {
        tracing::debug!(error = %error, "failed to write side-effect audit event");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn audit_event_new_creates_unique_id() {
        let event1 = AuditEvent::new(AuditEventType::CommandExecution);
        let event2 = AuditEvent::new(AuditEventType::CommandExecution);
        assert_ne!(event1.event_id, event2.event_id);
    }

    #[test]
    fn audit_event_with_actor() {
        let event = AuditEvent::new(AuditEventType::CommandExecution).with_actor(
            "telegram".to_string(),
            Some("123".to_string()),
            Some("@alice".to_string()),
        );

        assert!(event.actor.is_some());
        let actor = event.actor.as_ref().unwrap();
        assert_eq!(actor.channel, "telegram");
        assert_eq!(actor.user_id, Some("123".to_string()));
        assert_eq!(actor.username, Some("@alice".to_string()));
    }

    #[test]
    fn audit_event_with_action() {
        let event = AuditEvent::new(AuditEventType::CommandExecution).with_action(
            "ls -la".to_string(),
            "low".to_string(),
            false,
            true,
        );

        assert!(event.action.is_some());
        let action = event.action.as_ref().unwrap();
        assert_eq!(action.command, Some("ls -la".to_string()));
        assert_eq!(action.risk_level, Some("low".to_string()));
    }

    #[test]
    fn audit_event_serializes_to_json() {
        let event = AuditEvent::new(AuditEventType::CommandExecution)
            .with_actor("telegram".to_string(), None, None)
            .with_action("ls".to_string(), "low".to_string(), false, true)
            .with_result(true, Some(0), 15, None);

        let json = serde_json::to_string(&event);
        assert!(json.is_ok());
        let json = json.expect("serialize");
        let parsed: AuditEvent = serde_json::from_str(json.as_str()).expect("parse");
        assert!(parsed.actor.is_some());
        assert!(parsed.action.is_some());
        assert!(parsed.result.is_some());
    }

    #[test]
    fn audit_side_effect_decision_uses_tool_gate_event_type() -> Result<()> {
        let tmp = TempDir::new()?;
        let logger = AuditLogger::new(&AuditConfig::default(), tmp.path().to_path_buf())?;

        logger.log_side_effect_decision(SideEffectDecisionLog {
            tool_name: "file_write",
            operation_name: "file_write:write:abc",
            risk_level: "medium",
            approved: false,
            allowed: false,
            error: Some("requires approval"),
            principal_id: None,
            grant_id: None,
        })?;

        let log = std::fs::read_to_string(tmp.path().join("audit.log"))?;
        assert!(log.contains(r#""event_type":"tool_gate""#));
        assert!(log.contains("file_write:file_write:write:abc"));
        Ok(())
    }

    #[test]
    fn audit_side_effect_decision_records_compliance_fields() -> Result<()> {
        // EU AI Act Art.12: every gate decision audit entry must carry the
        // subject (principal_id), operation, decision, deny reason, and grant_id.
        let tmp = TempDir::new()?;
        let logger = AuditLogger::new(&AuditConfig::default(), tmp.path().to_path_buf())?;

        logger.log_side_effect_decision(SideEffectDecisionLog {
            tool_name: "file_write",
            operation_name: "file_write:write:abc",
            risk_level: "medium",
            approved: true,
            allowed: true,
            error: None,
            principal_id: Some("telegram:alice"),
            grant_id: Some("grant-123"),
        })?;

        let raw = std::fs::read_to_string(tmp.path().join("audit.log"))?;
        let event: AuditEvent = serde_json::from_str(raw.trim()).expect("parse audit event");
        assert!(event.timestamp <= Utc::now());
        let actor = event.actor.expect("actor present");
        assert_eq!(actor.user_id.as_deref(), Some("telegram:alice"));
        let action = event.action.expect("action present");
        let command = action.command.expect("command present");
        assert!(command.contains("file_write:file_write:write:abc"));
        assert!(command.contains("grant_id=grant-123"));
        assert!(action.allowed);
        Ok(())
    }

    #[test]
    fn audit_side_effect_decision_redacts_secrets_in_action_and_error() -> Result<()> {
        // BUG-D1-02: the operation_name and deny reason can embed raw command
        // arguments / credentials; both must be redacted before they land in the
        // structured audit fields.
        let tmp = TempDir::new()?;
        let logger = AuditLogger::new(&AuditConfig::default(), tmp.path().to_path_buf())?;

        logger.log_side_effect_decision(SideEffectDecisionLog {
            tool_name: "shell",
            operation_name: "shell:exec:curl -H 'Authorization: Bearer zzzsecret' --password=xxxsecret",
            risk_level: "high",
            approved: false,
            allowed: false,
            error: Some("Command not allowed by security policy: deploy --token=yyysecret"),
            principal_id: None,
            grant_id: None,
        })?;

        let log = std::fs::read_to_string(tmp.path().join("audit.log"))?;
        assert!(
            !log.contains("zzzsecret"),
            "bearer token must be redacted in audit action"
        );
        assert!(
            !log.contains("xxxsecret"),
            "password value must be redacted in audit action"
        );
        assert!(
            !log.contains("yyysecret"),
            "token value must be redacted in audit error"
        );
        assert!(log.contains("[REDACTED]"), "redaction marker must be present");
        Ok(())
    }

    /// REDACT-2 (D-054 amendments): the audit log detects secrets with the one
    /// redactor, so token formats with no `key=` in front (an AWS key id, an
    /// OpenAI key, a GitHub token) are redacted as well, and a short keyed
    /// value (`--token=zq`) still is, as before. Fails before REDACT-2.
    #[test]
    fn audit_records_redact_every_secret_the_one_redactor_detects() -> Result<()> {
        let tmp = TempDir::new()?;
        let logger = AuditLogger::new(&AuditConfig::default(), tmp.path().to_path_buf())?;

        logger.log_side_effect_decision(SideEffectDecisionLog {
            tool_name: "shell",
            operation_name: "shell:exec:aws configure set aws_access_key_id AKIAIOSFODNN7EXAMPLE",
            risk_level: "high",
            approved: false,
            allowed: false,
            error: Some(
                "denied: git push https://github.com with ghp_abcdefghijklmnop0123 and sk-proj-abcdefghijklmnop; --token=zq",
            ),
            principal_id: None,
            grant_id: None,
        })?;

        let log = std::fs::read_to_string(tmp.path().join("audit.log"))?;
        for secret in [
            "AKIAIOSFODNN7EXAMPLE",
            "ghp_abcdefghijklmnop0123",
            "sk-proj-abcdefghijklmnop",
            "--token=zq",
        ] {
            assert!(!log.contains(secret), "{secret} reached the audit log: {log}");
        }
        assert!(log.contains("[REDACTED]"), "redaction marker must be present");
        Ok(())
    }

    // ── §8.1 Log rotation tests ─────────────────────────────

    #[tokio::test]
    async fn audit_logger_writes_event_when_enabled() -> Result<()> {
        let tmp = TempDir::new()?;
        let config = AuditConfig {
            max_size_mb: 10,
            ..Default::default()
        };
        let logger = AuditLogger::new(&config, tmp.path().to_path_buf())?;
        let event = AuditEvent::new(AuditEventType::CommandExecution)
            .with_actor("cli".to_string(), None, None)
            .with_action("ls".to_string(), "low".to_string(), false, true);

        logger.log(&event)?;

        let log_path = tmp.path().join("audit.log");
        assert!(log_path.exists(), "audit log file must be created");

        let content = tokio::fs::read_to_string(&log_path).await?;
        assert!(!content.is_empty(), "audit log must not be empty");

        let parsed: AuditEvent = serde_json::from_str(content.trim())?;
        assert!(parsed.action.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn audit_log_command_event_writes_structured_entry() -> Result<()> {
        let tmp = TempDir::new()?;
        let config = AuditConfig {
            max_size_mb: 10,
            ..Default::default()
        };
        let logger = AuditLogger::new(&config, tmp.path().to_path_buf())?;

        logger.log_command_event(CommandExecutionLog {
            channel: "telegram",
            command: "echo test",
            risk_level: "low",
            approved: false,
            allowed: true,
            success: true,
            duration_ms: 42,
        })?;

        let log_path = tmp.path().join("audit.log");
        let content = tokio::fs::read_to_string(&log_path).await?;
        let parsed: AuditEvent = serde_json::from_str(content.trim())?;

        let action = parsed.action.unwrap();
        assert_eq!(action.command, Some("echo test".to_string()));
        assert_eq!(action.risk_level, Some("low".to_string()));
        assert!(action.allowed);

        let result = parsed.result.unwrap();
        assert!(result.success);
        assert_eq!(result.duration_ms, Some(42));
        Ok(())
    }

    #[test]
    fn concurrent_logger_instances_preserve_jsonl_records() -> Result<()> {
        const WRITERS: usize = 8;
        const EVENTS_PER_WRITER: usize = 25;

        let tmp = TempDir::new()?;
        let log_dir = tmp.path().to_path_buf();
        let barrier = Arc::new(std::sync::Barrier::new(WRITERS));
        let mut threads = Vec::new();
        for writer_id in 0..WRITERS {
            let log_dir = log_dir.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || -> Result<()> {
                let logger = AuditLogger::new(
                    &AuditConfig {
                        max_size_mb: 100,
                        ..Default::default()
                    },
                    log_dir,
                )?;
                barrier.wait();
                for event_id in 0..EVENTS_PER_WRITER {
                    logger.log(&AuditEvent::new(AuditEventType::SecurityEvent).with_action(
                        format!("writer={writer_id} event={event_id}"),
                        "test".to_string(),
                        false,
                        true,
                    ))?;
                }
                Ok(())
            }));
        }
        for thread in threads {
            thread.join().expect("test: audit writer thread panicked")?;
        }

        let raw = std::fs::read_to_string(tmp.path().join("audit.log"))?;
        let lines = raw.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), WRITERS * EVENTS_PER_WRITER);
        for (index, line) in lines.iter().enumerate() {
            serde_json::from_str::<AuditEvent>(line)
                .unwrap_or_else(|error| panic!("audit line {index} is invalid JSON: {error}: {line}"));
        }
        Ok(())
    }

    #[test]
    fn audit_rotation_creates_numbered_backup() -> Result<()> {
        let tmp = TempDir::new()?;
        let config = AuditConfig {
            max_size_mb: 0, // Force rotation on first write
            ..Default::default()
        };
        let logger = AuditLogger::new(&config, tmp.path().to_path_buf())?;

        // Write initial content that triggers rotation
        let log_path = tmp.path().join("audit.log");
        std::fs::write(&log_path, "initial content\n")?;

        let event = AuditEvent::new(AuditEventType::CommandExecution);
        logger.log(&event)?;

        let rotated = format!("{}.1.log", log_path.display());
        assert!(
            std::path::Path::new(&rotated).exists(),
            "rotation must create .1.log backup"
        );
        Ok(())
    }

    fn configured_logger(base: &Path, log_path: &str) -> Result<AuditLogger> {
        AuditLogger::new(
            &AuditConfig {
                log_path: log_path.to_string(),
                max_size_mb: 10,
                ..Default::default()
            },
            base.to_path_buf(),
        )
    }

    fn one_event() -> AuditEvent {
        AuditEvent::new(AuditEventType::SecurityEvent).with_action("probe".to_string(), "low".to_string(), false, true)
    }

    /// D-052, RED before SECRETS-k (R-SECRETSj F-2): a `log_path` with `..`
    /// whose target did not exist yet escaped the base directory, because
    /// `canonicalize` failed and the unresolved path kept its `..`.
    /// D-055 (R-SECRETSj K-2): such a path, an absolute one, an empty one and
    /// a symlinked directory out of the base are not replaced by another
    /// path: the record is skipped with an error and nothing is written. A
    /// plain relative path stays where it is configured.
    #[test]
    fn the_audit_log_never_leaves_the_base_directory() -> Result<()> {
        let outside = TempDir::new()?;
        let tmp = TempDir::new()?;
        let base = tmp.path().join("workspace");
        std::fs::create_dir_all(&base)?;

        let mut escaping = vec![
            "../escaped.log".to_string(),
            "logs/../../escaped.log".to_string(),
            outside.path().join("escaped.log").display().to_string(),
            String::new(),
        ];
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), base.join("link"))?;
            escaping.push("link/escaped.log".to_string());
        }
        for path in escaping {
            let logger = configured_logger(&base, &path)?;
            assert!(logger.target.is_none(), "{path:?} must not be confined");
            assert!(logger.log(&one_event()).is_err(), "{path:?}: the record is skipped");
        }
        assert!(!tmp.path().join("escaped.log").exists());
        assert!(!outside.path().join("escaped.log").exists());
        assert!(!base.join("audit.log").exists(), "no fallback file is written");

        std::fs::create_dir_all(base.join("logs"))?;
        let logger = configured_logger(&base, "logs/audit.log")?;
        assert_eq!(
            logger.target.as_ref().map(|target| target.log_path.clone()),
            Some(base.join("logs/audit.log"))
        );
        logger.log(&one_event())?;
        assert!(base.join("logs/audit.log").is_file());
        Ok(())
    }

    /// D-055 (R-SECRETSj K-2): `audit.log` planted as a dangling symlink.
    /// `exists()` is false for it, so it passes the check in `new`; before,
    /// the open followed it and created the file outside the workspace.
    #[cfg(unix)]
    #[test]
    fn a_dangling_audit_log_link_is_never_followed() -> Result<()> {
        let outside = TempDir::new()?;
        let base = TempDir::new()?;
        let target = outside.path().join("created.log");
        std::os::unix::fs::symlink(&target, base.path().join("audit.log"))?;

        let logger = configured_logger(base.path(), "audit.log")?;
        assert!(logger.log(&one_event()).is_err(), "the link is refused");
        assert!(!target.exists(), "nothing is created outside the workspace");
        Ok(())
    }

    /// D-055 (R-SECRETSj K-2): with the default `log_path`, `audit.log`
    /// linked to an existing file outside. Before, the unconfined fallback
    /// was that same link and records were appended to the outside file.
    /// The link planted after the logger was created is refused at open time
    /// as well.
    #[cfg(unix)]
    #[test]
    fn an_audit_log_link_to_an_outside_file_is_never_written() -> Result<()> {
        let outside = TempDir::new()?;
        let victim = outside.path().join("config.toml");
        std::fs::write(&victim, "keep\n")?;

        let base = TempDir::new()?;
        std::os::unix::fs::symlink(&victim, base.path().join("audit.log"))?;
        let logger = configured_logger(base.path(), &AuditConfig::default().log_path)?;
        assert!(logger.log(&one_event()).is_err(), "planted before the logger");
        assert_eq!(std::fs::read_to_string(&victim)?, "keep\n");

        let later = TempDir::new()?;
        let logger = configured_logger(later.path(), &AuditConfig::default().log_path)?;
        std::os::unix::fs::symlink(&victim, later.path().join("audit.log"))?;
        assert!(logger.log(&one_event()).is_err(), "planted after the logger");
        assert_eq!(std::fs::read_to_string(&victim)?, "keep\n");
        Ok(())
    }

    /// D-055 (R-SECRETSj K-2): the sidecar `audit.log.lock` planted as a
    /// symlink. Before, it was opened with create and write through the link.
    #[cfg(unix)]
    #[test]
    fn a_planted_audit_lock_link_is_never_followed() -> Result<()> {
        let outside = TempDir::new()?;
        let base = TempDir::new()?;
        let target = outside.path().join("created.lock");
        std::os::unix::fs::symlink(&target, base.path().join("audit.log.lock"))?;

        let logger = configured_logger(base.path(), "audit.log")?;
        assert!(logger.log(&one_event()).is_err(), "the lock link is refused");
        assert!(!target.exists(), "no lock file is created outside");
        assert!(!base.path().join("audit.log").exists(), "the record is skipped");
        Ok(())
    }

    /// D-055 (R-SECRETSj K-2): the parent directory is resolved again at
    /// open time, so a directory swapped for a symlink out of the workspace
    /// after the logger was created is refused.
    #[cfg(unix)]
    #[test]
    fn a_log_directory_swapped_for_a_link_is_refused_at_open() -> Result<()> {
        let outside = TempDir::new()?;
        let base = TempDir::new()?;
        std::fs::create_dir_all(base.path().join("logs"))?;
        let logger = configured_logger(base.path(), "logs/audit.log")?;
        assert!(logger.target.is_some());

        std::fs::remove_dir(base.path().join("logs"))?;
        std::os::unix::fs::symlink(outside.path(), base.path().join("logs"))?;
        assert!(logger.log(&one_event()).is_err(), "the swapped directory is refused");
        assert!(!outside.path().join("audit.log").exists());
        assert!(!outside.path().join("audit.log.lock").exists());
        Ok(())
    }

    /// INT-2B AUDIT-SIGN (D-056 DC-2, the D-055 middle-directory race), RED
    /// before: the open re-resolved the log's directory and then opened the
    /// resolved *path*, so a middle directory swapped for a symlink out of
    /// the workspace between that check and the open sent the lock and the
    /// record outside. After: the open walks from a handle on the workspace
    /// and never follows a link, so the record lands in the directory that
    /// was checked, never outside.
    #[cfg(unix)]
    #[test]
    fn a_middle_directory_swapped_after_the_check_is_never_followed() -> Result<()> {
        let outside = TempDir::new()?;
        std::fs::create_dir_all(outside.path().join("deep"))?;
        let base = TempDir::new()?;
        std::fs::create_dir_all(base.path().join("logs/deep"))?;
        let logger = configured_logger(base.path(), "logs/deep/audit.log")?;
        assert!(logger.target.is_some());

        let logs = base.path().join("logs");
        let moved = base.path().join("logs-moved");
        let link_target = outside.path().to_path_buf();
        BEFORE_OPEN.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::rename(&logs, &moved).expect("test: move the checked directory");
                std::os::unix::fs::symlink(&link_target, &logs).expect("test: plant the link");
            }));
        });
        let outcome = logger.log(&one_event());
        assert!(
            !outside.path().join("deep/audit.log").exists(),
            "the record left the workspace ({outcome:?})"
        );
        assert!(
            !outside.path().join("deep/audit.log.lock").exists(),
            "the lock left the workspace ({outcome:?})"
        );
        // The directory handle was taken before the swap, so the write
        // succeeds, into the directory that was checked (now moved).
        assert!(outcome.is_ok(), "the record is written: {outcome:?}");
        assert!(
            base.path().join("logs-moved/deep/audit.log").is_file(),
            "the record lands in the directory that was checked"
        );
        Ok(())
    }

    /// D-055: a middle directory already swapped for a symlink out of the
    /// workspace when the record is written is refused with an error.
    #[cfg(unix)]
    #[test]
    fn a_middle_directory_swapped_before_the_write_is_refused() -> Result<()> {
        let outside = TempDir::new()?;
        std::fs::create_dir_all(outside.path().join("deep"))?;
        let base = TempDir::new()?;
        std::fs::create_dir_all(base.path().join("logs/deep"))?;
        let logger = configured_logger(base.path(), "logs/deep/audit.log")?;

        std::fs::rename(base.path().join("logs"), base.path().join("logs-moved"))?;
        std::os::unix::fs::symlink(outside.path(), base.path().join("logs"))?;
        assert!(logger.log(&one_event()).is_err(), "the swapped directory is refused");
        assert!(!outside.path().join("deep/audit.log").exists());
        assert!(!outside.path().join("deep/audit.log.lock").exists());
        Ok(())
    }

    /// B2f F-3, RED before: a `log_path` through a symlinked directory that
    /// stays inside the workspace passed the construction check (it was
    /// canonicalized), and then every record was refused at open time
    /// because no directory is opened through a link. After: it is refused
    /// once, when the logger is built (and at configuration load).
    #[cfg(unix)]
    #[test]
    fn an_in_workspace_symlinked_log_directory_is_refused_up_front() -> Result<()> {
        let base = TempDir::new()?;
        std::fs::create_dir_all(base.path().join("real"))?;
        std::os::unix::fs::symlink(base.path().join("real"), base.path().join("alias"))?;
        assert_eq!(
            symlinked_log_directory(base.path(), "alias/audit.log"),
            Some(base.path().join("alias"))
        );
        assert_eq!(symlinked_log_directory(base.path(), "real/audit.log"), None);
        assert_eq!(symlinked_log_directory(base.path(), "missing/deeper/audit.log"), None);

        let logger = configured_logger(base.path(), "alias/audit.log")?;
        assert!(logger.target.is_none(), "refused when the logger is built");
        assert!(logger.log(&one_event()).is_err());
        assert!(!base.path().join("real/audit.log").exists());

        let logger = configured_logger(base.path(), "real/audit.log")?;
        logger.log(&one_event())?;
        assert!(base.path().join("real/audit.log").is_file());
        Ok(())
    }

    /// B2f F-4, RED before: `O_NOFOLLOW` does not see a hard link, so an
    /// `audit.log` (or `audit.log.lock`) hard-linked to a file outside the
    /// workspace was written through. After: a file with more than one link
    /// is refused and the outside file is untouched.
    #[cfg(unix)]
    #[test]
    fn a_hard_linked_audit_file_is_never_written() -> Result<()> {
        let outside = TempDir::new()?;
        let victim = outside.path().join("victim.txt");
        std::fs::write(&victim, "keep\n")?;

        let base = TempDir::new()?;
        std::fs::hard_link(&victim, base.path().join("audit.log"))?;
        let logger = configured_logger(base.path(), "audit.log")?;
        assert!(logger.log(&one_event()).is_err(), "a hard-linked log is refused");
        assert_eq!(std::fs::read_to_string(&victim)?, "keep\n");

        let lock_base = TempDir::new()?;
        std::fs::hard_link(&victim, lock_base.path().join("audit.log.lock"))?;
        let logger = configured_logger(lock_base.path(), "audit.log")?;
        assert!(logger.log(&one_event()).is_err(), "a hard-linked lock is refused");
        assert!(!lock_base.path().join("audit.log").exists(), "the record is skipped");
        assert_eq!(std::fs::read_to_string(&victim)?, "keep\n");
        Ok(())
    }

    /// B2f F-4, RED before: a FIFO planted at `audit.log` made the open
    /// block (no reader) while the audit locks were held, stalling every
    /// audit writer. After: the record is skipped at once.
    /// D-063, RED before: at `audit.log` the non-blocking open failed with
    /// ENXIO and was reported as an I/O error ("Device not configured").
    /// After: a FIFO at either name gives the confinement refusal.
    #[cfg(unix)]
    #[test]
    fn a_fifo_at_the_audit_log_is_refused_without_blocking() -> Result<()> {
        for name in ["audit.log", "audit.log.lock"] {
            let base = TempDir::new()?;
            let fifo = base.path().join(name);
            let made = std::process::Command::new("mkfifo").arg(&fifo).status()?;
            assert!(made.success(), "test: mkfifo");
            let logger = configured_logger(base.path(), "audit.log")?;
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let outcome = logger.log(&one_event()).map_err(|error| error.to_string());
                let _ = sender.send(outcome);
            });
            // A failure-only bound: the write returns at once or blocks
            // indefinitely, nothing in between.
            let outcome = receiver
                .recv_timeout(std::time::Duration::from_secs(10))
                .map_err(|_| anyhow::anyhow!("the audit write blocked on a FIFO at {name}"))?;
            let error = outcome.expect_err("a FIFO is not an audit file");
            assert!(
                error.contains("not confined to the workspace"),
                "a FIFO at {name} gives the confinement refusal: {error}"
            );
        }
        Ok(())
    }
}

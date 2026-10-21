//! Reviewed executable bundles (SL11).
//!
//! Two things live here, both for skills whose bundle carries scripts or
//! declared `[[tools]]`:
//!
//! * [`review_executable_bundle`] is the stronger review an operator-authored
//!   executable revision gets before it can be published (migration import or
//!   an operator import through the lifecycle service). Executable revisions
//!   never come from the proposer: a proposal may change `SKILL.md` text only,
//!   and `capability.rs` refuses any other change.
//! * [`prepare_execution`] resolves a declared tool of a lifecycle-managed
//!   skill for one `skill_execute` call. It uses the same resolution as
//!   `skill_read` ([`super::binding::resolve_revision`]), so a task that read or
//!   executed revision A executes A's script even after B became active. The
//!   complete pinned bundle is verified when it is read, staged into a private
//!   run directory with its exact modes, and verified again byte for byte
//!   before anything runs. The caller runs the tool through the existing
//!   shell/HTTP adapters and ends the run with [`PreparedExecution::finish`],
//!   which removes the run directory.
//!
//! A skill cannot grant its own access: [`ToolGrant`] is supplied by the
//! caller from the operator's policy, and nothing in a manifest can widen it.

use super::binding::{ResolvedRevision, resolve_revision};
use super::bundle::{Bundle, normalize_relative_path};
use super::manifest::validate_manifest_strict;
use super::store::{ensure_real_dir, write_private_file};
use super::types::{LifecycleError, OwnerScope, PinnedSkillRevision, RevisionId};
use crate::skills::SkillTool;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use thiserror::Error;

const SKILL_TOML: &str = "SKILL.toml";
const UNTRUSTED_ORIGIN_MARKER: &str = ".openprx-untrusted-origin.json";
/// Directory next to the store root (`<workspace>/self/`) holding run
/// directories. Nothing in it outlives a call; leftovers of a crashed process
/// are removed by the next preparation.
const RUN_ROOT_DIR: &str = "skill-exec";
const RUN_DIR_PREFIX: &str = "run-";
/// Run directories this old are leftovers of a process that died. Far above
/// any execution timeout, so a live run is never swept.
const STALE_RUN_AGE: Duration = Duration::from_hours(6);
const MAX_RUN_SCAN: usize = 256;
const MAX_TOOLS: usize = 32;
const MAX_TOOL_NAME_BYTES: usize = 64;
const MAX_COMMAND_BYTES: usize = 4096;
const MAX_ARGS: usize = 32;
const MAX_ARG_KEY_BYTES: usize = 64;
const MAX_ARG_VALUE_BYTES: usize = 4096;
const EXECUTION_IDENTITY_DOMAIN: &[u8] = b"prx-skill-exec-identity\0";

#[derive(Debug, Error)]
pub enum ExecutableError {
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    #[error("the bundle declares no executable tool and has no executable file; it is not an executable bundle")]
    NotExecutable,
    #[error("skill {skill:?} declares no tool {tool:?} in its pinned revision {revision}")]
    UndeclaredTool {
        skill: String,
        tool: String,
        revision: String,
    },
    #[error("invalid tool declaration {tool:?}: {reason}")]
    InvalidTool { tool: String, reason: String },
    #[error("SKILL.toml is unusable: {0}")]
    Manifest(String),
    #[error("executable file {path:?} is not referenced by any declared tool")]
    UnreferencedExecutable { path: String },
    #[error(
        "the bundle carries an untrusted-origin marker; only operator-authored bundles can be reviewed as executable"
    )]
    UntrustedOrigin,
    #[error("the bundle hashes to {found}, not to the revision {reviewed} the operator reviewed")]
    NotReviewed { reviewed: String, found: String },
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("tool {tool:?} is not granted: {reason}")]
    NotGranted { tool: String, reason: String },
    #[error("{path:?} differs from the pinned revision after it was staged for execution")]
    StagedFileDiffers { path: String },
    #[error("cannot stage the pinned bundle for execution: {0}")]
    Stage(String),
    #[error("the run directory {path} could not be removed: {reason}")]
    Cleanup { path: PathBuf, reason: String },
}

/// What the operator's policy grants a call. Implemented by the caller from
/// its own sources (security policy, run tool surface); a manifest never
/// contributes to it.
pub trait ToolGrant {
    /// `Ok` when the public tool `tool` may be used by this call.
    fn permits(&self, tool: &str) -> Result<(), String>;
}

/// SHA-256 of one file of the bundle, lowercase hex.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDigest {
    pub path: String,
    pub sha256: String,
}

/// What the review found in an executable bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutableReview {
    pub tools: Vec<String>,
    pub executables: Vec<FileDigest>,
}

/// Whether the tool kind is run by `skill_execute`.
#[must_use]
pub fn is_executable_kind(kind: &str) -> bool {
    matches!(kind.to_ascii_lowercase().as_str(), "shell" | "script" | "http")
}

/// The public tool a direct call would use for the kind: the policy a
/// declared tool runs under is at least the one of that tool.
#[must_use]
pub const fn equivalent_direct_tool(kind: &str) -> &'static str {
    if kind.eq_ignore_ascii_case("http") {
        "http_request"
    } else {
        "shell"
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The `[[tools]]` of a bundle. An absent `SKILL.toml` declares none.
pub fn declared_tools(bundle: &Bundle) -> Result<Vec<SkillTool>, ExecutableError> {
    let Some(file) = bundle.file(SKILL_TOML) else {
        return Ok(Vec::new());
    };
    let text = std::str::from_utf8(file.bytes()).map_err(|_| ExecutableError::Manifest("not UTF-8".to_string()))?;
    let manifest: super::super::SkillManifest =
        toml::from_str(text).map_err(|e| ExecutableError::Manifest(e.to_string()))?;
    Ok(manifest.tools)
}

/// Whether the bundle has an executable file or a declared executable tool.
/// An unreadable `SKILL.toml` that mentions tools counts as executable: a
/// bundle is never treated as plain instructions because its tools do not parse.
#[must_use]
pub fn is_executable_bundle(bundle: &Bundle) -> bool {
    if bundle.files().iter().any(super::bundle::BundleFile::is_executable) {
        return true;
    }
    declared_tools(bundle).map_or_else(
        |_| {
            bundle
                .file(SKILL_TOML)
                .is_some_and(|file| std::str::from_utf8(file.bytes()).map_or(true, |text| text.contains("[[tools]]")))
        },
        |tools| tools.iter().any(|tool| is_executable_kind(&tool.kind)),
    )
}

fn invalid_tool(tool: &SkillTool, reason: impl Into<String>) -> ExecutableError {
    ExecutableError::InvalidTool {
        tool: tool.name.clone(),
        reason: reason.into(),
    }
}

fn valid_ident(value: &str, max: usize, extra: &[char]) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || extra.contains(&c))
}

/// The bundle paths a tool runs: its script, or the executable files its
/// shell command names.
fn files_run_by<'a>(bundle: &'a Bundle, tool: &SkillTool) -> Vec<&'a str> {
    if tool.kind.eq_ignore_ascii_case("script") {
        return normalize_relative_path(Path::new(tool.command.trim()))
            .ok()
            .and_then(|path| bundle.file(&path))
            .map(|file| vec![file.path()])
            .unwrap_or_default();
    }
    if tool.kind.eq_ignore_ascii_case("shell") {
        return bundle
            .files()
            .iter()
            .filter(|file| tool.command.contains(file.path()))
            .map(super::bundle::BundleFile::path)
            .collect();
    }
    Vec::new()
}

/// Checks one declared tool against its bundle. Run at review and again at
/// every execution, so a bundle imported by another path is held to it too.
fn check_tool(bundle: &Bundle, tool: &SkillTool) -> Result<(), ExecutableError> {
    if !valid_ident(&tool.name, MAX_TOOL_NAME_BYTES, &[]) {
        return Err(invalid_tool(
            tool,
            "a tool name is 1..=64 ASCII letters, digits, '-' or '_'",
        ));
    }
    let command = tool.command.trim();
    if command.is_empty() || command.len() > MAX_COMMAND_BYTES || command.contains('\0') {
        return Err(invalid_tool(tool, "the command is empty, too long or contains NUL"));
    }
    for key in tool.args.keys() {
        if !valid_ident(key, MAX_ARG_KEY_BYTES, &['.']) {
            return Err(invalid_tool(
                tool,
                format!("argument name {key:?} is not 1..=64 ASCII letters, digits, '-', '_' or '.'"),
            ));
        }
    }
    if tool.args.len() > MAX_ARGS {
        return Err(invalid_tool(tool, format!("more than {MAX_ARGS} declared arguments")));
    }
    match tool.kind.to_ascii_lowercase().as_str() {
        "script" => {
            let path = normalize_relative_path(Path::new(command))
                .map_err(|e| invalid_tool(tool, format!("the script path is not a bundle path: {e}")))?;
            let file = bundle
                .file(&path)
                .ok_or_else(|| invalid_tool(tool, format!("the script {path:?} is not part of the bundle")))?;
            if !file.is_executable() {
                return Err(invalid_tool(
                    tool,
                    format!("the script {path:?} is not marked executable in the bundle"),
                ));
            }
        }
        "shell" => {}
        "http" => {
            if !(command.starts_with("https://") || command.starts_with("http://")) {
                return Err(invalid_tool(tool, "an http tool needs an http(s) URL"));
            }
        }
        other => {
            return Err(invalid_tool(
                tool,
                format!("unsupported kind {other:?}; supported: shell, script, http"),
            ));
        }
    }
    Ok(())
}

/// The review an executable revision gets before an operator publishes it.
///
/// Stricter than the checks of an instruction bundle: the manifests must
/// validate strictly, the bundle must not carry an untrusted-origin marker,
/// every declared tool must be well formed (a script tool names an executable
/// file of the bundle), tool names must be unique, and every executable file
/// must be run by some declared tool, so nothing executable rides along
/// unreferenced. File and path validity (traversal, symlinks, duplicates,
/// case collisions, size, UTF-8 names) was already enforced when the bundle
/// was built, and the result of this review is bound to the complete bundle
/// hash, which covers every script, reference and mode.
pub fn review_executable_bundle(bundle: &Bundle) -> Result<ExecutableReview, ExecutableError> {
    validate_manifest_strict(bundle).map_err(LifecycleError::from)?;
    if bundle.file(UNTRUSTED_ORIGIN_MARKER).is_some() {
        return Err(ExecutableError::UntrustedOrigin);
    }
    let tools = declared_tools(bundle)?;
    if tools.len() > MAX_TOOLS {
        return Err(ExecutableError::Manifest(format!("more than {MAX_TOOLS} tools")));
    }
    let mut names = BTreeSet::new();
    let mut referenced: BTreeSet<&str> = BTreeSet::new();
    for tool in &tools {
        if !names.insert(tool.name.to_ascii_lowercase()) {
            return Err(invalid_tool(tool, "the tool name is declared twice"));
        }
        check_tool(bundle, tool)?;
        referenced.extend(files_run_by(bundle, tool));
    }
    if !tools.iter().any(|tool| is_executable_kind(&tool.kind)) {
        return Err(ExecutableError::NotExecutable);
    }
    let mut executables = Vec::new();
    for file in bundle.files().iter().filter(|file| file.is_executable()) {
        if !referenced.contains(file.path()) {
            return Err(ExecutableError::UnreferencedExecutable {
                path: file.path().to_string(),
            });
        }
        executables.push(FileDigest {
            path: file.path().to_string(),
            sha256: sha256_hex(file.bytes()),
        });
    }
    Ok(ExecutableReview {
        tools: tools.into_iter().map(|tool| tool.name).collect(),
        executables,
    })
}

/// Validates the arguments of one call against the arguments the pinned
/// manifest declares for the tool and merges them over the declared defaults.
/// Only declared names are accepted, values are scalars of bounded size
/// without NUL, and the set is bounded.
pub fn validate_arguments(
    tool: &SkillTool,
    arguments: Option<&serde_json::Value>,
) -> Result<BTreeMap<String, String>, ExecutableError> {
    let bad = |reason: String| ExecutableError::InvalidArguments(reason);
    let mut values: BTreeMap<String, String> = tool
        .args
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let object = match arguments {
        None | Some(serde_json::Value::Null) => return Ok(values),
        Some(serde_json::Value::Object(object)) => object,
        Some(_) => return Err(bad("the arguments must be an object".to_string())),
    };
    if object.len() > MAX_ARGS {
        return Err(bad(format!("more than {MAX_ARGS} arguments")));
    }
    for (key, value) in object {
        if !tool.args.contains_key(key) {
            let shown: String = key.chars().take(MAX_ARG_KEY_BYTES).collect();
            return Err(bad(format!(
                "{shown:?} is not an argument the tool declares (declared: {})",
                tool.args.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
        }
        let text = match value {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Number(number) => number.to_string(),
            serde_json::Value::Bool(flag) => flag.to_string(),
            serde_json::Value::Null => String::new(),
            _ => return Err(bad(format!("argument {key:?} must be a scalar"))),
        };
        if text.len() > MAX_ARG_VALUE_BYTES || text.contains('\0') {
            return Err(bad(format!(
                "argument {key:?} is longer than {MAX_ARG_VALUE_BYTES} bytes or contains NUL"
            )));
        }
        values.insert(key.clone(), text);
    }
    Ok(values)
}

/// Everything the identity of one execution depends on. The key is the
/// digest over all of it: the complete bundle (revision id and the digests of
/// the files the tool runs), the declared tool and its argument schema, and
/// the settings, environment and policy the call runs under. Nothing about
/// resolution is cached; the key names a run in the audit record, and two runs
/// with equal keys cannot differ in any of these.
#[derive(Clone, Debug)]
pub struct ExecutionIdentity<'a> {
    pub revision_id: &'a RevisionId,
    pub tool: &'a SkillTool,
    pub files: &'a [FileDigest],
    pub settings: &'a str,
    pub environment: &'a str,
    pub policy: &'a str,
}

impl ExecutionIdentity<'_> {
    /// Lowercase hex SHA-256 of the length-prefixed fields.
    #[must_use]
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(EXECUTION_IDENTITY_DOMAIN);
        let mut field = |bytes: &[u8]| {
            hasher.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
            hasher.update(bytes);
        };
        field(self.revision_id.as_str().as_bytes());
        field(self.tool.name.as_bytes());
        field(self.tool.kind.to_ascii_lowercase().as_bytes());
        field(self.tool.command.as_bytes());
        let mut declared: Vec<_> = self.tool.args.iter().collect();
        declared.sort();
        for (key, default) in declared {
            field(key.as_bytes());
            field(default.as_bytes());
        }
        for file in self.files {
            field(file.path.as_bytes());
            field(file.sha256.as_bytes());
        }
        field(self.settings.as_bytes());
        field(self.environment.as_bytes());
        field(self.policy.as_bytes());
        hex::encode(hasher.finalize())
    }
}

/// A private run directory. Removed by [`PreparedExecution::finish`], and
/// removed best-effort when the owner is dropped (a cancelled call).
#[derive(Debug)]
struct RunDir {
    path: PathBuf,
    removed: bool,
}

impl RunDir {
    fn remove(&mut self) -> Result<(), ExecutableError> {
        if self.removed {
            return Ok(());
        }
        match fs::remove_dir_all(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(ExecutableError::Cleanup {
                    path: self.path.clone(),
                    reason: e.to_string(),
                });
            }
        }
        self.removed = true;
        Ok(())
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            tracing::warn!(%error, "failed to remove a skill run directory");
        }
    }
}

/// One tool call, resolved from the pinned revision and staged.
#[derive(Debug)]
pub struct PreparedExecution {
    pub pin: PinnedSkillRevision,
    /// Whether the revision came from a task pin.
    pub pinned: bool,
    pub tool: SkillTool,
    /// Declared defaults merged with the validated call arguments.
    pub arguments: BTreeMap<String, String>,
    /// Digests of the files the tool runs, taken from the verified bundle.
    pub files: Vec<FileDigest>,
    run: Option<RunDir>,
}

impl PreparedExecution {
    /// The staged copy of the pinned bundle the tool runs in; `None` for an
    /// http tool, which runs no local file.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.run.as_ref().map(|run| run.path.as_path())
    }

    /// Ends the run: removes the run directory. Call it after the process
    /// tree was killed and reaped; a failure is an error, not a warning,
    /// because nothing may outlive the call.
    pub fn finish(mut self) -> Result<(), ExecutableError> {
        self.run.as_mut().map_or(Ok(()), RunDir::remove)
    }
}

/// One `skill_execute` call to resolve.
pub struct PrepareRequest<'a> {
    pub workspace_dir: &'a Path,
    pub owner: &'a OwnerScope,
    pub skill_name: &'a str,
    pub tool_name: &'a str,
    /// The trusted task; the call is pinned to the revision the task first
    /// used. Without one the call resolves the active binding for itself.
    pub task_id: Option<&'a str>,
    pub arguments: Option<&'a serde_json::Value>,
    pub grant: &'a dyn ToolGrant,
}

/// Resolves a declared tool of a lifecycle-managed skill and stages it.
///
/// `Ok(None)` means the skill is not managed for this owner and the caller
/// keeps the legacy behaviour. Otherwise either everything holds (the
/// revision and the whole bundle verified against the pin, the tool declared
/// by the pinned manifest and well formed, its arguments valid, the policy
/// granting the tool's kind and every capability the revision requires, the
/// staged copy identical to the pinned bundle) or the call is refused.
///
/// Blocking: reads and writes files. Async callers use `spawn_blocking`.
pub fn prepare_execution(request: &PrepareRequest<'_>) -> Result<Option<PreparedExecution>, ExecutableError> {
    let Some(ResolvedRevision {
        store,
        pin,
        pinned,
        verified,
    }) = resolve_revision(
        request.workspace_dir,
        request.owner,
        request.skill_name,
        request.task_id,
    )?
    else {
        return Ok(None);
    };
    let tools = declared_tools(&verified.bundle)?;
    let tool = tools
        .into_iter()
        .find(|tool| tool.name.eq_ignore_ascii_case(request.tool_name) && is_executable_kind(&tool.kind))
        .ok_or_else(|| ExecutableError::UndeclaredTool {
            skill: request.skill_name.to_string(),
            tool: request.tool_name.chars().take(MAX_TOOL_NAME_BYTES).collect(),
            revision: pin.revision_id.to_string(),
        })?;
    check_tool(&verified.bundle, &tool)?;
    let arguments = validate_arguments(&tool, request.arguments)?;

    // The policy the call runs under: the direct tool of its kind, and every
    // tool the pinned revision declares it needs. A refusal of any is final.
    let mut needed: BTreeSet<String> = BTreeSet::new();
    needed.insert(equivalent_direct_tool(&tool.kind).to_string());
    for capability in &verified.revision.capability_requirements {
        needed.insert(capability_tool(capability).to_string());
    }
    for name in &needed {
        request
            .grant
            .permits(name)
            .map_err(|reason| ExecutableError::NotGranted {
                tool: name.clone(),
                reason,
            })?;
    }

    let files: Vec<FileDigest> = files_run_by(&verified.bundle, &tool)
        .into_iter()
        .filter_map(|path| verified.bundle.file(path))
        .map(|file| FileDigest {
            path: file.path().to_string(),
            sha256: sha256_hex(file.bytes()),
        })
        .collect();
    let run = if tool.kind.eq_ignore_ascii_case("http") {
        None
    } else {
        let run_root = store
            .root()
            .parent()
            .ok_or_else(|| ExecutableError::Stage("the store root has no parent directory".to_string()))?
            .join(RUN_ROOT_DIR);
        Some(stage_bundle(&run_root, &verified.bundle)?)
    };
    Ok(Some(PreparedExecution {
        pin,
        pinned,
        tool,
        arguments,
        files,
        run,
    }))
}

/// The public tool name inside a canonical capability id
/// (`family:namespace:name`).
fn capability_tool(capability: &str) -> &str {
    capability.rsplit(':').next().unwrap_or(capability)
}

#[cfg(unix)]
fn create_run_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_run_dir(path: &Path) -> std::io::Result<()> {
    fs::create_dir(path)
}

fn stage_error(path: &Path, error: impl std::fmt::Display) -> ExecutableError {
    ExecutableError::Stage(format!("{}: {error}", path.display()))
}

/// Writes the complete bundle into a fresh private directory, then reads every
/// file back and compares bytes and execute bit with the verified bundle.
fn stage_bundle(run_root: &Path, bundle: &Bundle) -> Result<RunDir, ExecutableError> {
    ensure_real_dir(run_root, true)?;
    reap_stale_runs(run_root);
    let path = run_root.join(format!("{RUN_DIR_PREFIX}{}", uuid::Uuid::new_v4().simple()));
    create_run_dir(&path).map_err(|e| stage_error(&path, e))?;
    let mut run = RunDir { path, removed: false };
    // Canonical, so that containment checks against it hold on platforms
    // whose temporary roots are symlinks.
    run.path = fs::canonicalize(&run.path).map_err(|e| stage_error(&run.path, e))?;
    for file in bundle.files() {
        let target = run.path.join(file.path());
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| stage_error(parent, e))?;
        }
        write_private_file(&target, file.bytes(), file.is_executable())?;
    }
    for file in bundle.files() {
        let target = run.path.join(file.path());
        let staged = fs::read(&target).map_err(|e| stage_error(&target, e))?;
        let differs = || ExecutableError::StagedFileDiffers {
            path: file.path().to_string(),
        };
        if sha256_hex(&staged) != sha256_hex(file.bytes()) {
            return Err(differs());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&target)
                .map_err(|e| stage_error(&target, e))?
                .permissions()
                .mode();
            if (mode & 0o111 != 0) != file.is_executable() {
                return Err(differs());
            }
        }
    }
    Ok(run)
}

/// Removes run directories of dead processes. Best effort and bounded.
fn reap_stale_runs(run_root: &Path) {
    let Ok(entries) = fs::read_dir(run_root) else {
        return;
    };
    for entry in entries.take(MAX_RUN_SCAN).flatten() {
        let stale = entry.file_name().to_string_lossy().starts_with(RUN_DIR_PREFIX)
            && entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age > STALE_RUN_AGE);
        if stale && let Err(error) = fs::remove_dir_all(entry.path()) {
            tracing::warn!(%error, path = %entry.path().display(), "failed to remove a stale skill run directory");
        }
    }
}

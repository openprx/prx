use super::{Context, Deserialize, OnceLock, Path, PathBuf, Result, RwLock, Serialize, fs, sync_directory};

pub(super) fn default_config_and_workspace_dirs() -> Result<(PathBuf, PathBuf)> {
    let config_dir = default_config_dir()?;
    Ok((config_dir.clone(), config_dir.join("workspace")))
}

pub(super) const ACTIVE_WORKSPACE_STATE_FILE: &str = "active_workspace.toml";
pub(super) const PRIMARY_CONFIG_DIR_NAME: &str = ".openprx";
pub(super) const LEGACY_CONFIG_DIR_NAME: &str = ".openprx";

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ActiveWorkspaceState {
    pub(super) config_dir: String,
}

pub(super) fn default_config_dir() -> Result<PathBuf> {
    let home = user_home_dir().context("Could not find home directory")?;
    Ok(preferred_user_config_dir(&home))
}

/// The user's home directory. Every location under the home (the default
/// `~/.openprx`, other tools' credential files, `~` in a path) is resolved
/// through here (D-070, D-071).
#[cfg(not(test))]
pub fn user_home_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|u| u.home_dir().to_path_buf())
}

/// The home a test installed with [`set_test_home`], if any.
#[cfg(test)]
pub(super) static TEST_HOME: parking_lot::RwLock<Option<PathBuf>> = parking_lot::RwLock::new(None);

/// Mirror a test's change of `HOME` into [`user_home_dir`]. Call it before
/// changing the variable. A home a test sets is installed; restoring the
/// `HOME` the process started with, or removing it, goes back to the scratch
/// home, so the process's own `HOME` is never installed, even when it lies
/// under the temp directory.
#[cfg(test)]
pub(crate) fn follow_test_home_env(value: Option<&std::ffi::OsStr>) {
    let original = original_process_home();
    *TEST_HOME.write() = value.filter(|home| Some(*home) != original).map(PathBuf::from);
}

/// The `HOME` the lib-test process started with. Every in-crate change of
/// `HOME` goes through [`follow_test_home_env`] first, so the first call
/// reads the variable before any test changed it.
#[cfg(test)]
pub(crate) fn original_process_home() -> Option<&'static std::ffi::OsStr> {
    static ORIGINAL_HOME: OnceLock<Option<std::ffi::OsString>> = OnceLock::new();
    ORIGINAL_HOME.get_or_init(|| std::env::var_os("HOME")).as_deref()
}

/// The lib-test binary never resolves the operator's real home, and never
/// reads `HOME`: a test that forgets to point its config, workspace or key
/// material at a temporary directory lands in a per-process scratch home
/// instead of reading or writing the real `~/.openprx` (D-070). A test that
/// needs a home of its own sets `HOME` through a helper that calls
/// [`follow_test_home_env`].
#[cfg(test)]
pub fn user_home_dir() -> Option<PathBuf> {
    static SCRATCH_HOME: OnceLock<PathBuf> = OnceLock::new();
    let installed = TEST_HOME.read().clone();
    if installed.is_some() {
        return installed;
    }
    Some(
        SCRATCH_HOME
            .get_or_init(|| {
                let scratch = std::env::temp_dir().join(format!("openprx-lib-test-home-{}", std::process::id()));
                if let Err(error) = std::fs::create_dir_all(&scratch) {
                    tracing::warn!(
                        "could not create the lib-test scratch home {}: {error}",
                        scratch.display()
                    );
                }
                scratch
            })
            .clone(),
    )
}

/// The default OpenPRX directory under the user's home (`~/.openprx`),
/// without consulting a published `--config-dir`.
pub fn default_openprx_dir() -> Option<PathBuf> {
    user_home_dir().map(|home| preferred_user_config_dir(&home))
}

/// `path` with a leading `~` (alone or followed by a separator) replaced by
/// [`user_home_dir`]; any other path, or a `~` with no resolvable home, is
/// returned unchanged. `~user` forms are not expanded.
pub fn expand_home(path: &Path) -> PathBuf {
    let mut components = path.components();
    match components.next() {
        Some(std::path::Component::Normal(first)) if first == "~" => {
            user_home_dir().map_or_else(|| path.to_path_buf(), |home| home.join(components.as_path()))
        }
        _ => path.to_path_buf(),
    }
}

/// The platform's per-user configuration directory for OpenPRX (for example
/// `~/Library/Application Support/openprx` on macOS), derived from
/// [`user_home_dir`] in the lib-test binary so no test reaches the real one.
#[cfg(not(test))]
pub fn platform_config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "openprx").map(|dirs| dirs.config_dir().to_path_buf())
}

/// See the non-test variant: under the lib-test home instead.
#[cfg(test)]
pub fn platform_config_dir() -> Option<PathBuf> {
    user_home_dir().map(|home| home.join(".platform-config").join("openprx"))
}

pub(super) fn preferred_user_config_dir(home: &Path) -> PathBuf {
    let primary = home.join(PRIMARY_CONFIG_DIR_NAME);
    if primary.exists() {
        return primary;
    }

    let legacy = home.join(LEGACY_CONFIG_DIR_NAME);
    if legacy.exists() {
        return legacy;
    }

    primary
}

pub(super) fn active_workspace_state_path(default_dir: &Path) -> PathBuf {
    default_dir.join(ACTIVE_WORKSPACE_STATE_FILE)
}

pub(super) async fn load_persisted_workspace_dirs(default_config_dir: &Path) -> Result<Option<(PathBuf, PathBuf)>> {
    let state_path = active_workspace_state_path(default_config_dir);
    if !state_path.exists() {
        return Ok(None);
    }

    let contents = match fs::read_to_string(&state_path).await {
        Ok(contents) => contents,
        Err(error) => {
            tracing::warn!(
                "Failed to read active workspace marker {}: {error}",
                state_path.display()
            );
            return Ok(None);
        }
    };

    let state: ActiveWorkspaceState = match toml::from_str(&contents) {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(
                "Failed to parse active workspace marker {}: {error}",
                state_path.display()
            );
            return Ok(None);
        }
    };

    let raw_config_dir = state.config_dir.trim();
    if raw_config_dir.is_empty() {
        tracing::warn!(
            "Ignoring active workspace marker {} because config_dir is empty",
            state_path.display()
        );
        return Ok(None);
    }

    let parsed_dir = PathBuf::from(raw_config_dir);
    let config_dir = if parsed_dir.is_absolute() {
        parsed_dir
    } else {
        default_config_dir.join(parsed_dir)
    };
    Ok(Some((config_dir.clone(), config_dir.join("workspace"))))
}

pub(crate) async fn persist_active_workspace_config_dir(config_dir: &Path) -> Result<()> {
    let default_config_dir = default_config_dir()?;
    persist_active_workspace_config_dir_at(config_dir, &default_config_dir).await
}

pub(crate) async fn persist_active_workspace_config_dir_at(config_dir: &Path, default_config_dir: &Path) -> Result<()> {
    let state_path = active_workspace_state_path(&default_config_dir);

    if config_dir == default_config_dir {
        if state_path.exists() {
            fs::remove_file(&state_path)
                .await
                .with_context(|| format!("Failed to clear active workspace marker: {}", state_path.display()))?;
        }
        return Ok(());
    }

    fs::create_dir_all(&default_config_dir).await.with_context(|| {
        format!(
            "Failed to create default config directory: {}",
            default_config_dir.display()
        )
    })?;

    let state = ActiveWorkspaceState {
        config_dir: config_dir.to_string_lossy().into_owned(),
    };
    let serialized = toml::to_string_pretty(&state).context("Failed to serialize active workspace marker")?;

    let temp_path = default_config_dir.join(format!(".{ACTIVE_WORKSPACE_STATE_FILE}.tmp-{}", uuid::Uuid::new_v4()));
    fs::write(&temp_path, serialized).await.with_context(|| {
        format!(
            "Failed to write temporary active workspace marker: {}",
            temp_path.display()
        )
    })?;

    if let Err(error) = fs::rename(&temp_path, &state_path).await {
        let _ = fs::remove_file(&temp_path).await;
        anyhow::bail!(
            "Failed to atomically persist active workspace marker {}: {error}",
            state_path.display()
        );
    }

    sync_directory(&default_config_dir).await?;
    Ok(())
}

pub(super) fn resolve_config_dir_for_workspace(workspace_dir: &Path) -> (PathBuf, PathBuf) {
    let workspace_config_dir = workspace_dir.to_path_buf();
    if workspace_config_dir.join("config.toml").exists() {
        return (workspace_config_dir.clone(), workspace_config_dir.join("workspace"));
    }

    let legacy_candidates = workspace_dir.parent().map(|parent| {
        [
            parent.join(PRIMARY_CONFIG_DIR_NAME),
            parent.join(LEGACY_CONFIG_DIR_NAME),
        ]
    });
    if let Some(candidates) = legacy_candidates {
        for legacy_dir in &candidates {
            if legacy_dir.join("config.toml").exists() {
                return (legacy_dir.clone(), workspace_config_dir);
            }
        }

        if workspace_dir
            .file_name()
            .is_some_and(|name| name == std::ffi::OsStr::new("workspace"))
        {
            return (candidates[0].clone(), workspace_config_dir);
        }
    }

    (workspace_config_dir.clone(), workspace_config_dir.join("workspace"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConfigResolutionSource {
    EnvConfigDir,
    EnvWorkspace,
    ActiveWorkspaceMarker,
    DefaultConfigDir,
}

impl ConfigResolutionSource {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::EnvConfigDir => "OPENPRX_CONFIG_DIR",
            Self::EnvWorkspace => "OPENPRX_WORKSPACE",
            Self::ActiveWorkspaceMarker => "active_workspace.toml",
            Self::DefaultConfigDir => "default",
        }
    }
}

pub(super) fn env_openprx(name_suffix: &str) -> std::result::Result<String, std::env::VarError> {
    std::env::var(format!("OPENPRX_{name_suffix}"))
}

/// Minimal `runtime` view used during process bootstrap.
///
/// Deliberately *not* the full [`Config`]: this is parsed before the tokio
/// runtime exists, so it must stay synchronous, allocation-light and immune to
/// validation failures in unrelated sections.
#[derive(Debug, Default, Deserialize)]
pub(super) struct BootstrapRuntimeSection {
    #[serde(default)]
    pub(super) max_blocking_threads: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct BootstrapConfig {
    #[serde(default)]
    pub(super) runtime: BootstrapRuntimeSection,
}

/// Synchronous, best-effort resolution of the config directory for bootstrap
/// reads.
///
/// Mirrors [`resolve_runtime_config_dirs`] minus its async marker read, which is
/// replaced by a blocking read of the same file. Any failure degrades to the
/// default directory rather than propagating: bootstrap must never prevent the
/// process from starting.
pub(super) fn bootstrap_config_dir(explicit_config_dir: Option<&str>) -> Option<PathBuf> {
    if let Some(dir) = explicit_config_dir {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }

    if let Ok(custom_config_dir) = env_openprx("CONFIG_DIR") {
        let custom_config_dir = custom_config_dir.trim();
        if !custom_config_dir.is_empty() {
            return Some(PathBuf::from(custom_config_dir));
        }
    }

    if let Ok(custom_workspace) = env_openprx("WORKSPACE") {
        if !custom_workspace.is_empty() {
            let (config_dir, _) = resolve_config_dir_for_workspace(&PathBuf::from(custom_workspace));
            return Some(config_dir);
        }
    }

    let default_dir = default_config_dir().ok()?;
    let state_path = active_workspace_state_path(&default_dir);
    if let Ok(contents) = std::fs::read_to_string(&state_path) {
        if let Ok(state) = toml::from_str::<ActiveWorkspaceState>(&contents) {
            let raw = state.config_dir.trim();
            if !raw.is_empty() {
                let parsed = PathBuf::from(raw);
                return Some(if parsed.is_absolute() {
                    parsed
                } else {
                    default_dir.join(parsed)
                });
            }
        }
    }

    Some(default_dir)
}

/// Read `[runtime] max_blocking_threads` before the tokio runtime is built.
///
/// The tokio blocking-pool size is fixed at `Builder` time, so it cannot come
/// from the normal (async, side-effecting) config load — that load itself needs
/// a runtime. This performs a synchronous, read-only, side-effect-free parse of
/// the same merged TOML.
///
/// Returns `None` when no config exists yet, when the key is unset, or when
/// anything at all goes wrong; the caller then uses
/// [`crate::runtime::blocking::default_max_blocking_threads`]. A value of zero
/// is rejected here as well as in [`Config::validate`], so a malformed file
/// cannot produce an unusable pool.
#[must_use]
pub fn bootstrap_max_blocking_threads(explicit_config_dir: Option<&str>) -> Option<usize> {
    let config_dir = bootstrap_config_dir(explicit_config_dir)?;
    let config_path = config_dir.join("config.toml");
    if !config_path.is_file() {
        return None;
    }

    let merged = crate::config::files::read_merged_toml_with_gate_once(&config_path).ok()?;
    let bootstrap: BootstrapConfig = merged.try_into().ok()?;
    bootstrap.runtime.max_blocking_threads.filter(|value| *value > 0)
}

pub(super) async fn resolve_runtime_config_dirs(
    default_openprx_dir: &Path,
    default_workspace_dir: &Path,
    explicit_config_dir: Option<&str>,
) -> Result<(PathBuf, PathBuf, ConfigResolutionSource)> {
    // CLI --config-dir takes highest priority
    if let Some(dir) = explicit_config_dir {
        let dir = dir.trim();
        if !dir.is_empty() {
            let openprx_dir = std::path::absolute(dir).context("resolve --config-dir against the process directory")?;
            return Ok((
                openprx_dir.clone(),
                openprx_dir.join("workspace"),
                ConfigResolutionSource::EnvConfigDir,
            ));
        }
    }

    if let Ok(custom_config_dir) = env_openprx("CONFIG_DIR") {
        let custom_config_dir = custom_config_dir.trim();
        if !custom_config_dir.is_empty() {
            let openprx_dir = std::path::absolute(custom_config_dir).context("resolve OPENPRX_CONFIG_DIR")?;
            return Ok((
                openprx_dir.clone(),
                openprx_dir.join("workspace"),
                ConfigResolutionSource::EnvConfigDir,
            ));
        }
    }

    if let Ok(custom_workspace) = env_openprx("WORKSPACE") {
        if !custom_workspace.is_empty() {
            let absolute = std::path::absolute(custom_workspace).context("resolve OPENPRX_WORKSPACE")?;
            let (openprx_dir, workspace_dir) = resolve_config_dir_for_workspace(&absolute);
            return Ok((openprx_dir, workspace_dir, ConfigResolutionSource::EnvWorkspace));
        }
    }

    if let Some((openprx_dir, workspace_dir)) = load_persisted_workspace_dirs(default_openprx_dir).await? {
        return Ok((
            openprx_dir,
            workspace_dir,
            ConfigResolutionSource::ActiveWorkspaceMarker,
        ));
    }

    Ok((
        default_openprx_dir.to_path_buf(),
        default_workspace_dir.to_path_buf(),
        ConfigResolutionSource::DefaultConfigDir,
    ))
}

/// The config directory this process actually resolved, published once the CLI
/// has decided it.
///
/// Most on-disk state is reached through [`Config::config_path`] and therefore
/// already follows `--config-dir`. A few process-global artefacts (the runtime
/// witness key, the session-worker secret) are initialised lazily from far
/// inside the call graph, where no `Config` is in hand; without this they
/// anchor on `$HOME/.openprx` and write into the operator's real configuration
/// even when the run was pointed somewhere else entirely.
pub(super) static PROCESS_CONFIG_DIR: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();

pub(super) fn process_config_dir_state() -> &'static RwLock<Option<PathBuf>> {
    PROCESS_CONFIG_DIR.get_or_init(|| RwLock::new(None))
}

/// Publish the resolved config directory for this process.
///
/// First writer wins: the CLI publishes the explicit `--config-dir` before any
/// config load, and the post-load publication of the fully resolved directory
/// must not move state that has already been created under the explicit path.
pub fn set_process_config_dir(dir: PathBuf) {
    let state = process_config_dir_state();
    let mut guard = state.write();
    if guard.is_none() {
        *guard = Some(dir);
    }
}

/// The config directory published by [`set_process_config_dir`], if any.
#[must_use]
pub fn process_config_dir() -> Option<PathBuf> {
    process_config_dir_state().read().clone()
}

/// Replace the published config directory and return the previous value, so a
/// test can install a temporary directory and restore the process afterwards.
#[cfg(test)]
pub(crate) fn swap_process_config_dir_for_tests(dir: Option<PathBuf>) -> Option<PathBuf> {
    std::mem::replace(&mut *process_config_dir_state().write(), dir)
}

/// Serialises every test that swaps the process-wide config directory, which is
/// shared by the whole lib-test binary.
#[cfg(test)]
pub(crate) fn process_config_dir_test_lock() -> parking_lot::MutexGuard<'static, ()> {
    static PROCESS_CONFIG_DIR_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    PROCESS_CONFIG_DIR_TEST_LOCK.lock()
}

/// Trim a directory-valued CLI argument, rejecting a present-but-blank value.
pub(super) fn trimmed_dir_arg<'a>(value: Option<&'a str>, flag: &str) -> Result<Option<&'a str>> {
    match value {
        None => Ok(None),
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                anyhow::bail!("{flag} cannot be empty");
            }
            Ok(Some(trimmed))
        }
    }
}

/// Whether two init targets name the same directory.
///
/// Canonicalisation is best-effort: `prx init` legitimately targets a directory
/// that does not exist yet, and a path that cannot be canonicalised is compared
/// literally rather than silently treated as equal.
pub(super) fn init_targets_are_same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Resolve the directory `prx init` writes into.
///
/// Precedence: `init --dir` > the global `--config-dir` > `OPENPRX_CONFIG_DIR` /
/// `OPENPRX_WORKSPACE` / the active-workspace marker > `~/.openprx`.
///
/// `init` rewrites a whole configuration generation, so the two explicit forms
/// are never reconciled by guessing: naming different directories with `--dir`
/// and `--config-dir` is an error. Before this existed, `init` read only its own
/// `--dir` and silently rewrote the default directory whenever the run was
/// scoped with `--config-dir`, which destroyed the structure of the operator's
/// real configuration while leaving the requested directory untouched.
pub async fn resolve_init_target_dir(
    explicit_init_dir: Option<&str>,
    explicit_config_dir: Option<&str>,
) -> Result<PathBuf> {
    let init_dir = trimmed_dir_arg(explicit_init_dir, "--dir")?;
    let config_dir = trimmed_dir_arg(explicit_config_dir, "--config-dir")?;

    if let (Some(init_dir), Some(config_dir)) = (init_dir, config_dir) {
        if !init_targets_are_same_dir(Path::new(init_dir), Path::new(config_dir)) {
            anyhow::bail!(
                "conflicting init targets: --dir {init_dir} and --config-dir {config_dir} name \
                 different directories. Pass only one: `prx init` rewrites a whole configuration \
                 generation and will not guess which one you meant."
            );
        }
    }

    if let Some(dir) = init_dir {
        return Ok(PathBuf::from(dir));
    }

    let (default_openprx_dir, default_workspace_dir) = default_config_and_workspace_dirs()?;
    let (openprx_dir, _workspace_dir, _source) =
        resolve_runtime_config_dirs(&default_openprx_dir, &default_workspace_dir, config_dir).await?;
    Ok(openprx_dir)
}

pub(super) fn config_dir_creation_error(path: &Path) -> String {
    format!(
        "Failed to create config directory: {}. If running as an OpenRC service, \
         ensure this path is writable by user 'prx'.",
        path.display()
    )
}

//! The launch policy of stdio MCP servers (D-045, D-049 E).
//!
//! A stdio MCP server is a local program PRX starts as its own user. It can
//! read every file PRX can, including the stored credentials, and it
//! receives whatever environment PRX hands it. One classifier decides, for
//! every surface:
//!
//! - **what the child inherits** ([`launch_environment`]): the child starts
//!   from a cleared environment; only [`INHERITED_VARIABLES`] and the
//!   `LC_*` locale variables are copied from PRX, then the server's own
//!   configured `env` is added. Nothing else of PRX's environment (provider
//!   keys exported in the shell, tokens of other tools) reaches it;
//! - **which variables a workspace may set** ([`classify_variable`]): a
//!   workspace `mcp.json` is not an operator surface, so a variable that
//!   changes what the program loads or runs (dynamic-loader `LD_*`/`DYLD_*`,
//!   interpreter and package-manager hooks, `PATH`), where it reads its
//!   configuration (`HOME`, `XDG_*`) or whom its connections trust (CA
//!   bundles, proxies) is refused there by name and by prefix;
//! - **which configuration changes relaunch a program**
//!   ([`launch_server`]): adding a stdio server, or changing the
//!   `transport`, `command` or `args` of one, starts a different program with
//!   PRX's privileges, so the configuration plan classifies it high risk
//!   whether or not the server holds `env` values.

use crate::config::secret_paths::PathElem;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};

/// Variables of PRX's own environment a stdio MCP child inherits, besides
/// the `LC_*` locale variables. Everything else is cleared.
#[cfg(not(windows))]
pub const INHERITED_VARIABLES: &[&str] = &["PATH", "HOME", "LANG", "TMPDIR"];

/// Variables of PRX's own environment a stdio MCP child inherits, besides
/// the `LC_*` locale variables. Everything else is cleared. On Windows a
/// program, and the allowlisted launchers (`npx`, `node`, `python`, `uvx`),
/// need the system layout variables as well; none of them holds a secret.
#[cfg(windows)]
pub const INHERITED_VARIABLES: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "TMPDIR",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROCESSOR_ARCHITECTURE",
    "NUMBER_OF_PROCESSORS",
    "TEMP",
    "TMP",
];

/// Prefix of the locale variables a stdio MCP child inherits.
const LOCALE_PREFIX: &str = "LC_";

/// Prefixes of dynamic-loader variables: they make any program load code of
/// the setter's choosing (`LDR_*` is AIX's loader).
const LOADER_PREFIXES: &[&str] = &["LD_", "DYLD_", "_RLD", "LDR_"];

/// Single dynamic-loader variables.
const LOADER_VARIABLES: &[&str] = &["GLIBC_TUNABLES", "LIBPATH", "SHLIB_PATH", "GCONV_PATH"];

/// Prefixes of interpreter, runtime and package-manager variables that load
/// or run code at start-up, or choose where a launcher fetches the code it
/// runs (`npm_config_registry` substitutes the package `npx` runs).
const INTERPRETER_PREFIXES: &[&str] = &[
    "PYTHON",
    "NODE_",
    "PERL5",
    "PERLLIB",
    "RUBY",
    "BASH_FUNC_",
    "NPM_CONFIG_",
    "UV_",
    "PIP_",
    "CORECLR_",
    "COREHOST_",
    "DOTNET_",
    "CARGO_",
    "GEM_",
    "BUNDLE_",
    "DENO_",
    "BUN_",
    "RUSTUP_",
    // git and ssh run programs for the fetches npm, cargo and go make
    // (`GIT_SSH_COMMAND`, `GIT_ASKPASS`, `GIT_CONFIG_*`).
    "GIT_",
    "DOCKER_",
];

/// Single interpreter, runtime and package-manager variables that load or
/// run code at start-up.
const INTERPRETER_VARIABLES: &[&str] = &[
    "BASH_ENV",
    "ENV",
    "CDPATH",
    "SHELLOPTS",
    "PS4",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
    "JDK_JAVA_OPTIONS",
    "CLASSPATH",
    "PHPRC",
    "PHP_INI_SCAN_DIR",
    "GOFLAGS",
    "GOTOOLCHAIN",
    "GOPROXY",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTFLAGS",
    "GOENV",
    "GOPATH",
    "GOROOT",
    "GOSUMDB",
    "GONOSUMDB",
    "GONOSUMCHECK",
    "GOINSECURE",
    "GOPRIVATE",
    "SSH_ASKPASS",
];

/// Variables that choose which executable runs, besides `PATH` (Windows:
/// the command interpreter, the executable extensions, the system
/// directories).
const SEARCH_PATH_VARIABLES: &[&str] = &["PATH", "COMSPEC", "PATHEXT", "SYSTEMROOT", "WINDIR"];

/// Variables that decide where a program looks up its configuration (an
/// `.npmrc`, a `pip.conf`, a python user-site `.pth` file runs code).
const CONFIG_LOOKUP_VARIABLES: &[&str] = &[
    "HOME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "HOMEDRIVE",
    "HOMEPATH",
    "PROGRAMDATA",
];

/// Prefix of the XDG base-directory variables (configuration lookup).
const CONFIG_LOOKUP_PREFIXES: &[&str] = &["XDG_"];

/// Variables that decide whom a program's outbound connections trust or go
/// through: a CA bundle or a proxy lets the setter read and alter them.
const TRUST_VARIABLES: &[&str] = &[
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
];

/// Leaves of `mcp.servers.<name>` that decide which program runs (the
/// working directory decides which cwd-relative files it loads).
const LAUNCH_FIELDS: &[&str] = &["transport", "command", "args", "working_dir"];

/// The directory a stdio MCP server starts in (D-088 C-3): the absolute
/// `working_dir` the operator configured, or else a dedicated directory
/// `<launch_root>/<name>-<hash>` (the name reduced to `[A-Za-z0-9_-]`, plus
/// the first 12 hex digits of the SHA-256 of the exact name, so `a b`,
/// `a/b` and `a_b` never share one, D-095 N-4), never the directory PRX was
/// started from, which may hold a `.npmrc`, a `node_modules/.bin` or a
/// shadowing Python module.
///
/// # Errors
///
/// A configured `working_dir` that is not absolute.
pub fn launch_directory(
    launch_root: &std::path::Path,
    server_name: &str,
    configured: Option<&str>,
) -> Result<std::path::PathBuf, String> {
    use sha2::Digest as _;
    if let Some(configured) = configured.map(str::trim).filter(|dir| !dir.is_empty()) {
        let dir = std::path::PathBuf::from(configured);
        return if dir.is_absolute() {
            Ok(dir)
        } else {
            Err(format!(
                "MCP server '{}' has a relative `working_dir`; it must be an absolute path",
                server_name.escape_default()
            ))
        };
    }
    let mut name = server_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(48)
        .collect::<String>();
    if name.is_empty() || name.chars().all(|character| character == '_') {
        name = "server".to_string();
    }
    let digest = hex::encode(sha2::Sha256::digest(server_name.as_bytes()));
    let short = digest.get(..12).unwrap_or(&digest);
    Ok(launch_root.join(format!("{name}-{short}")))
}

/// What setting an environment variable of a launched program can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchVariable {
    /// A dynamic-loader variable (`LD_*`, `DYLD_*`, ...).
    Loader,
    /// An interpreter or shell start-up hook (`PYTHONPATH`, `NODE_OPTIONS`,
    /// `BASH_ENV`, ...).
    Interpreter,
    /// `PATH` and the Windows variables that choose an executable
    /// (`COMSPEC`, `PATHEXT`, `SYSTEMROOT`, `WINDIR`).
    SearchPath,
    /// Where a program looks up its configuration (`HOME`, `XDG_*`, ...).
    ConfigLookup,
    /// Whom a program's connections trust or go through (a CA bundle, a
    /// proxy).
    Trust,
    /// Anything else.
    Ordinary,
}

impl LaunchVariable {
    /// Why a workspace may not set a variable of this class; `None` for an
    /// ordinary variable.
    #[must_use]
    pub const fn refusal(self) -> Option<&'static str> {
        match self {
            Self::Loader => Some("dynamic-loader variable"),
            Self::Interpreter => Some("interpreter start-up variable"),
            Self::SearchPath => Some("executable search path"),
            Self::ConfigLookup => Some("configuration lookup directory"),
            Self::Trust => Some("certificate or proxy setting"),
            Self::Ordinary => None,
        }
    }
}

/// Classify an environment variable name. Names are compared in upper case,
/// so a lower-case spelling is never a way around a rule.
#[must_use]
pub fn classify_variable(name: &str) -> LaunchVariable {
    let upper = name.trim().to_ascii_uppercase();
    let named = |names: &[&str], prefixes: &[&str]| {
        names.contains(&upper.as_str()) || prefixes.iter().any(|prefix| upper.starts_with(prefix))
    };
    if named(LOADER_VARIABLES, LOADER_PREFIXES) {
        LaunchVariable::Loader
    } else if named(SEARCH_PATH_VARIABLES, &[]) {
        LaunchVariable::SearchPath
    } else if named(INTERPRETER_VARIABLES, INTERPRETER_PREFIXES) {
        LaunchVariable::Interpreter
    } else if named(CONFIG_LOOKUP_VARIABLES, CONFIG_LOOKUP_PREFIXES) {
        LaunchVariable::ConfigLookup
    } else if named(TRUST_VARIABLES, &[]) {
        LaunchVariable::Trust
    } else {
        LaunchVariable::Ordinary
    }
}

/// Whether a stdio MCP child inherits the variable `name` from PRX.
fn is_inherited(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    #[cfg(windows)]
    let name = name.to_ascii_uppercase();
    #[cfg(windows)]
    let name = name.as_str();
    INHERITED_VARIABLES.contains(&name) || name.starts_with(LOCALE_PREFIX)
}

/// The complete environment of a stdio MCP child: the inherited part of
/// `parent`, then the server's own `env` (which wins on a clash).
#[must_use]
pub fn launch_environment<S: std::hash::BuildHasher>(
    parent: impl IntoIterator<Item = (OsString, OsString)>,
    own: &HashMap<String, String, S>,
) -> Vec<(OsString, OsString)> {
    let mut environment = parent
        .into_iter()
        .filter(|(name, _)| is_inherited(name) && !own.contains_key(name.to_str().unwrap_or_default()))
        .collect::<Vec<_>>();
    let mut own = own
        .iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect::<Vec<_>>();
    own.sort();
    environment.extend(own);
    environment
}

/// Workspace files that define programs PRX launches: the command hooks of
/// `hooks.json` and the MCP servers of `mcp.json`.
const WORKSPACE_LAUNCH_FILES: &[&str] = &["hooks.json", "mcp.json"];

/// Workspace directory of WASM plugins. Everything under it is plugin code or
/// plugin configuration that PRX loads and runs.
const WORKSPACE_PLUGINS_DIR: &str = "plugins";

/// Whether `target` (a resolved path) is a workspace launch definition: the
/// `hooks.json` or `mcp.json` at the root of `workspace_dir`, or anything
/// under its `plugins/` directory (WASM plugin code and manifests). Names are
/// compared case-insensitively so a case-folding filesystem cannot hide them.
/// A file tool's write there is classified high risk and named as such
/// (HOOKS-ENV): it changes which programs run as PRX's user.
#[must_use]
pub fn is_workspace_launch_file(workspace_dir: &std::path::Path, target: &std::path::Path) -> bool {
    let workspace = std::fs::canonicalize(workspace_dir).unwrap_or_else(|_| workspace_dir.to_path_buf());
    is_root_launch_file(&workspace, target) || is_under_plugins_dir(&workspace, target) || is_plugin_trust_state(target)
}

/// The operator's plugin trust file (and its lock and temporary files) and the
/// compiled-component cache, wherever they are: they decide which WASM runs
/// and with which permissions, so a file tool's write is high risk even when
/// the path policy lets it leave the workspace.
fn is_plugin_trust_state(target: &std::path::Path) -> bool {
    let named = target.file_name().and_then(|name| name.to_str()).is_some_and(|name| {
        let name = name.trim_start_matches('.').to_ascii_lowercase();
        name.starts_with("plugin-trust.json")
    });
    named
        || target.components().any(|component| match component {
            std::path::Component::Normal(name) => name
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case("plugin-cache")),
            _ => false,
        })
}

fn is_root_launch_file(workspace: &std::path::Path, target: &std::path::Path) -> bool {
    let Some(name) = target.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !WORKSPACE_LAUNCH_FILES
        .iter()
        .any(|launch| launch.eq_ignore_ascii_case(name))
    {
        return false;
    }
    target
        .parent()
        .is_some_and(|parent| std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf()) == workspace)
}

/// Whether `target` lies at or under `<workspace>/plugins`. The deepest
/// existing ancestor is canonicalised first, so a symlink cannot hide the
/// directory, and the not-yet-existing tail is appended unchanged.
fn is_under_plugins_dir(workspace: &std::path::Path, target: &std::path::Path) -> bool {
    let mut existing = target;
    let mut tail = Vec::new();
    let resolved_base = loop {
        match std::fs::canonicalize(existing) {
            Ok(path) => break path,
            Err(_) => {
                let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
                    break existing.to_path_buf();
                };
                tail.push(name.to_os_string());
                existing = parent;
            }
        }
    };
    let resolved = tail.iter().rev().fold(resolved_base, |mut path, part| {
        path.push(part);
        path
    });
    resolved
        .strip_prefix(workspace)
        .ok()
        .and_then(|relative| relative.components().next())
        .is_some_and(|first| match first {
            std::path::Component::Normal(name) => name
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(WORKSPACE_PLUGINS_DIR)),
            _ => false,
        })
}

/// Whether `path` is an existing real directory (`Ok(true)`), absent
/// (`Ok(false)`), or something a launch must not use: a symlink or a
/// non-directory (an error naming `what` and the path).
///
/// # Errors
///
/// A symlink, a non-directory, or an unreadable path.
pub fn existing_real_directory(path: &std::path::Path, what: &str) -> anyhow::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => anyhow::bail!(
            "the {what} {} is a symlink or not a directory; refusing to start a program there",
            path.display()
        ),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Create (when missing) the dedicated directory `dir` under `root` that a
/// launched program starts in, owner-only on Unix. The root and the
/// directory are refused when they are a symlink or not a directory, never
/// followed or `chmod`ed (D-095 N-4). Shared by stdio MCP servers and
/// workspace command hooks.
///
/// # Errors
///
/// A symlinked or non-directory root or directory, or a filesystem error.
pub async fn prepare_launch_directory(root: &std::path::Path, dir: &std::path::Path) -> anyhow::Result<()> {
    if !existing_real_directory(root, "launch root")? {
        tokio::fs::create_dir_all(root).await?;
    }
    if !existing_real_directory(dir, "launch directory")? {
        match tokio::fs::create_dir(dir).await {
            Ok(()) => {}
            // Two launches of the same definition (two hooks of one event)
            // may race to create it: what the other created must still be
            // a real directory.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !existing_real_directory(dir, "launch directory")? {
                    return Err(error.into());
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // Not a symlink (checked above), so this changes the directory itself.
        tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

/// The environment of a program launched for a workspace-supplied
/// definition (a hook or a workspace MCP server): the launch allowlist of
/// PRX's own environment plus the definition's own `env`, from which every
/// variable a workspace may not set ([`classify_variable`]) is dropped.
/// Returns the environment and the dropped names (names only).
#[must_use]
pub fn workspace_launch_environment<S: std::hash::BuildHasher>(
    own: &HashMap<String, String, S>,
) -> (Vec<(OsString, OsString)>, Vec<String>) {
    let mut dropped = Vec::new();
    let allowed = own
        .iter()
        .filter(|(name, value)| {
            let refused = classify_variable(name).refusal().is_some() || name.contains('\0') || value.contains('\0');
            if refused {
                dropped.push((*name).clone());
            }
            !refused
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<HashMap<_, _>>();
    dropped.sort();
    (launch_environment(std::env::vars_os(), &allowed), dropped)
}

/// Every `working_dir` a change sets or alters (compared with `current`)
/// must be an existing directory that is not a symlink, so a plan refuses
/// it instead of the server failing at launch (D-095 N-4). A directory an
/// unchanged configuration already names is not re-checked.
///
/// # Errors
///
/// One sentence per refused server, joined; paths only.
pub fn check_working_dirs(
    current: Option<&crate::config::Config>,
    candidate: &crate::config::Config,
) -> Result<(), String> {
    let mut refusals = Vec::new();
    let mut servers = candidate.mcp.servers.iter().collect::<Vec<_>>();
    servers.sort_by(|left, right| left.0.cmp(right.0));
    for (name, server) in servers {
        let Some(dir) = server.working_dir.as_deref().map(str::trim) else {
            continue;
        };
        let unchanged = current
            .and_then(|current| current.mcp.servers.get(name))
            .and_then(|before| before.working_dir.as_deref())
            .is_some_and(|before| before.trim() == dir);
        if unchanged {
            continue;
        }
        let path = std::path::Path::new(dir);
        let problem = match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Some("is a symlink"),
            Ok(metadata) if !metadata.is_dir() => Some("is not a directory"),
            Ok(_) => None,
            Err(_) => Some("does not exist"),
        };
        if let Some(problem) = problem {
            refusals.push(format!(
                "mcp.servers.{}.working_dir {} {problem}; a stdio server starts only in an existing directory \
                 that is not a symlink",
                name.escape_default(),
                path.display()
            ));
        }
    }
    if refusals.is_empty() {
        Ok(())
    } else {
        Err(refusals.join("; "))
    }
}

/// The MCP server whose launch the configuration leaf `path` belongs to:
/// `mcp.servers.<name>.transport`, `.command`, or one element of `.args`.
#[must_use]
pub fn launch_server(path: &[PathElem]) -> Option<&str> {
    match path {
        [
            PathElem::Key(mcp),
            PathElem::Key(servers),
            PathElem::Key(name),
            PathElem::Key(field),
            rest @ ..,
        ] if mcp == "mcp"
            && servers == "servers"
            && LAUNCH_FIELDS.contains(&field.as_str())
            && matches!(
                (field.as_str(), rest),
                ("command" | "transport" | "working_dir", []) | ("args", [PathElem::Index(_)])
            ) =>
        {
            Some(name.as_str())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two launches of one definition (two hooks of one event) prepare the
    /// same directory at once; neither fails because the other created it.
    #[tokio::test]
    async fn concurrent_launches_prepare_one_directory() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let root = temp.path().join("hooks");
        let dir = root.join("workspace-hooks-0");
        let results = futures_util::future::join_all((0..8).map(|_| prepare_launch_directory(&root, &dir))).await;
        for result in results {
            result.expect("test: a concurrent launch failed to prepare its directory");
        }
        assert!(dir.is_dir());
    }

    #[test]
    fn loader_and_interpreter_variables_are_classified_by_prefix_and_case_insensitively() {
        for name in [
            "LD_PRELOAD",
            "LD_BIND_NOW",
            "ld_audit",
            "DYLD_INSERT_LIBRARIES",
            "DYLD_FALLBACK_LIBRARY_PATH",
            "dyld_framework_path",
            "_RLD_LIST",
        ] {
            assert_eq!(classify_variable(name), LaunchVariable::Loader, "{name}");
        }
        for name in [
            "PYTHONPATH",
            "PYTHONSTARTUP",
            "PYTHONHOME",
            "NODE_OPTIONS",
            "node_path",
            "PERL5OPT",
            "RUBYOPT",
            "BASH_ENV",
            "BASH_FUNC_ls%%",
            "JAVA_TOOL_OPTIONS",
        ] {
            assert_eq!(classify_variable(name), LaunchVariable::Interpreter, "{name}");
        }
        assert_eq!(classify_variable("path"), LaunchVariable::SearchPath);
        for name in ["GITHUB_TOKEN", "MCP_API_KEY", "LANG", "DEBUG", "OLD_PATH", "LOG_LEVEL"] {
            assert_eq!(classify_variable(name), LaunchVariable::Ordinary, "{name}");
        }
    }

    /// RED before PMME-f (D-085 F-2): these variables change what an
    /// allowlisted launcher loads, where it reads its configuration or whom
    /// it trusts, and were classified ordinary.
    #[test]
    fn launcher_configuration_trust_and_loader_families_are_not_ordinary() {
        for (name, class) in [
            ("npm_config_registry", LaunchVariable::Interpreter),
            ("NPM_CONFIG_USERCONFIG", LaunchVariable::Interpreter),
            ("UV_INDEX_URL", LaunchVariable::Interpreter),
            ("PIP_INDEX_URL", LaunchVariable::Interpreter),
            ("CLASSPATH", LaunchVariable::Interpreter),
            ("DOTNET_STARTUP_HOOKS", LaunchVariable::Interpreter),
            ("CORECLR_PROFILER_PATH", LaunchVariable::Interpreter),
            ("COREHOST_TRACE", LaunchVariable::Interpreter),
            ("PHPRC", LaunchVariable::Interpreter),
            ("PHP_INI_SCAN_DIR", LaunchVariable::Interpreter),
            ("GOFLAGS", LaunchVariable::Interpreter),
            ("GOPROXY", LaunchVariable::Interpreter),
            ("RUSTC_WRAPPER", LaunchVariable::Interpreter),
            ("CARGO_HOME", LaunchVariable::Interpreter),
            ("GEM_HOME", LaunchVariable::Interpreter),
            ("BUNDLE_GEMFILE", LaunchVariable::Interpreter),
            ("GLIBC_TUNABLES", LaunchVariable::Loader),
            ("LDR_PRELOAD", LaunchVariable::Loader),
            ("HOME", LaunchVariable::ConfigLookup),
            ("xdg_config_home", LaunchVariable::ConfigLookup),
            ("SSL_CERT_FILE", LaunchVariable::Trust),
            ("REQUESTS_CA_BUNDLE", LaunchVariable::Trust),
            ("https_proxy", LaunchVariable::Trust),
            ("ALL_PROXY", LaunchVariable::Trust),
        ] {
            assert_eq!(classify_variable(name), class, "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_launch_directory_is_dedicated_unless_configured_absolute() {
        let root = std::path::Path::new("/srv/prx/mcp");
        let dir = |name: &str| launch_directory(root, name, None).expect("test: default");
        let name_of = |name: &str| {
            dir(name)
                .file_name()
                .map(|file| file.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        assert!(
            name_of("git hub/../x").starts_with("git_hub____x-"),
            "{}",
            name_of("git hub/../x")
        );
        assert!(name_of("").starts_with("server-"));
        assert_eq!(dir("a").parent(), Some(root));
        // D-095 N-4: names that reduce alike get different directories.
        assert_ne!(dir("a b"), dir("a/b"));
        assert_ne!(dir("a b"), dir("a_b"));
        assert_eq!(dir("a b"), dir("a b"), "stable");
        assert_eq!(
            launch_directory(root, "a", Some("/opt/mcp/a")).expect("test: configured"),
            std::path::PathBuf::from("/opt/mcp/a")
        );
        assert!(launch_directory(root, "a", Some("relative/dir")).is_err());
    }

    /// D-088 C-5 (defence in depth for opted-in workspace servers).
    #[test]
    fn deno_bun_go_rust_git_docker_and_windows_variables_are_not_ordinary() {
        for (name, class) in [
            ("DENO_CERT", LaunchVariable::Interpreter),
            ("BUN_CONFIG_REGISTRY", LaunchVariable::Interpreter),
            ("GOENV", LaunchVariable::Interpreter),
            ("GOPATH", LaunchVariable::Interpreter),
            ("GOSUMDB", LaunchVariable::Interpreter),
            ("GOINSECURE", LaunchVariable::Interpreter),
            ("RUSTFLAGS", LaunchVariable::Interpreter),
            ("RUSTUP_TOOLCHAIN", LaunchVariable::Interpreter),
            ("GIT_SSH_COMMAND", LaunchVariable::Interpreter),
            ("git_config_global", LaunchVariable::Interpreter),
            ("SSH_ASKPASS", LaunchVariable::Interpreter),
            ("DOCKER_HOST", LaunchVariable::Interpreter),
            ("COMSPEC", LaunchVariable::SearchPath),
            ("PathExt", LaunchVariable::SearchPath),
            ("SYSTEMROOT", LaunchVariable::SearchPath),
            ("HOMEPATH", LaunchVariable::ConfigLookup),
            ("PROGRAMDATA", LaunchVariable::ConfigLookup),
        ] {
            assert_eq!(classify_variable(name), class, "{name}");
        }
    }

    #[test]
    fn the_child_inherits_only_the_allowlist_and_its_own_env() {
        let parent = [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/prx"),
            ("LANG", "C.UTF-8"),
            ("LC_ALL", "C.UTF-8"),
            ("TMPDIR", "/tmp"),
            ("OPENAI_API_KEY", "parent-sentinel"),
            ("LD_PRELOAD", "/tmp/evil.so"),
            ("USER", "prx"),
        ]
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        let own = HashMap::from([
            ("MCP_TOKEN".to_string(), "own-sentinel".to_string()),
            ("HOME".to_string(), "/srv/mcp".to_string()),
        ]);
        let environment = launch_environment(parent, &own);
        let names = environment
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for inherited in ["PATH", "LANG", "LC_ALL", "TMPDIR", "MCP_TOKEN", "HOME"] {
            assert!(names.iter().any(|name| name == inherited), "{inherited}: {names:?}");
        }
        for cleared in ["OPENAI_API_KEY", "LD_PRELOAD", "USER"] {
            assert!(!names.iter().any(|name| name == cleared), "{cleared}: {names:?}");
        }
        // The server's own value wins, and appears once.
        let homes = environment
            .iter()
            .filter(|(name, _)| name == "HOME")
            .map(|(_, value)| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(homes, ["/srv/mcp"]);
    }

    #[test]
    fn launch_fields_are_transport_command_and_each_argument() {
        let path = |elems: &[&str], index: Option<usize>| {
            let mut path = elems
                .iter()
                .map(|elem| PathElem::Key((*elem).to_string()))
                .collect::<Vec<_>>();
            path.extend(index.map(PathElem::Index));
            path
        };
        assert_eq!(
            launch_server(&path(&["mcp", "servers", "a", "transport"], None)),
            Some("a")
        );
        assert_eq!(
            launch_server(&path(&["mcp", "servers", "a", "command"], None)),
            Some("a")
        );
        assert_eq!(
            launch_server(&path(&["mcp", "servers", "a", "args"], Some(0))),
            Some("a")
        );
        assert_eq!(launch_server(&path(&["mcp", "servers", "a", "args"], None)), None);
        assert_eq!(launch_server(&path(&["mcp", "servers", "a", "env", "X"], None)), None);
        assert_eq!(launch_server(&path(&["mcp", "servers", "a", "url"], None)), None);
        assert_eq!(
            launch_server(&path(&["mcp", "servers", "a", "working_dir"], None)),
            Some("a")
        );
        assert_eq!(
            launch_server(&path(&["mcp", "servers", "a", "startup_timeout_ms"], None)),
            None
        );
    }
}

#[cfg(test)]
mod plugin_launch_tests {
    use super::is_workspace_launch_file;
    use std::path::Path;

    #[test]
    fn plugin_trust_state_is_high_risk_wherever_it_lives() {
        let workspace = Path::new("/nonexistent-prx-test/ws");
        for target in [
            "/nonexistent-prx-test/state/plugin-trust.json",
            "/nonexistent-prx-test/state/plugin-trust.json.lock",
            "/nonexistent-prx-test/state/.plugin-trust.json.1234.tmp",
            "/nonexistent-prx-test/state/Plugin-Trust.JSON",
            "/nonexistent-prx-test/state/plugin-cache/abc.cwasm",
            "/nonexistent-prx-test/ws/plugins/p/plugin.wasm",
        ] {
            assert!(is_workspace_launch_file(workspace, Path::new(target)), "{target}");
        }
        for target in [
            "/nonexistent-prx-test/ws/notes.txt",
            "/nonexistent-prx-test/ws/plugin-trust-notes.txt.bak/x",
            "/nonexistent-prx-test/ws/docs/plugins/readme.md",
        ] {
            // `plugin-trust-notes` does not start with the trust file name.
            assert!(!is_workspace_launch_file(workspace, Path::new(target)), "{target}");
        }
    }
}

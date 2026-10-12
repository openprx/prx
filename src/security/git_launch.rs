//! The one way PRX runs `git` (GIT-ENV, D-098, D-100, D-103).
//!
//! A repository's own configuration is a launch definition. `.git/config`
//! (and every file it includes) can name programs git runs as PRX's user:
//! `core.fsmonitor` runs on a plain `git status`, hook scripts run on commit
//! and checkout, filter drivers run on add and checkout, credential helpers
//! run on any authenticated transfer. The workspace is writable by the
//! agent, so what the repository configures must not run. The operator's own
//! git configuration (the system and global files) is trusted (D-100): its
//! credential helpers, signing, LFS filters, aliases and line-ending rules
//! keep working. Every production `git` process is therefore built by
//! [`git_command`]:
//!
//! - **a cleared environment** through the shared launch builder
//!   ([`crate::config::mcp_launch::launch_environment`]): the launch
//!   allowlist of PRX's own environment (`PATH`, `HOME`, `LANG`, `TMPDIR`,
//!   `LC_*`), plus the non-secret variables the operator's helpers and
//!   transports need ([`GIT_INHERITED_VARIABLES`]: the ssh agent socket,
//!   proxies, CA bundles, XDG/D-Bus locations, `GNUPGHOME`, where the git
//!   configuration files live) and `GIT_TERMINAL_PROMPT=0`. No provider key,
//!   token or other variable of PRX's environment reaches git or anything it
//!   starts. `GIT_SSH_COMMAND` is not passed: set `core.sshCommand` in the
//!   global file instead;
//! - **the repository's configuration read first**, per launch, with
//!   `git config --list --show-scope --show-origin` (without any `-c` for a
//!   key it inspects: a command-line value would be listed as the trusted,
//!   effective one). A setting is **repository-controlled** when it comes
//!   from the local or worktree scope (including every file they include),
//!   or from any file inside the working tree whatever included it. Then:
//!   - the fixed program settings ([`COMMAND_SETTINGS`]: fsmonitor, ssh,
//!     pager, editors, askpass, external diff, signing programs, the default
//!     transport policy) are set with `-c` on **every** launch, to the
//!     operator's last value or else to a value that runs nothing. `-c`
//!     reaches every git process the command starts (a submodule's
//!     included), and it also closes the time-of-check gap for these keys;
//!   - the program settings a repository names (`filter`/`diff`/`merge`
//!     drivers, `pager.<cmd>`, `protocol.<name>.allow`) and the transport
//!     settings that could hand the operator's credentials to someone else
//!     (`http[.<url>].proxy`, `sslVerify`, `cookieFile`, `saveCookies`,
//!     `remote.<name>.proxy`) go back to the operator's value or to a
//!     harmless one when their effective value is repository-controlled;
//!   - the credential helper list is rebuilt from the operator's entries
//!     when the repository adds to it; a repository `core.gitProxy` turns
//!     the proxy off; a repository `gc.recentObjectsHook` turns automatic gc
//!     off;
//!   - the launch is **refused** for what cannot be neutralised by value: a
//!     configured hook (`hook.*`), a CA or resolver override
//!     (`http.sslCAInfo`/`sslCAPath`/`curloptResolve`), and any
//!     program-like key (`path`, `command`, `cmd`, `program`, `clean`,
//!     `smudge`, `process`, `driver`, `textconv`, `helper`, `editor`,
//!     `pager`, `askpass`, `sshcommand`) in a section PRX does not handle,
//!     such as git-lfs's `lfs.customtransfer.<name>.path` and
//!     `lfs.extension.<name>.clean`, which the operator's trusted git-lfs
//!     would otherwise run;
//!   - the launch is refused for a repository `remote.<name>.uploadpack` or
//!     `receivepack` (first-wins, and a partial clone's lazy fetch runs it
//!     with no option PRX could pass) and a repository `url.*.insteadOf` or
//!     `pushInsteadOf` (multi-valued: a `-c` cannot remove it);
//!   - the launch is refused when the directory git runs in is not inside
//!     the working tree git reports, or when that working tree's `.git` is
//!     not the repository git uses (`core.worktree` pointing elsewhere,
//!     a parent included);
//! - **settings forced on every launch** ([`ALWAYS_OVERRIDES`]):
//!   - `core.hooksPath` points nowhere (`/dev/null` on Unix, an empty
//!     directory under PRX's configuration directory elsewhere), overriding
//!     the global file too, so no hook script runs: `.git/hooks` is
//!     repository content, and a global hooks path would still run hooks in
//!     whichever repository PRX works in. The operator's global `hook.*`
//!     (config-based hooks of newer git versions) stays trusted;
//!   - `ext::` transports are refused;
//!   - `safe.bareRepository=explicit`: a workspace laid out as a bare
//!     repository (plain `HEAD`, `config`, `objects/`, `refs/` files) is not
//!     used as one;
//!   - git never starts a process inside a submodule
//!     (`submodule.recurse=false`, `diff.ignoreSubmodules=dirty`, and the
//!     same per submodule named in the configuration or in `.gitmodules` of
//!     the working tree, the index or `HEAD`).
//!
//! Callers pass [`DIFF_SAFETY_ARGS`] to every command printing a patch,
//! [`PUSH_SAFETY_ARGS`] to `push` and [`FETCH_SAFETY_ARGS`] to `fetch`,
//! `pull` and `ls-remote`: `remote.<name>.receivepack`/`uploadpack` are read
//! first-wins, so a `-c` cannot replace the repository's value, while the
//! explicit option does. A trusted value of these two keys is not used for
//! PRX's git.
//!
//! **Residuals (time of check to time of use).** The configuration is read
//! just before the command runs. A repository-controlled driver, protocol,
//! transport or credential setting written between the read and the command
//! (a file tool's write of `.git/config` is a high-risk operation), or one
//! that only applies after the command has started (an `includeIf
//! "onbranch:"` that matches the branch a checkout moves to), is not
//! overridden. [`COMMAND_SETTINGS`] and [`ALWAYS_OVERRIDES`] are not
//! affected. A push to a local-path remote runs that repository's own
//! `receive-pack` hooks and configuration (with the cleared environment;
//! push is high risk).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Flags every `git diff` (and any command printing a patch) passes: no
/// external diff program and no textconv filter, so the output is the plain
/// patch PRX parses.
pub const DIFF_SAFETY_ARGS: [&str; 2] = ["--no-ext-diff", "--no-textconv"];

/// Flags every `git push` passes: the receiving program is git's own, not a
/// repository's first-wins `remote.<name>.receivepack`.
pub const PUSH_SAFETY_ARGS: [&str; 1] = ["--receive-pack=git-receive-pack"];

/// Flags every `git fetch`, `pull` and `ls-remote` passes: the serving
/// program is git's own, not a repository's first-wins
/// `remote.<name>.uploadpack`.
pub const FETCH_SAFETY_ARGS: [&str; 1] = ["--upload-pack=git-upload-pack"];

/// Where the git process works.
#[derive(Debug, Clone, Copy)]
pub enum GitScope<'a> {
    /// An existing repository (or a directory inside one): git starts in
    /// that directory, and its configuration is read first.
    Repository(&'a Path),
    /// A command that creates a repository (`git clone <url> <dest>`): no
    /// repository configuration exists yet; the working directory is left
    /// to the caller.
    NewRepository,
}

/// Variables of PRX's environment a git process inherits besides the
/// launch allowlist (which holds `HOME`, so the operator's global file is
/// read). None is a provider secret: the ssh agent socket (ssh remotes and
/// ssh signing), proxies and CA bundles (https through a corporate proxy),
/// the XDG and D-Bus locations (the global file under `XDG_CONFIG_HOME`,
/// `gh`, libsecret and Git Credential Manager), `GNUPGHOME` (gpg signing),
/// and where the operator's git configuration files are.
pub const GIT_INHERITED_VARIABLES: &[&str] = &[
    "SSH_AUTH_SOCK",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "GIT_SSL_CAINFO",
    "GIT_SSL_CAPATH",
    "XDG_CONFIG_HOME",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "GNUPGHOME",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
];

/// Environment variables every git process gets.
const FIXED_ENVIRONMENT: &[(&str, &str)] = &[
    // Never prompt: there is no terminal to answer, and a prompt would hang.
    ("GIT_TERMINAL_PROMPT", "0"),
];

/// Settings forced on every launch, overriding every configuration file
/// (the operator's included). `core.hooksPath` is added at launch.
pub const ALWAYS_OVERRIDES: &[(&str, &str)] = &[
    // `ext::<command>` runs an arbitrary command.
    ("protocol.ext.allow", "never"),
    // A directory that merely looks like a bare repository is not one.
    ("safe.bareRepository", "explicit"),
    // A submodule's configuration and hooks are repository content: git
    // must not start a process inside one.
    ("submodule.recurse", "false"),
    ("fetch.recurseSubmodules", "false"),
    ("push.recurseSubmodules", "no"),
    ("diff.ignoreSubmodules", "dirty"),
];

/// Settings that run a program, set on every launch to the operator's last
/// value or, when the operator's files set none, to git's own default
/// program or a value that runs nothing. Keys are compared in lower case,
/// as git lists them.
pub const COMMAND_SETTINGS: &[(&str, &str)] = &[
    // Runs on every index refresh (status, diff, add, commit, checkout).
    ("core.fsmonitor", "false"),
    // The ssh program of a transfer.
    ("core.sshcommand", "ssh"),
    // Pager and editors (PRX never has a terminal, `cat` and `:` are git's
    // own "no pager" and "no editor").
    ("core.pager", "cat"),
    ("core.editor", ":"),
    ("sequence.editor", ":"),
    // Credential prompts.
    ("core.askpass", ""),
    // A push or fetch with alternates.
    ("core.alternaterefscommand", ""),
    // An external diff program.
    ("diff.external", ""),
    // Signing and verification programs (git's defaults).
    ("gpg.program", "gpg"),
    ("gpg.openpgp.program", "gpg"),
    ("gpg.x509.program", "gpgsm"),
    ("gpg.ssh.program", "ssh-keygen"),
    ("gpg.ssh.defaultkeycommand", ""),
    // The default transport policy (git's default for a protocol without a
    // policy of its own is `user`, allowed outside a submodule).
    ("protocol.allow", "user"),
];

/// Multi-valued, program-naming settings handled as a list.
const CREDENTIAL_HELPER: &str = "helper";

/// Last components of a key that name a program (or a program's path).
const PROGRAM_LIKE_NAMES: &[&str] = &[
    "path",
    "command",
    "cmd",
    "program",
    "clean",
    "smudge",
    "process",
    "driver",
    "textconv",
    "helper",
    "editor",
    "pager",
    "askpass",
    "sshcommand",
];

/// Sections whose keys PRX handles (or knows not to name a program), so
/// the program-like rule does not apply to them: `include.path`,
/// `submodule.<name>.path` and `color.pager` are not programs, and the
/// program keys of the others are neutralised by name.
const HANDLED_SECTIONS: &[&str] = &[
    "core",
    "diff",
    "filter",
    "merge",
    "gpg",
    "credential",
    "include",
    "includeif",
    "submodule",
    "remote",
    "url",
    "http",
    "user",
    "pager",
    "sequence",
    "protocol",
    "gc",
    "color",
    "alias",
    "branch",
    "init",
    "safe",
];

/// `http` keys that decide where a request, and so the operator's
/// credential, goes, and that have no harmless value to fall back to: a
/// repository setting of one refuses the launch.
const REFUSED_HTTP_NAMES: &[&str] = &["sslcainfo", "sslcapath", "curloptresolve"];

/// One entry of `git config --list --show-scope --show-origin -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEntry {
    /// `system`, `global`, `local`, `worktree` or `command`.
    pub scope: String,
    /// Where it was read, as git prints it (`file:<path>`, `blob:<id>`,
    /// `command line:`).
    pub origin: String,
    /// `section.key` or `section.subsection.key` (section and key in lower
    /// case, as git prints them).
    pub key: String,
    /// `None` for a bare boolean key.
    pub value: Option<String>,
}

/// Parse `git config --list --show-scope --show-origin -z`:
/// `scope NUL origin NUL key LF value NUL` (`key NUL` without a value for a
/// bare boolean).
#[must_use]
pub fn parse_config_listing(raw: &[u8]) -> Vec<ConfigEntry> {
    let text = String::from_utf8_lossy(raw);
    let mut fields = text.split('\0');
    let mut entries = Vec::new();
    while let (Some(scope), Some(origin), Some(item)) = (fields.next(), fields.next(), fields.next()) {
        if scope.is_empty() && item.is_empty() {
            break;
        }
        let (key, value) = item
            .split_once('\n')
            .map_or((item, None), |(key, value)| (key, Some(value.to_string())));
        entries.push(ConfigEntry {
            scope: scope.to_string(),
            origin: origin.to_string(),
            key: key.to_string(),
            value,
        });
    }
    entries
}

/// `section`, `subsection` (possibly empty) and `key` of a configuration
/// name: the section is before the first dot, the key after the last.
fn split_key(key: &str) -> Option<(&str, &str, &str)> {
    let (section, rest) = key.split_once('.')?;
    Some(
        rest.rsplit_once('.')
            .map_or((section, "", rest), |(subsection, name)| (section, subsection, name)),
    )
}

/// The repository whose configuration is inspected: its working tree (for
/// origins inside it) and the directory git runs in (relative origins such
/// as `file:.git/config` are relative to it).
#[derive(Debug, Clone)]
pub struct RepositoryPaths {
    /// The directory git runs in.
    pub dir: PathBuf,
    /// `git rev-parse --show-toplevel`, when there is a working tree.
    pub toplevel: Option<PathBuf>,
}

impl RepositoryPaths {
    fn resolve(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.dir.join(path)
        };
        std::fs::canonicalize(&joined).unwrap_or(joined)
    }

    /// Whether `entry` is repository-controlled: the local or worktree scope
    /// (and every file they include), or any file inside the working tree.
    #[must_use]
    pub fn controls(&self, entry: &ConfigEntry) -> bool {
        if matches!(entry.scope.as_str(), "local" | "worktree") {
            return true;
        }
        let Some(file) = entry.origin.strip_prefix("file:") else {
            return false;
        };
        let file = self.resolve(file);
        let inside = |root: &Path| {
            let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            file.starts_with(root)
        };
        self.toplevel.as_deref().is_some_and(inside) || inside(&self.dir)
    }
}

/// How the launch must be adjusted for a repository's configuration.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RepositoryOverrides {
    /// `-c key=value` pairs, in order.
    pub overrides: Vec<(String, String)>,
    /// A repository-controlled `core.gitProxy` exists: git reads that list
    /// first-match-wins, so `-c` cannot replace it; the proxy is disabled
    /// (`GIT_PROXY_COMMAND` set and empty) instead.
    pub disable_git_proxy: bool,
}

/// The fallback for a repository-named program or transport setting (the
/// driver, pager, protocol and proxy families), or `None` when the setting
/// is none of those.
fn command_fallback(section: &str, subsection: &str, name: &str) -> Option<&'static str> {
    match (section, subsection.is_empty(), name) {
        ("filter", false, "clean" | "smudge" | "process")
        | ("diff", false, "command" | "textconv")
        | ("merge", false, "driver")
        | ("http" | "remote", _, "proxy")
        | ("http", _, "cookiefile") => Some(""),
        ("http", _, "sslverify") => Some("true"),
        ("http", _, "savecookies") => Some("false"),
        ("protocol", false, "allow") => Some(match subsection.to_ascii_lowercase().as_str() {
            "http" | "https" | "ssh" | "git" => "always",
            "ext" => "never",
            _ => "user",
        }),
        ("pager", true, _) => Some("false"),
        _ => None,
    }
}

/// The operator's last value of `key` (from a file the repository does not
/// control), a bare boolean read as `true`.
fn trusted_value(entries: &[ConfigEntry], paths: &RepositoryPaths, key: &str) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|entry| entry.key.eq_ignore_ascii_case(key) && !paths.controls(entry))
        .map(|entry| entry.value.clone().unwrap_or_else(|| "true".to_string()))
}

/// [`COMMAND_SETTINGS`] as set on every launch: the operator's last value,
/// else the fallback.
#[must_use]
pub fn fixed_overrides(entries: &[ConfigEntry], paths: &RepositoryPaths) -> Vec<(String, String)> {
    COMMAND_SETTINGS
        .iter()
        .map(|(key, fallback)| {
            (
                (*key).to_string(),
                trusted_value(entries, paths, key).unwrap_or_else(|| (*fallback).to_string()),
            )
        })
        .collect()
}

/// The overrides a repository's configuration needs beyond
/// [`fixed_overrides`] (see the module documentation): every
/// repository-named program or transport setting whose effective value is
/// repository-controlled goes back to the operator's value or to a harmless
/// one; the credential helper list is rebuilt from the operator's entries
/// alone when the repository adds to it; `submodule.<name>.ignore` is set
/// for every submodule `submodules` or the configuration names.
///
/// # Errors
///
/// A repository-controlled configured hook (`hook.*`), CA or resolver
/// override, program-like key in a section PRX does not handle, or a
/// program setting whose subsection cannot be named on a `-c` command line.
pub fn repository_overrides(
    entries: &[ConfigEntry],
    paths: &RepositoryPaths,
    submodules: &[String],
) -> anyhow::Result<RepositoryOverrides> {
    let mut result = RepositoryOverrides::default();
    let mut emitted: Vec<String> = Vec::new();
    let mut emit = |result: &mut RepositoryOverrides, key: String, value: String| {
        let normalized = key.to_ascii_lowercase();
        if !emitted.contains(&normalized) {
            emitted.push(normalized);
            result.overrides.push((key, value));
        }
    };
    let mut credential_touched = false;
    let mut gc_hook = false;
    for (index, entry) in entries.iter().enumerate() {
        let Some((section, subsection, name)) = split_key(&entry.key) else {
            continue;
        };
        let section = section.to_ascii_lowercase();
        let name = name.to_ascii_lowercase();
        if !paths.controls(entry) {
            continue;
        }
        if section == "hook" {
            anyhow::bail!(
                "refusing to run git: the repository configuration defines a configured hook ({}); PRX cannot \
                 disable it, and it would run a program",
                entry.key.escape_default()
            );
        }
        if section == "http" && REFUSED_HTTP_NAMES.contains(&name.as_str()) {
            anyhow::bail!(
                "refusing to run git: the repository configuration sets {}, which decides whom a transfer (and \
                 the operator's credential) trusts; set it in the global git configuration instead",
                entry.key.escape_default()
            );
        }
        if !HANDLED_SECTIONS.contains(&section.as_str()) && PROGRAM_LIKE_NAMES.contains(&name.as_str()) {
            anyhow::bail!(
                "refusing to run git: the repository configuration sets {}, which names a program PRX cannot \
                 neutralise",
                entry.key.escape_default()
            );
        }
        if section == "remote" && matches!(name.as_str(), "uploadpack" | "receivepack") {
            // First-wins, so no `-c` replaces it, and a partial clone's lazy
            // fetch (during a checkout) starts it without any command-line
            // option PRX could pass (R-GITENV-confirm C-1).
            anyhow::bail!(
                "refusing to run git: the repository configuration sets {}, a program git would run on a \
                 fetch or push (including a partial clone's lazy fetch)",
                entry.key.escape_default()
            );
        }
        if section == "url" && matches!(name.as_str(), "insteadof" | "pushinsteadof") {
            // Multi-valued with longest-match: a `-c` can add a rule but never
            // remove the repository's, which redirects where a transfer (and
            // the operator's credential) goes (D-106).
            anyhow::bail!(
                "refusing to run git: the repository configuration sets {}, which redirects where a transfer \
                 goes; set URL rewrites in the global git configuration instead",
                entry.key.escape_default()
            );
        }
        if section == "credential" && name == CREDENTIAL_HELPER {
            credential_touched = true;
            continue;
        }
        if section == "core" && name == "gitproxy" {
            result.disable_git_proxy = true;
            continue;
        }
        if section == "gc" && name == "recentobjectshook" {
            gc_hook = true;
            continue;
        }
        // The fixed program settings are set on every launch.
        let lower_key = entry.key.to_ascii_lowercase();
        if COMMAND_SETTINGS.iter().any(|(key, _)| *key == lower_key) {
            continue;
        }
        let Some(fallback) = command_fallback(&section, subsection, &name) else {
            continue;
        };
        if subsection.contains(['=', '\n', '\0']) {
            anyhow::bail!(
                "refusing to run git: the repository setting {} cannot be overridden on a command line",
                entry.key.escape_default()
            );
        }
        // Only the effective (last) value matters: a later trusted entry
        // already wins.
        let effective_is_controlled = entries
            .iter()
            .skip(index + 1)
            .filter(|later| later.key.eq_ignore_ascii_case(&entry.key))
            .all(|later| paths.controls(later));
        if !effective_is_controlled {
            continue;
        }
        // A URL-specific or per-remote transport setting falls back to the
        // operator's general `http.<name>` before the harmless value, so a
        // repository's URL-specific proxy does not blank the operator's
        // general proxy (D-106).
        let trusted = trusted_value(entries, paths, &entry.key).or_else(|| match (section.as_str(), subsection) {
            ("http", subsection) if !subsection.is_empty() => trusted_value(entries, paths, &format!("http.{name}")),
            ("remote", _) if name == "proxy" => trusted_value(entries, paths, "http.proxy"),
            _ => None,
        });
        if section == "filter" && trusted.is_none() {
            emit(
                &mut result,
                format!("filter.{subsection}.required"),
                "false".to_string(),
            );
        }
        emit(
            &mut result,
            entry.key.clone(),
            trusted.unwrap_or_else(|| fallback.to_string()),
        );
    }
    if credential_touched {
        // An empty `credential.helper` clears the helper list (URL-scoped
        // entries included); the operator's entries are then replayed in
        // their original order.
        result.overrides.push(("credential.helper".to_string(), String::new()));
        for entry in entries.iter().filter(|entry| !paths.controls(entry)) {
            let is_helper = split_key(&entry.key).is_some_and(|(section, subsection, name)| {
                section.eq_ignore_ascii_case("credential")
                    && name.eq_ignore_ascii_case(CREDENTIAL_HELPER)
                    && !subsection.contains(['=', '\n', '\0'])
            });
            if is_helper {
                result
                    .overrides
                    .push((entry.key.clone(), entry.value.clone().unwrap_or_default()));
            }
        }
    }
    if gc_hook {
        // `gc.recentObjectsHook` runs during gc: no automatic gc then.
        emit(&mut result, "gc.auto".to_string(), "0".to_string());
        emit(&mut result, "maintenance.auto".to_string(), "false".to_string());
    }
    // Every submodule the configuration or `.gitmodules` names: its own
    // `ignore` setting would otherwise decide whether git runs inside it.
    let mut names = entries
        .iter()
        .filter_map(|entry| split_key(&entry.key))
        .filter(|(section, subsection, _)| section.eq_ignore_ascii_case("submodule") && !subsection.is_empty())
        .map(|(_, subsection, _)| subsection.to_string())
        .chain(submodules.iter().cloned())
        .filter(|name| !name.contains(['=', '\n', '\0']))
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    for name in names {
        emit(&mut result, format!("submodule.{name}.ignore"), "dirty".to_string());
    }
    Ok(result)
}

/// The environment of a git process: the launch allowlist of `parent`
/// (through the shared builder, `HOME` included), the
/// [`GIT_INHERITED_VARIABLES`], then the fixed git variables.
#[must_use]
pub fn git_environment(parent: impl IntoIterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    let parent = parent.into_iter().collect::<Vec<_>>();
    let mut environment = crate::config::mcp_launch::launch_environment(
        parent.iter().cloned(),
        &std::collections::HashMap::<String, String>::new(),
    );
    environment.extend(
        parent
            .into_iter()
            .filter(|(name, _)| GIT_INHERITED_VARIABLES.iter().any(|inherited| name == inherited)),
    );
    environment.extend(
        FIXED_ENVIRONMENT
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value))),
    );
    environment
}

/// Where `core.hooksPath` points, so no hook script runs. On Unix it is
/// `/dev/null`: git looks a hook up as `/dev/null/<name>`, which cannot
/// exist.
///
/// # Errors
///
/// None on Unix.
#[cfg(unix)]
pub fn hooks_path() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from("/dev/null"))
}

/// Where `core.hooksPath` points, so no hook script runs: an empty
/// directory `git-no-hooks` under PRX's configuration directory, created
/// when missing and refused when it is a symlink, not a directory, or not
/// empty.
///
/// # Errors
///
/// No configuration directory is known, or the directory is unusable.
#[cfg(not(unix))]
pub fn hooks_path() -> anyhow::Result<PathBuf> {
    let root = crate::config::schema::process_config_dir()
        .ok_or_else(|| anyhow::anyhow!("refusing to run git: no configuration directory for the empty hooks path"))?;
    let dir = root.join("git-no-hooks");
    if !crate::config::mcp_launch::existing_real_directory(&dir, "git hooks directory")? {
        std::fs::create_dir_all(&dir)?;
    }
    if std::fs::read_dir(&dir)?.next().is_some() {
        anyhow::bail!(
            "refusing to run git: the empty hooks directory {} is not empty",
            dir.display()
        );
    }
    Ok(dir)
}

/// How a read-only git command of the inspection may be configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadKind {
    /// The configuration listing: no `-c` for a key the classification
    /// inspects, since a command-line value would be listed as the trusted,
    /// effective one and hide the repository's own.
    Listing,
    /// Any other read (`rev-parse`, a `.gitmodules` blob). A read that loads
    /// the index runs `core.fsmonitor`, so it is switched off.
    Other,
}

/// Run a git command that only reads (`config`, `rev-parse`): it runs no
/// hook, filter or helper, and no fsmonitor (see [`ReadKind`]).
/// `safe.bareRepository=explicit` is always set: the classification does
/// not inspect that key.
async fn read_git(
    dir: &Path,
    environment: &[(OsString, OsString)],
    kind: ReadKind,
    args: &[&str],
) -> anyhow::Result<Vec<u8>> {
    let mut command = tokio::process::Command::new("git");
    command
        .env_clear()
        .envs(environment.iter().cloned())
        .env("GIT_NO_LAZY_FETCH", "1")
        .current_dir(dir)
        .args(["-c", "safe.bareRepository=explicit"]);
    if kind == ReadKind::Other {
        command.args(["-c", "core.fsmonitor=false"]);
    }
    command.args(args);
    let output = crate::runtime::shell_process::run_managed_output(command).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git {} in {} failed: {}",
            args.first().copied().unwrap_or_default(),
            dir.display(),
            stderr.lines().next().unwrap_or("unknown error")
        );
    }
    Ok(output.stdout)
}

/// The submodule names of a `.gitmodules` listing.
fn submodule_names(raw: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(raw)
        .split('\0')
        .filter_map(|key| split_key(key).map(|(_, subsection, _)| subsection.to_string()))
        .filter(|name| !name.is_empty())
        .collect()
}

/// Settings an operator-configured shell job's git gets through the
/// environment (D-107), where PRX cannot inspect the repository the job
/// works in: the fixed program settings ([`COMMAND_SETTINGS`], the
/// operator's system/global value or the fallback), no hook script, no bare
/// repository discovery, no `ext::` transport.
#[must_use]
pub fn operator_git_config_pairs(entries: &[ConfigEntry]) -> Vec<(String, String)> {
    let mut pairs = COMMAND_SETTINGS
        .iter()
        .map(|(key, fallback)| {
            let trusted = entries
                .iter()
                .rev()
                .find(|entry| {
                    matches!(entry.scope.as_str(), "system" | "global") && entry.key.eq_ignore_ascii_case(key)
                })
                .map(|entry| entry.value.clone().unwrap_or_else(|| "true".to_string()));
            ((*key).to_string(), trusted.unwrap_or_else(|| (*fallback).to_string()))
        })
        .collect::<Vec<_>>();
    pairs.extend(
        [("safe.bareRepository", "explicit"), ("protocol.ext.allow", "never")]
            .map(|(key, value)| (key.to_string(), value.to_string())),
    );
    pairs
}

/// `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>` for
/// `pairs` (command-line scope: they win over every configuration file and
/// reach every git process started below), plus `GIT_NO_LAZY_FETCH=1` (a
/// partial clone's lazy fetch would run the repository's first-wins
/// `remote.<name>.uploadpack`).
#[must_use]
pub fn git_config_environment(pairs: &[(String, String)]) -> Vec<(OsString, OsString)> {
    let mut environment = vec![
        (
            OsString::from("GIT_CONFIG_COUNT"),
            OsString::from(pairs.len().to_string()),
        ),
        (OsString::from("GIT_NO_LAZY_FETCH"), OsString::from("1")),
    ];
    for (index, (key, value)) in pairs.iter().enumerate() {
        environment.push((OsString::from(format!("GIT_CONFIG_KEY_{index}")), OsString::from(key)));
        environment.push((
            OsString::from(format!("GIT_CONFIG_VALUE_{index}")),
            OsString::from(value),
        ));
    }
    environment
}

/// The environment additions that harden the git of an operator-configured
/// shell job (a node allowlist entry, a system Xin task): the operator's
/// system and global configuration is read (from `/`, so no repository's
/// configuration is), and [`operator_git_config_pairs`] plus the hooks path
/// are passed through [`git_config_environment`]. A failed read falls back
/// to the fallbacks alone.
pub async fn operator_git_environment() -> anyhow::Result<Vec<(OsString, OsString)>> {
    let parent = std::env::vars_os().collect::<Vec<_>>();
    let environment = git_environment(parent);
    let neutral = if cfg!(unix) {
        PathBuf::from("/")
    } else {
        std::env::temp_dir()
    };
    let entries = match read_git(
        &neutral,
        &environment,
        ReadKind::Listing,
        &["config", "--list", "--show-scope", "--show-origin", "-z"],
    )
    .await
    {
        Ok(raw) => parse_config_listing(&raw),
        Err(error) => {
            tracing::warn!(error = %error, "could not read the operator's git configuration; using the fallbacks");
            Vec::new()
        }
    };
    let mut pairs = operator_git_config_pairs(&entries);
    let hooks = hooks_path()?;
    pairs.push(("core.hooksPath".to_string(), hooks.to_string_lossy().into_owned()));
    Ok(git_config_environment(&pairs))
}

/// Warn once per operator-configured job (a `surface` and a job key) that
/// its shell command runs git with only the environment hardening (D-107):
/// the repository's own drivers, credential helpers and transport settings
/// are not inspected there.
pub fn warn_operator_git_once(surface: &str, job: &str) {
    static WARNED: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashSet<String>>> =
        std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashSet::new()));
    let key = format!("{surface}\u{1f}{job}");
    if WARNED.lock().insert(key) {
        tracing::warn!(
            surface,
            job,
            "an operator-configured shell job runs git: fsmonitor, hook scripts, bare discovery, ext:: and lazy \
             fetch are disabled for it, but the repository's own drivers, credential helpers and transport \
             settings are not inspected; prefer the git_operations tool"
        );
    }
}

/// Whether `<toplevel>/.git` exists and is the repository git uses: the
/// directory itself, or a `gitdir:` link file (a linked worktree, a
/// submodule) pointing at it. A `core.worktree` that makes a parent (or any
/// other directory) the working tree fails this, since that directory's
/// `.git`, if any, is a different repository (R-GITENV-confirm C-2).
fn working_tree_owns_repository(toplevel: &Path, git_dir: &Path) -> bool {
    let dot_git = toplevel.join(".git");
    let Ok(metadata) = std::fs::metadata(&dot_git) else {
        return false;
    };
    let linked = if metadata.is_dir() {
        dot_git
    } else {
        let Ok(text) = std::fs::read_to_string(&dot_git) else {
            return false;
        };
        let Some(target) = text.lines().next().and_then(|line| line.strip_prefix("gitdir:")) else {
            return false;
        };
        toplevel.join(target.trim())
    };
    match (std::fs::canonicalize(linked), std::fs::canonicalize(git_dir)) {
        (Ok(linked), Ok(git_dir)) => linked == git_dir,
        _ => false,
    }
}

/// Whether `program` (a path or a name) is git or one of its helper
/// programs (`git-upload-pack`, ...), in any letter case, with or without
/// `.exe`.
#[must_use]
pub fn program_is_git(program: &str) -> bool {
    let name = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    name == "git" || name.starts_with("git-")
}

/// Words that precede the program of a simple shell command without being
/// it: wrappers, keywords and `!`.
const SHELL_PREFIX_WORDS: &[&str] = &[
    "sudo", "doas", "env", "command", "builtin", "exec", "nohup", "time", "nice", "ionice", "stdbuf", "timeout",
    "xargs", "eval", "then", "do", "else", "elif", "if", "while", "until", "!", "{", "(",
];

/// Wrapper options whose next word is data, not the command to execute.
const SHELL_PREFIX_VALUE_OPTIONS: &[&str] = &[
    "-u",
    "--user",
    "-g",
    "--group",
    "-h",
    "--host",
    "-p",
    "--prompt",
    "-C",
    "--close-from",
    "-T",
    "--command-timeout",
    "--unset",
    "--chdir",
    "--chroot",
];

/// Shells whose `-c` argument is itself a command string.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];

/// Whether the shell command string `command` runs git (or a git helper)
/// as the program of one of its simple commands: after `;`, `&&`, `||`,
/// `|`, `&`, a newline, `$(`, a backtick or a subshell, past environment
/// assignments and wrappers (`env`, `sudo`, `xargs`, `eval`, ...), and
/// inside `sh -c`/`bash -c` strings. Quotes are removed while quoted path
/// spaces stay in their program word, so `g'i't` is `git`. This is a
/// best-effort guard: a script file or name built at run time (`$(printf git)`)
/// is not recognised.
#[must_use]
pub fn shell_command_runs_git(command: &str) -> bool {
    // A Windows drive path may contain spaces before its final `git.exe`;
    // splitting it into shell words would otherwise hide the program name.
    let start = command.trim_start_matches(|character: char| character.is_whitespace() || character == '"');
    if start.as_bytes().get(1) == Some(&b':')
        && matches!(start.as_bytes().get(2), Some(b'\\' | b'/'))
        && start
            .rsplit(['\\', '/'])
            .next()
            .and_then(|tail| tail.split_whitespace().next())
            .is_some_and(|program| program_is_git(program.trim_matches('"')))
    {
        return true;
    }
    // Keep spaces inside literal quoted program paths in one word. A sentinel
    // avoids adding a shell parser to this deliberately bounded detector.
    const QUOTED_SPACE: char = '\u{1f}';
    let mut quote = None;
    let mut flattened = String::with_capacity(command.len());
    for character in command.chars() {
        if matches!(character, ';' | '&' | '|' | '\n' | '`' | '(' | ')' | '{' | '}') {
            quote = None;
            flattened.push('\n');
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            continue;
        }
        flattened.push(match character {
            ' ' | '\t' if quote.is_some() => QUOTED_SPACE,
            '\\' if quote.is_some() => '/',
            '\\' => continue,
            other => other,
        });
    }
    flattened.lines().any(|segment| {
        let words = segment.split_whitespace().collect::<Vec<_>>();
        let mut index = 0;
        while let Some(word) = words.get(index) {
            if SHELL_PREFIX_VALUE_OPTIONS.contains(word) {
                index += 2;
                continue;
            }
            let is_assignment = word.split_once('=').is_some_and(|(name, _)| {
                !name.is_empty()
                    && name
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '_')
            });
            let is_option_or_number = word.starts_with('-') || word.chars().all(|character| character.is_ascii_digit());
            if is_assignment || is_option_or_number || word.starts_with('$') || SHELL_PREFIX_WORDS.contains(word) {
                index += 1;
                continue;
            }
            break;
        }
        let Some(program) = words.get(index) else {
            return false;
        };
        if program_is_git(&program.replace(QUOTED_SPACE, " ")) {
            return true;
        }
        let base = program.rsplit('/').next().unwrap_or(program);
        SHELLS.contains(&base)
            && words
                .iter()
                .position(|word| word.starts_with('-') && !word.starts_with("--") && word.contains('c'))
                .is_some_and(|position| {
                    shell_command_runs_git(
                        &words
                            .get(position + 1..)
                            .unwrap_or_default()
                            .join(" ")
                            .replace(QUOTED_SPACE, " "),
                    )
                })
    })
}

/// Read the repository at `dir`: its effective configuration with scopes
/// and origins, its working tree (the launch is refused when `dir` is not
/// inside it), and the submodule names of its `.gitmodules` in the working
/// tree, the index and `HEAD`.
async fn inspect_repository(
    dir: &Path,
    environment: &[(OsString, OsString)],
) -> anyhow::Result<(Vec<ConfigEntry>, RepositoryPaths, Vec<String>)> {
    let listing = read_git(
        dir,
        environment,
        ReadKind::Listing,
        &["config", "--list", "--show-scope", "--show-origin", "-z"],
    )
    .await?;
    let entries = parse_config_listing(&listing);
    let raw = read_git(
        dir,
        environment,
        ReadKind::Other,
        &["rev-parse", "--show-toplevel", "--absolute-git-dir"],
    )
    .await?;
    let text = String::from_utf8_lossy(&raw);
    let mut lines = text.lines();
    let (Some(toplevel), Some(git_dir)) = (lines.next(), lines.next()) else {
        anyhow::bail!(
            "refusing to run git: no working tree and repository for {}",
            dir.display()
        );
    };
    let toplevel = PathBuf::from(toplevel);
    let canonical_dir = std::fs::canonicalize(dir)?;
    let canonical_top = std::fs::canonicalize(&toplevel)?;
    if !canonical_dir.starts_with(&canonical_top) {
        anyhow::bail!(
            "refusing to run git: the working tree of the repository at {} is {}, outside it (core.worktree)",
            dir.display(),
            toplevel.display()
        );
    }
    if !working_tree_owns_repository(&canonical_top, Path::new(git_dir)) {
        anyhow::bail!(
            "refusing to run git: the working tree {} does not hold the repository {} it is used with (a \
             core.worktree pointing at another directory)",
            toplevel.display(),
            git_dir
        );
    }
    let mut submodules = Vec::new();
    let gitmodules = canonical_top.join(".gitmodules");
    if gitmodules.is_file() {
        let gitmodules = gitmodules.to_string_lossy().into_owned();
        let names = read_git(
            dir,
            environment,
            ReadKind::Other,
            &["config", "--file", &gitmodules, "--name-only", "--list", "-z"],
        )
        .await?;
        submodules.extend(submodule_names(&names));
    }
    // git falls back to the index copy, then to HEAD's, when the working
    // tree has none; a missing blob is not an error here.
    for blob in [":.gitmodules", "HEAD:.gitmodules"] {
        let blob_arg = format!("--blob={blob}");
        if let Ok(names) = read_git(
            dir,
            environment,
            ReadKind::Other,
            &["config", &blob_arg, "--name-only", "--list", "-z"],
        )
        .await
        {
            submodules.extend(submodule_names(&names));
        }
    }
    let paths = RepositoryPaths {
        dir: canonical_dir,
        toplevel: Some(canonical_top),
    };
    Ok((entries, paths, submodules))
}

/// A `git` command for `scope` with the hardened launch: cleared
/// environment, every repository-controlled program-naming setting
/// neutralised, no hook script, `ext::` refused, no bare-repository
/// discovery, no git process inside a submodule, and stdin closed. The
/// caller adds the subcommand and its arguments (with
/// [`DIFF_SAFETY_ARGS`], [`PUSH_SAFETY_ARGS`] or [`FETCH_SAFETY_ARGS`] where
/// they apply).
///
/// # Errors
///
/// The repository configuration is unreadable, its working tree does not
/// hold the directory, or it holds a setting that cannot be neutralised.
pub async fn git_command(scope: GitScope<'_>) -> anyhow::Result<tokio::process::Command> {
    // Collected before any await: `VarsOs` is not `Send`.
    let parent = std::env::vars_os().collect::<Vec<_>>();
    git_command_with_parent(scope, parent).await
}

/// [`git_command`] with the parent environment given (so a test can run
/// git under a temporary `HOME`).
async fn git_command_with_parent(
    scope: GitScope<'_>,
    parent: Vec<(OsString, OsString)>,
) -> anyhow::Result<tokio::process::Command> {
    let hooks = hooks_path()?;
    let mut environment = git_environment(parent);
    let mut overrides = if let GitScope::Repository(dir) = scope {
        let (entries, paths, submodules) = inspect_repository(dir, &environment).await?;
        let repository = repository_overrides(&entries, &paths, &submodules)?;
        if repository.disable_git_proxy {
            environment.push((OsString::from("GIT_PROXY_COMMAND"), OsString::new()));
        }
        let mut overrides = fixed_overrides(&entries, &paths);
        overrides.extend(repository.overrides);
        overrides
    } else {
        // `clone`: no repository configuration exists yet.
        Vec::new()
    };
    overrides.extend(
        ALWAYS_OVERRIDES
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string())),
    );
    overrides.push(("core.hooksPath".to_string(), hooks.to_string_lossy().into_owned()));

    let mut command = tokio::process::Command::new("git");
    command.env_clear().envs(environment).stdin(Stdio::null());
    if let GitScope::Repository(dir) = scope {
        command.current_dir(dir);
    }
    command.arg("--no-pager");
    for (key, value) in overrides {
        command.arg("-c").arg(format!("{key}={value}"));
    }
    Ok(command)
}

/// Names inside a `.git` directory that decide which programs git runs, or
/// which repository (and so which configuration) it uses: the
/// configuration files, hooks, `info/` (attributes, excludes, sparse
/// checkout), the per-worktree and submodule repositories, the
/// `commondir`/`gitdir` links, and the rebase and sequencer state (whose
/// todo lists can hold `exec` lines).
const GIT_CONTROL_NAMES: &[&str] = &[
    "config",
    "config.worktree",
    "hooks",
    "info",
    "modules",
    "worktrees",
    "commondir",
    "gitdir",
    "rebase-merge",
    "rebase-apply",
    "sequencer",
];

/// Code points HFS+ ignores in a file name (git's `is_hfs_dotgit`), so
/// `.g\u{200c}it` names `.git` there.
const fn is_hfs_ignorable(character: char) -> bool {
    matches!(
        character,
        '\u{200c}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}'
    )
}

/// A path component as a case-insensitive, platform-folding filesystem
/// sees it: HFS+ ignorable code points dropped, an NTFS stream suffix
/// (`name::$INDEX_ALLOCATION`) cut, trailing dots and spaces trimmed (NTFS),
/// lower case.
fn folded_component(name: &str) -> String {
    let without_ignorables = name
        .chars()
        .filter(|character| !is_hfs_ignorable(*character))
        .collect::<String>();
    let without_stream = without_ignorables
        .split_once(':')
        .map_or(without_ignorables.as_str(), |(name, _)| name);
    without_stream.trim_end_matches(['.', ' ']).to_ascii_lowercase()
}

/// Whether a folded component names a `.git` directory (including the
/// NTFS short name `git~1`).
fn is_dot_git(folded: &str) -> bool {
    folded == ".git" || folded == "git~1"
}

/// Whether `target` is a git control file: a `.git` entry itself (a
/// directory or a `gitdir:` link file), or a path inside a `.git`
/// directory through one of the control names (`config`, `hooks`, `info`,
/// the rebase state, ...). Names are folded as a case-insensitive NTFS or
/// HFS+ filesystem folds them, so no spelling hides one. An existing file
/// with more than one hard link also counts: it may be a link to one (a
/// file tool's write would go through it). A file tool's write there is
/// high risk (GIT-ENV): it changes which programs git runs as PRX's user.
#[must_use]
pub fn is_git_control_file(target: &Path) -> bool {
    let components = target
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => name.to_str().map(folded_component),
            _ => None,
        })
        .collect::<Vec<_>>();
    let named = components.iter().enumerate().any(|(index, name)| {
        is_dot_git(name)
            && components
                .get(index + 1)
                .is_none_or(|next| GIT_CONTROL_NAMES.contains(&next.as_str()))
    });
    named || has_other_hard_links(target)
}

/// Whether `target` is an existing regular file with more than one link.
#[cfg(unix)]
fn has_other_hard_links(target: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(target).is_ok_and(|metadata| metadata.is_file() && metadata.nlink() > 1)
}

/// Hard links are not inspected on this platform.
#[cfg(not(unix))]
const fn has_other_hard_links(_target: &Path) -> bool {
    false
}

/// The workspace launch definitions (`hooks.json`, `mcp.json` at the
/// workspace root) as git pathspecs relative to the workspace, matched in
/// any letter case.
pub const LAUNCH_DEFINITION_PATHSPECS: [&str; 2] = [":(icase)hooks.json", ":(icase)mcp.json"];

/// Test support: every git process of the lib tests reads a scratch global
/// file and no system file, never the host's configuration (D-103). The
/// production launch passes `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_NOSYSTEM`
/// through, so setting them once in this process covers every launch.
#[cfg(test)]
#[allow(unsafe_code)]
pub(crate) fn hermetic_git_for_tests() {
    static SET: std::sync::Once = std::sync::Once::new();
    SET.call_once(|| {
        let scratch = std::env::temp_dir().join(format!("prx-git-test-global-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&scratch);
        let global = scratch.join("gitconfig");
        let _ = std::fs::write(&global, "");
        // SAFETY: test-only; set once, before the git processes of the
        // tests that call this, to names only git reads.
        unsafe {
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
            std::env::set_var("GIT_CONFIG_GLOBAL", &global);
        }
    });
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn entry(scope: &str, origin: &str, key: &str, value: &str) -> ConfigEntry {
        ConfigEntry {
            scope: scope.to_string(),
            origin: format!("file:{origin}"),
            key: key.to_string(),
            value: Some(value.to_string()),
        }
    }

    fn global(key: &str, value: &str) -> ConfigEntry {
        entry("global", "/home/op/.gitconfig", key, value)
    }

    fn local(key: &str, value: &str) -> ConfigEntry {
        entry("local", ".git/config", key, value)
    }

    fn paths() -> RepositoryPaths {
        RepositoryPaths {
            dir: PathBuf::from("/nonexistent-prx-test/ws"),
            toplevel: Some(PathBuf::from("/nonexistent-prx-test")),
        }
    }

    fn overrides(entries: &[ConfigEntry]) -> Vec<(String, String)> {
        repository_overrides(entries, &paths(), &[])
            .expect("test: overridable")
            .overrides
    }

    fn pair(key: &str, value: &str) -> (String, String) {
        (key.to_string(), value.to_string())
    }

    #[test]
    fn the_listing_parser_reads_scopes_origins_values_and_bare_booleans() {
        let raw = b"global\0file:/h/.gitconfig\0user.name\nA B\0local\0file:.git/config\0filter.lfs.clean\n\
                    git-lfs clean -- %f\0local\0file:.git/config\0core.bare\0";
        let entries = parse_config_listing(raw);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].scope, "global");
        assert_eq!(entries[0].origin, "file:/h/.gitconfig");
        assert_eq!(entries[0].value.as_deref(), Some("A B"));
        assert_eq!(entries[1].key, "filter.lfs.clean");
        assert_eq!(entries[2].value, None);
    }

    #[test]
    fn the_operators_settings_are_left_alone() {
        let entries = [
            global("core.fsmonitor", "/usr/local/bin/watchman-hook"),
            global("credential.helper", "osxkeychain"),
            global("filter.lfs.clean", "git-lfs clean -- %f"),
            global("gpg.program", "/opt/gpg"),
            global("hook.lint.command", "lint"),
            global("core.sshcommand", "ssh -i key"),
            global("lfs.customtransfer.x.path", "/opt/transfer"),
            global("http.proxy", "http://proxy.example:3128"),
        ];
        assert!(overrides(&entries).is_empty(), "{:?}", overrides(&entries));
        // The fixed program settings carry the operator's values.
        let fixed = fixed_overrides(&entries, &paths());
        for expected in [
            pair("core.fsmonitor", "/usr/local/bin/watchman-hook"),
            pair("gpg.program", "/opt/gpg"),
            pair("core.sshcommand", "ssh -i key"),
            pair("core.pager", "cat"),
        ] {
            assert!(fixed.contains(&expected), "{expected:?}: {fixed:?}");
        }
    }

    /// R-GITENV F-2: the fixed program settings are set on every launch
    /// (they then reach a submodule's git too), from the operator's value or
    /// the fallback, never from the repository's.
    #[test]
    fn the_fixed_program_settings_are_always_set_and_never_from_the_repository() {
        let fixed = fixed_overrides(
            &[
                local("core.fsmonitor", "./evil"),
                global("core.sshcommand", "ssh -i key"),
                local("core.sshcommand", "./evil"),
                local("gpg.program", "./evil"),
            ],
            &paths(),
        );
        assert_eq!(fixed.len(), COMMAND_SETTINGS.len());
        for expected in [
            pair("core.fsmonitor", "false"),
            pair("core.sshcommand", "ssh -i key"),
            pair("gpg.program", "gpg"),
            pair("protocol.allow", "user"),
        ] {
            assert!(fixed.contains(&expected), "{expected:?}: {fixed:?}");
        }
        assert!(!fixed.iter().any(|(_, value)| value == "./evil"), "{fixed:?}");
        assert_eq!(fixed_overrides(&[], &paths()).len(), COMMAND_SETTINGS.len());
    }

    #[test]
    fn repository_named_settings_fall_back_to_the_operators_value_or_to_nothing() {
        let entries = [
            global("filter.lfs.smudge", "git-lfs smudge -- %f"),
            local("filter.lfs.smudge", "./evil"),
            local("filter.ev.clean", "./evil"),
            local("merge.ev.driver", "./evil"),
            local("diff.ev.textconv", "./evil"),
            local("protocol.ext.allow", "always"),
            local("protocol.evil.allow", "always"),
            local("pager.log", "./evil"),
            // R-GITENV F-5: where a transfer (and the operator's
            // credential) goes.
            local("http.proxy", "http://attacker.invalid"),
            local("http.https://example.com.sslverify", "false"),
            local("http.cookiefile", "/home/op/.ssh/id_ed25519"),
            local("http.savecookies", "true"),
            local("remote.origin.proxy", "http://attacker.invalid"),
        ];
        let result = overrides(&entries);
        for expected in [
            pair("filter.lfs.smudge", "git-lfs smudge -- %f"),
            pair("filter.ev.clean", ""),
            pair("filter.ev.required", "false"),
            pair("merge.ev.driver", ""),
            pair("diff.ev.textconv", ""),
            pair("protocol.ext.allow", "never"),
            pair("protocol.evil.allow", "user"),
            pair("pager.log", "false"),
            pair("http.proxy", ""),
            pair("http.https://example.com.sslverify", "true"),
            pair("http.cookiefile", ""),
            pair("http.savecookies", "false"),
            pair("remote.origin.proxy", ""),
        ] {
            assert!(result.contains(&expected), "{expected:?}: {result:?}");
        }
        assert!(!result.iter().any(|(key, _)| key == "filter.lfs.required"));
    }

    /// R-GITENV-confirm C-1 and D-106: what `-c` cannot replace or remove
    /// refuses the launch when the repository sets it; the operator's own
    /// values do not.
    #[test]
    fn first_wins_pack_programs_and_url_rewrites_of_the_repository_refuse_the_launch() {
        for key in [
            "remote.origin.uploadpack",
            "remote.origin.receivepack",
            "url.https://attacker.invalid/.insteadof",
            "url.https://attacker.invalid/.pushinsteadof",
        ] {
            assert!(
                repository_overrides(&[local(key, "x")], &paths(), &[]).is_err(),
                "{key} was not refused"
            );
            assert!(
                repository_overrides(&[global(key, "x")], &paths(), &[]).is_ok(),
                "the operator's {key} was refused"
            );
        }
    }

    /// D-106: a repository's URL-specific or per-remote proxy falls back to
    /// the operator's general proxy, not to "no proxy".
    #[test]
    fn a_repository_url_specific_proxy_keeps_the_operators_general_proxy() {
        let entries = [
            global("http.proxy", "http://corp-proxy:3128"),
            global("http.sslverify", "true"),
            local("http.https://example.com.proxy", "http://attacker.invalid"),
            local("http.https://example.com.sslverify", "false"),
            local("remote.origin.proxy", "http://attacker.invalid"),
        ];
        let result = overrides(&entries);
        for expected in [
            pair("http.https://example.com.proxy", "http://corp-proxy:3128"),
            pair("http.https://example.com.sslverify", "true"),
            pair("remote.origin.proxy", "http://corp-proxy:3128"),
        ] {
            assert!(result.contains(&expected), "{expected:?}: {result:?}");
        }
    }

    #[test]
    fn shell_command_strings_that_run_git_are_recognised() {
        for command in [
            "git status",
            "  git push origin main",
            "cd repo && git status",
            "ls; git log",
            "echo x | xargs git add",
            "GIT_DIR=x git log",
            "env -i PATH=/usr/bin git status",
            "sudo -u prx git pull",
            "sudo --user prx git pull",
            "doas -u prx git status",
            "env -u GIT_CONFIG_PARAMETERS git status",
            "/usr/bin/git status",
            "C:\\Program Files\\Git\\bin\\git.exe status",
            "echo ready; \"C:\\Program Files\\Git\\bin\\git.exe\" status",
            "sudo -u prx \"C:\\Program Files\\Git\\bin\\git.exe\" status",
            "echo ready; \"/tmp/Program Files/git\" status",
            "GIT status",
            "g'i't status",
            "\"git\" status",
            "sh -c 'git status'",
            "bash -lc \"cd x && git fetch\"",
            "echo $(git rev-parse HEAD)",
            "echo `git rev-parse HEAD`",
            "(git status)",
            "if true; then git status; fi",
            "git-upload-pack /repo",
            "eval git status",
            "nice -n 5 git gc",
        ] {
            assert!(shell_command_runs_git(command), "{command}");
        }
        for command in [
            "echo git",
            "ls -la",
            "gitleaks detect",
            "cat .gitignore",
            "grep -r git src",
            "npm run build",
            "gh pr list",
            "echo 'use git later'",
            "sh -c 'ls'",
            "sudo -u git echo ready",
            "echo C:\\Program Files\\Git\\bin\\git.exe",
            "echo ready; echo \"/tmp/Program Files/git\"",
        ] {
            assert!(!shell_command_runs_git(command), "{command}");
        }
        assert!(program_is_git("/usr/local/bin/git"));
        assert!(program_is_git("GIT.EXE"));
        assert!(!program_is_git("gitk"));
    }

    #[test]
    fn a_later_operator_value_wins_without_an_override() {
        let later = ConfigEntry {
            scope: "command".to_string(),
            origin: "command line:".to_string(),
            key: "filter.ev.clean".to_string(),
            value: Some("cat".to_string()),
        };
        let entries = [local("filter.ev.clean", "./evil"), later];
        // A `command` scope entry with no file origin is not repository
        // content, and it is the effective value.
        assert!(overrides(&entries).is_empty(), "expected an empty value");
    }

    #[test]
    fn included_files_and_files_inside_the_working_tree_are_repository_controlled() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let top = std::fs::canonicalize(temp.path()).expect("test: canonical");
        let paths = RepositoryPaths {
            dir: top.clone(),
            toplevel: Some(top.clone()),
        };
        let inside = top.join("evil.cfg");
        let included_from_local = ConfigEntry {
            scope: "local".to_string(),
            origin: "file:/elsewhere/inc.cfg".to_string(),
            key: "core.fsmonitor".to_string(),
            value: Some("./evil".to_string()),
        };
        let included_from_global_into_the_tree = ConfigEntry {
            scope: "global".to_string(),
            origin: format!("file:{}", inside.display()),
            key: "core.pager".to_string(),
            value: Some("./evil".to_string()),
        };
        assert!(paths.controls(&included_from_local));
        assert!(paths.controls(&included_from_global_into_the_tree));
        assert!(!paths.controls(&global("core.pager", "less")));
        let fixed = fixed_overrides(&[included_from_local, included_from_global_into_the_tree], &paths);
        assert!(fixed.contains(&pair("core.fsmonitor", "false")), "{fixed:?}");
        assert!(fixed.contains(&pair("core.pager", "cat")), "{fixed:?}");
    }

    #[test]
    fn the_credential_helper_list_is_rebuilt_from_the_operators_entries() {
        let entries = [
            entry("system", "/etc/gitconfig", "credential.helper", "osxkeychain"),
            global("credential.https://example.com.helper", "store"),
            local("credential.helper", "./evil"),
            local("credential.https://example.com.helper", "./evil"),
        ];
        assert_eq!(
            overrides(&entries),
            vec![
                pair("credential.helper", ""),
                pair("credential.helper", "osxkeychain"),
                pair("credential.https://example.com.helper", "store"),
            ]
        );
    }

    #[test]
    fn configured_hooks_proxies_and_gc_hooks_of_the_repository() {
        assert!(repository_overrides(&[local("hook.x.command", "evil")], &paths(), &[]).is_err());
        assert!(repository_overrides(&[local("filter.a=b.clean", "evil")], &paths(), &[]).is_err());
        // A URL rewrite is refused even when its subsection contains `=`.
        assert!(repository_overrides(&[local("url.https://x/?a=b.insteadof", "y")], &paths(), &[]).is_err());
        // An unrelated, harmless subsection containing `=` remains allowed.
        assert!(repository_overrides(&[local("branch.feature=a.description", "note")], &paths(), &[]).is_ok());
        let proxy = repository_overrides(&[local("core.gitproxy", "evil")], &paths(), &[]).expect("test: proxy");
        assert!(proxy.disable_git_proxy);
        let gc = overrides(&[local("gc.recentobjectshook", "evil")]);
        assert!(gc.contains(&pair("gc.auto", "0")), "{gc:?}");
    }

    /// R-GITENV F-4 and F-5: what cannot be neutralised by value refuses the
    /// launch; ordinary keys of the same names in handled sections do not.
    #[test]
    fn program_like_keys_of_unknown_sections_and_trust_overrides_refuse_the_launch() {
        for key in [
            "lfs.customtransfer.x.path",
            "lfs.extension.x.clean",
            "lfs.extension.x.smudge",
            "mergetool.x.cmd",
            "trailer.sign.command",
            "http.sslcainfo",
            "http.https://example.com.sslcapath",
            "http.curloptresolve",
        ] {
            assert!(
                repository_overrides(&[local(key, "./evil")], &paths(), &[]).is_err(),
                "{key} was not refused"
            );
            assert!(
                repository_overrides(&[global(key, "/opt/x")], &paths(), &[]).is_ok(),
                "the operator's {key} was refused"
            );
        }
        for key in [
            "include.path",
            "submodule.lib.path",
            "color.pager",
            "lfs.url",
            "user.name",
        ] {
            assert!(
                repository_overrides(&[local(key, "x")], &paths(), &[]).is_ok(),
                "{key} was refused"
            );
        }
    }

    #[test]
    fn every_named_submodule_is_kept_from_starting_git() {
        let result = repository_overrides(
            &[local("submodule.lib.url", "https://example.invalid/lib")],
            &paths(),
            &["vendor".to_string()],
        )
        .expect("test: overridable")
        .overrides;
        assert!(result.contains(&pair("submodule.lib.ignore", "dirty")), "{result:?}");
        assert!(result.contains(&pair("submodule.vendor.ignore", "dirty")), "{result:?}");
    }

    #[test]
    fn the_git_environment_is_the_launch_allowlist_plus_the_operators_helper_variables() {
        let parent = [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/prx"),
            ("LANG", "C.UTF-8"),
            ("OPENAI_API_KEY", "parent-sentinel"),
            ("GITHUB_TOKEN", "parent-sentinel"),
            ("GIT_DIR", "/elsewhere"),
            ("GIT_CONFIG_PARAMETERS", "'core.fsmonitor=evil'"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_SSH_COMMAND", "evil"),
            ("GIT_EXEC_PATH", "/evil"),
            ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
            ("HTTPS_PROXY", "http://proxy.example:3128"),
            ("SSL_CERT_FILE", "/etc/ssl/cert.pem"),
            ("XDG_CONFIG_HOME", "/home/prx/.config"),
            ("GNUPGHOME", "/home/prx/.gnupg"),
            ("GIT_CONFIG_GLOBAL", "/home/prx/.gitconfig-work"),
            ("LD_PRELOAD", "/tmp/evil.so"),
        ]
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        let environment = git_environment(parent);
        let value = |name: &str| {
            environment
                .iter()
                .rev()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };
        for cleared in [
            "OPENAI_API_KEY",
            "GITHUB_TOKEN",
            "GIT_DIR",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
            "GIT_SSH_COMMAND",
            "GIT_EXEC_PATH",
            "LD_PRELOAD",
        ] {
            assert_eq!(value(cleared), None, "{cleared}");
        }
        assert!(
            !environment
                .iter()
                .any(|(_, value)| value.to_string_lossy().contains("sentinel"))
        );
        for (name, expected) in [
            ("HOME", "/home/prx"),
            ("PATH", "/usr/bin"),
            ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
            ("HTTPS_PROXY", "http://proxy.example:3128"),
            ("SSL_CERT_FILE", "/etc/ssl/cert.pem"),
            ("XDG_CONFIG_HOME", "/home/prx/.config"),
            ("GNUPGHOME", "/home/prx/.gnupg"),
            ("GIT_CONFIG_GLOBAL", "/home/prx/.gitconfig-work"),
            ("GIT_TERMINAL_PROMPT", "0"),
        ] {
            assert_eq!(value(name).as_deref(), Some(expected), "{name}");
        }
    }

    #[test]
    fn git_control_files_are_recognised_in_any_case_depth_and_spelling() {
        for path in [
            "/w/.git/config",
            "/w/.git/hooks/pre-commit",
            "/w/.git/info/attributes",
            "/w/.GIT/Config",
            "/w/sub/.git/modules/x/config",
            "/w/.git/worktrees/a/config.worktree",
            "/w/.git",
            "/w/vendor/.git",
            // R-GITENV F-8: NTFS and HFS+ spellings, rebase/sequencer state.
            "/w/GIT~1/config",
            "/w/.git./config",
            "/w/.git /hooks/x",
            "/w/.git::$INDEX_ALLOCATION/config",
            "/w/.g\u{200c}it/config",
            "/w/.git/config.",
            "/w/.git/rebase-merge/git-rebase-todo",
            "/w/.git/rebase-apply/patch",
            "/w/.git/sequencer/todo",
        ] {
            assert!(is_git_control_file(Path::new(path)), "{path}");
        }
        for path in [
            "/w/.gitignore",
            "/w/.git/objects/ab/cd",
            "/w/.git/HEAD",
            "/w/config",
            "/w/hooks.json",
            "/w/.github/workflows/ci.yml",
        ] {
            assert!(!is_git_control_file(Path::new(path)), "{path}");
        }
    }

    /// R-GITENV F-8: a hard link may alias `.git/config`.
    #[cfg(unix)]
    #[test]
    fn a_hard_linked_file_counts_as_a_git_control_file() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let original = temp.path().join("original");
        let alias = temp.path().join("notes.txt");
        std::fs::write(&original, "x").expect("test: write");
        assert!(!is_git_control_file(&original));
        std::fs::hard_link(&original, &alias).expect("test: hard link");
        assert!(is_git_control_file(&alias));
        assert!(is_git_control_file(&original));
    }

    /// R-GITENV F-12: nothing is created per process on Unix.
    #[cfg(unix)]
    #[test]
    fn the_hooks_path_is_dev_null_on_unix() {
        assert_eq!(hooks_path().expect("test: hooks path"), PathBuf::from("/dev/null"));
    }

    // ── Under a temporary HOME: the operator's global file is trusted ──

    #[cfg(unix)]
    fn script(path: &Path, marker: &Path) -> String {
        use std::os::unix::fs::PermissionsExt as _;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("test: script directory");
        }
        std::fs::write(path, format!("#!/bin/sh\necho ran >> '{}'\n", marker.display())).expect("test: script");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("test: chmod");
        path.display().to_string()
    }

    /// Plain test-side git under the temporary HOME (repository setup).
    #[cfg(unix)]
    async fn setup(dir: &Path, home: &Path, args: &[&str]) {
        let output = tokio::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", home)
            .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .kill_on_drop(true)
            .output()
            .await
            .expect("test: git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    fn parent_with_home(home: &Path) -> Vec<(OsString, OsString)> {
        vec![
            (OsString::from("PATH"), std::env::var_os("PATH").unwrap_or_default()),
            (OsString::from("HOME"), home.as_os_str().to_os_string()),
            // D-103: a scratch global file and no system file, never the
            // host's configuration.
            (
                OsString::from("GIT_CONFIG_GLOBAL"),
                home.join(".gitconfig").into_os_string(),
            ),
            (OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1")),
        ]
    }

    /// A temporary HOME with an empty global file, and a repository dir.
    #[cfg(unix)]
    fn temp_home_and_repo(temp: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&home).expect("test: home");
        std::fs::create_dir_all(&repo).expect("test: repo");
        std::fs::write(
            home.join(".gitconfig"),
            "[user]\n\tname = PRX Test\n\temail = prx@example.invalid\n",
        )
        .expect("test: global config");
        (home, repo)
    }

    #[cfg(unix)]
    async fn run(command: tokio::process::Command) -> std::process::Output {
        tokio::time::timeout(
            std::time::Duration::from_mins(1),
            crate::runtime::shell_process::run_managed_output(command),
        )
        .await
        .expect("test: git finished")
        .expect("test: git ran")
    }

    /// R-GITENV F-1: `remote.<name>.receivepack`/`uploadpack` are read
    /// first-wins, so the launch must refuse repository pack programs before
    /// a push or fetch to a local-path remote.
    #[cfg(unix)]
    #[tokio::test]
    async fn push_and_fetch_do_not_run_a_repository_receive_or_upload_pack() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let (home, repo) = temp_home_and_repo(&temp);
        let remote = temp.path().join("remote.git");
        std::fs::create_dir_all(&remote).expect("test: remote");
        setup(&remote, &home, &["init", "-q", "--bare"]).await;
        setup(&repo, &home, &["init", "-q"]).await;
        setup(&repo, &home, &["commit", "-q", "--allow-empty", "-m", "one"]).await;
        setup(
            &repo,
            &home,
            &["remote", "add", "origin", &remote.display().to_string()],
        )
        .await;
        let marker = temp.path().join("pack-program-ran");
        let receive = script(&temp.path().join("bin/receive.sh"), &marker);
        let upload = script(&temp.path().join("bin/upload.sh"), &marker);
        setup(&repo, &home, &["config", "remote.origin.receivepack", &receive]).await;
        setup(&repo, &home, &["config", "remote.origin.uploadpack", &upload]).await;

        let push_error = git_command_with_parent(GitScope::Repository(&repo), parent_with_home(&home))
            .await
            .expect_err("test: repository pack programs are refused before launch");
        assert!(push_error.to_string().contains("receivepack"), "{push_error}");
        assert!(!marker.exists(), "a repository pack program ran before refusal");

        setup(&repo, &home, &["config", "--unset", "remote.origin.receivepack"]).await;
        setup(&repo, &home, &["config", "--unset", "remote.origin.uploadpack"]).await;
        let mut push = git_command_with_parent(GitScope::Repository(&repo), parent_with_home(&home))
            .await
            .expect("test: safe launch");
        push.arg("push")
            .args(PUSH_SAFETY_ARGS)
            .args(["origin", "HEAD:refs/heads/main"]);
        let output = run(push).await;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let mut fetch = git_command_with_parent(GitScope::Repository(&repo), parent_with_home(&home))
            .await
            .expect("test: launch");
        fetch.arg("fetch").args(FETCH_SAFETY_ARGS).arg("origin");
        let output = run(fetch).await;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(!marker.exists(), "a repository receive-pack or upload-pack program ran");
    }

    /// R-GITENV F-2: git reads `.gitmodules` from the index when the working
    /// tree has none; its `ignore = none` would make `status` run git inside
    /// the submodule, with the submodule's own fsmonitor.
    #[cfg(unix)]
    #[tokio::test]
    async fn status_does_not_run_a_submodules_fsmonitor_named_by_an_index_only_gitmodules() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let (home, repo) = temp_home_and_repo(&temp);
        let upstream = temp.path().join("upstream");
        std::fs::create_dir_all(&upstream).expect("test: upstream");
        setup(&upstream, &home, &["init", "-q"]).await;
        std::fs::write(upstream.join("file"), "x\n").expect("test: file");
        setup(&upstream, &home, &["add", "file"]).await;
        setup(&upstream, &home, &["commit", "-q", "-m", "one"]).await;
        setup(&repo, &home, &["init", "-q"]).await;
        setup(
            &repo,
            &home,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "--quiet",
                "add",
                &upstream.display().to_string(),
                "sub",
            ],
        )
        .await;
        setup(
            &repo,
            &home,
            &["config", "-f", ".gitmodules", "submodule.sub.ignore", "none"],
        )
        .await;
        setup(&repo, &home, &["add", ".gitmodules"]).await;
        setup(&repo, &home, &["commit", "-q", "-m", "sub"]).await;
        // No `submodule.*` in .git/config, no working-tree .gitmodules.
        setup(&repo, &home, &["config", "--remove-section", "submodule.sub"]).await;
        std::fs::remove_file(repo.join(".gitmodules")).expect("test: remove .gitmodules");
        let marker = temp.path().join("submodule-fsmonitor-ran");
        let fsmonitor = script(&temp.path().join("bin/fsmonitor.sh"), &marker);
        setup(&repo.join("sub"), &home, &["config", "core.fsmonitor", &fsmonitor]).await;
        std::fs::write(repo.join("sub/file"), "dirty\n").expect("test: dirty submodule");

        let mut status = git_command_with_parent(GitScope::Repository(&repo), parent_with_home(&home))
            .await
            .expect("test: launch");
        status.args(["status", "--porcelain"]);
        let output = run(status).await;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(!marker.exists(), "git ran inside the submodule with its fsmonitor");
    }

    /// R-GITENV F-3: a workspace laid out as a bare repository whose
    /// `core.worktree` names another directory is not used as one.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_planted_bare_layout_with_a_foreign_worktree_is_refused() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let (home, workspace) = temp_home_and_repo(&temp);
        let secret = temp.path().join("secret");
        std::fs::create_dir_all(&secret).expect("test: secret");
        std::fs::write(secret.join("id_key"), "TOPSECRET\n").expect("test: secret file");
        std::fs::write(workspace.join("HEAD"), "ref: refs/heads/main\n").expect("test: HEAD");
        std::fs::write(
            workspace.join("config"),
            format!(
                "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tworktree = {}\n",
                secret.display()
            ),
        )
        .expect("test: config");
        std::fs::create_dir_all(workspace.join("objects")).expect("test: objects");
        std::fs::create_dir_all(workspace.join("refs/heads")).expect("test: refs");

        let launched = git_command_with_parent(GitScope::Repository(&workspace), parent_with_home(&home)).await;
        let Ok(mut command) = launched else {
            return;
        };
        command.args(["status", "--porcelain"]);
        let output = run(command).await;
        assert!(
            !output.status.success(),
            "a planted bare layout was used as a repository"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("id_key"));
    }

    /// R-GITENV F-3: a `core.worktree` pointing outside the directory git
    /// runs in is refused; a directory inside a larger repository is fine.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_worktree_outside_the_directory_is_refused_and_a_nested_directory_is_not() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let (home, repo) = temp_home_and_repo(&temp);
        setup(&repo, &home, &["init", "-q"]).await;
        std::fs::create_dir_all(repo.join("nested")).expect("test: nested");
        assert!(
            git_command_with_parent(GitScope::Repository(&repo.join("nested")), parent_with_home(&home))
                .await
                .is_ok(),
            "a directory inside the working tree was refused"
        );
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside).expect("test: outside");
        setup(
            &repo,
            &home,
            &["config", "core.worktree", &outside.display().to_string()],
        )
        .await;
        let error = git_command_with_parent(GitScope::Repository(&repo), parent_with_home(&home))
            .await
            .expect_err("a foreign working tree was accepted");
        assert!(error.to_string().contains("outside"), "{error}");
    }

    /// D-100: a credential helper of the operator's global file is used; one
    /// the repository adds is not. A loopback server answers 401 so git asks.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_global_credential_helper_is_used_and_a_repository_one_is_not() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&home).expect("test: home");
        std::fs::create_dir_all(&repo).expect("test: repo");
        let global_marker = temp.path().join("global-helper-ran");
        let local_marker = temp.path().join("local-helper-ran");
        let global_helper = script(&temp.path().join("bin/global.sh"), &global_marker);
        let local_helper = script(&temp.path().join("bin/local.sh"), &local_marker);
        std::fs::write(
            home.join(".gitconfig"),
            format!(
                "[user]\n\tname = PRX Test\n\temail = prx@example.invalid\n[credential]\n\thelper = {global_helper}\n"
            ),
        )
        .expect("test: global config");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
        let port = listener.local_addr().expect("test: address").port();
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = vec![0_u8; 8192];
                let _ = socket.read(&mut request).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"prx\"\r\n\
                          Content-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            }
        });
        setup(&repo, &home, &["init", "-q"]).await;
        setup(&repo, &home, &["commit", "-q", "--allow-empty", "-m", "one"]).await;
        setup(
            &repo,
            &home,
            &["remote", "add", "origin", &format!("http://127.0.0.1:{port}/r.git")],
        )
        .await;
        setup(&repo, &home, &["config", "credential.helper", &local_helper]).await;

        let mut command = git_command_with_parent(GitScope::Repository(&repo), parent_with_home(&home))
            .await
            .expect("test: launch");
        command.arg("push").args(PUSH_SAFETY_ARGS).args(["origin", "HEAD"]);
        let output = tokio::time::timeout(
            std::time::Duration::from_mins(1),
            crate::runtime::shell_process::run_managed_output(command),
        )
        .await
        .expect("test: push finished")
        .expect("test: push ran");
        server.abort();
        assert!(!output.status.success(), "a push to a 401 server cannot succeed");
        assert!(
            global_marker.exists(),
            "the global credential helper was not used: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!local_marker.exists(), "the repository's credential helper ran");
    }

    /// D-100: hooks never run, not even from a hooks path the operator's
    /// global file names; the global identity is used.
    #[cfg(unix)]
    #[tokio::test]
    async fn no_hook_runs_even_from_a_global_hooks_path() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&home).expect("test: home");
        std::fs::create_dir_all(&repo).expect("test: repo");
        let marker = temp.path().join("hook-ran");
        script(&temp.path().join("global-hooks/pre-commit"), &marker);
        std::fs::write(
            home.join(".gitconfig"),
            format!(
                "[user]\n\tname = PRX Global\n\temail = global@example.invalid\n[core]\n\thooksPath = {}\n",
                temp.path().join("global-hooks").display()
            ),
        )
        .expect("test: global config");
        setup(&repo, &home, &["init", "-q"]).await;
        script(&repo.join(".git/hooks/pre-commit"), &marker);

        let mut command = git_command_with_parent(GitScope::Repository(&repo), parent_with_home(&home))
            .await
            .expect("test: launch");
        command.args(["commit", "-q", "--allow-empty", "-m", "one"]);
        let output = crate::runtime::shell_process::run_managed_output(command)
            .await
            .expect("test: commit ran");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(!marker.exists(), "a hook ran");

        let mut log = git_command_with_parent(GitScope::Repository(&repo), parent_with_home(&home))
            .await
            .expect("test: launch");
        log.args(["log", "-1", "--format=%ae"]);
        let output = crate::runtime::shell_process::run_managed_output(log)
            .await
            .expect("test: log ran");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "global@example.invalid");
    }

    /// R-GITENV-confirm C-1: a partial clone's lazy fetch (here during a
    /// checkout) starts git with the repository's first-wins
    /// `remote.origin.uploadpack`; no option PRX passes reaches it, so the
    /// launch is refused.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_partial_clone_lazy_fetch_does_not_run_a_repository_upload_pack() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let (home, _) = temp_home_and_repo(&temp);
        let upstream = temp.path().join("upstream");
        std::fs::create_dir_all(upstream.join("skills")).expect("test: upstream");
        setup(&upstream, &home, &["init", "-q"]).await;
        std::fs::write(upstream.join("a.txt"), "a\n").expect("test: file");
        std::fs::write(upstream.join("skills/x.md"), "x\n").expect("test: file");
        std::fs::write(
            upstream.join(".gitmodules"),
            "[submodule \"fixture\"]\n\tpath = fixture\n\turl = https://example.invalid/fixture\n",
        )
        .expect("test: submodule metadata");
        setup(&upstream, &home, &["add", "-A"]).await;
        setup(&upstream, &home, &["commit", "-q", "-m", "one"]).await;
        setup(&upstream, &home, &["config", "uploadpack.allowFilter", "true"]).await;
        setup(&upstream, &home, &["config", "uploadpack.allowAnySHA1InWant", "true"]).await;
        let url = format!("file://{}", upstream.display());
        let partial = temp.path().join("partial");
        setup(
            temp.path(),
            &home,
            &[
                "clone",
                "-q",
                "--no-checkout",
                "--filter=blob:none",
                &url,
                &partial.display().to_string(),
            ],
        )
        .await;
        let marker = temp.path().join("upload-pack-ran");
        let program = script(&temp.path().join("bin/upload.sh"), &marker);
        setup(&partial, &home, &["config", "remote.origin.uploadpack", &program]).await;
        let missing_blob = read_git(
            &partial,
            &git_environment(parent_with_home(&home)),
            ReadKind::Other,
            &["cat-file", "-e", "HEAD:.gitmodules"],
        )
        .await;
        assert!(
            missing_blob.is_err(),
            "the preflight fixture must have a missing .gitmodules blob"
        );

        match git_command_with_parent(GitScope::Repository(&partial), parent_with_home(&home)).await {
            Err(error) => assert!(error.to_string().contains("uploadpack"), "{error}"),
            Ok(mut checkout) => {
                checkout.args(["checkout", "-q", "HEAD", "--", "."]);
                let _ = run(checkout).await;
            }
        }
        assert!(
            !marker.exists(),
            "a lazy fetch ran the repository's upload-pack program"
        );
    }

    /// D-106: the skills partial clone (`--filter=blob:none --sparse`, then
    /// `sparse-checkout`) keeps working through the launch when the
    /// repository sets no pack program.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_sparse_partial_clone_works_through_the_launch() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let (home, _) = temp_home_and_repo(&temp);
        let upstream = temp.path().join("upstream");
        std::fs::create_dir_all(upstream.join("skills")).expect("test: upstream");
        setup(&upstream, &home, &["init", "-q"]).await;
        std::fs::write(upstream.join("a.txt"), "a\n").expect("test: file");
        std::fs::write(upstream.join("skills/x.md"), "x\n").expect("test: file");
        setup(&upstream, &home, &["add", "-A"]).await;
        setup(&upstream, &home, &["commit", "-q", "-m", "one"]).await;
        setup(&upstream, &home, &["config", "uploadpack.allowFilter", "true"]).await;
        setup(&upstream, &home, &["config", "uploadpack.allowAnySHA1InWant", "true"]).await;
        let url = format!("file://{}", upstream.display());
        let clone = temp.path().join("clone");

        let mut command = git_command_with_parent(GitScope::NewRepository, parent_with_home(&home))
            .await
            .expect("test: launch");
        command
            .current_dir(temp.path())
            .args(["clone", "-q", "--depth", "1", "--filter=blob:none", "--sparse", &url])
            .arg(&clone);
        let output = run(command).await;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let mut sparse = git_command_with_parent(GitScope::Repository(&clone), parent_with_home(&home))
            .await
            .expect("test: launch");
        sparse.args(["sparse-checkout", "set", "skills"]);
        let output = run(sparse).await;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(clone.join("skills/x.md").is_file());
    }

    /// R-GITENV-confirm C-2: a `core.worktree` naming a parent of the
    /// directory passes containment; the parent's `.git` is not the
    /// repository git uses, so the launch is refused. A linked worktree (a
    /// `.git` link file) is fine.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_worktree_at_a_parent_is_refused_and_a_linked_worktree_is_not() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let (home, repo) = temp_home_and_repo(&temp);
        setup(&repo, &home, &["init", "-q"]).await;
        setup(&repo, &home, &["commit", "-q", "--allow-empty", "-m", "one"]).await;
        let linked = temp.path().join("linked");
        setup(&repo, &home, &["worktree", "add", "-q", &linked.display().to_string()]).await;
        assert!(
            git_command_with_parent(GitScope::Repository(&linked), parent_with_home(&home))
                .await
                .is_ok(),
            "a linked worktree was refused"
        );
        setup(
            &repo,
            &home,
            &["config", "core.worktree", &temp.path().display().to_string()],
        )
        .await;
        let error = git_command_with_parent(GitScope::Repository(&repo), parent_with_home(&home))
            .await
            .expect_err("a parent working tree was accepted");
        assert!(error.to_string().contains("does not hold the repository"), "{error}");
    }

    /// D-107: an operator shell job's git gets the fixed program settings
    /// (the operator's values or the fallbacks) and the forced settings
    /// through `GIT_CONFIG_*`, plus `GIT_NO_LAZY_FETCH`.
    #[test]
    fn operator_shell_jobs_get_the_fixed_settings_through_the_environment() {
        let pairs = operator_git_config_pairs(&[
            global("core.sshcommand", "ssh -i key"),
            local("core.fsmonitor", "./evil"),
        ]);
        for expected in [
            pair("core.sshcommand", "ssh -i key"),
            pair("core.fsmonitor", "false"),
            pair("safe.bareRepository", "explicit"),
            pair("protocol.ext.allow", "never"),
        ] {
            assert!(pairs.contains(&expected), "{expected:?}: {pairs:?}");
        }
        let environment =
            git_config_environment(&[pair("core.fsmonitor", "false"), pair("core.hooksPath", "/dev/null")]);
        let value = |name: &str| {
            environment
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };
        assert_eq!(value("GIT_CONFIG_COUNT").as_deref(), Some("2"));
        assert_eq!(value("GIT_CONFIG_KEY_1").as_deref(), Some("core.hooksPath"));
        assert_eq!(value("GIT_CONFIG_VALUE_1").as_deref(), Some("/dev/null"));
        assert_eq!(value("GIT_NO_LAZY_FETCH").as_deref(), Some("1"));
    }
}

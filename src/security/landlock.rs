//! Landlock sandbox (Linux kernel 5.13+ LSM)
//!
//! Landlock provides unprivileged sandboxing through the Linux kernel.
//! This module uses the pure-Rust `landlock` crate for filesystem access control.
//!
//! Landlock's `restrict_self()` affects the calling process and its future
//! descendants.  PRX therefore installs the ruleset through `Command::pre_exec`
//! so only the shell child process is restricted; the long-running daemon keeps
//! its gateway, memory, and channel permissions unchanged.

#[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
use landlock::{AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreated, RulesetCreatedAttr};

use crate::security::traits::Sandbox;
#[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
use std::path::Path;

#[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
use std::os::unix::process::CommandExt;

/// Landlock sandbox backend for Linux
#[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
#[derive(Debug)]
pub struct LandlockSandbox {
    workspace_dir: Option<std::path::PathBuf>,
    /// Bug #2: opt-in trusted toolchain directories granted read+execute access.
    /// Empty by default (only system paths are executable). Kept in lockstep with
    /// the shell tool's `extra_path_dirs` PATH extension.
    extra_exec_dirs: Vec<std::path::PathBuf>,
}

#[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
impl LandlockSandbox {
    /// Create a new Landlock sandbox with the given workspace directory
    pub fn new() -> std::io::Result<Self> {
        Self::with_workspace(None)
    }

    /// Create a Landlock sandbox with a specific workspace directory
    pub fn with_workspace(workspace_dir: Option<std::path::PathBuf>) -> std::io::Result<Self> {
        Self::with_workspace_and_exec_dirs(workspace_dir, Vec::new())
    }

    /// Create a Landlock sandbox with a workspace and opt-in trusted exec dirs.
    ///
    /// `extra_exec_dirs` (Bug #2) are granted read+execute so the shell tool can
    /// run binaries from operator-trusted toolchains (e.g. `~/.cargo/bin`). These
    /// MUST mirror the shell tool's `extra_path_dirs` PATH extension, otherwise the
    /// PATH would point at binaries the kernel still refuses to exec.
    pub fn with_workspace_and_exec_dirs(
        workspace_dir: Option<std::path::PathBuf>,
        extra_exec_dirs: Vec<std::path::PathBuf>,
    ) -> std::io::Result<Self> {
        // Test if Landlock is available by trying to create a minimal ruleset
        let test_ruleset = Ruleset::default()
            .handle_access(AccessFs::ReadFile | AccessFs::WriteFile)
            .and_then(|ruleset| ruleset.create());

        match test_ruleset {
            Ok(_) => Ok(Self {
                workspace_dir,
                extra_exec_dirs,
            }),
            Err(e) => {
                tracing::debug!("Landlock not available: {}", e);
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "Landlock not available",
                ))
            }
        }
    }

    /// Probe if Landlock is available (for auto-detection)
    pub fn probe() -> std::io::Result<Self> {
        Self::new()
    }

    /// Build a Landlock ruleset in the parent process.
    ///
    /// The returned ruleset is later consumed by `restrict_self()` in the
    /// forked child through `Command::pre_exec`.
    fn build_ruleset(
        workspace_dir: Option<&Path>,
        extra_exec_dirs: &[std::path::PathBuf],
    ) -> std::io::Result<RulesetCreated> {
        let mut ruleset = Ruleset::default()
            .handle_access(
                AccessFs::Execute
                    | AccessFs::ReadFile
                    | AccessFs::WriteFile
                    | AccessFs::ReadDir
                    | AccessFs::RemoveDir
                    | AccessFs::RemoveFile
                    | AccessFs::MakeChar
                    | AccessFs::MakeDir
                    | AccessFs::MakeSock
                    | AccessFs::MakeFifo
                    | AccessFs::MakeBlock
                    | AccessFs::MakeReg
                    | AccessFs::MakeSym,
            )
            .and_then(|ruleset| ruleset.create())
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        // Allow the workspace directory full filesystem operations. The
        // workspace is the agent's writable scratch area: shell commands must be
        // able to CREATE files/dirs and REMOVE them (e.g. `echo > f`, `mkdir`,
        // `rm`, `git` which writes a whole tree), not merely read/overwrite
        // existing files. Granting only Read/Write/ReadDir (the previous
        // behaviour) left `file_write` working but `shell` unable to create any
        // new file inside the workspace — half of BUG-02's FS breakage.
        let workspace_access = AccessFs::Execute
            | AccessFs::ReadFile
            | AccessFs::WriteFile
            | AccessFs::ReadDir
            | AccessFs::RemoveDir
            | AccessFs::RemoveFile
            | AccessFs::MakeDir
            | AccessFs::MakeReg
            | AccessFs::MakeSym;
        if let Some(workspace) = workspace_dir {
            if workspace.exists() {
                let workspace_fd = PathFd::new(workspace).map_err(|e| std::io::Error::other(e.to_string()))?;
                ruleset = ruleset
                    .add_rule(PathBeneath::new(workspace_fd, workspace_access))
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
            }
        }

        // Allow /tmp for general operations (same create/remove rights as the
        // workspace so temp scratch files behave like a real shell session).
        let tmp_fd = PathFd::new(Path::new("/tmp")).map_err(|e| std::io::Error::other(e.to_string()))?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(tmp_fd, workspace_access))
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        // Allow system executable and library paths for command startup.
        for path in ["/usr", "/bin", "/lib", "/lib64"] {
            let path = Path::new(path);
            if !path.exists() {
                continue;
            }
            let fd = PathFd::new(path).map_err(|e| std::io::Error::other(e.to_string()))?;
            ruleset = ruleset
                .add_rule(PathBeneath::new(
                    fd,
                    AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir,
                ))
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        // Bug #2: opt-in trusted toolchain directories. Granted read+execute (read
        // dir/file so dynamic loaders and wrapper scripts resolve) but NOT write —
        // these are search-path dirs, not scratch space. Mirrors the shell tool's
        // `extra_path_dirs` PATH extension so the two never drift. Empty by default,
        // preserving the hardened system-only baseline.
        for dir in extra_exec_dirs {
            if !dir.exists() {
                tracing::warn!(dir = %dir.display(), "extra_exec_dirs entry missing, skipping Landlock grant");
                continue;
            }
            let fd = PathFd::new(dir).map_err(|e| std::io::Error::other(e.to_string()))?;
            ruleset = ruleset
                .add_rule(PathBeneath::new(
                    fd,
                    AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir,
                ))
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        Ok(ruleset)
    }

    /// Apply Landlock restrictions to the current process.
    ///
    /// This is only called from the `pre_exec` child hook.
    fn restrict_child(ruleset: RulesetCreated) -> std::io::Result<()> {
        // Apply the ruleset
        match ruleset.restrict_self() {
            Ok(_) => Ok(()),
            Err(e) => Err(std::io::Error::other(e.to_string())),
        }
    }
}

#[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
impl Sandbox for LandlockSandbox {
    #[allow(unsafe_code)]
    fn wrap_command(&self, cmd: &mut std::process::Command) -> std::io::Result<()> {
        let mut ruleset = Some(Self::build_ruleset(
            self.workspace_dir.as_deref(),
            &self.extra_exec_dirs,
        )?);
        // SAFETY: `pre_exec` runs in the forked child immediately before exec.
        // The closure only consumes the prebuilt Landlock ruleset and performs
        // the kernel restriction call, leaving the daemon parent unrestricted.
        unsafe {
            cmd.pre_exec(move || {
                let ruleset = ruleset
                    .take()
                    .ok_or_else(|| std::io::Error::other("Landlock ruleset already consumed"))?;
                Self::restrict_child(ruleset)
            });
        }
        Ok(())
    }

    fn is_available(&self) -> bool {
        // Try to create a minimal ruleset to verify availability
        Ruleset::default()
            .handle_access(AccessFs::ReadFile)
            .and_then(|ruleset| ruleset.create())
            .is_ok()
    }

    fn name(&self) -> &str {
        "landlock"
    }

    fn description(&self) -> &str {
        "Linux kernel LSM sandboxing (filesystem access control)"
    }
}

// Stub implementations for non-Linux or when feature is disabled
#[cfg(not(all(feature = "sandbox-landlock", target_os = "linux")))]
pub struct LandlockSandbox;

#[cfg(not(all(feature = "sandbox-landlock", target_os = "linux")))]
impl LandlockSandbox {
    pub fn new() -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Landlock is only supported on Linux with the sandbox-landlock feature",
        ))
    }

    pub fn with_workspace(_workspace_dir: Option<std::path::PathBuf>) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Landlock is only supported on Linux",
        ))
    }

    pub fn with_workspace_and_exec_dirs(
        _workspace_dir: Option<std::path::PathBuf>,
        _extra_exec_dirs: Vec<std::path::PathBuf>,
    ) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Landlock is only supported on Linux",
        ))
    }

    pub fn probe() -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Landlock is only supported on Linux",
        ))
    }
}

#[cfg(not(all(feature = "sandbox-landlock", target_os = "linux")))]
impl Sandbox for LandlockSandbox {
    fn wrap_command(&self, _cmd: &mut std::process::Command) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Landlock is only supported on Linux",
        ))
    }

    fn is_available(&self) -> bool {
        false
    }

    fn name(&self) -> &str {
        "landlock"
    }

    fn description(&self) -> &str {
        "Linux kernel LSM sandboxing (not available on this platform)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
    #[test]
    fn landlock_sandbox_name() {
        if let Ok(sandbox) = LandlockSandbox::new() {
            assert_eq!(sandbox.name(), "landlock");
        }
    }

    #[cfg(not(all(feature = "sandbox-landlock", target_os = "linux")))]
    #[test]
    fn landlock_not_available_on_non_linux() {
        assert!(!LandlockSandbox.is_available());
        assert_eq!(LandlockSandbox.name(), "landlock");
    }

    #[test]
    fn landlock_with_none_workspace() {
        // Should work even without a workspace directory
        let result = LandlockSandbox::with_workspace(None);
        // Result depends on platform and feature flag
        match result {
            Ok(sandbox) => assert!(sandbox.is_available()),
            Err(_) => assert!(!cfg!(all(feature = "sandbox-landlock", target_os = "linux"))),
        }
    }

    #[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
    #[test]
    fn landlock_wrap_command_does_not_restrict_parent_process() {
        let Ok(sandbox) = LandlockSandbox::with_workspace(None) else {
            return;
        };
        let cargo_toml = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");

        let mut cmd = std::process::Command::new("true");
        sandbox.wrap_command(&mut cmd).unwrap();
        let status = cmd.status().unwrap();
        assert!(status.success());

        let cat = if std::path::Path::new("/bin/cat").exists() {
            "/bin/cat"
        } else {
            "/usr/bin/cat"
        };
        let mut denied_cmd = std::process::Command::new(cat);
        denied_cmd.arg(&cargo_toml);
        sandbox.wrap_command(&mut denied_cmd).unwrap();
        let denied = denied_cmd.output().unwrap();
        assert!(!denied.status.success());

        let contents = std::fs::read_to_string(cargo_toml).unwrap();
        assert!(contents.contains("[package]"));
    }

    #[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
    #[test]
    fn landlock_workspace_is_writable_and_shared_with_host() {
        // BUG-02: under Landlock the shell child must be able to write INSIDE the
        // workspace (so files are visible to file_read/file_write on the same
        // host FS), while writes OUTSIDE the workspace are denied. This proves
        // the FS-unification guarantee the deployment relies on.
        let tmp = std::env::temp_dir().join(format!("openprx_landlock_ws_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("test: create workspace");

        let Ok(sandbox) = LandlockSandbox::with_workspace(Some(tmp.clone())) else {
            let _ = std::fs::remove_dir_all(&tmp);
            return; // kernel without Landlock — skip
        };

        let sh = if std::path::Path::new("/bin/sh").exists() {
            "/bin/sh"
        } else {
            "/usr/bin/sh"
        };

        // Write inside the workspace → must succeed, and the host (parent, not
        // sandboxed) must see the file afterward.
        let inside = tmp.join("from_shell.txt");
        let mut ok_cmd = std::process::Command::new(sh);
        ok_cmd.arg("-c").arg(format!("echo SHARED > {}", inside.display()));
        sandbox.wrap_command(&mut ok_cmd).unwrap();
        let ok = ok_cmd.output().unwrap();
        assert!(
            ok.status.success(),
            "shell must be able to write inside the workspace; stderr={}",
            String::from_utf8_lossy(&ok.stderr)
        );
        let host_view = std::fs::read_to_string(&inside).unwrap_or_default();
        assert!(
            host_view.contains("SHARED"),
            "host (file_read) must see the file the sandboxed shell created"
        );

        // Write outside the workspace (a fresh path outside /tmp grant) → denied.
        let outside = std::env::temp_dir().join(format!("openprx_landlock_escape_{}", std::process::id()));
        let _ = std::fs::remove_file(&outside);
        // Note: /tmp itself is granted RW by build_ruleset, so to prove denial we
        // target /etc which is never granted.
        let etc_target = "/etc/openprx_landlock_should_be_denied";
        let mut deny_cmd = std::process::Command::new(sh);
        deny_cmd.arg("-c").arg(format!("echo NOPE > {etc_target}"));
        sandbox.wrap_command(&mut deny_cmd).unwrap();
        let denied = deny_cmd.status().unwrap();
        assert!(
            !denied.success(),
            "writing outside the workspace (/etc) must be denied by Landlock"
        );
        assert!(
            !std::path::Path::new(etc_target).exists(),
            "denied write must not have created the file"
        );

        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_file(&outside);
    }

    #[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
    #[test]
    fn landlock_extra_exec_dir_is_executable_only_when_granted() {
        // Bug #2: a script placed in an opt-in dir must run when that dir is in
        // extra_exec_dirs, and be denied (exec) when it is not — proving PATH and
        // the sandbox grant must travel together.
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        // The toolchain dir MUST live outside /tmp (build_ruleset always grants
        // /tmp read+execute), otherwise the "denied without grant" case wouldn't
        // hold. Use a unique dir under $HOME, which Landlock never grants unless we
        // add it to extra_exec_dirs.
        let Some(home) = directories::UserDirs::new().map(|d| d.home_dir().to_path_buf()) else {
            return; // no home dir — cannot place a dir outside the granted set
        };
        let toolchain = home.join(format!(".openprx_ll_tool_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&toolchain);
        if std::fs::create_dir_all(&toolchain).is_err() {
            return; // home not writable in this environment — skip
        }
        let script = toolchain.join("mytool");
        {
            let mut f = std::fs::File::create(&script).expect("test: create script");
            writeln!(f, "#!/bin/sh\necho TOOL_RAN").expect("test: write script");
        }
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("test: chmod");

        // Workspace dir (granted RW); kept separate from the toolchain dir.
        let ws = std::env::temp_dir().join(format!("openprx_ll_ws_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).expect("test: create ws");

        // Without the grant: exec of the toolchain script must be denied.
        let Ok(no_grant) = LandlockSandbox::with_workspace_and_exec_dirs(Some(ws.clone()), Vec::new()) else {
            let _ = std::fs::remove_dir_all(&toolchain);
            let _ = std::fs::remove_dir_all(&ws);
            return; // kernel without Landlock — skip
        };
        let mut denied = std::process::Command::new(&script);
        no_grant.wrap_command(&mut denied).expect("test: wrap");
        // Landlock denies exec at the kernel level: `output()` may return Err
        // (exec syscall blocked) OR a non-success status. Either proves denial.
        let denied_ok = match denied.output() {
            Ok(out) => out.status.success(),
            Err(_) => false,
        };
        assert!(!denied_ok, "exec from un-granted toolchain dir must be denied");

        // With the grant: the same script must run.
        let granted = LandlockSandbox::with_workspace_and_exec_dirs(Some(ws.clone()), vec![toolchain.clone()])
            .expect("test: granted sandbox");
        let mut allowed = std::process::Command::new(&script);
        granted.wrap_command(&mut allowed).expect("test: wrap granted");
        let allowed_out = allowed.output().expect("test: run granted");
        assert!(
            allowed_out.status.success(),
            "exec from granted toolchain dir must succeed; stderr={}",
            String::from_utf8_lossy(&allowed_out.stderr)
        );
        assert!(String::from_utf8_lossy(&allowed_out.stdout).contains("TOOL_RAN"));

        let _ = std::fs::remove_dir_all(&toolchain);
        let _ = std::fs::remove_dir_all(&ws);
    }

    // ── §1.1 Landlock stub tests ──────────────────────────────

    #[cfg(not(all(feature = "sandbox-landlock", target_os = "linux")))]
    #[test]
    fn landlock_stub_wrap_command_returns_unsupported() {
        let sandbox = LandlockSandbox;
        let mut cmd = std::process::Command::new("echo");
        let result = sandbox.wrap_command(&mut cmd);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
    }

    #[cfg(not(all(feature = "sandbox-landlock", target_os = "linux")))]
    #[test]
    fn landlock_stub_new_returns_unsupported() {
        let result = LandlockSandbox::new();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
    }

    #[cfg(not(all(feature = "sandbox-landlock", target_os = "linux")))]
    #[test]
    fn landlock_stub_probe_returns_unsupported() {
        let result = LandlockSandbox::probe();
        assert!(result.is_err());
    }
}
